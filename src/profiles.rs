//! Built-in profile store and startup detection helpers.
//!
//! Profiles group highlighting rules with inheritance, startup detection hints,
//! and private runtime metadata used by interactive profile switching.

use crate::config::{ConfigError, RuleSpec, parse_builtin_profile_yaml};
use serde::{Deserialize, Deserializer, de};
use std::collections::{BTreeMap, BTreeSet};

const BUNDLED_PROFILES: &[(&str, &str)] = &[
    ("generic.yml", include_str!("profiles/builtin/generic.yml")),
    ("juniper.yml", include_str!("profiles/builtin/juniper.yml")),
    (
        "fortinet.yml",
        include_str!("profiles/builtin/fortinet.yml"),
    ),
    ("arubacx.yml", include_str!("profiles/builtin/arubacx.yml")),
    ("arista.yml", include_str!("profiles/builtin/arista.yml")),
    ("cisco.yml", include_str!("profiles/builtin/cisco.yml")),
    (
        "palo-alto.yml",
        include_str!("profiles/builtin/palo-alto.yml"),
    ),
    ("versa.yml", include_str!("profiles/builtin/versa.yml")),
    (
        "linux-unix.yml",
        include_str!("profiles/builtin/linux-unix.yml"),
    ),
];

/// Default runtime priority assigned to user-loaded profiles.
pub const USER_PROFILE_RUNTIME_PRIORITY: u16 = 100;

pub(crate) fn is_generic_profile_set(profiles: &[String]) -> bool {
    profiles.len() == 1 && profiles.first().is_some_and(|profile| profile == "generic")
}

/// Registered profile with resolved rule and detection metadata.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Profile name used on the command line and in configuration.
    pub name: String,
    /// Parent profiles loaded before this profile.
    pub inherits: Vec<String>,
    /// Case-insensitive startup detection hints.
    pub detection: Vec<String>,
    /// Runtime metadata used for interactive profile transitions.
    pub runtime: ProfileRuntimeMeta,
    /// Highlight rules owned by this profile.
    pub rules: Vec<RuleSpec>,
}

/// Runtime-only profile metadata used by bundled profiles.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileRuntimeMeta {
    /// Lower values are considered before higher values during profile ordering.
    pub priority: u16,
    /// Whether this profile can represent the local starting shell context.
    #[serde(default)]
    pub local_baseline: bool,
    /// Signals strong enough to switch profiles from a banner or command output.
    #[serde(default)]
    pub strong_signals: Vec<StrongSignal>,
    /// Signals that block startup prompt detection for this profile.
    #[serde(default)]
    pub negative_signals: Vec<StrongSignal>,
    /// Prompt matcher used at session startup.
    pub startup_prompt: PromptMatcherKind,
    /// Prompt matcher used after the session has enough remote-context evidence.
    pub runtime_prompt: PromptMatcherKind,
    /// Evidence threshold for prompt-only transitions.
    #[serde(default)]
    pub prompt_confidence: PromptConfidence,
}

impl Default for ProfileRuntimeMeta {
    fn default() -> Self {
        Self {
            priority: USER_PROFILE_RUNTIME_PRIORITY,
            local_baseline: false,
            strong_signals: Vec::new(),
            negative_signals: Vec::new(),
            startup_prompt: PromptMatcherKind::None,
            runtime_prompt: PromptMatcherKind::None,
            prompt_confidence: PromptConfidence::default(),
        }
    }
}

/// Strong profile detection signal matched against visible text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StrongSignal {
    /// Match when the sample contains one value.
    Contains {
        /// Case-insensitive substring to search for.
        value: String,
    },
    /// Match when the sample contains any listed value.
    ContainsAny {
        /// Case-insensitive substrings accepted as matches.
        values: Vec<String>,
    },
    /// Match when a line has the given prefix and contains any listed value.
    LinePrefixAndAny {
        /// Required line prefix.
        prefix: String,
        /// Case-insensitive substrings accepted after the prefix matches.
        values: Vec<String>,
    },
}

