#!/usr/bin/env bash
# run-e2e.sh — full tnt-core E2E on local anvil (via Docker/colima).
#
# Proven working command sequence on macOS + colima:
#   - bollard needs the docker socket (colima's is NOT /var/run/docker.sock)
#   - the anvil container bind-mounts a state file from TMPDIR; colima only
#     shares /Users into the VM, so TMPDIR must live under /Users.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" == "Darwin" ]]; then
  export DOCKER_HOST="${DOCKER_HOST:-unix://$HOME/.colima/default/docker.sock}"
fi
mkdir -p .tmp-e2e
export TMPDIR="$PWD/.tmp-e2e"

exec cargo test -p gpu-lease-blueprint-lib --test anvil -- --nocapture
