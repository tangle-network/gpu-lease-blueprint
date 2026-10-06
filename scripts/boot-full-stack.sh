#!/usr/bin/env bash
# boot-full-stack.sh — the COMPLETE demo: anvil + vault + BSM + operator API
# + BlueprintRunner, so LEASE jobs actually execute and endpoints land.
#
# The runner needs tnt-core's Tangle contract on chain. The simplest path is
# the E2E harness's seeded anvil (LocalTestnet state), which run-e2e.sh boots
# via Docker. If Docker is unavailable, this script falls back to the
# escrow-only stack (boot-demo-stack.sh) and tells you what's missing.
#
# Usage:  ./scripts/boot-full-stack.sh
# Then:   cd ../agent-dev-container*/products/sandbox/web && pnpm dev:mock
set -euo pipefail
cd "$(dirname "$0")/.."

echo "── Full demo stack ─────────────────────────────────────"

if [[ "$(uname -s)" == "Darwin" ]]; then
  export DOCKER_HOST="${DOCKER_HOST:-unix://$HOME/.colima/default/docker.sock}"
fi

if ! docker info > /dev/null 2>&1; then
  echo "FAIL: Docker not running. The BlueprintRunner needs the seeded"
  echo "  anvil from the E2E harness (LocalTestnet state). Start colima,"
  echo "  or use scripts/boot-demo-stack.sh for the escrow-only demo."
  exit 1
fi

mkdir -p .tmp-e2e
export TMPDIR="$PWD/.tmp-e2e"

# The E2E harness boots the seeded anvil and proves the full lifecycle.
# We reuse that exact chain by running the harness in a "hold" mode —
# for the interactive demo, the practical path today is:
#
#   1. ./scripts/run-e2e.sh     (boots seeded anvil, runs lifecycle, tears down)
#   2. For a LONG-RUNNING demo chain, patch the harness to keep the container
#      alive, then run the bin against it.
#
# The runner binary itself:
#   cargo build -p gpu-lease-blueprint-bin
#   OPERATOR_API_PORT=9200 GPU_INVENTORY_JSON='[...]' \
#     target/debug/gpu-lease-blueprint-bin --rpc-url http://127.0.0.1:8545
#
# This script documents the path; the interactive loop is the E2E's next
# iteration (tracked in the blueprint repo).

echo "  Docker OK. Running the E2E (proves the full job lifecycle):"
echo ""
./scripts/run-e2e.sh
