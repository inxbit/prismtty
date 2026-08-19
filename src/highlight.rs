//! Terminal highlighting engines.
//!
//! `Highlighter` applies compiled rules to complete byte slices or strings.
//! `StreamingHighlighter` keeps enough state to highlight chunked terminal
//! output without breaking ANSI escapes or interactive command echo.

use crate::config::{CaptureRef, PrismConfig, RuleSpec, RuleStyle};
use crate::style::{ColorMode, Style};
use crate::terminal_text::escape_untrusted;
use pcre2::bytes::{Regex, RegexBuilder};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

const UNICODE_PROMPT_MARKERS: &[&str] = &["○", "●", "❯", "❮", "❱", "›", "»", "➜", "➤", "λ"];
pub(crate) const MAX_INCOMPLETE_ESCAPE_BYTES: usize = 16 * 1024;
const MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES: usize = 1024;
const MAX_TENTATIVE_UTF8_SUFFIX_BYTES: usize = 2;
const PCRE2_JIT_STACK_LIMIT_BYTES: usize = 32 * 1024;
const PCRE2_MATCH_LIMIT: usize = 100_000;
const PCRE2_DEPTH_LIMIT: usize = 1_000;
const MAX_COMPILED_RULES: usize = 512;
/// How many runtime match errors a rule may hit on one chunk before the scan
/// gives up. Each error means PCRE2 spent its whole match budget at that
/// starting position, so recovery has to stay bounded; a handful is enough to
/// get past one pathological span without letting a crafted line multiply the
/// budget by its length.
const MAX_RULE_MATCH_ERRORS: usize = 4;
const MAX_TOTAL_PATTERN_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
struct RuleIdentity {
    fingerprint: u64,
    spec: Arc<RuleSpec>,
}

impl PartialEq for RuleIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && (Arc::ptr_eq(&self.spec, &other.spec) || self.spec == other.spec)
    }
}

impl Eq for RuleIdentity {}

impl Hash for RuleIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

/// Interns rule identities for one configuration generation. Profile switches
/// compile independent highlighters, but identical rules share an `Arc`, so
/// quarantine and benchmark lookups stay constant-cost in the streaming path.
#[derive(Debug, Default)]
pub(crate) struct RuleIdentityRegistry {
    entries: HashMap<u64, Vec<Arc<RuleSpec>>>,
}

impl RuleIdentityRegistry {
    fn identity_for(&mut self, rule: &RuleSpec) -> RuleIdentity {
        let mut hasher = DefaultHasher::new();
        rule.hash(&mut hasher);
        let fingerprint = hasher.finish();
        let bucket = self.entries.entry(fingerprint).or_default();
        if let Some(spec) = bucket.iter().find(|candidate| candidate.as_ref() == rule) {
            return RuleIdentity {
                fingerprint,
                spec: Arc::clone(spec),
            };
        }

        let spec = Arc::new(rule.clone());
        bucket.push(Arc::clone(&spec));
        RuleIdentity { fingerprint, spec }
    }
}

/// Errors returned while compiling highlighting rules.
#[derive(Debug, Error)]
pub enum HighlightError {
    /// A PCRE2 rule failed to compile.
    #[error("rule '{description}' failed to compile: {source}")]
    Regex {
        /// Human-readable rule description.
        description: String,
        /// PCRE2 compilation error.
        source: pcre2::Error,
    },
}

/// A bounded PCRE2 match failed while applying a compiled rule.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("rule '{description}' match failed: {message}")]
pub struct RuleMatchError {
    /// Human-readable rule description.
    pub description: String,
    /// PCRE2 runtime error text.
    pub message: String,
}

/// Compiled highlighter for complete terminal output chunks.
#[derive(Clone, Debug)]
pub struct Highlighter {
    rules: Vec<CompiledRule>,
    color_mode: ColorMode,
}

/// Visible text span matched by a highlight rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    /// Matched visible text.
    pub text: String,
    /// Start byte offset in the ANSI-stripped visible input.
    pub start: usize,
    /// End byte offset in the ANSI-stripped visible input.
    pub end: usize,
    /// Style that would be applied to this span.
    pub style: Style,
}

/// Stateful highlighter for chunked terminal streams.
#[derive(Clone, Debug)]
pub struct StreamingHighlighter {
    highlighter: Highlighter,
    pending: Vec<u8>,
    oversized_string_controls: OversizedStringControlFilter,
    /// True when `pending` holds the trailing token split off a prompt-echo
    /// redraw remainder (set at the prompt-echo split branches in
    /// `push_combined_chunk`). The read loop flushes such a tail on idle even
    /// without a recent-input byte match; program output buffered by the normal
    /// streaming split leaves this `false`.
    pending_from_prompt_echo_remainder: bool,
    alternate_screen: bool,
    passthrough_single_byte_chunks: bool,
    prompt_echo_passthrough: bool,
    visible_line_tail: Vec<u8>,
    native_sgr: NativeSgrState,
    interactive_overlay: Option<Style>,
    no_minimal_resets: bool,
    /// Per-rule matching time, collected in every mode so the stream layer can
    /// surface pathologically slow user rules; exposed as a report only when
    /// `benchmark_enabled`.
    benchmark: BenchmarkReport,
    benchmark_enabled: bool,
    disabled_rule_identities: HashSet<RuleIdentity>,
}

/// Incrementally tracks an incomplete terminal string control. Once the
/// control exceeds the bounded carry budget, its payload is discarded until a
/// BEL/ST terminator or CAN/SUB cancellation arrives. This prevents bytes
/// inside a hostile oversized OSC/DCS/SOS/PM/APC payload from being
/// reinterpreted as top-level terminal controls on a later read.
#[derive(Clone, Debug, Default)]
pub(crate) struct OversizedStringControlFilter {
    mode: OversizedStringControlMode,
}

#[derive(Clone, Debug, Default)]
enum OversizedStringControlMode {
    #[default]
    Normal,
    Tracking(TrackedStringControl),
    Discarding(DiscardedStringControl),
}

#[derive(Clone, Copy, Debug)]
enum StringControlKind {
    EscOsc,
    EscString(u8),
    C1Osc(usize),
    C1String(u8, usize),
}

impl StringControlKind {
    fn at(bytes: &[u8], start: usize) -> Option<(Self, usize)> {
        match *bytes.get(start)? {
            0x1b => match *bytes.get(start + 1)? {
                b']' => Some((Self::EscOsc, start + 2)),
                byte @ (b'P' | b'X' | b'^' | b'_') => Some((Self::EscString(byte), start + 2)),
                _ => None,
            },
            _ => match c1_control_at(bytes, start)? {
                (0x9d, width) => Some((Self::C1Osc(width), start + width)),
                (byte @ (0x90 | 0x98 | 0x9e | 0x9f), width) => {
                    Some((Self::C1String(byte, width), start + width))
                }
                _ => None,
            },
        }
    }

    fn allows_bel(self) -> bool {
        matches!(self, Self::EscOsc | Self::C1Osc(_))
    }

    fn matches_at(self, bytes: &[u8], start: usize) -> bool {
        match self {
            Self::EscOsc => bytes.get(start..start + 2) == Some(b"\x1b]"),
            Self::EscString(byte) => {
                bytes.get(start) == Some(&0x1b) && bytes.get(start + 1) == Some(&byte)
            }
            Self::C1Osc(width) => c1_control_at(bytes, start) == Some((0x9d, width)),
            Self::C1String(byte, width) => c1_control_at(bytes, start) == Some((byte, width)),
        }
    }

    fn sequence_end(self, bytes: &[u8], start: usize) -> usize {
        match self {
            Self::EscOsc | Self::EscString(_) => ansi_sequence_end(bytes, start),
            Self::C1Osc(width) => c1_string_escape_end(bytes, start, 0x9d, width),
            Self::C1String(control, width) => c1_string_escape_end(bytes, start, control, width),
        }
    }
}

#[derive(Clone, Debug)]
struct TrackedStringControl {
    kind: StringControlKind,
    /// Earliest cancelled control prefix that must be removed if this nested
    /// string is later discarded. Removing only the nested ESC/C1 introducer
    /// would leave the cancelled outer control open in sanitized output.
    drop_start: usize,
    start: usize,
    carry_offset: usize,
    scanned: usize,
    total_bytes: usize,
    payload: StringPayloadScanner,
}

impl TrackedStringControl {
    fn new(kind: StringControlKind, start: usize, payload_start: usize, drop_start: usize) -> Self {
        Self {
            kind,
            drop_start,
            start,
            carry_offset: start.saturating_sub(drop_start),
            scanned: payload_start,
            total_bytes: payload_start - start,
            payload: StringPayloadScanner::new(kind.allows_bel()),
        }
    }

    fn align_with_reassembled_carry(&mut self, bytes: &[u8]) -> bool {
        // Both streaming pending and sanitize carry begin at the incomplete
        // control after their already-emitted visible prefix is removed. Keep
        // the tracker relative to that retained suffix; otherwise a nested
        // same-kind introducer at the stale absolute offset can be mistaken for
        // the outer control and make us skip an earlier terminator/cancellation.
        let start = self.carry_offset;
        if start + self.total_bytes <= bytes.len() && self.kind.matches_at(bytes, start) {
            self.drop_start = 0;
            self.start = start;
            self.scanned = start + self.total_bytes;
            return true;
        }
        false
    }

    fn scan_more(&mut self, bytes: &[u8]) -> TrackedStringScan {
        while self.scanned < bytes.len() {
            if self.total_bytes >= MAX_INCOMPLETE_ESCAPE_BYTES {
                return TrackedStringScan::Oversized(self.scanned);
            }
            let byte = bytes[self.scanned];
            self.scanned += 1;
            self.total_bytes += 1;
            match self.payload.feed(byte) {
                StringPayloadScan::Continue => {}
                StringPayloadScan::CompleteAfterCurrent => {
                    return TrackedStringScan::Complete(self.scanned);
                }
                StringPayloadScan::CompleteBeforeCurrent { replay_len, .. } => {
                    return TrackedStringScan::Complete(
                        self.scanned
                            .checked_sub(1 + replay_len)
                            .expect("tentative UTF-8 suffix is part of the scanned buffer"),
                    );
                }
            }
        }
        TrackedStringScan::Incomplete
    }

    fn into_discarded(self) -> DiscardedStringControl {
        DiscardedStringControl {
            payload: self.payload,
        }
    }
}

enum TrackedStringScan {
    Complete(usize),
    Incomplete,
    Oversized(usize),
}

enum StringPayloadScan {
    Continue,
    CompleteAfterCurrent,
    CompleteBeforeCurrent {
        replay: [u8; MAX_TENTATIVE_UTF8_SUFFIX_BYTES],
        replay_len: usize,
    },
}

struct DiscardedTerminator {
    end: usize,
    replay: [u8; MAX_TENTATIVE_UTF8_SUFFIX_BYTES],
    replay_len: usize,
}

#[derive(Clone, Debug)]
struct DiscardedStringControl {
    payload: StringPayloadScanner,
}

impl DiscardedStringControl {
    fn terminator(&mut self, bytes: &[u8], start: usize) -> Option<DiscardedTerminator> {
        for (offset, byte) in bytes[start..].iter().copied().enumerate() {
            match self.payload.feed(byte) {
                StringPayloadScan::Continue => {}
                StringPayloadScan::CompleteAfterCurrent => {
                    return Some(DiscardedTerminator {
                        end: start + offset + 1,
                        replay: [0; MAX_TENTATIVE_UTF8_SUFFIX_BYTES],
                        replay_len: 0,
                    });
                }
                StringPayloadScan::CompleteBeforeCurrent { replay, replay_len } => {
                    return Some(DiscardedTerminator {
                        end: start + offset,
                        replay,
                        replay_len,
                    });
                }
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
struct StringPayloadScanner {
    allows_bel: bool,
    saw_escape: bool,
    utf8_remaining: u8,
    next_utf8_min: u8,
    next_utf8_max: u8,
    utf8_c1_st_possible: bool,
    utf8_raw_st_pending: bool,
    utf8_raw_st_replay: [u8; MAX_TENTATIVE_UTF8_SUFFIX_BYTES],
    utf8_raw_st_replay_len: u8,
}

impl StringPayloadScanner {
    fn new(allows_bel: bool) -> Self {
        Self {
            allows_bel,
            saw_escape: false,
            utf8_remaining: 0,
            next_utf8_min: 0x80,
            next_utf8_max: 0xbf,
            utf8_c1_st_possible: false,
            utf8_raw_st_pending: false,
            utf8_raw_st_replay: [0; MAX_TENTATIVE_UTF8_SUFFIX_BYTES],
            utf8_raw_st_replay_len: 0,
        }
    }

    /// Reports a BEL/ST terminator or CAN/SUB cancellation. A raw ST that was
    /// tentatively consumed as UTF-8 is retained until the full codepoint is
    /// valid, so malformed input can terminate before the byte that disproves
    /// the candidate without dropping that visible suffix byte.
    fn feed(&mut self, byte: u8) -> StringPayloadScan {
        if self.utf8_remaining > 0 {
            if (self.next_utf8_min..=self.next_utf8_max).contains(&byte) {
                if self.utf8_raw_st_pending {
                    let replay_len = usize::from(self.utf8_raw_st_replay_len);
                    debug_assert!(replay_len < self.utf8_raw_st_replay.len());
                    self.utf8_raw_st_replay[replay_len] = byte;
                    self.utf8_raw_st_replay_len += 1;
                } else if byte == 0x9c {
                    self.utf8_raw_st_pending = true;
                    self.utf8_raw_st_replay_len = 0;
                }
                self.utf8_remaining -= 1;
                let is_utf8_c1_st =
                    self.utf8_c1_st_possible && self.utf8_remaining == 0 && byte == 0x9c;
                self.next_utf8_min = 0x80;
                self.next_utf8_max = 0xbf;
                self.utf8_c1_st_possible = false;
                if self.utf8_remaining == 0 {
                    self.clear_tentative_raw_st();
                }
                return if is_utf8_c1_st {
                    StringPayloadScan::CompleteAfterCurrent
                } else {
                    StringPayloadScan::Continue
                };
            }
            self.utf8_remaining = 0;
            self.utf8_c1_st_possible = false;
            if self.utf8_raw_st_pending {
                let replay = self.utf8_raw_st_replay;
                let replay_len = usize::from(self.utf8_raw_st_replay_len);
                self.clear_tentative_raw_st();
                return StringPayloadScan::CompleteBeforeCurrent { replay, replay_len };
            }
        }

        if self.saw_escape {
            self.saw_escape = false;
            if byte == b'\\' {
                return StringPayloadScan::CompleteAfterCurrent;
            }
        }

        if matches!(byte, 0x18 | 0x1a) || (self.allows_bel && byte == 0x07) || byte == 0x9c {
            return StringPayloadScan::CompleteAfterCurrent;
        }
        if byte == 0x1b {
            self.saw_escape = true;
            return StringPayloadScan::Continue;
        }

        match byte {
            0xc2 => {
                self.begin_utf8(1, 0x80, 0xbf);
                self.utf8_c1_st_possible = true;
            }
            0xc3..=0xdf => self.begin_utf8(1, 0x80, 0xbf),
            0xe0 => self.begin_utf8(2, 0xa0, 0xbf),
            0xe1..=0xec | 0xee..=0xef => self.begin_utf8(2, 0x80, 0xbf),
            0xed => self.begin_utf8(2, 0x80, 0x9f),
            0xf0 => self.begin_utf8(3, 0x90, 0xbf),
            0xf1..=0xf3 => self.begin_utf8(3, 0x80, 0xbf),
            0xf4 => self.begin_utf8(3, 0x80, 0x8f),
            _ => {}
        }
        StringPayloadScan::Continue
    }

    fn begin_utf8(&mut self, remaining: u8, minimum: u8, maximum: u8) {
        self.utf8_remaining = remaining;
        self.next_utf8_min = minimum;
        self.next_utf8_max = maximum;
        self.clear_tentative_raw_st();
    }

    fn clear_tentative_raw_st(&mut self) {
        self.utf8_raw_st_pending = false;
        self.utf8_raw_st_replay_len = 0;
    }
}

impl OversizedStringControlFilter {
    pub(crate) fn is_discarding(&self) -> bool {
        matches!(self.mode, OversizedStringControlMode::Discarding(_))
    }

    fn drop_unfinished_string(&mut self, bytes: &mut Vec<u8>) {
        let mode = std::mem::take(&mut self.mode);
        if let OversizedStringControlMode::Tracking(mut tracked) = mode {
            if tracked.align_with_reassembled_carry(bytes) {
                bytes.truncate(tracked.drop_start);
            }
        }
    }

    /// Consumes a new input chunk while an already-oversized control is open.
    /// The caller must pass only newly read bytes here, not a reassembled carry.
    /// A returned suffix begins immediately after the terminator/cancellation.
    pub(crate) fn consume_discarded(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        let mode = std::mem::take(&mut self.mode);
        let OversizedStringControlMode::Discarding(mut discarded) = mode else {
            self.mode = mode;
            return Some(bytes.to_vec());
        };
        let Some(terminator) = discarded.terminator(bytes, 0) else {
            self.mode = OversizedStringControlMode::Discarding(discarded);
            return None;
        };
        let mut suffix = Vec::with_capacity(terminator.replay_len + bytes.len() - terminator.end);
        suffix.extend_from_slice(&terminator.replay[..terminator.replay_len]);
        suffix.extend_from_slice(&bytes[terminator.end..]);
        Some(suffix)
    }

    /// Filters a buffer reassembled from its prior incomplete suffix plus new
    /// bytes. Tracking resumes at the previous scan offset, so one-byte reads
    /// do not repeatedly rescan the bounded carry.
    pub(crate) fn filter_reassembled(&mut self, bytes: &[u8]) -> Vec<u8> {
        debug_assert!(!self.is_discarding());
        let mut output = Vec::with_capacity(bytes.len());
        let mut copy_from = 0usize;
        let mut scan_from = 0usize;
        let mut cancellation_root = None;

        if let OversizedStringControlMode::Tracking(mut tracked) = std::mem::take(&mut self.mode) {
            if tracked.align_with_reassembled_carry(bytes) {
                match tracked.scan_more(bytes) {
                    TrackedStringScan::Complete(end) => scan_from = end,
                    TrackedStringScan::Incomplete => {
                        self.mode = OversizedStringControlMode::Tracking(tracked);
                        return bytes.to_vec();
                    }
                    TrackedStringScan::Oversized(overflow) => {
                        output.extend_from_slice(&bytes[..tracked.drop_start]);
                        let mut discarded = tracked.into_discarded();
                        let Some(terminator) = discarded.terminator(bytes, overflow) else {
                            self.mode = OversizedStringControlMode::Discarding(discarded);
                            return output;
                        };
                        output.extend_from_slice(&terminator.replay[..terminator.replay_len]);
                        copy_from = terminator.end;
                        scan_from = terminator.end;
                    }
                }
            }
        }

        while scan_from < bytes.len() {
            let Some(start) = next_stateful_control_start(bytes, scan_from) else {
                break;
            };
            let relative_start = start - scan_from;
            if relative_start > 0 {
                cancellation_root = None;
            }

            let Some((kind, payload_start)) = StringControlKind::at(bytes, start) else {
                match scan_escape_for_neutralization(bytes, start) {
                    EscapeScan::Complete(end) => {
                        cancellation_root = None;
                        scan_from = end.max(start + 1);
                    }
                    EscapeScan::Restart(restart) => {
                        cancellation_root.get_or_insert(start);
                        scan_from = restart;
                    }
                    EscapeScan::IncompleteWithinLimit | EscapeScan::IncompleteOversized => break,
                }
                continue;
            };

            let drop_start = cancellation_root.unwrap_or(start);
            let mut tracked = TrackedStringControl::new(kind, start, payload_start, drop_start);
            match tracked.scan_more(bytes) {
                TrackedStringScan::Complete(end) => {
                    cancellation_root = None;
                    scan_from = end;
                }
                TrackedStringScan::Incomplete => {
                    tracked.drop_start =
                        output.len() + tracked.drop_start.saturating_sub(copy_from);
                    tracked.start = output.len() + tracked.start.saturating_sub(copy_from);
                    tracked.scanned = output.len() + tracked.scanned.saturating_sub(copy_from);
                    tracked.carry_offset = tracked.start.saturating_sub(tracked.drop_start);
                    self.mode = OversizedStringControlMode::Tracking(tracked);
                    output.extend_from_slice(&bytes[copy_from..]);
                    return output;
                }
                TrackedStringScan::Oversized(overflow) => {
                    output.extend_from_slice(&bytes[copy_from..drop_start]);
                    let mut discarded = tracked.into_discarded();
                    let Some(terminator) = discarded.terminator(bytes, overflow) else {
                        self.mode = OversizedStringControlMode::Discarding(discarded);
                        return output;
                    };
                    output.extend_from_slice(&terminator.replay[..terminator.replay_len]);
                    copy_from = terminator.end;
                    scan_from = terminator.end;
                }
            }
        }

        output.extend_from_slice(&bytes[copy_from..]);
        output
    }
}

/// Aggregate timing and match data collected in benchmark mode.
#[derive(Clone, Debug, Default)]
pub struct BenchmarkReport {
    rules: Vec<RuleBenchmark>,
    rule_index: HashMap<RuleIdentity, usize>,
    rule_errors: Vec<RuleErrorTelemetry>,
}

#[derive(Clone, Debug, Default)]
struct RuleErrorTelemetry {
    count: usize,
    last: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuleErrorReport<'a> {
    pub(crate) description: &'a str,
    pub(crate) count: usize,
    pub(crate) last: Option<&'a str>,
}

/// Timing and match count for one rule description.
#[derive(Clone, Debug, Default)]
pub struct RuleBenchmark {
    /// Rule description from the source configuration.
    pub description: String,
    /// Total matching time spent on this rule.
    pub duration: Duration,
    /// Number of matches found for this rule.
    pub match_count: usize,
}

impl BenchmarkReport {
    /// Returns per-rule benchmark records in first-observed order.
    pub fn rules(&self) -> &[RuleBenchmark] {
        &self.rules
    }

    /// Returns the total matching time across all recorded rules.
    pub fn total_duration(&self) -> Duration {
        self.rules
            .iter()
            .map(|rule| rule.duration)
            .sum::<Duration>()
    }

    pub(crate) fn rules_with_errors(
        &self,
    ) -> impl Iterator<Item = (&RuleBenchmark, usize, Option<&str>)> {
        self.rules
            .iter()
            .zip(&self.rule_errors)
            .map(|(rule, errors)| (rule, errors.count, errors.last.as_deref()))
    }

    fn first_rule_error(&self) -> Option<RuleErrorReport<'_>> {
        let report = self
            .rules_with_errors()
            .find(|(_rule, count, _last)| *count > 0)
            .map(|(rule, count, last)| RuleErrorReport {
                description: &rule.description,
                count,
                last,
            });
        debug_assert!(report.is_none_or(|error| error.count > 0));
        report
    }

    fn record(
        &mut self,
        identity: &RuleIdentity,
        description: &str,
        duration: Duration,
        match_count: usize,
        match_error: Option<&str>,
    ) {
        if let Some(index) = self.rule_index.get(identity).copied() {
            if let Some(rule) = self.rules.get_mut(index) {
                rule.duration += duration;
                rule.match_count += match_count;
                if let Some(error) = match_error {
                    let errors = self
                        .rule_errors
                        .get_mut(index)
                        .expect("benchmark rule and error telemetry stay aligned");
                    errors.count += 1;
                    errors.last = Some(error.to_string());
                }
            }
        } else {
            let index = self.rules.len();
            self.rules.push(RuleBenchmark {
                description: description.to_string(),
                duration,
                match_count,
            });
            self.rule_errors.push(RuleErrorTelemetry {
                count: usize::from(match_error.is_some()),
                last: match_error.map(str::to_string),
            });
            self.rule_index.insert(identity.clone(), index);
        }
    }
}

#[derive(Clone, Debug)]
struct CompiledRule {
    identity: RuleIdentity,
    description: String,
    regex: Regex,
    style: CompiledStyle,
    exclusive: bool,
}

/// A rule's style with capture references resolved to group indices at
/// compile time, so matching does not look names up per match.
#[derive(Clone, Debug)]
enum CompiledStyle {
    Whole(Style),
    Captures(Vec<(usize, Style)>),
}

#[derive(Clone, Debug)]
enum Token {
    Ansi(Vec<u8>),
    Text(Vec<u8>),
}

#[derive(Clone, Debug)]
pub(crate) struct AnsiChunk {
    bytes: Vec<u8>,
    tokens: Vec<Token>,
    visible: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetMode {
    Full,
    Minimal,
}

impl Token {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Token::Ansi(bytes) | Token::Text(bytes) => bytes,
        }
    }
}

impl AnsiChunk {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        let tokens = tokenize_ansi(&bytes);
        let visible = visible_bytes(&tokens);
        Self {
            bytes,
            tokens,
            visible,
        }
    }

    pub(crate) fn from_slice(bytes: &[u8]) -> Self {
        Self::new(bytes.to_vec())
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn visible_bytes(&self) -> &[u8] {
        &self.visible
    }

    fn retokenize(&mut self) {
        self.tokens = tokenize_ansi(&self.bytes);
        self.visible = visible_bytes(&self.tokens);
    }

    fn neutralize_oversized_incomplete_escape(&mut self) {
        if neutralize_oversized_incomplete_escape(&mut self.bytes) {
            self.retokenize();
        }
    }

    fn prefix(&self, len: usize) -> Self {
        Self::new(self.bytes[..len].to_vec())
    }
}

impl Highlighter {
    /// Compiles a highlighter with true-color ANSI output.
    ///
    /// # Example
    ///
    /// ```
    /// use prismtty::{Highlighter, PrismConfig};
    ///
    /// let config = PrismConfig::from_chromaterm_yaml(r##"
    /// rules:
    ///   - description: documentation IPv4 addresses
    ///     regex: '\b192\.0\.2\.\d+\b'
    ///     color: f#00ffff
    /// "##)
    /// .expect("configuration parses");
    ///
    /// let highlighter = Highlighter::from_config(config).expect("rules compile");
    /// let output = highlighter.highlight_str("peer 192.0.2.1 up\n");
    ///
    /// assert!(output.contains("\x1b[38;2;0;255;255m192.0.2.1\x1b[0m"));
    /// ```
    pub fn from_config(config: PrismConfig) -> Result<Self, HighlightError> {
        Self::from_config_with_color_mode(config, ColorMode::TrueColor)
    }

    /// Compiles a highlighter with an explicit terminal color mode.
    pub fn from_config_with_color_mode(
        config: PrismConfig,
        color_mode: ColorMode,
    ) -> Result<Self, HighlightError> {
        let mut identities = RuleIdentityRegistry::default();
        Self::from_config_with_color_mode_and_identities(config, color_mode, &mut identities)
    }

    pub(crate) fn from_config_with_color_mode_and_identities(
        config: PrismConfig,
        color_mode: ColorMode,
        identities: &mut RuleIdentityRegistry,
    ) -> Result<Self, HighlightError> {
        let rule_count = config.rules.len();
        let pattern_bytes = config
            .rules
            .iter()
            .fold(0usize, |total, rule| total.saturating_add(rule.regex.len()));
        if rule_count > MAX_COMPILED_RULES || pattern_bytes > MAX_TOTAL_PATTERN_BYTES {
            return Err(configuration_validation_error(format!(
                "configuration contains {rule_count} rules and {pattern_bytes} pattern bytes; limits are {MAX_COMPILED_RULES} rules and {MAX_TOTAL_PATTERN_BYTES} bytes"
            )));
        }
        let mut rules = Vec::with_capacity(config.rules.len());
        let mut rule_specs = config.rules;
        rule_specs.sort_by_key(|rule| !rule.exclusive);
        for rule in rule_specs {
            rules.push(compile_rule(rule, identities)?);
        }
        Ok(Self { rules, color_mode })
    }

    /// Highlights a UTF-8 string and returns UTF-8 output with ANSI styling.
    ///
    /// # Example
    ///
    /// ```
    /// use prismtty::highlight::strip_ansi;
    /// use prismtty::{Highlighter, PrismConfig};
    ///
    /// let config = PrismConfig::from_chromaterm_yaml(r##"
    /// rules:
    ///   - description: operational state
    ///     regex: '\b(up|down)\b'
    ///     color: f#00ff00 bold
    /// "##)
    /// .expect("configuration parses");
    /// let highlighter = Highlighter::from_config(config).expect("rules compile");
    ///
    /// let output = highlighter.highlight_str("status up\n");
    ///
    /// assert_eq!(strip_ansi(output.as_bytes()), b"status up\n");
    /// assert!(output.contains("\x1b[1;38;2;0;255;0mup\x1b[0m"));
    /// ```
    pub fn highlight_str(&self, input: &str) -> String {
        self.try_highlight_str(input)
            .unwrap_or_else(|_| input.to_string())
    }

    /// Highlights a UTF-8 string and reports bounded PCRE2 runtime failures.
    pub fn try_highlight_str(&self, input: &str) -> Result<String, RuleMatchError> {
        Ok(
            String::from_utf8(self.try_highlight_bytes(input.as_bytes())?)
                .expect("highlighted UTF-8 input remains UTF-8"),
        )
    }

    /// Highlights bytes and returns bytes with ANSI styling.
    ///
    /// If a bounded regex match fails, this compatibility API returns the
    /// original bytes unchanged. Call [`Self::try_highlight_bytes`] when the
    /// caller needs the diagnostic.
    pub fn highlight_bytes(&self, input: &[u8]) -> Vec<u8> {
        self.try_highlight_bytes(input)
            .unwrap_or_else(|_| input.to_vec())
    }

    /// Highlights bytes and reports bounded PCRE2 runtime failures.
    pub fn try_highlight_bytes(&self, input: &[u8]) -> Result<Vec<u8>, RuleMatchError> {
        self.highlight_bytes_with_reset_mode(input, None, ResetMode::Full)
    }

    /// Returns visible styled spans without emitting ANSI escape sequences.
    pub fn style_spans(&self, input: &[u8]) -> Vec<StyledSpan> {
        self.try_style_spans(input).unwrap_or_default()
    }

    /// Returns visible styled spans or a bounded PCRE2 runtime failure.
    pub fn try_style_spans(&self, input: &[u8]) -> Result<Vec<StyledSpan>, RuleMatchError> {
        let tokens = tokenize_ansi(input);
        let visible = visible_bytes(&tokens);
        let styles = self.match_styles(&visible, None, None)?;
        Ok(collect_styled_spans(&visible, &styles))
    }

    fn highlight_bytes_with_reset_mode(
        &self,
        input: &[u8],
        benchmark: Option<&mut BenchmarkReport>,
        reset_mode: ResetMode,
    ) -> Result<Vec<u8>, RuleMatchError> {
        let mut native_sgr = NativeSgrState::default();
        self.highlight_bytes_with_native_sgr(input, benchmark, reset_mode, &mut native_sgr)
    }

    fn highlight_bytes_with_native_sgr(
        &self,
        input: &[u8],
        benchmark: Option<&mut BenchmarkReport>,
        reset_mode: ResetMode,
        native_sgr: &mut NativeSgrState,
    ) -> Result<Vec<u8>, RuleMatchError> {
        let tokens = tokenize_ansi(input);
        let visible = visible_bytes(&tokens);
        let styles = self.match_styles(&visible, benchmark, None)?;
        Ok(emit_highlighted(
            &tokens,
            &styles,
            self.color_mode,
            reset_mode,
            native_sgr,
        ))
    }

    fn highlight_chunk_with_native_sgr(
        &self,
        chunk: &AnsiChunk,
        benchmark: Option<&mut BenchmarkReport>,
        reset_mode: ResetMode,
        native_sgr: &mut NativeSgrState,
        disabled_rule_identities: &mut HashSet<RuleIdentity>,
    ) -> Vec<u8> {
        let styles = self
            .match_styles(
                chunk.visible_bytes(),
                benchmark,
                Some(disabled_rule_identities),
            )
            .unwrap_or_else(|_| vec![None; chunk.visible_bytes().len()]);
        emit_highlighted(
            &chunk.tokens,
            &styles,
            self.color_mode,
            reset_mode,
            native_sgr,
        )
    }

    fn highlight_chunk_with_interactive_overlay(
        &self,
        chunk: &AnsiChunk,
        benchmark: Option<&mut BenchmarkReport>,
        reset_mode: ResetMode,
        native_sgr: &mut NativeSgrState,
        overlay_style: &mut Option<Style>,
        disabled_rule_identities: &mut HashSet<RuleIdentity>,
    ) -> Vec<u8> {
        let styles = self
            .match_styles(
                chunk.visible_bytes(),
                benchmark,
                Some(disabled_rule_identities),
            )
            .unwrap_or_else(|_| vec![None; chunk.visible_bytes().len()]);
        emit_interactive_highlighted(
            &chunk.tokens,
            &styles,
            self.color_mode,
            reset_mode,
            native_sgr,
            overlay_style,
        )
    }
}

impl StreamingHighlighter {
    /// Creates a streaming highlighter for noninteractive output.
    pub fn new(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            oversized_string_controls: OversizedStringControlFilter::default(),
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: false,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: false,
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: false,
            disabled_rule_identities: HashSet::new(),
        }
    }

    /// Creates a streaming highlighter tuned for interactive PTY output.
    pub fn new_interactive(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            oversized_string_controls: OversizedStringControlFilter::default(),
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: true,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: false,
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: false,
            disabled_rule_identities: HashSet::new(),
        }
    }

