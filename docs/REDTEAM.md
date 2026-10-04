# Red-Team Analysis — GPU Lease Protocol v2

**Reviewer mindset**: I have $10M, a team of 5, and 3 months. I want to extract
value from this protocol without providing equivalent service. I will find and
exploit every gap. I will social-engineer, collude, bribe, and use MEV.

**Previous analysis was incomplete.** It covered service-level fraud but missed
economic attacks, cross-contract interactions, flash-loan vectors, governance
capture, and several smart-contract edge cases. This document is the real audit.

---

## A. Smart Contract-Level Attacks

### A.1 Reentrancy in `release()`

**Attack**: Lessee's fallback function reenters `release()` before state is committed.

```
release(leaseId)
  → _settle(l)           // state→Released, escrow→0, earnings += take
  → emit LeaseReleased
  → _refund(lessee, refund)  // ← ATTACKER'S FALLBACK HERE
    → attacker calls release(leaseId) again
    → state != 0 → NotLive() revert
```

**Verdict**: SAFE. State is committed before transfer (checks-effects-interactions).
The second `release()` call hits the `state != 0` guard.

BUT — what about reentrancy in `withdrawEarnings()`?

```
withdrawEarnings()
  → operatorEarningsOf[msg.sender] = 0
  → operatorEarnings -= amount
  → _refund(msg.sender, amount)
    → attacker reenters withdrawEarnings()
    → operatorEarningsOf[msg.sender] is already 0 → NothingToWithdraw() revert
```

**Verdict**: SAFE. Balance zeroed before transfer.

### A.2 Integer Overflow in Pro-Rata Math

**Attack**: Engineer a lease where `price * remaining` overflows uint128.

In `_settle()`:
```solidity
refund = uint128(uint256(l.pricePerSecond) * remaining);
```

If `pricePerSecond = 2^127` and `remaining = 2`, the product is `2^128` which
wraps to 0 on the uint128 cast — **the user gets ZERO refund**.

**Is this reachable?** The vault constrains:
- `create()`: `cost = price * duration`, checked `cost <= uint128.max`
- `extend()`: `newEscrow = escrow + cost`, checked `newEscrow <= uint128.max`

So `escrow <= uint128.max` always. And since `refund <= escrow` (by the I2
invariant `price * remaining <= escrow`), the refund can't overflow either.

BUT: the INVARIANT `price * remaining <= escrow` relies on `remaining <= paidSeconds`.
Can this be broken?

`remaining = expiry - now`. `paidSeconds = escrow / price`. If a user could make
`remaining > paidSeconds`, they'd get `refund > escrow` — stealing from the
operator's take.

**Can remaining exceed paidSeconds?**
- At create: `remaining = duration`, `paidSeconds = duration`. Equal. ✓
- After warp(t): `remaining = duration - t`, `paidSeconds = duration`. Less. ✓
- After extend(+d): `remaining = duration - t + d`, `paidSeconds = duration + d`.
  Since `t >= 0`: `remaining = paidSeconds - t <= paidSeconds`. ✓

**Verdict**: SAFE. The invariant holds under all reachable states. The fuzz
suite (1,000 runs × multi-extend) confirms.

### A.3 Intent Hash Collision / Second Preimage

**Attack**: Find two different intents that hash to the same `intentHash`,
allowing one quote to be used for a different purpose.

The canonical intent is:
```
gpu-lease-intent|v{version}|{class}|{duration}|{confidentiality}|{region}|{sandboxId}|{teeType}
```

**Verdict**: SAFE. keccak256 preimage resistance. Finding any collision is
computationally infeasible. The delimiter `|` prevents field-boundary attacks
(a class of "ab" + region "c" ≠ class of "a" + region of "bc" because the
pipe disambiguates).

