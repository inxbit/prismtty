//! Integration coverage for prismtty's PTY signal handling (AUDIT L14).
//!
//! These tests run the `ptty` binary *inside* a PTY (so it treats its stdin as
//! a terminal and installs its signal handlers), then deliver a real signal to
//! it and assert the wrapped child observes the forwarded signal rather than
//! being orphaned.
#![cfg(unix)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::Child;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const RECEIVE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CAPTURE_BYTE_LIMIT: usize = 1024 * 1024;
const CLEANUP_WAIT_LIMIT: Duration = Duration::from_secs(2);
const THREAD_JOIN_LIMIT: Duration = Duration::from_millis(250);

struct PtyChildGuard {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pid: libc::pid_t,
}

impl PtyChildGuard {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        let pid = child.process_id().expect("PTY child pid") as libc::pid_t;
        Self {
            child: Some(child),
            pid,
        }
    }

    fn pid(&self) -> libc::pid_t {
        self.pid
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<u32> {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut()?;
            if let Some(status) = child.try_wait().expect("poll PTY child") {
                self.child.take();
                return Some(status.exit_code());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(20)),
            );
        }
    }

    fn terminate_and_wait(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // SAFETY: kill has no memory-safety preconditions; the target is a process this test
        // spawned.
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
            libc::kill(self.pid, libc::SIGKILL);
        }
        let _ = child.kill();
        let deadline = Instant::now() + CLEANUP_WAIT_LIMIT;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

impl Drop for PtyChildGuard {
    fn drop(&mut self) {
        self.terminate_and_wait();
    }
}

struct ProcessGuard {
    child: Option<Child>,
}

impl ProcessGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("process still owned")
    }

    fn pid(&self) -> libc::pid_t {
        self.child.as_ref().expect("process still owned").id() as libc::pid_t
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut()?;
            if let Some(status) = child.try_wait().expect("poll process") {
                self.child.take();
                return Some(status);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(20)),
            );
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let deadline = Instant::now() + CLEANUP_WAIT_LIMIT;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

struct PidGuard {
    pid: Option<libc::pid_t>,
    marker_path: Option<std::path::PathBuf>,
}

