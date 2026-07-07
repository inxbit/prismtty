use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::process::ExitCode;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, AtomicPtr, Ordering},
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
use super::runtime::{ReloadWatcher, RuntimeRegistration};
use super::stream::{InputSource, highlight_stream};
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
const TERMINATING_SIGNALS: [libc::c_int; 4] =
    [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGINT];
const RAW_SIGNAL_STOP_BYTE: u8 = 0;
const RESIZE_STOP_BYTE: u8 = b'q';
const RESIZE_WAKE_BYTE: u8 = b'r';

static RAW_MODE_FD: AtomicI32 = AtomicI32::new(-1);
static RAW_MODE_ORIGINAL: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());
static RAW_MODE_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static RESIZE_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static FORWARD_CHILD_PID: AtomicI32 = AtomicI32::new(0);

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

/// Reaps the wrapped child and returns its process-exit byte. Prefers a direct
/// `waitpid` so a terminating signal becomes `128 + signal`, falling back to
/// portable-pty's status when the pid is unavailable.
fn reap_child(child: &mut Box<dyn portable_pty::Child + Send + Sync>) -> Result<u8, CliError> {
    if let Some(pid) = child.process_id() {
        if let Ok(status) = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid as i32), None) {
            return Ok(exit_code_from_wait(status));
        }
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
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    let Some(pid) = child.process_id() else {
        let _ = child.try_wait();
        return;
    };
    let pid = pid as i32;
    for signal in [libc::SIGHUP, libc::SIGKILL] {
        unsafe { libc::kill(-pid, signal) };
        let deadline = Instant::now() + CHILD_EXIT_GRACE;
        while Instant::now() < deadline {
            match waitpid(nix::unistd::Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => thread::sleep(Duration::from_millis(20)),
                _ => return,
            }
        }
    }
}

/// Passes a fallible post-spawn setup step through; on failure, terminates and
/// reaps the just-spawned child so an early error cannot orphan it or hang on
/// a child that ignores SIGHUP.
fn cleanup_child_on_err<T, E>(
    result: Result<T, E>,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
) -> Result<T, E> {
    if result.is_err() {
        terminate_and_reap_child(child);
        FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
    }
    result
}

/// Forwards a signal received by prismtty to the wrapped child's process group
/// so a `kill` of prismtty reaches the child instead of orphaning it. The child
/// is a PTY session leader, so its process-group id equals its pid.
fn forward_signal_to_child(signal: u8) {
    let pid = FORWARD_CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(-pid, libc::c_int::from(signal));
        }
    }
}

/// Runs `body`, catching a panic so it surfaces as a clear diagnostic instead
/// of silently killing a detached worker thread (which could otherwise leave
/// the session wedged with no error). Returns `false` if `body` panicked. When
/// `restore_terminal` is set (the stdin forwarder), a panic also restores
/// cooked mode so the user is not stranded in raw mode.
fn run_supervised(name: &str, restore_terminal: bool, body: impl FnOnce()) -> bool {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_ok() {
        return true;
    }
    if restore_terminal {
        restore_raw_mode_from_signal_state();
    }
    eprintln!("prismtty: the {name} thread stopped unexpectedly (panic)");
    false
}

/// Spawns a worker thread whose body is run under [`run_supervised`].
fn spawn_supervised(
    name: &'static str,
    restore_terminal: bool,
    body: impl FnOnce() + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        run_supervised(name, restore_terminal, body);
    })
}

