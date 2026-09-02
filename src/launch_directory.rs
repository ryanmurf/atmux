//! Owner-side mutations used by the bounded launch-directory browser.
//!
//! Every operation starts from an existing directory which the owning
//! machine has canonicalized against its configured launch roots. New names
//! are single path components, and creation is atomic so an existing file,
//! directory, or symlink is never reused.

use std::{
    ffi::OsStr,
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::config::Config;

const MAX_DIRECTORY_NAME_BYTES: usize = 240;
const MAX_REPOSITORY_BYTES: usize = 4_096;
const MAX_GIT_ERROR_BYTES: usize = 4_096;
const GIT_CLONE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    Invalid,
    Conflict,
    Internal,
}

#[derive(Debug)]
pub(crate) struct ActionError {
    kind: ErrorKind,
    message: String,
}

impl ActionError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Invalid,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Conflict,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: message.into(),
        }
    }

    pub(crate) const fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ActionError {}

/// Creates one real, previously absent child below an allowed directory.
pub(crate) fn create_folder(
    config: &Config,
    parent: &str,
    name: &str,
) -> Result<PathBuf, ActionError> {
    let parent = resolve_parent(config, parent)?;
    let name = validate_child_name(name)?;
    let target = parent.join(name);
    ensure_target_absent(&target)?;
    fs::create_dir(&target).map_err(|error| create_error(&target, &error))?;
    match revalidate_created_directory(config, &target) {
        Ok(target) => Ok(target),
        Err(error) => {
            let _ = fs::remove_dir(&target);
            Err(error)
        }
    }
}

/// Clones one repository into a new child below an allowed directory.
pub(crate) fn clone_repository(
    config: &Config,
    parent: &str,
    repository: &str,
    destination: Option<&str>,
) -> Result<PathBuf, ActionError> {
    clone_repository_with_program(
        config,
        parent,
        repository,
        destination,
        OsStr::new("git"),
        GIT_CLONE_TIMEOUT,
    )
}

fn clone_repository_with_program(
    config: &Config,
    parent: &str,
    repository: &str,
    destination: Option<&str>,
    git: &OsStr,
    timeout: Duration,
) -> Result<PathBuf, ActionError> {
    let parent = resolve_parent(config, parent)?;
    let repository = validate_repository(repository)?;
    let destination =
        if let Some(destination) = destination.map(str::trim).filter(|value| !value.is_empty()) {
            validate_child_name(destination)?.to_owned()
        } else {
            let derived = repository_destination(repository)?;
            validate_child_name(&derived)?;
            derived
        };
    let target = parent.join(&destination);
    ensure_target_absent(&target)?;
    fs::create_dir(&target).map_err(|error| create_error(&target, &error))?;
    let target = match revalidate_created_directory(config, &target) {
        Ok(target) => target,
        Err(error) => {
            let _ = fs::remove_dir(&target);
            return Err(error);
        }
    };

    let result = run_git_clone(git, repository, &target, timeout);
    match result {
        Ok(()) => revalidate_created_directory(config, &target).inspect_err(|_| {
            let _ = fs::remove_dir_all(&target);
        }),
        Err(error) => match fs::remove_dir_all(&target) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(ActionError::internal(format!(
                "git clone failed and its incomplete destination could not be removed: {cleanup}"
            ))),
        },
    }
}

fn resolve_parent(config: &Config, parent: &str) -> Result<PathBuf, ActionError> {
    let parent = parent.trim();
    if parent.is_empty()
        || parent.len() > 4_096
        || parent.chars().any(char::is_control)
        || !Path::new(parent).is_absolute()
    {
        return Err(ActionError::invalid(
            "destination directory must be an absolute, readable path",
        ));
    }
    config
        .resolve_launch_directory(Path::new(parent))
        .ok_or_else(|| ActionError::invalid("destination directory is outside the allowed roots"))
}

fn validate_child_name(name: &str) -> Result<&str, ActionError> {
    let name = name.trim();
    let mut components = Path::new(name).components();
    let one_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if name.is_empty()
        || name.len() > MAX_DIRECTORY_NAME_BYTES
        || name.chars().any(char::is_control)
        || name.starts_with('-')
        || name.contains(['/', '\\'])
        || !one_component
    {
        return Err(ActionError::invalid(
            "folder name must be one non-option path component",
        ));
    }
    Ok(name)
}

fn validate_repository(repository: &str) -> Result<&str, ActionError> {
    let repository = repository.trim();
    if repository.is_empty()
        || repository.len() > MAX_REPOSITORY_BYTES
        || repository.chars().any(char::is_control)
        || repository.starts_with('-')
    {
        return Err(ActionError::invalid(
            "repository must be a bounded URL and cannot begin with '-'",
        ));
    }
    let supported_url = repository
        .split_once("://")
        .is_some_and(|(scheme, rest)| match scheme {
            "https" => {
                let authority = rest.split('/').next().unwrap_or_default();
                !authority.is_empty()
                    && !authority.contains('@')
                    && !authority.chars().any(char::is_whitespace)
                    && !repository.contains(['?', '#'])
            }
            "ssh" => valid_ssh_repository(rest, repository),
            _ => false,
        });
    let supported_scp = valid_scp_repository(repository);
    if !supported_url && !supported_scp {
        return Err(ActionError::invalid(
            "repository must use credential-free https, ssh, or git@host:path syntax",
        ));
    }
    Ok(repository)
}

