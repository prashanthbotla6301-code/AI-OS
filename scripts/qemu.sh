#!/usr/bin/env bash
# Launch the built AgentOS ISO in QEMU. This is a VM test harness, not the OS
# implementation itself.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ISO="${AGENTOS_ISO:-$ROOT/build/agentos.iso}"
[[ -f "$ISO" ]] || { echo "qemu: $ISO does not exist; run make iso first" >&2; exit 1; }
QEMU="${AGENTOS_QEMU:-qemu-system-x86_64}"
command -v "$QEMU" >/dev/null 2>&1 || { echo "qemu: qemu-system-x86_64 is required (run make setup)" >&2; exit 2; }

MEM="${AGENTOS_QEMU_MEMORY:-1024}"
SMP="${AGENTOS_QEMU_SMP:-2}"
DISPLAY_MODE="${AGENTOS_QEMU_DISPLAY:-auto}"
if [[ "$DISPLAY_MODE" == auto ]]; then
    if [[ -n "${DISPLAY:-}" || -n "${WAYLAND_DISPLAY:-}" ]]; then DISPLAY_MODE=gtk; else DISPLAY_MODE=none; fi
fi

ARGS=(
    -machine q35,accel="${AGENTOS_QEMU_ACCEL:-tcg}"
    -m "$MEM" -smp "$SMP"
    -cdrom "$ISO"
    -boot d
    -vga std
    -no-reboot
    -monitor none
    -serial stdio
    -name AgentOS
)
if [[ "$DISPLAY_MODE" == none ]]; then
    ARGS+=(-display none)
else
    ARGS+=(-display "$DISPLAY_MODE")
fi

if [[ "${AGENTOS_UEFI:-0}" == 1 ]]; then
    OVMF="${AGENTOS_OVMF_CODE:-}"
    if [[ -z "$OVMF" ]]; then
        for candidate in \
            /usr/share/OVMF/OVMF_CODE_4M.fd \
            /usr/share/OVMF/OVMF_CODE.fd \
            /usr/share/edk2/ovmf/OVMF_CODE.fd; do
            [[ -f "$candidate" ]] && { OVMF="$candidate"; break; }
        done
    fi
    [[ -f "$OVMF" ]] || { echo "qemu: UEFI firmware not found; install ovmf or set AGENTOS_OVMF_CODE" >&2; exit 2; }
    # OVMF is a pflash code image; QEMU's -bios path treats it as a
    # legacy PC BIOS and rejects the larger firmware image.
    ARGS+=(-drive "if=pflash,format=raw,readonly=on,file=$OVMF")
fi

exec "$QEMU" "${ARGS[@]}"
