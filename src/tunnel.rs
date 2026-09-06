//! Phone-home tunnel: a node dials its coordinator and stays controllable.
//!
//! Federation normally runs the other way: the coordinator dials each
//! `[[machines]] url` over mutual TLS. A machine that only has outbound HTTPS —
//! a laptop away from home — has no address to dial, so it inverts the
//! direction. The node sends `GET /api/v1/tunnel/{machine}` with
//! `Upgrade: atmux-tunnel`; after `101` it runs an HTTP/2 **server** over the
//! raw upgraded stream, serving the very same axum app it serves on its own
//! listener. The coordinator keeps the multiplexing HTTP/2 client, so every
//! existing call path — the events stream, launches, pane reads, fleet updates
//! — works unchanged.
//!
//! Two properties carry the security of this design and are enforced here.
//!
//! First, the tunnel route is the one `/api/` path that does **not** accept the
//! web proxy token. `enforce_request_policy` accepts a node *or* proxy token
//! everywhere else, and the public gateway injects the proxy token on every
//! single-sign-on'd browser request. If that token could open a tunnel, any
//! authenticated browser could impersonate a machine. This module compares the
//! presented credential, in constant time, against exactly one secret: the
//! federation token configured for the named machine.
//!
//! Second, requests that arrive on a node through the tunnel are given a
//! synthetic *non-loopback* peer address. The node's `allow_unauthenticated_loopback`
//! development escape hatch keys off a loopback peer; a tunneled request that
//! looked local would bypass authentication entirely.

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{ConnectInfo, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use rustls::{RootCertStore, pki_types::ServerName};
use tokio::{net::TcpStream, sync::watch, task::AbortHandle};
use tokio_rustls::TlsConnector;

use crate::{
    config::{Config, CoordinatorConfig},
    machine::{NodeUrl, Secret, resolve_token},
};

/// Protocol name exchanged in the `Upgrade` header. It is deliberately not a
/// registered protocol: only atmux speaks it, at both ends.
pub const UPGRADE_PROTOCOL: &str = "atmux-tunnel";

/// Path prefix the coordinator serves. `enforce_request_policy` recognizes this
/// exact shape and hands authentication to this module instead of the shared
/// node/proxy token list.
pub const TUNNEL_PATH_PREFIX: &str = "/api/v1/tunnel/";

/// Reserved authority a coordinator addresses a tunneled node by.
///
/// HTTP/2 has no `Host` header; the authority travels in `:authority`, which
/// hyper turns back into the request URI. There is no real host at the other
/// end of the stream, so a reserved name keeps the node's Host allow-list
/// meaningful rather than disabling it for tunneled requests.
pub const TUNNEL_AUTHORITY: &str = "atmux.tunnel";

/// Synthetic peer recorded for every tunneled request on the node.
///
/// TEST-NET-1 (RFC 5737) is documentation-only address space: it is not
/// loopback, so the loopback development exemption can never apply, and it is
/// not routable, so it cannot be confused with a real peer in a log.
pub const TUNNEL_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 0);

/// HTTP/2 PING interval. Short enough to hold a connection open through a
/// proxy with a 60s idle timeout, and to notice a laptop that went to sleep.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Flow-control windows. The coordinator caps a federated response at 1 MiB,
/// so a 2 MiB stream window means a normal response never stalls waiting for a
/// window update round trip.
const STREAM_WINDOW: u32 = 2 * 1024 * 1024;
const CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a tunnel must carry traffic before the node treats the coordinator
/// as healthy and resets its reconnect backoff.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);

/// The coordinator's half of one live tunnel. Cloning it opens another
/// multiplexed HTTP/2 stream rather than another connection.
pub type TunnelSender = hyper::client::conn::http2::SendRequest<Full<Bytes>>;

/// Live tunnels, keyed by machine id.
///
/// Registration is last-wins: a node that reconnects after a network change
/// replaces its own stale entry and the previous connection driver is aborted,
/// so a half-dead socket cannot keep answering requests.
#[derive(Clone, Debug)]
pub struct TunnelRegistry {
    inner: Arc<Registry>,
}

