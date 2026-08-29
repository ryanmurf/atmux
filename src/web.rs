use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_stream::stream;
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event, sse::KeepAlive},
    routing::{delete, get, post},
};
#[cfg(feature = "pulse")]
use axum::{body::to_bytes, http::Method};
use serde::{Deserialize, Serialize};
use tokio::{sync::watch, task::JoinSet};

use crate::{
    MAX_REQUEST_BODY_BYTES,
    attachment::{ImageMessageRequest, MAX_ATTACHMENT_REQUEST_BODY_BYTES},
    config::Config,
    control::{
        ControlPlane, ErrorKind, LaunchDirectoryListing, LaunchRequest, ModelSwitchRequest,
        Overview, PaneModels, PaneOutput, ResumableLaunchSessions, error_kind, overview_patch,
        pane_patch,
    },
    discovery,
    machine::{MachineSummary, Secret, resolve_token},
    mcp,
    recovery::RecoveryStatus,
    transcript::Transcript,
    workspace::{FileWriteRequest, FilesResponse, GitResponse, MAX_FILE_WRITE_REQUEST_BYTES},
};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
const ATMUX_LOGO_JPG: &[u8] = include_bytes!("../web/atmux-logo.jpg");

#[derive(Clone, Debug)]
struct WebState {
    control: ControlPlane,
    allowed_origins: Arc<[String]>,
    shutdown: watch::Receiver<bool>,
}

#[derive(Clone, Debug)]
struct RequestPolicy {
    allowed_hosts: Arc<[String]>,
    /// Bearer tokens required of every API and MCP caller unless the operator
    /// explicitly enabled unauthenticated loopback development mode.
    tokens: Arc<[Secret]>,
    allow_unauthenticated_loopback: bool,
}

#[derive(Debug, Deserialize)]
struct OutputQuery {
    known_hash: Option<String>,
    lines: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TranscriptQuery {
    known_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelativePathQuery {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LaunchDirectoryQuery {
    machine: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LaunchSessionsQuery {
    machine: Option<String>,
    directory: String,
    profile_id: String,
}

#[derive(Debug, Deserialize)]
struct SendRequest {
    text: String,
    #[serde(default = "default_submit")]
    submit: bool,
}

#[derive(Debug, Deserialize)]
struct SpecialKeyRequest {
    action: String,
}

/// Deliberately empty: the owning node derives the Claude config root and
/// native session id itself, never from a browser request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeCurrentClaudeRequest {}

/// Deliberately empty: callers choose only the owning machine. The owner runs
/// its one compiled-in recovery script with no browser-supplied path or args.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuickResumeRequest {}

const fn default_submit() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    revision: u64,
    health: Option<String>,
}

impl From<Overview> for HealthResponse {
    fn from(overview: Overview) -> Self {
        Self {
            ok: overview.health.is_none(),
            revision: overview.revision,
            health: overview.health,
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    /// Maps a control-plane failure onto the status that describes whose
    /// problem it is: the caller's, an owning machine's, or this process's.
    ///
    /// The classification is carried by the error itself, so reads and
    /// mutations report the same failure the same way.
    fn from_control(error: &anyhow::Error) -> Self {
        let status = match error_kind(error) {
            ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::Offline => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Upstream => StatusCode::BAD_GATEWAY,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

struct WebBinding {
    listeners: Vec<(std::net::TcpListener, SocketAddr)>,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    node_token: Option<Secret>,
    discovery_token: Option<Secret>,
    proxy_token: Option<Secret>,
}

fn prepare_web_binding(
    config: &Config,
    bind: SocketAddr,
    allow_remote: bool,
) -> Result<WebBinding> {
    if !allow_remote && !bind.ip().is_loopback() {
        bail!(
            "refusing non-loopback bind {bind}; pass --allow-remote to acknowledge shell-equivalent access"
        );
    }
    let remote_bind = !bind.ip().is_loopback();
    if remote_bind && config.node.tls.is_none() {
        bail!(
            "refusing non-loopback bind {bind} without [node.tls]; remote access must use certificate-validating HTTPS"
        );
    }
    let listeners = bind_listeners(bind)?;
    let tls = remote_bind
        .then(|| {
            config
                .node
                .tls
                .as_ref()
                .context("[node.tls] unexpectedly missing for remote listener")
                .and_then(crate::tls::server_config)
        })
        .transpose()?;
    let node_token = resolve_token(
        &config.node.id,
        config.node.token_env.as_deref(),
        config.node.token_file.as_deref(),
    )
    .context("failed to resolve the [node] token")?;
    let discovery_token = if config.discovery.enabled {
        let token = resolve_token(
            "discovery",
            config.discovery.token_env.as_deref(),
            config.discovery.token_file.as_deref(),
        )?
        .context("[discovery] token unexpectedly missing")?;
        let node = node_token
            .as_ref()
            .context("[discovery] node token unexpectedly missing")?;
        if token.expose() != node.expose() {
            bail!(
                "[discovery] token must match the [node] token so nearby nodes can authenticate each other"
            );
        }
        Some(token)
    } else {
        None
    };
    let proxy_token = resolve_token(
        "web proxy",
        config.web.proxy_token_env.as_deref(),
        config.web.proxy_token_file.as_deref(),
    )
    .context("failed to resolve the [web] proxy token")?;
    if let (Some(node), Some(proxy)) = (&node_token, &proxy_token)
        && constant_time_eq(node.expose().as_bytes(), proxy.expose().as_bytes())
    {
        bail!("[web] proxy token must differ from the [node] federation token");
    }
    if remote_bind && node_token.is_none() && proxy_token.is_none() {
        bail!("refusing non-loopback bind {bind} without a [node] or [web] credential");
    }
    Ok(WebBinding {
        listeners,
        tls,
        node_token,
        discovery_token,
        proxy_token,
    })
}

#[cfg(feature = "pulse")]
struct PulseWebComponents {
    runtime: Option<crate::pulse::service::PulseRuntime>,
    api: Option<crate::pulse::api::PulseApi>,
    receiver: Option<Arc<crate::pulse::ingest::IngestReceiver>>,
    federation: Option<Arc<crate::pulse::federation::DirectFederationExporter>>,
}

#[cfg(feature = "pulse")]
async fn start_pulse_web_components(
    config: &Config,
    control: ControlPlane,
) -> Result<PulseWebComponents> {
    let runtime = crate::pulse::service::start_embedded_with_control_plane(
        &config.pulse,
        &config.node.id,
        control,
    )
    .await
    .context("failed to start embedded Pulse runtime")?;
    let local_machine = crate::pulse::MachineName::new(config.node.id.clone())
        .context("validated node id is not a Pulse machine name")?;
    let api = runtime
        .as_ref()
        .filter(|_| config.pulse.serve)
        .map(|runtime| {
            crate::pulse::api::PulseApi::new(
                runtime.store(),
                runtime.accounts().as_ref(),
                crate::pulse::api::PulseCapabilities {
                    collect: config.pulse.collect,
                    serve: config.pulse.serve,
                    receive: config.pulse.receive,
                },
            )
            .with_management(
                local_machine,
                runtime.management(),
                crate::pulse::api::PulseDeliveryCapabilities {
                    pull: true,
                    pane: true,
                    channel: false,
                },
            )
            .with_invalidations(runtime.invalidations())
        });
    let receiver = runtime
        .as_ref()
        .and_then(crate::pulse::service::PulseRuntime::receiver);
    let federation = runtime
        .as_ref()
        .map(crate::pulse::service::PulseRuntime::federation_exporter);
    Ok(PulseWebComponents {
        runtime,
        api,
        receiver,
        federation,
    })
}

/// Runs the web dashboard and stateless MCP server.
///
/// # Errors
///
/// Returns an error for unsafe bind configuration, tmux startup, or HTTP I/O.
pub async fn serve(
    config: Config,
    bind: SocketAddr,
    allow_remote: bool,
    extra_hosts: Vec<String>,
    extra_origins: Vec<String>,
) -> Result<()> {
    let binding = prepare_web_binding(&config, bind, allow_remote)?;
    let machine_count = config.machines.len();
    let control = ControlPlane::start(config.clone()).await?;
    let _discovery = binding
        .discovery_token
        .map(|token| discovery::start(&config, bind, control.clone(), token))
        .transpose()?;
    let hosts: Arc<[String]> = allowed_hosts(bind, extra_hosts).into();
    let origins: Arc<[String]> = allowed_origins(bind, extra_origins).into();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = WebState {
        control: control.clone(),
        allowed_origins: origins.clone(),
        shutdown: shutdown_rx.clone(),
    };
    #[cfg(feature = "pulse")]
    let PulseWebComponents {
        runtime: pulse_runtime,
        api: pulse_api,
        receiver: pulse_receiver,
        federation: pulse_federation,
    } = start_pulse_web_components(&config, control.clone()).await?;
    #[cfg(feature = "pulse")]
    let mcp_service =
        mcp::service_with_pulse(control, pulse_api.clone(), hosts.as_ref(), origins.as_ref());
    #[cfg(not(feature = "pulse"))]
    let mcp_service = mcp::service(control, hosts.as_ref(), origins.as_ref());
    let policy = RequestPolicy {
        allowed_hosts: hosts,
        tokens: binding
            .node_token
            .into_iter()
            .chain(binding.proxy_token)
            .collect(),
        allow_unauthenticated_loopback: config.web.allow_unauthenticated_loopback,
    };
    let app = routes(state.clone());
    #[cfg(feature = "pulse")]
    let app = match pulse_api {
        Some(api) => app.merge(pulse_routes(
            api,
            pulse_receiver,
            pulse_federation,
            state.allowed_origins.clone(),
        )),
        None => app,
    };
    let app = app
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            policy,
            enforce_request_policy,
        ));

    print_listeners(&binding.listeners);
    println!("atmux MCP  endpoint(s) above (stateless MCP 2026-07-28)");
    if machine_count > 0 {
        println!("atmux fed  aggregating {machine_count} remote machine(s)");
    }
    serve_with_lifecycle(
        binding.listeners,
        binding.tls,
        app,
        shutdown_tx,
        shutdown_rx,
        #[cfg(feature = "pulse")]
        pulse_runtime,
    )
    .await
}

async fn serve_with_lifecycle(
    listeners: Vec<(std::net::TcpListener, SocketAddr)>,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    app: Router,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    #[cfg(feature = "pulse")] pulse: Option<crate::pulse::service::PulseRuntime>,
) -> Result<()> {
    let result = serve_listeners(listeners, tls, app, shutdown_tx, shutdown_rx).await;
    #[cfg(feature = "pulse")]
    if let Some(pulse) = pulse {
        pulse.shutdown().await;
    }
    result
}

#[cfg(feature = "pulse")]
fn pulse_routes(
    api: crate::pulse::api::PulseApi,
    receiver: Option<Arc<crate::pulse::ingest::IngestReceiver>>,
    federation: Option<Arc<crate::pulse::federation::DirectFederationExporter>>,
    allowed_origins: Arc<[String]>,
) -> Router {
    let routes = crate::pulse::api::router(api);
    let routes = receiver.map_or(routes.clone(), |receiver| {
        routes.merge(
            Router::new()
                .route("/api/v1/pulse/ingest", post(pulse_ingest))
                .with_state(receiver),
        )
    });
    let routes = federation.map_or(routes.clone(), |federation| {
        routes.merge(
            Router::new()
                .route(
                    "/api/v1/pulse/accounts/{account}/federation",
                    get(pulse_federation_page),
                )
                .with_state(federation),
        )
    });
    routes
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            allowed_origins,
            enforce_mutation_origin,
        ))
}

#[cfg(feature = "pulse")]
async fn pulse_ingest(
    State(receiver): State<Arc<crate::pulse::ingest::IngestReceiver>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    use crate::pulse::ingest::{
        MAX_PUSH_BODY_BYTES, ReceiverRequest, ReceiverTransport, VerifiedReceiverBoundary,
    };

    let bearer = pulse_ingest_bearer(request.headers(), peer.ip()).to_owned();
    let Ok(body) = to_bytes(request.into_body(), MAX_PUSH_BODY_BYTES).await else {
        return pulse_transport_error(&crate::pulse::PulseError::invalid_input(
            "Pulse push body exceeded 1 MiB",
        ));
    };
    let transport = if peer.ip().is_loopback() {
        ReceiverTransport::Plaintext
    } else {
        // Non-loopback listeners are constructed only after TLS configuration
        // is validated in `serve`; there is no plaintext remote listener.
        ReceiverTransport::Tls
    };
    match receiver
        .receive(ReceiverRequest {
            boundary: VerifiedReceiverBoundary::after_host_auth_origin_checks(),
            peer_ip: peer.ip(),
            transport,
            bearer: &bearer,
            body: &body,
            received_at: crate::pulse::Instant::now(),
        })
        .await
    {
        Ok(result) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "replayed": result.replayed,
                "snapshots": result.result.snapshots,
                "token_grains": result.result.token_grains,
                "context_sessions": result.result.context_sessions,
                "gemini_quotas": result.result.gemini_quotas,
            })),
        )
            .into_response(),
        Err(error) => pulse_transport_error(&error),
    }
}

