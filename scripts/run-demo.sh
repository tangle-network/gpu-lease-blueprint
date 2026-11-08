#!/usr/bin/env bash
# Real local chain → generated GPU BSM registration → operator → Sandbox SDK.
# Run on a governed Beelink gate; the funded flow must pass before release.
# Requires Anvil, Forge, cargo-tangle and Node; DEMO_SDK_DIR selects a Sandbox source checkout over the pinned npm client.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ -n ${DEMO_SDK_DIR:-} ]] || npm ci --prefix tests/sandbox-client --ignore-scripts --no-audit --no-fund
forge build
cargo build -p gpu-lease-blueprint-bin -p gpu-lease-blueprint-gen
exec cargo test -p gpu-lease-blueprint-lib --test demo -- --nocapture --test-threads=1
