//! Narrow, opt-in startup-dialog handling shared by the web monitor and TUI.
use std::{path::Path, thread, time::Duration};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::{
    config::Config,
    status::AgentKind,
    tmux::{Session, Tmux},
};

const MAX_DIALOG_BYTES: usize = 16 * 1024;
const DEV_FLAG: &str = "--dangerously-load-development-channels";
const CODEX_TRUST_DISCLOSURE: &str = "Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. Folder settings can run code automatically, even without a model request. Continue only if you trust these files. Your trust decision will be saved.";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct StartupPromptConfig {
    pub auto_answer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialog {
    DevelopmentChannels,
    WorkspaceTrust,
    CodexTrust,
}

impl Dialog {
    const fn option(self) -> &'static str {
        match self {
            Self::DevelopmentChannels => "@atmux_startup_development",
            Self::WorkspaceTrust => "@atmux_startup_trust",
            Self::CodexTrust => "@atmux_startup_codex_trust",
        }
    }
}

fn lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.trim().trim_start_matches(['❯', '›']).trim())
        .filter(|line| !line.is_empty())
        .collect()
}

fn recognize(text: &str) -> Option<Dialog> {
    if text.len() > MAX_DIALOG_BYTES {
        return None;
    }
    let rows = lines(text);
    // Current Codex wraps its disclosure to terminal width. Rejoin only this
    // exact paragraph; do not treat a partial sentence as a trust dialog.
    if rows.contains(&"Folder access")
        && rows.last() == Some(&"enter continue · esc quit")
        && let Some(option) = rows.iter().position(|row| *row == "1. Trust and continue")
        && rows.get(option + 1) == Some(&"2. Quit")
        && let Some(disclosure) = rows[..option]
            .iter()
            .position(|row| row.starts_with("Trust this folder? "))
        && rows[disclosure..option].join(" ") == CODEX_TRUST_DISCLOSURE
    {
        return Some(Dialog::CodexTrust);
    }
    // Require complete native dialog rows, never a substring in an agent reply.
    let confirm = rows.last().is_some_and(|row| {
        matches!(
            *row,
            "Enter to confirm · Esc to cancel" | "Enter to confirm" | "Press enter to continue"
        )
    });
    if !confirm {
        return None;
    }
    if rows
        .iter().any(|row| matches!(*row, "--dangerously-load-development-channels is for local channel development only" | "--dangerously-load-development-channels is for local channel development only." | "--dangerously-load-development-channels is for local channel development only. Do not use this option to run channels you have downloaded off the internet."))
        && rows.contains(&"1. I am using this for local development")
    {
        return Some(Dialog::DevelopmentChannels);
    }
    if rows.contains(&"Security guide")
        && rows.contains(&"No, exit")
        && rows.contains(&"Yes, I trust this folder")
    {
        return Some(Dialog::WorkspaceTrust);
    }
    if rows.contains(&"1. Trust and continue")
        && rows.iter().any(|row| matches!(*row, "2. Quit" | "2. Exit"))
        && rows
            .iter()
            .any(|row| matches!(*row, "Do you trust the contents of this directory?"))
    {
        return Some(Dialog::CodexTrust);
    }
    None
}

fn has_flag(args: &[std::ffi::OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == DEV_FLAG)
}

fn trusted_cwd(config: &Config, cwd: &Path) -> bool {
    let Ok(cwd) = cwd.canonicalize() else {
        return false;
    };
    config
        .general
        .project_roots
        .iter()
        .filter_map(|root| crate::config::expand_tilde(root).canonicalize().ok())
        .any(|root| cwd.starts_with(root))
}

fn process(pid: u32) -> Option<(String, Vec<std::ffi::OsString>, std::path::PathBuf)> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    let process = system.process(pid)?;
    if process
        .user_id()
        .is_none_or(|uid| **uid != rustix::process::geteuid().as_raw())
    {
        return None;
    }
    if process.cmd().len() > 512
        || process.cmd().iter().map(|arg| arg.len()).sum::<usize>() > 64 * 1024
    {
        return None;
    }
    Some((
        format!(
            "{}:{}",
            pid.as_u32(),
            crate::control::native_process_start_stamp(pid.as_u32())?
        ),
        process.cmd().to_vec(),
        process.cwd()?.canonicalize().ok()?,
    ))
}

