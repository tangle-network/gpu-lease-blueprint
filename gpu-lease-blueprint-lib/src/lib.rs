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
pub mod chain;
pub mod consumer;
pub mod jobs;
pub mod quote;

use blueprint_sdk::Job;
use blueprint_sdk::Router;
use blueprint_sdk::alloy::sol;
use blueprint_sdk::tangle::TangleLayer;

pub use allocator::{Allocation, AllocatorError, GpuAllocator};
pub use quote::{QuoteInputs, QuotePolicy, QuoteValidationError};

/// Job IDs — MUST match the sequential indices in the blueprint registration
/// (see SPEC §1: LEASE, RELEASE, EXTEND, REAP).
pub const JOB_LEASE: u8 = 0;
pub const JOB_RELEASE: u8 = 1;
pub const JOB_EXTEND: u8 = 2;
pub const JOB_REAP: u8 = 3;

/// On-chain result schema version (SPEC §3: every result carries schemaVersion).
pub const SCHEMA_VERSION: u16 = 1;

/// Intent schema version. v2 binds `deviceCount` into the canonical intent
/// (v1 priced one device implicitly); unknown versions fail closed (SPEC §3).
pub const INTENT_VERSION: u8 = 2;

/// Maximum devices a single lease may rent — mirrors the platform's
/// accelerator cap (the vault allows headroom above this).
pub const MAX_DEVICE_COUNT_PER_LEASE: u16 = 16;

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
        /// Devices rented under this lease — the vault escrows
        /// price × duration × deviceCount and settles per-device pro-rata.
        uint16 deviceCount;
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
/// Returns a shared `Arc` handle — safe to hold in extractors/tasks.
pub fn try_allocator() -> Result<std::sync::Arc<GpuAllocator>, String> {
    static ALLOCATOR: once_cell::sync::OnceCell<std::sync::Arc<GpuAllocator>> =
        once_cell::sync::OnceCell::new();
    ALLOCATOR
        .get_or_try_init(|| GpuAllocator::from_env().map(std::sync::Arc::new))
        .map(std::sync::Arc::clone)
}
pub fn allocator() -> std::sync::Arc<GpuAllocator> {
    try_allocator().expect("operator configuration must be validated at startup")
}

pub async fn initialize() -> Result<(), String> {
    QuotePolicy::from_env()?;
    try_allocator()?;
    let signer = api::quote_signer()?;
    let chain = chain::ChainConfig::from_env()?;
    if signer.address() != chain.operator {
        return Err("quote signer does not match GPU_OPERATOR_ADDRESS".into());
    }
    chain.validate().await?;
    chain::reconcile(true).await
}
