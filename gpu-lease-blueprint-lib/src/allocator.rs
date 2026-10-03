//! Co-located GPU allocator — SPEC §2 Placement (v1) and §4 (no on-chain
//! inventory registry). Operators advertise capability off-chain; this
//! allocator is the operator-local truth about devices.
//!
//! Overstay impossibility (SPEC I2) is enforced HERE on the credential side:
//! a session's device access ends at `expires_at`, and `reap_expired`
//! (called by the bin's periodic sweep) frees devices whose escrow ran out.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum AllocatorError {
    #[error("no idle device of class {class} (tee={tee}) available")]
    NoDeviceAvailable { class: String, tee: bool },
    #[error("unknown lease 0x{}", hex::encode(.0))]
    UnknownLease([u8; 32]),
    #[error("lease 0x{} already allocated", hex::encode(.0))]
    LeaseAlreadyAllocated([u8; 32]),
    #[error("lease 0x{} not live", hex::encode(.0))]
    LeaseNotLive([u8; 32]),
}

/// A physical, co-located GPU. GPU generations (H100 → B200 → …) are DATA
/// here — invisible to the contract forever (SPEC §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuDevice {
    pub id: String,
    /// Free-form class label advertised in quotes ("h100", "a100-80gb", ...).
    pub gpu_class: String,
    /// Whether the device is inside a TEE (quotes bound confidentiality →
    /// a non-TEE device structurally cannot serve a TEE-bound quote).
    pub tee: bool,
    /// CUDA device ordinal for local-attach (v1 placement).
    pub cuda_ordinal: u8,
}

/// An active lease session binding a device to a leaseId.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allocation {
    pub lease_id: [u8; 32],
    pub device_id: String,
    /// Unix second at which escrow ends — credentials must be revoked at/before.
    pub expires_at: u64,
    /// Lessee's EVM address (hex, lowercase) — the credential session owner.
    pub lessee: String,
    /// v1 endpoint descriptor (public data; versioned discriminated union).
    pub endpoint_v1: String,
}

#[derive(Debug, Default)]
struct AllocatorState {
    devices: Vec<GpuDevice>,
    /// device_id -> lease_id for busy devices.
    busy: HashMap<String, [u8; 32]>,
    /// lease_id -> allocation.
    live: HashMap<[u8; 32], Allocation>,
}

/// Operator-local allocator. All mutation goes through `&self` (interior
/// mutability) so it can live in a `OnceLock`/`OnceCell` static.
#[derive(Debug, Default)]
pub struct GpuAllocator {
    state: Mutex<AllocatorState>,
}

impl GpuAllocator {
    pub fn new(devices: Vec<GpuDevice>) -> Self {
        Self {
            state: Mutex::new(AllocatorState {
                devices,
                busy: HashMap::new(),
                live: HashMap::new(),
            }),
        }
    }

    /// Inventory from `GPU_INVENTORY_JSON` (hot-reloadable operator config —
    /// SPEC §2: operator policy is env/config, never a contract change).
    /// Empty inventory is valid (operator joins later); a parse failure is not.
    pub fn from_env() -> Self {
        let devices = std::env::var("GPU_INVENTORY_JSON")
            .ok()
            .and_then(|json| serde_json::from_str::<Vec<GpuDevice>>(&json).ok())
            .unwrap_or_default();
        Self::new(devices)
    }

    /// Bind an idle device to a lease. Fails closed when the requested class
    /// or TEE binding cannot be honored (quote binding, SPEC §1).
    pub fn acquire(
        &self,
        gpu_class: &str,
        tee_required: bool,
        lease_id: [u8; 32],
        lessee: &str,
        duration_seconds: u64,
    ) -> Result<Allocation, AllocatorError> {
        let mut st = self.state.lock().expect("allocator poisoned");
        if st.live.contains_key(&lease_id) {
            return Err(AllocatorError::LeaseAlreadyAllocated(lease_id));
        }
        let expires_at = unix_now() + duration_seconds;
        let device = st
            .devices
            .iter()
            .find(|d| {
                d.gpu_class == gpu_class && (!tee_required || d.tee) && !st.busy.contains_key(&d.id)
            })
            .ok_or_else(|| AllocatorError::NoDeviceAvailable {
                class: gpu_class.to_string(),
                tee: tee_required,
            })?
            .clone();
        let endpoint_v1 = endpoint_descriptor_v1(&device);
        let alloc = Allocation {
            lease_id,
            device_id: device.id.clone(),
            expires_at,
            lessee: lessee.to_string(),
            endpoint_v1,
        };
        st.busy.insert(device.id.clone(), lease_id);
        st.live.insert(lease_id, alloc.clone());
        Ok(alloc)
    }

    /// Free the device bound to a lease (RELEASE path — voluntary).
    pub fn release(&self, lease_id: [u8; 32]) -> Result<Allocation, AllocatorError> {
        let mut st = self.state.lock().expect("allocator poisoned");
        let alloc = st
            .live
            .remove(&lease_id)
            .ok_or(AllocatorError::UnknownLease(lease_id))?;
        st.busy.remove(&alloc.device_id);
        Ok(alloc)
    }

