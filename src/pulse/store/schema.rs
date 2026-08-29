//! Forward-only `SQLite` schema migrations.

/// Latest schema version understood by this build.
pub const LATEST_SCHEMA_VERSION: u32 = 7;

pub(crate) struct Migration {
    pub(crate) version: u32,
    pub(crate) sql: &'static str,
}

pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r"
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    identity TEXT NOT NULL UNIQUE CHECK (length(identity) BETWEEN 1 AND 320),
    display_name TEXT
) STRICT;

CREATE TABLE machines (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, name),
    CHECK (last_seen_ms >= first_seen_ms)
) STRICT;

CREATE TABLE profiles (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    vendor_json TEXT NOT NULL,
    config_dir TEXT,
    poll_interval_minutes INTEGER NOT NULL CHECK (poll_interval_minutes >= 5),
    monthly_budget_usd REAL,
    api_key_env TEXT,
    api_key_file TEXT,
    refresh_json TEXT NOT NULL,
    hidden INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1)),
    PRIMARY KEY (account_id, name),
    CHECK (api_key_env IS NULL OR api_key_file IS NULL),
    CHECK (monthly_budget_usd IS NULL OR monthly_budget_usd > 0)
) STRICT;

CREATE TABLE usage_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    vendor_json TEXT NOT NULL,
    outcome_json TEXT NOT NULL,
    polled_at_ms INTEGER NOT NULL,
    reporter_version TEXT,
    FOREIGN KEY (account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY (account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE
) STRICT;

CREATE TABLE usage_windows (
    snapshot_id INTEGER NOT NULL REFERENCES usage_snapshots(id) ON DELETE CASCADE,
    kind_json TEXT NOT NULL,
    used_percent REAL NOT NULL CHECK (used_percent BETWEEN 0 AND 100),
    resets_at_ms INTEGER NOT NULL,
    accepted INTEGER NOT NULL CHECK (accepted IN (0, 1)),
    PRIMARY KEY (snapshot_id, kind_json)
) STRICT;

CREATE TABLE context_sessions (
    account_id INTEGER NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT,
    settings_json TEXT NOT NULL,
    context_tokens INTEGER,
    context_percent REAL,
    effective_limit INTEGER,
    last_active_at_ms INTEGER NOT NULL,
    last_reset_at_ms INTEGER,
    collected_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, profile, machine, session_id),
    FOREIGN KEY (account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY (account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE,
    CHECK (context_tokens IS NULL OR context_tokens >= 0),
    CHECK (context_percent IS NULL OR context_percent BETWEEN 0 AND 100),
    CHECK (effective_limit IS NULL OR effective_limit > 0)
) STRICT;

CREATE TABLE token_usage (
    account_id INTEGER NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    settings_hash TEXT NOT NULL,
    settings_json TEXT NOT NULL,
    day TEXT NOT NULL,
    tokens_in INTEGER NOT NULL CHECK (tokens_in >= 0),
    tokens_out INTEGER NOT NULL CHECK (tokens_out >= 0),
    cache_write_5m INTEGER NOT NULL CHECK (cache_write_5m >= 0),
    cache_write_1h INTEGER NOT NULL CHECK (cache_write_1h >= 0),
    cache_read INTEGER NOT NULL CHECK (cache_read >= 0),
    source_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (
        account_id, profile, machine, session_id, model, settings_hash, day, source_json
    ),
    FOREIGN KEY (account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY (account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE
) STRICT;
",
    },
    Migration {
        version: 2,
        sql: r"
CREATE TABLE pricing_defaults (
    key TEXT PRIMARY KEY,
    vendor_json TEXT NOT NULL,
    model_pattern TEXT NOT NULL,
    settings_json TEXT NOT NULL,
    input_rate REAL NOT NULL CHECK (input_rate >= 0),
    output_rate REAL NOT NULL CHECK (output_rate >= 0),
    cache_write_5m_rate REAL NOT NULL CHECK (cache_write_5m_rate >= 0),
    cache_write_1h_rate REAL NOT NULL CHECK (cache_write_1h_rate >= 0),
    cache_read_rate REAL NOT NULL CHECK (cache_read_rate >= 0)
) STRICT;

CREATE TABLE pricing_overrides (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    vendor_json TEXT NOT NULL,
    model_pattern TEXT NOT NULL,
    settings_json TEXT NOT NULL,
    input_rate REAL NOT NULL CHECK (input_rate >= 0),
    output_rate REAL NOT NULL CHECK (output_rate >= 0),
    cache_write_5m_rate REAL NOT NULL CHECK (cache_write_5m_rate >= 0),
    cache_write_1h_rate REAL NOT NULL CHECK (cache_write_1h_rate >= 0),
    cache_read_rate REAL NOT NULL CHECK (cache_read_rate >= 0),
    PRIMARY KEY (account_id, key)
) STRICT;

CREATE TABLE alert_subscriptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    profile TEXT NOT NULL,
    alert_type_json TEXT NOT NULL,
    threshold REAL,
    threshold_key TEXT NOT NULL,
    cooldown_minutes INTEGER NOT NULL CHECK (cooldown_minutes > 0),
    delivery_json TEXT,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    FOREIGN KEY (account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE,
    UNIQUE (account_id, profile, alert_type_json, threshold_key)
) STRICT;

CREATE TABLE alert_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    subscription_id INTEGER NOT NULL REFERENCES alert_subscriptions(id) ON DELETE CASCADE,
    profile TEXT NOT NULL,
    alert_type_json TEXT NOT NULL,
    message TEXT NOT NULL CHECK (length(message) BETWEEN 1 AND 4096),
    current_value REAL,
    threshold REAL,
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0, 1)),
    triggered_at_ms INTEGER NOT NULL,
    FOREIGN KEY (account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE
) STRICT;

CREATE TABLE ingest_tokens (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    machine TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash) = 64),
    created_at_ms INTEGER NOT NULL,
    last_used_at_ms INTEGER,
    revoked_at_ms INTEGER,
    FOREIGN KEY (account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE
) STRICT;

CREATE TABLE gemini_quota (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    remaining_fraction REAL NOT NULL CHECK (remaining_fraction BETWEEN 0 AND 1),
    remaining_amount TEXT,
    resets_at_ms INTEGER,
    collected_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, model_id)
) STRICT;

