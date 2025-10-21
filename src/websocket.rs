use crate::rpc::{Subscription, WebsocketSubscription};
use anyhow::Context;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use solana_sdk::signature::Signature;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{debug, info, warn};

const JSONRPC_VERSION: &str = "2.0";

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Deserialize)]
struct SignatureSubscribeConfig {
    commitment: Option<String>,
}

pub async fn start_websocket_server(
    addr: SocketAddr,
    subscriptions: Arc<DashMap<String, Subscription>>,
    mut shutdown: broadcast::Receiver<()>,
    max_connections: usize,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind websocket listener on {addr}"))?;

    info!("WebSocket server listening on {}", addr);

    let next_id = Arc::new(AtomicU64::new(1));
    let active_connections = Arc::new(AtomicUsize::new(0));

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, peer_addr)) => {
                        let current = active_connections.load(Ordering::Relaxed);
                        if current >= max_connections {
                            warn!("Connection limit ({}) reached, rejecting {}", max_connections, peer_addr);
                            drop(stream);
                            continue;
                        }

                        active_connections.fetch_add(1, Ordering::Relaxed);
                        let subs = subscriptions.clone();
                        let id_gen = next_id.clone();
                        let counter = active_connections.clone();
                        tokio::spawn(async move {
                            let _guard = ConnectionGuard(counter);
                            if let Err(e) = handle_connection(stream, peer_addr, subs, id_gen).await {
                                warn!("WebSocket connection {peer_addr} closed with error: {e:?}");
                            }
                        });
                    }
                    Err(e) => {
                        warn!("WebSocket accept error: {e}");
                    }
                }
            }
            _ = shutdown.recv() => {
                info!("Shutting down WebSocket server");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    subscriptions: Arc<DashMap<String, Subscription>>,
    next_id: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let ws_stream = accept_async(stream)
        .await
        .with_context(|| format!("failed websocket handshake with {peer_addr}"))?;

    info!("WebSocket connection established from {}", peer_addr);

    let (mut sink, mut source) = ws_stream.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(1024);

    let writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            if let Err(e) = sink.send(message).await {
                debug!("WebSocket writer send error: {e}");
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut active_subscriptions: HashMap<u64, String> = HashMap::new();

    while let Some(message) = source.next().await {
        match message {
            Ok(Message::Text(payload)) => {
                handle_json_message(
                    &payload,
                    &subscriptions,
                    &outbound_tx,
                    &mut active_subscriptions,
                    &next_id,
                );
            }
            Ok(Message::Binary(_)) => {
                send_error(
                    &outbound_tx,
                    Value::Null,
                    jsonrpc_core::ErrorCode::InvalidRequest.code(),
                    "Binary messages are not supported",
                );
            }
            Ok(Message::Ping(payload)) => {
                if outbound_tx.try_send(Message::Pong(payload)).is_err() {
                    debug!("Client channel full, dropping pong message");
                }
            }
            Ok(Message::Pong(_)) => {
                // No action required; keep-alive acknowledgement
            }
            Ok(Message::Close(frame)) => {
                debug!("WebSocket client {peer_addr} sent close frame: {:?}", frame);
                break;
            }
            Ok(other) => {
                debug!(
                    "WebSocket client {peer_addr} sent unsupported frame: {:?}",
                    other
                );
            }
            Err(e) => {
                debug!("WebSocket read error from {peer_addr}: {e}");
                break;
            }
        }
    }

    cleanup_connection(&subscriptions, active_subscriptions);

    drop(outbound_tx);
    let _ = writer.await;
    info!("WebSocket connection closed for {}", peer_addr);
    Ok(())
}

fn handle_json_message(
    payload: &str,
    subscriptions: &Arc<DashMap<String, Subscription>>,
    outbound_tx: &mpsc::Sender<Message>,
    active_subscriptions: &mut HashMap<u64, String>,
    next_id: &Arc<AtomicU64>,
) {
    let parsed: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => {
            send_error(
                outbound_tx,
                Value::Null,
                jsonrpc_core::ErrorCode::ParseError.code(),
                "Invalid JSON",
            );
            return;
        }
    };

    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    if parsed
        .get("jsonrpc")
        .and_then(Value::as_str)
        .map(|v| v == JSONRPC_VERSION)
        != Some(true)
    {
        send_error(
            outbound_tx,
            id,
            jsonrpc_core::ErrorCode::InvalidRequest.code(),
            "jsonrpc must be \"2.0\"",
        );
        return;
    }

    let method = match parsed.get("method").and_then(Value::as_str) {
        Some(m) => m,
        None => {
            send_error(
                outbound_tx,
                id,
                jsonrpc_core::ErrorCode::InvalidRequest.code(),
                "Missing method",
            );
            return;
        }
    };

    let params = parsed.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "signatureSubscribe" => {
            handle_signature_subscribe(
                id,
                params,
                subscriptions,
                outbound_tx,
                active_subscriptions,
                next_id,
            );
        }
        "signatureUnsubscribe" => {
            handle_signature_unsubscribe(
                id,
                params,
                subscriptions,
                outbound_tx,
                active_subscriptions,
            );
        }
        _ => {
            send_error(
                outbound_tx,
                id,
                jsonrpc_core::ErrorCode::MethodNotFound.code(),
                "Method not found",
            );
        }
    }
}

