# GPU Lease Blueprint

Decentralized GPU compute marketplace on Tangle. Operators with GPUs offer them for rent;
sandboxes (and any blueprint) attach via the sandbox SDK's `gpuLease`.

**Read [SPEC.md](./SPEC.md) first** — the supreme design: the contract is a fixed point of
invariants (escrow conservation, quote binding, slashing); everything that evolves — pricing
curves, GPU generations, topology, attach transports — is off-chain data with versioned schemas.
Designed so the only future churn is dependency updates.

Status: spec frozen · contract/operator/SDK per SPEC §6 build order.
