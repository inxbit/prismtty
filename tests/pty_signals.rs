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
// A supervised job-control stop lasts until something continues the wrapper,
// so the transition is sampled continuously rather than read after a fixed
// sleep. Nothing in the kernel ends it early: a `setsid` PTY session leader
// that stops stays stopped. Some developer machines run security tooling that
// sends SIGCONT to freshly executed binaries (observed here from a root
// daemon, identified through `si_pid`), which truncates the stop and, because
// SIGCONT also discards a pending SIGSTOP, can hide it from any sampler.
const JOB_CONTROL_OBSERVE_LIMIT: Duration = Duration::from_secs(10);
const JOB_CONTROL_SAMPLE_INTERVAL: Duration = Duration::from_millis(5);
const HEARTBEAT_FREEZE_WINDOW: Duration = Duration::from_millis(160);
/// How long to let the wrapper run before stopping it. The SIGCONT senders
/// mentioned above only react to a freshly executed binary, so a stop that
/// starts outside that window is not truncated and the whole transition can
/// be asserted.
const POST_EXEC_SETTLE: Duration = Duration::from_millis(2000);

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

/// True while `pid` is in a stopped process state. The job-control stop is
/// only observable for a few hundred milliseconds, so this must be cheap
/// enough to poll at millisecond granularity: no process is spawned.
#[cfg(target_os = "macos")]
fn process_is_stopped(pid: libc::pid_t) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    // SAFETY: the buffer is a valid, exclusively borrowed proc_bsdshortinfo of the size passed.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return false;
    }
    // SAFETY: proc_pidinfo reported it filled the whole structure.
    unsafe { info.assume_init() }.pbsi_status == libc::SSTOP
}

#[cfg(target_os = "linux")]
fn process_is_stopped(pid: libc::pid_t) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The comm field is parenthesised and may contain spaces; state follows it.
    let Some(after_comm) = stat.rsplit_once(") ") else {
        return false;
    };
    after_comm.1.trim_start().starts_with('T')
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

/// A signal the caller ignored (`nohup`, or `cmd &` in a non-interactive
/// shell) stays ignored: the wrapper must not catch it and end the job.
#[test]
fn inherited_ignored_signals_do_not_terminate_the_wrapped_job() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let marker_dir = tempfile::tempdir().expect("marker directory");
    let pid_path = marker_dir.path().join("child.pid");
    let done_path = marker_dir.path().join("done");
    let mut wrapped_guard = PidGuard::from_marker(&pid_path);
    let mut command = Command::new(env!("CARGO_BIN_EXE_ptty"));
    command
        .env("PRISMTTY_PID_MARKER", &pid_path)
        .env("PRISMTTY_DONE_MARKER", &done_path)
        .arg("sh")
        .arg("-c")
        .arg("echo $$ > \"$PRISMTTY_PID_MARKER\"; sleep 1; echo done > \"$PRISMTTY_DONE_MARKER\"")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: signal is async-signal-safe and runs in the forked child before exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut ptty = ProcessGuard::new(command.spawn().expect("spawn ptty"));
    wrapped_guard.set(wait_for_pid_file(&pid_path, Duration::from_secs(10)));

    for signal in [libc::SIGHUP, libc::SIGINT] {
        // SAFETY: kill has no memory-safety preconditions; the target is a process this test
        // spawned.
        assert_eq!(unsafe { libc::kill(ptty.pid(), signal) }, 0);
    }

    let status = ptty
        .wait_for_exit(Duration::from_secs(10))
        .expect("wrapper exits when the job finishes");
    assert_eq!(
        status.code(),
        Some(0),
        "an ignored signal ended the wrapped job"
    );
    assert_eq!(
        std::fs::read_to_string(&done_path).ok().as_deref(),
        Some("done\n"),
        "the wrapped job did not finish"
    );
    wrapped_guard.disarm();
}

