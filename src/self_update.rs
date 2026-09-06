//! Node-local signed self-update.
//!
//! Every node downloads, verifies, and installs its own release artifact. A
//! coordinator only triggers and observes; it never hands a node a URL, a
//! path, a version, or a command. No request value may become a program, URL,
//! argument, path, or environment assignment here: the repository coordinate
//! comes from configuration and is shape-checked, every URL must be HTTPS on a
//! fixed GitHub host allow-list, the asset name is derived from the target
//! triple compiled into this binary, and the running executable is replaced
//! only after an Ed25519 signature over the release checksum document verifies
//! against a public key compiled into this binary.

use std::{
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    future::Future,
    io::Write as _,
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ed25519_dalek::{Signature, VerifyingKey};
use http_body_util::{BodyExt as _, Full};
use hyper::{
    Method, Request, StatusCode,
    body::{Bytes, Incoming},
    header,
};
use hyper_util::rt::TokioIo;
use rustls::{RootCertStore, pki_types::ServerName};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio_rustls::TlsConnector;

/// Running version, as published in release tags.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Target triple this binary was compiled for, supplied by `build.rs`.
pub const TARGET: &str = env!("ATMUX_TARGET");
/// The exact string `atmux --version` prints after the program name.
pub const VERSION_LINE: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("ATMUX_TARGET"), ")");

/// Raw 32-byte Ed25519 release-signing public key, base64.
///
/// The matching private key exists only in the `ATMUX_RELEASE_SIGNING_KEY`
/// GitHub Actions secret. Rotation means a new constant and a new release: an
/// already-installed binary never learns a new key from the network.
pub const RELEASE_PUBLIC_KEY: &str = "hvFQRGa4xvRCKeazrWPnihuxkl3+Lsr3hM4GYZ+KyRQ=";

/// Names of the two release metadata assets.
const CHECKSUM_ASSET: &str = "SHA256SUMS";
const SIGNATURE_ASSET: &str = "SHA256SUMS.sig";

/// Hosts a release download is allowed to resolve to. Any redirect off this
/// list is a hard failure rather than a followed hop.
const ALLOWED_HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const MAX_REDIRECTS: usize = 5;
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 64 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
/// Hard ceiling on a downloaded executable.
pub const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a caller's response is given to reach the browser before this
/// process is replaced.
const RESTART_DELAY: Duration = Duration::from_millis(500);
/// Manual checks are cheap but not free; this is the floor between two.
const MANUAL_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const MIN_CHECK_INTERVAL_SECONDS: u64 = 300;
const MAX_CHECK_INTERVAL_SECONDS: u64 = 7 * 24 * 60 * 60;
const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 64 * 1024;

/// Bounded self-update policy. Disabled is the safe default.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct SelfUpdateConfig {
    pub enabled: bool,
    /// GitHub `owner/name` whose releases this node trusts.
    pub repository: String,
    pub check_interval_seconds: u64,
    /// Install a verified newer release without waiting for an operator.
    pub auto_apply: bool,
    /// Base64 raw 32-byte Ed25519 key replacing [`RELEASE_PUBLIC_KEY`].
    /// Intended for tests, which sign fixtures with a throwaway keypair.
    pub public_key: Option<String>,
}

impl Default for SelfUpdateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repository: "ryanmurf/atmux".to_owned(),
            check_interval_seconds: 3600,
            auto_apply: false,
            public_key: None,
        }
    }
}

impl SelfUpdateConfig {
    /// Rejects a policy that could produce an unsafe request or an unusable key.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed repository coordinate, an interval
    /// outside the supported range, or a public key that is not 32 raw bytes.
    pub fn validate(&self) -> Result<()> {
        if !is_repository_coordinate(&self.repository) {
            bail!(
                "[self_update].repository must look like owner/name using letters, digits, '_', '.', or '-'"
            );
        }
        if self.check_interval_seconds < MIN_CHECK_INTERVAL_SECONDS
            || self.check_interval_seconds > MAX_CHECK_INTERVAL_SECONDS
        {
            bail!(
                "[self_update].check_interval_seconds must be between {MIN_CHECK_INTERVAL_SECONDS} and {MAX_CHECK_INTERVAL_SECONDS}"
            );
        }
        decode_public_key(self.public_key.as_deref().unwrap_or(RELEASE_PUBLIC_KEY))
            .context("invalid [self_update].public_key")?;
        Ok(())
    }
}

/// `^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$` without pulling in a regex engine.
#[must_use]
pub fn is_repository_coordinate(value: &str) -> bool {
    let mut parts = value.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, name].iter().all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    })
}

/// Whether this node may replace its own executable at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// This node owns its executable and can replace it.
    #[serde(rename = "self")]
    Own,
    /// The executable comes from a container image; helm rolls it forward.
    ManagedExternally,
    /// Self-update is turned off in configuration.
    Disabled,
}

/// Where the update pipeline currently is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Checking,
    Downloading,
    Verifying,
    Applying,
    Restarting,
    Failed,
}

/// Bytes transferred for the asset currently downloading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// The newest release this node has seen that is newer than what it runs.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LatestRelease {
    pub version: String,
    pub tag: String,
    pub published_at: Option<String>,
    /// True only when the checksum document's signature verified against the
    /// compiled-in key and it names an asset for this target.
    pub verified: bool,
    pub asset: String,
}

/// The executable kept alongside the running one so a rollback needs no network.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreviousRelease {
    pub version: String,
    pub path: String,
}

/// The full node-side update document, identical over the API and the CLI.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct UpdateStatus {
    pub enabled: bool,
    pub version: String,
    pub target: String,
    pub mode: Mode,
    pub latest: Option<LatestRelease>,
    pub state: Phase,
    pub progress: Option<Progress>,
    pub last_checked_at: Option<u64>,
    pub last_error: Option<String>,
    pub previous: Option<PreviousRelease>,
}

/// One of the three things a coordinator may ask a node to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Check,
    Apply,
    Rollback,
}

impl Action {
    /// Parses the fixed action vocabulary. Anything else is not an action.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "check" => Some(Self::Check),
            "apply" => Some(Self::Apply),
            "rollback" => Some(Self::Rollback),
            _ => None,
        }
    }

    /// The fixed path segment this action forwards to on the owning node.
    #[must_use]
    pub const fn segment(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Apply => "apply",
            Self::Rollback => "rollback",
        }
    }
}

/// A refusal the caller can act on, separated from an operational failure.
#[derive(Debug)]
pub struct UpdateError {
    /// True when the action simply does not apply right now (HTTP 409).
    pub conflict: bool,
    message: String,
}

impl UpdateError {
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            conflict: true,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            conflict: false,
            message: message.into(),
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UpdateError {}

/// The original argv and environment, captured before anything can mutate them.
///
/// Re-executing with exactly what the supervisor started keeps tmux, systemd,
/// and launchd looking at the same process with the same bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub argv0: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl Launch {
    /// Records this process's own launch.
    #[must_use]
    pub fn capture() -> Self {
        let mut argv = std::env::args_os();
        let argv0 = argv.next().unwrap_or_else(|| OsString::from("atmux"));
        Self {
            argv0,
            args: argv.collect(),
            env: std::env::vars_os().collect(),
        }
    }
}

/// Everything outside this module that the apply pipeline is allowed to touch.
#[derive(Clone, Debug)]
pub struct Environment {
    /// The executable this process runs from.
    pub exe: PathBuf,
    /// Durable record of what was applied and what it replaced.
    pub state_dir: PathBuf,
    pub launch: Launch,
    /// Set when the executable belongs to a container image.
    pub container: bool,
    /// Cleared in tests so the harness is not replaced by a fixture script.
    pub restart: bool,
}