    /// Creates a noninteractive streaming highlighter with benchmark collection enabled.
    pub fn new_with_benchmark(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            oversized_string_controls: OversizedStringControlFilter::default(),
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: false,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: false,
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: true,
            disabled_rule_identities: HashSet::new(),
        }
    }

    /// Creates an interactive streaming highlighter with benchmark collection enabled.
    pub fn new_interactive_with_benchmark(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            oversized_string_controls: OversizedStringControlFilter::default(),
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: true,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: false,
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: true,
            disabled_rule_identities: HashSet::new(),
        }
    }

    /// Returns benchmark data when this stream was created in benchmark mode.
    pub fn benchmark_report(&self) -> Option<&BenchmarkReport> {
        self.benchmark_enabled.then_some(&self.benchmark)
    }

    /// Returns the rule with the largest cumulative matching time, if any rule
    /// has run. Timing accumulates in every mode (not just benchmark mode) so
    /// the stream layer can name a pathologically slow user rule instead of
    /// stalling silently.
    pub(crate) fn slowest_rule(&self) -> Option<&RuleBenchmark> {
        self.benchmark
            .rules()
            .iter()
            .max_by_key(|rule| rule.duration)
    }

    /// Returns the first rule that hit a PCRE2 runtime limit or other match error.
    pub(crate) fn first_rule_error(&self) -> Option<RuleErrorReport<'_>> {
        self.benchmark.first_rule_error()
    }

    /// Replaces the inner highlighter while preserving streaming and interactive state.
    pub fn replace_highlighter(&mut self, highlighter: Highlighter) {
        self.highlighter = highlighter;
    }

    /// Installs a newly loaded configuration generation and retries its rules.
    pub(crate) fn replace_highlighter_generation(&mut self, highlighter: Highlighter) {
        self.highlighter = highlighter;
        self.disabled_rule_identities.clear();
        self.benchmark = BenchmarkReport::default();
    }

    /// Controls whether interactive highlights use full SGR resets instead of minimal resets.
    pub fn set_no_minimal_resets(&mut self, value: bool) {
        self.no_minimal_resets = value;
    }

    /// Pushes a UTF-8 chunk and returns highlighted UTF-8 output ready to display.
    ///
    /// # Example
    ///
    /// ```
    /// use prismtty::highlight::strip_ansi;
    /// use prismtty::{Highlighter, PrismConfig, StreamingHighlighter};
    ///
    /// let config = PrismConfig::from_chromaterm_yaml(r##"
    /// rules:
    ///   - description: documentation IPv4 addresses
    ///     regex: '\b192\.0\.2\.\d+\b'
    ///     color: f#00ffff
    /// "##)
    /// .expect("configuration parses");
    /// let highlighter = Highlighter::from_config(config).expect("rules compile");
    /// let mut stream = StreamingHighlighter::new(highlighter);
    ///
    /// let mut output = String::new();
    /// output.push_str(&stream.push_str("peer 192.0."));
    /// output.push_str(&stream.push_str("2.1 up\n"));
    /// output.push_str(&String::from_utf8(stream.finish()).expect("finish output is UTF-8"));
    ///
    /// assert_eq!(strip_ansi(output.as_bytes()), b"peer 192.0.2.1 up\n");
    /// assert!(output.contains("192.0.2.1"));
    /// ```
    pub fn push_str(&mut self, input: &str) -> String {
        String::from_utf8(self.push(input.as_bytes()))
            .expect("highlighted UTF-8 input remains UTF-8")
    }

    /// Pushes a byte chunk and returns highlighted output ready to display.
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let chunk = AnsiChunk::from_slice(input);
        self.push_chunk(&chunk)
    }

    pub(crate) fn push_chunk(&mut self, chunk: &AnsiChunk) -> Vec<u8> {
        let incoming = if self.oversized_string_controls.is_discarding() {
            let Some(suffix) = self
                .oversized_string_controls
                .consume_discarded(chunk.bytes())
            else {
                return Vec::new();
            };
            suffix
        } else {
            chunk.bytes().to_vec()
        };

        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(&incoming);
        let filtered = self.oversized_string_controls.filter_reassembled(&combined);
        self.push_combined_chunk(AnsiChunk::new(filtered))
    }

    fn push_combined_chunk(&mut self, mut combined: AnsiChunk) -> Vec<u8> {
        // Recompute provenance for this chunk's pending from scratch: the flag is
        // only meaningful alongside the `pending` this call produces. Each split
        // site below re-asserts it; resetting here keeps a stale `true` from an
        // earlier chunk (e.g. after a partial `flush_buffered_echo` drain or an
        // emit-all passthrough that leaves `pending` untouched) from surviving.
        self.pending_from_prompt_echo_remainder = false;
        combined.neutralize_oversized_incomplete_escape();
        let alternate_screen_chunk =
            self.alternate_screen || contains_alternate_screen_enable_tokens(&combined.tokens);

        if alternate_screen_chunk {
            self.prompt_echo_passthrough = false;
        }

        let is_bypassed = !self.alternate_screen
            && !contains_alternate_screen_enable_tokens(&combined.tokens)
            && contains_cursor_positioning_sequence_tokens(&combined.tokens);

        if self.passthrough_single_byte_chunks
            && !alternate_screen_chunk
            && !is_bypassed
            && (self.prompt_echo_passthrough
                || chunk_contains_prompt_echo_anywhere(combined.visible_bytes()))
        {
            let bytes = combined.bytes();
            if let Some(mut boundary_idx) = find_first_line_boundary(bytes) {
                let mut output = Vec::new();
                let mut start = 0;

                loop {
                    output.extend(
                        self.push_combined_chunk(AnsiChunk::from_slice(
                            &bytes[start..boundary_idx],
                        )),
                    );
                    start = boundary_idx;

                    let Some(next_boundary) = find_first_line_boundary(&bytes[start..]) else {
                        break;
                    };
                    boundary_idx = start + next_boundary;
                }

                if start < bytes.len() {
                    output.extend(self.push_combined_chunk(AnsiChunk::from_slice(&bytes[start..])));
                }
                return output;
            }
        }

        if self.passthrough_single_byte_chunks
            && !alternate_screen_chunk
            && !self.prompt_echo_passthrough
            && (contains_prompt_echo_before_lf_visible(combined.visible_bytes())
                || prompt_echo_has_active_source_sgr(combined.bytes(), &self.visible_line_tail))
        {
            let Some(prefix_len) =
                prompt_echo_line_prefix_len(combined.bytes(), &self.visible_line_tail)
            else {
                let output = self.emit_prompt_echo_passthrough(combined.bytes());
                self.observe_interactive_visible_chunk(&AnsiChunk::new(output.clone()));
                self.prompt_echo_passthrough = true;
                return output;
            };

            let prompt = combined.prefix(prefix_len);
            let mut remainder = AnsiChunk::new(combined.bytes[prefix_len..].to_vec());
            let mut output = self.emit_prompt_echo_passthrough(prompt.bytes());
            self.observe_interactive_visible_chunk(&AnsiChunk::new(output.clone()));

            let split_at = interactive_split_at_chunk(&remainder, false, self.alternate_screen);
            let processed = split_prepared_pending(&mut remainder, split_at, &mut self.pending);
            self.pending_from_prompt_echo_remainder = !self.pending.is_empty();

            output.extend(self.highlight_output_chunk(&processed));
            self.observe_interactive_visible_chunk(&processed);
            self.reset_interactive_overlay_after_prompt_tail(&mut output);
            return output;
        }

        if self.passthrough_single_byte_chunks && self.prompt_echo_passthrough {
            let Some(prefix_len) =
                prompt_echo_line_prefix_len(combined.bytes(), &self.visible_line_tail)
            else {
                let output = self.emit_prompt_echo_passthrough(combined.bytes());
                self.observe_interactive_visible_chunk(&AnsiChunk::new(output.clone()));
                return output;
            };

            let prompt = combined.prefix(prefix_len);
            let mut remainder = AnsiChunk::new(combined.bytes[prefix_len..].to_vec());
            let mut output = self.emit_prompt_echo_passthrough(prompt.bytes());
            self.observe_interactive_visible_chunk(&AnsiChunk::new(output.clone()));

            let split_at = interactive_split_at_chunk(&remainder, false, self.alternate_screen);
            let processed = split_prepared_pending(&mut remainder, split_at, &mut self.pending);
            self.pending_from_prompt_echo_remainder = !self.pending.is_empty();

            output.extend(self.highlight_output_chunk(&processed));
            self.observe_interactive_visible_chunk(&processed);
            self.reset_interactive_overlay_after_prompt_tail(&mut output);
            return output;
        }

        let split_at = if self.passthrough_single_byte_chunks {
            interactive_split_at_chunk(
                &combined,
                self.prompt_echo_passthrough,
                self.alternate_screen,
            )
        } else {
            streaming_split_at(combined.bytes())
        };
        let processed = split_prepared_pending(&mut combined, split_at, &mut self.pending);
        self.pending_from_prompt_echo_remainder = false;

        let mut output = self.highlight_output_chunk(&processed);
        self.observe_interactive_visible_chunk(&processed);
        self.reset_interactive_overlay_after_prompt_tail(&mut output);
        output
    }

    /// Flushes any buffered token and returns the stream's final bytes.
    /// Incomplete terminal controls are made inert at EOF: unfinished string
    /// controls are dropped, while other introducers are rendered as `^`.
    ///
    /// Callers must write the returned bytes: they are part of the output, not
    /// optional cleanup. For interactive streams the result may include a
    /// terminal reset (`ESC[39m`, or `ESC[0m` with minimal resets disabled)
    /// when a highlight style is still active at end of input, so subsequent
    /// program output is not left rendered in the highlighter's color. The
    /// result is empty when nothing is buffered and no style is active.
    /// Noninteractive streams never append this end-of-input reset.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut pending = std::mem::take(&mut self.pending);
        self.oversized_string_controls
            .drop_unfinished_string(&mut pending);
        neutralize_incomplete_escape_at_eof(&mut pending);
        let incomplete_utf8 = incomplete_utf8_start(&pending)
            .map(|start| pending.split_off(start))
            .unwrap_or_default();
        let pending = AnsiChunk::new(pending);
        let mut output = self.highlight_output_chunk(&pending);
        output.extend(incomplete_utf8);
        if self.passthrough_single_byte_chunks {
            self.reset_interactive_overlay(&mut output);
        }
        output
    }

    /// Flushes buffered interactive input echo that is only waiting for a token
    /// boundary, while keeping an incomplete trailing escape sequence buffered.
    ///
    /// Interactive callers should invoke this when the input source has gone idle
    /// and the buffered tail is genuine input echo (see
    /// `cli::stream::should_flush_input_echo`), so typed or pasted characters
    /// surface promptly instead of staying buffered until the next byte completes
    /// a token. It is a no-op for noninteractive streams, where speculative
    /// token buffering is required for correct highlighting.
    pub(crate) fn flush_buffered_echo(&mut self) -> Vec<u8> {
        if !self.passthrough_single_byte_chunks || self.pending.is_empty() {
            return Vec::new();
        }

        // Keep only a trailing incomplete escape buffered; its completing bytes
        // arrive in the same burst and emitting a partial escape corrupts output.
        let flush_len = flushable_pending_len(&self.pending);
        if flush_len == 0 {
            return Vec::new();
        }

        let mut flushed = std::mem::take(&mut self.pending);
        self.pending = flushed.split_off(flush_len);
        let processed = AnsiChunk::new(flushed);
        let mut output = self.highlight_output_chunk(&processed);
        self.observe_interactive_visible_chunk(&processed);
        self.reset_interactive_overlay_after_prompt_tail(&mut output);
        output
    }

    /// The raw bytes that [`Self::flush_buffered_echo`] would surface right now:
    /// the buffered trailing token, minus any incomplete trailing escape. Empty
    /// when nothing would flush. The read loop matches these against recently
    /// forwarded input (byte equality) to confirm the tail is echo before
    /// flushing it, so a speculatively-buffered *program-output* token is
    /// normally left buffered; the exception is the accepted case where program
    /// output exactly coincides with the user's recent input.
    pub(crate) fn buffered_echo(&self) -> &[u8] {
        if !self.passthrough_single_byte_chunks || self.pending.is_empty() {
            return &[];
        }
        let flush_len = flushable_pending_len(&self.pending);
        &self.pending[..flush_len]
    }

    /// True only when [`Self::buffered_echo`] holds a token that was split off a
    /// prompt-echo redraw remainder AND the current visible line is a recognized
    /// `prompt# command` line, the shape a device produces when it redraws the
    /// command line after a Tab/`?` completion without echoing the trigger byte.
    /// Program output buffered by the normal streaming split has provenance
    /// `false` and never qualifies, so the read loop's idle flush of this tail
    /// cannot fire on ordinary output. Only the child's own buffered bytes are
    /// ever emitted, so a line with no echoed command (a password prompt) holds
    /// no token and returns false.
    pub(crate) fn buffered_echo_completes_prompt_line(&self) -> bool {
        !self.buffered_echo().is_empty()
            && self.pending_from_prompt_echo_remainder
            && contains_prompt_echo_in_visible_line(&self.visible_line_tail)
    }

    fn highlight_output_chunk(&mut self, input: &AnsiChunk) -> Vec<u8> {
        if self.passthrough_single_byte_chunks {
            self.highlight_interactive_output_chunk(input)
        } else {
            self.highlight_streaming_chunk(input)
        }
    }

    fn highlight_interactive_output_chunk(&mut self, input: &AnsiChunk) -> Vec<u8> {
        if !self.alternate_screen
            && !contains_alternate_screen_enable_tokens(&input.tokens)
            && contains_cursor_positioning_sequence_tokens(&input.tokens)
        {
            return self.emit_cursor_positioning_passthrough(input);
        }

        let mut output = Vec::new();
        let mut segment_start = 0;
        let bytes = input.bytes();

        while let Some((relative_line_end, relative_boundary_end)) =
            first_terminal_text_line_boundary(&bytes[segment_start..])
        {
            let line_end = segment_start + relative_line_end;
            let boundary_end = segment_start + relative_boundary_end;
            self.emit_interactive_line_segment(&bytes[segment_start..line_end], &mut output);
            output.extend_from_slice(&bytes[line_end..boundary_end]);
            segment_start = boundary_end;
        }

        if segment_start < bytes.len() {
            self.emit_interactive_line_segment(&bytes[segment_start..], &mut output);
        }

        output
    }

    fn emit_interactive_line_segment(&mut self, segment: &[u8], output: &mut Vec<u8>) {
        output.extend(self.highlight_streaming_chunk(&AnsiChunk::from_slice(segment)));
    }

    fn highlight_streaming_chunk(&mut self, input: &AnsiChunk) -> Vec<u8> {
        if self.passthrough_single_byte_chunks
            && !self.alternate_screen
            && !contains_alternate_screen_enable_tokens(&input.tokens)
            && contains_cursor_positioning_sequence_tokens(&input.tokens)
        {
            return self.emit_cursor_positioning_passthrough(input);
        }

        let mut output = Vec::new();
        let mut highlightable = Vec::new();

        for token in &input.tokens {
            match &token {
                Token::Ansi(bytes) if is_alternate_screen_enable(bytes) => {
                    self.flush_highlightable(&mut highlightable, &mut output);
                    self.native_sgr.apply_sequence(bytes);
                    self.alternate_screen = true;
                    self.prompt_echo_passthrough = false;
                    output.extend_from_slice(bytes);
                }
                Token::Ansi(bytes) if is_alternate_screen_disable(bytes) => {
                    self.flush_highlightable(&mut highlightable, &mut output);
                    self.reset_interactive_overlay(&mut output);
                    self.native_sgr.apply_sequence(bytes);
                    self.alternate_screen = false;
                    output.extend_from_slice(bytes);
                }
                Token::Ansi(bytes)
                    if self.alternate_screen
                        && self.passthrough_single_byte_chunks
                        && is_interactive_layout_boundary_sequence(bytes) =>
                {
                    self.flush_highlightable(&mut highlightable, &mut output);
                    self.reset_interactive_overlay(&mut output);
                    self.native_sgr.apply_sequence(bytes);
                    output.extend_from_slice(bytes);
                }
                Token::Ansi(bytes)
                    if self.alternate_screen && !self.passthrough_single_byte_chunks =>
                {
                    self.native_sgr.apply_sequence(bytes);
                    output.extend_from_slice(bytes);
                }
                Token::Text(bytes)
                    if self.alternate_screen && !self.passthrough_single_byte_chunks =>
                {
                    output.extend_from_slice(bytes);
                }
                _ => highlightable.extend_from_slice(token.as_bytes()),
            }
        }

        self.flush_highlightable(&mut highlightable, &mut output);
        output
    }

    fn flush_highlightable(&mut self, input: &mut Vec<u8>, output: &mut Vec<u8>) {
        if input.is_empty() {
            return;
        }

        let chunk = AnsiChunk::new(std::mem::take(input));
        if self.passthrough_single_byte_chunks {
            let reset_mode = self.interactive_reset_mode();
            output.extend(self.highlighter.highlight_chunk_with_interactive_overlay(
                &chunk,
                Some(&mut self.benchmark),
                reset_mode,
                &mut self.native_sgr,
                &mut self.interactive_overlay,
                &mut self.disabled_rule_identities,
            ));
        } else {
            output.extend(self.highlighter.highlight_chunk_with_native_sgr(
                &chunk,
                Some(&mut self.benchmark),
                ResetMode::Full,
                &mut NativeSgrState::default(),
                &mut self.disabled_rule_identities,
            ));
        }
    }

    fn emit_prompt_echo_passthrough(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        self.reset_interactive_overlay(&mut output);
        output.extend(neutralize_prompt_echo_source_sgr(
            input,
            &self.visible_line_tail,
            self.interactive_reset_mode(),
        ));
        self.observe_native_sgr_chunk(&AnsiChunk::new(output.clone()));
        output
    }

    fn emit_cursor_positioning_passthrough(&mut self, input: &AnsiChunk) -> Vec<u8> {
        let mut output = Vec::new();
        self.reset_interactive_overlay(&mut output);
        self.observe_native_sgr_chunk(input);
        output.extend_from_slice(input.bytes());
        output
    }

    fn observe_native_sgr_chunk(&mut self, input: &AnsiChunk) {
        for token in &input.tokens {
            if let Token::Ansi(bytes) = token {
                self.native_sgr.apply_sequence(bytes);
            }
        }
    }

    fn observe_interactive_visible_chunk(&mut self, input: &AnsiChunk) {
        if !self.passthrough_single_byte_chunks {
            return;
        }

        if contains_bracketed_paste_disable_tokens(&input.tokens) {
            self.prompt_echo_passthrough = false;
        }

        let redraws_prompt_echo = redraws_prompt_echo_line_without_prompt_chunk(input);
        let preserves_prompt_echo = self.prompt_echo_passthrough
            && contains_cursor_positioning_sequence_tokens(&input.tokens)
            && has_no_printable_visible_bytes(input.visible_bytes());

        if preserves_prompt_echo {
            return;
        }

        for byte in input.visible_bytes() {
            match *byte {
                b'\r' => {
                    let command_echo_was_submitted =
                        contains_prompt_echo_in_visible_line(&self.visible_line_tail);
                    self.visible_line_tail.clear();
                    if command_echo_was_submitted {
                        self.prompt_echo_passthrough = false;
                    }
                }
                b'\n' => {
                    self.visible_line_tail.clear();
                    self.prompt_echo_passthrough = false;
                }
                byte => {
                    self.visible_line_tail.push(byte);
                    if self.visible_line_tail.len() > 512 {
                        let overflow = self.visible_line_tail.len() - 512;
                        self.visible_line_tail.drain(..overflow);
                    }
                }
            }
        }

        if redraws_prompt_echo {
            self.prompt_echo_passthrough = true;
            return;
        }

        if looks_like_prompt_tail(&self.visible_line_tail)
            || contains_prompt_echo_in_visible_line(&self.visible_line_tail)
        {
            self.prompt_echo_passthrough = true;
        }
    }

    fn reset_interactive_overlay_after_prompt_tail(&mut self, output: &mut Vec<u8>) {
        if !self.passthrough_single_byte_chunks || !looks_like_prompt_tail(&self.visible_line_tail)
        {
            return;
        }

        self.reset_interactive_overlay(output);
    }

    fn reset_interactive_overlay(&mut self, output: &mut Vec<u8>) {
        if let Some(style) = self.interactive_overlay.take() {
            output.extend(style_reset_bytes(
                &style,
                &self.native_sgr,
                self.interactive_reset_mode(),
            ));
        }
    }

    fn interactive_reset_mode(&self) -> ResetMode {
        if self.no_minimal_resets {
            ResetMode::Full
        } else {
            ResetMode::Minimal
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct VisibleByte {
    byte: u8,
    raw: usize,
}

#[derive(Clone, Copy, Debug)]
struct AnsiRange {
    start: usize,
    end: usize,
    is_sgr: bool,
}

fn neutralize_prompt_echo_source_sgr(
    input: &[u8],
    previous_visible_tail: &[u8],
    reset_mode: ResetMode,
) -> Vec<u8> {
    let (mut remove_ranges, mut reset_positions) =
        prompt_echo_source_sgr_plan(input, previous_visible_tail);
    if remove_ranges.is_empty() {
        return input.to_vec();
    }

    remove_ranges.sort_unstable();
    remove_ranges.dedup();
    reset_positions.sort_unstable();
    reset_positions.dedup();

    let mut output = Vec::with_capacity(input.len());
    let mut idx = 0usize;
    let mut remove_idx = 0usize;
    let mut reset_idx = 0usize;

    while idx < input.len() {
        while reset_positions.get(reset_idx) == Some(&idx) {
            output.extend_from_slice(prompt_echo_source_sgr_reset(reset_mode));
            reset_idx += 1;
        }

        if let Some((start, end)) = remove_ranges.get(remove_idx).copied() {
            if idx == start {
                idx = end;
                remove_idx += 1;
                continue;
            }
        }

        output.push(input[idx]);
        idx += 1;
    }

    while reset_positions.get(reset_idx) == Some(&idx) {
        output.extend_from_slice(prompt_echo_source_sgr_reset(reset_mode));
        reset_idx += 1;
    }

    output
}

fn prompt_echo_source_sgr_reset(reset_mode: ResetMode) -> &'static [u8] {
    match reset_mode {
        ResetMode::Full => b"\x1b[0m",
        ResetMode::Minimal => b"\x1b[39m",
    }
}

fn prompt_echo_has_active_source_sgr(input: &[u8], previous_visible_tail: &[u8]) -> bool {
    let (remove_ranges, _) = prompt_echo_source_sgr_plan(input, previous_visible_tail);
    !remove_ranges.is_empty()
}

fn prompt_echo_source_sgr_plan(
    input: &[u8],
    previous_visible_tail: &[u8],
) -> (Vec<(usize, usize)>, Vec<usize>) {
    let (visible, ansi_ranges) = visible_byte_map_and_ansi_ranges(input);
    if visible.is_empty() || ansi_ranges.iter().all(|range| !range.is_sgr) {
        return (Vec::new(), Vec::new());
    }

    let mut remove_ranges = Vec::new();
    let mut reset_positions = Vec::new();
    let mut line_start = 0usize;
    let mut line = Vec::new();

    while line_start <= visible.len() {
        let line_end = visible[line_start..]
            .iter()
            .position(|mapped| matches!(mapped.byte, b'\r' | b'\n'))
            .map(|idx| line_start + idx)
            .unwrap_or(visible.len());
        let first_line_continues_prompt = line_start == 0
            && (looks_like_prompt_tail(previous_visible_tail)
                || contains_prompt_echo_in_visible_line(previous_visible_tail));

        if line_start < line_end {
            line.clear();
            line.extend(
                visible[line_start..line_end]
                    .iter()
                    .map(|mapped| mapped.byte),
            );
            if let Some((sgr_start_visible, command_start_visible)) =
                prompt_echo_sgr_bounds(&line, first_line_continues_prompt)
            {
                let line_raw_end = visible
                    .get(line_end)
                    .map(|mapped| mapped.raw)
                    .unwrap_or(input.len());
                let sgr_start_raw = if first_line_continues_prompt {
                    0
                } else {
                    visible[line_start + sgr_start_visible - 1]
                        .raw
                        .saturating_add(1)
                };
                let command_start_raw = visible[line_start + command_start_visible].raw;
                let ranges_for_line = ansi_ranges
                    .iter()
                    .copied()
                    .filter(|range| range.is_sgr)
                    .filter(|range| range.start >= sgr_start_raw && range.start < line_raw_end)
                    .map(|range| (range.start, range.end))
                    .collect::<Vec<_>>();

                if prompt_echo_source_sgr_leaves_active_style(input, &ranges_for_line) {
                    remove_ranges.extend(ranges_for_line);
                    reset_positions.push(command_start_raw);
                }
            }
        }

        if line_end == visible.len() {
            break;
        }
        line_start = line_end + 1;
    }

    (remove_ranges, reset_positions)
}

fn visible_byte_map_and_ansi_ranges(input: &[u8]) -> (Vec<VisibleByte>, Vec<AnsiRange>) {
    let mut visible = Vec::new();
    let mut ansi_ranges = Vec::new();
    let mut idx = 0usize;

    while idx < input.len() {
        // Same classification order as `tokenize_ansi`: a control (including the
        // UTF-8 C1 form `C2 xx`) wins over the UTF-8 character it also encodes.
        if input[idx] == 0x1b || c1_control_at(input, idx).is_some() {
            let c1 = input[idx] != 0x1b;
            let end = if c1 {
                c1_sequence_end(input, idx)
            } else {
                ansi_sequence_end(input, idx)
            };
            ansi_ranges.push(AnsiRange {
                start: idx,
                end,
                is_sgr: is_csi_sequence(&input[idx..end])
                    && input.get(end.saturating_sub(1)) == Some(&b'm'),
            });
            idx = end;
        } else if let Some(width) = valid_utf8_char_width_at(input, idx) {
            for (offset, byte) in input[idx..idx + width].iter().copied().enumerate() {
                visible.push(VisibleByte {
                    byte,
                    raw: idx + offset,
                });
            }
            idx += width;
        } else {
            visible.push(VisibleByte {
                byte: input[idx],
                raw: idx,
            });
            idx += 1;
        }
    }

    (visible, ansi_ranges)
}

fn prompt_echo_source_sgr_leaves_active_style(input: &[u8], ranges: &[(usize, usize)]) -> bool {
    if ranges.is_empty() {
        return false;
    }

    let mut state = NativeSgrState::default();
    for (start, end) in ranges {
        state.apply_sequence(&input[*start..*end]);
    }
    state.ansi_start().is_some()
}

fn prompt_echo_sgr_bounds(
    line: &[u8],
    first_line_continues_prompt: bool,
) -> Option<(usize, usize)> {
    if first_line_continues_prompt {
        let command_start = line.iter().position(|byte| !byte.is_ascii_whitespace())?;
        return Some((0, command_start));
    }

    for prompt_end in 1..line.len() {
        if !is_prompt_tail_candidate_end(&line[..prompt_end]) {
            continue;
        }
        if !looks_like_prompt_echo_prefix(&line[..prompt_end]) {
            continue;
        }
        let Some(command_offset) = line[prompt_end..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
        else {
            continue;
        };
        return Some((prompt_end, prompt_end + command_offset));
    }

    None
}

fn looks_like_prompt_tail(line: &[u8]) -> bool {
    let trimmed = trim_ascii_whitespace_end(line);
    if trimmed.is_empty() || trimmed.len() > 180 {
        return false;
    }
    let Some(last) = trimmed.last() else {
        return false;
    };
    if trimmed.windows(2).any(|window| window == b"->") {
        return false;
    }
    if matches!(last, b'>' | b'#' | b'$' | b'%') {
        let has_prompt_body = if trimmed.len() == 1 {
            matches!(last, b'$' | b'%')
        } else {
            trimmed[..trimmed.len() - 1]
                .iter()
                .any(|byte| byte.is_ascii_alphanumeric())
        };
        return has_prompt_body
            && trimmed
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' ');
    }

    looks_like_unicode_prompt_tail(trimmed)
}

fn looks_like_unicode_prompt_tail(trimmed: &[u8]) -> bool {
    // Cheap marker check first: this runs for every candidate prompt end of a
    // visible line, so the UTF-8 decode and control scan below must only run
    // for the rare tail that actually ends in a Unicode prompt marker.
    let Some(marker) = UNICODE_PROMPT_MARKERS
        .iter()
        .copied()
        .find(|marker| trimmed.ends_with(marker.as_bytes()))
    else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(trimmed) else {
        return false;
    };
    if !text.chars().all(|ch| !ch.is_control()) {
        return false;
    }

    let body = &text[..text.len() - marker.len()];
    body.is_empty()
        || body.chars().any(|ch| ch.is_alphanumeric())
        || body.chars().any(is_prompt_decoration_char)
}

fn is_prompt_decoration_char(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '\u{2500}'..='\u{257f}' | '\u{2580}'..='\u{259f}' | '\u{e0b0}'..='\u{e0bf}'
        )
}

