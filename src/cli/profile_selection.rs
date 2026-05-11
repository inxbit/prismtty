use std::fs;
use std::path::{Path, PathBuf};

use directories::BaseDirs;

use crate::config::{PrismConfig, load_profile_file};
use crate::highlight::{Highlighter, strip_ansi};
use crate::profiles::ProfileStore;
use crate::style::ColorMode;

use super::CliError;
use super::args::Options;

pub(super) fn build_highlighter_for_profiles(
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

pub(super) fn select_profile_names(
    options: &Options,
    sample: &[u8],
) -> Result<Vec<String>, CliError> {
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

pub(super) fn build_config_for_profiles(
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

pub(super) fn auto_detect_enabled(options: &Options) -> bool {
    options.profiles.is_empty() && !options.no_auto_detect
}

pub(super) fn dynamic_profile_enabled(options: &Options, interactive: bool) -> bool {
    interactive && auto_detect_enabled(options) && !options.no_dynamic_profile
}

pub(super) fn should_continue_auto_detect(options: &Options, profile_names: &[String]) -> bool {
    auto_detect_enabled(options)
        && profile_names.len() == 1
        && profile_names
            .first()
            .is_some_and(|profile| profile == "generic")
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

pub(super) fn profile_store() -> Result<ProfileStore, CliError> {
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

#[cfg(test)]
mod tests {
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
