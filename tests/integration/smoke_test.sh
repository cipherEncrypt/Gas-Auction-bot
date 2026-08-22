#!/usr/bin/env bash
# End-to-end smoke test against a local Anvil node.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

ANVIL_PORT="${ANVIL_PORT:-18545}"
METRICS_PORT="${METRICS_PORT:-19090}"
ANVIL_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
BOT_BIN="${BOT_BIN:-}"

cleanup() {
  if [[ -n "${ANVIL_PID:-}" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
    kill "$ANVIL_PID" 2>/dev/null || true
  fi
  if [[ -n "${BOT_PID:-}" ]] && kill -0 "$BOT_PID" 2>/dev/null; then
    kill "$BOT_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

echo "==> Building bot"
TARGET_DIR="$(cargo metadata --format-version=1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
cargo build --quiet
BOT_BIN="$TARGET_DIR/debug/gas-auction-bot"

echo "==> Starting Anvil on port $ANVIL_PORT"
anvil --port "$ANVIL_PORT" --chain-id 31337 >/tmp/gas-auction-anvil.log 2>&1 &
ANVIL_PID=$!
sleep 2

curl -sf -X POST "http://127.0.0.1:$ANVIL_PORT" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' >/dev/null

echo "==> Starting bot"
GAS_BOT__NETWORK__CHAIN_ID=31337 \
GAS_BOT__NETWORK__RPC_URLS='["http://127.0.0.1:'"$ANVIL_PORT"'"]' \
GAS_BOT__PROFIT__MIN_TX_VALUE_ETH=0.001 \
GAS_BOT__PROFIT__MIN_PROFIT_PERCENT=1.0 \
GAS_BOT__ANALYSIS__MAX_RISK_SCORE=95 \
GAS_BOT__SERVER__BIND_ADDRESS="127.0.0.1:$METRICS_PORT" \
GAS_BOT__WALLET__PRIVATE_KEY="$ANVIL_KEY" \
RUST_LOG=info \
"$BOT_BIN" >/tmp/gas-auction-bot.log 2>&1 &
BOT_PID=$!

for _ in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:$METRICS_PORT/ready" | grep -q ready; then
    break
  fi
  sleep 1
done

echo "==> Checking health endpoints"
curl -sf "http://127.0.0.1:$METRICS_PORT/health" | grep -q healthy
curl -sf "http://127.0.0.1:$METRICS_PORT/ready" | grep -q ready
curl -sf "http://127.0.0.1:$METRICS_PORT/metrics" | grep -q bot_transactions_processed_total

echo "==> Broadcasting test transaction"
cast send --value 1ether \
  0x70997970C51812dc3A010C7d01b50e0d17dc79C8 \
  --rpc-url "http://127.0.0.1:$ANVIL_PORT" \
  --private-key "$ANVIL_KEY" >/dev/null

sleep 5

echo "==> Verifying bot processed transactions"
curl -sf "http://127.0.0.1:$METRICS_PORT/metrics" | grep -q 'bot_transactions_processed_total [1-9]'
grep -q "pending transaction received\|transaction analyzed\|opportunity detected" /tmp/gas-auction-bot.log

echo "PASS: integration smoke test succeeded"
