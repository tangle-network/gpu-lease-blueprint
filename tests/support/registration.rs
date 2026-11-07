//! Shared real Anvil + cargo-tangle registration fixture; no alternate registrar.
use blueprint_sdk::{
    alloy::{
        primitives::{Address, U256},
        providers::{Provider, ProviderBuilder, ext::AnvilApi},
        sol_types::SolValue,
        transports::http::reqwest::Url,
    },
    clients::tangle::{TangleClientConfig, TangleSettings, contracts::ITangleBlueprints},
    keystore::{Keystore, KeystoreConfig, backends::ecdsa::EcdsaBackend},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::Command;
#[path = "chain.rs"]
mod chain;
pub use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn checked(command: &mut Command) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        command.kill_on_drop(true).stdout(Stdio::null()).output(),
    )
    .await??;
    anyhow::ensure!(
        output.status.success(),
        "{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
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

pub struct Registered {
    pub scratch: tempfile::TempDir,
    pub broadcast: Value,
    pub http: String,
    pub tangle: Address,
    pub staking: Address,
    pub status: Address,
    pub owner: Address,
    pub keystore_path: PathBuf,
    pub vault: Address,
    pub manager: Address,
    pub id: u64,
    pub binary_hash: String,
    pub binary: PathBuf,
    _anvil: Process,
}

impl Registered {
    pub fn client_config(&self, keystore: &Path) -> Result<TangleClientConfig> {
        Ok(TangleClientConfig::new(
            self.http.parse::<Url>()?,
            self.http.replace("http://", "ws://").parse::<Url>()?,
            keystore.display().to_string(),
            TangleSettings {
                blueprint_id: self.id,
                tangle_contract: self.tangle,
                staking_contract: self.staking,
                status_registry_contract: self.status,
                ..Default::default()
            },
        ))
    }
}

pub async fn registered(root: &Path, generator: &Path) -> Result<Registered> {
    let scratch = tempfile::tempdir()?;
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
        std::process::Command::new("anvil")
            .args(["--silent", "--host", "127.0.0.1", "--load-state"])
            .arg(snapshot)
            .args(["--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let http = format!("http://127.0.0.1:{port}");
    let provider = ProviderBuilder::new().connect_http(http.parse()?);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if provider.get_chain_id().await.is_ok() {
                break Ok::<_, anyhow::Error>(());
            }
            if let Some(exit) = anvil.0.try_wait()? {
                return Err(anyhow::anyhow!("anvil exited {exit}"));
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
        .anvil_set_balance(owner, U256::from(100_000_000_000_000_000_000u128))
        .await?;
    let sender = provider.get_accounts().await?[0];
    let vault = chain::deploy(&provider, sender, "GpuLeaseVault", Vec::new()).await?;
    let verifier = chain::deploy(
        &provider,
        sender,
        "GpuLeaseEvidenceVerifier",
        sender.abi_encode(),
    )
    .await?;
    let manager = chain::deploy(
        &provider,
        sender,
        "GpuLeaseBlueprint",
        (vault, verifier).abi_encode(),
    )
    .await?;

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
    let mut generator = Command::new(generator);
    generator.args(["--manager", &manager.to_string()]);
    for (flag, path) in [
        ("--metadata-dir", scratch.path()),
        ("--definition", &definition),
        ("--sources", &sources),
    ] {
        generator.arg(flag).arg(path);
    }
    checked(&mut generator).await?;
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
    cli.args([
        "tangle",
        "blueprint",
        "deploy",
        "tangle",
        "--network",
        "testnet",
    ]);
    for (flag, value) in [
        ("--definition", definition.display().to_string()),
        ("--settings-file", settings.display().to_string()),
        ("--http-rpc-url", http.clone()),
        ("--ws-rpc-url", format!("ws://127.0.0.1:{port}")),
        ("--keystore-path", keystore_path.display().to_string()),
        ("--tangle-contract", tangle.to_string()),
        ("--staking-contract", staking.to_string()),
        ("--status-registry-contract", status.to_string()),
    ] {
        cli.arg(flag).arg(value);
    }
    checked(&mut cli).await?;
    assert_eq!(registry.blueprintCount().call().await?, before + 1);
    let view = registry.getBlueprint(before).call().await?;
    assert_eq!((view.owner, view.manager), (owner, manager));
    Ok(Registered {
        scratch,
        broadcast,
        http,
        tangle,
        staking,
        status,
        owner,
        keystore_path,
        vault,
        manager,
        id: before,
        binary_hash,
        binary,
        _anvil: anvil,
    })
}