/// A wrapper started as a session leader with no controlling terminal but a
/// terminal on stdin (`setsid ptty ...`) must not take the wrapped command's
/// PTY as its own controlling terminal: the child then cannot claim it, the
/// spawn fails, and the PTY hangup kills the wrapper. Linux only, since BSD
/// kernels never assign a controlling terminal on open.
#[cfg(target_os = "linux")]
#[test]
fn session_leader_without_controlling_terminal_runs_the_command() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};

    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(pair.master.tty_name().expect("outer PTY tty name"))
        .expect("open outer slave tty");
    let capture = CaptureTask::from_master(&*pair.master);
    let mut command = Command::new(env!("CARGO_BIN_EXE_ptty"));
    command
        .args(["sh", "-c", "echo CHILD-RAN"])
        .stdin(Stdio::from(tty.try_clone().expect("stdin tty")))
        .stdout(Stdio::from(tty.try_clone().expect("stdout tty")))
        .stderr(Stdio::from(tty.try_clone().expect("stderr tty")));
    // SAFETY: setsid is async-signal-safe and runs in the forked child before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut ptty = ProcessGuard::new(command.spawn().expect("spawn ptty"));
    drop(command);
    drop(tty);

    let status = ptty
        .wait_for_exit(Duration::from_secs(10))
        .expect("session-leader wrapper exits");
    let output = capture.wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"CHILD-RAN")
    });
    assert_eq!(
        status.signal(),
        None,
        "wrapper was killed; output: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(status.code(), Some(0));
    assert!(
        contains_bytes(&output, b"CHILD-RAN"),
        "wrapped command never ran: {:?}",
        String::from_utf8_lossy(&output)
    );
}

/// From a terminal the wrapped command is spawned by portable-pty, which closes
/// std's exec-error pipe in the child, so a script whose `#!` interpreter
/// cannot be run must be caught before the spawn: the child aborted with
/// "fatal runtime error" (exit 134) drawn into the output instead of failing
/// with the shell's 127.
#[test]
fn interactive_script_with_unusable_interpreter_exits_127() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("work directory");
    let script = work.path().join("crlf.sh");
    std::fs::write(&script, "#!/bin/sh\r\necho ran\r\n").expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("script is executable");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.cwd(work.path());
    builder.arg("./crlf.sh");
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);

    let status = ptty.wait_for_exit(Duration::from_secs(10));
    let output = ptty.capture().wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"bad interpreter")
    });
    assert_eq!(
        status,
        Some(127),
        "output: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        contains_bytes(&output, b"bad interpreter"),
        "the bad interpreter was not reported: {:?}",
        String::from_utf8_lossy(&output)
    );
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

/// A terminating signal ends the session through `_exit`, which skips the
/// destructor that removes the session's pid file, so each closed window or
/// `kill` used to leave one behind in `pids.d`.
fn assert_pid_file_removed_after(signal: libc::c_int) {
    let runtime_dir = tempfile::tempdir().expect("runtime directory");
    let marker_dir = tempfile::tempdir().expect("signal child marker directory");
    let child_pid_path = marker_dir.path().join("child.pid");
    let mut child_guard = PidGuard::from_marker(&child_pid_path);
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_RUNTIME_DIR", runtime_dir.path());
    builder.env("PRISMTTY_CHILD_PID", &child_pid_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg("echo $$ > \"$PRISMTTY_CHILD_PID\"; echo READY; exec sleep 1000");
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
    let pid_file = runtime_dir
        .path()
        .join("pids.d")
        .join(ptty.pid().to_string());
    assert!(
        pid_file.is_file(),
        "the session did not register: {}",
        pid_file.display()
    );

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(ptty.pid(), signal) }, 0);
    assert_eq!(
        ptty.wait_for_exit(Duration::from_secs(5)),
        Some(128 + signal as u32),
        "interactive wrapper did not report signal-style termination"
    );
    assert!(
        !pid_file.exists(),
        "signal {signal} left the session's pid file behind"
    );
    child_guard.disarm();
}