impl PidGuard {
    fn from_marker(marker_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            pid: None,
            marker_path: Some(marker_path.into()),
        }
    }

    fn set(&mut self, pid: libc::pid_t) {
        self.pid = Some(pid);
    }

    fn disarm(&mut self) {
        self.pid = None;
        self.marker_path = None;
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let marker_pid = self.marker_path.as_ref().and_then(|path| {
            let deadline = Instant::now() + THREAD_JOIN_LIMIT;
            loop {
                if let Some(pid) = std::fs::read_to_string(path).ok().and_then(|contents| {
                    contents
                        .split_whitespace()
                        .next()
                        .and_then(|value| value.parse().ok())
                }) {
                    break Some(pid);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        if let Some(pid) = self.pid.or(marker_pid) {
            // SAFETY: kill has no memory-safety preconditions; the target is a process this test
            // spawned.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

struct CaptureTask {
    receiver: mpsc::Receiver<()>,
    output: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    completion: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CaptureTask {
    fn from_master(master: &dyn portable_pty::MasterPty) -> Self {
        let master_fd = master.as_raw_fd().expect("PTY master fd");
        // SAFETY: `master_fd` is the open PTY master; dup returns a fresh descriptor this test
        // owns.
        let reader_fd = unsafe { libc::dup(master_fd) };
        assert!(reader_fd >= 0, "duplicate PTY master fd failed");
        // SAFETY: `reader_fd` is a freshly duplicated descriptor that nothing else owns or closes.
        let reader = unsafe { File::from_raw_fd(reader_fd) };
        Self::from_reader(reader, reader_fd)
    }

    fn from_reader(mut reader: impl Read + Send + 'static, reader_fd: libc::c_int) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let output = Arc::new(Mutex::new(Vec::new()));
        let thread_output = Arc::clone(&output);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let finished = Arc::new(AtomicBool::new(false));
        let thread_finished = Arc::clone(&finished);
        let (completion_tx, completion) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            struct Finished(Arc<AtomicBool>, mpsc::SyncSender<()>);
            impl Drop for Finished {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                    let _ = self.1.try_send(());
                }
            }
            let _finished = Finished(thread_finished, completion_tx);
            let mut buf = [0u8; 256];
            while !thread_stop.load(Ordering::Acquire) {
                let mut poll_fd = libc::pollfd {
                    fd: reader_fd,
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                };
                // SAFETY: `poll_fd` is a valid, exclusively borrowed pollfd and the count matches.
                let ready = unsafe {
                    libc::poll(
                        &mut poll_fd,
                        1,
                        CAPTURE_POLL_INTERVAL.as_millis() as libc::c_int,
                    )
                };
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if ready == 0 {
                    continue;
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut output = thread_output
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let remaining = CAPTURE_BYTE_LIMIT.saturating_sub(output.len());
                        if remaining > 0 {
                            output.extend_from_slice(&buf[..n.min(remaining)]);
                        }
                        drop(output);
                        if matches!(
                            sender.try_send(()),
                            Err(mpsc::TrySendError::Disconnected(()))
                        ) {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        Self {
            receiver,
            output,
            stop,
            finished,
            completion,
            thread: Some(thread),
        }
    }

    fn wait_until(&self, timeout: Duration, mut predicate: impl FnMut(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let output = self
                    .output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if predicate(&output) {
                    return output.clone();
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return self
                    .output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
            }
            let wait = deadline
                .saturating_duration_since(now)
                .min(RECEIVE_POLL_INTERVAL);
            match self.receiver.recv_timeout(wait) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return self
                        .output
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                }
            }
        }
    }

    fn finished_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.finished)
    }

    fn stop_bounded(&mut self) {
        self.stop.store(true, Ordering::Release);
        let completed = self.completion.recv_timeout(THREAD_JOIN_LIMIT).is_ok()
            || self.finished.load(Ordering::Acquire);
        if completed {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            self.thread.take();
        }
    }
}

impl Drop for CaptureTask {
    fn drop(&mut self) {
        self.stop_bounded();
    }
}

struct PtySession {
    child: PtyChildGuard,
    capture: CaptureTask,
}

impl PtySession {
    fn new(
        child: Box<dyn portable_pty::Child + Send + Sync>,
        master: &dyn portable_pty::MasterPty,
    ) -> Self {
        Self {
            child: PtyChildGuard::new(child),
            capture: CaptureTask::from_master(master),
        }
    }

    fn pid(&self) -> libc::pid_t {
        self.child.pid()
    }

    fn capture(&self) -> &CaptureTask {
        &self.capture
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<u32> {
        self.child.wait_for_exit(timeout)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.child.terminate_and_wait();
        self.capture.stop_bounded();
    }
}

fn wait_for_condition(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(20)),
        );
    }
}

fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: kill with signal 0 has no memory-safety preconditions and only checks for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn process_group_exists(pgid: libc::pid_t) -> bool {
    // SAFETY: kill with signal 0 has no memory-safety preconditions and only checks for existence.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn wait_for_pid_file(path: &std::path::Path, timeout: Duration) -> libc::pid_t {
    let mut pid = None;
    assert!(
        wait_for_condition(timeout, || {
            pid = std::fs::read_to_string(path)
                .ok()
                .and_then(|contents| contents.trim().parse().ok());
            pid.is_some()
        }),
        "pid marker was not written: {}",
        path.display()
    );
    pid.expect("pid parsed")
}

fn wait_for_pid_pair(path: &std::path::Path, timeout: Duration) -> (libc::pid_t, libc::pid_t) {
    let mut pair = None;
    assert!(
        wait_for_condition(timeout, || {
            pair = std::fs::read_to_string(path).ok().and_then(|contents| {
                let mut fields = contents.split_whitespace();
                Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
            });
            pair.is_some()
        }),
        "pid pair marker was not written: {}",
        path.display()
    );
    pair.expect("pid pair parsed")
}

#[test]
fn harness_reaps_child_and_joins_reader_on_early_error() {
    let started = Instant::now();
    let (result, child_pid, reader_finished): (
        Result<(), &'static str>,
        libc::pid_t,
        Arc<AtomicBool>,
    ) = {
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .expect("openpty");
        let mut builder = CommandBuilder::new("sh");
        builder.arg("-c");
        builder.arg("trap '' HUP TERM; while true; do sleep 1; done");
        let child = pair
            .slave
            .spawn_command(builder)
            .expect("spawn guarded child");
        drop(pair.slave);
        let session = PtySession::new(child, &*pair.master);
        let child_pid = session.pid();
        let reader_finished = session.capture().finished_flag();

        (Err("intentional early error"), child_pid, reader_finished)
    };

    assert_eq!(result, Err("intentional early error"));
    assert!(
        started.elapsed() < CLEANUP_WAIT_LIMIT + THREAD_JOIN_LIMIT + Duration::from_millis(250),
        "early-error RAII cleanup exceeded its bounded wait"
    );
    assert!(
        wait_for_condition(Duration::from_secs(2), || !process_exists(child_pid)),
        "child survived guard cleanup after an early error"
    );
    assert!(
        reader_finished.load(Ordering::Acquire),
        "capture thread was not joined on the early-error path"
    );
}

#[test]
fn capture_cleanup_is_bounded_when_a_reader_is_stuck() {
    use std::sync::Condvar;

    struct GatedReader {
        file: File,
        entered: Arc<AtomicBool>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Read for GatedReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.entered.store(true, Ordering::Release);
            let (lock, wake) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.file.read(buffer)
        }
    }

    let mut fds = [0; 2];
    // SAFETY: `fds` is a valid array of two c_int that pipe fills on success.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let read_fd = fds[0];
    let write_fd = fds[1];
    // SAFETY: `write_fd` is our pipe end and the buffer is a valid 1-byte slice.
    assert_eq!(unsafe { libc::write(write_fd, b"x".as_ptr().cast(), 1) }, 1);
    let entered = Arc::new(AtomicBool::new(false));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let reader = GatedReader {
        // SAFETY: `read_fd` is our pipe end and this File becomes its sole owner.
        file: unsafe { File::from_raw_fd(read_fd) },
        entered: Arc::clone(&entered),
        gate: Arc::clone(&gate),
    };
    let capture = CaptureTask::from_reader(reader, read_fd);
    assert!(
        wait_for_condition(Duration::from_secs(1), || entered.load(Ordering::Acquire)),
        "capture reader did not enter injected blocking read"
    );

    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let dropper = thread::spawn(move || {
        drop(capture);
        let _ = drop_tx.send(());
    });
    let bounded = drop_rx.recv_timeout(THREAD_JOIN_LIMIT + Duration::from_millis(250));
    let (lock, wake) = &*gate;
    *lock.lock().unwrap() = true;
    wake.notify_all();
    dropper.join().expect("capture dropper joins after release");
    // SAFETY: `write_fd` is our pipe end, opened above and not used after this close.
    unsafe {
        libc::close(write_fd);
    }

    assert!(bounded.is_ok(), "capture cleanup blocked on a stuck reader");
}

#[test]
fn marker_backed_pid_guard_cleans_an_unrecorded_wrapped_group() {
    let marker = tempfile::NamedTempFile::new().expect("pid marker");
    let marker_path = marker.path().to_path_buf();
    std::fs::remove_file(&marker_path).ok();
    let guard = PidGuard::from_marker(&marker_path);
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new("sh");
    builder.env("PRISMTTY_PID_MARKER", &marker_path);
    builder.arg("-c");
    builder
        .arg("echo $$ > \"$PRISMTTY_PID_MARKER\"; trap '' HUP TERM; while true; do sleep 1; done");
    let mut child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn marker-guarded child");
    drop(pair.slave);
    let child_pid = wait_for_pid_file(&marker_path, Duration::from_secs(2));

    drop(guard);

    assert!(
        wait_for_condition(Duration::from_secs(2), || {
            child.try_wait().ok().flatten().is_some()
        }),
        "marker-backed guard did not terminate and reap its test process"
    );
    assert!(
        !process_exists(child_pid),
        "marker-backed guard left the wrapped process group alive"
    );
}

#[test]
fn interactive_termination_reaps_a_signal_immune_child_group() {
    let marker = tempfile::NamedTempFile::new().expect("pid marker");
    let marker_path = marker.path().to_path_buf();
    std::fs::remove_file(&marker_path).ok();
    let mut wrapped_guard = PidGuard::from_marker(&marker_path);
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_PID_MARKER", &marker_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(
        "echo $$ > \"$PRISMTTY_PID_MARKER\"; trap '' HUP INT QUIT TERM USR1 USR2; sh -c 'trap \"\" HUP INT QUIT TERM USR1 USR2; while true; do sleep 1; done' & descendant=$!; echo \"$$ $descendant\" > \"$PRISMTTY_PID_MARKER\"; echo READY; wait",
    );
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);
    let ready = ptty
        .capture()
        .wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"READY"));
    assert!(
        contains_bytes(&ready, b"READY"),
        "wrapped child reached READY"
    );

    let (wrapped_pid, descendant_pid) = wait_for_pid_pair(&marker_path, Duration::from_secs(2));
    wrapped_guard.set(wrapped_pid);
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(ptty.pid(), libc::SIGTERM) }, 0);

    assert_eq!(
        ptty.wait_for_exit(Duration::from_secs(5)),
        Some(128 + libc::SIGTERM as u32),
        "interactive wrapper did not report signal-style termination"
    );
    assert!(
        wait_for_condition(Duration::from_secs(3), || {
            !process_exists(wrapped_pid)
                && !process_exists(descendant_pid)
                && !process_group_exists(wrapped_pid)
        }),
        "interactive wrapped child group survived wrapper termination"
    );
    wrapped_guard.disarm();
}

