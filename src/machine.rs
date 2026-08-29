//! Machine identity, node URLs, and credential handling for federation.
//!
//! atmux federates *live state* rather than copying tmux processes. One
//! coordinator owns browser and MCP access; every configured remote node is
//! reached only through its own HTTP API, and browsers never learn a node URL
//! credential.

use std::{
    fmt, fs,
    net::IpAddr,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::metrics::MachineMetrics;

/// Identifier used for the coordinator's own tmux server when the operator did
/// not choose one. Keeping this stable preserves pre-federation session ids.
pub const LOCAL_MACHINE_ID: &str = "local";

/// Separator between a machine id and a tmux pane id inside a composite id.
///
/// `~` is an unreserved URL character, so composite ids survive
/// `encodeURIComponent`, axum path decoding, and reverse proxies that
/// aggressively normalize `%2F`.
pub const COMPOSITE_SEPARATOR: char = '~';

const MAX_MACHINE_ID_LEN: usize = 32;
const MAX_MACHINE_LABEL_LEN: usize = 64;

/// A bearer token held in memory. `Debug` is redacted so credentials cannot
/// reach logs through a derived `Debug` on any containing struct.
#[derive(Clone)]
pub struct Secret(Arc<str>);

impl Secret {
    #[must_use]
    pub fn new(value: &str) -> Self {
        Self(Arc::from(value))
    }

    /// Returns the raw credential. Only the outbound HTTP client may call this.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(redacted)")
    }
}

/// Validates a machine identifier.
///
/// # Errors
///
/// Returns an error when the id is empty, too long, reserved, or contains
/// characters that would make a composite id ambiguous.
pub fn validate_machine_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_MACHINE_ID_LEN {
        bail!("machine id must contain 1 to {MAX_MACHINE_ID_LEN} characters: {id:?}");
    }
    if !id.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '_')
    }) {
        bail!("machine id may contain only lowercase letters, digits, '-' and '_': {id:?}");
    }
    if !id
        .starts_with(|character: char| character.is_ascii_lowercase() || character.is_ascii_digit())
    {
        bail!("machine id must start with a lowercase letter or digit: {id:?}");
    }
    Ok(())
}

/// Validates a human-facing machine label.
///
/// # Errors
///
/// Returns an error for an empty, oversized, or control-character label.
pub fn validate_machine_label(label: &str) -> Result<()> {
    if label.trim().is_empty() || label.len() > MAX_MACHINE_LABEL_LEN {
        bail!("machine label must contain 1 to {MAX_MACHINE_LABEL_LEN} characters");
    }
    if label.chars().any(char::is_control) {
        bail!("machine label may not contain control characters");
    }
    Ok(())
}

/// Builds the stable composite identity for one pane on one machine.
#[must_use]
pub fn composite_id(machine: &str, pane_id: &str) -> String {
    format!("{machine}{COMPOSITE_SEPARATOR}{pane_id}")
}

/// Splits a composite identity into its machine and pane halves.
///
/// Returns `None` for any value that is not shaped like `machine~pane`, which
/// lets callers fall back to bare pane-id or session-name lookup.
#[must_use]
pub fn split_composite(id: &str) -> Option<(&str, &str)> {
    let (machine, pane) = id.split_once(COMPOSITE_SEPARATOR)?;
    if pane.is_empty() || validate_machine_id(machine).is_err() {
        return None;
    }
    Some((machine, pane))
}

/// Milliseconds since the Unix epoch, saturating instead of panicking.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// A validated `http://` or `https://` base address for a remote node.
///
/// See [`NodeUrl::parse`] for the accepted shapes.
///
#[derive(Clone, PartialEq, Eq)]
pub struct NodeUrl {
    secure: bool,
    host: String,
    port: u16,
    prefix: String,
    bracketed: bool,
}

