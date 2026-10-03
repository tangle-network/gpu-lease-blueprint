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
| Contract `contracts/src/GpuLeaseVault.sol` | **DONE** — real pro-rata settlement, conservation-enforcing | `forge test` → 35/35 (I1–I5 pinned, 1k-run fuzz) |
| BSM `contracts/src/GpuLeaseBlueprint.sol` | **DONE** — tnt-core service manager (routing + result binding against the vault) | 11 integration tests in the same run |
| Registration `contracts/script/RegisterGpuLeaseBlueprint.s.sol` | **DONE** — tnt-core `createBlueprint` w/ 4 job definitions | compiles; job order pinned to Rust constants |
| Rust lib `gpu-lease-blueprint-lib/` | **DONE** — allocator, quote policy, EIP-191 credentials, 4 jobs | `cargo test -p gpu-lease-blueprint-lib` → 28/28 |
| Rust bin `gpu-lease-blueprint-bin/` | **COMPILES** — runner + periodic reaper sweep wired | `cargo check --workspace` clean |
| **tnt-core E2E on local anvil** | **DONE** — LEASE→EXTEND→RELEASE through real tnt-core (seeded anvil container, real BlueprintRunner, on-chain job results) | `./scripts/run-e2e.sh` → 1/1 green in ~2s |
| SDK `gpuLease` resolver | **NOT STARTED** — lives in sandbox-sdk repo | SPEC §5 |
| External audit | **NOT STARTED** — before mainnet money, non-negotiable | SPEC §6.4 |

## Layout

```
SPEC.md                             design law (frozen)
contracts/src/GpuLeaseVault.sol     escrow state machine (I1–I5)
contracts/src/GpuLeaseBlueprint.sol tnt-core BSM (routing + vault-verified result binding)
contracts/test/                     46 tests: invariants + tnt-core integration
contracts/script/                   deploy + tnt-core registration
scripts/run-e2e.sh                  local tnt-core E2E (anvil container via Docker)
gpu-lease-blueprint-lib/            operator: allocator + quote policy + credentials + jobs
gpu-lease-blueprint-bin/            blueprint runner (BlueprintRunner + reaper sweep)
```

## Build & verify

```bash
forge soldeer install                                        # once (tnt-core 0.19.0, forge-std 1.9.6)
forge test                                                   # 46 tests: I1–I5 + tnt-core wiring
cargo test --workspace                                       # 28 unit tests (e2e skips without Docker)
./scripts/run-e2e.sh                                         # full tnt-core E2E on local anvil
```

The Rust workspace pins the Blueprint SDK graph to the same `tangle-network/blueprint` rev as
`ai-agent-sandbox-blueprint` (proven graph; see `[patch.crates-io]` in the root `Cargo.toml`).
