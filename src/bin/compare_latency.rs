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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Compare latency between Shred, local, and Helius WebSocket notifications"
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

    #[arg(long, help = "Shred WebSocket URL (e.g., ws://127.0.0.1:38899)")]
    shred_ws: Option<String>,

    #[arg(
        long,
        help = "Standard Solana RPC WebSocket URL (e.g., ws://127.0.0.1:8900)"
    )]
    local_ws: Option<String>,

    #[arg(
        long,
        help = "Local Enhanced WebSocket URL (e.g., ws://127.0.0.1:8901)"
    )]
    local_enhanced_ws: Option<String>,

    #[arg(long, help = "Helius WebSocket URL (including ?api-key=YOUR_KEY)")]
    helius_ws: Option<String>,

    #[arg(long, default_value = "0.01", help = "Amount to transfer in SOL")]
    amount: f64,
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
    format!("{:.3} ms", latency_ms)
}

async fn subscribe_shred_ws(ws_url: &str, signature: &str) -> Result<(WsStream, u64)> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .context("Failed to connect to Shred WebSocket")?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "signatureSubscribe",
        "params": [signature, {"commitment": "processed"}]
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

    println!("✓ Shred WS subscribed (ID: {})", sub_id);
    Ok((ws, sub_id))
}

async fn subscribe_solana_ws(ws_url: &str, signature: &str) -> Result<(WsStream, u64)> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .context("Failed to connect to standard Solana WebSocket")?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "signatureSubscribe",
        "params": [signature, {"commitment": "processed"}]
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

