//! Native `PostgreSQL` implementation of the Pulse store contract.
//!
//! This backend deliberately does not translate `SQLite` SQL. `PostgreSQL` owns a
//! dedicated schema, stores wall-clock values as `TIMESTAMPTZ` and token days
//! as `DATE`, and stores structured values as `JSONB`.

use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    future::Future,
    io::{Read, Seek as _, SeekFrom},
    net::IpAddr,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
};

use jiff::{Timestamp, civil::Date};
use rustls::{ClientConfig, RootCertStore};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{sync::Mutex, task::AbortHandle};
use tokio_postgres::config::Host;
use tokio_postgres::{Client, Config, NoTls, Row, Transaction, error::SqlState};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::{
    AlertEvent, AlertEventInput, AlertReply, AlertReplyInput, CurrentQuotaWindow,
    IdempotentIngestResult, ImportBatch, ImportBatchResult, ImportProvenance, ImportedAlertEvent,
    ImportedAlertSubscription, ImportedRow, IngestBatch, IngestLimits, IngestReplay, IngestResult,
    IngestToken, MAX_ALERT_REPLIES_PER_EVENT, MAX_ALERT_REPLY_BYTES, MAX_IMPORT_BATCH_ROWS,
    MAX_IMPORT_RECONCILIATION_KEYS, MAX_INGEST_REPLAYS_PER_ACCOUNT,
    MAX_REPORTER_DESTINATIONS_PER_ACCOUNT, MAX_RESET_HORIZON_MILLIS, MAX_RESET_JOBS_PER_ACCOUNT,
    PricingRule, ReporterCursorState, ReporterPendingChunk, ReporterPendingDraft,
    ReporterPendingPage, ReporterStreamKind, ReporterTokenPosition, ResetResumeInput,
    ResetResumeJob, ResetResumeLimits, RetentionResult, Store, StoreFuture,
    StoredAlertSubscription, StoredTokenTotals, StoredUsageSnapshot, TokenBackfillPage,
    TokenBackfillState, TokenReconciliationKey, TokenWriteObservation,
    schema::LATEST_SCHEMA_VERSION, validate_pricing_key, validate_reporter_destination,
    validate_reporter_transition,
};
use crate::pulse::{
    Account, AccountId, AlertSubscription, AlertType, ContextSession, Fraction, GeminiQuota,
    Instant, Machine, MachineName, Percent, Profile, ProfileName, ProfileOrigin, QuotaWindow,
    RefreshPolicy, SessionId, TokenGrain, UsageContributor, UsageSnapshot, Vendor,
    error::{PulseError, PulseErrorKind, PulseResult},
    federation::{
        FederatedPulseRow, FederatedRecord, FederationExportPosition, FederationState,
        LocalFederationRecord, MAX_PAGE_ROWS, OpaqueCursor,
    },
    token::TokenSourceGeneration,
};

const SCHEMA: &str = "atmux_pulse";
const MIGRATION_LOCK: i64 = 4_708_041_764_058_715_468;
const RESET_JITTER_TOLERANCE_MS: i64 = 5 * 60 * 1_000;
const MAX_QUERY_ROWS: usize = 10_000;
const MAX_CA_BUNDLE_BYTES: u64 = 8 * 1024 * 1024;
const INGEST_TOKEN_LOCK_NAMESPACE: i64 = 0x4154_4d55_5854_4f4b;

type TxFuture<'a, T> = Pin<Box<dyn Future<Output = PulseResult<T>> + Send + 'a>>;

struct PostgresInner {
    client: Mutex<Client>,
    connection_driver: AbortHandle,
}

impl Drop for PostgresInner {
    fn drop(&mut self) {
        self.connection_driver.abort();
    }
}

/// A cloneable, serialized `PostgreSQL` store connection.
//
// A single connection is intentional for the embedded service: transactions
// never leak account GUCs across operations, and a future pool can preserve the
// same contract by applying `SET LOCAL` on every checkout transaction.
#[derive(Clone)]
pub struct PostgresStore {
    inner: Arc<PostgresInner>,
}

impl PostgresStore {
    /// Connects, validates transport policy, and applies forward migrations.
    ///
    /// Non-loopback TCP connections must explicitly use `sslmode=require` and
    /// are certificate-validated against the operating system root bundle.
    /// Plaintext is accepted only for loopback/Unix-socket test databases.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an unsafe connection string or a
    /// storage error when connection/migration fails.
    pub async fn connect(connection_url: &str) -> PulseResult<Self> {
        let (mut client, driver) = connect_client(connection_url).await?;
        apply_migrations(&mut client).await?;
        Ok(Self::from_client(client, driver))
    }

    /// Connects to an already-current database without executing DDL and
    /// enforces read-only transactions for the lifetime of this connection.
    ///
    /// # Errors
    ///
    /// Returns configuration for an absent/outdated schema or unsafe
    /// transport, and storage when the read-only health check fails.
    pub async fn connect_read_only(connection_url: &str) -> PulseResult<Self> {
        let (client, driver) = connect_client(connection_url).await?;
        client
            .batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
            .await
            .map_err(sql_error)?;
        validate_read_only_schema(&client).await?;
        let store = Self::from_client(client, driver);
        if !store.integrity_check().await?.eq_ignore_ascii_case("ok") {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "PostgreSQL Pulse schema failed its read-only health check",
            ));
        }
        Ok(store)
    }

    fn from_client(client: Client, driver: AbortHandle) -> Self {
        Self {
            inner: Arc::new(PostgresInner {
                client: Mutex::new(client),
                connection_driver: driver,
            }),
        }
    }

    fn account_operation<T, F>(&self, account_id: AccountId, operation: F) -> StoreFuture<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a Transaction<'a>) -> TxFuture<'a, T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let mut client = inner.client.lock().await;
            let transaction = client.transaction().await.map_err(sql_error)?;
            set_account_scope(&transaction, account_id).await?;
            let value = operation(&transaction).await?;
            transaction.commit().await.map_err(sql_error)?;
            Ok(value)
        })
    }

    fn bypass_operation<T, F>(&self, operation: F) -> StoreFuture<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a Transaction<'a>) -> TxFuture<'a, T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let mut client = inner.client.lock().await;
            let transaction = client.transaction().await.map_err(sql_error)?;
            set_bypass_scope(&transaction).await?;
            let value = operation(&transaction).await?;
            transaction.commit().await.map_err(sql_error)?;
            Ok(value)
        })
    }

    fn global_operation<T, F>(&self, operation: F) -> StoreFuture<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a Transaction<'a>) -> TxFuture<'a, T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let mut client = inner.client.lock().await;
            let transaction = client.transaction().await.map_err(sql_error)?;
            clear_account_scope(&transaction).await?;
            let value = operation(&transaction).await?;
            transaction.commit().await.map_err(sql_error)?;
            Ok(value)
        })
    }
}

async fn connect_client(connection_url: &str) -> PulseResult<(Client, AbortHandle)> {
    let mut config = Config::from_str(connection_url)
        .map_err(|_| PulseError::configuration("invalid PostgreSQL connection configuration"))?;
    config.application_name("atmux-pulse");
    let local = config_is_local(&config);
    let ssl_mode = config.get_ssl_mode();

    let (client, driver) = match ssl_mode {
        tokio_postgres::config::SslMode::Disable if local => connect_plain(config).await?,
        tokio_postgres::config::SslMode::Prefer if local => {
            config.ssl_mode(tokio_postgres::config::SslMode::Disable);
            connect_plain(config).await?
        }
        tokio_postgres::config::SslMode::Require => connect_tls(config).await?,
        tokio_postgres::config::SslMode::Disable => {
            return Err(PulseError::configuration(
                "non-loopback PostgreSQL connections require TLS",
            ));
        }
        _ => {
            return Err(PulseError::configuration(
                "non-loopback PostgreSQL connections must set sslmode=require",
            ));
        }
    };
    Ok((client, driver))
}

async fn validate_read_only_schema(client: &Client) -> PulseResult<()> {
    let migration_table = client
        .query_one(
            "SELECT to_regclass('atmux_pulse.pulse_schema_migrations')::TEXT",
            &[],
        )
        .await
        .map_err(sql_error)?
        .get::<_, Option<String>>(0);
    if migration_table.is_none() {
        return Err(PulseError::configuration(
            "Pulse doctor PostgreSQL database has no recognized schema",
        ));
    }
    let version = current_schema_version(client).await.map_err(|_| {
        PulseError::configuration("Pulse doctor PostgreSQL database has no recognized schema")
    })?;
    if version != LATEST_SCHEMA_VERSION {
        return Err(PulseError::configuration(format!(
            "Pulse doctor requires schema {LATEST_SCHEMA_VERSION}; found {version}"
        )));
    }
    Ok(())
}

async fn connect_plain(config: Config) -> PulseResult<(Client, AbortHandle)> {
    let (client, connection) = config.connect(NoTls).await.map_err(connect_error)?;
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok((client, task.abort_handle()))
}

async fn connect_tls(config: Config) -> PulseResult<(Client, AbortHandle)> {
    let roots = system_root_store()?;
    let tls = MakeRustlsConnect::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let (client, connection) = config.connect(tls).await.map_err(connect_error)?;
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok((client, task.abort_handle()))
}

fn config_is_local(config: &Config) -> bool {
    let hosts_local = config.get_hosts().is_empty()
        || config.get_hosts().iter().all(|host| match host {
            Host::Tcp(host) => {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            }
            #[cfg(unix)]
            Host::Unix(_) => true,
        });
    hosts_local && config.get_hostaddrs().iter().all(IpAddr::is_loopback)
}

fn system_root_store() -> PulseResult<RootCertStore> {
    for candidate in [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/ssl/cert.pem",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/openssl/certs/ca-certificates.crt",
    ] {
        let Ok(bytes) = read_bounded_file(Path::new(candidate), MAX_CA_BUNDLE_BYTES) else {
            continue;
        };
        let certificates = rustls_pemfile::certs(&mut std::io::Cursor::new(bytes))
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let mut roots = RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(certificates);
        if added > 0 {
            return Ok(roots);
        }
    }
    Err(PulseError::configuration(
        "no usable system TLS root bundle was found for PostgreSQL",
    ))
}

fn read_bounded_file(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(std::io::Error::other(
            "TLS root bundle is not a bounded file",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(std::io::Error::other(
            "TLS root bundle exceeds its size bound",
        ));
    }
    Ok(bytes)
}

async fn set_account_scope(
    transaction: &Transaction<'_>,
    account_id: AccountId,
) -> PulseResult<()> {
    transaction
        .query_one(
            "SELECT set_config('atmux.account_id', $1, true), \
                    set_config('atmux.pulse_bypass', 'off', true)",
            &[&account_id.get().to_string()],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn set_bypass_scope(transaction: &Transaction<'_>) -> PulseResult<()> {
    transaction
        .query_one(
            "SELECT set_config('atmux.account_id', '', true), \
                    set_config('atmux.pulse_bypass', 'on', true)",
            &[],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn clear_account_scope(transaction: &Transaction<'_>) -> PulseResult<()> {
    transaction
        .query_one(
            "SELECT set_config('atmux.account_id', '', true), \
                    set_config('atmux.pulse_bypass', 'off', true)",
            &[],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn connect_error(_: tokio_postgres::Error) -> PulseError {
    PulseError::new(
        PulseErrorKind::Storage,
        "failed to establish the PostgreSQL store connection",
    )
}

#[allow(clippy::needless_pass_by_value)]
fn sql_error(error: tokio_postgres::Error) -> PulseError {
    let kind = error
        .as_db_error()
        .map_or(PulseErrorKind::Storage, |database| match *database.code() {
            SqlState::UNIQUE_VIOLATION
            | SqlState::FOREIGN_KEY_VIOLATION
            | SqlState::CHECK_VIOLATION
            | SqlState::EXCLUSION_VIOLATION => PulseErrorKind::Conflict,
            _ => PulseErrorKind::Storage,
        });
    let suffix = error
        .as_db_error()
        .map(|database| format!(" ({})", database.code().code()))
        .unwrap_or_default();
    PulseError::new(kind, format!("PostgreSQL store operation failed{suffix}"))
}

fn json<T: Serialize>(value: &T) -> PulseResult<Value> {
    serde_json::to_value(value).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Internal,
            "failed to encode a typed PostgreSQL value",
        )
    })
}

fn decode_json<T: DeserializeOwned>(value: Value) -> PulseResult<T> {
    serde_json::from_value(value).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "PostgreSQL contains an invalid typed Pulse value",
        )
    })
}

fn pg_timestamp(value: Instant) -> PulseResult<Timestamp> {
    Timestamp::from_millisecond(value.epoch_millis()).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Internal,
            "validated Pulse instant was outside the PostgreSQL timestamp range",
        )
    })
}

fn pulse_instant(value: Timestamp) -> PulseResult<Instant> {
    Instant::from_epoch_millis(value.as_millisecond()).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "PostgreSQL contains an invalid Pulse timestamp",
        )
    })
}

fn path_text(path: Option<&PathBuf>, field: &str) -> PulseResult<Option<String>> {
    path.map(|path| {
        path.to_str().map(str::to_owned).ok_or_else(|| {
            PulseError::configuration(format!("{field} must be valid UTF-8 for PostgreSQL"))
        })
    })
    .transpose()
}

fn query_limit(limit: usize) -> PulseResult<i64> {
    if limit == 0 || limit > MAX_QUERY_ROWS {
        return Err(PulseError::invalid_input(format!(
            "query limit must be between 1 and {MAX_QUERY_ROWS}"
        )));
    }
    i64::try_from(limit).map_err(|_| PulseError::invalid_input("query limit is too large"))
}

fn as_i64(value: u64, field: &str) -> PulseResult<i64> {
    i64::try_from(value)
        .map_err(|_| PulseError::invalid_input(format!("{field} exceeds PostgreSQL BIGINT")))
}

fn as_u64(value: i64) -> PulseResult<u64> {
    u64::try_from(value)
        .map_err(|_| PulseError::new(PulseErrorKind::Storage, "stored Pulse count is negative"))
}

struct PostgresMigration {
    version: u32,
    sql: &'static str,
}

