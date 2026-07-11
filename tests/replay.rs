use prismtty::style::{Rgb, Style};
use prismtty::{Highlighter, PrismConfig, ProfileStore, StreamingHighlighter, StyledSpan};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayExpectations {
    fixtures: BTreeMap<String, FixtureExpectation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureExpectation {
    synthetic: bool,
    expected_profiles: Vec<String>,
    tokens: Vec<TokenExpectation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenExpectation {
    text: String,
    foreground: Option<String>,
    #[serde(default)]
    bold: Option<bool>,
    /// Assert the token is present in the visible output but covered by no
    /// styled span (guards against over-broad rules / false positives).
    #[serde(default)]
    unstyled: bool,
}

#[derive(Clone, Debug)]
enum ChunkMode {
    Whole,
    Lines,
    Fixed(usize),
    SplitToken(String),
}

#[test]
fn replay_expectations_are_valid_and_synthetic_only() {
    let expectations = load_expectations();
    assert!(
        !expectations.fixtures.is_empty(),
        "replay expectations must not be empty"
    );

    for (fixture, expected) in expectations.fixtures {
        assert!(expected.synthetic, "{fixture} must be marked synthetic");
        assert!(
            !fixture.contains("/Users/") && !fixture.contains("Desktop"),
            "{fixture} must be a repo-relative synthetic fixture"
        );
        let path = fixture_path(&fixture);
        let input = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("failed to read synthetic fixture {}: {err}", path.display())
        });
        assert!(!input.trim().is_empty(), "{fixture} must not be empty");
        assert!(
            !input.contains("/Users/") && !input.contains("Desktop"),
            "{fixture} must not contain private source paths"
        );
        for token in expected.tokens {
            assert!(
                input.contains(&token.text),
                "{fixture} does not contain expected token {:?}",
                token.text
            );
        }
    }
}

#[test]
fn replay_expectation_schema_rejects_unknown_fields() {
    let malformed_expectations = [
        (
            "top-level expectation",
            "unexpected_root",
            r#"
fixtures: {}
unexpected_root: true
"#,
        ),
        (
            "fixture expectation",
            "unexpected_fixture",
            r#"
fixtures:
  sample.txt:
    synthetic: true
    expected_profiles: []
    tokens: []
    unexpected_fixture: true
"#,
        ),
        (
            "token expectation",
            "unexpected_token",
            r#"
fixtures:
  sample.txt:
    synthetic: true
    expected_profiles: []
    tokens:
      - text: sample
        foreground: null
        unexpected_token: true
"#,
        ),
    ];

    for (context, field, input) in malformed_expectations {
        let error = match serde_norway::from_str::<ReplayExpectations>(input) {
            Ok(parsed) => panic!("{context} accepted unknown field {field}: {parsed:?}"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("unknown field") && message.contains(field),
            "{context} returned an unexpected error for {field}: {message}"
        );
    }
}

#[test]
fn replay_fixtures_detect_expected_profiles() {
    let store = ProfileStore::builtin();
    let expectations = load_expectations();

    for (fixture, expected) in expectations.fixtures {
        let input = fs::read_to_string(fixture_path(&fixture)).expect("fixture reads");
        let detected = store.detect_profiles(&input);
        assert_eq!(
            detected, expected.expected_profiles,
            "{fixture} detected unexpected profile set"
        );
    }
}

#[test]
fn replay_fixtures_keep_expected_token_styles_across_chunking_modes() {
    let store = ProfileStore::builtin();
    let expectations = load_expectations();

    for (fixture, expected) in expectations.fixtures {
        let input = fs::read(fixture_path(&fixture)).expect("fixture reads");
        let detected_profiles = store.detect_profiles(&String::from_utf8_lossy(&input));
        assert_eq!(
            detected_profiles, expected.expected_profiles,
            "{fixture} detected unexpected profile set"
        );
        let profile_names: Vec<&str> = detected_profiles.iter().map(String::as_str).collect();
        let config = PrismConfig::from_profiles(&store, &profile_names)
            .unwrap_or_else(|err| panic!("{fixture} profiles failed to load: {err}"));
        let highlighter = Highlighter::from_config(config)
            .unwrap_or_else(|err| panic!("{fixture} highlighter failed to compile: {err}"));

        for mode in chunk_modes(&expected.tokens) {
            let output = replay_bytes(highlighter.clone(), &input, &mode, true);
            let full_style_output = replay_bytes(highlighter.clone(), &input, &mode, false);
            let visible_text = visible_text_from_ansi(&output);
            let spans = styled_spans_from_ansi(&output);
            let full_style_visible_text = visible_text_from_ansi(&full_style_output);
            let full_style_spans = styled_spans_from_ansi(&full_style_output);
            assert_eq!(
                full_style_visible_text, visible_text,
                "{fixture} {mode:?}: interactive and full renderers changed visible text"
            );
            for token in &expected.tokens {
                assert_token(
                    &fixture,
                    &mode,
                    &visible_text,
                    &spans,
                    &full_style_spans,
                    token,
                );
            }
        }
    }
}

fn load_expectations() -> ReplayExpectations {
    let path = fixture_path("expectations.yml");
    let input = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_norway::from_str(&input)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()))
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("replay")
}

