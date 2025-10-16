use dashmap::DashMap;
use jsonrpc_core::{Error, ErrorCode, Params, Result};
use jsonrpc_derive::rpc;
use jsonrpc_http_server::{Server, ServerBuilder};
use serde::{Deserialize, Serialize};
use solana_sdk::signature::Signature;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tokio_tungstenite::tungstenite::Message;
use tracing::info;

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

#[rpc(server)]
pub trait Rpc {
    #[rpc(name = "subscribe_tx")]
    fn subscribe_tx(&self, params: Params) -> Result<SubscribeResponse>; // Returns immediately with status
}

pub struct RpcImpl {
    pub subscriptions: Arc<DashMap<String, Subscription>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SubscribeParamsByName {
    tx_sig: String,
    // Only accept `timestamp` as milliseconds since UNIX epoch
    timestamp: u64,
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

        let tx_sig = parsed.tx_sig;
        let timestamp_ms = parsed.timestamp;

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

        // Return immediately with success status
        Ok(SubscribeResponse {
            status: "subscribed".to_string(),
        })
    }
}

pub async fn start_rpc_server(
    addr: SocketAddr,
    subscriptions: Arc<DashMap<String, Subscription>>,
) -> std::result::Result<Server, jsonrpc_core::Error> {
    let rpc_impl = RpcImpl { subscriptions };

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
