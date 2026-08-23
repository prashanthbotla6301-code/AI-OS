# AgentOS

**AgentOS v0.1.0-dev** is an AI-native operating-system foundation. It is a
bootable Linux image whose first-class system software is designed around
agents, goals, permissions, and model routing—not a web application or a
chatbot wrapper.

```text
Hardware -> Linux kernel -> AgentOS Core -> system services
                                      -> agent runtime -> agents / user goals
```

## Current phase

Phase 1 builds the bootable foundation:

- pinned Linux kernel with built-in QEMU/UEFI/console/framebuffer support;
- GRUB ISO boot path for legacy BIOS and UEFI;
- BusyBox-backed initramfs with `agentos-core` as PID 1;
- Rust services for core supervision, the agent runtime, deterministic goal
  planning, agent-to-agent bus contracts, package validation, permissions, and
  model routing;
- a direct Linux framebuffer/evdev Rust desktop shell with AgentOS branding,
  Agents, Goals, Tasks, Files, Terminal, Settings, and system-status panels;
- deny-by-default permissions and a mock runtime that never executes package
  code;
- reproducible, scriptable component/image builds and automated tests.

The Phase 1 planner and model provider are deliberately deterministic
scaffolding. A package with `process` or `wasm` runtime metadata can be
validated and installed, but cannot execute yet. No network/cloud provider or
untrusted agent code is run by this release.

## Build and boot

After installing prerequisites documented in [`docs/build.md`](docs/build.md),
run one command from the repository root:

```sh
make iso
```

This produces `build/agentos.iso`. To boot it in QEMU with a desktop window:

```sh
make qemu
```

To run the automated headless boot test (kernel, initramfs, all services, IPC,
and desktop self-test), use:

```sh
make boot-test
```

`make boot-test` rebuilds the ISO with the `agentos.autotest` kernel flag,
starts QEMU, captures the serial transcript at
`build/test/serial.log`, and fails unless the in-guest test reports all checks
passing. It requires QEMU and a built kernel; it does not substitute a host
process or web simulation for an OS boot.

For development without a VM, the Rust workspace can be tested with:

```sh
cargo test --workspace --all-targets --locked
```

## Architecture

The components are independent Rust processes. They communicate across Unix
domain sockets using newline-delimited, versioned JSON request/response
envelopes. See [`docs/architecture.md`](docs/architecture.md) for the
service API table, lifecycle boundaries, and why this IPC was selected.

- `system/agentos-core` is PID 1 and the service supervisor.
- `system/agent-runtime` owns the Agent abstraction and lifecycle state machine.
- `system/ai-orchestrator` owns Goal -> Plan -> Task structures and the mock
  planner plug-in point.
- `system/agent-package-manager` validates and installs `.agent` directories;
  it never executes package contents.
- `system/permission-manager` is a deny-by-default permission authority.
- `system/agent-bus` defines and gates future agent-to-agent messages.
- `system/model-router` defines local/cloud/hybrid model providers and usage
  metadata; only the local deterministic echo provider works in Phase 1.
- `desktop/shell` is a native framebuffer shell, not an application running
  inside another desktop environment.

## Repository map

```text
kernel/                 Linux configuration fragment
rootfs/                 immutable image inputs and /init
system/                 independent AgentOS daemons
libs/                   shared paths, logging, and IPC primitives
desktop/shell/          native framebuffer/evdev desktop
packages/schema/        JSON schemas for the .agent contract
packages/examples/      safe validation-only example packages
sdk/                    package schema and developer scaffolding SDK
scripts/                dependency, build, image, QEMU, and test tooling
docs/                   architecture, security, build, boot, and roadmap
tests/                  workspace/integration tests
```

## Limitations and honesty boundary

Phase 1 is not a general-purpose desktop distribution. The kernel and image
are intentionally small, state is RAM-resident, and the shell targets x86_64
framebuffer hardware. There is no package signing, archive download/store,
real sandboxed agent execution, LLM backend, consent UI, persistent disk
installer, or production device/driver coverage yet. Dangerous permissions are
denied unless an operator explicitly creates a Phase 1 grant through the
local service interface; the future release must add authenticated consent and
stronger sandboxing before third-party code is executable.

See [`docs/roadmap.md`](docs/roadmap.md) for the boundary between Phase 1 and
later work.

## License

The AgentOS source is MIT OR Apache-2.0. The bundled DejaVu fonts retain their
upstream license; see `rootfs/usr/share/agentos/fonts/LICENSE-dejavu.txt`.
