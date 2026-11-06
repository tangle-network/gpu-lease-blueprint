//! Synchronize access to paid chain expiry; job retries never create time.
use crate::{GpuLeaseAck, GpuLeaseExtendRequest, chain::ChainConfig};
use blueprint_sdk::tangle::extract::{Caller, TangleArg, TangleResult};
pub async fn extend(
    Caller(caller): Caller,
    TangleArg(r): TangleArg<GpuLeaseExtendRequest>,
) -> Result<TangleResult<GpuLeaseAck>, String> {
    let funded = ChainConfig::from_env()?.lease(r.leaseId.0).await?;
    funded.require_live()?;
    funded.require_owner(&format!("0x{}", hex::encode(caller)))?;
    let local = crate::allocator()
        .allocation(r.leaseId.0)
        .ok_or("unknown operator allocation")?;
    if r.addSeconds == 0
        || local
            .expires_at
            .checked_add(r.addSeconds)
            .ok_or("expiry overflow")?
            > funded.expiry
    {
        return Err("extension has not been paid in escrow".into());
    }
    crate::allocator()
        .sync_expiry(r.leaseId.0, funded.expiry)
        .map_err(|e| e.to_string())?;
    Ok(TangleResult(GpuLeaseAck {
        leaseId: r.leaseId,
        state: 0,
        schemaVersion: crate::SCHEMA_VERSION,
    }))
}