#[derive(Debug)]
struct Registry {
    live: Mutex<BTreeMap<String, Live>>,
    /// Monotonic across the whole registry, so a late cleanup from a replaced
    /// connection can prove it is talking about the entry it created.
    next_generation: AtomicU64,
    /// Bumped on every registration so a machine watcher sleeping in its
    /// reconnect backoff wakes the moment its node dials in.
    changes: watch::Sender<u64>,
}

#[derive(Debug)]
struct Live {
    generation: u64,
    sender: TunnelSender,
    driver: AbortHandle,
}

impl Default for TunnelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelRegistry {
    #[must_use]
    pub fn new() -> Self {
        let (changes, _) = watch::channel(0);
        Self {
            inner: Arc::new(Registry {
                live: Mutex::new(BTreeMap::new()),
                next_generation: AtomicU64::new(1),
                changes,
            }),
        }
    }

    /// Publishes a freshly negotiated tunnel and returns its generation.
    ///
    /// Any tunnel this machine already held is dropped and its connection
    /// driver aborted before the new one is visible.
    pub fn register(&self, machine: &str, sender: TunnelSender, driver: AbortHandle) -> u64 {
        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        let replaced = self
            .inner
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                machine.to_owned(),
                Live {
                    generation,
                    sender,
                    driver,
                },
            );
        if let Some(replaced) = replaced {
            replaced.driver.abort();
        }
        self.inner.changes.send_replace(generation);
        generation
    }

    /// Retires one tunnel, but only if it is still the live one.
    ///
    /// A connection that ends after it was already replaced must not evict its
    /// successor, which is why the generation is checked rather than the id.
    pub fn disconnect(&self, machine: &str, generation: u64) {
        let mut live = self
            .inner
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if live
            .get(machine)
            .is_some_and(|entry| entry.generation == generation)
            && let Some(entry) = live.remove(machine)
        {
            entry.driver.abort();
        }
    }

    /// A sender for one machine, or `None` when no usable tunnel exists.
    #[must_use]
    pub fn sender(&self, machine: &str) -> Option<TunnelSender> {
        self.inner
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(machine)
            .map(|entry| entry.sender.clone())
            .filter(|sender| !sender.is_closed())
    }

    #[must_use]
    pub fn is_connected(&self, machine: &str) -> bool {
        self.sender(machine).is_some()
    }

    /// Resolves the next time any machine registers a tunnel.
    pub async fn changed(&self) {
        let mut receiver = self.inner.changes.subscribe();
        let _ = receiver.changed().await;
    }
}

/// The coordinator's tunnel credentials: one token per machine that is allowed
/// to dial in. A machine without `tunnel = true` is absent, so it can never
/// authenticate here no matter which token it presents.
pub struct TunnelAcceptor {
    registry: TunnelRegistry,
    machines: BTreeMap<String, Secret>,
}

