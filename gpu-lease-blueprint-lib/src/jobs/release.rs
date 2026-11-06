//! A lessee can release its hardware; anyone may free it after paid expiry.
use crate::{GpuLeaseAck, GpuLeaseIdRequest, chain::ChainConfig};
use blueprint_sdk::tangle::extract::{Caller, TangleArg, TangleResult};
pub async fn release(
    Caller(caller): Caller,
    TangleArg(r): TangleArg<GpuLeaseIdRequest>,
) -> Result<TangleResult<GpuLeaseAck>, String> {
    let funded = ChainConfig::from_env()?.lease(r.leaseId.0).await?;
    if funded.now < funded.expiry {
        funded.require_owner(&format!("0x{}", hex::encode(caller)))?;
    }
    teardown(r.leaseId.0, 1)
}
pub fn teardown(id: [u8; 32], state: u8) -> Result<TangleResult<GpuLeaseAck>, String> {
    if crate::allocator().allocation(id).is_some() {
        crate::allocator().release(id).map_err(|e| e.to_string())?;
    }
    Ok(TangleResult(GpuLeaseAck {
        leaseId: id.into(),
        state,
        schemaVersion: crate::SCHEMA_VERSION,
    }))
}
