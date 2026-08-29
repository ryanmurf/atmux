//! Bounded provider collectors.
//!
//! Provider bodies and credentials are projected into the Pulse domain before
//! they can reach storage, logs, federation, or API responses.

use std::{
    collections::VecDeque,
    fmt,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant as MonotonicInstant, UNIX_EPOCH},
};

#[cfg(unix)]
use std::fs::OpenOptions;

use http_body_util::{BodyExt as _, Full};
use hyper::{
    Method, Request, StatusCode, Uri,
    body::{Bytes, Incoming},
    header,
};
use hyper_util::rt::TokioIo;
use rustls::{RootCertStore, pki_types::ServerName};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_rustls::TlsConnector;

use super::error::{PulseError, PulseErrorKind, PulseResult};

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod deepseek;
pub mod gemini;
pub mod grok;

const USER_AGENT: &str = concat!("atmux-pulse/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_SECRET_BYTES: usize = 16 * 1024;
const MAX_CA_BUNDLE_BYTES: usize = 4 * 1024 * 1024;

/// An external credential reference. There is intentionally no inline-secret
/// variant and resolved values never implement `Debug`, `Serialize`, or schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretRef {
    Environment { name: String },
    File { path: PathBuf },
}

impl SecretRef {
    /// Validates and resolves a bounded credential without placing it in an
    /// error, debug representation, or serializable model.
    ///
    /// # Errors
    ///
    /// Returns a configuration/authentication failure when the reference is
    /// malformed, unavailable, nonregular, symlinked, or oversized.
    pub fn resolve(&self) -> PulseResult<ResolvedSecret> {
        let value = match self {
            Self::Environment { name } => {
                validate_environment_name(name)?;
                std::env::var(name).map_err(|_| {
                    safe_error(
                        PulseErrorKind::Authentication,
                        "referenced credential is unavailable",
                    )
                })?
            }
            Self::File { path } => {
                if !path.is_absolute() {
                    return Err(PulseError::configuration(
                        "credential file reference must be absolute",
                    ));
                }
                read_regular_bounded(path, MAX_SECRET_BYTES).map_err(|_| {
                    safe_error(
                        PulseErrorKind::Authentication,
                        "referenced credential file is unavailable",
                    )
                })?
            }
        };
        validate_secret_value(&value)?;
        Ok(ResolvedSecret(value.trim().to_owned()))
    }
}

/// A resolved credential with deliberately redacted formatting.
pub struct ResolvedSecret(String);

impl ResolvedSecret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ResolvedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ResolvedSecret([redacted])")
    }
}

fn validate_environment_name(value: &str) -> PulseResult<()> {
    let mut characters = value.chars();
    let first = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    if !first || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(PulseError::configuration(
            "credential environment reference is invalid",
        ));
    }
    Ok(())
}

fn validate_secret_value(value: &str) -> PulseResult<()> {
    if value.trim().is_empty()
        || value.len() > MAX_SECRET_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(safe_error(
            PulseErrorKind::Authentication,
            "referenced credential value is invalid",
        ));
    }
    Ok(())
}

/// One bounded HTTPS result. Bodies are never included in errors.
#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub status: StatusCode,
    /// Small allowlisted response metadata used by the Anthropic inference
    /// fallback. Arbitrary provider headers (cookies, identity, tracing) never
    /// cross the transport boundary.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Minimal certificate-validating HTTP/1.1 client for fixed provider APIs.
#[derive(Clone)]
pub(crate) struct HttpsJsonClient {
    tls: TlsConnector,
    connect_timeout: Duration,
    request_timeout: Duration,
    max_response_bytes: usize,
}

impl fmt::Debug for HttpsJsonClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpsJsonClient")
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish_non_exhaustive()
    }
}

