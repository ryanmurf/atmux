//! Opt-in systemd user-scope isolation for interactive agent process trees.
//!
//! `tmux` accepts a shell-command string, but this module first constructs a
//! fixed argv and doubles literal dollars using systemd's documented escape.
//! The sole string conversion happens at the tmux transport boundary via
//! `shell_words::join`; splitting that string recreates these exact arguments.

use std::{
    fs,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::AgentResourcesConfig;

const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const ENV: &str = "/usr/bin/env";
#[cfg(all(test, target_os = "linux"))]
const SYSTEMCTL: &str = "/usr/bin/systemctl";
static SCOPE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_POLL: Duration = Duration::from_millis(20);
pub(crate) const GIBIBYTE: u64 = 1024 * 1024 * 1024;

/// One already-preflighted policy for one agent process generation.
///
/// Private fields make it impossible for another module to manufacture a
/// configured scope without successfully completing this module's probe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PreparedScope {
    unit: Option<String>,
    memory_max_bytes: Option<u64>,
    user_bus: Option<UserBusEnvironment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UserBusEnvironment {
    runtime_dir: String,
    bus_address: String,
}

impl PreparedScope {
    /// Prefixes an exact agent argv with a foreground systemd scope runner.
    /// No shell fragment or caller-controlled option is accepted here.
    pub(crate) fn wrap(&self, agent_argv: Vec<String>) -> Result<Vec<String>> {
        if agent_argv.is_empty() {
            bail!("cannot launch an empty agent command");
        }
        match (&self.unit, self.memory_max_bytes, &self.user_bus) {
            (Some(unit), Some(memory_max_bytes), Some(user_bus)) => Ok(systemd_run_argv(
                user_bus,
                unit,
                memory_max_bytes,
                agent_argv,
            )),
            (None, None, None) => Ok(agent_argv),
            _ => unreachable!("a prepared scope always has complete metadata"),
        }
    }

    pub(crate) fn metadata(&self) -> Option<(&str, u64)> {
        self.unit.as_deref().zip(self.memory_max_bytes)
    }

    pub(crate) fn memory_max_bytes(&self) -> Option<u64> {
        self.memory_max_bytes
    }
}

/// Preflights the configured cgroup property before tmux is allowed to create
/// or destructively respawn a pane. Disabled configuration performs no systemd
/// lookup, preserving existing behavior on every platform.
pub(crate) fn prepare(
    resources: &AgentResourcesConfig,
    launch_hint: &str,
) -> Result<PreparedScope> {
    prepare_override(resources, None, launch_hint)
}

/// Resolves one request against owner configuration and preflights its exact
/// cap. `None` always selects the configured default. A caller-supplied value
/// can never enable isolation or raise the owner-configured override ceiling.
pub(crate) fn prepare_override(
    resources: &AgentResourcesConfig,
    requested_memory_max_bytes: Option<u64>,
    launch_hint: &str,
) -> Result<PreparedScope> {
    let Some(memory_max_bytes) = resolve_memory_max(resources, requested_memory_max_bytes)? else {
        return Ok(PreparedScope {
            unit: None,
            memory_max_bytes: None,
            user_bus: None,
        });
    };
    if memory_max_bytes == 0 {
        bail!("[agent_resources].memory_max_bytes must be greater than zero");
    }
    if memory_max_bytes == u64::MAX {
        bail!("agent MemoryMax cannot be u64::MAX because systemd treats it as infinity");
    }

    #[cfg(target_os = "linux")]
    {
        validate_host_ceiling(memory_max_bytes)?;
        let user_bus = user_bus_environment()?;
        prepare_linux(memory_max_bytes, launch_hint, user_bus, run_probe)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = launch_hint;
        bail!(
            "[agent_resources].memory_max_bytes requires Linux with a working systemd user manager"
        )
    }
}

/// Applies the owner policy without consulting systemd. Host and inherited
/// cgroup bounds are checked immediately afterwards by `prepare_override`.
pub(crate) fn resolve_memory_max(
    resources: &AgentResourcesConfig,
    requested_memory_max_bytes: Option<u64>,
) -> Result<Option<u64>> {
    let Some(requested) = requested_memory_max_bytes else {
        return Ok(resources.memory_max_bytes);
    };
    if requested == 0 {
        bail!("requested agent MemoryMax must be greater than zero");
    }
    if requested == u64::MAX {
        bail!("requested agent MemoryMax cannot be infinity");
    }
    let Some(default) = resources.memory_max_bytes else {
        bail!("this machine does not enable per-agent memory isolation");
    };
    // Preserve a pane launched under the current default without requiring
    // overrides to be enabled. This also lets Duplicate state the observed
    // exact cap rather than depending on browser assumptions.
    if requested == default {
        return Ok(Some(requested));
    }
    let Some(ceiling) = resources.memory_override_max_bytes else {
        bail!("this machine does not allow per-agent memory overrides");
    };
    if requested % GIBIBYTE != 0 {
        bail!("agent memory overrides must be a whole number of GiB");
    }
    if requested > ceiling {
        bail!(
            "requested agent MemoryMax={requested} exceeds the configured override ceiling of {ceiling} bytes"
        );
    }
    Ok(Some(requested))
}

#[cfg(target_os = "linux")]
fn user_bus_environment() -> Result<UserBusEnvironment> {
    let euid = rustix::process::geteuid().as_raw();
    let runtime_dir = PathBuf::from(format!("/run/user/{euid}"));
    let runtime_metadata = fs::symlink_metadata(&runtime_dir).with_context(|| {
        format!(
            "systemd user runtime {} is unavailable",
            runtime_dir.display()
        )
    })?;
    if runtime_metadata.file_type().is_symlink()
        || !runtime_metadata.is_dir()
        || runtime_metadata.uid() != euid
        || runtime_metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "systemd user runtime {} is not a private owner directory",
            runtime_dir.display()
        );
    }
    let bus = runtime_dir.join("bus");
    let bus_metadata = fs::symlink_metadata(&bus)
        .with_context(|| format!("systemd user bus {} is unavailable", bus.display()))?;
    if bus_metadata.file_type().is_symlink()
        || !bus_metadata.file_type().is_socket()
        || bus_metadata.uid() != euid
    {
        bail!("systemd user bus {} is not an owner socket", bus.display());
    }
    let runtime_dir = runtime_dir
        .to_str()
        .context("systemd user runtime path is not UTF-8")?
        .to_owned();
    Ok(UserBusEnvironment {
        bus_address: format!("unix:path={runtime_dir}/bus"),
        runtime_dir,
    })
}

