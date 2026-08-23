# AgentOS development

## Repository conventions

AgentOS components are independent Rust processes. Add cross-component types to
a shared contract crate only when they are actually on a wire or persistence
boundary; do not make every service a direct library call. Keep boot/image
inputs under `rootfs`, `kernel`, and `scripts` and keep generated output under
`build/`.

The fixed development branch for this checkout is
`arena/01a02cf3-ai-os`; do not create a second branch for local changes.

## Fast feedback loop

```sh
make check
cargo test --workspace --all-targets --locked
```

For the full real-service integration:

```sh
make test
```

The integration test builds all bins, launches core in `--dev` mode, points its
config at those binaries, and exercises real Unix sockets. It uses temporary
run/state/etc/log directories and cannot mount filesystems or become PID 1.

For userspace image iteration after a kernel exists:

```sh
AGENTOS_KERNEL_IMAGE=build/kernel/bzImage-6.12.104 make iso
```

For a repeatable guest acceptance run:

```sh
make boot-test
```

## Adding a service

1. Add a dedicated `system/<name>` Cargo package and binary.
2. Define its JSON methods and error behavior in its source/documentation.
3. Add a service entry to `rootfs/etc/agentos/agentos.json` with an explicit
   user and socket filename.
4. Add its binary to the explicit list in `build-components.sh` and
   `build-initramfs.sh`.
5. Use the shared IPC crate; do not open an ad hoc TCP listener.
6. Add unit tests for domain transitions and an integration call in
   `tests/src/integration.rs` where practical.
7. Update the API table in `docs/architecture.md`.

Core starts services by executable path and appends canonical `--socket`,
`--run-dir`, `--state-dir`, and `--etc-dir` arguments. A service must not
assume a working directory or write outside its declared state/log paths.

## Adding package fields

Update all three surfaces together:

- Rust types/validator in `sdk/agentos-schema`;
- JSON companion schema under `packages/schema`;
- examples and tests/docs.

Unknown dangerous permissions must be rejected, and metadata changes must not
turn into execution in Phase 1.

## Debugging a guest

The serial console is the stable diagnostic channel. Set
`AGENTOS_QEMU_DISPLAY=none` and capture QEMU output, or inspect:

```text
/run/agentos/boot.json
/run/agentos/shell.json
/var/log/agentos/*.log
/var/log/agentos/*.out
```

`agentos-ctl` is intentionally a small operator client. The services also
accept a request by writing one JSON line to the documented Unix socket. Avoid
using host absolute paths in the shell or service code; use `Paths` overrides
for tests.

## Reproducibility and review

Use `cargo --locked`. Do not commit `target/`, kernel sources, ISO files, logs,
or host credentials. Kernel source is pinned to an immutable commit and the
build fails if it changes. Review any increase to image contents, privileges,
permissions, or IPC socket exposure as a security change.