impl<'de> Deserialize<'de> for StrongSignal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let doc = StrongSignalDoc::deserialize(deserializer)?;
        match doc.kind {
            StrongSignalKind::Contains => {
                let value = doc.value.ok_or_else(|| de::Error::missing_field("value"))?;
                Ok(StrongSignal::Contains { value })
            }
            StrongSignalKind::ContainsAny => {
                let values = doc
                    .values
                    .ok_or_else(|| de::Error::missing_field("values"))?;
                Ok(StrongSignal::ContainsAny { values })
            }
            StrongSignalKind::LinePrefixAndAny => {
                let prefix = doc
                    .prefix
                    .ok_or_else(|| de::Error::missing_field("prefix"))?;
                let values = doc
                    .values
                    .ok_or_else(|| de::Error::missing_field("values"))?;
                Ok(StrongSignal::LinePrefixAndAny { prefix, values })
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrongSignalDoc {
    #[serde(rename = "type")]
    kind: StrongSignalKind,
    value: Option<String>,
    values: Option<Vec<String>>,
    prefix: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StrongSignalKind {
    Contains,
    ContainsAny,
    LinePrefixAndAny,
}

impl StrongSignal {
    fn matches(&self, text: &str) -> bool {
        match self {
            StrongSignal::Contains { value } => contains_case_insensitive(text, value),
            StrongSignal::ContainsAny { values } => values
                .iter()
                .any(|value| contains_case_insensitive(text, value)),
            StrongSignal::LinePrefixAndAny { prefix, values } => text.lines().any(|line| {
                let trimmed = line.trim();
                starts_with_case_insensitive(trimmed, prefix)
                    && values
                        .iter()
                        .any(|value| contains_case_insensitive(trimmed, value))
            }),
        }
    }
}

/// Built-in prompt matcher used by runtime profile detection.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptMatcherKind {
    /// No prompt matcher.
    None,
    /// Junos-style `user@host>` prompt.
    JunosUserAtHost,
    /// Cisco-style host prompt ending in `>` or `#`.
    CiscoHostMarker,
    /// Arista-style host prompt ending in `>` or `#`.
    AristaHostMarker,
    /// Fortinet-style host prompt ending in `#`.
    FortinetHostHash,
    /// Unix-style `user@host` prompt with an optional path.
    UnixUserAtHostPath,
    /// PAN-OS-style `user@host>` prompt.
    PaloAltoUserAtHost,
    /// Versa-style `user@host>` prompt.
    VersaUserAtHost,
}

/// Required prompt evidence before a profile transition is accepted.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptConfidence {
    /// Require repeated prompt evidence.
    #[default]
    Repeated,
    /// Accept a single prompt after a typed remote-session hint.
    SingleAfterRemoteHint,
    /// Accept a single prompt from a local baseline or after a remote-session hint.
    SingleFromBaselineOrRemoteHint,
}

/// Collection of built-in and user-registered profiles.
#[derive(Clone, Debug, Default)]
pub struct ProfileStore {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileStore {
    /// Loads the bundled built-in profiles.
    pub fn builtin() -> Self {
        let mut store = Self::default();
        for (_file_name, contents) in BUNDLED_PROFILES {
            let loaded = parse_builtin_profile_yaml(contents)
                .expect("bundled built-in profile YAML is valid");
            let runtime = loaded
                .runtime
                .expect("bundled built-in profile has runtime metadata");
            let profile = Profile {
                name: loaded.meta.name,
                inherits: loaded.meta.inherits,
                detection: loaded.meta.detection,
                runtime,
                rules: loaded.rules,
            };
            store.profiles.insert(profile.name.clone(), profile);
        }
        store
    }

    #[cfg(test)]
    pub(crate) fn bundled_profile_file_names() -> Vec<&'static str> {
        BUNDLED_PROFILES
            .iter()
            .map(|(file_name, _contents)| *file_name)
            .collect()
    }

    /// Returns profile names in deterministic sorted order.
    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(String::as_str).collect()
    }

    /// Looks up a registered profile by name.
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Registers a user profile, returning `true` if it shadowed an existing
    /// (built-in or earlier user) profile of the same name.
    pub fn insert_profile(
        &mut self,
        name: String,
        inherits: Vec<String>,
        detection: Vec<String>,
        rules: Vec<RuleSpec>,
    ) -> bool {
        self.profiles
            .insert(
                name.clone(),
                Profile {
                    name,
                    inherits,
                    detection,
                    runtime: ProfileRuntimeMeta::default(),
                    rules,
                },
            )
            .is_some()
    }

    /// Detects likely profiles from a startup sample, always including `generic`.
    pub fn detect_profiles(&self, sample: &str) -> Vec<String> {
        let lower = sample.to_ascii_lowercase();
        self.detect_profiles_with_lowercase(sample, &lower)
    }

    pub(crate) fn detect_profiles_with_lowercase(&self, sample: &str, lower: &str) -> Vec<String> {
        let mut detected: Vec<String> = self
            .profiles
            .iter()
            .filter(|(name, profile)| {
                name.as_str() != "generic" && profile.matches_startup_detection(sample, lower)
            })
            .map(|(name, _profile)| name.clone())
            .collect();
        self.sort_profiles_by_priority(&mut detected);

        let mut with_generic = vec!["generic".to_string()];
        with_generic.extend(detected);
        with_generic
    }

    /// Appends inherited rules for a profile while detecting inheritance cycles.
    pub fn append_profile_rules(
        &self,
        profile_name: &str,
        loaded: &mut BTreeSet<String>,
        rules: &mut Vec<RuleSpec>,
    ) -> Result<(), ConfigError> {
        let mut resolving = Vec::new();
        self.append_profile_rules_inner(profile_name, loaded, &mut resolving, rules)
    }

    pub(crate) fn top_level_profile_names<'a>(
        &self,
        profile_names: &'a [&str],
    ) -> Result<Vec<&'a str>, ConfigError> {
        let mut top_level = Vec::new();
        for candidate in profile_names {
            let mut inherited_by_selected = false;
            for other in profile_names.iter().filter(|other| *other != candidate) {
                if !self.profile_inherits_profile(other, candidate)? {
                    continue;
                }
                // Mutual inheritance would drop BOTH candidates here, silently
                // yielding a config with zero rules; surface the cycle instead.
                if self.profile_inherits_profile(candidate, other)? {
                    return Err(ConfigError::CyclicProfileInheritance(
                        [*candidate, *other, *candidate].join(" -> "),
                    ));
                }
                inherited_by_selected = true;
                break;
            }
            if !inherited_by_selected {
                top_level.push(*candidate);
            }
        }
        Ok(top_level)
    }

    fn profile_inherits_profile(
        &self,
        profile_name: &str,
        ancestor: &str,
    ) -> Result<bool, ConfigError> {
        let mut seen = BTreeSet::new();
        self.profile_inherits_profile_inner(profile_name, ancestor, &mut seen)
    }

    fn profile_inherits_profile_inner(
        &self,
        profile_name: &str,
        ancestor: &str,
        seen: &mut BTreeSet<String>,
    ) -> Result<bool, ConfigError> {
        if !seen.insert(profile_name.to_string()) {
            return Ok(false);
        }
        let profile = self
            .profiles
            .get(profile_name)
            .ok_or_else(|| ConfigError::UnknownProfile(profile_name.to_string()))?;
        for parent in &profile.inherits {
            if parent == ancestor || self.profile_inherits_profile_inner(parent, ancestor, seen)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn append_profile_rules_inner(
        &self,
        profile_name: &str,
        loaded: &mut BTreeSet<String>,
        resolving: &mut Vec<String>,
        rules: &mut Vec<RuleSpec>,
    ) -> Result<(), ConfigError> {
        if loaded.contains(profile_name) {
            return Ok(());
        }
        if let Some(cycle_start) = resolving
            .iter()
            .position(|resolving_name| resolving_name.as_str() == profile_name)
        {
            let mut cycle = resolving[cycle_start..].to_vec();
            cycle.push(profile_name.to_string());
            return Err(ConfigError::CyclicProfileInheritance(cycle.join(" -> ")));
        }

        let profile = self
            .profiles
            .get(profile_name)
            .ok_or_else(|| ConfigError::UnknownProfile(profile_name.to_string()))?;

        resolving.push(profile_name.to_string());
        let rule_start = rules.len();
        rules.extend(profile.rules.clone());
        for parent in &profile.inherits {
            if let Err(error) = self.append_profile_rules_inner(parent, loaded, resolving, rules) {
                rules.truncate(rule_start);
                resolving.pop();
                return Err(error);
            }
        }
        resolving.pop();
        loaded.insert(profile.name.clone());
        Ok(())
    }

    pub(crate) fn active_specific_profile<'a>(&self, profiles: &'a [String]) -> Option<&'a str> {
        self.ordered_specific_profiles(profiles)
            .into_iter()
            .next()
            .map(String::as_str)
    }

    pub(crate) fn strong_transition_profile(
        &self,
        detected: &[String],
        text: &str,
        active_profile: Option<&str>,
    ) -> Option<String> {
        self.ordered_specific_profiles(detected)
            .into_iter()
            .filter(|profile| Some(profile.as_str()) != active_profile)
            .find(|profile| {
                self.profiles
                    .get(profile.as_str())
                    .is_some_and(|profile| profile.matches_strong_signal(text))
            })
            .cloned()
    }

    pub(crate) fn prompt_transition_profile(
        &self,
        detected: &[String],
        text: &str,
        active_profile: Option<&str>,
    ) -> Option<String> {
        self.ordered_specific_profiles(detected)
            .into_iter()
            .filter(|profile| Some(profile.as_str()) != active_profile)
            .find(|profile| {
                self.profiles
                    .get(profile.as_str())
                    .is_some_and(|profile| profile.matches_runtime_prompt(text))
            })
            .cloned()
    }

    pub(crate) fn prompt_switches_on_first_detection(
        &self,
        profile_name: &str,
        remote_candidate: bool,
        at_baseline: bool,
    ) -> bool {
        self.profiles.get(profile_name).is_some_and(|profile| {
            match profile.runtime.prompt_confidence {
                PromptConfidence::Repeated => false,
                PromptConfidence::SingleAfterRemoteHint => remote_candidate,
                PromptConfidence::SingleFromBaselineOrRemoteHint => remote_candidate || at_baseline,
            }
        })
    }

    pub(crate) fn profiles_are_local_baseline(&self, profiles: &[String]) -> bool {
        profiles
            .iter()
            .all(|profile| self.is_local_baseline_profile(profile))
    }

    pub(crate) fn is_local_baseline_profile(&self, profile_name: &str) -> bool {
        self.profiles
            .get(profile_name)
            .is_some_and(|profile| profile.runtime.local_baseline)
    }

    fn ordered_specific_profiles<'a>(&self, profiles: &'a [String]) -> Vec<&'a String> {
        let mut ordered: Vec<&String> = profiles
            .iter()
            .filter(|profile| profile.as_str() != "generic")
            .collect();
        ordered.sort_by(|left, right| {
            self.profile_priority(left)
                .cmp(&self.profile_priority(right))
                .then_with(|| left.cmp(right))
        });
        ordered
    }

    fn sort_profiles_by_priority(&self, profiles: &mut [String]) {
        profiles.sort_by(|left, right| {
            self.profile_priority(left)
                .cmp(&self.profile_priority(right))
                .then_with(|| left.cmp(right))
        });
    }

    fn profile_priority(&self, profile_name: &str) -> u16 {
        self.profiles
            .get(profile_name)
            .map(|profile| profile.runtime.priority)
            .unwrap_or(USER_PROFILE_RUNTIME_PRIORITY)
    }
}

