#![cfg(feature = "pulse")]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use atmux::pulse::{
    Account, AccountId, Instant, MachineName, ProfileName, RefreshPolicy,
    import::{
        ExternalCredential, ImportDiagnosticCode, ImportLimits, ImportRequest, import_legacy_sqlite,
    },
    store::{SqliteStore, Store},
};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct TestFiles {
    base: PathBuf,
    source: PathBuf,
    target: PathBuf,
}

impl TestFiles {
    fn new(label: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "atmux-pulse-import-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&base).expect("private import test root");
        let source = base.join("source.sqlite3");
        let target = base.join("target.sqlite3");
        Self {
            base,
            source,
            target,
        }
    }
}

impl Drop for TestFiles {
    fn drop(&mut self) {
        remove_sqlite(&self.source);
        remove_sqlite(&self.target);
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn remove_sqlite(path: &Path) {
    let _ = fs::remove_file(path);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        let _ = fs::remove_file(PathBuf::from(sidecar));
    }
}

fn file_sha256(path: &Path) -> Vec<u8> {
    Sha256::digest(fs::read(path).expect("read fixture")).to_vec()
}

#[allow(clippy::too_many_lines)]
fn create_frozen_fixture(path: &Path) {
    let connection = Connection::open(path).expect("create legacy fixture");
    connection
        .execute_batch(
            r"
CREATE TABLE accounts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  identity TEXT NOT NULL UNIQUE,
  display_name TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE profiles (
  name TEXT PRIMARY KEY,
  config_dir TEXT NOT NULL,
  poll_interval_minutes INTEGER NOT NULL DEFAULT 5,
  vendor TEXT NOT NULL DEFAULT 'anthropic-oauth',
  monthly_budget_usd REAL,
  api_key TEXT,
  account_id INTEGER,
  hidden INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE usage_snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  profile TEXT NOT NULL,
  five_hour_pct REAL,
  five_hour_resets_at TEXT,
  seven_day_pct REAL,
  seven_day_resets_at TEXT,
  raw_response TEXT,
  polled_at TEXT NOT NULL DEFAULT (datetime('now')),
  context_tokens INTEGER,
  context_pct REAL,
  context_session_id TEXT,
  context_model TEXT,
  context_effective_limit INTEGER,
  context_last_reset_at TEXT,
  machine TEXT,
  account_id INTEGER,
  reporter_version TEXT
);
CREATE TABLE gemini_quota (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  timestamp TEXT NOT NULL DEFAULT (datetime('now')),
  model_id TEXT NOT NULL,
  remaining_fraction REAL NOT NULL,
  remaining_amount TEXT,
  reset_time TEXT,
  account_id INTEGER
);
CREATE TABLE machines (
  account_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  first_seen TEXT NOT NULL DEFAULT (datetime('now')),
  last_seen TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(account_id, name)
);
CREATE TABLE token_usage (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id INTEGER NOT NULL,
  profile TEXT NOT NULL,
  machine TEXT NOT NULL,
  session_id TEXT NOT NULL,
  model TEXT NOT NULL,
  settings_hash TEXT NOT NULL,
  settings_json TEXT NOT NULL DEFAULT '{}',
  day TEXT NOT NULL,
  tokens_in INTEGER NOT NULL DEFAULT 0,
  tokens_out INTEGER NOT NULL DEFAULT 0,
  cache_write_5m INTEGER NOT NULL DEFAULT 0,
  cache_write_1h INTEGER NOT NULL DEFAULT 0,
  cache_read INTEGER NOT NULL DEFAULT 0,
  source TEXT NOT NULL DEFAULT 'local',
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(account_id, profile, machine, session_id, model, settings_hash, day)
);
CREATE TABLE context_sessions (
  account_id INTEGER NOT NULL,
  profile TEXT NOT NULL,
  machine TEXT NOT NULL,
  session_id TEXT NOT NULL,
  model TEXT,
  settings_json TEXT NOT NULL DEFAULT '{}',
  context_tokens INTEGER,
  context_pct REAL,
  effective_limit INTEGER,
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  last_active_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(account_id, profile, machine, session_id)
);
CREATE TABLE pricing_defaults (
  model TEXT NOT NULL,
  settings_match_json TEXT NOT NULL,
  input REAL NOT NULL,
  output REAL NOT NULL,
  cache_write_5m REAL NOT NULL,
  cache_write_1h REAL NOT NULL,
  cache_read REAL NOT NULL,
  source_url TEXT,
  as_of TEXT
);
CREATE TABLE pricing_overrides (
  account_id INTEGER NOT NULL,
  model TEXT NOT NULL,
  settings_match_json TEXT NOT NULL,
  input REAL NOT NULL,
  output REAL NOT NULL,
  cache_write_5m REAL NOT NULL,
  cache_write_1h REAL NOT NULL,
  cache_read REAL NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE alert_subscriptions (
  id INTEGER PRIMARY KEY,
  profile TEXT NOT NULL,
  alert_type TEXT NOT NULL,
  threshold REAL,
  channel TEXT,
  cooldown_minutes INTEGER NOT NULL,
  enabled INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  account_id INTEGER
);
CREATE TABLE alert_events (
  id INTEGER PRIMARY KEY,
  subscription_id INTEGER NOT NULL,
  profile TEXT NOT NULL,
  alert_type TEXT NOT NULL,
  message TEXT NOT NULL,
  current_value REAL,
  threshold REAL,
  acknowledged INTEGER NOT NULL,
  triggered_at TEXT NOT NULL,
  account_id INTEGER
);
CREATE TABLE ingest_tokens (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id INTEGER NOT NULL,
  machine TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  last_used_at TEXT,
  revoked_at TEXT
);
CREATE TABLE token_rollups (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  profile TEXT NOT NULL,
  host TEXT NOT NULL,
  day TEXT NOT NULL,
  model TEXT NOT NULL,
  input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  cost_usd REAL NOT NULL DEFAULT 0,
  source TEXT NOT NULL DEFAULT 'local',
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
",
        )
        .expect("legacy schema");
    connection
        .execute(
            "INSERT INTO accounts(id,identity,display_name,created_at) VALUES(1,'local','Local','2026-08-01 00:00:00')",
            [],
        )
        .expect("account one");
    connection
        .execute(
            "INSERT INTO accounts(id,identity,display_name,created_at) VALUES(2,'other','Other','2026-08-01 00:00:00')",
            [],
        )
        .expect("account two");
    connection
        .execute(
            "INSERT INTO profiles(name,config_dir,poll_interval_minutes,vendor,monthly_budget_usd,api_key,account_id,hidden) VALUES(?1,?2,15,?3,?4,?5,1,?6)",
            params!["claude-max", "/var/lib/claude-max", "anthropic-oauth", Option::<f64>::None, Option::<String>::None, 0],
        )
        .expect("Claude profile");
    connection
        .execute(
            "INSERT INTO profiles(name,config_dir,poll_interval_minutes,vendor,monthly_budget_usd,api_key,account_id,hidden) VALUES(?1,?2,15,?3,?4,?5,1,?6)",
            params!["deepseek", "/var/lib/deepseek", "deepseek-balance", 100.0, "legacy-super-secret", 1],
        )
        .expect("DeepSeek profile");
    connection
        .execute(
            "INSERT INTO profiles(name,config_dir,poll_interval_minutes,vendor,account_id,hidden) VALUES('other-profile','/other',15,'anthropic-oauth',2,0)",
            [],
        )
        .expect("other account profile");
    connection
        .execute(
            "INSERT INTO machines(account_id,name,first_seen,last_seen) VALUES(1,'machine-a','2026-08-01 01:00:00','2026-08-08 01:00:00')",
            [],
        )
        .expect("machine");
    connection
        .execute(
            "INSERT INTO usage_snapshots(id,profile,five_hour_pct,five_hour_resets_at,seven_day_pct,seven_day_resets_at,raw_response,polled_at,machine,account_id,reporter_version) VALUES(10,'claude-max',25.0,'2026-08-08T05:00:00Z',50.0,'2026-08-14T00:00:00Z','raw-bearer-secret','2026-08-08 01:30:00',NULL,1,'legacy-1.2')",
            [],
        )
        .expect("snapshot");
    connection
        .execute(
            "INSERT INTO token_usage(id,account_id,profile,machine,session_id,model,settings_hash,settings_json,day,tokens_in,tokens_out,cache_write_5m,cache_write_1h,cache_read,source,updated_at) VALUES(20,1,'claude-max','machine-a','session-a','claude-opus-4-8','legacy-json-key','{\"service_tier\":\"standard\"}','2026-08-08',100,20,30,40,50,'local','2026-08-08 01:40:00')",
            [],
        )
        .expect("token usage");
    connection
        .execute(
            "INSERT INTO context_sessions(account_id,profile,machine,session_id,model,settings_json,context_tokens,context_pct,effective_limit,updated_at,last_active_at) VALUES(1,'claude-max','machine-a','session-a','claude-opus-4-8','{\"service_tier\":\"standard\"}',100000,50.0,200000,'2026-08-08 01:45:00','2026-08-08 01:44:00')",
            [],
        )
        .expect("context");
    connection
        .execute(
            "INSERT INTO gemini_quota(id,timestamp,model_id,remaining_fraction,remaining_amount,reset_time,account_id) VALUES(30,'2026-08-08 01:50:00','gemini-2.5-pro',0.75,'750','2026-08-09T00:00:00Z',1)",
            [],
        )
        .expect("Gemini quota");
    connection
        .execute(
            "INSERT INTO pricing_defaults(model,settings_match_json,input,output,cache_write_5m,cache_write_1h,cache_read) VALUES('gpt-5','{}',1,2,3,4,5)",
            [],
        )
        .expect("legacy pricing default");
    connection
        .execute(
            "INSERT INTO pricing_overrides(account_id,model,settings_match_json,input,output,cache_write_5m,cache_write_1h,cache_read,updated_at) VALUES(1,'gpt-5','{}',10,20,30,40,50,'2026-08-08 01:55:00')",
            [],
        )
        .expect("pricing override");
    connection
        .execute(
            "INSERT INTO alert_subscriptions(id,profile,alert_type,threshold,channel,cooldown_minutes,enabled,created_at,account_id) VALUES(60,'claude-max','five_hour_threshold',80,'legacy-channel',15,1,'2026-08-08 01:56:00',1)",
            [],
        )
        .expect("threshold alert subscription");
    connection
        .execute(
            "INSERT INTO alert_subscriptions(id,profile,alert_type,threshold,channel,cooldown_minutes,enabled,created_at,account_id) VALUES(61,'deepseek','auth_failure',NULL,NULL,30,1,'2026-08-08 01:57:00',1)",
            [],
        )
        .expect("auth alert subscription");
    connection
        .execute(
            "INSERT INTO alert_events(id,subscription_id,profile,alert_type,message,current_value,threshold,acknowledged,triggered_at,account_id) VALUES(70,60,'claude-max','five_hour_threshold','Legacy threshold reached',81,80,1,'2026-08-08 01:58:00',1)",
            [],
        )
        .expect("threshold alert event");
    connection
        .execute(
            "INSERT INTO alert_events(id,subscription_id,profile,alert_type,message,current_value,threshold,acknowledged,triggered_at,account_id) VALUES(71,61,'deepseek','auth_failure','Legacy authentication failure',NULL,NULL,0,'2026-08-08 01:59:00',1)",
            [],
        )
        .expect("auth alert event");
    connection
        .execute(
            "INSERT INTO ingest_tokens(id,account_id,machine,token_hash) VALUES(40,1,'machine-a','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')",
            [],
        )
        .expect("ingest hash");
    connection
        .execute(
            "INSERT INTO token_rollups(id,profile,host,day,model,input_tokens) VALUES(50,'claude-max','machine-a','2026-08-08','claude-opus-4-8',999)",
            [],
        )
        .expect("coarse rollup");
    assert_eq!(
        connection
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .expect("source quick check"),
        "ok"
    );
}

async fn target_store(path: &Path) -> (SqliteStore, AccountId) {
    let store = SqliteStore::open(path).await.expect("target store");
    let account_id = AccountId::new(7).expect("account id");
    store
        .upsert_account(Account {
            id: account_id,
            identity: "target@example.test".to_owned(),
            display_name: Some("Target".to_owned()),
        })
        .await
        .expect("target account");
    (store, account_id)
}

fn request(source: &Path, target: AccountId, dry_run: bool) -> ImportRequest {
    let mut credentials = BTreeMap::new();
    credentials.insert(
        ProfileName::new("deepseek").expect("profile"),
        ExternalCredential::Environment("DEEPSEEK_API_KEY".to_owned()),
    );
    ImportRequest {
        source: source.to_path_buf(),
        target_account_id: target,
        source_account_id: Some(1),
        fallback_machine: Some(MachineName::new("legacy-host").expect("machine")),
        machine_aliases: BTreeMap::new(),
        credentials,
        refresh: RefreshPolicy::InMemory,
        imported_at: Instant::from_iso8601("2026-08-08T02:00:00Z").expect("instant"),
        dry_run,
        limits: ImportLimits::default(),
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dry_run_and_import_are_read_only_bounded_idempotent_and_exact() {
    let files = TestFiles::new("complete");
    create_frozen_fixture(&files.source);
    let source_before = file_sha256(&files.source);
    let metadata_before = fs::metadata(&files.source).expect("source metadata");
    let (store, account_id) = target_store(&files.target).await;

    let dry_run = import_legacy_sqlite(&store, request(&files.source, account_id, true))
        .await
        .expect("dry run");
    assert!(dry_run.dry_run);
    assert!(dry_run.reconciliation_complete);
    assert!(dry_run.reconciliation_exact);
    assert_eq!(dry_run.tables["token_usage"].planned, 1);
    assert_eq!(
        dry_run.tables["pricing_overrides"].planned, 1,
        "diagnostics: {:#?}",
        dry_run.diagnostics
    );
    assert_eq!(dry_run.tables["alert_subscriptions"].planned, 2);
    assert_eq!(dry_run.tables["alert_events"].planned, 2);
    assert_eq!(dry_run.tables["token_usage"].imported, 0);
    assert!(
        dry_run.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ImportDiagnosticCode::InlineSecretExternalized
        })
    );
    assert!(
        dry_run
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ImportDiagnosticCode::IngestTokensExcluded })
    );
    assert!(
        dry_run.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ImportDiagnosticCode::LossyLegacyRollupExcluded
        })
    );
    assert!(dry_run.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == ImportDiagnosticCode::AuthoritativePricingDefaultsSeeded
    }));
    assert!(
        dry_run
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == ImportDiagnosticCode::AlertDeliveryExcluded })
    );
    let encoded = serde_json::to_string(&dry_run).expect("report JSON");
    assert!(!encoded.contains("legacy-super-secret"));
    assert!(!encoded.contains("raw-bearer-secret"));
    assert!(!encoded.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    assert!(
        store
            .list_profiles(account_id)
            .await
            .expect("profiles")
            .is_empty()
    );

    let imported = import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect("import");
    assert!(!imported.dry_run);
    assert!(imported.reconciliation_complete);
    assert!(imported.reconciliation_exact);
    assert_eq!(imported.tables["profiles"].imported, 2);
    assert_eq!(imported.tables["usage_snapshots"].imported, 1);
    assert_eq!(imported.tables["token_usage"].imported, 1);
    assert_eq!(imported.tables["pricing_overrides"].imported, 1);
    assert_eq!(imported.tables["alert_subscriptions"].imported, 2);
    assert_eq!(imported.tables["alert_events"].imported, 2);

    let profiles = store.list_profiles(account_id).await.expect("profiles");
    assert_eq!(profiles.len(), 2);
    let deepseek = profiles
        .iter()
        .find(|profile| profile.name.as_str() == "deepseek")
        .expect("DeepSeek profile");
    assert_eq!(deepseek.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
    assert!(deepseek.api_key_file.is_none());
    let history = store
        .usage_history(
            account_id,
            ProfileName::new("claude-max").expect("profile"),
            None,
            100,
        )
        .await
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].snapshot.machine.as_str(), "legacy-host");
    let tokens = store
        .list_token_grains(account_id, None, None, 100)
        .await
        .expect("tokens");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].tokens_in, 100);
    assert_eq!(tokens[0].cache_write_1h, 40);
    assert_eq!(
        store
            .list_context_sessions(account_id, None)
            .await
            .expect("contexts")
            .len(),
        1
    );
    let overrides = store
        .list_pricing_overrides(account_id)
        .await
        .expect("pricing overrides");
    assert_eq!(overrides.len(), 1);
    assert!((overrides[0].input_per_million_usd - 10.0).abs() < f64::EPSILON);
    let subscriptions = store
        .list_alert_subscriptions(account_id)
        .await
        .expect("alert subscriptions");
    assert_eq!(subscriptions.len(), 2);
    assert!(
        subscriptions
            .iter()
            .all(|subscription| ![60, 61].contains(&subscription.id))
    );
    assert!(
        subscriptions
            .iter()
            .all(|subscription| subscription.subscription.delivery.is_none())
    );
    let events = store
        .list_alert_events(account_id, None)
        .await
        .expect("alert events");
    assert_eq!(events.len(), 2);
    assert_eq!(events.iter().filter(|event| event.acknowledged).count(), 1);
    assert_eq!(
        store
            .list_gemini_quotas(account_id)
            .await
            .expect("Gemini")
            .len(),
        1
    );
    assert!(
        store
            .list_ingest_tokens(account_id)
            .await
            .expect("ingest tokens")
            .is_empty()
    );

    let replay = import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect("idempotent replay");
    assert_eq!(replay.tables["usage_snapshots"].replayed, 1);
    assert_eq!(replay.tables["token_usage"].replayed, 1);
    assert_eq!(replay.tables["pricing_overrides"].replayed, 1);
    assert_eq!(replay.tables["alert_subscriptions"].replayed, 2);
    assert_eq!(replay.tables["alert_events"].replayed, 2);
    assert!(replay.reconciliation_exact);
    assert_eq!(
        store
            .usage_history(
                account_id,
                ProfileName::new("claude-max").expect("profile"),
                None,
                100,
            )
            .await
            .expect("history")
            .len(),
        1
    );

    assert_eq!(file_sha256(&files.source), source_before);
    let metadata_after = fs::metadata(&files.source).expect("source metadata");
    assert_eq!(metadata_after.len(), metadata_before.len());
    assert_eq!(
        metadata_after.modified().ok(),
        metadata_before.modified().ok()
    );
    let source =
        Connection::open_with_flags(&files.source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("reopen source read-only");
    assert_eq!(
        source
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .expect("source quick check"),
        "ok"
    );
}