const POSTGRES_MIGRATIONS: &[PostgresMigration] = &[
    PostgresMigration {
        version: 1,
        sql: r"
CREATE TABLE atmux_pulse.accounts (
    id BIGINT PRIMARY KEY CHECK (id > 0),
    identity TEXT NOT NULL UNIQUE CHECK (char_length(identity) BETWEEN 1 AND 320),
    display_name TEXT
);

CREATE TABLE atmux_pulse.machines (
    account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    first_seen TIMESTAMPTZ NOT NULL,
    last_seen TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (account_id, name),
    CHECK (last_seen >= first_seen)
);

CREATE TABLE atmux_pulse.profiles (
    account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    vendor JSONB NOT NULL,
    config_dir TEXT,
    poll_interval_minutes INTEGER NOT NULL CHECK (poll_interval_minutes >= 5),
    monthly_budget_usd DOUBLE PRECISION,
    api_key_env TEXT,
    api_key_file TEXT,
    refresh JSONB NOT NULL,
    hidden BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (account_id, name),
    CHECK (api_key_env IS NULL OR api_key_file IS NULL),
    CHECK (monthly_budget_usd IS NULL OR monthly_budget_usd > 0)
);

CREATE TABLE atmux_pulse.usage_snapshots (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    vendor JSONB NOT NULL,
    outcome JSONB NOT NULL,
    polled_at TIMESTAMPTZ NOT NULL,
    reporter_version TEXT,
    UNIQUE (account_id, id),
    FOREIGN KEY (account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY (account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.usage_windows (
    account_id BIGINT NOT NULL,
    snapshot_id BIGINT NOT NULL,
    kind JSONB NOT NULL,
    used_percent DOUBLE PRECISION NOT NULL CHECK (used_percent BETWEEN 0 AND 100),
    resets_at TIMESTAMPTZ NOT NULL,
    accepted BOOLEAN NOT NULL,
    PRIMARY KEY (snapshot_id, kind),
    FOREIGN KEY (account_id, snapshot_id)
        REFERENCES atmux_pulse.usage_snapshots(account_id, id) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.context_sessions (
    account_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT,
    settings JSONB NOT NULL,
    context_tokens BIGINT,
    context_percent DOUBLE PRECISION,
    effective_limit BIGINT,
    last_active_at TIMESTAMPTZ NOT NULL,
    last_reset_at TIMESTAMPTZ,
    collected_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (account_id, profile, machine, session_id),
    FOREIGN KEY (account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY (account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE,
    CHECK (context_tokens IS NULL OR context_tokens >= 0),
    CHECK (context_percent IS NULL OR context_percent BETWEEN 0 AND 100),
    CHECK (effective_limit IS NULL OR effective_limit > 0)
);

CREATE TABLE atmux_pulse.token_usage (
    account_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    settings_hash TEXT NOT NULL,
    settings JSONB NOT NULL,
    day DATE NOT NULL,
    tokens_in BIGINT NOT NULL CHECK (tokens_in >= 0),
    tokens_out BIGINT NOT NULL CHECK (tokens_out >= 0),
    cache_write_5m BIGINT NOT NULL CHECK (cache_write_5m >= 0),
    cache_write_1h BIGINT NOT NULL CHECK (cache_write_1h >= 0),
    cache_read BIGINT NOT NULL CHECK (cache_read >= 0),
    source JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (
        account_id, profile, machine, session_id, model, settings_hash, day, source
    ),
    FOREIGN KEY (account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY (account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE
);

ALTER TABLE atmux_pulse.accounts ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.accounts FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.accounts AS PERMISSIVE FOR ALL
USING (
    COALESCE(id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.machines ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.machines FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.machines AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.profiles ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.profiles FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.profiles AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.usage_snapshots ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.usage_snapshots FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.usage_snapshots AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.usage_windows ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.usage_windows FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.usage_windows AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.context_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.context_sessions FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.context_sessions AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.token_usage ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.token_usage FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.token_usage AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);
",
    },
    PostgresMigration {
        version: 2,
        sql: r"
CREATE TABLE atmux_pulse.pricing_defaults (
    key TEXT PRIMARY KEY,
    vendor JSONB NOT NULL,
    model_pattern TEXT NOT NULL,
    settings JSONB NOT NULL,
    input_rate DOUBLE PRECISION NOT NULL CHECK (input_rate >= 0),
    output_rate DOUBLE PRECISION NOT NULL CHECK (output_rate >= 0),
    cache_write_5m_rate DOUBLE PRECISION NOT NULL CHECK (cache_write_5m_rate >= 0),
    cache_write_1h_rate DOUBLE PRECISION NOT NULL CHECK (cache_write_1h_rate >= 0),
    cache_read_rate DOUBLE PRECISION NOT NULL CHECK (cache_read_rate >= 0)
);

CREATE TABLE atmux_pulse.pricing_overrides (
    account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    vendor JSONB NOT NULL,
    model_pattern TEXT NOT NULL,
    settings JSONB NOT NULL,
    input_rate DOUBLE PRECISION NOT NULL CHECK (input_rate >= 0),
    output_rate DOUBLE PRECISION NOT NULL CHECK (output_rate >= 0),
    cache_write_5m_rate DOUBLE PRECISION NOT NULL CHECK (cache_write_5m_rate >= 0),
    cache_write_1h_rate DOUBLE PRECISION NOT NULL CHECK (cache_write_1h_rate >= 0),
    cache_read_rate DOUBLE PRECISION NOT NULL CHECK (cache_read_rate >= 0),
    PRIMARY KEY (account_id, key)
);

CREATE TABLE atmux_pulse.alert_subscriptions (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY,
    account_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    alert_type JSONB NOT NULL,
    threshold DOUBLE PRECISION,
    threshold_key TEXT NOT NULL,
    cooldown_minutes INTEGER NOT NULL CHECK (cooldown_minutes > 0),
    delivery JSONB,
    enabled BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (id),
    UNIQUE (account_id, id),
    UNIQUE (account_id, profile, alert_type, threshold_key),
    FOREIGN KEY (account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.alert_events (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id) ON DELETE CASCADE,
    subscription_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    alert_type JSONB NOT NULL,
    message TEXT NOT NULL CHECK (char_length(message) BETWEEN 1 AND 4096),
    current_value DOUBLE PRECISION,
    threshold DOUBLE PRECISION,
    acknowledged BOOLEAN NOT NULL DEFAULT FALSE,
    triggered_at TIMESTAMPTZ NOT NULL,
    FOREIGN KEY (account_id, subscription_id)
        REFERENCES atmux_pulse.alert_subscriptions(account_id, id) ON DELETE CASCADE,
    FOREIGN KEY (account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.ingest_tokens (
    id BIGINT PRIMARY KEY CHECK (id > 0),
    account_id BIGINT NOT NULL,
    machine TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE CHECK (char_length(token_hash) = 64),
    created_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    FOREIGN KEY (account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.gemini_quota (
    account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    remaining_fraction DOUBLE PRECISION NOT NULL CHECK (remaining_fraction BETWEEN 0 AND 1),
    remaining_amount TEXT,
    resets_at TIMESTAMPTZ,
    collected_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (account_id, model_id)
);

CREATE TABLE atmux_pulse.import_provenance (
    account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id) ON DELETE CASCADE,
    source_fingerprint TEXT NOT NULL,
    source_table TEXT NOT NULL,
    source_row_id TEXT NOT NULL,
    target_key TEXT NOT NULL,
    imported_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (account_id, source_fingerprint, source_table, source_row_id)
);

CREATE INDEX usage_snapshots_profile_time
    ON atmux_pulse.usage_snapshots(account_id, profile, polled_at DESC, id DESC);
CREATE INDEX usage_snapshots_machine_time
    ON atmux_pulse.usage_snapshots(account_id, profile, machine, polled_at DESC, id DESC);
CREATE INDEX usage_windows_winner
    ON atmux_pulse.usage_windows(account_id, kind, accepted, resets_at DESC, snapshot_id DESC);
CREATE INDEX context_sessions_freshness
    ON atmux_pulse.context_sessions(account_id, last_active_at DESC);
CREATE INDEX token_usage_report
    ON atmux_pulse.token_usage(account_id, day, profile, machine, model);
CREATE INDEX alert_events_cooldown
    ON atmux_pulse.alert_events(account_id, subscription_id, triggered_at DESC);
CREATE INDEX alert_events_ack
    ON atmux_pulse.alert_events(account_id, acknowledged, triggered_at DESC);

ALTER TABLE atmux_pulse.pricing_overrides ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.pricing_overrides FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.pricing_overrides AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.alert_subscriptions ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.alert_subscriptions FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.alert_subscriptions AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.alert_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.alert_events FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.alert_events AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.ingest_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.ingest_tokens FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.ingest_tokens AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.gemini_quota ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.gemini_quota FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.gemini_quota AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.import_provenance ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.import_provenance FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.import_provenance AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);
",
    },
    PostgresMigration {
        version: 3,
        sql: r#"
ALTER TABLE atmux_pulse.profiles
    ADD COLUMN origin JSONB NOT NULL DEFAULT '"local"'::JSONB;

ALTER TABLE atmux_pulse.alert_events
    ADD CONSTRAINT alert_events_account_identity UNIQUE (account_id, id);

CREATE TABLE atmux_pulse.alert_replies (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL,
    event_id BIGINT NOT NULL,
    message TEXT NOT NULL CHECK (octet_length(message) BETWEEN 1 AND 2048),
    replied_at TIMESTAMPTZ NOT NULL,
    FOREIGN KEY (account_id, event_id)
        REFERENCES atmux_pulse.alert_events(account_id, id) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.reset_resume_jobs (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    resets_at TIMESTAMPTZ NOT NULL,
    resume_at TIMESTAMPTZ NOT NULL,
    scheduled_at TIMESTAMPTZ NOT NULL,
    lease_until TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    delivered_at TIMESTAMPTZ,
    cancelled_at TIMESTAMPTZ,
    FOREIGN KEY (account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE,
    UNIQUE (account_id, profile, resets_at),
    CHECK (resume_at = resets_at + INTERVAL '1 minute'),
    CHECK (resets_at > scheduled_at),
    CHECK (delivered_at IS NULL OR cancelled_at IS NULL)
);

CREATE TABLE atmux_pulse.ingest_replays (
    account_id BIGINT NOT NULL,
    machine TEXT NOT NULL,
    request_id TEXT NOT NULL CHECK (char_length(request_id) BETWEEN 1 AND 128),
    payload_fingerprint TEXT NOT NULL CHECK (char_length(payload_fingerprint) = 64),
    snapshots BIGINT NOT NULL CHECK (snapshots >= 0),
    token_grains BIGINT NOT NULL CHECK (token_grains >= 0),
    context_sessions BIGINT NOT NULL CHECK (context_sessions >= 0),
    gemini_quotas BIGINT NOT NULL CHECK (gemini_quotas >= 0),
    received_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (account_id, machine, request_id),
    FOREIGN KEY (account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE
);

CREATE INDEX alert_replies_event
    ON atmux_pulse.alert_replies(account_id, event_id, replied_at, id);
CREATE INDEX reset_resume_pending
    ON atmux_pulse.reset_resume_jobs(account_id, resume_at, id)
    WHERE delivered_at IS NULL AND cancelled_at IS NULL;
CREATE INDEX ingest_replays_account ON atmux_pulse.ingest_replays(account_id);

ALTER TABLE atmux_pulse.alert_replies ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.alert_replies FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.alert_replies AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.reset_resume_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.reset_resume_jobs FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.reset_resume_jobs AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.ingest_replays ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.ingest_replays FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.ingest_replays AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);
"#,
    },
    PostgresMigration {
        version: 4,
        sql: r"
CREATE TABLE atmux_pulse.federation_peers (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id) ON DELETE CASCADE,
    source_machine TEXT NOT NULL,
    cursor TEXT,
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    pages_applied BIGINT NOT NULL DEFAULT 0 CHECK (pages_applied >= 0),
    records_applied BIGINT NOT NULL DEFAULT 0 CHECK (records_applied >= 0),
    complete BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE (account_id, source_machine)
);

CREATE TABLE atmux_pulse.federation_records (
    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL,
    source_machine TEXT NOT NULL,
    record_key TEXT NOT NULL CHECK (char_length(record_key) BETWEEN 1 AND 192),
    fingerprint TEXT NOT NULL CHECK (char_length(fingerprint) = 64),
    received_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (account_id, source_machine, record_key),
    FOREIGN KEY (account_id, source_machine)
        REFERENCES atmux_pulse.federation_peers(account_id, source_machine) ON DELETE CASCADE
);

CREATE INDEX federation_records_peer
    ON atmux_pulse.federation_records(account_id, source_machine, id);
CREATE INDEX federation_profiles_export
    ON atmux_pulse.profiles(account_id, origin, name);
CREATE INDEX federation_usage_export
    ON atmux_pulse.usage_snapshots(account_id, machine, id);
CREATE INDEX federation_context_export
    ON atmux_pulse.context_sessions(account_id, machine, profile, session_id);
CREATE INDEX federation_token_export
    ON atmux_pulse.token_usage(
        account_id, machine, profile, session_id, model, settings_hash, day, source
    );

ALTER TABLE atmux_pulse.federation_peers ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.federation_peers FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.federation_peers AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.federation_records ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.federation_records FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.federation_records AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);
",
    },
    PostgresMigration {
        version: 5,
        sql: r"
ALTER TABLE atmux_pulse.import_provenance
    ADD COLUMN payload_fingerprint TEXT NOT NULL
    DEFAULT '0000000000000000000000000000000000000000000000000000000000000000'
    CHECK (char_length(payload_fingerprint) = 64);

SET LOCAL atmux.pulse_bypass = 'on';

-- A v4 database could record the same logical target through copied legacy
-- sources. Retain the lexicographically first audit witness deterministically;
-- the zero payload fingerprint makes every non-identical future replay fail
-- closed instead of silently accepting unverifiable legacy provenance.
DELETE FROM atmux_pulse.import_provenance AS candidate
USING atmux_pulse.import_provenance AS keeper
WHERE keeper.account_id = candidate.account_id
  AND keeper.source_table = candidate.source_table
  AND keeper.target_key = candidate.target_key
  AND (
      keeper.source_fingerprint < candidate.source_fingerprint
      OR (
          keeper.source_fingerprint = candidate.source_fingerprint
          AND keeper.source_row_id < candidate.source_row_id
      )
  );

CREATE UNIQUE INDEX import_provenance_logical_target
    ON atmux_pulse.import_provenance(account_id, source_table, target_key);
",
    },
    PostgresMigration {
        version: 6,
        sql: r"
CREATE TABLE atmux_pulse.reporter_cursors (
    id BIGSERIAL PRIMARY KEY,
    account_id BIGINT NOT NULL,
    machine TEXT NOT NULL,
    destination_key TEXT NOT NULL CHECK(char_length(destination_key) BETWEEN 1 AND 96),
    usage_after_id BIGINT NOT NULL DEFAULT 0 CHECK(usage_after_id >= 0),
    token_cursor JSONB,
    token_generation BIGINT NOT NULL DEFAULT 0 CHECK(token_generation >= 0),
    UNIQUE(account_id, machine, destination_key),
    FOREIGN KEY(account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.reporter_pending_pages (
    id BIGSERIAL PRIMARY KEY,
    account_id BIGINT NOT NULL,
    machine TEXT NOT NULL,
    destination_key TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('usage', 'token')),
    expected_cursor JSONB NOT NULL,
    next_cursor JSONB NOT NULL,
    chunk_count BIGINT NOT NULL CHECK(chunk_count BETWEEN 1 AND 64),
    total_bytes BIGINT NOT NULL CHECK(total_bytes BETWEEN 1 AND 8388608),
    UNIQUE(account_id, machine, destination_key, kind),
    UNIQUE(id, account_id),
    FOREIGN KEY(account_id, machine, destination_key)
        REFERENCES atmux_pulse.reporter_cursors(account_id, machine, destination_key)
        ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.reporter_pending_chunks (
    pending_id BIGINT NOT NULL,
    account_id BIGINT NOT NULL,
    chunk_index BIGINT NOT NULL CHECK(chunk_index >= 0),
    request_id TEXT NOT NULL CHECK(char_length(request_id) BETWEEN 1 AND 128),
    body BYTEA NOT NULL CHECK(octet_length(body) BETWEEN 1 AND 1048576),
    rows BIGINT NOT NULL CHECK(rows > 0),
    PRIMARY KEY(pending_id, chunk_index),
    FOREIGN KEY(pending_id, account_id)
        REFERENCES atmux_pulse.reporter_pending_pages(id, account_id) ON DELETE CASCADE
);

CREATE INDEX reporter_pending_chunks_account
    ON atmux_pulse.reporter_pending_chunks(account_id, pending_id, chunk_index);

ALTER TABLE atmux_pulse.reporter_cursors ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.reporter_cursors FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.reporter_cursors AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.reporter_pending_pages ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.reporter_pending_pages FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.reporter_pending_pages AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.reporter_pending_chunks ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.reporter_pending_chunks FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.reporter_pending_chunks AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);
",
    },
    PostgresMigration {
        version: 7,
        sql: r"
ALTER TABLE atmux_pulse.token_usage ADD COLUMN write_revision BIGINT NOT NULL DEFAULT 0
    CHECK(write_revision >= 0);

CREATE TABLE atmux_pulse.token_write_revisions (
    account_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 0 CHECK(revision >= 0),
    PRIMARY KEY(account_id, profile, machine),
    FOREIGN KEY(account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY(account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE
);

CREATE TABLE atmux_pulse.backfill_progress (
    account_id BIGINT NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    generation BIGINT NOT NULL CHECK(generation > 0),
    source_generation TEXT NOT NULL CHECK(char_length(source_generation) = 64),
    write_revision BIGINT NOT NULL CHECK(write_revision >= 0),
    cursor JSONB,
    complete BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(account_id, profile, machine),
    FOREIGN KEY(account_id, profile)
        REFERENCES atmux_pulse.profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY(account_id, machine)
        REFERENCES atmux_pulse.machines(account_id, name) ON DELETE CASCADE
);

ALTER TABLE atmux_pulse.backfill_progress ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.backfill_progress FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.backfill_progress AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);

ALTER TABLE atmux_pulse.token_write_revisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE atmux_pulse.token_write_revisions FORCE ROW LEVEL SECURITY;
CREATE POLICY account_scope ON atmux_pulse.token_write_revisions AS PERMISSIVE FOR ALL
USING (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
)
WITH CHECK (
    COALESCE(account_id = NULLIF(current_setting('atmux.account_id', true), '')::BIGINT, FALSE)
    OR COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)
);
",
    },
];

async fn apply_migrations(client: &mut Client) -> PulseResult<u32> {
    let transaction = client.transaction().await.map_err(sql_error)?;
    transaction
        .query_one("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
        .await
        .map_err(sql_error)?;
    transaction
        .batch_execute(
            "CREATE SCHEMA IF NOT EXISTS atmux_pulse; \
             CREATE TABLE IF NOT EXISTS atmux_pulse.pulse_schema_migrations (\
                 version INTEGER PRIMARY KEY CHECK (version > 0), \
                 applied_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()\
             );",
        )
        .await
        .map_err(sql_error)?;
    let current = current_schema_version(&transaction).await?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(PulseError::configuration(format!(
            "Pulse PostgreSQL schema {current} is newer than supported version \
             {LATEST_SCHEMA_VERSION}"
        )));
    }
    for migration in POSTGRES_MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
    {
        transaction
            .batch_execute(migration.sql)
            .await
            .map_err(sql_error)?;
        let version = i32::try_from(migration.version).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "invalid PostgreSQL migration version",
            )
        })?;
        transaction
            .execute(
                "INSERT INTO atmux_pulse.pulse_schema_migrations (version) VALUES ($1)",
                &[&version],
            )
            .await
            .map_err(sql_error)?;
    }
    transaction.commit().await.map_err(sql_error)?;
    Ok(LATEST_SCHEMA_VERSION)
}

async fn current_schema_version<C>(client: &C) -> PulseResult<u32>
where
    C: tokio_postgres::GenericClient + Sync,
{
    let row = client
        .query_one(
            "SELECT COALESCE(MAX(version), 0) FROM atmux_pulse.pulse_schema_migrations",
            &[],
        )
        .await
        .map_err(sql_error)?;
    let version: i32 = row.get(0);
    u32::try_from(version).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "PostgreSQL contains an invalid Pulse schema version",
        )
    })
}

async fn load_pg_federation_state(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    source_machine: &MachineName,
) -> PulseResult<FederationState> {
    let row = transaction
        .query_one(
            "SELECT cursor, generation, pages_applied, records_applied, complete \
             FROM atmux_pulse.federation_peers \
             WHERE account_id=$1 AND source_machine=$2 FOR UPDATE",
            &[&account_id.get(), &source_machine.as_str()],
        )
        .await
        .map_err(sql_error)?;
    Ok(FederationState {
        cursor: row
            .get::<_, Option<String>>(0)
            .map(OpaqueCursor::new)
            .transpose()?,
        generation: as_u64(row.get(1))?,
        pages_applied: as_u64(row.get(2))?,
        records_applied: as_u64(row.get(3))?,
        complete: row.get(4),
    })
}

async fn load_pg_reporter_pending(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    machine: &MachineName,
    destination_key: &str,
    kind: ReporterStreamKind,
) -> PulseResult<Option<ReporterPendingPage>> {
    let row = transaction
        .query_opt(
            "SELECT id,expected_cursor,next_cursor,chunk_count,total_bytes \
             FROM atmux_pulse.reporter_pending_pages WHERE account_id=$1 AND machine=$2 \
             AND destination_key=$3 AND kind=$4 FOR UPDATE",
            &[
                &account_id.get(),
                &machine.as_str(),
                &destination_key,
                &kind.as_str(),
            ],
        )
        .await
        .map_err(sql_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id = row.get::<_, i64>(0);
    let expected = decode_json(row.get::<_, Value>(1))?;
    let next = decode_json(row.get::<_, Value>(2))?;
    let expected_count = usize::try_from(row.get::<_, i64>(3)).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox contains an invalid chunk count",
        )
    })?;
    let expected_bytes = usize::try_from(row.get::<_, i64>(4)).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox contains an invalid byte count",
        )
    })?;
    let rows = transaction
        .query(
            "SELECT request_id,body,rows FROM atmux_pulse.reporter_pending_chunks \
             WHERE account_id=$1 AND pending_id=$2 ORDER BY chunk_index",
            &[&account_id.get(), &id],
        )
        .await
        .map_err(sql_error)?;
    let mut chunks = Vec::with_capacity(rows.len());
    for row in rows {
        chunks.push(ReporterPendingChunk {
            request_id: row.get(0),
            body: row.get(1),
            rows: usize::try_from(row.get::<_, i64>(2)).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "Pulse reporter outbox contains an invalid row count",
                )
            })?,
        });
    }
    if chunks.len() != expected_count
        || chunks.iter().map(|chunk| chunk.body.len()).sum::<usize>() != expected_bytes
    {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox chunk manifest is inconsistent",
        ));
    }
    let draft = ReporterPendingDraft {
        kind,
        expected,
        next,
        chunks,
    };
    draft.validate(account_id, machine)?;
    Ok(Some(ReporterPendingPage { id, draft }))
}

async fn load_pg_reporter_cursor_for_update(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    machine: &MachineName,
    destination_key: &str,
) -> PulseResult<ReporterCursorState> {
    let row = transaction
        .query_opt(
            "SELECT usage_after_id,token_cursor,token_generation \
             FROM atmux_pulse.reporter_cursors WHERE account_id=$1 AND machine=$2 \
             AND destination_key=$3 FOR UPDATE",
            &[&account_id.get(), &machine.as_str(), &destination_key],
        )
        .await
        .map_err(sql_error)?
        .ok_or_else(|| {
            PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse reporter cursor was not initialized",
            )
        })?;
    Ok(ReporterCursorState {
        usage_after_id: row.get(0),
        token_after: row
            .get::<_, Option<Value>>(1)
            .map(decode_json)
            .transpose()?,
        token_generation: as_u64(row.get(2))?,
    })
}

async fn insert_pg_reporter_pending(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    machine: &MachineName,
    destination_key: &str,
    draft: &ReporterPendingDraft,
) -> PulseResult<ReporterPendingPage> {
    let expected = json(&draft.expected)?;
    let next = json(&draft.next)?;
    let chunk_count = i64::try_from(draft.chunks.len())
        .map_err(|_| PulseError::invalid_input("too many reporter chunks"))?;
    let total_bytes = i64::try_from(
        draft
            .chunks
            .iter()
            .map(|chunk| chunk.body.len())
            .sum::<usize>(),
    )
    .map_err(|_| PulseError::invalid_input("reporter outbox is too large"))?;
    let row = transaction
        .query_one(
            "INSERT INTO atmux_pulse.reporter_pending_pages \
             (account_id,machine,destination_key,kind,expected_cursor,next_cursor, \
              chunk_count,total_bytes) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id",
            &[
                &account_id.get(),
                &machine.as_str(),
                &destination_key,
                &draft.kind.as_str(),
                &expected,
                &next,
                &chunk_count,
                &total_bytes,
            ],
        )
        .await
        .map_err(sql_error)?;
    let pending_id = row.get::<_, i64>(0);
    for (index, chunk) in draft.chunks.iter().enumerate() {
        let index = i64::try_from(index)
            .map_err(|_| PulseError::invalid_input("too many reporter chunks"))?;
        let rows = i64::try_from(chunk.rows).map_err(|_| {
            PulseError::invalid_input("Pulse reporter chunk row count is too large")
        })?;
        transaction
            .execute(
                "INSERT INTO atmux_pulse.reporter_pending_chunks \
                 (pending_id,account_id,chunk_index,request_id,body,rows) \
                 VALUES ($1,$2,$3,$4,$5,$6)",
                &[
                    &pending_id,
                    &account_id.get(),
                    &index,
                    &chunk.request_id,
                    &chunk.body,
                    &rows,
                ],
            )
            .await
            .map_err(sql_error)?;
    }
    load_pg_reporter_pending(
        transaction,
        account_id,
        machine,
        destination_key,
        draft.kind,
    )
    .await?
    .ok_or_else(|| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox insert was not visible",
        )
    })
}

async fn commit_pg_reporter_pending(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    machine: &MachineName,
    destination_key: &str,
    pending: ReporterPendingPage,
) -> PulseResult<ReporterCursorState> {
    let next_cursor = pending
        .draft
        .next
        .token_after
        .as_ref()
        .map(json)
        .transpose()?;
    let next_generation = i64::try_from(pending.draft.next.token_generation)
        .map_err(|_| PulseError::invalid_input("Pulse reporter generation is too large"))?;
    let changed = transaction
        .execute(
            "UPDATE atmux_pulse.reporter_cursors SET usage_after_id=$4, \
             token_cursor=$5,token_generation=$6 WHERE account_id=$1 AND machine=$2 \
             AND destination_key=$3",
            &[
                &account_id.get(),
                &machine.as_str(),
                &destination_key,
                &pending.draft.next.usage_after_id,
                &next_cursor,
                &next_generation,
            ],
        )
        .await
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse reporter cursor changed before outbox commit",
        ));
    }
    let removed = transaction
        .execute(
            "DELETE FROM atmux_pulse.reporter_pending_pages \
             WHERE account_id=$1 AND machine=$2 AND destination_key=$3 AND id=$4",
            &[
                &account_id.get(),
                &machine.as_str(),
                &destination_key,
                &pending.id,
            ],
        )
        .await
        .map_err(sql_error)?;
    if removed != 1 {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse reporter outbox page changed concurrently",
        ));
    }
    Ok(pending.draft.next)
}

fn pg_federation_query_limit(limit: usize) -> PulseResult<i64> {
    if limit == 0 || limit > usize::from(MAX_PAGE_ROWS).saturating_add(1) {
        return Err(PulseError::invalid_input(
            "Pulse federation export page limit is out of bounds",
        ));
    }
    i64::try_from(limit)
        .map_err(|_| PulseError::invalid_input("Pulse federation page limit is too large"))
}

fn pg_export_position(
    phase: u8,
    values: impl IntoIterator<Item = String>,
) -> PulseResult<FederationExportPosition> {
    FederationExportPosition::new(phase, values.into_iter().collect())
}

