#![cfg(feature = "pulse")]

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use atmux::pulse::{
    AccountId, CollectionOutcome, Instant, PulseError, Vendor,
    collect::{
        SecretRef,
        antigravity::{
            EMITS_RATE_LIMIT_SNAPSHOT, UNKNOWN_MODEL, collect_conversations,
            extract_latest_timestamp, extract_usage_from_payloads, resolve_conversation_model,
            tally_conversation_db,
        },
        deepseek::parse_balance_response,
        gemini::{GeminiCollector, GeminiConfig, SCHEDULE_INTERVAL, parse_quota_response},
        grok::{collect_transcript_usage, parse_billing_response, parse_transcript_line},
    },
};
use rusqlite::{Connection, params};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pulse")
        .join(name)
}

fn instant(value: &str) -> Instant {
    Instant::from_iso8601(value).expect("valid fixture instant")
}

fn decode_hex_fixture(name: &str) -> Vec<Vec<u8>> {
    fs::read_to_string(fixture(name))
        .expect("read hex fixture")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(decode_hex)
        .collect()
}

fn decode_hex(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0);
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("fixture is ASCII");
            u8::from_str_radix(text, 16).expect("fixture is hex")
        })
        .collect()
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "atmux-pulse-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1.0e-9, "{actual} != {expected}");
}

#[test]
fn deepseek_fixture_normalizes_monthly_budget_in_utc() {
    let body = fs::read(fixture("deepseek_balance.json")).unwrap();
    let reading = parse_balance_response(&body, 100.0, instant("2026-08-08T23:59:59.999Z"))
        .expect("valid balance");
    assert!(reading.available);
    assert_close(reading.balance_usd, 61.5);
    assert_close(reading.window.used_percent.get(), 38.5);
    assert_eq!(
        reading.window.resets_at.to_iso8601(),
        "2026-09-01T00:00:00Z"
    );

    let december = parse_balance_response(&body, 100.0, instant("2026-12-31T23:59:59Z"))
        .expect("valid balance");
    assert_eq!(
        december.window.resets_at.to_iso8601(),
        "2027-01-01T00:00:00Z"
    );
    assert!(parse_balance_response(&body, 0.0, instant("2026-08-08T00:00:00Z")).is_err());
}

#[test]
fn grok_billing_and_transcript_fixtures_are_bounded_and_ranked() {
    let billing = fs::read(fixture("grok_billing.json")).unwrap();
    let window = parse_billing_response(&billing).expect("valid billing fixture");
    assert_close(window.used_percent.get(), 42.25);
    assert_eq!(window.resets_at.to_iso8601(), "2026-08-10T00:00:00Z");

    let transcript = fs::read_to_string(fixture("grok_updates.jsonl")).unwrap();
    let reading = transcript
        .lines()
        .find_map(|line| parse_transcript_line(line, 1))
        .expect("usage line");
    assert_eq!(reading.actual, 1_500_000);
    assert_eq!(reading.limit, 2_000_000);
    assert_eq!(reading.window_hours, 24);
    assert_eq!(reading.measured_at_ms, 1_786_213_800_123);

    let directory = TempDirectory::new("grok");
    let sessions = directory.0.join("sessions/run-1");
    fs::create_dir_all(&sessions).unwrap();
    fs::copy(
        fixture("grok_updates.jsonl"),
        sessions.join("updates.jsonl"),
    )
    .unwrap();
    let observed = collect_transcript_usage(&directory.0, instant("2026-08-08T18:40:00Z"))
        .expect("bounded scan")
        .expect("fresh signal");
    assert_close(observed.used_percent.get(), 75.0);
    assert_eq!(observed.resets_at.to_iso8601(), "2026-08-09T18:30:00Z");
}