impl std::fmt::Debug for TunnelAcceptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only the machine ids are printable; the values are federation
        // tokens, which must never reach a log through a derived `Debug`.
        formatter
            .debug_struct("TunnelAcceptor")
            .field("registry", &self.registry)
            .field("machines", &self.machines.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl TunnelAcceptor {
    /// Collects the federation tokens of every `tunnel = true` machine.
    ///
    /// Returns `None` when no machine dials in, so the route is not mounted at
    /// all rather than mounted and always failing.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured token cannot be read, or when a
    /// tunnel machine has no credential: an unauthenticated tunnel would let
    /// anyone on the internet impersonate that machine.
    pub fn from_config(config: &Config, registry: TunnelRegistry) -> Result<Option<Arc<Self>>> {
        let mut machines = BTreeMap::new();
        for machine in config.machines.iter().filter(|machine| machine.tunnel) {
            let token = resolve_token(
                &machine.id,
                machine.token_env.as_deref(),
                machine.token_file.as_deref(),
            )
            .with_context(|| format!("failed to resolve the token for machine {}", machine.id))?
            .with_context(|| {
                format!(
                    "machine {} sets tunnel = true without token_env or token_file; an inbound tunnel must be authenticated",
                    machine.id
                )
            })?;
            machines.insert(machine.id.clone(), token);
        }
        if machines.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(Self { registry, machines })))
    }

    /// Builds an acceptor directly, for tests and callers that already hold the
    /// per-machine credentials.
    #[must_use]
    pub fn new(registry: TunnelRegistry, machines: BTreeMap<String, Secret>) -> Arc<Self> {
        Arc::new(Self { registry, machines })
    }

    /// Decides whether one request may open a tunnel for one machine.
    ///
    /// Kept separate from the handler so the whole narrowing — unknown machine,
    /// wrong machine's token, proxy token, missing credential — is unit
    /// testable without a socket.
    fn authorize(&self, machine: &str, headers: &HeaderMap) -> Result<(), TunnelRejection> {
        // Machine ids are not secret: they appear in this coordinator's own
        // configuration, in its dashboard, and in LAN discovery records. A
        // clear 404 is what makes the single most likely misconfiguration — a
        // node dialing in under the wrong id — diagnosable from the node's
        // log, and no credential is compared for an unknown id, so this
        // answers nothing about any token.
        let Some(expected) = self.machines.get(machine) else {
            return Err(TunnelRejection::UnknownMachine);
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(crate::web::bearer_credential)
            .unwrap_or_default();
        if !crate::web::constant_time_eq(presented.as_bytes(), expected.expose().as_bytes()) {
            return Err(TunnelRejection::BadCredential);
        }
        if !requests_upgrade(headers) {
            return Err(TunnelRejection::NotAnUpgrade);
        }
        Ok(())
    }
}

/// Why a tunnel request was refused. The variants map one-to-one onto the
/// statuses documented in the README's troubleshooting list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TunnelRejection {
    UnknownMachine,
    BadCredential,
    NotAnUpgrade,
}

impl TunnelRejection {
    const fn status(self) -> StatusCode {
        match self {
            Self::UnknownMachine => StatusCode::NOT_FOUND,
            Self::BadCredential => StatusCode::UNAUTHORIZED,
            Self::NotAnUpgrade => StatusCode::BAD_REQUEST,
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::UnknownMachine => "no machine is configured with tunnel = true under that id",
            Self::BadCredential => "that machine's federation token is required",
            Self::NotAnUpgrade => "an Upgrade: atmux-tunnel request is required",
        }
    }
}

/// Whether the request asked for exactly the atmux tunnel upgrade.
///
/// `Connection` is a comma-separated list and both header values are
/// case-insensitive, so neither can be compared with a plain equality test.
fn requests_upgrade(headers: &HeaderMap) -> bool {
    let names_upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    let protocol = headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(UPGRADE_PROTOCOL));
    names_upgrade && protocol
}

/// The coordinator's tunnel route.
///
/// It is a separate router merged into the app so the credential narrowing
/// lives with the handler rather than in the shared policy layer.
pub fn router(acceptor: Arc<TunnelAcceptor>) -> Router {
    Router::new()
        .route("/api/v1/tunnel/{machine}", get(accept))
        .with_state(acceptor)
}

async fn accept(
    State(acceptor): State<Arc<TunnelAcceptor>>,
    Path(machine): Path<String>,
    mut request: Request,
) -> Response {
    if let Err(rejection) = acceptor.authorize(&machine, request.headers()) {
        return (
            rejection.status(),
            axum::Json(serde_json::json!({ "error": rejection.message() })),
        )
            .into_response();
    }
    let upgrade = hyper::upgrade::on(&mut request);
    let registry = acceptor.registry.clone();
    tokio::spawn(async move {
        match upgrade.await {
            Ok(stream) => drive_tunnel(&registry, &machine, stream).await,
            Err(error) => {
                println!("atmux tun  {machine} upgrade failed: {error}");
            }
        }
    });
    (
        StatusCode::SWITCHING_PROTOCOLS,
        [
            (header::CONNECTION, "upgrade"),
            (header::UPGRADE, UPGRADE_PROTOCOL),
        ],
    )
        .into_response()
}

