mod args;
mod profile_selection;
mod pty;
mod runtime;
mod stream;
mod trace;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::process::ExitCode;

use thiserror::Error;

use crate::config::{PrismConfig, load_profile_file};
use crate::highlight::Highlighter;

use args::{Action, parse_args, print_help};
use profile_selection::profile_store;
use pty::run_command;
use runtime::request_reload;
use stream::run_stdin;

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
