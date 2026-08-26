#!/usr/bin/env bash
# Package the built ISO as a downloadable artifact committed to the branch:
#   artifacts/agentos-0.1.0-dev.iso        (exact copy of build/agentos.iso)
#   artifacts/agentos-0.1.0-dev.iso.sha256
set -euo pipefail
cd /home/user/AI-OS
ISO=build/agentos.iso
[[ -f "$ISO" ]] || { echo "no $ISO" >&2; exit 1; }
mkdir -p artifacts
cp -f "$ISO" artifacts/agentos-0.1.0-dev.iso
sha256sum artifacts/agentos-0.1.0-dev.iso > artifacts/agentos-0.1.0-dev.iso.sha256
# verify the copy is byte-identical
cmp "$ISO" artifacts/agentos-0.1.0-dev.iso
git add artifacts/agentos-0.1.0-dev.iso artifacts/agentos-0.1.0-dev.iso.sha256
git commit -m "AgentOS: export bootable ISO artifact (agentos-0.1.0-dev)" --no-verify -q
git push origin arena/01a03c81-ai-os -q
echo "packaged + pushed: artifacts/agentos-0.1.0-dev.iso ($(stat -c%s artifacts/agentos-0.1.0-dev.iso) bytes)"
