# Architecture

## Overview

Single-process async Rust application. Tokio drives the event loop; worker tasks handle analysis in parallel. No external database — state lives in memory with LRU caches and atomic counters.

## Data flow

```
RPC pool (HTTP)                    WebSocket (derived from first healthy RPC)
      |                                        |
      +--- fetch tx, block, gas price          +--- eth_subscribe pending hashes
      |                                        |
      v                                        v
                         MempoolSubscriber
                    (dedup LRU, value/gas filter)
                                  |
                                  v
                      TransactionWorkerPool
                    (semaphore-bounded concurrency)
                                  |
                    +-------------+-------------+
                    |                           |
             OpportunityDetector          NetworkStateCache
           (profit + risk scoring)        (TTL-backed refresh)
                    |                           |
                    v                           |
              SafetyGuard                       |
           (circuit breaker, caps)              |
                    |                           |
                    v                           |
            ReplacementExecutor <---------------+
         (sign, submit, monitor, replace)
                    |
                    v
              BotMetrics + HTTP server
              (/metrics, /health, /ready)
```

## Modules

### `blockchain/`

- **connection.rs** — Multi-provider HTTP pool with round-robin failover, exponential backoff per endpoint, chain ID verification on connect.
- **mempool.rs** — Subscribes to pending tx hashes over WebSocket, fetches full tx via HTTP, deduplicates with LRU cache (10k entries, 5min TTL).
- **transaction.rs** — Normalizes legacy and EIP-1559 txs, decodes ERC20/721 transfers, detects Uniswap-style selectors.

### `analysis/`

- **profitability.rs** — Estimates gross profit from DEX interaction heuristics, subtracts gas and slippage, computes ROI.
- **risk_assessment.rs** — Scores congestion, liquidity, and gas volatility on a 0–100 scale; tracks historical success rates.
- **opportunity.rs** — Classifies opportunities (arbitrage, sandwich, gas replacement), maintains a priority queue capped by `max_queue_size`.

### `execution/`

- **gas_auction.rs** — Computes replacement gas as victim price + configured bump, clamped to min/max bounds. Manages nonce allocation.
- **safety.rs** — Emergency stop, circuit breaker, daily spend limit, ROI floor, hard gas cap checks before submission.
- **replacement.rs** — Full submission lifecycle: preflight → sign → broadcast → poll receipt → bump and resubmit (max 3 attempts).

### `runtime/`

- **worker.rs** — Spawns analysis tasks under a semaphore; increments Prometheus counters per processed tx.
- **network_cache.rs** — Avoids redundant `eth_gasPrice` / block fetches within TTL window.
- **shutdown.rs** — Broadcasts shutdown signal; main loop drains in-flight work for `shutdown_drain_secs`.

### `metrics/`

- **collector.rs** — Prometheus registry: throughput, latency histograms, gas spent, profit estimates, queue depth.
- **server.rs** — Minimal HTTP/1.1 server on configurable bind address.

## Config loading

Priority: `GAS_BOT__*` env vars → `.env` → `config.toml` → built-in defaults.

Validation runs after deserialize. Invalid combinations (inverted gas bounds, zero spend limit, bad RPC scheme) fail fast at startup.

## Execution modes

| Mode | Condition | Behavior |
|------|-----------|----------|
| Analysis-only | Empty `wallet.private_key` | Detects and logs opportunities, no submissions |
| Execution | Wallet key set, emergency stop off | Full pipeline including sign/submit |
| Halted | `emergency_stop = true` | Startup exits immediately |

## Performance notes

- Mempool hash processing is async and non-blocking; channel buffer is 2048.
- Worker count defaults to 8; tune via `server.worker_count` based on RPC rate limits.
- Network state refreshes every `network_cache_ttl_secs` (default 12s) in the main loop, with per-worker cache fallback.

## Failure handling

- RPC errors trigger per-endpoint backoff; after 3 consecutive failures an endpoint is skipped.
- WebSocket disconnects reconnect with exponential backoff (capped at 60s).
- Submission failures increment the circuit breaker; success resets the counter.
- Private keys are zeroized from the heap copy immediately after wallet parsing.
