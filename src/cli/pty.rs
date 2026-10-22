use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, AtomicUsize, Ordering},
    mpsc::{self, SyncSender},
};
use std::thread;
use std::time::{Duration, Instant};

use is_terminal::IsTerminal;
use nix::libc;
use nix::sys::termios::{
    InputFlags, LocalFlags, OutputFlags, SetArg, Termios, tcgetattr, tcsetattr,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::CliError;
use super::args::Options;
use super::profile_selection::dynamic_profile_enabled;
use super::runtime::{register_for_reload, unregister_before_exit};
use super::stream::{InputSource, highlight_stream, prepare_session};
use super::trace::IoTrace;

const STRIPPED_ITERM_ENV: [&str; 6] = [
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
];
const PROFILE_INPUT_QUEUE_CAPACITY: usize = 1024;
/// How many bytes of recently forwarded input the highlight loop keeps to match
/// against buffered echo. A buffered token is small (≤512 bytes), so this only
/// needs to cover the tail of the latest input; older input is dropped.
const RECENT_INPUT_WINDOW: usize = 4096;
/// Maximum incomplete control-sequence bytes retained by local echo between
/// reads. Real keyboard CSI sequences are far smaller; the cap prevents a
/// marker-free sequence from growing and being rescanned without bound.
const MAX_LOCAL_ECHO_PENDING_BYTES: usize = 256;
/// Catchable asynchronous signals whose default disposition terminates a
/// macOS or Linux process. Rust deliberately ignores SIGPIPE, synchronous
/// fault/core signals cannot safely return to the faulting instruction for
/// deferred cleanup, and SIGKILL/SIGSTOP cannot be caught.
#[cfg(not(target_os = "linux"))]
const TERMINATING_SIGNALS: [libc::c_int; 11] = [
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
];
#[cfg(all(
    target_os = "linux",
    not(any(
        target_arch = "mips",
        target_arch = "mips32r6",
        target_arch = "mips64",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))
))]
const TERMINATING_SIGNALS: [libc::c_int; 14] = [
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
    libc::SIGPWR,
    libc::SIGSTKFLT,
    libc::SIGIO,
];
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "mips",
        target_arch = "mips32r6",
        target_arch = "mips64",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "sparc64"
    )
))]
const TERMINATING_SIGNALS: [libc::c_int; 13] = [
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
    libc::SIGPWR,
    libc::SIGIO,
];
const JOB_CONTROL_SIGNALS: [libc::c_int; 3] = [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU];
const RAW_SIGNAL_STOP_BYTE: u8 = 0;
const RESIZE_STOP_BYTE: u8 = b'q';
const RESIZE_WAKE_BYTE: u8 = b'r';

static RAW_MODE_SIGNAL_PIPE: SignalPipeSlot = SignalPipeSlot::new();
static RESIZE_SIGNAL_PIPE: SignalPipeSlot = SignalPipeSlot::new();

/// How long `SignalPipeSlot::disarm` waits for an in-flight signal handler to
/// finish its write before giving up and leaking the descriptor instead of
/// closing it out from under the handler.
const SIGNAL_PIPE_DISARM_LIMIT: Duration = Duration::from_secs(1);

/// The write end of a self-pipe shared with an async signal handler.
///
/// The handler runs `notify`: it announces itself in `in_flight`, loads the
/// descriptor, writes one byte, and leaves. `disarm` publishes `-1` and then
/// waits for `in_flight` to drain before the caller closes the descriptor, so a
/// handler that loaded the descriptor just before it was withdrawn can never
/// write into a descriptor number that has since been reused. Everything the
/// handler touches is an atomic or `write(2)`, all async-signal-safe.
struct SignalPipeSlot {
    write_fd: AtomicI32,
    in_flight: AtomicUsize,
}

impl SignalPipeSlot {
    const fn new() -> Self {
        Self {
            write_fd: AtomicI32::new(-1),
            in_flight: AtomicUsize::new(0),
        }
    }

    fn arm(&self, write_fd: RawFd) {
        self.write_fd.store(write_fd, Ordering::SeqCst);
    }