#[cfg(target_os = "linux")]
fn validate_host_ceiling(memory_max_bytes: u64) -> Result<()> {
    let effective_ceiling = effective_host_ceiling()?;
    validate_effective_ceiling(memory_max_bytes, effective_ceiling)
}

#[cfg(target_os = "linux")]
fn effective_host_ceiling() -> Result<u64> {
    let host_total = linux_host_total_bytes()?;
    let inherited = inherited_cgroup_memory_max()?;
    Ok(inherited.map_or(host_total, |limit| limit.min(host_total)))
}

/// Largest whole-GiB request the owner can truthfully advertise right now.
/// Launch still repeats this check because cgroup ancestry can change between
/// option discovery and process creation.
pub(crate) fn advertised_override_ceiling(resources: &AgentResourcesConfig) -> Result<Option<u64>> {
    let Some(configured) = resources.memory_override_max_bytes else {
        return Ok(None);
    };
    #[cfg(target_os = "linux")]
    {
        let effective = effective_host_ceiling()?;
        Ok(clamp_advertised_override_ceiling(configured, effective))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = configured;
        bail!("per-agent memory overrides require Linux")
    }
}

fn clamp_advertised_override_ceiling(configured: u64, effective_host_ceiling: u64) -> Option<u64> {
    let strict_whole_gib = effective_host_ceiling
        .saturating_sub(1)
        .checked_div(GIBIBYTE)
        .unwrap_or(0)
        .saturating_mul(GIBIBYTE);
    (strict_whole_gib > 0).then_some(configured.min(strict_whole_gib))
}

