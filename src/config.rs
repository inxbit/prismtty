//! Configuration and profile-file parsing.
//!
//! PrismTTY accepts ChromaTerm-style YAML rule files and native profile files
//! that add profile metadata such as inheritance and detection hints.

use crate::profiles::{ProfileRuntimeMeta, ProfileStore};
use crate::style::{Style, parse_palette};
use crate::terminal_text::escape_untrusted;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Error message returned when user profile files include reserved runtime metadata.
pub const RESERVED_PROFILE_RUNTIME_MESSAGE: &str =
    "the profile.runtime field is reserved for built-in profiles in this PrismTTY version";

/// Errors returned while loading, parsing, or resolving PrismTTY configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A configuration or profile file could not be read.
    #[error("failed to read {path}: {source}")]
    Read {
        /// Path that failed to load.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// YAML decoding failed.
    #[error("failed to parse YAML: {0}")]
    Yaml(#[from] serde_norway::Error),
    /// YAML decoding failed for a specific file.
    #[error("failed to parse YAML in {path}: {source}")]
    YamlFile {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying YAML parser error.
        source: serde_norway::Error,
    },
    /// A requested profile name was not registered.
    #[error("unknown profile '{0}'")]
    UnknownProfile(String),
    /// Profile inheritance loops back to a profile already being resolved.
    #[error("cyclic profile inheritance: {0}")]
    CyclicProfileInheritance(String),
    /// A native profile file omitted `profile.name`.
    #[error("profile files must include profile.name")]
    MissingProfileName,
    /// A bundled built-in profile omitted its private runtime metadata.
    #[error("bundled profile files must include profile.runtime")]
    MissingProfileRuntime,
    /// A user profile attempted to set reserved runtime metadata.
    #[error("{0}")]
    ReservedProfileRuntime(&'static str),
    /// A rule style string or capture style mapping is invalid.
    #[error("rule '{description}' has invalid style: {message}")]
    InvalidStyle {
        /// Human-readable rule description.
        description: String,
        /// Style parser error text.
        message: String,
    },
    /// The palette section contains an invalid color name or value.
    #[error("palette has invalid color: {0}")]
    InvalidPalette(String),
    /// A capture style key was neither a group index nor a group name.
    #[error("rule '{description}' has invalid capture key: {key}")]
    InvalidCaptureKey {
        /// Human-readable rule description.
        description: String,
        /// Invalid capture key as it appeared in YAML.
        key: String,
    },
}

/// Fully resolved highlighting configuration.
#[derive(Clone, Debug, Default)]
pub struct PrismConfig {
    /// Rule list in application order.
    pub rules: Vec<RuleSpec>,
    /// Profiles that contributed rules to this configuration.
    pub enabled_profiles: Vec<String>,
}

/// One highlight rule before PCRE2 compilation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RuleSpec {
    /// Human-readable rule name used in errors and benchmark reports.
    pub description: String,
    /// PCRE2 regular expression matched against visible terminal text.
    pub regex: String,
    /// Style applied to the whole match or selected capture groups.
    pub style: RuleStyle,
    /// Whether this rule prevents later rules from changing the same span.
    pub exclusive: bool,
}

/// Capture group reference used by capture-specific styles.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CaptureRef {
    /// Numeric capture group index, including `0` for the whole match.
    Index(usize),
    /// Named capture group.
    Name(String),
}