#[cfg(feature = "pulse")]
fn pulse_ingest_bearer(headers: &HeaderMap, peer: IpAddr) -> &str {
    let separate = headers
        .get("x-atmux-pulse-token")
        .and_then(|value| value.to_str().ok());
    if !peer.is_loopback() {
        return separate.unwrap_or_default();
    }
    separate
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(bearer_credential)
        })
        .unwrap_or_default()
}

#[cfg(feature = "pulse")]
fn pulse_transport_error(error: &crate::pulse::PulseError) -> Response {
    let status = match error.kind() {
        crate::pulse::PulseErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
        crate::pulse::PulseErrorKind::NotFound => StatusCode::NOT_FOUND,
        crate::pulse::PulseErrorKind::Conflict => StatusCode::CONFLICT,
        crate::pulse::PulseErrorKind::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        crate::pulse::PulseErrorKind::Authentication => StatusCode::UNAUTHORIZED,
        crate::pulse::PulseErrorKind::Offline => StatusCode::SERVICE_UNAVAILABLE,
        crate::pulse::PulseErrorKind::Upstream => StatusCode::BAD_GATEWAY,
        crate::pulse::PulseErrorKind::Storage
        | crate::pulse::PulseErrorKind::Configuration
        | crate::pulse::PulseErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({
            "error": error.message(),
            "kind": error.kind(),
        })),
    )
        .into_response()
}

#[cfg(feature = "pulse")]
#[derive(Debug, Deserialize)]
struct PulseFederationQuery {
    cursor: Option<String>,
    limit: Option<u16>,
}

#[cfg(feature = "pulse")]
async fn pulse_federation_page(
    State(exporter): State<Arc<crate::pulse::federation::DirectFederationExporter>>,
    Path(account): Path<i64>,
    Query(query): Query<PulseFederationQuery>,
) -> Response {
    use crate::pulse::federation::{DEFAULT_PAGE_ROWS, OpaqueCursor, VerifiedFederationBoundary};

    let account = match crate::pulse::AccountId::new(account) {
        Ok(account) => account,
        Err(error) => return pulse_transport_error(&error),
    };
    let cursor = match query.cursor.map(OpaqueCursor::new).transpose() {
        Ok(cursor) => cursor,
        Err(error) => return pulse_transport_error(&error),
    };
    match exporter
        .page(
            VerifiedFederationBoundary::after_host_and_node_auth_checks(),
            account,
            cursor,
            query.limit.unwrap_or(DEFAULT_PAGE_ROWS),
        )
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(error) => pulse_transport_error(&error),
    }
}

#[cfg(feature = "pulse")]
async fn enforce_mutation_origin(
    State(allowed): State<Arc<[String]>>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) && let Err(error) = ensure_origin(request.headers(), &allowed)
    {
        return error.into_response();
    }
    next.run(request).await
}

/// Chooses concrete sockets for a requested bind.
///
/// A wildcard remote bind is expanded into the current non-loopback interface
/// addresses. That leaves the loopback address free for the local HTTP UI on
/// the same port, while every network-reachable listener remains strict mTLS.
fn listener_addresses(bind: SocketAddr) -> Result<Vec<SocketAddr>> {
    if bind.ip().is_loopback() {
        return Ok(vec![bind]);
    }

    let mut addresses = if bind.ip().is_unspecified() {
        if_addrs::get_if_addrs()
            .context("failed to enumerate network interfaces for the web listener")?
            .into_iter()
            .map(|interface| interface.ip())
            .filter(|ip| {
                ip.is_ipv4() == bind.ip().is_ipv4()
                    && !ip.is_loopback()
                    && !ip.is_unspecified()
                    && !ip.is_multicast()
            })
            .map(|ip| SocketAddr::new(ip, bind.port()))
            .collect::<Vec<_>>()
    } else {
        vec![bind]
    };
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        bail!("no usable non-loopback address is available for remote bind {bind}");
    }

    let loopback = if bind.ip().is_ipv4() {
        "127.0.0.1"
    } else {
        "::1"
    }
    .parse::<IpAddr>()
    .expect("loopback address is valid");
    addresses.push(SocketAddr::new(loopback, bind.port()));
    Ok(addresses)
}

