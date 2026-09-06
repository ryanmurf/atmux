//! End-to-end federation tests against a real HTTP node.
//!
//! A small axum server stands in for a remote `atmux web` process, so the
//! coordinator's transport, credential handling, SSE decoding, and routing are
//! exercised over a real socket rather than mocked out.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use atmux::{
    attachment::{EncodedImage, ImageMessageRequest},
    config::{Config, MachineConfig},
    control::{
        CloneLaunchRepositoryRequest, ControlPlane, CreateLaunchDirectoryRequest, ErrorKind,
        LaunchRequest, error_kind,
    },
    machine::MachineKind,
    remote::RemoteMachine,
    tmux::{PaneSpecialKey, Tmux},
    workspace::{FileWriteRequest, FilesResponse, GitResponse},
};

use axum::http::Request as HttpRequest;
use axum::{
    Json, Router,
    body::Body,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Reports whether tmux is available, failing instead of skipping when CI has
/// declared that tmux must be present.
///
/// Silently skipping is how a whole federation suite disappears from CI without
/// anyone noticing, so `ATMUX_REQUIRE_TMUX` turns a missing tmux into a failure.
fn tmux_available(test: &str) -> bool {
    let isolated = std::env::var("ATMUX_TMUX_SOCKET_NAME")
        .ok()
        .filter(|socket| {
            (socket.starts_with("atmux-test-") || socket.starts_with("atmux-ci-"))
                && socket.len() <= 100
                && socket
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        });
    if isolated.is_none() {
        assert!(
            std::env::var_os("ATMUX_REQUIRE_TMUX").is_none(),
            "{test} requires an isolated ATMUX_TMUX_SOCKET_NAME beginning with atmux-test- or atmux-ci-"
        );
        eprintln!("SKIPPED {test}: no validated disposable tmux socket was configured");
        return false;
    }
    match Tmux::check() {
        Ok(()) => true,
        Err(error) => {
            assert!(
                std::env::var_os("ATMUX_REQUIRE_TMUX").is_none(),
                "{test} requires tmux and ATMUX_REQUIRE_TMUX is set, but tmux is unusable: {error:#}"
            );
            eprintln!("SKIPPED {test}: tmux is unavailable: {error:#}");
            false
        }
    }
}

/// Everything the fake node observed, so tests can assert on the wire format.
#[derive(Debug, Default)]
struct Recorder {
    authorizations: Vec<Option<String>>,
    hosts: Vec<Option<String>>,
    paths: Vec<String>,
    bodies: Vec<String>,
    /// Whether this fixture represents a new owner which explicitly
    /// advertises and validates per-launch memory overrides.
    advertise_memory: bool,
    /// Latency injected into pane reads, so a test can hold several callers
    /// inside one fetch window at the same time.
    pane_latency: Duration,
    /// Simulates an owner discovering that a coordinator-cached pane
    /// generation was replaced after the command was forwarded.
    reject_generation_mutations: bool,
}

type Shared = Arc<Mutex<Recorder>>;

fn record(state: &Shared, headers: &HeaderMap, path: &str, body: &str) {
    record_locked(&mut state.lock().unwrap(), headers, path, body);
}

fn record_locked(recorder: &mut Recorder, headers: &HeaderMap, path: &str, body: &str) {
    recorder.authorizations.push(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    );
    recorder.hosts.push(
        headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    );
    recorder.paths.push(path.to_owned());
    recorder.bodies.push(body.to_owned());
}

fn session(pane: &str, name: &str, status: &str, hash: &str) -> Value {
    let instance = if pane == "%7" { 'a' } else { 'b' };
    json!({
        "id": format!("local~{pane}"),
        "instance_id": format!("pane-v1-{}", instance.to_string().repeat(64)),
        "machine": "local",
        "name": name,
        "pane_id": pane,
        "status": status,
        "agent": "claude",
        "attached": false,
        "activity": 1,
        "path": "/srv/models",
        "title": name,
        "command": "claude",
        "windows": 1,
        "window_index": 0,
        "pane_index": 0,
        "content_hash": hash,
    })
}

fn fixture_accepts_trainer_instance(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|value| {
        value.get("instance_id").and_then(Value::as_str)
            == Some(format!("pane-v1-{}", "a".repeat(64)).as_str())
    })
}

fn node_machines() -> Value {
    json!([{
        "id": "local",
        "label": "GPU box",
        "kind": "local",
        "online": true,
        "sessions": 1,
        "health": null,
        "last_seen_ms": null,
        "address": null,
    }])
}

async fn events(State(state): State<Shared>, headers: HeaderMap) -> Response {
    record(&state, &headers, "/api/v1/events", "");
    let snapshot = json!({
        "revision": 4,
        "sessions": [session("%7", "trainer", "working", "aaaa")],
        "health": null,
        "machines": node_machines(),
    });
    let patch = json!({
        "base_revision": 4,
        "revision": 5,
        "upsert": [session("%8", "evaluator", "waiting", "bbbb")],
        "remove": [],
        "health": null,
        "machines": node_machines(),
    });
    // Deliberately split across frames and interleaved with a keep-alive comment
    // so the decoder is exercised the way a real node drives it.
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from(format!(
            ": keep-alive\n\nevent: sessions.snapshot\nid: 4\ndata: {snapshot}\n"
        )));
        yield Ok(axum::body::Bytes::from("\n".to_owned()));
        tokio::time::sleep(Duration::from_millis(30)).await;
        yield Ok(axum::body::Bytes::from(format!(
            "event: sessions.patch\nid: 5\ndata: {patch}\n\n"
        )));
        // Hold the connection open like a live node instead of closing it.
        tokio::time::sleep(Duration::from_secs(30)).await;
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn pane(State(state): State<Shared>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let latency = {
        let mut recorder = state.lock().unwrap();
        record_locked(&mut recorder, &headers, &format!("/api/v1/panes/{id}"), "");
        recorder.pane_latency
    };
    if !latency.is_zero() {
        tokio::time::sleep(latency).await;
    }
    if id != "%7" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no agent pane matches {id}") })),
        )
            .into_response();
    }
    Json(json!({
        "revision": 4,
        "pane_id": "local~%7",
        "session": "trainer",
        "content_hash": "aaaa",
        "content": "epoch 1\nepoch 2\nepoch 3",
        "changed": true,
    }))
    .into_response()
}

async fn transcript(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    record(
        &state,
        &headers,
        &format!("/api/v1/panes/{id}/transcript"),
        "",
    );
    Json(json!({
        "available": true,
        "source": "claude",
        "content_hash": "0123456789abcdef",
        "changed": true,
        "truncated": false,
        "messages": [{
            "id": "assistant-1",
            "role": "assistant",
            "markdown": "Training is **healthy**.",
            "timestamp": "2026-08-07T00:00:00Z"
        }]
    }))
    .into_response()
}