impl Profile {
    fn matches_startup_detection(&self, sample: &str, lower: &str) -> bool {
        self.detection
            .iter()
            .any(|hint| lower.contains(&hint.to_ascii_lowercase()))
            || self.matches_strong_signal(sample)
            || self.matches_startup_prompt(sample)
    }

    fn matches_strong_signal(&self, text: &str) -> bool {
        self.runtime
            .strong_signals
            .iter()
            .any(|signal| signal.matches(text))
    }

    fn matches_startup_prompt(&self, sample: &str) -> bool {
        self.runtime.startup_prompt.matches_startup(sample) && !self.matches_negative_signal(sample)
    }

    fn matches_negative_signal(&self, text: &str) -> bool {
        self.runtime
            .negative_signals
            .iter()
            .any(|signal| signal.matches(text))
    }

    fn matches_runtime_prompt(&self, text: &str) -> bool {
        self.runtime.runtime_prompt.matches_runtime(text)
    }
}

impl PromptMatcherKind {
    fn matches_startup(self, sample: &str) -> bool {
        match self {
            PromptMatcherKind::None => false,
            PromptMatcherKind::JunosUserAtHost => looks_like_juniper_prompt(sample),
            PromptMatcherKind::CiscoHostMarker => looks_like_cisco_prompt(sample),
            PromptMatcherKind::AristaHostMarker => looks_like_arista_prompt(sample),
            PromptMatcherKind::FortinetHostHash => looks_like_fortinet_prompt(sample),
            PromptMatcherKind::UnixUserAtHostPath => looks_like_unix_prompt(sample),
            PromptMatcherKind::PaloAltoUserAtHost => looks_like_palo_alto_prompt(sample),
            PromptMatcherKind::VersaUserAtHost => looks_like_versa_prompt(sample),
        }
    }

