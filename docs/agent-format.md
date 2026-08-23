# AgentOS `.agent` package format v1

Phase 1 packages are directories with a `.agent` suffix. They are data-only
inputs to the package manager. A package manager install copies and validates
the directory; it never runs `runtime/`, tools, workflows, models, or UI files.

```text
video-editor.agent/
├── manifest.json       # required, schema: packages/schema/manifest-v1.schema.json
├── agent.json          # required runtime/interface descriptor
├── permissions.json    # required explicit permission declarations
├── runtime/            # engine assets (descriptors only in Phase 1)
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
    "engine": "mock",
    "entry": {"kind": "builtin:echo"},
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

Supported engine identifiers are `mock`, `process`, and `wasm`. Only `mock`
can be selected by the Phase 1 runtime, and it simulates lifecycle state
without executing an agent payload. `process` and `wasm` are accepted as
forward-compatible metadata but fail closed at start.

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
