# GPU Lease Blueprint — Session Handoff

**Repo:** https://github.com/tangle-network/gpu-lease-blueprint
**Date:** 2026-10-03 · **Author:** drewstone (session agent)
**Spec:** `SPEC.md` in the repo root — read it first; it is frozen design law.

## Where we are

The marketplace crypto core was proven end-to-end in the prior session (operator signs RFQ quote
→ buyer validates → tnt-core accepts on a live anvil chain — blueprint#1568, merged). This repo is
the next product: the bare-bones GPU-lessor blueprint.

Pushed so far:
- `SPEC.md` — the supreme design. Contract = fixed point of invariants (I1 escrow conservation,
  I2 overstay impossibility, I3 refund atomicity, I4 single settlement, I5 quote binding).
  Everything evolving (pricing, GPU generations, attach transports) is off-chain versioned data.
- `src/GpuLeaseVault.sol` — frozen-core skeleton: create/extend/release/reap/slash, operator
  earnings withdrawal, schema-versioned events carrying public data only.

## THE CRITICAL ITEM — start here

**`_settle()` in GpuLeaseVault.sol contains PLACEHOLDER pro-rata math.** It is marked in-code.
It is wrong-by-design until the test suite exists. Do not deploy, do not wire money, do not
"quick-fix" it blind. The next session's first deliverable:

### 1. The invariant-pinning Foundry test suite (`test/GpuLeaseVault.t.sol`)

Write tests that pin, at minimum:
- I1: for fuzzed (pricePerSecond, duration, extend-times, release-times):
  `operatorEarnings + Σ live escrow + refunded == totalEscrowed` at every step.
- I2: no state where `expiry - now` exceeds `escrow / pricePerSecond`.
- I3: release refunds EXACTLY `escrow × remainingSeconds / paidSeconds`, atomically.
- I4: release-then-reap and reap-then-release both revert (`NotLive`).
- I5: intentHash/operator/lessee immutable (attempt mutation → impossible; struct fields are
  only written at `create`).
- Edge: price=0, duration=0, value<cost, expiry overflow at type boundaries.

Then implement the real pro-rata in `_settle` and keep the placeholder tests red-then-green.
Storage note: exact pro-rata needs paidSeconds tracked (or derive: `escrow/price` at settle
undercounts after extends — decide derivation vs. explicit field; the tests decide, not taste).

### 2. Then, in order (SPEC §6)
- Operator allocator (co-located GPU passthrough) + quote policy — copy the composable
  env-driven policy from ai-agent-sandbox-blueprint `operator_quote.rs` (#177, merged).
- Credentials off-chain via EIP-191 session auth — machinery exists and is proven
  (operator-api session_auth). NEVER in results/events (SPEC §2; the bug class we killed).
- SDK `gpuLease` resolver: collect N quotes → validate (TS rfq.ts, proven) → LEASE → attach →
  RELEASE/REAP. Multi-quote failover is the scheduler (no chain-side scheduler).
- External audit before mainnet. Non-negotiable.

## Environment facts that cost time last session — read once, save an hour

- **macOS cannot build** `sandbox-runtime`/`ai-agent-sandbox-blueprint-bin` (microvm-runtime nix
  mknod i32/u64). Verify Rust against the blueprint repo or CI; `--no-verify` with rationale.
- **Docker via colima**: restart with `colima start --cpu 6 --memory 10`. The harness needs
  `DOCKER_HOST=unix:///Users/drew/.colima/default/docker.sock` AND `TMPDIR=/Users/drew/.tmp-bp-anvil/`
  (colima doesn't mount /var/folders).
- **The anvil snapshot harness**: `cargo test -p blueprint-tangle-extra --features keepers --test
  anvil_integration <name>` with the env above boots real anvil containers against
  `crates/chain-setup/anvil/snapshots/localtestnet-state.json`.
- Seeded service allowlists its owner (deployer key `0xac09…ff80`) — buyer must be the owner or
  permitted caller (`NotPermittedCaller(uint64,address)` = `0xd5dd5b44`).
- `rg -rn` is replace-not-recursive; `cd` does not persist between tool calls.

## Related merged work this week (context for reviewers)

- blueprint#1565/#1567: canonical RFQ digest fixtures + live generator (parity pinned both sides)
- blueprint#1568: `submit_job_from_quote` event-parse fix + the live chain proof test
- sandbox-blueprint#177: operator `/api/quote` + `/api/capabilities` (liquid pricing)
- adc#8844/#8896: TS RFQ client + Rust→TS interop proof
- Outstanding debt (tracked, not blocking): full operator e2e via docker-compose; adc dependabot
  majors; blueprint #175 credentials-off-chain implementation
