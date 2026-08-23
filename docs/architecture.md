# AgentOS architecture

AgentOS Phase 1 is a Linux initramfs distribution, not an application that is
installed into another operating system.

```text
UEFI/BIOS -> GRUB -> Linux 6.12 LTS -> /init (initramfs)
                              |
                         agentos-core (PID 1)
                              |
        +---------------------+----------------------+
        |                     |                      |
  system services       agentos-shell          /run/agentos/*.sock
        |
  permission authority -> runtime -> bus/orchestrator/model router/APM
```

## Boot and process model

The kernel is built with the Phase 1 driver and filesystem options as built-ins;
there is no kernel module dependency in the image. GRUB loads the kernel and the
compressed `newc` initramfs. `rootfs/init` sets a minimal environment and
`exec`s `agentos-core`, making the core process PID 1. Core mounts `proc`,
`sysfs`, `devtmpfs`, `devpts`, and small `tmpfs` instances, sets the hostname,
starts the service set, and reaps/restarts children.

The image is RAM-resident in Phase 1. `/var/lib/agentos` and `/var/log/agentos`
are writable during a boot but are not a disk installer or persistent OS data
store.

## Component boundaries

| Component | Process/API responsibility | Phase 1 behavior |
| --- | --- | --- |
| `agentos-core` | PID 1, config, lifecycle, state, shutdown | Real supervisor and core socket |
| `permission-manager` | Fine-grained permission authority | Deny by default; explicit local grants |
| `agent-runtime` | Agent metadata, lifecycle, sandbox/resource contract | Registry plus mock executor; no package code runs |
| `ai-orchestrator` | Goal, Plan, Task, planner interface | Deterministic offline planner |
| `agent-bus` | Envelope routing, subscriptions, audit ring | Real messages; agent sends require `agent.invoke` |
| `model-router` | Provider abstraction and usage accounting | Local `echo-mock`; cloud/hybrid declarations refuse honestly |
| `agent-package-manager` | Validate/copy/registry/integrity | Directory `.agent` packages; never executes payloads |
| `agentos-shell` | Native framebuffer desktop | Direct `/dev/fb0` renderer and evdev input |

All services are separate executables. Shared Rust crates contain only stable
cross-cutting primitives (`agentos-common`, `agentos-ipc`, and the schema SDK),
not hidden direct calls between daemons.

## IPC contract

Each service owns a Unix domain socket under `/run/agentos`. The transport is
newline-delimited JSON. Requests and responses are correlated by an unsigned
integer id:

```json
{"id":1,"method":"status","params":{}}
{"id":1,"ok":true,"result":{"version":"0.1.0-dev"}}
```

Errors use a JSON-RPC-inspired code/message object. The agent bus can push:

```json
{"event":"bus.deliver","params":{"id":"...","from":"...","to":"..."}}
```

Sockets default to mode `0660`; the real initramfs assigns them to the
`agentos` group (gid 1000), while development runs use the process user's
ownership. Filesystem permissions are a transport boundary, not a substitute
for future authenticated peer credentials.

This deliberately small protocol was selected because it needs no broker,
works before a full userspace exists, is language-neutral, and can be inspected
with a Unix-socket tool. A later D-Bus/gRPC or brokered transport can preserve
the envelopes and service methods.

## Service API summary

- Core: `ping`, `status`, `services`, `service.restart`, `service.stop`,
  `config.get`, `config.set`, `sys.info`, `shutdown`, `reboot`.
- Permissions: `permissions.list`, `policy.get`, `grants.list`,
  `grants.grant`, `grants.revoke`, `permissions.check`.
- Runtime: `agents.list`, `agents.get`, `agents.rescan`, `agents.start`,
  `agents.stop`.
- Orchestrator: `goals.submit`, `goals.list`, `plans.list`, `tasks.list`,
  `tasks.update`.
- Bus: `bus.send`, `bus.recent`, `bus.subscribe`.
- Models: `models.list`, `models.route`, `models.usage`.
- Package manager: `apm.validate`, `apm.install`, `apm.list`, `apm.verify`,
  `apm.uninstall`.

The request names and response structures are implemented next to each daemon
and are exercised by workspace tests and the in-guest autotest.

## Desktop

The initial shell is a native Rust program that draws directly to the Linux
framebuffer and reads evdev events. It has no X11, Wayland, browser, JavaScript,
Electron, or embedded web server. Its panels expose the current service APIs:
Agents, Goals, Tasks, Files, Terminal, Settings, and status. The terminal is a
PTY-backed BusyBox shell. File browsing is read-only in the UI until a future
permission-aware file operation layer is added.
