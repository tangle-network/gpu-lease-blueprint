//! Shared job helpers.

pub mod extend;
pub mod lease;
pub mod reap;
pub mod release;

/// Canonical intent encoding — the preimage of `intentHash` (SPEC §1 I5).
/// Versioned and deterministic: field order and separators are part of the
/// schema. `intentVersion` bumps whenever the field set changes; unknown
/// versions must fail closed (SPEC §3). v2 binds `device_count` — the
/// quantity is priced, so it is part of the intent the quote signs.
pub fn canonical_intent(
    intent_version: u8,
    gpu_class: &str,
    duration_seconds: u64,
    confidentiality: u8,
    region: &str,
    device_count: u16,
) -> String {
    format!(
        "gpu-lease-intent|v{intent_version}|{gpu_class}|{duration_seconds}|{confidentiality}|{region}|{device_count}"
    )
}

/// keccak256 of the canonical intent — what the RFQ quote signs and what the
/// vault stores immutably (I5).
pub fn intent_hash(
    intent_version: u8,
    gpu_class: &str,
    duration_seconds: u64,
    confidentiality: u8,
    region: &str,
    device_count: u16,
) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    k.update(
        canonical_intent(
            intent_version,
            gpu_class,
            duration_seconds,
            confidentiality,
            region,
            device_count,
        )
        .as_bytes(),
    );
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}
