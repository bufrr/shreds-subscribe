# Solana Shreds Subscribe

Monitor Solana’s shred UDP stream, reconstruct entries (with FEC recovery), deshred, and detect specific transactions by signature. Exposes a simple JSON‑RPC endpoint to “subscribe” to a transaction signature and logs when that transaction is first observed in the shred stream, including a latency computed from a client‑provided timestamp.

This repo is intended to help measure the interval between “client submitted” (you provide the timestamp) and “transaction packed and propagated in shreds (observed here)”.

## Architecture

- `src/main.rs`: Entry point. Starts shred receiver and JSON‑RPC server; coordinates shutdown.
- `src/shred.rs`: UDP shred listener using `solana-streamer`; forwards `PacketBatch` for reconstruction.
- `src/deshred.rs`: Reconstructs/re‑covers FEC sets, deshreds entries, scans transactions, logs matches with latency.
- `src/rpc.rs`: JSON‑RPC 2.0 method `subscribe_tx({ tx_sig, timestamp })`; validates and tracks subscriptions.

## Ports

- UDP shreds: `18888` (default in code)
- RPC HTTP: `12345` (default in code)

Notes:
- Docs and scripts use `12345` by default; change ports via CLI flags or environment variables.

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
http://localhost:12345
```

Change the port via CLI flags or environment variables.

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
curl -X POST http://localhost:12345 \
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
async function subscribeToTransaction(txHash, rpcUrl = 'http://localhost:12345') {
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

def subscribe_to_transaction(tx_hash, rpc_url='http://localhost:12345'):
    ts = int(time.time() * 1000)
    payload = {"jsonrpc":"2.0","method":"subscribe_tx","params":{"tx_sig": tx_hash, "timestamp": ts},"id":1}
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

- Smoke test script: `./test_rpc.sh` (uses port `12345`). Adjust the script or change `rpc_port` in `src/main.rs` to match.
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
- UDP shreds come from your Solana node; configure your network to forward or mirror them to this host and port (`18888` by default).

## CLI Flags and Environment

The binary reads configuration from flags or environment variables:

- `--udp-port` or `UDP_PORT` (default `18888`)
- `--rpc-port` or `RPC_PORT` (default `12345`)
- `--trace-log-path` or `TRACE_LOG_PATH` (optional file path)
- `--config` or `CONFIG_PATH` (path to a `config.toml` file)

Examples:

```bash
# Highest priority: CLI flags
shreds-subscribe --udp-port 18888 --rpc-port 12345 --trace-log-path server.log

# Or via exported env vars (not .env)
export UDP_PORT=18888 RPC_PORT=12345 TRACE_LOG_PATH=server.log
shreds-subscribe

# Or via config file (TOML). Precedence: CLI/env > config file > defaults
cp config.example.toml config.toml
sed -i 's/^# udp_port.*/udp_port = 18888/' config.toml
sed -i 's/^# rpc_port.*/rpc_port = 12345/' config.toml
shreds-subscribe --config config.toml
```

### Configuration File (TOML)

You can provide a `config.toml` (auto-detected in CWD) or pass `--config /path/to/config.toml`.

Keys:
- `udp_port` (u16)
- `rpc_port` (u16)
- `trace_log_path` (string)

Example: see `config.example.toml`.

## Docker

This project references a local `solana-ledger` path in `Cargo.toml`. Docker builds run in a clean container, so you must make that path available inside the build context or switch to a git dependency. Options:

- Vendor the ledger into the repo and use a relative path:
  - Place your checkout at `vendor/jito-solana/ledger`.
  - Update `Cargo.toml` to `solana-ledger = { path = "vendor/jito-solana/ledger" }`.
  - Ensure `vendor/` is included in the Docker build context and copied in the Dockerfile.
- Or switch to a pinned git dependency specifically for Docker builds.

Using docker-compose (provided):

```bash
docker compose up --build
```

Compose publishes UDP `18888` and HTTP `12345`, mounts `./logs` at `/app/logs`. Override ports with exported env vars before running compose, e.g. `export UDP_PORT=20000 RPC_PORT=30000`.

Note: `.env` is reserved for e2e test secrets only and is not consumed by the runtime binary or compose by default.

## E2E Test .env

The e2e test `tests/e2e_send_self.rs` loads `.env` for sensitive values only.

Put only secrets in `.env`:

```
PRIVKEY=/home/you/.config/solana/id.json
```

Other test vars have safe defaults and can be exported if needed:
- `SOLANA_RPC_URL` defaults to `http://127.0.0.1:8899`
- `SUBSCRIBE_URL` defaults to `http://127.0.0.1:12345`
- `AMOUNT_SOL` defaults to `0.01`

If you want to override those, export them in your shell (do not add to `.env`):

```
export SOLANA_RPC_URL=http://127.0.0.1:8899
export SUBSCRIBE_URL=http://127.0.0.1:12345
export AMOUNT_SOL=0.01
export SKIP_PREFLIGHT=false
```

See `.env.example` for a template. It includes only `PRIVKEY` and commented examples of shell exports for the rest.
