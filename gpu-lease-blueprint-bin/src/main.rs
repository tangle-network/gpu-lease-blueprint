//! Blueprint runner for gpu-lease-blueprint.
//!
//! Wires the four SPEC §1 jobs (LEASE/RELEASE/EXTEND/REAP) into the Tangle
//! runner, plus a periodic reaper sweep so devices whose escrow ran out are
//! freed and their credentials revoked even if nobody submits a REAP job
//! (overstay impossibility, operator side — SPEC I2).

use blueprint_sdk::contexts::tangle::TangleClientContext;
use blueprint_sdk::runner::BlueprintRunner;
use blueprint_sdk::runner::config::BlueprintEnvironment;
use blueprint_sdk::runner::tangle::config::TangleConfig;
use blueprint_sdk::tangle::TangleProducer;
use blueprint_sdk::{info, warn};
use gpu_lease_blueprint_lib::router;

fn setup_log() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};
    if tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .try_init()
        .is_err()
    {}
}

#[tokio::main]
async fn main() -> Result<(), blueprint_sdk::Error> {
    setup_log();

    let env = BlueprintEnvironment::load()?;
    let tangle_client = env
        .tangle_client()
        .await
        .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?;

    let service_id: u64 = env
        .protocol_settings
        .tangle()
        .ok()
        .and_then(|s| s.service_id)
        .unwrap_or_default();
    info!(service_id, "starting gpu-lease-blueprint operator");

    let tangle_producer = TangleProducer::new(tangle_client.clone(), service_id);

    // Operator HTTP API — the UI's fuel (capabilities/quotes/lease status/
    // EIP-191 sessions). State-changing ops stay on-chain via the driver.
    let api_port: u16 = std::env::var("OPERATOR_API_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9100);
    let api_listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{api_port}"))
        .await
        .map_err(|e| blueprint_sdk::Error::Other(format!("operator API bind :{api_port}: {e}")))?;
    info!(port = api_port, "operator API listening");
    tokio::spawn(async move {
        axum::serve(
            api_listener,
            gpu_lease_blueprint_lib::api::operator_api_router(),
        )
        .await
        .ok();
    });

    // Periodic sweep: free devices + revoke credentials at escrow exhaustion.
    // Interval is operator policy (SPEC §2) — default 30s, tighter for local dev.
    let sweep_secs: u64 = std::env::var("REAPER_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(sweep_secs));
        loop {
            interval.tick().await;
            let freed = gpu_lease_blueprint_lib::allocator().reap_expired();
            for alloc in freed {
                let revoked =
                    gpu_lease_blueprint_lib::credentials().revoke_for_lease(alloc.lease_id);
                warn!(
                    lease = ?alloc.lease_id,
                    device = %alloc.device_id,
                    revoked,
                    "reaped expired lease session"
                );
            }
        }
    });

    let tangle_config = TangleConfig::default();

    let result = BlueprintRunner::builder(tangle_config, env)
        .router(router())
        .producer(tangle_producer)
        .run()
        .await;

    if let Err(e) = result {
        blueprint_sdk::error!("runner failed: {e:?}");
    }

    Ok(())
}
