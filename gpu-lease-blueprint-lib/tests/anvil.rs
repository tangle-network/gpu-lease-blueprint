//! E2E: the full GPU-lease lifecycle through a REAL local tnt-core on anvil —
//! jobs AND money on one chain.
//!
//! `BlueprintHarness` boots an anvil container seeded from the bundled
//! LocalTestnet broadcast (full Tangle stack), runs the real `BlueprintRunner`
//! with our job handlers, and submits jobs on-chain. This suite additionally
//! deploys the REAL `GpuLeaseVault` (bytecode from the foundry artifact) on
//! the same chain and proves the complete loop:
//!
//!   buyer escrows (vault.create) → LEASE job (request carries the vault
//!   leaseId; result echoes it) → EXTEND (job + vault escrow) → time warp →
//!   RELEASE (job + vault release: EXACT pro-rata refund asserted to the wei)
//!   → operator withdraws earnings → second lease → warp past expiry →
//!   REAP job + permissionless vault.reap → full take.
//!
//! I1 is asserted on the live chain after every step:
//!   vault.balance == totalEscrowed + operatorEarnings.
//!
//! Requires Docker (colima). Skips gracefully when Docker/artifacts are
//! missing — use `scripts/run-e2e.sh` for the proven invocation.

use alloy_rpc_types::TransactionRequest;
use anyhow::{Context, Result, bail};
use blueprint_anvil_testing_utils::{BlueprintHarness, missing_tnt_core_artifacts};
use blueprint_sdk::alloy::primitives::{Address, Bytes, U256};
use blueprint_sdk::alloy::providers::{Provider, ProviderBuilder};
use blueprint_sdk::alloy::sol;
use blueprint_sdk::alloy::sol_types::{SolCall, SolEvent, SolValue};
use gpu_lease_blueprint_lib::jobs;
use gpu_lease_blueprint_lib::{
    GpuLeaseAck, GpuLeaseExtendRequest, GpuLeaseIdRequest, GpuLeaseOutput, GpuLeaseRequest,
    JOB_EXTEND, JOB_LEASE, JOB_REAP, JOB_RELEASE, router,
};
use once_cell::sync::Lazy;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

const ANVIL_TEST_TIMEOUT: Duration = Duration::from_secs(600);
const JOB_RESULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Foundry artifact for the vault — produced by `forge build`.
const VAULT_ARTIFACT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../contracts/out/GpuLeaseVault.sol/GpuLeaseVault.json"
);

static HARNESS_LOCK: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));
static LOG_INIT: std::sync::Once = std::sync::Once::new();

fn setup_log() {
    LOG_INIT.call_once(|| {
        let _ = tracing_subscriber::fmt::try_init();
    });
}

/// One co-located H100, non-TEE — the operator's advertised inventory
/// (SPEC §4: no on-chain registry; this is operator-local truth).
/// Operator quote-signing key (test-constant; production feeds the runner keystore).
const QUOTE_SIGNING_KEY: &str = "4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d";
const GPU_INVENTORY: &str = r#"[{"id":"gpu-0","gpu_class":"h100","tee":false,"cuda_ordinal":0}]"#;