    fn matches_runtime(self, text: &str) -> bool {
        match self {
            PromptMatcherKind::None => false,
            PromptMatcherKind::JunosUserAtHost => text.lines().any(looks_like_juniper_prompt_line),
            PromptMatcherKind::CiscoHostMarker => text.lines().any(looks_like_cisco_prompt_line),
            PromptMatcherKind::AristaHostMarker => text.lines().any(looks_like_arista_prompt_line),
            PromptMatcherKind::FortinetHostHash => {
                text.lines().any(looks_like_fortinet_prompt_line)
            }
            PromptMatcherKind::UnixUserAtHostPath => text.lines().any(looks_like_unix_prompt_line),
            PromptMatcherKind::PaloAltoUserAtHost => {
                text.lines().any(looks_like_palo_alto_prompt_line)
            }
            PromptMatcherKind::VersaUserAtHost => text.lines().any(looks_like_versa_prompt_line),
        }
    }
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_len = needle.len();
    if needle_len > haystack.len() {
        return false;
    }
    haystack.char_indices().any(|(start, _)| {
        let end = start + needle_len;
        end <= haystack.len()
            && haystack.is_char_boundary(end)
            && haystack.as_bytes()[start..end].eq_ignore_ascii_case(needle.as_bytes())
    })
}

