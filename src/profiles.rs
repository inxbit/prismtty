//! Built-in profile store and startup detection helpers.
//!
//! Profiles group highlighting rules with inheritance, startup detection hints,
//! and private runtime metadata used by interactive profile switching.

use crate::config::{ConfigError, RuleSpec, parse_builtin_profile_yaml};
use serde::{Deserialize, Deserializer, de};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use thiserror::Error;

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

/// Per-entry byte bound for user profile inheritance names and detection hints.
pub(crate) const MAX_PROFILE_METADATA_ENTRY_BYTES: usize = 256;
/// Maximum unique detection hints retained by one profile.
pub(crate) const MAX_PROFILE_DETECTION_HINTS: usize = 32;
/// Maximum normalized detection-hint bytes retained by one profile.
pub(crate) const MAX_PROFILE_DETECTION_BYTES: usize = 4 * 1024;
/// Maximum unique inheritance entries retained by one profile.
pub(crate) const MAX_PROFILE_INHERITANCE_ENTRIES: usize = 32;
/// Maximum normalized inheritance-name bytes retained by one profile.
pub(crate) const MAX_PROFILE_INHERITANCE_BYTES: usize = 4 * 1024;
/// Maximum detection hints retained across the complete profile store.
pub(crate) const MAX_STORE_DETECTION_HINTS: usize = 512;
/// Maximum normalized detection-hint bytes retained across the profile store.
pub(crate) const MAX_STORE_DETECTION_BYTES: usize = 64 * 1024;
/// Maximum inheritance edges retained across the complete profile store.
pub(crate) const MAX_STORE_INHERITANCE_ENTRIES: usize = 512;
/// Maximum normalized inheritance-name bytes retained across the profile store.
pub(crate) const MAX_STORE_INHERITANCE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct MetadataBudget {
    metadata: &'static str,
    count_measurement: &'static str,
    entry_measurement: &'static str,
    total_measurement: &'static str,
    max_entries: usize,
    max_entry_bytes: usize,
    max_total_bytes: usize,
}

const DETECTION_METADATA_BUDGET: MetadataBudget = MetadataBudget {
    metadata: "detection",
    count_measurement: "hint count",
    entry_measurement: "hint entry bytes",
    total_measurement: "hint bytes",
    max_entries: MAX_PROFILE_DETECTION_HINTS,
    max_entry_bytes: MAX_PROFILE_METADATA_ENTRY_BYTES,
    max_total_bytes: MAX_PROFILE_DETECTION_BYTES,
};

const INHERITANCE_METADATA_BUDGET: MetadataBudget = MetadataBudget {
    metadata: "inheritance",
    count_measurement: "entry count",
    entry_measurement: "entry bytes",
    total_measurement: "entry bytes",
    max_entries: MAX_PROFILE_INHERITANCE_ENTRIES,
    max_entry_bytes: MAX_PROFILE_METADATA_ENTRY_BYTES,
    max_total_bytes: MAX_PROFILE_INHERITANCE_BYTES,
};

const UNBOUNDED_INHERITANCE_METADATA: MetadataBudget = MetadataBudget {
    metadata: "inheritance",
    count_measurement: "entry count",
    entry_measurement: "entry bytes",
    total_measurement: "entry bytes",
    max_entries: usize::MAX,
    max_entry_bytes: usize::MAX,
    max_total_bytes: usize::MAX,
};

const UNBOUNDED_DETECTION_METADATA: MetadataBudget = MetadataBudget {
    metadata: "detection",
    count_measurement: "hint count",
    entry_measurement: "hint entry bytes",
    total_measurement: "hint bytes",
    max_entries: usize::MAX,
    max_entry_bytes: usize::MAX,
    max_total_bytes: usize::MAX,
};

#[derive(Debug, Error)]
#[error("{scope} {metadata} {measurement} is {actual}; limit is {limit}")]
pub(crate) struct ProfileMetadataLimitError {
    scope: &'static str,
    metadata: &'static str,
    measurement: &'static str,
    actual: usize,
    limit: usize,
}

/// Normalizes, de-duplicates, and bounds metadata before a parsed profile can
/// be retained or considered by auto-detection.
pub(crate) fn normalize_and_validate_profile_metadata(
    inherits: Vec<String>,
    detection: Vec<String>,
) -> Result<(Vec<String>, Vec<String>), ProfileMetadataLimitError> {
    Ok((
        normalize_metadata_entries(inherits, false, INHERITANCE_METADATA_BUDGET)?,
        normalize_metadata_entries(detection, true, DETECTION_METADATA_BUDGET)?,
    ))
}

fn normalize_profile_metadata_unbounded(
    inherits: Vec<String>,
    detection: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let inherits = normalize_metadata_entries(inherits, false, UNBOUNDED_INHERITANCE_METADATA)
        .expect("in-memory profile metadata cannot exceed usize limits");
    let detection = normalize_metadata_entries(detection, true, UNBOUNDED_DETECTION_METADATA)
        .expect("in-memory profile metadata cannot exceed usize limits");
    (inherits, detection)
}

