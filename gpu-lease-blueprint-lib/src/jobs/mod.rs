//! Shared job helpers.

pub mod extend;
pub mod lease;
pub mod reap;
pub mod release;

/// Convert a raw 20-byte EVM caller address to a lowercase hex string with
/// `0x` prefix (same helper as the sandbox blueprint).
pub(crate) fn caller_hex(bytes: &[u8; 20]) -> String {
    let mut s = String::with_capacity(42);
    s.push_str("0x");
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// Canonical intent encoding — the preimage of `intentHash` (SPEC §1 I5).
/// Versioned and deterministic: field order and separators are part of the
/// schema. `intentVersion` bumps whenever the field set changes; unknown
/// versions must fail closed (SPEC §3).
pub fn canonical_intent(
    intent_version: u8,
    gpu_class: &str,
    duration_seconds: u64,
    confidentiality: u8,
    region: &str,
) -> String {
    format!(
        "gpu-lease-intent|v{intent_version}|{gpu_class}|{duration_seconds}|{confidentiality}|{region}"
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
        )
        .as_bytes(),
    );
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_hex_shape() {
        assert_eq!(
            caller_hex(&[0u8; 20]),
            "0x0000000000000000000000000000000000000000"
        );
        assert_eq!(
            caller_hex(&[0xff; 20]),
            "0xffffffffffffffffffffffffffffffffffffffff"
        );
        assert_eq!(caller_hex(&[0xde; 20]).len(), 42);
    }

    #[test]
    fn intent_hash_deterministic_and_field_sensitive() {
        let a = intent_hash(1, "h100", 3600, 0, "us-east");
        let b = intent_hash(1, "h100", 3600, 0, "us-east");
        assert_eq!(a, b, "deterministic");
        // Every field is bound — mutation anywhere changes the hash (I5).
        assert_ne!(a, intent_hash(2, "h100", 3600, 0, "us-east"));
        assert_ne!(a, intent_hash(1, "b200", 3600, 0, "us-east"));
        assert_ne!(a, intent_hash(1, "h100", 3601, 0, "us-east"));
        assert_ne!(a, intent_hash(1, "h100", 3600, 1, "us-east"));
        assert_ne!(a, intent_hash(1, "h100", 3600, 0, "eu-west"));
    }
}
