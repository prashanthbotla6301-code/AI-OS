# runtime/

HelloAgent uses the `native` engine: its behavior is the declarative
`native.task` in `manifest.json` / `agent.json` (one `say` op). No engine
binaries, scripts, or assets live here. The `native.task` is executed inside
the sandboxed agent runtime; the sandbox grants no filesystem, network, or
process capabilities.
