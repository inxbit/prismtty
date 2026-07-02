use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgAction, CommandFactory, Parser};

use super::CliError;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Options {
    pub(super) profiles: Vec<String>,
    pub(super) no_auto_detect: bool,
    pub(super) config: Option<PathBuf>,
    pub(super) strip_ansi: bool,
    pub(super) sanitize: bool,
    pub(super) force_rgb: bool,
    pub(super) benchmark: bool,
    pub(super) show_profile: bool,
    pub(super) local_echo: bool,
    pub(super) no_dynamic_profile: bool,
    pub(super) no_minimal_reset: bool,
    pub(super) trace_io: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
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

#[derive(Debug, Parser)]
#[command(
    name = "prismtty",
    version,
    about = "Fast terminal output highlighter focused on network devices and Unix systems",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true,
    trailing_var_arg = true,
    after_help = "\
PROFILE COMMANDS:
  prismtty profiles list
  prismtty profiles show <PROFILE>
  prismtty profiles validate <FILE>
  prismtty profiles test <PROFILE> <FILE>"
)]
struct RawArgs {
    #[arg(short = 'h', long = "help", action = ArgAction::SetTrue, help = "Show help")]
    help: bool,
    #[arg(
        short = 'V',
        visible_short_alias = 'v',
        long = "version",
        action = ArgAction::SetTrue,
        help = "Show version"
    )]
    version: bool,
    #[arg(short = 'b', long = "benchmark", action = ArgAction::SetTrue, help = "Print per-rule timing and match-count data to stderr")]
    benchmark: bool,
    #[arg(short = 'r', long = "reload", action = ArgAction::SetTrue, help = "Ask running PrismTTY sessions to reload config")]
    reload: bool,
    #[arg(short = 'R', long = "rgb", action = ArgAction::SetTrue, help = "Force RGB color output")]
    force_rgb: bool,
    #[arg(long = "pcre", action = ArgAction::SetTrue, help = "Accepted for ChromaTerm compatibility; PCRE2 is always used")]
    pcre: bool,
    #[arg(long = "no-auto-detect", action = ArgAction::SetTrue, help = "Use only the generic profile unless --profile is set")]
    no_auto_detect: bool,
    #[arg(long = "no-dynamic-profile", action = ArgAction::SetTrue, help = "Disable profile switching inside wrapped interactive shells")]
    no_dynamic_profile: bool,
    #[arg(long = "no-minimal-reset", action = ArgAction::SetTrue, help = "Use full SGR resets instead of minimal foreground/background resets in interactive streams")]
    no_minimal_reset: bool,
    #[arg(long = "strip-ansi", action = ArgAction::SetTrue, help = "Remove existing ANSI before applying PrismTTY styles")]
    strip_ansi: bool,
    #[arg(long = "sanitize", action = ArgAction::SetTrue, help = "Strip window-title, clipboard (OSC 52), and other OSC/DCS string escapes from program output")]
    sanitize: bool,
    #[arg(long = "show-profile", action = ArgAction::SetTrue, help = "Print selected profiles to stderr")]
    show_profile: bool,
    #[arg(long = "local-echo", action = ArgAction::SetTrue, help = "Locally echo typed printable keys for no-echo device sessions (also echoes secrets typed at hidden prompts)")]
    local_echo: bool,
    #[arg(
        long = "trace-io",
        value_name = "FILE",
        help = "Append hex-encoded PTY input/output diagnostics (records all keystrokes, including passwords)"
    )]
    trace_io: Option<PathBuf>,
    #[arg(short = 'p', long = "profile", value_name = "NAME", action = ArgAction::Append, help = "Force a profile; repeat to enable several")]
    profiles: Vec<String>,
    #[arg(
        short = 'c',
        long = "config",
        value_name = "FILE",
        help = "Load a ChromaTerm-compatible YAML config"
    )]
    config: Option<PathBuf>,
    #[arg(value_name = "COMMAND", trailing_var_arg = true)]
    command: Vec<OsString>,
}

