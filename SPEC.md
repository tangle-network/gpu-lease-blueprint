# GPU lease D8 target

Contracts, operator and ADC share the released SDK types and lease ABI.

## Escrow and quotes

The vault is the only payout path. It holds prepaid escrow, accrues the operator's
payment per second, and refunds unused escrow on release or operator preemption.
JobsRFQ must not also pay the operator for that lease.

Pricing-engine `GetJobPrice` signs tnt-core `GpuJobQuoteDetails`. The signed data
binds the buyer, GPU catalog class, device count, duration, confidentiality and price.
The vault stores the resource commitment hash; the BSM reads that commitment when
processing the lease. The ADC client verifies the typed data with viem
`verifyTypedData`. Pricing and signing use the SDK without a second quote format.

Lease creation, extension and settlement conserve escrow. Release requires the
lessee or expiry; reap requires expiry; extension requires prepaid escrow. Allocation
follows confirmed vault state and is released if the result transaction reverts.
Only the authorized BSM or slashing contract may invoke the slashing hook.

## Devices and access

The allocator schedules operator-owned hardware and persists active leases across
restart, preserving device identity, endpoint and expiry. Inventory comes from SDK
detection, with each device bound to a signed device attestation. GPU identifiers
come from the released SDK catalog; VRAM per GPU is expressed in decimal GB.
TEE detection and confidentiality types come from the SDK.

Cloud capacity uses the SDK's allocation and termination lifecycle. Provider
adapters belong to the SDK.

Job results contain public endpoints and lease identifiers. Lease credentials stay
off-chain. Blueprint-auth supplies EIP-191 recovery and tokens scoped to a lease
with expiry; its owner predicate checks the lease owner. That predicate replaces
the local `LeaseAuth` implementation when released. Invalid configuration fails at
startup.

## Registration and consumer proof

`cargo tangle blueprint deploy` registers the generated definition through the SDK
keystore. The definition binds the manager, job schemas, metadata digest and
operator artifact.

The acceptance run starts Anvil, deploys the contracts, starts the operator, and
uses the ADC `sdk/gpu-lease` client to create, use and release a funded lease.
Lifecycle abuse cases need public chain or HTTP reproductions that fail on the
audit base and pass on the final head. Cargo, Forge and consumer gates run on that
same head before merge. Tempo deployment follows the repository deployment path
after the local stack is proven.