#[expect(
    clippy::too_many_lines,
    reason = "the ordered SQL keyset phases must remain in one transaction and one visible sequence"
)]
async fn pg_local_federation_page(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    local_machine: &MachineName,
    after: Option<FederationExportPosition>,
    limit: usize,
) -> PulseResult<Vec<LocalFederationRecord>> {
    let limit = pg_federation_query_limit(limit)?;
    let after_phase = after.as_ref().map_or(0, |position| position.phase);
    let mut records = Vec::new();
    let machine = if after.is_none() {
        transaction
            .query_opt(
                "SELECT name, first_seen, last_seen FROM atmux_pulse.machines \
                 WHERE account_id=$1 AND name=$2",
                &[&account_id.get(), &local_machine.as_str()],
            )
            .await
            .map_err(sql_error)?
    } else {
        None
    };
    if let Some(row) = machine {
        let name = row.get::<_, String>(0);
        records.push(LocalFederationRecord::new(
            pg_export_position(0, [name.clone()])?,
            FederatedPulseRow::Machine(Machine {
                account_id,
                name: MachineName::new(name)?,
                first_seen: pulse_instant(row.get(1))?,
                last_seen: pulse_instant(row.get(2))?,
            }),
        )?);
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 1 {
        let after_name = if after_phase == 1 {
            let values = &after
                .as_ref()
                .ok_or_else(|| PulseError::invalid_input("missing profile cursor"))?
                .values;
            if values.len() != 1 {
                return Err(PulseError::invalid_input(
                    "Pulse federation profile cursor is invalid",
                ));
            }
            values[0].as_str()
        } else {
            ""
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let origin = json(&ProfileOrigin::Local)?;
        let rows = transaction
            .query(
                "SELECT account_id, name, vendor, config_dir, poll_interval_minutes, \
                 monthly_budget_usd, api_key_env, api_key_file, refresh, hidden, origin \
                 FROM atmux_pulse.profiles WHERE account_id=$1 AND origin=$2 AND name>$3 \
                 ORDER BY name LIMIT $4",
                &[&account_id.get(), &origin, &after_name, &remaining],
            )
            .await
            .map_err(sql_error)?;
        for row in rows {
            let mut profile = decode_profile(&row)?;
            let name = profile.name.as_str().to_owned();
            profile.config_dir = None;
            profile.api_key_env = None;
            profile.api_key_file = None;
            profile.refresh = RefreshPolicy::Never;
            profile.origin = ProfileOrigin::Reported;
            records.push(LocalFederationRecord::new(
                pg_export_position(1, [name])?,
                FederatedPulseRow::Profile(profile),
            )?);
        }
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 2 {
        let after_id = if after_phase == 2 {
            let values = &after
                .as_ref()
                .ok_or_else(|| PulseError::invalid_input("missing usage cursor"))?
                .values;
            if values.len() != 1 {
                return Err(PulseError::invalid_input(
                    "Pulse federation usage cursor is invalid",
                ));
            }
            values[0].parse::<i64>().map_err(|_| {
                PulseError::invalid_input("Pulse federation usage cursor is invalid")
            })?
        } else {
            0
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let rows = transaction
            .query(
                "SELECT id, account_id, profile, machine, vendor, outcome, polled_at, \
                 reporter_version FROM atmux_pulse.usage_snapshots \
                 WHERE account_id=$1 AND machine=$2 AND id>$3 ORDER BY id LIMIT $4",
                &[
                    &account_id.get(),
                    &local_machine.as_str(),
                    &after_id,
                    &remaining,
                ],
            )
            .await
            .map_err(sql_error)?;
        for row in rows {
            let id = row.get::<_, i64>(0);
            records.push(LocalFederationRecord::new(
                pg_export_position(2, [id.to_string()])?,
                FederatedPulseRow::Usage(decode_snapshot(transaction, &row).await?.snapshot),
            )?);
        }
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 3 {
        let (after_profile, after_session) = if after_phase == 3 {
            let values = &after
                .as_ref()
                .ok_or_else(|| PulseError::invalid_input("missing context cursor"))?
                .values;
            if values.len() != 2 {
                return Err(PulseError::invalid_input(
                    "Pulse federation context cursor is invalid",
                ));
            }
            (values[0].as_str(), values[1].as_str())
        } else {
            ("", "")
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let rows = transaction
            .query(
                "SELECT account_id, profile, machine, session_id, model, settings, \
                 context_tokens, context_percent, effective_limit, last_active_at, \
                 last_reset_at, collected_at FROM atmux_pulse.context_sessions \
                 WHERE account_id=$1 AND machine=$2 \
                 AND ROW(profile,session_id)>ROW($3::text,$4::text) \
                 ORDER BY profile,session_id LIMIT $5",
                &[
                    &account_id.get(),
                    &local_machine.as_str(),
                    &after_profile,
                    &after_session,
                    &remaining,
                ],
            )
            .await
            .map_err(sql_error)?;
        for row in rows {
            let position =
                pg_export_position(3, [row.get::<_, String>(1), row.get::<_, String>(3)])?;
            records.push(LocalFederationRecord::new(
                position,
                FederatedPulseRow::Context(decode_context(&row)?),
            )?);
        }
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 4 {
        let empty = [
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ];
        let values = if after_phase == 4 {
            let values = &after
                .as_ref()
                .ok_or_else(|| PulseError::invalid_input("missing token cursor"))?
                .values;
            if values.len() != 6 {
                return Err(PulseError::invalid_input(
                    "Pulse federation token cursor is invalid",
                ));
            }
            values.as_slice()
        } else {
            empty.as_slice()
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let rows = transaction
            .query(
                "SELECT account_id, profile, machine, session_id, model, settings_hash, \
                 settings, day, tokens_in, tokens_out, cache_write_5m, cache_write_1h, \
                 cache_read, source FROM atmux_pulse.token_usage \
                 WHERE account_id=$1 AND machine=$2 AND \
                 ROW(profile,session_id,model,settings_hash,day::text,source::text) \
                   >ROW($3::text,$4::text,$5::text,$6::text,$7::text,$8::text) \
                 ORDER BY profile,session_id,model,settings_hash,day,source::text LIMIT $9",
                &[
                    &account_id.get(),
                    &local_machine.as_str(),
                    &values[0],
                    &values[1],
                    &values[2],
                    &values[3],
                    &values[4],
                    &values[5],
                    &remaining,
                ],
            )
            .await
            .map_err(sql_error)?;
        for row in rows {
            let source = row.get::<_, Value>(13).to_string();
            let position = pg_export_position(
                4,
                [
                    row.get::<_, String>(1),
                    row.get::<_, String>(3),
                    row.get::<_, String>(4),
                    row.get::<_, String>(5),
                    row.get::<_, Date>(7).to_string(),
                    source,
                ],
            )?;
            records.push(LocalFederationRecord::new(
                position,
                FederatedPulseRow::Token(decode_token(&row)?),
            )?);
        }
    }
    Ok(records)
}

impl Store for PostgresStore {
    fn schema_version(&self) -> StoreFuture<u32> {
        self.global_operation(|transaction| {
            Box::pin(async move { current_schema_version(transaction).await })
        })
    }

    fn integrity_check(&self) -> StoreFuture<String> {
        self.global_operation(|transaction| {
            Box::pin(async move {
                let row = transaction
                    .query_one(
                        "SELECT COUNT(*) FROM pg_catalog.pg_class c \
                         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname IN (\
                           'accounts', 'machines', 'profiles', 'usage_snapshots', 'usage_windows', \
                           'context_sessions', 'token_usage', 'pricing_defaults', \
                           'pricing_overrides', 'alert_subscriptions', 'alert_events', \
                           'ingest_tokens', 'gemini_quota', 'import_provenance', \
                           'alert_replies', 'reset_resume_jobs', 'ingest_replays', \
                           'federation_peers', 'federation_records', 'reporter_cursors', \
                           'reporter_pending_pages', 'reporter_pending_chunks', \
                           'backfill_progress', 'token_write_revisions'\
                         )",
                        &[&SCHEMA],
                    )
                    .await
                    .map_err(sql_error)?;
                let tables: i64 = row.get(0);
                if tables != 24 {
                    return Err(PulseError::new(
                        PulseErrorKind::Storage,
                        "PostgreSQL Pulse schema is incomplete",
                    ));
                }
                Ok("ok".to_owned())
            })
        })
    }

    fn upsert_account(&self, account: Account) -> StoreFuture<()> {
        self.account_operation(account.id, move |transaction| {
            Box::pin(async move {
                account.validate()?;
                transaction
                    .execute(
                        "INSERT INTO atmux_pulse.accounts (id, identity, display_name) \
                         VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET \
                         identity = excluded.identity, display_name = excluded.display_name",
                        &[&account.id.get(), &account.identity, &account.display_name],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(())
            })
        })
    }

    fn get_account(&self, account_id: AccountId) -> StoreFuture<Option<Account>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query_opt(
                        "SELECT id, identity, display_name FROM atmux_pulse.accounts WHERE id = $1",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .map(|row| {
                        Ok(Account {
                            id: AccountId::new(row.get(0))?,
                            identity: row.get(1),
                            display_name: row.get(2),
                        })
                    })
                    .transpose()
            })
        })
    }

    fn upsert_machine(&self, machine: Machine) -> StoreFuture<()> {
        self.account_operation(machine.account_id, move |transaction| {
            Box::pin(async move {
                if machine.last_seen < machine.first_seen {
                    return Err(PulseError::invalid_input(
                        "machine last_seen cannot precede first_seen",
                    ));
                }
                let first_seen = pg_timestamp(machine.first_seen)?;
                let last_seen = pg_timestamp(machine.last_seen)?;
                transaction
                    .execute(
                        "INSERT INTO atmux_pulse.machines \
                         (account_id, name, first_seen, last_seen) VALUES ($1, $2, $3, $4) \
                         ON CONFLICT (account_id, name) DO UPDATE SET \
                         first_seen = LEAST(atmux_pulse.machines.first_seen, excluded.first_seen), \
                         last_seen = GREATEST(atmux_pulse.machines.last_seen, excluded.last_seen)",
                        &[
                            &machine.account_id.get(),
                            &machine.name.as_str(),
                            &first_seen,
                            &last_seen,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(())
            })
        })
    }

    fn list_machines(&self, account_id: AccountId) -> StoreFuture<Vec<Machine>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT name, first_seen, last_seen FROM atmux_pulse.machines \
                         WHERE account_id = $1 ORDER BY name LIMIT 10001",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .into_iter()
                    .map(|row| {
                        Ok(Machine {
                            account_id,
                            name: MachineName::new(row.get::<_, String>(0))?,
                            first_seen: pulse_instant(row.get(1))?,
                            last_seen: pulse_instant(row.get(2))?,
                        })
                    })
                    .collect()
            })
        })
    }

    fn upsert_profile(&self, profile: Profile) -> StoreFuture<()> {
        self.account_operation(profile.account_id, move |transaction| {
            Box::pin(async move { upsert_profile(transaction, &profile).await })
        })
    }

    fn get_profile(
        &self,
        account_id: AccountId,
        name: ProfileName,
    ) -> StoreFuture<Option<Profile>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query_opt(
                        "SELECT account_id, name, vendor, config_dir, poll_interval_minutes, \
                         monthly_budget_usd, api_key_env, api_key_file, refresh, hidden, origin \
                         FROM atmux_pulse.profiles WHERE account_id = $1 AND name = $2",
                        &[&account_id.get(), &name.as_str()],
                    )
                    .await
                    .map_err(sql_error)?
                    .map(|row| decode_profile(&row))
                    .transpose()
            })
        })
    }

    fn list_profiles(&self, account_id: AccountId) -> StoreFuture<Vec<Profile>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT account_id, name, vendor, config_dir, poll_interval_minutes, \
                         monthly_budget_usd, api_key_env, api_key_file, refresh, hidden, origin \
                         FROM atmux_pulse.profiles WHERE account_id = $1 ORDER BY name LIMIT 10001",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .iter()
                    .map(decode_profile)
                    .collect()
            })
        })
    }

    fn set_profile_hidden(
        &self,
        account_id: AccountId,
        name: ProfileName,
        hidden: bool,
    ) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .execute(
                        "UPDATE atmux_pulse.profiles SET hidden = $3 \
                         WHERE account_id = $1 AND name = $2",
                        &[&account_id.get(), &name.as_str(), &hidden],
                    )
                    .await
                    .map(|count| count != 0)
                    .map_err(sql_error)
            })
        })
    }

    fn delete_profile(&self, account_id: AccountId, name: ProfileName) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .execute(
                        "DELETE FROM atmux_pulse.profiles WHERE account_id = $1 AND name = $2",
                        &[&account_id.get(), &name.as_str()],
                    )
                    .await
                    .map(|count| count != 0)
                    .map_err(sql_error)
            })
        })
    }

    fn append_usage_snapshot(&self, snapshot: UsageSnapshot) -> StoreFuture<i64> {
        self.account_operation(snapshot.account_id, move |transaction| {
            Box::pin(async move {
                lock_snapshot_profile(transaction, snapshot.account_id, &snapshot.profile).await?;
                insert_snapshot(transaction, &snapshot).await
            })
        })
    }

    fn usage_history(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        since: Option<Instant>,
        limit: usize,
    ) -> StoreFuture<Vec<StoredUsageSnapshot>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let limit = query_limit(limit)?;
                let rows = if let Some(since) = since {
                    let since = pg_timestamp(since)?;
                    transaction
                        .query(
                            "SELECT id, account_id, profile, machine, vendor, outcome, polled_at, \
                             reporter_version FROM atmux_pulse.usage_snapshots \
                             WHERE account_id = $1 AND profile = $2 AND polled_at >= $3 \
                             ORDER BY polled_at DESC, id DESC LIMIT $4",
                            &[&account_id.get(), &profile.as_str(), &since, &limit],
                        )
                        .await
                } else {
                    transaction
                        .query(
                            "SELECT id, account_id, profile, machine, vendor, outcome, polled_at, \
                             reporter_version FROM atmux_pulse.usage_snapshots \
                             WHERE account_id = $1 AND profile = $2 \
                             ORDER BY polled_at DESC, id DESC LIMIT $3",
                            &[&account_id.get(), &profile.as_str(), &limit],
                        )
                        .await
                }
                .map_err(sql_error)?;
                let mut snapshots = Vec::with_capacity(rows.len());
                for row in rows {
                    snapshots.push(decode_snapshot(transaction, &row).await?);
                }
                Ok(snapshots)
            })
        })
    }

    fn current_usage(
        &self,
        account_id: AccountId,
        profile: ProfileName,
    ) -> StoreFuture<Vec<CurrentQuotaWindow>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move { load_current_usage(transaction, account_id, &profile).await })
        })
    }

    fn upsert_context_session(&self, session: ContextSession) -> StoreFuture<()> {
        self.account_operation(session.account_id, move |transaction| {
            Box::pin(async move { upsert_context(transaction, &session).await })
        })
    }

    fn list_context_sessions(
        &self,
        account_id: AccountId,
        profile: Option<ProfileName>,
    ) -> StoreFuture<Vec<ContextSession>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let rows = if let Some(profile) = profile {
                    transaction
                        .query(
                            "SELECT account_id, profile, machine, session_id, model, settings, \
                             context_tokens, context_percent, effective_limit, last_active_at, \
                             last_reset_at, collected_at FROM atmux_pulse.context_sessions \
                             WHERE account_id = $1 AND profile = $2 \
                             ORDER BY last_active_at DESC, profile, machine, session_id LIMIT 10001",
                            &[&account_id.get(), &profile.as_str()],
                        )
                        .await
                } else {
                    transaction
                        .query(
                            "SELECT account_id, profile, machine, session_id, model, settings, \
                             context_tokens, context_percent, effective_limit, last_active_at, \
                             last_reset_at, collected_at FROM atmux_pulse.context_sessions \
                             WHERE account_id = $1 \
                             ORDER BY last_active_at DESC, profile, machine, session_id LIMIT 10001",
                            &[&account_id.get()],
                        )
                        .await
                }
                .map_err(sql_error)?;
                rows.iter().map(decode_context).collect()
            })
        })
    }

    fn upsert_token_grain(&self, grain: TokenGrain) -> StoreFuture<()> {
        self.account_operation(grain.account_id, move |transaction| {
            Box::pin(async move { upsert_token(transaction, &grain).await })
        })
    }

    fn begin_token_observation(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
    ) -> StoreFuture<TokenWriteObservation> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let revision =
                    allocate_pg_token_revision(transaction, account_id, &profile, &machine).await?;
                Ok(TokenWriteObservation {
                    account_id,
                    profile,
                    machine,
                    revision: as_u64(revision)?,
                })
            })
        })
    }

    fn upsert_observed_token_grain(
        &self,
        observation: TokenWriteObservation,
        grain: TokenGrain,
    ) -> StoreFuture<()> {
        let account_id = observation.account_id;
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_token_observation(&observation, &grain)?;
                let reserved = pg_token_write_revision(
                    transaction,
                    observation.account_id,
                    &observation.profile,
                    &observation.machine,
                )
                .await?;
                let revision = i64::try_from(observation.revision).map_err(|_| {
                    PulseError::new(
                        PulseErrorKind::Storage,
                        "token observation revision overflowed",
                    )
                })?;
                if reserved != revision {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse token observation is no longer current",
                    ));
                }
                upsert_token_at_revision(transaction, &grain, revision, true).await
            })
        })
    }

    fn list_token_grains(
        &self,
        account_id: AccountId,
        profile: Option<ProfileName>,
        since_day: Option<String>,
        limit: usize,
    ) -> StoreFuture<Vec<TokenGrain>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let limit = query_limit(limit)?;
                let since = since_day.as_deref().unwrap_or("0001-01-01");
                let since = Date::from_str(since).map_err(|error| {
                    PulseError::invalid_input(format!("invalid since_day: {error}"))
                })?;
                let rows = if let Some(profile) = profile {
                    transaction
                        .query(
                            "SELECT account_id, profile, machine, session_id, model, settings_hash, \
                             settings, day, tokens_in, tokens_out, cache_write_5m, cache_write_1h, \
                             cache_read, source FROM atmux_pulse.token_usage \
                             WHERE account_id = $1 AND day >= $2 AND profile = $3 \
                             ORDER BY day DESC, profile, machine, session_id LIMIT $4",
                            &[&account_id.get(), &since, &profile.as_str(), &limit],
                        )
                        .await
                } else {
                    transaction
                        .query(
                            "SELECT account_id, profile, machine, session_id, model, settings_hash, \
                             settings, day, tokens_in, tokens_out, cache_write_5m, cache_write_1h, \
                             cache_read, source FROM atmux_pulse.token_usage \
                             WHERE account_id = $1 AND day >= $2 \
                             ORDER BY day DESC, profile, machine, session_id LIMIT $3",
                            &[&account_id.get(), &since, &limit],
                        )
                        .await
                }
                .map_err(sql_error)?;
                rows.iter().map(decode_token).collect()
            })
        })
    }

    fn token_totals_by_keys(
        &self,
        account_id: AccountId,
        keys: Vec<TokenReconciliationKey>,
    ) -> StoreFuture<Vec<(TokenReconciliationKey, StoredTokenTotals)>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_reconciliation_keys(&keys)?;
                let mut totals = Vec::with_capacity(keys.len());
                for key in keys {
                    let day = Date::from_str(&key.day).map_err(|_| {
                        PulseError::invalid_input("Pulse token reconciliation day is invalid")
                    })?;
                    let row = transaction
                        .query_one(
                            "SELECT COALESCE(SUM(tokens_in),0)::TEXT, \
                             COALESCE(SUM(tokens_out),0)::TEXT, \
                             COALESCE(SUM(cache_write_5m),0)::TEXT, \
                             COALESCE(SUM(cache_write_1h),0)::TEXT, \
                             COALESCE(SUM(cache_read),0)::TEXT \
                             FROM atmux_pulse.token_usage \
                             WHERE account_id=$1 AND profile=$2 AND day=$3",
                            &[&account_id.get(), &key.profile.as_str(), &day],
                        )
                        .await
                        .map_err(sql_error)?;
                    totals.push((
                        key,
                        StoredTokenTotals {
                            tokens_in: parse_pg_total(&row.get::<_, String>(0))?,
                            tokens_out: parse_pg_total(&row.get::<_, String>(1))?,
                            cache_write_5m: parse_pg_total(&row.get::<_, String>(2))?,
                            cache_write_1h: parse_pg_total(&row.get::<_, String>(3))?,
                            cache_read: parse_pg_total(&row.get::<_, String>(4))?,
                        },
                    ));
                }
                Ok(totals)
            })
        })
    }

    fn upsert_pricing_default(&self, rule: PricingRule) -> StoreFuture<()> {
        self.global_operation(move |transaction| {
            Box::pin(async move { upsert_pricing_default(transaction, &rule).await })
        })
    }

    fn upsert_pricing_override(&self, account_id: AccountId, rule: PricingRule) -> StoreFuture<()> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move { upsert_pricing_override(transaction, account_id, &rule).await })
        })
    }

    fn delete_pricing_override(&self, account_id: AccountId, key: String) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_pricing_key(&key)?;
                transaction
                    .execute(
                        "DELETE FROM atmux_pulse.pricing_overrides \
                         WHERE account_id = $1 AND key = $2",
                        &[&account_id.get(), &key],
                    )
                    .await
                    .map(|deleted| deleted == 1)
                    .map_err(sql_error)
            })
        })
    }

    fn list_pricing_defaults(&self) -> StoreFuture<Vec<PricingRule>> {
        self.global_operation(|transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT key, vendor, model_pattern, settings, input_rate, output_rate, \
                         cache_write_5m_rate, cache_write_1h_rate, cache_read_rate \
                         FROM atmux_pulse.pricing_defaults ORDER BY key LIMIT 10001",
                        &[],
                    )
                    .await
                    .map_err(sql_error)?
                    .iter()
                    .map(decode_pricing)
                    .collect()
            })
        })
    }

    fn list_pricing_overrides(&self, account_id: AccountId) -> StoreFuture<Vec<PricingRule>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT key, vendor, model_pattern, settings, input_rate, output_rate, \
                         cache_write_5m_rate, cache_write_1h_rate, cache_read_rate \
                         FROM atmux_pulse.pricing_overrides WHERE account_id = $1 ORDER BY key LIMIT 10001",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .iter()
                    .map(decode_pricing)
                    .collect()
            })
        })
    }

    fn create_alert_subscription(
        &self,
        subscription: AlertSubscription,
        created_at: Instant,
    ) -> StoreFuture<StoredAlertSubscription> {
        self.account_operation(subscription.account_id, move |transaction| {
            Box::pin(async move {
                subscription.validate()?;
                let alert_type = json(&subscription.alert_type)?;
                let delivery = subscription.delivery.as_ref().map(json).transpose()?;
                let threshold = subscription.threshold.map(Percent::get);
                let threshold_key = threshold
                    .map_or_else(|| "none".to_owned(), |value| format!("{value:.9}"));
                let created = pg_timestamp(created_at)?;
                let cooldown = i32::try_from(subscription.cooldown_minutes).map_err(|_| {
                    PulseError::invalid_input("alert cooldown exceeds PostgreSQL INTEGER")
                })?;
                let row = transaction
                    .query_one(
                        "INSERT INTO atmux_pulse.alert_subscriptions \
                         (account_id, profile, alert_type, threshold, threshold_key, \
                          cooldown_minutes, delivery, enabled, created_at) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
                         ON CONFLICT (account_id, profile, alert_type, threshold_key) DO UPDATE SET \
                         cooldown_minutes = excluded.cooldown_minutes, delivery = excluded.delivery, \
                         enabled = excluded.enabled \
                         RETURNING id, created_at",
                        &[
                            &subscription.account_id.get(),
                            &subscription.profile.as_str(),
                            &alert_type,
                            &threshold,
                            &threshold_key,
                            &cooldown,
                            &delivery,
                            &subscription.enabled,
                            &created,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(StoredAlertSubscription {
                    id: row.get(0),
                    subscription,
                    created_at: pulse_instant(row.get(1))?,
                })
            })
        })
    }

    fn list_alert_subscriptions(
        &self,
        account_id: AccountId,
    ) -> StoreFuture<Vec<StoredAlertSubscription>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT id, account_id, profile, alert_type, threshold, cooldown_minutes, \
                         delivery, enabled, created_at FROM atmux_pulse.alert_subscriptions \
                         WHERE account_id = $1 ORDER BY id LIMIT 10001",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .iter()
                    .map(decode_alert_subscription)
                    .collect()
            })
        })
    }

    fn delete_alert_subscription(
        &self,
        account_id: AccountId,
        subscription_id: i64,
    ) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .execute(
                        "DELETE FROM atmux_pulse.alert_subscriptions \
                         WHERE account_id = $1 AND id = $2",
                        &[&account_id.get(), &subscription_id],
                    )
                    .await
                    .map(|count| count != 0)
                    .map_err(sql_error)
            })
        })
    }

    fn record_alert_if_due(&self, event: AlertEventInput) -> StoreFuture<Option<AlertEvent>> {
        self.account_operation(event.account_id, move |transaction| {
            Box::pin(async move { record_alert_if_due(transaction, &event).await })
        })
    }

    fn list_alert_events(
        &self,
        account_id: AccountId,
        acknowledged: Option<bool>,
    ) -> StoreFuture<Vec<AlertEvent>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let rows = if let Some(acknowledged) = acknowledged {
                    transaction
                        .query(
                            "SELECT id, account_id, subscription_id, profile, alert_type, message, \
                             current_value, threshold, acknowledged, triggered_at \
                             FROM atmux_pulse.alert_events \
                             WHERE account_id = $1 AND acknowledged = $2 \
                             ORDER BY triggered_at DESC, id DESC LIMIT 10001",
                            &[&account_id.get(), &acknowledged],
                        )
                        .await
                } else {
                    transaction
                        .query(
                            "SELECT id, account_id, subscription_id, profile, alert_type, message, \
                             current_value, threshold, acknowledged, triggered_at \
                             FROM atmux_pulse.alert_events WHERE account_id = $1 \
                             ORDER BY triggered_at DESC, id DESC LIMIT 10001",
                            &[&account_id.get()],
                        )
                        .await
                }
                .map_err(sql_error)?;
                rows.iter().map(decode_alert_event).collect()
            })
        })
    }

    fn acknowledge_alert(&self, account_id: AccountId, event_id: i64) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .execute(
                        "UPDATE atmux_pulse.alert_events SET acknowledged = TRUE \
                         WHERE account_id = $1 AND id = $2",
                        &[&account_id.get(), &event_id],
                    )
                    .await
                    .map(|count| count != 0)
                    .map_err(sql_error)
            })
        })
    }

    fn reply_to_alert(&self, reply: AlertReplyInput) -> StoreFuture<Option<AlertReply>> {
        self.account_operation(reply.account_id, move |transaction| {
            Box::pin(async move {
                validate_reply(&reply)?;
                let row = transaction
                    .query_opt(
                        "UPDATE atmux_pulse.alert_events SET acknowledged = TRUE \
                         WHERE account_id = $1 AND id = $2 RETURNING id",
                        &[&reply.account_id.get(), &reply.event_id],
                    )
                    .await
                    .map_err(sql_error)?;
                if row.is_none() {
                    return Ok(None);
                }
                let reply_count = count_row(
                    &transaction
                        .query_one(
                            "SELECT COUNT(*) FROM atmux_pulse.alert_replies \
                             WHERE account_id=$1 AND event_id=$2",
                            &[&reply.account_id.get(), &reply.event_id],
                        )
                        .await
                        .map_err(sql_error)?,
                )?;
                if reply_count >= MAX_ALERT_REPLIES_PER_EVENT {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "alert reply count reached its event cap",
                    ));
                }
                let replied_at = pg_timestamp(reply.replied_at)?;
                let row = transaction
                    .query_one(
                        "INSERT INTO atmux_pulse.alert_replies \
                         (account_id, event_id, message, replied_at) VALUES ($1,$2,$3,$4) \
                         RETURNING id",
                        &[
                            &reply.account_id.get(),
                            &reply.event_id,
                            &reply.message,
                            &replied_at,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(Some(AlertReply {
                    id: row.get(0),
                    account_id: reply.account_id,
                    event_id: reply.event_id,
                    message: reply.message,
                    replied_at: reply.replied_at,
                }))
            })
        })
    }

    fn list_alert_replies(
        &self,
        account_id: AccountId,
        event_id: i64,
    ) -> StoreFuture<Vec<AlertReply>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT id, account_id, event_id, message, replied_at \
                         FROM atmux_pulse.alert_replies WHERE account_id = $1 AND event_id = $2 \
                         ORDER BY replied_at, id LIMIT 256",
                        &[&account_id.get(), &event_id],
                    )
                    .await
                    .map_err(sql_error)?
                    .iter()
                    .map(decode_alert_reply)
                    .collect()
            })
        })
    }

    fn schedule_reset_resume(
        &self,
        input: ResetResumeInput,
        limits: ResetResumeLimits,
    ) -> StoreFuture<ResetResumeJob> {
        self.account_operation(input.account_id, move |transaction| {
            Box::pin(async move {
                let resume_at = validate_reset_input(&input, limits)?;
                transaction
                    .query_one(
                        "SELECT pg_advisory_xact_lock($1)",
                        &[&reset_lock_key(input.account_id)],
                    )
                    .await
                    .map_err(sql_error)?;
                let resets_at = pg_timestamp(input.resets_at)?;
                if let Some(row) = transaction
                    .query_opt(
                        "SELECT id, account_id, profile, resets_at, resume_at, scheduled_at, \
                         lease_until, attempts, delivered_at, cancelled_at \
                         FROM atmux_pulse.reset_resume_jobs \
                         WHERE account_id = $1 AND profile = $2 AND resets_at = $3 FOR UPDATE",
                        &[&input.account_id.get(), &input.profile.as_str(), &resets_at],
                    )
                    .await
                    .map_err(sql_error)?
                {
                    let existing = decode_reset_resume(&row)?;
                    if existing.delivered_at.is_some() || existing.cancelled_at.is_none() {
                        return Ok(existing);
                    }
                }
                let pending = account_table_count(
                    transaction,
                    "pending_reset_resume_jobs",
                    input.account_id,
                )
                .await?;
                if pending >= limits.max_pending_per_account {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "reset resume jobs reached the account cap",
                    ));
                }
                let resume_at = pg_timestamp(resume_at)?;
                let scheduled_at = pg_timestamp(input.scheduled_at)?;
                let row = transaction
                    .query_one(
                        "INSERT INTO atmux_pulse.reset_resume_jobs \
                         (account_id, profile, resets_at, resume_at, scheduled_at, lease_until, \
                          attempts, delivered_at, cancelled_at) \
                         VALUES ($1,$2,$3,$4,$5,NULL,0,NULL,NULL) \
                         ON CONFLICT (account_id, profile, resets_at) DO UPDATE SET \
                         resume_at = excluded.resume_at, scheduled_at = excluded.scheduled_at, \
                         lease_until = NULL, attempts = 0, delivered_at = NULL, cancelled_at = NULL \
                         RETURNING id, account_id, profile, resets_at, resume_at, scheduled_at, \
                                   lease_until, attempts, delivered_at, cancelled_at",
                        &[
                            &input.account_id.get(),
                            &input.profile.as_str(),
                            &resets_at,
                            &resume_at,
                            &scheduled_at,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                decode_reset_resume(&row)
            })
        })
    }

    fn list_pending_reset_resumes(
        &self,
        account_id: AccountId,
        through: Instant,
        limit: usize,
    ) -> StoreFuture<Vec<ResetResumeJob>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let limit = query_limit(limit)?;
                let through = pg_timestamp(through)?;
                transaction
                    .query(
                        "SELECT id, account_id, profile, resets_at, resume_at, scheduled_at, \
                         lease_until, attempts, delivered_at, cancelled_at \
                         FROM atmux_pulse.reset_resume_jobs \
                         WHERE account_id = $1 AND resume_at <= $2 \
                         AND delivered_at IS NULL AND cancelled_at IS NULL \
                         ORDER BY resume_at, id LIMIT $3",
                        &[&account_id.get(), &through, &limit],
                    )
                    .await
                    .map_err(sql_error)?
                    .iter()
                    .map(decode_reset_resume)
                    .collect()
            })
        })
    }

    fn claim_due_reset_resumes(
        &self,
        account_id: AccountId,
        now: Instant,
        lease_until: Instant,
        limit: usize,
    ) -> StoreFuture<Vec<ResetResumeJob>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                if lease_until <= now {
                    return Err(PulseError::invalid_input("reset lease must end after now"));
                }
                let limit = query_limit(limit)?;
                let now = pg_timestamp(now)?;
                let lease_until = pg_timestamp(lease_until)?;
                transaction
                    .query(
                        "WITH due AS ( \
                           SELECT id FROM atmux_pulse.reset_resume_jobs \
                           WHERE account_id = $1 AND resume_at <= $2 \
                           AND delivered_at IS NULL AND cancelled_at IS NULL \
                           AND (lease_until IS NULL OR lease_until <= $2) \
                           ORDER BY resume_at, id FOR UPDATE SKIP LOCKED LIMIT $4 \
                         ) UPDATE atmux_pulse.reset_resume_jobs jobs \
                         SET lease_until = $3, attempts = jobs.attempts + 1 FROM due \
                         WHERE jobs.id = due.id \
                         RETURNING jobs.id, jobs.account_id, jobs.profile, jobs.resets_at, \
                                   jobs.resume_at, jobs.scheduled_at, jobs.lease_until, \
                                   jobs.attempts, jobs.delivered_at, jobs.cancelled_at",
                        &[&account_id.get(), &now, &lease_until, &limit],
                    )
                    .await
                    .map_err(sql_error)?
                    .iter()
                    .map(decode_reset_resume)
                    .collect()
            })
        })
    }

    fn complete_reset_resume(
        &self,
        account_id: AccountId,
        job_id: i64,
        delivered_at: Instant,
    ) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let delivered_at = pg_timestamp(delivered_at)?;
                transaction
                    .execute(
                        "UPDATE atmux_pulse.reset_resume_jobs \
                         SET delivered_at = $3, lease_until = NULL \
                         WHERE account_id = $1 AND id = $2 \
                         AND delivered_at IS NULL AND cancelled_at IS NULL",
                        &[&account_id.get(), &job_id, &delivered_at],
                    )
                    .await
                    .map(|count| count != 0)
                    .map_err(sql_error)
            })
        })
    }

    fn cancel_reset_resumes(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        cancelled_at: Instant,
    ) -> StoreFuture<usize> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let cancelled_at = pg_timestamp(cancelled_at)?;
                transaction
                    .execute(
                        "UPDATE atmux_pulse.reset_resume_jobs \
                         SET cancelled_at = $3, lease_until = NULL \
                         WHERE account_id = $1 AND profile = $2 \
                         AND delivered_at IS NULL AND cancelled_at IS NULL",
                        &[&account_id.get(), &profile.as_str(), &cancelled_at],
                    )
                    .await
                    .map_err(sql_error)
                    .map(|count| usize::try_from(count).unwrap_or(usize::MAX))
            })
        })
    }

    fn insert_ingest_token(&self, token: IngestToken) -> StoreFuture<()> {
        self.account_operation(token.account_id, move |transaction| {
            Box::pin(async move {
                validate_token_hash(&token.token_hash)?;
                if token.id <= 0 {
                    return Err(PulseError::invalid_input(
                        "ingest token id must be positive",
                    ));
                }
                let created = pg_timestamp(token.created_at)?;
                let used = token.last_used_at.map(pg_timestamp).transpose()?;
                let revoked = token.revoked_at.map(pg_timestamp).transpose()?;
                transaction
                    .execute(
                        "INSERT INTO atmux_pulse.ingest_tokens \
                         (id, account_id, machine, token_hash, created_at, last_used_at, revoked_at) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7)",
                        &[
                            &token.id,
                            &token.account_id.get(),
                            &token.machine.as_str(),
                            &token.token_hash,
                            &created,
                            &used,
                            &revoked,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(())
            })
        })
    }

    fn issue_ingest_token(
        &self,
        machine: Machine,
        token: IngestToken,
        max_active_tokens: usize,
    ) -> StoreFuture<()> {
        self.account_operation(token.account_id, move |transaction| {
            Box::pin(async move {
                validate_issued_token(&machine, &token, max_active_tokens)?;
                let lock_key = token.account_id.get() ^ INGEST_TOKEN_LOCK_NAMESPACE;
                transaction
                    .query_one("SELECT pg_advisory_xact_lock($1)", &[&lock_key])
                    .await
                    .map_err(sql_error)?;
                let active = transaction
                    .query_one(
                        "SELECT COUNT(*) FROM atmux_pulse.ingest_tokens \
                         WHERE account_id=$1 AND revoked_at IS NULL",
                        &[&token.account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .get::<_, i64>(0);
                if usize::try_from(active).unwrap_or(usize::MAX) >= max_active_tokens {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse ingest tokens reached the account cap",
                    ));
                }
                let first_seen = pg_timestamp(machine.first_seen)?;
                let last_seen = pg_timestamp(machine.last_seen)?;
                transaction
                    .execute(
                        "INSERT INTO atmux_pulse.machines \
                         (account_id,name,first_seen,last_seen) VALUES ($1,$2,$3,$4) \
                         ON CONFLICT(account_id,name) DO UPDATE SET \
                         first_seen=LEAST(atmux_pulse.machines.first_seen, excluded.first_seen), \
                         last_seen=GREATEST(atmux_pulse.machines.last_seen, excluded.last_seen)",
                        &[
                            &machine.account_id.get(),
                            &machine.name.as_str(),
                            &first_seen,
                            &last_seen,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                let created_at = pg_timestamp(token.created_at)?;
                transaction
                    .execute(
                        "INSERT INTO atmux_pulse.ingest_tokens \
                         (id,account_id,machine,token_hash,created_at,last_used_at,revoked_at) \
                         VALUES ($1,$2,$3,$4,$5,NULL,NULL)",
                        &[
                            &token.id,
                            &token.account_id.get(),
                            &token.machine.as_str(),
                            &token.token_hash,
                            &created_at,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(())
            })
        })
    }

    fn list_ingest_tokens(&self, account_id: AccountId) -> StoreFuture<Vec<IngestToken>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT id, account_id, machine, token_hash, created_at, last_used_at, revoked_at \
                         FROM atmux_pulse.ingest_tokens WHERE account_id = $1 ORDER BY id",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .into_iter()
                    .map(|row| {
                        Ok(IngestToken {
                            id: row.get(0),
                            account_id: AccountId::new(row.get(1))?,
                            machine: MachineName::new(row.get::<_, String>(2))?,
                            token_hash: row.get(3),
                            created_at: pulse_instant(row.get(4))?,
                            last_used_at: row
                                .get::<_, Option<Timestamp>>(5)
                                .map(pulse_instant)
                                .transpose()?,
                            revoked_at: row
                                .get::<_, Option<Timestamp>>(6)
                                .map(pulse_instant)
                                .transpose()?,
                        })
                    })
                    .collect()
            })
        })
    }

    fn get_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
    ) -> StoreFuture<Option<IngestToken>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query_opt(
                        "SELECT id, account_id, machine, token_hash, created_at, last_used_at, revoked_at \
                         FROM atmux_pulse.ingest_tokens WHERE account_id = $1 AND id = $2",
                        &[&account_id.get(), &token_id],
                    )
                    .await
                    .map_err(sql_error)?
                    .map(|row| decode_ingest_token(&row))
                    .transpose()
            })
        })
    }

    fn touch_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
        used_at: Instant,
    ) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let used_at = pg_timestamp(used_at)?;
                transaction
                    .execute(
                        "UPDATE atmux_pulse.ingest_tokens SET last_used_at = \
                         GREATEST(COALESCE(last_used_at, $3), $3) \
                         WHERE account_id = $1 AND id = $2 AND revoked_at IS NULL",
                        &[&account_id.get(), &token_id, &used_at],
                    )
                    .await
                    .map(|count| count != 0)
                    .map_err(sql_error)
            })
        })
    }

    fn revoke_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
        revoked_at: Instant,
    ) -> StoreFuture<bool> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let revoked_at = pg_timestamp(revoked_at)?;
                transaction
                    .execute(
                        "UPDATE atmux_pulse.ingest_tokens SET revoked_at = COALESCE(revoked_at, $3) \
                         WHERE account_id = $1 AND id = $2",
                        &[&account_id.get(), &token_id, &revoked_at],
                    )
                    .await
                    .map(|count| count != 0)
                    .map_err(sql_error)
            })
        })
    }

    fn upsert_gemini_quota(&self, quota: GeminiQuota) -> StoreFuture<()> {
        self.account_operation(quota.account_id, move |transaction| {
            Box::pin(async move { upsert_gemini(transaction, &quota).await })
        })
    }

    fn list_gemini_quotas(&self, account_id: AccountId) -> StoreFuture<Vec<GeminiQuota>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .query(
                        "SELECT account_id, model_id, remaining_fraction, remaining_amount, \
                         resets_at, collected_at FROM atmux_pulse.gemini_quota \
                         WHERE account_id = $1 ORDER BY model_id LIMIT 10001",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .into_iter()
                    .map(|row| {
                        Ok(GeminiQuota {
                            account_id: AccountId::new(row.get(0))?,
                            model_id: row.get(1),
                            remaining_fraction: Fraction::new(row.get(2))?,
                            remaining_amount: row.get(3),
                            resets_at: row
                                .get::<_, Option<Timestamp>>(4)
                                .map(pulse_instant)
                                .transpose()?,
                            collected_at: pulse_instant(row.get(5))?,
                        })
                    })
                    .collect()
            })
        })
    }

    fn record_import(&self, provenance: ImportProvenance) -> StoreFuture<bool> {
        self.account_operation(provenance.account_id, move |transaction| {
            Box::pin(async move {
                lock_import_account(transaction, provenance.account_id).await?;
                claim_import(transaction, &provenance).await
            })
        })
    }

    fn append_imported_usage_snapshot_once(
        &self,
        provenance: ImportProvenance,
        snapshot: UsageSnapshot,
    ) -> StoreFuture<bool> {
        self.account_operation(provenance.account_id, move |transaction| {
            Box::pin(async move {
                snapshot.validate()?;
                if provenance.account_id != snapshot.account_id {
                    return Err(PulseError::invalid_input(
                        "Pulse import provenance and snapshot accounts differ",
                    ));
                }
                lock_import_account(transaction, provenance.account_id).await?;
                let inserted = claim_import(transaction, &provenance).await?;
                if inserted {
                    lock_snapshot_profile(transaction, snapshot.account_id, &snapshot.profile)
                        .await?;
                    insert_snapshot(transaction, &snapshot).await?;
                }
                Ok(inserted)
            })
        })
    }

    fn apply_import_batch_once(&self, batch: ImportBatch) -> StoreFuture<ImportBatchResult> {
        self.account_operation(batch.account_id, move |transaction| {
            Box::pin(async move {
                validate_import_batch(&batch)?;
                lock_import_account(transaction, batch.account_id).await?;
                let mut result = ImportBatchResult::default();

                for machine in &batch.prerequisite_machines {
                    upsert_import_machine(transaction, machine).await?;
                }
                for row in &batch.profiles {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        upsert_profile(transaction, &row.value).await?;
                    }
                    result.profiles.push(inserted);
                }
                for row in &batch.machines {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        upsert_import_machine(transaction, &row.value).await?;
                    }
                    result.machines.push(inserted);
                }
                for row in &batch.snapshots {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        lock_snapshot_profile(
                            transaction,
                            row.value.account_id,
                            &row.value.profile,
                        )
                        .await?;
                        insert_snapshot(transaction, &row.value).await?;
                    }
                    result.snapshots.push(inserted);
                }
                let mut token_writes = Vec::new();
                for row in &batch.token_grains {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        token_writes.push(&row.value);
                    }
                    result.token_grains.push(inserted);
                }
                upsert_token_batch(transaction, token_writes).await?;
                for row in &batch.context_sessions {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        upsert_context(transaction, &row.value).await?;
                    }
                    result.context_sessions.push(inserted);
                }
                for row in &batch.gemini_quotas {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        upsert_gemini(transaction, &row.value).await?;
                    }
                    result.gemini_quotas.push(inserted);
                }
                for row in &batch.pricing_overrides {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        upsert_pricing_override(transaction, batch.account_id, &row.value).await?;
                    }
                    result.pricing_overrides.push(inserted);
                }
                for row in &batch.alert_subscriptions {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        upsert_import_alert_subscription(transaction, &row.value).await?;
                    }
                    result.alert_subscriptions.push(inserted);
                }
                for row in &batch.alert_events {
                    let inserted = claim_import(transaction, &row.provenance).await?;
                    if inserted {
                        insert_import_alert_event(transaction, &row.value).await?;
                    }
                    result.alert_events.push(inserted);
                }
                Ok(result)
            })
        })
    }

    fn begin_token_backfill(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
        source_generation: TokenSourceGeneration,
        restart_completed: bool,
    ) -> StoreFuture<TokenBackfillState> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                begin_pg_token_backfill(
                    transaction,
                    account_id,
                    &profile,
                    &machine,
                    &source_generation,
                    restart_completed,
                )
                .await
            })
        })
    }

    fn apply_token_backfill_page(
        &self,
        page: TokenBackfillPage,
    ) -> StoreFuture<TokenBackfillState> {
        self.account_operation(page.expected.account_id, move |transaction| {
            Box::pin(async move { apply_pg_token_backfill_page(transaction, &page).await })
        })
    }

    fn begin_federation_sync(
        &self,
        account_id: AccountId,
        source_machine: MachineName,
    ) -> StoreFuture<FederationState> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                transaction
                    .execute(
                        "INSERT INTO atmux_pulse.federation_peers (account_id, source_machine) \
                         VALUES ($1,$2) ON CONFLICT (account_id, source_machine) DO NOTHING",
                        &[&account_id.get(), &source_machine.as_str()],
                    )
                    .await
                    .map_err(sql_error)?;
                let mut state =
                    load_pg_federation_state(transaction, account_id, &source_machine).await?;
                if state.complete {
                    transaction
                        .execute(
                            "UPDATE atmux_pulse.federation_peers SET cursor=NULL, \
                             generation=generation+1, complete=FALSE \
                             WHERE account_id=$1 AND source_machine=$2",
                            &[&account_id.get(), &source_machine.as_str()],
                        )
                        .await
                        .map_err(sql_error)?;
                    state.cursor = None;
                    state.generation = state.generation.saturating_add(1);
                    state.complete = false;
                }
                Ok(state)
            })
        })
    }

    fn apply_federation_page(
        &self,
        account_id: AccountId,
        source_machine: MachineName,
        expected_cursor: Option<OpaqueCursor>,
        next_cursor: Option<OpaqueCursor>,
        mut records: Vec<FederatedRecord>,
    ) -> StoreFuture<FederationState> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let mut keys = HashSet::with_capacity(records.len());
                for record in &records {
                    record.validate(account_id, &source_machine)?;
                    if !keys.insert(record.key.clone()) {
                        return Err(PulseError::new(
                            PulseErrorKind::Conflict,
                            "Pulse federation page repeated a record key",
                        ));
                    }
                }
                records.sort_by_key(FederatedRecord::apply_priority);
                let state =
                    load_pg_federation_state(transaction, account_id, &source_machine).await?;
                if state.cursor != expected_cursor || state.complete {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse federation cursor no longer matches durable state",
                    ));
                }

                let mut pending = Vec::new();
                for record in records {
                    let fingerprint = record.fingerprint()?;
                    let existing = transaction
                        .query_opt(
                            "SELECT fingerprint FROM atmux_pulse.federation_records \
                             WHERE account_id=$1 AND source_machine=$2 AND record_key=$3",
                            &[&account_id.get(), &source_machine.as_str(), &record.key],
                        )
                        .await
                        .map_err(sql_error)?
                        .map(|row| row.get::<_, String>(0));
                    match existing {
                        Some(existing) if existing == fingerprint => {}
                        Some(_) => {
                            return Err(PulseError::new(
                                PulseErrorKind::Conflict,
                                "Pulse federation stable key changed its fingerprint",
                            ));
                        }
                        None => pending.push((record, fingerprint)),
                    }
                }

                for (record, _) in &pending {
                    if !matches!(record.row, FederatedPulseRow::Token(_)) {
                        apply_pg_federated_row(transaction, &record.row).await?;
                    }
                }
                upsert_token_batch(
                    transaction,
                    pending.iter().filter_map(|(record, _)| match &record.row {
                        FederatedPulseRow::Token(grain) => Some(grain),
                        _ => None,
                    }),
                )
                .await?;
                for (record, fingerprint) in &pending {
                    transaction
                        .execute(
                            "INSERT INTO atmux_pulse.federation_records \
                             (account_id, source_machine, record_key, fingerprint) \
                             VALUES ($1,$2,$3,$4)",
                            &[
                                &account_id.get(),
                                &source_machine.as_str(),
                                &record.key,
                                fingerprint,
                            ],
                        )
                        .await
                        .map_err(sql_error)?;
                }
                let inserted = i64::try_from(pending.len()).map_err(|_| {
                    PulseError::new(PulseErrorKind::Internal, "federation page count overflow")
                })?;
                let next_cursor_text = next_cursor
                    .as_ref()
                    .map(OpaqueCursor::as_str)
                    .map(str::to_owned);
                let complete = next_cursor.is_none();
                transaction
                    .execute(
                        "UPDATE atmux_pulse.federation_peers SET cursor=$3, \
                         pages_applied=pages_applied+1, records_applied=records_applied+$4, \
                         complete=$5 WHERE account_id=$1 AND source_machine=$2",
                        &[
                            &account_id.get(),
                            &source_machine.as_str(),
                            &next_cursor_text,
                            &inserted,
                            &complete,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                load_pg_federation_state(transaction, account_id, &source_machine).await
            })
        })
    }

    fn local_federation_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after: Option<FederationExportPosition>,
        limit: usize,
    ) -> StoreFuture<Vec<LocalFederationRecord>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                pg_local_federation_page(transaction, account_id, &local_machine, after, limit)
                    .await
            })
        })
    }

    fn load_reporter_cursor(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
    ) -> StoreFuture<ReporterCursorState> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_reporter_destination(&destination_key)?;
                transaction
                    .query_opt(
                        "SELECT id FROM atmux_pulse.accounts WHERE id=$1 FOR UPDATE",
                        &[&account_id.get()],
                    )
                    .await
                    .map_err(sql_error)?
                    .ok_or_else(|| {
                        PulseError::new(PulseErrorKind::NotFound, "Pulse account was not found")
                    })?;
                let exists = transaction
                    .query_opt(
                        "SELECT 1 FROM atmux_pulse.reporter_cursors \
                         WHERE account_id=$1 AND machine=$2 AND destination_key=$3",
                        &[&account_id.get(), &local_machine.as_str(), &destination_key],
                    )
                    .await
                    .map_err(sql_error)?
                    .is_some();
                if !exists {
                    let row = transaction
                        .query_one(
                            "SELECT COUNT(*) FROM atmux_pulse.reporter_cursors WHERE account_id=$1",
                            &[&account_id.get()],
                        )
                        .await
                        .map_err(sql_error)?;
                    let destinations = row.get::<_, i64>(0);
                    let maximum =
                        i64::try_from(MAX_REPORTER_DESTINATIONS_PER_ACCOUNT).map_err(|_| {
                            PulseError::new(
                                PulseErrorKind::Internal,
                                "Pulse reporter destination bound is invalid",
                            )
                        })?;
                    if destinations >= maximum {
                        return Err(PulseError::new(
                            PulseErrorKind::Conflict,
                            "Pulse reporter destination limit was reached for this account",
                        ));
                    }
                    transaction
                        .execute(
                            "INSERT INTO atmux_pulse.reporter_cursors \
                             (account_id,machine,destination_key) VALUES ($1,$2,$3)",
                            &[&account_id.get(), &local_machine.as_str(), &destination_key],
                        )
                        .await
                        .map_err(sql_error)?;
                }
                let row = transaction
                    .query_one(
                        "SELECT usage_after_id,token_cursor,token_generation \
                         FROM atmux_pulse.reporter_cursors WHERE account_id=$1 AND machine=$2 \
                         AND destination_key=$3",
                        &[&account_id.get(), &local_machine.as_str(), &destination_key],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(ReporterCursorState {
                    usage_after_id: row.get(0),
                    token_after: row
                        .get::<_, Option<Value>>(1)
                        .map(decode_json)
                        .transpose()?,
                    token_generation: as_u64(row.get(2))?,
                })
            })
        })
    }

    fn local_reporter_usage_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after_id: i64,
        limit: usize,
    ) -> StoreFuture<Vec<StoredUsageSnapshot>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                if after_id < 0 {
                    return Err(PulseError::invalid_input(
                        "Pulse reporter usage cursor is invalid",
                    ));
                }
                let limit = query_limit(limit)?;
                let rows = transaction
                    .query(
                        "SELECT id,account_id,profile,machine,vendor,outcome,polled_at, \
                         reporter_version FROM atmux_pulse.usage_snapshots \
                         WHERE account_id=$1 AND machine=$2 AND id>$3 ORDER BY id LIMIT $4",
                        &[
                            &account_id.get(),
                            &local_machine.as_str(),
                            &after_id,
                            &limit,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                let mut snapshots = Vec::with_capacity(rows.len());
                for row in rows {
                    snapshots.push(decode_snapshot(transaction, &row).await?);
                }
                Ok(snapshots)
            })
        })
    }

    fn local_reporter_token_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after: Option<ReporterTokenPosition>,
        limit: usize,
    ) -> StoreFuture<Vec<TokenGrain>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                let limit = query_limit(limit)?;
                let rows = if let Some(position) = after {
                    let day = Date::from_str(&position.day).map_err(|error| {
                        PulseError::invalid_input(format!(
                            "invalid Pulse reporter token cursor day: {error}"
                        ))
                    })?;
                    let source =
                        serde_json::from_str::<Value>(&position.source_json).map_err(|_| {
                            PulseError::invalid_input(
                                "Pulse reporter token cursor source is invalid",
                            )
                        })?;
                    transaction
                        .query(
                            "SELECT account_id,profile,machine,session_id,model,settings_hash, \
                             settings,day,tokens_in,tokens_out,cache_write_5m,cache_write_1h, \
                             cache_read,source FROM atmux_pulse.token_usage \
                             WHERE account_id=$1 AND machine=$2 AND \
                             (profile,session_id,model,settings_hash,day,source) \
                             > ($3,$4,$5,$6,$7,$8) \
                             ORDER BY profile,session_id,model,settings_hash,day,source LIMIT $9",
                            &[
                                &account_id.get(),
                                &local_machine.as_str(),
                                &position.profile,
                                &position.session_id,
                                &position.model,
                                &position.settings_hash,
                                &day,
                                &source,
                                &limit,
                            ],
                        )
                        .await
                } else {
                    transaction
                        .query(
                            "SELECT account_id,profile,machine,session_id,model,settings_hash, \
                             settings,day,tokens_in,tokens_out,cache_write_5m,cache_write_1h, \
                             cache_read,source FROM atmux_pulse.token_usage \
                             WHERE account_id=$1 AND machine=$2 \
                             ORDER BY profile,session_id,model,settings_hash,day,source LIMIT $3",
                            &[&account_id.get(), &local_machine.as_str(), &limit],
                        )
                        .await
                }
                .map_err(sql_error)?;
                rows.iter().map(decode_token).collect()
            })
        })
    }

    fn advance_reporter_cursor(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        expected: ReporterCursorState,
        next: ReporterCursorState,
    ) -> StoreFuture<ReporterCursorState> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_reporter_destination(&destination_key)?;
                validate_reporter_transition(&expected, &next)?;
                let expected_cursor = expected.token_after.as_ref().map(json).transpose()?;
                let next_cursor = next.token_after.as_ref().map(json).transpose()?;
                let expected_generation =
                    i64::try_from(expected.token_generation).map_err(|_| {
                        PulseError::invalid_input("Pulse reporter generation is too large")
                    })?;
                let next_generation = i64::try_from(next.token_generation).map_err(|_| {
                    PulseError::invalid_input("Pulse reporter generation is too large")
                })?;
                let changed = transaction
                    .execute(
                        "UPDATE atmux_pulse.reporter_cursors SET usage_after_id=$6, \
                         token_cursor=$7,token_generation=$8 WHERE account_id=$1 AND machine=$2 \
                         AND destination_key=$3 AND usage_after_id=$4 \
                         AND token_cursor IS NOT DISTINCT FROM $5 AND token_generation=$9",
                        &[
                            &account_id.get(),
                            &local_machine.as_str(),
                            &destination_key,
                            &expected.usage_after_id,
                            &expected_cursor,
                            &next.usage_after_id,
                            &next_cursor,
                            &next_generation,
                            &expected_generation,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                if changed != 1 {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse reporter cursor changed concurrently",
                    ));
                }
                Ok(next)
            })
        })
    }

    fn load_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        kind: ReporterStreamKind,
    ) -> StoreFuture<Option<ReporterPendingPage>> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_reporter_destination(&destination_key)?;
                load_pg_reporter_pending(
                    transaction,
                    account_id,
                    &local_machine,
                    &destination_key,
                    kind,
                )
                .await
            })
        })
    }

    fn prepare_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        draft: ReporterPendingDraft,
    ) -> StoreFuture<ReporterPendingPage> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_reporter_destination(&destination_key)?;
                draft.validate(account_id, &local_machine)?;
                let current = load_pg_reporter_cursor_for_update(
                    transaction,
                    account_id,
                    &local_machine,
                    &destination_key,
                )
                .await?;
                if let Some(existing) = load_pg_reporter_pending(
                    transaction,
                    account_id,
                    &local_machine,
                    &destination_key,
                    draft.kind,
                )
                .await?
                {
                    return Ok(existing);
                }
                if current != draft.expected {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse reporter cursor changed before outbox preparation",
                    ));
                }
                insert_pg_reporter_pending(
                    transaction,
                    account_id,
                    &local_machine,
                    &destination_key,
                    &draft,
                )
                .await
            })
        })
    }

    fn commit_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        kind: ReporterStreamKind,
        pending_id: i64,
    ) -> StoreFuture<ReporterCursorState> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_reporter_destination(&destination_key)?;
                let current = load_pg_reporter_cursor_for_update(
                    transaction,
                    account_id,
                    &local_machine,
                    &destination_key,
                )
                .await?;
                let pending = load_pg_reporter_pending(
                    transaction,
                    account_id,
                    &local_machine,
                    &destination_key,
                    kind,
                )
                .await?
                .ok_or_else(|| {
                    PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse reporter outbox page is missing",
                    )
                })?;
                if pending.id != pending_id {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse reporter outbox page changed concurrently",
                    ));
                }
                if current != pending.draft.expected {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse reporter cursor changed before outbox commit",
                    ));
                }
                commit_pg_reporter_pending(
                    transaction,
                    account_id,
                    &local_machine,
                    &destination_key,
                    pending,
                )
                .await
            })
        })
    }

    fn ingest_batch(
        &self,
        account_id: AccountId,
        machine: MachineName,
        batch: IngestBatch,
        limits: IngestLimits,
    ) -> StoreFuture<IngestResult> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_ingest_scope(account_id, &machine, &batch, limits)?;
                transaction
                    .query_one(
                        "SELECT pg_advisory_xact_lock($1)",
                        &[&ingest_lock_key(account_id)],
                    )
                    .await
                    .map_err(sql_error)?;
                enforce_ingest_caps(transaction, account_id, &batch, limits).await?;

                for profile in &batch.profiles {
                    upsert_reported_profile(transaction, profile).await?;
                }
                let mut profiles = batch
                    .snapshots
                    .iter()
                    .map(|snapshot| snapshot.profile.clone())
                    .collect::<Vec<_>>();
                profiles.sort();
                profiles.dedup();
                for profile in &profiles {
                    lock_snapshot_profile(transaction, account_id, profile).await?;
                }
                for snapshot in &batch.snapshots {
                    insert_snapshot(transaction, snapshot).await?;
                }
                upsert_token_batch(transaction, &batch.token_grains).await?;
                for session in &batch.context_sessions {
                    upsert_context(transaction, session).await?;
                }
                for quota in &batch.gemini_quotas {
                    upsert_gemini(transaction, quota).await?;
                }
                Ok(IngestResult {
                    snapshots: batch.snapshots.len(),
                    token_grains: batch.token_grains.len(),
                    context_sessions: batch.context_sessions.len(),
                    gemini_quotas: batch.gemini_quotas.len(),
                })
            })
        })
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_batch_once(
        &self,
        account_id: AccountId,
        machine: MachineName,
        batch: IngestBatch,
        limits: IngestLimits,
        replay: IngestReplay,
    ) -> StoreFuture<IdempotentIngestResult> {
        self.account_operation(account_id, move |transaction| {
            Box::pin(async move {
                validate_ingest_scope(account_id, &machine, &batch, limits)?;
                validate_replay(&replay)?;
                transaction
                    .query_one(
                        "SELECT pg_advisory_xact_lock($1)",
                        &[&ingest_lock_key(account_id)],
                    )
                    .await
                    .map_err(sql_error)?;
                if let Some(row) = transaction
                    .query_opt(
                        "SELECT payload_fingerprint, snapshots, token_grains, \
                         context_sessions, gemini_quotas FROM atmux_pulse.ingest_replays \
                         WHERE account_id = $1 AND machine = $2 AND request_id = $3",
                        &[&account_id.get(), &machine.as_str(), &replay.request_id],
                    )
                    .await
                    .map_err(sql_error)?
                {
                    let fingerprint: String = row.get(0);
                    if fingerprint != replay.payload_fingerprint {
                        return Err(PulseError::new(
                            PulseErrorKind::Conflict,
                            "ingest request id was reused with a different payload",
                        ));
                    }
                    return Ok(IdempotentIngestResult {
                        result: IngestResult {
                            snapshots: count_row_value(row.get(1))?,
                            token_grains: count_row_value(row.get(2))?,
                            context_sessions: count_row_value(row.get(3))?,
                            gemini_quotas: count_row_value(row.get(4))?,
                        },
                        replayed: true,
                    });
                }
                if account_table_count(transaction, "ingest_replays", account_id).await?
                    >= MAX_INGEST_REPLAYS_PER_ACCOUNT
                {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "ingest replay keys reached the account cap",
                    ));
                }
                enforce_ingest_caps(transaction, account_id, &batch, limits).await?;
                for profile in &batch.profiles {
                    upsert_reported_profile(transaction, profile).await?;
                }
                let mut snapshot_profiles = batch
                    .snapshots
                    .iter()
                    .map(|snapshot| snapshot.profile.clone())
                    .collect::<Vec<_>>();
                snapshot_profiles.sort();
                snapshot_profiles.dedup();
                for profile in &snapshot_profiles {
                    lock_snapshot_profile(transaction, account_id, profile).await?;
                }
                for snapshot in &batch.snapshots {
                    insert_snapshot(transaction, snapshot).await?;
                }
                upsert_token_batch(transaction, &batch.token_grains).await?;
                for session in &batch.context_sessions {
                    upsert_context(transaction, session).await?;
                }
                for quota in &batch.gemini_quotas {
                    upsert_gemini(transaction, quota).await?;
                }
                let result = IngestResult {
                    snapshots: batch.snapshots.len(),
                    token_grains: batch.token_grains.len(),
                    context_sessions: batch.context_sessions.len(),
                    gemini_quotas: batch.gemini_quotas.len(),
                };
                let received_at = pg_timestamp(replay.received_at)?;
                let snapshots = result_count(result.snapshots)?;
                let token_grains = result_count(result.token_grains)?;
                let context_sessions = result_count(result.context_sessions)?;
                let gemini_quotas = result_count(result.gemini_quotas)?;
                transaction
                    .execute(
                        "INSERT INTO atmux_pulse.ingest_replays \
                         (account_id, machine, request_id, payload_fingerprint, snapshots, \
                          token_grains, context_sessions, gemini_quotas, received_at) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
                        &[
                            &account_id.get(),
                            &machine.as_str(),
                            &replay.request_id,
                            &replay.payload_fingerprint,
                            &snapshots,
                            &token_grains,
                            &context_sessions,
                            &gemini_quotas,
                            &received_at,
                        ],
                    )
                    .await
                    .map_err(sql_error)?;
                Ok(IdempotentIngestResult {
                    result,
                    replayed: false,
                })
            })
        })
    }

    fn apply_retention(
        &self,
        now: Instant,
        context_days: u16,
        alert_days: u16,
        hourly_after_days: u16,
        daily_after_days: u16,
    ) -> StoreFuture<RetentionResult> {
        self.bypass_operation(move |transaction| {
            Box::pin(async move {
                if context_days == 0
                    || alert_days == 0
                    || hourly_after_days == 0
                    || daily_after_days <= hourly_after_days
                {
                    return Err(PulseError::invalid_input(
                        "retention periods must be nonzero and daily must follow hourly",
                    ));
                }
                let day_ms = 24_i64 * 60 * 60 * 1_000;
                let cutoff = |days: u16| {
                    Instant::from_epoch_millis(
                        now.epoch_millis()
                            .saturating_sub(i64::from(days) * day_ms),
                    )
                    .and_then(pg_timestamp)
                };
                let context_cutoff = cutoff(context_days)?;
                let alert_cutoff = cutoff(alert_days)?;
                let hourly_cutoff = cutoff(hourly_after_days)?;
                let daily_cutoff = cutoff(daily_after_days)?;
                let windows_before = table_count(transaction, "usage_windows").await?;
                let context_sessions = transaction
                    .execute(
                        "DELETE FROM atmux_pulse.context_sessions WHERE last_active_at < $1",
                        &[&context_cutoff],
                    )
                    .await
                    .map_err(sql_error)?;
                let alert_events = transaction
                    .execute(
                        "DELETE FROM atmux_pulse.alert_events WHERE triggered_at < $1",
                        &[&alert_cutoff],
                    )
                    .await
                    .map_err(sql_error)?;
                let daily_removed = transaction
                    .execute(
                        "WITH ranked AS (\
                           SELECT id, ROW_NUMBER() OVER (\
                             PARTITION BY account_id, profile, machine, date_trunc('day', polled_at) \
                             ORDER BY polled_at DESC, id DESC\
                           ) AS rank FROM atmux_pulse.usage_snapshots WHERE polled_at < $1\
                         ) DELETE FROM atmux_pulse.usage_snapshots s USING ranked r \
                         WHERE s.id = r.id AND r.rank > 1",
                        &[&daily_cutoff],
                    )
                    .await
                    .map_err(sql_error)?;
                let hourly_removed = transaction
                    .execute(
                        "WITH ranked AS (\
                           SELECT id, ROW_NUMBER() OVER (\
                             PARTITION BY account_id, profile, machine, date_trunc('hour', polled_at) \
                             ORDER BY polled_at DESC, id DESC\
                           ) AS rank FROM atmux_pulse.usage_snapshots \
                           WHERE polled_at >= $1 AND polled_at < $2\
                         ) DELETE FROM atmux_pulse.usage_snapshots s USING ranked r \
                         WHERE s.id = r.id AND r.rank > 1",
                        &[&daily_cutoff, &hourly_cutoff],
                    )
                    .await
                    .map_err(sql_error)?;
                let windows_after = table_count(transaction, "usage_windows").await?;
                Ok(RetentionResult {
                    context_sessions: usize::try_from(context_sessions).unwrap_or(usize::MAX),
                    usage_windows: windows_before.saturating_sub(windows_after),
                    usage_snapshots: usize::try_from(daily_removed.saturating_add(hourly_removed))
                        .unwrap_or(usize::MAX),
                    alert_events: usize::try_from(alert_events).unwrap_or(usize::MAX),
                })
            })
        })
    }
}