#[test]
fn noninteractive_termination_reaps_a_signal_immune_child_group() {
    use std::process::{Command, Stdio};

    let marker = tempfile::NamedTempFile::new().expect("pid marker");
    let marker_path = marker.path().to_path_buf();
    std::fs::remove_file(&marker_path).ok();
    let mut wrapped_guard = PidGuard::from_marker(&marker_path);
    let ptty = Command::new(env!("CARGO_BIN_EXE_ptty"))
        .env("PRISMTTY_PID_MARKER", &marker_path)
        .arg("sh")
        .arg("-c")
        .arg(
            "echo $$ > \"$PRISMTTY_PID_MARKER\"; trap '' HUP INT QUIT TERM USR1 USR2; sh -c 'trap \"\" HUP INT QUIT TERM USR1 USR2; while true; do sleep 1; done' & descendant=$!; echo \"$$ $descendant\" > \"$PRISMTTY_PID_MARKER\"; wait",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn noninteractive ptty");
    let mut ptty = ProcessGuard::new(ptty);
    let (wrapped_pid, descendant_pid) = wait_for_pid_pair(&marker_path, Duration::from_secs(10));
    wrapped_guard.set(wrapped_pid);
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(ptty.pid(), libc::SIGTERM) }, 0);

    let status = ptty
        .wait_for_exit(Duration::from_secs(5))
        .expect("noninteractive wrapper exits after SIGTERM");
    assert_eq!(
        status.code(),
        Some(128 + libc::SIGTERM),
        "noninteractive wrapper did not report signal-style termination"
    );
    assert!(
        wait_for_condition(Duration::from_secs(3), || {
            !process_exists(wrapped_pid)
                && !process_exists(descendant_pid)
                && !process_group_exists(wrapped_pid)
        }),
        "noninteractive wrapped child group survived wrapper termination"
    );
    wrapped_guard.disarm();
}

