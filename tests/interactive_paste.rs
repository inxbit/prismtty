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

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
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
/// "...Vlan11" + "91" across two reads with an inter-write gap).
#[test]
fn split_program_output_token_keeps_single_highlight_span() {
    let trace_dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = trace_dir.path().join("split.trace");
    let continue_path = trace_dir.path().join("continue");
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
    // boundaries without relying on a scheduler-dependent sleep.
    builder.arg(
        "printf 'show: Vlan11'; while [ ! -f \"$PRISMTTY_TEST_CONTINUE\" ]; do sleep 0.01; done; printf '91 New TZ GW to Internal\\n'",
    );

    let child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);

    // Pure program output: we never write to the master, so no input echo is
    // pending and the idle flush must leave the buffered token alone.
    let session = PtySession::new(child, &*pair.master);
    let prefix_crossed_boundary = wait_for_condition(Duration::from_secs(5), || {
        contains_bytes(&trace_output_bytes(&trace_path), b"show: Vlan11")
    });
    assert!(
        prefix_crossed_boundary,
        "raw trace never recorded the token prefix before the continuation gate"
    );
    std::fs::write(&continue_path, b"continue").expect("release continuation gate");

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
fn echo_off_split_stream_while_typing(typed: &'static [u8]) -> Vec<u8> {
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
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(
        "stty -echo; i=0; while [ $i -lt 12 ]; do printf 'aaaaaaaa Vlan11'; \
         sleep 0.12; printf '91 bbbb\\n'; sleep 0.12; i=$((i+1)); done; printf 'STREAM_DONE\\n'",
    );

    let child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);

    let writer = pair.master.take_writer().expect("take writer");
    let session = PtySession::new(child, &*pair.master);

    // Type throughout, then stop explicitly. Linux PTYs can keep accepting
    // master writes briefly after the child exits, so do not rely on write
    // failure as the only thread-exit signal.
    let typer = TypingTask::start(writer, typed);
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

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
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

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
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

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
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

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
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
}
