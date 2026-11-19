//! Command-line entry point for the PrismTTY binaries.
//!
//! The public surface is intentionally small: `run` performs argument parsing,
//! dispatches the requested CLI action, and returns the process exit code.
//! This module exists for the crate's own binaries; it is `#[doc(hidden)]`
//! and outside the library's semver contract.

mod args;
mod profile_selection;
mod pty;
mod runtime;
mod stream;
mod trace;

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;

use is_terminal::IsTerminal;
use thiserror::Error;

use crate::config::{
    MAX_PROFILE_TEST_FILE_BYTES, PrismConfig, load_profile_file, read_bounded_regular_file,
};
use crate::highlight::{
    Highlighter, RuleMatchError, StreamingHighlighter, set_terminal_reads_utf8, strip_ansi,
    strip_string_escapes,
};
use crate::terminal_text::escape_untrusted;

use args::{Action, Options, parse_args, print_help};
use profile_selection::profile_store;
use pty::{WaitWhenWouldBlock, run_command};
use runtime::{register_for_reload, request_reload};
use stream::{InputSource, highlight_stream, prepare_session};
use trace::IoTrace;

#[cfg(feature = "completion-generation")]
#[doc(hidden)]
pub use args::completion_command;

/// Errors that can be reported by the command-line runner.
#[derive(Debug, Error)]
pub enum CliError {
    /// The user supplied invalid or incomplete command-line arguments.
    #[error("{0}")]
    Usage(String),
    /// Loading or parsing configuration failed.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// Highlight rule compilation failed.
    #[error(transparent)]
    Highlight(#[from] crate::highlight::HighlightError),
    /// A filesystem, stream, or terminal I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// PTY creation or command execution failed.
    #[error("PTY error: {0}")]
    Pty(#[from] anyhow::Error),
    /// The wrapped command could not be started.
    #[error("PTY error: {source}")]
    Spawn {
        /// Why spawning failed.
        source: anyhow::Error,
        /// The exit code to report: 127 not found, 126 not executable, else 1.
        code: u8,
    },
    /// Switching terminal modes failed.
    #[error("terminal mode error: {0}")]
    Terminal(#[from] nix::errno::Errno),
}

impl CliError {
    /// The process exit code: what a shell reports for a wrapped command that
    /// could not be started (127 not found, 126 not executable), otherwise 1.
    fn exit_code(&self) -> u8 {
        match self {
            Self::Spawn { code, .. } => *code,
            _ => 1,
        }
    }
}

/// Writes one diagnostic line to standard error. Best effort: a closed stderr
/// must not cost the highlighted output, and unlike `eprintln!` this never
/// panics.
///
/// A wrapped session puts the terminal in raw mode, where LF no longer returns
/// the cursor to the left edge, so a diagnostic that ended in a bare LF left
/// the lines after it starting mid-screen. On a terminal that does not add the
/// CR itself, the line ends in CR LF.
fn diagnostic(message: std::fmt::Arguments<'_>) {
    write_diagnostic_lines(format!("{message}\n"));
}

/// Writes whole diagnostic lines, each ending in LF, the way `diagnostic`
/// writes one.
fn write_diagnostic_lines(mut text: String) {
    if stderr_leaves_lf_bare() {
        text = text.replace('\n', "\r\n");
    }
    let _ = io::stderr().write_all(text.as_bytes());
}

fn stderr_leaves_lf_bare() -> bool {
    use nix::sys::termios::{OutputFlags, tcgetattr};

    tcgetattr(io::stderr()).is_ok_and(|terminal| {
        !terminal
            .output_flags
            .contains(OutputFlags::OPOST | OutputFlags::ONLCR)
    })
}

/// Exit status for output whose reader went away: 128 + SIGPIPE, what a
/// filter killed by SIGPIPE reports, so `set -o pipefail` sees the same code.
const BROKEN_PIPE_EXIT_STATUS: u8 = 141;

/// Runs the CLI using process arguments and returns the desired exit code.
pub fn run() -> ExitCode {
    match run_inner(std::env::args_os().skip(1).collect()) {
        Ok(code) => code,
        // `| head` or quitting `less` ends a pipeline; it is not an error worth
        // a message. PTY mode has already reaped the child by this point.
        Err(CliError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
            ExitCode::from(BROKEN_PIPE_EXIT_STATUS)
        }
        Err(error) => {
            let message = error.to_string();
            let mut stderr = io::stderr().lock();
            if message.is_empty() {
                let _ = writeln!(stderr, "prismtty:");
            } else {
                for line in message.lines() {
                    let _ = writeln!(stderr, "prismtty: {}", escape_untrusted(line));
                }
            }
            ExitCode::from(error.exit_code())
        }
    }
}

fn run_inner(args: Vec<OsString>) -> Result<ExitCode, CliError> {
    let (options, action) = parse_args(args)?;
    // Decided once from prismtty's own environment, before any output is
    // read. It never follows the stream and a reload does not revisit it.
    set_terminal_reads_utf8(locale_reads_utf8(
        std::env::var_os("LC_ALL"),
        std::env::var_os("LC_CTYPE"),
        std::env::var_os("LANG"),
    ));
    // Reports are written with `writeln!` rather than `println!`, which panics
    // when the reader has gone away instead of returning the error.
    match action {
        Action::Help => {
            print_help()?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Version => {
            writeln!(io::stdout(), "prismtty {}", env!("CARGO_PKG_VERSION"))?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Reload => {
            let count = request_reload()?;
            writeln!(io::stdout(), "Processes reloaded: {count}")?;
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesList => {
            let store = profile_store()?;
            let mut out = io::stdout().lock();
            for name in store.names() {
                writeln!(out, "{}", escape_untrusted(name))?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesShow(profile_name) => {
            let store = profile_store()?;
            let profile = store.profile(&profile_name).ok_or_else(|| {
                CliError::Usage(format!(
                    "unknown profile '{}'",
                    escape_untrusted(&profile_name)
                ))
            })?;
            let mut out = io::stdout().lock();
            writeln!(out, "profile: {}", escape_untrusted(&profile.name))?;
            if profile.inherits.is_empty() {
                writeln!(out, "inherits: none")?;
            } else {
                writeln!(
                    out,
                    "inherits: {}",
                    profile
                        .inherits
                        .iter()
                        .map(|name| escape_untrusted(name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )?;
            }
            if profile.detection.is_empty() {
                writeln!(out, "detection: none")?;
            } else {
                writeln!(
                    out,
                    "detection: {}",
                    profile
                        .detection
                        .iter()
                        .map(|hint| escape_untrusted(hint))
                        .collect::<Vec<_>>()
                        .join(", ")
                )?;
            }
            writeln!(out, "rules:")?;
            for rule in &profile.rules {
                writeln!(out, "  - {}", escape_untrusted(&rule.description))?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesValidate(path) => {
            let loaded = load_profile_file(&path)?;
            let mut store = profile_store()?;
            store
                .try_insert_user_profile(
                    loaded.meta.name.clone(),
                    loaded.meta.inherits.clone(),
                    loaded.meta.detection.clone(),
                    loaded.rules.clone(),
                )
                .map_err(|error| CliError::Usage(error.to_string()))?;
            let config = PrismConfig {
                rules: PrismConfig::from_profiles(&store, &[loaded.meta.name.as_str()])?.rules,
                enabled_profiles: vec![loaded.meta.name.clone()],
            };
            let _ = Highlighter::from_config(config)?;
            writeln!(
                io::stdout(),
                "profile {} valid",
                escape_untrusted(&loaded.meta.name)
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesTest { profile, fixture } => {
            let input = read_bounded_regular_file(&fixture, MAX_PROFILE_TEST_FILE_BYTES, "fixture")
                .map_err(|source| crate::config::ConfigError::Read {
                    path: fixture.clone(),
                    source,
                })?;
            // The fixture is printed to the terminal like program output, so
            // the flags that remove escapes from that output apply to it too.
            let input = if options.strip_ansi {
                strip_ansi(&input)
            } else if options.sanitize {
                strip_string_escapes(&input)
            } else {
                input
            };
            let store = profile_store()?;
            let config = PrismConfig::from_profiles(&store, &[profile.as_str()])?;
            let highlighter = Highlighter::from_config(config)?;
            let mut streaming = StreamingHighlighter::new(highlighter);
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            for chunk in input.chunks(64 * 1024) {
                let rendered = streaming.push(chunk);
                if let Some(rule) = streaming.first_rule_error() {
                    return Err(CliError::Usage(
                        RuleMatchError {
                            description: rule.description.to_string(),
                            message: rule.last.unwrap_or("PCRE2 match error").to_string(),
                        }
                        .to_string(),
                    ));
                }
                stdout.write_all(&rendered)?;
            }
            let rendered = streaming.finish();
            if let Some(rule) = streaming.first_rule_error() {
                return Err(CliError::Usage(
                    RuleMatchError {
                        description: rule.description.to_string(),
                        message: rule.last.unwrap_or("PCRE2 match error").to_string(),
                    }
                    .to_string(),
                ));
            }
            stdout.write_all(&rendered)?;
            // Output with no final line end is still buffered, and the flush
            // at exit would drop its error.
            stdout.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Stdin => run_stdin(options),
        Action::Run(command) => run_command(options, command),
    }
}

fn run_stdin(options: Options) -> Result<ExitCode, CliError> {
    let stdout = io::stdout();
    let interactive = stdin_mode_interactive_highlighting(stdout.is_terminal());
    // Reject a bad config before reading: with empty input it would otherwise
    // never be loaded, and with input it would cost the first chunk.
    let prepared = prepare_session(&options, interactive)?;
    let trace = IoTrace::open(options.trace_io.as_deref())?;
    let (_registration, reload_watcher) = register_for_reload();
    let stdin = io::stdin();
    // As in PTY mode, another program on the terminal or pipe can leave
    // stdin and stdout non-blocking.
    let mut stdout = WaitWhenWouldBlock {
        fd: stdout.as_raw_fd(),
        inner: stdout,
    };
    highlight_stream(
        WaitWhenWouldBlock {
            fd: stdin.as_raw_fd(),
            inner: stdin.lock(),
        },
        &mut stdout,
        prepared,
        InputSource {
            interactive,
            pty_fd: None,
            // The stream reads 8 KiB at a time, no less than the stdin lock's
            // buffer, so each read goes straight to the descriptor and a poll
            // of it sees everything not yet read.
            idle_fd: interactive.then(|| stdin.as_raw_fd()),
            recent_input: None,
        },
        reload_watcher,
        trace,
        None,
    )?;
    Ok(ExitCode::SUCCESS)
}

fn stdin_mode_interactive_highlighting(stdout_is_terminal: bool) -> bool {
    stdout_is_terminal
}

/// Whether the locale says the terminal reads UTF-8. The first of `LC_ALL`,
/// `LC_CTYPE` and `LANG` that is set and not empty decides, in POSIX order,
/// and its codeset must be UTF-8: the part after the `.` (`en_US.UTF-8`,
/// `C.utf8`), or the whole value when it has none (`LC_CTYPE=UTF-8` on
/// macOS). No locale at all is not a UTF-8 one.
fn locale_reads_utf8(
    lc_all: Option<OsString>,
    lc_ctype: Option<OsString>,
    lang: Option<OsString>,
) -> bool {
    let Some(locale) = [lc_all, lc_ctype, lang]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
    else {
        return false;
    };
    let locale = locale.to_string_lossy();
    let without_modifier = locale.split('@').next().unwrap_or_default();
    let codeset = without_modifier
        .split_once('.')
        .map_or(without_modifier, |(_, codeset)| codeset);
    codeset.eq_ignore_ascii_case("utf-8") || codeset.eq_ignore_ascii_case("utf8")
}

#[cfg(test)]
mod tests {
    #[test]
    fn stdin_mode_uses_interactive_highlighting_when_output_is_terminal() {
        assert!(super::stdin_mode_interactive_highlighting(true));
        assert!(!super::stdin_mode_interactive_highlighting(false));
    }

    #[test]
    fn locale_reads_utf8_follows_posix_precedence_and_the_codeset() {
        let reads = |lc_all: Option<&str>, lc_ctype: Option<&str>, lang: Option<&str>| {
            super::locale_reads_utf8(
                lc_all.map(Into::into),
                lc_ctype.map(Into::into),
                lang.map(Into::into),
            )
        };

        for locale in [
            "en_US.UTF-8",
            "C.UTF-8",
            "en_US.utf8",
            "UTF-8",
            "de_DE.UTF-8@euro",
        ] {
            assert!(reads(None, None, Some(locale)), "LANG={locale}");
            assert!(reads(None, Some(locale), None), "LC_CTYPE={locale}");
            assert!(reads(Some(locale), None, None), "LC_ALL={locale}");
        }
        for locale in [
            "C",
            "POSIX",
            "en_US",
            "en_US.ISO-8859-1",
            "ja_JP.eucJP",
            "utf8_user",
        ] {
            assert!(!reads(None, None, Some(locale)), "LANG={locale}");
        }
        // No locale at all is not a UTF-8 one.
        assert!(!reads(None, None, None));
        assert!(!reads(Some(""), Some(""), Some("")));
        // The first one that is set and not empty decides.
        assert!(!reads(Some("C"), Some("UTF-8"), Some("en_US.UTF-8")));
        assert!(reads(Some(""), Some("UTF-8"), Some("C")));
        assert!(!reads(None, Some("C"), Some("en_US.UTF-8")));
        assert!(reads(Some("en_US.UTF-8"), Some("C"), Some("C")));
    }

    #[test]
    fn pty_module_only_imports_highlight_stream_from_stream_module() {
        let source = include_str!("cli/pty.rs");

        assert!(
            !source.contains("run_stdin"),
            "stdin orchestration should stay outside pty.rs"
        );
    }
}
