use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub use crate::auto_update::MaintenanceConfig;
use crate::machine::{LOCAL_MACHINE_ID, NodeUrl, validate_machine_id, validate_machine_label};
use crate::project;

pub const DEFAULT_CONFIG: &str = r#"# atmux configuration

[general]
# Roots are searched through grouping and project folders. Git worktrees,
# `.atmux.toml` directories, and folders with AGENTS.md, AGENT.md, CLAUDE.md,
# or GEMINI.md are launchable.
project_roots = ["~/IdeaProjects", "~/work"]
favorite_dirs = []
refresh_ms = 750
preview_lines = 160
switch_on_launch = true

# Profiles are grouped by harness in the launcher. Add as many as you like.
[[profiles]]
name = "Default"
harness = "codex"
command = "codex"
args = []

[profiles.env]
CODEX_HOME = "~/.codex"

[[profiles]]
name = "Default"
harness = "claude"
command = "claude"
args = []

[profiles.env]
CLAUDE_CONFIG_DIR = "~/.claude"

# Example:
# [[profiles]]
# name = "Sol xhigh"
# harness = "codex"
# command = "codex"
# args = ["-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"xhigh\""]
#
# [profiles.env]
# SOME_VARIABLE = "value"

# A profile may expose only the launch modes that its account can use. The
# dashboard and launcher never invent extra model choices. `effort` and
# `service_tier` apply only to Codex; `service_tier = "fast"` is selected when
# a new Codex session launches, not changed in an existing conversation.
# [[profiles.modes]]
# id = "sol-xhigh-fast"
# label = "Sol · xhigh · Fast"
# model = "gpt-5.6-sol"
# effort = "xhigh"
# service_tier = "fast"

[status]
# Matching is case-insensitive. These extend atmux's built-in heuristics.
working_markers = []
waiting_markers = []

# Owner-local, native-log-driven context compaction. atmux never estimates
# tokens from terminal output and never compacts a working or remote pane.
[auto_compact]
enabled = false
inactivity_minutes = 15
input_tokens = 200000
poll_seconds = 30

# Optional per-agent process-tree memory isolation on Linux. When configured,
# every new launch and in-place relaunch must enter its own transient systemd
# user scope with this MemoryMax. Leave unset on macOS and non-systemd hosts.
[agent_resources]
# memory_max_bytes = 34359738368 # 32 GiB
# memory_override_max_bytes = 68719476736 # optional 64 GiB per-launch ceiling

# Owner-local native CLI maintenance. Each owner runs one scheduler; federated
# coordinators never update another machine. Disabled until explicitly enabled.
[maintenance]
enabled = false
interval_minutes = 30
update_timeout_seconds = 180
relaunch_limit = 4

# This machine's federated identity. With no [[machines]] below, atmux emits
# bare tmux pane ids exactly as it always has, so leaving [node] out changes
# nothing. Adding a machine switches emitted ids to "machine~pane"; both forms
# are always accepted as input.
# [node]
# id = "local"
# label = "This machine"
# Run only as a federation/Pulse coordinator. This removes this process's
# machine, tmux sessions, launch inputs, metrics, and owner-local mutations
# from the API. The fail-closed restrictions are documented in the README.
# coordinator_only = false
# Require this bearer token for API and MCP access from both remote and local
# callers. Unauthenticated loopback is a separate, explicit [web] opt-in.
# token_env = "ATMUX_NODE_TOKEN"
# token_file = "~/.config/atmux/node.token"
#
# Every non-loopback listener uses mutual TLS. The certificate must contain
# each LAN IP address at which this machine advertises itself; all peers use
# certificates signed by the same private atmux CA.
# [node.tls]
# cert_file = "~/.config/atmux/tls/node.crt"
# key_file = "~/.config/atmux/tls/node.key"
# ca_file = "~/.config/atmux/tls/ca.crt"

# Discover nearby atmux web nodes with DNS-SD/mDNS. Discovery is opt-in because
# a discovered node can receive the same control commands as a local tmux
# session. Give every participating machine a unique [node] id and configure
# the same bearer token both here and under [node].
# [discovery]
# enabled = true
# token_env = "ATMUX_LAN_TOKEN"
# token_file = "~/.config/atmux/lan.token"

# Optional credential accepted only for a trusted web reverse proxy. Keep this
# distinct from [node]'s LAN federation token.
# [web]
# Local API and MCP access is authenticated by default. Set this only for a
# single-user development machine whose local processes are fully trusted.
# allow_unauthenticated_loopback = true
# proxy_token_env = "ATMUX_WEB_PROXY_TOKEN"
# proxy_token_file = "~/.config/atmux/web-proxy.token"

# Pulse is embedded in `atmux web` but all capabilities are explicit and
# disabled by default. On Midnight, keep launching atmux-web through the
# existing Aqua LaunchAgent so Claude credentials retain Keychain access.
[pulse]
collect = false
serve = false
receive = false

# Gemini quota collection requires explicit external Google OAuth application
# configuration. Export the values into the service environment; never put
# either value in this file.
# [pulse.credentials]
# gemini_oauth_client_id_env = "ATMUX_GEMINI_OAUTH_CLIENT_ID"
# gemini_oauth_client_secret_env = "ATMUX_GEMINI_OAUTH_CLIENT_SECRET"

# Optional push reporting uses two distinct external credentials outside
# loopback: the receiver-issued ingest token and the existing node/proxy token.
# report_to = "https://peer.example.test/api/v1/pulse/ingest"
# report_token_file = "~/.config/atmux/pulse-ingest.token"
# report_node_token_file = "~/.config/atmux/node.token"

# REST and MCP never infer an account from forwarded headers or invent a local
# account. Configure each identity explicitly before enabling Pulse.
# [[pulse.accounts]]
# id = 1
# identity = "operator@example.com"
# display_name = "Operator"
#
# [[pulse.accounts.profiles]]
# name = "claude-max"
# vendor = "anthropic-oauth"
# config_dir = "~/.claude-max"

# Explicitly trusted remote atmux nodes. This coordinator aggregates their live
# state; it never copies tmux processes and browsers never contact a node.
# [[machines]]
# id = "gpu-box"
# label = "GPU box"
# url = "https://gpu-box.tail1234.ts.net:7345"
# token_env = "ATMUX_GPU_BOX_TOKEN"
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub profiles: Vec<AgentProfile>,
    #[serde(default)]
    pub status: StatusConfig,
    #[serde(default)]
    pub auto_compact: AutoCompactConfig,
    #[serde(default)]
    pub agent_resources: AgentResourcesConfig,
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
    #[serde(default)]
    pub node: NodeConfig,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[cfg(feature = "pulse")]
    #[serde(default)]
    pub pulse: crate::pulse::PulseConfig,
    /// Explicitly trusted remote atmux nodes aggregated by this coordinator.
    #[serde(default)]
    pub machines: Vec<MachineConfig>,
}

/// Opt-in LAN discovery for nearby atmux web nodes.
///
/// Discovery transports only a node id, label, and local-network address over
/// DNS-SD. The shared token remains local and is presented only to the
/// discovered node's normal HTTP API.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    pub token_env: Option<String>,
    pub token_file: Option<PathBuf>,
}

/// Credentials for a trusted reverse proxy terminating user authentication.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WebConfig {
    /// Explicit development-mode escape hatch for local browsers and clients.
    ///
    /// This is false by default because API/MCP access is shell-equivalent and
    /// a loopback exemption would otherwise grant every local process control.
    pub allow_unauthenticated_loopback: bool,
    pub proxy_token_env: Option<String>,
    pub proxy_token_file: Option<PathBuf>,
}

/// How this atmux process identifies itself and guards non-loopback access.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct NodeConfig {
    pub id: String,
    pub label: Option<String>,
    /// Disable every owner-local tmux/launch capability while retaining this
    /// process's node identity for federation, Pulse, and authenticated web.
    pub coordinator_only: bool,
    pub token_env: Option<String>,
    pub token_file: Option<PathBuf>,
    pub tls: Option<TlsConfig>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            id: LOCAL_MACHINE_ID.to_owned(),
            label: None,
            coordinator_only: false,
            token_env: None,
            token_file: None,
            tls: None,
        }
    }
}

/// Certificate material used to secure a node's HTTPS listener and its
/// federation connections. Paths are references only; credentials never live
/// in the configuration file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TlsConfig {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    pub ca_file: PathBuf,
}

/// One trusted remote node. Credentials are referenced, never inlined.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MachineConfig {
    pub id: String,
    pub label: Option<String>,
    pub url: String,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG).expect("the embedded default config must be valid")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub project_roots: Vec<PathBuf>,
    pub favorite_dirs: Vec<PathBuf>,
    pub refresh_ms: u64,
    pub preview_lines: usize,
    pub switch_on_launch: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            project_roots: vec![PathBuf::from("~/IdeaProjects")],
            favorite_dirs: Vec::new(),
            refresh_ms: 750,
            preview_lines: 160,
            switch_on_launch: true,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct StatusConfig {
    pub working_markers: Vec<String>,
    pub waiting_markers: Vec<String>,
}

/// Owner-local automatic context compaction policy.
///
/// Token counts come only from a live Claude or Codex native session log that
/// can be tied unambiguously to the pane. Other harnesses fail closed.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AutoCompactConfig {
    pub enabled: bool,
    pub inactivity_minutes: u64,
    pub input_tokens: u64,
    pub poll_seconds: u64,
}

/// Opt-in process-tree resource isolation for locally launched agents.
///
/// A configured memory maximum is fail-closed: atmux will not launch or
/// relaunch an agent unless a systemd user scope accepts `MemoryMax`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentResourcesConfig {
    /// Default cap for launches and recovery paths which do not request an
    /// explicit owner-approved override.
    pub memory_max_bytes: Option<u64>,
    /// Explicit ceiling for per-launch overrides. Overrides remain disabled
    /// when this is absent, even when a default cap is configured.
    pub memory_override_max_bytes: Option<u64>,
}

