//! Terminal highlighting engines.
//!
//! `Highlighter` applies compiled rules to complete byte slices or strings.
//! `StreamingHighlighter` keeps enough state to highlight chunked terminal
//! output without breaking ANSI escapes or interactive command echo.

use crate::config::{CaptureRef, PrismConfig, RuleSpec, RuleStyle};
use crate::style::{ColorMode, Style};
use crate::terminal_text::escape_untrusted;
use pcre2::bytes::{Regex, RegexBuilder};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use thiserror::Error;

const UNICODE_PROMPT_MARKERS: &[&str] = &["○", "●", "❯", "❮", "❱", "›", "»", "➜", "➤", "λ"];
pub(crate) const MAX_INCOMPLETE_ESCAPE_BYTES: usize = 16 * 1024;
const MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES: usize = 1024;
/// How many times the sanitize split may scan the read. A clean split takes
/// two scans, one to find it and one to check the prefix before it; each
/// further scan steps back over one control that ends in a character the
/// previous split cut off. Scans after the first resume from a stop before
/// their split instead of the start of the read, so stepping back over a run
/// of controls costs about one scan of the run. Only crafted output steps
/// back this often, and past the cap one more scan checks a split in front
/// of the run that the first scan noted, so the read is carried from there.
const MAX_SANITIZE_SPLIT_PASSES: usize = 8;
/// How far past a stop the top-level escape scan may have read to reach it:
/// whether a character after an ESC is cut off depends on the three bytes
/// from the stop and on whether a fourth follows. The scan of a prefix makes
/// every stop that the scan of a longer buffer made at least this far before
/// the prefix ends.
const TOP_LEVEL_SCAN_LOOKAHEAD: usize = 4;
/// The least distance between the stops that the first scan of a sanitize
/// split keeps to resume from, so a read of many controls keeps a stop per
/// this many bytes instead of one per control. A later scan resuming from one
/// reads at most this much more than it would from the nearest stop.
const SANITIZE_SPLIT_STOP_SPACING: usize = 64;
/// How many CSI parameters xterm keeps. Past them it ignores `;` and `:` and
/// reads every further digit into the last parameter.
const XTERM_MAX_CSI_PARAMETERS: usize = 30;
/// The most parameters a removed prompt-echo SGR may hold and still leave an
/// off behind: the Linux console keeps 16 and drops a CSI with more, Vim's
/// libvterm keeps 16, xterm 30.
const PROMPT_ECHO_OFF_SOURCE_MAX_PARAMETERS: usize = 16;
/// The groups whose end the prompt-echo neutralizer writes in place of a
/// removed SGR, as (attribute group of `SgrRendition`, or `None` for the
/// background; off code), in ascending code order: italic, underline, blink,
/// inverse, conceal, strike, background, overline. Each code ends that one
/// group on every terminal that has it and is ignored by one that does not,
/// so writing it where the group is already off changes nothing. Intensity
/// (22 ends bold and faint together, which needs the terminal's state to be
/// right), the foreground (the reset inserted before the command ends it),
/// fonts (10 resets the Linux console's character mapping, which 0 leaves
/// alone) and the rarer groups are not written.
const PROMPT_ECHO_OFF_CODES: [(Option<u16>, u16); 8] = [
    (Some(3), 23),
    (Some(4), 24),
    (Some(5), 25),
    (Some(7), 27),
    (Some(8), 28),
    (Some(9), 29),
    (None, 49),
    (Some(53), 55),
];
const MAX_TENTATIVE_UTF8_SUFFIX_BYTES: usize = 2;
const PCRE2_JIT_STACK_LIMIT_BYTES: usize = 32 * 1024;
const PCRE2_MATCH_LIMIT: usize = 100_000;
const PCRE2_DEPTH_LIMIT: usize = 1_000;
const MAX_COMPILED_RULES: usize = 512;
/// Longest visible prompt (ASCII whitespace trimmed) that `looks_like_prompt_tail`
/// accepts. A prompt candidate end is never ASCII whitespace, so no candidate end
/// past this offset can begin a prompt echo; scans stop there.
const MAX_PROMPT_TAIL_BYTES: usize = 180;
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
    /// A PCRE2 rule failed to compile, or the configuration was refused for
    /// a reason that is no rule's compile error: it holds too many rules or
    /// too much pattern text, or a color names a capture group its pattern
    /// lacks.
    ///
    /// The two are told apart by `source`. A refused configuration carries a
    /// PCRE2 match error with the code of an exceeded depth limit
    /// (`PCRE2_ERROR_DEPTHLIMIT`), which compiling a rule never yields, and
    /// is displayed as `description` alone. Every other value is displayed
    /// as a rule that failed to compile, one built by hand included.
    Regex {
        /// Human-readable rule description. For a refused configuration, the
        /// whole message instead.
        description: String,
        /// PCRE2 compilation error. For a refused configuration, the match
        /// error described above, which says nothing about the refusal.
        source: pcre2::Error,
    },
}

impl std::fmt::Display for HighlightError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self::Regex {
            description,
            source,
        } = self;
        if is_validation_marker(source) {
            // The message is complete; the marker would only send the reader
            // to a pattern offset that does not exist.
            return formatter.write_str(description);
        }
        write!(
            formatter,
            "rule '{description}' failed to compile: {source}"
        )
    }
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
    /// True when `pending` holds a trailing token split off the ordinary
    /// interactive split (set at the last branch in `push_combined_chunk`). This
    /// is the provenance half of the line-edit flush trigger; its only job is to
    /// keep that trigger disjoint from the prompt-echo one, which buffers its
    /// tail at a different split site. The shape half lives in the visible line,
    /// not here, so the signal survives a redraw delivered across several reads.
    pending_from_interactive_split: bool,
    /// True when the current visible line has been rewritten in place: a
    /// backspace or a CSI cursor-back / in-line delete moved back over text
    /// already on the line so new text could be written over it. Sticky for the
    /// life of the line: set by `observe_interactive_visible_chunk` wherever it
    /// sees such a control, cleared by the `\r`/`\n` that ends the line and by
    /// either alternate-screen switch. This is the shape half of the line-edit
    /// flush trigger. It is deliberately NOT derived from `visible_line_tail`,
    /// which is capped and can age the control out of a long rewritten line,
    /// and NOT reset per chunk, since a redraw spans several reads.
    line_rewritten_in_place: bool,
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

/// Whether the locale says the terminal reads UTF-8. The command line infers
/// it at startup from the text of the locale variables; nothing checks the
/// terminal. Off unless the command line turns it on, so the library keeps
/// treating a raw C1 byte as the control an 8-bit terminal sees.
static TERMINAL_READS_UTF8: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Test hook, read in test builds only: lets one test thread run with the
    /// switch on without touching the tests that run beside it. It is not
    /// behind a `cfg` attribute because the source-shape tests in this file
    /// take everything before the first one as runtime code.
    pub(crate) static TERMINAL_READS_UTF8_IN_THIS_THREAD: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Called once, before any output is read. The value must not change during a
/// run: the tokenizer and both escape scanners read it, and they have to
/// agree on where a string ends.
pub(crate) fn set_terminal_reads_utf8(reads_utf8: bool) {
    TERMINAL_READS_UTF8.store(reads_utf8, Ordering::Relaxed);
}

fn terminal_reads_utf8() -> bool {
    if cfg!(test) {
        if let Some(reads_utf8) = TERMINAL_READS_UTF8_IN_THIS_THREAD.with(std::cell::Cell::get) {
            return reads_utf8;
        }
    }
    TERMINAL_READS_UTF8.load(Ordering::Relaxed)
}

/// Incrementally tracks an incomplete terminal string control. Once the
/// control exceeds the bounded carry budget, its payload is discarded until a
/// BEL/ST terminator, a CAN/SUB cancellation, or an ESC that starts another
/// control arrives. This prevents bytes inside a hostile oversized
/// OSC/DCS/SOS/PM/APC payload from being reinterpreted as top-level terminal
/// controls on a later read.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringControlKind {
    EscOsc,
    EscString(u8),
    C1Osc(usize),
    C1String(u8, usize),
}

