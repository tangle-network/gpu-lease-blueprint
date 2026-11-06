//! Reaping is permissionless only after the vault's paid expiry.
use crate::{GpuLeaseAck, GpuLeaseIdRequest, chain::ChainConfig};
use blueprint_sdk::tangle::extract::{TangleArg, TangleResult};
pub async fn reap(
    TangleArg(r): TangleArg<GpuLeaseIdRequest>,
) -> Result<TangleResult<GpuLeaseAck>, String> {
    let funded = ChainConfig::from_env()?.lease(r.leaseId.0).await?;
    if funded.now < funded.expiry {
        return Err("lease has not expired".into());
    }
    super::release::teardown(r.leaseId.0, 2)
}
