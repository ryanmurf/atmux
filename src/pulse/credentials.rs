//! Secret-safe Claude and Codex credential handling.
//!
//! Claude refresh is deliberately cooperative: take the profile lock, re-read
//! the authoritative store, and adopt a sibling's newer token before deciding
//! whether a refresh grant is safe. Anthropic refresh tokens rotate, so atmux
//! spends one only when `Persist` is configured for the Linux JSON store. The
//! in-memory policy and macOS Keychain reads are adoption-only: neither can
//! durably preserve a newly rotated refresh token across a process crash.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant as MonotonicInstant, SystemTime},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    error::{PulseError, PulseErrorKind, PulseResult},
    model::RefreshPolicy,
};

const MAX_CREDENTIAL_BYTES: usize = 256 * 1024;
const EXPIRY_SKEW_MILLIS: i64 = 60_000;
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);

/// A secret string whose formatting is always redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Creates a bounded nonempty secret.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for empty, control-bearing, or
    /// unexpectedly large values.
    pub fn new(value: impl Into<String>) -> PulseResult<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 * 1024 || value.chars().any(char::is_control) {
            return Err(safe_error(
                PulseErrorKind::Authentication,
                "credential value is invalid",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([redacted])")
    }
}

/// Claude OAuth credentials projected out of the provider store.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeOauthTokens {
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    expires_at_millis: i64,
    scopes: Vec<String>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

impl ClaudeOauthTokens {
    /// Creates a validated OAuth credential value.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for malformed secrets or scope names.
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_at_millis: i64,
        scopes: Vec<String>,
    ) -> PulseResult<Self> {
        if scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 128
                || scope
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        }) {
            return Err(safe_error(
                PulseErrorKind::Authentication,
                "credential scopes are invalid",
            ));
        }
        Ok(Self {
            access_token: SecretString::new(access_token)?,
            refresh_token: refresh_token.map(SecretString::new).transpose()?,
            expires_at_millis,
            scopes,
            subscription_type: None,
            rate_limit_tier: None,
        })
    }

    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|value| value == scope)
    }

    #[must_use]
    pub const fn expires_at_millis(&self) -> i64 {
        self.expires_at_millis
    }

    #[must_use]
    pub const fn can_refresh(&self) -> bool {
        self.refresh_token.is_some()
    }

    #[must_use]
    pub fn is_expired(&self, now_millis: i64) -> bool {
        self.expires_at_millis < now_millis.saturating_add(EXPIRY_SKEW_MILLIS)
    }

    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    fn refresh_token(&self) -> Option<&SecretString> {
        self.refresh_token.as_ref()
    }

    fn with_metadata(mut self, blob: &OauthBlob) -> Self {
        self.subscription_type.clone_from(&blob.subscription_type);
        self.rate_limit_tier.clone_from(&blob.rate_limit_tier);
        self
    }
}

impl fmt::Debug for ClaudeOauthTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeOauthTokens")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at_millis", &self.expires_at_millis)
            .field("scopes", &self.scopes)
            .field("has_subscription_type", &self.subscription_type.is_some())
            .field("has_rate_limit_tier", &self.rate_limit_tier.is_some())
            .finish()
    }
}

/// Credentials required by the `ChatGPT` usage endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexCredentials {
    access_token: SecretString,
    account_id: SecretString,
}

impl CodexCredentials {
    /// Creates a redacted credential pair.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for malformed values.
    pub fn new(
        access_token: impl Into<String>,
        account_id: impl Into<String>,
    ) -> PulseResult<Self> {
        Ok(Self {
            access_token: SecretString::new(access_token)?,
            account_id: SecretString::new(account_id)?,
        })
    }

    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    pub(crate) fn account_id(&self) -> &SecretString {
        &self.account_id
    }
}

impl fmt::Debug for CodexCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexCredentials")
            .field("access_token", &"[redacted]")
            .field("account_id", &"[redacted]")
            .finish()
    }
}

/// Secret-free state exposed by credential preflight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialState {
    Ok,
    Expired,
    MissingDirectory,
    MissingStore,
    Unreadable,
    Unparseable,
    MissingAccessToken,
    MissingExpiry,
    UnsafeStore,
}

/// A credential inspection result that can safely cross APIs and logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialInspection {
    pub state: CredentialState,
    pub expires_at_millis: Option<i64>,
    pub scopes: Vec<String>,
}

/// Secret-free health of the Codex `auth.json` store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodexCredentialState {
    Ok,
    MissingDirectory,
    MissingStore,
    Unreadable,
    Unparseable,
    MissingAccessToken,
    MissingAccountId,
    UnsafeStore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexCredentialInspection {
    pub state: CodexCredentialState,
}

impl CodexCredentialInspection {
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        matches!(self.state, CodexCredentialState::Ok)
    }
}

impl CredentialInspection {
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        // An expired access token is still a usable credential identity when
        // a refresh token is present; collection will perform the bounded
        // cooperative refresh. Preflight must not repoint that profile to a
        // different identity merely because it was idle.
        matches!(self.state, CredentialState::Ok | CredentialState::Expired)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OauthBlob {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    #[serde(default)]
    scopes: Vec<String>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialPlatform {
    Linux,
    Macos,
    Other,
}

impl CredentialPlatform {
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Other
        }
    }
}