#[test]
fn terminating_signals_remove_the_session_pid_file() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        assert_pid_file_removed_after(signal);
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
    let spawned_at = Instant::now();
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

    // Stop the wrapper only once it is past the window in which external
    // SIGCONT senders react to a new process; see POST_EXEC_SETTLE.
    let settle = POST_EXEC_SETTLE.saturating_sub(spawned_at.elapsed());
    if !settle.is_zero() {
        thread::sleep(settle);
    }
    let heartbeat_before_stop = file_counter(&heartbeat_path).unwrap_or(0);
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);

    // Sample the whole transition instead of waiting for it and then assuming
    // it holds: the wrapper reports its stop through waitpid, the wrapped child
    // must be stopped too, and the terminal must be cooked while that lasts.
    let mut observed_stop = None;
    let mut observed_child_stopped = false;
    let mut observed_cooked_terminal = false;
    let deadline = Instant::now() + JOB_CONTROL_OBSERVE_LIMIT;
    while Instant::now() < deadline {
        // `let` chains need Rust 1.88; this crate builds on 1.85.
        let wait_status = if observed_stop.is_none() {
            waitpid(
                Pid::from_raw(pid),
                Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG),
            )
            .ok()
        } else {
            None
        };
        if let Some(WaitStatus::Stopped(_, stopped_by)) = wait_status {
            observed_stop = Some(stopped_by);
        }
        if !observed_child_stopped && process_is_stopped(child_pid) {
            observed_child_stopped = true;
        }
        if !observed_cooked_terminal
            && tcgetattr(&tty).is_ok_and(|attrs| {
                attrs.local_flags.contains(LocalFlags::ICANON) && attrs == original
            })
        {
            observed_cooked_terminal = true;
        }
        if observed_stop.is_some() && observed_child_stopped && observed_cooked_terminal {
            break;
        }
        thread::sleep(JOB_CONTROL_SAMPLE_INTERVAL);
    }

    if observed_stop.is_some() {
        assert_eq!(
            observed_stop,
            Some(nix::sys::signal::Signal::SIGSTOP),
            "ptty stopped before completing its supervised job-control transition for signal {signal}"
        );
    }
    assert!(
        observed_stop.is_some(),
        "ptty did not stop through its supervised job-control transition for signal {signal}"
    );
    assert!(
        observed_cooked_terminal,
        "terminal was never cooked around the job-control stop for signal {signal}"
    );
    assert!(
        observed_child_stopped,
        "wrapped child was never stopped while ptty was stopped by signal {signal}"
    );
    // The stop persists until this test resumes the wrapper, so the child must
    // stay frozen: a momentary stop followed by a spurious continue would
    // advance the heartbeat here.
    let frozen_heartbeat = file_counter(&heartbeat_path);
    thread::sleep(HEARTBEAT_FREEZE_WINDOW);
    assert_eq!(
        file_counter(&heartbeat_path),
        frozen_heartbeat,
        "wrapped child kept running while ptty was stopped by signal {signal}"
    );
    let stopped_heartbeat = file_counter(&heartbeat_path).unwrap_or(heartbeat_before_stop);

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