impl AgentResourcesConfig {
    fn validate(self) -> Result<()> {
        // Validate the policy shape on every platform before reporting whether
        // the platform can enforce it. This keeps unsafe sentinel values and
        // malformed bounds from being hidden by a generic availability error.
        for (key, value) in [
            ("memory_max_bytes", self.memory_max_bytes),
            ("memory_override_max_bytes", self.memory_override_max_bytes),
        ] {
            if value == Some(0) {
                bail!("[agent_resources].{key} must be greater than zero");
            }
            if value == Some(u64::MAX) {
                bail!(
                    "[agent_resources].{key} cannot be u64::MAX because systemd treats it as infinity"
                );
            }
        }
        match (self.memory_max_bytes, self.memory_override_max_bytes) {
            (None, Some(_)) => {
                bail!("[agent_resources].memory_override_max_bytes requires memory_max_bytes")
            }
            (Some(default), Some(ceiling)) if default > ceiling => bail!(
                "[agent_resources].memory_max_bytes must not exceed memory_override_max_bytes"
            ),
            _ => {}
        }
        if self
            .memory_override_max_bytes
            .is_some_and(|ceiling| ceiling % (1024 * 1024 * 1024) != 0)
        {
            bail!("[agent_resources].memory_override_max_bytes must be a whole number of GiB");
        }
        #[cfg(not(target_os = "linux"))]
        if self.memory_max_bytes.is_some() || self.memory_override_max_bytes.is_some() {
            bail!("[agent_resources] memory limits require Linux with systemd and cgroup v2");
        }
        Ok(())
    }
}

impl Default for AutoCompactConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            inactivity_minutes: 15,
            input_tokens: 200_000,
            poll_seconds: 30,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentProfile {
    pub name: String,
    pub harness: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Retain the command, arguments, and environment discovered from an
    /// existing local wrapper/alias while this config supplies its modes.
    #[serde(default)]
    pub inherit_discovered: bool,
    /// Declares which typed boundary supplies Claude's permission flag when
    /// atmux reconstructs a saved conversation. `None` keeps old configs
    /// compatible by keeping atmux-managed injection. An opaque wrapper that
    /// supplies the option itself must opt into launcher ownership explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_relaunch_permissions: Option<ClaudeRelaunchPermissions>,
    /// Explicit model/effort/tier combinations available to this profile.
    /// An empty list deliberately exposes no selectable mode for a profile.
    #[serde(default)]
    pub modes: Vec<ProfileMode>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeRelaunchPermissions {
    AtmuxInjects,
    LauncherProvides,
}

impl AgentProfile {
    #[must_use]
    pub(crate) fn effective_claude_relaunch_permissions(&self) -> ClaudeRelaunchPermissions {
        self.claude_relaunch_permissions
            .unwrap_or(ClaudeRelaunchPermissions::AtmuxInjects)
    }
}

/// One explicit, profile-scoped agent mode.
///
/// The identifier is an opaque configuration key passed through the browser
/// and MCP APIs. Its model, reasoning effort, and service tier are validated
/// data, never shell fragments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileMode {
    pub id: String,
    pub label: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
}

impl ProfileMode {
    #[must_use]
    pub fn display_label(&self) -> String {
        let mut parts = vec![self.label.clone().unwrap_or_else(|| self.model.clone())];
        if let Some(effort) = &self.effort
            && !parts[0].to_ascii_lowercase().contains(effort)
        {
            parts.push(effort.clone());
        }
        if self.service_tier.as_deref() == Some("fast")
            && !parts[0].to_ascii_lowercase().contains("fast")
        {
            parts.push("Fast".to_owned());
        }
        parts.join(" · ")
    }
}

