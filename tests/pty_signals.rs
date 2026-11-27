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
    process_state(pid).is_some_and(|(state, _)| state == 'T')
}

/// The state letter and the parent pid of `pid`.
#[cfg(target_os = "linux")]
fn process_state(pid: libc::pid_t) -> Option<(char, libc::pid_t)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field is parenthesised and may contain spaces; state follows it.
    let mut fields = stat.rsplit_once(") ")?.1.split_whitespace();
    let state = fields.next()?.chars().next()?;
    Some((state, fields.next()?.parse().ok()?))
}

/// True while the wrapped shell `pid` is stopped with its job. dash starts a
/// simple command such as `sleep` with vfork, and a stop that lands before
/// the vforked child has exec'd stops that child while the shell keeps
/// waiting for it in state D, so the shell never reaches T until the job is
/// continued. Its job is just as frozen then, so a stopped child counts.
#[cfg(target_os = "linux")]
fn shell_job_is_stopped(pid: libc::pid_t) -> bool {
    match process_state(pid) {
        Some(('T', _)) => true,
        Some(('D', _)) => std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .filter_map(process_state)
            .any(|(state, parent)| state == 'T' && parent == pid),
        _ => false,
    }
}

/// bash, the /bin/sh of macOS, starts commands with fork, so the shell
/// itself stops with its job.
#[cfg(target_os = "macos")]
fn shell_job_is_stopped(pid: libc::pid_t) -> bool {
    process_is_stopped(pid)
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

/// A wrapped command killed by a real-time signal reports `128 + signal`,
/// like any other signal death. Linux only: macOS has no real-time signals.
#[cfg(target_os = "linux")]
#[test]
fn child_killed_by_a_real_time_signal_reports_128_plus_signal() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    let home = tempfile::tempdir().expect("scratch home");
    let config = home.path().join("empty.yml");
    std::fs::write(&config, "rules: []\n").expect("empty config");
    for signal in [50, 64] {
        let script = format!("kill -{signal} $$");
        let bare = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .status()
            .expect("run sh without the wrapper");
        assert_eq!(
            bare.signal(),
            Some(signal),
            "control: sh without the wrapper did not die from signal {signal}"
        );

        let stderr_path = home.path().join(format!("stderr-{signal}"));
        let ptty = Command::new(env!("CARGO_BIN_EXE_ptty"))
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("PRISMTTY_RUNTIME_DIR", home.path().join("runtime"))
            .arg("-c")
            .arg(&config)
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&stderr_path).expect("stderr file"))
            .spawn()
            .expect("spawn noninteractive ptty");
        let mut ptty = ProcessGuard::new(ptty);
        let status = ptty
            .wait_for_exit(Duration::from_secs(10))
            .expect("wrapper exits after its command was killed");
        let stderr = std::fs::read_to_string(&stderr_path).expect("read stderr");
        assert_eq!(
            (status.code(), stderr.as_str()),
            (Some(128 + signal), ""),
            "wrapped command killed by signal {signal}"
        );
    }
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

/// From a terminal, a script whose `#!` interpreter cannot be run is caught
/// before the spawn, so the message names the bad interpreter and the exit
/// code is the shell's 127. When portable-pty spawned the command here, the
/// child aborted with "fatal runtime error" (exit 134) drawn into the output.
#[test]
fn interactive_script_with_unusable_interpreter_exits_127() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("work directory");
    let mut scripts = vec![("crlf.sh", "#!/bin/sh\r\necho ran\r\n")];
    // Linux ends the `#!` line of a short file with no newline at its end.
    // macOS reports ENOEXEC for it instead, and execvp runs it with /bin/sh.
    if cfg!(target_os = "linux") {
        scripts.push(("short.sh", "#!/nonexistent/prismtty-interp"));
    }
    for (name, body) in scripts {
        let script = work.path().join(name);
        std::fs::write(&script, body).expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("script is executable");
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .expect("openpty");
        let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
        builder.cwd(work.path());
        builder.arg(format!("./{name}"));
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
            "{name} output: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert!(
            contains_bytes(&output, b"bad interpreter"),
            "{name}: the bad interpreter was not reported: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

/// From a terminal, an execve that fails after the fork is reported with the
/// shell's exit code and a prismtty message. The child used to abort with
/// "fatal runtime error" (exit 134) instead: portable-pty closed std's
/// exec-error pipe in it. On macOS a program for another CPU type fails there
/// (126); on Linux a script whose interpreter is a script with a missing
/// interpreter (127).
#[test]
fn interactive_exec_failure_after_the_fork_uses_shell_exit_codes() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("work directory");
    let config = work.path().join("empty.yml");
    std::fs::write(&config, "rules: []\n").expect("config writes");
    let (program, code) = if cfg!(target_os = "linux") {
        let inner = work.path().join("inner");
        std::fs::write(&inner, "#!/nonexistent/prismtty-interp\n").expect("write inner");
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755))
            .expect("inner is executable");
        let outer = work.path().join("outer.sh");
        std::fs::write(&outer, format!("#!{}\necho ran\n", inner.display())).expect("write outer");
        (outer, 127)
    } else {
        // A 64-bit Mach-O header for PowerPC, which no Mac runs.
        let mut header = Vec::new();
        for word in [0xfeed_facf_u32, 0x0100_0012, 0, 2, 0, 0, 0, 0] {
            header.extend_from_slice(&word.to_le_bytes());
        }
        header.resize(header.len() + 4096, 0);
        let binary = work.path().join("ppc");
        std::fs::write(&binary, header).expect("write binary");
        (binary, 126)
    };
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("program is executable");

    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env_clear();
    builder.env("HOME", work.path());
    builder.env("PRISMTTY_RUNTIME_DIR", work.path().join("runtime"));
    builder.env("PATH", "/usr/bin:/bin");
    builder.cwd(work.path());
    builder.arg("--config");
    builder.arg(&config);
    builder.arg("--");
    builder.arg(&program);
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);

    let status = ptty.wait_for_exit(Duration::from_secs(10));
    let output = ptty.capture().wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"Unable to spawn")
    });
    assert_eq!(status, Some(code), "{:?}", String::from_utf8_lossy(&output));
    assert!(
        contains_bytes(&output, b"prismtty: PTY error: Unable to spawn"),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
    // Not "fatal runtime error": the highlighter would color "error".
    assert!(
        !contains_bytes(&output, b"output.write(&bytes)"),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
}

/// From a terminal too, a descriptor the caller hands the wrapped command on
/// purpose (`cmd 3<file`, a process substitution) reaches it as it does
/// without the wrapper, and nothing prismtty opens itself joins it: the
/// command lists the same descriptors as when the shell runs it directly,
/// with `--trace-io` on as well.
#[test]
fn interactive_command_inherits_exactly_the_callers_extra_descriptors() {
    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("empty.yml");
    std::fs::write(&config, "rules: []\n").expect("config writes");
    let fd7 = home.path().join("fd7.out");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new("/bin/sh");
    builder.env_clear();
    builder.env("HOME", home.path());
    builder.env("PRISMTTY_RUNTIME_DIR", home.path().join("runtime"));
    builder.env("PATH", "/usr/bin:/bin");
    builder.env("PTTY", env!("CARGO_BIN_EXE_ptty"));
    builder.env("CONFIG", &config);
    builder.env("TRACE", home.path().join("trace.log"));
    builder.env("FD7", &fd7);
    builder.args([
        "-c",
        "list='echo via-fd7 >&7; echo fd-list; exec ls /dev/fd'; \
         echo direct-start; /bin/sh -c \"$list\" 7>>\"$FD7\"; echo direct-end; \
         echo plain-start; \"$PTTY\" --config \"$CONFIG\" -- /bin/sh -c \"$list\" 7>>\"$FD7\"; \
         echo plain-end=$?; \
         echo traced-start; \"$PTTY\" --config \"$CONFIG\" --trace-io \"$TRACE\" -- \
         /bin/sh -c \"$list\" 7>>\"$FD7\"; echo traced-end=$?",
    ]);
    let child = pair.slave.spawn_command(builder).expect("spawn shell");
    drop(pair.slave);
    let mut shell = PtySession::new(child, &*pair.master);

    let status = shell.wait_for_exit(Duration::from_secs(20));
    let output = shell.capture().wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"traced-end=")
    });
    let text = String::from_utf8_lossy(&output).replace('\r', "");
    assert_eq!(status, Some(0), "{text:?}");
    let listed = |start: &str, end: &str| -> Vec<u32> {
        // Sorted: on a terminal `ls` fills columns, so with enough
        // descriptors to need a second row the words are not in order.
        let mut fds: Vec<u32> = text
            .split(start)
            .nth(1)
            .and_then(|rest| rest.split(end).next())
            .and_then(|run| run.split("fd-list").nth(1))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect();
        fds.sort_unstable();
        fds
    };
    let direct = listed("direct-start", "direct-end");
    assert!(direct.contains(&7), "direct: {text:?}");
    assert_eq!(listed("plain-start", "plain-end=0"), direct, "{text:?}");
    assert_eq!(listed("traced-start", "traced-end=0"), direct, "{text:?}");
    assert_eq!(
        std::fs::read_to_string(&fd7).expect("fd 7 output"),
        "via-fd7\n".repeat(3),
        "{text:?}"
    );
}

