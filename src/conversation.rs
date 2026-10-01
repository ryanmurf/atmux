//! Filtered, cursor-based views of the existing owner-redacted transcript.

use anyhow::{Result, bail};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    control::ControlPlane,
    transcript::{Transcript, TranscriptMessage},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Include {
    Human,
    Agent,
    Subagent,
    Tools,
    Compaction,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConversationRequest {
    /// Pane reference or stable `session_key` from `agents_list`.
    pub id: String,
    /// Default: human, agent, subagent, compaction. Tools are opt-in.
    pub include: Option<Vec<Include>>,
    /// Return entries strictly after this entry id, before applying filters.
    pub after: Option<String>,
    /// Clamped to 1..=240; defaults to 80.
    pub limit: Option<usize>,
    /// Serialized response ceiling, clamped to 512..=786432; defaults to 65536.
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ConversationPage {
    pub available: bool,
    pub source: String,
    pub entries: Vec<TranscriptMessage>,
    /// Last returned entry, including on the final page, for subsequent tailing.
    pub next: Option<String>,
    pub truncated: bool,
}

pub(crate) fn page(transcript: Transcript, request: &ConversationRequest) -> Result<ConversationPage> {
    let defaults = [
        Include::Human,
        Include::Agent,
        Include::Subagent,
        Include::Compaction,
    ];
    let include = request.include.as_deref().unwrap_or(&defaults);
    if include.len() > 5 {
        bail!("include accepts at most five categories");
    }
    let messages = transcript.messages.unwrap_or_default();
    let start = match request.after.as_deref() {
        Some(cursor) if cursor.len() > 512 => bail!("cursor exceeds 512 bytes"),
        Some(cursor) => messages
            .iter()
            .position(|entry| entry.id == cursor)
            .map(|index| index + 1)
            .ok_or_else(|| {
                crate::control::ControlError::new(
                    crate::control::ErrorKind::Conflict,
                    "cursor is outside the bounded transcript window; reload without after",
                )
            })?,
        None => 0,
    };
    let limit = request.limit.unwrap_or(80).clamp(1, 240);
    let max_bytes = request.max_bytes.unwrap_or(65_536).clamp(512, 768 * 1024);
    let mut result = ConversationPage {
        available: transcript.available,
        source: transcript.source,
        entries: Vec::new(),
        next: None,
        truncated: transcript.truncated,
    };
    for mut entry in messages
        .into_iter()
        .skip(start)
        .filter(|entry| matches_include(entry, include))
    {
        if result.entries.len() == limit {
            result.truncated = true;
            break;
        }
        if !include.contains(&Include::Tools) {
            entry.tool_name = None;
            entry.tool_input = None;
            entry.tool_output = None;
        }
        let previous = result.next.clone();
        result.next = Some(entry.id.clone());
        result.entries.push(entry);
        // Include the envelope and cursor in the byte ceiling, not just markdown.
        if serde_json::to_vec(&result)?.len() > max_bytes {
            result.entries.pop();
            result.next = previous;
            result.truncated = true;
            break;
        }
    }
    Ok(result)
}

fn matches_include(entry: &TranscriptMessage, include: &[Include]) -> bool {
    let category = match entry.kind.as_str() {
        "tool" => Include::Tools,
        "compaction" => Include::Compaction,
        "message" => match entry.role.as_str() {
            "user" => Include::Human,
            "assistant" => Include::Agent,
            "subagent" => Include::Subagent,
            _ => return false,
        },
        _ => return false,
    };
    include.contains(&category)
}

impl ControlPlane {
    /// Reads the owner-bounded, redacted conversation across federation.
    ///
    /// # Errors
    /// Returns an error for an unknown/ambiguous session, missing cursor, or offline owner.
    pub async fn agent_conversation(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationPage> {
        let session = self.conversation_session(&request.id)?;
        let transcript = self
            .transcript(&session.id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session disappeared"))?;
        page(transcript, &request)
    }

    pub(crate) fn conversation_session(&self, id: &str) -> Result<crate::control::SessionSummary> {
        if id.len() > 512 {
            bail!("session reference exceeds 512 bytes");
        }
        let overview = self.overview();
        let mut keys = overview
            .sessions
            .iter()
            .filter(|session| session.session_key.as_deref() == Some(id));
        if let Some(session) = keys.next() {
            if keys.next().is_some() {
                bail!("session key has multiple live owners; use a pane reference");
            }
            return Ok(session.clone());
        }
        // Preserve the established local-preference and ambiguity rules.
        let reference = self.conversation_reference(id)?;
        overview
            .sessions
            .into_iter()
            .find(|session| session.id == reference)
            .ok_or_else(|| anyhow::anyhow!("no agent session matches {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ConversationRequest {
        ConversationRequest {
            id: "%1".into(),
            include: None,
            after: None,
            limit: None,
            max_bytes: None,
        }
    }

    #[test]
    fn cursor_is_applied_before_filters_and_payload_is_bounded() {
        for source in ["claude", "codex"] {
            let messages = [
                ("user", "message"),
                ("assistant", "tool"),
                ("assistant", "message"),
                ("subagent", "message"),
                ("system", "compaction"),
                ("system", "message"),
            ]
            .iter()
            .enumerate()
            .map(|(i, (role, kind))| {
                serde_json::from_value(
                    serde_json::json!({"id": i.to_string(), "role": role, "kind": kind,
                    "markdown": "fixture", "tool_output": "redacted owner data"}),
                )
                .unwrap()
            })
            .collect();
            let transcript = Transcript {
                available: true,
                source: source.into(),
                content_hash: "hash".into(),
                changed: true,
                truncated: false,
                messages: Some(messages),
                note: None,
            };
            let mut req = request();
            req.limit = Some(2);
            let first = page(transcript.clone(), &req).unwrap();
            assert_eq!(
                first
                    .entries
                    .iter()
                    .map(|e| e.id.as_str())
                    .collect::<Vec<_>>(),
                ["0", "2"]
            );
            assert!(first.truncated);
            assert!(first.entries.iter().all(|e| e.tool_output.is_none()));
            req.after = Some("1".into());
            req.include = Some(vec![Include::Agent, Include::Compaction]);
            let second = page(transcript.clone(), &req).unwrap();
            assert_eq!(
                second
                    .entries
                    .iter()
                    .map(|e| e.id.as_str())
                    .collect::<Vec<_>>(),
                ["2", "4"]
            );
            assert!(!second.truncated);
            assert_eq!(second.next.as_deref(), Some("4"));
            req.include = Some(vec![Include::Tools]);
            req.after = None;
            assert!(
                page(transcript.clone(), &req).unwrap().entries[0]
                    .tool_output
                    .is_some()
            );
            req.after = Some("missing".into());
            assert!(page(transcript.clone(), &req).is_err());
            req.after = None;
            req.include = None;
            req.max_bytes = Some(512);
            assert!(
                serde_json::to_vec(&page(transcript, &req).unwrap())
                    .unwrap()
                    .len()
                    <= 512
            );
        }
    }
}
