//! Terminal highlighting engines.
//!
//! `Highlighter` applies compiled rules to complete byte slices or strings.
//! `StreamingHighlighter` keeps enough state to highlight chunked terminal
//! output without breaking ANSI escapes or interactive command echo.

use crate::config::{CaptureRef, PrismConfig, RuleSpec, RuleStyle};
use crate::style::{ColorMode, Style};
use pcre2::bytes::{Regex, RegexBuilder};
use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant};
use thiserror::Error;

const UNICODE_PROMPT_MARKERS: &[&str] = &["○", "●", "❯", "❮", "❱", "›", "»", "➜", "➤", "λ"];
pub(crate) const MAX_INCOMPLETE_ESCAPE_BYTES: usize = 16 * 1024;
const PCRE2_JIT_STACK_LIMIT_BYTES: usize = 32 * 1024;

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
}

/// Aggregate timing and match data collected in benchmark mode.
#[derive(Clone, Debug, Default)]
pub struct BenchmarkReport {
    rules: Vec<RuleBenchmark>,
    rule_index: HashMap<String, usize>,
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

    fn record(&mut self, description: &str, duration: Duration, match_count: usize) {
        if let Some(index) = self.rule_index.get(description).copied() {
            if let Some(rule) = self.rules.get_mut(index) {
                rule.duration += duration;
                rule.match_count += match_count;
            }
        } else {
            let index = self.rules.len();
            self.rules.push(RuleBenchmark {
                description: description.to_string(),
                duration,
                match_count,
            });
            self.rule_index.insert(description.to_string(), index);
        }
    }
}

#[derive(Clone, Debug)]
struct CompiledRule {
    description: String,
    regex: Regex,
    style: RuleStyle,
    exclusive: bool,
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
        let mut rules = Vec::with_capacity(config.rules.len());
        let mut rule_specs = config.rules;
        rule_specs.sort_by_key(|rule| !rule.exclusive);
        for rule in rule_specs {
            rules.push(compile_rule(rule)?);
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
        String::from_utf8(self.highlight_bytes(input.as_bytes()))
            .expect("highlighted UTF-8 input remains UTF-8")
    }

    /// Highlights bytes and returns bytes with ANSI styling.
    pub fn highlight_bytes(&self, input: &[u8]) -> Vec<u8> {
        self.highlight_bytes_with_benchmark(input, None)
    }

    /// Returns visible styled spans without emitting ANSI escape sequences.
    pub fn style_spans(&self, input: &[u8]) -> Vec<StyledSpan> {
        let tokens = tokenize_ansi(input);
        let visible = visible_bytes(&tokens);
        let styles = self.match_styles(&visible, None);
        collect_styled_spans(&visible, &styles)
    }

    fn highlight_bytes_with_benchmark(
        &self,
        input: &[u8],
        benchmark: Option<&mut BenchmarkReport>,
    ) -> Vec<u8> {
        self.highlight_bytes_with_reset_mode(input, benchmark, ResetMode::Full)
    }

    fn highlight_bytes_with_reset_mode(
        &self,
        input: &[u8],
        benchmark: Option<&mut BenchmarkReport>,
        reset_mode: ResetMode,
    ) -> Vec<u8> {
        let mut native_sgr = NativeSgrState::default();
        self.highlight_bytes_with_native_sgr(input, benchmark, reset_mode, &mut native_sgr)
    }

    fn highlight_bytes_with_native_sgr(
        &self,
        input: &[u8],
        benchmark: Option<&mut BenchmarkReport>,
        reset_mode: ResetMode,
        native_sgr: &mut NativeSgrState,
    ) -> Vec<u8> {
        let tokens = tokenize_ansi(input);
        let visible = visible_bytes(&tokens);
        let styles = self.match_styles(&visible, benchmark);
        emit_highlighted(&tokens, &styles, self.color_mode, reset_mode, native_sgr)
    }

