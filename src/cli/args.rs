use std::ffi::OsString;
use std::path::PathBuf;

use super::CliError;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Options {
    pub(super) profiles: Vec<String>,
    pub(super) no_auto_detect: bool,
    pub(super) config: Option<PathBuf>,
    pub(super) strip_ansi: bool,
    pub(super) force_rgb: bool,
    pub(super) benchmark: bool,
    pub(super) show_profile: bool,
    pub(super) local_echo: bool,
    pub(super) no_dynamic_profile: bool,
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

pub(super) fn parse_args(args: Vec<OsString>) -> Result<(Options, Action), CliError> {
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

pub(super) fn print_help() {
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
    use std::ffi::OsString;

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn parser_contract_pcre_is_a_true_noop() {
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
}
