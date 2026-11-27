//! Integration coverage for interactive input echo (the pasted-command bug).
//!
//! prismtty buffers a trailing partial token of interactive echo so a token
//! split across reads still highlights as a unit. A pasted line echoes back in
//! a single large read, so the buffered trailing token used to stay invisible
//! until the next keystroke surfaced it (reported against `nsupdate`, whose bare
//! `> ` prompt is not recognized, so echo is not passed through). prismtty must
//! instead surface it once the child goes idle.
#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use prismtty::highlight::strip_ansi;

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

    fn terminate_and_wait(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // forkpty children are session and process-group leaders. Kill both the
        // group and direct pid so cleanup remains reliable if that assumption
        // changes in portable-pty.
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

struct CaptureTask {
    receiver: mpsc::Receiver<()>,
    output: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    completion: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CaptureTask {
    fn start(master: &dyn portable_pty::MasterPty) -> Self {
        let master_fd = master.as_raw_fd().expect("PTY master fd");
        // SAFETY: `master_fd` is the open PTY master; dup returns a fresh descriptor this test
        // owns.
        let reader_fd = unsafe { libc::dup(master_fd) };
        assert!(
            reader_fd >= 0,
            "duplicate PTY master fd: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `reader_fd` is a freshly duplicated descriptor that nothing else owns or closes.
        let mut reader = unsafe { File::from_raw_fd(reader_fd) };

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
                    // PTY masters commonly report EIO after their slave closes.
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
            // Dropping a JoinHandle detaches it. The thread owns its reader and
            // stop flag, so a pathological platform read cannot hang teardown.
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
            capture: CaptureTask::start(master),
        }
    }

    fn pid(&self) -> libc::pid_t {
        self.child.pid()
    }

    fn capture(&self) -> &CaptureTask {
        &self.capture
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Keep draining output while the child group is terminated, then stop
        // the reader. Both operations are bounded.
        self.child.terminate_and_wait();
        self.capture.stop_bounded();
    }
}

struct TypingTask {
    stop: Arc<AtomicBool>,
    completion: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TypingTask {
    fn start(mut writer: Box<dyn Write + Send>, typed: &'static [u8]) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (completion_tx, completion) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                if writer.write_all(typed).is_err() || writer.flush().is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
            let _ = completion_tx.try_send(());
        });
        Self {
            stop,
            completion,
            thread: Some(thread),
        }
    }
}

impl Drop for TypingTask {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.completion.recv_timeout(THREAD_JOIN_LIMIT).is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            self.thread.take();
        }
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
                .min(Duration::from_millis(10)),
        );
    }
}

fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: kill with signal 0 has no memory-safety preconditions and only checks for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn visible_output(output: &[u8]) -> String {
    String::from_utf8_lossy(&strip_ansi(output)).into_owned()
}

fn trace_output_bytes(path: &Path) -> Vec<u8> {
    let Ok(trace) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    trace
        .lines()
        .filter_map(|line| line.split_once(" OUT ").map(|(_, bytes)| bytes))
        .flat_map(|bytes| bytes.split_whitespace())
        .filter_map(|byte| u8::from_str_radix(byte, 16).ok())
        .collect()
}

/// The reason traced for each flush of a held-back tail, decoded from the
/// `FLUSH` lines of a `--trace-io` file.
fn trace_flush_reasons(path: &Path) -> Vec<String> {
    let Ok(trace) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    trace
        .lines()
        .filter_map(|line| line.split_once(" FLUSH ").map(|(_, bytes)| bytes))
        .map(|bytes| {
            let bytes: Vec<u8> = bytes
                .split_whitespace()
                .filter_map(|byte| u8::from_str_radix(byte, 16).ok())
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .collect()
}

/// Asserts that an echo trigger, not the program-output holdback, drew the
/// tail. The holdback also makes the tail visible, only 250 ms late, so
/// visibility alone cannot tell a broken echo trigger apart. The trace line is
/// written before the tail is drawn, so a visible tail's reason is on file.
fn assert_drawn_by_echo_trigger(path: &Path, reason: &str) {
    let reasons = trace_flush_reasons(path);
    assert!(
        reasons.iter().any(|traced| traced == reason),
        "tail was not drawn by the {reason:?} trigger; traced flushes: {reasons:?}"
    );
    assert!(
        !reasons.iter().any(|traced| traced == "holdback"),
        "echo waited out the program-output holdback; traced flushes: {reasons:?}"
    );
}

/// Makes a FIFO for a child to wait on with `read -r _ < "$gate"`. Unlike a
/// file the child polls with `sleep`, the child resumes the moment
/// [`release_fifo_gate`] writes to it, so the pause it ends is the test's own
/// reaction time, however slowly the machine starts processes.
fn make_fifo_gate(path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("gate path");
    // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
    let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(
        made,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

/// Ends one `read -r _ < "$gate"` of the child, waiting for it to open its
/// end of the FIFO if it has not yet.
fn release_fifo_gate(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(mut gate) => {
                gate.write_all(b"\n").expect("write the FIFO gate");
                return;
            }
            // ENXIO: no reader has the FIFO open yet.
            Err(error)
                if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("open the FIFO gate: {error}"),
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
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
fn typing_cleanup_is_bounded_when_a_writer_is_stuck() {
    use std::sync::Condvar;

    struct GatedWriter {
        entered: Arc<AtomicBool>,
        exited: Arc<AtomicBool>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            self.entered.store(true, Ordering::Release);
            let (lock, wake) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.exited.store(true, Ordering::Release);
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected blocked writer released",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let entered = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let task = TypingTask::start(
        Box::new(GatedWriter {
            entered: Arc::clone(&entered),
            exited: Arc::clone(&exited),
            gate: Arc::clone(&gate),
        }),
        b"x",
    );
    assert!(
        wait_for_condition(Duration::from_secs(1), || entered.load(Ordering::Acquire)),
        "typing thread did not enter injected blocking write"
    );

    let (drop_tx, drop_rx) = mpsc::sync_channel(1);
    let dropper = thread::spawn(move || {
        drop(task);
        let _ = drop_tx.send(());
    });
    let bounded = drop_rx.recv_timeout(THREAD_JOIN_LIMIT + Duration::from_millis(250));
    let (lock, wake) = &*gate;
    *lock.lock().unwrap() = true;
    wake.notify_all();
    dropper.join().expect("typing dropper joins after release");

    assert!(bounded.is_ok(), "typing cleanup blocked on a stuck writer");
    assert!(
        wait_for_condition(Duration::from_secs(1), || exited.load(Ordering::Acquire)),
        "detached typing thread did not exit after its writer was released"
    );
}

/// A pasted command (no trailing newline) must become fully visible without any
/// further input. `cat` keeps the wrapped PTY in canonical echo mode, so the
/// tty line discipline echoes the paste exactly as `nsupdate`'s prompt would.
#[test]
fn pasted_line_is_fully_visible_without_extra_input() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("echo.trace");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("--trace-io");
    builder.arg(&trace_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg("printf 'READY\\n'; exec cat");

    let child = pair.slave.spawn_command(builder).expect("spawn ptty cat");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);

    // Wait until prismtty is forwarding child output before sending the paste.
    let ready = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains("READY")
    });
    let mut visible = visible_output(&ready);
    assert!(
        visible.contains("READY"),
        "wrapped command was not ready before paste; saw: {visible:?}"
    );

    // A multi-word line whose final, delimiter-less token ("192.0.2.1") is the
    // piece prismtty buffers. No trailing newline: the child stays at the line,
    // exactly like a paste awaiting Enter.
    let paste = b"update add test.example.com 3600 A 192.0.2.1";
    writer.write_all(paste).expect("write paste");
    writer.flush().expect("flush paste");

    // Wait for the full line to surface. Crucially we send NO further bytes, so
    // the trailing token can only appear via prismtty's idle flush.
    let target = "update add test.example.com 3600 A 192.0.2.1";
    let output = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains(target)
    });
    visible = visible_output(&output);

    assert!(
        visible.contains(target),
        "pasted line never fully surfaced without extra input; saw: {visible:?}"
    );
    assert_drawn_by_echo_trigger(&trace_path, "echo-suffix");
}