#[tokio::test]
async fn large_gemini_history_is_bounded_to_latest_row_per_model_in_sql() {
    let files = TestFiles::new("large-gemini-history");
    create_frozen_fixture(&files.source);
    let connection = Connection::open(&files.source).expect("open source fixture");
    connection
        .execute(
            "WITH RECURSIVE history(row_id) AS (\
               SELECT 1 UNION ALL SELECT row_id + 1 FROM history WHERE row_id < 50000\
             ) \
             INSERT INTO gemini_quota(\
               id,timestamp,model_id,remaining_fraction,remaining_amount,reset_time,account_id\
             ) \
             SELECT 1000 + row_id, '2026-08-09 01:50:00', \
                    CASE WHEN row_id = 50000 THEN 'gemini-2.5-flash' ELSE 'gemini-2.5-pro' END, \
                    0.5, CAST(row_id AS TEXT), NULL, 1 \
             FROM history",
            [],
        )
        .expect("insert large Gemini history");
    drop(connection);
    let (store, account_id) = target_store(&files.target).await;
    let mut import_request = request(&files.source, account_id, true);
    import_request.limits.max_rows_per_table = 10;
    import_request.limits.max_total_rows = 50;

    let report = import_legacy_sqlite(&store, import_request)
        .await
        .expect("Gemini history must be bounded by selected output rows");

    assert_eq!(report.tables["gemini_quota"].discovered, 50_001);
    assert_eq!(report.tables["gemini_quota"].planned, 2);
    assert_eq!(report.tables["gemini_quota"].skipped, 0);
}