fn is_prompt_tail_candidate_end(bytes: &[u8]) -> bool {
    let Some(last) = bytes.last() else {
        return false;
    };
    matches!(last, b'>' | b'#' | b'$' | b'%') || looks_like_unicode_prompt_tail(bytes)
}

fn trim_ascii_whitespace_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[..end]
}

fn prompt_echo_line_prefix_len(bytes: &[u8], previous_visible_tail: &[u8]) -> Option<usize> {
    if let Some(prefix_len) = leading_prompt_terminator_len(bytes, previous_visible_tail) {
        return Some(prefix_len);
    }

    let cr_prefix = prompt_echo_cr_prefix_len(bytes, previous_visible_tail);
    let lf_prefix = prompt_echo_lf_prefix_len(bytes, previous_visible_tail);

    match (cr_prefix, lf_prefix) {
        (Some(cr), Some(lf)) if cr < lf => Some(cr),
        (_, Some(lf)) => Some(lf),
        (Some(cr), None) => Some(cr),
        (None, None) => None,
    }
}

fn leading_prompt_terminator_len(bytes: &[u8], previous_visible_tail: &[u8]) -> Option<usize> {
    if !(looks_like_prompt_tail(previous_visible_tail)
        || contains_prompt_echo_in_visible_line(previous_visible_tail))
    {
        return None;
    }

    match bytes {
        [b'\r', b'\n', ..] => Some(2),
        [b'\n', ..] => Some(1),
        _ => None,
    }
}

fn prompt_echo_cr_prefix_len(bytes: &[u8], previous_visible_tail: &[u8]) -> Option<usize> {
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'\r' {
            continue;
        }

        let before_cr_visible = strip_ansi(&bytes[..idx]);
        let line_start = before_cr_visible
            .iter()
            .rposition(|byte| matches!(*byte, b'\r' | b'\n'))
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let before_cr_line = &before_cr_visible[line_start..];

        if bytes.get(idx + 1) == Some(&b'\n') {
            if contains_prompt_echo_in_visible_line(before_cr_line) {
                return Some(idx + 2);
            }
            continue;
        }

        let after_cr = &bytes[idx + 1..];
        if after_cr.is_empty() {
            return None;
        }

        let has_echo_before_cr = before_cr_line
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
            && (contains_prompt_echo_in_visible_line(before_cr_line)
                || contains_prompt_echo_in_visible_line(previous_visible_tail)
                || looks_like_prompt_tail(previous_visible_tail));
        if !has_echo_before_cr {
            continue;
        }

        if redraws_interactive_prompt_line(after_cr) {
            continue;
        }

        return Some(idx + 1);
    }

    None
}

fn prompt_echo_lf_prefix_len(bytes: &[u8], previous_visible_tail: &[u8]) -> Option<usize> {
    let (visible, _) = visible_byte_map_and_ansi_ranges(bytes);
    let mut line_start = 0usize;
    let mut line = Vec::new();

    while line_start <= visible.len() {
        let line_end = visible[line_start..]
            .iter()
            .position(|mapped| matches!(mapped.byte, b'\r' | b'\n'))
            .map(|idx| line_start + idx)
            .unwrap_or(visible.len());
        let first_line_continues_prompt = line_start == 0
            && (looks_like_prompt_tail(previous_visible_tail)
                || contains_prompt_echo_in_visible_line(previous_visible_tail));

        let line_has_prompt_echo = if line_start < line_end {
            line.clear();
            line.extend(
                visible[line_start..line_end]
                    .iter()
                    .map(|mapped| mapped.byte),
            );
            contains_prompt_echo_in_visible_line(&line)
                || (first_line_continues_prompt
                    && line.iter().any(|byte| !byte.is_ascii_whitespace()))
        } else {
            false
        };

        if line_has_prompt_echo {
            if line_end == visible.len() {
                return None;
            }

            let separator = visible[line_end];
            if separator.byte == b'\r' && bytes.get(separator.raw + 1) == Some(&b'\n') {
                return Some(separator.raw + 2);
            }
            if separator.byte == b'\n' {
                return Some(separator.raw + 1);
            }
            return None;
        }

        if line_end == visible.len() {
            break;
        }
        line_start = line_end + 1;
    }

    None
}