fn count_subslice(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

fn contains_sgr_span(haystack: &[u8], token: &[u8]) -> bool {
    let mut rest = haystack;
    while let Some(esc_idx) = rest.iter().position(|byte| *byte == 0x1b) {
        let candidate = &rest[esc_idx..];
        let Some(m_idx) = candidate.iter().position(|byte| *byte == b'm') else {
            return false;
        };
        if candidate[m_idx + 1..].starts_with(token) {
            return true;
        }
        rest = &candidate[1..];
    }
    false
}

/// The mirror invariant: a token split across reads in pure PROGRAM output (no
/// input echo) must keep its cross-read highlighting. The idle flush surfaces
/// input echo, so it must NOT fire for buffered program-output tokens — there is
/// no pending input echo. Regression guard for the bulk-output highlighting that
/// an unconditional idle flush would break (cisco "Vlan1191" split as
/// "...Vlan11" + "91" across two reads with an inter-write gap). The gap here is
/// the test's reaction time, far inside the program-output holdback.
#[test]
fn split_program_output_token_keeps_single_highlight_span() {
    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("split.trace");
    let continue_path = trace_dir.path().join("continue");
    make_fifo_gate(&continue_path);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("--trace-io");
    builder.arg(&trace_path);
    builder.arg("-p");
    builder.arg("cisco");
    builder.env("PRISMTTY_TEST_CONTINUE", &continue_path);
    builder.arg("sh");
    builder.arg("-c");
    // The child cannot emit the suffix until the test has observed the prefix in
    // prismtty's raw I/O trace. This proves the token crossed two downstream read
    // boundaries without relying on a scheduler-dependent sleep. It waits on a
    // FIFO rather than polling a file, so it resumes as soon as the test
    // releases it and the holdback does not draw the prefix on its own.
    builder.arg(
        "printf 'show: Vlan11'; read -r _ < \"$PRISMTTY_TEST_CONTINUE\"; printf '91 New TZ GW to Internal\\n'",
    );

    let child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);

    // Pure program output: we never write to the master, so no input echo is
    // pending and, within the holdback window, the idle flush must leave the
    // buffered token alone.
    let session = PtySession::new(child, &*pair.master);
    let prefix_crossed_boundary = wait_for_condition(Duration::from_secs(5), || {
        contains_bytes(&trace_output_bytes(&trace_path), b"show: Vlan11")
    });
    assert!(
        prefix_crossed_boundary,
        "raw trace never recorded the token prefix before the continuation gate"
    );
    release_fifo_gate(&continue_path);

    let out = session.capture().wait_until(Duration::from_secs(5), |out| {
        contains_sgr_span(out, b"Vlan1191")
    });

    assert!(
        contains_sgr_span(&out, b"Vlan1191"),
        "split program-output token lost its single highlight span; saw: {:?}",
        String::from_utf8_lossy(&out)
    );
}

