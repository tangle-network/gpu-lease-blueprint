//! Operator HTTP API — the UI's fuel (SPEC §2 off-chain surfaces).
//!
//! State-changing operations go through the TangleDriver (on-chain jobs);
//! this API serves the read-only + ephemeral surfaces the driver probes:
//!
//!   GET  /api/capabilities       discovery: kind, classes, prices, idle counts
//!   POST /api/quote              RFQ quote from the hot-reloadable policy
//!   GET  /api/leases/:leaseId    public lease status
//!   GET  /api/leases/:leaseId/access  lease-scoped bearer authorization
//!   POST /api/session/challenge  EIP-191 challenge bound to (lease, lessee)
//!   POST /api/session/verify     signature → scoped, expiring credential
//!
//! Everything derives from the same single truths the jobs use: the global
//! allocator, SDK authentication, and `QuotePolicy::from_env()`
//! (read per request — hot-reloadable, never a contract change).

use axum::Json as AxumJson;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::allocator::{Allocation, GpuAllocator};
use crate::chain::ChainConfig;
use crate::quote::{QuoteInputs, QuotePolicy, QuoteValidationError};
use blueprint_sdk::alloy::{
    primitives::Signature,
    signers::{Signer, local::PrivateKeySigner},
};
use blueprint_sdk::auth::{
    generate_challenge, paseto_tokens::PasetoTokenManager, types::ServiceId,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
    time::Duration,
};

/// Operator inventory and SDK authentication state shared by its HTTP routes.
#[derive(Clone)]
pub struct ApiState {
    pub allocator: Arc<GpuAllocator>,
    pub auth: Arc<LeaseAuth>,
}

impl ApiState {
    /// Product path: the process globals (`Arc` around the statics' refs).
    pub fn from_globals() -> Self {
        static AUTH: once_cell::sync::Lazy<Arc<LeaseAuth>> =
            once_cell::sync::Lazy::new(|| Arc::new(LeaseAuth::new()));
        Self {
            allocator: crate::allocator(),
            auth: Arc::clone(&AUTH),
        }
    }
}