/// Runs one accepted tunnel until the node hangs up.
async fn drive_tunnel<I>(registry: &TunnelRegistry, machine: &str, stream: I)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let handshake = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        // Keep-alive pings are timer driven; without one hyper panics rather
        // than silently running without them.
        .timer(TokioTimer::new())
        .initial_stream_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .keep_alive_interval(KEEP_ALIVE_INTERVAL)
        .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
        .keep_alive_while_idle(true)
        .handshake(stream)
        .await;
    let (sender, connection) = match handshake {
        Ok(pair) => pair,
        Err(error) => {
            println!("atmux tun  {machine} rejected the HTTP/2 handshake: {error}");
            return;
        }
    };
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let driver = tokio::spawn(async move {
        let reason = connection
            .await
            .map_or_else(|error| error.to_string(), |()| "closed".to_owned());
        let _ = finished_tx.send(reason);
    });
    let generation = registry.register(machine, sender, driver.abort_handle());
    println!("atmux tun  {machine} connected");
    let reason = finished_rx
        .await
        .unwrap_or_else(|_| "replaced by a newer connection".to_owned());
    registry.disconnect(machine, generation);
    println!("atmux tun  {machine} disconnected: {reason}");
}

/// Stamps the synthetic peer onto every request that arrives through a tunnel.
///
/// This layer is applied outside the request policy, so the policy sees a
/// non-loopback peer and still demands this node's token.
pub async fn mark_tunnel_peer(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(ConnectInfo(TUNNEL_PEER));
    next.run(request).await
}

/// Everything the node needs to dial its coordinator.
pub struct PhoneHome {
    machine: String,
    url: NodeUrl,
    token: Secret,
    tls: Option<TlsConnector>,
    app: Router,
}

