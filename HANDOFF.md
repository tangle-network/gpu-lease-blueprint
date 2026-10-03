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
2. **E2E on local tnt-core — now the FULL money+job lifecycle:**
   `gpu-lease-blueprint-lib/tests/anvil.rs` — `BlueprintHarness` boots an anvil
   container seeded from the bundled LocalTestnet broadcast (full Tangle stack),
   runs the REAL BlueprintRunner with our router, AND deploys the REAL vault
   bytecode (from `forge build` artifacts) on the same chain. Proven end-to-end:
   buyer escrows (vault.create) → LEASE job (request carries the VAULT leaseId;
   result echoes it — ONE identity across money and routing) → EXTEND (vault
   escrow + job session) → anvil time-warp → vault.release with EXACT pro-rata
   refund (fee-accounted balance assertions) → RELEASE job → operator withdraws
   the exact take → second lease → warp past expiry → permissionless vault.reap
   (full take via a third party) → REAP job. **I1 conservation asserted
   on-chain after every step.** `./scripts/run-e2e.sh` → green (~3s, boot
   retried x3 for colima flakes). Skips gracefully without Docker/artifacts.

   Design completion this session: `GpuLeaseRequest` carries `leaseId` — the
   vault lease the buyer already escrowed. The operator binds the device to
   THAT id (own derive_lease_id removed), the result echoes it, and the BSM
   cross-checks it against the vault. Bug found+fixed by the E2E: reap.rs
   double-released the allocator lease (UnknownLease) — now single-release,
   idempotent.

## NEW: canonical metadata + driver round-trip (done this session)

- `gpu-lease-blueprint-gen` — THE single-truth flow: Rust `sol!` types →
  EIP-712 `encode_type` → `metadata/blueprint.json` (jobs, encodeTypes,
  typehashes, category=Compute, resource descriptor). A bin + staleness test
  (`committed_metadata_matches_sol_types`) makes drift a failing test, not a
  review comment.
- `RegisterGpuLeaseBlueprint.s.sol` now embeds the canonical JSON as a
  `data:application/json;base64,...` metadataUri + keccak metadataHash
  (self-contained on-chain; switch to IPFS later on gas-capped chains — same
  pin). Also fixed: sources array was EMPTY (would have reverted
  BlueprintSourcesRequired on broadcast) — now a minimal valid container
  source (replace sha256 with the real image digest at publish).
- E2E round-trip (live chain, part of run-e2e.sh): `createBlueprint` with our
  pinned definition → read back via `blueprintMetadata` → on-chain hash ==
  keccak(json) → data URI decoded → jobs `lease/release/extend/reap`
  verified. `driver round-trip ok: registered id=1 hash=0xe72c0587...`
- Zero protocol changes. All fields/views used exist in tnt-core 0.19.

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
