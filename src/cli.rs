use crate::config::{PrismConfig, load_profile_file};
use crate::highlight::{BenchmarkReport, Highlighter, StreamingHighlighter, strip_ansi};
use crate::profile_runtime::ProfileRuntime;
use crate::profiles::ProfileStore;
use crate::style::ColorMode;
use directories::BaseDirs;
use is_terminal::IsTerminal;
use nix::libc;
use nix::sys::termios::{
    InputFlags, LocalFlags, OutputFlags, SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::cmp::Reverse;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

const AUTO_DETECT_SAMPLE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Highlight(#[from] crate::highlight::HighlightError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("PTY error: {0}")]
    Pty(#[from] anyhow::Error),
    #[error("terminal mode error: {0}")]
    Terminal(#[from] nix::errno::Errno),
}

#[derive(Debug, Default)]
struct Options {
    profiles: Vec<String>,
    no_auto_detect: bool,
    config: Option<PathBuf>,
    strip_ansi: bool,
    force_rgb: bool,
    benchmark: bool,
    show_profile: bool,
    local_echo: bool,
    no_dynamic_profile: bool,
    trace_io: Option<PathBuf>,
}

#[derive(Debug)]
enum Action {
    Stdin,
    Run(Vec<OsString>),
    ProfilesList,
    ProfilesShow(String),
    ProfilesValidate(PathBuf),
    ProfilesTest { profile: String, fixture: PathBuf },
    Reload,
    Help,
    Version,
}

pub fn run() -> ExitCode {
    match run_inner(std::env::args_os().skip(1).collect()) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(io::stderr(), "prismtty: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_inner(args: Vec<OsString>) -> Result<ExitCode, CliError> {
    let (options, action) = parse_args(args)?;
    match action {
        Action::Help => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        Action::Version => {
            println!("prismtty {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        Action::Reload => {
            let count = request_reload()?;
            println!("Processes reloaded: {count}");
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesList => {
            let store = profile_store()?;
            for name in store.names() {
                println!("{name}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesShow(profile_name) => {
            let store = profile_store()?;
            let profile = store
                .profile(&profile_name)
                .ok_or_else(|| CliError::Usage(format!("unknown profile '{profile_name}'")))?;
            println!("profile: {}", profile.name);
            if profile.inherits.is_empty() {
                println!("inherits: none");
            } else {
                println!("inherits: {}", profile.inherits.join(", "));
            }
            if profile.detection.is_empty() {
                println!("detection: none");
            } else {
                println!("detection: {}", profile.detection.join(", "));
            }
            println!("rules:");
            for rule in &profile.rules {
                println!("  - {}", rule.description);
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesValidate(path) => {
            let loaded = load_profile_file(&path)?;
            let mut store = profile_store()?;
            store.insert_profile(
                loaded.meta.name.clone(),
                loaded.meta.inherits.clone(),
                loaded.meta.detection.clone(),
                loaded.rules.clone(),
            );
            let config = PrismConfig {
                rules: PrismConfig::from_profiles(&store, &[loaded.meta.name.as_str()])?.rules,
                enabled_profiles: vec![loaded.meta.name.clone()],
            };
            let _ = Highlighter::from_config(config)?;
            println!("profile {} valid", loaded.meta.name);
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesTest { profile, fixture } => {
            let input = fs::read(&fixture)?;
            let store = profile_store()?;
            let config = PrismConfig::from_profiles(&store, &[profile.as_str()])?;
            let highlighter = Highlighter::from_config(config)?;
            io::stdout().write_all(&highlighter.highlight_bytes(&input))?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Stdin => run_stdin(options),
        Action::Run(command) => run_command(options, command),
    }
}

fn parse_args(args: Vec<OsString>) -> Result<(Options, Action), CliError> {
    let mut options = Options::default();
    let mut idx = 0;

    while idx < args.len() {
        let arg = args[idx].to_string_lossy();
        match arg.as_ref() {
            "-h" | "--help" => return Ok((options, Action::Help)),
            "-V" | "-v" | "--version" => return Ok((options, Action::Version)),
            "-b" | "--benchmark" => {
                options.benchmark = true;
                idx += 1;
            }
            "-r" | "--reload" => return Ok((options, Action::Reload)),
            "-R" | "--rgb" => {
                options.force_rgb = true;
                idx += 1;
            }
            "--pcre" => {
                idx += 1;
            }
            "--no-auto-detect" => {
                options.no_auto_detect = true;
                idx += 1;
            }
            "--no-dynamic-profile" => {
                options.no_dynamic_profile = true;
                idx += 1;
            }
            "--strip-ansi" => {
                options.strip_ansi = true;
                idx += 1;
            }
            "--show-profile" => {
                options.show_profile = true;
                idx += 1;
            }
            "--local-echo" => {
                options.local_echo = true;
                idx += 1;
            }
            "--trace-io" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(|| CliError::Usage("--trace-io requires a path".to_string()))?;
                options.trace_io = Some(PathBuf::from(path));
                idx += 1;
            }
            "-p" | "--profile" => {
                idx += 1;
                let profile = args
                    .get(idx)
                    .ok_or_else(|| CliError::Usage("--profile requires a value".to_string()))?;
                options.profiles.push(profile.to_string_lossy().to_string());
                idx += 1;
            }
            "-c" | "--config" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(|| CliError::Usage("--config requires a path".to_string()))?;
                options.config = Some(PathBuf::from(path));
                idx += 1;
            }
            "profiles" => return parse_profiles_command(options, &args[idx + 1..]),
            "--" => {
                let command = args[idx + 1..].to_vec();
                if command.is_empty() {
                    return Ok((options, Action::Stdin));
                }
                return Ok((options, Action::Run(command)));
            }
            _ if arg.starts_with('-') => {
                return Err(CliError::Usage(format!("unknown option '{arg}'")));
            }
            _ => return Ok((options, Action::Run(args[idx..].to_vec()))),
        }
    }

    Ok((options, Action::Stdin))
}

fn parse_profiles_command(
    options: Options,
    args: &[OsString],
) -> Result<(Options, Action), CliError> {
    let subcommand = args
        .first()
        .map(|arg| arg.to_string_lossy().to_string())
        .unwrap_or_else(|| "list".to_string());

    match subcommand.as_str() {
        "list" => Ok((options, Action::ProfilesList)),
        "show" => {
            let profile = args.get(1).ok_or_else(|| {
                CliError::Usage("profiles show requires a profile name".to_string())
            })?;
            Ok((
                options,
                Action::ProfilesShow(profile.to_string_lossy().to_string()),
            ))
        }
        "validate" => {
            let path = args.get(1).ok_or_else(|| {
                CliError::Usage("profiles validate requires a profile path".to_string())
            })?;
            Ok((options, Action::ProfilesValidate(PathBuf::from(path))))
        }
        "test" => {
            let profile = args.get(1).ok_or_else(|| {
                CliError::Usage("profiles test requires a profile name".to_string())
            })?;
            let fixture = args.get(2).ok_or_else(|| {
                CliError::Usage("profiles test requires a fixture path".to_string())
            })?;
            Ok((
                options,
                Action::ProfilesTest {
                    profile: profile.to_string_lossy().to_string(),
                    fixture: PathBuf::from(fixture),
                },
            ))
        }
        other => Err(CliError::Usage(format!(
            "unknown profiles subcommand '{other}'"
        ))),
    }
}

fn run_stdin(options: Options) -> Result<ExitCode, CliError> {
    let _registration = RuntimeRegistration::register()?;
    let reload_watcher = Some(ReloadWatcher::new());
    let trace = IoTrace::open(options.trace_io.as_deref())?;
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let interactive = stdin_mode_interactive_highlighting(stdout.is_terminal());
    highlight_stream(
        stdin.lock(),
        &mut stdout,
        &options,
        interactive,
        reload_watcher,
        trace,
        None,
    )?;
    Ok(ExitCode::SUCCESS)
}

fn stdin_mode_interactive_highlighting(stdout_is_terminal: bool) -> bool {
    stdout_is_terminal
}

fn run_command(options: Options, command: Vec<OsString>) -> Result<ExitCode, CliError> {
    if command.is_empty() {
        return run_stdin(options);
    }

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

    let raw_mode = if interactive {
        Some(RawModeGuard::enable()?)
    } else {
        None
    };

    let trace = IoTrace::open(options.trace_io.as_deref())?;
    let (profile_input_tx, profile_input_rx) = if dynamic_profile_enabled(&options, interactive) {
        let (tx, rx) = mpsc::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    if raw_mode.is_some() {
        let mut writer = pair.master.take_writer()?;
        let trace = trace.clone();
        let local_echo = options.local_echo;
        thread::spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            let _ =
                forward_stdin_to_pty(&mut stdin, &mut writer, local_echo, trace, profile_input_tx);
        });
    }

    let mut reader = pair.master.try_clone_reader()?;
    let stop_resize = Arc::new(AtomicBool::new(false));
    let resize_thread = {
        let stop_resize = Arc::clone(&stop_resize);
        let master = pair.master;
        thread::spawn(move || poll_pty_size(master, stop_resize))
    };

    let mut stdout = io::stdout();
    let _registration = RuntimeRegistration::register()?;
    let reload_watcher = Some(ReloadWatcher::new());
    highlight_stream(
        &mut reader,
        &mut stdout,
        &options,
        interactive,
        reload_watcher,
        trace,
        profile_input_rx,
    )?;

    let status = child.wait()?;
    stop_resize.store(true, Ordering::Relaxed);
    let _ = resize_thread.join();
    drop(raw_mode);
    Ok(ExitCode::from(status.exit_code() as u8))
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

    for key in [
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "LC_TERMINAL",
        "LC_TERMINAL_VERSION",
        "ITERM_SESSION_ID",
        "ITERM_PROFILE",
    ] {
        builder.env_remove(key);
    }

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

#[cfg(not(unix))]
fn configure_child_pty(_master: &dyn portable_pty::MasterPty) -> Result<(), CliError> {
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
    profile_input: Option<mpsc::Sender<Vec<u8>>>,
) -> io::Result<()> {
    let mut buffer = [0_u8; 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        let input = &buffer[..read];
        trace.log("IN", input);
        if let Some(sender) = &profile_input {
            let _ = sender.send(input.to_vec());
        }
        writer.write_all(input)?;
        writer.flush()?;

        if local_echo {
            let echo = local_echo_bytes(input);
            if !echo.is_empty() {
                let mut stdout = io::stdout().lock();
                stdout.write_all(&echo)?;
                stdout.flush()?;
            }
        }
    }
}

fn local_echo_bytes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut idx = 0;
    while idx < input.len() {
        match input[idx] {
            0x08 | 0x7f => output.extend_from_slice(b"\x08 \x08"),
            b'\r' | b'\n' => output.extend_from_slice(b"\r\n"),
            0x1b => {
                if input.get(idx + 1) == Some(&b'[') {
                    idx += 2;
                    while idx < input.len() && !(0x40..=0x7e).contains(&input[idx]) {
                        idx += 1;
                    }
                } else {
                    idx += 1;
                }
            }
            byte if byte.is_ascii_control() => {}
            byte => output.push(byte),
        }
        idx += 1;
    }
    output
}

#[derive(Clone)]
struct IoTrace {
    file: Option<Arc<Mutex<File>>>,
}

impl IoTrace {
    fn open(path: Option<&Path>) -> io::Result<Self> {
        let file = match path {
            Some(path) => Some(Arc::new(Mutex::new(
                OpenOptions::new().create(true).append(true).open(path)?,
            ))),
            None => None,
        };
        Ok(Self { file })
    }

    fn log(&self, direction: &str, bytes: &[u8]) {
        let Some(file) = &self.file else {
            return;
        };
        let Ok(mut file) = file.lock() else {
            return;
        };
        let _ = writeln!(file, "{direction} {}", trace_hex(bytes));
    }
}

fn trace_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(3).saturating_sub(1));
    for (idx, byte) in bytes.iter().enumerate() {
        if idx > 0 {
            output.push(' ');
        }
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

struct RawModeGuard {
    original: Termios,
}

impl RawModeGuard {
    fn enable() -> Result<Self, CliError> {
        let stdin = io::stdin();
        let original = tcgetattr(stdin.as_fd())?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw)?;
        Ok(Self { original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let stdin = io::stdin();
        let _ = tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &self.original);
    }
}

fn current_pty_size() -> PtySize {
    let stdout = io::stdout();
    if let Some(size) = pty_size_from_fd(stdout.as_fd()) {
        return size;
    }

    let stdin = io::stdin();
    pty_size_from_fd(stdin.as_fd()).unwrap_or_default()
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

struct RuntimeRegistration {
    pid: u32,
    path: PathBuf,
}

impl RuntimeRegistration {
    fn register() -> io::Result<Self> {
        let dir = runtime_dir();
        fs::create_dir_all(&dir)?;
        let path = pid_registry_path();
        let pid = std::process::id();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{pid}")?;
        Ok(Self { pid, path })
    }
}

impl Drop for RuntimeRegistration {
    fn drop(&mut self) {
        let Ok(input) = fs::read_to_string(&self.path) else {
            return;
        };
        let retained = input
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .filter(|pid| *pid != self.pid && process_is_alive(*pid))
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let output = if retained.is_empty() {
            String::new()
        } else {
            format!("{retained}\n")
        };
        let _ = fs::write(&self.path, output);
    }
}

struct ReloadWatcher {
    marker: PathBuf,
    last_seen: Option<SystemTime>,
}

impl ReloadWatcher {
    fn new() -> Self {
        let marker = reload_marker_path();
        let last_seen = reload_marker_time(&marker);
        Self { marker, last_seen }
    }

    fn reload_requested(&mut self) -> bool {
        let current = reload_marker_time(&self.marker);
        if current.is_some() && current != self.last_seen {
            self.last_seen = current;
            return true;
        }
        false
    }
}

fn request_reload() -> io::Result<usize> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir)?;
    fs::write(reload_marker_path(), format!("{:?}\n", SystemTime::now()))?;

    let path = pid_registry_path();
    let input = fs::read_to_string(&path).unwrap_or_default();
    let mut count = 0usize;
    let mut retained = Vec::new();
    for pid in input
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
    {
        if process_is_alive(pid) {
            count += 1;
            retained.push(pid.to_string());
        }
    }

    let output = if retained.is_empty() {
        String::new()
    } else {
        format!("{}\n", retained.join("\n"))
    };
    fs::write(path, output)?;
    Ok(count)
}

fn runtime_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("PRISMTTY_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join(format!("prismtty-{}", current_uid()))
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn pid_registry_path() -> PathBuf {
    runtime_dir().join("pids")
}

fn reload_marker_path() -> PathBuf {
    runtime_dir().join("reload")
}

fn reload_marker_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn poll_pty_size(master: Box<dyn portable_pty::MasterPty + Send>, stop: Arc<AtomicBool>) {
    let mut last_size = current_pty_size();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(250));
        let next_size = current_pty_size();
        if next_size != last_size {
            let _ = master.resize(next_size);
            last_size = next_size;
        }
    }
}

fn highlight_stream<R: Read, W: Write>(
    mut reader: R,
    writer: &mut W,
    options: &Options,
    interactive: bool,
    mut reload_watcher: Option<ReloadWatcher>,
    trace: IoTrace,
    profile_input_rx: Option<mpsc::Receiver<Vec<u8>>>,
) -> Result<(), CliError> {
    let started = Instant::now();
    let mut input_bytes = 0usize;
    let mut buffer = [0_u8; 8192];
    let read = reader.read(&mut buffer)?;
    if read == 0 {
        return Ok(());
    }

    trace.log("OUT", &buffer[..read]);
    let first_chunk = prepare_chunk(&buffer[..read], options.strip_ansi);
    let mut detection_sample = first_chunk.clone();
    input_bytes += first_chunk.len();
    let mut profile_names = select_profile_names(options, &detection_sample)?;
    let highlighter = build_highlighter_for_profiles(options, &profile_names, interactive)?;
    let mut streaming = new_streaming_highlighter(highlighter, interactive, options.benchmark);
    let mut reporter = ProfileReporter::new(options.show_profile, auto_detect_enabled(options));
    reporter.report(&profile_names);
    let dynamic_profiles =
        dynamic_profile_enabled(options, interactive) && profile_input_rx.is_some();
    let runtime_store = if dynamic_profiles {
        Some(profile_store()?)
    } else {
        None
    };
    let mut profile_runtime = if dynamic_profiles {
        Some(ProfileRuntime::new(profile_names.clone()))
    } else {
        None
    };
    let mut auto_detect_pending =
        !dynamic_profiles && should_continue_auto_detect(options, &profile_names);
    if let Some(next_profile_names) = observe_dynamic_profile(
        &mut profile_runtime,
        profile_input_rx.as_ref(),
        runtime_store.as_ref(),
        &first_chunk,
    )
    .filter(|next_profile_names| next_profile_names != &profile_names)
    {
        write_rendered(writer, &trace, streaming.finish())?;
        profile_names = next_profile_names;
        let highlighter = build_highlighter_for_profiles(options, &profile_names, interactive)?;
        streaming = new_streaming_highlighter(highlighter, interactive, options.benchmark);
        reporter.report(&profile_names);
    }
    write_rendered(writer, &trace, streaming.push(&first_chunk))?;
    writer.flush()?;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        trace.log("OUT", &buffer[..read]);
        let chunk = prepare_chunk(&buffer[..read], options.strip_ansi);
        input_bytes += chunk.len();
        if let Some(next_profile_names) = observe_dynamic_profile(
            &mut profile_runtime,
            profile_input_rx.as_ref(),
            runtime_store.as_ref(),
            &chunk,
        )
        .filter(|next_profile_names| next_profile_names != &profile_names)
        {
            write_rendered(writer, &trace, streaming.finish())?;
            profile_names = next_profile_names;
            let highlighter = build_highlighter_for_profiles(options, &profile_names, interactive)?;
            streaming = new_streaming_highlighter(highlighter, interactive, options.benchmark);
            reporter.report(&profile_names);
        }
        if auto_detect_pending && detection_sample.len() < AUTO_DETECT_SAMPLE_LIMIT {
            detection_sample.extend_from_slice(&chunk);
            let next_profile_names = select_profile_names(options, &detection_sample)?;
            if next_profile_names != profile_names {
                write_rendered(writer, &trace, streaming.finish())?;
                profile_names = next_profile_names;
                let highlighter =
                    build_highlighter_for_profiles(options, &profile_names, interactive)?;
                streaming = new_streaming_highlighter(highlighter, interactive, options.benchmark);
                reporter.report(&profile_names);
                auto_detect_pending = should_continue_auto_detect(options, &profile_names);
            } else if detection_sample.len() >= AUTO_DETECT_SAMPLE_LIMIT {
                auto_detect_pending = false;
                reporter.report(&profile_names);
            }
        }
        if reload_watcher
            .as_mut()
            .is_some_and(ReloadWatcher::reload_requested)
        {
            write_rendered(writer, &trace, streaming.finish())?;
            let highlighter = build_highlighter_for_profiles(options, &profile_names, interactive)?;
            streaming = new_streaming_highlighter(highlighter, interactive, options.benchmark);
        }
        write_rendered(writer, &trace, streaming.push(&chunk))?;
        writer.flush()?;
    }

    write_rendered(writer, &trace, streaming.finish())?;
    writer.flush()?;
    reporter.report(&profile_names);

    if options.benchmark {
        print_benchmark_report(
            streaming.benchmark_report(),
            input_bytes,
            started.elapsed().as_secs_f64(),
        );
    }

    Ok(())
}

fn observe_dynamic_profile(
    runtime: &mut Option<ProfileRuntime>,
    profile_input_rx: Option<&mpsc::Receiver<Vec<u8>>>,
    store: Option<&ProfileStore>,
    chunk: &[u8],
) -> Option<Vec<String>> {
    let runtime = runtime.as_mut()?;
    let store = store?;
    if let Some(receiver) = profile_input_rx {
        while let Ok(input) = receiver.try_recv() {
            runtime.observe_input(&input);
        }
    }
    let visible_chunk = strip_ansi(chunk);
    runtime.observe_output(&visible_chunk, store)
}

fn write_rendered<W: Write>(writer: &mut W, trace: &IoTrace, rendered: Vec<u8>) -> io::Result<()> {
    trace.log("RENDER", &rendered);
    writer.write_all(&rendered)
}

fn new_streaming_highlighter(
    highlighter: Highlighter,
    interactive: bool,
    benchmark: bool,
) -> StreamingHighlighter {
    if interactive && benchmark {
        StreamingHighlighter::new_interactive_with_benchmark(highlighter)
    } else if interactive {
        StreamingHighlighter::new_interactive(highlighter)
    } else if benchmark {
        StreamingHighlighter::new_with_benchmark(highlighter)
    } else {
        StreamingHighlighter::new(highlighter)
    }
}

fn print_benchmark_report(report: Option<&BenchmarkReport>, input_bytes: usize, elapsed_secs: f64) {
    eprintln!("Benchmark results (time spent, match count):");
    if let Some(report) = report {
        let total = report.total_duration().as_secs_f64();
        let mut rules = report.rules().to_vec();
        rules.sort_by_key(|rule| Reverse(rule.duration));
        for rule in rules {
            let percent = if total > 0.0 {
                rule.duration.as_secs_f64() / total * 100.0
            } else {
                0.0
            };
            eprintln!(
                "{percent:>6.2}% {:>8.3}s  {:<7}  {}",
                rule.duration.as_secs_f64(),
                rule.match_count,
                rule.description
            );
        }
    }
    eprintln!("Processed {input_bytes} bytes in {elapsed_secs:.3}s");
}

fn prepare_chunk(input: &[u8], strip_existing_ansi: bool) -> Vec<u8> {
    if strip_existing_ansi {
        strip_ansi(input)
    } else {
        input.to_vec()
    }
}

fn build_highlighter_for_profiles(
    options: &Options,
    profile_names: &[String],
    interactive: bool,
) -> Result<Highlighter, CliError> {
    let config = build_config_for_profiles(options, profile_names)?;
    Ok(Highlighter::from_config_with_color_mode(
        config,
        color_mode(options, interactive),
    )?)
}

fn color_mode(options: &Options, interactive: bool) -> ColorMode {
    color_mode_for_context(options, interactive, terminal_supports_truecolor())
}

fn color_mode_for_context(
    options: &Options,
    _interactive: bool,
    terminal_truecolor: bool,
) -> ColorMode {
    if options.force_rgb || terminal_truecolor {
        ColorMode::TrueColor
    } else {
        ColorMode::Xterm256
    }
}

fn terminal_supports_truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|value| matches!(value.as_str(), "truecolor" | "24bit"))
        .unwrap_or(false)
}

fn select_profile_names(options: &Options, sample: &[u8]) -> Result<Vec<String>, CliError> {
    let store = profile_store()?;
    Ok(if !options.profiles.is_empty() {
        options.profiles.clone()
    } else if options.no_auto_detect {
        vec!["generic".to_string()]
    } else {
        let visible_sample = strip_ansi(sample);
        let sample_text = String::from_utf8_lossy(&visible_sample);
        store.detect_profiles(&sample_text)
    })
}

fn build_config_for_profiles(
    options: &Options,
    profile_names: &[String],
) -> Result<PrismConfig, CliError> {
    let store = profile_store()?;
    let profile_refs: Vec<&str> = profile_names.iter().map(String::as_str).collect();
    let mut config = PrismConfig::from_profiles(&store, &profile_refs)?;

    if let Some(path) = &options.config {
        config = config.merge(PrismConfig::from_chromaterm_file(path)?);
    } else {
        for path in default_config_paths() {
            if path.exists() {
                config = config.merge(PrismConfig::from_chromaterm_file(path)?);
            }
        }
    }

    Ok(config)
}

fn auto_detect_enabled(options: &Options) -> bool {
    options.profiles.is_empty() && !options.no_auto_detect
}

fn dynamic_profile_enabled(options: &Options, interactive: bool) -> bool {
    interactive && auto_detect_enabled(options) && !options.no_dynamic_profile
}

fn should_continue_auto_detect(options: &Options, profile_names: &[String]) -> bool {
    auto_detect_enabled(options)
        && profile_names.len() == 1
        && profile_names
            .first()
            .is_some_and(|profile| profile == "generic")
}

struct ProfileReporter {
    show_profile: bool,
    auto_detect: bool,
    last_reported: Option<Vec<String>>,
}

impl ProfileReporter {
    fn new(show_profile: bool, auto_detect: bool) -> Self {
        Self {
            show_profile,
            auto_detect,
            last_reported: None,
        }
    }

    fn report(&mut self, profile_names: &[String]) {
        if let Some(message) = self.message_for(profile_names) {
            eprintln!("{message}");
        }
    }

    fn message_for(&mut self, profile_names: &[String]) -> Option<String> {
        if !self.show_profile {
            return None;
        }
        if self.auto_detect && is_generic_only(profile_names) && self.last_reported.is_none() {
            return None;
        }
        if self
            .last_reported
            .as_ref()
            .is_some_and(|reported| reported == profile_names)
        {
            return None;
        }
        self.last_reported = Some(profile_names.to_vec());
        Some(format!(
            "prismtty: profiles selected: {}",
            profile_names.join(", ")
        ))
    }
}

fn is_generic_only(profile_names: &[String]) -> bool {
    profile_names.len() == 1
        && profile_names
            .first()
            .is_some_and(|profile| profile == "generic")
}

fn profile_store() -> Result<ProfileStore, CliError> {
    let mut store = ProfileStore::builtin();
    for loaded in load_profiles_d()? {
        store.insert_profile(
            loaded.meta.name,
            loaded.meta.inherits,
            loaded.meta.detection,
            loaded.rules,
        );
    }
    Ok(store)
}

fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(base_dirs) = BaseDirs::new() {
        paths.push(base_dirs.home_dir().join(".chromaterm.yml"));
        paths.push(base_dirs.home_dir().join(".chromaterm.yaml"));
    }
    if let Some(config_dir) = config_base_dir() {
        paths.push(config_dir.join("chromaterm").join("chromaterm.yml"));
        paths.push(config_dir.join("chromaterm").join("chromaterm.yaml"));
        paths.push(config_dir.join("prismtty").join("config.yml"));
        paths.push(config_dir.join("prismtty").join("config.yaml"));
    }
    paths.push(PathBuf::from("/etc/chromaterm/chromaterm.yml"));
    paths.push(PathBuf::from("/etc/chromaterm/chromaterm.yaml"));
    paths
}

fn load_profiles_d() -> Result<Vec<crate::config::LoadedProfileFile>, CliError> {
    let mut profiles = Vec::new();
    let Some(config_dir) = config_base_dir() else {
        return Ok(profiles);
    };
    let dir = config_dir.join("prismtty").join("profiles.d");
    if !dir.exists() {
        return Ok(profiles);
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if is_yaml(&path) {
            entries.push(path);
        }
    }
    entries.sort();

    for path in entries {
        let loaded = load_profile_file(path)?;
        profiles.push(loaded);
    }

    Ok(profiles)
}

fn config_base_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME")
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }
    BaseDirs::new().map(|base_dirs| base_dirs.home_dir().join(".config"))
}

fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("yml" | "yaml")
    )
}