async fn spawn_harness() -> Result<Option<BlueprintHarness>> {
    // The anvil container occasionally dies mid-seed (colima flake) — retry.
    let mut last_err = None;
    for attempt in 1..=3 {
        match BlueprintHarness::builder(router())
            .poll_interval(Duration::from_millis(50))
            .with_env_var("GPU_INVENTORY_JSON", GPU_INVENTORY)
        .with_env_var("GPU_QUOTE_SIGNING_KEY", QUOTE_SIGNING_KEY)
            .spawn()
            .await
        {
            Ok(harness) => return Ok(Some(harness)),
            Err(err) => {
                if missing_tnt_core_artifacts(&err) {
                    eprintln!("skipping e2e: {err}");
                    return Ok(None);
                }
                eprintln!("harness boot attempt {attempt}/3 failed: {err}");
                last_err = Some(err);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_err.unwrap())
}

/// The anvil harness needs Docker. On macOS + colima the socket is NOT at
/// /var/run/docker.sock — skip (not fail) when it is unreachable so plain
/// `cargo test --workspace` stays green without Docker. Use scripts/run-e2e.sh
/// to run the E2E with the right DOCKER_HOST/TMPDIR.
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
    if host.starts_with("tcp://") {
        return true; // assume reachable; the harness errors clearly if not
    }
    std::path::Path::new("/var/run/docker.sock").exists()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tangle view ABI (tnt-core 0.19 ITangleBlueprints) — the driver spike:
// everything the TangleDriver needs is READABLE VIEW STATE. Zero protocol
// changes required: definition, metadata, URI + pinned hash all come back
// from existing getters.
// ─────────────────────────────────────────────────────────────────────────────

sol! {
    struct TangleBlueprintMetadata {
        string name;
        string description;
        string author;
        string category;
        string codeRepository;
        string logo;
        string website;
        string license;
        string profilingData;
    }

    #[sol(rpc)]
    interface ITangleViews {
        function blueprintCount() external view returns (uint64);
        function blueprintMetadata(uint64 blueprintId)
            external
            view
            returns (TangleBlueprintMetadata metadata, string metadataUri, bytes32 metadataHash);
    }

    // ── tnt-core 0.19 Types mirror (only what createBlueprint needs) ──────

    enum MembershipModel { Fixed, Dynamic }
    enum PricingModel { PayOnce, Subscription, EventDriven }
    enum BlueprintSourceKind { Container, Wasm, Native }
    enum BlueprintFetcherKind { None }
    enum WasmRuntime { Unknown, Wasmtime, Wasmer }
    enum BlueprintArchitecture { Wasm32, Wasm64, Wasi32, Wasi64, Amd32, Amd64, Arm32, Arm64 }
    enum BlueprintOperatingSystem { Unknown, Linux, Windows, MacOS, BSD }

    struct ImageRegistrySource { string registry; string image; string tag; }
    struct WasmSource { WasmRuntime runtime; BlueprintFetcherKind fetcher; string artifactUri; string entrypoint; }
    struct NativeSource { BlueprintFetcherKind fetcher; string artifactUri; string entrypoint; }
    struct TestingSource { string cargoPackage; string cargoBin; string basePath; }
    struct BlueprintBinary { BlueprintArchitecture arch; BlueprintOperatingSystem os; string name; bytes32 sha256; }
    struct BlueprintSource {
        BlueprintSourceKind kind;
        ImageRegistrySource container;
        WasmSource wasm;
        NativeSource native;
        TestingSource testing;
        BlueprintBinary[] binaries;
    }
    struct TangleJobDefinition { string name; string description; string metadataUri; bytes paramsSchema; bytes resultSchema; }
    struct TangleBlueprintConfig {
        MembershipModel membership;
        PricingModel pricing;
        uint32 minOperators;
        uint32 maxOperators;
        uint256 subscriptionRate;
        uint64 subscriptionInterval;
        uint256 eventRate;
    }
    struct BlueprintDefinitionFull {
        string metadataUri;
        bytes32 metadataHash;
        address manager;
        uint32 masterManagerRevision;
        bool hasConfig;
        TangleBlueprintConfig config;
        TangleBlueprintMetadata metadata;
        TangleJobDefinition[] jobs;
        bytes registrationSchema;
        bytes requestSchema;
        BlueprintSource[] sources;
        MembershipModel[] supportedMemberships;
    }

    #[sol(rpc)]
    interface ITangleReg {
        function createBlueprint(BlueprintDefinitionFull calldata def) external returns (uint64);
    }
}

/// The canonical metadata JSON path (generated: `cargo run -p gpu-lease-blueprint-gen`).
const METADATA_JSON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../metadata/blueprint.json");

fn vault_placeholder() -> Address {
    // The manager address is irrelevant to the metadata round-trip proof.
    Address::ZERO
}

fn deployer_for_registration() -> Address {
    // anvil #0 — the LocalTestnet deployer that originally registered blueprint 0.
    "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        .parse()
        .unwrap()
}

fn keccak256(data: &[u8]) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    k.update(data);
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Vault ABI (mirror of contracts/src/GpuLeaseVault.sol)
// ─────────────────────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IGpuLeaseVault {
        function create(address operator, uint128 pricePerSecond, uint64 durationSeconds, bytes32 intentHash, uint8 confidentiality, bytes endpointInfo) external payable returns (bytes32 leaseId);
        function extend(bytes32 leaseId, uint64 addSeconds) external payable;
        function release(bytes32 leaseId) external;
        function reap(bytes32 leaseId) external;
        function withdrawEarnings() external;
        function totalEscrowed() external view returns (uint256);
        function operatorEarnings() external view returns (uint256);

        event LeaseCreated(bytes32 indexed leaseId, address indexed operator, address indexed lessee, uint128 escrow, uint128 pricePerSecond, uint64 expiry, bytes32 intentHash, uint8 confidentiality, bytes endpointInfo, uint16 schemaVersion);
        event LeaseReleased(bytes32 indexed leaseId, uint128 refund, uint128 operatorTake);
        event LeaseReaped(bytes32 indexed leaseId, uint128 operatorTake, address caller);
    }
}

