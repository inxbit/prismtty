use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, SyncSender},
};
use std::thread;
use std::time::{Duration, Instant};

use is_terminal::IsTerminal;
use nix::libc;
use nix::sys::termios::{
    InputFlags, LocalFlags, OutputFlags, SetArg, Termios, tcgetattr, tcsetattr,
};
use portable_pty::{CommandBuilder, PtySize, SlavePty, native_pty_system};

use super::CliError;
use super::args::Options;
use super::profile_selection::dynamic_profile_enabled;
use super::runtime::{register_for_reload, unregister_before_exit};
use super::stream::{InputSource, PreparedSession, highlight_stream, prepare_session};
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

/// Whether `signal` is a message for the wrapped command. SIGUSR1 and SIGUSR2
/// sent to prismtty go to the command it started, alone, and the session goes
/// on. Async-signal-safe.
fn is_message_signal(signal: libc::c_int) -> bool {
    signal == libc::SIGUSR1 || signal == libc::SIGUSR2
}

/// Whether `signal` ends the session: every caught terminating signal except
/// the two messages. Async-signal-safe.
fn is_end_signal(signal: libc::c_int) -> bool {
    !is_message_signal(signal) && TERMINATING_SIGNALS.contains(&signal)
}

/// How long the end of a session may take once the signal watcher has taken
/// an ending signal: the 1.4.0 teardown's two windows of `CHILD_EXIT_GRACE`.
/// The normal end that returns the command's own exit code must be over by
/// then too; past it, the end is abrupt.
const END_LIMIT: Duration = Duration::from_secs(4);

/// The ending signal (End) the handlers recorded, and whether a thread has
/// taken it. The watcher acts on it before any byte queued in the self-pipe:
/// the pipe only wakes the watcher, and its nonblocking write fails once a
/// burst of messages has filled it, while this state does not.
///
/// The handoff of an End, in one session:
///
/// 1. No End: nothing recorded. `SessionSignalWatcher::stop` gives the
///    signals back to their previous actions; from then on a signal takes
///    that action, as in 1.4.1.
/// 2. Recorded, not taken: the first `record` wins. If recorded before
///    `stop`'s final check, the watcher or `stop` takes it; `stop` may
///    already be giving dispositions back.
/// 3. Taken, not published: the watcher or `stop` won `take` and runs
///    `end_session_after_signal`. If give-back began, `stop` may need to
///    reinstall handlers before or after joining the watcher.
/// 4. Published: the outcome is under the reap lock and its status in
///    `PUBLISHED_LEADER_STATUS`. Who ends the process follows from it.
///
/// An End recorded before `stop`'s final check is taken by the watcher or by
/// `stop`. Give-back and any reinstallation each use one `sigaction` per
/// signal, so another signal can take its previous action in between. A
/// handler that records only after the final check has no taker; that End
/// does not change the session's otherwise determined result.
struct PendingEnd {
    signal: AtomicI32,
    taken: AtomicBool,
}

impl PendingEnd {
    const fn new() -> Self {
        Self {
            signal: AtomicI32::new(0),
            taken: AtomicBool::new(false),
        }
    }

    /// Async-signal-safe: called from signal handlers.
    fn record(&self, signal: libc::c_int) {
        let _ = self
            .signal
            .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
    }

    fn get(&self) -> Option<libc::c_int> {
        let signal = self.signal.load(Ordering::SeqCst);
        (signal != 0).then_some(signal)
    }

    /// The recorded End, for the one thread that takes it.
    fn take(&self) -> Option<libc::c_int> {
        let signal = self.get()?;
        self.taken
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| signal)
    }

    fn clear(&self) {
        self.taken.store(false, Ordering::SeqCst);
        self.signal.store(0, Ordering::SeqCst);
    }
}

static PENDING_END: PendingEnd = PendingEnd::new();

/// Test only: an ending signal to record at the watcher's stop byte, after
/// its last check, as a handler can while the session stops.
#[cfg(test)]
static END_TO_RECORD_AT_STOP: AtomicI32 = AtomicI32::new(0);

#[cfg(test)]
fn record_end_at_stop_for_test(pending_end: &PendingEnd) {
    let signal = END_TO_RECORD_AT_STOP.swap(0, Ordering::SeqCst);
    if signal != 0 {
        pending_end.record(signal);
    }
}

/// Test only: when armed, holds the next End at the entry of its path, as a
/// slow step can, until released (for ten seconds at most).
#[cfg(test)]
struct EndEntryPause {
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::atomic::AtomicBool,
    released: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
static END_ENTRY_PAUSE: EndEntryPause = EndEntryPause {
    armed: std::sync::atomic::AtomicBool::new(false),
    entered: std::sync::atomic::AtomicBool::new(false),
    released: std::sync::atomic::AtomicBool::new(false),
};

#[cfg(test)]
fn pause_at_end_entry_for_test() {
    if END_ENTRY_PAUSE.armed.swap(false, Ordering::SeqCst) {
        END_ENTRY_PAUSE.entered.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(10);
        while !END_ENTRY_PAUSE.released.load(Ordering::SeqCst) && Instant::now() < until {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

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
/// Serializes signal forwarding with reaping the wrapped child, and holds
/// what became of it. Whoever reaps the child writes that here under this
/// lock, in the same critical section as the reap and before it clears
/// `FORWARD_CHILD_PID`, so whoever takes the lock next sees the reap and its
/// result together. `set_forward_child_pid` resets it when the next child is
/// registered.
static FORWARD_CHILD_LOCK: Mutex<ReapState> = Mutex::new(ReapState::Unreaped);

/// What became of the registered child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReapState {
    Unreaped,
    /// The main thread reaped it, with this raw wait status.
    Reaped(libc::c_int),
    /// The signal watcher ended the session after an ending signal.
    Ended(EndOutcome),
}

/// How the signal watcher ended a session. It is published once, under the
/// reap lock in the critical section of the watcher's reap, and never changes;
/// the watcher and the main thread both take the final exit from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EndOutcome {
    /// The ending signal prismtty was sent.
    signal: libc::c_int,
    /// The command's raw wait status, when it was reaped.
    leader_status: Option<libc::c_int>,
    /// When the end must be over: `END_LIMIT` after the watcher took the
    /// signal.
    deadline: Instant,
    /// Who ends the process.
    owner: EndOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndOwner {
    /// The command exited by itself: the main thread finishes the normal end
    /// and exits with the command's code, unless the deadline passes first.
    NormalEnd,
    /// The watcher ends the process at once, as 1.4.0 did.
    Watcher,
}

impl EndOutcome {
    /// The status of an abrupt end: 128 plus the signal prismtty was sent.
    fn abrupt_code(&self) -> u8 {
        128u8.saturating_add(self.signal as u8)
    }

    /// How the child ended, for the main thread's reap.
    fn child_exit(&self) -> ChildExit {
        ChildExit {
            code: self
                .leader_status
                .map_or_else(|| self.abrupt_code(), exit_code_from_wait),
            wait_status: self.leader_status,
        }
    }
}

/// The command exited by itself when its reaped status is an ordinary exit,
/// whenever that happened: prismtty's SIGKILL can only leave WIFSIGNALED. The
/// normal end is left to the main thread only when it is bounded, that is
/// when the limit's watchdog runs.
fn end_owner(leader_status: Option<libc::c_int>, normal_end_bounded: bool) -> EndOwner {
    match leader_status {
        Some(status) if normal_end_bounded && libc::WIFEXITED(status) => EndOwner::NormalEnd,
        _ => EndOwner::Watcher,
    }
}

fn forward_child_lock() -> std::sync::MutexGuard<'static, ReapState> {
    FORWARD_CHILD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The outcome the signal watcher published, if it ended the session.
fn published_end_outcome() -> Option<EndOutcome> {
    match *forward_child_lock() {
        ReapState::Ended(outcome) => Some(outcome),
        _ => None,
    }
}

/// Registers the wrapped child for signal forwarding. What was published for
/// an earlier child belongs to that child, so it is dropped here.
fn set_forward_child_pid(pid: libc::pid_t) {
    let mut reap_state = forward_child_lock();
    *reap_state = ReapState::Unreaped;
    PUBLISHED_LEADER_STATUS.store(0, Ordering::SeqCst);
    FORWARD_CHILD_PID.store(pid, Ordering::SeqCst);
}

/// Maps a raw process exit code to a process exit byte, clamping codes above
/// 255 instead of truncating them (so e.g. 256 stays non-zero rather than
/// wrapping to a misleading success).
fn exit_code_byte(raw: u32) -> u8 {
    u8::try_from(raw).unwrap_or(u8::MAX)
}

/// Maps a reaped child's raw `waitpid` status to a process exit byte: a normal
/// exit preserves its (clamped) code, and a signal death reports
/// `128 + signal` (the shell convention) instead of a misleading generic code.
/// The status is decoded as the kernel wrote it, so a signal number with no
/// name here (the Linux real-time signals) is reported like any other.
fn exit_code_from_wait(status: libc::c_int) -> u8 {
    if libc::WIFEXITED(status) {
        exit_code_byte(libc::WEXITSTATUS(status) as u32)
    } else if libc::WIFSIGNALED(status) {
        128u8.saturating_add(libc::WTERMSIG(status) as u8)
    } else {
        1
    }
}

/// Waits until the child can be reaped and returns waitid's `si_code` for it.
/// That is normally an exit (`CLD_EXITED`, `CLD_KILLED`, `CLD_DUMPED`), but a
/// child that made prismtty its tracer is also reported when it stops
/// (`CLD_TRAPPED`), and it is still alive then.
fn wait_for_child_exit_without_reaping(pid: libc::pid_t) -> io::Result<libc::c_int> {
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
            return Ok(info.si_code);
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

/// Closes the parent's PTY slave on a thread of its own once the wrapped
/// child has exited. On macOS the last close of a slave waits up to 0.6 s for
/// the master to read the output still queued and then discards it, so
/// closing it on the thread that reads the master after a short command has
/// already exited would discard all of that command's output. While the
/// parent holds the slave, the child's exit there waits for its output to be
/// read instead, and this close then runs while the read loop drains the
/// master. The slave is closed at once when the child's pid is unknown, the
/// wait fails or no thread can be started.
///
/// Where the child's exit can end the session early (see
/// [`child_exit_notice`]), it also returns the notice through which the
/// waiter reports that exit, after it has closed the slave.
fn close_slave_after_child_exit(
    slave: Box<dyn SlavePty + Send>,
    pid: Option<u32>,
) -> Option<Arc<ChildExitNotice>> {
    let Some(pid) = pid else {
        drop(slave);
        return None;
    };
    let notice = child_exit_notice();
    // A failed spawn drops the closure, the slave and the reporter with it,
    // and a reporter dropped before the wait reports the exit as unknown.
    let mut reporter = notice.clone().map(|notice| ChildExitReporter {
        notice,
        exited: false,
    });
    let _ = thread::Builder::new().spawn(move || {
        let exit = wait_for_child_exit_without_reaping(pid as libc::pid_t);
        drop(slave);
        if let Some(reporter) = reporter.as_mut() {
            reporter.exited = match exit {
                Ok(si_code) => child_has_exited(si_code),
                // Reaped before this wait began: the signal watcher took it
                // after an ending signal. It has exited all the same.
                Err(error) => error.raw_os_error() == Some(libc::ECHILD),
            };
        }
    });
    notice
}

/// Whether the child's exit may end the session before the PTY's EOF, and
/// if so the notice that carries it from the waiter to the read loop.
///
/// On Linux a PTY is not hung up when its session leader exits: a background
/// job the command left behind keeps the slave open, and reading the master
/// to EOF would keep the session (raw mode, no exit status) until that job
/// closes it. With the notice the read loop learns of the exit and ends once
/// a poll of the master shows nothing left queued. That needs two kernel
/// fixes: from Linux 4.7 (commit 0f40fbbcc34e) a poll of the master first
/// lets the kernel hand over output still on its way from the slave, so the
/// command's last bytes cannot look absent; from Linux 4.12 (commit
/// 77dae6134440) a flush on the slave no longer removes bytes the master has
/// already reported readable, so the read that follows such a poll cannot
/// block on a background job. Older kernels, a release that cannot be
/// parsed, and WSL1 (which reports 4.4) keep reading to EOF.
///
/// The early end also assumes the PTY's ordinary line discipline (N_TTY),
/// which hands written output to the master at once. A command that puts its
/// terminal into another discipline that can hold written output back (such
/// as N_HDLC with output stopped) can lose bytes it wrote successfully.
///
/// macOS revokes the terminal when the session leader exits, so EOF arrives
/// at once there and there is no notice.
#[cfg(target_os = "linux")]
fn child_exit_notice() -> Option<Arc<ChildExitNotice>> {
    kernel_release()
        .is_some_and(|release| pty_drain_is_reliable(&release))
        .then(Default::default)
}

#[cfg(not(target_os = "linux"))]
fn child_exit_notice() -> Option<Arc<ChildExitNotice>> {
    None
}

/// Hands the report of the child's exit from the waiter to the pipe the
/// read loop polls. The waiter starts with the child, but the pipe is only
/// made once everything else the session needs is open, since it is the
/// first thing to go without when descriptors run short; either may come
/// first, and whichever comes second writes the byte, once.
#[derive(Default)]
struct ChildExitNotice {
    state: Mutex<ChildExitState>,
}

#[derive(Default)]
enum ChildExitState {
    /// Neither the exit nor the pipe yet.
    #[default]
    Waiting,
    /// The child has exited; the pipe is not there yet.
    Exited,
    /// The wait ended without a real exit; the pipe is not there yet.
    Unknown,
    /// The pipe's write end, waiting for the exit. Only Linux installs one.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    Writer(OwnedFd),
    /// Settled: the byte was written, or the write end closed without one.
    Done,
}

impl ChildExitNotice {
    /// The waiter's report: whether the child really exited.
    fn report(&self, exited: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = match mem::take(&mut *state) {
            ChildExitState::Waiting if exited => ChildExitState::Exited,
            ChildExitState::Waiting => ChildExitState::Unknown,
            ChildExitState::Writer(exit_write) => {
                if exited {
                    write_exit_byte(exit_write);
                }
                ChildExitState::Done
            }
            settled => settled,
        };
    }

    /// Gives the notice the pipe's write end, to write to now if the child
    /// has already exited, or once it does. A write end that comes after an
    /// unknown end of the wait is closed without a byte.
    #[cfg(any(target_os = "linux", test))]
    fn install(&self, exit_write: OwnedFd) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = match mem::take(&mut *state) {
            ChildExitState::Waiting => ChildExitState::Writer(exit_write),
            ChildExitState::Exited => {
                write_exit_byte(exit_write);
                ChildExitState::Done
            }
            _ => ChildExitState::Done,
        };
    }
}

/// The waiter's side of a [`ChildExitNotice`]. It reports when it is
/// dropped, so a waiter thread that never ran reports an unknown end.
struct ChildExitReporter {
    notice: Arc<ChildExitNotice>,
    exited: bool,
}

impl Drop for ChildExitReporter {
    fn drop(&mut self) {
        self.notice.report(self.exited);
    }
}

/// Writes the one byte that reports the child's exit and closes the pipe.
fn write_exit_byte(exit_write: OwnedFd) {
    let mut exit_write = std::fs::File::from(exit_write);
    loop {
        // An unread pipe end that is already closed only fails the write.
        match exit_write.write(&[1]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            _ => return,
        }
    }
}

/// Whether a waitid `si_code` reports the end of the process: a normal exit,
/// death by a signal, or death by a signal with a core dump. waitid also
/// reports a child that made prismtty its tracer when it stops, and that
/// child is alive.
fn child_has_exited(si_code: libc::c_int) -> bool {
    matches!(
        si_code,
        libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
    )
}

/// The running kernel's release, as `uname -r` prints it.
#[cfg(target_os = "linux")]
fn kernel_release() -> Option<String> {
    // SAFETY: utsname is a plain C struct of byte arrays; all-zero bytes are a
    // valid value that uname overwrites.
    let mut name = unsafe { mem::zeroed::<libc::utsname>() };
    // SAFETY: `name` is a valid, exclusively borrowed utsname for the call.
    if unsafe { libc::uname(&mut name) } != 0 {
        return None;
    }
    // SAFETY: uname filled `release` with a NUL-terminated string that lives
    // as long as `name`.
    let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) };
    Some(release.to_string_lossy().into_owned())
}

/// Whether a Linux kernel release (`6.8.0-110-generic`) is 4.12 or newer, the
/// first release with both fixes [`child_exit_notice`] needs. A release that
/// does not start with `major.minor` counts as older.
#[cfg(any(target_os = "linux", test))]
fn pty_drain_is_reliable(release: &str) -> bool {
    let mut parts = release.splitn(2, '.');
    let major = parts.next().unwrap_or_default();
    let minor_digits = parts
        .next()
        .unwrap_or_default()
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .unwrap_or_default();
    if major.is_empty() || !major.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match (major.parse::<u64>(), minor_digits.parse::<u64>()) {
        (Ok(major), Ok(minor)) => (major, minor) >= (4, 12),
        _ => false,
    }
}

/// How many bytes the reader still returns once it has seen the child exit:
/// what the PTY has queued then (about 20 KiB at most on Linux 6.8) and
/// whatever a leftover background job adds while it is read. The limit only
/// ends a session whose leftover job never stops writing.
#[cfg(any(target_os = "linux", test))]
const CHILD_EXIT_READ_LIMIT: usize = 1024 * 1024;

/// What a poll of the PTY master reported.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
enum MasterReadiness {
    /// Output to read, with or without a hangup behind it.
    Data,
    /// Every slave descriptor is closed and nothing is left to read. A
    /// blocking read is not safe here: a process that opens the slave again
    /// before it starts would make it wait for that process.
    HungUp,
    /// Nothing to read yet.
    Quiet,
    /// The descriptor is invalid or reported an error.
    Failed,
}

#[cfg(any(target_os = "linux", test))]
fn master_readiness(revents: libc::c_short) -> MasterReadiness {
    if revents & libc::POLLNVAL != 0 {
        MasterReadiness::Failed
    } else if revents & libc::POLLIN != 0 {
        MasterReadiness::Data
    } else if revents & libc::POLLERR != 0 {
        MasterReadiness::Failed
    } else if revents & libc::POLLHUP != 0 {
        MasterReadiness::HungUp
    } else {
        MasterReadiness::Quiet
    }
}

/// Polls `fds` for input and returns each one's `revents`, retrying a poll a
/// signal interrupted.
#[cfg(any(target_os = "linux", test))]
fn poll_input<const N: usize>(
    fds: [RawFd; N],
    timeout_ms: libc::c_int,
) -> io::Result<[libc::c_short; N]> {
    let mut poll_fds = fds.map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        // SAFETY: `poll_fds` is a valid, exclusively borrowed array of N
        // initialized entries and the count passed matches it.
        let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), N as libc::nfds_t, timeout_ms) };
        if ready >= 0 {
            return Ok(poll_fds.map(|poll_fd| poll_fd.revents));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn master_poll_failed() -> io::Error {
    io::Error::other("polling the PTY master failed")
}

/// Reads the PTY master like the plain reader until the wrapped child has
/// exited, and after that only what the PTY still has queued, so the session
/// ends with the child even while something else keeps the slave open (see
/// [`child_exit_notice`]). `master` is the descriptor `inner` reads, polled
/// as well. The child's exit arrives as one byte on `exit_read`; a pipe
/// closed without a byte leaves plain reads to EOF.
#[cfg(any(target_os = "linux", test))]
struct ChildExitReader<R, E = std::fs::File> {
    inner: R,
    master: RawFd,
    exit_read: Option<E>,
    /// Bytes still to return once the exit is seen; `None` until then.
    remaining: Option<usize>,
    exit_seen: Arc<AtomicBool>,
}

#[cfg(any(target_os = "linux", test))]
impl<R: Read, E: Read + AsRawFd> ChildExitReader<R, E> {
    fn new(inner: R, master: RawFd, exit_read: E, exit_seen: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            master,
            exit_read: Some(exit_read),
            remaining: None,
            exit_seen,
        }
    }

    /// One read once the exit is seen: what is queued now, never more than
    /// the bytes still allowed, and the end once nothing is queued.
    fn read_queued(&mut self, buf: &mut [u8], remaining: usize) -> io::Result<usize> {
        let [master] = poll_input([self.master], 0)?;
        match master_readiness(master) {
            MasterReadiness::Data => {
                let limit = buf.len().min(remaining);
                let read = self.inner.read(&mut buf[..limit])?;
                self.remaining = Some(if read == 0 { 0 } else { remaining - read });
                Ok(read)
            }
            MasterReadiness::HungUp | MasterReadiness::Quiet => {
                self.remaining = Some(0);
                Ok(0)
            }
            MasterReadiness::Failed => Err(master_poll_failed()),
        }
    }
}