#[tokio::test]
async fn more_than_ten_thousand_token_rows_reconcile_exactly_by_profile_day() {
    let files = TestFiles::new("large-token-reconciliation");
    create_frozen_fixture(&files.source);
    let connection = Connection::open(&files.source).expect("open source fixture");
    connection
        .execute(
            "WITH RECURSIVE grains(row_id) AS (\
               SELECT 1 UNION ALL SELECT row_id + 1 FROM grains WHERE row_id < 12050\
             ) \
             INSERT INTO token_usage(\
               id,account_id,profile,machine,session_id,model,settings_hash,settings_json,day,\
               tokens_in,tokens_out,cache_write_5m,cache_write_1h,cache_read,source,updated_at\
             ) \
             SELECT 1000 + row_id,1,'claude-max','machine-a',printf('bulk-%d',row_id),\
                    'claude-opus-4','legacy','{}',\
                    CASE WHEN row_id <= 6000 THEN '2026-08-06' ELSE '2026-08-07' END,\
                    1,2,3,4,5,'local','2026-08-08 02:00:00' FROM grains",
            [],
        )
        .expect("insert large token history");
    drop(connection);
    let (store, account_id) = target_store(&files.target).await;

    let report = import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect("large token import");

    assert_eq!(report.tables["token_usage"].imported, 12_051);
    assert!(report.reconciliation_complete);
    assert!(report.reconciliation_exact);
    assert_eq!(report.reconciliation.len(), 3);
}