/// Style target for a highlight rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RuleStyle {
    /// Apply one style to the whole regex match.
    Whole(Style),
    /// Apply individual styles to capture groups.
    Captures(BTreeMap<CaptureRef, Style>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesDoc {
    #[serde(default)]
    profile: Option<ProfileMetaDoc>,
    #[serde(default)]
    palette: BTreeMap<String, String>,
    #[serde(default)]
    rules: Vec<RuleDoc>,
}

/// Metadata declared in a native profile YAML file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileMetaDoc {
    /// Profile name used on the command line and in inheritance lists.
    pub name: String,
    /// Parent profiles loaded before this profile.
    #[serde(default)]
    pub inherits: Vec<String>,
    /// Startup detection hints used for auto-detection.
    #[serde(default)]
    pub detection: Vec<String>,
    #[serde(default)]
    pub(crate) runtime: Option<ProfileRuntimeMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDoc {
    #[serde(default)]
    description: String,
    regex: String,
    color: serde_norway::Value,
    #[serde(default)]
    exclusive: bool,
}

/// Parsed native profile file, including metadata and rules.
#[derive(Clone, Debug)]
pub struct LoadedProfileFile {
    /// Public profile metadata from the `profile` YAML section.
    pub meta: ProfileMetaDoc,
    /// Runtime metadata for bundled profiles, or `None` for user profiles.
    pub runtime: Option<ProfileRuntimeMeta>,
    /// Parsed highlighting rules from the file.
    pub rules: Vec<RuleSpec>,
}

impl PrismConfig {
    /// Parses a ChromaTerm-style YAML document into highlighting rules.
    pub fn from_chromaterm_yaml(input: &str) -> Result<Self, ConfigError> {
        let doc: RulesDoc = serde_norway::from_str(input)?;
        Self::from_rules_doc(doc)
    }

    fn from_rules_doc(doc: RulesDoc) -> Result<Self, ConfigError> {
        let palette = parse_palette(&doc.palette).map_err(ConfigError::InvalidPalette)?;
        Ok(Self {
            rules: parse_rule_docs(doc.rules, &palette)?,
            enabled_profiles: Vec::new(),
        })
    }

    /// Reads and parses a ChromaTerm-style YAML file.
    pub fn from_chromaterm_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let input = read_config_file(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let doc: RulesDoc =
            serde_norway::from_str(&input).map_err(|source| ConfigError::YamlFile {
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_rules_doc(doc)
    }

    /// Builds a configuration from registered profiles and their inherited rules.
    pub fn from_profiles(
        store: &ProfileStore,
        profile_names: &[&str],
    ) -> Result<Self, ConfigError> {
        let mut rules = Vec::new();
        let mut loaded = BTreeSet::new();

        for profile_name in store.top_level_profile_names(profile_names)? {
            store.append_profile_rules(profile_name, &mut loaded, &mut rules)?;
        }

        Ok(Self {
            rules,
            enabled_profiles: loaded.into_iter().collect(),
        })
    }

    /// Appends another configuration, preserving unique enabled-profile names.
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

/// Largest config / profile file PrismTTY will read. Profiles and ChromaTerm
/// configs are kilobytes; this bounds the read so a giant (or non-regular) file
/// cannot be slurped without limit.
const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// Largest fixture accepted by the profiles test command.
pub(crate) const MAX_PROFILE_TEST_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Reads one bounded regular file without allowing a FIFO open to block.
pub(crate) fn read_bounded_regular_file(
    path: &Path,
    max_bytes: u64,
    kind: &str,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{kind} path is not a regular file: {}", path.display()),
        ));
    }
    let len = metadata.len();
    if len > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{kind} file is too large ({len} bytes; limit {max_bytes})"),
        ));
    }
    let mut input = Vec::with_capacity(len as usize);
    file.take(max_bytes + 1).read_to_end(&mut input)?;
    if input.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{kind} file is too large (more than {max_bytes} bytes)"),
        ));
    }
    Ok(input)
}

/// Reads a config / profile file as UTF-8, rejecting anything larger than
/// [`MAX_CONFIG_FILE_BYTES`].
fn read_config_file(path: &Path) -> std::io::Result<String> {
    let input = read_bounded_regular_file(path, MAX_CONFIG_FILE_BYTES, "config")?;
    String::from_utf8(input)
        .map_err(|source| std::io::Error::new(std::io::ErrorKind::InvalidData, source))
}

/// Loads and validates a native PrismTTY profile YAML file from disk.
pub fn load_profile_file(path: impl AsRef<Path>) -> Result<LoadedProfileFile, ConfigError> {
    let path = path.as_ref();
    let input = read_profile_file_contents(path)?;
    parse_profile_file_contents(path, &input)
}

