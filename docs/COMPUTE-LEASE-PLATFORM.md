# Compute blueprints in the TangleDriver — no protocol changes

**Constraint (agreed):** the protocol changes only when something is *provably
impossible* today. Nothing below requires a protocol change. Everything uses
fields and views that **already exist in tnt-core 0.19** — most are simply
unpopulated today.

## Source of truth — the on-chain definition, nothing beside it

`createBlueprint` already persists everything a driver needs, and 0.19 exposes
read-only views for all of it (verified live on our anvil E2E chain):

```text
driver spike: blueprintCount=1 name="Test Blueprint"
              metadataUri="http://localhost:3333" hash=0x93b26ff9...
```

| On-chain (already exists) | View (already exists) | Driver use |
|---|---|---|
| `JobDefinition{name, description, metadataUri, paramsSchema, resultSchema}` | `getBlueprintDefinition(id)` | render job forms; ABI field descriptors |
| `BlueprintMetadata{name, category, ...}` + `metadataUri` + `metadataHash` | `blueprintMetadata(id)` | nav, icons, category semantics |
| `BlueprintConfig{membership, pricing, rates}` | `getBlueprintConfig(id)` | cost display |
| definition integrity | `blueprintDefinitionHash(id)` | tamper check |

**The rule:** a blueprint ships `sol!` types (Rust) → the register script
auto-derives the canonical JSON (jobs, ABI fields, units) → publishes it at
`metadataUri` and pins `metadataHash` on-chain. The TS-side hand-written
`BlueprintDefinition` files die. Drift is impossible *by construction* — the
chain pins the hash; `gen:abi`'s staleness check becomes unnecessary.

Rich payload lives at the URI (gas-cheap); on-chain stays lean. If a chain has
gas headroom, `paramsSchema`/`resultSchema` bytes can also be populated —
optional, same data, still no protocol change.

## Generic handling lives in the TangleDriver / tangle package

The driver resolves any blueprint from chain state + hash-verified fetch and
renders it. For the compute zoo (sandboxes, tee-instances, multi-instance
clouds, GPUs, future TPUs) it recognizes **conventions, not schemas**:

- `metadata.category == "Compute"` + job names `lease` / `release` / `extend` /
  `reap` ⇒ parking-meter UX (countdown, feed-the-meter, refund receipt) keyed
  off the vault's public getters (chain clock, never `Date.now()`)
- Job names are already on-chain — a TPU blueprint that registers the same
  names gets the same UX for free. **No side registry.**
- Operator endpoints are **probed, never registered**: driver probes
  `/api/capabilities`; absent ⇒ degrades to plain job forms. Failure is
  graceful, never a registration requirement.

## What each blueprint still owns (small, local, no coordination)

1. Its `sol!` types — the single ABI origin (already true today)
2. Its register script pointing `metadataUri` at the generated JSON + hash
3. Its operator HTTP: capabilities/quote (read-only) + EIP-191 credential
   sessions (credentials are *never* on-chain — public-by-construction is the
   eliminated bug class)
4. The conformance kit green: `run-e2e.sh`-style full lifecycle (escrow →
   lease → extend → exact-refund → reap, I1 asserted on-chain per step)

## Escrow kernel reuse

`GpuLeaseVault` contains zero GPU-specific code — it is `pricePerSecond ×
seconds` with invariants I1–I5 pinned by 35 tests + 1k-run fuzz. Any metered
compute resource reuses it unchanged (rename at deploy time if desired). The
BSM result-binding pattern (`GpuLeaseBlueprint.sol`) is likewise
resource-agnostic. **Do not** fork these per resource class — deploy the same
kernel, register different blueprints.

## If a protocol change ever becomes "explicitly needed"

The bar: demonstrably impossible with existing fields/views, and the fix is
backwards-compatible for unknown-version readers (fail closed). Candidates
none today. Anything proposed must come with the failed attempt using what
exists.
