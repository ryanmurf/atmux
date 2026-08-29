//! Per-project preferences and project-root detection.
//!
//! A project is a Git worktree (a `.git` directory or file), a directory
//! explicitly marked with `.atmux.toml`, or a directory that carries agent
//! instructions. Grouping folders are intentionally not projects: discovery
//! continues through them until it finds an eligible launch directory.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::AgentProfile;

/// Project-local atmux preferences, deliberately visible and versionable.
pub const PROJECT_FILE: &str = ".atmux.toml";

/// Agent instruction files that make a directory useful as an agent launch
/// root even when it is not a Git worktree.
pub const AGENT_INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md", "GEMINI.md"];

/// A process-local suffix makes temporary preference files distinct even when
/// several launch requests for the same project arrive at once. `create_new`
/// below is the actual safety boundary: a pre-existing file or symlink is
/// never opened for writing.
static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_PROJECT_FILE_BYTES: u64 = 1024 * 1024;

/// Preferences safe to expose through the launch-options API.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProjectPreferences {
    pub session_name: Option<String>,
    pub harness: Option<String>,
    pub profile: Option<String>,
}

/// The on-disk representation retains unrecognized fields so atmux does not
/// destroy future project settings when it updates a remembered launch.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct ProjectFile {
    session_name: Option<String>,
    harness: Option<String>,
    profile: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

impl ProjectFile {
    fn preferences(&self) -> ProjectPreferences {
        ProjectPreferences {
            session_name: self.session_name.clone(),
            harness: self.harness.clone(),
            profile: self.profile.clone(),
        }
    }

    fn normalize(&mut self) {
        self.session_name = normalize_value(self.session_name.take());
        self.harness = normalize_value(self.harness.take());
        self.profile = normalize_value(self.profile.take());
    }
}

fn normalize_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

/// Returns true when this directory contains a supported agent instruction
/// file.
#[must_use]
pub fn has_agent_instructions(directory: &Path) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_type().is_ok_and(|file_type| file_type.is_file())
            && entry.file_name().to_str().is_some_and(|name| {
                AGENT_INSTRUCTION_FILES
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate))
            })
    })
}

/// Returns true for a Git worktree, an explicitly configured atmux project, or
/// a directory with agent instructions.
#[must_use]
pub fn is_project_directory(directory: &Path) -> bool {
    directory.join(".git").is_dir()
        || directory.join(".git").is_file()
        || is_regular_file_without_symlinks(&directory.join(PROJECT_FILE))
        || has_agent_instructions(directory)
}

/// Reads project-local launch preferences when present.
///
/// # Errors
///
/// Returns an error if the marker exists but cannot be read or parsed.
pub fn load(directory: &Path) -> Result<Option<ProjectPreferences>> {
    let path = directory.join(PROJECT_FILE);
    let Some(mut file) = read_project_file(&path)? else {
        return Ok(None);
    };
    file.normalize();
    Ok(Some(file.preferences()))
}

/// Writes the latest successful local launch choice to `.atmux.toml`.
///
/// Existing unknown keys are preserved. The file is replaced atomically in the
/// project directory so an interrupted write never leaves a partial TOML file.
///
/// # Errors
///
/// Returns an error if the preferences cannot be read, serialized, or written.
pub fn remember_launch(directory: &Path, session_name: &str, profile: &AgentProfile) -> Result<()> {
    let path = directory.join(PROJECT_FILE);
    let mut file = read_project_file(&path)?.unwrap_or_default();
    file.session_name = normalize_value(Some(session_name.to_owned()));
    file.harness = normalize_value(Some(profile.harness.clone()));
    file.profile = normalize_value(Some(profile.name.clone()));
    let rendered = toml::to_string_pretty(&file).context("failed to encode project preferences")?;
    let (temporary, mut temporary_file) = create_temporary_file(&path)?;
    if let Err(error) = temporary_file.write_all(rendered.as_bytes()) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to write {}", temporary.display()));
    }
    drop(temporary_file);
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to replace {}", path.display()));
    }
    Ok(())
}

