use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::libc;

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

pub(super) struct RuntimeRegistration {
    pid: u32,
    path: PathBuf,
}

impl RuntimeRegistration {
    pub(super) fn register() -> io::Result<Self> {
        let dir = runtime_dir();
        ensure_private_runtime_dir(&dir)?;
        let path = dir.join("pids");
        let pid = std::process::id();
        let mut file = open_private_append_file(&path)?;
        writeln!(file, "{pid}")?;
        Ok(Self { pid, path })
    }
}

impl Drop for RuntimeRegistration {
    fn drop(&mut self) {
        let Ok(input) = read_private_file_to_string(&self.path) else {
            return;
        };
        let retained = input
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .filter(|pid| *pid != self.pid && process_is_alive(*pid))
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let output = if retained.is_empty() {
            String::new()
        } else {
            format!("{retained}\n")
        };
        let _ = write_private_file(&self.path, output.as_bytes());
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
    ensure_private_runtime_dir(&dir)?;
    write_private_file(
        &dir.join("reload"),
        format!("{:?}\n", SystemTime::now()).as_bytes(),
    )?;

    let path = dir.join("pids");
    let input = match read_private_file_to_string(&path) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut count = 0usize;
    let mut retained = Vec::new();
    for pid in input
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
    {
        if process_is_alive(pid) {
            count += 1;
            retained.push(pid.to_string());
        }
    }

    let output = if retained.is_empty() {
        String::new()
    } else {
        format!("{}\n", retained.join("\n"))
    };
    write_private_file(&path, output.as_bytes())?;
    Ok(count)
}

fn runtime_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("PRISMTTY_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join(format!("prismtty-{}", current_uid()))
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn reload_marker_path() -> PathBuf {
    runtime_dir().join("reload")
}

#[cfg(unix)]
fn reload_marker_time(path: &Path) -> Option<SystemTime> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    metadata.modified().ok()
}

#[cfg(not(unix))]
fn reload_marker_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
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

#[cfg(not(unix))]
fn ensure_private_runtime_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private_append_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure_private_runtime_file(&file, path)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private_append_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
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

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::write(path, contents)
}

#[cfg(unix)]
fn read_private_file_to_string(path: &Path) -> io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure_private_runtime_file(&file, path)?;

    let mut input = String::new();
    file.read_to_string(&mut input)?;
    Ok(input)
}

#[cfg(not(unix))]
fn read_private_file_to_string(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
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
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    use std::io::Write;

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
        let pids = runtime.join("pids");
        let reload = runtime.join("reload");

        super::ensure_private_runtime_dir(&runtime).expect("runtime dir creates");
        {
            let mut file = super::open_private_append_file(&pids).expect("pids opens");
            writeln!(file, "123").expect("pid writes");
        }
        super::write_private_file(&reload, b"now\n").expect("reload writes");

        assert_eq!(mode(&runtime), 0o700);
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
    fn runtime_append_file_rejects_symlink_target() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let runtime = temp.path().join("runtime");
        let target = temp.path().join("target.txt");
        let pids = runtime.join("pids");
        std::fs::write(&target, "original\n").expect("target writes");
        super::ensure_private_runtime_dir(&runtime).expect("runtime dir creates");
        symlink(&target, &pids).expect("pids symlink creates");

        let error =
            super::open_private_append_file(&pids).expect_err("pids symlink should be rejected");

        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            std::fs::read_to_string(&target).expect("target reads"),
            "original\n"
        );
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
}
