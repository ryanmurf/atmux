//! Narrow safe wrapper for assigning a retained directory handle as a Unix
//! child's working directory.

#![deny(unsafe_code)]

use std::{fs::File, io, os::unix::process::CommandExt as _, process::Command};

/// Configures `command` to enter `directory` immediately before `exec`.
///
/// The directory descriptor is cloned before the fork and owned by the
/// registered closure, so renaming or replacing its original path cannot
/// redirect the child. The descriptor remains available until `exec`, when
/// its close-on-exec flag releases it in the new program.
///
/// # Errors
///
/// Returns an error when the directory descriptor cannot be cloned. A later
/// [`Command::spawn`] can still fail if the retained handle is not a directory
/// or `fchdir(2)` rejects it.
#[allow(
    unsafe_code,
    reason = "CommandExt::pre_exec is required to call fchdir after fork"
)]
pub fn set_current_dir(command: &mut Command, directory: &File) -> io::Result<()> {
    let child_cwd = directory.try_clone()?;
    // SAFETY: `child_cwd` owns the inherited descriptor until `exec`, and the
    // post-fork closure performs only rustix's allocation-free `fchdir(2)`
    // wrapper plus the allocation-free Errno -> io::Error conversion.
    // `fchdir` is async-signal-safe on the supported Unix targets.
    unsafe {
        command.pre_exec(move || rustix::process::fchdir(&child_cwd).map_err(io::Error::from));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::set_current_dir;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    fn fixture_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fd-command-cwd-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create fixture root");
        root
    }

    #[test]
    fn child_follows_the_retained_directory_after_its_path_is_replaced() {
        let root = fixture_root();
        let original = root.join("original");
        let moved = root.join("moved");
        fs::create_dir(&original).expect("create original directory");
        let directory = File::open(&original).expect("open original directory");
        fs::rename(&original, &moved).expect("move retained directory");
        fs::create_dir(&original).expect("replace original path");

        let mut command = Command::new("/bin/pwd");
        set_current_dir(&mut command, &directory).expect("retain child cwd");
        let output = command.output().expect("run child in retained cwd");
        assert!(output.status.success());
        let reported = String::from_utf8(output.stdout).expect("pwd emits UTF-8");
        assert_eq!(
            fs::canonicalize(reported.trim()).expect("canonicalize reported cwd"),
            fs::canonicalize(&moved).expect("canonicalize moved directory")
        );
        assert_ne!(
            fs::canonicalize(reported.trim()).expect("canonicalize reported cwd"),
            fs::canonicalize(&original).expect("canonicalize replacement directory")
        );

        fs::remove_dir_all(root).expect("remove fixture root");
    }

    #[test]
    fn spawn_reports_fchdir_failure_for_a_non_directory_handle() {
        let root = fixture_root();
        let path = root.join("not-a-directory");
        fs::write(&path, "fixture").expect("create fixture file");
        let file = File::open(path).expect("open fixture file");
        let mut command = Command::new("/bin/pwd");
        set_current_dir(&mut command, &file).expect("clone fixture handle");
        assert!(command.spawn().is_err(), "fchdir must reject a file handle");
        fs::remove_dir_all(root).expect("remove fixture root");
    }
}
