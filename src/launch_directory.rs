//! Owner-side mutations used by the bounded launch-directory browser.
//!
//! Every operation starts from an existing directory which the owning
//! machine has canonicalized against its configured launch roots. New names
//! are single path components, and creation is atomic so an existing file,
//! directory, or symlink is never reused.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata},
    io::{self, Read},
    os::unix::{fs::MetadataExt as _, io::AsRawFd as _, process::CommandExt as _},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::{
    fs::{AtFlags, Mode, OFlags, RenameFlags},
    process::{Pid, Signal, kill_process_group, test_kill_process_group},
};

use crate::config::Config;

const MAX_DIRECTORY_NAME_BYTES: usize = 240;
const MAX_REPOSITORY_BYTES: usize = 4_096;
const MAX_GIT_ERROR_BYTES: usize = 4_096;
const MAX_STDERR_READS_PER_TICK: usize = 64;
const GIT_CLONE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(250);
const PROCESS_GROUP_KILL_WAIT: Duration = Duration::from_secs(1);
const STAGING_ATTEMPTS: usize = 64;
const HEX: &[u8; 16] = b"0123456789abcdef";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn from_metadata(metadata: &Metadata) -> Result<Self, ActionError> {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ActionError::invalid(
                "destination directory is not a real directory",
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

struct HeldDirectory {
    path: PathBuf,
    handle: File,
    identity: DirectoryIdentity,
}

struct StagingDirectory {
    name: OsString,
    handle: File,
    identity: DirectoryIdentity,
}

/// Creates one real, previously absent child below an allowed directory.
pub(crate) fn create_folder(
    config: &Config,
    parent: &str,
    name: &str,
) -> Result<PathBuf, ActionError> {
    create_folder_with_hook(config, parent, name, None)
}

fn create_folder_with_hook(
    config: &Config,
    parent: &str,
    name: &str,
    before_publish: Option<&dyn Fn()>,
) -> Result<PathBuf, ActionError> {
    let parent = resolve_parent(config, parent)?;
    let destination = validate_child_name(name)?;
    ensure_target_absent_at(&parent.handle, OsStr::new(destination))?;
    let staging = create_staging_directory(&parent)?;
    rustix::fs::fchmod(&staging.handle, Mode::from_raw_mode(0o755)).map_err(|_| {
        let _ = cleanup_staging_directory(&parent, &staging);
        ActionError::internal("new folder permissions could not be finalized")
    })?;
    if let Some(hook) = before_publish {
        hook();
    }
    publish_staging(config, &parent, &staging, destination)
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
    clone_repository_with_program_and_hook(
        config,
        parent,
        repository,
        destination,
        git,
        timeout,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn clone_repository_with_program_and_hook(
    config: &Config,
    parent: &str,
    repository: &str,
    destination: Option<&str>,
    git: &OsStr,
    timeout: Duration,
    before_publish: Option<&dyn Fn()>,
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
    ensure_target_absent_at(&parent.handle, OsStr::new(&destination))?;
    let staging = create_staging_directory(&parent)?;
    if let Err(error) = run_git_clone(git, repository, &staging.handle, timeout) {
        return match cleanup_staging_directory(&parent, &staging) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(ActionError::internal(format!(
                "git clone failed and its private staging directory could not be removed: {cleanup}"
            ))),
        };
    }
    if let Some(hook) = before_publish {
        hook();
    }
    publish_staging(config, &parent, &staging, &destination)
}

fn resolve_parent(config: &Config, parent: &str) -> Result<HeldDirectory, ActionError> {
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
    let path = config
        .resolve_launch_directory(Path::new(parent))
        .ok_or_else(|| {
            ActionError::invalid("destination directory is outside the allowed roots")
        })?;
    let handle = open_absolute_directory(&path)?;
    let identity = DirectoryIdentity::from_metadata(
        &handle
            .metadata()
            .map_err(|_| ActionError::invalid("destination directory could not be inspected"))?,
    )?;
    Ok(HeldDirectory {
        path,
        handle,
        identity,
    })
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

fn open_absolute_directory(path: &Path) -> Result<File, ActionError> {
    let expected = fs::symlink_metadata(path)
        .map_err(|_| ActionError::invalid("destination directory is unavailable"))?;
    let expected_identity = DirectoryIdentity::from_metadata(&expected)?;
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
    let mut current = rustix::fs::open("/", flags, Mode::empty())
        .map(File::from)
        .map_err(|_| ActionError::invalid("destination directory could not be opened safely"))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = rustix::fs::openat(&current, name, flags, Mode::empty())
                    .map(File::from)
                    .map_err(|_| {
                        ActionError::invalid("destination directory could not be opened safely")
                    })?;
            }
            _ => {
                return Err(ActionError::invalid(
                    "destination directory could not be opened safely",
                ));
            }
        }
    }
    let actual = DirectoryIdentity::from_metadata(
        &current
            .metadata()
            .map_err(|_| ActionError::invalid("destination directory could not be inspected"))?,
    )?;
    if actual != expected_identity {
        return Err(ActionError::invalid(
            "destination directory changed while it was being opened",
        ));
    }
    Ok(current)
}

fn open_child_directory(
    parent: &File,
    name: &OsStr,
) -> Result<(File, DirectoryIdentity), ActionError> {
    let before = named_directory_identity(parent, name)
        .map_err(|_| ActionError::internal("private staging directory could not be inspected"))?;
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
    let handle = rustix::fs::openat(parent, name, flags, Mode::empty())
        .map(File::from)
        .map_err(|_| ActionError::internal("private staging directory could not be opened"))?;
    let after =
        DirectoryIdentity::from_metadata(&handle.metadata().map_err(|_| {
            ActionError::internal("private staging directory could not be inspected")
        })?)?;
    if before != after {
        return Err(ActionError::internal(
            "private staging directory changed while it was being opened",
        ));
    }
    Ok((handle, after))
}

fn named_directory_identity(parent: &File, name: &OsStr) -> rustix::io::Result<DirectoryIdentity> {
    let stat = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(rustix::io::Errno::NOTDIR);
    }
    Ok(DirectoryIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

fn ensure_target_absent_at(parent: &File, destination: &OsStr) -> Result<(), ActionError> {
    match rustix::fs::statat(parent, destination, AtFlags::SYMLINK_NOFOLLOW) {
        Err(error) if error == rustix::io::Errno::NOENT => Ok(()),
        Ok(_) => Err(ActionError::conflict(
            "destination already exists; choose a new folder name",
        )),
        Err(_) => Err(ActionError::internal(
            "destination could not be inspected safely",
        )),
    }
}

fn create_staging_directory(parent: &HeldDirectory) -> Result<StagingDirectory, ActionError> {
    for _ in 0..STAGING_ATTEMPTS {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| ActionError::internal("private staging name could not be generated"))?;
        let mut suffix = String::with_capacity(random.len() * 2);
        for byte in random {
            suffix.push(char::from(HEX[usize::from(byte >> 4)]));
            suffix.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        let name = OsString::from(format!(".atmux-stage-{suffix}"));
        match rustix::fs::mkdirat(&parent.handle, &name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {
                let (handle, identity) = open_child_directory(&parent.handle, &name)?;
                return Ok(StagingDirectory {
                    name,
                    handle,
                    identity,
                });
            }
            Err(error) if error == rustix::io::Errno::EXIST => {}
            Err(_) => {
                return Err(ActionError::internal(
                    "private staging directory could not be created",
                ));
            }
        }
    }
    Err(ActionError::internal(
        "private staging directory name could not be allocated",
    ))
}

fn parent_is_still_authorized(config: &Config, parent: &HeldDirectory) -> bool {
    let Some(current_path) = config.resolve_launch_directory(&parent.path) else {
        return false;
    };
    if current_path != parent.path {
        return false;
    }
    open_absolute_directory(&current_path)
        .and_then(|handle| {
            DirectoryIdentity::from_metadata(&handle.metadata().map_err(|_| {
                ActionError::invalid("destination directory could not be inspected")
            })?)
        })
        .is_ok_and(|identity| identity == parent.identity)
}

fn publish_staging(
    config: &Config,
    parent: &HeldDirectory,
    staging: &StagingDirectory,
    destination: &str,
) -> Result<PathBuf, ActionError> {
    if !parent_is_still_authorized(config, parent) {
        let _ = cleanup_staging_directory(parent, staging);
        return Err(ActionError::conflict(
            "destination directory changed before the operation completed",
        ));
    }
    match rustix::fs::renameat_with(
        &parent.handle,
        &staging.name,
        &parent.handle,
        destination,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST | rustix::io::Errno::NOTEMPTY) => {
            let _ = cleanup_staging_directory(parent, staging);
            return Err(ActionError::conflict(
                "destination already exists; choose a new folder name",
            ));
        }
        Err(_) => {
            let _ = cleanup_staging_directory(parent, staging);
            return Err(ActionError::internal(
                "private staging directory could not be published atomically",
            ));
        }
    }

    let target = parent.path.join(destination);
    let named_matches = named_directory_identity(&parent.handle, OsStr::new(destination))
        .is_ok_and(|identity| identity == staging.identity);
    let resolved_matches = config
        .resolve_launch_directory(&target)
        .is_some_and(|resolved| resolved == target)
        && open_absolute_directory(&target)
            .and_then(|handle| {
                DirectoryIdentity::from_metadata(&handle.metadata().map_err(|_| {
                    ActionError::internal("published directory could not be inspected")
                })?)
            })
            .is_ok_and(|identity| identity == staging.identity);
    if named_matches && resolved_matches {
        return Ok(target);
    }

    cleanup_published_directory(parent, destination, staging);
    Err(ActionError::conflict(
        "destination changed while the operation was being published",
    ))
}

fn descriptor_path(directory: &File) -> PathBuf {
    #[cfg(target_os = "linux")]
    let base = "/proc/self/fd";
    #[cfg(target_vendor = "apple")]
    let base = "/dev/fd";
    Path::new(base).join(directory.as_raw_fd().to_string())
}

fn clear_directory(directory: &File) -> io::Result<()> {
    for entry in fs::read_dir(descriptor_path(directory))? {
        let entry = entry?;
        let path = descriptor_path(directory).join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() && !kind.is_symlink() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn cleanup_staging_directory(
    parent: &HeldDirectory,
    staging: &StagingDirectory,
) -> Result<(), ActionError> {
    clear_directory(&staging.handle)
        .map_err(|_| ActionError::internal("private staging contents could not be removed"))?;
    if !named_directory_identity(&parent.handle, &staging.name)
        .is_ok_and(|identity| identity == staging.identity)
    {
        return Err(ActionError::internal(
            "private staging name no longer identifies the retained directory",
        ));
    }
    rustix::fs::unlinkat(&parent.handle, &staging.name, AtFlags::REMOVEDIR)
        .map_err(|_| ActionError::internal("private staging directory could not be removed"))
}

fn cleanup_published_directory(
    parent: &HeldDirectory,
    destination: &str,
    published: &StagingDirectory,
) {
    let _ = clear_directory(&published.handle);
    if named_directory_identity(&parent.handle, OsStr::new(destination))
        .is_ok_and(|identity| identity == published.identity)
    {
        let _ = rustix::fs::unlinkat(&parent.handle, destination, AtFlags::REMOVEDIR);
    }
}

fn run_git_clone(
    git: &OsStr,
    repository: &str,
    staging: &File,
    timeout: Duration,
) -> Result<(), ActionError> {
    let mut child = Command::new(git)
        .arg("clone")
        .arg("--")
        .arg(repository)
        .arg(".")
        .current_dir(descriptor_path(staging))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| ActionError::internal(format!("could not start git clone: {error}")))?;
    let Some(group) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ActionError::internal(
            "git clone did not expose a process group",
        ));
    };
    let Some(mut stderr) = child.stderr.take() else {
        let _ = abort_started_clone(&mut child, group);
        return Err(ActionError::internal(
            "git clone did not expose an error stream",
        ));
    };
    let Ok(flags) = rustix::fs::fcntl_getfl(&stderr) else {
        let _ = abort_started_clone(&mut child, group);
        return Err(ActionError::internal(
            "git clone error stream could not be inspected",
        ));
    };
    if rustix::fs::fcntl_setfl(&stderr, flags | OFlags::NONBLOCK).is_err() {
        let _ = abort_started_clone(&mut child, group);
        return Err(ActionError::internal(
            "git clone error stream could not be bounded",
        ));
    }
    let mut retained = Vec::new();
    let deadline = Instant::now() + timeout;
    let status = loop {
        drain_available(&mut stderr, &mut retained)?;
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                if !terminate_process_group(&mut child, group, &mut stderr, &mut retained) {
                    return Err(ActionError::internal(
                        "git clone timed out and its process group could not be stopped",
                    ));
                }
                return Err(ActionError::internal("git clone timed out"));
            }
            Err(error) => {
                let _ = terminate_process_group(&mut child, group, &mut stderr, &mut retained);
                return Err(ActionError::internal(format!(
                    "could not wait for git clone: {error}"
                )));
            }
        }
    };
    if test_kill_process_group(group).is_ok()
        && !terminate_process_group(&mut child, group, &mut stderr, &mut retained)
    {
        return Err(ActionError::internal(
            "git clone left a process group that could not be stopped",
        ));
    }
    drain_available(&mut stderr, &mut retained)?;
    if status.success() {
        return Ok(());
    }
    Err(ActionError::invalid(git_failure_message(
        status, &retained, repository,
    )))
}

