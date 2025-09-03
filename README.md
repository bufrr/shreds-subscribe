# Solana Shreds Subscribe

Monitor Solana’s shred UDP stream, reconstruct entries (with FEC recovery), deshred, and detect specific transactions by signature. Exposes a simple JSON‑RPC endpoint to “subscribe” to a transaction signature and logs when that transaction is first observed in the shred stream, including a latency computed from a client‑provided timestamp.

This repo is intended to help measure the interval between “client submitted” (you provide the timestamp) and “transaction packed and propagated in shreds (observed here)”.

## Architecture

- `src/main.rs`: Entry point. Starts shred receiver and JSON‑RPC server; coordinates shutdown.
- `src/shred.rs`: UDP shred listener using `solana-streamer`; forwards `PacketBatch` for reconstruction.
- `src/deshred.rs`: Reconstructs/re‑covers FEC sets, deshreds entries, scans transactions, logs matches with latency.
- `src/rpc.rs`: JSON‑RPC 2.0 method `subscribe_tx({ tx_sig, timestamp })`; validates and tracks subscriptions.

## Ports

- UDP shreds: `18999` (default in code)
- RPC HTTP: `28899` (default in code)

Notes:
- Docs and scripts use `28899` by default; adjust `rpc_port` in `src/main.rs` if you prefer a different port.

## Requirements

- Rust 1.80+ (stable)
- A Solana node (or routing of its shred UDP stream) to this host
- System deps (examples):
  - Ubuntu: `sudo apt install -y build-essential clang llvm-dev libclang-dev pkg-config libssl-dev`
  - CentOS: `sudo dnf groupinstall -y "Development Tools" && sudo dnf install -y clang llvm-devel libclang-devel pkgconfig openssl-devel`

## Build

Important: `Cargo.toml` references a local `solana-ledger` path. Update it to your local `jito-solana/ledger` checkout before building.

```bash
cargo build --release
```

## Run

```bash
cargo run --release
```

You should see logs indicating the UDP listener and RPC server ports.

## RPC API

### Base URL

```
http://localhost:28899
```

Change the port if you modify `rpc_port` in `src/main.rs`.

### JSON‑RPC Version

JSON‑RPC 2.0

### Method: `subscribe_tx`

Subscribe to be notified (via server log) when a specific transaction is observed in deshredded entries.

Parameters (named object only — timestamp must be milliseconds [ms]):
```json
"params": {"tx_sig": "<transaction_signature>", "timestamp": 1701234567890}
```

Return value:

```json
{
  "status": "subscribed"
}
```

Errors:

- Invalid transaction signature
  - code: `-32602`, message: `Invalid transaction signature`
- Transaction already subscribed
  - code: `-32600`, message: `Transaction already subscribed`

Behavior:

- Fire‑and‑forget: returns immediately after registering the subscription
- One‑time per `tx_sig`
- When found, the server logs: signature, slot, total latency, client timestamp, and server discovery time
 - The `timestamp` field must be in milliseconds (ms) since UNIX epoch; passing seconds will be rejected.

### Examples

curl:

```bash
TIMESTAMP=$(date +%s%3N)
curl -X POST http://localhost:28899 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "method": "subscribe_tx",
    "params": {"tx_sig": "5WzvRtu7qxWbQBhbGKdqrpfuiLaZpKoSEQUZjhnYQ5XB2wGdZs9hPVLCLHKdqrpXJ9VhLLe9juMpGZYvgFnVUqeY", "timestamp": '"$TIMESTAMP"'},
    "id": 1
  }'
```

JavaScript:

```javascript
async function subscribeToTransaction(txHash, rpcUrl = 'http://localhost:28899') {
  const timestamp = Date.now();
  const response = await fetch(rpcUrl, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      method: 'subscribe_tx',
      params: { tx_sig: txHash, timestamp },
      id: 1,
    }),
  });
  const result = await response.json();
  if (result.error) throw new Error(result.error.message);
  return result.result;
}
```

Python:

```python
import requests, time

def subscribe_to_transaction(tx_hash, rpc_url='http://localhost:28899'):
    ts = int(time.time() * 1000)
    payload = {"jsonrpc":"2.0","method":"subscribe_tx","params":[tx_hash, ts],"id":1}
    r = requests.post(rpc_url, json=payload)
    j = r.json()
    if 'error' in j: raise Exception(j['error']['message'])
    return j['result']
```

## Latency Measurement

When a subscribed transaction is first observed in shreds, the service logs:

- Transaction signature and slot
- Total latency: `found_ms - timestamp`
- RPC latency: `rpc_received_ms - timestamp`
- After-RPC latency: `found_ms - rpc_received_ms`
- Raw timestamps: `timestamp` (from request), `rpc_received_ms`, `found_ms`

Interpretation:

- RPC latency approximates client→RPC network time (and minor server overhead).
- After-RPC latency approximates submission→leader inclusion→shred propagation→arrival to this host.
- Total latency = RPC latency + After-RPC latency.
- This does not capture TPU ingress timing precisely or consensus confirmation; it measures time to first shred observation.

## Test & Dev

- Smoke test script: `./test_rpc.sh` (uses port `28899`). Adjust the script or change `rpc_port` in `src/main.rs` to match.
- Build: `cargo build --release`
- Run: `cargo run --release`
- Format: `cargo fmt --all`
- Lint: `cargo clippy -- -D warnings`
- Tests: `cargo test`

## Security & Ops

- Avoid exposing the RPC publicly; restrict access by firewall or security group.
- Monitor CPU and network usage; shred streams can be high‑volume.
- Consider log rotation if redirecting logs to a file.

## Notes

- Ensure `Cargo.toml`’s `solana-ledger` path points to your local `jito-solana/ledger` checkout.
- UDP shreds come from your Solana node; configure your network to forward or mirror them to this host and port (`18999` by default).
