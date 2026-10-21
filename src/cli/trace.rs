use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(unix)]
use nix::libc;

#[derive(Clone)]
pub(super) struct IoTrace {
    inner: Option<Arc<TraceInner>>,
}

/// Upper bound on a trace file's size. I/O traces are a debugging aid; this
/// keeps a long-running or noisy session from filling the disk.
const MAX_TRACE_FILE_BYTES: u64 = 64 * 1024 * 1024;

struct TraceInner {
    file: Mutex<File>,
    start: Instant,
    written: AtomicU64,
    limit: u64,
}

impl IoTrace {
    pub(super) fn open(path: Option<&Path>) -> io::Result<Self> {
        Self::open_with_limit(path, MAX_TRACE_FILE_BYTES)
    }

    fn open_with_limit(path: Option<&Path>, limit: u64) -> io::Result<Self> {
        let inner = match path {
            Some(path) => {
                let file = open_trace_file(path)?;
                let written = file.metadata()?.len().min(limit);
                Some(Arc::new(TraceInner {
                    file: Mutex::new(file),
                    start: Instant::now(),
                    written: AtomicU64::new(written),
                    limit,
                }))
            }
            None => None,
        };
        Ok(Self { inner })
    }

    pub(super) fn log(&self, direction: &str, bytes: &[u8]) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.written.load(Ordering::Relaxed) >= inner.limit {
            return;
        }
        let elapsed = inner.start.elapsed();
        let line = format!(
            "{:>4}.{:06} {direction} {}\n",
            elapsed.as_secs(),
            elapsed.subsec_micros(),
            trace_hex(bytes),
        );
        let Ok(mut file) = inner.file.lock() else {
            return;
        };
        let previous = inner.written.load(Ordering::Relaxed);
        if previous >= inner.limit {
            return;
        }
        let line_len = line.len() as u64;
        if previous.saturating_add(line_len) > inner.limit {
            // The marker is terminal: whether or not it fits, the trace is
            // closed at the limit so no shorter line lands below it.
            let marker = format!("---- trace truncated at {} bytes ----\n", inner.limit);
            let remaining = (inner.limit - previous) as usize;
            if remaining >= marker.len() {
                let _ = file.write_all(marker.as_bytes());
            }
            inner.written.store(inner.limit, Ordering::Relaxed);
            return;
        }
        if file.write_all(line.as_bytes()).is_err() {
            return;
        }
        inner.written.store(previous + line_len, Ordering::Relaxed);
    }
}

#[cfg(unix)]
fn open_trace_file(path: &Path) -> io::Result<File> {
    let not_regular_file = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("trace path is not a regular file: {}", path.display()),
        )
    };
    // O_NONBLOCK keeps a FIFO with no reader from blocking the open forever:
    // it fails with ENXIO instead, and one with a reader is rejected below.
    let file = match OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
            return Err(not_regular_file());
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(not_regular_file());
    }
    // SAFETY: getuid has no preconditions and cannot fail.
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
    // Writes keep their usual blocking semantics; O_APPEND is preserved.
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is the open descriptor owned by `file`; F_GETFL only reads
    // its status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; F_SETFL only changes the descriptor's status flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
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
    fn trace_file_stops_growing_past_its_limit() {
        let dir = tempfile::tempdir().expect("tempdir creates");
        let path = dir.path().join("trace.log");

        let trace = super::IoTrace::open_with_limit(Some(&path), 128).expect("trace opens");
        for _ in 0..1000 {
            trace.log("OUT", b"some representative payload bytes");
        }
        drop(trace);

        let size = std::fs::metadata(&path).expect("trace metadata").len();
        assert!(size <= 128, "trace should stay within its byte limit");
    }

    // The truncation marker must be the last thing in the file: once it is
    // written, no shorter line may slip in below it and it must not repeat.
    #[cfg(unix)]
    #[test]
    fn trace_truncation_marker_is_terminal() {
        let dir = tempfile::tempdir().expect("tempdir creates");
        let path = dir.path().join("trace.log");

        // Three 37-byte lines (111), then a line too long to fit: the 39-byte
        // marker lands at 150. An 18-byte line and another long line would
        // then still fit in the bytes left before 210.
        let trace = super::IoTrace::open_with_limit(Some(&path), 210).expect("trace opens");
        for _ in 0..3 {
            trace.log("OUT", b"payload");
        }
        trace.log("OUT", &[b'x'; 200]);
        trace.log("IN", b"a");
        trace.log("OUT", &[b'x'; 200]);
        trace.log("IN", b"a");
        drop(trace);

        let contents = std::fs::read_to_string(&path).expect("trace reads");
        let marker = "---- trace truncated at 210 bytes ----\n";
        assert_eq!(
            contents.matches(marker).count(),
            1,
            "marker must appear exactly once: {contents:?}"
        );
        assert!(
            contents.ends_with(marker),
            "marker must be the last line: {contents:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn trace_file_limit_counts_existing_bytes() {
        let dir = tempfile::tempdir().expect("tempdir creates");
        let path = dir.path().join("trace.log");
        std::fs::write(&path, vec![b'x'; 128]).expect("existing trace writes");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("trace permissions set");

        let trace = super::IoTrace::open_with_limit(Some(&path), 128).expect("trace opens");
        trace.log("OUT", b"new sensitive bytes");
        drop(trace);

        assert_eq!(std::fs::metadata(&path).expect("trace metadata").len(), 128);
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

    // Opening a FIFO for writing blocks until a reader appears, which would
    // hang startup before the non-regular-file check could reject it.
    #[cfg(unix)]
    #[test]
    fn trace_file_rejects_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir creates");
        let path = dir.path().join("trace.fifo");
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { nix::libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let (tx, rx) = mpsc::channel();
        let thread_path = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(
                super::IoTrace::open(Some(&thread_path))
                    .err()
                    .map(|error| error.kind()),
            );
        });
        let Ok(result) = rx.recv_timeout(Duration::from_secs(5)) else {
            // Open the read end so the stuck writer returns and the test can exit.
            let _ = std::fs::File::open(&path);
            panic!("IoTrace::open blocked on a FIFO with no reader");
        };
        assert_eq!(result, Some(std::io::ErrorKind::InvalidInput));
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