/// Read-provider against the harness chain with impersonation helpers.
struct Chain {
    rpc: String,
}

impl Chain {
    async fn new(harness: &BlueprintHarness) -> Self {
        Self {
            rpc: harness.environment().http_rpc_endpoint.to_string(),
        }
    }

    async fn provider(&self) -> Result<impl Provider> {
        ProviderBuilder::new()
            .connect(&self.rpc)
            .await
            .context("failed to connect to anvil")
    }

    async fn raw(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let provider = self.provider().await?;
        let method = method.to_string();
        provider
            .raw_request::<_, serde_json::Value>(std::borrow::Cow::Owned(method.clone()), params)
            .await
            .with_context(|| format!("raw_request {method} failed"))
    }

    /// Anvil impersonation: send transactions FROM any address without its key.
    async fn impersonate(&self, addr: Address) -> Result<()> {
        self.raw(
            "anvil_impersonateAccount",
            serde_json::json!([format!("{addr:#x}")]),
        )
        .await
        .map(|_| ())
    }

    async fn fund(&self, addr: Address) -> Result<()> {
        self.raw(
            "anvil_setBalance",
            serde_json::json!([format!("{addr:#x}"), "0x3635c9adc5dea0000000000"]), // 1e9 ETH
        )
        .await
        .map(|_| ())
    }

    /// Warp the chain clock and mine a block.
    async fn warp(&self, seconds: u64) -> Result<()> {
        self.raw("evm_increaseTime", serde_json::json!([seconds]))
            .await?;
        self.raw("evm_mine", serde_json::json!([])).await?;
        Ok(())
    }

    async fn send(
        &self,
        from: Address,
        input: Vec<u8>,
        to: Option<Address>,
        value: U256,
    ) -> Result<()> {
        let provider = self.provider().await?;
        let mut tx = TransactionRequest::default();
        tx.from = Some(from);
        tx.input = Bytes::from(input).into();
        tx.value = Some(value);
        tx.to = to.map(blueprint_sdk::alloy::primitives::TxKind::Call);
        tx.gas_price = Some(0); // exact-value assertions: no gas noise in deltas
        let receipt = provider
            .send_transaction(tx)
            .await
            .context("send_transaction failed")?
            .get_receipt()
            .await
            .context("no receipt")?;
        if !receipt.status() {
            bail!("transaction reverted");
        }
        Ok(())
    }

    /// Send a tx, decode the first matching event E, and report the ACTUAL
    /// fee paid (gas_used x effective_gas_price) for exact balance accounting.
    async fn send_and_decode<E: SolEvent>(
        &self,
        from: Address,
        input: Vec<u8>,
        to: Option<Address>,
        value: U256,
    ) -> Result<(E, U256)> {
        let provider = self.provider().await?;
        let mut tx = TransactionRequest::default();
        tx.from = Some(from);
        tx.input = Bytes::from(input).into();
        tx.value = Some(value);
        tx.to = to.map(blueprint_sdk::alloy::primitives::TxKind::Call);
        let receipt = provider
            .send_transaction(tx)
            .await
            .context("send_transaction failed")?
            .get_receipt()
            .await
            .context("no receipt")?;
        if !receipt.status() {
            bail!("transaction reverted");
        }
        let fee = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
        for log in receipt.logs() {
            if let Ok(decoded) = E::decode_log(&log.inner) {
                return Ok((decoded.data, fee));
            }
        }
        bail!("event {} not found in receipt logs", E::SIGNATURE)
    }

    async fn balance(&self, addr: Address) -> Result<U256> {
        let provider = self.provider().await?;
        Ok(provider.get_balance(addr).await?)
    }

    /// Latest block timestamp (the chain's clock).
    async fn timestamp(&self) -> Result<u64> {
        let provider = self.provider().await?;
        let block = provider
            .get_block(alloy_rpc_types::BlockId::latest())
            .await
            .context("latest block")?
            .context("no block")?;
        Ok(block.header.timestamp)
    }

    /// I1 on a live chain: vault holds exactly live escrow + unwithdrawn earnings.
    async fn assert_conservation(&self, vault: Address) -> Result<()> {
        let provider = self.provider().await?;
        let total: U256 = IGpuLeaseVault::new(vault, &provider)
            .totalEscrowed()
            .call()
            .await?;
        let earnings: U256 = IGpuLeaseVault::new(vault, &provider)
            .operatorEarnings()
            .call()
            .await?;
        let actual = provider.get_balance(vault).await?;
        anyhow::ensure!(
            actual == total + earnings,
            "I1 violated on-chain: balance {actual} != totalEscrowed {total} + earnings {earnings}"
        );
        Ok(())
    }
}