fn bind_listeners(bind: SocketAddr) -> Result<Vec<(std::net::TcpListener, SocketAddr)>> {
    listener_addresses(bind)?
        .into_iter()
        .map(|address| {
            let listener = std::net::TcpListener::bind(address)
                .with_context(|| format!("failed to bind atmux listener at {address}"))?;
            listener
                .set_nonblocking(true)
                .context("failed to configure the web listener")?;
            let address = listener
                .local_addr()
                .context("listener has no local address")?;
            Ok((listener, address))
        })
        .collect()
}

fn print_listeners(listeners: &[(std::net::TcpListener, SocketAddr)]) {
    for (_, address) in listeners {
        let scheme = if address.ip().is_loopback() {
            "http"
        } else {
            "https"
        };
        let scope = if address.ip().is_loopback() {
            " (local browser)"
        } else {
            " (mutual TLS)"
        };
        println!("atmux web  {scheme}://{address}{scope}");
    }
}

async fn serve_listeners(
    listeners: Vec<(std::net::TcpListener, SocketAddr)>,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    app: Router,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let signal_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        shutdown_signal(signal_tx).await;
    });

    let mut servers = JoinSet::new();
    for (listener, address) in listeners {
        let app = app.clone();
        let shutdown = shutdown_rx.clone();
        let listener_tls = (!address.ip().is_loopback()).then(|| tls.clone()).flatten();
        servers.spawn(async move { serve_listener(listener, listener_tls, app, shutdown).await });
    }

    let first = servers
        .join_next()
        .await
        .context("atmux started without any web listeners")?
        .context("atmux web listener task failed")?;
    shutdown_tx.send_replace(true);
    while let Some(result) = servers.join_next().await {
        result.context("atmux web listener task failed")??;
    }
    first
}

async fn serve_listener(
    listener: std::net::TcpListener,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    app: Router,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    if let Some(tls) = tls {
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            wait_for_shutdown(shutdown).await;
            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(30)));
        });
        return axum_server::from_tcp_rustls(listener, tls)
            .context("failed to start the HTTPS listener")?
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .context("atmux HTTPS server failed");
    }
    let listener = tokio::net::TcpListener::from_std(listener)
        .context("failed to activate the loopback web listener")?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown))
    .await
    .context("atmux web server failed")
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let _ = shutdown.changed().await;
}

