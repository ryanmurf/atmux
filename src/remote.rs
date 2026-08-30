//! Outbound transport to trusted remote atmux nodes.
//!
//! The coordinator keeps exactly one shared watcher per remote machine. That
//! watcher subscribes to the node's change-only overview stream, so browser
//! count never multiplies remote traffic. Pane output is fetched on demand and
//! only when a machine reports that the pane's content hash changed.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
#[cfg(target_os = "macos")]
use std::{io, process::Stdio};

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, StatusCode,
    body::{Bytes, Incoming},
    header,
};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{io::ReadBuf, net::TcpStream, task::JoinHandle};
#[cfg(target_os = "macos")]
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream},
    process::{Child, Command},
};
use tokio_rustls::TlsConnector;

use crate::{
    config::{MachineConfig, TlsConfig},
    control::{ControlPlane, LaunchOptions, Overview, OverviewPatch, SessionSummary},
    machine::{MachineKind, MachineSummary, NodeUrl, Secret, composite_id, resolve_token},
};

const USER_AGENT: &str = concat!("atmux/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// A node that stops sending even keep-alives is treated as dead.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// A configured, validated remote node together with its resolved credential.
///
/// Every request opens its own TCP connection; there is no connection pool, so
/// a request's cost is one handshake plus one round trip. The shared watcher
/// keeps federation to roughly one connection per machine regardless of client
/// count, which is why pooling has not been needed.
pub struct RemoteMachine {
    pub id: String,
    pub label: String,
    pub url: NodeUrl,
    token: Option<Secret>,
    /// Present only for certificate-validating HTTPS federation. Keeping the
    /// legacy manual HTTP transport confined to loopback makes existing local
    /// test harnesses work while production configurations are TLS-only.
    tls_client: Option<TlsConnector>,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl std::fmt::Debug for RemoteMachine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteMachine")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("url", &self.url)
            .field("token", &self.token)
            .field("tls", &self.tls_client.is_some())
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl RemoteMachine {
    /// Builds a runtime handle from validated configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid URL or an unreadable credential.
    pub fn from_config(machine: &MachineConfig) -> Result<Self> {
        let url = NodeUrl::parse(&machine.url)
            .with_context(|| format!("invalid url for machine {}", machine.id))?;
        let token = resolve_token(
            &machine.id,
            machine.token_env.as_deref(),
            machine.token_file.as_deref(),
        )?;
        Ok(Self {
            id: machine.id.clone(),
            label: machine.label.clone().unwrap_or_else(|| machine.id.clone()),
            url,
            token,
            tls_client: None,
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        })
    }

    /// Builds a configured HTTPS remote whose server certificate and client
    /// certificate both chain to the node's private atmux CA.
    ///
    /// # Errors
    ///
    /// Returns an error when the machine URL is not HTTPS or TLS material is
    /// unreadable or invalid.
    pub fn from_config_with_tls(machine: &MachineConfig, tls: &TlsConfig) -> Result<Self> {
        let mut remote = Self::from_config(machine)?;
        if !remote.url.is_https() {
            bail!(
                "machine {} uses plaintext HTTP; remote federation requires HTTPS",
                remote.id
            );
        }
        remote.tls_client = Some(crate::tls::client(tls)?);
        Ok(remote)
    }

    /// Builds a mutually authenticated HTTPS remote from a validated DNS-SD
    /// advertisement. The multicast record carries no credential; its address
    /// must first prove possession of a certificate signed by this node's CA.
    ///
    /// # Errors
    ///
    /// Returns an error only if the synthesized private-network endpoint would
    /// fail the same URL validation applied to configured remotes.
    pub fn from_discovery(
        id: String,
        label: String,
        address: Ipv4Addr,
        port: u16,
        token: Option<Secret>,
        tls: &TlsConfig,
    ) -> Result<Self> {
        let url = NodeUrl::parse(&format!("https://{address}:{port}"))
            .context("invalid discovered machine address")?;
        Ok(Self {
            id,
            label,
            url,
            token,
            tls_client: Some(crate::tls::client(tls)?),
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        })
    }

    /// Overrides the transport timeouts.
    ///
    /// This is the seam tests use to exercise a hung node without waiting for
    /// the production timeouts.
    #[must_use]
    pub const fn with_timeouts(mut self, connect: Duration, request: Duration) -> Self {
        self.connect_timeout = connect;
        self.request_timeout = request;
        self
    }

    #[must_use]
    pub fn address(&self) -> String {
        self.url.authority()
    }

    #[must_use]
    pub const fn is_authenticated(&self) -> bool {
        self.token.is_some()
    }

    /// Reads and decodes a JSON document from the node.
    ///
    /// # Errors
    ///
    /// Returns an error for a transport failure, a non-success status, an
    /// oversized body, or a malformed payload.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let body = self.request(Method::GET, path, None).await?;
        serde_json::from_slice(&body)
            .with_context(|| format!("machine {} returned an unreadable {path} payload", self.id))
    }

    /// Sends a JSON command to the node.
    ///
    /// # Errors
    ///
    /// Returns an error for a transport failure or a non-success status.
    pub async fn post_json<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let encoded = serde_json::to_vec(body).context("failed to encode a federated command")?;
        self.request(Method::POST, path, Some(encoded)).await?;
        Ok(())
    }

    /// Sends a JSON command and decodes its bounded JSON response.
    ///
    /// This is used for owner-scoped operations whose asynchronous status must
    /// be reported back through a coordinator without exposing owner-local
    /// implementation details.
    ///
    /// # Errors
    ///
    /// Returns an error for request serialization, transport failure, a
    /// non-success response, an oversized response, or malformed JSON.
    pub async fn post_json_response<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let encoded = serde_json::to_vec(body).context("failed to encode a federated command")?;
        let response = self.request(Method::POST, path, Some(encoded)).await?;
        serde_json::from_slice(&response)
            .with_context(|| format!("machine {} returned an unreadable {path} payload", self.id))
    }

    /// Sends an optimistic JSON replacement and decodes its bounded response.
    /// The rejected HTTP status remains downcastable so a coordinator can
    /// preserve an owner's 400/404/409 classification.
    ///
    /// # Errors
    ///
    /// Returns an error for request serialization, transport failure, a
    /// non-success response, an oversized response, or malformed JSON.
    pub async fn put_json_response<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let encoded = serde_json::to_vec(body).context("failed to encode a federated update")?;
        let response = self.request(Method::PUT, path, Some(encoded)).await?;
        serde_json::from_slice(&response)
            .with_context(|| format!("machine {} returned an unreadable {path} payload", self.id))
    }

    /// Sends a DELETE command to the node.
    ///
    /// # Errors
    ///
    /// Returns an error for a transport failure or a non-success status.
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.request(Method::DELETE, path, None).await?;
        Ok(())
    }

    /// Opens the node's change-only overview stream.
    ///
    /// # Errors
    ///
    /// Returns an error when the node is unreachable or rejects the request.
    pub async fn open_events(&self, path: &str) -> Result<EventStream> {
        // The guard owns the connection driver from the moment it exists, so a
        // timeout, a refusal, or a rejected status can never leave a detached
        // task holding the socket open. Ownership moves into the returned
        // stream only once the stream itself exists.
        let (mut sender, guard) = self.connect().await?;
        let request = self
            .build(Method::GET, path)
            .header(header::ACCEPT, "text/event-stream")
            .body(Full::new(Bytes::new()))
            .context("failed to build a federated stream request")?;
        let response = tokio::time::timeout(self.request_timeout, sender.send_request(request))
            .await
            .with_context(|| format!("machine {} timed out opening {path}", self.id))?
            .with_context(|| format!("machine {} refused {path}", self.id))?;
        check_status(&self.id, path, response.status())?;
        Ok(EventStream {
            body: response.into_body(),
            decoder: SseDecoder::default(),
            pending: Vec::new(),
            _guard: guard,
        })
    }

    async fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Bytes> {
        let (mut sender, _guard) = self.connect().await?;
        let mut builder = self
            .build(method, path)
            .header(header::ACCEPT, "application/json");
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .context("failed to build a federated request")?;
        let response = tokio::time::timeout(self.request_timeout, sender.send_request(request))
            .await
            .with_context(|| format!("machine {} timed out on {path}", self.id))?
            .with_context(|| format!("machine {} refused {path}", self.id))?;
        let status = response.status();
        let collected = tokio::time::timeout(
            self.request_timeout,
            collect_bounded(response.into_body(), MAX_RESPONSE_BYTES),
        )
        .await
        .with_context(|| format!("machine {} timed out sending {path}", self.id))??;
        if !status.is_success() {
            let detail = remote_error_detail(&collected);
            return Err(RemoteResponseError {
                machine: self.id.clone(),
                path: path.to_owned(),
                status,
                detail,
            }
            .into());
        }
        Ok(collected)
    }

    fn build(&self, method: Method, path: &str) -> hyper::http::request::Builder {
        let mut builder = Request::builder()
            .method(method)
            .uri(self.url.request_target(path))
            .header(header::HOST, self.url.authority())
            .header(header::USER_AGENT, USER_AGENT);
        if let Some(token) = &self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {}", token.expose()));
        }
        builder
    }

    async fn connect(&self) -> Result<(SendRequest, ConnectionGuard)> {
        let target = (self.url.host().to_owned(), self.url.port());
        let stream =
            tokio::time::timeout(self.connect_timeout, connect_target(&target.0, target.1))
                .await
                .with_context(|| {
                    format!(
                        "machine {} did not answer within {:?}",
                        self.id, self.connect_timeout
                    )
                })?
                .with_context(|| format!("machine {} is unreachable at {}", self.id, self.url))?;
        stream
            .set_nodelay()
            .context("failed to configure the federated socket")?;
        if let Some(tls) = &self.tls_client {
            let server_name =
                ServerName::try_from(self.url.host().to_owned()).with_context(|| {
                    format!(
                        "machine {} has an invalid TLS server name {}",
                        self.id,
                        self.url.host()
                    )
                })?;
            let stream =
                tokio::time::timeout(self.connect_timeout, tls.connect(server_name, stream))
                    .await
                    .with_context(|| format!("machine {} timed out negotiating TLS", self.id))?
                    .with_context(|| format!("machine {} rejected its TLS identity", self.id))?;
            let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .with_context(|| format!("machine {} rejected the HTTPS handshake", self.id))?;
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            return Ok((sender, ConnectionGuard(driver)));
        }
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .with_context(|| format!("machine {} rejected the HTTP handshake", self.id))?;
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok((sender, ConnectionGuard(driver)))
    }
}