    /// Async-signal-safe: called from signal handlers.
    fn notify(&self, byte: u8) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let write_fd = self.write_fd.load(Ordering::SeqCst);
        if write_fd >= 0 {
            write_signal_byte(write_fd, byte);
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Withdraws the descriptor from handlers and returns once no handler can
    /// still be writing to it, so the caller may close it. Returns `false` if a
    /// handler stayed in flight past the limit; the caller must then leak the
    /// descriptor rather than close it. A stalled handler's eventual write on
    /// the leaked end (whose reader has gone) fails with EPIPE, which is
    /// harmless because the Rust runtime ignores SIGPIPE.
    fn disarm(&self) -> bool {
        self.disarm_within(SIGNAL_PIPE_DISARM_LIMIT)
    }

    fn disarm_within(&self, limit: Duration) -> bool {
        self.write_fd.store(-1, Ordering::SeqCst);
        let deadline = Instant::now() + limit;
        while self.in_flight.load(Ordering::SeqCst) != 0 {
            if Instant::now() >= deadline {
                return false;
            }
            thread::yield_now();
        }
        true
    }
}
static FORWARD_CHILD_PID: AtomicI32 = AtomicI32::new(0);
static FORWARD_CHILD_LOCK: Mutex<()> = Mutex::new(());

fn forward_child_lock() -> std::sync::MutexGuard<'static, ()> {
    FORWARD_CHILD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn set_forward_child_pid(pid: libc::pid_t) {
    let _guard = forward_child_lock();
    FORWARD_CHILD_PID.store(pid, Ordering::SeqCst);
}

/// Maps a raw process exit code to a process exit byte, clamping codes above
/// 255 instead of truncating them (so e.g. 256 stays non-zero rather than
/// wrapping to a misleading success).
fn exit_code_byte(raw: u32) -> u8 {
    u8::try_from(raw).unwrap_or(u8::MAX)
}

/// Maps a reaped child's wait status to a process exit byte: a normal exit
/// preserves its (clamped) code, and a signal death reports `128 + signal`
/// (the shell convention) instead of a misleading generic code.
fn exit_code_from_wait(status: nix::sys::wait::WaitStatus) -> u8 {
    use nix::sys::wait::WaitStatus;
    match status {
        WaitStatus::Exited(_, code) => exit_code_byte(code as u32),
        WaitStatus::Signaled(_, signal, _) => 128u8.saturating_add(signal as i32 as u8),
        _ => 1,
    }
}

fn wait_for_child_exit_without_reaping(pid: libc::pid_t) -> io::Result<()> {
    loop {
        // SAFETY: siginfo_t is a plain C struct; all-zero bytes are a valid value that waitid
        // overwrites.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: `info` is a valid, exclusively borrowed siginfo_t for the call; WNOWAIT leaves
        // the child reapable.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn child_exit_is_waitable(pid: libc::pid_t) -> io::Result<bool> {
    loop {
        // SAFETY: siginfo_t is a plain C struct; all-zero bytes are a valid value that waitid
        // overwrites.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: `info` is a valid, exclusively borrowed siginfo_t for the call; WNOWAIT leaves
        // the child reapable.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if result == 0 {
            // SAFETY: waitid returned 0, so `info` is populated (WNOHANG zeroes si_pid when nothing
            // changed) and readable.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Reaps the wrapped child and returns its process-exit byte. Waits without
/// reaping first so the process-group id remains signalable while the PTY read
/// loop is ending, then serializes the final reap with signal forwarding. This
/// closes the otherwise orphan-prone gap between PTY EOF and child exit.
fn reap_child(child: &mut Box<dyn portable_pty::Child + Send + Sync>) -> Result<u8, CliError> {
    if let Some(pid) = child.process_id() {
        let pid = pid as libc::pid_t;
        wait_for_child_exit_without_reaping(pid)?;
        let _guard = forward_child_lock();
        let status = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid), None);
        FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
        return Ok(exit_code_from_wait(status?));
    }
    let status = child.wait()?;
    Ok(exit_code_byte(status.exit_code()))
}

/// Grace period between the polite SIGHUP and the SIGKILL escalation when the
/// wrapped child must be terminated on a failure path.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Terminates and reaps the wrapped child on a failure path without risking a
/// hang. Signals go to the child's process group (it is a PTY session leader,
/// so its pgid equals its pid), escalating from SIGHUP — which the child may
/// ignore — to SIGKILL. Every wait is bounded: a child blocked writing to an
/// undrained PTY sits in uninterruptible sleep (macOS), where even SIGKILL is
/// not acted on, and an unbounded reap would wedge the whole session. If the
/// child still cannot be reaped, give up; the process is exiting and init
/// will reap the remains.
fn terminate_and_reap_child(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let Some(pid) = child.process_id() else {
        let _ = child.try_wait();
        return;
    };
    terminate_and_reap_process_group(pid as libc::pid_t, libc::SIGHUP);
}

fn terminate_and_reap_process_group(pid: libc::pid_t, initial_signal: libc::c_int) {
    use nix::errno::Errno;
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    // SAFETY: kill has no memory-safety preconditions; `pid` is the wrapped session leader, so
    // `-pid` is its process group.
    unsafe {
        libc::kill(-pid, initial_signal);
    }
    let child_pid = nix::unistd::Pid::from_raw(pid);
    let deadline = Instant::now() + CHILD_EXIT_GRACE;
    while Instant::now() < deadline {
        match child_exit_is_waitable(pid) {
            Ok(true) => {
                // Keep the leader waitable until the final group signal so its
                // process-group id cannot be reused while descendants remain.
                // SAFETY: kill has no memory-safety preconditions; the leader is still unreaped, so
                // its group id has not been reused.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                let _ = waitpid(child_pid, None);
                return;
            }
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return,
            Ok(false) | Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }

    // SAFETY: kill has no memory-safety preconditions; the group id and pid belong to the still-
    // unreaped wrapped child.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
    let reap_deadline = Instant::now() + CHILD_EXIT_GRACE;
    while Instant::now() < reap_deadline {
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => thread::sleep(Duration::from_millis(20)),
            Ok(_) | Err(Errno::ECHILD) => break,
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Claims failure-path cleanup under the same lock used by signal forwarding.
/// A signal watcher either wins the lock and owns termination, or waits until
/// this path has reaped the child and cleared the registered pid.
fn terminate_and_reap_registered_child(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let _guard = forward_child_lock();
    FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
    terminate_and_reap_child(child);
}

/// Passes a fallible post-spawn setup step through; on failure, terminates and
/// reaps the just-spawned child so an early error cannot orphan it or hang on
/// a child that ignores SIGHUP.
fn cleanup_child_on_err<T, E>(
    result: Result<T, E>,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
) -> Result<T, E> {
    if result.is_err() {
        terminate_and_reap_registered_child(child);
    }
    result
}

/// Forwards a signal received by prismtty to the wrapped child's process group
/// so a `kill` of prismtty reaches the child instead of orphaning it. The child
/// is a PTY session leader, so its process-group id equals its pid.
fn forward_signal_to_child(signal: u8) {
    let _guard = forward_child_lock();
    let pid = FORWARD_CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill has no memory-safety preconditions; `pid` was read under
        // `forward_child_lock`, which serializes it with reaping.
        unsafe {
            libc::kill(-pid, libc::c_int::from(signal));
        }
    }
}

fn process_group_exists(pid: libc::pid_t) -> bool {
    // SAFETY: kill with signal 0 has no memory-safety preconditions and only checks that the group
    // exists.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Forwards a terminating signal to the wrapped PTY process group, allows a
/// bounded graceful exit, then kills any signal-immune descendants and reaps
/// the process-group leader before the wrapper exits from its signal watcher.
fn terminate_and_reap_forwarded_child(signal: libc::c_int) {
    let _guard = forward_child_lock();
    let pid = FORWARD_CHILD_PID.load(Ordering::SeqCst);
    if pid <= 0 {
        return;
    }
    FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
    terminate_and_reap_process_group(pid, signal);
}

fn wake_main_loop_after_input_failure() {
    let _guard = forward_child_lock();
    let pid = FORWARD_CHILD_PID.load(Ordering::SeqCst);
    if pid <= 0 {
        return;
    }
    // SAFETY: kill has no memory-safety preconditions; `pid` was read under `forward_child_lock`,
    // which serializes it with reaping.
    unsafe {
        libc::kill(-pid, libc::SIGHUP);
    }
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline && process_group_exists(pid) {
        thread::sleep(Duration::from_millis(10));
    }
    if process_group_exists(pid) {
        // SAFETY: kill has no memory-safety preconditions; `pid` was read under
        // `forward_child_lock`, which serializes it with reaping.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

fn record_input_worker_failure(
    state: &Mutex<Option<io::Error>>,
    error: io::Error,
    wake_session: impl FnOnce(),
) {
    let mut stored = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if stored.is_none() {
        *stored = Some(error);
    }
    drop(stored);
    wake_session();
}

fn run_input_worker(
    state: &Mutex<Option<io::Error>>,
    body: impl FnOnce() -> io::Result<()>,
    wake_session: impl FnOnce(),
) {
    let error = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error,
        Err(_) => io::Error::other("input forwarding thread stopped unexpectedly (panic)"),
    };
    record_input_worker_failure(state, error, wake_session);
}

fn take_input_worker_failure(state: &Mutex<Option<io::Error>>) -> Option<io::Error> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

fn with_terminal_restored_for_job_control(
    restore: impl FnOnce() -> io::Result<()>,
    suspend: impl FnOnce(),
    reapply: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    restore()?;
    suspend();
    reapply()
}

/// Prints a diagnostic to stderr. Unlike `eprintln!` it never panics, so an
/// unwritable stderr (EIO once the controlling terminal hangs up) cannot
/// abort cleanup or kill the signal watcher before it reaps the child.
fn warn(args: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "{args}");
}

/// Runs `body`, catching a panic so it surfaces as a clear diagnostic instead
/// of silently killing a detached worker thread (which could otherwise leave
/// the session wedged with no error). Returns `false` if `body` panicked.
fn run_supervised(name: &str, body: impl FnOnce()) -> bool {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_ok() {
        return true;
    }
    warn(format_args!(
        "prismtty: the {name} thread stopped unexpectedly (panic)"
    ));
    false
}

/// Spawns a worker thread whose body is run under [`run_supervised`].
fn spawn_supervised(
    name: &'static str,
    body: impl FnOnce() + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        run_supervised(name, body);
    })
}

pub(super) fn run_command(options: Options, command: Vec<OsString>) -> Result<ExitCode, CliError> {
    let command_name = command[0].clone();
    let command_args = command[1..].to_vec();
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(current_pty_size())?;
    let interactive = io::stdin().is_terminal();
    // Output on a terminal is watched live even when stdin is redirected
    // (`ptty cmd </dev/null`), so it gets the interactive highlighter and the
    // holdback flush, as stdin mode does when stdout is a terminal. Raw mode,
    // stdin forwarding and dynamic profiles stay keyed to `interactive`.
    let live_output = interactive || io::stdout().is_terminal();
    // Everything that can reject the invocation runs before the child exists,
    // so a typo in a flag or a broken config never runs part of the command,
    // and a reload warning prints before the terminal goes raw.
    let prepared = prepare_session(&options, live_output)?;
    let trace = IoTrace::open(options.trace_io.as_deref())?;
    let (_registration, reload_watcher) = register_for_reload();

    let program = cwd_relative_program(command_name);
    let mut builder = CommandBuilder::new(&program);
    let cwd = child_working_directory();
    if let Some(error) = unusable_script_interpreter(&program, cwd.as_deref()) {
        return Err(error);
    }
    if let Some(cwd) = &cwd {
        builder.cwd(cwd);
    }
    for arg in command_args {
        builder.arg(arg);
    }
    apply_iterm_shell_integration_guard(&mut builder, interactive, parent_terminal_is_iterm());

    if interactive {
        configure_child_pty(&*pair.master)?;
    } else {
        queue_end_of_input(&*pair.master)?;
    }

    let mut signal_mask = SessionSignalMask::block()?;
    let mut terminal_session = TerminalSession::start(interactive)?;
    let mut child = if interactive {
        pair.slave
            .spawn_command(builder)
            .map_err(|source| CliError::Spawn {
                code: spawn_failure_exit_code(&program, cwd.as_deref()),
                source,
            })?
    } else {
        spawn_with_caller_stdin(&builder, &*pair.master)?
    };
    drop(pair.slave);
    set_forward_child_pid(child.process_id().map(|pid| pid as i32).unwrap_or(0));
    cleanup_child_on_err(signal_mask.restore(), &mut child)?;

    let (profile_input_tx, profile_input_rx) = if dynamic_profile_enabled(&options, interactive) {
        let (tx, rx) = mpsc::sync_channel(PROFILE_INPUT_QUEUE_CAPACITY);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // An owned dup of the wrapped PTY master fd, kept for the whole session. The
    // read loop polls it (idle detection). Duping keeps that use independent of
    // the resize watcher (which owns the master itself), so a resize-thread exit
    // cannot close the descriptor out from under the read loop.
    let pty_fd_owned = if live_output {
        Some(cleanup_child_on_err(
            dup_master_fd(&*pair.master),
            &mut child,
        )?)
    } else {
        None
    };
    let idle_fd = pty_fd_owned.as_ref().map(AsRawFd::as_raw_fd);
    // Without a terminal on stdin nothing reaches the child through prismtty,
    // so there is no echo to wait for, and the highlight color must close
    // before each wait as in stdin mode (see `flush_before_wait`).
    let pty_fd = idle_fd.filter(|_| interactive);

    // Records bytes forwarded to the child so the highlight loop can recognise
    // their echo: it surfaces a buffered trailing token on idle only when the
    // token is a byte-for-byte suffix of this recent input, never a
    // speculatively-buffered token of program output. The buffer is scoped to
    // the in-progress line (cleared on submit/abort) and is never emitted to the
    // screen, so a non-echoed secret is never drawn even if it is briefly held.
    let recent_input = Arc::new(Mutex::new(Vec::new()));
    let input_worker_failure = Arc::new(Mutex::new(None));
    if interactive {
        let mut writer = cleanup_child_on_err(stdin_forward_writer(&*pair.master), &mut child)?;
        let trace = trace.clone();
        let local_echo = options.local_echo;
        let recent_input = Arc::clone(&recent_input);
        let input_worker_failure = Arc::clone(&input_worker_failure);
        thread::spawn(move || {
            run_input_worker(
                &input_worker_failure,
                || {
                    let stdin = io::stdin();
                    let mut stdin = WaitWhenWouldBlock {
                        inner: stdin.lock(),
                        fd: libc::STDIN_FILENO,
                    };
                    forward_stdin_to_pty(
                        &mut stdin,
                        &mut writer,
                        local_echo,
                        trace,
                        profile_input_tx,
                        &recent_input,
                    )
                },
                wake_main_loop_after_input_failure,
            );
        });
    }

    let mut reader = cleanup_child_on_err(pair.master.try_clone_reader(), &mut child)?;
    let mut resize_watcher =
        cleanup_child_on_err(PtyResizeWatcher::start(pair.master), &mut child)?;

    let mut stdout = WaitWhenWouldBlock {
        inner: io::stdout(),
        fd: libc::STDOUT_FILENO,
    };
    let stream_result = highlight_stream(
        &mut reader,
        &mut stdout,
        prepared,
        InputSource {
            interactive: live_output,
            pty_fd,
            idle_fd,
            recent_input: Some(recent_input),
        },
        reload_watcher,
        trace,
        profile_input_rx,
    );

    resize_watcher.stop();
    // Reap the child on every exit path. If the stream errored the child may
    // still be running, so terminate it first — escalating past SIGHUP, which
    // the child may ignore — to avoid blocking forever on the reap. Drain the
    // master while doing so: with the read loop gone, a child blocked writing
    // to the full PTY is uninterruptible and cannot act on any signal until
    // its write completes.
    let exit = if stream_result.is_err() {
        thread::spawn(move || {
            let mut sink = [0u8; 4096];
            while matches!(reader.read(&mut sink), Ok(n) if n > 0) {}
        });
        terminate_and_reap_registered_child(&mut child);
        Ok(1)
    } else {
        reap_child(&mut child)
    };
    let input_worker_failure = take_input_worker_failure(&input_worker_failure);
    terminal_session.stop();
    if let Some(error) = input_worker_failure {
        return Err(error.into());
    }
    stream_result?;
    Ok(ExitCode::from(exit?))
}

/// The directory to start the wrapped command in: the caller's, whenever the
/// child can enter it. portable-pty starts the child in $HOME when it is given
/// no directory, or one it cannot stat. `.` still names the caller's directory
/// when it was deleted (getcwd fails) or an ancestor cannot be searched; the
/// child then stays there, as exec would leave it, and a relative path fails
/// instead of acting on $HOME. Only a directory the user cannot search at all
/// (`sudo -u postgres prismtty psql` from a private home, where getcwd still
/// works on Linux but chdir fails) falls back to $HOME, with a warning, so the
/// command still starts.
fn child_working_directory() -> Option<PathBuf> {
    let enterable = |dir: &Path| dir.is_dir() && is_executable(dir);
    match std::env::current_dir() {
        Ok(dir) if enterable(&dir) => Some(dir),
        _ if enterable(Path::new(".")) => Some(PathBuf::from(".")),
        _ => {
            warn(format_args!(
                "prismtty: cannot enter the current directory; running the command in $HOME"
            ));
            None
        }
    }
}

/// Prefixes a relative program path with a directory part (`bin/tool`) with
/// `./`. Like execvp, a name containing `/` is resolved against the working
/// directory, but portable-pty only does that for `./` and `../` paths and
/// searches PATH for anything else.
fn cwd_relative_program(program: OsString) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    let path = Path::new(&program);
    let explicit = matches!(
        path.components().next(),
        Some(Component::CurDir | Component::ParentDir)
    );
    if path.is_relative() && !explicit && program.as_bytes().contains(&b'/') {
        let mut prefixed = OsString::from("./");
        prefixed.push(&program);
        prefixed
    } else {
        program
    }
}

/// Exit code for a wrapped command that could not be spawned, following the
/// shell convention: 127 when nothing by that name exists, 126 when it exists
/// but is a directory or is not executable. A name that resolves to an
/// executable failed for some other reason and keeps the generic 1.
fn spawn_failure_exit_code(program: &OsStr, cwd: Option<&Path>) -> u8 {
    let mut exists = false;
    for candidate in program_candidates(program, cwd) {
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_dir() && is_executable(&candidate) {
            return 1;
        }
        exists = true;
    }
    if exists { 126 } else { 127 }
}

/// The paths execvp would try for `program`, in order: the name itself
/// against `cwd` when it contains `/`, otherwise each PATH entry.
fn program_candidates(program: &OsStr, cwd: Option<&Path>) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let cwd = cwd.unwrap_or(Path::new(""));
    if program.is_empty() {
        Vec::new()
    } else if program.as_bytes().contains(&b'/') {
        vec![cwd.join(program)]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|dir| cwd.join(dir).join(program))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// How a kernel reads the `#!` line of a script. It reads the start of the
/// file into a buffer, zero-filled past the end of a shorter file, skips
/// blanks (space, tab) after `#!`, and ends the interpreter name at a blank,
/// a NUL or the end of the line.
struct ShebangSyntax {
    /// How much of the file the kernel reads.
    buffer_bytes: usize,
    /// The bytes that end the line.
    line_ends: &'static [u8],
    /// Whether a line with no end in the buffer still names an interpreter
    /// when a blank follows the name, which only cuts off its arguments.
    /// Otherwise the kernel reports ENOEXEC, and execvp runs the file with
    /// /bin/sh.
    names_in_unended_line: bool,
}

/// Linux, `load_script` in fs/binfmt_script.c: the buffer is BINPRM_BUF_SIZE,
/// and the newline search (strnchr) stops at a NUL, so the line of a short
/// file with no newline ends where the file does. `#` is part of the name.
const LINUX_SHEBANG: ShebangSyntax = ShebangSyntax {
    buffer_bytes: 256,
    line_ends: b"\n\0",
    names_in_unended_line: true,
};

/// macOS, `exec_shell_imgact` in XNU's bsd/kern/kern_exec.c: the buffer is
/// IMG_SHSIZE, and `#` ends the line as a newline does. A NUL ends neither,
/// so a short file with no newline is ENOEXEC.
const XNU_SHEBANG: ShebangSyntax = ShebangSyntax {
    buffer_bytes: 512,
    line_ends: b"\n#",
    names_in_unended_line: false,
};

/// The `#!` syntax of the kernel prismtty runs on, where it is known.
const KERNEL_SHEBANG: Option<ShebangSyntax> =
    if cfg!(any(target_os = "linux", target_os = "android")) {
        Some(LINUX_SHEBANG)
    } else if cfg!(target_vendor = "apple") {
        Some(XNU_SHEBANG)
    } else {
        None
    };

/// The interpreter a kernel with `syntax` runs for a file that starts with
/// `start`, or None when it runs none: not a script, no name, or ENOEXEC.
fn script_interpreter(start: &[u8], syntax: &ShebangSyntax) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let mut buffer = start[..start.len().min(syntax.buffer_bytes)].to_vec();
    buffer.resize(syntax.buffer_bytes, 0);
    let line = buffer.strip_prefix(b"#!")?;
    let (line, ended) = match line.iter().position(|byte| syntax.line_ends.contains(byte)) {
        Some(end) => (&line[..end], true),
        None if syntax.names_in_unended_line => (line, false),
        None => return None,
    };
    let is_blank = |byte: &u8| matches!(byte, b' ' | b'\t');
    let name = &line[line.iter().position(|byte| !is_blank(byte))?..];
    let name = match name.iter().position(|byte| is_blank(byte) || *byte == 0) {
        Some(end) => &name[..end],
        // The buffer may have cut the name off: ENOEXEC.
        None if !ended => return None,
        None => name,
    };
    (!name.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(name)))
}

/// Checks the `#!` interpreter of a script before it is spawned. A script
/// whose interpreter cannot be executed only fails in execve, after the fork.
/// portable-pty, which spawns the command from a terminal, closes every
/// descriptor above 2 in the child, std's exec-error pipe included, so the
/// child aborts with "fatal runtime error" instead of the spawn reporting the
/// failure. The line is parsed as the kernel does, so a CRLF script names
/// `/bin/sh\r`. Whatever the kernel would not reject for certain is left to
/// the spawn: an empty name, a relative one when the child's directory is not
/// known, and every script on a kernel whose syntax is not known here.
fn unusable_script_interpreter(program: &OsStr, cwd: Option<&Path>) -> Option<CliError> {
    let syntax = KERNEL_SHEBANG?;
    let script = program_candidates(program, cwd)
        .into_iter()
        .find(|candidate| !candidate.is_dir() && is_executable(candidate))?;
    // Opening an executable FIFO for reading would wait for a writer.
    if !script.is_file() {
        return None;
    }
    let mut start = Vec::new();
    std::fs::File::open(&script)
        .ok()?
        .take(syntax.buffer_bytes as u64)
        .read_to_end(&mut start)
        .ok()?;
    let interpreter = script_interpreter(&start, &syntax)?;
    // A relative interpreter is found from the child's directory, which is
    // $HOME, not known here, when the caller's cannot be entered.
    let interpreter_path = match cwd {
        Some(cwd) => cwd.join(&interpreter),
        None if interpreter.is_absolute() => interpreter.clone(),
        None => return None,
    };
    let code = match std::fs::metadata(&interpreter_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => 127,
        Err(_) => 126,
        Ok(metadata) if metadata.is_dir() || !is_executable(&interpreter_path) => 126,
        Ok(_) => return None,
    };
    Some(CliError::Spawn {
        code,
        source: anyhow::anyhow!(
            "Unable to spawn {} because of a bad interpreter {:?}",
            script.display(),
            interpreter
        ),
    })
}

/// Spawns the wrapped command when stdin is not a terminal. The child reads
/// the caller's stdin itself, so every read sees what `cmd < file` would: the
/// data of a pipe or file, then end of input, however often it reads. The PTY
/// slave is its stdout, stderr and controlling terminal. portable-pty always
/// puts the slave on stdin, so this mirrors its spawn otherwise: the program
/// is resolved against the same directory and PATH (`resolve_program` says
/// where its messages differ), SHELL is set when unset (the builder's
/// environment is the process's own on this path), and the child starts as a
/// session leader with default signal dispositions and an empty signal mask.
/// Inherited descriptors are marked close-on-exec rather than closed, which
/// keeps std's exec-error pipe working, so an execve failure is reported
/// instead of aborting the child.
fn spawn_with_caller_stdin(
    builder: &CommandBuilder,
    master: &dyn portable_pty::MasterPty,
) -> Result<Box<dyn portable_pty::Child + Send + Sync>, CliError> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let argv = builder.get_argv();
    let dir = builder
        .get_cwd()
        .map(Path::new)
        .filter(|dir| dir.is_dir())
        .map_or_else(|| home_dir(builder), Path::to_path_buf);
    let program = resolve_program(&argv[0], &dir).map_err(|source| CliError::Spawn {
        code: spawn_failure_exit_code(&argv[0], Some(&dir)),
        source,
    })?;
    let tty_name = master
        .tty_name()
        .ok_or_else(|| io::Error::other("PTY slave name is unavailable"))?;
    // O_NOCTTY: a session leader without a controlling terminal would
    // otherwise take this one, and the child could no longer claim it.
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty_name)?;
    let inherited = inherited_fds();
    // SAFETY: sigset_t is a plain C type; sigemptyset below initializes it before it is used.
    let mut empty_mask = unsafe { mem::zeroed::<libc::sigset_t>() };
    // SAFETY: `empty_mask` is a valid, exclusively borrowed sigset_t.
    unsafe {
        libc::sigemptyset(&mut empty_mask);
    }

    let mut command = Command::new(&program);
    command
        .arg0(&argv[0])
        .args(&argv[1..])
        .current_dir(&dir)
        .stdin(Stdio::inherit())
        .stdout(slave.try_clone()?)
        .stderr(slave);
    if let Some(shell) = builder.get_env("SHELL") {
        command.env("SHELL", shell);
    }
    // SAFETY: the closure runs in the forked child before exec. It only calls
    // async-signal-safe functions (signal, sigprocmask, setsid, ioctl, fcntl)
    // and allocates nothing: the mask and the descriptor list were prepared
    // before the fork.
    unsafe {
        command.pre_exec(move || {
            for signal in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            libc::sigprocmask(libc::SIG_SETMASK, &empty_mask, std::ptr::null_mut());
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // stdout is the PTY slave.
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            for &fd in &inherited {
                // A descriptor closed since it was listed only reports EBADF.
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| {
        // std enters `dir` in the child before the exec and reports a failure
        // there as the spawn's. Only the $HOME fallback can be a directory the
        // child cannot enter, and then the program is not to blame.
        if !(dir.is_dir() && is_executable(&dir)) {
            return CliError::Spawn {
                code: 1,
                source: anyhow::anyhow!(
                    "Unable to enter {} to spawn {}: {error}",
                    dir.display(),
                    program.display()
                ),
            };
        }
        CliError::Spawn {
            code: exec_failure_exit_code(&error),
            source: anyhow::anyhow!("Unable to spawn {}: {error}", program.display()),
        }
    })?;
    Ok(Box::new(child))
}

/// Where portable-pty starts a child that has no usable directory: $HOME,
/// else the user's home directory, else `/`.
fn home_dir(builder: &CommandBuilder) -> PathBuf {
    builder
        .get_env("HOME")
        .and_then(OsStr::to_str)
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Resolves the program as portable-pty's spawn does: a name with a `/`
/// against `dir`, any other name in each PATH entry. The messages are its
/// own, except for an absolute path that is missing or not executable, which
/// reads as a relative one does (portable-pty says "doesn't exist on the
/// filesystem" and adds the errno), and an empty name, for which portable-pty
/// also lists each PATH entry as a directory.
fn resolve_program(program: &OsStr, dir: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    if program.as_bytes().contains(&b'/') {
        let path = dir.join(program);
        if path.is_dir() {
            anyhow::bail!(
                "Unable to spawn {} because it is a directory",
                path.display()
            );
        }
        if is_executable(&path) {
            return Ok(path);
        }
        if path.exists() {
            anyhow::bail!(
                "Unable to spawn {} because it is not executable",
                path.display()
            );
        }
        anyhow::bail!(
            "Unable to spawn {} because it does not exist",
            path.display()
        );
    }
    let mut errors = Vec::new();
    for candidate in program_candidates(program, Some(dir)) {
        if candidate.is_dir() {
            errors.push(format!("{} exists but is a directory", candidate.display()));
        } else if is_executable(&candidate) {
            return Ok(candidate);
        } else if candidate.exists() {
            errors.push(format!(
                "{} exists but is not executable",
                candidate.display()
            ));
        }
    }
    errors.push(match std::env::var_os("PATH") {
        Some(path) => format!("No viable candidates found in PATH {path:?}"),
        None => "Unable to resolve the PATH".to_owned(),
    });
    anyhow::bail!(
        "Unable to spawn {} because:\n{}",
        Path::new(program).display(),
        errors.join(".\n")
    )
}

/// Exit code for a resolved command whose execve failed, as a shell reports
/// it: 127 when a file it needs does not exist (a script's interpreter, a
/// program's loader), 126 when it cannot be executed. Anything else, such as
/// the child failing to take the PTY, keeps the generic 1.
fn exec_failure_exit_code(error: &io::Error) -> u8 {
    match error.raw_os_error() {
        Some(libc::ENOENT) => 127,
        Some(
            libc::EACCES
            | libc::ENOEXEC
            | libc::EISDIR
            | libc::ENOTDIR
            | libc::ELOOP
            | libc::ENAMETOOLONG
            | libc::ETXTBSY
            | libc::E2BIG,
        ) => 126,
        _ => 1,
    }
}

/// Descriptors above stderr that would survive an exec: ones prismtty
/// inherited without close-on-exec, since all it opens itself is
/// close-on-exec. Like portable-pty, this lists /dev/fd, and finds nothing
/// where that cannot be read. Listing allocates, so it runs before the fork.
fn inherited_fds() -> Vec<RawFd> {
    let Ok(entries) = std::fs::read_dir("/dev/fd") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<RawFd>().ok())
        .filter(|&fd| fd > 2)
        .filter(|&fd| {
            // SAFETY: F_GETFD takes no pointer argument; an invalid `fd` reports EBADF instead of
            // touching memory.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            flags >= 0 && flags & libc::FD_CLOEXEC == 0
        })
        .collect()
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

fn parent_terminal_is_iterm() -> bool {
    std::env::var_os("ITERM_SESSION_ID").is_some()
        || std::env::var_os("TERM_PROGRAM").as_deref() == Some(OsStr::new("iTerm.app"))
        || std::env::var_os("LC_TERMINAL").as_deref() == Some(OsStr::new("iTerm.app"))
}

fn apply_iterm_shell_integration_guard(
    builder: &mut CommandBuilder,
    interactive: bool,
    iterm_parent: bool,
) {
    if !interactive || !iterm_parent {
        return;
    }

    for key in STRIPPED_ITERM_ENV {
        if let Some(value) = builder
            .get_env(key)
            .map(OsString::from)
            .or_else(|| std::env::var_os(key))
        {
            builder.env(format!("PRISMTTY_PARENT_{key}"), value);
        }
        builder.env_remove(key);
    }

    // iTerm shell-integration scripts key off the original names. The
    // PRISMTTY_PARENT_* copies keep user dotfile context without re-enabling
    // nested integration marks in the child shell.
    builder.env("ITERM_SHELL_INTEGRATION_INSTALLED", "prismtty");
    builder.env("ITERM2_SQUELCH_MARK", "1");
    builder.env("PRISMTTY_NESTED_ITERM", "1");
}

#[cfg(unix)]
fn configure_child_pty(master: &dyn portable_pty::MasterPty) -> Result<(), CliError> {
    let stdin = io::stdin();
    let source = tcgetattr(stdin.as_fd())?;
    let Some(tty_name) = master.tty_name() else {
        return Ok(());
    };
    // O_NOCTTY: a session leader without a controlling terminal would
    // otherwise take this one, and the child could no longer claim it.
    let slave_tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty_name)?;
    let mut termios = source;
    normalize_child_pty_termios(&mut termios);
    tcsetattr(slave_tty.as_fd(), SetArg::TCSANOW, &termios)?;
    Ok(())
}

/// Queues an end of input on the child's PTY when there is no terminal to
/// forward from. The child reads the caller's stdin itself then, but the PTY
/// is still its controlling terminal, so a first reader of /dev/tty (a
/// password prompt) sees EOF instead of blocking forever. Only VEOF is sent,
/// before the child exists: a newline would read as an extra empty line, and
/// echo is off for the write because macOS echoes VEOF as `^D`.
fn queue_end_of_input(master: &dyn portable_pty::MasterPty) -> io::Result<()> {
    let mut pty = stdin_forward_writer(master)?;
    let fd = pty.as_raw_fd();
    let attrs = terminal_attrs(fd)?;
    let eof = attrs.c_cc[libc::VEOF];
    if eof == 0 {
        return Ok(());
    }
    let mut quiet = attrs;
    quiet.c_lflag &= !libc::ECHO;
    set_terminal_attrs_retrying_eintr(fd, &quiet)?;
    let written = pty.write_all(&[eof]);
    set_terminal_attrs_retrying_eintr(fd, &attrs)?;
    written
}

fn normalize_child_pty_termios(termios: &mut Termios) {
    let (local, input, output) = normalize_child_pty_flags(
        termios.local_flags,
        termios.input_flags,
        termios.output_flags,
    );
    termios.local_flags = local;
    termios.input_flags = input;
    termios.output_flags = output;
}

fn normalize_child_pty_flags(
    mut local: LocalFlags,
    mut input: InputFlags,
    mut output: OutputFlags,
) -> (LocalFlags, InputFlags, OutputFlags) {
    local.insert(
        LocalFlags::ECHO
            | LocalFlags::ECHOE
            | LocalFlags::ECHOK
            | LocalFlags::ICANON
            | LocalFlags::ISIG
            | LocalFlags::IEXTEN,
    );
    input.insert(InputFlags::ICRNL);
    output.insert(OutputFlags::OPOST);
    (local, input, output)
}

/// Reads from or writes to `inner`, waiting for `fd` to become ready whenever
/// the call reports `WouldBlock`. A terminal's open file description is
/// shared by every process on it, and usually by prismtty's stdin and stdout
/// too, so another program can leave or set `O_NONBLOCK` on them; an empty
/// non-blocking read, or an output queue the terminal has not drained yet, is
/// not an I/O failure. Clearing the flag instead would change it for those
/// programs too, and miss it being set again mid-session.
struct WaitWhenWouldBlock<T> {
    inner: T,
    fd: RawFd,
}

impl<R: Read> Read for WaitWhenWouldBlock<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        retry_when_would_block(self.fd, libc::POLLIN, || self.inner.read(buf))
    }
}