/// Runs an echo-off child that streams the cisco token "Vlan1191" split across
/// two writes per iteration, while a thread types `typed` bytes throughout, and
/// returns the captured output. With echo off the typed bytes never echo back,
/// so the buffered token is always program output and its span must stay intact
/// regardless of what is typed.
///
/// Each second half waits on a FIFO that the test releases as soon as prismtty
/// draws the `aaaaaaaa ` before the first half, which it does only once it has
/// read it. So every token crosses a read boundary, and its halves arrive far
/// closer together than the program-output holdback, which would draw the
/// first half on its own.
fn echo_off_split_stream_while_typing(typed: &'static [u8]) -> Vec<u8> {
    const SPLITS: usize = 12;
    let gate_dir = tempfile::tempdir().expect("gate tempdir");
    let continue_path = gate_dir.path().join("continue");
    make_fifo_gate(&continue_path);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("-p");
    builder.arg("cisco");
    builder.env("PRISMTTY_TEST_CONTINUE", &continue_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(format!(
        "stty -echo; i=0; while [ $i -lt {SPLITS} ]; do printf 'aaaaaaaa Vlan11'; \
         read -r _ < \"$PRISMTTY_TEST_CONTINUE\"; printf '91 bbbb\\n'; sleep 0.12; \
         i=$((i+1)); done; printf 'STREAM_DONE\\n'"
    ));

    let child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);

    let writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);

    // Type throughout, then stop explicitly. Linux PTYs can keep accepting
    // master writes briefly after the child exits, so do not rely on write
    // failure as the only thread-exit signal.
    let typer = TypingTask::start(writer, typed);
    for split in 1..=SPLITS {
        let drawn = session.capture().wait_until(Duration::from_secs(5), |out| {
            visible_output(out).matches("aaaaaaaa ").count() >= split
        });
        assert!(
            visible_output(&drawn).matches("aaaaaaaa ").count() >= split,
            "prismtty never drew the first half of split {split}; saw: {:?}",
            visible_output(&drawn)
        );
        release_fifo_gate(&continue_path);
    }
    let out = session.capture().wait_until(Duration::from_secs(8), |out| {
        visible_output(out).contains("STREAM_DONE")
    });
    drop(typer);
    out
}

fn assert_spans_intact(out: &[u8]) {
    let broken = count_subslice(out, b"mVlan11\x1b[39m91");
    assert_eq!(
        broken,
        0,
        "concurrent input split {broken} program-output token span(s); saw: {:?}",
        String::from_utf8_lossy(out)
    );
    assert!(
        contains_sgr_span(out, b"Vlan1191"),
        "expected at least one intact Vlan1191 span; saw: {:?}",
        String::from_utf8_lossy(out)
    );
}

/// Non-matching type-ahead must not split a streamed program token. A coarse
/// "input happened" signal would wrongly flush the buffered token; the suffix
/// match leaves it buffered because "x" is not the token.
#[test]
fn split_program_output_token_survives_concurrent_nonechoing_input() {
    assert_spans_intact(&echo_off_split_stream_while_typing(b"x"));
}

// Accepted limitation (no test): when the child has ECHO off AND the user types
// the EXACT bytes of a concurrently-streamed program token, the byte-equality
// suffix match can surface that program token and split its span. This is the
// deliberate trade that lets raw-mode/ssh echo surface (see the raw_mode_* tests
// below); the `idle` gate prevents it during continuous output, and the
// non-matching guard above still holds. Screen-safety is unaffected — only the
// child's own output bytes are ever emitted, never recent_input.

/// Raw-mode (ECHO-off) echo must also surface without extra input. This is the
/// nsupdate-over-ssh shape: the local PTY is raw with ECHO off, and the child
/// (here `cat` after `stty raw -echo`) re-emits forwarded bytes as program
/// output — exactly as a remote readline app's echo arrives back over ssh. The
/// buffered trailing token must surface on idle, not wait for a delimiter.
#[test]
fn raw_mode_paste_line_is_visible_without_extra_input() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("echo.trace");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("--trace-io");
    builder.arg(&trace_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg("stty raw -echo 2>/dev/null; printf 'READY\\n'; exec cat");

    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn ptty raw cat");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);
    let ready = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains("READY")
    });
    let mut visible = visible_output(&ready);
    assert!(
        visible.contains("READY"),
        "raw-mode child was not ready before paste; saw: {visible:?}"
    );

    // A delimiter-less trailing token ("192.0.2.1") echoed back by `cat` while
    // the tty has ECHO off. No newline: it can only surface via the idle flush.
    let paste = b"update add test.example.com 3600 A 192.0.2.1";
    writer.write_all(paste).expect("write paste");
    writer.flush().expect("flush paste");

    let target = "update add test.example.com 3600 A 192.0.2.1";
    let output = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains(target)
    });
    visible = visible_output(&output);

    assert!(
        visible.contains(target),
        "raw-mode pasted line never fully surfaced without extra input; saw: {visible:?}"
    );
    assert_drawn_by_echo_trigger(&trace_path, "echo-suffix");
}

/// The char-by-char mirror of the nsupdate report: in a raw/ECHO-off session the
/// running prefix of a typed token must surface at idle, before any delimiter.
/// Bytes are written one at a time with gaps, so each single-byte read is the
/// maximum split; without the idle flush the token stays invisible until Enter.
#[test]
fn raw_mode_typed_chars_are_visible_without_extra_input() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("echo.trace");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("--trace-io");
    builder.arg(&trace_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg("stty raw -echo 2>/dev/null; printf 'READY\\n'; exec cat");

    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn ptty raw cat");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);
    let ready = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains("READY")
    });
    let mut visible = visible_output(&ready);
    assert!(
        visible.contains("READY"),
        "raw-mode child was not ready before typing; saw: {visible:?}"
    );

    // A single delimiter-less token typed one byte at a time. With no space or
    // newline ever following, the only path to visibility is the idle flush.
    let target = b"showversion";
    for (idx, byte) in target.iter().enumerate() {
        writer.write_all(&[*byte]).expect("write byte");
        writer.flush().expect("flush byte");
        let prefix = &target[..=idx];
        let output = session.capture().wait_until(Duration::from_secs(2), |out| {
            contains_bytes(visible_output(out).as_bytes(), prefix)
        });
        visible = visible_output(&output);
        assert!(
            contains_bytes(visible.as_bytes(), prefix),
            "typed prefix {:?} did not cross the idle boundary; saw: {visible:?}",
            String::from_utf8_lossy(prefix)
        );
    }

    assert!(
        visible.contains("showversion"),
        "raw-mode typed token never surfaced without a delimiter; saw: {visible:?}"
    );
    assert_drawn_by_echo_trigger(&trace_path, "echo-suffix");
}

