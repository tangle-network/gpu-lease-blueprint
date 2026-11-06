//! Operator integration proof: public SDK Router + HTTP API + real vault/BSM on Anvil.
use anyhow::{Context, Result, ensure};
use axum::{body::Body, http::Request};
use blueprint_sdk::{
    JobCall, JobResult,
    alloy::{
        primitives::{Address, Bytes, U256, keccak256},
        providers::{Provider, ProviderBuilder},
        rpc::types::TransactionRequest,
        sol,
        sol_types::{SolCall, SolEvent, SolValue},
    },
};
use gpu_lease_blueprint_lib::{
    GpuLeaseExtendRequest, GpuLeaseIdRequest, GpuLeaseOutput, GpuLeaseRequest, router,
};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};
use tower::ServiceExt;

sol! {
    interface Vault {
        function create(address operator,uint128 price,uint64 duration,uint16 count,bytes32 intent,uint8 confidentiality,bytes endpoint) external payable returns(bytes32);
        function extend(bytes32 id,uint64 seconds_) external payable;
        event LeaseCreated(bytes32 indexed leaseId,address indexed operator,address indexed lessee,uint128 escrow,uint128 pricePerSecond,uint16 deviceCount,uint64 expiry,bytes32 intentHash,uint8 confidentiality,bytes endpointInfo,uint16 schemaVersion);
    }
    interface Bsm {
        function onBlueprintCreated(uint64 id,address owner,address tangle) external;
        function onJobCall(uint64 service,uint8 job,uint64 callId,bytes inputs) external payable;
        function onJobResult(uint64 service,uint8 job,uint64 callId,address operator,bytes32 inputsHash,bytes outputs) external payable;
    }
}
struct Anvil(Child);
impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn call<T: SolValue>(job: u8, input: T, caller: Address) -> Result<Vec<JobResult>> {
    let mut request = JobCall::new(job, input.abi_encode());
    request
        .metadata_mut()
        .insert(blueprint_sdk::tangle::extract::CallId::METADATA_KEY, 0u64);
    request.metadata_mut().insert(
        blueprint_sdk::tangle::extract::ServiceId::METADATA_KEY,
        0u64,
    );
    request.metadata_mut().insert(
        blueprint_sdk::tangle::extract::Caller::METADATA_KEY,
        caller.0.0,
    );
    let results = router()
        .oneshot(request)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .context("router did not match job")?;
    ensure!(!results.is_empty(), "router did not execute handler");
    Ok(results)
}
fn rejected(r: &[JobResult]) -> bool {
    r.iter().all(JobResult::is_err)
}
async fn http(
    method: &str,
    uri: &str,
    body: serde_json::Value,
    token: Option<&str>,
) -> Result<(u16, serde_json::Value)> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = gpu_lease_blueprint_lib::api::operator_api_router()
        .oneshot(request.body(Body::from(body.to_string()))?)
        .await?;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000).await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

async fn tx<P: Provider>(
    p: &P,
    from: Address,
    to: Option<Address>,
    input: Vec<u8>,
    value: U256,
) -> Result<blueprint_sdk::alloy::rpc::types::TransactionReceipt> {
    let mut request = TransactionRequest::default();
    request.from = Some(from);
    request.input = Bytes::from(input).into();
    request.value = Some(value);
    request.to = to.map(blueprint_sdk::alloy::primitives::TxKind::Call);
    let r = p.send_transaction(request).await?.get_receipt().await?;
    ensure!(r.status(), "transaction reverted");
    Ok(r)
}

