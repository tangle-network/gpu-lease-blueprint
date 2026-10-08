//! LOCAL DEMO — the TypeScript `gpuLease` resolver (agent-dev-container,
//! @tangle-network/sandbox) driving the REAL stack: anvil + seeded tnt-core,
//! the live operator (BlueprintRunner + operator API), and the deployed
//! GpuLeaseVault. This test is the merge gate: `scripts/run-demo.sh`.
//!
//! Orchestration:
//!   1. boot the harness (operator runner + chain) with GPU inventory
//!   2. serve the operator API on an ephemeral port
//!   3. deploy the real vault from forge artifacts
//!   4. mint a fresh demo buyer key, fund it, permit it on the service
//!   5. exec the TS demo (vitest, env-gated) in the SDK worktree
//!   6. assert the child exited 0

use alloy_rpc_types::TransactionRequest;
use anyhow::{Context, Result};
use blueprint_anvil_testing_utils::{BlueprintHarness, missing_tnt_core_artifacts};
use blueprint_sdk::alloy::primitives::{Address, Bytes, TxKind, U256};
use blueprint_sdk::alloy::providers::{Provider, ProviderBuilder};
use gpu_lease_blueprint_lib::api::operator_api_router;
use gpu_lease_blueprint_lib::router;
use once_cell::sync::Lazy;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

const DEMO_TIMEOUT: Duration = Duration::from_secs(600);
/// Operator quote-signing key (mirrors anvil.rs — the SDK verifies it client-side).
const QUOTE_SIGNING_KEY: &str = "4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d";
const GPU_INVENTORY: &str = r#"[{"id":"gpu-0","gpu_class":"h100","tee":false,"cuda_ordinal":0},{"id":"gpu-1","gpu_class":"h100","tee":false,"cuda_ordinal":1}]"#;
const VAULT_ARTIFACT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../contracts/out/GpuLeaseVault.sol/GpuLeaseVault.json"
);

static HARNESS_LOCK: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));

fn docker_socket_available() -> bool {
    let host = std::env::var("DOCKER_HOST").unwrap_or_default();
    if let Some(path) = host.strip_prefix("unix://") {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            return std::fs::metadata(path)
                .map(|m| m.file_type().is_socket())
                .unwrap_or(false);
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            return false;
        }
    }
    std::path::Path::new("/var/run/docker.sock").exists()
}

async fn raw(rpc: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let provider = ProviderBuilder::new().connect(rpc).await?;
    let owned = method.to_string();
    provider
        .raw_request::<_, serde_json::Value>(std::borrow::Cow::Owned(owned.clone()), params)
        .await
        .with_context(|| format!("raw {owned} failed"))
}

async fn impersonated_send(
    rpc: &str,
    from: Address,
    to: Option<Address>,
    input: Vec<u8>,
    value: U256,
) -> Result<()> {
    raw(
        rpc,
        "anvil_impersonateAccount",
        serde_json::json!([format!("{from:#x}")]),
    )
    .await?;
    let provider = ProviderBuilder::new().connect(rpc).await?;
    let mut tx = TransactionRequest::default();
    tx.from = Some(from);
    tx.input = Bytes::from(input).into();
    tx.value = Some(value);
    tx.to = to.map(TxKind::Call);
    let receipt = provider
        .send_transaction(tx)
        .await
        .context("send failed")?
        .get_receipt()
        .await
        .context("no receipt")?;
    anyhow::ensure!(receipt.status(), "impersonated tx reverted");
    Ok(())
}

fn demo_sdk_dir() -> std::path::PathBuf {
    std::env::var("DEMO_SDK_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            // Default: sibling worktree of this machine.
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../agent-dev-container-gpu-lease/products/sandbox/sdk"
            )
            .into()
        })
}

#[test]
fn demo_ts_resolver_drives_real_stack() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(64 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("runtime");
    if let Err(e) = rt.block_on(run_demo()) {
        panic!("demo failed: {e:?}");
    }
}