#[tokio::test]
async fn logical_copy_replays_but_mutated_logical_row_conflicts_without_writes() {
    let files = TestFiles::new("logical-identity");
    create_frozen_fixture(&files.source);
    let copied = files.source.with_extension("copy.sqlite3");
    remove_sqlite(&copied);
    fs::copy(&files.source, &copied).expect("copy legacy database");
    let (store, account_id) = target_store(&files.target).await;

    import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect("initial import");
    let copied_report = import_legacy_sqlite(&store, request(&copied, account_id, false))
        .await
        .expect("copied database replay");
    assert_eq!(copied_report.tables["usage_snapshots"].replayed, 1);
    assert_eq!(copied_report.tables["token_usage"].replayed, 1);
    assert_eq!(
        store
            .usage_history(
                account_id,
                ProfileName::new("claude-max").expect("profile"),
                None,
                100,
            )
            .await
            .expect("history")
            .len(),
        1
    );

    let source = Connection::open(&files.source).expect("mutate source fixture");
    source
        .execute("UPDATE token_usage SET tokens_in=999 WHERE id=20", [])
        .expect("mutate stable logical row");
    drop(source);
    let error = import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect_err("mutated logical row must conflict");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::Conflict);
    let tokens = store
        .list_token_grains(account_id, None, None, 100)
        .await
        .expect("tokens after rejected mutation");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].tokens_in, 100);

    remove_sqlite(&copied);
}