/// Bounded cooperative refresh settings, injectable for deterministic tests.
#[derive(Clone, Copy, Debug)]
pub struct RefreshOptions {
    pub platform: CredentialPlatform,
    pub lock_timeout: Duration,
}

impl Default for RefreshOptions {
    fn default() -> Self {
        Self {
            platform: CredentialPlatform::current(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }
}

/// Whether a refresh adopted a sibling value or performed a durable grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshSource {
    Adopted,
    GrantedAndPersisted,
}

/// Result of cooperative refresh; token formatting remains redacted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshResult {
    pub tokens: ClaudeOauthTokens,
    pub source: RefreshSource,
}

/// Secret return type supplied by the centralized OAuth transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshGrant {
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    pub expires_at_millis: i64,
}

impl RefreshGrant {
    /// Creates a validated transport result.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for malformed token values.
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at_millis: i64,
    ) -> PulseResult<Self> {
        Ok(Self {
            access_token: SecretString::new(access_token)?,
            refresh_token: SecretString::new(refresh_token)?,
            expires_at_millis,
        })
    }
}

/// Inspects Claude credentials without returning credential material.
#[must_use]
pub fn inspect_claude_credentials(config_dir: &Path, now_millis: i64) -> CredentialInspection {
    inspect_claude_credentials_for(config_dir, now_millis, CredentialPlatform::current())
}

/// Platform-injectable form used by cross-platform tests and doctor output.
#[must_use]
pub fn inspect_claude_credentials_for(
    config_dir: &Path,
    now_millis: i64,
    platform: CredentialPlatform,
) -> CredentialInspection {
    let default = |state| CredentialInspection {
        state,
        expires_at_millis: None,
        scopes: Vec::new(),
    };
    let directory = match ValidatedDirectory::open(config_dir) {
        Err(StoreReadError::MissingDirectory) => {
            return default(CredentialState::MissingDirectory);
        }
        Err(StoreReadError::Unreadable) => return default(CredentialState::Unreadable),
        Err(_) => return default(CredentialState::UnsafeStore),
        Ok(directory) => directory,
    };
    let document = match read_document(&directory, platform) {
        Ok(document) => document,
        Err(StoreReadError::MissingStore) => return default(CredentialState::MissingStore),
        Err(StoreReadError::Unreadable) => return default(CredentialState::Unreadable),
        Err(StoreReadError::Unparseable) => return default(CredentialState::Unparseable),
        Err(StoreReadError::UnsafeStore | StoreReadError::MissingDirectory) => {
            return default(CredentialState::UnsafeStore);
        }
    };
    let Some(blob) = oauth_blob(&document) else {
        return default(CredentialState::Unparseable);
    };
    let Some(access_token) = blob.access_token.as_deref() else {
        return CredentialInspection {
            state: CredentialState::MissingAccessToken,
            expires_at_millis: blob.expires_at,
            scopes: safe_scopes(blob.scopes),
        };
    };
    if SecretString::new(access_token).is_err() {
        return default(CredentialState::MissingAccessToken);
    }
    let Some(expires_at_millis) = blob.expires_at else {
        return CredentialInspection {
            state: CredentialState::MissingExpiry,
            expires_at_millis: None,
            scopes: safe_scopes(blob.scopes),
        };
    };
    CredentialInspection {
        state: if expires_at_millis < now_millis.saturating_add(EXPIRY_SKEW_MILLIS) {
            CredentialState::Expired
        } else {
            CredentialState::Ok
        },
        expires_at_millis: Some(expires_at_millis),
        scopes: safe_scopes(blob.scopes),
    }
}

/// Reads usable Claude credentials from the platform store.
///
/// # Errors
///
/// Returns a stable secret-free authentication or configuration error.
pub fn read_claude_credentials(config_dir: &Path) -> PulseResult<ClaudeOauthTokens> {
    read_claude_credentials_for(config_dir, CredentialPlatform::current())
}

/// Platform-injectable credential read.
///
/// # Errors
///
/// Returns a stable secret-free authentication or configuration error.
pub fn read_claude_credentials_for(
    config_dir: &Path,
    platform: CredentialPlatform,
) -> PulseResult<ClaudeOauthTokens> {
    let directory = ValidatedDirectory::open(config_dir).map_err(store_error)?;
    let document = read_document(&directory, platform).map_err(store_error)?;
    tokens_from_document(&document)
}

/// Inspects a bounded, no-follow Codex `auth.json` without returning identity
/// or token material.
#[must_use]
pub fn inspect_codex_credentials(config_dir: &Path) -> CodexCredentialInspection {
    let state = match read_codex_document(config_dir) {
        Ok(document) => match document.get("tokens").and_then(Value::as_object) {
            None => CodexCredentialState::Unparseable,
            Some(tokens)
                if tokens
                    .get("access_token")
                    .and_then(Value::as_str)
                    .is_none_or(|value| SecretString::new(value).is_err()) =>
            {
                CodexCredentialState::MissingAccessToken
            }
            Some(tokens)
                if tokens
                    .get("account_id")
                    .and_then(Value::as_str)
                    .is_none_or(|value| SecretString::new(value).is_err()) =>
            {
                CodexCredentialState::MissingAccountId
            }
            Some(_) => CodexCredentialState::Ok,
        },
        Err(StoreReadError::MissingDirectory) => CodexCredentialState::MissingDirectory,
        Err(StoreReadError::MissingStore) => CodexCredentialState::MissingStore,
        Err(StoreReadError::Unreadable) => CodexCredentialState::Unreadable,
        Err(StoreReadError::Unparseable) => CodexCredentialState::Unparseable,
        Err(StoreReadError::UnsafeStore) => CodexCredentialState::UnsafeStore,
    };
    CodexCredentialInspection { state }
}