fn normalize_metadata_entries(
    entries: Vec<String>,
    ascii_case_insensitive: bool,
    budget: MetadataBudget,
) -> Result<Vec<String>, ProfileMetadataLimitError> {
    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    let mut total_bytes = 0usize;

    for entry in entries {
        if entry.len() > budget.max_entry_bytes {
            return Err(metadata_limit(
                "profile",
                budget.metadata,
                budget.entry_measurement,
                entry.len(),
                budget.max_entry_bytes,
            ));
        }
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let key = if ascii_case_insensitive {
            entry.to_ascii_lowercase()
        } else {
            entry.to_string()
        };
        if !seen.insert(key) {
            continue;
        }

        let next_count = normalized.len().saturating_add(1);
        if next_count > budget.max_entries {
            return Err(metadata_limit(
                "profile",
                budget.metadata,
                budget.count_measurement,
                next_count,
                budget.max_entries,
            ));
        }
        let next_total_bytes = total_bytes.saturating_add(entry.len());
        if next_total_bytes > budget.max_total_bytes {
            return Err(metadata_limit(
                "profile",
                budget.metadata,
                budget.total_measurement,
                next_total_bytes,
                budget.max_total_bytes,
            ));
        }

        normalized.push(entry.to_string());
        total_bytes = next_total_bytes;
    }

    Ok(normalized)
}

fn metadata_limit(
    scope: &'static str,
    metadata: &'static str,
    measurement: &'static str,
    actual: usize,
    limit: usize,
) -> ProfileMetadataLimitError {
    ProfileMetadataLimitError {
        scope,
        metadata,
        measurement,
        actual,
        limit,
    }
}

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

const DETECTION_BOUNDARY_SYMBOL: u16 = u8::MAX as u16 + 1;
const NO_DETECTION_STATE: usize = usize::MAX;

#[derive(Clone, Copy)]
struct DetectionTransition {
    symbol: u16,
    target: usize,
}

#[derive(Clone, Default)]
struct DetectionNode {
    transitions: Vec<DetectionTransition>,
    failure: usize,
    output_link: usize,
    profile_outputs: Vec<usize>,
}

#[derive(Clone)]
struct DetectionMatcher {
    nodes: Vec<DetectionNode>,
    profile_count: usize,
    profiles_with_hints: usize,
    pattern_count: usize,
}

impl Default for DetectionMatcher {
    fn default() -> Self {
        Self {
            nodes: vec![DetectionNode {
                output_link: NO_DETECTION_STATE,
                ..DetectionNode::default()
            }],
            profile_count: 0,
            profiles_with_hints: 0,
            pattern_count: 0,
        }
    }
}

impl fmt::Debug for DetectionMatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetectionMatcher")
            .field("node_count", &self.nodes.len())
            .field("profile_count", &self.profile_count)
            .field("profiles_with_hints", &self.profiles_with_hints)
            .field("pattern_count", &self.pattern_count)
            .finish()
    }
}

#[derive(Default)]
struct DetectionScanMetrics {
    bytes_examined: usize,
    symbols_examined: usize,
    transition_lookups: usize,
    failure_steps: usize,
    output_checks: usize,
}

#[cfg(test)]
struct DetectionMatcherStructure {
    node_count: usize,
    edge_count: usize,
    transition_slots: usize,
    pattern_count: usize,
}

impl DetectionMatcher {
    fn from_profiles(profiles: &BTreeMap<String, Profile>) -> Self {
        let mut matcher = Self {
            profile_count: profiles.len(),
            ..Self::default()
        };

        for (profile_index, (name, profile)) in profiles.iter().enumerate() {
            if name == "generic" {
                continue;
            }
            let patterns_before = matcher.pattern_count;
            for hint in &profile.detection {
                matcher.insert_pattern(hint, profile_index);
            }
            if matcher.pattern_count > patterns_before {
                matcher.profiles_with_hints += 1;
            }
        }
        matcher.build_failure_links();
        matcher
    }

    fn insert_pattern(&mut self, hint: &str, profile_index: usize) {
        if hint.is_empty() {
            return;
        }

        let mut state = 0usize;
        let mut previous_word = None;
        for byte in hint.bytes() {
            let word = is_detection_word_byte(byte);
            if previous_word.is_some_and(|previous| previous != word)
                || (previous_word.is_none() && word)
            {
                state = self.ensure_transition(state, DETECTION_BOUNDARY_SYMBOL);
            }
            state = self.ensure_transition(state, u16::from(byte.to_ascii_lowercase()));
            previous_word = Some(word);
        }
        if previous_word == Some(true) {
            state = self.ensure_transition(state, DETECTION_BOUNDARY_SYMBOL);
        }

        if !self.nodes[state].profile_outputs.contains(&profile_index) {
            self.nodes[state].profile_outputs.push(profile_index);
            self.pattern_count += 1;
        }
    }