fn assert_terminal_restored_after(signal: libc::c_int) {
    use nix::sys::termios::{LocalFlags, tcgetattr};

    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let tty_name = pair.master.tty_name().expect("outer PTY tty name");
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open(tty_name)
        .expect("open outer slave tty");
    let original = tcgetattr(&tty).expect("original tty attrs");
    let marker_dir = tempfile::tempdir().expect("signal child marker directory");
    let child_pid_path = marker_dir.path().join("child.pid");
    let mut child_guard = PidGuard::from_marker(&child_pid_path);
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_CHILD_PID", &child_pid_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(
        "echo $$ > \"$PRISMTTY_CHILD_PID\"; trap 'sleep 0.3; exit 0' HUP INT QUIT TERM USR1 USR2 ALRM VTALRM PROF XCPU XFSZ; echo READY; while true; do sleep 0.05; done",
    );
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);
    let ready = ptty
        .capture()
        .wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"READY"));
    assert!(
        contains_bytes(&ready, b"READY"),
        "wrapped child reached READY"
    );
    child_guard.set(wait_for_pid_file(&child_pid_path, Duration::from_secs(2)));
    assert!(
        wait_for_condition(Duration::from_secs(2), || {
            tcgetattr(&tty)
                .map(|attrs| !attrs.local_flags.contains(LocalFlags::ICANON))
                .unwrap_or(false)
        }),
        "ptty never entered raw mode before signal {signal}"
    );

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(ptty.pid(), signal) }, 0);
    assert!(
        wait_for_condition(Duration::from_secs(1), || {
            tcgetattr(&tty)
                .map(|attrs| attrs == original)
                .unwrap_or(false)
        }),
        "terminal attributes were not restored before exit after signal {signal}"
    );
    assert_eq!(
        ptty.wait_for_exit(Duration::from_secs(5)),
        Some(128 + signal as u32),
        "interactive wrapper did not report signal-style termination"
    );
    child_guard.disarm();
}