fn print_help() {
    println!(
        "\
PrismTTY {}

USAGE:
  prismtty [OPTIONS] [COMMAND...]
  command | prismtty [OPTIONS]
  prismtty profiles list
  prismtty profiles show <PROFILE>
  prismtty profiles validate <FILE>
  prismtty profiles test <PROFILE> <FILE>

OPTIONS:
  -p, --profile <NAME>     Force a profile; repeat to enable several
      --no-auto-detect     Use only the generic profile unless --profile is set
      --no-dynamic-profile Disable profile switching inside wrapped interactive shells
  -c, --config <FILE>      Load a ChromaTerm-compatible YAML config
      --strip-ansi         Remove existing ANSI before applying PrismTTY styles
      --show-profile       Print selected profiles to stderr
      --local-echo         Locally echo typed printable keys for no-echo device sessions
      --trace-io <FILE>    Append hex-encoded PTY input/output diagnostics
  -R, --rgb                Force RGB color output
      --pcre               Accepted for ChromaTerm compatibility; PCRE2 is always used
  -b, --benchmark          Print per-rule timing and match-count data to stderr
  -r, --reload             Ask running PrismTTY sessions to reload config
  -h, --help               Show this help
  -V, -v, --version        Show version
",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use nix::sys::termios::{InputFlags, LocalFlags, OutputFlags};

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
    fn trace_hex_encodes_bytes_for_diagnostics() {
        assert_eq!(super::trace_hex(b"echo\r\n"), "65 63 68 6f 0d 0a");
    }

    #[test]
    fn stdin_mode_uses_interactive_highlighting_when_output_is_terminal() {
        assert!(super::stdin_mode_interactive_highlighting(true));
        assert!(!super::stdin_mode_interactive_highlighting(false));
    }

    #[test]
    fn interactive_color_mode_keeps_truecolor_when_terminal_supports_it() {
        let options = super::Options::default();
        assert_eq!(
            super::color_mode_for_context(&options, true, true),
            super::ColorMode::TrueColor
        );
        assert_eq!(
            super::color_mode_for_context(&options, true, false),
            super::ColorMode::Xterm256
        );

        let options = super::Options {
            force_rgb: true,
            ..super::Options::default()
        };
        assert_eq!(
            super::color_mode_for_context(&options, true, true),
            super::ColorMode::TrueColor
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
        builder.env("TERM_PROGRAM", "iTerm.app");

        super::apply_iterm_shell_integration_guard(&mut builder, true, false);
        assert_eq!(
            builder.get_env("TERM_PROGRAM"),
            Some(std::ffi::OsStr::new("iTerm.app"))
        );

        super::apply_iterm_shell_integration_guard(&mut builder, false, true);
        assert_eq!(
            builder.get_env("TERM_PROGRAM"),
            Some(std::ffi::OsStr::new("iTerm.app"))
        );
    }

    #[test]
    fn profile_reporter_waits_for_auto_detect_promotion() {
        let mut reporter = super::ProfileReporter::new(true, true);

        assert!(reporter.message_for(&["generic".to_string()]).is_none());
        assert_eq!(
            reporter.message_for(&["generic".to_string(), "cisco".to_string()]),
            Some("prismtty: profiles selected: generic, cisco".to_string())
        );
    }

    #[test]
    fn dynamic_profile_switching_is_default_only_for_interactive_auto_detect() {
        let options = super::Options::default();
        assert!(super::dynamic_profile_enabled(&options, true));
        assert!(!super::dynamic_profile_enabled(&options, false));

        let forced = super::Options {
            profiles: vec!["juniper".to_string()],
            ..super::Options::default()
        };
        assert!(!super::dynamic_profile_enabled(&forced, true));

        let no_auto = super::Options {
            no_auto_detect: true,
            ..super::Options::default()
        };
        assert!(!super::dynamic_profile_enabled(&no_auto, true));

        let opt_out = super::Options {
            no_dynamic_profile: true,
            ..super::Options::default()
        };
        assert!(!super::dynamic_profile_enabled(&opt_out, true));
    }
}