fn handle_signature_subscribe(
    id: Value,
    params: Value,
    subscriptions: &Arc<DashMap<String, Subscription>>,
    outbound_tx: &mpsc::Sender<Message>,
    active_subscriptions: &mut HashMap<u64, String>,
    next_id: &Arc<AtomicU64>,
) {
    let params_arr = match params {
        Value::Array(arr) => arr,
        Value::Null => Vec::new(),
        _ => {
            send_error(
                outbound_tx,
                id,
                jsonrpc_core::ErrorCode::InvalidParams.code(),
                "Params must be an array",
            );
            return;
        }
    };

    if params_arr.is_empty() {
        send_error(
            outbound_tx,
            id,
            jsonrpc_core::ErrorCode::InvalidParams.code(),
            "signatureSubscribe requires signature parameter",
        );
        return;
    }

    if params_arr.len() > 2 {
        send_error(
            outbound_tx,
            id,
            jsonrpc_core::ErrorCode::InvalidParams.code(),
            "Too many parameters",
        );
        return;
    }

    let signature_str = match params_arr.get(0).and_then(Value::as_str) {
        Some(s) => s,
        None => {
            send_error(
                outbound_tx,
                id,
                jsonrpc_core::ErrorCode::InvalidParams.code(),
                "Signature parameter must be a string",
            );
            return;
        }
    };

    if let Err(_) = Signature::from_str(signature_str) {
        send_error(
            outbound_tx,
            id,
            jsonrpc_core::ErrorCode::InvalidParams.code(),
            "Invalid transaction signature",
        );
        return;
    }

    if let Some(config_value) = params_arr.get(1) {
        if !config_value.is_null() {
            match serde_json::from_value::<SignatureSubscribeConfig>(config_value.clone()) {
                Ok(config) => {
                    if let Some(commitment) = config.commitment {
                        match commitment.as_str() {
                            "processed" | "confirmed" | "finalized" => {}
                            _ => {
                                send_error(
                                    outbound_tx,
                                    id,
                                    jsonrpc_core::ErrorCode::InvalidParams.code(),
                                    "Unsupported commitment level",
                                );
                                return;
                            }
                        }
                    }
                }
                Err(_) => {
                    send_error(
                        outbound_tx,
                        id,
                        jsonrpc_core::ErrorCode::InvalidParams.code(),
                        "Invalid commitment config",
                    );
                    return;
                }
            }
        }
    }

    use dashmap::mapref::entry::Entry;
    match subscriptions.entry(signature_str.to_string()) {
        Entry::Occupied(_) => {
            send_error(
                outbound_tx,
                id,
                jsonrpc_core::ErrorCode::InvalidRequest.code(),
                "Transaction already subscribed",
            );
        }
        Entry::Vacant(entry) => {
            let subscription_id = next_id.fetch_add(1, Ordering::Relaxed);
            let now_ms = current_timestamp_ms();
            let websocket = WebsocketSubscription {
                id: subscription_id,
                sender: outbound_tx.clone(),
            };
            let subscription = Subscription {
                client_timestamp_ms: now_ms,
                rpc_received_ms: now_ms,
                websocket: Some(websocket),
                summary_sender: None,
            };

            entry.insert(subscription);
            active_subscriptions.insert(subscription_id, signature_str.to_string());

            send_result(outbound_tx, id, json!(subscription_id));
        }
    }
}

fn handle_signature_unsubscribe(
    id: Value,
    params: Value,
    subscriptions: &Arc<DashMap<String, Subscription>>,
    outbound_tx: &mpsc::Sender<Message>,
    active_subscriptions: &mut HashMap<u64, String>,
) {
    let params_arr = match params {
        Value::Array(arr) => arr,
        _ => {
            send_error(
                outbound_tx,
                id,
                jsonrpc_core::ErrorCode::InvalidParams.code(),
                "Params must be an array",
            );
            return;
        }
    };

    if params_arr.len() != 1 {
        send_error(
            outbound_tx,
            id,
            jsonrpc_core::ErrorCode::InvalidParams.code(),
            "signatureUnsubscribe requires the subscription id",
        );
        return;
    }

    let Some(subscription_id) = params_arr.get(0).and_then(Value::as_u64) else {
        send_error(
            outbound_tx,
            id,
            jsonrpc_core::ErrorCode::InvalidParams.code(),
            "Subscription id must be an unsigned integer",
        );
        return;
    };

    let existed = match active_subscriptions.remove(&subscription_id) {
        Some(signature) => {
            subscriptions.remove_if(&signature, |_, subscription| {
                subscription
                    .websocket
                    .as_ref()
                    .map(|ws| ws.id == subscription_id)
                    .unwrap_or(false)
            });
            true
        }
        None => false,
    };

    send_result(outbound_tx, id, json!(existed));
}

fn cleanup_connection(
    subscriptions: &Arc<DashMap<String, Subscription>>,
    active_subscriptions: HashMap<u64, String>,
) {
    for (subscription_id, signature) in active_subscriptions {
        subscriptions.remove_if(&signature, |_, subscription| {
            subscription
                .websocket
                .as_ref()
                .map(|ws| ws.id == subscription_id)
                .unwrap_or(false)
        });
    }
}

fn send_result(outbound: &mpsc::Sender<Message>, id: Value, result: Value) {
    let response = json!({
        "jsonrpc": JSONRPC_VERSION,
        "result": result,
        "id": id,
    });
    if let Ok(text) = serde_json::to_string(&response) {
        if outbound.try_send(Message::Text(text.into())).is_err() {
            debug!("Client channel full, dropping result response");
        }
    }
}

fn send_error(outbound: &mpsc::Sender<Message>, id: Value, code: i64, message: &str) {
    let response = json!({
        "jsonrpc": JSONRPC_VERSION,
        "error": {
            "code": code,
            "message": message,
        },
        "id": id,
    });
    if let Ok(text) = serde_json::to_string(&response) {
        if outbound.try_send(Message::Text(text.into())).is_err() {
            debug!("Client channel full, dropping error response");
        }
    }
}

fn current_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
