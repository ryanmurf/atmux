//! Headless conversation summaries for duplicate-with-summary launches.
//!
//! When an account runs out of usage the owner duplicates the pane under a
//! different credential profile. That new profile still has usage, so it is
//! the one asked to summarize the old conversation: the CLI runs
//! non-interactively, outside tmux, with the target profile's environment and
//! the target project directory. No caller-supplied value becomes a program,
//! flag, or environment assignment here.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::process::Command;

use crate::{config::AgentProfile, transcript::TranscriptMessage};

/// One headless summary is a single short CLI turn. Beyond this the duplicate
/// is not worth waiting for and the owner is told to launch without a summary.
pub(crate) const SUMMARY_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_PROMPT_ENTRIES: usize = 40;
const MAX_PROMPT_BYTES: usize = 24 * 1024;
const MAX_ENTRY_CHARS: usize = 1_500;
const MAX_SUMMARY_CHARS: usize = 8_000;
const MAX_STDERR_CHARS: usize = 400;

const INSTRUCTIONS: &str = "Summarize this coding session for a fresh agent that will continue it. \
Reply with the summary only, no preamble and no questions. Cover the goal, the current state, \
the files touched, the decisions taken, what remains, and the exact next step.";

const CONTEXT_PREFIX: &str = "Context from previous session (summarized):";

/// Builds the bounded summarization prompt for one previous conversation.
///
/// Tool calls, tool results, and subagent turns are dropped: they are the
/// bulk of a transcript and the least useful part of a handover. Entries are
/// taken newest-first so a long session contributes its most recent work, then
/// restored to chronological order.
///
/// Returns `None` when the conversation carries no summarizable turn.
pub(crate) fn summary_prompt(messages: &[TranscriptMessage]) -> Option<String> {
    let mut selected = Vec::new();
    let mut remaining = MAX_PROMPT_BYTES;
    for message in messages.iter().rev() {
        if selected.len() >= MAX_PROMPT_ENTRIES {
            break;
        }
        let Some(role) = summarizable_role(message) else {
            continue;
        };
        let text = message.markdown.trim();
        if text.is_empty() {
            continue;
        }
        let entry = format!("[{role}] {}", truncate_chars(text, MAX_ENTRY_CHARS));
        let Some(left) = remaining.checked_sub(entry.len()) else {
            break;
        };
        remaining = left;
        selected.push(entry);
    }
    if selected.is_empty() {
        return None;
    }
    selected.reverse();
    Some(format!(
        "{INSTRUCTIONS}\n\n<transcript>\n{}\n</transcript>",
        selected.join("\n\n")
    ))
}

/// Wraps a finished summary as the duplicate's first turn.
pub(crate) fn initial_prompt(summary: &str) -> String {
    format!("{CONTEXT_PREFIX}\n\n{}", summary.trim())
}

fn summarizable_role(message: &TranscriptMessage) -> Option<&'static str> {
    if message.kind != "message" {
        return None;
    }
    match message.role.as_str() {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        _ => None,
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_owned(),
    }
}

/// Builds the non-interactive argument list for one profile's harness.
///
/// # Errors
///
/// Returns an error when the harness has no supported non-interactive mode or
/// when the profile already pins a conversation selector that would make this
/// run continue an existing conversation instead of reading the prompt.
pub(crate) fn headless_arguments(profile: &AgentProfile, prompt: &str) -> Result<Vec<String>> {
    let harness = profile.harness.to_ascii_lowercase();
    let reserved: &[&str] = match harness.as_str() {
        "claude" => &["--resume", "-r", "--continue", "-c", "--print", "-p"],
        "codex" => &["resume", "exec", "fork"],
        other => bail!("profile harness {other} cannot summarize a previous session"),
    };
    if let Some(argument) = profile
        .args
        .iter()
        .find(|argument| reserved.contains(&argument.as_str()))
    {
        bail!("profile already defines {argument}, so it cannot run a headless summary");
    }
    let mut arguments = Vec::new();
    if harness == "codex" {
        arguments.push("exec".to_owned());
    }
    arguments.extend(profile.args.iter().cloned());
    if harness == "claude" {
        arguments.extend([
            "--print".to_owned(),
            "--output-format".to_owned(),
            "text".to_owned(),
        ]);
    } else {
        arguments.extend([
            "--color".to_owned(),
            "never".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "--sandbox".to_owned(),
            "read-only".to_owned(),
        ]);
    }
    arguments.push(prompt.to_owned());
    Ok(arguments)
}