fn starts_with_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .get(..needle.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle.as_bytes()))
}

fn prompt_token(line: &str) -> &str {
    line.split_whitespace()
        .next()
        .unwrap_or(line)
        .trim_matches(|ch: char| ch.is_ascii_control())
}

fn looks_like_cisco_prompt(sample: &str) -> bool {
    sample.lines().any(looks_like_cisco_prompt_line)
}

fn looks_like_cisco_prompt_line(line: &str) -> bool {
    let prompt = prompt_token(line);
    let marker = prompt.find(['>', '#']);
    let Some(marker) = marker else {
        return false;
    };
    if marker == 0 {
        return false;
    }
    let body = &prompt[..marker];
    !body.contains('@')
        && !body.contains(':')
        && body.bytes().all(is_cisco_prompt_byte)
        && prompt[marker + 1..]
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b' ' | b'\t'))
}

fn looks_like_arista_prompt(sample: &str) -> bool {
    sample.lines().any(looks_like_arista_prompt_line)
}

fn looks_like_arista_prompt_line(line: &str) -> bool {
    let prompt = prompt_token(line);
    let Some(marker) = prompt.rfind(['>', '#']) else {
        return false;
    };
    if marker == 0 {
        return false;
    }
    let host = &prompt[..marker];
    let command_tail = &prompt[marker + 1..];
    let arista_like_host = host.to_ascii_lowercase();
    let has_arista_host_hint = arista_like_host.contains("arista")
        || arista_like_host.contains("ceos")
        || arista_like_host.starts_with("eos")
        || arista_like_host.starts_with("leaf")
        || arista_like_host.starts_with("spine");

    has_arista_host_hint
        && !host.contains('@')
        && !host.contains(':')
        && host.bytes().all(is_prompt_name_byte)
        && command_tail
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b' ' | b'\t'))
}

fn looks_like_fortinet_prompt(sample: &str) -> bool {
    sample.lines().any(looks_like_fortinet_prompt_line)
}

fn looks_like_fortinet_prompt_line(line: &str) -> bool {
    let trimmed = line.trim_matches(|ch: char| ch.is_ascii_control());
    let Some((host, rest)) = trimmed.split_once(" #") else {
        return false;
    };
    let host = fortinet_prompt_host(host.trim_end());
    !host.is_empty()
        && !host.contains('@')
        && !host.contains(':')
        && host.bytes().all(is_prompt_name_byte)
        && rest
            .bytes()
            .next()
            .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_alphabetic())
}

