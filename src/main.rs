mod deshred;
mod rpc;
mod shred;

use clap::Parser;
use dashmap::DashMap;
use rpc::Subscription;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, env = "TRACE_LOG_PATH", help = "Path to the trace log file")]
    trace_log_path: Option<String>,

    #[arg(
        long,
        env = "UDP_PORT",
        help = "UDP port for receiving shred stream"
    )]
    udp_port: Option<u16>,

    #[arg(
        long,
        env = "RPC_PORT",
        help = "Port for RPC server"
    )]
    rpc_port: Option<u16>,

    #[arg(long, env = "CONFIG_PATH", help = "Path to a config TOML file")]
    config_path: Option<String>,
}

#[derive(Default, Debug, serde::Deserialize)]
struct FileConfig {
    udp_port: Option<u16>,
    rpc_port: Option<u16>,
    trace_log_path: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load config file if provided (or `config.toml` if present)
    let cfg_path = args
        .config_path
        .clone()
        .or_else(|| {
            let default = std::path::Path::new("config.toml");
            if default.exists() { Some(default.display().to_string()) } else { None }
        });
    let file_cfg: FileConfig = match cfg_path {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(cfg) => {
                    info!("Loaded config from {}", path);
                    cfg
                }
                Err(e) => {
                    tracing::warn!("Failed to parse config {}: {}", path, e);
                    FileConfig::default()
                }
            },
            Err(e) => {
                tracing::warn!("Failed to read config {}: {}", path, e);
                FileConfig::default()
            }
        },
        None => FileConfig::default(),
    };

    // Configuration precedence: CLI/env > config file > defaults
    let udp_port = args.udp_port.or(file_cfg.udp_port).unwrap_or(18888);
    let rpc_port = args.rpc_port.or(file_cfg.rpc_port).unwrap_or(12345);

    // Initialize logging; respect RUST_LOG if set, default to info
    tracing_subscriber::fmt()
        .with_target(true)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Exit channel
    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // Shared subscriptions map for RPC and deshred
    let subscriptions = Arc::new(DashMap::<String, Subscription>::new());

    // Start RPC server
    let rpc_addr: SocketAddr = format!("0.0.0.0:{}", rpc_port).parse()?;
    let rpc_server = rpc::start_rpc_server(rpc_addr, subscriptions.clone()).await?;
    info!("RPC server started on port {}", rpc_port);

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
    let trace_log_path = args
        .trace_log_path
        .clone()
        .or(file_cfg.trace_log_path.clone());
    let reconstruct_handle = tokio::spawn(async move {
        let _ = deshred::reconstruct_shreds_server(
            reconstruct_shutdown,
            reconstruct_rx,
            subscriptions,
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
