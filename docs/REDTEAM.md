# Red-Team Analysis: GPU Lease Protocol

Status: active analysis. Every vector rated by severity × likelihood × current mitigation.
Conclusion: the vault's invariants are sound (money can't be stolen), but service-level
fraud needs a unified slashing framework — which is the blueprint for all compute
marketplaces, not just GPUs.

## Part 1: Operator Attacks

### 1.1 Inventory Fraud — "I have 4 H100s" (has 0)

**Vector**: Operator advertises inventory in `GPU_INVENTORY_JSON` that doesn't exist.
Quotes are signed, escrows are accepted, devices are "allocated."

**Current mitigation**: NONE on-chain. The vault conserves the escrow (I1) — the user
can release and get a full refund minus elapsed time.

**Impact**: User wastes gas + time. Operator earns nothing (no service was rendered).
**Severity**: LOW (annoyance, no value loss)
**Slashable?**: YES — condition: "accepted a lease but returned a result referencing
a device that cannot be verified." Evidence: job result claims device X; user attests
device X doesn't exist or is unreachable.

### 1.2 Class Substitution — "You paid for H100, here's an A100"

**Vector**: Operator accepts an H100 quote but provisions an A100. The on-chain record
says "h100" (I5 binding holds). The actual GPU is different.

**Current mitigation**: NONE on-chain. The protocol cannot verify GPU identity.
The credential session gives access to *a* GPU, not *the* GPU.

**Impact**: User pays H100 price (~$0.30/hr) for A100 performance (~$0.15/hr).
Over a 1-hour lease: ~$0.15 stolen. Over thousands of leases: significant.
**Severity**: HIGH (value theft, scalable)
**Slashable?**: YES — condition: "the device served does not match the class quoted."
Evidence: user runs `nvidia-smi` and submits the signed output. The operator's
credential session has the leaseId; the GPU's `nvidia-smi` output includes the
device UUID which should match the operator's inventory.

**Mitigation path**: The credential challenge message should include the expected
GPU class and CUDA device UUID. The GPU's TEE (in CC mode) can attest to its own
identity. This ties into the TEE attestation work already started.

### 1.3 Service Degradation — "It's an H100 but I'm throttling it to 50%"

**Vector**: Operator provides the right GPU class but limits clock speed, shares
the GPU across multiple leases, or constrains memory.

**Current mitigation**: NONE. No on-chain performance verification.
**Impact**: Proportional value theft.
**Severity**: MEDIUM (hard to prove, moderate impact)
**Slashable?**: PARTIALLY — condition: "measured performance is below X% of class
baseline." Evidence: user submits benchmark results signed within the TEE.
Difficulty: defining a fair baseline. This is the HARDEST slashing condition.

### 1.4 Credential Overstay — "I'll just... not revoke the credentials"

**Vector**: Operator fails to revoke the user's GPU access at escrow exhaustion.
The user keeps using the GPU for free.

**Current mitigation**: The credential expires at `lease.expiry`. The reaper sweep
(default 30s) frees devices. But if the operator doesn't run the sweep, credentials
remain valid.

**Impact**: Operator provides free service. The USER doesn't lose money — the
operator does. This is actually an attack on the PROTOCOL's economic model, not
on the user.
**Severity**: LOW for users, MEDIUM for protocol economics
**Slashable?**: YES — condition: "credential was used after lease expiry."
Evidence: access logs with timestamps > expiry. The operator is liable because
credential lifecycle is their responsibility.

### 1.5 False Job Results — "I allocated a device" (didn't)

**Vector**: Operator's LEASE job handler returns a result with a leaseId and
endpoint, but never actually allocated anything.

**Current mitigation**: The BSM's `_bindLease` verifies the vault lease is Live
and matches the intent. But it CANNOT verify that a real device was allocated.
The operator's own allocator state is off-chain.

**Impact**: User sees a fake "active lease" but can't use a GPU.
Escrow is safe — user can release for full refund.
**Severity**: LOW (same as 1.1 — annoyance)
**Slashable?**: YES — same condition as 1.1.

### 1.6 Front-Running Quotes

**Vector**: Operator sees an incoming `POST /api/quote`, adjusts their price
upward before the transaction lands.

**Current mitigation**: The quote is signed with `validUntil`. The price in the
signed quote is the binding price. If the operator changes the price after
signing, the signature verification fails.

**Impact**: Minimal — the signed quote is immutable once issued.
**Severity**: LOW (mitigated by signature binding)
**Slashable?**: Not needed (structurally prevented).

### 1.7 Blocking Release

**Vector**: Operator somehow prevents the user from releasing.

**Current mitigation**: `vault.release()` is a direct contract call from the
lessee. The operator is NOT in the path. The refund goes directly to the lessee
via `transfer()`. The operator cannot intercept, block, or modify this.