/// History recall: a shell rewrites the current input line by backspacing over
/// it, and the recalled command ends mid-token (`username=operator` has no
/// delimiter after the `=`). The keystroke that triggered the redraw is an arrow
/// key, so the recent-input byte match can never confirm the tail is echo — only
/// the line-edit provenance can surface it. Reported live: everything after the
/// `=` stayed invisible until the next keypress.
///
/// The child holds the line open on a file gate so the tail cannot surface via
/// `finish()` at EOF, which would make the assertion a tautology.
#[test]
fn history_recall_redraw_tail_is_visible_without_extra_input() {
    let gate_dir = tempfile::tempdir().expect("gate tempdir");
    let continue_path = gate_dir.path().join("continue");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("echo.trace");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("--trace-io");
    builder.arg(&trace_path);
    builder.env("PRISMTTY_TEST_CONTINUE", &continue_path);
    builder.arg("sh");
    builder.arg("-c");
    // Raw + ECHO off is the ssh shape: the remote line editor's redraw arrives as
    // ordinary program output, and the arrow key itself is never echoed back.
    builder.arg(
        "stty raw -echo 2>/dev/null; printf 'READY\\n'; head -c 3 >/dev/null; \
         printf '\\b\\b\\b\\b\\b\\bvault login -method=userpass username=operator'; \
         while [ ! -f \"$PRISMTTY_TEST_CONTINUE\" ]; do sleep 0.05; done; printf '\\n'",
    );

    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn ptty raw shell");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);
    let ready = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains("READY")
    });
    assert!(
        visible_output(&ready).contains("READY"),
        "raw-mode child was not ready before the recall; saw: {:?}",
        visible_output(&ready)
    );

    writer.write_all(b"\x1b[A").expect("write up arrow");
    writer.flush().expect("flush up arrow");

    let target = "username=operator";
    let output = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains(target)
    });
    let visible = visible_output(&output);
    std::fs::write(&continue_path, b"continue").expect("release the line-edit gate");

    assert!(
        visible.contains(target),
        "recalled history line lost its tail until the next keypress; saw: {visible:?}"
    );
    assert_drawn_by_echo_trigger(&trace_path, "line-edit-redraw");
}

/// The same recall, delivered in two reads. Over ssh the transport splits a
/// remote line editor's redraw routinely: the read that strands the tail no
/// longer carries the backspaces, so a per-chunk test of it sees nothing. Only
/// the accumulated line still shows the rewrite. Without that, the reported
/// symptom comes straight back.
#[test]
fn split_history_recall_redraw_tail_is_visible_without_extra_input() {
    let gate_dir = tempfile::tempdir().expect("gate tempdir");
    let continue_path = gate_dir.path().join("continue");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("echo.trace");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("--trace-io");
    builder.arg(&trace_path);
    builder.env("PRISMTTY_TEST_CONTINUE", &continue_path);
    builder.arg("sh");
    builder.arg("-c");
    // Two writes with a gap: the backspaces and the head of the line land in one
    // read, the stranded tail in another.
    builder.arg(
        "stty raw -echo 2>/dev/null; printf 'READY\\n'; head -c 3 >/dev/null; \
         printf '\\b\\b\\b\\b\\b\\bvault login'; sleep 0.4; \
         printf ' -method=userpass username=operator'; \
         while [ ! -f \"$PRISMTTY_TEST_CONTINUE\" ]; do sleep 0.05; done; printf '\\n'",
    );

    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn ptty raw shell");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);
    let ready = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains("READY")
    });
    assert!(
        visible_output(&ready).contains("READY"),
        "raw-mode child was not ready before the recall; saw: {:?}",
        visible_output(&ready)
    );

    writer.write_all(b"\x1b[A").expect("write up arrow");
    writer.flush().expect("flush up arrow");

    let target = "username=operator";
    let output = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains(target)
    });
    let visible = visible_output(&output);
    std::fs::write(&continue_path, b"continue").expect("release the line-edit gate");

    assert!(
        visible.contains(target),
        "split recall lost its tail until the next keypress; saw: {visible:?}"
    );
    assert_drawn_by_echo_trigger(&trace_path, "line-edit-redraw");
}

/// A line editor that redraws with cursor movement rather than backspaces
/// (zsh's zle once the edit is more than a few columns back) strands the tail
/// the same way. The read that moves the cursor and the read that strands the
/// tail arrive separately, so only a per-line signal connects them.
#[test]
fn csi_history_recall_redraw_tail_is_visible_without_extra_input() {
    let gate_dir = tempfile::tempdir().expect("gate tempdir");
    let continue_path = gate_dir.path().join("continue");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("echo.trace");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("--trace-io");
    builder.arg(&trace_path);
    builder.env("PRISMTTY_TEST_CONTINUE", &continue_path);
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(
        "stty raw -echo 2>/dev/null; printf 'READY\\n'; head -c 3 >/dev/null; \
         printf '\\033[6D\\033[K'; sleep 0.4; \
         printf 'vault login -method=userpass username=operator'; \
         while [ ! -f \"$PRISMTTY_TEST_CONTINUE\" ]; do sleep 0.05; done; printf '\\n'",
    );

    let child = pair
        .slave
        .spawn_command(builder)
        .expect("spawn ptty raw shell");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);
    let ready = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains("READY")
    });
    assert!(
        visible_output(&ready).contains("READY"),
        "raw-mode child was not ready before the recall; saw: {:?}",
        visible_output(&ready)
    );

    writer.write_all(b"\x1b[A").expect("write up arrow");
    writer.flush().expect("flush up arrow");

    let target = "username=operator";
    let output = session.capture().wait_until(Duration::from_secs(5), |out| {
        visible_output(out).contains(target)
    });
    let visible = visible_output(&output);
    std::fs::write(&continue_path, b"continue").expect("release the line-edit gate");

    assert!(
        visible.contains(target),
        "cursor-movement recall lost its tail until the next keypress; saw: {visible:?}"
    );
    assert_drawn_by_echo_trigger(&trace_path, "line-edit-redraw");
}

