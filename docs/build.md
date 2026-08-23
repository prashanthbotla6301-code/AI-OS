# Building AgentOS

## Host prerequisites

The supported build host is x86_64 Debian/Ubuntu. `make setup` installs:

- Rust 1.88+ (`rustc`, `cargo`; the repository pins 1.88 with
  `rust-toolchain.toml`), gcc/make, and Linux build tools;
- BusyBox, cpio, gzip/xz, and kernel headers/libraries;
- GRUB BIOS and x86_64 EFI modules plus xorriso;
- QEMU x86_64 and OVMF firmware for acceptance boots.

Run:

```sh
make setup
```

The helper uses `apt-get` and requires root or passwordless sudo. On another
Linux distribution install equivalent packages manually. A standard host Rust
target works; a musl toolchain (`musl-tools` plus
`rustup target add x86_64-unknown-linux-musl`) is preferred for a smaller
static initramfs.

## One-command ISO build

After prerequisites:

```sh
make iso
```

The pipeline is:

1. `scripts/build-components.sh` builds all Rust binaries with `cargo
   build --locked --workspace --bins --release` and stages only the named OS
   executables.
2. `scripts/build-initramfs.sh` copies the explicit rootfs inputs, BusyBox,
   example packages, binaries, and only the dynamic libraries reported by
   `ldd`. `scripts/mkcpio.py` emits a deterministic cpio archive including
   `/dev/console` metadata without requiring host `mknod`.
3. `scripts/build-kernel.sh` fetches the pinned Linux stable tag
   `v6.12.104` at commit
   `3696cfb7acec262a298e60f9fc0da59d8fc2451e`, merges
   `kernel/config-x86_64`, verifies load-bearing options, and builds `bzImage`.
4. `scripts/build-iso.sh` creates a GRUB ISO with both BIOS El Torito and EFI
   boot entries.

Output:

```text
build/agentos.iso
build/agentos.iso.sha256
```

Generated files are ignored by Git. `SOURCE_DATE_EPOCH=0` is used by default
for deterministic archive/ISO input timestamps; set it explicitly to a fixed
Unix timestamp in CI if your release process uses a different epoch.

## Useful overrides

```sh
# Use a prebuilt kernel for fast image iteration (still a real kernel boot).
AGENTOS_KERNEL_IMAGE=/path/to/bzImage make iso

# Build static musl binaries if the target is installed.
AGENTOS_TARGET=x86_64-unknown-linux-musl make iso

# Use a local cargo/rustc installation.
AGENTOS_CARGO=/opt/rust/bin/cargo AGENTOS_RUSTC=/opt/rust/bin/rustc make components

# Tune parallel kernel compilation.
AGENTOS_JOBS=4 make kernel
```

`AGENTOS_KERNEL_SOURCE` may point to a clean git checkout of the pinned Linux
tag. If its commit is not the pinned commit, the build fails rather than
silently changing the kernel.

## Tests and checks

```sh
make check   # shell/JSON checks, rustfmt --check, and cargo check
make test    # build bins, unit/schema/integration tests
```

The service integration test starts the real compiled daemons in `--dev` mode,
using temporary directories and real Unix sockets. It never needs host root.

## Clean rebuild

```sh
make clean
make iso
```

A kernel source checkout/download can be large. Removing `build/kernel` forces
source/configuration regeneration; setting `AGENTOS_KERNEL_IMAGE` avoids a
kernel rebuild while iterating on userspace.
