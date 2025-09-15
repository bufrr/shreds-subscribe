use crossbeam_channel::unbounded;
use solana_streamer::packet::{PacketBatch, PacketBatchRecycler};
use solana_streamer::streamer::{self, StreamerReceiveStats};
use std::net::UdpSocket as StdUdpSocket;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use tracing::info;

pub async fn run_udp_server(
    port: u16,
    mut shutdown_rx: Receiver<()>,
    reconstruct_tx: crossbeam_channel::Sender<PacketBatch>,
) -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{}", port);
    info!("Shred UDP server (streamer-based) listening on {}", addr);

    let std_socket = StdUdpSocket::bind(&addr)?;
    std_socket.set_nonblocking(true)?;
    let socket = Arc::new(std_socket);

    let exit = Arc::new(AtomicBool::new(false));
    let (packet_sender, packet_receiver) = unbounded();

    let recycler = PacketBatchRecycler::warmed(100, 1024);

    let stream_handle = std::thread::Builder::new()
        .name("shred-recv-thread".to_string())
        .spawn({
            let socket = socket.clone();
            let exit = exit.clone();
            let packet_sender = packet_sender.clone();
            let recycler = recycler.clone();
            move || {
                streamer::receiver(
                    "shred-recv".parse().unwrap(),
                    socket,
                    exit,
                    packet_sender,
                    recycler,
                    Arc::new(StreamerReceiveStats::new("packet_modifier")),
                    Some(Duration::from_millis(1)),
                    true,
                    None,
                    false,
                );
            }
        })?;

    let shutdown_exit = exit.clone();
    use std::thread;

    let receiver_handle = thread::spawn({
        let shutdown_flag = shutdown_exit.clone();
        move || {
            loop {
                if shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    info!("Shred receiver thread exiting...");
                    return;
                }
                if let Ok(batch) = packet_receiver.recv_timeout(Duration::from_millis(100)) {
                    if let Err(e) = reconstruct_tx.try_send(batch) {
                        tracing::warn!(
                            "Failed to send packet batch for reconstruction, channel may be full: {:?}",
                            e
                        );
                    }
                }
            }
        }
    });

    shutdown_rx.recv().await.ok();
    info!("Shred UDP server received shutdown signal");

    exit.store(true, std::sync::atomic::Ordering::Relaxed);

    stream_handle.join().unwrap();
    receiver_handle.join().unwrap();

    info!("Shred UDP server exit");
    Ok(())
}