CREATE TABLE import_provenance (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    source_fingerprint TEXT NOT NULL,
    source_table TEXT NOT NULL,
    source_row_id TEXT NOT NULL,
    target_key TEXT NOT NULL,
    imported_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, source_fingerprint, source_table, source_row_id)
) STRICT;

CREATE INDEX usage_snapshots_profile_time
    ON usage_snapshots(account_id, profile, polled_at_ms DESC, id DESC);
CREATE INDEX usage_snapshots_machine_time
    ON usage_snapshots(account_id, profile, machine, polled_at_ms DESC, id DESC);
CREATE INDEX usage_windows_winner
    ON usage_windows(kind_json, accepted, resets_at_ms DESC, snapshot_id DESC);
CREATE INDEX context_sessions_freshness
    ON context_sessions(account_id, last_active_at_ms DESC);
CREATE INDEX token_usage_report
    ON token_usage(account_id, day, profile, machine, model);
CREATE INDEX alert_events_cooldown
    ON alert_events(account_id, subscription_id, triggered_at_ms DESC);
CREATE INDEX alert_events_ack
    ON alert_events(account_id, acknowledged, triggered_at_ms DESC);
",
    },
    Migration {
        version: 3,
        sql: r#"
ALTER TABLE profiles ADD COLUMN origin_json TEXT NOT NULL DEFAULT '"local"';

CREATE UNIQUE INDEX alert_events_account_identity ON alert_events(account_id, id);

CREATE TABLE alert_replies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    event_id INTEGER NOT NULL,
    message TEXT NOT NULL CHECK (length(CAST(message AS BLOB)) BETWEEN 1 AND 2048),
    replied_at_ms INTEGER NOT NULL,
    FOREIGN KEY (account_id, event_id) REFERENCES alert_events(account_id, id) ON DELETE CASCADE
) STRICT;