fn validate_effective_ceiling(memory_max_bytes: u64, effective_ceiling: u64) -> Result<()> {
    if effective_ceiling == 0 {
        bail!("the effective host/cgroup memory ceiling is zero");
    }
    if memory_max_bytes >= effective_ceiling {
        bail!(
            "agent MemoryMax={memory_max_bytes} must be below the effective host/cgroup memory ceiling of {effective_ceiling} bytes"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_host_total_bytes() -> Result<u64> {
    let source = fs::read_to_string("/proc/meminfo")
        .context("could not read /proc/meminfo for agent MemoryMax validation")?;
    linux_host_total_bytes_from(&source)
}

fn linux_host_total_bytes_from(source: &str) -> Result<u64> {
    let kibibytes = source
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .context("/proc/meminfo has no valid MemTotal")?;
    kibibytes
        .checked_mul(1024)
        .context("host MemTotal overflows bytes")
}

#[cfg(target_os = "linux")]
fn inherited_cgroup_memory_max() -> Result<Option<u64>> {
    let source = fs::read_to_string("/proc/self/cgroup")
        .context("could not read /proc/self/cgroup for agent MemoryMax validation")?;
    inherited_cgroup_memory_max_from(&source, Path::new("/sys/fs/cgroup"))
}

fn inherited_cgroup_memory_max_from(source: &str, root: &Path) -> Result<Option<u64>> {
    let relative = source
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|path| path.starts_with('/'))
        .context("the process is not in a cgroup v2 hierarchy")?;
    if Path::new(relative.trim_start_matches('/'))
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("the process cgroup v2 path is unsafe");
    }
    let mut current = root.join(relative.trim_start_matches('/'));
    let mut inherited: Option<u64> = None;
    loop {
        let memory_max = current.join("memory.max");
        match fs::read_to_string(&memory_max) {
            Ok(value) => {
                let value = value.trim();
                if value != "max" {
                    let parsed = value.parse::<u64>().with_context(|| {
                        format!(
                            "{} contains an invalid memory ceiling",
                            memory_max.display()
                        )
                    })?;
                    inherited = Some(inherited.map_or(parsed, |prior| prior.min(parsed)));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not read inherited limit {}", memory_max.display())
                });
            }
        }
        if current == root {
            break;
        }
        current = current
            .parent()
            .map(Path::to_path_buf)
            .filter(|parent| parent.starts_with(root))
            .unwrap_or_else(|| PathBuf::from(root));
    }
    Ok(inherited)
}

#[cfg(target_os = "linux")]
fn prepare_linux(
    memory_max_bytes: u64,
    launch_hint: &str,
    user_bus: UserBusEnvironment,
    probe: impl FnOnce(&[String]) -> Result<()>,
) -> Result<PreparedScope> {
    let unit = unique_scope_name("spawn", launch_hint);
    let probe_unit = unique_scope_name("probe", launch_hint);
    let argv = systemd_run_argv(
        &user_bus,
        &probe_unit,
        memory_max_bytes,
        vec!["/bin/true".to_owned()],
    );
    probe(&argv).with_context(|| {
        format!("agent MemoryMax={memory_max_bytes} preflight failed; no tmux process was launched")
    })?;
    Ok(PreparedScope {
        unit: Some(unit),
        memory_max_bytes: Some(memory_max_bytes),
        user_bus: Some(user_bus),
    })
}

fn systemd_run_argv(
    user_bus: &UserBusEnvironment,
    unit: &str,
    memory_max_bytes: u64,
    command: Vec<String>,
) -> Vec<String> {
    debug_assert!(valid_scope_name(unit));
    let mut argv = vec![
        ENV.to_owned(),
        format!("XDG_RUNTIME_DIR={}", user_bus.runtime_dir),
        format!("DBUS_SESSION_BUS_ADDRESS={}", user_bus.bus_address),
        SYSTEMD_RUN.to_owned(),
        "--user".to_owned(),
        "--scope".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!("--unit={unit}"),
        format!("--property=MemoryMax={memory_max_bytes}"),
    ];
    argv.push("--".to_owned());
    argv.extend(
        command
            .into_iter()
            .map(|argument| escape_dollars(&argument)),
    );
    argv
}

