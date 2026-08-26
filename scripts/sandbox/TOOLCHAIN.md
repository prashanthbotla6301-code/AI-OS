# Sandbox toolchain recipe (Arena environment)

This directory documents how the AgentOS ISO is built **inside the Arena
sandbox**, where the egress allowlist is limited to github.com,
api.github.com, codeload.github.com, pypi.org, files.pythonhosted.org and
registry.npmjs.org, and no distro packages (apt) are available. These scripts
are **not** part of the AgentOS product; the canonical build is
`scripts/build-{components,initramfs,kernel,iso}.sh` run on a normal
workstation. Sandbox-only patches are applied to throwaway trees under /tmp.

Persistence warning: only git-tracked files survive between sessions here.
/opt, /tmp, .cargo/ and build/ are recreated each session.

## 1. Rust (for scripts/build-components.sh)

* `pip install arena-rust-toolchain` wheels (data1/2/3, zstd-compressed tar of
  the toolchain) -> /opt/arena-rust/toolchain (cargo + rustc 1.97).
* Crates.io is unreachable -> re-vendor the 26 workspace crates from upstream
  GitHub tags: `python3 scripts/sandbox/vendor.py` (map of repo/tag per crate
  is inside; generic-array pinned to commit 1397d2bb40 for 0.14.7).
* Recreate `/home/user/AI-OS/.cargo/config.toml` (git-ignored, NEVER commit):

  ```toml
  [source.crates-io]
  replace-with = "vendored-sources"
  [source.vendored-sources]
  directory = "/opt/arena-rust/vendor"
  ```

* Verify with `cargo check --workspace --locked`.

## 2. Kernel build tools (for scripts/build-kernel.sh)

* bison/flex/m4/bc: prebuilt AOSP host binaries from
  `LineageOS/android_prebuilts_build-tools` `linux-x86/bin/{bison,flex,m4,
  gavinhoward-bc}` (+ `linux-x86/lib64/libc++.so` for bison, fetched via the
  api.github.com contents API) installed to /opt/aosp-tools.
  bison 3.8.2 additionally needs its data dir: `BISON_PKGDATADIR` pointing at
  bison 3.8.2 `data/{m4sugar,skeletons,xslt}` (m4sugar files from
  autotools-mirror/autoconf lib/m4sugar; the git-tree copies are dangling
  submodule symlinks) and `M4=/opt/aosp-tools/bin/m4` must be exported.
* libelf (objtool): build `arachsys/libelf` (freestanding elfutils libelf):
  disable `USE_ZSTD` in src/config.h, drop src/elf_compress.c (add a stub for
  `__libelf_decompress_elf` returning ELF_E_UNKNOWN_ERROR since elf_strptr
  references it), `make libelf.a`, install headers to /usr/include and
  libelf.a to /usr/lib/x86_64-linux-gnu.
* OpenSSL is NOT obtainable (crypto repos are egress-blocked by name), so the
  built-in keyring certs tooling is dropped: after defconfig+merge, sed
  `CONFIG_SYSTEM_TRUSTED_KEYRING`, `CONFIG_SYSTEM_BLACKLIST_KEYRING`,
  `CONFIG_SYSTEM_REVOCATION_LIST` to not-set before olddefconfig, then run
  scripts/build-kernel.sh (it skips configuration when .config exists).
  AgentOS uses no modules, kexec or signed-kernel loading, so the keyring is
  unused in the Phase 1 image.
* `scripts/sandbox/kernel-sandbox.sh` = clone-retry (SSL drops happen) +
  config pre-seed + build-kernel.sh invocation.

## 3. ISO tooling (for scripts/build-iso.sh)

Built under /opt/iso-tools (codeload tarballs, all with pre-generated
`configure`):

* xorriso 1.3.0 (`Distrotech/xorriso`) - plain ./configure/make/make install.
* mtools 4.0.48 (`deepin-community/mtools`) - plain ./configure/make/install.
* grub 2.00 (`Distrotech/grub`), TWO builds into the same prefix:
  * i386-pc: `--with-platform=pc --target=i386-pc-linux-gnu
    --program-transform-name='s&$&-pc&'`
  * x86_64-efi: `--with-platform=efi --target=x86_64-linux-gnu
    --program-transform-name='s&$&-efi&'`
* grub-2.00-on-modern-toolchain patches (applied to the /tmp extraction only):
  1. binutils >= 2.39 rejects `-Wl,-N` links with "PHDR segment not covered":
     configure vars `TARGET_LDFLAGS_OLDMAGIC`, `TARGET_IMG_LDFLAGS`,
     `TARGET_IMG_LDFLAGS_AC` get `-no-pie` prepended; configure/make run with
     `CFLAGS="-O2 -fno-pie" LDFLAGS="-no-pie" TARGET_LDFLAGS="-no-pie"`.
  2. glibc >= 2.28: `#include <sys/sysmacros.h>` added to util/getroot.c and
     util/raid.c (major/makedev).
  3. gas >= 2.31 emits R_X86_64_PLT32 (reloc 0x4) for `call` in .S files:
     `case R_X86_64_PLT32:` added next to `case R_X86_64_PC32:` in
     util/grub-mkimagexx.c (the upstream 2.02 fix).
  4. After `make install`, symlink untransformed names:
     /opt/iso-tools/bin/grub-mkimage -> grub-mkimage-pc etc. (grub-mkrescue
     looks up untransformed `grub-mkimage`).
* grub.cfg for the EFI core: 2.00's grub-mkrescue embeds a memdisk
  `search --fs-uuid --set=root` + insmods, then sources the ISO's
  /boot/grub/grub.cfg - identical behaviour to 2.12's mkrescue used by CI.

## 4. ISO assembly + verification

* `scripts/sandbox/make-iso-sandbox.sh` mirrors scripts/build-iso.sh:
  iso-root/boot/{agentos-kernel,agentos-initramfs.img,grub/grub.cfg} and
  `grub-mkrescue-pc -o build/agentos.iso iso-root` with SOURCE_DATE_EPOCH=0.
* Verification (xorriso 1.3 predates -report_el_torito, use -toc):
  `xorriso -indev build/agentos.iso -toc` must show
  `Boot record : El Torito`, boot image `eltorito.img` (BIOS) and `efi.img`
  with `platform_id=0xEF` (UEFI). Byte 0x1FE must be `55 aa`
  (protective/embbedded-boot MBR). Payload files inside the ISO must match
  the build outputs byte-for-byte (xorriso -osirrox extract + cmp).

## 5. Artifact packaging

`scripts/sandbox/package-artifact.sh` copies build/agentos.iso to
artifacts/agentos-0.1.0-dev.iso (+ .sha256), verifies byte-identity, commits
and pushes to the session branch.
