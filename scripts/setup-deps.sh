#!/usr/bin/env bash
# Install the host tools used by the AgentOS image pipeline.
set -euo pipefail

if [[ "$(id -u)" -eq 0 ]]; then
    SUDO=()
elif command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then
    SUDO=(sudo)
else
    echo "setup-deps: root or passwordless sudo is required to install packages" >&2
    exit 1
fi

if command -v apt-get >/dev/null 2>&1; then
    echo "[deps] installing Debian/Ubuntu prerequisites"
    "${SUDO[@]}" apt-get update
    "${SUDO[@]}" apt-get install -y \
        bc bison build-essential busybox cpio curl flex \
        grub-common grub-efi-amd64-bin grub-pc-bin \
        libelf-dev libncurses-dev libssl-dev mtools musl-tools \
        ovmf pkg-config qemu-system-x86 qemu-utils \
        rustc cargo xorriso xz-utils
else
    cat >&2 <<'EOF'
setup-deps: this helper currently supports Debian/Ubuntu hosts.
Install the equivalent of these packages with your distribution's package
manager, then rerun the checks:
  Rust toolchain (rustc, cargo), gcc/make, bc, bison, flex, libssl/libelf/
  ncurses development headers, BusyBox, cpio, xz, GRUB BIOS+UEFI tools,
  xorriso, QEMU x86_64, and OVMF.
EOF
    exit 1
fi

if ! command -v rustup >/dev/null 2>&1; then
    echo "[deps] rustup is not installed; installing the official rustup launcher"
    rustup_tmp="$(mktemp)"
    curl --fail --location --proto '=https' --tlsv1.2 https://sh.rustup.rs -o "$rustup_tmp"
    sh "$rustup_tmp" -y --default-toolchain none --profile minimal
    rm -f "$rustup_tmp"
    export PATH="$HOME/.cargo/bin:$PATH"
fi

if command -v rustup >/dev/null 2>&1; then
    echo "[deps] ensuring the pinned Rust toolchain is available"
    rustup toolchain install 1.88.0 --profile minimal
    rustup component add rustfmt --toolchain 1.88.0
    if command -v musl-gcc >/dev/null 2>&1; then
        echo "[deps] adding optional static musl target"
        rustup target add x86_64-unknown-linux-musl --toolchain 1.88.0
    else
        echo "[deps] musl target not installed; builds will use the host glibc target"
        echo "       Install musl-tools if you want a static initramfs build."
    fi
else
    echo "setup-deps: rustup is required because rust-toolchain.toml pins Rust 1.88.0" >&2
    echo "Install rustup from https://rustup.rs, then rerun make setup." >&2
    exit 1
fi

echo "[deps] prerequisites installed"