/// Reads bounded Codex authentication from `<config>/auth.json`.
///
/// # Errors
///
/// Returns a secret-free error for missing, malformed, symlinked, replaced, or
/// oversized stores.
pub fn read_codex_credentials(config_dir: &Path) -> PulseResult<CodexCredentials> {
    let document = read_codex_document(config_dir).map_err(store_error)?;
    let tokens = document
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            safe_error(
                PulseErrorKind::Authentication,
                "codex credential store has no token record",
            )
        })?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            safe_error(
                PulseErrorKind::Authentication,
                "codex credential store has no access token",
            )
        })?;
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            safe_error(
                PulseErrorKind::Authentication,
                "codex credential store has no account identifier",
            )
        })?;
    CodexCredentials::new(access_token, account_id)
}

fn read_codex_document(config_dir: &Path) -> Result<Value, StoreReadError> {
    let directory = ValidatedDirectory::open(config_dir)?;
    read_json_child(&directory, "auth.json")
}

/// Performs a lock-first, re-read/adopt cooperative OAuth refresh.
///
/// The callback is the only transport seam: it receives the freshest refresh
/// token found under the lock. Adoption is permitted for every policy because
/// it does not spend a credential. A grant is permitted only for Linux plus
/// `RefreshPolicy::Persist`; all other combinations reject before the callback
/// can run because they cannot preserve a rotated refresh token across a
/// process crash.
///
/// # Errors
///
/// Returns a stable, secret-free error when the lock, store, grant, or atomic
/// persistence operation fails.
pub fn cooperative_refresh_with<F>(
    config_dir: &Path,
    rejected: &ClaudeOauthTokens,
    force: bool,
    policy: RefreshPolicy,
    now_millis: i64,
    options: RefreshOptions,
    mut grant: F,
) -> PulseResult<RefreshResult>
where
    F: FnMut(&SecretString) -> PulseResult<RefreshGrant>,
{
    if policy == RefreshPolicy::Persist && options.platform != CredentialPlatform::Linux {
        return Err(PulseError::configuration(
            "persistent credential refresh is supported only by the Linux JSON store",
        ));
    }
    let directory = ValidatedDirectory::open(config_dir).map_err(store_error)?;
    let _lock = CredentialLock::acquire(&directory, options.lock_timeout)?;
    let mut document = read_document(&directory, options.platform).map_err(store_error)?;
    let current = tokens_from_document(&document)?;
    let sibling_changed = current.access_token != rejected.access_token;
    if !current.is_expired(now_millis) && (!force || sibling_changed) {
        return Ok(RefreshResult {
            tokens: current,
            source: RefreshSource::Adopted,
        });
    }
    ensure_refresh_grant_is_durable(policy, options.platform)?;
    let refresh_token = current.refresh_token().ok_or_else(|| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential has no refresh capability",
        )
    })?;
    let refreshed = grant(refresh_token)?;
    let mut tokens = ClaudeOauthTokens::new(
        refreshed.access_token.expose(),
        Some(refreshed.refresh_token.expose().to_owned()),
        refreshed.expires_at_millis,
        current.scopes.clone(),
    )?;
    tokens
        .subscription_type
        .clone_from(&current.subscription_type);
    tokens.rate_limit_tier.clone_from(&current.rate_limit_tier);
    update_document_tokens(&mut document, &refreshed)?;
    persist_linux_document(&directory, &document)?;
    Ok(RefreshResult {
        tokens,
        source: RefreshSource::GrantedAndPersisted,
    })
}