fn fixture_path(name: &str) -> PathBuf {
    let relative_path = Path::new(name);
    assert!(
        !relative_path.is_absolute(),
        "fixture path must be relative: {name}"
    );
    assert!(
        relative_path
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "fixture path must contain only normal file-name components: {name}"
    );

    let root = fixture_root();
    let path = root.join(relative_path);
    let canonical_root = root
        .canonicalize()
        .unwrap_or_else(|err| panic!("failed to canonicalize fixture root: {err}"));
    let canonical_path = path
        .canonicalize()
        .unwrap_or_else(|err| panic!("failed to canonicalize fixture path {name}: {err}"));
    assert!(
        canonical_path.starts_with(&canonical_root),
        "fixture path escaped fixture root: {name}"
    );
    canonical_path
}

fn chunk_modes(tokens: &[TokenExpectation]) -> Vec<ChunkMode> {
    let mut modes = vec![
        ChunkMode::Whole,
        ChunkMode::Lines,
        ChunkMode::Fixed(1),
        ChunkMode::Fixed(2),
        ChunkMode::Fixed(3),
        ChunkMode::Fixed(5),
        ChunkMode::Fixed(8),
        ChunkMode::Fixed(13),
        ChunkMode::Fixed(64),
    ];
    for token in tokens {
        if token.text.len() > 3 {
            modes.push(ChunkMode::SplitToken(token.text.clone()));
        }
    }
    modes
}

fn replay_bytes(
    highlighter: Highlighter,
    input: &[u8],
    mode: &ChunkMode,
    interactive: bool,
) -> Vec<u8> {
    let mut streaming = if interactive {
        StreamingHighlighter::new_interactive(highlighter)
    } else {
        StreamingHighlighter::new(highlighter)
    };
    let mut output = Vec::new();

    match mode {
        ChunkMode::Whole => output.extend(streaming.push(input)),
        ChunkMode::Lines => {
            for line in input.split_inclusive(|byte| *byte == b'\n') {
                output.extend(streaming.push(line));
            }
        }
        ChunkMode::Fixed(size) => {
            for chunk in input.chunks(*size) {
                output.extend(streaming.push(chunk));
            }
        }
        ChunkMode::SplitToken(token) => {
            output.extend(replay_split_token(&mut streaming, input, token.as_bytes()));
        }
    }

    output.extend(streaming.finish());
    output
}