impl NodeUrl {
    /// Parses and validates a configured node URL.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-HTTP(S) scheme, embedded credentials, a query or
    /// fragment, a malformed authority, or a suspicious path prefix.
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            bail!("machine url must not be empty");
        }
        let lowered = value.to_ascii_lowercase();
        let (secure, rest) = if lowered.starts_with("https://") {
            (true, &value["https://".len()..])
        } else if lowered.starts_with("http://") {
            (false, &value["http://".len()..])
        } else {
            bail!("machine url must start with http:// or https://: {value:?}");
        };
        if rest.contains('@') {
            bail!("machine url must not embed credentials; use token_env or token_file");
        }
        if rest.contains('?') || rest.contains('#') {
            bail!("machine url must not contain a query or fragment");
        }
        if rest.contains(char::is_whitespace) {
            bail!("machine url must not contain whitespace");
        }
        let (authority, path) = rest
            .split_once('/')
            .map_or((rest, ""), |(authority, path)| (authority, path));
        let (host, port, bracketed) = parse_authority(authority, secure)?;
        let prefix = normalize_prefix(path)?;
        Ok(Self {
            secure,
            host,
            port,
            prefix,
            bracketed,
        })
    }

    /// Host and port exactly as they must appear in an HTTP `Host` header.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.bracketed {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Hostname without IPv6 brackets, suitable for a TCP connect.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Whether this endpoint uses certificate-validating HTTPS.
    #[must_use]
    pub const fn is_https(&self) -> bool {
        self.secure
    }

    /// Whether the endpoint is confined to this host. Plain HTTP is retained
    /// only for loopback test fixtures and local development; federation over
    /// any network interface must use HTTPS.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    }

    /// Builds an origin-form request target below the configured path prefix.
    #[must_use]
    pub fn request_target(&self, path_and_query: &str) -> String {
        format!("{}{path_and_query}", self.prefix)
    }

    /// Builds an absolute request URL for clients that own the TLS transport.
    #[must_use]
    pub fn request_url(&self, path_and_query: &str) -> String {
        format!(
            "{}{}/{}",
            if self.secure { "https://" } else { "http://" },
            self.authority(),
            self.request_target(path_and_query).trim_start_matches('/')
        )
    }
}

impl fmt::Display for NodeUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}://{}{}",
            if self.secure { "https" } else { "http" },
            self.authority(),
            self.prefix
        )
    }
}

impl fmt::Debug for NodeUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "NodeUrl({self})")
    }
}

fn parse_authority(authority: &str, secure: bool) -> Result<(String, u16, bool)> {
    if authority.is_empty() {
        bail!("machine url is missing a host");
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .context("machine url has an unterminated IPv6 literal")?;
        if host.is_empty()
            || !host
                .chars()
                .all(|character| character.is_ascii_hexdigit() || matches!(character, ':' | '.'))
        {
            bail!("machine url has an invalid IPv6 literal");
        }
        let port = parse_port(tail.strip_prefix(':'), secure)?;
        return Ok((host.to_ascii_lowercase(), port, true));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, parse_port(Some(port), secure)?)
        }
        _ => (authority, if secure { 443 } else { 80 }),
    };
    validate_reg_name(host)?;
    Ok((host.to_ascii_lowercase(), port, false))
}

fn parse_port(port: Option<&str>, secure: bool) -> Result<u16> {
    let Some(port) = port else {
        return Ok(if secure { 443 } else { 80 });
    };
    let parsed: u16 = port
        .parse()
        .with_context(|| format!("machine url has an invalid port: {port:?}"))?;
    if parsed == 0 {
        bail!("machine url port must be 1 to 65535");
    }
    Ok(parsed)
}

fn validate_reg_name(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("machine url is missing a host");
    }
    if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        bail!("machine url host is malformed: {host:?}");
    }
    if !host
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_'))
    {
        bail!("machine url host contains unsupported characters: {host:?}");
    }
    Ok(())
}

fn normalize_prefix(path: &str) -> Result<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.contains("..") || trimmed.contains("//") {
        bail!("machine url path prefix is malformed: {path:?}");
    }
    if !trimmed.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_' | '/')
    }) {
        bail!("machine url path prefix contains unsupported characters: {path:?}");
    }
    Ok(format!("/{trimmed}"))
}

