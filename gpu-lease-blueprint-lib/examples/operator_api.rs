//! Standalone operator API — capabilities + signed quotes without a chain.
//!
//! Serves `gpu_lease_blueprint_lib::api::operator_api_router()` on
//! `OPERATOR_API_LISTEN` (default 127.0.0.1:9200). Useful for UI development
//! and screenshots against real policy math — the chain is not needed for the
//! read-only quote surfaces (leasing still goes through tnt-core).
//!
//! Run:
//!   GPU_INVENTORY_JSON='[{"id":"gpu-0","gpu_class":"h100","tee":false,"cuda_ordinal":0}]' \
//!   GPU_QUOTE_SIGNING_KEY=<64-hex> \
//!   cargo run -p gpu-lease-blueprint-lib --example operator_api

use gpu_lease_blueprint_lib::api::operator_api_router;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listen = std::env::var("OPERATOR_API_LISTEN").unwrap_or_else(|_| "127.0.0.1:9200".into());
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    eprintln!("operator API listening on http://{listen} (inventory from GPU_INVENTORY_JSON)");
    axum::serve(listener, operator_api_router()).await?;
    Ok(())
}
