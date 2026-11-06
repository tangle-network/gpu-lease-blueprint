//! Generate metadata/blueprint.json from the operator's Solidity wire types.
//! Registration publishes its data URI and keccak256 digest.
//! Run: cargo run -p gpu-lease-blueprint-gen

use blueprint_sdk::alloy::sol_types::SolStruct;
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
        "schemaVersion": 1,
        "blueprint": {
            "id": "gpu-lease-blueprint",
            "name": "GPU Lease Blueprint",
            "category": "Compute",
            "description": "Escrowed GPU leases: parking-meter economics, RFQ pricing, TEE-bound quotes",
            "codeRepository": "https://github.com/tangle-network/gpu-lease-blueprint",
            "license": "MIT OR Apache-2.0",
        },
        "resource": {
            "kind": "gpu",
            "unit": "second",
            "meter": "escrow",           // parking-meter: prepay, pro-rata refund
            "settlement": "vault",        // escrow kernel + pro-rata, invariants I1–I5
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