fn answer(
    config: &Config,
    session: &Session,
    events: Option<&crate::events::EventService>,
) -> Result<()> {
    if !matches!(session.agent, AgentKind::Claude | AgentKind::Codex) {
        return Ok(());
    }
    let Some(pid) = session.agent_pid else {
        return Ok(());
    };
    let Some((generation, args, agent_cwd)) = process(pid) else {
        return Ok(());
    };
    // A resume transaction can hold the registry lock while a concurrent close
    // holds the pane lock. Skip this scan rather than invert that lock order.
    let Some(_lock) = crate::auto_update::PaneProcessLock::try_acquire(&session.pane_id)? else {
        return Ok(());
    };
    let Some(live) = Tmux::live_pane_identity(&session.pane_id)? else {
        return Ok(());
    };
    if live.pane_identity != session.pane_identity || live.pane_pid != session.pane_pid {
        return Ok(());
    }
    let text = Tmux.capture(&session.pane_id, 80)?;
    let Some(dialog) = recognize(&text) else {
        if looks_like_startup(&text) {
            report(session, "unrecognized dialog", events);
        }
        return Ok(());
    };
    if (dialog == Dialog::CodexTrust && session.agent != AgentKind::Codex)
        || (dialog != Dialog::CodexTrust && session.agent != AgentKind::Claude)
        || !config.startup_prompts.auto_answer
        || (dialog == Dialog::DevelopmentChannels && !has_flag(&args))
        || (matches!(dialog, Dialog::WorkspaceTrust | Dialog::CodexTrust)
            && (!trusted_cwd(config, &live.path)
                || live.path.canonicalize().ok().as_ref() != Some(&agent_cwd)))
    {
        report(session, "startup dialog requires input", events);
        return Ok(());
    }
    let marker = Tmux::output([
        "show-options",
        "-p",
        "-v",
        "-q",
        "-t",
        &session.pane_id,
        dialog.option(),
    ])?;
    if marker.trim() == generation {
        return Ok(());
    }
    // Claim before keys: crashes and overlapping monitors fail closed. Each
    // recognized dialog is answered at most once per native process generation.
    Tmux::output([
        "set-option",
        "-p",
        "-t",
        &session.pane_id,
        dialog.option(),
        &generation,
    ])?;
    if process(pid).is_none_or(|(current, current_args, cwd)| {
        current != generation
            || cwd != agent_cwd
            || (dialog == Dialog::DevelopmentChannels && !has_flag(&current_args))
    }) {
        bail!("startup process changed");
    }
    match dialog {
        Dialog::DevelopmentChannels | Dialog::CodexTrust => {
            Tmux::output(["send-keys", "-t", &session.pane_id, "1", "Enter"])?;
        }
        Dialog::WorkspaceTrust => {
            let selected_yes = text
                .lines()
                .any(|line| line.trim() == "❯ Yes, I trust this folder");
            if !selected_yes {
                Tmux::output(["send-keys", "-t", &session.pane_id, "Down"])?;
            }
            Tmux::output(["send-keys", "-t", &session.pane_id, "Enter"])?;
        }
    }
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(25));
        if recognize(&Tmux.capture(&session.pane_id, 80)?) != Some(dialog) {
            if let Some(events) = events {
                events.startup_event(session, Some((dialog.option(), true)));
            }
            Tmux::output([
                "set-option",
                "-p",
                "-t",
                &session.pane_id,
                "@atmux_startup_answered",
                &format!("{generation}|{}", dialog.option()),
            ])?;
            return Ok(());
        }
    }
    if let Some(events) = events {
        events.startup_event(session, Some((dialog.option(), false)));
    }
    report(session, "dialog remained after one answer", events);
    Ok(())
}

fn looks_like_startup(text: &str) -> bool {
    text.len() <= MAX_DIALOG_BYTES
        && (text.contains("Security guide")
            || text.contains(DEV_FLAG)
            || text.contains("Do you trust the contents of this directory?")
            || text.contains("Folder access"))
}

// Captured text and argv never enter logs or the event spool.
fn report(session: &Session, reason: &str, events: Option<&crate::events::EventService>) {
    if let Some(events) = events {
        events.startup_event(session, None);
    } else {
        eprintln!(
            "atmux agent.needs_input/startup_prompt pane={} session_key={} reason={reason}",
            session.pane_id,
            session.session_key.as_deref().unwrap_or("unknown")
        );
    }
}

pub(crate) fn handle(config: &Config, session: &Session) {
    handle_with_events(config, session, None);
}

