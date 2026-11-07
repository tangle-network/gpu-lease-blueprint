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

The allocator persists leases across restart, preserving device identity, endpoint
and expiry. `GPU_INVENTORY_JSON` is UNVERIFIED operator input. Signed device
attestation awaits Drew's trust-root decision; no GPU verifier or signer is
configured. TEE types come from the SDK.
GPU class ids and decimal GB per GPU follow the released SDK catalog.

Cloud capacity uses the SDK's allocation and termination lifecycle. Provider
adapters belong to the SDK.

Job results contain public endpoints and lease identifiers; credentials stay
off-chain. Access uses blueprint-auth's domain-bound owner sessions, with funded
lease ownership and expiry checked on every access. Maintained SDK publication
and a companion client validator are adoption preconditions; a local dependency
patch proves source integration only. Invalid configuration fails at startup.

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