impl std::fmt::Debug for PhoneHome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PhoneHome")
            .field("machine", &self.machine)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl PhoneHome {
    /// Prepares the outbound tunnel described by `[coordinator]`.
    ///
    /// `app` must already carry this node's full request policy; the tunnel
    /// grants no path that the node's own listener would not.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid coordinator URL, an unreadable token, or
    /// a TLS client that cannot be built.
    pub fn new(config: &CoordinatorConfig, machine: &str, app: Router) -> Result<Self> {
        let url = NodeUrl::parse(
            config
                .url
                .as_deref()
                .context("[coordinator] requires a url")?,
        )
        .context("invalid [coordinator] url")?;
        let token = resolve_token(
            "coordinator",
            config.token_env.as_deref(),
            config.token_file.as_deref(),
        )
        .context("failed to resolve the [coordinator] token")?
        .context("[coordinator] requires token_env or token_file")?;
        let tls = url.is_https().then(public_tls_client).transpose()?;
        Ok(Self {
            machine: machine.to_owned(),
            url,
            token,
            tls,
            // The tunnel peer must be stamped before the request policy runs,
            // and the outermost layer runs first.
            app: app.layer(axum::middleware::from_fn(mark_tunnel_peer)),
        })
    }

    /// Dials the coordinator forever, reconnecting with jittered backoff.
    #[must_use]
    pub fn spawn(self) -> AbortHandle {
        tokio::spawn(async move {
            let mut reconnect = crate::remote::ReconnectState::default();
            loop {
                match self.connect_once().await {
                    Ok(disconnect) => {
                        // A proxy that accepts the upgrade and then drops the
                        // stream immediately would otherwise reset the backoff
                        // on every attempt, turning a broken path into a hot
                        // loop against a public endpoint. Only a connection
                        // that actually carried traffic counts as healthy.
                        if disconnect.served >= HEALTHY_AFTER {
                            reconnect.record_healthy();
                        }
                        println!(
                            "atmux tun  disconnected from {} as {} after {}s: {}",
                            self.url,
                            self.machine,
                            disconnect.served.as_secs(),
                            disconnect.reason
                        );
                    }
                    Err(error) => {
                        println!(
                            "atmux tun  could not reach {} as {}: {error:#}",
                            self.url, self.machine
                        );
                    }
                }
                tokio::time::sleep(crate::remote::jittered(reconnect.record_failure())).await;
            }
        })
        .abort_handle()
    }

    /// Opens one tunnel and serves it until it ends, returning why it ended.
    async fn connect_once(&self) -> Result<Disconnect> {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((self.url.host(), self.url.port())),
        )
        .await
        .context("the coordinator did not answer in time")?
        .context("the coordinator is unreachable")?;
        stream
            .set_nodelay(true)
            .context("failed to configure the tunnel socket")?;
        match &self.tls {
            Some(tls) => {
                let name = ServerName::try_from(self.url.host().to_owned())
                    .context("the coordinator url is not a valid TLS server name")?;
                let stream = tokio::time::timeout(CONNECT_TIMEOUT, tls.connect(name, stream))
                    .await
                    .context("the coordinator timed out negotiating TLS")?
                    .context("the coordinator presented an untrusted certificate")?;
                self.upgrade_and_serve(TokioIo::new(stream)).await
            }
            None => self.upgrade_and_serve(TokioIo::new(stream)).await,
        }
    }

    async fn upgrade_and_serve<I>(&self, stream: I) -> Result<Disconnect>
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(stream)
            .await
            .context("the coordinator rejected the HTTP handshake")?;
        // Only an upgradeable driver hands the raw stream back after 101.
        let driver = tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
        });
        let request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(self.url.request_target(&format!(
                "{TUNNEL_PATH_PREFIX}{}",
                crate::remote::encode_segment(&self.machine)
            )))
            .header(header::HOST, self.url.authority())
            .header(
                header::USER_AGENT,
                concat!("atmux/", env!("CARGO_PKG_VERSION")),
            )
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, UPGRADE_PROTOCOL)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.token.expose()),
            )
            .body(Full::<Bytes>::new(Bytes::new()))
            .context("failed to build the tunnel request")?;
        let response = tokio::time::timeout(HANDSHAKE_TIMEOUT, sender.send_request(request))
            .await
            .context("the coordinator timed out answering the tunnel request")?
            .context("the coordinator refused the tunnel request")?;
        let status = response.status();
        if status != StatusCode::SWITCHING_PROTOCOLS {
            driver.abort();
            // A proxy that strips hop-by-hop headers turns the upgrade into an
            // ordinary request and answers 200 with a body. Treating anything
            // but 101 as a hard failure is what keeps that from looking like a
            // working tunnel that never carries a request.
            bail!("{}", non_upgrade_hint(status));
        }
        let upgraded = hyper::upgrade::on(response)
            .await
            .context("the coordinator accepted but never handed over the stream")?;
        println!("atmux tun  connected to {} as {}", self.url, self.machine);
        let opened = std::time::Instant::now();
        let outcome = serve_h2(self.app.clone(), upgraded).await;
        Ok(Disconnect {
            reason: outcome.map_or_else(|error| error.to_string(), |()| "closed".to_owned()),
            served: opened.elapsed(),
        })
    }
}

/// Why one tunnel ended, and how long it actually carried traffic.
#[derive(Debug)]
struct Disconnect {
    reason: String,
    served: Duration,
}

/// Serves one already-upgraded stream as this node's HTTP/2 API.
async fn serve_h2<I>(app: Router, io: I) -> hyper::Result<()>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .initial_stream_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .keep_alive_interval(KEEP_ALIVE_INTERVAL)
        .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
        .serve_connection(io, TowerToHyperService::new(app))
        .await
}

/// Explains a non-101 answer in the terms an operator can act on.
fn non_upgrade_hint(status: StatusCode) -> String {
    let hint = match status {
        StatusCode::OK => {
            " (a proxy on the path stripped Upgrade/Connection, or the request reached the single-sign-on port instead of the tunnel port)"
        }
        StatusCode::UNAUTHORIZED => " (the coordinator holds a different token for this machine)",
        StatusCode::NOT_FOUND => {
            " (no [[machines]] entry with this id sets tunnel = true on the coordinator)"
        }
        StatusCode::FORBIDDEN => " (the coordinator rejected the Host header)",
        _ => "",
    };
    format!("the coordinator answered {status} instead of 101{hint}")
}