/// SIGSTOP cannot be caught, so the wrapper stops without restoring the
/// terminal, and a shell resets the terminal to its own cooked mode while the
/// job is stopped. Once continued in the foreground the wrapper must enter raw
/// mode again; otherwise Ctrl-C raises SIGINT in the wrapper and ends the
/// session instead of reaching the wrapped command.
#[test]
fn uncatchable_stop_then_continue_reapplies_raw_mode() {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

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
        "trap '' INT; echo $$ > \"$PRISMTTY_CHILD_PID\"; echo READY; while true; do sleep 0.05; done",
    );
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    let spawned_at = Instant::now();
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
    child_guard.set(wait_for_pid_file(&child_pid_path, Duration::from_secs(5)));
    let is_raw = || {
        tcgetattr(&tty)
            .map(|attrs| !attrs.local_flags.contains(LocalFlags::ICANON))
            .unwrap_or(false)
    };
    assert!(
        wait_for_condition(Duration::from_secs(5), is_raw),
        "ptty never entered raw mode"
    );
    // Stop the wrapper only once it is past the window in which external
    // SIGCONT senders react to a new process; see POST_EXEC_SETTLE.
    let settle = POST_EXEC_SETTLE.saturating_sub(spawned_at.elapsed());
    if !settle.is_zero() {
        thread::sleep(settle);
    }

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    assert!(
        wait_for_condition(Duration::from_secs(5), || {
            matches!(
                waitpid(
                    Pid::from_raw(pid),
                    Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG)
                ),
                Ok(WaitStatus::Stopped(..))
            )
        }),
        "ptty did not stop on SIGSTOP"
    );
    tcsetattr(&tty, SetArg::TCSANOW, &original).expect("restore the shell's cooked mode");
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    assert!(
        wait_for_condition(Duration::from_secs(5), is_raw),
        "ptty did not reapply raw mode after SIGCONT"
    );

    let master_fd = pair.master.as_raw_fd().expect("outer PTY master fd");
    let interrupt = [0x03_u8];
    // SAFETY: `interrupt` is valid for reads of its length and the master stays open for the call.
    let written = unsafe { libc::write(master_fd, interrupt.as_ptr().cast(), interrupt.len()) };
    assert_eq!(written, 1, "write Ctrl-C to the outer terminal");
    assert_eq!(
        ptty.wait_for_exit(Duration::from_millis(500)),
        None,
        "Ctrl-C ended the wrapper instead of reaching the wrapped command"
    );

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert_eq!(
        ptty.wait_for_exit(Duration::from_secs(5)),
        Some(128 + libc::SIGTERM as u32),
        "ptty exits after cleanup signal"
    );
    child_guard.disarm();
}

/// Runs `script` with `sh -c` as the session leader of a fresh PTY. With
/// `set -m` the shell does job control like an interactive one: it can start
/// the wrapper as a background job and move it between foreground and
/// background. `PTTY` names the wrapper binary, and the script should write
/// the wrapper's pid to `$PRISMTTY_WRAPPER_PID` and the wrapped child's to
/// `$PRISMTTY_CHILD_PID` so both can be cleaned up.
struct JobControlShell {
    // Dropped first, so the wrapper and the wrapped child are killed before
    // the shell, whatever state the test left them in.
    _guards: (PidGuard, PidGuard),
    session: PtySession,
    master: Box<dyn portable_pty::MasterPty + Send>,
    wrapper_pid_path: std::path::PathBuf,
    child_pid_path: std::path::PathBuf,
    spawned_at: Instant,
    _marker_dir: tempfile::TempDir,
}

impl JobControlShell {
    fn spawn(script: &str) -> Self {
        let marker_dir = tempfile::tempdir().expect("marker directory");
        let wrapper_pid_path = marker_dir.path().join("wrapper.pid");
        let child_pid_path = marker_dir.path().join("child.pid");
        let guards = (
            PidGuard::from_marker(&wrapper_pid_path),
            PidGuard::from_marker(&child_pid_path),
        );
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .expect("openpty");
        let mut builder = CommandBuilder::new("sh");
        builder.env("PTTY", env!("CARGO_BIN_EXE_ptty"));
        builder.env("PRISMTTY_WRAPPER_PID", &wrapper_pid_path);
        builder.env("PRISMTTY_CHILD_PID", &child_pid_path);
        builder.arg("-c");
        builder.arg(script);
        let child = pair
            .slave
            .spawn_command(builder)
            .expect("spawn job-control shell");
        let spawned_at = Instant::now();
        drop(pair.slave);
        Self {
            _guards: guards,
            session: PtySession::new(child, &*pair.master),
            master: pair.master,
            wrapper_pid_path,
            child_pid_path,
            spawned_at,
            _marker_dir: marker_dir,
        }
    }

    fn wrapper_pid(&self) -> libc::pid_t {
        wait_for_pid_file(&self.wrapper_pid_path, Duration::from_secs(10))
    }

