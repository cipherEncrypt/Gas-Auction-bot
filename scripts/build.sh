#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "Building release binary..."
cargo build --release

TARGET_DIR="$(cargo metadata --format-version=1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
BINARY="$TARGET_DIR/release/gas-auction-bot"

if [[ ! -f "$BINARY" ]]; then
  echo "Build failed: binary not found at $BINARY"
  exit 1
fi

echo "Build OK — $(du -h "$BINARY" | cut -f1)"

echo "Running release tests..."
cargo test --release --quiet

echo "Done."