async fn upsert_profile(transaction: &Transaction<'_>, profile: &Profile) -> PulseResult<()> {
    profile.validate()?;
    if profile.origin == ProfileOrigin::Reported {
        return upsert_reported_profile(transaction, profile).await;
    }
    let vendor = json(&profile.vendor)?;
    let refresh = json(&profile.refresh)?;
    let origin = json(&profile.origin)?;
    let config_dir = path_text(profile.config_dir.as_ref(), "config_dir")?;
    let api_key_file = path_text(profile.api_key_file.as_ref(), "api_key_file")?;
    let poll_interval = i32::try_from(profile.poll_interval_minutes)
        .map_err(|_| PulseError::invalid_input("profile poll interval is too large"))?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.profiles \
             (account_id, name, vendor, config_dir, poll_interval_minutes, monthly_budget_usd, \
              api_key_env, api_key_file, refresh, hidden, origin) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) \
             ON CONFLICT (account_id, name) DO UPDATE SET \
             vendor = excluded.vendor, config_dir = excluded.config_dir, \
             poll_interval_minutes = excluded.poll_interval_minutes, \
             monthly_budget_usd = excluded.monthly_budget_usd, \
             api_key_env = excluded.api_key_env, api_key_file = excluded.api_key_file, \
             refresh = excluded.refresh, hidden = excluded.hidden, origin = excluded.origin",
            &[
                &profile.account_id.get(),
                &profile.name.as_str(),
                &vendor,
                &config_dir,
                &poll_interval,
                &profile.monthly_budget_usd,
                &profile.api_key_env,
                &api_key_file,
                &refresh,
                &profile.hidden,
                &origin,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn apply_pg_federated_row(
    transaction: &Transaction<'_>,
    row: &FederatedPulseRow,
) -> PulseResult<()> {
    match row {
        FederatedPulseRow::Machine(machine) => {
            if machine.last_seen < machine.first_seen {
                return Err(PulseError::invalid_input(
                    "machine last_seen cannot precede first_seen",
                ));
            }
            let first_seen = pg_timestamp(machine.first_seen)?;
            let last_seen = pg_timestamp(machine.last_seen)?;
            transaction
                .execute(
                    "INSERT INTO atmux_pulse.machines \
                     (account_id,name,first_seen,last_seen) VALUES ($1,$2,$3,$4) \
                     ON CONFLICT(account_id,name) DO UPDATE SET \
                     first_seen=LEAST(atmux_pulse.machines.first_seen, excluded.first_seen), \
                     last_seen=GREATEST(atmux_pulse.machines.last_seen, excluded.last_seen)",
                    &[
                        &machine.account_id.get(),
                        &machine.name.as_str(),
                        &first_seen,
                        &last_seen,
                    ],
                )
                .await
                .map_err(sql_error)?;
            Ok(())
        }
        FederatedPulseRow::Profile(profile) => upsert_reported_profile(transaction, profile).await,
        FederatedPulseRow::Usage(snapshot) => {
            lock_snapshot_profile(transaction, snapshot.account_id, &snapshot.profile).await?;
            insert_snapshot(transaction, snapshot).await.map(|_| ())
        }
        FederatedPulseRow::Context(session) => upsert_context(transaction, session).await,
        FederatedPulseRow::Token(_) => Ok(()),
    }
}

fn decode_profile(row: &Row) -> PulseResult<Profile> {
    let poll_interval: i32 = row.get(4);
    Ok(Profile {
        account_id: AccountId::new(row.get(0))?,
        name: ProfileName::new(row.get::<_, String>(1))?,
        vendor: decode_json(row.get(2))?,
        config_dir: row.get::<_, Option<String>>(3).map(PathBuf::from),
        poll_interval_minutes: u32::try_from(poll_interval).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "PostgreSQL contains an invalid profile poll interval",
            )
        })?,
        monthly_budget_usd: row.get(5),
        api_key_env: row.get(6),
        api_key_file: row.get::<_, Option<String>>(7).map(PathBuf::from),
        refresh: decode_json(row.get(8))?,
        hidden: row.get(9),
        origin: decode_json(row.get(10))?,
    })
}

