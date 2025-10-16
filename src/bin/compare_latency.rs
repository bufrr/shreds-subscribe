use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::hash::Hash;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use solana_system_transaction as system_transaction;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Compare latency between local and Helius WebSocket notifications"
)]
struct Args {
    #[arg(long, help = "Path to keypair JSON file")]
    keypair: String,

    #[arg(
        long,
        default_value = "https://api.mainnet-beta.solana.com",
        help = "Solana RPC URL"
    )]
    rpc_url: String,

    #[arg(
        long,
        default_value = "ws://127.0.0.1:38899",
        help = "Local WebSocket URL"
    )]
    local_ws: String,

    #[arg(long, help = "Helius WebSocket URL (including ?api-key=YOUR_KEY)")]
    helius_ws: String,

    #[arg(long, default_value = "0.01", help = "Amount to transfer in SOL")]
    amount: f64,

    #[arg(
        long,
        help = "Enable verbose diagnostics for timestamp capture investigation"
    )]
    verbose: bool,
}
fn now_ms() -> u64 {
    let system_now = SystemTime::now();
    let duration = system_now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let millis = duration.as_millis() as u64;

    if VERBOSE_LOGGING.load(Ordering::Relaxed) {
        let seq = NOW_MS_CALL_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
        let micros = duration.as_micros();
        println!(
            "[DIAGNOSTIC][now_ms][seq={}] Captured {} ms ({} μs) [thread {:?}] at {:?}",
            seq,
            millis,
            micros,
            thread::current().id(),
            system_now
        );
    }

    millis
}

fn now_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

fn format_human(ts_ms: u64) -> String {
    let secs = (ts_ms / 1_000) as i64;
    let sub_ms = (ts_ms % 1_000) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, sub_ms * 1_000_000)
        .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
    dt.format("%Y-%m-%d %H:%M:%S%.3f UTC").to_string()
}

fn format_hms_millis(ts_ms: u64) -> String {
    let secs = (ts_ms / 1_000) as i64;
    let sub_ms = (ts_ms % 1_000) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, sub_ms * 1_000_000)
        .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
    dt.format("%H:%M:%S%.3f").to_string()
}

fn format_latency(latency_micros: u128) -> String {
    let latency_ms = latency_micros as f64 / 1_000.0;
    format!("{:.3} ms ({} μs)", latency_ms, latency_micros)
}

// Temporary diagnostics to investigate identical timestamp reporting across WebSocket tasks.
static VERBOSE_LOGGING: AtomicBool = AtomicBool::new(false);
static NOW_MS_CALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn current_micro_ts() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
}

async fn subscribe_local_ws(
    ws_url: &str,
    signature: &str,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u64,
)> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .context("Failed to connect to local WebSocket")?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "signatureSubscribe",
        "params": [signature, {"commitment": "confirmed"}]
    });

    ws.send(Message::Text(request.to_string().into()))
        .await
        .context("Failed to send signatureSubscribe")?;

    let response_text = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => break Ok(text),
                Some(Ok(Message::Ping(p))) => {
                    ws.send(Message::Pong(p)).await?;
                }
                Some(Ok(Message::Close(_))) => anyhow::bail!("WebSocket closed"),
                Some(Err(e)) => return Err(e.into()),
                None => anyhow::bail!("WebSocket closed"),
                _ => continue,
            }
        }
    })
    .await
    .context("Timeout waiting for subscription response")??;

    let value: serde_json::Value =
        serde_json::from_str(&response_text).context("Failed to parse subscription response")?;
    let sub_id = value
        .get("result")
        .and_then(|v| v.as_u64())
        .context("Missing subscription ID")?;

    println!("✓ Local WS subscribed (ID: {})", sub_id);
    Ok((ws, sub_id))
}

