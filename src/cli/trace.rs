use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
use nix::libc;

#[derive(Clone)]
pub(super) struct IoTrace {
    inner: Option<Arc<TraceInner>>,
}

struct TraceInner {
    file: Mutex<File>,
    start: Instant,
}

impl IoTrace {
    pub(super) fn open(path: Option<&Path>) -> io::Result<Self> {
        let inner = match path {
            Some(path) => Some(Arc::new(TraceInner {
                file: Mutex::new(open_trace_file(path)?),
                start: Instant::now(),
            })),
            None => None,
        };
        Ok(Self { inner })
    }

    pub(super) fn log(&self, direction: &str, bytes: &[u8]) {
        let Some(inner) = &self.inner else {
            return;
        };
        let Ok(mut file) = inner.file.lock() else {
            return;
        };
        let elapsed = inner.start.elapsed();
        let _ = writeln!(
            file,
            "{:>4}.{:06} {direction} {}",
            elapsed.as_secs(),
            elapsed.subsec_micros(),
            trace_hex(bytes),
        );
    }
}

#[cfg(unix)]
fn open_trace_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("trace path is not a regular file: {}", path.display()),
        ));
    }
    if metadata.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "trace file is not owned by the current user: {}",
                path.display()
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "trace file permissions must be 0600, found {mode:03o}: {}",
                path.display()
            ),
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_trace_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn trace_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(bytes.len().saturating_mul(3).saturating_sub(1));
    for (idx, byte) in bytes.iter().enumerate() {
        if idx > 0 {
            output.push(' ');
        }
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn trace_hex_encodes_bytes_for_diagnostics() {
        assert_eq!(super::trace_hex(b"echo\r\n"), "65 63 68 6f 0d 0a");
    }

    #[cfg(unix)]
    #[test]
    fn trace_file_is_user_private() {
        let dir = tempfile::tempdir().expect("tempdir creates");
        let path = dir.path().join("trace.log");

        let trace = super::IoTrace::open(Some(&path)).expect("trace opens");
        trace.log("OUT", b"secret-token\n");
        drop(trace);

        let mode = std::fs::metadata(&path)
            .expect("trace metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn trace_file_rejects_symlink_target() {
        let dir = tempfile::tempdir().expect("tempdir creates");
        let target = dir.path().join("target.log");
        let path = dir.path().join("trace.log");
        std::fs::write(&target, "original\n").expect("target writes");
        symlink(&target, &path).expect("trace symlink creates");

        let error = super::IoTrace::open(Some(&path))
            .err()
            .expect("trace symlink should be rejected");

        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            std::fs::read_to_string(&target).expect("target reads"),
            "original\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn trace_file_rejects_existing_loose_permissions() {
        let dir = tempfile::tempdir().expect("tempdir creates");
        let path = dir.path().join("trace.log");
        std::fs::write(&path, "existing\n").expect("trace writes");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("trace mode set");

        let error = super::IoTrace::open(Some(&path))
            .err()
            .expect("loose trace permissions should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