impl Environment {
    /// Describes the real running process.
    ///
    /// # Errors
    ///
    /// Returns an error when the executable path or the state directory cannot
    /// be determined.
    pub fn production() -> Result<Self> {
        let exe = std::env::current_exe().context("could not locate the running atmux binary")?;
        let dirs = directories::ProjectDirs::from("dev", "ryanmurf", "atmux")
            .context("could not determine atmux state directory")?;
        let state_dir = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .join("self-update");
        Ok(Self {
            exe,
            state_dir,
            launch: Launch::capture(),
            container: in_container(),
            restart: true,
        })
    }
}

/// True when this process runs from an image it does not own.
#[must_use]
pub fn in_container() -> bool {
    container_markers(
        Path::new("/.dockerenv"),
        std::env::var_os("KUBERNETES_SERVICE_HOST"),
    )
}

fn container_markers(docker_marker: &Path, kubernetes_host: Option<OsString>) -> bool {
    docker_marker.exists() || kubernetes_host.is_some_and(|value| !value.is_empty())
}

/// Durable record of the last applied update.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct PersistedState {
    version: u32,
    applied_version: Option<String>,
    previous_version: Option<String>,
    previous_path: Option<String>,
    applied_at_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Signature and checksum verification
// ---------------------------------------------------------------------------

/// Decodes a base64 raw 32-byte Ed25519 public key.
///
/// # Errors
///
/// Returns an error when the value is not base64, is not exactly 32 bytes, or
/// is not a valid Ed25519 point.
pub fn decode_public_key(encoded: &str) -> Result<VerifyingKey> {
    let bytes = BASE64
        .decode(encoded.trim())
        .context("release public key is not valid base64")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("release public key must decode to exactly 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).context("release public key is not a valid Ed25519 key")
}

/// Verifies a base64 raw 64-byte Ed25519 signature over `message`.
///
/// Strict verification is deliberate: a malleable or small-order signature is
/// rejected rather than accepted for compatibility.
///
/// # Errors
///
/// Returns an error when the signature is not base64, is not 64 bytes, or does
/// not verify under `key`.
pub fn verify_detached(key: &VerifyingKey, message: &[u8], signature_b64: &str) -> Result<()> {
    let bytes = BASE64
        .decode(signature_b64.trim())
        .context("release signature is not valid base64")?;
    let bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| anyhow!("release signature must decode to exactly 64 bytes"))?;
    key.verify_strict(message, &Signature::from_bytes(&bytes))
        .context("release signature did not verify against the compiled-in key")
}

/// Reads the checksum for one asset out of a `sha256sum`-format document.
///
/// Lines are `<64 lowercase hex>  <name>`; the binary-mode ` *name` marker is
/// accepted too. An asset named more than once is a malformed document.
///
/// # Errors
///
/// Returns an error when the asset is absent or the document is malformed.
pub fn checksum_for(document: &str, asset: &str) -> Result<String> {
    let mut found: Option<String> = None;
    for line in document.lines() {
        let line = line.trim_end_matches(['\r']);
        if line.trim().is_empty() {
            continue;
        }
        let (digest, rest) = line
            .split_once("  ")
            .or_else(|| line.split_once(" *"))
            .ok_or_else(|| anyhow!("{CHECKSUM_ASSET} has a line that is not sha256sum format"))?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("{CHECKSUM_ASSET} has a line whose digest is not 64 hex characters");
        }
        if rest.trim() != asset {
            continue;
        }
        if found.is_some() {
            bail!("{CHECKSUM_ASSET} names {asset} more than once");
        }
        found = Some(digest.to_ascii_lowercase());
    }
    found.ok_or_else(|| anyhow!("{CHECKSUM_ASSET} has no entry for {asset}"))
}

/// The release asset name for a target triple.
#[must_use]
pub fn asset_name(target: &str) -> String {
    format!("atmux-{target}")
}

// ---------------------------------------------------------------------------
// GitHub release discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// A release this node could install, with every URL already validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub version: Version,
    pub tag: String,
    pub published_at: Option<String>,
    pub asset: String,
    asset_url: String,
    checksums_url: String,
    signature_url: String,
}

/// Chooses the release worth installing, or `None` when there is nothing newer.
///
/// A tag is a candidate only when it parses as semver after an optional `v`,
/// carries no prerelease identifier, is neither a draft nor flagged as a
/// prerelease, is strictly newer than `running`, and ships all three assets
/// this target needs.
///
/// # Errors
///
/// Returns an error when the release names an asset URL that is not HTTPS on a
/// known GitHub host.
fn select_candidate(
    release: &GithubRelease,
    running: &Version,
    target: &str,
) -> Result<Option<Candidate>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let Ok(version) = Version::parse(release.tag_name.trim_start_matches('v')) else {
        return Ok(None);
    };
    if !version.pre.is_empty() || version <= *running {
        return Ok(None);
    }
    let asset = asset_name(target);
    let Some(asset_url) = release.assets.iter().find(|item| item.name == asset) else {
        return Ok(None);
    };
    let Some(checksums) = release
        .assets
        .iter()
        .find(|item| item.name == CHECKSUM_ASSET)
    else {
        return Ok(None);
    };
    let Some(signature) = release
        .assets
        .iter()
        .find(|item| item.name == SIGNATURE_ASSET)
    else {
        return Ok(None);
    };
    Ok(Some(Candidate {
        version,
        tag: release.tag_name.clone(),
        published_at: release.published_at.clone(),
        asset,
        asset_url: validated_url(&asset_url.browser_download_url)?,
        checksums_url: validated_url(&checksums.browser_download_url)?,
        signature_url: validated_url(&signature.browser_download_url)?,
    }))
}

/// An HTTPS URL on a known GitHub host, split for the manual hyper client.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HttpsUrl {
    host: String,
    target: String,
}

fn parse_https_url(value: &str) -> Result<HttpsUrl> {
    let rest = value
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("release URLs must use https"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.contains('@') {
        bail!("release URLs must not carry credentials");
    }
    let host = match authority.split_once(':') {
        Some((host, "443")) => host,
        Some(_) => bail!("release URLs must use the default https port"),
        None => authority,
    }
    .to_ascii_lowercase();
    if !ALLOWED_HOSTS.contains(&host.as_str()) {
        bail!("release host {host} is not a trusted GitHub release host");
    }
    Ok(HttpsUrl {
        host,
        target: format!("/{path}"),
    })
}

fn validated_url(value: &str) -> Result<String> {
    parse_https_url(value)?;
    Ok(value.to_owned())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a downloaded body goes.
pub trait AssetSink: Send {
    /// Reports the announced length before the first chunk, when known.
    fn expect_total(&mut self, total: Option<u64>);
    /// Consumes one chunk, or fails to stop the transfer.
    ///
    /// # Errors
    ///
    /// Returns an error when the chunk cannot be stored or exceeds a bound.
    fn write_chunk(&mut self, chunk: &[u8]) -> Result<()>;
}

/// Bounded HTTPS reads for the update pipeline.
///
/// This is the only place a URL becomes a socket. Tests substitute an
/// in-memory implementation so the whole apply pipeline runs offline.
pub trait Fetcher: Send + Sync {
    /// Reads a small document fully into memory.
    ///
    /// # Errors
    ///
    /// Returns an error for a transport failure, a rejected status, or a body
    /// over `limit`.
    fn get<'a>(
        &'a self,
        url: &'a str,
        accept: &'static str,
        limit: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>>>;

    /// Streams a large asset into `sink`.
    ///
    /// # Errors
    ///
    /// Returns an error for a transport failure, a rejected status, or a sink
    /// that refuses a chunk.
    fn download<'a>(
        &'a self,
        url: &'a str,
        sink: &'a mut dyn AssetSink,
    ) -> BoxFuture<'a, Result<()>>;
}

struct BufferSink {
    bytes: Vec<u8>,
    limit: u64,
}

impl AssetSink for BufferSink {
    fn expect_total(&mut self, total: Option<u64>) {
        if let Some(total) = total
            && total <= self.limit
            && let Ok(total) = usize::try_from(total)
        {
            self.bytes.reserve(total);
        }
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        if self.bytes.len() as u64 + chunk.len() as u64 > self.limit {
            bail!("release document exceeds its {} byte bound", self.limit);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }
}

/// Certificate-validating HTTPS client restricted to GitHub release hosts.
pub struct HttpsFetcher {
    connector: TlsConnector,
    user_agent: String,
}

impl fmt::Debug for HttpsFetcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HttpsFetcher")
    }
}