/// Runs `program args` on a terminal while the child holds its last write open
/// on a file gate, and returns what the terminal showed once `shown` held or
/// three seconds passed, before the gate was released. Nothing is ever typed,
/// so no echo trigger can surface the held output.
fn shown_while_program_output_is_quiet(
    program: &str,
    args: &[&str],
    shown: impl Fn(&str) -> bool,
) -> String {
    let gate_dir = tempfile::tempdir().expect("gate tempdir");
    let continue_path = gate_dir.path().join("continue");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(program);
    builder.env("PRISMTTY_TEST_CONTINUE", &continue_path);
    builder.args(args);
    let child = pair.slave.spawn_command(builder).expect("spawn child");
    drop(pair.slave);

    let session = PtySession::new(child, &*pair.master);
    let output = session
        .capture()
        .wait_until(Duration::from_secs(3), |out| shown(&visible_output(out)));
    std::fs::write(&continue_path, b"continue").expect("release the gate");
    visible_output(&output)
}

fn ends_with_visible(screen: &str, tail: &str) -> bool {
    screen.trim_end_matches(['\r', '\n']).ends_with(tail)
}

/// Program output that ends mid-token is held back so a token split across
/// reads still highlights as one span. When the program then goes quiet, as
/// Cisco `ping` does for two seconds per unanswered probe, that hold must end:
/// the dots used to stay invisible until the summary line arrived.
#[test]
fn quiet_program_output_tail_is_visible_after_holdback() {
    let visible = shown_while_program_output_is_quiet(
        env!("CARGO_BIN_EXE_ptty"),
        &[
            "-p",
            "cisco",
            "sh",
            "-c",
            "printf 'Sending 5, 100-byte ICMP Echos:\\r\\n..'; \
             while [ ! -f \"$PRISMTTY_TEST_CONTINUE\" ]; do sleep 0.05; done; \
             printf '\\r\\nSuccess rate is 0 percent\\r\\n'",
        ],
        |screen| ends_with_visible(screen, ".."),
    );

    assert!(
        ends_with_visible(&visible, ".."),
        "quiet program output stayed hidden; saw: {visible:?}"
    );
}

/// The same hold for a wrapped command whose stdin is not a terminal
/// (`ptty cmd </dev/null` run from a terminal): its output is still watched
/// live, so the held tail must be drawn once the program goes quiet.
#[test]
fn quiet_program_output_tail_is_visible_after_holdback_with_stdin_redirected() {
    let script = format!(
        "exec '{}' -p cisco sh -c \"printf 'Building configuration...'; \
         while [ ! -f \\\"\\$PRISMTTY_TEST_CONTINUE\\\" ]; do sleep 0.05; done; \
         printf '\\\\r\\\\n[OK]\\\\r\\\\n'\" </dev/null",
        env!("CARGO_BIN_EXE_ptty")
    );
    let visible = shown_while_program_output_is_quiet("sh", &["-c", &script], |screen| {
        ends_with_visible(screen, "configuration...")
    });

    assert!(
        ends_with_visible(&visible, "Building configuration..."),
        "quiet output of a wrapped command with stdin redirected stayed hidden; saw: {visible:?}"
    );
}

/// The same hold in pipe mode with stdout on a terminal, where no PTY master
/// carries the output: `tail -f` of a log that stops mid-token.
#[test]
fn quiet_piped_output_tail_is_visible_after_holdback() {
    let script = format!(
        "{{ printf 'Building configuration...'; \
         while [ ! -f \"$PRISMTTY_TEST_CONTINUE\" ]; do sleep 0.05; done; \
         printf '\\n[OK]\\n'; }} | '{}' -p cisco",
        env!("CARGO_BIN_EXE_prismtty")
    );
    let visible = shown_while_program_output_is_quiet("sh", &["-c", &script], |screen| {
        ends_with_visible(screen, "configuration...")
    });

    assert!(
        ends_with_visible(&visible, "Building configuration..."),
        "quiet piped output stayed hidden; saw: {visible:?}"
    );
}

