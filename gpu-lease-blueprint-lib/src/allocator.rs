//! Allocation of operator-owned hardware. Vault state authorizes mutations;
//! the journal reserves devices across crashes before a result is submitted.
use blueprint_sdk::tee::TeeProvider;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, thiserror::Error)]
pub enum AllocatorError {
    #[error("not enough idle devices of class {class}: requested {requested}, idle {idle}")]
    NotEnoughIdleDevices {
        class: String,
        requested: u16,
        idle: usize,
    },
    #[error("unknown lease 0x{}", hex::encode(.0))]
    UnknownLease([u8; 32]),
    #[error("lease already allocated")]
    LeaseAlreadyAllocated,
    #[error("invalid allocation: {0}")]
    Invalid(String),
    #[error("allocation journal: {0}")]
    Journal(String),
}

pub fn tee_from_wire(value: u8) -> Result<Option<TeeProvider>, AllocatorError> {
    match value {
        0 => Ok(None),
        1 => Ok(Some(TeeProvider::AwsNitro)),
        2 => Ok(Some(TeeProvider::IntelTdx)),
        3 => Ok(Some(TeeProvider::AmdSevSnp)),
        _ => Err(AllocatorError::Invalid("unknown TEE type".into())),
    }
}

/// UNVERIFIED operator input; these fields are not signed device attestation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuDevice {
    pub id: String,
    pub gpu_class: String,
    pub tee: bool,
    #[serde(default)]
    pub tee_type: Option<TeeProvider>,
    pub cuda_ordinal: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub lease_id: [u8; 32],
    pub device_ids: Vec<String>,
    pub gpu_class: String,
    pub intent_hash: [u8; 32],
    pub tee_required: bool,
    pub tee_provider: Option<TeeProvider>,
    pub expires_at: u64,
    pub lessee: String,
    pub endpoint_v1: String,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Debug, Default, Clone)]
struct AllocatorState {
    live: HashMap<[u8; 32], Allocation>,
}

#[derive(Debug)]
pub struct GpuAllocator {
    devices: Vec<GpuDevice>,
    state: Mutex<AllocatorState>,
    path: Option<PathBuf>,
    _lock: Option<File>,
}