#[derive(Debug)]
struct RemoteResponseError {
    machine: String,
    path: String,
    status: StatusCode,
    detail: String,
}

impl std::fmt::Display for RemoteResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "machine {} rejected {} with {}{}",
            self.machine, self.path, self.status, self.detail
        )
    }
}

impl std::error::Error for RemoteResponseError {}

/// Returns the owner's rejected HTTP status without exposing transport
/// internals in the public control-plane API.
#[must_use]
pub(crate) fn rejected_status(error: &anyhow::Error) -> Option<u16> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RemoteResponseError>())
        .map(|error| error.status.as_u16())
}

/// Opens a TCP connection without passing numeric IP addresses through the
/// platform resolver. This avoids macOS resolver failures for otherwise
/// reachable LAN peers while preserving hostname support for configured nodes.
async fn connect_target(host: &str, port: u16) -> std::io::Result<ConnectedStream> {
    let direct = connect_direct(host, port).await;
    #[cfg(target_os = "macos")]
    if let Err(error) = direct.as_ref()
        && error.raw_os_error() == Some(65)
        && let Ok(stream) = connect_with_nc(host, port)
    {
        return Ok(stream);
    }
    direct.map(ConnectedStream::Tcp)
}

async fn connect_direct(host: &str, port: u16) -> std::io::Result<TcpStream> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        let target = SocketAddr::new(ip, port);
        match ip {
            IpAddr::V4(_) => {
                let socket = tokio::net::TcpSocket::new_v4()?;
                if let Some(source) = routed_source(target) {
                    socket.bind(SocketAddr::new(source, 0))?;
                }
                socket.connect(target).await
            }
            IpAddr::V6(_) => TcpStream::connect(target).await,
        }
    } else {
        TcpStream::connect((host, port)).await
    }
}

