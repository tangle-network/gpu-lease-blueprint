# GPU Lease Blueprint

Escrowed, per-second leases of operator-owned GPUs on Tangle. The
[Sandbox SDK client](https://github.com/tangle-network/agent-container/tree/develop/products/sandbox/sdk/src/gpu-lease)
drives quoting, escrow, operator jobs, access and release.

[SPEC.md](./SPEC.md) owns D8. The allocator serves operator hardware; cloud capacity uses the SDK.
`GPU_INVENTORY_JSON` is UNVERIFIED operator input; attestation awaits Drew's trust-root decision.

## Build and verify

```bash
git submodule update --init --recursive
forge soldeer install
forge build
cargo test --workspace
forge test
DEMO_SDK_DIR=/path/to/agent-container/products/sandbox/sdk ./scripts/run-demo.sh
```

The lib Anvil suite needs Docker; registration uses native Anvil, the CLI and built artifacts.

## Cloud capacity deployment (pending SDK release)

The opt-in `cloud` feature deploys a caller-selected workload through the SDK;
it needs a published SDK containing `lock_lease` and `recover_lease`. The current
dependency pin does not contain these APIs. Source-override checks are interim proof.

After adopting that release, build the existing operator binary with `--features cloud`:

```bash
GPU_CLOUD_CAPACITY=1 target/debug/gpu-lease-blueprint < "$PRIVATE_CONFIGURATION"
```

The bounded JSON input has exactly `credentials`, `spec`, and `state_dir` keys.
Use the SDK's [ProviderCredentials](https://github.com/tangle-network/blueprint/blob/main/crates/blueprint-remote-providers/src/managed/credentials.rs)
and [ResourceSpec](https://github.com/tangle-network/blueprint/blob/main/crates/core/src/resources.rs)
formats. Supply ownership, GPU class/count, a positive duration and price cap,
an immutable image digest, explicit workload command and a stable SSH public key.
Keep the corresponding private key with the caller. Provider credentials stay on stdin;
workload configuration is explicit and never inherits the launcher's environment.

Use a dedicated, initially empty private state directory and retain it for recovery.
The public receipt contains SDK connection facts after bounded status/readiness checks.
These are provider observations, not hardware attestation or proof of image execution.
Ctrl-C/SIGTERM retires capacity through the SDK and waits for absence; deadline retirement
uses the original SDK deadline. Interrupted or failed cleanup resumes with the same input
and state, without another rental. Missing/corrupt state and unproved account rotation fail
closed. Do not reuse a retired identity or erase its journal to authorize a new rental.
An abruptly stopped supervisor needs a recovery invocation; provider auto-expiry is not assumed.

This deployment mode leaves local GPU inventory and paid LEASE jobs unchanged. It does not
attach a remote node's CUDA devices to a Sandbox on another host or add cloud fallback to
marketplace jobs. Input inventory remains UNVERIFIED while the attestation trust root is held.

## Deploy

Use an SDK keystore (`cargo tangle key import --help` supports interactive import); keep keys out
of arguments and logs. Supply the deployed manager as `GPU_LEASE_BSM` and the publisher's
[SDK sources array](https://github.com/tangle-network/blueprint/blob/main/cli/src/command/deploy/definition.rs)
with real artifact locations and SHA-256 digests as `BLUEPRINT_SOURCES_FILE`.
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

Prove SDK registration against the full protocol snapshot:

```bash
cargo build -p gpu-lease-blueprint-bin
cargo test -p gpu-lease-blueprint-gen --test registration -- --nocapture
```
