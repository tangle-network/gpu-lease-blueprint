//! Normal SDK CLI registration against the full protocol snapshot and compiled GPU manager.
//! Requires anvil, cargo-tangle, forge artifacts, and a built operator binary.

use base64::{Engine, engine::general_purpose::STANDARD};
use blueprint_sdk::{
    alloy::{
        primitives::{Address, Bytes, U256, keccak256},
        providers::{Provider, ProviderBuilder},
        rpc::types::TransactionRequest,
        sol_types::SolValue,
    },
    clients::tangle::contracts::ITangleBlueprints,
    keystore::{Keystore, KeystoreConfig, backends::ecdsa::EcdsaBackend},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn checked(command: &mut Command) -> Result {
    let log = tempfile::NamedTempFile::new()?;
    let mut child = Process(
        command
            .stdout(Stdio::null())
            .stderr(log.reopen()?)
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.0.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("{status}\n{}", fs::read_to_string(log.path())?).into())
            };
        }
        if Instant::now() >= deadline {
            return Err("child process exceeded 60 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn deployed(broadcast: &Value, name: &str, proxied: bool) -> Address {
    let transactions = broadcast["transactions"].as_array().unwrap();
    let implementation = transactions
        .iter()
        .find(|tx| tx["transactionType"] == "CREATE" && tx["contractName"] == name)
        .unwrap();
    let address = implementation["contractAddress"].as_str().unwrap();
    let deployment = if proxied {
        transactions
            .iter()
            .find(|tx| {
                tx["transactionType"] == "CREATE"
                    && tx["contractName"] == "ERC1967Proxy"
                    && tx["arguments"][0]
                        .as_str()
                        .is_some_and(|arg| arg.eq_ignore_ascii_case(address))
            })
            .unwrap()
    } else {
        implementation
    };
    deployment["contractAddress"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn bytecode(root: &Path, name: &str) -> Result<Vec<u8>> {
    let path = root.join(format!("contracts/out/{name}.sol/{name}.json"));
    let artifact: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(hex::decode(
        artifact["bytecode"]["object"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
    )?)
}

async fn deploy(provider: &impl Provider, sender: Address, code: Vec<u8>) -> Result<Address> {
    let tx = TransactionRequest::default()
        .from(sender)
        .input(Bytes::from(code).into());
    let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
    assert!(receipt.status());
    Ok(receipt.contract_address.unwrap())
}

#[tokio::test]
async fn cargo_tangle_registers_generated_definition() -> Result {
    let scratch = tempfile::tempdir()?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let snapshot = blueprint_chain_setup_anvil::snapshot::default_snapshot_path();
    let broadcast: Value = serde_json::from_slice(&fs::read(
        snapshot.with_file_name("localtestnet-broadcast.json"),
    )?)?;
    let tangle = deployed(&broadcast, "Tangle", true);
    let staking = deployed(&broadcast, "MultiAssetDelegation", true);
    let status = deployed(&broadcast, "OperatorStatusRegistry", false);
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut anvil = Process(
        Command::new("anvil")
            .args([
                "--silent",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--load-state",
            ])
            .arg(snapshot)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let http = format!("http://127.0.0.1:{port}");
    let provider = ProviderBuilder::new().connect_http(http.parse()?);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if provider.get_chain_id().await.is_ok() {
                break Ok::<_, Box<dyn std::error::Error>>(());
            }
            if let Some(exit) = anvil.0.try_wait()? {
                return Err(format!("anvil exited {exit}").into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;

    let keystore_path = scratch.path().join("keystore");
    let keystore = Keystore::new(KeystoreConfig::new().fs_root(&keystore_path))?;
    let public = keystore.ecdsa_generate_new(None)?;
    let owner = Address::from_public_key(&public.0);
    provider
        .client()
        .request::<_, Value>("anvil_setBalance", (owner, "0x56bc75e2d63100000"))
        .await?;
    let sender = provider.get_accounts().await?[0];
    let vault = deploy(&provider, sender, bytecode(root, "GpuLeaseVault")?).await?;
    let mut bsm = bytecode(root, "GpuLeaseBlueprint")?;
    bsm.extend((vault, Address::ZERO).abi_encode());
    let manager = deploy(&provider, sender, bsm).await?;

    let binary = std::env::var_os("GPU_LEASE_OPERATOR_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/debug/gpu-lease-blueprint"))
        .canonicalize()?;
    let binary_hash = hex::encode(Sha256::digest(fs::read(&binary)?));
    let source = json!({
        "kind": "native", "fetcher": "none", "artifact_uri": format!("file://{}", binary.display()),
        "entrypoint": "gpu-lease-blueprint",
        "binaries": [{"name": "gpu-lease-blueprint", "arch": "x86_64", "os": "linux", "sha256": binary_hash}],
    });
    let sources = scratch.path().join("sources.json");
    fs::write(&sources, serde_json::to_vec(&vec![source])?)?;
    let definition = scratch.path().join("definition.json");
    let mut generator = Command::new(env!("CARGO_BIN_EXE_gpu-lease-blueprint-gen"));
    generator.args(["--manager", &manager.to_string()]);
    for (flag, path) in [
        ("--metadata-dir", scratch.path()),
        ("--definition", &definition),
        ("--sources", &sources),
    ] {
        generator.arg(flag).arg(path);
    }
    checked(&mut generator)?;
    let settings = scratch.path().join("settings.env");
    fs::write(
        &settings,
        format!(
            "BLUEPRINT_ID=0\nTANGLE_CONTRACT={tangle}\nSTAKING_CONTRACT={staking}\nSTATUS_REGISTRY_CONTRACT={status}\n"
        ),
    )?;
    let registry = ITangleBlueprints::new(tangle, &provider);
    let before = registry.blueprintCount().call().await?;
    let mut cli = Command::new("cargo");
    cli.args(["tangle", "blueprint", "deploy", "tangle"]);
    cli.args(["--network", "testnet"]);
    for (flag, value) in [
        ("--definition", definition.display().to_string()),
        ("--settings-file", settings.display().to_string()),
        ("--http-rpc-url", http),
        ("--ws-rpc-url", format!("ws://127.0.0.1:{port}")),
        ("--keystore-path", keystore_path.display().to_string()),
        ("--tangle-contract", tangle.to_string()),
        ("--staking-contract", staking.to_string()),
        ("--status-registry-contract", status.to_string()),
    ] {
        cli.arg(flag).arg(value);
    }
    checked(&mut cli)?;
    let id = registry.blueprintCount().call().await?;
    assert_eq!(id, before + 1);
    let registered = registry.getBlueprint(id).call().await?;
    assert_eq!(registered.owner, owner);
    assert_eq!(registered.manager, manager);
    let registered = registry.getBlueprintDefinition(id).call().await?;
    assert_eq!(registered.manager, manager);
    assert_eq!(
        registered
            .jobs
            .iter()
            .map(|job| job.name.as_str())
            .collect::<Vec<_>>(),
        ["lease", "release", "extend", "reap"]
    );
    assert_eq!(registered.config.minOperators, 1);
    assert_eq!(registered.config.maxOperators, 100);
    assert!(registered.hasConfig);
    assert_eq!(u8::from(registered.config.membership), 1);
    assert_eq!(u8::from(registered.config.pricing), 2);
    assert_eq!(
        registered.supportedMemberships,
        [registered.config.membership]
    );
    assert_eq!(registered.config.subscriptionRate, U256::ZERO);
    assert_eq!(registered.config.subscriptionInterval, 0);
    assert_eq!(
        registered.config.eventRate,
        U256::from(1_000_000_000_000_000u64)
    );
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
    assert_eq!(hex::encode(binary.sha256), binary_hash);
    let metadata = registry.blueprintMetadata(id).call().await?;
    let bytes = STANDARD.decode(
        metadata
            .metadataUri
            .strip_prefix("data:application/json;base64,")
            .unwrap(),
    )?;
    assert_eq!(bytes, fs::read(scratch.path().join("blueprint.json"))?);
    assert_eq!(metadata.metadataHash, keccak256(&bytes));
    assert_eq!(metadata.metadata.category, "Compute");
    assert_eq!(metadata.metadataHash, registered.metadataHash);
    assert_eq!(metadata.metadataUri, registered.metadataUri);
    println!(
        "cargo-tangle registration ok: chain={} fixture={} blueprint={id} manager={manager} metadataHash={}",
        provider.get_chain_id().await?,
        broadcast["commit"],
        metadata.metadataHash
    );
    Ok(())
}