fn is_regular_file_without_symlinks(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn read_project_file(path: &Path) -> Result<Option<ProjectFile>> {
    let Some(file) = open_project_file(path)? else {
        return Ok(None);
    };
    let length = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .len();
    anyhow::ensure!(
        length <= MAX_PROJECT_FILE_BYTES,
        "{} exceeds the project preference size limit",
        path.display()
    );
    let mut source = String::with_capacity(usize::try_from(length).unwrap_or_default());
    file.take(MAX_PROJECT_FILE_BYTES + 1)
        .read_to_string(&mut source)
        .with_context(|| format!("failed to read {}", path.display()))?;
    anyhow::ensure!(
        u64::try_from(source.len()).is_ok_and(|length| length <= MAX_PROJECT_FILE_BYTES),
        "{} exceeds the project preference size limit",
        path.display()
    );
    toml::from_str::<ProjectFile>(&source)
        .with_context(|| format!("failed to parse {}", path.display()))
        .map(Some)
}

fn open_project_file(path: &Path) -> Result<Option<File>> {
    let expected = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    anyhow::ensure!(
        expected.file_type().is_file(),
        "{} must be a regular file, not a symlink or special file",
        path.display()
    );
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    anyhow::ensure!(
        same_file(&expected, &opened),
        "{} changed while it was being opened",
        path.display()
    );
    Ok(Some(file))
}

#[cfg(unix)]
fn same_file(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    expected.dev() == opened.dev() && expected.ino() == opened.ino() && opened.file_type().is_file()
}

#[cfg(not(unix))]
fn same_file(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    expected.file_type().is_file()
        && opened.file_type().is_file()
        && expected.len() == opened.len()
        && expected.modified().ok() == opened.modified().ok()
}

fn create_temporary_file(path: &Path) -> Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(PROJECT_FILE);
    for _ in 0..64 {
        let suffix = TEMPORARY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary =
            path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), suffix));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", temporary.display()));
            }
        }
    }
    anyhow::bail!(
        "could not create an exclusive temporary preference file beside {}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_project() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("atmux-project-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn git_atmux_and_agent_instruction_markers_define_projects() {
        let project = temp_project();
        assert!(!is_project_directory(&project));
        fs::create_dir(project.join(".git")).unwrap();
        assert!(is_project_directory(&project));
        fs::remove_dir_all(project.join(".git")).unwrap();
        fs::write(project.join(PROJECT_FILE), "session_name = 'demo'\n").unwrap();
        assert!(is_project_directory(&project));
        fs::remove_file(project.join(PROJECT_FILE)).unwrap();
        for instruction_file in AGENT_INSTRUCTION_FILES {
            fs::write(project.join(instruction_file), "# Agent instructions\n").unwrap();
            assert!(
                is_project_directory(&project),
                "{instruction_file} should define a launchable project"
            );
            fs::remove_file(project.join(instruction_file)).unwrap();
        }
        fs::write(project.join("agents.MD"), "# Case-insensitive marker\n").unwrap();
        assert!(is_project_directory(&project));
        fs::remove_file(project.join("agents.MD")).unwrap();
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn remembered_launch_round_trips_and_preserves_unknown_keys() {
        let project = temp_project();
        fs::write(project.join(PROJECT_FILE), "custom = 'keep'\n").unwrap();
        let profile = AgentProfile {
            name: "Focused".to_owned(),
            harness: "claude".to_owned(),
            command: "claude".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            modes: Vec::new(),
        };
        remember_launch(&project, "petclinic", &profile).unwrap();
        assert_eq!(
            load(&project).unwrap(),
            Some(ProjectPreferences {
                session_name: Some("petclinic".to_owned()),
                harness: Some("claude".to_owned()),
                profile: Some("Focused".to_owned()),
            })
        );
        let source = fs::read_to_string(project.join(PROJECT_FILE)).unwrap();
        let rendered: toml::Value = toml::from_str(&source).unwrap();
        assert_eq!(rendered["custom"].as_str(), Some("keep"));
        fs::remove_dir_all(project).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_preferences_never_follow_or_replace_a_symlink() {
        use std::os::unix::fs::symlink;

        let project = temp_project();
        let external = temp_project().join("external.toml");
        let canary = "secret = 'external-canary'\n";
        fs::write(&external, canary).unwrap();
        let marker = project.join(PROJECT_FILE);
        symlink(&external, &marker).unwrap();
        assert!(!is_project_directory(&project));
        assert!(load(&project).is_err());
        let profile = AgentProfile {
            name: "Default".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            modes: Vec::new(),
        };
        assert!(remember_launch(&project, "should-not-copy", &profile).is_err());
        assert_eq!(fs::read_to_string(&external).unwrap(), canary);
        assert!(
            fs::symlink_metadata(&marker)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(project).unwrap();
        fs::remove_dir_all(external.parent().unwrap()).unwrap();
    }
}