    fn ensure_transition(&mut self, state: usize, symbol: u16) -> usize {
        match self.nodes[state]
            .transitions
            .binary_search_by_key(&symbol, |transition| transition.symbol)
        {
            Ok(index) => self.nodes[state].transitions[index].target,
            Err(index) => {
                let target = self.nodes.len();
                self.nodes.push(DetectionNode {
                    output_link: NO_DETECTION_STATE,
                    ..DetectionNode::default()
                });
                self.nodes[state]
                    .transitions
                    .insert(index, DetectionTransition { symbol, target });
                target
            }
        }
    }

    fn transition(&self, state: usize, symbol: u16) -> Option<usize> {
        self.nodes[state]
            .transitions
            .binary_search_by_key(&symbol, |transition| transition.symbol)
            .ok()
            .map(|index| self.nodes[state].transitions[index].target)
    }

    fn build_failure_links(&mut self) {
        let mut pending = VecDeque::new();
        let root_children = self.nodes[0]
            .transitions
            .iter()
            .map(|transition| transition.target)
            .collect::<Vec<_>>();
        for child in root_children {
            self.nodes[child].failure = 0;
            self.nodes[child].output_link = NO_DETECTION_STATE;
            pending.push_back(child);
        }

        while let Some(state) = pending.pop_front() {
            let transitions = self.nodes[state].transitions.clone();
            for transition in transitions {
                let mut fallback = self.nodes[state].failure;
                let failure = loop {
                    if let Some(target) = self.transition(fallback, transition.symbol) {
                        break target;
                    }
                    if fallback == 0 {
                        break 0;
                    }
                    fallback = self.nodes[fallback].failure;
                };
                let output_link = if self.nodes[failure].profile_outputs.is_empty() {
                    self.nodes[failure].output_link
                } else {
                    failure
                };
                self.nodes[transition.target].failure = failure;
                self.nodes[transition.target].output_link = output_link;
                pending.push_back(transition.target);
            }
        }
    }

    fn matching_profiles(&self, haystack: &[u8]) -> Vec<bool> {
        self.scan_internal::<false>(haystack).0
    }

    fn scan_internal<const COUNT_OPERATIONS: bool>(
        &self,
        haystack: &[u8],
    ) -> (Vec<bool>, DetectionScanMetrics) {
        let mut matches = vec![false; self.profile_count];
        let mut metrics = DetectionScanMetrics::default();
        if self.profiles_with_hints == 0 {
            return (matches, metrics);
        }

        let mut state = 0usize;
        let mut matched_profiles = 0usize;
        let mut previous_word = None;
        for byte in haystack.iter().copied() {
            if COUNT_OPERATIONS {
                metrics.bytes_examined += 1;
            }
            let word = is_detection_word_byte(byte);
            if (previous_word.is_some_and(|previous| previous != word)
                || (previous_word.is_none() && word))
                && self.advance_symbol::<COUNT_OPERATIONS>(
                    DETECTION_BOUNDARY_SYMBOL,
                    &mut state,
                    &mut matches,
                    &mut matched_profiles,
                    &mut metrics,
                )
            {
                return (matches, metrics);
            }
            if self.advance_symbol::<COUNT_OPERATIONS>(
                u16::from(byte.to_ascii_lowercase()),
                &mut state,
                &mut matches,
                &mut matched_profiles,
                &mut metrics,
            ) {
                return (matches, metrics);
            }
            previous_word = Some(word);
        }
        if previous_word == Some(true) {
            self.advance_symbol::<COUNT_OPERATIONS>(
                DETECTION_BOUNDARY_SYMBOL,
                &mut state,
                &mut matches,
                &mut matched_profiles,
                &mut metrics,
            );
        }
        (matches, metrics)
    }

    fn advance_symbol<const COUNT_OPERATIONS: bool>(
        &self,
        symbol: u16,
        state: &mut usize,
        matches: &mut [bool],
        matched_profiles: &mut usize,
        metrics: &mut DetectionScanMetrics,
    ) -> bool {
        if COUNT_OPERATIONS {
            metrics.symbols_examined += 1;
        }
        loop {
            if COUNT_OPERATIONS {
                metrics.transition_lookups += 1;
            }
            if let Some(target) = self.transition(*state, symbol) {
                *state = target;
                break;
            }
            if *state == 0 {
                break;
            }
            *state = self.nodes[*state].failure;
            if COUNT_OPERATIONS {
                metrics.failure_steps += 1;
            }
        }

        let mut output_state = *state;
        loop {
            for profile_index in &self.nodes[output_state].profile_outputs {
                if COUNT_OPERATIONS {
                    metrics.output_checks += 1;
                }
                if !matches[*profile_index] {
                    matches[*profile_index] = true;
                    *matched_profiles += 1;
                    if *matched_profiles == self.profiles_with_hints {
                        return true;
                    }
                }
            }
            let next = self.nodes[output_state].output_link;
            if next == NO_DETECTION_STATE {
                break;
            }
            output_state = next;
        }
        false
    }

    #[cfg(test)]
    fn scan_with_metrics(&self, haystack: &[u8]) -> (Vec<bool>, DetectionScanMetrics) {
        self.scan_internal::<true>(haystack)
    }