#[tokio::test]
async fn absent_legacy_columns_produce_typed_diagnostics_without_inventing_rows() {
    let files = TestFiles::new("missing-columns");
    let source = Connection::open(&files.source).expect("source");
    source
        .execute_batch(
            "CREATE TABLE profiles(name TEXT PRIMARY KEY, config_dir TEXT NOT NULL);\
             INSERT INTO profiles VALUES('old-profile','/old');",
        )
        .expect("old schema");
    drop(source);
    let (store, account_id) = target_store(&files.target).await;
    let report = import_legacy_sqlite(&store, request(&files.source, account_id, true))
        .await
        .expect("diagnostic dry run");
    assert_eq!(report.tables["profiles"].planned, 0);
    assert_eq!(report.tables["profiles"].skipped, 1);
    assert!(report.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == ImportDiagnosticCode::MissingColumn
            && diagnostic.table.as_deref() == Some("profiles")
            && diagnostic.column.as_deref() == Some("vendor")
    }));
    assert!(
        store
            .list_profiles(account_id)
            .await
            .expect("profiles")
            .is_empty()
    );
}

#[tokio::test]
async fn legacy_profiles_without_hidden_default_visible_and_import_dependents() {
    let files = TestFiles::new("profiles-without-hidden");
    create_frozen_fixture(&files.source);
    let source = Connection::open(&files.source).expect("source");
    source
        .execute_batch("ALTER TABLE profiles DROP COLUMN hidden")
        .expect("remove newer visibility column");
    drop(source);
    let (store, account_id) = target_store(&files.target).await;

    let report = import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect("old-schema import");

    assert_eq!(report.tables["profiles"].imported, 2);
    assert_eq!(report.tables["usage_snapshots"].imported, 1);
    assert!(report.reconciliation_complete);
    assert!(report.reconciliation_exact);
    assert!(report.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == ImportDiagnosticCode::LegacyProfileVisibilityDefaulted
            && diagnostic.table.as_deref() == Some("profiles")
            && diagnostic.column.as_deref() == Some("hidden")
    }));
    assert!(
        store
            .list_profiles(account_id)
            .await
            .expect("profiles")
            .iter()
            .all(|profile| !profile.hidden)
    );
    assert_eq!(
        store
            .usage_history(
                account_id,
                ProfileName::new("claude-max").expect("profile"),
                None,
                10,
            )
            .await
            .expect("snapshot")
            .len(),
        1
    );
}

