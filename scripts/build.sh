#!/usr/bin/env bash
# Explicit OS image orchestration entry point. build-iso also lazily invokes
# these stages, but this script makes the pipeline readable to CI and humans.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

./scripts/build-components.sh
./scripts/build-initramfs.sh
if [[ -z "${AGENTOS_KERNEL_IMAGE:-}" ]]; then
    ./scripts/build-kernel.sh
fi
./scripts/build-iso.sh