#[test]
fn catchable_terminating_signals_restore_raw_terminal_state() {
    for signal in [
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGINT,
        libc::SIGUSR1,
        libc::SIGUSR2,
        libc::SIGALRM,
        libc::SIGVTALRM,
        libc::SIGPROF,
        libc::SIGXCPU,
        libc::SIGXFSZ,
    ] {
        assert_terminal_restored_after(signal);
    }
}

fn file_counter(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn assert_job_control_stop_resume(signal: libc::c_int) {
    use nix::sys::termios::{LocalFlags, tcgetattr};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

    let marker_dir = tempfile::tempdir().expect("job-control marker directory");
    let child_pid_path = marker_dir.path().join("child.pid");
    let heartbeat_path = marker_dir.path().join("heartbeat");
    let resume_path = marker_dir.path().join("resumed");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let tty_name = pair.master.tty_name().expect("outer PTY tty name");
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tty_name)
        .expect("open outer slave tty");
    let original = tcgetattr(&tty).expect("original tty attrs");
    let mut child_guard = PidGuard::from_marker(&child_pid_path);
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_CHILD_PID", &child_pid_path);
    builder.env("PRISMTTY_HEARTBEAT", &heartbeat_path);
    builder.env("PRISMTTY_RESUMED", &resume_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(
        "trap 'echo resumed >> \"$PRISMTTY_RESUMED\"' CONT; echo $$ > \"$PRISMTTY_CHILD_PID\"; echo READY; i=0; while true; do i=$((i+1)); echo $i > \"$PRISMTTY_HEARTBEAT\"; sleep 0.03; done",
    );
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);
    let pid = ptty.pid();
    let ready = ptty
        .capture()
        .wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"READY"));
    assert!(
        contains_bytes(&ready, b"READY"),
        "wrapped child reached READY"
    );
    let child_pid = wait_for_pid_file(&child_pid_path, Duration::from_secs(5));
    child_guard.set(child_pid);
    let initial_heartbeat = file_counter(&heartbeat_path).unwrap_or(0);
    assert!(
        wait_for_condition(Duration::from_secs(5), || {
            file_counter(&heartbeat_path).is_some_and(|value| value > initial_heartbeat)
        }),
        "wrapped child heartbeat did not start"
    );
    assert!(
        wait_for_condition(Duration::from_secs(5), || {
            tcgetattr(&tty)
                .map(|attrs| !attrs.local_flags.contains(LocalFlags::ICANON))
                .unwrap_or(false)
        }),
        "ptty never entered raw mode before job-control signal {signal}"
    );

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    let mut observed_stop = None;
    assert!(
        wait_for_condition(Duration::from_secs(5), || {
            match waitpid(
                Pid::from_raw(pid),
                Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG),
            ) {
                Ok(WaitStatus::Stopped(_, stopped_by)) => {
                    observed_stop = Some(stopped_by);
                    true
                }
                _ => false,
            }
        }),
        "ptty did not enter a stopped state for job-control signal {signal}"
    );
    assert_eq!(
        observed_stop,
        Some(nix::sys::signal::Signal::SIGSTOP),
        "ptty stopped before completing its supervised job-control transition"
    );
    assert_eq!(
        tcgetattr(&tty).expect("attrs while stopped"),
        original,
        "terminal was not cooked before ptty stopped"
    );
    // The wrapper sends SIGSTOP to the child group before stopping itself.
    // Allow any already-running heartbeat write to finish, then prove the
    // counter stays stable across several normal update intervals.
    thread::sleep(Duration::from_millis(80));
    let stopped_heartbeat = file_counter(&heartbeat_path).expect("heartbeat while stopped");
    thread::sleep(Duration::from_millis(160));
    assert_eq!(
        file_counter(&heartbeat_path),
        Some(stopped_heartbeat),
        "wrapped child kept running while ptty was stopped by signal {signal}"
    );

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    let resumed_raw = wait_for_condition(Duration::from_secs(5), || {
        tcgetattr(&tty)
            .map(|attrs| !attrs.local_flags.contains(LocalFlags::ICANON))
            .unwrap_or(false)
    });
    if !resumed_raw {
        let diagnostics = ptty
            .capture()
            .wait_until(Duration::from_millis(200), |_| false);
        panic!(
            "ptty did not reapply raw mode after resume; output: {:?}",
            String::from_utf8_lossy(&diagnostics)
        );
    }
    assert!(
        wait_for_condition(Duration::from_secs(5), || {
            file_counter(&heartbeat_path).is_some_and(|value| value > stopped_heartbeat)
                && std::fs::read_to_string(&resume_path)
                    .map(|contents| contents.contains("resumed"))
                    .unwrap_or(false)
        }),
        "wrapped child did not resume after job-control signal {signal}"
    );

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert_eq!(
        ptty.wait_for_exit(Duration::from_secs(5)),
        Some(128 + libc::SIGTERM as u32),
        "ptty exits after cleanup signal"
    );
    assert!(
        wait_for_condition(Duration::from_secs(5), || !process_exists(child_pid)),
        "wrapped child survived cleanup after job-control signal {signal}"
    );
    child_guard.disarm();
}

