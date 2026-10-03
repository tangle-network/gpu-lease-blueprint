# GPU Lease Blueprint — Session Handoff

**Repo:** https://github.com/tangle-network/gpu-lease-blueprint
**Date:** 2026-10-03 (updated after the completion session)
**Spec:** `SPEC.md` in the repo root — read it first; it is frozen design law.

## Where we are — UPDATED: the two "next steps" from the last handoff are DONE

The last handoff said the contract wasn't done until its tests said so. They now say so:

1. **Contract + invariant suite: DONE.** `_settle()` placeholder is gone — real exact
   pro-rata (`refund = price * remaining`, exact because `escrow == price * paidSeconds`
   by construction; derivation pinned by fuzz). 35/35 Foundry tests green at 1000 fuzz
   runs: I1 conservation after every step (vault-empty terminal states), I2 overstay
   impossibility mid-flight, I3 exact refunds, I4 single settlement (all four orderings),
   I5 record immutability. Also fixed en route: `foundry.toml` `libs` string→array (was
   broken from the first commit), uint64/uint128 overflows now fail closed with
   `Overflow()` instead of `Panic(0x11)`, overpay refunded at call boundaries so I1
   holds exactly, double `LeaseReleased` emit removed.
2. **The Rust blueprint EXISTS now.** Full workspace mirroring
   `ai-agent-sandbox-blueprint`'s proven structure (same SDK pin, `[patch.crates-io]` →
   tangle-network/blueprint rev `5405c01`):
   - `gpu-lease-blueprint-lib/` — 28/28 tests: co-located **allocator** (class/TEE-
     exclusive binding, reap sweep), **quote policy** (per-class base, class bps, TEE
     premium, piecewise utilization curve, fail-closed redeemed-price ceiling),
     **EIP-191 challenge-session credentials** (single-use challenges, signer recovery,
     scoped expiring tokens, lease-bound revocation), and the four SPEC §1 jobs
     (LEASE=0/RELEASE=1/EXTEND=2/REAP=3) as extractor-style handlers with public-data-only
     outputs + `router()`.
   - `gpu-lease-blueprint-bin/` — `BlueprintRunner` wiring + 30s reaper sweep
     (frees devices/revokes credentials at escrow exhaustion — the operator side of I2).
     Compiles clean; NOT yet run against a live chain.
3. `script/DeployGpuLeaseVault.s.sol` — vault deploy script (anvil default key).

## Continuation point (in order)

1. **tnt-core registration**: write `RegisterBlueprint` for the 4 jobs (import tnt-core
   `Types.sol`, mirror `ai-agent-sandbox-blueprint/contracts/script/RegisterBlueprint.s.sol`).
   Job IDs are pinned in `gpu-lease-blueprint-lib/src/lib.rs` (0–3, sequential).
2. **Live smoke test**: anvil chain + `blueprint-anvil-testing-utils` (sibling has the
   harness) — LEASE→EXTEND→RELEASE happy path end-to-end, verify vault accounting on the
   result path. This is where bin bugs (if any) surface.
3. **SDK `gpuLease` resolver** (sandbox-sdk repo, SPEC §5): create sandbox → collect N
   quotes → validate (#1568 client) → LEASE → attach via endpoint v1 → teardown w/ refund.
4. **Audit** before mainnet money (SPEC §6.4 — non-negotiable).

## Known-not-done (do NOT claim otherwise)

- Bin has never connected to a chain — registration + smoke test are the proof it needs.
- No `RegisterBlueprint` yet — nothing is registered anywhere.
- The allocator is in-memory per-process (fine for v1 single-operator; multi-process
  needs the local-database store, `blueprint-store-local-database` is in the patch set).
- Reap-sweep interval (30s) is hardcoded in `bin/src/main.rs` — env-ify when it matters.

## Environment notes (carried forward)

- `forge install foundry-rs/forge-std --no-commit` after fresh clone (`dependencies/` is
  gitignored; forge-std v1.17.0 pinned by the install).
- Rust toolchain: 1.91 (`rust-toolchain.toml`), same as the sibling repo.
- The SDK graph patch section must stay in sync with
  `ai-agent-sandbox-blueprint/Cargo.toml` — it's the proven-resolution pin.
- Foundry config: `libs = ["dependencies"]` (ARRAY — the string form breaks forge 1.8).
- vm.expectRevert gotcha that cost time: an unfunded caller's `{value:...}` call fails
  the EVM-level balance check → empty revert data, NOT the contract's custom error.