impl HttpsFetcher {
    /// Builds a client that trusts the bundled Mozilla root program only.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable roots are available.
    pub fn new() -> Result<Self> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if roots.is_empty() {
            bail!("no trusted certificate authorities are available for release downloads");
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            user_agent: format!("atmux/{VERSION}"),
        })
    }

    async fn send(&self, url: &str, accept: &'static str, sink: &mut dyn AssetSink) -> Result<()> {
        let mut current = url.to_owned();
        for _ in 0..=MAX_REDIRECTS {
            let parsed = parse_https_url(&current)?;
            let response = self.round_trip(&parsed, accept).await?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| anyhow!("release host redirected without a location"))?;
                current = location.to_owned();
                continue;
            }
            if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
                bail!("GitHub rate limited this release check (HTTP {status})");
            }
            if !status.is_success() {
                bail!("release host answered HTTP {status}");
            }
            let total = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            sink.expect_total(total);
            return drain(response.into_body(), sink).await;
        }
        bail!("release host redirected more than {MAX_REDIRECTS} times")
    }

    async fn round_trip(
        &self,
        url: &HttpsUrl,
        accept: &'static str,
    ) -> Result<hyper::Response<Incoming>> {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((url.host.as_str(), 443)),
        )
        .await
        .with_context(|| format!("{} did not answer in time", url.host))?
        .with_context(|| format!("{} is unreachable", url.host))?;
        stream
            .set_nodelay(true)
            .context("failed to configure the release download socket")?;
        let server_name = ServerName::try_from(url.host.clone())
            .with_context(|| format!("{} is not a valid TLS server name", url.host))?;
        let stream =
            tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect(server_name, stream))
                .await
                .with_context(|| format!("{} timed out negotiating TLS", url.host))?
                .with_context(|| format!("{} presented an untrusted certificate", url.host))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .with_context(|| format!("{} rejected the HTTPS handshake", url.host))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = Request::builder()
            .method(Method::GET)
            .uri(&url.target)
            .header(header::HOST, &url.host)
            .header(header::USER_AGENT, &self.user_agent)
            .header(header::ACCEPT, accept)
            .body(Full::new(Bytes::new()))
            .context("failed to build a release request")?;
        tokio::time::timeout(HEADER_TIMEOUT, sender.send_request(request))
            .await
            .with_context(|| format!("{} timed out answering", url.host))?
            .with_context(|| format!("{} refused the release request", url.host))
    }
}

async fn drain(mut body: Incoming, sink: &mut dyn AssetSink) -> Result<()> {
    let transfer = async {
        while let Some(frame) = body.frame().await {
            let frame = frame.context("release download failed mid-transfer")?;
            if let Ok(data) = frame.into_data() {
                sink.write_chunk(&data)?;
            }
        }
        Ok(())
    };
    tokio::time::timeout(DOWNLOAD_TIMEOUT, transfer)
        .await
        .context("release download exceeded its time bound")?
}

impl Fetcher for HttpsFetcher {
    fn get<'a>(
        &'a self,
        url: &'a str,
        accept: &'static str,
        limit: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let mut sink = BufferSink {
                bytes: Vec::new(),
                limit,
            };
            self.send(url, accept, &mut sink).await?;
            Ok(sink.bytes)
        })
    }

    fn download<'a>(
        &'a self,
        url: &'a str,
        sink: &'a mut dyn AssetSink,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.send(url, "application/octet-stream", sink).await })
    }
}

/// Streams an asset to disk while hashing it and reporting progress.
struct StagingSink {
    file: File,
    digest: Sha256,
    written: u64,
    total: Option<u64>,
    shared: Arc<Mutex<Shared>>,
}

impl AssetSink for StagingSink {
    fn expect_total(&mut self, total: Option<u64>) {
        self.total = total.filter(|value| *value <= MAX_ASSET_BYTES);
        self.publish();
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.written += chunk.len() as u64;
        if self.written > MAX_ASSET_BYTES {
            bail!("release asset exceeds the {MAX_ASSET_BYTES} byte bound");
        }
        self.file
            .write_all(chunk)
            .context("failed to write the staged atmux executable")?;
        self.digest.update(chunk);
        self.publish();
        Ok(())
    }
}

impl StagingSink {
    fn publish(&self) {
        lock(&self.shared).progress = Some(Progress {
            downloaded: self.written,
            total: self.total,
        });
    }
}

// ---------------------------------------------------------------------------
// The updater
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Shared {
    phase: Phase,
    progress: Option<Progress>,
    latest: Option<LatestRelease>,
    candidate: Option<Candidate>,
    last_checked_at: Option<u64>,
    last_error: Option<String>,
    previous: Option<PreviousRelease>,
    last_manual_check: Option<Instant>,
    busy: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One node's self-update state machine.
pub struct SelfUpdater {
    config: SelfUpdateConfig,
    key: VerifyingKey,
    environment: Environment,
    /// Built on first network use so a node that never checks never needs a
    /// TLS stack, and so tests can inject a fake before anything connects.
    fetcher: OnceLock<Arc<dyn Fetcher>>,
    shared: Arc<Mutex<Shared>>,
    running: Version,
}

impl fmt::Debug for SelfUpdater {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SelfUpdater")
            .field("enabled", &self.config.enabled)
            .field("repository", &self.config.repository)
            .field("target", &TARGET)
            .field("exe", &self.environment.exe)
            .finish_non_exhaustive()
    }
}

impl SelfUpdater {
    /// Builds the updater this process will use.
    ///
    /// # Errors
    ///
    /// Returns an error when the policy is invalid or the running environment
    /// cannot be described.
    pub fn production(config: &SelfUpdateConfig) -> Result<Arc<Self>> {
        Self::with_environment(config, Environment::production()?)
    }

    /// Builds an updater over a caller-supplied environment.
    ///
    /// # Errors
    ///
    /// Returns an error when the policy is invalid or this binary's own
    /// version is not semver.
    pub fn with_environment(
        config: &SelfUpdateConfig,
        environment: Environment,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        let key = decode_public_key(config.public_key.as_deref().unwrap_or(RELEASE_PUBLIC_KEY))?;
        let running = Version::parse(VERSION).context("this binary's own version is not semver")?;
        let updater = Arc::new(Self {
            config: config.clone(),
            key,
            environment,
            fetcher: OnceLock::new(),
            shared: Arc::new(Mutex::new(Shared::default())),
            running,
        });
        updater.restore_persisted_state();
        Ok(updater)
    }

