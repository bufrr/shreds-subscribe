use anyhow::{Context, Result as AnyhowResult, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use jsonrpc_core::{Error, ErrorCode, Params, Result};
use jsonrpc_derive::rpc;
use jsonrpc_http_server::{Server, ServerBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_client::rpc_client::RpcClient;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self, Sender};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

#[derive(Debug)]
pub struct Subscription {
    pub client_timestamp_ms: u64, // Client-provided timestamp for latency tracking
    pub rpc_received_ms: u64,     // Server timestamp when RPC was received
    pub websocket: Option<WebsocketSubscription>,
}

#[derive(Debug)]
pub struct WebsocketSubscription {
    pub id: u64,
    pub sender: Sender<Message>,
}

// New struct for the immediate success response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeResponse {
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct WebSocketEndpointsConfig {
    pub shred_ws_url: Option<String>,
    pub local_ws_url: Option<String>,
    pub local_enhanced_ws_url: Option<String>,
    pub helius_ws_url: Option<String>,
    pub solana_rpc_url: String,
}

#[rpc(server)]
pub trait Rpc {
    #[rpc(name = "subscribe_tx")]
    fn subscribe_tx(&self, params: Params) -> Result<SubscribeResponse>; // Returns immediately with status
}

pub struct RpcImpl {
    pub subscriptions: Arc<DashMap<String, Subscription>>,
    pub ws_config: Option<WebSocketEndpointsConfig>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SubscribeParamsByName {
    tx_sig: String,
    // Only accept `timestamp` as milliseconds since UNIX epoch
    timestamp: u64,
    transaction: Option<String>,
}

impl RpcImpl {
    fn spawn_websocket_followups(
        &self,
        tx_sig: &str,
        client_timestamp_ms: u64,
        transaction: Option<String>,
    ) {
        if let Some(config) = self.ws_config.clone() {
            spawn_ws_followups(config, tx_sig.to_string(), client_timestamp_ms, transaction);
        } else if transaction.is_some() {
            warn!("Transaction provided but no websocket configuration available; skipping send");
        }
    }
}

impl Rpc for RpcImpl {
    fn subscribe_tx(&self, params: Params) -> Result<SubscribeResponse> {
        // Over HTTP, jsonrpc-derive unwraps the outer single-element array and
        // passes the inner object as Params::Map. This preserves the on-wire
        // requirement for array params while giving us a Map here.
        let parsed: SubscribeParamsByName = match params {
            Params::Map(map) => {
                serde_json::from_value(serde_json::Value::Object(map)).map_err(|_| Error {
                    code: ErrorCode::InvalidParams,
                    message: "Expected object with tx_sig and timestamp fields".to_string(),
                    data: None,
                })?
            }
            _ => {
                return Err(Error {
                    code: ErrorCode::InvalidParams,
                    message: "Expected params array: [{tx_sig, timestamp}]".to_string(),
                    data: None,
                });
            }
        };

        let SubscribeParamsByName {
            tx_sig,
            timestamp,
            transaction,
        } = parsed;
        let timestamp_ms = timestamp;

        // Validate timestamp precision (must be milliseconds since UNIX epoch)
        if timestamp_ms < 1_000_000_000_000 {
            return Err(Error {
                code: ErrorCode::InvalidParams,
                message: "`timestamp` must be milliseconds since UNIX epoch (ms), not seconds"
                    .to_string(),
                data: None,
            });
        }

        // Validate transaction signature format
        if Signature::from_str(&tx_sig).is_err() {
            return Err(Error {
                code: ErrorCode::InvalidParams,
                message: "Invalid transaction signature".to_string(),
                data: None,
            });
        }

        // Capture server receive time (in ms) for RPC arrival
        let rpc_received_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // Create the subscription with client + server timestamps
        let subscription = Subscription {
            client_timestamp_ms: timestamp_ms,
            rpc_received_ms,
            websocket: None,
        };

        // Try to insert the subscription
        use dashmap::mapref::entry::Entry;
        match self.subscriptions.entry(tx_sig.clone()) {
            Entry::Occupied(_) => {
                return Err(Error {
                    code: ErrorCode::InvalidRequest,
                    message: "Transaction already subscribed".to_string(),
                    data: None,
                });
            }
            Entry::Vacant(entry) => {
                entry.insert(subscription);
                info!(
                    "Subscribed to tx_sig: {} with client timestamp: {} and rpc_received_ms: {}",
                    tx_sig, timestamp_ms, rpc_received_ms
                );
            }
        }

        self.spawn_websocket_followups(&tx_sig, timestamp_ms, transaction);

        // Return immediately with success status
        Ok(SubscribeResponse {
            status: "subscribed".to_string(),
        })
    }
}

fn spawn_ws_followups(
    config: WebSocketEndpointsConfig,
    tx_sig: String,
    client_timestamp_ms: u64,
    transaction_base64: Option<String>,
) {
    tokio::spawn(async move {
        let WebSocketEndpointsConfig {
            shred_ws_url,
            local_ws_url,
            local_enhanced_ws_url,
            helius_ws_url,
            solana_rpc_url,
        } = config;

        let (ready_tx, mut ready_rx) = mpsc::channel::<String>(4);
        let mut subscription_count = 0usize;

        if let Some(url) = shred_ws_url {
            subscription_count += 1;
            let signature = tx_sig.clone();
            let ready = ready_tx.clone();
            tokio::spawn(async move {
                if let Err(err) = run_signature_subscription(
                    url,
                    signature,
                    "Shred WS",
                    client_timestamp_ms,
                    Some(ready),
                )
                .await
                {
                    warn!("Shred WS subscription task failed: {}", err);
                }
            });
        }

        if let Some(url) = local_ws_url {
            subscription_count += 1;
            let signature = tx_sig.clone();
            let ready = ready_tx.clone();
            tokio::spawn(async move {
                if let Err(err) = run_signature_subscription(
                    url,
                    signature,
                    "Local WS",
                    client_timestamp_ms,
                    Some(ready),
                )
                .await
                {
                    warn!("Local WS subscription task failed: {}", err);
                }
            });
        }

        if let Some(url) = local_enhanced_ws_url {
            subscription_count += 1;
            let signature = tx_sig.clone();
            let ready = ready_tx.clone();
            tokio::spawn(async move {
                if let Err(err) = run_signature_subscription(
                    url,
                    signature,
                    "Local Enhanced WS",
                    client_timestamp_ms,
                    Some(ready),
                )
                .await
                {
                    warn!("Local Enhanced WS subscription task failed: {}", err);
                }
            });
        }

        if let Some(url) = helius_ws_url {
            subscription_count += 1;
            let signature = tx_sig.clone();
            let ready = ready_tx.clone();
            tokio::spawn(async move {
                if let Err(err) = run_signature_subscription(
                    url,
                    signature,
                    "Helius WS",
                    client_timestamp_ms,
                    Some(ready),
                )
                .await
                {
                    warn!("Helius WS subscription task failed: {}", err);
                }
            });
        }

        drop(ready_tx);

        if subscription_count > 0 {
            let mut ready_count = 0usize;
            let timeout = tokio::time::sleep(Duration::from_secs(2));
            tokio::pin!(timeout);

            while ready_count < subscription_count {
                tokio::select! {
                    _ = &mut timeout => {
                        warn!(
                            "Subscription readiness timeout: {}/{} ready before sending transaction",
                            ready_count, subscription_count
                        );
                        break;
                    }
                    maybe_source = ready_rx.recv() => {
                        match maybe_source {
                            Some(source) => {
                                ready_count += 1;
                                info!("✓ {} subscription confirmed", source);
                                if ready_count >= subscription_count {
                                    info!("All {} subscriptions confirmed", subscription_count);
                                    break;
                                }
                            }
                            None => {
                                warn!(
                                    "Subscription readiness channel closed early: {}/{} ready",
                                    ready_count, subscription_count
                                );
                                break;
                            }
                        }
                    }
                }
            }

            if ready_count < subscription_count {
                warn!(
                    "Proceeding with transaction send after partial readiness: {}/{} ready",
                    ready_count, subscription_count
                );
            }
        }

        match transaction_base64 {
            Some(tx_base64) => {
                if solana_rpc_url.trim().is_empty() {
                    warn!("Solana RPC URL missing; unable to send transaction");
                } else {
                    match send_transaction_to_solana(&solana_rpc_url, &tx_base64) {
                        Ok(_) => {
                            info!("✓ Transaction sent to Solana at {} µs", current_micros());
                        }
                        Err(err) => {
                            warn!("⚠ Failed to send transaction to Solana: {}", err);
                        }
                    }
                }
            }
            None => {
                warn!("Transaction payload missing; skipping send to Solana RPC");
            }
        }
    });
}

fn send_transaction_to_solana(rpc_url: &str, transaction_base64: &str) -> AnyhowResult<()> {
    let transaction_bytes = BASE64
        .decode(transaction_base64.trim())
        .context("Failed to decode transaction from base64")?;

    let transaction: Transaction =
        bincode::deserialize(&transaction_bytes).context("Failed to deserialize transaction")?;

    let client = RpcClient::new(rpc_url.to_string());
    client
        .send_transaction(&transaction)
        .context("Failed to send transaction to Solana")?;

    Ok(())
}

async fn run_signature_subscription(
    url: String,
    signature: String,
    source_name: &'static str,
    client_timestamp_ms: u64,
    ready_notifier: Option<Sender<String>>,
) -> AnyhowResult<()> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "signatureSubscribe",
        "params": [signature, {"commitment": "processed"}]
    });

    run_ws_task(
        url,
        request,
        source_name,
        "signatureNotification",
        ready_notifier,
        None,
        client_timestamp_ms,
    )
    .await
}

