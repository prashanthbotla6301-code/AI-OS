# Booting AgentOS

## QEMU desktop boot

Build first, then run:

```sh
make qemu
```

QEMU starts with a virtual `std` VGA device, serial console, 1 GiB RAM, and
Q35 machine type. With a graphical host the native AgentOS framebuffer shell
is visible in the QEMU window. Without a display, `make qemu` falls back to a
serial-only QEMU instance; `ttyS0` remains an emergency BusyBox console.

The visible shell starts only after the kernel creates `/dev/fb0`; it does not
use a display server. The console shows the AgentOS logo/name and the six
initial panels. The terminal panel starts a PTY-backed BusyBox shell, Files
browses the guest filesystem, and Settings reads core status and the loaded
permission policy.

## Automated BIOS + UEFI acceptance boot

Run:

```sh
make boot-test
```

This rebuilds `build/agentos.iso` with `agentos.autotest=1` in the GRUB kernel
command line, then boots it headlessly twice:

1. legacy BIOS through the ISO El Torito path;
2. UEFI through OVMF (`/usr/share/OVMF/OVMF_CODE_4M.fd` by default).

The test waits for real PID 1, all six service sockets, core supervision,
permission denial, package validation/install, runtime lifecycle, goal planning,
model routing, bus routing, and the shell heartbeat. The guest writes result
lines to serial logs:

```text
build/test/serial-bios.log
build/test/serial-uefi.log
```

The command succeeds only when the logs contain a complete
`[AGENTOS-TEST] RESULT: ... PASSED` line and no test failure. A QEMU timeout or
missing UEFI firmware is an error; no host-side simulation is substituted.

To use non-default tools:

```sh
AGENTOS_QEMU=/opt/qemu/bin/qemu-system-x86_64 \
AGENTOS_OVMF_CODE=/firmware/OVMF_CODE.fd \
make boot-test
```

The `AGENTOS_QEMU_ACCEL=kvm` override can be used on a host with KVM; the
default `tcg` is portable.

## Serial/debug consoles

The kernel command line includes both `console=tty0` and
`console=ttyS0,115200n8`. PID 1 attempts to spawn a BusyBox root shell on
`tty1` and `ttyS0`. In a QEMU serial terminal, useful commands include:

```sh
agentos-ctl status
agentos-ctl services
agentos-ctl agents
agentos-ctl packages
agentos-ctl goal "organize my downloads"
```

The auto-login root debug console and permissive device setup are Phase 1
bring-up choices. They must be replaced by a real login/console policy before a
security-focused release.

## What a successful boot proves

It proves the ISO loads Linux under both firmware paths, `/init` creates a real
PID 1, AgentOS Core mounts the required virtual filesystems, native services
start and communicate over sockets, the native framebuffer shell can start,
and the explicit Phase 1 interfaces work. It does not prove physical hardware
coverage, persistent installation, real model inference, or safe third-party
agent execution.