pub(super) fn run_command(options: Options, command: Vec<OsString>) -> Result<ExitCode, CliError> {
    let command_name = command[0].clone();
    let command_args = command[1..].to_vec();
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(current_pty_size())?;
    let interactive = io::stdin().is_terminal();

    let mut builder = CommandBuilder::new(command_name);
    for arg in command_args {
        builder.arg(arg);
    }
    apply_iterm_shell_integration_guard(&mut builder, interactive, parent_terminal_is_iterm());

    if interactive {
        configure_child_pty(&*pair.master)?;
    }

    let mut child = pair.slave.spawn_command(builder)?;
    drop(pair.slave);
    FORWARD_CHILD_PID.store(
        child.process_id().map(|pid| pid as i32).unwrap_or(0),
        Ordering::SeqCst,
    );

    let raw_mode = if interactive {
        Some(cleanup_child_on_err(RawModeGuard::enable(), &mut child)?)
    } else {
        None
    };

    let trace = cleanup_child_on_err(IoTrace::open(options.trace_io.as_deref()), &mut child)?;
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
    let pty_fd_owned = if interactive {
        Some(cleanup_child_on_err(
            dup_master_fd(&*pair.master),
            &mut child,
        )?)
    } else {
        None
    };
    let pty_fd = pty_fd_owned.as_ref().map(AsRawFd::as_raw_fd);

    // Records bytes forwarded to the child so the highlight loop can recognise
    // their echo: it surfaces a buffered trailing token on idle only when the
    // token is a byte-for-byte suffix of this recent input, never a
    // speculatively-buffered token of program output. The buffer is scoped to
    // the in-progress line (cleared on submit/abort) and is never emitted to the
    // screen, so a non-echoed secret is never drawn even if it is briefly held.
    let recent_input = Arc::new(Mutex::new(Vec::new()));
    if raw_mode.is_some() {
        let mut writer = cleanup_child_on_err(pair.master.take_writer(), &mut child)?;
        let trace = trace.clone();
        let local_echo = options.local_echo;
        let recent_input = Arc::clone(&recent_input);
        spawn_supervised("input forwarding", true, move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            let _ = forward_stdin_to_pty(
                &mut stdin,
                &mut writer,
                local_echo,
                trace,
                profile_input_tx,
                &recent_input,
            );
        });
    }

    let mut reader = cleanup_child_on_err(pair.master.try_clone_reader(), &mut child)?;
    let mut resize_watcher =
        cleanup_child_on_err(PtyResizeWatcher::start(pair.master), &mut child)?;

    let mut stdout = io::stdout();
    let _registration = cleanup_child_on_err(RuntimeRegistration::register(), &mut child)?;
    let reload_watcher = Some(ReloadWatcher::new());
    let stream_result = highlight_stream(
        &mut reader,
        &mut stdout,
        &options,
        InputSource {
            interactive,
            pty_fd,
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
        terminate_and_reap_child(&mut child);
        1
    } else {
        reap_child(&mut child)?
    };
    FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
    drop(raw_mode);
    stream_result?;
    Ok(ExitCode::from(exit))
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
    let slave_tty = OpenOptions::new().read(true).write(true).open(tty_name)?;
    let mut termios = source;
    normalize_child_pty_termios(&mut termios);
    tcsetattr(slave_tty.as_fd(), SetArg::TCSANOW, &termios)?;
    Ok(())
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
        writer.write_all(input)?;
        writer.flush()?;

        if local_echo {
            let echo = echo_state.push(input);
            if !echo.is_empty() {
                let mut stdout = io::stdout().lock();
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

fn dup_master_fd(master: &dyn portable_pty::MasterPty) -> io::Result<OwnedFd> {
    let fd = master.as_raw_fd().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY master file descriptor is unavailable",
        )
    })?;
    // SAFETY: `fd` is the live PTY master descriptor. `dup` returns a fresh
    // descriptor that `OwnedFd` takes sole ownership of and closes on drop.
    let duped = unsafe { libc::dup(fd) };
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
                    self.pending.extend_from_slice(&bytes[idx..]);
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

struct RawModeGuard {
    stdin_fd: RawFd,
    original: Box<libc::termios>,
    signal_watcher: RawSignalWatcher,
    previous_signal_handlers: Vec<(libc::c_int, libc::sigaction)>,
}

impl RawModeGuard {
    fn enable() -> Result<Self, CliError> {
        let stdin = io::stdin();
        let stdin_fd = stdin.as_fd().as_raw_fd();
        let original = terminal_attrs(stdin_fd)?;
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        set_terminal_attrs(stdin_fd, &raw)?;
        let signal_watcher = match RawSignalWatcher::start() {
            Ok(watcher) => watcher,
            Err(error) => {
                let _ = set_terminal_attrs(stdin_fd, &original);
                return Err(error.into());
            }
        };

        let mut guard = Self {
            stdin_fd,
            original: Box::new(original),
            signal_watcher,
            previous_signal_handlers: Vec::new(),
        };
        guard.activate_signal_restore();
        for signal in TERMINATING_SIGNALS {
            let previous = install_signal_handler(signal, restore_raw_mode_for_signal, 0)?;
            guard.previous_signal_handlers.push((signal, previous));
        }
        Ok(guard)
    }

    fn activate_signal_restore(&mut self) {
        RAW_MODE_FD.store(self.stdin_fd, Ordering::SeqCst);
        RAW_MODE_ORIGINAL.store(self.original.as_mut(), Ordering::SeqCst);
    }

    fn deactivate_signal_restore(&mut self) {
        RAW_MODE_ORIGINAL.store(std::ptr::null_mut(), Ordering::SeqCst);
        RAW_MODE_FD.store(-1, Ordering::SeqCst);
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = set_terminal_attrs(self.stdin_fd, &self.original);
        self.deactivate_signal_restore();
        restore_signal_handlers(&self.previous_signal_handlers);
        self.signal_watcher.stop();
    }
}

extern "C" fn restore_raw_mode_for_signal(signal: libc::c_int) {
    let write_fd = RAW_MODE_SIGNAL_WRITE_FD.load(Ordering::SeqCst);
    if write_fd >= 0 {
        write_signal_byte(write_fd, signal as u8);
    }
}

fn restore_raw_mode_from_signal_state() {
    let fd = RAW_MODE_FD.load(Ordering::SeqCst);
    let original = RAW_MODE_ORIGINAL.load(Ordering::SeqCst);
    if fd >= 0 && !original.is_null() {
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, original);
        }
    }
}

