#!/usr/bin/env bash
# Assemble the complete AgentOS userspace into a deterministic cpio initramfs.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BUSYBOX="${AGENTOS_BUSYBOX:-$(command -v busybox || true)}"
[[ -x "$BUSYBOX" ]] || { echo "build-initramfs: BusyBox is required (run make setup)" >&2; exit 2; }

COMPONENT_BIN="$ROOT/build/components/bin"
if [[ ! -x "$COMPONENT_BIN/agentos-core" ]]; then
    AGENTOS_CARGO="${AGENTOS_CARGO:-cargo}" "$ROOT/scripts/build-components.sh"
fi

OUT="$ROOT/build/initramfs"
STAGE="$OUT/stage"
ARCHIVE="$OUT/agentos-initramfs.cpio.gz"
rm -rf "$OUT"
mkdir -p "$STAGE" "$STAGE/bin" "$STAGE/sbin" "$STAGE/usr/bin" "$STAGE/dev" "$STAGE/proc" "$STAGE/sys" "$STAGE/run" "$STAGE/tmp" "$STAGE/var" "$STAGE/var/lib/agentos" "$STAGE/var/log/agentos" "$STAGE/home" "$STAGE/home/agent" "$STAGE/root" "$STAGE/opt"

# Copy the static image inputs first. Source files are data; no host /etc,
# /dev, or /proc content is ever copied into the guest.
cp -a rootfs/etc "$STAGE/"
cp -a rootfs/usr "$STAGE/"
for dir in rootfs/home rootfs/opt rootfs/var; do
    if [[ -d "$dir" ]]; then cp -a "$dir/." "$STAGE/${dir#rootfs/}/"; fi
done
install -Dm755 rootfs/init "$STAGE/init"

# BusyBox is intentionally the only non-Rust executable in the base image.
# Install applet symlinks so /bin/sh and the emergency console work without a
# second userspace distribution.
install -Dm755 "$BUSYBOX" "$STAGE/bin/busybox"
(
    cd "$STAGE"
    ./bin/busybox --install -s /bin >/dev/null 2>&1 || true
)
# Some BusyBox builds place applets in /usr/bin; make the essential paths
# explicit and harmless if --install already created them.
for applet in sh mount umount mkdir mknod cat echo printf sleep grep sed ls cp mv rm ps kill dmesg clear stty setsid cttyhack switch_root reboot poweroff; do
    ln -sfn /bin/busybox "$STAGE/bin/$applet"
done

# System components are separate processes but share one image boundary.
for name in \
    agentos-core agentos-ctl agentos-runtime agentos-orchestrator agentos-bus \
    agentos-apm agentos-permissions agentos-models agentos-shell; do
    install -Dm755 "$COMPONENT_BIN/$name" "$STAGE/usr/bin/$name"
done

# Example packages are validation fixtures, not executable programs. They are
# bundled only so the in-guest acceptance test exercises a real install path.
mkdir -p "$STAGE/opt/agentos/packages"
cp -a packages/examples/. "$STAGE/opt/agentos/packages/"

# Host-target Rust builds may be dynamically linked. A musl build has no
# dependencies to copy; a glibc build gets exactly the loader/libraries ldd
# reports, never the host filesystem wholesale.
copy_runtime_deps() {
    local binary="$1" dep
    if ! ldd "$binary" >/dev/null 2>&1; then return 0; fi
    while read -r dep; do
        [[ -f "$dep" ]] || continue
        install -D "$dep" "$STAGE$dep"
    done < <(ldd "$binary" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }' | sort -u)
}
if command -v ldd >/dev/null 2>&1; then
    for binary in "$STAGE/usr/bin"/*; do copy_runtime_deps "$binary"; done
fi

# Runtime directories are writable in this RAM-resident Phase 1 image. The
# core mounts /run and /tmp as tmpfs during real boot.
chmod 1777 "$STAGE/tmp"
chmod 0755 "$STAGE/run" "$STAGE/var/lib/agentos" "$STAGE/var/log/agentos"

SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}" python3 scripts/mkcpio.py "$STAGE" "$OUT/agentos-initramfs.cpio"
gzip -n -9 -c "$OUT/agentos-initramfs.cpio" > "$ARCHIVE"
rm -f "$OUT/agentos-initramfs.cpio"
sha256sum "$ARCHIVE" > "$ARCHIVE.sha256"
printf '%s\n' "$ARCHIVE"