fn contains_prompt_echo_before_lf_visible(visible: &[u8]) -> bool {
    let line_end = visible
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(visible.len());
    let sub = &visible[..line_end];
    let start = sub
        .iter()
        .rposition(|byte| *byte == b'\r')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    contains_prompt_echo_in_visible_line(&sub[start..])
}

fn contains_prompt_echo_in_visible_line(line: &[u8]) -> bool {
    for prompt_end in 1..line.len() {
        if !is_prompt_tail_candidate_end(&line[..prompt_end]) {
            continue;
        }
        if !looks_like_prompt_echo_prefix(&line[..prompt_end]) {
            continue;
        }
        if line[prompt_end..]
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
        {
            return true;
        }
    }

    false
}

fn looks_like_prompt_echo_prefix(line: &[u8]) -> bool {
    if !looks_like_prompt_tail(line) {
        return false;
    }

    let trimmed = trim_ascii_whitespace_end(line);
    if matches!(trimmed, b">" | b"#") {
        return false;
    }

    true
}

fn redraws_interactive_prompt_line(bytes: &[u8]) -> bool {
    let visible = strip_ansi(bytes);
    let line_end = visible
        .iter()
        .position(|byte| matches!(*byte, b'\r' | b'\n'))
        .unwrap_or(visible.len());
    let line = &visible[..line_end];
    looks_like_prompt_tail(line) || contains_prompt_echo_in_visible_line(line)
}

fn redraws_prompt_echo_line_without_prompt_chunk(input: &AnsiChunk) -> bool {
    contains_cursor_positioning_sequence_tokens(&input.tokens)
        && promptless_line_tail_visible(input.visible_bytes())
}

fn has_no_printable_visible_bytes(input: &[u8]) -> bool {
    input
        .iter()
        .all(|byte| matches!(*byte, b'\r' | b'\n' | 0x08))
}

fn promptless_line_tail_visible(visible: &[u8]) -> bool {
    let line_start = visible
        .iter()
        .rposition(|byte| matches!(*byte, b'\r' | b'\n'))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let line = trim_ascii_whitespace_end(&visible[line_start..]);

    !line.is_empty()
        && line.iter().any(|byte| !byte.is_ascii_whitespace())
        && !looks_like_prompt_tail(line)
        && !contains_prompt_echo_in_visible_line(line)
}

fn compile_rule(
    rule: RuleSpec,
    identities: &mut RuleIdentityRegistry,
) -> Result<CompiledRule, HighlightError> {
    let identity = identities.identity_for(&rule);
    let description = rule.description;
    let bounded_pattern = format!(
        "(*LIMIT_MATCH={PCRE2_MATCH_LIMIT})(*LIMIT_DEPTH={PCRE2_DEPTH_LIMIT}){}",
        rule.regex
    );
    let regex = RegexBuilder::new()
        .multi_line(true)
        .crlf(true)
        .jit_if_available(true)
        .max_jit_stack_size(Some(PCRE2_JIT_STACK_LIMIT_BYTES))
        .build(&bounded_pattern)
        .map_err(|source| HighlightError::Regex {
            description: escape_untrusted(&description),
            source,
        })?;
    let style = resolve_capture_references(&description, rule.style, &regex)?;
    Ok(CompiledRule {
        identity,
        description,
        regex,
        style,
        exclusive: rule.exclusive,
    })
}

fn configuration_validation_error(message: String) -> HighlightError {
    // HighlightError was already a public exhaustive enum in 1.x. Reuse its
    // original Regex carrier so validation can reject unsafe configurations
    // without adding a semver-breaking enum variant.
    let source = RegexBuilder::new()
        .build("(?<")
        .expect_err("the validation sentinel is intentionally invalid PCRE2");
    HighlightError::Regex {
        description: escape_untrusted(&message),
        source,
    }
}

/// Resolves every styled capture reference to a group index, rejecting
/// references to groups the pattern does not define.
fn resolve_capture_references(
    description: &str,
    style: RuleStyle,
    regex: &Regex,
) -> Result<CompiledStyle, HighlightError> {
    let capture_styles = match style {
        RuleStyle::Whole(style) => return Ok(CompiledStyle::Whole(style)),
        RuleStyle::Captures(capture_styles) => capture_styles,
    };
    let mut resolved = Vec::with_capacity(capture_styles.len());
    for (capture, style) in capture_styles {
        let index = match &capture {
            CaptureRef::Index(index) => (*index < regex.captures_len()).then_some(*index),
            CaptureRef::Name(name) => capture_group_index(regex, name),
        };
        let Some(index) = index else {
            let capture = match capture {
                CaptureRef::Index(index) => index.to_string(),
                CaptureRef::Name(name) => name,
            };
            return Err(configuration_validation_error(format!(
                "{} references capture '{}' that does not exist",
                escape_untrusted(description),
                escape_untrusted(&capture)
            )));
        };
        resolved.push((index, style));
    }
    Ok(CompiledStyle::Captures(resolved))
}

impl Highlighter {
    fn match_styles(
        &self,
        visible: &[u8],
        mut benchmark: Option<&mut BenchmarkReport>,
        mut disabled_rule_identities: Option<&mut HashSet<RuleIdentity>>,
    ) -> Result<Vec<Option<Style>>, RuleMatchError> {
        let mut styles = vec![Style::default(); visible.len()];
        let mut protected = vec![false; visible.len()];

        for rule in &self.rules {
            if disabled_rule_identities
                .as_ref()
                .is_some_and(|disabled| disabled.contains(&rule.identity))
            {
                continue;
            }
            let started = benchmark.as_ref().map(|_| Instant::now());
            let (matches, match_count, match_error) = match_rule(rule, visible);
            if let (Some(report), Some(started)) = (benchmark.as_deref_mut(), started) {
                report.record(
                    &rule.identity,
                    &rule.description,
                    started.elapsed(),
                    match_count,
                    match_error.as_deref(),
                );
            }
            // A rule that hit a runtime error still reports the matches it
            // found around the failing span, so apply them before deciding
            // what the error means for the rest of the session.
            for (start, end, style) in matches {
                if start >= end || end > styles.len() {
                    continue;
                }
                // A byte-mode regex can return offsets inside a multibyte UTF-8
                // sequence. Snap the span outward to char boundaries so the SGR
                // escapes emitted at style transitions never split a codepoint.
                let start = floor_char_boundary(visible, start);
                let end = ceil_char_boundary(visible, end);
                if protected[start..end]
                    .iter()
                    .any(|is_protected| *is_protected)
                {
                    continue;
                }
                for idx in start..end {
                    styles[idx].merge_from(&style);
                    if rule.exclusive {
                        protected[idx] = true;
                    }
                }
            }

            // The matches found around a failing span are applied above; the
            // error itself decides what happens to the rule from here on.
            if let Some(message) = match_error {
                if let Some(disabled) = disabled_rule_identities.as_deref_mut() {
                    disabled.insert(rule.identity.clone());
                    continue;
                }
                return Err(RuleMatchError {
                    description: escape_untrusted(&rule.description),
                    message: escape_untrusted(&message),
                });
            }
        }

        Ok(styles
            .into_iter()
            .map(|style| (!style.is_empty()).then_some(style))
            .collect())
    }
}

/// Largest UTF-8 char-boundary index at or below `index` in `bytes`.
fn floor_char_boundary(bytes: &[u8], mut index: usize) -> usize {
    while index > 0 && index < bytes.len() && (bytes[index] & 0xC0) == 0x80 {
        index -= 1;
    }
    index
}

/// Smallest UTF-8 char-boundary index at or above `index` in `bytes`.
fn ceil_char_boundary(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && (bytes[index] & 0xC0) == 0x80 {
        index += 1;
    }
    index
}

fn match_rule(
    rule: &CompiledRule,
    visible: &[u8],
) -> (Vec<(usize, usize, Style)>, usize, Option<String>) {
    let mut ranges = Vec::new();
    let mut match_count = 0;
    let mut match_error = None;
    let mut errors = 0;
    let mut locations = match &rule.style {
        CompiledStyle::Whole(_) => None,
        // pcre2's `captures_iter` allocates a fresh match-data block (and, with
        // a JIT stack limit set, a fresh JIT stack) for every match attempt.
        // Every rule runs on every line, so reuse one capture block per call;
        // whole-match rules use the regex's own pooled match data below.
        CompiledStyle::Captures(_) => Some(rule.regex.capture_locations()),
    };
    let mut last_end = 0;
    let mut last_match = None;

    while last_end <= visible.len() {
        let found = match &mut locations {
            Some(locations) => rule
                .regex
                .captures_read_at(locations, visible, last_end)
                .map(|matched| matched.map(|matched| (matched.start(), matched.end()))),
            None => rule
                .regex
                .find_at(visible, last_end)
                .map(|matched| matched.map(|matched| (matched.start(), matched.end()))),
        };

        let (start, end) = match found {
            Ok(Some(matched)) => matched,
            Ok(None) => break,
            Err(error) => {
                // The rule exhausted its match budget at this starting
                // position. Keep the first error for reporting, then resume
                // after the token that blew up, so one pathological word does
                // not hide every later match on the line. Stepping a single
                // byte would just re-enter the same word for each of its
                // bytes, and resuming is bounded by MAX_RULE_MATCH_ERRORS.
                if match_error.is_none() {
                    match_error = Some(error.to_string());
                }
                errors += 1;
                if errors > MAX_RULE_MATCH_ERRORS {
                    break;
                }
                match visible[last_end..]
                    .iter()
                    .position(|byte| byte.is_ascii_whitespace())
                {
                    Some(offset) => last_end += offset + 1,
                    None => break,
                }
                continue;
            }
        };

        // Same stepping as pcre2's own iterator: an empty match advances one
        // byte so the search makes progress, and an empty match directly after
        // the previous match is skipped.
        if start == end {
            last_end = end + 1;
            if Some(end) == last_match {
                continue;
            }
        } else {
            last_end = end;
        }
        last_match = Some(end);
        match_count += 1;

        match (&rule.style, &locations) {
            (CompiledStyle::Whole(style), _) => ranges.push((start, end, style.clone())),
            (CompiledStyle::Captures(capture_styles), Some(locations)) => {
                for (index, style) in capture_styles {
                    if let Some((start, end)) = locations.get(*index) {
                        ranges.push((start, end, style.clone()));
                    }
                }
            }
            (CompiledStyle::Captures(_), None) => unreachable!("capture rules keep locations"),
        }
    }

    (ranges, match_count, match_error)
}

fn capture_group_index(regex: &Regex, name: &str) -> Option<usize> {
    regex
        .capture_names()
        .iter()
        .rposition(|candidate| candidate.as_deref() == Some(name))
}

fn tokenize_ansi(input: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut text = Vec::new();
    let mut idx = 0;

    while idx < input.len() {
        if input[idx] == 0x1b || c1_control_at(input, idx).is_some() {
            if !text.is_empty() {
                tokens.push(Token::Text(std::mem::take(&mut text)));
            }
            let end = if input[idx] == 0x1b {
                ansi_sequence_end(input, idx)
            } else {
                c1_sequence_end(input, idx)
            };
            tokens.push(Token::Ansi(input[idx..end].to_vec()));
            idx = end;
        } else if let Some(width) = valid_utf8_char_width_at(input, idx) {
            text.extend_from_slice(&input[idx..idx + width]);
            idx += width;
        } else {
            text.push(input[idx]);
            idx += 1;
        }
    }

    if !text.is_empty() {
        tokens.push(Token::Text(text));
    }

    tokens
}

fn c1_sequence_end(input: &[u8], start: usize) -> usize {
    let Some((control, width)) = c1_control_at(input, start) else {
        return (start + 1).min(input.len());
    };
    match control {
        0x9b => csi_sequence_end(input, start + width),
        0x90 | 0x98 | 0x9d | 0x9e | 0x9f => c1_string_escape_end(input, start, control, width),
        _ => (start + width).min(input.len()),
    }
}

fn ansi_sequence_end(input: &[u8], start: usize) -> usize {
    if start + 1 >= input.len() {
        return input.len();
    }
    if c1_control_at(input, start + 1).is_some() {
        return start + 1;
    }

    match input[start + 1] {
        b'[' => csi_sequence_end(input, start + 2),
        b']' => osc_sequence_end(input, start + 2),
        b'P' | b'X' | b'^' | b'_' => string_sequence_end(input, start + 2),
        0x20..=0x2f => esc_intermediate_sequence_end(input, start),
        0x1b | 0x80..=0x9f => start + 1,
        _ => (start + 2).min(input.len()),
    }
}

fn esc_intermediate_sequence_end(input: &[u8], start: usize) -> usize {
    let mut idx = start + 1;
    while idx < input.len() {
        let byte = input[idx];
        if byte == 0x1b || is_c1_control(byte) {
            return idx;
        }
        idx += 1;
        if matches!(byte, 0x18 | 0x1a) || (0x30..=0x7e).contains(&byte) {
            return idx;
        }
        if !(byte <= 0x1f || (0x20..=0x2f).contains(&byte) || byte == 0x7f) {
            return idx;
        }
    }
    idx
}

