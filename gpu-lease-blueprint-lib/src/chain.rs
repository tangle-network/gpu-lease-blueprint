//! Vault reads are pinned to one block so authorization and paid expiry agree.
use crate::{Allocation, GpuLeaseRequest};
use blueprint_sdk::alloy::{
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder},
    sol,
};

sol! {
    #[sol(rpc)]
    interface Vault {
        function leases(bytes32 id) external view returns (address operator,address lessee,uint128 escrow,uint128 pricePerSecond,uint64 expiry,bytes32 intentHash,uint8 confidentiality,uint8 state,uint16 deviceCount);
    }
    #[sol(rpc)]
    interface Blueprint { function leaseOperatorOf(bytes32 id) external view returns (address); }
}

#[derive(Clone)]
pub struct ChainConfig {
    pub rpc: String,
    pub vault: Address,
    pub blueprint: Address,
    pub operator: Address,
}
#[derive(Debug)]
pub struct Lease {
    pub operator: Address,
    pub lessee: Address,
    pub escrow: u128,
    pub price: u128,
    pub expiry: u64,
    pub intent: B256,
    pub confidentiality: u8,
    pub state: u8,
    pub count: u16,
    pub now: u64,
    pub bound_operator: Address,
}
impl ChainConfig {
    pub fn from_env() -> Result<Self, String> {
        fn address(key: &str) -> Result<Address, String> {
            let value: Address = std::env::var(key)
                .map_err(|_| format!("{key} is required"))?
                .parse()
                .map_err(|_| format!("invalid {key}"))?;
            if value.is_zero() {
                return Err(format!("{key} must not be zero"));
            }
            Ok(value)
        }
        let rpc = std::env::var("HTTP_RPC_URL").map_err(|_| "HTTP_RPC_URL is required")?;
        let url: blueprint_sdk::alloy::transports::http::reqwest::Url =
            rpc.parse().map_err(|_| "invalid HTTP_RPC_URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("HTTP_RPC_URL must use http or https".into());
        }
        Ok(Self {
            rpc,
            vault: address("GPU_LEASE_VAULT_ADDRESS")?,
            blueprint: address("GPU_LEASE_BLUEPRINT_ADDRESS")?,
            operator: address("GPU_OPERATOR_ADDRESS")?,
        })
    }
    pub async fn validate(&self) -> Result<(), String> {
        let p = ProviderBuilder::new()
            .connect(&self.rpc)
            .await
            .map_err(|e| e.to_string())?;
        for a in [self.vault, self.blueprint] {
            if p.get_code_at(a)
                .await
                .map_err(|e| e.to_string())?
                .is_empty()
            {
                return Err("configured vault/blueprint has no deployed code".into());
            }
        }
        Ok(())
    }
    pub async fn lease(&self, id: [u8; 32]) -> Result<Lease, String> {
        let p = ProviderBuilder::new()
            .connect(&self.rpc)
            .await
            .map_err(|e| e.to_string())?;
        let block = p
            .get_block_by_number(Default::default())
            .await
            .map_err(|e| e.to_string())?
            .ok_or("missing latest block")?;
        let at = block.header.number.into();
        let l = Vault::new(self.vault, &p)
            .leases(id.into())
            .block(at)
            .call()
            .await
            .map_err(|e| e.to_string())?;
        let bound = Blueprint::new(self.blueprint, &p)
            .leaseOperatorOf(id.into())
            .block(at)
            .call()
            .await
            .map_err(|e| e.to_string())?;
        if l.operator != self.operator
            || l.lessee.is_zero()
            || l.pricePerSecond == 0
            || l.deviceCount == 0
        {
            return Err("unknown lease or wrong operator".into());
        }
        Ok(Lease {
            operator: l.operator,
            lessee: l.lessee,
            escrow: l.escrow,
            price: l.pricePerSecond,
            expiry: l.expiry,
            intent: l.intentHash,
            confidentiality: l.confidentiality,
            state: l.state,
            count: l.deviceCount,
            now: block.header.timestamp,
            bound_operator: bound,
        })
    }
}
impl Lease {
    pub fn require_live(&self) -> Result<(), String> {
        if self.state != 0 || self.expiry <= self.now {
            return Err("lease is settled or expired".into());
        }
        let cost = self
            .price
            .checked_mul(self.count.into())
            .and_then(|v| v.checked_mul((self.expiry - self.now).into()))
            .ok_or("lease cost overflow")?;
        if cost > self.escrow {
            return Err("lease escrow does not cover access".into());
        }
        Ok(())
    }
    pub fn require_owner(&self, caller: &str) -> Result<(), String> {
        if !format!("{:#x}", self.lessee).eq_ignore_ascii_case(caller) {
            return Err("not the lease's lessee".into());
        }
        Ok(())
    }
    pub fn require_allocation(&self, a: &Allocation) -> Result<(), String> {
        self.require_live()?;
        self.require_owner(&a.lessee)?;
        if self.bound_operator != self.operator
            || self.intent.0 != a.intent_hash
            || self.count as usize != a.device_ids.len()
            || a.expires_at > self.expiry
        {
            return Err("allocation is not bound to a funded chain result".into());
        }
        Ok(())
    }
    pub fn require_request(&self, r: &GpuLeaseRequest) -> Result<(), String> {
        self.require_live()?;
        if self.lessee != r.lessee
            || self.price != r.pricePerSecond
            || self.intent != r.intentHash
            || self.count != r.deviceCount
            || self.confidentiality != r.confidentiality
            || !self.bound_operator.is_zero()
        {
            return Err("lease request does not match the unallocated vault commitment".into());
        }
        Ok(())
    }
}

pub async fn reconcile(discard_unconfirmed: bool) -> Result<(), String> {
    let chain = ChainConfig::from_env()?;
    for a in crate::allocator().allocations() {
        let lease = chain.lease(a.lease_id).await?;
        if lease.state != 0
            || lease.expiry <= lease.now
            || (discard_unconfirmed && !a.confirmed && lease.bound_operator != chain.operator)
        {
            crate::allocator()
                .release(a.lease_id)
                .map_err(|e| e.to_string())?;
        } else if lease.bound_operator == chain.operator {
            lease.require_allocation(&a)?;
            crate::allocator()
                .confirm(a.lease_id)
                .map_err(|e| e.to_string())?;
            crate::allocator()
                .sync_expiry(a.lease_id, lease.expiry)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
