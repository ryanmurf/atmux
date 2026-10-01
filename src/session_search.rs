//! The H2 external producer contract, kept separate from transport. The
//! coordinator serializes publications under the digest store lock, advances a
//! durable per-session timestamp only after enqueue, and rejects older snapshots.

use crate::summarizer::{DigestRecord, bounded_text};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const SEARCH_TOPIC: &str = "entity-change";
pub const HQ_TENANT: &str = "95efe33d-fa71-53ce-8e0a-3fe45ac0e58a";
const SYSTEM_USER: &str = "00000000-0000-0000-0000-000000000000";
pub const MAX_DOCUMENT_BYTES: usize = 8_000;
pub const MAX_ENVELOPE_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchChange {
    Created,
    Updated,
    Archived,
    SoftDeleted,
    HardDeleted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangeType {
    Created,
    Updated,
    SoftDeleted,
    HardDeleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSnapshot {
    pub id: String,
    pub session_key: String,
    #[serde(rename = "tenantId")]
    pub tenant_id: String,
    #[serde(rename = "ownerType")]
    pub owner_type: String,
    #[serde(rename = "ownerId")]
    pub owner_id: String,
    pub scope: String,
    #[serde(rename = "createdBy")]
    pub created_by: String,
    #[serde(rename = "updatedBy")]
    pub updated_by: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "archivedAt")]
    pub archived_at: Option<String>,
    pub title: String,
    pub description: String,
    pub digest: String,
    pub machine: String,
    pub project_remote: String,
    pub project_branch: String,
    pub cwd: String,
    pub harness: String,
    pub profile: String,
    pub state: String,
}
impl SearchSnapshot {
    /// Exactly H2's content-extractor template, substituted and trimmed.
    #[must_use]
    pub fn rendered_document(&self) -> String {
        format!("Session: {}\n{}\n\n{}\n\nMachine: {}\nProject: {}\nBranch: {}\nCwd: {}\nHarness: {}; profile: {}; state: {}\nCreated: {}; updated: {}; archived: {}",
            self.title, self.description, self.digest, self.machine, self.project_remote, self.project_branch,
            self.cwd, self.harness, self.profile, self.state, self.created_at, self.updated_at,
            self.archived_at.as_deref().unwrap_or_default()).trim().to_owned()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityContext {
    pub tenant_id: String,
    pub token: Option<String>,
    pub platform: String,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityChange {
    #[serde(rename = "type")]
    pub kind: String,
    pub entity_type: String,
    pub entity_id: String,
    pub change_type: ChangeType,
    pub event_ts: String,
    pub previous: Option<SearchSnapshot>,
    pub current: Option<SearchSnapshot>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchEnvelope {
    pub envelope_version: u8,
    pub event_type: String,
    pub published_at: String,
    pub security_context: SecurityContext,
    pub mdc: serde_json::Map<String, serde_json::Value>,
    pub payload_ref: Option<String>,
    pub payload_summary: Option<String>,
    pub event_payload: EntityChange,
}
#[derive(Clone, Debug)]
pub struct SearchPublication {
    pub topic: &'static str,
    pub key: String,
    pub envelope: SearchEnvelope,
}
impl SearchPublication {
    /// Produces the inline Kafka value, including its complete envelope.
    ///
    /// # Errors
    /// Returns an error if serialization or the 65,536-byte envelope gate fails.
    pub fn value(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(&self.envelope)?;
        if bytes.len() > MAX_ENVELOPE_BYTES {
            bail!("search envelope exceeds 65,536 bytes");
        }
        Ok(bytes)
    }
}

pub(crate) fn valid_tenant(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}
/// Pure builder for H2's full-snapshot producer contract. Digest shortening is
/// subordinate to the complete 8,000-byte rendered-document limit.
///
/// # Errors
/// Returns an error for invalid identity/tenant/time or an oversized envelope.
pub fn search_publication(
    record: &DigestRecord,
    tenant: &str,
    change: SearchChange,
    updated_at_ms: u64,
) -> Result<SearchPublication> {
    if !crate::tmux::valid_session_key(&record.session_key) || !valid_tenant(tenant) {
        bail!("search publication requires a stable session key and canonical tenant UUID");
    }
    let created_at_ms = record
        .created_at
        .checked_mul(1000)
        .ok_or_else(|| anyhow::anyhow!("invalid created time"))?;
    if updated_at_ms < created_at_ms {
        bail!("search snapshot predates session creation");
    }
    let timestamp = rfc3339(updated_at_ms)?;
    let deletion = matches!(
        change,
        SearchChange::SoftDeleted | SearchChange::HardDeleted
    );
    let current = if deletion {
        None
    } else {
        Some(search_snapshot(
            record,
            tenant,
            change,
            &timestamp,
            created_at_ms,
        )?)
    };

    let change_type = match change {
        SearchChange::Created => ChangeType::Created,
        SearchChange::Updated | SearchChange::Archived => ChangeType::Updated,
        SearchChange::SoftDeleted => ChangeType::SoftDeleted,
        SearchChange::HardDeleted => ChangeType::HardDeleted,
    };
    let publication = SearchPublication {
        topic: SEARCH_TOPIC,
        key: record.session_key.clone(),
        envelope: SearchEnvelope {
            envelope_version: 2,
            event_type: "ENTITY_CHANGE".into(),
            published_at: timestamp.clone(),
            security_context: SecurityContext {
                tenant_id: tenant.into(),
                token: None,
                platform: "SYSTEM".into(),
                user_id: None,
                session_id: None,
            },
            mdc: serde_json::Map::new(),
            payload_ref: None,
            payload_summary: None,
            event_payload: EntityChange {
                kind: "ENTITY_CHANGE".into(),
                entity_type: "ATMUX_SESSION".into(),
                entity_id: record.session_key.clone(),
                change_type,
                event_ts: timestamp,
                previous: None,
                current,
            },
        },
    };
    publication.value()?;
    Ok(publication)
}

fn search_snapshot(
    record: &DigestRecord,
    tenant: &str,
    change: SearchChange,
    timestamp: &str,
    created_at_ms: u64,
) -> Result<SearchSnapshot> {
    let remote = record.project_remote.as_deref().unwrap_or_default();
    let remote = if remote.contains(['?', '#'])
        || remote.split_once("://").is_some_and(|(_, rest)| {
            rest.split('/')
                .next()
                .is_some_and(|host| host.contains('@'))
        }) {
        ""
    } else {
        remote
    };
    let mut snapshot = SearchSnapshot {
        id: record.session_key.clone(),
        session_key: record.session_key.clone(),
        tenant_id: tenant.into(),
        owner_type: "Tenant".into(),
        owner_id: tenant.into(),
        scope: "tenant".into(),
        created_by: SYSTEM_USER.into(),
        updated_by: SYSTEM_USER.into(),
        created_at: rfc3339(created_at_ms)?,
        updated_at: timestamp.to_owned(),
        archived_at: (change == SearchChange::Archived || record.state == "archived")
            .then(|| timestamp.to_owned()),
        title: bounded_text(&record.title, 400),
        description: bounded_text(record.search_description(), 480),
        digest: bounded_text(&record.digest, MAX_DOCUMENT_BYTES),
        machine: bounded_text(&record.machine, 128),
        project_remote: bounded_text(remote, 512),
        project_branch: bounded_text(record.project_branch.as_deref().unwrap_or_default(), 256),
        cwd: bounded_text(&record.cwd, 512),
        harness: bounded_text(&record.harness, 32),
        profile: bounded_text(&record.profile, 128),
        state: if change == SearchChange::Archived {
            "archived".into()
        } else {
            bounded_text(&record.state, 32)
        },
    };
    let fixed_bytes = {
        let mut fixed = snapshot.clone();
        fixed.digest.clear();
        fixed.rendered_document().len()
    };
    if fixed_bytes > MAX_DOCUMENT_BYTES {
        bail!("search metadata exceeds rendered-document limit");
    }
    snapshot.digest = bounded_text(&snapshot.digest, MAX_DOCUMENT_BYTES - fixed_bytes);
    // Measure the actual rendered string after truncation/trim as H2 does.
    while snapshot.rendered_document().len() > MAX_DOCUMENT_BYTES {
        snapshot.digest = bounded_text(&snapshot.digest, snapshot.digest.len().saturating_sub(1));
    }
    Ok(snapshot)
}

/// Millisecond timestamps preserve ordering for lifecycle changes in the same
/// second. Replays and stale snapshots never reach the publisher.
#[must_use]
pub const fn newer_snapshot(updated_at_ms: u64, high_water_ms: u64) -> bool {
    updated_at_ms > high_water_ms
}

fn rfc3339(milliseconds: u64) -> Result<String> {
    if milliseconds > 253_402_300_799_999 {
        bail!("search timestamp exceeds year 9999");
    }
    let seconds = milliseconds / 1000;
    let days = i64::try_from(seconds / 86_400)? + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = seconds % 86_400 / 3600;
    let minute = seconds % 3600 / 60;
    let second = seconds % 60;
    let fraction = milliseconds % 1000;
    let suffix = if fraction == 0 {
        "Z".into()
    } else {
        format!(".{fraction:03}Z")
    };
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{suffix}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const CREATED: &str = include_str!("../tests/fixtures/atmux-search/session-created.json");
    const UPDATED: &str = include_str!("../tests/fixtures/atmux-search/session-updated.json");
    const ARCHIVED: &str = include_str!("../tests/fixtures/atmux-search/session-archived.json");
    const CREATED_MS: u64 = 1_790_798_400_000;
    fn record(snapshot: &SearchSnapshot) -> DigestRecord {
        serde_json::from_value(serde_json::json!({"session_key":snapshot.session_key,"machine":snapshot.machine,"pane":"tron~%1",
            "name":"index-atmux-sessions","description":snapshot.description,"title":snapshot.title,"digest":snapshot.digest,
            "digest_updated_at":CREATED_MS/1000,"digest_version":1,"transcript_hash":"fixture","pane_hash":"fixture","last_entry":null,
            "last_attempt":0,"last_seen":CREATED_MS/1000,"created_at":CREATED_MS/1000,"checked_at":0,
            "project_remote":snapshot.project_remote,"project_branch":snapshot.project_branch,"cwd":snapshot.cwd,
            "harness":snapshot.harness,"profile":snapshot.profile,"state":snapshot.state})).unwrap()
    }
    #[test]
    fn envelopes_exactly_match_h2_create_update_archive_fixtures() {
        for (fixture, change, offset) in [
            (CREATED, SearchChange::Created, 0),
            (UPDATED, SearchChange::Updated, 300_000),
            (ARCHIVED, SearchChange::Archived, 600_000),
        ] {
            let expected: SearchEnvelope = serde_json::from_str(fixture).unwrap();
            let digest = record(expected.event_payload.current.as_ref().unwrap());
            let publication =
                search_publication(&digest, HQ_TENANT, change, CREATED_MS + offset).unwrap();
            assert_eq!(publication.topic, "entity-change");
            assert_eq!(publication.key, digest.session_key);
            assert_eq!(publication.envelope, expected);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&publication.value().unwrap()).unwrap(),
                serde_json::from_str::<serde_json::Value>(fixture).unwrap()
            );
        }
    }
    #[test]
    fn rendered_template_and_utf8_document_envelope_limits_match_h2() {
        let envelope: SearchEnvelope = serde_json::from_str(CREATED).unwrap();
        let mut digest = record(envelope.event_payload.current.as_ref().unwrap());
        digest.digest = "é🎉\"\\".repeat(16_000);
        let publication =
            search_publication(&digest, HQ_TENANT, SearchChange::Updated, CREATED_MS + 1).unwrap();
        let snapshot = publication.envelope.event_payload.current.as_ref().unwrap();
        assert!(snapshot.rendered_document().len() <= MAX_DOCUMENT_BYTES);
        assert!(publication.value().unwrap().len() <= MAX_ENVELOPE_BYTES);
        assert!(
            snapshot
                .rendered_document()
                .starts_with("Session: Index atmux sessions\nMake agent session digests")
        );
        assert!(
            snapshot
                .rendered_document()
                .contains("Harness: codex; profile: hd; state: working")
        );
        digest.digest = "x".repeat(16_000);
        let publication =
            search_publication(&digest, HQ_TENANT, SearchChange::Updated, CREATED_MS + 1000)
                .unwrap();
        assert_eq!(
            publication
                .envelope
                .event_payload
                .current
                .unwrap()
                .rendered_document()
                .len(),
            8000
        );
    }
    #[test]
    fn removals_are_null_snapshots_resume_clears_archive_and_identity_fails_closed() {
        let envelope: SearchEnvelope = serde_json::from_str(ARCHIVED).unwrap();
        let mut digest = record(envelope.event_payload.current.as_ref().unwrap());
        for (change, kind) in [
            (SearchChange::SoftDeleted, ChangeType::SoftDeleted),
            (SearchChange::HardDeleted, ChangeType::HardDeleted),
        ] {
            let publication =
                search_publication(&digest, HQ_TENANT, change, CREATED_MS + 1).unwrap();
            assert_eq!(publication.envelope.event_payload.change_type, kind);
            assert!(publication.envelope.event_payload.current.is_none());
        }
        digest.state = "working".into();
        let resumed =
            search_publication(&digest, HQ_TENANT, SearchChange::Updated, CREATED_MS + 1000)
                .unwrap();
        assert!(
            resumed
                .envelope
                .event_payload
                .current
                .unwrap()
                .archived_at
                .is_none()
        );
        assert!(
            search_publication(&digest, "bad-tenant", SearchChange::Created, CREATED_MS).is_err()
        );
        digest.session_key = "bad-key".into();
        assert!(search_publication(&digest, HQ_TENANT, SearchChange::Created, CREATED_MS).is_err());
    }
    #[test]
    fn timestamp_order_suppresses_stale_replays_and_preserves_same_second_updates() {
        assert_eq!(rfc3339(CREATED_MS).unwrap(), "2026-09-30T20:00:00Z");
        assert_eq!(rfc3339(CREATED_MS + 1).unwrap(), "2026-09-30T20:00:00.001Z");
        assert_eq!(rfc3339(0).unwrap(), "1970-01-01T00:00:00Z");
        assert!(newer_snapshot(101, 100));
        assert!(!newer_snapshot(100, 100));
        assert!(!newer_snapshot(99, 100));
    }
}