CREATE TABLE reset_resume_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    profile TEXT NOT NULL,
    resets_at_ms INTEGER NOT NULL,
    resume_at_ms INTEGER NOT NULL,
    scheduled_at_ms INTEGER NOT NULL,
    lease_until_ms INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    delivered_at_ms INTEGER,
    cancelled_at_ms INTEGER,
    FOREIGN KEY (account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE,
    UNIQUE (account_id, profile, resets_at_ms),
    CHECK (resume_at_ms = resets_at_ms + 60000),
    CHECK (resets_at_ms > scheduled_at_ms),
    CHECK (delivered_at_ms IS NULL OR cancelled_at_ms IS NULL)
) STRICT;

CREATE TABLE ingest_replays (
    account_id INTEGER NOT NULL,
    machine TEXT NOT NULL,
    request_id TEXT NOT NULL CHECK (length(request_id) BETWEEN 1 AND 128),
    payload_fingerprint TEXT NOT NULL CHECK (length(payload_fingerprint) = 64),
    snapshots INTEGER NOT NULL CHECK (snapshots >= 0),
    token_grains INTEGER NOT NULL CHECK (token_grains >= 0),
    context_sessions INTEGER NOT NULL CHECK (context_sessions >= 0),
    gemini_quotas INTEGER NOT NULL CHECK (gemini_quotas >= 0),
    received_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, machine, request_id),
    FOREIGN KEY (account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE
) STRICT;

CREATE INDEX alert_replies_event ON alert_replies(account_id, event_id, replied_at_ms, id);
CREATE INDEX reset_resume_pending
    ON reset_resume_jobs(account_id, resume_at_ms, id)
    WHERE delivered_at_ms IS NULL AND cancelled_at_ms IS NULL;
CREATE INDEX ingest_replays_account ON ingest_replays(account_id);
"#,
    },
    Migration {
        version: 4,
        sql: r"
CREATE TABLE federation_peers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    source_machine TEXT NOT NULL,
    cursor TEXT,
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    pages_applied INTEGER NOT NULL DEFAULT 0 CHECK (pages_applied >= 0),
    records_applied INTEGER NOT NULL DEFAULT 0 CHECK (records_applied >= 0),
    complete INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0, 1)),
    UNIQUE (account_id, source_machine)
) STRICT;

CREATE TABLE federation_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    source_machine TEXT NOT NULL,
    record_key TEXT NOT NULL CHECK (length(record_key) BETWEEN 1 AND 192),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64),
    received_at_ms INTEGER NOT NULL,
    UNIQUE (account_id, source_machine, record_key),
    FOREIGN KEY (account_id, source_machine)
        REFERENCES federation_peers(account_id, source_machine) ON DELETE CASCADE
) STRICT;

CREATE INDEX federation_records_peer
    ON federation_records(account_id, source_machine, id);
CREATE INDEX federation_profiles_export ON profiles(account_id, origin_json, name);
CREATE INDEX federation_usage_export ON usage_snapshots(account_id, machine, id);
CREATE INDEX federation_context_export
    ON context_sessions(account_id, machine, profile, session_id);
CREATE INDEX federation_token_export
    ON token_usage(
        account_id, machine, profile, session_id, model, settings_hash, day, source_json
    );