#[test]
fn job_control_signals_restore_stop_and_resume_wrapper_and_child() {
    for signal in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
        assert_job_control_stop_resume(signal);
    }
}

/// AUDIT L14: a signal delivered to prismtty is forwarded to the wrapped
/// child's process group instead of orphaning it.
#[test]
fn external_signal_is_forwarded_to_wrapped_child() {
    let marker = tempfile::NamedTempFile::new().expect("marker file");
    let marker_path = marker.path().to_path_buf();
    let pid_marker = tempfile::NamedTempFile::new().expect("pid marker");
    let pid_marker_path = pid_marker.path().to_path_buf();
    // Start absent so its later presence proves the child's TERM trap ran.
    std::fs::remove_file(&marker_path).ok();
    std::fs::remove_file(&pid_marker_path).ok();
    let mut wrapped_guard = PidGuard::from_marker(&pid_marker_path);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_SIGNAL_MARKER", marker_path.as_os_str());
    builder.env("PRISMTTY_PID_MARKER", pid_marker_path.as_os_str());
    builder.arg("sh");
    builder.arg("-c");
    // Ignore SIGHUP (which PTY teardown delivers when prismtty exits) so that
    // writing the marker can only be the result of a *forwarded* SIGTERM.
    builder.arg(
        "echo $$ > \"$PRISMTTY_PID_MARKER\"; trap '' HUP; trap 'echo forwarded > \"$PRISMTTY_SIGNAL_MARKER\" ; exit 0' TERM; echo READY; while true; do sleep 0.2; done",
    );

    let child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);
    let mut session = PtySession::new(child, &*pair.master);
    let ptty_pid = session.pid();

    // Wait until the wrapped shell has installed its traps and printed READY
    // (relayed through prismtty to the outer master).
    let ready = session
        .capture()
        .wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"READY"));
    assert!(
        contains_bytes(&ready, b"READY"),
        "wrapped child reached READY"
    );
    wrapped_guard.set(wait_for_pid_file(&pid_marker_path, Duration::from_secs(2)));

    // Deliver SIGTERM to prismtty; it must forward to the wrapped child group.
    let signal_started = Instant::now();
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(ptty_pid, libc::SIGTERM) }, 0);

    // The child's TERM trap writes the marker; poll for it.
    let forwarded = wait_for_condition(Duration::from_secs(10), || {
        std::fs::read_to_string(&marker_path)
            .map(|c| c.contains("forwarded"))
            .unwrap_or(false)
    });

    assert_eq!(
        session.wait_for_exit(Duration::from_secs(2)),
        Some(128 + libc::SIGTERM as u32),
        "wrapper did not report signal-style termination"
    );
    assert!(
        forwarded,
        "wrapped child never received the forwarded SIGTERM (it was orphaned)"
    );
    assert!(
        signal_started.elapsed() < Duration::from_secs(2),
        "graceful child exit waited through the full termination grace period"
    );
    wrapped_guard.disarm();
}

