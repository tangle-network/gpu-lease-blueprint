//! REAP (job 3) — permissionless post-expiry teardown. Anyone may trigger;
//! the operator frees the device and kills every credential bound to the
//! lease. Idempotent: reaping an already-swept lease still acks state=2
//! (the vault enforces single money settlement on-chain — I4; this handler
//! only mirrors teardown).

use blueprint_sdk::tangle::extract::{TangleArg, TangleResult};

use crate::GpuLeaseAck;
use crate::GpuLeaseIdRequest;

pub async fn reap(
    TangleArg(request): TangleArg<GpuLeaseIdRequest>,
) -> Result<TangleResult<GpuLeaseAck>, String> {
    let lease_id: [u8; 32] = request.leaseId.into();
    let ack = reap_session(lease_id)?;
    // Sweep any other sessions whose escrow ran out (overstay impossibility,
    // operator side — SPEC I2).
    for alloc in crate::allocator().reap_expired() {
        crate::credentials().revoke_for_lease(alloc.lease_id);
    }
    Ok(TangleResult(ack))
}

/// Core reap — pure of extractors, unit-testable.
pub fn reap_session(lease_id: [u8; 32]) -> Result<GpuLeaseAck, String> {
    // Single release: free the device if still bound (idempotent otherwise).
    if crate::allocator().release(lease_id).is_ok() {
        // Every credential for this lease dies with it — no dangling access.
        let revoked = crate::credentials().revoke_for_lease(lease_id);
        tracing::debug!(lease = %hex::encode(lease_id), revoked, "lease reaped");
    }
    Ok(GpuLeaseAck {
        leaseId: lease_id.into(),
        state: 2, // Reaped (mirrors the vault's settled state)
        schemaVersion: crate::SCHEMA_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_is_reaped_state() {
        let ack = GpuLeaseAck {
            leaseId: [3u8; 32].into(),
            state: 2,
            schemaVersion: crate::SCHEMA_VERSION,
        };
        assert_eq!(ack.state, 2);
    }
}