async fn upsert_reported_profile(
    transaction: &Transaction<'_>,
    profile: &Profile,
) -> PulseResult<()> {
    profile.validate()?;
    if profile.origin != ProfileOrigin::Reported {
        return Err(PulseError::invalid_input(
            "reported profile must have reported origin",
        ));
    }
    let existing = transaction
        .query_opt(
            "SELECT account_id, name, vendor, config_dir, poll_interval_minutes, \
             monthly_budget_usd, api_key_env, api_key_file, refresh, hidden, origin \
             FROM atmux_pulse.profiles WHERE account_id = $1 AND name = $2 FOR UPDATE",
            &[&profile.account_id.get(), &profile.name.as_str()],
        )
        .await
        .map_err(sql_error)?
        .map(|row| decode_profile(&row))
        .transpose()?;
    if let Some(existing) = existing {
        if existing.vendor != profile.vendor {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "reported profile vendor conflicts with the stored profile",
            ));
        }
        if existing.origin == ProfileOrigin::Local {
            return Ok(());
        }
    }
    let vendor = json(&profile.vendor)?;
    let refresh = json(&profile.refresh)?;
    let origin = json(&profile.origin)?;
    let poll_interval = i32::try_from(profile.poll_interval_minutes)
        .map_err(|_| PulseError::invalid_input("profile poll interval is too large"))?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.profiles \
             (account_id, name, vendor, config_dir, poll_interval_minutes, monthly_budget_usd, \
              api_key_env, api_key_file, refresh, hidden, origin) \
             VALUES ($1,$2,$3,NULL,$4,$5,NULL,NULL,$6,$7,$8) \
             ON CONFLICT (account_id, name) DO UPDATE SET \
             vendor = excluded.vendor, poll_interval_minutes = excluded.poll_interval_minutes, \
             monthly_budget_usd = excluded.monthly_budget_usd, hidden = excluded.hidden, \
             origin = excluded.origin",
            &[
                &profile.account_id.get(),
                &profile.name.as_str(),
                &vendor,
                &poll_interval,
                &profile.monthly_budget_usd,
                &refresh,
                &profile.hidden,
                &origin,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn lock_snapshot_profile(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    profile: &ProfileName,
) -> PulseResult<()> {
    transaction
        .query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, $2))",
            &[&profile.as_str(), &account_id.get()],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn insert_snapshot(
    transaction: &Transaction<'_>,
    snapshot: &UsageSnapshot,
) -> PulseResult<i64> {
    snapshot.validate()?;
    let vendor = json(&snapshot.vendor)?;
    let outcome = json(&snapshot.outcome)?;
    let polled_at = pg_timestamp(snapshot.polled_at)?;
    let row = transaction
        .query_one(
            "INSERT INTO atmux_pulse.usage_snapshots \
             (account_id, profile, machine, vendor, outcome, polled_at, reporter_version) \
             VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
            &[
                &snapshot.account_id.get(),
                &snapshot.profile.as_str(),
                &snapshot.machine.as_str(),
                &vendor,
                &outcome,
                &polled_at,
                &snapshot.reporter_version,
            ],
        )
        .await
        .map_err(sql_error)?;
    let snapshot_id: i64 = row.get(0);
    for window in &snapshot.windows {
        let kind = json(&window.kind)?;
        let previous = transaction
            .query_opt(
                "SELECT w.resets_at, w.used_percent FROM atmux_pulse.usage_windows w \
                 JOIN atmux_pulse.usage_snapshots s ON s.id = w.snapshot_id \
                 WHERE s.account_id = $1 AND s.profile = $2 AND w.kind = $3 AND w.accepted \
                 ORDER BY w.resets_at DESC, s.polled_at DESC, s.id DESC LIMIT 1",
                &[
                    &snapshot.account_id.get(),
                    &snapshot.profile.as_str(),
                    &kind,
                ],
            )
            .await
            .map_err(sql_error)?;
        let accepted = previous.is_none_or(|row| {
            let old_reset: Timestamp = row.get(0);
            let old_percent: f64 = row.get(1);
            window_is_accepted(
                snapshot.vendor,
                window,
                old_reset.as_millisecond(),
                old_percent,
            )
        });
        let resets_at = pg_timestamp(window.resets_at)?;
        transaction
            .execute(
                "INSERT INTO atmux_pulse.usage_windows \
                 (account_id, snapshot_id, kind, used_percent, resets_at, accepted) \
                 VALUES ($1,$2,$3,$4,$5,$6)",
                &[
                    &snapshot.account_id.get(),
                    &snapshot_id,
                    &kind,
                    &window.used_percent.get(),
                    &resets_at,
                    &accepted,
                ],
            )
            .await
            .map_err(sql_error)?;
    }
    Ok(snapshot_id)
}