    /// Extend a live session's expiry (EXTEND path — escrow topped up on-chain).
    pub fn extend(&self, lease_id: [u8; 32], add_seconds: u64) -> Result<u64, AllocatorError> {
        let mut st = self.state.lock().expect("allocator poisoned");
        let alloc = st
            .live
            .get_mut(&lease_id)
            .ok_or(AllocatorError::UnknownLease(lease_id))?;
        alloc.expires_at += add_seconds;
        Ok(alloc.expires_at)
    }

    /// Sweep expired sessions (REAP path — permissionless, idempotent).
    /// Returns the freed allocations whose escrow had run out.
    pub fn reap_expired(&self) -> Vec<Allocation> {
        let mut st = self.state.lock().expect("allocator poisoned");
        let now = unix_now();
        let expired: Vec<[u8; 32]> = st
            .live
            .values()
            .filter(|a| a.expires_at <= now)
            .map(|a| a.lease_id)
            .collect();
        expired
            .into_iter()
            .filter_map(|id| {
                let alloc = st.live.remove(&id)?;
                st.busy.remove(&alloc.device_id);
                Some(alloc)
            })
            .collect()
    }

    pub fn allocation(&self, lease_id: [u8; 32]) -> Option<Allocation> {
        self.state
            .lock()
            .expect("allocator poisoned")
            .live
            .get(&lease_id)
            .cloned()
    }

    /// The operator's advertised inventory (public data — SPEC §4:
    /// operator-local truth, never an on-chain registry).
    pub fn inventory(&self) -> Vec<GpuDevice> {
        self.state
            .lock()
            .expect("allocator poisoned")
            .devices
            .clone()
    }

    pub fn idle_count(&self, gpu_class: &str, tee_required: bool) -> usize {
        let st = self.state.lock().expect("allocator poisoned");
        st.devices
            .iter()
            .filter(|d| {
                d.gpu_class == gpu_class && (!tee_required || d.tee) && !st.busy.contains_key(&d.id)
            })
            .count()
    }
}

/// v1 endpoint descriptor: the versioned discriminated union of attach
/// transports (SPEC §2 Placement). Later transports (network-cuda, ...) are
/// new schema versions, not redesigns.
pub fn endpoint_descriptor_v1(device: &GpuDevice) -> String {
    serde_json::json!({
        "v": 1,
        "transport": "local-attach",
        "device": device.cuda_ordinal,
    })
    .to_string()
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, class: &str, tee: bool) -> GpuDevice {
        GpuDevice {
            id: id.into(),
            gpu_class: class.into(),
            tee,
            cuda_ordinal: 0,
        }
    }

    #[test]
    fn acquire_binds_class_and_tee_exclusively() {
        let a = GpuAllocator::new(vec![dev("d1", "h100", false), dev("d2", "h100-tee", true)]);
        let l = [7u8; 32];
        let alloc = a.acquire("h100", false, l, "0xlessee", 60).unwrap();
        assert_eq!(alloc.device_id, "d1");
        assert_eq!(a.idle_count("h100", false), 0, "device now busy");
        // Same lease cannot double-allocate (single-settlement on the operator side).
        assert!(matches!(
            a.acquire("h100", false, l, "0xlessee", 60),
            Err(AllocatorError::LeaseAlreadyAllocated(_))
        ));
    }

    #[test]
    fn tee_binding_fails_closed() {
        let a = GpuAllocator::new(vec![dev("d1", "h100", false)]);
        assert!(matches!(
            a.acquire("h100", true, [1u8; 32], "0xlessee", 60),
            Err(AllocatorError::NoDeviceAvailable { .. })
        ));
    }

    #[test]
    fn release_then_reacquire() {
        let a = GpuAllocator::new(vec![dev("d1", "h100", false)]);
        let l = [1u8; 32];
        a.acquire("h100", false, l, "0xlessee", 60).unwrap();
        a.release(l).unwrap();
        assert_eq!(a.idle_count("h100", false), 1);
        a.acquire("h100", false, [2u8; 32], "0xother", 60).unwrap();
    }

    #[test]
    fn reap_expired_only_after_expiry() {
        let a = GpuAllocator::new(vec![dev("d1", "h100", false)]);
        let l = [9u8; 32];
        let alloc = a.acquire("h100", false, l, "0xlessee", 0).unwrap();
        // duration 0 → already expired → reap frees it.
        let freed = a.reap_expired();
        assert_eq!(freed.len(), 1);
        assert_eq!(freed[0].lease_id, l);
        assert!(a.allocation(l).is_none());
        assert!(matches!(a.release(l), Err(AllocatorError::UnknownLease(_))));
        let _ = alloc;
    }

    #[test]
    fn extend_pushes_expiry() {
        let a = GpuAllocator::new(vec![dev("d1", "h100", false)]);
        let l = [3u8; 32];
        let alloc = a.acquire("h100", false, l, "0xlessee", 100).unwrap();
        let new_exp = a.extend(l, 50).unwrap();
        assert_eq!(new_exp, alloc.expires_at + 50);
    }

    #[test]
    fn endpoint_v1_shape() {
        let d = dev("d1", "h100", false);
        let ep = endpoint_descriptor_v1(&d);
        let v: serde_json::Value = serde_json::from_str(&ep).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["transport"], "local-attach");
    }
}
