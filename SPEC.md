# GPU Lease Blueprint — Supreme Spec (v1, designed to never need a redesign)

Design principle: **the contract is a fixed point of invariants; everything that evolves lives off-chain.**
On-chain: money conservation, commitment binding, slashing. Off-chain: pricing curves, inventory
claims, GPU generations, topology, scheduling policy. The only permissible future churn is dependency
updates — any needed semantic change is already represented as data.

## 1. On-chain surface (minimal, complete, final)

**Jobs** (allocation events; carry no secrets ever):
- `LEASE(uint8 intentVersion, bytes32 intentHash)` → returns `{leaseId, endpoint (public), schemaVersion}`
- `RELEASE(bytes32 leaseId)`
- `EXTEND(bytes32 leaseId, uint64 addSeconds)` (pays escrow)
- `REAP(bytes32 leaseId)` (anyone, after expiry; releases escrow to operator minus slashing)

**Lease object** (the money-bearing state; parking-meter economics):
- `{id, operator, lessee, escrowAmount, pricePerSecond, expiry, intentHash, confidentiality, state,
deviceCount}`
- Invariant: `escrow(lessee) ≥ pricePerSecond × deviceCount × remaining(lease)` at all times. EXTEND
  enforces; RELEASE refunds pro-rata atomically; REAP pays operator for elapsed time. No streaming,
  no debt, no deadbeat risk, no gas-per-block accounting. Complete money semantics:
  pay/extend/refund/slash. `deviceCount` (intent v2) is I5-immutable: a lease rents N devices of
  one class, escrow = price × duration × N, refunds and takes scale per device.

**Quoting**: existing `JobsRFQ` unchanged. The signed quote binds `requester + inputsHash +
confidentiality + price + deviceCount`. Price is per-second PER DEVICE; escrow = quote price ×
requested duration × deviceCount (both are in the hashed inputs). A non-TEE operator structurally
cannot serve a TEE-bound quote (proven: blueprint #1568).

**Slashing hook**: a LEASE that expires unfulfilled (operator never returned a leaseId) or a
RELEASE-attested violation slashes via the existing operator staking. This is the only inventory
enforcement — **there is no trusted on-chain inventory registry**. Operators advertise capability
off-chain; the signed quote is the sole commitment the chain enforces. GPU generations (H100→B200→…)
are therefore invisible to the contract forever.

## 2. Off-chain surfaces

**Operator policy** (env/config, hot-reloadable, never a contract change): GPU classes, per-class
`PRICE_BPS` multipliers, idle/full utilization curve, TEE premium — same composable policy as the
sandbox operator's `/api/quote` (ai-agent-sandbox-blueprint#177).

**Credentials**: NEVER on-chain (job results are public forever — the bug class eliminated this
week). The LEASE result carries `{leaseId, publicEndpoint, schemaVersion}` only. Credentials
(temporary, scoped, revocable) are delivered via the operator's **EIP-191 challenge-session API**
(existing, proven). Endpoint rotation on extend is allowed; leaseId is the stable handle.

**Scheduler**: buyer-driven multi-quote (natively supported: `quotes[]`). Client collects N quotes,
validates all (expiry/buyer/inputs/signature — proven client), submits with the chosen one. Failover
= resubmit with the next unused quote. No chain-side scheduler to redesign.

**Placement**: v1 co-located (operator hosts sandbox + GPU on one box; zero network-CUDA latency
problem). The result's `endpoint` field is a versioned discriminated union — `v1 = local-attach`,
later `v2 = network-cuda`, `v3 = …` — so new attach transports are schema versions, not redesigns.

## 3. Schema discipline (the never-update guarantee)

Every result and intent carries `schemaVersion`. Unknown-version readers fail closed. New GPU
capabilities, regions, attestation formats = new *versions of off-chain data*, not contract changes.

## 4. What is deliberately absent (and why that's permanent)

- No inventory registry (griefing vector; quotes + slashing suffice)
- No on-chain scheduling (buyer-driven multi-quote is the decentralized scheduler)
- No streaming payments (escrow conserves the same invariants at 1/100th the gas)
- No credential transport on-chain (public-by-construction)
- No GPU-class enum on-chain (generational churn is data, not code)

## 5. Integration

Sandbox SDK `gpuLease` resolves: create sandbox → collect+validate GPU quotes → LEASE (escrowed) →
attach via endpoint → RELEASE/REAP on teardown with pro-rata refund. The existing
`/api/capabilities` `gpu: true` flag is the discovery bit.

## 6. Build order (each shippable)

1. Contract: lease object + 4 jobs + escrow math + slashing hook (auditable in one sitting)
2. Operator: allocator (co-located device passthrough) + quote policy + session-auth credential path
3. SDK: gpuLease resolver + multi-quote failover client
4. External audit of the contract before mainnet money (non-negotiable)