fn routes(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/atmux-logo.jpg", get(logo))
        .route("/api/v1/health", get(health))
        .route("/api/v1/machines", get(machines))
        .route(
            "/api/v1/machines/{id}/quick-resume",
            get(quick_resume_status).post(start_quick_resume),
        )
        .route("/api/v1/sessions", get(sessions).post(launch))
        .route("/api/v1/events", get(overview_events))
        .route("/api/v1/launch-options", get(launch_options))
        .route("/api/v1/launch-directories", get(launch_directories))
        .route("/api/v1/launch-sessions", get(launch_sessions))
        .route("/api/v1/panes/{id}", get(pane_output))
        .route("/api/v1/panes/{id}/transcript", get(pane_transcript))
        .route(
            "/api/v1/panes/{id}/files",
            get(pane_files)
                .put(write_pane_file)
                .layer(DefaultBodyLimit::max(MAX_FILE_WRITE_REQUEST_BYTES)),
        )
        .route("/api/v1/panes/{id}/git", get(pane_git))
        .route("/api/v1/panes/{id}/models", get(pane_models))
        .route("/api/v1/panes/{id}/model", post(switch_model))
        .route("/api/v1/panes/{id}/resume", post(resume_current_claude))
        .route("/api/v1/panes/{id}/events", get(pane_events))
        .route("/api/v1/panes/{id}/messages", post(send_message))
        .route(
            "/api/v1/panes/{id}/image-messages",
            post(send_image_message)
                .layer(DefaultBodyLimit::max(MAX_ATTACHMENT_REQUEST_BODY_BYTES)),
        )
        .route("/api/v1/panes/{id}/special-keys", post(send_special_keys))
        .route("/api/v1/panes/{id}/interrupt", post(interrupt))
        .route("/api/v1/sessions/{id}", delete(kill))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

/// Builds the dashboard and JSON API routes without the host and token policy
/// layer.
///
/// Exposed so tests can drive the real routes — and therefore the real status
/// mapping — instead of a stand-in router.
pub fn api_router(
    control: ControlPlane,
    allowed_origins: Vec<String>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    routes(WebState {
        control,
        allowed_origins: allowed_origins.into(),
        shutdown,
    })
}

async fn shutdown_signal(shutdown: watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    shutdown.send_replace(true);
}

async fn enforce_request_policy(
    State(policy): State<RequestPolicy>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if let Err(error) = ensure_host(request.headers(), &policy.allowed_hosts) {
        return error.into_response();
    }
    let sensitive =
        request.uri().path().starts_with("/api/") || request.uri().path().starts_with("/mcp");
    let development_loopback = peer.ip().is_loopback() && policy.allow_unauthenticated_loopback;
    if sensitive
        && !development_loopback
        && let Err(error) = ensure_node_token(request.headers(), &policy.tokens, peer)
    {
        return error.into_response();
    }
    let mut response = next.run(request).await;
    if sensitive {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

async fn index() -> Response {
    static_asset(INDEX_HTML, "text/html; charset=utf-8")
}

async fn css() -> Response {
    static_asset(APP_CSS, "text/css; charset=utf-8")
}

async fn js() -> Response {
    static_asset(APP_JS, "text/javascript; charset=utf-8")
}

async fn logo() -> Response {
    static_binary_asset(ATMUX_LOGO_JPG, "image/jpeg")
}

fn static_asset(body: &'static str, content_type: &'static str) -> Response {
    static_binary_asset(body.as_bytes(), content_type)
}

fn static_binary_asset(body: &'static [u8], content_type: &'static str) -> Response {
    let mut response = Body::from(body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn health(State(state): State<WebState>) -> Json<HealthResponse> {
    Json(state.control.overview().into())
}

async fn sessions(State(state): State<WebState>) -> Json<Overview> {
    Json(state.control.overview())
}

async fn launch_options(State(state): State<WebState>) -> impl IntoResponse {
    Json(state.control.launch_options())
}

async fn launch_directories(
    State(state): State<WebState>,
    Query(query): Query<LaunchDirectoryQuery>,
) -> Result<Json<LaunchDirectoryListing>, ApiError> {
    state
        .control
        .browse_launch_directories(query.machine.as_deref(), query.path.as_deref())
        .await
        .map(Json)
        .map_err(|error| ApiError::from_control(&error))
}

async fn launch_sessions(
    State(state): State<WebState>,
    Query(query): Query<LaunchSessionsQuery>,
) -> Result<Json<ResumableLaunchSessions>, ApiError> {
    state
        .control
        .resumable_launch_sessions(
            query.machine.as_deref(),
            &query.directory,
            &query.profile_id,
        )
        .await
        .map(Json)
        .map_err(|error| ApiError::from_control(&error))
}

async fn pane_output(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Query(query): Query<OutputQuery>,
) -> Result<Json<PaneOutput>, ApiError> {
    state
        .control
        .pane_output(&id, query.known_hash.as_deref(), query.lines.unwrap_or(160))
        .await
        .map_err(|error| ApiError::from_control(&error))?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no agent pane matches {id}")))
}

async fn pane_transcript(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<Transcript>, ApiError> {
    state
        .control
        .transcript(&id, query.known_hash.as_deref())
        .await
        .map_err(|error| ApiError::from_control(&error))?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no agent pane matches {id}")))
}

async fn pane_files(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Query(query): Query<RelativePathQuery>,
) -> Result<Json<FilesResponse>, ApiError> {
    state
        .control
        .pane_files(&id, query.path.as_deref())
        .await
        .map_err(|error| ApiError::from_control(&error))?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no agent pane matches {id}")))
}

async fn write_pane_file(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<FileWriteRequest>,
) -> Result<Json<FilesResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .write_pane_file(&id, request)
        .await
        .map_err(|error| ApiError::from_control(&error))?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no agent pane matches {id}")))
}

async fn pane_git(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Query(query): Query<RelativePathQuery>,
) -> Result<Json<GitResponse>, ApiError> {
    state
        .control
        .pane_git(&id, query.path.as_deref())
        .await
        .map_err(|error| ApiError::from_control(&error))?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no agent pane matches {id}")))
}

async fn pane_models(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<PaneModels>, ApiError> {
    state
        .control
        .pane_models(&id)
        .await
        .map_err(|error| ApiError::from_control(&error))?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no agent pane matches {id}")))
}

async fn switch_model(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ModelSwitchRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .switch_model(&id, request)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok(Json(OkResponse { ok: true }))
}

async fn resume_current_claude(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(_request): Json<ResumeCurrentClaudeRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .resume_current_claude(&id)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok(Json(OkResponse { ok: true }))
}

async fn machines(State(state): State<WebState>) -> Json<Vec<MachineSummary>> {
    Json(state.control.machines())
}

async fn quick_resume_status(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<RecoveryStatus>, ApiError> {
    state
        .control
        .recovery_status(&id)
        .await
        .map(Json)
        .map_err(|error| ApiError::from_control(&error))
}

async fn start_quick_resume(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(_request): Json<QuickResumeRequest>,
) -> Result<(StatusCode, Json<RecoveryStatus>), ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .start_recovery(&id)
        .await
        .map(|status| (StatusCode::ACCEPTED, Json(status)))
        .map_err(|error| ApiError::from_control(&error))
}

async fn send_message(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SendRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .send_text(&id, request.text, request.submit)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok(Json(OkResponse { ok: true }))
}

async fn send_image_message(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ImageMessageRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .send_image_message(&id, request)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok(Json(OkResponse { ok: true }))
}

async fn send_special_keys(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SpecialKeyRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    if request.action != "tmux_prefix_twice" {
        return Err(ApiError::bad_request("unsupported special key action"));
    }
    state
        .control
        .tmux_prefix_twice(&id)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok(Json(OkResponse { ok: true }))
}

async fn interrupt(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<OkResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .interrupt(&id)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok(Json(OkResponse { ok: true }))
}

async fn kill(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<OkResponse>, ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .kill(&id)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok(Json(OkResponse { ok: true }))
}

async fn launch(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(request): Json<LaunchRequest>,
) -> Result<(StatusCode, Json<OkResponse>), ApiError> {
    ensure_origin(&headers, &state.allowed_origins)?;
    state
        .control
        .launch(request)
        .await
        .map_err(|error| ApiError::from_control(&error))?;
    Ok((StatusCode::CREATED, Json(OkResponse { ok: true })))
}

async fn overview_events(
    State(state): State<WebState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let control = state.control;
    let mut receiver = control.subscribe();
    let mut shutdown = state.shutdown;
    let output = stream! {
        let mut previous = control.overview();
        yield Ok(event("sessions.snapshot", previous.revision, &previous));
        while wait_for_revision_or_shutdown(&mut receiver, &mut shutdown).await {
            let current = control.overview();
            let patch = overview_patch(&previous, &current);
            if !patch.upsert.is_empty()
                || !patch.remove.is_empty()
                || patch.health != previous.health
                || patch.machines != previous.machines
            {
                yield Ok(event("sessions.patch", current.revision, &patch));
                // `previous` is the last state the client actually has, so a
                // suppressed revision must not advance it. Otherwise the next
                // patch would carry a base_revision the client never saw and
                // every client would resynchronize for nothing.
                previous = current;
            }
        }
    };
    Sse::new(output).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(25))
            .text("keep-alive"),
    )
}

async fn pane_events(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let control = state.control;
    let mut receiver = control.subscribe();
    let mut shutdown = state.shutdown;
    let output = stream! {
        // A pane whose machine is offline is not gone: report why, stay open,
        // and deliver a snapshot as soon as the machine answers again.
        let mut reported: Option<String> = None;
        let mut previous = loop {
            match control.pane_output(&id, None, 2_000).await {
                Ok(Some(output)) => break output,
                Ok(None) => {
                    yield Ok(Event::default().event("pane.removed").data("{}"));
                    return;
                }
                Err(error) => {
                    if let Some(event) = pane_error_event(&error, &mut reported) {
                        yield Ok(event);
                    }
                    if !wait_for_revision_or_shutdown(&mut receiver, &mut shutdown).await {
                        return;
                    }
                }
            }
        };
        reported = None;
        yield Ok(event("pane.snapshot", previous.revision, &previous));
        while wait_for_revision_or_shutdown(&mut receiver, &mut shutdown).await {
            // A revision bump on any machine wakes every pane stream. Skip the
            // owning machine entirely unless this pane's advertised hash moved,
            // so remote traffic never scales with connected browsers.
            if !control.pane_may_have_changed(&id, &previous.content_hash) {
                continue;
            }
            let current = match control.pane_output(&id, Some(&previous.content_hash), 2_000).await {
                Ok(Some(current)) => current,
                Ok(None) => {
                    yield Ok(Event::default().event("pane.removed").data("{}"));
                    return;
                }
                Err(error) => {
                    // An outage on the owning machine is not a local tmux
                    // fault and must not be re-announced on every unrelated
                    // revision, so it is reported once per distinct reason.
                    if let Some(event) = pane_error_event(&error, &mut reported) {
                        yield Ok(event);
                    }
                    continue;
                }
            };
            reported = None;
            if current.changed {
                let old_content = previous.content.as_deref().unwrap_or_default();
                let new_content = current.content.as_deref().unwrap_or_default();
                let patch = pane_patch(&previous, &current, old_content, new_content);
                let patch_json = serde_json::to_string(&patch).unwrap_or_default();
                let snapshot_json = serde_json::to_string(&current).unwrap_or_default();
                if patch_json.len() < snapshot_json.len() {
                    yield Ok(Event::default()
                        .event("pane.patch")
                        .id(current.revision.to_string())
                        .data(patch_json));
                } else {
                    yield Ok(Event::default()
                        .event("pane.snapshot")
                        .id(current.revision.to_string())
                        .data(snapshot_json));
                }
                previous = current;
            }
        }
    };
    Sse::new(output).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(25))
            .text("keep-alive"),
    )
}

/// Builds the `pane.error` event for a failed pane read, or `None` when the
/// same reason was already reported.
///
/// A pane stream is woken by every machine's revision, so an offline machine
/// would otherwise emit one error per unrelated change. Deduplicating by reason
/// keeps the surface to one event per outage while still reporting a change of
/// cause.
fn pane_error_event(error: &anyhow::Error, reported: &mut Option<String>) -> Option<Event> {
    let message = error.to_string();
    if reported.as_deref() == Some(message.as_str()) {
        return None;
    }
    let kind = match error_kind(error) {
        ErrorKind::Offline => "offline",
        ErrorKind::Upstream => "upstream",
        ErrorKind::BadRequest | ErrorKind::NotFound | ErrorKind::Conflict => "request",
        ErrorKind::Internal => "internal",
    };
    reported.replace(message.clone());
    Some(
        Event::default()
            .event("pane.error")
            .json_data(serde_json::json!({ "error": message, "kind": kind }))
            .unwrap_or_else(|_| Event::default().event("pane.error").data(message)),
    )
}

async fn wait_for_revision_or_shutdown(
    revisions: &mut watch::Receiver<u64>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        result = revisions.changed() => result.is_ok(),
        result = shutdown.changed() => result.is_ok() && !*shutdown.borrow(),
    }
}

fn event<T: Serialize>(kind: &'static str, revision: u64, value: &T) -> Event {
    Event::default()
        .event(kind)
        .id(revision.to_string())
        .json_data(value)
        .unwrap_or_else(|_| {
            Event::default()
                .event("protocol.error")
                .data("serialization failed")
        })
}

fn ensure_origin(headers: &HeaderMap, allowed: &[String]) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let origin = origin
        .to_str()
        .map_err(|_| ApiError::bad_request("invalid Origin header"))?;
    if allowed.iter().any(|candidate| candidate == origin) {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "cross-origin control request rejected".to_owned(),
        })
    }
}

/// Requires a configured node or proxy token from every protected caller.
///
/// An unauthenticated loopback caller is accepted only through the explicit
/// development-mode opt-in. Static assets remain public regardless.
fn ensure_node_token(
    headers: &HeaderMap,
    expected: &[Secret],
    _peer: SocketAddr,
) -> Result<(), ApiError> {
    if expected.is_empty() {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "a node or proxy token is required for API access".to_owned(),
        });
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_credential)
        .unwrap_or_default();
    if expected
        .iter()
        .any(|token| constant_time_eq(presented.as_bytes(), token.expose().as_bytes()))
    {
        return Ok(());
    }
    Err(ApiError {
        status: StatusCode::UNAUTHORIZED,
        message: "a node or proxy token is required for API access".to_owned(),
    })
}

/// Extracts the credential from an `Authorization` header.
///
/// RFC 9110 defines the authentication scheme as case-insensitive, so clients
/// and proxies that send `bearer` or `BEARER` are accepted.
fn bearer_credential(value: &str) -> Option<&str> {
    let (scheme, credential) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| credential.trim_start())
}