async fn reverted_result_frees_reservation<P: Provider>(
    p: &P,
    rpc: &str,
    owner: Address,
    vault: Address,
    bsm: Address,
    mut request: GpuLeaseRequest,
) -> Result<()> {
    use blueprint_sdk::{
        clients::tangle::{TangleClient, TangleClientConfig, TangleSettings},
        crypto::k256::K256Ecdsa,
        keystore::{Keystore, KeystoreConfig, backends::Backend},
    };
    use futures_util::SinkExt;
    let receipt = tx(
        p,
        owner,
        Some(vault),
        Vault::createCall {
            operator: request_operator(),
            price: 3,
            duration: 600,
            count: 1,
            intent: request.intentHash,
            confidentiality: 0,
            endpoint: Bytes::new(),
        }
        .abi_encode(),
        U256::from(1800),
    )
    .await?;
    request.leaseId = receipt
        .logs()
        .iter()
        .find_map(|l| {
            Vault::LeaseCreated::decode_log(&l.inner)
                .ok()
                .map(|v| v.data.leaseId)
        })
        .context("LeaseCreated")?;
    let mut results = call(0, request.clone(), owner).await?;
    ensure!(results[0].is_ok(), "funded result reservation failed");
    let keystore = Keystore::new(KeystoreConfig::new().in_memory(true))?;
    keystore.generate::<K256Ecdsa>(None)?;
    let config = TangleClientConfig::new(
        rpc.parse::<blueprint_sdk::alloy::transports::http::reqwest::Url>()?,
        rpc.replace("http://", "ws://")
            .parse::<blueprint_sdk::alloy::transports::http::reqwest::Url>()?,
        "memory://",
        TangleSettings {
            tangle_contract: bsm,
            ..Default::default()
        },
    );
    let client = TangleClient::with_keystore(config, keystore).await?;
    p.raw_request::<_, serde_json::Value>(
        "anvil_setBalance".into(),
        serde_json::json!([format!("{:#x}", client.account()), "0x3635c9adc5dea00000"]),
    )
    .await?;
    let before = p.get_block_number().await?;
    let mut consumer = gpu_lease_blueprint_lib::consumer::LeaseConsumer::new(
        blueprint_sdk::tangle::TangleConsumer::new(client),
    );
    ensure!(
        consumer.send(results.remove(0)).await.is_err(),
        "invalid result submission did not revert"
    );
    ensure!(
        p.get_block_number().await? > before,
        "result rejection was not an actual mined reverted transaction"
    );
    ensure!(
        gpu_lease_blueprint_lib::allocator()
            .allocation(request.leaseId.0)
            .is_none(),
        "reverted result leaked hardware reservation"
    );
    Ok(())
}
fn request_operator() -> Address {
    std::env::var("GPU_OPERATOR_ADDRESS")
        .unwrap()
        .parse()
        .unwrap()
}