/// Reads a bearer token from an environment variable or a file.
///
/// The value is never echoed in an error message.
///
/// # Errors
///
/// Returns an error when both sources are configured, the variable is unset or
/// empty, or the file cannot be read.
pub fn resolve_token(
    machine: &str,
    token_env: Option<&str>,
    token_file: Option<&Path>,
) -> Result<Option<Secret>> {
    resolve_token_with(machine, token_env, token_file, |name| {
        std::env::var(name).ok()
    })
}

fn resolve_token_with(
    machine: &str,
    token_env: Option<&str>,
    token_file: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<Secret>> {
    match (token_env, token_file) {
        (Some(_), Some(_)) => {
            bail!("machine {machine} sets both token_env and token_file; choose one")
        }
        (Some(variable), None) => {
            let value = lookup(variable).with_context(|| {
                format!("machine {machine} token environment variable {variable} is not set")
            })?;
            let value = value.trim();
            if value.is_empty() {
                bail!("machine {machine} token environment variable {variable} is empty");
            }
            Ok(Some(Secret::new(value)))
        }
        (None, Some(path)) => {
            let value = fs::read_to_string(path)
                .with_context(|| format!("machine {machine} token file could not be read"))?;
            let value = value.trim();
            if value.is_empty() {
                bail!("machine {machine} token file is empty");
            }
            Ok(Some(Secret::new(value)))
        }
        (None, None) => Ok(None),
    }
}

/// Whether one machine is the coordinator's own tmux server or a remote node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MachineKind {
    Local,
    Remote,
}