fn csi_sequence_end(input: &[u8], mut idx: usize) -> usize {
    while idx < input.len() {
        if input[idx] == 0x1b || c1_control_at(input, idx).is_some() {
            break;
        }
        if let Some(width) = valid_utf8_char_width_at(input, idx) {
            idx += width;
            continue;
        }
        let byte = input[idx];
        idx += 1;
        if matches!(byte, 0x18 | 0x1a) || (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    idx
}

fn osc_sequence_end(input: &[u8], mut idx: usize) -> usize {
    string_terminator_end(input, &mut idx, true).unwrap_or(input.len())
}

fn c1_string_escape_end(input: &[u8], start: usize, control: u8, width: usize) -> usize {
    match control {
        0x9d => osc_sequence_end(input, start + width),
        0x90 | 0x98 | 0x9e | 0x9f => string_sequence_end(input, start + width),
        _ => (start + width).min(input.len()),
    }
}

fn string_sequence_end(input: &[u8], mut idx: usize) -> usize {
    string_terminator_end(input, &mut idx, false).unwrap_or(input.len())
}

fn string_terminator_end(input: &[u8], idx: &mut usize, allows_bel: bool) -> Option<usize> {
    while *idx < input.len() {
        if c1_control_at(input, *idx) == Some((0x9c, 2)) {
            return Some(*idx + 2);
        }
        if let Some(width) = valid_utf8_char_width_at(input, *idx) {
            *idx += width;
            continue;
        }
        if allows_bel && input[*idx] == 0x07 {
            return Some(*idx + 1);
        }
        if matches!(input[*idx], 0x18 | 0x1a) {
            return Some(*idx + 1);
        }
        if input[*idx] == 0x9c && !byte_is_inside_utf8_codepoint_or_prefix(input, *idx) {
            return Some(*idx + 1);
        }
        if input[*idx] == 0x1b && *idx + 1 < input.len() && input[*idx + 1] == b'\\' {
            return Some(*idx + 2);
        }
        *idx += 1;
    }
    None
}

fn is_alternate_screen_enable(bytes: &[u8]) -> bool {
    alternate_screen_command(bytes) == Some(true)
}

fn is_alternate_screen_disable(bytes: &[u8]) -> bool {
    alternate_screen_command(bytes) == Some(false)
}

fn contains_alternate_screen_enable_tokens(tokens: &[Token]) -> bool {
    tokens.iter().any(|token| match token {
        Token::Ansi(bytes) => is_alternate_screen_enable(bytes),
        Token::Text(_) => false,
    })
}

fn alternate_screen_command(bytes: &[u8]) -> Option<bool> {
    let introducer = csi_introducer_len(bytes)?;
    if bytes.get(introducer) != Some(&b'?') {
        return None;
    }
    let body_start = introducer + 1;
    let final_byte = *bytes.last()?;
    let enable = match final_byte {
        b'h' => true,
        b'l' => false,
        _ => return None,
    };
    let body = &bytes[body_start..bytes.len().saturating_sub(1)];
    let has_alternate_screen_mode = body
        .split(|byte| *byte == b';')
        .any(|mode| matches!(mode, b"47" | b"1047" | b"1049"));
    has_alternate_screen_mode.then_some(enable)
}

fn contains_cursor_positioning_sequence_tokens(tokens: &[Token]) -> bool {
    tokens.iter().any(|token| match token {
        Token::Ansi(bytes) => is_cursor_positioning_sequence(bytes),
        Token::Text(_) => false,
    })
}

fn contains_bracketed_paste_disable_tokens(tokens: &[Token]) -> bool {
    tokens.iter().any(|token| match token {
        Token::Ansi(bytes) => {
            csi_introducer_len(bytes).is_some_and(|introducer| &bytes[introducer..] == b"?2004l")
        }
        Token::Text(_) => false,
    })
}

fn is_cursor_positioning_sequence(bytes: &[u8]) -> bool {
    is_csi_sequence(bytes)
        && matches!(
            bytes.last(),
            Some(b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'G' | b'H' | b'f')
        )
}

fn is_interactive_layout_boundary_sequence(bytes: &[u8]) -> bool {
    is_csi_sequence(bytes)
        && matches!(
            bytes.last(),
            Some(
                b'A' | b'B'
                    | b'C'
                    | b'D'
                    | b'E'
                    | b'F'
                    | b'G'
                    | b'H'
                    | b'J'
                    | b'K'
                    | b'X'
                    | b'd'
                    | b'f'
            )
        )
}

/// Length of the CSI introducer at the start of `bytes`: `ESC [`, the raw C1
/// `9B`, or its UTF-8 encoding `C2 9B` (the tokenizer accepts both C1 forms,
/// so every classifier must too). `None` when `bytes` is not a CSI sequence.
fn csi_introducer_len(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(b"\x1b[") || bytes.starts_with(b"\xc2\x9b") {
        Some(2)
    } else if bytes.starts_with(b"\x9b") {
        Some(1)
    } else {
        None
    }
}

fn is_csi_sequence(bytes: &[u8]) -> bool {
    csi_introducer_len(bytes).is_some()
}

fn visible_bytes(tokens: &[Token]) -> Vec<u8> {
    let mut visible = Vec::new();
    for token in tokens {
        if let Token::Text(text) = token {
            visible.extend(text);
        }
    }
    visible
}

fn emit_highlighted(
    tokens: &[Token],
    styles: &[Option<Style>],
    color_mode: ColorMode,
    reset_mode: ResetMode,
    native_sgr: &mut NativeSgrState,
) -> Vec<u8> {
    let mut output = Vec::new();
    let mut visible_pos = 0;
    let mut active_style: Option<Style> = None;

    for token in tokens {
        match token {
            Token::Ansi(bytes) => {
                native_sgr.apply_sequence(bytes);
                output.extend_from_slice(bytes);
                if let Some(style) = &active_style {
                    output.extend_from_slice(style.ansi_start_with_mode(color_mode).as_bytes());
                }
            }
            Token::Text(bytes) => {
                for byte in bytes {
                    let wanted = styles
                        .get(visible_pos)
                        .and_then(Clone::clone)
                        .map(|style| style_for_reset_mode(style, reset_mode))
                        .filter(|style| !style.is_empty());
                    if wanted != active_style {
                        if let Some(style) = &active_style {
                            output.extend(style_reset_bytes(style, native_sgr, reset_mode));
                        }
                        if let Some(style) = &wanted {
                            output.extend_from_slice(
                                style.ansi_start_with_mode(color_mode).as_bytes(),
                            );
                        }
                        active_style = wanted;
                    }
                    output.push(*byte);
                    visible_pos += 1;
                }
            }
        }
    }

    if let Some(style) = &active_style {
        output.extend(style_reset_bytes(style, native_sgr, reset_mode));
    }

    output
}

fn emit_interactive_highlighted(
    tokens: &[Token],
    styles: &[Option<Style>],
    color_mode: ColorMode,
    reset_mode: ResetMode,
    native_sgr: &mut NativeSgrState,
    active_style: &mut Option<Style>,
) -> Vec<u8> {
    let mut output = Vec::new();
    let mut visible_pos = 0;

    for token in tokens {
        match token {
            Token::Ansi(bytes) => {
                native_sgr.apply_sequence(bytes);
                output.extend_from_slice(bytes);
                if let Some(style) = active_style {
                    output.extend_from_slice(style.ansi_start_with_mode(color_mode).as_bytes());
                }
            }
            Token::Text(bytes) => {
                for byte in bytes {
                    let wanted = styles
                        .get(visible_pos)
                        .and_then(Clone::clone)
                        .map(|style| style_for_reset_mode(style, reset_mode))
                        .filter(|style| !style.is_empty());

                    match wanted {
                        Some(style) => {
                            if active_style.as_ref() != Some(&style) {
                                output.extend_from_slice(
                                    style.ansi_start_with_mode(color_mode).as_bytes(),
                                );
                                *active_style = Some(style);
                            }
                            output.push(*byte);
                        }
                        None => {
                            if active_style.is_some() && !is_interactive_spacing(*byte) {
                                let style = active_style
                                    .take()
                                    .expect("active style checked as present");
                                output.extend(style_reset_bytes(&style, native_sgr, reset_mode));
                            }
                            output.push(*byte);
                        }
                    }

                    visible_pos += 1;
                }
            }
        }
    }

    output
}

fn is_interactive_spacing(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn style_for_reset_mode(style: Style, reset_mode: ResetMode) -> Style {
    match reset_mode {
        ResetMode::Full => style,
        ResetMode::Minimal => Style {
            bold: false,
            ..style
        },
    }
}

fn style_reset_bytes(style: &Style, native_sgr: &NativeSgrState, reset_mode: ResetMode) -> Vec<u8> {
    match reset_mode {
        ResetMode::Full => {
            let mut output = b"\x1b[0m".to_vec();
            if let Some(native) = native_sgr.ansi_start() {
                output.extend_from_slice(native.as_bytes());
            }
            output
        }
        ResetMode::Minimal => native_sgr.restore_after_interactive_style(style),
    }
}

fn collect_styled_spans(visible: &[u8], styles: &[Option<Style>]) -> Vec<StyledSpan> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut active_style = None;

    for idx in 0..visible.len() {
        let style = styles.get(idx).and_then(Clone::clone);
        if style == active_style {
            continue;
        }

        if let (Some(span_start), Some(style)) = (start, active_style.take()) {
            spans.push(StyledSpan {
                text: String::from_utf8_lossy(&visible[span_start..idx]).into_owned(),
                start: span_start,
                end: idx,
                style,
            });
        }

        start = style.as_ref().map(|_| idx);
        active_style = style;
    }

    if let (Some(span_start), Some(style)) = (start, active_style) {
        spans.push(StyledSpan {
            text: String::from_utf8_lossy(&visible[span_start..]).into_owned(),
            start: span_start,
            end: visible.len(),
            style,
        });
    }

    spans
}

#[derive(Clone, Debug, Default)]
struct NativeSgrState {
    foreground: Option<String>,
    background: Option<String>,
    underline_color: Option<String>,
    attributes: BTreeMap<u16, String>,
}

impl NativeSgrState {
    fn apply_sequence(&mut self, bytes: &[u8]) {
        let Some(body_start) = csi_introducer_len(bytes) else {
            return;
        };
        if !bytes.ends_with(b"m") {
            return;
        }

        let body = &bytes[body_start..bytes.len() - 1];
        if body.is_empty() {
            self.reset_all();
            return;
        }

        let parameters = body.split(|byte| *byte == b';').collect::<Vec<_>>();

        let mut idx = 0;
        while idx < parameters.len() {
            let parameter = parameters[idx];
            if parameter.contains(&b':') {
                self.apply_colon_parameter(parameter);
                idx += 1;
                continue;
            }

            let code = if parameter.is_empty() {
                Some(0)
            } else {
                parse_sgr_number(parameter)
            };
            match code {
                Some(38 | 48 | 58) => {
                    let target = code.expect("extended color target is present");
                    let parsed = parse_semicolon_extended_color(&parameters[idx..]);
                    if let Some(code) = parsed.code {
                        self.set_color(target, Some(code));
                    }
                    idx += parsed.consumed;
                    continue;
                }
                Some(code) => self.apply_plain_code(code),
                None => {}
            }
            idx += 1;
        }
    }

    fn apply_plain_code(&mut self, code: u16) {
        match code {
            0 => self.reset_all(),
            1 | 2 => self.set_attribute(code, code),
            3 | 20 => self.set_attribute(3, code),
            4 | 21 => self.set_attribute(4, code),
            5 | 6 => self.set_attribute(5, code),
            7..=9 => self.set_attribute(code, code),
            10 => self.remove_attributes([10]),
            11..=19 => self.set_attribute(10, code),
            22 => self.remove_attributes([1, 2]),
            23 => self.remove_attributes([3]),
            24 => self.remove_attributes([4]),
            25 => self.remove_attributes([5]),
            26 => self.set_attribute(26, code),
            27 => self.remove_attributes([7]),
            28 => self.remove_attributes([8]),
            29 => self.remove_attributes([9]),
            30..=37 | 90..=97 => self.foreground = Some(code.to_string()),
            39 => self.foreground = None,
            40..=47 | 100..=107 => self.background = Some(code.to_string()),
            49 => self.background = None,
            50 => self.remove_attributes([26]),
            51 | 52 => self.set_attribute(51, code),
            53 => self.set_attribute(53, code),
            54 => self.remove_attributes([51]),
            55 => self.remove_attributes([53]),
            59 => self.underline_color = None,
            60..=64 => self.set_attribute(60, code),
            65 => self.remove_attributes([60]),
            73 | 74 => self.set_attribute(73, code),
            75 => self.remove_attributes([73]),
            _ => {}
        }
    }

    fn apply_colon_parameter(&mut self, parameter: &[u8]) {
        let parts = parameter.split(|byte| *byte == b':').collect::<Vec<_>>();
        let Some(target) = parts.first().and_then(|part| parse_colon_number(part)) else {
            return;
        };
        match target {
            4 if parts.len() == 2 => match parts.get(1).and_then(|part| parse_colon_number(part)) {
                Some(0) => self.remove_attributes([4]),
                Some(underline @ 1..=5) => {
                    self.attributes.insert(4, format!("4:{underline}"));
                }
                _ => {}
            },
            38 | 48 | 58 => {
                if let Some(color) = canonical_colon_color(target, &parts) {
                    self.set_color(target, Some(color));
                }
            }
            _ => {}
        }
    }

    fn set_attribute(&mut self, group: u16, code: u16) {
        self.attributes.insert(group, code.to_string());
    }

    fn set_color(&mut self, target: u16, code: Option<String>) {
        match target {
            38 => self.foreground = code,
            48 => self.background = code,
            58 => self.underline_color = code,
            _ => {}
        }
    }

    fn ansi_start(&self) -> Option<String> {
        let mut parts = self.attributes.values().cloned().collect::<Vec<_>>();
        parts.sort_by_key(|parameter| {
            parameter
                .split(':')
                .next()
                .and_then(|code| code.parse::<u16>().ok())
                .unwrap_or(u16::MAX)
        });
        if let Some(foreground) = &self.foreground {
            parts.push(foreground.clone());
        }
        if let Some(background) = &self.background {
            parts.push(background.clone());
        }
        if let Some(underline_color) = &self.underline_color {
            parts.push(underline_color.clone());
        }

        (!parts.is_empty()).then(|| format!("\x1b[{}m", parts.join(";")))
    }

    fn restore_after_interactive_style(&self, style: &Style) -> Vec<u8> {
        let mut parts = Vec::new();

        if style.bold {
            self.restore_attribute_group(&mut parts, 22, &[1, 2]);
        }
        if style.italic {
            self.restore_attribute_group(&mut parts, 23, &[3]);
        }
        if style.underline {
            self.restore_attribute_group(&mut parts, 24, &[4]);
        }
        if style.blink {
            self.restore_attribute_group(&mut parts, 25, &[5]);
        }
        if style.invert {
            self.restore_attribute_group(&mut parts, 27, &[7]);
        }
        if style.strike {
            self.restore_attribute_group(&mut parts, 29, &[9]);
        }
        if style.foreground.is_some() {
            if let Some(foreground) = &self.foreground {
                parts.push(foreground.clone());
            } else {
                parts.push("39".to_string());
            }
        }
        if style.background.is_some() {
            if let Some(background) = &self.background {
                parts.push(background.clone());
            } else {
                parts.push("49".to_string());
            }
        }

        if parts.is_empty() {
            return Vec::new();
        }

        format!("\x1b[{}m", parts.join(";")).into_bytes()
    }

    fn reset_all(&mut self) {
        *self = Self::default();
    }

    fn remove_attributes(&mut self, codes: impl IntoIterator<Item = u16>) {
        for code in codes {
            self.attributes.remove(&code);
        }
    }

    fn restore_attribute_group(&self, parts: &mut Vec<String>, reset: u16, codes: &[u16]) {
        parts.push(reset.to_string());
        parts.extend(
            codes
                .iter()
                .filter_map(|code| self.attributes.get(code))
                .cloned(),
        );
    }
}

struct ExtendedColorParse {
    code: Option<String>,
    consumed: usize,
}

fn parse_semicolon_extended_color(parameters: &[&[u8]]) -> ExtendedColorParse {
    let Some(target @ (38 | 48 | 58)) = parameters.first().and_then(|part| parse_sgr_number(part))
    else {
        return ExtendedColorParse {
            code: None,
            consumed: 1,
        };
    };

    match parameters.get(1).and_then(|part| parse_sgr_number(part)) {
        Some(5) => {
            let code = parameters
                .get(2)
                .and_then(|part| parse_sgr_number(part))
                .filter(|color| *color <= 255)
                .map(|color| format!("{target};5;{color}"));
            ExtendedColorParse {
                code,
                consumed: parameters.len().min(3),
            }
        }
        Some(2) => {
            let code = match (
                parameters.get(2).and_then(|part| parse_sgr_number(part)),
                parameters.get(3).and_then(|part| parse_sgr_number(part)),
                parameters.get(4).and_then(|part| parse_sgr_number(part)),
            ) {
                (Some(red), Some(green), Some(blue))
                    if red <= 255 && green <= 255 && blue <= 255 =>
                {
                    Some(format!("{target};2;{red};{green};{blue}"))
                }
                _ => None,
            };
            ExtendedColorParse {
                code,
                consumed: parameters.len().min(5),
            }
        }
        Some(_) => ExtendedColorParse {
            code: None,
            consumed: 2,
        },
        None => ExtendedColorParse {
            code: None,
            consumed: 1,
        },
    }
}

fn parse_sgr_number(parameter: &[u8]) -> Option<u16> {
    std::str::from_utf8(parameter).ok()?.parse().ok()
}

fn parse_colon_number(part: &[u8]) -> Option<u16> {
    if part.is_empty() || !part.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    std::str::from_utf8(part).ok()?.parse().ok()
}

fn canonical_colon_color(target: u16, parts: &[&[u8]]) -> Option<String> {
    let mode = parts.get(1).and_then(|part| parse_colon_number(part))?;
    match (mode, parts.len()) {
        (5, 3) => {
            let color = parse_colon_number(parts[2]).filter(|color| *color <= 255)?;
            Some(format!("{target}:5:{color}"))
        }
        (2, 5) => {
            let (red, green, blue) = parse_colon_rgb(parts, 2)?;
            Some(format!("{target}:2:{red}:{green}:{blue}"))
        }
        (2, 6) => {
            let color_space = if parts[2].is_empty() {
                String::new()
            } else {
                parse_colon_number(parts[2])?.to_string()
            };
            let (red, green, blue) = parse_colon_rgb(parts, 3)?;
            Some(format!("{target}:2:{color_space}:{red}:{green}:{blue}"))
        }
        _ => None,
    }
}

fn parse_colon_rgb(parts: &[&[u8]], start: usize) -> Option<(u16, u16, u16)> {
    let channel = |offset: usize| {
        parts
            .get(start + offset)
            .copied()
            .and_then(parse_colon_number)
            .filter(|channel| *channel <= 255)
    };
    Some((channel(0)?, channel(1)?, channel(2)?))
}

fn find_first_line_boundary(bytes: &[u8]) -> Option<usize> {
    first_terminal_text_line_boundary(bytes)
        .map(|(_, boundary_end)| boundary_end)
        .filter(|boundary_end| *boundary_end < bytes.len())
}

fn first_terminal_text_line_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == 0x1b || c1_control_at(bytes, idx).is_some() {
            idx = if bytes[idx] == 0x1b {
                ansi_sequence_end(bytes, idx)
            } else {
                c1_sequence_end(bytes, idx)
            };
            continue;
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            idx += width;
            continue;
        }
        if matches!(bytes[idx], b'\r' | b'\n') {
            let boundary_end = if bytes[idx] == b'\r' && bytes.get(idx + 1) == Some(&b'\n') {
                idx + 2
            } else {
                idx + 1
            };
            return Some((idx, boundary_end));
        }
        idx += 1;
    }
    None
}

fn chunk_contains_prompt_echo_anywhere(visible: &[u8]) -> bool {
    let mut line_start = 0;
    while line_start < visible.len() {
        let line_end = visible[line_start..]
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .map(|idx| line_start + idx)
            .unwrap_or(visible.len());

        let line = &visible[line_start..line_end];
        if contains_prompt_echo_in_visible_line(line) {
            return true;
        }

        if line_end == visible.len() {
            break;
        }
        line_start = line_end + 1;
        if line_end + 1 < visible.len()
            && visible[line_end] == b'\r'
            && visible[line_end + 1] == b'\n'
        {
            line_start = line_end + 2;
        }
    }
    false
}

fn streaming_split_at(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }

    if let Some(escape_start) = incomplete_top_level_escape_start(bytes) {
        return escape_start;
    }

    if let Some(utf8_start) = incomplete_utf8_start(bytes) {
        return utf8_start;
    }

    if !is_token_continuation(*bytes.last().expect("checked non-empty")) {
        return bytes.len();
    }

    let mut start = bytes.len();
    while start > 0 && is_token_continuation(bytes[start - 1]) {
        start -= 1;
    }
    if let Some(ansi_end) = ansi_sequence_end_containing(bytes, start) {
        start = ansi_end;
    }

    let tail = &bytes[start..];
    if tail.len() <= 512 {
        start
    } else {
        bytes.len()
    }
}

/// If `bytes` ends partway through a multibyte UTF-8 sequence, returns the
/// index of that sequence's lead byte so the partial codepoint can be carried
/// to the next chunk instead of being emitted and split by an SGR escape.
fn incomplete_utf8_start(bytes: &[u8]) -> Option<usize> {
    let mut idx = bytes.len();
    let mut continuations = 0usize;
    while idx > 0 && (bytes[idx - 1] & 0xC0) == 0x80 {
        idx -= 1;
        continuations += 1;
    }
    let lead = *bytes.get(idx.checked_sub(1)?)?;
    let expected = match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    (continuations + 1 < expected).then_some(idx - 1)
}

pub(crate) fn is_incomplete_utf8_prefix(bytes: &[u8]) -> bool {
    incomplete_utf8_start(bytes) == Some(0)
}

/// The leading length of `pending` that is safe to surface now: up to a trailing
/// incomplete escape OR a trailing incomplete UTF-8 sequence, whichever begins
/// earlier. Their completing bytes arrive in the same burst, so emitting a
/// partial escape or codepoint would corrupt output.
fn flushable_pending_len(pending: &[u8]) -> usize {
    let escape = incomplete_top_level_escape_start(pending).unwrap_or(pending.len());
    let utf8 = incomplete_utf8_start(pending).unwrap_or(pending.len());
    escape.min(utf8)
}

fn neutralize_incomplete_escape_at_eof(bytes: &mut Vec<u8>) {
    let Some(root) = incomplete_top_level_escape_start(bytes) else {
        return;
    };

    let mut current = root;
    loop {
        let is_string_control = StringControlKind::at(bytes, current).is_some();
        match scan_escape_for_neutralization(bytes, current) {
            EscapeScan::Restart(restart) => {
                neutralize_control_start(bytes, current);
                current = restart;
            }
            EscapeScan::IncompleteWithinLimit | EscapeScan::IncompleteOversized => {
                if is_string_control {
                    bytes.truncate(root);
                } else {
                    neutralize_control_start(bytes, current);
                }
                return;
            }
            EscapeScan::Complete(_) => return,
        }
    }
}

fn ansi_sequence_end_containing(bytes: &[u8], index: usize) -> Option<usize> {
    let mut end = index.min(bytes.len());
    while end > 0 {
        let start = bytes[..end]
            .iter()
            .rposition(|byte| *byte == 0x1b || is_c1_control(*byte))?;
        if byte_is_inside_utf8_codepoint_or_prefix(bytes, start) {
            end = start;
            continue;
        }
        let control_end = if bytes[start] == 0x1b {
            ansi_sequence_end(bytes, start)
        } else {
            c1_sequence_end(bytes, start)
        };
        if start < index && index < control_end {
            return Some(control_end);
        }
        end = start;
    }
    None
}

fn split_prepared_pending(
    chunk: &mut AnsiChunk,
    split_at: usize,
    pending: &mut Vec<u8>,
) -> AnsiChunk {
    if split_at >= chunk.bytes.len() {
        pending.clear();
        return chunk.clone();
    }

    *pending = chunk.bytes[split_at..].to_vec();
    chunk.prefix(split_at)
}

fn interactive_split_at_chunk(
    chunk: &AnsiChunk,
    prompt_echo_passthrough: bool,
    alternate_screen: bool,
) -> usize {
    if alternate_screen
        || contains_alternate_screen_enable_tokens(&chunk.tokens)
        || contains_cursor_positioning_sequence_tokens(&chunk.tokens)
        || prompt_echo_passthrough
    {
        incomplete_top_level_escape_start(chunk.bytes()).unwrap_or(chunk.bytes.len())
    } else {
        streaming_split_at(chunk.bytes())
    }
}

#[cfg(test)]
pub(crate) fn incomplete_escape_start(bytes: &[u8]) -> Option<usize> {
    let mut search_from = 0;
    let mut last = None;
    while let Some(start) = next_stateful_control_start(bytes, search_from) {
        last = Some(start);
        search_from = start + 1;
    }
    last.filter(|start| escape_is_incomplete_at(bytes, *start))
}

/// Returns the start of the first top-level terminal sequence that is still
/// open at the end of `bytes`. String-control payload bytes are skipped as one
/// sequence, so an ESC or C1 byte inside an unfinished OSC/DCS/SOS/PM/APC
/// cannot be mistaken for a newer top-level control and cause the outer
/// introducer to be emitted prematurely.
fn incomplete_top_level_escape_start(bytes: &[u8]) -> Option<usize> {
    let mut idx = 0usize;
    let mut cancellation_root = None;
    while idx < bytes.len() {
        let start = next_stateful_control_start(bytes, idx)?;
        let relative_start = start - idx;
        if relative_start > 0 {
            cancellation_root = None;
        }
        match scan_escape_for_neutralization(bytes, start) {
            EscapeScan::Complete(end) => {
                cancellation_root = None;
                idx = end.max(start + 1);
            }
            EscapeScan::Restart(restart) => {
                cancellation_root.get_or_insert(start);
                idx = restart;
            }
            EscapeScan::IncompleteWithinLimit => {
                return Some(cancellation_root.unwrap_or(start));
            }
            EscapeScan::IncompleteOversized => return None,
        }
    }
    None
}

#[cfg(test)]
fn escape_is_incomplete_at(bytes: &[u8], start: usize) -> bool {
    incomplete_top_level_escape_start(&bytes[start..]) == Some(0)
}

pub(crate) fn incomplete_sanitize_start(bytes: &[u8]) -> Option<usize> {
    let escape = incomplete_top_level_escape_start(bytes).unwrap_or(bytes.len());
    let utf8 = incomplete_utf8_start(bytes).unwrap_or(bytes.len());
    let c1 = incomplete_c1_string_escape_start(bytes).unwrap_or(bytes.len());
    let split = escape.min(utf8).min(c1);
    (split < bytes.len()).then_some(split)
}

fn incomplete_c1_string_escape_start(bytes: &[u8]) -> Option<usize> {
    for start in (0..bytes.len()).rev() {
        let Some((kind, _payload_start)) = StringControlKind::at(bytes, start) else {
            continue;
        };
        if !matches!(
            kind,
            StringControlKind::C1Osc(_) | StringControlKind::C1String(_, _)
        ) {
            continue;
        }
        if matches!(
            kind,
            StringControlKind::C1Osc(1) | StringControlKind::C1String(_, 1)
        ) && byte_is_inside_utf8_codepoint_or_prefix(bytes, start)
        {
            continue;
        }
        return c1_string_escape_is_incomplete_at(bytes, start).then_some(start);
    }
    None
}

fn byte_is_inside_utf8_codepoint_or_prefix(bytes: &[u8], index: usize) -> bool {
    let search_start = index.saturating_sub(3);
    for start in search_start..index {
        if let Some(width) = valid_utf8_char_width_at(bytes, start) {
            if start + width > index {
                return true;
            }
        }

        let width = match bytes[start] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => continue,
        };
        if start + width <= index {
            continue;
        }
        if start + width <= bytes.len() {
            // A full-width but invalid sequence cannot be completed by a later
            // chunk. Its C1-valued byte is therefore a real control candidate.
            continue;
        }

        let prefix = &bytes[start..=index];
        if std::str::from_utf8(prefix).is_err() && utf8_prefix_can_be_completed(prefix, width) {
            return true;
        }
    }
    false
}

fn utf8_prefix_can_be_completed(prefix: &[u8], expected_width: usize) -> bool {
    if prefix.len() >= expected_width || prefix.len() < 2 {
        return false;
    }

    let first_continuation = prefix[1];
    let first_is_valid = match prefix[0] {
        0xe0 => (0xa0..=0xbf).contains(&first_continuation),
        0xed => (0x80..=0x9f).contains(&first_continuation),
        0xf0 => (0x90..=0xbf).contains(&first_continuation),
        0xf4 => (0x80..=0x8f).contains(&first_continuation),
        _ => (0x80..=0xbf).contains(&first_continuation),
    };
    first_is_valid && prefix[2..].iter().all(|byte| (0x80..=0xbf).contains(byte))
}

fn c1_string_escape_is_incomplete_at(bytes: &[u8], start: usize) -> bool {
    if bytes.len().saturating_sub(start) > MAX_INCOMPLETE_ESCAPE_BYTES {
        return false;
    }
    let Some((kind, mut payload)) = StringControlKind::at(bytes, start) else {
        return false;
    };
    let complete = match kind {
        StringControlKind::C1Osc(_) => string_terminator_end(bytes, &mut payload, true).is_some(),
        StringControlKind::C1String(_, _) => {
            string_terminator_end(bytes, &mut payload, false).is_some()
        }
        StringControlKind::EscOsc | StringControlKind::EscString(_) => true,
    };
    !complete
}

enum EscapeScan {
    Complete(usize),
    /// The current control was cancelled and parsing restarts at this nested
    /// ESC/C1 introducer, which must not be swallowed as parameter/final data.
    Restart(usize),
    IncompleteWithinLimit,
    IncompleteOversized,
}

fn neutralize_oversized_incomplete_escape(bytes: &mut [u8]) -> bool {
    let mut changed = false;
    let mut idx = 0usize;
    let mut cancellation_root = None;
    while idx < bytes.len() {
        let Some(start) = next_stateful_control_start(bytes, idx) else {
            break;
        };
        let relative_start = start - idx;
        if relative_start > 0 {
            cancellation_root = None;
        }
        match scan_escape_for_neutralization(bytes, start) {
            EscapeScan::Complete(end) => {
                cancellation_root = None;
                idx = end.max(start + 1);
            }
            EscapeScan::Restart(restart) => {
                cancellation_root.get_or_insert(start);
                idx = restart;
            }
            EscapeScan::IncompleteWithinLimit => break,
            EscapeScan::IncompleteOversized => {
                if let Some(root) = cancellation_root {
                    neutralize_restart_chain(bytes, root, start);
                } else {
                    neutralize_control_start(bytes, start);
                }
                changed = true;
                idx = start + 1;
                cancellation_root = None;
            }
        }
    }
    changed
}

fn neutralize_restart_chain(bytes: &mut [u8], root: usize, terminal: usize) {
    let mut current = root;
    while current < terminal {
        match scan_escape_for_neutralization(bytes, current) {
            EscapeScan::Restart(restart) if restart <= terminal => {
                neutralize_control_start(bytes, current);
                current = restart;
            }
            _ => {
                neutralize_control_start(bytes, current);
                break;
            }
        }
    }
    neutralize_control_start(bytes, terminal);
}

