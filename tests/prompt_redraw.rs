//! Output lines that arrive in one read together with a prompt redrawn by a
//! cursor move (zsh's right prompt, a device CLI redrawing its prompt with
//! `CSI G`) are colored, as they are when they arrive alone. Every read in
//! which a control, or a terminal mode set earlier, could take the cursor
//! back to those lines keeps the cursor-move bypass and its exact bytes.
//!
//! Judged by what a terminal shows: tests/vt_cells is a small cell model.

mod vt_cells;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use prismtty::{Highlighter, PrismConfig, StreamingHighlighter};
use vt_cells::{Color, Screen};

const RED: Color = Color::Rgb(255, 0, 0);
const RULES: &str = "rules:\n  - description: error\n    regex: 'error'\n    color: f#ff0000\n";
/// A prompt redrawn on its own row after the output lines.
const PROMPT: &[u8] = b"\x1b[1Gp$ ";

fn streaming() -> StreamingHighlighter {
    let config = PrismConfig::from_chromaterm_yaml(RULES).expect("rules parse");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    StreamingHighlighter::new_interactive(highlighter)
}

/// The terminal the library tests write to, unless a test names its own.
const ROWS: usize = 24;
const COLUMNS: usize = 80;

/// Pushes each read as one chunk, the way the read loop does, with the size
/// of a terminal `rows` by `cols` that the read loop gives, then finishes.
fn render_on(rows: usize, cols: usize, reads: &[&[u8]]) -> Vec<u8> {
    let rows = u16::try_from(rows).expect("rows");
    let cols = u16::try_from(cols).expect("cols");
    let mut streaming = streaming();
    let mut output = Vec::new();
    for read in reads {
        streaming.set_terminal_size(rows, cols);
        output.extend(streaming.push(read));
    }
    output.extend(streaming.finish());
    output
}