/// A current directory the user cannot enter makes the wrapped command start
/// in the home directory, from a terminal and with stdin redirected. As in
/// portable-pty's spawn, a HOME that is not UTF-8 is passed over for the home
/// directory in the passwd database.
#[test]
fn command_from_an_unenterable_directory_starts_in_the_passwd_home() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // root can enter any directory, so nothing falls back.
        return;
    }
    // SAFETY: passwd is a plain C struct that getpwuid_r fills before it is read.
    let mut entry = unsafe { std::mem::zeroed::<libc::passwd>() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut found = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and the buffer length is its own;
    // getpwuid_r is the thread-safe lookup, as other tests spawn concurrently.
    let result = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if result != 0 || found.is_null() || entry.pw_dir.is_null() {
        // No passwd entry or home (some containers): nothing to compare with.
        return;
    }
    // SAFETY: getpwuid_r succeeded, so pw_dir points into `buffer` at a
    // NUL-terminated string.
    let passwd_home = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    let Ok(passwd_home) = std::fs::canonicalize(OsStr::from_bytes(passwd_home.to_bytes())) else {
        // A home directory that does not exist (packaging sandboxes).
        return;
    };
    let work = tempfile::tempdir().expect("work directory");
    let locked = work.path().join("locked");
    std::fs::create_dir(&locked).expect("locked directory");
    let config = work.path().join("empty.yml");
    std::fs::write(&config, "rules: []\n").expect("config writes");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new("/bin/sh");
    builder.env_clear();
    builder.env(
        "HOME",
        OsStr::from_bytes(b"/nonexistent/prismtty-home-\xff"),
    );
    builder.env("PRISMTTY_RUNTIME_DIR", work.path().join("runtime"));
    builder.env("PATH", "/usr/bin:/bin");
    builder.env("PTTY", env!("CARGO_BIN_EXE_ptty"));
    builder.env("CONFIG", &config);
    builder.env("LOCKED", &locked);
    builder.cwd(work.path());
    builder.args([
        "-c",
        "show='echo \"cwd=$(/bin/pwd -P)\"'; cd \"$LOCKED\" && chmod 000 . || exit 9; \
         \"$PTTY\" --config \"$CONFIG\" -- /bin/sh -c \"$show\"; echo terminal=$?; \
         \"$PTTY\" --config \"$CONFIG\" -- /bin/sh -c \"$show\" </dev/null; echo redirected=$?; \
         chmod 755 \"$LOCKED\"",
    ]);
    let child = pair.slave.spawn_command(builder).expect("spawn shell");
    drop(pair.slave);
    let mut shell = PtySession::new(child, &*pair.master);

    let status = shell.wait_for_exit(Duration::from_secs(20));
    let output = shell.capture().wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"redirected=")
    });
    let raw = String::from_utf8_lossy(&output).replace('\r', "");
    let mut text = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // Drop a CSI sequence: the highlighter may color the path.
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        text.push(ch);
    }
    let expected = format!("cwd={}\n", passwd_home.display());
    assert_eq!(status, Some(0), "{text:?}");
    assert_eq!(
        text.matches(&expected).count(),
        2,
        "{expected:?} in {text:?}"
    );
    for line in ["terminal=0", "redirected=0"] {
        assert!(text.contains(line), "{line} missing: {text:?}");
    }
}

/// In an iTerm2 session the wrapped shell must not see the variables that
/// turn iTerm2's shell integration on a second time: they move to
/// PRISMTTY_PARENT_* and the session is marked as nested. SHELL is set, as
/// for any wrapped command.
#[test]
fn interactive_command_gets_the_iterm_guard_environment() {
    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("empty.yml");
    std::fs::write(&config, "rules: []\n").expect("config writes");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env_clear();
    builder.env("HOME", home.path());
    builder.env("PRISMTTY_RUNTIME_DIR", home.path().join("runtime"));
    builder.env("PATH", "/usr/bin:/bin");
    builder.env("TERM_PROGRAM", "iTerm.app");
    builder.env("LC_TERMINAL", "iTerm.app");
    builder.env("ITERM_SESSION_ID", "w0t0p0:prismtty-test");
    builder.arg("--config");
    builder.arg(&config);
    builder.args(["--", "/usr/bin/env"]);
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);

    let status = ptty.wait_for_exit(Duration::from_secs(10));
    let output = ptty.capture().wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"PRISMTTY_NESTED_ITERM=")
    });
    let text = String::from_utf8_lossy(&output).replace('\r', "");
    assert_eq!(status, Some(0), "{text:?}");
    let env: Vec<&str> = text.lines().collect();
    for line in [
        "PRISMTTY_PARENT_TERM_PROGRAM=iTerm.app",
        "PRISMTTY_PARENT_LC_TERMINAL=iTerm.app",
        "PRISMTTY_PARENT_ITERM_SESSION_ID=w0t0p0:prismtty-test",
        "ITERM_SHELL_INTEGRATION_INSTALLED=prismtty",
        "ITERM2_SQUELCH_MARK=1",
        "PRISMTTY_NESTED_ITERM=1",
        "PATH=/usr/bin:/bin",
    ] {
        assert!(env.contains(&line), "{line} missing: {text:?}");
    }
    for name in ["TERM_PROGRAM=", "LC_TERMINAL=", "ITERM_SESSION_ID="] {
        assert!(
            !env.iter().any(|line| line.starts_with(name)),
            "{name} reached the command: {text:?}"
        );
    }
    assert!(
        env.iter()
            .any(|line| line.len() > "SHELL=".len() && line.starts_with("SHELL=")),
        "SHELL missing: {text:?}"
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
        if !observed_child_stopped && shell_job_is_stopped(child_pid) {
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

/// The shell that runs a `JobControlShell` script. macOS's sh is bash 3.2,
/// which after `set -m` in a non-interactive shell neither hands the terminal
/// to a job on `fg` nor notices that its foreground job stopped. The dash that
/// macOS also ships does both, as do the dash and newer bash behind sh on
/// Linux.
#[cfg(target_os = "macos")]
const JOB_CONTROL_SHELL: &str = "/bin/dash";
#[cfg(not(target_os = "macos"))]
const JOB_CONTROL_SHELL: &str = "sh";

/// Runs `script` with `JOB_CONTROL_SHELL -c` as the session leader of a fresh
/// PTY. With `set -m` the shell does job control like an interactive one: it
/// can start the wrapper as a background job and move it between foreground
/// and background. `PTTY` names the wrapper binary, and the script should
/// write the wrapper's pid to `$PRISMTTY_WRAPPER_PID` and the wrapped child's
/// to `$PRISMTTY_CHILD_PID` so both can be cleaned up.
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
        let mut builder = CommandBuilder::new(JOB_CONTROL_SHELL);
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

/// Whether the wrapper ends the session at the wrapped command's exit while
/// a background job still holds the terminal: on macOS, which revokes the
/// terminal at the exit, and on Linux from 4.12, where prismtty learns of
/// the exit itself. Older Linux kernels read the terminal to its EOF.
fn session_ends_at_command_exit() -> bool {
    if cfg!(target_os = "macos") {
        return true;
    }
    if !cfg!(target_os = "linux") {
        return false;
    }
    // SAFETY: utsname is a plain C struct of byte arrays; all-zero bytes are a
    // valid value that uname overwrites.
    let mut name = unsafe { std::mem::zeroed::<libc::utsname>() };
    // SAFETY: `name` is a valid, exclusively borrowed utsname for the call.
    if unsafe { libc::uname(&mut name) } != 0 {
        return false;
    }
    // SAFETY: uname filled `release` with a NUL-terminated string that lives
    // as long as `name`.
    let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) }.to_string_lossy();
    let mut parts = release.splitn(2, '.');
    let major = parts.next().and_then(|major| major.parse::<u64>().ok());
    let minor = parts
        .next()
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|minor| minor.parse::<u64>().ok());
    matches!((major, minor), (Some(major), Some(minor)) if (major, minor) >= (4, 12))
}

/// A wrapped command that leaves a background job behind and then ends with
/// `last`. The job ignores SIGHUP and SIGTERM, keeps the PTY slave open on
/// its stdout and stderr, and holds the write end of its `alive` FIFO for as
/// long as it lives. It reports its pid and whether its stdout is that
/// terminal, then waits on its FIFO gate. When the gate opens it records that
/// it is still alive, writes once more to the terminal and exits. The command
/// waits for the job's report before it prints its marker and ends.
fn command_leaving_a_job_behind(last: &str) -> String {
    format!(
        "sh -c 'exec 3>\"$1/alive\"; trap \"\" HUP TERM; \
         if [ -t 1 ]; then held=terminal; else held=none; fi; \
         echo \"$$ $held\" > \"$1/ready\"; read gate < \"$1/gate\"; : > \"$1/released\"; \
         echo late' job \"$1\" &\n\
         read job < \"$1/ready\"\n\
         echo \"$job\" > \"$1/job\"\n\
         {last}\n"
    )
}

/// The background job of [`command_leaving_a_job_behind`], reached only
/// through its private FIFOs. It is ended by opening its gate and never by a
/// signal to its pid, which can belong to another process once the job has
/// exited. Dropping this, also while a failed test unwinds, opens the gate if
/// the job still waits there and waits for the job to be gone.
struct LeftJob {
    dir: tempfile::TempDir,
    /// The read end of the FIFO the job holds open for writing while it
    /// lives: it reads EOF once the job is gone (or before it started).
    alive: File,
}