async fn run_demo() -> Result<()> {
    let guard = HARNESS_LOCK.lock().await;
    let result = timeout(DEMO_TIMEOUT, async {
        if !docker_socket_available() {
            eprintln!("skipping demo: docker socket unreachable (macOS+colima: set DOCKER_HOST)");
            return Ok(());
        }
        let artifact = std::path::Path::new(VAULT_ARTIFACT);
        if !artifact.exists() {
            eprintln!("skipping demo: vault artifact missing — run `forge build`");
            return Ok(());
        }
        let sdk_dir = demo_sdk_dir();
        if !sdk_dir.join("package.json").exists() {
            eprintln!(
                "skipping demo: SDK dir {} missing (clone agent-dev-container worktree or set DEMO_SDK_DIR)",
                sdk_dir.display()
            );
            return Ok(());
        }

        // 1-2. Boot the operator + chain; serve the operator API.
        let harness = BlueprintHarness::builder(router())
            .poll_interval(Duration::from_millis(50))
            .with_env_var("GPU_INVENTORY_JSON", GPU_INVENTORY)
            .with_env_var("GPU_QUOTE_SIGNING_KEY", QUOTE_SIGNING_KEY)
            .spawn()
            .await
            .map_err(|e| {
                if missing_tnt_core_artifacts(&e) {
                    eprintln!("skipping demo: {e}");
                }
                e
            })?;

        let rpc = harness.environment().http_rpc_endpoint.to_string();
        let tangle = harness.deployment().tangle_contract;
        let service_id: u64 = harness.service_id();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let operator_port = listener.local_addr()?.port();
        let operator_url = format!("http://127.0.0.1:{operator_port}");
        tokio::spawn(async move {
            axum::serve(listener, operator_api_router()).await.ok();
        });

        // 3. Deploy the real vault.
        let artifact_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(artifact)?)?;
        let bytecode = hex::decode(
            artifact_json["bytecode"]["object"]
                .as_str()
                .context("bytecode missing")?
                .trim_start_matches("0x"),
        )?;
        let deployer: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".parse()?;
        raw(&rpc, "anvil_setBalance", serde_json::json!([format!("{deployer:#x}"), "0x3635c9adc5dea0000000000"])).await?;
        let provider = ProviderBuilder::new().connect(&rpc).await?;
        let mut tx = TransactionRequest::default();
        tx.from = Some(deployer);
        tx.input = Bytes::from(bytecode).into();
        let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
        let vault = receipt.contract_address.context("no vault address")?;
        eprintln!("[demo] vault deployed at {vault:#x}");

        // 4. Fresh buyer key (deterministic demo key), funded + permitted on the service.
        // The service owner (harness caller) permits job submitters.
        let service_owner = harness.caller_account();
        // Broker treasury key (anvil well-known #5). The address is DERIVED
        // with viem itself — never hand-copied.
        const DEMO_TREASURY_KEY: &str = "0x8166f546333643e517fd0b7dcf8b3f23fbfbd3ae5f2ea5c34bf5e58b37f07f56";
        let treasury: Address = derive_address_with_sdk(&sdk_dir, DEMO_TREASURY_KEY).await?;
        raw(&rpc, "anvil_setBalance", serde_json::json!([format!("{treasury:#x}"), "0x3635c9adc5dea0000000000"])).await?;
        let permit_treasury = gpu_lease_demo_abi::encode_add_permitted_caller(service_id, treasury);
        impersonated_send(&rpc, service_owner, Some(tangle), permit_treasury, U256::ZERO).await?;
        eprintln!("[demo] broker treasury {treasury:#x} funded + permitted");

        // Deterministic demo key (anvil well-known list). The address is DERIVED
        // with viem itself — never hand-copied, never drifted.
        const DEMO_BUYER_KEY: &str = "0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba";
        let buyer: Address = derive_address_with_sdk(&sdk_dir, DEMO_BUYER_KEY).await?;
        raw(&rpc, "anvil_setBalance", serde_json::json!([format!("{buyer:#x}"), "0x3635c9adc5dea0000000000"])).await?;
        let permit = gpu_lease_demo_abi::encode_add_permitted_caller(service_id, buyer);
        impersonated_send(&rpc, service_owner, Some(tangle), permit, U256::ZERO).await?;
        eprintln!("[demo] buyer {buyer:#x} permitted on service {service_id}");

        // The vault lease names the operator: use the harness operator account
        // (the runner's operator) so BSM/result binding would match. The
        // operator API carries the identity for the quote flow.
        // For the demo the money sink is the address the resolver gets from the quote envelope.
        let operator_address: Address = "0x00000000000000000000000000000000000000b1".parse()?;
        let _ = operator_address;

        // 5. Exec the TS demo child.
        let status = std::process::Command::new("corepack")
            .arg("pnpm")
            .arg("vitest")
            .arg("run")
            .arg("tests/unit/gpu-lease-demo.test.ts")
            .arg("--disableConsoleIntercept")
            .current_dir(&sdk_dir)
            .env("DEMO_GPU_LEASE", "1")
            .env("DEMO_RPC_URL", &rpc)
            .env("DEMO_OPERATOR_URL", &operator_url)
            .env("DEMO_VAULT_ADDRESS", format!("{vault:#x}"))
            .env("DEMO_TANGLE_ADDRESS", format!("{tangle:#x}"))
            .env("DEMO_SERVICE_ID", service_id.to_string())
            .env("DEMO_BUYER_KEY", DEMO_BUYER_KEY)
            .env("DEMO_OPERATOR_ADDRESS", format!("{operator_address:#x}"))
            .env("DEMO_DURATION_SECONDS", "600")
            .status()
            .context("failed to spawn `corepack pnpm vitest` (is the SDK worktree installed?)")?;
        anyhow::ensure!(status.success(), "TS demo failed with {status}");

        // ── Sponsored (broker) leg: the treasury drives the same resolver with
        //     the FromPrivateKey transport — the USD rail's server-side proof.
        let broker_status = std::process::Command::new("corepack")
            .arg("pnpm")
            .arg("vitest")
            .arg("run")
            .arg("tests/unit/gpu-lease-broker-demo.test.ts")
            .arg("--disableConsoleIntercept")
            .current_dir(&sdk_dir)
            .env("DEMO_BROKER", "1")
            .env("DEMO_RPC_URL", &rpc)
            .env("DEMO_OPERATOR_URL", &operator_url)
            .env("DEMO_VAULT_ADDRESS", format!("{vault:#x}"))
            .env("DEMO_TANGLE_ADDRESS", format!("{tangle:#x}"))
            .env("DEMO_SERVICE_ID", service_id.to_string())
            .env("DEMO_TREASURY_KEY", DEMO_TREASURY_KEY)
            .env("DEMO_OPERATOR_ADDRESS", format!("{operator_address:#x}"))
            .env("DEMO_DURATION_SECONDS", "600")
            .status()
            .context("failed to spawn broker demo child")?;
        anyhow::ensure!(broker_status.success(), "broker demo failed with {broker_status}");

        harness.shutdown().await;
        Ok(())
    })
    .await;

    drop(guard);
    result.context("demo timed out")?
}

