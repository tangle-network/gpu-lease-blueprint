//! Caller-owned cloud capacity deployment; separate from paid local-device leases.
use blueprint_sdk::{
    alloy::primitives::keccak256,
    remote::{
        CloudProvider, CloudProvisioner, InstanceStatus, ManagedResource, ProviderCredentials,
        ResourceSpec,
        managed::{ManagedLeaseIdentity, lock_lease, request_fingerprint},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

/// Maximum cloud configuration size, including credentials, in bytes.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;
type Result<T> = std::result::Result<T, String>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    credentials: Vec<ProviderCredentials>,
    spec: ResourceSpec,
    state_dir: PathBuf,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Account {
    provider: CloudProvider,
    digest: String,
}
#[derive(Deserialize, Serialize, PartialEq)]
enum Phase {
    Attempted,
    Live,
    Retiring,
    Retired,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    identity: ManagedLeaseIdentity,
    request: String,
    accounts: Vec<Account>,
    phase: Phase,
    capacity: Option<Capacity>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Capacity {
    provider: CloudProvider,
    id: String,
    binding: String,
    deadline: String,
}

fn accounts(raw: &serde_json::Value) -> Result<Vec<Account>> {
    raw["credentials"]
        .as_array()
        .ok_or("invalid cloud configuration")?
        .iter()
        .map(|value| {
            let provider = CloudProvider::from_catalog_key(
                value["provider"].as_str().ok_or("invalid cloud account")?,
            )
            .ok_or("invalid cloud account")?;
            let ordered: BTreeMap<String, serde_json::Value> =
                serde_json::from_value(value.clone()).map_err(|_| "invalid cloud account")?;
            let bytes = serde_json::to_vec(&ordered).map_err(|_| "invalid cloud account")?;
            Ok(Account {
                provider,
                digest: keccak256(bytes).to_string(),
            })
        })
        .collect()
}
fn operation(stage: &str, error: blueprint_sdk::remote::Error) -> String {
    match error {
        blueprint_sdk::remote::Error::Provider(code) => format!("cloud {stage}: {code}"),
        _ => format!("cloud {stage} failed"),
    }
}
fn save(path: &Path, record: &Record) -> Result<()> {
    crate::journal::write(path, record).map_err(|_| "cloud recovery state write failed".into())
}
fn private(path: &Path, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "cloud private state is unavailable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("cloud state must be private".into());
        }
        if !directory
            && (metadata.nlink() != 1
                || metadata.uid()
                    != fs::metadata(path.parent().unwrap())
                        .map_err(|_| "cloud state ownership unavailable")?
                        .uid())
        {
            return Err("invalid cloud state ownership".into());
        }
    }
    if metadata.file_type().is_symlink()
        || (if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        })
    {
        return Err("invalid cloud state path".into());
    }
    Ok(())
}
fn capacity(resource: &ManagedResource) -> Result<Capacity> {
    Ok(Capacity {
        provider: resource.instance.provider.clone(),
        id: resource.instance.id.clone(),
        binding: resource
            .request_fingerprint
            .clone()
            .ok_or("cloud resource has no recovery binding")?,
        deadline: resource
            .expires_at
            .ok_or("cloud resource has no deadline")?
            .to_rfc3339(),
    })
}
fn same_resource(saved: &Capacity, resource: &ManagedResource) -> bool {
    saved.provider == resource.instance.provider
        && saved.id == resource.instance.id
        && resource.request_fingerprint.as_deref() == Some(saved.binding.as_str())
        && resource
            .expires_at
            .is_some_and(|deadline| deadline.to_rfc3339() == saved.deadline)
}
async fn verify_account(
    provisioner: &CloudProvisioner,
    previous: &Account,
    current: &[Account],
    record: &Record,
) -> Result<()> {
    let now = current
        .iter()
        .find(|a| a.provider == previous.provider)
        .ok_or("original cloud account is unavailable")?;
    if now.digest == previous.digest {
        return Ok(());
    }
    if let Some(saved) = record
        .capacity
        .as_ref()
        .filter(|saved| saved.provider == previous.provider)
    {
        if matches!(provisioner.status_managed(&previous.provider,&record.identity,&saved.id).await,
            Ok(Some(ref resource)) if same_resource(saved,resource))
        {
            return Ok(());
        }
    }
    Err("rotated cloud credentials have not proved original account ownership".into())
}
async fn retire(
    provisioner: &CloudProvisioner,
    path: &Path,
    record: &mut Record,
    current: &[Account],
) -> Result<()> {
    record.phase = Phase::Retiring;
    let mut errors: Vec<String> = save(path, record).err().into_iter().collect();
    let mut absent = true;
    for account in &record.accounts {
        if let Err(error) = verify_account(provisioner, account, current, record).await {
            absent = false;
            errors.push(error);
            continue;
        }
        let id = record
            .capacity
            .as_ref()
            .filter(|r| r.provider == account.provider)
            .map(|r| r.id.as_str());
        if let Err(error) = provisioner
            .destroy_managed(&account.provider, &record.identity, id)
            .await
        {
            absent = false;
            errors.push(operation("retirement", error));
        }
    }
    record.phase = if absent {
        Phase::Retired
    } else {
        Phase::Retiring
    };
    if let Err(error) = save(path, record) {
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
async fn failed(
    provisioner: &CloudProvisioner,
    path: &Path,
    record: &mut Record,
    current: &[Account],
    failure: String,
) -> Result<()> {
    match retire(provisioner, path, record, current).await {
        Ok(()) => Err(failure),
        Err(cleanup) => Err(format!("{failure}; {cleanup}")),
    }
}

fn connection(value: Option<&str>, ssh: bool) -> Option<&str> {
    let value = value?;
    let url = value
        .parse::<blueprint_sdk::alloy::transports::http::reqwest::Url>()
        .ok()?;
    (url.host_str().is_some()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && if ssh {
            url.scheme() == "ssh" && !url.username().is_empty() && url.port().is_some()
        } else {
            matches!(url.scheme(), "http" | "https") && url.username().is_empty()
        })
    .then_some(value)
}
fn wait_to_deadline(resource: &ManagedResource) -> Result<Duration> {
    Ok((resource
        .expires_at
        .ok_or("cloud resource has no deadline")?
        - chrono::Utc::now())
    .to_std()
    .unwrap_or_default())
}
async fn readiness(
    provisioner: &CloudProvisioner,
    spec: &ResourceSpec,
    ttl: Duration,
    mut resource: ManagedResource,
) -> Result<ManagedResource> {
    let expected = capacity(&resource)?;
    loop {
        if wait_to_deadline(&resource)?.is_zero() {
            return Err("cloud deadline reached before connection readiness".into());
        }
        if resource.instance.status == InstanceStatus::Running
            && (connection(resource.ssh_endpoint.as_deref(), true).is_some()
                || connection(resource.endpoint_url.as_deref(), false).is_some())
        {
            return Ok(resource);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        resource = provisioner
            .recover_lease(spec, ttl)
            .await
            .map_err(|e| operation("readiness", e))?
            .ok_or("cloud capacity disappeared during readiness")?
            .resource;
        if !same_resource(&expected, &resource) {
            return Err("cloud resource changed during readiness".into());
        }
    }
}

/// Deploy configured cloud capacity and await shutdown or SDK deadline retirement.
/// Credentials arrive on the caller's bounded stdin; only public connection facts are printed.
/// This never adds devices to the local marketplace allocator or changes its paid lease path.
pub async fn run(bytes: &[u8], shutdown: impl Future<Output = ()>) -> Result<()> {
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("cloud configuration is too large".into());
    }
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "invalid cloud configuration")?;
    let config: Config =
        serde_json::from_value(raw.clone()).map_err(|_| "invalid cloud configuration")?;
    let current_accounts = accounts(&raw)?;
    let spec = config.spec;
    spec.validate()
        .map_err(|_| "invalid cloud resource request")?;
    let identity = spec
        .ownership
        .as_ref()
        .ok_or("cloud ownership is required")?
        .clone();
    let ttl = Duration::from_secs(
        spec.lease_duration_secs
            .filter(|s| *s > 0)
            .ok_or("cloud duration is required")?,
    );
    let image = spec.image.as_deref().unwrap_or("");
    let immutable = image.rsplit_once("@sha256:").is_some_and(|(name, digest)| {
        !name.is_empty() && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
    });
    if spec
        .ssh_public_key
        .as_ref()
        .is_none_or(|key| key.trim().is_empty())
        || !immutable
        || spec.command.as_ref().is_none_or(|args| args.is_empty())
        || spec.gpu_class.is_none()
        || spec.gpu_count.is_none()
        || spec.max_hourly_cost_usd.is_none()
        || current_accounts.is_empty()
        || spec
            .env
            .keys()
            .any(|key| key.eq_ignore_ascii_case("GPU_CLOUD_CAPACITY"))
    {
        return Err(
            "cloud deployment requires explicit workload, shape, key and price policy".into(),
        );
    }
    let secrets: Vec<&str> = raw["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|credential| credential["api_key"].as_str())
        .filter(|key| !key.is_empty())
        .collect();
    if secrets.iter().any(|secret| {
        spec.env
            .values()
            .chain(spec.command.iter().flatten())
            .chain(spec.image.iter())
            .any(|value| value.contains(secret))
    }) {
        return Err("provider credentials cannot enter the workload".into());
    }
    if !config.state_dir.is_absolute() {
        return Err("cloud state directory must be absolute".into());
    }
    private(&config.state_dir, true)?;
    let fresh = fs::read_dir(&config.state_dir)
        .map_err(|_| "cloud state read failed")?
        .next()
        .is_none();
    let _lock = lock_lease(
        &config.state_dir,
        &ManagedLeaseIdentity {
            namespace: "cloud-capacity-store".into(),
            lease_id: "journal".into(),
        },
    )
    .await
    .map_err(|e| operation("identity lock", e))?;
    let path = config.state_dir.join("capacity.json");
    let request = request_fingerprint(&spec).map_err(|e| operation("request binding", e))?;
    let saved = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            private(&path, false)?;
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err("invalid cloud recovery state".into());
            }
            let bytes = fs::read(&path).map_err(|_| "cloud recovery state read failed")?;
            Some(
                serde_json::from_slice::<Record>(&bytes)
                    .map_err(|_| "invalid cloud recovery state")?,
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && fresh => None,
        Err(_) => return Err("cloud recovery state is missing or unreadable".into()),
    };
    let first_attempt = saved.is_none();
    let provisioner = CloudProvisioner::from_credentials(config.credentials)
        .map_err(|e| operation("account setup", e))?;
    let mut record = saved.unwrap_or_else(|| Record {
        identity: identity.clone(),
        request: request.clone(),
        accounts: current_accounts.clone(),
        phase: Phase::Attempted,
        capacity: None,
    });
    if record.identity != identity || record.request != request {
        return Err("cloud recovery request changed".into());
    }
    if record.accounts.is_empty() || (record.phase == Phase::Live && record.capacity.is_none()) {
        return Err("invalid cloud recovery state".into());
    }
    if record.phase == Phase::Retired {
        return Ok(());
    }
    let provisioner = if first_attempt {
        provisioner
            .initialize_lease_state_dir(&config.state_dir)
            .await
    } else {
        provisioner.with_lease_state_dir(&config.state_dir)
    }
    .map_err(|e| operation("authorization context", e))?;
    if record.phase == Phase::Retiring {
        return retire(&provisioner, &path, &mut record, &current_accounts).await;
    }
    for previous in &record.accounts {
        verify_account(&provisioner, previous, &current_accounts, &record).await?;
    }
    let original: Vec<_> = record.accounts.iter().map(|a| a.provider.clone()).collect();
    let provisioner = provisioner.only_providers(&original);
    let result = if first_attempt {
        let listed = provisioner
            .list_managed(&identity.namespace)
            .await
            .map_err(|e| operation("reconciliation", e))?;
        if listed
            .iter()
            .any(|r| r.lease_id == identity.lease_id && r.is_present())
        {
            return Err("cloud resource exists without recovery state".into());
        }
        save(&path, &record)?;
        provisioner.allocate(&spec, ttl).await.map(Some)
    } else {
        provisioner.recover_lease(&spec, ttl).await
    };
    let lease = match result {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            return failed(
                &provisioner,
                &path,
                &mut record,
                &current_accounts,
                "cloud capacity is absent; use a new deployment identity".into(),
            )
            .await;
        }
        Err(error) => {
            return failed(
                &provisioner,
                &path,
                &mut record,
                &current_accounts,
                operation("allocation or recovery", error),
            )
            .await;
        }
    };
    let selected = match capacity(&lease.resource) {
        Ok(capacity) => capacity,
        Err(error) => {
            return failed(&provisioner, &path, &mut record, &current_accounts, error).await;
        }
    };
    if record
        .capacity
        .as_ref()
        .is_some_and(|saved| !same_resource(saved, &lease.resource))
    {
        return failed(
            &provisioner,
            &path,
            &mut record,
            &current_accounts,
            "cloud recovery resource changed".into(),
        )
        .await;
    }
    if secrets
        .iter()
        .any(|secret| serde_json::to_string(&selected).is_ok_and(|value| value.contains(secret)))
    {
        return failed(
            &provisioner,
            &path,
            &mut record,
            &current_accounts,
            "cloud provider returned unsafe recovery metadata".into(),
        )
        .await;
    }
    record.accounts = current_accounts
        .iter()
        .filter(|account| account.provider == selected.provider)
        .cloned()
        .collect();
    record.capacity = Some(selected);
    if let Err(error) = save(&path, &record) {
        return failed(&provisioner, &path, &mut record, &current_accounts, error).await;
    }
    tokio::pin!(shutdown);
    let resource = tokio::select! {
        biased;
        _ = &mut shutdown => return retire(&provisioner,&path,&mut record,&current_accounts).await,
        ready = tokio::time::timeout(Duration::from_secs(60), readiness(&provisioner,&spec,ttl,lease.resource)) => match ready {
            Ok(Ok(resource)) => resource,
            _ => return failed(&provisioner,&path,&mut record,&current_accounts,"cloud connection readiness failed".into()).await,
        }
    };
    let receipt = serde_json::json!({"namespace":identity.namespace,"lease_id":identity.lease_id,
        "provider":resource.instance.provider,"id":resource.instance.id,
        "endpoint_url":connection(resource.endpoint_url.as_deref(),false),"ssh_endpoint":connection(resource.ssh_endpoint.as_deref(),true),
        "gpu_class":resource.gpu_class,"gpu_count":resource.gpu_count,"gpu_vram_gb":resource.gpu_vram_gb,
        "expires_at":resource.expires_at});
    if secrets
        .iter()
        .any(|secret| receipt.to_string().contains(secret))
    {
        return failed(
            &provisioner,
            &path,
            &mut record,
            &current_accounts,
            "cloud provider returned unsafe connection metadata".into(),
        )
        .await;
    }
    record.phase = Phase::Live;
    if let Err(error) = save(&path, &record) {
        return failed(&provisioner, &path, &mut record, &current_accounts, error).await;
    }
    use std::io::Write;
    if writeln!(std::io::stdout().lock(), "{receipt}").is_err() {
        return failed(
            &provisioner,
            &path,
            &mut record,
            &current_accounts,
            "cloud receipt delivery failed".into(),
        )
        .await;
    }
    let provisioner = provisioner.only_providers(&[resource.instance.provider.clone()]);
    let mut resource = resource;
    loop {
        let wait = wait_to_deadline(&resource)?;
        tokio::select! {
            _ = &mut shutdown => return retire(&provisioner, &path, &mut record, &current_accounts).await,
            _ = tokio::time::sleep(wait) => {
                let report = provisioner.retire_expired(&identity.namespace).await;
                if !report.failures.is_empty() {
                    return failed(&provisioner,&path,&mut record,&current_accounts,"cloud deadline reconciliation failed".into()).await;
                }
                if report.retired.contains(&identity) {
                    record.phase = Phase::Retired; save(&path, &record)?; return Ok(());
                }
                match provisioner.recover_lease(&spec,ttl).await {
                    Ok(Some(lease)) if same_resource(record.capacity.as_ref().unwrap(),&lease.resource) => resource = lease.resource,
                    _ => return failed(&provisioner,&path,&mut record,&current_accounts,"cloud capacity unavailable or changed".into()).await,
                }
            }
        }
    }
}