/// Deploy the real vault bytecode from the foundry artifact.
async fn deploy_vault(chain: &Chain, deployer: Address) -> Result<Address> {
    let artifact = std::fs::read_to_string(VAULT_ARTIFACT).with_context(|| {
        format!("vault artifact missing at {VAULT_ARTIFACT} — run `forge build`")
    })?;
    let artifact: serde_json::Value = serde_json::from_str(&artifact)?;
    let bytecode = artifact["bytecode"]["object"]
        .as_str()
        .context("artifact missing bytecode.object")?
        .trim_start_matches("0x");
    let bytecode = hex::decode(bytecode).context("bytecode hex")?;
    let provider = chain.provider().await?;
    let mut tx = TransactionRequest::default();
    tx.from = Some(deployer);
    tx.input = Bytes::from(bytecode).into();
    let receipt = provider
        .send_transaction(tx)
        .await?
        .get_receipt()
        .await
        .context("no deploy receipt")?;
    Ok(receipt
        .contract_address
        .context("deploy receipt missing contract address")?)
}

/// Extract the LeaseCreated leaseId from the deploy/call receipt's first log.
#[test]
fn gpu_lease_full_lifecycle_end_to_end() {
    // Deep async poll chains (harness + giant test future) overflow tokio's
    // default 2MB worker stacks — build the runtime with real headroom.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(64 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("runtime");
    if let Err(e) = rt.block_on(gpu_lease_full_lifecycle_end_to_end_inner()) {
        panic!("e2e failed: {e:?}");
    }
}