impl HttpsJsonClient {
    pub(crate) fn new(max_response_bytes: usize) -> PulseResult<Self> {
        if max_response_bytes == 0 || max_response_bytes > 1024 * 1024 {
            return Err(PulseError::configuration(
                "provider response bound must be between 1 byte and 1 MiB",
            ));
        }
        // Installation is process-global and idempotent for our purposes. If a
        // provider is already installed, rustls uses that provider below.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let roots = system_root_store()?;
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            tls: TlsConnector::from(Arc::new(config)),
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
            max_response_bytes,
        })
    }

    pub(crate) async fn request(
        &self,
        method: Method,
        endpoint: &str,
        headers: &[(&str, String)],
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> PulseResult<HttpResponse> {
        let endpoint = HttpsEndpoint::parse(endpoint)?;
        let stream = tokio::time::timeout(
            self.connect_timeout,
            TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
        )
        .await
        .map_err(|_| safe_error(PulseErrorKind::Offline, "provider connection timed out"))?
        .map_err(|_| safe_error(PulseErrorKind::Offline, "provider connection failed"))?;
        let server_name = ServerName::try_from(endpoint.host.clone()).map_err(|_| {
            PulseError::configuration("provider endpoint has an invalid TLS server name")
        })?;
        let stream =
            tokio::time::timeout(self.connect_timeout, self.tls.connect(server_name, stream))
                .await
                .map_err(|_| {
                    safe_error(PulseErrorKind::Offline, "provider TLS handshake timed out")
                })?
                .map_err(|_| {
                    safe_error(
                        PulseErrorKind::Offline,
                        "provider TLS identity was rejected",
                    )
                })?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| safe_error(PulseErrorKind::Offline, "provider HTTP handshake failed"))?;
        let _guard = ConnectionGuard(tokio::spawn(async move {
            let _ = connection.await;
        }));

        let mut request = Request::builder()
            .method(method)
            .uri(&endpoint.target)
            .header(header::HOST, &endpoint.authority)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, "application/json");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| PulseError::configuration("provider request headers are invalid"))?;
        let response = tokio::time::timeout(self.request_timeout, sender.send_request(request))
            .await
            .map_err(|_| safe_error(PulseErrorKind::Offline, "provider request timed out"))?
            .map_err(|_| safe_error(PulseErrorKind::Offline, "provider request failed"))?;
        let status = response.status();
        let headers = allowed_response_headers(response.headers());
        let body = tokio::time::timeout(
            self.request_timeout,
            collect_bounded(response.into_body(), self.max_response_bytes),
        )
        .await
        .map_err(|_| safe_error(PulseErrorKind::Upstream, "provider response timed out"))??;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

fn allowed_response_headers(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    // Only quota/reset metadata is allowed across the transport boundary.
    // Cookies, tracing values, identity headers, and arbitrary provider
    // metadata remain unavailable to collectors and logs.
    const ALLOWED: [&str; 6] = [
        "retry-after",
        "x-ratelimit-reset",
        "anthropic-ratelimit-unified-5h-utilization",
        "anthropic-ratelimit-unified-5h-reset",
        "anthropic-ratelimit-unified-7d-utilization",
        "anthropic-ratelimit-unified-7d-reset",
    ];
    ALLOWED
        .into_iter()
        .filter_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 128)
                .map(|value| (name.to_owned(), value.to_owned()))
        })
        .collect()
}

struct ConnectionGuard(JoinHandle<()>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct HttpsEndpoint {
    host: String,
    port: u16,
    authority: String,
    target: String,
}

impl HttpsEndpoint {
    fn parse(value: &str) -> PulseResult<Self> {
        let uri = value
            .parse::<Uri>()
            .map_err(|_| PulseError::configuration("provider endpoint is invalid"))?;
        if uri.scheme_str() != Some("https") {
            return Err(PulseError::configuration(
                "provider endpoint must use HTTPS",
            ));
        }
        let authority = uri
            .authority()
            .ok_or_else(|| PulseError::configuration("provider endpoint has no authority"))?;
        if authority.as_str().contains('@') {
            return Err(PulseError::configuration(
                "provider endpoint cannot embed credentials",
            ));
        }
        let host = uri
            .host()
            .ok_or_else(|| PulseError::configuration("provider endpoint has no host"))?
            .to_owned();
        let port = uri.port_u16().unwrap_or(443);
        let target = uri
            .path_and_query()
            .map_or("/", hyper::http::uri::PathAndQuery::as_str)
            .to_owned();
        Ok(Self {
            host,
            port,
            authority: authority.as_str().to_owned(),
            target,
        })
    }
}

async fn collect_bounded(mut body: Incoming, limit: usize) -> PulseResult<Vec<u8>> {
    let mut collected = Vec::with_capacity(limit.min(16 * 1024));
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| {
            safe_error(PulseErrorKind::Upstream, "provider response was unreadable")
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if collected.len().saturating_add(data.len()) > limit {
            return Err(safe_error(
                PulseErrorKind::Upstream,
                "provider response exceeded its size bound",
            ));
        }
        collected.extend_from_slice(&data);
    }
    Ok(collected)
}

fn system_root_store() -> PulseResult<RootCertStore> {
    let candidates = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/ssl/cert.pem",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/openssl/certs/ca-certificates.crt",
    ];
    for candidate in candidates {
        let Ok(bytes) =
            read_bounded_following_system_link(Path::new(candidate), MAX_CA_BUNDLE_BYTES)
        else {
            continue;
        };
        let mut roots = RootCertStore::empty();
        let certificates = rustls_pemfile::certs(&mut std::io::Cursor::new(bytes))
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let (added, _) = roots.add_parsable_certificates(certificates);
        if added > 0 {
            return Ok(roots);
        }
    }
    Err(PulseError::configuration(
        "no usable system TLS root bundle was found",
    ))
}