// A write that fails took none of `buf`, as `Write` requires and std's
// buffered stdout keeps, so it can be retried as it is.
impl<W: Write> Write for WaitWhenWouldBlock<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        retry_when_would_block(self.fd, libc::POLLOUT, || self.inner.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        retry_when_would_block(self.fd, libc::POLLOUT, || self.inner.flush())
    }
}

fn retry_when_would_block<T>(
    fd: RawFd,
    events: libc::c_short,
    mut call: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    loop {
        match call() {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_until_ready(fd, events)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn wait_until_ready(fd: RawFd, events: libc::c_short) -> io::Result<()> {
    let mut poll_fd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        // SAFETY: `poll_fd` is a valid, exclusively borrowed pollfd and the count matches.
        if unsafe { libc::poll(&mut poll_fd, 1, -1) } >= 0 {
            // The requested event, POLLHUP and POLLERR all mean the next call
            // does not report WouldBlock again: it transfers data, sees end of
            // input or returns the error.
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn forward_stdin_to_pty<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    local_echo: bool,
    trace: IoTrace,
    profile_input: Option<SyncSender<Vec<u8>>>,
    recent_input: &Mutex<Vec<u8>>,
) -> io::Result<()> {
    let mut buffer = [0_u8; 1024];
    let mut echo_state = LocalEchoState::default();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        let input = &buffer[..read];
        trace.log("IN", input);
        if let Some(sender) = &profile_input {
            let _ = sender.try_send(input.to_vec());
        }
        // Record before the PTY write so an immediate line-discipline echo cannot
        // outrun the highlight loop's suffix match. The match runs on the read-loop
        // thread and `write_all` may block, so this is a cross-thread pre-write
        // window; it is idle-gated and only ever surfaces the child's own output.
        record_recent_input(recent_input, input);
        if let Err(error) = writer.write_all(input).and_then(|()| writer.flush()) {
            // EIO on the master means every slave descriptor is closed: the
            // wrapped child has exited and the read loop is already at EOF.
            // Input that arrived after that point (a paste ending in `exit`,
            // fast typing) is not an input failure; the child's own exit
            // status must be reported.
            if error.raw_os_error() == Some(libc::EIO) {
                return Ok(());
            }
            return Err(error);
        }

        if local_echo {
            let echo = echo_state.push(input);
            if !echo.is_empty() {
                let mut stdout = WaitWhenWouldBlock {
                    inner: io::stdout().lock(),
                    fd: libc::STDOUT_FILENO,
                };
                stdout.write_all(&echo)?;
                stdout.flush()?;
            }
        }
    }
}

/// Records forwarded input for the highlight loop's echo matching.
///
/// Appends `input`, then scopes the buffer to the in-progress line: everything up
/// to and including the last line-break/abort byte is dropped, so a line that is
/// submitted (CR/LF), interrupted (Ctrl-C), EOF'd (Ctrl-D), killed (Ctrl-U),
/// suspended (Ctrl-Z), or quit (Ctrl-\) does not linger. The retained tail is
/// bounded by [`RECENT_INPUT_WINDOW`]. This buffer is only ever compared against
/// the child's echoed output and is never emitted, so a non-echoed secret (e.g. a
/// password) is never drawn even while it is briefly held. Backspace and
/// word-erase are not normalised out — such bytes may remain until the next
/// break/abort byte, a window overflow, or a successful flush clears them.
fn record_recent_input(recent_input: &Mutex<Vec<u8>>, input: &[u8]) {
    fn is_line_break(byte: &u8) -> bool {
        matches!(byte, b'\r' | b'\n' | 0x03 | 0x04 | 0x15 | 0x1a | 0x1c)
    }
    let mut recent = recent_input
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    recent.extend_from_slice(input);
    if let Some(idx) = recent.iter().rposition(is_line_break) {
        recent.drain(..=idx);
    }
    let overflow = recent.len().saturating_sub(RECENT_INPUT_WINDOW);
    if overflow > 0 {
        recent.drain(..overflow);
    }
}

/// The writer the stdin forwarder sends input through: a plain dup of the
/// PTY master. portable-pty's own writer writes a newline and VEOF into the
/// PTY when dropped, which would submit a partial line the user never entered
/// (and can log out a remote shell) whenever forwarding ends.
fn stdin_forward_writer(master: &dyn portable_pty::MasterPty) -> io::Result<std::fs::File> {
    dup_master_fd(master).map(std::fs::File::from)
}

fn dup_master_fd(master: &dyn portable_pty::MasterPty) -> io::Result<OwnedFd> {
    let fd = master.as_raw_fd().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY master file descriptor is unavailable",
        )
    })?;
    // SAFETY: `fd` is the live PTY master descriptor. F_DUPFD_CLOEXEC returns a
    // fresh close-on-exec descriptor that `OwnedFd` takes sole ownership of and
    // closes on drop.
    let duped = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duped < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duped` is a newly-created descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(duped) })
}

#[cfg(test)]
fn local_echo_bytes(input: &[u8]) -> Vec<u8> {
    LocalEchoState::default().push(input)
}

#[derive(Default)]
struct LocalEchoState {
    pending: Vec<u8>,
}

impl LocalEchoState {
    fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(input);
        let mut output = Vec::new();
        let mut idx = 0;

        while idx < bytes.len() {
            match local_echo_step(&bytes, idx) {
                LocalEchoStep::Echo(byte) => {
                    output.push(byte);
                    idx += 1;
                }
                LocalEchoStep::Backspace => {
                    output.extend_from_slice(b"\x08 \x08");
                    idx += 1;
                }
                LocalEchoStep::Newline => {
                    output.extend_from_slice(b"\r\n");
                    idx += 1;
                }
                LocalEchoStep::Skip(next_idx) => idx = next_idx,
                LocalEchoStep::Incomplete => {
                    let incomplete = &bytes[idx..];
                    if incomplete.len() <= MAX_LOCAL_ECHO_PENDING_BYTES {
                        self.pending.extend_from_slice(incomplete);
                    }
                    break;
                }
            }
        }

        output
    }
}

enum LocalEchoStep {
    Echo(u8),
    Backspace,
    Newline,
    Skip(usize),
    Incomplete,
}

fn local_echo_step(input: &[u8], idx: usize) -> LocalEchoStep {
    match input[idx] {
        0x08 | 0x7f => LocalEchoStep::Backspace,
        b'\r' | b'\n' => LocalEchoStep::Newline,
        0x1b => local_echo_escape_step(input, idx),
        byte if byte.is_ascii_control() => LocalEchoStep::Skip(idx + 1),
        byte => LocalEchoStep::Echo(byte),
    }
}

fn local_echo_escape_step(input: &[u8], idx: usize) -> LocalEchoStep {
    let Some(next) = input.get(idx + 1) else {
        return LocalEchoStep::Incomplete;
    };
    if *next != b'[' {
        return LocalEchoStep::Skip(idx + 1);
    }

    let mut end = idx + 2;
    while end < input.len() && !(0x40..=0x7e).contains(&input[end]) {
        end += 1;
    }
    if end >= input.len() {
        return LocalEchoStep::Incomplete;
    }
    LocalEchoStep::Skip(end + 1)
}

struct SessionSignalMask {
    previous: libc::sigset_t,
    active: bool,
}

impl SessionSignalMask {
    fn block() -> io::Result<Self> {
        // SAFETY: sigset_t is a plain C type; sigemptyset below initializes it before it is used.
        let mut blocked = unsafe { mem::zeroed::<libc::sigset_t>() };
        // SAFETY: sigset_t is a plain C type; pthread_sigmask writes it before it is read.
        let mut previous = unsafe { mem::zeroed::<libc::sigset_t>() };
        // SAFETY: `blocked` is a valid, exclusively borrowed sigset_t for both calls.
        unsafe {
            libc::sigemptyset(&mut blocked);
            for signal in TERMINATING_SIGNALS.into_iter().chain(JOB_CONTROL_SIGNALS) {
                libc::sigaddset(&mut blocked, signal);
            }
        }
        // SAFETY: both sets are valid for the call; the returned mask is kept in `previous` and
        // reinstated by `restore`.
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(Self {
            previous,
            active: true,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // SAFETY: `self.previous` was written by pthread_sigmask in `block`; a null old-set pointer
        // is permitted.
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for SessionSignalMask {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Clone, Copy)]
struct RawModeState {
    stdin_fd: RawFd,
    original: libc::termios,
    raw: libc::termios,
    /// Set when the snapshot was taken while another job owned the terminal:
    /// `original` is then that job's mode and is read again once this process
    /// reaches the foreground.
    snapshot_stale: bool,
}

enum RawModeLifecycle {
    Inactive,
    Active(RawModeState),
    Suspended(RawModeState),
    Terminating,
}

impl RawModeLifecycle {
    /// Applies raw attributes only while startup is still active. The caller
    /// holds the lifecycle mutex across both this transition and `apply`, so a
    /// terminating signal cannot restore cooked mode and then lose a race to
    /// a late raw-mode activation.
    /// `apply` returns the state it applied, or `None` when the process is a
    /// background job; raw mode then waits for the resume that brings it to
    /// the foreground.
    fn activate_with(
        &mut self,
        terminal: RawModeState,
        apply: impl FnOnce(&RawModeState) -> io::Result<Option<RawModeState>>,
    ) -> io::Result<bool> {
        match self {
            Self::Inactive => {
                *self = match apply(&terminal)? {
                    Some(active) => Self::Active(active),
                    None => Self::Suspended(RawModeState {
                        snapshot_stale: true,
                        ..terminal
                    }),
                };
                Ok(true)
            }
            Self::Terminating => Ok(false),
            Self::Active(_) | Self::Suspended(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "raw terminal mode is already active",
            )),
        }
    }

    /// Marks termination before restoring attributes. `restore` runs while
    /// the lifecycle mutex is held, preventing activation or job-control
    /// reapplication from racing after cooked mode has been restored.
    fn begin_termination_with(
        &mut self,
        restore: impl FnOnce(Option<&RawModeState>) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            Self::Active(terminal) => {
                restore(Some(terminal))?;
                *self = Self::Terminating;
            }
            Self::Inactive | Self::Suspended(_) | Self::Terminating => {
                restore(None)?;
                *self = Self::Terminating;
            }
        }
        Ok(())
    }

    fn suspend_with(
        &mut self,
        restore: impl FnOnce(&RawModeState) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Self::Active(terminal) = *self {
            restore(&terminal)?;
            *self = Self::Suspended(terminal);
        }
        Ok(())
    }

    /// Applies raw mode again once the process has been continued. A stop by
    /// SIGSTOP cannot be caught and skips `suspend_with`, so the state can
    /// still be active while the job-control shell has put its own mode back
    /// on the terminal. A continued background job only gives up raw mode.
    fn reassert_with(
        &mut self,
        apply: impl FnOnce(&RawModeState) -> io::Result<Option<RawModeState>>,
    ) -> io::Result<()> {
        match *self {
            Self::Active(terminal) => {
                if apply(&terminal)?.is_none() {
                    *self = Self::Suspended(terminal);
                }
                Ok(())
            }
            Self::Suspended(_) => self.resume_with(apply),
            Self::Inactive | Self::Terminating => Ok(()),
        }
    }

    /// Stays suspended when `apply` returns `None`, as for `activate_with`.
    fn resume_with(
        &mut self,
        apply: impl FnOnce(&RawModeState) -> io::Result<Option<RawModeState>>,
    ) -> io::Result<()> {
        if let Self::Suspended(terminal) = *self {
            if let Some(active) = apply(&terminal)? {
                *self = Self::Active(active);
            }
        }
        Ok(())
    }
}

type SharedRawModeState = Arc<Mutex<RawModeLifecycle>>;

struct RawModeGuard {
    state: SharedRawModeState,
}

impl RawModeGuard {
    fn enable(state: SharedRawModeState) -> Result<Self, CliError> {
        let stdin = io::stdin();
        let terminal = raw_mode_state_for(stdin.as_fd().as_raw_fd())?;
        let activated = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .activate_with(terminal, apply_raw_mode_in_foreground)?;
        if !activated {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminal session terminated during raw-mode startup",
            )
            .into());
        }
        Ok(Self { state })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(error) = terminate_raw_mode_from_signal_state(&self.state) {
            warn(format_args!(
                "prismtty: could not restore terminal during cleanup: {error}"
            ));
        }
    }
}

extern "C" fn restore_raw_mode_for_signal(signal: libc::c_int) {
    RAW_MODE_SIGNAL_PIPE.notify(signal as u8);
}

fn terminate_raw_mode_from_signal_state(state: &Mutex<RawModeLifecycle>) -> io::Result<()> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .begin_termination_with(|active| {
            if let Some(active) = active {
                restore_cooked_mode_in_foreground(active)?;
            }
            Ok(())
        })
}

fn suspend_raw_mode_from_signal_state(state: &Mutex<RawModeLifecycle>) -> io::Result<()> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .suspend_with(restore_cooked_mode_in_foreground)
}

/// Restores the mode saved before raw mode unless this process is a
/// background job, which leaves the mode to the job that owns the terminal;
/// restoring would overwrite that job's settings. An uncatchable SIGSTOP
/// leaves the state active while the shell takes the terminal back, so this
/// also guards a termination that arrives before the continue.
fn restore_cooked_mode_in_foreground(active: &RawModeState) -> io::Result<()> {
    if owns_terminal(active.stdin_fd) == Some(false) {
        return Ok(());
    }
    restore_terminal_attrs(active.stdin_fd, &active.original)
}

fn reapply_raw_mode_from_signal_state(state: &Mutex<RawModeLifecycle>) -> io::Result<()> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resume_with(apply_raw_mode_in_foreground)
}

fn reassert_raw_mode_from_signal_state(state: &Mutex<RawModeLifecycle>) -> io::Result<()> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reassert_with(apply_raw_mode_in_foreground)
}

fn raw_mode_state_for(stdin_fd: RawFd) -> io::Result<RawModeState> {
    let original = terminal_attrs(stdin_fd)?;
    let mut raw = original;
    // SAFETY: `raw` is a valid termios copied from tcgetattr; cfmakeraw only rewrites its
    // fields.
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    Ok(RawModeState {
        stdin_fd,
        original,
        raw,
        snapshot_stale: false,
    })
}

/// Applies raw mode unless this process is a background job, which returns
/// `None` and leaves the terminal to its foreground job. The signal masks in
/// force here hold SIGTTOU, so the kernel would not refuse the write itself.
fn apply_raw_mode_in_foreground(terminal: &RawModeState) -> io::Result<Option<RawModeState>> {
    if owns_terminal(terminal.stdin_fd) == Some(false) {
        return Ok(None);
    }
    let terminal = if terminal.snapshot_stale {
        raw_mode_state_for(terminal.stdin_fd)?
    } else {
        *terminal
    };
    set_terminal_attrs_retrying_eintr(terminal.stdin_fd, &terminal.raw)?;
    Ok(Some(terminal))
}

fn set_terminal_attrs_retrying_eintr(fd: RawFd, attrs: &libc::termios) -> io::Result<()> {
    loop {
        // SAFETY: `attrs` is a valid termios and `fd` is a terminal descriptor the caller keeps
        // open for the call.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, attrs) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Restores cooked attributes while blocking SIGTTOU in the calling thread.
/// A background wrapper must be able to restore its terminal before stopping;
/// otherwise `tcsetattr` recursively queues SIGTTOU and never reaches SIGSTOP.
fn restore_terminal_attrs(fd: RawFd, attrs: &libc::termios) -> io::Result<()> {
    // SAFETY: sigset_t is a plain C type; sigemptyset below initializes it before it is used.
    let mut blocked = unsafe { mem::zeroed::<libc::sigset_t>() };
    // SAFETY: sigset_t is a plain C type; pthread_sigmask writes it before it is read.
    let mut previous = unsafe { mem::zeroed::<libc::sigset_t>() };
    // SAFETY: `blocked` is a valid, exclusively borrowed sigset_t for both calls.
    unsafe {
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
    }
    // SAFETY: both sets are valid for the call; the previous mask is reinstated below.
    let block_result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) };
    if block_result != 0 {
        return Err(io::Error::from_raw_os_error(block_result));
    }
    let result = set_terminal_attrs_retrying_eintr(fd, attrs);
    let restore_result =
        // SAFETY: `previous` was written by the pthread_sigmask call above; a null old-set pointer
        // is permitted.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
    if result.is_ok() && restore_result != 0 {
        return Err(io::Error::from_raw_os_error(restore_result));
    }
    result
}

fn terminal_attrs(fd: RawFd) -> io::Result<libc::termios> {
    // SAFETY: termios is a plain C struct; tcgetattr fills it before it is read.
    let mut attrs = unsafe { mem::zeroed::<libc::termios>() };
    // SAFETY: `attrs` is a valid, exclusively borrowed termios for the call.
    if unsafe { libc::tcgetattr(fd, &mut attrs) } == 0 {
        Ok(attrs)
    } else {
        Err(io::Error::last_os_error())
    }
}

struct SessionSignalWatcher {
    write_fd: RawFd,
    thread: Option<thread::JoinHandle<()>>,
    previous_signal_handlers: Vec<(libc::c_int, libc::sigaction)>,
    raw_mode_state: SharedRawModeState,
}

impl SessionSignalWatcher {
    fn start(raw_mode_state: SharedRawModeState) -> io::Result<Self> {
        ensure_child_is_reapable()?;
        let (read_fd, write_fd) = pipe_fds()?;
        if let Err(error) = set_fd_nonblocking(write_fd) {
            close_fd(read_fd);
            close_fd(write_fd);
            return Err(error);
        }
        RAW_MODE_SIGNAL_PIPE.arm(write_fd);
        let mut previous_signal_handlers = Vec::new();
        for signal in TERMINATING_SIGNALS
            .into_iter()
            .chain(JOB_CONTROL_SIGNALS)
            .chain([libc::SIGCONT])
        {
            if signal_is_ignored(signal) {
                // Keep a disposition the caller chose (`nohup`, or `cmd &` in a
                // non-interactive shell) instead of acting on the signal.
                continue;
            }
            match install_signal_handler(signal, restore_raw_mode_for_signal, libc::SA_RESTART) {
                Ok(previous) => previous_signal_handlers.push((signal, previous)),
                Err(error) => {
                    restore_signal_handlers(&previous_signal_handlers);
                    close_fd(read_fd);
                    if RAW_MODE_SIGNAL_PIPE.disarm() {
                        close_fd(write_fd);
                    }
                    return Err(error);
                }
            }
        }
        let thread_state = Arc::clone(&raw_mode_state);
        let thread = spawn_supervised("signal handler", move || {
            handle_session_signals(read_fd, thread_state)
        });
        Ok(Self {
            write_fd,
            thread: Some(thread),
            previous_signal_handlers,
            raw_mode_state,
        })
    }

    fn stop(&mut self) {
        if let Err(error) = terminate_raw_mode_from_signal_state(&self.raw_mode_state) {
            warn(format_args!(
                "prismtty: could not restore terminal during cleanup: {error}"
            ));
        }
        restore_signal_handlers(&self.previous_signal_handlers);
        self.previous_signal_handlers.clear();
        // Handlers are restored, so no new one starts; wait out any that is
        // mid-write before the descriptor is closed (or leak it if one stalls).
        let closable = RAW_MODE_SIGNAL_PIPE.disarm();
        if self.write_fd >= 0 {
            write_signal_byte(self.write_fd, RAW_SIGNAL_STOP_BYTE);
            if closable {
                close_fd(self.write_fd);
            }
            self.write_fd = -1;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SessionSignalWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Owns signal supervision and raw mode as one ordered lifecycle. The signal
/// watcher is installed before raw mode is entered, and is always stopped and
/// joined before the shared termios state is released.
struct TerminalSession {
    signal_watcher: Option<SessionSignalWatcher>,
    raw_mode: Option<RawModeGuard>,
}

impl TerminalSession {
    fn start(interactive: bool) -> Result<Self, CliError> {
        let raw_mode_state = Arc::new(Mutex::new(RawModeLifecycle::Inactive));
        let signal_watcher = SessionSignalWatcher::start(Arc::clone(&raw_mode_state))?;
        let raw_mode = if interactive {
            Some(RawModeGuard::enable(raw_mode_state)?)
        } else {
            None
        };
        Ok(Self {
            signal_watcher: Some(signal_watcher),
            raw_mode,
        })
    }

    fn stop(&mut self) {
        if let Some(mut signal_watcher) = self.signal_watcher.take() {
            signal_watcher.stop();
        }
        drop(self.raw_mode.take());
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_session_signals(read_fd: RawFd, raw_mode_state: SharedRawModeState) {
    handle_session_signals_with(read_fd, &raw_mode_state, || {
        with_terminal_restored_for_job_control(
            || suspend_raw_mode_from_signal_state(&raw_mode_state),
            || {
                forward_signal_to_child(libc::SIGSTOP as u8);
                // SAFETY: raise has no memory-safety preconditions; SIGSTOP cannot be
                // caught and raw mode was restored just before.
                unsafe {
                    // Target the calling thread so Linux delivers the
                    // stop before this worker can continue and reapply
                    // raw mode. A process-directed kill may return
                    // while SIGSTOP is still pending.
                    libc::raise(libc::SIGSTOP);
                }
                forward_signal_to_child(libc::SIGCONT as u8);
            },
            || reapply_raw_mode_from_signal_state(&raw_mode_state),
        )
    });
}

/// Acts on the signal numbers the handlers queue in the self-pipe until the
/// stop byte arrives. `stop_for_job_control` performs one supervised stop and
/// returns once the wrapper has been continued.
fn handle_session_signals_with(
    read_fd: RawFd,
    raw_mode_state: &Mutex<RawModeLifecycle>,
    mut stop_for_job_control: impl FnMut() -> io::Result<()>,
) {
    let mut pending = VecDeque::new();
    let mut resumed_at = None;
    while let Some(bytes) = read_signal_bytes(read_fd) {
        pending.extend(bytes);
        while let Some(signal) = pending.pop_front() {
            if signal == RAW_SIGNAL_STOP_BYTE {
                close_fd(read_fd);
                return;
            }
            let signal = libc::c_int::from(signal);
            if TERMINATING_SIGNALS.contains(&signal) {
                if let Err(error) = terminate_raw_mode_from_signal_state(raw_mode_state) {
                    warn(format_args!(
                        "prismtty: could not restore terminal before exit: {error}"
                    ));
                }
                terminate_and_reap_forwarded_child(signal);
                // This runs on the watcher thread, not in the signal handler.
                unregister_before_exit();
                // SAFETY: _exit never returns and skips destructors by design: the terminal was
                // restored, the child reaped and the pid file removed above.
                unsafe {
                    libc::_exit(128 + signal);
                }
            }
            if signal == libc::SIGCONT {
                if let Err(error) = reassert_raw_mode_from_signal_state(raw_mode_state) {
                    warn(format_args!(
                        "prismtty: could not reapply raw terminal mode: {error}"
                    ));
                }
                RESIZE_SIGNAL_PIPE.notify(RESIZE_WAKE_BYTE);
                continue;
            }
            if JOB_CONTROL_SIGNALS.contains(&signal) {
                if is_stale_stop_request(signal, resumed_at, || {
                    owns_terminal(libc::STDIN_FILENO) == Some(true)
                }) {
                    continue;
                }
                if let Err(error) = stop_for_job_control() {
                    warn(format_args!(
                        "prismtty: job-control terminal transition failed: {error}"
                    ));
                }
                // A resize while stopped signalled the shell, which owned the
                // terminal then, so check the size again.
                RESIZE_SIGNAL_PIPE.notify(RESIZE_WAKE_BYTE);
                // Stop requests queued before the resume were satisfied by
                // this stop, just as SIGCONT discards pending stop signals.
                // Replaying them would stop the wrapper again right after
                // `fg`: a background read of the terminal raises SIGTTIN
                // each time it restarts, queueing a burst of them.
                pending.extend(ready_signal_bytes(read_fd));
                pending.retain(|byte| !JOB_CONTROL_SIGNALS.contains(&libc::c_int::from(*byte)));
                resumed_at = Some(Instant::now());
            }
        }
    }
    close_fd(read_fd);
}

/// How long after a job-control resume a SIGTTIN or SIGTTOU that reaches the
/// watcher while the wrapper is in the foreground is still taken as stale.
const STALE_STOP_WINDOW: Duration = Duration::from_secs(1);

/// The kernel raises SIGTTIN and SIGTTOU only for terminal I/O from the
/// background. One that reaches the watcher in the foreground shortly after a
/// resume was queued by a handler the stop interrupted mid-way, which only
/// finishes once the wrapper is continued; it is not a new request.
fn is_stale_stop_request(
    signal: libc::c_int,
    resumed_at: Option<Instant>,
    in_foreground: impl FnOnce() -> bool,
) -> bool {
    signal != libc::SIGTSTP
        && resumed_at.is_some_and(|at| at.elapsed() < STALE_STOP_WINDOW)
        && in_foreground()
}

/// Whether this process group is the foreground job of the terminal on `fd`,
/// or `None` when that cannot be told: `fd` is not this process's controlling
/// terminal, or the terminal has no foreground job.
fn owns_terminal(fd: RawFd) -> Option<bool> {
    // SAFETY: tcgetpgrp and getpgrp have no memory-safety preconditions.
    let (foreground, own) = unsafe { (libc::tcgetpgrp(fd), libc::getpgrp()) };
    (foreground > 0).then_some(foreground == own)
}

fn current_pty_size() -> PtySize {
    let stdout = io::stdout();
    if let Some(size) = pty_size_from_fd(stdout.as_fd()) {
        return size;
    }

    let stdin = io::stdin();
    fallback_pty_size(pty_size_from_fd(stdin.as_fd()))
}

fn fallback_pty_size(size: Option<PtySize>) -> PtySize {
    size.unwrap_or_default()
}

fn pty_size_from_fd(fd: BorrowedFd<'_>) -> Option<PtySize> {
    // SAFETY: winsize is a plain C struct; the ioctl fills it before it is read.
    let mut winsize: libc::winsize = unsafe { mem::zeroed() };
    // SAFETY: TIOCGWINSZ takes a pointer to a winsize; `winsize` is valid and exclusively borrowed
    // and `fd` is a live borrowed descriptor.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut winsize) };
    if result != 0 || winsize.ws_row == 0 || winsize.ws_col == 0 {
        return None;
    }

    Some(PtySize {
        rows: winsize.ws_row,
        cols: winsize.ws_col,
        pixel_width: winsize.ws_xpixel,
        pixel_height: winsize.ws_ypixel,
    })
}

struct PtyResizeWatcher {
    write_fd: RawFd,
    thread: Option<thread::JoinHandle<()>>,
    previous_signal_handler: libc::sigaction,
}

impl PtyResizeWatcher {
    fn start(master: Box<dyn portable_pty::MasterPty + Send>) -> io::Result<Self> {
        let (read_fd, write_fd) = pipe_fds()?;
        if let Err(error) = set_fd_nonblocking(write_fd) {
            close_fd(read_fd);
            close_fd(write_fd);
            return Err(error);
        }
        RESIZE_SIGNAL_PIPE.arm(write_fd);
        let previous_signal_handler = match install_signal_handler(
            libc::SIGWINCH,
            notify_resize_for_signal,
            libc::SA_RESTART,
        ) {
            Ok(previous) => previous,
            Err(error) => {
                close_fd(read_fd);
                if RESIZE_SIGNAL_PIPE.disarm() {
                    close_fd(write_fd);
                }
                return Err(error);
            }
        };
        let thread = spawn_supervised("resize watcher", move || {
            resize_pty_on_signals(master, read_fd)
        });
        Ok(Self {
            write_fd,
            thread: Some(thread),
            previous_signal_handler,
        })
    }

    fn stop(&mut self) {
        restore_signal_handler(libc::SIGWINCH, &self.previous_signal_handler);
        // Handler restored, so no new one starts; wait out any that is
        // mid-write before the descriptor is closed (or leak it if one stalls).
        let closable = RESIZE_SIGNAL_PIPE.disarm();
        if self.write_fd >= 0 {
            write_signal_byte(self.write_fd, RESIZE_STOP_BYTE);
            if closable {
                close_fd(self.write_fd);
            }
            self.write_fd = -1;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PtyResizeWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

extern "C" fn notify_resize_for_signal(_signal: libc::c_int) {
    RESIZE_SIGNAL_PIPE.notify(RESIZE_WAKE_BYTE);
}

fn resize_pty_on_signals(master: Box<dyn portable_pty::MasterPty + Send>, read_fd: RawFd) {
    let mut last_size = start_pty_size_sync(&*master, current_pty_size());
    while let Some(bytes) = read_signal_bytes(read_fd) {
        if bytes.contains(&RESIZE_STOP_BYTE) {
            break;
        }
        last_size = sync_pty_size(&*master, last_size, current_pty_size());
    }
    close_fd(read_fd);
}

/// Compares against the PTY's own size rather than the outer terminal's: a
/// resize made after the PTY was opened, before the SIGWINCH handler was
/// installed, would otherwise never reach it.
fn start_pty_size_sync(master: &dyn portable_pty::MasterPty, outer_size: PtySize) -> PtySize {
    let pty_size = master.get_size().unwrap_or_default();
    sync_pty_size(master, pty_size, outer_size)
}

/// Gives the PTY the outer terminal's size when it differs from the size the
/// PTY was last given, and returns the size it was given now.
fn sync_pty_size(
    master: &dyn portable_pty::MasterPty,
    last_size: PtySize,
    outer_size: PtySize,
) -> PtySize {
    if outer_size != last_size {
        let _ = master.resize(outer_size);
    }
    outer_size
}

/// Creates a close-on-exec pipe. The signal self-pipe exists before the
/// wrapped command is spawned, and portable-pty's /dev/fd scan, the only other
/// thing that keeps it out of the child, does nothing when /proc is missing.
/// macOS has no pipe2, so the flag is set afterwards; the only fork is the
/// spawn on this same thread, so no child can inherit the ends in between.
fn pipe_fds() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` is a valid array of two c_int that pipe fills on success.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        if let Err(error) = set_fd_cloexec(fd) {
            close_fd(fds[0]);
            close_fd(fds[1]);
            return Err(error);
        }
    }
    Ok((fds[0], fds[1]))
}