    #[cfg(test)]
    fn structure(&self) -> DetectionMatcherStructure {
        DetectionMatcherStructure {
            node_count: self.nodes.len(),
            edge_count: self.nodes.iter().map(|node| node.transitions.len()).sum(),
            transition_slots: self
                .nodes
                .iter()
                .map(|node| node.transitions.capacity())
                .sum(),
            pattern_count: self.pattern_count,
        }
    }
}

/// Collection of built-in and user-registered profiles.
#[derive(Clone, Debug, Default)]
pub struct ProfileStore {
    profiles: BTreeMap<String, Profile>,
    detection_matcher: DetectionMatcher,
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
        store.rebuild_detection_matcher();
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

    /// Registers a trusted programmatic profile, returning `true` if it
    /// shadowed an existing profile of the same name.
    ///
    /// CLI-loaded profile files use the fallible, resource-bounded insertion
    /// path after parsing. This compatibility API still normalizes and
    /// de-duplicates metadata, but does not reject an in-memory caller's data.
    pub fn insert_profile(
        &mut self,
        name: String,
        inherits: Vec<String>,
        detection: Vec<String>,
        rules: Vec<RuleSpec>,
    ) -> bool {
        let (inherits, detection) = normalize_profile_metadata_unbounded(inherits, detection);
        self.insert_normalized_profile(name, inherits, detection, rules)
    }

    /// Registers a parsed user profile after enforcing complete-store metadata
    /// budgets. Replacements are charged only for their new metadata.
    pub(crate) fn try_insert_user_profile(
        &mut self,
        name: String,
        inherits: Vec<String>,
        detection: Vec<String>,
        rules: Vec<RuleSpec>,
    ) -> Result<bool, ProfileMetadataLimitError> {
        let (inherits, detection) = normalize_and_validate_profile_metadata(inherits, detection)?;
        self.validate_store_metadata_limits(&name, &inherits, &detection)?;
        Ok(self.insert_normalized_profile(name, inherits, detection, rules))
    }

    fn insert_normalized_profile(
        &mut self,
        name: String,
        inherits: Vec<String>,
        detection: Vec<String>,
        rules: Vec<RuleSpec>,
    ) -> bool {
        let shadowed = self
            .profiles
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
            .is_some();
        self.rebuild_detection_matcher();
        shadowed
    }

    fn rebuild_detection_matcher(&mut self) {
        self.detection_matcher = DetectionMatcher::from_profiles(&self.profiles);
    }

    fn validate_store_metadata_limits(
        &self,
        replaced_name: &str,
        inherits: &[String],
        detection: &[String],
    ) -> Result<(), ProfileMetadataLimitError> {
        let mut inheritance_count = inherits.len();
        let mut inheritance_bytes = metadata_bytes(inherits);
        let mut detection_count = detection.len();
        let mut detection_bytes = metadata_bytes(detection);

        for (name, profile) in &self.profiles {
            if name == replaced_name {
                continue;
            }
            inheritance_count = inheritance_count.saturating_add(profile.inherits.len());
            inheritance_bytes = inheritance_bytes.saturating_add(metadata_bytes(&profile.inherits));
            detection_count = detection_count.saturating_add(profile.detection.len());
            detection_bytes = detection_bytes.saturating_add(metadata_bytes(&profile.detection));
        }

        validate_store_metadata_limit(
            "detection",
            "hint count",
            detection_count,
            MAX_STORE_DETECTION_HINTS,
        )?;
        validate_store_metadata_limit(
            "detection",
            "hint bytes",
            detection_bytes,
            MAX_STORE_DETECTION_BYTES,
        )?;
        validate_store_metadata_limit(
            "inheritance",
            "entry count",
            inheritance_count,
            MAX_STORE_INHERITANCE_ENTRIES,
        )?;
        validate_store_metadata_limit(
            "inheritance",
            "entry bytes",
            inheritance_bytes,
            MAX_STORE_INHERITANCE_BYTES,
        )
    }

    /// Detects likely profiles from a startup sample, always including `generic`.
    pub fn detect_profiles(&self, sample: &str) -> Vec<String> {
        let lower = sample.to_ascii_lowercase();
        self.detect_profiles_with_lowercase(sample, &lower)
    }

    pub(crate) fn detect_profiles_with_lowercase(&self, sample: &str, lower: &str) -> Vec<String> {
        let hint_matches = self.detection_matcher.matching_profiles(lower.as_bytes());
        let mut detected: Vec<String> = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(profile_index, (name, profile))| {
                name.as_str() != "generic"
                    && profile.matches_startup_detection(sample, hint_matches[*profile_index])
            })
            .map(|(_profile_index, (name, _profile))| name.clone())
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
        if loaded.contains(profile_name) {
            return Ok(());
        }

        let rule_start = rules.len();
        let loaded_before = loaded.clone();
        let Some(profile) = self.profiles.get(profile_name) else {
            return Err(ConfigError::UnknownProfile(profile_name.to_string()));
        };
        rules.extend(profile.rules.clone());

