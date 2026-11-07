#!/usr/bin/env bash
# Real local chain → generated GPU BSM registration → operator → Sandbox SDK.
# Run on a governed Beelink gate; the funded flow must pass before release.
# Requires native Anvil, Forge, cargo-tangle, corepack and installed SDK dependencies.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${DEMO_SDK_DIR:?set the updated Sandbox SDK checkout}"
forge build
cargo build -p gpu-lease-blueprint-bin -p gpu-lease-blueprint-gen
exec cargo test -p gpu-lease-blueprint-lib --test demo -- --nocapture --test-threads=1