fn ensure_refresh_grant_is_durable(
    policy: RefreshPolicy,
    platform: CredentialPlatform,
) -> PulseResult<()> {
    match (policy, platform) {
        (RefreshPolicy::Persist, CredentialPlatform::Linux) => Ok(()),
        (RefreshPolicy::Never, _) => Err(safe_error(
            PulseErrorKind::Authentication,
            "credential refresh is disabled",
        )),
        (RefreshPolicy::InMemory, _) => Err(PulseError::configuration(
            "rotating credential refresh requires durable Linux persistence",
        )),
        (RefreshPolicy::Persist, CredentialPlatform::Macos | CredentialPlatform::Other) => {
            Err(PulseError::configuration(
                "persistent credential refresh is supported only by the Linux JSON store",
            ))
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum StoreReadError {
    MissingDirectory,
    MissingStore,
    Unreadable,
    Unparseable,
    UnsafeStore,
}

struct ValidatedDirectory {
    path: PathBuf,
    handle: File,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    identity: FileIdentity,
}

impl ValidatedDirectory {
    fn open(path: &Path) -> Result<Self, StoreReadError> {
        if !path.is_absolute() {
            return Err(StoreReadError::UnsafeStore);
        }
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StoreReadError::MissingDirectory
            } else {
                StoreReadError::Unreadable
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreReadError::UnsafeStore);
        }
        let handle = File::open(path).map_err(|_| StoreReadError::Unreadable)?;
        let handle_metadata = handle.metadata().map_err(|_| StoreReadError::Unreadable)?;
        let current = fs::symlink_metadata(path).map_err(|_| StoreReadError::Unreadable)?;
        if !handle_metadata.is_dir()
            || current.file_type().is_symlink()
            || !same_file(&metadata, &handle_metadata)
            || !same_file(&current, &handle_metadata)
        {
            return Err(StoreReadError::UnsafeStore);
        }
        Ok(Self {
            path: path.to_path_buf(),
            handle,
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            identity: FileIdentity::from_metadata(&handle_metadata),
        })
    }

    fn child(&self, name: &str) -> Result<PathBuf, StoreReadError> {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || matches!(name, "." | "..")
        {
            return Err(StoreReadError::UnsafeStore);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            Ok(PathBuf::from(format!(
                "/proc/self/fd/{}/{}",
                self.handle.as_raw_fd(),
                name
            )))
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd as _;
            Ok(PathBuf::from(format!(
                "/dev/fd/{}/{}",
                self.handle.as_raw_fd(),
                name
            )))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.ensure_path_identity()?;
            Ok(self.path.join(name))
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn ensure_path_identity(&self) -> Result<(), StoreReadError> {
        let metadata = fs::symlink_metadata(&self.path).map_err(|_| StoreReadError::Unreadable)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || FileIdentity::from_metadata(&metadata) != self.identity
        {
            return Err(StoreReadError::UnsafeStore);
        }
        Ok(())
    }
}

fn read_document(
    directory: &ValidatedDirectory,
    platform: CredentialPlatform,
) -> Result<Value, StoreReadError> {
    match platform {
        CredentialPlatform::Macos => read_macos_document(&directory.path),
        CredentialPlatform::Linux | CredentialPlatform::Other => read_linux_document(directory),
    }
}

fn read_linux_document(directory: &ValidatedDirectory) -> Result<Value, StoreReadError> {
    read_json_child(directory, ".credentials.json")
}

#[cfg(unix)]
fn read_json_child(directory: &ValidatedDirectory, name: &str) -> Result<Value, StoreReadError> {
    use rustix::fs::{Mode, OFlags};

    validate_child_name(name)?;
    let descriptor = rustix::fs::openat(
        &directory.handle,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            StoreReadError::MissingStore
        } else if error == rustix::io::Errno::LOOP {
            StoreReadError::UnsafeStore
        } else {
            StoreReadError::Unreadable
        }
    })?;
    read_json_file(File::from(descriptor))
}

#[cfg(not(unix))]
fn read_json_child(directory: &ValidatedDirectory, name: &str) -> Result<Value, StoreReadError> {
    validate_child_name(name)?;
    directory.ensure_path_identity()?;
    let path = directory.path.join(name);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StoreReadError::MissingStore
        } else {
            StoreReadError::Unreadable
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreReadError::UnsafeStore);
    }
    let file = File::open(&path).map_err(|_| StoreReadError::Unreadable)?;
    let opened = file.metadata().map_err(|_| StoreReadError::Unreadable)?;
    let current = fs::symlink_metadata(path).map_err(|_| StoreReadError::Unreadable)?;
    if !opened.is_file()
        || current.file_type().is_symlink()
        || FileIdentity::from_metadata(&metadata) != FileIdentity::from_metadata(&opened)
        || FileIdentity::from_metadata(&current) != FileIdentity::from_metadata(&opened)
    {
        return Err(StoreReadError::UnsafeStore);
    }
    read_json_file(file)
}

fn validate_child_name(name: &str) -> Result<(), StoreReadError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || matches!(name, "." | "..") {
        return Err(StoreReadError::UnsafeStore);
    }
    Ok(())
}

fn read_json_file(mut file: File) -> Result<Value, StoreReadError> {
    let metadata = file.metadata().map_err(|_| StoreReadError::Unreadable)?;
    if !metadata.is_file()
        || metadata.len() > u64::try_from(MAX_CREDENTIAL_BYTES).unwrap_or(u64::MAX)
    {
        return Err(StoreReadError::UnsafeStore);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_CREDENTIAL_BYTES)
            .min(MAX_CREDENTIAL_BYTES),
    );
    std::io::Read::by_ref(&mut file)
        .take(u64::try_from(MAX_CREDENTIAL_BYTES + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|_| StoreReadError::Unreadable)?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return Err(StoreReadError::UnsafeStore);
    }
    serde_json::from_slice(&bytes).map_err(|_| StoreReadError::Unparseable)
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[cfg(target_os = "macos")]
fn read_macos_document(config_dir: &Path) -> Result<Value, StoreReadError> {
    use sha2::{Digest as _, Sha256};

    let digest = format!(
        "{:x}",
        Sha256::digest(config_dir.as_os_str().as_encoded_bytes())
    );
    let service = format!("Claude Code-credentials-{}", &digest[..8]);
    let account = std::env::var("USER").map_err(|_| StoreReadError::Unreadable)?;
    let output = run_bounded_child(
        Path::new("/usr/bin/security"),
        &[
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &account,
            "-w",
        ],
        MAX_CREDENTIAL_BYTES,
        Duration::from_secs(3),
    )?;
    serde_json::from_slice(&output).map_err(|_| StoreReadError::Unparseable)
}