fn window_is_accepted(
    vendor: Vendor,
    window: &QuotaWindow,
    old_reset: i64,
    old_percent: f64,
) -> bool {
    let new_reset = window.resets_at.epoch_millis();
    if new_reset < old_reset.saturating_sub(RESET_JITTER_TOLERANCE_MS) {
        return false;
    }
    let same_period = new_reset.abs_diff(old_reset)
        <= u64::try_from(RESET_JITTER_TOLERANCE_MS).unwrap_or_default();
    !(same_period
        && vendor.rejects_same_period_decrease(window.kind)
        && window.used_percent.get() < old_percent)
}

async fn decode_snapshot(
    transaction: &Transaction<'_>,
    row: &Row,
) -> PulseResult<StoredUsageSnapshot> {
    let id: i64 = row.get(0);
    let windows = transaction
        .query(
            "SELECT kind, used_percent, resets_at FROM atmux_pulse.usage_windows \
             WHERE snapshot_id = $1 ORDER BY kind",
            &[&id],
        )
        .await
        .map_err(sql_error)?
        .into_iter()
        .map(|window| {
            Ok(QuotaWindow {
                kind: decode_json(window.get(0))?,
                used_percent: Percent::new(window.get(1))?,
                resets_at: pulse_instant(window.get(2))?,
            })
        })
        .collect::<PulseResult<Vec<_>>>()?;
    Ok(StoredUsageSnapshot {
        id,
        snapshot: UsageSnapshot {
            account_id: AccountId::new(row.get(1))?,
            profile: ProfileName::new(row.get::<_, String>(2))?,
            machine: MachineName::new(row.get::<_, String>(3))?,
            vendor: decode_json(row.get(4))?,
            windows,
            outcome: decode_json(row.get(5))?,
            polled_at: pulse_instant(row.get(6))?,
            reporter_version: row.get(7),
        },
    })
}

async fn load_current_usage(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    profile: &ProfileName,
) -> PulseResult<Vec<CurrentQuotaWindow>> {
    let candidates = transaction
        .query(
            "SELECT s.machine, s.vendor, s.reporter_version, s.polled_at, \
             w.kind, w.used_percent, w.resets_at \
             FROM atmux_pulse.usage_windows w \
             JOIN atmux_pulse.usage_snapshots s ON s.id = w.snapshot_id \
             WHERE s.account_id = $1 AND s.profile = $2 AND w.accepted \
             ORDER BY w.kind, w.resets_at DESC, s.polled_at DESC, s.id DESC",
            &[&account_id.get(), &profile.as_str()],
        )
        .await
        .map_err(sql_error)?;
    let mut windows = Vec::<CurrentQuotaWindow>::new();
    let mut keys = Vec::<Value>::new();
    let mut winners = Vec::<(MachineName, Instant)>::new();
    for candidate in candidates {
        let key: Value = candidate.get(4);
        if keys.contains(&key) {
            continue;
        }
        let machine = MachineName::new(candidate.get::<_, String>(0))?;
        let polled_at = pulse_instant(candidate.get(3))?;
        keys.push(key.clone());
        winners.push((machine.clone(), polled_at));
        windows.push(CurrentQuotaWindow {
            profile: profile.clone(),
            vendor: decode_json(candidate.get(1))?,
            window: QuotaWindow {
                kind: decode_json(key)?,
                used_percent: Percent::new(candidate.get(5))?,
                resets_at: pulse_instant(candidate.get(6))?,
            },
            polled_at,
            contributors: Vec::new(),
        });
    }

    let reports = transaction
        .query(
            "SELECT DISTINCT ON (w.kind, s.machine) w.kind, s.machine, s.reporter_version, s.polled_at \
             FROM atmux_pulse.usage_windows w \
             JOIN atmux_pulse.usage_snapshots s ON s.id = w.snapshot_id \
             WHERE s.account_id = $1 AND s.profile = $2 \
             ORDER BY w.kind, s.machine, s.polled_at DESC, s.id DESC",
            &[&account_id.get(), &profile.as_str()],
        )
        .await
        .map_err(sql_error)?;
    for report in reports {
        let key: Value = report.get(0);
        let Some(index) = keys.iter().position(|candidate| candidate == &key) else {
            continue;
        };
        let machine = MachineName::new(report.get::<_, String>(1))?;
        let polled_at = pulse_instant(report.get(3))?;
        windows[index].contributors.push(UsageContributor {
            chosen: winners[index] == (machine.clone(), polled_at),
            machine,
            reporter_version: report.get(2),
            polled_at,
        });
    }
    for window in &mut windows {
        window.contributors.sort_by(|left, right| {
            right
                .chosen
                .cmp(&left.chosen)
                .then_with(|| left.machine.cmp(&right.machine))
        });
    }
    Ok(windows)
}

async fn upsert_context(
    transaction: &Transaction<'_>,
    session: &ContextSession,
) -> PulseResult<()> {
    session.validate()?;
    let settings = json(&session.settings)?;
    let context_tokens = session
        .context_tokens
        .map(|value| as_i64(value, "context_tokens"))
        .transpose()?;
    let effective_limit = session
        .effective_limit
        .map(|value| as_i64(value, "effective_limit"))
        .transpose()?;
    let last_active = pg_timestamp(session.last_active_at)?;
    let last_reset = session.last_reset_at.map(pg_timestamp).transpose()?;
    let collected = pg_timestamp(session.collected_at)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.context_sessions \
             (account_id, profile, machine, session_id, model, settings, context_tokens, \
              context_percent, effective_limit, last_active_at, last_reset_at, collected_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
             ON CONFLICT (account_id, profile, machine, session_id) DO UPDATE SET \
             model = excluded.model, settings = excluded.settings, \
             context_tokens = excluded.context_tokens, context_percent = excluded.context_percent, \
             effective_limit = excluded.effective_limit, last_active_at = excluded.last_active_at, \
             last_reset_at = excluded.last_reset_at, collected_at = excluded.collected_at \
             WHERE excluded.collected_at >= atmux_pulse.context_sessions.collected_at",
            &[
                &session.account_id.get(),
                &session.profile.as_str(),
                &session.machine.as_str(),
                &session.session_id.as_str(),
                &session.model,
                &settings,
                &context_tokens,
                &session.context_percent.map(Percent::get),
                &effective_limit,
                &last_active,
                &last_reset,
                &collected,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn decode_context(row: &Row) -> PulseResult<ContextSession> {
    Ok(ContextSession {
        account_id: AccountId::new(row.get(0))?,
        profile: ProfileName::new(row.get::<_, String>(1))?,
        machine: MachineName::new(row.get::<_, String>(2))?,
        session_id: SessionId::new(row.get::<_, String>(3))?,
        model: row.get(4),
        settings: decode_json(row.get(5))?,
        context_tokens: row.get::<_, Option<i64>>(6).map(as_u64).transpose()?,
        context_percent: row.get::<_, Option<f64>>(7).map(Percent::new).transpose()?,
        effective_limit: row.get::<_, Option<i64>>(8).map(as_u64).transpose()?,
        last_active_at: pulse_instant(row.get(9))?,
        last_reset_at: row
            .get::<_, Option<Timestamp>>(10)
            .map(pulse_instant)
            .transpose()?,
        collected_at: pulse_instant(row.get(11))?,
    })
}

async fn pg_token_write_revision(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
) -> PulseResult<i64> {
    transaction
        .execute(
            "INSERT INTO atmux_pulse.token_write_revisions(account_id,profile,machine,revision) \
             VALUES ($1,$2,$3,0) ON CONFLICT(account_id,profile,machine) DO NOTHING",
            &[&account_id.get(), &profile.as_str(), &machine.as_str()],
        )
        .await
        .map_err(sql_error)?;
    transaction
        .query_one(
            "SELECT revision FROM atmux_pulse.token_write_revisions \
             WHERE account_id=$1 AND profile=$2 AND machine=$3 FOR UPDATE",
            &[&account_id.get(), &profile.as_str(), &machine.as_str()],
        )
        .await
        .map(|row| row.get(0))
        .map_err(sql_error)
}

fn validate_token_observation(
    observation: &TokenWriteObservation,
    grain: &TokenGrain,
) -> PulseResult<()> {
    observation.validate()?;
    grain.validate()?;
    if grain.account_id != observation.account_id
        || grain.profile != observation.profile
        || grain.machine != observation.machine
        || grain.source != crate::pulse::TokenSource::Local
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse token observation is outside its reserved local scope",
        ));
    }
    Ok(())
}

