use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use reqwest::Client;
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::{Keypair, Signer, read_keypair_file};
use solana_system_transaction as system_transaction;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_SOLANA_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(
        long,
        help = "Path to keypair JSON file",
        default_value = "/data/shreds-subscribe/id.json"
    )]
    keypair_path: String,

    #[arg(
        long,
        help = "RPC URL of shred-subscribe server",
        default_value = "http://localhost:12345"
    )]
    rpc_url: String,

    #[arg(
        long,
        help = "Amount of SOL to transfer per test",
        default_value = "0.001"
    )]
    amount: f64,

    #[arg(
        long,
        help = "Number of test transactions to send",
        default_value = "1"
    )]
    test_amount: usize,

    #[arg(
        long,
        help = "Delay between tests in milliseconds",
        default_value = "500"
    )]
    test_delay_ms: u64,
}

async fn call_subscribe_tx(
    client: &Client,
    rpc_url: &str,
    tx_sig: &str,
    timestamp_ms: u64,
    transaction_base64: &str,
) -> Result<()> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "subscribe_tx",
        "params": [{
            "tx_sig": tx_sig,
            "timestamp": timestamp_ms,
            "transaction": transaction_base64,
        }]
    });

    let response = client
        .post(rpc_url)
        .json(&request)
        .send()
        .await
        .context("Failed to send RPC request")?;

    if !response.status().is_success() {
        anyhow::bail!("RPC request failed with status: {}", response.status());
    }

    let response_json: serde_json::Value = response
        .json()
        .await
        .context("Failed to parse RPC response body")?;

    if let Some(error) = response_json.get("error") {
        anyhow::bail!("RPC error: {error:?}");
    }

    Ok(())
}

fn format_timestamp(timestamp_ms: u64) -> String {
    let secs = (timestamp_ms / 1_000) as i64;
    let nanos = ((timestamp_ms % 1_000) * 1_000_000) as u32;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.3f UTC").to_string())
        .unwrap_or_else(|| timestamp_ms.to_string())
}

fn current_timestamp_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|err| anyhow!("System clock error: {}", err))
}

fn load_keypair(path: &str) -> Result<Keypair> {
    read_keypair_file(path).map_err(|err| anyhow!("Failed to read keypair file {path}: {err}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.test_amount == 0 {
        anyhow::bail!("--test-amount must be at least 1");
    }

    println!("Loading keypair from {}...", args.keypair_path);
    let keypair = load_keypair(&args.keypair_path)?;
    println!("Wallet: {}\n", keypair.pubkey());

    let lamports = (args.amount * LAMPORTS_PER_SOL as f64) as u64;
    if lamports == 0 {
        anyhow::bail!("Transfer amount is too small; results in 0 lamports");
    }

    let solana_client = RpcClient::new(DEFAULT_SOLANA_RPC_URL.to_string());
    let http_client = Client::new();

    for test_num in 1..=args.test_amount {
        if args.test_amount > 1 {
            println!("{}", "=".repeat(60));
            println!("Test {}/{}", test_num, args.test_amount);
            println!("{}", "=".repeat(60));
        }

        let blockhash = solana_client
            .get_latest_blockhash()
            .context("Failed to get latest blockhash")?;

        let transaction =
            system_transaction::transfer(&keypair, &keypair.pubkey(), lamports, blockhash);
        let signature = transaction.signatures[0].to_string();

        println!("\nTransaction signature: {}", signature);
        println!("Amount: {} SOL", args.amount);

        let transaction_bytes =
            bincode::serialize(&transaction).context("Failed to serialize transaction")?;
        let transaction_base64 = BASE64.encode(transaction_bytes);

        let timestamp_ms = current_timestamp_ms()?;

        println!("\n🚀 Calling subscribe_tx on {}...", args.rpc_url);
        call_subscribe_tx(
            &http_client,
            &args.rpc_url,
            &signature,
            timestamp_ms,
            &transaction_base64,
        )
        .await?;

        println!("✓ RPC request sent at {}", format_timestamp(timestamp_ms));
        println!("\nCheck server logs for latency measurements.\n");

        if test_num < args.test_amount {
            tokio::time::sleep(Duration::from_millis(args.test_delay_ms)).await;
        }
    }

    if args.test_amount > 1 {
        println!("{}", "=".repeat(60));
        println!("All {} tests completed", args.test_amount);
        println!("Check server logs for detailed latency analysis");
        println!("{}", "=".repeat(60));
    }

    Ok(())
}
