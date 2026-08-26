# AgentOS roadmap

## v0.1.0-dev — Phase 1 foundation (this repository)

- [x] UEFI/BIOS GRUB ISO build path
- [x] Linux kernel configuration and QEMU target
- [x] PID-1 AgentOS Core with service lifecycle and state
- [x] Unix-socket IPC envelopes and service APIs
- [x] native framebuffer/evdev AgentOS shell
- [x] terminal, file browser, settings/status panels
- [x] agent lifecycle/runtime/sandbox/resource contracts
- [x] deterministic Goal -> Plan -> Task planner
- [x] package format, validation, install registry, integrity hash
- [x] deny-by-default permission authority
- [x] agent-bus and model-router skeletons
- [x] automated unit/schema/service tests and QEMU acceptance harness
- [ ] persistent disk installer and broad physical hardware coverage
- [ ] production security hardening (not a Phase 1 claim)

## Phase 2 — trusted execution boundary

- authenticated operator/session identity and a privileged consent path;
- signed manifests/packages, trust roots, rollback, dependency resolution;
- real process/Wasmtime execution only inside a sandbox;
- Landlock/seccomp/no-new-privileges/capability drop profiles;
- cgroups or equivalent CPU/memory/time budgets and audit events;
- permission-aware filesystem and network brokers;
- persistent storage/installer, update/rollback, and hardware discovery;
- replace the root debug console and broad device permissions.

## Phase 3 — model and agent ecosystem

- local model backends and optional cloud/hybrid providers behind model-router;
- token/cost/resource budgets, provider credentials isolated from agents, and
  offline policy controls;
- agent capability discovery/selection and task dispatch;
- production agent-bus subscriptions, identities, replay/backpressure, and
  durable audit log;
- memory service, semantic file indexing, scheduled/background goals, and
  workflow execution.

## Phase 4 — distribution

- signed Agent Store/index and developer publishing workflow;
- native `.agent` archive format with reproducible builds;
- graphical consent, package updates, user profiles, accessibility,
  localization, power management, and desktop hardware support;
- independent security audits, fuzzing, threat-model review, and release
  qualification on real UEFI hardware.

The roadmap intentionally keeps Phase 1 small. A feature does not belong in the
boot image merely because a future architecture diagram names it; it belongs
when its security boundary and testable interface are real.