fn valid_ssh_repository(rest: &str, repository: &str) -> bool {
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty()
        || authority.starts_with('-')
        || authority.chars().any(char::is_whitespace)
        || repository.contains(['?', '#'])
    {
        return false;
    }
    match authority.rsplit_once('@') {
        Some((user, host)) => {
            !user.is_empty()
                && !user.starts_with('-')
                && !user.contains(['@', ':'])
                && !host.is_empty()
                && !host.starts_with('-')
        }
        None => !authority.starts_with('-'),
    }
}

fn valid_scp_repository(repository: &str) -> bool {
    repository
        .split_once(':')
        .and_then(|(owner, path)| owner.split_once('@').map(|(user, host)| (user, host, path)))
        .is_some_and(|(user, host, path)| {
            !user.is_empty()
                && !user.starts_with('-')
                && !user.contains('@')
                && !host.is_empty()
                && !host.starts_with('-')
                && !host.contains(['/', '@'])
                && !host.chars().any(char::is_whitespace)
                && !path.is_empty()
                && !path.starts_with('-')
        })
}

fn repository_destination(repository: &str) -> Result<String, ActionError> {
    let repository = repository
        .split(['?', '#'])
        .next()
        .unwrap_or(repository)
        .trim_end_matches('/');
    let name = repository
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default()
        .strip_suffix(".git")
        .unwrap_or_else(|| repository.rsplit(['/', ':']).next().unwrap_or_default())
        .trim();
    if name.is_empty() {
        return Err(ActionError::invalid(
            "repository URL does not contain a destination folder name",
        ));
    }
    Ok(name.to_owned())
}

fn ensure_target_absent(target: &Path) -> Result<(), ActionError> {
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(ActionError::conflict(
            "destination already exists; choose a new folder name",
        )),
        Err(error) => Err(ActionError::internal(format!(
            "destination could not be inspected: {error}"
        ))),
    }
}

fn create_error(target: &Path, error: &io::Error) -> ActionError {
    if error.kind() == io::ErrorKind::AlreadyExists {
        ActionError::conflict("destination already exists; choose a new folder name")
    } else {
        ActionError::internal(format!("failed to create {}: {error}", target.display()))
    }
}

fn revalidate_created_directory(config: &Config, target: &Path) -> Result<PathBuf, ActionError> {
    let resolved = config
        .resolve_launch_directory(target)
        .ok_or_else(|| ActionError::internal("created directory failed its owner policy check"))?;
    let metadata = fs::symlink_metadata(&resolved).map_err(|error| {
        ActionError::internal(format!("created directory disappeared: {error}"))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ActionError::internal(
            "created destination is not a real directory",
        ));
    }
    Ok(resolved)
}