pub(super) fn parse_args(args: Vec<OsString>) -> Result<(Options, Action), CliError> {
    let command_forced_by_delimiter = args
        .iter()
        .position(|arg| arg == "--")
        .and_then(|idx| args.get(idx + 1))
        .cloned();
    let raw = RawArgs::try_parse_from(std::iter::once(OsString::from("prismtty")).chain(args))
        .map_err(|error| CliError::Usage(error.to_string()))?;

    if raw.help {
        return Ok((Options::default(), Action::Help));
    }
    if raw.version {
        return Ok((Options::default(), Action::Version));
    }
    if raw.reload {
        return Ok((Options::default(), Action::Reload));
    }

    // ChromaTerm compatibility: PCRE2 is always used, so this flag has no
    // representation in Options and must remain a parser-only no-op.
    let _ = raw.pcre;

    let options = Options {
        profiles: raw.profiles,
        no_auto_detect: raw.no_auto_detect,
        config: raw.config,
        strip_ansi: raw.strip_ansi,
        sanitize: raw.sanitize,
        force_rgb: raw.force_rgb,
        benchmark: raw.benchmark,
        show_profile: raw.show_profile,
        local_echo: raw.local_echo,
        no_dynamic_profile: raw.no_dynamic_profile,
        no_minimal_reset: raw.no_minimal_reset || detect_no_minimal_reset_env(),
        trace_io: raw.trace_io,
    };

    if raw.command.first().is_some_and(|arg| arg == "profiles")
        && command_forced_by_delimiter.as_ref() != raw.command.first()
    {
        return parse_profiles_command(options, &raw.command[1..]);
    }

    if raw.command.is_empty() {
        return Ok((options, Action::Stdin));
    }

    Ok((options, Action::Run(raw.command)))
}

fn detect_no_minimal_reset_env() -> bool {
    env_flag_enabled("PRISMTTY_NO_MINIMAL_RESET") || env_flag_enabled("PRISMTTY_NO_39_49_RESETS")
}

fn env_flag_enabled(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| env_flag_value_enabled(&value.to_string_lossy()))
}