/// The public capability descriptor — the shape the TangleDriver's compute
/// widgets consume (probed, never registered; unknown schemaVersion fails
/// closed on the reader side).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub schema_version: u16,
    pub compute: ComputeCapabilities,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeCapabilities {
    pub kind: &'static str,
    pub classes: Vec<ClassCapability>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassCapability {
    pub id: String,
    pub tee: bool,
    pub unit: &'static str,
    /// Indicative price (policy-priced at current idle utilization) — the
    /// binding price is always the signed quote.
    pub price_per_second: String,
    pub idle_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest {
    pub gpu_class: String,
    pub duration_seconds: u64,
    #[serde(default)]
    pub confidentiality: u8,
    #[serde(default)]
    pub region: Option<String>,
    /// Devices to rent — escrow scales per device (default 1).
    #[serde(default = "one_device")]
    pub device_count: u16,
}

fn one_device() -> u16 {
    1
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub schema_version: u16,
    pub gpu_class: String,
    pub price_per_second: String,
    pub duration_seconds: u64,
    pub confidentiality: u8,
    pub device_count: u16,
    pub escrow_total: String,
    /// Current utilization the price was struck at — the driver renders the
    /// "why" (surge indicator) from this.
    pub utilization_bps: u32,
    /// keccak256 of the canonical intent — what the RFQ signature binds and
    /// what the vault stores immutably (I5).
    pub intent_hash: String,
    /// Quote validity window (unix seconds).
    pub valid_until: u64,
    /// The operator's EVM address (derived from the quote signing key). Present
    /// iff signed; the client MUST verify the signature and use the recovered
    /// address — never a config-trusted one.
    pub operator_address: Option<String>,
    /// EIP-191 personal_sign over [`canonical_quote_payload`] (0x-prefixed RSV).
    pub signature: Option<String>,
}

/// The exact string the operator signs over a quote — every priced field is
/// bound. The SDK mirrors this byte-for-byte (`gpu-lease/quotes.ts`).
pub fn canonical_quote_payload(
    gpu_class: &str,
    price_per_second: &str,
    duration_seconds: u64,
    confidentiality: u8,
    device_count: u16,
    escrow_total: &str,
    intent_hash: &str,
    valid_until: u64,
) -> String {
    format!(
        "gpu-lease-quote|v2|{gpu_class}|{price_per_second}|{duration_seconds}|{confidentiality}|{device_count}|{escrow_total}|{intent_hash}|{valid_until}"
    )
}

pub fn quote_signer() -> Result<PrivateKeySigner, String> {
    std::env::var("GPU_QUOTE_SIGNING_KEY")
        .map_err(|_| "GPU_QUOTE_SIGNING_KEY is required".to_string())?
        .parse()
        .map_err(|_| "invalid GPU_QUOTE_SIGNING_KEY".to_string())
}

pub struct LeaseAuth {
    manager: PasetoTokenManager,
    challenges: Mutex<HashMap<[u8; 32], Challenge>>,
}
#[derive(Clone)]
struct Challenge {
    lease: [u8; 32],
    lessee: String,
    message: String,
    expiry: u64,
}
impl LeaseAuth {
    pub fn new() -> Self {
        Self {
            manager: PasetoTokenManager::new(Duration::from_secs(300)),
            challenges: Mutex::new(HashMap::new()),
        }
    }
}

/// Public lease status — endpoint descriptor + session facts. NEVER contains
/// credential material (SPEC §2: credentials are delivered via session, and
/// nothing here is on-chain).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseStatus {
    pub schema_version: u16,
    pub lease_id: String,
    /// Every device bound to the lease (one per rented GPU).
    pub device_ids: Vec<String>,
    pub lessee: String,
    pub expires_at: u64,
    pub endpoint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeRequest {
    pub lease_id: String,
    pub lessee: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeResponse {
    pub nonce: String,
    /// The exact string the lessee must EIP-191 personal_sign.
    pub message: String,
    pub expires_at: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    pub nonce: String,
    /// 65-byte RSV hex (0x-prefixed), EIP-191 personal_sign over `message`.
    pub signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResponse {
    pub token: String,
    pub lease_id: String,
    pub scopes: Vec<String>,
    pub expires_at: u64,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

type ApiResult<T> = Result<AxumJson<T>, (StatusCode, AxumJson<ApiError>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, AxumJson<ApiError>) {
    (status, AxumJson(ApiError { error: msg.into() }))
}

fn parse_lease_id(hex_str: &str) -> Result<[u8; 32], (StatusCode, AxumJson<ApiError>)> {
    let bytes = hex::decode(hex_str.trim_start_matches("0x"))
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("leaseId hex: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| err(StatusCode::BAD_REQUEST, "leaseId must be 32 bytes"))
}

/// Browser clients hit operator APIs cross-origin by design (the sandbox web
/// app is a first-class consumer). Allowed origins default to any — quotes
/// and capabilities are public data; state-changing surfaces are not served
/// here (jobs go through tnt-core; credentials are session-scoped).
fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::Any;
    tower_http::cors::CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}

/// Build the operator API router over injectable state.
pub fn operator_api_router_with(state: ApiState) -> axum::Router {
    axum::Router::new()
        .route("/api/capabilities", get(capabilities))
        .route("/api/quote", post(quote))
        .route("/api/leases/{leaseId}", get(lease_status))
        .route("/api/leases/{leaseId}/access", get(lease_access))
        .route("/api/session/challenge", post(session_challenge))
        .route("/api/session/verify", post(session_verify))
        .layer(cors_layer())
        .with_state(state)
}

/// Build the operator API router over the process globals (product path).
pub fn operator_api_router() -> axum::Router {
    operator_api_router_with(ApiState::from_globals())
}

fn class_idle_utilization(state: &ApiState, class: &str) -> u32 {
    // Utilization signal derived from the operator's own inventory: no idle
    // devices of the class => saturated (10_000 bps), otherwise scale by
    // total-vs-idle. Deliberately simple; the policy curve does the pricing.
    let total = state
        .allocator
        .inventory()
        .iter()
        .filter(|d| d.gpu_class == class)
        .count();
    let idle = state.allocator.idle_count(class, false);
    if total == 0 || idle == 0 {
        10_000
    } else {
        ((total - idle) * 10_000 / total).min(10_000) as u32
    }
}

async fn capabilities(State(state): State<ApiState>) -> ApiResult<Capabilities> {
    let policy = QuotePolicy::from_env().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let mut classes: Vec<ClassCapability> = Vec::new();
    for (id, base) in policy.base_price_per_second.iter() {
        let tee = state
            .allocator
            .inventory()
            .iter()
            .any(|d| d.gpu_class == *id && d.tee);
        classes.push(ClassCapability {
            id: id.clone(),
            tee,
            unit: "second",
            price_per_second: base.to_string(),
            idle_count: state.allocator.idle_count(id, false),
        });
    }
    Ok(AxumJson(Capabilities {
        schema_version: crate::SCHEMA_VERSION,
        compute: ComputeCapabilities {
            kind: "gpu",
            classes,
        },
    }))
}

async fn quote(
    State(state): State<ApiState>,
    AxumJson(req): AxumJson<QuoteRequest>,
) -> ApiResult<QuoteResponse> {
    let policy = QuotePolicy::from_env().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let utilization_bps = class_idle_utilization(&state, &req.gpu_class);
    let inputs = QuoteInputs {
        gpu_class: req.gpu_class.clone(),
        duration_seconds: req.duration_seconds,
        confidentiality: req.confidentiality,
        device_count: req.device_count,
        utilization_bps,
    };
    let quote = policy
        .quote(&inputs)
        .map_err(|e: QuoteValidationError| err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let region = req.region.clone().unwrap_or_else(|| "any".to_string());
    let intent = crate::jobs::intent_hash(
        crate::INTENT_VERSION,
        &req.gpu_class,
        req.duration_seconds,
        req.confidentiality,
        &region,
        req.device_count,
    );
    let intent_hash = format!("0x{}", hex::encode(intent));
    let valid_until = crate::allocator::unix_now() + 60;

    let signer = quote_signer().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let payload = canonical_quote_payload(
        &req.gpu_class,
        &quote.price_per_second.to_string(),
        req.duration_seconds,
        req.confidentiality,
        req.device_count,
        &quote.escrow_total.to_string(),
        &intent_hash,
        valid_until,
    );
    let signature = Some(
        signer
            .sign_message(payload.as_bytes())
            .await
            .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "quote signing failed"))?
            .to_string(),
    );
    let operator_address = Some(format!("{:#x}", signer.address()));

    Ok(AxumJson(QuoteResponse {
        schema_version: crate::SCHEMA_VERSION,
        gpu_class: quote.gpu_class,
        price_per_second: quote.price_per_second.to_string(),
        duration_seconds: quote.duration_seconds,
        confidentiality: quote.confidentiality,
        device_count: quote.device_count,
        escrow_total: quote.escrow_total.to_string(),
        utilization_bps,
        intent_hash,
        valid_until,
        operator_address,
        signature,
    }))
}

async fn lease_status(
    State(state): State<ApiState>,
    Path(lease_id): Path<String>,
) -> ApiResult<LeaseStatus> {
    let id = parse_lease_id(&lease_id)?;
    let alloc: Allocation = state
        .allocator
        .allocation(id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown or settled lease"))?;
    Ok(AxumJson(LeaseStatus {
        schema_version: crate::SCHEMA_VERSION,
        lease_id: format!("0x{}", hex::encode(id)),
        device_ids: alloc.device_ids,
        lessee: alloc.lessee,
        expires_at: alloc.expires_at,
        endpoint: alloc.endpoint_v1,
    }))
}

async fn funded_allocation(
    state: &ApiState,
    id: [u8; 32],
) -> Result<Allocation, (StatusCode, AxumJson<ApiError>)> {
    let a = state
        .allocator
        .allocation(id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown or settled lease"))?;
    let lease = ChainConfig::from_env()
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?
        .lease(id)
        .await
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    lease
        .require_allocation(&a)
        .map_err(|e| err(StatusCode::FORBIDDEN, e))?;
    if !state.allocator.allocation(id).is_some_and(|current| {
        current.device_ids == a.device_ids && current.expires_at == a.expires_at
    }) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "lease changed during authorization",
        ));
    }
    if a.expires_at <= crate::allocator::unix_now() {
        return Err(err(StatusCode::FORBIDDEN, "lease expired"));
    }
    Ok(a)
}

async fn session_challenge(
    State(state): State<ApiState>,
    AxumJson(req): AxumJson<ChallengeRequest>,
) -> ApiResult<ChallengeResponse> {
    let id = parse_lease_id(&req.lease_id)?;
    let a = funded_allocation(&state, id).await?;
    if !a.lessee.eq_ignore_ascii_case(&req.lessee) {
        return Err(err(StatusCode::FORBIDDEN, "not the lease's lessee"));
    }
    let now = crate::allocator::unix_now();
    let expiry = a.expires_at.min(now.saturating_add(300));
    let nonce = generate_challenge(&mut blueprint_sdk::std::BlueprintRng::new());
    let message = format!(
        "GPU lease access\nlease: 0x{}\nnonce: 0x{}\nexpires: {}",
        hex::encode(id),
        hex::encode(nonce),
        expiry
    );
    let mut challenges = state.auth.challenges.lock().unwrap();
    challenges.retain(|_, c| c.expiry > now);
    if challenges.len() >= 4096 {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many pending challenges",
        ));
    }
    challenges.insert(
        nonce,
        Challenge {
            lease: id,
            lessee: a.lessee,
            message: message.clone(),
            expiry,
        },
    );
    Ok(AxumJson(ChallengeResponse {
        nonce: format!("0x{}", hex::encode(nonce)),
        message,
        expires_at: expiry,
    }))
}

async fn session_verify(
    State(state): State<ApiState>,
    AxumJson(req): AxumJson<VerifyRequest>,
) -> ApiResult<VerifyResponse> {
    let nonce = parse_lease_id(&req.nonce)?;
    let challenge = state
        .auth
        .challenges
        .lock()
        .unwrap()
        .get(&nonce)
        .cloned()
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "unknown challenge"))?;
    let a = funded_allocation(&state, challenge.lease).await?;
    let now = crate::allocator::unix_now();
    if challenge.expiry <= now {
        return Err(err(StatusCode::UNAUTHORIZED, "challenge expired"));
    }
    let signature: Signature = req
        .signature
        .parse()
        .map_err(|_| err(StatusCode::UNAUTHORIZED, "invalid signature"))?;
    let recovered = signature
        .recover_address_from_msg(&challenge.message)
        .map_err(|_| err(StatusCode::UNAUTHORIZED, "signature recovery failed"))?;
    if !format!("{recovered:#x}").eq_ignore_ascii_case(&challenge.lessee)
        || !a.lessee.eq_ignore_ascii_case(&challenge.lessee)
    {
        return Err(err(StatusCode::UNAUTHORIZED, "signer is not lessee"));
    }
    if state
        .auth
        .challenges
        .lock()
        .unwrap()
        .remove(&nonce)
        .is_none()
    {
        return Err(err(StatusCode::UNAUTHORIZED, "challenge already consumed"));
    }
    let scopes = vec!["gpu:attach".into(), "gpu:exec".into()];
    let token = state
        .auth
        .manager
        .generate_token(
            ServiceId::new(0),
            hex::encode(challenge.lease),
            Some(challenge.lessee),
            BTreeMap::new(),
            Some(Duration::from_secs(a.expires_at.saturating_sub(now))),
            Some(scopes.clone()),
        )
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "token issuance failed"))?;
    Ok(AxumJson(VerifyResponse {
        token,
        lease_id: format!("0x{}", hex::encode(challenge.lease)),
        scopes,
        expires_at: a.expires_at,
    }))
}

async fn lease_access(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<LeaseStatus> {
    let id = parse_lease_id(&id)?;
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bearer credential required"))?;
    let claims = state
        .auth
        .manager
        .validate_token(token)
        .map_err(|_| err(StatusCode::UNAUTHORIZED, "invalid or expired credential"))?;
    if claims.key_id != hex::encode(id)
        || claims.service_id != ServiceId::new(0)
        || !claims
            .scopes
            .as_ref()
            .is_some_and(|s| s.iter().any(|s| s == "gpu:attach"))
    {
        return Err(err(
            StatusCode::FORBIDDEN,
            "credential does not authorize this lease",
        ));
    }
    let a = funded_allocation(&state, id).await?;
    if claims.tenant_id.as_deref() != Some(a.lessee.as_str()) {
        return Err(err(StatusCode::FORBIDDEN, "credential owner mismatch"));
    }
    Ok(AxumJson(LeaseStatus {
        schema_version: crate::SCHEMA_VERSION,
        lease_id: format!("0x{}", hex::encode(id)),
        device_ids: a.device_ids,
        lessee: a.lessee,
        expires_at: a.expires_at,
        endpoint: a.endpoint_v1,
    }))
}
