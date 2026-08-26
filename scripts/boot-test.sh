#!/usr/bin/env bash
# Build an autotest ISO and prove it boots in QEMU. The guest writes its
# acceptance result to a serial log before powering off.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

QEMU="${AGENTOS_QEMU:-qemu-system-x86_64}"
command -v "$QEMU" >/dev/null 2>&1 || { echo "boot-test: QEMU is required (run make setup)" >&2; exit 2; }

# Build a separate test-flavoured ISO. The flag is in GRUB's kernel command
# line, so the test covers the actual boot path rather than invoking services
# directly on the host.
AGENTOS_AUTOTEST=1 ./scripts/build-iso.sh
ISO="$ROOT/build/agentos.iso"
TEST_DIR="$ROOT/build/test"
mkdir -p "$TEST_DIR"

find_ovmf() {
    if [[ -n "${AGENTOS_OVMF_CODE:-}" && -f "$AGENTOS_OVMF_CODE" ]]; then
        printf '%s\n' "$AGENTOS_OVMF_CODE"
        return 0
    fi
    for candidate in \
        /usr/share/OVMF/OVMF_CODE_4M.fd \
        /usr/share/OVMF/OVMF_CODE.fd \
        /usr/share/edk2/ovmf/OVMF_CODE.fd; do
        [[ -f "$candidate" ]] && { printf '%s\n' "$candidate"; return 0; }
    done
    return 1
}

run_guest() {
    local label="$1"
    local firmware_arg="$2"
    local log="$TEST_DIR/serial-$label.log"
    local -a firmware=( )
    if [[ -n "$firmware_arg" ]]; then
        firmware=(-drive "$firmware_arg")
    fi
    rm -f "$log"
    echo "[boot-test] starting $label boot"
    set +e
    timeout --foreground "${AGENTOS_BOOT_TIMEOUT:-120}s" "$QEMU" \
        -machine q35,accel="${AGENTOS_QEMU_ACCEL:-tcg}" \
        -m "${AGENTOS_QEMU_MEMORY:-1024}" -smp "${AGENTOS_QEMU_SMP:-2}" \
        -cdrom "$ISO" -boot d -vga std -display none \
        -monitor none -serial "file:$log" -no-reboot "${firmware[@]}"
    local status=$?
    set -e
    # A normal guest poweroff makes QEMU exit 0; timeout is 124 and any other
    # status is useful context but the serial assertions below are authoritative.
    if [[ "$status" -eq 124 ]]; then
        echo "[boot-test] $label timed out" >&2
        tail -80 "$log" >&2 || true
        return 1
    fi
    grep -q 'AgentOS v0.1.0-dev booting' "$log" || { echo "[boot-test] $label did not reach AgentOS Core" >&2; tail -80 "$log" >&2; return 1; }
    grep -q '\[AGENTOS-TEST\] RESULT:' "$log" || { echo "[boot-test] $label produced no in-guest result" >&2; tail -80 "$log" >&2; return 1; }
    grep -Eq '\[AGENTOS-TEST\] RESULT: [0-9]+/[0-9]+ PASSED' "$log" || { echo "[boot-test] $label reported a malformed result" >&2; tail -80 "$log" >&2; return 1; }
    if grep -Eq '\[AGENTOS-TEST\] FAIL ' "$log"; then
        echo "[boot-test] $label reported failing checks" >&2
        grep '\[AGENTOS-TEST\]' "$log" >&2 || true
        return 1
    fi
    echo "[boot-test] $label passed; serial log: $log"
}

run_guest bios ""
OVMF="$(find_ovmf || true)"
if [[ -z "$OVMF" ]]; then
    echo "boot-test: OVMF firmware is required for the UEFI acceptance boot" >&2
    echo "         Install ovmf or set AGENTOS_OVMF_CODE=/path/OVMF_CODE.fd" >&2
    exit 2
fi
run_guest uefi "if=pflash,format=raw,readonly=on,file=$OVMF"
printf '%s\n' 'boot-test: BIOS and UEFI QEMU boots passed'