/// Compares two credentials without leaking *where* they first differ.
///
/// The timing of this comparison does not depend on the contents, only on the
/// length. Unequal lengths are rejected immediately, which reveals nothing an
/// attacker cannot already learn: the configured token's length is fixed, and a
/// guess of the wrong length is wrong regardless.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn ensure_host(headers: &HeaderMap, allowed: &[String]) -> Result<(), ApiError> {
    let host = headers
        .get(header::HOST)
        .ok_or_else(|| ApiError::bad_request("missing Host header"))?
        .to_str()
        .map_err(|_| ApiError::bad_request("invalid Host header"))?;
    if allowed
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(host))
    {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "unrecognized Host header rejected".to_owned(),
        })
    }
}

fn allowed_hosts(bind: SocketAddr, extra: Vec<String>) -> Vec<String> {
    let port = bind.port();
    let mut values = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        bind.to_string(),
    ];
    if bind.ip().is_unspecified() {
        for interface in if_addrs::get_if_addrs().unwrap_or_default() {
            let ip = interface.ip();
            if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
                continue;
            }
            let authority = match ip {
                IpAddr::V4(ip) => format!("{ip}:{port}"),
                IpAddr::V6(ip) => format!("[{ip}]:{port}"),
            };
            values.push(authority);
        }
    }
    extend_unique(&mut values, extra);
    values
}

fn allowed_origins(bind: SocketAddr, extra: Vec<String>) -> Vec<String> {
    let port = bind.port();
    let mut values = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => vec![
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ],
        IpAddr::V6(ip) if ip.is_unspecified() => vec![format!("http://localhost:{port}")],
        IpAddr::V6(ip) => vec![
            format!("http://[{ip}]:{port}"),
            format!("http://localhost:{port}"),
        ],
        IpAddr::V4(ip) => vec![
            format!("http://{ip}:{port}"),
            format!("http://localhost:{port}"),
        ],
    };
    extend_unique(&mut values, extra);
    values
}

