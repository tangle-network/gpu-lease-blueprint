#!/usr/bin/env bash
# run-demo.sh — LOCAL DEMO: the TypeScript gpuLease resolver (from the
# agent-dev-container SDK worktree) driving the REAL gpu-lease-blueprint
# stack (anvil + tnt-core + operator + vault), end to end.
#
# The merge gate: this demo must be green before the SDK PR merges.
#
# Requirements:
#   - Docker (colima) — same as run-e2e.sh
#   - forge build                (vault artifact)
#   - the SDK worktree installed at ../agent-dev-container-gpu-lease
#     (override with DEMO_SDK_DIR)
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" == "Darwin" ]]; then
  export DOCKER_HOST="${DOCKER_HOST:-unix://$HOME/.colima/default/docker.sock}"
fi
mkdir -p .tmp-e2e
export TMPDIR="$PWD/.tmp-e2e"

exec cargo test -p gpu-lease-blueprint-lib --test demo -- --nocapture
