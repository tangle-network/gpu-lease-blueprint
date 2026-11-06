use base64::{Engine, engine::general_purpose::STANDARD};
use blueprint_sdk::alloy::{
    primitives::{Address, keccak256},
    sol_types::SolStruct,
};
use clap::Parser;
use gpu_lease_blueprint_lib::{
    GpuLeaseAck, GpuLeaseExtendRequest, GpuLeaseIdRequest, GpuLeaseOutput, GpuLeaseRequest,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(about = "Generate wire metadata and an optional cargo-tangle definition")]
struct Args {
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../metadata"))]
    metadata_dir: PathBuf,
    /// SDK definition output; requires a deployed manager and SDK source descriptors.
    #[arg(long, requires_all = ["manager", "sources"])]
    definition: Option<PathBuf>,
    #[arg(long, requires = "definition")]
    manager: Option<Address>,
    /// JSON array in cargo-tangle's standard sources format, from the artifact publisher.
    #[arg(long, requires = "definition")]
    sources: Option<PathBuf>,
}

fn output_path(path: &Path) -> std::io::Result<PathBuf> {
    let mut path = std::path::absolute(path)?;
    for _ in 0..40 {
        match std::fs::read_link(&path) {
            Ok(target) => path = path.parent().unwrap().join(target),
            Err(_) => {
                return Ok(path
                    .parent()
                    .unwrap()
                    .canonicalize()?
                    .join(path.file_name().unwrap()));
            }
        }
    }
    Err(std::io::Error::other("output symlink loop"))
}

fn type_info<T: SolStruct>() -> Value {
    let encode_type = T::eip712_encode_type().into_owned();
    let typehash = keccak256(encode_type.as_bytes());
    json!({
        "encodeType": encode_type,
        "typehash": format!("{typehash}"),
    })
}

fn job(id: u8, name: &str, description: &str, params: Value, result: Value) -> Value {
    json!({
        "id": id,
        "name": name,
        "description": description,
        "params": params,
        "result": result,
    })
}

fn build_document() -> Value {
    json!({
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
            "meter": "escrow",
            "settlement": "vault",
            "quote": {
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
        "vault": {
            "leaseView": "leases(bytes32)",
            "states": { "0": "Live", "1": "Released", "2": "Reaped" },
        },
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.metadata_dir)?;
    let out_path = args.metadata_dir.join("blueprint.json");
    if let Some(path) = &args.definition {
        if output_path(path)? == output_path(&out_path)?
            || same_file::is_same_file(path, &out_path).unwrap_or(false)
        {
            return Err("definition and metadata outputs must be different files".into());
        }
    }
    let doc = build_document();
    let json = serde_json::to_string_pretty(&doc)?;
    let hash = keccak256(json.as_bytes());

    if let Some(path) = args.definition {
        let manager = args.manager.unwrap();
        if manager.is_zero() {
            return Err("manager must be a deployed service manager address".into());
        }
        let sources: Vec<Value> = serde_json::from_slice(&std::fs::read(args.sources.unwrap())?)?;
        if sources.is_empty() {
            return Err("sources must contain published artifact descriptors".into());
        }
        let blueprint = &doc["blueprint"];
        let definition = json!({
            "metadata_uri": format!("data:application/json;base64,{}", STANDARD.encode(&json)),
            "metadata_hash": format!("{hash}"),
            "manager": manager,
            "config": {
                "membership": "dynamic", "pricing": "pay_once",
                "min_operators": 1, "max_operators": 100,
                "subscription_rate": 0, "subscription_interval": 0, "event_rate": 0,
            },
            "metadata": {
                "name": blueprint["name"], "description": blueprint["description"],
                "author": "Tangle", "category": blueprint["category"],
                "code_repository": blueprint["codeRepository"],
                "website": "https://tangle.network", "license": blueprint["license"],
            },
            "jobs": doc["jobs"].as_array().unwrap().iter().map(|job| json!({
                "name": job["name"], "description": job["description"],
            })).collect::<Vec<_>>(),
            "sources": sources,
            "supported_memberships": ["dynamic"],
        });
        std::fs::write(&path, serde_json::to_string_pretty(&definition)?)?;
        println!("wrote {}", path.display());
    }
    std::fs::write(&out_path, &json)?;

    println!("wrote {}", out_path.display());
    println!("metadataHash = {hash}");
    Ok(())
}