/// Derive an address from a private key using the SDK worktree's own viem
/// (single source of truth for key->address).
async fn derive_address_with_sdk(sdk_dir: &std::path::Path, key: &str) -> Result<Address> {
    let program = format!(
        "const {{ privateKeyToAccount }} = await import('viem/accounts'); \
         console.log(privateKeyToAccount('{key}').address);"
    );
    let out = std::process::Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg(&program)
        .current_dir(sdk_dir)
        .output()
        .context("node derivation failed")?;
    anyhow::ensure!(
        out.status.success(),
        "derive: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let addr = String::from_utf8_lossy(&out.stdout).trim().to_string();
    addr.parse()
        .with_context(|| format!("derived address {addr}"))
}

mod gpu_lease_demo_abi {
    /// addPermittedCaller(uint64,address) selector + args.
    pub fn encode_add_permitted_caller(
        service_id: u64,
        caller: blueprint_sdk::alloy::primitives::Address,
    ) -> Vec<u8> {
        use tiny_keccak::{Hasher, Keccak};
        let mut k = Keccak::v256();
        k.update(b"addPermittedCaller(uint64,address)");
        let mut out = [0u8; 32];
        k.finalize(&mut out);
        let mut calldata = out[..4].to_vec();
        calldata.extend_from_slice(&[0u8; 24]);
        calldata.extend_from_slice(&service_id.to_be_bytes());
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(caller.as_slice());
        calldata
    }
}
