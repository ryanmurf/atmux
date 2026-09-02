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
const PAYLOAD_NAME: &str = "payload";

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

#[derive(Debug)]
struct StagingDirectory {
    parent: File,
    name: OsString,
    handle: File,
    identity: DirectoryIdentity,
    payload_identity: Option<DirectoryIdentity>,
    cleanup_armed: bool,
}

impl StagingDirectory {
    fn cleanup(mut self) -> Result<(), ActionError> {
        self.cleanup_armed = false;
        cleanup_staging_parts(
            &self.parent,
            &self.name,
            &self.handle,
            self.identity,
            self.payload_identity,
        )
    }

    fn disarm_cleanup(&mut self) {
        self.cleanup_armed = false;
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if self.cleanup_armed {
            let _ = cleanup_staging_parts(
                &self.parent,
                &self.name,
                &self.handle,
                self.identity,
                self.payload_identity,
            );
        }
    }
}

struct PendingStagingName<'a> {
    parent: &'a File,
    name: OsString,
    armed: bool,
}

impl Drop for PendingStagingName<'_> {
    fn drop(&mut self) {
        if self.armed {
            // The random entry was just created empty with mode 0700. Anchoring
            // rollback to the retained parent avoids re-resolving a mutable
            // absolute path after a post-mkdir open/stat failure.
            let _ = rustix::fs::unlinkat(self.parent, &self.name, AtFlags::REMOVEDIR);
        }
    }
}

struct PayloadDirectory {
    handle: File,
    identity: DirectoryIdentity,
}

#[derive(Debug)]
struct CloneRunError {
    error: ActionError,
    cleanup_safe: bool,
}

