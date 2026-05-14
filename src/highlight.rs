//! Terminal highlighting engines.
//!
//! `Highlighter` applies compiled rules to complete byte slices or strings.
//! `StreamingHighlighter` keeps enough state to highlight chunked terminal
//! output without breaking ANSI escapes or interactive command echo.

use crate::config::{CaptureRef, PrismConfig, RuleSpec, RuleStyle};
use crate::style::{ColorMode, Style};
use pcre2::bytes::{Regex, RegexBuilder};
use std::time::{Duration, Instant};
use thiserror::Error;

const UNICODE_PROMPT_MARKERS: &[&str] = &["○", "●", "❯", "❮", "❱", "›", "»", "➜", "➤", "λ"];
const MAX_INCOMPLETE_ESCAPE_BYTES: usize = 16 * 1024;

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
    alternate_screen: bool,
    passthrough_single_byte_chunks: bool,
    prompt_echo_passthrough: bool,
    visible_line_tail: Vec<u8>,
    native_sgr: NativeSgrState,
    interactive_overlay: Option<Style>,
    benchmark: Option<BenchmarkReport>,
}

/// Aggregate timing and match data collected in benchmark mode.
#[derive(Clone, Debug, Default)]
pub struct BenchmarkReport {
    rules: Vec<RuleBenchmark>,
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
        if let Some(rule) = self
            .rules
            .iter_mut()
            .find(|rule| rule.description == description)
        {
            rule.duration += duration;
            rule.match_count += match_count;
            return;
        }

        self.rules.push(RuleBenchmark {
            description: description.to_string(),
            duration,
            match_count,
        });
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

impl Highlighter {
    /// Compiles a highlighter with true-color ANSI output.
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

    fn highlight_bytes_with_interactive_overlay(
        &self,
        input: &[u8],
        benchmark: Option<&mut BenchmarkReport>,
        native_sgr: &mut NativeSgrState,
        overlay_style: &mut Option<Style>,
    ) -> Vec<u8> {
        let tokens = tokenize_ansi(input);
        let visible = visible_bytes(&tokens);
        let styles = self.match_styles(&visible, benchmark);
        emit_interactive_highlighted(&tokens, &styles, self.color_mode, native_sgr, overlay_style)
    }
}

impl StreamingHighlighter {
    /// Creates a streaming highlighter for noninteractive output.
    pub fn new(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            alternate_screen: false,
            passthrough_single_byte_chunks: false,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            benchmark: None,
        }
    }