fn drain_available(reader: &mut impl Read, retained: &mut Vec<u8>) -> Result<(), ActionError> {
    let mut buffer = [0_u8; 1_024];
    for _ in 0..MAX_STDERR_READS_PER_TICK {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                let remaining = MAX_GIT_ERROR_BYTES.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..read.min(remaining)]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(_) => {
                return Err(ActionError::internal(
                    "git clone error stream could not be read",
                ));
            }
        }
    }
    Ok(())
}

fn abort_started_clone(child: &mut std::process::Child, group: Pid) -> bool {
    let _ = kill_process_group(group, Signal::KILL);
    let deadline = Instant::now() + PROCESS_GROUP_KILL_WAIT;
    let mut leader_reaped = false;
    while Instant::now() < deadline {
        leader_reaped |= child.try_wait().is_ok_and(|status| status.is_some());
        if leader_reaped && test_kill_process_group(group).is_err() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    leader_reaped && test_kill_process_group(group).is_err()
}

fn terminate_process_group(
    child: &mut std::process::Child,
    group: Pid,
    stderr: &mut impl Read,
    retained: &mut Vec<u8>,
) -> bool {
    let _ = kill_process_group(group, Signal::TERM);
    let term_deadline = Instant::now() + PROCESS_GROUP_GRACE;
    let mut leader_reaped = child.try_wait().is_ok_and(|status| status.is_some());
    while Instant::now() < term_deadline && test_kill_process_group(group).is_ok() {
        let _ = drain_available(stderr, retained);
        leader_reaped |= child.try_wait().is_ok_and(|status| status.is_some());
        thread::sleep(Duration::from_millis(10));
    }
    if test_kill_process_group(group).is_ok() {
        let _ = kill_process_group(group, Signal::KILL);
    }
    let kill_deadline = Instant::now() + PROCESS_GROUP_KILL_WAIT;
    while Instant::now() < kill_deadline {
        let _ = drain_available(stderr, retained);
        leader_reaped |= child.try_wait().is_ok_and(|status| status.is_some());
        if leader_reaped && test_kill_process_group(group).is_err() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    leader_reaped && test_kill_process_group(group).is_err()
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
            ["clone", "--", repository, "."]
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

    #[test]
    fn held_parent_and_no_replace_publish_resist_parent_and_target_swaps() {
        let fixture = Fixture::new();
        let moved = fixture.base.join("moved allowed root");
        let replacement_marker = fixture.root.join("replacement-marker");
        let swap_parent = || {
            fs::rename(&fixture.root, &moved).unwrap();
            fs::create_dir(&fixture.root).unwrap();
            fs::write(&replacement_marker, "unrelated").unwrap();
        };
        let error = create_folder_with_hook(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "must not escape",
            Some(&swap_parent),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert_eq!(fs::read_to_string(replacement_marker).unwrap(), "unrelated");
        assert!(!moved.join("must not escape").exists());
        assert!(staging_names(&moved).is_empty());

        let fixture = Fixture::new();
        let target = fixture.root.join("raced target");
        let install_target = || {
            fs::create_dir(&target).unwrap();
            fs::write(target.join("keep"), "unrelated").unwrap();
        };
        let error = clone_repository_with_program_and_hook(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "https://example.test/team/repo.git",
            Some("raced target"),
            fixture.fake_git(true).as_os_str(),
            Duration::from_secs(2),
            Some(&install_target),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert_eq!(
            fs::read_to_string(target.join("keep")).unwrap(),
            "unrelated"
        );
        assert!(staging_names(&fixture.root).is_empty());
    }

    #[test]
    fn timeout_kills_descendants_retaining_stderr_and_cleans_staging() {
        let fixture = Fixture::new();
        let script = fixture.base.join("git-hangs-with-descendant");
        let pid_file = fixture.base.join("descendant.pid");
        let pid_file_text = pid_file.display().to_string();
        let quoted_pid_file = shell_words::quote(&pid_file_text);
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n(trap '' TERM; while :; do printf 0123456789abcdef >&2; done) &\nprintf '%s' \"$!\" >{quoted_pid_file}\nwait\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        let error = clone_repository_with_program(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "https://example.test/team/hanging.git",
            Some("hanging clone"),
            script.as_os_str(),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Internal);
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!fixture.root.join("hanging clone").exists());
        assert!(staging_names(&fixture.root).is_empty());

        let raw_pid = fs::read_to_string(pid_file).unwrap();
        let pid = Pid::from_raw(raw_pid.parse().unwrap()).unwrap();
        for _ in 0..100 {
            if rustix::process::test_kill_process(pid).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            rustix::process::test_kill_process(pid).is_err(),
            "timeout left a descendant alive"
        );
    }

    fn staging_names(parent: &Path) -> Vec<OsString> {
        fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with(".atmux-stage-"))
            .collect()
    }
}
