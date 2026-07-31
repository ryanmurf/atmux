use std::fmt;

use crate::config::StatusConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Codex,
    Claude,
    Other,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Other => "Shell",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Working,
    Waiting,
    Other,
}

impl AgentStatus {
    #[must_use]
    pub const fn icon(self) -> &'static str {
        match self {
            Self::Working => "●",
            Self::Waiting => "◆",
            Self::Other => "○",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Other => "other",
        }
    }
}

#[must_use]
pub fn detect_kind(current_command: &str, process_tree: &str) -> AgentKind {
    let haystack = format!("{current_command} {process_tree}").to_lowercase();
    if haystack.split_whitespace().any(is_codex_token) || haystack.contains("/codex ") {
        AgentKind::Codex
    } else if haystack.split_whitespace().any(is_claude_token) {
        AgentKind::Claude
    } else {
        AgentKind::Other
    }
}

fn is_codex_token(token: &str) -> bool {
    token == "codex" || token.ends_with("/codex")
}

fn is_claude_token(token: &str) -> bool {
    token == "claude"
        || token.starts_with("claude-")
        || token.ends_with("/claude")
        || token
            .rsplit_once('/')
            .is_some_and(|(_, name)| name.starts_with("claude-"))
}

#[must_use]
pub fn classify(
    kind: AgentKind,
    content: &str,
    title: &str,
    override_value: &str,
    changed: bool,
    config: &StatusConfig,
) -> AgentStatus {
    match override_value.trim().to_lowercase().as_str() {
        "working" | "busy" => return AgentStatus::Working,
        "waiting" | "input" | "idle" | "ready" => return AgentStatus::Waiting,
        "other" | "off" => return AgentStatus::Other,
        _ => {}
    }

    if kind == AgentKind::Other {
        return AgentStatus::Other;
    }

    let recent = content.lines().rev().take(18).collect::<Vec<_>>();
    let lower = recent
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let immediate = content
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let waiting_markers = [
        "do you want to proceed?",
        "would you like to proceed?",
        "waiting for your input",
        "press enter to continue",
        "select an option",
        "yes, allow",
        "allow this command",
        "[y/n]",
        "(y/n)",
    ];
    if waiting_markers
        .iter()
        .any(|marker| immediate.contains(marker))
        || config
            .waiting_markers
            .iter()
            .any(|marker| immediate.contains(&marker.to_lowercase()))
    {
        return AgentStatus::Waiting;
    }

    let working_markers = [
        "esc to interrupt",
        "ctrl+c to interrupt",
        "working (",
        "running…",
        "running...",
    ];
    if working_markers.iter().any(|marker| lower.contains(marker))
        || config
            .working_markers
            .iter()
            .any(|marker| lower.contains(&marker.to_lowercase()))
        || title
            .chars()
            .next()
            .is_some_and(|character| ('\u{2801}'..='\u{28ff}').contains(&character))
    {
        return AgentStatus::Working;
    }

    let tail = content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim_start();
    if tail.starts_with('›') || tail.starts_with('❯') || tail.starts_with("> ") {
        return AgentStatus::Waiting;
    }

    if changed {
        AgentStatus::Working
    } else {
        AgentStatus::Waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> StatusConfig {
        StatusConfig::default()
    }

    #[test]
    fn detects_agents_from_process_command() {
        assert_eq!(detect_kind("node", "node /opt/bin/codex"), AgentKind::Codex);
        assert_eq!(
            detect_kind("bash", "/home/me/.local/bin/claude-max"),
            AgentKind::Claude
        );
        assert_eq!(detect_kind("bash", "claude-max"), AgentKind::Claude);
        assert_eq!(detect_kind("bash", "bash"), AgentKind::Other);
        assert_eq!(
            detect_kind("atmux", "atmux --config /tmp/claude-tmp/config.toml"),
            AgentKind::Other
        );
    }

    #[test]
    fn working_marker_wins_over_composer_prompt() {
        let content = "• Working (12s • esc to interrupt)\n\n› next prompt";
        assert_eq!(
            classify(AgentKind::Codex, content, "", "", false, &config()),
            AgentStatus::Working
        );
    }

    #[test]
    fn prompt_means_waiting() {
        let content = "✻ Baked for 1m\n\n❯ ";
        assert_eq!(
            classify(AgentKind::Claude, content, "", "", false, &config()),
            AgentStatus::Waiting
        );
    }

    #[test]
    fn explicit_override_is_authoritative() {
        assert_eq!(
            classify(AgentKind::Other, "", "", "working", false, &config()),
            AgentStatus::Working
        );
    }

    #[test]
    fn historical_busy_marker_does_not_hide_current_prompt() {
        let mut lines = vec!["• Working (12s • esc to interrupt)".to_owned()];
        lines.extend((0..20).map(|index| format!("completed output {index}")));
        lines.push("❯ ".to_owned());
        assert_eq!(
            classify(
                AgentKind::Claude,
                &lines.join("\n"),
                "",
                "",
                false,
                &config()
            ),
            AgentStatus::Waiting
        );
    }

    #[test]
    fn custom_markers_extend_detection() {
        let custom = StatusConfig {
            working_markers: vec!["crunching widgets".to_owned()],
            waiting_markers: Vec::new(),
        };
        assert_eq!(
            classify(
                AgentKind::Codex,
                "crunching widgets",
                "",
                "",
                false,
                &custom
            ),
            AgentStatus::Working
        );
    }
}