/// PTY EOF can arrive before the wrapped process exits when it deliberately
/// closes all terminal descriptors. The process must remain registered for
/// signal forwarding until it has actually exited and is ready to be reaped.
#[test]
fn signal_after_pty_eof_still_terminates_wrapped_child() {
    let marker_dir = tempfile::tempdir().expect("marker directory");
    let pid_path = marker_dir.path().join("child.pid");
    let closed_path = marker_dir.path().join("pty-closed");
    let mut wrapped_guard = PidGuard::from_marker(&pid_path);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_PID_MARKER", pid_path.as_os_str());
    builder.env("PRISMTTY_CLOSED_MARKER", closed_path.as_os_str());
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(
        "echo $$ > \"$PRISMTTY_PID_MARKER\"; exec 0<&- 1>&- 2>&-; : > \"$PRISMTTY_CLOSED_MARKER\"; sleep 30",
    );

    let child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);
    let mut session = PtySession::new(child, &*pair.master);
    let wrapped_pid = wait_for_pid_file(&pid_path, Duration::from_secs(2));
    wrapped_guard.set(wrapped_pid);
    assert!(
        wait_for_condition(Duration::from_secs(2), || closed_path.exists()),
        "wrapped child did not close its terminal descriptors"
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        process_exists(wrapped_pid),
        "wrapped child exited too early"
    );

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(session.pid(), libc::SIGTERM) }, 0);
    assert_eq!(
        session.wait_for_exit(Duration::from_secs(3)),
        Some(128 + libc::SIGTERM as u32),
        "wrapper did not report signal-style termination after PTY EOF"
    );
    assert!(
        wait_for_condition(Duration::from_secs(2), || !process_exists(wrapped_pid)),
        "wrapped child survived a signal delivered after PTY EOF"
    );
    wrapped_guard.disarm();
}

/// A stream failure (stdout closing mid-session) must terminate and reap a
/// wrapped child that ignores SIGHUP instead of hanging forever in the reap:
/// portable-pty's `kill()` only delivers SIGHUP on unix, which such a child
/// shrugs off, so the exit path needs a bounded escalation to SIGKILL.
#[test]
fn stream_error_terminates_sighup_immune_child() {
    use std::process::{Command, Stdio};

    let marker = tempfile::NamedTempFile::new().expect("marker file");
    let marker_path = marker.path().to_path_buf();
    std::fs::remove_file(&marker_path).ok();
    let mut wrapped_guard = PidGuard::from_marker(&marker_path);

    let ptty = Command::new(env!("CARGO_BIN_EXE_ptty"))
        .env("PRISMTTY_PID_MARKER", &marker_path)
        .arg("sh")
        .arg("-c")
        // The child ignores HUP/TERM, records its pid, announces readiness,
        // then keeps emitting output so ptty's next stdout write hits EPIPE
        // once the test closes its end. The loop is bounded so a regression
        // cannot leak a runaway process.
        .arg(
            "echo $$ > \"$PRISMTTY_PID_MARKER\"; trap '' HUP TERM; echo START; i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo tick; sleep 0.1; done",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptty");
    let mut ptty = ProcessGuard::new(ptty);

    // Wait for START so the wrapped shell has installed its traps and written
    // its pid before the stream is broken.
    let stdout = ptty.child_mut().stdout.take().expect("piped stdout");
    let stdout_fd = stdout.as_raw_fd();
    let capture = CaptureTask::from_reader(stdout, stdout_fd);
    let output = capture.wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"START"));
    assert!(
        contains_bytes(&output, b"START"),
        "wrapped child never announced readiness"
    );
    let wrapped_pid: libc::pid_t = std::fs::read_to_string(&marker_path)
        .expect("pid marker")
        .trim()
        .parse()
        .expect("pid marker contents");
    wrapped_guard.set(wrapped_pid);

    // Break the stream: ptty's next relay write fails with EPIPE.
    drop(capture);

    // Race external termination against the failure-path cleanup. The wrapped
    // child ignores both HUP and TERM, so clearing its registered pid before
    // the bounded reap would let the signal watcher exit and orphan it.
    thread::sleep(Duration::from_millis(250));
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(ptty.pid(), libc::SIGTERM) }, 0);

    // ptty must exit instead of blocking forever reaping the HUP-immune child.
    assert!(
        ptty.wait_for_exit(Duration::from_secs(10)).is_some(),
        "ptty hung reaping a wrapped child that ignores SIGHUP"
    );

    // And the wrapped child must be gone, not orphaned.
    assert!(
        wait_for_condition(Duration::from_secs(5), || !process_exists(wrapped_pid)),
        "wrapped child survived ptty's stream failure (orphaned)"
    );
    wrapped_guard.disarm();
}