fn set_fd_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFD takes no pointer argument; an invalid `fd` reports EBADF instead of touching
    // memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFD takes an integer flags argument.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_fd_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL takes no pointer argument; an invalid `fd` reports EBADF instead of touching
    // memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL takes an integer flags argument.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_signal_bytes(read_fd: RawFd) -> Option<Vec<u8>> {
    let mut buffer = [0_u8; 64];
    loop {
        // SAFETY: `buffer` is valid for writes of `buffer.len()` bytes for the call and `read_fd`
        // is our own pipe end.
        let read = unsafe {
            libc::read(
                read_fd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        if read > 0 {
            return Some(buffer[..read as usize].to_vec());
        }
        if read == 0 {
            return None;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
}

/// Reads whatever is already queued in the signal pipe without waiting. The
/// pass limit keeps a burst that is still being written from pinning the
/// thread.
fn ready_signal_bytes(read_fd: RawFd) -> Vec<u8> {
    let mut bytes = Vec::new();
    for _ in 0..1024 {
        let mut poll_fd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll_fd` is a valid, exclusively borrowed pollfd and the count matches.
        if unsafe { libc::poll(&mut poll_fd, 1, 0) } <= 0 || poll_fd.revents & libc::POLLIN == 0 {
            break;
        }
        match read_signal_bytes(read_fd) {
            Some(read) => bytes.extend(read),
            None => break,
        }
    }
    bytes
}

fn write_signal_byte(write_fd: RawFd, byte: u8) {
    let bytes = [byte];
    // SAFETY: `bytes` is valid for reads of `bytes.len()`; write is async-signal-safe and
    // `write_fd` is our nonblocking pipe end.
    unsafe {
        libc::write(write_fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
    }
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: `fd` is a descriptor this module opened and owns, and it is not used after this
        // call.
        unsafe {
            libc::close(fd);
        }
    }
}

/// Puts SIGCHLD back to its default disposition when it was inherited as
/// ignored (or with SA_NOCLDWAIT). Either makes the kernel reap the wrapped
/// child itself, so waiting for it fails with ECHILD and its exit status is
/// lost. prismtty never handles SIGCHLD, and portable-pty already resets it
/// for the child, so nothing else sees the change.
fn ensure_child_is_reapable() -> io::Result<()> {
    // SAFETY: sigaction is a plain C struct; the sigaction call fills it before it is read.
    let mut current = unsafe { mem::zeroed::<libc::sigaction>() };
    // SAFETY: a null new-action pointer only queries the disposition; `current` is valid for the
    // call.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut current) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if current.sa_sigaction != libc::SIG_IGN && current.sa_flags & libc::SA_NOCLDWAIT == 0 {
        return Ok(());
    }
    // SAFETY: sigaction is a plain C struct; all-zero is SIG_DFL with no flags, and the mask is
    // initialized below.
    let mut default = unsafe { mem::zeroed::<libc::sigaction>() };
    default.sa_sigaction = libc::SIG_DFL;
    // SAFETY: `default.sa_mask` is a valid, exclusively borrowed sigset_t.
    unsafe {
        libc::sigemptyset(&mut default.sa_mask);
    }
    // SAFETY: `default` is a valid action for the call; a null old-action pointer is permitted.
    if unsafe { libc::sigaction(libc::SIGCHLD, &default, std::ptr::null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// True when `signal` is currently ignored, for example inherited as ignored
/// from `nohup`.
fn signal_is_ignored(signal: libc::c_int) -> bool {
    // SAFETY: sigaction is a plain C struct; the sigaction call fills it before it is read.
    let mut current = unsafe { mem::zeroed::<libc::sigaction>() };
    // SAFETY: a null new-action pointer only queries the disposition; `current` is valid for the
    // call.
    let queried = unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) } == 0;
    queried && current.sa_sigaction == libc::SIG_IGN
}

fn install_signal_handler(
    signal: libc::c_int,
    handler: extern "C" fn(libc::c_int),
    flags: libc::c_int,
) -> io::Result<libc::sigaction> {
    // SAFETY: sigaction is a plain C struct; the fields used are set below and the rest stay zero.
    let mut action = unsafe { mem::zeroed::<libc::sigaction>() };
    // SAFETY: sigaction is a plain C struct; the sigaction call fills it before it is returned.
    let mut previous = unsafe { mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = handler as libc::sighandler_t;
    action.sa_flags = flags;
    // SAFETY: `action.sa_mask` is a valid, exclusively borrowed sigset_t.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    // SAFETY: `handler` is an `extern "C" fn(c_int)` that lives for the whole process; `action` and
    // `previous` are valid for the call.
    if unsafe { libc::sigaction(signal, &action, &mut previous) } == 0 {
        Ok(previous)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn restore_signal_handlers(handlers: &[(libc::c_int, libc::sigaction)]) {
    for (signal, previous) in handlers {
        restore_signal_handler(*signal, previous);
    }
}

fn restore_signal_handler(signal: libc::c_int, previous: &libc::sigaction) {
    // SAFETY: `previous` was returned by sigaction in `install_signal_handler`; a null old-action
    // pointer is permitted.
    unsafe {
        libc::sigaction(signal, previous, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::fd::RawFd;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};

    use nix::libc;
    use nix::sys::termios::{InputFlags, LocalFlags, OutputFlags};

    #[test]
    fn stdin_reader_waits_through_would_block_instead_of_failing() {
        use std::io::{Read, Write};
        use std::os::fd::FromRawFd;

        let (read_fd, write_fd) = super::pipe_fds().expect("pipe");
        super::set_fd_nonblocking(read_fd).expect("nonblocking read end");
        // SAFETY: both descriptors were just created by pipe and are owned only here.
        let (reader, mut writer) = unsafe {
            (
                std::fs::File::from_raw_fd(read_fd),
                std::fs::File::from_raw_fd(write_fd),
            )
        };
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            writer.write_all(b"hello").expect("write pipe");
        });
        let mut stdin = super::WaitWhenWouldBlock {
            inner: reader,
            fd: read_fd,
        };
        let mut buffer = [0_u8; 16];
        let read = stdin.read(&mut buffer).expect("read waits for data");
        assert_eq!(&buffer[..read], b"hello");
        feeder.join().expect("feeder");
        assert_eq!(
            stdin
                .read(&mut buffer)
                .expect("end of input once the writer closes"),
            0
        );
    }

    // stdout shares the terminal's file description with stdin, so it can be
    // non-blocking too, and a burst of output then fills the terminal's queue.
    #[test]
    fn stdout_writer_waits_through_would_block_instead_of_failing() {
        use std::io::{Read, Write};
        use std::os::fd::FromRawFd;

        let (read_fd, write_fd) = super::pipe_fds().expect("pipe");
        super::set_fd_nonblocking(write_fd).expect("nonblocking write end");
        // SAFETY: both descriptors were just created by pipe and are owned only here.
        let (mut reader, writer) = unsafe {
            (
                std::fs::File::from_raw_fd(read_fd),
                std::fs::File::from_raw_fd(write_fd),
            )
        };
        let mut stdout = super::WaitWhenWouldBlock {
            inner: writer,
            fd: write_fd,
        };
        let mut queued = 0;
        loop {
            match stdout.inner.write(&[b'x'; 4096]) {
                Ok(written) => queued += written,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill pipe: {error}"),
            }
        }
        let drainer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let mut drained = Vec::new();
            reader.read_to_end(&mut drained).expect("drain pipe");
            drained
        });
        stdout.write_all(b"hello").expect("write waits for room");
        stdout.flush().expect("flush");
        drop(stdout);
        let drained = drainer.join().expect("drainer");
        assert_eq!(drained.len(), queued + 5);
        assert!(drained.ends_with(b"hello"));
    }

    #[test]
    fn supervised_run_catches_a_panicking_body() {
        assert!(super::run_supervised("test-thread", || {}));
        assert!(!super::run_supervised("test-thread", || panic!(
            "intentional test panic"
        )));
    }

    // AUDIT L15: a child killed by a signal must report 128+signal, not a
    // misleading generic code (portable-pty's ExitStatus reports 1 and drops
    // the signal number, so we reap with waitpid and map the raw status).
    #[test]
    fn exit_code_from_wait_maps_signal_deaths_to_128_plus_signal() {
        use nix::sys::signal::Signal;
        use nix::sys::wait::WaitStatus;
        use nix::unistd::Pid;

        let pid = Pid::from_raw(1);
        assert_eq!(super::exit_code_from_wait(WaitStatus::Exited(pid, 0)), 0);
        assert_eq!(super::exit_code_from_wait(WaitStatus::Exited(pid, 42)), 42);
        assert_eq!(
            super::exit_code_from_wait(WaitStatus::Signaled(pid, Signal::SIGTERM, false)),
            143
        );
        assert_eq!(
            super::exit_code_from_wait(WaitStatus::Signaled(pid, Signal::SIGSEGV, false)),
            139
        );
    }

    // AUDIT G5: exit codes above 255 must clamp, not wrap to a misleading 0.
    #[test]
    fn exit_code_byte_clamps_codes_above_255() {
        assert_eq!(super::exit_code_byte(0), 0);
        assert_eq!(super::exit_code_byte(1), 1);
        assert_eq!(super::exit_code_byte(255), 255);
        assert_eq!(super::exit_code_byte(256), 255);
        assert_eq!(super::exit_code_byte(u32::MAX), 255);
    }

    #[test]
    fn relative_program_paths_with_a_directory_resolve_against_the_cwd() {
        use std::ffi::OsString;

        assert_eq!(
            super::cwd_relative_program(OsString::from("bin/tool")),
            OsString::from("./bin/tool")
        );
        for unchanged in ["tool", "./tool", "../bin/tool", "/usr/bin/tool", ""] {
            assert_eq!(
                super::cwd_relative_program(OsString::from(unchanged)),
                OsString::from(unchanged)
            );
        }
    }

    #[test]
    fn spawn_failures_map_to_shell_exit_codes() {
        use std::ffi::OsStr;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp directory");
        let script = dir.path().join("script.sh");
        std::fs::write(&script, "#!/bin/sh\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644))
            .expect("script is not executable");
        let cwd = Some(dir.path());

        assert_eq!(super::spawn_failure_exit_code(OsStr::new(""), cwd), 127);
        assert_eq!(
            super::spawn_failure_exit_code(OsStr::new("./missing"), cwd),
            127
        );
        assert_eq!(
            super::spawn_failure_exit_code(OsStr::new("./script.sh"), cwd),
            126
        );
        assert_eq!(super::spawn_failure_exit_code(OsStr::new("./"), cwd), 126);
        // A program that resolves to an executable failed for another reason.
        assert_eq!(
            super::spawn_failure_exit_code(OsStr::new("/bin/sh"), cwd),
            1
        );
    }

    // The kernel ends a `#!` interpreter name only at a blank or the line end,
    // so a CRLF script names `/bin/sh\r`, which does not exist.
    #[test]
    fn scripts_with_unusable_interpreters_map_to_shell_exit_codes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp directory");
        let write = |name: &str, body: &str, mode: u32| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).expect("write file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("set mode");
            path
        };
        write("crlf.sh", "#!/bin/sh\r\necho ran\r\n", 0o755);
        write("missing.sh", "#!/nonexistent/interp -x\necho ran\n", 0o755);
        let interp = write("interp", "#!/bin/sh\n", 0o644);
        write(
            "noexec.sh",
            &format!("#!{}\necho ran\n", interp.display()),
            0o755,
        );
        write("dir.sh", "#!/\necho ran\n", 0o755);
        write("ok.sh", "#! \t/bin/sh -e\necho ran\n", 0o755);
        write("plain.sh", "echo ran\n", 0o755);
        write("unterminated.sh", &format!("#!/{}", "a".repeat(300)), 0o755);
        write("short.sh", "#!/nonexistent/interp", 0o755);
        write("hash.sh", "#!/bin/sh#x\necho ran\n", 0o755);
        let cwd = Some(dir.path());

        let code =
            |program: &str| match super::unusable_script_interpreter(OsStr::new(program), cwd) {
                Some(super::CliError::Spawn { code, source }) => {
                    assert!(
                        source.to_string().contains("bad interpreter"),
                        "{program}: {source}"
                    );
                    Some(code)
                }
                Some(other) => panic!("{program}: unexpected error {other}"),
                None => None,
            };
        assert_eq!(code("./crlf.sh"), Some(127));
        assert_eq!(code("./missing.sh"), Some(127));
        assert_eq!(code("./noexec.sh"), Some(126));
        assert_eq!(code("./dir.sh"), Some(126));
        assert_eq!(code("./ok.sh"), None);
        assert_eq!(code("./plain.sh"), None);
        // A first line with no end in the bytes the kernel reads is left to
        // execve, which reports ENOEXEC and lets execvp try /bin/sh.
        assert_eq!(code("./unterminated.sh"), None);
        // Linux ends the line of a short file with no newline at its end, and
        // `#` is part of the name. macOS ends the line at `#`, and reports
        // ENOEXEC for a line with no end at all.
        #[cfg(target_os = "linux")]
        {
            assert_eq!(code("./short.sh"), Some(127));
            assert_eq!(code("./hash.sh"), Some(127));
        }
        #[cfg(target_vendor = "apple")]
        {
            assert_eq!(code("./short.sh"), None);
            assert_eq!(code("./hash.sh"), None);
        }
        // Nothing to check: spawn reports these itself.
        assert_eq!(code("./absent.sh"), None);
        assert_eq!(code("./interp"), None);
        let fifo = std::ffi::CString::new(dir.path().join("fifo").into_os_string().into_vec())
            .expect("FIFO path");
        // SAFETY: `fifo` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o700) }, 0, "mkfifo");
        assert_eq!(code("./fifo"), None);
    }

    // When the child falls back to $HOME, a relative interpreter is found from
    // there, so it cannot be checked from prismtty's directory.
    #[test]
    fn relative_interpreter_is_not_checked_when_the_child_directory_is_unknown() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp directory");
        let write = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).expect("write script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("script is executable");
            path
        };
        let relative = write("relative.sh", "#!prismtty-no-such-interp\necho ran\n");
        let absolute = write("absolute.sh", "#!/nonexistent/interp\necho ran\n");
        let check = super::unusable_script_interpreter;

        assert!(check(relative.as_os_str(), Some(dir.path())).is_some());
        assert!(check(relative.as_os_str(), None).is_none());
        assert!(check(absolute.as_os_str(), None).is_some());
    }

    fn interpreter(start: &[u8], syntax: &super::ShebangSyntax) -> Option<String> {
        super::script_interpreter(start, syntax).map(|path| path.display().to_string())
    }

    // fs/binfmt_script.c: a 256-byte buffer; the line ends at a newline or a
    // NUL; only a blank or a NUL ends the name; with no line end in the
    // buffer, a name followed by a blank still runs.
    #[test]
    fn linux_shebang_syntax_matches_binfmt_script() {
        let linux = |start: &[u8]| interpreter(start, &super::LINUX_SHEBANG);
        let long = |name_bytes: usize, rest: &str| {
            format!("#!/{}{rest}", "a".repeat(name_bytes - 1)).into_bytes()
        };

        assert_eq!(linux(b"#!/bin/sh\necho ran\n").as_deref(), Some("/bin/sh"));
        assert_eq!(linux(b"#! \t/bin/sh -e\n").as_deref(), Some("/bin/sh"));
        assert_eq!(linux(b"#!/bin/sh\r\n").as_deref(), Some("/bin/sh\r"));
        assert_eq!(linux(b"#!/bin/sh#x\n").as_deref(), Some("/bin/sh#x"));
        assert_eq!(linux(b"#!/bin/sh\0x\n").as_deref(), Some("/bin/sh"));
        // A short file with no newline ends its line at its end.
        assert_eq!(linux(b"#!/missing").as_deref(), Some("/missing"));
        assert_eq!(linux(b"#!/missing  ").as_deref(), Some("/missing"));
        // No line end in the buffer: the blank after the name must be in it.
        let name = format!("/{}", "a".repeat(252));
        assert_eq!(linux(&long(253, " -x\n")).as_deref(), Some(name.as_str()));
        assert_eq!(linux(&long(254, " -x\n")), None);
        assert_eq!(linux(&long(300, "\n")), None);
        assert_eq!(
            linux(format!("#!/missing {}", "a".repeat(300)).as_bytes()).as_deref(),
            Some("/missing")
        );
        // No name, or not a script: nothing to check.
        assert_eq!(linux(b"#!"), None);
        assert_eq!(linux(b"#!\n/bin/sh\n"), None);
        assert_eq!(linux(b"#! \t \n"), None);
        assert_eq!(linux(format!("#!{}", " ".repeat(300)).as_bytes()), None);
        assert_eq!(linux(b"echo ran\n"), None);
        assert_eq!(linux(b"#"), None);
        assert_eq!(linux(b""), None);
    }

    // XNU exec_shell_imgact: a 512-byte buffer; the line ends at a newline or
    // `#`, and a line with no end in the buffer is ENOEXEC, so a short file
    // with no newline runs with /bin/sh through execvp.
    #[test]
    fn xnu_shebang_syntax_matches_exec_shell_imgact() {
        let xnu = |start: &[u8]| interpreter(start, &super::XNU_SHEBANG);

        assert_eq!(xnu(b"#!/bin/sh\necho ran\n").as_deref(), Some("/bin/sh"));
        assert_eq!(xnu(b"#! \t/bin/sh -e\n").as_deref(), Some("/bin/sh"));
        assert_eq!(xnu(b"#!/bin/sh\r\n").as_deref(), Some("/bin/sh\r"));
        assert_eq!(xnu(b"#!/bin/sh#x\n").as_deref(), Some("/bin/sh"));
        assert_eq!(xnu(b"#!/bin/sh -x #y\n").as_deref(), Some("/bin/sh"));
        assert_eq!(xnu(b"#!/bin/sh\0x\n").as_deref(), Some("/bin/sh"));
        assert_eq!(xnu(b"#!/missing"), None);
        assert_eq!(xnu(b"#!/missing\0\0\n").as_deref(), Some("/missing"));
        // The line end must be in the buffer.
        let name = format!("/{}", "a".repeat(508));
        assert_eq!(
            xnu(format!("#!{name}\n").as_bytes()).as_deref(),
            Some(name.as_str())
        );
        assert_eq!(xnu(format!("#!{name}a\n").as_bytes()), None);
        assert_eq!(
            xnu(format!("#!/missing {}", "a".repeat(600)).as_bytes()),
            None
        );
        // No name, or not a script: nothing to check.
        assert_eq!(xnu(b"#!#\n"), None);
        assert_eq!(xnu(b"#! \t#/bin/sh\n"), None);
        assert_eq!(xnu(b"#!\n/bin/sh\n"), None);
        assert_eq!(xnu(b"#!\0/bin/sh\n"), None);
        assert_eq!(xnu(b"echo ran\n"), None);
    }

    #[test]
    fn child_pty_flags_enable_echo_and_canonical_input() {
        let local = LocalFlags::empty();
        let input = InputFlags::empty();
        let output = OutputFlags::empty();

        let (local, input, output) = super::normalize_child_pty_flags(local, input, output);

        assert!(local.contains(LocalFlags::ECHO));
        assert!(local.contains(LocalFlags::ECHOE));
        assert!(local.contains(LocalFlags::ECHOK));
        assert!(local.contains(LocalFlags::ICANON));
        assert!(local.contains(LocalFlags::ISIG));
        assert!(local.contains(LocalFlags::IEXTEN));
        assert!(input.contains(InputFlags::ICRNL));
        assert!(output.contains(OutputFlags::OPOST));
    }

    #[test]
    fn local_echo_bytes_echo_printable_enter_and_backspace() {
        assert_eq!(
            super::local_echo_bytes(b"show\x7f route\r\x1b[A"),
            b"show\x08 \x08 route\r\n"
        );
    }

    #[test]
    fn local_echo_state_buffers_split_escape_sequences() {
        let mut echo = super::LocalEchoState::default();

        assert!(echo.push(b"\x1b").is_empty());
        assert!(echo.push(b"[A").is_empty());
        assert_eq!(echo.push(b"show\r"), b"show\r\n");
    }

    #[test]
    fn local_echo_state_bounds_incomplete_csi_and_recovers() {
        let mut echo = super::LocalEchoState::default();
        let mut incomplete = b"\x1b[".to_vec();
        incomplete.resize(super::MAX_LOCAL_ECHO_PENDING_BYTES, b'1');

        assert!(echo.push(&incomplete).is_empty());
        assert_eq!(
            echo.pending.len(),
            super::MAX_LOCAL_ECHO_PENDING_BYTES,
            "pending CSI is retained up to the documented limit"
        );
        assert!(echo.push(b"1").is_empty());
        assert!(
            echo.pending.is_empty(),
            "the first byte beyond the limit resets the incomplete CSI"
        );
        assert_eq!(
            echo.push(b"show"),
            b"show",
            "printable input after recovery is echoed normally"
        );
    }

    #[test]
    fn local_echo_state_does_not_retain_oversized_marker_free_input() {
        let mut echo = super::LocalEchoState::default();
        let input = vec![b'1'; super::MAX_LOCAL_ECHO_PENDING_BYTES * 8];
        let mut csi = b"\x1b[".to_vec();
        csi.extend_from_slice(&input);

        assert!(echo.push(&csi).is_empty());
        assert!(
            echo.pending.len() <= super::MAX_LOCAL_ECHO_PENDING_BYTES,
            "retained local-echo state must stay bounded"
        );
    }

    #[test]
    fn profile_input_observation_drops_when_queue_is_full() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        tx.try_send(Vec::new())
            .expect("queue accepts the first send up to capacity");
        // _rx is alive and the queue is full, so the production try_send
        // exercises Err(TrySendError::Full), the drop-on-full path H3
        // introduced, rather than Err(Disconnected).

        let mut input = Cursor::new(b"show version\n".to_vec());
        let mut output = Vec::new();
        let trace = super::IoTrace::open(None).expect("trace disabled");
        let recent_input = Mutex::new(Vec::new());

        super::forward_stdin_to_pty(
            &mut input,
            &mut output,
            false,
            trace,
            Some(tx),
            &recent_input,
        )
        .expect("stdin forwards even when profile input queue is full");

        assert_eq!(output, b"show version\n");
        // The line ended with a newline, so recent_input is scoped back to empty
        // (the submitted line is dropped); the forwarding itself is unaffected.
        assert!(
            recent_input.lock().unwrap().is_empty(),
            "a submitted line is not retained in recent_input"
        );
    }

    #[test]
    fn forwarded_input_is_recorded_before_child_can_echo_it() {
        let source = include_str!("pty.rs");
        let function_source = source
            .split("fn forward_stdin_to_pty")
            .nth(1)
            .expect("forwarder exists")
            .split("#[cfg(test)]")
            .next()
            .expect("function precedes tests");

        let record_idx = function_source
            .find("record_recent_input(recent_input, input)")
            .expect("recent input is recorded");
        let write_idx = function_source
            .find("writer.write_all(input)")
            .expect("input is written to child PTY");

        assert!(
            record_idx < write_idx,
            "recent input must be recorded before PTY write so immediate echo can be matched"
        );
    }

    #[test]
    fn dropping_the_stdin_forward_writer_injects_no_input() {
        use std::os::unix::fs::OpenOptionsExt;

        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .expect("openpty");
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(pair.master.tty_name().expect("slave tty name"))
            .expect("open slave tty");

        let writer = super::stdin_forward_writer(&*pair.master).expect("stdin writer");
        drop(writer);

        let mut poll_fd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&slave),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll_fd` is a valid, exclusively borrowed pollfd and the count matches.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 200) };
        assert_eq!(
            ready, 0,
            "ending stdin forwarding typed a line the user never entered"
        );

        let source = include_str!("pty.rs");
        let run_command = source
            .split("pub(super) fn run_command")
            .nth(1)
            .expect("run_command exists")
            .split("fn cwd_relative_program")
            .next()
            .expect("run_command boundary exists");
        assert!(run_command.contains("stdin_forward_writer("));
        assert!(!run_command.contains("take_writer("));
    }

    #[test]
    fn record_recent_input_scopes_to_current_line() {
        // Recording no longer depends on ECHO state; instead the buffer is scoped
        // to the in-progress line, so a submitted or aborted line (and any secret
        // in it) does not linger.
        let recent = Mutex::new(Vec::new());

        // Plain typing accumulates for echo matching.
        super::record_recent_input(&recent, b"show version");
        assert_eq!(recent.lock().unwrap().as_slice(), b"show version");

        // A submitted line (CR) drops; an embedded newline keeps only the tail.
        super::record_recent_input(&recent, b"\r");
        assert!(recent.lock().unwrap().is_empty());
        super::record_recent_input(&recent, b"ab\ncd");
        assert_eq!(recent.lock().unwrap().as_slice(), b"cd");

        // Abort/kill controls drop the in-progress line immediately, so a partial
        // password cannot be retained.
        super::record_recent_input(&recent, b"secret\x03"); // Ctrl-C
        assert!(
            recent.lock().unwrap().is_empty(),
            "Ctrl-C abandons the line, dropping any partial secret"
        );
        super::record_recent_input(&recent, b"secret\x1c"); // Ctrl-\
        assert!(
            recent.lock().unwrap().is_empty(),
            "Ctrl-\\ abandons the line, dropping any partial secret"
        );
        super::record_recent_input(&recent, b"pw\x15retyped"); // Ctrl-U
        assert_eq!(
            recent.lock().unwrap().as_slice(),
            b"retyped",
            "Ctrl-U kills the line, keeping only what is typed after"
        );
    }

    #[test]
    fn raw_mode_registers_cleanup_for_catchable_termination_signals() {
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
            assert!(
                super::TERMINATING_SIGNALS.contains(&signal),
                "missing signal {signal} handler"
            );
        }
        #[cfg(target_os = "linux")]
        for signal in [libc::SIGPWR, libc::SIGIO] {
            assert!(
                super::TERMINATING_SIGNALS.contains(&signal),
                "missing Linux signal {signal} handler"
            );
        }
        #[cfg(all(
            target_os = "linux",
            not(any(
                target_arch = "mips",
                target_arch = "mips32r6",
                target_arch = "mips64",
                target_arch = "mips64r6",
                target_arch = "sparc",
                target_arch = "sparc64"
            ))
        ))]
        assert!(super::TERMINATING_SIGNALS.contains(&libc::SIGSTKFLT));
        assert!(!super::TERMINATING_SIGNALS.contains(&libc::SIGKILL));
        assert!(!super::TERMINATING_SIGNALS.contains(&libc::SIGSTOP));
    }

    #[test]
    fn child_spawn_is_covered_by_blocked_signal_supervision() {
        let source = include_str!("pty.rs");
        let run_command = source
            .split("pub(super) fn run_command")
            .nth(1)
            .expect("run_command exists")
            .split("fn parent_terminal_is_iterm")
            .next()
            .expect("run_command boundary exists");
        let blocked = run_command
            .find("SessionSignalMask::block")
            .expect("session signals are blocked");
        let supervised = run_command
            .find("TerminalSession::start")
            .expect("signal watcher starts");
        let spawned = run_command.find("spawn_command").expect("child is spawned");
        let spawned_with_caller_stdin = run_command
            .find("spawn_with_caller_stdin(")
            .expect("child is spawned without a terminal on stdin");
        let published = run_command
            .find("set_forward_child_pid")
            .expect("child pid is published");
        let unblocked = run_command
            .find("signal_mask.restore")
            .expect("session signals are unblocked");

        assert!(blocked < supervised);
        assert!(supervised < spawned);
        assert!(supervised < spawned_with_caller_stdin);
        assert!(spawned < published);
        assert!(spawned_with_caller_stdin < published);
        assert!(published < unblocked);
    }

    #[test]
    fn termination_that_wins_startup_race_cancels_raw_mode_activation() {
        let state = Arc::new(Mutex::new(super::RawModeLifecycle::Inactive));
        let (termination_marked_tx, termination_marked_rx) = mpsc::sync_channel(0);
        let (finish_termination_tx, finish_termination_rx) = mpsc::sync_channel(0);
        let termination_state = Arc::clone(&state);
        let termination = std::thread::spawn(move || {
            termination_state
                .lock()
                .unwrap()
                .begin_termination_with(|active| {
                    assert!(active.is_none(), "startup had not activated raw mode");
                    termination_marked_tx.send(()).unwrap();
                    finish_termination_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });
        termination_marked_rx.recv().unwrap();

        let applied = Arc::new(AtomicBool::new(false));
        let activation_applied = Arc::clone(&applied);
        let activation_state = Arc::clone(&state);
        let (activation_ready_tx, activation_ready_rx) = mpsc::sync_channel(0);
        let activation = std::thread::spawn(move || {
            let terminal = super::RawModeState {
                stdin_fd: -1,
                // SAFETY: termios is a plain C struct; this state is never applied to a terminal
                // (stdin_fd is -1).
                original: unsafe { std::mem::zeroed() },
                // SAFETY: termios is a plain C struct; this state is never applied to a terminal
                // (stdin_fd is -1).
                raw: unsafe { std::mem::zeroed() },
                snapshot_stale: false,
            };
            activation_ready_tx.send(()).unwrap();
            activation_state
                .lock()
                .unwrap()
                .activate_with(terminal, |terminal| {
                    activation_applied.store(true, Ordering::Release);
                    Ok(Some(*terminal))
                })
                .unwrap()
        });

        activation_ready_rx.recv().unwrap();
        finish_termination_tx.send(()).unwrap();
        termination.join().unwrap();
        assert!(
            !activation.join().unwrap(),
            "late activation was not cancelled"
        );
        assert!(
            !applied.load(Ordering::Acquire),
            "raw termios was applied after termination began"
        );
    }

    #[test]
    fn failed_terminal_restore_keeps_original_state_for_cleanup_retry() {
        let terminal = super::RawModeState {
            stdin_fd: -1,
            // SAFETY: termios is a plain C struct; this state is never applied to a terminal
            // (stdin_fd is -1).
            original: unsafe { std::mem::zeroed() },
            // SAFETY: termios is a plain C struct; this state is never applied to a terminal
            // (stdin_fd is -1).
            raw: unsafe { std::mem::zeroed() },
            snapshot_stale: false,
        };
        let mut lifecycle = super::RawModeLifecycle::Active(terminal);

        let error = lifecycle
            .begin_termination_with(|_| Err(std::io::Error::other("injected restore failure")))
            .expect_err("restore failure must surface");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(matches!(lifecycle, super::RawModeLifecycle::Active(_)));
        lifecycle
            .begin_termination_with(|active| {
                assert!(active.is_some(), "retry retains the original termios state");
                Ok(())
            })
            .expect("cleanup retry succeeds");
        assert!(matches!(lifecycle, super::RawModeLifecycle::Terminating));
    }

    #[test]
    fn raw_mode_waits_for_the_foreground_when_started_in_the_background() {
        let terminal = super::RawModeState {
            stdin_fd: -1,
            // SAFETY: termios is a plain C struct; this state is never applied to a terminal
            // (stdin_fd is -1).
            original: unsafe { std::mem::zeroed() },
            // SAFETY: termios is a plain C struct; this state is never applied to a terminal
            // (stdin_fd is -1).
            raw: unsafe { std::mem::zeroed() },
            snapshot_stale: false,
        };
        let mut lifecycle = super::RawModeLifecycle::Inactive;

        assert!(
            lifecycle
                .activate_with(terminal, |_| Ok(None))
                .expect("background activation succeeds")
        );
        assert!(
            matches!(lifecycle, super::RawModeLifecycle::Suspended(suspended) if suspended.snapshot_stale),
            "a background start must wait suspended and re-read the terminal later"
        );

        lifecycle
            .resume_with(|_| Ok(None))
            .expect("background resume succeeds");
        assert!(
            matches!(lifecycle, super::RawModeLifecycle::Suspended(_)),
            "a resume in the background must not enter raw mode"
        );

        lifecycle
            .resume_with(|_| Ok(Some(terminal)))
            .expect("foreground resume succeeds");
        assert!(
            matches!(lifecycle, super::RawModeLifecycle::Active(active) if !active.snapshot_stale),
            "the resume that brings the wrapper forward keeps the state it applied"
        );
    }

    #[test]
    fn continue_reapplies_raw_mode_only_in_the_foreground() {
        let terminal = super::RawModeState {
            stdin_fd: -1,
            // SAFETY: termios is a plain C struct; this state is never applied to a terminal
            // (stdin_fd is -1).
            original: unsafe { std::mem::zeroed() },
            // SAFETY: termios is a plain C struct; this state is never applied to a terminal
            // (stdin_fd is -1).
            raw: unsafe { std::mem::zeroed() },
            snapshot_stale: false,
        };
        let mut lifecycle = super::RawModeLifecycle::Active(terminal);
        let mut applied = 0;

        lifecycle
            .reassert_with(|terminal| {
                applied += 1;
                Ok(Some(*terminal))
            })
            .expect("foreground continue succeeds");
        assert_eq!(
            applied, 1,
            "a continued foreground session re-enters raw mode"
        );
        assert!(matches!(lifecycle, super::RawModeLifecycle::Active(_)));

        lifecycle
            .reassert_with(|_| Ok(None))
            .expect("background continue succeeds");
        assert!(
            matches!(lifecycle, super::RawModeLifecycle::Suspended(_)),
            "a continued background job gives the terminal up"
        );

        lifecycle
            .reassert_with(|terminal| Ok(Some(*terminal)))
            .expect("continue into the foreground succeeds");
        assert!(matches!(lifecycle, super::RawModeLifecycle::Active(_)));

        for mut lifecycle in [
            super::RawModeLifecycle::Inactive,
            super::RawModeLifecycle::Terminating,
        ] {
            lifecycle
                .reassert_with(|_| panic!("raw mode applied outside a session"))
                .expect("continue outside a session is a no-op");
        }
    }

    // The outer terminal can be resized after the PTY is opened and before the
    // resize watcher's SIGWINCH handler is installed.
    #[test]
    fn resize_watcher_starts_from_the_ptys_own_size() {
        let opened = portable_pty::PtySize {
            rows: 24,
            cols: 80,
            ..portable_pty::PtySize::default()
        };
        let resized = portable_pty::PtySize {
            rows: 30,
            cols: 100,
            ..portable_pty::PtySize::default()
        };
        let pair = portable_pty::native_pty_system()
            .openpty(opened)
            .expect("openpty");

        let synced = super::start_pty_size_sync(&*pair.master, resized);

        assert_eq!(synced, resized);
        assert_eq!(
            pair.master.get_size().expect("PTY size"),
            resized,
            "a resize made during startup never reached the PTY"
        );
    }

    #[test]
    fn session_diagnostics_do_not_panic_on_an_unwritable_stderr() {
        let source = include_str!("pty.rs");
        let runtime_source = source.split("mod tests").next().unwrap_or(source);

        assert!(
            !runtime_source.contains("eprintln!(") && !runtime_source.contains("println!("),
            "print macros panic on EIO after a terminal hangup; use warn()"
        );
    }

    #[test]
    fn raw_mode_signal_handler_does_not_restore_terminal_directly() {
        let source = include_str!("pty.rs");
        let handler_source = source
            .split("extern \"C\" fn restore_raw_mode_for_signal")
            .nth(1)
            .expect("signal handler exists")
            .split("fn terminate_raw_mode_from_signal_state")
            .next()
            .expect("handler ends before restore helper");

        assert!(
            !handler_source.contains("terminate_raw_mode_from_signal_state"),
            "signal handler should delegate restore work out of signal context"
        );
        assert!(
            !handler_source.contains("tcsetattr"),
            "signal handler must not call non-async-signal-safe terminal APIs"
        );
    }

    #[test]
    fn job_control_transition_restores_before_suspend_and_reapplies_after_resume() {
        let events = Mutex::new(Vec::new());
        super::with_terminal_restored_for_job_control(
            || {
                events.lock().unwrap().push("restore");
                Ok(())
            },
            || events.lock().unwrap().push("suspend"),
            || {
                events.lock().unwrap().push("reapply");
                Ok(())
            },
        )
        .expect("job-control transition succeeds");

        assert_eq!(
            events.into_inner().unwrap(),
            ["restore", "suspend", "reapply"]
        );
    }

    #[test]
    fn failed_job_control_restore_does_not_suspend() {
        let suspended = AtomicBool::new(false);
        let reapplied = AtomicBool::new(false);

        let error = super::with_terminal_restored_for_job_control(
            || Err(std::io::Error::other("injected restore failure")),
            || suspended.store(true, Ordering::Release),
            || {
                reapplied.store(true, Ordering::Release);
                Ok(())
            },
        )
        .expect_err("failed restore must abort suspension");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(!suspended.load(Ordering::Acquire));
        assert!(!reapplied.load(Ordering::Acquire));
    }

    // A background read of the terminal raises SIGTTIN again every time it
    // restarts, so a burst of stop requests is queued before the wrapper
    // stops. One stop satisfies them all, as SIGCONT discards pending stops.
    #[test]
    fn stop_requests_queued_before_a_resume_are_coalesced() {
        let (read_fd, write_fd) = super::pipe_fds().expect("signal pipe");
        for _ in 0..200 {
            super::write_signal_byte(write_fd, libc::SIGTTIN as u8);
        }
        super::write_signal_byte(write_fd, super::RAW_SIGNAL_STOP_BYTE);
        let state = Mutex::new(super::RawModeLifecycle::Inactive);
        let mut stops = 0;

        super::handle_session_signals_with(read_fd, &state, || {
            stops += 1;
            Ok(())
        });

        assert_eq!(stops, 1, "stale stop requests stopped the wrapper again");
        super::close_fd(write_fd);
    }

    #[test]
    fn only_background_io_stops_shortly_after_a_resume_in_the_foreground_are_stale() {
        let just_resumed = Some(Instant::now());
        let long_resumed = Instant::now().checked_sub(super::STALE_STOP_WINDOW * 2);

        assert!(super::is_stale_stop_request(
            libc::SIGTTIN,
            just_resumed,
            || true
        ));
        assert!(super::is_stale_stop_request(
            libc::SIGTTOU,
            just_resumed,
            || true
        ));
        // An explicit stop from the user is honored, and so is any request
        // while in the background or long after the last resume.
        assert!(!super::is_stale_stop_request(
            libc::SIGTSTP,
            just_resumed,
            || true
        ));
        assert!(!super::is_stale_stop_request(
            libc::SIGTTIN,
            just_resumed,
            || false
        ));
        assert!(!super::is_stale_stop_request(libc::SIGTTIN, None, || true));
        if long_resumed.is_some() {
            assert!(!super::is_stale_stop_request(
                libc::SIGTTIN,
                long_resumed,
                || true
            ));
        }
    }

    #[test]
    fn input_worker_failure_is_reported_and_wakes_the_session() {
        struct BrokenWriter;
        impl std::io::Write for BrokenWriter {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected PTY write failure",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut input = Cursor::new(b"show version\n".to_vec());
        let mut output = BrokenWriter;
        let recent_input = Mutex::new(Vec::new());
        let state = Mutex::new(None);
        let mut woke_session = false;
        super::run_input_worker(
            &state,
            || {
                super::forward_stdin_to_pty(
                    &mut input,
                    &mut output,
                    false,
                    super::IoTrace::open(None).expect("trace disabled"),
                    None,
                    &recent_input,
                )
            },
            || woke_session = true,
        );

        assert!(
            woke_session,
            "main PTY loop was not woken after worker failure"
        );
        assert_eq!(
            super::take_input_worker_failure(&state)
                .expect("main PTY loop receives the worker failure")
                .kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    // The self-pipe protocol between a signal handler and the watcher's stop:
    // once the slot is disarmed a handler must never write, and the descriptor
    // must not be closed while a handler that already loaded it is still
    // writing (its number could be reused by then).
    fn signal_pipe_for_test() -> (RawFd, RawFd) {
        let (read_fd, write_fd) = super::pipe_fds().expect("pipe");
        super::set_fd_nonblocking(read_fd).expect("nonblocking read end");
        super::set_fd_nonblocking(write_fd).expect("nonblocking write end");
        (read_fd, write_fd)
    }

    fn drain_signal_pipe(read_fd: RawFd) -> Vec<u8> {
        let mut buffer = [0_u8; 16];
        // SAFETY: `buffer` is valid for writes of its length and `read_fd` is our pipe end.
        let read = unsafe { libc::read(read_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read <= 0 {
            Vec::new()
        } else {
            buffer[..read as usize].to_vec()
        }
    }

    #[test]
    fn signal_pipe_slot_notify_writes_only_while_armed() {
        let slot = super::SignalPipeSlot::new();
        let (read_fd, write_fd) = signal_pipe_for_test();

        slot.notify(b'x');
        assert!(drain_signal_pipe(read_fd).is_empty(), "unarmed slot wrote");

        slot.arm(write_fd);
        slot.notify(b'y');
        assert_eq!(drain_signal_pipe(read_fd), b"y");

        assert!(slot.disarm(), "no handler in flight, so closable");
        slot.notify(b'z');
        assert!(drain_signal_pipe(read_fd).is_empty(), "disarmed slot wrote");

        super::close_fd(read_fd);
        super::close_fd(write_fd);
    }

    #[test]
    fn signal_pipe_slot_disarm_waits_for_an_in_flight_handler() {
        static SLOT: super::SignalPipeSlot = super::SignalPipeSlot::new();
        let (read_fd, write_fd) = signal_pipe_for_test();
        SLOT.arm(write_fd);

        // A handler that has announced itself but not finished writing yet.
        SLOT.in_flight.fetch_add(1, Ordering::SeqCst);
        let disarm = std::thread::spawn(|| SLOT.disarm());
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !disarm.is_finished(),
            "disarm returned while a handler was still in flight"
        );

        SLOT.in_flight.fetch_sub(1, Ordering::SeqCst);
        assert!(
            disarm.join().expect("disarm thread"),
            "descriptor is closable once the handler has left"
        );

        super::close_fd(read_fd);
        super::close_fd(write_fd);
    }

    #[test]
    fn signal_pipe_slot_disarm_gives_up_on_a_stalled_handler() {
        let slot = super::SignalPipeSlot::new();
        slot.arm(3);
        slot.in_flight.fetch_add(1, Ordering::SeqCst);

        let started = Instant::now();
        assert!(
            !slot.disarm_within(Duration::from_millis(50)),
            "a stalled handler must make the descriptor non-closable"
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_eq!(slot.write_fd.load(Ordering::SeqCst), -1);
    }

    fn fd_is_close_on_exec(fd: RawFd) -> bool {
        // SAFETY: F_GETFD takes no pointer argument; an invalid `fd` reports EBADF.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            flags >= 0,
            "fcntl(F_GETFD) on {fd}: {}",
            std::io::Error::last_os_error()
        );
        flags & libc::FD_CLOEXEC != 0
    }

    // The signal self-pipe exists before the wrapped command is spawned, and
    // portable-pty's /dev/fd scan, the only other thing that keeps it out of
    // the child, does nothing when /proc is not mounted.
    #[test]
    fn self_pipe_ends_are_close_on_exec() {
        let (read_fd, write_fd) = super::pipe_fds().expect("pipe");
        let read_cloexec = fd_is_close_on_exec(read_fd);
        let write_cloexec = fd_is_close_on_exec(write_fd);
        super::close_fd(read_fd);
        super::close_fd(write_fd);
        assert!(read_cloexec, "self-pipe read end lacks FD_CLOEXEC");
        assert!(write_cloexec, "self-pipe write end lacks FD_CLOEXEC");
    }

    #[test]
    fn master_dup_is_close_on_exec() {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .expect("openpty");
        let duped = super::dup_master_fd(&*pair.master).expect("dup master");
        assert!(
            fd_is_close_on_exec(std::os::fd::AsRawFd::as_raw_fd(&duped)),
            "PTY master dup lacks FD_CLOEXEC"
        );
    }

    #[test]
    fn master_write_eio_ends_forwarding_without_failure() {
        // EIO on the PTY master means the slave side is gone: the wrapped child
        // has exited (a paste ending in `exit`, or fast typing). That is the
        // session ending, not an input failure, so the worker must return Ok
        // and let the main loop report the child's own exit code.
        struct ClosedSlaveWriter;
        impl std::io::Write for ClosedSlaveWriter {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut input = Cursor::new(b"y\n".to_vec());
        let mut output = ClosedSlaveWriter;
        let recent_input = Mutex::new(Vec::new());
        let state = Mutex::new(None);
        let mut woke_session = false;
        super::run_input_worker(
            &state,
            || {
                super::forward_stdin_to_pty(
                    &mut input,
                    &mut output,
                    false,
                    super::IoTrace::open(None).expect("trace disabled"),
                    None,
                    &recent_input,
                )
            },
            || woke_session = true,
        );

        assert!(
            !woke_session,
            "a closed slave must not be treated as an input worker failure"
        );
        assert!(
            super::take_input_worker_failure(&state).is_none(),
            "EIO on the PTY master must not surface as a session error"
        );
    }

    #[test]
    fn input_worker_panic_is_reported_and_wakes_the_session() {
        let state = Mutex::new(None);
        let mut woke_session = false;
        super::run_input_worker(
            &state,
            || -> std::io::Result<()> { panic!("injected input worker panic") },
            || woke_session = true,
        );

        assert!(
            woke_session,
            "main PTY loop was not woken after worker panic"
        );
        assert_eq!(
            super::take_input_worker_failure(&state)
                .expect("main PTY loop receives the worker panic")
                .kind(),
            std::io::ErrorKind::Other
        );
    }

    #[test]
    fn signal_restore_state_uses_owned_shared_storage() {
        let source = include_str!("pty.rs");
        let runtime_source = source.split("mod tests").next().unwrap_or(source);

        assert!(
            !runtime_source.contains("AtomicPtr"),
            "signal restoration must not dereference raw pointers into a dropping guard"
        );
        let worker_source = runtime_source
            .split("fn run_input_worker")
            .nth(1)
            .expect("input worker helper exists")
            .split("fn take_input_worker_failure")
            .next()
            .expect("input worker helper has a boundary");
        assert!(
            !worker_source.contains("restore_raw_mode"),
            "detached input workers must leave terminal restoration to the owning teardown"
        );
    }

    #[test]
    fn pty_resize_uses_sigwinch_instead_of_fixed_polling() {
        let source = include_str!("pty.rs");
        let runtime_source = source.split("mod tests").next().unwrap_or(source);

        assert!(runtime_source.contains("SIGWINCH"));
        assert!(!runtime_source.contains("Duration::from_millis(250)"));
    }

    #[test]
    fn pty_size_falls_back_to_standard_terminal_dimensions() {
        assert_eq!(
            super::fallback_pty_size(None),
            portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }
        );
    }

    #[test]
    fn iterm_shell_integration_guard_removes_nested_iterm_environment() {
        let mut builder = portable_pty::CommandBuilder::new("/bin/zsh");
        builder.env("TERM_PROGRAM", "iTerm.app");
        builder.env("TERM_PROGRAM_VERSION", "3.6.0");
        builder.env("LC_TERMINAL", "iTerm.app");
        builder.env("LC_TERMINAL_VERSION", "3.6.0");
        builder.env("ITERM_SESSION_ID", "w0t0p0");
        builder.env("ITERM_PROFILE", "Default");

        super::apply_iterm_shell_integration_guard(&mut builder, true, true);

        for key in [
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
            "ITERM_SESSION_ID",
            "ITERM_PROFILE",
        ] {
            assert!(builder.get_env(key).is_none(), "{key} should be removed");
        }
        for (key, value) in [
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.6.0"),
            ("LC_TERMINAL", "iTerm.app"),
            ("LC_TERMINAL_VERSION", "3.6.0"),
            ("ITERM_SESSION_ID", "w0t0p0"),
            ("ITERM_PROFILE", "Default"),
        ] {
            let parent_key = format!("PRISMTTY_PARENT_{key}");
            assert_eq!(
                builder.get_env(&parent_key),
                Some(std::ffi::OsStr::new(value)),
                "{parent_key} should preserve {key}"
            );
        }
        assert_eq!(
            builder.get_env("ITERM2_SQUELCH_MARK"),
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(
            builder.get_env("ITERM_SHELL_INTEGRATION_INSTALLED"),
            Some(std::ffi::OsStr::new("prismtty"))
        );
        assert_eq!(
            builder.get_env("PRISMTTY_NESTED_ITERM"),
            Some(std::ffi::OsStr::new("1"))
        );
    }

    #[test]
    fn iterm_shell_integration_guard_keeps_environment_for_non_iterm_or_noninteractive() {
        let mut builder = portable_pty::CommandBuilder::new("/bin/zsh");
        // CommandBuilder seeds itself from the process environment, so clear it
        // first: otherwise an ambient PRISMTTY_PARENT_* (present when the suite
        // runs inside a nested prismtty session) would masquerade as output of
        // the guard under test and fail the is_none() assertions below.
        builder.env_clear();
        builder.env("TERM_PROGRAM", "iTerm.app");
        builder.env("ITERM_SESSION_ID", "w0t0p0");

        super::apply_iterm_shell_integration_guard(&mut builder, true, false);
        assert_eq!(
            builder.get_env("TERM_PROGRAM"),
            Some(std::ffi::OsStr::new("iTerm.app"))
        );
        assert!(builder.get_env("PRISMTTY_PARENT_TERM_PROGRAM").is_none());
        assert!(
            builder
                .get_env("PRISMTTY_PARENT_ITERM_SESSION_ID")
                .is_none()
        );

        super::apply_iterm_shell_integration_guard(&mut builder, false, true);
        assert_eq!(
            builder.get_env("TERM_PROGRAM"),
            Some(std::ffi::OsStr::new("iTerm.app"))
        );
        assert!(builder.get_env("PRISMTTY_PARENT_TERM_PROGRAM").is_none());
        assert!(
            builder
                .get_env("PRISMTTY_PARENT_ITERM_SESSION_ID")
                .is_none()
        );
    }
}
