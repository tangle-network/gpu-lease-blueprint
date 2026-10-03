//! Operator HTTP API — the UI's fuel (SPEC §2 off-chain surfaces).
//!
//! State-changing operations go through the TangleDriver (on-chain jobs);
//! this API serves the read-only + ephemeral surfaces the driver probes:
//!
//!   GET  /api/capabilities       discovery: kind, classes, prices, idle counts
//!   POST /api/quote              RFQ quote from the hot-reloadable policy
//!   GET  /api/leases/:leaseId    public lease status (NO credential material)
//!   POST /api/session/challenge  EIP-191 challenge bound to (lease, lessee)
//!   POST /api/session/verify     signature → scoped, expiring credential
//!
//! Everything derives from the same single truths the jobs use: the global
//! allocator, the global credential sessions, and `QuotePolicy::from_env()`
//! (read per request — hot-reloadable, never a contract change).

use axum::Json as AxumJson;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::allocator::{Allocation, GpuAllocator};
use crate::credentials::{CredentialSessions, ScopedCredential};
use crate::quote::{QuoteInputs, QuotePolicy, QuoteValidationError};

/// Injected dependencies — unit tests build their own; the product uses
/// [`crate::allocator()`] / [`crate::credentials()`] globals.
#[derive(Clone)]
pub struct ApiState {
    pub allocator: Arc<GpuAllocator>,
    pub credentials: Arc<CredentialSessions>,
}

