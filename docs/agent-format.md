# AgentOS `.agent` package format v1

Packages are directories with a `.agent` suffix. An install copies and
validates the directory; the package manager itself never runs package
content. Since Phase 2A, packages using the `native` engine declare a
deterministic task that the sandboxed agent-runtime executes as OS-level
agent execution — still without ever loading or running files from
`runtime/`, tools, workflows, models, or ui.

```text
video-editor.agent/
├── manifest.json       # required, schema: packages/schema/manifest-v1.schema.json
├── agent.json          # required runtime/interface descriptor
├── permissions.json    # required explicit permission declarations
├── runtime/            # engine assets (native packages declare their task
│                       #   inline; nothing here is executed)
├── tools/
├── skills/
├── workflows/
├── models/
├── ui/
└── assets/
```

The seven content directories may be empty in a minimal package. Their names
are reserved so future SDKs can add capabilities without changing the install
layout.

## Manifest

The canonical manifest shape is:

```json
{
  "format": "agentos",
  "format_version": "1.0.0",
  "id": "com.agentos.example",
  "name": "Example Agent",
  "version": "1.0.0",
  "developer": {"name": "AgentOS Project"},
  "runtime": {
    "engine": "native",
    "entry": {
      "kind": "native.task",
      "task": {"ops": [{"op": "say", "message": "Hello from AgentOS"}]}
    },
    "min_agentos": "0.1.0"
  },
  "permissions": [],
  "capabilities": ["greeting"],
  "tags": ["example"]
}
```

`format_version` and package `version` are numeric `MAJOR.MINOR.PATCH`
semver strings. Agent IDs are lowercase reverse-DNS names. Capabilities are
lowercase dotted/dashed identifiers.

Supported engine identifiers are `mock`, `native`, `process`, and `wasm`.

* `native` (Phase 2A) is the first *executable* engine. Its entry is a
  declarative `native.task` executed inside the sandboxed agent-runtime
  process — no external model, no interpreter, no subprocess.
* `mock` is simulated by the runtime (lifecycle only, no payload).
* `process` and `wasm` are accepted as forward-compatible metadata but fail
  closed at start until their sandboxed engines ship.

### Native task specification (`native.task`)

```json
{
  "kind": "native.task",
  "task": {
    "ops": [
      {"op": "say", "message": "Hello from AgentOS"}
    ]
  }
}
```

* `ops` is a bounded, ordered list of deterministic operations
  (at most 64; each message at most 4 KiB; combined output capped at 64 KiB).
* `say` appends its `message` to the task output; `fail` deterministically
  aborts the task with its `message` as the reason (used to model failure).
* There are no filesystem, network, process, or model primitives in the op
  set, so a native task cannot reach outside the runtime sandbox.
* The task is validated at **install** time (a malformed task rejects the
  package) and re-loaded from the installed `agent.json` at **run** time.
* The result is stored on the agent record (`agents.result`) and announced
  on the agent bus (`kind: "agent.result"` on `topic:agent-results`),
  which requires the `agent.invoke` permission like any agent bus use.

## Permissions

`manifest.json` contains the user-facing request. The same permission names
must also appear in `permissions.json`:

```json
{
  "permissions": [
    {
      "permission": "filesystem.read",
      "access": "read",
      "resource": "/home/agent/Documents",
      "reason": "read source documents"
    }
  ]
}
```

The Phase 1 set is:

`filesystem.read`, `filesystem.write`, `network`, `microphone`, `camera`,
`gpu.compute`, `system.settings`, `process.execute`, `agent.invoke`,
`memory.read`, and `memory.write`.

Unknown permissions, malformed IDs/versions, malformed JSON, missing required
files, package symlinks, special files, and oversized packages are rejected.
The validator also refuses symlinks so a copy cannot escape the source tree.

## Installation layout and integrity

The package manager installs to:

```text
/var/lib/agentos/agents/<id>/<version>/
/var/lib/agentos/agents/registry.json
```

The registry records the raw `manifest.json` SHA-256. `apm.verify` reports
`ok` or `TAMPERED`; this is an integrity primitive, not a signature. Package
signing, trusted developer keys, archives/store downloads, dependency solving,
and rollback are roadmap work.

## SDK

`agentos-sdk new com.vendor.example` creates a contract-complete package
skeleton. `sdk/agentos-schema` is the canonical Rust validator used by both the
SDK and package manager. `packages/schema/*.schema.json` are language-neutral
companion schemas for tooling; they are kept in sync by tests.