async fn files(
    State(state): State<Shared>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    record(&state, &headers, &uri.to_string(), "");
    let path = query.get("path").cloned().unwrap_or_default();
    if id != "%7" {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(json!({
        "kind": "file",
        "pane_id": "%7",
        "path": path,
        "content": "fn federated() {}\n",
        "language": "rust",
        "size": 18,
        "truncated": false,
        "binary": false,
    }))
    .into_response()
}

async fn write_file(
    State(state): State<Shared>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: String,
) -> Response {
    record(&state, &headers, &uri.to_string(), &body);
    if id != "%7" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let request: FileWriteRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if request.expected_hash == "f".repeat(64) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "project file changed since it was opened" })),
        )
            .into_response();
    }
    Json(json!({
        "kind": "file",
        "pane_id": "%7",
        "path": request.path,
        "content": request.content,
        "language": "rust",
        "size": 21,
        "truncated": false,
        "binary": false,
        "content_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "line_count": 1,
    }))
    .into_response()
}

async fn git_view(
    State(state): State<Shared>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    record(&state, &headers, &uri.to_string(), "");
    if id != "%7" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(path) = query.get("path") {
        return Json(json!({
            "pane_id": "%7",
            "path": path,
            "diff": "@@ -1 +1 @@\n-old\n+new\n",
            "language": "diff",
            "truncated": false,
            "binary": false,
        }))
        .into_response();
    }
    Json(json!({
        "pane_id": "%7",
        "available": true,
        "branch": "main",
        "detached": false,
        "clean": false,
        "changes": [{
            "status": " M",
            "path": "src/federated.rs",
        }],
        "truncated": false,
    }))
    .into_response()
}

async fn messages(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let reject = {
        let mut recorder = state.lock().unwrap();
        record_locked(
            &mut recorder,
            &headers,
            &format!("/api/v1/panes/{id}/messages"),
            &body,
        );
        recorder.reject_generation_mutations
    };
    if reject && fixture_accepts_trainer_instance(&body) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "owner-only-generation-detail" })),
        )
            .into_response();
    }
    if reject {
        return StatusCode::BAD_REQUEST.into_response();
    }
    Json(json!({ "ok": true })).into_response()
}

async fn image_messages(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let reject = {
        let mut recorder = state.lock().unwrap();
        record_locked(
            &mut recorder,
            &headers,
            &format!("/api/v1/panes/{id}/image-messages"),
            &body,
        );
        recorder.reject_generation_mutations
    };
    if reject && fixture_accepts_trainer_instance(&body) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "owner-only-generation-detail" })),
        )
            .into_response();
    }
    if reject {
        return StatusCode::BAD_REQUEST.into_response();
    }
    Json(json!({ "ok": true })).into_response()
}

async fn input_keys(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let reject = {
        let mut recorder = state.lock().unwrap();
        record_locked(
            &mut recorder,
            &headers,
            &format!("/api/v1/panes/{id}/input-keys"),
            &body,
        );
        recorder.reject_generation_mutations
    };
    if reject && fixture_accepts_trainer_instance(&body) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "owner-only-generation-detail" })),
        )
            .into_response();
    }
    Json(json!({ "ok": true })).into_response()
}

async fn legacy_special_keys(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    record(
        &state,
        &headers,
        &format!("/api/v1/panes/{id}/special-keys"),
        &body,
    );
    Json(json!({ "ok": true })).into_response()
}

async fn interrupt(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    record(
        &state,
        &headers,
        &format!("/api/v1/panes/{id}/interrupt"),
        "",
    );
    Json(json!({ "ok": true })).into_response()
}

async fn resume(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    record(
        &state,
        &headers,
        &format!("/api/v1/panes/{id}/resume"),
        &body,
    );
    Json(json!({ "ok": true })).into_response()
}

async fn recovery_status(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    record(
        &state,
        &headers,
        &format!("/api/v1/machines/{id}/quick-resume"),
        "",
    );
    Json(json!({
        "machine": id,
        "available": true,
        "phase": "idle",
        "message": "Ready for fixture recovery"
    }))
    .into_response()
}

async fn start_recovery(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    record(
        &state,
        &headers,
        &format!("/api/v1/machines/{id}/quick-resume"),
        &body,
    );
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "machine": id,
            "available": true,
            "phase": "running",
            "started_at_ms": 1,
            "message": "Fixture recovery running"
        })),
    )
        .into_response()
}

async fn kill(State(state): State<Shared>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    record(&state, &headers, &format!("/api/v1/sessions/{id}"), "");
    Json(json!({ "ok": true })).into_response()
}

async fn launch(State(state): State<Shared>, headers: HeaderMap, body: String) -> Response {
    record(&state, &headers, "/api/v1/sessions", &body);
    (StatusCode::CREATED, Json(json!({ "ok": true }))).into_response()
}

async fn launch_with_memory(
    State(state): State<Shared>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !state.lock().unwrap().advertise_memory {
        return StatusCode::NOT_FOUND.into_response();
    }
    record(&state, &headers, "/api/v1/memory-launches/v1", &body);
    (StatusCode::CREATED, Json(json!({ "ok": true }))).into_response()
}

async fn launch_options(State(state): State<Shared>, headers: HeaderMap) -> Response {
    record(&state, &headers, "/api/v1/launch-options", "");
    let advertise_memory = state.lock().unwrap().advertise_memory;
    let mut payload = json!({
        "directories": ["/srv/models"],
        "profiles": [{ "id": "profile-0", "name": "Default", "harness": "claude" }],
        "machines": [],
    });
    if advertise_memory {
        payload["memory"] = json!({
            "supported": true,
            "default_bytes": 17_179_869_184_u64,
            "override_max_bytes": 25_769_803_776_u64,
            "presets_bytes": [8_589_934_592_u64, 17_179_869_184_u64, 25_769_803_776_u64],
            "note": "fixture capability"
        });
    }
    Json(payload).into_response()
}

async fn launch_directories(
    State(state): State<Shared>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    record(&state, &headers, &uri.to_string(), "");
    let current = query.get("path").cloned();
    Json(json!({
        "machine": "gpu-box",
        "current": current,
        "parent": null,
        "directories": [{ "path": "/srv/models/child", "name": "child" }],
        "truncated": false,
    }))
    .into_response()
}

async fn launch_directory_action(
    State(state): State<Shared>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: String,
) -> Response {
    record(&state, &headers, &uri.to_string(), &body);
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let parent = value
        .get("directory")
        .and_then(Value::as_str)
        .unwrap_or("/srv/models");
    let name = value
        .get("name")
        .or_else(|| value.get("destination"))
        .and_then(Value::as_str)
        .unwrap_or("cloned-repo");
    Json(json!({
        "directory": { "path": format!("{parent}/{name}"), "name": name },
        "listing": {
            "machine": "gpu-box",
            "current": parent,
            "parent": "/srv",
            "directories": [{ "path": format!("{parent}/{name}"), "name": name }],
            "truncated": false,
        },
    }))
    .into_response()
}

async fn start_node() -> (SocketAddr, Shared) {
    start_node_with_memory(false).await
}

