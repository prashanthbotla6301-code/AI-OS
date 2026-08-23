#!/usr/bin/env bash
# Validate a developer checkout without building a kernel or image.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CARGO_BIN="${AGENTOS_CARGO:-cargo}"
RUSTC_BIN="${AGENTOS_RUSTC:-rustc}"
for tool in "$CARGO_BIN" "$RUSTC_BIN" python3; do
    if ! command -v "$tool" >/dev/null 2>&1 && [[ ! -x "$tool" ]]; then
        echo "dev-env: missing $tool" >&2
        exit 2
    fi
done

"$RUSTC_BIN" --version
"$CARGO_BIN" --version
"$CARGO_BIN" metadata --no-deps --format-version 1 >/dev/null
python3 - <<'PY'
import json
from pathlib import Path
for path in sorted(Path("packages/examples").glob("*.agent")):
    manifest = json.loads((path / "manifest.json").read_text())
    print(f"dev-env: validated JSON inputs are present: {manifest['id']}")
PY
printf '%s\n' 'dev-env: checkout is ready; next steps are make check and make test'