    fn child_pid(&self) -> libc::pid_t {
        wait_for_pid_file(&self.child_pid_path, Duration::from_secs(10))
    }

    /// The shell's terminal, opened so its settings can be read and set.
    fn tty(&self) -> File {
        use std::os::unix::fs::OpenOptionsExt;

        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(self.master.tty_name().expect("shell PTY tty name"))
            .expect("open shell tty")
    }

    /// Waits out the window in which external SIGCONT senders react to a new
    /// process, so a stop sent now is not cut short; see POST_EXEC_SETTLE.
    fn settle(&self) {
        let settle = POST_EXEC_SETTLE.saturating_sub(self.spawned_at.elapsed());
        if !settle.is_zero() {
            thread::sleep(settle);
        }
    }

    fn wait_for_output(&self, needle: &[u8]) -> Vec<u8> {
        self.session
            .capture()
            .wait_until(Duration::from_secs(10), |out| contains_bytes(out, needle))
    }
}

/// A wrapper started in the background stops when it reads the terminal, and
/// that read raises SIGTTIN again every time it restarts, so a burst of stop
/// requests queues up before the stop. Once `fg` resumes the wrapper those
/// requests are stale: it must keep running and forward input.
#[test]
fn background_wrapper_keeps_running_after_fg() {
    let shell = JobControlShell::spawn(
        "set -m; \"$PTTY\" sh -c 'echo $$ > \"$PRISMTTY_CHILD_PID\"; read line; echo \"GOT-$((1+1)):$line\"' & echo $! > \"$PRISMTTY_WRAPPER_PID\"; sleep 1; fg %1; echo \"FG-DONE-$((1+1)):$?\"",
    );
    // Typed input waits in the terminal until the wrapper, once in the
    // foreground, reads it.
    let mut writer = shell.master.take_writer().expect("take writer");
    writer.write_all(b"hello\r").expect("type a line");
    writer.flush().expect("flush typed line");

    let output = shell.wait_for_output(b"FG-DONE-2");
    assert!(
        contains_bytes(&output, b"GOT-2:hello") && contains_bytes(&output, b"FG-DONE-2:0"),
        "wrapper stopped again after fg: {:?}",
        String::from_utf8_lossy(&output)
    );
}

/// A wrapper started in the background must not put the shell's terminal in
/// raw mode: the shell owns its settings until `fg` hands the terminal over.
#[test]
fn background_wrapper_enters_raw_mode_only_once_in_the_foreground() {
    use nix::sys::termios::{LocalFlags, tcgetattr};

    let shell = JobControlShell::spawn(
        "set -m; \"$PTTY\" sh -c 'echo $$ > \"$PRISMTTY_CHILD_PID\"; exec sleep 30' & echo $! > \"$PRISMTTY_WRAPPER_PID\"; sleep 2; fg %1",
    );
    let tty = shell.tty();
    let original = tcgetattr(&tty).expect("original tty attrs");
    let sample_until = shell.spawned_at + Duration::from_millis(1500);
    while Instant::now() < sample_until {
        let attrs = tcgetattr(&tty).expect("tty attrs");
        assert!(
            attrs.local_flags.contains(LocalFlags::ISIG),
            "a background wrapper put the shell's terminal in raw mode"
        );
        thread::sleep(Duration::from_micros(500));
    }
    assert_eq!(
        tcgetattr(&tty).expect("tty attrs"),
        original,
        "a background wrapper changed the shell's terminal settings"
    );

    assert!(
        wait_for_condition(Duration::from_secs(10), || {
            tcgetattr(&tty)
                .map(|attrs| !attrs.local_flags.contains(LocalFlags::ICANON))
                .unwrap_or(false)
        }),
        "the wrapper never entered raw mode after fg"
    );
}