/// Reads one user profile through the same bounded regular-file path used by
/// [`load_profile_file`]. Directory loaders can retain this exact snapshot,
/// enforce an aggregate byte budget, and only then parse it without reopening
/// a path that may have changed in between.
pub(crate) fn read_profile_file_contents(path: &Path) -> Result<String, ConfigError> {
    read_config_file(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// Parses a previously read user-profile snapshot while preserving the file
/// path in YAML diagnostics.
pub(crate) fn parse_profile_file_contents(
    path: &Path,
    input: &str,
) -> Result<LoadedProfileFile, ConfigError> {
    let doc: RulesDoc = serde_norway::from_str(input).map_err(|source| ConfigError::YamlFile {
        path: path.to_path_buf(),
        source,
    })?;
    match profile_file_from_doc(doc, ProfileYamlMode::User) {
        Err(ConfigError::Yaml(source)) => Err(ConfigError::YamlFile {
            path: path.to_path_buf(),
            source,
        }),
        result => result,
    }
}

/// Parses native PrismTTY profile YAML from a string.
///
/// # Example
///
/// ```
/// use prismtty::config::parse_profile_yaml;
///
/// let profile = parse_profile_yaml(r##"
/// profile:
///   name: custom-router
///   inherits: [generic]
///   detection:
///     - CustomOS
/// rules:
///   - description: management IPv4 addresses
///     regex: '\b192\.0\.2\.\d+\b'
///     color: f#00ffff
/// "##)
/// .expect("profile parses");
///
/// assert_eq!(profile.meta.name, "custom-router");
/// assert_eq!(profile.meta.inherits, vec!["generic".to_string()]);
/// assert_eq!(profile.rules.len(), 1);
/// ```
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
    profile_file_from_doc(doc, mode)
}

fn profile_file_from_doc(
    doc: RulesDoc,
    mode: ProfileYamlMode,
) -> Result<LoadedProfileFile, ConfigError> {
    let mut meta = doc.profile.ok_or(ConfigError::MissingProfileName)?;
    if meta.name.trim().is_empty() {
        return Err(ConfigError::MissingProfileName);
    }
    let (inherits, detection) =
        crate::profiles::normalize_and_validate_profile_metadata(meta.inherits, meta.detection)
            .map_err(|error| {
                ConfigError::Yaml(<serde_norway::Error as serde::de::Error>::custom(
                    error.to_string(),
                ))
            })?;
    meta.inherits = inherits;
    meta.detection = detection;
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
                    description: escape_untrusted(description),
                    message: "capture color must be a string".to_string(),
                })?;
                parsed.insert(group, parse_style(description, spec, palette)?);
            }
            Ok(RuleStyle::Captures(parsed))
        }
        _ => Err(ConfigError::InvalidStyle {
            description: escape_untrusted(description),
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
                    description: escape_untrusted(description),
                    key: escape_untrusted(&number.to_string()),
                });
            };
            Ok(CaptureRef::Index(group as usize))
        }
        serde_norway::Value::String(name) => parse_capture_ref_string(description, name),
        other => Err(ConfigError::InvalidCaptureKey {
            description: escape_untrusted(description),
            key: escape_untrusted(&format!("{other:?}")),
        }),
    }
}

fn parse_capture_ref_string(description: &str, name: String) -> Result<CaptureRef, ConfigError> {
    if name.bytes().all(|byte| byte.is_ascii_digit()) {
        return name.parse::<usize>().map(CaptureRef::Index).map_err(|_| {
            ConfigError::InvalidCaptureKey {
                description: escape_untrusted(description),
                key: escape_untrusted(&name),
            }
        });
    }

    if is_valid_capture_name(&name) {
        Ok(CaptureRef::Name(name))
    } else {
        Err(ConfigError::InvalidCaptureKey {
            description: escape_untrusted(description),
            key: escape_untrusted(&name),
        })
    }
}