Wait — is the delimiter actually sufficient? What if `gpuClass` contains a `|`?
E.g., class `"a|b"` with duration `3600` vs class `"a"` with duration... no,
the format is always `class|duration` so a `|` in the class would shift all
subsequent fields. But both the operator and the client compute the same hash
from the same structured fields, so they'd agree on the interpretation. An
attacker can't exploit this because the hash is computed by both sides
independently from the structured data, not from the string.

**Verdict**: SAFE with the caveat that gpuClass should be validated to not
contain `|`. Add input validation.

### A.4 Front-Running leaseId Prediction

**Attack**: Predict the leaseId (keccak of known parameters) and front-run
the victim's `create()`.

leaseId = keccak(operator, lessee, intentHash, block.timestamp, totalEscrowed, msg.value)

An attacker would need to predict `block.timestamp` and `totalEscrowed` at
inclusion time. This is possible with high probability on a low-traffic chain
(where timestamp is predictable and totalEscrowed changes rarely).

**Impact**: The attacker creates a lease with the SAME leaseId first. The
victim's `create()` then computes a different leaseId (because totalEscrowed
changed), so the victim isn't affected — they just get a different leaseId.
The attacker has a useless lease.

**Verdict**: LOW RISK. No value extraction possible.

### A.5 Flash Loan Attack on totalEscrowed

**Attack**: Flash-borrow ETH → create a massive lease (bumping totalEscrowed)
→ victim's leaseId derivation changes → attacker somehow profits?

The totalEscrowed is only used as a nonce for leaseId uniqueness. Changing it
doesn't affect pricing, refunds, or any economic calculation.

**Verdict**: NO IMPACT. totalEscrowed is a counter, not a price feed.

### A.6 Signature Malleability on RFQ Quotes