#[tokio::test]
async fn machine_aliases_cover_case_variants_existing_names_and_null_fallbacks() {
    let files = TestFiles::new("machine-aliases");
    create_frozen_fixture(&files.source);
    let source = Connection::open(&files.source).expect("source");
    source
        .execute_batch(
            "INSERT INTO machines(account_id,name,first_seen,last_seen) \
             VALUES(1,'Machine-A','2026-08-02 01:00:00','2026-08-07 01:00:00');\
             INSERT INTO machines(account_id,name,first_seen,last_seen) \
             VALUES(1,'midnight','2026-07-31 01:00:00','2026-08-09 01:00:00');\
             INSERT INTO token_usage(\
               id,account_id,profile,machine,session_id,model,settings_hash,settings_json,day,\
               tokens_in,tokens_out,cache_write_5m,cache_write_1h,cache_read,source,updated_at\
             ) VALUES(21,1,'claude-max','Machine-A','session-b','claude-opus-4-8',\
               'legacy-json-key','{\"service_tier\":\"standard\"}','2026-08-08',\
               10,2,3,4,5,'local','2026-08-08 01:41:00')",
        )
        .expect("alias fixtures");
    drop(source);
    let (store, account_id) = target_store(&files.target).await;
    let mut import_request = request(&files.source, account_id, false);
    for source in ["machine-a", "Machine-A", "legacy-host"] {
        import_request.machine_aliases.insert(
            MachineName::new(source).expect("legacy machine"),
            MachineName::new("midnight").expect("canonical machine"),
        );
    }

    let report = import_legacy_sqlite(&store, import_request)
        .await
        .expect("aliased import");

    let alias_diagnostics = report
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == ImportDiagnosticCode::MachineAliasApplied)
        .collect::<Vec<_>>();
    assert_eq!(alias_diagnostics.len(), 3);
    assert!(alias_diagnostics.iter().all(|diagnostic| {
        diagnostic.message.contains("selected rows") && !diagnostic.message.contains('/')
    }));
    assert_eq!(report.tables["machines"].planned, 1);
    let machines = store.list_machines(account_id).await.expect("machines");
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].name.as_str(), "midnight");
    assert_eq!(machines[0].first_seen.to_iso8601(), "2026-07-31T01:00:00Z");
    assert_eq!(machines[0].last_seen.to_iso8601(), "2026-08-09T01:00:00Z");
    assert!(
        store
            .list_token_grains(account_id, None, None, 10)
            .await
            .expect("tokens")
            .iter()
            .all(|grain| grain.machine.as_str() == "midnight")
    );
    assert!(
        store
            .list_context_sessions(account_id, None)
            .await
            .expect("contexts")
            .iter()
            .all(|context| context.machine.as_str() == "midnight")
    );
    assert!(
        store
            .usage_history(
                account_id,
                ProfileName::new("claude-max").expect("profile"),
                None,
                10,
            )
            .await
            .expect("snapshots")
            .iter()
            .all(|snapshot| snapshot.snapshot.machine.as_str() == "midnight")
    );
}

