# GPU Lease Blueprint

Escrowed, per-second leases of operator-owned GPUs on Tangle. The
[Sandbox SDK client](https://github.com/tangle-network/agent-container/tree/develop/products/sandbox/sdk/src/gpu-lease)
drives quoting, escrow, operator jobs, access and release.

[SPEC.md](./SPEC.md) defines the escrow invariants and SDK boundaries. The allocator serves
operator-owned hardware; cloud capacity comes through the Blueprint SDK's remote providers.
Client ABI and authentication updates are under review in
[#17186](https://github.com/tangle-network/agent-container/pull/17186). The complete
client/operator/chain proof and final security review remain pending.

## Code

| Path | Responsibility |
|---|---|
| `contracts/src/` | Lease escrow and tnt-core service manager |
| `contracts/script/` | Deployment and blueprint registration |
| `gpu-lease-blueprint-lib/` | Hardware allocation, durable state, operator HTTP API and jobs |
| `gpu-lease-blueprint-bin/` | SDK runner and reconciliation |
| `gpu-lease-blueprint-gen/` | Metadata generated from the Rust wire types |
| `scripts/run-demo.sh` | ADC client through the operator and local chain |

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

The root `Cargo.toml` owns the published Blueprint SDK version. Foundry's manifest and lockfile
own contract dependencies. Anvil integration requires Docker and the Forge artifacts.

## Deploy

Select an encrypted Foundry account or hardware wallet. Set `TANGLE_CORE` from the protocol's
deployment manifest and `BLUEPRINT_BINARY_SHA256` from the published operator artifact.
For Tempo, use the network and deployment records owned by
[tnt-core](https://github.com/tangle-network/tnt-core/tree/main/deployments/tempo).

```bash
forge script contracts/script/RegisterGpuLeaseBlueprint.s.sol \
  --rpc-url "$RPC_URL" --account "$DEPLOY_ACCOUNT" --password-file /dev/stdin --broadcast --slow
```

The external audit required by SPEC §6 precedes mainnet deployment.