#[cfg(not(target_os = "macos"))]
fn read_macos_document(_config_dir: &Path) -> Result<Value, StoreReadError> {
    Err(StoreReadError::MissingStore)
}

#[cfg(any(target_os = "macos", test))]
fn run_bounded_child(
    program: &Path,
    arguments: &[&str],
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>, StoreReadError> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| StoreReadError::Unreadable)?;
    let mut stdout = child.stdout.take().ok_or(StoreReadError::Unreadable)?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
        stdout
            .by_ref()
            .take(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let started = MonotonicInstant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout && !reader.is_finished() => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(if started.elapsed() >= timeout {
                    StoreReadError::Unreadable
                } else {
                    StoreReadError::UnsafeStore
                });
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(StoreReadError::Unreadable);
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| StoreReadError::Unreadable)?
        .map_err(|_| StoreReadError::Unreadable)?;
    if bytes.len() > limit {
        return Err(StoreReadError::UnsafeStore);
    }
    if !status.success() {
        return Err(StoreReadError::MissingStore);
    }
    Ok(bytes)
}

fn oauth_blob(document: &Value) -> Option<OauthBlob> {
    serde_json::from_value(document.get("claudeAiOauth")?.clone()).ok()
}

fn safe_scopes(scopes: Vec<String>) -> Vec<String> {
    scopes
        .into_iter()
        .filter(|scope| {
            !scope.is_empty()
                && scope.len() <= 128
                && scope.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-')
                })
        })
        .take(64)
        .collect()
}

fn tokens_from_document(document: &Value) -> PulseResult<ClaudeOauthTokens> {
    let blob = oauth_blob(document).ok_or_else(|| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential store has no usable OAuth record",
        )
    })?;
    let access_token = blob.access_token.clone().ok_or_else(|| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential store has no access token",
        )
    })?;
    let expires_at = blob.expires_at.ok_or_else(|| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential store has no token expiry",
        )
    })?;
    Ok(ClaudeOauthTokens::new(
        access_token,
        blob.refresh_token.clone(),
        expires_at,
        blob.scopes.clone(),
    )?
    .with_metadata(&blob))
}

fn store_error(error: StoreReadError) -> PulseError {
    match error {
        StoreReadError::MissingDirectory => safe_error(
            PulseErrorKind::NotFound,
            "credential directory is unavailable",
        ),
        StoreReadError::MissingStore => {
            safe_error(PulseErrorKind::NotFound, "credential store is unavailable")
        }
        StoreReadError::Unreadable => safe_error(
            PulseErrorKind::Authentication,
            "credential store could not be read",
        ),
        StoreReadError::Unparseable => safe_error(
            PulseErrorKind::Authentication,
            "credential store is not valid JSON",
        ),
        StoreReadError::UnsafeStore => safe_error(
            PulseErrorKind::Configuration,
            "credential store failed safety checks",
        ),
    }
}

struct CredentialLock {
    _file: File,
}

impl CredentialLock {
    fn acquire(directory: &ValidatedDirectory, timeout: Duration) -> PulseResult<Self> {
        use fs2::FileExt as _;

        let path = directory
            .child(".credentials.json.lock")
            .map_err(store_error)?;
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(PulseError::configuration(
                "credential refresh lock failed safety checks",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|_| {
                safe_error(
                    PulseErrorKind::Authentication,
                    "credential refresh lock is unavailable",
                )
            })?;
        let file_metadata = file.metadata().map_err(|_| {
            safe_error(
                PulseErrorKind::Authentication,
                "credential refresh lock is unavailable",
            )
        })?;
        let path_metadata = fs::symlink_metadata(&path).map_err(|_| {
            safe_error(
                PulseErrorKind::Authentication,
                "credential refresh lock is unavailable",
            )
        })?;
        if !file_metadata.is_file()
            || path_metadata.file_type().is_symlink()
            || !same_file(&file_metadata, &path_metadata)
        {
            return Err(PulseError::configuration(
                "credential refresh lock failed safety checks",
            ));
        }
        let started = MonotonicInstant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= timeout {
                        return Err(safe_error(
                            PulseErrorKind::Conflict,
                            "credential refresh is already in progress",
                        ));
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(_) => {
                    return Err(safe_error(
                        PulseErrorKind::Authentication,
                        "credential refresh lock is unavailable",
                    ));
                }
            }
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[cfg(not(unix))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    modified: Option<SystemTime>,
    len: u64,
}

#[cfg(not(unix))]
impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }
    }
}

fn update_document_tokens(document: &mut Value, grant: &RefreshGrant) -> PulseResult<()> {
    let oauth = document
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            safe_error(
                PulseErrorKind::Authentication,
                "credential store has no usable OAuth record",
            )
        })?;
    oauth.insert(
        "accessToken".to_owned(),
        Value::String(grant.access_token.expose().to_owned()),
    );
    oauth.insert(
        "refreshToken".to_owned(),
        Value::String(grant.refresh_token.expose().to_owned()),
    );
    oauth.insert(
        "expiresAt".to_owned(),
        Value::Number(grant.expires_at_millis.into()),
    );
    Ok(())
}

