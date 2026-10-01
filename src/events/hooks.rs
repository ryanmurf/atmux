use super::{AgentEvent, MAX_EVENT_BYTES};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env,
    io::Read as _,
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::UnixStream,
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookDelivery {
    pub harness: String,
    pub pane: String,
    pub parent_pid: u32,
    pub event: Option<String>,
    pub payload: Value,
}

pub(crate) fn socket_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = env::var_os("TMPDIR")
        .map(PathBuf::from)
        .context("TMPDIR is unavailable")?;
    #[cfg(not(target_os = "macos"))]
    let base = env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || PathBuf::from(format!("/run/user/{}", rustix::process::geteuid().as_raw())),
        PathBuf::from,
    );
    let meta = std::fs::symlink_metadata(&base)?;
    if !base.is_absolute()
        || !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o022 != 0
    {
        bail!("insecure hook runtime directory");
    }
    Ok(base.join("atmux/hooks.sock"))
}

pub(crate) fn validate_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt as _;
    super::spool::private_directory(path.parent().context("hook socket has no parent")?)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o777 != 0o600
    {
        bail!("insecure hook socket");
    }
    Ok(())
}

/// Best-effort, silent delivery. The caller always exits successfully.
/// The deadline covers stdin, connection and delivery, even with a hung owner.
pub async fn hook_client(harness: String, event: Option<String>) {
    let work = async {
        if !["claude", "codex"].contains(&harness.as_str()) {
            bail!("unknown harness");
        }
        let path = socket_path()?;
        validate_socket(&path)?;
        let pane = env::var("TMUX_PANE")?;
        let mut input = Vec::new();
        // Read nonblocking from a duplicated descriptor; Tokio stdin uses a
        // blocking worker that can keep runtime shutdown alive after timeout.
        let mut stdin = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(i32::try_from(rustix::fs::OFlags::NONBLOCK.bits())?)
            .open("/dev/stdin")?;
        loop {
            let mut bytes = [0; 4096];
            match stdin.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    input.extend_from_slice(&bytes[..n]);
                    if input.len() > MAX_EVENT_BYTES {
                        bail!("oversized hook");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        let payload = serde_json::from_slice(&input)?;
        let delivery = HookDelivery {
            harness,
            pane,
            parent_pid: u32::try_from(rustix::process::Pid::as_raw(rustix::process::getppid()))?,
            event,
            payload,
        };
        let bytes = serde_json::to_vec(&delivery)?;
        if bytes.len() > MAX_EVENT_BYTES {
            bail!("oversized hook delivery");
        }
        let mut socket = UnixStream::connect(path).await?;
        socket.write_all(&bytes).await?;
        socket.shutdown().await?;
        let mut ack = [0];
        let _ = socket.read(&mut ack).await;
        Ok::<(), anyhow::Error>(())
    };
    let _ = tokio::time::timeout(Duration::from_millis(160), work).await;
}

/// Injects only process-scoped CLI overrides; never writes native config files.
/// # Errors
/// Rejects an invalid event configuration or unavailable executable path.
pub fn configure_profiles(config: &mut crate::config::Config) -> Result<()> {
    let Some(events) = &config.events else {
        return Ok(());
    };
    events.validate(
        config.node.coordinator_only || !config.machines.is_empty() || config.discovery.enabled,
    )?;
    if !events.inject_hooks {
        return Ok(());
    }
    let binary = env::current_exe()?;
    for profile in &mut config.profiles {
        if !profile.args.iter().any(|v| v.contains(" hook ")) {
            let mut args = injection_args(&profile.harness, &binary)?;
            args.append(&mut profile.args);
            profile.args = args;
        }
    }
    Ok(())
}

pub(crate) fn injection_args(harness: &str, binary: &Path) -> Result<Vec<String>> {
    let command = shell_words::join([binary.to_string_lossy().as_ref(), "hook", harness]);
    let events = match harness {
        "claude" => &[
            "Notification",
            "PermissionRequest",
            "Stop",
            "PreCompact",
            "SessionStart",
            "SessionEnd",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
        ][..],
        "codex" => &[
            "PermissionRequest",
            "Stop",
            "PreCompact",
            "SessionStart",
            "SessionEnd",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
        ][..],
        _ => return Ok(Vec::new()),
    };
    let mut hooks = serde_json::Map::new();
    for event in events {
        hooks.insert(
            (*event).into(),
            json!([{"hooks":[{"type":"command", "command":command, "timeout":1}]}]),
        );
        if matches!(*event, "PreToolUse" | "PostToolUse") {
            hooks.get_mut(*event).unwrap()[0]["matcher"] = json!(if harness == "claude" {
                "^(AskUserQuestion|ExitPlanMode)$"
            } else {
                "^(request_user_input|request_user_input_async)$"
            });
        }
    }
    if harness == "claude" {
        return Ok(vec![
            "--settings".into(),
            serde_json::to_string(&json!({"hooks":hooks}))?,
        ]);
    }
    let mut args = vec![
        "--dangerously-bypass-hook-trust".into(),
        "-c".into(),
        "features.hooks=true".into(),
    ];
    for (event, handlers) in hooks {
        // JSON to TOML through serde avoids inventing or escaping TOML strings.
        let value: toml::Value = serde_json::from_value(handlers)?;
        args.extend(["-c".into(), format!("hooks.{event}={value}")]);
    }
    Ok(args)
}

/// Quick Resume command bridge, preserving env wrappers and native arguments.
/// # Errors
/// Rejects an unavailable atmux executable path.
pub fn inject_command(mut command: Vec<String>, enabled: bool) -> Result<Vec<String>> {
    if !enabled {
        return Ok(command);
    }
    if let Some((index, harness)) = command.iter().enumerate().find_map(|(index, value)| {
        let name = Path::new(value).file_name()?.to_str()?;
        if name == "codex" {
            Some((index, "codex"))
        } else if name == "claude" || name.starts_with("claude-") {
            Some((index, "claude"))
        } else {
            None
        }
    }) && !command.iter().any(|arg| arg.contains(" hook "))
    {
        command.splice(
            std::ops::Range {
                start: index + 1,
                end: index + 1,
            },
            injection_args(harness, &env::current_exe()?)?,
        );
    }
    Ok(command)
}

#[allow(clippy::too_many_lines)] // Explicit native-event allowlist keeps message extraction auditable.
pub(crate) fn map_hook(event: &mut AgentEvent, delivery: &HookDelivery) -> bool {
    if !["claude", "codex"].contains(&delivery.harness.as_str()) {
        return false;
    }
    event.harness.clone_from(&delivery.harness);
    let name = delivery
        .payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .or(delivery.event.as_deref())
        .unwrap_or_default();
    let reason = match (delivery.harness.as_str(), name) {
        ("claude", "PreToolUse") => match delivery.payload.get("tool_name").and_then(Value::as_str)
        {
            Some("AskUserQuestion") => "question",
            Some("ExitPlanMode") => "plan_approval",
            _ => return false,
        },
        ("codex", "PreToolUse")
            if delivery
                .payload
                .get("tool_name")
                .and_then(Value::as_str)
                .is_some_and(|v| {
                    matches!(v, "request_user_input" | "request_user_input_async")
                }) =>
        {
            "question"
        }
        ("claude" | "codex", "PostToolUse")
            if delivery
                .payload
                .get("tool_name")
                .and_then(Value::as_str)
                .is_some_and(|v| {
                    matches!(
                        v,
                        "AskUserQuestion"
                            | "ExitPlanMode"
                            | "request_user_input"
                            | "request_user_input_async"
                    )
                }) =>
        {
            event.event_type = "agent.working".into();
            ""
        }
        ("claude", "Notification") => match delivery
            .payload
            .get("notification_type")
            .and_then(Value::as_str)
        {
            Some("permission_prompt") => "permission",
            Some("idle_prompt") => "idle_prompt",
            Some("elicitation_dialog") => "question",
            _ => return false,
        },
        ("claude" | "codex", "PermissionRequest") => {
            if delivery.payload.get("tool_name").and_then(Value::as_str) == Some("ExitPlanMode") {
                "plan_approval"
            } else {
                "permission"
            }
        }
        ("claude" | "codex", "Stop") => {
            event.event_type = "agent.turn_completed".into();
            ""
        }
        ("claude" | "codex", "UserPromptSubmit") => {
            event.event_type = "agent.working".into();
            ""
        }
        ("claude" | "codex", "PreCompact") => {
            event.event_type = "agent.compacted".into();
            let trigger = delivery
                .payload
                .get("trigger")
                .and_then(Value::as_str)
                .filter(|v| ["auto", "manual"].contains(v));
            event.detail = json!({"trigger":trigger});
            ""
        }
        ("claude" | "codex", "SessionStart") => {
            if delivery.payload.get("source").and_then(Value::as_str) == Some("compact") {
                return false;
            }
            event.event_type = "agent.started".into();
            event.reason = Some(
                if delivery.payload.get("source").and_then(Value::as_str) == Some("resume") {
                    "resume"
                } else {
                    "launch"
                }
                .into(),
            );
            return true;
        }
        ("claude" | "codex", "SessionEnd") => {
            event.event_type = "agent.exited".into();
            ""
        }
        _ => return false,
    };
    if reason.is_empty() {
        event.reason = None;
    } else {
        event.event_type = "agent.needs_input".into();
        event.reason = Some(reason.into());
    }
    if let Some(model) = delivery
        .payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|v| {
            v.len() <= 128
                && v.bytes()
                    .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'.' | b'_'))
        })
    {
        event.model = Some(model.into());
    }
    true
}