impl LeftJob {
    /// A scratch directory with the job's FIFOs, an empty config and a
    /// private home and runtime directory for the wrapper.
    fn new() -> Self {
        use std::os::unix::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().expect("job directory");
        for fifo in ["ready", "gate", "alive"] {
            let path =
                std::ffi::CString::new(dir.path().join(fifo).into_os_string().into_encoded_bytes())
                    .expect("FIFO path");
            // SAFETY: `path` is a valid NUL-terminated string for the call.
            let made = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
            assert_eq!(made, 0, "mkfifo {fifo}");
        }
        std::fs::write(dir.path().join("empty.yml"), "rules: []\n").expect("empty config");
        for subdirectory in ["home", "runtime"] {
            std::fs::create_dir(dir.path().join(subdirectory)).expect("private directory");
        }
        let alive = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(dir.path().join("alive"))
            .expect("open the job's alive FIFO");
        Self { dir, alive }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Whether the job (which must have started) has exited.
    fn is_gone(&mut self) -> bool {
        let mut byte = [0_u8; 1];
        matches!(self.alive.read(&mut byte), Ok(0))
    }

    /// Opens the gate if a process waits on it. Fails with ENXIO when none
    /// does.
    fn open_gate(&self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(self.path().join("gate"))?
            .write_all(b"go\n")
    }

    /// Opens the gate and requires the job to say it saw it open, so it was
    /// still alive then, and then to be gone. The job must be waiting on the
    /// gate, so a gate nobody opens for reading within the bound is a failure.
    fn release(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.open_gate() {
                Ok(()) => break,
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    assert!(
                        Instant::now() < deadline,
                        "the background job is not waiting on its gate"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("open the job's gate: {error}"),
            }
        }
        assert!(
            wait_for_condition(Duration::from_secs(10), || self
                .path()
                .join("released")
                .exists()),
            "the background job never saw its gate open"
        );
        assert!(
            wait_for_condition(Duration::from_secs(10), || self.is_gone()),
            "the background job did not exit after its gate opened"
        );
    }

    /// Checks that the job the command left held the wrapped terminal.
    fn assert_held_the_terminal(&self) {
        let record = std::fs::read_to_string(self.path().join("job"))
            .expect("the command did not record its background job");
        assert_eq!(
            record.split_whitespace().nth(1),
            Some("terminal"),
            "the background job's stdout was not the wrapped terminal: {record:?}"
        );
    }
}

impl Drop for LeftJob {
    fn drop(&mut self) {
        use std::os::unix::fs::OpenOptionsExt;

        if self.is_gone() {
            return;
        }
        // A job still parked on its gate leaves through it; one still waiting
        // to report on `ready` finds a reader there and goes on to the gate.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !self.is_gone() {
            let _ = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(self.path().join("ready"));
            let _ = self.open_gate();
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// The wrapper's arguments for running `script` with `dir` as `$1`.
fn job_command_arguments(dir: &std::path::Path, script: &str) -> Vec<std::ffi::OsString> {
    let mut arguments: Vec<std::ffi::OsString> = ["-c".into(), dir.join("empty.yml").into()].into();
    arguments.extend(["--", "sh", "-c", script, "sh"].map(std::ffi::OsString::from));
    arguments.push(dir.into());
    arguments
}

/// Waits for the wrapper to end while the job left behind holds the
/// terminal and returns its status. Where the session ends at the command's
/// exit, the wrapper must return with the job still waiting on its gate, and
/// the job must then see the gate open, so it outlived the wrapper. Where it
/// does not, the gate is opened first and the wrapper ends at the terminal's
/// EOF, as before.
fn wrapper_status_leaving_a_job<T>(
    job: &mut LeftJob,
    mut wait_for_exit: impl FnMut(Duration) -> Option<T>,
) -> T {
    let ends_at_exit = session_ends_at_command_exit();
    if !ends_at_exit {
        job.release();
    }
    let status = wait_for_exit(Duration::from_secs(20)).unwrap_or_else(|| {
        if ends_at_exit {
            panic!(
                "the wrapper did not exit after the wrapped command did while its background \
                 job still held the terminal"
            )
        }
        panic!("the wrapper did not exit after its background job let the terminal go")
    });
    if ends_at_exit {
        job.release();
    }
    job.assert_held_the_terminal();
    status
}

/// Everything `capture` read, once its source has reached its end.
fn completed_output(capture: &CaptureTask) -> Vec<u8> {
    let finished = capture.finished_flag();
    assert!(
        wait_for_condition(Duration::from_secs(10), || finished.load(Ordering::Acquire)),
        "the wrapper's output did not reach its end"
    );
    capture.wait_until(Duration::ZERO, |_| true)
}

/// `bytes` with CSI sequences (the highlighter's colors) taken out.
fn without_csi(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                index += 1;
            }
            index += 1;
            continue;
        }
        plain.push(bytes[index]);
        index += 1;
    }
    plain
}

/// Runs `script` under the wrapper on a terminal of its own and returns the
/// wrapper's exit status and everything that reached the terminal.
fn run_on_terminal_leaving_a_job(script: &str) -> (u32, Vec<u8>) {
    let mut job = LeftJob::new();
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("HOME", job.path().join("home"));
    builder.env("XDG_CONFIG_HOME", job.path().join("home"));
    builder.env("PRISMTTY_RUNTIME_DIR", job.path().join("runtime"));
    builder.args(job_command_arguments(job.path(), script));
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);
    let status = wrapper_status_leaving_a_job(&mut job, |timeout| ptty.wait_for_exit(timeout));
    (status, without_csi(&completed_output(ptty.capture())))
}

// A command that exits while a job it started keeps the terminal open ends
// the session: the wrapper reports the command's status at once, the
// command's last output is drawn, and the job is left running, as it would
// be without the wrapper. On Linux the PTY stays open for the job, so the
// wrapper ends at the command's exit, not at the PTY's EOF; macOS revokes
// the terminal at the exit and ends the same way.
#[test]
fn session_ends_with_the_command_while_its_background_job_holds_the_terminal() {
    let (status, output) = run_on_terminal_leaving_a_job(&command_leaving_a_job_behind(
        "printf '%s\\n' eos-last-marker; exit 7",
    ));
    assert_eq!(status, 7, "output: {:?}", String::from_utf8_lossy(&output));
    assert!(
        contains_bytes(&output, b"eos-last-marker"),
        "the command's last output was lost: {:?}",
        String::from_utf8_lossy(&output)
    );
}

// The same with stdin and stdout not a terminal: no raw mode, no live
// output, and the wrapper still ends with the command.
#[test]
fn session_ends_with_the_command_while_its_background_job_holds_the_terminal_without_a_terminal_on_stdin()
 {
    use std::process::{Command, Stdio};

    let mut job = LeftJob::new();
    let script = command_leaving_a_job_behind("printf '%s\\n' eos-last-marker; exit 7");
    let ptty = Command::new(env!("CARGO_BIN_EXE_ptty"))
        .env("HOME", job.path().join("home"))
        .env("XDG_CONFIG_HOME", job.path().join("home"))
        .env("PRISMTTY_RUNTIME_DIR", job.path().join("runtime"))
        .args(job_command_arguments(job.path(), &script))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ptty");
    let mut ptty = ProcessGuard::new(ptty);
    let stdout = ptty.child_mut().stdout.take().expect("piped stdout");
    let stdout_fd = stdout.as_raw_fd();
    let capture = CaptureTask::from_reader(stdout, stdout_fd);
    let status = wrapper_status_leaving_a_job(&mut job, |timeout| ptty.wait_for_exit(timeout));
    let output = without_csi(&completed_output(&capture));
    assert_eq!(
        status.code(),
        Some(7),
        "the wrapper did not exit with the command's status; output: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        contains_bytes(&output, b"eos-last-marker"),
        "the command's last output was lost: {:?}",
        String::from_utf8_lossy(&output)
    );
}