async fn start_node_with_memory(advertise_memory: bool) -> (SocketAddr, Shared) {
    let recorder: Shared = Arc::new(Mutex::new(Recorder {
        advertise_memory,
        ..Recorder::default()
    }));
    let app = Router::new()
        .route("/api/v1/events", get(events))
        .route("/api/v1/launch-options", get(launch_options))
        .route("/api/v1/launch-directories", get(launch_directories))
        .route(
            "/api/v1/launch-directories/folders",
            post(launch_directory_action),
        )
        .route(
            "/api/v1/launch-directories/clone",
            post(launch_directory_action),
        )
        .route("/api/v1/panes/{id}", get(pane))
        .route("/api/v1/panes/{id}/transcript", get(transcript))
        .route("/api/v1/panes/{id}/files", get(files).put(write_file))
        .route("/api/v1/panes/{id}/git", get(git_view))
        .route("/api/v1/panes/{id}/messages", post(messages))
        .route("/api/v1/panes/{id}/image-messages", post(image_messages))
        .route("/api/v1/panes/{id}/special-keys", post(legacy_special_keys))
        .route("/api/v1/panes/{id}/input-keys", post(input_keys))
        .route("/api/v1/panes/{id}/interrupt", post(interrupt))
        .route("/api/v1/panes/{id}/resume", post(resume))
        .route(
            "/api/v1/machines/{id}/quick-resume",
            get(recovery_status).post(start_recovery),
        )
        .route("/api/v1/sessions", post(launch))
        .route("/api/v1/memory-launches/v1", post(launch_with_memory))
        .route("/api/v1/sessions/{id}", delete(kill))
        .with_state(Arc::clone(&recorder));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, recorder)
}

async fn start_legacy_input_node() -> (SocketAddr, Shared) {
    let recorder: Shared = Arc::new(Mutex::new(Recorder::default()));
    let app = Router::new()
        .route("/api/v1/events", get(events))
        // This intentionally models the prior owner: it would execute the
        // unbound Ctrl+B command here, but knows nothing about /input-keys.
        .route("/api/v1/panes/{id}/special-keys", post(legacy_special_keys))
        .with_state(Arc::clone(&recorder));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, recorder)
}

fn machine_config(address: SocketAddr, token_file: Option<&std::path::Path>) -> MachineConfig {
    MachineConfig {
        id: "gpu-box".to_owned(),
        label: Some("GPU box".to_owned()),
        url: format!("http://{address}"),
        token_env: None,
        token_file: token_file.map(std::path::Path::to_path_buf),
    }
}

fn token_file(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("atmux-fed-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    std::fs::write(&path, "  node-token\n").unwrap();
    path
}

#[tokio::test]
async fn transport_carries_credentials_and_decodes_a_live_stream() {
    let (address, recorder) = start_node().await;
    let token = token_file("transport.token");
    let machine = RemoteMachine::from_config(&machine_config(address, Some(&token))).unwrap();
    assert!(machine.is_authenticated());

    let options: atmux::control::LaunchOptions =
        machine.get_json("/api/v1/launch-options").await.unwrap();
    assert_eq!(options.directories, ["/srv/models"]);

    let mut stream = machine.open_events("/api/v1/events").await.unwrap();
    let snapshot = stream.next_event().await.unwrap();
    assert_eq!(snapshot.name, "sessions.snapshot");
    let decoded: Value = serde_json::from_str(&snapshot.data).unwrap();
    assert_eq!(decoded["revision"], 4);
    assert_eq!(decoded["sessions"][0]["pane_id"], "%7");
    let patch = stream.next_event().await.unwrap();
    assert_eq!(patch.name, "sessions.patch");
    assert_eq!(
        serde_json::from_str::<Value>(&patch.data).unwrap()["revision"],
        5
    );
    drop(stream);

    // Every request presented the credential and the configured authority.
    let seen = recorder.lock().unwrap();
    assert!(!seen.authorizations.is_empty());
    assert!(
        seen.authorizations
            .iter()
            .all(|value| value.as_deref() == Some("Bearer node-token")),
        "{:?}",
        seen.authorizations
    );
    assert!(
        seen.hosts
            .iter()
            .all(|value| value.as_deref() == Some(address.to_string().as_str())),
        "{:?}",
        seen.hosts
    );
}

#[tokio::test]
async fn an_unauthenticated_machine_sends_no_authorization_header() {
    let (address, recorder) = start_node().await;
    let machine = RemoteMachine::from_config(&machine_config(address, None)).unwrap();
    assert!(!machine.is_authenticated());
    let _: atmux::control::LaunchOptions =
        machine.get_json("/api/v1/launch-options").await.unwrap();
    assert_eq!(recorder.lock().unwrap().authorizations, [None]);
}

#[tokio::test(flavor = "multi_thread")]
async fn generation_bound_keys_fail_closed_against_a_legacy_owner() {
    if !tmux_available("generation_bound_keys_fail_closed_against_a_legacy_owner") {
        return;
    }
    let (address, recorder) = start_legacy_input_node().await;
    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    assert!(wait_for_session(&control, "gpu-box~%7").await);
    let error = control
        .send_special_key_for_instance(
            "gpu-box~%7",
            PaneSpecialKey::TmuxPrefixTwice,
            "gpu-box".to_owned(),
            format!("pane-v1-{}", "a".repeat(64)),
        )
        .await
        .unwrap_err();
    assert_eq!(error_kind(&error), ErrorKind::NotFound, "{error:#}");

    {
        let seen = recorder.lock().unwrap();
        assert!(
            !seen
                .paths
                .iter()
                .any(|path| path.ends_with("/special-keys")),
            "a new coordinator must never fall back to the legacy unbound route: {:#?}",
            seen.paths,
        );
        assert!(
            seen.bodies.iter().all(String::is_empty),
            "the legacy mutation handler must receive no key body: {:#?}",
            seen.bodies,
        );
    }

    control.tmux_prefix_twice("gpu-box~%7").await.unwrap();
    let seen = recorder.lock().unwrap();
    assert!(
        seen.paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/special-keys"),
        "only the explicit legacy control may use the old owner route",
    );
    assert!(seen.bodies.iter().any(|body| {
        serde_json::from_str::<Value>(body).ok() == Some(json!({ "action": "tmux_prefix_twice" }))
    }));
}

#[tokio::test]
async fn node_errors_surface_with_their_message_and_unreachable_nodes_fail_fast() {
    let (address, _recorder) = start_node().await;
    let machine = RemoteMachine::from_config(&machine_config(address, None)).unwrap();

    let error = machine
        .get_json::<Value>("/api/v1/panes/%259")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("404"), "{error}");
    assert!(error.contains("no agent pane matches %9"), "{error}");

    // A closed port must fail quickly rather than hang a coordinator request.
    let dead = RemoteMachine::from_config(&MachineConfig {
        id: "dead".to_owned(),
        label: None,
        url: "http://127.0.0.1:1".to_owned(),
        token_env: None,
        token_file: None,
    })
    .unwrap();
    let started = Instant::now();
    let error = dead
        .get_json::<Value>("/api/v1/sessions")
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "connect hung");
    assert!(format!("{error:#}").contains("dead"), "{error:#}");
}

/// Counts the connections a fake node has accepted and how many are still open.
#[derive(Debug, Default)]
struct Connections {
    accepted: AtomicUsize,
    open: AtomicUsize,
}

impl Connections {
    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }
}

