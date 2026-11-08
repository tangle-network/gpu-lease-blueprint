//! Full-stack proof: generated registration, real GPU BSM, SDK runner and Sandbox client.
use anyhow::{Context, Result, ensure};
use blueprint_anvil_testing_utils::blueprint::seed_operator_key;
use blueprint_sdk::{
    alloy::{
        primitives::{Address, Bytes, U256},
        providers::{Provider, ProviderBuilder, ext::AnvilApi},
        signers::local::PrivateKeySigner,
    },
    clients::tangle::{TangleClient, services::ServiceRequestParams},
    crypto::BytesEncoding,
    keystore::{Keystore, KeystoreConfig},
    runner::{
        BlueprintRunner,
        config::{BlueprintEnvironment, ProtocolSettings},
        tangle::config::{TangleConfig, TangleProtocolSettings},
    },
    tangle::{TangleConsumer, TangleProducer},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

#[path = "../../tests/support/registration.rs"]
mod fixture;

// Anvil's public development account, used only on this fixture's isolated chain.
const BUYER_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
struct Tasks(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
struct Environment(Vec<(String, Option<std::ffi::OsString>)>);
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo_ts_resolver_drives_real_stack() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(600), run_demo())
        .await
        .context("funded demo timed out")?
}

async fn run_demo() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let sdk = PathBuf::from(
        std::env::var("DEMO_SDK_DIR").context("DEMO_SDK_DIR must name the updated Sandbox SDK")?,
    );
    ensure!(
        sdk.join("package.json").is_file(),
        "Sandbox SDK checkout is unavailable"
    );
    let generator = std::env::var_os("GPU_LEASE_GENERATOR_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/debug/gpu-lease-blueprint-gen"));
    let f = fixture::registered(root, &generator).await?;
    let provider = ProviderBuilder::new().connect(&f.http).await?;
    let owner = TangleClient::new(f.client_config(&f.keystore_path)?.test_mode(true)).await?;
    let operator_path = f.scratch.path().join("operator");
    seed_operator_key(&operator_path)?;
    let operator = TangleClient::with_keystore(
        f.client_config(&operator_path)?.test_mode(true),
        Keystore::new(KeystoreConfig::new().fs_root(&operator_path))?,
    )
    .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let operator_url = format!("http://{}", listener.local_addr()?);
    operator
        .register_operator(f.id, &operator_url, None)
        .await?;
    let buyer: PrivateKeySigner = BUYER_KEY.parse()?;
    provider
        .anvil_set_balance(buyer.address(), U256::from(100_000_000_000_000_000_000u128))
        .await?;
    let (_, request) = owner
        .request_service(ServiceRequestParams::new(
            f.id,
            vec![operator.account()],
            vec![buyer.address()],
            Bytes::new(),
            3_600,
            Address::ZERO,
            U256::ZERO,
        ))
        .await?;
    let service_id = owner.service_count().await?;
    operator.approve_service(request).await?;
    ensure!(
        owner.service_count().await? == service_id + 1,
        "service approval did not create one service"
    );
    ensure!(
        owner.get_service_info(service_id).await?.blueprint_id == f.id,
        "service is registered to another blueprint"
    );
    ensure!(
        owner.get_blueprint_manager(service_id).await? == Some(f.manager),
        "service does not use the deployed GPU BSM"
    );

    let policy = gpu_lease_blueprint_lib::QuotePolicy {
        base_price_per_second: [("nvidia-h100".into(), 3)].into(),
        utilization_curve: vec![(0, 10_000)],
        max_duration_seconds: 3_600,
        ..Default::default()
    };
    let vars = [
        ("HTTP_RPC_URL", f.http.clone()),
        ("GPU_LEASE_VAULT_ADDRESS", format!("{:#x}", f.vault)),
        ("GPU_LEASE_BLUEPRINT_ADDRESS", format!("{:#x}", f.manager)),
        ("GPU_OPERATOR_ADDRESS", format!("{:#x}", operator.account())),
        (
            "GPU_LEASE_STATE_PATH",
            f.scratch.path().join("leases.json").display().to_string(),
        ),
        (
            "GPU_INVENTORY_JSON",
            r#"[{"id":"fixture-gpu","gpu_class":"nvidia-h100","tee":false,"cuda_ordinal":0}]"#
                .into(),
        ),
        ("GPU_QUOTE_POLICY_JSON", serde_json::to_string(&policy)?),
        (
            "GPU_QUOTE_SIGNING_KEY",
            hex::encode(operator.ecdsa_signing_key()?.to_bytes()),
        ),
    ];
    // This integration target owns its process environment; readers start after initialization.
    let _environment = Environment(
        vars.iter()
            .map(|(key, _)| (key.to_string(), std::env::var_os(key)))
            .collect(),
    );
    for (key, value) in vars {
        unsafe {
            std::env::set_var(key, value);
        }
    }
    gpu_lease_blueprint_lib::initialize()
        .await
        .map_err(anyhow::Error::msg)?;
    let mut environment = BlueprintEnvironment::default();
    environment.http_rpc_endpoint = f.http.parse()?;
    environment.ws_rpc_endpoint = f.http.replace("http://", "ws://").parse()?;
    environment.keystore_uri = operator_path.display().to_string();
    environment.data_dir = f.scratch.path().join("runner");
    environment.test_mode = true;
    environment.protocol_settings = ProtocolSettings::Tangle(TangleProtocolSettings {
        blueprint_id: f.id,
        service_id: Some(service_id),
        tangle_contract: f.tangle,
        staking_contract: f.staking,
        status_registry_contract: f.status,
    });
    let producer = TangleProducer::from_block(
        operator.clone(),
        service_id,
        provider.get_block_number().await?,
    )
    .with_poll_interval(Duration::from_millis(50));
    let mut tasks = Tasks(Vec::new());
    tasks.0.push(tokio::spawn(async move {
        BlueprintRunner::builder(TangleConfig::default(), environment)
            .router(gpu_lease_blueprint_lib::router())
            .producer(producer)
            .consumer(gpu_lease_blueprint_lib::consumer::LeaseConsumer::new(
                TangleConsumer::new(operator),
            ))
            .run()
            .await
            .expect("registered GPU operator runner failed");
    }));
    tasks.0.push(tokio::spawn(async move {
        axum::serve(
            listener,
            gpu_lease_blueprint_lib::api::operator_api_router(),
        )
        .await
        .expect("operator API failed");
    }));
    let status = Command::new("corepack")
        .kill_on_drop(true)
        .args([
            "pnpm",
            "exec",
            "vitest",
            "run",
            "tests/unit/gpu-lease-demo.test.ts",
        ])
        .current_dir(sdk)
        .env("DEMO_GPU_LEASE", "1")
        .env("DEMO_RPC_URL", &f.http)
        .env("DEMO_OPERATOR_URL", operator_url)
        .env("DEMO_VAULT_ADDRESS", format!("{:#x}", f.vault))
        .env("DEMO_BLUEPRINT_ADDRESS", format!("{:#x}", f.manager))
        .env("DEMO_TANGLE_ADDRESS", format!("{:#x}", f.tangle))
        .env("DEMO_SERVICE_ID", service_id.to_string())
        .env("DEMO_BUYER_KEY", BUYER_KEY)
        .env(
            "DEMO_OPERATOR_ADDRESS",
            std::env::var("GPU_OPERATOR_ADDRESS").unwrap(),
        )
        .env("DEMO_DURATION_SECONDS", "600")
        .status()
        .await?;
    ensure!(
        status.success(),
        "Sandbox client funded demo failed: {status}"
    );
    Ok(())
}