#[derive(Debug)]
enum ConnectedStream {
    Tcp(TcpStream),
    #[cfg(target_os = "macos")]
    Nc(NcTunnel),
}

impl ConnectedStream {
    fn set_nodelay(&self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_nodelay(true),
            #[cfg(target_os = "macos")]
            Self::Nc(_) => Ok(()),
        }
    }
}

impl tokio::io::AsyncRead for ConnectedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_read(context, buffer),
            #[cfg(target_os = "macos")]
            Self::Nc(stream) => Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl tokio::io::AsyncWrite for ConnectedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_write(context, buffer),
            #[cfg(target_os = "macos")]
            Self::Nc(stream) => Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_flush(context),
            #[cfg(target_os = "macos")]
            Self::Nc(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_shutdown(context),
            #[cfg(target_os = "macos")]
            Self::Nc(stream) => Pin::new(stream).poll_shutdown(context),
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct NcTunnel {
    stream: DuplexStream,
    child: Child,
    to_child: JoinHandle<io::Result<u64>>,
    from_child: JoinHandle<io::Result<u64>>,
}

#[cfg(target_os = "macos")]
fn connect_with_nc(host: &str, port: u16) -> io::Result<ConnectedStream> {
    let mut child = Command::new("/usr/bin/nc")
        .arg(host)
        .arg(port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("failed to open the macOS network-tunnel input"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to open the macOS network-tunnel output"))?;
    let (stream, bridge) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(bridge);
    let to_child = tokio::spawn(async move {
        let mut reader = reader;
        let mut stdin = stdin;
        tokio::io::copy(&mut reader, &mut stdin).await
    });
    let from_child = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut writer = writer;
        tokio::io::copy(&mut stdout, &mut writer).await
    });
    Ok(ConnectedStream::Nc(NcTunnel {
        stream,
        child,
        to_child,
        from_child,
    }))
}

#[cfg(target_os = "macos")]
impl Drop for NcTunnel {
    fn drop(&mut self) {
        self.to_child.abort();
        self.from_child.abort();
        let _ = self.child.start_kill();
    }
}

#[cfg(target_os = "macos")]
impl AsyncRead for NcTunnel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

#[cfg(target_os = "macos")]
impl AsyncWrite for NcTunnel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

/// Asks the operating system which local interface it would use for a numeric
/// peer, then pins the TCP socket to that source. This works around a macOS
/// route-selection failure observed in long-running Tokio clients on hosts
/// with multiple private interfaces, while keeping normal OS routing intact.
fn routed_source(target: SocketAddr) -> Option<IpAddr> {
    let bind = match target {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(target).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

type SendRequest = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

/// Owns a spawned hyper connection driver and aborts it on drop.
///
/// Every path that creates a connection holds one of these, so an error before
/// the response is fully owned can never detach the driver task or leak its
/// socket.
#[derive(Debug)]
struct ConnectionGuard(JoinHandle<()>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn check_status(machine: &str, path: &str, status: StatusCode) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        bail!(
            "machine {machine} rejected {path} with {status}; check its token and --allowed-host value"
        );
    }
    bail!("machine {machine} rejected {path} with {status}");
}

fn remote_error_detail(body: &Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .map_or_else(String::new, |message| format!(": {message}"))
}

async fn collect_bounded(mut body: Incoming, limit: usize) -> Result<Bytes> {
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.context("a federated response body failed")?;
        if let Ok(chunk) = frame.into_data() {
            if collected.len() + chunk.len() > limit {
                bail!("a federated response exceeded {limit} bytes");
            }
            collected.extend_from_slice(&chunk);
        }
    }
    Ok(Bytes::from(collected))
}

/// One decoded Server-Sent Event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub name: String,
    pub data: String,
}