/// Reads the one byte of the exit pipe: true for the byte, false for a pipe
/// closed without one.
#[cfg(any(target_os = "linux", test))]
fn read_exit_byte(exit_read: &mut impl Read) -> io::Result<bool> {
    let mut byte = [0_u8; 1];
    loop {
        match exit_read.read(&mut byte) {
            Ok(read) => return Ok(read == 1),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl<R: Read, E: Read + AsRawFd> Read for ChildExitReader<R, E> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(remaining) = self.remaining {
                if remaining == 0 {
                    return Ok(0);
                }
                return self.read_queued(buf, remaining);
            }
            let Some(exit_read) = self.exit_read.as_mut() else {
                return self.inner.read(buf);
            };
            let [master, exit] = poll_input([self.master, exit_read.as_raw_fd()], -1)?;
            if exit != 0 {
                // The exit is taken first even when the same poll also shows
                // output: that output is then read, and counted, as queued.
                // A master that looked empty in this poll is polled again
                // before the end, since the child's last write may have
                // landed after it.
                if read_exit_byte(exit_read)? {
                    self.remaining = Some(CHILD_EXIT_READ_LIMIT);
                    self.exit_seen.store(true, Ordering::Release);
                } else {
                    self.exit_read = None;
                }
                continue;
            }
            match master_readiness(master) {
                MasterReadiness::Data => return self.inner.read(buf),
                MasterReadiness::HungUp => {
                    self.remaining = Some(0);
                    return Ok(0);
                }
                MasterReadiness::Quiet => {}
                MasterReadiness::Failed => return Err(master_poll_failed()),
            }
        }
    }
}

/// The PTY master read as portable-pty's reader reads it, where EIO (every
/// slave descriptor closed) is the end, from a descriptor of its own that
/// [`ChildExitReader`] can also poll.
#[cfg(target_os = "linux")]
struct MasterFile(std::fs::File);

#[cfg(target_os = "linux")]
impl Read for MasterFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
            result => result,
        }
    }
}

/// The reader of the PTY master, and the descriptor it reads when that is
/// needed to end at the child's exit: on Linux with a [`ChildExitNotice`], a
/// duplicate of the master of its own, which costs the same one descriptor
/// as portable-pty's reader; otherwise portable-pty's reader, as before.
#[cfg(target_os = "linux")]
fn master_reader(
    master: &dyn portable_pty::MasterPty,
    child_exit: Option<&Arc<ChildExitNotice>>,
) -> anyhow::Result<(Box<dyn Read + Send>, Option<RawFd>)> {
    if child_exit.is_none() {
        return Ok((master.try_clone_reader()?, None));
    }
    let file = std::fs::File::from(dup_master_fd(master)?);
    let fd = file.as_raw_fd();
    Ok((Box::new(MasterFile(file)), Some(fd)))
}

#[cfg(not(target_os = "linux"))]
fn master_reader(
    master: &dyn portable_pty::MasterPty,
    _child_exit: Option<&Arc<ChildExitNotice>>,
) -> anyhow::Result<(Box<dyn Read + Send>, Option<RawFd>)> {
    Ok((master.try_clone_reader()?, None))
}

/// Puts [`ChildExitReader`] in front of the master reader so that the session
/// ends at the child's exit, and tells the session so that it stops holding
/// back output for the child once it has exited. It runs after everything
/// else the session needs is open, and the pipe is all it adds: without one
/// (no descriptors left, or no notice) the reader reads to EOF as before.
#[cfg(target_os = "linux")]
fn end_reads_at_child_exit(
    reader: Box<dyn Read + Send>,
    master: Option<RawFd>,
    child_exit: Option<Arc<ChildExitNotice>>,
    prepared: &mut PreparedSession<'_>,
) -> Box<dyn Read + Send> {
    let (Some(master), Some(child_exit)) = (master, child_exit) else {
        return reader;
    };
    let Ok((exit_read, exit_write)) = pipe_fds() else {
        return reader;
    };
    // SAFETY: both descriptors were just created by pipe_fds and nothing else
    // owns them.
    let (exit_read, exit_write) = unsafe {
        (
            OwnedFd::from_raw_fd(exit_read),
            OwnedFd::from_raw_fd(exit_write),
        )
    };
    child_exit.install(exit_write);
    let exit_seen = Arc::new(AtomicBool::new(false));
    prepared.end_holdback_at_child_exit(Arc::clone(&exit_seen));
    Box::new(ChildExitReader::new(
        reader,
        master,
        std::fs::File::from(exit_read),
        exit_seen,
    ))
}

/// The terminal ends the session when the child exits, so the reader reads
/// to EOF unchanged.
#[cfg(not(target_os = "linux"))]
fn end_reads_at_child_exit(
    reader: Box<dyn Read + Send>,
    _master: Option<RawFd>,
    _child_exit: Option<Arc<ChildExitNotice>>,
    _prepared: &mut PreparedSession<'_>,
) -> Box<dyn Read + Send> {
    reader
}

/// How the wrapped child ended: the exit byte prismtty reports for it, and
/// the raw wait status when prismtty reaped it with waitpid.
struct ChildExit {
    code: u8,
    wait_status: Option<libc::c_int>,
}

/// Whether a raw wait status says the process was killed by SIGINT.
fn killed_by_sigint(wait_status: Option<libc::c_int>) -> bool {
    wait_status
        .is_some_and(|status| libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGINT)
}

/// Reaps the wrapped child and returns how it ended. Waits without reaping
/// first so the process-group id remains signalable while the PTY read loop is
/// ending, then serializes the final reap with signal forwarding. This closes
/// the otherwise orphan-prone gap between PTY EOF and child exit.
fn reap_child(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
) -> Result<ChildExit, CliError> {
    if let Some(pid) = child.process_id() {
        let pid = pid as libc::pid_t;
        // This wait fails with ECHILD when the signal watcher reaped the
        // child first; the outcome it published says how the child ended.
        let waited = wait_for_child_exit_without_reaping(pid);
        let mut reap_state = forward_child_lock();
        if let ReapState::Ended(outcome) = *reap_state {
            return Ok(outcome.child_exit());
        }
        waited?;
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid, exclusively borrowed c_int for the call.
        let reaped = if unsafe { libc::waitpid(pid, &mut status, 0) } == -1 {
            Err(io::Error::last_os_error())
        } else {
            *reap_state = ReapState::Reaped(status);
            Ok(ChildExit {
                code: exit_code_from_wait(status),
                wait_status: Some(status),
            })
        };
        FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
        return Ok(reaped?);
    }
    let status = child.wait()?;
    Ok(ChildExit {
        code: exit_code_byte(status.exit_code()),
        wait_status: None,
    })
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
    terminate_and_reap_process_group(pid as libc::pid_t, libc::SIGHUP, Instant::now() + END_LIMIT);
}