#[test]
fn stream_error_kills_immune_descendant_after_group_leader_exits() {
    use std::process::{Command, Stdio};

    let marker = tempfile::NamedTempFile::new().expect("marker file");
    let marker_path = marker.path().to_path_buf();
    std::fs::remove_file(&marker_path).ok();
    let mut wrapped_guard = PidGuard::from_marker(&marker_path);

    let ptty = Command::new(env!("CARGO_BIN_EXE_ptty"))
        .env("PRISMTTY_PID_MARKER", &marker_path)
        .arg("sh")
        .arg("-c")
        .arg(
            "sh -c 'trap \"\" HUP TERM; while true; do sleep 1; done' & descendant=$!; echo \"$$ $descendant\" > \"$PRISMTTY_PID_MARKER\"; echo START; i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo tick; sleep 0.1; done",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptty");
    let mut ptty = ProcessGuard::new(ptty);
    let stdout = ptty.child_mut().stdout.take().expect("piped stdout");
    let stdout_fd = stdout.as_raw_fd();
    let capture = CaptureTask::from_reader(stdout, stdout_fd);
    let output = capture.wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"START"));
    assert!(
        contains_bytes(&output, b"START"),
        "wrapped child never announced readiness"
    );

    let (wrapped_pid, descendant_pid) = wait_for_pid_pair(&marker_path, Duration::from_secs(2));
    wrapped_guard.set(wrapped_pid);
    drop(capture);

    assert!(
        ptty.wait_for_exit(Duration::from_secs(10)).is_some(),
        "ptty hung after its stream failed"
    );
    assert!(
        wait_for_condition(Duration::from_secs(5), || {
            !process_exists(wrapped_pid)
                && !process_exists(descendant_pid)
                && !process_group_exists(wrapped_pid)
        }),
        "signal-immune descendant survived after its group leader exited"
    );
    wrapped_guard.disarm();
}

/// Input that reaches the wrapper after the wrapped child has already exited
/// (a paste ending in `exit`, or fast typing) fails the PTY master write with
/// EIO because the slave side is gone. That is the session ending, not an
/// input failure: the child's own exit code must be reported, not an I/O error.
#[test]
fn input_after_child_exit_preserves_wrapped_exit_code() {
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("sh");
    builder.arg("-c");
    // No output before `read`: the echoed line is the wrapper's first chunk,
    // so it is still building the highlighter when the child exits, which is
    // the widest window for late input to hit the closed slave.
    builder.arg("read line; exit 3");
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut writer = pair.master.take_writer().expect("take writer");
    let mut ptty = PtySession::new(child, &*pair.master);
    thread::sleep(Duration::from_millis(300));

    writer.write_all(b"go\n").expect("write line");
    writer.flush().expect("flush line");
    // Keep typing while the child exits so some bytes reach the wrapper after
    // the slave has closed. Writes to our master fail once ptty itself is gone.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if writer
            .write_all(b"y\n")
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    let code = ptty.wait_for_exit(Duration::from_secs(5));
    let output = ptty
        .capture()
        .wait_until(Duration::from_millis(200), |_| false);
    assert_eq!(
        code,
        Some(3),
        "wrapped exit code not preserved; wrapper output: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        !contains_bytes(&output, b"I/O error"),
        "wrapper reported an input I/O error after the child exited: {:?}",
        String::from_utf8_lossy(&output)
    );
}
