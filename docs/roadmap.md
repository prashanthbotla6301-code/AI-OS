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

## v0.2.0-dev — Phase 2A native agent execution (this branch)

- [x] `native` engine in the `.agent` format: declarative, deterministic
      `native.task` spec with bounded ops/output budgets
- [x] HelloAgent example package (`com.agentos.helloagent`) returning
      `Hello from AgentOS`
- [x] install-time validation of native entries (malformed task ⇒ rejected
      package) and run-time revalidation inside agent-runtime
- [x] NativeExecutor: pure ops, budget caps, permission re-check (fail
      closed), result capture, `agents.result` API
- [x] full agent lifecycle events (installed/validated, permission check,
      starting, running, failed, stopping, stopped) published on the agent
      bus; desktop Agents panel shows the captured result
- [x] end-to-end Phase 2A integration suite (`tests/src/phase2a.rs`):
      validate → install → discover → denied → grant → start → execute →
      bus delivery → stop → failed-path → uninstall refusal while running →
      uninstall → verify removal → reinstall
- [x] in-guest autotest extended with the native agent lifecycle (24 checks)
- [ ] Landlock/seccomp/no-new-privileges/capability-drop profiles around a
      process-based sandbox (Phase 2B)
- [ ] authenticated operator/session identity and a privileged consent path

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