/// Extracts the agent's last message from `codex exec` output.
///
/// Codex prefixes each event with a timestamp and names the speaking role on
/// its own line. Everything after the final `codex` marker is the answer; a
/// release that stops emitting the marker falls back to the whole output,
/// which is still a usable handover rather than a failure.
pub(crate) fn codex_final_message(stdout: &str) -> &str {
    let mut start = None;
    let mut offset = 0;
    for line in stdout.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim();
        if trimmed == "codex" || trimmed.ends_with("] codex") {
            start = Some(offset);
        }
    }
    let answer = start.map_or("", |start| stdout[start..].trim());
    if answer.is_empty() {
        stdout.trim()
    } else {
        answer
    }
}

/// Runs one profile's CLI non-interactively to summarize a previous session.
///
/// # Errors
///
/// Returns an error when the CLI cannot start, exceeds [`SUMMARY_TIMEOUT`],
/// exits unsuccessfully, or produces no summary text.
pub(crate) async fn summarize(
    profile: &AgentProfile,
    directory: &Path,
    prompt: &str,
) -> Result<String> {
    let arguments = headless_arguments(profile, prompt)?;
    let mut command = Command::new(&profile.command);
    command
        .args(&arguments)
        .current_dir(directory)
        .envs(&profile.env)
        // The summarizer runs beside the owner's agents, never inside one.
        // Without this it would inherit atmux's own tmux client and could
        // reach the protected server it was started from.
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A new process group makes the deadline reach the CLI's descendants
    // instead of only the process atmux spawned.
    let child = command
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to run {}", profile.command))?;
    let group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    let Ok(finished) = tokio::time::timeout(SUMMARY_TIMEOUT, child.wait_with_output()).await else {
        if let Some(group) = group {
            let _ = kill_process_group(group, Signal::KILL);
        }
        bail!(
            "{} did not summarize the previous session within {} seconds",
            profile.command,
            SUMMARY_TIMEOUT.as_secs()
        );
    };
    let output = finished?;
    if !output.status.success() {
        bail!(
            "{} could not summarize the previous session ({}): {}",
            profile.command,
            output.status,
            truncate_chars(
                String::from_utf8_lossy(&output.stderr).trim(),
                MAX_STDERR_CHARS
            )
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = if profile.harness.eq_ignore_ascii_case("codex") {
        codex_final_message(&stdout)
    } else {
        stdout.trim()
    };
    if text.is_empty() {
        bail!("{} returned an empty session summary", profile.command);
    }
    Ok(truncate_chars(text, MAX_SUMMARY_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, kind: &str, markdown: &str) -> TranscriptMessage {
        TranscriptMessage {
            id: format!("{role}-{kind}-{}", markdown.len()),
            role: role.to_owned(),
            kind: kind.to_owned(),
            markdown: markdown.to_owned(),
            tool_name: None,
            tool_input: None,
            tool_output: None,
            timestamp: None,
            agent_name: None,
            input_tokens: None,
            output_tokens: None,
        }
    }

    fn profile(harness: &str, args: &[&str]) -> AgentProfile {
        AgentProfile {
            name: "max".to_owned(),
            harness: harness.to_owned(),
            command: harness.to_owned(),
            args: args.iter().map(|argument| (*argument).to_owned()).collect(),
            env: std::collections::BTreeMap::new(),
            inherit_discovered: false,
            modes: Vec::new(),
        }
    }

    #[test]
    fn summary_prompt_keeps_conversation_turns_and_drops_tool_noise() {
        let messages = vec![
            message("user", "message", "port the launcher"),
            message("assistant", "tool", "Bash(cargo test)"),
            message("subagent", "message", "explorer notes"),
            message("assistant", "message", "  done, one test fails  "),
            message("user", "message", "   "),
        ];
        let prompt = summary_prompt(&messages).expect("a conversation has a prompt");
        assert!(prompt.starts_with(INSTRUCTIONS));
        assert!(prompt.contains("[user] port the launcher"));
        assert!(prompt.contains("[assistant] done, one test fails"));
        assert!(
            !prompt.contains("cargo test"),
            "tool calls are not handover"
        );
        assert!(
            !prompt.contains("explorer notes"),
            "subagents are not handover"
        );
        assert!(
            prompt.find("[user] port").unwrap() < prompt.find("[assistant] done").unwrap(),
            "entries stay chronological"
        );
        assert_eq!(summary_prompt(&[]), None);
        assert_eq!(summary_prompt(&[message("assistant", "tool", "ls")]), None);
    }

    #[test]
    fn summary_prompt_bounds_entry_count_and_total_size() {
        let many = (0..MAX_PROMPT_ENTRIES + 20)
            .map(|index| message("user", "message", &format!("turn {index}")))
            .collect::<Vec<_>>();
        let prompt = summary_prompt(&many).expect("a prompt");
        assert_eq!(prompt.matches("[user]").count(), MAX_PROMPT_ENTRIES);
        assert!(prompt.contains(&format!("turn {}", MAX_PROMPT_ENTRIES + 19)));
        assert!(
            !prompt.contains("[user] turn 0\n"),
            "oldest turns are dropped"
        );

        let long = message("user", "message", &"x".repeat(MAX_ENTRY_CHARS * 3));
        let truncated = summary_prompt(std::slice::from_ref(&long)).expect("a prompt");
        assert!(truncated.contains('…'));
        assert!(truncated.len() < MAX_ENTRY_CHARS * 3);

        let huge = (0..MAX_PROMPT_ENTRIES)
            .map(|_| long.clone())
            .collect::<Vec<_>>();
        let bounded = summary_prompt(&huge).expect("a prompt");
        assert!(bounded.len() <= MAX_PROMPT_BYTES + INSTRUCTIONS.len() + 64);
    }

    #[test]
    fn summary_prompt_truncates_on_a_character_boundary() {
        let wide = message("user", "message", &"é".repeat(MAX_ENTRY_CHARS + 10));
        let prompt = summary_prompt(std::slice::from_ref(&wide)).expect("a prompt");
        assert_eq!(prompt.matches('é').count(), MAX_ENTRY_CHARS);
    }

    #[test]
    fn headless_arguments_are_harness_specific_and_reject_pinned_selectors() {
        assert_eq!(
            headless_arguments(
                &profile("claude", &["--dangerously-skip-permissions"]),
                "why"
            )
            .unwrap(),
            vec![
                "--dangerously-skip-permissions",
                "--print",
                "--output-format",
                "text",
                "why"
            ]
        );
        let codex = headless_arguments(&profile("codex", &["-m", "gpt-5.6"]), "why").unwrap();
        assert_eq!(codex.first().map(String::as_str), Some("exec"));
        assert_eq!(codex.last().map(String::as_str), Some("why"));
        assert!(codex.contains(&"read-only".to_owned()));
        for pinned in [
            profile("claude", &["--continue"]),
            profile("claude", &["-c"]),
            profile("codex", &["resume"]),
        ] {
            assert!(headless_arguments(&pinned, "why").is_err());
        }
        assert!(headless_arguments(&profile("gemini", &[]), "why").is_err());
    }

    #[test]
    fn codex_final_message_takes_the_last_agent_turn() {
        let stdout = "workdir: /srv\nmodel: gpt\n[2026-01-01T00:00:00] User instructions:\nhi\n[2026-01-01T00:00:01] codex\nGoal: ship it.\nNext: run tests.\n";
        assert_eq!(
            codex_final_message(stdout),
            "Goal: ship it.\nNext: run tests."
        );
        assert_eq!(codex_final_message("  bare output  "), "bare output");
    }

    #[test]
    fn initial_prompt_labels_the_handover() {
        assert_eq!(
            initial_prompt("  work left  "),
            format!("{CONTEXT_PREFIX}\n\nwork left")
        );
    }
}