fn extend_unique(values: &mut Vec<String>, extra: Vec<String>) {
    for value in extra {
        let value = value.trim();
        if !value.is_empty() && !values.iter().any(|candidate| candidate == value) {
            values.push(value.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::Request as HttpRequest, routing::get};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    fn pane(revision: u64, content: &str) -> PaneOutput {
        PaneOutput {
            revision,
            pane_id: "%1".to_owned(),
            session: "test".to_owned(),
            content_hash: revision.to_string(),
            content: Some(content.to_owned()),
            changed: true,
        }
    }

    #[test]
    fn line_patch_is_unicode_safe() {
        let old = pane(1, "one\n🍰 two\nthree");
        let new = pane(2, "one\n🍰 changed\nthree");
        let patch = pane_patch(
            &old,
            &new,
            old.content.as_deref().unwrap(),
            new.content.as_deref().unwrap(),
        );
        assert_eq!(patch.start_line, 1);
        assert_eq!(patch.delete_lines, 1);
        assert_eq!(patch.lines, vec!["🍰 changed"]);
    }

    #[test]
    fn host_policy_accepts_allowlist_and_rejects_rebinding() {
        let allowed = allowed_hosts("127.0.0.1:7345".parse().unwrap(), Vec::new());
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost:7345"));
        assert!(ensure_host(&headers, &allowed).is_ok());

        headers.insert(
            header::HOST,
            HeaderValue::from_static("attacker.example:7345"),
        );
        let error = ensure_host(&headers, &allowed).unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);

        headers.remove(header::HOST);
        let error = ensure_host(&headers, &allowed).unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn remote_listener_keeps_the_same_port_for_a_local_http_ui() {
        let addresses = listener_addresses("192.0.2.19:7345".parse().unwrap()).unwrap();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&"192.0.2.19:7345".parse().unwrap()));
        assert!(addresses.contains(&"127.0.0.1:7345".parse().unwrap()));
    }

    #[test]
    fn loopback_bind_does_not_create_a_second_listener() {
        let bind = "127.0.0.1:7345".parse().unwrap();
        assert_eq!(listener_addresses(bind).unwrap(), vec![bind]);
    }

    #[test]
    fn explicit_remote_host_and_origin_are_allowlisted() {
        let bind = "0.0.0.0:7345".parse().unwrap();
        let hosts = allowed_hosts(bind, vec!["tron.example.ts.net:7345".to_owned()]);
        let origins = allowed_origins(bind, vec!["https://tron.example.ts.net:7345".to_owned()]);
        assert!(hosts.iter().any(|host| host == "tron.example.ts.net:7345"));
        assert!(
            origins
                .iter()
                .any(|origin| origin == "https://tron.example.ts.net:7345")
        );
    }

    #[test]
    fn origin_policy_rejects_cross_origin_mutation() {
        let allowed = vec!["http://localhost:7345".to_owned()];
        let mut headers = HeaderMap::new();
        assert!(ensure_origin(&headers, &allowed).is_ok());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        let error = ensure_origin(&headers, &allowed).unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
    }

    fn probe_app(node_token: Option<Secret>) -> Router {
        probe_app_with_loopback_policy(node_token, false)
    }

    fn probe_app_with_loopback_policy(
        node_token: Option<Secret>,
        allow_unauthenticated_loopback: bool,
    ) -> Router {
        let policy = RequestPolicy {
            allowed_hosts: vec!["localhost:7345".to_owned()].into(),
            tokens: node_token.into_iter().collect(),
            allow_unauthenticated_loopback,
        };
        Router::new()
            .route("/api/probe", get(|| async { "ok" }))
            .route("/api/v1/pulse/probe", get(|| async { "ok" }))
            .route("/public", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                policy,
                enforce_request_policy,
            ))
    }

    fn probe(uri: &str, host: &str, peer: &str, token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .uri(uri)
            .header(header::HOST, host)
            .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn raw_http_status(
        address: SocketAddr,
        method: &str,
        path: &str,
        token: Option<&str>,
    ) -> StatusCode {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let authorization = token.map_or_else(String::new, |token| {
            format!("Authorization: Bearer {token}\r\n")
        });
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{authorization}Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let status = String::from_utf8_lossy(&response)
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap();
        StatusCode::from_u16(status).unwrap()
    }

    #[tokio::test]
    async fn policy_middleware_protects_reads_and_disables_api_caching() {
        // This test isolates Host/cache behavior; the explicit development
        // opt-in keeps authentication from masking those assertions.
        let app = probe_app_with_loopback_policy(None, true);

        let rejected = app
            .clone()
            .oneshot(probe(
                "/api/probe",
                "attacker.example:7345",
                "127.0.0.1:5000",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let accepted = app
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "127.0.0.1:5000",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(accepted.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn forwarded_email_never_bypasses_pulse_authentication() {
        let app = probe_app(Some(Secret::new("pulse-node-token")));
        let request = HttpRequest::builder()
            .uri("/api/v1/pulse/probe")
            .header(header::HOST, "localhost:7345")
            .header("x-auth-request-email", "operator@example.test")
            .extension(ConnectInfo(
                "100.64.0.9:41000".parse::<SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[cfg(feature = "pulse")]
    async fn authenticated_pulse_app() -> Router {
        use crate::pulse::{
            AccountId,
            api::{PulseApi, PulseCapabilities},
            invalidation::PulseInvalidationHub,
            store::SqliteStore,
        };

        let account = AccountId::new(1).unwrap();
        let store = Arc::new(SqliteStore::open(":memory:").await.unwrap());
        let api = PulseApi::new(
            store,
            &[account],
            PulseCapabilities {
                collect: false,
                serve: true,
                receive: false,
            },
        )
        .with_invalidations(PulseInvalidationHub::new(&[account]));
        let origins: Arc<[String]> = vec!["https://atmux.example.test".to_owned()].into();
        let policy = RequestPolicy {
            allowed_hosts: vec!["atmux.example.test".to_owned()].into(),
            tokens: vec![Secret::new("outer-pulse-token")].into(),
            allow_unauthenticated_loopback: false,
        };
        pulse_routes(api, None, None, origins).layer(middleware::from_fn_with_state(
            policy,
            enforce_request_policy,
        ))
    }

    #[cfg(feature = "pulse")]
    fn pulse_request(method: &str, path: &str, token: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "atmux.example.test")
            .extension(ConnectInfo(
                "100.64.0.9:41000".parse::<SocketAddr>().unwrap(),
            ));
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    #[cfg(feature = "pulse")]
    #[tokio::test]
    async fn real_pulse_router_enforces_auth_host_and_mutation_origin_before_sse_or_actions() {
        let app = authenticated_pulse_app().await;
        let anonymous = app
            .clone()
            .oneshot(pulse_request(
                "GET",
                "/api/v1/pulse/accounts/1/events",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let wrong_host = HttpRequest::builder()
            .uri("/api/v1/pulse/accounts/1/events")
            .header(header::HOST, "attacker.example")
            .header(header::AUTHORIZATION, "Bearer outer-pulse-token")
            .extension(ConnectInfo(
                "100.64.0.9:41000".parse::<SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(wrong_host).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let authenticated = app
            .clone()
            .oneshot(pulse_request(
                "GET",
                "/api/v1/pulse/accounts/1/events",
                Some("outer-pulse-token"),
            ))
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::OK);
        assert_eq!(authenticated.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            authenticated.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );

        let mut cross_origin = pulse_request(
            "POST",
            "/api/v1/pulse/accounts/1/poll",
            Some("outer-pulse-token"),
        );
        cross_origin.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert_eq!(
            app.clone().oneshot(cross_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let mut same_origin = pulse_request(
            "POST",
            "/api/v1/pulse/accounts/1/poll",
            Some("outer-pulse-token"),
        );
        same_origin.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://atmux.example.test"),
        );
        assert_eq!(
            app.oneshot(same_origin).await.unwrap().status(),
            StatusCode::CONFLICT,
            "an allowed origin reaches the Pulse handler instead of being rejected at the boundary"
        );
    }

    #[cfg(feature = "pulse")]
    #[test]
    fn remote_pulse_ingest_requires_distinct_headers_without_fallback() {
        let remote = "100.64.0.9".parse::<IpAddr>().expect("remote ip");
        let expected = [Secret::new("outer-node-token")];
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer outer-node-token"),
        );
        headers.insert(
            "x-atmux-pulse-token",
            HeaderValue::from_static("separate-ingest-token"),
        );
        assert!(ensure_node_token(&headers, &expected, SocketAddr::new(remote, 41000)).is_ok());
        assert_eq!(
            pulse_ingest_bearer(&headers, remote),
            "separate-ingest-token"
        );

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer separate-ingest-token"),
        );
        headers.insert(
            "x-atmux-pulse-token",
            HeaderValue::from_static("outer-node-token"),
        );
        assert!(ensure_node_token(&headers, &expected, SocketAddr::new(remote, 41000)).is_err());

        headers.remove("x-atmux-pulse-token");
        assert_eq!(pulse_ingest_bearer(&headers, remote), "");
        assert!(ensure_node_token(&headers, &expected, SocketAddr::new(remote, 41000)).is_err());

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer outer-node-token"),
        );
        assert!(ensure_node_token(&headers, &expected, SocketAddr::new(remote, 41000)).is_ok());
        assert_eq!(pulse_ingest_bearer(&headers, remote), "");
    }

    #[cfg(feature = "pulse")]
    #[test]
    fn loopback_pulse_ingest_may_use_authorization_bearer() {
        let loopback = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer loopback-ingest-token"),
        );
        assert_eq!(
            pulse_ingest_bearer(&headers, loopback),
            "loopback-ingest-token"
        );
    }

    #[cfg(feature = "pulse")]
    #[tokio::test]
    async fn every_pulse_mutation_uses_the_existing_origin_policy() {
        let allowed: Arc<[String]> = vec!["https://atmux.example.test".to_owned()].into();
        let app = Router::new()
            .route(
                "/api/v1/pulse/probe",
                axum::routing::patch(|| async { "ok" }),
            )
            .layer(middleware::from_fn_with_state(
                allowed,
                enforce_mutation_origin,
            ));
        let rejected = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/pulse/probe")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let accepted = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/pulse/probe")
                    .header(header::ORIGIN, "https://atmux.example.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn node_token_gates_remote_and_loopback_callers() {
        let app = probe_app(Some(Secret::new("federation-token")));

        // A coordinator on the tailnet must present the bearer token.
        let anonymous = app
            .clone()
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "100.64.0.7:41000",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let wrong = app
            .clone()
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "100.64.0.7:41000",
                Some("guessed"),
            ))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let authorized = app
            .clone()
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "100.64.0.7:41000",
                Some("federation-token"),
            ))
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);

        // Loopback is not an ambient authority boundary: every local process
        // must authenticate when the development escape hatch is disabled.
        let loopback = app
            .clone()
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "127.0.0.1:5000",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(loopback.status(), StatusCode::UNAUTHORIZED);

        let authorized_loopback = app
            .clone()
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "127.0.0.1:5000",
                Some("federation-token"),
            ))
            .await
            .unwrap();
        assert_eq!(authorized_loopback.status(), StatusCode::OK);

        // Static assets are not gated, so the node still serves its own UI.
        let asset = app
            .oneshot(probe("/public", "localhost:7345", "100.64.0.7:41000", None))
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unauthenticated_loopback_requires_an_explicit_development_opt_in() {
        let secure = probe_app_with_loopback_policy(None, false)
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "127.0.0.1:5000",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(secure.status(), StatusCode::UNAUTHORIZED);

        let development = probe_app_with_loopback_policy(None, true)
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "127.0.0.1:5000",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(development.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn real_loopback_listener_rejects_anonymous_reads_and_mutations_before_routing() {
        let control = crate::control::test_control(&[]);
        let (app, route_shutdown) = real_app(control);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let policy = RequestPolicy {
            allowed_hosts: vec![address.to_string()].into(),
            tokens: vec![Secret::new("local-test-token")].into(),
            allow_unauthenticated_loopback: false,
        };
        let app = app
            .route("/mcp", post(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn_with_state(
                policy,
                enforce_request_policy,
            ));
        let (listener_shutdown, listener_shutdown_rx) = watch::channel(false);
        let server = tokio::spawn(serve_listener(listener, None, app, listener_shutdown_rx));

        for (method, path) in [
            ("GET", "/api/v1/sessions"),
            ("GET", "/api/v1/panes/nope"),
            ("GET", "/api/v1/panes/nope/transcript"),
            ("GET", "/api/v1/panes/nope/events"),
            ("POST", "/api/v1/panes/nope/messages"),
            ("DELETE", "/api/v1/sessions/nope"),
            ("POST", "/mcp"),
        ] {
            assert_eq!(
                raw_http_status(address, method, path, None).await,
                StatusCode::UNAUTHORIZED,
                "{method} {path} must be rejected before route resolution"
            );
        }
        assert_eq!(
            raw_http_status(address, "GET", "/api/v1/sessions", Some("local-test-token"),).await,
            StatusCode::OK
        );

        listener_shutdown.send_replace(true);
        server.await.unwrap().unwrap();
        route_shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn an_unconfigured_node_never_fails_open_for_a_remote_api_caller() {
        let app = probe_app(None);
        let response = app
            .oneshot(probe(
                "/api/probe",
                "localhost:7345",
                "192.168.1.44:41000",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn a_distinct_proxy_credential_is_accepted_without_replacing_the_node_token() {
        let tokens = [
            Secret::new("federation-token"),
            Secret::new("web-proxy-token"),
        ];
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer web-proxy-token"),
        );
        assert!(
            ensure_node_token(&headers, &tokens, "10.1.112.10:41000".parse().unwrap(),).is_ok()
        );
    }

    #[test]
    fn token_comparison_is_length_safe_and_content_independent() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_authentication_scheme_is_matched_case_insensitively() {
        for header in ["Bearer secret", "bearer secret", "BEARER secret"] {
            assert_eq!(
                bearer_credential(header),
                Some("secret"),
                "{header} must be accepted"
            );
        }
        assert_eq!(bearer_credential("Bearer  secret"), Some("secret"));
        assert_eq!(bearer_credential("Basic secret"), None);
        assert_eq!(bearer_credential("Bearer"), None);
        assert_eq!(bearer_credential("secret"), None);
    }

    #[tokio::test]
    async fn a_lowercase_bearer_scheme_authenticates_a_remote_coordinator() {
        let app = probe_app(Some(Secret::new("federation-token")));
        let request = HttpRequest::builder()
            .uri("/api/probe")
            .header(header::HOST, "localhost:7345")
            .header(header::AUTHORIZATION, "bearer federation-token")
            .extension(ConnectInfo(
                "100.64.0.7:41000".parse::<SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    /// Drives the real routes, so the statuses asserted here are the ones a
    /// browser or coordinator actually receives.
    fn real_app(control: ControlPlane) -> (Router, watch::Sender<bool>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let app = api_router(
            control,
            vec!["http://localhost:7345".to_owned()],
            shutdown_rx,
        );
        (app, shutdown_tx)
    }

    fn authenticated_real_app(control: ControlPlane) -> (Router, watch::Sender<bool>) {
        let (app, shutdown) = real_app(control);
        let policy = RequestPolicy {
            allowed_hosts: vec!["localhost:7345".to_owned()].into(),
            tokens: vec![Secret::new("quick-resume-test-token")].into(),
            allow_unauthenticated_loopback: false,
        };
        (
            app.layer(middleware::from_fn_with_state(
                policy,
                enforce_request_policy,
            )),
            shutdown,
        )
    }

    fn protected_api(
        method: &str,
        uri: &str,
        body: &str,
        token: Option<&str>,
        origin: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost:7345")
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(
                "100.64.0.9:41000".parse::<SocketAddr>().unwrap(),
            ));
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        builder.body(Body::from(body.to_owned())).unwrap()
    }

    fn api(method: &str, uri: &str, body: Option<&str>) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        builder
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_owned())))
            .unwrap()
    }

    async fn status_of(app: &Router, method: &str, uri: &str, body: Option<&str>) -> StatusCode {
        app.clone()
            .oneshot(api(method, uri, body))
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn quick_resume_is_owner_scoped_and_origin_protected_without_running_a_script() {
        let control = crate::control::test_control(&[]);
        let local_id = control.local_id().to_owned();
        let (app, _shutdown) = real_app(control);
        let path = format!("/api/v1/machines/{local_id}/quick-resume");

        let status = app.clone().oneshot(api("GET", &path, None)).await.unwrap();
        assert_eq!(status.status(), StatusCode::OK);

        let cross_origin = HttpRequest::builder()
            .method("POST")
            .uri(&path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "https://attacker.example")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(cross_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let same_origin = HttpRequest::builder()
            .method("POST")
            .uri(&path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "http://localhost:7345")
            .body(Body::from("{}"))
            .unwrap();
        // The test node is intentionally not Tron, so the fixed runner fails
        // closed after Origin validation and no process is spawned.
        assert_eq!(
            app.oneshot(same_origin).await.unwrap().status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn outer_router_protects_quick_resume_schema_origin_and_owner_without_spawning() {
        // `test_control` identifies as `local`, not Tron, so its fixed recovery
        // runner fails closed before process creation. The configured remote is
        // offline, which also proves that its owner route is not run locally.
        let control = crate::control::test_control(&["gpu-box"]);
        let (app, _shutdown) = authenticated_real_app(control);
        let local = "/api/v1/machines/local/quick-resume";

        let anonymous = protected_api("POST", local, "{}", None, Some("http://localhost:7345"));
        assert_eq!(
            app.clone().oneshot(anonymous).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let wrong_origin = protected_api(
            "POST",
            local,
            "{}",
            Some("quick-resume-test-token"),
            Some("https://attacker.example"),
        );
        assert_eq!(
            app.clone().oneshot(wrong_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let supplied_path = protected_api(
            "POST",
            local,
            r#"{"path":"/tmp/operator-selected.sh"}"#,
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.clone().oneshot(supplied_path).await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let wrong_owner = protected_api(
            "POST",
            "/api/v1/machines/ghost/quick-resume",
            "{}",
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.clone().oneshot(wrong_owner).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let remote_owner = protected_api(
            "POST",
            "/api/v1/machines/gpu-box/quick-resume",
            "{}",
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.clone().oneshot(remote_owner).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        let local_unavailable = protected_api(
            "POST",
            local,
            "{}",
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.oneshot(local_unavailable).await.unwrap().status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn failures_map_onto_statuses_that_say_whose_problem_they_are() {
        let control = crate::control::test_control(&["gpu-box"]);
        control.apply_refresh(vec![crate::control::test_session(
            "agent",
            "%4294967295",
            "hello",
        )]);
        control.mark_machine_offline("gpu-box", "connection refused");
        let (app, _shutdown) = real_app(control);

        // A caller error is the caller's, on reads and on every mutation.
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/nope", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/nope/transcript", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/nope/files", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/nope/git", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/nope/files?root=%2Fetc", None,).await,
            StatusCode::BAD_REQUEST,
            "a browser cannot supply a filesystem root"
        );
        assert_eq!(
            status_of(&app, "POST", "/api/v1/panes/nope/files", Some("{}")).await,
            StatusCode::METHOD_NOT_ALLOWED,
            "workspace routes are read-only"
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/%254294967295/transcript", None,).await,
            StatusCode::OK
        );
        assert_eq!(
            status_of(
                &app,
                "GET",
                "/api/v1/panes/%254294967295/files?path=%2Fetc%2Fpasswd",
                None,
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "GET",
                "/api/v1/panes/%254294967295/git?path=..%2Fsecret",
                None,
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/nope/messages",
                Some(r#"{"text":"hi"}"#)
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, "POST", "/api/v1/panes/nope/interrupt", Some("{}")).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, "DELETE", "/api/v1/sessions/nope", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/sessions",
                Some(r#"{"name":"bad name","directory":"/srv","profile_id":"profile-0"}"#)
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/sessions",
                Some(
                    r#"{"name":"x","directory":"/srv","profile_id":"profile-0","machine":"ghost"}"#
                )
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        // A name that already exists conflicts rather than being malformed.
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/sessions",
                Some(r#"{"name":"agent","directory":"/srv","profile_id":"profile-0"}"#)
            )
            .await,
            StatusCode::CONFLICT
        );

        // An offline machine is unavailable, not a bad request, on every route.
        for (method, uri, body) in [
            ("GET", "/api/v1/panes/gpu-box~%251", None),
            ("GET", "/api/v1/panes/gpu-box~%251/files", None),
            ("GET", "/api/v1/panes/gpu-box~%251/git", None),
            (
                "POST",
                "/api/v1/panes/gpu-box~%251/messages",
                Some(r#"{"text":"hi"}"#),
            ),
            (
                "POST",
                "/api/v1/panes/gpu-box~%251/special-keys",
                Some(r#"{"action":"tmux_prefix_twice"}"#),
            ),
            ("POST", "/api/v1/panes/gpu-box~%251/interrupt", Some("{}")),
            ("DELETE", "/api/v1/sessions/gpu-box~%251", None),
            (
                "POST",
                "/api/v1/sessions",
                Some(
                    r#"{"name":"x","directory":"/srv","profile_id":"profile-0","machine":"gpu-box"}"#,
                ),
            ),
        ] {
            assert_eq!(
                status_of(&app, method, uri, body).await,
                StatusCode::SERVICE_UNAVAILABLE,
                "{method} {uri}"
            );
        }

        // A healthy local pane still reads normally.
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/%254294967295", None).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn file_update_route_is_owner_scoped_hash_guarded_and_no_store() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("atmux-web-edit-{nonce}"));
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("main.rs"), "fn old() {}\n").unwrap();
        std::fs::write(project.join("max.txt"), "seed\n").unwrap();
        let mut config = Config::default();
        config.general.project_roots = vec![base.clone()];
        config.general.favorite_dirs.clear();
        let control = crate::control::test_control_with_config(&[], config);
        let mut session = crate::control::test_session("agent", "%4294967295", "hello");
        session.path = project.clone();
        control.apply_refresh(vec![session]);
        let (app, _shutdown) = authenticated_real_app(control);
        let uri = "/api/v1/panes/%254294967295/files";

        let read = protected_api(
            "GET",
            &format!("{uri}?path=main.rs"),
            "",
            Some("quick-resume-test-token"),
            None,
        );
        let response = app.clone().oneshot(read).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let FilesResponse::File { content_hash, .. } =
            serde_json::from_slice::<FilesResponse>(&body).unwrap()
        else {
            panic!("expected file")
        };
        let expected_hash = content_hash.unwrap();
        let update = serde_json::to_string(&FileWriteRequest {
            path: "main.rs".to_owned(),
            content: "fn new() {}\n".to_owned(),
            expected_hash: expected_hash.clone(),
        })
        .unwrap();
        let request = protected_api(
            "PUT",
            uri,
            &update,
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            std::fs::read_to_string(project.join("main.rs")).unwrap(),
            "fn new() {}\n"
        );

        let stale = protected_api(
            "PUT",
            uri,
            &update,
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.clone().oneshot(stale).await.unwrap().status(),
            StatusCode::CONFLICT
        );

        // The route body limit includes JSON's worst accepted two-byte
        // escaping overhead, so an exact 256 KiB text file remains writable.
        let read_max = protected_api(
            "GET",
            &format!("{uri}?path=max.txt"),
            "",
            Some("quick-resume-test-token"),
            None,
        );
        let response = app.clone().oneshot(read_max).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let FilesResponse::File {
            content_hash: max_hash,
            ..
        } = serde_json::from_slice::<FilesResponse>(&body).unwrap()
        else {
            panic!("expected file")
        };
        let max_content = "\t".repeat(256 * 1024);
        let max_update = serde_json::to_string(&FileWriteRequest {
            path: "max.txt".to_owned(),
            content: max_content,
            expected_hash: max_hash.unwrap(),
        })
        .unwrap();
        assert!(max_update.len() > MAX_REQUEST_BODY_BYTES);
        assert!(max_update.len() <= MAX_FILE_WRITE_REQUEST_BYTES);
        let request = protected_api(
            "PUT",
            uri,
            &max_update,
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            std::fs::metadata(project.join("max.txt")).unwrap().len(),
            (256 * 1024) as u64
        );

        let smuggled_root = format!(
            r#"{{"path":"main.rs","content":"bad","expected_hash":"{expected_hash}","root":"/etc"}}"#
        );
        let request = protected_api(
            "PUT",
            uri,
            &smuggled_root,
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let oversized = serde_json::to_string(&FileWriteRequest {
            path: "main.rs".to_owned(),
            content: "x".repeat(MAX_FILE_WRITE_REQUEST_BYTES),
            expected_hash: expected_hash.clone(),
        })
        .unwrap();
        let request = protected_api(
            "PUT",
            uri,
            &oversized,
            Some("quick-resume-test-token"),
            Some("http://localhost:7345"),
        );
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let cross_origin = protected_api(
            "PUT",
            uri,
            &update,
            Some("quick-resume-test-token"),
            Some("https://attacker.example"),
        );
        assert_eq!(
            app.oneshot(cross_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn launch_directory_route_is_bounded_and_owner_aware() {
        let control = crate::control::test_control(&["gpu-box"]);
        control.mark_machine_offline("gpu-box", "connection refused");
        let (app, _shutdown) = real_app(control);

        assert_eq!(
            status_of(&app, "GET", "/api/v1/launch-directories", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/launch-directories?path=%2Fetc", None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/launch-sessions", None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "GET",
                "/api/v1/launch-sessions?directory=%2Fetc&profile_id=profile-0",
                None,
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "GET",
                "/api/v1/launch-directories?machine=gpu-box",
                None,
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status_of(
                &app,
                "GET",
                "/api/v1/launch-sessions?machine=gpu-box&directory=%2Fsrv&profile_id=profile-0",
                None,
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn model_routes_reject_bad_targets_and_report_offline_owners() {
        let control = crate::control::test_control(&["gpu-box"]);
        control.apply_refresh(vec![crate::control::test_session(
            "agent",
            "%4294967295",
            "OpenAI Codex (v0.147.0)\nmodel: gpt-5.6-sol xhigh",
        )]);
        control.mark_machine_offline("gpu-box", "connection refused");
        let (app, _shutdown) = real_app(control);

        let response = app
            .clone()
            .oneshot(api("GET", "/api/v1/panes/%254294967295/models", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let models: PaneModels = serde_json::from_slice(&body).unwrap();
        assert!(models.current.is_some());
        assert!(models.models.is_empty());
        assert!(
            models
                .note
                .as_deref()
                .unwrap()
                .contains("no selectable modes")
        );

        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/nope/models", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/%254294967295/model",
                Some(r#"{"mode_id":"gpt-5.6-sol;touch-pwned"}"#),
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/%254294967295/resume",
                Some("{}"),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "non-Claude panes cannot use the Claude resume route"
        );
        assert_eq!(
            status_of(&app, "GET", "/api/v1/panes/gpu-box~%251/models", None,).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/gpu-box~%251/model",
                Some(r#"{"mode_id":"sol-xhigh"}"#),
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/gpu-box~%251/resume",
                Some("{}"),
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn special_key_route_is_allowlisted() {
        let control = crate::control::test_control(&[]);
        control.apply_refresh(vec![crate::control::test_session(
            "agent",
            "%4294967295",
            "hello",
        )]);
        let (app, _shutdown) = real_app(control);

        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/%254294967295/special-keys",
                Some(r#"{"action":"anything_else"}"#),
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/nope/special-keys",
                Some(r#"{"action":"tmux_prefix_twice"}"#),
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn image_route_overrides_the_small_text_json_body_limit() {
        let control = crate::control::test_control(&[]);
        let (app, _shutdown) = real_app(control);
        let body = serde_json::json!({
            "text": "",
            "images": [{
                "media_type": "image/png",
                "data": "A".repeat(MAX_REQUEST_BODY_BYTES + 1),
            }],
        })
        .to_string();
        assert!(body.len() < MAX_ATTACHMENT_REQUEST_BODY_BYTES);
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/nope/image-messages",
                Some(&body)
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn image_route_classifies_unknown_invalid_and_offline_targets() {
        let control = crate::control::test_control(&["gpu-box"]);
        control.apply_refresh(vec![crate::control::test_session(
            "agent",
            "%4294967295",
            "hello",
        )]);
        control.mark_machine_offline("gpu-box", "connection refused");
        let (app, _shutdown) = real_app(control);
        let body = Some(r#"{"text":"look","images":[]}"#);

        assert_eq!(
            status_of(&app, "POST", "/api/v1/panes/nope/image-messages", body).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/%254294967295/image-messages",
                body,
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                "POST",
                "/api/v1/panes/gpu-box~%251/image-messages",
                body
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    async fn read_events(response: Response, limit: usize, budget: Duration) -> String {
        use http_body_util::BodyExt;

        let mut body = response.into_body();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + budget;
        while text.matches("event:").count() < limit {
            let Ok(Some(Ok(frame))) = tokio::time::timeout_at(deadline, body.frame()).await else {
                break;
            };
            if let Ok(chunk) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
        text
    }

    #[tokio::test]
    async fn an_offline_pane_stream_reports_the_outage_once_and_stays_open() {
        let control = crate::control::test_control(&["gpu-box"]);
        control.mark_machine_offline("gpu-box", "connection refused");
        let (app, _shutdown) = real_app(control.clone());

        let response = app
            .oneshot(api("GET", "/api/v1/panes/gpu-box~%254/events", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Unrelated local churn wakes every pane stream. The outage must not be
        // re-announced for each one, and must never be reported as local tmux
        // health, which is what `protocol.error` means to the dashboard.
        let churn = tokio::spawn(async move {
            for index in 0..5 {
                control.apply_refresh(vec![crate::control::test_session(
                    "agent",
                    "%1",
                    &format!("line {index}"),
                )]);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let text = read_events(response, 3, Duration::from_millis(400)).await;
        churn.await.unwrap();

        assert_eq!(
            text.matches("event: pane.error").count(),
            1,
            "the outage must be reported exactly once: {text}"
        );
        assert!(text.contains("connection refused"), "{text}");
        assert!(text.contains("\"kind\":\"offline\""), "{text}");
        assert!(
            !text.contains("protocol.error"),
            "a remote outage is not a local tmux fault: {text}"
        );
        assert!(
            !text.contains("pane.removed"),
            "an offline pane still exists: {text}"
        );
    }

    #[test]
    fn a_configured_token_is_never_rendered_into_a_response_or_log() {
        let policy = RequestPolicy {
            allowed_hosts: vec!["localhost:7345".to_owned()].into(),
            tokens: vec![Secret::new("federation-token")].into(),
            allow_unauthenticated_loopback: false,
        };
        let rendered = format!("{policy:?}");
        assert!(!rendered.contains("federation-token"), "{rendered}");

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer no"));
        let error = ensure_node_token(
            &headers,
            &policy.tokens,
            "100.64.0.7:41000".parse().unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        assert!(!error.message.contains("federation-token"));
    }

    #[tokio::test]
    async fn shutdown_wakes_revision_streams() {
        let (_revision_tx, mut revisions) = watch::channel(1_u64);
        let (shutdown_tx, mut shutdown) = watch::channel(false);
        shutdown_tx.send_replace(true);
        assert!(!wait_for_revision_or_shutdown(&mut revisions, &mut shutdown).await);
    }

    #[test]
    fn embedded_assets_are_revalidated() {
        let response = static_asset("body", "text/plain");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    }

    #[test]
    fn health_payload_does_not_duplicate_session_inventory() {
        let payload = HealthResponse::from(Overview {
            revision: 7,
            sessions: Vec::new(),
            health: Some("tmux unavailable".to_owned()),
            machines: Vec::new(),
        });
        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["revision"], 7);
        assert_eq!(json["health"], "tmux unavailable");
        assert!(json.get("sessions").is_none());
    }

    #[test]
    fn json_body_limit_allows_worst_case_escaped_message_at_input_limit() {
        let text = "\u{0001}".repeat(64 * 1024);
        let encoded = serde_json::to_vec(&serde_json::json!({
            "text": text,
            "submit": true,
        }))
        .unwrap();
        assert!(encoded.len() <= MAX_REQUEST_BODY_BYTES);
    }
}
