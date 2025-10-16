mod deshred;
mod rpc;
mod shred;
mod websocket;

use clap::Parser;
use dashmap::DashMap;
use rpc::Subscription;
use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(
        long,
        env = "TRACE_LOG_PATH",
        help = "Path for structured transaction trace data (separate from app logs)"
    )]
    trace_log_path: Option<String>,

    #[arg(
        long,
        env = "LOG_PATH",
        help = "Path to the general application log file"
    )]
    log_path: Option<String>,

    #[arg(long, env = "UDP_PORT", help = "UDP port for receiving shred stream")]
    udp_port: Option<u16>,

    #[arg(long, env = "RPC_PORT", help = "Port for RPC server")]
    rpc_port: Option<u16>,

    #[arg(long, env = "WS_PORT", help = "Port for WebSocket server")]
    ws_port: Option<u16>,

    #[arg(long, env = "CONFIG_PATH", help = "Path to a config TOML file")]
    config_path: Option<String>,
}

#[derive(Default, Debug, serde::Deserialize)]
struct Config {
    udp_port: Option<u16>,
    rpc_port: Option<u16>,
    ws_port: Option<u16>,
    trace_log_path: Option<String>,
    log_path: Option<String>,
}

/// Simple logging setup - returns guard that must be kept alive
fn init_logging(
    log_path: Option<&str>,
) -> anyhow::Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if let Some(path) = log_path {
        // Create parent directories if needed
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Setup non-blocking file writer
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let (non_blocking, guard) = tracing_appender::non_blocking(file);

        // Log to both console and file
        tracing_subscriber::fmt()
            .with_target(true)
            .with_env_filter(env_filter)
            .with_writer(non_blocking.and(std::io::stdout))
            .init();

        info!("Logging to file: {}", path);
        Ok(Some(guard))
    } else {
        // Console only
        tracing_subscriber::fmt()
            .with_target(true)
            .with_env_filter(env_filter)
            .init();
        Ok(None)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load config from file if it exists
    let config_path = args
        .config_path
        .as_deref()
        .or(std::path::Path::new("config.toml")
            .exists()
            .then_some("config.toml"));

    let mut config = if let Some(path) = config_path {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str::<Config>(&s).ok())
            .unwrap_or_default()
    } else {
        Config::default()
    };

    // CLI args override config file
    config.udp_port = args.udp_port.or(config.udp_port);
    config.rpc_port = args.rpc_port.or(config.rpc_port);
    config.ws_port = args.ws_port.or(config.ws_port);
    config.log_path = args.log_path.or(config.log_path);
    config.trace_log_path = args.trace_log_path.or(config.trace_log_path);

    // Setup logging (both console and file if log_path is provided)
    let _log_guard = init_logging(config.log_path.as_deref())?;

    // Get final configuration values
    let udp_port = config.udp_port.unwrap_or(18888);
    let rpc_port = config.rpc_port.unwrap_or(12345);
    let ws_port = config.ws_port.unwrap_or(38899);

    // Trace log is for structured transaction data (handled separately by deshred module)
    let trace_log_path = config.trace_log_path;

    // Exit channel
    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // Shared subscriptions map for RPC and deshred
    let subscriptions = Arc::new(DashMap::<String, Subscription>::new());
    let deshred_subscriptions = subscriptions.clone();
    let ws_subscriptions = subscriptions.clone();

    // Start RPC server
    let rpc_addr: SocketAddr = format!("0.0.0.0:{}", rpc_port).parse()?;
    let rpc_server = rpc::start_rpc_server(rpc_addr, subscriptions.clone()).await?;
    info!("RPC server started on port {}", rpc_port);

    let ws_addr: SocketAddr = format!("0.0.0.0:{}", ws_port).parse()?;
    let ws_shutdown = shutdown_tx.subscribe();
    let ws_handle = tokio::spawn(async move {
        if let Err(e) =
            websocket::start_websocket_server(ws_addr, ws_subscriptions, ws_shutdown, 10_000).await
        {
            error!("WebSocket server error: {:?}", e);
        }
    });

    // udp server
    let (reconstruct_tx, reconstruct_rx) = crossbeam_channel::bounded(1_024);

    let udp_shutdown_rx = shutdown_tx.subscribe();
    let udp_handle = tokio::spawn(async move {
        if let Err(e) = shred::run_udp_server(udp_port, udp_shutdown_rx, reconstruct_tx).await {
            error!("UDP server occur err: {:?}", e);
        }
    });

    // reconstruct server
    let reconstruct_shutdown = shutdown_tx.subscribe();
    let reconstruct_handle = tokio::spawn(async move {
        let _ = deshred::reconstruct_shreds_server(
            reconstruct_shutdown,
            reconstruct_rx,
            deshred_subscriptions,
            trace_log_path,
        )
        .await;
    });

    wait_for_shutdown().await;
    info!("receive exit signal, begin exit...");
    let _ = shutdown_tx.send(());

    // Close RPC server on a plain OS thread so that its internal
    // runtime is dropped outside of our Tokio async context.
    let rpc_close = std::thread::spawn(move || {
        rpc_server.close();
    });

    udp_handle.await?;
    ws_handle.await?;
    reconstruct_handle.await?;
    let _ = rpc_close.join();

    info!("All tasks have shut down.");
    Ok(())
}

pub async fn wait_for_shutdown() {
    if signal::ctrl_c().await.is_ok() {
        info!("receive SIGINT（Ctrl+C / kill -2）");
    }
}