",
    },
    Migration {
        version: 5,
        sql: r"
ALTER TABLE import_provenance ADD COLUMN payload_fingerprint TEXT NOT NULL
    DEFAULT '0000000000000000000000000000000000000000000000000000000000000000';

-- A v4 database could record the same logical target through copied legacy
-- sources. Retain the lexicographically first audit witness deterministically;
-- the zero payload fingerprint makes every non-identical future replay fail
-- closed instead of silently accepting unverifiable legacy provenance.
DELETE FROM import_provenance AS candidate
WHERE EXISTS (
    SELECT 1 FROM import_provenance AS keeper
    WHERE keeper.account_id = candidate.account_id
      AND keeper.source_table = candidate.source_table
      AND keeper.target_key = candidate.target_key
      AND (
          keeper.source_fingerprint < candidate.source_fingerprint
          OR (
              keeper.source_fingerprint = candidate.source_fingerprint
              AND keeper.source_row_id < candidate.source_row_id
          )
      )
);

CREATE UNIQUE INDEX import_provenance_logical_target
    ON import_provenance(account_id, source_table, target_key);
",
    },
    Migration {
        version: 6,
        sql: r"
CREATE TABLE reporter_cursors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    machine TEXT NOT NULL,
    destination_key TEXT NOT NULL CHECK(length(destination_key) BETWEEN 1 AND 96),
    usage_after_id INTEGER NOT NULL DEFAULT 0 CHECK(usage_after_id >= 0),
    token_cursor_json TEXT,
    token_generation INTEGER NOT NULL DEFAULT 0 CHECK(token_generation >= 0),
    UNIQUE(account_id, machine, destination_key),
    FOREIGN KEY(account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE
) STRICT;

CREATE TABLE reporter_pending_pages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    machine TEXT NOT NULL,
    destination_key TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('usage', 'token')),
    expected_cursor_json TEXT NOT NULL,
    next_cursor_json TEXT NOT NULL,
    chunk_count INTEGER NOT NULL CHECK(chunk_count BETWEEN 1 AND 64),
    total_bytes INTEGER NOT NULL CHECK(total_bytes BETWEEN 1 AND 8388608),
    UNIQUE(account_id, machine, destination_key, kind),
    UNIQUE(id, account_id),
    FOREIGN KEY(account_id, machine, destination_key)
        REFERENCES reporter_cursors(account_id, machine, destination_key) ON DELETE CASCADE
) STRICT;

CREATE TABLE reporter_pending_chunks (
    pending_id INTEGER NOT NULL,
    account_id INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL CHECK(chunk_index >= 0),
    request_id TEXT NOT NULL CHECK(length(request_id) BETWEEN 1 AND 128),
    body BLOB NOT NULL CHECK(length(body) BETWEEN 1 AND 1048576),
    rows INTEGER NOT NULL CHECK(rows > 0),
    PRIMARY KEY(pending_id, chunk_index),
    FOREIGN KEY(pending_id, account_id)
        REFERENCES reporter_pending_pages(id, account_id) ON DELETE CASCADE
) STRICT;

CREATE INDEX reporter_pending_chunks_account
    ON reporter_pending_chunks(account_id, pending_id, chunk_index);
",
    },
    Migration {
        version: 7,
        sql: r"
ALTER TABLE token_usage ADD COLUMN write_revision INTEGER NOT NULL DEFAULT 0
    CHECK(write_revision >= 0);

CREATE TABLE token_write_revisions (
    account_id INTEGER NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0),
    PRIMARY KEY(account_id, profile, machine),
    FOREIGN KEY(account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY(account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE
) STRICT;

CREATE TABLE backfill_progress (
    account_id INTEGER NOT NULL,
    profile TEXT NOT NULL,
    machine TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    source_generation TEXT NOT NULL CHECK(length(source_generation) = 64),
    write_revision INTEGER NOT NULL CHECK(write_revision >= 0),
    cursor_json TEXT,
    complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN (0,1)),
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY(account_id, profile, machine),
    FOREIGN KEY(account_id, profile) REFERENCES profiles(account_id, name) ON DELETE CASCADE,
    FOREIGN KEY(account_id, machine) REFERENCES machines(account_id, name) ON DELETE CASCADE
) STRICT;
",
    },
];