fn replay_split_token(streaming: &mut StreamingHighlighter, input: &[u8], token: &[u8]) -> Vec<u8> {
    let Some(start) = find_subslice(input, token) else {
        return streaming.push(input);
    };
    let split = start + token.len() / 2;
    let mut output = Vec::new();
    output.extend(streaming.push(&input[..split]));
    output.extend(streaming.push(&input[split..]));
    output
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn assert_token(
    fixture: &str,
    mode: &ChunkMode,
    visible_text: &str,
    spans: &[StyledSpan],
    full_style_spans: &[StyledSpan],
    expected: &TokenExpectation,
) {
    assert_bold_expectation(fixture, mode, visible_text, full_style_spans, expected);

    if expected.unstyled {
        let occurrences = count_occurrences(visible_text, &expected.text);
        assert_eq!(
            occurrences, 1,
            "{fixture} {mode:?}: token {:?} must appear exactly once in visible output",
            expected.text
        );
        let token_start = visible_text
            .find(&expected.text)
            .expect("token exists after occurrence count");
        let token_end = token_start + expected.text.len();
        let covering: Vec<&StyledSpan> = spans
            .iter()
            .filter(|span| span.start < token_end && span.end > token_start)
            .collect();
        assert!(
            covering.is_empty(),
            "{fixture} {mode:?}: token {:?} must be unstyled but is covered by {covering:?}",
            expected.text
        );
        return;
    }

    if expected.foreground.is_none() {
        assert!(
            visible_text.contains(&expected.text),
            "{fixture} {mode:?}: visible output does not contain token {:?}",
            expected.text
        );
        return;
    }

    let occurrences = count_occurrences(visible_text, &expected.text);
    assert_eq!(
        occurrences, 1,
        "{fixture} {mode:?}: styled token {:?} must appear exactly once in visible output",
        expected.text
    );
    let token_start = visible_text
        .find(&expected.text)
        .expect("styled token exists after occurrence count");
    let token_end = token_start + expected.text.len();
    let matching: Vec<&StyledSpan> = spans
        .iter()
        .filter(|span| span.start <= token_start && span.end >= token_end)
        .collect();
    assert!(
        !matching.is_empty(),
        "{fixture} {mode:?}: no styled span covers token {:?} at {token_start}..{token_end}; spans={spans:?}",
        expected.text
    );
    let span = matching[0];
    if let Some(foreground) = &expected.foreground {
        let rgb = parse_hex_rgb(foreground);
        assert_eq!(
            span.style.foreground,
            Some(rgb),
            "{fixture} {mode:?}: token {:?} has wrong foreground in span {:?}",
            expected.text,
            span
        );
    }
}

fn assert_bold_expectation(
    fixture: &str,
    mode: &ChunkMode,
    visible_text: &str,
    spans: &[StyledSpan],
    expected: &TokenExpectation,
) {
    let Some(expected_bold) = expected.bold else {
        return;
    };

    let occurrences = count_occurrences(visible_text, &expected.text);
    assert_eq!(
        occurrences, 1,
        "{fixture} {mode:?}: bold token {:?} must appear exactly once in visible output",
        expected.text
    );
    let token_start = visible_text
        .find(&expected.text)
        .expect("bold token exists after occurrence count");
    let token_end = token_start + expected.text.len();
    let mut bold_coverage = vec![false; token_end - token_start];
    for span in spans.iter().filter(|span| span.style.bold) {
        let overlap_start = span.start.max(token_start);
        let overlap_end = span.end.min(token_end);
        if overlap_start >= overlap_end {
            continue;
        }
        for covered in &mut bold_coverage
            [overlap_start.saturating_sub(token_start)..overlap_end.saturating_sub(token_start)]
        {
            *covered = true;
        }
    }
    let bold_matches = if expected_bold {
        bold_coverage.iter().all(|covered| *covered)
    } else {
        bold_coverage.iter().all(|covered| !*covered)
    };
    assert!(
        bold_matches,
        "{fixture} {mode:?}: token {:?} has wrong bold state; spans={spans:?}",
        expected.text
    );
}

#[test]
fn replay_oracle_rejects_a_bold_mismatch() {
    let visible_text = "Established";
    let spans = vec![StyledSpan {
        text: visible_text.to_string(),
        start: 0,
        end: visible_text.len(),
        style: Style {
            foreground: Some(Rgb { r: 1, g: 2, b: 3 }),
            ..Style::default()
        },
    }];
    let expected = TokenExpectation {
        text: visible_text.to_string(),
        foreground: Some("#010203".to_string()),
        bold: Some(true),
        unstyled: false,
    };

    let mismatch = std::panic::catch_unwind(|| {
        assert_token(
            "bold-mismatch.txt",
            &ChunkMode::Whole,
            visible_text,
            &spans,
            &spans,
            &expected,
        );
    });

    assert!(mismatch.is_err(), "replay oracle accepted a bold mismatch");
}

#[test]
fn replay_oracle_rejects_partial_bold_coverage() {
    let visible_text = "Established";
    let spans = vec![StyledSpan {
        text: "E".to_string(),
        start: 0,
        end: 1,
        style: Style {
            bold: true,
            ..Style::default()
        },
    }];
    let expected = TokenExpectation {
        text: visible_text.to_string(),
        foreground: None,
        bold: Some(true),
        unstyled: false,
    };

    let mismatch = std::panic::catch_unwind(|| {
        assert_bold_expectation(
            "partial-bold.txt",
            &ChunkMode::Whole,
            visible_text,
            &spans,
            &expected,
        );
    });

    assert!(
        mismatch.is_err(),
        "replay oracle accepted one-byte bold coverage for a whole token"
    );
}

#[test]
fn replay_oracle_handles_string_controls_and_raw_c1_sequences() {
    let input = b"a\x1b]0;hidden\x07b\x1bPignored\x1b\\c\x9d0;secret\x9cd\x9b1me\x9b0mf\x1b(Bg";

    assert_eq!(visible_text_from_ansi(input), "abcdefg");
    let spans = styled_spans_from_ansi(input);
    assert_eq!(spans.len(), 1, "unexpected styled spans: {spans:?}");
    assert_eq!(spans[0].text, "e");
    assert!(spans[0].style.bold);
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}

fn parse_hex_rgb(input: &str) -> Rgb {
    let hex = input.strip_prefix('#').expect("foreground starts with #");
    assert_eq!(hex.len(), 6, "foreground must be #rrggbb");
    Rgb {
        r: u8::from_str_radix(&hex[0..2], 16).expect("valid red channel"),
        g: u8::from_str_radix(&hex[2..4], 16).expect("valid green channel"),
        b: u8::from_str_radix(&hex[4..6], 16).expect("valid blue channel"),
    }
}

fn visible_text_from_ansi(input: &[u8]) -> String {
    let mut visible = Vec::new();
    let mut idx = 0;
    while idx < input.len() {
        if let Some(width) = valid_utf8_width_at(input, idx) {
            visible.extend_from_slice(&input[idx..idx + width]);
            idx += width;
        } else if is_terminal_control(input[idx]) {
            idx = ansi_end(input, idx);
        } else {
            visible.push(input[idx]);
            idx += 1;
        }
    }
    String::from_utf8_lossy(&visible).into_owned()
}

fn styled_spans_from_ansi(input: &[u8]) -> Vec<StyledSpan> {
    let mut spans = Vec::new();
    let mut current = Style::default();
    let mut text = Vec::new();
    let mut text_start = 0;
    let mut visible_idx = 0;
    let mut idx = 0;

    while idx < input.len() {
        if let Some(width) = valid_utf8_width_at(input, idx) {
            if text.is_empty() {
                text_start = visible_idx;
            }
            text.extend_from_slice(&input[idx..idx + width]);
            visible_idx += width;
            idx += width;
        } else if is_terminal_control(input[idx]) {
            push_visible_span(&mut spans, &mut text, text_start, visible_idx, &current);
            let end = ansi_end(input, idx);
            apply_sgr(&input[idx..end], &mut current);
            idx = end;
        } else {
            if text.is_empty() {
                text_start = visible_idx;
            }
            text.push(input[idx]);
            visible_idx += 1;
            idx += 1;
        }
    }

    push_visible_span(&mut spans, &mut text, text_start, visible_idx, &current);
    spans
}

fn push_visible_span(
    spans: &mut Vec<StyledSpan>,
    text: &mut Vec<u8>,
    start: usize,
    end: usize,
    style: &Style,
) {
    if text.is_empty() {
        return;
    }
    if !style.is_empty() {
        spans.push(StyledSpan {
            text: String::from_utf8_lossy(text).into_owned(),
            start,
            end,
            style: style.clone(),
        });
    }
    text.clear();
}

fn apply_sgr(sequence: &[u8], style: &mut Style) {
    let params_start = if sequence.starts_with(b"\x1b[") {
        2
    } else if sequence.starts_with(b"\x9b") {
        1
    } else {
        return;
    };
    if sequence.last() != Some(&b'm') {
        return;
    }
    let params = String::from_utf8_lossy(&sequence[params_start..sequence.len() - 1]);
    let numbers: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .filter_map(|part| {
                if part.is_empty() {
                    Some(0)
                } else {
                    part.parse::<u16>().ok()
                }
            })
            .collect()
    };
    let mut idx = 0;
    while idx < numbers.len() {
        match numbers[idx] {
            0 => *style = Style::default(),
            1 => style.bold = true,
            3 => style.italic = true,
            4 => style.underline = true,
            5 => style.blink = true,
            7 => style.invert = true,
            9 => style.strike = true,
            22 => style.bold = false,
            23 => style.italic = false,
            24 => style.underline = false,
            25 => style.blink = false,
            27 => style.invert = false,
            29 => style.strike = false,
            39 => style.foreground = None,
            49 => style.background = None,
            38 if idx + 4 < numbers.len() && numbers[idx + 1] == 2 => {
                style.foreground = Some(Rgb {
                    r: numbers[idx + 2] as u8,
                    g: numbers[idx + 3] as u8,
                    b: numbers[idx + 4] as u8,
                });
                idx += 4;
            }
            48 if idx + 4 < numbers.len() && numbers[idx + 1] == 2 => {
                style.background = Some(Rgb {
                    r: numbers[idx + 2] as u8,
                    g: numbers[idx + 3] as u8,
                    b: numbers[idx + 4] as u8,
                });
                idx += 4;
            }
            _ => {}
        }
        idx += 1;
    }
}