async fn restart_from_journal(lease_id: blueprint_sdk::alloy::primitives::B256) -> Result<()> {
    let path = std::env::var("GPU_LEASE_STATE_PATH")?;
    let recovered = format!("{path}.restart");
    std::fs::copy(path, &recovered)?;
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "restart_reconciles_persisted_lease",
            "--nocapture",
        ])
        .env("GPU_LEASE_STATE_PATH", recovered)
        .env("GPU_LEASE_RESTART_PROBE", format!("{lease_id:#x}"))
        .output()?;
    ensure!(
        output.status.success(),
        "restarted operator failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
#[tokio::test]
async fn restart_reconciles_persisted_lease() -> Result<()> {
    let Ok(id) = std::env::var("GPU_LEASE_RESTART_PROBE") else {
        return Ok(());
    };
    gpu_lease_blueprint_lib::chain::reconcile(true)
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(
        gpu_lease_blueprint_lib::allocator().idle_count("h100", false) == 0,
        "restart lost paid allocation"
    );
    let (status, lease) = http(
        "GET",
        &format!("/api/leases/{id}"),
        serde_json::Value::Null,
        None,
    )
    .await?;
    ensure!(
        status == 200 && lease["deviceIds"] == serde_json::json!(["d0"]),
        "restart changed allocation"
    );
    ensure!(
        http(
            "GET",
            &format!("/api/leases/{id}/access"),
            serde_json::Value::Null,
            None
        )
        .await?
        .0 == 401,
        "restart bypasses credential gate"
    );
    Ok(())
}

#[tokio::test]
async fn funded_lease_operator_boundaries() -> Result<()> {
    let Some(artifact_path) = std::env::var_os("GPU_LEASE_TEST_ARTIFACTS") else {
        eprintln!(
            "operator chain proof requires GPU_LEASE_TEST_ARTIFACTS from the Forge build; not counted as proof in this invocation"
        );
        return Ok(());
    };
    let artifacts: serde_json::Value = serde_json::from_slice(&std::fs::read(artifact_path)?)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let _anvil = Anvil(
        Command::new("anvil")
            .args(["--silent", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let rpc = format!("http://127.0.0.1:{port}");
    let p = ProviderBuilder::new().connect(&rpc).await?;
    let mut accounts = vec![];
    for _ in 0..100 {
        if let Ok(a) = p.get_accounts().await {
            accounts = a;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    ensure!(accounts.len() > 3, "Anvil failed to start");
    let core = accounts[0];
    let owner = accounts[1];
    let operator = accounts[2];
    let attacker = accounts[3];
    let bytes = |source: &str, name: &str| -> Result<Vec<u8>> {
        Ok(hex::decode(
            artifacts["contracts"][source][name]["evm"]["bytecode"]["object"]
                .as_str()
                .context("missing bytecode")?,
        )?)
    };
    let vault = tx(
        &p,
        core,
        None,
        bytes("contracts/src/GpuLeaseVault.sol", "GpuLeaseVault")?,
        U256::ZERO,
    )
    .await?
    .contract_address
    .context("vault deployment")?;
    let mut code = bytes("contracts/src/GpuLeaseBlueprint.sol", "GpuLeaseBlueprint")?;
    code.extend((vault, Address::ZERO).abi_encode());
    let bsm = tx(&p, core, None, code, U256::ZERO)
        .await?
        .contract_address
        .context("BSM deployment")?;
    tx(
        &p,
        core,
        Some(bsm),
        Bsm::onBlueprintCreatedCall {
            id: 0,
            owner: core,
            tangle: core,
        }
        .abi_encode(),
        U256::ZERO,
    )
    .await?;
    let state_path = format!("/tmp/gpu-lease-operator-proof-{}.json", std::process::id());
    unsafe {
        for (key, value) in [
            ("HTTP_RPC_URL", rpc.clone()),
            ("GPU_LEASE_VAULT_ADDRESS", format!("{vault:#x}")),
            ("GPU_LEASE_BLUEPRINT_ADDRESS", format!("{bsm:#x}")),
            ("GPU_OPERATOR_ADDRESS", format!("{operator:#x}")),
            ("GPU_LEASE_STATE_PATH", state_path.clone()),
            (
                "GPU_INVENTORY_JSON",
                r#"[{"id":"d0","gpu_class":"h100","tee":false,"cuda_ordinal":0}]"#.into(),
            ),
        ] {
            std::env::set_var(key, value);
        }
    }
    let mut r = GpuLeaseRequest {
        intentVersion: 2,
        intentHash: gpu_lease_blueprint_lib::jobs::intent_hash(2, "h100", 600, 0, "any", 1).into(),
        pricePerSecond: 3,
        durationSeconds: 600,
        confidentiality: 0,
        gpuClass: "h100".into(),
        region: "any".into(),
        lessee: owner,
        leaseId: [0x73; 32].into(),
        sandboxId: [0; 32].into(),
        sandboxTeeType: 0,
        deviceCount: 1,
    };
    let fake = call(0, r.clone(), owner).await?;
    ensure!(
        rejected(&fake),
        "fake lease id accepted and hardware locked"
    );
    ensure!(
        gpu_lease_blueprint_lib::allocator().idle_count("h100", false) == 1,
        "fake lease reserved hardware"
    );
    let receipt = tx(
        &p,
        owner,
        Some(vault),
        Vault::createCall {
            operator,
            price: 3,
            duration: 600,
            count: 1,
            intent: r.intentHash,
            confidentiality: 0,
            endpoint: Bytes::new(),
        }
        .abi_encode(),
        U256::from(1800),
    )
    .await?;
    r.leaseId = receipt
        .logs()
        .iter()
        .find_map(|l| {
            Vault::LeaseCreated::decode_log(&l.inner)
                .ok()
                .map(|v| v.data.leaseId)
        })
        .context("LeaseCreated")?;
    tx(
        &p,
        core,
        Some(bsm),
        Bsm::onJobCallCall {
            service: 0,
            job: 0,
            callId: 0,
            inputs: r.abi_encode().into(),
        }
        .abi_encode(),
        U256::ZERO,
    )
    .await?;
    let result = call(0, r.clone(), owner).await?;
    ensure!(result[0].is_ok(), "funded lease refused: {result:?}");
    let body = result[0].body().unwrap().clone();
    let output = GpuLeaseOutput::abi_decode(&body)?;
    ensure!(output.leaseId == r.leaseId, "lease identity changed");
    tx(
        &p,
        core,
        Some(bsm),
        Bsm::onJobResultCall {
            service: 0,
            job: 0,
            callId: 0,
            operator,
            inputsHash: keccak256(r.abi_encode()),
            outputs: body.into(),
        }
        .abi_encode(),
        U256::ZERO,
    )
    .await?;
    restart_from_journal(r.leaseId).await?;
    let id = GpuLeaseIdRequest { leaseId: r.leaseId };
    let foreign_extend = call(
        2,
        GpuLeaseExtendRequest {
            leaseId: r.leaseId,
            addSeconds: 60,
        },
        attacker,
    )
    .await?;
    let early_reap = call(3, id.clone(), attacker).await?;
    let foreign_release = call(1, id.clone(), attacker).await?;
    ensure!(
        rejected(&foreign_extend) && rejected(&early_reap) && rejected(&foreign_release),
        "foreign extend={},early reap={},foreign release={}",
        !rejected(&foreign_extend),
        !rejected(&early_reap),
        !rejected(&foreign_release)
    );
    ensure!(
        rejected(
            &call(
                2,
                GpuLeaseExtendRequest {
                    leaseId: r.leaseId,
                    addSeconds: 60
                },
                owner
            )
            .await?
        ),
        "unpaid owner extension accepted"
    );
    let uri = format!("/api/leases/{:#x}/access", r.leaseId);
    ensure!(
        http("GET", &uri, serde_json::Value::Null, None).await?.0 == 401,
        "GPU access bypasses credentials"
    );
    let (status, challenge) = http(
        "POST",
        "/api/session/challenge",
        serde_json::json!({"leaseId":format!("{:#x}",r.leaseId),"lessee":format!("{owner:#x}")}),
        None,
    )
    .await?;
    ensure!(status == 200, "challenge failed: {challenge}");
    let message = challenge["message"].as_str().context("challenge message")?;
    let signature: String = p
        .raw_request(
            "personal_sign".into(),
            serde_json::json!([format!("0x{}", hex::encode(message)), format!("{owner:#x}")]),
        )
        .await?;
    let (status, credential) = http(
        "POST",
        "/api/session/verify",
        serde_json::json!({"nonce":challenge["nonce"],"signature":signature}),
        None,
    )
    .await?;
    ensure!(status == 200, "verification failed: {credential}");
    let token = credential["token"].as_str().context("token")?;
    ensure!(
        http("GET", &uri, serde_json::Value::Null, Some(token))
            .await?
            .0
            == 200,
        "valid credential refused"
    );
    tx(
        &p,
        owner,
        Some(vault),
        Vault::extendCall {
            id: r.leaseId,
            seconds_: 60,
        }
        .abi_encode(),
        U256::from(180),
    )
    .await?;
    ensure!(
        !rejected(
            &call(
                2,
                GpuLeaseExtendRequest {
                    leaseId: r.leaseId,
                    addSeconds: 60
                },
                owner
            )
            .await?
        ),
        "funded extension refused"
    );
    ensure!(
        rejected(
            &call(
                2,
                GpuLeaseExtendRequest {
                    leaseId: r.leaseId,
                    addSeconds: 60
                },
                owner
            )
            .await?
        ),
        "replayed extension creates free time"
    );
    ensure!(
        !rejected(&call(1, id, owner).await?),
        "owner release refused"
    );
    ensure!(
        http("GET", &uri, serde_json::Value::Null, Some(token))
            .await?
            .0
            != 200,
        "released lease credential still authorizes access"
    );
    ensure!(
        gpu_lease_blueprint_lib::allocator().idle_count("h100", false) == 1,
        "release did not free hardware"
    );
    reverted_result_frees_reservation(&p, &rpc, owner, vault, bsm, r).await?;
    eprintln!(
        "Anvil operator proof: fake id denied; funded lease allocated; foreign release/early reap/unpaid extension denied; bearer enforced; funded extension idempotent; release revokes access; fresh process recovers journal; mined reverted result frees reservation"
    );
    Ok(())
}