        // Each frame stores the profile being resolved and the next parent to
        // visit. A profile can appear at most once in the active path, so stack
        // use is bounded by the registered graph instead of the process stack.
        let mut resolving = vec![(profile_name.to_string(), 0usize)];
        while let Some((current_name, next_parent)) = resolving.last().cloned() {
            let current = self
                .profiles
                .get(&current_name)
                .expect("only registered profiles enter the resolver stack");

            if next_parent >= current.inherits.len() {
                resolving.pop();
                loaded.insert(current_name);
                continue;
            }

            resolving.last_mut().expect("resolver stack is non-empty").1 += 1;
            let parent_name = current.inherits[next_parent].clone();
            if loaded.contains(&parent_name) {
                continue;
            }
            if let Some(cycle_start) = resolving
                .iter()
                .position(|(resolving_name, _)| resolving_name == &parent_name)
            {
                let mut cycle: Vec<String> = resolving[cycle_start..]
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect();
                cycle.push(parent_name);
                rules.truncate(rule_start);
                *loaded = loaded_before;
                return Err(ConfigError::CyclicProfileInheritance(cycle.join(" -> ")));
            }

            let Some(parent) = self.profiles.get(&parent_name) else {
                rules.truncate(rule_start);
                *loaded = loaded_before;
                return Err(ConfigError::UnknownProfile(parent_name));
            };
            rules.extend(parent.rules.clone());
            resolving.push((parent_name, 0));
        }