// A command killed by a signal while its job holds the terminal is reported
// as 128 plus the signal, with its output drawn.
#[test]
fn session_reports_a_command_killed_while_its_background_job_holds_the_terminal() {
    for (signal, name) in [(libc::SIGTERM, "TERM"), (libc::SIGKILL, "KILL")] {
        let (status, output) = run_on_terminal_leaving_a_job(&command_leaving_a_job_behind(
            &format!("printf '%s\\n' eos-last-marker; kill -{name} $$"),
        ));
        assert_eq!(
            status,
            128 + signal as u32,
            "SIG{name}: output: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert!(
            contains_bytes(&output, b"eos-last-marker"),
            "SIG{name}: the command's last output was lost: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

// A test that fails while the job it left still waits on its gate ends the
// job through the gate as its fixture is dropped, not by a signal to a pid
// that could belong to another process by then.
#[test]
fn left_job_leaves_through_its_gate_when_its_test_ends_early() {
    use std::process::{Command, Stdio};

    let job = LeftJob::new();
    let alive = job.alive.try_clone().expect("keep the alive FIFO");
    let status = Command::new("sh")
        .args(["-c", &command_leaving_a_job_behind("exit 0"), "sh"])
        .arg(job.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run the command");
    assert!(status.success(), "the command failed: {status:?}");
    let mut byte = [0_u8; 1];
    assert!(
        matches!((&alive).read(&mut byte), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "the job is not waiting on its gate"
    );
    drop(job);
    assert!(
        matches!((&alive).read(&mut byte), Ok(0)),
        "the job outlived its fixture"
    );
}

// A last word with no line end is held back by the live highlighter in case
// the command continues it. When the command exits instead, the word is
// drawn once, as the session ends.
#[test]
fn held_back_last_word_is_drawn_once_when_the_command_exits_leaving_a_job() {
    let (status, output) =
        run_on_terminal_leaving_a_job(&command_leaving_a_job_behind("printf eosheldtail; exit 0"));
    assert_eq!(status, 0, "output: {:?}", String::from_utf8_lossy(&output));
    let drawn = output
        .windows(b"eosheldtail".len())
        .filter(|window| *window == b"eosheldtail")
        .count();
    assert_eq!(
        drawn,
        1,
        "the held-back last word was not drawn exactly once: {:?}",
        String::from_utf8_lossy(&output)
    );
}

/// The lowest descriptor the wrapper under [`TIGHT_DESCRIPTOR_LIMIT`] may
/// still get: descriptors 3 to one below it are taken when it starts.
const TIGHT_DESCRIPTOR_FLOOR: libc::c_int = 55;
/// A limit on open descriptors that leaves the wrapper just what it needs.
const TIGHT_DESCRIPTOR_LIMIT: libc::rlim_t = 64;
/// The wrapped command for the tight-limit test: it counts the descriptors
/// from 3 to below the floor that it inherited (the caller's descriptors
/// reach the command), then prints its word and exits 23.
const COUNT_INHERITED_DESCRIPTORS: &str = "inherited=0; fd=3; while [ $fd -lt 55 ]; do \
     if [ -e /dev/fd/$fd ]; then inherited=$((inherited+1)); fi; fd=$((fd+1)); done; \
     printf 'inherited=%s\\n' \"$inherited\"; printf own-word; exit 23";

/// Starts the wrapper as a caller short of descriptors would: from a child
/// forked by hand that closes every descriptor above stderr, opens
/// /dev/null on every one from 3 to below [`TIGHT_DESCRIPTOR_FLOOR`] without
/// close-on-exec, lowers RLIMIT_NOFILE to [`TIGHT_DESCRIPTOR_LIMIT`] and
/// execs. Nothing of the test process (std's close-on-exec descriptors,
/// other tests' files) can leave a gap among them at the exec. `stdio`
/// becomes the wrapper's 0, 1 and 2; with `terminal`, its stdin becomes its
/// controlling terminal in a session of its own. Returns the child's pid.
fn spawn_wrapper_short_of_descriptors(
    arguments: &[std::ffi::OsString],
    home: &std::path::Path,
    runtime: &std::path::Path,
    stdio: [libc::c_int; 3],
    terminal: bool,
) -> libc::pid_t {
    use std::ffi::CString;

    let program = CString::new(env!("CARGO_BIN_EXE_ptty")).expect("program path");
    let argv: Vec<CString> = std::iter::once(program.clone())
        .chain(
            arguments
                .iter()
                .map(|argument| CString::new(argument.as_encoded_bytes()).expect("argument")),
        )
        .collect();
    let home = home.as_os_str().as_encoded_bytes();
    let runtime = runtime.as_os_str().as_encoded_bytes();
    let envp: Vec<CString> = [
        b"PATH=/usr/bin:/bin".to_vec(),
        [b"HOME=".as_slice(), home].concat(),
        [b"XDG_CONFIG_HOME=".as_slice(), home].concat(),
        [b"PRISMTTY_RUNTIME_DIR=".as_slice(), runtime].concat(),
        b"TERM=xterm-256color".to_vec(),
    ]
    .into_iter()
    .map(|entry| CString::new(entry).expect("environment entry"))
    .collect();
    let argv_pointers: Vec<*const libc::c_char> = argv
        .iter()
        .map(|argument| argument.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp_pointers: Vec<*const libc::c_char> = envp
        .iter()
        .map(|entry| entry.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `current` is a valid, exclusively borrowed rlimit for the call.
    let got = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) };
    assert_eq!(got, 0, "getrlimit");
    let highest = libc::c_int::try_from(current.rlim_cur.min(65_536)).unwrap_or(65_536);
    let limit = libc::rlimit {
        rlim_cur: TIGHT_DESCRIPTOR_LIMIT,
        rlim_max: TIGHT_DESCRIPTOR_LIMIT,
    };

    // SAFETY: fork has no memory-safety preconditions. Between fork and exec
    // the child calls only async-signal-safe functions (setsid, dup2, ioctl,
    // close, open, setrlimit, execve, _exit) on data prepared before the fork,
    // and allocates nothing.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        // SAFETY: see the fork above.
        unsafe {
            if terminal && libc::setsid() == -1 {
                libc::_exit(126);
            }
            for (target, fd) in stdio.into_iter().enumerate() {
                if libc::dup2(fd, target as libc::c_int) == -1 {
                    libc::_exit(126);
                }
            }
            if terminal && libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                libc::_exit(126);
            }
            for fd in 3..highest {
                libc::close(fd);
            }
            loop {
                let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                if fd < 0 {
                    libc::_exit(126);
                }
                if fd >= TIGHT_DESCRIPTOR_FLOOR {
                    libc::close(fd);
                    break;
                }
            }
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == -1 {
                libc::_exit(126);
            }
            libc::execve(
                program.as_ptr(),
                argv_pointers.as_ptr(),
                envp_pointers.as_ptr(),
            );
            libc::_exit(127);
        }
    }
    pid
}

/// Waits for this test's own child `pid` and returns its exit status; kills
/// and reaps it if it has not exited in time. The pid cannot be reused while
/// the child is unreaped, so the kill reaches only that child.
fn wait_for_own_child(pid: libc::pid_t, timeout: Duration) -> Option<libc::c_int> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status = 0;
        // SAFETY: `status` is a valid, exclusively borrowed int for the call.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            return Some(status);
        }
        if Instant::now() >= deadline {
            // SAFETY: kill and waitpid have no memory-safety preconditions;
            // `pid` is this test's unreaped child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// A pipe as two owned files: (read end, write end).
fn pipe_files() -> (File, File) {
    let mut fds = [0; 2];
    // SAFETY: `fds` is a valid array of two c_int that pipe fills on success.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    // SAFETY: both descriptors were just created and nothing else owns them.
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

// A wrapper that starts with few descriptors to spare still runs the command
// and reports its status and output. The early end at the command's exit is
// optional: its two descriptors are taken only once everything the session
// needs is open, and the session goes without it when they are not there.
// The command reports how many of descriptors 3 to 54 it inherited, which
// shows that the wrapper really started with only those above to spare.
#[test]
fn wrapped_command_runs_with_few_descriptors_to_spare() {
    use std::os::unix::fs::OpenOptionsExt;

    for terminal in [false, true] {
        let dir = tempfile::tempdir().expect("scratch directory");
        std::fs::write(dir.path().join("empty.yml"), "rules: []\n").expect("empty config");
        for subdirectory in ["home", "runtime"] {
            std::fs::create_dir(dir.path().join(subdirectory)).expect("private directory");
        }
        let mut arguments = job_command_arguments(dir.path(), COUNT_INHERITED_DESCRIPTORS);
        arguments.pop();
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .expect("openpty");
        let null = File::open("/dev/null").expect("open /dev/null");
        let (captures, child_ends, pid) = if terminal {
            let tty = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(pair.master.tty_name().expect("outer PTY tty name"))
                .expect("open outer slave tty");
            let capture = CaptureTask::from_master(&*pair.master);
            let fd = tty.as_raw_fd();
            let pid = spawn_wrapper_short_of_descriptors(
                &arguments,
                &dir.path().join("home"),
                &dir.path().join("runtime"),
                [fd, fd, fd],
                true,
            );
            (vec![capture], vec![tty], pid)
        } else {
            let (stdout_read, stdout_write) = pipe_files();
            let (stderr_read, stderr_write) = pipe_files();
            let pid = spawn_wrapper_short_of_descriptors(
                &arguments,
                &dir.path().join("home"),
                &dir.path().join("runtime"),
                [
                    null.as_raw_fd(),
                    stdout_write.as_raw_fd(),
                    stderr_write.as_raw_fd(),
                ],
                false,
            );
            let (stdout_fd, stderr_fd) = (stdout_read.as_raw_fd(), stderr_read.as_raw_fd());
            (
                vec![
                    CaptureTask::from_reader(stdout_read, stdout_fd),
                    CaptureTask::from_reader(stderr_read, stderr_fd),
                ],
                vec![stdout_write, stderr_write],
                pid,
            )
        };
        // The captures end only once no other copy of their write ends or of
        // the outer slave is left, this test's own included.
        drop(child_ends);
        drop(null);
        drop(pair.slave);
        let status = wait_for_own_child(pid, Duration::from_secs(20))
            .unwrap_or_else(|| panic!("terminal stdin {terminal}: the wrapper did not exit"));
        let output: Vec<u8> = captures.iter().flat_map(completed_output).collect();
        let output = without_csi(&output);
        assert!(
            contains_bytes(&output, b"inherited=52"),
            "terminal stdin {terminal}: the wrapper did not start with descriptors 3 to 54 \
             taken, or the command did not get them; output: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 23,
            "terminal stdin {terminal}: the command's status was lost under a tight \
             descriptor limit (wait status {status:#x}); output: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert!(
            contains_bytes(&output, b"own-word"),
            "terminal stdin {terminal}: the command's output was lost under a tight descriptor \
             limit: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
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

/// Opens the slave of the PTY that `master` belongs to as a new file
/// description with O_NONBLOCK set.
fn open_nonblocking_terminal(master: &dyn portable_pty::MasterPty) -> File {
    use std::os::unix::fs::OpenOptionsExt;

    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(master.tty_name().expect("outer PTY tty name"))
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
    tty
}

/// Runs `ptty sh -c <script>` as the session leader of the PTY that `master`
/// belongs to, on a terminal file description with O_NONBLOCK set. The
/// wrapper's stdin, stdout and stderr all share that description, as they do
/// when it is started from a shell.
fn spawn_ptty_on_nonblocking_terminal(
    master: &dyn portable_pty::MasterPty,
    script: &str,
) -> ProcessGuard {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let tty = open_nonblocking_terminal(master);
    let mut command = Command::new(env!("CARGO_BIN_EXE_ptty"));
    command
        .args(["sh", "-c", script])
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
    ProcessGuard::new(command.spawn().expect("spawn ptty"))
}

/// The terminal's open file description is shared by every process on it, so
/// another program (a libuv tool, a crashed TUI) can leave O_NONBLOCK set on
/// it. The stdin forwarder must wait for input instead of treating EAGAIN as
/// an input failure that kills the wrapped session.
#[test]
fn nonblocking_terminal_stdin_does_not_kill_the_session() {
    use std::os::unix::process::ExitStatusExt;

    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let capture = CaptureTask::from_master(&*pair.master);
    let mut ptty =
        spawn_ptty_on_nonblocking_terminal(&*pair.master, "echo READY; read x; echo got=$x");

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

/// The same description is the wrapper's stdout, so a burst of output that
/// the terminal does not drain at once fills its queue and a write reports
/// EAGAIN. The wrapper must wait for room instead of ending the session.
#[test]
fn nonblocking_terminal_output_does_not_kill_the_session() {
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut ptty = spawn_ptty_on_nonblocking_terminal(
        &*pair.master,
        "i=0; while [ $i -lt 2000 ]; do echo \"burst line $i\"; i=$((i+1)); done; echo BURSTEND",
    );
    // Read nothing yet, so the burst fills the terminal's output queue.
    thread::sleep(Duration::from_millis(500));
    let capture = CaptureTask::from_master(&*pair.master);

    let status = ptty
        .wait_for_exit(Duration::from_secs(20))
        .expect("wrapper exits after the burst");
    let output = capture.wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"BURSTEND")
    });
    let tail = String::from_utf8_lossy(&output[output.len().saturating_sub(300)..]);
    assert_eq!(status.code(), Some(0), "output ends with: {tail:?}");
    assert!(
        contains_bytes(&output, b"BURSTEND"),
        "the burst did not reach the terminal; output ends with: {tail:?}"
    );
}

/// Stdin mode (`cmd | prismtty`) writes to the terminal through such a
/// description as well. A burst that the terminal does not drain at once must
/// wait for room there too instead of ending the run with an I/O error.
#[test]
fn nonblocking_terminal_output_does_not_end_stdin_mode() {
    use std::process::{Command, Stdio};

    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let tty = open_nonblocking_terminal(&*pair.master);
    let work = tempfile::tempdir().expect("work directory");
    let input = work.path().join("burst.txt");
    let mut burst: String = (0..2000).map(|i| format!("burst line {i}\n")).collect();
    burst.push_str("BURSTEND\n");
    std::fs::write(&input, burst).expect("write burst");
    let mut ptty = ProcessGuard::new(
        Command::new(env!("CARGO_BIN_EXE_ptty"))
            .stdin(File::open(&input).expect("open burst"))
            .stdout(Stdio::from(tty.try_clone().expect("stdout tty")))
            .stderr(Stdio::from(tty))
            .spawn()
            .expect("spawn ptty"),
    );
    // Read nothing yet, so the burst fills the terminal's output queue.
    thread::sleep(Duration::from_millis(500));
    let capture = CaptureTask::from_master(&*pair.master);

    let status = ptty
        .wait_for_exit(Duration::from_secs(20))
        .expect("prismtty exits after the burst");
    let output = capture.wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"BURSTEND")
    });
    let tail = String::from_utf8_lossy(&output[output.len().saturating_sub(300)..]);
    assert_eq!(status.code(), Some(0), "output ends with: {tail:?}");
    assert!(
        contains_bytes(&output, b"BURSTEND"),
        "the burst did not reach the terminal; output ends with: {tail:?}"
    );
}

/// A wrapped session puts the terminal in raw mode, where LF no longer returns
/// the cursor to the left edge. A diagnostic that ended in a bare LF left the
/// lines after it starting mid-screen.
#[test]
fn diagnostics_in_a_raw_terminal_end_in_cr_lf() {
    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("empty.yml");
    std::fs::write(&config, "rules: []\n").expect("config writes");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env_clear();
    builder.env("HOME", home.path());
    builder.env("PRISMTTY_RUNTIME_DIR", home.path().join("runtime"));
    builder.env("PATH", "/usr/bin:/bin");
    builder.args(["--show-profile", "--benchmark", "--profile", "generic"]);
    builder.arg("--config");
    builder.arg(&config);
    builder.args(["--", "/bin/sh", "-c", "printf 'started\\n'"]);
    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn interactive ptty");
    drop(pair.slave);
    let mut ptty = PtySession::new(child, &*pair.master);

    let status = ptty.wait_for_exit(Duration::from_secs(10));
    let output = ptty.capture().wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"Processed ")
    });

    assert_eq!(status, Some(0), "{:?}", String::from_utf8_lossy(&output));
    for line in [
        &b"prismtty: profiles selected: generic\r\n"[..],
        b"Benchmark results (time spent, match count):\r\n",
    ] {
        assert!(
            contains_bytes(&output, line),
            "{:?}",
            String::from_utf8_lossy(&output)
        );
    }
    let bare_lf = output
        .iter()
        .enumerate()
        .any(|(idx, byte)| *byte == b'\n' && (idx == 0 || output[idx - 1] != b'\r'));
    assert!(!bare_lf, "{:?}", String::from_utf8_lossy(&output));
}