/// A node that accepts connections and then never answers, so every request
/// against it can only end in a timeout.
async fn start_hung_node() -> (SocketAddr, Arc<Connections>) {
    let connections = Arc::new(Connections::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observer = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            observer.accepted.fetch_add(1, Ordering::SeqCst);
            observer.open.fetch_add(1, Ordering::SeqCst);
            let observer = Arc::clone(&observer);
            tokio::spawn(async move {
                // Read until the peer hangs up, never replying. A connection
                // whose client-side driver is still running stays counted as
                // open, which is exactly what must not accumulate.
                let mut scratch = [0_u8; 1024];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut stream, &mut scratch).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                observer.open.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (address, connections)
}

/// Waits for a condition that must become true promptly, so a failure is a real
/// regression rather than a slow machine.
async fn settles<F: Fn() -> bool>(condition: F, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    condition()
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_failed_stream_opens_leak_no_connections_or_tasks() {
    const ATTEMPTS: usize = 8;

    let (hung, connections) = start_hung_node().await;
    let (live, _recorder) = start_node().await;

    // Short timeouts keep this deterministic instead of costing 20s per case.
    let hung_machine = RemoteMachine::from_config(&MachineConfig {
        id: "hung".to_owned(),
        label: None,
        url: format!("http://{hung}"),
        token_env: None,
        token_file: None,
    })
    .unwrap()
    .with_timeouts(Duration::from_millis(500), Duration::from_millis(80));
    let rejecting = RemoteMachine::from_config(&machine_config(live, None))
        .unwrap()
        .with_timeouts(Duration::from_millis(500), Duration::from_millis(500));

    // One of each failure mode first, so the baseline includes any one-time
    // runtime task rather than counting it as a leak.
    assert!(hung_machine.open_events("/api/v1/events").await.is_err());
    assert!(rejecting.open_events("/api/v1/missing").await.is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let metrics = tokio::runtime::Handle::current().metrics();
    let baseline = metrics.num_alive_tasks();

    for attempt in 0..ATTEMPTS {
        let timed_out = hung_machine
            .open_events("/api/v1/events")
            .await
            .expect_err("a hung node must time out");
        assert!(
            format!("{timed_out:#}").contains("timed out"),
            "attempt {attempt}: {timed_out:#}"
        );
        let rejected = rejecting
            .open_events("/api/v1/missing")
            .await
            .expect_err("a rejected stream must fail");
        assert!(
            format!("{rejected:#}").contains("404"),
            "attempt {attempt}: {rejected:#}"
        );
    }
    // The node accepted one connection per attempt, and none of them may still
    // be open: a driver that outlived its failed open would hold its socket.
    assert_eq!(connections.accepted(), ATTEMPTS + 1);
    assert!(
        settles(|| connections.open() == 0, Duration::from_secs(2)).await,
        "{} connections were still open after {ATTEMPTS} failed opens",
        connections.open()
    );

    // Task count must return to where it started rather than growing with the
    // number of attempts, which is what a detached driver per open looks like.
    assert!(
        settles(
            || metrics.num_alive_tasks() <= baseline,
            Duration::from_secs(2)
        )
        .await,
        "failed stream opens leaked tasks: {baseline} before, {} after {ATTEMPTS} attempts",
        metrics.num_alive_tasks()
    );

    // A successful open still owns its driver, so dropping the subscription
    // tears the connection down too.
    let stream = hung_machine.open_events("/api/v1/events");
    let opened = tokio::time::timeout(Duration::from_millis(200), stream).await;
    assert!(opened.is_err() || opened.unwrap().is_err());
    assert!(settles(|| connections.open() == 0, Duration::from_secs(2)).await);
}

fn federated_config(address: SocketAddr) -> Config {
    Config {
        machines: vec![machine_config(address, None)],
        ..Config::default()
    }
}

async fn wait_for_session(control: &ControlPlane, id: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if control
            .overview()
            .sessions
            .iter()
            .any(|session| session.id == id)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

async fn wait_for_online(control: &ControlPlane, machine: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if control
            .machines()
            .iter()
            .any(|candidate| candidate.id == machine && candidate.online)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

async fn wait_for_memory_capability(control: &ControlPlane, machine: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if control
            .launch_options()
            .machines
            .iter()
            .find(|candidate| candidate.id == machine)
            .and_then(|candidate| candidate.memory.as_ref())
            .is_some_and(|memory| memory.supported)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_version_owner_never_receives_an_unadvertised_memory_override() {
    if !tmux_available("mixed_version_owner_never_receives_an_unadvertised_memory_override") {
        return;
    }
    let (address, recorder) = start_node().await;
    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    assert!(wait_for_online(&control, "gpu-box").await);
    let before = recorder
        .lock()
        .unwrap()
        .paths
        .iter()
        .filter(|path| path.as_str() == "/api/v1/sessions")
        .count();

    for (index, requested) in [0, u64::MAX, 8 * 1024 * 1024 * 1024]
        .into_iter()
        .enumerate()
    {
        let error = control
            .launch(LaunchRequest {
                name: format!("old-owner-memory-{index}"),
                directory: "/srv/models".to_owned(),
                profile_id: "profile-0".to_owned(),
                mode_id: None,
                machine: Some("gpu-box".to_owned()),
                resume_session_id: None,
                memory_max_bytes: Some(requested),
            })
            .await
            .unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::BadRequest);
        assert!(error.to_string().contains("has not advertised"));
    }
    let seen = recorder.lock().unwrap();
    assert_eq!(
        seen.paths
            .iter()
            .filter(|path| path.as_str() == "/api/v1/sessions")
            .count(),
        before,
        "an older owner must never receive or silently ignore an override"
    );
    assert!(
        seen.bodies
            .iter()
            .all(|body| !body.contains("old-owner-memory"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn capable_owner_receives_valid_memory_but_coordinator_rejects_advertised_bounds() {
    if !tmux_available(
        "capable_owner_receives_valid_memory_but_coordinator_rejects_advertised_bounds",
    ) {
        return;
    }
    let (address, recorder) = start_node_with_memory(true).await;
    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    assert!(wait_for_online(&control, "gpu-box").await);
    assert!(wait_for_memory_capability(&control, "gpu-box").await);

    let custom = 20 * 1024 * 1024 * 1024;
    control
        .launch(LaunchRequest {
            name: "new-owner-memory".to_owned(),
            directory: "/srv/models".to_owned(),
            profile_id: "profile-0".to_owned(),
            mode_id: None,
            machine: Some("gpu-box".to_owned()),
            resume_session_id: None,
            memory_max_bytes: Some(custom),
        })
        .await
        .unwrap();

    let successful_posts = recorder
        .lock()
        .unwrap()
        .paths
        .iter()
        .filter(|path| path.as_str() == "/api/v1/memory-launches/v1")
        .count();
    for (index, requested) in [0, u64::MAX, 25 * 1024 * 1024 * 1024]
        .into_iter()
        .enumerate()
    {
        let error = control
            .launch(LaunchRequest {
                name: format!("new-owner-invalid-{index}"),
                directory: "/srv/models".to_owned(),
                profile_id: "profile-0".to_owned(),
                mode_id: None,
                machine: Some("gpu-box".to_owned()),
                resume_session_id: None,
                memory_max_bytes: Some(requested),
            })
            .await
            .unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::BadRequest);
    }
    let seen = recorder.lock().unwrap();
    assert_eq!(
        seen.paths
            .iter()
            .filter(|path| path.as_str() == "/api/v1/memory-launches/v1")
            .count(),
        successful_posts
    );
    assert!(
        seen.paths
            .iter()
            .all(|path| path.as_str() != "/api/v1/sessions"),
        "an explicit limit must never use the legacy launch route"
    );
    let forwarded = seen
        .bodies
        .iter()
        .find(|body| body.contains("new-owner-memory"))
        .map(|body| serde_json::from_str::<Value>(body).unwrap())
        .unwrap();
    assert_eq!(forwarded["memory_max_bytes"], custom);
    assert_eq!(forwarded["machine"], Value::Null);
    assert!(
        seen.bodies
            .iter()
            .all(|body| !body.contains("new-owner-invalid"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_capability_cannot_launch_after_an_owner_downgrade() {
    if !tmux_available("stale_capability_cannot_launch_after_an_owner_downgrade") {
        return;
    }
    let (address, recorder) = start_node_with_memory(true).await;
    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    assert!(wait_for_online(&control, "gpu-box").await);
    assert!(wait_for_memory_capability(&control, "gpu-box").await);

    // Simulate a rolling downgrade behind the same address while the
    // coordinator still has the newer owner's launch options cached. The old
    // owner retains only the legacy route and would ignore an additive field.
    recorder.lock().unwrap().advertise_memory = false;
    let error = control
        .launch(LaunchRequest {
            name: "downgraded-owner-memory".to_owned(),
            directory: "/srv/models".to_owned(),
            profile_id: "profile-0".to_owned(),
            mode_id: None,
            machine: Some("gpu-box".to_owned()),
            resume_session_id: None,
            memory_max_bytes: Some(20 * 1024 * 1024 * 1024),
        })
        .await
        .unwrap_err();
    assert_eq!(error_kind(&error), ErrorKind::Upstream);

    let seen = recorder.lock().unwrap();
    assert!(
        seen.paths
            .iter()
            .all(|path| path.as_str() != "/api/v1/sessions"),
        "a stale capability must never fall back to the legacy launch route"
    );
    assert!(
        seen.bodies
            .iter()
            .all(|body| !body.contains("downgraded-owner-memory")),
        "the downgraded owner must not launch the request"
    );
}

async fn assert_remote_directory_browsing(control: &ControlPlane, recorder: &Shared) {
    let listing = control
        .browse_launch_directories(Some("gpu-box"), Some("/srv/models with space"))
        .await
        .unwrap();
    assert_eq!(listing.machine, "gpu-box");
    assert_eq!(listing.current.as_deref(), Some("/srv/models with space"));
    assert!(
        recorder.lock().unwrap().paths.iter().any(|path| {
            path == "/api/v1/launch-directories?path=%2Fsrv%2Fmodels%20with%20space"
        })
    );
}

async fn assert_remote_directory_actions(control: &ControlPlane, recorder: &Shared) {
    let created = control
        .create_launch_directory(CreateLaunchDirectoryRequest {
            machine: Some("gpu-box".to_owned()),
            directory: "/srv/models with space".to_owned(),
            name: "new project".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(created.listing.machine, "gpu-box");
    assert_eq!(created.directory.path, "/srv/models with space/new project");

    let cloned = control
        .clone_launch_repository(CloneLaunchRepositoryRequest {
            machine: Some("gpu-box".to_owned()),
            directory: "/srv/models with space".to_owned(),
            repository: "git@example.test:team/repo.git".to_owned(),
            destination: Some("repo copy".to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(cloned.directory.path, "/srv/models with space/repo copy");

    let seen = recorder.lock().unwrap();
    for path in [
        "/api/v1/launch-directories/folders",
        "/api/v1/launch-directories/clone",
    ] {
        assert!(
            seen.paths.iter().any(|seen| seen == path),
            "{:#?}",
            seen.paths
        );
    }
    assert!(seen.bodies.iter().any(|body| {
        serde_json::from_str::<Value>(body).is_ok_and(|value| {
            value.get("machine").is_none()
                && value.get("directory").and_then(Value::as_str) == Some("/srv/models with space")
                && value.get("name").and_then(Value::as_str) == Some("new project")
        })
    }));
    assert!(seen.bodies.iter().any(|body| {
        serde_json::from_str::<Value>(body).is_ok_and(|value| {
            value.get("machine").is_none()
                && value.get("repository").and_then(Value::as_str)
                    == Some("git@example.test:team/repo.git")
                && value.get("destination").and_then(Value::as_str) == Some("repo copy")
        })
    }));
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn a_coordinator_groups_routes_and_survives_an_offline_machine() {
    if !tmux_available("a_coordinator_groups_routes_and_survives_an_offline_machine") {
        return;
    }
    let (address, recorder) = start_node().await;
    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    assert!(
        wait_for_online(&control, "gpu-box").await,
        "node never came online"
    );
    // The node's incremental patch must reach the coordinator's mirror too.
    assert!(
        wait_for_session(&control, "gpu-box~%8").await,
        "a node patch was never applied"
    );

    // Machines are grouped, this coordinator first, with the remote's own label.
    let overview = control.overview();
    assert_eq!(overview.machines[0].kind, MachineKind::Local);
    assert_eq!(overview.machines[0].id, "local");
    let remote = &overview.machines[1];
    assert_eq!(remote.id, "gpu-box");
    assert_eq!(remote.label, "GPU box");
    assert!(remote.online);
    assert_eq!(
        remote.address.as_deref(),
        Some(address.to_string().as_str())
    );
    assert!(remote.last_seen_ms.is_some());

    // Host recovery remains owner-scoped across federation. The coordinator
    // forwards only the machine id and an empty command body; script paths and
    // arguments never cross the wire.
    let recovery = control.recovery_status("gpu-box").await.unwrap();
    assert!(recovery.available);
    assert_eq!(recovery.phase, atmux::recovery::RecoveryPhase::Idle);
    let started = control.start_recovery("gpu-box").await.unwrap();
    assert_eq!(started.phase, atmux::recovery::RecoveryPhase::Running);
    {
        let snapshot = recorder.lock().unwrap();
        assert!(
            snapshot
                .paths
                .iter()
                .any(|path| path == "/api/v1/machines/gpu-box/quick-resume")
        );
        assert!(snapshot.bodies.iter().any(|body| body == "{}"));
        assert!(
            !snapshot
                .bodies
                .iter()
                .any(|body| body.contains("resume-tron") || body.contains("/home/"))
        );
    }

    // Remote sessions are re-keyed into the coordinator's composite namespace.
    let federated = overview
        .sessions
        .iter()
        .filter(|session| session.machine == "gpu-box")
        .collect::<Vec<_>>();
    assert!(federated.iter().any(|session| session.id == "gpu-box~%7"));
    assert!(
        federated
            .iter()
            .all(|session| session.id.starts_with("gpu-box~"))
    );
    // Summaries never carry pane output.
    let encoded = serde_json::to_string(&overview).unwrap();
    assert!(
        !encoded.contains("epoch 1"),
        "summaries must stay output-free"
    );

    // Output is fetched on demand and then served from the hash-keyed cache.
    let output = control
        .pane_output("gpu-box~%7", None, 2)
        .await
        .unwrap()
        .expect("federated pane output");
    assert_eq!(output.pane_id, "gpu-box~%7");
    assert_eq!(output.session, "trainer");
    assert_eq!(output.content.as_deref(), Some("epoch 2\nepoch 3"));
    let fetches = || {
        recorder
            .lock()
            .unwrap()
            .paths
            .iter()
            .filter(|path| path.as_str() == "/api/v1/panes/%7")
            .count()
    };
    assert_eq!(fetches(), 1);
    for _ in 0..5 {
        let repeat = control
            .pane_output("gpu-box~%7", None, 80)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeat.content_hash, "aaaa");
    }
    assert_eq!(fetches(), 1, "repeat reads must not re-hit the node");
    assert!(!control.pane_may_have_changed("gpu-box~%7", "aaaa"));

    // Structured conversation logs are fetched from the owning node as data,
    // while its filesystem path remains private to that node.
    let transcript = control
        .transcript("gpu-box~%7", None)
        .await
        .unwrap()
        .expect("federated transcript");
    assert!(transcript.available);
    assert_eq!(transcript.source, "claude");
    assert_eq!(
        transcript.messages.unwrap()[0].markdown,
        "Training is **healthy**."
    );
    assert!(
        recorder
            .lock()
            .unwrap()
            .paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/transcript")
    );

    // File and Git views remain pane-owner scoped. Only the bare pane id and
    // percent-encoded relative path cross the federation boundary; the
    // coordinator restores its composite identity in the returned document.
    let relative = "src/a space #? 雪.rs";
    let files = control
        .pane_files("gpu-box~%7", Some(relative))
        .await
        .unwrap()
        .expect("federated file");
    let FilesResponse::File { pane_id, path, .. } = files else {
        panic!("expected federated file")
    };
    assert_eq!(pane_id, "gpu-box~%7");
    assert_eq!(path, relative);

    let updated = control
        .write_pane_file(
            "gpu-box~%7",
            FileWriteRequest {
                path: relative.to_owned(),
                content: "fn federated_new() {}\n".to_owned(),
                expected_hash: "a".repeat(64),
            },
        )
        .await
        .unwrap()
        .expect("federated file update");
    let FilesResponse::File {
        pane_id,
        content_hash,
        ..
    } = updated
    else {
        panic!("expected updated federated file")
    };
    assert_eq!(pane_id, "gpu-box~%7");
    assert_eq!(
        content_hash.as_deref(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    );

    let conflict = control
        .write_pane_file(
            "gpu-box~%7",
            FileWriteRequest {
                path: relative.to_owned(),
                content: "stale\n".to_owned(),
                expected_hash: "f".repeat(64),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error_kind(&conflict), ErrorKind::Conflict);

    let git = control
        .pane_git("gpu-box~%7", None)
        .await
        .unwrap()
        .expect("federated Git summary");
    let GitResponse::Summary(summary) = git else {
        panic!("expected Git summary")
    };
    assert_eq!(summary.pane_id, "gpu-box~%7");
    assert_eq!(summary.branch.as_deref(), Some("main"));
    let diff = control
        .pane_git("gpu-box~%7", Some("src/federated.rs"))
        .await
        .unwrap()
        .expect("federated Git diff");
    let GitResponse::Diff(diff) = diff else {
        panic!("expected Git diff")
    };
    assert_eq!(diff.pane_id, "gpu-box~%7");
    let paths = recorder.lock().unwrap().paths.clone();
    assert!(
        paths.iter().any(|path| {
            path == "/api/v1/panes/%257/files?path=src%2Fa%20space%20%23%3F%20%E9%9B%AA.rs"
        }),
        "recorded paths: {paths:#?}"
    );
    assert!(
        paths.iter().any(|path| path == "/api/v1/panes/%257/files"),
        "recorded paths: {paths:#?}"
    );
    let bodies = recorder.lock().unwrap().bodies.clone();
    assert!(bodies.iter().any(|body| {
        serde_json::from_str::<Value>(body).is_ok_and(|value| {
            value.get("path").and_then(Value::as_str) == Some(relative)
                && value.get("expected_hash").and_then(Value::as_str)
                    == Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                && value.get("root").is_none()
        })
    }));
    assert!(
        paths
            .iter()
            .any(|path| path == "/api/v1/panes/%257/git?path=src%2Ffederated.rs"),
        "recorded paths: {paths:#?}"
    );

    assert_remote_directory_browsing(&control, &recorder).await;
    assert_remote_directory_actions(&control, &recorder).await;

    route_commands_to_the_owning_machine(&control, &recorder).await;
}
/// Exercises every mutating route against the owning machine and asserts the
/// exact wire form the node received.
#[allow(clippy::too_many_lines)]
async fn route_commands_to_the_owning_machine(control: &ControlPlane, recorder: &Shared) {
    let trainer_instance = format!("pane-v1-{}", "a".repeat(64));
    control
        .send_text_for_instance(
            "gpu-box~%7",
            "resume training".to_owned(),
            true,
            Some(trainer_instance.clone()),
        )
        .await
        .unwrap();
    control
        .send_image_message(
            "gpu-box~%7",
            ImageMessageRequest {
                text: "inspect this".to_owned(),
                images: vec![EncodedImage {
                    media_type: "image/png".to_owned(),
                    data: "iVBORw0KGgo=".to_owned(),
                }],
                instance_id: Some(trainer_instance.clone()),
            },
        )
        .await
        .unwrap();
    let sent_before_mismatch = recorder.lock().unwrap().bodies.len();
    let error = control
        .send_text_for_instance(
            "gpu-box~%7",
            "must not cross generations".to_owned(),
            true,
            Some(format!("pane-v1-{}", "f".repeat(64))),
        )
        .await
        .unwrap_err();
    assert_eq!(error_kind(&error), ErrorKind::Conflict);
    assert_eq!(recorder.lock().unwrap().bodies.len(), sent_before_mismatch);
    for key in [
        PaneSpecialKey::Up,
        PaneSpecialKey::Down,
        PaneSpecialKey::Left,
        PaneSpecialKey::Right,
        PaneSpecialKey::Enter,
        PaneSpecialKey::TmuxPrefixTwice,
    ] {
        control
            .send_special_key_for_instance(
                "gpu-box~%7",
                key,
                "gpu-box".to_owned(),
                trainer_instance.clone(),
            )
            .await
            .unwrap();
    }
    let sent_before_key_mismatch = recorder.lock().unwrap().bodies.len();
    for error in [
        control
            .send_special_key_for_instance(
                "gpu-box~%8",
                PaneSpecialKey::Enter,
                "gpu-box".to_owned(),
                trainer_instance.clone(),
            )
            .await
            .unwrap_err(),
        control
            .send_special_key_for_instance(
                "gpu-box~%7",
                PaneSpecialKey::Down,
                "midnight".to_owned(),
                trainer_instance.clone(),
            )
            .await
            .unwrap_err(),
    ] {
        assert_eq!(error_kind(&error), ErrorKind::Conflict);
    }
    assert_eq!(
        recorder.lock().unwrap().bodies.len(),
        sent_before_key_mismatch,
        "mismatched machine or generation must not cross federation",
    );
    control.interrupt("gpu-box~%7").await.unwrap();
    control.resume_current_claude("gpu-box~%7").await.unwrap();
    control.kill("gpu-box~%8").await.unwrap();
    control
        .launch(LaunchRequest {
            name: "federated-agent".to_owned(),
            directory: "/srv/models".to_owned(),
            profile_id: "profile-0".to_owned(),
            mode_id: None,
            machine: Some("gpu-box".to_owned()),
            resume_session_id: None,
            memory_max_bytes: None,
        })
        .await
        .unwrap();

    let seen = recorder.lock().unwrap();
    assert!(
        seen.paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/messages")
    );
    assert!(
        seen.paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/image-messages")
    );
    assert!(
        seen.paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/input-keys")
    );
    assert!(
        !seen
            .paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/special-keys"),
        "generation-bound controls must never use the legacy route",
    );
    assert!(
        seen.paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/interrupt")
    );
    assert!(
        seen.paths
            .iter()
            .any(|path| path == "/api/v1/panes/%7/resume")
    );
    assert!(seen.paths.iter().any(|path| path == "/api/v1/sessions/%8"));
    let sent = seen
        .bodies
        .iter()
        .find(|body| body.contains("resume training"))
        .expect("message body");
    let sent: Value = serde_json::from_str(sent).unwrap();
    assert_eq!(sent["text"], "resume training");
    assert_eq!(sent["submit"], true);
    assert_eq!(sent["instance_id"], trainer_instance);
    let image_message: Value = seen
        .bodies
        .iter()
        .find(|body| body.contains("inspect this"))
        .map(|body| serde_json::from_str(body).unwrap())
        .expect("image message body");
    assert_eq!(image_message["images"][0]["media_type"], "image/png");
    assert_eq!(image_message["images"][0]["data"], "iVBORw0KGgo=");
    assert_eq!(image_message["instance_id"], trainer_instance);
    let special_keys: Value = seen
        .bodies
        .iter()
        .find(|body| body.contains("tmux_prefix_twice"))
        .map(|body| serde_json::from_str(body).unwrap())
        .expect("special key body");
    assert_eq!(special_keys["action"], "tmux_prefix_twice");
    assert_eq!(special_keys["machine"], "gpu-box");
    assert_eq!(special_keys["instance_id"], trainer_instance);
    for action in ["up", "down", "left", "right", "enter"] {
        assert!(seen.bodies.iter().any(|body| {
            serde_json::from_str::<Value>(body).is_ok_and(|value| {
                value.get("action").and_then(Value::as_str) == Some(action)
                    && value.get("machine").and_then(Value::as_str) == Some("gpu-box")
                    && value.get("instance_id").and_then(Value::as_str)
                        == Some(trainer_instance.as_str())
            })
        }));
    }
    let resume: Value = seen
        .bodies
        .iter()
        .find(|body| body.as_str() == "{}")
        .map(|body| serde_json::from_str(body).unwrap())
        .expect("resume body");
    assert_eq!(resume, json!({}));
    let launched: Value = seen
        .bodies
        .iter()
        .filter(|body| body.contains("federated-agent"))
        .map(|body| serde_json::from_str(body).unwrap())
        .next()
        .expect("launch body");
    assert_eq!(launched["name"], "federated-agent");
    // The forwarded request never re-hops to another machine.
    assert_eq!(launched["machine"], Value::Null);
    drop(seen);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_unreachable_machine_never_breaks_local_or_healthy_machines() {
    if !tmux_available("one_unreachable_machine_never_breaks_local_or_healthy_machines") {
        return;
    }
    let (address, _recorder) = start_node().await;
    let mut config = federated_config(address);
    config.machines.push(MachineConfig {
        id: "dead".to_owned(),
        label: Some("Dead box".to_owned()),
        // Port 1 is closed, so this machine can never connect.
        url: "http://127.0.0.1:1".to_owned(),
        token_env: None,
        token_file: None,
    });
    let control = ControlPlane::start(config).await.unwrap();
    assert!(
        wait_for_online(&control, "gpu-box").await,
        "healthy node never came online"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut dead = None;
    while Instant::now() < deadline {
        let machines = control.machines();
        let candidate = machines
            .iter()
            .find(|machine| machine.id == "dead")
            .cloned();
        if candidate.as_ref().is_some_and(|machine| {
            !machine.online && machine.health.as_deref() != Some("connecting")
        }) {
            dead = candidate;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let dead = dead.expect("the unreachable machine was never marked offline");
    assert!(!dead.online);
    assert_eq!(dead.label, "Dead box");
    assert!(
        dead.health.is_some(),
        "an offline machine must explain itself"
    );
    assert!(dead.last_seen_ms.is_none());

    // The local machine and the healthy remote are unaffected.
    let overview = control.overview();
    assert!(overview.machines[0].online, "local must stay usable");
    assert!(
        overview
            .machines
            .iter()
            .any(|m| m.id == "gpu-box" && m.online)
    );
    assert!(
        control
            .pane_output("gpu-box~%7", None, 80)
            .await
            .unwrap()
            .is_some()
    );
    assert!(control.launch_options().machines[0].online);

    // Operations aimed at the dead machine fail fast with a clear reason.
    let error = control
        .pane_output("dead~%1", None, 80)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("offline"), "{error}");
    assert!(
        control
            .send_text("dead~%1", "hi".to_owned(), true)
            .await
            .is_err()
    );
    assert!(
        control
            .launch(LaunchRequest {
                name: "nope".to_owned(),
                directory: "/srv".to_owned(),
                profile_id: "profile-0".to_owned(),
                mode_id: None,
                machine: Some("dead".to_owned()),
                resume_session_id: None,
                memory_max_bytes: None,
            })
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configuration_without_machines_reports_only_the_local_machine() {
    if !tmux_available("a_configuration_without_machines_reports_only_the_local_machine") {
        return;
    }
    let control = ControlPlane::start(Config::default()).await.unwrap();
    let overview = control.overview();
    assert_eq!(overview.machines.len(), 1);
    assert_eq!(overview.machines[0].id, "local");
    assert_eq!(overview.machines[0].kind, MachineKind::Local);
    assert_eq!(control.local_id(), "local");
    // With no [[machines]] the emitted identity is the bare tmux pane id this
    // process used before federation, so saved URLs and MCP clients still work.
    assert!(
        overview
            .sessions
            .iter()
            .all(|session| session.id == session.pane_id && session.machine == "local")
    );
    let options = control.launch_options();
    assert_eq!(options.machines.len(), 1);
    assert_eq!(options.machines[0].directories, options.directories);
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_readers_of_one_remote_pane_cause_exactly_one_node_fetch() {
    if !tmux_available("simultaneous_readers_of_one_remote_pane_cause_exactly_one_node_fetch") {
        return;
    }
    let (address, recorder) = start_node().await;
    // Hold every caller inside the same fetch window. Without single-flight
    // each of them misses the cache and issues its own request.
    recorder.lock().unwrap().pane_latency = Duration::from_millis(200);

    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    assert!(wait_for_session(&control, "gpu-box~%7").await);

    let readers = (0..8)
        .map(|_| {
            let control = control.clone();
            tokio::spawn(async move { control.pane_output("gpu-box~%7", None, 80).await })
        })
        .collect::<Vec<_>>();
    for reader in readers {
        let output = reader
            .await
            .unwrap()
            .unwrap()
            .expect("federated pane output");
        assert_eq!(output.content_hash, "aaaa");
        assert_eq!(output.content.as_deref(), Some("epoch 1\nepoch 2\nepoch 3"));
    }

    let fetches = recorder
        .lock()
        .unwrap()
        .paths
        .iter()
        .filter(|path| path.as_str() == "/api/v1/panes/%7")
        .count();
    assert_eq!(
        fetches, 1,
        "eight simultaneous readers must collapse into one node fetch"
    );
}

/// A node that snapshots and then immediately desynchronizes, so the
/// coordinator must resync — and must keep doing so at the base retry delay.
async fn start_desyncing_node() -> (SocketAddr, Arc<Mutex<Vec<Instant>>>) {
    let opens: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::clone(&opens);
    let app = Router::new().route(
        "/api/v1/events",
        get(move || {
            observer.lock().unwrap().push(Instant::now());
            async move {
                let snapshot = json!({
                    "revision": 4,
                    "sessions": [session("%7", "trainer", "working", "aaaa")],
                    "health": null,
                    "machines": node_machines(),
                });
                // A patch built on a revision the coordinator never mirrored.
                let patch = json!({
                    "base_revision": 99,
                    "revision": 100,
                    "upsert": [session("%8", "ghost", "working", "bbbb")],
                    "remove": [],
                    "health": null,
                    "machines": node_machines(),
                });
                let stream = async_stream::stream! {
                    yield Ok::<_, std::io::Error>(axum::body::Bytes::from(format!(
                        "event: sessions.snapshot\ndata: {snapshot}\n\n"
                    )));
                    yield Ok(axum::body::Bytes::from(format!(
                        "event: sessions.patch\ndata: {patch}\n\n"
                    )));
                    tokio::time::sleep(Duration::from_secs(30)).await;
                };
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(stream),
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, opens)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_desynchronized_node_resyncs_at_the_base_delay_instead_of_backing_off() {
    if !tmux_available("a_desynchronized_node_resyncs_at_the_base_delay_instead_of_backing_off") {
        return;
    }
    let (address, opens) = start_desyncing_node().await;
    let _control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();

    // Each cycle is: connect, snapshot (healthy), bad patch, resync. Because a
    // valid snapshot resets the failure counter, every retry waits the 0.5s base
    // delay. Without that reset the delays would be 0.5s, 1s, 2s, 4s, so the
    // fifth connection could not arrive inside this budget.
    let count = || opens.lock().unwrap().len();
    assert!(
        settles(|| count() >= 5, Duration::from_secs(4)).await,
        "only {} reconnects in 4s; backoff is escalating instead of resetting",
        count()
    );

    let times = opens.lock().unwrap().clone();
    let gaps = times
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]))
        .collect::<Vec<_>>();
    assert!(
        gaps.iter().all(|gap| *gap < Duration::from_millis(1_500)),
        "a reconnect gap grew past the base delay: {gaps:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_http_surface_reports_an_upstream_rejection_as_a_gateway_failure() {
    if !tmux_available("the_http_surface_reports_an_upstream_rejection_as_a_gateway_failure") {
        return;
    }
    let (address, _recorder) = start_node().await;
    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    // %8 exists in the mirror but the node refuses to serve its output, so the
    // failure belongs to the owning machine rather than to the caller.
    assert!(wait_for_session(&control, "gpu-box~%8").await);

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let app = atmux::web::api_router(control, Vec::new(), shutdown_rx);
    let status = |uri: &'static str| {
        let app = app.clone();
        async move {
            app.oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };

    assert_eq!(
        status("/api/v1/panes/gpu-box~%258").await,
        StatusCode::BAD_GATEWAY,
        "a node rejecting a read is an upstream failure"
    );
    assert_eq!(
        status("/api/v1/panes/gpu-box~%258/files").await,
        StatusCode::BAD_GATEWAY,
        "a node rejecting a file read is an upstream failure"
    );
    assert_eq!(
        status("/api/v1/panes/gpu-box~%258/git").await,
        StatusCode::BAD_GATEWAY,
        "a node rejecting a Git read is an upstream failure"
    );
    assert_eq!(status("/api/v1/panes/gpu-box~%257").await, StatusCode::OK);
    assert_eq!(
        status("/api/v1/panes/gpu-box~%259").await,
        StatusCode::NOT_FOUND,
        "a pane no machine reports is simply missing"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn owner_generation_conflicts_remain_conflicts_through_a_coordinator() {
    if !tmux_available("owner_generation_conflicts_remain_conflicts_through_a_coordinator") {
        return;
    }
    let (address, recorder) = start_node().await;
    let control = ControlPlane::start(federated_config(address))
        .await
        .unwrap();
    assert!(wait_for_session(&control, "gpu-box~%7").await);
    recorder.lock().unwrap().reject_generation_mutations = true;
    let instance_id = format!("pane-v1-{}", "a".repeat(64));

    let direct = control
        .send_text_for_instance(
            "gpu-box~%7",
            "direct conflict".to_owned(),
            true,
            Some(instance_id.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error_kind(&direct), ErrorKind::Conflict);
    assert!(
        !direct.to_string().contains("owner-only-generation-detail"),
        "owner response detail escaped through the coordinator: {direct:#}"
    );
    let key_conflict = control
        .send_special_key_for_instance(
            "gpu-box~%7",
            PaneSpecialKey::Enter,
            "gpu-box".to_owned(),
            instance_id.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error_kind(&key_conflict), ErrorKind::Conflict);
    assert!(
        !key_conflict
            .to_string()
            .contains("owner-only-generation-detail"),
        "owner response detail escaped through special-key federation: {key_conflict:#}"
    );

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let app = atmux::web::api_router(control, Vec::new(), shutdown_rx);
    let cases = [
        (
            "/api/v1/panes/gpu-box~%257/messages",
            json!({
                "text": "HTTP text conflict",
                "submit": true,
                "instance_id": instance_id,
            }),
        ),
        (
            "/api/v1/panes/gpu-box~%257/image-messages",
            json!({
                "text": "HTTP image conflict",
                "images": [{ "media_type": "image/png", "data": "iVBORw0KGgo=" }],
                "instance_id": instance_id,
            }),
        ),
        (
            "/api/v1/panes/gpu-box~%257/input-keys",
            json!({
                "action": "enter",
                "machine": "gpu-box",
                "instance_id": instance_id,
            }),
        ),
    ];
    for (uri, body) in cases {
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT, "{uri}");
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            !body
                .windows("owner-only-generation-detail".len())
                .any(|window| window == b"owner-only-generation-detail"),
            "owner response detail escaped through {uri}"
        );
    }

    let seen = recorder.lock().unwrap();
    let rejected = seen
        .bodies
        .iter()
        .filter_map(|body| serde_json::from_str::<Value>(body).ok())
        .filter(|body| {
            body.get("instance_id").and_then(Value::as_str) == Some(instance_id.as_str())
        })
        .count();
    assert!(rejected >= 5, "forwarded bodies: {:#?}", seen.bodies);
}
