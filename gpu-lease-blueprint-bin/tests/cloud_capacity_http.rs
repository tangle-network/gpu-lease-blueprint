#![cfg(all(feature = "cloud", unix))]
//! Actual operator binary -> public SDK -> provider HTTP, with native create/recovery/retire.
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use blueprint_sdk::{core::gpu, remote::ResourceSpec};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::Notify,
};

const KEY: &str = "synthetic-provider-credential";
const PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMo32AY0vFK7g5FBIBcyPdxaxSEM5rEc0kEzMVveA9b+";
#[derive(Default)]
struct Native {
    pods: BTreeMap<String, Value>,
    posts: Vec<Value>,
    deletes: usize,
    stock: bool,
    hold_post: bool,
    deny_delete: bool,
    list_error: bool,
    lambda_error: bool,
    hide_connections: bool,
    hold_lists: bool,
    first_list_primary: Option<bool>,
}
#[derive(Clone)]
struct Peer {
    native: Arc<Mutex<Native>>,
    created: Arc<Notify>,
    release: Arc<Notify>,
    listed: Arc<Notify>,
    release_list: Arc<Notify>,
}
async fn http(
    State(peer): State<Peer>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let other_account = token == "Bearer different-account-credential";
    if token != format!("Bearer {KEY}")
        && token != "Bearer rotated-same-account-credential"
        && !other_account
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut hold = false;
    let mut hold_list = false;
    let response = {
        let mut state = peer.native.lock().unwrap();
        match (method.as_str(), uri.path()) {
            ("GET", "/lambda/instances") if state.lambda_error => {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
            ("GET", "/lambda/instances") => Json(json!({"data":[]})).into_response(),
            ("GET", "/lambda/instance-types") => Json(json!({"data":{}})).into_response(),
            ("GET", "/catalog/gpus") => Json(json!({"gpus":if state.stock {vec![json!({
                "id":"NVIDIA B200","memory":192,"secure":true,"community":false,
                "maxCount":{"secure":8,"community":0},"price":{"secure":1.0,"community":0},
                "availability":"HIGH","dataCenters":[{"id":"US-TEST-1","availability":"HIGH"}]
            })]}else{vec![]}}))
            .into_response(),
            ("POST", "/pods") => {
                let request: Value = serde_json::from_slice(&body).unwrap();
                let id = format!("capacity{}", state.posts.len() + 1);
                let mut pod = json!({"id":id,"name":request["name"],"image":request["image"],
                    "gpu":{"id":request["gpu"]["id"],"count":request["gpu"]["count"],"vcpuCount":64,"memory":512},
                    "env":request["env"],"ports":request["ports"],"mounts":request["mounts"],"cmd":request["cmd"],
                    "status":"RUNNING","dataCenterId":"US-TEST-1","cost":request["gpu"]["count"],
                    "ssh":{"direct":{"host":"192.0.2.10","port":22334,"username":"root"}},
                    "runtime":{"ports":[{"private":9100,"public":null,"type":"http","ip":null}]}});
                if state.hide_connections {
                    pod["ssh"] = json!({});
                    pod["runtime"]["ports"] = json!([]);
                    pod["ports"] = json!([]);
                }
                state.pods.insert(id, pod.clone());
                state.posts.push(request);
                state.stock = false;
                hold = state.hold_post;
                (StatusCode::CREATED, Json(pod)).into_response()
            }
            ("GET", "/pods") if state.list_error => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            ("GET", "/pods") => {
                hold_list = state.hold_lists;
                if state.first_list_primary.is_none() {
                    state.first_list_primary = Some(token == format!("Bearer {KEY}"));
                }
                Json(json!({"pods":if other_account {vec![]}else{state.pods.values().cloned().collect::<Vec<_>>()}})).into_response()
            }
            ("GET", path) => {
                if other_account {
                    StatusCode::NOT_FOUND.into_response()
                } else {
                    state
                        .pods
                        .get(path.trim_start_matches("/pods/"))
                        .map(|pod| Json(pod.clone()).into_response())
                        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
                }
            }
            ("DELETE", _) if state.deny_delete => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            ("DELETE", path) => {
                state.deletes += 1;
                state.pods.remove(path.trim_start_matches("/pods/"));
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    };
    if hold {
        peer.created.notify_one();
        peer.release.notified().await;
    }
    if hold_list {
        peer.listed.notify_one();
        peer.release_list.notified().await;
    }
    response
}
struct Server {
    peer: Peer,
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start() -> Self {
        let peer = Peer {
            native: Arc::new(Mutex::new(Native::default())),
            created: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            listed: Arc::new(Notify::new()),
            release_list: Arc::new(Notify::new()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = axum::Router::new().fallback(http).with_state(peer.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { peer, url, task }
    }
    fn config(&self, root: &Path, name: &str, seconds: u64) -> Value {
        let directory = root.join(name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut spec = serde_json::to_value(ResourceSpec::basic()).unwrap();
        spec["ownership"] = json!({"namespace":"consumer-cell","lease_id":name});
        spec["gpu_class"] = json!("nvidia-b200");
        spec["gpu_count"] = json!(8);
        spec["gpu_vram_gb"] = json!(gpu::find("nvidia-b200").unwrap().vram_gb);
        spec["region"] = json!("US-TEST-1");
        spec["lease_duration_secs"] = json!(seconds);
        spec["ssh_public_key"] = json!(PUBLIC_KEY);
        spec["max_hourly_cost_usd"] = json!(20.0);
        spec["image"] = json!(format!(
            "example.invalid/operator@sha256:{}",
            "1".repeat(64)
        ));
        spec["command"] = json!(["/workload", "--serve"]);
        spec["ports"] = json!([9100]);
        spec["endpoint_port"] = json!(9100);
        spec["storage_gb"] = json!(200.0);
        spec["env"] = json!({"WORKLOAD_TAG":"consumer-boundary"});
        self.peer.native.lock().unwrap().stock = true;
        json!({"credentials":[{"provider":"runpod","api_key":KEY,"endpoint":self.url}],"spec":spec,"state_dir":directory})
    }
}
fn spawn() -> Child {
    Command::new(env!("CARGO_BIN_EXE_gpu-lease-blueprint"))
        .env_remove("GPU_INVENTORY_JSON")
        .env("GPU_CLOUD_CAPACITY", "1")
        .env("RUST_LOG", "off")
        .env("TOKIO_WORKER_THREADS", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}
async fn input(child: &mut Child, config: &Value) {
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(config).unwrap())
        .await
        .unwrap();
}
async fn launch(config: &Value) -> Child {
    let mut child = spawn();
    input(&mut child, config).await;
    child
}
async fn receipt(child: &mut Child) -> Value {
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(20),
        BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        !line.is_empty(),
        "cloud caller did not produce a deployment receipt"
    );
    assert!(!line.contains(KEY));
    serde_json::from_str(&line).unwrap()
}
async fn exited(child: &mut Child, success: bool) -> String {
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .as_mut()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    assert!(!stderr.contains(KEY));
    assert_eq!(status.success(), success, "{stderr}");
    stderr
}
async fn terminate(child: &mut Child, success: bool) {
    signal(child, "-TERM").await;
    exited(child, success).await;
}
async fn signal(child: &Child, kind: &str) {
    assert!(
        Command::new("kill")
            .args([kind, &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
}
fn phase(config: &Value) -> Value {
    serde_json::from_slice::<Value>(
        &std::fs::read(Path::new(config["state_dir"].as_str().unwrap()).join("capacity.json"))
            .unwrap(),
    )
    .unwrap()["phase"]
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_binary_propagates_eight_b200_and_recovers_without_new_stock() {
    let server = Server::start().await;
    let temp = tempfile::tempdir().unwrap();
    let config = server.config(temp.path(), "crash", 3600);
    server.peer.native.lock().unwrap().hold_post = true;
    let mut first = launch(&config).await;
    tokio::time::timeout(Duration::from_secs(20), server.peer.created.notified())
        .await
        .unwrap();
    assert_eq!(phase(&config), json!("Attempted"));
    let original = server
        .peer
        .native
        .lock()
        .unwrap()
        .pods
        .values()
        .next()
        .unwrap()["env"]["BPG_EXPIRES_AT"]
        .clone();
    first.start_kill().unwrap();
    first.wait().await.unwrap();
    server.peer.release.notify_one();
    server.peer.native.lock().unwrap().hold_post = false;
    // Treat SDK authority as opaque durable state; native tags cannot repair it.
    let authority = Path::new(config["state_dir"].as_str().unwrap())
        .join("blueprint/lease-authorizations.json");
    let original_authority = std::fs::read(&authority).unwrap();
    assert!(!String::from_utf8_lossy(&original_authority).contains(KEY));
    for corrupt in [true, false] {
        if corrupt {
            std::fs::write(&authority, "invalid SDK authority").unwrap();
        } else {
            std::fs::remove_file(&authority).unwrap();
        }
        let mut refused = launch(&config).await;
        assert!(
            exited(&mut refused, false)
                .await
                .contains("cloud authorization context")
        );
        assert_eq!(phase(&config), json!("Attempted"));
        assert_eq!(server.peer.native.lock().unwrap().posts.len(), 1);
    }
    std::fs::write(&authority, original_authority).unwrap();
    std::fs::set_permissions(&authority, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut restarted = launch(&config).await;
    let live = receipt(&mut restarted).await;
    assert_eq!(live["gpu_count"], 8);
    assert_eq!(live["gpu_class"], "nvidia-b200");
    assert_eq!(
        live["expires_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap(),
        original
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()
    );
    {
        let mut state = server.peer.native.lock().unwrap();
        assert_eq!(state.posts.len(), 1);
        let post = &state.posts[0];
        assert_eq!(
            post["gpu"]["id"],
            gpu::find("nvidia-b200").unwrap().sku("runpod", 8).unwrap()
        );
        assert_eq!(post["gpu"]["count"], 8);
        assert_eq!(post["disk"], 200);
        assert_eq!(post["image"], config["spec"]["image"]);
        assert_eq!(post["cmd"], config["spec"]["command"]);
        assert_eq!(post["ports"], json!(["22/tcp", "9100/http"]));
        assert_eq!(post["env"]["PUBLIC_KEY"], PUBLIC_KEY);
        assert_eq!(post["env"]["WORKLOAD_TAG"], "consumer-boundary");
        assert!(!post.to_string().contains(KEY));
        assert!(post["env"].get("GPU_CLOUD_CAPACITY").is_none());
        let pod = state.pods.values().next().unwrap().clone();
        for (id, namespace, lease) in [
            ("foreign", "other-cell", "expired"),
            ("unrelated", "consumer-cell", "live"),
        ] {
            let mut other = pod.clone();
            // Legacy controls exercise namespace isolation, not modern recovery authorization.
            other["env"]
                .as_object_mut()
                .unwrap()
                .remove("BPG_RECOVERY_TOKEN");
            other["id"] = json!(id);
            other["env"]["BPG_NAMESPACE"] = json!(namespace);
            other["env"]["BPG_LEASE_ID"] = json!(lease);
            if id == "foreign" {
                other["env"]["BPG_CREATED_AT"] = json!("2020-01-01T00:00:00+00:00");
                other["env"]["BPG_EXPIRES_AT"] = json!("2020-01-02T00:00:00+00:00");
            }
            state.pods.insert(id.into(), other);
        }
    }
    terminate(&mut restarted, true).await;
    assert_eq!(phase(&config), json!("Retired"));
    let path = Path::new(config["state_dir"].as_str().unwrap()).join("capacity.json");
    let saved = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    server.peer.native.lock().unwrap().stock = true;
    let mut missing = launch(&config).await;
    assert!(
        exited(&mut missing, false)
            .await
            .contains("cloud recovery state is missing or unreadable")
    );
    std::fs::write(&path, saved).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut changed = config.clone();
    changed["spec"]["gpu_count"] = json!(4);
    let mut different = launch(&changed).await;
    assert!(
        exited(&mut different, false)
            .await
            .contains("cloud recovery request changed")
    );
    let mut again = launch(&config).await;
    exited(&mut again, true).await;
    let state = server.peer.native.lock().unwrap();
    assert_eq!(state.posts.len(), 1);
    assert_eq!(state.deletes, 1);
    assert_eq!(state.pods.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_and_failed_retirement_stay_durable_without_rerenting() {
    let server = Server::start().await;
    let temp = tempfile::tempdir().unwrap();
    let config = server.config(temp.path(), "deadline", 2);
    let mut child = launch(&config).await;
    receipt(&mut child).await;
    exited(&mut child, true).await;
    assert_eq!(phase(&config), json!("Retired"));
    let config = server.config(temp.path(), "refused", 3600);
    let mut child = launch(&config).await;
    receipt(&mut child).await;
    server.peer.native.lock().unwrap().deny_delete = true;
    terminate(&mut child, false).await;
    assert_eq!(phase(&config), json!("Retiring"));
    let count = server.peer.native.lock().unwrap().posts.len();
    let mut different = config.clone();
    different["credentials"][0]["api_key"] = json!("different-account-credential");
    let mut wrong = launch(&different).await;
    exited(&mut wrong, false).await;
    assert_eq!(phase(&config), json!("Retiring"));
    let mut missing = config.clone();
    missing["credentials"][0]["provider"] = json!("vast-ai");
    let mut child = launch(&missing).await;
    assert!(
        exited(&mut child, false)
            .await
            .contains("original cloud account is unavailable")
    );
    server.peer.native.lock().unwrap().list_error = true;
    let mut child = launch(&config).await;
    exited(&mut child, false).await;
    assert_eq!(phase(&config), json!("Retiring"));
    {
        let mut state = server.peer.native.lock().unwrap();
        state.list_error = false;
        state.deny_delete = false;
    }
    let mut rotated = config.clone();
    rotated["credentials"][0]["api_key"] = json!("rotated-same-account-credential");
    let mut child = launch(&rotated).await;
    exited(&mut child, false).await;
    assert_eq!(phase(&config), json!("Retiring"));
    assert_eq!(server.peer.native.lock().unwrap().posts.len(), count);
    let mut child = launch(&config).await;
    exited(&mut child, true).await;
    assert_eq!(phase(&config), json!("Retired"));
    assert_eq!(server.peer.native.lock().unwrap().posts.len(), count);
    let config = server.config(temp.path(), "invalid", 3600);
    let before = server.peer.native.lock().unwrap().posts.len();
    let mut invalid = config.clone();
    invalid["spec"]["env"]["LEAK"] = json!(KEY);
    let mut child = launch(&invalid).await;
    exited(&mut child, false).await;
    let state = Path::new(config["state_dir"].as_str().unwrap()).join("capacity.json");
    std::fs::write(&state, "not valid recovery data").unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = launch(&config).await;
    assert!(
        exited(&mut child, false)
            .await
            .contains("invalid cloud recovery state")
    );
    assert_eq!(server.peer.native.lock().unwrap().posts.len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_lock_serializes_different_identities_and_interrupt_is_armed_before_create() {
    let server = Server::start().await;
    let temp = tempfile::tempdir().unwrap();
    let first_config = server.config(temp.path(), "store-owner", 3600);
    let mut competitor = first_config.clone();
    competitor["spec"]["ownership"]["lease_id"] = json!("competitor");
    server.peer.native.lock().unwrap().hold_lists = true;
    competitor["credentials"][0]["api_key"] = json!("rotated-same-account-credential");
    let mut first = spawn();
    let mut second = spawn();
    tokio::join!(
        input(&mut first, &first_config),
        input(&mut second, &competitor)
    );
    tokio::time::timeout(Duration::from_secs(20), server.peer.listed.notified())
        .await
        .unwrap();
    let crossed = tokio::time::timeout(Duration::from_secs(2), server.peer.listed.notified())
        .await
        .is_ok();
    server.peer.native.lock().unwrap().hold_lists = false;
    server.peer.release_list.notify_waiters();
    assert!(
        !crossed,
        "two fresh identities entered provider reconciliation under the same store"
    );
    let primary = server
        .peer
        .native
        .lock()
        .unwrap()
        .first_list_primary
        .unwrap();
    let (winner, loser) = if primary {
        (&mut first, &mut second)
    } else {
        (&mut second, &mut first)
    };
    receipt(winner).await;
    terminate(winner, true).await;
    assert!(
        exited(loser, false)
            .await
            .contains("cloud recovery request changed")
    );
    assert_eq!(server.peer.native.lock().unwrap().posts.len(), 1);
    let path = Path::new(first_config["state_dir"].as_str().unwrap()).join("capacity.json");
    let record: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        record["identity"]["lease_id"],
        if primary { "store-owner" } else { "competitor" }
    );

    let config = server.config(temp.path(), "interrupt", 3600);
    server.peer.native.lock().unwrap().hold_post = true;
    let mut child = launch(&config).await;
    tokio::time::timeout(Duration::from_secs(20), server.peer.created.notified())
        .await
        .unwrap();
    signal(&child, "-INT").await;
    server.peer.release.notify_one();
    server.peer.native.lock().unwrap().hold_post = false;
    exited(&mut child, true).await;
    assert_eq!(phase(&config), json!("Retired"));
    assert!(server.peer.native.lock().unwrap().pods.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_create_journal_failure_and_one_failed_account_do_not_suppress_cleanup() {
    let server = Server::start().await;
    let temp = tempfile::tempdir().unwrap();
    let config = server.config(temp.path(), "write-failure", 3600);
    server.peer.native.lock().unwrap().hold_post = true;
    let mut child = launch(&config).await;
    tokio::time::timeout(Duration::from_secs(20), server.peer.created.notified())
        .await
        .unwrap();
    let pending = Path::new(config["state_dir"].as_str().unwrap()).join("capacity.pending");
    std::fs::create_dir(&pending).unwrap();
    server.peer.release.notify_one();
    server.peer.native.lock().unwrap().hold_post = false;
    assert!(
        exited(&mut child, false)
            .await
            .contains("cloud recovery state write failed")
    );
    assert!(
        server.peer.native.lock().unwrap().pods.is_empty(),
        "bookkeeping failure suppressed native cleanup"
    );
    std::fs::remove_dir(pending).unwrap();
    server.peer.native.lock().unwrap().stock = true;
    let mut retry = launch(&config).await;
    exited(&mut retry, false).await;
    assert_eq!(phase(&config), json!("Retired"));
    assert_eq!(server.peer.native.lock().unwrap().posts.len(), 1);

    let mut config = server.config(temp.path(), "accounts", 3600);
    config["credentials"].as_array_mut().unwrap().insert(
        0,
        json!({"provider":"lambda-labs","api_key":KEY,"endpoint":format!("{}/lambda",server.url)}),
    );
    server.peer.native.lock().unwrap().hold_post = true;
    let mut child = launch(&config).await;
    tokio::time::timeout(Duration::from_secs(20), server.peer.created.notified())
        .await
        .unwrap();
    child.start_kill().unwrap();
    child.wait().await.unwrap();
    server.peer.release.notify_one();
    {
        let mut native = server.peer.native.lock().unwrap();
        native.hold_post = false;
        native.lambda_error = true;
    }
    let mut retry = launch(&config).await;
    exited(&mut retry, false).await;
    assert!(
        server.peer.native.lock().unwrap().pods.is_empty(),
        "earlier failed account starved accepted resource cleanup"
    );
    assert_eq!(phase(&config), json!("Retiring"));
    let creates = server.peer.native.lock().unwrap().posts.len();
    server.peer.native.lock().unwrap().lambda_error = false;
    let mut retry = launch(&config).await;
    exited(&mut retry, true).await;
    assert_eq!(phase(&config), json!("Retired"));
    assert_eq!(server.peer.native.lock().unwrap().posts.len(), creates);

    let config = server.config(temp.path(), "expired-before-ready", 1);
    server.peer.native.lock().unwrap().hold_post = true;
    let mut child = launch(&config).await;
    tokio::time::timeout(Duration::from_secs(20), server.peer.created.notified())
        .await
        .unwrap();
    let end = server
        .peer
        .native
        .lock()
        .unwrap()
        .pods
        .values()
        .next()
        .unwrap()["env"]["BPG_EXPIRES_AT"]
        .as_str()
        .unwrap()
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap();
    tokio::time::sleep(
        (end - chrono::Utc::now()).to_std().unwrap_or_default() + Duration::from_millis(20),
    )
    .await;
    server.peer.release.notify_one();
    server.peer.native.lock().unwrap().hold_post = false;
    exited(&mut child, false).await;
    assert!(server.peer.native.lock().unwrap().pods.is_empty());
    assert_eq!(phase(&config), json!("Retired"));

    let config = server.config(temp.path(), "no-connection", 2);
    server.peer.native.lock().unwrap().hide_connections = true;
    let mut child = launch(&config).await;
    assert!(
        exited(&mut child, false)
            .await
            .contains("cloud connection readiness failed")
    );
    assert!(server.peer.native.lock().unwrap().pods.is_empty());
    assert_eq!(phase(&config), json!("Retired"));
}
