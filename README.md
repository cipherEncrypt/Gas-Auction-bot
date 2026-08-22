# gas-auction-bot

Rust bot that watches the Ethereum mempool, scores pending transactions, and submits gas-competitive replacements when the math works out.

Built for research and controlled testing. Don't point it at mainnet with real funds until you've run it locally and on a testnet first.

## What it does

```
pending tx (websocket) → filter → parse → profit/risk analysis → opportunity queue
                                                          ↓
                                              safety checks → sign → submit → monitor/replace
                                                          ↓
                                              prometheus metrics + health endpoints
```

The bot subscribes to `newPendingTransactions` over WebSocket, pulls full tx data over HTTP, runs profitability and risk scoring, then optionally signs and broadcasts a replacement with a higher gas price. Safety limits (circuit breaker, daily spend cap, hard gas ceiling) sit in front of execution.

Without a wallet key configured, it runs in analysis-only mode — detects opportunities but never submits.

## Requirements

- Rust 1.75+
- Ethereum RPC with WebSocket support (pending tx subscription)
- Funded wallet if you want execution enabled

## Setup

```bash
git clone <repo>
cd "Gas Auction Bot"
cp .env.example .env
cargo run
```

Config loads in this order: environment variables → `.env` → `config.toml` → defaults.

Env vars use the `GAS_BOT__` prefix with double underscores for nesting:

```
GAS_BOT__NETWORK__CHAIN_ID=1
GAS_BOT__WALLET__PRIVATE_KEY=0x...
```

The default `config.toml` ships with public mainnet RPCs — no API key needed:

```toml
[network]
chain_id = 1
rpc_urls = [
  "https://ethereum.publicnode.com",
  "https://eth.llamarpc.com",
  "https://1rpc.io/eth",
]
```

HTTP URLs are used for JSON-RPC calls. The bot derives the WebSocket URL automatically (`https://` → `wss://`) for the mempool subscription. Multiple URLs give you failover on the HTTP side.

`rpc_urls` also accepts a JSON string or comma-separated list in env vars:

```
GAS_BOT__NETWORK__RPC_URLS='["https://ethereum.publicnode.com"]'
GAS_BOT__NETWORK__RPC_URLS=https://ethereum.publicnode.com,https://eth.llamarpc.com
```

### Public RPC caveats

Free endpoints work for getting started, but they rate-limit aggressively and some don't expose full pending tx streams. If mempool subscription keeps dropping or you see empty streams, swap in a dedicated provider (Alchemy, Infura, QuickNode, etc.) — just drop the URL into `rpc_urls`.

## Local dev with Anvil

```bash
anvil &

GAS_BOT__NETWORK__CHAIN_ID=31337 \
GAS_BOT__NETWORK__RPC_URLS='["http://127.0.0.1:8545"]' \
GAS_BOT__PROFIT__MIN_TX_VALUE_ETH=0.001 \
GAS_BOT__WALLET__PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
cargo run
```

Use a separate wallet from whatever you're sending test transactions with, otherwise you'll hit nonce conflicts.

Full local smoke test:

```bash
./tests/integration/smoke_test.sh
```

Needs `anvil` and `cast` (Foundry).

## Configuration reference

| Section | What it controls |
|---------|-----------------|
| `network` | Chain ID, RPC endpoints |
| `gas` | Min/max gas price, replacement bump % |
| `profit` | Minimum ROI and tx value to bother analyzing |
| `safety` | Circuit breaker, daily spend cap, emergency stop |
| `analysis` | Slippage tolerance, max risk score, queue size |
| `execution` | Confirmation timeout, replacement poll interval |
| `server` | Metrics bind address, worker count, shutdown drain |

Emergency stop — halts everything immediately:

```
GAS_BOT__SAFETY__EMERGENCY_STOP=true
```

## Metrics and health

Server binds to `0.0.0.0:9090` by default.

| Endpoint | Purpose |
|----------|---------|
| `GET /metrics` | Prometheus scrape target |
| `GET /health` | Liveness |
| `GET /ready` | Readiness — returns 503 until RPC is connected |

Useful counters: `bot_transactions_processed_total`, `bot_opportunities_detected_total`, `bot_execution_successes_total`.

## Docker

```bash
cd docker
docker compose up --build
```

Healthcheck hits `/health` on port 9090.

## Tests

```bash
cargo test
cargo build --release

# Full verification (clippy, fmt, optional smoke test)
./scripts/verify.sh
```

47 tests covering config validation, tx parsing, profitability math, safety guards, metrics encoding, and worker concurrency.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for module breakdown and data flow.

## Troubleshooting

**Bot starts but `/ready` returns 503**
RPC connection failed or chain ID mismatch. Check `rpc_urls` and `chain_id` match your network.

**Mempool stream empty on public RPC**
Free endpoints often throttle or omit pending tx feeds. Switch to a paid provider with WebSocket support.

**`nonce too low` on submission**
Bot wallet nonce is out of sync, or another process is using the same key. Use a dedicated wallet; restart to re-sync from chain.

**`execution disabled` in logs**
No wallet key configured. Set `GAS_BOT__WALLET__PRIVATE_KEY` or leave empty for analysis-only mode.

**Config parse error on `RPC_URLS`**
Env var must be a JSON array string, comma-separated URLs, or a single URL. See `.env.example`.

**High CPU, no opportunities**
Normal on mainnet with conservative profit thresholds. Lower `min_tx_value_eth` and `min_profit_percent` for testing only.

## Project layout

```
src/
  blockchain/     RPC pool, mempool subscriber, tx parser
  analysis/       profit/risk scoring, opportunity detection
  execution/      gas bidding, signing, replacement loop
  metrics/        prometheus collector + HTTP server
  runtime/        worker pool, network cache, shutdown
  config/         settings loading and validation
```

Entry point is `src/main.rs`. Library modules are re-exported from `src/lib.rs`.

## Security

- Don't commit `.env` or real private keys
- Start with `emergency_stop = true` or no wallet key until you're confident in the config
- Hard gas caps and ROI floors are enforced before any submission
- Circuit breaker trips after `max_consecutive_failures` (default 5)