/// Browser and MCP view of one federated machine. Contains no credentials.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MachineSummary {
    pub id: String,
    pub label: String,
    pub kind: MachineKind,
    pub online: bool,
    pub sessions: usize,
    /// Human-readable reason a machine is degraded or offline.
    pub health: Option<String>,
    /// Epoch milliseconds of the last successful contact.
    pub last_seen_ms: Option<u64>,
    /// Credential-free `host:port` of a remote node, for operator diagnostics.
    pub address: Option<String>,
    /// Resource telemetry sampled by the machine that owns this tmux server.
    #[serde(default)]
    pub metrics: MachineMetrics,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_ids_reject_separators_and_shouting() {
        assert!(validate_machine_id("gpu-box_2").is_ok());
        assert!(validate_machine_id(LOCAL_MACHINE_ID).is_ok());
        assert!(validate_machine_id("GPU").is_err());
        assert!(validate_machine_id("has~separator").is_err());
        assert!(validate_machine_id("has/slash").is_err());
        assert!(validate_machine_id("-leading").is_err());
        assert!(validate_machine_id("").is_err());
        assert!(validate_machine_id(&"a".repeat(33)).is_err());
    }

    #[test]
    fn composite_ids_round_trip_and_reject_bare_ids() {
        let id = composite_id("gpu-box", "%17");
        assert_eq!(id, "gpu-box~%17");
        assert_eq!(split_composite(&id), Some(("gpu-box", "%17")));
        assert_eq!(split_composite("local~%3"), Some(("local", "%3")));
        // Bare tmux identities and session names must not look composite.
        assert_eq!(split_composite("%3"), None);
        assert_eq!(split_composite("release-review"), None);
        assert_eq!(split_composite("local~"), None);
        assert_eq!(split_composite("NOT-A-MACHINE~%3"), None);
    }

    #[test]
    fn node_urls_accept_private_network_addresses() {
        let url = NodeUrl::parse("http://gpu-box.tail1234.ts.net:7345").unwrap();
        assert_eq!(url.authority(), "gpu-box.tail1234.ts.net:7345");
        assert_eq!(url.host(), "gpu-box.tail1234.ts.net");
        assert_eq!(url.port(), 7345);
        assert_eq!(url.request_target("/api/v1/events"), "/api/v1/events");
        assert_eq!(url.to_string(), "http://gpu-box.tail1234.ts.net:7345");

        let defaulted = NodeUrl::parse("http://10.0.0.4").unwrap();
        assert_eq!(defaulted.port(), 80);

        let prefixed = NodeUrl::parse("http://proxy.internal:8080/atmux/").unwrap();
        assert_eq!(
            prefixed.request_target("/api/v1/health"),
            "/atmux/api/v1/health"
        );
        assert_eq!(prefixed.to_string(), "http://proxy.internal:8080/atmux");

        let ipv6 = NodeUrl::parse("http://[fd7a::1]:7345").unwrap();
        assert_eq!(ipv6.authority(), "[fd7a::1]:7345");
        assert_eq!(ipv6.host(), "fd7a::1");

        let secure = NodeUrl::parse("https://10.0.0.4").unwrap();
        assert!(secure.is_https());
        assert_eq!(secure.port(), 443);
        assert_eq!(
            secure.request_url("/api/v1/events"),
            "https://10.0.0.4:443/api/v1/events"
        );
    }

    #[test]
    fn node_urls_reject_credentials_schemes_and_open_proxy_shapes() {
        for value in [
            "",
            "gpu-box:7345",
            "ftp://gpu-box:7345",
            "file:///etc/passwd",
            "http://user:secret@gpu-box:7345",
            "http://gpu-box:7345/?redirect=http://evil",
            "http://gpu-box:7345/#fragment",
            "http://gpu-box:7345/../etc",
            "http://:7345",
            "http://gpu-box:0",
            "http://gpu-box:99999",
            "http://gpu box:7345",
            "http://[fd7a::1:7345",
            "http://.gpu-box:7345",
        ] {
            assert!(
                NodeUrl::parse(value).is_err(),
                "expected rejection: {value:?}"
            );
        }

        assert!(NodeUrl::parse("https://gpu-box:7345").is_ok());
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        #[derive(Debug)]
        struct Holder {
            #[allow(dead_code)]
            token: Option<Secret>,
        }

        let secret = Secret::new("super-secret-token");
        assert_eq!(format!("{secret:?}"), "Secret(redacted)");
        assert!(!format!("{secret:?}").contains("super-secret"));
        assert_eq!(secret.expose(), "super-secret-token");

        let rendered = format!(
            "{:?}",
            Holder {
                token: Some(Secret::new("super-secret-token")),
            }
        );
        assert!(!rendered.contains("super-secret"), "leaked: {rendered}");
    }

    #[test]
    fn tokens_resolve_from_env_or_file_and_reject_ambiguity() {
        let directory = std::env::temp_dir().join(format!("atmux-token-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("node.token");
        fs::write(&path, "  file-token\n").unwrap();
        let resolved = resolve_token("gpu-box", None, Some(path.as_path())).unwrap();
        assert_eq!(resolved.unwrap().expose(), "file-token");

        let environment =
            |name: &str| (name == "ATMUX_TEST_TOKEN").then(|| " env-token ".to_owned());
        let resolved =
            resolve_token_with("gpu-box", Some("ATMUX_TEST_TOKEN"), None, environment).unwrap();
        assert_eq!(resolved.unwrap().expose(), "env-token");

        assert!(resolve_token_with("gpu-box", Some("ATMUX_MISSING"), None, environment).is_err());
        assert!(
            resolve_token_with("gpu-box", Some("A"), Some(path.as_path()), environment).is_err()
        );
        assert!(
            resolve_token_with("gpu-box", None, None, environment)
                .unwrap()
                .is_none()
        );

        // An error must never echo the credential itself.
        fs::write(&path, "top-secret-value").unwrap();
        let blank = |_: &str| Some(String::new());
        let error = resolve_token_with("gpu-box", Some("ATMUX_TEST_TOKEN"), None, blank)
            .unwrap_err()
            .to_string();
        assert!(error.contains("is empty"));

        fs::write(&path, "   \n").unwrap();
        assert!(resolve_token("gpu-box", None, Some(path.as_path())).is_err());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn machine_summaries_never_serialize_credentials() {
        let summary = MachineSummary {
            id: "gpu-box".to_owned(),
            label: "GPU box".to_owned(),
            kind: MachineKind::Remote,
            online: true,
            sessions: 2,
            health: None,
            last_seen_ms: Some(1_700_000_000_000),
            address: Some("gpu-box.tail1234.ts.net:7345".to_owned()),
            metrics: MachineMetrics::default(),
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"kind\":\"remote\""));
        assert!(!json.contains("token"));
        assert!(!json.contains("Authorization"));
    }
}
