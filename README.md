# GPU Lease Blueprint

Decentralized GPU compute marketplace on Tangle. Operators with GPUs offer them for rent;
sandboxes (and any blueprint) attach via the sandbox SDK's `gpuLease`.

**Read [SPEC.md](./SPEC.md) first** — the supreme design: the contract is a fixed point of
invariants (escrow conservation, quote binding, slashing); everything that evolves — pricing
curves, GPU generations, topology, attach transports — is off-chain data with versioned schemas.
Designed so the only future churn is dependency updates.

## Status (evidence, not claims)

| Layer | State | Proof |
|---|---|---|
| Contract `src/GpuLeaseVault.sol` | **DONE** — real pro-rata settlement, conservation-enforcing | `forge test` → 35/35 (I1–I5 pinned, 1k-run fuzz) |
| Invariant suite `test/GpuLeaseVault.t.sol` | **DONE** — exact-equality money assertions | same |
| Rust lib `gpu-lease-blueprint-lib/` | **DONE** — allocator, quote policy, EIP-191 credentials, 4 jobs | `cargo test -p gpu-lease-blueprint-lib` → 28/28 |
| Rust bin `gpu-lease-blueprint-bin/` | **COMPILES** — runner + periodic reaper sweep wired | `cargo check --workspace` clean |
| Deploy script | vault deploy | `script/DeployGpuLeaseVault.s.sol` |
| tnt-core registration (`RegisterBlueprint`) | **NOT STARTED** — next | needs tnt-core Types import |
| SDK `gpuLease` resolver | **NOT STARTED** — lives in sandbox-sdk repo | SPEC §5 |
| External audit | **NOT STARTED** — before mainnet money, non-negotiable | SPEC §6.4 |

## Layout

```
SPEC.md                          design law (frozen)
src/GpuLeaseVault.sol            escrow state machine (I1–I5)
test/GpuLeaseVault.t.sol         invariant-pinning suite
script/DeployGpuLeaseVault.s.sol vault deploy
gpu-lease-blueprint-lib/         operator: allocator + quote policy + credentials + jobs
gpu-lease-blueprint-bin/         blueprint runner (BlueprintRunner + reaper sweep)
```

## Build & verify

```bash
forge install foundry-rs/forge-std --no-commit   # once (dependencies/ is gitignored)
forge test                                        # 35 tests, I1–I5
cargo test --workspace                            # 28 tests (lib)
cargo check --workspace                           # bin compiles
```

The Rust workspace pins the Blueprint SDK graph to the same `tangle-network/blueprint` rev as
`ai-agent-sandbox-blueprint` (proven graph; see `[patch.crates-io]` in the root `Cargo.toml`).