    /// Replaces the transport, so a test can drive the pipeline offline.
    #[must_use]
    pub fn with_fetcher(self: Arc<Self>, fetcher: Arc<dyn Fetcher>) -> Arc<Self> {
        let _ = self.fetcher.set(fetcher);
        self
    }

    fn fetcher(&self) -> Result<Arc<dyn Fetcher>> {
        if let Some(existing) = self.fetcher.get() {
            return Ok(existing.clone());
        }
        let built: Arc<dyn Fetcher> = Arc::new(HttpsFetcher::new()?);
        let _ = self.fetcher.set(built);
        Ok(self
            .fetcher
            .get()
            .expect("the transport was just installed")
            .clone())
    }

    /// How this node relates to its own executable.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        if self.environment.container {
            Mode::ManagedExternally
        } else if self.config.enabled {
            Mode::Own
        } else {
            Mode::Disabled
        }
    }

    /// The current update document.
    #[must_use]
    pub fn status(&self) -> UpdateStatus {
        let shared = lock(&self.shared);
        UpdateStatus {
            enabled: self.config.enabled,
            version: VERSION.to_owned(),
            target: TARGET.to_owned(),
            mode: self.mode(),
            latest: shared.latest.clone(),
            state: shared.phase,
            progress: shared.progress,
            last_checked_at: shared.last_checked_at,
            last_error: shared.last_error.clone(),
            previous: shared.previous.clone(),
        }
    }

    /// Asks GitHub what the newest usable release is.
    ///
    /// A transport or rate-limit failure is recorded as `last_error` and does
    /// not fail the call; only a refusal to act at all does.
    ///
    /// # Errors
    ///
    /// Returns a conflict when self-update is unavailable on this node or the
    /// manual check rate limit has not elapsed.
    pub async fn check(&self, manual: bool) -> Result<UpdateStatus, UpdateError> {
        self.ensure_actionable()?;
        if manual {
            let mut shared = lock(&self.shared);
            if let Some(previous) = shared.last_manual_check
                && previous.elapsed() < MANUAL_CHECK_INTERVAL
            {
                return Err(UpdateError::conflict(
                    "an update check already ran in the last 30 seconds",
                ));
            }
            shared.last_manual_check = Some(Instant::now());
        }
        {
            let mut shared = lock(&self.shared);
            if shared.busy {
                return Err(UpdateError::conflict("an update is already in progress"));
            }
            shared.phase = Phase::Checking;
        }
        let outcome = self.discover().await;
        let mut shared = lock(&self.shared);
        shared.last_checked_at = Some(now_ms());
        shared.phase = Phase::Idle;
        match outcome {
            Ok((latest, candidate)) => {
                shared.latest = latest;
                shared.candidate = candidate;
                shared.last_error = None;
            }
            Err(error) => {
                shared.last_error = Some(format!("{error:#}"));
            }
        }
        drop(shared);
        Ok(self.status())
    }

    /// Starts installing the verified candidate and answers immediately.
    ///
    /// # Errors
    ///
    /// Returns a conflict when there is nothing verified to install, when
    /// self-update is unavailable, or when an update is already running.
    pub async fn apply(self: &Arc<Self>) -> Result<UpdateStatus, UpdateError> {
        self.ensure_actionable()?;
        self.ensure_writable_directory()?;
        {
            let shared = lock(&self.shared);
            if shared.busy {
                return Err(UpdateError::conflict("an update is already in progress"));
            }
        }
        if lock(&self.shared).candidate.is_none() {
            self.check(false).await?;
        }
        let candidate = {
            let mut shared = lock(&self.shared);
            let verified = shared.latest.as_ref().is_some_and(|latest| latest.verified);
            let Some(candidate) = shared.candidate.clone().filter(|_| verified) else {
                return Err(UpdateError::conflict(format!(
                    "no verified release newer than {VERSION} is available for {TARGET}"
                )));
            };
            shared.busy = true;
            shared.phase = Phase::Downloading;
            shared.progress = None;
            shared.last_error = None;
            candidate
        };
        let updater = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = updater.install(&candidate).await;
            updater.finish(outcome);
        });
        Ok(self.status())
    }

    /// Puts the kept-aside previous executable back and restarts.
    ///
    /// # Errors
    ///
    /// Returns a conflict when there is nothing to roll back to, when
    /// self-update is unavailable, or when an update is already running.
    // The preflight is synchronous, but the three verbs share one caller-facing
    // shape so a route or the CLI can dispatch them without special cases.
    #[allow(clippy::unused_async)]
    pub async fn rollback(self: &Arc<Self>) -> Result<UpdateStatus, UpdateError> {
        self.ensure_actionable()?;
        self.ensure_writable_directory()?;
        let previous = self.previous_path();
        if !previous.is_file() {
            return Err(UpdateError::conflict(
                "this machine has no previous atmux executable to roll back to",
            ));
        }
        {
            let mut shared = lock(&self.shared);
            if shared.busy {
                return Err(UpdateError::conflict("an update is already in progress"));
            }
            shared.busy = true;
            shared.phase = Phase::Applying;
            shared.progress = None;
            shared.last_error = None;
        }
        let updater = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = updater.install_rollback(&previous).await;
            updater.finish(outcome);
        });
        Ok(self.status())
    }

    fn finish(&self, outcome: Result<()>) {
        let mut shared = lock(&self.shared);
        shared.busy = false;
        match outcome {
            Ok(()) => {
                shared.phase = Phase::Restarting;
                shared.progress = None;
            }
            Err(error) => {
                shared.phase = Phase::Failed;
                shared.progress = None;
                shared.last_error = Some(format!("{error:#}"));
            }
        }
    }

    fn ensure_actionable(&self) -> Result<(), UpdateError> {
        match self.mode() {
            Mode::Own => Ok(()),
            Mode::ManagedExternally => Err(UpdateError::conflict(
                "this atmux runs from a container image and is managed externally",
            )),
            Mode::Disabled => Err(UpdateError::conflict(
                "self-update is disabled in this machine's configuration",
            )),
        }
    }

    fn ensure_writable_directory(&self) -> Result<(), UpdateError> {
        let directory = self
            .environment
            .exe
            .parent()
            .ok_or_else(|| UpdateError::internal("the running executable has no directory"))?;
        let writable =
            fs::metadata(directory).is_ok_and(|metadata| !metadata.permissions().readonly());
        if writable {
            Ok(())
        } else {
            Err(UpdateError::conflict(format!(
                "{} is not writable, so this atmux cannot replace itself",
                directory.display()
            )))
        }
    }

    /// Fetches the release index, then the signed checksum document.
    async fn discover(&self) -> Result<(Option<LatestRelease>, Option<Candidate>)> {
        let fetcher = self.fetcher()?;
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            self.config.repository
        );
        let body = fetcher
            .get(&url, "application/vnd.github+json", MAX_METADATA_BYTES)
            .await?;
        let release: GithubRelease =
            serde_json::from_slice(&body).context("GitHub returned an unreadable release")?;
        let Some(candidate) = select_candidate(&release, &self.running, TARGET)? else {
            return Ok((None, None));
        };
        let checksums = fetcher
            .get(&candidate.checksums_url, "text/plain", MAX_CHECKSUM_BYTES)
            .await?;
        let signature = fetcher
            .get(&candidate.signature_url, "text/plain", MAX_SIGNATURE_BYTES)
            .await?;
        let signature = String::from_utf8(signature)
            .context("the release signature is not valid UTF-8 base64")?;
        let verified = verify_detached(&self.key, &checksums, &signature).and_then(|()| {
            let document =
                std::str::from_utf8(&checksums).context("the checksum document is not UTF-8")?;
            checksum_for(document, &candidate.asset)
        });
        let latest = LatestRelease {
            version: candidate.version.to_string(),
            tag: candidate.tag.clone(),
            published_at: candidate.published_at.clone(),
            verified: verified.is_ok(),
            asset: candidate.asset.clone(),
        };
        match verified {
            Ok(_) => Ok((Some(latest), Some(candidate))),
            Err(error) => {
                let latest = LatestRelease {
                    verified: false,
                    ..latest
                };
                lock(&self.shared).latest = Some(latest.clone());
                Err(error)
            }
        }
    }

    /// Downloads, verifies, swaps, records, and restarts.
    async fn install(&self, candidate: &Candidate) -> Result<()> {
        let fetcher = self.fetcher()?;
        let checksums = fetcher
            .get(&candidate.checksums_url, "text/plain", MAX_CHECKSUM_BYTES)
            .await?;
        let signature = fetcher
            .get(&candidate.signature_url, "text/plain", MAX_SIGNATURE_BYTES)
            .await?;
        let signature = String::from_utf8(signature)
            .context("the release signature is not valid UTF-8 base64")?;
        verify_detached(&self.key, &checksums, &signature)?;
        let document =
            std::str::from_utf8(&checksums).context("the checksum document is not UTF-8")?;
        let expected = checksum_for(document, &candidate.asset)?;

        let staged = self.staging_path("staged");
        let _ = fs::remove_file(&staged);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o755)
            .open(&staged)
            .with_context(|| format!("could not stage {}", staged.display()))?;
        let mut sink = StagingSink {
            file,
            digest: Sha256::new(),
            written: 0,
            total: None,
            shared: Arc::clone(&self.shared),
        };
        let outcome = fetcher.download(&candidate.asset_url, &mut sink).await;
        let digest = format!("{:x}", sink.digest.clone().finalize());
        let written = sink.written;
        drop(sink);
        if let Err(error) = outcome {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
        lock(&self.shared).phase = Phase::Verifying;
        if digest != expected {
            let _ = fs::remove_file(&staged);
            bail!(
                "the downloaded {} does not match its signed SHA-256",
                candidate.asset
            );
        }
        if written == 0 {
            let _ = fs::remove_file(&staged);
            bail!("the downloaded {} is empty", candidate.asset);
        }
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
            .context("could not make the staged atmux executable runnable")?;
        let expected_version = candidate.version.to_string();
        if let Err(error) = probe_version(&staged, Some(&expected_version)).await {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }

        lock(&self.shared).phase = Phase::Applying;
        self.swap_into_place(&staged)?;
        self.record_applied(&expected_version, VERSION)?;
        self.restart();
        Ok(())
    }

    async fn install_rollback(&self, previous: &Path) -> Result<()> {
        let restored = probe_version(previous, None).await?;
        let staging = self.staging_path("rollback");
        let _ = fs::remove_file(&staging);
        fs::rename(&self.environment.exe, &staging)
            .context("could not move the running atmux aside for a rollback")?;
        if let Err(error) = fs::rename(previous, &self.environment.exe) {
            let _ = fs::rename(&staging, &self.environment.exe);
            return Err(error).context("could not restore the previous atmux executable");
        }
        fs::rename(&staging, previous)
            .context("could not keep the rolled-back atmux for a second attempt")?;
        self.record_applied(&restored, VERSION)?;
        self.restart();
        Ok(())
    }

    /// Renames the running executable aside and moves the staged one in.
    ///
    /// Both renames are within one directory, so each is atomic and the
    /// executable is never missing or partially written.
    fn swap_into_place(&self, staged: &Path) -> Result<()> {
        let previous = self.previous_path();
        let _ = fs::remove_file(&previous);
        fs::rename(&self.environment.exe, &previous)
            .context("could not move the running atmux aside")?;
        if let Err(error) = fs::rename(staged, &self.environment.exe) {
            let _ = fs::rename(&previous, &self.environment.exe);
            return Err(error).context("could not install the staged atmux executable");
        }
        Ok(())
    }

    fn restart(&self) {
        if !self.environment.restart {
            return;
        }
        let exe = self.environment.exe.clone();
        let launch = self.environment.launch.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RESTART_DELAY).await;
            let mut command = std::process::Command::new(&exe);
            command.arg0(&launch.argv0);
            command.args(&launch.args);
            command.env_clear();
            command.envs(launch.env.iter().map(|(key, value)| (key, value)));
            // `exec` only returns on failure; on success this process image is
            // replaced and tmux, systemd, and launchd keep the same pid.
            let error = command.exec();
            eprintln!("atmux could not restart into the new executable: {error}");
        });
    }

    fn previous_path(&self) -> PathBuf {
        let mut name = self.environment.exe.clone().into_os_string();
        name.push(".prev");
        PathBuf::from(name)
    }

    fn staging_path(&self, label: &str) -> PathBuf {
        let mut name = self.environment.exe.clone().into_os_string();
        name.push(format!(".{label}-{}", std::process::id()));
        PathBuf::from(name)
    }

    fn record_applied(&self, applied: &str, replaced: &str) -> Result<()> {
        let previous = PreviousRelease {
            version: replaced.to_owned(),
            path: self.previous_path().display().to_string(),
        };
        let state = PersistedState {
            version: STATE_VERSION,
            applied_version: Some(applied.to_owned()),
            previous_version: Some(previous.version.clone()),
            previous_path: Some(previous.path.clone()),
            applied_at_ms: Some(now_ms()),
        };
        lock(&self.shared).previous = Some(previous);
        write_state(&self.environment.state_dir, &state)
    }

    fn restore_persisted_state(&self) {
        let previous = self.previous_path();
        let Ok(state) = read_state(&self.environment.state_dir) else {
            return;
        };
        if !previous.is_file() {
            return;
        }
        if let Some(version) = state.previous_version {
            lock(&self.shared).previous = Some(PreviousRelease {
                version,
                path: previous.display().to_string(),
            });
        }
    }

    /// Runs the periodic check, applying automatically when policy allows.
    pub fn spawn_background(self: &Arc<Self>) {
        if !self.config.enabled || self.environment.container {
            return;
        }
        let updater = Arc::clone(self);
        let period = Duration::from_secs(updater.config.check_interval_seconds);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                if updater.check(false).await.is_err() {
                    continue;
                }
                if !updater.config.auto_apply {
                    continue;
                }
                let ready = {
                    let shared = lock(&updater.shared);
                    shared.candidate.is_some()
                        && shared.latest.as_ref().is_some_and(|latest| latest.verified)
                };
                if ready && let Err(error) = updater.apply().await {
                    eprintln!("atmux could not start an automatic update: {error}");
                }
            }
        });
    }
}

