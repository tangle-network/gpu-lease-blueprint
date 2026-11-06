//! Reserve operator-owned hardware only for an existing funded vault lease.
use crate::{GpuLeaseOutput, GpuLeaseRequest, chain::ChainConfig};
use blueprint_sdk::tangle::extract::{Caller, TangleArg, TangleResult};

pub async fn lease(
    Caller(caller): Caller,
    TangleArg(r): TangleArg<GpuLeaseRequest>,
) -> Result<TangleResult<GpuLeaseOutput>, String> {
    let caller = format!("0x{}", hex::encode(caller));
    if r.intentVersion != crate::INTENT_VERSION || r.confidentiality > 1 {
        return Err("unsupported intent/confidentiality".into());
    }
    let expected = super::intent_hash(
        r.intentVersion,
        &r.gpuClass,
        r.durationSeconds,
        r.confidentiality,
        &r.region,
        r.deviceCount,
    );
    if expected != r.intentHash.0 {
        return Err("intent hash mismatch".into());
    }
    if !format!("{:#x}", r.lessee).eq_ignore_ascii_case(&caller) {
        return Err("requester is not lessee".into());
    }
    crate::QuotePolicy::from_env()?
        .require_lease_price(&r)
        .map_err(|e| e.to_string())?;
    let chain = ChainConfig::from_env()?;
    let funded = chain.lease(r.leaseId.0).await?;
    funded.require_request(&r)?;
    funded.require_owner(&caller)?;
    let tee = crate::allocator::tee_from_wire(r.sandboxTeeType).map_err(|e| e.to_string())?;
    if r.confidentiality == 0 && tee.is_some() {
        return Err("TEE requested by nonconfidential lease".into());
    }
    let allocation = crate::allocator()
        .acquire_until(
            &r.gpuClass,
            r.confidentiality == 1,
            tee,
            r.deviceCount,
            r.leaseId.0,
            &caller,
            r.intentHash.0,
            funded.expiry,
        )
        .map_err(|e| e.to_string())?;
    Ok(TangleResult(GpuLeaseOutput {
        leaseId: r.leaseId,
        endpoint: allocation.endpoint_v1,
        schemaVersion: crate::SCHEMA_VERSION,
    }))
}
