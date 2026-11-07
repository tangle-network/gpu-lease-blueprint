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

    if let Some(mode) = std::env::var_os("GPU_CLOUD_CAPACITY") {
        if mode != "1" {
            return Err(blueprint_sdk::Error::Other(
                "GPU_CLOUD_CAPACITY must be 1 when set".into(),
            ));
        }
        #[cfg(all(feature = "cloud", unix))]
        {
            use tokio::io::AsyncReadExt;
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .map_err(|_| blueprint_sdk::Error::Other("cloud signal setup failed".into()))?;
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .map_err(|_| blueprint_sdk::Error::Other("cloud signal setup failed".into()))?;
            let mut bytes = Vec::new();
            tokio::io::stdin()
                .take(gpu_lease_blueprint_lib::cloud::MAX_CONFIG_BYTES + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| {
                    blueprint_sdk::Error::Other("cloud configuration read failed".into())
                })?;
            return gpu_lease_blueprint_lib::cloud::run(&bytes, async {
                tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
            })
            .await
            .map_err(blueprint_sdk::Error::Other);
        }
        #[cfg(not(all(feature = "cloud", unix)))]
        return Err(blueprint_sdk::Error::Other(
            "cloud feature on Unix is required".into(),
        ));
    }

    gpu_lease_blueprint_lib::initialize()
        .await
        .map_err(blueprint_sdk::Error::Other)?;
    let env = BlueprintEnvironment::load()?;
    let tangle_client = env
        .tangle_client()
        .await
        .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?;

    let service_id: u64 = env
        .protocol_settings
        .tangle()
        .map_err(|e| blueprint_sdk::Error::Other(e.to_string()))?
        .service_id
        .ok_or_else(|| blueprint_sdk::Error::Other("service id is required".into()))?;
    info!(service_id, "starting gpu-lease-blueprint operator");

    let tangle_producer = TangleProducer::new(tangle_client.clone(), service_id);

    // Operator HTTP API — the UI's fuel (capabilities/quotes/lease status/
    // EIP-191 sessions). State-changing ops stay on-chain via the driver.
    let api_port: u16 = std::env::var("OPERATOR_API_PORT")
        .unwrap_or_else(|_| "9100".into())
        .parse()
        .map_err(|_| blueprint_sdk::Error::Other("invalid OPERATOR_API_PORT".into()))?;
    let sweep_secs: u64 = std::env::var("REAPER_SWEEP_INTERVAL_SECS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .map_err(|_| blueprint_sdk::Error::Other("invalid REAPER_SWEEP_INTERVAL_SECS".into()))?;
    if sweep_secs == 0 {
        return Err(blueprint_sdk::Error::Other(
            "REAPER_SWEEP_INTERVAL_SECS must be positive".into(),
        ));
    }
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

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(sweep_secs));
        loop {
            interval.tick().await;
            if let Err(error) = gpu_lease_blueprint_lib::chain::reconcile(false).await {
                warn!(%error,"lease reconciliation failed; access fails closed");
            }
        }
    });

    let tangle_config = TangleConfig::default();

    let result = BlueprintRunner::builder(tangle_config, env)
        .router(router())
        .producer(tangle_producer)
        .consumer(gpu_lease_blueprint_lib::consumer::LeaseConsumer::new(
            blueprint_sdk::tangle::TangleConsumer::new(tangle_client),
        ))
        .run()
        .await;

    result.map_err(|e| blueprint_sdk::Error::Other(e.to_string()))
}
