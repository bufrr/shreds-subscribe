use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use std::fs;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_keypair(privkey_arg: &str) -> anyhow::Result<Keypair> {
    // Check if it looks like a file path (contains '/' or '\' or ends with common extensions)
    if privkey_arg.contains('/')
        || privkey_arg.contains('\\')
        || privkey_arg.ends_with(".json")
        || privkey_arg.ends_with(".key")
    {
        if !Path::new(privkey_arg).exists() {
            anyhow::bail!("Keypair file not found: {}", privkey_arg);
        }
        let data = fs::read_to_string(privkey_arg)?;
        let bytes: Vec<u8> = serde_json::from_str(&data)?;
        if bytes.len() != 64 {
            anyhow::bail!("keypair must be 64 bytes, got {}", bytes.len());
        }
        let kp = Keypair::try_from(&bytes[..])?;
        return Ok(kp);
    }
    if privkey_arg.trim_start().starts_with('[') {
        let bytes: Vec<u8> = serde_json::from_str(privkey_arg)?;
        if bytes.len() != 64 {
            anyhow::bail!("keypair must be 64 bytes, got {}", bytes.len());
        }
        let kp = Keypair::try_from(&bytes[..])?;
        return Ok(kp);
    }
    // Try to decode as base58
    let decoded = solana_sdk::bs58::decode(privkey_arg).into_vec()?;
    if decoded.len() != 64 {
        anyhow::bail!("base58 key must decode to 64 bytes, got {}", decoded.len());
    }
    let kp = Keypair::try_from(&decoded[..])?;
    Ok(kp)
}

fn subscribe_tx(
    subscribe_url: &str,
    tx_sig: &str,
    ts_ms: u64,
) -> anyhow::Result<serde_json::Value> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "subscribe_tx",
        "params": { "tx_sig": tx_sig, "timestamp": ts_ms },
    });
    match ureq::post(subscribe_url).send_json(&payload) {
        Ok(mut resp) => {
            let v: serde_json::Value = resp.body_mut().read_json()?;
            Ok(v)
        }
        Err(ureq::Error::StatusCode(code)) => {
            anyhow::bail!("subscribe http status {}", code);
        }
        Err(e) => anyhow::bail!("request error: {}", e),
    }
}

#[test]
#[ignore] // Requires network, a running shreds-subscribe, and shreds feed
fn e2e_send_self_and_subscribe() -> anyhow::Result<()> {
    // Load .env if present (PRIVKEY, SOLANA_RPC_URL, SUBSCRIBE_URL, AMOUNT_SOL)
    let _ = dotenvy::dotenv();
    // Configuration from env
    let privkey_arg = std::env::var("PRIVKEY")
        .expect("set PRIVKEY to a keypair path, json array, or base58-64-byte");
    let rpc_url =
        std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8899".to_string());
    let subscribe_url =
        std::env::var("SUBSCRIBE_URL").unwrap_or_else(|_| "http://127.0.0.1:28899".to_string());
    let amount_sol: f64 = std::env::var("AMOUNT_SOL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.01);

    let kp = parse_keypair(&privkey_arg)?;
    let from = kp.pubkey();
    println!("Using pubkey: {}", from);

    let lamports = (amount_sol * LAMPORTS_PER_SOL as f64) as u64;
    // Create transfer instruction manually (self-transfer)
    // System program ID: 11111111111111111111111111111111
    let system_program_id = Pubkey::new_from_array([0u8; 32]);
    let ix = Instruction::new_with_bincode(
        system_program_id,
        &(2u32, from, from, lamports), // SystemInstruction::Transfer
        vec![AccountMeta::new(from, true), AccountMeta::new(from, false)],
    );
    let msg = Message::new(&[ix], Some(&from));

    let client = RpcClient::new(rpc_url.clone());
    let bh: Hash = client.get_latest_blockhash()?;

    let mut tx = Transaction::new_unsigned(msg);
    tx.try_sign(&[&kp], bh)?;
    let tx_sig = tx.signatures[0].to_string();
    let ts_ms = now_ms();
    println!("Signature: {}", tx_sig);

    // 1) Subscribe before sending
    let v = subscribe_tx(&subscribe_url, &tx_sig, ts_ms)?;
    if v.get("error").is_some() {
        anyhow::bail!("subscribe error: {}", v);
    }
    println!("Subscribed at {} ms to {}", ts_ms, tx_sig);

    // 2) Send transaction to cluster
    let sent_sig = client.send_transaction(&tx)?;
    println!("Submitted tx: {}", sent_sig);

    // 3) Optionally poll to see if the subscription was consumed
    // We re-attempt subscription; if it succeeds, the previous one was consumed by the server.
    // If we still get an error "Transaction already subscribed", keep waiting.
    let timeout = Duration::from_secs(60);
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "timeout waiting for subscriber to observe shreds for {}",
                tx_sig
            );
        }
        sleep(Duration::from_secs(2));
        let v = subscribe_tx(&subscribe_url, &tx_sig, now_ms())?;
        if let Some(err) = v.get("error") {
            // Expecting code -32600 and message "Transaction already subscribed" until consumed
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if code == -32600 && msg.contains("already subscribed") {
                println!("still waiting for subscriber to observe shreds...");
                continue;
            }
            anyhow::bail!("unexpected subscribe error: {}", err);
        } else {
            println!(
                "subscriber consumed initial subscription; shreds observed for {}",
                tx_sig
            );
            break;
        }
    }

    Ok(())
}