/// Runs `ptty` on a fresh terminal whose slave has `outer_oflag` added to its
/// output flags, with the wrapped `script` and an optional shell redirection
/// for ptty's stdout. With `drawn_before_enter`, the script is expected to
/// wait for a line of input: the terminal must have received those bytes,
/// with the keyboard still read raw, before Enter is typed. Returns what the
/// terminal received and what the redirection saved in `$SAVED`.
fn run_on_terminal(
    outer_oflag: libc::tcflag_t,
    script: &str,
    redirect: &str,
    drawn_before_enter: Option<&[u8]>,
) -> (Vec<u8>, Option<Vec<u8>>) {
    use std::os::unix::fs::OpenOptionsExt;

    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("empty.yml");
    std::fs::write(&config, "rules: []\n").expect("config writes");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(pair.master.tty_name().expect("outer PTY tty name"))
        .expect("open outer slave tty");
    // SAFETY: termios is a plain C struct that tcgetattr fills before it is read.
    let mut attrs = unsafe { std::mem::zeroed::<libc::termios>() };
    // SAFETY: `tty` is an open terminal and `attrs` a valid termios for both calls.
    unsafe {
        assert_eq!(libc::tcgetattr(tty.as_raw_fd(), &mut attrs), 0);
        attrs.c_oflag |= outer_oflag;
        assert_eq!(libc::tcsetattr(tty.as_raw_fd(), libc::TCSANOW, &attrs), 0);
    }
    let mut builder = CommandBuilder::new("/bin/sh");
    builder.env_clear();
    builder.env("HOME", home.path());
    builder.env("PRISMTTY_RUNTIME_DIR", home.path().join("runtime"));
    builder.env("PATH", "/usr/bin:/bin");
    builder.env("PTTY", env!("CARGO_BIN_EXE_ptty"));
    builder.env("CONFIG", &config);
    builder.env("SCRIPT", script);
    builder.env("SAVED", home.path().join("saved.out"));
    builder.env("STATUS", home.path().join("status"));
    builder.args([
        "-c",
        &format!(
            "{{ \"$PTTY\" --config \"$CONFIG\" -- /bin/sh -c \"$SCRIPT\"; echo $? > \"$STATUS\"; }} {redirect}; \
             echo wrapper-status=$(cat \"$STATUS\")"
        ),
    ]);
    let child = pair.slave.spawn_command(builder).expect("spawn shell");
    drop(pair.slave);
    let mut shell = PtySession::new(child, &*pair.master);

    if let Some(drawn) = drawn_before_enter {
        let seen = shell
            .capture()
            .wait_until(Duration::from_secs(10), |out| contains_bytes(out, drawn));
        // SAFETY: `tty` is an open terminal and `attrs` a valid termios.
        unsafe {
            assert_eq!(libc::tcgetattr(tty.as_raw_fd(), &mut attrs), 0);
        }
        assert!(
            contains_bytes(&seen, drawn) && attrs.c_lflag & libc::ICANON == 0,
            "while the keyboard is read raw (ICANON {}): {:?}",
            attrs.c_lflag & libc::ICANON != 0,
            String::from_utf8_lossy(&seen)
        );
        // Not `take_writer`: dropping that writer types a newline and an end
        // of input of its own.
        let keyboard = pair.master.as_raw_fd().expect("outer PTY descriptor");
        // SAFETY: `keyboard` is the open PTY master and the buffer is one byte.
        let typed = unsafe { libc::write(keyboard, b"\n".as_ptr().cast(), 1) };
        assert_eq!(typed, 1, "type Enter");
    }
    let status = shell.wait_for_exit(Duration::from_secs(10));
    let output = shell.capture().wait_until(Duration::from_secs(2), |out| {
        contains_bytes(out, b"wrapper-status=0")
    });
    drop(tty);
    assert_eq!(status, Some(0), "{:?}", String::from_utf8_lossy(&output));
    assert!(
        contains_bytes(&output, b"wrapper-status=0"),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
    (output, std::fs::read(home.path().join("saved.out")).ok())
}

fn has_bare_lf(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .any(|(idx, byte)| *byte == b'\n' && (idx == 0 || bytes[idx - 1] != b'\r'))
}

/// The output flag that makes a terminal write spaces in place of a tab.
fn expand_tabs() -> libc::tcflag_t {
    use nix::sys::termios::OutputFlags;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flag = OutputFlags::TAB3;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flag = OutputFlags::OXTABS;
    flag.bits()
}

/// With the keyboard on a terminal and the output saved to a file, the wrapped
/// PTY starts from the terminal's settings. None of the terminal's output
/// conversions belong in the file: no CR in front of a LF, and no spaces in
/// place of a tab when the terminal expands tabs.
#[test]
fn output_saved_from_a_terminal_session_keeps_the_commands_bytes() {
    let (_, saved) = run_on_terminal(
        expand_tabs(),
        "printf 'a\\tb\\nx\\r\\ny\\rz\\n'",
        "> \"$SAVED\"",
        None,
    );
    let saved = saved.expect("saved output");

    assert_eq!(
        saved,
        b"a\tb\nx\r\ny\rz\n",
        "{:?}",
        String::from_utf8_lossy(&saved)
    );
}

/// `ptty cmd | tee log` typed at a terminal: the wrapper keeps that terminal
/// raw and the output comes back to it through `tee`, so the lines need the
/// CR the command's PTY puts in front of each LF, in the pipe too. The command
/// waits for a line of input, so the lines are drawn while the wrapper holds
/// the terminal, not after it gave it back.
#[test]
fn output_piped_back_to_the_terminal_does_not_stair_step() {
    let (terminal, saved) = run_on_terminal(
        0,
        "printf 'a\\nb\\nc\\n'; read line",
        "| tee \"$SAVED\"",
        Some(b"a\r\nb\r\nc\r\n"),
    );

    // The typed Enter is echoed by the command's PTY into the pipe as well.
    assert_eq!(saved.as_deref(), Some(&b"a\r\nb\r\nc\r\n\r\n"[..]));
    assert!(
        !has_bare_lf(&terminal),
        "{:?}",
        String::from_utf8_lossy(&terminal)
    );
}

/// A full-screen program turns its PTY's LF conversion off and writes a bare
/// LF to go one row down in the same column. Through `tee` that LF has to
/// reach the terminal bare; a CR in front of it would send the next
/// characters to the left edge.
#[test]
fn a_bare_line_feed_piped_back_to_the_terminal_stays_bare() {
    let (terminal, _) = run_on_terminal(
        0,
        "stty -opost; printf 'ab\\ncd\\n'; read line",
        "| tee \"$SAVED\"",
        Some(b"ab\ncd\n"),
    );

    assert!(
        !contains_bytes(&terminal, b"ab\r\ncd"),
        "{:?}",
        String::from_utf8_lossy(&terminal)
    );
}

/// On a terminal the wrapper keeps the outer terminal raw, so the wrapped PTY
/// still has to turn each LF into CR LF, as before.
#[test]
fn output_on_a_terminal_still_ends_lines_with_cr_lf() {
    let (output, _) = run_on_terminal(0, "printf 'a\\nb\\n'", "", None);

    assert!(
        contains_bytes(&output, b"a\r\nb\r\n"),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
}

// A shell decides whether one Ctrl-C ends a loop or a script from how the
// command it waited for ended. An interactive shell stops when the job was
// killed by SIGINT; a script stops when the shell itself received SIGINT. The
// wrapper therefore ends by SIGINT when the command it wrapped was killed by
// SIGINT, so a loop typed at a prompt around `ptty cmd` stops as it does
// around `cmd`. Every wrapper below runs with its own HOME, configuration
// directory, runtime directory and an explicit empty config, and starts with
// SIGINT set the way the test says, so a disposition or mask the test runner
// inherited cannot decide the result.

/// How a test starts a process with respect to SIGINT.
#[derive(Clone, Copy)]
enum EntrySigint {
    /// Default action and unblocked, as a shell starts a foreground command.
    Default,
    /// Ignored, as a non-interactive shell starts `cmd &`.
    Ignored,
    /// Blocked in the signal mask.
    Blocked,
}

/// Which session a process a test starts belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leader {
    /// The test's own, as a command run from a shell script is.
    Inherited,
    /// A new session of its own, without a terminal.
    Session,
    /// A new session whose controlling terminal is the one on its stdin.
    SessionOnTerminal,
}

/// Sets SIGINT for the process `command` starts, and the session it runs in.
fn start_with_sigint(command: &mut std::process::Command, entry: EntrySigint, leader: Leader) {
    use std::os::unix::process::CommandExt;

    // SAFETY: the closure runs in the forked child before exec and only calls
    // async-signal-safe functions (setsid, ioctl, signal, sigemptyset,
    // sigaddset, sigprocmask) on a stack-local signal set.
    unsafe {
        command.pre_exec(move || {
            if leader != Leader::Inherited && libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if leader == Leader::SessionOnTerminal && libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            let mut sigint = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut sigint);
            libc::sigaddset(&mut sigint, libc::SIGINT);
            let (action, how) = match entry {
                EntrySigint::Default => (libc::SIG_DFL, libc::SIG_UNBLOCK),
                EntrySigint::Ignored => (libc::SIG_IGN, libc::SIG_UNBLOCK),
                EntrySigint::Blocked => (libc::SIG_DFL, libc::SIG_BLOCK),
            };
            libc::signal(libc::SIGINT, action);
            libc::sigprocmask(how, &sigint, std::ptr::null_mut());
            Ok(())
        });
    }
}