async fn run_ws_task(
    url: String,
    request: Value,
    source_name: &'static str,
    expected_method: &'static str,
    ready_notifier: Option<Sender<String>>,
    signature_filter: Option<String>,
    client_timestamp_ms: u64,
) -> AnyhowResult<()> {
    let (mut ws, _) = connect_async(&url)
        .await
        .with_context(|| format!("Failed to connect to {}", source_name))?;

    ws.send(Message::Text(request.to_string().into()))
        .await
        .with_context(|| format!("Failed to send subscription request to {}", source_name))?;

    let mut received_notification = false;
    let mut ready_notifier = ready_notifier;
    let mut subscription_confirmed = ready_notifier.is_none();

    while let Some(message) = ws.next().await {
        match message {
            Ok(Message::Text(text)) => {
                let value = match serde_json::from_str::<Value>(&text) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(
                            "{} returned non-JSON text: {}; err: {}",
                            source_name, text, err
                        );
                        continue;
                    }
                };

                if let Some(error) = value.get("error") {
                    return Err(anyhow!(
                        "{} subscription error response: {}",
                        source_name,
                        error
                    ));
                }

                if !subscription_confirmed && value.get("result").is_some() {
                    if let Some(notifier) = ready_notifier.take() {
                        if notifier.send(source_name.to_string()).await.is_err() {
                            warn!(
                                "{} subscription ready signal dropped before being received",
                                source_name
                            );
                        }
                    }
                    subscription_confirmed = true;
                    continue;
                }

                if value.get("method").and_then(|method| method.as_str()) != Some(expected_method) {
                    continue;
                }

                if let Some(expected) = signature_filter.as_deref() {
                    let maybe_sig = extract_notification_signature(&value);
                    if maybe_sig != Some(expected) {
                        if let Some(mut sig) = maybe_sig.map(str::to_string) {
                            const MAX_PREVIEW: usize = 8;
                            if sig.len() > MAX_PREVIEW {
                                sig.truncate(MAX_PREVIEW);
                            }
                            warn!(
                                "{} ignored {} for signature {} (expected {})",
                                source_name, expected_method, sig, expected
                            );
                        } else {
                            warn!(
                                "{} ignored {} missing signature field",
                                source_name, expected_method
                            );
                        }
                        continue;
                    }
                }

                let arrival_micros = current_micros();
                let client_micros = (client_timestamp_ms as u128).saturating_mul(1_000);
                let latency_ms =
                    (arrival_micros.saturating_sub(client_micros) as f64) / 1_000.0_f64;

                info!(
                    "{} notification received at {}us (latency: {:.3} ms)",
                    source_name, arrival_micros, latency_ms
                );

                received_notification = true;
                break;
            }
            Ok(Message::Ping(payload)) => {
                ws.send(Message::Pong(payload))
                    .await
                    .with_context(|| format!("Failed to reply to ping from {}", source_name))?;
            }
            Ok(Message::Close(_)) => {
                break;
            }
            Ok(Message::Binary(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Err(err) => {
                return Err(err.into());
            }
        }
    }

    if received_notification {
        Ok(())
    } else {
        Err(anyhow!(
            "{} connection closed before notification was received",
            source_name
        ))
    }
}