**Impact**: NONE — structurally impossible.
**Severity**: NONE (structurally prevented by I3/I4).
**Slashable?**: Not needed (impossible).

## Part 2: User (Lessee) Attacks

### 2.1 Overstay — "I'll keep using the GPU past expiry"

**Vector**: User keeps credentials active past escrow exhaustion.

**Current mitigation**: The credential token has `expires_at == lease.expiry`.
The operator's reaper sweep frees the device. The vault has no more escrow to
pay for additional time.

**Impact**: Operator provides unpaid service for up to the sweep interval (30s
default). Bounded: the device is freed automatically.
**Severity**: LOW (bounded by sweep interval, ~30 seconds of free GPU time)
**Slashable?**: NO — this is the operator's responsibility, not a user violation.
The user doesn't control the credential expiry.

### 2.2 Escrow Spam — "I'll create thousands of tiny leases"

**Vector**: User creates many short-lived leases to grief the operator.

**Current mitigation**: Each `vault.create()` costs gas. The operator gets paid
for elapsed time (even if very short). The vault conserves escrow exactly (I1).

**Impact**: User wastes gas. Operator gets paid for the gas-cost-equivalent of
GPU time. Not profitable for the attacker.
**Severity**: NONE (economically unprofitable for the attacker)

### 2.3 Early Release Abuse — "I'll release right before expiry to maximize refund"

**Vector**: User releases at the last second to get near-full refund.

**Current mitigation**: This is the PROTOCOL WORKING CORRECTLY. The pro-rata
refund is exact: `refund = price × remaining`. If the user used 599 of 600
seconds, they get 1 second's worth back. The operator earned 599 seconds.

**Severity**: NONE (this is the product, not an attack)

## Part 3: Platform/Broker Attacks

### 3.1 Broker Pocketing Refunds

**Vector**: The broker (USD rail) receives the user's pro-rata refund but doesn't
credit the user's platform balance.

**Current mitigation**: The `LeaseReleased(refund, operatorTake)` event is on-chain.
The user can verify. But there's no automatic enforcement.