/// systemd expands command arguments on every supported release. Doubling
/// each dollar is its documented literal-dollar escape and works before the
/// newer `--expand-environment=` switch existed.
fn escape_dollars(argument: &str) -> String {
    argument.replace('$', "$$")
}

fn unescape_dollars(argument: &str) -> Option<String> {
    let mut result = String::with_capacity(argument.len());
    let mut characters = argument.chars();
    while let Some(character) = characters.next() {
        if character != '$' {
            result.push(character);
            continue;
        }
        if characters.next() != Some('$') {
            return None;
        }
        result.push('$');
    }
    Some(result)
}

#[cfg(target_os = "linux")]
fn run_probe(argv: &[String]) -> Result<()> {
    run_probe_with_timeout(argv, PROBE_TIMEOUT)
}

#[cfg(target_os = "linux")]
fn run_probe_with_timeout(argv: &[String], timeout: Duration) -> Result<()> {
    let (program, command_args) = argv
        .split_first()
        .context("systemd MemoryMax probe has no program")?;
    let mut child = Command::new(program)
        .args(command_args)
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "{SYSTEMD_RUN} is unavailable; configured agent memory isolation is fail-closed"
            )
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .context("could not observe the systemd user-scope probe")?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "systemd user-scope probe exceeded its {} second deadline",
                timeout.as_secs_f64()
            );
        }
        thread::sleep(PROBE_POLL);
    }
    let output = child
        .wait_with_output()
        .context("could not collect the systemd user-scope probe")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim().chars().take(400).collect::<String>();
        if detail.is_empty() {
            bail!("systemd user-scope probe exited with {}", output.status);
        }
        bail!("systemd user-scope probe failed: {detail}");
    }
    Ok(())
}

fn unique_scope_name(kind: &str, launch_hint: &str) -> String {
    let sequence = SCOPE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let digest = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}:{now}:{sequence}:{kind}:{launch_hint}",
                std::process::id()
            )
            .as_bytes()
        )
    );
    format!(
        "atmux-tmux-{kind}-{}-{sequence}-{}.scope",
        std::process::id(),
        &digest[..16]
    )
}

pub(crate) fn valid_scope_name(value: &str) -> bool {
    value.starts_with("atmux-tmux-")
        && value.strip_suffix(".scope").is_some()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
}

/// Returns the exact agent argv only for wrappers emitted by this module.
pub(crate) fn agent_argv(argv: &[String]) -> Option<Vec<String>> {
    let runtime_dir = argv.get(1)?.strip_prefix("XDG_RUNTIME_DIR=")?;
    let bus_address = argv.get(2)?.strip_prefix("DBUS_SESSION_BUS_ADDRESS=")?;
    if argv.first().map(String::as_str) != Some(ENV)
        || !runtime_dir
            .strip_prefix("/run/user/")
            .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|byte| byte.is_ascii_digit()))
        || bus_address != format!("unix:path={runtime_dir}/bus")
        || argv.get(3).map(String::as_str) != Some(SYSTEMD_RUN)
        || !argv.iter().any(|arg| arg == "--user")
        || !argv.iter().any(|arg| arg == "--scope")
        || !argv
            .iter()
            .any(|arg| arg.strip_prefix("--unit=").is_some_and(valid_scope_name))
        || !argv.iter().any(|arg| {
            arg.strip_prefix("--property=MemoryMax=")
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|value| value > 0)
        })
    {
        return None;
    }
    let delimiter = argv.iter().position(|arg| arg == "--")?;
    (delimiter + 1 < argv.len()).then(|| {
        argv[delimiter + 1..]
            .iter()
            .map(|arg| unescape_dollars(arg))
            .collect()
    })?
}