fn read_bounded_following_system_link(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    read_file_handle_bounded(file, limit)
}

/// Bounds for recursive discovery of collector-owned local data.
#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_field_names)]
pub(crate) struct ScanLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_duration: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct ScannedFile {
    pub path: PathBuf,
    pub size: u64,
    pub modified_ms: i64,
}

pub(crate) fn scan_regular_files(
    root: &Path,
    limits: ScanLimits,
    accept: impl Fn(&Path) -> bool,
) -> PulseResult<Vec<ScannedFile>> {
    scan_regular_files_since(root, limits, None, accept)
}

pub(crate) fn scan_regular_files_since(
    root: &Path,
    limits: ScanLimits,
    modified_since: Option<i64>,
    accept: impl Fn(&Path) -> bool,
) -> PulseResult<Vec<ScannedFile>> {
    if !root.is_absolute() {
        return Err(PulseError::configuration(
            "collector data directory must be absolute",
        ));
    }
    let root_meta = fs::symlink_metadata(root).map_err(|_| {
        safe_error(
            PulseErrorKind::NotFound,
            "collector data directory is absent",
        )
    })?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(PulseError::configuration(
            "collector data directory must be a real directory",
        ));
    }
    let started = MonotonicInstant::now();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0_usize)]);
    let mut entries_seen = 0_usize;
    let mut total_bytes = 0_u64;
    let mut files = Vec::new();
    while let Some((directory, depth)) = queue.pop_front() {
        if started.elapsed() > limits.max_duration {
            return Err(safe_error(
                PulseErrorKind::Storage,
                "collector file scan exceeded its time bound",
            ));
        }
        let entries = fs::read_dir(&directory).map_err(|_| {
            safe_error(
                PulseErrorKind::Storage,
                "collector data directory could not be read",
            )
        })?;
        for entry in entries {
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > limits.max_entries {
                return Err(safe_error(
                    PulseErrorKind::Storage,
                    "collector file scan exceeded its entry bound",
                ));
            }
            let entry = entry.map_err(|_| {
                safe_error(
                    PulseErrorKind::Storage,
                    "collector directory entry could not be read",
                )
            })?;
            let file_type = entry.file_type().map_err(|_| {
                safe_error(
                    PulseErrorKind::Storage,
                    "collector file type could not be read",
                )
            })?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if depth < limits.max_depth {
                    queue.push_back((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_file() || !accept(&path) {
                continue;
            }
            let Some(file) = inspect_scanned_file(path, limits.max_file_bytes)? else {
                continue;
            };
            if modified_since.is_some_and(|floor| file.modified_ms < floor) {
                continue;
            }
            if file.size > limits.max_file_bytes {
                return Err(safe_error(
                    PulseErrorKind::Storage,
                    "collector file exceeded its size bound",
                ));
            }
            total_bytes = total_bytes.saturating_add(file.size);
            if total_bytes > limits.max_total_bytes || files.len() >= limits.max_files {
                return Err(safe_error(
                    PulseErrorKind::Storage,
                    "collector file scan exceeded its work bound",
                ));
            }
            files.push(file);
        }
    }
    Ok(files)
}

fn inspect_scanned_file(path: PathBuf, _byte_limit: u64) -> PulseResult<Option<ScannedFile>> {
    let metadata = fs::symlink_metadata(&path).map_err(|_| {
        safe_error(
            PulseErrorKind::Storage,
            "collector file metadata could not be read",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|value| i64::try_from(value.as_millis()).ok())
        .unwrap_or(0);
    Ok(Some(ScannedFile {
        path,
        size: metadata.len(),
        modified_ms,
    }))
}

pub(crate) fn read_regular_bounded(path: &Path, limit: usize) -> std::io::Result<String> {
    let file = open_regular_bounded(path, u64::try_from(limit).unwrap_or(u64::MAX))?;
    let bytes = read_file_handle_bounded(file, limit)?;
    String::from_utf8(bytes).map_err(|_| std::io::Error::other("file is not UTF-8"))
}

pub(crate) fn open_regular_bounded(path: &Path, limit: u64) -> std::io::Result<File> {
    let expected = validate_path_without_links(path)?;
    open_regular_with_expected_metadata(path, limit, &expected)
}

fn open_regular_with_expected_metadata(
    path: &Path,
    limit: u64,
    expected: &fs::Metadata,
) -> std::io::Result<File> {
    if !expected.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    if expected.len() > limit {
        return Err(std::io::Error::other("file exceeds bound"));
    }
    let file = open_no_follow(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_file(expected, &opened) {
        return Err(std::io::Error::other(
            "file identity changed during bounded open",
        ));
    }
    if opened.len() > limit {
        return Err(std::io::Error::other("file exceeds bound"));
    }
    Ok(file)
}

fn validate_path_without_links(path: &Path) -> std::io::Result<fs::Metadata> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(std::io::Error::other(
            "collector file path must be absolute and normalized",
        ));
    }
    let mut current = PathBuf::new();
    let mut final_metadata = None;
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(
                "symbolic path component is not allowed",
            ));
        }
        final_metadata = Some(metadata);
    }
    final_metadata.ok_or_else(|| std::io::Error::other("collector file path was empty"))
}