/// A scratch HOME, configuration directory, runtime directory and config
/// for one test.
struct Scratch {
    dir: tempfile::TempDir,
    config: &'static str,
}

/// The word the cleanup tests leave as the command's last, unterminated
/// output, and the one rule of their config, which colours it.
const HELD_TAIL: &str = "heldtail";

impl Scratch {
    /// With an empty config.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("scratch directory");
        std::fs::write(dir.path().join("empty.yml"), "rules: []\n").expect("empty config");
        Self {
            dir,
            config: "empty.yml",
        }
    }

    /// With a config whose one rule colours `HELD_TAIL`, so a session that
    /// ends on it has a colour open to close.
    fn colouring_the_held_tail() -> Self {
        let mut scratch = Self::new();
        std::fs::write(
            scratch.path("tail.yml"),
            format!(
                "rules:\n  - description: held tail\n    regex: {HELD_TAIL}\n    color: f#ff0000\n"
            ),
        )
        .expect("config with one rule");
        scratch.config = "tail.yml";
        scratch
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }

    fn config(&self) -> std::path::PathBuf {
        self.path(self.config)
    }

    /// The pid file a wrapper with process id `pid` registers while it runs.
    fn pid_file(&self, pid: libc::pid_t) -> std::path::PathBuf {
        self.path("runtime").join("pids.d").join(pid.to_string())
    }

    /// Replaces the environment of `command` with the scratch one. The
    /// wrapper path and its config are in `PTTY` and `CONFIG` for shells.
    fn environment(&self, command: &mut std::process::Command) {
        command
            .env_clear()
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("PRISMTTY_RUNTIME_DIR", self.path("runtime"))
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("PTTY", env!("CARGO_BIN_EXE_ptty"))
            .env("CONFIG", self.config())
            .env("SCRATCH", self.dir.path())
            .env(TEST_TAG, test_tag());
    }

    /// The file in which the sessions a test's processes lead record their
    /// pid, for `SessionSweep`.
    fn sessions(&self) -> std::path::PathBuf {
        self.path("sessions")
    }

    /// `ptty -c <config> [extra] -- sh -c <script>`. The command first records
    /// its pid: under the wrapper it leads a session of its own.
    fn wrapper(&self, extra: &[&std::ffi::OsStr], script: &str) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ptty"));
        self.environment(&mut command);
        command.arg("-c").arg(self.config());
        command.args(extra);
        command.args([
            "--",
            "/bin/sh",
            "-c",
            &format!("echo $$ >> \"$SCRATCH/sessions\"; {script}"),
        ]);
        command
    }
}

/// Every process these tests start carries this variable, so a run can be
/// checked from outside for anything it left behind. Its value is the test
/// process's own value of it, or `pty2`.
const TEST_TAG: &str = "PTY2_TEST_TAG";

fn test_tag() -> String {
    std::env::var(TEST_TAG).unwrap_or_else(|_| "pty2".to_owned())
}

/// Kills, when it is dropped, every process left in the sessions a test
/// created, on return and on panic alike. Those are the sessions of the
/// processes in the tree under `root`, a process the test spawned, and the
/// sessions whose leaders wrote their pid to `marker`, one per line: a
/// command under the wrapper leads a session of its own, and is no longer in
/// the tree once the wrapper is gone. Processes are found by session id,
/// never by name, and the test's own session is never touched. Declare it
/// after the guard of `root`, so it runs first, while the tree is whole.
struct SessionSweep {
    root: libc::pid_t,
    marker: std::path::PathBuf,
}

impl SessionSweep {
    fn new(root: libc::pid_t, marker: std::path::PathBuf) -> Self {
        Self { root, marker }
    }
}

impl Drop for SessionSweep {
    fn drop(&mut self) {
        // SAFETY: getsid and getpid have no memory-safety preconditions.
        let (own_session, own_pid) = unsafe { (libc::getsid(0), libc::getpid()) };
        let mut swept = Vec::new();
        let mut killed = Vec::new();
        // A process that forked while it was being killed is caught by the
        // next pass.
        for _ in 0..3 {
            let processes = all_processes();
            let mut sessions: Vec<libc::pid_t> = process_tree(&processes, self.root)
                .into_iter()
                .filter_map(session_of)
                .collect();
            sessions.extend(
                std::fs::read_to_string(&self.marker)
                    .unwrap_or_default()
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<libc::pid_t>().ok()),
            );
            sessions.retain(|session| *session > 1 && *session != own_session);
            sessions.sort_unstable();
            sessions.dedup();
            let mut found = false;
            for (pid, _) in processes {
                if pid != own_pid
                    && session_of(pid).is_some_and(|session| sessions.contains(&session))
                {
                    // SAFETY: kill has no memory-safety preconditions; `pid` is in a session this
                    // test created.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    killed.push(pid);
                    found = true;
                }
            }
            swept.extend(sessions);
            if !found {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        swept.sort_unstable();
        swept.dedup();
        eprintln!("swept sessions {swept:?}, killed {killed:?}");
    }
}

/// Every process as (pid, parent pid).
fn all_processes() -> Vec<(libc::pid_t, libc::pid_t)> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect()
}

/// `root` and every process below it.
fn process_tree(processes: &[(libc::pid_t, libc::pid_t)], root: libc::pid_t) -> Vec<libc::pid_t> {
    let mut tree = vec![root];
    let mut next = 0;
    while next < tree.len() {
        let parent = tree[next];
        tree.extend(
            processes
                .iter()
                .filter(|(_, ppid)| *ppid == parent)
                .map(|(pid, _)| *pid),
        );
        next += 1;
    }
    tree
}

fn session_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    // SAFETY: getsid has no memory-safety preconditions.
    let session = unsafe { libc::getsid(pid) };
    (session > 0).then_some(session)
}

/// The script the tests use to kill a command with SIGINT dies from it
/// without the wrapper too, so the wrapped result is compared with the
/// command's own.
fn assert_sh_dies_from_its_own_sigint() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    let mut bare = Command::new("/bin/sh");
    bare.args(["-c", "kill -INT $$; sleep 5"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    start_with_sigint(&mut bare, EntrySigint::Default, Leader::Inherited);
    let status = bare.status().expect("run sh without the wrapper");
    assert_eq!(
        status.signal(),
        Some(libc::SIGINT),
        "control: sh did not die from its own SIGINT ({status:?})"
    );
}

fn wait_for_file_containing(path: &std::path::Path, needle: &[u8], timeout: Duration) -> bool {
    wait_for_condition(timeout, || {
        std::fs::read(path).is_ok_and(|contents| contains_bytes(&contents, needle))
    })
}

/// The outer terminal's settings, read where they can still be read after
/// the wrapper, its session leader, has exited. macOS then revokes the slave
/// from every descriptor, but both ends share one set of settings there, so
/// they are read through the master. Linux keeps the slave usable and gives
/// the master settings of its own.
fn outer_terminal_settings(
    tty: &File,
    master: &dyn portable_pty::MasterPty,
) -> nix::sys::termios::Termios {
    #[cfg(target_os = "macos")]
    {
        let _ = tty;
        let fd = master.as_raw_fd().expect("outer PTY descriptor");
        // SAFETY: `fd` is the PTY master that `master` keeps open for this call.
        let master = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        nix::sys::termios::tcgetattr(master).expect("terminal settings")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = master;
        nix::sys::termios::tcgetattr(tty).expect("terminal settings")
    }
}

/// Whether `output` shows `HELD_TAIL` in a foreground colour that is closed
/// right after it, by a full reset or a foreground reset.
fn held_tail_coloured_and_closed(output: &[u8]) -> bool {
    let tail = HELD_TAIL.as_bytes();
    let Some(at) = output.windows(tail.len()).position(|window| window == tail) else {
        return false;
    };
    let before = &output[..at];
    let after = &output[at + tail.len()..];
    contains_bytes(&before[before.len().saturating_sub(24)..], b"\x1b[38;")
        && (after.starts_with(b"\x1b[0m") || after.starts_with(b"\x1b[39m"))
}

/// Whether the trace has drawn `HELD_TAIL` followed by its colour reset.
fn trace_has_closed_held_tail(trace: &str) -> bool {
    [b"\x1b[0m".as_slice(), b"\x1b[39m"]
        .iter()
        .any(|reset| trace_has(trace, "RENDER", &[HELD_TAIL.as_bytes(), reset].concat()))
}

/// Hex of `bytes` as `--trace-io` writes it.
fn trace_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the trace has a `direction` line whose bytes contain `bytes`.
fn trace_has(trace: &str, direction: &str, bytes: &[u8]) -> bool {
    let hex = trace_hex(bytes);
    trace
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(direction) && line.contains(&hex))
}

