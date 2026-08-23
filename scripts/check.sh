#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CARGO_BIN="${AGENTOS_CARGO:-cargo}"
if ! command -v "$CARGO_BIN" >/dev/null 2>&1 && [[ ! -x "$CARGO_BIN" ]]; then
    echo "check: cargo is required (run make setup or set AGENTOS_CARGO=/path/to/cargo)" >&2
    exit 2
fi

for file in scripts/*.sh; do
    bash -n "$file"
done
python3 - <<'PY'
import json
from pathlib import Path
for path in sorted(Path('.').rglob('*.json')):
    if any(part in {'.git', 'target', 'build'} for part in path.parts):
        continue
    json.loads(path.read_text())
print('check: JSON documents parse')
PY

[[ -x rootfs/init ]] || { echo "check: rootfs/init must be executable" >&2; exit 1; }
[[ -f kernel/config-x86_64 ]] || { echo "check: kernel configuration fragment missing" >&2; exit 1; }
[[ -f packages/schema/manifest-v1.schema.json ]] || { echo "check: manifest schema missing" >&2; exit 1; }

"$CARGO_BIN" fmt --all -- --check
"$CARGO_BIN" check --workspace --all-targets --locked
printf '%s\n' 'check: formatting and Rust workspace checks passed'