/// Incremental Server-Sent Events decoder.
///
/// Chunk boundaries, `\r\n` line endings, comment keep-alives, and multi-line
/// `data:` fields are all handled, and a single oversized event is rejected
/// rather than buffered without bound.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    name: String,
    data: String,
}

impl SseDecoder {
    /// Feeds one transport chunk and returns every event it completed.
    ///
    /// # Errors
    ///
    /// Returns an error when a single event exceeds the buffer limit.
    pub fn push(&mut self, chunk: &[u8], into: &mut Vec<SseEvent>) -> Result<()> {
        if self.buffer.len() + chunk.len() > MAX_EVENT_BYTES {
            bail!("a federated event exceeded {MAX_EVENT_BYTES} bytes");
        }
        self.buffer.extend_from_slice(chunk);
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer.drain(..=position).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            self.line(line.trim_end_matches(['\r', '\n']), into);
        }
        if self.data.len() > MAX_EVENT_BYTES {
            bail!("a federated event exceeded {MAX_EVENT_BYTES} bytes");
        }
        Ok(())
    }

    fn line(&mut self, line: &str, into: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if !self.data.is_empty() || !self.name.is_empty() {
                into.push(SseEvent {
                    name: std::mem::take(&mut self.name),
                    data: std::mem::take(&mut self.data)
                        .strip_suffix('\n')
                        .map_or_else(String::new, str::to_owned),
                });
            }
            self.name.clear();
            self.data.clear();
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => {
                self.name.clear();
                self.name.push_str(value);
            }
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            _ => {}
        }
    }
}

/// A live Server-Sent Events subscription to one node.
#[derive(Debug)]
pub struct EventStream {
    body: Incoming,
    decoder: SseDecoder,
    pending: Vec<SseEvent>,
    /// Aborts the connection driver when the subscription is dropped.
    _guard: ConnectionGuard,
}

impl EventStream {
    /// Waits for the next decoded event.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream closes, stalls past the idle timeout,
    /// or produces an oversized event.
    pub async fn next_event(&mut self) -> Result<SseEvent> {
        loop {
            if !self.pending.is_empty() {
                return Ok(self.pending.remove(0));
            }
            let frame = tokio::time::timeout(STREAM_IDLE_TIMEOUT, self.body.frame())
                .await
                .context("a federated stream stalled past its keep-alive interval")?
                .context("a federated stream closed")?
                .context("a federated stream failed")?;
            if let Ok(chunk) = frame.into_data() {
                let mut decoded = Vec::new();
                self.decoder.push(&chunk, &mut decoded)?;
                self.pending = decoded;
            }
        }
    }
}

/// Reconnect delay for the given consecutive-failure count.
///
/// Bounded, monotonic, and capped so an offline node produces at most two
/// connection attempts per minute.
#[must_use]
pub fn backoff_delay(failures: u32) -> Duration {
    const BASE_MS: u64 = 500;
    const MAX_MS: u64 = 30_000;
    let shift = failures.saturating_sub(1).min(16);
    Duration::from_millis((BASE_MS.saturating_mul(1_u64 << shift)).min(MAX_MS))
}