fn fortinet_prompt_host(host: &str) -> &str {
    if !host.ends_with(')') {
        return host;
    }
    host.rsplit_once(" (")
        .map(|(base, _context)| base.trim_end())
        .unwrap_or(host)
}

fn looks_like_unix_prompt(sample: &str) -> bool {
    sample.lines().any(looks_like_unix_prompt_line)
}

fn looks_like_unix_prompt_line(line: &str) -> bool {
    let prompt = line
        .trim_matches(|ch: char| ch.is_ascii_control())
        .trim_end();
    let Some(marker) = prompt.rfind(['#', '$', '%']) else {
        return false;
    };
    if marker == 0 || marker + 1 != prompt.len() {
        return false;
    }

    let marker_byte = prompt.as_bytes()[marker];
    let body = prompt[..marker].trim_end();
    let Some(at) = body.rfind('@') else {
        return false;
    };
    let user = body[..at].split_whitespace().last().unwrap_or_default();
    let rest = &body[at + 1..];
    let host_end = rest
        .find(|ch: char| ch == ':' || ch.is_ascii_whitespace())
        .unwrap_or(rest.len());
    let host = &rest[..host_end];
    let separator = rest[host_end..].chars().next();

    !user.is_empty()
        && !host.is_empty()
        && user.bytes().all(is_prompt_name_byte)
        && host.bytes().all(is_prompt_name_byte)
        && (marker_byte != b'%'
            || matches!(separator, Some(':'))
            || rest[host_end..].chars().any(|ch| ch.is_ascii_whitespace()))
}

fn looks_like_juniper_prompt(sample: &str) -> bool {
    sample.lines().any(looks_like_juniper_prompt_line)
}

fn looks_like_juniper_prompt_line(line: &str) -> bool {
    let prompt = prompt_token(line);
    let Some(marker) = prompt.rfind(['>', '%']) else {
        return false;
    };
    if marker + 1 != prompt.len() {
        return false;
    }
    let body = &prompt[..marker];
    let Some((user, host)) = body.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !host.is_empty()
        && !host.contains(':')
        && user.bytes().all(is_prompt_name_byte)
        && host.bytes().all(is_prompt_name_byte)
}

fn looks_like_palo_alto_prompt(sample: &str) -> bool {
    sample.lines().any(looks_like_palo_alto_prompt_line)
}

fn looks_like_palo_alto_prompt_line(line: &str) -> bool {
    let prompt = prompt_token(line);
    let Some(marker) = prompt.rfind(['>', '#']) else {
        return false;
    };
    if marker == 0 {
        return false;
    }
    let body = &prompt[..marker];
    let Some((user, host)) = body.split_once('@') else {
        return false;
    };
    let host_lower = host.to_ascii_lowercase();
    let has_pan_host_hint = host_lower.starts_with("pa-")
        || host_lower.starts_with("fw-")
        || host_lower.contains("pan")
        || host_lower.contains("palo");

    has_pan_host_hint
        && !user.is_empty()
        && !host.is_empty()
        && !host.contains(':')
        && user.bytes().all(is_prompt_name_byte)
        && host.bytes().all(is_prompt_name_byte)
}

fn looks_like_versa_prompt(sample: &str) -> bool {
    sample.lines().any(looks_like_versa_prompt_line)
}

fn looks_like_versa_prompt_line(line: &str) -> bool {
    let prompt = prompt_token(line);
    let Some(marker) = prompt.rfind(['>', '#']) else {
        return false;
    };
    if marker == 0 {
        return false;
    }
    let body = &prompt[..marker];
    let Some((user, host)) = body.split_once('@') else {
        return false;
    };
    let host_lower = host.to_ascii_lowercase();
    let has_versa_hint = host_lower.contains("versa") || host_lower.contains("voss");
    has_versa_hint
        && !user.is_empty()
        && !host.is_empty()
        && !host.contains(':')
        && user.bytes().all(is_prompt_name_byte)
        && host.bytes().all(is_prompt_name_byte)
}

fn is_prompt_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

fn is_cisco_prompt_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'(' | b')')
}