fn extract_notification_signature(value: &Value) -> Option<&str> {
    value
        .get("params")
        .and_then(|params| params.get("result"))
        .and_then(|result| result.get("signature"))
        .and_then(|sig| sig.as_str())
        .or_else(|| {
            value
                .get("params")
                .and_then(|params| params.get("result"))
                .and_then(|result| result.get("value"))
                .and_then(|value| value.get("transaction"))
                .and_then(|transaction| transaction.get("transaction"))
                .and_then(|transaction| transaction.get("signatures"))
                .and_then(|sigs| sigs.get(0))
                .and_then(|sig| sig.as_str())
        })
}

fn current_micros() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or_default()
}

pub async fn start_rpc_server_with_ws_config(
    addr: SocketAddr,
    subscriptions: Arc<DashMap<String, Subscription>>,
    ws_config: Option<WebSocketEndpointsConfig>,
) -> std::result::Result<Server, jsonrpc_core::Error> {
    let rpc_impl = RpcImpl {
        subscriptions,
        ws_config,
    };

    let mut io = jsonrpc_core::IoHandler::new();
    io.extend_with(rpc_impl.to_delegate());

    let server = ServerBuilder::new(io)
        .start_http(&addr)
        .map_err(|e| Error {
            code: ErrorCode::InternalError,
            message: format!("Failed to start RPC server: {}", e),
            data: None,
        })?;

    info!("RPC server started on {}", addr);
    Ok(server)
}

// (unit test removed per request)
