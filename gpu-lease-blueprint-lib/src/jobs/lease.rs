//! LEASE (job 0) — allocate a co-located device for an escrowed lease.
//!
//! Operator-side of SPEC §1: the vault already holds `price × duration`
//! escrow when this job runs. This handler verifies the intent binding
//! (recompute intentHash — fail closed), validates the redeemed price
//! against operator policy, allocates a device, and returns PUBLIC data
//! only: `{leaseId, endpoint, schemaVersion}`. Credentials are issued
//! out-of-band via the EIP-191 challenge-session API (credentials.rs).

use blueprint_sdk::tangle::extract::{Caller, TangleArg, TangleResult};

use crate::GpuLeaseOutput;
use crate::GpuLeaseRequest;
use crate::allocator::AllocatorError;
use crate::quote::QuoteValidationError;

pub async fn lease(
    Caller(caller): Caller,
    TangleArg(request): TangleArg<GpuLeaseRequest>,
) -> Result<TangleResult<GpuLeaseOutput>, String> {
    let caller = super::caller_hex(&caller);
    let output = allocate(&request, &caller).map_err(|e| e.to_string())?;
    Ok(TangleResult(output))
}

/// Core allocation logic, pure of Tangle extractors so it is unit-testable.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error(
        "intent hash mismatch: request binds 0x{actual}, canonical intent hashes to 0x{expected}"
    )]
    IntentHashMismatch { actual: String, expected: String },
    #[error("unsupported intent version {0} (fail closed, SPEC §3)")]
    UnknownIntentVersion(u8),
    #[error(transparent)]
    Allocator(#[from] AllocatorError),
    #[error(transparent)]
    QuoteValidation(#[from] QuoteValidationError),
}

pub fn allocate(request: &GpuLeaseRequest, caller: &str) -> Result<GpuLeaseOutput, LeaseError> {
    if request.intentVersion != 1 {
        return Err(LeaseError::UnknownIntentVersion(request.intentVersion));
    }
    // I5 operator side: the intent the lessee claims must hash to the
    // intentHash the RFQ quote signed and the vault stores.
    let expected = super::intent_hash(
        request.intentVersion,
        &request.gpuClass,
        request.durationSeconds,
        request.confidentiality,
        &request.region,
    );
    let actual: [u8; 32] = request.intentHash.into();
    if expected != actual {
        return Err(LeaseError::IntentHashMismatch {
            actual: hex::encode(actual),
            expected: hex::encode(expected),
        });
    }

    // Fail-closed price check against operator policy (SPEC §2 quote policy).
    let policy = crate::QuotePolicy::from_env();
    policy.validate_redeemed_price(&request.gpuClass, request.pricePerSecond)?;

    // Deterministic leaseId: binds intent + lessee + operator monotonic nonce.
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let lease_id = crate::allocator::derive_lease_id(actual, caller, nonce);

    let alloc = crate::allocator().acquire(
        &request.gpuClass,
        request.confidentiality > 0,
        lease_id,
        caller,
        request.durationSeconds,
    )?;

    // PUBLIC DATA ONLY — no credentials, no secrets (SPEC §2).
    Ok(GpuLeaseOutput {
        leaseId: lease_id.into(),
        endpoint: alloc.endpoint_v1,
        schemaVersion: crate::SCHEMA_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::GpuAllocator;
    use crate::allocator::GpuDevice;

    /// Tests share no global static (env-inventory); they build a fresh
    /// allocator — which is why `allocate_core` takes an allocator below.
    fn alloc_core(
        a: &GpuAllocator,
        request: &GpuLeaseRequest,
        caller: &str,
    ) -> Result<GpuLeaseOutput, LeaseError> {
        if request.intentVersion != 1 {
            return Err(LeaseError::UnknownIntentVersion(request.intentVersion));
        }
        let expected = super::super::intent_hash(
            request.intentVersion,
            &request.gpuClass,
            request.durationSeconds,
            request.confidentiality,
            &request.region,
        );
        let actual: [u8; 32] = request.intentHash.into();
        if expected != actual {
            return Err(LeaseError::IntentHashMismatch {
                actual: hex::encode(actual),
                expected: hex::encode(expected),
            });
        }
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let lease_id = crate::allocator::derive_lease_id(actual, caller, nonce);
        let alloc = a.acquire(
            &request.gpuClass,
            request.confidentiality > 0,
            lease_id,
            caller,
            request.durationSeconds,
        )?;
        Ok(GpuLeaseOutput {
            leaseId: lease_id.into(),
            endpoint: alloc.endpoint_v1,
            schemaVersion: crate::SCHEMA_VERSION,
        })
    }

    fn request() -> GpuLeaseRequest {
        let class = "h100".to_string();
        let region = "us-east".to_string();
        let dur = 3600u64;
        GpuLeaseRequest {
            intentVersion: 1,
            intentHash: crate::jobs::intent_hash(1, &class, dur, 0, &region).into(),
            pricePerSecond: 300_000_000_000_000u128,
            durationSeconds: dur,
            confidentiality: 0,
            gpuClass: class,
            region,
        }
    }

    #[test]
    fn happy_path_allocates_public_output_only() {
        let a = GpuAllocator::new(vec![GpuDevice {
            id: "d1".into(),
            gpu_class: "h100".into(),
            tee: false,
            cuda_ordinal: 0,
        }]);
        let out = alloc_core(&a, &request(), "0xabc").unwrap();
        assert_eq!(out.schemaVersion, 1);
        let endpoint: serde_json::Value = serde_json::from_str(&out.endpoint).unwrap();
        assert_eq!(endpoint["transport"], "local-attach");
        assert!(
            !out.endpoint.contains("token"),
            "no credential material in job output"
        );
        assert!(!out.endpoint.contains("secret"));
    }

    #[test]
    fn tampered_intent_fails_closed() {
        let a = GpuAllocator::new(vec![]);
        let mut r = request();
        r.durationSeconds += 1; // intent mutated after quoting
        assert!(matches!(
            alloc_core(&a, &r, "0xabc"),
            Err(LeaseError::IntentHashMismatch { .. })
        ));
    }

    #[test]
    fn unknown_version_fails_closed() {
        let a = GpuAllocator::new(vec![]);
        let mut r = request();
        r.intentVersion = 99;
        assert!(matches!(
            alloc_core(&a, &r, "0xabc"),
            Err(LeaseError::UnknownIntentVersion(99))
        ));
    }
}