async fn allocate_pg_token_revision(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
) -> PulseResult<i64> {
    let current = pg_token_write_revision(transaction, account_id, profile, machine).await?;
    let revision = current.checked_add(1).ok_or_else(|| {
        PulseError::new(PulseErrorKind::Storage, "token write revision overflowed")
    })?;
    let updated = transaction
        .execute(
            "UPDATE atmux_pulse.token_write_revisions SET revision=$4 \
             WHERE account_id=$1 AND profile=$2 AND machine=$3 AND revision=$5",
            &[
                &account_id.get(),
                &profile.as_str(),
                &machine.as_str(),
                &revision,
                &current,
            ],
        )
        .await
        .map_err(sql_error)?;
    if updated != 1 {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "token write revision changed concurrently",
        ));
    }
    Ok(revision)
}

async fn pg_token_matches(transaction: &Transaction<'_>, grain: &TokenGrain) -> PulseResult<bool> {
    let settings = json(&grain.settings)?;
    let source = json(&grain.source)?;
    let day = Date::from_str(&grain.day)
        .map_err(|error| PulseError::invalid_input(format!("invalid token day: {error}")))?;
    transaction
        .query_opt(
            "SELECT settings=$9 AND tokens_in=$10 AND tokens_out=$11 \
                    AND cache_write_5m=$12 AND cache_write_1h=$13 AND cache_read=$14 \
             FROM atmux_pulse.token_usage WHERE account_id=$1 AND profile=$2 AND machine=$3 \
             AND session_id=$4 AND model=$5 AND settings_hash=$6 AND day=$7 AND source=$8",
            &[
                &grain.account_id.get(),
                &grain.profile.as_str(),
                &grain.machine.as_str(),
                &grain.session_id.as_str(),
                &grain.model,
                &grain.settings_hash,
                &day,
                &source,
                &settings,
                &as_i64(grain.tokens_in, "tokens_in")?,
                &as_i64(grain.tokens_out, "tokens_out")?,
                &as_i64(grain.cache_write_5m, "cache_write_5m")?,
                &as_i64(grain.cache_write_1h, "cache_write_1h")?,
                &as_i64(grain.cache_read, "cache_read")?,
            ],
        )
        .await
        .map(|row| row.is_some_and(|row| row.get(0)))
        .map_err(sql_error)
}

async fn upsert_token_batch<'a>(
    transaction: &Transaction<'_>,
    grains: impl IntoIterator<Item = &'a TokenGrain>,
) -> PulseResult<()> {
    let mut scopes = BTreeMap::<(AccountId, ProfileName, MachineName), Vec<&TokenGrain>>::new();
    for grain in grains {
        grain.validate()?;
        scopes
            .entry((
                grain.account_id,
                grain.profile.clone(),
                grain.machine.clone(),
            ))
            .or_default()
            .push(grain);
    }
    for ((account_id, profile, machine), rows) in scopes {
        let current = pg_token_write_revision(transaction, account_id, &profile, &machine).await?;
        let mut changed = Vec::new();
        for grain in rows {
            if !pg_token_matches(transaction, grain).await? {
                changed.push(grain);
            }
        }
        if changed.is_empty() {
            continue;
        }
        let revision = current.checked_add(1).ok_or_else(|| {
            PulseError::new(PulseErrorKind::Storage, "token write revision overflowed")
        })?;
        transaction
            .execute(
                "UPDATE atmux_pulse.token_write_revisions SET revision=$4 \
                 WHERE account_id=$1 AND profile=$2 AND machine=$3 AND revision=$5",
                &[
                    &account_id.get(),
                    &profile.as_str(),
                    &machine.as_str(),
                    &revision,
                    &current,
                ],
            )
            .await
            .map_err(sql_error)?;
        for grain in changed {
            upsert_token_at_revision(transaction, grain, revision, false).await?;
        }
    }
    Ok(())
}

async fn upsert_token(transaction: &Transaction<'_>, grain: &TokenGrain) -> PulseResult<()> {
    upsert_token_batch(transaction, std::iter::once(grain)).await
}

async fn upsert_token_at_revision(
    transaction: &Transaction<'_>,
    grain: &TokenGrain,
    revision: i64,
    conditional: bool,
) -> PulseResult<()> {
    grain.validate()?;
    let settings = json(&grain.settings)?;
    let source = json(&grain.source)?;
    let day = Date::from_str(&grain.day)
        .map_err(|error| PulseError::invalid_input(format!("invalid token day: {error}")))?;
    let conditional = if conditional {
        " WHERE atmux_pulse.token_usage.write_revision < excluded.write_revision"
    } else {
        ""
    };
    let sql = format!(
        "INSERT INTO atmux_pulse.token_usage \
         (account_id, profile, machine, session_id, model, settings_hash, settings, day, \
          tokens_in, tokens_out, cache_write_5m, cache_write_1h, cache_read, source, updated_at, \
          write_revision) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,clock_timestamp(),$15) \
         ON CONFLICT (account_id, profile, machine, session_id, model, settings_hash, day, source) \
         DO UPDATE SET settings = excluded.settings, tokens_in = excluded.tokens_in, \
         tokens_out = excluded.tokens_out, cache_write_5m = excluded.cache_write_5m, \
         cache_write_1h = excluded.cache_write_1h, cache_read = excluded.cache_read, \
         updated_at = excluded.updated_at, write_revision = excluded.write_revision{conditional}"
    );
    transaction
        .execute(
            &sql,
            &[
                &grain.account_id.get(),
                &grain.profile.as_str(),
                &grain.machine.as_str(),
                &grain.session_id.as_str(),
                &grain.model,
                &grain.settings_hash,
                &settings,
                &day,
                &as_i64(grain.tokens_in, "tokens_in")?,
                &as_i64(grain.tokens_out, "tokens_out")?,
                &as_i64(grain.cache_write_5m, "cache_write_5m")?,
                &as_i64(grain.cache_write_1h, "cache_write_1h")?,
                &as_i64(grain.cache_read, "cache_read")?,
                &source,
                &revision,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn decode_token(row: &Row) -> PulseResult<TokenGrain> {
    let day: Date = row.get(7);
    Ok(TokenGrain {
        account_id: AccountId::new(row.get(0))?,
        profile: ProfileName::new(row.get::<_, String>(1))?,
        machine: MachineName::new(row.get::<_, String>(2))?,
        session_id: SessionId::new(row.get::<_, String>(3))?,
        model: row.get(4),
        settings_hash: row.get(5),
        settings: decode_json(row.get(6))?,
        day: day.to_string(),
        tokens_in: as_u64(row.get(8))?,
        tokens_out: as_u64(row.get(9))?,
        cache_write_5m: as_u64(row.get(10))?,
        cache_write_1h: as_u64(row.get(11))?,
        cache_read: as_u64(row.get(12))?,
        source: decode_json(row.get(13))?,
    })
}

async fn upsert_pricing_default(
    transaction: &Transaction<'_>,
    rule: &PricingRule,
) -> PulseResult<()> {
    rule.validate()?;
    let vendor = json(&rule.vendor)?;
    let settings = json(&rule.settings_match)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.pricing_defaults \
             (key, vendor, model_pattern, settings, input_rate, output_rate, \
              cache_write_5m_rate, cache_write_1h_rate, cache_read_rate) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (key) DO UPDATE SET \
             vendor = excluded.vendor, model_pattern = excluded.model_pattern, \
             settings = excluded.settings, input_rate = excluded.input_rate, \
             output_rate = excluded.output_rate, \
             cache_write_5m_rate = excluded.cache_write_5m_rate, \
             cache_write_1h_rate = excluded.cache_write_1h_rate, \
             cache_read_rate = excluded.cache_read_rate",
            &[
                &rule.key,
                &vendor,
                &rule.model_pattern,
                &settings,
                &rule.input_per_million_usd,
                &rule.output_per_million_usd,
                &rule.cache_write_5m_per_million_usd,
                &rule.cache_write_1h_per_million_usd,
                &rule.cache_read_per_million_usd,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn upsert_pricing_override(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    rule: &PricingRule,
) -> PulseResult<()> {
    rule.validate()?;
    let vendor = json(&rule.vendor)?;
    let settings = json(&rule.settings_match)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.pricing_overrides \
             (account_id, key, vendor, model_pattern, settings, input_rate, output_rate, \
              cache_write_5m_rate, cache_write_1h_rate, cache_read_rate) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
             ON CONFLICT (account_id, key) DO UPDATE SET vendor = excluded.vendor, \
             model_pattern = excluded.model_pattern, settings = excluded.settings, \
             input_rate = excluded.input_rate, output_rate = excluded.output_rate, \
             cache_write_5m_rate = excluded.cache_write_5m_rate, \
             cache_write_1h_rate = excluded.cache_write_1h_rate, \
             cache_read_rate = excluded.cache_read_rate",
            &[
                &account_id.get(),
                &rule.key,
                &vendor,
                &rule.model_pattern,
                &settings,
                &rule.input_per_million_usd,
                &rule.output_per_million_usd,
                &rule.cache_write_5m_per_million_usd,
                &rule.cache_write_1h_per_million_usd,
                &rule.cache_read_per_million_usd,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn alert_threshold_key(threshold: Option<Percent>) -> String {
    threshold.map_or_else(|| "none".to_owned(), |value| format!("{:.9}", value.get()))
}

async fn upsert_import_alert_subscription(
    transaction: &Transaction<'_>,
    stored: &ImportedAlertSubscription,
) -> PulseResult<()> {
    let subscription = &stored.subscription;
    subscription.validate()?;
    let alert_type = json(&subscription.alert_type)?;
    let delivery = subscription.delivery.as_ref().map(json).transpose()?;
    let threshold = subscription.threshold.map(Percent::get);
    let threshold_key = alert_threshold_key(subscription.threshold);
    let created_at = pg_timestamp(stored.created_at)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.alert_subscriptions \
             (account_id, profile, alert_type, threshold, threshold_key, cooldown_minutes, \
              delivery, enabled, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
             ON CONFLICT (account_id, profile, alert_type, threshold_key) DO UPDATE SET \
             cooldown_minutes=excluded.cooldown_minutes, delivery=excluded.delivery, \
             enabled=excluded.enabled",
            &[
                &subscription.account_id.get(),
                &subscription.profile.as_str(),
                &alert_type,
                &threshold,
                &threshold_key,
                &i32::try_from(subscription.cooldown_minutes)
                    .map_err(|_| PulseError::invalid_input("alert cooldown is too large"))?,
                &delivery,
                &subscription.enabled,
                &created_at,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn insert_import_alert_event(
    transaction: &Transaction<'_>,
    imported: &ImportedAlertEvent,
) -> PulseResult<()> {
    let subscription = &imported.subscription;
    let input = &imported.input;
    subscription.validate()?;
    if input.account_id != subscription.account_id
        || input.profile != subscription.profile
        || input.alert_type != subscription.alert_type
        || input.message.is_empty()
        || input.message.len() > 4_096
    {
        return Err(PulseError::invalid_input(
            "imported alert event does not match its bounded subscription",
        ));
    }
    let alert_type = json(&subscription.alert_type)?;
    let threshold_key = alert_threshold_key(subscription.threshold);
    let subscription_id: i64 = transaction
        .query_one(
            "SELECT id FROM atmux_pulse.alert_subscriptions WHERE account_id=$1 AND profile=$2 \
             AND alert_type=$3 AND threshold_key=$4",
            &[
                &subscription.account_id.get(),
                &subscription.profile.as_str(),
                &alert_type,
                &threshold_key,
            ],
        )
        .await
        .map_err(sql_error)?
        .get(0);
    let triggered_at = pg_timestamp(input.triggered_at)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.alert_events \
             (account_id,subscription_id,profile,alert_type,message,current_value,threshold, \
              acknowledged,triggered_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            &[
                &input.account_id.get(),
                &subscription_id,
                &input.profile.as_str(),
                &alert_type,
                &input.message,
                &input.current_value.map(Percent::get),
                &input.threshold.map(Percent::get),
                &imported.acknowledged,
                &triggered_at,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn decode_pricing(row: &Row) -> PulseResult<PricingRule> {
    Ok(PricingRule {
        key: row.get(0),
        vendor: decode_json(row.get(1))?,
        model_pattern: row.get(2),
        settings_match: decode_json(row.get::<_, Value>(3))?,
        input_per_million_usd: row.get(4),
        output_per_million_usd: row.get(5),
        cache_write_5m_per_million_usd: row.get(6),
        cache_write_1h_per_million_usd: row.get(7),
        cache_read_per_million_usd: row.get(8),
    })
}

fn decode_alert_subscription(row: &Row) -> PulseResult<StoredAlertSubscription> {
    let cooldown: i32 = row.get(5);
    Ok(StoredAlertSubscription {
        id: row.get(0),
        subscription: AlertSubscription {
            account_id: AccountId::new(row.get(1))?,
            profile: ProfileName::new(row.get::<_, String>(2))?,
            alert_type: decode_json(row.get(3))?,
            threshold: row.get::<_, Option<f64>>(4).map(Percent::new).transpose()?,
            cooldown_minutes: u32::try_from(cooldown).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "PostgreSQL contains an invalid alert cooldown",
                )
            })?,
            delivery: row
                .get::<_, Option<Value>>(6)
                .map(decode_json)
                .transpose()?,
            enabled: row.get(7),
        },
        created_at: pulse_instant(row.get(8))?,
    })
}

fn decode_alert_event(row: &Row) -> PulseResult<AlertEvent> {
    Ok(AlertEvent {
        id: row.get(0),
        input: AlertEventInput {
            account_id: AccountId::new(row.get(1))?,
            subscription_id: row.get(2),
            profile: ProfileName::new(row.get::<_, String>(3))?,
            alert_type: decode_json(row.get(4))?,
            message: row.get(5),
            current_value: row.get::<_, Option<f64>>(6).map(Percent::new).transpose()?,
            threshold: row.get::<_, Option<f64>>(7).map(Percent::new).transpose()?,
            triggered_at: pulse_instant(row.get(9))?,
        },
        acknowledged: row.get(8),
    })
}

fn validate_reply(reply: &AlertReplyInput) -> PulseResult<()> {
    if reply.event_id <= 0
        || reply.message.is_empty()
        || reply.message.len() > MAX_ALERT_REPLY_BYTES
        || reply
            .message
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(PulseError::invalid_input(
            "alert reply must be nonempty, bounded safe text",
        ));
    }
    Ok(())
}

fn decode_alert_reply(row: &Row) -> PulseResult<AlertReply> {
    Ok(AlertReply {
        id: row.get(0),
        account_id: AccountId::new(row.get(1))?,
        event_id: row.get(2),
        message: row.get(3),
        replied_at: pulse_instant(row.get(4))?,
    })
}

fn validate_reset_input(
    input: &ResetResumeInput,
    limits: ResetResumeLimits,
) -> PulseResult<Instant> {
    if limits.max_pending_per_account == 0
        || limits.max_pending_per_account > MAX_RESET_JOBS_PER_ACCOUNT
        || limits.max_horizon_millis == 0
        || limits.max_horizon_millis > MAX_RESET_HORIZON_MILLIS
    {
        return Err(PulseError::invalid_input(
            "reset resume limits are out of bounds",
        ));
    }
    let reset_delta = input
        .resets_at
        .epoch_millis()
        .checked_sub(input.scheduled_at.epoch_millis())
        .filter(|delta| *delta > 0)
        .ok_or_else(|| PulseError::invalid_input("reset must be in the future"))?;
    let max_horizon = i64::try_from(limits.max_horizon_millis)
        .map_err(|_| PulseError::invalid_input("reset horizon is too large"))?;
    if reset_delta > max_horizon {
        return Err(PulseError::invalid_input(
            "reset exceeds the scheduling horizon",
        ));
    }
    let resume_at = input
        .resets_at
        .epoch_millis()
        .checked_add(60_000)
        .ok_or_else(|| PulseError::invalid_input("reset resume time overflowed"))?;
    Instant::from_epoch_millis(resume_at)
}

const fn reset_lock_key(account_id: AccountId) -> i64 {
    account_id.get() ^ 0x645a_5b37_351d_4cc2
}

fn decode_reset_resume(row: &Row) -> PulseResult<ResetResumeJob> {
    let attempts: i32 = row.get(7);
    Ok(ResetResumeJob {
        id: row.get(0),
        input: ResetResumeInput {
            account_id: AccountId::new(row.get(1))?,
            profile: ProfileName::new(row.get::<_, String>(2))?,
            resets_at: pulse_instant(row.get(3))?,
            scheduled_at: pulse_instant(row.get(5))?,
        },
        resume_at: pulse_instant(row.get(4))?,
        lease_until: row
            .get::<_, Option<Timestamp>>(6)
            .map(pulse_instant)
            .transpose()?,
        attempts: u32::try_from(attempts).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "stored reset attempt count is invalid",
            )
        })?,
        delivered_at: row
            .get::<_, Option<Timestamp>>(8)
            .map(pulse_instant)
            .transpose()?,
        cancelled_at: row
            .get::<_, Option<Timestamp>>(9)
            .map(pulse_instant)
            .transpose()?,
    })
}

async fn record_alert_if_due(
    transaction: &Transaction<'_>,
    event: &AlertEventInput,
) -> PulseResult<Option<AlertEvent>> {
    if event.message.is_empty() || event.message.len() > 4_096 {
        return Err(PulseError::invalid_input(
            "alert message must be between 1 and 4096 bytes",
        ));
    }
    let subscription_row = transaction
        .query_opt(
            "SELECT id, account_id, profile, alert_type, threshold, cooldown_minutes, \
             delivery, enabled, created_at FROM atmux_pulse.alert_subscriptions \
             WHERE account_id = $1 AND id = $2 FOR UPDATE",
            &[&event.account_id.get(), &event.subscription_id],
        )
        .await
        .map_err(sql_error)?
        .ok_or_else(|| PulseError::new(PulseErrorKind::NotFound, "alert subscription not found"))?;
    let subscription = decode_alert_subscription(&subscription_row)?;
    if !subscription.subscription.enabled {
        return Ok(None);
    }
    if subscription.subscription.profile != event.profile
        || subscription.subscription.alert_type != event.alert_type
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "alert event does not match its account-scoped subscription",
        ));
    }
    if subscription.subscription.threshold != event.threshold {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "alert event threshold does not match its account-scoped subscription",
        ));
    }
    match event.alert_type {
        AlertType::FiveHourThreshold
        | AlertType::SevenDayThreshold
        | AlertType::ContextThreshold => {
            let (Some(current), Some(threshold)) = (event.current_value, event.threshold) else {
                return Err(PulseError::invalid_input(
                    "threshold alerts require threshold and current value",
                ));
            };
            if current.get() < threshold.get() {
                return Err(PulseError::invalid_input(
                    "threshold alert current value is below its stored threshold",
                ));
            }
        }
        AlertType::AuthenticationFailure => {
            if event.threshold.is_some() || event.current_value.is_some() {
                return Err(PulseError::invalid_input(
                    "authentication alerts cannot contain threshold values",
                ));
            }
        }
    }
    let last = transaction
        .query_opt(
            "SELECT triggered_at FROM atmux_pulse.alert_events \
             WHERE account_id = $1 AND subscription_id = $2 \
             ORDER BY triggered_at DESC, id DESC LIMIT 1",
            &[&event.account_id.get(), &event.subscription_id],
        )
        .await
        .map_err(sql_error)?
        .map(|row| row.get::<_, Timestamp>(0).as_millisecond());
    let cooldown_ms = i64::from(subscription.subscription.cooldown_minutes) * 60 * 1_000;
    if last.is_some_and(|last| event.triggered_at.epoch_millis() < last.saturating_add(cooldown_ms))
    {
        return Ok(None);
    }
    let alert_type = json(&event.alert_type)?;
    let triggered_at = pg_timestamp(event.triggered_at)?;
    let row = transaction
        .query_one(
            "INSERT INTO atmux_pulse.alert_events \
             (account_id, subscription_id, profile, alert_type, message, current_value, \
              threshold, acknowledged, triggered_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,FALSE,$8) RETURNING id",
            &[
                &event.account_id.get(),
                &event.subscription_id,
                &event.profile.as_str(),
                &alert_type,
                &event.message,
                &event.current_value.map(Percent::get),
                &event.threshold.map(Percent::get),
                &triggered_at,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(Some(AlertEvent {
        id: row.get(0),
        input: event.clone(),
        acknowledged: false,
    }))
}

async fn upsert_gemini(transaction: &Transaction<'_>, quota: &GeminiQuota) -> PulseResult<()> {
    quota.validate()?;
    let resets_at = quota.resets_at.map(pg_timestamp).transpose()?;
    let collected_at = pg_timestamp(quota.collected_at)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.gemini_quota \
             (account_id, model_id, remaining_fraction, remaining_amount, resets_at, collected_at) \
             VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (account_id, model_id) DO UPDATE SET \
             remaining_fraction = excluded.remaining_fraction, \
             remaining_amount = excluded.remaining_amount, resets_at = excluded.resets_at, \
             collected_at = excluded.collected_at \
             WHERE excluded.collected_at >= atmux_pulse.gemini_quota.collected_at",
            &[
                &quota.account_id.get(),
                &quota.model_id,
                &quota.remaining_fraction.get(),
                &quota.remaining_amount,
                &resets_at,
                &collected_at,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn validate_token_hash(value: &str) -> PulseResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PulseError::invalid_input(
            "ingest token hash must be lowercase SHA-256 hex",
        ));
    }
    Ok(())
}

fn decode_ingest_token(row: &Row) -> PulseResult<IngestToken> {
    Ok(IngestToken {
        id: row.get(0),
        account_id: AccountId::new(row.get(1))?,
        machine: MachineName::new(row.get::<_, String>(2))?,
        token_hash: row.get(3),
        created_at: pulse_instant(row.get(4))?,
        last_used_at: row
            .get::<_, Option<Timestamp>>(5)
            .map(pulse_instant)
            .transpose()?,
        revoked_at: row
            .get::<_, Option<Timestamp>>(6)
            .map(pulse_instant)
            .transpose()?,
    })
}

fn validate_issued_token(
    machine: &Machine,
    token: &IngestToken,
    max_active_tokens: usize,
) -> PulseResult<()> {
    validate_token_hash(&token.token_hash)?;
    if token.id <= 0
        || machine.account_id != token.account_id
        || machine.name != token.machine
        || machine.last_seen < machine.first_seen
        || token.last_used_at.is_some()
        || token.revoked_at.is_some()
        || max_active_tokens == 0
        || max_active_tokens > MAX_QUERY_ROWS
    {
        return Err(PulseError::invalid_input(
            "atomic ingest token issuance input is invalid",
        ));
    }
    Ok(())
}

async fn load_pg_token_backfill(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
) -> PulseResult<Option<TokenBackfillState>> {
    let row = transaction
        .query_opt(
            "SELECT generation,source_generation,write_revision,cursor,complete \
             FROM atmux_pulse.backfill_progress \
             WHERE account_id=$1 AND profile=$2 AND machine=$3 FOR UPDATE",
            &[&account_id.get(), &profile.as_str(), &machine.as_str()],
        )
        .await
        .map_err(sql_error)?;
    row.map(|row| {
        let state = TokenBackfillState {
            account_id,
            profile: profile.clone(),
            machine: machine.clone(),
            generation: as_u64(row.get::<_, i64>(0))?,
            source_generation: TokenSourceGeneration::new(row.get::<_, String>(1))?,
            write_revision: as_u64(row.get::<_, i64>(2))?,
            cursor: row
                .get::<_, Option<Value>>(3)
                .map(decode_json)
                .transpose()?,
            complete: row.get(4),
        };
        state.validate()?;
        Ok(state)
    })
    .transpose()
}

async fn begin_pg_token_backfill(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
    source_generation: &TokenSourceGeneration,
    restart_completed: bool,
) -> PulseResult<TokenBackfillState> {
    source_generation.validate()?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.backfill_progress \
             (account_id,profile,machine,generation,source_generation,write_revision,cursor,complete) \
             VALUES ($1,$2,$3,1,$4,0,NULL,FALSE) \
             ON CONFLICT (account_id,profile,machine) DO NOTHING",
            &[
                &account_id.get(),
                &profile.as_str(),
                &machine.as_str(),
                &source_generation.as_str(),
            ],
        )
        .await
        .map_err(sql_error)?;
    let mut state = load_pg_token_backfill(transaction, account_id, profile, machine)
        .await?
        .ok_or_else(|| PulseError::new(PulseErrorKind::Storage, "backfill insert failed"))?;
    if state.write_revision == 0 {
        state.write_revision =
            as_u64(allocate_pg_token_revision(transaction, account_id, profile, machine).await?)?;
        transaction
            .execute(
                "UPDATE atmux_pulse.backfill_progress SET write_revision=$4 \
                 WHERE account_id=$1 AND profile=$2 AND machine=$3 AND write_revision=0",
                &[
                    &account_id.get(),
                    &profile.as_str(),
                    &machine.as_str(),
                    &i64::try_from(state.write_revision).map_err(|_| {
                        PulseError::new(
                            PulseErrorKind::Storage,
                            "backfill write revision overflowed",
                        )
                    })?,
                ],
            )
            .await
            .map_err(sql_error)?;
    }
    if state.source_generation != *source_generation || (state.complete && restart_completed) {
        let write_revision =
            allocate_pg_token_revision(transaction, account_id, profile, machine).await?;
        state.generation = state.generation.checked_add(1).ok_or_else(|| {
            PulseError::new(PulseErrorKind::Storage, "backfill generation overflowed")
        })?;
        state.source_generation = source_generation.clone();
        state.write_revision = as_u64(write_revision)?;
        state.cursor = None;
        state.complete = false;
        let generation = i64::try_from(state.generation).map_err(|_| {
            PulseError::new(PulseErrorKind::Storage, "backfill generation overflowed")
        })?;
        transaction
            .execute(
                "UPDATE atmux_pulse.backfill_progress SET generation=$4, \
                 source_generation=$5,write_revision=$6,cursor=NULL,complete=FALSE, \
                 updated_at=clock_timestamp() \
                 WHERE account_id=$1 AND profile=$2 AND machine=$3",
                &[
                    &account_id.get(),
                    &profile.as_str(),
                    &machine.as_str(),
                    &generation,
                    &source_generation.as_str(),
                    &write_revision,
                ],
            )
            .await
            .map_err(sql_error)?;
    }
    Ok(state)
}

async fn apply_pg_token_backfill_page(
    transaction: &Transaction<'_>,
    page: &TokenBackfillPage,
) -> PulseResult<TokenBackfillState> {
    page.validate()?;
    let current = load_pg_token_backfill(
        transaction,
        page.expected.account_id,
        &page.expected.profile,
        &page.expected.machine,
    )
    .await?
    .ok_or_else(|| PulseError::new(PulseErrorKind::Conflict, "backfill cursor is missing"))?;
    if current != page.expected {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "backfill cursor changed concurrently",
        ));
    }
    let revision = i64::try_from(page.expected.write_revision).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "backfill write revision overflowed",
        )
    })?;
    for row in &page.rows {
        upsert_token_at_revision(transaction, row, revision, true).await?;
    }
    let cursor = page.next_cursor.as_ref().map(json).transpose()?;
    transaction
        .execute(
            "UPDATE atmux_pulse.backfill_progress SET cursor=$4,complete=$5, \
             updated_at=clock_timestamp() WHERE account_id=$1 AND profile=$2 AND machine=$3",
            &[
                &page.expected.account_id.get(),
                &page.expected.profile.as_str(),
                &page.expected.machine.as_str(),
                &cursor,
                &page.complete,
            ],
        )
        .await
        .map_err(sql_error)?;
    let mut next = page.expected.clone();
    next.cursor.clone_from(&page.next_cursor);
    next.complete = page.complete;
    Ok(next)
}

