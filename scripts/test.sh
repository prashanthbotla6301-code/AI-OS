#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
CARGO_BIN="${AGENTOS_CARGO:-cargo}"

"$CARGO_BIN" build --workspace --bins --locked
"$CARGO_BIN" test --workspace --all-targets --locked
printf '%s\n' 'test: Rust unit, schema, and integration tests passed'
