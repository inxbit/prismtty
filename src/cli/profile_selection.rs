use std::fs;
use std::path::{Path, PathBuf};

use directories::BaseDirs;

use crate::config::{PrismConfig, parse_profile_file_contents, read_profile_file_contents};
use crate::highlight::{Highlighter, RuleIdentityRegistry, strip_ansi};
use crate::profiles::{ProfileStore, is_generic_profile_set};
use crate::style::ColorMode;
use crate::terminal_text::escape_untrusted;

use super::CliError;
use super::args::Options;

/// User profile directories are configuration inputs, not bulk data stores.
/// Bound both directory fan-out and aggregate bytes before parsing any file so
/// startup and dynamic detection cannot be made to retain an unbounded profile
/// graph from a hostile or accidentally populated profiles.d directory.
const MAX_PROFILES_D_FILES: usize = 128;
const MAX_PROFILES_D_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PROFILES_D_ENTRIES: usize = 1024;

pub(super) fn build_highlighter_for_profiles_with_store(
    options: &Options,
    store: &ProfileStore,
    profile_names: &[String],
    interactive: bool,
    overlay_config: &PrismConfig,
    rule_identities: &mut RuleIdentityRegistry,
) -> Result<Highlighter, CliError> {
    let config = build_config_for_profiles_with_store(store, profile_names, overlay_config)?;
    Ok(Highlighter::from_config_with_color_mode_and_identities(
        config,
        color_mode(options, interactive),
        rule_identities,
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

pub(super) fn select_profile_names_with_store(
    options: &Options,
    store: &ProfileStore,
    sample: &[u8],
) -> Result<Vec<String>, CliError> {
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

pub(super) fn build_config_for_profiles_with_store(
    store: &ProfileStore,
    profile_names: &[String],
    overlay_config: &PrismConfig,
) -> Result<PrismConfig, CliError> {
    let profile_refs: Vec<&str> = profile_names.iter().map(String::as_str).collect();
    Ok(PrismConfig::from_profiles(store, &profile_refs)?.merge(overlay_config.clone()))
}

pub(super) fn load_overlay_config(options: &Options) -> Result<PrismConfig, CliError> {
    let mut config = PrismConfig::default();
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

pub(super) fn auto_detect_enabled(options: &Options) -> bool {
    options.profiles.is_empty() && !options.no_auto_detect
}

pub(super) fn dynamic_profile_enabled(options: &Options, interactive: bool) -> bool {
    interactive && auto_detect_enabled(options) && !options.no_dynamic_profile
}

pub(super) fn should_continue_auto_detect(options: &Options, profile_names: &[String]) -> bool {
    auto_detect_enabled(options) && is_generic_profile_set(profile_names)
}

pub(super) struct ProfileReporter {
    show_profile: bool,
    auto_detect: bool,
    last_reported: Option<Vec<String>>,
}

impl ProfileReporter {
    pub(super) fn new(show_profile: bool, auto_detect: bool) -> Self {
        Self {
            show_profile,
            auto_detect,
            last_reported: None,
        }
    }

    pub(super) fn report(&mut self, profile_names: &[String]) {
        if let Some(message) = self.message_for(profile_names) {
            eprintln!("{message}");
        }
    }

    fn message_for(&mut self, profile_names: &[String]) -> Option<String> {
        if !self.show_profile {
            return None;
        }
        if self.auto_detect && is_generic_profile_set(profile_names) && self.last_reported.is_none()
        {
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
        let display_names = profile_names
            .iter()
            .map(|name| escape_untrusted(name))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("prismtty: profiles selected: {}", display_names))
    }
}

pub(super) fn profile_store() -> Result<ProfileStore, CliError> {
    let mut store = ProfileStore::builtin();
    for loaded in load_profiles_d()? {
        let name = loaded.meta.name.clone();
        let shadowed = store
            .try_insert_user_profile(
                loaded.meta.name,
                loaded.meta.inherits,
                loaded.meta.detection,
                loaded.rules,
            )
            .map_err(|error| CliError::Usage(error.to_string()))?;
        if shadowed {
            let name = escape_untrusted(&name);
            eprintln!(
                "prismtty: user profile '{name}' from profiles.d overrides the built-in profile of the same name"
            );
        }
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
    let Some(config_dir) = config_base_dir() else {
        return Ok(Vec::new());
    };
    load_profiles_d_from_config_dir(&config_dir)
}

fn load_profiles_d_from_config_dir(
    config_dir: &Path,
) -> Result<Vec<crate::config::LoadedProfileFile>, CliError> {
    let mut profiles = Vec::new();
    let dir = config_dir.join("prismtty").join("profiles.d");
    if !dir.exists() {
        return Ok(profiles);
    }

    let mut entries = Vec::new();
    let mut directory_entry_count = 0_usize;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        directory_entry_count = directory_entry_count.saturating_add(1);
        enforce_profiles_d_entry_budget(directory_entry_count)?;
        let path = entry.path();
        // `fs::metadata` follows symlinks (unlike `DirEntry::file_type`) so
        // symlinked profiles install by dotfile managers are discovered; a
        // broken symlink is skipped like any other non-file entry.
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.is_file() && is_yaml(&path) {
            enforce_profiles_d_budget(entries.len() + 1, 0)?;
            entries.push(path);
        }
    }
    entries.sort();

    // Read each path exactly once, retain that bounded snapshot, and enforce
    // the aggregate against bytes actually read before parsing any YAML. This
    // prevents a symlink target or regular file from growing/swapping between
    // a metadata pass and a later reopen.
    let mut sources = Vec::with_capacity(entries.len());
    let mut aggregate_bytes = 0_u64;
    for path in entries {
        let input = read_profile_file_contents(&path)?;
        aggregate_bytes = aggregate_bytes.saturating_add(input.len() as u64);
        enforce_profiles_d_budget(sources.len() + 1, aggregate_bytes)?;
        sources.push((path, input));
    }

    for (path, input) in sources {
        let loaded = parse_profile_file_contents(&path, &input)?;
        profiles.push(loaded);
    }

    Ok(profiles)
}

fn enforce_profiles_d_budget(file_count: usize, aggregate_bytes: u64) -> Result<(), CliError> {
    if file_count > MAX_PROFILES_D_FILES {
        return Err(CliError::Usage(format!(
            "profiles.d contains {file_count} profile files; limit is {MAX_PROFILES_D_FILES}"
        )));
    }
    if aggregate_bytes > MAX_PROFILES_D_BYTES {
        return Err(CliError::Usage(format!(
            "profiles.d aggregate size is {aggregate_bytes} bytes; limit is {MAX_PROFILES_D_BYTES} bytes"
        )));
    }
    Ok(())
}

fn enforce_profiles_d_entry_budget(entry_count: usize) -> Result<(), CliError> {
    if entry_count > MAX_PROFILES_D_ENTRIES {
        return Err(CliError::Usage(format!(
            "profiles.d contains more than {MAX_PROFILES_D_ENTRIES} directory entries"
        )));
    }
    Ok(())
}

fn config_base_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    BaseDirs::new().map(|base_dirs| base_dirs.home_dir().join(".config"))
}

fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("yml" | "yaml")
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn profiles_d_budget_accepts_exact_limits() {
        assert!(
            super::enforce_profiles_d_budget(
                super::MAX_PROFILES_D_FILES,
                super::MAX_PROFILES_D_BYTES,
            )
            .is_ok()
        );
    }

    #[test]
    fn profiles_d_budget_rejects_excess_files_or_bytes() {
        let too_many = super::enforce_profiles_d_budget(
            super::MAX_PROFILES_D_FILES + 1,
            super::MAX_PROFILES_D_BYTES,
        )
        .expect_err("one extra profile file must be rejected")
        .to_string();
        assert!(too_many.contains("profile files"), "{too_many}");

        let too_large = super::enforce_profiles_d_budget(
            super::MAX_PROFILES_D_FILES,
            super::MAX_PROFILES_D_BYTES + 1,
        )
        .expect_err("one extra aggregate byte must be rejected")
        .to_string();
        assert!(too_large.contains("aggregate size"), "{too_large}");
    }

    #[test]
    fn profiles_d_directory_entry_budget_has_an_exact_boundary() {
        assert!(super::enforce_profiles_d_entry_budget(super::MAX_PROFILES_D_ENTRIES).is_ok());
        let error = super::enforce_profiles_d_entry_budget(super::MAX_PROFILES_D_ENTRIES + 1)
            .expect_err("one extra directory entry must be rejected")
            .to_string();
        assert!(error.contains("directory entries"), "{error}");
    }

    #[test]
    fn load_profiles_d_rejects_excess_files_before_parsing() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let profiles_dir = temp.path().join("prismtty").join("profiles.d");
        fs::create_dir_all(&profiles_dir).expect("profiles.d creates");
        for index in 0..=super::MAX_PROFILES_D_FILES {
            fs::write(profiles_dir.join(format!("{index:03}.yaml")), b"")
                .expect("empty profile placeholder writes");
        }

        let error = super::load_profiles_d_from_config_dir(temp.path())
            .expect_err("directory fan-out must be rejected before YAML parsing")
            .to_string();
        assert!(error.contains("profile files"), "{error}");
    }

    #[test]
    fn load_profiles_d_stops_enumerating_excess_non_profile_entries() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let profiles_dir = temp.path().join("prismtty").join("profiles.d");
        fs::create_dir_all(&profiles_dir).expect("profiles.d creates");
        for index in 0..=super::MAX_PROFILES_D_ENTRIES {
            fs::write(profiles_dir.join(format!("backup-{index:04}.txt")), b"")
                .expect("non-profile placeholder writes");
        }

        let error = super::load_profiles_d_from_config_dir(temp.path())
            .expect_err("directory traversal fan-out must be bounded")
            .to_string();
        assert!(error.contains("directory entries"), "{error}");
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
    fn profile_reporter_waits_for_auto_detect_promotion() {
        let mut reporter = super::ProfileReporter::new(true, true);

        assert!(reporter.message_for(&["generic".to_string()]).is_none());
        assert_eq!(
            reporter.message_for(&["generic".to_string(), "cisco".to_string()]),
            Some("prismtty: profiles selected: generic, cisco".to_string())
        );
    }

    #[test]
    fn profile_reporter_escapes_untrusted_profile_names() {
        let mut reporter = super::ProfileReporter::new(true, false);
        let message = reporter
            .message_for(&["router\u{1b}]0;title\u{7}".to_string()])
            .expect("forced profile is reported");

        assert_eq!(
            message,
            "prismtty: profiles selected: router\\x1b]0;title\\x07"
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

    #[test]
    fn load_profiles_d_skips_yaml_named_directories() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let profiles_dir = temp.path().join("prismtty").join("profiles.d");
        fs::create_dir_all(profiles_dir.join("backup.yaml")).expect("yaml directory creates");
        fs::write(
            profiles_dir.join("router.yml"),
            r##"
profile:
  name: router
rules:
  - description: router prompt
    regex: '^router#'
    color: f#ffffff
"##,
        )
        .expect("profile writes");

        let profiles =
            super::load_profiles_d_from_config_dir(temp.path()).expect("profiles.d loads");

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].meta.name, "router");
    }

    // Dotfile managers (stow, chezmoi) install profiles.d entries as symlinks.
    // Discovery must follow them; `DirEntry::file_type()` does not, which used
    // to silently skip every symlinked profile.
    #[test]
    fn load_profiles_d_follows_symlinked_profile_files() {
        let temp = tempfile::tempdir().expect("tempdir creates");
        let profiles_dir = temp.path().join("prismtty").join("profiles.d");
        fs::create_dir_all(&profiles_dir).expect("profiles.d creates");
        let target = temp.path().join("router-target.yml");
        fs::write(
            &target,
            r##"
profile:
  name: router
rules:
  - description: router prompt
    regex: '^router#'
    color: f#ffffff
"##,
        )
        .expect("profile target writes");
        std::os::unix::fs::symlink(&target, profiles_dir.join("router.yml"))
            .expect("symlink creates");

        let profiles =
            super::load_profiles_d_from_config_dir(temp.path()).expect("profiles.d loads");

        assert_eq!(
            profiles.len(),
            1,
            "symlinked profile file must be discovered"
        );
        assert_eq!(profiles[0].meta.name, "router");
    }
}