/// Consecutive-failure counter behind the reconnect backoff.
///
/// A watcher can only ever leave [`watch_once`] with an error, so the counter
/// is reset from inside the loop the moment a node delivers a valid snapshot.
/// Without that, a node that fails after hours of healthy streaming would still
/// be retried at the 30s ceiling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconnectState {
    failures: u32,
}

impl ReconnectState {
    /// Records that the node delivered a valid snapshot, restoring base delay.
    pub const fn record_healthy(&mut self) {
        self.failures = 0;
    }

    /// Records a failed or ended connection and returns the delay to wait.
    pub fn record_failure(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        backoff_delay(self.failures)
    }

    #[must_use]
    pub const fn failures(self) -> u32 {
        self.failures
    }
}

/// Mirror of one remote machine's session inventory.
///
/// Sessions are re-keyed into the coordinator's composite namespace, and any
/// session the node itself federates from a third machine is dropped so that
/// pane ids can never collide.
#[derive(Debug, Default)]
pub struct RemoteMirror {
    home: Option<String>,
    sessions: HashMap<String, SessionSummary>,
    /// Revision of the last snapshot or patch this mirror accepted. `None`
    /// until an authoritative snapshot arrives.
    revision: Option<u64>,
}

impl RemoteMirror {
    /// Replaces the mirror from an authoritative snapshot.
    pub fn snapshot(&mut self, machine: &str, overview: &Overview) {
        self.home = home_machine(overview);
        self.sessions.clear();
        self.revision = Some(overview.revision);
        self.merge(machine, &overview.sessions);
    }

    /// Applies an incremental patch, or reports that the mirror must resync.
    ///
    /// A patch is applied only when it continues the exact revision this mirror
    /// last accepted. A patch that arrives before any snapshot, or one whose
    /// `base_revision` does not match, is discarded rather than merged: merging
    /// it would silently produce a mirror that matches neither node nor
    /// coordinator.
    pub(crate) fn patch(&mut self, machine: &str, patch: &OverviewPatch) -> bool {
        if self.revision != Some(patch.base_revision) {
            return false;
        }
        if !patch.machines.is_empty()
            && let Some(home) = home_machine_of(&patch.machines)
        {
            self.home = Some(home);
        }
        for removed in &patch.remove {
            let pane = match removed.split_once(crate::machine::COMPOSITE_SEPARATOR) {
                Some((owner, _)) if self.home.as_deref().is_some_and(|home| owner != home) => {
                    // A coordinator's stream includes removals from machines
                    // it mirrors. Ignore those just as `merge` ignores nested
                    // upserts; stripping the owner first would let a remote
                    // `%4` delete this node's unrelated local `%4`.
                    continue;
                }
                Some((_, pane)) => pane,
                None => removed.as_str(),
            };
            self.sessions.remove(pane);
        }
        self.merge(machine, &patch.upsert);
        self.revision = Some(patch.revision);
        true
    }

    /// The revision this mirror last accepted, if any.
    #[must_use]
    pub const fn revision(&self) -> Option<u64> {
        self.revision
    }

    fn merge(&mut self, machine: &str, sessions: &[SessionSummary]) {
        for session in sessions {
            if !self.owns(session) {
                continue;
            }
            let mut session = session.clone();
            session.id = composite_id(machine, &session.pane_id);
            session.machine.clear();
            session.machine.push_str(machine);
            self.sessions.insert(session.pane_id.clone(), session);
        }
    }

    /// Only sessions the node runs itself are federated. A node that is also a
    /// coordinator keeps its own remotes to itself, so federation stays one
    /// level deep and composite ids stay unique.
    fn owns(&self, session: &SessionSummary) -> bool {
        match &self.home {
            // A node that reports no machines predates federation, so every
            // session it lists is its own.
            Some(home) if !session.machine.is_empty() => &session.machine == home,
            _ => true,
        }
    }

    /// The mirrored sessions in a stable order.
    #[must_use]
    pub fn sessions(&self) -> Vec<SessionSummary> {
        let mut sessions = self.sessions.values().cloned().collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        sessions
    }
}

fn home_machine(overview: &Overview) -> Option<String> {
    home_machine_of(&overview.machines)
}

fn home_machine_of(machines: &[crate::machine::MachineSummary]) -> Option<String> {
    machines
        .iter()
        .find(|machine| machine.kind == crate::machine::MachineKind::Local)
        .map(|machine| machine.id.clone())
}

fn local_metrics(machines: &[MachineSummary]) -> Option<crate::metrics::MachineMetrics> {
    machines
        .iter()
        .find(|machine| machine.kind == MachineKind::Local)
        .map(|machine| machine.metrics.clone())
}