fn persist_linux_document(directory: &ValidatedDirectory, document: &Value) -> PulseResult<()> {
    let bytes = serde_json::to_vec(document).map_err(|_| {
        safe_error(
            PulseErrorKind::Internal,
            "credential update could not be encoded",
        )
    })?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return Err(safe_error(
            PulseErrorKind::Configuration,
            "credential update exceeded its size bound",
        ));
    }
    let destination = directory.child(".credentials.json").map_err(store_error)?;
    let temporary = directory
        .child(&format!(".credentials.json.atmux-{}.tmp", unique_nonce()))
        .map_err(store_error)?;
    write_atomic_file(&temporary, &destination, &bytes)
}

fn write_atomic_file(temporary: &Path, destination: &Path, bytes: &[u8]) -> PulseResult<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(temporary).map_err(|_| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential temporary file could not be created",
        )
    })?;
    let mut owned = OwnedTemporary {
        path: temporary.to_path_buf(),
        identity: FileIdentity::from_metadata(&file.metadata().map_err(|_| {
            safe_error(
                PulseErrorKind::Authentication,
                "credential temporary file could not be inspected",
            )
        })?),
        committed: false,
    };
    file.write_all(bytes).map_err(|_| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential update could not be written",
        )
    })?;
    file.sync_all().map_err(|_| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential update could not be synchronized",
        )
    })?;
    drop(file);
    fs::rename(temporary, destination).map_err(|_| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential update could not be committed",
        )
    })?;
    owned.committed = true;
    File::open(
        destination
            .parent()
            .ok_or_else(|| PulseError::configuration("credential path has no parent"))?,
    )
    .and_then(|directory| directory.sync_all())
    .map_err(|_| {
        safe_error(
            PulseErrorKind::Authentication,
            "credential directory could not be synchronized",
        )
    })?;
    Ok(())
}

struct OwnedTemporary {
    path: PathBuf,
    identity: FileIdentity,
    committed: bool,
}