impl StringControlKind {
    fn at(bytes: &[u8], start: usize) -> Option<(Self, usize)> {
        match *bytes.get(start)? {
            0x1b => {
                let introducer = skip_escape_state_controls(bytes, start + 1);
                match *bytes.get(introducer)? {
                    b']' => Some((Self::EscOsc, introducer + 1)),
                    byte @ (b'P' | b'X' | b'^' | b'_') => {
                        Some((Self::EscString(byte), introducer + 1))
                    }
                    _ => None,
                }
            }
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
        Self::at(bytes, start).is_some_and(|(kind, _payload_start)| kind == self)
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
            // The budget covers the cancelled controls and ESC-ended strings
            // carried in front of this string too, so such a chain cannot grow
            // the carry without bound.
            if self.carry_offset + self.total_bytes >= MAX_INCOMPLETE_ESCAPE_BYTES {
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
                StringPayloadScan::RestartAtEscape => {
                    return TrackedStringScan::Restart(self.scanned - 2);
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
    /// The string ended at this ESC, which starts the next control.
    Restart(usize),
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
    /// The previous byte was an ESC that does not start ST: the string ended
    /// before it, and that ESC and the current byte begin a new control.
    RestartAtEscape,
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
                StringPayloadScan::RestartAtEscape => {
                    // Bytes are fed contiguously, so the ESC is the byte before
                    // the current one. Resume there; if it closed the previous
                    // chunk it is no longer in `bytes`, so replay it.
                    let mut replay = [0; MAX_TENTATIVE_UTF8_SUFFIX_BYTES];
                    return Some(match (start + offset).checked_sub(1) {
                        Some(escape) => DiscardedTerminator {
                            end: escape,
                            replay,
                            replay_len: 0,
                        },
                        None => {
                            replay[0] = 0x1b;
                            DiscardedTerminator {
                                end: 0,
                                replay,
                                replay_len: 1,
                            }
                        }
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

    /// Reports a BEL/ST terminator, a CAN/SUB cancellation, or an ESC that
    /// ends the string by starting another control. A raw ST that was
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
                } else if byte == 0x9c && !terminal_reads_utf8() {
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
            // Like a terminal's parser, any other byte after ESC ends the
            // string before that ESC, which then starts a new control.
            return StringPayloadScan::RestartAtEscape;
        }

        if matches!(byte, 0x18 | 0x1a)
            || (self.allows_bel && byte == 0x07)
            || (byte == 0x9c && !terminal_reads_utf8())
        {
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

    /// Goes on to discard the string being tracked, up to where it ends. For
    /// a caller that has dropped the buffer holding its start: what follows
    /// is still its payload, and would be text without that start.
    pub(crate) fn discard_tracked_string(&mut self) {
        let mode = std::mem::take(&mut self.mode);
        let OversizedStringControlMode::Tracking(tracked) = mode else {
            self.mode = mode;
            return;
        };
        self.mode = OversizedStringControlMode::Discarding(tracked.into_discarded());
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
                    TrackedStringScan::Restart(escape) => {
                        cancellation_root = Some(tracked.drop_start);
                        scan_from = escape;
                    }
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
                TrackedStringScan::Restart(escape) => {
                    // Like a cancelled control, the ended string is carried and
                    // dropped together with whatever its ESC starts.
                    cancellation_root = Some(drop_start);
                    scan_from = escape;
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
                    // The discarded string took its cancelled prefix with it, so
                    // a control right after the terminator starts a new chain.
                    cancellation_root = None;
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
    /// Whether what the rule matches depends on where its search starts.
    search_start_matters: bool,
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
            pending_from_interactive_split: false,
            line_rewritten_in_place: false,
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
            pending_from_interactive_split: false,
            line_rewritten_in_place: false,
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
            pending_from_interactive_split: false,
            line_rewritten_in_place: false,
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
            pending_from_interactive_split: false,
            line_rewritten_in_place: false,
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
        self.pending_from_interactive_split = false;
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
            let remainder = AnsiChunk::new(combined.bytes[prefix_len..].to_vec());
            let mut output = self.emit_prompt_echo_passthrough(prompt.bytes());
            self.observe_interactive_visible_chunk(&AnsiChunk::new(output.clone()));

            let split_at = interactive_split_at_chunk(&remainder, false, self.alternate_screen);
            let processed = split_prepared_pending(remainder, split_at, &mut self.pending);
            self.pending_from_prompt_echo_remainder = !self.pending.is_empty();
            self.pending_from_interactive_split = false;

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
            let remainder = AnsiChunk::new(combined.bytes[prefix_len..].to_vec());
            let mut output = self.emit_prompt_echo_passthrough(prompt.bytes());
            self.observe_interactive_visible_chunk(&AnsiChunk::new(output.clone()));

            let split_at = interactive_split_at_chunk(&remainder, false, self.alternate_screen);
            let processed = split_prepared_pending(remainder, split_at, &mut self.pending);
            self.pending_from_prompt_echo_remainder = !self.pending.is_empty();
            self.pending_from_interactive_split = false;

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
            streaming_split_at(&combined)
        };
        let processed = split_prepared_pending(combined, split_at, &mut self.pending);
        self.pending_from_prompt_echo_remainder = false;
        self.pending_from_interactive_split = !self.pending.is_empty();

        let mut output = self.highlight_output_chunk(&processed);
        // Load-bearing: a chunk that switches screens belongs to no primary-screen
        // line. Clearing at the enable/disable token alone is not enough, because a
        // final alt-screen frame coalesced with the disable in one read would be
        // observed after that clear and re-arm the rewrite signal. Uses the
        // chunk-entry value; `self.alternate_screen` is already false here for a
        // disable chunk. Cost: bytes sharing a read with a screen switch go
        // unobserved, which at most skips a flush.
        if !alternate_screen_chunk {
            self.observe_interactive_visible_chunk(&processed);
        }
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

    /// Returns the reset for an interactive highlight style that is still
    /// active, or nothing when none is (always the case for noninteractive
    /// streams). A caller about to wait for more input uses it so the terminal
    /// is not left in a highlight color meanwhile; the next highlighted byte
    /// opens its style again.
    pub(crate) fn close_interactive_overlay(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        self.reset_interactive_overlay(&mut output);
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

    /// True only when [`Self::buffered_echo`] holds a token stranded on a visible
    /// line that has been rewritten in place: something moved back over text
    /// already on the line and wrote over it, with a backspace or with a CSI
    /// cursor-back or in-line delete. A readline-style editor emits exactly that
    /// when the user recalls history or edits the command line, so the stranded
    /// tail is the user's own recalled command, echoed back. The keystroke
    /// behind it is an arrow key, so the read loop's byte match against recent
    /// input can never confirm it; this shape is the only signal that it is
    /// echo.
    ///
    /// The shape is a flag on the line, not a search of the chunk that stranded
    /// the tail nor of the retained line tail. That matters three ways: a redraw
    /// delivered across several reads qualifies as long as one of them carried
    /// the control, a prompt-echo repaint chunk still reports the controls
    /// inside it even though the rest of the line tracker skips it, and a
    /// rewritten line longer than the retained tail does not age its control
    /// out. The flag clears at the `\r`/`\n` that ends the line and at either
    /// alternate-screen switch, so it cannot outlive the tracked line it
    /// describes.
    ///
    /// Column-one repaints (a bare `\r`, `ESC [ G`, a lone `ESC [ K`) are
    /// deliberately not line edits: that is byte-for-byte what a progress bar
    /// emits on every update, and treating it as one would flush during
    /// continuous output.
    ///
    /// Ordinary program output does not qualify *unless it too rewrites its line
    /// in place* (an in-place counter that backs up, an overstrike): for those,
    /// the tail is flushed early, which can split one highlight span into two.
    /// That is the ADR's accepted cosmetic limitation, never text loss. Of these
    /// controls only CUB is also a cursor-positioning sequence, so only a
    /// CUB-carrying chunk is sure to pass through whole; one carrying a
    /// backspace, DCH, ECH or ICH can strand its own trailing token and qualify
    /// in the same read. Either way the cost is one span drawn as two.
    pub(crate) fn buffered_echo_completes_line_edit_redraw(&self) -> bool {
        !self.buffered_echo().is_empty()
            && self.pending_from_interactive_split
            && !self.alternate_screen
            && self.line_rewritten_in_place
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
            // At the bottom of the screen a line feed scrolls in a blank line
            // drawn in the current rendition, so a style that shows on blank
            // cells ends before the terminator. No highlight spans one.
            let terminator = &bytes[line_end..boundary_end];
            match self.interactive_overlay.as_ref() {
                Some(style) if style_paints_blank_cells(style) => {
                    self.reset_interactive_overlay(&mut output);
                    output.extend_from_slice(terminator);
                }
                // The program's own rendition can draw blank cells in the
                // foreground (inverse, strike, overline, plain underline), and
                // then a foreground-only highlight shows on that blank line as
                // well. Only the terminator leaves the highlight: it is open
                // again right after it, as before.
                Some(style) if self.native_sgr.draws_blank_cells_with_the_foreground() => {
                    output.extend(style_reset_bytes(
                        style,
                        &self.native_sgr,
                        self.interactive_reset_mode(),
                    ));
                    output.extend_from_slice(terminator);
                    output.extend(
                        style
                            .ansi_start_with_mode(self.highlighter.color_mode)
                            .into_bytes(),
                    );
                }
                _ => output.extend_from_slice(terminator),
            }
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
        // Outside the alternate screen and without a screen switch, every token
        // below lands in `highlightable` and rebuilds `input` byte for byte, so
        // highlight the already tokenized chunk instead.
        if !self.alternate_screen
            && !input.tokens.iter().any(|token| {
                matches!(token, Token::Ansi(bytes) if alternate_screen_command(bytes).is_some())
            })
        {
            if !input.bytes.is_empty() {
                self.highlight_prepared(input, &mut output);
            }
            return output;
        }

        let mut highlightable = Vec::new();

        for token in &input.tokens {
            match &token {
                Token::Ansi(bytes) if is_alternate_screen_enable(bytes) => {
                    self.flush_highlightable(&mut highlightable, &mut output);
                    if saves_rendition(bytes) {
                        self.reset_interactive_overlay(&mut output);
                    }
                    self.native_sgr.apply_sequence(bytes);
                    self.alternate_screen = true;
                    self.prompt_echo_passthrough = false;
                    // Entering the alternate screen is a line discontinuity: the
                    // tracked line describes the primary screen and must not
                    // carry into (or back out of) the app's own drawing.
                    self.visible_line_tail.clear();
                    self.line_rewritten_in_place = false;
                    output.extend_from_slice(bytes);
                }
                // RIS resets the terminal, which puts it back on the normal
                // screen too.
                Token::Ansi(bytes)
                    if is_alternate_screen_disable(bytes)
                        || (self.alternate_screen && escape_final_byte(bytes) == Some(b'c')) =>
                {
                    self.flush_highlightable(&mut highlightable, &mut output);
                    self.reset_interactive_overlay(&mut output);
                    self.native_sgr.apply_sequence(bytes);
                    self.alternate_screen = false;
                    self.visible_line_tail.clear();
                    self.line_rewritten_in_place = false;
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
        self.highlight_prepared(&chunk, output);
    }

    fn highlight_prepared(&mut self, chunk: &AnsiChunk, output: &mut Vec<u8>) {
        if self.passthrough_single_byte_chunks {
            let reset_mode = self.interactive_reset_mode();
            output.extend(self.highlighter.highlight_chunk_with_interactive_overlay(
                chunk,
                Some(&mut self.benchmark),
                reset_mode,
                &mut self.native_sgr,
                &mut self.interactive_overlay,
                &mut self.disabled_rule_identities,
            ));
        } else {
            output.extend(self.highlighter.highlight_chunk_with_native_sgr(
                chunk,
                Some(&mut self.benchmark),
                ResetMode::Full,
                &mut self.native_sgr,
                &mut self.disabled_rule_identities,
            ));
        }
    }

    fn emit_prompt_echo_passthrough(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        self.reset_interactive_overlay(&mut output);
        let neutralized = neutralize_prompt_echo_source_sgr_on(
            input,
            &self.visible_line_tail,
            self.interactive_reset_mode(),
            &self.native_sgr,
        );
        output.extend_from_slice(&neutralized.bytes);
        if neutralized.offs.is_empty() && self.native_sgr.without_offs.is_none() {
            self.observe_native_sgr_chunk(&AnsiChunk::new(output.clone()));
        } else {
            self.native_sgr.observe_prompt_echo(
                &AnsiChunk::new(output.clone()),
                &AnsiChunk::new(neutralized.without_offs()),
            );
        }
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

    /// Tracks whether the current line has been rewritten in place, for the
    /// line-edit flush trigger. Runs before the rest of
    /// [`Self::observe_interactive_visible_chunk`], including its prompt-echo
    /// preservation shortcut: that shortcut exists to keep `visible_line_tail`
    /// intact across a repaint, but a repaint chunk can carry the very
    /// backspaces that mark the edit, and the `\r`/`\n` that ends the line.
    /// Both have to be seen.
    ///
    /// Walks the chunk's tokens in stream order, because order decides the
    /// outcome: a cursor-back after a `\r` leaves the line rewritten, the same
    /// two controls the other way round do not.
    fn observe_line_rewrite_controls(&mut self, input: &AnsiChunk) {
        for token in &input.tokens {
            match token {
                Token::Ansi(bytes) => {
                    if is_in_place_line_edit_sequence(bytes) {
                        self.line_rewritten_in_place = true;
                    }
                }
                Token::Text(text) => {
                    for byte in text.iter() {
                        match *byte {
                            0x08 => self.line_rewritten_in_place = true,
                            b'\r' | b'\n' => self.line_rewritten_in_place = false,
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn observe_interactive_visible_chunk(&mut self, input: &AnsiChunk) {
        if !self.passthrough_single_byte_chunks {
            return;
        }

        self.observe_line_rewrite_controls(input);

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
    /// The ESC or C1 control that starts this range is what ends the control
    /// before it: a string control with no terminator, or a sequence cut
    /// short. Removing the range would leave the terminal inside that control.
    ends_open_control: bool,
}

/// Takes the program's SGRs out of a prompt-echo line and resets the
/// foreground before the command, as `prompt_echo_source_sgr_plan` decides.
/// Each removed SGR leaves behind, where it stood, what it turns off of the
/// groups in `PROMPT_ECHO_OFF_CODES`: a prompt that ends its own background,
/// inverse or underline keeps that end. `rendition` is what the terminal
/// shows before this output; it is followed through the controls the output
/// keeps and writes, and decides only whether an off is worth writing: one
/// written for a group already off changes nothing, and one not written
/// leaves the group as before. No off is written once the program has used
/// the SGR stack (`NativeSgrState::sgr_stack_used`).
fn neutralize_prompt_echo_source_sgr_on(
    input: &[u8],
    previous_visible_tail: &[u8],
    reset_mode: ResetMode,
    rendition: &NativeSgrState,
) -> NeutralizedPromptEcho {
    let PromptEchoSgrPlan {
        mut remove_ranges,
        mut reset_positions,
        controls,
    } = prompt_echo_source_sgr_plan_and_controls(input, previous_visible_tail);
    if remove_ranges.is_empty() {
        return NeutralizedPromptEcho {
            bytes: input.to_vec(),
            offs: Vec::new(),
        };
    }

    remove_ranges.sort_unstable();
    remove_ranges.dedup();
    reset_positions.sort_unstable();
    reset_positions.dedup();

    let mut terminal = rendition.clone();
    let mut controls = controls.iter().peekable();
    let mut output = Vec::with_capacity(input.len());
    let mut offs = Vec::new();
    let mut idx = 0usize;
    let mut remove_idx = 0usize;
    let mut reset_idx = 0usize;

    while idx < input.len() {
        while reset_positions.get(reset_idx) == Some(&idx) {
            let reset = prompt_echo_source_sgr_reset(reset_mode);
            output.extend_from_slice(reset);
            terminal.apply_sequence(reset);
            reset_idx += 1;
        }
        while controls.next_if(|control| control.start < idx).is_some() {}

        if let Some((start, end)) = remove_ranges.get(remove_idx).copied() {
            if idx == start {
                // A terminal runs the C0 controls it meets inside the
                // sequence, so they stay where it was, and the off it
                // leaves follows them.
                output.extend(
                    input[start..end]
                        .iter()
                        .filter(|byte| matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f)),
                );
                let off = terminal.prompt_echo_off(&input[start..end]);
                if !off.is_empty() {
                    terminal.apply_prompt_echo_off(&off);
                    offs.push((output.len(), output.len() + off.len()));
                    output.extend(off);
                }
                idx = end;
                remove_idx += 1;
                continue;
            }
        }

        if let Some(control) = controls.next_if(|control| control.start == idx) {
            let control = &input[control.start..control.end];
            terminal.apply_sequence(control);
            output.extend_from_slice(control);
            idx += control.len();
            continue;
        }

        output.push(input[idx]);
        idx += 1;
    }

    while reset_positions.get(reset_idx) == Some(&idx) {
        output.extend_from_slice(prompt_echo_source_sgr_reset(reset_mode));
        reset_idx += 1;
    }

    NeutralizedPromptEcho {
        bytes: output,
        offs,
    }
}

/// What `neutralize_prompt_echo_source_sgr_on` writes for a prompt-echo
/// line, and where in it the offs are.
struct NeutralizedPromptEcho {
    bytes: Vec<u8>,
    /// The `(start, end)` ranges of `bytes` that are offs, in order.
    offs: Vec<(usize, usize)>,
}

impl NeutralizedPromptEcho {
    /// The bytes without the offs: what 1.4.1 wrote for the line.
    fn without_offs(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.bytes.len());
        let mut last = 0;
        for (start, end) in &self.offs {
            bytes.extend_from_slice(&self.bytes[last..*start]);
            last = *end;
        }
        bytes.extend_from_slice(&self.bytes[last..]);
        bytes
    }
}

/// Whether the terminals PrismTTY writes for read the removed prompt-echo
/// SGR `bytes` as the parser does for the groups of `PROMPT_ECHO_OFF_CODES`,
/// so that an off taken from it is the source's own on every one of them:
/// opened by ESC, no sub-parameter, at most
/// `PROMPT_ECHO_OFF_SOURCE_MAX_PARAMETERS` parameters, each empty or a
/// number up to 65535, and no extended color.
///
/// A C0 control between ESC and `[` is run in place and leaves the sequence
/// a CSI, so it does not change this. Whether a C1 introducer runs depends
/// on the encoding in force and on the terminal. The Linux console drops a
/// whole CSI holding `:` outside 38, 48 and 58 or a 17th parameter, and
/// keeps a parameter modulo 2^32 where the parser ignores one past 65535:
/// `27;4294967303` is inverse off and on again there. It also takes the
/// empty parameter of `38;;` as the mode where the parser reads a reset,
/// and has no 58, so it reads 58's arguments as codes.
fn is_plain_sgr(bytes: &[u8]) -> bool {
    if bytes.first() != Some(&0x1b) {
        return false;
    }
    let Some(body) = sgr_parameters(bytes) else {
        return false;
    };
    if body.contains(&b':') {
        return false;
    }
    let mut parameters = 0usize;
    for parameter in body.split(|byte| *byte == b';') {
        parameters += 1;
        if parameters > PROMPT_ECHO_OFF_SOURCE_MAX_PARAMETERS {
            return false;
        }
        if !parameter.is_empty() && matches!(parse_sgr_number(parameter), None | Some(38 | 48 | 58))
        {
            return false;
        }
    }
    true
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

/// The SGRs `prompt_echo_source_sgr_plan` removes and the positions of the
/// resets it inserts, with every control of the input in order.
#[derive(Default)]
struct PromptEchoSgrPlan {
    remove_ranges: Vec<(usize, usize)>,
    reset_positions: Vec<usize>,
    controls: Vec<AnsiRange>,
}

fn prompt_echo_source_sgr_plan(
    input: &[u8],
    previous_visible_tail: &[u8],
) -> (Vec<(usize, usize)>, Vec<usize>) {
    let plan = prompt_echo_source_sgr_plan_and_controls(input, previous_visible_tail);
    (plan.remove_ranges, plan.reset_positions)
}

fn prompt_echo_source_sgr_plan_and_controls(
    input: &[u8],
    previous_visible_tail: &[u8],
) -> PromptEchoSgrPlan {
    // Most chunks carry no escape at all; skip building the visible-byte map
    // for them. Any ESC, raw C1 or UTF-8 C1 (`C2 xx`) form contains one of
    // these bytes.
    if !input
        .iter()
        .any(|byte| *byte == 0x1b || is_c1_control(*byte))
    {
        return PromptEchoSgrPlan::default();
    }
    let (visible, ansi_ranges) = visible_byte_map_and_ansi_ranges(input);
    if visible.is_empty() || ansi_ranges.iter().all(|range| !range.is_sgr) {
        return PromptEchoSgrPlan::default();
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
                    .filter(|range| range.is_sgr && !range.ends_open_control)
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

    PromptEchoSgrPlan {
        remove_ranges,
        reset_positions,
        controls: ansi_ranges,
    }
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
            let is_sgr = sgr_parameters(&input[idx..end]).is_some();
            let ends_open_control = is_sgr
                && ansi_ranges.last().is_some_and(|previous: &AnsiRange| {
                    previous.end == idx
                        && !matches!(
                            scan_escape_for_neutralization(input, previous.start),
                            EscapeScan::Complete(_)
                        )
                });
            ansi_ranges.push(AnsiRange {
                start: idx,
                end,
                is_sgr,
                ends_open_control,
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

    // Whether the program may have left a style on, which a terminal that
    // runs the SGR would draw the command in: one opened by the UTF-8 form
    // `C2 9B` counts as well (see `NativeSgrState::encoded_csi`).
    let mut state = NativeSgrState {
        reads_encoded_csi: true,
        ..NativeSgrState::default()
    };
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

    for prompt_end in 1..line.len().min(MAX_PROMPT_TAIL_BYTES + 1) {
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
    if trimmed.is_empty() || trimmed.len() > MAX_PROMPT_TAIL_BYTES {
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
    // for the rare tail that actually ends in a Unicode prompt marker. Every
    // marker is multibyte UTF-8 and so ends in a non-ASCII byte; an ASCII last
    // byte, the common case, needs no marker compares at all.
    if trimmed.last().is_none_or(u8::is_ascii) {
        return false;
    }
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
    // One pass over the visible bytes: a CR inside an escape (an OSC payload,
    // say) is not a line break, and the line before each CR is built up as the
    // pass goes instead of re-stripping the whole prefix for every CR.
    if !bytes.contains(&b'\r') {
        return None;
    }
    let (visible, _) = visible_byte_map_and_ansi_ranges(bytes);
    let previous_continues_prompt = contains_prompt_echo_in_visible_line(previous_visible_tail)
        || looks_like_prompt_tail(previous_visible_tail);
    let mut before_cr_line = Vec::new();
    let mut after_cr_line = Vec::new();

    for (visible_idx, mapped) in visible.iter().enumerate() {
        match mapped.byte {
            b'\r' => {}
            b'\n' => {
                before_cr_line.clear();
                continue;
            }
            byte => {
                before_cr_line.push(byte);
                continue;
            }
        }

        let idx = mapped.raw;
        if bytes.get(idx + 1) == Some(&b'\n') {
            if contains_prompt_echo_in_visible_line(&before_cr_line) {
                return Some(idx + 2);
            }
            before_cr_line.clear();
            continue;
        }

        if idx + 1 == bytes.len() {
            return None;
        }

        let has_echo_before_cr = before_cr_line
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
            && (contains_prompt_echo_in_visible_line(&before_cr_line) || previous_continues_prompt);
        if has_echo_before_cr {
            after_cr_line.clear();
            after_cr_line.extend(
                visible[visible_idx + 1..]
                    .iter()
                    .map(|mapped| mapped.byte)
                    .take_while(|byte| !matches!(*byte, b'\r' | b'\n')),
            );
            if !redraws_interactive_prompt_line(&after_cr_line) {
                return Some(idx + 1);
            }
        }
        before_cr_line.clear();
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
    // A line this chunk already ended with LF is finished output, not a command
    // line still being echoed. A CRLF line already has nothing after its CR to
    // match below; a bare LF or LF-CR ending must not match either.
    if visible.contains(&b'\n') {
        return false;
    }
    let start = visible
        .iter()
        .rposition(|byte| *byte == b'\r')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    contains_prompt_echo_in_visible_line(&visible[start..])
}

/// Reports whether an escape sequence moves back over, or deletes within, text
/// already on the current line: CSI CUB (`D`), DCH (`P`), ECH (`X`) and ICH
/// (`@`). Together with a literal backspace these are the controls a
/// readline-style line editor uses to rewrite a command line in place, and they
/// are what distinguishes that from a full-line repaint.
///
/// A bare `\r`, `ESC [ G` and a lone `ESC [ K` are deliberately excluded: those
/// return to column one and repaint, which is byte-for-byte what a progress bar
/// emits on every update.
fn is_in_place_line_edit_sequence(bytes: &[u8]) -> bool {
    is_csi_sequence(bytes) && matches!(bytes.last(), Some(b'D' | b'P' | b'X' | b'@'))
}

fn contains_prompt_echo_in_visible_line(line: &[u8]) -> bool {
    for prompt_end in 1..line.len().min(MAX_PROMPT_TAIL_BYTES + 1) {
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
    !matches!(trimmed, b">" | b"#")
}

fn redraws_interactive_prompt_line(line: &[u8]) -> bool {
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
            // The offset in `source` counts the limits in front. Compile the
            // rule as written to report where it fails in the user's text.
            source: RegexBuilder::new()
                .multi_line(true)
                .crlf(true)
                .build(&rule.regex)
                .err()
                .unwrap_or(source),
        })?;
    let style = resolve_capture_references(&description, rule.style, &regex)?;
    Ok(CompiledRule {
        identity,
        description,
        regex,
        search_start_matters: SEARCH_START_ITEMS
            .iter()
            .any(|item| rule.regex.contains(item)),
        style,
        exclusive: rule.exclusive,
    })
}

fn configuration_validation_error(message: String) -> HighlightError {
    // HighlightError was already a public exhaustive enum in 1.x. Reuse its
    // original Regex carrier so validation can reject unsafe configurations
    // without adding a semver-breaking enum variant. What marks the carrier
    // as a validation failure is a source that compiling a rule can never
    // yield, whatever the rule: the error of a match. A compile error would
    // not do, since a rule can be written to fail with any of them.
    HighlightError::Regex {
        description: escape_untrusted(&message),
        source: validation_marker(),
    }
}

/// The PCRE2 code of the error that marks a validation failure,
/// `PCRE2_ERROR_DEPTHLIMIT`.
const VALIDATION_MARKER_CODE: i32 = -53;

/// The source that marks a `HighlightError::Regex` as a validation failure.
fn validation_marker() -> pcre2::Error {
    // A match in byte mode with every option at its default, stopped by a
    // backtracking depth limit of zero that the pattern sets for itself. It
    // asks for nothing a PCRE2 build may have been made without.
    RegexBuilder::new()
        .build("(*LIMIT_DEPTH=0)x")
        .and_then(|regex| regex.is_match(b"x"))
        .err()
        // A PCRE2 that let this match through would leave no marker. A
        // refusal can come up in a running session, at a reload or a profile
        // switch, so it must not panic there: it falls back to an error
        // every PCRE2 gives and is then shown as a rule that failed to
        // compile.
        .unwrap_or_else(|| {
            RegexBuilder::new()
                .build("(?<")
                .expect_err("an unfinished group name is invalid PCRE2")
        })
}

/// Whether `source` is the marker `configuration_validation_error` puts on a
/// validation failure rather than the compile error of a rule.
fn is_validation_marker(source: &pcre2::Error) -> bool {
    matches!(source.kind(), pcre2::ErrorKind::Match) && source.code() == VALIDATION_MARKER_CODE
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
            //
            // An exclusive rule protects its spans from later rules only, so
            // they are marked once all of its own capture groups are applied:
            // group 1 lies inside group 0, which is styled first.
            let mut exclusive_spans = Vec::new();
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
                for cell in &mut styles[start..end] {
                    cell.merge_from(&style);
                }
                if rule.exclusive {
                    exclusive_spans.push(start..end);
                }
            }
            for span in exclusive_spans {
                protected[span].fill(true);
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

/// Largest UTF-8 char-boundary index at or below `index` in `bytes`. In a
/// UTF-8 locale only a valid character has inner positions, so a highlight is
/// never widened over an invalid byte, a stray one from 80 to BF included.
fn floor_char_boundary(bytes: &[u8], mut index: usize) -> usize {
    if terminal_reads_utf8() {
        return validated_char_start(bytes, index).unwrap_or(index);
    }
    while index > 0 && index < bytes.len() && (bytes[index] & 0xC0) == 0x80 {
        index -= 1;
    }
    index
}

/// The start of the valid UTF-8 character that `index` falls strictly inside,
/// if any. An invalid byte is a character of its own.
fn validated_char_start(bytes: &[u8], index: usize) -> Option<usize> {
    if index == 0 || index >= bytes.len() {
        return None;
    }
    (index.saturating_sub(3)..index).find(|&start| {
        valid_utf8_char_width_at(bytes, start).is_some_and(|width| start + width > index)
    })
}

/// Smallest UTF-8 char-boundary index at or above `index` in `bytes`, read
/// the same way as `floor_char_boundary`.
fn ceil_char_boundary(bytes: &[u8], mut index: usize) -> usize {
    if terminal_reads_utf8() {
        return validated_char_start(bytes, index)
            .and_then(|start| valid_utf8_char_width_at(bytes, start).map(|width| start + width))
            .unwrap_or(index);
    }
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
    // Where the last byte that is not UTF-8 ends, once a rule in UTF mode
    // has been refused for one.
    let mut valid_tail = None;

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
            // The step after an empty match landed inside a multi-byte
            // character, where a rule in UTF mode cannot be searched. That is
            // no runaway match: try the next byte.
            Err(error) if error.code() == PCRE2_ERROR_BADUTFOFFSET => {
                last_end += 1;
                continue;
            }
            // A rule in UTF mode is not searched across a byte that is not
            // UTF-8: PCRE2 checks the text from where the search starts to
            // its end and refuses it. That is no fault of the rule, so it is
            // not a match error. No search in front of the last such byte
            // can succeed, so go on behind it. When the text from here on
            // is valid, PCRE2 has read back over such a byte for a
            // lookbehind: try the next byte.
            //
            // A rule whose match depends on where its search starts would
            // match right behind the byte what it matches nowhere else in
            // the middle of a read, so it is not searched there.
            Err(error)
                if PCRE2_ERROR_UTF8_INVALID.contains(&error.code())
                    && rule.search_start_matters =>
            {
                break;
            }
            Err(error) if PCRE2_ERROR_UTF8_INVALID.contains(&error.code()) => {
                let valid_from = *valid_tail.get_or_insert_with(|| {
                    let mut valid_from = last_end;
                    while let Err(invalid) = std::str::from_utf8(&visible[valid_from..]) {
                        let good = invalid.valid_up_to();
                        let rest = visible.len() - valid_from - good;
                        valid_from += good + invalid.error_len().unwrap_or(rest);
                    }
                    valid_from
                });
                last_end = valid_from.max(last_end + 1);
                continue;
            }
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

        let (next_search, skip) = next_match_search(start, end, last_match);
        last_end = next_search;
        if skip {
            continue;
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

/// PCRE2's error code for a search started inside a UTF-8 character, which
/// only a rule in UTF mode reports.
const PCRE2_ERROR_BADUTFOFFSET: i32 = -36;

/// What in the text of a rule makes its match depend on where the search
/// starts: `\G` is true there, `(*NOTEMPTY_ATSTART)` refuses an empty match
/// there, and after `(*COMMIT)` a failure ends the search for every later
/// start. Text that only looks like one of these, such as an escaped
/// backslash in front of a `G`, counts too: that errs on the side of not
/// searching behind an invalid byte.
const SEARCH_START_ITEMS: [&str; 3] = ["\\G", "NOTEMPTY_ATSTART", "(*COMMIT"];

/// PCRE2's error codes for text that is not valid UTF-8, from
/// `PCRE2_ERROR_UTF8_ERR21` to `PCRE2_ERROR_UTF8_ERR1`, which only a rule in
/// UTF mode reports.
const PCRE2_ERROR_UTF8_INVALID: std::ops::RangeInclusive<i32> = -23..=-3;

/// Returns where the search resumes after a match at `start..end`, and whether
/// to skip that match. Same stepping as pcre2's own iterator: an empty match
/// advances one byte so the search makes progress, and an empty match directly
/// after the previous match is skipped. A match that starts after its end
/// (`\K` inside a lookaround, which PCRE2 before 10.38 allows by default) steps
/// the same way: resuming at its end would find it again forever.
fn next_match_search(start: usize, end: usize, last_match: Option<usize>) -> (usize, bool) {
    if start >= end {
        (end + 1, Some(end) == last_match)
    } else {
        (end, false)
    }
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
    let introducer = skip_escape_state_controls(input, start + 1);
    if introducer >= input.len() {
        return input.len();
    }
    if c1_control_at(input, introducer).is_some() {
        return introducer;
    }

    match input[introducer] {
        b'[' => csi_sequence_end(input, introducer + 1),
        b']' => osc_sequence_end(input, introducer + 1),
        b'P' | b'X' | b'^' | b'_' => string_sequence_end(input, introducer + 1),
        0x20..=0x2f => esc_intermediate_sequence_end(input, start),
        // A non-ASCII byte is never part of the escape, so a UTF-8 character
        // after ESC stays whole visible text.
        0x1b | 0x80..=0xff => introducer,
        _ => introducer + 1,
    }
}

/// VT parsers execute C0 controls other than CAN, SUB and ESC, and ignore
/// DEL, without leaving the escape state, so the first byte after such a run
/// still selects the sequence: `ESC NUL ]` opens an OSC.
fn skip_escape_state_controls(bytes: &[u8], mut idx: usize) -> usize {
    while matches!(
        bytes.get(idx),
        Some(0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f)
    ) {
        idx += 1;
    }
    idx
}

fn esc_intermediate_sequence_end(input: &[u8], start: usize) -> usize {
    let mut idx = start + 1;
    while idx < input.len() {
        let byte = input[idx];
        if byte == 0x1b || byte >= 0x80 {
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
    let allows_bel = match control {
        0x9d => true,
        0x90 | 0x98 | 0x9e | 0x9f => false,
        _ => return (start + width).min(input.len()),
    };
    let mut idx = start + width;
    string_terminator_end(input, &mut idx, allows_bel).unwrap_or(input.len())
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
        if input[*idx] == 0x9c
            && !terminal_reads_utf8()
            && !byte_is_inside_utf8_codepoint_or_prefix(input, *idx)
        {
            return Some(*idx + 1);
        }
        if input[*idx] == 0x1b && *idx + 1 < input.len() {
            // ESC \ is ST; any other ESC ends the string and starts a new
            // control, as it does in a terminal's parser.
            return Some(if input[*idx + 1] == b'\\' {
                *idx + 2
            } else {
                *idx
            });
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
    let (modes, enable) = private_mode_sequence(bytes)?;
    let has_alternate_screen_mode =
        private_mode_numbers(&modes).any(|mode| matches!(mode, 47 | 1047 | 1049));
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
        Token::Ansi(bytes) => csi_body(bytes).is_some_and(|body| *body == *b"?2004l"),
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
/// so every classifier must too). Like the tokenizer, it counts the C0
/// controls a terminal runs between ESC and `[`. `None` when `bytes` is not a
/// CSI sequence.
fn csi_introducer_len(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(b"\xc2\x9b") {
        Some(2)
    } else if bytes.starts_with(b"\x9b") {
        Some(1)
    } else if bytes.first() == Some(&0x1b) {
        let introducer = skip_escape_state_controls(bytes, 1);
        (bytes.get(introducer) == Some(&b'[')).then_some(introducer + 1)
    } else {
        None
    }
}

/// The final byte of an escape sequence with no intermediates, such as the
/// `7` of DECSC (`ESC 7`), past the C0 controls a terminal runs after ESC.
fn escape_final_byte(bytes: &[u8]) -> Option<u8> {
    if bytes.first() != Some(&0x1b) {
        return None;
    }
    let final_byte = skip_escape_state_controls(bytes, 1);
    (final_byte + 1 == bytes.len()).then(|| bytes[final_byte])
}

fn is_csi_sequence(bytes: &[u8]) -> bool {
    csi_introducer_len(bytes).is_some()
}

/// What follows the introducer of the CSI sequence `bytes`, as a terminal
/// reads it: it runs a C0 control it meets inside a CSI and ignores DEL, and
/// goes on with the sequence, so neither is part of it. CAN and SUB cancel
/// the sequence and stay, as a last byte that is no final byte. `None` when
/// `bytes` is not a CSI sequence.
fn csi_body(bytes: &[u8]) -> Option<Cow<'_, [u8]>> {
    let body = &bytes[csi_introducer_len(bytes)?..];
    let is_run_or_ignored = |byte: &u8| matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f);
    Some(if body.iter().any(is_run_or_ignored) {
        Cow::Owned(
            body.iter()
                .copied()
                .filter(|byte| !is_run_or_ignored(byte))
                .collect(),
        )
    } else {
        Cow::Borrowed(body)
    })
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
    let mut starts = SgrStartCache::default();

    for token in tokens {
        match token {
            Token::Ansi(bytes) => {
                if let Some(style) = active_style
                    .as_ref()
                    .filter(|style| saves_rendition(bytes) || fills_blank_cells_with(bytes, style))
                {
                    output.extend(style_reset_bytes(style, native_sgr, reset_mode));
                }
                native_sgr.apply_sequence(bytes);
                output.extend_from_slice(bytes);
                if let Some(style) = &active_style {
                    output.extend_from_slice(starts.start(style, color_mode));
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
                            output.extend_from_slice(starts.start(style, color_mode));
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
    let mut starts = SgrStartCache::default();

    for token in tokens {
        match token {
            Token::Ansi(bytes) => {
                let shows_on_blank_line = active_style.as_ref().is_some_and(|style| {
                    style_paints_blank_cells(style)
                        || native_sgr.draws_blank_cells_with_the_foreground()
                });
                // A CR or LF inside a control the terminal runs is executed
                // where it stands and ends the line like a text one, so the
                // highlight closes before the whole control when it would show
                // on the blank line. It is open again right after the control,
                // as it is after every other one.
                let closes_before = active_style.as_ref().is_some_and(|style| {
                    (shows_on_blank_line && control_runs_a_line_end(bytes))
                        || saves_rendition(bytes)
                        || fills_blank_cells_with(bytes, style)
                });
                if let Some(style) = active_style.as_ref().filter(|_| closes_before) {
                    output.extend(style_reset_bytes(style, native_sgr, reset_mode));
                }
                native_sgr.apply_sequence(bytes);
                output.extend_from_slice(bytes);
                if let Some(style) = active_style {
                    output.extend_from_slice(starts.start(style, color_mode));
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
                                // A start only adds to what is drawn, so end
                                // what the previous style set and this lacks.
                                if let Some(previous) = active_style
                                    .as_ref()
                                    .filter(|previous| style_outlasts_start_of(previous, &style))
                                {
                                    output.extend(style_reset_bytes(
                                        previous, native_sgr, reset_mode,
                                    ));
                                }
                                output.extend_from_slice(starts.start(&style, color_mode));
                                *active_style = Some(style);
                            }
                            output.push(*byte);
                        }
                        None => {
                            // A foreground-only overlay stays open across the
                            // spacing between highlighted columns; a style
                            // that shows on blank cells must not paint it,
                            // and neither may the overlay's color where the
                            // program's own rendition shows it there.
                            if active_style.as_ref().is_some_and(|style| {
                                !is_interactive_spacing(*byte)
                                    || style_paints_blank_cells(style)
                                    || native_sgr.draws_blank_cells_with_the_foreground()
                            }) {
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

/// Rendered SGR start sequences of the styles one emit call has used. A
/// style's start is emitted at every transition into it and again after every
/// escape inside its span, so render each one once per call instead. Bounded:
/// past the limit the last entry is replaced.
#[derive(Default)]
struct SgrStartCache {
    entries: Vec<(Style, Vec<u8>)>,
}

impl SgrStartCache {
    const MAX_ENTRIES: usize = 16;

    fn start(&mut self, style: &Style, color_mode: ColorMode) -> &[u8] {
        let index = match self.entries.iter().position(|(cached, _)| cached == style) {
            Some(index) => index,
            None => {
                if self.entries.len() == Self::MAX_ENTRIES {
                    self.entries.pop();
                }
                let start = style.ansi_start_with_mode(color_mode).into_bytes();
                self.entries.push((style.clone(), start));
                self.entries.len() - 1
            }
        };
        &self.entries[index].1
    }
}

fn is_interactive_spacing(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

/// Reports whether `previous` sets a color or attribute that starting `next`
/// on top of it would leave in place, because an SGR start only adds.
fn style_outlasts_start_of(previous: &Style, next: &Style) -> bool {
    (previous.foreground.is_some() && next.foreground.is_none())
        || (previous.background.is_some() && next.background.is_none())
        || (previous.bold && !next.bold)
        || (previous.italic && !next.italic)
        || (previous.underline && !next.underline)
        || (previous.blink && !next.blink)
        || (previous.invert && !next.invert)
        || (previous.strike && !next.strike)
}

/// Reports whether a style shows on blank cells: a background, underline,
/// inverse or strike is drawn on spaces and on the cells a terminal fills when
/// it erases, inserts or scrolls, while a foreground color alone is not.
fn style_paints_blank_cells(style: &Style) -> bool {
    style.background.is_some() || style.underline || style.invert || style.strike
}

fn is_line_end(byte: u8) -> bool {
    matches!(byte, b'\r' | b'\n')
}

/// Reports whether the terminal runs the control token `bytes` and executes a
/// CR or LF inside it. VT parsers execute C0 controls met in the escape, CSI
/// and intermediate states without leaving the sequence (`ESC LF [31m`,
/// `CSI 3 LF 1 m`). Inside an OSC, DCS, SOS, PM or APC payload they are string
/// data and move nothing, so a line end there is no reason to close a
/// highlight; neither is the string a place to write one.
///
/// Only controls whose run is certain count, and only whole ones in ASCII:
/// where a byte from 80 to FF sits in a control, or cuts one short before its
/// final byte, terminals disagree on how it runs. A raw `9B` CSI exists only
/// outside a UTF-8 locale: in one the byte is text (see `c1_control_at`).
/// `C2 9B` never counts: an 8-bit
/// terminal draws the `C2`, and a UTF-8 one may not run it at all. VT and FF
/// are not line ends here: some terminals draw them.
fn control_runs_a_line_end(bytes: &[u8]) -> bool {
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !rest.is_ascii() || !matches!(first, 0x1b | 0x9b) {
        return false;
    }
    let canceled = matches!(rest.last(), Some(0x18 | 0x1a));
    let ends_in = |body: &[u8], finals: std::ops::RangeInclusive<u8>| {
        canceled || body.last().is_some_and(|last| finals.contains(last))
    };
    let has_line_end = |body: &[u8]| body.iter().any(|byte| is_line_end(*byte));
    if first == 0x9b {
        return ends_in(rest, 0x40..=0x7e) && has_line_end(rest);
    }
    let introducer = skip_escape_state_controls(bytes, 1);
    let prefix_line_end = has_line_end(&bytes[1..introducer]);
    match bytes.get(introducer) {
        Some(b'[') => {
            let body = &bytes[introducer + 1..];
            ends_in(body, 0x40..=0x7e) && (prefix_line_end || has_line_end(body))
        }
        Some(0x20..=0x2f) => {
            let body = &bytes[introducer..];
            ends_in(body, 0x30..=0x7e) && (prefix_line_end || has_line_end(body))
        }
        // A string: its payload is data, so only a line end before it counts.
        Some(b']' | b'P' | b'X' | b'^' | b'_') => {
            prefix_line_end
                && (canceled
                    || bytes.ends_with(b"\x1b\\")
                    || (bytes[introducer] == b']' && bytes.ends_with(b"\x07")))
        }
        // An escape whose final byte follows the line ends before it.
        Some(_) => prefix_line_end,
        // Cut short before its final byte.
        None => false,
    }
}

/// Reports whether `bytes` is a control that fills cells with blanks in the
/// current rendition (CSI erase in display or line `J`/`K`, erase, insert or
/// delete characters `X`/`@`/`P`, insert or delete lines `L`/`M`, scroll
/// `S`/`T`, and index, next line and reverse index) while `style`, if left
/// active, would show on them.
fn fills_blank_cells_with(bytes: &[u8], style: &Style) -> bool {
    style_paints_blank_cells(style)
        && (is_index_control(bytes)
            || (is_csi_sequence(bytes)
                && matches!(
                    bytes.last(),
                    Some(b'J' | b'K' | b'X' | b'@' | b'P' | b'L' | b'M' | b'S' | b'T')
                )))
}

/// Reports whether `bytes` makes the terminal save the current rendition:
/// DECSC or CSI s, XTPUSHSGR, or setting mode 1048 or 1049. A highlight left
/// active there would be saved with it and come back with the restore, on
/// text it never matched.
fn saves_rendition(bytes: &[u8]) -> bool {
    saves_cursor(bytes)
        || pushes_rendition(bytes)
        || private_mode_sequence(bytes).is_some_and(|(modes, set)| {
            set && private_mode_numbers(&modes).any(|mode| matches!(mode, 1048 | 1049))
        })
}

/// Reports whether `bytes` is xterm's XTPUSHSGR (`CSI Ps ; Ps # {`, also
/// spelled `CSI Ps ; Ps # p`), which saves the rendition, or the attributes
/// its parameters select, on a stack. The stack itself is not tracked: a
/// terminal without it ignores the push and the pop.
fn pushes_rendition(bytes: &[u8]) -> bool {
    csi_body(bytes).is_some_and(|body| {
        body.strip_suffix(b"#{")
            .or_else(|| body.strip_suffix(b"#p"))
            .is_some_and(|parameters| {
                parameters
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || *byte == b';')
            })
    })
}

/// Reports whether `bytes` uses xterm's SGR stack: XTPUSHSGR (`CSI Pm # {`
/// or `# p`) or XTPOPSGR (`CSI # }` or `# q`), with any parameters and in
/// any introducer, including a raw C1 one the terminal may not run: taking
/// one for a use only costs a prompt-echo off.
///
/// It deliberately also takes these finals after a private marker or
/// another intermediate, as in `CSI ? # {` or `CSI ! # p`, which xterm's
/// parser ignores (its `dec_table` and `csi_ex_table` send `#` to
/// `CASE_CSI_IGNORE`). Such a false match only returns the session to
/// 1.4.1's bytes; narrowing it needs that proof for every terminal with
/// the stack, not only xterm.
fn uses_sgr_stack(bytes: &[u8]) -> bool {
    matches!(bytes.last(), Some(b'{' | b'p' | b'}' | b'q'))
        && csi_body(bytes).is_some_and(|body| {
            [b"#{", b"#p", b"#}", b"#q"]
                .iter()
                .any(|suffix| body.ends_with(*suffix))
        })
}

/// Reports whether `bytes` is index (`ESC D`), next line (`ESC E`) or reverse
/// index (`ESC M`), in the 7-bit form (with any C0 controls a terminal runs
/// after the ESC) or either C1 form. Like a line feed, each scrolls a blank
/// line in when the cursor is on a scroll margin.
fn is_index_control(bytes: &[u8]) -> bool {
    matches!(escape_final_byte(bytes), Some(b'D' | b'E' | b'M'))
        || matches!(
            bytes,
            b"\x84" | b"\x85" | b"\x8d" | b"\xc2\x84" | b"\xc2\x85" | b"\xc2\x8d"
        )
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

/// The program's own rendition, tracked the way an xterm-compatible terminal
/// keeps it: SGR changes it, RIS and DECSTR reset it, and DECSC saves it and
/// DECRC restores it. Mode 1048 saves and restores like DECSC and DECRC, and
/// mode 1049 saves before it switches to the alternate screen and restores
/// after it switches back; modes 47 and 1047 switch screens without saving.
/// CSI s and CSI u with no parameters save and restore like DECSC and DECRC,
/// as they do while left and right margin mode is off, its default; that
/// mode is not tracked. xterm's SGR stack is not tracked either; see
/// `sgr_stack_used`.
#[derive(Clone, Debug, Default)]
struct NativeSgrState {
    rendition: SgrRendition,
    /// The copies saved on the normal and on the alternate screen, which a
    /// terminal keeps apart.
    saved: [SgrRendition; 2],
    alternate_screen: bool,
    /// Whether the program has pushed or popped xterm's SGR stack
    /// (XTPUSHSGR, XTPOPSGR), in any spelling, or a pop without a push seen,
    /// which can pop what was pushed before PrismTTY started. A later pop
    /// may then bring back a rendition this state does not hold, so from
    /// here on the prompt-echo neutralizer writes no off and every write is
    /// 1.4.1's. xterm keeps the stack through RIS and DECSTR, and SGR 0 does
    /// not touch it, so nothing clears this.
    sgr_stack_used: bool,
    /// This state as 1.4.1 tracks it, without the prompt-echo offs written
    /// since the two last agreed; `None` while they agree. It is taken over
    /// at the first use of the SGR stack.
    without_offs: Option<Box<NativeSgrState>>,
    /// Set on the copy in `encoded_csi`, which also takes rendition from the
    /// UTF-8 form `C2 9B` in a UTF-8 locale.
    reads_encoded_csi: bool,
    /// In a UTF-8 locale, this state as a terminal that also runs the UTF-8
    /// form `C2 9B` holds it; `None` while the two agree. No restore after a
    /// highlight is written from it, so a terminal that prints `C2 9B` gets
    /// no new live control from one. Only the prompt-echo offs read it: a
    /// style such an SGR may have set is ended like any other, and on a
    /// terminal that printed the SGR the off ends a style that is not there,
    /// which changes nothing.
    encoded_csi: Option<Box<NativeSgrState>>,
}

impl NativeSgrState {
    fn apply_sequence(&mut self, bytes: &[u8]) {
        let encoded_csi =
            !self.reads_encoded_csi && bytes.starts_with(b"\xc2\x9b") && terminal_reads_utf8();
        if encoded_csi && self.encoded_csi.is_none() {
            self.encoded_csi = Some(Box::new(self.reading_encoded_csi()));
        }
        if let Some(encoded) = self.encoded_csi.as_deref_mut() {
            encoded.apply_sequence(bytes);
        }
        if let Some(without_offs) = self.without_offs.as_deref_mut() {
            without_offs.apply_sequence(bytes);
        }
        if uses_sgr_stack(bytes) {
            if let Some(without_offs) = self.without_offs.take() {
                *self = *without_offs;
            }
            self.sgr_stack_used = true;
            return;
        }
        // In a UTF-8 locale no rendition is taken from the UTF-8 form `C2 9B`:
        // terminals that read UTF-8 do not agree on running it, and a color
        // taken from one that shows it as text would come back as a live
        // `ESC [` control after a highlight. `encoded_csi` keeps it.
        if encoded_csi {
            return;
        }
        match bytes {
            _ if escape_final_byte(bytes) == Some(b'c') => {
                *self = Self {
                    sgr_stack_used: self.sgr_stack_used,
                    without_offs: self.without_offs.take(),
                    ..Self::default()
                };
            }
            _ if saves_cursor(bytes) => self.save(),
            _ if restores_cursor(bytes) => self.restore(),
            _ if is_soft_terminal_reset(bytes) => {
                self.rendition = SgrRendition::default();
                self.saved[usize::from(self.alternate_screen)] = SgrRendition::default();
            }
            _ => match private_mode_sequence(bytes) {
                Some((modes, set)) => {
                    for mode in private_mode_numbers(&modes) {
                        match (mode, set) {
                            (1048, true) => self.save(),
                            (1048, false) => self.restore(),
                            (1049, true) => {
                                self.save();
                                self.alternate_screen = true;
                            }
                            (1049, false) => {
                                self.alternate_screen = false;
                                self.restore();
                            }
                            (47 | 1047, _) => self.alternate_screen = set,
                            _ => {}
                        }
                    }
                }
                None => self.rendition.apply_sequence(bytes),
            },
        }
        if self
            .without_offs
            .as_deref()
            .is_some_and(|without_offs| without_offs.agrees_with(self))
        {
            self.without_offs = None;
        }
        if self
            .encoded_csi
            .as_deref()
            .is_some_and(|encoded| encoded.agrees_with(self))
        {
            self.encoded_csi = None;
        }
    }

    /// A copy of this state that also takes rendition from `C2 9B`.
    fn reading_encoded_csi(&self) -> Self {
        Self {
            rendition: self.rendition.clone(),
            saved: self.saved.clone(),
            alternate_screen: self.alternate_screen,
            sgr_stack_used: self.sgr_stack_used,
            without_offs: None,
            reads_encoded_csi: true,
            encoded_csi: None,
        }
    }

    fn agrees_with(&self, other: &Self) -> bool {
        self.rendition == other.rendition
            && self.saved == other.saved
            && self.alternate_screen == other.alternate_screen
    }

    /// Applies `off`, written in place of a removed prompt-echo SGR, to this
    /// state only, so the state without offs keeps what 1.4.1 tracks.
    fn apply_prompt_echo_off(&mut self, off: &[u8]) {
        if self.without_offs.is_none() {
            self.without_offs = Some(Box::new(self.clone()));
        }
        self.rendition.apply_sequence(off);
        if let Some(encoded) = self.encoded_csi.as_deref_mut() {
            encoded.rendition.apply_sequence(off);
        }
    }

    /// Follows a prompt-echo line written as `written`, which may begin
    /// with PrismTTY's own restore after a highlight, while the state
    /// without offs follows `without_offs`, the line as 1.4.1 wrote it after
    /// that restore. A restore leaves the state it was written from as it
    /// was, and 1.4.1 would have written its own from the state without
    /// offs, so that state skips it.
    fn observe_prompt_echo(&mut self, written: &AnsiChunk, without_offs: &AnsiChunk) {
        let stack_used = self.sgr_stack_used;
        let mut untouched = match self.without_offs.take() {
            Some(untouched) => *untouched,
            None => self.clone(),
        };
        for token in &written.tokens {
            if let Token::Ansi(bytes) = token {
                self.apply_sequence(bytes);
            }
        }
        for token in &without_offs.tokens {
            if let Token::Ansi(bytes) = token {
                untouched.apply_sequence(bytes);
            }
        }
        if self.sgr_stack_used && !stack_used {
            *self = untouched;
        } else if !untouched.agrees_with(self) {
            self.without_offs = Some(Box::new(untouched));
        }
    }

    fn save(&mut self) {
        self.saved[usize::from(self.alternate_screen)] = self.rendition.clone();
    }

    fn restore(&mut self) {
        self.rendition = self.saved[usize::from(self.alternate_screen)].clone();
    }

    fn ansi_start(&self) -> Option<String> {
        self.rendition.ansi_start()
    }

    /// Whether the program's rendition draws on blank cells in the foreground
    /// color: inverse, strike-through, overline, or an underline with no
    /// color of its own. While a prompt-echo off has ended one of these, it
    /// is read from the state without offs, as 1.4.1 reads it: an off only
    /// ever ends one of them, so a highlight closes exactly where 1.4.1
    /// closes it, and an off changes what a restore writes, never when.
    fn draws_blank_cells_with_the_foreground(&self) -> bool {
        if let Some(without_offs) = self.without_offs.as_deref() {
            return without_offs.draws_blank_cells_with_the_foreground();
        }
        let attributes = &self.rendition.attributes;
        [7, 9, 53]
            .iter()
            .any(|group| attributes.contains_key(group))
            || (attributes.contains_key(&4) && self.rendition.underline_color.is_none())
    }

    fn restore_after_interactive_style(&self, style: &Style) -> Vec<u8> {
        self.rendition.restore_after_interactive_style(style)
    }

    /// The SGR left in place of the removed prompt-echo SGR `sequence` on a
    /// terminal showing this rendition: the code from
    /// `PROMPT_ECHO_OFF_CODES` of each group shown here that `sequence`,
    /// read by the parser, leaves off, in ascending order. A group that only
    /// a `C2 9B` SGR may have turned on counts (see `encoded_csi`). Empty
    /// when there is none, when `sequence` is not plain (`is_plain_sgr`),
    /// and once the SGR stack has been used.
    fn prompt_echo_off(&self, sequence: &[u8]) -> Vec<u8> {
        if self.sgr_stack_used || !is_plain_sgr(sequence) {
            return Vec::new();
        }
        let shown = self
            .encoded_csi
            .as_deref()
            .map_or(&self.rendition, |encoded| &encoded.rendition);
        let mut after = shown.clone();
        after.apply_sequence(sequence);
        let codes = PROMPT_ECHO_OFF_CODES
            .iter()
            .filter(|(group, _)| match group {
                None => shown.background.is_some() && after.background.is_none(),
                Some(group) => {
                    shown.attributes.contains_key(group) && !after.attributes.contains_key(group)
                }
            })
            .map(|(_, code)| code.to_string())
            .collect::<Vec<_>>();
        if codes.is_empty() {
            return Vec::new();
        }
        format!("\x1b[{}m", codes.join(";")).into_bytes()
    }
}

/// Reports whether `bytes` saves the cursor and its rendition: DECSC
/// (`ESC 7`), or `CSI s` with no parameters (SCOSC).
fn saves_cursor(bytes: &[u8]) -> bool {
    escape_final_byte(bytes) == Some(b'7') || csi_body(bytes).is_some_and(|body| *body == *b"s")
}

/// Reports whether `bytes` restores the cursor and its rendition: DECRC
/// (`ESC 8`), or `CSI u` with no parameters (SCORC).
fn restores_cursor(bytes: &[u8]) -> bool {
    escape_final_byte(bytes) == Some(b'8') || csi_body(bytes).is_some_and(|body| *body == *b"u")
}

/// Reports whether `bytes` is DECSTR (`CSI ! p`), which xterm runs whatever
/// parameters come before the `!`.
fn is_soft_terminal_reset(bytes: &[u8]) -> bool {
    csi_body(bytes).is_some_and(|body| {
        body.strip_suffix(b"!p").is_some_and(|parameters| {
            parameters
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b';')
        })
    })
}

/// The modes of a DEC private mode set (`CSI ? Pm h`) or reset (`CSI ? Pm l`)
/// and whether it sets them. A terminal ignores the whole sequence once it
/// meets a second private marker or an intermediate byte, so only digits and
/// `;` may stand between the `?` and the final byte.
fn private_mode_sequence(bytes: &[u8]) -> Option<(Cow<'_, [u8]>, bool)> {
    let body = csi_body(bytes)?;
    let (final_byte, modes) = body.strip_prefix(b"?")?.split_last()?;
    let set = match final_byte {
        b'h' => true,
        b'l' => false,
        _ => return None,
    };
    if !modes
        .iter()
        .all(|byte| byte.is_ascii_digit() || *byte == b';')
    {
        return None;
    }
    let modes = 1..=modes.len();
    Some((
        match body {
            Cow::Borrowed(body) => Cow::Borrowed(&body[modes]),
            Cow::Owned(body) => Cow::Owned(body[modes].to_vec()),
        },
        set,
    ))
}

/// The modes of `private_mode_sequence` as the numbers xterm reads. It keeps
/// `XTERM_MAX_CSI_PARAMETERS` of them; past those it ignores each `;` and
/// reads the digits on into the last one, so a later parameter is no mode of
/// its own: behind 29 others, `1;01049` is 101049 and `10;49` is 1049.
fn private_mode_numbers(modes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    modes
        .splitn(XTERM_MAX_CSI_PARAMETERS, |byte| *byte == b';')
        .map(private_mode_number)
}

/// The number that the digits of one mode spell, as a terminal reads it:
/// `01049` is 1049. A `;` among them, which only the last mode that xterm
/// keeps can hold, is skipped. The number is capped at 65535 as xterm caps a
/// parameter, so a longer number is no other mode. Only the zeros in front
/// and six digits behind them are looked at, however long the mode.
fn private_mode_number(mode: &[u8]) -> u32 {
    let mut digits = mode
        .iter()
        .filter(|byte| **byte != b';')
        .skip_while(|digit| **digit == b'0');
    let number = digits
        .by_ref()
        .take(5)
        .fold(0, |number, digit| number * 10 + u32::from(digit - b'0'));
    if digits.next().is_some() {
        65_535
    } else {
        number.min(65_535)
    }
}

/// The colors and attributes that SGR sequences have set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SgrRendition {
    foreground: Option<String>,
    background: Option<String>,
    underline_color: Option<String>,
    attributes: BTreeMap<u16, String>,
}

impl SgrRendition {
    fn apply_sequence(&mut self, bytes: &[u8]) {
        let Some(body) = sgr_parameters(bytes) else {
            return;
        };
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

/// The parameters of `bytes` when it is an SGR: a CSI that ends in `m` and
/// holds only digits, `;` and `:`. A private marker makes it another control
/// (`CSI > Pp ; Pv m` is xterm's key modifier option, XTMODKEYS), and a
/// terminal ignores one with an intermediate byte. A terminal runs a C0
/// control found inside a CSI and ignores DEL, so neither is a parameter.
fn sgr_parameters(bytes: &[u8]) -> Option<Vec<u8>> {
    let (final_byte, body) = bytes[csi_introducer_len(bytes)?..].split_last()?;
    if *final_byte != b'm' {
        return None;
    }
    let body = body
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_control())
        .collect::<Vec<_>>();
    body.iter()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        .then_some(body)
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

fn streaming_split_at(chunk: &AnsiChunk) -> usize {
    let bytes = chunk.bytes();
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
    if let Some(ansi_end) = ansi_token_end_containing(&chunk.tokens, start) {
        start = ansi_end;
    }

    let tail = &bytes[start..];
    if tail.len() <= 512 {
        start
    } else {
        bytes.len()
    }
}

#[cfg(test)]
thread_local! {
    /// Bytes that `incomplete_utf8_start` walked back over on this thread.
    /// The walk covers every trailing byte from 80 to BF, so a caller inside
    /// a loop over the same buffer pays for that tail once per call; stream
    /// tests sum the bytes to see that none does.
    pub(crate) static UTF8_WALK_BACK_BYTES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// If `bytes` ends partway through a multibyte UTF-8 sequence, returns the
/// index of that sequence's lead byte so the partial codepoint can be carried
/// to the next chunk instead of being emitted and split by an SGR escape.
/// Bytes that no later byte can make a character, such as `F0 8E`, are not a
/// partial codepoint: their C1-valued bytes are controls.
fn incomplete_utf8_start(bytes: &[u8]) -> Option<usize> {
    let mut idx = bytes.len();
    let mut continuations = 0usize;
    while idx > 0 && (bytes[idx - 1] & 0xC0) == 0x80 {
        idx -= 1;
        continuations += 1;
    }
    #[cfg(test)]
    UTF8_WALK_BACK_BYTES.with(|walked| walked.set(walked.get() + continuations));
    let lead = *bytes.get(idx.checked_sub(1)?)?;
    let expected = match lead {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    (continuations + 1 < expected
        && (continuations == 0 || utf8_prefix_can_be_completed(&bytes[idx - 1..], expected)))
    .then_some(idx - 1)
}

pub(crate) fn is_incomplete_utf8_prefix(bytes: &[u8]) -> bool {
    incomplete_utf8_start(bytes) == Some(0)
}

/// True when a UTF-8 character starting at `idx` is cut off by the end of
/// `bytes`.
fn is_partial_utf8_char_at_end(bytes: &[u8], idx: usize) -> bool {
    bytes.len() - idx < 4 && is_incomplete_utf8_prefix(&bytes[idx..])
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
    let mut chain_has_string_control = false;
    loop {
        chain_has_string_control |= StringControlKind::at(bytes, current).is_some();
        match scan_escape_for_neutralization(bytes, current) {
            EscapeScan::Restart(restart) => {
                neutralize_control_start(bytes, current);
                current = restart;
            }
            EscapeScan::IncompleteWithinLimit | EscapeScan::IncompleteOversized => {
                // A string ended by the ESC of a later control in the chain is
                // dropped with it, like an unfinished one: emitting the string
                // without that ESC would leave the terminal inside it.
                if chain_has_string_control {
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

/// The end of the control token that `index` falls inside, if any. Walking
/// the chunk's tokens takes one pass and agrees with how the chunk is
/// highlighted, where scanning back from `index` would rescan every earlier
/// string introducer up to a terminator they share.
fn ansi_token_end_containing(tokens: &[Token], index: usize) -> Option<usize> {
    let mut offset = 0usize;
    for token in tokens {
        if offset >= index {
            return None;
        }
        let end = offset + token.as_bytes().len();
        if matches!(token, Token::Ansi(_)) && index < end {
            return Some(end);
        }
        offset = end;
    }
    None
}

fn split_prepared_pending(chunk: AnsiChunk, split_at: usize, pending: &mut Vec<u8>) -> AnsiChunk {
    if split_at >= chunk.bytes.len() {
        pending.clear();
        return chunk;
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
        streaming_split_at(chunk)
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
/// sequence, so a C1 byte inside an unfinished OSC/DCS/SOS/PM/APC cannot be
/// mistaken for a newer top-level control and cause the outer introducer to be
/// emitted prematurely. An ESC that does not start ST ends the string, as it
/// does in a terminal, and the string is carried with the control it starts.
fn incomplete_top_level_escape_start(bytes: &[u8]) -> Option<usize> {
    resume_top_level_escape_scan(bytes, TopLevelScanStop::default(), &mut |_| {})
}

/// Where the top-level escape scan stops between two controls: the offset it
/// reads on from, the first control of the cancellation chain it is in and
/// the start of the control it has just read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TopLevelScanStop {
    idx: usize,
    cancellation_root: Option<usize>,
    control_start: usize,
}

#[cfg(test)]
thread_local! {
    /// Bytes the top-level escape scans on this thread set out to read, from
    /// where each one starts to the end of its buffer. A sanitize split that
    /// rescans its prefix adds the prefix again, so stream tests sum them to
    /// see how often a split rescans a read.
    pub(crate) static TOP_LEVEL_SCAN_BYTES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Runs the scan of `incomplete_top_level_escape_start` on from `from`,
/// passing each stop it makes after `from` to `stop`. Taking `stop` as a
/// trait object keeps one copy of the scan for every caller.
fn resume_top_level_escape_scan(
    bytes: &[u8],
    from: TopLevelScanStop,
    stop: &mut dyn FnMut(TopLevelScanStop),
) -> Option<usize> {
    #[cfg(test)]
    TOP_LEVEL_SCAN_BYTES
        .with(|scanned| scanned.set(scanned.get() + bytes.len().saturating_sub(from.idx)));
    let TopLevelScanStop {
        mut idx,
        mut cancellation_root,
        control_start: _,
    } = from;
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
        stop(TopLevelScanStop {
            idx,
            cancellation_root,
            control_start: start,
        });
    }
    None
}

#[cfg(test)]
fn escape_is_incomplete_at(bytes: &[u8], start: usize) -> bool {
    incomplete_top_level_escape_start(&bytes[start..]) == Some(0)
}

/// Where a sanitize read must be split so the incomplete tail is carried.
/// Raw and UTF-8 C1 strings are top-level controls like their ESC forms, so
/// the top-level scan finds them; a C1 byte inside another string's payload
/// is not a control and must not split that string.
///
/// The prefix before the split is sanitized on its own, and there a character
/// the split cut off looks unfinished: a CSI or ESC that ends in it would be
/// emitted whole, though the rest of the read shows its C1-valued bytes are
/// controls. So split again until that prefix ends with no control open. Only
/// the end of the read can cut off a character that later bytes complete.
/// Before a split, one in text never completes, and the sanitizer already
/// reads its C1-valued bytes as controls, so text that looks like lead bytes,
/// as a legacy encoding does, is not carried with the split.
///
/// The scan of a prefix makes every stop that the scan of the read made at
/// least `TOP_LEVEL_SCAN_LOOKAHEAD` bytes before the prefix ends, in the same
/// state. So each split after the first resumes from the last such stop
/// instead of rescanning the prefix from its start, and keeps every stop it
/// makes for the next split to resume from.
///
/// A run of controls that each end in a character the next one cuts off is
/// stepped back over one control per split, and crafted output can make the
/// run longer than the splits allowed. The text in front of the run must not
/// wait with it: a read is written up to its unfinished end, and a carry that
/// reads keep adding to is dropped once it is oversized. So the first scan
/// notes the last control that starts where cutting the read cannot leave a
/// control open: no control is cancelled there, and the bytes in front of it
/// do not end in a cut-off character, or end in one that began in text after
/// the control before, which then ended without it. Past the allowed splits
/// the read is split there, once a scan of the prefix has confirmed it.
pub(crate) fn incomplete_sanitize_start(bytes: &[u8]) -> Option<usize> {
    let mut stops: Vec<TopLevelScanStop> = Vec::new();
    let mut previous = TopLevelScanStop::default();
    let mut run_start = 0;
    let escape = resume_top_level_escape_scan(bytes, TopLevelScanStop::default(), &mut |stop| {
        let start = stop.control_start;
        // A cut-off character is at most three bytes. Looking at no more
        // keeps a run of raw C1 controls, which look like the rest of a
        // character, from being walked back over at every control.
        let near = start.saturating_sub(3);
        if (previous.cancellation_root.is_none() || start > previous.idx)
            && incomplete_utf8_start(&bytes[near..start])
                .is_none_or(|cut_off| near + cut_off > previous.idx)
        {
            run_start = start;
        }
        previous = stop;
        if stop.idx >= stops.last().map_or(0, |last| last.idx) + SANITIZE_SPLIT_STOP_SPACING {
            stops.push(stop);
        }
    })
    .unwrap_or(bytes.len());
    let utf8 = incomplete_utf8_start(bytes).unwrap_or(bytes.len());
    let mut split = escape.min(utf8);
    if split == bytes.len() {
        return None;
    }
    for pass in 1..=MAX_SANITIZE_SPLIT_PASSES {
        if pass == MAX_SANITIZE_SPLIT_PASSES {
            // Only crafted output keeps cutting off one more control. Carry
            // the run without the text in front of it; the carry limit drops
            // the run once it is oversized.
            split = run_start;
        }
        while stops
            .last()
            .is_some_and(|stop| stop.idx + TOP_LEVEL_SCAN_LOOKAHEAD > split)
        {
            stops.pop();
        }
        let from = stops.last().copied().unwrap_or_default();
        match resume_top_level_escape_scan(&bytes[..split], from, &mut |stop| stops.push(stop)) {
            Some(start) => split = start,
            None => return Some(split),
        }
    }
    // Carrying all of it leaves nothing open.
    Some(0)
}

/// `incomplete_sanitize_start` with each split rescanning its prefix from the
/// start, which it must match.
#[cfg(test)]
fn incomplete_sanitize_start_by_rescans(bytes: &[u8]) -> Option<usize> {
    let mut split = bytes.len();
    for pass in 0..MAX_SANITIZE_SPLIT_PASSES {
        let prefix = &bytes[..split];
        let escape = incomplete_top_level_escape_start(prefix).unwrap_or(split);
        let utf8 = if pass == 0 {
            incomplete_utf8_start(prefix).unwrap_or(split)
        } else {
            split
        };
        let next = escape.min(utf8);
        if next == split {
            return (split < bytes.len()).then_some(split);
        }
        split = next;
    }
    // Past the cap: the last control of the read that starts where no cut
    // can leave a control open, if a scan of the prefix before it agrees.
    let mut previous = TopLevelScanStop::default();
    let mut run_start = 0;
    resume_top_level_escape_scan(bytes, TopLevelScanStop::default(), &mut |stop| {
        let start = stop.control_start;
        if (previous.cancellation_root.is_none() || start > previous.idx)
            && incomplete_utf8_start(&bytes[..start]).is_none_or(|cut_off| cut_off > previous.idx)
        {
            run_start = start;
        }
        previous = stop;
    });
    match incomplete_top_level_escape_start(&bytes[..run_start]) {
        None => Some(run_start),
        Some(_) => Some(0),
    }
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

        // Every byte to the end belongs to this cut-off character, and a later
        // one that cannot continue it shows the character never completes.
        let prefix = &bytes[start..];
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

enum EscapeScan {
    Complete(usize),
    /// The current control was cancelled, or a string control was ended, by
    /// this nested ESC/C1 introducer. Parsing restarts there, so it must not be
    /// swallowed as parameter/final data, and the ended control is carried
    /// with whatever the introducer starts.
    Restart(usize),
    IncompleteWithinLimit,
    IncompleteOversized,
}

fn neutralize_oversized_incomplete_escape(bytes: &mut [u8]) -> bool {
    let mut changed = false;
    let mut idx = 0usize;
    let mut cancellation_root = None;
    let mut chain_has_string_control = false;
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
                if cancellation_root.is_none() {
                    chain_has_string_control = false;
                }
                cancellation_root.get_or_insert(start);
                chain_has_string_control |= StringControlKind::at(bytes, start).is_some();
                idx = restart;
            }
            EscapeScan::IncompleteWithinLimit => {
                // The chain is carried from its root, so cap all of it, not
                // just its last control. A string tail is left to the string
                // filter, whose budget also counts from the root.
                if let Some(root) = cancellation_root {
                    let limit = if chain_has_string_control {
                        MAX_INCOMPLETE_ESCAPE_BYTES
                    } else {
                        MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES
                    };
                    if StringControlKind::at(bytes, start).is_none() && bytes.len() - root > limit {
                        neutralize_restart_chain(bytes, root, start);
                        changed = true;
                    }
                }
                break;
            }
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
    let introducer = skip_escape_state_controls(bytes, start + 1);
    if introducer >= bytes.len() {
        return incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES);
    }
    if c1_control_at(bytes, introducer).is_some() {
        return EscapeScan::Restart(introducer);
    }

    match bytes[introducer] {
        b'[' => scan_csi_for_neutralization(bytes, start, introducer + 1),
        b']' => scan_st_terminated_for_neutralization(bytes, start, introducer + 1, true),
        b'P' | b'X' | b'^' | b'_' => {
            scan_st_terminated_for_neutralization(bytes, start, introducer + 1, false)
        }
        b'N' | b'O' => scan_single_shift_for_neutralization(bytes, start, introducer + 1),
        0x20..=0x2f => scan_esc_intermediate_for_neutralization(bytes, start),
        // In a UTF-8 locale a raw byte from 80 to 9F ends the escape before
        // it, as FF and every other non-ASCII byte do.
        0x80..=0x9f if terminal_reads_utf8() => {
            non_ascii_after_escape_scan(bytes, start, introducer)
        }
        0x1b | 0x80..=0x9f => EscapeScan::Restart(introducer),
        0xa0..=0xff => non_ascii_after_escape_scan(bytes, start, introducer),
        _ => EscapeScan::Complete(introducer + 1),
    }
}

fn scan_esc_intermediate_for_neutralization(bytes: &[u8], start: usize) -> EscapeScan {
    let mut idx = start + 1;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if byte == 0x1b || c1_control_at(bytes, idx).is_some() {
            return EscapeScan::Restart(idx);
        }
        if byte >= 0x80 {
            return non_ascii_after_escape_scan(bytes, start, idx);
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

/// Matches the tokenizer: a non-ASCII byte ends the escape before it, so the
/// character stays visible text. A character cut off by the end of the buffer
/// waits with the escape: a trailing `C2` may still become a UTF-8 C1 control
/// that cancels it, and an ESC split from the character would be read at the
/// end of a buffer, where it is still open.
fn non_ascii_after_escape_scan(bytes: &[u8], start: usize, idx: usize) -> EscapeScan {
    if is_partial_utf8_char_at_end(bytes, idx) {
        return incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES);
    }
    EscapeScan::Complete(idx)
}

fn scan_single_shift_for_neutralization(bytes: &[u8], start: usize, mut idx: usize) -> EscapeScan {
    while idx < bytes.len() {
        if bytes[idx] == 0x1b || c1_control_at(bytes, idx).is_some() {
            return EscapeScan::Restart(idx);
        }
        if matches!(bytes[idx], b'\r' | b'\n') {
            // Like the tokenizer, end the shift before a line ending rather
            // than carry the line ending while waiting for a character.
            return EscapeScan::Complete(idx);
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            return EscapeScan::Complete(idx + width);
        }
        if is_partial_utf8_char_at_end(bytes, idx) {
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
        if is_partial_utf8_char_at_end(bytes, idx) {
            // A C1-valued byte that completes this character is text, not a
            // control that cancels the sequence, so wait for the rest of it.
            break;
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
            || (bytes[idx] == 0x9c
                && !terminal_reads_utf8()
                && !byte_is_inside_utf8_codepoint_or_prefix(bytes, idx))
        {
            return EscapeScan::Complete(idx + 1);
        }
        if bytes[idx] == 0x1b && idx + 1 < bytes.len() {
            if bytes[idx + 1] == b'\\' {
                return EscapeScan::Complete(idx + 2);
            }
            return EscapeScan::Restart(idx);
        }
        idx += 1;
    }
    incomplete_escape_scan_result(bytes, start, MAX_INCOMPLETE_ESCAPE_BYTES)
}

fn scan_csi_for_neutralization(bytes: &[u8], start: usize, mut idx: usize) -> EscapeScan {
    while idx < bytes.len() {
        if bytes[idx] == 0x1b || c1_control_at(bytes, idx).is_some() {
            return EscapeScan::Restart(idx);
        }
        if let Some(width) = valid_utf8_char_width_at(bytes, idx) {
            idx += width;
            continue;
        }
        if is_partial_utf8_char_at_end(bytes, idx) {
            // A C1-valued byte that completes this character is text, not a
            // control that cancels the sequence, so wait for the rest of it.
            break;
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
        if allows_bel && bytes[idx] == 0x07 {
            return EscapeScan::Complete(idx + 1);
        }
        if matches!(bytes[idx], 0x18 | 0x1a) {
            return EscapeScan::Complete(idx + 1);
        }
        if bytes[idx] == 0x9c
            && !terminal_reads_utf8()
            && !byte_is_inside_utf8_codepoint_or_prefix(bytes, idx)
        {
            return EscapeScan::Complete(idx + 1);
        }
        if bytes[idx] == 0x1b && idx + 1 < bytes.len() {
            if bytes[idx + 1] == b'\\' {
                return EscapeScan::Complete(idx + 2);
            }
            return EscapeScan::Restart(idx);
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

/// Removes ANSI escape sequences and returns only visible bytes. A terminal
/// runs the format effectors (backspace through carriage return) it meets
/// inside an escape sequence or a CSI, so those stay: dropping them with the
/// control would join the lines around an escape cut short at a line end.
/// Inside a string control they are payload and go with it.
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut stripped = Vec::new();
    let mut offset = 0;
    for token in tokenize_ansi(input) {
        let start = offset;
        offset += token.as_bytes().len();
        match token {
            Token::Text(_) => extend_without_lone_c2(&mut stripped, input, start..offset),
            Token::Ansi(bytes) if StringControlKind::at(&bytes, 0).is_none() => stripped.extend(
                bytes
                    .iter()
                    .copied()
                    .filter(|byte| matches!(byte, 0x08..=0x0d)),
            ),
            Token::Ansi(_) => {}
        }
    }
    stripped
}

/// Rewrites each `ESC k` as `ESC _`, in place. GNU screen and tmux read
/// `ESC k title ST` as a string that sets the window title; no other terminal
/// does, so it is a plain two-byte escape everywhere else in this file. For
/// sanitizing, where a title a remote device can set is what must not get
/// through, it is read as the APC string it is shaped like: same length, same
/// ways to end, and so found, carried across reads and dropped the same way.
pub(crate) fn read_screen_titles_as_apc(bytes: &mut [u8]) {
    let mut from = 0;
    while let Some(title) = screen_title_final_from(bytes, from) {
        bytes[title] = b'_';
        from = title + 1;
    }
}

/// The index of the `k` of the first `ESC k` at or after `from`, past the C0
/// controls a terminal runs after ESC.
fn screen_title_final_from(bytes: &[u8], mut from: usize) -> Option<usize> {
    while let Some(offset) = bytes[from..].iter().position(|byte| *byte == 0x1b) {
        from += offset + 1;
        let after = skip_escape_state_controls(bytes, from);
        if bytes.get(after) == Some(&b'k') {
            return Some(after);
        }
    }
    None
}

/// Removes string-type escape sequences — OSC, DCS, SOS, PM, APC — while
/// keeping visible text, CSI/SGR, and simple escapes intact. These are the
/// 7-bit ESC and 8-bit C1 sequences a hostile child or remote device can abuse
/// (window-title spoofing, OSC 52 clipboard injection, DCS payload smuggling);
/// everything a normal TUI needs to render survives. An unterminated string
/// sequence at the end of the input is dropped rather than passed through.
/// So are controls that a terminal could read as a string introducer or that
/// could switch it out of UTF-8: an ESC that a non-ASCII byte follows (the
/// byte is kept) with any control that the ESC cancelled, every DOCS sequence
/// (`ESC %`), a CSI that holds a byte from 80 up or that a C1 control cancels,
/// and a CSI that sets, resets or restores DEC private mode 42. The title
/// string of GNU screen and tmux, `ESC k title ST`, is removed as a string
/// too.
pub fn strip_string_escapes(input: &[u8]) -> Vec<u8> {
    let with_titles_as_strings;
    let input = if screen_title_final_from(input, 0).is_some() {
        let mut rewritten = input.to_vec();
        read_screen_titles_as_apc(&mut rewritten);
        with_titles_as_strings = rewritten;
        with_titles_as_strings.as_slice()
    } else {
        input
    };
    let mut sanitized = Vec::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        if input[idx] == 0x1b || c1_control_at(input, idx).is_some() {
            // The chain is emitted up to `copied` as it goes, so a string
            // control, DOCS or CSI that it reaches can still drop all of it.
            let chain_start = sanitized.len();
            let mut copied = idx;
            let mut current = idx;
            loop {
                if let Some((kind, _payload_start)) = StringControlKind::at(input, current) {
                    sanitized.truncate(chain_start);
                    idx = kind.sequence_end(input, current);
                    break;
                }
                // xterm reads a character of U+00FF or below after ESC from
                // the Latin-1 half of its escape table, which mirrors ASCII:
                // U+00DD opens an OSC and U+00D0 a DCS, as the raw bytes do in
                // a Latin-1 xterm. In UTF-8 mode it ignores a C1 control there,
                // UTF-8 encoded or raw, and Alacritty ignores every byte from
                // 80 up, so in `ESC 80 ]` the `]` opens an OSC. Other
                // terminals show the character or run the control, so the ESC
                // run is dropped and its C0 controls are kept. A control that
                // its first ESC cancelled would be completed by the bytes
                // after the run, so it is dropped with the chain that reached
                // it, apart from the C0 controls of the ESC runs in that
                // chain. A C1 control goes on with the chain, as it would after
                // the ESC, so a string control that it reaches still drops the
                // chain from its first control; a C1 string control is left to
                // the chain. A character is read on its own, and one cut off
                // by the end of the input stays whole, as it did while the ESC
                // waited for it, so its C1-valued bytes are not read as
                // controls that drop it.
                if escape_precedes_non_ascii(input, current) {
                    let next = skip_escape_state_controls(input, current + 1);
                    if c1_control_at(input, next).is_some() {
                        push_escape_run_controls(&mut sanitized, input, copied, current, next);
                        copied = next;
                        current = next;
                        continue;
                    }
                    let end = if is_partial_utf8_char_at_end(input, next) {
                        input.len()
                    } else {
                        next
                    };
                    push_escape_run_controls(&mut sanitized, input, copied, current, end);
                    idx = end;
                    break;
                }
                let docs = is_docs_escape(input, current);
                let csi_parameters = csi_parameters_start(input, current);
                match scan_escape_for_neutralization(input, current) {
                    // DOCS selects how xterm decodes its input: `ESC % @`
                    // leaves UTF-8 unless its utf8 resource is "always", and
                    // then the 9D in U+201D opens an OSC. xterm also ignores a
                    // C1 control inside DOCS and reads U+00C0 and U+00C7 as
                    // `@` and `G`, so DOCS is dropped however it ends, with
                    // the chain that reached it. The C0 controls and DEL
                    // inside it and a CAN or SUB that ends it go with it; an
                    // ESC, C1 control or other non-ASCII byte that ends it is
                    // read on its own. A terminal already set to another
                    // encoding is beyond what --sanitize can protect, since
                    // prismtty reads output as UTF-8.
                    EscapeScan::Complete(end) | EscapeScan::Restart(end) if docs => {
                        sanitized.truncate(chain_start);
                        idx = end;
                        break;
                    }
                    // In UTF-8 mode xterm ignores a C1 control inside a CSI
                    // and reads a character from U+00A0 to U+00FF there, which
                    // a lone raw A0-BF byte before a byte from 21 to 7F decodes
                    // to, as the ASCII byte it mirrors. So it reads
                    // `CSI ? 4 C2 B2 h` and `CSI ? 4 C2 80 2 h` as
                    // `CSI ? 42 h`, and Alacritty ignores every byte from 80
                    // up there. A CSI that holds a byte from 80 up, however it
                    // ends, or that a C1 control cancels is dropped with the
                    // chain that reached it, and the C1 control is read as
                    // before.
                    EscapeScan::Restart(restart)
                        if csi_parameters.is_some_and(|parameters| {
                            input[parameters..=restart].iter().any(|byte| *byte >= 0x80)
                        }) =>
                    {
                        sanitized.truncate(chain_start);
                        copied = restart;
                        current = restart;
                    }
                    EscapeScan::Restart(restart) => current = restart,
                    // So is one that sets, resets or restores mode 42
                    // (DECNRCM), which can take xterm out of UTF-8 even when
                    // its utf8 resource is "always", and then the 9D in U+201D
                    // opens an OSC.
                    EscapeScan::Complete(end)
                        if csi_parameters.is_some_and(|parameters| {
                            input[parameters..end].iter().any(|byte| *byte >= 0x80)
                                || changes_decnrcm(&input[parameters..end])
                        }) =>
                    {
                        sanitized.truncate(chain_start);
                        idx = end;
                        break;
                    }
                    EscapeScan::Complete(end) => {
                        extend_without_lone_c2(&mut sanitized, input, copied..end);
                        idx = end;
                        break;
                    }
                    EscapeScan::IncompleteOversized
                        if input[current] == 0x1b
                            && skip_escape_state_controls(input, current + 1) == input.len() =>
                    {
                        // The terminal is still in the escape state after these
                        // controls, so emitting the ESC would let a `]` in the
                        // next read open a string control.
                        push_escape_run_controls(
                            &mut sanitized,
                            input,
                            copied,
                            current,
                            input.len(),
                        );
                        idx = input.len();
                        break;
                    }
                    EscapeScan::IncompleteWithinLimit | EscapeScan::IncompleteOversized => {
                        extend_without_lone_c2(&mut sanitized, input, copied..input.len());
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
        extend_without_lone_c2(&mut sanitized, input, idx..idx + 1);
        idx += 1;
    }
    sanitized
}

/// True when the ESC at `start`, past its escape-state controls, is followed
/// by a byte from 80 up that does not start a C1 string control: a UTF-8 lead
/// byte, a raw Latin-1 byte, an invalid byte or a C1 control, raw or UTF-8
/// encoded. A C1 string control is left to the chain, which drops it with the
/// ESC.
fn escape_precedes_non_ascii(bytes: &[u8], start: usize) -> bool {
    if bytes[start] != 0x1b {
        return false;
    }
    let next = skip_escape_state_controls(bytes, start + 1);
    match c1_control_at(bytes, next) {
        Some((control, _width)) => !is_c1_string_escape_start(control),
        None => bytes.get(next).is_some_and(|byte| *byte >= 0x80),
    }
}

/// True when the ESC at `start`, past its escape-state controls, starts a
/// DOCS sequence, whose first intermediate is `%`.
fn is_docs_escape(bytes: &[u8], start: usize) -> bool {
    bytes[start] == 0x1b && bytes.get(skip_escape_state_controls(bytes, start + 1)) == Some(&b'%')
}

/// Appends the controls of the bare ESC run in `input[from..=current]` that
/// ends with the ESC at `current`, each ESC followed only by escape-state
/// controls up to the next: `input[open..end]` without its ESCs, where `open`
/// starts the run. A bare ESC that the next one cancels leaves the terminal in
/// the escape state too, so the byte at `end` would still select a sequence.
/// A real control before the run, which its first ESC cancelled, is not
/// appended.
fn push_escape_run_controls(
    sanitized: &mut Vec<u8>,
    input: &[u8],
    from: usize,
    current: usize,
    end: usize,
) {
    let mut open = current;
    while let Some(previous) = input[from..open].iter().rposition(|byte| *byte == 0x1b) {
        if skip_escape_state_controls(input, from + previous + 1) != open {
            break;
        }
        open = from + previous;
    }
    for idx in open..end {
        if input[idx] != 0x1b {
            extend_without_lone_c2(sanitized, input, idx..idx + 1);
        }
    }
}

/// Where the parameters of a CSI that starts at `start` begin: after `ESC [`,
/// past the escape-state controls between them, or after a C1 CSI, raw or
/// UTF-8 encoded.
fn csi_parameters_start(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes[start] == 0x1b {
        let introducer = skip_escape_state_controls(bytes, start + 1);
        return (bytes.get(introducer) == Some(&b'[')).then_some(introducer + 1);
    }
    match c1_control_at(bytes, start) {
        Some((0x9b, width)) => Some(start + width),
        _ => None,
    }
}

/// True when `csi`, the bytes of an ASCII CSI after its introducer, sets,
/// resets or restores DEC private mode 42: `?`, parameters split on `;` and
/// `:` of which one is 42, and the final `h`, `l` or `r`, with no
/// intermediate. The terminal runs or ignores C0 controls and DEL inside a
/// CSI without ending it, so they are skipped. xterm reads the digits after
/// its last parameter into it, so after 29 others `4;2` is 42 too.
fn changes_decnrcm(csi: &[u8]) -> bool {
    let Some((final_byte, parameters)) = csi.split_last() else {
        return false;
    };
    if !matches!(final_byte, b'h' | b'l' | b'r') {
        return false;
    }
    let mut parameters = parameters
        .iter()
        .filter(|byte| !matches!(byte, 0x00..=0x1f | 0x7f));
    if parameters.next() != Some(&b'?') {
        return false;
    }
    // The parameter being read, capped at 65535 as xterm caps it, or `None`
    // once a byte other than a digit means it cannot be 42. Past the last
    // parameter that xterm keeps, `;` and `:` are skipped and digits run on.
    let mut value = Some(0u32);
    let mut separators = 0;
    let mut sets_42 = false;
    for byte in parameters {
        match byte {
            b'0'..=b'9' => {
                value = value.map(|value| (value * 10 + u32::from(byte - b'0')).min(65_535));
            }
            b';' | b':' if separators < XTERM_MAX_CSI_PARAMETERS - 1 => {
                sets_42 |= value == Some(42);
                separators += 1;
                value = Some(0);
            }
            b';' | b':' => {}
            b'<'..=b'?' => value = None,
            _ => return false,
        }
    }
    sets_42 || value == Some(42)
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

/// Appends `input[range]` without the `C2` bytes in it that begin no
/// character in `input` (the byte after one is not one from 80 to
/// BF). On a terminal that reads UTF-8 such a `C2` is one invalid byte, and
/// both filters drop it wherever they write it, as text or inside a control
/// they keep: a control they remove after it could otherwise put it next to
/// a raw byte from 80 to 9F, and `C2 9D` is an OSC to a terminal that runs
/// the UTF-8 form of C1 controls. `C2` is the only lead byte such a join can
/// make a control of.
fn extend_without_lone_c2(output: &mut Vec<u8>, input: &[u8], range: std::ops::Range<usize>) {
    if !terminal_reads_utf8() {
        output.extend_from_slice(&input[range]);
        return;
    }
    for idx in range {
        let lone = input[idx] == 0xc2 && !matches!(input.get(idx + 1), Some(0x80..=0xbf));
        if !lone {
            output.push(input[idx]);
        }
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
/// UTF-8 (`C2 80` through `C2 9F`). When the locale says UTF-8 only the
/// encoded form counts: there a raw byte from 80 to 9F that is not part of a
/// character is an isolated invalid byte in ordinary text, as on a terminal
/// that reads UTF-8. Every classifier asks this function, so such a byte
/// starts and ends no control anywhere; inside an ESC, CSI or string that is
/// already open, that control's own rules decide what the byte does.
fn c1_control_at(input: &[u8], start: usize) -> Option<(u8, usize)> {
    let byte = *input.get(start)?;
    if is_c1_control(byte) {
        // On a terminal that reads UTF-8 a raw byte from 80 to 9F is an
        // invalid byte, shown as text: it starts nothing.
        return (!terminal_reads_utf8()).then_some((byte, 1));
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

/// `neutralize_prompt_echo_source_sgr_on` for a terminal showing the default
/// rendition, as the neutralizer's unit tests drive it.
#[cfg(test)]
fn neutralize_prompt_echo_source_sgr(
    input: &[u8],
    previous_visible_tail: &[u8],
    reset_mode: ResetMode,
) -> Vec<u8> {
    neutralize_prompt_echo_source_sgr_on(
        input,
        previous_visible_tail,
        reset_mode,
        &NativeSgrState::default(),
    )
    .bytes
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

    // The marker of a validation failure has to come out the same from every
    // PCRE2 a build may link, so it is a plain byte-mode match error with a
    // fixed code.
    #[test]
    fn validation_marker_is_a_depth_limit_match_error() {
        let marker = super::validation_marker();

        assert!(matches!(marker.kind(), pcre2::ErrorKind::Match), "{marker}");
        assert_eq!(marker.code(), super::VALIDATION_MARKER_CODE);
        assert_eq!(marker.offset(), None);
        assert!(super::is_validation_marker(&marker));
    }

    // Unicode support is optional in PCRE2. A marker built with it would turn
    // into a compile error on a library made without, and the refusal would
    // be shown as a rule that failed to compile again. No test can link such
    // a library, so this reads the construction itself.
    #[test]
    fn validation_marker_asks_for_no_optional_pcre2_feature() {
        let source = include_str!("highlight.rs");
        let start = source
            .find("fn validation_marker() -> pcre2::Error {")
            .expect("the marker function is defined");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("the function ends")];
        let lowered = body.to_ascii_lowercase();

        for option in ["utf", "ucp", "jit", "\\p{", "\\x{"] {
            assert!(
                !lowered.contains(option),
                "the marker uses {option:?}: {body}"
            );
        }
        assert!(body.contains(".build(\"(*LIMIT_DEPTH=0)x\")"), "{body}");
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

    // PCRE2 before 10.38 allows `\K` in a lookaround by default, so a rule like
    // `(?=ab\K)` reports a match on "xx ab" that starts at 5 and ends at 3.
    // Resuming the search at 3 would find the same match forever.
    #[test]
    fn match_stepping_moves_past_a_match_that_starts_after_its_end() {
        assert_eq!(super::next_match_search(5, 3, None), (4, false));
        assert_eq!(super::next_match_search(5, 3, Some(3)), (4, true));

        assert_eq!(super::next_match_search(0, 0, None), (1, false));
        assert_eq!(super::next_match_search(4, 4, Some(4)), (5, true));
        assert_eq!(super::next_match_search(1, 3, Some(0)), (3, false));
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

    // A rule in UTF mode cannot be searched across a byte that is not UTF-8:
    // PCRE2 refuses the text. One stray byte in the output is no fault of the
    // rule, so it must not count as a match error and switch the rule off for
    // the session. The rule goes on matching behind the last such byte of a
    // read, as it did in the read that held the first one, and a rule that
    // does run out of its match budget is still switched off.
    #[test]
    fn invalid_utf8_in_the_output_does_not_switch_a_utf_rule_off() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: umlauts
    regex: '(*UTF)[äöü]'
    color: f#ff0000
  - description: any character between x and y
    regex: '(*UTF)x.y'
    color: f#00ff00
  - description: x behind an e acute
    regex: '(*UTF)(?<=é)x'
    color: f#ffff00
  - description: bounded pathological rule
    regex: '^(a+)+$'
    color: f#0000ff
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        // What a session that has seen nothing else writes for `text`.
        let fresh = |text: &[u8]| StreamingHighlighter::new(highlighter.clone()).push(text);
        let line = "\u{e4} x\u{e9}y \u{e9}x\n".as_bytes();
        let colored = fresh(line);
        assert_eq!(
            colored
                .windows(2)
                .filter(|window| window == b"\x1b[")
                .count(),
            6,
            "{colored:02x?}"
        );

        let mut streaming = StreamingHighlighter::new_with_benchmark(highlighter.clone());
        assert_eq!(streaming.push(line), colored);
        for invalid in [
            b"caf\xe9 au lait\n".as_slice(),
            b"noise \xff here\n",
            b"cut \xe4\xb8 character\n",
            b"\xc0\xaf overlong\n",
            b"\x80\x80\x80\n",
        ] {
            assert_eq!(streaming.push(invalid), invalid);
            assert_eq!(streaming.push(line), colored, "after {invalid:02x?}");
            highlighter
                .try_highlight_bytes(invalid)
                .expect("invalid UTF-8 is not a match error");
        }

        // In the read that holds such bytes, the text behind the last one is
        // searched and the text in front of it is not.
        for front in [
            "\u{f6} one \u{e9}x ".as_bytes(),
            b"\xc3\xb6 one \xff \xc3\xbc two \xc3\xa9x ",
        ] {
            for invalid in [b"\xff".as_slice(), b"\xe9", b"\xe4\xb8", b"\xfe\xff\x80"] {
                let read = [front, invalid, line].concat();
                assert_eq!(
                    streaming.push(&read),
                    [front, invalid, &colored].concat(),
                    "{read:02x?}"
                );
            }
        }
        // A lookbehind may not read back over the byte either.
        assert_eq!(streaming.push(b"\xffx \xe9x\n"), b"\xffx \xe9x\n");
        assert_eq!(streaming.push(line), colored);

        let report = streaming.benchmark_report().expect("benchmark enabled");
        assert!(report.first_rule_error().is_none());

        let mut pathological = vec![b'a'; 8 * 1024];
        pathological.extend_from_slice(b"!\n");
        let _ = streaming.push(&pathological);
        let _ = streaming.push(&pathological);
        let failed = streaming
            .benchmark_report()
            .expect("benchmark enabled")
            .first_rule_error()
            .expect("failed rule recorded");
        assert_eq!(failed.description, "bounded pathological rule");
        assert_eq!(failed.count, 1, "failed rule must not be retried");
        assert_eq!(streaming.push(line), colored);
    }

    // Text that is not UTF-8 is not an error of a UTF-mode rule for the
    // library either: the call that reports match errors returns the text.
    #[test]
    fn fallible_highlight_call_returns_ok_for_a_utf_rule_on_text_that_is_not_utf8() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: umlauts\n    regex: '(*UTF)[\u{e4}\u{f6}\u{fc}]'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");

        for input in [
            b"caf\xe9 au lait\n".as_slice(),
            b"\xc3\xa4 noise \xff here\n",
            b"\xff",
        ] {
            assert_eq!(
                highlighter
                    .try_highlight_bytes(input)
                    .expect("text that is not UTF-8 is not a match error"),
                input,
                "{input:02x?}"
            );
        }
    }

    // A rule in UTF mode that runs out of its match budget is switched off
    // like any other rule, also in a session that has already met bytes that
    // are not UTF-8 and in a read that holds one.
    #[test]
    fn utf_rule_that_exhausts_its_match_limit_is_still_switched_off() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: bounded pathological utf rule
    regex: '(*UTF)^(a+)+$'
    color: f#ff0000
  - description: healthy suffix
    regex: '!'
    color: f#00ff00
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        let mut pathological = vec![b'a'; 8 * 1024];
        pathological.extend_from_slice(b"!\n");

        for front in [b"".as_slice(), b"noise \xff\n"] {
            let mut streaming = StreamingHighlighter::new_with_benchmark(highlighter.clone());
            assert_eq!(streaming.push(b"caf\xe9 au lait\n"), b"caf\xe9 au lait\n");
            assert!(
                streaming
                    .benchmark_report()
                    .expect("benchmark enabled")
                    .first_rule_error()
                    .is_none()
            );

            let read = [front, &pathological].concat();
            let _ = streaming.push(&read);
            let _ = streaming.push(&read);
            let failed = streaming
                .benchmark_report()
                .expect("benchmark enabled")
                .first_rule_error()
                .expect("failed rule recorded");
            assert_eq!(failed.description, "bounded pathological utf rule");
            assert_eq!(failed.count, 1, "failed rule must not be retried");
            assert!(
                highlighter.try_highlight_bytes(&read).is_err(),
                "the match limit is still reported"
            );
        }
    }

    // The search of a UTF-mode rule goes on behind a byte that is not UTF-8.
    // A read that holds nothing else, or that ends in the middle of a
    // character, has to end the search: each step moves on.
    #[test]
    fn utf_rule_search_ends_on_a_read_of_bytes_that_are_not_utf8() {
        let config = PrismConfig::from_chromaterm_yaml(
            r##"
rules:
  - description: any character
    regex: '(*UTF).'
    color: f#ff0000
  - description: nothing or more
    regex: '(*UTF)x*'
    color: f#00ff00
  - description: behind two characters
    regex: '(*UTF)(?<=..)y'
    color: f#0000ff
  - description: line end
    regex: '(*UTF)$'
    color: f#ffff00
"##,
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");

        for unit in [
            b"\xff".as_slice(),
            b"\xc3",
            b"\xe2\x82",
            b"\xa0",
            b"\xf8\xa0",
            b"\xff\n",
            b"y\xff",
        ] {
            for input in [unit.to_vec(), unit.repeat(8192 / unit.len())] {
                let mut report = super::BenchmarkReport::default();
                let _ = highlighter.highlight_bytes_with_reset_mode(
                    &input,
                    Some(&mut report),
                    super::ResetMode::Full,
                );
                assert!(report.first_rule_error().is_none(), "{unit:02x?}");
                highlighter
                    .try_highlight_bytes(&input)
                    .expect("text that is not UTF-8 is not a match error");
            }
        }
    }

    // Going on behind a byte that is not UTF-8 starts a search in the middle
    // of the read. A rule whose match depends on where its search starts
    // would then match there and nowhere else like it: `\G` is true at that
    // point, an empty match is refused there, and a `(*COMMIT)` that an
    // earlier part of the read would have reached is not. Such a rule gets
    // no color in a read that holds such a byte, and stays switched on.
    #[test]
    fn utf_rule_that_depends_on_its_search_start_is_not_searched_behind_an_invalid_byte() {
        for (regex, invalid_reads, valid_read) in [
            (
                r"(*UTF)\Gx",
                [b"\xffx\n".as_slice(), b"\xff x\n", b"x\xffx\n"].as_slice(),
                b"xx ax\n".as_slice(),
            ),
            (
                r"(*UTF)\Gfoo\Kbar",
                [b"\xfffoobar\n".as_slice(), b"noise \xff\nfoobar\n"].as_slice(),
                b"foobar afoobar\n",
            ),
            (
                r"(*UTF)(*NOTEMPTY_ATSTART)(?:|foo)",
                [b"\xfffoo foo\n".as_slice(), b"\xff foo\n"].as_slice(),
                b"foo foo\n",
            ),
            (
                r"(*UTF)x(*COMMIT)y",
                [b"xa \xff xy\n".as_slice(), b"\xffxy\n"].as_slice(),
                b"xy xa xy\n",
            ),
            (
                r"(*UTF)(*NO_START_OPT)(*COMMIT)x",
                [b"\xffx\n".as_slice(), b"a\xff x\n"].as_slice(),
                b"x ax\n",
            ),
        ] {
            let config = PrismConfig::from_chromaterm_yaml(&format!(
                "rules:\n  - description: start\n    regex: '{regex}'\n    color: f#ff0000\n  \
                 - description: plain\n    regex: '(*UTF)z'\n    color: f#00ff00\n"
            ))
            .expect("config parses");
            let highlighter = Highlighter::from_config(config).expect("rules compile");
            let colored = StreamingHighlighter::new(highlighter.clone()).push(valid_read);
            assert!(
                colored.windows(2).any(|window| window == b"\x1b["),
                "{regex}: {colored:02x?}"
            );

            let mut streaming = StreamingHighlighter::new_with_benchmark(highlighter.clone());
            for invalid in invalid_reads {
                assert_eq!(
                    &streaming.push(invalid),
                    invalid,
                    "{regex} on {invalid:02x?}"
                );
                highlighter
                    .try_highlight_bytes(invalid)
                    .expect("text that is not UTF-8 is not a match error");
                assert_eq!(
                    streaming.push(valid_read),
                    colored,
                    "{regex} after {invalid:02x?}"
                );
            }
            assert!(
                streaming
                    .benchmark_report()
                    .expect("benchmark enabled")
                    .first_rule_error()
                    .is_none(),
                "{regex}"
            );
            // A rule beside it that does not depend on its search start is
            // still searched behind the byte.
            let behind = streaming.push(b"z \xff z\n");
            assert!(
                behind.starts_with(b"z \xff \x1b["),
                "{regex}: {behind:02x?}"
            );
        }
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

    // A VT parser executes C0 controls (except CAN, SUB and ESC) and ignores
    // DEL without leaving the escape state, so `ESC NUL ]` still opens an OSC.
    #[test]
    fn sanitize_strips_string_escapes_after_escape_state_controls() {
        for control in [0x00u8, 0x01, b'\r', b'\n', 0x19, 0x1c, 0x1f, 0x7f] {
            for (introducer, terminator) in [
                (b"]52;c;U0VDUkVU".as_slice(), b"\x07".as_slice()),
                (b"]2;TITLE".as_slice(), b"\x1b\\".as_slice()),
                (b"P1$qm".as_slice(), b"\x1b\\".as_slice()),
                (b"_apc".as_slice(), b"\x1b\\".as_slice()),
            ] {
                let input = [
                    b"a\x1b".as_slice(),
                    &[control],
                    introducer,
                    terminator,
                    b"b",
                ]
                .concat();
                assert_eq!(
                    super::strip_string_escapes(&input),
                    b"ab",
                    "string survived after {control:#04x}: {input:02x?}"
                );
                let unterminated = [b"a\x1b".as_slice(), &[control, control], introducer].concat();
                assert_eq!(super::strip_string_escapes(&unterminated), b"a");
                assert_eq!(super::incomplete_sanitize_start(&unterminated), Some(1));
            }
            let split = [b"ok\x1b".as_slice(), &[control]].concat();
            assert_eq!(super::incomplete_sanitize_start(&split), Some(2));
        }

        // CAN and SUB still cancel the escape, so the bracket is plain text.
        assert_eq!(super::strip_string_escapes(b"\x1b\x18]x"), b"\x1b\x18]x");
        assert_eq!(super::strip_string_escapes(b"\x1b\x1a]x"), b"\x1b\x1a]x");

        // An escape left open by a long run of controls is not emitted, since
        // the terminal would still be waiting to read a string introducer.
        // Neither is a bare escape that it cancelled, which would leave the
        // terminal in the same state, nor a control that the run cancelled,
        // which the next read could complete.
        let controls = vec![0; super::MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES + 1];
        for (prefix, expected) in [
            (b"ok\x1b".as_slice(), b"ok".as_slice()),
            (b"ok\x1b\x1b".as_slice(), b"ok".as_slice()),
            (b"ok\x1b\x00\x1b".as_slice(), b"ok\x00".as_slice()),
            (b"ok\x1b[1\x1b\x1b".as_slice(), b"ok".as_slice()),
            (b"ok\x1b[?4\x1b\r\x1b".as_slice(), b"ok\r".as_slice()),
            (b"ok\x9b1\x1b".as_slice(), b"ok".as_slice()),
            (b"ok\x1b(\x1b".as_slice(), b"ok".as_slice()),
        ] {
            let oversized = [prefix, &controls].concat();
            assert_eq!(
                super::strip_string_escapes(&oversized),
                [expected, &controls].concat(),
                "open ESC was emitted for {prefix:02x?}"
            );
        }
    }

    // xterm reads a character of U+00FF or below after ESC from the upper half
    // of its escape table, which mirrors ASCII: ESC U+00DD opens an OSC, U+00D0
    // a DCS, U+00D8 an SOS, U+00DE a PM, U+00DF an APC, U+00DB a CSI and
    // U+00DC is ST, and a Latin-1 xterm reads the raw bytes the same way.
    // Escape-state controls and a bare ESC that the next one cancels leave the
    // terminal in the escape state, so none of those ESCs may reach it. Other
    // terminals show the character, so it and the controls stay visible.
    #[test]
    fn sanitize_drops_escapes_that_a_latin1_character_would_complete() {
        for code in [0xd0u8, 0xd8, 0xdb, 0xdc, 0xdd, 0xde, 0xdf, 0xa0, 0xc0, 0xff] {
            let encoded = char::from(code).to_string().into_bytes();
            for character in [encoded.as_slice(), &[code]] {
                for (escapes, kept) in [
                    (b"\x1b".as_slice(), b"".as_slice()),
                    (b"\x1b\x00", b"\x00"),
                    (b"\x1b\x7f\r\n", b"\x7f\r\n"),
                    (b"\x1b\x1b", b""),
                    (b"\x1b\x00\x1b\x01", b"\x00\x01"),
                    // A real control that a bare ESC cancels is dropped too,
                    // or the character would complete it, and the controls
                    // of the run stay.
                    (b"\x1b[1\x1b", b""),
                    (b"\x1b(\x1b\x1b\x00", b"\x00"),
                    (b"\x9b1\x1b", b""),
                    (b"\x1b[?4\x1b\r", b"\r"),
                    // A string control that the bare ESC ends is dropped.
                    (b"\x1b]0;x\x1b", b""),
                ] {
                    let input = [b"a", escapes, character, b"0;title\x07b"].concat();
                    let expected = [b"a", kept, character, b"0;title\x07b"].concat();
                    assert_eq!(
                        super::strip_string_escapes(&input),
                        expected,
                        "ESC was emitted before {character:02x?} in {input:02x?}"
                    );
                }
            }
        }

        // The character may be cut off by the end of the input, or follow a
        // run of controls past the limit for an open escape. A cut-off
        // character stays whole, though its continuation bytes look like C1
        // controls.
        for (input, expected) in [
            (b"a\x1b\xc3".as_slice(), b"a\xc3".as_slice()),
            (b"a\x1b\x00\xc2", b"a\x00\xc2"),
            (b"a\x1b\xe2\x9d", b"a\xe2\x9d"),
            (b"a\x1b\x07\xf0\x9f\x8e", b"a\x07\xf0\x9f\x8e"),
            (b"a\x1b\x1b\xf0\x9f\x98", b"a\xf0\x9f\x98"),
            (b"a\x1b[1\x1b\xe2\x90", b"a\xe2\x90"),
        ] {
            assert_eq!(super::strip_string_escapes(input), expected, "{input:02x?}");
        }
        let controls = vec![0; super::MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES + 1];
        for character in [b"\xc3\x9d".as_slice(), b"\xdd", b"\xc3", b"\xe2\x9d"] {
            let input = [b"ok\x1b\x1b".as_slice(), &controls, character].concat();
            assert_eq!(
                super::strip_string_escapes(&input),
                [b"ok".as_slice(), &controls, character].concat()
            );
        }

        // A C1 string control is dropped with the ESC, raw or UTF-8 encoded,
        // and ESC followed by an ASCII byte is unchanged.
        for input in [
            b"a\x1b\x9d0;t\x07b".as_slice(),
            b"a\x1b\xc2\x9d0;t\x07b",
            b"a\x1b\x00\xc2\x90q\x1b\\b",
            b"a\x1b\x1b\x980;t\x1b\\b",
        ] {
            assert_eq!(super::strip_string_escapes(input), b"ab", "{input:02x?}");
        }
        for input in [
            b"a\x1b7b\x1b\x008\x1b(Bc".as_slice(),
            b"a\x1b",
            b"a\x1b\x00",
        ] {
            assert_eq!(super::strip_string_escapes(input), input, "{input:02x?}");
        }
    }

    // In UTF-8 mode xterm ignores a C1 control in the escape state, decoded
    // from UTF-8 or from a lone raw byte that an ASCII byte follows, and
    // Alacritty ignores every byte from 80 up there, so the escape goes on
    // past it: ESC C2 80 ] and ESC 80 ] open an OSC, as does ESC C2 80 U+00DD.
    // VTE-style parsers read the control as cancelling the ESC instead. The
    // ESC run is dropped as before a Latin-1 character and the control is
    // read on its own: one that starts a string is dropped with its string,
    // as before, and any other one is kept.
    #[test]
    fn sanitize_drops_escapes_that_a_c1_control_follows() {
        for control in 0x80u8..=0x9f {
            for encoded in [[0xc2, control].as_slice(), &[control]] {
                for (escapes, kept) in [
                    (b"\x1b".as_slice(), b"".as_slice()),
                    (b"\x1b\x00", b"\x00"),
                    (b"\x1b\x7f\r\n", b"\x7f\r\n"),
                    (b"\x1b\x1b", b""),
                    (b"\x1b\x00\x1b\x01", b"\x00\x01"),
                ] {
                    let input = [b"a", escapes, encoded, b"]0;title\x1b\\b"].concat();
                    let expected = if super::is_c1_string_escape_start(control) {
                        b"ab".to_vec()
                    } else {
                        [b"a", kept, encoded, b"]0;title\x1b\\b"].concat()
                    };
                    assert_eq!(
                        super::strip_string_escapes(&input),
                        expected,
                        "{input:02x?}"
                    );
                }
            }
        }

        for (input, expected) in [
            (
                b"a\x1b\xc2\x80]0;title\x07b".as_slice(),
                b"a\xc2\x80]0;title\x07b".as_slice(),
            ),
            (b"a\x1b\x80]0;title\x07b", b"a\x80]0;title\x07b"),
            (b"a\x1b\xc2\x9b]0;t\x07b", b"a\xc2\x9b]0;t\x07b"),
            (b"a\x1b\x9b]0;t\x07b", b"a\x9b]0;t\x07b"),
            (b"a\x1b\xc2\x9c]0;t\x07b", b"a\xc2\x9c]0;t\x07b"),
            (b"a\x1b\xc2\x8e]0;t\x07b", b"a\xc2\x8e]0;t\x07b"),
            (b"a\x1b\x00\xc2\x80]0;t\x07b", b"a\x00\xc2\x80]0;t\x07b"),
            (b"a\x1b\x00\x8e]0;t\x07b", b"a\x00\x8e]0;t\x07b"),
            (b"a\x1b\x9b31mb", b"a\x9b31mb"),
            (b"a\x1b\x80\x81]0;t\x07b", b"a\x80\x81]0;t\x07b"),
            (
                b"a\x1b\xc2\x80\xc2\x81]0;t\x07b",
                b"a\xc2\x80\xc2\x81]0;t\x07b",
            ),
            (
                b"a\x1b\xc2\x80\xc3\x9d0;t\x07b",
                b"a\xc2\x80\xc3\x9d0;t\x07b",
            ),
            (b"a\x1b\xc2\x9b31mb", b"a\xc2\x9b31mb"),
            // A real control that a bare ESC cancels is dropped too, or the
            // bytes after the control would complete it, and so is a string
            // control that the bare ESC ends.
            (b"a\x1b[1\x1b\xc2\x80]0;t\x07b", b"a\xc2\x80]0;t\x07b"),
            (b"a\x1b(\x1b\x1b\x00\xc2\x8e]b", b"a\x00\xc2\x8e]b"),
            (b"a\x9b1\x1b\xc2\x80]0;t\x07b", b"a\xc2\x80]0;t\x07b"),
            (b"a\x1b[?4\x1b\x802hb", b"a\x802hb"),
            (b"a\x1b]0;x\x1b\xc2\x80]0;t\x07b", b"a\xc2\x80]0;t\x07b"),
            // A string control is still dropped with the whole chain.
            (b"a\x1b[1\x1b\xc2\x9d0;t\x07b", b"ab"),
            (b"a\x1b\xc2\x80\xc2\x9d0;t\x07b", b"a\xc2\x80b"),
            (b"a\x1b\x80\x9d0;t\x07b", b"a\x80b"),
            // Cut off by the end of the input, the character stays whole.
            (b"a\x1b\xc2", b"a\xc2"),
            (b"a\x1b\x1b\x00\xc2", b"a\x00\xc2"),
        ] {
            assert_eq!(super::strip_string_escapes(input), expected, "{input:02x?}");
        }

        let controls = vec![0; super::MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES + 1];
        for control in [
            b"\xc2\x80".as_slice(),
            b"\xc2\x9b",
            b"\xc2\x8e",
            b"\x80",
            b"\x9b",
            b"\x8e",
        ] {
            let input = [b"ok\x1b\x1b".as_slice(), &controls, control, b"]0;t\x07"].concat();
            assert_eq!(
                super::strip_string_escapes(&input),
                [b"ok".as_slice(), &controls, control, b"]0;t\x07"].concat()
            );
        }
    }

    // A C1 control after a dropped ESC run goes on with the chain whose
    // control the run's first ESC cancelled, as it would after the ESC. That
    // control is dropped, or the bytes after the run would complete it, and a
    // string control that a CSI or single shift there reaches drops the chain
    // from its first control. A chain that ends without one keeps the
    // controls of the run and the C1 control's sequence.
    #[test]
    fn sanitize_keeps_the_chain_of_a_c1_control_after_a_dropped_escape() {
        for root in [
            b"\x1b[1".as_slice(),
            b"\x1b(",
            b"\x9b1",
            b"\xc2\x9b1",
            b"\x8e",
            b"\x8f",
        ] {
            for (escapes, kept) in [
                (b"\x1b".as_slice(), b"".as_slice()),
                (b"\x1b\x00", b"\x00"),
                (b"\x1b\x1b\r", b"\r"),
            ] {
                for (control, end) in [
                    (b"\xc2\x9b1".as_slice(), b"m".as_slice()),
                    (b"\x9b1", b"m"),
                    (b"\xc2\x8e", b"x"),
                    (b"\x8e", b"x"),
                    (b"\xc2\x8f\x00", b"x"),
                    (b"\x8f", b"x"),
                ] {
                    let chain = [root, escapes, control].concat();
                    for (string, after) in [
                        (b"\x1b]0;t\x07".as_slice(), b"b".as_slice()),
                        (b"\x9d0;t\x07", b"b"),
                        (b"\xc2\x90q\x1b\\", b"b"),
                        (b"\x1b\x00\x1b_x\x1b\\", b"b"),
                        // An unterminated string at the end of the input.
                        (b"\x1b]0;t", b""),
                    ] {
                        let input = [b"a".as_slice(), &chain, string, after].concat();
                        assert_eq!(
                            super::strip_string_escapes(&input),
                            [b"a", after].concat(),
                            "{input:02x?}"
                        );
                    }
                    let input = [b"a".as_slice(), &chain, end, b"b"].concat();
                    assert_eq!(
                        super::strip_string_escapes(&input),
                        [b"a", kept, control, end, b"b"].concat(),
                        "{input:02x?}"
                    );
                }
            }
        }

        // The chain may pass more than one dropped ESC run, and a C1 control
        // that starts no sequence ends it.
        for (input, expected) in [
            (
                b"a\x1b[1\x1b\x9b1\x1b\xc2\x8e\x1b]0;t\x07b".as_slice(),
                b"ab".as_slice(),
            ),
            (b"a\x1b[1\x1b\x9b1\x1b\xc2\x8exb", b"a\xc2\x8exb"),
            (b"a\x1b(\x1b\x80\x1b]0;t\x07b", b"a\x80b"),
        ] {
            assert_eq!(super::strip_string_escapes(input), expected, "{input:02x?}");
        }

        // A later dropped ESC run keeps the C0 controls that an earlier one
        // in the chain kept, since the terminal runs them either way.
        for (input, expected) in [
            (
                b"a\x1b\r\xc2\x8e\x1b\xc3\x9eb".as_slice(),
                b"a\r\xc3\x9eb".as_slice(),
            ),
            (b"a\x1b\n\x9b1\x1b\x80b", b"a\n\x80b"),
            (b"a\x1b\r\x8e\x1b\x00\x1b\xc2\x9b1mb", b"a\r\x00\xc2\x9b1mb"),
        ] {
            assert_eq!(super::strip_string_escapes(input), expected, "{input:02x?}");
        }
        let controls = vec![0; super::MAX_INCOMPLETE_ESCAPE_BYTES + 1];
        let input = [b"a\x1b\r\xc2\x8e\x1b".as_slice(), &controls].concat();
        assert_eq!(
            super::strip_string_escapes(&input),
            [b"a\r".as_slice(), &controls].concat()
        );
    }

    // DOCS selects how xterm decodes its input: after `ESC % @` it reads
    // U+201D (E2 80 9D) byte by byte, and the 9D opens an OSC. xterm ignores
    // a C1 control inside DOCS and reads U+00C0 and U+00C7 as its `@` and `G`,
    // so --sanitize drops every DOCS however it ends, with the chain that
    // reached it. The C0 controls and DEL inside it and a CAN or SUB that
    // ends it go with it; an ESC, C1 control or other non-ASCII byte that
    // ends it is read on its own.
    #[test]
    fn sanitize_drops_docs_sequences() {
        let rest = "\u{201d}0;title\x07b".as_bytes();
        for docs in [
            b"\x1b%@".as_slice(),
            b"\x1b%G",
            b"\x1b%8",
            b"\x1b%/G",
            b"\x1b%/@",
            b"\x1b% @",
            b"\x1b\x00%\r@",
            b"\x1b\x7f%\x00/\x00G",
            b"\x1b\x1b%G",
            b"\x1b[1\x1b%@",
            b"\x1b(\x1b\x00%@",
            b"\x9b1\x1b%@",
            b"\xc2\x8e\x1b%G",
            b"\x1b%\x18",
            b"\x1b%\x1a",
            b"\x1b%\r/\n\x18",
        ] {
            let input = [b"a", docs, rest].concat();
            assert_eq!(
                super::strip_string_escapes(&input),
                [b"a", rest].concat(),
                "{input:02x?}"
            );
        }

        for (input, expected) in [
            (b"a\x1b%\xc3\x80b".as_slice(), b"a\xc3\x80b".as_slice()),
            (b"a\x1b%\xc3\x87b", b"a\xc3\x87b"),
            (b"a\x1b%/\xc3\x87b", b"a\xc3\x87b"),
            (b"a\x1b%\xc0b", b"a\xc0b"),
            (b"a\x1b%\xc2\x80@b", b"a\xc2\x80@b"),
            (b"a\x1b%\x80@b", b"a\x80@b"),
            (b"a\x1b%\x9b1mb", b"a\x9b1mb"),
            (b"a\x1b%\xc2\x9d0;t\x07b", b"ab"),
            (b"a\x1b%\x1b[1mb", b"a\x1b[1mb"),
            (b"a\x1b%\x1b]0;t\x07b", b"ab"),
            (b"a\x1b%\x1b%Gb", b"ab"),
        ] {
            assert_eq!(super::strip_string_escapes(input), expected, "{input:02x?}");
        }

        // Cut off by the end of a read, it is carried like any other
        // incomplete escape, so the final in the next read drops it.
        for input in [b"a\x1b%".as_slice(), b"a\x1b%/", b"a\x1b\x00%\x00"] {
            assert_eq!(super::incomplete_sanitize_start(input), Some(1));
        }

        // Another first intermediate starts no DOCS, and the other filters
        // handle DOCS as before.
        for input in [b"a\x1b(%5b".as_slice(), b"a\x1b %Gb", b"a\x1b#8b"] {
            assert_eq!(super::strip_string_escapes(input), input, "{input:02x?}");
        }
        let docs = b"a\x1b%@b\x1b%Gc";
        assert_eq!(super::strip_ansi(docs), b"abc");
        let mut streaming = StreamingHighlighter::new(
            Highlighter::from_config(PrismConfig::default()).expect("empty config"),
        );
        let mut highlighted = streaming.push(docs);
        highlighted.extend(streaming.finish());
        assert_eq!(highlighted, docs);
    }

    // In UTF-8 mode xterm ignores a C1 control inside a CSI, UTF-8 encoded or
    // a lone raw byte that an ASCII byte follows, reads a character from
    // U+00A0 to U+00FF there, or a lone raw A0-BF byte, as the ASCII byte it
    // mirrors, and adds a combining character to the cell before without
    // reading it. So it reads `CSI ? 4 C2 B2 h`, `CSI ? 4 B2 h` and
    // `CSI ? 4 C2 80 2 h` as `CSI ? 42 h`. Alacritty ignores every byte from
    // 80 up inside a CSI. --sanitize drops a CSI that holds such a byte or
    // that a C1 control cancels, with the chain that reached it, and reads
    // the C1 control on its own as before.
    #[test]
    fn sanitize_drops_csis_that_hold_bytes_from_80_up() {
        for introducer in [
            b"\x1b[".as_slice(),
            b"\x1b\x00[",
            b"\x1b\x7f\r[",
            b"\x9b",
            b"\xc2\x9b",
        ] {
            for inside in [
                "\u{b2}".as_bytes(),
                b"\xb2",
                "\u{e8}".as_bytes(),
                b"\xe8",
                "\u{a0}".as_bytes(),
                b"\xff",
                b"\xc3",
                "\u{301}".as_bytes(),
                "\u{201d}".as_bytes(),
            ] {
                let input = [b"a", introducer, b"?4", inside, b"2h\x1b[mb"].concat();
                assert_eq!(
                    super::strip_string_escapes(&input),
                    b"a\x1b[mb",
                    "{input:02x?}"
                );
            }
            for control in 0x80u8..=0x9f {
                for encoded in [[0xc2, control].as_slice(), &[control]] {
                    let input = [b"a", introducer, b"?4", encoded, b"2h\x1b\\b"].concat();
                    let expected = if super::is_c1_string_escape_start(control) {
                        b"ab".to_vec()
                    } else {
                        [b"a", encoded, b"2h\x1b\\b"].concat()
                    };
                    assert_eq!(
                        super::strip_string_escapes(&input),
                        expected,
                        "{input:02x?}"
                    );
                }
            }
        }

        for (input, expected) in [
            // A Latin-1 character can be the final byte to xterm.
            (b"a\x1b[1\xc3\x9e0;t\x07b".as_slice(), b"a\x07b".as_slice()),
            // An ESC that ends the CSI is read as before, a CAN or SUB that
            // ends it goes with it, and so do the controls that the chain
            // cancelled on the way.
            (b"a\x1b[?4\xc2\xb2\x1b[mb", b"a\x1b[mb"),
            (b"a\x1b[1\xc3\xa9\x1b]0;t\x07b", b"ab"),
            (b"a\x1b[1\xc3\xa9\x18b", b"ab"),
            (b"a\x9b1\xb2\x1ab", b"ab"),
            (b"a\x1b(\x1b[1\xc3\xa9mb", b"ab"),
            (b"a\x1b[1\x1b[?4\xc2\xb2hb", b"ab"),
            (b"a\x8e\x1b\x00[\xb2mb", b"ab"),
            (b"a\x1b[31\x9b1mb", b"a\x9b1mb"),
            (b"a\x1b[1\xc2\x9d0;t\x07b", b"ab"),
            // ASCII CSIs, with controls inside them, are kept.
            (
                b"a\x1b[1;31m\x9b0m\xc2\x9b2J\x1b[\r1mb",
                b"a\x1b[1;31m\x9b0m\xc2\x9b2J\x1b[\r1mb",
            ),
        ] {
            assert_eq!(super::strip_string_escapes(input), expected, "{input:02x?}");
        }

        // Cut off by the end of a read, it is carried like any other
        // incomplete CSI, so the next read decides.
        for input in [
            b"a\x1b[1\xc3\xa9".as_slice(),
            b"a\x9b?4\xc2",
            b"a\x1b[1\xb2",
        ] {
            assert_eq!(super::incomplete_sanitize_start(input), Some(1));
        }
    }

    // An ESC run dropped because a Latin-1 character or a C1 control follows
    // it would leave the control that its first ESC cancelled open for the
    // bytes after the run: `ESC [ ? 4 ESC C2 B2 h` would reach xterm as
    // `CSI ? 4 C2 B2 h`, DECSET 42, and `ESC [ 1 ESC C3 9E` as `CSI 1 ^`.
    // --sanitize drops that control with the run and keeps the run's
    // controls.
    #[test]
    fn sanitize_drops_controls_that_a_dropped_escape_cancelled() {
        let rest = "h\u{201d}0;t\x07b".as_bytes();
        for root in [
            b"\x1b[?4".as_slice(),
            b"\x1b[1",
            b"\x1b\x00[1",
            b"\x9b?4",
            b"\xc2\x9b1",
            b"\x1b(",
            b"\x1b#",
            b"\x1b ",
            b"\x1bN",
            b"\x1bO",
            b"\x8e",
            b"\xc2\x8f",
        ] {
            for (escapes, kept) in [
                (b"\x1b".as_slice(), b"".as_slice()),
                (b"\x1b\x00", b"\x00"),
                (b"\x1b\x1b\r", b"\r"),
            ] {
                for follower in [
                    "\u{b2}".as_bytes(),
                    b"\xb2",
                    "\u{de}".as_bytes(),
                    "\u{e8}".as_bytes(),
                    b"\xc2\x80",
                    b"\x80",
                    b"\xc2\x8e",
                    b"\x9b",
                ] {
                    let input = [b"a", root, escapes, follower, rest].concat();
                    assert_eq!(
                        super::strip_string_escapes(&input),
                        [b"a", kept, follower, rest].concat(),
                        "{input:02x?}"
                    );
                }
            }
        }
    }

    // DECSET, DECRST and XTRESTORE of DEC private mode 42 (DECNRCM) make
    // xterm swap its UTF-8 mode out, even when its utf8 resource is "always",
    // and then the 9D in U+201D opens an OSC. --sanitize drops them in every
    // CSI form, wherever 42 stands among the parameters.
    #[test]
    fn sanitize_drops_decnrcm_changes() {
        let rest = "\u{201d}0;t\x07b".as_bytes();
        for introducer in [b"\x1b[".as_slice(), b"\x1b\x00[", b"\x9b", b"\xc2\x9b"] {
            for parameters in [
                b"?42".as_slice(),
                b"?042",
                b"?0042",
                b"?1;42",
                b"?42;1049",
                b"?25;0042;7",
                b"?;42",
                b"?42;",
                b"?1:42",
                b"?42:1",
                b"?4\r2",
                b"?\x0042\x7f",
            ] {
                for final_byte in [b"h", b"l", b"r"] {
                    let input = [b"a", introducer, parameters, final_byte, rest].concat();
                    assert_eq!(
                        super::strip_string_escapes(&input),
                        [b"a", rest].concat(),
                        "{input:02x?}"
                    );
                }
            }
        }

        // Reached through a chain, it goes with the chain.
        for chain in [
            b"a\x1b(\x1b[?42h".as_slice(),
            b"a\x1b\x9b?42l",
            b"a\x1b[1\x1b\xc2\x9b?42r",
        ] {
            let input = [chain, rest].concat();
            assert_eq!(
                super::strip_string_escapes(&input),
                [b"a", rest].concat(),
                "{input:02x?}"
            );
        }

        // xterm keeps 30 parameters and reads the digits after the 30th into
        // it, so after 29 others `4;2` is 42, and `42;0` is 420.
        for (others, last, dropped) in [
            (29, b"42".as_slice(), true),
            (29, b"4;2", true),
            (29, b"4:2", true),
            (29, b"0;04;;2", true),
            (29, b"1;42", false),
            (29, b"42;0", false),
            (28, b"4;2", false),
        ] {
            for final_byte in [b"h", b"l", b"r"] {
                let csi = [
                    b"\x1b[?".as_slice(),
                    &b"1;".repeat(others),
                    last,
                    final_byte,
                ]
                .concat();
                let input = [b"a", csi.as_slice(), rest].concat();
                let expected = if dropped {
                    [b"a", rest].concat()
                } else {
                    input.clone()
                };
                assert_eq!(
                    super::strip_string_escapes(&input),
                    expected,
                    "{input:02x?}"
                );
            }
        }

        // Other modes, ANSI mode 42, saving mode 42, and a sequence with an
        // intermediate or another final byte are kept.
        for csi in [
            b"\x1b[?25h".as_slice(),
            b"\x1b[?1049l",
            b"\x1b[?4h",
            b"\x1b[?2;4r",
            b"\x1b[?420h",
            b"\x1b[?4200000000000000000042h",
            b"\x1b[?4?2h",
            b"\x1b[42h",
            b"\x1b[>42h",
            b"\x1b[?42s",
            b"\x1b[?42m",
            b"\x1b[?42$p",
            b"\x1b[?42 h",
            b"\x9b?1049h",
            b"\xc2\x9b?25l",
        ] {
            let input = [b"a", csi, rest].concat();
            assert_eq!(super::strip_string_escapes(&input), input, "{input:02x?}");
        }
    }

    // Preserve mode keeps such a string as one opaque control, so highlighting
    // cannot insert SGR bytes inside what the terminal reads as a title.
    #[test]
    fn escape_state_controls_before_a_string_introducer_stay_in_one_token() {
        let input = b"x\x1b\x00]2;192.0.2.1\x07y";
        assert_eq!(super::ansi_sequence_end(input, 1), input.len() - 1);
        assert_eq!(super::ansi_sequence_end(b"\x1b\r[31m", 0), 6);
        assert_eq!(super::strip_ansi(input), b"xy");

        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - regex: '192\\.0\\.2\\.1'\n    color: f#00ff00\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");
        assert_eq!(highlighter.highlight_bytes(input), input);

        let mut streaming = StreamingHighlighter::new(highlighter);
        let mut output = Vec::new();
        for byte in input {
            output.extend(streaming.push(&[*byte]));
        }
        output.extend(streaming.finish());
        assert_eq!(output, input);
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

    // Each sanitize split after the first resumes the scan from a stop near it
    // instead of rescanning its prefix, which must not move any split. That
    // relies on the scan of a prefix making every stop the scan of the whole
    // read made far enough before the prefix ends, so check that too.
    #[test]
    fn sanitize_split_resuming_its_scan_matches_rescanning_each_prefix() {
        const FRAGMENTS: &[&[u8]] = &[
            b"\x1b",
            b"\x1b\x00",
            b"\x1b\r",
            b"\x1b\x1b",
            b"\x1b\x18",
            b"\x00",
            b"\x07",
            b"\x18",
            b"\x1a",
            b"\x1c",
            b"\x7f",
            b"\r\n",
            b"\x9b",
            b"\x9d",
            b"\x90",
            b"\x98",
            b"\x9e",
            b"\x9f",
            b"\x9c",
            b"\x8e",
            b"\x8f",
            b"\x84",
            b"\xc2\x9b",
            b"\xc2\x9d",
            b"\xc2\x90",
            b"\xc2\x98",
            b"\xc2\x9e",
            b"\xc2\x9f",
            b"\xc2\x9c",
            b"\xc2\x8e",
            b"\xc2\x8f",
            b"\x1b]0;title\x07",
            b"\x1b]52;c;",
            b"\x1bPq",
            b"\x1b\\",
            b"\x1b_",
            b"\x1bX",
            b"\x1b^",
            b"\x9d0;t\x9c",
            b"\x1b[",
            b"\x1b[1",
            b";1",
            b"\x1b[1;2H",
            b"\x1b[?25h",
            b"\x1b[31m",
            b"\x1b[0m",
            b"\x1b[38;5;196m",
            b"\x9b2J",
            b"\x9b1",
            b"\x1b(",
            b"\x1b(B",
            b"\x1b ",
            b"\x1b%G",
            b"\x1bN",
            b"\x1bO",
            b"\x1bc",
            "\u{e9}".as_bytes(),
            "\u{4e2d}".as_bytes(),
            "\u{275c}".as_bytes(),
            "\u{1f600}".as_bytes(),
            b"\xc2",
            b"\xe2",
            b"\xe2\x9d",
            b"\xe0",
            b"\xed",
            b"\xf0",
            b"\xf0\x9f",
            b"\xf0\x9f\x9c",
            b"\xf4\x90",
            b"\x80",
            b"\xa9",
            b"\xbf",
            b"\xff",
            // KOI8-R and CP1251 words: legacy text full of UTF-8 lead bytes.
            b"\xd3\xcf\xcf\xc2\xdd\xc5\xce\xc9\xc5",
            b"\xef\xf0\xe8\xe2\xe5\xf2",
            b"\xce\xf8\xe8\xe1\xea\xe0",
            // Controls that each end in a character the next one cuts off.
            b"\x1b\xf0\x9f\x9c",
            b"\x1b[\xf0\x9f\x9c",
            b"\x1b\xe2\x9d",
            b"\x1b\xc2",
            b"\x9b\xf0\x9f\xc2",
            b"a",
            b" ",
            b"router# show ip\n",
        ];
        // The reads of the other sanitize split and filtered read tests, joined.
        const CASES: &[&[u8]] = &[
            b"\x1b[\xe2\x9d\\",
            b"\x90\x1b\xe2\x90\x9d",
            b"\x1b]0;\x90t\x07visible\x1b\\after",
            b"\x1b]0;\x90t\x07vis",
            b"\x9b\xf0\x9f\xc2\x9c",
            b"\x9b\xf0\x9f\xc2Gpayload\x9c",
            b"\x9b\xf0\x9f\xc2",
            b"\x9b\xf0\x9d\xc2\xa052;c;aGk=\x07",
            b"\x9b\xf0\x90\xc2q\x9c",
            b"\x9b\xe2\x9f\xc2\x9c",
            b"\x9b\xf0\x9f\x1b\\",
            b"\x1b[\xf0\x9f\xc2\x9c",
            b"\x1b[\xe2\x9d\xc2x\x07",
            b"\x1b\xf0\x9f\x9c\xc2\x1b[\x9c",
            b"\x1b\xf0\x9f\xa0\xc2G\x9c",
            b"\x9b\xe0\x9d",
            b"\x1b[\xf4\x90",
            b"\xf0\x9f\x72\xe0",
            b"\xf0\x9f\x01",
            b"\xf0\x9d\x9c\x1b[31m",
            b"\x9b\xf0\x9f\xc2hidden-payload\x9c",
            b"ok \xd3\xcf\xcf\xc2\xdd\xc5\xce\xc9\xc5\x1b[",
            b"safe\x1b[1\x1b]52;c;unterminated",
            b"safe\x1b(\x1b]52;c;unterminated",
            b"safe\x9b1\x9d52;c;unterminated",
            b"safe\x1b[1\x1b]52;c;SGVsbG8=\x07Jafter",
            b"safe\x1b(\x1b]52;c;SGVsbG8=\x07Jafter",
            b"safe\x9b1\x9d52;c;SGVsbG8=\x07Jafter",
            b"safe\x1b[1\x9d52;c;SGVsbG8=\x07Jafter",
            b"safe\x1b[1\x18\x1b]52;c;SGVsbG8=\x07Jafter",
            b"a\x1b\xc2",
            b"a\x1b\xe2\x90",
            b"a\x1b(\xc2",
            b"a\x1b\xc2\x9d0;t\x07b",
            b"ok\x9dtitle",
            b"ok\x9b31",
            b"hello\x1b[31m world",
            b"safe\x1b]52;c;aGVsbG8=\x07 after",
            b"safe\x9d52;c;aGVsbG8=\x07 after",
            b"safe\xc2\x9d52;c;aGVsbG8=\x07 after",
            b"safe\xc2\x90dcs\xc2\x9c after",
            b"ok\x1b\x00]52;c;U0VDUkVU\x07done",
            b"x\x1b\x00]2;192.0.2.1\x07y",
            b"last\x1bN\r\n",
            b"last\x8e\n",
            b"last\x1bO\r\n",
            b"x\x1b]0;t\x1b[0m hello",
            b"before\x1b]52;abc\x18after\n",
            b"before\x1bPabc\x1aafter\n",
            b"before\x9d52;abc\x18after\n",
            b"before\x90abc\x1aafter\n",
            b"safe\x1b]ab\x1b]\x07after",
            b"prompt \xe2\x9d\xaf ready",
            b"\xc2\xa9\xe2\x9d\x9c\x9b2J\x1b\\after",
        ];

        let check = |input: &[u8], lens: &mut dyn Iterator<Item = usize>| {
            let mut stops = Vec::new();
            super::resume_top_level_escape_scan(input, Default::default(), &mut |stop| {
                stops.push(stop)
            });
            for len in lens {
                let prefix = &input[..len];
                assert_eq!(
                    super::incomplete_sanitize_start(prefix),
                    super::incomplete_sanitize_start_by_rescans(prefix),
                    "split of {prefix:02x?}"
                );
                let mut prefix_stops = Vec::new();
                super::resume_top_level_escape_scan(prefix, Default::default(), &mut |stop| {
                    prefix_stops.push(stop)
                });
                let settled = |stop: &&super::TopLevelScanStop| {
                    stop.idx + super::TOP_LEVEL_SCAN_LOOKAHEAD <= len
                };
                assert!(
                    prefix_stops
                        .iter()
                        .filter(settled)
                        .eq(stops.iter().filter(settled)),
                    "stops of {prefix:02x?}: {prefix_stops:?}, whole input: {stops:?}"
                );
            }
        };

        // A read starts wherever the previous one left its carry, so check
        // every part of each case.
        for case in CASES {
            for start in 0..case.len() {
                let input = &case[start..];
                check(input, &mut (0..=input.len()));
            }
        }
        for control in [0x00u8, 0x01, b'\r', b'\n', 0x19, 0x1c, 0x1f, 0x7f] {
            for introducer in [b"]52;c;U0VDUkVU".as_slice(), b"]2;TITLE", b"P1$qm", b"_apc"] {
                let input = [b"a\x1b".as_slice(), &[control, control], introducer].concat();
                check(&input, &mut (0..=input.len()));
            }
            let input = [b"ok\x1b".as_slice(), &[control]].concat();
            check(&input, &mut (0..=input.len()));
        }
        for group in [
            b"\x1b\xf0\x9f\x9c".as_slice(),
            b"\x1b[\xf0\x9f\x9c",
            b"\x1b\xe2\x9d",
            b"\x1b\xc2",
        ] {
            let input = group.repeat(8192 / group.len());
            check(&input, &mut (input.len() - 16..=input.len()));
        }
        let mut legacy = b"0m ".to_vec();
        legacy.extend_from_slice(&b"x".repeat(4000));
        legacy.extend_from_slice(b" \xd3\xcf\xcf\xc2\xdd\xc5\xce\xc9\xc5\x1b[");
        check(&legacy, &mut (legacy.len() - 16..=legacy.len()));
        // Controls at and past the size beyond which they are no longer
        // carried, followed by a run that the split steps back over.
        let (control_limit, string_limit) = (
            super::MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES,
            super::MAX_INCOMPLETE_ESCAPE_BYTES,
        );
        for (introducer, filler, limit) in [
            (b"\x1b[".as_slice(), b'1', control_limit),
            (b"\x9b", b'1', control_limit),
            (b"ok\x1b", 0x00, control_limit),
            (b"safe\x1b]52;c;", b'x', string_limit),
            (b"safe\x1bP", b'x', string_limit),
            (b"safe\xc2\x90", b'x', string_limit),
            (b"safe\x1b[1\x1b]52;c;", b'x', string_limit),
        ] {
            for extra in [0, 1, 2] {
                let mut input = introducer.to_vec();
                input.resize(limit + extra, filler);
                input.extend_from_slice(&b"\x1b\xc2".repeat(5));
                let len = input.len();
                check(&input, &mut (len - 32..=len));
            }
        }

        let mut seed = 0x5eed_5b11_7a11_0c4e_u64;
        let mut below = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        for case in 0..1000 {
            let large = case % 100 == 99;
            let fragments = if large { 1000 } else { 1 + below(24) };
            let mut input = Vec::new();
            for _ in 0..fragments {
                let fragment = FRAGMENTS[below(FRAGMENTS.len())];
                // Repeats make runs that cross the pass cap and the spacing
                // of the stops the split keeps.
                let repeats = if below(8) == 0 { 1 + below(40) } else { 1 };
                for _ in 0..repeats {
                    input.extend_from_slice(fragment);
                }
            }
            // Start anywhere, as a carry can, even inside a character.
            let input = &input[below(input.len().min(8) + 1)..];
            if input.len() <= 128 {
                check(input, &mut (0..=input.len()));
            } else {
                let lens: Vec<usize> = (0..32).map(|_| below(input.len() + 1)).collect();
                check(input, &mut lens.into_iter().chain([input.len()]));
            }
        }
    }

    // Seven controls that each end in a character the split cut off, in front
    // of an unfinished one, use up the split's passes. The split is then the
    // start of the run, as it is for a shorter run, not the start of the
    // read: the text in front of the run is not part of any control.
    #[test]
    fn sanitize_split_past_its_pass_cap_carries_the_run_without_the_text_before_it() {
        for units in [6, 7, 8, 9, 100] {
            let mut ending = b"\x1b[1\xe2\x80".repeat(units);
            ending.extend_from_slice(b"\x1b[");

            let mut read = b"visible\r\n".to_vec();
            read.extend_from_slice(&ending);
            assert_eq!(
                super::incomplete_sanitize_start(&read),
                Some(9),
                "{units} units after text"
            );

            let mut read = b"\x1b[31mred\x1b[0m".to_vec();
            read.extend_from_slice(&ending);
            assert_eq!(
                super::incomplete_sanitize_start(&read),
                Some(12),
                "{units} units after a control"
            );

            // With nothing in front, all of it is carried.
            assert_eq!(
                super::incomplete_sanitize_start(&ending),
                Some(0),
                "{units} units"
            );
        }
    }

    // The first scan looks at the bytes in front of each control for a
    // cut-off character. A character is at most four bytes, so that look must
    // not walk back over a whole run of bytes that only look like the rest of
    // one, as raw C1 controls do: a read full of them would cost its length
    // once per control. A worker thread keeps a regression from hanging.
    #[test]
    fn sanitize_split_looks_back_a_bounded_distance_at_each_control() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut splits = Vec::new();
            for unit in [
                b"\x9b\x9c".as_slice(),
                b"\x9b\x80\x9c",
                b"\x1b[\x80\x80\x80\x9c",
            ] {
                let mut read = unit.repeat(4 * 1024 * 1024 / unit.len());
                read.push(b'm');
                splits.push(super::incomplete_sanitize_start(&read));
            }
            let _ = sender.send(splits);
        });
        let splits = receiver
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("4 MiB of raw C1 controls should be split within 60s");
        assert_eq!(splits, [None, None, None]);
    }

    // Past its pass cap the split falls back to a control it noted on the
    // first scan. Whatever the read holds, the bytes in front of that split
    // must end with no control open, as they do in front of every other
    // split, and the split must not be later than the one that stepping back
    // without a cap reaches.
    #[test]
    fn sanitize_split_past_its_pass_cap_leaves_no_control_open_before_it() {
        const FRAGMENTS: &[&[u8]] = &[
            b"\x1b",
            b"\x1b\x00",
            b"\x1b\x1b",
            b"\x18",
            b"\r\n",
            b"\x9b",
            b"\x9d",
            b"\x90",
            b"\x9c",
            b"\x8e",
            b"\x84",
            b"\xc2\x9b",
            b"\xc2\x9d",
            b"\xc2\x9c",
            b"\xc2\x8e",
            b"\x1b]0;title\x07",
            b"\x1b]52;c;",
            b"\x1bPq",
            b"\x1b\\",
            b"\x1b[",
            b"\x1b[1",
            b"\x1b[31m",
            b"\x1b[0m",
            b"\x9b2J",
            b"\x1b(",
            b"\x1b(B",
            b"\x1bN",
            b"\x1bc",
            "\u{e9}".as_bytes(),
            "\u{275c}".as_bytes(),
            "\u{1f600}".as_bytes(),
            b"\xc2",
            b"\xe2",
            b"\xe2\x9d",
            b"\xe2\x80",
            b"\xf0",
            b"\xf0\x9f",
            b"\xf0\x9f\x9c",
            b"\x80",
            b"\xa9",
            b"\xff",
            b"\xd3\xcf\xcf\xc2\xdd\xc5\xce\xc9\xc5",
            b"\x1b\xf0\x9f\x9c",
            b"\x1b[\xf0\x9f\x9c",
            b"\x1b\xe2\x9d",
            b"\x1b\xc2",
            b"\x1b[1\xe2\x80",
            b"\x9b\xf0\x9f\xc2",
            b"\x1bN\xe2\x80",
            b"\x1b]0;t\xe2\x9c",
            b"a",
            b" ",
            b"router# show ip\n",
        ];
        // Where stepping back with no cap on its passes ends, and how many
        // controls it steps back over.
        let uncapped = |bytes: &[u8]| {
            let escape = super::incomplete_top_level_escape_start(bytes).unwrap_or(bytes.len());
            let utf8 = super::incomplete_utf8_start(bytes).unwrap_or(bytes.len());
            let mut split = escape.min(utf8);
            let mut steps = 0;
            while let Some(start) = super::incomplete_top_level_escape_start(&bytes[..split]) {
                split = start;
                steps += 1;
            }
            (split, steps)
        };

        let mut seed = 0x0dd5_eed5_ca27_7a11_u64;
        let mut below = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let mut past_the_cap = 0;
        for _ in 0..4000 {
            let mut input = Vec::new();
            for _ in 0..1 + below(12) {
                let fragment = FRAGMENTS[below(FRAGMENTS.len())];
                let repeats = if below(4) == 0 { 8 + below(40) } else { 1 };
                for _ in 0..repeats {
                    input.extend_from_slice(fragment);
                }
            }
            let split = super::incomplete_sanitize_start(&input).unwrap_or(input.len());
            let (uncapped_split, steps) = uncapped(&input);
            if steps < super::MAX_SANITIZE_SPLIT_PASSES - 1 {
                assert_eq!(split, uncapped_split, "{input:02x?}");
                continue;
            }
            past_the_cap += 1;
            assert_eq!(
                super::incomplete_top_level_escape_start(&input[..split]),
                None,
                "split {split} of {input:02x?}"
            );
            assert!(split <= uncapped_split, "split {split} of {input:02x?}");
        }
        assert!(past_the_cap > 100, "{past_the_cap} inputs past the cap");
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

    // A VT parser ends a string control at any ESC that does not start ST and
    // parses that ESC as a new control, so an ESC-ended or stray OSC must not
    // hold back or discard the output after it.
    #[test]
    fn escape_ends_string_controls_like_a_vt_parser() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for input in [
            b"x\x1b]0;t\x1b[0m hello world\n".as_slice(),
            b"a\x1bPq\x1b[0m\nback\n".as_slice(),
            b"a\x9d0;t\x1b[0m\nback\n".as_slice(),
            b"a\x1b]0;t\x1b\x18back\n".as_slice(),
        ] {
            let mut whole = StreamingHighlighter::new(highlighter.clone());
            let mut output = whole.push(input);
            output.extend(whole.finish());
            assert_eq!(output, input);

            let mut bytewise = StreamingHighlighter::new(highlighter.clone());
            let mut output = Vec::new();
            for byte in input {
                output.extend(bytewise.push(&[*byte]));
            }
            output.extend(bytewise.finish());
            assert_eq!(output, input);
        }

        assert_eq!(super::strip_ansi(b"x\x1b]0;t\x1b[0m hi"), b"x hi");
        assert_eq!(
            super::strip_string_escapes(b"x\x1b]0;t\x1b[0m hi"),
            b"x\x1b[0m hi"
        );
        assert_eq!(
            super::strip_string_escapes(b"x\x9d0;t\x1b[0m hi"),
            b"x\x1b[0m hi"
        );
        // A nested introducer starts a string of its own, which is stripped too.
        assert_eq!(
            super::strip_string_escapes(b"x\x1b]0;t\x1b]52;c;eA==\x07y"),
            b"xy"
        );

        let mut input = b"start\x1b]".to_vec();
        let mut visible = b"start".to_vec();
        for line in 0..2000 {
            input.extend_from_slice(format!("\x1b[32mline {line}\x1b[0m\n").as_bytes());
            visible.extend_from_slice(format!("line {line}\n").as_bytes());
        }
        let mut streaming = StreamingHighlighter::new(highlighter);
        let mut output = Vec::new();
        for chunk in input.chunks(8192) {
            output.extend(streaming.push(chunk));
        }
        output.extend(streaming.finish());
        assert_eq!(output, input);
        assert_eq!(super::strip_ansi(&output), visible);
    }

    #[test]
    fn oversized_string_discard_ends_at_an_escape_that_starts_a_new_control() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for introducer in [
            b"\x1b]0;".as_slice(),
            b"\x1bP".as_slice(),
            b"\x9d0;".as_slice(),
        ] {
            for reads in [
                [b"".as_slice(), b"\x1b[1mafter\x1b[0m\n".as_slice()],
                [b"\x1b".as_slice(), b"[1mafter\x1b[0m\n".as_slice()],
                [b"\x1b[".as_slice(), b"1mafter\x1b[0m\n".as_slice()],
            ] {
                let mut first = b"safe".to_vec();
                first.extend_from_slice(introducer);
                first.extend(std::iter::repeat_n(
                    b'x',
                    super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
                ));

                let mut streaming = StreamingHighlighter::new(highlighter.clone());
                let mut output = streaming.push(&first);
                for read in reads {
                    output.extend(streaming.push(read));
                }
                output.extend(streaming.finish());
                assert_eq!(output, b"safe\x1b[1mafter\x1b[0m\n");

                // The same bytes in one push end the discard at the same ESC.
                let mut whole = StreamingHighlighter::new(highlighter.clone());
                let mut input = first.clone();
                input.extend(reads.concat());
                let mut output = whole.push(&input);
                output.extend(whole.finish());
                assert_eq!(output, b"safe\x1b[1mafter\x1b[0m\n");
            }
        }
    }

    #[test]
    fn chain_of_escape_ended_strings_stays_within_the_carry_budget() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new(highlighter);
        let mut string = b"\x1b]".to_vec();
        string.resize(64, b'a');

        let mut output = Vec::new();
        for _ in 0..512 {
            output.extend(streaming.push(&string));
            assert!(
                streaming.pending.len() <= super::MAX_INCOMPLETE_ESCAPE_BYTES + string.len(),
                "pending grew to {} bytes",
                streaming.pending.len()
            );
        }
        output.extend(streaming.push(b"\x07after\n"));
        output.extend(streaming.finish());
        assert!(super::strip_ansi(&output).ends_with(b"after\n"));
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
            b"\x1b\x00".as_slice(),
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
            (b"\x1b\x00".as_slice(), b"safeafter".as_slice()),
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

    // A line ending is not the character a single shift selects, so like the
    // tokenizer the scanner ends the shift there instead of carrying the line
    // ending with it.
    #[test]
    fn single_shift_before_a_line_ending_keeps_the_line_ending() {
        for input in [
            b"last line\x1bN\r\n".as_slice(),
            b"last line\x1bO\n".as_slice(),
            b"last line\x8e\r\n".as_slice(),
            b"x\x8f\r".as_slice(),
        ] {
            assert_eq!(
                super::incomplete_top_level_escape_start(input),
                None,
                "{input:02x?}"
            );
        }

        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        for shift in [b"\x1bN".as_slice(), b"\x8e".as_slice()] {
            let mut input = b"R1# show clock\r\nlast line".to_vec();
            input.extend_from_slice(shift);
            input.extend_from_slice(b"\r\nR1# show ver\r\nCisco\r\n");

            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut output = streaming.push(&input);
            output.extend(streaming.finish());

            let mut line_end = shift.to_vec();
            line_end.extend_from_slice(b"\r\nR1# ");
            assert!(
                output
                    .windows(line_end.len())
                    .any(|window| window == line_end),
                "line ending lost after {shift:02x?}: {output:02x?}"
            );
            assert_eq!(super::strip_ansi(&output), super::strip_ansi(&input));
        }

        // The line is released without waiting for more output, and EOF does
        // not neutralize the shift.
        let mut streaming = StreamingHighlighter::new(highlighter);
        assert_eq!(streaming.push(b"abc\x1bN\n"), b"abc\x1bN\n");
        assert!(streaming.finish().is_empty());
    }

    // Many string introducers that share one far terminator must not make the
    // split point search rescan to that terminator from each of them.
    #[test]
    fn split_point_search_stays_linear_for_shared_string_terminators() {
        for (introducer, terminator) in [
            (b"\x90x".as_slice(), b"\x9c".as_slice()),
            (b"\x9dx".as_slice(), b"\x07".as_slice()),
        ] {
            let mut chunk = introducer.repeat(4094);
            chunk.extend_from_slice(terminator);
            chunk.extend_from_slice(b"tok");
            assert_eq!(
                super::streaming_split_at(&super::AnsiChunk::from_slice(&chunk)),
                chunk.len() - 3
            );

            let highlighter =
                Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
            let mut streaming = StreamingHighlighter::new(highlighter);
            let started = std::time::Instant::now();
            let mut output = Vec::new();
            for _ in 0..32 {
                output.extend(streaming.push(&chunk));
            }
            output.extend(streaming.finish());
            let elapsed = started.elapsed();

            assert_eq!(output, chunk.repeat(32));
            assert!(
                elapsed < std::time::Duration::from_secs(2),
                "32 reads of {introducer:02x?} chains took {elapsed:?}"
            );
        }
    }

    // A run of cancelled controls is carried from its first control, so the
    // carry cap must hold for the whole run, not just its incomplete tail.
    #[test]
    fn incomplete_cancellation_chain_is_capped_across_pushes() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for unit in [b"\x1b".as_slice(), b"\x1b[".as_slice(), b"\x9b".as_slice()] {
            let run = unit.repeat(16);
            let mut streaming = StreamingHighlighter::new(highlighter.clone());
            let mut output = Vec::new();
            for _ in 0..256 {
                output.extend(streaming.push(&run));
                assert!(
                    streaming.pending.len()
                        <= super::MAX_INCOMPLETE_NON_STRING_ESCAPE_BYTES + run.len(),
                    "pending grew to {} for {unit:02x?}",
                    streaming.pending.len()
                );
            }
            output.extend(streaming.finish());

            assert_eq!(output.len(), 256 * run.len());
            assert!(
                !output.contains(&0x1b) && !output.contains(&0x9b),
                "live control survived for {unit:02x?}"
            );
        }

        // A run that holds an ESC-ended string gets the string budget.
        let mut input = b"\x1b]0;".to_vec();
        input.extend(std::iter::repeat_n(b't', 2000));
        input.extend_from_slice(b"\x1b[");
        let mut streaming = StreamingHighlighter::new(highlighter.clone());
        let mut output = streaming.push(&input);
        output.extend(streaming.push(b"31mtext\n"));
        output.extend(streaming.finish());
        input.extend_from_slice(b"31mtext\n");
        assert_eq!(output, input);

        let mut streaming = StreamingHighlighter::new(highlighter);
        let mut output = streaming.push(b"\x1b]0;title");
        for _ in 0..80 {
            output.extend(streaming.push(&[0x1b; 256]));
            assert!(
                streaming.pending.len() <= super::MAX_INCOMPLETE_ESCAPE_BYTES + 256,
                "pending grew to {} after a string",
                streaming.pending.len()
            );
        }
        output.extend(streaming.finish());
        assert!(!output.contains(&0x1b), "live ESC survived after a string");
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

    // Regression: a shell recalling history with an arrow key rewrites the input
    // line by backspacing over it. The rewritten line ends mid-token (no
    // delimiter after `=`), so the tail stays buffered, and the arrow key never
    // matches it byte for byte, so only the line-edit provenance can surface it.
    #[test]
    fn interactive_buffered_history_recall_tail_is_flagged_as_line_edit_redraw() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push_str("user@host:~$ deploy\r\n");
        let redraw = "\x08\x08\x08\x08\x08\x08vault login -method=userpass username=operator";
        let out = streaming.push_str(redraw);
        let visible = super::strip_ansi(out.as_bytes());

        // The trailing token is withheld (the reported bug): the recalled line is
        // drawn only up to the `=`.
        assert!(
            visible.ends_with(b"username="),
            "tail should be withheld: {out:?}"
        );
        assert_eq!(streaming.buffered_echo(), b"operator");

        // The buffered tail provably came from a line-editor redraw.
        assert!(streaming.buffered_echo_completes_line_edit_redraw());
        assert!(
            !streaming.buffered_echo_completes_prompt_line(),
            "the redraw carries no prompt, so the prompt-echo trigger stays out of it"
        );

        // The idle flush surfaces exactly the tail.
        assert_eq!(
            super::strip_ansi(&streaming.flush_buffered_echo()),
            b"operator"
        );
        assert!(streaming.finish().is_empty());
    }

    // Provenance guard: only a backspace-led rewrite of the current line counts.
    #[test]
    fn program_output_does_not_qualify_for_line_edit_redraw_flush() {
        // Bulk output split mid-token: no leading backspace, so not a line edit.
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push(b"interface Vlan11");
        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "bulk program output must not qualify as a line-edit redraw tail"
        );

        // A bare-CR rewrite is what a progress bar emits on every update; it must
        // not arm the flush.
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push(b"\rdownloading 42 of 96 blocks eta=0012");
        assert!(!streaming.buffered_echo().is_empty());
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a carriage-return rewrite is not a line edit"
        );

        // A backspace-led chunk that then starts a new line has moved on to
        // program output; the stranded tail is no longer the edited line.
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push(b"\x08\x08done\r\nnext chunk=value");
        assert!(!streaming.buffered_echo().is_empty());
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a chunk containing a line feed is not an in-place line edit"
        );
    }

    // The signal is per-LINE, not per-chunk: it must survive the reads that
    // follow on the same line (that is what makes a split redraw work) and must
    // be gone once the line ends.
    #[test]
    fn line_edit_redraw_signal_is_scoped_to_the_rewritten_line() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"\x08\x08\x08show version=1");
        assert!(streaming.buffered_echo_completes_line_edit_redraw());

        // Same line, next read: still the rewritten line, still eligible.
        let _ = streaming.push(b".2 target=host");
        assert_eq!(streaming.buffered_echo(), b"host");
        assert!(
            streaming.buffered_echo_completes_line_edit_redraw(),
            "the rewritten line continues across reads"
        );

        // The line ends: the signal must not carry into what follows.
        let _ = streaming.push(b"\r\ninterface Vlan11");
        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a new line is not a rewritten line"
        );
    }

    // The reported redraw delivered in two reads. Only the first carries the
    // backspaces; the second strands the tail. A per-chunk test of the stranding
    // read cannot see the edit, so the signal has to come from the line.
    #[test]
    fn split_history_recall_redraw_still_qualifies_for_the_line_edit_flush() {
        for boundary in [6usize, 12, 26, 44] {
            let redraw: &[u8] =
                b"\x08\x08\x08\x08\x08\x08vault login -method=userpass username=operator";
            let highlighter =
                Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
            let mut streaming = StreamingHighlighter::new_interactive(highlighter);
            let _ = streaming.push_str("user@host:~$ deploy\r\n");
            let _ = streaming.push(&redraw[..boundary]);
            let _ = streaming.push(&redraw[boundary..]);

            assert_eq!(
                streaming.buffered_echo(),
                b"operator",
                "boundary {boundary}: the tail is stranded as reported"
            );
            assert!(
                streaming.buffered_echo_completes_line_edit_redraw(),
                "boundary {boundary}: a split redraw must still qualify for the flush"
            );
            assert_eq!(
                super::strip_ansi(&streaming.flush_buffered_echo()),
                b"operator",
                "boundary {boundary}: the flush surfaces exactly the tail"
            );
        }
    }

    // The redraw arriving while an earlier echo token is still buffered: the
    // combined chunk no longer starts with the backspaces, but the line does
    // carry them.
    #[test]
    fn history_recall_after_a_pending_echo_token_still_qualifies() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        // Echo the detector does not recognize as a prompt is held back; it is
        // still buffered when the recall redraw arrives.
        let _ = streaming.push(b"lt");
        assert_eq!(streaming.buffered_echo(), b"lt");
        let _ = streaming.push(b"\x08\x08vault login -method=userpass username=operator");

        assert_eq!(streaming.buffered_echo(), b"operator");
        assert!(
            streaming.buffered_echo_completes_line_edit_redraw(),
            "a pending token in front of the backspaces must not hide the line edit"
        );
    }

    // Alternate-screen apps draw with backspaces (xterm's cub1 is ^H). Those
    // bytes belong to the app's own screen, not to any line of the shell, and
    // must not arm the rewrite signal for the output that follows the exit.
    #[test]
    fn alt_screen_backspace_does_not_arm_after_exit_separate_reads() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"\x1b[?1049h");
        let _ = streaming.push(b"x\x08y");
        let _ = streaming.push(b"\x1b[?1049l");
        let _ = streaming.push(b"interface Vlan11");

        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "alternate-screen bytes must not arm the trigger on a primary-screen line"
        );
    }

    // The same, with the app's last frame and the screen switch in one read:
    // clearing the tracked line at the switch token alone would be observed away
    // again by the rest of the chunk.
    #[test]
    fn alt_screen_backspace_does_not_arm_after_coalesced_exit_read() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"\x1b[?1049h");
        let _ = streaming.push(b"x\x08y\x1b[?1049l");
        let _ = streaming.push(b"interface Vlan11");

        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a screen switch coalesced with the frame must still break the line"
        );
    }

    // A genuine line edit made before the app started must not come back when it
    // exits: the alternate screen is a line discontinuity in both directions.
    #[test]
    fn pre_alt_screen_backspace_does_not_survive_the_round_trip() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"vault lo\x08gin ");
        let _ = streaming.push(b"\x1b[?1049h");
        let _ = streaming.push(b"\x1b[?1049l");
        let _ = streaming.push(b"interface Vlan11");

        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a pre-app line edit must not arm the trigger after the app exits"
        );
    }

    // White-box guards on the two conjuncts no byte stream can currently reach.
    // They are defense in depth, so they need a regression signal of their own.
    #[test]
    fn qualifier_is_false_while_the_alternate_screen_is_active() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"\x08\x08show ver");
        assert!(streaming.buffered_echo_completes_line_edit_redraw());

        streaming.alternate_screen = true;
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a stranded tail must never flush while an app owns the screen"
        );
    }

    #[test]
    fn prompt_echo_provenance_does_not_qualify_for_line_edit_redraw() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"\x08\x08show ver");
        assert!(streaming.buffered_echo_completes_line_edit_redraw());

        // A tail buffered at the prompt-echo split site belongs to trigger 2.
        streaming.pending_from_interactive_split = false;
        streaming.pending_from_prompt_echo_remainder = true;
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "triggers 2 and 3 must stay disjoint"
        );
    }

    // The `\r` half of the scope is covered by
    // `line_edit_redraw_signal_is_scoped_to_the_rewritten_line`; pin the bare
    // `\n` half too, so dropping either reset fails a test.
    #[test]
    fn bare_lf_clears_the_rewritten_line_signal() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push(b"\x08\x08\x08show version=1");
        assert!(streaming.buffered_echo_completes_line_edit_redraw());

        let _ = streaming.push(b"\ninterface Vlan11");
        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a bare line feed ends the rewritten line"
        );
    }

    // A line editor that redraws with cursor movement instead of backspaces
    // (zsh's zle past a few columns) strands its tail the same way. The moving
    // read and the stranding read arrive separately, so only a per-line signal
    // can connect them.
    #[test]
    fn csi_cursor_back_redraw_split_across_reads_qualifies() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let _ = streaming.push_str("user@host:~$ deploy\r\n");
        let _ = streaming.push(b"\x1b[6D\x1b[K");
        let _ = streaming.push(b"vault login -method=userpass username=operator");

        assert_eq!(streaming.buffered_echo(), b"operator");
        assert!(
            streaming.buffered_echo_completes_line_edit_redraw(),
            "a cursor-movement rewrite is a line edit too"
        );
    }

    // Every in-place edit control the trigger recognizes, and the repaint
    // controls it must keep ignoring.
    #[test]
    fn only_in_place_edit_controls_mark_the_line_as_rewritten() {
        for control in [&b"\x1b[3D"[..], b"\x1b[2P", b"\x1b[4X", b"\x1b[1@"] {
            let highlighter =
                Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
            let mut streaming = StreamingHighlighter::new_interactive(highlighter);
            let _ = streaming.push(control);
            let _ = streaming.push(b"vault login username=operator");
            assert!(
                streaming.buffered_echo_completes_line_edit_redraw(),
                "{:?} rewrites the line in place",
                String::from_utf8_lossy(control)
            );
        }

        // Column-one repaints: byte-identical to a progress bar's update.
        for control in [&b"\x1b[G"[..], b"\x1b[K", b"\x1b[2K", b"\x1b[J", b"\r"] {
            let highlighter =
                Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
            let mut streaming = StreamingHighlighter::new_interactive(highlighter);
            let _ = streaming.push(control);
            let _ = streaming.push(b"downloading 42 of 96 blocks eta=0012");
            assert!(
                !streaming.buffered_echo().is_empty(),
                "{:?} should still strand a tail",
                String::from_utf8_lossy(control)
            );
            assert!(
                !streaming.buffered_echo_completes_line_edit_redraw(),
                "{:?} repaints the line, it does not edit it",
                String::from_utf8_lossy(control)
            );
        }
    }

    // Order decides the outcome, so the controls must be read in stream order
    // rather than scanned in two passes.
    #[test]
    fn line_rewrite_controls_are_read_in_stream_order() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        // Repaint, then edit what was repainted: the line is rewritten. The
        // stranding read has to be a separate one, since a chunk carrying a
        // cursor sequence passes through whole and strands nothing itself.
        let _ = streaming.push(b"\r\x1b[4D");
        let _ = streaming.push(b"vault username=operator");
        assert!(
            streaming.buffered_echo_completes_line_edit_redraw(),
            "a cursor-back after a repaint still rewrites the line"
        );

        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        // Edit, then start the line over: the edit is gone.
        let _ = streaming.push(b"\x1b[4D\r");
        let _ = streaming.push(b"downloading eta=0012");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a repaint after a cursor-back ends the rewritten line"
        );
    }

    // A prompt-echo repaint chunk is skipped by the visible-line tracker so a
    // redraw does not lose the prompt. The rewrite signal must still see the
    // controls inside it, in both directions.
    #[test]
    fn prompt_echo_repaint_window_still_reports_line_rewrites() {
        // Backspaces delivered inside a preserved repaint chunk must arm.
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push(b"\x1b[2Dvault relogin");
        let _ = streaming.push(b"\x1b[C\x08\x08\x08");
        let _ = streaming.push(b"\x1b[?2004l");
        let _ = streaming.push(b" username=operator");
        assert_eq!(streaming.buffered_echo(), b"operator");
        assert!(
            streaming.buffered_echo_completes_line_edit_redraw(),
            "backspaces inside a preserved repaint chunk still rewrite the line"
        );

        // A line break delivered inside one must clear.
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push(b"\x08\x08\x08vault login ");
        let _ = streaming.push(b"\x1b[42Gfoo ");
        let _ = streaming.push(b"\x1b[H\r\n");
        let _ = streaming.push(b"\x1b[?2004l");
        let _ = streaming.push(b"interface Vlan11");
        assert_eq!(streaming.buffered_echo(), b"Vlan11");
        assert!(
            !streaming.buffered_echo_completes_line_edit_redraw(),
            "a line break inside a preserved repaint chunk still ends the line"
        );
    }

    // The signal is a flag, not a search of the retained line tail, so a rewrite
    // survives a command longer than the tail cap.
    #[test]
    fn line_rewrite_signal_survives_a_line_longer_than_the_retained_tail() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);

        let mut redraw = b"\x08\x08\x08vault login".to_vec();
        for index in 0..80 {
            redraw.extend_from_slice(format!(" --option-{index}=value-{index}").as_bytes());
        }
        redraw.extend_from_slice(b" username=operator");
        assert!(redraw.len() > 1024, "the fixture must outrun the tail cap");

        let _ = streaming.push(&redraw);
        assert_eq!(streaming.buffered_echo(), b"operator");
        assert!(
            streaming.buffered_echo_completes_line_edit_redraw(),
            "a rewritten line longer than the retained tail still qualifies"
        );
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
            b"\x1b]0;hidden\n\x9b2J\x07user@host$ cmd\n".as_slice(),
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

    // Every position of a line full of Unicode prompt markers is a candidate
    // prompt end. Candidates past the prompt length limit can never match, so
    // scanning them made each line quadratic.
    #[test]
    fn interactive_marker_flood_line_is_linear() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let mut line = b"!".to_vec();
        for _ in 0..4095 {
            line.extend_from_slice("»".as_bytes());
        }
        line.push(b'\n');

        let started = std::time::Instant::now();
        let mut output = Vec::new();
        for _ in 0..8 {
            output.extend(streaming.push(&line));
        }
        output.extend(streaming.finish());

        assert_eq!(output.len(), line.len() * 8);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "marker flood took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn prompt_echo_scan_bound_matches_prompt_length_limit() {
        let limit = super::MAX_PROMPT_TAIL_BYTES;
        let at_limit = format!("{}# show run", "a".repeat(limit - 1));
        assert!(super::contains_prompt_echo_in_visible_line(
            at_limit.as_bytes()
        ));
        assert!(super::prompt_echo_sgr_bounds(at_limit.as_bytes(), false).is_some());

        let past_limit = format!("{}# show run", "a".repeat(limit));
        assert!(!super::contains_prompt_echo_in_visible_line(
            past_limit.as_bytes()
        ));
        assert!(super::prompt_echo_sgr_bounds(past_limit.as_bytes(), false).is_none());

        // `»` is two bytes, so these prompts are exactly at and one byte past
        // the limit.
        let unicode_at_limit = format!("{}» ls", "a".repeat(limit - 2));
        assert!(super::contains_prompt_echo_in_visible_line(
            unicode_at_limit.as_bytes()
        ));
        let unicode_past_limit = format!("{}» ls", "a".repeat(limit - 1));
        assert!(!super::contains_prompt_echo_in_visible_line(
            unicode_past_limit.as_bytes()
        ));
    }

    // The Unicode prompt check skips every tail with an ASCII last byte, which
    // is only sound while no marker ends in one.
    #[test]
    fn unicode_prompt_markers_end_in_non_ascii_bytes() {
        for marker in super::UNICODE_PROMPT_MARKERS {
            assert!(
                marker
                    .as_bytes()
                    .last()
                    .is_some_and(|last| !last.is_ascii()),
                "{marker:?} must end in a non-ASCII byte"
            );
        }
    }

    #[test]
    fn every_unicode_prompt_marker_still_starts_a_prompt_echo() {
        for marker in super::UNICODE_PROMPT_MARKERS {
            for line in [
                format!("{marker} make"),
                format!("~/src {marker} git status"),
            ] {
                assert!(
                    super::contains_prompt_echo_in_visible_line(line.as_bytes()),
                    "{line:?} is a prompt echo"
                );
            }
        }
        assert!(super::contains_prompt_echo_in_visible_line(
            "──○ cmd".as_bytes()
        ));
        assert!(!super::contains_prompt_echo_in_visible_line(
            b"plain ascii line with no prompt"
        ));
    }

    #[test]
    fn prompt_source_sgr_plan_is_empty_without_escapes() {
        for input in [b"router# show version".as_slice(), "~/src ❯ ls".as_bytes()] {
            assert_eq!(
                super::prompt_echo_source_sgr_plan(input, b""),
                (Vec::new(), Vec::new())
            );
            assert_eq!(
                super::neutralize_prompt_echo_source_sgr(input, b"", super::ResetMode::Minimal),
                input
            );
        }
    }

    #[test]
    fn utf8_c1_prompt_source_sgr_is_neutralized_before_command_echo() {
        let input = b"router# \xc2\x9b31mshow up";
        let output =
            super::neutralize_prompt_echo_source_sgr(input, b"", super::ResetMode::Minimal);

        assert!(
            !output.windows(2).any(|window| window == b"\xc2\x9b"),
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

    // A CR inside an OSC payload is not a line break: splitting the prompt
    // echo there would highlight the rest of the payload and inject SGR into it.
    #[test]
    fn prompt_echo_split_ignores_carriage_returns_inside_string_controls() {
        let input = b"R#show\x1b]0;a\r\n192.0.2.1\x07 more\r\n";
        assert_eq!(
            super::prompt_echo_line_prefix_len(input, b"R#"),
            Some(input.len())
        );

        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: ipv4\n    regex: '\\b192\\.0\\.2\\.\\d+\\b'\n    color: f#00ffff\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rules compile");
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let mut output = streaming.push(b"R#");
        output.extend(streaming.push(input));
        output.extend(streaming.finish());

        let osc = b"\x1b]0;a\r\n192.0.2.1\x07";
        assert!(
            output.windows(osc.len()).any(|window| window == osc),
            "OSC payload must pass through intact: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    // A string control with no terminator ends at the ESC of the next control,
    // and a CSI or escape sequence cut short ends there too. When that next
    // control is a source SGR on a prompt echo, removing it would leave the
    // terminal inside the earlier control, hiding the text after it.
    #[test]
    fn prompt_echo_keeps_source_sgr_that_ends_an_open_control() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for opener in [
            b"\x90".as_slice(),
            b"\xc2\x90",
            b"\x9d0;t",
            b"\x1bPq",
            b"\x1b]0;t",
            b"\x1bX",
            b"\x1b^",
            b"\x1b_",
            b"\x1b[3",
            b"\x9b3",
            b"\x1b(",
            b"\x1b",
        ] {
            let mut input = b">0] \x1b[31m # \x18".to_vec();
            input.extend_from_slice(opener);
            input.extend_from_slice(b"\x1b[31m \r\n end");

            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut output = streaming.push(&input);
            output.extend(streaming.finish());

            assert!(
                super::strip_ansi(&output).ends_with(b" \r\n end"),
                "text after {opener:02x?} was hidden: {output:02x?}"
            );
        }

        // An SGR after a control that ended on its own is still removed.
        let input = b"R# \x1bP\x18\x1b[31mshow";
        let output =
            super::neutralize_prompt_echo_source_sgr(input, b"", super::ResetMode::Minimal);
        assert_eq!(output, b"R# \x1bP\x18\x1b[39mshow");
    }

    // A terminal runs a C0 control it meets inside a CSI and ignores DEL
    // there. An SGR taken out of a prompt's command echo must leave those
    // controls where it was: without the line feed the terminal shows one
    // line where the source drew two.
    #[test]
    fn removed_prompt_echo_sgr_leaves_the_c0_controls_inside_it() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for (source_sgr, kept) in [
            (b"\x1b[3\n1m".as_slice(), b"\n".as_slice()),
            (b"\x9b3\n1m", b"\n"),
            (b"\xc2\x9b3\n1m", b"\n"),
            (b"\x1b\r[3\n1\x07m", b"\r\n\x07"),
            (b"\x1b[\x0e3\x7f1;\x001\x0fm", b"\x0e\x00\x0f"),
        ] {
            let mut input = b"host# show ".to_vec();
            input.extend_from_slice(source_sgr);
            input.extend_from_slice(b"run\n\x1b[0mnext\n");
            let mut expected = b"host# \x1b[39mshow ".to_vec();
            expected.extend_from_slice(kept);
            expected.extend_from_slice(b"run\n\x1b[0mnext\n");

            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut output = streaming.push(&input);
            output.extend(streaming.finish());

            assert_eq!(
                output, expected,
                "controls inside {source_sgr:02x?} were not kept: {output:02x?}"
            );
        }
    }

    #[test]
    fn prompt_echo_carriage_return_scan_is_linear() {
        let mut input = b"\x1b[1C".to_vec();
        input.extend(std::iter::repeat_n(b'\r', 32 * 1024));

        let started = std::time::Instant::now();
        assert_eq!(super::prompt_echo_line_prefix_len(&input, b"R#"), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "carriage return scan took {:?}",
            started.elapsed()
        );
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
                // A nested 7-bit ESC would end the string as it does in a
                // terminal, so the nested control is the C1 form, which a
                // terminal keeps as payload.
                output.extend(streaming.push(b"\x9b2J"));
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
                    !output.windows(3).any(|window| window == b"\x9b2J"),
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
        output.extend(streaming.push(b"\x9c\x9b2J"));
        output.extend(streaming.push(b"\x1b"));
        output.extend(streaming.push(b"\\after"));
        output.extend(streaming.finish());

        assert_eq!(output, b"safeafter");
        assert!(!output.windows(3).any(|window| window == b"\x9b2J"));
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
    fn oversized_string_after_cancelled_control_does_not_reuse_a_stale_drop_root() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for second in [b"\x1b]0;".as_slice(), b"\x1bP".as_slice()] {
            let mut input = b"pre\x1b[1\x1b]0;".to_vec();
            input.extend(std::iter::repeat_n(
                b'a',
                super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
            ));
            input.push(0x07);
            input.extend_from_slice(second);
            input.extend(std::iter::repeat_n(
                b'b',
                super::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
            ));
            input.extend_from_slice(b"\x1b\\visible\n");

            let mut whole = StreamingHighlighter::new(highlighter.clone());
            let mut output = whole.push(&input);
            output.extend(whole.finish());

            let mut chunked = StreamingHighlighter::new(highlighter.clone());
            let mut reference = Vec::new();
            for chunk in input.chunks(8192) {
                reference.extend(chunked.push(chunk));
            }
            reference.extend(chunked.finish());

            assert_eq!(output, b"previsible\n");
            assert_eq!(output, reference);
        }
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

    fn background_rule_highlighter() -> Highlighter {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: bad status\n    regex: '\\berror\\b'\n    color: f#ffffff b#ff0000\n",
        )
        .expect("config parses");
        Highlighter::from_config(config).expect("highlighter builds")
    }

    // A line terminator at the bottom of the screen scrolls in a blank line
    // filled with the current background, so a background highlight must end
    // before it, not at the first text of the next line.
    #[test]
    fn interactive_background_highlight_ends_before_the_line_terminator() {
        for chunks in [
            vec!["disk error\r\n    retrying\r\n"],
            vec!["disk error", "\r\n    retrying\r\n"],
        ] {
            let mut streaming =
                StreamingHighlighter::new_interactive(background_rule_highlighter());
            streaming.set_no_minimal_resets(false);
            let mut output = String::new();
            for chunk in &chunks {
                output.push_str(&streaming.push_str(chunk));
                // Surface the held-back token, so a split read leaves the
                // highlight open until the next read brings the terminator.
                output.push_str(
                    &String::from_utf8(streaming.flush_buffered_echo()).expect("output is utf8"),
                );
            }
            output.push_str(&String::from_utf8(streaming.finish()).expect("output is utf8"));

            assert!(
                output.contains("error\x1b[39;49m\r\n    retrying"),
                "{chunks:?}: {output:?}"
            );
        }
    }

    #[test]
    fn interactive_background_highlight_does_not_cover_trailing_spaces() {
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

        assert!(output.contains("up\x1b[24;39;49m down"), "{output:?}");
    }

    // An erase fills cells with the current background; a highlight still
    // active at a program's `ESC [ K` must not paint the rest of the line.
    #[test]
    fn background_highlight_is_reset_before_an_erase_sequence() {
        let input = b"\x1b[01;31m\x1b[Kerror\x1b[m\x1b[K on sda\n";
        for interactive in [false, true] {
            let mut streaming = if interactive {
                StreamingHighlighter::new_interactive(background_rule_highlighter())
            } else {
                StreamingHighlighter::new(background_rule_highlighter())
            };
            let mut output = streaming.push(input);
            output.extend(streaming.finish());
            let output = String::from_utf8(output).expect("output is utf8");

            let erase = output
                .match_indices("\x1b[K")
                .nth(1)
                .map(|(idx, _)| idx)
                .expect("second erase is present");
            let last_sgr = output[..erase]
                .rsplit("\x1b[")
                .next()
                .expect("an SGR precedes the erase");
            assert!(
                last_sgr == "0m" || last_sgr.ends_with("49m"),
                "interactive={interactive}: {output:?}"
            );
            assert_eq!(super::strip_ansi(output.as_bytes()), b"error on sda\n");
        }
    }

    // Index, next line and reverse index scroll in a blank line at a margin,
    // which the terminal fills with the current background as for a line feed.
    #[test]
    fn background_highlight_is_reset_before_an_index_control() {
        for control in [
            b"\x1bD".as_slice(),
            b"\x1bE",
            b"\x1bM",
            b"\x84",
            b"\x85",
            b"\x8d",
            b"\xc2\x84",
            b"\xc2\x85",
            b"\xc2\x8d",
        ] {
            let mut input = b"disk error".to_vec();
            input.extend_from_slice(control);
            input.extend_from_slice(b" retrying\n");
            for interactive in [false, true] {
                let mut streaming = if interactive {
                    StreamingHighlighter::new_interactive(background_rule_highlighter())
                } else {
                    StreamingHighlighter::new(background_rule_highlighter())
                };
                let mut output = streaming.push(&input);
                output.extend(streaming.finish());

                let control_at = output
                    .windows(control.len())
                    .position(|window| window == control)
                    .expect("control is passed through");
                let before = String::from_utf8_lossy(&output[..control_at]);
                let last_sgr = before
                    .rsplit("\x1b[")
                    .next()
                    .expect("an SGR precedes the control");
                assert!(
                    before.contains("48;2;255;0;0merror")
                        && (last_sgr == "0m" || last_sgr.ends_with("49m")),
                    "interactive={interactive} control={control:02x?}: {:?}",
                    String::from_utf8_lossy(&output)
                );
            }
        }
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
            b"\x1b[1\x1b]0;ended title\x1b[".as_slice(),
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

        // A complete 7-bit control ends the string as it would in a terminal,
        // so neither is unfinished at EOF.
        let input = b"safe prefix\x1b]0;title ended by a complete csi \x1b[31m";
        let mut streaming = StreamingHighlighter::new_interactive(highlighter);
        let mut output = streaming.push(input);
        output.extend(streaming.finish());
        assert_eq!(output, input);
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

    // `F0 8E` can never become a character, so its `8E` is a single shift and
    // the `9B` after it a CSI that end of input must neutralize, not bytes of
    // a character still waiting for its end. E0 and F4 take narrower first
    // continuations too, so `E0 9D` and `F4 90` start an OSC and a DCS.
    #[test]
    fn finish_neutralizes_controls_after_bytes_that_cannot_be_a_character() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for input in [
            b"\x8e\xf0\x8e\x9b".as_slice(),
            b"\x1b\xf0\x8e\x9b".as_slice(),
            b"\x1b[\xe0\x9d".as_slice(),
            b"\x9b\xf4\x90".as_slice(),
        ] {
            let mut streaming = StreamingHighlighter::new(highlighter.clone());
            let mut output = streaming.push(input);
            output.extend(streaming.finish());

            assert_eq!(
                super::incomplete_top_level_escape_start(&output),
                None,
                "{input:02x?} left a control open: {output:02x?}"
            );
        }
        assert_eq!(super::incomplete_utf8_start(b"x\xf0\x8e"), None);
        assert_eq!(super::incomplete_utf8_start(b"x\xf0\x9f"), Some(1));
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

    // What xterm draws after each control sequence: RIS and DECSTR reset the
    // rendition, DECSTR also the copy saved for the current screen, DECSC and
    // setting mode 1048 or 1049 save it and DECRC and resetting them restore
    // it, any number of times. The normal and the alternate screen keep
    // separate saved copies, and leaving mode 1049 restores the normal one.
    #[test]
    fn native_state_follows_terminal_resets_saves_and_restores() {
        for (input, expected) in [
            (b"\x1b[33m\x1bc".as_slice(), None),
            (b"\x1b[33m\x1b[!p", None),
            (b"\x1b[33m\x1b[1!p", None),
            (b"\x1b[33m\x9b!p", None),
            (b"\x1b[33m\x1b7\x1b[31m\x1b8", Some("\x1b[33m")),
            (b"\x1b[33m\x1b7\x1b[31m\x1b8\x1b[32m\x1b8", Some("\x1b[33m")),
            (b"\x1b[33m\x1b8", None),
            (b"\x1b[33m\x1b7\x1b[31m\x1b[!p\x1b8", None),
            (b"\x1b[33m\x1b7\x1b[31m\x1bc\x1b8", None),
            (b"\x1b[33m\x1b7\x1b[31m\x1b 8", Some("\x1b[31m")),
            // SGR 0 resets the rendition but not the saved copy.
            (b"\x1b[33m\x1b7\x1b[0m\x1b8", Some("\x1b[33m")),
            (b"\x1b[?1049h\x1b[33m\x1b[?1049l", None),
            (b"\x1b[33m\x1b[?1049h\x1b[31m\x1b[?1049l", Some("\x1b[33m")),
            (
                b"\x1b[33m\xc2\x9b?1049h\x1b[31m\x9b?1049l",
                Some("\x1b[33m"),
            ),
            (
                b"\x1b[33m\x1b[?25;1049h\x1b[31m\x1b[?1049;25l",
                Some("\x1b[33m"),
            ),
            (b"\x1b[33m\x1b7\x1b[31m\x1b[?1049l", Some("\x1b[33m")),
            (b"\x1b[33m\x1b[?1048h\x1b[31m\x1b[?1048l", Some("\x1b[33m")),
            (b"\x1b[33m\x1b[?1047h\x1b[31m\x1b[?1047l", Some("\x1b[31m")),
            (b"\x1b[33m\x1b[?47h\x1b[31m\x1b[?47l", Some("\x1b[31m")),
            (
                b"\x1b[33m\x1b[?1049h\x1b[32m\x1b7\x1b[31m\x1b[?1049l",
                Some("\x1b[33m"),
            ),
            (
                b"\x1b[33m\x1b7\x1b[?1049h\x1b[32m\x1b7\x1b[34m\x1b8",
                Some("\x1b[32m"),
            ),
            (
                b"\x1b[35m\x1b7\x1b[?1047h\x1b[32m\x1b7\x1b[?1047l\x1b8",
                Some("\x1b[35m"),
            ),
            (
                b"\x1b[35m\x1b7\x1b[31m\x1b[?1049h\x1b[?1049l\x1b[33m\x1b8",
                Some("\x1b[31m"),
            ),
            (
                b"\x1b[33m\x1b[?1049h\x1b[31m\x1b[!p\x1b[?1049l",
                Some("\x1b[33m"),
            ),
            (b"\x1b[33m\x1b[?1049h\x1b[32m\x1b7\x1b[!p\x1b8", None),
            // CSI s and CSI u with no parameters save and restore like DECSC
            // and DECRC; with parameters or a prefix they are other controls.
            (b"\x1b[33m\x1b[s\x1b[31m\x1b[u", Some("\x1b[33m")),
            (b"\x1b[33m\x9bs\x1b[31m\xc2\x9bu", Some("\x1b[33m")),
            (b"\x1b[33m\x1b7\x1b[31m\x1b[u", Some("\x1b[33m")),
            (b"\x1b[33m\x1b[s\x1b[31m\x1b8", Some("\x1b[33m")),
            (b"\x1b[33m\x1b[s\x1b[31m\x1b[>1u\x1b[<u", Some("\x1b[31m")),
            (b"\x1b[33m\x1b[s\x1b[31m\x1b[?u", Some("\x1b[31m")),
            // A terminal runs C0 controls after ESC and then the control.
            (b"\x1b\x00[33m", Some("\x1b[33m")),
            (b"\x1b[33m\x1b\x00c", None),
            (b"\x1b[33m\x1b\x00[!p", None),
            (b"\x1b[33m\x1b\x007\x1b[31m\x1b\r\n8", Some("\x1b[33m")),
            (b"\x1b[33m\x1b\x7f[s\x1b[31m\x1b\x00[u", Some("\x1b[33m")),
            (
                b"\x1b[33m\x1b\x00[?1049h\x1b[31m\x1b\x00[?1049l",
                Some("\x1b[33m"),
            ),
        ] {
            let mut state = NativeSgrState::default();
            for token in super::tokenize_ansi(input) {
                if let super::Token::Ansi(sequence) = token {
                    state.apply_sequence(&sequence);
                }
            }
            assert_eq!(
                state.ansi_start().as_deref(),
                expected,
                "after {input:02x?}"
            );
        }
    }

    /// The foreground xterm draws each visible byte of `bytes` in, modeled
    /// apart from `NativeSgrState`: SGR sets it, RIS and DECSTR reset it, and
    /// DECSC, CSI s and modes 1048 and 1049 save it and DECRC, CSI u and those
    /// modes restore it, with a saved copy per screen.
    fn drawn_foregrounds(bytes: &[u8]) -> Vec<(u8, Option<String>)> {
        let mut foreground: Option<String> = None;
        let mut saved: [Option<String>; 2] = [None, None];
        let mut screen = 0;
        let mut drawn = Vec::new();
        for token in super::tokenize_ansi(bytes) {
            let control = match token {
                super::Token::Text(text) => {
                    drawn.extend(text.iter().map(|byte| (*byte, foreground.clone())));
                    continue;
                }
                super::Token::Ansi(control) => control,
            };
            // A terminal runs C0 controls between ESC and the next byte.
            let mut control = control;
            while control.len() > 1
                && control[0] == 0x1b
                && matches!(control[1], 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f)
            {
                control.remove(1);
            }
            match control.as_slice() {
                b"\x1bc" => {
                    foreground = None;
                    saved = [None, None];
                    screen = 0;
                }
                b"\x1b[!p" => {
                    foreground = None;
                    saved[screen] = None;
                }
                b"\x1b7" | b"\x1b[s" | b"\x1b[?1048h" => saved[screen] = foreground.clone(),
                b"\x1b8" | b"\x1b[u" | b"\x1b[?1048l" => foreground = saved[screen].clone(),
                b"\x1b[?1049h" => {
                    saved[screen] = foreground.clone();
                    screen = 1;
                }
                b"\x1b[?1049l" => {
                    screen = 0;
                    foreground = saved[0].clone();
                }
                b"\x1b[?47h" | b"\x1b[?1047h" => screen = 1,
                b"\x1b[?47l" | b"\x1b[?1047l" => screen = 0,
                sgr if sgr.starts_with(b"\x1b[") && sgr.ends_with(b"m") => {
                    let parameters =
                        std::str::from_utf8(&sgr[2..sgr.len() - 1]).expect("test SGR is ASCII");
                    let codes = parameters.split(';').collect::<Vec<_>>();
                    let mut idx = 0;
                    while idx < codes.len() {
                        match codes[idx] {
                            "" | "0" | "39" => foreground = None,
                            "38" => {
                                let len = if codes[idx + 1] == "5" { 3 } else { 5 };
                                foreground = Some(codes[idx..idx + len].join(";"));
                                idx += len - 1;
                            }
                            code if code.len() == 2 && code.starts_with('3') => {
                                foreground = Some(code.to_string());
                            }
                            _ => {}
                        }
                        idx += 1;
                    }
                }
                _ => {}
            }
        }
        drawn
    }

    // However reads split the output, highlighting must leave every byte it
    // does not highlight in the color the program drew it in, through the
    // terminal's resets, saves and restores of that color.
    #[test]
    fn highlighting_keeps_program_colors_through_terminal_resets_and_saves() {
        const FRAGMENTS: &[&[u8]] = &[
            b"\x1b[31m",
            b"\x1b[32m",
            b"\x1b[33m",
            b"\x1b[39m",
            b"\x1b[0m",
            b"\x1b[1m",
            b"\x1b7",
            b"\x1b8",
            b"\x1b[s",
            b"\x1b[u",
            b"\x1b[>1u",
            b"\x1b\x007",
            b"\x1b\x008",
            b"\x1b\r8",
            b"\x1b\x00c",
            b"\x1b\x00[32m",
            b"\x1b\x00[s",
            b"\x1b\x7f[u",
            b"\x1b\x00[!p",
            b"\x1b\x00[?1049h",
            b"\x1b\x00[?1049l",
            b"\x1bc",
            b"\x1b[!p",
            b"\x1b[?1048h",
            b"\x1b[?1048l",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[?1047h",
            b"\x1b[?1047l",
            b"\x1b[?47h",
            b"\x1b[?47l",
            b"error",
            b" ",
            b"x",
            b"\n",
        ];
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: bad status\n    regex: '\\berror\\b'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");

        let mut seed = 0x5eed_c010_4ba5_e5e1_u64;
        let mut below = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        for _ in 0..1000 {
            let mut input = Vec::new();
            for _ in 0..1 + below(40) {
                input.extend_from_slice(FRAGMENTS[below(FRAGMENTS.len())]);
            }
            let program = drawn_foregrounds(&input);
            let visible = program.iter().map(|(byte, _)| *byte).collect::<Vec<_>>();
            let mut highlightable = vec![false; visible.len()];
            for (start, _) in visible
                .windows(b"error".len())
                .enumerate()
                .filter(|(_, window)| *window == b"error")
            {
                highlightable[start..start + b"error".len()].fill(true);
            }

            let mut random = Vec::new();
            let mut start = 0;
            while start < input.len() {
                let end = input.len().min(start + 1 + below(16));
                random.push(&input[start..end]);
                start = end;
            }
            for chunks in [vec![input.as_slice()], random, input.chunks(1).collect()] {
                for interactive in [false, true] {
                    let mut streaming = if interactive {
                        StreamingHighlighter::new_interactive(highlighter.clone())
                    } else {
                        StreamingHighlighter::new(highlighter.clone())
                    };
                    let mut output = Vec::new();
                    for chunk in &chunks {
                        output.extend(streaming.push(chunk));
                    }
                    output.extend(streaming.finish());

                    let drawn = drawn_foregrounds(&output);
                    assert_eq!(
                        drawn.iter().map(|(byte, _)| *byte).collect::<Vec<_>>(),
                        visible,
                        "visible text changed for {input:02x?}"
                    );
                    for (idx, ((byte, color), (_, program_color))) in
                        drawn.iter().zip(&program).enumerate()
                    {
                        if highlightable[idx] || matches!(byte, b' ' | b'\n') {
                            continue;
                        }
                        assert_eq!(
                            color,
                            program_color,
                            "byte {idx} of {input:02x?} in {} reads (interactive: {interactive}): {output:02x?}",
                            chunks.len()
                        );
                    }
                }
            }
        }
    }

    // Pins the style starts emitted at transitions and re-emitted after an
    // escape inside a highlight, in both emit paths and color modes.
    #[test]
    fn style_starts_are_emitted_at_transitions_and_after_escapes() {
        let yaml = "rules:\n  - description: state\n    regex: '\\bup\\b'\n    color: f#00ff00\n  - description: wan\n    regex: '\\bwan\\b'\n    color: f#ff0000 b#0000ff underline\n";
        let input = b"is \x1b[32mup\x1b[0m, is \x1b[32mup\x1b[0m, u\x1b[1mp w\x1b]0;t\x07an,up\n";
        let render = |color_mode, interactive: bool| {
            let config = PrismConfig::from_chromaterm_yaml(yaml).expect("config parses");
            let highlighter = Highlighter::from_config_with_color_mode(config, color_mode)
                .expect("rules compile");
            let mut streaming = if interactive {
                StreamingHighlighter::new_interactive(highlighter)
            } else {
                StreamingHighlighter::new(highlighter)
            };
            let mut output = streaming.push(input);
            output.extend(streaming.finish());
            String::from_utf8(output).expect("utf8")
        };

        assert_eq!(
            render(crate::style::ColorMode::TrueColor, false),
            concat!(
                "is \x1b[32m\x1b[38;2;0;255;0mup\x1b[0m\x1b[38;2;0;255;0m\x1b[0m, ",
                "is \x1b[32m\x1b[38;2;0;255;0mup\x1b[0m\x1b[38;2;0;255;0m\x1b[0m, ",
                "\x1b[38;2;0;255;0mu\x1b[1m\x1b[38;2;0;255;0mp\x1b[0m\x1b[1m ",
                "\x1b[4;38;2;255;0;0;48;2;0;0;255mw\x1b]0;t\x07\x1b[4;38;2;255;0;0;48;2;0;0;255man",
                "\x1b[0m\x1b[1m,\x1b[38;2;0;255;0mup\x1b[0m\x1b[1m\n",
            )
        );
        assert_eq!(
            render(crate::style::ColorMode::Xterm256, false),
            concat!(
                "is \x1b[32m\x1b[38;5;46mup\x1b[0m\x1b[38;5;46m\x1b[0m, ",
                "is \x1b[32m\x1b[38;5;46mup\x1b[0m\x1b[38;5;46m\x1b[0m, ",
                "\x1b[38;5;46mu\x1b[1m\x1b[38;5;46mp\x1b[0m\x1b[1m ",
                "\x1b[4;38;5;196;48;5;21mw\x1b]0;t\x07\x1b[4;38;5;196;48;5;21man",
                "\x1b[0m\x1b[1m,\x1b[38;5;46mup\x1b[0m\x1b[1m\n",
            )
        );
        assert_eq!(
            render(crate::style::ColorMode::TrueColor, true),
            concat!(
                "is \x1b[32m\x1b[38;2;0;255;0mup\x1b[0m\x1b[38;2;0;255;0m\x1b[39m, ",
                "is \x1b[32m\x1b[38;2;0;255;0mup\x1b[0m\x1b[38;2;0;255;0m\x1b[39m, ",
                "\x1b[38;2;0;255;0mu\x1b[1m\x1b[38;2;0;255;0mp ",
                "\x1b[4;38;2;255;0;0;48;2;0;0;255mw\x1b]0;t\x07\x1b[4;38;2;255;0;0;48;2;0;0;255man",
                "\x1b[24;39;49m,\x1b[38;2;0;255;0mup\n\x1b[39m",
            )
        );
        assert_eq!(
            render(crate::style::ColorMode::Ansi16, true),
            concat!(
                "is \x1b[32m\x1b[92mup\x1b[0m\x1b[92m\x1b[39m, ",
                "is \x1b[32m\x1b[92mup\x1b[0m\x1b[92m\x1b[39m, ",
                "\x1b[92mu\x1b[1m\x1b[92mp ",
                "\x1b[4;91;104mw\x1b]0;t\x07\x1b[4;91;104man",
                "\x1b[24;39;49m,\x1b[92mup\n\x1b[39m",
            )
        );
    }

    // An SGR start only adds to what is drawn, so switching the interactive
    // overlay to another style must first end what the previous style set and
    // the next one does not, or it stays on the text after it.
    #[test]
    fn interactive_overlay_ends_the_previous_style_before_starting_another() {
        let render = |yaml: &str, input: &[u8], no_minimal_resets: bool| {
            let config = PrismConfig::from_chromaterm_yaml(yaml).expect("config parses");
            let highlighter =
                Highlighter::from_config_with_color_mode(config, crate::style::ColorMode::Xterm256)
                    .expect("rules compile");
            let mut streaming = StreamingHighlighter::new_interactive(highlighter);
            streaming.set_no_minimal_resets(no_minimal_resets);
            let mut output = streaming.push(input);
            output.extend(streaming.finish());
            String::from_utf8(output).expect("utf8")
        };

        let background_then_foreground = "rules:\n  - description: host\n    regex: '\\bhost\\d+'\n    color: f#ffffff b#003366\n  - description: port\n    regex: ':\\d+\\b'\n    color: f#00ff00\n";
        assert_eq!(
            render(background_then_foreground, b"host1:22 open\nnext\n", false),
            "\x1b[38;5;231;48;5;23mhost1\x1b[39;49m\x1b[38;5;46m:22 \x1b[39mopen\nnext\n",
        );

        let bold_then_plain = "rules:\n  - description: down\n    regex: '\\bdown\\b'\n    color: f#ff0000 bold\n  - description: port\n    regex: '\\bGi\\S+'\n    color: f#0087ff\n";
        let output = render(bold_then_plain, b"down Gi0/1\n", true);
        let gi = output.find("Gi0/1").expect("token is emitted");
        let before_gi = &output[..gi];
        assert!(
            before_gi.rfind("\x1b[0m") > before_gi.rfind("\x1b[1;"),
            "bold from `down` is not ended before `Gi0/1`: {output:?}"
        );

        // A switch between foreground-only styles needs no reset.
        let foreground_only = "rules:\n  - description: host\n    regex: '\\bhost\\d+'\n    color: f#ffffff\n  - description: port\n    regex: ':\\d+\\b'\n    color: f#00ff00\n";
        assert_eq!(
            render(foreground_only, b"host1:22 open\n", false),
            "\x1b[38;5;231mhost1\x1b[38;5;46m:22 \x1b[39mopen\n",
        );
    }

    #[test]
    fn emit_paths_render_style_starts_through_the_cache() {
        let source = include_str!("highlight.rs");
        for name in ["fn emit_highlighted(", "fn emit_interactive_highlighted("] {
            let function_source = source
                .split(name)
                .nth(1)
                .expect("function exists")
                .split("\n}\n")
                .next()
                .expect("function ends");

            assert!(
                !function_source.contains("ansi_start_with_mode"),
                "{name} renders style starts outside SgrStartCache"
            );
        }
    }

    #[test]
    fn primary_screen_streaming_reuses_the_tokenized_chunk() {
        let source = include_str!("highlight.rs");
        let fast_path_source = source
            .split("fn highlight_streaming_chunk")
            .nth(1)
            .expect("function exists")
            .split("let mut highlightable")
            .next()
            .expect("fast path precedes the token loop");
        assert!(fast_path_source.contains("self.highlight_prepared(input"));

        let split_source = source
            .split("fn split_prepared_pending")
            .nth(1)
            .expect("function exists")
            .split("\n}\n")
            .next()
            .expect("function ends");
        assert!(!split_source.contains(".clone()"));
    }

    #[test]
    fn sgr_start_cache_matches_direct_rendering_past_its_bound() {
        let styles = (0..40u8)
            .map(|step| crate::style::Style {
                foreground: Some(crate::style::Rgb {
                    r: step,
                    g: 255 - step,
                    b: 7,
                }),
                underline: step % 3 == 0,
                ..crate::style::Style::default()
            })
            .collect::<Vec<_>>();
        for color_mode in [
            crate::style::ColorMode::TrueColor,
            crate::style::ColorMode::Xterm256,
        ] {
            let mut cache = super::SgrStartCache::default();
            // In order, strided (so evicted entries come back), and repeated.
            for idx in (0..styles.len()).chain((0..styles.len()).map(|idx| (idx * 7) % 40)) {
                for _ in 0..2 {
                    assert_eq!(
                        cache.start(&styles[idx], color_mode),
                        styles[idx].ansi_start_with_mode(color_mode).as_bytes()
                    );
                }
            }
        }
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
    fn escape_before_a_multibyte_character_keeps_highlighted_output_utf8() {
        for rules in [
            "rules:\n  - regex: '\\b(up)\\b'\n    color: f#00ff00\n",
            "rules:\n  - regex: 'link.*now'\n    color: f#00ff00\n",
        ] {
            let config = PrismConfig::from_chromaterm_yaml(rules).expect("config parses");
            let highlighter = Highlighter::from_config(config).expect("rule compiles");
            for input in [
                "link up\u{1b}\u{e9} now\n",
                "link up\u{1b}(\u{e9} now\n",
                "link up\u{1b}\u{4e2d} now\n",
            ] {
                let output = highlighter.highlight_bytes(input.as_bytes());
                assert!(
                    std::str::from_utf8(&output).is_ok(),
                    "invalid UTF-8 for {input:?}: {output:02x?}"
                );
                assert!(std::str::from_utf8(&super::strip_ansi(&output)).is_ok());
                let _ = highlighter.highlight_str(input);
                let _ = StreamingHighlighter::new(highlighter.clone()).push_str(input);
            }
        }

        // The character stays visible text and the escape scanner agrees with
        // the tokenizer that the bare ESC is already complete.
        assert_eq!(
            super::strip_ansi("a\u{1b}\u{e9}".as_bytes()),
            "a\u{e9}".as_bytes()
        );
        assert_eq!(
            super::strip_ansi("a\u{1b}(\u{e9}".as_bytes()),
            "a\u{e9}".as_bytes()
        );
        assert_eq!(
            super::incomplete_top_level_escape_start("a\u{1b}\u{e9}".as_bytes()),
            None
        );
        assert_eq!(
            super::incomplete_top_level_escape_start("a\u{1b}(\u{e9}".as_bytes()),
            None
        );
        // A trailing C2 can still become a UTF-8 C1 control that cancels the
        // ESC, so the ESC is carried with it instead of being emitted alone.
        assert_eq!(super::incomplete_sanitize_start(b"a\x1b\xc2"), Some(1));
        assert_eq!(super::incomplete_sanitize_start(b"a\x1b\xe2\x90"), Some(1));
        assert_eq!(super::incomplete_sanitize_start(b"a\x1b(\xc2"), Some(1));
        assert_eq!(
            super::strip_string_escapes(b"a\x1b\xc2\x9d0;t\x07b"),
            b"ab".to_vec()
        );
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

    // On a terminal that reads UTF-8 the raw byte `9B` is an invalid byte and
    // what follows it is text, so no rendition is learned from it and none is
    // written after a highlight: output that held no ESC must not gain a live
    // SGR. An 8-bit terminal runs the same bytes, and there the rendition is
    // restored as it is for the 7-bit form on either terminal. Each row is
    // what the program sends before the highlighted words, which terminal
    // reads it, and what a filter stream (after its `ESC [ 0 m`) and an
    // interactive stream write to end a highlight.
    #[test]
    fn rendition_is_restored_after_a_highlight_only_from_controls_the_terminal_runs() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: state\n    regex: 'up|down'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");

        for (program, reads_utf8, filter_restore, interactive_restore) in [
            (b"\x9b8m".as_slice(), true, "", "\x1b[39m"),
            (b"\x9b8m", false, "\x1b[8m", "\x1b[39m"),
            (b"\x1b[8m", true, "\x1b[8m", "\x1b[39m"),
            (b"\x1b[8m", false, "\x1b[8m", "\x1b[39m"),
            (b"\x9b30m", true, "", "\x1b[39m"),
            (b"\x9b30m", false, "\x1b[30m", "\x1b[30m"),
            (b"\x1b[30m", true, "\x1b[30m", "\x1b[30m"),
            (b"\x1b[30m", false, "\x1b[30m", "\x1b[30m"),
            (b"\x9b30;40m", true, "", "\x1b[39m"),
            (b"\x9b30;40m", false, "\x1b[30;40m", "\x1b[30m"),
            // A restore, a reset or a screen switch the terminal does not run
            // leaves it drawing in the color the program set last.
            (b"\x1b[33m\x1b[s\x1b[31m\x9bu", true, "\x1b[31m", "\x1b[31m"),
            (
                b"\x1b[33m\x1b[s\x1b[31m\x9bu",
                false,
                "\x1b[33m",
                "\x1b[33m",
            ),
            (b"\x1b[33m\x9b!p", true, "\x1b[33m", "\x1b[33m"),
            (b"\x1b[33m\x9b!p", false, "", "\x1b[39m"),
            (
                b"\x1b[33m\x9b?1049h\x1b[31m\x9b?1049l",
                true,
                "\x1b[31m",
                "\x1b[31m",
            ),
            (
                b"\x1b[33m\x9b?1049h\x1b[31m\x9b?1049l",
                false,
                "\x1b[33m",
                "\x1b[33m",
            ),
        ] {
            super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(reads_utf8)));
            let input = [b"x ".as_slice(), program, b" link up and down\n"].concat();
            let expected_filter = [
                b"x ".as_slice(),
                program,
                b" link \x1b[38;2;255;0;0mup\x1b[0m",
                filter_restore.as_bytes(),
                b" and \x1b[38;2;255;0;0mdown\x1b[0m",
                filter_restore.as_bytes(),
                b"\n",
            ]
            .concat();
            let expected_interactive = [
                b"x ".as_slice(),
                program,
                b" link \x1b[38;2;255;0;0mup ",
                interactive_restore.as_bytes(),
                b"and \x1b[38;2;255;0;0mdown\n",
                interactive_restore.as_bytes(),
            ]
            .concat();

            // As read, and as `--sanitize` hands it on.
            for prepared in [input.clone(), super::strip_string_escapes(&input)] {
                let mut filter = StreamingHighlighter::new(highlighter.clone());
                let mut output = filter.push(&prepared);
                output.extend(filter.finish());
                assert_eq!(
                    output, expected_filter,
                    "filter stream, terminal reads UTF-8: {reads_utf8}, {input:02x?}"
                );

                let mut interactive = StreamingHighlighter::new_interactive(highlighter.clone());
                let mut output = interactive.push(&prepared);
                output.extend(interactive.finish());
                assert_eq!(
                    output, expected_interactive,
                    "interactive stream, terminal reads UTF-8: {reads_utf8}, {input:02x?}"
                );
            }
        }
        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(None));
    }

    // The same holds when a whole buffer is highlighted in one call.
    #[test]
    fn whole_buffer_highlighting_restores_only_a_rendition_the_terminal_runs() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: state\n    regex: 'up|down'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");

        for (program, reads_utf8, restore) in [
            (b"\x9b8m".as_slice(), true, ""),
            (b"\x9b8m", false, "\x1b[8m"),
            (b"\x1b[8m", true, "\x1b[8m"),
            (b"\x1b[8m", false, "\x1b[8m"),
        ] {
            super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(reads_utf8)));
            let input = [b"x ".as_slice(), program, b" link up and down\n"].concat();
            let expected = [
                b"x ".as_slice(),
                program,
                b" link \x1b[38;2;255;0;0mup\x1b[0m",
                restore.as_bytes(),
                b" and \x1b[38;2;255;0;0mdown\x1b[0m",
                restore.as_bytes(),
                b"\n",
            ]
            .concat();
            assert_eq!(
                highlighter.highlight_bytes(&input),
                expected,
                "terminal reads UTF-8: {reads_utf8}, {input:02x?}"
            );
        }
        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(None));
    }

    // `ESC % @` and DEC private mode 42 can take xterm out of UTF-8, and
    // without `--sanitize` both pass through. Whether the terminal reads
    // UTF-8 is still decided once, at startup: the terminal may ignore the
    // switch, and a switch that was honored once must not make the raw `9B`
    // of all later output a live control. So after either switch a raw `9B`
    // SGR is still not learned on a terminal that reads UTF-8, and on an
    // 8-bit terminal it is learned as before.
    #[test]
    fn decoding_switch_passed_through_does_not_make_a_raw_c1_csi_learned() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: state\n    regex: 'up|down'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");

        for (program, reads_utf8, filter_restore, interactive_restore) in [
            (b"\x1b%@\x9b8m".as_slice(), true, "", "\x1b[39m"),
            (b"\x1b%@\x9b8m", false, "\x1b[8m", "\x1b[39m"),
            (b"\x1b%@\x9b30m", true, "", "\x1b[39m"),
            (b"\x1b%@\x9b30m", false, "\x1b[30m", "\x1b[30m"),
            (b"\x1b[?42h\x9b8m", true, "", "\x1b[39m"),
            (b"\x1b[?42h\x9b8m", false, "\x1b[8m", "\x1b[39m"),
            (b"\x1b[?42h\x9b30m", true, "", "\x1b[39m"),
            (b"\x1b[?42h\x9b30m", false, "\x1b[30m", "\x1b[30m"),
        ] {
            super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(reads_utf8)));
            let input = [b"x ".as_slice(), program, b" link up and down\n"].concat();

            let mut filter = StreamingHighlighter::new(highlighter.clone());
            let mut output = filter.push(&input);
            output.extend(filter.finish());
            assert_eq!(
                output,
                [
                    b"x ".as_slice(),
                    program,
                    b" link \x1b[38;2;255;0;0mup\x1b[0m",
                    filter_restore.as_bytes(),
                    b" and \x1b[38;2;255;0;0mdown\x1b[0m",
                    filter_restore.as_bytes(),
                    b"\n",
                ]
                .concat(),
                "filter stream, terminal reads UTF-8: {reads_utf8}, {input:02x?}"
            );

            let mut interactive = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut output = interactive.push(&input);
            output.extend(interactive.finish());
            assert_eq!(
                output,
                [
                    b"x ".as_slice(),
                    program,
                    b" link \x1b[38;2;255;0;0mup ",
                    interactive_restore.as_bytes(),
                    b"and \x1b[38;2;255;0;0mdown\n",
                    interactive_restore.as_bytes(),
                ]
                .concat(),
                "interactive stream, terminal reads UTF-8: {reads_utf8}, {input:02x?}"
            );
        }
        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(None));
    }

    // A terminal that reads UTF-8 does not switch screens for a raw `9B`
    // sequence, so the stream stays on the screen it was on: a filter stream
    // goes on highlighting after an inert `9B ? 1049 h`, and stays off after
    // a real switch that only an inert `9B ? 1049 l` follows. An 8-bit
    // terminal runs both, and the 7-bit and `C2 9B` forms switch as before.
    #[test]
    fn screen_switch_is_followed_only_for_controls_the_terminal_runs() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: state\n    regex: 'error'\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");
        const PLAIN: &[u8] = b"error";
        const COLORED: &[u8] = b"\x1b[38;2;255;0;0merror\x1b[0m";

        for (enter, leave, reads_utf8, second, third) in [
            (
                b"\x9b?1049h".as_slice(),
                b"".as_slice(),
                true,
                COLORED,
                COLORED,
            ),
            (b"\x9b?1049h", b"", false, PLAIN, PLAIN),
            (b"\x9b?47h", b"", true, COLORED, COLORED),
            (b"\x9b?47h", b"", false, PLAIN, PLAIN),
            (b"\x1b[?1049h", b"", true, PLAIN, PLAIN),
            (b"\x1b[?1049h", b"", false, PLAIN, PLAIN),
            (b"\xc2\x9b?1049h", b"", true, PLAIN, PLAIN),
            (b"\xc2\x9b?1049h", b"", false, PLAIN, PLAIN),
            (b"\x1b[?1049h", b"\x9b?1049l", true, PLAIN, PLAIN),
            (b"\x1b[?1049h", b"\x9b?1049l", false, PLAIN, COLORED),
            (b"\x1b[?1049h", b"\x1b[?1049l", true, PLAIN, COLORED),
            (b"\x1b[?1049h", b"\x1b[?1049l", false, PLAIN, COLORED),
        ] {
            super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(reads_utf8)));
            let input = [
                b"first error\n".as_slice(),
                enter,
                b"second error\n",
                leave,
                b"third error\n",
            ]
            .concat();
            let expected = [
                b"first ".as_slice(),
                COLORED,
                b"\n",
                enter,
                b"second ",
                second,
                b"\n",
                leave,
                b"third ",
                third,
                b"\n",
            ]
            .concat();

            // As read, and as `--sanitize` hands it on.
            for prepared in [input.clone(), super::strip_string_escapes(&input)] {
                let mut filter = StreamingHighlighter::new(highlighter.clone());
                let mut output = filter.push(&prepared);
                output.extend(filter.finish());
                assert_eq!(
                    output, expected,
                    "terminal reads UTF-8: {reads_utf8}, {input:02x?}"
                );
            }
        }

        // An interactive stream highlights on either screen, but passes a
        // read that moves the cursor through unhighlighted on the normal one.
        for (enter, reads_utf8, positioned) in [
            (b"\x9b?1049h".as_slice(), true, PLAIN),
            (
                b"\x9b?1049h",
                false,
                b"\x1b[38;2;255;0;0merror\n\x1b[39m".as_slice(),
            ),
            (b"\x1b[?1049h", true, b"\x1b[38;2;255;0;0merror\n\x1b[39m"),
            (b"\x1b[?1049h", false, b"\x1b[38;2;255;0;0merror\n\x1b[39m"),
        ] {
            super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(reads_utf8)));
            let mut interactive = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut output = interactive.push(&[b"first\n".as_slice(), enter].concat());
            output.extend(interactive.push(b"\x1b[5Gsecond error\n"));
            output.extend(interactive.finish());
            let line_end: &[u8] = if positioned == PLAIN { b"\n" } else { b"" };
            assert_eq!(
                output,
                [
                    b"first\n".as_slice(),
                    enter,
                    b"\x1b[5Gsecond ",
                    positioned,
                    line_end,
                ]
                .concat(),
                "terminal reads UTF-8: {reads_utf8}, {enter:02x?}"
            );
        }
        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(None));
    }

    // A color after the prompt is taken out and the foreground reset before
    // the typed command, so the command is not drawn in it. On a terminal
    // that reads UTF-8 a raw `9B` is an isolated invalid byte in ordinary
    // text: it sets no color and is the first byte of the typed command, so
    // a color in front of it is taken out before it.
    #[test]
    fn prompt_color_is_neutralized_only_for_controls_the_terminal_runs() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        for (after_prompt, reads_utf8, full_resets, expected) in [
            (
                b"\x9b31mshow".as_slice(),
                true,
                false,
                b"\x9b31mshow".as_slice(),
            ),
            (b"\x9b31mshow", true, true, b"\x9b31mshow"),
            (b"\x9b31mshow", false, false, b"\x1b[39mshow"),
            (b"\x9b31mshow", false, true, b"\x1b[0mshow"),
            (b"\x1b[31mshow", true, false, b"\x1b[39mshow"),
            (b"\x1b[31mshow", false, false, b"\x1b[39mshow"),
            // Where `9B 0 m` is text, the 7-bit color goes before it.
            (b"\x1b[31m\x9b0mshow", true, false, b"\x1b[39m\x9b0mshow"),
            (b"\x1b[31m\x9b0mshow", false, false, b"\x1b[31m\x9b0mshow"),
        ] {
            super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(reads_utf8)));
            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            streaming.set_no_minimal_resets(full_resets);
            assert_eq!(streaming.push(b"FGT01 # "), b"FGT01 # ");

            let mut output = streaming.push(after_prompt);
            output.extend(streaming.finish());
            assert_eq!(
                output, expected,
                "terminal reads UTF-8: {reads_utf8}, {after_prompt:02x?}"
            );
        }
        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(None));
    }

    // Whether a C1 byte runs depends on the encoding the terminal is in and
    // on the terminal itself, so a removed SGR opened by one writes no off
    // in its place, in either locale class: the bytes are 1.4.1's. A raw
    // `9B` in a UTF-8 locale is text and is not removed at all.
    #[test]
    fn prompt_echo_off_is_not_taken_from_an_sgr_opened_by_a_c1_byte() {
        let highlighter =
            Highlighter::from_config(PrismConfig::default()).expect("empty config compiles");

        // Generated from the released 1.4.1 prismtty, stdin to a terminal,
        // in the C and en_US.UTF-8 locales.
        for (introducer, reads_utf8, expected) in [
            (
                b"\x9b".as_slice(),
                false,
                b"\x1b[44mR1# \x1b[39mshow int\r\n".as_slice(),
            ),
            (b"\x9b", true, b"\x1b[44mR1#\x9b0;1m show int\r\n"),
            (b"\xc2\x9b", false, b"\x1b[44mR1# \x1b[39mshow int\r\n"),
            (b"\xc2\x9b", true, b"\x1b[44mR1# \x1b[39mshow int\r\n"),
        ] {
            super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(reads_utf8)));
            let mut streaming = StreamingHighlighter::new_interactive(highlighter.clone());
            let mut input = b"\x1b[44mR1#".to_vec();
            input.extend_from_slice(introducer);
            input.extend_from_slice(b"0;1m show int\r\n");
            let mut output = streaming.push(&input);
            output.extend(streaming.finish());
            assert_eq!(
                output, expected,
                "terminal reads UTF-8: {reads_utf8}, {introducer:02x?}"
            );
        }
        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(None));
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

    // A terminal runs C0 controls between ESC and the byte that selects the
    // sequence, and the tokenizer keeps them in the control's token, so the
    // classifiers must read `ESC NUL [` as a CSI and `ESC NUL 7` as DECSC.
    #[test]
    fn escapes_with_c0_controls_after_esc_match_their_plain_forms() {
        assert_eq!(
            super::tokenize_ansi(b"\x1b\x00[?1049hx").len(),
            2,
            "the C0 control stays in the CSI token"
        );
        assert_eq!(
            super::alternate_screen_command(b"\x1b\x00[?1049h"),
            Some(true)
        );
        assert!(super::is_cursor_positioning_sequence(b"\x1b\x7f[2H"));
        assert!(super::saves_rendition(b"\x1b\x007"));
        assert!(super::saves_rendition(b"\x1b\r\n[s"));
        assert!(super::saves_rendition(b"\x1b\x00[?1049h"));
        assert!(super::is_index_control(b"\x1b\x00D"));
        assert!(!super::saves_rendition(b"\x1b\x00#7"));

        let (visible, ranges) = super::visible_byte_map_and_ansi_ranges(b"a\x1b\x00[31mb");
        assert_eq!(
            visible.iter().map(|byte| byte.byte).collect::<Vec<_>>(),
            b"ab"
        );
        assert!(ranges[0].is_sgr);
    }

    // A terminal runs a C0 control it meets inside a CSI, ignores DEL there,
    // and goes on reading the sequence, so the classifiers that compare what
    // follows the introducer must read past both. CAN and SUB cancel the
    // sequence instead: it then names nothing.
    #[test]
    fn csi_classifiers_read_past_controls_inside_the_sequence() {
        for control in [0x00u8, 0x07, b'\n', b'\r', 0x1f, 0x7f] {
            let with = |parts: &[&[u8]]| parts.join(&control);
            assert_eq!(
                super::alternate_screen_command(&with(&[b"\x1b[?10", b"49h"])),
                Some(true),
                "{control:#04x}"
            );
            assert_eq!(
                super::alternate_screen_command(&with(&[b"\x1b[", b"?", b"47", b"l"])),
                Some(false),
                "{control:#04x}"
            );
            assert_eq!(
                super::alternate_screen_command(&with(&[b"\x9b?25;10", b"47h"])),
                Some(true),
                "{control:#04x}"
            );
            assert!(super::saves_rendition(&with(&[b"\x1b[", b"s"])));
            assert!(super::saves_rendition(&with(&[b"\x1b[?10", b"48h"])));
            assert!(super::saves_rendition(&with(&[
                b"\xc2\x9b?104",
                b"9",
                b"h"
            ])));
            assert!(!super::saves_rendition(&with(&[b"\x1b[?10", b"48l"])));
            assert!(super::restores_cursor(&with(&[b"\x1b[", b"u"])));
            assert!(super::is_soft_terminal_reset(&with(&[
                b"\x1b[1", b"!", b"p"
            ])));
            assert!(super::contains_bracketed_paste_disable_tokens(&[
                super::Token::Ansi(with(&[b"\x1b[?20", b"04l"]))
            ]));

            // The rendition saved on the way into the alternate screen comes
            // back on the way out, and DECSTR still resets it.
            for (input, expected) in [
                (
                    with(&[b"\x1b[33m\x1b[?10", b"49h\x1b[31m\x1b[?1049", b"l"]),
                    Some("\x1b[33m"),
                ),
                (
                    with(&[b"\x1b[33m\x1b[", b"s\x1b[31m\x1b[", b"u"]),
                    Some("\x1b[33m"),
                ),
                (with(&[b"\x1b[33m\x1b[", b"!p"]), None),
            ] {
                let mut state = NativeSgrState::default();
                for token in super::tokenize_ansi(&input) {
                    if let super::Token::Ansi(sequence) = token {
                        state.apply_sequence(&sequence);
                    }
                }
                assert_eq!(
                    state.ansi_start().as_deref(),
                    expected,
                    "after {input:02x?}"
                );
            }
        }

        for cancel in [0x18u8, 0x1a] {
            let cancelled = |sequence: &[u8]| [sequence, &[cancel]].concat();
            assert_eq!(
                super::alternate_screen_command(&cancelled(b"\x1b[?1049")),
                None
            );
            assert!(!super::saves_rendition(&cancelled(b"\x1b[?1049")));
            assert!(!super::saves_rendition(&cancelled(b"\x1b[")));
            assert!(!super::restores_cursor(&cancelled(b"\x1b[")));
            assert!(!super::is_soft_terminal_reset(&cancelled(b"\x1b[!")));
        }
    }

    // A terminal reads a mode parameter as a number, so `01049` is mode 1049
    // as `1049` is. Every place that looks for the screen and save modes has
    // to read the spelling the same way, or a full-screen frame behind it is
    // highlighted like scrolling text. A number too long to be one of them
    // must not come out as one, and in a UTF-8 locale a raw `9B` is text, so
    // it starts no sequence to read a mode from.
    #[test]
    fn private_modes_are_read_as_numbers() {
        for (sequence, enable) in [
            (b"\x1b[?01049h".as_slice(), true),
            (b"\x1b[?0047h", true),
            (b"\x1b[?001047l", false),
            (b"\x1b[?1049;025h", true),
            (b"\x1b[?25;0001049l", false),
            (b"\x1b[?0\x001049h", true),
            (b"\x9b?01049h", true),
            (b"\xc2\x9b?01047l", false),
        ] {
            assert_eq!(
                super::alternate_screen_command(sequence),
                Some(enable),
                "{sequence:02x?}"
            );
        }
        for sequence in [
            b"\x1b[?01048h".as_slice(),
            b"\x1b[?001049h",
            b"\x1b[?1;01048h",
        ] {
            assert!(super::saves_rendition(sequence), "{sequence:02x?}");
        }
        for sequence in [
            // 2^16, 2^32 and 2^64 more than 1049, 1048 and 47.
            b"\x1b[?66585h".as_slice(),
            b"\x1b[?4294968345h",
            b"\x1b[?18446744073709552665h",
            b"\x1b[?4294968344h",
            b"\x1b[?65583h",
            b"\x1b[?10490h",
            b"\x1b[?104h",
            b"\x1b[?h",
            b"\x1b[?01049:1h",
            b"\x1b[?01049$p",
        ] {
            assert_eq!(
                super::alternate_screen_command(sequence),
                None,
                "{sequence:02x?}"
            );
            assert!(!super::saves_rendition(sequence), "{sequence:02x?}");
        }

        for (input, expected) in [
            // Saved by 01048 and by 01049, restored by the same spelling.
            (
                b"\x1b[33m\x1b[?01048h\x1b[31m\x1b[?01048l".as_slice(),
                Some("\x1b[33m"),
            ),
            (
                b"\x1b[33m\x1b[?01049h\x1b[31m\x1b[?001049l",
                Some("\x1b[33m"),
            ),
            // 0047 switches screens, and each screen keeps its own save.
            (
                b"\x1b[33m\x1b7\x1b[?0047h\x1b[31m\x1b7\x1b[?0047l\x1b8",
                Some("\x1b[33m"),
            ),
        ] {
            let mut state = NativeSgrState::default();
            for token in super::tokenize_ansi(input) {
                if let super::Token::Ansi(sequence) = token {
                    state.apply_sequence(&sequence);
                }
            }
            assert_eq!(
                state.ansi_start().as_deref(),
                expected,
                "after {input:02x?}"
            );
        }

        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(Some(true)));
        let raw =
            super::contains_alternate_screen_enable_tokens(&super::tokenize_ansi(b"\x9b?01049h"));
        let encoded = super::alternate_screen_command(b"\xc2\x9b?01049h");
        super::TERMINAL_READS_UTF8_IN_THIS_THREAD.with(|switch| switch.set(None));
        assert_eq!((raw, encoded), (false, Some(true)));
    }

    // xterm keeps 30 parameters. Past them it ignores each `;` and reads the
    // digits on into the 30th, so a 31st parameter is not a mode of its own:
    // behind 29 others, `1;01049` is 101049, which is no mode, and `10;49`
    // is 1049. Reading a 31st parameter as a mode would switch screens, and
    // save or restore the rendition, where the terminal does neither.
    #[test]
    fn private_modes_past_the_thirtieth_run_on_into_it() {
        let sequence = |front: usize, modes: &str, last: char| {
            format!("\x1b[?{}{modes}{last}", "0;".repeat(front)).into_bytes()
        };
        for (modes, switches) in [
            ("1;01049", false),
            ("1;1049", false),
            ("1049;5", false),
            ("1049;0", false),
            ("25;1047", false),
            ("3;47", false),
            ("0;1049", true),
            (";1049", true),
            ("0;0;0;01049", true),
            ("10;49", true),
            ("1;0;4;9", true),
            ("104;7", true),
            ("4;7", true),
            ("1049", true),
            ("1049;", true),
            ("01049;;", true),
        ] {
            assert_eq!(
                super::alternate_screen_command(&sequence(29, modes, 'h')),
                switches.then_some(true),
                "29 parameters, then {modes}"
            );
            assert_eq!(
                super::alternate_screen_command(&sequence(29, modes, 'l')),
                switches.then_some(false),
                "29 parameters, then {modes}"
            );
        }
        // In front of the 30th, each parameter is a mode of its own.
        for (front, modes) in [(28, "1049;5"), (0, "1049;1;2"), (28, "47;1;01049")] {
            assert_eq!(
                super::alternate_screen_command(&sequence(front, modes, 'h')),
                Some(true),
                "{front} parameters, then {modes}"
            );
        }

        for (modes, saves) in [
            ("1;01048", false),
            ("1048;5", false),
            ("1;01049", false),
            ("0;1048", true),
            ("10;48", true),
            ("1048", true),
        ] {
            assert_eq!(
                super::saves_rendition(&sequence(29, modes, 'h')),
                saves,
                "29 parameters, then {modes}"
            );
        }

        let rendition = |input: &[u8]| {
            let mut state = NativeSgrState::default();
            for token in super::tokenize_ansi(input) {
                if let super::Token::Ansi(sequence) = token {
                    state.apply_sequence(&sequence);
                }
            }
            state.ansi_start()
        };
        let stream = |save: &str, restore: &str| {
            [
                b"\x1b[33m".as_slice(),
                &sequence(29, save, 'h'),
                b"\x1b[31m",
                &sequence(29, restore, 'l'),
            ]
            .concat()
        };
        for (save, restore, expected) in [
            // Nothing is saved and nothing restored: the last color stays.
            ("1;01048", "1;01048", Some("\x1b[31m")),
            ("1;01049", "1;01049", Some("\x1b[31m")),
            // A real save is not restored by a 31st parameter.
            ("1048", "1;01048", Some("\x1b[31m")),
            // Digits that run on into mode 1048 save and restore.
            ("10;48", "0;1048", Some("\x1b[33m")),
            ("1049", "104;9", Some("\x1b[33m")),
        ] {
            assert_eq!(
                rendition(&stream(save, restore)).as_deref(),
                expected,
                "saved by {save}, restored by {restore}"
            );
        }

        // The stream behind a 31st parameter is not on another screen.
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: up\n    regex: '\\bup\\b'\n    color: f#00ff00\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");
        let colors = |modes: &str| {
            let mut input = b"before up\n".to_vec();
            input.extend_from_slice(&sequence(29, modes, 'h'));
            input.extend_from_slice(b"frame up\n");
            let mut streaming = StreamingHighlighter::new(highlighter.clone());
            let mut output = streaming.push(&input);
            output.extend(streaming.finish());
            output
                .windows(4)
                .filter(|window| window == b"\x1b[38")
                .count()
        };
        assert_eq!(colors("1;01049"), colors("1;0"));
        assert_eq!(colors("10;49"), colors("1049"));
    }

    // Whatever a stream does with its highlighting when the program
    // switches screens, the spelling of the mode must not change it.
    #[test]
    fn alternate_screen_mode_with_leading_zeros_suspends_highlighting_like_the_plain_one() {
        let config = PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: error\n    regex: error\n    color: f#ff0000\n",
        )
        .expect("config parses");
        let highlighter = Highlighter::from_config(config).expect("rule compiles");
        for (plain, zeros) in [("1049", "01049"), ("1047", "001047"), ("47", "0047")] {
            let stream = |mode: &str| {
                let input =
                    format!("error\n\x1b[?{mode}herror on the other screen\n\x1b[?{mode}lerror\n");
                let mut streaming = StreamingHighlighter::new(highlighter.clone());
                let mut output = streaming.push(input.as_bytes());
                output.extend(streaming.finish());
                String::from_utf8(output).expect("output is text")
            };
            let expected = stream(plain);
            assert!(expected.contains("\x1b[38"), "{plain}: {expected:?}");
            assert_eq!(
                stream(zeros),
                expected.replace(&format!("?{plain}"), &format!("?{zeros}")),
                "{zeros}"
            );
        }
    }

    // A terminal ignores a private-mode sequence once it meets a second
    // private marker or an intermediate byte, so such a sequence sets and
    // resets nothing: no screen switch, no saved rendition.
    #[test]
    fn malformed_private_mode_sequences_name_no_mode() {
        for sequence in [
            b"\x1b[?1049;?1h".as_slice(),
            b"\x1b[?10\x0049;?1h",
            b"\x1b[?1049;!h",
            b"\x1b[?1049 h",
            b"\x1b[?1048$h",
            b"\x1b[?1;1047>l",
            b"\x1b[??1049h",
            b"\x9b?47;?1l",
            b"\xc2\x9b?1048:1h",
        ] {
            assert_eq!(
                super::alternate_screen_command(sequence),
                None,
                "{sequence:02x?}"
            );
            assert!(!super::saves_rendition(sequence), "{sequence:02x?}");
            assert_eq!(
                super::private_mode_sequence(sequence),
                None,
                "{sequence:02x?}"
            );
        }

        for (input, expected) in [
            // The malformed save is ignored, so the restore brings back the
            // rendition that nothing saved: the default one.
            (
                b"\x1b[33m\x1b[?1048;?1h\x1b[31m\x1b[?1048l".as_slice(),
                None,
            ),
            (b"\x1b[33m\x1b[?1049;!h\x1b[31m\x1b[?1049l", None),
            // The malformed restore is ignored too.
            (
                b"\x1b[33m\x1b[?1048h\x1b[31m\x1b[?1048;?1l",
                Some("\x1b[31m"),
            ),
        ] {
            let mut state = NativeSgrState::default();
            for token in super::tokenize_ansi(input) {
                if let super::Token::Ansi(sequence) = token {
                    state.apply_sequence(&sequence);
                }
            }
            assert_eq!(
                state.ansi_start().as_deref(),
                expected,
                "after {input:02x?}"
            );
        }

        // Well-formed sequences still name their modes.
        assert_eq!(
            super::alternate_screen_command(b"\x1b[?25;1049h"),
            Some(true)
        );
        assert_eq!(super::alternate_screen_command(b"\x1b[?;47l"), Some(false));
        assert!(super::saves_rendition(b"\x1b[?1048;25h"));
    }

    // XTPUSHSGR saves the rendition, with or without parameters, in either
    // spelling and behind any CSI introducer. The pop, the palette stack
    // (`CSI # P`), DECSTR and a private-marker form save nothing.
    #[test]
    fn xtpushsgr_is_a_rendition_save() {
        for push in [
            b"\x1b[#{".as_slice(),
            b"\x1b[#p",
            b"\x1b[1;4;30;31#{",
            b"\x1b[;#p",
            b"\x9b#{",
            b"\xc2\x9b#{",
            b"\x1b\x00[#{",
            b"\x1b[1\x00;4#\n{",
        ] {
            assert!(super::saves_rendition(push), "{push:02x?}");
        }
        for other in [
            b"\x1b[#}".as_slice(),
            b"\x1b[#q",
            b"\x1b[#P",
            b"\x1b[#Q",
            b"\x1b[#|",
            b"\x1b[!p",
            b"\x1b[?#{",
            b"\x1b[1 #{",
            b"\x1b[#\x18",
            b"\x1b[{",
            b"\x1b[p",
        ] {
            assert!(!super::saves_rendition(other), "{other:02x?}");
        }
    }

    // Every push and pop spelling counts as a use of the SGR stack, a raw C1
    // one too; the color stack, the SGR report and DECSTR do not.
    #[test]
    fn sgr_stack_use_is_read_in_every_spelling() {
        for stack in [
            b"\x1b[#{".as_slice(),
            b"\x1b[#p",
            b"\x1b[1;4;30;31#{",
            b"\x1b[;#p",
            b"\x1b[#}",
            b"\x1b[#q",
            b"\x9b#p",
            b"\xc2\x9b#q",
            b"\x1b\x00[#}",
            b"\x1b[#\n}",
        ] {
            assert!(super::uses_sgr_stack(stack), "{stack:02x?}");
        }
        for other in [
            b"\x1b[#P".as_slice(),
            b"\x1b[#Q",
            b"\x1b[1;1;1;1#|",
            b"\x1b[!p",
            b"\x1b[p",
            b"\x1b[{",
            b"\x1b[#\x18",
            b"\x1b7",
        ] {
            assert!(!super::uses_sgr_stack(other), "{other:02x?}");
        }
    }

    // A deliberate over-match: xterm ignores `#` after a private marker or
    // another intermediate, but taking these for a use of the stack only
    // returns the session to 1.4.1's bytes. Narrowing it needs the parser
    // proof for every terminal with the stack.
    #[test]
    fn sgr_stack_use_deliberately_over_matches_forms_xterm_ignores() {
        for ignored_by_xterm in [
            b"\x1b[?#{".as_slice(),
            b"\x1b[!#p",
            b"\x1b[>#}",
            b"\x1b[1 #q",
        ] {
            assert!(
                super::uses_sgr_stack(ignored_by_xterm),
                "{ignored_by_xterm:02x?}"
            );
        }
    }

    // The state without offs lives while an off makes the two differ, ends
    // when they agree again, and is taken over, for good, at a push.
    #[test]
    fn tracked_state_without_offs_follows_the_offs_and_the_sgr_stack() {
        let mut state = NativeSgrState::default();
        state.apply_sequence(b"\x1b[44m");
        state.apply_prompt_echo_off(b"\x1b[49m");
        assert!(state.rendition.background.is_none());
        let without_offs = state
            .without_offs
            .as_deref()
            .expect("kept while they differ");
        assert_eq!(without_offs.rendition.background.as_deref(), Some("44"));
        state.apply_sequence(b"\x1b[0m");
        assert!(state.without_offs.is_none());

        state.apply_sequence(b"\x1b[7m");
        state.apply_prompt_echo_off(b"\x1b[27m");
        state.apply_sequence(b"\x1b[#p");
        assert!(state.sgr_stack_used);
        assert!(state.without_offs.is_none());
        assert!(state.rendition.attributes.contains_key(&7));
        assert!(state.prompt_echo_off(b"\x1b[0m").is_empty());
        state.apply_sequence(b"\x1bc");
        assert!(state.sgr_stack_used);
        assert!(state.prompt_echo_off(b"\x1b[0m").is_empty());
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