struct RunningClone {
    child: std::process::Child,
    group: Pid,
    stderr: std::process::ChildStderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StagingFault {
    BeforeOpen,
    BeforeMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloneRunFault {
    DrainAfterSpawn,
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
    let mut staging = create_staging_directory(&parent)?;
    let payload = create_payload_directory(&mut staging)?;
    if let Some(hook) = before_publish {
        hook();
    }
    publish_payload(config, &parent, &staging, &payload, destination)
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
    let mut staging = create_staging_directory(&parent)?;
    let payload = create_payload_directory(&mut staging)?;
    if let Err(failure) = run_git_clone(git, repository, &staging.handle, timeout) {
        if !failure.cleanup_safe {
            staging.disarm_cleanup();
            return Err(failure.error);
        }
        return match staging.cleanup() {
            Ok(()) => Err(failure.error),
            Err(cleanup) => Err(ActionError::internal(format!(
                "git clone failed and its private staging directory could not be removed: {cleanup}"
            ))),
        };
    }
    if let Some(hook) = before_publish {
        hook();
    }
    publish_payload(config, &parent, &staging, &payload, &destination)
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
    fault: Option<StagingFault>,
) -> Result<(File, DirectoryIdentity), ActionError> {
    let before = named_directory_identity(parent, name)
        .map_err(|_| ActionError::internal("private staging directory could not be inspected"))?;
    if fault == Some(StagingFault::BeforeOpen) {
        return Err(ActionError::internal(
            "injected private staging open failure",
        ));
    }
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
    let handle = rustix::fs::openat(parent, name, flags, Mode::empty())
        .map(File::from)
        .map_err(|_| ActionError::internal("private staging directory could not be opened"))?;
    if fault == Some(StagingFault::BeforeMetadata) {
        return Err(ActionError::internal(
            "injected private staging metadata failure",
        ));
    }
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
    create_staging_directory_with_fault(parent, None)
}

fn create_staging_directory_with_fault(
    parent: &HeldDirectory,
    fault: Option<StagingFault>,
) -> Result<StagingDirectory, ActionError> {
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
                let mut pending = PendingStagingName {
                    parent: &parent.handle,
                    name,
                    armed: true,
                };
                let (handle, identity) =
                    open_child_directory(&parent.handle, &pending.name, fault)?;
                let parent_handle = parent.handle.try_clone().map_err(|_| {
                    ActionError::internal("private staging parent could not be retained")
                })?;
                pending.armed = false;
                return Ok(StagingDirectory {
                    parent: parent_handle,
                    name: std::mem::take(&mut pending.name),
                    handle,
                    identity,
                    payload_identity: None,
                    cleanup_armed: true,
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

fn create_payload_directory(
    staging: &mut StagingDirectory,
) -> Result<PayloadDirectory, ActionError> {
    rustix::fs::mkdirat(&staging.handle, PAYLOAD_NAME, Mode::from_raw_mode(0o777))
        .map_err(|_| ActionError::internal("private payload directory could not be created"))?;
    let (handle, identity) = open_child_directory(&staging.handle, OsStr::new(PAYLOAD_NAME), None)?;
    staging.payload_identity = Some(identity);
    Ok(PayloadDirectory { handle, identity })
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

fn publish_payload(
    config: &Config,
    parent: &HeldDirectory,
    staging: &StagingDirectory,
    payload: &PayloadDirectory,
    destination: &str,
) -> Result<PathBuf, ActionError> {
    if !parent_is_still_authorized(config, parent) {
        return Err(ActionError::conflict(
            "destination directory changed before the operation completed",
        ));
    }
    match rustix::fs::renameat_with(
        &staging.handle,
        PAYLOAD_NAME,
        &parent.handle,
        destination,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST | rustix::io::Errno::NOTEMPTY) => {
            return Err(ActionError::conflict(
                "destination already exists; choose a new folder name",
            ));
        }
        Err(_) => {
            return Err(ActionError::internal(
                "private payload directory could not be published atomically",
            ));
        }
    }

    let target = parent.path.join(destination);
    let named_matches = named_directory_identity(&parent.handle, OsStr::new(destination))
        .is_ok_and(|identity| identity == payload.identity);
    let resolved_matches = config
        .resolve_launch_directory(&target)
        .is_some_and(|resolved| resolved == target)
        && open_absolute_directory(&target)
            .and_then(|handle| {
                DirectoryIdentity::from_metadata(&handle.metadata().map_err(|_| {
                    ActionError::internal("published directory could not be inspected")
                })?)
            })
            .is_ok_and(|identity| identity == payload.identity);
    if named_matches && resolved_matches {
        return Ok(target);
    }

    cleanup_published_directory(payload);
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

fn clear_directory(directory: &File) -> Result<(), ActionError> {
    let entries = fs::read_dir(descriptor_path(directory))
        .map_err(|_| ActionError::internal("private directory could not be inspected"))?;
    for entry in entries {
        let entry =
            entry.map_err(|_| ActionError::internal("private directory could not be inspected"))?;
        let name = entry.file_name();
        let stat = rustix::fs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| ActionError::internal("private entry could not be inspected"))?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir() {
            let (child, identity) = open_child_directory(directory, &name, None)?;
            clear_directory(&child)?;
            if !named_directory_identity(directory, &name).is_ok_and(|current| current == identity)
            {
                return Err(ActionError::internal(
                    "private directory entry changed during cleanup",
                ));
            }
            rustix::fs::unlinkat(directory, &name, AtFlags::REMOVEDIR)
                .map_err(|_| ActionError::internal("private directory could not be removed"))?;
        } else {
            rustix::fs::unlinkat(directory, &name, AtFlags::empty())
                .map_err(|_| ActionError::internal("private file could not be removed"))?;
        }
    }
    Ok(())
}

fn cleanup_staging_parts(
    parent: &File,
    name: &OsStr,
    staging: &File,
    identity: DirectoryIdentity,
    expected_payload: Option<DirectoryIdentity>,
) -> Result<(), ActionError> {
    let entries = fs::read_dir(descriptor_path(staging))
        .map_err(|_| ActionError::internal("private staging directory could not be inspected"))?
        .collect::<io::Result<Vec<_>>>()
        .map_err(|_| ActionError::internal("private staging directory could not be inspected"))?;
    if entries
        .iter()
        .any(|entry| entry.file_name() != OsStr::new(PAYLOAD_NAME))
    {
        // The random mode-0700 stage is a same-UID boundary, not protection
        // from another process running as the owner. Unexpected top-level
        // entries therefore fail closed and remain for manual inspection.
        return Err(ActionError::internal(
            "private staging directory contained an unexpected entry",
        ));
    }
    if !entries.is_empty() {
        let (payload, payload_identity) =
            open_child_directory(staging, OsStr::new(PAYLOAD_NAME), None)?;
        if Some(payload_identity) != expected_payload {
            return Err(ActionError::internal(
                "private payload name no longer identifies the retained directory",
            ));
        }
        clear_directory(&payload)?;
        if !named_directory_identity(staging, OsStr::new(PAYLOAD_NAME))
            .is_ok_and(|current| current == payload_identity)
        {
            return Err(ActionError::internal(
                "private payload changed during cleanup",
            ));
        }
        rustix::fs::unlinkat(staging, PAYLOAD_NAME, AtFlags::REMOVEDIR)
            .map_err(|_| ActionError::internal("private payload could not be removed"))?;
    }
    if !named_directory_identity(parent, name).is_ok_and(|current| current == identity) {
        return Err(ActionError::internal(
            "private staging name no longer identifies the retained directory",
        ));
    }
    rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
        .map_err(|_| ActionError::internal("private staging directory could not be removed"))
}

fn cleanup_published_directory(published: &PayloadDirectory) {
    // Once publication occurred, never recursively resolve or unlink the
    // destination name during error recovery. Empty only the retained object;
    // a concurrent rename can at worst leave that owned directory as residue,
    // never cause an unrelated replacement to be deleted.
    let _ = clear_directory(&published.handle);
}

fn run_git_clone(
    git: &OsStr,
    repository: &str,
    staging: &File,
    timeout: Duration,
) -> Result<(), CloneRunError> {
    run_git_clone_with_fault(git, repository, staging, timeout, None)
}

fn run_git_clone_with_fault(
    git: &OsStr,
    repository: &str,
    staging: &File,
    timeout: Duration,
    fault: Option<CloneRunFault>,
) -> Result<(), CloneRunError> {
    let RunningClone {
        mut child,
        group,
        mut stderr,
    } = start_git_clone(git, repository, staging)?;
    let mut retained = Vec::new();
    if fault == Some(CloneRunFault::DrainAfterSpawn) {
        thread::sleep(Duration::from_millis(50));
        let cleanup_safe = terminate_process_group(&mut child, group, &mut stderr, &mut retained);
        return Err(CloneRunError {
            error: ActionError::internal("injected git clone error stream failure"),
            cleanup_safe,
        });
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Err(error) = drain_available(&mut stderr, &mut retained) {
            let cleanup_safe =
                terminate_process_group(&mut child, group, &mut stderr, &mut retained);
            return Err(CloneRunError {
                error,
                cleanup_safe,
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                if !terminate_process_group(&mut child, group, &mut stderr, &mut retained) {
                    return Err(CloneRunError {
                        error: ActionError::internal(
                            "git clone timed out and its process group could not be stopped",
                        ),
                        cleanup_safe: false,
                    });
                }
                return Err(CloneRunError {
                    error: ActionError::internal("git clone timed out"),
                    cleanup_safe: true,
                });
            }
            Err(error) => {
                let cleanup_safe =
                    terminate_process_group(&mut child, group, &mut stderr, &mut retained);
                return Err(CloneRunError {
                    error: ActionError::internal(format!("could not wait for git clone: {error}")),
                    cleanup_safe,
                });
            }
        }
    };
    if test_kill_process_group(group).is_ok()
        && !terminate_process_group(&mut child, group, &mut stderr, &mut retained)
    {
        return Err(CloneRunError {
            error: ActionError::internal(
                "git clone left a process group that could not be stopped",
            ),
            cleanup_safe: false,
        });
    }
    drain_available(&mut stderr, &mut retained).map_err(|error| CloneRunError {
        error,
        cleanup_safe: true,
    })?;
    if status.success() {
        return Ok(());
    }
    Err(CloneRunError {
        error: ActionError::invalid(git_failure_message(status, &retained, repository)),
        cleanup_safe: true,
    })
}

fn start_git_clone(
    git: &OsStr,
    repository: &str,
    staging: &File,
) -> Result<RunningClone, CloneRunError> {
    let mut child = Command::new(git)
        .arg("clone")
        .arg("--")
        .arg(repository)
        .arg(PAYLOAD_NAME)
        .current_dir(descriptor_path(staging))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| CloneRunError {
            error: ActionError::internal(format!("could not start git clone: {error}")),
            cleanup_safe: true,
        })?;
    let Some(group) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CloneRunError {
            error: ActionError::internal("git clone did not expose a process group"),
            cleanup_safe: false,
        });
    };
    let Some(stderr) = child.stderr.take() else {
        let cleanup_safe = abort_started_clone(&mut child, group);
        return Err(CloneRunError {
            error: ActionError::internal("git clone did not expose an error stream"),
            cleanup_safe,
        });
    };
    let Ok(flags) = rustix::fs::fcntl_getfl(&stderr) else {
        let cleanup_safe = abort_started_clone(&mut child, group);
        return Err(CloneRunError {
            error: ActionError::internal("git clone error stream could not be inspected"),
            cleanup_safe,
        });
    };
    if rustix::fs::fcntl_setfl(&stderr, flags | OFlags::NONBLOCK).is_err() {
        let cleanup_safe = abort_started_clone(&mut child, group);
        return Err(CloneRunError {
            error: ActionError::internal("git clone error stream could not be bounded"),
            cleanup_safe,
        });
    }
    Ok(RunningClone {
        child,
        group,
        stderr,
    })
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

    fn checked_test_mode<T>(mode: u32) -> Option<T>
    where
        T: TryFrom<u32>,
    {
        T::try_from(mode).ok()
    }

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
            ["clone", "--", repository, PAYLOAD_NAME]
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
    fn real_git_clones_into_the_retained_precreated_payload() {
        let fixture = Fixture::new();
        let source = fixture.base.join("source.git");
        let status = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success());

        let parent = resolve_parent(&fixture.config, fixture.root.to_str().unwrap()).unwrap();
        let mut staging = create_staging_directory(&parent).unwrap();
        let payload = create_payload_directory(&mut staging).unwrap();
        run_git_clone(
            OsStr::new("git"),
            source.to_str().unwrap(),
            &staging.handle,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(descriptor_path(&payload.handle).join(".git").is_dir());
        staging.cleanup().unwrap();
        assert!(staging_names(&fixture.root).is_empty());
    }

    #[test]
    fn failed_clone_never_adopts_a_swapped_payload_for_cleanup() {
        let fixture = Fixture::new();
        let script = fixture.base.join("git-swaps-payload");
        fs::write(
            &script,
            "#!/bin/sh\nmv \"$4\" ../moved-owned-payload\nmkdir \"$4\"\nprintf unrelated > \"$4/replacement-marker\"\nexit 19\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let error = clone_repository_with_program(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "https://example.test/team/swapped.git",
            Some("must not publish"),
            script.as_os_str(),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Internal);
        assert!(error.to_string().contains("could not be removed"));
        assert!(!fixture.root.join("must not publish").exists());
        assert!(fixture.root.join("moved-owned-payload").is_dir());
        let stages = staging_names(&fixture.root);
        assert_eq!(stages.len(), 1, "swapped staging must fail closed");
        assert_eq!(
            fs::read_to_string(
                fixture
                    .root
                    .join(&stages[0])
                    .join(PAYLOAD_NAME)
                    .join("replacement-marker")
            )
            .unwrap(),
            "unrelated"
        );
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

    #[test]
    fn post_mkdir_open_and_metadata_failures_roll_back_only_staging() {
        let fixture = Fixture::new();
        let unrelated = fixture.root.join("keep-unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("marker"), "keep").unwrap();
        let parent = resolve_parent(&fixture.config, fixture.root.to_str().unwrap()).unwrap();

        for fault in [StagingFault::BeforeOpen, StagingFault::BeforeMetadata] {
            let error = create_staging_directory_with_fault(&parent, Some(fault)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Internal);
            assert!(staging_names(&fixture.root).is_empty(), "{fault:?}");
            assert_eq!(
                fs::read_to_string(unrelated.join("marker")).unwrap(),
                "keep"
            );
        }
    }

    #[test]
    fn drain_failure_stops_and_reaps_clone_before_staging_cleanup() {
        let fixture = Fixture::new();
        let script = fixture.base.join("git-drain-failure");
        let pid_file = fixture.base.join("drain-leader.pid");
        let pid_file_text = pid_file.display().to_string();
        let quoted_pid_file = shell_words::quote(&pid_file_text);
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" >{quoted_pid_file}\nwhile :; do sleep 30; done\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let parent = resolve_parent(&fixture.config, fixture.root.to_str().unwrap()).unwrap();
        let staging = create_staging_directory(&parent).unwrap();
        let failure = run_git_clone_with_fault(
            script.as_os_str(),
            "https://example.test/team/drain.git",
            &staging.handle,
            Duration::from_secs(2),
            Some(CloneRunFault::DrainAfterSpawn),
        )
        .unwrap_err();
        assert!(failure.cleanup_safe);
        assert!(failure.error.to_string().contains("error stream"));
        staging.cleanup().unwrap();
        assert!(staging_names(&fixture.root).is_empty());

        let raw_pid = fs::read_to_string(pid_file).unwrap();
        let pid = Pid::from_raw(raw_pid.parse().unwrap()).unwrap();
        assert!(
            rustix::process::test_kill_process(pid).is_err(),
            "drain error returned before reaping the clone leader"
        );
    }

    #[test]
    fn payload_modes_follow_owner_umask_in_isolated_processes() {
        for mask in ["077", "022"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("launch_directory::tests::umask_payload_mode_probe")
                .arg("--nocapture")
                .env("ATMUX_TEST_UMASK", mask)
                .status()
                .unwrap();
            assert!(status.success(), "umask probe {mask} failed");
        }
    }

    #[test]
    fn umask_payload_mode_probe() {
        let Ok(raw_mask) = std::env::var("ATMUX_TEST_UMASK") else {
            return;
        };
        let mask = u32::from_str_radix(&raw_mask, 8).unwrap();
        assert!(
            mask <= 0o777,
            "test umask must fit the portable permission bits"
        );
        assert_eq!(
            u32::from(checked_test_mode::<u16>(mask).expect("permission mask fits u16")),
            mask
        );
        let platform_mask: rustix::fs::RawMode =
            checked_test_mode(mask).expect("a bounded permission mask fits the platform RawMode");
        rustix::process::umask(Mode::from_raw_mode(platform_mask));
        let expected = 0o777 & !mask;
        let fixture = Fixture::new();

        let inspect_folder_stage = || assert_staging_and_payload_modes(&fixture.root, expected);
        let folder = create_folder_with_hook(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "mode folder",
            Some(&inspect_folder_stage),
        )
        .unwrap();
        assert_eq!(fs::metadata(folder).unwrap().mode() & 0o777, expected);
        assert!(staging_names(&fixture.root).is_empty());

        let inspect_clone_stage = || assert_staging_and_payload_modes(&fixture.root, expected);
        let clone = clone_repository_with_program_and_hook(
            &fixture.config,
            fixture.root.to_str().unwrap(),
            "https://example.test/team/mode.git",
            Some("mode clone"),
            fixture.fake_git(true).as_os_str(),
            Duration::from_secs(2),
            Some(&inspect_clone_stage),
        )
        .unwrap();
        assert_eq!(fs::metadata(clone).unwrap().mode() & 0o777, expected);
        assert!(staging_names(&fixture.root).is_empty());
    }

    fn assert_staging_and_payload_modes(parent: &Path, payload_mode: u32) {
        let stages = staging_names(parent);
        assert_eq!(stages.len(), 1);
        let stage = parent.join(&stages[0]);
        assert_eq!(fs::metadata(&stage).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(stage.join(PAYLOAD_NAME)).unwrap().mode() & 0o777,
            payload_mode
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