#[test]
fn grok_errors_do_not_contain_throttle_trigger_phrases() {
    let error = parse_billing_response(br#"{"config":{}}"#).unwrap_err();
    assert_no_throttle_trigger(&error);

    let directory = TempDirectory::new("grok-empty");
    fs::create_dir_all(directory.0.join("sessions")).unwrap();
    assert!(
        collect_transcript_usage(&directory.0, instant("2026-08-08T18:40:00Z"))
            .unwrap()
            .is_none()
    );
}

fn assert_no_throttle_trigger(error: &PulseError) {
    let message = error.message().to_ascii_lowercase();
    for forbidden in ["429", "rate limit", "too many requests", "usage limit"] {
        assert!(
            !message.contains(forbidden),
            "unexpected trigger in {message}"
        );
    }
}

#[test]
fn gemini_fixture_preserves_per_model_quota_and_rejects_hostile_buckets() {
    let body = fs::read(fixture("gemini_quota.json")).unwrap();
    let quotas = parse_quota_response(
        AccountId::new(7).unwrap(),
        &body,
        instant("2026-08-08T18:40:00Z"),
    )
    .expect("valid quota fixture");
    assert_eq!(quotas.len(), 2);
    assert_eq!(quotas[0].model_id, "gemini-2.5-pro");
    assert_close(quotas[0].remaining_fraction.get(), 0.5);
    assert_close(quotas[1].remaining_fraction.get(), 0.875);
    assert_eq!(SCHEDULE_INTERVAL.as_secs(), 30 * 60);

    let too_many = serde_json::json!({
        "buckets": (0..129).map(|index| serde_json::json!({
            "modelId": format!("gemini-fixture-{index}"),
            "remainingFraction": 0.5
        })).collect::<Vec<_>>()
    });
    assert!(
        parse_quota_response(
            AccountId::new(7).unwrap(),
            &serde_json::to_vec(&too_many).unwrap(),
            instant("2026-08-08T18:40:00Z")
        )
        .is_err()
    );
}

#[tokio::test]
async fn gemini_missing_credentials_is_gracefully_disabled() {
    let directory = TempDirectory::new("gemini-missing");
    let collector = GeminiCollector::new(
        GeminiConfig::new(
            directory.0.join("missing-oauth.json"),
            "fixture-client-id".to_owned(),
            "fixture-client-secret".to_owned(),
        )
        .expect("injected config"),
    )
    .expect("collector config");
    let result = collector
        .collect(AccountId::new(7).unwrap(), instant("2026-08-08T18:40:00Z"))
        .await;
    assert_eq!(
        result.outcome,
        CollectionOutcome::Disabled {
            code: "gemini_credentials_missing".to_owned()
        }
    );
    assert!(result.quotas.is_empty());
}

#[test]
fn antigravity_fixture_requires_checksum_dedupes_and_has_exact_totals() {
    let payloads = decode_hex_fixture("antigravity_payloads.hex");
    let usage = extract_usage_from_payloads(&payloads)
        .expect("valid payloads")
        .expect("usage");
    assert_eq!(usage.prompt, 15_528);
    assert_eq!(usage.output, 347);
    assert_eq!(usage.thinking, 386);
    assert_eq!(
        extract_latest_timestamp(&payloads).unwrap(),
        Some(1_780_842_253)
    );

    let metadata = decode_hex_fixture("antigravity_metadata.hex");
    assert_eq!(
        resolve_conversation_model(&metadata).unwrap(),
        "gemini-3.5-flash-low"
    );
    assert_eq!(resolve_conversation_model(&[]).unwrap(), UNKNOWN_MODEL);
    assert!(!std::hint::black_box(EMITS_RATE_LIMIT_SNAPSHOT));
    assert!(!Vendor::Antigravity.emits_usage_snapshots());

    let missing_checksum = length_field(
        7,
        &[
            varint_field(2, 1_000),
            varint_field(9, 50),
            varint_field(10, 100),
        ]
        .concat(),
    );
    assert_eq!(
        extract_usage_from_payloads(&[missing_checksum]).unwrap(),
        None
    );
    let corrupt_checksum = length_field(
        7,
        &[
            varint_field(2, 1_000),
            varint_field(3, 999),
            varint_field(9, 50),
            varint_field(10, 100),
        ]
        .concat(),
    );
    assert_eq!(
        extract_usage_from_payloads(&[corrupt_checksum]).unwrap(),
        None
    );
    assert!(extract_usage_from_payloads(&[vec![0x80]]).is_err());
    assert!(extract_usage_from_payloads(&[vec![0; 1024 * 1024 + 1]]).is_err());
}

#[test]
fn antigravity_sqlite_adapter_is_read_only_and_exact() {
    let directory = TempDirectory::new("antigravity-db");
    let conversations = directory.0.join("conversations");
    fs::create_dir_all(&conversations).unwrap();
    let database = conversations.join("11111111-1111-1111-1111-111111111111.db");
    let payloads = decode_hex_fixture("antigravity_payloads.hex");
    let metadata = decode_hex_fixture("antigravity_metadata.hex");
    {
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE steps(step_payload BLOB);\
                 CREATE TABLE gen_metadata(data BLOB);\
                 CREATE TABLE executor_metadata(data BLOB);",
            )
            .unwrap();
        for payload in &payloads {
            connection
                .execute(
                    "INSERT INTO steps(step_payload) VALUES (?1)",
                    params![payload],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO gen_metadata(data) VALUES (?1)",
                params![&metadata[0]],
            )
            .unwrap();
    }
    let conversation = tally_conversation_db(&database, 0)
        .expect("read-only tally")
        .expect("usage");
    assert_eq!(conversation.usage.prompt, 15_528);
    assert_eq!(conversation.usage.output, 347);
    assert_eq!(conversation.usage.thinking, 386);
    assert_eq!(conversation.model, "gemini-3.5-flash-low");
    assert_eq!(conversation.day, "2026-06-07");
    assert_eq!(
        conversation.session_id,
        "11111111-1111-1111-1111-111111111111"
    );
    let all = collect_conversations(&directory.0, None).expect("bounded db scan");
    assert_eq!(all, vec![conversation]);
}

#[cfg(unix)]
#[test]
fn collectors_refuse_symlinked_inputs_and_oversized_files() {
    use std::os::unix::fs::symlink;

    let directory = TempDirectory::new("hostile-files");
    let real_secret = directory.0.join("real-secret");
    fs::write(&real_secret, "not-a-real-secret").unwrap();
    let linked_secret = directory.0.join("linked-secret");
    symlink(&real_secret, &linked_secret).unwrap();
    assert!(
        SecretRef::File {
            path: linked_secret
        }
        .resolve()
        .is_err()
    );

    // Simulate a final-component swap after an earlier discovery/stat pass.
    let swapped_secret = directory.0.join("swapped-secret");
    fs::write(&swapped_secret, "original-regular-value").unwrap();
    let displaced_secret = directory.0.join("displaced-secret");
    fs::rename(&swapped_secret, &displaced_secret).unwrap();
    symlink(&real_secret, &swapped_secret).unwrap();
    assert!(
        SecretRef::File {
            path: swapped_secret
        }
        .resolve()
        .is_err()
    );

    let real_db = directory.0.join("real.db");
    Connection::open(&real_db).unwrap();
    let linked_db = directory.0.join("linked.db");
    symlink(&real_db, &linked_db).unwrap();
    assert!(tally_conversation_db(&linked_db, 0).is_err());

    let sessions = directory.0.join("grok/sessions/run");
    fs::create_dir_all(&sessions).unwrap();
    symlink(
        fixture("grok_updates.jsonl"),
        sessions.join("updates.jsonl"),
    )
    .unwrap();
    assert!(
        collect_transcript_usage(&directory.0.join("grok"), instant("2026-08-08T18:40:00Z"))
            .unwrap()
            .is_none()
    );

    let oversized = sessions.join("oversized/updates.jsonl");
    fs::create_dir_all(oversized.parent().unwrap()).unwrap();
    let file = fs::File::create(&oversized).unwrap();
    file.set_len(1024 * 1024 + 1).unwrap();
    assert!(
        collect_transcript_usage(&directory.0.join("grok"), instant("2026-08-08T18:40:00Z"))
            .is_err()
    );
}

fn varint(mut value: u64) -> Vec<u8> {
    let mut encoded = Vec::new();
    loop {
        let mut byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        encoded.push(byte);
        if value == 0 {
            return encoded;
        }
    }
}

fn varint_field(field: u64, value: u64) -> Vec<u8> {
    [varint(field * 8), varint(value)].concat()
}

fn length_field(field: u64, value: &[u8]) -> Vec<u8> {
    [
        varint(field * 8 + 2),
        varint(u64::try_from(value.len()).unwrap()),
        value.to_vec(),
    ]
    .concat()
}
