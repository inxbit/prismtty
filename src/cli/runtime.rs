use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::libc;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

const PID_DIR_NAME: &str = "pids.d";

pub(super) struct RuntimeRegistration {
    path: PathBuf,
}

impl RuntimeRegistration {
    pub(super) fn register() -> io::Result<Self> {
        let dir = runtime_dir();
        let pid = std::process::id();
        let path = register_pid_file(&dir, pid)?;
        Ok(Self { path })
    }
}

impl Drop for RuntimeRegistration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) struct ReloadWatcher {
    marker: PathBuf,
    last_seen: Option<SystemTime>,
}

impl ReloadWatcher {
    pub(super) fn new() -> Self {
        let marker = reload_marker_path();
        let last_seen = reload_marker_time(&marker);
        Self { marker, last_seen }
    }

    pub(super) fn reload_requested(&mut self) -> bool {
        let current = reload_marker_time(&self.marker);
        if current.is_some() && current != self.last_seen {
            self.last_seen = current;
            return true;
        }
        false
    }
}

pub(super) fn request_reload() -> io::Result<usize> {
    let dir = runtime_dir();
    request_reload_in_dir(&dir, process_is_alive)
}

fn request_reload_in_dir<F>(dir: &Path, mut is_alive: F) -> io::Result<usize>
where
    F: FnMut(u32) -> bool,
{
    ensure_private_runtime_dir(dir)?;
    write_private_file(
        &dir.join("reload"),
        format!("{:?}\n", SystemTime::now()).as_bytes(),
    )?;

    let pid_dir = ensure_private_pid_dir(dir)?;
    let mut count = 0usize;
    for entry in fs::read_dir(pid_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let pid = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok());
        let Some(pid) = pid else {
            let _ = fs::remove_file(path);
            continue;
        };
        if is_alive(pid) {
            count += 1;
        } else {
            let _ = fs::remove_file(path);
        }
    }

    Ok(count)
}

fn register_pid_file(dir: &Path, pid: u32) -> io::Result<PathBuf> {
    ensure_private_runtime_dir(dir)?;
    let pid_dir = ensure_private_pid_dir(dir)?;
    let path = pid_dir.join(pid.to_string());
    write_private_file(&path, b"")?;
    Ok(path)
}

fn runtime_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("PRISMTTY_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join(format!("prismtty-{}", current_uid()))
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn reload_marker_path() -> PathBuf {
    runtime_dir().join("reload")
}

#[cfg(unix)]
fn ensure_private_pid_dir(dir: &Path) -> io::Result<PathBuf> {
    let path = dir.join(PID_DIR_NAME);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(metadata) if metadata.file_type().is_file() => {
            if metadata.uid() != current_uid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "legacy pid file is not owned by the current user: {}",
                        path.display()
                    ),
                ));
            }
            fs::remove_file(&path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("runtime pid path is not a directory: {}", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    ensure_private_runtime_dir(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn reload_marker_time(path: &Path) -> Option<SystemTime> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    metadata.modified().ok()
}

#[cfg(unix)]
fn ensure_private_runtime_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    match builder.create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime path is not a directory: {}", dir.display()),
        ));
    }
    if metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory is not owned by the current user: {}",
                dir.display()
            ),
        ));
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure_private_runtime_file(&file, path)?;
    file.write_all(contents)
}

#[cfg(unix)]
fn ensure_private_runtime_file(file: &File, path: &Path) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime path is not a regular file: {}", path.display()),
        ));
    }
    if metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime file is not owned by the current user: {}",
                path.display()
            ),
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill with signal 0 has no memory-safety preconditions and only checks that the
    // process exists.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let errno = (result != 0)
        .then(|| io::Error::last_os_error().raw_os_error())
        .flatten();
    process_is_alive_from_kill_result(result, errno)
}

fn process_is_alive_from_kill_result(result: libc::c_int, errno: Option<libc::c_int>) -> bool {
    result == 0 || errno == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata reads")
            .permissions()
            .mode()
            & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_and_files_are_user_private() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let runtime = temp.path().join("runtime");
        let pids = runtime.join(super::PID_DIR_NAME).join("123");
        let reload = runtime.join("reload");

        super::ensure_private_runtime_dir(&runtime).expect("runtime dir creates");
        super::register_pid_file(&runtime, 123).expect("pid file registers");
        super::write_private_file(&reload, b"now\n").expect("reload writes");

        assert_eq!(mode(&runtime), 0o700);
        assert_eq!(mode(&runtime.join(super::PID_DIR_NAME)), 0o700);
        assert_eq!(mode(&pids), 0o600);
        assert_eq!(mode(&reload), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn existing_runtime_dir_is_tightened() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let runtime = temp.path().join("runtime");
        std::fs::create_dir(&runtime).expect("runtime dir creates");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
            .expect("mode set");

        super::ensure_private_runtime_dir(&runtime).expect("runtime dir tightens");

        assert_eq!(mode(&runtime), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_write_file_rejects_symlink_target() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let runtime = temp.path().join("runtime");
        let target = temp.path().join("target.txt");
        let reload = runtime.join("reload");
        std::fs::write(&target, "original\n").expect("target writes");
        super::ensure_private_runtime_dir(&runtime).expect("runtime dir creates");
        symlink(&target, &reload).expect("reload symlink creates");

        let error = super::write_private_file(&reload, b"updated\n")
            .expect_err("reload symlink should be rejected");

        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            std::fs::read_to_string(&target).expect("target reads"),
            "original\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pid_registration_preserves_legacy_shared_pid_file_for_older_binaries() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let runtime = temp.path().join("runtime");
        super::ensure_private_runtime_dir(&runtime).expect("runtime dir creates");
        super::write_private_file(&runtime.join("pids"), b"999\n")
            .expect("legacy pids file writes");

        let pid_file = super::register_pid_file(&runtime, 123).expect("pid file registers");

        assert!(runtime.join("pids").is_file());
        assert_eq!(
            std::fs::read_to_string(runtime.join("pids")).expect("legacy pids file reads"),
            "999\n"
        );
        assert!(runtime.join(super::PID_DIR_NAME).is_dir());
        assert_eq!(pid_file, runtime.join(super::PID_DIR_NAME).join("123"));
        assert!(pid_file.is_file());
    }

    #[test]
    fn request_reload_prunes_dead_pid_files_without_rewriting_live_entries() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let runtime = temp.path().join("runtime");
        super::register_pid_file(&runtime, 101).expect("live pid registers");
        super::register_pid_file(&runtime, 202).expect("dead pid registers");

        let count = super::request_reload_in_dir(&runtime, |pid| pid == 101)
            .expect("reload marker writes and pids prune");

        assert_eq!(count, 1);
        assert!(runtime.join(super::PID_DIR_NAME).join("101").exists());
        assert!(!runtime.join(super::PID_DIR_NAME).join("202").exists());
    }

    #[test]
    fn process_liveness_treats_permission_denied_as_alive() {
        assert!(super::process_is_alive_from_kill_result(
            -1,
            Some(nix::libc::EPERM)
        ));
        assert!(!super::process_is_alive_from_kill_result(
            -1,
            Some(nix::libc::ESRCH)
        ));
    }
}