/// A wrapper run with stdin, stdout and stderr redirected: its real wait
/// status and what it wrote.
struct RedirectedRun {
    status: std::process::ExitStatus,
    output: String,
    pid: libc::pid_t,
}

/// Runs the wrapper on `script` with stdin from /dev/null and output to a
/// file. With `signal`, sends it to the wrapper once the script printed READY.
fn run_redirected(
    scratch: &Scratch,
    extra: &[&std::ffi::OsStr],
    script: &str,
    entry: EntrySigint,
    signal: Option<libc::c_int>,
) -> RedirectedRun {
    use std::process::Stdio;

    let output_path = scratch.path("output");
    let mut command = scratch.wrapper(extra, script);
    command
        .stdin(Stdio::null())
        .stdout(File::create(&output_path).expect("output file"))
        .stderr(File::create(scratch.path("stderr")).expect("stderr file"));
    start_with_sigint(&mut command, entry, Leader::Inherited);
    let mut wrapper = ProcessGuard::new(command.spawn().expect("spawn ptty"));
    let pid = wrapper.pid();
    let _sweep = SessionSweep::new(pid, scratch.sessions());
    if let Some(signal) = signal {
        assert!(
            wait_for_file_containing(&output_path, b"READY", Duration::from_secs(10)),
            "the command never printed READY: {:?}",
            std::fs::read_to_string(&output_path)
        );
        // SAFETY: kill has no memory-safety preconditions; the target is the wrapper this test
        // spawned and has not reaped.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    }
    let status = wrapper
        .wait_for_exit(Duration::from_secs(10))
        .expect("wrapper exits");
    let mut output = std::fs::read_to_string(&output_path).unwrap_or_default();
    output.push_str(&std::fs::read_to_string(scratch.path("stderr")).unwrap_or_default());
    RedirectedRun {
        status,
        output,
        pid,
    }
}

#[test]
fn wrapper_dies_from_sigint_after_cleanup_when_its_command_did() {
    use std::os::unix::process::ExitStatusExt;

    assert_sh_dies_from_its_own_sigint();
    let scratch = Scratch::colouring_the_held_tail();
    let trace = scratch.path("trace.log");
    let run = run_redirected(
        &scratch,
        &[std::ffi::OsStr::new("--trace-io"), trace.as_os_str()],
        "printf 'first line\\nheld''tail'; kill -INT $$; sleep 5",
        EntrySigint::Default,
        None,
    );
    let trace = std::fs::read_to_string(&trace).expect("trace file");

    assert!(
        run.output.contains("first line\n") && held_tail_coloured_and_closed(run.output.as_bytes()),
        "the last, unterminated output was not written first with its colour closed: {:?}",
        run.output
    );
    assert!(
        trace_has(&trace, "OUT", HELD_TAIL.as_bytes()) && trace_has_closed_held_tail(&trace),
        "the trace does not end with the last output and its colour reset: {trace}"
    );
    assert!(
        scratch.path("runtime").join("pids.d").is_dir() && !scratch.pid_file(run.pid).exists(),
        "the session's pid file was not removed"
    );
    assert_eq!(
        (run.status.signal(), run.status.code()),
        (Some(libc::SIGINT), None),
        "wrapper of a command killed by SIGINT: {:?}",
        run.output
    );
}

#[test]
fn wrapper_on_a_terminal_dies_from_sigint_after_restoring_it_when_its_command_did() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::ExitStatusExt;

    assert_sh_dies_from_its_own_sigint();
    let scratch = Scratch::colouring_the_held_tail();
    let trace = scratch.path("trace.log");
    let pair = native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty");
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(pair.master.tty_name().expect("outer PTY tty name"))
        .expect("open outer slave tty");
    drop(pair.slave);
    let cooked = outer_terminal_settings(&tty, &*pair.master);
    let capture = CaptureTask::from_master(&*pair.master);
    let mut command = scratch.wrapper(
        &[std::ffi::OsStr::new("--trace-io"), trace.as_os_str()],
        "printf 'first line\\nheld''tail'; kill -INT $$; sleep 5",
    );
    command
        .stdin(tty.try_clone().expect("stdin tty"))
        .stdout(tty.try_clone().expect("stdout tty"))
        .stderr(tty.try_clone().expect("stderr tty"));
    start_with_sigint(
        &mut command,
        EntrySigint::Default,
        Leader::SessionOnTerminal,
    );
    let mut wrapper = ProcessGuard::new(command.spawn().expect("spawn ptty"));
    let pid = wrapper.pid();
    let _sweep = SessionSweep::new(pid, scratch.sessions());
    let status = wrapper
        .wait_for_exit(Duration::from_secs(10))
        .expect("wrapper exits");
    let output = capture.wait_until(Duration::from_secs(2), held_tail_coloured_and_closed);
    let trace = std::fs::read_to_string(&trace).expect("trace file");

    assert!(
        contains_bytes(&output, b"first line\r\n") && held_tail_coloured_and_closed(&output),
        "the last, unterminated output was not drawn first with its colour closed: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        trace_has_closed_held_tail(&trace),
        "the trace does not end with the last output and its colour reset: {trace}"
    );
    assert!(
        scratch.path("runtime").join("pids.d").is_dir() && !scratch.pid_file(pid).exists(),
        "the session's pid file was not removed"
    );
    assert!(
        outer_terminal_settings(&tty, &*pair.master) == cooked,
        "the terminal was not given back in its own settings"
    );
    assert_eq!(
        (status.signal(), status.code()),
        (Some(libc::SIGINT), None),
        "wrapper on a terminal of a command killed by SIGINT: {:?}",
        String::from_utf8_lossy(&output)
    );
}

/// What a shell prints for the loop typed at its prompt, from the typed line
/// until one Ctrl-C has either brought the prompt back or let the next round
/// start. `argv` starts the shell (or a wrapper around it) as the session
/// leader of a fresh terminal with SIGINT at its default action; with
/// `wrapped`, the loop runs its command under the wrapper. Each round's
/// command prints READY and then waits until it is killed, so Ctrl-C is typed
/// only once the first round's command runs, and no round can end on its own
/// however slow the machine is.
fn loop_after_one_ctrl_c(scratch: &Scratch, argv: &[&str], wrapped: bool) -> Vec<u8> {
    use std::os::unix::fs::OpenOptionsExt;

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 250,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(pair.master.tty_name().expect("outer PTY tty name"))
        .expect("open outer slave tty");
    drop(pair.slave);
    let capture = CaptureTask::from_master(&*pair.master);
    let mut command = std::process::Command::new(argv[0]);
    command.args(&argv[1..]);
    scratch.environment(&mut command);
    command
        .stdin(tty.try_clone().expect("stdin tty"))
        .stdout(tty.try_clone().expect("stdout tty"))
        .stderr(tty.try_clone().expect("stderr tty"));
    start_with_sigint(
        &mut command,
        EntrySigint::Default,
        Leader::SessionOnTerminal,
    );
    let mut shell = ProcessGuard::new(command.spawn().expect("spawn interactive shell"));
    let _sweep = SessionSweep::new(shell.pid(), scratch.sessions());
    let keyboard = pair.master.as_raw_fd().expect("outer PTY descriptor");
    let type_text = |text: &[u8]| {
        // SAFETY: `keyboard` is the open PTY master and the buffer is valid for its length.
        let typed = unsafe { libc::write(keyboard, text.as_ptr().cast(), text.len()) };
        assert_eq!(typed, text.len() as isize, "type into the terminal");
    };
    let len_now = || capture.wait_until(Duration::ZERO, |_| true).len();
    let seen_after = |start: usize, needle: &[u8], timeout: Duration| {
        let output = capture.wait_until(timeout, |out| {
            out.len() > start && contains_bytes(&out[start..], needle)
        });
        output
            .get(start..)
            .is_some_and(|tail| contains_bytes(tail, needle))
    };
    let shown = || capture.wait_until(Duration::ZERO, |_| true);

    assert!(
        wait_for_condition(Duration::from_secs(10), || len_now() > 0),
        "the shell printed nothing"
    );
    // The shell records its session, for the sweep. The prompt it prints
    // differs from the typed text.
    type_text(b"echo $$ >> \"$SCRATCH/sessions\"; PS1='PRO''MPT> '\r");
    assert!(
        seen_after(0, b"PROMPT> ", Duration::from_secs(10)),
        "the prompt never appeared"
    );
    let prefix = if wrapped {
        "\"$PTTY\" -c \"$CONFIG\" -- "
    } else {
        ""
    };
    let start = len_now();
    type_text(
        format!(
            "for i in 1 2 3; do {prefix}sh -c 'echo $$ >> \"$SCRATCH/sessions\"; echo RE\"\"ADY; exec sleep 60'; echo IT\"\"ER $i; done; echo LOOP\"\"DONE\r"
        )
        .as_bytes(),
    );
    assert!(
        seen_after(start, b"READY", Duration::from_secs(10)),
        "the loop's first command never started"
    );
    let interrupted = len_now();
    type_text(b"\x03");
    // Whichever comes first: the prompt (the loop stopped) or the next
    // round's READY (it went on).
    let settled = wait_for_condition(Duration::from_secs(20), || {
        let output = shown();
        output
            .get(interrupted..)
            .is_some_and(|tail| contains_bytes(tail, b"PROMPT> ") || contains_bytes(tail, b"READY"))
    });
    let output = shown()[start..].to_vec();
    assert!(
        settled,
        "after Ctrl-C neither the prompt came back nor the next round started within 20 s: {:?}",
        String::from_utf8_lossy(&output)
    );
    if count_of(&output, b"READY") == 1 {
        type_text(b"exit\r");
        let _ = shell.wait_for_exit(Duration::from_secs(5));
    }
    output
}

