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

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, help = "Path to the trace log file")]
    trace_log_path: Option<String>,

    #[arg(
        long,
        default_value = "18999",
        help = "UDP port for receiving shred stream"
    )]
    udp_port: u16,

    #[arg(long, default_value = "28899", help = "Port for RPC server")]
    rpc_port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Configuration
    let udp_port = args.udp_port;
    let rpc_port = args.rpc_port;

    // Initialize logging;
    tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
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
    let trace_log_path = args.trace_log_path.clone();
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

    // Close RPC server from a blocking context to avoid dropping a runtime
    // inside Tokio's async context (jsonrpc spawns its own runtime internally).
    let _ = tokio::task::spawn_blocking(move || {
        rpc_server.close();
    })
    .await;

    udp_handle.await?;
    reconstruct_handle.await?;

    info!("All tasks have shut down.");
    Ok(())
}

pub async fn wait_for_shutdown() {
    if signal::ctrl_c().await.is_ok() {
        info!("receive SIGINT（Ctrl+C / kill -2）");
    }
}
