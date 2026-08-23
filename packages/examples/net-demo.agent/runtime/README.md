# runtime/

Engine assets for this agent. `engine: "mock"` (see manifest.json) means the
Phase 1 runtime simulates execution — this directory is intentionally empty of
executable code. When `process`/`wasm` engines arrive (Phase 2+), the entry
descriptor in agent.json names the artifact that lands here.