#[cfg(test)]
pub(crate) fn fixture_scope(unit: &str, memory_max_bytes: u64) -> PreparedScope {
    assert!(valid_scope_name(unit));
    PreparedScope {
        unit: Some(unit.to_owned()),
        memory_max_bytes: Some(memory_max_bytes),
        user_bus: Some(UserBusEnvironment {
            runtime_dir: "/run/user/1000".to_owned(),
            bus_address: "unix:path=/run/user/1000/bus".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, thread, time::Duration};

    use super::*;

    fn fixture_bus() -> UserBusEnvironment {
        UserBusEnvironment {
            runtime_dir: "/run/user/1000".to_owned(),
            bus_address: "unix:path=/run/user/1000/bus".to_owned(),
        }
    }

    #[test]
    fn disabled_policy_never_calls_the_probe() {
        let resources = AgentResourcesConfig::default();
        assert!(
            prepare(&resources, "anything")
                .unwrap()
                .metadata()
                .is_none()
        );
    }

    #[test]
    fn owner_policy_bounds_every_requested_override() {
        let resources = AgentResourcesConfig {
            memory_max_bytes: Some(16 * GIBIBYTE),
            memory_override_max_bytes: Some(24 * GIBIBYTE),
        };
        assert_eq!(
            resolve_memory_max(&resources, None).unwrap(),
            Some(16 * GIBIBYTE)
        );
        assert_eq!(
            resolve_memory_max(&resources, Some(8 * GIBIBYTE)).unwrap(),
            Some(8 * GIBIBYTE)
        );
        assert_eq!(
            resolve_memory_max(&resources, Some(24 * GIBIBYTE)).unwrap(),
            Some(24 * GIBIBYTE)
        );
        assert_eq!(
            resolve_memory_max(&resources, Some(20 * GIBIBYTE)).unwrap(),
            Some(20 * GIBIBYTE),
            "a non-preset whole-GiB cap remains valid for Duplicate/relaunch"
        );
        for malicious in [0, 8 * GIBIBYTE + 1, 25 * GIBIBYTE, u64::MAX] {
            assert!(resolve_memory_max(&resources, Some(malicious)).is_err());
        }
        let tightened = AgentResourcesConfig {
            memory_max_bytes: Some(16 * GIBIBYTE),
            memory_override_max_bytes: Some(18 * GIBIBYTE),
        };
        assert!(
            resolve_memory_max(&tightened, Some(20 * GIBIBYTE)).is_err(),
            "a previously observed cap must be revalidated after policy changes"
        );
    }

    #[test]
    fn requests_cannot_enable_or_raise_a_default_only_policy() {
        let disabled = AgentResourcesConfig::default();
        assert!(resolve_memory_max(&disabled, Some(GIBIBYTE)).is_err());
        let default_only = AgentResourcesConfig {
            memory_max_bytes: Some(16 * GIBIBYTE),
            memory_override_max_bytes: None,
        };
        assert_eq!(
            resolve_memory_max(&default_only, Some(16 * GIBIBYTE)).unwrap(),
            Some(16 * GIBIBYTE)
        );
        assert!(resolve_memory_max(&default_only, Some(8 * GIBIBYTE)).is_err());
        assert!(resolve_memory_max(&default_only, Some(24 * GIBIBYTE)).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fake_probe_receives_fixed_exact_argv_and_builds_safe_unique_scope() {
        let captured = Mutex::new(Vec::new());
        let plan = prepare_linux(
            34_359_738_368,
            "$(touch /tmp/not-run); weird",
            fixture_bus(),
            |argv| {
                *captured.lock().unwrap() = argv.to_vec();
                Ok(())
            },
        )
        .unwrap();
        let probe = captured.into_inner().unwrap();

        assert_eq!(probe[0], ENV);
        assert_eq!(probe[1], "XDG_RUNTIME_DIR=/run/user/1000");
        assert_eq!(
            probe[2],
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus"
        );
        assert_eq!(probe[3], SYSTEMD_RUN);
        assert_eq!(&probe[4..8], ["--user", "--scope", "--quiet", "--collect"]);
        assert!(probe[8].starts_with("--unit=atmux-tmux-probe-"));
        assert_eq!(probe[9], "--property=MemoryMax=34359738368");
        assert_eq!(&probe[10..], ["--", "/bin/true"]);
        let (unit, memory_max_bytes) = plan.metadata().unwrap();
        assert!(valid_scope_name(unit));
        assert!(!unit.contains("touch"));
        assert_eq!(memory_max_bytes, 34_359_738_368);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn configured_probe_failure_is_fail_closed() {
        let error = prepare_linux(1024, "worker", fixture_bus(), |_| bail!("no user bus"))
            .expect_err("the agent must not receive a scope plan");
        let message = format!("{error:#}");
        assert!(message.contains("MemoryMax=1024 preflight failed"));
        assert!(message.contains("no tmux process was launched"));
        assert!(message.contains("no user bus"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn one_preflighted_scope_never_runs_a_second_probe_when_wrapped() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let scope = prepare_linux(1024, "once", fixture_bus(), |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        scope.wrap(vec!["/bin/true".to_owned()]).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_command_has_a_hard_deadline() {
        let error = run_probe_with_timeout(
            &["/bin/sleep".to_owned(), "10".to_owned()],
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(error.to_string().contains("deadline"));
    }

    #[test]
    fn wrapper_round_trips_arbitrary_agent_arguments_without_interpolation() {
        let plan = fixture_scope("atmux-tmux-spawn-1-2-0123456789abcdef.scope", 4096);
        let agent = vec![
            "env".to_owned(),
            "PROMPT=$(touch /tmp/never)".to_owned(),
            "/tmp/agent with spaces".to_owned(),
            "semi;colon".to_owned(),
            "$HOME".to_owned(),
        ];
        let wrapped = plan.wrap(agent.clone()).unwrap();
        assert_eq!(
            &wrapped[wrapped.len() - agent.len()..],
            [
                "env",
                "PROMPT=$$(touch /tmp/never)",
                "/tmp/agent with spaces",
                "semi;colon",
                "$$HOME",
            ]
        );
        assert_eq!(
            shell_words::split(&shell_words::join(&wrapped)).unwrap(),
            wrapped
        );
        assert_eq!(agent_argv(&wrapped), Some(agent));
    }

    #[test]
    fn dollar_escaping_is_reversible_for_every_literal_shape() {
        for literal in [
            "$FOO",
            "${FOO}",
            "$$",
            "$$$FOO",
            "'single' \"double\"",
            "space $FOO and ${BAR} with $$",
        ] {
            let escaped = escape_dollars(literal);
            assert_eq!(unescape_dollars(&escaped).as_deref(), Some(literal));
        }
        assert!(unescape_dollars("$FOO").is_none());
    }

    #[test]
    fn effective_ceiling_rejects_non_caps_and_infinity() {
        assert!(validate_effective_ceiling(1023, 1024).is_ok());
        assert!(validate_effective_ceiling(1024, 1024).is_err());
        assert!(validate_effective_ceiling(2048, 1024).is_err());
        assert!(validate_effective_ceiling(1, 0).is_err());
        assert!(validate_effective_ceiling(u64::MAX, u64::MAX).is_err());
    }

    #[test]
    fn advertised_override_ceiling_is_strict_whole_gib_and_host_clamped() {
        assert_eq!(
            clamp_advertised_override_ceiling(24 * GIBIBYTE, 10 * GIBIBYTE),
            Some(9 * GIBIBYTE)
        );
        assert_eq!(
            clamp_advertised_override_ceiling(8 * GIBIBYTE, 10 * GIBIBYTE),
            Some(8 * GIBIBYTE)
        );
        assert_eq!(
            clamp_advertised_override_ceiling(24 * GIBIBYTE, 10 * GIBIBYTE + 1),
            Some(10 * GIBIBYTE)
        );
        assert_eq!(
            clamp_advertised_override_ceiling(24 * GIBIBYTE, GIBIBYTE),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_ceiling_sources_use_memtotal_and_the_smallest_cgroup_ancestor() {
        assert_eq!(
            linux_host_total_bytes_from("MemFree: 9 kB\nMemTotal: 65536 kB\n").unwrap(),
            64 * 1024 * 1024
        );
        assert!(linux_host_total_bytes_from("MemFree: 9 kB\n").is_err());

        let nonce = unique_scope_name("ceiling", "fixture");
        let root = std::env::temp_dir().join(nonce);
        let leaf = root.join("user.slice/fixture.scope");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(root.join("memory.max"), "max\n").unwrap();
        fs::write(root.join("user.slice/memory.max"), "67108864\n").unwrap();
        fs::write(leaf.join("memory.max"), "134217728\n").unwrap();
        assert_eq!(
            inherited_cgroup_memory_max_from("0::/user.slice/fixture.scope\n", &root).unwrap(),
            Some(67_108_864)
        );
        assert!(inherited_cgroup_memory_max_from("0::/../outside\n", &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_unit_names_are_unique_and_restricted() {
        let first = unique_scope_name("spawn", "same");
        let second = unique_scope_name("spawn", "same");
        assert_ne!(first, second);
        assert!(valid_scope_name(&first));
        assert!(valid_scope_name(&second));
    }

    /// Manual host probe: creates only a uniquely named user scope, verifies
    /// its `MemoryMax`, then stops that exact scope. It skips hosts without an
    /// available user manager and never touches the default tmux server.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an active systemd user manager"]
    fn real_bounded_systemd_user_scope_probe() {
        let user_bus = user_bus_environment().unwrap();
        let xdg = format!("XDG_RUNTIME_DIR={}", user_bus.runtime_dir);
        let dbus = format!("DBUS_SESSION_BUS_ADDRESS={}", user_bus.bus_address);
        assert!(
            Command::new(ENV)
                .args([&xdg, &dbus, SYSTEMCTL, "--user", "show-environment"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        );

        let literal_plan = prepare_linux(
            64 * 1024 * 1024,
            "real-dollar-test",
            user_bus.clone(),
            run_probe,
        )
        .unwrap();
        let literal_argv = literal_plan
            .wrap(vec![
                "/usr/bin/printf".to_owned(),
                "<%s>\n".to_owned(),
                "$FOO".to_owned(),
                "${FOO}".to_owned(),
                "$$".to_owned(),
                "'single' \"double\"".to_owned(),
            ])
            .unwrap();
        let (literal_program, literal_args) = literal_argv.split_first().unwrap();
        let literal_output = Command::new(literal_program)
            .args(literal_args)
            .output()
            .unwrap();
        assert!(literal_output.status.success());
        assert_eq!(
            String::from_utf8(literal_output.stdout).unwrap(),
            "<$FOO>\n<${FOO}>\n<$$>\n<'single' \"double\">\n"
        );

        let plan = prepare_linux(64 * 1024 * 1024, "real-limit-test", user_bus, run_probe).unwrap();
        let (unit, _) = plan.metadata().unwrap();
        let argv = plan
            .wrap(vec!["/bin/sleep".to_owned(), "10".to_owned()])
            .unwrap();
        let (program, command_args) = argv.split_first().unwrap();
        let mut child = Command::new(program)
            .args(command_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let mut observed = None;
        for _ in 0..40 {
            let output = Command::new(ENV)
                .args([
                    &xdg,
                    &dbus,
                    SYSTEMCTL,
                    "--user",
                    "show",
                    unit,
                    "--property=LoadState",
                    "--property=MemoryMax",
                ])
                .output()
                .unwrap();
            if output.status.success() {
                let properties = String::from_utf8_lossy(&output.stdout);
                if properties.lines().any(|line| line == "LoadState=loaded")
                    && let Some(memory) = properties
                        .lines()
                        .find_map(|line| line.strip_prefix("MemoryMax="))
                {
                    observed = Some(memory.to_owned());
                    break;
                }
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = Command::new(ENV)
            .args([&xdg, &dbus, SYSTEMCTL, "--user", "stop", unit])
            .status();
        let _ = child.wait();

        assert_eq!(observed.as_deref(), Some("67108864"));
    }
}