**Impact**: User loses their refund.
**Severity**: HIGH (if broker is malicious)
**Slashable?**: Not via operator slashing — this is a PLATFORM trust issue.
Mitigation: the receipt event IS the reconciliation record (the chain can't be
lied to, only the broker's off-chain crediting can). Periodic reconciliation
(the `charge_intents` pattern) catches discrepancies.

### 3.2 Broker Charging Without Leasing

**Vector**: Broker charges the user's card but never creates a lease.

**Current mitigation**: NONE on-chain (this is entirely off-chain trust).
**Impact**: User pays for nothing.
**Severity**: HIGH
**Mitigation**: The user can verify on-chain that a lease exists for the amount
they were charged. The `LeaseCreated` event is the proof of purchase.

## Part 4: Protocol-Level Attacks

### 4.1 Intent Hash Collision

**Vector**: Two different intents hash to the same `intentHash`.

**Current mitigation**: keccak256 collision resistance. The canonical intent
string includes every field. Finding a collision is computationally infeasible.
**Severity**: NONE (cryptographically prevented)

### 4.2 Replay Attack — "I'll reuse a quote from yesterday"

**Vector**: User submits a stale quote.

**Current mitigation**: Quotes have `validUntil` (60s default). The quote validation
checks expiry. The vault's `create()` verifies the current block timestamp.
**Severity**: NONE (structurally prevented)

### 4.3 Race Condition — "Two leases for the same device simultaneously"

**Vector**: Two users lease simultaneously for the same device.

**Current mitigation**: The allocator uses a Mutex. Only one lease can bind to a
device at a time. The second request gets `NoDeviceAvailable`.
On-chain: the vault allows multiple leases (they're independent escrows). The
operator's allocator is the serialization point.
**Severity**: NONE (allocator enforces exclusivity)

---

## Part 5: Unified Slashing Framework

### The insight: slashing conditions are shared across ALL compute blueprints

Every compute marketplace (GPUs, TPUs, inference, storage) faces the same
service-level trust problem:

> **The operator claimed to provide X. Did they actually provide X?**

This decomposes into four checkable conditions:

| Condition | What it proves | Evidence source | Applies to |
|---|---|---|---|
| **Delivery** | Service was actually provided | Benchmark/attestation from within the service | All compute |
| **Integrity** | The service matches what was quoted | Device identity + class verification | All hardware |
| **Liveness** | The service was available for the paid duration | Uptime/heartbeat logs | All services |
| **Economy** | The operator's economic behavior was honest | On-chain reconciliation | All marketplaces |

### Proposed slashing types

```solidity
enum SlashingType {
    /// Operator accepted a lease but didn't provide the service.
    SERVICE_NOT_DELIVERED,

    /// Operator provided a different service than quoted (wrong GPU class,
    /// wrong model, degraded performance).
    SERVICE_MISMATCH,

    /// Operator's service was unavailable during the paid period.
    SERVICE_UNAVAILABLE,

    /// Operator's attestation doesn't match their claims (TEE fraud).
    ATTESTATION_INVALID,

    /// Operator didn't maintain the service lifecycle (credentials not
    /// revoked, devices not freed).
    LIFECYCLE_VIOLATION
}
```

### Evidence standards

Each slashing type has a standard evidence format:

```solidity
struct SlashingEvidence {
    SlashingType evidenceType;
    bytes32 leaseId;         // links to the vault lease
    address operator;        // who is accused
    address accuser;         // who submits the evidence
    bytes evidenceData;      // type-specific (see below)
    uint256 submittedAt;
    uint256 challengeDeadline;
}
```

**SERVICE_NOT_DELIVERED**: The accuser submits a zero-knowledge proof or a
signed attestation that they attempted to use the service during the lease
period and it was not available. For GPU leases: a signed `nvidia-smi` output
from within the TEE showing device unreachable.

**SERVICE_MISMATCH**: The accuser submits evidence that the provided service
differs from the quoted class. For GPU leases: `nvidia-smi` output showing a
different device than expected. The operator's inventory (which they signed)
lists device UUIDs; the actual device UUID must match.

**SERVICE_UNAVAILABLE**: Uptime logs showing the service was unreachable for
a significant portion of the paid period. This is the "SLA violation" case.

**ATTESTATION_INVALID**: The TEE report doesn't match the claimed TEE type,
or the attestation is stale/revoked. This ties into the `tee_nonce` work.

**LIFECYCLE_VIOLATION**: Access logs showing credential use after expiry,
or device allocation state that doesn't match the vault state.

### Severity matrix

| Violation | First offense | Repeat | Severity bps |
|---|---|---|---|
| SERVICE_NOT_DELIVERED | Warning | 5% slash | 500 |
| SERVICE_MISMATCH | 10% slash | 25% slash | 1000-2500 |
| SERVICE_UNAVAILABLE | Pro-rated refund | 2x refund | varies |
| ATTESTATION_INVALID | 50% slash | 100% slash (eject) | 5000-10000 |
| LIFECYCLE_VIOLATION | Warning | 1% slash | 100 |

### Challenge period

Every slashing submission has a challenge window (default: 7 days). During this
period, the operator can submit counter-evidence:

- For SERVICE_NOT_DELIVERED: "The service was available; here are my access logs."
- For SERVICE_MISMATCH: "The user is lying; here's the actual device allocation."
- For ATTESTATION_INVALID: "The attestation was valid at the time; here's a fresh one."

After the challenge period, if no valid counter-evidence is submitted, the slash
executes automatically via the operator's staking position.

### Implementation path

1. **v1 (this repo)**: The slashing hook exists in `GpuLeaseVault.sol` — it
   emits `OperatorSlashed(operator, leaseId, amount)`. tnt-core's operator
   staking system picks this up and deducts from the operator's stake.

2. **v2 (unified)**: A `SlashingRegistry` contract shared across all compute
   blueprints. Each blueprint registers its evidence verifiers. The registry
   manages the challenge period, severity calculation, and execution.

3. **v3 (automated)**: Heartbeat-based liveness proofs — operators periodically
   submit attestations that their services are running. Missing heartbeats
   trigger automatic slashing without user accusation.

### What the vault already prevents (no slashing needed)

- **Money theft**: I1 (conservation) makes it impossible for the operator to
  take more than the elapsed time's worth. The exact pro-rata refund (I3)
  is enforced by the contract, not by the operator's honesty.

- **Double-settlement**: I4 makes it impossible to settle twice. The operator
  gets paid exactly once per lease.

- **Quote manipulation**: I5 makes it impossible to change the terms after
  the fact. The intentHash, price, and operator are immutable.

These are the strong guarantees. Slashing handles what the contract CAN'T
verify: whether the physical service matched the on-chain promise.

---

## Part 6: What we should build NOW

The most impactful slashing condition for v1:

**SERVICE_MISMATCH** (wrong GPU class) — because:
1. It's the most likely attack (easy to do, profitable, hard to catch)
2. It has clear evidence (`nvidia-smi` output with device UUID)
3. It generalizes to all compute (wrong model, wrong storage tier, etc.)
4. The TEE attestation work already started (`tee_nonce`) is the verification path

Implementation: the credential session challenge already includes the GPU class.
When the user connects to the GPU, the operator's service can verify that the
connected device matches the quoted class. If not, the user has evidence for
slashing. With TEE GPUs (H100 CC mode), the device itself can attest to its
own identity — making the evidence cryptographically verifiable.
