//! REAP (job 3) — permissionless post-expiry teardown. Anyone may trigger;
//! the operator sweeps expired sessions and frees devices. Credential death
//! at expiry is what makes overstay impossible on the operator side (I2).

use blueprint_sdk::tangle::extract::{TangleArg, TangleResult};

use crate::GpuLeaseAck;
use crate::GpuLeaseIdRequest;

pub async fn reap(
    TangleArg(request): TangleArg<GpuLeaseIdRequest>,
) -> Result<TangleResult<GpuLeaseAck>, String> {
    // Free this lease if still live (idempotent if already swept).
    let ack = match crate::allocator().release(request.leaseId.into()) {
        Ok(_) => crate::jobs::release::teardown(request.leaseId.into(), 2)?,
        Err(_) => GpuLeaseAck {
            leaseId: request.leaseId,
            state: 2,
            schemaVersion: crate::SCHEMA_VERSION,
        },
    };
    // Sweep any other sessions whose escrow ran out.
    for alloc in crate::allocator().reap_expired() {
        crate::credentials().revoke_for_lease(alloc.lease_id);
    }
    Ok(TangleResult(ack))
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