    /// Creates a streaming highlighter tuned for interactive PTY output.
    pub fn new_interactive(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            alternate_screen: false,
            passthrough_single_byte_chunks: true,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            benchmark: None,
        }
    }

    /// Creates a noninteractive streaming highlighter with benchmark collection enabled.
    pub fn new_with_benchmark(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            alternate_screen: false,
            passthrough_single_byte_chunks: false,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            benchmark: Some(BenchmarkReport::default()),
        }
    }

    /// Creates an interactive streaming highlighter with benchmark collection enabled.
    pub fn new_interactive_with_benchmark(highlighter: Highlighter) -> Self {
        Self {
            highlighter,
            pending: Vec::new(),
            alternate_screen: false,
            passthrough_single_byte_chunks: true,
            prompt_echo_passthrough: false,
            visible_line_tail: Vec::new(),
            native_sgr: NativeSgrState::default(),
            interactive_overlay: None,
            benchmark: Some(BenchmarkReport::default()),
        }
    }

    /// Returns benchmark data when this stream was created in benchmark mode.
    pub fn benchmark_report(&self) -> Option<&BenchmarkReport> {
        self.benchmark.as_ref()
    }

    /// Pushes a UTF-8 chunk and returns highlighted UTF-8 output ready to display.
    pub fn push_str(&mut self, input: &str) -> String {
        String::from_utf8(self.push(input.as_bytes()))
            .expect("highlighted UTF-8 input remains UTF-8")
    }

    /// Pushes a byte chunk and returns highlighted output ready to display.
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(input);
        neutralize_oversized_incomplete_escape(&mut combined);
        let alternate_screen_chunk =
            self.alternate_screen || contains_alternate_screen_enable(&combined);

        if alternate_screen_chunk {
            self.prompt_echo_passthrough = false;
        }

        if self.passthrough_single_byte_chunks
            && !alternate_screen_chunk
            && !self.prompt_echo_passthrough
            && (contains_prompt_echo_before_lf(&combined)
                || prompt_echo_has_active_source_sgr(&combined, &self.visible_line_tail))
        {
            let Some(prefix_len) = prompt_echo_line_prefix_len(&combined, &self.visible_line_tail)
            else {
                let output = self.emit_prompt_echo_passthrough(&combined);
                self.observe_interactive_visible_bytes(&combined);
                self.prompt_echo_passthrough = true;
                return output;
            };

            let mut remainder = combined.split_off(prefix_len);
            let mut output = self.emit_prompt_echo_passthrough(&combined);
            self.observe_interactive_visible_bytes(&output);

            let split_at = streaming_split_at(&remainder);
            let pending = remainder.split_off(split_at);
            self.pending = pending;

            output.extend(self.highlight_output_bytes(&remainder));
            self.observe_interactive_visible_bytes(&remainder);
            self.reset_interactive_overlay_after_prompt_tail(&mut output);
            return output;
        }

        if self.passthrough_single_byte_chunks && self.prompt_echo_passthrough {
            let Some(prefix_len) = prompt_echo_line_prefix_len(&combined, &self.visible_line_tail)
            else {
                let output = self.emit_prompt_echo_passthrough(&combined);
                self.observe_interactive_visible_bytes(&combined);
                return output;
            };

            let mut remainder = combined.split_off(prefix_len);
            let mut output = self.emit_prompt_echo_passthrough(&combined);
            self.observe_interactive_visible_bytes(&output);

            let split_at = streaming_split_at(&remainder);
            let pending = remainder.split_off(split_at);
            self.pending = pending;

            output.extend(self.highlight_output_bytes(&remainder));
            self.observe_interactive_visible_bytes(&remainder);
            self.reset_interactive_overlay_after_prompt_tail(&mut output);
            return output;
        }

        let split_at = if self.passthrough_single_byte_chunks {
            interactive_split_at(
                &combined,
                self.prompt_echo_passthrough,
                self.alternate_screen,
            )
        } else {
            streaming_split_at(&combined)
        };
        let pending = combined.split_off(split_at);
        self.pending = pending;

        let mut output = self.highlight_output_bytes(&combined);
        self.observe_interactive_visible_bytes(&combined);
        self.reset_interactive_overlay_after_prompt_tail(&mut output);
        output
    }

    /// Flushes any buffered partial terminal sequence or token.
    pub fn finish(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        self.highlight_output_bytes(&pending)
    }

    fn highlight_output_bytes(&mut self, input: &[u8]) -> Vec<u8> {
        if self.passthrough_single_byte_chunks {
            self.highlight_interactive_output_bytes(input)
        } else {
            self.highlight_streaming_bytes(input)
        }
    }

    fn highlight_interactive_output_bytes(&mut self, input: &[u8]) -> Vec<u8> {
        if !self.alternate_screen
            && !contains_alternate_screen_enable(input)
            && contains_cursor_positioning_sequence(input)
        {
            return self.emit_cursor_positioning_passthrough(input);
        }

        let mut output = Vec::new();
        let mut segment_start = 0;
        let mut idx = 0;

        while idx < input.len() {
            if matches!(input[idx], b'\r' | b'\n') {
                self.emit_interactive_line_segment(&input[segment_start..idx], &mut output);
                output.push(input[idx]);
                if input[idx] == b'\r' && input.get(idx + 1) == Some(&b'\n') {
                    output.push(b'\n');
                    idx += 1;
                }
                segment_start = idx + 1;
            }
            idx += 1;
        }

        if segment_start < input.len() {
            self.emit_interactive_line_segment(&input[segment_start..], &mut output);
        }

        output
    }

    fn emit_interactive_line_segment(&mut self, segment: &[u8], output: &mut Vec<u8>) {
        output.extend(self.highlight_streaming_bytes(segment));
    }

    fn highlight_streaming_bytes(&mut self, input: &[u8]) -> Vec<u8> {
        if self.passthrough_single_byte_chunks
            && !self.alternate_screen
            && !contains_alternate_screen_enable(input)
            && contains_cursor_positioning_sequence(input)
        {
            return self.emit_cursor_positioning_passthrough(input);
        }

        let tokens = tokenize_ansi(input);
        let mut output = Vec::new();
        let mut highlightable = Vec::new();

        for token in tokens {
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

        if self.passthrough_single_byte_chunks {
            output.extend(self.highlighter.highlight_bytes_with_interactive_overlay(
                input,
                self.benchmark.as_mut(),
                &mut self.native_sgr,
                &mut self.interactive_overlay,
            ));
        } else {
            output.extend(
                self.highlighter
                    .highlight_bytes_with_benchmark(input, self.benchmark.as_mut()),
            );
        }
        input.clear();
    }

    fn emit_prompt_echo_passthrough(&mut self, input: &[u8]) -> Vec<u8> {
        let output = neutralize_prompt_echo_source_sgr(input, &self.visible_line_tail);
        self.observe_native_sgr(&output);
        output
    }

    fn emit_cursor_positioning_passthrough(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        self.reset_interactive_overlay(&mut output);
        self.observe_native_sgr(input);
        output.extend_from_slice(input);
        output
    }

    fn observe_native_sgr(&mut self, input: &[u8]) {
        for token in tokenize_ansi(input) {
            if let Token::Ansi(bytes) = token {
                self.native_sgr.apply_sequence(&bytes);
            }
        }
    }

    fn observe_interactive_visible_bytes(&mut self, input: &[u8]) {
        if !self.passthrough_single_byte_chunks {
            return;
        }

        if contains_bracketed_paste_disable(input) {
            self.prompt_echo_passthrough = false;
        }

        let redraws_prompt_echo = redraws_prompt_echo_line_without_prompt(input);
        let preserves_prompt_echo = self.prompt_echo_passthrough
            && contains_cursor_positioning_sequence(input)
            && has_no_printable_visible_bytes(input);

        if preserves_prompt_echo {
            return;
        }

        for byte in strip_ansi(input) {
            match byte {
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
                ResetMode::Minimal,
            ));
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

fn neutralize_prompt_echo_source_sgr(input: &[u8], previous_visible_tail: &[u8]) -> Vec<u8> {
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
            output.extend_from_slice(b"\x1b[39m");
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
        output.extend_from_slice(b"\x1b[39m");
        reset_idx += 1;
    }

    output
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
            let line = visible[line_start..line_end]
                .iter()
                .map(|mapped| mapped.byte)
                .collect::<Vec<_>>();
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
            let line = visible[line_start..line_end]
                .iter()
                .map(|mapped| mapped.byte)
                .collect::<Vec<_>>();
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

fn contains_prompt_echo_before_lf(bytes: &[u8]) -> bool {
    let visible = strip_ansi(bytes);
    let line_end = visible
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(visible.len());
    contains_prompt_echo_in_visible_line(&visible[..line_end])
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

fn redraws_prompt_echo_line_without_prompt(input: &[u8]) -> bool {
    contains_cursor_positioning_sequence(input) && promptless_line_tail(input)
}

fn has_no_printable_visible_bytes(input: &[u8]) -> bool {
    strip_ansi(input)
        .iter()
        .all(|byte| matches!(*byte, b'\r' | b'\n' | 0x08))
}

fn promptless_line_tail(input: &[u8]) -> bool {
    let visible = strip_ansi(input);
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
        b'[' => {
            let mut idx = start + 2;
            while idx < input.len() {
                let byte = input[idx];
                idx += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            idx
        }
        b']' => {
            let mut idx = start + 2;
            while idx < input.len() {
                if input[idx] == 0x07 {
                    return idx + 1;
                }
                if input[idx] == 0x1b && idx + 1 < input.len() && input[idx + 1] == b'\\' {
                    return idx + 2;
                }
                idx += 1;
            }
            input.len()
        }
        b'P' | b'X' | b'^' | b'_' => {
            let mut idx = start + 2;
            while idx + 1 < input.len() {
                if input[idx] == 0x1b && input[idx + 1] == b'\\' {
                    return idx + 2;
                }
                idx += 1;
            }
            input.len()
        }
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' => {
            (start + 3).min(input.len())
        }
        _ => (start + 2).min(input.len()),
    }
}

fn is_alternate_screen_enable(bytes: &[u8]) -> bool {
    alternate_screen_command(bytes) == Some(true)
}

fn is_alternate_screen_disable(bytes: &[u8]) -> bool {
    alternate_screen_command(bytes) == Some(false)
}

fn contains_alternate_screen_enable(input: &[u8]) -> bool {
    tokenize_ansi(input).into_iter().any(|token| match token {
        Token::Ansi(bytes) => is_alternate_screen_enable(&bytes),
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

fn contains_cursor_positioning_sequence(input: &[u8]) -> bool {
    tokenize_ansi(input).into_iter().any(|token| match token {
        Token::Ansi(bytes) => is_cursor_positioning_sequence(&bytes),
        Token::Text(_) => false,
    })
}

fn contains_bracketed_paste_disable(input: &[u8]) -> bool {
    tokenize_ansi(input).into_iter().any(|token| match token {
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
                        .map(|style| style_for_reset_mode(style, ResetMode::Minimal))
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
                                output.extend(style_reset_bytes(
                                    &style,
                                    native_sgr,
                                    ResetMode::Minimal,
                                ));
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
            foreground: style.foreground,
            ..Style::default()
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
                    if let Some((code, consumed)) = parse_extended_color(&codes[idx..]) {
                        if is_foreground {
                            self.foreground = Some(code);
                        } else {
                            self.background = Some(code);
                        }
                        idx += consumed;
                        continue;
                    }
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

fn parse_extended_color(codes: &[u16]) -> Option<(String, usize)> {
    match codes {
        [target @ (38 | 48), 5, color, ..] if *color <= 255 => {
            Some((format!("{target};5;{color}"), 3))
        }
        [target @ (38 | 48), 2, red, green, blue, ..]
            if *red <= 255 && *green <= 255 && *blue <= 255 =>
        {
            Some((format!("{target};2;{red};{green};{blue}"), 5))
        }
        _ => None,
    }
}

fn streaming_split_at(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }

    if let Some(escape_start) = incomplete_escape_start(bytes) {
        return escape_start;
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

fn interactive_split_at(
    bytes: &[u8],
    prompt_echo_passthrough: bool,
    alternate_screen: bool,
) -> usize {
    if alternate_screen
        || contains_alternate_screen_enable(bytes)
        || contains_cursor_positioning_sequence(bytes)
        || prompt_echo_passthrough
    {
        incomplete_escape_start(bytes).unwrap_or(bytes.len())
    } else {
        streaming_split_at(bytes)
    }
}

fn incomplete_escape_start(bytes: &[u8]) -> Option<usize> {
    let mut search_start = 0usize;
    while search_start < bytes.len() {
        let start = search_start
            + bytes[search_start..]
                .iter()
                .position(|byte| *byte == 0x1b)?;
        if escape_is_incomplete_at(bytes, start) {
            return Some(start);
        }
        search_start = ansi_sequence_end(bytes, start).max(start + 1);
    }

    None
}

fn escape_is_incomplete_at(bytes: &[u8], start: usize) -> bool {
    if start + 1 >= bytes.len() {
        return true;
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

fn neutralize_oversized_incomplete_escape(bytes: &mut [u8]) {
    let mut search_start = 0usize;
    while search_start < bytes.len() {
        let Some(relative_start) = incomplete_escape_start(&bytes[search_start..]) else {
            break;
        };
        let start = search_start + relative_start;
        if bytes.len().saturating_sub(start) <= MAX_INCOMPLETE_ESCAPE_BYTES {
            break;
        }
        bytes[start] = b'^';
        search_start = start + 1;
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
