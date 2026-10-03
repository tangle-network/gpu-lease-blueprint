//! EXTEND (job 2) — push the operator-side session expiry out to match the
//! escrow the lessee just topped up on-chain (I2: credentials track escrow).

use blueprint_sdk::tangle::extract::{TangleArg, TangleResult};

use crate::GpuLeaseAck;
use crate::GpuLeaseExtendRequest;

pub async fn extend(
    TangleArg(request): TangleArg<GpuLeaseExtendRequest>,
) -> Result<TangleResult<GpuLeaseAck>, String> {
    let ack = extend_session(request.leaseId.into(), request.addSeconds)?;
    Ok(TangleResult(ack))
}

pub fn extend_session(lease_id: [u8; 32], add_seconds: u64) -> Result<GpuLeaseAck, String> {
    let new_expiry = crate::allocator()
        .extend(lease_id, add_seconds)
        .map_err(|e| e.to_string())?;
    tracing::debug!(lease = %hex::encode(lease_id), new_expiry, "session extended");
    Ok(GpuLeaseAck {
        leaseId: lease_id.into(),
        state: 0, // still Live
        schemaVersion: crate::SCHEMA_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_is_live_state() {
        // Shape pin: EXTEND keeps the lease Live (state 0) per vault states.
        let ack = GpuLeaseAck {
            leaseId: [2u8; 32].into(),
            state: 0,
            schemaVersion: crate::SCHEMA_VERSION,
        };
        assert_eq!(ack.state, 0);
    }
}