#[tokio::test]
async fn machine_alias_outside_selected_account_is_rejected() {
    let files = TestFiles::new("machine-alias-scope");
    create_frozen_fixture(&files.source);
    let source = Connection::open(&files.source).expect("source");
    source
        .execute(
            "INSERT INTO machines(account_id,name,first_seen,last_seen) \
             VALUES(2,'other-only','2026-08-01 01:00:00','2026-08-08 01:00:00')",
            [],
        )
        .expect("out-of-scope machine");
    drop(source);
    let (store, account_id) = target_store(&files.target).await;
    let mut import_request = request(&files.source, account_id, true);
    import_request.machine_aliases.insert(
        MachineName::new("other-only").expect("legacy machine"),
        MachineName::new("midnight").expect("canonical machine"),
    );

    let error = import_legacy_sqlite(&store, import_request)
        .await
        .expect_err("out-of-scope alias must fail");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::InvalidInput);
    assert!(error.message().contains("selected legacy account"));
}

#[tokio::test]
async fn machine_alias_dependent_collision_requires_identical_canonical_payloads() {
    let files = TestFiles::new("machine-alias-conflict");
    create_frozen_fixture(&files.source);
    let source = Connection::open(&files.source).expect("source");
    source
        .execute(
            "INSERT INTO token_usage(\
               id,account_id,profile,machine,session_id,model,settings_hash,settings_json,day,\
               tokens_in,tokens_out,cache_write_5m,cache_write_1h,cache_read,source,updated_at\
             ) VALUES(21,1,'claude-max','Machine-A','session-a','claude-opus-4-8',\
               'legacy-json-key','{\"service_tier\":\"standard\"}','2026-08-08',\
               999,20,30,40,50,'local','2026-08-08 01:41:00')",
            [],
        )
        .expect("conflicting dependent row");
    drop(source);
    let (store, account_id) = target_store(&files.target).await;
    let mut import_request = request(&files.source, account_id, true);
    for source in ["machine-a", "Machine-A"] {
        import_request.machine_aliases.insert(
            MachineName::new(source).expect("legacy machine"),
            MachineName::new("midnight").expect("canonical machine"),
        );
    }

    let error = import_legacy_sqlite(&store, import_request)
        .await
        .expect_err("divergent alias collision must fail");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::Conflict);
    assert!(
        store
            .list_profiles(account_id)
            .await
            .expect("target unchanged")
            .is_empty()
    );
}