fn run_git_clone(
    git: &OsStr,
    repository: &str,
    target: &Path,
    timeout: Duration,
) -> Result<(), ActionError> {
    let mut child = Command::new(git)
        .arg("clone")
        .arg("--")
        .arg(repository)
        .arg(target)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ActionError::internal(format!("could not start git clone: {error}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ActionError::internal("git clone did not expose an error stream"))?;
    let reader = thread::spawn(move || read_bounded_and_drain(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(ActionError::internal("git clone timed out"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(ActionError::internal(format!(
                    "could not wait for git clone: {error}"
                )));
            }
        }
    };
    let stderr = reader
        .join()
        .map_err(|_| ActionError::internal("git clone error reader panicked"))?;
    let status = status?;
    if status.success() {
        return Ok(());
    }
    Err(ActionError::invalid(git_failure_message(
        status, &stderr, repository,
    )))
}

fn read_bounded_and_drain(mut reader: impl Read) -> Vec<u8> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 1_024];
    while let Ok(read) = reader.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let remaining = MAX_GIT_ERROR_BYTES.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    retained
}

fn git_failure_message(status: ExitStatus, stderr: &[u8], repository: &str) -> String {
    let detail = String::from_utf8_lossy(stderr)
        .replace(repository, "<repository>")
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    let detail = detail.trim();
    if detail.is_empty() {
        format!("git clone failed with {status}")
    } else {
        format!("git clone failed: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NONCE: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        config: Config,
    }

    impl Fixture {
        fn new() -> Self {
            let base = std::env::temp_dir().join(format!(
                "atmux-launch-directory-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ));
            let root = base.join("projects with spaces");
            fs::create_dir_all(&root).unwrap();
            let mut config = Config::default();
            config.general.project_roots = vec![root.clone()];
            config.general.favorite_dirs.clear();
            Self { base, root, config }
        }

        fn fake_git(&self, success: bool) -> PathBuf {
            let script = self.base.join(if success { "git-ok" } else { "git-fail" });
            let arguments_path = self.base.join("args").display().to_string();
            let arguments = shell_words::quote(&arguments_path);
            let body = if success {
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" >{arguments}\ntouch \"$4/cloned-marker\"\n"
                )
            } else {
                "#!/bin/sh\nprintf 'fatal: could not clone %s\\n' \"$3\" >&2\ntouch \"$4/partial\"\nexit 19\n".to_owned()
            };
            fs::write(&script, body).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
            script
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn folder_creation_allows_spaces_and_rejects_traversal_and_symlink_escape() {
        let fixture = Fixture::new();
        let created = create_folder(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "new folder",
        )
        .unwrap();
        assert_eq!(
            created,
            fixture.root.join("new folder").canonicalize().unwrap()
        );

        for invalid in [
            "../escape",
            "child/name",
            "child\\name",
            "-option",
            ".",
            "..",
        ] {
            let error = create_folder(&fixture.config, fixture.root.to_str().unwrap(), invalid)
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Invalid, "{invalid}: {error}");
        }

        let outside = fixture.base.join("outside");
        fs::create_dir(&outside).unwrap();
        let escape = fixture.root.join("escape");
        symlink(&outside, &escape).unwrap();
        let error =
            create_folder(&fixture.config, escape.to_str().unwrap(), "escaped child").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Invalid);
        assert!(!outside.join("escaped child").exists());
    }

    #[test]
    fn clone_uses_literal_argv_derives_destination_and_cleans_failures() {
        let fixture = Fixture::new();
        let args = fixture.base.join("args");
        let repository = "https://example.test/team/demo repo.git";
        let cloned = clone_repository_with_program(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            repository,
            None,
            fixture.fake_git(true).as_os_str(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(cloned.ends_with("demo repo"));
        assert!(cloned.join("cloned-marker").is_file());
        assert_eq!(
            fs::read_to_string(args)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["clone", "--", repository, cloned.to_str().unwrap()]
        );

        let failed = clone_repository_with_program(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "ssh://git@example.test/team/failure.git",
            Some("failed clone"),
            fixture.fake_git(false).as_os_str(),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(failed.kind(), ErrorKind::Invalid);
        assert!(!failed.to_string().contains("ssh://git@example.test"));
        assert!(!fixture.root.join("failed clone").exists());
    }

    #[test]
    fn clone_rejects_option_inputs_and_every_existing_destination() {
        let fixture = Fixture::new();
        for (repository, destination) in [
            ("--upload-pack=evil", None),
            ("https://example.test/team/repo.git", Some("--config=evil")),
            ("relative/repo", Some("safe")),
            ("ext::sh -c evil", Some("safe")),
            ("file:///tmp/repo", Some("safe")),
            ("/tmp/local-repo", Some("safe")),
            ("https://token@example.test/team/repo.git", Some("safe")),
            (
                "https://oauth2:super-secret@example.test/team/repo.git",
                Some("safe"),
            ),
            ("ssh://git:secret@example.test/team/repo.git", Some("safe")),
            ("ssh://git@-oProxyCommand=evil/team/repo.git", Some("safe")),
            ("git@-oProxyCommand=evil:team/repo.git", Some("safe")),
            ("git@example.test:--upload-pack=evil", Some("safe")),
            (
                "https://example.test/team/repo.git?token=secret",
                Some("safe"),
            ),
            (
                "https://example.test/team/repo.git\n--upload-pack=evil",
                Some("safe"),
            ),
        ] {
            let error = clone_repository_with_program(
                &fixture.config,
                fixture.root.to_str().unwrap(),
                repository,
                destination,
                OsStr::new("git"),
                Duration::from_millis(1),
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Invalid, "{repository}: {error}");
        }

        fs::create_dir(fixture.root.join("existing empty")).unwrap();
        fs::create_dir(fixture.root.join("existing full")).unwrap();
        fs::write(fixture.root.join("existing full/file"), "keep").unwrap();
        for destination in ["existing empty", "existing full"] {
            let error = clone_repository_with_program(
                &fixture.config,
                fixture.root.to_str().unwrap(),
                "https://example.test/team/repo.git",
                Some(destination),
                OsStr::new("git"),
                Duration::from_millis(1),
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Conflict);
        }
        assert_eq!(
            fs::read_to_string(fixture.root.join("existing full/file")).unwrap(),
            "keep"
        );

        for supported in [
            "https://example.test/team/repo.git",
            "ssh://git@example.test/team/repo.git",
            "git@example.test:team/repo.git",
        ] {
            assert_eq!(validate_repository(supported).unwrap(), supported);
        }
    }
}