async fn subscribe_local_enhanced_ws(
    ws_url: &str,
    _signature: &str,
    wallet_pubkey: &str,
) -> Result<(WsStream, u64)> {
    let (mut ws, _) = connect_async(ws_url)
        .await
        .context("Failed to connect to Local Enhanced WebSocket")?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "transactionSubscribe",
        "params": [
            {
                "failed": false,
                "accounts": {
                    "include": [wallet_pubkey]
                }
            },
            {
                "commitment": "processed",
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

    if let Some(error) = value.get("error") {
        anyhow::bail!("Local Enhanced subscription error: {}", error);
    }

    let sub_id = value
        .get("result")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
        .with_context(|| format!("Missing subscription ID. Response: {}", response_text))?;

    println!("✓ Local Enhanced WS subscribed (ID: {})", sub_id);
    Ok((ws, sub_id))
}

async fn subscribe_helius_ws(
    ws_url: &str,
    _signature: &str,
    wallet_pubkey: &str,
) -> Result<(WsStream, u64)> {
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
                "commitment": "processed",
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

async fn handle_ws_message(
    msg: Result<Message, WsError>,
    expected_method: &str,
    source_name: &str,
    signature_filter: Option<&str>,
    tx: &tokio::sync::mpsc::Sender<(String, u128)>,
    ws: &mut WsStream,
) -> bool {
    match msg {
        Ok(Message::Text(text)) => {
            let received_micros = now_micros();
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if value.get("method").and_then(|m| m.as_str()) == Some(expected_method) {
                    if let Some(filter) = signature_filter {
                        let maybe_sig = value
                            .get("params")
                            .and_then(|p| p.get("result"))
                            .and_then(|r| r.get("signature"))
                            .and_then(|s| s.as_str())
                            .or_else(|| {
                                value
                                    .get("params")
                                    .and_then(|p| p.get("result"))
                                    .and_then(|r| r.get("value"))
                                    .and_then(|v| v.get("transaction"))
                                    .and_then(|t| t.get("transaction"))
                                    .and_then(|t| t.get("signatures"))
                                    .and_then(|s| s.get(0))
                                    .and_then(|s| s.as_str())
                            });

                        if maybe_sig != Some(filter) {
                            if let Some(sig) = maybe_sig {
                                let preview_len = sig.len().min(8);
                                println!(
                                    "⚠ {} ignored notification for signature {}",
                                    source_name,
                                    &sig[..preview_len]
                                );
                            } else {
                                println!(
                                    "⚠ {} ignored notification missing signature field",
                                    source_name
                                );
                            }
                            return false;
                        }
                    }

                    let _ = tx.send((source_name.to_string(), received_micros)).await;
                    let received_ms = (received_micros / 1_000) as u64;
                    println!(
                        "✓ {} notification received at {}",
                        source_name,
                        format_hms_millis(received_ms)
                    );
                    return true;
                }
            }
            false
        }
        Ok(Message::Ping(p)) => {
            let _ = ws.send(Message::Pong(p)).await;
            false
        }
        Ok(Message::Close(_)) => true,
        Ok(Message::Binary(_)) | Ok(Message::Pong(_)) => false,
        Err(e) => {
            println!("⚠ {} WebSocket error: {}", source_name, e);
            true
        }
        Ok(Message::Frame(_)) => false,
    }
}

fn spawn_ws_listener(
    mut ws: WsStream,
    expected_method: &'static str,
    source_name: &'static str,
    signature_filter: Option<String>,
    tx: tokio::sync::mpsc::Sender<(String, u128)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = ws.next().await {
            let should_break = handle_ws_message(
                msg,
                expected_method,
                source_name,
                signature_filter.as_deref(),
                &tx,
                &mut ws,
            )
            .await;
            if should_break {
                break;
            }
        }
    })
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
    let transaction: Transaction =
        system_transaction::transfer(&keypair, &keypair.pubkey(), lamports, blockhash);
    let signature = transaction.signatures[0].to_string();

    println!("\nTransaction signature: {}", signature);
    println!("Amount: {} SOL", args.amount);

    if args.shred_ws.is_none()
        && args.local_ws.is_none()
        && args.local_enhanced_ws.is_none()
        && args.helius_ws.is_none()
    {
        anyhow::bail!(
            "At least one WebSocket endpoint must be provided (--shred-ws, --local-ws, --local-enhanced-ws, or --helius-ws)"
        );
    }

    let has_shred = args.shred_ws.is_some();
    let has_local = args.local_ws.is_some();
    let has_local_enhanced = args.local_enhanced_ws.is_some();
    let has_helius = args.helius_ws.is_some();

    let endpoint_count = [has_shred, has_local, has_local_enhanced, has_helius]
        .iter()
        .filter(|&&x| x)
        .count();

    // Subscribe to requested WebSockets BEFORE sending the transaction
    println!("\n📡 Subscribing to WebSockets...");
    let wallet_str = keypair.pubkey().to_string();

    let shred_ws_stream = if let Some(shred_url) = args.shred_ws.as_deref() {
        let (stream, _shred_sub_id) = subscribe_shred_ws(shred_url, &signature).await?;
        Some(stream)
    } else {
        None
    };

    let local_ws_stream = if let Some(local_url) = args.local_ws.as_deref() {
        let (stream, _local_sub_id) = subscribe_solana_ws(local_url, &signature).await?;
        Some(stream)
    } else {
        None
    };

    let local_enhanced_ws_stream =
        if let Some(local_enhanced_url) = args.local_enhanced_ws.as_deref() {
            let (stream, _local_enhanced_sub_id) =
                subscribe_local_enhanced_ws(local_enhanced_url, &signature, &wallet_str).await?;
            Some(stream)
        } else {
            None
        };

    let helius_ws_stream = if let Some(helius_url) = args.helius_ws.as_deref() {
        let (stream, _helius_sub_id) =
            subscribe_helius_ws(helius_url, &signature, &wallet_str).await?;
        Some(stream)
    } else {
        None
    };

    println!("\n⏳ Preparing to listen for notifications...\n");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, u128)>(endpoint_count);
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    if let Some(shred_ws) = shred_ws_stream {
        let channel = tx.clone();
        handles.push(spawn_ws_listener(
            shred_ws,
            "signatureNotification",
            "Shred WS",
            None,
            channel,
        ));
    }

    if let Some(local_ws_stream) = local_ws_stream {
        let channel = tx.clone();
        handles.push(spawn_ws_listener(
            local_ws_stream,
            "signatureNotification",
            "Local WS",
            None,
            channel,
        ));
    }

    if let Some(local_enhanced_ws) = local_enhanced_ws_stream {
        let channel = tx.clone();
        handles.push(spawn_ws_listener(
            local_enhanced_ws,
            "transactionNotification",
            "Local Enhanced WS",
            Some(signature.clone()),
            channel,
        ));
    }

    if let Some(helius_ws) = helius_ws_stream {
        let channel = tx.clone();
        handles.push(spawn_ws_listener(
            helius_ws,
            "transactionNotification",
            "Helius WS",
            Some(signature.clone()),
            channel,
        ));
    }

    // Send transaction
    println!("\n🚀 Sending transaction...");
    let tx_sent_at_micros = now_micros();
    client
        .send_and_confirm_transaction(&transaction)
        .context("Failed to send transaction")?;
    let tx_sent_at_ms = (tx_sent_at_micros / 1_000) as u64;
    println!("✓ Transaction sent at {}", format_human(tx_sent_at_ms));

    // Wait for notifications from both sources
    println!("\n⏳ Waiting for notifications (max 60s)...\n");

    // Wait for WebSocket tasks with timeout
    let wait_for_tasks = async move {
        for handle in handles {
            let _ = handle.await;
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(60), wait_for_tasks).await;

    drop(tx);
    let mut results = Vec::new();
    while let Some((source, micros)) = rx.recv().await {
        results.push((source, micros));
    }
    results.sort_by_key(|entry| entry.1);

    let mut shred_micros: Option<u128> = None;
    let mut local_micros: Option<u128> = None;
    let mut local_enhanced_micros: Option<u128> = None;
    let mut helius_micros: Option<u128> = None;

    for (source, micros) in &results {
        match source.as_str() {
            "Shred WS" => shred_micros = Some(*micros),
            "Local WS" => local_micros = Some(*micros),
            "Local Enhanced WS" => local_enhanced_micros = Some(*micros),
            "Helius WS" => helius_micros = Some(*micros),
            _ => {}
        }
    }

    let mut table_rows: Vec<(&str, Option<u128>)> = Vec::new();
    if has_shred {
        table_rows.push(("Shred WS", shred_micros));
    }
    if has_local {
        table_rows.push(("Local WS", local_micros));
    }
    if has_local_enhanced {
        table_rows.push(("Local Enhanced WS", local_enhanced_micros));
    }
    if has_helius {
        table_rows.push(("Helius WS", helius_micros));
    }

    // Print comparison table
    println!("\n┌──────────────────┬─────────────────────────────────┬──────────────┐");
    println!("│ Source           │ Notification Time               │ Latency (ms) │");
    println!("├──────────────────┼─────────────────────────────────┼──────────────┤");

    for (label, maybe_micros) in &table_rows {
        match maybe_micros {
            Some(value) => {
                let ts_ms = (value / 1_000) as u64;
                let latency_micros = value.saturating_sub(tx_sent_at_micros);
                println!(
                    "│ {:<16} │ {:<31} │ {:>12} │",
                    label,
                    format_human(ts_ms),
                    format_latency(latency_micros)
                );
            }
            None => {
                println!("│ {:<16} │ {:^31} │ {:^12} │", label, "TIMEOUT", "N/A");
            }
        }
    }

    println!("└──────────────────┴─────────────────────────────────┴──────────────┘");

    let mut arrival_times: Vec<(&str, u128)> = table_rows
        .iter()
        .filter_map(|(label, maybe)| maybe.map(|value| (*label, value)))
        .collect();
    arrival_times.sort_by_key(|(_, micros)| *micros);

    if arrival_times.is_empty() {
        println!("\n❌ All sources timed out");
    } else {
        let (winner_label, winner_micros) = arrival_times[0];
        let winner_latency = winner_micros.saturating_sub(tx_sent_at_micros);
        println!(
            "\n🏆 Winner: {} ({} latency)",
            winner_label,
            format_latency(winner_latency)
        );

        println!("\nDifferences vs winner:");
        for (label, maybe) in &table_rows {
            match maybe {
                Some(value) => {
                    let diff = value.saturating_sub(winner_micros);
                    if diff == 0 {
                        println!("• {}: tie", label);
                    } else {
                        println!("• {}: +{}", label, format_latency(diff));
                    }
                }
                None => {
                    println!("• {}: TIMEOUT", label);
                }
            }
        }
    }

    Ok(())
}