#[cfg(unix)]
fn same_file(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    expected.dev() == opened.dev() && expected.ino() == opened.ino()
}

#[cfg(not(unix))]
fn same_file(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    expected.len() == opened.len() && expected.modified().ok() == opened.modified().ok()
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .read(true)
        .custom_flags(o_no_follow())
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("symbolic link is not allowed"));
    }
    File::open(path)
}

#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "s390x",
        target_arch = "riscv32",
        target_arch = "riscv64",
        target_arch = "mips",
        target_arch = "mips64"
    )
))]
const fn o_no_follow() -> i32 {
    0x20_000
}

#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "powerpc",
        target_arch = "powerpc64"
    )
))]
const fn o_no_follow() -> i32 {
    0x8_000
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "sparc", target_arch = "sparc64")
))]
const fn o_no_follow() -> i32 {
    0x40_000
}

#[cfg(all(target_os = "linux", target_arch = "loongarch64"))]
const fn o_no_follow() -> i32 {
    0x40_0000
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
const fn o_no_follow() -> i32 {
    0x100
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
const fn o_no_follow() -> i32 {
    // Conservative POSIX/BSD value for secondary Unix targets. Supported atmux
    // production targets use one of the explicit Linux/Apple definitions.
    0x100
}

fn read_file_handle_bounded(file: File, limit: usize) -> std::io::Result<Vec<u8>> {
    let take = u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    file.take(take).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::other("file exceeds bound"));
    }
    Ok(bytes)
}

pub(crate) fn safe_error(kind: PulseErrorKind, message: &'static str) -> PulseError {
    PulseError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_requires_https_and_forbids_embedded_credentials() {
        assert!(HttpsEndpoint::parse("http://example.test/path").is_err());
        assert!(HttpsEndpoint::parse("https://user@example.test/path").is_err());
        let endpoint = HttpsEndpoint::parse("https://example.test:8443/path?q=1").unwrap();
        assert_eq!(endpoint.host, "example.test");
        assert_eq!(endpoint.port, 8443);
        assert_eq!(endpoint.target, "/path?q=1");
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = ResolvedSecret("fixture-value".to_owned());
        let debug = format!("{secret:?}");
        assert!(!debug.contains("fixture-value"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn response_headers_are_quota_allowlisted_and_bounded() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.42".parse().unwrap(),
        );
        headers.insert("set-cookie", "secret=value".parse().unwrap());
        headers.insert(
            "anthropic-ratelimit-unified-7d-reset",
            "x".repeat(129).parse().unwrap(),
        );
        assert_eq!(
            allowed_response_headers(&headers),
            vec![
                ("retry-after".to_owned(), "30".to_owned()),
                (
                    "anthropic-ratelimit-unified-5h-utilization".to_owned(),
                    "0.42".to_owned()
                )
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_swap_cannot_escape_the_validated_tree() {
        use std::os::unix::fs::symlink;

        let nonce = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("atmux-scan-swap-{nonce}"));
        let child = root.join("child");
        let external = root.join("external");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&external).unwrap();
        let target = child.join("updates.jsonl");
        fs::write(&target, "validated").unwrap();
        fs::write(external.join("updates.jsonl"), "outside").unwrap();
        let expected = validate_path_without_links(&target).unwrap();

        fs::rename(&child, root.join("displaced-child")).unwrap();
        symlink(&external, &child).unwrap();
        assert!(open_regular_with_expected_metadata(&target, 1024, &expected).is_err());
        assert!(validate_path_without_links(&target).is_err());

        fs::remove_dir_all(root).unwrap();
    }
}
