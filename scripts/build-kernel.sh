#!/usr/bin/env bash
# Build the pinned Linux LTS kernel used by the AgentOS ISO.
#
# Default source is the signed stable tag mirrored by gregkh/linux. The tag is
# pinned to a commit, so a moving branch cannot silently change the kernel.
# AGENTOS_KERNEL_SOURCE may point at an already unpacked source tree, and
# AGENTOS_KERNEL_IMAGE may be used by the top-level ISO build when a developer
# deliberately supplies a prebuilt bzImage.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="${AGENTOS_KERNEL_VERSION:-6.12.104}"
EXPECTED_COMMIT="${AGENTOS_KERNEL_COMMIT:-3696cfb7acec262a298e60f9fc0da59d8fc2451e}"
SOURCE_URL="${AGENTOS_KERNEL_GIT_URL:-https://github.com/gregkh/linux.git}"
JOBS="${AGENTOS_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)}"
OUT="$ROOT/build/kernel"
SRC="${AGENTOS_KERNEL_SOURCE:-$OUT/linux-$VERSION}"
KOUT="$OUT/build-$VERSION"
mkdir -p "$OUT"

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "build-kernel: missing '$1'; run make setup" >&2
        exit 2
    }
}
for tool in git make gcc awk grep sed; do need "$tool"; done

if [[ ! -d "$SRC/.git" ]]; then
    if [[ -e "$SRC" && -n "$(find "$SRC" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]]; then
        echo "build-kernel: AGENTOS_KERNEL_SOURCE/$SRC exists but is not a git checkout" >&2
        echo "  Point AGENTOS_KERNEL_SOURCE at a clean Linux $VERSION tree or remove it." >&2
        exit 1
    fi
    rm -rf "$SRC"
    echo "[kernel] fetching stable tag v$VERSION"
    git clone --filter=blob:none --depth 1 --branch "v$VERSION" --single-branch "$SOURCE_URL" "$SRC"
fi

actual="$(git -C "$SRC" rev-parse HEAD)"
if [[ "$actual" != "$EXPECTED_COMMIT" ]]; then
    echo "build-kernel: source commit mismatch" >&2
    echo "  expected: $EXPECTED_COMMIT" >&2
    echo "  actual:   $actual" >&2
    echo "  Set AGENTOS_KERNEL_COMMIT only when intentionally updating the pin." >&2
    exit 1
fi
printf '%s\n' "$actual" > "$OUT/source.commit"

# Keep generated configuration outside the source tree so the source checkout
# remains pristine and two builds can share a downloaded source tree safely.
if [[ ! -f "$KOUT/.config" || "${AGENTOS_KERNEL_RECONFIGURE:-0}" == 1 ]]; then
    rm -rf "$KOUT"
    mkdir -p "$KOUT"
    echo "[kernel] configuring Linux $VERSION"
    make -C "$SRC" O="$KOUT" ARCH=x86_64 defconfig
    (cd "$KOUT" && "$SRC/scripts/kconfig/merge_config.sh" -m .config "$ROOT/kernel/config-x86_64" >/dev/null)
    make -C "$SRC" O="$KOUT" ARCH=x86_64 olddefconfig
fi

# Load-bearing options for the initramfs, serial console, framebuffer shell,
# and UEFI/ISO boot. This fails closed instead of producing a non-booting ISO.
for option in \
    CONFIG_DEVTMPFS=y CONFIG_TMPFS=y CONFIG_PROC_FS=y CONFIG_SYSFS=y \
    CONFIG_DRM_VIRTIO_GPU=y CONFIG_DRM_BOCHS=y CONFIG_FRAMEBUFFER_CONSOLE=y \
    CONFIG_INPUT_EVDEV=y CONFIG_SERIAL_8250_CONSOLE=y CONFIG_EFI_STUB=y \
    CONFIG_ISO9660_FS=y CONFIG_UNIX98_PTYS=y CONFIG_SECCOMP_FILTER=y; do
    grep -qx "$option" "$KOUT/.config" || {
        echo "build-kernel: required $option missing after configuration" >&2
        exit 1
    }
done
if grep -qx 'CONFIG_MODULES=y' "$KOUT/.config"; then
    echo "build-kernel: modules must be disabled for the Phase 1 built-in kernel" >&2
    exit 1
fi

echo "[kernel] building bzImage with -j$JOBS"
make -C "$SRC" O="$KOUT" ARCH=x86_64 -j"$JOBS" bzImage
install -Dm644 "$KOUT/arch/x86/boot/bzImage" "$OUT/bzImage-$VERSION"
printf '%s\n' "$OUT/bzImage-$VERSION"
