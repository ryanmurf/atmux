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
    } else if haystack.split_whitespace().any(is_claude_token)
        // Recent Claude Code releases execute a versioned binary such as
        // `~/.local/share/claude/versions/2.1.232`. Its basename has no
        // "claude" marker, so use the stable executable path or the
        // profile-scoping environment variable emitted by atmux launches.
        || haystack.contains("/.local/share/claude/versions/")
        || haystack.contains("claude_config_dir=")
    {
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
    if WAITING_MARKERS
        .iter()
        .any(|marker| immediate.contains(marker))
        || config
            .waiting_markers
            .iter()
            .any(|marker| immediate.contains(&marker.to_lowercase()))
    {
        return AgentStatus::Waiting;
    }

    if WORKING_MARKERS.iter().any(|marker| lower.contains(marker))
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

/// Characters the harnesses use to rule off the composer box.
const COMPOSER_RULE_CHARS: &str = "─━┄┅┈┉│┃┌┐└┘├┤┬┴┼╭╮╯╰═║╔╗╚╝╠╣╦╩╬▁▔";

/// Phrases that mean the harness is mid-turn.
const WORKING_MARKERS: [&str; 5] = [
    "esc to interrupt",
    "ctrl+c to interrupt",
    "working (",
    "running…",
    "running...",
];

/// Phrases that mean the harness is holding a question open.
const WAITING_MARKERS: [&str; 9] = [
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

/// Extra dialog phrases that only an automation needs to fear. A first-run
/// trust dialog reuses the composer glyph as its menu cursor and closes with
/// this hint, so submitting into it would answer a security question.
const AUTOMATION_DIALOG_MARKERS: [&str; 1] = ["enter to confirm"];

/// Drops the blank rows a harness pads the pane with. Codex parks its composer
/// mid-screen, so the last row of the capture carries no signal at all.
fn visible_tail(content: &str) -> Vec<&str> {
    let mut lines = content.lines().collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// Recognizes a `1.`/`2)` menu enumerator, which marks a dialog choice rather
/// than composer text or harness chrome.
fn option_enumerator(text: &str) -> bool {
    let trimmed = text.trim_start();
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && trimmed[digits..].starts_with(['.', ')'])
}

/// Accepts the chrome a harness draws under its composer: blank rows, the box
/// rules, and one-line status or shortcut strips. A dialog instead leaves its
/// remaining choices there, and those read as enumerated options or sentences.
fn footer_row(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.chars().all(|c| COMPOSER_RULE_CHARS.contains(c)) {
        return true;
    }
    if option_enumerator(trimmed) {
        return false;
    }
    !trimmed.ends_with(['.', '?', '!', ':'])
}

/// Proves the composer holds nothing an automation would corrupt.
///
/// Both CLIs now seed the empty composer with a dim placeholder hint, and tmux
/// hands us plain text with the dim styling already stripped, so an empty row
/// is not the only shape of "empty". Claude 2.1.261 draws its `←` agents
/// affordance in the status strip only while the composer is empty (checked in
/// bypass, auto, accept-edits, plan, and manual modes, and against a single
/// typed space), and its first-run hint is a quoted suggestion. Codex 0.153.4
/// uses one fixed placeholder string.
fn empty_composer(kind: AgentKind, body: &str, footer: &[&str]) -> bool {
    let body = body.trim();
    if body.is_empty() {
        return true;
    }
    if option_enumerator(body) {
        return false;
    }
    match kind {
        AgentKind::Claude => {
            (body.starts_with("Try \"") && body.ends_with('"'))
                || footer.iter().any(|line| {
                    line.split('·')
                        .any(|segment| segment.trim().starts_with('←'))
                })
        }
        AgentKind::Codex => body == "Ask Codex to do anything",
        AgentKind::Other => false,
    }
}

/// Proves that an automation may safely submit at the harness's empty,
/// top-level composer. Generic Waiting fallbacks, overrides, approval prompts,
/// option pickers, and custom waiting markers are deliberately insufficient.
///
/// The composer is no longer the last row of the pane: both CLIs render a
/// status footer beneath it. Idle therefore means a composer row whose body is
/// empty, with nothing but recognized chrome below it and no spinner or dialog
/// beside it.
#[must_use]
pub(crate) fn automation_idle(
    kind: AgentKind,
    content: &str,
    title: &str,
    config: &StatusConfig,
) -> bool {
    let glyph = match kind {
        AgentKind::Claude => '❯',
        AgentKind::Codex => '›',
        AgentKind::Other => return false,
    };
    if title
        .chars()
        .next()
        .is_some_and(|character| ('\u{2801}'..='\u{28ff}').contains(&character))
    {
        return false;
    }
    let lines = visible_tail(content);
    let Some(prompt) = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with(glyph))
    else {
        return false;
    };
    // Both harnesses render their live status next to the composer: Claude in
    // the strip below it, Codex on the row above. Anchoring the scan there
    // catches the spinner that matters without letting a scrolled-back one
    // freeze automation forever.
    let window = lines[prompt.saturating_sub(8)..].join("\n").to_lowercase();
    if WORKING_MARKERS
        .iter()
        .chain(WAITING_MARKERS.iter())
        .chain(AUTOMATION_DIALOG_MARKERS.iter())
        .any(|marker| window.contains(marker))
        || config
            .working_markers
            .iter()
            .any(|marker| window.contains(&marker.to_lowercase()))
    {
        return false;
    }
    let footer = &lines[prompt + 1..];
    if !footer.iter().all(|line| footer_row(line)) {
        return false;
    }
    let body = lines[prompt]
        .trim_start()
        .strip_prefix(glyph)
        .unwrap_or_default();
    empty_composer(kind, body, footer)
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
            detect_kind(
                "2.1.232",
                "Claude Code v2.1.232\nSonnet 5 with xhigh effort"
            ),
            AgentKind::Claude
        );
        assert_eq!(
            detect_kind(
                "2.1.232",
                "env CLAUDE_CONFIG_DIR=/Users/ryan/.claude-hd /Users/ryan/.local/share/claude/versions/2.1.232"
            ),
            AgentKind::Claude
        );
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

    /// Real `tmux capture-pane -p` tails from Claude Code 2.1.261 and Codex
    /// 0.153.4. Both CLIs now draw a status footer under the composer and seed
    /// the empty composer with a dim placeholder, so neither one ever ends the
    /// pane on a bare prompt glyph again.
    mod panes {
        pub const CLAUDE_FRESH: &str = concat!(
            "                  tmux focus-events off · add 'set -g focus-events on' to ~/.tmux.conf\n",
            "────────────────────────────────────────────────────────────\n",
            "❯\u{a0}Try \"fix lint errors\"\n",
            "────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n",
        );
        pub const CLAUDE_IDLE_SUGGESTION: &str = concat!(
            "────────────────────────────────────────────────────────────\n",
            "❯\u{a0}did it finish?\n",
            "────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on · 1 shell · ← for agents · ↓ to manage\n",
        );
        pub const CLAUDE_IDLE_AFTER_TURN: &str = concat!(
            "✻ Crunched for 3s · done 7:51 PM\n",
            "────────────────────────────────────────────────────────────\n",
            "❯\u{a0}now do 41 to 60\n",
            "────────────────────────────────────────────────────────────\n",
            "  ⏸ manual mode on · ? for shortcuts · ← for agents\n",
        );
        pub const CLAUDE_TYPED: &str = concat!(
            "────────────────────────────────────────────────────────────\n",
            "❯\u{a0}hello there\n",
            "────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        pub const CLAUDE_STREAMING: &str = concat!(
            "  twenty-nine\n",
            "  thirty\n",
            "                                              ● high · /effort\n",
            "────────────────────────────────────────────────────────────\n",
            "❯\u{a0}\n",
            "────────────────────────────────────────────────────────────\n",
            "  ⏸ manual mode on · esc to interrupt · ← for agents\n",
        );
        pub const CLAUDE_APPROVAL: &str = concat!(
            " This command requires approval\n",
            "\n",
            " Do you want to proceed?\n",
            " ❯ 1. Yes\n",
            "   2. Yes, and don’t ask again for: rtk curl *\n",
            "   3. Yes, and switch to auto mode · auto mode handles these prompts for you\n",
            "   4. No\n",
            "\n",
            " Esc to cancel · Tab to amend\n",
        );
        pub const CLAUDE_TRUST: &str = concat!(
            " Security guide\n",
            "\n",
            " ❯ No, exit\n",
            "   Yes, I trust this folder\n",
            "\n",
            " Enter to confirm · Esc to cancel\n",
        );
        pub const CODEX_IDLE: &str = concat!(
            "• You have 2 usage limit resets available. Run /usage to use one.\n",
            "\n",
            "\n",
            "› Ask Codex to do anything\n",
            "\n",
            "  gpt-5.6-sol low fast · /tmp/claude-1000/-home-ryan-IdeaProjects-atmux/scratchpad…\n",
            "\n",
            "\n",
            "\n",
        );
        pub const CODEX_TYPED: &str = concat!(
            "• You have 2 usage limit resets available. Run /usage to use one.\n",
            "\n",
            "\n",
            "› hello there\n",
            "\n",
            "  gpt-5.6-sol low fast · /tmp/claude-1000/-home-ryan-IdeaProjects-atmux/scratchpad…\n",
            "\n",
            "\n",
            "\n",
        );
        pub const CODEX_TRUST: &str = concat!(
            "  Do you trust the contents of this directory? Working with untrusted contents\n",
            "  comes with higher risk of prompt injection.\n",
            "\n",
            "› 1. Yes, continue\n",
            "  2. No, quit\n",
            "\n",
            "  Press enter to continue\n",
            "\n",
            "\n",
            "\n",
        );
    }

    #[test]
    fn automation_reads_an_empty_composer_through_the_status_footer() {
        for pane in [
            panes::CLAUDE_FRESH,
            panes::CLAUDE_IDLE_SUGGESTION,
            panes::CLAUDE_IDLE_AFTER_TURN,
        ] {
            assert!(
                automation_idle(AgentKind::Claude, pane, "", &config()),
                "expected idle for {pane:?}"
            );
        }
        assert!(automation_idle(
            AgentKind::Codex,
            panes::CODEX_IDLE,
            "",
            &config()
        ));
        // A composer that already holds the operator's own draft is off limits:
        // submitting would append to it and send the pair.
        assert!(!automation_idle(
            AgentKind::Claude,
            panes::CLAUDE_TYPED,
            "",
            &config()
        ));
        assert!(!automation_idle(
            AgentKind::Codex,
            panes::CODEX_TYPED,
            "",
            &config()
        ));
    }

    #[test]
    fn automation_refuses_a_streaming_or_dialog_pane() {
        for (kind, pane) in [
            (AgentKind::Claude, panes::CLAUDE_STREAMING),
            (AgentKind::Claude, panes::CLAUDE_APPROVAL),
            (AgentKind::Claude, panes::CLAUDE_TRUST),
            (AgentKind::Codex, panes::CODEX_TRUST),
        ] {
            assert!(
                !automation_idle(kind, pane, "", &config()),
                "expected busy for {pane:?}"
            );
        }
        // A braille spinner in the pane title outranks a quiet-looking pane.
        assert!(!automation_idle(
            AgentKind::Claude,
            panes::CLAUDE_FRESH,
            "⠹ thinking",
            &config()
        ));
        assert!(!automation_idle(
            AgentKind::Other,
            panes::CLAUDE_FRESH,
            "",
            &config()
        ));
    }

    #[test]
    fn automation_requires_a_recognized_composer() {
        assert!(!automation_idle(
            AgentKind::Claude,
            "quiet but unrecognized work",
            "",
            &config()
        ));
        assert!(!automation_idle(
            AgentKind::Claude,
            "Do you want to proceed?\n❯",
            "",
            &config()
        ));
        assert!(!automation_idle(
            AgentKind::Codex,
            "Select an option\n›",
            "",
            &config()
        ));
        // A custom working marker still suppresses delivery.
        let custom = StatusConfig {
            working_markers: vec!["crunching widgets".to_owned()],
            waiting_markers: Vec::new(),
        };
        assert!(!automation_idle(
            AgentKind::Claude,
            "crunching widgets\n❯\u{a0}\n  ⏵⏵ bypass permissions on · ← for agents",
            "",
            &custom
        ));
        // Bare composers from older harness builds stay idle.
        assert!(automation_idle(
            AgentKind::Claude,
            "finished\n\n❯ ",
            "",
            &config()
        ));
        assert!(automation_idle(
            AgentKind::Codex,
            "finished\n\n› ",
            "",
            &config()
        ));
    }
}