/// A TLS client that trusts the public root program and pins ALPN to
/// HTTP/1.1.
///
/// Pinning matters: an edge that negotiates `h2` would never see an HTTP/1.1
/// `Upgrade` at all, and the handshake would fail in a way that looks like a
/// server problem.
fn public_tls_client() -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if roots.is_empty() {
        bail!("no trusted certificate authorities are available for the coordinator tunnel");
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Serves one already-established stream as a tunnel node.
///
/// Exposed so an integration test can run a real node app over an in-memory
/// duplex without a socket or a TLS handshake.
///
/// # Errors
///
/// Returns an error when the HTTP/2 connection itself fails.
pub async fn serve_node_stream<T>(app: Router, stream: T) -> Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let app = app.layer(axum::middleware::from_fn(mark_tunnel_peer));
    serve_h2(app, TokioIo::new(stream))
        .await
        .context("a tunneled HTTP/2 connection failed")
}

/// Registers an already-established stream as one machine's tunnel.
///
/// Exposed for the same reason as [`serve_node_stream`]: it lets a test drive
/// the coordinator's real transport over an in-memory pipe.
pub async fn register_stream<T>(registry: &TunnelRegistry, machine: &str, stream: T)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    drive_tunnel(registry, machine, TokioIo::new(stream)).await;
}

