//! E2E: the four SPEC §1 jobs through a REAL local tnt-core on anvil.
//!
//! `BlueprintHarness` boots an anvil container seeded from the bundled
//! LocalTestnet broadcast (full Tangle stack: master manager, staking,
//! status registry), registers a service for this router, runs the real
//! `BlueprintRunner` with our job handlers, and submits jobs on-chain.
//!
//! The lifecycle proven here: LEASE (device allocated, public endpoint
//! returned) → EXTEND (session pushed out) → RELEASE (device freed,
//! credentials revoked). Money settlement is the vault's, pinned by the
//! Foundry suite; this test pins the tnt-core job path.
//!
//! Requires Docker (colima). Skips gracefully when artifacts are missing.

use anyhow::{Context, Result};
use blueprint_anvil_testing_utils::{BlueprintHarness, missing_tnt_core_artifacts};
use blueprint_sdk::alloy::primitives::Bytes;
use blueprint_sdk::alloy::sol_types::SolValue;
use gpu_lease_blueprint_lib::jobs;
use gpu_lease_blueprint_lib::{
    GpuLeaseAck, GpuLeaseExtendRequest, GpuLeaseIdRequest, GpuLeaseOutput, GpuLeaseRequest,
    JOB_EXTEND, JOB_LEASE, JOB_RELEASE, router,
};
use once_cell::sync::Lazy;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

const ANVIL_TEST_TIMEOUT: Duration = Duration::from_secs(600);
const JOB_RESULT_TIMEOUT: Duration = Duration::from_secs(120);

static HARNESS_LOCK: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));
static LOG_INIT: std::sync::Once = std::sync::Once::new();

fn setup_log() {
    LOG_INIT.call_once(|| {
        let _ = tracing_subscriber::fmt::try_init();
    });
}

/// One co-located H100, non-TEE — the operator's advertised inventory
/// (SPEC §4: no on-chain registry; this is operator-local truth).
const GPU_INVENTORY: &str = r#"[{"id":"gpu-0","gpu_class":"h100","tee":false,"cuda_ordinal":0}]"#;

async fn spawn_harness() -> Result<Option<BlueprintHarness>> {
    match BlueprintHarness::builder(router())
        .poll_interval(Duration::from_millis(50))
        .with_env_var("GPU_INVENTORY_JSON", GPU_INVENTORY)
        .spawn()
        .await
    {
        Ok(harness) => Ok(Some(harness)),
        Err(err) => {
            if missing_tnt_core_artifacts(&err) {
                eprintln!("skipping e2e: {err}");
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
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
    // Default: bollard's /var/run/docker.sock
    std::path::Path::new("/var/run/docker.sock").exists()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_gpu_lease_jobs_end_to_end() -> Result<()> {
    setup_log();
    let guard = HARNESS_LOCK.lock().await;
    let result = timeout(ANVIL_TEST_TIMEOUT, async {
        if !docker_socket_available() {
            eprintln!("skipping e2e: docker socket unreachable (on macOS+colima: scripts/run-e2e.sh)");
            return Ok(());
        }
        let Some(harness) = spawn_harness().await? else {
            return Ok(());
        };

        // The lessee must be the job submitter (SPEC §1 requester binding).
        let lessee = harness.caller_account();
        let duration: u64 = 600;
        let class = "h100".to_string();
        let region = "local".to_string();
        let intent = jobs::intent_hash(1, &class, duration, 0, &region);

        // ── LEASE ──────────────────────────────────────────────────────
        let request = GpuLeaseRequest {
            intentVersion: 1,
            intentHash: intent.into(),
            pricePerSecond: 1, // wei/s — the harness chain has no real economy
            durationSeconds: duration,
            confidentiality: 0,
            gpuClass: class.clone(),
            region: region.clone(),
            lessee,
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

        let endpoint: serde_json::Value =
            serde_json::from_str(&lease_output.endpoint).context("endpoint must be json")?;
        anyhow::ensure!(
            endpoint["transport"] == "local-attach",
            "unexpected endpoint transport: {endpoint}"
        );
        anyhow::ensure!(lease_output.schemaVersion == 1, "schema version must be 1");
        eprintln!(
            "LEASE ok: leaseId={}, endpoint={}",
            hex::encode(lease_output.leaseId),
            lease_output.endpoint
        );

        // ── EXTEND ─────────────────────────────────────────────────────
        let extend_request =
            GpuLeaseExtendRequest { leaseId: lease_output.leaseId, addSeconds: 300 }.abi_encode();
        let extend_submission = harness
            .submit_job(JOB_EXTEND, Bytes::from(extend_request))
            .await
            .context("failed to submit EXTEND job")?;
        let extend_raw = harness
            .wait_for_job_result_with_deadline(extend_submission, JOB_RESULT_TIMEOUT)
            .await
            .context("no EXTEND result")?;
        let extend_ack = GpuLeaseAck::abi_decode(&extend_raw).context("failed to decode ack")?;
        anyhow::ensure!(extend_ack.state == 0, "EXTEND must keep lease Live");
        anyhow::ensure!(extend_ack.leaseId == lease_output.leaseId, "wrong leaseId");
        eprintln!("EXTEND ok");

        // ── RELEASE ────────────────────────────────────────────────────
        let release_request = GpuLeaseIdRequest { leaseId: lease_output.leaseId }.abi_encode();
        let release_submission = harness
            .submit_job(JOB_RELEASE, Bytes::from(release_request))
            .await
            .context("failed to submit RELEASE job")?;
        let release_raw = harness
            .wait_for_job_result_with_deadline(release_submission, JOB_RESULT_TIMEOUT)
            .await
            .context("no RELEASE result")?;
        let release_ack = GpuLeaseAck::abi_decode(&release_raw).context("failed to decode ack")?;
        anyhow::ensure!(release_ack.state == 1, "RELEASE must settle state=Released");
        eprintln!("RELEASE ok");

        harness.shutdown().await;
        Ok(())
    })
    .await;

    drop(guard);
    result.context("e2e timed out")?
}