async fn gpu_lease_full_lifecycle_end_to_end_inner() -> Result<()> {
    setup_log();
    let guard = HARNESS_LOCK.lock().await;
    let result = timeout(ANVIL_TEST_TIMEOUT, async {
        if !docker_socket_available() {
            eprintln!("skipping e2e: docker socket unreachable (on macOS+colima: scripts/run-e2e.sh)");
            return Ok(());
        }
        if !std::path::Path::new(VAULT_ARTIFACT).exists() {
            eprintln!("skipping e2e: vault artifact missing — run `forge build` first");
            return Ok(());
        }
        let Some(harness) = spawn_harness().await? else {
            return Ok(());
        };

        let chain = Chain::new(&harness).await;

        // ── 0. Driver spike: read blueprint state back from the live chain ──
        let tangle_addr = harness.deployment().tangle_contract;
        {
            let provider = chain.provider().await?;
            let views = ITangleViews::new(tangle_addr, &provider);
            let count: u64 = views.blueprintCount().call().await?;
            anyhow::ensure!(count >= 1, "expected at least one registered blueprint");
            let meta = views.blueprintMetadata(0).call().await?;
            eprintln!(
                "driver spike: blueprintCount={count} name={:?} category={:?} metadataUri={:?} hash={:#x}",
                meta.metadata.name, meta.metadata.category, meta.metadataUri, meta.metadataHash
            );
        }

        // ── 0b. Driver round-trip: register OUR hash-pinned definition, read it
        //       back, verify the pin and the payload — the full TangleDriver
        //       contract with zero protocol changes. ─────────────────────────
        if let Ok(canonical_json) = std::fs::read_to_string(METADATA_JSON) {
            let json_bytes = canonical_json.as_bytes();
            let json_hash = keccak256(json_bytes);
            use base64::Engine as _;
            let data_uri = format!(
                "data:application/json;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(json_bytes)
            );

            let count_before: u64 = {
                let provider = chain.provider().await?;
                ITangleViews::new(tangle_addr, &provider).blueprintCount().call().await?
            };

            let jobs: Vec<TangleJobDefinition> = ["lease", "release", "extend", "reap"]
                .iter()
                .map(|&name| TangleJobDefinition {
                    name: name.to_string(),
                    description: String::new(),
                    metadataUri: String::new(),
                    paramsSchema: Vec::new().into(),
                    resultSchema: Vec::new().into(),
                })
                .collect();
            let def = BlueprintDefinitionFull {
                metadataUri: data_uri.clone(),
                metadataHash: json_hash.into(),
                manager: vault_placeholder(),
                masterManagerRevision: 0,
                hasConfig: true,
                config: TangleBlueprintConfig {
                    membership: MembershipModel::Dynamic,
                    pricing: PricingModel::EventDriven,
                    minOperators: 1,
                    maxOperators: 100,
                    subscriptionRate: U256::ZERO,
                    subscriptionInterval: 0,
                    eventRate: U256::from(1e15 as u64),
                },
                metadata: TangleBlueprintMetadata {
                    name: "GPU Lease Blueprint".into(),
                    description: "Escrowed GPU leases".into(),
                    author: "Tangle".into(),
                    category: "Compute".into(),
                    codeRepository: "https://github.com/tangle-network/gpu-lease-blueprint".into(),
                    logo: String::new(),
                    website: "https://tangle.network".into(),
                    license: "MIT OR Apache-2.0".into(),
                    profilingData: String::new(),
                },
                jobs,
                registrationSchema: Vec::new().into(),
                requestSchema: Vec::new().into(),
                sources: vec![BlueprintSource {
                    kind: BlueprintSourceKind::Container,
                    container: ImageRegistrySource {
                        registry: "ghcr.io".into(),
                        image: "tangle-network/gpu-lease-blueprint".into(),
                        tag: "latest".into(),
                    },
                    wasm: WasmSource {
                        runtime: WasmRuntime::Unknown,
                        fetcher: BlueprintFetcherKind::None,
                        artifactUri: String::new(),
                        entrypoint: String::new(),
                    },
                    native: NativeSource {
                        fetcher: BlueprintFetcherKind::None,
                        artifactUri: String::new(),
                        entrypoint: String::new(),
                    },
                    testing: TestingSource {
                        cargoPackage: String::new(),
                        cargoBin: String::new(),
                        basePath: String::new(),
                    },
                    binaries: vec![BlueprintBinary {
                        arch: BlueprintArchitecture::Amd64,
                        os: BlueprintOperatingSystem::Linux,
                        name: "gpu-lease-blueprint".into(),
                        sha256: [0xaa; 32].into(),
                    }],
                }],
                supportedMemberships: vec![MembershipModel::Dynamic],
            };

            let reg_input = ITangleReg::createBlueprintCall { def }.abi_encode();
            chain
                .send(deployer_for_registration(), reg_input, Some(tangle_addr), U256::ZERO)
                .await
                .context("createBlueprint failed")?;

            // Read it back — exactly what the TangleDriver does.
            let provider = chain.provider().await?;
            let views = ITangleViews::new(tangle_addr, &provider);
            let count_after: u64 = views.blueprintCount().call().await?;
            anyhow::ensure!(count_after == count_before + 1, "blueprint count did not advance");
            let read_back = views.blueprintMetadata(count_after - 1).call().await?;

            // The pin holds: on-chain hash == keccak(canonical JSON).
            anyhow::ensure!(
                read_back.metadataHash.0 == json_hash,
                "metadataHash mismatch: on-chain pin does not match the canonical JSON"
            );
            // The payload is intact and driver-parseable from the data URI.
            anyhow::ensure!(read_back.metadataUri == data_uri, "metadataUri mismatch");
            let b64 = data_uri.strip_prefix("data:application/json;base64,").unwrap();
            let decoded = base64::engine::general_purpose::STANDARD.decode(b64)?;
            let doc: serde_json::Value = serde_json::from_slice(&decoded)?;
            anyhow::ensure!(doc["blueprint"]["category"] == "Compute");
            anyhow::ensure!(doc["schemaVersion"] == 1);
            let names: Vec<&str> = doc["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|j| j["name"].as_str().unwrap())
                .collect();
            anyhow::ensure!(names == ["lease", "release", "extend", "reap"], "job names: {names:?}");
            eprintln!(
                "driver round-trip ok: registered id={} hash=0x{} jobs={:?}",
                count_after - 1,
                hex::encode(json_hash),
                names
            );
        } else {
            eprintln!("skipping driver round-trip: {METADATA_JSON} missing (run the gen)");
        }

        // ── Cast: buyer, operator-money-sink, reaper, deployer ────────────
        let buyer = harness.caller_account();
        let operator: Address = "0x00000000000000000000000000000000000000b1".parse()?; // money sink
        let reaper: Address = "0x00000000000000000000000000000000000000b2".parse()?; // permissionless reaper
        let deployer: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266".parse()?; // anvil #0

        for addr in [buyer, operator, reaper, deployer] {
            chain.impersonate(addr).await?;
            chain.fund(addr).await?;
        }

        // ── Deploy the real vault on the same chain as the jobs ──────────
        let vault = deploy_vault(&chain, deployer).await?;
        eprintln!("vault deployed at {vault:#x}");

        let price: u128 = 1_000; // wei/s
        let duration: u64 = 600; // seconds
        let cost = U256::from(price) * U256::from(duration);

        // ── 1. Buyer escrows on the vault ────────────────────────────────
        let class = "h100".to_string();
        let region = "local".to_string();
        let intent = jobs::intent_hash(1, &class, duration, 0, &region);
        let create_input = IGpuLeaseVault::createCall {
            operator,
            pricePerSecond: price,
            durationSeconds: duration,
            intentHash: intent.into(),
            confidentiality: 0,
            endpointInfo: "{}".into(),
        }
        .abi_encode();
        let (created, _create_fee): (IGpuLeaseVault::LeaseCreated, U256) = chain
            .send_and_decode(buyer, create_input, Some(vault), cost)
            .await?;
        let lease_id = created.leaseId.0;
        let lease_expiry: u64 = created.expiry;
        eprintln!("escrowed: leaseId={}", hex::encode(lease_id));
        chain.assert_conservation(vault).await?;

        // ── 2. LEASE job through real tnt-core, bound to the vault lease ──
        let request = GpuLeaseRequest {
            intentVersion: 1,
            intentHash: intent.into(),
            pricePerSecond: price,
            durationSeconds: duration,
            confidentiality: 0,
            gpuClass: class.clone(),
            region: region.clone(),
            lessee: buyer,
            leaseId: lease_id.into(),
        }
        .abi_encode();

        let lease_submission = harness
            .submit_job(JOB_LEASE, Bytes::from(request))
            .await
            .context("failed to submit LEASE job")?;
        let lease_output_raw = harness
            .wait_for_job_result_with_deadline(lease_submission, JOB_RESULT_TIMEOUT)
            .await
            .context("no LEASE result")?;
        let lease_output = GpuLeaseOutput::abi_decode(&lease_output_raw)
            .context("failed to decode GpuLeaseOutput")?;

        // ── Operator API leg: the UI's fuel, live against the running
        //    operator's own state (globals seeded by the harness env).
        //    Raw HTTP over TcpStream: no extra HTTP client dependency. ────
        {
            use gpu_lease_blueprint_lib::api::operator_api_router;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let api_port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                axum::serve(listener, operator_api_router()).await.ok();
            });
            let base = format!("127.0.0.1:{api_port}");

            async fn http_json(method: &str, base: &str, path: &str, body: Option<&str>) -> Result<serde_json::Value> {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut stream = tokio::net::TcpStream::connect(base).await?;
                let body_bytes = body.map(|b| b.as_bytes().to_vec()).unwrap_or_default();
                let req = format!(
                    "{method} {path} HTTP/1.1\r\nHost: {base}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_bytes.len()
                );
                stream.write_all(req.as_bytes()).await?;
                stream.write_all(&body_bytes).await?;
                let mut raw = Vec::new();
                stream.read_to_end(&mut raw).await?;
                let text = String::from_utf8_lossy(&raw);
                let body_start = text.find("\r\n\r\n").context("malformed response")? + 4;
                let payload = &text[body_start..];
                // Handle chunked responses by extracting the first JSON value.
                let start = payload.find('{').context("no json")?;
                let mut depth = 0i32;
                let mut end = start;
                for (i, c) in payload[start..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = start + i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(serde_json::from_str(&payload[start..end])?)
            }

            // Wait for readiness.
            let mut ready = false;
            for _ in 0..50 {
                if http_json("GET", &base, "/api/capabilities", None).await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            anyhow::ensure!(ready, "operator API not ready");

            // Capabilities: the leased class now shows zero idle h100 devices.
            let caps = http_json("GET", &base, "/api/capabilities", None).await?;
            let h100 = caps["compute"]["classes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == "h100")
                .context("h100 class missing")?;
            anyhow::ensure!(h100["idleCount"] == 0, "device should be busy after LEASE");

            // Quote: live math from the hot-reloadable policy (saturated => +20%).
            let quote = http_json(
                "POST",
                &base,
                "/api/quote",
                Some(r#"{"gpuClass":"h100","durationSeconds":600}"#),
            )
            .await?;
            let expected = 300_000_000_000_000u128 * 12_000 / 10_000;
            anyhow::ensure!(
                quote["pricePerSecond"].as_str() == Some(&expected.to_string()),
                "quote {quote:?} != saturated policy price {expected}"
            );

            // Signed RFQ envelope: signature present and recovers to the
            // claimed operator address over the canonical payload.
            let sig = quote["signature"].as_str().context("quote must be signed")?;
            let claimed = quote["operatorAddress"].as_str().context("operatorAddress")?;
            let payload = gpu_lease_blueprint_lib::api::canonical_quote_payload(
                "h100",
                &expected.to_string(),
                600,
                0,
                &(expected * 600).to_string(),
                quote["intentHash"].as_str().unwrap(),
                quote["validUntil"].as_u64().unwrap(),
            );
            let recovered = gpu_lease_blueprint_lib::eip191_recover_signer(
                &payload,
                sig.trim_start_matches("0x"),
            )
            .context("signature must recover")?;
            anyhow::ensure!(recovered == claimed, "recovered {recovered} != claimed {claimed}");
            eprintln!("operator API leg: signed quote verified, operator={claimed}");

            // Lease status: public data only, reflects the live allocation.
            let status = http_json(
                "GET",
                &base,
                &format!("/api/leases/{}", hex::encode(lease_id)),
                None,
            )
            .await?;
            anyhow::ensure!(status["deviceId"] == "gpu-0", "status device: {status}");
            anyhow::ensure!(status["expiresAt"].as_u64().unwrap() > 0);
            let raw = status.to_string();
            anyhow::ensure!(!raw.contains("token") && !raw.contains("secret"), "credential leak");
            eprintln!(
                "operator API leg ok: quote {} wei/s, status device={}",
                quote["pricePerSecond"], status["deviceId"]
            );
        }

        let endpoint: serde_json::Value =
            serde_json::from_str(&lease_output.endpoint).context("endpoint must be json")?;
        anyhow::ensure!(
            endpoint["transport"] == "local-attach",
            "unexpected endpoint transport: {endpoint}"
        );
        anyhow::ensure!(lease_output.schemaVersion == 1, "schema version must be 1");
        anyhow::ensure!(
            lease_output.leaseId.0 == lease_id,
            "job result leaseId must equal the vault leaseId (one identity)"
        );
        eprintln!(
            "LEASE ok: leaseId={}, endpoint={}",
            hex::encode(lease_output.leaseId),
            lease_output.endpoint
        );

        // ── 3. EXTEND: escrow on the vault + job for the session ─────────
        let add_seconds: u64 = 300;
        let extend_cost = U256::from(price) * U256::from(add_seconds);
        chain
            .send(
                buyer,
                IGpuLeaseVault::extendCall {
                    leaseId: lease_id.into(),
                    addSeconds: add_seconds,
                }
                .abi_encode(),
                Some(vault),
                extend_cost,
            )
            .await?;
        chain.assert_conservation(vault).await?;

        let extend_request =
            GpuLeaseExtendRequest { leaseId: lease_id.into(), addSeconds: add_seconds }.abi_encode();
        let extend_submission = harness
            .submit_job(JOB_EXTEND, Bytes::from(extend_request))
            .await?;
        let extend_raw = harness
            .wait_for_job_result_with_deadline(extend_submission, JOB_RESULT_TIMEOUT)
            .await?;
        let extend_ack = GpuLeaseAck::abi_decode(&extend_raw)?;
        anyhow::ensure!(extend_ack.state == 0, "EXTEND must keep lease Live");
        anyhow::ensure!(extend_ack.leaseId.0 == lease_id);
        eprintln!("EXTEND ok (vault escrow + session)");

        // ── 4. Warp 100s, RELEASE: exact pro-rata refund on a live chain ─
        // The chain's own clock is the truth: anvil advances time per mined
        // block, so elapsed is measured from the chain, never assumed.
        chain.warp(100).await?;
        let now = chain.timestamp().await?;
        anyhow::ensure!(lease_expiry > now, "warp overshot the expiry");
        let buyer_before = chain.balance(buyer).await?;
        let (released, release_fee): (IGpuLeaseVault::LeaseReleased, U256) = chain
            .send_and_decode(
                buyer,
                IGpuLeaseVault::releaseCall { leaseId: lease_id.into() }.abi_encode(),
                Some(vault),
                U256::ZERO,
            )
            .await?;
        // Exact against the chain clock: refund == price * (expiry - release_ts).
        // (expiry includes the extend: created.expiry + addSeconds.)
        let current_expiry = lease_expiry + add_seconds;
        anyhow::ensure!(current_expiry > now, "release past expiry?");
        let expected_refund = U256::from(price) * U256::from(current_expiry - now);
        let total_paid = U256::from(price) * U256::from(duration + add_seconds);
        anyhow::ensure!(
            U256::from(released.refund) + U256::from(released.operatorTake) == total_paid,
            "I1/I4 violated: refund + take != total paid ({})",
            total_paid
        );
        anyhow::ensure!(
            U256::from(released.refund) == expected_refund,
            "I3 violated on-chain: refund {} != {}",
            released.refund,
            expected_refund
        );
        let buyer_after = chain.balance(buyer).await?;
        anyhow::ensure!(
            buyer_after - buyer_before + release_fee == expected_refund,
            "I3 violated: buyer delta {} + fee {} != exact refund {}",
            buyer_after - buyer_before,
            release_fee,
            expected_refund
        );
        eprintln!(
            "vault RELEASE ok: exact refund {} wei, operator take {} wei",
            released.refund, released.operatorTake
        );
        chain.assert_conservation(vault).await?;

        // RELEASE job settles the routing side.
        let release_request = GpuLeaseIdRequest { leaseId: lease_id.into() }.abi_encode();
        let release_submission = harness
            .submit_job(JOB_RELEASE, Bytes::from(release_request))
            .await?;
        let release_raw = harness
            .wait_for_job_result_with_deadline(release_submission, JOB_RESULT_TIMEOUT)
            .await?;
        let release_ack = GpuLeaseAck::abi_decode(&release_raw)?;
        anyhow::ensure!(release_ack.state == 1, "RELEASE must settle state=Released");
        eprintln!("RELEASE job ok");

        // ── 5. Operator withdraws the exact take ─────────────────────────
        let operator_before = chain.balance(operator).await?;
        chain
            .send(
                operator,
                IGpuLeaseVault::withdrawEarningsCall {}.abi_encode(),
                Some(vault),
                U256::ZERO,
            )
            .await?;
        let operator_after = chain.balance(operator).await?;
        anyhow::ensure!(
            operator_after - operator_before == U256::from(released.operatorTake),
            "operator withdraw != take"
        );
        chain.assert_conservation(vault).await?;
        eprintln!("operator withdrew {}", operator_after - operator_before);

        // ── 6. Second lease: REAP path (overstay impossible) ─────────────
        let (created2, _): (IGpuLeaseVault::LeaseCreated, U256) = chain
            .send_and_decode(
                buyer,
                IGpuLeaseVault::createCall {
                    operator,
                    pricePerSecond: price,
                    durationSeconds: duration,
                    intentHash: intent.into(),
                    confidentiality: 0,
                    endpointInfo: "{}".into(),
                }
                .abi_encode(),
                Some(vault),
                cost,
            )
            .await?;
        let lease2 = created2.leaseId.0;

        // LEASE the second one through tnt-core too.
        let request2 = GpuLeaseRequest {
            intentVersion: 1,
            intentHash: intent.into(),
            pricePerSecond: price,
            durationSeconds: duration,
            confidentiality: 0,
            gpuClass: class.clone(),
            region: region.clone(),
            lessee: buyer,
            leaseId: lease2.into(),
        }
        .abi_encode();
        let sub2 = harness.submit_job(JOB_LEASE, Bytes::from(request2)).await?;
        let out2_raw = harness
            .wait_for_job_result_with_deadline(sub2, JOB_RESULT_TIMEOUT)
            .await?;
        let out2 = GpuLeaseOutput::abi_decode(&out2_raw)?;
        anyhow::ensure!(out2.leaseId.0 == lease2, "second lease identity mismatch");

        // Warp past expiry; ANYONE reaps — full remaining escrow to operator.
        chain.warp(duration + 60).await?;
        let (reaped, _): (IGpuLeaseVault::LeaseReaped, U256) = chain
            .send_and_decode(
                reaper,
                IGpuLeaseVault::reapCall { leaseId: lease2.into() }.abi_encode(),
                Some(vault),
                U256::ZERO,
            )
            .await?;
        anyhow::ensure!(
            U256::from(reaped.operatorTake) == cost,
            "reap must pay the full escrow"
        );
        chain.assert_conservation(vault).await?;

        // REAP job settles the routing side (state=2).
        let reap_request = GpuLeaseIdRequest { leaseId: lease2.into() }.abi_encode();
        let reap_submission = harness.submit_job(JOB_REAP, Bytes::from(reap_request)).await?;
        let reap_raw = harness
            .wait_for_job_result_with_deadline(reap_submission, JOB_RESULT_TIMEOUT)
            .await?;
        let reap_ack = GpuLeaseAck::abi_decode(&reap_raw)?;
        anyhow::ensure!(reap_ack.state == 2, "REAP must settle state=Reaped");
        eprintln!(
            "REAP ok: permissionless full take {} wei via {:#x}",
            reaped.operatorTake, reaper
        );

        // Final conservation after the whole lifecycle.
        chain.assert_conservation(vault).await?;
        eprintln!("I1 held on-chain through the entire lifecycle");

        harness.shutdown().await;
        Ok(())
    })
    .await;

    drop(guard);
    result.context("e2e timed out")?
}
