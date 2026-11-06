# GPU Lease Blueprint

Escrowed, per-second leases of operator-owned GPUs on Tangle. The
[Sandbox SDK client](https://github.com/tangle-network/agent-container/tree/develop/products/sandbox/sdk/src/gpu-lease)
drives quoting, escrow, operator jobs, access and release.

[SPEC.md](./SPEC.md) owns D8. The allocator serves operator hardware; cloud capacity uses the SDK.
The [ADC update](https://github.com/tangle-network/agent-container/pull/17186), complete lease
lifecycle proof and held contract review remain pending.

## Build and verify

```bash
git submodule update --init --recursive
forge soldeer install
forge build
cargo test --workspace
forge test
./scripts/run-e2e.sh
DEMO_SDK_DIR=/path/to/agent-container/products/sandbox/sdk ./scripts/run-demo.sh
```

Workspace and Foundry manifests own dependencies. The lib Anvil suite needs Docker; registration
uses native Anvil, the CLI, Forge artifacts and a built operator.

## Deploy

Use an SDK keystore (`cargo tangle key import --help` supports interactive import); keep keys out
of arguments and logs. Supply the deployed manager as `GPU_LEASE_BSM` and the publisher's
[SDK sources array](https://github.com/tangle-network/blueprint/blob/main/cli/src/command/deploy/definition.rs)
with real artifact locations and SHA-256 digests as `BLUEPRINT_SOURCES_FILE`.
The installed CLI's help owns deployment flags; its SDK settings loader owns required settings.
Protocol addresses come from [tnt-core](https://github.com/tangle-network/tnt-core/tree/main/deployments/tempo).
Production broadcasts await the approved D8 stack.

```bash
cargo run -p gpu-lease-blueprint-gen -- \
  --definition "$DEFINITION_FILE" --manager "$GPU_LEASE_BSM" --sources "$BLUEPRINT_SOURCES_FILE"
cargo tangle blueprint deploy tangle --network testnet \
  --definition "$DEFINITION_FILE" --settings-file "$SETTINGS_FILE" \
  --http-rpc-url "$RPC_URL" --ws-rpc-url "$WS_RPC_URL" --keystore-path "$KEYSTORE_PATH" \
  --tangle-contract "$TANGLE_CORE" --staking-contract "$STAKING_CONTRACT" \
  --status-registry-contract "$STATUS_REGISTRY_CONTRACT"
```

The generator preserves current metadata and config; the SDK encodes, signs and submits.
Prove normal registration against the full protocol snapshot:

```bash
cargo build -p gpu-lease-blueprint-bin
cargo test -p gpu-lease-blueprint-gen --test registration -- --nocapture
```
