//! Owner-local policy for native-log-driven automatic context compaction.
//!
//! This module is intentionally data-only. The control plane maps a live pane
//! to its native log, evaluates this policy under the pane mutation gate, then
//! persists the returned marker in tmux before sending the literal `/compact`.

use crate::{
    config::AutoCompactConfig,
    status::{AgentKind, AgentStatus},
    tmux::Session,
    transcript::NativeContext,
};

pub(crate) const MARKER_OPTION: &str = "@atmux_auto_compact";
const MARKER_VERSION: &str = "ac1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Skip,
    ClearMarker,
    Compact { marker: String },
}

/// Evaluates one freshly observed local pane.
///
/// Comparisons are deliberately strict: exactly 15 minutes or exactly 200,000
/// input tokens does not qualify for the default "more than" thresholds.
#[must_use]
pub(crate) fn decide(
    policy: &AutoCompactConfig,
    now_epoch_seconds: u64,
    session: &Session,
    context: &NativeContext,
    existing_marker: Option<&str>,
) -> Decision {
    if !policy.enabled
        || session.status != AgentStatus::Waiting
        || !matches!(session.agent, AgentKind::Claude | AgentKind::Codex)
        || session.activity == 0
        || session.activity > now_epoch_seconds
    {
        return Decision::Skip;
    }
    let Some(inactivity_seconds) = policy.inactivity_minutes.checked_mul(60) else {
        return Decision::Skip;
    };
    if now_epoch_seconds.saturating_sub(session.activity) <= inactivity_seconds {
        return Decision::Skip;
    }

    let parsed_marker = match existing_marker {
        Some(marker) => match parse_marker(marker) {
            Some(marker) => Some(marker),
            // Unknown pane metadata is not ours to overwrite. Fail closed.
            None => return Decision::Skip,
        },
        None => None,
    };
    if let Some(marker) = &parsed_marker
        && marker.session_fingerprint == context.session_fingerprint
    {
        if context.input_tokens <= policy.input_tokens {
            return Decision::ClearMarker;
        }
        // One action per native-session threshold episode. Keep suppressing
        // until the CLI proves the post-compact context fell below threshold.
        return Decision::Skip;
    }

    if context.input_tokens <= policy.input_tokens {
        return if parsed_marker.is_some() {
            Decision::ClearMarker
        } else {
            Decision::Skip
        };
    }
    if context.reset_pending {
        return Decision::Skip;
    }
    Decision::Compact {
        marker: encode_marker(
            &context.session_fingerprint,
            context.input_tokens,
            now_epoch_seconds,
        ),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Marker<'a> {
    session_fingerprint: &'a str,
    _input_tokens: u64,
    _acted_at: u64,
}

fn encode_marker(fingerprint: &str, input_tokens: u64, acted_at: u64) -> String {
    format!("{MARKER_VERSION}:{fingerprint}:{input_tokens}:{acted_at}")
}

fn parse_marker(value: &str) -> Option<Marker<'_>> {
    let mut parts = value.split(':');
    let version = parts.next()?;
    let fingerprint = parts.next()?;
    let input_tokens = parts.next()?.parse().ok()?;
    let acted_at = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || version != MARKER_VERSION
        || fingerprint.len() != 64
        || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(Marker {
        session_fingerprint: fingerprint,
        _input_tokens: input_tokens,
        _acted_at: acted_at,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Barrier, Mutex},
        thread,
    };

    use super::*;

    fn session(status: AgentStatus, agent: AgentKind, activity: u64) -> Session {
        Session {
            name: "agent".to_owned(),
            attached: false,
            windows: 1,
            activity,
            window_index: 0,
            pane_index: 0,
            pane_id: "%1".to_owned(),
            pane_pid: 1,
            pane_identity: format!("pane-v1-{}", "a".repeat(64)),
            agent_pid: Some(2),
            agent_started_ms: Some(1),
            path: PathBuf::from("/project"),
            command: "claude".to_owned(),
            launch_command: "claude".to_owned(),
            title: String::new(),
            content: String::new(),
            content_hash: 0,
            agent,
            profile: "Default".to_owned(),
            resume_lease: None,
            status,
        }
    }

    fn context(tokens: u64) -> NativeContext {
        NativeContext {
            session_fingerprint: "a".repeat(64),
            input_tokens: tokens,
            reset_pending: false,
        }
    }

    fn enabled_policy() -> AutoCompactConfig {
        AutoCompactConfig {
            enabled: true,
            ..AutoCompactConfig::default()
        }
    }

    #[test]
    fn defaults_require_strictly_more_than_fifteen_minutes_and_two_hundred_k() {
        let policy = enabled_policy();
        let exact_time = session(AgentStatus::Waiting, AgentKind::Claude, 100);
        assert_eq!(
            decide(&policy, 1_000, &exact_time, &context(200_001), None),
            Decision::Skip
        );
        let eligible = session(AgentStatus::Waiting, AgentKind::Claude, 99);
        assert_eq!(
            decide(&policy, 1_000, &eligible, &context(200_000), None),
            Decision::Skip
        );
        assert!(matches!(
            decide(&policy, 1_000, &eligible, &context(200_001), None),
            Decision::Compact { .. }
        ));
    }

    #[test]
    fn working_future_unknown_and_unsupported_sessions_fail_closed() {
        let policy = enabled_policy();
        for candidate in [
            session(AgentStatus::Working, AgentKind::Claude, 1),
            session(AgentStatus::Waiting, AgentKind::Other, 1),
            session(AgentStatus::Waiting, AgentKind::Codex, 0),
            session(AgentStatus::Waiting, AgentKind::Codex, 2_000),
        ] {
            assert_eq!(
                decide(&policy, 1_000, &candidate, &context(300_000), None),
                Decision::Skip
            );
        }
    }

    #[test]
    fn marker_survives_restart_and_resets_only_after_a_proven_token_drop() {
        let policy = enabled_policy();
        let candidate = session(AgentStatus::Waiting, AgentKind::Codex, 1);
        let Decision::Compact { marker } =
            decide(&policy, 1_000, &candidate, &context(200_001), None)
        else {
            panic!("expected initial compact");
        };
        // A new policy instance models a process restart; tmux metadata remains.
        let restarted = enabled_policy();
        assert_eq!(
            decide(
                &restarted,
                2_000,
                &candidate,
                &context(250_000),
                Some(&marker)
            ),
            Decision::Skip
        );
        assert_eq!(
            decide(
                &restarted,
                2_000,
                &candidate,
                &context(10_000),
                Some(&marker)
            ),
            Decision::ClearMarker
        );
        assert!(matches!(
            decide(&restarted, 3_000, &candidate, &context(250_000), None),
            Decision::Compact { .. }
        ));
    }

    #[test]
    fn native_compact_after_latest_usage_blocks_an_action() {
        let mut pending = context(250_000);
        pending.reset_pending = true;
        assert_eq!(
            decide(
                &enabled_policy(),
                1_000,
                &session(AgentStatus::Waiting, AgentKind::Claude, 1),
                &pending,
                None
            ),
            Decision::Skip
        );
    }

    #[test]
    fn mutation_gate_allows_only_one_concurrent_claim() {
        let gate = Arc::new(Mutex::new(None::<String>));
        let barrier = Arc::new(Barrier::new(3));
        let claims = Arc::new(Mutex::new(0_u8));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let gate = Arc::clone(&gate);
            let barrier = Arc::clone(&barrier);
            let claims = Arc::clone(&claims);
            workers.push(thread::spawn(move || {
                barrier.wait();
                let mut marker = gate.lock().unwrap();
                if let Decision::Compact { marker: claim } = decide(
                    &enabled_policy(),
                    1_000,
                    &session(AgentStatus::Waiting, AgentKind::Claude, 1),
                    &context(250_000),
                    marker.as_deref(),
                ) {
                    *marker = Some(claim);
                    *claims.lock().unwrap() += 1;
                }
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(*claims.lock().unwrap(), 1);
    }
}
