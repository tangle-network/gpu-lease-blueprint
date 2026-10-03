//! Canonical blueprint metadata generator — THE single source of truth flow.
//!
//! The Rust `sol!` types in `gpu-lease-blueprint-lib` are the wire format the
//! operator actually encodes and decodes. This generator derives the canonical
//! metadata JSON from THOSE TYPES via their EIP-712 `encode_type` — the field
//! lists and types are extracted from the compiled macro output, not parsed
//! from source or hand-maintained. Drift is structurally impossible: change a
//! `sol!` field and the JSON (and its typehash) changes with it.
//!
//! Output: `metadata/blueprint.json` + the keccak256 hash printed on stdout.
//! The register script embeds this JSON as the on-chain `metadataUri` (data
//! URI) and pins `metadataHash` — the TangleDriver then reads the definition
//! from chain, fetches the URI, verifies the hash. No protocol changes; the
//! fields used all exist in tnt-core 0.19.
//!
//! Run: `cargo run -p gpu-lease-blueprint-gen`

use blueprint_sdk::alloy::sol_types::SolStruct;
use gpu_lease_blueprint_lib::QuotePolicy;
use gpu_lease_blueprint_lib::{
    GpuLeaseAck, GpuLeaseExtendRequest, GpuLeaseIdRequest, GpuLeaseOutput, GpuLeaseRequest,
};
use tiny_keccak::{Hasher, Keccak};

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut k = Keccak::v256();
    k.update(data);
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}

fn type_info<T: SolStruct>() -> serde_json::Value {
    let encode_type = T::eip712_encode_type().into_owned();
    let typehash = keccak256(encode_type.as_bytes());
    serde_json::json!({
        // EIP-712 canonical encode-type, e.g.
        // "GpuLeaseRequest(uint8 intentVersion,bytes32 intentHash,...)"
        "encodeType": encode_type,
        "typehash": format!("0x{}", hex::encode(typehash)),
    })
}

fn job(
    id: u8,
    name: &str,
    description: &str,
    params: serde_json::Value,
    result: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": name,
        "description": description,
        // Job ids are positional and MUST match the Rust router + BSM constants.
        "params": params,
        "result": result,
    })
}

fn build_document() -> serde_json::Value {
    serde_json::json!({
        // Unknown schemaVersion => readers fail closed (SPEC §3).
        "schemaVersion": 1,
        "blueprint": {
            "id": "gpu-lease-blueprint",
            "name": "GPU Lease Blueprint",
            // Convention the TangleDriver keys on (no side registry):
            "category": "Compute",
            "description": "Escrowed GPU leases: parking-meter economics, RFQ pricing, TEE-bound quotes",
            "codeRepository": "https://github.com/tangle-network/gpu-lease-blueprint",
            "license": "MIT OR Apache-2.0",
        },
        // The compute resource this blueprint meters. Generic vocabulary —
        // a TPU/storage/inference blueprint fills the same shape.
        "resource": {
            "kind": "gpu",
            "unit": "second",
            "meter": "escrow",           // parking-meter: prepay, pro-rata refund
            "settlement": "vault",        // escrow kernel + pro-rata, invariants I1–I5
            "classes": QuotePolicy::default()
                .base_price_per_second
                .keys()
                .cloned()
                .collect::<Vec<String>>(),
            "quote": {
                // The operator's composable bps policy (see quote.rs): the
                // driver renders the "why" behind a price from these facts.
                "factors": ["classBps", "utilizationCurve", "teePremiumBps"],
                "endpoint": "/api/quote",       // probed, never registered
                "capabilities": "/api/capabilities",
            },
        },
        "jobs": [
            job(0, "lease", "Create an escrowed GPU lease (allocates a device)",
                type_info::<GpuLeaseRequest>(), type_info::<GpuLeaseOutput>()),
            job(1, "release", "Voluntarily release a lease (exact pro-rata refund on the vault)",
                type_info::<GpuLeaseIdRequest>(), type_info::<GpuLeaseAck>()),
            job(2, "extend", "Extend a lease session (escrow topped up on the vault)",
                type_info::<GpuLeaseExtendRequest>(), type_info::<GpuLeaseAck>()),
            job(3, "reap", "Permissionless post-expiry teardown",
                type_info::<GpuLeaseIdRequest>(), type_info::<GpuLeaseAck>()),
        ],
        // Lease-state read surface for driver widgets (chain clock truth):
        "vault": {
            "leaseView": "leases(bytes32)",
            "states": { "0": "Live", "1": "Released", "2": "Reaped" },
        },
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let doc = build_document();
    let json = serde_json::to_string_pretty(&doc)?;
    let hash = keccak256(json.as_bytes());

    let out_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../metadata");
    std::fs::create_dir_all(&out_dir)?;
    let out_path = out_dir.join("blueprint.json");
    std::fs::write(&out_path, &json)?;

    println!("wrote {}", out_path.display());
    println!("metadataHash = 0x{}", hex::encode(hash));
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed metadata/blueprint.json must exactly match what the
    /// current `sol!` types generate. If this fails, run:
    ///   cargo run -p gpu-lease-blueprint-gen
    /// and commit the result. This is the drift-killer: the JSON, the wire
    /// format, and the on-chain pin are one artifact.
    #[test]
    fn committed_metadata_matches_sol_types() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../metadata/blueprint.json");
        let Ok(committed) = std::fs::read_to_string(&path) else {
            eprintln!(
                "skipping: {} not found — run `cargo run -p gpu-lease-blueprint-gen`",
                path.display()
            );
            return;
        };
        let expected = serde_json::to_string_pretty(&build_document()).unwrap();
        assert_eq!(
            committed, expected,
            "metadata/blueprint.json is stale — regenerate with `cargo run -p gpu-lease-blueprint-gen`"
        );
    }

    #[test]
    fn job_ids_are_sequential_and_pinned() {
        let doc = build_document();
        let jobs = doc["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 4);
        for (i, job) in jobs.iter().enumerate() {
            assert_eq!(
                job["id"].as_u64().unwrap(),
                i as u64,
                "job ids must be sequential"
            );
        }
        assert_eq!(jobs[0]["name"], "lease");
        assert_eq!(jobs[1]["name"], "release");
        assert_eq!(jobs[2]["name"], "extend");
        assert_eq!(jobs[3]["name"], "reap");
    }
}