/// Returns the leader's raw wait status when this call reaped it. The leader
/// has `CHILD_EXIT_GRACE` to exit, never past `deadline`; then the group is
/// killed and the leader reaped until `deadline`.
fn terminate_and_reap_process_group(
    pid: libc::pid_t,
    initial_signal: libc::c_int,
    deadline: Instant,
) -> Option<libc::c_int> {
    // SAFETY: kill has no memory-safety preconditions; `pid` is the wrapped session leader, so
    // `-pid` is its process group.
    unsafe {
        libc::kill(-pid, initial_signal);
    }
    let mut status: libc::c_int = 0;
    let grace_end = (Instant::now() + CHILD_EXIT_GRACE).min(deadline);
    while Instant::now() < grace_end {
        match child_exit_is_waitable(pid) {
            Ok(true) => {
                // Keep the leader waitable until the final group signal so its
                // process-group id cannot be reused while descendants remain.
                // SAFETY: kill has no memory-safety preconditions; the leader is still unreaped, so
                // its group id has not been reused.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                // SAFETY: `status` is a valid, exclusively borrowed c_int for the call.
                let reaped = unsafe { libc::waitpid(pid, &mut status, 0) } == pid;
                return reaped.then_some(status);
            }
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return None,
            Ok(false) | Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }

    // SAFETY: kill has no memory-safety preconditions; the group id and pid belong to the still-
    // unreaped wrapped child.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
    while Instant::now() < deadline {
        // SAFETY: `status` is a valid, exclusively borrowed c_int for the call.
        match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
            0 => thread::sleep(Duration::from_millis(20)),
            reaped if reaped == pid => return Some(status),
            _ if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) => break,
            _ => thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

/// Claims failure-path cleanup under the same lock used by signal forwarding.
/// A signal watcher either wins the lock and owns termination, or waits until
/// this path has reaped the child and cleared the registered pid. A child
/// that was already reaped is left alone: its group was torn down then, and
/// its group id may since belong to other processes.
fn terminate_and_reap_registered_child(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let reap_state = forward_child_lock();
    if *reap_state != ReapState::Unreaped {
        return;
    }
    FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
    terminate_and_reap_child(child);
    drop(reap_state);
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

/// Passes a message signal to the command prismtty started, and to nothing
/// else in its group, while that command is registered and not yet reaped.
fn forward_message_to_command(signal: libc::c_int) {
    let _guard = forward_child_lock();
    let pid = FORWARD_CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill has no memory-safety preconditions; `pid` was read under
        // `forward_child_lock`, which serializes it with reaping.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// Forwards an ending signal to the wrapped PTY process group, allows a
/// bounded graceful exit, then kills any signal-immune descendants and reaps
/// the process-group leader, as 1.4.0 did, all by `deadline`. Publishes the
/// outcome in the same critical section as the reap, and returns it. The
/// leader's status is the one this reap took, or the one `reap_child` left
/// when the main thread reaped the leader first, in which case no group
/// signal is sent.
fn end_registered_child(
    signal: libc::c_int,
    deadline: Instant,
    normal_end_bounded: bool,
) -> EndOutcome {
    let mut reap_state = forward_child_lock();
    let pid = FORWARD_CHILD_PID.load(Ordering::SeqCst);
    let leader_status = if pid > 0 {
        FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
        terminate_and_reap_process_group(pid, signal, deadline)
    } else if let ReapState::Reaped(status) = *reap_state {
        Some(status)
    } else {
        None
    };
    let outcome = EndOutcome {
        signal,
        leader_status,
        deadline,
        owner: end_owner(leader_status, normal_end_bounded),
    };
    publish_end_outcome(&mut reap_state, outcome);
    outcome
}

/// Publishes the watcher's outcome, under the reap lock that `reap_state`
/// holds. The command's status goes out first in `PUBLISHED_LEADER_STATUS`,
/// which the limit's watchdog reads without that lock.
fn publish_end_outcome(reap_state: &mut ReapState, outcome: EndOutcome) {
    PUBLISHED_LEADER_STATUS.store(
        encode_published_status(outcome.leader_status),
        Ordering::SeqCst,
    );
    *reap_state = ReapState::Ended(outcome);
}

/// What the limit's watchdog needs of the published outcome, which is all
/// `end_abruptly` decides from besides the signal the watchdog already has:
/// the command's raw wait status, or that the command was not collected.
/// It is one value, written once by `publish_end_outcome` before the outcome
/// under the reap lock, so a watcher delayed while it still holds that lock
/// cannot keep it from the watchdog. 0 until then; `set_forward_child_pid`
/// clears it for the next child.
static PUBLISHED_LEADER_STATUS: AtomicU64 = AtomicU64::new(0);
const STATUS_PUBLISHED: u64 = 1 << 63;
const STATUS_COLLECTED: u64 = 1 << 62;

fn encode_published_status(leader_status: Option<libc::c_int>) -> u64 {
    STATUS_PUBLISHED | leader_status.map_or(0, |status| STATUS_COLLECTED | u64::from(status as u32))
}

/// The command's status as the watcher published it: `None` before it did,
/// `Some(None)` when it could not collect the command.
fn published_leader_status() -> Option<Option<libc::c_int>> {
    published_leader_status_from(PUBLISHED_LEADER_STATUS.load(Ordering::SeqCst))
}

fn published_leader_status_from(value: u64) -> Option<Option<libc::c_int>> {
    (value & STATUS_PUBLISHED != 0)
        .then(|| (value & STATUS_COLLECTED != 0).then_some(value as u32 as libc::c_int))
}

/// Who ends the process once the watcher has taken an ending signal: the main
/// thread after a normal end, or an abrupt exit from the watcher or the
/// limit's watchdog. Only the first claim counts.
static EXIT_CLAIM: AtomicI32 = AtomicI32::new(0);
const EXIT_AFTER_NORMAL_END: i32 = 1;
const EXIT_ABRUPTLY: i32 = 2;

fn claim_exit(by: i32) -> bool {
    EXIT_CLAIM
        .compare_exchange(0, by, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Waits for the thread that claimed the exit to end the process.
fn wait_for_the_claimed_exit() -> ! {
    loop {
        thread::park();
    }
}

/// Bounds the end of a session from outside every thread that can block in
/// it: at `deadline`, unless the main thread has claimed the exit, the
/// process ends abruptly (see `end_at_deadline`). Returns false when its
/// thread cannot be started; the end is then abrupt at once.
fn start_end_watchdog(signal: libc::c_int, deadline: Instant, sigint_left_default: bool) -> bool {
    thread::Builder::new()
        .name("end limit".to_owned())
        .spawn(move || {
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                thread::sleep(deadline - now);
            }
            end_at_deadline(signal, sigint_left_default);
        })
        .is_ok()
}

/// The limit's end, at the deadline. It ends the way the command's status
/// that the watcher published decides, as `end_abruptly` does for the
/// watcher, so a SIGINT that killed the command still ends prismtty by
/// SIGINT; it reads that status without the reap lock. When the watcher has
/// published nothing by the deadline (it has not collected the command, or
/// collected it only an instant before and not yet published), the end is
/// 128 plus `signal`.
fn end_at_deadline(signal: libc::c_int, sigint_left_default: bool) {
    let outcome = EndOutcome {
        signal,
        leader_status: published_leader_status().flatten(),
        deadline: Instant::now(),
        owner: EndOwner::Watcher,
    };
    end_abruptly(&outcome, sigint_left_default);
}

/// Ends the process at once after an ending signal, as 1.4.0 did. SIGINT that
/// killed the command it was forwarded to (Ctrl-C with stdin redirected,
/// `kill -INT`) ends prismtty the same way; anything else exits with 128 plus
/// the signal prismtty was sent, whatever the command then died from.
fn end_abruptly(outcome: &EndOutcome, sigint_left_default: bool) -> ! {
    if !claim_exit(EXIT_ABRUPTLY) {
        wait_for_the_claimed_exit();
    }
    // This runs on the watcher, the limit's or the stopping thread, not in a
    // signal handler.
    unregister_before_exit();
    if outcome.signal == libc::SIGINT
        && sigint_left_default
        && killed_by_sigint(outcome.leader_status)
    {
        die_from_signal(libc::SIGINT);
    }
    // SAFETY: _exit never returns and skips destructors by design: the terminal was restored,
    // the child reaped or given up on, and the pid file removed above.
    unsafe { libc::_exit(128 + outcome.signal) }
}

/// The watcher's side of an ending signal. The limit is armed first, before
/// anything here that can block; then the terminal is restored and the group
/// torn down as in 1.4.0. A command that exited by itself leaves the rest of
/// the end to the main thread; anything else ends the process here.
fn end_session_after_signal(
    signal: libc::c_int,
    raw_mode_state: &Mutex<RawModeLifecycle>,
    sigint_left_default: bool,
) {
    #[cfg(test)]
    pause_at_end_entry_for_test();
    let deadline = Instant::now() + END_LIMIT;
    let bounded = start_end_watchdog(signal, deadline, sigint_left_default);
    if let Err(error) = terminate_raw_mode_from_signal_state(raw_mode_state) {
        warn(format_args!(
            "prismtty: could not restore terminal before exit: {error}"
        ));
    }
    let outcome = end_registered_child(signal, deadline, bounded);
    if outcome.owner == EndOwner::Watcher {
        end_abruptly(&outcome, sigint_left_default);
    }
}

/// How the process ends after the watcher took an ending signal, decided by
/// the main thread from the one published outcome once the watcher has been
/// joined. `normal_end` is the command's code when the normal end completed.
/// The main thread claims the exit only after its last step that can block,
/// and when it loses the claim the watchdog is ending the process.
fn exit_after_end_signal(outcome: &EndOutcome, normal_end: Option<u8>) -> ExitCode {
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    let (claim, code) = match normal_end {
        Some(code) if outcome.owner == EndOwner::NormalEnd && Instant::now() < outcome.deadline => {
            (EXIT_AFTER_NORMAL_END, code)
        }
        _ => (EXIT_ABRUPTLY, outcome.abrupt_code()),
    };
    if !claim_exit(claim) {
        wait_for_the_claimed_exit();
    }
    ExitCode::from(code)
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
    super::diagnostic(args);
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

/// How a wrapped session ends, decided once everything it opened is closed.
enum SessionEnd {
    /// Exit with this status byte.
    Exit(u8),
    /// The command was killed by SIGINT: end the same way.
    Interrupted,
}

pub(super) fn run_command(options: Options, command: Vec<OsString>) -> Result<ExitCode, CliError> {
    // Read before prismtty blocks or handles any signal, so this is the
    // caller's choice and not prismtty's own.
    let sigint_left_default = caller_left_sigint_default();
    let session = run_session(options, command, sigint_left_default);
    // Read once the signal watcher has been joined: when it took an ending
    // signal, its outcome decides how this process ends. A session that
    // failed after that ends abruptly, without an error message, as 1.4.0's
    // watcher ended it.
    if let Some(outcome) = published_end_outcome() {
        let normal_end = match session {
            Ok(SessionEnd::Exit(code)) => Some(code),
            _ => None,
        };
        return Ok(exit_after_end_signal(&outcome, normal_end));
    }
    match session? {
        SessionEnd::Exit(code) => Ok(ExitCode::from(code)),
        SessionEnd::Interrupted => {
            // The session's locals are dropped: the terminal is restored, the
            // pid file removed and the trace closed. What remains buffered is
            // written before the signal ends the process.
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            die_from_signal(libc::SIGINT)
        }
    }
}

fn run_session(
    options: Options,
    command: Vec<OsString>,
    sigint_left_default: bool,
) -> Result<SessionEnd, CliError> {
    // Listed before prismtty opens anything of its own, so these are the
    // descriptors the caller handed over (`cmd 3<file`), which the command
    // inherits as it would without the wrapper.
    let caller_fds = inherited_fds();
    let command_name = command[0].clone();
    let command_args = command[1..].to_vec();
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(current_pty_size())?;
    let interactive = io::stdin().is_terminal();
    // Output on a terminal is watched live even when stdin is redirected
    // (`ptty cmd </dev/null`), so it gets the interactive highlighter and the
    // holdback flush, as stdin mode does when stdout is a terminal. Raw mode,
    // stdin forwarding and dynamic profiles stay keyed to `interactive`.
    let output_on_terminal = io::stdout().is_terminal();
    let live_output = interactive || output_on_terminal;
    let plain_output = output_is_saved_as_written(interactive, output_on_terminal);
    // Everything that can reject the invocation runs before the child exists,
    // so a typo in a flag or a broken config never runs part of the command,
    // and a reload warning prints before the terminal goes raw.
    let mut prepared = prepare_session(&options, live_output)?;
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
        configure_child_pty(&*pair.master, plain_output)?;
    } else {
        queue_end_of_input(&*pair.master)?;
        if plain_output {
            pass_child_output_through(&*pair.master)?;
        }
    }

    let mut signal_mask = SessionSignalMask::block()?;
    let mut terminal_session = TerminalSession::start(interactive, sigint_left_default)?;
    let mut child = if interactive && pair.master.tty_name().is_none() {
        // The slave cannot be opened by name (seen with musl in a chroot
        // without /proc), so portable-pty spawns the command from its own
        // slave descriptor. Where it can list /dev/fd it closes every
        // descriptor above stderr in the child: the caller's do not reach
        // the command, and with std's exec-error pipe closed an execve
        // failure aborts the child. Where it cannot, it closes none.
        pair.slave
            .spawn_command(builder)
            .map_err(|source| CliError::Spawn {
                code: spawn_failure_exit_code(&program, cwd.as_deref()),
                source,
            })?
    } else {
        spawn_with_pty_slave(&builder, &*pair.master, interactive, &caller_fds)?
    };
    let child_exit = close_slave_after_child_exit(pair.slave, child.process_id());
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

    let (reader, reader_fd) = cleanup_child_on_err(
        master_reader(&*pair.master, child_exit.as_ref()),
        &mut child,
    )?;
    let mut resize_watcher =
        cleanup_child_on_err(PtyResizeWatcher::start(pair.master), &mut child)?;
    let mut reader = end_reads_at_child_exit(reader, reader_fd, child_exit, &mut prepared);

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
        Ok(ChildExit {
            code: 1,
            wait_status: None,
        })
    } else {
        reap_child(&mut child)
    };
    let input_worker_failure = take_input_worker_failure(&input_worker_failure);
    terminal_session.stop();
    if let Some(error) = input_worker_failure {
        return Err(error.into());
    }
    stream_result?;
    let exit = exit?;
    // A shell stops a loop after Ctrl-C only when the job it waited for was
    // killed by SIGINT, so prismtty ends that way when its command did. Not
    // when the caller ignored or blocked SIGINT: it chose that this process
    // does not die from it.
    if sigint_left_default && killed_by_sigint(exit.wait_status) {
        return Ok(SessionEnd::Interrupted);
    }
    Ok(SessionEnd::Exit(exit.code))
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
    /// How much of the file every version of the kernel reads.
    buffer_bytes: usize,
    /// The bytes that end the line.
    line_ends: &'static [u8],
    /// Whether a line with no end in the buffer still names an interpreter
    /// when a blank follows the name, which only cuts off its arguments.
    /// Otherwise the kernel reports ENOEXEC, and execvp runs the file with
    /// /bin/sh.
    names_in_unended_line: bool,
}

/// Linux, `load_script` in fs/binfmt_script.c: the newline search (strnchr)
/// stops at a NUL, so the line of a short file with no newline ends where the
/// file does. `#` is part of the name. The buffer is BINPRM_BUF_SIZE, 256
/// bytes since Linux 5.1 but 128 before, where a name that does not end in
/// it is ENOEXEC (5.0, 4.x stable) or cut off there (older kernels). Reading
/// 128 bytes leaves such a name to the spawn on every version.
const LINUX_SHEBANG: ShebangSyntax = ShebangSyntax {
    buffer_bytes: 128,
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
/// whose interpreter cannot be executed only fails in execve, after the fork,
/// where the spawn can report no more than the errno: ENOENT reads as if the
/// script itself were missing. This names the interpreter instead. It also
/// covers the spawn by portable-pty when the slave has no name, which closes
/// std's exec-error pipe in the child where it can list /dev/fd, so the child
/// would abort with "fatal runtime error" instead. The line is parsed as the
/// kernel does, so a CRLF script names `/bin/sh\r`. Whatever the kernel would
/// not reject for certain is left to the spawn: an empty name, a relative one
/// when the child's directory is not known, and every script on a kernel
/// whose syntax is not known here.
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

/// Spawns the wrapped command with the PTY slave as its stdout, stderr and
/// controlling terminal, and as its stdin when `interactive`. Otherwise the
/// child reads the caller's stdin itself, so every read sees what
/// `cmd < file` would: the data of a pipe or file, then end of input, however
/// often it reads. This mirrors portable-pty's spawn: the program is resolved
/// against the same directory and PATH (`resolve_program` says where its
/// messages differ), the environment is the builder's, and the child starts
/// as a session leader with default signal dispositions and an empty signal
/// mask.
///
/// Unlike portable-pty, which closes every descriptor above stderr it finds
/// in /dev/fd in the child, this keeps the ones in `caller_fds` (sorted, as
/// `inherited_fds` lists them) for the command, as a shell does, and marks
/// any other that would survive the exec close-on-exec. That marking also
/// needs a readable /dev/fd; where there is none, nothing is marked, and
/// what prismtty opens itself still closes at the exec because it is opened
/// close-on-exec. None is closed, so std's exec-error pipe keeps working and
/// an execve failure is reported instead of aborting the child.
fn spawn_with_pty_slave(
    builder: &CommandBuilder,
    master: &dyn portable_pty::MasterPty,
    interactive: bool,
    caller_fds: &[RawFd],
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
    // What prismtty opens itself is close-on-exec, so this is expected to be
    // empty; it keeps a descriptor some library left inheritable out of the
    // command.
    let own_fds: Vec<RawFd> = inherited_fds()
        .into_iter()
        .filter(|fd| caller_fds.binary_search(fd).is_err())
        .collect();
    // SAFETY: sigset_t is a plain C type; sigemptyset below initializes it before it is used.
    let mut empty_mask = unsafe { mem::zeroed::<libc::sigset_t>() };
    // SAFETY: `empty_mask` is a valid, exclusively borrowed sigset_t.
    unsafe {
        libc::sigemptyset(&mut empty_mask);
    }

    let stdin = if interactive {
        Stdio::from(slave.try_clone()?)
    } else {
        Stdio::inherit()
    };
    let mut command = Command::new(&program);
    command
        .arg0(&argv[0])
        .args(&argv[1..])
        .current_dir(&dir)
        .stdin(stdin)
        .stdout(slave.try_clone()?)
        .stderr(slave);
    command.env_clear();
    for key in builder_env_keys(builder) {
        if let Some(value) = builder.get_env(&key) {
            command.env(&key, value);
        }
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
            for &fd in &own_fds {
                if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
                    let error = io::Error::last_os_error();
                    // A descriptor closed since it was listed is no leak.
                    if error.raw_os_error() != Some(libc::EBADF) {
                        return Err(error);
                    }
                }
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

/// Where the child starts when it has no usable directory. See
/// `choose_home_dir`.
fn home_dir(builder: &CommandBuilder) -> PathBuf {
    choose_home_dir(builder.get_env("HOME"), passwd_home)
}

/// $HOME as it is whenever it is UTF-8, as portable-pty takes it, so an
/// empty, relative or missing one is tried and fails to be entered. Else the
/// user's home directory in the passwd database, as it is even when it is
/// not UTF-8 (portable-pty fails the spawn there). Else, when the lookup
/// fails or finds no entry or an empty home, `/`, as the lookup prismtty
/// used before (dirs-sys) did.
fn choose_home_dir(
    home: Option<&OsStr>,
    passwd_home: impl FnOnce() -> io::Result<Option<OsString>>,
) -> PathBuf {
    if let Some(home) = home.and_then(OsStr::to_str) {
        return PathBuf::from(home);
    }
    match passwd_home() {
        Ok(Some(dir)) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/"),
    }
}

/// The user's home directory in the passwd database, looked up as dirs-sys
/// does: one getpwuid_r with a buffer of _SC_GETPW_R_SIZE_MAX bytes, or 512
/// when that is not known. A home too long for the buffer fails the lookup.
/// None when there is no entry or the entry has no home field.
fn passwd_home() -> io::Result<Option<OsString>> {
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: sysconf has no memory-safety preconditions.
    let size = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n < 0 => 512,
        n => n as usize,
    };
    let mut buffer = vec![0 as libc::c_char; size];
    // SAFETY: passwd is a plain C struct; getpwuid_r fills it before it is read.
    let mut entry = unsafe { mem::zeroed::<libc::passwd>() };
    let mut found = std::ptr::null_mut();
    // SAFETY: `entry`, `buffer` and `found` are valid and exclusively borrowed
    // for the call, and the length passed is the buffer's own.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    if found.is_null() || entry.pw_dir.is_null() {
        return Ok(None);
    }
    // SAFETY: the lookup succeeded, so pw_dir points to a NUL-terminated
    // string in `buffer`, which is still alive.
    let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    Ok(Some(OsStr::from_bytes(dir.to_bytes()).to_owned()))
}

/// The names of every variable the builder's environment can hold: the
/// process's own, SHELL, which the builder sets when it is unset, and the
/// ones set on the builder (the iTerm guard's). The builder lists only
/// UTF-8 entries, so the guard's copies of the parent's values are named
/// here as well. Each is looked up in the builder, so a removed variable
/// stays out and a value that is not UTF-8 passes as it is, as in
/// portable-pty's spawn.
fn builder_env_keys(builder: &CommandBuilder) -> Vec<OsString> {
    let mut keys: Vec<OsString> = std::env::vars_os().map(|(key, _)| key).collect();
    keys.push(OsString::from("SHELL"));
    keys.extend(
        builder
            .iter_extra_env_as_str()
            .map(|(key, _)| OsString::from(key)),
    );
    keys.extend(STRIPPED_ITERM_ENV.map(|key| OsString::from(format!("PRISMTTY_PARENT_{key}"))));
    keys
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
        // A program built for another CPU type.
        #[cfg(target_vendor = "apple")]
        Some(libc::EBADARCH) => 126,
        _ => 1,
    }
}

/// Descriptors above stderr that would survive an exec, in ascending order.
/// Like portable-pty, this lists /dev/fd, and finds nothing where that cannot
/// be read. Listing allocates, so it runs before the fork.
fn inherited_fds() -> Vec<RawFd> {
    let Ok(entries) = std::fs::read_dir("/dev/fd") else {
        return Vec::new();
    };
    let mut fds: Vec<RawFd> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<RawFd>().ok())
        .filter(|&fd| fd > 2)
        .filter(|&fd| {
            // SAFETY: F_GETFD takes no pointer argument; an invalid `fd` reports EBADF instead of
            // touching memory.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            flags >= 0 && flags & libc::FD_CLOEXEC == 0
        })
        .collect();
    fds.sort_unstable();
    fds
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
fn configure_child_pty(
    master: &dyn portable_pty::MasterPty,
    plain_output: bool,
) -> Result<(), CliError> {
    let stdin = io::stdin();
    let source = tcgetattr(stdin.as_fd())?;
    let Some(tty_name) = master.tty_name() else {
        // Seen with musl in a chroot without /proc: the PTY works but its
        // name cannot be found. The master reaches the same settings.
        if plain_output {
            pass_child_output_through(master)?;
        }
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
    if plain_output {
        // See `pass_child_output_through`. The flags copied from the terminal
        // can also ask for tabs as spaces, so the whole step goes.
        termios.output_flags.remove(OutputFlags::OPOST);
    }
    tcsetattr(slave_tty.as_fd(), SetArg::TCSANOW, &termios)?;
    Ok(())
}

/// Whether the command's PTY should start without the kernel's output
/// conversions, so that prismtty's output has the line ends the command
/// wrote. A new PTY puts a CR in front of every LF, which a raw terminal on
/// the other side needs and a saved file does not.
///
/// Not when the keyboard is on a terminal and the output goes into a pipe.
/// That terminal is raw for the whole session, and the pipe usually leads
/// back to it (`ptty ssh host | tee log`). The command's PTY is then the only
/// place that knows whether a LF means a new line or, for a full-screen
/// program that turned the conversion off, one row down; converting anywhere
/// else draws one of the two wrong.
fn output_is_saved_as_written(interactive: bool, output_on_terminal: bool) -> bool {
    !output_on_terminal && (!interactive || stdout_is_regular_file())
}

/// Through std's metadata and not `libc::fstat`: on a 32-bit glibc build the
/// plain call fails with EOVERFLOW for a file of 2 GiB or more, and a log
/// would change from LF to CR LF line ends when it grew past that size.
fn stdout_is_regular_file() -> bool {
    io::stdout()
        .as_fd()
        .try_clone_to_owned()
        .and_then(|fd| std::fs::File::from(fd).metadata())
        .is_ok_and(|metadata| metadata.is_file())
}

/// Turns off the kernel's output conversions on the child's PTY (see
/// `output_is_saved_as_written`). The command can still turn them back on
/// itself.
fn pass_child_output_through(master: &dyn portable_pty::MasterPty) -> io::Result<()> {
    let pty = stdin_forward_writer(master)?;
    let fd = pty.as_raw_fd();
    let mut attrs = terminal_attrs(fd)?;
    attrs.c_oflag &= !libc::OPOST;
    set_terminal_attrs_retrying_eintr(fd, &attrs)
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
pub(super) struct WaitWhenWouldBlock<T> {
    pub(super) inner: T,
    pub(super) fd: RawFd,
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
    // SS3: an arrow or function key in application mode, `ESC O` plus one
    // final byte. Anything else after `ESC O` is not a key: the `O` and what
    // follows are handled as typed.
    if *next == b'O' {
        return match input.get(idx + 2) {
            None => LocalEchoStep::Incomplete,
            Some(0x40..=0x7e) => LocalEchoStep::Skip(idx + 3),
            Some(_) => LocalEchoStep::Skip(idx + 1),
        };
    }
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
    if is_end_signal(signal) {
        PENDING_END.record(signal);
    }
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
    sigint_left_default: bool,
}

impl SessionSignalWatcher {
    fn start(raw_mode_state: SharedRawModeState, sigint_left_default: bool) -> io::Result<Self> {
        ensure_child_is_reapable()?;
        PENDING_END.clear();
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
            handle_session_signals(read_fd, thread_state, sigint_left_default)
        });
        Ok(Self {
            write_fd,
            thread: Some(thread),
            previous_signal_handlers,
            raw_mode_state,
            sigint_left_default,
        })
    }

    fn stop(&mut self) {
        if let Err(error) = terminate_raw_mode_from_signal_state(&self.raw_mode_state) {
            warn(format_args!(
                "prismtty: could not restore terminal during cleanup: {error}"
            ));
        }
        // See `PendingEnd` for the handoff. With an End recorded (taken or
        // not, published or not) the process is on its way out: its signals
        // stay caught, with nothing left to act on them, so a second SIGTERM
        // or a late SIGUSR1 cannot kill it by signal before it has removed
        // its pid file and returned its status. With none, the signals are
        // given back first, as in 1.4.1.
        let handlers = mem::take(&mut self.previous_signal_handlers);
        let mut given_back = false;
        if PENDING_END.get().is_none() {
            restore_signal_handlers(&handlers);
            given_back = true;
            if PENDING_END.get().is_some() {
                // Recorded while they were being given back.
                catch_signals_again(&handlers);
                given_back = false;
            }
        }
        // Wait out any handler that is mid-write before the descriptor is
        // closed (or leak it if one stalls).
        let closable = RAW_MODE_SIGNAL_PIPE.disarm();
        if self.write_fd >= 0 {
            write_signal_byte(self.write_fd, RAW_SIGNAL_STOP_BYTE);
            if closable {
                close_fd(self.write_fd);
            }
            self.write_fd = -1;
        }
        let Some(thread) = self.thread.take() else {
            return;
        };
        let _ = thread.join();
        // An End that a handler recorded and the watcher did not take (it
        // came after the watcher's last check) is taken here, on the stopping
        // thread: the session is ending, its command has been reaped or given
        // up on, and that decides the status as for any end.
        if PENDING_END.get().is_some() {
            if given_back {
                catch_signals_again(&handlers);
            }
            if let Some(signal) = PENDING_END.take() {
                end_session_after_signal(signal, &self.raw_mode_state, self.sigint_left_default);
            }
        }
    }
}

impl Drop for SessionSignalWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Puts prismtty's handler back on the signals in `handlers`, once an End
/// was recorded after they were given back. The self-pipe is disarmed by
/// then, so the handler only records, and the End is already recorded.
fn catch_signals_again(handlers: &[(libc::c_int, libc::sigaction)]) {
    for (signal, _) in handlers {
        let _ = install_signal_handler(*signal, restore_raw_mode_for_signal, libc::SA_RESTART);
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
    fn start(interactive: bool, sigint_left_default: bool) -> Result<Self, CliError> {
        let raw_mode_state = Arc::new(Mutex::new(RawModeLifecycle::Inactive));
        let signal_watcher =
            SessionSignalWatcher::start(Arc::clone(&raw_mode_state), sigint_left_default)?;
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

fn handle_session_signals(
    read_fd: RawFd,
    raw_mode_state: SharedRawModeState,
    sigint_left_default: bool,
) {
    handle_session_signals_with(
        read_fd,
        &PENDING_END,
        &raw_mode_state,
        || {
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
        },
        forward_message_to_command,
        |signal| end_session_after_signal(signal, &raw_mode_state, sigint_left_default),
    );
}

/// Acts on the signal numbers the handlers queue in the self-pipe until the
/// stop byte arrives. An ending signal recorded in `pending_end` outranks
/// every queued byte: `end_session` takes it, and the watcher stops once that
/// returns (the main thread then finishes the session). A message queued
/// behind it is not delivered. `forward_message` passes on SIGUSR1 and
/// SIGUSR2, and `stop_for_job_control` performs one supervised stop and
/// returns once the wrapper has been continued.
fn handle_session_signals_with(
    read_fd: RawFd,
    pending_end: &PendingEnd,
    raw_mode_state: &Mutex<RawModeLifecycle>,
    mut stop_for_job_control: impl FnMut() -> io::Result<()>,
    mut forward_message: impl FnMut(libc::c_int),
    mut end_session: impl FnMut(libc::c_int),
) {
    let mut pending = VecDeque::new();
    let mut resumed_at = None;
    loop {
        if pending_end.get().is_some() {
            // Taken here, or already by another thread, which then ends
            // the session.
            if let Some(signal) = pending_end.take() {
                end_session(signal);
            }
            break;
        }
        let Some(signal) = pending.pop_front() else {
            match read_signal_bytes(read_fd) {
                Some(bytes) => {
                    pending.extend(bytes);
                    continue;
                }
                None => break,
            }
        };
        if signal == RAW_SIGNAL_STOP_BYTE {
            #[cfg(test)]
            record_end_at_stop_for_test(pending_end);
            break;
        }
        let signal = libc::c_int::from(signal);
        if is_end_signal(signal) {
            // The handler recorded it before it wrote this byte; a byte that
            // came alone counts all the same.
            pending_end.record(signal);
            continue;
        }
        if is_message_signal(signal) {
            forward_message(signal);
            continue;
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
            // Stop requests queued before the resume were satisfied by this
            // stop, just as SIGCONT discards pending stop signals. Replaying
            // them would stop the wrapper again right after `fg`: a
            // background read of the terminal raises SIGTTIN each time it
            // restarts, queueing a burst of them.
            pending.extend(ready_signal_bytes(read_fd));
            pending.retain(|byte| !JOB_CONTROL_SIGNALS.contains(&libc::c_int::from(*byte)));
            resumed_at = Some(Instant::now());
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

/// Whether the caller left SIGINT at its default action and unblocked, as a
/// shell starts a foreground command. A caller that ignored it (`cmd &` in a
/// script), blocked it, or catches it with a handler of its own (a program
/// that runs prismtty in process) chose what SIGINT does to this process.
fn caller_left_sigint_default() -> bool {
    // SAFETY: sigset_t is a plain C type; pthread_sigmask writes it before it is read.
    let mut blocked = unsafe { mem::zeroed::<libc::sigset_t>() };
    // SAFETY: a null new-set pointer only queries the calling thread's mask into `blocked`.
    let queried =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut blocked) } == 0;
    // SAFETY: `blocked` was written by pthread_sigmask above.
    let unblocked = queried && unsafe { libc::sigismember(&blocked, libc::SIGINT) } == 0;
    // SAFETY: sigaction is a plain C struct; the sigaction call fills it before it is read.
    let mut current = unsafe { mem::zeroed::<libc::sigaction>() };
    // SAFETY: a null new-action pointer only queries the disposition into `current`.
    let queried = unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current) } == 0;
    unblocked && queried && current.sa_sigaction == libc::SIG_DFL
}

/// Ends prismtty by `signal`'s default action, as the wrapped command ended,
/// so whoever waits for prismtty sees the wait status it would have seen for
/// the command. Only for a signal the caller left at its default action and
/// unblocked, once prismtty has cleaned up. The signal is raised on the
/// calling thread and unblocked there, which delivers it before
/// pthread_sigmask returns; a process-directed kill could instead be taken by
/// another thread while this one went on. Should that return, prismtty exits
/// with 128 plus the signal, the status it reported before.
fn die_from_signal(signal: libc::c_int) -> ! {
    // SAFETY: sigaction and sigset_t are plain C types, initialized before use; every call takes
    // valid pointers to these locals, and _exit never returns.
    unsafe {
        let mut default = mem::zeroed::<libc::sigaction>();
        default.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut default.sa_mask);
        libc::sigaction(signal, &default, std::ptr::null_mut());
        let mut only = mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut only);
        libc::sigaddset(&mut only, signal);
        libc::raise(signal);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
        libc::_exit(128 + signal)
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
        use std::os::unix::process::ExitStatusExt;

        let raw_status = |script: &str| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .status()
                .expect("run sh")
                .into_raw()
        };
        assert_eq!(super::exit_code_from_wait(raw_status("exit 0")), 0);
        assert_eq!(super::exit_code_from_wait(raw_status("exit 42")), 42);
        assert_eq!(super::exit_code_from_wait(raw_status("kill -TERM $$")), 143);
        assert_eq!(super::exit_code_from_wait(raw_status("kill -KILL $$")), 137);
        #[cfg(target_os = "linux")]
        assert_eq!(super::exit_code_from_wait(raw_status("kill -64 $$")), 192);
    }

    #[test]
    fn only_a_death_by_sigint_counts_as_killed_by_sigint() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        let raw_status = |script: &str| {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(script);
            // SAFETY: the closure runs in the forked child before exec and only calls
            // async-signal-safe functions on a stack-local signal set. A SIGINT the test runner
            // inherited as ignored or blocked must not keep sh alive.
            unsafe {
                command.pre_exec(|| {
                    let mut sigint = std::mem::zeroed::<libc::sigset_t>();
                    libc::sigemptyset(&mut sigint);
                    libc::sigaddset(&mut sigint, libc::SIGINT);
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    libc::sigprocmask(libc::SIG_UNBLOCK, &sigint, std::ptr::null_mut());
                    Ok(())
                });
            }
            command.status().expect("run sh").into_raw()
        };
        assert!(super::killed_by_sigint(Some(raw_status("kill -INT $$"))));
        assert!(!super::killed_by_sigint(Some(raw_status("kill -TERM $$"))));
        assert!(!super::killed_by_sigint(Some(raw_status("exit 130"))));
        assert!(!super::killed_by_sigint(None));
    }

    /// Set, to `<probe>:<state>`, in a copy of this test binary that a test
    /// starts to run one probe in a process of its own: the probes change
    /// process-wide state that tests running in parallel threads would see.
    const PROBE: &str = "PRISMTTY_UNIT_TEST_PROBE";

    /// The state a probe was started for, or `None` when it runs as an
    /// ordinary test of the suite, where it does nothing.
    fn probe_state(probe: &str) -> Option<String> {
        std::env::var(PROBE)
            .ok()?
            .strip_prefix(&format!("{probe}:"))
            .map(str::to_owned)
    }

    /// Runs the probe test `probe` alone in a copy of this test binary, for
    /// `state`, and returns what it printed.
    fn run_probe(probe: &str, state: &str) -> String {
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, path)| path);
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                &format!("{module}::{probe}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE, format!("{probe}:{state}"))
            .output()
            .expect("run the probe");
        let printed = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success() && printed.contains("1 passed"),
            "probe {probe} for {state}: {output:?}"
        );
        printed
    }

    extern "C" fn sigint_caught_by_the_probe(_: libc::c_int) {}

    /// Sets SIGINT for its whole process as `state` says and prints whether
    /// that counts as left at its default.
    #[test]
    fn sigint_policy_probe() {
        let Some(state) = probe_state("sigint_policy_probe") else {
            return;
        };
        // SAFETY: sigaction and sigset_t are plain C types, initialized before use; every call
        // takes valid pointers to these locals, and the handler is an `extern "C" fn(c_int)`
        // that lives for the whole process. Only this probe runs in this process.
        unsafe {
            let mut sigint = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut sigint);
            libc::sigaddset(&mut sigint, libc::SIGINT);
            let mut action = std::mem::zeroed::<libc::sigaction>();
            libc::sigemptyset(&mut action.sa_mask);
            let caught: extern "C" fn(libc::c_int) = sigint_caught_by_the_probe;
            let (handler, how) = match state.as_str() {
                "default" => (libc::SIG_DFL, libc::SIG_UNBLOCK),
                "ignored" => (libc::SIG_IGN, libc::SIG_UNBLOCK),
                "caught" => (caught as libc::sighandler_t, libc::SIG_UNBLOCK),
                "blocked" => (libc::SIG_DFL, libc::SIG_BLOCK),
                other => panic!("unknown SIGINT state {other}"),
            };
            action.sa_sigaction = handler;
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
            libc::pthread_sigmask(how, &sigint, std::ptr::null_mut());
        }
        println!("left-default={}", super::caller_left_sigint_default());
    }

    // A program that embeds prismtty and catches SIGINT itself chose what
    // SIGINT does, as one that ignores or blocks it did. SIGINT's action is
    // process-wide, so each state is set in a process of its own.
    #[test]
    fn sigint_counts_as_left_default_only_at_its_default_action_and_unblocked() {
        for (state, left_default) in [
            ("default", true),
            ("ignored", false),
            ("caught", false),
            ("blocked", false),
        ] {
            let printed = run_probe("sigint_policy_probe", state);
            assert!(
                printed.contains(&format!("left-default={left_default}")),
                "SIGINT {state}: {printed}"
            );
        }
    }

    /// Publishes an earlier child's death by SIGINT, registers the next child
    /// and prints what is published then.
    #[test]
    fn registration_probe() {
        if probe_state("registration_probe").is_none() {
            return;
        }
        *super::forward_child_lock() = super::ReapState::Reaped(libc::SIGINT);
        super::set_forward_child_pid(0);
        println!("published={:?}", *super::forward_child_lock());
    }

    // The status a reap published belongs to that child. Registering the next
    // child must drop it, or a second session in one process could take an
    // earlier session's SIGINT death for its own. The status slot is
    // process-wide and other tests publish to it, so the steps run in a
    // process of their own.
    #[test]
    fn registering_a_child_drops_the_status_published_for_the_last_one() {
        assert!(super::killed_by_sigint(Some(libc::SIGINT)));
        let printed = run_probe("registration_probe", "after an earlier session");
        assert!(printed.contains("published=Unreaped"), "{printed}");
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
    fn home_dir_takes_a_utf8_home_else_a_usable_passwd_home_else_the_root() {
        use std::ffi::{OsStr, OsString};
        use std::os::unix::ffi::OsStrExt;
        use std::path::PathBuf;

        let choose = |home: Option<&[u8]>, passwd: std::io::Result<Option<&[u8]>>| {
            super::choose_home_dir(home.map(OsStr::from_bytes), || {
                passwd.map(|dir| dir.map(|dir| OsStr::from_bytes(dir).to_owned()))
            })
        };
        let path = |bytes: &[u8]| PathBuf::from(OsString::from(OsStr::from_bytes(bytes)));
        let failed = || Err(std::io::Error::from_raw_os_error(libc::ERANGE));

        // HOME as it is whenever it is UTF-8, even when it cannot be entered.
        assert_eq!(choose(Some(b"/home/u"), Ok(Some(b"/p"))), path(b"/home/u"));
        assert_eq!(choose(Some(b""), Ok(Some(b"/p"))), path(b""));
        assert_eq!(choose(Some(b"rel"), Ok(Some(b"/p"))), path(b"rel"));
        // Otherwise the passwd home, as it is.
        assert_eq!(choose(Some(b"/home-\xff"), Ok(Some(b"/p"))), path(b"/p"));
        assert_eq!(choose(None, Ok(Some(b"/p-\xff"))), path(b"/p-\xff"));
        // No usable passwd home: no entry or no home field, an empty one, or a
        // failed lookup (a home too long for the buffer).
        assert_eq!(choose(None, Ok(None)), path(b"/"));
        assert_eq!(choose(None, Ok(Some(b""))), path(b"/"));
        assert_eq!(choose(None, failed()), path(b"/"));
        assert_eq!(choose(Some(b"/home-\xff"), failed()), path(b"/"));
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
        let long_name = format!("/nonexistent/{}", "a".repeat(150));
        write("long.sh", &format!("#!{long_name}\necho ran\n"), 0o755);
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
        // `#` is part of the name, but a name past 128 bytes runs differently
        // by kernel version. macOS ends the line at `#`, reports ENOEXEC for a
        // line with no end at all, and reads 512 bytes on every version.
        #[cfg(target_os = "linux")]
        {
            assert_eq!(code("./short.sh"), Some(127));
            assert_eq!(code("./hash.sh"), Some(127));
            assert_eq!(code("./long.sh"), None);
        }
        #[cfg(target_vendor = "apple")]
        {
            assert_eq!(code("./short.sh"), None);
            assert_eq!(code("./hash.sh"), None);
            assert_eq!(code("./long.sh"), Some(127));
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

    // fs/binfmt_script.c: a 128-byte buffer before Linux 5.1; the line ends at
    // a newline or a NUL; only a blank or a NUL ends the name; with no line
    // end in the buffer, a name followed by a blank still runs.
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
        let name = format!("/{}", "a".repeat(124));
        assert_eq!(linux(&long(125, "\n")).as_deref(), Some(name.as_str()));
        assert_eq!(linux(&long(125, " -x\n")).as_deref(), Some(name.as_str()));
        assert_eq!(linux(&long(126, " -x\n")), None);
        // Linux 5.1 reads 256 bytes and runs this name, but earlier kernels
        // report ENOEXEC or cut it off.
        assert_eq!(linux(&long(126, "\n")), None);
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

    // In application cursor-key mode an arrow or function key arrives as SS3
    // (`ESC O A`, `ESC O P`). Its final byte is part of the key, not text.
    #[test]
    fn local_echo_skips_ss3_keys_whole_even_when_split() {
        assert_eq!(super::local_echo_bytes(b"a\x1bOAb\x1bOPc"), b"abc");

        let mut echo = super::LocalEchoState::default();
        assert!(echo.push(b"\x1bO").is_empty());
        assert_eq!(echo.push(b"Ax"), b"x");

        // Only a final byte completes the key. Escape, `O`, Enter typed one
        // after the other must still echo the line break.
        assert_eq!(super::local_echo_bytes(b"x\x1bO\ry\r"), b"xO\r\ny\r\n");
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
            .find("spawn_with_pty_slave(")
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

        super::handle_session_signals_with(
            read_fd,
            &super::PendingEnd::new(),
            &state,
            || {
                stops += 1;
                Ok(())
            },
            |_| panic!("no message was sent"),
            |_| panic!("no ending signal was sent"),
        );

        assert_eq!(stops, 1, "stale stop requests stopped the wrapper again");
        super::close_fd(write_fd);
    }

    // The self-pipe only wakes the watcher, and a burst of messages can fill
    // it so that the byte of a later ending signal is lost. The ending signal
    // is recorded beside the pipe, and it is taken before any queued message.
    #[test]
    fn an_ending_signal_outranks_queued_messages_and_a_full_signal_pipe() {
        let (read_fd, write_fd) = super::pipe_fds().expect("signal pipe");
        super::set_fd_nonblocking(write_fd).expect("nonblocking write end");
        let message = [libc::SIGUSR1 as u8];
        let mut queued = 0;
        // SAFETY: `message` is valid for reads of one byte and `write_fd` is our pipe end.
        while unsafe { libc::write(write_fd, message.as_ptr().cast(), 1) } == 1 {
            queued += 1;
        }
        let pending_end = super::PendingEnd::new();
        pending_end.record(libc::SIGTERM);
        // The handler's byte for the ending signal cannot be written now.
        super::write_signal_byte(write_fd, libc::SIGTERM as u8);
        let state = Mutex::new(super::RawModeLifecycle::Inactive);
        let mut forwarded = 0;
        let mut ended = Vec::new();

        super::handle_session_signals_with(
            read_fd,
            &pending_end,
            &state,
            || panic!("no job-control signal was sent"),
            |_| forwarded += 1,
            |signal| ended.push(signal),
        );

        assert!(queued > 0, "the pipe took no message byte");
        assert_eq!(ended, [libc::SIGTERM]);
        assert_eq!(forwarded, 0, "messages were forwarded before the end");
        super::close_fd(write_fd);
    }

    // An ending signal that arrives while messages are queued is taken before
    // the rest of them.
    #[test]
    fn an_ending_signal_is_taken_before_the_messages_still_queued() {
        let (read_fd, write_fd) = super::pipe_fds().expect("signal pipe");
        for _ in 0..32 {
            super::write_signal_byte(write_fd, libc::SIGUSR1 as u8);
        }
        let pending_end = super::PendingEnd::new();
        let state = Mutex::new(super::RawModeLifecycle::Inactive);
        let mut forwarded = 0;
        let mut ended = Vec::new();

        super::handle_session_signals_with(
            read_fd,
            &pending_end,
            &state,
            || panic!("no job-control signal was sent"),
            |_| {
                forwarded += 1;
                // SIGTERM arrives while the first message is passed on.
                if forwarded == 1 {
                    pending_end.record(libc::SIGTERM);
                }
            },
            |signal| ended.push(signal),
        );

        assert_eq!(ended, [libc::SIGTERM]);
        assert_eq!(
            forwarded, 1,
            "queued messages were passed on before the end"
        );
        super::close_fd(write_fd);
    }

    // A byte of an ending signal that nothing recorded still ends the session,
    // after the messages queued before it.
    #[test]
    fn an_ending_signal_byte_alone_ends_the_session_after_earlier_messages() {
        let (read_fd, write_fd) = super::pipe_fds().expect("signal pipe");
        for byte in [libc::SIGUSR1, libc::SIGUSR2, libc::SIGHUP, libc::SIGUSR1] {
            super::write_signal_byte(write_fd, byte as u8);
        }
        let state = Mutex::new(super::RawModeLifecycle::Inactive);
        let mut forwarded = Vec::new();
        let mut ended = Vec::new();

        super::handle_session_signals_with(
            read_fd,
            &super::PendingEnd::new(),
            &state,
            || panic!("no job-control signal was sent"),
            |signal| forwarded.push(signal),
            |signal| ended.push(signal),
        );

        assert_eq!(forwarded, [libc::SIGUSR1, libc::SIGUSR2]);
        assert_eq!(ended, [libc::SIGHUP]);
        super::close_fd(write_fd);
    }

    // Two threads can race to take the same End: the watcher at the top of
    // its loop and the stopping thread after joining it. Only one wins, and
    // a watcher that finds the End taken stops without acting on it.
    #[test]
    fn a_recorded_end_is_taken_by_one_thread_only() {
        let pending_end = super::PendingEnd::new();
        assert_eq!(pending_end.take(), None);
        pending_end.record(libc::SIGTERM);
        let start = std::sync::Barrier::new(4);
        let taken: Vec<_> = std::thread::scope(|scope| {
            let takers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        pending_end.take()
                    })
                })
                .collect();
            takers
                .into_iter()
                .filter_map(|taker| taker.join().ok().flatten())
                .collect()
        });
        assert_eq!(taken, [libc::SIGTERM]);
        assert_eq!(pending_end.get(), Some(libc::SIGTERM));

        let (read_fd, write_fd) = super::pipe_fds().expect("signal pipe");
        super::write_signal_byte(write_fd, libc::SIGUSR1 as u8);
        let state = Mutex::new(super::RawModeLifecycle::Inactive);
        let mut forwarded = 0;
        super::handle_session_signals_with(
            read_fd,
            &pending_end,
            &state,
            || panic!("no job-control signal was sent"),
            |_| forwarded += 1,
            |_| panic!("an End another thread took was taken again"),
        );
        assert_eq!(forwarded, 0);
        super::close_fd(write_fd);
    }

    #[test]
    fn the_first_ending_signal_recorded_wins() {
        let pending_end = super::PendingEnd::new();
        assert_eq!(pending_end.get(), None);
        pending_end.record(libc::SIGTERM);
        pending_end.record(libc::SIGHUP);
        assert_eq!(pending_end.get(), Some(libc::SIGTERM));
    }

    #[test]
    fn messages_are_sigusr1_and_sigusr2_and_every_other_caught_signal_ends_the_session() {
        for signal in super::TERMINATING_SIGNALS {
            let message = signal == libc::SIGUSR1 || signal == libc::SIGUSR2;
            assert_eq!(super::is_message_signal(signal), message, "signal {signal}");
            assert_eq!(super::is_end_signal(signal), !message, "signal {signal}");
        }
        for signal in [libc::SIGCONT, libc::SIGTSTP, libc::SIGWINCH, libc::SIGKILL] {
            assert!(!super::is_end_signal(signal), "signal {signal}");
            assert!(!super::is_message_signal(signal), "signal {signal}");
        }
    }

    // The raw wait status decides: an ordinary exit, whenever it happened and
    // whatever its code, leaves the end to the main thread, as long as the
    // watchdog bounds it. A signal death, or no status at all, does not.
    #[test]
    fn the_command_exited_by_itself_only_with_an_ordinary_exit_status() {
        use super::EndOwner::{NormalEnd, Watcher};

        let exited = |code: libc::c_int| code << 8;
        assert_eq!(super::end_owner(Some(exited(0)), true), NormalEnd);
        assert_eq!(super::end_owner(Some(exited(7)), true), NormalEnd);
        assert_eq!(super::end_owner(Some(exited(137)), true), NormalEnd);
        assert_eq!(super::end_owner(Some(exited(7)), false), Watcher);
        assert_eq!(super::end_owner(Some(libc::SIGTERM), true), Watcher);
        assert_eq!(super::end_owner(Some(libc::SIGKILL), true), Watcher);
        assert_eq!(super::end_owner(None, true), Watcher);
        assert!(libc::WIFEXITED(exited(7)) && libc::WEXITSTATUS(exited(7)) == 7);
    }

    /// An outcome as the watcher publishes it for a command that exited 0.
    fn ended_with_exit_0() -> super::ReapState {
        super::ReapState::Ended(super::EndOutcome {
            signal: libc::SIGTERM,
            leader_status: Some(0),
            deadline: Instant::now() + super::END_LIMIT,
            owner: super::EndOwner::NormalEnd,
        })
    }

    /// Spawns `program` with `args` in a session of its own, as the wrapper
    /// spawns its command, and returns it with its PTY, which the caller
    /// keeps open: closing it would hang the command up.
    fn spawn_command(
        program: &str,
        args: &[&str],
    ) -> (
        Box<dyn portable_pty::Child + Send + Sync>,
        portable_pty::PtyPair,
    ) {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .expect("openpty");
        let mut builder = portable_pty::CommandBuilder::new(program);
        builder.args(args);
        builder.cwd("/");
        let child =
            super::spawn_with_pty_slave(&builder, &*pair.master, false, &[]).expect("spawn");
        (child, pair)
    }

    /// Publishes the watcher's end while a live decoy is registered, then runs
    /// the failure cleanup of a stream error, and prints whether the decoy
    /// was signalled.
    #[test]
    fn cleanup_after_end_probe() {
        if probe_state("cleanup_after_end_probe").is_none() {
            return;
        }
        let (mut child, _pty) = spawn_command("sleep", &["5"]);
        let pid = child.process_id().expect("decoy pid") as libc::pid_t;
        super::set_forward_child_pid(pid);
        {
            let mut reap_state = super::forward_child_lock();
            super::FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
            *reap_state = ended_with_exit_0();
        }
        super::terminate_and_reap_registered_child(&mut child);
        std::thread::sleep(Duration::from_millis(300));
        // Alive means neither waitable nor reaped.
        // SAFETY: siginfo_t is a plain C struct; all-zero bytes are a valid value that waitid
        // overwrites.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: `info` is valid for the call; WNOWAIT leaves the decoy unreaped.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        // SAFETY: waitid returned 0, so `info` is populated.
        let alive = waited == 0 && unsafe { info.si_pid() } == 0;
        println!("decoy-exited={}", !alive);
        // SAFETY: kill has no memory-safety preconditions; `pid` is this probe's own child,
        // not reaped yet.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
        let _ = child.wait();
    }

    // After the watcher reaped the command and killed its group, a stream
    // error's cleanup must not signal that group again: its id may belong to
    // other processes by then. The decoy is a live command in that place.
    #[test]
    fn cleanup_after_the_watcher_ended_the_session_signals_nothing() {
        let printed = run_probe("cleanup_after_end_probe", "after the watcher's reap");
        assert!(printed.contains("decoy-exited=false"), "{printed}");
    }

    /// Reaps a finished command the way the watcher does, publishes the
    /// outcome, and prints what the main thread's reap makes of it.
    #[test]
    fn reap_after_end_probe() {
        if probe_state("reap_after_end_probe").is_none() {
            return;
        }
        let (mut child, _pty) = spawn_command("sh", &["-c", "exit 7"]);
        let pid = child.process_id().expect("command pid") as libc::pid_t;
        super::set_forward_child_pid(pid);
        let mut status = 0;
        {
            let mut reap_state = super::forward_child_lock();
            super::FORWARD_CHILD_PID.store(0, Ordering::SeqCst);
            // SAFETY: `status` is a valid, exclusively borrowed c_int for the call.
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            *reap_state = super::ReapState::Ended(super::EndOutcome {
                signal: libc::SIGTERM,
                leader_status: Some(status),
                deadline: Instant::now() + super::END_LIMIT,
                owner: super::end_owner(Some(status), true),
            });
        }
        match super::reap_child(&mut child) {
            Ok(exit) => println!("code={} status={:?}", exit.code, exit.wait_status),
            Err(error) => println!("error={error}"),
        }
    }

    /// Runs the probe test `probe` alone in a copy of this test binary, for
    /// `state`, and returns how that process ended and what it printed. For a
    /// probe that ends its process itself.
    fn run_ending_probe(probe: &str, state: &str) -> (std::process::ExitStatus, String) {
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, path)| path);
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                &format!("{module}::{probe}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE, format!("{probe}:{state}"))
            .output()
            .expect("run the probe");
        (
            output.status,
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    /// Publishes the watcher's outcome for a command that SIGINT killed, the
    /// signal received being INT or TERM, and then runs the limit's end at
    /// the deadline: as when the watcher, delayed after its reap, has not yet
    /// ended the process itself. With `LOCKED_INT` the watcher is delayed
    /// while it still holds the reap lock, for longer than the limit would
    /// ever wait for it.
    #[test]
    fn end_at_deadline_probe() {
        let Some(state) = probe_state("end_at_deadline_probe") else {
            return;
        };
        let signal = if state == "TERM" {
            libc::SIGTERM
        } else {
            libc::SIGINT
        };
        let outcome = super::EndOutcome {
            signal,
            leader_status: Some(libc::SIGINT),
            deadline: Instant::now(),
            owner: super::EndOwner::Watcher,
        };
        let mut reap_state = super::forward_child_lock();
        super::publish_end_outcome(&mut reap_state, outcome);
        if state == "LOCKED_INT" {
            let limit = std::thread::spawn(move || super::end_at_deadline(signal, true));
            std::thread::sleep(Duration::from_millis(1500));
            drop(reap_state);
            let _ = limit.join();
        } else {
            drop(reap_state);
            super::end_at_deadline(signal, true);
        }
        println!("the limit did not end the process");
    }

    #[test]
    fn the_status_published_for_the_limit_reads_back_as_written() {
        assert_eq!(super::published_leader_status_from(0), None);
        for status in [
            None,
            Some(0),
            Some(libc::SIGINT),
            Some(7 << 8),
            Some(137 << 8),
        ] {
            let encoded = super::encode_published_status(status);
            assert_eq!(super::published_leader_status_from(encoded), Some(status));
        }
    }

    /// Starts the signal watcher as a session does, publishes that the main
    /// thread reaped the command (`exit 0`, or `killed` by SIGKILL), and stops
    /// the watcher while a SIGTERM is recorded at its stop byte: after the
    /// watcher's last check, as a handler can record one while the session
    /// stops. Prints the outcome published then.
    #[test]
    fn end_at_stop_probe() {
        let Some(state) = probe_state("end_at_stop_probe") else {
            return;
        };
        let raw_mode = Arc::new(Mutex::new(super::RawModeLifecycle::Inactive));
        let mut watcher =
            super::SessionSignalWatcher::start(Arc::clone(&raw_mode), true).expect("watcher");
        let status = if state == "killed" { libc::SIGKILL } else { 0 };
        *super::forward_child_lock() = super::ReapState::Reaped(status);
        super::END_TO_RECORD_AT_STOP.store(libc::SIGTERM, Ordering::SeqCst);
        watcher.stop();
        println!("outcome={:?}", super::published_end_outcome());
    }

    // An ending signal that a handler records while the session stops, after
    // the watcher's last check, is still taken: its command was reaped, so
    // the command's status decides as for any end. A command that exited 0
    // gives 0 through the normal end; one that was killed gives 128 plus the
    // signal received.
    #[test]
    fn an_ending_signal_recorded_while_the_session_stops_is_taken() {
        use std::os::unix::process::ExitStatusExt;

        let printed = run_probe("end_at_stop_probe", "exit 0");
        assert!(
            printed.contains("signal: 15")
                && printed.contains("leader_status: Some(0)")
                && printed.contains("NormalEnd"),
            "{printed}"
        );
        let (status, printed) = run_ending_probe("end_at_stop_probe", "killed");
        assert_eq!(
            (status.signal(), status.code()),
            (None, Some(128 + libc::SIGTERM)),
            "{printed}"
        );
    }

    static PRIOR_USR1_RAN: AtomicBool = AtomicBool::new(false);

    extern "C" fn prior_usr1(_: libc::c_int) {
        PRIOR_USR1_RAN.store(true, Ordering::SeqCst);
    }

    /// Gives SIGUSR1 an action of its own, as a caller can, starts the signal
    /// watcher, lets it take SIGTERM and holds that End at the entry of its
    /// path, before anything is published, and meanwhile stops the session
    /// from another thread. Prints whether a SIGUSR1 sent then ran the prior
    /// action and whether the End was published yet, then lets the End go on,
    /// which ends the process.
    #[test]
    fn stop_during_end_probe() {
        if probe_state("stop_during_end_probe").is_none() {
            return;
        }
        let prior: extern "C" fn(libc::c_int) = prior_usr1;
        // SIGTERM at its default and unblocked, whatever the runner left, so
        // that the watcher catches it; the watcher thread inherits this
        // thread's mask.
        // SAFETY: the handler is an `extern "C" fn(c_int)` that only stores to an atomic; the set
        // is a stack-local sigset_t initialized before use. Only this probe runs in this process.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGUSR1, prior as libc::sighandler_t);
            let mut set = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGUSR1);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        }
        let raw_mode = Arc::new(Mutex::new(super::RawModeLifecycle::Inactive));
        let watcher = super::SessionSignalWatcher::start(raw_mode, true).expect("watcher");
        super::END_ENTRY_PAUSE.armed.store(true, Ordering::SeqCst);
        // SAFETY: kill has no memory-safety preconditions; the target is this probe's own process.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGTERM);
        }
        let until = Instant::now() + Duration::from_secs(5);
        while !super::END_ENTRY_PAUSE.entered.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        let entered = super::END_ENTRY_PAUSE.entered.load(Ordering::SeqCst);
        let stopper = std::thread::spawn(move || {
            let mut watcher = watcher;
            watcher.stop();
        });
        // Time for stop to give the signals back, or keep them, and then
        // wait for the watcher.
        std::thread::sleep(Duration::from_millis(300));
        // SAFETY: kill has no memory-safety preconditions; the target is this probe's own process.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGUSR1);
        }
        std::thread::sleep(Duration::from_millis(100));
        println!(
            "entered={entered} prior_ran={} unpublished={}",
            PRIOR_USR1_RAN.load(Ordering::SeqCst),
            super::published_end_outcome().is_none()
        );
        super::END_ENTRY_PAUSE
            .released
            .store(true, Ordering::SeqCst);
        let _ = stopper.join();
        println!("the End did not end the process");
    }

    // While an End is in progress (taken, not yet published), stopping the
    // session keeps the signals caught: a SIGUSR1 with a default action
    // could otherwise kill PrismTTY before its pid file is removed and its
    // status returned. The End then ends the process with 128 plus SIGTERM.
    #[test]
    fn signals_stay_caught_while_an_end_is_in_progress() {
        use std::os::unix::process::ExitStatusExt;

        let (status, printed) = run_ending_probe("stop_during_end_probe", "TERM");
        assert!(
            printed.contains("entered=true prior_ran=false unpublished=true"),
            "{printed}"
        );
        assert_eq!(
            (status.signal(), status.code()),
            (None, Some(128 + libc::SIGTERM)),
            "{printed}"
        );
    }

    // When the limit ends the process, the 1.4.0 SIGINT rule still holds: a
    // SIGINT that PrismTTY was sent and that killed the command ends PrismTTY
    // by SIGINT; with any other signal it exits with 128 plus that signal.
    #[test]
    fn the_limit_ends_by_sigint_when_the_sigint_it_was_sent_killed_the_command() {
        use std::os::unix::process::ExitStatusExt;

        let (status, printed) = run_ending_probe("end_at_deadline_probe", "INT");
        assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?} {printed}");
        // The same while the watcher, delayed, still holds the reap lock.
        let (status, printed) = run_ending_probe("end_at_deadline_probe", "LOCKED_INT");
        assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?} {printed}");
        let (status, printed) = run_ending_probe("end_at_deadline_probe", "TERM");
        assert_eq!(
            (status.signal(), status.code()),
            (None, Some(128 + libc::SIGTERM)),
            "{printed}"
        );
    }

    // The main thread's reap finds the command gone (ECHILD) when the watcher
    // reaped it first; it takes the code from the outcome published with that
    // reap instead of failing.
    #[test]
    fn the_main_thread_takes_the_code_from_the_watchers_reap() {
        let printed = run_probe("reap_after_end_probe", "after the watcher's reap");
        assert!(printed.contains("code=7"), "{printed}");
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
    fn output_of_a_child_that_exits_before_the_slave_is_closed_is_read() {
        use std::io::Read;

        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .expect("openpty");
        let mut builder = portable_pty::CommandBuilder::new("printf");
        builder.arg("short-lived output");
        builder.cwd("/");
        let mut child =
            super::spawn_with_pty_slave(&builder, &*pair.master, false, &[]).expect("spawn printf");
        // Let the child write and exit while the parent still holds the slave,
        // as a short command can before the parent reaches the close.
        std::thread::sleep(Duration::from_millis(300));
        super::close_slave_after_child_exit(pair.slave, child.process_id());

        let mut reader = pair.master.try_clone_reader().expect("master reader");
        let mut output = Vec::new();
        let mut buffer = [0u8; 256];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // EIO: every slave descriptor is closed (Linux).
                Err(_) => break,
            }
        }
        assert_eq!(super::reap_child(&mut child).expect("reap printf").code, 0);
        assert_eq!(output, b"short-lived output");

        let source = include_str!("pty.rs");
        let run_command = source
            .split("pub(super) fn run_command")
            .nth(1)
            .expect("run_command exists")
            .split("fn child_working_directory")
            .next()
            .expect("run_command boundary exists");
        assert!(run_command.contains("close_slave_after_child_exit(pair.slave"));
        assert!(!run_command.contains("drop(pair.slave)"));
    }

    /// A pipe as two owned files: (read end, write end).
    fn test_pipe() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;

        let (read, write) = super::pipe_fds().expect("pipe");
        // SAFETY: both descriptors were just created by pipe_fds and nothing
        // else owns them.
        unsafe {
            (
                std::fs::File::from_raw_fd(read),
                std::fs::File::from_raw_fd(write),
            )
        }
    }

    /// An exit pipe whose byte is already written and whose writer is closed,
    /// as the waiter leaves it after the child has exited.
    fn reported_exit() -> std::fs::File {
        use std::io::Write;

        let (read, mut write) = test_pipe();
        write.write_all(&[1]).expect("write the exit byte");
        drop(write);
        read
    }

    /// A reader over `file` that polls `file` itself, as on Linux the reader
    /// polls the master descriptor it reads.
    fn exit_reader(
        file: std::fs::File,
        exit_read: std::fs::File,
        exit_seen: &Arc<AtomicBool>,
    ) -> super::ChildExitReader<std::fs::File> {
        let master = std::os::fd::AsRawFd::as_raw_fd(&file);
        super::ChildExitReader::new(file, master, exit_read, Arc::clone(exit_seen))
    }

    // When the exit and output are both ready in one poll, the reader takes
    // the exit first, so the read that follows counts against the limit. A
    // regular file stands in for a master that always has more to read, as
    // under a leftover job that never stops writing: the reader stops at
    // exactly the limit, a read is cut to what is left, and the end stays.
    #[test]
    fn child_exit_reader_stops_at_exactly_the_limit_counting_from_the_first_read() {
        use std::io::{Read, Seek, Write};

        let mut file = tempfile::tempfile().expect("temp file");
        file.write_all(&vec![b'x'; 2 * super::CHILD_EXIT_READ_LIMIT])
            .expect("fill the file");
        file.rewind().expect("rewind");
        let exit_seen = Arc::new(AtomicBool::new(false));
        let mut reader = exit_reader(file, reported_exit(), &exit_seen);

        // 7000 does not divide the limit, so the last read is cut short.
        let mut buffer = vec![0_u8; 7000];
        let first = reader.read(&mut buffer).expect("first read");
        assert!(first > 0, "the first read returned nothing");
        assert!(
            exit_seen.load(Ordering::Acquire),
            "the exit was not taken before the output of the same poll"
        );
        let mut total = first;
        loop {
            let read = reader.read(&mut buffer).expect("read");
            if read == 0 {
                break;
            }
            total += read;
            assert!(
                total <= super::CHILD_EXIT_READ_LIMIT,
                "read past the limit: {total}"
            );
        }
        assert_eq!(total, super::CHILD_EXIT_READ_LIMIT);
        for _ in 0..3 {
            assert_eq!(reader.read(&mut buffer).expect("read after the end"), 0);
        }
    }

    // After the exit, the reader returns what is queued and then ends; the
    // end stays even when more output arrives later.
    #[test]
    fn child_exit_reader_returns_queued_output_then_ends_for_good() {
        use std::io::{Read, Write};

        let (master_read, mut master_write) = test_pipe();
        master_write.write_all(b"last").expect("queue output");
        let exit_seen = Arc::new(AtomicBool::new(false));
        let mut reader = exit_reader(master_read, reported_exit(), &exit_seen);
        let mut buffer = [0_u8; 64];
        let read = reader.read(&mut buffer).expect("read queued output");
        assert_eq!(&buffer[..read], b"last");
        assert!(exit_seen.load(Ordering::Acquire));
        assert_eq!(reader.read(&mut buffer).expect("read at the end"), 0);
        master_write
            .write_all(b"late")
            .expect("write after the end");
        assert_eq!(reader.read(&mut buffer).expect("read after the end"), 0);
    }

    // An exit seen on its own, with nothing queued, ends the read.
    #[test]
    fn child_exit_reader_ends_when_the_exit_comes_with_nothing_queued() {
        use std::io::Read;

        let (master_read, _master_write) = test_pipe();
        let exit_seen = Arc::new(AtomicBool::new(false));
        let mut reader = exit_reader(master_read, reported_exit(), &exit_seen);
        let mut buffer = [0_u8; 64];
        assert_eq!(reader.read(&mut buffer).expect("read"), 0);
        assert!(exit_seen.load(Ordering::Acquire));
    }

    // The poll that shows the exit can show the master still empty while the
    // child's last write is about to land. The reader polls the master again
    // after taking the exit, so output that arrives in between is read, and
    // counted, instead of being cut off by the earlier, empty sample.
    #[test]
    fn child_exit_reader_reads_output_that_lands_after_the_exit_was_polled() {
        use std::io::{Read, Write};

        /// The exit pipe; reading its byte, which the reader does only after
        /// the poll that found it, makes the master readable.
        struct ExitThenOutput {
            exit_read: std::fs::File,
            master_write: Option<std::fs::File>,
        }
        impl Read for ExitThenOutput {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if let Some(mut master_write) = self.master_write.take() {
                    master_write.write_all(b"lastword")?;
                }
                self.exit_read.read(buf)
            }
        }
        impl std::os::fd::AsRawFd for ExitThenOutput {
            fn as_raw_fd(&self) -> RawFd {
                self.exit_read.as_raw_fd()
            }
        }

        let (master_read, master_write) = test_pipe();
        let master = std::os::fd::AsRawFd::as_raw_fd(&master_read);
        let exit_seen = Arc::new(AtomicBool::new(false));
        let mut reader = super::ChildExitReader::new(
            master_read,
            master,
            ExitThenOutput {
                exit_read: reported_exit(),
                master_write: Some(master_write),
            },
            Arc::clone(&exit_seen),
        );
        let mut buffer = [0_u8; 64];
        let read = reader.read(&mut buffer).expect("read");
        assert_eq!(&buffer[..read], b"lastword");
        assert!(exit_seen.load(Ordering::Acquire));
        assert_eq!(
            reader.remaining,
            Some(super::CHILD_EXIT_READ_LIMIT - b"lastword".len())
        );
        assert_eq!(reader.read(&mut buffer).expect("read at the end"), 0);
    }

    // A waiter that could not report the exit closes the pipe without a
    // byte. The reader then reads to the master's EOF exactly as without it,
    // and stops polling the dead pipe.
    #[test]
    fn child_exit_reader_without_an_exit_byte_reads_to_eof() {
        use std::io::{Read, Write};

        let (exit_read, exit_write) = test_pipe();
        drop(exit_write);
        let (master_read, mut master_write) = test_pipe();
        let exit_seen = Arc::new(AtomicBool::new(false));
        let mut reader = exit_reader(master_read, exit_read, &exit_seen);
        master_write.write_all(b"abc").expect("write");
        // A reader that kept polling the dead pipe would spin instead of
        // returning, so the read runs on a thread the test can give up on.
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 64];
            let read = reader.read(&mut buffer).map(|read| buffer[..read].to_vec());
            let _ = done_tx.send((read, reader));
        });
        let (read, mut reader) = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the read did not return; the closed exit pipe is still polled");
        assert_eq!(read.expect("read"), b"abc");
        assert!(
            reader.exit_read.is_none(),
            "the dead exit pipe is still polled"
        );
        let mut buffer = [0_u8; 64];
        master_write.write_all(b"def").expect("write");
        let read = reader.read(&mut buffer).expect("read");
        assert_eq!(&buffer[..read], b"def");
        drop(master_write);
        assert_eq!(reader.read(&mut buffer).expect("read at EOF"), 0);
        assert!(!exit_seen.load(Ordering::Acquire));
    }

    // A read into an empty buffer returns nothing and takes nothing: the
    // exit and the queued output are still there for the next read.
    #[test]
    fn child_exit_reader_changes_nothing_for_an_empty_buffer() {
        use std::io::{Read, Write};

        let (master_read, mut master_write) = test_pipe();
        master_write.write_all(b"last").expect("queue output");
        let exit_seen = Arc::new(AtomicBool::new(false));
        let mut reader = exit_reader(master_read, reported_exit(), &exit_seen);
        assert_eq!(reader.read(&mut []).expect("empty read"), 0);
        assert!(!exit_seen.load(Ordering::Acquire));
        let mut buffer = [0_u8; 64];
        let read = reader.read(&mut buffer).expect("read");
        assert_eq!(&buffer[..read], b"last");
        assert!(exit_seen.load(Ordering::Acquire));
    }

    // Output is read first, whatever else comes with it; then an invalid
    // descriptor or an error is a failure; a hangup alone is the end without
    // a read, which could block if something opened the slave again.
    #[test]
    fn master_readiness_reads_output_first_and_ends_at_a_hangup_alone() {
        use super::MasterReadiness::{Data, Failed, HungUp, Quiet};

        assert_eq!(super::master_readiness(libc::POLLIN), Data);
        assert_eq!(super::master_readiness(libc::POLLIN | libc::POLLHUP), Data);
        assert_eq!(super::master_readiness(libc::POLLIN | libc::POLLERR), Data);
        assert_eq!(super::master_readiness(libc::POLLHUP), HungUp);
        assert_eq!(
            super::master_readiness(libc::POLLHUP | libc::POLLERR),
            Failed
        );
        assert_eq!(super::master_readiness(libc::POLLERR), Failed);
        assert_eq!(super::master_readiness(libc::POLLNVAL), Failed);
        assert_eq!(
            super::master_readiness(libc::POLLIN | libc::POLLNVAL),
            Failed
        );
        assert_eq!(super::master_readiness(0), Quiet);
    }

    // On Linux a master whose slaves are all closed reports a hangup alone.
    // The reader ends there without a read of its own.
    #[cfg(target_os = "linux")]
    #[test]
    fn child_exit_reader_ends_at_a_hangup_alone_without_reading() {
        use std::io::Read;

        struct MustNotRead;
        impl Read for MustNotRead {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                panic!("the reader read a master that reported a hangup alone");
            }
        }

        let pty = nix::pty::openpty(None, None).expect("openpty");
        drop(pty.slave);
        let (exit_read, _exit_write) = test_pipe();
        let mut reader = super::ChildExitReader::new(
            MustNotRead,
            std::os::fd::AsRawFd::as_raw_fd(&pty.master),
            exit_read,
            Arc::new(AtomicBool::new(false)),
        );
        let mut buffer = [0_u8; 64];
        assert_eq!(reader.read(&mut buffer).expect("read"), 0);
    }

    // The early end needs a kernel with both PTY fixes it relies on: from
    // 4.7 a master poll lets output still on its way arrive first, and from
    // 4.12 a slave flush no longer removes output a poll has reported.
    #[test]
    fn pty_drain_is_reliable_from_linux_4_12() {
        for release in [
            "6.8.0-110-generic",
            "5.14.0-427.13.1.el9_4.x86_64",
            "4.18.0-553.el8_10.x86_64",
            "4.12.0",
            "4.12",
            "4.14.336",
            "10.0.1",
        ] {
            assert!(super::pty_drain_is_reliable(release), "{release}");
        }
        for release in [
            "4.11.12",
            "4.11",
            "4.8.0-22-generic",
            "4.7.0",
            "4.7",
            "4.6.9",
            "4.4.0-19041-Microsoft",
            "3.10.0-1160.el7.x86_64",
            "",
            "garbage",
            "6",
            "6.",
            ".8",
            "v6.8.0",
            "6.x",
        ] {
            assert!(!super::pty_drain_is_reliable(release), "{release}");
        }
    }

    // Only a process that is gone counts as exited. waitid also reports a
    // child that made prismtty its tracer when it stops: that child is alive,
    // and so is one that stopped or continued.
    #[test]
    fn only_a_real_exit_counts() {
        for (si_code, exited) in [
            (libc::CLD_EXITED, true),
            (libc::CLD_KILLED, true),
            (libc::CLD_DUMPED, true),
            (libc::CLD_TRAPPED, false),
            (libc::CLD_STOPPED, false),
            (libc::CLD_CONTINUED, false),
        ] {
            assert_eq!(super::child_has_exited(si_code), exited, "{si_code}");
        }
    }

    /// Everything the exit pipe receives until its write end is closed. A
    /// write end left open fails here, after a bound, instead of blocking the
    /// test.
    fn exit_pipe_bytes(mut exit_read: std::fs::File) -> Vec<u8> {
        use std::io::Read;

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bytes = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = libc::c_int::try_from(remaining.as_millis()).unwrap_or(libc::c_int::MAX);
            let [revents] =
                super::poll_input([std::os::fd::AsRawFd::as_raw_fd(&exit_read)], timeout)
                    .expect("poll the exit pipe");
            assert!(revents != 0, "the exit pipe's write end was left open");
            let mut chunk = [0_u8; 16];
            let read = exit_read.read(&mut chunk).expect("read the exit pipe");
            if read == 0 {
                return bytes;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
    }

    // The waiter can report the exit before or after the read loop's pipe is
    // handed over. Either way the byte is written exactly once, by whichever
    // comes second, and the pipe is closed after it.
    #[test]
    fn child_exit_notice_writes_the_byte_once_in_either_order() {
        let notice = super::ChildExitNotice::default();
        let (exit_read, exit_write) = test_pipe();
        notice.report(true);
        notice.install(exit_write.into());
        assert_eq!(exit_pipe_bytes(exit_read), vec![1], "exit before the pipe");

        let notice = super::ChildExitNotice::default();
        let (exit_read, exit_write) = test_pipe();
        notice.install(exit_write.into());
        notice.report(true);
        assert_eq!(exit_pipe_bytes(exit_read), vec![1], "pipe before the exit");
    }

    // A wait that ended without a real exit, in either order, and a waiter
    // that never ran close the pipe without a byte.
    #[test]
    fn child_exit_notice_writes_no_byte_without_a_real_exit() {
        let notice = super::ChildExitNotice::default();
        let (exit_read, exit_write) = test_pipe();
        notice.report(false);
        notice.install(exit_write.into());
        assert!(exit_pipe_bytes(exit_read).is_empty(), "unknown end first");

        let notice = super::ChildExitNotice::default();
        let (exit_read, exit_write) = test_pipe();
        notice.install(exit_write.into());
        notice.report(false);
        assert!(exit_pipe_bytes(exit_read).is_empty(), "pipe first");

        let notice = Arc::new(super::ChildExitNotice::default());
        let (exit_read, exit_write) = test_pipe();
        notice.install(exit_write.into());
        drop(super::ChildExitReporter {
            notice: Arc::clone(&notice),
            exited: false,
        });
        assert!(exit_pipe_bytes(exit_read).is_empty(), "waiter never ran");
    }

    // The waiter reports a real exit after it has closed the slave, on Linux
    // from 4.12; elsewhere there is no notice and nothing changes.
    #[test]
    fn waiter_reports_the_child_exit_where_the_kernel_allows_it() {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .expect("openpty");
        let mut builder = portable_pty::CommandBuilder::new("true");
        builder.cwd("/");
        let mut child =
            super::spawn_with_pty_slave(&builder, &*pair.master, false, &[]).expect("spawn true");
        let notice = super::close_slave_after_child_exit(pair.slave, child.process_id());
        #[cfg(target_os = "linux")]
        let expected =
            super::kernel_release().is_some_and(|release| super::pty_drain_is_reliable(&release));
        #[cfg(not(target_os = "linux"))]
        let expected = false;
        assert_eq!(notice.is_some(), expected);
        if let Some(notice) = notice {
            let (exit_read, exit_write) = test_pipe();
            notice.install(exit_write.into());
            assert_eq!(exit_pipe_bytes(exit_read), vec![1]);
        }
        assert_eq!(super::reap_child(&mut child).expect("reap true").code, 0);
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