        Ok(())
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
        let mut pending = vec![profile_name.to_string()];
        let mut seen = BTreeSet::new();
        while let Some(current_name) = pending.pop() {
            if !seen.insert(current_name.clone()) {
                continue;
            }
            let profile = self
                .profiles
                .get(&current_name)
                .ok_or_else(|| ConfigError::UnknownProfile(current_name.clone()))?;
            if profile.inherits.iter().any(|parent| parent == ancestor) {
                return Ok(true);
            }
            pending.extend(profile.inherits.iter().rev().cloned());
        }
        Ok(false)
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
    fn matches_startup_detection(&self, sample: &str, detection_hint_matched: bool) -> bool {
        detection_hint_matched
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

fn metadata_bytes(entries: &[String]) -> usize {
    entries
        .iter()
        .fold(0usize, |total, entry| total.saturating_add(entry.len()))
}

fn validate_store_metadata_limit(
    metadata: &'static str,
    measurement: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), ProfileMetadataLimitError> {
    if actual > limit {
        return Err(metadata_limit(
            "profile store",
            metadata,
            measurement,
            actual,
            limit,
        ));
    }
    Ok(())
}

fn is_detection_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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

    fn fixed_length_entries(prefix: &str, count: usize, length: usize) -> Vec<String> {
        (0..count)
            .map(|index| {
                let label = format!("{prefix}-{index:04}-");
                assert!(label.len() <= length, "test label must fit entry length");
                format!("{label}{}", "x".repeat(length - label.len()))
            })
            .collect()
    }

    fn reference_contains_detection_hint(haystack: &str, hint: &str) -> bool {
        let needle = hint.as_bytes();
        if needle.is_empty() || needle.len() > haystack.len() {
            return false;
        }

        haystack
            .as_bytes()
            .windows(needle.len())
            .enumerate()
            .any(|(start, window)| {
                if !window.eq_ignore_ascii_case(needle) {
                    return false;
                }
                let end = start + needle.len();
                let starts_on_boundary = !super::is_detection_word_byte(needle[0])
                    || start == 0
                    || !super::is_detection_word_byte(haystack.as_bytes()[start - 1]);
                let ends_on_boundary = !super::is_detection_word_byte(needle[needle.len() - 1])
                    || end == haystack.len()
                    || !super::is_detection_word_byte(haystack.as_bytes()[end]);
                starts_on_boundary && ends_on_boundary
            })
    }

    fn reference_hint_profiles(store: &ProfileStore, sample: &str) -> Vec<String> {
        let mut detected = store
            .profiles
            .iter()
            .filter(|(name, profile)| {
                name.as_str() != "generic"
                    && profile
                        .detection
                        .iter()
                        .any(|hint| reference_contains_detection_hint(sample, hint))
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        store.sort_profiles_by_priority(&mut detected);

        let mut with_generic = vec!["generic".to_string()];
        with_generic.extend(detected);
        with_generic
    }

    fn next_generated(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed
    }

    fn generated_ascii(seed: &mut u64, max_len: usize) -> String {
        const ALPHABET: &[u8] = b"abCDxyZ019_ -.:/";
        let len = (next_generated(seed) as usize) % (max_len + 1);
        (0..len)
            .map(|_| {
                let index = (next_generated(seed) as usize) % ALPHABET.len();
                ALPHABET[index] as char
            })
            .collect()
    }

    fn generated_ascii_case(seed: &mut u64, value: &str) -> String {
        value
            .bytes()
            .map(|byte| {
                (if next_generated(seed) & 1 == 0 {
                    byte.to_ascii_lowercase()
                } else {
                    byte.to_ascii_uppercase()
                }) as char
            })
            .collect()
    }

    #[test]
    fn per_profile_detection_metadata_accepts_exact_limits_and_rejects_one_over() {
        let exact_count = fixed_length_entries("hint", super::MAX_PROFILE_DETECTION_HINTS, 16);
        let (_, normalized) =
            super::normalize_and_validate_profile_metadata(Vec::new(), exact_count)
                .expect("exact detection count limit should be accepted");
        assert_eq!(normalized.len(), super::MAX_PROFILE_DETECTION_HINTS);

        let count_error = super::normalize_and_validate_profile_metadata(
            Vec::new(),
            fixed_length_entries("hint", super::MAX_PROFILE_DETECTION_HINTS + 1, 16),
        )
        .expect_err("one hint above the count limit must be rejected");
        assert!(count_error.to_string().contains("detection hint count"));

        let exact_bytes = fixed_length_entries(
            "hint",
            super::MAX_PROFILE_DETECTION_BYTES / super::MAX_PROFILE_METADATA_ENTRY_BYTES,
            super::MAX_PROFILE_METADATA_ENTRY_BYTES,
        );
        super::normalize_and_validate_profile_metadata(Vec::new(), exact_bytes.clone())
            .expect("exact detection byte limit should be accepted");

        let mut one_byte_over = exact_bytes;
        one_byte_over.push("z".to_string());
        let byte_error = super::normalize_and_validate_profile_metadata(Vec::new(), one_byte_over)
            .expect_err("one byte above the detection byte limit must be rejected");
        assert!(byte_error.to_string().contains("detection hint bytes"));

        super::normalize_and_validate_profile_metadata(
            Vec::new(),
            vec!["x".repeat(super::MAX_PROFILE_METADATA_ENTRY_BYTES)],
        )
        .expect("an exact-length detection hint should be accepted");
        let entry_error = super::normalize_and_validate_profile_metadata(
            Vec::new(),
            vec!["x".repeat(super::MAX_PROFILE_METADATA_ENTRY_BYTES + 1)],
        )
        .expect_err("an oversized detection hint must be rejected");
        assert!(
            entry_error
                .to_string()
                .contains("detection hint entry bytes")
        );
    }

    #[test]
    fn per_profile_inheritance_metadata_accepts_exact_limits_and_rejects_one_over() {
        let exact_count =
            fixed_length_entries("parent", super::MAX_PROFILE_INHERITANCE_ENTRIES, 16);
        let (normalized, _) =
            super::normalize_and_validate_profile_metadata(exact_count, Vec::new())
                .expect("exact inheritance count limit should be accepted");
        assert_eq!(normalized.len(), super::MAX_PROFILE_INHERITANCE_ENTRIES);

        let count_error = super::normalize_and_validate_profile_metadata(
            fixed_length_entries("parent", super::MAX_PROFILE_INHERITANCE_ENTRIES + 1, 16),
            Vec::new(),
        )
        .expect_err("one parent above the count limit must be rejected");
        assert!(count_error.to_string().contains("inheritance entry count"));

        let exact_bytes = fixed_length_entries(
            "parent",
            super::MAX_PROFILE_INHERITANCE_BYTES / super::MAX_PROFILE_METADATA_ENTRY_BYTES,
            super::MAX_PROFILE_METADATA_ENTRY_BYTES,
        );
        super::normalize_and_validate_profile_metadata(exact_bytes.clone(), Vec::new())
            .expect("exact inheritance byte limit should be accepted");

        let mut one_byte_over = exact_bytes;
        one_byte_over.push("z".to_string());
        let byte_error = super::normalize_and_validate_profile_metadata(one_byte_over, Vec::new())
            .expect_err("one byte above the inheritance byte limit must be rejected");
        assert!(byte_error.to_string().contains("inheritance entry bytes"));

        super::normalize_and_validate_profile_metadata(
            vec!["x".repeat(super::MAX_PROFILE_METADATA_ENTRY_BYTES)],
            Vec::new(),
        )
        .expect("an exact-length inheritance name should be accepted");
        let entry_error = super::normalize_and_validate_profile_metadata(
            vec!["x".repeat(super::MAX_PROFILE_METADATA_ENTRY_BYTES + 1)],
            Vec::new(),
        )
        .expect_err("an oversized inheritance name must be rejected");
        assert!(entry_error.to_string().contains("inheritance entry bytes"));
    }

    #[test]
    fn store_detection_metadata_accepts_exact_aggregate_limits_and_replacements() {
        let mut count_store = ProfileStore::default();
        let profile_count = super::MAX_STORE_DETECTION_HINTS / super::MAX_PROFILE_DETECTION_HINTS;
        for profile_index in 0..profile_count {
            count_store
                .try_insert_user_profile(
                    format!("count-{profile_index}"),
                    Vec::new(),
                    fixed_length_entries(
                        &format!("h{profile_index}"),
                        super::MAX_PROFILE_DETECTION_HINTS,
                        16,
                    ),
                    Vec::new(),
                )
                .expect("aggregate detection count up to the exact limit should be accepted");
        }
        assert!(
            count_store
                .try_insert_user_profile(
                    "count-0".to_string(),
                    Vec::new(),
                    fixed_length_entries("replacement", super::MAX_PROFILE_DETECTION_HINTS, 20,),
                    Vec::new(),
                )
                .expect("replacing a profile must subtract its old metadata first")
        );
        let count_error = count_store
            .try_insert_user_profile(
                "one-over".to_string(),
                Vec::new(),
                vec!["z".to_string()],
                Vec::new(),
            )
            .expect_err("one hint above the store count limit must be rejected");
        assert!(
            count_error
                .to_string()
                .contains("store detection hint count")
        );

        let mut byte_store = ProfileStore::default();
        let entries_per_profile =
            super::MAX_PROFILE_DETECTION_BYTES / super::MAX_PROFILE_METADATA_ENTRY_BYTES;
        let profile_count = super::MAX_STORE_DETECTION_BYTES / super::MAX_PROFILE_DETECTION_BYTES;
        for profile_index in 0..profile_count {
            byte_store
                .try_insert_user_profile(
                    format!("bytes-{profile_index}"),
                    Vec::new(),
                    fixed_length_entries(
                        &format!("b{profile_index}"),
                        entries_per_profile,
                        super::MAX_PROFILE_METADATA_ENTRY_BYTES,
                    ),
                    Vec::new(),
                )
                .expect("aggregate detection bytes up to the exact limit should be accepted");
        }
        let byte_error = byte_store
            .try_insert_user_profile(
                "one-over".to_string(),
                Vec::new(),
                vec!["z".to_string()],
                Vec::new(),
            )
            .expect_err("one byte above the store detection limit must be rejected");
        assert!(
            byte_error
                .to_string()
                .contains("store detection hint bytes")
        );
    }

    #[test]
    fn store_inheritance_metadata_accepts_exact_aggregate_limits() {
        let mut count_store = ProfileStore::default();
        let profile_count =
            super::MAX_STORE_INHERITANCE_ENTRIES / super::MAX_PROFILE_INHERITANCE_ENTRIES;
        for profile_index in 0..profile_count {
            count_store
                .try_insert_user_profile(
                    format!("count-{profile_index}"),
                    fixed_length_entries(
                        &format!("p{profile_index}"),
                        super::MAX_PROFILE_INHERITANCE_ENTRIES,
                        16,
                    ),
                    Vec::new(),
                    Vec::new(),
                )
                .expect("aggregate inheritance count up to the exact limit should be accepted");
        }
        let count_error = count_store
            .try_insert_user_profile(
                "one-over".to_string(),
                vec!["z".to_string()],
                Vec::new(),
                Vec::new(),
            )
            .expect_err("one parent above the store count limit must be rejected");
        assert!(
            count_error
                .to_string()
                .contains("store inheritance entry count")
        );

        let mut byte_store = ProfileStore::default();
        let entries_per_profile =
            super::MAX_PROFILE_INHERITANCE_BYTES / super::MAX_PROFILE_METADATA_ENTRY_BYTES;
        let profile_count =
            super::MAX_STORE_INHERITANCE_BYTES / super::MAX_PROFILE_INHERITANCE_BYTES;
        for profile_index in 0..profile_count {
            byte_store
                .try_insert_user_profile(
                    format!("bytes-{profile_index}"),
                    fixed_length_entries(
                        &format!("p{profile_index}"),
                        entries_per_profile,
                        super::MAX_PROFILE_METADATA_ENTRY_BYTES,
                    ),
                    Vec::new(),
                    Vec::new(),
                )
                .expect("aggregate inheritance bytes up to the exact limit should be accepted");
        }
        let byte_error = byte_store
            .try_insert_user_profile(
                "one-over".to_string(),
                vec!["z".to_string()],
                Vec::new(),
                Vec::new(),
            )
            .expect_err("one byte above the store inheritance limit must be rejected");
        assert!(
            byte_error
                .to_string()
                .contains("store inheritance entry bytes")
        );
    }

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

    #[test]
    fn multi_pattern_detection_matches_simple_reference_across_generated_cases() {
        let mut store = ProfileStore::default();
        store.insert_profile(
            "fixed-boundaries".to_string(),
            Vec::new(),
            vec![
                "junos".to_string(),
                "-edge".to_string(),
                "v1.".to_string(),
                "_id".to_string(),
                "id_".to_string(),
                "Café".to_string(),
            ],
            Vec::new(),
        );

        let mut seed = 0x6a09_e667_f3bc_c909_u64;
        for profile_index in 0..12 {
            let hints = (0..8)
                .map(|_| {
                    let generated = generated_ascii(&mut seed, 20);
                    if generated.trim().is_empty() {
                        format!("hint-{profile_index}")
                    } else {
                        generated
                    }
                })
                .collect();
            store.insert_profile(
                format!("generated-{profile_index:02}"),
                Vec::new(),
                hints,
                Vec::new(),
            );
        }

        let normalized_hints = store
            .profiles
            .values()
            .flat_map(|profile| profile.detection.iter().cloned())
            .collect::<Vec<_>>();
        let fixed_samples = [
            "prefix JUNOS suffix",
            "prejunosx",
            "router-edge",
            "routerx-edge",
            "release v1. ready",
            "release v1.x",
            "_id id_",
            "x_idy xid_x",
            "CAFé",
            "CAFÉ",
            "",
        ];
        for sample in fixed_samples {
            assert_eq!(
                store.detect_profiles(sample),
                reference_hint_profiles(&store, sample),
                "fixed sample mismatch for {sample:?}"
            );
        }

        const NEIGHBORS: &[&str] = &["", "x", " ", "-", "_", "/"];
        for case in 0..1_000 {
            let mut sample = generated_ascii(&mut seed, 160);
            if case % 3 == 0 {
                let hint = &normalized_hints
                    [(next_generated(&mut seed) as usize) % normalized_hints.len()];
                let prefix = NEIGHBORS[(next_generated(&mut seed) as usize) % NEIGHBORS.len()];
                let suffix = NEIGHBORS[(next_generated(&mut seed) as usize) % NEIGHBORS.len()];
                sample.push_str(prefix);
                sample.push_str(&generated_ascii_case(&mut seed, hint));
                sample.push_str(suffix);
                sample.push_str(&generated_ascii(&mut seed, 80));
            }

            assert_eq!(
                store.detect_profiles(&sample),
                reference_hint_profiles(&store, &sample),
                "generated sample {case} mismatch for {sample:?}"
            );
        }
    }

    #[test]
    fn aggregate_detection_scan_has_sparse_linear_structure() {
        let mut store = ProfileStore::default();
        for profile_index in 0..16 {
            let hints = (0..super::MAX_PROFILE_DETECTION_HINTS)
                .map(|hint_index| {
                    let index = profile_index * super::MAX_PROFILE_DETECTION_HINTS + hint_index;
                    format!("{}z{index:03}", "a".repeat(28))
                })
                .collect();
            store
                .try_insert_user_profile(
                    format!("profile-{profile_index:02}"),
                    Vec::new(),
                    hints,
                    Vec::new(),
                )
                .expect("exact aggregate hint count remains valid");
        }

        let haystack = "a".repeat(64 * 1024);
        let (matched_profiles, metrics) = store
            .detection_matcher
            .scan_with_metrics(haystack.as_bytes());
        let structure = store.detection_matcher.structure();

        assert!(!matched_profiles.into_iter().any(|matched| matched));
        assert_eq!(metrics.bytes_examined, haystack.len());
        assert!(
            metrics.transition_lookups <= metrics.symbols_examined * 2,
            "aggregate scan performed {} transition lookups for {} symbols",
            metrics.transition_lookups,
            metrics.symbols_examined
        );
        assert!(metrics.failure_steps <= haystack.len());
        assert_eq!(metrics.output_checks, 0);
        assert_eq!(structure.pattern_count, super::MAX_STORE_DETECTION_HINTS);
        assert_eq!(structure.edge_count + 1, structure.node_count);
        assert!(
            structure.transition_slots <= structure.edge_count.saturating_mul(4).max(1),
            "sparse edges retained {} slots for {} transitions",
            structure.transition_slots,
            structure.edge_count
        );
    }

    #[test]
    fn word_boundaries_prune_overlapping_output_floods() {
        let mut store = ProfileStore::default();
        for profile_index in 0..16 {
            let hints = (1..=super::MAX_PROFILE_DETECTION_HINTS)
                .map(|length| "a".repeat(length))
                .collect();
            store
                .try_insert_user_profile(
                    format!("profile-{profile_index:02}"),
                    Vec::new(),
                    hints,
                    Vec::new(),
                )
                .expect("overlapping hints fit the aggregate metadata budget");
        }

        let haystack = "a".repeat(64 * 1024);
        let (matched_profiles, metrics) = store
            .detection_matcher
            .scan_with_metrics(haystack.as_bytes());

        assert!(!matched_profiles.into_iter().any(|matched| matched));
        assert_eq!(metrics.bytes_examined, haystack.len());
        assert!(metrics.symbols_examined <= haystack.len() + 2);
        assert!(metrics.transition_lookups <= metrics.symbols_examined * 2);
        assert_eq!(
            metrics.output_checks, 0,
            "word-internal suffixes must not become boundary-valid outputs"
        );
    }

    #[test]
    fn detection_matcher_rebuilds_after_insert_replace_and_clone() {
        let mut store = ProfileStore::default();
        store.insert_profile(
            "router".to_string(),
            Vec::new(),
            vec!["first-os".to_string()],
            Vec::new(),
        );
        assert_eq!(
            store.detect_profiles("FIRST-OS ready"),
            vec!["generic", "router"]
        );

        assert!(store.insert_profile(
            "router".to_string(),
            Vec::new(),
            vec!["second-os".to_string()],
            Vec::new(),
        ));
        assert_eq!(store.detect_profiles("first-os ready"), vec!["generic"]);
        assert_eq!(
            store.detect_profiles("SECOND-OS ready"),
            vec!["generic", "router"]
        );

        let cloned = store.clone();
        assert_eq!(
            cloned.detect_profiles("second-os ready"),
            vec!["generic", "router"]
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