impl Drop for OwnedTemporary {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if !metadata.file_type().is_symlink()
            && metadata.is_file()
            && FileIdentity::from_metadata(&metadata) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn unique_nonce() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos()),
        NEXT_NONCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn safe_error(kind: PulseErrorKind, message: &'static str) -> PulseError {
    PulseError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "atmux-pulse-creds-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create temp directory");
            Self(path)
        }

        fn write(&self, json: &str) {
            fs::write(self.0.join(".credentials.json"), json).expect("write credentials");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn document(access: &str, refresh: &str, expiry: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expiry,
                "scopes": ["user:profile", "user:inference"],
                "subscriptionType": "fixture"
            },
            "mcpOAuth": {"preserve": true}
        })
        .to_string()
    }

    #[test]
    fn inspection_states_are_secret_free() {
        let directory = TempDir::new();
        directory.write("{not-json secret-inspection-value");
        let inspection =
            inspect_claude_credentials_for(&directory.0, 1_000, CredentialPlatform::Linux);
        assert_eq!(inspection.state, CredentialState::Unparseable);
        assert!(!format!("{inspection:?}").contains("secret-inspection-value"));

        directory.write(&document("secret-access", "secret-refresh", 500));
        let inspection =
            inspect_claude_credentials_for(&directory.0, 1_000, CredentialPlatform::Linux);
        assert_eq!(inspection.state, CredentialState::Expired);
        assert!(inspection.is_usable());
        assert!(!format!("{inspection:?}").contains("secret"));
    }

    #[test]
    fn token_and_request_material_are_redacted() {
        let tokens = ClaudeOauthTokens::new(
            "access-redaction-canary",
            Some("refresh-redaction-canary".to_owned()),
            10_000,
            vec!["user:profile".to_owned()],
        )
        .expect("tokens");
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("redaction-canary"));
        let codex = CodexCredentials::new("codex-token-canary", "codex-account-canary")
            .expect("codex credentials");
        assert!(!format!("{codex:?}").contains("canary"));
    }

    #[test]
    fn lock_first_reread_adopts_a_sibling_token() {
        let directory = TempDir::new();
        directory.write(&document("rejected-token", "old-refresh", 500));
        let rejected = read_claude_credentials_for(&directory.0, CredentialPlatform::Linux)
            .expect("read rejected");
        directory.write(&document("sibling-token", "sibling-refresh", 90_000));
        let mut grants = 0;
        let result = cooperative_refresh_with(
            &directory.0,
            &rejected,
            true,
            RefreshPolicy::InMemory,
            1_000,
            RefreshOptions {
                platform: CredentialPlatform::Linux,
                lock_timeout: Duration::ZERO,
            },
            |_| {
                grants += 1;
                RefreshGrant::new("unused", "unused", 100_000)
            },
        )
        .expect("adopt sibling");
        assert_eq!(result.source, RefreshSource::Adopted);
        assert_eq!(grants, 0);
        assert_eq!(result.tokens.access_token.expose(), "sibling-token");
    }

    #[test]
    fn in_memory_policy_never_spends_a_rotating_refresh_token() {
        let directory = TempDir::new();
        directory.write(&document("expired-token", "freshest-refresh", 500));
        let rejected = read_claude_credentials_for(&directory.0, CredentialPlatform::Linux)
            .expect("read rejected");
        let mut grants = 0;
        let error = cooperative_refresh_with(
            &directory.0,
            &rejected,
            false,
            RefreshPolicy::InMemory,
            1_000,
            RefreshOptions {
                platform: CredentialPlatform::Linux,
                lock_timeout: Duration::ZERO,
            },
            |_| {
                grants += 1;
                RefreshGrant::new("memory-access", "memory-refresh", 100_000)
            },
        )
        .expect_err("in-memory refresh is not crash safe");
        assert_eq!(error.kind(), PulseErrorKind::Configuration);
        assert_eq!(grants, 0);
        let stored = read_claude_credentials_for(&directory.0, CredentialPlatform::Linux)
            .expect("reread store");
        assert_eq!(stored.access_token.expose(), "expired-token");
    }

    #[test]
    fn never_policy_may_adopt_but_never_spends_a_refresh_token() {
        let directory = TempDir::new();
        directory.write(&document("rejected-token", "old-refresh", 500));
        let rejected = read_claude_credentials_for(&directory.0, CredentialPlatform::Linux)
            .expect("read rejected");
        directory.write(&document("sibling-token", "sibling-refresh", 90_000));
        let mut grants = 0;
        let adopted = cooperative_refresh_with(
            &directory.0,
            &rejected,
            true,
            RefreshPolicy::Never,
            1_000,
            RefreshOptions {
                platform: CredentialPlatform::Linux,
                lock_timeout: Duration::ZERO,
            },
            |_| {
                grants += 1;
                RefreshGrant::new("unused", "unused", 100_000)
            },
        )
        .expect("newer authoritative token is safe to adopt");
        assert_eq!(adopted.source, RefreshSource::Adopted);
        assert_eq!(grants, 0);

        directory.write(&document("expired-token", "freshest-refresh", 500));
        let rejected = read_claude_credentials_for(&directory.0, CredentialPlatform::Linux)
            .expect("read rejected");
        let error = cooperative_refresh_with(
            &directory.0,
            &rejected,
            false,
            RefreshPolicy::Never,
            1_000,
            RefreshOptions {
                platform: CredentialPlatform::Linux,
                lock_timeout: Duration::ZERO,
            },
            |_| {
                grants += 1;
                RefreshGrant::new("unused", "unused", 100_000)
            },
        )
        .expect_err("disabled policy cannot spend");
        assert_eq!(error.kind(), PulseErrorKind::Authentication);
        assert_eq!(grants, 0);
    }

    #[test]
    fn linux_persist_is_atomic_and_preserves_siblings() {
        let directory = TempDir::new();
        directory.write(&document("expired-token", "refresh-token", 500));
        let rejected = read_claude_credentials_for(&directory.0, CredentialPlatform::Linux)
            .expect("read rejected");
        let result = cooperative_refresh_with(
            &directory.0,
            &rejected,
            false,
            RefreshPolicy::Persist,
            1_000,
            RefreshOptions {
                platform: CredentialPlatform::Linux,
                lock_timeout: Duration::ZERO,
            },
            |_| RefreshGrant::new("persist-access", "persist-refresh", 100_000),
        )
        .expect("persist refresh");
        assert_eq!(result.source, RefreshSource::GrantedAndPersisted);
        let value: Value = serde_json::from_slice(
            &fs::read(directory.0.join(".credentials.json")).expect("read persisted"),
        )
        .expect("parse persisted");
        assert_eq!(value["mcpOAuth"]["preserve"], true);
        assert_eq!(value["claudeAiOauth"]["accessToken"], "persist-access");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(directory.0.join(".credentials.json"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn stale_temporary_file_does_not_block_persistence() {
        let directory = TempDir::new();
        directory.write(&document("expired-token", "refresh-token", 500));
        let stale = directory.0.join(format!(
            ".credentials.json.atmux-{}-stale.tmp",
            std::process::id()
        ));
        fs::write(&stale, "stale").expect("write stale temp");
        let rejected = read_claude_credentials_for(&directory.0, CredentialPlatform::Linux)
            .expect("read rejected");
        let result = cooperative_refresh_with(
            &directory.0,
            &rejected,
            false,
            RefreshPolicy::Persist,
            1_000,
            RefreshOptions {
                platform: CredentialPlatform::Linux,
                lock_timeout: Duration::ZERO,
            },
            |_| RefreshGrant::new("new-access", "new-refresh", 100_000),
        )
        .expect("persist despite stale temp");
        assert_eq!(result.source, RefreshSource::GrantedAndPersisted);
        assert!(stale.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn directory_handle_anchors_reads_and_persistence_after_ancestor_swap() {
        use std::os::unix::fs::symlink;

        let base = TempDir::new();
        let profile = base.0.join("profile");
        let original = base.0.join("original-profile");
        let attacker = base.0.join("attacker-profile");
        fs::create_dir(&profile).expect("create profile");
        fs::create_dir(&attacker).expect("create attacker");
        fs::write(
            profile.join(".credentials.json"),
            document("original-access", "original-refresh", 500),
        )
        .expect("write original");
        fs::write(
            attacker.join(".credentials.json"),
            document("attacker-access", "attacker-refresh", 500),
        )
        .expect("write attacker");
        let anchored = ValidatedDirectory::open(&profile).expect("anchor directory");
        fs::rename(&profile, &original).expect("move original");
        symlink(&attacker, &profile).expect("replace path with symlink");

        let read = read_document(&anchored, CredentialPlatform::Linux).expect("anchored read");
        assert_eq!(
            read["claudeAiOauth"]["accessToken"],
            Value::String("original-access".to_owned())
        );
        let grant =
            RefreshGrant::new("anchored-access", "anchored-refresh", 100_000).expect("grant");
        let mut updated = read;
        update_document_tokens(&mut updated, &grant).expect("update");
        persist_linux_document(&anchored, &updated).expect("anchored persist");
        let original_value: Value =
            serde_json::from_slice(&fs::read(original.join(".credentials.json")).expect("read"))
                .expect("parse");
        let attacker_value: Value =
            serde_json::from_slice(&fs::read(attacker.join(".credentials.json")).expect("read"))
                .expect("parse");
        assert_eq!(
            original_value["claudeAiOauth"]["accessToken"],
            "anchored-access"
        );
        assert_eq!(
            attacker_value["claudeAiOauth"]["accessToken"],
            "attacker-access"
        );
    }

    #[test]
    fn macos_persistence_is_rejected_before_grant() {
        let directory = TempDir::new();
        let rejected = ClaudeOauthTokens::new(
            "rejected",
            Some("refresh".to_owned()),
            500,
            vec!["user:profile".to_owned()],
        )
        .expect("tokens");
        let mut called = false;
        let error = cooperative_refresh_with(
            &directory.0,
            &rejected,
            true,
            RefreshPolicy::Persist,
            1_000,
            RefreshOptions {
                platform: CredentialPlatform::Macos,
                lock_timeout: Duration::ZERO,
            },
            |_| {
                called = true;
                RefreshGrant::new("unused", "unused", 10_000)
            },
        )
        .expect_err("mac persistence rejected");
        assert_eq!(error.kind(), PulseErrorKind::Configuration);
        assert!(!called);
    }

    #[test]
    fn only_linux_persistence_can_spend_a_rotating_refresh_token() {
        assert!(
            ensure_refresh_grant_is_durable(RefreshPolicy::Persist, CredentialPlatform::Linux)
                .is_ok()
        );
        for (policy, platform) in [
            (RefreshPolicy::InMemory, CredentialPlatform::Linux),
            (RefreshPolicy::InMemory, CredentialPlatform::Macos),
            (RefreshPolicy::Persist, CredentialPlatform::Macos),
            (RefreshPolicy::Persist, CredentialPlatform::Other),
            (RefreshPolicy::Never, CredentialPlatform::Linux),
        ] {
            assert!(ensure_refresh_grant_is_durable(policy, platform).is_err());
        }
    }

    #[test]
    fn codex_auth_reader_is_bounded_and_redacted() {
        let directory = TempDir::new();
        fs::write(
            directory.0.join("auth.json"),
            serde_json::json!({
                "tokens": {
                    "access_token": "codex-secret-token",
                    "account_id": "codex-secret-account",
                    "id_token": "ignored-identity"
                }
            })
            .to_string(),
        )
        .expect("write auth");
        assert!(inspect_codex_credentials(&directory.0).is_usable());
        let credentials = read_codex_credentials(&directory.0).expect("read codex credentials");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("codex-secret"));
        assert!(!debug.contains("ignored-identity"));

        fs::write(
            directory.0.join("auth.json"),
            vec![b'x'; MAX_CREDENTIAL_BYTES + 1],
        )
        .expect("write oversized auth");
        assert_eq!(
            inspect_codex_credentials(&directory.0).state,
            CodexCredentialState::UnsafeStore
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_reader_refuses_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new();
        let target = directory.0.join("target.json");
        fs::write(
            &target,
            r#"{"tokens":{"access_token":"hidden","account_id":"hidden"}}"#,
        )
        .expect("write target");
        symlink(&target, directory.0.join("auth.json")).expect("create symlink");
        assert_eq!(
            inspect_codex_credentials(&directory.0).state,
            CodexCredentialState::UnsafeStore
        );
    }

    #[test]
    fn stale_lock_file_is_reusable_after_owner_exit() {
        let directory = TempDir::new();
        fs::write(
            directory.0.join(".credentials.json.lock"),
            "crashed-owner\n",
        )
        .expect("write stale lock file");
        let validated = ValidatedDirectory::open(&directory.0).expect("validate");
        let lock = CredentialLock::acquire(&validated, Duration::ZERO).expect("lock");
        drop(lock);
        assert_eq!(
            fs::read_to_string(directory.0.join(".credentials.json.lock"))
                .expect("advisory lock file remains"),
            "crashed-owner\n"
        );
    }

    #[test]
    fn concurrent_owner_conflicts_until_file_handle_is_dropped() {
        let directory = TempDir::new();
        let validated = ValidatedDirectory::open(&directory.0).expect("validate");
        let first = CredentialLock::acquire(&validated, Duration::ZERO).expect("first lock");
        let error = CredentialLock::acquire(&validated, Duration::ZERO)
            .err()
            .expect("lock conflict");
        assert_eq!(error.kind(), PulseErrorKind::Conflict);
        drop(first);
        CredentialLock::acquire(&validated, Duration::ZERO).expect("lock after owner exit");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_runner_caps_output_and_time() {
        assert!(matches!(
            run_bounded_child(Path::new("/usr/bin/yes"), &[], 64, Duration::from_secs(1)),
            Err(StoreReadError::UnsafeStore)
        ));
        let started = MonotonicInstant::now();
        assert!(matches!(
            run_bounded_child(
                Path::new("/usr/bin/sleep"),
                &["2"],
                64,
                Duration::from_millis(20)
            ),
            Err(StoreReadError::Unreadable)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