impl ApiState {
    /// Product path: the process globals (`Arc` around the statics' refs).
    pub fn from_globals() -> Self {
        Self {
            allocator: crate::allocator(),
            credentials: crate::credentials(),
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub schema_version: u16,
    pub gpu_class: String,
    pub price_per_second: String,
    pub duration_seconds: u64,
    pub confidentiality: u8,
    pub escrow_total: String,
    /// Current utilization the price was struck at — the driver renders the
    /// "why" (surge indicator) from this.
    pub utilization_bps: u32,
    /// keccak256 of the canonical intent — what the RFQ signature binds and
    /// what the vault stores immutably (I5).
    pub intent_hash: String,
    /// Quote validity window (unix seconds).
    pub valid_until: u64,
}

/// Public lease status — endpoint descriptor + session facts. NEVER contains
/// credential material (SPEC §2: credentials are delivered via session, and
/// nothing here is on-chain).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseStatus {
    pub schema_version: u16,
    pub lease_id: String,
    pub device_id: String,
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

/// Build the operator API router over injectable state.
pub fn operator_api_router_with(state: ApiState) -> axum::Router {
    axum::Router::new()
        .route("/api/capabilities", get(capabilities))
        .route("/api/quote", post(quote))
        .route("/api/leases/{leaseId}", get(lease_status))
        .route("/api/session/challenge", post(session_challenge))
        .route("/api/session/verify", post(session_verify))
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
    let idle = state.allocator.idle_count(class, false) + state.allocator.idle_count(class, true);
    if total == 0 || idle == 0 {
        10_000
    } else {
        ((total - idle) * 10_000 / total).min(10_000) as u32
    }
}

async fn capabilities(State(state): State<ApiState>) -> ApiResult<Capabilities> {
    let policy = QuotePolicy::from_env();
    let mut classes: Vec<ClassCapability> = Vec::new();
    for (id, base) in policy.base_price_per_second.iter() {
        let tee = id.ends_with("-tee");
        classes.push(ClassCapability {
            id: id.clone(),
            tee,
            unit: "second",
            price_per_second: base.to_string(),
            idle_count: state.allocator.idle_count(id, false)
                + state.allocator.idle_count(id, true),
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
    let policy = QuotePolicy::from_env();
    let utilization_bps = class_idle_utilization(&state, &req.gpu_class);
    let inputs = QuoteInputs {
        gpu_class: req.gpu_class.clone(),
        duration_seconds: req.duration_seconds,
        confidentiality: req.confidentiality,
        utilization_bps,
    };
    let quote = policy
        .quote(&inputs)
        .map_err(|e: QuoteValidationError| err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let region = req.region.clone().unwrap_or_else(|| "any".to_string());
    let intent = crate::jobs::intent_hash(
        1,
        &req.gpu_class,
        req.duration_seconds,
        req.confidentiality,
        &region,
    );
    Ok(AxumJson(QuoteResponse {
        schema_version: crate::SCHEMA_VERSION,
        gpu_class: quote.gpu_class,
        price_per_second: quote.price_per_second.to_string(),
        duration_seconds: quote.duration_seconds,
        confidentiality: quote.confidentiality,
        escrow_total: quote.escrow_total.to_string(),
        utilization_bps,
        intent_hash: format!("0x{}", hex::encode(intent)),
        valid_until: crate::allocator::unix_now() + 60,
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
        device_id: alloc.device_id,
        lessee: alloc.lessee,
        expires_at: alloc.expires_at,
        endpoint: alloc.endpoint_v1,
    }))
}

async fn session_challenge(
    State(state): State<ApiState>,
    AxumJson(req): AxumJson<ChallengeRequest>,
) -> ApiResult<ChallengeResponse> {
    let id = parse_lease_id(&req.lease_id)?;
    let alloc = state
        .allocator
        .allocation(id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown or settled lease"))?;
    if alloc.lessee != req.lessee {
        return Err(err(StatusCode::FORBIDDEN, "not the lease's lessee"));
    }
    let ch = state.credentials.issue_challenge(id, &req.lessee);
    Ok(AxumJson(ChallengeResponse {
        nonce: format!("0x{}", hex::encode(ch.nonce)),
        message: ch.message(),
        expires_at: ch.expires_at,
    }))
}

async fn session_verify(
    State(state): State<ApiState>,
    AxumJson(req): AxumJson<VerifyRequest>,
) -> ApiResult<VerifyResponse> {
    let nonce = parse_lease_id(&req.nonce)?; // same 32-byte shape
    // Credential lifetime == the lease's session expiry: the credential dies
    // with the escrow (overstay impossible, operator side — SPEC I2).
    let alloc_expiry = match state.credentials.challenge_lease(nonce) {
        Some(lease_id) => state
            .allocator
            .allocation(lease_id)
            .map(|a| a.expires_at)
            .unwrap_or_else(crate::allocator::unix_now),
        None => crate::allocator::unix_now(),
    };
    let cred: ScopedCredential = state
        .credentials
        .verify_and_issue(nonce, &req.signature, alloc_expiry)
        .map_err(|e| err(StatusCode::UNAUTHORIZED, e.to_string()))?;
    Ok(AxumJson(VerifyResponse {
        token: cred.token,
        lease_id: format!("0x{}", hex::encode(cred.lease_id)),
        scopes: cred.scopes,
        expires_at: cred.expires_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::GpuDevice;
    use crate::credentials::eip191_personal_message_digest;
    use axum::body::Body;
    use axum::http::Request;
    use k256::ecdsa::{RecoveryId, SigningKey, VerifyingKey};
    use tiny_keccak::Hasher as _;
    use tower::ServiceExt; // oneshot

    pub(crate) fn tcp_test_state() -> ApiState {
        test_state()
    }

    fn test_state() -> ApiState {
        let allocator = GpuAllocator::new(vec![
            GpuDevice {
                id: "gpu-0".into(),
                gpu_class: "h100".into(),
                tee: false,
                cuda_ordinal: 0,
            },
            GpuDevice {
                id: "gpu-1".into(),
                gpu_class: "h100-tee".into(),
                tee: true,
                cuda_ordinal: 1,
            },
        ]);
        ApiState {
            allocator: Arc::new(allocator),
            credentials: Arc::new(CredentialSessions::new()),
        }
    }

    async fn get_json(state: &ApiState, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = operator_api_router_with(state.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn post_json(
        state: &ApiState,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = operator_api_router_with(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn address_of(key: &SigningKey) -> String {
        let point = VerifyingKey::from(key).to_encoded_point(false);
        let mut k = tiny_keccak::Keccak::v256();
        k.update(&point.as_bytes()[1..]);
        let mut hash = [0u8; 32];
        k.finalize(&mut hash);
        format!("0x{}", hex::encode(&hash[12..]))
    }

    fn sign_personal(key: &SigningKey, message: &str) -> String {
        let digest = eip191_personal_message_digest(message);
        let (sig, rec) = key.sign_prehash_recoverable(&digest).unwrap();
        let mut rsv = sig.to_bytes().to_vec();
        rsv.push(u8::from(rec.is_y_odd()) + 27);
        hex::encode(rsv)
    }

    #[tokio::test]
    async fn capabilities_lists_classes_and_idle_counts() {
        let state = test_state();
        let (status, body) = get_json(&state, "/api/capabilities").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["schemaVersion"], 1);
        assert_eq!(body["compute"]["kind"], "gpu");
        let classes = body["compute"]["classes"].as_array().unwrap();
        let h100 = classes.iter().find(|c| c["id"] == "h100").unwrap();
        assert_eq!(h100["idleCount"], 1);
        assert_eq!(h100["unit"], "second");
    }

    #[tokio::test]
    async fn quote_matches_policy_math_and_binds_intent() {
        let state = test_state();
        let (status, body) = post_json(
            &state,
            "/api/quote",
            serde_json::json!({ "gpuClass": "h100", "durationSeconds": 3600 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Policy default: h100 base 3e14 wei/s × 8000/10000 (fully idle => 8000 bps)
        let expected = 300_000_000_000_000u128 * 8_000 / 10_000;
        assert_eq!(
            body["pricePerSecond"].as_str().unwrap(),
            expected.to_string()
        );
        assert_eq!(
            body["escrowTotal"].as_str().unwrap(),
            (expected * 3600).to_string()
        );
        // Intent hash binds the exact request (region defaults to "any").
        let want = crate::jobs::intent_hash(1, "h100", 3600, 0, "any");
        assert_eq!(
            body["intentHash"].as_str().unwrap(),
            format!("0x{}", hex::encode(want))
        );
    }

    #[tokio::test]
    async fn quote_unknown_class_fails_closed() {
        let state = test_state();
        let (status, body) = post_json(
            &state,
            "/api/quote",
            serde_json::json!({ "gpuClass": "v100", "durationSeconds": 3600 }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["error"].as_str().is_some_and(|e| e.contains("v100")),
            "body: {body}"
        );
    }

    #[tokio::test]
    async fn lease_status_is_public_and_secret_free() {
        let state = test_state();
        let lease = [9u8; 32];
        state
            .allocator
            .acquire("h100", false, lease, "0xabc", 60)
            .unwrap();
        let (status, body) = get_json(&state, &format!("/api/leases/{}", hex::encode(lease))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deviceId"], "gpu-0");
        assert_eq!(body["schemaVersion"], 1);
        let s = body.to_string();
        assert!(!s.contains("token"), "no credential material in status");
        assert!(!s.contains("secret"));
        // Unknown lease => 404.
        let (status, _) = get_json(
            &state,
            "/api/leases/00000000000000000000000000000000000000000000000000000000000000ff",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn full_session_flow_over_http() {
        let state = test_state();
        let key = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let lessee = address_of(&key);
        let lease = [5u8; 32];
        state
            .allocator
            .acquire("h100", false, lease, &lessee, 600)
            .unwrap();

        // Challenge is lessee-bound.
        let (status, ch) = post_json(
            &state,
            "/api/session/challenge",
            serde_json::json!({ "leaseId": hex::encode(lease), "lessee": lessee }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let message = ch["message"].as_str().unwrap().to_string();
        let nonce = ch["nonce"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x")
            .to_string();

        // Wrong signer fails.
        let attacker = SigningKey::from_slice(&[8u8; 32]).unwrap();
        let (status, _) = post_json(
            &state,
            "/api/session/verify",
            serde_json::json!({ "nonce": nonce, "signature": sign_personal(&attacker, &message) }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Right signer gets a scoped credential.
        let (status, cred) = post_json(
            &state,
            "/api/session/verify",
            serde_json::json!({ "nonce": nonce, "signature": sign_personal(&key, &message) }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "verify failed: {cred}");
        assert!(cred["token"].as_str().unwrap().starts_with("glv1_"));
        assert_eq!(
            cred["leaseId"].as_str().unwrap(),
            format!("0x{}", hex::encode(lease))
        );
        assert!(
            cred["scopes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s == "gpu:attach")
        );

        // Nonce is single-use.
        let (status, _) = post_json(
            &state,
            "/api/session/verify",
            serde_json::json!({ "nonce": nonce, "signature": sign_personal(&key, &message) }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn challenge_rejects_non_lessee() {
        let state = test_state();
        let lease = [6u8; 32];
        state
            .allocator
            .acquire("h100", false, lease, "0xowner", 60)
            .unwrap();
        let (status, _) = post_json(
            &state,
            "/api/session/challenge",
            serde_json::json!({ "leaseId": hex::encode(lease), "lessee": "0xintruder" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // Keep the unused helper referenced (compile guard).
    #[allow(dead_code)]
    fn _unused(_r: RecoveryId) {}
}

#[cfg(test)]
mod globals_tests {
    use super::*;

    /// The globals-backed router (product path) must serve capabilities in a
    /// fresh process — this is the exact path that crashed in the E2E binary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_router_serves_capabilities_over_tcp() {
        let state = super::tests::tcp_test_state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, operator_api_router_with(state))
                .await
                .ok();
        });
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /api/capabilities HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("gpu"), "resp: {text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn globals_router_serves_capabilities() {
        // SAFETY: single-threaded test init before any tasks poll.
        unsafe {
            std::env::set_var(
                "GPU_INVENTORY_JSON",
                r#"[{"id":"g0","gpu_class":"h100","tee":false,"cuda_ordinal":0}]"#,
            )
        };
        // Init the globals BEFORE spawning the serve task (bisection knob).
        let a = crate::allocator();
        let c = crate::credentials();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _ = (a, c);
        tokio::spawn(async move {
            axum::serve(listener, operator_api_router()).await.ok();
        });

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /api/capabilities HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.contains("\"kind\":\"gpu\"") || text.contains("kind"),
            "resp: {text}"
        );
    }
}

#[cfg(test)]
mod globals_min {
    #[test]
    fn allocator_global_direct() {
        unsafe {
            std::env::set_var(
                "GPU_INVENTORY_JSON",
                r#"[{"id":"g0","gpu_class":"h100","tee":false,"cuda_ordinal":0}]"#,
            )
        };
        let a = crate::allocator();
        assert_eq!(a.idle_count("h100", false), 1);
        let a2 = crate::allocator();
        assert_eq!(a2.inventory().len(), 1);
        assert_eq!(a.idle_count("h100", false), 1, "first handle still valid");
    }
}