impl Config {
    /// Resolves the platform-specific default configuration path.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system has no usable configuration directory.
    pub fn path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("dev", "ryanmurf", "atmux")
            .context("could not determine the user config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Loads and normalizes a configuration, creating the defaults when absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be created, read, or parsed.
    pub fn load(path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = path.map_or_else(Self::path, |value| Ok(value.to_path_buf()))?;
        if !path.exists() {
            Self::write_default(&path, false)?;
        }
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config: Self = toml::from_str(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.normalize();
        config.validate_coordinator_only().with_context(|| {
            format!(
                "invalid coordinator-only configuration in {}",
                path.display()
            )
        })?;
        config
            .validate_federation()
            .with_context(|| format!("invalid federation configuration in {}", path.display()))?;
        config
            .validate_profiles()
            .with_context(|| format!("invalid profile configuration in {}", path.display()))?;
        config
            .validate_auto_compact()
            .with_context(|| format!("invalid auto-compact configuration in {}", path.display()))?;
        config.agent_resources.validate().with_context(|| {
            format!("invalid agent resource configuration in {}", path.display())
        })?;
        config
            .maintenance
            .validate()
            .with_context(|| format!("invalid maintenance configuration in {}", path.display()))?;
        #[cfg(feature = "pulse")]
        config
            .pulse
            .validate()
            .with_context(|| format!("invalid Pulse configuration in {}", path.display()))?;
        Ok((config, path))
    }

    /// Rejects machine configuration that would produce ambiguous identities or
    /// unsafe outbound requests.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid machine id or label, a duplicate or
    /// reserved id, or a URL that fails [`NodeUrl`] validation.
    pub fn validate_federation(&self) -> Result<()> {
        validate_machine_id(&self.node.id).context("invalid [node] id")?;
        if let Some(label) = &self.node.label {
            validate_machine_label(label).context("invalid [node] label")?;
        }
        let mut seen = BTreeSet::from([self.node.id.clone()]);
        for machine in &self.machines {
            validate_machine_id(&machine.id).context("invalid [[machines]] id")?;
            if let Some(label) = &machine.label {
                validate_machine_label(label)
                    .with_context(|| format!("invalid label for machine {}", machine.id))?;
            }
            if !seen.insert(machine.id.clone()) {
                bail!(
                    "machine id {} is used more than once; ids must be unique and must not collide with [node] id",
                    machine.id
                );
            }
            NodeUrl::parse(&machine.url)
                .with_context(|| format!("invalid url for machine {}", machine.id))?;
        }
        if self.discovery.enabled {
            if self.node.id == LOCAL_MACHINE_ID {
                bail!(
                    "[discovery] requires an explicit [node] id; set a unique lowercase id such as \"workstation\""
                );
            }
            if self.discovery.token_env.is_some() && self.discovery.token_file.is_some() {
                bail!("[discovery] sets both token_env and token_file; choose one");
            }
            if self.discovery.token_env.is_none() && self.discovery.token_file.is_none() {
                bail!(
                    "[discovery] requires token_env or token_file so discovered nodes remain authenticated"
                );
            }
            if self.node.token_env.is_none() && self.node.token_file.is_none() {
                bail!(
                    "[discovery] requires [node].token_env or [node].token_file to protect this node from LAN callers"
                );
            }
            if self.node.tls.is_none() {
                bail!(
                    "[discovery] requires [node.tls] so an unauthenticated multicast record cannot receive a federation credential"
                );
            }
        }
        if !self.machines.is_empty() {
            let urls = self
                .machines
                .iter()
                .map(|machine| NodeUrl::parse(&machine.url).map(|url| (&machine.id, url)))
                .collect::<Result<Vec<_>>>()?;
            if let Some((id, _)) = urls
                .iter()
                .find(|(_, url)| !url.is_https() && !url.is_loopback())
            {
                bail!(
                    "machine {id} uses plaintext HTTP; remote federation requires an https:// URL"
                );
            }
            if urls.iter().any(|(_, url)| url.is_https()) && self.node.tls.is_none() {
                bail!(
                    "[[machines]] with HTTPS requires [node.tls] for a mutual-TLS client identity"
                );
            }
        }
        if self.web.proxy_token_env.is_some() && self.web.proxy_token_file.is_some() {
            bail!("[web] sets both proxy_token_env and proxy_token_file; choose one");
        }
        Ok(())
    }

    /// Ensures a coordinator-only process cannot silently regain any local
    /// owner capability through an otherwise-valid configuration field.
    ///
    /// Remote federation and Pulse serving remain available. Pulse account
    /// names may be configured so pulled rows can be associated with their
    /// identities, but local stores, credentials, collection, receive, and
    /// push reporting remain forbidden.
    ///
    /// # Errors
    ///
    /// Returns an error when `[node].coordinator_only` is enabled alongside an
    /// owner-local profile, root, discovery, scheduler, or Pulse capability.
    pub fn validate_coordinator_only(&self) -> Result<()> {
        if !self.node.coordinator_only {
            return Ok(());
        }
        if !self.profiles.is_empty() {
            bail!("[node].coordinator_only requires profiles = []");
        }
        if !self.general.project_roots.is_empty() || !self.general.favorite_dirs.is_empty() {
            bail!(
                "[node].coordinator_only requires empty [general].project_roots and favorite_dirs"
            );
        }
        if self.general.switch_on_launch {
            bail!("[node].coordinator_only requires [general].switch_on_launch = false");
        }
        if self.discovery.enabled {
            bail!("[node].coordinator_only requires [discovery].enabled = false");
        }
        if self.auto_compact.enabled {
            bail!("[node].coordinator_only requires [auto_compact].enabled = false");
        }
        if self.agent_resources.memory_max_bytes.is_some() {
            bail!("[node].coordinator_only forbids [agent_resources].memory_max_bytes");
        }
        if self.agent_resources.memory_override_max_bytes.is_some() {
            bail!("[node].coordinator_only forbids [agent_resources].memory_override_max_bytes");
        }
        if self.maintenance.enabled {
            bail!("[node].coordinator_only requires [maintenance].enabled = false");
        }
        #[cfg(feature = "pulse")]
        {
            if self.pulse.collect {
                bail!("[node].coordinator_only requires [pulse].collect = false");
            }
            if self.pulse.receive {
                bail!("[node].coordinator_only requires [pulse].receive = false");
            }
            if self.pulse.report_to.is_some()
                || self.pulse.report_token_env.is_some()
                || self.pulse.report_token_file.is_some()
                || self.pulse.report_node_token_env.is_some()
                || self.pulse.report_node_token_file.is_some()
            {
                bail!("[node].coordinator_only forbids Pulse push reporting");
            }
            if self
                .pulse
                .accounts
                .iter()
                .flat_map(|account| &account.profiles)
                .any(|profile| {
                    profile.config_dir.is_some()
                        || profile.api_key_env.is_some()
                        || profile.api_key_file.is_some()
                })
                || self.pulse.credentials.gemini_oauth_client_id_env.is_some()
                || self
                    .pulse
                    .credentials
                    .gemini_oauth_client_secret_env
                    .is_some()
            {
                bail!("[node].coordinator_only forbids owner-local Pulse credential references");
            }
        }
        Ok(())
    }

    /// Validates profile-scoped launch modes before they can reach a tmux
    /// command or a browser/API response.
    ///
    /// # Errors
    ///
    /// Returns an error when a profile name, mode id, model, effort, or tier
    /// is malformed or when a mode requests a harness capability it does not
    /// support.
    pub fn validate_profiles(&self) -> Result<()> {
        for profile in &self.profiles {
            if profile.name.trim().is_empty()
                || profile.name.len() > 120
                || profile.name.chars().any(char::is_control)
            {
                bail!("profile name {:?} must be readable text", profile.name);
            }
            let harness = profile.harness.to_ascii_lowercase();
            if profile.claude_relaunch_permissions.is_some() && harness != "claude" {
                bail!(
                    "profile {} sets claude_relaunch_permissions but is not a Claude profile",
                    profile.name
                );
            }
            if !profile.modes.is_empty() && !matches!(harness.as_str(), "claude" | "codex") {
                bail!(
                    "profile {} uses modes but has unsupported harness {:?}",
                    profile.name,
                    profile.harness
                );
            }
            let mut ids = BTreeSet::new();
            for mode in &profile.modes {
                if !valid_profile_name(&mode.id) {
                    bail!("profile {} has invalid mode id {:?}", profile.name, mode.id);
                }
                if !ids.insert(mode.id.to_ascii_lowercase()) {
                    bail!(
                        "profile {} defines mode {} more than once",
                        profile.name,
                        mode.id
                    );
                }
                if !crate::tmux::valid_model_id(&mode.model) {
                    bail!(
                        "profile {} mode {} has an invalid model id",
                        profile.name,
                        mode.id
                    );
                }
                if mode.label.as_ref().is_some_and(|label| {
                    label.is_empty() || label.len() > 120 || label.chars().any(char::is_control)
                }) {
                    bail!(
                        "profile {} mode {} has an invalid label",
                        profile.name,
                        mode.id
                    );
                }
                if let Some(effort) = &mode.effort {
                    if harness == "claude" {
                        if !matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
                            bail!(
                                "profile {} mode {} has unsupported Claude effort {:?}",
                                profile.name,
                                mode.id,
                                effort
                            );
                        }
                    } else if harness != "codex" {
                        bail!(
                            "profile {} mode {} sets effort but has unsupported harness {:?}",
                            profile.name,
                            mode.id,
                            profile.harness
                        );
                    } else if !matches!(
                        effort.as_str(),
                        "none" | "low" | "medium" | "high" | "xhigh" | "max"
                    ) {
                        bail!(
                            "profile {} mode {} has unsupported Codex effort {:?}",
                            profile.name,
                            mode.id,
                            effort
                        );
                    }
                }
                if let Some(tier) = &mode.service_tier {
                    if harness != "codex" {
                        bail!(
                            "profile {} mode {} sets a service tier but is not a Codex profile",
                            profile.name,
                            mode.id
                        );
                    }
                    if tier != "fast" {
                        bail!(
                            "profile {} mode {} has unsupported service tier {:?}",
                            profile.name,
                            mode.id,
                            tier
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Validates bounded scheduler settings before any background task starts.
    ///
    /// # Errors
    ///
    /// Returns an error for zero or excessive thresholds/cadences.
    pub fn validate_auto_compact(&self) -> Result<()> {
        let policy = &self.auto_compact;
        if !(1..=7 * 24 * 60).contains(&policy.inactivity_minutes) {
            bail!("[auto_compact].inactivity_minutes must be between 1 and 10080");
        }
        if !(1..=10_000_000).contains(&policy.input_tokens) {
            bail!("[auto_compact].input_tokens must be between 1 and 10000000");
        }
        if !(5..=60 * 60).contains(&policy.poll_seconds) {
            bail!("[auto_compact].poll_seconds must be between 5 and 3600");
        }
        Ok(())
    }

    /// The label shown for this machine in federated views.
    #[must_use]
    pub fn node_label(&self) -> String {
        self.node.label.clone().unwrap_or_else(|| {
            if self.node.id == LOCAL_MACHINE_ID {
                "This machine".to_owned()
            } else {
                self.node.id.clone()
            }
        })
    }

    /// Writes the embedded default configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory or file cannot be written.
    pub fn write_default(path: &Path, force: bool) -> Result<()> {
        if path.exists() && !force {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, DEFAULT_CONFIG)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    fn normalize(&mut self) {
        self.general.refresh_ms = self.general.refresh_ms.clamp(100, 10_000);
        self.general.preview_lines = self.general.preview_lines.clamp(20, 2_000);
        for path in &mut self.general.project_roots {
            *path = expand_tilde(path);
        }
        for path in &mut self.general.favorite_dirs {
            *path = expand_tilde(path);
        }
        for profile in &mut self.profiles {
            let command = expand_tilde(Path::new(&profile.command));
            profile.command = command.to_string_lossy().into_owned();
            normalize_profile_environment(&mut profile.env);
        }
        if let Some(path) = &mut self.node.token_file {
            *path = expand_tilde(path);
        }
        if let Some(tls) = &mut self.node.tls {
            tls.cert_file = expand_tilde(&tls.cert_file);
            tls.key_file = expand_tilde(&tls.key_file);
            tls.ca_file = expand_tilde(&tls.ca_file);
        }
        if let Some(path) = &mut self.discovery.token_file {
            *path = expand_tilde(path);
        }
        if let Some(path) = &mut self.web.proxy_token_file {
            *path = expand_tilde(path);
        }
        #[cfg(feature = "pulse")]
        {
            if let Some(path) = &mut self.pulse.database.sqlite_path {
                *path = expand_tilde(path);
            }
            if let Some(path) = &mut self.pulse.report_token_file {
                *path = expand_tilde(path);
            }
            if let Some(path) = &mut self.pulse.report_node_token_file {
                *path = expand_tilde(path);
            }
            for account in &mut self.pulse.accounts {
                for profile in &mut account.profiles {
                    if let Some(path) = &mut profile.config_dir {
                        *path = expand_tilde(path);
                    }
                    if let Some(path) = &mut profile.api_key_file {
                        *path = expand_tilde(path);
                    }
                }
            }
        }
        for machine in &mut self.machines {
            machine.id = machine.id.trim().to_owned();
            machine.url = machine.url.trim().to_owned();
            if let Some(path) = &mut machine.token_file {
                *path = expand_tilde(path);
            }
        }
        // A coordinator must not infer local launch capability merely because
        // an agent executable happens to be present in its container image.
        if !self.node.coordinator_only {
            self.discover_profiles();
        }
    }

    fn discover_profiles(&mut self) {
        let home = env::var_os("HOME").map(PathBuf::from);
        let codex = find_program(
            "codex",
            &home.as_deref().map_or_else(Vec::new, codex_candidates),
        );
        let claude = current_claude_program();
        self.discover_profiles_from(home.as_deref(), codex.as_deref(), claude.as_deref());
    }

    fn discover_profiles_from(
        &mut self,
        home: Option<&Path>,
        codex: Option<&Path>,
        claude: Option<&Path>,
    ) {
        let configured_profiles = self.profiles.len();
        let mut seen: BTreeSet<(String, String)> = self
            .profiles
            .iter()
            .map(|profile| (profile.harness.to_lowercase(), profile.name.to_lowercase()))
            .collect();
        // Keep the first source merged into configured profiles because
        // discovery is ordered from precise executables to shell aliases.
        let mut inherited = BTreeSet::new();

        for profile in &mut self.profiles {
            resolve_configured_default_command(profile, codex, claude);
        }

        if let Some(home) = home {
            discover_executable_profiles(
                home,
                "codex",
                &mut self.profiles,
                &mut seen,
                &mut inherited,
            );
            discover_executable_profiles(
                home,
                "claude",
                &mut self.profiles,
                &mut seen,
                &mut inherited,
            );
            discover_shell_alias_profiles(
                home,
                "codex",
                codex,
                &mut self.profiles,
                &mut seen,
                &mut inherited,
            );
            discover_shell_alias_profiles(
                home,
                "claude",
                claude,
                &mut self.profiles,
                &mut seen,
                &mut inherited,
            );
            discover_codex_config_profiles(
                home,
                codex,
                &mut self.profiles,
                &mut seen,
                &mut inherited,
            );
        }

        add_discovered_profile(
            &mut self.profiles,
            &mut seen,
            &mut inherited,
            discovered_profile("codex", "Default", codex, Vec::new(), None),
        );
        add_discovered_profile(
            &mut self.profiles,
            &mut seen,
            &mut inherited,
            discovered_profile("claude", "Default", claude, Vec::new(), None),
        );

        self.profiles[configured_profiles..].sort_by_key(|profile| {
            (
                profile.harness.to_lowercase(),
                profile.name != "Default",
                profile.name.to_lowercase(),
            )
        });
    }

    #[must_use]
    pub fn harnesses(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        self.profiles
            .iter()
            .map(|profile| profile.harness.to_lowercase())
            .filter(|harness| seen.insert(harness.clone()))
            .collect()
    }

    #[must_use]
    pub fn profiles_for(&self, harness: &str) -> Vec<AgentProfile> {
        self.profiles
            .iter()
            .filter(|profile| profile.harness.eq_ignore_ascii_case(harness))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn directories(&self) -> Vec<PathBuf> {
        let mut paths = BTreeSet::new();
        for favorite in &self.general.favorite_dirs {
            discover_projects(favorite, &mut paths);
        }
        for root in &self.general.project_roots {
            discover_projects(root, &mut paths);
        }
        paths.into_iter().collect()
    }

    /// Canonical roots within which callers may browse and launch agents.
    ///
    /// Missing roots are omitted. Returning canonical paths keeps the browser
    /// and the final launch validation on the same symlink-safe boundary.
    #[must_use]
    pub fn launch_roots(&self) -> Vec<PathBuf> {
        self.general
            .project_roots
            .iter()
            .chain(&self.general.favorite_dirs)
            .filter_map(|root| root.canonicalize().ok())
            .filter(|root| root.is_dir())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Resolves a typed launch directory while keeping manual entry inside the
    /// configured project/favorite roots.
    ///
    /// Canonicalizing both sides means a symlink below an allowed root cannot
    /// be used to launch an agent in an unrelated part of the filesystem.
    #[must_use]
    pub fn resolve_launch_directory(&self, requested: &Path) -> Option<PathBuf> {
        let requested = expand_tilde(requested);
        if !requested.is_absolute() {
            return None;
        }
        let requested = requested.canonicalize().ok()?;
        if !requested.is_dir() {
            return None;
        }
        self.general
            .project_roots
            .iter()
            .chain(&self.general.favorite_dirs)
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| requested == root || requested.starts_with(&root))
            .then_some(requested)
    }
}

/// A generated/default config intentionally uses portable bare commands. Once
/// discovery has found the host's actual CLI, retain all configured arguments
/// and environment while making that command independent of the web service's
/// inherited `PATH` (which is commonly minimal under launchd/systemd/tmux).
fn resolve_configured_default_command(
    profile: &mut AgentProfile,
    codex: Option<&Path>,
    claude: Option<&Path>,
) {
    let resolved = match (
        profile.harness.to_ascii_lowercase().as_str(),
        profile.command.as_str(),
    ) {
        ("codex", "codex") => codex,
        ("claude", "claude") => claude,
        _ => None,
    };
    if let Some(command) = resolved {
        profile.command = command.to_string_lossy().into_owned();
    }
}

fn add_discovered_profile(
    profiles: &mut Vec<AgentProfile>,
    seen: &mut BTreeSet<(String, String)>,
    inherited: &mut BTreeSet<(String, String)>,
    discovered: Option<AgentProfile>,
) {
    if let Some(discovered) = discovered {
        merge_or_add_discovered_profile(profiles, seen, inherited, discovered);
    }
}

fn discovered_profile(
    harness: &str,
    name: &str,
    command: Option<&Path>,
    args: Vec<String>,
    claude_relaunch_permissions: Option<ClaudeRelaunchPermissions>,
) -> Option<AgentProfile> {
    Some(AgentProfile {
        name: name.to_owned(),
        harness: harness.to_owned(),
        command: command?.to_string_lossy().into_owned(),
        args,
        env: BTreeMap::new(),
        inherit_discovered: false,
        claude_relaunch_permissions,
        modes: Vec::new(),
    })
}

/// A profile that explicitly opts into discovery keeps its configured model
/// allowlist and environment overrides while inheriting the local wrapper's
/// command, arguments, and other credential-bound environment. Explicit store
/// bindings remain authoritative without copying provider credentials here.
fn merge_or_add_discovered_profile(
    profiles: &mut Vec<AgentProfile>,
    seen: &mut BTreeSet<(String, String)>,
    inherited: &mut BTreeSet<(String, String)>,
    discovered: AgentProfile,
) {
    let key = (
        discovered.harness.to_ascii_lowercase(),
        discovered.name.to_ascii_lowercase(),
    );
    if let Some(configured) = profiles.iter_mut().find(|profile| {
        profile.inherit_discovered
            && profile.harness.eq_ignore_ascii_case(&discovered.harness)
            && profile.name.eq_ignore_ascii_case(&discovered.name)
    }) {
        if !inherited.insert(key) {
            return;
        }
        let configured_env = std::mem::take(&mut configured.env);
        configured.command = discovered.command;
        configured.args = discovered.args;
        configured.env = discovered.env;
        configured.env.extend(configured_env);
        if configured.claude_relaunch_permissions.is_none() {
            configured.claude_relaunch_permissions = discovered.claude_relaunch_permissions;
        }
        return;
    }
    if seen.insert(key) {
        profiles.push(discovered);
    }
}

fn discover_codex_config_profiles(
    home: &Path,
    command: Option<&Path>,
    profiles: &mut Vec<AgentProfile>,
    seen: &mut BTreeSet<(String, String)>,
    inherited: &mut BTreeSet<(String, String)>,
) {
    let Ok(entries) = fs::read_dir(home.join(".codex")) else {
        return;
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(name) = file_name.strip_suffix(".config.toml") else {
            continue;
        };
        add_discovered_profile(
            profiles,
            seen,
            inherited,
            discovered_profile(
                "codex",
                name,
                command,
                vec!["--profile".to_owned(), name.to_owned()],
                None,
            ),
        );
    }
}

/// Finds executable wrapper profiles such as `claude-max` and `codex-work`.
///
/// Wrapper files take precedence over shell aliases of the same name because
/// they can be run directly without evaluating a user's shell configuration.
/// Discovery cannot inspect an opaque executable's internal permission or `--`
/// forwarding policy. It therefore keeps the backward-compatible
/// `atmux_injects` default; a configured profile must explicitly declare
/// `launcher_provides` when its wrapper supplies the option itself.
fn discover_executable_profiles(
    home: &Path,
    harness: &str,
    profiles: &mut Vec<AgentProfile>,
    seen: &mut BTreeSet<(String, String)>,
    inherited: &mut BTreeSet<(String, String)>,
) {
    let prefix = format!("{harness}-");
    for bin_dir in profile_bin_directories(home) {
        let Ok(entries) = fs::read_dir(bin_dir) else {
            continue;
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let Some(file_name) = path
                .file_name()
                .and_then(|value| value.to_str().map(str::to_owned))
            else {
                continue;
            };
            let Some(name) = file_name.strip_prefix(&prefix) else {
                continue;
            };
            if !valid_profile_name(name)
                || file_name.contains('.')
                || !is_executable(&path)
                || is_internal_codex_wrapper(home, harness, &path)
            {
                continue;
            }
            add_discovered_profile(
                profiles,
                seen,
                inherited,
                discovered_profile(harness, name, Some(&path), Vec::new(), None),
            );
        }
    }
}

/// Codex ships helper executables next to its standalone binary. They are not
/// user launch profiles even though their filenames begin with `codex-`.
fn is_internal_codex_wrapper(home: &Path, harness: &str, path: &Path) -> bool {
    if harness != "codex" {
        return false;
    }
    if matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("codex-execve-wrapper" | "codex-linux-sandbox")
    ) {
        return true;
    }
    let internal_roots = [
        home.join(".codex/packages/standalone"),
        home.join(".codex/tmp/arg0"),
    ];
    let resolved = path.canonicalize().ok();
    internal_roots.iter().any(|root| {
        path.starts_with(root)
            || resolved
                .as_ref()
                .is_some_and(|resolved| resolved.starts_with(root))
    })
}

/// Reads simple `claude-*`/`codex-*` aliases without sourcing or evaluating a
/// shell file. Only direct invocations of the matching local CLI are accepted.
fn discover_shell_alias_profiles(
    home: &Path,
    harness: &str,
    command: Option<&Path>,
    profiles: &mut Vec<AgentProfile>,
    seen: &mut BTreeSet<(String, String)>,
    inherited: &mut BTreeSet<(String, String)>,
) {
    let Some(command) = command else {
        return;
    };
    for path in shell_profile_files(home) {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        for line in source.lines() {
            if let Some(profile) = profile_from_shell_alias(line, harness, command) {
                merge_or_add_discovered_profile(profiles, seen, inherited, profile);
            }
        }
    }
}

fn profile_bin_directories(home: &Path) -> Vec<PathBuf> {
    let mut directories = vec![home.join(".local/bin"), home.join("bin")];
    #[cfg(target_os = "macos")]
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    if let Some(paths) = env::var_os("PATH") {
        for directory in env::split_paths(&paths) {
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
    }
    directories
}

fn shell_profile_files(home: &Path) -> Vec<PathBuf> {
    [
        ".zshrc",
        ".zprofile",
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".config/fish/config.fish",
    ]
    .into_iter()
    .map(|relative| home.join(relative))
    .collect()
}

fn profile_from_shell_alias(
    line: &str,
    harness: &str,
    default_command: &Path,
) -> Option<AgentProfile> {
    let line = line.trim();
    let remainder = line.strip_prefix("alias")?;
    if !remainder.chars().next()?.is_whitespace() {
        return None;
    }
    let (alias_name, raw_invocation) = remainder.trim_start().split_once('=')?;
    let profile_name = alias_name.trim().strip_prefix(&format!("{harness}-"))?;
    if !valid_profile_name(profile_name) {
        return None;
    }
    let invocation = unquote_shell_alias(raw_invocation)?;
    if contains_shell_syntax(invocation) {
        return None;
    }
    let words = shell_words::split(invocation).ok()?;
    let (command, args, env) = parse_alias_invocation(words, harness, default_command)?;
    Some(AgentProfile {
        name: profile_name.to_owned(),
        harness: harness.to_owned(),
        command: command.to_string_lossy().into_owned(),
        args,
        env,
        inherit_discovered: false,
        claude_relaunch_permissions: None,
        modes: Vec::new(),
    })
}

fn unquote_shell_alias(value: &str) -> Option<&str> {
    let value = value
        .trim()
        .strip_suffix(';')
        .unwrap_or(value.trim())
        .trim();
    let quote = value.chars().next()?;
    if matches!(quote, '\'' | '"') {
        return value
            .strip_prefix(quote)
            .and_then(|inner| inner.strip_suffix(quote));
    }
    Some(value)
}

fn contains_shell_syntax(value: &str) -> bool {
    value.contains(['$', '`', ';', '|', '&', '<', '>', '\n', '\r'])
}

fn parse_alias_invocation(
    words: Vec<String>,
    harness: &str,
    default_command: &Path,
) -> Option<(PathBuf, Vec<String>, BTreeMap<String, String>)> {
    let mut words = words.into_iter().peekable();
    if words.peek().is_some_and(|word| word == "env") {
        words.next();
    }
    let mut env = BTreeMap::new();
    while let Some(word) = words.peek() {
        let Some((key, value)) = word.split_once('=') else {
            break;
        };
        if !valid_environment_name(key) {
            return None;
        }
        let value = if matches!(key, "CLAUDE_CONFIG_DIR" | "CODEX_HOME") {
            expand_tilde(Path::new(value))
                .to_string_lossy()
                .into_owned()
        } else {
            value.to_owned()
        };
        env.insert(key.to_owned(), value);
        words.next();
    }
    if words.peek().is_some_and(|word| word == "command") {
        words.next();
    }
    let executable = words.next()?;
    let command = resolve_alias_command(&executable, harness, default_command)?;
    Some((command, words.collect(), env))
}

fn resolve_alias_command(command: &str, harness: &str, default_command: &Path) -> Option<PathBuf> {
    if command == harness {
        return Some(default_command.to_path_buf());
    }
    let path = expand_tilde(Path::new(command));
    (path.is_absolute()
        && path.file_name().and_then(|name| name.to_str()) == Some(harness)
        && is_executable(&path))
    .then_some(path)
}

fn valid_profile_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(byte) if byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Expands path-valued agent settings that atmux carries into a tmux `env`
/// invocation. Quoted `env KEY=~/path` arguments do not receive shell
/// tilde expansion, so leaving it relative would create a fresh Claude setup.
fn normalize_profile_environment(env: &mut BTreeMap<String, String>) {
    for key in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        if let Some(directory) = env.get_mut(key) {
            *directory = expand_tilde(Path::new(directory))
                .to_string_lossy()
                .into_owned();
        }
    }
}

fn find_program(command: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    program_on_path(command).or_else(|| {
        candidates
            .iter()
            .find_map(|path| canonical_executable(path))
    })
}

/// Resolves Claude for general profile discovery.
///
/// Profiles preserve the existing PATH-first behavior. Destructive in-place
/// resume deliberately uses the narrower owner-only resolver below.
pub(crate) fn current_claude_program() -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    resolve_claude_program(env::var_os("PATH").as_deref(), home.as_deref())
}

/// Resolves the owner-installed Claude launcher used for in-place resume.
///
/// Unlike general profile discovery, this boundary never trusts inherited
/// `PATH`. A GUI service may have a minimal PATH, while an attacker-controlled
/// PATH entry must never become a destructive tmux respawn target. Only the
/// explicit owner-local Claude locations are considered, and the returned
/// executable is a canonical, owner-controlled regular file below HOME.
pub(crate) fn resume_claude_program() -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    resolve_resume_claude_program(&home, rustix::process::geteuid().as_raw())
}

/// Rechecks a previously resolved resume launcher immediately before tmux is
/// invoked. This catches removal, permission changes, symlink replacement, or
/// ownership changes after the capability check without falling back to PATH.
pub(crate) fn revalidate_resume_claude_program(program: &Path) -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let euid = rustix::process::geteuid().as_raw();
    let home = trusted_resume_home(&home, euid)?;
    validate_canonical_resume_executable(program, &home, euid)
}

fn resolve_claude_program(path: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    path.and_then(|paths| {
        env::split_paths(paths)
            .map(|directory| directory.join("claude"))
            .find_map(|candidate| canonical_executable(&candidate))
    })
    .or_else(|| {
        home.and_then(|home| {
            claude_candidates(home)
                .iter()
                .find_map(|candidate| canonical_executable(candidate))
        })
    })
}

fn resolve_resume_claude_program(home: &Path, euid: u32) -> Option<PathBuf> {
    let home = trusted_resume_home(home, euid)?;
    claude_candidates(&home)
        .into_iter()
        .find_map(|candidate| validate_resume_candidate(&candidate, &home, euid))
}

fn trusted_resume_home(home: &Path, euid: u32) -> Option<PathBuf> {
    let home = home.canonicalize().ok()?;
    let metadata = fs::symlink_metadata(&home).ok()?;
    (home.is_absolute()
        && metadata.is_dir()
        && metadata.uid() == euid
        && metadata.permissions().mode() & 0o022 == 0
        && !metadata.file_type().is_symlink())
    .then_some(home)
}

fn validate_resume_candidate(candidate: &Path, home: &Path, euid: u32) -> Option<PathBuf> {
    if !candidate.is_absolute() || !candidate.starts_with(home) {
        return None;
    }
    trusted_owned_directory_chain(candidate.parent()?, home, euid)?;
    let candidate_metadata = fs::symlink_metadata(candidate).ok()?;
    if candidate_metadata.file_type().is_symlink() {
        // The native installer intentionally maintains this one current-version
        // symlink. Other explicit candidates must themselves be regular files.
        if candidate != home.join(".local/bin/claude") || candidate_metadata.uid() != euid {
            return None;
        }
    } else if !trusted_executable_metadata(&candidate_metadata, euid) {
        return None;
    }
    let resolved = candidate.canonicalize().ok()?;
    validate_canonical_resume_executable(&resolved, home, euid)
}

fn validate_canonical_resume_executable(program: &Path, home: &Path, euid: u32) -> Option<PathBuf> {
    if !program.is_absolute() || !program.starts_with(home) {
        return None;
    }
    trusted_owned_directory_chain(program.parent()?, home, euid)?;
    let metadata = fs::symlink_metadata(program).ok()?;
    if metadata.file_type().is_symlink() || !trusted_executable_metadata(&metadata, euid) {
        return None;
    }
    let canonical = program.canonicalize().ok()?;
    (canonical == program).then_some(canonical)
}

fn trusted_owned_directory_chain(directory: &Path, home: &Path, euid: u32) -> Option<()> {
    let relative = directory.strip_prefix(home).ok()?;
    let mut current = home.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return None;
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).ok()?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != euid
            || metadata.permissions().mode() & 0o022 != 0
        {
            return None;
        }
    }
    Some(())
}

fn trusted_executable_metadata(metadata: &fs::Metadata, euid: u32) -> bool {
    metadata.is_file()
        && metadata.uid() == euid
        && metadata.permissions().mode() & 0o111 != 0
        && metadata.permissions().mode() & 0o022 == 0
}

fn program_on_path(command: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join(command))
            .find_map(|path| canonical_executable(&path))
    })
}

fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().ok()?;
    (path.is_absolute() && is_executable(&path)).then_some(path)
}

fn codex_candidates(home: &Path) -> Vec<PathBuf> {
    let candidates = vec![home.join(".local/bin/codex"), home.join("bin/codex")];
    #[cfg(target_os = "macos")]
    let candidates = candidates
        .into_iter()
        .chain([
            PathBuf::from("/opt/homebrew/bin/codex"),
            PathBuf::from("/usr/local/bin/codex"),
        ])
        .collect();
    candidates
}

fn claude_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        home.join(".local/bin/claude"),
        home.join("bin/claude"),
        home.join(".local/share/claude/ClaudeCode.app/Contents/MacOS/claude"),
    ];
    let runtime = home.join("Library/Application Support/Claude/claude-code-vm");
    if let Ok(entries) = fs::read_dir(runtime) {
        let mut versions = entries
            .flatten()
            .map(|entry| entry.path().join("claude"))
            .collect::<Vec<_>>();
        versions.sort();
        versions.reverse();
        candidates.extend(versions);
    }
    candidates
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

const MAX_PROJECT_SCAN_DEPTH: usize = 4;
const IGNORED_PROJECT_DIRECTORIES: &[&str] = &[
    ".git",
    ".gradle",
    ".idea",
    ".terraform",
    "build",
    "dist",
    "node_modules",
    "target",
    "vendor",
];