fn count_of(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn assert_loop_stopped(output: &[u8], what: &str) {
    assert!(
        count_of(output, b"READY") == 1
            && contains_bytes(output, b"PROMPT> ")
            && !contains_bytes(output, b"ITER 2")
            && !contains_bytes(output, b"LOOPDONE"),
        "{what}: the loop went on after one Ctrl-C: {:?}",
        String::from_utf8_lossy(output)
    );
}

/// The first of `paths` that exists.
fn installed(paths: &[&'static str]) -> Option<&'static str> {
    paths
        .iter()
        .copied()
        .find(|path| std::path::Path::new(path).exists())
}

#[test]
fn one_ctrl_c_stops_a_loop_typed_at_an_interactive_zsh_prompt() {
    let Some(zsh) = installed(&["/bin/zsh", "/usr/bin/zsh"]) else {
        eprintln!("skipped: zsh is not installed");
        return;
    };
    let scratch = Scratch::new();
    let shell = [zsh, "-f", "-i"];

    assert_loop_stopped(
        &loop_after_one_ctrl_c(&scratch, &shell, false),
        "control without the wrapper",
    );
    assert_loop_stopped(
        &loop_after_one_ctrl_c(&scratch, &shell, true),
        "wrapped command",
    );
}

#[test]
fn one_ctrl_c_stops_a_loop_typed_at_an_interactive_bash_prompt() {
    let Some(bash) = installed(&["/bin/bash", "/usr/bin/bash"]) else {
        eprintln!("skipped: bash is not installed");
        return;
    };
    let scratch = Scratch::new();
    let shell = [bash, "--norc", "--noprofile", "-i"];

    assert_loop_stopped(
        &loop_after_one_ctrl_c(&scratch, &shell, false),
        "control without the wrapper",
    );
    assert_loop_stopped(
        &loop_after_one_ctrl_c(&scratch, &shell, true),
        "wrapped command",
    );
}

/// The wrapper around an interactive shell, as it is used all day, with a
/// second wrapper running the loop's command inside it.
#[test]
fn one_ctrl_c_stops_a_loop_typed_in_a_wrapped_interactive_shell() {
    let shell = match installed(&["/bin/zsh", "/usr/bin/zsh"]) {
        Some(zsh) => vec![zsh, "-f", "-i"],
        None => vec![
            installed(&["/bin/bash", "/usr/bin/bash"]).expect("bash is installed"),
            "--norc",
            "--noprofile",
            "-i",
        ],
    };
    let scratch = Scratch::new();
    let config = scratch.config();
    let mut argv = vec![
        env!("CARGO_BIN_EXE_ptty"),
        "-c",
        config.to_str().expect("UTF-8 scratch path"),
        "--",
    ];
    argv.extend(shell);

    assert_loop_stopped(
        &loop_after_one_ctrl_c(&scratch, &argv, false),
        "control with an unwrapped command",
    );
    assert_loop_stopped(
        &loop_after_one_ctrl_c(&scratch, &argv, true),
        "wrapped command",
    );
}

/// SIGINT sent to the wrapper is forwarded; when it kills the command, the
/// wrapper ends by it too.
#[test]
fn sigint_sent_to_the_wrapper_ends_it_by_sigint_when_it_kills_the_command() {
    use std::os::unix::process::ExitStatusExt;

    let scratch = Scratch::new();
    let run = run_redirected(
        &scratch,
        &[],
        "echo READY; exec sleep 5",
        EntrySigint::Default,
        Some(libc::SIGINT),
    );

    assert!(
        scratch.path("runtime").join("pids.d").is_dir() && !scratch.pid_file(run.pid).exists(),
        "the session's pid file was not removed"
    );
    assert_eq!(
        (run.status.signal(), run.status.code()),
        (Some(libc::SIGINT), None),
        "{:?}",
        run.output
    );
}

// The guards below hold before and after the change.

/// A command that handles SIGINT and exits normally keeps its exit code, both
/// when it raised the signal itself and when the wrapper forwarded it.
#[test]
fn a_command_that_handles_sigint_keeps_its_exit_code() {
    use std::os::unix::process::ExitStatusExt;

    let scratch = Scratch::new();
    let own = run_redirected(
        &scratch,
        &[],
        "trap 'exit 7' INT; kill -INT $$; sleep 5",
        EntrySigint::Default,
        None,
    );
    assert_eq!(
        (own.status.signal(), own.status.code()),
        (None, Some(7)),
        "{:?}",
        own.output
    );

    let forwarded = run_redirected(
        &scratch,
        &[],
        "trap 'exit 0' INT; echo READY; while :; do sleep 0.1; done",
        EntrySigint::Default,
        Some(libc::SIGINT),
    );
    assert_eq!(
        (forwarded.status.signal(), forwarded.status.code()),
        (None, Some(128 + libc::SIGINT)),
        "{:?}",
        forwarded.output
    );
}

/// Only SIGINT is passed on as a signal: a command killed by SIGTERM or
/// SIGQUIT is still reported as 128 plus the signal.
#[test]
fn a_command_killed_by_another_signal_still_reports_128_plus_the_signal() {
    use std::os::unix::process::ExitStatusExt;

    let scratch = Scratch::new();
    for (signal, name) in [(libc::SIGTERM, "TERM"), (libc::SIGQUIT, "QUIT")] {
        let run = run_redirected(
            &scratch,
            &[],
            &format!("kill -{name} $$; sleep 5"),
            EntrySigint::Default,
            None,
        );
        assert_eq!(
            (run.status.signal(), run.status.code()),
            (None, Some(128 + signal)),
            "SIG{name}: {:?}",
            run.output
        );
    }
}

/// SIGTERM sent to the wrapper ends it with 143 even when the command is then
/// killed by SIGINT. The test sends SIGTERM once the command's READY has come
/// through the wrapper, which by then has registered the command and so
/// forwards the SIGTERM to it. The command answers that SIGTERM by recording
/// it and sending SIGINT to itself. Its two markers show that the forwarded
/// SIGTERM arrived and that the command did not run on past its own SIGINT;
/// with the control, which shows this shell dies from its own SIGINT, that is
/// the command dying from SIGINT while the wrapper waits for it. The
/// command's wait status itself only the wrapper can read.
#[test]
fn sigterm_to_the_wrapper_still_reports_143_when_the_command_then_dies_from_sigint() {
    use std::os::unix::process::ExitStatusExt;

    assert_sh_dies_from_its_own_sigint();
    let scratch = Scratch::new();
    let run = run_redirected(
        &scratch,
        &[],
        "trap 'echo seen > \"$SCRATCH/term-forwarded\"; kill -INT $$; echo on > \"$SCRATCH/ran-on\"' TERM; \
         echo READY; while :; do sleep 1; done",
        EntrySigint::Default,
        Some(libc::SIGTERM),
    );

    assert!(
        scratch.path("term-forwarded").exists(),
        "the command never received the SIGTERM the wrapper forwards: {:?}",
        run.output
    );
    assert!(
        !scratch.path("ran-on").exists(),
        "the command ran on past its own SIGINT: {:?}",
        run.output
    );
    assert_eq!(
        (run.status.signal(), run.status.code()),
        (None, Some(128 + libc::SIGTERM)),
        "{:?}",
        run.output
    );
}

/// A caller that ignored or blocked SIGINT chose that the wrapper must not die
/// from it: the wrapper reports a command killed by SIGINT as 130, as before,
/// and leaves the caller's setting alone.
#[test]
fn sigint_ignored_or_blocked_by_the_caller_still_reports_130() {
    use std::os::unix::process::ExitStatusExt;

    let scratch = Scratch::new();
    for (entry, name) in [
        (EntrySigint::Ignored, "ignored"),
        (EntrySigint::Blocked, "blocked"),
    ] {
        let run = run_redirected(&scratch, &[], "kill -INT $$; sleep 5", entry, None);
        assert_eq!(
            (run.status.signal(), run.status.code()),
            (None, Some(128 + libc::SIGINT)),
            "SIGINT {name} at entry: {:?}",
            run.output
        );
    }
}

/// A script starts `cmd &` with SIGINT ignored, so a wrapper started that way
/// keeps 130 when its command is killed by SIGINT. The script's `$?` reads 130
/// both for an exit with 130 and for a death by SIGINT, so the real background
/// job pins only what the script sees. The wait status itself is read from a
/// wrapper that the shell started with SIGINT ignored by the shell itself
/// (`trap '' INT` before `exec`), the setting a background job inherits.
#[test]
fn a_wrapper_started_by_a_script_with_sigint_ignored_still_reports_130() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    let scratch = Scratch::new();
    let shell = |script: &str| {
        let output_path = scratch.path("job-output");
        let mut command = Command::new("/bin/sh");
        scratch.environment(&mut command);
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(File::create(&output_path).expect("output file"))
            .stderr(Stdio::null());
        start_with_sigint(&mut command, EntrySigint::Default, Leader::Session);
        let mut shell = ProcessGuard::new(command.spawn().expect("spawn sh"));
        let _sweep = SessionSweep::new(shell.pid(), scratch.sessions());
        let status = shell
            .wait_for_exit(Duration::from_secs(10))
            .expect("sh exits");
        (
            status,
            std::fs::read_to_string(&output_path).unwrap_or_default(),
        )
    };

    // Control: the shell starts its background job with SIGINT ignored.
    let (_, bare) = shell("sh -c 'kill -INT $$; echo SURVIVED' & wait $!; echo BG:$?");
    assert!(
        bare.contains("SURVIVED") && bare.contains("BG:0"),
        "control: the background job did not ignore SIGINT: {bare:?}"
    );

    let (_, job) = shell(
        "\"$PTTY\" -c \"$CONFIG\" -- sh -c 'echo $$ >> \"$SCRATCH/sessions\"; kill -INT $$; sleep 5' & \
         wait $!; echo BG:$?",
    );
    assert!(job.contains("BG:130"), "background job: {job:?}");

    let (status, output) = shell(
        "trap '' INT; exec \"$PTTY\" -c \"$CONFIG\" -- \
         sh -c 'echo $$ >> \"$SCRATCH/sessions\"; kill -INT $$; sleep 5'",
    );
    assert_eq!(
        (status.signal(), status.code()),
        (None, Some(128 + libc::SIGINT)),
        "wrapper started with SIGINT ignored by its shell: {output:?}"
    );
}
