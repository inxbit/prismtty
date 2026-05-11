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

pub const USER_PROFILE_RUNTIME_PRIORITY: u16 = 100;

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub inherits: Vec<String>,
    pub detection: Vec<String>,
    pub runtime: ProfileRuntimeMeta,
    pub rules: Vec<RuleSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileRuntimeMeta {
    pub priority: u16,
    #[serde(default)]
    pub strong_signals: Vec<StrongSignal>,
    pub startup_prompt: PromptMatcherKind,
    pub runtime_prompt: PromptMatcherKind,
    #[serde(default)]
    pub prompt_confidence: PromptConfidence,
}

impl Default for ProfileRuntimeMeta {
    fn default() -> Self {
        Self {
            priority: USER_PROFILE_RUNTIME_PRIORITY,
            strong_signals: Vec::new(),
            startup_prompt: PromptMatcherKind::None,
            runtime_prompt: PromptMatcherKind::None,
            prompt_confidence: PromptConfidence::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StrongSignal {
    Contains { value: String },
    ContainsAny { values: Vec<String> },
    LinePrefixAndAny { prefix: String, values: Vec<String> },
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
            StrongSignal::LinePrefixAndAny { prefix, values } => {
                let prefix = prefix.to_ascii_lowercase();
                text.lines().any(|line| {
                    let lower = line.trim().to_ascii_lowercase();
                    lower.starts_with(&prefix)
                        && values
                            .iter()
                            .any(|value| lower.contains(&value.to_ascii_lowercase()))
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptMatcherKind {
    None,
    JunosUserAtHost,
    CiscoHostMarker,
    AristaHostMarker,
    FortinetHostHash,
    UnixUserAtHostPath,
    PaloAltoUserAtHost,
    VersaUserAtHost,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptConfidence {
    #[default]
    Repeated,
    SingleAfterRemoteHint,
    SingleFromBaselineOrRemoteHint,
}

#[derive(Clone, Debug, Default)]
pub struct ProfileStore {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileStore {
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

    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(String::as_str).collect()
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn insert_profile(
        &mut self,
        name: String,
        inherits: Vec<String>,
        detection: Vec<String>,
        rules: Vec<RuleSpec>,
    ) {
        self.profiles.insert(
            name.clone(),
            Profile {
                name,
                inherits,
                detection,
                runtime: ProfileRuntimeMeta::default(),
                rules,
            },
        );
    }

    pub fn detect_profiles(&self, sample: &str) -> Vec<String> {
        let lower = sample.to_ascii_lowercase();
        let mut detected: Vec<String> = self
            .profiles
            .iter()
            .filter(|(name, profile)| {
                name.as_str() != "generic" && profile.matches_startup_detection(sample, &lower)
            })
            .map(|(name, _profile)| name.clone())
            .collect();
        self.sort_profiles_by_priority(&mut detected);

        let mut with_generic = vec!["generic".to_string()];
        with_generic.extend(detected);
        with_generic
    }

    pub fn append_profile_rules(
        &self,
        profile_name: &str,
        loaded: &mut BTreeSet<String>,
        rules: &mut Vec<RuleSpec>,
    ) -> Result<(), ConfigError> {
        if loaded.contains(profile_name) {
            return Ok(());
        }

        let profile = self
            .profiles
            .get(profile_name)
            .ok_or_else(|| ConfigError::UnknownProfile(profile_name.to_string()))?;

        for parent in &profile.inherits {
            self.append_profile_rules(parent, loaded, rules)?;
        }

        loaded.insert(profile.name.clone());
        rules.extend(profile.rules.clone());
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
            || self.runtime.startup_prompt.matches_startup(sample, lower)
    }

    fn matches_strong_signal(&self, text: &str) -> bool {
        self.runtime
            .strong_signals
            .iter()
            .any(|signal| signal.matches(text))
    }

    fn matches_runtime_prompt(&self, text: &str) -> bool {
        self.runtime.runtime_prompt.matches_runtime(text)
    }
}

impl PromptMatcherKind {
    fn matches_startup(self, sample: &str, lower: &str) -> bool {
        match self {
            PromptMatcherKind::None => false,
            PromptMatcherKind::JunosUserAtHost => {
                looks_like_juniper_prompt(sample)
                    && !contains_case_insensitive(lower, "pan-os")
                    && !contains_case_insensitive(lower, "pa-vm")
                    && !contains_case_insensitive(lower, "palo alto")
                    && !contains_case_insensitive(lower, "versa-")
                    && !contains_case_insensitive(lower, "versa networks")
                    && !contains_case_insensitive(lower, "versa director")
            }
            PromptMatcherKind::CiscoHostMarker => {
                looks_like_cisco_prompt(sample)
                    && !contains_case_insensitive(lower, "arubaos-cx")
                    && !contains_case_insensitive(lower, "aos-cx")
                    && !contains_case_insensitive(lower, "hpe-restd")
            }
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
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
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
    let prompt = prompt_token(line);
    let Some((user, rest)) = prompt.split_once('@') else {
        return false;
    };
    let Some((host, tail)) = rest.split_once(':') else {
        return false;
    };
    let marker = tail
        .bytes()
        .position(|byte| matches!(byte, b'#' | b'$' | b'%'));
    !user.is_empty()
        && !host.is_empty()
        && marker.is_some()
        && user.bytes().all(is_prompt_name_byte)
        && host.bytes().all(is_prompt_name_byte)
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
    use crate::highlight::Highlighter;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

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
        assert!(PromptMatcherKind::CiscoHostMarker.matches_startup("Router(config-if)#", ""));
        assert!(!PromptMatcherKind::AristaHostMarker.matches_startup("Router(config-if)#", ""));
        assert!(PromptMatcherKind::AristaHostMarker.matches_startup("leaf01#", ""));
        assert!(!PromptMatcherKind::PaloAltoUserAtHost.matches_startup("admin@router>", ""));
        assert!(PromptMatcherKind::PaloAltoUserAtHost.matches_startup("admin@pa-edge>", ""));
    }
}