/// Runs a staged executable's `--version` and returns the version it printed.
///
/// A staged file that cannot run, hangs, or reports a version other than the
/// one that was signed never replaces the running executable.
async fn probe_version(path: &Path, expected: Option<&str>) -> Result<String> {
    let output = tokio::time::timeout(
        VERSION_PROBE_TIMEOUT,
        tokio::process::Command::new(path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .with_context(|| format!("{} did not answer --version in time", path.display()))?
    .with_context(|| format!("{} could not be run", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} exited with {} for --version",
            path.display(),
            output.status
        );
    }
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if let Some(expected) = expected
        && !printed.contains(expected)
    {
        bail!("the staged atmux reported {printed:?} instead of version {expected}");
    }
    Ok(printed)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn write_state(directory: &Path, state: &PersistedState) -> Result<()> {
    crate::auto_update::secure_owner_directory(directory)
        .context("could not prepare the self-update state directory")?;
    let path = directory.join("state.json");
    let temporary = directory.join(format!("state.json.{}", std::process::id()));
    let encoded = serde_json::to_vec_pretty(state).context("could not encode self-update state")?;
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .context("could not open the self-update state file")?;
        file.write_all(&encoded)
            .context("could not write the self-update state file")?;
        file.sync_all()
            .context("could not flush the self-update state file")?;
    }
    fs::rename(&temporary, &path).context("could not replace the self-update state file")
}

fn read_state(directory: &Path) -> Result<PersistedState> {
    let path = directory.join("state.json");
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_STATE_BYTES
    {
        bail!("self-update state file is unsafe");
    }
    let bytes = fs::read(&path)?;
    serde_json::from_slice(&bytes).context("self-update state file is unreadable")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    fn test_keypair(seed: u8) -> (SigningKey, String) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let public = BASE64.encode(signing.verifying_key().to_bytes());
        (signing, public)
    }

    fn sign(signing: &SigningKey, message: &[u8]) -> String {
        BASE64.encode(signing.sign(message).to_bytes())
    }

    #[test]
    fn embedded_release_key_decodes_to_thirty_two_bytes() {
        let raw = BASE64.decode(RELEASE_PUBLIC_KEY).unwrap();
        assert_eq!(raw.len(), 32);
        decode_public_key(RELEASE_PUBLIC_KEY).unwrap();
    }

    #[test]
    fn version_line_carries_the_compiled_target() {
        assert_eq!(VERSION_LINE, format!("{VERSION} ({TARGET})"));
        assert!(!TARGET.is_empty());
    }

    #[test]
    fn a_fresh_keypair_verifies_its_own_signature() {
        let (signing, public) = test_keypair(7);
        let key = decode_public_key(&public).unwrap();
        let message = b"deadbeef  atmux-x86_64-unknown-linux-gnu\n";
        verify_detached(&key, message, &sign(&signing, message)).unwrap();
    }

    #[test]
    fn a_tampered_document_fails_verification() {
        let (signing, public) = test_keypair(9);
        let key = decode_public_key(&public).unwrap();
        let signature = sign(&signing, b"original checksums");
        assert!(verify_detached(&key, b"tampered checksums", &signature).is_err());
    }

    #[test]
    fn a_signature_from_another_key_fails_verification() {
        let (signing, _) = test_keypair(11);
        let (_, other_public) = test_keypair(12);
        let key = decode_public_key(&other_public).unwrap();
        let message = b"checksums";
        assert!(verify_detached(&key, message, &sign(&signing, message)).is_err());
    }

    #[test]
    fn a_short_signature_is_rejected_before_verification() {
        let (_, public) = test_keypair(13);
        let key = decode_public_key(&public).unwrap();
        assert!(verify_detached(&key, b"checksums", &BASE64.encode([0_u8; 32])).is_err());
    }

    #[test]
    fn a_public_key_of_the_wrong_length_is_rejected() {
        assert!(decode_public_key(&BASE64.encode([0_u8; 31])).is_err());
        assert!(decode_public_key("not base64!!").is_err());
    }

    #[test]
    fn checksums_are_parsed_per_asset() {
        let document = concat!(
            "1111111111111111111111111111111111111111111111111111111111111111  atmux-aarch64-apple-darwin\n",
            "2222222222222222222222222222222222222222222222222222222222222222  atmux-x86_64-unknown-linux-gnu\n",
            "\n",
        );
        assert_eq!(
            checksum_for(document, "atmux-x86_64-unknown-linux-gnu").unwrap(),
            "2".repeat(64)
        );
        assert!(checksum_for(document, "atmux-aarch64-unknown-linux-gnu").is_err());
    }

    #[test]
    fn binary_mode_checksum_lines_are_accepted_and_bad_ones_are_not() {
        let binary = "3333333333333333333333333333333333333333333333333333333333333333 *atmux-x86_64-apple-darwin\n";
        assert_eq!(
            checksum_for(binary, "atmux-x86_64-apple-darwin").unwrap(),
            "3".repeat(64)
        );
        assert!(
            checksum_for(
                "nothexdigits  atmux-x86_64-apple-darwin",
                "atmux-x86_64-apple-darwin"
            )
            .is_err()
        );
        assert!(checksum_for("no-separator-here", "atmux-x86_64-apple-darwin").is_err());
        let duplicated = format!("{}{}", "4".repeat(64) + "  a\n", "5".repeat(64) + "  a\n");
        assert!(checksum_for(&duplicated, "a").is_err());
    }

    fn release(tag: &str, prerelease: bool, assets: &[&str]) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_owned(),
            prerelease,
            draft: false,
            published_at: Some("2026-09-06T00:00:00Z".to_owned()),
            assets: assets
                .iter()
                .map(|name| GithubAsset {
                    name: (*name).to_owned(),
                    browser_download_url: format!(
                        "https://github.com/ryanmurf/atmux/releases/download/{tag}/{name}"
                    ),
                })
                .collect(),
        }
    }

    fn full_assets(target: &str) -> Vec<String> {
        vec![
            asset_name(target),
            CHECKSUM_ASSET.to_owned(),
            SIGNATURE_ASSET.to_owned(),
        ]
    }

    fn select(tag: &str, prerelease: bool, running: &str, target: &str) -> Option<Candidate> {
        let assets = full_assets(target);
        let names: Vec<&str> = assets.iter().map(String::as_str).collect();
        select_candidate(
            &release(tag, prerelease, &names),
            &Version::parse(running).unwrap(),
            target,
        )
        .unwrap()
    }

    #[test]
    fn only_newer_stable_releases_are_candidates() {
        let target = "x86_64-unknown-linux-gnu";
        assert_eq!(
            select("v0.3.0", false, "0.2.0", target).unwrap().version,
            Version::parse("0.3.0").unwrap()
        );
        assert!(select("v0.2.0", false, "0.2.0", target).is_none());
        assert!(select("v0.1.9", false, "0.2.0", target).is_none());
        assert!(select("v0.3.0", true, "0.2.0", target).is_none());
        assert!(select("v0.3.0-rc.1", false, "0.2.0", target).is_none());
        assert!(select("nightly", false, "0.2.0", target).is_none());
    }

    #[test]
    fn an_asset_for_another_target_is_not_a_candidate() {
        let assets = full_assets("aarch64-apple-darwin");
        let names: Vec<&str> = assets.iter().map(String::as_str).collect();
        let release = release("v0.3.0", false, &names);
        let running = Version::parse("0.2.0").unwrap();
        assert!(
            select_candidate(&release, &running, "x86_64-unknown-linux-gnu")
                .unwrap()
                .is_none()
        );
        let chosen = select_candidate(&release, &running, "aarch64-apple-darwin")
            .unwrap()
            .unwrap();
        assert_eq!(chosen.asset, "atmux-aarch64-apple-darwin");
    }

    #[test]
    fn a_release_missing_its_signature_is_not_a_candidate() {
        let release = release(
            "v0.3.0",
            false,
            &["atmux-x86_64-unknown-linux-gnu", "SHA256SUMS"],
        );
        assert!(
            select_candidate(
                &release,
                &Version::parse("0.2.0").unwrap(),
                "x86_64-unknown-linux-gnu"
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn asset_urls_must_be_https_on_a_github_host() {
        parse_https_url("https://objects.githubusercontent.com/a/b?c=d").unwrap();
        parse_https_url("https://api.github.com:443/repos/x/y").unwrap();
        assert!(parse_https_url("http://github.com/a").is_err());
        assert!(parse_https_url("https://evil.example/a").is_err());
        assert!(parse_https_url("https://github.com:8443/a").is_err());
        assert!(parse_https_url("https://user@github.com/a").is_err());
        let mut release = release("v0.3.0", false, &[]);
        release.assets = full_assets("x86_64-unknown-linux-gnu")
            .into_iter()
            .map(|name| GithubAsset {
                browser_download_url: format!("https://evil.example/{name}"),
                name,
            })
            .collect();
        assert!(
            select_candidate(
                &release,
                &Version::parse("0.2.0").unwrap(),
                "x86_64-unknown-linux-gnu"
            )
            .is_err()
        );
    }

    #[test]
    fn repository_coordinates_are_shape_checked() {
        assert!(is_repository_coordinate("ryanmurf/atmux"));
        assert!(is_repository_coordinate("a.b-c_d/e.f-g_h"));
        assert!(!is_repository_coordinate("ryanmurf"));
        assert!(!is_repository_coordinate("ryanmurf/atmux/extra"));
        assert!(!is_repository_coordinate("ryanmurf/"));
        assert!(!is_repository_coordinate("../../etc/passwd"));
        assert!(!is_repository_coordinate("owner/name?x=1"));
        assert!(!is_repository_coordinate("owner name/repo"));
    }

    #[test]
    fn container_markers_are_recognized() {
        let directory = tempdir("container");
        let missing = directory.join("dockerenv");
        assert!(!container_markers(&missing, None));
        assert!(!container_markers(&missing, Some(OsString::from(""))));
        assert!(container_markers(
            &missing,
            Some(OsString::from("10.0.0.1"))
        ));
        fs::write(&missing, b"").unwrap();
        assert!(container_markers(&missing, None));
    }

    #[test]
    fn configuration_bounds_are_enforced() {
        let mut config = SelfUpdateConfig::default();
        config.validate().unwrap();
        config.check_interval_seconds = 299;
        assert!(config.validate().is_err());
        config.check_interval_seconds = 300;
        config.validate().unwrap();
        config.repository = "not-a-repo".to_owned();
        assert!(config.validate().is_err());
        config.repository = "ryanmurf/atmux".to_owned();
        config.public_key = Some("short".to_owned());
        assert!(config.validate().is_err());
    }

    fn tempdir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "atmux-self-update-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn fake_binary(path: &Path, prints: &str) {
        fs::write(path, format!("#!/bin/sh\necho '{prints}'\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn a_staged_binary_must_report_the_signed_version() {
        let directory = tempdir("probe");
        let good = directory.join("good");
        fake_binary(&good, "atmux 0.3.0 (x86_64-unknown-linux-gnu)");
        assert!(
            probe_version(&good, Some("0.3.0"))
                .await
                .unwrap()
                .contains("0.3.0")
        );
        assert!(probe_version(&good, Some("0.4.0")).await.is_err());

        let broken = directory.join("broken");
        fs::write(&broken, "#!/bin/sh\nexit 3\n").unwrap();
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(probe_version(&broken, None).await.is_err());

        assert!(
            probe_version(&directory.join("absent"), None)
                .await
                .is_err()
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    fn test_environment(directory: &Path) -> Environment {
        Environment {
            exe: directory.join("atmux"),
            state_dir: directory.join("state"),
            launch: Launch::capture(),
            container: false,
            restart: false,
        }
    }

    #[test]
    fn capturing_a_launch_records_this_process_argv_and_environment() {
        let launch = Launch::capture();
        assert_eq!(Some(&launch.argv0), std::env::args_os().next().as_ref());
        assert_eq!(launch.args.len(), std::env::args_os().count() - 1);
        assert_eq!(launch.env.len(), std::env::vars_os().count());
        assert!(
            launch
                .env
                .iter()
                .any(|(key, _)| key == std::ffi::OsStr::new("PATH"))
        );
    }

    #[tokio::test]
    async fn swapping_keeps_the_running_executable_as_prev() {
        let directory = tempdir("swap");
        let environment = test_environment(&directory);
        fake_binary(&environment.exe, "atmux 0.2.0 (test)");
        let staged = directory.join("staged");
        fake_binary(&staged, "atmux 0.3.0 (test)");
        let updater = SelfUpdater::with_environment(
            &SelfUpdateConfig {
                enabled: true,
                ..SelfUpdateConfig::default()
            },
            environment.clone(),
        )
        .unwrap();
        updater.swap_into_place(&staged).unwrap();
        let previous = directory.join("atmux.prev");
        assert!(previous.is_file());
        assert!(!staged.exists());
        assert!(
            fs::read_to_string(&environment.exe)
                .unwrap()
                .contains("0.3.0")
        );
        assert!(fs::read_to_string(&previous).unwrap().contains("0.2.0"));
        updater.record_applied("0.3.0", "0.2.0").unwrap();
        let status = updater.status();
        assert_eq!(
            status.previous.unwrap().path,
            previous.display().to_string()
        );
        fs::remove_dir_all(&directory).unwrap();
    }

    /// Serves a whole release out of memory so the pipeline never opens a socket.
    struct FakeFetcher {
        documents: HashMap<String, Vec<u8>>,
    }

    impl Fetcher for FakeFetcher {
        fn get<'a>(
            &'a self,
            url: &'a str,
            _accept: &'static str,
            limit: u64,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async move {
                let body = self
                    .documents
                    .get(url)
                    .ok_or_else(|| anyhow!("fake release host has no {url}"))?;
                if body.len() as u64 > limit {
                    bail!("fake release document exceeds its bound");
                }
                Ok(body.clone())
            })
        }

        fn download<'a>(
            &'a self,
            url: &'a str,
            sink: &'a mut dyn AssetSink,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                let body = self
                    .documents
                    .get(url)
                    .ok_or_else(|| anyhow!("fake release host has no {url}"))?;
                sink.expect_total(Some(body.len() as u64));
                for chunk in body.chunks(64) {
                    sink.write_chunk(chunk)?;
                }
                Ok(())
            })
        }
    }

    struct Fixture {
        directory: PathBuf,
        updater: Arc<SelfUpdater>,
    }

    fn fixture(label: &str, tamper: bool, key_seed: u8) -> Fixture {
        let directory = tempdir(label);
        let environment = test_environment(&directory);
        fake_binary(&environment.exe, "atmux 0.2.0 (test)");
        let new_version = "99.0.0";
        let asset = asset_name(TARGET);
        let executable = format!("#!/bin/sh\necho 'atmux {new_version} ({TARGET})'\n").into_bytes();
        let digest = format!("{:x}", Sha256::digest(&executable));
        let checksums = format!("{digest}  {asset}\n");
        let (signing, public) = test_keypair(key_seed);
        let signature = sign(&signing, checksums.as_bytes());
        let base = format!("https://github.com/ryanmurf/atmux/releases/download/v{new_version}");
        let mut documents = HashMap::new();
        documents.insert(
            "https://api.github.com/repos/ryanmurf/atmux/releases/latest".to_owned(),
            serde_json::to_vec(&serde_json::json!({
                "tag_name": format!("v{new_version}"),
                "prerelease": false,
                "draft": false,
                "published_at": "2026-09-06T12:00:00Z",
                "assets": [
                    { "name": asset, "browser_download_url": format!("{base}/{asset}") },
                    { "name": CHECKSUM_ASSET, "browser_download_url": format!("{base}/{CHECKSUM_ASSET}") },
                    { "name": SIGNATURE_ASSET, "browser_download_url": format!("{base}/{SIGNATURE_ASSET}") },
                ],
            }))
            .unwrap(),
        );
        let published = if tamper {
            let mut altered = executable.clone();
            altered.extend_from_slice(b"# tampered\n");
            altered
        } else {
            executable
        };
        documents.insert(format!("{base}/{asset}"), published);
        documents.insert(format!("{base}/{CHECKSUM_ASSET}"), checksums.into_bytes());
        documents.insert(format!("{base}/{SIGNATURE_ASSET}"), signature.into_bytes());
        let updater = SelfUpdater::with_environment(
            &SelfUpdateConfig {
                enabled: true,
                public_key: Some(public),
                ..SelfUpdateConfig::default()
            },
            environment,
        )
        .unwrap()
        .with_fetcher(Arc::new(FakeFetcher { documents }));
        Fixture { directory, updater }
    }

    #[tokio::test]
    async fn the_whole_pipeline_installs_a_signed_release_without_a_network() {
        let Fixture { directory, updater } = fixture("apply", false, 21);
        let checked = updater.check(true).await.unwrap();
        let latest = checked.latest.clone().unwrap();
        assert_eq!(latest.version, "99.0.0");
        assert!(latest.verified);
        assert_eq!(latest.asset, asset_name(TARGET));
        assert_eq!(checked.mode, Mode::Own);
        assert_eq!(checked.target, TARGET);

        let candidate = lock(&updater.shared).candidate.clone().unwrap();
        updater.install(&candidate).await.unwrap();
        let exe = directory.join("atmux");
        assert!(fs::read_to_string(&exe).unwrap().contains("99.0.0"));
        assert!(
            fs::read_to_string(directory.join("atmux.prev"))
                .unwrap()
                .contains("0.2.0")
        );
        let state: PersistedState =
            serde_json::from_slice(&fs::read(directory.join("state/state.json")).unwrap()).unwrap();
        assert_eq!(state.applied_version.as_deref(), Some("99.0.0"));
        assert_eq!(state.previous_version.as_deref(), Some(VERSION));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn a_tampered_asset_never_replaces_the_running_executable() {
        let Fixture { directory, updater } = fixture("tamper", true, 22);
        updater.check(false).await.unwrap();
        let candidate = lock(&updater.shared).candidate.clone().unwrap();
        let error = updater.install(&candidate).await.unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256"));
        assert!(
            fs::read_to_string(directory.join("atmux"))
                .unwrap()
                .contains("0.2.0")
        );
        assert!(!directory.join("atmux.prev").exists());
        fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn a_release_signed_by_another_key_is_never_verified() {
        let directory = tempdir("wrongkey");
        let environment = test_environment(&directory);
        fake_binary(&environment.exe, "atmux 0.2.0 (test)");
        let Fixture {
            directory: source,
            updater: signed,
        } = fixture("wrongkey-source", false, 31);
        // The same signed release, handed to an updater that trusts a
        // different key. Nothing about the payload changes; only the key does.
        let (_, other_public) = test_keypair(32);
        let updater = SelfUpdater::with_environment(
            &SelfUpdateConfig {
                enabled: true,
                public_key: Some(other_public),
                ..SelfUpdateConfig::default()
            },
            environment,
        )
        .unwrap()
        .with_fetcher(signed.fetcher.get().unwrap().clone());
        let status = updater.check(false).await.unwrap();
        assert!(status.last_error.is_some());
        assert!(status.latest.is_none_or(|latest| !latest.verified));
        assert!(updater.apply().await.is_err());
        fs::remove_dir_all(&directory).unwrap();
        fs::remove_dir_all(&source).unwrap();
    }

    #[tokio::test]
    async fn rolling_back_restores_the_previous_executable() {
        let Fixture { directory, updater } = fixture("rollback", false, 41);
        updater.check(false).await.unwrap();
        let candidate = lock(&updater.shared).candidate.clone().unwrap();
        updater.install(&candidate).await.unwrap();
        let previous = directory.join("atmux.prev");
        updater.install_rollback(&previous).await.unwrap();
        let exe = directory.join("atmux");
        assert!(fs::read_to_string(&exe).unwrap().contains("0.2.0"));
        assert!(fs::read_to_string(&previous).unwrap().contains("99.0.0"));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn a_container_or_disabled_node_refuses_every_action() {
        let directory = tempdir("refuse");
        let mut environment = test_environment(&directory);
        fake_binary(&environment.exe, "atmux 0.2.0 (test)");
        let disabled =
            SelfUpdater::with_environment(&SelfUpdateConfig::default(), environment.clone())
                .unwrap();
        assert_eq!(disabled.mode(), Mode::Disabled);
        assert!(disabled.check(true).await.unwrap_err().conflict);
        assert!(disabled.apply().await.unwrap_err().conflict);
        assert!(disabled.rollback().await.unwrap_err().conflict);

        environment.container = true;
        let managed = SelfUpdater::with_environment(
            &SelfUpdateConfig {
                enabled: true,
                ..SelfUpdateConfig::default()
            },
            environment,
        )
        .unwrap();
        assert_eq!(managed.mode(), Mode::ManagedExternally);
        assert_eq!(managed.status().mode, Mode::ManagedExternally);
        assert!(managed.apply().await.unwrap_err().conflict);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn manual_checks_are_rate_limited() {
        let Fixture { directory, updater } = fixture("ratelimit", false, 51);
        updater.check(true).await.unwrap();
        let error = updater.check(true).await.unwrap_err();
        assert!(error.conflict);
        // A background check is never rate limited.
        updater.check(false).await.unwrap();
        fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn rollback_without_a_previous_executable_is_a_conflict() {
        let Fixture { directory, updater } = fixture("norollback", false, 61);
        assert!(updater.rollback().await.unwrap_err().conflict);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn actions_come_only_from_a_fixed_vocabulary() {
        assert_eq!(Action::parse("check"), Some(Action::Check));
        assert_eq!(Action::parse("apply"), Some(Action::Apply));
        assert_eq!(Action::parse("rollback"), Some(Action::Rollback));
        assert_eq!(Action::parse("../../etc/passwd"), None);
        assert_eq!(Action::parse("Apply"), None);
        assert_eq!(Action::Apply.segment(), "apply");
    }

    #[test]
    fn the_status_document_uses_the_published_field_names() {
        let directory = tempdir("payload");
        let environment = test_environment(&directory);
        let updater =
            SelfUpdater::with_environment(&SelfUpdateConfig::default(), environment).unwrap();
        let encoded = serde_json::to_value(updater.status()).unwrap();
        for field in [
            "enabled",
            "version",
            "target",
            "mode",
            "latest",
            "state",
            "progress",
            "last_checked_at",
            "last_error",
            "previous",
        ] {
            assert!(encoded.get(field).is_some(), "missing {field}");
        }
        assert_eq!(encoded["mode"], "disabled");
        assert_eq!(encoded["state"], "idle");
        assert_eq!(
            serde_json::to_value(Mode::Own).unwrap(),
            serde_json::Value::from("self")
        );
        assert_eq!(
            serde_json::to_value(Mode::ManagedExternally).unwrap(),
            serde_json::Value::from("managed_externally")
        );
        assert_eq!(
            serde_json::to_value(Phase::Restarting).unwrap(),
            serde_json::Value::from("restarting")
        );
        fs::remove_dir_all(&directory).unwrap();
    }
}