fn discover_projects(root: &Path, paths: &mut BTreeSet<PathBuf>) {
    if !root.is_dir() {
        return;
    }
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        if project::is_project_directory(&directory) {
            paths.insert(directory.clone());
        }
        if depth >= MAX_PROJECT_SCAN_DEPTH {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.') || IGNORED_PROJECT_DIRECTORIES.contains(&name) {
                continue;
            }
            pending.push((path, depth + 1));
        }
    }
}

#[must_use]
pub fn expand_tilde(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        return env::var_os("HOME").map_or_else(|| path.to_path_buf(), PathBuf::from);
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ResumeHome {
        root: PathBuf,
        home: PathBuf,
    }

    impl ResumeHome {
        fn new(name: &str) -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};

            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!(
                "atmux resume {name}-{}-{nonce}",
                std::process::id()
            ));
            let home = root.join("owner home");
            fs::create_dir_all(&home).unwrap();
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
            Self { root, home }
        }

        fn owner_dir(&self, relative: &str) -> PathBuf {
            let path = self.home.join(relative);
            fs::create_dir_all(&path).unwrap();
            let mut current = self.home.clone();
            for component in Path::new(relative).components() {
                current.push(component);
                fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).unwrap();
            }
            path
        }

        fn native_launcher(&self, mode: u32) -> PathBuf {
            use std::os::unix::fs::{PermissionsExt, symlink};

            let bin = self.owner_dir(".local/bin");
            let versions = self.owner_dir(".local/share/claude/versions");
            let installed = versions.join("2.1.241");
            fs::write(&installed, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&installed, fs::Permissions::from_mode(mode)).unwrap();
            symlink(&installed, bin.join("claude")).unwrap();
            installed
        }
    }

    impl Drop for ResumeHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn resume_launcher_accepts_midnight_native_symlink_and_space_paths() {
        let fixture = ResumeHome::new("native layout");
        let installed = fixture.native_launcher(0o700);
        let resolved =
            resolve_resume_claude_program(&fixture.home, rustix::process::geteuid().as_raw());
        assert_eq!(resolved, Some(installed.canonicalize().unwrap()));
    }

    #[test]
    fn resume_launcher_does_not_consider_a_path_only_executable() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = ResumeHome::new("path attacker");
        let attacker = fixture.root.join("attacker-bin/claude");
        fs::create_dir_all(attacker.parent().unwrap()).unwrap();
        fs::write(&attacker, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&attacker, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            resolve_resume_claude_program(&fixture.home, rustix::process::geteuid().as_raw())
                .is_none()
        );
    }

    #[test]
    fn resume_launcher_rejects_a_current_symlink_outside_home() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = ResumeHome::new("outside symlink");
        let bin = fixture.owner_dir(".local/bin");
        let outside = fixture.root.join("outside/claude");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        fs::write(&outside, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&outside, bin.join("claude")).unwrap();

        assert!(
            resolve_resume_claude_program(&fixture.home, rustix::process::geteuid().as_raw())
                .is_none()
        );
    }

    #[test]
    fn resume_launcher_rejects_wrong_owner_nonexec_and_directory() {
        use std::os::unix::fs::PermissionsExt;

        let wrong_owner = ResumeHome::new("wrong owner");
        wrong_owner.native_launcher(0o700);
        let euid = rustix::process::geteuid().as_raw();
        assert!(resolve_resume_claude_program(&wrong_owner.home, euid.wrapping_add(1)).is_none());

        let nonexec = ResumeHome::new("nonexec");
        nonexec.native_launcher(0o600);
        assert!(resolve_resume_claude_program(&nonexec.home, euid).is_none());

        let writable = ResumeHome::new("group writable");
        writable.native_launcher(0o720);
        assert!(resolve_resume_claude_program(&writable.home, euid).is_none());

        let directory = ResumeHome::new("directory");
        let candidate = directory.owner_dir(".local/bin").join("claude");
        fs::create_dir_all(&candidate).unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(resolve_resume_claude_program(&directory.home, euid).is_none());
    }

    #[test]
    fn resume_launcher_rejects_symlinked_parent_components() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = ResumeHome::new("parent symlink");
        let alternate = fixture.home.join("alternate/bin");
        fs::create_dir_all(&alternate).unwrap();
        let launcher = alternate.join("claude");
        fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(fixture.home.join("alternate"), fixture.home.join(".local")).unwrap();

        assert!(
            resolve_resume_claude_program(&fixture.home, rustix::process::geteuid().as_raw())
                .is_none()
        );
    }

    #[test]
    fn default_config_parses() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.general.refresh_ms, 750);
        assert_eq!(config.agent_resources.memory_max_bytes, None);
        assert_eq!(config.agent_resources.memory_override_max_bytes, None);
        let claude = config.profiles_for("claude").remove(0);
        assert_eq!(claude.claude_relaunch_permissions, None);
        assert_eq!(
            claude.effective_claude_relaunch_permissions(),
            ClaudeRelaunchPermissions::AtmuxInjects
        );
    }

    #[test]
    fn agent_memory_max_is_explicit_and_nonzero() {
        let configured: Config = toml::from_str(
            r"
[agent_resources]
memory_max_bytes = 34359738368
",
        )
        .unwrap();
        assert_eq!(
            configured.agent_resources.memory_max_bytes,
            Some(34_359_738_368)
        );
        if cfg!(target_os = "linux") {
            configured.agent_resources.validate().unwrap();
        } else {
            assert!(configured.agent_resources.validate().is_err());
        }

        let disabled = AgentResourcesConfig {
            memory_max_bytes: None,
            memory_override_max_bytes: None,
        };
        disabled.validate().unwrap();
        let invalid = AgentResourcesConfig {
            memory_max_bytes: Some(0),
            memory_override_max_bytes: None,
        };
        assert!(invalid.validate().is_err());
        let infinity = AgentResourcesConfig {
            memory_max_bytes: Some(u64::MAX),
            memory_override_max_bytes: None,
        };
        let error = infinity.validate().unwrap_err().to_string();
        assert!(error.contains("infinity"));

        let configurable: Config = toml::from_str(
            r"
[agent_resources]
memory_max_bytes = 17179869184
memory_override_max_bytes = 25769803776
",
        )
        .unwrap();
        if cfg!(target_os = "linux") {
            configurable.agent_resources.validate().unwrap();
        } else {
            assert!(configurable.agent_resources.validate().is_err());
        }
        assert_eq!(
            configurable.agent_resources.memory_override_max_bytes,
            Some(25_769_803_776)
        );

        for invalid in [
            AgentResourcesConfig {
                memory_max_bytes: None,
                memory_override_max_bytes: Some(16),
            },
            AgentResourcesConfig {
                memory_max_bytes: Some(32),
                memory_override_max_bytes: Some(16),
            },
            AgentResourcesConfig {
                memory_max_bytes: Some(16),
                memory_override_max_bytes: Some(u64::MAX),
            },
            AgentResourcesConfig {
                memory_max_bytes: Some(16),
                memory_override_max_bytes: Some(1024 * 1024 * 1024 + 1),
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn groups_profiles_by_harness() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.profiles_for("CODEX").len(), 1);
        assert_eq!(config.harnesses(), vec!["codex", "claude"]);
    }

    #[test]
    fn simple_shell_aliases_become_launchable_profiles_without_evaluation() {
        let profile = profile_from_shell_alias(
            "alias claude-fast='env PROVIDER=local TOKEN=value claude --model fast'",
            "claude",
            Path::new("/opt/homebrew/bin/claude"),
        )
        .unwrap();
        assert_eq!(profile.name, "fast");
        assert_eq!(profile.harness, "claude");
        assert_eq!(profile.command, "/opt/homebrew/bin/claude");
        assert_eq!(profile.args, vec!["--model", "fast"]);
        assert_eq!(
            profile.effective_claude_relaunch_permissions(),
            ClaudeRelaunchPermissions::AtmuxInjects
        );
        assert_eq!(profile.env.get("PROVIDER"), Some(&"local".to_owned()));
        assert_eq!(profile.env.get("TOKEN"), Some(&"value".to_owned()));
    }

    #[test]
    fn shell_alias_expands_claude_config_dir_before_tmux_quotes_it() {
        let profile = profile_from_shell_alias(
            "alias claude-max=\"CLAUDE_CONFIG_DIR=~/.claude-max claude --dangerously-skip-permissions\"",
            "claude",
            Path::new("/opt/homebrew/bin/claude"),
        )
        .unwrap();
        let directory = profile.env.get("CLAUDE_CONFIG_DIR").unwrap();
        assert!(Path::new(directory).is_absolute());
        assert!(directory.ends_with(".claude-max"));
    }

    #[test]
    fn configured_default_profiles_use_the_discovered_absolute_cli() {
        let mut codex = AgentProfile {
            name: "Default".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: vec!["--profile".to_owned(), "work".to_owned()],
            env: BTreeMap::from([("CODEX_HOME".to_owned(), "/tmp/codex".to_owned())]),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let mut custom = AgentProfile {
            name: "Custom".to_owned(),
            harness: "codex".to_owned(),
            command: "/opt/custom/codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let discovered = Path::new("/home/ryan/.local/bin/codex");

        resolve_configured_default_command(&mut codex, Some(discovered), None);
        resolve_configured_default_command(&mut custom, Some(discovered), None);

        assert_eq!(codex.command, discovered.to_string_lossy());
        assert!(Path::new(&codex.command).is_absolute());
        assert_eq!(codex.args, ["--profile", "work"]);
        assert_eq!(codex.env["CODEX_HOME"], "/tmp/codex");
        assert_eq!(custom.command, "/opt/custom/codex");
    }

    #[test]
    fn configured_store_binding_survives_discovered_wrapper_environment() {
        let configured_store = "/srv/agents/claude-max";
        let mut profiles = vec![AgentProfile {
            name: "max".to_owned(),
            harness: "claude".to_owned(),
            command: "claude".to_owned(),
            args: Vec::new(),
            env: BTreeMap::from([
                ("CLAUDE_CONFIG_DIR".to_owned(), configured_store.to_owned()),
                ("LOCAL_MARKER".to_owned(), "configured".to_owned()),
            ]),
            inherit_discovered: true,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        }];
        let discovered = AgentProfile {
            name: "max".to_owned(),
            harness: "claude".to_owned(),
            command: "/usr/local/bin/claude-max".to_owned(),
            args: vec!["--wrapper-flag".to_owned()],
            env: BTreeMap::from([
                (
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    "/wrong/discovered/store".to_owned(),
                ),
                ("DISCOVERED_MARKER".to_owned(), "present".to_owned()),
            ]),
            inherit_discovered: false,
            claude_relaunch_permissions: Some(ClaudeRelaunchPermissions::LauncherProvides),
            modes: Vec::new(),
        };

        let mut explicitly_managed = profiles.clone();
        explicitly_managed[0].claude_relaunch_permissions =
            Some(ClaudeRelaunchPermissions::AtmuxInjects);
        merge_or_add_discovered_profile(
            &mut explicitly_managed,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
            discovered.clone(),
        );
        assert_eq!(
            explicitly_managed[0].claude_relaunch_permissions,
            Some(ClaudeRelaunchPermissions::AtmuxInjects)
        );

        merge_or_add_discovered_profile(
            &mut profiles,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
            discovered,
        );

        let merged = &profiles[0];
        assert_eq!(merged.command, "/usr/local/bin/claude-max");
        assert_eq!(merged.args, ["--wrapper-flag"]);
        assert_eq!(merged.env["CLAUDE_CONFIG_DIR"], configured_store);
        assert_eq!(merged.env["LOCAL_MARKER"], "configured");
        assert_eq!(merged.env["DISCOVERED_MARKER"], "present");
        assert_eq!(
            merged.claude_relaunch_permissions,
            Some(ClaudeRelaunchPermissions::LauncherProvides)
        );
    }

    #[test]
    fn configured_store_binding_survives_wrapper_with_empty_environment() {
        let mut profiles = vec![AgentProfile {
            name: "work".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::from([("CODEX_HOME".to_owned(), "/srv/codex-work".to_owned())]),
            inherit_discovered: true,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        }];
        let discovered = AgentProfile {
            name: "work".to_owned(),
            harness: "codex".to_owned(),
            command: "/usr/local/bin/codex-work".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };

        merge_or_add_discovered_profile(
            &mut profiles,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
            discovered,
        );

        assert_eq!(profiles[0].env["CODEX_HOME"], "/srv/codex-work");
    }

    #[test]
    fn executable_claude_wrapper_discovery_does_not_guess_launcher_permissions() {
        let fixture = ResumeHome::new("wrapper policy");
        let bin = fixture.owner_dir(".local/bin");
        let wrapper = bin.join("claude-policy-canary");
        fs::write(&wrapper, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let mut profiles = Vec::new();
        let mut seen = BTreeSet::new();
        let mut inherited = BTreeSet::new();

        discover_executable_profiles(
            &fixture.home,
            "claude",
            &mut profiles,
            &mut seen,
            &mut inherited,
        );

        let profile = profiles
            .iter()
            .find(|profile| profile.name == "policy-canary")
            .unwrap();
        assert_eq!(profile.claude_relaunch_permissions, None);
        assert_eq!(
            profile.effective_claude_relaunch_permissions(),
            ClaudeRelaunchPermissions::AtmuxInjects
        );
    }

    #[test]
    fn configured_discovery_keeps_executable_precedence_and_still_merges_alias_only_profiles() {
        let fixture = ResumeHome::new("configured discovery precedence");
        let wrapper = fixture.owner_dir(".local/bin").join("claude-max");
        fs::write(&wrapper, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            fixture.home.join(".zshrc"),
            concat!(
                "alias claude-max='claude --dangerously-skip-permissions'\n",
                "alias claude-work='CLAUDE_CONFIG_DIR=~/.claude-work claude --dangerously-skip-permissions'\n",
            ),
        )
        .unwrap();
        let mut profiles = ["max", "work"]
            .into_iter()
            .map(|name| AgentProfile {
                name: name.to_owned(),
                harness: "claude".to_owned(),
                command: "claude".to_owned(),
                args: Vec::new(),
                env: BTreeMap::from([("LOCAL_MARKER".to_owned(), name.to_owned())]),
                inherit_discovered: true,
                claude_relaunch_permissions: None,
                modes: Vec::new(),
            })
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::from([
            ("claude".to_owned(), "max".to_owned()),
            ("claude".to_owned(), "work".to_owned()),
        ]);
        let mut inherited = BTreeSet::new();
        let native = Path::new("/usr/local/bin/claude");

        discover_executable_profiles(
            &fixture.home,
            "claude",
            &mut profiles,
            &mut seen,
            &mut inherited,
        );
        discover_shell_alias_profiles(
            &fixture.home,
            "claude",
            Some(native),
            &mut profiles,
            &mut seen,
            &mut inherited,
        );

        let max = profiles
            .iter()
            .find(|profile| profile.name == "max")
            .unwrap();
        assert_eq!(max.command, wrapper.to_string_lossy());
        assert!(max.args.is_empty());
        assert_eq!(max.env["LOCAL_MARKER"], "max");

        let work = profiles
            .iter()
            .find(|profile| profile.name == "work")
            .unwrap();
        assert_eq!(work.command, native.to_string_lossy());
        assert_eq!(work.args, ["--dangerously-skip-permissions"]);
        assert_eq!(work.env["LOCAL_MARKER"], "work");
        assert!(work.env["CLAUDE_CONFIG_DIR"].ends_with(".claude-work"));
    }

    #[test]
    fn full_discovery_keeps_claude_default_wrapper_ahead_of_generic_default() {
        let fixture = ResumeHome::new("claude default wrapper precedence");
        let wrapper = fixture.owner_dir(".local/bin").join("claude-Default");
        fs::write(&wrapper, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let native = fixture.home.join("native-claude");
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        let profile = config
            .profiles
            .iter_mut()
            .find(|profile| profile.harness == "claude" && profile.name == "Default")
            .unwrap();
        profile.inherit_discovered = true;
        profile
            .env
            .insert("LOCAL_MARKER".into(), "configured".into());

        config.discover_profiles_from(Some(&fixture.home), None, Some(&native));

        let profile = config
            .profiles
            .iter()
            .find(|profile| profile.harness == "claude" && profile.name == "Default")
            .unwrap();
        assert_eq!(profile.command, wrapper.to_string_lossy());
        assert!(profile.args.is_empty());
        assert_eq!(profile.env["LOCAL_MARKER"], "configured");
    }

    #[test]
    fn full_discovery_keeps_codex_wrapper_ahead_of_named_config_fallback() {
        let fixture = ResumeHome::new("codex named wrapper precedence");
        let wrapper = fixture.owner_dir(".local/bin").join("codex-work");
        fs::write(&wrapper, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(fixture.owner_dir(".codex").join("work.config.toml"), "").unwrap();
        let native = fixture.home.join("native-codex");
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        config.profiles.push(AgentProfile {
            name: "work".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::from([("LOCAL_MARKER".to_owned(), "configured".to_owned())]),
            inherit_discovered: true,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        });

        config.discover_profiles_from(Some(&fixture.home), Some(&native), None);

        let profile = config
            .profiles
            .iter()
            .find(|profile| profile.harness == "codex" && profile.name == "work")
            .unwrap();
        assert_eq!(profile.command, wrapper.to_string_lossy());
        assert!(profile.args.is_empty());
        assert_eq!(profile.env["LOCAL_MARKER"], "configured");
    }

    #[test]
    fn full_discovery_sorts_case_colliding_codex_config_fallbacks() {
        let fixture = ResumeHome::new("codex config collision order");
        let codex_dir = fixture.owner_dir(".codex");
        fs::write(codex_dir.join("zz-atmux-case.config.toml"), "").unwrap();
        fs::write(codex_dir.join("Zz-Atmux-Case.config.toml"), "").unwrap();
        let native = fixture.home.join("native-codex");
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        config.profiles.push(AgentProfile {
            name: "zZ-aTmUx-CaSe".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: true,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        });

        config.discover_profiles_from(Some(&fixture.home), Some(&native), None);

        let profile = config
            .profiles
            .iter()
            .find(|profile| profile.harness == "codex" && profile.name == "zZ-aTmUx-CaSe")
            .unwrap();
        assert_eq!(profile.command, native.to_string_lossy());
        assert_eq!(profile.args, ["--profile", "Zz-Atmux-Case"]);
    }

    #[test]
    fn explicit_launcher_permission_policy_is_claude_only() {
        let configured: Config = toml::from_str(
            r#"
[[profiles]]
name = "wrapped"
harness = "claude"
command = "/owner/bin/claude-wrapper"
claude_relaunch_permissions = "launcher_provides"
"#,
        )
        .unwrap();
        assert_eq!(
            configured.profiles[0].claude_relaunch_permissions,
            Some(ClaudeRelaunchPermissions::LauncherProvides)
        );
        configured.validate_profiles().unwrap();

        let invalid: Config = toml::from_str(
            r#"
[[profiles]]
name = "codex"
harness = "codex"
command = "codex"
claude_relaunch_permissions = "atmux_injects"
"#,
        )
        .unwrap();
        assert!(invalid.validate_profiles().is_err());
    }

    #[test]
    fn shell_alias_discovery_rejects_shell_syntax_and_wrong_harnesses() {
        let command = Path::new("/usr/local/bin/claude");
        assert!(
            profile_from_shell_alias(
                "alias claude-unsafe='claude --model $(lookup)'",
                "claude",
                command,
            )
            .is_none()
        );
        assert!(
            profile_from_shell_alias(
                "alias claude-remote='claude --model fast | tee output'",
                "claude",
                command,
            )
            .is_none()
        );
        assert!(
            profile_from_shell_alias("alias claude-wrong='codex'", "claude", command).is_none()
        );
    }

    #[test]
    fn codex_standalone_helpers_are_not_launch_profiles() {
        let home = Path::new("/home/ryan");
        assert!(is_internal_codex_wrapper(
            home,
            "codex",
            Path::new(
                "/home/ryan/.codex/packages/standalone/releases/current/bin/codex-linux-sandbox"
            ),
        ));
        assert!(is_internal_codex_wrapper(
            home,
            "codex",
            Path::new("/home/ryan/.codex/tmp/arg0/codex-arg0/codex-execve-wrapper"),
        ));
        assert!(!is_internal_codex_wrapper(
            home,
            "claude",
            Path::new(
                "/home/ryan/.codex/packages/standalone/releases/current/bin/codex-linux-sandbox"
            ),
        ));
    }

    #[test]
    fn partial_config_uses_field_defaults_without_recursing() {
        let config: Config = toml::from_str("profiles = []").unwrap();
        assert_eq!(config.general.refresh_ms, 750);
        assert!(config.status.working_markers.is_empty());
        assert!(!config.auto_compact.enabled);
        assert_eq!(config.auto_compact.inactivity_minutes, 15);
        assert_eq!(config.auto_compact.input_tokens, 200_000);
        assert_eq!(config.auto_compact.poll_seconds, 30);
        config.validate_auto_compact().unwrap();
    }

    #[test]
    fn auto_compact_configuration_is_bounded_and_explicitly_enabled() {
        let configured: Config = toml::from_str(
            "
[auto_compact]
enabled = true
inactivity_minutes = 20
input_tokens = 250000
poll_seconds = 60
",
        )
        .unwrap();
        assert!(configured.auto_compact.enabled);
        configured.validate_auto_compact().unwrap();

        for source in [
            "[auto_compact]\ninactivity_minutes = 0",
            "[auto_compact]\ninput_tokens = 0",
            "[auto_compact]\npoll_seconds = 4",
            "[auto_compact]\npoll_seconds = 3601",
        ] {
            let invalid: Config = toml::from_str(source).unwrap();
            assert!(invalid.validate_auto_compact().is_err(), "{source}");
        }
    }

    fn coordinator_only_config() -> Config {
        toml::from_str(
            r#"
profiles = []

[general]
project_roots = []
favorite_dirs = []
switch_on_launch = false

[node]
id = "home"
coordinator_only = true
"#,
        )
        .unwrap()
    }

    #[test]
    fn coordinator_only_is_opt_in_and_does_not_discover_local_profiles() {
        let default: Config = toml::from_str("profiles = []").unwrap();
        assert!(!default.node.coordinator_only);

        let mut coordinator = coordinator_only_config();
        coordinator.normalize();
        assert!(coordinator.profiles.is_empty());
        coordinator.validate_coordinator_only().unwrap();
    }

    #[test]
    fn coordinator_only_rejects_every_local_owner_capability() {
        let assert_rejected = |config: Config, expected: &str| {
            let error = config.validate_coordinator_only().unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        };

        let mut config = coordinator_only_config();
        config.profiles.push(Config::default().profiles[0].clone());
        assert_rejected(config, "profiles = []");

        let mut config = coordinator_only_config();
        config.general.project_roots.push(PathBuf::from("/srv"));
        assert_rejected(config, "project_roots");

        let mut config = coordinator_only_config();
        config.general.switch_on_launch = true;
        assert_rejected(config, "switch_on_launch");

        let mut config = coordinator_only_config();
        config.discovery.enabled = true;
        assert_rejected(config, "[discovery].enabled");

        let mut config = coordinator_only_config();
        config.auto_compact.enabled = true;
        assert_rejected(config, "[auto_compact].enabled");

        let mut config = coordinator_only_config();
        config.agent_resources.memory_max_bytes = Some(1024);
        assert_rejected(config, "[agent_resources].memory_max_bytes");

        let mut config = coordinator_only_config();
        config.maintenance.enabled = true;
        assert_rejected(config, "[maintenance].enabled");

        #[cfg(feature = "pulse")]
        {
            let mut config = coordinator_only_config();
            config.pulse.collect = true;
            assert_rejected(config, "[pulse].collect");

            let mut config = coordinator_only_config();
            config.pulse.receive = true;
            assert_rejected(config, "[pulse].receive");

            let mut config = coordinator_only_config();
            config.pulse.report_to = Some("http://127.0.0.1:9000".to_owned());
            assert_rejected(config, "push reporting");

            let mut config = coordinator_only_config();
            config.pulse.credentials.gemini_oauth_client_id_env = Some("CLIENT_ID".to_owned());
            config.pulse.credentials.gemini_oauth_client_secret_env =
                Some("CLIENT_SECRET".to_owned());
            assert_rejected(config, "owner-local Pulse credential references");
        }
    }

    #[test]
    fn web_proxy_token_path_expands_tilde() {
        let mut config: Config = toml::from_str(
            r#"
[web]
proxy_token_file = "~/.config/atmux/web-proxy.token"
"#,
        )
        .unwrap();

        config.normalize();

        let path = config.web.proxy_token_file.unwrap();
        assert!(path.is_absolute());
        assert!(path.ends_with(".config/atmux/web-proxy.token"));
    }

    #[test]
    fn unauthenticated_loopback_is_an_explicit_opt_in() {
        let default: Config = toml::from_str("profiles = []").unwrap();
        assert!(!default.web.allow_unauthenticated_loopback);

        let opted_in: Config = toml::from_str(
            r"
[web]
allow_unauthenticated_loopback = true
",
        )
        .unwrap();
        assert!(opted_in.web.allow_unauthenticated_loopback);
    }

    #[cfg(feature = "pulse")]
    #[test]
    fn pulse_bootstrap_paths_expand_before_validation() {
        let mut config: Config = toml::from_str(
            r#"
[pulse]
serve = true

[[pulse.accounts]]
id = 7
identity = "operator@example.test"

[[pulse.accounts.profiles]]
name = "deepseek"
vendor = "deepseek-balance"
config_dir = "~/.config/deepseek"
monthly_budget_usd = 20.0
api_key_file = "~/.config/deepseek/api-key"
"#,
        )
        .unwrap();

        config.normalize();
        let profile = &config.pulse.accounts[0].profiles[0];
        assert!(profile.config_dir.as_ref().unwrap().is_absolute());
        assert!(profile.api_key_file.as_ref().unwrap().is_absolute());
        config.pulse.validate().unwrap();
    }

    #[test]
    fn directory_picker_finds_grouped_and_instruction_marked_subprojects() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("atmux-project-tree-{nonce}"));
        let grouped = root.join("nes-spring").join("spring-ws");
        let marked = root.join("nes-experimental").join("scratch");
        let instruction_marked = root.join("monorepo").join("scripts");
        let ignored = root.join("node_modules").join("dependency");
        fs::create_dir_all(grouped.join(".git")).unwrap();
        fs::create_dir_all(&marked).unwrap();
        fs::write(
            marked.join(project::PROJECT_FILE),
            "session_name = 'scratch'\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("monorepo").join(".git")).unwrap();
        fs::create_dir_all(&instruction_marked).unwrap();
        fs::write(
            instruction_marked.join("AGENTS.md"),
            "# Launch this directory with an agent\n",
        )
        .unwrap();
        fs::create_dir_all(ignored.join(".git")).unwrap();

        let mut config = Config::default();
        config.general.project_roots = vec![root.clone()];
        config.general.favorite_dirs.clear();
        let directories = config.directories();
        assert!(directories.contains(&grouped));
        assert!(directories.contains(&marked));
        assert!(directories.contains(&root.join("monorepo")));
        assert!(directories.contains(&instruction_marked));
        assert!(!directories.contains(&root.join("nes-spring")));
        assert!(!directories.contains(&ignored));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_launch_directory_must_exist_below_a_configured_root() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("atmux-manual-root-{nonce}"));
        let manual = root.join("grouping").join("plain-folder");
        let outside = std::env::temp_dir().join(format!("atmux-manual-outside-{nonce}"));
        fs::create_dir_all(&manual).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let mut config = Config::default();
        config.general.project_roots = vec![root.clone()];
        config.general.favorite_dirs.clear();

        assert_eq!(
            config.resolve_launch_directory(&manual),
            Some(manual.canonicalize().unwrap())
        );
        assert_eq!(config.resolve_launch_directory(&outside), None);
        assert_eq!(
            config.resolve_launch_directory(Path::new("relative/path")),
            None
        );

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_configuration_without_machines_stays_local_only() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(config.machines.is_empty());
        assert_eq!(config.node.id, LOCAL_MACHINE_ID);
        assert_eq!(config.node_label(), "This machine");
        assert!(config.node.token_env.is_none());
        assert!(config.validate_federation().is_ok());
    }

    #[test]
    fn discovery_requires_unique_identity_and_shared_credentials() {
        let invalid: Config = toml::from_str(
            r#"
[discovery]
enabled = true
token_env = "ATMUX_LAN_TOKEN"
"#,
        )
        .unwrap();
        assert!(invalid.validate_federation().is_err());

        let valid: Config = toml::from_str(
            r#"
[node]
id = "workstation"
token_env = "ATMUX_LAN_TOKEN"

[node.tls]
cert_file = "/run/atmux/node.crt"
key_file = "/run/atmux/node.key"
ca_file = "/run/atmux/ca.crt"

[discovery]
enabled = true
token_env = "ATMUX_LAN_TOKEN"
"#,
        )
        .unwrap();
        assert!(valid.validate_federation().is_ok());
    }

    #[test]
    fn machines_parse_with_referenced_credentials_only() {
        let config: Config = toml::from_str(
            r#"
[node]
id = "hub"
label = "Hub"

[node.tls]
cert_file = "/run/atmux/node.crt"
key_file = "/run/atmux/node.key"
ca_file = "/run/atmux/ca.crt"

[[machines]]
id = "gpu-box"
label = "GPU box"
url = "https://gpu-box.tail1234.ts.net:7345"
token_env = "ATMUX_GPU_BOX_TOKEN"

[[machines]]
id = "mini"
url = "https://100.64.0.9:7345"
token_file = "/run/secrets/mini"
"#,
        )
        .unwrap();
        config.validate_federation().unwrap();
        assert_eq!(config.machines.len(), 2);
        assert_eq!(config.node_label(), "Hub");
        assert_eq!(
            config.machines[0].token_env.as_deref(),
            Some("ATMUX_GPU_BOX_TOKEN")
        );
        // No configuration field can carry an inline secret.
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("Authorization"));
        assert!(rendered.contains("token_env"));
    }

    #[test]
    fn federation_validation_rejects_collisions_and_unsafe_urls() {
        let duplicate: Config = toml::from_str(
            r#"
[[machines]]
id = "gpu-box"
url = "http://a:7345"

[[machines]]
id = "gpu-box"
url = "http://b:7345"
"#,
        )
        .unwrap();
        assert!(duplicate.validate_federation().is_err());

        let reserved: Config = toml::from_str(
            r#"
[[machines]]
id = "local"
url = "http://a:7345"
"#,
        )
        .unwrap();
        assert!(reserved.validate_federation().is_err());

        let bad_url: Config = toml::from_str(
            r#"
[[machines]]
id = "gpu-box"
url = "http://user:pass@a:7345"
"#,
        )
        .unwrap();
        assert!(bad_url.validate_federation().is_err());

        let bad_id: Config = toml::from_str(
            r#"
[[machines]]
id = "GPU~box"
url = "http://a:7345"
"#,
        )
        .unwrap();
        assert!(bad_id.validate_federation().is_err());
    }

    #[test]
    fn network_federation_requires_https_and_a_node_tls_identity() {
        let plaintext: Config = toml::from_str(
            r#"
[[machines]]
id = "gpu-box"
url = "http://192.168.1.8:7345"
"#,
        )
        .unwrap();
        assert!(plaintext.validate_federation().is_err());

        let missing_tls: Config = toml::from_str(
            r#"
[[machines]]
id = "gpu-box"
url = "https://192.168.1.8:7345"
"#,
        )
        .unwrap();
        assert!(missing_tls.validate_federation().is_err());
    }
}