fn is_valid_capture_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_style(
    description: &str,
    spec: &str,
    palette: &BTreeMap<String, crate::style::Rgb>,
) -> Result<Style, ConfigError> {
    let palette = (!palette.is_empty()).then_some(palette);
    Style::parse_with_palette(spec, palette).map_err(|message| ConfigError::InvalidStyle {
        description: escape_untrusted(description),
        message: escape_untrusted(&message),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        PrismConfig, RESERVED_PROFILE_RUNTIME_MESSAGE, load_profile_file,
        parse_builtin_profile_yaml, parse_profile_yaml,
    };

    #[test]
    fn read_config_file_rejects_oversized_files() {
        let small = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(small.path(), "rules: []\n").expect("write small");
        assert!(
            super::read_config_file(small.path()).is_ok(),
            "a normal config file should read"
        );

        let big = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(
            big.path(),
            vec![b'#'; super::MAX_CONFIG_FILE_BYTES as usize + 1],
        )
        .expect("write big");
        assert!(
            super::read_config_file(big.path()).is_err(),
            "an oversized config file should be rejected"
        );
    }

    #[test]
    fn read_config_file_rejects_non_regular_files() {
        let dir = tempfile::tempdir().expect("tempdir creates");

        let error =
            super::read_config_file(dir.path()).expect_err("directory paths should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_config_file_rejects_fifo_without_waiting_for_a_writer() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().expect("tempdir creates");
        let fifo = dir.path().join("config.fifo");
        let path = CString::new(fifo.as_os_str().as_bytes()).expect("path has no NUL");
        // SAFETY: path is a valid NUL-terminated string owned for the duration
        // of this call and the mode contains only ordinary permission bits.
        let result = unsafe { nix::libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(
            result,
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );

        let started = Instant::now();
        let error = super::read_config_file(&fifo).expect_err("FIFO must be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

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
    fn profile_metadata_is_normalized_and_deduplicated() {
        let yaml = r#"
profile:
  name: custom-router
  inherits: [" generic ", generic, "base", "base ", ""]
  detection: [" JUNOS ", junos, "jUnOs", " IOS ", ""]
rules: []
"#;

        let loaded = parse_profile_yaml(yaml).expect("bounded profile metadata should parse");

        assert_eq!(loaded.meta.inherits, ["generic", "base"]);
        assert_eq!(loaded.meta.detection, ["JUNOS", "IOS"]);
    }

    #[test]
    fn profile_parser_rejects_metadata_one_over_the_limit_with_safe_diagnostics() {
        let mut yaml =
            String::from("profile:\n  name: \"bad\\u001b]0;title\\u0007\"\n  detection:\n");
        for index in 0..=crate::profiles::MAX_PROFILE_DETECTION_HINTS {
            yaml.push_str(&format!("    - hint-{index}\n"));
        }
        yaml.push_str("rules: []\n");

        let message = parse_profile_yaml(&yaml)
            .expect_err("one unique hint above the limit must fail during parsing")
            .to_string();

        assert_eq!(
            message,
            format!(
                "failed to parse YAML: profile detection hint count is {}; limit is {}",
                crate::profiles::MAX_PROFILE_DETECTION_HINTS + 1,
                crate::profiles::MAX_PROFILE_DETECTION_HINTS
            )
        );
        assert!(!message.contains('\u{1b}'), "{message:?}");
        assert!(!message.contains('\u{7}'), "{message:?}");
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

    #[test]
    fn chromaterm_file_yaml_errors_include_path() {
        let file = tempfile::NamedTempFile::new().expect("temp file creates");
        std::fs::write(file.path(), "rules: [").expect("invalid yaml writes");

        let err = PrismConfig::from_chromaterm_file(file.path())
            .expect_err("invalid file YAML should fail");
        let message = err.to_string();

        assert!(message.contains(&file.path().display().to_string()));
        assert!(message.contains("failed to parse YAML in"));
    }

    #[test]
    fn profile_file_yaml_errors_include_path() {
        let file = tempfile::NamedTempFile::new().expect("temp file creates");
        std::fs::write(file.path(), "profile: [").expect("invalid yaml writes");

        let err = load_profile_file(file.path()).expect_err("invalid profile YAML should fail");
        let message = err.to_string();

        assert!(message.contains(&file.path().display().to_string()));
        assert!(message.contains("failed to parse YAML in"));
    }

    #[test]
    fn capture_names_must_match_pcre2_identifier_shape() {
        let yaml = r#"
rules:
  - description: named capture
    regex: '(?P<name>\w+)'
    color:
      _valid_name_1: f#ffffff
      bad-name: f#ff0000
"#;

        let err = PrismConfig::from_chromaterm_yaml(yaml)
            .expect_err("invalid capture name should fail during config parsing");

        assert_eq!(
            err.to_string(),
            "rule 'named capture' has invalid capture key: bad-name"
        );
    }

    #[test]
    fn config_diagnostics_escape_untrusted_terminal_metadata() {
        let yaml = r#"
rules:
  - description: "bad\u001b]0;title\u0007"
    regex: '(?P<name>\w+)'
    color:
      "bad\u001b[31m": f#ffffff
"#;

        let message = PrismConfig::from_chromaterm_yaml(yaml)
            .expect_err("invalid capture name should fail")
            .to_string();

        assert!(!message.contains('\u{1b}'), "{message:?}");
        assert!(!message.contains('\u{7}'), "{message:?}");
        assert!(message.contains("\\x1b"), "{message:?}");
        assert!(message.contains("\\x07"), "{message:?}");
    }
}