fn render(reads: &[&[u8]]) -> Vec<u8> {
    render_on(ROWS, COLUMNS, reads)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn joined(reads: &[&[u8]]) -> Vec<u8> {
    reads.concat()
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// The read stays on the cursor-move bypass: released 1.4.1 writes these
/// reads byte for byte as they came, and so must this one.
fn assert_bypassed(name: &str, reads: &[&[u8]]) {
    assert_bypassed_on(name, ROWS, COLUMNS, reads);
}

fn assert_bypassed_on(name: &str, rows: usize, cols: usize, reads: &[&[u8]]) {
    let output = render_on(rows, cols, reads);
    assert_eq!(
        output,
        joined(reads),
        "{name}: expected the bytes unchanged, got {:?}",
        String::from_utf8_lossy(&output)
    );
}

/// The columns of `row` drawn in red that hold a character. A highlight may
/// stay open over the blank after a match, as it does in 1.4.1; a blank cell
/// shows no foreground.
fn red_text(screen: &Screen, row: usize) -> Vec<usize> {
    screen
        .columns_in(row, RED)
        .into_iter()
        .filter(|col| screen.cell(row, *col).ch != ' ')
        .collect()
}

/// True when two cells look the same: a blank whose foreground alone
/// differs shows nothing of it.
fn looks_the_same(a: vt_cells::Cell, b: vt_cells::Cell) -> bool {
    a == b
        || (a.ch == ' '
            && b.ch == ' '
            && a.attrs.bg == b.attrs.bg
            && !a.attrs.reverse
            && !b.attrs.reverse
            && !a.attrs.underline
            && !b.attrs.underline)
}

/// The columns of `row` that hold one of the occurrences of `word`.
fn word_columns(screen: &Screen, row: usize, word: &str) -> Vec<usize> {
    let cells: Vec<char> = (0..screen.cols())
        .map(|col| screen.cell(row, col).ch)
        .collect();
    let word: Vec<char> = word.chars().collect();
    let mut columns = Vec::new();
    for start in 0..cells.len() {
        if cells[start..].starts_with(&word) {
            columns.extend(start..start + word.len());
        }
    }
    columns.sort_unstable();
    columns.dedup();
    columns
}

/// Final text equals what the unchanged bytes show, and every cell drawn
/// differently is a red cell of an `error` in one of `rows`.
fn assert_only_errors_colored(
    name: &str,
    rows: usize,
    cols: usize,
    reads: &[&[u8]],
    colored_rows: &[usize],
) {
    let output = render_on(rows, cols, reads);
    let shown = Screen::rendered(rows, cols, &output);
    let raw = Screen::rendered(rows, cols, &joined(reads));
    assert!(
        shown.same_text(&raw),
        "{name}: text changed: {:?}",
        String::from_utf8_lossy(&output)
    );
    let differences: Vec<(usize, usize)> = shown
        .attribute_differences(&raw)
        .into_iter()
        .filter(|(row, col)| !looks_the_same(shown.cell(*row, *col), raw.cell(*row, *col)))
        .collect();
    assert!(
        !differences.is_empty(),
        "{name}: nothing was colored: {:?}",
        String::from_utf8_lossy(&output)
    );
    for (row, col) in differences {
        assert!(
            colored_rows.contains(&row) && word_columns(&shown, row, "error").contains(&col),
            "{name}: cell {row},{col} ({:?}) changed outside an error: {:?}",
            shown.cell(row, col),
            String::from_utf8_lossy(&output)
        );
        assert_eq!(
            shown.cell(row, col).attrs.fg,
            RED,
            "{name}: cell {row},{col}"
        );
    }
    for row in colored_rows {
        assert_eq!(
            red_text(&shown, *row),
            word_columns(&shown, *row, "error"),
            "{name}: row {row} {:?}",
            shown.text(*row)
        );
    }
}

#[test]
fn lines_that_share_a_read_with_a_prompt_redraw_are_colored() {
    let read = b"link is down error 192.0.2.1\r\nsecond error\r\n\x1b[1G\x1b[0Kuser@host ~ $ ";
    let output = render_on(6, 40, &[read]);
    assert!(
        output.ends_with(b"\x1b[1G\x1b[0Kuser@host ~ $ "),
        "the prompt redraw is written unchanged: {:?}",
        String::from_utf8_lossy(&output)
    );
    let screen = Screen::rendered(6, 40, &output);
    assert_eq!(screen.text(0), "link is down error 192.0.2.1");
    assert_eq!(red_text(&screen, 0), (13..18).collect::<Vec<_>>());
    assert_eq!(red_text(&screen, 1), (7..12).collect::<Vec<_>>());
    assert_eq!(screen.text(2), "user@host ~ $");
    assert!(red_text(&screen, 2).is_empty());
    assert_only_errors_colored(
        "STR-3 trigger, LF only",
        6,
        40,
        &[b"link is down error\nsecond error\n\x1b[1G\x1b[0Kuser@host ~ $ "],
        &[0, 1],
    );
}

#[test]
fn device_prompts_redrawn_with_a_column_move_keep_the_output_colored() {
    let cases: [(&str, &[u8]); 5] = [
        (
            "router",
            b"interface is down error\r\nline error\r\n\x1b[1G\x1b[0Krouter# ",
        ),
        ("cisco", b"Gi0/1 up error\r\n\x1b[1G\x1b[0KR1# "),
        (
            "junos",
            b"interface down error\r\n\x1b[1G\x1b[0Kuser@router> ",
        ),
        ("arista", b"link error\r\n\x1b[1G\x1b[0Kswitch# "),
        ("fortios", b"error\r\n\x1b[1G\x1b[0KFGT # "),
    ];
    for (name, read) in cases {
        let lines = read.iter().filter(|byte| **byte == b'\n').count();
        let rows: Vec<usize> = (0..lines).collect();
        assert_only_errors_colored(name, 6, 40, &[read], &rows);
    }
}

#[test]
fn zsh_output_coalesced_with_its_right_prompt_is_colored() {
    // What reaches the bypass of a zsh read behind a coalescing link once
    // the prompt-echo handling has taken the echoed command: output lines,
    // the PROMPT_SP line, then the prompt and its right prompt.
    let prompt_sp = concat(&[
        b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m",
        &[b' '; 79],
        b"\r \r",
    ]);
    let right_prompt = concat(&[
        b"\r\n",
        b"link is down error 192.0.2.1\r\nsecond error\r\n",
        &prompt_sp,
        b"\r\x1b[0m\x1b[27m\x1b[24m\x1b[Jhost% \x1b[K\x1b[53C[0]\x1b[56D\x1b[?2004h",
    ]);
    assert_only_errors_colored("right prompt", 24, 80, &[&right_prompt], &[1, 2]);

    // A two-line prompt: its first line shares the PROMPT_SP line, which is
    // rewritten in place, so it and everything after it stay as they came.
    let two_line = concat(&[
        b"\r\n",
        b"link is down error 192.0.2.1\r\n",
        &prompt_sp,
        b"\r\x1b[0m\x1b[27m\x1b[24m\x1b[Jerror zone up\r\n% \x1b[K\x1b[74C[0]\x1b[77D\x1b[?2004h",
    ]);
    assert_only_errors_colored("two-line prompt", 24, 80, &[&two_line], &[1]);
}

#[test]
fn a_mode_reset_lets_the_next_prompt_redraw_color_its_lines_again() {
    let read: &[u8] = &concat(&[b"error\r\n", PROMPT]);
    let cases: [(&str, &[&[u8]]); 8] = [
        ("reverse wrap 1045", &[b"\x1b[?1045h", b"\x1b[?1045l", read]),
        ("reverse wrap 45", &[b"\x1b[?45h", b"\x1b[?45l", read]),
        (
            "both, both off",
            &[b"\x1b[?45;1045h", b"\x1b[?45;1045l", read],
        ),
        ("margins", &[b"\x1b[?69h", b"\x1b[?69l", read]),
        ("autowrap", &[b"\x1b[?7l", b"\x1b[?7h", read]),
        ("leading zeros", &[b"\x1b[?01045h", b"\x1b[?001045l", read]),
        ("scroll region", &[b"\x1b[1;3r", b"\x1b[r", read]),
        ("RIS", &[b"\x1b[?1045h\x1b[?7l\x1b[1;3r", b"\x1bc", read]),
    ];
    for (name, reads) in cases {
        let output = render_on(6, 24, reads);
        let screen = Screen::rendered(6, 24, &output);
        let row = (0..6)
            .find(|row| screen.text(*row).starts_with("error"))
            .expect("error row");
        assert_eq!(
            red_text(&screen, row),
            (0..5).collect::<Vec<_>>(),
            "{name}: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

/// The cases with characters past ASCII are in
/// `pty_characters_past_ascii_are_judged_as_a_utf8_terminal_draws_them`: the
/// library is not told that the terminal reads UTF-8.
#[test]
fn narrow_rows_keep_text_and_color_only_the_matches() {
    let cases: [(&str, &[u8], &[usize]); 2] = [
        (
            "erase forms in the tail",
            b"error up\r\n\x1b[K\x1b[1K\x1b[2K\x1b[0J\x1b[1Gp$ ",
            &[0],
        ),
        (
            "metadata in the tail",
            b"error\r\n\x1b]2;ab\x07\x1b[?2004h\x1b[?1h\x1b=\x1b[1Gp$ ",
            &[0],
        ),
    ];
    for (name, read, rows) in cases {
        assert_only_errors_colored(name, 6, 10, &[read], rows);
    }
}

#[test]
fn reads_that_can_return_to_their_lines_stay_on_the_bypass_review_45() {
    let cases: [(&str, &[&[u8]]); 20] = [
        ("cursor up after the lines", &[b"error error\n\x1b[1A\x1b[1GOK\n"]),
        ("column move, then up", &[b"error\r\n\x1b[1G\x1b[1AOK"]),
        ("save before, restore after", &[b"\x1b7error\n\x1b8up\n\x1b[1Gplain"]),
        ("CSI s and CSI u", &[b"\x1b[serror\n\x1b[up\n\x1b[1Gplain"]),
        ("absolute row", &[b"error\r\n\x1b[1d\x1b[1GOK"]),
        ("scroll region homes the cursor", &[b"error\r\n\x1b[1;4rOK\x1b[1Gdone"]),
        ("reverse index", &[b"error\r\n\x1bM\x1b[1GOK"]),
        ("CR rewrite before the LF", &[b"error\rzz\n\x1b[1Gx"]),
        ("BS rewrite before the LF", &[b"error\x08Z\n\x1b[1Gx"]),
        ("partial erase after up", &[b"error\r\n\x1b[1A\x1b[3G\x1b[0K"]),
        ("two rows repainted", &[b"error AAAA\r\nerror BBBB\r\n\x1b[2A\x1b[1Gxx\x1b[1B\x1b[1Gxx"]),
        ("absolute position", &[b"error AAAA\r\nerror BBBB\r\n\x1b[1;1HOK"]),
        ("OSC, then up", &[b"\x1b]0;title\ninner\x07error\r\n\x1b[1AOK"]),
        ("logo, then up", &[b"\x1b[1m\x1b[31m   --+oo up oo+--\r\n   ++ 192.0.2.9 ++\r\n\x1b[m\x1b[2A\x1b[30Cuser@host up\r\n"]),
        ("CSI A cut by the read end", &[b"error error\r\n\x1b[1Gp$ \x1b[1", b"A\x1b[1GOK"]),
        ("ESC 8 cut by the read end", &[b"\x1b7\r\nerror error\r\n\x1b[1Gp$ \x1b", b"8OK"]),
        ("OSC payload LF", &[b"\x1b]0;error\n\x07\x1b[1Gplain"]),
        ("DCS payload LF", &[b"\x1bPerror\n\x1b\\\x1b[1Gplain"]),
        ("progress repaint", &[b"build error 192.0.2.1\r\n\x1b[1A\x1b[2Kprogress 50%\r\n"]),
        ("cursor move on the first line", &[b"\x1b[5Gabc error\r\nnext error\r\n"]),
    ];
    for (name, reads) in cases {
        assert_bypassed(name, reads);
    }
}

/// A C1 control among the text of a read: reverse index (`8D`) moves the
/// cursor up, as `ESC M` does, on a terminal that runs it. The raw byte and
/// its UTF-8 form `C2 8D` are both controls to the tokenizer, so the read
/// keeps the bypass.
#[test]
fn reads_with_a_c1_control_in_their_text_stay_on_the_bypass() {
    let cases: [(&str, &[&[u8]]); 3] = [
        (
            "reverse index, UTF-8 form",
            &[b"error\r\n\xc2\x8d\x1b[1Gp$ "],
        ),
        ("reverse index, raw byte", &[b"error\r\n\x8d\x1b[1Gp$ "]),
        ("next line, raw byte", &[b"error error\r\n\x1b[1Gp$ \x85x"]),
    ];
    for (name, reads) in cases {
        assert_bypassed(name, reads);
    }
}

/// A read whose prompt stays visible in the default modes, so it is promoted
/// unless a mode is on: `CSI 11D` stops at the first column, left of the
/// prompt, and `OK` lands there. With reverse wraparound on, the same move
/// steps back onto the line above.
const BACK_UNLESS_REVERSE_WRAP: &[u8] = b"error\r\n\x1b[4Gp$ \x1b[11DOK";

#[test]
fn reads_after_a_mode_that_lets_the_cursor_return_stay_on_the_bypass_review_56() {
    let read: &[u8] = &concat(&[b"error\r\n", PROMPT]);
    let back = BACK_UNLESS_REVERSE_WRAP;
    let cases: &[(&str, &[&[u8]])] = &[
        (
            "partial scroll region, cursor below it",
            &[b"\x1b[1;3r\x1b[8;1H", b"error\r\n\x1b[1GOK"],
        ),
        (
            "partial scroll region, prompt",
            &[b"\x1b[1;3r\x1b[8;1H", read],
        ),
        (
            "region set in the C1 form",
            &[b"\xc2\x9b1;3r\x1b[8;1H", read],
        ),
        (
            "region, then DECSTR",
            &[b"\x1b[1;3r\x1b[8;1H", b"\x1b[!p", read],
        ),
        (
            "reverse wrap 1045, CSI D",
            &[b"\x1b[?1045h", b"error\r\n\x1b[1G\x1b[5DOK"],
        ),
        (
            "reverse wrap 1045, BS",
            &[b"\x1b[?1045h", b"error\r\n\x1b[1G\x08\x08\x08\x08\x08OK"],
        ),
        ("reverse wrap 1045, prompt", &[b"\x1b[?1045h", back]),
        (
            "reverse wrap 1045, prompt, review bytes",
            &[b"\x1b[?1045h", b"error\r\n\x1b[1Gp$ \x1b[5DOK"],
        ),
        ("reverse wrap 45", &[b"\x1b[?45h", back]),
        ("margins", &[b"\x1b[?69h\x1b[1;10s", read]),
        ("autowrap off", &[b"\x1b[?7l", b"abcdeferror\r\n\x1b[1Gp$ "]),
        (
            "setter inside an echoed command",
            &[
                b"R1#",
                b"show x\x1b[?1045h\r\nerror\r\n\x1b[4Gp$ \x1b[11DOK",
            ],
        ),
        (
            "non-prompt tail, then up",
            &[b"error error\r\n\x1b[1Gx", b"\x1b[1A\x1b[1GOK"],
        ),
        (
            "BS in the tail",
            &[&concat(&[b"error\r\n", PROMPT, b"\x08"])],
        ),
        ("tab in an output line", &[b"ab\terror\r\n\x1b[1Gp$ "]),
    ];
    for &(name, reads) in cases {
        if name == "setter inside an echoed command" {
            // The echoed command is the prompt-echo handling's, as in 1.4.1;
            // only the part after it is checked against the bypass.
            let output = render(reads);
            assert!(
                output.ends_with(b"\r\nerror\r\n\x1b[4Gp$ \x1b[11DOK"),
                "{name}: {:?}",
                String::from_utf8_lossy(&output)
            );
            continue;
        }
        assert_bypassed(name, reads);
    }
}

#[test]
fn reads_after_a_mode_the_tracker_must_read_as_the_terminal_does_stay_on_the_bypass_review_65() {
    // The review's reads end `p$ CSI 8D OK`, which in the default modes
    // writes over the prompt and is never promoted; each is kept, and each
    // mode is checked again with a read that is promoted when the mode is off.
    let review: &[u8] = b"error\r\n\x1b[1Gp$ \x1b[8DOK";
    let back = BACK_UNLESS_REVERSE_WRAP;
    let read: &[u8] = &concat(&[b"error\r\n", PROMPT]);
    let thirty = concat(&[b"\x1b[?", &b"1;".repeat(30), b"1045l"]);
    let cases: &[(&str, &[&[u8]])] = &[
        (
            "45 off leaves 1045 on, review bytes",
            &[b"\x1b[?45;1045h", b"\x1b[?45l", review],
        ),
        (
            "leading zeros set 1045, review bytes",
            &[b"\x1b[?01045h", review],
        ),
        (
            "a reset past 30 parameters, review bytes",
            &[b"\x1b[?1045h", &thirty, review],
        ),
        (
            "45 off leaves 1045 on",
            &[b"\x1b[?45;1045h", b"\x1b[?45l", back],
        ),
        (
            "1045 off leaves 45 on",
            &[b"\x1b[?45;1045h", b"\x1b[?1045l", back],
        ),
        ("leading zeros set 1045", &[b"\x1b[?01045h", back]),
        (
            "leading zeros clear autowrap",
            &[b"\x1b[?07l", b"abcdeferror\r\n\x1b[1Gp$ "],
        ),
        ("leading zeros set margins", &[b"\x1b[?069h", read]),
        ("leading zeros set 45", &[b"\x1b[?0045h", back]),
        (
            "a reset past 30 parameters",
            &[b"\x1b[?1045h", &thirty, back],
        ),
        (
            "overflowing region",
            &[b"\x1b[1;3r\x1b[8;1H", b"\x1b[4294967296r", read],
        ),
        (
            "C1 region reset",
            &[b"\x1b[1;3r", b"\xc2\x9b1r\x1b[8;1H", read],
        ),
        ("C1 mode reset", &[b"\x1b[?1045h", b"\xc2\x9b?1045l", back]),
        (
            "XTRESTORE of the alternate screen",
            &[b"\x1b[?47h", b"\x1b[?47s", b"\x1b[?47l", b"\x1b[?47r", read],
        ),
        ("XTRESTORE of reverse wrap", &[b"\x1b[?1045r", back]),
        (
            "progress tail, then up",
            &[
                b"error fetching package\r\n\x1b[1GDownloading 50% ",
                b"\x1b[1A\x1b[1GOK",
            ],
        ),
        (
            "overwritten prompt",
            &[b"error\r\n\x1b[1GR1#  \r\x1b[1Gworking"],
        ),
    ];
    for &(name, reads) in cases {
        if name == "XTRESTORE of the alternate screen" {
            // 1.4.1 follows the h and l of mode 47 as it always has; the read
            // after the restore stays on the bypass.
            let output = render(reads);
            assert!(
                output.ends_with(read),
                "{name}: {:?}",
                String::from_utf8_lossy(&output)
            );
            assert!(
                !contains(&output, b"\x1b[38"),
                "{name}: {:?}",
                String::from_utf8_lossy(&output)
            );
            continue;
        }
        assert_bypassed(name, reads);
    }
}

#[test]
fn prompt_shaped_tails_are_read_strictly() {
    let cases: [(&str, &[u8]); 6] = [
        (
            "colon is no prompt end",
            b"router: show error\r\n\x1b[1G\x1b[0Krouter: ",
        ),
        (
            "bracket is no prompt end",
            b"[router] show error\r\n\x1b[1G\x1b[0K[router] ",
        ),
        (
            "echoed command line",
            b"R1# show error\r\n\x1b[1G\x1b[0KR1# ",
        ),
        (
            "echoed command line in color",
            b"\x1b[32mR1#\x1b[0m show error\r\nlink error\r\n\x1b[1G\x1b[0KR1# ",
        ),
        ("hash in ordinary text", b"hello # error\r\n\x1b[1Gx"),
        ("percentage is no prompt", b"error\r\n\x1b[1G[====> ] 40% "),
    ];
    for (name, read) in cases {
        assert_bypassed(name, &[read]);
    }
}

/// A move stops at the edge of the screen, so where `CSI C`, `CSI D` and
/// `CSI G` leave the cursor, and where text wraps, depends on the terminal's
/// width. A read in which later text could land on the prompt, or leave the
/// cursor on a row without it, keeps the bypass.
#[test]
fn moves_that_stop_at_the_edge_keep_the_bypass_review_70() {
    let zsh_right_prompt: &[u8] = b"error\r\n\x1b[1G\x1b[0KHOST% \x1b[K\x1b[53C[0]\x1b[56D";
    let cases: [(&str, usize, &[u8]); 11] = [
        (
            "right then left past both edges",
            8,
            b"error\r\n\x1b[1GR1# \x1b[999C\x1b[999Dworking",
        ),
        (
            "a shorter overwrite after both edges",
            8,
            b"error\r\n\x1b[1GR1# \x1b[999C\x1b[999Dok",
        ),
        (
            "a longer overwrite after both edges",
            8,
            b"error\r\n\x1b[1GR1# \x1b[99C\x1b[99Dworking-on-it",
        ),
        (
            "large counts right, then left",
            20,
            b"error\r\n\x1b[1Gp$ \x1b[50C\x1b[40DOK",
        ),
        (
            "left past the first column",
            20,
            b"error\r\n\x1b[1Gp$ \x1b[9Dx",
        ),
        (
            "a column past the written text, then left",
            8,
            b"error\r\n\x1b[1Gp$ \x1b[30G\x1b[20Dx",
        ),
        (
            "text past the last column wraps to the next row",
            8,
            b"error\r\n\x1b[1Gp$ \x1b[4Cxyz",
        ),
        (
            "wide characters before the prompt",
            80,
            "error\r\n\x1b[1G\u{754c}\u{754c}\x1b[Kp$ \x1b[7Gx".as_bytes(),
        ),
        (
            "combining marks before the prompt",
            80,
            "error\r\n\x1b[1Ge\u{301}e\u{301}\x1b[Kp$ \x1b[3Gx".as_bytes(),
        ),
        (
            "a zsh right prompt drawn for a wider terminal",
            40,
            zsh_right_prompt,
        ),
        (
            "a wide character in the last column wraps to the next row",
            20,
            "error\r\n\x1b[1Gp$ \x1b[99C\u{754c}".as_bytes(),
        ),
    ];
    for (name, cols, read) in cases {
        assert_bypassed_on(name, 5, cols, &[read]);
    }
}

/// A move that stops at the edge is followed as the terminal follows it, so a
/// prompt that survives it still counts, and a zsh right prompt drawn for the
/// terminal's width does too.
#[test]
fn moves_that_stop_at_the_edge_are_followed_exactly() {
    assert_only_errors_colored(
        "right and left past both edges, back after the prompt",
        5,
        20,
        &[b"error\r\n\x1b[1Gp$ \x1b[K\x1b[99C\x1b[3D[0]\x1b[99D\x1b[3C"],
        &[0],
    );
    assert_only_errors_colored(
        "zsh right prompt at its width",
        5,
        80,
        &[b"error\r\n\x1b[1G\x1b[0KHOST% \x1b[K\x1b[53C[0]\x1b[56D"],
        &[0],
    );
}

/// Without the terminal's size nothing tells where a move lands, so a read
/// with a cursor move is written as it came, as in 1.4.1. A size counts for
/// the push it was given for.
#[test]
fn a_read_pushed_without_the_terminal_size_keeps_the_bypass() {
    let read: &[u8] = b"error\r\n\x1b[1G\x1b[0KR1# ";
    let mut without_size = streaming();
    let mut output = without_size.push(read);
    output.extend(without_size.finish());
    assert_eq!(output, read, "{:?}", String::from_utf8_lossy(&output));

    let mut sized = streaming();
    sized.set_terminal_size(24, 80);
    let first = sized.push(read);
    assert!(
        contains(&first, b"\x1b[38"),
        "{:?}",
        String::from_utf8_lossy(&first)
    );
    // The next read starts on a new line, so with a size it would be
    // promoted too.
    let next: &[u8] = b"\r\nerror\r\n\x1b[1G\x1b[0KR1# ";
    let second = sized.push(next);
    assert_eq!(second, next, "{:?}", String::from_utf8_lossy(&second));
    sized.set_terminal_size(24, 80);
    let third = sized.push(next);
    assert!(
        contains(&third, b"\x1b[38"),
        "{:?}",
        String::from_utf8_lossy(&third)
    );
}

/// Xterm keeps one scroll region for both screens: `top_marg` and
/// `bot_marg` sit once in its `TScreen` (ptyx.h); `ToAlternate`,
/// `FromAlternate` and `SwitchBufs` (charproc.c, screen.c) leave them alone,
/// and the cursor that mode 1049 saves (`SavedCursor`, ptyx.h) holds none. So
/// a region set on either screen still counts after a switch. A terminal that
/// keeps one region for each screen, as Microsoft documents for its console,
/// still has the main screen's region after a reset on the alternate screen,
/// so a region set on the main screen counts until an exact reset there, or
/// RIS. Any form that may switch to the alternate screen counts as
/// switching; only an exact switch back counts as back. A switch to the
/// alternate screen while a region is set is a save of the cursor while a
/// followed state is hazardous, which keeps the session on the bypass.
#[test]
fn a_main_screen_region_counts_until_it_is_reset_there_review_70() {
    let region: &[u8] = b"\x1b[1;3r\x1b[8;1H";
    let read: &[u8] = &concat(&[b"error\r\n", PROMPT]);
    let cases: [(&str, &[&[u8]]); 8] = [
        (
            "reset on the main screen after the switch back",
            &[region, b"\x1b[?1049h", b"\x1b[?1049l", b"\x1b[r", read],
        ),
        (
            "set on the alternate screen and left set, 1049",
            &[b"\x1b[?1049h", b"\x1b[1;3r", b"\x1b[?1049l", read],
        ),
        (
            "set on the alternate screen and left set, 47",
            &[b"\x1b[?47h", b"\x1b[1;3r", b"\x1b[?47l", read],
        ),
        (
            "reset on the alternate screen, 1049",
            &[region, b"\x1b[?1049h", b"\x1b[r", b"\x1b[?1049l", read],
        ),
        (
            "reset on the alternate screen, 1047",
            &[region, b"\x1b[?1047h", b"\x1b[r", b"\x1b[?1047l", read],
        ),
        (
            "reset on the alternate screen, 47",
            &[region, b"\x1b[?47h", b"\x1b[r", b"\x1b[?47l", read],
        ),
        (
            "switched in the C1 form",
            &[region, b"\xc2\x9b?1049h", b"\x1b[r", b"\x1b[?1049l", read],
        ),
        (
            "switched back in the C1 form",
            &[
                region,
                b"\x1b[?1049h",
                b"\xc2\x9b?1049l",
                b"\x1b[r",
                b"\x1b[?1049l",
                read,
            ],
        ),
    ];
    for (name, reads) in cases {
        let output = render_on(8, 24, reads);
        assert!(
            output.ends_with(read) && !contains(&output, b"\x1b[38"),
            "{name}: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    // A region set and reset on the alternate screen leaves the main one as
    // it was, and an exact reset on the main screen clears it.
    let recovered: [(&str, &[&[u8]]); 2] = [
        (
            "region only on the alternate screen",
            &[
                b"\x1b[?1049h",
                b"\x1b[1;5r",
                b"\x1b[r",
                b"\x1b[?1049l",
                read,
            ],
        ),
        (
            "reset on the main screen before the switch",
            &[
                region,
                b"\x1b[r",
                b"\x1b[?1049h",
                b"\x1b[1;5r",
                b"\x1b[r",
                b"\x1b[?1049l",
                read,
            ],
        ),
    ];
    for (name, reads) in recovered {
        let output = render_on(8, 24, reads);
        let screen = Screen::rendered(8, 24, &output);
        let row = (0..8)
            .find(|row| screen.text(*row).starts_with("error"))
            .expect("error row");
        assert_eq!(
            red_text(&screen, row),
            (0..5).collect::<Vec<_>>(),
            "{name}: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

/// Terminal states set in an earlier read that change how wide a row is,
/// where a character lands, or which bytes are characters:
/// - a row of double width or double height (`ESC # 3`, `4`, `6`): xterm
///   stops a move at the row's last column, half the width (`CursorForward`
///   in cursor.c, with `LineMaxCol`), and only clears the row's size when it
///   clears the whole row (`ClearBufRows` in screen.c), as RIS does; `ESC # 5`
///   resets one row and DECSTR none;
/// - insert mode (`CSI 4 h`): text shifts the prompt right, past the edge;
/// - a screen size the program asks for (DECCOLM, DECSCPP, DECSNLS, a window
///   resize, a status line), which the terminal applies after the read loop
///   asked for the size;
/// - a decoding other than UTF-8 (`ESC % @`, national character sets), under
///   which a byte of a UTF-8 character can be a control.
///
/// Each keeps the bypass from then on. RIS clears the first three; only the
/// exact reverse clears a decoding switch. A line feed can also return the
/// cursor to the first column (the terminal's output processing turns it
/// into CR LF, or newline mode is on), so it leaves the column unknown.
#[test]
fn reads_after_a_layout_state_the_tracker_must_see_stay_on_the_bypass_review_77() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let cases: [(&str, usize, &[&[u8]]); 26] = [
        (
            "double width set in an earlier read",
            8,
            &[
                b"\x1b[2;1H\x1b#6\x1b[1;1H",
                b"error\r\n\x1b[1GR# \x1b[6C\x1b[2Dx",
            ],
        ),
        ("double height, top half", 8, &[b"\x1b#3", read]),
        ("double height, bottom half", 8, &[b"\x1b#4", read]),
        (
            "double width several reads earlier",
            8,
            &[b"\x1b#6", b"one\r\n", b"two\r\n", read],
        ),
        ("single width after it", 8, &[b"\x1b#6", b"\x1b#5", read]),
        (
            "double width split across reads",
            8,
            &[b"\x1b#", b"6", read],
        ),
        ("a line size not known", 8, &[b"\x1b#7", read]),
        (
            "insert mode",
            20,
            &[b"\x1b[4h", b"error\r\n\x1b[18Gp$ \x1b[1GOK"],
        ),
        ("insert mode in a list", 20, &[b"\x1b[2;4h", read]),
        ("insert mode in the C1 form", 20, &[b"\xc2\x9b4h", read]),
        (
            "insert mode reset in the C1 form",
            20,
            &[b"\x1b[4h", b"\xc2\x9b4l", read],
        ),
        ("132 columns", 20, &[b"\x1b[?3h", read]),
        ("80 columns", 20, &[b"\x1b[?3l", read]),
        ("columns per page", 20, &[b"\x1b[132$|", read]),
        ("lines per screen", 20, &[b"\x1b[36*|", read]),
        (
            "window resized in characters",
            20,
            &[b"\x1b[8;24;40t", read],
        ),
        ("window resized in pixels", 20, &[b"\x1b[4;480;640t", read]),
        ("window resized to 30 lines", 20, &[b"\x1b[30t", read]),
        ("window maximized", 20, &[b"\x1b[9;1t", read]),
        ("status line shown", 20, &[b"\x1b[2$~", read]),
        ("output to the status line", 20, &[b"\x1b[1$}", read]),
        ("a decoding other than UTF-8", 20, &[b"\x1b%@", read]),
        (
            "RIS after the decoding switch",
            20,
            &[b"\x1b%@", b"\x1bc", read],
        ),
        ("national character sets", 20, &[b"\x1b[?42h", read]),
        (
            "a line feed that may also return the cursor",
            20,
            &[b"error\r\n\rabcdefghij\np$ \x1b[1Gx"],
        ),
        (
            "a line feed after an unknown column",
            20,
            &[b"error\r\nabc\np$ \x1b[1Gx"],
        ),
    ];
    for (name, cols, reads) in cases {
        assert_bypassed_on(name, 5, cols, reads);
    }
}

/// A reset that clears those states, and controls that leave the width
/// alone, let the next prompt redraw color its lines.
#[test]
fn layout_states_that_are_reset_or_change_nothing_let_the_lines_be_colored() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let cases: [(&str, &[&[u8]]); 10] = [
        ("single width alone", &[b"\x1b#5", read]),
        ("screen alignment test", &[b"\x1b#8", read]),
        ("insert mode, then reset", &[b"\x1b[4h", b"\x1b[4l", read]),
        ("insert mode, then RIS", &[b"\x1b[4h", b"\x1bc", read]),
        ("UTF-8 again", &[b"\x1b%@", b"\x1b%G", read]),
        (
            "national character sets off",
            &[b"\x1b[?42h", b"\x1b[?42l", read],
        ),
        ("title stack", &[b"\x1b[22;0t", b"\x1b[23;0t", read]),
        ("size report", &[b"\x1b[18t", read]),
        ("keypad mode", &[b"\x1b=", b"\x1b>", read]),
        (
            "a line feed from the first column",
            &[b"error\r\n\x1b[1G\nR# "],
        ),
    ];
    for (name, reads) in cases {
        let output = render_on(6, 24, reads);
        let screen = Screen::rendered(6, 24, &output);
        let row = (0..6)
            .find(|row| screen.text(*row).starts_with("error"))
            .unwrap_or_else(|| {
                panic!(
                    "{name}: no error row: {:?}",
                    String::from_utf8_lossy(&output)
                )
            });
        assert_eq!(
            red_text(&screen, row),
            (0..5).collect::<Vec<_>>(),
            "{name}: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

/// The library starts out not knowing that the terminal reads UTF-8. A byte
/// past ASCII can then be a control (the UTF-8 `\u{14d}` holds `8D`, reverse
/// index, to a terminal that reads 8-bit bytes) or take a column of its own,
/// so a read with one in its text keeps the bypass.
#[test]
fn a_read_with_text_past_ascii_keeps_the_bypass_until_the_terminal_reads_utf8() {
    let cases: [(&str, &[u8]); 2] = [
        (
            "in an output line",
            "error \u{14d}\r\n\x1b[1GR# ".as_bytes(),
        ),
        ("in the prompt", "error\r\n\x1b[1G\u{3bb} ".as_bytes()),
    ];
    for (name, read) in cases {
        assert_bypassed(name, &[read]);
    }
}

/// Only controls on the table of controls proved to leave later output
/// drawn as it would be without them may come before a promoted read: any
/// other one, known or not, keeps every later read on the bypass until RIS.
/// VT52 mode (`CSI ? 2 l`) turns the terminal's parser over to VT52, where
/// `ESC [` starts nothing and the added SGR bytes would show as text
/// (xterm: charproc.c, VTPrsTbl.c); Tektronix mode (`CSI ? 38 h`) leaves
/// the VT parser altogether.
#[test]
fn reads_after_a_control_off_the_table_stay_on_the_bypass_until_ris_review_79() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let cases: [(&str, &[&[u8]]); 32] = [
        (
            "VT52 mode, then what looks like RIS",
            &[b"\x1b[?2l\x1bc", read],
        ),
        (
            "VT52 mode, then what looks like RIS in its own read",
            &[b"\x1b[?2l", b"\x1bc", read],
        ),
        (
            "Tektronix mode, then what looks like RIS",
            &[b"\x1b[?38h\x1bc", read],
        ),
        ("double width, then RIS", &[b"\x1b#6", b"\x1bc", read]),
        ("132 columns, then RIS", &[b"\x1b[?3h", b"\x1bc", read]),
        (
            "a soft reset, then autowrap on",
            &[b"\x1b[!p", b"\x1b[?7h", read],
        ),
        ("VT52 mode, the read before", &[b"\x1b[?2l", read]),
        (
            "VT52 mode, several reads before",
            &[b"\x1b[?2l", b"one\r\n", b"\x1b[?25h", b"two\r\n", read],
        ),
        ("Tektronix mode, the read before", &[b"\x1b[?38h", read]),
        (
            "Tektronix mode, several reads before",
            &[b"\x1b[?38h", b"one\r\n", b"\x1b[?25h", b"two\r\n", read],
        ),
        ("VT52 mode restored", &[b"\x1b[?2s", b"\x1b[?2r", read]),
        ("Tektronix mode restored", &[b"\x1b[?38r", read]),
        ("column mode restored", &[b"\x1b[?3r", read]),
        ("a private mode not on the table", &[b"\x1b[?1234h", read]),
        (
            "a listed mode beside one that is not",
            &[b"\x1b[?25;2l", read],
        ),
        ("an ANSI mode not on the table", &[b"\x1b[20h", read]),
        ("an escape not on the table", &[b"\x1bn", read]),
        ("a character set designated", &[b"\x1b(0", read]),
        ("shift out in the text", &[b"x\x0e", b"\x0f", read]),
        ("a DCS string", &[b"\x1bP$qm\x1b\\", read]),
        ("an APC string", &[b"\x1b_Ga=d\x1b\\", read]),
        ("an SGR in the C1 form", &[b"\xc2\x9b1m", read]),
        (
            "a CSI with an intermediate not on the table",
            &[b"\x1b[1\"q", read],
        ),
        ("a CSI final not on the table", &[b"\x1b[1z", read]),
        (
            "an image in OSC 1337",
            &[b"\x1b]1337;File=inline=1:AA==\x07", read],
        ),
        ("a font change", &[b"\x1b]50;fixed\x07", read]),
        ("no status line", &[b"\x1b[0$~", read]),
        ("a window moved", &[b"\x1b[3;0;0t", read]),
        ("a soft reset leaves autowrap off", &[b"\x1b[!p", read]),
        (
            "an ESC in the C1 form inside UTF-8 text",
            &["\u{85}".as_bytes(), read],
        ),
        ("RIS, then VT52 mode", &[b"\x1bc", b"\x1b[?2l", read]),
        (
            "VT52 mode, then the alternate screen and back",
            &[b"\x1b[?2l", b"\x1b[?1049h", b"\x1b[?1049l", read],
        ),
    ];
    for (name, reads) in cases {
        assert_bypassed_on(name, 6, 24, reads);
    }
}

/// The controls on the table let the next prompt redraw color its lines:
/// those a shell, a line editor, a pager or vim send, and RIS after one
/// that is not on the table.
#[test]
fn reads_after_controls_on_the_table_let_the_lines_be_colored_review_79() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let cases: [(&str, &[&[u8]]); 18] = [
        ("RIS with nothing off the table before it", &[b"\x1bc", read]),
        (
            "a save while every followed mode is safe",
            &[b"\x1b7\x1b[?7l\x1b[?7h\x1b8\x1b[s\x1b[u", read],
        ),
        (
            "the alternate screen entered while every followed mode is safe",
            &[b"\x1b[?1049h\x1b[?7l\x1b[?7h\x1b[?1049l", read],
        ),
        ("cursor shown and hidden", &[b"\x1b[?25l", b"\x1b[?25h", read]),
        (
            "mouse and focus reporting",
            &[b"\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h", b"\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1004l", read],
        ),
        ("bracketed paste and cursor keys", &[b"\x1b[?2004h\x1b[?1h\x1b=", b"\x1b[?2004l\x1b[?1l\x1b>", read]),
        ("cursor shape", &[b"\x1b[2 q", read]),
        ("key modifier options", &[b"\x1b[>4;2m\x1b[?4m", b"\x1b[>4m", read]),
        ("keyboard protocol push and pop", &[b"\x1b[>1u\x1b[?u", b"\x1b[<u", read]),
        (
            "vim's compatibility probe",
            &[b"\x1b[3;1H\x1bPzz\x1b\\\x1b[0%m\x1b[6n", read],
        ),
        ("device queries", &[b"\x1b[c\x1b[>c\x1b[5n\x1b[?1$p", read]),
        (
            "save and restore the cursor",
            &[b"\x1b7\x1b[5;5H\x1b8\x1b[s\x1b[u", read],
        ),
        ("the alternate screen and back", &[b"\x1b[?1049h", b"hello\x1b[?1049l", read]),
        (
            "titles, directory, links and marks",
            &[b"\x1b]0;t\x07\x1b]7;file://h/p\x07\x1b]8;;http://h\x1b\\\x1b]8;;\x1b\\\x1b]133;A\x07", read],
        ),
        ("colors set and queried", &[b"\x1b]11;?\x07\x1b]4;1;#ff0000\x07\x1b]104\x07", read]),
        ("G0 back to ASCII", &[b"\x1b(B\x1b[m", read]),
        ("tab stops", &[b"\x1bH\x1b[3g", read]),
        ("edits that leave no mode", &[b"\x1b[2@\x1b[3P\x1b[2X\x1b[1L\x1b[1M\x1b[S\x1b[T", read]),
    ];
    for (name, reads) in cases {
        let output = render_on(6, 24, reads);
        let screen = Screen::rendered(6, 24, &output);
        let row = (0..6)
            .find(|row| screen.text(*row).starts_with("error"))
            .unwrap_or_else(|| {
                panic!(
                    "{name}: no error row: {:?}",
                    String::from_utf8_lossy(&output)
                )
            });
        assert_eq!(
            red_text(&screen, row),
            (0..5).collect::<Vec<_>>(),
            "{name}: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

/// A saved cursor can bring a mode back. Kitty saves autowrap with the
/// cursor and restores it (screen.c `screen_save_cursor`,
/// `screen_restore_cursor`), on `ESC 7`, `CSI s` and on the switch to the
/// alternate screen by mode 1049; iTerm2 does the same (VT100Terminal.m
/// `saveCursor`, `restoreCursor`). A save while any followed mode is in its
/// hazardous value keeps every later read on the bypass for the session;
/// so does a soft reset, which can leave autowrap off and saves the cursor.
#[test]
fn reads_after_a_save_while_a_mode_is_hazardous_stay_on_the_bypass_review_82() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let cases: [(&str, &[&[u8]]); 14] = [
        (
            "margins set while mode 69 is on, taken as a save",
            &[b"\x1b[?69h\x1b[2;10s\x1b[?69l", read],
        ),
        (
            "autowrap off, then the alternate screen by mode 47",
            &[b"\x1b[?7l\x1b[?47h\x1b[?7h\x1b[?47l", read],
        ),
        (
            "autowrap off, then the alternate screen by mode 1047",
            &[b"\x1b[?7l\x1b[?1047h\x1b[?7h\x1b[?1047l", read],
        ),
        (
            "autowrap off saved by DECSC, on, restored",
            &[b"\x1b[?7l\x1b7\x1b[?7h\x1b8", read],
        ),
        (
            "autowrap off saved by SCOSC, on, restored",
            &[b"\x1b[?7l\x1b[s\x1b[?7h\x1b[u", read],
        ),
        (
            "autowrap off saved by the alternate screen",
            &[b"\x1b[?7l\x1b[?1049h\x1b[?7h\x1b[?1049l", read],
        ),
        (
            "the save and the reset in reads of their own",
            &[b"\x1b[?7l", b"\x1b7", b"\x1b[?7h", b"\x1b8", read],
        ),
        (
            "a save with no restore after it",
            &[b"\x1b[?7l\x1b7\x1b[?7h", read],
        ),
        (
            "a save while insert mode is on",
            &[b"\x1b[4h\x1b7\x1b[4l\x1b8", read],
        ),
        (
            "a save while a scroll region is set",
            &[b"\x1b[1;3r\x1b7\x1b[r\x1b8", read],
        ),
        (
            "a save while reverse wrap is on",
            &[b"\x1b[?45h\x1b7\x1b[?45l\x1b8", read],
        ),
        (
            "a save while the margins are on",
            &[b"\x1b[?69h\x1b7\x1b[?69l\x1b8", read],
        ),
        (
            "the alternate screen entered in a list after autowrap off",
            &[b"\x1b[?7l", b"\x1b[?25;1049h\x1b[?7h\x1b[?1049l", read],
        ),
        ("a soft reset", &[b"\x1b[!p", read]),
    ];
    for (name, reads) in cases {
        assert_bypassed_on(name, 6, 24, reads);
    }
}

/// A string on the table is inert only when nothing inside it can run as a
/// control: xterm runs a raw C1 CSI inside an OSC string (VTPrsTbl.c). A
/// payload with a C0 control, ESC, DEL or a byte from 80 to 9F keeps the
/// read on the bypass, and so does any byte from 80 up while the terminal
/// is not known to read UTF-8, as here: the library is not told it does.
#[test]
fn reads_after_a_string_holding_a_control_stay_on_the_bypass_review_82() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let earlier: [(&str, &[u8]); 12] = [
        ("a line feed in a title", b"\x1b]0;a\nb\x07"),
        (
            "a UTF-8 title while the terminal is not known to read UTF-8",
            b"\x1b]0;caf\xc3\xa9\x07",
        ),
        ("a C1 CSI in a title", b"\x1b]0;X\x9b?2l\x07"),
        ("a C1 CSI in a title ended by ST", b"\x1b]2;X\x9b?2l\x1b\\"),
        ("a C1 CSI in a hyperlink", b"\x1b]8;;http://h/\x9b?2l\x07"),
        ("a C1 CSI in the clipboard", b"\x1b]52;c;\x9b?2l\x07"),
        ("a C1 CSI in OSC 1337", b"\x1b]1337;CurrentDir=/\x9b?2l\x07"),
        ("a UTF-8 C1 CSI in a title", b"\x1b]0;X\xc2\x9b?2l\x07"),
        ("shift out in a title", b"\x1b]0;X\x0eY\x07"),
        ("DEL in a title", b"\x1b]0;X\x7fY\x07"),
        ("an escape in a title", b"\x1b]0;X\x1b[?2lY\x07"),
        ("a byte past ASCII in a title", b"\x1b]0;caf\xe9\x07"),
    ];
    for (name, string) in earlier {
        assert_bypassed_on(name, 6, 24, &[string, read]);
    }
    let same_read: [(&str, &[u8]); 3] = [
        (
            "a line feed in a title",
            b"error\r\n\x1b]2;a\nb\x07\x1b[1GR# ",
        ),
        (
            "a C1 CSI in a title",
            b"error\r\n\x1b]0;X\x9b?2l\x07\x1b[1GR# ",
        ),
        (
            "a C1 CSI in the directory",
            b"error\r\n\x1b]7;file://h/\x9b?2l\x07\x1b[1GR# ",
        ),
    ];
    for (name, string) in same_read {
        assert_bypassed_on(name, 6, 24, &[string]);
    }
}

/// Accepted limit: the lines are colored when they are written, and a later
/// read that goes back into one and rewrites only part of it without
/// erasing leaves color on the rest. Pinned so a change to it is seen.
#[test]
fn documented_limit_a_later_read_rewrites_part_of_a_colored_line() {
    let up = render_on(6, 24, &[b"error error\r\n\x1b[1Gp$ ", b"\x1b[1A\x1b[1GOK"]);
    let screen = Screen::rendered(6, 24, &up);
    assert_eq!(screen.text(0), "OKror error");
    assert_eq!(
        red_text(&screen, 0),
        vec![2, 3, 4, 6, 7, 8, 9, 10],
        "{:?}",
        String::from_utf8_lossy(&up)
    );

    let shell = render_on(
        6,
        24,
        &[
            b"error error\r\n\x1b[1G\x1b[0Kuser@host:~$ ",
            b"\x1b[1;1HOK",
        ],
    );
    let screen = Screen::rendered(6, 24, &shell);
    assert_eq!(screen.text(0), "OKror error");
    assert_eq!(red_text(&screen, 0), vec![2, 3, 4, 6, 7, 8, 9, 10]);

    let restore = render_on(6, 24, &[b"\x1b7", b"error error\r\n\x1b[1Gp$ ", b"\x1b8OK"]);
    let screen = Screen::rendered(6, 24, &restore);
    assert_eq!(screen.text(0), "OKror error");
    assert_eq!(red_text(&screen, 0), vec![2, 3, 4, 6, 7, 8, 9, 10]);

    // An erase in the later read leaves nothing behind.
    let erased = render_on(
        6,
        24,
        &[b"error error\r\n\x1b[1Gp$ ", b"\x1b[1A\x1b[1G\x1b[KOK"],
    );
    let screen = Screen::rendered(6, 24, &erased);
    assert_eq!(screen.text(0), "OK");
    assert!(red_text(&screen, 0).is_empty());
}

/// Accepted limit: a mode the terminal was in before PrismTTY started is not
/// seen. With reverse wraparound 1045 already on, a backward move from the
/// prompt row lands on the colored line. Pinned so a change to it is seen.
#[test]
fn documented_limit_a_mode_set_before_prismtty_started() {
    // In the default modes `CSI 11D` stops at the first column and `OK`
    // lands left of the prompt; with 1045 already on it steps back onto
    // the colored line.
    let output = render_on(5, 8, &[b"error\r\n\x1b[4Gp$ \x1b[11DOK"]);
    let default_modes = Screen::rendered(5, 8, &output);
    assert_eq!(default_modes.text(0), "error");
    assert_eq!(default_modes.text(1), "OK p$");
    assert_eq!(red_text(&default_modes, 0), vec![0, 1, 2, 3, 4]);
    let mut screen = Screen::new(5, 8).with_reverse_wrap_any();
    screen.feed(&output);
    assert_eq!(screen.text(0), "errOK");
    assert_eq!(
        red_text(&screen, 0),
        vec![0, 1, 2],
        "{:?}",
        String::from_utf8_lossy(&output)
    );
}

// CLI, in a PTY.

struct Run {
    renders: Vec<u8>,
    render_records: Vec<Vec<u8>>,
    reads: Vec<Vec<u8>>,
}

fn trace_records(path: &Path, kind: &str) -> Vec<Vec<u8>> {
    let text = std::fs::read_to_string(path).expect("trace");
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _time = fields.next()?;
            (fields.next()? == kind).then(|| {
                let hex: String = fields.collect();
                (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                    .collect()
            })
        })
        .collect()
}

/// Runs `script` under `sh -c` inside prismtty, inside a PTY of `rows` rows,
/// with a scratch home and the test rules. `during` runs while the command
/// does, with the PTY master and the fifo directory.
fn run_in_pty(
    rows: u16,
    script: &str,
    during: impl FnOnce(&dyn portable_pty::MasterPty, &Path),
) -> Run {
    run_in_pty_of(rows, 40, script, during)
}

/// As [`run_in_pty`], in a PTY `cols` wide.
fn run_in_pty_of(
    rows: u16,
    cols: u16,
    script: &str,
    during: impl FnOnce(&dyn portable_pty::MasterPty, &Path),
) -> Run {
    run_in_pty_with(rows, cols, &[], script, during)
}

/// As [`run_in_pty_of`], with `extra` arguments to prismtty.
fn run_in_pty_with(
    rows: u16,
    cols: u16,
    extra: &[&str],
    script: &str,
    during: impl FnOnce(&dyn portable_pty::MasterPty, &Path),
) -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let runtime = dir.path().join("rt");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&runtime).expect("runtime");
    std::fs::set_permissions(
        &runtime,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod");
    let config = dir.path().join("rules.yml");
    std::fs::write(&config, RULES).expect("rules");
    let trace = dir.path().join("io.trace");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_prismtty"));
    builder.env_clear();
    builder.env("HOME", &home);
    builder.env("XDG_CONFIG_HOME", dir.path().join("xdg"));
    builder.env("PRISMTTY_RUNTIME_DIR", &runtime);
    builder.env("PATH", "/usr/bin:/bin");
    builder.env("TERM", "xterm-256color");
    builder.env("LANG", "en_US.UTF-8");
    builder.env("FIFOS", dir.path());
    builder.args(["-c"]);
    builder.arg(&config);
    builder.args([
        "--rgb",
        "--no-auto-detect",
        "--no-dynamic-profile",
        "--trace-io",
    ]);
    builder.arg(&trace);
    builder.args(extra);
    // Raw output in the child's PTY, so the bytes arrive as written; each
    // printf goes through cat so that it is one write, not one per line.
    let script = format!("stty -opost; {script}");
    builder.args(["--", "sh", "-c", &script]);
    let mut child = pair.slave.spawn_command(builder).expect("spawn prismtty");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let (sender, receiver) = mpsc::channel();
    let drain = thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    during(&*pair.master, dir.path());
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().expect("wait").is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "prismtty did not finish: {:?}",
            receiver.try_iter().flatten().collect::<Vec<u8>>()
        );
    }
    drop(pair.master);
    let _ = drain.join();
    let render_records = trace_records(&trace, "RENDER");
    Run {
        renders: render_records.concat(),
        render_records,
        reads: trace_records(&trace, "OUT"),
    }
}

fn release_gate(dir: &Path, name: &str) {
    let path: PathBuf = dir.join(name);
    // Opening a fifo for writing waits until the script opens it to read.
    std::fs::write(&path, b"go\n").expect("release gate");
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{} never appeared", path.display());
}

#[test]
fn pty_output_written_with_its_prompt_redraw_in_one_write_is_colored() {
    let run = run_in_pty(
        24,
        "printf 'link is down error 192.0.2.1\\r\\nsecond error\\r\\n\\033[1G\\033[0Kuser@host ~ $ ' | cat; sleep 0.2",
        |_, _| {},
    );
    let write = b"link is down error 192.0.2.1\r\nsecond error\r\n\x1b[1G\x1b[0Kuser@host ~ $ ";
    assert_eq!(
        run.reads,
        vec![write.to_vec()],
        "the child's one write came as one read"
    );
    // The CLI adds the generic profile's rules, so other words can be
    // colored too: every error is red, the text is as written and the
    // prompt redraw is written unchanged.
    let screen = Screen::rendered(24, 40, &run.renders);
    assert!(
        screen.same_text(&Screen::rendered(24, 40, write)),
        "{:?}",
        String::from_utf8_lossy(&run.renders)
    );
    for row in 0..2 {
        let red = red_text(&screen, row);
        assert!(
            word_columns(&screen, row, "error")
                .iter()
                .all(|col| red.contains(col)),
            "row {row}: {:?}",
            String::from_utf8_lossy(&run.renders)
        );
    }
    assert!(red_text(&screen, 2).is_empty());
    assert!(run.renders.ends_with(b"\x1b[1G\x1b[0Kuser@host ~ $ "));
}

#[test]
fn pty_moves_that_stop_at_the_edge_are_judged_against_the_terminal_width() {
    // Review 70's read in a PTY 8 columns wide: `CSI 999C` stops at the last
    // column, `CSI 999D` at the first, and `working` writes over `R1# `.
    let overwritten =
        "printf 'error\\r\\n\\033[1GR1# \\033[999C\\033[999Dworking' | cat; sleep 0.2";
    let run = run_in_pty_of(5, 8, overwritten, |_, _| {});
    assert_eq!(run.reads.len(), 1, "one read: {:?}", run.reads);
    assert_eq!(
        run.renders,
        run.reads.concat(),
        "{:?}",
        String::from_utf8_lossy(&run.renders)
    );
    // Back past the prompt instead, it survives and the line is colored.
    let kept = "printf 'error\\r\\n\\033[1GR1# \\033[999C\\033[999D\\033[4C' | cat; sleep 0.2";
    let run = run_in_pty_of(5, 8, kept, |_, _| {});
    assert_eq!(run.reads.len(), 1, "one read: {:?}", run.reads);
    assert!(
        contains(&run.renders, b"\x1b[38;2;255;0;0m"),
        "{:?}",
        String::from_utf8_lossy(&run.renders)
    );
}

/// A `sh` script that writes `read` in one write: printf, with every byte
/// past printable ASCII, and `\\`, `%` and `'`, as an octal escape.
fn printf_script(read: &[u8]) -> String {
    let mut format = String::new();
    for byte in read {
        match byte {
            b' '..=b'~' if !matches!(byte, b'\\' | b'%' | b'\'') => format.push(char::from(*byte)),
            _ => format.push_str(&format!("\\{byte:03o}")),
        }
    }
    format!("printf '{format}' | cat; sleep 0.2")
}

/// `read` written by the child in one write, in a PTY `rows` by `cols`.
fn pty_one_read(rows: u16, cols: u16, read: &[u8]) -> Run {
    let run = run_in_pty_of(rows, cols, &printf_script(read), |_, _| {});
    assert_eq!(run.reads, vec![read.to_vec()], "one read");
    run
}

/// Every `error` in the first rows of `run` is red, and the text is what the
/// unchanged bytes show. The command line adds the generic profile's rules,
/// so other words may be colored too.
fn assert_errors_colored(name: &str, rows: usize, cols: usize, run: &Run, read: &[u8]) {
    let screen = Screen::rendered(rows, cols, &run.renders);
    assert!(
        screen.same_text(&Screen::rendered(rows, cols, read)),
        "{name}: {:?}",
        String::from_utf8_lossy(&run.renders)
    );
    assert!(
        contains(&run.renders, b"\x1b[38;2;255;0;0m"),
        "{name}: {:?}",
        String::from_utf8_lossy(&run.renders)
    );
    for row in 0..rows {
        let red = red_text(&screen, row);
        assert!(
            word_columns(&screen, row, "error")
                .iter()
                .all(|col| red.contains(col)),
            "{name}: row {row}: {:?}",
            String::from_utf8_lossy(&run.renders)
        );
    }
}

/// With the terminal reading UTF-8 (the command line takes it from the
/// locale), characters past ASCII in the lines are colored around, and the
/// width model's cases keep the bypass.
#[test]
fn pty_characters_past_ascii_are_judged_as_a_utf8_terminal_draws_them() {
    let colored: [(&str, &[u8]); 2] = [
        (
            "wide glyph and long line",
            "\u{754c} error ab error cd\r\n\x1b[1Gp$ ".as_bytes(),
        ),
        ("combining mark", "e\u{301} error\r\n\x1b[1Gp$ ".as_bytes()),
    ];
    for (name, read) in colored {
        let run = pty_one_read(6, 10, read);
        assert_errors_colored(name, 6, 10, &run, read);
    }
    let raw: [(&str, u16, &[u8]); 3] = [
        (
            "wide characters before the prompt",
            80,
            "error\r\n\x1b[1G\u{754c}\u{754c}\x1b[Kp$ \x1b[7Gx".as_bytes(),
        ),
        (
            "combining marks before the prompt",
            80,
            "error\r\n\x1b[1Ge\u{301}e\u{301}\x1b[Kp$ \x1b[3Gx".as_bytes(),
        ),
        (
            "a wide character in the last column",
            20,
            "error\r\n\x1b[1Gp$ \x1b[99C\u{754c}".as_bytes(),
        ),
    ];
    for (name, cols, read) in raw {
        let run = pty_one_read(5, cols, read);
        assert_eq!(
            run.renders,
            read,
            "{name}: {:?}",
            String::from_utf8_lossy(&run.renders)
        );
    }
}

/// A prompt holding characters past ASCII has a width PrismTTY does not
/// know, so nothing after it can be placed: an erase after it forgets it,
/// as any other control does, and the read keeps the bypass.
#[test]
fn pty_a_unicode_prompt_followed_by_an_erase_keeps_the_bypass_review_79() {
    let after_erase: [(&str, u16, &[u8]); 6] = [
        (
            "then CSI K",
            80,
            "error\r\n\x1b[1G\u{276f} \x1b[K".as_bytes(),
        ),
        (
            "then CSI 0K",
            80,
            "error\r\n\x1b[1G\u{276f} \x1b[0K".as_bytes(),
        ),
        (
            "a wide CJK character",
            80,
            "error\r\n\x1b[1G~/\u{754c} \u{276f} \x1b[K".as_bytes(),
        ),
        (
            "combining marks",
            80,
            "error\r\n\x1b[1Ge\u{301}t\u{e9} \u{276f} \x1b[K".as_bytes(),
        ),
        (
            "redrawn after an erase below, then bracketed paste",
            80,
            "error\r\n\x1b[1G\x1b[J\u{276f} \x1b[K\x1b[?2004h".as_bytes(),
        ),
        (
            "the bound just before the edge",
            8,
            "error\r\n\x1b[4G\u{276f} \x1b[K".as_bytes(),
        ),
    ];
    for (name, cols, read) in after_erase {
        let run = pty_one_read(5, cols, read);
        assert_eq!(
            run.renders,
            read,
            "{name}: {:?}",
            String::from_utf8_lossy(&run.renders)
        );
    }
    let raw: [(&str, u16, &[u8]); 6] = [
        (
            "the bound reaches the edge",
            8,
            "error\r\n\x1b[6G\u{276f} \x1b[K".as_bytes(),
        ),
        ("CSI 1K", 80, "error\r\n\x1b[1G\u{276f} \x1b[1K".as_bytes()),
        ("CSI 2K", 80, "error\r\n\x1b[1G\u{276f} \x1b[2K".as_bytes()),
        (
            "a right prompt after the erase",
            80,
            "error\r\n\x1b[1G\u{276f} \x1b[K\x1b[73C[0]\x1b[76D".as_bytes(),
        ),
        (
            "erase below",
            80,
            "error\r\n\x1b[1G\u{276f} \x1b[J".as_bytes(),
        ),
        (
            "text between the prompt and the erase",
            80,
            "error\r\n\x1b[1G\u{276f} x\x1b[K".as_bytes(),
        ),
    ];
    for (name, cols, read) in raw {
        let run = pty_one_read(5, cols, read);
        assert_eq!(
            run.renders,
            read,
            "{name}: {:?}",
            String::from_utf8_lossy(&run.renders)
        );
    }
}

/// The reads written by the child one after another, each in one write,
/// in a PTY `rows` by `cols`.
fn pty_reads(rows: u16, cols: u16, extra: &[&str], reads: &[&[u8]]) -> Run {
    let script = reads
        .iter()
        .map(|read| printf_script(read).replace("; sleep 0.2", "; sleep 0.3"))
        .collect::<Vec<_>>()
        .join("; ");
    let run = run_in_pty_with(rows, cols, extra, &script, |_, _| {});
    assert_eq!(
        run.reads.len(),
        reads.len(),
        "one read each: {:?}",
        run.reads
    );
    run
}

/// Where the terminal reads UTF-8, a string on the table holding a C1
/// control, in its UTF-8 form or as a byte that is no UTF-8, keeps the read
/// on the bypass; one holding plain UTF-8 text does not.
#[test]
fn pty_strings_are_judged_as_a_utf8_terminal_reads_them_review_82() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let raw: [(&str, &[&[u8]]); 3] = [
        (
            "a UTF-8 C1 CSI in a title",
            &[b"\x1b]0;X\xc2\x9b?2l\x07", read],
        ),
        (
            "a byte that is no UTF-8 in a title",
            &[b"\x1b]0;X\x9b?2l\x07", read],
        ),
        (
            "a UTF-8 C1 CSI in a title in the same read",
            &[b"error\r\n\x1b]2;X\xc2\x9b?2l\x07\x1b[1GR# "],
        ),
    ];
    for (name, reads) in raw {
        let run = pty_reads(5, 40, &[], reads);
        assert_eq!(
            run.renders,
            reads.concat(),
            "{name}: {:?}",
            String::from_utf8_lossy(&run.renders)
        );
    }
    let colored: [(&str, &[&[u8]]); 2] = [
        ("a UTF-8 title", &["\x1b]0;caf\u{e9}\x07".as_bytes(), read]),
        (
            "a UTF-8 title in the same read",
            &["error\r\n\x1b]2;caf\u{e9}\x07\x1b[1GR# ".as_bytes()],
        ),
    ];
    for (name, reads) in colored {
        let run = pty_reads(5, 40, &[], reads);
        assert!(
            contains(&run.renders, b"\x1b[38;2;255;0;0m"),
            "{name}: {:?}",
            String::from_utf8_lossy(&run.renders)
        );
    }
}

/// With `--local-echo` PrismTTY writes typed bytes to the terminal outside
/// the highlighter, where the guard does not see them, so no read is
/// promoted in that session.
#[test]
fn pty_local_echo_keeps_every_read_on_the_bypass_review_82() {
    let read: &[u8] = b"error\r\n\x1b[1GR# ";
    let run = pty_reads(5, 40, &["--local-echo"], &[read]);
    assert_eq!(
        run.renders,
        read,
        "{:?}",
        String::from_utf8_lossy(&run.renders)
    );
    let run = pty_reads(5, 40, &[], &[read]);
    assert!(
        contains(&run.renders, b"\x1b[38;2;255;0;0m"),
        "{:?}",
        String::from_utf8_lossy(&run.renders)
    );
}

#[test]
fn pty_scroll_region_is_judged_against_the_terminal_height() {
    let script = "printf '\\033[1;8r'; sleep 0.3; printf 'error\\r\\n\\033[1Gp$ ' | cat; sleep 0.2";
    let whole = run_in_pty(8, script, |_, _| {});
    assert!(
        contains(&whole.renders, b"\x1b[38;2;255;0;0m"),
        "a region over all 8 rows changes nothing: {:?}",
        String::from_utf8_lossy(&whole.renders)
    );
    let partial = run_in_pty(10, script, |_, _| {});
    assert_eq!(
        partial.renders,
        partial.reads.concat(),
        "rows 1 to 8 of 10 keep the bypass"
    );
}

#[test]
fn pty_resize_rechecks_a_recorded_scroll_region() {
    // The script writes one prompt read per gate; the test resizes the PTY
    // between them. A region over rows 1 to 8 is the whole screen at 8 rows
    // and a partial one at 10.
    let script = "mkfifo \"$FIFOS/g1\" \"$FIFOS/g2\"; printf '\\033[1;8r'; sleep 0.3; \
                  printf '\\r\\nerror one\\r\\n\\033[1Gp$ ' | cat; touch \"$FIFOS/one\"; \
                  read a < \"$FIFOS/g1\"; printf '\\r\\nerror two\\r\\n\\033[1Gp$ ' | cat; touch \"$FIFOS/two\"; \
                  read b < \"$FIFOS/g2\"; printf '\\r\\nerror three\\r\\n\\033[1Gp$ ' | cat; sleep 0.2";
    let run = run_in_pty(8, script, |master, dir| {
        wait_for_file(&dir.join("one"));
        thread::sleep(Duration::from_millis(300));
        master
            .resize(PtySize {
                rows: 10,
                cols: 40,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize to 10");
        thread::sleep(Duration::from_millis(100));
        release_gate(dir, "g1");
        wait_for_file(&dir.join("two"));
        thread::sleep(Duration::from_millis(300));
        master
            .resize(PtySize {
                rows: 8,
                cols: 40,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize to 8");
        thread::sleep(Duration::from_millis(100));
        release_gate(dir, "g2");
    });
    let record = |word: &[u8]| {
        run.render_records
            .iter()
            .find(|record| contains(record, word))
            .unwrap_or_else(|| {
                panic!(
                    "no render holds {:?}: {:?}",
                    String::from_utf8_lossy(word),
                    run.render_records
                )
            })
    };
    let red = |record: &Vec<u8>| contains(record, b"\x1b[38;2;255;0;0m");
    assert!(
        red(record(b"one")),
        "8 rows: the region is the whole screen"
    );
    assert!(!red(record(b"two")), "10 rows: the region is partial");
    assert!(red(record(b"three")), "8 rows again");
}