fn ansi_end(input: &[u8], start: usize) -> usize {
    if input[start] != 0x1b {
        return match input[start] {
            0x9b => csi_end(input, start + 1),
            0x9d => string_control_end(input, start + 1, true),
            0x90 | 0x98 | 0x9e | 0x9f => string_control_end(input, start + 1, false),
            _ => (start + 1).min(input.len()),
        };
    }
    if start + 1 >= input.len() {
        return input.len();
    }
    match input[start + 1] {
        b'[' => csi_end(input, start + 2),
        b']' => string_control_end(input, start + 2, true),
        b'P' | b'X' | b'^' | b'_' => string_control_end(input, start + 2, false),
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%' => {
            (start + 3).min(input.len())
        }
        _ => (start + 2).min(input.len()),
    }
}

fn csi_end(input: &[u8], mut idx: usize) -> usize {
    while idx < input.len() {
        let byte = input[idx];
        idx += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    idx
}

fn string_control_end(input: &[u8], mut idx: usize, allows_bel: bool) -> usize {
    while idx < input.len() {
        if let Some(width) = valid_utf8_width_at(input, idx) {
            idx += width;
            continue;
        }
        if (allows_bel && input[idx] == 0x07) || input[idx] == 0x9c {
            return idx + 1;
        }
        if input[idx] == 0x1b && input.get(idx + 1) == Some(&b'\\') {
            return idx + 2;
        }
        idx += 1;
    }
    input.len()
}

fn valid_utf8_width_at(input: &[u8], idx: usize) -> Option<usize> {
    let width = match input[idx] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let end = idx.checked_add(width)?;
    (end <= input.len() && std::str::from_utf8(&input[idx..end]).is_ok()).then_some(width)
}

fn is_terminal_control(byte: u8) -> bool {
    byte == 0x1b || (0x80..=0x9f).contains(&byte)
}