/// After a stop the shell owns the terminal again and may change its settings.
/// `bg` continues the wrapper without handing the terminal back, so it must
/// leave those settings alone instead of applying raw mode and then, when its
/// next read stops it, restoring the cooked mode it saved earlier.
#[test]
fn wrapper_resumed_with_bg_leaves_the_shells_terminal_settings_alone() {
    use nix::sys::termios::{LocalFlags, tcgetattr};

    let shell = JobControlShell::spawn(
        "set -m; \"$PTTY\" sh -c 'echo $PPID > \"$PRISMTTY_WRAPPER_PID\"; echo $$ > \"$PRISMTTY_CHILD_PID\"; exec sleep 30'; stty -echo; bg %1; echo \"BG-$((1+1))\"; sleep 30",
    );
    let wrapper_pid = shell.wrapper_pid();
    let tty = shell.tty();
    assert!(
        wait_for_condition(Duration::from_secs(10), || {
            tcgetattr(&tty)
                .map(|attrs| !attrs.local_flags.contains(LocalFlags::ICANON))
                .unwrap_or(false)
        }),
        "the foreground wrapper never entered raw mode"
    );
    shell.settle();

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(wrapper_pid, libc::SIGTSTP) }, 0);
    let output = shell.wait_for_output(b"BG-2");
    assert!(
        contains_bytes(&output, b"BG-2"),
        "the shell never resumed the wrapper with bg: {:?}",
        String::from_utf8_lossy(&output)
    );
    thread::sleep(Duration::from_millis(500));

    let attrs = tcgetattr(&tty).expect("tty attrs");
    assert!(
        !attrs.local_flags.contains(LocalFlags::ECHO)
            && attrs.local_flags.contains(LocalFlags::ICANON),
        "a background wrapper overwrote the shell's terminal settings: {:?}",
        attrs.local_flags
    );
}

/// SIGSTOP cannot be caught, so a wrapper it stops is still in raw mode when
/// the shell takes the terminal back. A termination queued before the wrapper
/// is continued is then handled in the background, and must not restore the
/// cooked mode saved at startup over the settings the shell has made since.
/// The caller ignores SIGCONT here: the wrapper's SIGCONT handler would
/// otherwise race the termination and, when it ran first, give raw mode up
/// without touching the terminal.
#[test]
fn termination_in_the_background_leaves_the_shells_terminal_settings_alone() {
    use nix::sys::termios::{LocalFlags, tcgetattr};

    let shell = JobControlShell::spawn(
        "set -m; trap '' CONT; \"$PTTY\" sh -c 'echo $PPID > \"$PRISMTTY_WRAPPER_PID\"; echo $$ > \"$PRISMTTY_CHILD_PID\"; exec sleep 30'; stty sane -echo; echo \"STOPPED-$((1+1))\"; sleep 30",
    );
    let wrapper_pid = shell.wrapper_pid();
    let child_pid = shell.child_pid();
    let tty = shell.tty();
    assert!(
        wait_for_condition(Duration::from_secs(10), || {
            tcgetattr(&tty)
                .map(|attrs| !attrs.local_flags.contains(LocalFlags::ICANON))
                .unwrap_or(false)
        }),
        "the foreground wrapper never entered raw mode"
    );
    shell.settle();

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(wrapper_pid, libc::SIGSTOP) }, 0);
    let output = shell.wait_for_output(b"STOPPED-2");
    assert!(
        contains_bytes(&output, b"STOPPED-2"),
        "the shell never took the terminal back: {:?}",
        String::from_utf8_lossy(&output)
    );
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(wrapper_pid, libc::SIGTERM) }, 0);
    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(wrapper_pid, libc::SIGCONT) }, 0);
    assert!(
        wait_for_condition(Duration::from_secs(10), || {
            // A background read can reach the watcher before the termination
            // and stop the wrapper again; continue it so the termination runs.
            if process_is_stopped(wrapper_pid) {
                // SAFETY: kill has no memory-safety preconditions; the target is a process this
                // test spawned.
                unsafe { libc::kill(wrapper_pid, libc::SIGCONT) };
            }
            !process_exists(child_pid)
        }),
        "the wrapper never ended the wrapped command after SIGTERM"
    );

    let attrs = tcgetattr(&tty).expect("tty attrs");
    assert!(
        !attrs.local_flags.contains(LocalFlags::ECHO)
            && attrs.local_flags.contains(LocalFlags::ICANON),
        "a background wrapper overwrote the shell's terminal settings on exit: {:?}",
        attrs.local_flags
    );
}