/// Whether the terminal reads UTF-8 decides what a raw `9B` is: in a UTF-8
/// locale an isolated invalid byte in ordinary text, in an 8-bit one a CSI.
/// In UTF-8 the bytes after it are text like any other, so the highlight
/// ends after the matched word and nothing after the byte is colored; in an
/// 8-bit locale a line end the CSI holds runs, so under an inverse rendition
/// the highlight closes before the whole CSI and opens again after it.
#[test]
fn a_line_end_in_a_raw_9b_sequence_leaves_the_highlight_as_the_locale_reads_it() {
    let home = tempfile::tempdir().expect("scratch home");
    let runtime = home.path().join("runtime");
    std::fs::create_dir(&runtime).expect("runtime dir");
    std::fs::set_permissions(
        &runtime,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("runtime dir mode");
    let config = home.path().join("rules.yml");
    std::fs::write(
        &config,
        "rules:\n  - description: failure\n    regex: 'failed'\n    color: f#ff0000\n",
    )
    .expect("config");

    const RED: &str = "\x1b[38;2;255;0;0m";
    let utf8_minimal = "failed\x1b[39m\u{9b}3\r\n1mnext".to_string();
    let utf8_full = "failed\x1b[0m\x1b[7m\u{9b}3\r\n1mnext".to_string();
    let eight_bit_minimal = "failed\x1b[39m\u{9b}3\r\n1m\x1b[".to_string();
    let eight_bit_full = "failed\x1b[0m\x1b[7m\u{9b}3\r\n1m\x1b[".to_string();
    // Without a line end: text after the highlight in UTF-8, the program's
    // own red in an 8-bit locale.
    let utf8_no_line_end = "failed\x1b[39m\u{9b}31mx".to_string();
    let eight_bit_no_line_end = format!("failed\u{9b}31m{RED}\x1b[31mx");
    for (locale, flags, printed, expected) in [
        ("en_US.UTF-8", &[][..], "\\2333\\n1m", utf8_minimal),
        (
            "en_US.UTF-8",
            &["--no-minimal-reset"][..],
            "\\2333\\n1m",
            utf8_full,
        ),
        ("C", &[][..], "\\2333\\n1m", eight_bit_minimal),
        (
            "C",
            &["--no-minimal-reset"][..],
            "\\2333\\n1m",
            eight_bit_full,
        ),
        ("en_US.UTF-8", &[][..], "\\23331mx", utf8_no_line_end),
        ("C", &[][..], "\\23331mx", eight_bit_no_line_end),
    ] {
        // `\u{9b}` above stands for the raw byte 9B.
        let expected: Vec<u8> = expected
            .chars()
            .map(|c| if c == '\u{9b}' { 0x9b } else { c as u8 })
            .collect();
        let expected = expected.as_slice();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
        builder.env("LC_ALL", locale);
        builder.env("HOME", home.path());
        builder.env("XDG_CONFIG_HOME", home.path());
        builder.env("PRISMTTY_RUNTIME_DIR", &runtime);
        builder.arg("-c");
        builder.arg(&config);
        builder.args(["--rgb", "--no-auto-detect", "--no-dynamic-profile"]);
        builder.args(flags);
        builder.args(["sh", "-c"]);
        builder.arg(format!("printf '\\033[7mfailed{printed}next\\033[0m\\n'"));

        let child = pair.slave.spawn_command(builder).expect("spawn ptty");
        drop(pair.slave);
        let session = PtySession::new(child, &*pair.master);
        let out = session
            .capture()
            .wait_until(Duration::from_secs(5), |out| contains_bytes(out, b"next"));

        assert!(
            contains_bytes(&out, expected),
            "{locale} {flags:?}: expected {:?} in {:?}",
            String::from_utf8_lossy(expected),
            String::from_utf8_lossy(&out)
        );
    }
}

/// Runs `ptty` around `sh -c "printf <format>"` on a PTY this test creates,
/// with `rule` painted in its color, and returns the output once it is at
/// least `expected_len` bytes long or five seconds have passed.
fn ptty_printf_output(
    locale: &str,
    full_reset: bool,
    rule: &str,
    format: &str,
    expected_len: usize,
) -> Vec<u8> {
    let home = tempfile::tempdir().expect("scratch home");
    let runtime = home.path().join("runtime");
    std::fs::create_dir(&runtime).expect("runtime dir");
    std::fs::set_permissions(
        &runtime,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("runtime dir mode");
    let config = home.path().join("rules.yml");
    let color = if rule == "WARN" {
        "b#ffff00 f#000000"
    } else {
        "f#ff0000"
    };
    std::fs::write(
        &config,
        format!("rules:\n  - description: rule\n    regex: '{rule}'\n    color: {color}\n"),
    )
    .expect("config");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("LC_ALL", locale);
    builder.env("HOME", home.path());
    builder.env("XDG_CONFIG_HOME", home.path());
    builder.env("PRISMTTY_RUNTIME_DIR", &runtime);
    builder.arg("-c");
    builder.arg(&config);
    builder.args(["--rgb", "--no-auto-detect", "--no-dynamic-profile"]);
    if full_reset {
        builder.arg("--no-minimal-reset");
    }
    builder.args(["sh", "-c"]);
    builder.arg(format!("printf '{format}'"));
    let child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);
    let session = PtySession::new(child, &*pair.master);
    session
        .capture()
        .wait_until(Duration::from_secs(5), |out| out.len() >= expected_len)
}

/// A raw 9B sequence that ends in a command saving the rendition (`s`, mode
/// 1048 or 1049, XTPUSHSGR), or one filling blank cells under a highlight
/// that paints them (`J`, `K`, `X`, `@`, `P`, `L`, `M`, `S`, `T`), already has
/// the highlight closed before it in 1.4.0. In an 8-bit locale the output is
/// 1.4.0's, byte for byte, in both reset modes. In a UTF-8 locale the raw
/// `9B` is an isolated invalid byte in ordinary text, so no sequence ends
/// there and nothing after the line end is colored again.
#[test]
fn a_raw_9b_sequence_with_the_highlight_closed_before_it_keeps_its_bytes() {
    // Locale, full reset, rule, printf format, output. The C rows were
    // generated from the released 1.4.0 ptty; the UTF-8 rows give the bytes
    // 1.4.1 gives when the raw byte is FF instead.
    const RELEASE_RAW_9B_FAMILY: &[(&str, bool, &str, &str, &[u8])] = &[
        ("en_US.UTF-8", false, "failed", "\\033[7mfailed\\233\\nsnext\\033[0m\\n", b"\x1b[7m\x1b[38;2;255;0;0mfailed\x1b[39m\x9b\x0d\x0asnext\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", false, "failed", "\\033[7mfailed\\233?1049\\nhnext\\033[0m\\n", b"\x1b[7m\x1b[38;2;255;0;0mfailed\x1b[39m\x9b?1049\x0d\x0ahnext\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", false, "failed", "\\033[7mfailed\\233?1048\\nhnext\\033[0m\\n", b"\x1b[7m\x1b[38;2;255;0;0mfailed\x1b[39m\x9b?1048\x0d\x0ahnext\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", false, "failed", "\\033[7mfailed\\233\\n#{next\\033[0m\\n", b"\x1b[7m\x1b[38;2;255;0;0mfailed\x1b[39m\x9b\x0d\x0a#{next\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nJnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aJnext\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nKnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aKnext\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nXnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aXnext\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\n@next\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0a@next\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nPnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aPnext\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nLnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aLnext\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nMnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aMnext\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nSnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aSnext\x0d\x0a"),
        ("en_US.UTF-8", false, "WARN", "WARN\\233\\nTnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aTnext\x0d\x0a"),
        ("en_US.UTF-8", true, "failed", "\\033[7mfailed\\233\\nsnext\\033[0m\\n", b"\x1b[7m\x1b[1;38;2;255;0;0mfailed\x1b[0m\x1b[7m\x9b\x0d\x0asnext\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", true, "failed", "\\033[7mfailed\\233?1049\\nhnext\\033[0m\\n", b"\x1b[7m\x1b[1;38;2;255;0;0mfailed\x1b[0m\x1b[7m\x9b?1049\x0d\x0ahnext\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", true, "failed", "\\033[7mfailed\\233?1048\\nhnext\\033[0m\\n", b"\x1b[7m\x1b[1;38;2;255;0;0mfailed\x1b[0m\x1b[7m\x9b?1048\x0d\x0ahnext\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", true, "failed", "\\033[7mfailed\\233\\n#{next\\033[0m\\n", b"\x1b[7m\x1b[1;38;2;255;0;0mfailed\x1b[0m\x1b[7m\x9b\x0d\x0a#{next\x1b[0m\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nJnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aJnext\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nKnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aKnext\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nXnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aXnext\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\n@next\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0a@next\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nPnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aPnext\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nLnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aLnext\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nMnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aMnext\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nSnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aSnext\x0d\x0a"),
        ("en_US.UTF-8", true, "WARN", "WARN\\233\\nTnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aTnext\x0d\x0a"),
        ("C", false, "failed", "\\033[7mfailed\\233\\nsnext\\033[0m\\n", b"\x1b[7m\x1b[38;2;255;0;0mfailed\x1b[39m\x9b\x0d\x0as\x1b[38;2;255;0;0m\x1b[39mnext\x1b[0m\x0d\x0a"),
        ("C", false, "WARN", "WARN\\233\\nKnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[39;49m\x9b\x0d\x0aK\x1b[38;2;0;0;0;48;2;255;255;0m\x1b[39;49mnext\x0d\x0a"),
        ("C", true, "failed", "\\033[7mfailed\\233\\nsnext\\033[0m\\n", b"\x1b[7m\x1b[38;2;255;0;0mfailed\x1b[0m\x1b[7m\x9b\x0d\x0as\x1b[38;2;255;0;0m\x1b[0m\x1b[7mnext\x1b[0m\x0d\x0a"),
        ("C", true, "WARN", "WARN\\233\\nKnext\\n", b"\x1b[38;2;0;0;0;48;2;255;255;0mWARN\x1b[0m\x9b\x0d\x0aK\x1b[38;2;0;0;0;48;2;255;255;0m\x1b[0mnext\x0d\x0a"),
    ];
    for (locale, full_reset, rule, format, expected) in RELEASE_RAW_9B_FAMILY {
        let out = ptty_printf_output(locale, *full_reset, rule, format, expected.len());
        assert!(
            out == *expected,
            "{locale} full reset {full_reset}: {format}\n  got {:?}\n want {:?}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(expected)
        );
    }
}

#[path = "support/screen.rs"]
mod screen;

/// Through a real PTY: a wrapped program's blue prompt that resets its own
/// style right after the prompt (the audit's ESC-6 form a). The blue ends
/// where the program ended it, so the space after the prompt, the typed
/// command, the next output line and the next prompt are plain, in both
/// reset modes; 1.4.1 left all of them blue.
#[test]
fn prompt_background_ends_where_the_program_ends_it() {
    let format = "\\033[44mR1#\\033[0m show \\033[1mint\\r\\nnext output here\\r\\nR1#";
    for (full_reset, reset) in [(false, "\x1b[39m"), (true, "\x1b[0m")] {
        let expected =
            format!("\x1b[44mR1#\x1b[49m {reset}show int\r\r\nnext output here\r\r\nR1#");
        let out = ptty_printf_output("en_US.UTF-8", full_reset, "nomatch", format, expected.len());
        assert!(
            out == expected.as_bytes(),
            "full reset {full_reset}\n  got {:?}\n want {:?}",
            String::from_utf8_lossy(&out),
            expected
        );
        assert_eq!(
            screen::shown(&out),
            screen::runs(&[
                (0, "R1#", "bg=44"),
                (0, " show int", ""),
                (1, "next output here", ""),
                (2, "R1#", ""),
            ]),
            "full reset {full_reset}"
        );
    }
}

/// How a test puts prismtty on a terminal: `Stdout` makes the PTY its
/// standard output and a pipe its input (`printf | prismtty`); `Wrapped` runs
/// the command under `ptty`, which reads it from a PTY of its own.
#[derive(Clone, Copy, Debug)]
enum OnPty {
    Stdout,
    Wrapped,
}

/// Puts the output of `printf <format>` through prismtty on a PTY this test
/// creates, with `up` painted green, in an environment that holds only the
/// given locale variables, and returns what reached the PTY when it closes or
/// after five seconds.
fn printf_on_a_pty(how: OnPty, locale: &[(&str, &str)], flags: &[&str], format: &str) -> Vec<u8> {
    let home = tempfile::tempdir().expect("scratch home");
    let runtime = home.path().join("runtime");
    std::fs::create_dir(&runtime).expect("runtime dir");
    std::fs::set_permissions(
        &runtime,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("runtime dir mode");
    let config = home.path().join("rules.yml");
    std::fs::write(
        &config,
        "rules:\n  - description: up\n    regex: '\\bup\\b'\n    color: f#00ff00\n",
    )
    .expect("config");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut builder = match how {
        OnPty::Stdout => {
            let mut builder = CommandBuilder::new("sh");
            builder.arg("-c");
            builder.arg(format!(
                "printf '{format}' | '{}' -c '{}' --rgb --no-auto-detect {}",
                env!("CARGO_BIN_EXE_prismtty"),
                config.display(),
                flags.join(" ")
            ));
            builder
        }
        OnPty::Wrapped => {
            let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
            builder.arg("-c");
            builder.arg(&config);
            builder.args(["--rgb", "--no-auto-detect", "--no-dynamic-profile"]);
            builder.args(flags);
            builder.args(["sh", "-c"]);
            builder.arg(format!("printf '{format}'"));
            builder
        }
    };
    builder.env_clear();
    builder.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    builder.env("TERM", "xterm-256color");
    builder.env("HOME", home.path());
    builder.env("XDG_CONFIG_HOME", home.path());
    builder.env("PRISMTTY_RUNTIME_DIR", &runtime);
    for (name, value) in locale {
        builder.env(name, value);
    }
    let child = pair.slave.spawn_command(builder).expect("spawn prismtty");
    drop(pair.slave);
    let session = PtySession::new(child, &*pair.master);
    let finished = session.capture().finished_flag();
    session
        .capture()
        .wait_until(Duration::from_secs(5), |_| finished.load(Ordering::Acquire))
}

const UTF8_LOCALES_ON_A_PTY: &[&[(&str, &str)]] = &[
    &[("LC_ALL", "en_US.UTF-8")],
    &[("LC_ALL", ""), ("LC_CTYPE", "en_US.UTF-8")],
    &[("LANG", "bogus.UTF-8")],
];

const EIGHT_BIT_LOCALES_ON_A_PTY: &[&[(&str, &str)]] = &[
    &[("LC_ALL", "C")],
    &[("LC_ALL", "en_US.ISO8859-1")],
    &[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")],
];

/// On a terminal the rule is the pipe's: in a UTF-8 locale an isolated
/// invalid raw byte in ordinary text is text, by default and under
/// `--sanitize`, and the text after it is highlighted; outside one it opens
/// the string an 8-bit terminal reads, which hides the rest. `--sanitize`
/// drops the ESC in front of a raw byte either way, so no OSC 52 reaches the
/// terminal.
#[test]
fn a_stray_raw_c1_byte_reaches_a_terminal_as_the_locale_reads_it() {
    const UP: &[u8] = b"\x1b[38;2;0;255;0mup";
    let stav = "Stav: dr\\236\\355, rest of line up\\nnext up\\n";
    for how in [OnPty::Stdout, OnPty::Wrapped] {
        for flags in [&[][..], &["--sanitize"][..]] {
            for locale in UTF8_LOCALES_ON_A_PTY {
                let out = printf_on_a_pty(how, locale, flags, stav);
                let kept = [b"Stav: dr\x9e\xed, rest of line ".as_slice(), UP].concat();
                assert!(
                    contains_bytes(&out, &kept) && contains_bytes(&out, &[b"next ", UP].concat()),
                    "{how:?} {flags:?} {locale:?}: {:?}",
                    String::from_utf8_lossy(&out)
                );
            }
            for locale in EIGHT_BIT_LOCALES_ON_A_PTY {
                let out = printf_on_a_pty(how, locale, flags, stav);
                assert!(
                    contains_bytes(&out, b"Stav: dr") && !contains_bytes(&out, b"rest of line"),
                    "{how:?} {flags:?} {locale:?}: {:?}",
                    String::from_utf8_lossy(&out)
                );
            }
        }
        for (locales, expected) in [
            (UTF8_LOCALES_ON_A_PTY, b"A\x9d]52;c;QUJD\x07B".as_slice()),
            (EIGHT_BIT_LOCALES_ON_A_PTY, b"AB".as_slice()),
        ] {
            for locale in locales {
                let out = printf_on_a_pty(
                    how,
                    locale,
                    &["--sanitize"],
                    "A\\033\\235]52;c;QUJD\\007B\\n",
                );
                assert!(
                    contains_bytes(&out, expected) && !contains_bytes(&out, b"\x1b]"),
                    "{how:?} {locale:?}: {:?}",
                    String::from_utf8_lossy(&out)
                );
            }
        }
    }
}

/// A color set after a prompt is taken out before the typed command. With a
/// raw `9B` in the prompt, a UTF-8 terminal shows the byte and the text after
/// it, so the line is no prompt and the program's red stays; in an 8-bit
/// locale `9B ?1049h` switches to the alternate screen, where prompts are not
/// read either.
#[test]
fn a_prompt_holding_a_raw_c1_byte_is_read_as_the_terminal_shows_it() {
    let prompt = "\\233?1049hrouter\\233?9999999h#\\033[31mshow";
    for how in [OnPty::Stdout, OnPty::Wrapped] {
        for locale in UTF8_LOCALES_ON_A_PTY
            .iter()
            .chain(EIGHT_BIT_LOCALES_ON_A_PTY)
        {
            let out = printf_on_a_pty(how, locale, &[], prompt);
            assert!(
                contains_bytes(&out, b"\x9b?1049hrouter\x9b?9999999h#\x1b[31mshow"),
                "{how:?} {locale:?}: {:?}",
                String::from_utf8_lossy(&out)
            );
        }
    }
}

/// Known limit, stated in SECURITY.md and both READMEs: on a terminal
/// `--sanitize` does not end every unfinished control at the end of input.
/// In a UTF-8 locale `9D X` is text, and the ESC after it reaches the
/// terminal as it came, still open for whatever writes to the terminal next.
/// In an 8-bit locale the raw OSC is dropped and nothing is left open.
#[test]
fn known_limit_sanitize_on_a_terminal_can_end_with_an_unfinished_escape() {
    let tail = "user@host $ \\235X\\033";
    for how in [OnPty::Stdout, OnPty::Wrapped] {
        for locale in UTF8_LOCALES_ON_A_PTY {
            let out = printf_on_a_pty(how, locale, &["--sanitize"], tail);
            assert!(
                out.ends_with(b"user@host $ \x9dX\x1b"),
                "{how:?} {locale:?}: {:?}",
                String::from_utf8_lossy(&out)
            );
        }
        for locale in EIGHT_BIT_LOCALES_ON_A_PTY {
            let out = printf_on_a_pty(how, locale, &["--sanitize"], tail);
            assert!(
                out.ends_with(b"user@host $ ") && !contains_bytes(&out, b"X"),
                "{how:?} {locale:?}: {:?}",
                String::from_utf8_lossy(&out)
            );
        }
    }
}