fn validate_import(provenance: &ImportProvenance) -> PulseResult<()> {
    validate_token_hash(&provenance.source_fingerprint)?;
    validate_token_hash(&provenance.payload_fingerprint)?;
    for (name, value, limit) in [
        ("source_table", provenance.source_table.as_str(), 128),
        ("source_row_id", provenance.source_row_id.as_str(), 256),
        ("target_key", provenance.target_key.as_str(), 1_024),
    ] {
        if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
            return Err(PulseError::invalid_input(format!(
                "import {name} must be nonempty, bounded text"
            )));
        }
    }
    Ok(())
}

fn validate_reconciliation_keys(keys: &[TokenReconciliationKey]) -> PulseResult<()> {
    if keys.is_empty() || keys.len() > MAX_IMPORT_RECONCILIATION_KEYS {
        return Err(PulseError::invalid_input(
            "Pulse token reconciliation key count is outside its bounds",
        ));
    }
    let mut unique = HashSet::with_capacity(keys.len());
    for key in keys {
        Date::from_str(&key.day)
            .map_err(|_| PulseError::invalid_input("Pulse token reconciliation day is invalid"))?;
        if !unique.insert((key.profile.clone(), key.day.clone())) {
            return Err(PulseError::invalid_input(
                "Pulse token reconciliation contains duplicate keys",
            ));
        }
    }
    Ok(())
}

fn parse_pg_total(value: &str) -> PulseResult<u128> {
    value.parse::<u128>().map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "stored PostgreSQL Pulse token aggregate is invalid",
        )
    })
}

fn validate_import_batch(batch: &ImportBatch) -> PulseResult<()> {
    if batch.row_count() == 0 || batch.row_count() > MAX_IMPORT_BATCH_ROWS {
        return Err(PulseError::invalid_input(
            "Pulse import batch exceeds its bounded row limit",
        ));
    }
    if batch
        .prerequisite_machines
        .iter()
        .any(|row| row.account_id != batch.account_id)
        || batch
            .profiles
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .machines
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .snapshots
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .token_grains
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .context_sessions
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .gemini_quotas
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .pricing_overrides
            .iter()
            .any(|row| row.provenance.account_id != batch.account_id)
        || batch.alert_subscriptions.iter().any(|row| {
            !import_row_matches(row, batch.account_id, |value| value.subscription.account_id)
        })
        || batch.alert_events.iter().any(|row| {
            !import_row_matches(row, batch.account_id, |value| value.input.account_id)
                || row.value.subscription.account_id != batch.account_id
        })
    {
        return Err(PulseError::invalid_input(
            "Pulse import batch contains a cross-account row",
        ));
    }
    for provenance in batch
        .profiles
        .iter()
        .map(|row| &row.provenance)
        .chain(batch.machines.iter().map(|row| &row.provenance))
        .chain(batch.snapshots.iter().map(|row| &row.provenance))
        .chain(batch.token_grains.iter().map(|row| &row.provenance))
        .chain(batch.context_sessions.iter().map(|row| &row.provenance))
        .chain(batch.gemini_quotas.iter().map(|row| &row.provenance))
        .chain(batch.pricing_overrides.iter().map(|row| &row.provenance))
        .chain(batch.alert_subscriptions.iter().map(|row| &row.provenance))
        .chain(batch.alert_events.iter().map(|row| &row.provenance))
    {
        validate_import(provenance)?;
    }
    Ok(())
}

fn import_row_matches<T>(
    row: &ImportedRow<T>,
    account_id: AccountId,
    value_account: impl FnOnce(&T) -> AccountId,
) -> bool {
    row.provenance.account_id == account_id && value_account(&row.value) == account_id
}

async fn lock_import_account(
    transaction: &Transaction<'_>,
    account_id: AccountId,
) -> PulseResult<()> {
    transaction
        .query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended('atmux-pulse-import', $1))",
            &[&account_id.get()],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

async fn claim_import(
    transaction: &Transaction<'_>,
    provenance: &ImportProvenance,
) -> PulseResult<bool> {
    validate_import(provenance)?;
    let existing = transaction
        .query_opt(
            "SELECT payload_fingerprint FROM atmux_pulse.import_provenance \
             WHERE account_id=$1 AND source_table=$2 AND target_key=$3 FOR UPDATE",
            &[
                &provenance.account_id.get(),
                &provenance.source_table,
                &provenance.target_key,
            ],
        )
        .await
        .map_err(sql_error)?;
    if let Some(existing) = existing {
        if existing.get::<_, String>(0) == provenance.payload_fingerprint {
            return Ok(false);
        }
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse import logical row conflicts with previously imported content",
        ));
    }
    let imported_at = pg_timestamp(provenance.imported_at)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.import_provenance \
             (account_id, source_fingerprint, source_table, source_row_id, target_key, \
              payload_fingerprint, imported_at) VALUES ($1,$2,$3,$4,$5,$6,$7)",
            &[
                &provenance.account_id.get(),
                &provenance.source_fingerprint,
                &provenance.source_table,
                &provenance.source_row_id,
                &provenance.target_key,
                &provenance.payload_fingerprint,
                &imported_at,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(true)
}

async fn upsert_import_machine(
    transaction: &Transaction<'_>,
    machine: &Machine,
) -> PulseResult<()> {
    if machine.last_seen < machine.first_seen {
        return Err(PulseError::invalid_input(
            "machine last_seen cannot precede first_seen",
        ));
    }
    let first_seen = pg_timestamp(machine.first_seen)?;
    let last_seen = pg_timestamp(machine.last_seen)?;
    transaction
        .execute(
            "INSERT INTO atmux_pulse.machines \
             (account_id, name, first_seen, last_seen) VALUES ($1,$2,$3,$4) \
             ON CONFLICT (account_id, name) DO UPDATE SET \
             first_seen=LEAST(atmux_pulse.machines.first_seen, excluded.first_seen), \
             last_seen=GREATEST(atmux_pulse.machines.last_seen, excluded.last_seen)",
            &[
                &machine.account_id.get(),
                &machine.name.as_str(),
                &first_seen,
                &last_seen,
            ],
        )
        .await
        .map_err(sql_error)?;
    Ok(())
}

fn validate_ingest_scope(
    account_id: AccountId,
    machine: &MachineName,
    batch: &IngestBatch,
    limits: IngestLimits,
) -> PulseResult<()> {
    if batch.row_count() > limits.max_rows_per_request {
        return Err(PulseError::invalid_input(
            "ingest request exceeds its row limit",
        ));
    }
    let snapshots = batch
        .snapshots
        .iter()
        .all(|row| row.account_id == account_id && &row.machine == machine);
    let tokens = batch
        .token_grains
        .iter()
        .all(|row| row.account_id == account_id && &row.machine == machine);
    let contexts = batch
        .context_sessions
        .iter()
        .all(|row| row.account_id == account_id && &row.machine == machine);
    let gemini = batch
        .gemini_quotas
        .iter()
        .all(|row| row.account_id == account_id);
    let profiles = batch
        .profiles
        .iter()
        .all(|row| row.account_id == account_id && row.origin == ProfileOrigin::Reported);
    if !(profiles && snapshots && tokens && contexts && gemini) {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "ingest rows must match the token-authoritative account and machine",
        ));
    }
    for profile in &batch.profiles {
        profile.validate()?;
    }
    Ok(())
}

const fn ingest_lock_key(account_id: AccountId) -> i64 {
    account_id.get() ^ 0x315d_4843_af5e_5457
}

#[allow(clippy::too_many_lines)]
async fn enforce_ingest_caps(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    batch: &IngestBatch,
    limits: IngestLimits,
) -> PulseResult<()> {
    let profiles = account_table_count(transaction, "profiles", account_id).await?;
    let snapshots = account_table_count(transaction, "usage_snapshots", account_id).await?;
    let tokens = account_table_count(transaction, "token_usage", account_id).await?;
    let contexts = account_table_count(transaction, "context_sessions", account_id).await?;
    let gemini = account_table_count(transaction, "gemini_quota", account_id).await?;

    let mut profile_keys = HashSet::new();
    let mut new_profiles = 0_usize;
    for profile in &batch.profiles {
        if profile_keys.insert(profile.name.clone()) {
            let exists: bool = transaction
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM atmux_pulse.profiles \
                     WHERE account_id = $1 AND name = $2)",
                    &[&profile.account_id.get(), &profile.name.as_str()],
                )
                .await
                .map_err(sql_error)?
                .get(0);
            new_profiles += usize::from(!exists);
        }
    }

    let mut token_keys = HashSet::new();
    let mut new_tokens = 0_usize;
    for grain in &batch.token_grains {
        let source = json(&grain.source)?;
        let day = Date::from_str(&grain.day)
            .map_err(|error| PulseError::invalid_input(format!("invalid token day: {error}")))?;
        let key = serde_json::to_string(&(
            grain.account_id,
            &grain.profile,
            &grain.machine,
            &grain.session_id,
            &grain.model,
            &grain.settings_hash,
            &grain.day,
            &grain.source,
        ))
        .map_err(|_| PulseError::new(PulseErrorKind::Internal, "failed to key token row"))?;
        if token_keys.insert(key) {
            let exists: bool = transaction
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM atmux_pulse.token_usage \
                     WHERE account_id=$1 AND profile=$2 AND machine=$3 AND session_id=$4 \
                     AND model=$5 AND settings_hash=$6 AND day=$7 AND source=$8)",
                    &[
                        &grain.account_id.get(),
                        &grain.profile.as_str(),
                        &grain.machine.as_str(),
                        &grain.session_id.as_str(),
                        &grain.model,
                        &grain.settings_hash,
                        &day,
                        &source,
                    ],
                )
                .await
                .map_err(sql_error)?
                .get(0);
            new_tokens += usize::from(!exists);
        }
    }

    let mut context_keys = HashSet::new();
    let mut new_contexts = 0_usize;
    for session in &batch.context_sessions {
        let key = (
            session.profile.as_str().to_owned(),
            session.machine.as_str().to_owned(),
            session.session_id.as_str().to_owned(),
        );
        if context_keys.insert(key) {
            let exists: bool = transaction
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM atmux_pulse.context_sessions \
                     WHERE account_id=$1 AND profile=$2 AND machine=$3 AND session_id=$4)",
                    &[
                        &session.account_id.get(),
                        &session.profile.as_str(),
                        &session.machine.as_str(),
                        &session.session_id.as_str(),
                    ],
                )
                .await
                .map_err(sql_error)?
                .get(0);
            new_contexts += usize::from(!exists);
        }
    }

    let mut gemini_keys = HashSet::new();
    let mut new_gemini = 0_usize;
    for quota in &batch.gemini_quotas {
        if gemini_keys.insert(quota.model_id.clone()) {
            let exists: bool = transaction
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM atmux_pulse.gemini_quota \
                     WHERE account_id=$1 AND model_id=$2)",
                    &[&quota.account_id.get(), &quota.model_id],
                )
                .await
                .map_err(sql_error)?
                .get(0);
            new_gemini += usize::from(!exists);
        }
    }

    if profiles.saturating_add(new_profiles) > limits.max_profiles
        || snapshots.saturating_add(batch.snapshots.len()) > limits.max_usage_snapshots
        || tokens.saturating_add(new_tokens) > limits.max_token_rows
        || contexts.saturating_add(new_contexts) > limits.max_context_sessions
        || gemini.saturating_add(new_gemini) > limits.max_gemini_models
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "ingest would exceed an account row cap",
        ));
    }
    Ok(())
}

fn validate_replay(replay: &IngestReplay) -> PulseResult<()> {
    if replay.request_id.is_empty()
        || replay.request_id.len() > 128
        || !replay
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(PulseError::invalid_input(
            "ingest request id must be a stable ASCII identifier",
        ));
    }
    validate_token_hash(&replay.payload_fingerprint)
}

fn result_count(value: usize) -> PulseResult<i64> {
    i64::try_from(value).map_err(|_| PulseError::invalid_input("ingest result count is too large"))
}

fn count_row_value(value: i64) -> PulseResult<usize> {
    usize::try_from(value).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "stored ingest replay count is invalid",
        )
    })
}

async fn table_count(transaction: &Transaction<'_>, table: &str) -> PulseResult<usize> {
    let sql = match table {
        "usage_windows" => "SELECT COUNT(*) FROM atmux_pulse.usage_windows",
        _ => {
            return Err(PulseError::new(
                PulseErrorKind::Internal,
                "unsupported PostgreSQL table count",
            ));
        }
    };
    count_row(&transaction.query_one(sql, &[]).await.map_err(sql_error)?)
}

async fn account_table_count(
    transaction: &Transaction<'_>,
    table: &str,
    account_id: AccountId,
) -> PulseResult<usize> {
    let sql = match table {
        "profiles" => "SELECT COUNT(*) FROM atmux_pulse.profiles WHERE account_id = $1",
        "usage_snapshots" => {
            "SELECT COUNT(*) FROM atmux_pulse.usage_snapshots WHERE account_id = $1"
        }
        "token_usage" => "SELECT COUNT(*) FROM atmux_pulse.token_usage WHERE account_id = $1",
        "context_sessions" => {
            "SELECT COUNT(*) FROM atmux_pulse.context_sessions WHERE account_id = $1"
        }
        "gemini_quota" => "SELECT COUNT(*) FROM atmux_pulse.gemini_quota WHERE account_id = $1",
        "ingest_replays" => "SELECT COUNT(*) FROM atmux_pulse.ingest_replays WHERE account_id = $1",
        "pending_reset_resume_jobs" => {
            "SELECT COUNT(*) FROM atmux_pulse.reset_resume_jobs WHERE account_id = $1 \
             AND delivered_at IS NULL AND cancelled_at IS NULL"
        }
        _ => {
            return Err(PulseError::new(
                PulseErrorKind::Internal,
                "unsupported PostgreSQL account table count",
            ));
        }
    };
    count_row(
        &transaction
            .query_one(sql, &[&account_id.get()])
            .await
            .map_err(sql_error)?,
    )
}

fn count_row(row: &Row) -> PulseResult<usize> {
    usize::try_from(row.get::<_, i64>(0)).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "PostgreSQL contains an invalid Pulse row count",
        )
    })
}
