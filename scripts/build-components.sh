#!/usr/bin/env bash
# Build all native AgentOS binaries. The image assembler consumes only the
# explicit list below; SDK/tests stay outside the boot image unless requested.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CARGO_BIN="${AGENTOS_CARGO:-cargo}"
RUSTC_BIN="${AGENTOS_RUSTC:-rustc}"
if [[ -n "${AGENTOS_RUSTC:-}" ]]; then
    export RUSTC="$AGENTOS_RUSTC"
fi
if ! command -v "$CARGO_BIN" >/dev/null 2>&1 && [[ ! -x "$CARGO_BIN" ]]; then
    echo "build-components: cargo is required (run make setup or set AGENTOS_CARGO)" >&2
    exit 2
fi

if [[ -n "${AGENTOS_CARGO_TARGET_DIR:-}" ]]; then
    export CARGO_TARGET_DIR="$AGENTOS_CARGO_TARGET_DIR"
elif [[ -z "${CARGO_TARGET_DIR:-}" ]]; then
    export CARGO_TARGET_DIR="$ROOT/target"
fi
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

TARGET="${AGENTOS_TARGET:-}"
TARGET_ARGS=()
if [[ -n "$TARGET" ]]; then
    TARGET_ARGS=(--target "$TARGET")
else
    TARGET="$("$RUSTC_BIN" -vV | awk '/^host: / {print $2}')"
fi

PROFILE="release"
echo "[components] building target $TARGET"
"$CARGO_BIN" build --locked --workspace --bins --release "${TARGET_ARGS[@]}"

if [[ -n "${AGENTOS_TARGET:-}" ]]; then
    BIN_DIR="$CARGO_TARGET_DIR/$TARGET/$PROFILE"
else
    BIN_DIR="$CARGO_TARGET_DIR/$PROFILE"
fi
OUT="$ROOT/build/components/bin"
rm -rf "$OUT"
mkdir -p "$OUT"

# Names are stable API/packaging names, even where Cargo package names use
# hyphens. Do not glob: a newly-added developer tool must not enter PID 1.
BINS=(
    agentos-core
    agentos-ctl
    agentos-runtime
    agentos-orchestrator
    agentos-bus
    agentos-apm
    agentos-permissions
    agentos-models
    agentos-shell
)
for name in "${BINS[@]}"; do
    src="$BIN_DIR/$name"
    [[ -x "$src" ]] || { echo "build-components: missing $src" >&2; exit 1; }
    install -Dm755 "$src" "$OUT/$name"
done

# Record the exact target and source revision alongside the image inputs.
printf '%s\n' "$TARGET" > "$ROOT/build/components/target.txt"
git rev-parse HEAD > "$ROOT/build/components/source.commit" 2>/dev/null || true

if [[ "${AGENTOS_STRIP:-1}" == 1 ]] && command -v strip >/dev/null 2>&1; then
    for bin in "$OUT"/*; do strip --strip-unneeded "$bin" 2>/dev/null || true; done
fi

echo "[components] binaries staged in $OUT"
