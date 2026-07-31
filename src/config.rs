use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG: &str = r#"# atmux configuration

[general]
# Each root and its immediate child directories appear in the launch picker.
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

[[profiles]]
name = "Default"
harness = "claude"
command = "claude"
args = []

# Example:
# [[profiles]]
# name = "Sol xhigh"
# harness = "codex"
# command = "codex"
# args = ["-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"xhigh\""]
#
# [profiles.env]
# SOME_VARIABLE = "value"

[status]
# Matching is case-insensitive. These extend atmux's built-in heuristics.
working_markers = []
waiting_markers = []
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub profiles: Vec<AgentProfile>,
    #[serde(default)]
    pub status: StatusConfig,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentProfile {
    pub name: String,
    pub harness: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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
        Ok((config, path))
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
        }
        self.discover_profiles();
    }

    fn discover_profiles(&mut self) {
        let configured_profiles = self.profiles.len();
        let mut seen: BTreeSet<(String, String)> = self
            .profiles
            .iter()
            .map(|profile| (profile.harness.to_lowercase(), profile.name.to_lowercase()))
            .collect();

        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            let codex_dir = home.join(".codex");
            if let Ok(entries) = fs::read_dir(codex_dir) {
                for entry in entries.flatten() {
                    let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    let Some(name) = file_name.strip_suffix(".config.toml") else {
                        continue;
                    };
                    let key = ("codex".to_owned(), name.to_lowercase());
                    if seen.insert(key) {
                        self.profiles.push(AgentProfile {
                            name: name.to_owned(),
                            harness: "codex".to_owned(),
                            command: "codex".to_owned(),
                            args: vec!["--profile".to_owned(), name.to_owned()],
                            env: BTreeMap::new(),
                        });
                    }
                }
            }

            for bin_dir in [home.join(".local/bin"), home.join("bin")] {
                let Ok(entries) = fs::read_dir(bin_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
                        continue;
                    };
                    if !file_name.starts_with("claude-")
                        || file_name.contains('.')
                        || !path.is_file()
                    {
                        continue;
                    }
                    let name = file_name.trim_start_matches("claude-");
                    let key = ("claude".to_owned(), name.to_lowercase());
                    if seen.insert(key) {
                        self.profiles.push(AgentProfile {
                            name: name.to_owned(),
                            harness: "claude".to_owned(),
                            command: path.to_string_lossy().into_owned(),
                            args: Vec::new(),
                            env: BTreeMap::new(),
                        });
                    }
                }
            }
        }

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
            if favorite.is_dir() {
                paths.insert(favorite.clone());
            }
        }
        for root in &self.general.project_roots {
            if !root.is_dir() {
                continue;
            }
            paths.insert(root.clone());
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir()
                        && !path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with('.'))
                    {
                        paths.insert(path);
                    }
                }
            }
        }
        paths.into_iter().collect()
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

    #[test]
    fn default_config_parses() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.general.refresh_ms, 750);
    }

    #[test]
    fn groups_profiles_by_harness() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.profiles_for("CODEX").len(), 1);
        assert_eq!(config.harnesses(), vec!["codex", "claude"]);
    }

    #[test]
    fn partial_config_uses_field_defaults_without_recursing() {
        let config: Config = toml::from_str("profiles = []").unwrap();
        assert_eq!(config.general.refresh_ms, 750);
        assert!(config.status.working_markers.is_empty());
    }
}