fn terminal_attrs(fd: RawFd) -> io::Result<libc::termios> {
    let mut attrs = unsafe { mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut attrs) } == 0 {
        Ok(attrs)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_terminal_attrs(fd: RawFd, attrs: &libc::termios) -> io::Result<()> {
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, attrs) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

struct RawSignalWatcher {
    write_fd: RawFd,
    thread: Option<thread::JoinHandle<()>>,
}

impl RawSignalWatcher {
    fn start() -> io::Result<Self> {
        let (read_fd, write_fd) = pipe_fds()?;
        if let Err(error) = set_fd_nonblocking(write_fd) {
            close_fd(read_fd);
            close_fd(write_fd);
            return Err(error);
        }
        RAW_MODE_SIGNAL_WRITE_FD.store(write_fd, Ordering::SeqCst);
        let thread = spawn_supervised("signal handler", false, move || {
            restore_raw_mode_on_signals(read_fd)
        });
        Ok(Self {
            write_fd,
            thread: Some(thread),
        })
    }

    fn stop(&mut self) {
        RAW_MODE_SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
        if self.write_fd >= 0 {
            write_signal_byte(self.write_fd, RAW_SIGNAL_STOP_BYTE);
            close_fd(self.write_fd);
            self.write_fd = -1;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RawSignalWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn restore_raw_mode_on_signals(read_fd: RawFd) {
    while let Some(bytes) = read_signal_bytes(read_fd) {
        if bytes.contains(&RAW_SIGNAL_STOP_BYTE) {
            break;
        }
        if let Some(signal) = bytes.into_iter().next() {
            restore_raw_mode_from_signal_state();
            forward_signal_to_child(signal);
            unsafe {
                libc::_exit(128 + libc::c_int::from(signal));
            }
        }
    }
    close_fd(read_fd);
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
    let mut winsize: libc::winsize = unsafe { mem::zeroed() };
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
        RESIZE_SIGNAL_WRITE_FD.store(write_fd, Ordering::SeqCst);
        let previous_signal_handler = match install_signal_handler(
            libc::SIGWINCH,
            notify_resize_for_signal,
            libc::SA_RESTART,
        ) {
            Ok(previous) => previous,
            Err(error) => {
                RESIZE_SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
                close_fd(read_fd);
                close_fd(write_fd);
                return Err(error);
            }
        };
        let thread = spawn_supervised("resize watcher", false, move || {
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
        RESIZE_SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
        if self.write_fd >= 0 {
            write_signal_byte(self.write_fd, RESIZE_STOP_BYTE);
            close_fd(self.write_fd);
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
    let write_fd = RESIZE_SIGNAL_WRITE_FD.load(Ordering::SeqCst);
    if write_fd >= 0 {
        write_signal_byte(write_fd, RESIZE_WAKE_BYTE);
    }
}

fn resize_pty_on_signals(master: Box<dyn portable_pty::MasterPty + Send>, read_fd: RawFd) {
    let mut last_size = current_pty_size();
    while let Some(bytes) = read_signal_bytes(read_fd) {
        if bytes.contains(&RESIZE_STOP_BYTE) {
            break;
        }
        let next_size = current_pty_size();
        if next_size != last_size {
            let _ = master.resize(next_size);
            last_size = next_size;
        }
    }
    close_fd(read_fd);
}

fn pipe_fds() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
        Ok((fds[0], fds[1]))
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_fd_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_signal_bytes(read_fd: RawFd) -> Option<Vec<u8>> {
    let mut buffer = [0_u8; 64];
    loop {
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

fn write_signal_byte(write_fd: RawFd, byte: u8) {
    let bytes = [byte];
    unsafe {
        libc::write(write_fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
    }
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

fn install_signal_handler(
    signal: libc::c_int,
    handler: extern "C" fn(libc::c_int),
    flags: libc::c_int,
) -> io::Result<libc::sigaction> {
    let mut action = unsafe { mem::zeroed::<libc::sigaction>() };
    let mut previous = unsafe { mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = handler as libc::sighandler_t;
    action.sa_flags = flags;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
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
    unsafe {
        libc::sigaction(signal, previous, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Mutex;

    use nix::sys::termios::{InputFlags, LocalFlags, OutputFlags};

    #[test]
    fn supervised_run_catches_a_panicking_body() {
        assert!(super::run_supervised("test-thread", false, || {}));
        assert!(!super::run_supervised("test-thread", false, || panic!(
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
            .find("writer.write_all(input)?")
            .expect("input is written to child PTY");

        assert!(
            record_idx < write_idx,
            "recent input must be recorded before PTY write so immediate echo can be matched"
        );
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
        let source = include_str!("pty.rs");
        let runtime_source = source.split("mod tests").next().unwrap_or(source);

        assert!(runtime_source.contains("restore_raw_mode_for_signal"));
        for signal in ["SIGTERM", "SIGHUP", "SIGQUIT", "SIGINT"] {
            assert!(runtime_source.contains(signal), "missing {signal} handler");
        }
    }

    #[test]
    fn raw_mode_signal_handler_does_not_restore_terminal_directly() {
        let source = include_str!("pty.rs");
        let handler_source = source
            .split("extern \"C\" fn restore_raw_mode_for_signal")
            .nth(1)
            .expect("signal handler exists")
            .split("fn restore_raw_mode_from_signal_state")
            .next()
            .expect("handler ends before restore helper");

        assert!(
            !handler_source.contains("restore_raw_mode_from_signal_state"),
            "signal handler should delegate restore work out of signal context"
        );
        assert!(
            !handler_source.contains("tcsetattr"),
            "signal handler must not call non-async-signal-safe terminal APIs"
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