fn neutralize_control_start(bytes: &mut [u8], start: usize) {
    let width = c1_control_at(bytes, start)
        .map(|(_control, width)| width)
        .unwrap_or(1);
    bytes[start] = b'^';
    if width == 2 {
        bytes[start + 1] = b'?';
    }
}

fn scan_escape_for_neutralization(bytes: &[u8], start: usize) -> EscapeScan {
    if bytes[start] != 0x1b {
        let Some((control, width)) = c1_control_at(bytes, start) else {
            return EscapeScan::Complete(start + 1);
        };
        return match control {
            0x8e | 0x8f => scan_single_shift_for_neutralization(bytes, start, start + width),
            0x9b => scan_c1_csi_for_neutralization(bytes, start, start + width),
            0x9d => scan_c1_string_for_neutralization(bytes, start, start + width, true),
            0x90 | 0x98 | 0x9e | 0x9f => {
                scan_c1_string_for_neutralization(bytes, start, start + width, false)
            }
            _ => EscapeScan::Complete(start + width),
        };
    }
    if start + 1 >= bytes.len() {
        return incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES);
    }
    if c1_control_at(bytes, start + 1).is_some() {
        return EscapeScan::Restart(start + 1);
    }

    match bytes[start + 1] {
        b'[' => scan_csi_for_neutralization(bytes, start),
        b']' => scan_st_terminated_for_neutralization(bytes, start, true),
        b'P' | b'X' | b'^' | b'_' => scan_st_terminated_for_neutralization(bytes, start, false),
        b'N' | b'O' => scan_single_shift_for_neutralization(bytes, start, start + 2),
        0x20..=0x2f => scan_esc_intermediate_for_neutralization(bytes, start),
        0x1b | 0x80..=0x9f => EscapeScan::Restart(start + 1),
        0x18 | 0x1a => EscapeScan::Complete(start + 2),
        _ => EscapeScan::Complete((start + 2).min(bytes.len())),
    }
}

fn scan_esc_intermediate_for_neutralization(bytes: &[u8], start: usize) -> EscapeScan {
    let mut idx = start + 1;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if byte == 0x1b || c1_control_at(bytes, idx).is_some() {
            return EscapeScan::Restart(idx);
        }
        idx += 1;
        if matches!(byte, 0x18 | 0x1a) || (0x30..=0x7e).contains(&byte) {
            return EscapeScan::Complete(idx);
        }
        if !(byte <= 0x1f || (0x20..=0x2f).contains(&byte) || byte == 0x7f) {
            return EscapeScan::Complete(idx);
        }
    }
    incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES)
}

fn scan_single_shift_for_neutralization(bytes: &[u8], start: usize, mut idx: usize) -> EscapeScan {
    while idx < bytes.len() {
        if bytes[idx] == 0x1b || c1_control_at(bytes, idx).is_some() {
            return EscapeScan::Restart(idx);
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            return EscapeScan::Complete(idx + width);
        }
        if incomplete_utf8_start(&bytes[idx..]) == Some(0) {
            return incomplete_escape_scan_result(
                bytes,
                start,
                MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES,
            );
        }
        let byte = bytes[idx];
        idx += 1;
        if matches!(byte, 0x18 | 0x1a) {
            return EscapeScan::Complete(idx);
        }
        if byte <= 0x1f || byte == 0x7f {
            continue;
        }
        return EscapeScan::Complete(idx);
    }
    incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES)
}

fn scan_c1_csi_for_neutralization(bytes: &[u8], start: usize, mut idx: usize) -> EscapeScan {
    while idx < bytes.len() {
        if bytes[idx] == 0x1b || c1_control_at(bytes, idx).is_some() {
            return EscapeScan::Restart(idx);
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            idx += width;
            continue;
        }
        let byte = bytes[idx];
        idx += 1;
        if matches!(byte, 0x18 | 0x1a) {
            return EscapeScan::Complete(idx);
        }
        if (0x40..=0x7e).contains(&byte) {
            return EscapeScan::Complete(idx);
        }
    }
    incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES)
}

fn scan_c1_string_for_neutralization(
    bytes: &[u8],
    start: usize,
    mut idx: usize,
    allows_bel: bool,
) -> EscapeScan {
    while idx < bytes.len() {
        if idx.saturating_sub(start) > MAX_INCOMPLETE_ESCAPE_BYTES {
            return EscapeScan::IncompleteOversized;
        }
        if c1_control_at(bytes, idx) == Some((0x9c, 2)) {
            return EscapeScan::Complete(idx + 2);
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            idx += width;
            continue;
        }
        if (allows_bel && bytes[idx] == 0x07)
            || matches!(bytes[idx], 0x18 | 0x1a)
            || (bytes[idx] == 0x9c && !byte_is_inside_utf8_codepoint_or_prefix(bytes, idx))
        {
            return EscapeScan::Complete(idx + 1);
        }
        if bytes[idx] == 0x1b && idx + 1 < bytes.len() && bytes[idx + 1] == b'\\' {
            return EscapeScan::Complete(idx + 2);
        }
        idx += 1;
    }
    incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_ESCAPE_BYTES)
}

fn scan_csi_for_neutralization(bytes: &[u8], start: usize) -> EscapeScan {
    let mut idx = start + 2;
    while idx < bytes.len() {
        if bytes[idx] == 0x1b || c1_control_at(bytes, idx).is_some() {
            return EscapeScan::Restart(idx);
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            idx += width;
            continue;
        }
        let byte = bytes[idx];
        idx += 1;
        if matches!(byte, 0x18 | 0x1a) {
            return EscapeScan::Complete(idx);
        }
        if (0x40..=0x7e).contains(&byte) {
            return EscapeScan::Complete(idx);
        }
    }
    incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES)
}

fn scan_st_terminated_for_neutralization(
    bytes: &[u8],
    start: usize,
    allows_bel: bool,
) -> EscapeScan {
    let mut idx = start + 2;
    while idx < bytes.len() {
        if idx.saturating_sub(start) > MAX_INCOMPLETE_ESCAPE_BYTES {
            return EscapeScan::IncompleteOversized;
        }
        if c1_control_at(bytes, idx) == Some((0x9c, 2)) {
            return EscapeScan::Complete(idx + 2);
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            idx += width;
            continue;
        }
        if allows_bel && bytes[idx] == 0x07 {
            return EscapeScan::Complete(idx + 1);
        }
        if matches!(bytes[idx], 0x18 | 0x1a) {
            return EscapeScan::Complete(idx + 1);
        }
        if bytes[idx] == 0x9c && !byte_is_inside_utf8_codepoint_or_prefix(bytes, idx) {
            return EscapeScan::Complete(idx + 1);
        }
        if bytes[idx] == 0x1b && idx + 1 < bytes.len() && bytes[idx + 1] == b'\\' {
            return EscapeScan::Complete(idx + 2);
        }
        idx += 1;
    }
    incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_ESCAPE_BYTES)
}

fn incomplete_escape_scan_result(bytes: &[u8], start: usize, limit: usize) -> EscapeScan {
    if bytes.len().saturating_sub(start) > limit {
        EscapeScan::IncompleteOversized
    } else {
        EscapeScan::IncompleteWithinLimit
    }
}

fn is_token_continuation(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'/' | b'.')
}

/// Removes ANSI escape sequences and returns only visible bytes.
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut stripped = Vec::new();
    for token in tokenize_ansi(input) {
        if let Token::Text(text) = token {
            stripped.extend(text);
        }
    }
    stripped
}

/// Removes string-type escape sequences — OSC, DCS, SOS, PM, APC — while
/// keeping visible text, CSI/SGR, and simple escapes intact. These are the
/// 7-bit ESC and 8-bit C1 sequences a hostile child or remote device can abuse
/// (window-title spoofing, OSC 52 clipboard injection, DCS payload smuggling);
/// everything a normal TUI needs to render survives. An unterminated string
/// sequence at the end of the input is dropped rather than passed through.
pub fn strip_string_escapes(input: &[u8]) -> Vec<u8> {
    let mut sanitized = Vec::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        if input[idx] == 0x1b || c1_control_at(input, idx).is_some() {
            let root = idx;
            let mut current = idx;
            loop {
                if let Some((kind, _payload_start)) = StringControlKind::at(input, current) {
                    idx = kind.sequence_end(input, current);
                    break;
                }
                match scan_escape_for_neutralization(input, current) {
                    EscapeScan::Restart(restart) => current = restart,
                    EscapeScan::Complete(end) => {
                        sanitized.extend_from_slice(&input[root..end]);
                        idx = end;
                        break;
                    }
                    EscapeScan::IncompleteWithinLimit | EscapeScan::IncompleteOversized => {
                        sanitized.extend_from_slice(&input[root..]);
                        idx = input.len();
                        break;
                    }
                }
            }
            continue;
        }
        if let Some(width) = valid_utf8_char_width_at(input, idx) {
            sanitized.extend_from_slice(&input[idx..idx + width]);
            idx += width;
            continue;
        }
        sanitized.push(input[idx]);
        idx += 1;
    }
    sanitized
}

fn valid_utf8_char_width_at(input: &[u8], idx: usize) -> Option<usize> {
    let width = match input[idx] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let end = idx.checked_add(width)?;
    if end <= input.len() && std::str::from_utf8(&input[idx..end]).is_ok() {
        Some(width)
    } else {
        None
    }
}

fn is_c1_string_escape_start(byte: u8) -> bool {
    matches!(byte, 0x90 | 0x98 | 0x9d | 0x9e | 0x9f)
}

fn is_stateful_c1_start(byte: u8) -> bool {
    matches!(byte, 0x8e | 0x8f | 0x9b) || is_c1_string_escape_start(byte)
}

fn is_c1_control(byte: u8) -> bool {
    (0x80..=0x9f).contains(&byte)
}

/// Returns the decoded C1 control and its encoded width. Terminals may accept
/// either the legacy single-byte form or the Unicode C1 codepoint encoded as
/// UTF-8 (`C2 80` through `C2 9F`).
fn c1_control_at(input: &[u8], start: usize) -> Option<(u8, usize)> {
    let byte = *input.get(start)?;
    if is_c1_control(byte) {
        return Some((byte, 1));
    }
    if byte == 0xc2 {
        let control = *input.get(start + 1)?;
        if is_c1_control(control) {
            return Some((control, 2));
        }
    }
    None
}

