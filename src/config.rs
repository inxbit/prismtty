//! Configuration and profile-file parsing.
//!
//! PrismTTY accepts ChromaTerm-style YAML rule files and native profile files
//! that add profile metadata such as inheritance and detection hints.

use crate::profiles::{ProfileRuntimeMeta, ProfileStore};
use crate::style::{Style, parse_palette};
use crate::terminal_text::escape_untrusted;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
        let doc = parse_rules_doc(input)?;
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
        let doc = parse_rules_doc(&input).map_err(|source| ConfigError::YamlFile {
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

/// Deepest collection nesting a config or profile document may use. This is
/// serde_norway's own recursion limit, so checking it up front rejects no
/// document that could load.
const MAX_YAML_NESTING: usize = 128;

/// Most that YAML aliases may add to a config or profile document once they
/// are expanded, counted as the scalar bytes plus [`YAML_NODE_OVERHEAD`] per
/// node of everything they copy. Every alias copies its anchored node again,
/// so without this bound a file within [`MAX_CONFIG_FILE_BYTES`] could expand
/// to many gigabytes.
const MAX_EXPANDED_YAML_BYTES: usize = 4 * MAX_CONFIG_FILE_BYTES as usize;

/// Charge for each YAML node on top of its scalar bytes, so that an alias of a
/// large sequence of tiny scalars is bounded too.
const YAML_NODE_OVERHEAD: usize = 16;

/// Most bytes the tags of a config or profile document may add up to. libyaml
/// resolves each tag against its `%TAG` directive into a fresh string, so
/// without this bound one long directive prefix used by many short tags could
/// expand a file within [`MAX_CONFIG_FILE_BYTES`] to many gigabytes.
const MAX_YAML_TAG_BYTES: usize = 4 * MAX_CONFIG_FILE_BYTES as usize;

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

/// Parses a config or profile YAML document. Every YAML entry point goes
/// through here, so a document within [`MAX_CONFIG_FILE_BYTES`] also stays
/// bounded in parse time and memory.
fn parse_rules_doc(input: &str) -> Result<RulesDoc, serde_norway::Error> {
    check_yaml_limits(input)?;
    serde_norway::from_str(input)
}

/// Rejects a document whose collections nest deeper than [`MAX_YAML_NESTING`],
/// whose aliases add more than [`MAX_EXPANDED_YAML_BYTES`] to it, or whose
/// tags add up to more than [`MAX_YAML_TAG_BYTES`].
///
/// serde_norway applies its recursion limit only after libyaml has produced
/// every event, and libyaml's scanner does work proportional to the flow depth
/// for each token, so a deeply nested file could take minutes to hours to
/// reject. serde_norway also copies an anchored node again for every alias and
/// only limits how many alias jumps a document makes relative to its own size,
/// so a large anchored scalar or sequence referenced many times could exhaust
/// memory before any PrismTTY limit applies. Likewise libyaml spells out each
/// tag in full, and serde_norway keeps every one.
///
/// This walks the same events and stops as soon as any limit is passed,
/// reporting too deep a document in serde_norway's own words. Each alias is
/// charged the expanded size of the node it names, from the scalar lengths
/// libyaml reports, so it does not matter whether serde_norway later reads a
/// scalar as a string or a number. Syntax errors and unknown anchors are left
/// for serde_norway to report.
fn check_yaml_limits(input: &str) -> Result<(), serde_norway::Error> {
    use std::mem::MaybeUninit;
    use unsafe_libyaml_norway::{
        YAML_ALIAS_EVENT, YAML_DOCUMENT_START_EVENT, YAML_MAPPING_END_EVENT,
        YAML_MAPPING_START_EVENT, YAML_SCALAR_EVENT, YAML_SEQUENCE_END_EVENT,
        YAML_SEQUENCE_START_EVENT, YAML_STREAM_END_EVENT, YAML_UTF8_ENCODING, yaml_event_delete,
        yaml_event_t, yaml_parser_delete, yaml_parser_initialize, yaml_parser_parse,
        yaml_parser_set_encoding, yaml_parser_set_input_string, yaml_parser_t,
    };

    let rejected = |message: String| <serde_norway::Error as serde::de::Error>::custom(message);
    let mut storage = MaybeUninit::<yaml_parser_t>::uninit();
    let parser = storage.as_mut_ptr();
    // SAFETY: `parser` points to writable, aligned storage for a
    // yaml_parser_t, which initialize fills in before anything reads it.
    if unsafe { yaml_parser_initialize(parser) }.fail {
        return Err(rejected("failed to initialize the YAML parser".to_string()));
    }
    // SAFETY: the parser is initialized and has no encoding or input yet.
    // `input` outlives the parser, which is deleted below, and `storage` is
    // never moved, which matters because the parser now points to itself.
    unsafe {
        yaml_parser_set_encoding(parser, YAML_UTF8_ENCODING);
        yaml_parser_set_input_string(parser, input.as_ptr(), input.len() as u64);
    }

    let mut rejection = None;
    // Expanded size of the document so far, how much of it aliases added, and
    // how much its tags take.
    let mut expanded = 0usize;
    let mut alias_bytes = 0usize;
    let mut tag_bytes = 0usize;
    // Each anchor name maps to a slot holding the expanded size of the node it
    // names, or `None` while that collection is still open: an alias inside
    // it would repeat it without end. Like serde_norway, the latest definition
    // of a name wins and names do not carry over into the next document.
    let mut anchors: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut anchor_sizes: Vec<Option<usize>> = Vec::new();
    // `expanded` where each open collection started, and its anchor slot.
    let mut open: Vec<(usize, Option<usize>)> = Vec::new();
    loop {
        let mut event = MaybeUninit::<yaml_event_t>::uninit();
        // SAFETY: the parser is initialized with its input set, and `event` is
        // writable storage that parse fills in.
        if unsafe { yaml_parser_parse(parser, event.as_mut_ptr()) }.fail {
            // serde_norway stops at the same error, so it never expands an
            // alias that comes after it.
            break;
        }
        // SAFETY: a successful parse initialized the event.
        let mut event = unsafe { event.assume_init() };
        // SAFETY: the event came from a successful parse and is not deleted
        // until the anchor borrowed from it is no longer used.
        let (anchor, tag_len) = unsafe { yaml_event_anchor_and_tag(&event) };
        tag_bytes = tag_bytes.saturating_add(tag_len);
        if tag_bytes > MAX_YAML_TAG_BYTES {
            let mark = event.start_mark;
            rejection = Some(format!(
                "YAML tags expand the document by more than {MAX_YAML_TAG_BYTES} bytes \
                 at line {} column {}",
                mark.line + 1,
                mark.column + 1
            ));
        }
        // A copy of a node made for an alias copies its tag as well.
        let node_size = YAML_NODE_OVERHEAD.saturating_add(tag_len);
        let mut stop = event.type_ == YAML_STREAM_END_EVENT;
        match event.type_ {
            YAML_DOCUMENT_START_EVENT => {
                anchors.clear();
                anchor_sizes.clear();
            }
            YAML_SCALAR_EVENT => {
                // SAFETY: a scalar event carries the scalar variant.
                let length = unsafe { event.data.scalar.length } as usize;
                let size = node_size.saturating_add(length);
                expanded = expanded.saturating_add(size);
                if let Some(name) = anchor {
                    anchors.insert(name.to_vec(), anchor_sizes.len());
                    anchor_sizes.push(Some(size));
                }
            }
            YAML_SEQUENCE_START_EVENT | YAML_MAPPING_START_EVENT => {
                let slot = anchor.map(|name| {
                    anchors.insert(name.to_vec(), anchor_sizes.len());
                    anchor_sizes.push(None);
                    anchor_sizes.len() - 1
                });
                open.push((expanded, slot));
                expanded = expanded.saturating_add(node_size);
                if open.len() > MAX_YAML_NESTING {
                    let mark = event.start_mark;
                    rejection = Some(format!(
                        "recursion limit exceeded at line {} column {}",
                        mark.line + 1,
                        mark.column + 1
                    ));
                }
            }
            YAML_SEQUENCE_END_EVENT | YAML_MAPPING_END_EVENT => {
                if let Some((start, Some(slot))) = open.pop() {
                    if let Some(size) = anchor_sizes.get_mut(slot) {
                        *size = Some(expanded.saturating_sub(start));
                    }
                }
            }
            YAML_ALIAS_EVENT => {
                let anchored = anchor
                    .and_then(|name| anchors.get(name))
                    .and_then(|&slot| anchor_sizes.get(slot).copied());
                match anchored {
                    Some(anchored) => {
                        // An alias inside the collection it names never ends.
                        let size = anchored.unwrap_or(usize::MAX);
                        expanded = expanded.saturating_add(size);
                        alias_bytes = alias_bytes.saturating_add(size);
                        if alias_bytes > MAX_EXPANDED_YAML_BYTES {
                            let mark = event.start_mark;
                            rejection = Some(format!(
                                "YAML aliases expand the document by more than \
                                 {MAX_EXPANDED_YAML_BYTES} bytes at line {} column {}",
                                mark.line + 1,
                                mark.column + 1
                            ));
                        }
                    }
                    // serde_norway reports an unknown anchor and loads nothing
                    // after it.
                    None => stop = true,
                }
            }
            _ => {}
        }
        // SAFETY: the event came from a successful parse and is deleted once.
        unsafe { yaml_event_delete(&mut event) };
        if stop || rejection.is_some() {
            break;
        }
    }
    // SAFETY: the parser was initialized above and is deleted exactly once.
    unsafe { yaml_parser_delete(parser) };

    match rejection {
        Some(message) => Err(rejected(message)),
        None => Ok(()),
    }
}

/// The anchor that an alias, scalar, sequence or mapping event names, if any,
/// and the length of the tag a scalar, sequence or mapping event carries.
///
/// # Safety
///
/// `event` must come from a successful `yaml_parser_parse` and not have been
/// deleted yet.
unsafe fn yaml_event_anchor_and_tag(
    event: &unsafe_libyaml_norway::yaml_event_t,
) -> (Option<&[u8]>, usize) {
    use std::ffi::CStr;
    use unsafe_libyaml_norway::{
        YAML_ALIAS_EVENT, YAML_MAPPING_START_EVENT, YAML_SCALAR_EVENT, YAML_SEQUENCE_START_EVENT,
    };

    // SAFETY: each arm reads the variant of the event data that its event type
    // carries.
    let (anchor, tag) = unsafe {
        match event.type_ {
            YAML_ALIAS_EVENT => (event.data.alias.anchor, std::ptr::null_mut()),
            YAML_SCALAR_EVENT => (event.data.scalar.anchor, event.data.scalar.tag),
            YAML_SEQUENCE_START_EVENT => (
                event.data.sequence_start.anchor,
                event.data.sequence_start.tag,
            ),
            YAML_MAPPING_START_EVENT => (
                event.data.mapping_start.anchor,
                event.data.mapping_start.tag,
            ),
            _ => return (None, 0),
        }
    };
    // SAFETY: libyaml stores a non-null anchor as a NUL-terminated string that
    // the event owns until it is deleted, which the returned slice's borrow of
    // `event` rules out.
    let anchor = (!anchor.is_null()).then(|| unsafe { CStr::from_ptr(anchor.cast()) }.to_bytes());
    let tag_len = if tag.is_null() {
        0
    } else {
        // SAFETY: libyaml stores a non-null tag as a NUL-terminated string
        // that the event owns until it is deleted.
        unsafe { CStr::from_ptr(tag.cast()) }.to_bytes().len()
    };
    (anchor, tag_len)
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
    let doc = parse_rules_doc(input).map_err(|source| ConfigError::YamlFile {
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
    let doc = parse_rules_doc(input)?;
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

    /// Runs `parse` on a worker thread so that a parser regression on a
    /// pathological document fails the test instead of hanging it.
    fn parse_within_time_limit<T: Send + 'static>(parse: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(parse());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("YAML parsing should finish within the time limit")
    }

    const DEEP_COLOR_HEADER: &str = "rules:\n  - regex: x\n    color: ";

    fn deep_flow_payloads() -> [String; 4] {
        [
            "[".repeat(200_000),
            "{a: ".repeat(100_000),
            "[\n".repeat(200_000),
            format!("{}{}", "[".repeat(16_000), "a,".repeat(400_000)),
        ]
    }

    #[test]
    fn deeply_nested_flow_yaml_is_rejected_quickly() {
        for payload in deep_flow_payloads() {
            let config = format!("{DEEP_COLOR_HEADER}{payload}");
            let message = parse_within_time_limit(move || {
                PrismConfig::from_chromaterm_yaml(&config).map(|_| ())
            })
            .expect_err("deep config nesting must be rejected")
            .to_string();
            assert!(message.contains("recursion limit exceeded"), "{message}");

            let profile = format!("profile:\n  name: deep\n{DEEP_COLOR_HEADER}{payload}");
            let message = parse_within_time_limit(move || parse_profile_yaml(&profile).map(|_| ()))
                .expect_err("deep profile nesting must be rejected")
                .to_string();
            assert!(message.contains("recursion limit exceeded"), "{message}");
        }
    }

    #[test]
    fn deeply_nested_config_and_profile_files_are_rejected_quickly() {
        let file = tempfile::NamedTempFile::new().expect("temp file creates");
        std::fs::write(file.path(), format!("rules: {}\n", "{".repeat(200_000)))
            .expect("deep yaml writes");

        let loaders: [fn(std::path::PathBuf) -> Result<(), super::ConfigError>; 2] = [
            |path| PrismConfig::from_chromaterm_file(path).map(|_| ()),
            |path| load_profile_file(path).map(|_| ()),
        ];
        for load in loaders {
            let path = file.path().to_path_buf();
            let error = parse_within_time_limit(move || load(path))
                .expect_err("deep file nesting must be rejected");
            let message = error.to_string();
            assert!(
                matches!(error, super::ConfigError::YamlFile { .. }),
                "{message}"
            );
            assert!(
                message.contains(&file.path().display().to_string()),
                "{message}"
            );
            assert!(message.contains("recursion limit exceeded"), "{message}");
        }
    }

    #[test]
    fn yaml_nesting_limit_matches_serde_norway_recursion_limit() {
        // The header puts the color value at depth 4, so 125 flow levels end
        // exactly at serde_norway's limit of 128.
        let at_limit = format!(
            "{DEEP_COLOR_HEADER}{}{}\n",
            "[".repeat(125),
            "]".repeat(125)
        );
        let error = PrismConfig::from_chromaterm_yaml(&at_limit)
            .expect_err("a sequence is not a valid color");
        assert!(
            matches!(error, super::ConfigError::InvalidStyle { .. }),
            "{error}"
        );

        let over_limit = format!(
            "{DEEP_COLOR_HEADER}{}{}\n",
            "[".repeat(126),
            "]".repeat(126)
        );
        let serde_error = serde_norway::from_str::<serde_norway::Value>(&over_limit)
            .expect_err("serde_norway rejects nesting past its recursion limit");
        assert_eq!(
            PrismConfig::from_chromaterm_yaml(&over_limit)
                .expect_err("nesting past the limit must be rejected")
                .to_string(),
            format!("failed to parse YAML: {serde_error}")
        );
        assert_eq!(
            serde_error.to_string(),
            "recursion limit exceeded at line 3 column 137"
        );
    }

    /// A 200 KB anchored regex reused by 40 more rules, about 8 MB once its
    /// aliases are expanded.
    fn aliased_regex_config() -> String {
        let mut yaml = format!(
            "rules:\n  - {{regex: &s \"{}\", color: bold}}\n",
            "a".repeat(200_000)
        );
        yaml.push_str(&"  - {regex: *s, color: bold}\n".repeat(40));
        yaml
    }

    /// The same 200 KB anchored scalar as 41 profile detection hints.
    fn aliased_detection_profile() -> String {
        format!(
            "profile:\n  name: p\n  detection: [&s \"{}\"{}]\nrules: []\n",
            "a".repeat(200_000),
            ", *s".repeat(40)
        )
    }

    #[test]
    fn yaml_alias_expansion_is_bounded() {
        // 500 aliases of a 1,000-item sequence inside the free-form color
        // value: tiny scalars, but half a million nodes once expanded.
        let aliased_color = format!(
            "rules:\n  - regex: x\n    color: [&a [{}], {}]\n",
            ["''"; 1_000].join(", "),
            ["*a"; 500].join(", ")
        );
        // serde_norway reads these plain scalars as numbers, but a string
        // field such as regex still copies all of their text for every alias.
        let zeros = "0".repeat(200_000);
        let aliased_numbers = [format!("0x{zeros}1"), format!("1.{zeros}")].map(|scalar| {
            let mut yaml = format!("rules:\n  - {{regex: &s {scalar}, color: bold}}\n");
            yaml.push_str(&"  - {regex: *s, color: bold}\n".repeat(40));
            yaml
        });
        // Aliases inside anchored sequences multiply: six levels of ten make
        // a million scalars from a few hundred bytes.
        let mut nested = String::from(
            "rules:\n  - regex: x\n    color:\n      - &l0 [a, a, a, a, a, a, a, a, a, a]\n",
        );
        for level in 1..6 {
            let items = vec![format!("*l{}", level - 1); 10].join(", ");
            nested.push_str(&format!("      - &l{level} [{items}]\n"));
        }
        // An alias inside the sequence it names repeats it without end.
        let recursive = format!(
            "rules:\n  - regex: x\n    color: &r [\"{}\", *r]\n",
            "a".repeat(200_000)
        );
        let configs = [aliased_regex_config(), aliased_color, nested, recursive];
        for config in configs.into_iter().chain(aliased_numbers) {
            let message = parse_within_time_limit(move || {
                PrismConfig::from_chromaterm_yaml(&config).map(|_| ())
            })
            .expect_err("alias expansion past the budget must be rejected")
            .to_string();
            assert!(message.contains("YAML aliases expand"), "{message}");
        }

        let profile = aliased_detection_profile();
        let message = parse_within_time_limit(move || parse_profile_yaml(&profile).map(|_| ()))
            .expect_err("alias expansion past the budget must be rejected")
            .to_string();
        assert!(message.contains("YAML aliases expand"), "{message}");
    }

    #[test]
    fn yaml_alias_expansion_in_config_and_profile_files_is_bounded() {
        let config = tempfile::NamedTempFile::new().expect("temp file creates");
        std::fs::write(config.path(), aliased_regex_config()).expect("config writes");
        let profile = tempfile::NamedTempFile::new().expect("temp file creates");
        std::fs::write(profile.path(), aliased_detection_profile()).expect("profile writes");

        let loaders: [fn(std::path::PathBuf) -> Result<(), super::ConfigError>; 2] = [
            |path| PrismConfig::from_chromaterm_file(path).map(|_| ()),
            |path| load_profile_file(path).map(|_| ()),
        ];
        for (file, load) in [&config, &profile].into_iter().zip(loaders) {
            let path = file.path().to_path_buf();
            let error = parse_within_time_limit(move || load(path))
                .expect_err("alias expansion past the budget must be rejected");
            let message = error.to_string();
            assert!(
                matches!(error, super::ConfigError::YamlFile { .. }),
                "{message}"
            );
            assert!(
                message.contains(&file.path().display().to_string()),
                "{message}"
            );
            assert!(message.contains("YAML aliases expand"), "{message}");
        }
    }

    #[test]
    fn yaml_tag_expansion_is_bounded() {
        let config = PrismConfig::from_chromaterm_yaml(
            "%TAG !e! tag:example.com,2000:\n---\nrules:\n  - {regex: !e!x y, color: bold}\n",
        )
        .expect("an ordinary tag directive parses");
        assert_eq!(config.rules[0].regex, "y");

        // libyaml spells out the 200 KB directive prefix again for each tag.
        let mut yaml = format!("%TAG !e! tag:{}\n---\nrules:\n", "a".repeat(200_000));
        yaml.push_str(&"  - {regex: !e!x y, color: bold}\n".repeat(40));
        let message =
            parse_within_time_limit(move || PrismConfig::from_chromaterm_yaml(&yaml).map(|_| ()))
                .expect_err("tag expansion past the budget must be rejected")
                .to_string();
        assert!(message.contains("YAML tags expand"), "{message}");
    }

    #[test]
    fn ordinary_yaml_anchors_still_work() {
        let config = PrismConfig::from_chromaterm_yaml(
            r#"
rules:
  - {regex: a, color: &red f#ff0000}
  - {regex: b, color: *red}
  - {regex: '(c)', color: &captures {1: b#000000}}
  - {regex: '(d)', color: *captures}
  - {regex: !custom &tagged e, color: bold}
  - {regex: *tagged, color: bold}
"#,
        )
        .expect("anchored styles parse");
        assert_eq!(config.rules.len(), 6);
        assert_eq!(config.rules[0].style, config.rules[1].style);
        assert_eq!(config.rules[2].style, config.rules[3].style);
        assert_eq!(config.rules[5].regex, "e");

        let profile = parse_profile_yaml(
            "profile:\n  name: p\n  inherits: [&base generic]\n  detection: [*base, Foo]\nrules: []\n",
        )
        .expect("anchored metadata parses");
        assert_eq!(profile.meta.detection, ["generic", "Foo"]);

        // A 1 KB anchored regex reused 1,000 times stays within the budget.
        let mut yaml = format!(
            "rules:\n  - {{regex: &s \"{}\", color: bold}}\n",
            "b".repeat(1_000)
        );
        yaml.push_str(&"  - {regex: *s, color: bold}\n".repeat(1_000));
        let config = PrismConfig::from_chromaterm_yaml(&yaml).expect("modest aliasing parses");
        assert_eq!(config.rules.len(), 1_001);

        // An alias names the latest anchor of its name, as serde_norway
        // resolves it, so these aliases copy one byte, not a megabyte.
        let mut yaml = format!(
            "rules:\n  - {{regex: &r \"{}\", color: bold}}\n  - {{regex: &r b, color: bold}}\n",
            "a".repeat(1_000_000)
        );
        yaml.push_str(&"  - {regex: *r, color: bold}\n".repeat(10));
        let config = PrismConfig::from_chromaterm_yaml(&yaml).expect("redefined anchor parses");
        assert_eq!(config.rules[11].regex, "b");

        let message =
            PrismConfig::from_chromaterm_yaml("rules:\n  - {regex: *missing, color: bold}\n")
                .expect_err("an unknown anchor must be rejected")
                .to_string();
        assert!(message.contains("unknown anchor"), "{message}");
    }

    #[test]
    fn only_what_aliases_add_is_charged_for_expansion() {
        let description = "d".repeat(super::MAX_EXPANDED_YAML_BYTES + 1);
        let config = PrismConfig::from_chromaterm_yaml(&format!(
            "rules:\n  - {{description: {description}, regex: x, color: &c bold}}\n  - {{regex: y, color: *c}}\n"
        ))
        .expect("a large document with a small alias parses");
        assert_eq!(config.rules[0].description, description);
        assert_eq!(config.rules[0].style, config.rules[1].style);
    }
}