/// The kernel signals a resize to the terminal's foreground job, which is the
/// shell while the wrapper is stopped. The wrapped PTY must still take the new
/// size once `fg` resumes the wrapper.
#[test]
fn resize_while_stopped_reaches_the_wrapped_pty_after_fg() {
    let shell = JobControlShell::spawn(
        "set -m; \"$PTTY\" sh -c 'echo $PPID > \"$PRISMTTY_WRAPPER_PID\"; echo $$ > \"$PRISMTTY_CHILD_PID\"; while true; do echo \"SZ=$(stty size)\"; sleep 0.05; done'; echo \"STOPPED-$((1+1))\"; sleep 1; fg %1",
    );
    let wrapper_pid = shell.wrapper_pid();
    let output = shell.wait_for_output(b"SZ=24 80");
    assert!(
        contains_bytes(&output, b"SZ=24 80"),
        "the wrapped command never reported its size: {:?}",
        String::from_utf8_lossy(&output)
    );
    shell.settle();

    // SAFETY: kill has no memory-safety preconditions; the target is a process this test spawned.
    assert_eq!(unsafe { libc::kill(wrapper_pid, libc::SIGTSTP) }, 0);
    let output = shell.wait_for_output(b"STOPPED-2");
    assert!(
        contains_bytes(&output, b"STOPPED-2"),
        "the wrapper never stopped: {:?}",
        String::from_utf8_lossy(&output)
    );
    shell
        .master
        .resize(PtySize {
            rows: 40,
            cols: 120,
            ..PtySize::default()
        })
        .expect("resize the shell's terminal");

    let output = shell.wait_for_output(b"SZ=40 120");
    assert!(
        contains_bytes(&output, b"SZ=40 120"),
        "the wrapped PTY kept its old size after fg: {:?}",
        String::from_utf8_lossy(&output)
    );
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

/// Closing the outer terminal (a window close, or a dropped ssh link) hangs
/// up the wrapper's controlling tty: stderr becomes unwritable and SIGHUP
/// arrives. Cleanup diagnostics must not panic there, so the wrapper still
/// reaps the wrapped child and exits 128+SIGHUP instead of aborting.
fn assert_hangup_exits_cleanly(script: &str, pending_input: usize) {
    let marker_dir = tempfile::tempdir().expect("hangup marker directory");
    let child_pid_path = marker_dir.path().join("child.pid");
    let mut child_guard = PidGuard::from_marker(&child_pid_path);
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_CHILD_PID", &child_pid_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(script);
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtyChildGuard::new(child);
    let mut capture = CaptureTask::from_master(&*pair.master);
    let ready = capture.wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"READY"));
    assert!(
        contains_bytes(&ready, b"READY"),
        "wrapped child reached READY"
    );
    let child_pid = wait_for_pid_file(&child_pid_path, Duration::from_secs(2));
    child_guard.set(child_pid);

    if pending_input > 0 {
        // Queue more input than the wrapped child's PTY holds, so the input
        // forwarder is blocked writing it when the hangup arrives.
        let master_fd = pair.master.as_raw_fd().expect("PTY master fd");
        // SAFETY: `master_fd` is the open PTY master; dup returns a fresh descriptor this test
        // owns.
        let writer_fd = unsafe { libc::dup(master_fd) };
        assert!(writer_fd >= 0, "duplicate PTY master fd failed");
        // SAFETY: `writer_fd` is a freshly duplicated descriptor that nothing else owns or closes.
        let mut writer = unsafe { File::from_raw_fd(writer_fd) };
        // SAFETY: F_GETFL and F_SETFL take integer arguments and `writer_fd` is open.
        unsafe {
            let flags = libc::fcntl(writer_fd, libc::F_GETFL);
            libc::fcntl(writer_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let chunk = [b'y'; 4096];
        let mut written = 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        while written < pending_input && Instant::now() < deadline {
            match writer.write(&chunk) {
                Ok(n) => written += n,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        thread::sleep(Duration::from_millis(200));
    }

    capture.stop_bounded();
    drop(pair.master);

    assert_eq!(
        ptty.wait_for_exit(Duration::from_secs(10)),
        Some(128 + libc::SIGHUP as u32),
        "wrapper did not exit cleanly after a terminal hangup"
    );
    assert!(
        wait_for_condition(Duration::from_secs(5), || !process_exists(child_pid)),
        "wrapped child survived a terminal hangup"
    );
    child_guard.disarm();
}

#[test]
fn terminal_hangup_exits_cleanly_and_reaps_child() {
    assert_hangup_exits_cleanly(
        "echo $$ > \"$PRISMTTY_CHILD_PID\"; echo READY; exec sleep 1000",
        0,
    );
}

#[test]
fn terminal_hangup_with_blocked_input_forwarder_still_terminates() {
    assert_hangup_exits_cleanly(
        "echo $$ > \"$PRISMTTY_CHILD_PID\"; stty raw -echo; echo READY; exec sleep 1000",
        128 * 1024,
    );
}

/// The terminal's open file description is shared by every process on it, so
/// another program (a libuv tool, a crashed TUI) can leave O_NONBLOCK set on
/// it. The stdin forwarder must wait for input instead of treating EAGAIN as
/// an input failure that kills the wrapped session.
#[test]
fn nonblocking_terminal_stdin_does_not_kill_the_session() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};

    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(pair.master.tty_name().expect("outer PTY tty name"))
        .expect("open outer slave tty");
    // SAFETY: F_GETFL and F_SETFL take integer arguments; `tty` is a descriptor this test owns.
    unsafe {
        let flags = libc::fcntl(tty.as_raw_fd(), libc::F_GETFL);
        assert!(flags >= 0, "F_GETFL failed");
        assert_eq!(
            libc::fcntl(tty.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
            0,
            "F_SETFL failed"
        );
    }
    let capture = CaptureTask::from_master(&*pair.master);
    let mut command = Command::new(env!("CARGO_BIN_EXE_ptty"));
    command
        .args(["sh", "-c", "echo READY; read x; echo got=$x"])
        .stdin(Stdio::from(tty.try_clone().expect("stdin tty")))
        .stdout(Stdio::from(tty.try_clone().expect("stdout tty")))
        .stderr(Stdio::from(tty.try_clone().expect("stderr tty")));
    // SAFETY: setsid and ioctl are async-signal-safe and run in the forked child before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut ptty = ProcessGuard::new(command.spawn().expect("spawn ptty"));
    drop(command);
    drop(tty);

    let ready = capture.wait_until(Duration::from_secs(10), |out| contains_bytes(out, b"READY"));
    assert!(
        contains_bytes(&ready, b"READY"),
        "wrapped command never started: {:?}",
        String::from_utf8_lossy(&ready)
    );
    // Give the forwarder time to find the non-blocking stdin empty.
    thread::sleep(Duration::from_millis(300));
    let mut writer = pair.master.take_writer().expect("PTY master writer");
    writer.write_all(b"hello\r").expect("type into outer PTY");
    writer.flush().expect("flush outer PTY");

    let status = ptty
        .wait_for_exit(Duration::from_secs(10))
        .expect("wrapper exits after the child reads its line");
    let output = capture.wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"got=hello")
    });
    assert_eq!(
        status.signal(),
        None,
        "output: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        contains_bytes(&output, b"got=hello"),
        "wrapped session did not receive input: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        status.code(),
        Some(0),
        "output: {:?}",
        String::from_utf8_lossy(&output)
    );
}
