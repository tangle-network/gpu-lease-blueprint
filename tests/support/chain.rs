//! Existing real-contract deployment and transaction boundary shared by integration fixtures.
use anyhow::{Context, Result, ensure};
use blueprint_sdk::alloy::{
    primitives::{Address, Bytes, U256},
    providers::Provider,
    rpc::types::TransactionRequest,
};
pub async fn tx<P: Provider>(
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

pub async fn deploy<P: Provider>(
    p: &P,
    from: Address,
    name: &str,
    args: Vec<u8>,
) -> Result<Address> {
    let path = format!(
        "{}/../contracts/out/{name}.sol/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let artifact: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).context("run forge build")?)?;
    let object = artifact["bytecode"]["object"]
        .as_str()
        .context("missing bytecode")?;
    let mut code = hex::decode(object.trim_start_matches("0x"))?;
    code.extend(args);
    tx(p, from, None, code, U256::ZERO)
        .await?
        .contract_address
        .context(path)
}