async fn subscribe_helius_ws(
    ws_url: &str,
    _signature: &str,
    wallet_pubkey: &str,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u64,
)> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .context("Failed to connect to Helius WebSocket")?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "transactionSubscribe",
        "params": [
            {
                "failed": false,
                "accountInclude": [wallet_pubkey]
            },
            {
                "commitment": "confirmed",
                "encoding": "jsonParsed",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0
            }
        ]
    });

    ws.send(Message::Text(request.to_string().into()))
        .await
        .context("Failed to send transactionSubscribe")?;

    let response_text = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => break Ok(text),
                Some(Ok(Message::Ping(p))) => {
                    ws.send(Message::Pong(p)).await?;
                }
                Some(Ok(Message::Close(_))) => anyhow::bail!("WebSocket closed"),
                Some(Err(e)) => return Err(e.into()),
                None => anyhow::bail!("WebSocket closed"),
                _ => continue,
            }
        }
    })
    .await
    .context("Timeout waiting for subscription response")??;

    let value: serde_json::Value =
        serde_json::from_str(&response_text).context("Failed to parse subscription response")?;

    // Check for error response
    if let Some(error) = value.get("error") {
        anyhow::bail!("Helius subscription error: {}", error);
    }

    // Handle both number and string subscription IDs
    let sub_id = value
        .get("result")
        .and_then(|v| {
            // Try as u64 first, then try parsing string as u64
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
        .with_context(|| format!("Missing subscription ID. Response: {}", response_text))?;

    println!("✓ Helius WS subscribed (ID: {})", sub_id);
    Ok((ws, sub_id))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Flip global switch for temporary diagnostic logging.
    VERBOSE_LOGGING.store(args.verbose, Ordering::Relaxed);
    if args.verbose {
        println!("[DIAGNOSTIC] Verbose diagnostics enabled");
    }

    // Load keypair
    println!("Loading keypair from {}...", args.keypair);
    let keypair_data = fs::read_to_string(&args.keypair)
        .with_context(|| format!("Failed to read keypair file: {}", args.keypair))?;
    let keypair_bytes: Vec<u8> = serde_json::from_str(&keypair_data)
        .context("Failed to parse keypair JSON (expected array of 64 bytes)")?;

    // Extract the first 32 bytes as the secret key
    if keypair_bytes.len() < 32 {
        anyhow::bail!("Keypair file must contain at least 32 bytes");
    }
    let mut secret_key = [0u8; 32];
    secret_key.copy_from_slice(&keypair_bytes[..32]);
    let keypair = Keypair::new_from_array(secret_key);

    println!("Wallet: {}", keypair.pubkey());

    // Build transaction
    let client =
        RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::confirmed());
    let blockhash: Hash = client
        .get_latest_blockhash()
        .context("Failed to get latest blockhash")?;

    let lamports = (args.amount * LAMPORTS_PER_SOL as f64) as u64;
    let tx: Transaction =
        system_transaction::transfer(&keypair, &keypair.pubkey(), lamports, blockhash);
    let signature = tx.signatures[0].to_string();

    println!("\nTransaction signature: {}", signature);
    println!("Amount: {} SOL", args.amount);

    // Subscribe to both WebSockets BEFORE sending transaction
    println!("\n📡 Subscribing to WebSockets...");
    let wallet_str = keypair.pubkey().to_string();
    let (mut local_ws, _local_sub_id) = subscribe_local_ws(&args.local_ws, &signature).await?;
    let (mut helius_ws, _helius_sub_id) =
        subscribe_helius_ws(&args.helius_ws, &signature, &wallet_str).await?;

    // Send transaction
    println!("\n🚀 Sending transaction...");
    let tx_sent_at = now_ms();
    let tx_sent_at_micros = now_micros();
    client
        .send_and_confirm_transaction(&tx)
        .context("Failed to send transaction")?;
    println!("✓ Transaction sent at {}", format_human(tx_sent_at));

    // Wait for notifications from both sources
    println!("\n⏳ Waiting for notifications (max 60s)...\n");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, u128)>(2);
    let tx_local = tx.clone();
    let tx_helius = tx.clone();
    let expected_sig_helius = signature.clone();
    let verbose = args.verbose;
    let local_verbose = verbose;
    let helius_verbose = verbose;

    let local_task = tokio::spawn(async move {
        while let Some(msg) = local_ws.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if value.get("method").and_then(|m| m.as_str())
                            == Some("signatureNotification")
                        {
                            let received_micros = now_micros();
                            let _ = tx_local.send(("LOCAL".to_string(), received_micros)).await;
                            let received_ms = (received_micros / 1_000) as u64;
                            println!(
                                "✓ Local WS notification received at {} ({} μs)",
                                format_hms_millis(received_ms),
                                received_micros
                            );
                            break;
                        }
                    }
                }
                Ok(Message::Ping(p)) => {
                    if local_verbose {
                        println!(
                            "[DIAGNOSTIC][local] Responding to ping at {} μs",
                            current_micro_ts()
                        );
                    }
                    let _ = local_ws.send(Message::Pong(p)).await;
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => continue,
            }
        }
    });

    let helius_task = tokio::spawn(async move {
        while let Some(msg) = helius_ws.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if value.get("method").and_then(|m| m.as_str())
                            == Some("transactionNotification")
                        {
                            // IMPORTANT: Verify this is OUR transaction, not just any transaction for this wallet
                            let sig = value
                                .get("params")
                                .and_then(|p| p.get("result"))
                                .and_then(|r| r.get("signature"))
                                .and_then(|s| s.as_str());

                            if sig == Some(expected_sig_helius.as_str()) {
                                let received_micros = now_micros();
                                let _ = tx_helius
                                    .send(("HELIUS".to_string(), received_micros))
                                    .await;
                                let received_ms = (received_micros / 1_000) as u64;
                                println!(
                                    "✓ Helius WS notification received at {} ({} μs)",
                                    format_hms_millis(received_ms),
                                    received_micros
                                );
                                break;
                            } else {
                                println!(
                                    "⚠ Helius sent notification for different transaction: {:?}",
                                    sig.map(|s| &s[..8])
                                );
                            }
                        }
                    }
                }
                Ok(Message::Ping(p)) => {
                    if helius_verbose {
                        println!(
                            "[DIAGNOSTIC][helius] Responding to ping at {} μs",
                            current_micro_ts()
                        );
                    }
                    let _ = helius_ws.send(Message::Pong(p)).await;
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => continue,
            }
        }
    });

    // Wait for both tasks with timeout
    let _ = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(local_task, helius_task)
    })
    .await;

    drop(tx);
    let mut results = Vec::new();
    while let Some((source, micros)) = rx.recv().await {
        results.push((source, micros));
    }
    results.sort_by_key(|entry| entry.1);

    let mut local_micros: Option<u128> = None;
    let mut helius_micros: Option<u128> = None;

    for (source, micros) in &results {
        match source.as_str() {
            "LOCAL" => local_micros = Some(*micros),
            "HELIUS" => helius_micros = Some(*micros),
            _ => {}
        }
    }

    // Print comparison table
    println!("\n┌─────────────────────────────────────────────────────────────────┐");
    println!("│                    LATENCY COMPARISON                           │");
    println!("├─────────────────────┬───────────────────────────┬─────────────────────┤");
    println!("│ Source              │ Notification Time         │ Latency (ms / μs)   │");
    println!("├─────────────────────┼───────────────────────────┼─────────────────────┤");

    match local_micros {
        Some(local_value) => {
            let local_ms = (local_value / 1_000) as u64;
            let latency_micros = local_value.saturating_sub(tx_sent_at_micros);
            println!(
                "│ Local WS            │ {:<25} │ {:>19} │",
                format_human(local_ms),
                format_latency(latency_micros)
            );
        }
        None => {
            println!(
                "│ Local WS            │ {:^25} │ {:^19} │",
                "TIMEOUT", "N/A"
            );
        }
    }

    match helius_micros {
        Some(helius_value) => {
            let helius_ms = (helius_value / 1_000) as u64;
            let latency_micros = helius_value.saturating_sub(tx_sent_at_micros);
            println!(
                "│ Helius WS           │ {:<25} │ {:>19} │",
                format_human(helius_ms),
                format_latency(latency_micros)
            );
        }
        None => {
            println!(
                "│ Helius WS           │ {:^25} │ {:^19} │",
                "TIMEOUT", "N/A"
            );
        }
    }

    println!("└─────────────────────┴───────────────────────────┴─────────────────────┘");

    // Determine winner
    match (local_micros, helius_micros) {
        (Some(local), Some(helius)) => {
            let (diff_micros, helius_faster) = if local <= helius {
                (helius.saturating_sub(local), false)
            } else {
                (local.saturating_sub(helius), true)
            };

            let diff_string = format_latency(diff_micros);

            println!("\nDifference: {}", diff_string);

            if diff_micros == 0 {
                println!("\n🤝 Tie: Both notified at the same time");
            } else if helius_faster {
                println!("\n🏆 Winner: Helius WS ({} faster)", diff_string);
            } else {
                println!("\n🏆 Winner: Local WS ({} faster)", diff_string);
            }
        }
        (Some(_), None) => println!("\n🏆 Winner: Local WS (Helius timed out)"),
        (None, Some(_)) => println!("\n🏆 Winner: Helius WS (Local timed out)"),
        (None, None) => println!("\n❌ Both sources timed out"),
    }

    Ok(())
}
