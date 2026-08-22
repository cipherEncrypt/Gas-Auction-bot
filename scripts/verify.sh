#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> rustc"
rustc --version

echo "==> build"
cargo build --release

echo "==> tests"
cargo test --release --quiet

echo "==> clippy"
cargo clippy --all-targets -- -D warnings

echo "==> fmt"
cargo fmt -- --check

if command -v cargo-audit >/dev/null 2>&1; then
  echo "==> audit"
  cargo audit
else
  echo "==> audit skipped (cargo-audit not installed)"
fi

if command -v anvil >/dev/null 2>&1 && command -v cast >/dev/null 2>&1; then
  echo "==> integration smoke test"
  ./tests/integration/smoke_test.sh
else
  echo "==> integration skipped (install Foundry for anvil + cast)"
fi

echo "All checks passed."
