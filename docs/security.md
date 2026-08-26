# AgentOS security model

Security is an architectural boundary, not a claim that Phase 2A is production
hardened.

## Invariants (Phase 1, strengthened in Phase 2A)

- Agent packages are parsed/copy-validated data. The package manager never
  executes package contents.
- The runtime executes no arbitrary agent code. Phase 2A adds the `native`
  engine, which executes a package's *declarative* `native.task` inside the
  runtime process: pure operations only (`say`, `fail`), bounded op count and
  output size, and no filesystem, network, process, or model primitives —
  there is nothing in the op set that could reach outside the sandbox. Files
  from the package are never loaded or run; `process`/`wasm` engines fail
  closed until they exist.
- The permission set is explicit and deny-by-default. An absent grant is a
  denial; `ask` is also blocked because the consent UI does not exist. A
  native agent is additionally re-checked by the executor immediately before
  execution: any denial — or an unreachable permission-manager — aborts the
  run (fail closed).
- Network, microphone, camera, GPU, process execution, settings, agent invoke,
  and memory permissions are not enabled by the shipped policy. The sandbox
  profile forces `network`/`exec` off regardless of what a package requests.
- The runtime and most system services run as the unprivileged `agent` user in
  the image. Core/APM/permission authority need root for Phase 1 filesystem or
  initialization work.
- Service sockets are group-scoped (`0660`, gid 1000 in the image). The IPC
  library refuses to replace a non-socket path and limits the protocol to
  newline-delimited JSON frames.
- Package traversal rejects symlinks, special files, invalid metadata, and
  packages over the file/byte limits. Native entries are fully validated at
  install time: a malformed task (unknown op, oversized message, too many
  ops) rejects the package.
- Kernel modules and unrelated driver families are disabled in the pinned
  build configuration; seccomp, Landlock, Yama, and lockdown support are built
  as future sandbox groundwork.

## Permission flow

```text
agent/runtime request
        -> permission-manager.permissions.check
        -> explicit grant + access/resource scope?
        -> allow or deny
```

A write grant satisfies the corresponding read permission for filesystem and
memory. A resource grant is an absolute path prefix without `..`; the current
runtime does not yet perform file operations, so this is an API contract for
the future sandbox.

The agent bus applies the same policy to agent-originated direct sends using
`agent.invoke`. System-originated service messages are infrastructure events,
not arbitrary third-party agent execution.

## Known Phase 1 gaps

The following are intentionally not hidden:

- the framebuffer shell and debug serial console auto-login as root/are brought
  up with broad device modes for QEMU bring-up;
- local Phase 1 grant methods are not yet backed by a user-authenticated consent
  UI or a separate privileged admin channel;
- Unix-socket group membership is a coarse boundary, not peer credential
  authentication;
- the initramfs is RAM-resident and has no secure disk installer, measured boot,
  signing key store, or signed package index;
- `process`/`wasm` agents, network egress, resource cgroups, seccomp profiles,
  Landlock rules, and capability dropping for untrusted agents are not yet
  implemented;
- service config is trusted image input; a production update mechanism needs
  signed, atomic configuration updates.

These gaps are why `0.1.0-dev`/`0.2.0-dev` must not be used as a
third-party-agent execution platform. Phase 2A executes only declarative
native tasks inside the runtime's own process, under the deny-by-default
authority. Phase 2 continues with authenticated operator actions, process/Wasm
sandboxing (seccomp/Landlock/capability drop), package signatures, and
resource enforcement before untrusted code can run.