#[tokio::test]
async fn failed_snapshot_write_rolls_back_provenance_and_replays_cleanly() {
    let files = TestFiles::new("snapshot-atomicity");
    create_frozen_fixture(&files.source);
    let (store, account_id) = target_store(&files.target).await;
    let target_admin = Connection::open(&files.target).expect("target admin connection");
    target_admin
        .execute_batch(
            "CREATE TRIGGER reject_imported_snapshot BEFORE INSERT ON usage_snapshots \
             BEGIN SELECT RAISE(ABORT, 'fixture rejection'); END;",
        )
        .expect("failure trigger");

    import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect_err("snapshot insert must fail");
    target_admin
        .execute_batch("DROP TRIGGER reject_imported_snapshot")
        .expect("remove failure trigger");

    let retry = import_legacy_sqlite(&store, request(&files.source, account_id, false))
        .await
        .expect("retry after rolled-back atomic write");
    assert_eq!(retry.tables["usage_snapshots"].imported, 1);
    assert_eq!(retry.tables["usage_snapshots"].replayed, 0);
    assert_eq!(
        store
            .usage_history(
                account_id,
                ProfileName::new("claude-max").expect("profile"),
                None,
                100,
            )
            .await
            .expect("history")
            .len(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn source_symlinks_are_rejected_before_sqlite_is_opened() {
    use std::os::unix::fs::symlink;

    let files = TestFiles::new("symlink");
    create_frozen_fixture(&files.source);
    let link = files.source.with_extension("link.sqlite3");
    let _ = fs::remove_file(&link);
    symlink(&files.source, &link).expect("source symlink");
    let (store, account_id) = target_store(&files.target).await;
    let error = import_legacy_sqlite(&store, request(&link, account_id, true))
        .await
        .expect_err("symlink must fail");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::InvalidInput);
    assert!(error.message().contains("symlink"));
    let _ = fs::remove_file(link);
}

#[tokio::test]
async fn source_row_and_file_bounds_fail_closed() {
    let files = TestFiles::new("bounds");
    let source = Connection::open(&files.source).expect("source");
    source
        .execute_batch(
            "CREATE TABLE profiles(name TEXT PRIMARY KEY,config_dir TEXT,poll_interval_minutes INTEGER,vendor TEXT,monthly_budget_usd REAL,api_key TEXT,hidden INTEGER);\
             INSERT INTO profiles VALUES('one','/one',15,'anthropic-oauth',NULL,NULL,0);\
             INSERT INTO profiles VALUES('two','/two',15,'anthropic-oauth',NULL,NULL,0);",
        )
        .expect("bounded fixture");
    drop(source);
    let (store, account_id) = target_store(&files.target).await;
    let mut bounded = request(&files.source, account_id, true);
    bounded.limits.max_rows_per_table = 1;
    let error = import_legacy_sqlite(&store, bounded)
        .await
        .expect_err("row bound");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::InvalidInput);

    let mut bounded = request(&files.source, account_id, true);
    bounded.limits.max_source_bytes = 1;
    let error = import_legacy_sqlite(&store, bounded)
        .await
        .expect_err("file bound");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::InvalidInput);
}