/// Treats a falsy env value ("0", "false", "no", "off", or empty) as disabled,
/// so the flag can be turned *off* through the environment and not only on
fn env_flag_value_enabled(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn parse_profiles_command(
    options: Options,
    args: &[OsString],
) -> Result<(Options, Action), CliError> {
    // A conventional `--` separator carries no meaning inside `profiles`
    // arguments (nothing after it is a command to spawn); strip the first one
    // so `profiles test cisco -- fixture` reads the fixture, not a file
    // literally named `--`.
    let mut args: Vec<&OsString> = args.iter().collect();
    if let Some(delimiter) = args.iter().position(|arg| *arg == "--") {
        args.remove(delimiter);
    }
    let subcommand = args
        .first()
        .map(|arg| arg.to_string_lossy().to_string())
        .unwrap_or_else(|| "list".to_string());

    match subcommand.as_str() {
        "list" => Ok((options, Action::ProfilesList)),
        "show" => {
            let profile = args.get(1).ok_or_else(|| {
                CliError::Usage("profiles show required value: profile name".to_string())
            })?;
            Ok((
                options,
                Action::ProfilesShow(profile.to_string_lossy().to_string()),
            ))
        }
        "validate" => {
            let path = args.get(1).ok_or_else(|| {
                CliError::Usage("profiles validate required value: profile path".to_string())
            })?;
            Ok((options, Action::ProfilesValidate(PathBuf::from(path))))
        }
        "test" => {
            let profile = args.get(1).ok_or_else(|| {
                CliError::Usage("profiles test required value: profile name".to_string())
            })?;
            let fixture = args.get(2).ok_or_else(|| {
                CliError::Usage("profiles test required value: fixture path".to_string())
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

pub(super) fn print_help() {
    let mut command = RawArgs::command();
    command.print_help().expect("help writes to stdout");
    println!();
}

#[cfg(feature = "completion-generation")]
#[doc(hidden)]
pub fn completion_command() -> clap::Command {
    // Keep this completion-only profiles command in sync with parse_profiles_command.
    RawArgs::command().subcommand(
        clap::Command::new("profiles")
            .about("Manage profiles")
            .disable_help_subcommand(true)
            .subcommand(clap::Command::new("list").about("List available profiles"))
            .subcommand(
                clap::Command::new("show")
                    .about("Show a profile")
                    .arg(clap::Arg::new("PROFILE").required(true)),
            )
            .subcommand(
                clap::Command::new("validate")
                    .about("Validate a profile file")
                    .arg(clap::Arg::new("FILE").required(true)),
            )
            .subcommand(
                clap::Command::new("test")
                    .about("Highlight a fixture with a profile")
                    .arg(clap::Arg::new("PROFILE").required(true))
                    .arg(clap::Arg::new("FILE").required(true)),
            ),
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::Mutex;

    #[test]
    fn env_flag_value_distinguishes_on_and_off() {
        for on in ["1", "true", "yes", "on", "enabled"] {
            assert!(
                super::env_flag_value_enabled(on),
                "{on:?} should be enabled"
            );
        }
        for off in ["0", "false", "no", "off", "", "  0  "] {
            assert!(
                !super::env_flag_value_enabled(off),
                "{off:?} should be disabled"
            );
        }
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        previous_preferred: Option<OsString>,
        previous_legacy: Option<OsString>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: tests execute under `ENV_LOCK`, preventing concurrent env races.
            unsafe {
                match &self.previous_preferred {
                    Some(value) => std::env::set_var("PRISMTTY_NO_MINIMAL_RESET", value),
                    None => std::env::remove_var("PRISMTTY_NO_MINIMAL_RESET"),
                }
                match &self.previous_legacy {
                    Some(value) => std::env::set_var("PRISMTTY_NO_39_49_RESETS", value),
                    None => std::env::remove_var("PRISMTTY_NO_39_49_RESETS"),
                }
            }
        }
    }

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn usage_message(args: &[&str]) -> String {
        match super::parse_args(os_args(args)) {
            Err(super::CliError::Usage(message)) => message,
            Ok((_options, action)) => panic!("expected usage error, got action {action:?}"),
            Err(error) => panic!("expected usage error, got {error}"),
        }
    }

    fn with_no_minimal_reset_env<R>(value: Option<&str>, test: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().expect("env lock is never poisoned");
        let previous_preferred = std::env::var_os("PRISMTTY_NO_MINIMAL_RESET");
        let previous_legacy = std::env::var_os("PRISMTTY_NO_39_49_RESETS");
        let guard = EnvGuard {
            previous_preferred,
            previous_legacy,
        };

        if let Some(value) = value {
            // SAFETY: tests execute under `ENV_LOCK`, preventing concurrent env races.
            unsafe {
                std::env::set_var("PRISMTTY_NO_MINIMAL_RESET", value);
                std::env::remove_var("PRISMTTY_NO_39_49_RESETS");
            }
        } else {
            // SAFETY: tests execute under `ENV_LOCK`, preventing concurrent env races.
            unsafe {
                std::env::remove_var("PRISMTTY_NO_MINIMAL_RESET");
                std::env::remove_var("PRISMTTY_NO_39_49_RESETS");
            }
        }

        let result = test();
        drop(guard);
        result
    }

    fn with_legacy_no_minimal_reset_env<R>(value: Option<&str>, test: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().expect("env lock is never poisoned");
        let previous_preferred = std::env::var_os("PRISMTTY_NO_MINIMAL_RESET");
        let previous_legacy = std::env::var_os("PRISMTTY_NO_39_49_RESETS");
        let guard = EnvGuard {
            previous_preferred,
            previous_legacy,
        };

        // SAFETY: tests execute under `ENV_LOCK`, preventing concurrent env races.
        unsafe { std::env::remove_var("PRISMTTY_NO_MINIMAL_RESET") };

        if let Some(value) = value {
            // SAFETY: tests execute under `ENV_LOCK`, preventing concurrent env races.
            unsafe { std::env::set_var("PRISMTTY_NO_39_49_RESETS", value) };
        } else {
            // SAFETY: tests execute under `ENV_LOCK`, preventing concurrent env races.
            unsafe { std::env::remove_var("PRISMTTY_NO_39_49_RESETS") };
        }

        let result = test();
        drop(guard);
        result
    }

    #[test]
    fn parser_contract_pcre_is_a_true_noop() {
        with_no_minimal_reset_env(None, || {
            let without_pcre = super::parse_args(os_args(&[
                "--profile",
                "generic",
                "--profile",
                "cisco",
                "ssh",
                "r1",
            ]))
            .expect("args parse without --pcre");
            let with_pcre = super::parse_args(os_args(&[
                "--pcre",
                "--profile",
                "generic",
                "--profile",
                "cisco",
                "ssh",
                "r1",
            ]))
            .expect("args parse with --pcre");

            assert_eq!(with_pcre, without_pcre);
        });
    }

    #[test]
    fn parser_contract_version_aliases_map_to_version_action() {
        for flag in ["-v", "-V", "--version"] {
            let (_options, action) =
                super::parse_args(os_args(&[flag])).expect("version flag parses");
            assert_eq!(action, super::Action::Version);
        }
    }

    #[test]
    fn parser_contract_double_dash_takes_exact_remaining_command() {
        let (options, action) =
            super::parse_args(os_args(&["--profile", "cisco", "--", "-literal", "--flag"]))
                .expect("double dash command parses");

        assert_eq!(options.profiles, vec!["cisco".to_string()]);
        assert_eq!(action, super::Action::Run(os_args(&["-literal", "--flag"])));
    }

    #[test]
    fn parser_contract_double_dash_allows_wrapped_profiles_command() {
        let (options, action) =
            super::parse_args(os_args(&["--profile", "cisco", "--", "profiles", "list"]))
                .expect("double dash command parses");

        assert_eq!(options.profiles, vec!["cisco".to_string()]);
        assert_eq!(action, super::Action::Run(os_args(&["profiles", "list"])));
    }

    #[test]
    fn parser_contract_first_non_flag_starts_command_without_double_dash() {
        let (options, action) =
            super::parse_args(os_args(&["ssh", "--profile", "cisco", "router"]))
                .expect("positional command parses");

        assert!(options.profiles.is_empty());
        assert_eq!(
            action,
            super::Action::Run(os_args(&["ssh", "--profile", "cisco", "router"]))
        );
    }

    #[test]
    fn parser_contract_double_dash_inside_profiles_subcommand_is_stripped() {
        let (_options, action) =
            super::parse_args(os_args(&["profiles", "test", "cisco", "--", "fixture.txt"]))
                .expect("profiles test with delimiter parses");
        assert_eq!(
            action,
            super::Action::ProfilesTest {
                profile: "cisco".to_string(),
                fixture: std::path::PathBuf::from("fixture.txt"),
            }
        );

        let (_options, action) =
            super::parse_args(os_args(&["profiles", "validate", "--", "custom.yml"]))
                .expect("profiles validate with delimiter parses");
        assert_eq!(
            action,
            super::Action::ProfilesValidate(std::path::PathBuf::from("custom.yml"))
        );
    }

    #[test]
    fn parser_contract_profiles_subcommands_parse_after_global_options() {
        let (options, action) = super::parse_args(os_args(&[
            "--profile",
            "generic",
            "profiles",
            "show",
            "cisco",
        ]))
        .expect("profiles subcommand parses");

        assert_eq!(options.profiles, vec!["generic".to_string()]);
        assert_eq!(action, super::Action::ProfilesShow("cisco".to_string()));
    }

    #[test]
    fn parser_contract_repeated_profile_preserves_order() {
        let (options, action) =
            super::parse_args(os_args(&["--profile", "generic", "-p", "juniper"]))
                .expect("repeated profile parses");

        assert_eq!(
            options.profiles,
            vec!["generic".to_string(), "juniper".to_string()]
        );
        assert_eq!(action, super::Action::Stdin);
    }

    #[test]
    fn parser_contract_sanitize_sets_option() {
        let (options, action) = super::parse_args(os_args(&["--sanitize"])).expect("flag parses");

        assert!(options.sanitize);
        assert_eq!(action, super::Action::Stdin);
    }

    #[test]
    fn parser_contract_no_minimal_reset_sets_option() {
        let (options, action) =
            super::parse_args(os_args(&["--no-minimal-reset"])).expect("flag parses");

        assert!(options.no_minimal_reset);
        assert_eq!(action, super::Action::Stdin);
    }

    #[test]
    fn parser_contract_no_minimal_reset_env_sets_option() {
        with_no_minimal_reset_env(Some("1"), || {
            let (options, action) = super::parse_args(os_args(&[])).expect("empty parse");

            assert!(options.no_minimal_reset);
            assert_eq!(action, super::Action::Stdin);
        });
    }

    #[test]
    fn parser_contract_legacy_no_minimal_reset_env_sets_option() {
        with_legacy_no_minimal_reset_env(Some("1"), || {
            let (options, action) = super::parse_args(os_args(&[])).expect("empty parse");

            assert!(options.no_minimal_reset);
            assert_eq!(action, super::Action::Stdin);
        });
    }

    #[test]
    fn parser_contract_reload_short_circuits_and_ignores_later_args() {
        with_no_minimal_reset_env(None, || {
            let (options, action) =
                super::parse_args(os_args(&["-r", "--profile", "cisco", "ssh", "router"]))
                    .expect("reload parses");

            assert_eq!(options, super::Options::default());
            assert_eq!(action, super::Action::Reload);
        });
    }

    #[test]
    fn parser_contract_missing_profile_value_is_usage_error_for_profile_flag() {
        let message = usage_message(&["--profile"]);

        assert!(message.contains("--profile"));
    }

    #[test]
    fn parser_contract_missing_config_value_is_usage_error_for_config_flag() {
        let message = usage_message(&["--config"]);

        assert!(message.contains("--config"));
    }

    #[test]
    fn parser_contract_missing_trace_io_value_is_usage_error_for_trace_io_flag() {
        let message = usage_message(&["--trace-io"]);

        assert!(message.contains("--trace-io"));
    }

    #[test]
    fn parser_contract_unknown_flag_is_usage_error_for_unknown_flag() {
        let message = usage_message(&["--not-a-real-flag"]);

        assert!(message.contains("--not-a-real-flag"));
    }

    #[test]
    fn parser_contract_profiles_show_without_name_is_usage_error_for_show() {
        let message = usage_message(&["profiles", "show"]);

        assert!(message.contains("profiles show"));
    }
}