#[cfg(test)]
mod tests {
    use super::{ProfileStore, PromptMatcherKind};
    use crate::config::{PrismConfig, RuleSpec, RuleStyle};
    use crate::highlight::Highlighter;
    use crate::style::{Rgb, Style};
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    #[test]
    fn insert_profile_reports_shadowing_a_builtin() {
        let mut store = ProfileStore::builtin();
        assert!(
            store.insert_profile("cisco".to_string(), Vec::new(), Vec::new(), Vec::new()),
            "overriding a built-in profile name must report a shadow"
        );
        assert!(
            !store.insert_profile("user-only".to_string(), Vec::new(), Vec::new(), Vec::new()),
            "a fresh profile name must not report a shadow"
        );
    }

    #[test]
    fn every_builtin_yaml_file_is_registered() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let builtin_dir = Path::new(manifest_dir).join("src/profiles/builtin");
        let registered: BTreeSet<_> = ProfileStore::bundled_profile_file_names()
            .into_iter()
            .collect();

        for entry in fs::read_dir(&builtin_dir).expect("builtin profile directory exists") {
            let entry = entry.expect("builtin profile directory entry is readable");
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("yml") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("builtin profile file has UTF-8 name");
            assert!(
                registered.contains(file_name),
                "bundled profile file {file_name} is missing from BUNDLED_PROFILES"
            );
        }
    }

    #[test]
    fn builtin_profiles_have_valid_runtime_metadata_and_highlighter_rules() {
        let store = ProfileStore::builtin();

        for profile in store.profiles.values() {
            assert!(
                profile.runtime.priority < 100,
                "built-in profile {} should use a bundled runtime priority below user profiles",
                profile.name
            );
            let config =
                crate::config::PrismConfig::from_profiles(&store, &[profile.name.as_str()])
                    .expect("built-in profile config should resolve");
            Highlighter::from_config(config).expect("built-in regexes and styles should compile");
        }
    }

    #[test]
    fn prompt_matchers_keep_vendor_specific_behavior() {
        assert!(PromptMatcherKind::CiscoHostMarker.matches_startup("Router(config-if)#"));
        assert!(!PromptMatcherKind::AristaHostMarker.matches_startup("Router(config-if)#"));
        assert!(PromptMatcherKind::AristaHostMarker.matches_startup("leaf01#"));
        assert!(!PromptMatcherKind::PaloAltoUserAtHost.matches_startup("admin@router>"));
        assert!(PromptMatcherKind::PaloAltoUserAtHost.matches_startup("admin@pa-edge>"));
        assert!(PromptMatcherKind::UnixUserAtHostPath.matches_startup("cdassy@MacBook-Pro ~ %"));
        assert!(!PromptMatcherKind::UnixUserAtHostPath.matches_startup("admin@mx480%"));
    }

    #[test]
    fn child_profile_rules_take_precedence_over_inherited_exclusive_rules() {
        let mut store = ProfileStore::default();
        store.insert_profile(
            "base".to_string(),
            Vec::new(),
            Vec::new(),
            vec![RuleSpec {
                description: "base catch-all".to_string(),
                regex: r"\b\w+\b".to_string(),
                style: RuleStyle::Whole(Style::parse("f#ff0000").expect("base style parses")),
                exclusive: true,
            }],
        );
        store.insert_profile(
            "router".to_string(),
            vec!["base".to_string()],
            Vec::new(),
            vec![RuleSpec {
                description: "router keyword".to_string(),
                regex: r"\brouter\b".to_string(),
                style: RuleStyle::Whole(Style::parse("f#0000ff").expect("child style parses")),
                exclusive: true,
            }],
        );

        let config = PrismConfig::from_profiles(&store, &["base", "router"])
            .expect("profile inheritance resolves");
        let highlighter = Highlighter::from_config(config).expect("highlighter builds");
        let spans = highlighter.style_spans(b"router");

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.foreground, Some(Rgb { r: 0, g: 0, b: 255 }));
    }

    #[test]
    fn prompt_matchers_do_not_embed_cross_vendor_negative_signals() {
        assert!(
            PromptMatcherKind::JunosUserAtHost
                .matches_startup("admin@mx480>\nPAN-OS 11.1\nVersa Director\n",)
        );
        assert!(
            PromptMatcherKind::CiscoHostMarker
                .matches_startup("CoreSW#\nArubaOS-CX Version 10.13\nhpe-restd\n",)
        );
    }

    #[test]
    fn bundled_negative_signals_block_only_startup_prompt_detection() {
        let store = ProfileStore::builtin();

        let juniper_from_prompt = store.detect_profiles("admin@mx480>\nPAN-OS 11.1\n");
        assert!(
            !juniper_from_prompt.contains(&"juniper".to_string()),
            "Juniper prompt startup detection should be blocked by bundled PAN-OS negative signal"
        );

        let cisco_from_prompt = store.detect_profiles("CoreSW#\nArubaOS-CX Version 10.13\n");
        assert!(
            !cisco_from_prompt.contains(&"cisco".to_string()),
            "Cisco prompt startup detection should be blocked by bundled ArubaCX negative signal"
        );

        let juniper_from_weak_hint = store.detect_profiles("commit check\nPAN-OS 11.1\n");
        assert!(
            juniper_from_weak_hint.contains(&"juniper".to_string()),
            "Negative signals must not block weak detection hints"
        );

        let cisco_from_weak_hint = store.detect_profiles("line protocol is up\nArubaOS-CX\n");
        assert!(
            cisco_from_weak_hint.contains(&"cisco".to_string()),
            "Negative signals must not block weak detection hints"
        );
    }

    #[test]
    fn builtin_profiles_mark_only_generic_and_linux_as_local_baseline() {
        let store = ProfileStore::builtin();

        assert!(store.is_local_baseline_profile("generic"));
        assert!(store.is_local_baseline_profile("linux-unix"));

        for profile in [
            "arista",
            "arubacx",
            "cisco",
            "fortinet",
            "juniper",
            "palo-alto",
            "versa",
        ] {
            assert!(
                !store.is_local_baseline_profile(profile),
                "{profile} must not be treated as a local-shell baseline"
            );
        }
    }

    #[test]
    fn generic_profile_set_helper_matches_only_single_generic() {
        assert!(super::is_generic_profile_set(&["generic".to_string()]));
        assert!(!super::is_generic_profile_set(&[
            "generic".to_string(),
            "linux-unix".to_string(),
        ]));
        assert!(!super::is_generic_profile_set(&["cisco".to_string()]));
    }

    #[test]
    fn case_insensitive_signal_helpers_match_without_lowercase_allocations() {
        assert!(super::contains_case_insensitive(
            "Version: FortiGate-VM64 v7.4",
            "fortigate"
        ));
        assert!(super::contains_case_insensitive("cafe PAN-OS", "PAN-os"));
        assert!(!super::contains_case_insensitive("JUNOS", "ios"));
        assert!(super::starts_with_case_insensitive(
            "Version: FortiGate-VM64 v7.4",
            "version:"
        ));

        let source = include_str!("profiles.rs");
        let helper_source = source
            .split("fn contains_case_insensitive")
            .nth(1)
            .expect("contains_case_insensitive helper exists")
            .split("fn prompt_token")
            .next()
            .expect("helper source ends before prompt_token");
        assert!(
            !helper_source.contains("to_ascii_lowercase"),
            "case-insensitive signal helpers should avoid lowercase String allocations"
        );
    }

    // Two selected profiles that inherit each other must surface the cycle as
    // an error. Pre-filtering in `top_level_profile_names` used to drop both
    // (each looks "inherited by" the other), silently yielding a config with
    // zero rules and no diagnostic.
    #[test]
    fn mutually_inheriting_selected_profiles_error_instead_of_empty_config() {
        let mut store = ProfileStore::default();
        store.insert_profile(
            "alpha".to_string(),
            vec!["beta".to_string()],
            Vec::new(),
            vec![RuleSpec {
                description: "alpha keyword".to_string(),
                regex: r"\bup\b".to_string(),
                style: RuleStyle::Whole(Style::parse("f#00ff00").expect("style parses")),
                exclusive: false,
            }],
        );
        store.insert_profile(
            "beta".to_string(),
            vec!["alpha".to_string()],
            Vec::new(),
            Vec::new(),
        );

        match PrismConfig::from_profiles(&store, &["alpha", "beta"]) {
            Err(crate::config::ConfigError::CyclicProfileInheritance(cycle)) => {
                assert!(
                    cycle.contains("alpha") && cycle.contains("beta"),
                    "cycle message should name both profiles: {cycle}"
                );
            }
            Err(other) => panic!("expected cyclic-inheritance error, got: {other}"),
            Ok(_) => panic!("expected cyclic-inheritance error, got a silently empty config"),
        }
    }
}
