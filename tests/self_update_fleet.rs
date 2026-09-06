//! Coordinator-side self-update aggregation against a real HTTP node.
//!
//! A small axum server stands in for a remote `atmux web` process. The
//! coordinator runs coordinator-only, so no tmux is involved and the test
//! exercises exactly the federation path a public coordinator uses: read every
//! machine's update document, and forward one fixed verb to the owning node.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use atmux::{
    config::{Config, GeneralConfig, MachineConfig},
    control::ControlPlane,
    self_update::Action,
};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

/// Every path the fake node was asked for, in order.
type Recorder = Arc<Mutex<Vec<String>>>;

fn node_update_document() -> Value {
    json!({
        "enabled": true,
        "version": "0.2.0",
        "target": "aarch64-unknown-linux-gnu",
        "mode": "self",
        "latest": {
            "version": "0.3.0",
            "tag": "v0.3.0",
            "published_at": "2026-09-06T12:00:00Z",
            "verified": true,
            "asset": "atmux-aarch64-unknown-linux-gnu",
        },
        "state": "idle",
        "progress": null,
        "last_checked_at": 1_757_160_000_000_u64,
        "last_error": null,
        "previous": null,
    })
}

async fn node_update(State(recorder): State<Recorder>) -> Json<Value> {
    recorder.lock().unwrap().push("/api/v1/update".to_owned());
    Json(node_update_document())
}

async fn node_apply(State(recorder): State<Recorder>) -> (StatusCode, Json<Value>) {
    recorder
        .lock()
        .unwrap()
        .push("/api/v1/update/apply".to_owned());
    let mut document = node_update_document();
    document["state"] = Value::from("downloading");
    document["progress"] = json!({ "downloaded": 0, "total": 9_000_000 });
    (StatusCode::ACCEPTED, Json(document))
}

async fn node_rollback(State(recorder): State<Recorder>) -> Response {
    recorder
        .lock()
        .unwrap()
        .push("/api/v1/update/rollback".to_owned());
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "no previous atmux executable" })),
    )
        .into_response()
}

async fn node_events() -> Response {
    let snapshot = json!({
        "revision": 1,
        "sessions": [],
        "health": null,
        "machines": [],
    });
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from(format!(
            "event: sessions.snapshot\nid: 1\ndata: {snapshot}\n\n"
        )));
        // Hold the connection open the way a live node does.
        tokio::time::sleep(Duration::from_secs(30)).await;
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn start_node() -> (SocketAddr, Recorder) {
    let recorder: Recorder = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/api/v1/events", get(node_events))
        .route("/api/v1/update", get(node_update))
        .route("/api/v1/update/apply", post(node_apply))
        .route("/api/v1/update/rollback", post(node_rollback))
        .with_state(Arc::clone(&recorder));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, recorder)
}

/// A coordinator with no owner-local capabilities, so this test needs no tmux.
fn coordinator_config(address: SocketAddr) -> Config {
    let mut config = Config::default();
    config.node.coordinator_only = true;
    config.profiles.clear();
    config.general = GeneralConfig {
        project_roots: Vec::new(),
        favorite_dirs: Vec::new(),
        switch_on_launch: false,
        ..config.general
    };
    config.machines = vec![MachineConfig {
        id: "gpu-box".to_owned(),
        label: Some("GPU box".to_owned()),
        url: format!("http://{address}"),
        token_env: None,
        token_file: None,
    }];
    config
}

async fn wait_until_online(control: &ControlPlane) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if control
            .machines()
            .iter()
            .any(|machine| machine.id == "gpu-box" && machine.online)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the fake node never came online");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_aggregates_and_forwards_node_update_state() {
    let (address, recorder) = start_node().await;
    let control = ControlPlane::start(coordinator_config(address))
        .await
        .unwrap();
    wait_until_online(&control).await;

    let fleet = control.fleet_updates().await;
    assert_eq!(fleet.len(), 2, "{fleet:#?}");
    let local = fleet
        .iter()
        .find(|entry| entry.id == "local")
        .expect("a coordinator always reports itself");
    assert!(local.error.is_none());
    let local_update = local.update.as_ref().unwrap();
    assert_eq!(local_update.target, atmux::self_update::TARGET);
    assert_eq!(local_update.version, atmux::self_update::VERSION);

    let remote = fleet
        .iter()
        .find(|entry| entry.id == "gpu-box")
        .expect("every configured machine is reported");
    assert_eq!(remote.label, "GPU box");
    assert!(remote.online);
    assert!(remote.error.is_none(), "{:?}", remote.error);
    let update = remote.update.as_ref().unwrap();
    assert_eq!(update.mode, atmux::self_update::Mode::Own);
    let latest = update.latest.as_ref().unwrap();
    assert_eq!(latest.version, "0.3.0");
    assert!(latest.verified);

    // A forwarded verb reaches exactly one fixed owner path.
    let started = control
        .machine_update_action("gpu-box", Action::Apply)
        .await
        .unwrap();
    assert_eq!(started.state, atmux::self_update::Phase::Downloading);
    assert_eq!(
        started.progress.unwrap().total,
        Some(9_000_000),
        "the owner's own progress is passed through unchanged"
    );

    // The owner's 409 stays a 409 rather than becoming the coordinator's fault.
    let refused = control
        .machine_update_action("gpu-box", Action::Rollback)
        .await
        .unwrap_err();
    assert_eq!(
        atmux::control::error_kind(&refused),
        atmux::control::ErrorKind::Conflict,
        "{refused:#}"
    );

    // A machine that is not in the configured list never becomes a request.
    let unknown = control
        .machine_update_action("ghost", Action::Check)
        .await
        .unwrap_err();
    assert_eq!(
        atmux::control::error_kind(&unknown),
        atmux::control::ErrorKind::BadRequest,
        "{unknown:#}"
    );

    let seen = recorder.lock().unwrap().clone();
    assert!(seen.contains(&"/api/v1/update".to_owned()), "{seen:?}");
    assert!(
        seen.contains(&"/api/v1/update/apply".to_owned()),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|path| path.contains("ghost")),
        "an unknown machine must never produce a request: {seen:?}"
    );
}
