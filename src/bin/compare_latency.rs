use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::hash::Hash;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use solana_system_transaction as system_transaction;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
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

    #[arg(long, default_value = "http://127.0.0.1:8899", help = "Solana RPC URL")]
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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn format_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let dt = chrono::DateTime::from_timestamp(secs as i64, (millis * 1_000_000) as u32)
        .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
    dt.format("%Y-%m-%d %H:%M:%S%.3f UTC").to_string()
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

    ws.send(Message::Text(request.to_string()))
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
    signature: &str,
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
                "signature": signature,
                "failed": false
            },
            {
                "commitment": "confirmed",
                "encoding": "jsonParsed",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0
            }
        ]
    });

    ws.send(Message::Text(request.to_string()))
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
    let sub_id = value
        .get("result")
        .and_then(|v| v.as_u64())
        .context("Missing subscription ID")?;

    println!("✓ Helius WS subscribed (ID: {})", sub_id);
    Ok((ws, sub_id))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load keypair
    println!("Loading keypair from {}...", args.keypair);
    let keypair_data = fs::read_to_string(&args.keypair)
        .with_context(|| format!("Failed to read keypair file: {}", args.keypair))?;
    let keypair_bytes: Vec<u8> = serde_json::from_str(&keypair_data)
        .context("Failed to parse keypair JSON (expected array of 64 bytes)")?;
    let keypair = Keypair::from_bytes(&keypair_bytes).context("Invalid keypair bytes")?;

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
    let (mut local_ws, _local_sub_id) = subscribe_local_ws(&args.local_ws, &signature).await?;
    let (mut helius_ws, _helius_sub_id) = subscribe_helius_ws(&args.helius_ws, &signature).await?;

    // Send transaction
    println!("\n🚀 Sending transaction...");
    let tx_sent_at = now_ms();
    client
        .send_and_confirm_transaction(&tx)
        .context("Failed to send transaction")?;
    println!("✓ Transaction sent at {}", format_timestamp(tx_sent_at));

    // Wait for notifications from both sources
    println!("\n⏳ Waiting for notifications (max 60s)...\n");

    let local_notified_at = Arc::new(Mutex::new(None::<u64>));
    let helius_notified_at = Arc::new(Mutex::new(None::<u64>));

    let local_notified_clone = local_notified_at.clone();
    let helius_notified_clone = helius_notified_at.clone();

    let local_task = tokio::spawn(async move {
        while let Some(msg) = local_ws.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if value.get("method").and_then(|m| m.as_str())
                            == Some("signatureNotification")
                        {
                            let ts = now_ms();
                            *local_notified_clone.lock().await = Some(ts);
                            println!("✓ Local WS notification received");
                            break;
                        }
                    }
                }
                Ok(Message::Ping(p)) => {
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
                            let ts = now_ms();
                            *helius_notified_clone.lock().await = Some(ts);
                            println!("✓ Helius WS notification received");
                            break;
                        }
                    }
                }
                Ok(Message::Ping(p)) => {
                    let _ = helius_ws.send(Message::Pong(p)).await;
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => continue,
            }
        }
    });

    // Wait for both tasks with timeout
    let _ = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::join!(local_task, helius_task),
    )
    .await;

    let local_ts = *local_notified_at.lock().await;
    let helius_ts = *helius_notified_at.lock().await;

    // Print comparison table
    println!("\n┌─────────────────────────────────────────────────────────────────┐");
    println!("│                    LATENCY COMPARISON                           │");
    println!("├─────────────────────┬───────────────────────────┬───────────────┤");
    println!("│ Source              │ Notification Time         │ Latency (ms)  │");
    println!("├─────────────────────┼───────────────────────────┼───────────────┤");

    if let Some(local_ts) = local_ts {
        let latency = local_ts.saturating_sub(tx_sent_at);
        println!(
            "│ Local WS            │ {} │ {:>13} │",
            format_timestamp(local_ts),
            format!("{} ms", latency)
        );
    } else {
        println!(
            "│ Local WS            │ {:^25} │ {:^13} │",
            "TIMEOUT", "N/A"
        );
    }

    if let Some(helius_ts) = helius_ts {
        let latency = helius_ts.saturating_sub(tx_sent_at);
        println!(
            "│ Helius WS           │ {} │ {:>13} │",
            format_timestamp(helius_ts),
            format!("{} ms", latency)
        );
    } else {
        println!(
            "│ Helius WS           │ {:^25} │ {:^13} │",
            "TIMEOUT", "N/A"
        );
    }

    println!("└─────────────────────┴───────────────────────────┴───────────────┘");

    // Determine winner
    match (local_ts, helius_ts) {
        (Some(local), Some(helius)) => {
            let diff = local.abs_diff(helius);
            if local < helius {
                println!("\n🏆 Winner: Local WS ({} ms faster)", diff);
            } else if helius < local {
                println!("\n🏆 Winner: Helius WS ({} ms faster)", diff);
            } else {
                println!("\n🤝 Tie: Both notified at the same time");
            }
        }
        (Some(_), None) => println!("\n🏆 Winner: Local WS (Helius timed out)"),
        (None, Some(_)) => println!("\n🏆 Winner: Helius WS (Local timed out)"),
        (None, None) => println!("\n❌ Both sources timed out"),
    }

    Ok(())
}
