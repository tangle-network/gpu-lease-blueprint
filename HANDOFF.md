# GPU Lease Blueprint — Session Handoff

**Repo:** https://github.com/tangle-network/gpu-lease-blueprint
**Date:** 2026-10-03 (updated after the tnt-core + E2E session)
**Spec:** `SPEC.md` in the repo root — read it first; it is frozen design law.

## Where we are — UPDATED: tnt-core integration + local E2E are DONE

Everything from the prior handoffs stands (contract I1–I5 35/35, Rust lib 28/28).
New this session:

1. **Proper tnt-core integration (soldeer tnt-core 0.19.0, sibling layout):**
   - `contracts/src/GpuLeaseBlueprint.sol` — the BSM: `onJobCall` caches inputs
     (0.19 passes only inputsHash at result time); `onJobResult(LEASE)` binds
     leaseId→operator ONLY after cross-checking the REAL vault lease (exists,
     Live, intentHash match, operator match, lessee match, price match).
     Nonexistent leases read as empty state-0 structs — caught via expiry==0.
   - `contracts/script/RegisterGpuLeaseBlueprint.s.sol` — deploys vault + BSM,
     `createBlueprint` with 4 job definitions (LEASE=0/RELEASE=1/EXTEND=2/REAP=3,
     order pinned to the Rust router).
   - 11 tnt-core integration tests (foundry): routing, all fail-closed paths,
     vault-money+BSM-routing full lifecycle. **forge test: 46/46.**
2. **E2E on local tnt-core (the real proof):**
   `gpu-lease-blueprint-lib/tests/anvil.rs` — `BlueprintHarness` boots an anvil
   container seeded from the bundled LocalTestnet broadcast (full Tangle stack),
   runs the REAL BlueprintRunner with our router, and submits jobs ON-CHAIN:
   LEASE → GpuLeaseOutput{leaseId, endpoint v1, schemaVersion=1} → EXTEND →
   RELEASE. **`./scripts/run-e2e.sh` → green in ~2s.** Skips gracefully without
   Docker so `cargo test --workspace` stays green anywhere.

## Continuation point (in order)

1. **Register against a persistent LocalTestnet anvil** (not the ephemeral
   harness): `forge script contracts/script/RegisterGpuLeaseBlueprint.s.sol
   --rpc-url $RPC --broadcast` + run the bin against it, mirroring the sibling's
   `deploy-local.sh` flow. This proves the register script + bin end-to-end.
2. **SDK `gpuLease` resolver** (sandbox-sdk repo, SPEC §5).
3. **Audit** before mainnet money (SPEC §6.4 — non-negotiable).

## Known-not-done (do NOT claim otherwise)

- The register SCRIPT is written+compiling but has not been broadcast against a
  live chain (the E2E used the harness's own registration path).
- The bin has not been run as a standalone process against a chain.
- Multi-operator E2E (harness supports `operator_specs`) — v1 proof is single-op.
- SDK resolver + audit remain.

## Environment notes (all proven this session)

- **E2E needs Docker**: `./scripts/run-e2e.sh` encodes the two macOS+colima traps:
  `DOCKER_HOST=unix://$HOME/.colima/default/docker.sock` (bollard default socket
  doesn't exist) and `TMPDIR` under `/Users` (colima only shares /Users into the
  VM; /tmp bind-mounts fail with "bind source path does not exist").
- Foundry deps via soldeer: `forge soldeer install` (tnt-core 0.19.0, forge-std
  1.9.6). remappings.txt is committed — has /src suffixes (soldeer's generated
  ones lacked them).
- `libs = ["dependencies"]` (ARRAY — string form breaks forge 1.8).
- forge 1.8 gotcha: `vm.expectRevert(SomeError.selector)` FAILS on errors with
  args — use `vm.expectPartialRevert`.
- prank gotcha: reading `bsm.JOB_X()` inside a pranked call's arg list CONSUMES
  the prank (it's itself a call) — hoist job-id reads to setUp.