/// Percent-encodes one path segment, so tmux pane ids such as `%7` survive
/// transport to a node unchanged.
#[must_use]
pub fn encode_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

/// Runs the single shared watcher for one remote machine until shutdown.
///
/// One offline machine only affects its own group: every failure is recorded as
/// that machine's health and retried with bounded backoff.
#[must_use]
pub fn spawn_watcher(
    control: ControlPlane,
    machine: Arc<RemoteMachine>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let mut reconnect = ReconnectState::default();
        loop {
            // `watch_once` streams until something goes wrong, so it can only
            // ever return the reason it stopped. The failure counter is reset
            // from inside it, the moment a valid snapshot arrives.
            let error = watch_once(&control, &machine, &mut reconnect).await;
            control.mark_machine_offline(&machine.id, &format!("{error:#}"));
            tokio::time::sleep(reconnect.record_failure()).await;
        }
    })
    .abort_handle()
}

/// Streams one node's events until the connection fails, ends, or desynchronizes.
///
/// Returns the reason it stopped; there is no success path.
async fn watch_once(
    control: &ControlPlane,
    machine: &Arc<RemoteMachine>,
    reconnect: &mut ReconnectState,
) -> anyhow::Error {
    match watch_stream(control, machine, reconnect).await {
        // `watch_stream` yields `Infallible` on success, so this arm is a
        // compile-time proof that the only exit is an error.
        Ok(never) => match never {},
        Err(error) => error,
    }
}

async fn watch_stream(
    control: &ControlPlane,
    machine: &Arc<RemoteMachine>,
    reconnect: &mut ReconnectState,
) -> Result<std::convert::Infallible> {
    let mut stream = machine.open_events("/api/v1/events").await?;
    let mut mirror = RemoteMirror::default();
    let mut launch_options_fetched = false;
    loop {
        let event = stream.next_event().await?;
        match event.name.as_str() {
            "sessions.snapshot" => {
                let overview: Overview = serde_json::from_str(&event.data)
                    .with_context(|| format!("machine {} sent an invalid snapshot", machine.id))?;
                mirror.snapshot(&machine.id, &overview);
                control.apply_machine_sessions(&machine.id, mirror.sessions(), overview.health);
                if let Some(metrics) = local_metrics(&overview.machines) {
                    control.set_machine_metrics(&machine.id, metrics);
                }
                // A node that snapshots is healthy, whatever happens next.
                reconnect.record_healthy();
                if !launch_options_fetched {
                    launch_options_fetched = true;
                    fetch_launch_options(control, machine).await;
                }
            }
            "sessions.patch" => {
                let patch: OverviewPatch = serde_json::from_str(&event.data)
                    .with_context(|| format!("machine {} sent an invalid patch", machine.id))?;
                if !mirror.patch(&machine.id, &patch) {
                    bail!(
                        "machine {} sent a patch based on revision {} while the mirror held {:?}; resyncing",
                        machine.id,
                        patch.base_revision,
                        mirror.revision()
                    );
                }
                control.apply_machine_sessions(&machine.id, mirror.sessions(), patch.health);
                if let Some(metrics) = local_metrics(&patch.machines) {
                    control.set_machine_metrics(&machine.id, metrics);
                }
            }
            _ => {}
        }
    }
}