impl GpuAllocator {
    pub fn new(devices: Vec<GpuDevice>) -> Self {
        Self {
            devices,
            state: Mutex::new(AllocatorState::default()),
            path: None,
            _lock: None,
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let devices: Vec<GpuDevice> = serde_json::from_str(
            &std::env::var("GPU_INVENTORY_JSON").map_err(|_| "GPU_INVENTORY_JSON is required")?,
        )
        .map_err(|_| "invalid GPU_INVENTORY_JSON")?;
        tracing::warn!(
            "GPU_INVENTORY_JSON is UNVERIFIED operator input; device attestation trust root is pending"
        );
        let mut ids = HashSet::new();
        let mut ordinals = HashSet::new();
        for d in &devices {
            if d.id.is_empty()
                || d.gpu_class.is_empty()
                || !ids.insert(d.id.clone())
                || !ordinals.insert(d.cuda_ordinal)
                || d.tee != d.tee_type.is_some()
            {
                return Err("invalid or duplicate GPU inventory device/ordinal/TEE".into());
            }
        }
        let path = PathBuf::from(
            std::env::var("GPU_LEASE_STATE_PATH")
                .map_err(|_| "GPU_LEASE_STATE_PATH is required")?,
        );
        if !path.is_absolute() {
            return Err("GPU_LEASE_STATE_PATH must be absolute".into());
        }
        let parent = path.parent().ok_or("allocation journal has no parent")?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("allocation journal directory: {e}"))?;
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|_| "allocation journal is already in use")?;
        let mut allocator = Self::new(devices);
        allocator.path = Some(path.clone());
        allocator._lock = Some(lock);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let saved: Vec<Allocation> =
                    serde_json::from_slice(&bytes).map_err(|_| "invalid allocation journal")?;
                let mut busy = HashSet::new();
                for a in saved {
                    let selected: Vec<&GpuDevice> = a
                        .device_ids
                        .iter()
                        .filter_map(|id| allocator.devices.iter().find(|d| d.id == *id))
                        .collect();
                    let endpoint = serde_json::json!({"v":1,"transport":"local-attach","devices":selected.iter().map(|d|d.cuda_ordinal).collect::<Vec<_>>()});
                    if a.device_ids.is_empty()
                        || serde_json::from_str::<serde_json::Value>(&a.endpoint_v1).ok()
                            != Some(endpoint)
                        || selected.iter().any(|d| {
                            d.gpu_class != a.gpu_class
                                || (a.tee_required
                                    && (!d.tee
                                        || (a.tee_provider.is_some()
                                            && d.tee_type != a.tee_provider)))
                        })
                        || a.device_ids
                            .iter()
                            .any(|id| !ids.contains(id) || !busy.insert(id.clone()))
                        || allocator
                            .state
                            .get_mut()
                            .unwrap()
                            .live
                            .insert(a.lease_id, a)
                            .is_some()
                    {
                        return Err("allocation journal conflicts with inventory".into());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("allocation journal read: {e}")),
        }
        allocator
            .persist(&allocator.state.lock().unwrap())
            .map_err(|e| e.to_string())?;
        Ok(allocator)
    }

    fn persist(&self, state: &AllocatorState) -> Result<(), AllocatorError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let write = || -> Result<(), Box<dyn std::error::Error>> {
            let tmp = path.with_extension("pending");
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(&serde_json::to_vec(
                &state.live.values().collect::<Vec<_>>(),
            )?)?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            File::open(path.parent().unwrap())?.sync_all()?;
            Ok(())
        };
        write().map_err(|e| AllocatorError::Journal(e.to_string()))
    }

    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut AllocatorState) -> Result<T, AllocatorError>,
    ) -> Result<T, AllocatorError> {
        let mut state = self.state.lock().expect("allocator poisoned");
        let mut next = state.clone();
        let result = change(&mut next)?;
        self.persist(&next)?;
        *state = next;
        Ok(result)
    }

    pub fn acquire_until(
        &self,
        class: &str,
        tee_required: bool,
        tee: Option<TeeProvider>,
        count: u16,
        id: [u8; 32],
        lessee: &str,
        intent_hash: [u8; 32],
        expiry: u64,
    ) -> Result<Allocation, AllocatorError> {
        if expiry <= unix_now() || count == 0 {
            return Err(AllocatorError::Invalid(
                "lease expired or count zero".into(),
            ));
        }
        self.mutate(|state| {
            if state.live.contains_key(&id) { return Err(AllocatorError::LeaseAlreadyAllocated); }
            let busy: HashSet<&String> = state.live.values().flat_map(|a| &a.device_ids).collect();
            let idle: Vec<&GpuDevice> = self.devices.iter().filter(|d| d.gpu_class == class && !busy.contains(&d.id)
                && (!tee_required || (d.tee && (tee.is_none() || d.tee_type == tee)))).collect();
            if idle.len() < count as usize { return Err(AllocatorError::NotEnoughIdleDevices { class: class.into(), requested: count, idle: idle.len() }); }
            let devices = &idle[..count as usize];
            let a = Allocation { lease_id:id, device_ids:devices.iter().map(|d|d.id.clone()).collect(), gpu_class:class.into(),intent_hash,tee_required,tee_provider:tee,expires_at:expiry,
                lessee:lessee.to_ascii_lowercase(), endpoint_v1:serde_json::json!({"v":1,"transport":"local-attach","devices":devices.iter().map(|d|d.cuda_ordinal).collect::<Vec<_>>()}).to_string(), confirmed:false };
            state.live.insert(id,a.clone()); Ok(a)
        })
    }

    pub fn release(&self, id: [u8; 32]) -> Result<Allocation, AllocatorError> {
        self.mutate(|s| s.live.remove(&id).ok_or(AllocatorError::UnknownLease(id)))
    }

    pub fn confirm(&self, id: [u8; 32]) -> Result<(), AllocatorError> {
        self.mutate(|s| {
            s.live
                .get_mut(&id)
                .ok_or(AllocatorError::UnknownLease(id))?
                .confirmed = true;
            Ok(())
        })
    }

    pub fn sync_expiry(&self, id: [u8; 32], expiry: u64) -> Result<(), AllocatorError> {
        self.mutate(|s| {
            s.live
                .get_mut(&id)
                .ok_or(AllocatorError::UnknownLease(id))?
                .expires_at = expiry;
            Ok(())
        })
    }

    pub fn allocations(&self) -> Vec<Allocation> {
        self.state.lock().unwrap().live.values().cloned().collect()
    }
    pub fn allocation(&self, id: [u8; 32]) -> Option<Allocation> {
        self.state.lock().unwrap().live.get(&id).cloned()
    }
    pub fn inventory(&self) -> Vec<GpuDevice> {
        self.devices.clone()
    }
    pub fn idle_count(&self, class: &str, tee: bool) -> usize {
        let state = self.state.lock().unwrap();
        self.devices
            .iter()
            .filter(|d| {
                d.gpu_class == class
                    && (!tee || d.tee)
                    && !state.live.values().any(|a| a.device_ids.contains(&d.id))
            })
            .count()
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