**Attack**: Take a valid (r, s, v) signature and produce (r, n-s, v') which
recovers to the same address. Submit both to confuse the system.

In secp256k1, for every (r, s, v) there is (r, n-s, 1-v) that recovers the
same public key. This means a signature can be "flipped."

**Impact**: None. Both signatures recover to the SAME operator address. The
verification passes for both, and the quote terms are identical. There's no
way to use malleability to impersonate a different operator.

**Verdict**: SAFE. Malleability exists but is harmless in this context.

### A.7 DoS via Block Gas Limit

**Attack**: Create so many leases that `executeSlash()` or other iteration
functions run out of gas.

`executeSlash()` operates on a single claim — O(1). No unbounded iteration
exists in the vault. The `slashingClaimIds` array grows but is never iterated
on-chain.

**Verdict**: SAFE. No unbounded loops.

### A.8 Precision Loss in Pro-Rata Refund

**Attack**: Engineer a price/duration combination where the integer division
loses wei, accumulating dust that can be extracted.

The refund formula: `refund = price * remaining`. This is exact integer
multiplication. There's NO division in the refund calculation. The
"division" in `escrow / price = paidSeconds` is only used conceptually — the
actual refund calculation uses only multiplication and subtraction.

**Verdict**: SAFE. Exact integer math, no precision loss.

---

## B. Economic Attacks

### B.1 Operator Cartel — Price Coordination

**Attack**: N operators collude to raise prices uniformly. Users have no
alternative because all operators raise simultaneously.

**Mitigation**:
1. Cloud providers (runpod, vast-ai, lambda-labs) provide a price ceiling.
   If operator prices exceed cloud prices, users switch to the USD rail.
2. The broker (USD rail) sources from the cheapest operator — cartel members
   are undercut by any non-cartel operator.
3. New operators can join permissionlessly (no on-chain inventory registry),
   competing on price immediately.

**Severity**: MEDIUM (temporarily effective until competitive pressure breaks it)
**Slashable?**: No — this is market behavior, not fraud. The protocol's
decentralization IS the mitigation.

### B.2 Quote Arbitrage — Stale Quote Exploitation

**Attack**: Monitor the mempool for `POST /api/quote` responses. If a quote
is about to become stale (near validUntil), submit a transaction that uses it
before it expires, while simultaneously front-running on the cloud provider
to lock in a lower price.

**Impact**: Minimal. The quote is signed and the price is binding. The
arbitrageur can't change the price. They're just racing to use a valid quote.

**Severity**: LOW (standard MEV, not protocol-specific)

### B.3 Operator Undercuts Then Rug-Pulls

**Attack**: Operator offers very low prices to attract users, then:
(a) provides terrible service (throttled GPUs), or
(b) goes offline after collecting escrow, or
(c) provides wrong GPU class.

**Mitigation**: This is exactly what SERVICE_MISMATCH and SERVICE_NOT_DELIVERED
slashing covers. The operator's stake is at risk. The economics:
- Operator stake: e.g., $10,000
- Slashing for SERVICE_MISMATCH: 25% = $2,500 per incident
- Value extractable per lease: ~$0.15 (class substitution)
- Breakeven: ~16,667 successful frauds before one slash makes it unprofitable

If slashing detection rate > 0.006%, the attack is unprofitable.

**Severity**: MEDIUM (depends on detection rate — user education needed)
**Slashable?**: YES — SERVICE_MISMATCH is the primary deterrent.

### B.4 Broker Race — Front-Running the USD Rail

**Attack**: See a Stripe payment in the broker's queue, front-run the lease
creation to claim the GPU the broker is about to lease.

**Impact**: The broker gets a different GPU (or none). The user paid but
didn't get the specific GPU they were promised.

**Mitigation**: The broker should use a private mempool or submit immediately
upon Stripe confirmation. This is an operational concern, not a protocol flaw.

**Severity**: LOW (operational)

### B.5 Circular Leasing — Self-Dealing

**Attack**: Operator leases their own GPU through the protocol to:
(a) inflate utilization metrics (making their GPUs look scarce → higher prices)
(b) wash-trade volume to look like a popular operator
(c) test the slashing system with a controlled "violation"

**Impact**: The escrow is conserved (I1). The operator pays themselves. No
value is stolen. The utilization manipulation could temporarily raise prices
for other users.

**Severity**: LOW (market manipulation, not theft — similar to wash trading
on any marketplace)
**Slashable?**: Potentially — if the operator's utilization data is
verifiably wrong, it's a form of SERVICE_MISMATCH.

---

## C. Cross-Contract Attacks

### C.1 BSM `_bindLease` — Impersonation

**Attack**: Malicious operator submits a result with a leaseId that belongs
to a different operator's lease.

**Mitigation**: `_bindLease()` checks `vOperator != operator` → `OperatorMismatch()`.
The vault's lease record names the operator; the result must come from that
operator.

**Verdict**: SAFE.

### C.2 BSM Input Hash Manipulation

**Attack**: Operator modifies the cached inputs between `onJobCall` and
`onJobResult` to bind to a different intent.

**Mitigation**: `_consumeInputs()` verifies `keccak256(inputs) != inputsHash` →
`IntentMismatch()`. The hash pins the inputs.

**Verdict**: SAFE.

### C.3 Vault ↔ tnt-core — Job Payment Manipulation

**Attack**: Someone submits a job with excessive `msg.value` to tnt-core,
which forwards it to the BSM's `onJobCall` which is `payable`.

**Impact**: The BSM receives ETH it didn't expect. This is a donation, not
an attack. The BSM doesn't use msg.value for anything.

**Verdict**: SAFE.

### C.4 Operator API — Cross-Origin Attack

**Attack**: A malicious website makes cross-origin requests to the operator
API to obtain quotes or manipulate sessions.

**Mitigation**: CORS is permissive (Any) — but the API only serves read-only
data (capabilities, quotes). Session endpoints require the lessee's signature.
A malicious website can read quotes but can't create sessions or leases.

**Severity**: LOW (quotes are public data by design)
**NOTE**: We should consider restricting CORS on session endpoints specifically,
since the challenge-response flow could theoretically be used by a phishing site.

---

## D. Governance & Meta-Protocol Attacks

### D.1 Slashing Governance Capture

**Attack**: Attacker gains control of tnt-core governance and:
(a) rejects all slashing claims
(b) reduces all severity caps to 0
(c) removes specific operators from the slashable set

**Impact**: Slashing becomes ineffective. Operators can defraud with impunity.

**Mitigation**: This is a tnt-core governance issue, not specific to this
protocol. The vault's invariants (I1-I5) still prevent direct money theft.
The slashing is a SERVICE-LEVEL deterrent, not the primary security mechanism.

**Severity**: HIGH if governance is captured (but this affects ALL blueprints,
not just this one)
**Mitigation path**: Decentralized governance (multi-sig, on-chain voting,
timelock). This is tnt-core's responsibility.

### D.2 BSM Upgrade Attack

**Attack**: The BSM contract is upgradeable and a malicious upgrade changes
the result-binding logic.

**Mitigation**: The BSM in this repo is NOT upgradeable (no proxy pattern).
If an upgradeable version is deployed later, this becomes a risk.

**Severity**: N/A for current deployment. HIGH if proxy is added without
timelock + multi-sig.

### D.3 Canonical Metadata Replacement

**Attack**: Replace the `metadataUri` content while keeping the same
`metadataHash` (impossible without a keccak collision), or change the
`metadataUri` to point to different content.

**Mitigation**: `metadataHash` pins the content. Any change to the metadata
JSON changes the hash, which would fail the driver's verification.

**Verdict**: SAFE (hash-pinned).

---

## E. Infrastructure Attacks (Off-Chain)

### E.1 Operator API DDoS

**Attack**: Flood the operator's HTTP API with quote requests.

**Impact**: Legitimate users can't get quotes. No on-chain impact.

**Mitigation**: Rate limiting, CDN caching of capabilities, standard DDoS
protection. This is an operational concern.

**Severity**: LOW (availability, not integrity)

### E.2 Credential Session Hijacking

**Attack**: Steal a scoped credential token and use it to access the GPU.

**Mitigation**: Tokens are scoped to a specific leaseId and expire at lease
expiry. A stolen token gives access only to that specific GPU for the
remaining lease duration. The economic damage is bounded by the escrow.

**Severity**: LOW (bounded by escrow — the thief gets GPU time that was
already paid for by the victim)

### E.3 Quote Server Compromise

**Attack**: Compromise the operator's key that signs quotes.

**Impact**: Attacker can sign arbitrary quotes, potentially:
(a) offering unrealistically low prices (attracting users to a rug-pull)
(b) impersonating the operator for other blueprints

**Mitigation**: The signing key is separate from the operator's staking key.
Compromising the quote key doesn't give access to stake or earnings. The
operator can rotate the quote key (`GPU_QUOTE_SIGNING_KEY` is env-based,
hot-reloadable).

**Severity**: MEDIUM (temporary — operator rotates key, old quotes expire
within 60 seconds)

---

## F. What I Missed in v1

| Gap | Why it matters | Fixed in v2? |
|---|---|---|
| Flash loan analysis | Standard DeFi attack vector | ✓ (A.5 — no impact) |
| Signature malleability | Common ECDSA pitfall | ✓ (A.6 — harmless here) |
| Integer precision edge cases | Common source of theft | ✓ (A.2, A.8 — exact math) |
| Governance capture | The "who guards the guards" problem | ✓ (D.1 — tnt-core responsibility) |
| Operator collusion economics | Market manipulation | ✓ (B.1 — cloud providers as ceiling) |
| Circular self-dealing | Wash trading | ✓ (B.5 — no value stolen) |
| Cross-contract reentrancy | BSM ↔ vault interaction | ✓ (C.1-C.3 — checks are correct) |
| Credential session theft | Off-chain but protocol-relevant | ✓ (E.2 — bounded by escrow) |
| Quote key compromise | Operational but economic | ✓ (E.3 — rotatable, 60s expiry) |
| BSM upgrade risk | Future-proofing | ✓ (D.2 — no proxy, noted as risk) |
| gpuClass validation | Input sanitization | ⚠ ADD: reject `\|` in class names |

---

## G. Severity Summary (Updated)

| Vector | Severity | Likelihood | Current | Gap |
|---|---|---|---|---|
| Wrong GPU class (SERVICE_MISMATCH) | **HIGH** | HIGH | Slashable 25% | Needs on-chain evidence verification |
| No service (NOT_DELIVERED) | LOW | MEDIUM | Slashable 5% | Low stakes |
| TEE fraud (ATTESTATION_INVALID) | HIGH | LOW | Slashable 100% | Needs TEE integration |
| Credential overstay | MEDIUM | LOW | Operator loss | Bounded by sweep |
| Reentrancy | NONE | N/A | Prevented | — |
| Overflow | NONE | N/A | Prevented | — |
| Quote malleability | NONE | N/A | Harmless | — |
| Governance capture | HIGH | LOW | External | tnt-core |
| DDoS | LOW | HIGH | Operational | Rate limiting |

**The ONE attack that matters most**: SERVICE_MISMATCH (wrong GPU class).
It's the most profitable, most scalable, and hardest to detect without
TEE-based device identity attestation. This is where the TEE work pays off.

---

## H. The Reusable Slashing Package

Yes — a standalone package. The conditions are universal across all compute
blueprints. Here's the architecture:

```
packages/slashing/
├── ISlashingRegistry.sol    — the interface every blueprint imports
├── SlashingTypes.sol        — shared types (5 violation types, evidence format)
├── SlashingRegistry.sol     — the core: submit → challenge → execute
├── IEvidenceVerifier.sol    — per-blueprint evidence verification
└── SeverityPolicy.sol       — configurable severity caps
```

**Design principle**: the registry is GENERIC (any blueprint), the verifiers
are SPECIFIC (each blueprint provides its own).

```solidity
// Any compute blueprint:
contract MyComputeBlueprint {
    ISlashingRegistry public slashing;

    function onResult(...) external {
        // ... normal result handling ...
    }

    // A user accuses the operator:
    function accuse(
        bytes32 serviceId,
        SlashingType violation,
        bytes calldata evidence
    ) external {
        slashing.submit(serviceId, violation, evidence, msg.sender);
    }
}

// The slashing registry:
contract SlashingRegistry is ISlashingRegistry {
    // Verifiers are registered per blueprint:
    mapping(uint64 => IEvidenceVerifier) public verifiers;

    function submit(...) external returns (bytes32 claimId) {
        // Record the claim
        // Start the challenge period
        // Emit event for the operator
    }

    function counter(bytes32 claimId, bytes calldata evidence) external {
        // Only the accused operator
        // Within the challenge period
        // Mark as countered → requires governance review
    }

    function execute(bytes32 claimId) external {
        // After challenge period, if uncountered
        // Verify evidence via the blueprint's verifier
        // Emit OperatorSlashed event → tnt-core staking picks up
    }
}
```

The evidence verifier is blueprint-specific:

```solidity
interface IEvidenceVerifier {
    /// @return valid Whether the evidence supports the accusation
    /// @return severityBps Adjusted severity (verifier can recommend)
    function verify(
        SlashingType violation,
        bytes calldata evidence,
        bytes32 serviceId
    ) external view returns (bool valid, uint256 severityBps);
}
```

For GPU leases, the verifier checks `nvidia-smi` output, TEE reports, and
benchmark results. For inference blueprints, it checks model output hashes.
For storage, it checks data availability proofs.

**This is the right abstraction**: the CONDITIONS are universal, the EVIDENCE
is domain-specific, and the ENFORCEMENT is centralized (one registry, one
staking integration).