pub(crate) fn handle_with_events(
    config: &Config,
    session: &Session,
    events: Option<&crate::events::EventService>,
) {
    if let Some(events) = events
        && let Some(pid) = session.agent_pid
        && let Some((generation, _, _)) = process(pid)
        && let Ok(marker) = Tmux::output([
            "show-options",
            "-p",
            "-v",
            "-q",
            "-t",
            &session.pane_id,
            "@atmux_startup_answered",
        ])
        && let Some((answered, dialog)) = marker.trim().split_once('|')
        && answered == generation
    {
        events.startup_event(session, Some((dialog, true)));
    }
    if !config.startup_prompts.auto_answer && events.is_none() {
        return;
    }
    if let Err(error) = answer(config, session, events) {
        report(session, &error.to_string(), events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) const DEV: &str = "--dangerously-load-development-channels is for local channel development only\n❯ 1. I am using this for local development\n  2. Exit\nEnter to confirm · Esc to cancel\n";
    const TRUST: &str = "Security guide\n❯ No, exit\n  Yes, I trust this folder\nEnter to confirm · Esc to cancel\n";
    #[test]
    fn exact_dialogs_and_near_misses() {
        assert_eq!(recognize(DEV), Some(Dialog::DevelopmentChannels));
        assert_eq!(recognize(TRUST), Some(Dialog::WorkspaceTrust));
        for text in [
            DEV.replace("local development", "production"),
            DEV.replace("1.", "2."),
            TRUST.replace("No, exit", "No, run"),
            format!("{DEV}❯ Tell me what to do"),
            "x".repeat(MAX_DIALOG_BYTES + 1),
        ] {
            assert_eq!(recognize(&text), None);
        }
    }
    #[test]
    fn codex_folder_trust_matches_only_the_exact_option() {
        let text = "Do you trust the contents of this directory?\n› 1. Trust and continue\n  2. Quit\nPress enter to continue";
        assert_eq!(recognize(text), Some(Dialog::CodexTrust));
        assert_eq!(
            recognize(&text.replace("Trust and continue", "Yes, continue")),
            None
        );
        assert_eq!(recognize(&text.replace("2. Quit", "2. Run command")), None);
        assert_eq!(recognize(&format!("{text}\n› user draft")), None);
    }
    #[test]
    fn codex_current_folder_access_matches_wrapped_disclosure_exactly() {
        // Official Codex 40-column onboarding snapshot, including a long worktree path.
        let text = "  Folder access\n  workspace/…/repository\n  Trust this folder? Codex can read,\n  edit, and run files here, subject to\n  your permission settings. Folder\n  settings can run code automatically,\n  even without a model request.\n  Continue only if you trust these\n  files. Your trust decision will be\n  saved.\n› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit\n";
        assert_eq!(recognize(text), Some(Dialog::CodexTrust));
        let unwrapped = format!(
            "Folder access\n/worktree\n{CODEX_TRUST_DISCLOSURE}\n› 1. Trust and continue\n2. Quit\nenter continue · esc quit"
        );
        assert_eq!(recognize(&unwrapped), Some(Dialog::CodexTrust));
        for changed in [
            text.replace("Trust and continue", "Open restricted"),
            text.replace("2. Quit", "2. Keep current directory"),
            text.replace(
                "Your trust decision will be\n  saved.",
                "Your decision changed.",
            ),
            text.replace("enter continue · esc quit", "enter run · esc quit"),
            format!("{text}user draft"),
        ] {
            assert_eq!(recognize(&changed), None);
        }
    }
    #[test]
    fn trust_is_limited_to_canonical_project_roots() {
        let root = std::env::temp_dir().join(format!(
            "atmux-startup-root-{}",
            crate::tmux::new_session_key().unwrap()
        ));
        std::fs::create_dir_all(root.join("worktree")).unwrap();
        let mut config = Config::default();
        config.general.project_roots = vec![root.clone()];
        assert!(trusted_cwd(&config, &root.join("worktree")));
        assert!(!trusted_cwd(&config, root.parent().unwrap()));
        assert!(!trusted_cwd(&config, &root.join("missing")));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn flags_are_exact_active_argv_tokens() {
        let args = |tokens: &[&str]| {
            tokens
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert!(has_flag(&args(&["claude", DEV_FLAG])));
        for tokens in [
            vec!["claude", "--", DEV_FLAG],
            vec!["claude", "echo --dangerously-load-development-channels"],
            vec!["claude", "--dangerously-load-development-channels=false"],
        ] {
            assert!(!has_flag(&args(&tokens)));
        }
    }
}
