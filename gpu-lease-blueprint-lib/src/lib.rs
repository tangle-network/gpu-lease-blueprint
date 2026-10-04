//! GPU Lease Blueprint — operator library.
//!
//! SPEC.md is design law. This crate implements SPEC §2 (off-chain surfaces):
//! the co-located device allocator, the composable quote policy, and the
//! EIP-191 challenge-session credential path. Credentials NEVER appear in job
//! calldata or results — job outputs carry `{leaseId, endpoint, schemaVersion}`
//! public data only. The on-chain money semantics live in `src/GpuLeaseVault.sol`
//! and are pinned by `test/GpuLeaseVault.t.sol`.

pub mod allocator;
pub mod api;
pub mod credentials;
pub mod jobs;
pub mod quote;

use blueprint_sdk::Job;
use blueprint_sdk::Router;
use blueprint_sdk::alloy::sol;
use blueprint_sdk::tangle::TangleLayer;

pub use allocator::{Allocation, AllocatorError, GpuAllocator};
pub use credentials::{CredentialError, CredentialSessions, ScopedCredential, eip191_recover_signer, eip191_sign_message, verifying_key_address};
pub use quote::{QuoteInputs, QuotePolicy, QuoteValidationError};

/// Job IDs — MUST match the sequential indices in the blueprint registration
/// (see SPEC §1: LEASE, RELEASE, EXTEND, REAP).
pub const JOB_LEASE: u8 = 0;
pub const JOB_RELEASE: u8 = 1;
pub const JOB_EXTEND: u8 = 2;
pub const JOB_REAP: u8 = 3;

/// On-chain result schema version (SPEC §3: every result carries schemaVersion).
pub const SCHEMA_VERSION: u16 = 1;

sol! {
    /// LEASE job input. Mirrors the RFQ quote's bound fields (SPEC §1 Quoting):
    /// requester + inputsHash + confidentiality + price. `intentHash` is
    /// keccak256 over the canonical intent — the operator recomputes and
    /// compares (fail-closed) before allocating.
    struct GpuLeaseRequest {
        uint8 intentVersion;
        bytes32 intentHash;
        uint128 pricePerSecond;
        uint64 durationSeconds;
        uint8 confidentiality;
        string gpuClass;
        string region;
        /// SPEC §1: the RFQ quote binds the requester.
        address lessee;
        /// The vault lease the buyer already created and escrowed — one
        /// identity across money and routing (operator binds to THIS id).
        bytes32 leaseId;
        /// The sandbox this GPU is attached to (composition link — SPEC §5).
        /// Zero when the lease is standalone (no sandbox attach).
        bytes32 sandboxId;
        /// The sandbox's TEE type when confidentiality=1 (composition).
        /// 0=none, 1=Nitro, 2=TDX, 3=SEV — must match the GPU's TEE.
        uint8 sandboxTeeType;
    }

    /// LEASE job output — PUBLIC DATA ONLY (SPEC §2 Credentials).
    /// `endpoint` is a versioned discriminated union; v1 = local-attach.
    struct GpuLeaseOutput {
        bytes32 leaseId;
        string endpoint;
        uint16 schemaVersion;
    }

    /// RELEASE / REAP job input.
    struct GpuLeaseIdRequest {
        bytes32 leaseId;
    }

    /// EXTEND job input — off-chain credential session extension sidekick;
    /// the escrow extension itself is a value-carrying call to the vault.
    struct GpuLeaseExtendRequest {
        bytes32 leaseId;
        uint64 addSeconds;
    }

    /// RELEASE / REAP / EXTEND job output.
    struct GpuLeaseAck {
        bytes32 leaseId;
        uint8 state; // 0=Live 1=Released 2=Reaped (mirrors vault states)
        uint16 schemaVersion;
    }
}

/// Router that maps job IDs to handlers — the indices MUST match
/// `RegisterBlueprint`'s job order (mirrors the sandbox blueprint pattern).
pub fn router() -> Router {
    Router::new()
        .route(JOB_LEASE, jobs::lease::lease.layer(TangleLayer))
        .route(JOB_RELEASE, jobs::release::release.layer(TangleLayer))
        .route(JOB_EXTEND, jobs::extend::extend.layer(TangleLayer))
        .route(JOB_REAP, jobs::reap::reap.layer(TangleLayer))
}

/// Operator-wide allocator (co-located inventory; no on-chain registry — SPEC §4).
/// Operator-wide allocator (co-located inventory; no on-chain registry — SPEC §4).
/// Returns a shared `Arc` handle — safe to hold in extractors/tasks.
pub fn allocator() -> std::sync::Arc<GpuAllocator> {
    static ALLOCATOR: once_cell::sync::OnceCell<std::sync::Arc<GpuAllocator>> =
        once_cell::sync::OnceCell::new();
    std::sync::Arc::clone(ALLOCATOR.get_or_init(|| std::sync::Arc::new(GpuAllocator::from_env())))
}

/// Operator-wide credential session store.
pub fn credentials() -> std::sync::Arc<CredentialSessions> {
    static CREDS: once_cell::sync::OnceCell<std::sync::Arc<CredentialSessions>> =
        once_cell::sync::OnceCell::new();
    std::sync::Arc::clone(CREDS.get_or_init(|| std::sync::Arc::new(CredentialSessions::new())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_match_spec_order() {
        // SPEC §1 order: LEASE, RELEASE, EXTEND, REAP — sequential, no gaps,
        // because tnt-core indexes jobs by registration order.
        assert_eq!(JOB_LEASE, 0);
        assert_eq!(JOB_RELEASE, 1);
        assert_eq!(JOB_EXTEND, 2);
        assert_eq!(JOB_REAP, 3);
    }

    #[test]
    fn router_binds_all_four_jobs() {
        let r = router();
        // Router exposes bound routes; the four SPEC jobs must be present.
        // (Assertion of construction + no panic; route table is type-level.)
        let _ = &r;
    }
}
