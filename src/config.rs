use crate::profiles::{ProfileRuntimeMeta, ProfileStore};
use crate::style::{Style, parse_palette};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const RESERVED_PROFILE_RUNTIME_MESSAGE: &str =
    "the profile.runtime field is reserved for built-in profiles in this PrismTTY version";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse YAML: {0}")]
    Yaml(#[from] serde_norway::Error),
    #[error("unknown profile '{0}'")]
    UnknownProfile(String),
    #[error("cyclic profile inheritance: {0}")]
    CyclicProfileInheritance(String),
    #[error("profile files must include profile.name")]
    MissingProfileName,
    #[error("bundled profile files must include profile.runtime")]
    MissingProfileRuntime,
    #[error("{0}")]
    ReservedProfileRuntime(&'static str),
    #[error("rule '{description}' has invalid style: {message}")]
    InvalidStyle {
        description: String,
        message: String,
    },
    #[error("palette has invalid color: {0}")]
    InvalidPalette(String),
    #[error("rule '{description}' has invalid capture key: {key}")]
    InvalidCaptureKey { description: String, key: String },
}

#[derive(Clone, Debug, Default)]
pub struct PrismConfig {
    pub rules: Vec<RuleSpec>,
    pub enabled_profiles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSpec {
    pub description: String,
    pub regex: String,
    pub style: RuleStyle,
    pub exclusive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CaptureRef {
    Index(usize),
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleStyle {
    Whole(Style),
    Captures(BTreeMap<CaptureRef, Style>),
}

#[derive(Debug, Deserialize)]
struct RulesDoc {
    #[serde(default)]
    profile: Option<ProfileMetaDoc>,
    #[serde(default)]
    palette: BTreeMap<String, String>,
    #[serde(default)]
    rules: Vec<RuleDoc>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProfileMetaDoc {
    pub name: String,
    #[serde(default)]
    pub inherits: Vec<String>,
    #[serde(default)]
    pub detection: Vec<String>,
    #[serde(default)]
    pub(crate) runtime: Option<ProfileRuntimeMeta>,
}

#[derive(Debug, Deserialize)]
struct RuleDoc {
    #[serde(default)]
    description: String,
    regex: String,
    color: serde_norway::Value,
    #[serde(default)]
    exclusive: bool,
}

#[derive(Clone, Debug)]
pub struct LoadedProfileFile {
    pub meta: ProfileMetaDoc,
    pub runtime: Option<ProfileRuntimeMeta>,
    pub rules: Vec<RuleSpec>,
}

impl PrismConfig {
    pub fn from_chromaterm_yaml(input: &str) -> Result<Self, ConfigError> {
        let doc: RulesDoc = serde_norway::from_str(input)?;
        let palette = parse_palette(&doc.palette).map_err(ConfigError::InvalidPalette)?;
        Ok(Self {
            rules: parse_rule_docs(doc.rules, &palette)?,
            enabled_profiles: Vec::new(),
        })
    }

    pub fn from_chromaterm_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let input = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_chromaterm_yaml(&input)
    }

    pub fn from_profiles(
        store: &ProfileStore,
        profile_names: &[&str],
    ) -> Result<Self, ConfigError> {
        let mut rules = Vec::new();
        let mut loaded = BTreeSet::new();

        for profile_name in profile_names {
            store.append_profile_rules(profile_name, &mut loaded, &mut rules)?;
        }

        Ok(Self {
            rules,
            enabled_profiles: loaded.into_iter().collect(),
        })
    }

    pub fn merge(mut self, mut other: Self) -> Self {
        self.rules.append(&mut other.rules);
        for profile in other.enabled_profiles {
            if !self.enabled_profiles.contains(&profile) {
                self.enabled_profiles.push(profile);
            }
        }
        self
    }
}

pub fn load_profile_file(path: impl AsRef<Path>) -> Result<LoadedProfileFile, ConfigError> {
    let path = path.as_ref();
    let input = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse_profile_yaml(&input)
}

pub fn parse_profile_yaml(input: &str) -> Result<LoadedProfileFile, ConfigError> {
    parse_profile_yaml_with_mode(input, ProfileYamlMode::User)
}

pub(crate) fn parse_builtin_profile_yaml(input: &str) -> Result<LoadedProfileFile, ConfigError> {
    parse_profile_yaml_with_mode(input, ProfileYamlMode::Bundled)
}

#[derive(Clone, Copy)]
enum ProfileYamlMode {
    User,
    Bundled,
}

fn parse_profile_yaml_with_mode(
    input: &str,
    mode: ProfileYamlMode,
) -> Result<LoadedProfileFile, ConfigError> {
    let doc: RulesDoc = serde_norway::from_str(input)?;
    let mut meta = doc.profile.ok_or(ConfigError::MissingProfileName)?;
    let runtime = meta.runtime.take();
    match mode {
        ProfileYamlMode::User if runtime.is_some() => {
            return Err(ConfigError::ReservedProfileRuntime(
                RESERVED_PROFILE_RUNTIME_MESSAGE,
            ));
        }
        ProfileYamlMode::Bundled if runtime.is_none() => {
            return Err(ConfigError::MissingProfileRuntime);
        }
        _ => {}
    }
    let palette = parse_palette(&doc.palette).map_err(ConfigError::InvalidPalette)?;
    Ok(LoadedProfileFile {
        meta,
        runtime,
        rules: parse_rule_docs(doc.rules, &palette)?,
    })
}

fn parse_rule_docs(
    rule_docs: Vec<RuleDoc>,
    palette: &BTreeMap<String, crate::style::Rgb>,
) -> Result<Vec<RuleSpec>, ConfigError> {
    rule_docs
        .into_iter()
        .enumerate()
        .map(|(idx, rule)| {
            let description = if rule.description.trim().is_empty() {
                format!("rule {}", idx + 1)
            } else {
                rule.description
            };
            let style = parse_color_doc(&description, rule.color, palette)?;
            Ok(RuleSpec {
                description,
                regex: rule.regex,
                style,
                exclusive: rule.exclusive,
            })
        })
        .collect()
}

fn parse_color_doc(
    description: &str,
    color: serde_norway::Value,
    palette: &BTreeMap<String, crate::style::Rgb>,
) -> Result<RuleStyle, ConfigError> {
    match color {
        serde_norway::Value::String(spec) => {
            Ok(RuleStyle::Whole(parse_style(description, &spec, palette)?))
        }
        serde_norway::Value::Mapping(captures) => {
            let mut parsed = BTreeMap::new();
            for (group, spec) in captures {
                let group = parse_capture_ref(description, group)?;
                let spec = spec.as_str().ok_or_else(|| ConfigError::InvalidStyle {
                    description: description.to_string(),
                    message: "capture color must be a string".to_string(),
                })?;
                parsed.insert(group, parse_style(description, spec, palette)?);
            }
            Ok(RuleStyle::Captures(parsed))
        }
        _ => Err(ConfigError::InvalidStyle {
            description: description.to_string(),
            message: "color must be a string or capture-group mapping".to_string(),
        }),
    }
}

fn parse_capture_ref(
    description: &str,
    value: serde_norway::Value,
) -> Result<CaptureRef, ConfigError> {
    match value {
        serde_norway::Value::Number(number) => {
            let Some(group) = number.as_u64() else {
                return Err(ConfigError::InvalidCaptureKey {
                    description: description.to_string(),
                    key: number.to_string(),
                });
            };
            Ok(CaptureRef::Index(group as usize))
        }
        serde_norway::Value::String(name) if name.bytes().all(|byte| byte.is_ascii_digit()) => name
            .parse::<usize>()
            .map(CaptureRef::Index)
            .map_err(|_| ConfigError::InvalidCaptureKey {
                description: description.to_string(),
                key: name,
            }),
        serde_norway::Value::String(name) if !name.trim().is_empty() => {
            Ok(CaptureRef::Name(name.to_string()))
        }
        other => Err(ConfigError::InvalidCaptureKey {
            description: description.to_string(),
            key: format!("{other:?}"),
        }),
    }
}

fn parse_style(
    description: &str,
    spec: &str,
    palette: &BTreeMap<String, crate::style::Rgb>,
) -> Result<Style, ConfigError> {
    let palette = (!palette.is_empty()).then_some(palette);
    Style::parse_with_palette(spec, palette).map_err(|message| ConfigError::InvalidStyle {
        description: description.to_string(),
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::{RESERVED_PROFILE_RUNTIME_MESSAGE, parse_builtin_profile_yaml, parse_profile_yaml};

    #[test]
    fn user_profile_runtime_is_reserved() {
        let yaml = r#"
profile:
  name: custom-router
  runtime:
    priority: 5
    startup_prompt: cisco_host_marker
    runtime_prompt: cisco_host_marker
    strong_signals: []
rules: []
"#;

        let err = parse_profile_yaml(yaml).expect_err("user profile.runtime must be rejected");

        assert_eq!(err.to_string(), RESERVED_PROFILE_RUNTIME_MESSAGE);
    }

    #[test]
    fn bundled_profile_runtime_rejects_unknown_prompt_matcher() {
        let yaml = r#"
profile:
  name: broken-builtin
  runtime:
    priority: 1
    startup_prompt: mystery_prompt
    runtime_prompt: none
    strong_signals: []
rules: []
"#;

        let err = parse_builtin_profile_yaml(yaml).expect_err("unknown prompt matcher should fail");

        assert!(err.to_string().contains("mystery_prompt"));
    }
}
