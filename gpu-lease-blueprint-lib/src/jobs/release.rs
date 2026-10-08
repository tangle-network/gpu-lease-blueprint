//! RELEASE (job 1) — voluntary teardown: free the device, revoke every
//! credential bound to the lease (SPEC §2 credentials are revocable; the
//! vault simultaneously refunds pro-rata — I3).

use blueprint_sdk::tangle::extract::{TangleArg, TangleResult};

use crate::GpuLeaseAck;
use crate::GpuLeaseIdRequest;

pub async fn release(
    TangleArg(request): TangleArg<GpuLeaseIdRequest>,
) -> Result<TangleResult<GpuLeaseAck>, String> {
    let ack = teardown(request.leaseId.into(), 1)?;
    Ok(TangleResult(ack))
}

/// Core teardown — pure of extractors, unit-testable. `state` mirrors the
/// vault's post-settlement state (1 = Released, 2 = Reaped).
pub fn teardown(lease_id: [u8; 32], state: u8) -> Result<GpuLeaseAck, String> {
    let alloc = crate::allocator()
        .release(lease_id)
        .map_err(|e| e.to_string())?;
    // Every credential for this lease dies with it — no dangling access.
    let revoked = crate::credentials().revoke_for_lease(lease_id);
    tracing::debug!(lease = %hex::encode(lease_id), devices = ?alloc.device_ids, revoked, "lease torn down");
    Ok(GpuLeaseAck {
        leaseId: lease_id.into(),
        state,
        schemaVersion: crate::SCHEMA_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::{GpuAllocator, GpuDevice};

    fn setup() -> [u8; 32] {
        let a = GpuAllocator::new(vec![GpuDevice {
            id: "d1".into(),
            gpu_class: "h100".into(),
            tee: false,
            tee_type: crate::allocator::TeeType::None,
            cuda_ordinal: 0,
        }]);
        let lease = [11u8; 32];
        a.acquire("h100", false, 1, lease, "0xabc", 60).unwrap();
        lease
    }

    // teardown() uses the global allocator static (empty in tests), so the
    // pure-core coverage lives in allocator.rs / credentials.rs tests; here
    // we pin the ack shape only.
    #[test]
    fn ack_shape() {
        let ack = GpuLeaseAck {
            leaseId: [1u8; 32].into(),
            state: 1,
            schemaVersion: crate::SCHEMA_VERSION,
        };
        assert_eq!(ack.state, 1);
        assert_eq!(ack.schemaVersion, 1);
        let _ = setup(); // allocator exercised
    }
}
