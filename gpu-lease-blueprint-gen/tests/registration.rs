use base64::{Engine, engine::general_purpose::STANDARD};
use blueprint_sdk::{
    alloy::{
        primitives::{U256, keccak256},
        providers::{Provider, ProviderBuilder},
        sol_types::SolValue,
    },
    clients::tangle::{
        TangleClient,
        contracts::{ITangleBlueprints, ITangleTypes},
    },
};
use serde_json::Value;
use std::{fs, path::Path};

#[path = "../../tests/support/registration.rs"]
mod fixture;
use fixture::Result;

#[tokio::test]
async fn cargo_tangle_registers_generated_definition() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let fixture = fixture::registered(
        root,
        Path::new(env!("CARGO_BIN_EXE_gpu-lease-blueprint-gen")),
    )
    .await?;
    let provider = ProviderBuilder::new().connect_http(fixture.http.parse()?);
    let registry = ITangleBlueprints::new(fixture.tangle, &provider);
    let (id, owner, manager) = (fixture.id, fixture.owner, fixture.manager);
    let (scratch, broadcast, keystore_path) =
        (&fixture.scratch, &fixture.broadcast, &fixture.keystore_path);
    let (binary, binary_hash) = (&fixture.binary, &fixture.binary_hash);
    assert_eq!(registry.blueprintCount().call().await?, id + 1);
    let registered = registry.getBlueprint(id).call().await?;
    assert_eq!(registered.owner, owner);
    assert_eq!(registered.manager, manager);
    let view = registry.getBlueprintDefinition(id).call().await?;
    let client = TangleClient::new(fixture.client_config(keystore_path)?).await?;
    let encoded = client.get_raw_blueprint_definition_from_event(id).await?;
    let registered = ITangleTypes::BlueprintDefinition::abi_decode(&encoded)?;
    assert_eq!(view.config.abi_encode(), registered.config.abi_encode());
    assert_eq!(view.jobs.len(), 4);
    assert!(
        view.jobs
            .iter()
            .all(|job| job.paramsSchema.is_empty() && job.resultSchema.is_empty())
    );
    assert!(view.registrationSchema.is_empty() && view.requestSchema.is_empty());
    let names = registered.jobs.iter().map(|job| job.name.as_str());
    assert!(names.eq(["lease", "release", "extend", "reap"]));
    let config = &registered.config;
    assert_eq!(config.minOperators, 1);
    assert_eq!(config.maxOperators, 100);
    assert!(view.hasConfig && registered.hasConfig);
    assert_eq!(u8::from(config.membership), 1);
    // Pay-once with no payment: the vault is the only payout path, so jobs carry no fee.
    assert_eq!(u8::from(config.pricing), 0);
    assert_eq!(registered.supportedMemberships, [config.membership]);
    assert_eq!(view.supportedMemberships, registered.supportedMemberships);
    assert_eq!(config.subscriptionRate, U256::ZERO);
    assert_eq!(config.subscriptionInterval, 0);
    assert_eq!(config.eventRate, U256::ZERO);
    assert_eq!(registered.sources.len(), 1);
    let source = &registered.sources[0];
    assert_eq!(u8::from(source.kind), 2);
    assert_eq!(u8::from(source.native.fetcher), 0);
    assert_eq!(
        source.native.artifactUri,
        format!("file://{}", binary.display())
    );
    assert_eq!(source.native.entrypoint, "gpu-lease-blueprint");
    assert_eq!(source.binaries.len(), 1);
    let binary = &source.binaries[0];
    assert_eq!((u8::from(binary.arch), u8::from(binary.os)), (5, 1));
    assert_eq!(binary.name, "gpu-lease-blueprint");
    assert_eq!(hex::encode(binary.sha256), *binary_hash);
    let metadata = registry.blueprintMetadata(id).call().await?;
    let bytes = STANDARD.decode(
        metadata
            .metadataUri
            .strip_prefix("data:application/json;base64,")
            .unwrap(),
    )?;
    assert_eq!(bytes, fs::read(scratch.path().join("blueprint.json"))?);
    let document: Value = serde_json::from_slice(&bytes)?;
    for (id, job) in registered.jobs.iter().enumerate() {
        assert_eq!(document["jobs"][id]["id"], id);
        assert_eq!(document["jobs"][id]["name"], job.name);
    }
    assert_eq!(
        document["blueprint"]["category"],
        registered.metadata.category
    );
    assert_eq!(metadata.metadataHash, keccak256(&bytes));
    assert_eq!(registered.metadata.category, "Compute");
    println!(
        "cargo-tangle registration ok: chain={} fixture={} blueprint={id} manager={manager} metadataHash={}",
        provider.get_chain_id().await?,
        broadcast["commit"],
        metadata.metadataHash
    );
    Ok(())
}