/// Where the coordinator's HTTP/2 client sends a tunneled request.
///
/// Kept next to [`TUNNEL_AUTHORITY`] so the node's allow-list and the
/// coordinator's request target cannot drift apart.
#[must_use]
pub fn tunnel_request_target(path: &str) -> String {
    format!("http://{TUNNEL_AUTHORITY}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn acceptor(machines: &[(&str, &str)]) -> Arc<TunnelAcceptor> {
        TunnelAcceptor::new(
            TunnelRegistry::new(),
            machines
                .iter()
                .map(|(id, token)| ((*id).to_owned(), Secret::new(token)))
                .collect(),
        )
    }

    fn upgrade_headers(authorization: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static(UPGRADE_PROTOCOL));
        if let Some(value) = authorization {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn only_the_named_machines_own_token_opens_a_tunnel() {
        let acceptor = acceptor(&[("midnight", "midnight-token"), ("tron", "tron-token")]);
        assert_eq!(
            acceptor.authorize("midnight", &upgrade_headers(Some("Bearer midnight-token"))),
            Ok(())
        );
        // The web proxy token authenticates every other /api/ path; here it is
        // simply another wrong secret.
        assert_eq!(
            acceptor.authorize("midnight", &upgrade_headers(Some("Bearer web-proxy-token"))),
            Err(TunnelRejection::BadCredential)
        );
        // Another machine's federation token must not impersonate this one.
        assert_eq!(
            acceptor.authorize("midnight", &upgrade_headers(Some("Bearer tron-token"))),
            Err(TunnelRejection::BadCredential)
        );
        assert_eq!(
            acceptor.authorize("midnight", &upgrade_headers(None)),
            Err(TunnelRejection::BadCredential)
        );
        assert_eq!(
            acceptor.authorize("midnight", &upgrade_headers(Some("midnight-token"))),
            Err(TunnelRejection::BadCredential),
            "a bare credential without the Bearer scheme must not authenticate"
        );
    }

    #[test]
    fn a_machine_without_tunnel_is_not_found_and_compares_no_token() {
        let acceptor = acceptor(&[("midnight", "midnight-token")]);
        assert_eq!(
            acceptor.authorize("tron", &upgrade_headers(Some("Bearer midnight-token"))),
            Err(TunnelRejection::UnknownMachine)
        );
        assert_eq!(
            TunnelRejection::UnknownMachine.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            TunnelRejection::BadCredential.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn the_upgrade_headers_are_validated_after_the_credential() {
        let acceptor = acceptor(&[("midnight", "midnight-token")]);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer midnight-token"),
        );
        assert_eq!(
            acceptor.authorize("midnight", &headers),
            Err(TunnelRejection::NotAnUpgrade)
        );
        // An unauthenticated caller learns nothing about the upgrade shape.
        assert_eq!(
            acceptor.authorize("midnight", &HeaderMap::new()),
            Err(TunnelRejection::BadCredential)
        );
    }

    #[test]
    fn upgrade_headers_are_matched_case_insensitively_and_per_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, UPGRADE"),
        );
        headers.insert(header::UPGRADE, HeaderValue::from_static("ATMUX-Tunnel"));
        assert!(requests_upgrade(&headers));

        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(
            !requests_upgrade(&headers),
            "another upgrade protocol must not open an atmux tunnel"
        );

        headers.insert(header::UPGRADE, HeaderValue::from_static("atmux-tunnel"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
        assert!(!requests_upgrade(&headers));

        // A `Connection` value that merely contains the substring must not pass.
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("no-upgrade-here"),
        );
        assert!(!requests_upgrade(&headers));
        assert!(!requests_upgrade(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn the_registry_is_last_wins_and_aborts_the_replaced_driver() {
        let registry = TunnelRegistry::new();
        assert!(!registry.is_connected("midnight"));

        let (first_sender, first_driver) = fake_tunnel().await;
        let first = registry.register("midnight", first_sender, first_driver.abort_handle());
        assert!(registry.is_connected("midnight"));

        let (second_sender, second_driver) = fake_tunnel().await;
        let second = registry.register("midnight", second_sender, second_driver.abort_handle());
        assert!(second > first, "generations must be monotonic");
        // The replaced connection driver is aborted, so a half-dead socket
        // cannot keep answering.
        assert!(first_driver.await.unwrap_err().is_cancelled());

        // A late cleanup from the replaced connection must not evict the live
        // one.
        registry.disconnect("midnight", first);
        assert!(registry.is_connected("midnight"));
        registry.disconnect("midnight", second);
        assert!(!registry.is_connected("midnight"));
    }

    #[tokio::test]
    async fn a_registration_wakes_a_waiting_watcher() {
        let registry = TunnelRegistry::new();
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.changed().await })
        };
        // Give the waiter a chance to subscribe before the change lands.
        tokio::task::yield_now().await;
        let (sender, driver) = fake_tunnel().await;
        registry.register("midnight", sender, driver.abort_handle());
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("a registration must wake a waiting watcher")
            .unwrap();
        driver.abort();
    }

    /// A live HTTP/2 client half whose peer never answers. Enough to exercise
    /// registry bookkeeping without a node.
    async fn fake_tunnel() -> (TunnelSender, tokio::task::JoinHandle<()>) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let driver_server = tokio::spawn(async move {
            let _ = serve_node_stream(Router::new(), server).await;
        });
        let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(client))
            .await
            .unwrap();
        let driver = tokio::spawn(async move {
            let _ = connection.await;
            driver_server.abort();
        });
        (sender, driver)
    }

    #[test]
    fn a_non_upgrade_answer_names_the_likely_cause() {
        assert!(non_upgrade_hint(StatusCode::OK).contains("stripped"));
        assert!(non_upgrade_hint(StatusCode::NOT_FOUND).contains("tunnel = true"));
        assert!(non_upgrade_hint(StatusCode::UNAUTHORIZED).contains("token"));
        assert!(non_upgrade_hint(StatusCode::FORBIDDEN).contains("Host"));
        assert!(non_upgrade_hint(StatusCode::BAD_GATEWAY).contains("502"));
    }

    #[test]
    fn the_synthetic_tunnel_peer_is_never_loopback() {
        assert!(!TUNNEL_PEER.ip().is_loopback());
        assert_eq!(
            tunnel_request_target("/api/v1/events"),
            "http://atmux.tunnel/api/v1/events"
        );
    }
}
