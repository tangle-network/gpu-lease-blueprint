//! A paid lease crosses two real operator processes and one real Anvil chain.
use anyhow::{Context, Result, ensure};
use axum::{body::Body, http::Request};
use blueprint_sdk::{
    JobCall,
    alloy::{
        primitives::{Address, B256, Bytes, U256},
        providers::{Provider, ProviderBuilder},
        rpc::types::{TransactionReceipt, TransactionRequest},
        signers::local::PrivateKeySigner,
        sol,
        sol_types::{SolCall, SolEvent, SolValue},
    },
    tangle::extract::{CallId, Caller, ServiceId},
};
use gpu_lease_blueprint_lib::{GpuLeaseOutput, GpuLeaseRequest, JOB_LEASE, api, jobs};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tower::ServiceExt;

const DURATION: u64 = 3_600;
const PRICE: u128 = 1_000_000;
const INVENTORY: &str = r#"[{"id":"gpu-a","gpu_class":"h100","tee":false,"cuda_ordinal":2},{"id":"gpu-b","gpu_class":"h100","tee":false,"cuda_ordinal":0}]"#;

sol! {
    interface Fixture {
        function create(address operator, uint128 pricePerSecond, uint64 durationSeconds, uint16 deviceCount, bytes32 intentHash, uint8 confidentiality, bytes endpointInfo) external payable returns (bytes32);
        function onBlueprintCreated(uint64 blueprintId, address owner, address tangleCore) external;
        function onJobCall(uint64 serviceId, uint8 job, uint64 callId, bytes inputs) external payable;
        function onJobResult(uint64 serviceId, uint8 job, uint64 callId, address operator, bytes32 inputsHash, bytes outputs) external payable;
        event LeaseCreated(bytes32 indexed leaseId, address indexed operator, address indexed lessee, uint128 escrow, uint128 pricePerSecond, uint16 deviceCount, uint64 expiry, bytes32 intentHash, uint8 confidentiality, bytes endpointInfo, uint16 schemaVersion);
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn send(
    rpc: &str,
    from: Address,
    to: Option<Address>,
    data: Vec<u8>,
    value: U256,
) -> Result<TransactionReceipt> {
    let provider = ProviderBuilder::new().connect(rpc).await?;
    let tx = TransactionRequest::default()
        .from(from)
        .input(Bytes::from(data).into())
        .value(value);
    let tx = if let Some(to) = to { tx.to(to) } else { tx };
    let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
    ensure!(receipt.status(), "normal fixture transaction reverted");
    Ok(receipt)
}

fn bytecode(name: &str) -> Result<Vec<u8>> {
    let object = if let Ok(path) = std::env::var("GPU_LEASE_CONTRACT_ARTIFACTS") {
        let artifact: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        artifact["contracts"][format!("contracts/src/{name}.sol")][name]["evm"]["bytecode"]["object"].clone()
    } else {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../contracts/out/{name}.sol/{name}.json"));
        let artifact: Value = serde_json::from_slice(
            &std::fs::read(path).context("run forge build or set GPU_LEASE_CONTRACT_ARTIFACTS")?,
        )?;
        artifact["bytecode"]["object"].clone()
    };
    Ok(hex::decode(
        object
            .as_str()
            .context("missing contract bytecode")?
            .trim_start_matches("0x"),
    )?)
}

#[tokio::test]
async fn paid_lease_survives_operator_restart() -> Result<()> {
    let socket = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = socket.local_addr()?.port();
    drop(socket);
    let mut anvil = Process(
        Command::new("anvil")
            .args(["--port", &port.to_string(), "--silent"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start anvil")?,
    );
    let rpc = format!("http://127.0.0.1:{port}");
    let provider = ProviderBuilder::new().connect(&rpc).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while provider.get_chain_id().await.is_err() {
        ensure!(
            Instant::now() < deadline && anvil.0.try_wait()?.is_none(),
            "anvil did not start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let buyer = provider.get_accounts().await?[0];
    let signer = PrivateKeySigner::random();
    let operator = signer.address();
    let vault = send(&rpc, buyer, None, bytecode("GpuLeaseVault")?, U256::ZERO)
        .await?
        .contract_address
        .context("vault address")?;
    let mut code = bytecode("GpuLeaseBlueprint")?;
    code.extend((vault, Address::ZERO).abi_encode_params());
    let blueprint = send(&rpc, buyer, None, code, U256::ZERO)
        .await?
        .contract_address
        .context("blueprint address")?;
    send(
        &rpc,
        buyer,
        Some(blueprint),
        Fixture::onBlueprintCreatedCall {
            blueprintId: 1,
            owner: buyer,
            tangleCore: buyer,
        }
        .abi_encode(),
        U256::ZERO,
    )
    .await?;
    let intent = B256::from(jobs::intent_hash(2, "h100", DURATION, 0, "any", 2));
    let receipt = send(
        &rpc,
        buyer,
        Some(vault),
        Fixture::createCall {
            operator,
            pricePerSecond: PRICE,
            durationSeconds: DURATION,
            deviceCount: 2,
            intentHash: intent,
            confidentiality: 0,
            endpointInfo: Bytes::new(),
        }
        .abi_encode(),
        U256::from(PRICE) * U256::from(DURATION) * U256::from(2),
    )
    .await?;
    let created = receipt
        .inner
        .logs()
        .iter()
        .find_map(|log| Fixture::LeaseCreated::decode_log(&log.inner).ok())
        .context("LeaseCreated event")?
        .data;
    ensure!(
        created.escrow == PRICE * u128::from(DURATION) * 2,
        "lease must be paid"
    );
    let dir = std::env::temp_dir().join(format!(
        "gpu-lease-persistence-{}-{}",
        std::process::id(),
        created.leaseId
    ));
    std::fs::create_dir_all(&dir)?;
    let mut child = Command::new(std::env::current_exe()?);
    child
        .args(["--exact", "operator_process", "--ignored", "--nocapture"])
        .env("HTTP_RPC_URL", &rpc)
        .env("GPU_LEASE_VAULT_ADDRESS", vault.to_string())
        .env("GPU_LEASE_BLUEPRINT_ADDRESS", blueprint.to_string())
        .env("GPU_OPERATOR_ADDRESS", operator.to_string())
        .env("GPU_QUOTE_SIGNING_KEY", hex::encode(signer.to_bytes()))
        .env("GPU_LEASE_STATE_PATH", dir.join("leases.json"))
        .env(
            "GPU_QUOTE_POLICY_JSON",
            serde_json::to_string(&gpu_lease_blueprint_lib::QuotePolicy {
                base_price_per_second: [("h100".into(), PRICE)].into(),
                ..Default::default()
            })?,
        )
        .env("PERSISTENCE_LEASE", created.leaseId.to_string())
        .env("PERSISTENCE_BUYER", buyer.to_string());
    let first = run_operator(&mut child, "create", INVENTORY).await?;
    let mut reordered: Vec<Value> = serde_json::from_str(INVENTORY)?;
    reordered.reverse();
    let second = run_operator(&mut child, "restore", &serde_json::to_string(&reordered)?).await?;
    ensure!(
        first == second,
        "public lease status changed after process restart: {first} -> {second}"
    );
    ensure!(second["deviceIds"] == json!(["gpu-a", "gpu-b"]));
    ensure!(
        serde_json::from_str::<Value>(second["endpoint"].as_str().context("endpoint")?)?
            == json!({"devices":[2,0],"transport":"local-attach","v":1})
    );
    ensure!(second["expiresAt"] == created.expiry);
    println!(
        "PERSISTENCE_PROOF={}",
        json!({"paidEscrow":created.escrow.to_string(),"expiry":created.expiry,"before":first,"after":second})
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

async fn run_operator(command: &mut Command, phase: &str, inventory: &str) -> Result<Value> {
    let mut probe;
    let command = if let Some(binary) =
        std::env::var_os(format!("GPU_LEASE_{}_PROBE", phase.to_uppercase()))
    {
        probe = Command::new(binary);
        probe.args(command.get_args());
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                probe.env(key, value);
            }
        }
        &mut probe
    } else {
        command
    };
    let mut process = Process(
        command
            .env("PERSISTENCE_PHASE", phase)
            .env("GPU_INVENTORY_JSON", inventory)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if process.0.try_wait()?.is_some() {
            break;
        }
        ensure!(Instant::now() < deadline, "operator {phase} timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut stdout = String::new();
    let mut stderr = String::new();
    use std::io::Read;
    process
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)?;
    process
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)?;
    ensure!(
        process.0.wait()?.success(),
        "operator {phase} failed: {stdout}\n{stderr}"
    );
    println!(
        "PERSISTENCE_PROCESS phase={phase} pid={} exit=0",
        process.0.id()
    );
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("PERSISTENCE_STATUS="))
        .context("operator status output")?;
    Ok(serde_json::from_str(line)?)
}

#[tokio::test]
#[ignore = "child process entrypoint for paid_lease_survives_operator_restart"]
async fn operator_process() -> Result<()> {
    gpu_lease_blueprint_lib::initialize()
        .await
        .map_err(anyhow::Error::msg)?;
    let lease_id: B256 = std::env::var("PERSISTENCE_LEASE")?.parse()?;
    if std::env::var("PERSISTENCE_PHASE")? == "create" {
        let buyer: Address = std::env::var("PERSISTENCE_BUYER")?.parse()?;
        let request = GpuLeaseRequest {
            intentVersion: 2,
            intentHash: jobs::intent_hash(2, "h100", DURATION, 0, "any", 2).into(),
            pricePerSecond: PRICE,
            durationSeconds: DURATION,
            confidentiality: 0,
            gpuClass: "h100".into(),
            region: "any".into(),
            lessee: buyer,
            leaseId: lease_id,
            sandboxId: B256::ZERO,
            sandboxTeeType: 0,
            deviceCount: 2,
        };
        let mut call = JobCall::new(JOB_LEASE, request.abi_encode());
        call.metadata_mut()
            .insert(Caller::METADATA_KEY, buyer.into_array());
        call.metadata_mut().insert(CallId::METADATA_KEY, 1u64);
        call.metadata_mut().insert(ServiceId::METADATA_KEY, 1u64);
        let results = gpu_lease_blueprint_lib::router()
            .oneshot(call)
            .await
            .map_err(anyhow::Error::msg)?
            .context("router result")?;
        let body = results[0]
            .body()
            .map_err(|e| anyhow::anyhow!("lease job: {e}"))?;
        let output = GpuLeaseOutput::abi_decode(body)?;
        ensure!(output.leaseId == lease_id);
        let rpc = std::env::var("HTTP_RPC_URL")?;
        let blueprint = std::env::var("GPU_LEASE_BLUEPRINT_ADDRESS")?.parse()?;
        send(
            &rpc,
            buyer,
            Some(blueprint),
            Fixture::onJobCallCall {
                serviceId: 1,
                job: JOB_LEASE,
                callId: 1,
                inputs: request.abi_encode().into(),
            }
            .abi_encode(),
            U256::ZERO,
        )
        .await?;
        send(
            &rpc,
            buyer,
            Some(blueprint),
            Fixture::onJobResultCall {
                serviceId: 1,
                job: JOB_LEASE,
                callId: 1,
                operator: std::env::var("GPU_OPERATOR_ADDRESS")?.parse()?,
                inputsHash: blueprint_sdk::alloy::primitives::keccak256(request.abi_encode()),
                outputs: body.to_vec().into(),
            }
            .abi_encode(),
            U256::ZERO,
        )
        .await?;
        gpu_lease_blueprint_lib::chain::reconcile(false)
            .await
            .map_err(anyhow::Error::msg)?;
    }
    let response = api::operator_api_router()
        .oneshot(
            Request::builder()
                .uri(format!("/api/leases/{lease_id}"))
                .body(Body::empty())?,
        )
        .await?;
    ensure!(
        response.status().is_success(),
        "lease status: {}",
        response.status()
    );
    let body = axum::body::to_bytes(response.into_body(), 16_384).await?;
    println!("PERSISTENCE_STATUS={}", std::str::from_utf8(&body)?);
    Ok(())
}