    fn highlight_chunk_with_native_sgr(
        &self,
        chunk: &AnsiChunk,
        benchmark: Option<&mut BenchmarkReport>,
        reset_mode: ResetMode,
        native_sgr: &mut NativeSgrState,
    ) -> Vec<u8> {
        let styles = self.match_styles(chunk.visible_bytes(), benchmark);
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
    ) -> Vec<u8> {
        let styles = self.match_styles(chunk.visible_bytes(), benchmark);
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
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: false,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: detect_no_minimal_resets(),
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: false,
        }
    }

    /// Creates a streaming highlighter tuned for interactive PTY output.
    pub fn new_interactive(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: true,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: detect_no_minimal_resets(),
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: false,
        }
    }

    /// Creates a noninteractive streaming highlighter with benchmark collection enabled.
    pub fn new_with_benchmark(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: false,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: detect_no_minimal_resets(),
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: true,
        }
    }

    /// Creates an interactive streaming highlighter with benchmark collection enabled.
    pub fn new_interactive_with_benchmark(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            pending_from_prompt_echo_remainder: false,
            alternate_screen: false,
            passthrough_single_byte_chunks: true,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            no_minimal_resets: detect_no_minimal_resets(),
            benchmark: BenchmarkReport::default(),
            benchmark_enabled: true,
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

    /// Replaces the inner highlighter while preserving streaming and interactive state.
    pub fn replace_highlighter(&mut self, highlighter: Highlighter) {
        self.highlighter = highlighter;
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
        let mut combined = std::mem::take(&mut self.pending);
        if combined.is_empty() {
            return self.push_combined_chunk(chunk.clone());
        }
        combined.extend_from_slice(chunk.bytes());
        self.push_combined_chunk(AnsiChunk::new(combined))
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

    /// Flushes any buffered partial terminal sequence or token and returns the
    /// stream's final bytes.
    ///
    /// Callers must write the returned bytes: they are part of the output, not
    /// optional cleanup. For interactive streams the result may include a
    /// terminal reset (`ESC[39m`, or `ESC[0m` with minimal resets disabled)
    /// when a highlight style is still active at end of input, so subsequent
    /// program output is not left rendered in the highlighter's color. The
    /// result is empty when nothing is buffered and no style is active.
    /// Noninteractive streams never append this end-of-input reset.
    pub fn finish(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        let pending = AnsiChunk::new(pending);
        let mut output = self.highlight_output_chunk(&pending);
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
        let mut idx = 0;
        let bytes = input.bytes();

        while idx < bytes.len() {
            if matches!(bytes[idx], b'\r' | b'\n') {
                self.emit_interactive_line_segment(&bytes[segment_start..idx], &mut output);
                output.push(bytes[idx]);
                if bytes[idx] == b'\r' && bytes.get(idx + 1) == Some(&b'\n') {
                    output.push(b'\n');
                    idx += 1;
                }
                segment_start = idx + 1;
            }
            idx += 1;
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
            ));
        } else {
            output.extend(self.highlighter.highlight_chunk_with_native_sgr(
                &chunk,
                Some(&mut self.benchmark),
                ResetMode::Full,
                &mut NativeSgrState::default(),
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

fn detect_no_minimal_resets() -> bool {
    env::var_os("PRISMTTY_NO_39_49_RESETS").is_some()
        || env::var_os("PRISMTTY_NO_MINIMAL_RESET").is_some()
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

        if let Some((start, end)) = remove_ranges.get(remove_idx).copied()
            && idx == start
        {
            idx = end;
            remove_idx += 1;
            continue;
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
                    visible[line_start].raw.min(input.len())
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
        if input[idx] == 0x1b {
            let end = ansi_sequence_end(input, idx);
            ansi_ranges.push(AnsiRange {
                start: idx,
                end,
                is_sgr: input[idx..end].starts_with(b"\x1b[")
                    && input.get(end.saturating_sub(1)) == Some(&b'm'),
            });
            idx = end;
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
    let Ok(text) = std::str::from_utf8(trimmed) else {
        return false;
    };
    if !text.chars().all(|ch| !ch.is_control()) {
        return false;
    }

    let Some(marker) = UNICODE_PROMPT_MARKERS
        .iter()
        .copied()
        .find(|marker| text.ends_with(marker))
    else {
        return false;
    };

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

fn compile_rule(rule: RuleSpec) -> Result<CompiledRule, HighlightError> {
    let description = rule.description;
    let regex = RegexBuilder::new()
        .multi_line(true)
        .crlf(true)
        .jit_if_available(true)
        .max_jit_stack_size(Some(PCRE2_JIT_STACK_LIMIT_BYTES))
        .build(&rule.regex)
        .map_err(|source| HighlightError::Regex {
            description: description.clone(),
            source,
        })?;
    Ok(CompiledRule {
        description,
        regex,
        style: rule.style,
        exclusive: rule.exclusive,
    })
}

impl Highlighter {
    fn match_styles(
        &self,
        visible: &[u8],
        mut benchmark: Option<&mut BenchmarkReport>,
    ) -> Vec<Option<Style>> {
        let mut styles = vec![Style::default(); visible.len()];
        let mut protected = vec![false; visible.len()];

        for rule in &self.rules {
            let started = benchmark.as_ref().map(|_| Instant::now());
            let (matches, match_count) = match_rule(rule, visible);
            if let (Some(report), Some(started)) = (benchmark.as_deref_mut(), started) {
                report.record(&rule.description, started.elapsed(), match_count);
            }
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
        }

        styles
            .into_iter()
            .map(|style| (!style.is_empty()).then_some(style))
            .collect()
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

fn match_rule(rule: &CompiledRule, visible: &[u8]) -> (Vec<(usize, usize, Style)>, usize) {
    let mut ranges = Vec::new();
    let mut match_count = 0;
    for captures_result in rule.regex.captures_iter(visible) {
        let captures = match captures_result {
            Ok(captures) => captures,
            Err(_) => break,
        };
        match_count += 1;

        match &rule.style {
            RuleStyle::Whole(style) => {
                if let Some(matched) = captures.get(0) {
                    ranges.push((matched.start(), matched.end(), style.clone()));
                }
            }
            RuleStyle::Captures(capture_styles) => {
                for (group, style) in capture_styles {
                    let matched = match group {
                        CaptureRef::Index(index) => captures.get(*index),
                        CaptureRef::Name(name) => captures.name(name),
                    };
                    if let Some(matched) = matched {
                        ranges.push((matched.start(), matched.end(), style.clone()));
                    }
                }
            }
        }
    }

    (ranges, match_count)
}

fn tokenize_ansi(input: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut text = Vec::new();
    let mut idx = 0;

    while idx < input.len() {
        if input[idx] == 0x1b {
            if !text.is_empty() {
                tokens.push(Token::Text(std::mem::take(&mut text)));
            }
            let end = ansi_sequence_end(input, idx);
            tokens.push(Token::Ansi(input[idx..end].to_vec()));
            idx = end;
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

fn ansi_sequence_end(input: &[u8], start: usize) -> usize {
    if start + 1 >= input.len() {
        return input.len();
    }

    match input[start + 1] {
        b'[' => csi_sequence_end(input, start + 2),
        b']' => osc_sequence_end(input, start + 2),
        b'P' | b'X' | b'^' | b'_' => string_sequence_end(input, start + 2),
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' => {
            (start + 3).min(input.len())
        }
        _ => (start + 2).min(input.len()),
    }
}

fn csi_sequence_end(input: &[u8], mut idx: usize) -> usize {
    while idx < input.len() {
        let byte = input[idx];
        idx += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    idx
}

fn osc_sequence_end(input: &[u8], mut idx: usize) -> usize {
    while idx < input.len() {
        if input[idx] == 0x07 || input[idx] == 0x9c {
            return idx + 1;
        }
        if input[idx] == 0x1b && idx + 1 < input.len() && input[idx + 1] == b'\\' {
            return idx + 2;
        }
        idx += 1;
    }
    input.len()
}

fn c1_string_escape_end(input: &[u8], start: usize) -> usize {
    match input[start] {
        0x9d => osc_sequence_end(input, start + 1),
        0x90 | 0x98 | 0x9e | 0x9f => string_sequence_end(input, start + 1),
        _ => (start + 1).min(input.len()),
    }
}

fn string_sequence_end(input: &[u8], mut idx: usize) -> usize {
    while idx < input.len() {
        if input[idx] == 0x9c {
            return idx + 1;
        }
        if input[idx] == 0x1b && idx + 1 < input.len() && input[idx + 1] == b'\\' {
            return idx + 2;
        }
        idx += 1;
    }
    input.len()
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
    if !bytes.starts_with(b"\x1b[?") {
        return None;
    }
    let final_byte = *bytes.last()?;
    let enable = match final_byte {
        b'h' => true,
        b'l' => false,
        _ => return None,
    };
    let body = &bytes[3..bytes.len().saturating_sub(1)];
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
        Token::Ansi(bytes) => bytes == b"\x1b[?2004l",
        Token::Text(_) => false,
    })
}

fn is_cursor_positioning_sequence(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1b[")
        && matches!(
            bytes.last(),
            Some(b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'G' | b'H' | b'f')
        )
}

fn is_interactive_layout_boundary_sequence(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1b[")
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
    bold: bool,
    blink: bool,
    invert: bool,
    italic: bool,
    strike: bool,
    underline: bool,
}

impl NativeSgrState {
    fn apply_sequence(&mut self, bytes: &[u8]) {
        if !bytes.starts_with(b"\x1b[") || !bytes.ends_with(b"m") {
            return;
        }

        let body = &bytes[2..bytes.len() - 1];
        if body.is_empty() {
            self.reset_all();
            return;
        }

        let normalized = body
            .iter()
            .map(|byte| if *byte == b':' { b';' } else { *byte })
            .collect::<Vec<_>>();
        let codes = normalized
            .split(|byte| *byte == b';')
            .filter(|part| !part.is_empty())
            .filter_map(|part| std::str::from_utf8(part).ok())
            .filter_map(|part| part.parse::<u16>().ok())
            .collect::<Vec<_>>();

        if codes.is_empty() {
            self.reset_all();
            return;
        }

        let mut idx = 0;
        while idx < codes.len() {
            match codes[idx] {
                0 => self.reset_all(),
                1 => self.bold = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.invert = true,
                9 => self.strike = true,
                22 => self.bold = false,
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.invert = false,
                29 => self.strike = false,
                30..=37 | 90..=97 => self.foreground = Some(codes[idx].to_string()),
                40..=47 | 100..=107 => self.background = Some(codes[idx].to_string()),
                39 => self.foreground = None,
                49 => self.background = None,
                38 | 48 => {
                    let is_foreground = codes[idx] == 38;
                    let parsed = parse_extended_color(&codes[idx..]);
                    if let Some(code) = parsed.code {
                        if is_foreground {
                            self.foreground = Some(code);
                        } else {
                            self.background = Some(code);
                        }
                    }
                    idx += parsed.consumed;
                    continue;
                }
                _ => {}
            }
            idx += 1;
        }
    }

    fn ansi_start(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.bold {
            parts.push("1".to_string());
        }
        if self.italic {
            parts.push("3".to_string());
        }
        if self.underline {
            parts.push("4".to_string());
        }
        if self.blink {
            parts.push("5".to_string());
        }
        if self.invert {
            parts.push("7".to_string());
        }
        if self.strike {
            parts.push("9".to_string());
        }
        if let Some(foreground) = &self.foreground {
            parts.push(foreground.clone());
        }
        if let Some(background) = &self.background {
            parts.push(background.clone());
        }

        (!parts.is_empty()).then(|| format!("\x1b[{}m", parts.join(";")))
    }

    fn restore_after_interactive_style(&self, style: &Style) -> Vec<u8> {
        let mut parts = Vec::new();

        if style.bold {
            parts.push(if self.bold { "1" } else { "22" }.to_string());
        }
        if style.italic {
            parts.push(if self.italic { "3" } else { "23" }.to_string());
        }
        if style.underline {
            parts.push(if self.underline { "4" } else { "24" }.to_string());
        }
        if style.blink {
            parts.push(if self.blink { "5" } else { "25" }.to_string());
        }
        if style.invert {
            parts.push(if self.invert { "7" } else { "27" }.to_string());
        }
        if style.strike {
            parts.push(if self.strike { "9" } else { "29" }.to_string());
        }
        if style.foreground.is_some()
            && let Some(foreground) = &self.foreground
        {
            parts.push(foreground.clone());
        } else if style.foreground.is_some() {
            parts.push("39".to_string());
        }
        if style.background.is_some()
            && let Some(background) = &self.background
        {
            parts.push(background.clone());
        } else if style.background.is_some() {
            parts.push("49".to_string());
        }

        if parts.is_empty() {
            return Vec::new();
        }

        format!("\x1b[{}m", parts.join(";")).into_bytes()
    }

    fn reset_all(&mut self) {
        *self = Self::default();
    }
}

struct ExtendedColorParse {
    code: Option<String>,
    consumed: usize,
}

fn parse_extended_color(codes: &[u16]) -> ExtendedColorParse {
    let Some(target @ (38 | 48)) = codes.first().copied() else {
        return ExtendedColorParse {
            code: None,
            consumed: 1,
        };
    };

    match codes.get(1).copied() {
        Some(5) => {
            let code = codes
                .get(2)
                .copied()
                .filter(|color| *color <= 255)
                .map(|color| format!("{target};5;{color}"));
            ExtendedColorParse {
                code,
                consumed: codes.len().min(3),
            }
        }
        Some(2) => {
            let code = match (codes.get(2), codes.get(3), codes.get(4)) {
                (Some(red), Some(green), Some(blue))
                    if *red <= 255 && *green <= 255 && *blue <= 255 =>
                {
                    Some(format!("{target};2;{red};{green};{blue}"))
                }
                _ => None,
            };
            ExtendedColorParse {
                code,
                consumed: codes.len().min(5),
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

fn find_first_line_boundary(bytes: &[u8]) -> Option<usize> {
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'\r' {
            if idx + 1 < bytes.len() && bytes[idx + 1] == b'\n' {
                if idx + 2 < bytes.len() {
                    return Some(idx + 2);
                }
            } else if idx + 1 < bytes.len() {
                return Some(idx + 1);
            }
        } else if bytes[idx] == b'\n' && idx + 1 < bytes.len() {
            return Some(idx + 1);
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

    if let Some(escape_start) = incomplete_escape_start(bytes) {
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

/// The leading length of `pending` that is safe to surface now: up to a trailing
/// incomplete escape OR a trailing incomplete UTF-8 sequence, whichever begins
/// earlier. Their completing bytes arrive in the same burst, so emitting a
/// partial escape or codepoint would corrupt output.
fn flushable_pending_len(pending: &[u8]) -> usize {
    let escape = incomplete_escape_start(pending).unwrap_or(pending.len());
    let utf8 = incomplete_utf8_start(pending).unwrap_or(pending.len());
    escape.min(utf8)
}

fn ansi_sequence_end_containing(bytes: &[u8], index: usize) -> Option<usize> {
    let mut search_end = index.min(bytes.len());
    while search_end > 0 {
        let escape_start = bytes[..search_end].iter().rposition(|byte| *byte == 0x1b)?;
        let escape_end = ansi_sequence_end(bytes, escape_start);
        if escape_start < index && index < escape_end {
            return Some(escape_end);
        }
        search_end = escape_start;
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
        incomplete_escape_start(chunk.bytes()).unwrap_or(chunk.bytes.len())
    } else {
        streaming_split_at(chunk.bytes())
    }
}

pub(crate) fn incomplete_escape_start(bytes: &[u8]) -> Option<usize> {
    let start = bytes.iter().rposition(|byte| *byte == 0x1b)?;
    escape_is_incomplete_at(bytes, start).then_some(start)
}

fn escape_is_incomplete_at(bytes: &[u8], start: usize) -> bool {
    if start + 1 >= bytes.len() {
        return true;
    }

    // An unterminated escape longer than the cap is treated as complete so it is
    // flushed rather than carried across reads without bound. This protects the
    // streaming `pending`, `--strip-ansi` carry, and interactive echo buffers
    // (all reached through `incomplete_escape_start`) from a memory-exhaustion
    // vector in hostile device output.
    if bytes.len().saturating_sub(start) > MAX_INCOMPLETE_ESCAPE_BYTES {
        return false;
    }

    match bytes[start + 1] {
        b'[' => bytes[start + 2..]
            .iter()
            .any(|byte| (0x40..=0x7e).contains(byte))
            .then_some(())
            .is_none(),
        b']' => {
            let complete = bytes[start + 2..]
                .iter()
                .position(|byte| *byte == 0x07)
                .is_some()
                || bytes[start + 2..]
                    .windows(2)
                    .any(|window| window == b"\x1b\\");
            !complete
        }
        b'P' | b'X' | b'^' | b'_' => {
            let complete = bytes[start + 2..]
                .windows(2)
                .any(|window| window == b"\x1b\\");
            !complete
        }
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' => start + 2 >= bytes.len(),
        _ => false,
    }
}

pub(crate) fn incomplete_sanitize_start(bytes: &[u8]) -> Option<usize> {
    let escape = incomplete_escape_start(bytes).unwrap_or(bytes.len());
    let utf8 = incomplete_utf8_start(bytes).unwrap_or(bytes.len());
    let c1 = incomplete_c1_string_escape_start(bytes).unwrap_or(bytes.len());
    let split = escape.min(utf8).min(c1);
    (split < bytes.len()).then_some(split)
}

fn incomplete_c1_string_escape_start(bytes: &[u8]) -> Option<usize> {
    let mut search_end = bytes.len();
    while let Some(start) = bytes[..search_end]
        .iter()
        .rposition(|byte| is_c1_string_escape_start(*byte))
    {
        if byte_is_inside_valid_utf8_codepoint(bytes, start) {
            search_end = start;
            continue;
        }
        return c1_string_escape_is_incomplete_at(bytes, start).then_some(start);
    }
    None
}

fn byte_is_inside_valid_utf8_codepoint(bytes: &[u8], index: usize) -> bool {
    let search_start = index.saturating_sub(3);
    for start in search_start..index {
        if let Some(width) = valid_utf8_char_width_at(bytes, start)
            && start + width > index
        {
            return true;
        }
    }
    false
}

fn c1_string_escape_is_incomplete_at(bytes: &[u8], start: usize) -> bool {
    if bytes.len().saturating_sub(start) > MAX_INCOMPLETE_ESCAPE_BYTES {
        return false;
    }
    let complete = match bytes[start] {
        0x9d => {
            bytes[start + 1..]
                .iter()
                .any(|byte| *byte == 0x07 || *byte == 0x9c)
                || bytes[start + 1..]
                    .windows(2)
                    .any(|window| window == b"\x1b\\")
        }
        0x90 | 0x98 | 0x9e | 0x9f => {
            bytes[start + 1..].contains(&0x9c)
                || bytes[start + 1..]
                    .windows(2)
                    .any(|window| window == b"\x1b\\")
        }
        _ => true,
    };
    !complete
}

enum EscapeScan {
    Complete(usize),
    IncompleteWithinLimit,
    IncompleteOversized,
}

fn neutralize_oversized_incomplete_escape(bytes: &mut [u8]) -> bool {
    let mut changed = false;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let Some(relative_start) = bytes[idx..].iter().position(|byte| *byte == 0x1b) else {
            break;
        };
        let start = idx + relative_start;
        match scan_escape_for_neutralization(bytes, start) {
            EscapeScan::Complete(end) => idx = end.max(start + 1),
            EscapeScan::IncompleteWithinLimit => break,
            EscapeScan::IncompleteOversized => {
                bytes[start] = b'^';
                changed = true;
                idx = start + 1;
            }
        }
    }
    changed
}

fn scan_escape_for_neutralization(bytes: &[u8], start: usize) -> EscapeScan {
    if start + 1 >= bytes.len() {
        return incomplete_escape_scan_result(bytes, start);
    }

    match bytes[start + 1] {
        b'[' => scan_csi_for_neutralization(bytes, start),
        b']' => scan_st_terminated_for_neutralization(bytes, start, true),
        b'P' | b'X' | b'^' | b'_' => scan_st_terminated_for_neutralization(bytes, start, false),
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' => {
            if start + 3 <= bytes.len() {
                EscapeScan::Complete(start + 3)
            } else {
                incomplete_escape_scan_result(bytes, start)
            }
        }
        _ => EscapeScan::Complete((start + 2).min(bytes.len())),
    }
}

fn scan_csi_for_neutralization(bytes: &[u8], start: usize) -> EscapeScan {
    let mut idx = start + 2;
    while idx < bytes.len() {
        if idx.saturating_sub(start) > MAX_INCOMPLETE_ESCAPE_BYTES {
            return EscapeScan::IncompleteOversized;
        }
        let byte = bytes[idx];
        idx += 1;
        if (0x40..=0x7e).contains(&byte) {
            return EscapeScan::Complete(idx);
        }
    }
    incomplete_escape_scan_result(bytes, start)
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
        if allows_bel && bytes[idx] == 0x07 {
            return EscapeScan::Complete(idx + 1);
        }
        if bytes[idx] == 0x1b && idx + 1 < bytes.len() && bytes[idx + 1] == b'\\' {
            return EscapeScan::Complete(idx + 2);
        }
        idx += 1;
    }
    incomplete_escape_scan_result(bytes, start)
}

fn incomplete_escape_scan_result(bytes: &[u8], start: usize) -> EscapeScan {
    if bytes.len().saturating_sub(start) > MAX_INCOMPLETE_ESCAPE_BYTES {
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
        if input[idx] == 0x1b {
            let end = ansi_sequence_end(input, idx);
            if !is_string_escape(&input[idx..end]) {
                sanitized.extend_from_slice(&input[idx..end]);
            }
            idx = end;
            continue;
        }
        if let Some(width) = valid_utf8_char_width_at(input, idx) {
            sanitized.extend_from_slice(&input[idx..idx + width]);
            idx += width;
            continue;
        }
        if is_c1_string_escape_start(input[idx]) {
            idx = c1_string_escape_end(input, idx);
        } else {
            sanitized.push(input[idx]);
            idx += 1;
        }
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

fn is_string_escape(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0x1b && matches!(bytes[1], b']' | b'P' | b'X' | b'^' | b'_')
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
    fn incomplete_escape_scan_checks_the_suffix_candidate() {
        assert_eq!(incomplete_escape_start(b"ok\x1b[31mstill\x1b["), Some(12));
        assert_eq!(incomplete_escape_start(b"ok\x1b[31m"), None);
    }

    #[test]
    fn incomplete_escape_scan_uses_reverse_search() {
        let source = include_str!("highlight.rs");
        let function_source = source
            .split("fn incomplete_escape_start")
            .nth(1)
            .expect("function exists")
            .split("fn escape_is_incomplete_at")
            .next()
            .expect("function ends before next helper");

        assert!(function_source.contains("rposition"));
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