async fn fetch_launch_options(control: &ControlPlane, machine: &Arc<RemoteMachine>) {
    match machine
        .get_json::<LaunchOptions>("/api/v1/launch-options")
        .await
    {
        Ok(options) => control.set_machine_launch_options(&machine.id, options),
        Err(error) => control.set_machine_launch_note(&machine.id, &format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{MachineKind, MachineSummary};

    fn decode(chunks: &[&str]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for chunk in chunks {
            decoder.push(chunk.as_bytes(), &mut events).unwrap();
        }
        events
    }

    fn summary(machine: &str, pane: &str, name: &str) -> SessionSummary {
        SessionSummary {
            id: format!("{machine}~{pane}"),
            machine: machine.to_owned(),
            name: name.to_owned(),
            pane_id: pane.to_owned(),
            status: "working".to_owned(),
            agent: "codex".to_owned(),
            profile: "Default".to_owned(),
            attached: false,
            activity: 1,
            path: "/tmp".to_owned(),
            title: name.to_owned(),
            command: "codex".to_owned(),
            launch_command: "codex".to_owned(),
            systemd_scope: None,
            memory_max_bytes: None,
            windows: 1,
            window_index: 0,
            pane_index: 0,
            content_hash: "0000000000000001".to_owned(),
        }
    }

    fn machine_summary(id: &str, kind: MachineKind) -> MachineSummary {
        MachineSummary {
            id: id.to_owned(),
            label: id.to_owned(),
            kind,
            online: true,
            sessions: 0,
            health: None,
            last_seen_ms: None,
            address: None,
            metrics: crate::metrics::MachineMetrics::default(),
        }
    }

    #[test]
    fn sse_decoder_joins_split_chunks_comments_and_multiline_data() {
        let events = decode(&[
            ": keep-alive\n\n",
            "event: sessions.sna",
            "pshot\nid: 4\ndata: {\"a\":1,\n",
            "data: \"b\":2}\n\n",
            "event: sessions.patch\r\ndata: {}\r\n\r\n",
        ]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "sessions.snapshot");
        assert_eq!(events[0].data, "{\"a\":1,\n\"b\":2}");
        assert_eq!(events[1].name, "sessions.patch");
        assert_eq!(events[1].data, "{}");
    }

    #[test]
    fn sse_decoder_survives_utf8_split_across_chunks() {
        let payload = "event: sessions.snapshot\ndata: 🍰\n\n".as_bytes();
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for byte in payload {
            decoder.push(&[*byte], &mut events).unwrap();
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "🍰");
    }

    #[test]
    fn sse_decoder_rejects_an_unbounded_event() {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        let chunk = "x".repeat(64 * 1024);
        let mut error = None;
        for _ in 0..64 {
            if let Err(failure) = decoder.push(chunk.as_bytes(), &mut events) {
                error = Some(failure);
                break;
            }
        }
        assert!(error.is_some(), "an unbounded event must be rejected");
        assert!(events.is_empty());
    }

    #[test]
    fn backoff_is_bounded_and_monotonic() {
        assert_eq!(backoff_delay(0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1), Duration::from_millis(500));
        assert_eq!(backoff_delay(2), Duration::from_secs(1));
        assert_eq!(backoff_delay(3), Duration::from_secs(2));
        let mut previous = Duration::ZERO;
        for failures in 0..1_000 {
            let delay = backoff_delay(failures);
            assert!(delay >= previous, "backoff must not decrease");
            assert!(
                delay <= Duration::from_secs(30),
                "backoff must stay bounded"
            );
            previous = delay;
        }
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn a_healthy_snapshot_restores_the_base_reconnect_delay() {
        let mut state = ReconnectState::default();
        assert_eq!(state.record_failure(), Duration::from_millis(500));
        assert_eq!(state.record_failure(), Duration::from_secs(1));
        assert_eq!(state.record_failure(), Duration::from_secs(2));
        assert_eq!(state.failures(), 3);

        // A node that streams a valid snapshot is healthy again, so the next
        // failure must start over at the base delay instead of the ceiling.
        state.record_healthy();
        assert_eq!(state.failures(), 0);
        assert_eq!(state.record_failure(), Duration::from_millis(500));

        // A node that never snapshots still escalates to the ceiling.
        let mut cold = ReconnectState::default();
        for _ in 0..64 {
            cold.record_failure();
        }
        assert_eq!(cold.record_failure(), Duration::from_secs(30));
        cold.record_healthy();
        assert_eq!(cold.record_failure(), Duration::from_millis(500));
    }

    #[test]
    fn mirror_rekeys_sessions_and_refuses_nested_federation() {
        let overview = Overview {
            revision: 3,
            sessions: vec![
                summary("local", "%1", "alpha"),
                summary("third-party", "%1", "collides"),
            ],
            health: None,
            machines: vec![
                machine_summary("local", MachineKind::Local),
                machine_summary("third-party", MachineKind::Remote),
            ],
        };
        let mut mirror = RemoteMirror::default();
        mirror.snapshot("gpu-box", &overview);
        let sessions = mirror.sessions();
        assert_eq!(
            sessions.len(),
            1,
            "a node's own remotes must not be re-federated"
        );
        assert_eq!(sessions[0].id, "gpu-box~%1");
        assert_eq!(sessions[0].machine, "gpu-box");
        assert_eq!(sessions[0].name, "alpha");
    }

    #[test]
    fn mirror_applies_incremental_patches() {
        let overview = Overview {
            revision: 1,
            sessions: vec![
                summary("local", "%1", "alpha"),
                summary("local", "%2", "beta"),
            ],
            health: None,
            machines: vec![machine_summary("local", MachineKind::Local)],
        };
        let mut mirror = RemoteMirror::default();
        mirror.snapshot("gpu-box", &overview);
        assert_eq!(mirror.sessions().len(), 2);

        let mut changed = summary("local", "%2", "beta");
        changed.status = "waiting".to_owned();
        assert!(mirror.patch(
            "gpu-box",
            &OverviewPatch {
                base_revision: 1,
                revision: 2,
                upsert: vec![changed, summary("local", "%3", "gamma")],
                remove: vec!["local~%1".to_owned()],
                health: None,
                machines: Vec::new(),
            },
        ));
        assert_eq!(mirror.revision(), Some(2));
        let sessions = mirror.sessions();
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["gpu-box~%2", "gpu-box~%3"]
        );
        assert_eq!(sessions[0].status, "waiting");
    }

    #[test]
    fn mirror_ignores_nested_federation_removals_with_colliding_pane_ids() {
        let overview = Overview {
            revision: 1,
            sessions: vec![
                summary("local", "%1", "alpha"),
                summary("local", "%4", "must-survive"),
                summary("third-party", "%4", "nested"),
            ],
            health: None,
            machines: vec![
                machine_summary("local", MachineKind::Local),
                machine_summary("third-party", MachineKind::Remote),
            ],
        };
        let mut mirror = RemoteMirror::default();
        mirror.snapshot("gpu-box", &overview);
        assert_eq!(mirror.sessions().len(), 2);

        assert!(mirror.patch(
            "gpu-box",
            &OverviewPatch {
                base_revision: 1,
                revision: 2,
                upsert: Vec::new(),
                remove: vec!["third-party~%4".to_owned()],
                health: None,
                machines: overview.machines,
            },
        ));

        assert_eq!(
            mirror
                .sessions()
                .iter()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "must-survive"]
        );
    }

    fn patch_at(base_revision: u64, revision: u64, pane: &str) -> OverviewPatch {
        OverviewPatch {
            base_revision,
            revision,
            upsert: vec![summary("local", pane, "late")],
            remove: Vec::new(),
            health: None,
            machines: Vec::new(),
        }
    }

    #[test]
    fn mirror_discards_a_patch_that_arrives_before_any_snapshot() {
        let mut mirror = RemoteMirror::default();
        assert_eq!(mirror.revision(), None);
        assert!(
            !mirror.patch("gpu-box", &patch_at(0, 1, "%1")),
            "a patch without a snapshot must never be merged"
        );
        assert!(mirror.sessions().is_empty());
        assert_eq!(mirror.revision(), None);
    }

    #[test]
    fn mirror_discards_a_patch_built_on_a_stale_base_revision() {
        let overview = Overview {
            revision: 7,
            sessions: vec![summary("local", "%1", "alpha")],
            health: None,
            machines: vec![machine_summary("local", MachineKind::Local)],
        };
        let mut mirror = RemoteMirror::default();
        mirror.snapshot("gpu-box", &overview);
        assert_eq!(mirror.revision(), Some(7));

        // A patch from before the snapshot, and one that skips a revision, are
        // both rejected so the caller resyncs instead of merging a gap.
        assert!(!mirror.patch("gpu-box", &patch_at(6, 8, "%2")));
        assert!(!mirror.patch("gpu-box", &patch_at(9, 10, "%3")));
        assert_eq!(mirror.revision(), Some(7));
        assert_eq!(
            mirror
                .sessions()
                .iter()
                .map(|session| session.id.clone())
                .collect::<Vec<_>>(),
            ["gpu-box~%1"]
        );

        // The matching continuation still applies.
        assert!(mirror.patch("gpu-box", &patch_at(7, 8, "%2")));
        assert_eq!(mirror.revision(), Some(8));
        assert_eq!(mirror.sessions().len(), 2);
    }

    #[test]
    fn mirror_accepts_a_node_that_reports_no_machines() {
        let overview = Overview {
            revision: 1,
            sessions: vec![SessionSummary {
                machine: String::new(),
                ..summary("", "%9", "legacy")
            }],
            health: None,
            machines: Vec::new(),
        };
        let mut mirror = RemoteMirror::default();
        mirror.snapshot("gpu-box", &overview);
        assert_eq!(mirror.sessions()[0].id, "gpu-box~%9");
    }

    #[test]
    fn pane_ids_are_percent_encoded_for_transport() {
        assert_eq!(encode_segment("%7"), "%257");
        assert_eq!(encode_segment("release-review_2"), "release-review_2");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("a b"), "a%20b");
        assert_eq!(encode_segment("../etc"), "..%2Fetc");
    }

    #[test]
    fn remote_error_detail_surfaces_a_node_message() {
        let body = Bytes::from_static(br#"{"error":"no agent pane matches %9"}"#);
        assert_eq!(remote_error_detail(&body), ": no agent pane matches %9");
        assert_eq!(remote_error_detail(&Bytes::from_static(b"not json")), "");
    }

    #[test]
    fn unauthorized_status_explains_the_likely_cause() {
        let error = check_status("gpu-box", "/api/v1/events", StatusCode::UNAUTHORIZED)
            .unwrap_err()
            .to_string();
        assert!(error.contains("token"));
        assert!(error.contains("allowed-host"));
        assert!(check_status("gpu-box", "/x", StatusCode::OK).is_ok());
    }
}