fn next_stateful_control_start(input: &[u8], mut start: usize) -> Option<usize> {
    while start < input.len() {
        if input[start] == 0x1b {
            return Some(start);
        }
        if let Some((control, width)) = c1_control_at(input, start) {
            if width == 2 || !byte_is_inside_utf8_codepoint_or_prefix(input, start) {
                if is_stateful_c1_start(control) {
                    return Some(start);
                }
                start += width;
                continue;
            }
        }
        if let Some(width) = valid_utf8_char_width_at(input, start) {
            start += width;
        } else {
            start += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Highlighter, NativeSgrState, StreamingHighlighter, incomplete_escape_start};
    use crate::PrismConfig;

    // Timing must accumulate even without benchmark mode so the stream layer
    // can name a pathologically slow rule; the report itself stays gated.
    #[test]
    fn rule_timings_collect_without_benchmark_mode() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: up state\n    regex: up\n    color: f#00ff00\n",
        )
        .expect("config parses");
        let mut stream =
            StreamingHighlighter::new(Highlighter::from_config(config).expect("rules compile"));

        let _ = stream.push_str("link up\n");

        assert!(
            stream.benchmark_report().is_none(),
            "report is exposed only in benchmark mode"
        );
        let slowest = stream
            .slowest_rule()
            .expect("timings collected in every mode");
        assert_eq!(slowest.description, "up state");
    }

    #[test]
    fn streaming_constructors_default_to_minimal_resets() {
        let highlighter = Highlighter::from_config(PrismConfig::default()).expect("empty config");

        assert!(!StreamingHighlighter::new(highlighter.clone()).no_minimal_resets);
        assert!(!StreamingHighlighter::new_interactive(highlighter.clone()).no_minimal_resets);
        assert!(!StreamingHighlighter::new_with_benchmark(highlighter.clone()).no_minimal_resets);
        assert!(
            !StreamingHighlighter::new_interactive_with_benchmark(highlighter).no_minimal_resets
        );
    }

    #[test]
    fn benchmark_keeps_distinct_rules_with_the_same_description_separate() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: duplicate label
    regex: 'x'
    color: f#ff0000
  - description: duplicate label
    regex: 'y'
    color: f#00ff00
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        let mut report = super::BenchmarkReport::default();

        let _ = highlighter.highlight_bytes_with_reset_mode(
            b"xy",
            Some(&mut report),
            super::ResetMode::Full,
        );

        assert_eq!(report.rules().len(), 2);
        assert_eq!(
            report
                .rules()
                .iter()
                .map(|rule| rule.match_count)
                .collect::<Vec<_>>(),
            vec![1, 1]
        );
    }

    #[test]
    fn shared_registry_interns_identical_rules_across_highlighters() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: shared rule\n    regex: up\n    color: f#00ff00\n",
        )
        .expect("config parses");
        let mut identities = super::RuleIdentityRegistry::default();
        let first = Highlighter::from_config_with_color_mode_and_identities(
            config.clone(),
            crate::style::ColorMode::TrueColor,
            &mut identities,
        )
        .expect("first highlighter compiles");
        let second = Highlighter::from_config_with_color_mode_and_identities(
            config,
            crate::style::ColorMode::TrueColor,
            &mut identities,
        )
        .expect("second highlighter compiles");

        assert!(std::sync::Arc::ptr_eq(
            &first.rules[0].identity.spec,
            &second.rules[0].identity.spec,
        ));
    }

    #[test]
    fn benchmark_identity_cannot_collide_through_metadata_delimiters() {
        let style = crate::style::Style::parse("bold").expect("style parses");
        let config = PrismConfig {
            rules: vec![
                crate::config::RuleSpec {
                    description: "a".to_string(),
                    regex: "b\u{1f}c".to_string(),
                    style: crate::config::RuleStyle::Whole(style.clone()),
                    exclusive: false,
                },
                crate::config::RuleSpec {
                    description: "a\u{1f}b".to_string(),
                    regex: "c".to_string(),
                    style: crate::config::RuleStyle::Whole(style),
                    exclusive: false,
                },
            ],
            enabled_profiles: Vec::new(),
        };
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        let mut report = super::BenchmarkReport::default();

        let _ = highlighter.highlight_bytes_with_reset_mode(
            b"b\x1fc",
            Some(&mut report),
            super::ResetMode::Full,
        );

        assert_eq!(report.rules().len(), 2);
    }

    #[test]
    fn compile_rejects_capture_styles_that_reference_missing_groups() {
        for color in ["2: f#ff0000", "missing: f#ff0000"] {
            let config = PrismConfig::from_chromaterm_yaml(&format!(
                r##"
rules:
  - description: capture validation
    regex: '(?P<present>x)'
    color:
      {color}
"##
            ))
            .expect("config parses");

            let message = Highlighter::from_config(config)
                .expect_err("missing capture reference should fail")
                .to_string();
            assert!(message.contains("capture validation"), "{message}");
            assert!(message.contains("does not exist"), "{message}");
        }
    }

    #[test]
    fn compile_rejects_rule_and_pattern_aggregates_above_limits() {
        let rule = crate::config::RuleSpec {
            description: "bounded rule".to_string(),
            regex: "x".to_string(),
            style: crate::config::RuleStyle::Whole(
                crate::style::Style::parse("bold").expect("style parses"),
            ),
            exclusive: false,
        };
        let too_many = PrismConfig {
            rules: vec![rule.clone(); super::MAX_COMPILED_RULES + 1],
            enabled_profiles: Vec::new(),
        };
        assert!(Highlighter::from_config(too_many).is_err());

        let maximum_rule_count = PrismConfig {
            rules: vec![rule.clone(); super::MAX_COMPILED_RULES],
            enabled_profiles: Vec::new(),
        };
        assert!(Highlighter::from_config(maximum_rule_count).is_ok());

        let oversized = PrismConfig {
            rules: vec![crate::config::RuleSpec {
                regex: "x".repeat(super::MAX_TOTAL_PATTERN_BYTES + 1),
                ..rule.clone()
            }],
            enabled_profiles: Vec::new(),
        };
        assert!(Highlighter::from_config(oversized).is_err());

        let comment_payload = "x".repeat(super::MAX_TOTAL_PATTERN_BYTES - 5);
        let maximum_pattern = PrismConfig {
            rules: vec![crate::config::RuleSpec {
                regex: format!("(?#{} )", comment_payload),
                ..rule
            }],
            enabled_profiles: Vec::new(),
        };
        assert_eq!(
            maximum_pattern.rules[0].regex.len(),
            super::MAX_TOTAL_PATTERN_BYTES
        );
        assert!(Highlighter::from_config(maximum_pattern).is_ok());
    }

    #[test]
    fn match_limit_errors_are_recorded_in_rule_telemetry() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: bounded pathological rule
    regex: '^(a+)+$'
    color: f#ff0000
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");
        let mut report = super::BenchmarkReport::default();
        let mut input = vec![b'a'; 8 * 1024];
        input.push(b'!');

        let _ = highlighter.highlight_bytes_with_reset_mode(
            &input,
            Some(&mut report),
            super::ResetMode::Full,
        );

        assert_eq!(report.rules().len(), 1);
        let failure = report
            .first_rule_error()
            .expect("match error telemetry is recorded");
        assert!(failure.count > 0);
        assert!(failure.last.is_some());
        let error = highlighter
            .try_highlight_bytes(&input)
            .expect_err("fallible batch API must report the match limit");
        assert!(error.description.contains("bounded pathological rule"));
        assert_eq!(highlighter.highlight_bytes(&input), input);
    }

    #[test]
    fn streaming_quarantines_a_rule_after_its_first_match_error() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: bounded pathological rule
    regex: '^(a+)+$'
    color: f#ff0000
  - description: healthy suffix
    regex: '!'
    color: f#00ff00
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        let mut streaming = StreamingHighlighter::new_with_benchmark(highlighter);
        let mut input = vec![b'a'; 8 * 1024];
        input.extend_from_slice(b"!\n");

        let first = streaming.push(&input);
        let second = streaming.push(&input);

        assert!(
            first
                .windows(b"\x1b[38".len())
                .any(|window| window == b"\x1b[38")
        );
        assert!(
            second
                .windows(b"\x1b[38".len())
                .any(|window| window == b"\x1b[38")
        );
        let failed = streaming
            .benchmark_report()
            .expect("benchmark enabled")
            .first_rule_error()
            .expect("failed rule recorded");
        assert_eq!(failed.description, "bounded pathological rule");
        assert_eq!(failed.count, 1, "failed rule must not be retried");
    }

    #[test]
    fn profile_switches_do_not_bypass_rule_quarantine() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: bounded pathological rule\n    regex: '^(a+)+$'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let pathological = Highlighter::from_config(config.clone()).expect("rule compiles");
        let recompiled_pathological = Highlighter::from_config(config).expect("rule recompiles");
        let healthy = Highlighter::from_config(PrismConfig::default()).expect("empty config");
        let mut streaming = StreamingHighlighter::new_with_benchmark(pathological.clone());
        let mut input = vec![b'a'; 8 * 1024];
        input.extend_from_slice(b"!\n");

        let _ = streaming.push(&input);
        streaming.replace_highlighter(healthy);
        let _ = streaming.push(b"healthy\n");
        streaming.replace_highlighter(recompiled_pathological.clone());
        let _ = streaming.push(&input);

        let report = streaming.benchmark_report().expect("benchmark enabled");
        let error_count = report
            .rules_with_errors()
            .map(|(_rule, count, _last)| count)
            .sum::<usize>();
        assert_eq!(error_count, 1, "recompiled profile switch retried the rule");

        streaming.replace_highlighter_generation(recompiled_pathological);
        let _ = streaming.push(&input);
        let failed = streaming
            .benchmark_report()
            .expect("benchmark enabled")
            .first_rule_error()
            .expect("failed rule recorded");
        assert_eq!(
            failed.count, 1,
            "new generation should retry its rules once"
        );
    }

    #[test]
    fn generation_replacement_discards_stale_telemetry() {
        let mut streaming = StreamingHighlighter::new_with_benchmark(
            Highlighter::from_config(PrismConfig::default()).expect("empty config"),
        );

        for generation in 0..64 {
            let config = PrismConfig::from_chromaterm_yaml(&format!(
                "rules:\n  - description: generation {generation}\n    regex: x\n    color: f#ffffff\n"
            ))
            .expect("config parses");
            streaming.replace_highlighter_generation(
                Highlighter::from_config(config).expect("rule compiles"),
            );
            let _ = streaming.push(b"x\n");
            assert_eq!(
                streaming
                    .benchmark_report()
                    .expect("benchmark enabled")
                    .rules()
                    .len(),
                1
            );
        }
    }

    // --sanitize core: string escapes (OSC title/clipboard, DCS, SOS, PM, APC)
    // are dropped; text, CSI/SGR, and simple escapes survive byte-for-byte.
    #[test]
    fn strip_string_escapes_drops_only_string_sequences() {
        // OSC 52 clipboard write, BEL-terminated.
        assert_eq!(
            super::strip_string_escapes(b"safe\x1b]52;c;aGVsbG8=\x07 after"),
            b"safe after".to_vec()
        );
        // OSC 0 title, ST-terminated.
        assert_eq!(
            super::strip_string_escapes(b"a\x1b]0;evil title\x1b\\b"),
            b"ab".to_vec()
        );
        // DCS, SOS, PM, APC payloads.
        assert_eq!(
            super::strip_string_escapes(
                b"x\x1bPpayload\x1b\\y\x1bXs\x1b\\z\x1b^p\x1b\\w\x1b_a\x1b\\v"
            ),
            b"xyzwv".to_vec()
        );
        // 8-bit C1 string controls have the same terminal effects as their
        // ESC-prefixed forms and must be stripped by --sanitize too.
        assert_eq!(
            super::strip_string_escapes(
                b"a\x9d52;c;aGVsbG8=\x07b\x90dcs\x9cc\x98sos\x9cd\x9epm\x9ce\x9fapc\x9cf"
            ),
            b"abcdef".to_vec()
        );
        let utf8_c1_strings =
            b"a\xc2\x9d52;c;aGVsbG8=\x07b\xc2\x90dcs\xc2\x9cc\xc2\x98sos\xc2\x9cd\xc2\x9epm\xc2\x9ce\xc2\x9fapc\xc2\x9cf";
        assert_eq!(
            super::strip_string_escapes(utf8_c1_strings),
            b"abcdef".to_vec()
        );
        assert_eq!(super::strip_ansi(utf8_c1_strings), b"abcdef".to_vec());
        assert_eq!(
            super::strip_string_escapes(b"safe\xc2\x9b1\xc2\x9d52;c;SGVsbG8=\x07Jafter"),
            b"safeJafter".to_vec()
        );
        assert_eq!(
            super::strip_string_escapes(b"copyright \xc2\xa9"),
            b"copyright \xc2\xa9".to_vec()
        );
        let utf8_prompt = "prompt \u{276f} ready".as_bytes();
        assert_eq!(
            super::strip_string_escapes(utf8_prompt),
            utf8_prompt.to_vec()
        );
        // CSI/SGR and simple escapes pass through unchanged.
        assert_eq!(
            super::strip_string_escapes(b"\x1b[1;38;2;0;255;0mok\x1b[0m\x1b[2J\x1b(B"),
            b"\x1b[1;38;2;0;255;0mok\x1b[0m\x1b[2J\x1b(B".to_vec()
        );
        assert_eq!(
            super::strip_string_escapes(b"\x9b1mok\x9b0m"),
            b"\x9b1mok\x9b0m".to_vec()
        );
        // An unterminated OSC at end of input is dropped, not passed through.
        assert_eq!(
            super::strip_string_escapes(b"tail\x1b]52;c;aGVsbG8="),
            b"tail".to_vec()
        );
        assert_eq!(
            super::strip_string_escapes(b"tail\x9d52;c;aGVsbG8="),
            b"tail".to_vec()
        );
    }

    #[test]
    fn sanitize_drops_cancelled_control_prefixes_that_restart_as_strings() {
        for input in [
            b"safe\x1b[1\x1b]52;c;SGVsbG8=\x07Jafter".as_slice(),
            b"safe\x1b(\x1b]52;c;SGVsbG8=\x07Jafter".as_slice(),
            b"safe\x9b1\x9d52;c;SGVsbG8=\x07Jafter".as_slice(),
            b"safe\x1b[1\x9d52;c;SGVsbG8=\x07Jafter".as_slice(),
        ] {
            assert_eq!(
                super::strip_string_escapes(input),
                b"safeJafter",
                "nested string survived cancellation: {input:02x?}"
            );
        }

        for cancel in [0x18, 0x1a] {
            let input = [
                b"safe\x1b[1".as_slice(),
                &[cancel],
                b"\x1b]52;c;SGVsbG8=\x07Jafter".as_slice(),
            ]
            .concat();
            let expected = [b"safe\x1b[1".as_slice(), &[cancel], b"Jafter"].concat();
            assert_eq!(super::strip_string_escapes(&input), expected);
        }
    }

    #[test]
    fn sanitize_drops_unterminated_strings_reached_through_cancellation() {
        for input in [
            b"safe\x1b[1\x1b]52;c;unterminated".as_slice(),
            b"safe\x1b(\x1b]52;c;unterminated".as_slice(),
            b"safe\x9b1\x9d52;c;unterminated".as_slice(),
        ] {
            assert_eq!(super::strip_string_escapes(input), b"safe");
            assert_eq!(super::incomplete_sanitize_start(input), Some(4));
        }
    }

    #[test]
    fn sanitize_finds_nested_strings_after_oversized_cancelled_csi_prefixes() {
        for csi in [b"\x1b[".as_slice(), b"\x9b".as_slice()] {
            let mut input = b"safe".to_vec();
            input.extend_from_slice(csi);
            input.extend(std::iter::repeat_n(
                b'1',
                super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
            ));
            input.extend_from_slice(b"\x1b]52;c;SGVsbG8=\x07Jafter");

            assert_eq!(super::strip_string_escapes(&input), b"safeJafter");
        }
    }

    #[test]
    fn string_controls_end_at_can_or_sub_and_resume_visible_output() {
        for control in [
            b"\x1b]52;abc".as_slice(),
            b"\x1bPabc".as_slice(),
            b"\x1bXabc".as_slice(),
            b"\x1b^abc".as_slice(),
            b"\x1b_abc".as_slice(),
            b"\x9d52;abc".as_slice(),
            b"\x90abc".as_slice(),
            b"\x98abc".as_slice(),
            b"\x9eabc".as_slice(),
            b"\x9fabc".as_slice(),
        ] {
            for cancel in [0x18, 0x1a] {
                let input = [b"before".as_slice(), control, &[cancel], b"after\n"].concat();
                assert_eq!(super::strip_string_escapes(&input), b"beforeafter\n");

                let highlighter = Highlighter::from_config(PrismConfig::default())
                    .expect("empty config compiles");
                let mut streaming = StreamingHighlighter::new(highlighter);
                let mut output = streaming.push(&input);
                output.extend(streaming.finish());
                assert_eq!(output, input);
                assert_eq!(super::strip_ansi(&output), b"beforeafter\n");

                let mut split_streaming = StreamingHighlighter::new(
                    Highlighter::from_config(PrismConfig::default())
                        .expect("empty config compiles"),
                );
                let first = [b"before".as_slice(), control].concat();
                let second = [cancel, b'a', b'f', b't', b'e', b'r', b'\n'];
                let mut split_output = split_streaming.push(&first);
                split_output.extend(split_streaming.push(&second));
                split_output.extend(split_streaming.finish());
                assert_eq!(split_output, input);
                assert_eq!(super::strip_ansi(&split_output), b"beforeafter\n");
            }
        }
    }

    #[test]
    fn oversized_string_discard_ends_at_split_can_or_sub() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for control in [
            b"\x1b]52;".as_slice(),
            b"\x1bP".as_slice(),
            b"\x9d52;".as_slice(),
            b"\x90".as_slice(),
        ] {
            for cancel in [0x18, 0x1a] {
                let mut streaming = StreamingHighlighter::new(highlighter.clone());
                let mut first = b"before".to_vec();
                first.extend_from_slice(control);
                first.extend(std::iter::repeat_n(
                    b'x',
                    super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
                ));

                let mut output = streaming.push(&first);
                output.extend(streaming.push(&[cancel]));
                output.extend(streaming.push(b"after\n"));
                output.extend(streaming.finish());

                assert_eq!(output, b"beforeafter\n");
            }
        }
    }

    #[test]
    fn streaming_finish_drops_cancelled_prefix_with_unterminated_nested_string() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for input in [
            b"safe\x1b[1\x1b]52;c;unterminated".as_slice(),
            b"safe\x1b(\x1b]52;c;unterminated".as_slice(),
            b"safe\x9b1\x9d52;c;unterminated".as_slice(),
        ] {
            let mut streaming = StreamingHighlighter::new(highlighter.clone());
            let mut output = streaming.push(input);
            output.extend(streaming.finish());
            assert_eq!(output, b"safe", "unsafe EOF output: {output:02x?}");
        }
    }

    #[test]
    fn finish_neutralizes_every_incomplete_control_in_a_cancellation_chain() {
        for (input, expected) in [
            (b"safe\x1b[1\x1b[".as_slice(), b"safe^[1^[".as_slice()),
            (b"safe\x1b(\x1b(".as_slice(), b"safe^(^(".as_slice()),
            (b"safe\x9b1\x9b".as_slice(), b"safe^1^".as_slice()),
            (
                b"safe\x1b[1\x1b[2\x1b[".as_slice(),
                b"safe^[1^[2^[".as_slice(),
            ),
        ] {
            let highlighter =
                Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
            let mut streaming = StreamingHighlighter::new(highlighter);
            let mut output = streaming.push(input);
            output.extend(streaming.finish());

            assert_eq!(output, expected);
            assert_eq!(super::incomplete_top_level_escape_start(&output), None);
        }
    }

    #[test]
    fn oversized_nested_csi_neutralizes_the_entire_cancellation_chain() {
        for prefix in [
            b"safe\x1b[1\x1b[".as_slice(),
            b"safe\x9b1\x9b".as_slice(),
            b"safe\x1b[1\x1b[2\x1b[".as_slice(),
        ] {
            let highlighter =
                Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
            let mut streaming = StreamingHighlighter::new(highlighter);
            let mut input = prefix.to_vec();
            input.extend(std::iter::repeat_n(
                b'1',
                super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
            ));

            let mut output = streaming.push(&input);
            output.extend(streaming.finish());

            assert!(!output.contains(&0x1b), "raw ESC survived: {output:02x?}");
            assert!(
                !output.contains(&0x9b),
                "raw C1 CSI survived: {output:02x?}"
            );
            assert_eq!(super::incomplete_top_level_escape_start(&output), None);
        }
    }

    #[test]
    fn general_esc_intermediates_and_single_shifts_are_carried_and_safe_at_eof() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for partial in [
            b"\x1b ".as_slice(),
            b"\x1b$".as_slice(),
            b"\x1b!".as_slice(),
            b"\x1b&".as_slice(),
            b"\x1b$(".as_slice(),
            b"\x1bN".as_slice(),
            b"\x1bO".as_slice(),
            b"\x8e".as_slice(),
            b"\x8f".as_slice(),
        ] {
            let mut streaming = StreamingHighlighter::new(highlighter.clone());
            let mut input = b"safe".to_vec();
            input.extend_from_slice(partial);
            let mut output = streaming.push(&input);
            output.extend(streaming.finish());

            assert!(!output.contains(&0x1b), "raw ESC survived: {output:02x?}");
            assert!(
                !output.contains(&0x8e),
                "raw C1 SS2 survived: {output:02x?}"
            );
            assert!(
                !output.contains(&0x8f),
                "raw C1 SS3 survived: {output:02x?}"
            );
            assert_eq!(super::incomplete_top_level_escape_start(&output), None);
        }

        for (partial, visible) in [
            (b"\x1b ".as_slice(), b"safeafter".as_slice()),
            (b"\x1b$".as_slice(), b"safeafter".as_slice()),
            (b"\x1b$(".as_slice(), b"safeafter".as_slice()),
            (b"\x1bN".as_slice(), b"safeBafter".as_slice()),
            (b"\x1bO".as_slice(), b"safeBafter".as_slice()),
            (b"\x8e".as_slice(), b"safeBafter".as_slice()),
            (b"\x8f".as_slice(), b"safeBafter".as_slice()),
        ] {
            let mut streaming = StreamingHighlighter::new(highlighter.clone());
            let mut first = b"safe".to_vec();
            first.extend_from_slice(partial);
            let mut output = streaming.push(&first);
            output.extend(streaming.push(b"Bafter"));
            output.extend(streaming.finish());

            let mut expected = first;
            expected.extend_from_slice(b"Bafter");
            assert_eq!(output, expected);
            assert_eq!(super::strip_ansi(&output), visible);
        }
    }

    // AUDIT (1.0.8): an unterminated escape longer than the cap must be treated
    // as complete so it is flushed instead of carried across reads without
    // bound (memory-exhaustion vector from hostile device output).
    #[test]
    fn unterminated_escape_past_the_cap_is_treated_as_complete() {
        // A short unterminated CSI is still incomplete (carry it across reads).
        assert!(super::escape_is_incomplete_at(b"\x1b[1;2", 0));

        // One longer than the cap is treated as complete (flush, don't buffer).
        let mut oversized = b"\x1b[".to_vec();
        oversized.extend(std::iter::repeat_n(
            b';',
            super::MAX_INCOMPLETE_ESCAPE_BYTES,
        ));
        assert!(!super::escape_is_incomplete_at(&oversized, 0));
    }

    // Cisco Nexus ambiguous-Tab redraw (real --trace-io bytes, hostname
    // synthesized). The device redraws the command line ending in the bare token
    // "log" and idles; the trailing token is withheld in `pending`. The live read
    // loop must surface it on idle. This asserts the provenance signal the read
    // loop keys on and that the flush emits exactly the tail.
    #[test]
    fn interactive_buffered_tab_completion_tail_is_flagged_as_prompt_echo() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push_str("LAB-N9K-CORE-01# ");
        assert_eq!(streaming.push_str("sh lo"), "sh lo");

        let redraw = "\x1b[23D\x1b[J\rLAB-N9K-CORE-01# sh log\r\r\nlogging   login     \r\r\n\x1b[J\rLAB-N9K-CORE-01# sh log";
        let out = streaming.push_str(redraw);
        let visible = super::strip_ansi(out.as_bytes());

        // The trailing token is withheld (the live bug): only "...# sh " is emitted.
        assert!(
            visible.ends_with(b"LAB-N9K-CORE-01# sh "),
            "tail should be withheld: {out:?}"
        );
        assert_eq!(streaming.buffered_echo(), b"log");

        // New signal: the buffered tail provably came from a prompt-echo redraw.
        assert!(streaming.buffered_echo_completes_prompt_line());

        // The idle flush surfaces exactly the tail.
        assert_eq!(super::strip_ansi(&streaming.flush_buffered_echo()), b"log");
        assert!(streaming.finish().is_empty());
    }

    #[test]
    fn raw_c1_prompt_source_sgr_is_neutralized_before_command_echo() {
        let input = b"router# \x9b31mshow up";
        let output =
            super::neutralize_prompt_echo_source_sgr(input, b"", super::ResetMode::Minimal);

        assert!(
            !output.contains(&0x9b),
            "source C1 SGR survived: {output:02x?}"
        );
        assert!(
            output
                .windows(b"\x1b[39m".len())
                .any(|window| window == b"\x1b[39m"),
            "command boundary reset missing: {output:02x?}"
        );
        assert_eq!(super::strip_ansi(&output), b"router# show up");
    }

    #[test]
    fn split_prompt_neutralizes_leading_source_sgr_before_command_echo() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for source_sgr in [
            b"\x1b[31m".as_slice(),
            b"\x9b31m".as_slice(),
            b"\xc2\x9b31m".as_slice(),
        ] {
            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            assert_eq!(streaming.push(b"FGT01 # "), b"FGT01 # ");

            let mut chunk = source_sgr.to_vec();
            chunk.extend_from_slice(b"show");
            let mut output = streaming.push(&chunk);
            output.extend(streaming.finish());

            assert!(
                !output
                    .windows(source_sgr.len())
                    .any(|window| window == source_sgr),
                "leading source SGR survived: {output:02x?}"
            );
            assert_eq!(super::strip_ansi(&output), b"show");
            assert!(
                output
                    .windows(b"\x1b[39mshow".len())
                    .any(|window| window == b"\x1b[39mshow"),
                "command reset missing: {output:02x?}"
            );
        }
    }

    #[test]
    fn interactive_line_splitting_ignores_newlines_inside_string_controls() {
        let highlighter = Highlighter::from_config(PrismConfig::default()).expect("empty config");

        for input in [
            b"\x1b]0;hidden\n\x1b[2J\x07user@host$ cmd\n".as_slice(),
            b"\x1bPignored\r\n\x1b[31m\x1b\\user@host$ cmd\n".as_slice(),
            b"\x9d0;hidden\n\x9b2J\x9cuser@host$ cmd\n".as_slice(),
        ] {
            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut output = streaming.push(input);
            output.extend(streaming.finish());

            assert_eq!(
                output, input,
                "string control was split or rewritten: {output:02x?}"
            );
            assert_eq!(super::strip_ansi(&output), b"user@host$ cmd\n");
        }
    }

    #[test]
    fn buffered_echo_withholds_incomplete_utf8_tail() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        // Interactive chunk ending mid-codepoint (first two bytes of '❯' U+276F =
        // E2 9D AF). The partial multibyte must not be surfaced by the idle flush.
        let _ = streaming.push(b"echo \xe2\x9d");
        assert!(
            streaming.buffered_echo().is_empty(),
            "incomplete UTF-8 tail must stay buffered: {:?}",
            streaming.buffered_echo()
        );
        assert!(streaming.flush_buffered_echo().is_empty());

        // Completing the codepoint surfaces the whole character.
        let out = streaming.push(b"\xafx");
        assert!(
            super::strip_ansi(&out)
                .windows(3)
                .any(|w| w == "❯".as_bytes()),
            "completed codepoint should surface intact: {out:?}"
        );
    }

    // Provenance guard: program output must never trigger the prompt-echo idle
    // flush.
    #[test]
    fn program_output_does_not_qualify_for_prompt_echo_flush() {
        // Plain bulk output split mid-token: buffered via the normal streaming
        // branch, so provenance is false and it is not eligible.
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push(b"interface Vlan11");
        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_prompt_line(),
            "bulk program output must not qualify as a prompt-echo redraw tail"
        );

        // Output whose line resembles `word# text` must likewise never trigger
        // the flush (it is emitted as prompt-echo passthrough, not stranded as a
        // buffered tail with prompt-echo provenance).
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push(b"status# Vlan11");
        assert!(
            !streaming.buffered_echo_completes_prompt_line(),
            "prompt-shaped program output must not trigger the prompt-echo flush"
        );
    }

    #[test]
    fn password_prompt_after_command_does_not_qualify_for_prompt_echo_flush() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"host# ssh admin@192.0.2.1\r\n");
        let _ = streaming.push(b"Password: ");
        // A non-echoed password produces no buffered token, and `Password: ` is
        // not a `prompt# command` line, so the prompt-echo flush cannot fire.
        assert!(streaming.buffered_echo().is_empty());
        assert!(!streaming.buffered_echo_completes_prompt_line());
    }

    #[test]
    fn more_paging_prompt_does_not_qualify_for_prompt_echo_flush() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"a line of output\r\n");
        let _ = streaming.push(b"\x1b[7m--More--\x1b[27m");
        assert!(
            !streaming.buffered_echo_completes_prompt_line(),
            "the --More-- pager prompt is not a prompt-echo command line"
        );
    }

    #[test]
    fn prompt_echo_redraw_split_across_writes_loses_no_text() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push_str("LAB-N9K-CORE-01# ");
        let _ = streaming.push_str("sh lo");

        // The same redraw delivered as two fragmented writes must not lose the
        // command tail; the idle flush plus finish() surface anything buffered.
        let full = "\x1b[23D\x1b[J\rLAB-N9K-CORE-01# sh log\r\r\nlogging   login     \r\r\n\x1b[J\rLAB-N9K-CORE-01# sh log";
        let mid = full.len() / 2;
        let mut visible = super::strip_ansi(streaming.push_str(&full[..mid]).as_bytes());
        visible.extend(super::strip_ansi(
            streaming.push_str(&full[mid..]).as_bytes(),
        ));
        visible.extend(super::strip_ansi(&streaming.flush_buffered_echo()));
        visible.extend(super::strip_ansi(&streaming.finish()));
        let needle = b"LAB-N9K-CORE-01# sh log";
        assert!(
            visible.windows(needle.len()).any(|w| w == needle),
            "command tail lost across a split redraw: {visible:?}"
        );
    }

    #[test]
    fn local_shell_dollar_prompt_completion_tail_qualifies_for_prompt_echo_flush() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        // A local shell `$` prompt redrawing the command after Tab, ending in a
        // bare token, must qualify exactly like a network device (vendor-agnostic).
        let _ = streaming.push_str("user@host:~$ ");
        let _ = streaming.push_str("git lo");
        let redraw = "\x1b[20D\x1b[J\ruser@host:~$ git log\r\r\nlog       logout    \r\r\n\x1b[J\ruser@host:~$ git log";
        let _ = streaming.push_str(redraw);

        assert_eq!(streaming.buffered_echo(), b"log");
        assert!(
            streaming.buffered_echo_completes_prompt_line(),
            "a $-prompt completion redraw tail must qualify"
        );
        assert_eq!(super::strip_ansi(&streaming.flush_buffered_echo()), b"log");
    }

    #[test]
    fn streaming_does_not_buffer_unterminated_escape_without_bound() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new(highlighter);

        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(
            b';',
            super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
        ));
        let out = streaming.push(&input);

        assert!(
            out.len() >= super::MAX_INCOMPLETE_ESCAPE_BYTES,
            "an unterminated escape past the cap must be flushed, not buffered (got {} bytes)",
            out.len()
        );
    }

    #[test]
    fn oversized_string_controls_remain_discarded_until_their_real_terminator() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for interactive in [false, true] {
            for (introducer, terminator) in [
                (b"\x1b]".as_slice(), b"\x07".as_slice()),
                (b"\x9d".as_slice(), b"\x07".as_slice()),
                (b"\x1bP".as_slice(), b"\x1b\\".as_slice()),
                (b"\x1bX".as_slice(), b"\x1b\\".as_slice()),
                (b"\x1b^".as_slice(), b"\x1b\\".as_slice()),
                (b"\x1b_".as_slice(), b"\x1b\\".as_slice()),
                (b"\x90".as_slice(), b"\x9c".as_slice()),
                (b"\x98".as_slice(), b"\x9c".as_slice()),
                (b"\x9e".as_slice(), b"\x9c".as_slice()),
                (b"\x9f".as_slice(), b"\x9c".as_slice()),
            ] {
                let mut streaming = if interactive {
                    StreamingHighlighter::new_interactive(highlighter.clone())
                } else {
                    StreamingHighlighter::new(highlighter.clone())
                };
                let mut first = b"safe".to_vec();
                first.extend_from_slice(introducer);
                first.extend(std::iter::repeat_n(
                    b'x',
                    super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
                ));

                let mut output = streaming.push(&first);
                output.extend(streaming.push(b"\x1b[2J\x9b2J"));
                if terminator == b"\x1b\\" {
                    output.extend(streaming.push(b"\x1b"));
                    output.extend(streaming.push(b"\\after"));
                } else {
                    let mut final_chunk = terminator.to_vec();
                    final_chunk.extend_from_slice(b"after");
                    output.extend(streaming.push(&final_chunk));
                }
                output.extend(streaming.finish());

                assert_eq!(
                    output, b"safeafter",
                    "payload escaped from {introducer:02x?}: {output:02x?}"
                );
                assert!(
                    !output.windows(4).any(|window| window == b"\x1b[2J"),
                    "nested CSI became active for {introducer:02x?}: {output:02x?}"
                );
            }
        }
    }

    #[test]
    fn oversized_string_discard_ignores_non_c1_utf8_continuations() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new(highlighter);
        let mut first = b"safe\x1bP".to_vec();
        first.extend(std::iter::repeat_n(
            b'x',
            super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
        ));

        let mut output = streaming.push(&first);
        output.extend(streaming.push(b"\xc2"));
        output.extend(streaming.push(b"\xa9"));
        output.extend(streaming.push(b"\xe2"));
        output.extend(streaming.push(b"\x9d"));
        output.extend(streaming.push(b"\x9c\x1b[2J"));
        output.extend(streaming.push(b"\x1b"));
        output.extend(streaming.push(b"\\after"));
        output.extend(streaming.finish());

        assert_eq!(output, b"safeafter");
        assert!(!output.windows(4).any(|window| window == b"\x1b[2J"));
    }

    #[test]
    fn oversized_string_discard_recovers_raw_st_after_malformed_utf8() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for introducer in [b"\x1b]".as_slice(), b"\x1bP".as_slice()] {
            let mut streaming = StreamingHighlighter::new(highlighter.clone());
            let mut first = b"safe".to_vec();
            first.extend_from_slice(introducer);
            first.extend(std::iter::repeat_n(
                b'x',
                super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
            ));

            let mut output = streaming.push(&first);
            output.extend(streaming.push(b"\xe2"));
            output.extend(streaming.push(b"\x9c"));
            output.extend(streaming.push(b"after"));
            output.extend(streaming.finish());

            assert_eq!(
                output, b"safeafter",
                "malformed UTF-8 hid the suffix after {introducer:02x?}"
            );
        }
    }

    #[test]
    fn oversized_string_discard_replays_utf8_suffix_after_cross_chunk_raw_st() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new(highlighter);
        let mut first = b"safe\x1bP".to_vec();
        first.extend(std::iter::repeat_n(
            b'x',
            super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
        ));

        let mut output = streaming.push(&first);
        output.extend(streaming.push(b"\xf0\x9c\x80"));
        output.extend(streaming.push(b"after"));
        output.extend(streaming.finish());

        assert_eq!(output, b"safe\x80after");
    }

    #[test]
    fn fragmented_incomplete_csi_is_neutralized_at_the_non_string_work_cap() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new(highlighter);
        let mut output = streaming.push(b"\x1b[");

        for _ in 0..1_025 {
            output.extend(streaming.push(b";"));
        }

        assert!(
            !output.is_empty(),
            "fragmented CSI remained buffered beyond the intended work cap"
        );
        assert!(!output.contains(&0x1b), "raw ESC was emitted after the cap");
    }

    #[test]
    fn string_tracker_rebases_after_emitting_a_visible_prefix() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new(highlighter);

        let mut output = streaming.push(b"safe\x1b]ab\x1b]");
        output.extend(streaming.push(b"\x07after"));
        output.extend(streaming.finish());

        assert_eq!(output, b"safe\x1b]ab\x1b]\x07after");
    }

    #[test]
    fn interactive_minimal_reset_keeps_text_attributes_and_backgrounds() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: rich state
    regex: '\bup\b'
    color: f#00ff00 b#0000ff bold underline
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("highlighter builds");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        streaming.set_no_minimal_resets(false);

        let output = String::from_utf8(streaming.push(b"up down\n")).expect("output is utf8");

        assert!(output.contains("\x1b[4;38;2;0;255;0;48;2;0;0;255mup"));
        assert!(output.contains("\x1b[24;39;49m"));
    }

    #[test]
    fn interactive_flush_buffered_echo_surfaces_colon_prompt_typed_characters() {
        // Reproduces the interactive echo bug: at a colon/menu-style prompt that
        // the detector does not recognise (e.g. useradd's "Full Name []:" or a
        // 1/2/3 menu), a typed character is buffered as an incomplete token and
        // stays invisible until another byte arrives. flush_buffered_echo surfaces
        // it once the input source goes idle (a keystroke-sized read with nothing
        // following).
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        assert_eq!(streaming.push_str("Full Name []: "), "Full Name []: ");

        // The keystroke is held back by the streaming token buffer (the bug).
        assert_eq!(streaming.push_str("1"), "");
        // Going idle surfaces the buffered echo from that push, not a later one.
        assert_eq!(
            String::from_utf8(streaming.flush_buffered_echo()).expect("echo is UTF-8"),
            "1"
        );

        // A second keystroke behaves identically.
        assert_eq!(streaming.push_str("j"), "");
        assert_eq!(
            String::from_utf8(streaming.flush_buffered_echo()).expect("echo is UTF-8"),
            "j"
        );

        // Nothing remains buffered.
        assert!(streaming.finish().is_empty());
    }

    #[test]
    fn flush_buffered_echo_never_emits_a_partial_escape_sequence() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        assert_eq!(streaming.push_str("Choice []: "), "Choice []: ");
        // A read that ends mid escape sequence leaves only the partial escape buffered.
        assert_eq!(streaming.push_str("\x1b["), "");
        // An idle flush must keep the incomplete escape buffered, never emit it raw.
        assert!(streaming.flush_buffered_echo().is_empty());
        // The completing bytes produce the full sequence.
        assert_eq!(streaming.push_str("0m"), "\x1b[0m");
        assert!(streaming.finish().is_empty());
    }

    #[test]
    fn interactive_finish_neutralizes_incomplete_csi_before_resetting_overlay() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: up state\n    regex: up\n    color: f#00ff00\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let mut output = streaming.push(b"up\x1b[31");
        assert!(
            output
                .windows(b"\x1b[38".len())
                .any(|bytes| bytes == b"\x1b[38"),
            "the highlight overlay should be active before finish: {output:02x?}"
        );
        output.extend(streaming.finish());

        assert_eq!(super::strip_ansi(&output), b"up^[31");
        assert_eq!(super::incomplete_escape_start(&output), None);
        assert!(
            output
                .windows(b"\x1b[39m^[31".len())
                .any(|bytes| bytes == b"\x1b[39m^[31"),
            "the generated reset must precede neutralized CSI text: {output:02x?}"
        );
    }

    #[test]
    fn finish_drops_unterminated_string_controls_and_preserves_visible_prefix() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for suffix in [
            b"\x1b]0;unfinished title".as_slice(),
            b"\x1b]0;payload with nested csi \x1b[".as_slice(),
            b"\x1b]0;payload with complete nested csi \x1b[31m".as_slice(),
            b"\x1bPunfinished dcs".as_slice(),
            b"\x1bXunfinished sos".as_slice(),
            b"\x1b^unfinished pm".as_slice(),
            b"\x1b_unfinished apc".as_slice(),
            b"\x9d0;unfinished title".as_slice(),
            b"\x9d0;payload with nested csi \x9b31".as_slice(),
            b"\x9d0;payload with complete nested csi \x9b31m".as_slice(),
            b"\x90unfinished dcs".as_slice(),
            b"\x98unfinished sos".as_slice(),
            b"\x9eunfinished pm".as_slice(),
            b"\x9funfinished apc".as_slice(),
        ] {
            let mut input = b"safe prefix".to_vec();
            input.extend_from_slice(suffix);
            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());

            let mut output = streaming.push(&input);
            output.extend(streaming.finish());

            assert_eq!(output, b"safe prefix", "unsafe EOF output: {output:02x?}");
        }
    }

    #[test]
    fn completed_string_control_keeps_nested_incomplete_looking_bytes_in_its_payload() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for input in [
            b"safe\x1b]0;payload\x1b[\x07after".as_slice(),
            b"safe\x9d0;payload\x9b31\x9cafter".as_slice(),
        ] {
            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut output = streaming.push(input);
            output.extend(streaming.finish());
            assert_eq!(
                output, input,
                "completed string control changed: {output:02x?}"
            );
        }
    }

    #[test]
    fn finish_neutralizes_raw_c1_csi_and_drops_raw_c1_strings() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        let mut csi = StreamingHighlighter::new_interactive(highlighter.clone());
        let mut csi_output = csi.push(b"safe\x9b31");
        csi_output.extend(csi.finish());
        assert_eq!(csi_output, b"safe^31");
        assert!(!csi_output.contains(&0x9b));

        let mut osc = StreamingHighlighter::new_interactive(highlighter);
        let mut osc_output = osc.push(b"safe\x9d0;unfinished");
        osc_output.extend(osc.finish());
        assert_eq!(osc_output, b"safe");
        assert!(!osc_output.contains(&0x9d));
    }

    #[test]
    fn noninteractive_finish_never_emits_an_incomplete_terminal_control() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        let mut csi = StreamingHighlighter::new(highlighter.clone());
        let mut csi_output = csi.push(b"plain\x1b[2;");
        csi_output.extend(csi.finish());
        assert_eq!(csi_output, b"plain^[2;");
        assert_eq!(super::incomplete_escape_start(&csi_output), None);

        let mut dcs = StreamingHighlighter::new(highlighter);
        let mut dcs_output = dcs.push(b"plain\x1bPunfinished");
        dcs_output.extend(dcs.finish());
        assert_eq!(dcs_output, b"plain");
        assert_eq!(super::incomplete_escape_start(&dcs_output), None);
    }

    #[test]
    fn finish_preserves_incomplete_utf8_bytes_that_contain_c1_values() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: lead byte\n    regex: '\\xE2'\n    color: f#00ff00\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");

        for interactive in [false, true] {
            let mut streaming = if interactive {
                StreamingHighlighter::new_interactive(highlighter.clone())
            } else {
                StreamingHighlighter::new(highlighter.clone())
            };
            let input = b"\xe2\x9d";
            let mut output = streaming.push(input);
            output.extend(streaming.finish());
            assert_eq!(output, input, "partial UTF-8 changed: {output:02x?}");
        }
    }

    #[test]
    fn flush_buffered_echo_is_noop_for_noninteractive_streams() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: documentation IPv4 addresses
    regex: '\b192\.0\.2\.\d+\b'
    color: f#00ffff
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        let mut streaming = StreamingHighlighter::new(highlighter);

        // Noninteractive streaming speculatively buffers a partial token so a token
        // split across reads is still highlighted as a unit.
        assert_eq!(streaming.push_str("ip 192.0."), "ip ");
        // An echo flush must not disturb noninteractive token buffering.
        assert!(streaming.flush_buffered_echo().is_empty());
        // The rest of the token completes and is highlighted as a single span.
        let mut output = String::from("ip ");
        output.push_str(&streaming.push_str("2.1\n"));
        assert!(
            output.contains("\x1b[38;2;0;255;255m192.0.2.1\x1b[0m"),
            "{output:?}"
        );
    }

    #[test]
    fn malformed_extended_sgr_color_parameters_are_not_reinterpreted_as_attributes() {
        let mut state = NativeSgrState::default();

        state.apply_sequence(b"\x1b[38;5m");
        assert_eq!(state.ansi_start(), None);

        state.apply_sequence(b"\x1b[48;2;3;4m");
        assert_eq!(state.ansi_start(), None);
    }

    #[test]
    fn empty_sgr_parameters_reset_native_state_before_following_codes() {
        let mut state = NativeSgrState::default();

        state.apply_sequence(b"\x1b[1m");
        state.apply_sequence(b"\x1b[;31m");

        assert_eq!(state.ansi_start().as_deref(), Some("\x1b[31m"));
    }

    #[test]
    fn native_state_restores_faint_conceal_overline_and_attribute_variants() {
        let mut state = NativeSgrState::default();

        state.apply_sequence(b"\x1b[2;6;8;21;53m");
        assert_eq!(state.ansi_start().as_deref(), Some("\x1b[2;6;8;21;53m"));

        state.apply_sequence(b"\x1b[22;25;28;24;55m");
        assert_eq!(state.ansi_start(), None);
    }

    #[test]
    fn native_state_preserves_colon_underline_and_replaces_exclusive_variants() {
        let mut state = NativeSgrState::default();

        state.apply_sequence(b"\x1b[4:3m");
        assert_eq!(state.ansi_start().as_deref(), Some("\x1b[4:3m"));

        state.apply_sequence(b"\x1b[21m");
        state.apply_sequence(b"\x1b[4m");
        assert_eq!(state.ansi_start().as_deref(), Some("\x1b[4m"));

        state.apply_sequence(b"\x1b[12m");
        state.apply_sequence(b"\x1b[11m");
        assert_eq!(state.ansi_start().as_deref(), Some("\x1b[4;11m"));
    }

    #[test]
    fn native_colon_sgr_reconstructs_only_exact_safe_parameters() {
        let mut state = NativeSgrState::default();

        state.apply_sequence(b"\x1b[4:03m");
        state.apply_sequence(b"\x1b[38:05:042m");
        let empty_color_space = [b"\x1b[48:2:".as_slice(), b":001:002:003m"].concat();
        state.apply_sequence(&empty_color_space);
        state.apply_sequence(b"\x1b[58:2:000:004:005:006m");
        let expected = ["\x1b[4:3;38:5:42;48:2:", ":1:2:3;58:2:0:4:5:6m"].concat();
        assert_eq!(state.ansi_start().as_deref(), Some(expected.as_str()));

        let extra_rgb_field = [b"\x1b[48:2:".as_slice(), b":1:2:3:4m"].concat();
        for invalid in [
            b"\x1b[4:3:0m".as_slice(),
            b"\x1b[38:5:42:0m".as_slice(),
            extra_rgb_field.as_slice(),
            b"\x1b[4:3:\x07m".as_slice(),
            b"\x1b[38:5:42:\nm".as_slice(),
        ] {
            let mut state = NativeSgrState::default();
            state.apply_sequence(invalid);
            assert_eq!(state.ansi_start(), None, "accepted {invalid:02x?}");
        }
    }

    #[test]
    fn highlighted_output_does_not_reemit_controls_from_malformed_colon_sgr() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: marker\n    regex: 'x'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");

        for prefix in [b"\x1b[4:3:\x07m".as_slice(), b"\x1b[38:5:42:\nm".as_slice()] {
            let mut input = prefix.to_vec();
            input.push(b'x');
            let output = highlighter.highlight_bytes(&input);
            let input_controls = input
                .iter()
                .filter(|byte| matches!(**byte, b'\x07' | b'\n'))
                .count();
            let output_controls = output
                .iter()
                .filter(|byte| matches!(**byte, b'\x07' | b'\n'))
                .count();

            assert_eq!(output_controls, input_controls, "output {output:02x?}");
            assert!(output.ends_with(b"\x1b[0m"), "output {output:02x?}");
        }
    }

    #[test]
    fn native_colon_underline_is_restored_after_prism_underline() {
        let mut state = NativeSgrState::default();
        state.apply_sequence(b"\x1b[4:3m");
        let prism_style = crate::style::Style {
            underline: true,
            ..crate::style::Style::default()
        };

        assert_eq!(
            state.restore_after_interactive_style(&prism_style),
            b"\x1b[24;4:3m"
        );
    }

    #[test]
    fn regex_compile_diagnostics_escape_untrusted_rule_descriptions() {
        let config = PrismConfig::from_chromaterm_yaml(
            r#"
rules:
  - description: "bad\u001b]0;title\u0007"
    regex: '('
    color: f#ff0000
"#,
        )
        .expect("config parses before regex compilation");

        let message = Highlighter::from_config(config)
            .expect_err("invalid regex should fail")
            .to_string();

        assert!(!message.contains('\u{1b}'), "{message:?}");
        assert!(!message.contains('\u{7}'), "{message:?}");
        assert!(message.contains("\\x1b"), "{message:?}");
        assert!(message.contains("\\x07"), "{message:?}");
    }

    #[test]
    fn raw_c1_controls_are_not_visible_or_highlightable_text() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: control payload
    regex: '31m'
    color: f#ff0000
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");
        let input = b"before\x9b31mafter\x9b0m\n";

        assert_eq!(highlighter.highlight_bytes(input), input);
        assert_eq!(super::strip_ansi(input), b"beforeafter\n");
        assert_eq!(
            super::AnsiChunk::from_slice(input).visible_bytes(),
            b"beforeafter\n"
        );
    }

    #[test]
    fn raw_c1_string_controls_are_sanitized_but_c1_csi_is_preserved() {
        let input = b"safe\x9d0;title\x07\x9b31mred\x9b0m\n";

        assert_eq!(
            super::strip_string_escapes(input),
            b"safe\x9b31mred\x9b0m\n"
        );
    }

    #[test]
    fn raw_c1_mode_contract_covers_every_control_byte() {
        for control in 0x80u8..=0x9f {
            let sequence = match control {
                0x9b => vec![control, b'3', b'1', b'm'],
                0x9d => vec![control, b'x', 0x07],
                0x90 | 0x98 | 0x9e | 0x9f => vec![control, b'x', 0x9c],
                _ => vec![control],
            };
            let mut input = b"a".to_vec();
            input.extend_from_slice(&sequence);
            input.push(b'b');

            assert_eq!(super::strip_ansi(&input), b"ab", "control 0x{control:02x}");
            let expected_sanitized = if super::is_c1_string_escape_start(control) {
                b"ab".to_vec()
            } else {
                input.clone()
            };
            assert_eq!(
                super::strip_string_escapes(&input),
                expected_sanitized,
                "control 0x{control:02x}"
            );
        }
    }

    #[test]
    fn utf8_continuation_bytes_do_not_terminate_string_controls() {
        // U+071C is DC 9C in UTF-8. Its continuation byte is numerically the
        // 8-bit ST control, but it remains payload while part of valid UTF-8.
        for introducer in [b"\x1b]0;".as_slice(), b"\x9d0;".as_slice()] {
            let mut input = b"before".to_vec();
            input.extend_from_slice(introducer);
            input.extend_from_slice("hidden \u{071c} payload".as_bytes());
            input.push(0x07);
            input.extend_from_slice(b"after");

            assert_eq!(super::strip_string_escapes(&input), b"beforeafter");
            assert_eq!(super::strip_ansi(&input), b"beforeafter");
        }
    }

    #[test]
    fn malformed_utf8_before_raw_st_does_not_hide_visible_suffix_text() {
        for introducer in [b"\x1b]0;".as_slice(), b"\x9d0;".as_slice()] {
            let mut input = b"before".to_vec();
            input.extend_from_slice(introducer);
            input.extend_from_slice(&[0xe2, 0x9c]);
            input.extend_from_slice(b"after");

            assert_eq!(super::strip_string_escapes(&input), b"beforeafter");
            assert_eq!(super::strip_ansi(&input), b"beforeafter");
        }
    }

    #[test]
    fn raw_c1_csi_and_strings_are_carried_across_read_boundaries() {
        assert_eq!(super::incomplete_escape_start(b"ok\x9b31"), Some(2));
        assert_eq!(super::incomplete_sanitize_start(b"ok\x9dtitle"), Some(2));
    }

    #[test]
    fn raw_c1_csi_updates_terminal_state_checks() {
        assert_eq!(super::alternate_screen_command(b"\x9b?1049h"), Some(true));
        assert!(super::is_cursor_positioning_sequence(b"\x9b2H"));
    }

    // The tokenizer accepts the UTF-8 encoding of C1 CSI (`C2 9B`) as a
    // control, so every classifier must treat it exactly like `9B` and `ESC [`.
    #[test]
    fn utf8_c1_csi_matches_raw_c1_csi_in_every_classifier() {
        assert_eq!(
            super::alternate_screen_command(b"\xc2\x9b?1049h"),
            Some(true)
        );
        assert_eq!(
            super::alternate_screen_command(b"\xc2\x9b?1049l"),
            Some(false)
        );
        assert!(super::is_csi_sequence(b"\xc2\x9b2H"));
        assert!(super::is_cursor_positioning_sequence(b"\xc2\x9b2H"));
        assert!(super::is_interactive_layout_boundary_sequence(
            b"\xc2\x9b2J"
        ));
        assert!(super::contains_bracketed_paste_disable_tokens(&[
            super::Token::Ansi(b"\xc2\x9b?2004l".to_vec())
        ]));

        let mut state = NativeSgrState::default();
        state.apply_sequence(b"\xc2\x9b31m");
        assert_eq!(state.ansi_start().as_deref(), Some("\x1b[31m"));
        state.apply_sequence(b"\xc2\x9b0m");
        assert_eq!(state.ansi_start(), None);

        // The visible-byte map must agree with the tokenizer: the control is
        // not visible text, and it is an SGR range.
        let (visible, ranges) = super::visible_byte_map_and_ansi_ranges(b"a\xc2\x9b31mb");
        assert_eq!(
            visible.iter().map(|byte| byte.byte).collect::<Vec<_>>(),
            b"ab"
        );
        assert_eq!(ranges.len(), 1);
        assert!(ranges[0].is_sgr);
    }

    #[test]
    fn incomplete_escape_scan_checks_the_suffix_candidate() {
        assert_eq!(incomplete_escape_start(b"ok\x1b[31mstill\x1b["), Some(12));
        assert_eq!(incomplete_escape_start(b"ok\x1b[31m"), None);
    }

    #[test]
    fn incomplete_escape_scan_uses_one_forward_suffix_search() {
        let source = include_str!("highlight.rs");
        let function_source = source
            .split("fn incomplete_escape_start")
            .nth(1)
            .expect("function exists")
            .split("fn escape_is_incomplete_at")
            .next()
            .expect("function ends before next helper");

        assert!(function_source.contains("next_stateful_control_start"));
        assert!(function_source.contains("search_from = start + 1"));
        assert!(!function_source.contains("rposition"));
        assert!(!function_source.contains("search_start"));
        assert!(!function_source.contains(".position(|byte| *byte == 0x1b)"));
    }

    #[test]
    fn oversized_escape_neutralization_does_not_repeat_full_incomplete_scans() {
        let source = include_str!("highlight.rs");
        let function_source = source
            .rsplit("fn neutralize_oversized_incomplete_escape")
            .next()
            .expect("function exists")
            .split("fn is_token_continuation")
            .next()
            .expect("function ends before next helper");

        assert!(
            !function_source.contains("while let Some(start) = incomplete_escape_start(bytes)")
        );
    }

    #[test]
    fn streaming_hot_path_reuses_tokenized_helper_checks() {
        let source = include_str!("highlight.rs");
        let runtime_source = source.split("#[cfg(test)]").next().unwrap_or(source);
        let function_source = runtime_source
            .split("fn highlight_streaming_chunk")
            .nth(1)
            .expect("function exists")
            .split("fn flush_highlightable")
            .next()
            .expect("function ends before next helper");

        assert!(!function_source.contains("contains_alternate_screen_enable(input)"));
        assert!(!function_source.contains("contains_cursor_positioning_sequence(input)"));
    }
}
