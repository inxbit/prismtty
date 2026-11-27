//! A small terminal model for tests that assert what a terminal shows, not
//! only the bytes PrismTTY writes: every drawn cell's character and its
//! rendition, as xterm reads SGR for the groups those tests look at.
//!
//! It knows SGR (plain codes, and 38, 48 and 58 with their `5;n` and
//! `2;r;g;b` arguments), CR, LF and BS, the C0 controls a terminal runs
//! inside a CSI, CAN and SUB cancelling one, `CSI K` with background-color
//! erase over a fixed width, DECSCNM (`CSI ? 5 h` and `l`), RIS and DECSTR,
//! and OSC 8 hyperlinks. With `sgr0_closes_links` set, SGR 0 also ends a
//! hyperlink, as WezTerm does; xterm and most others leave it open. With
//! `sgr_stack` set it keeps xterm's SGR stack (XTPUSHSGR `CSI Pm # {` or
//! `# p` with xterm's save mask, XTPOPSGR `CSI # }` or `# q` with any
//! parameters, ten entries deep, kept through RIS and DECSTR as xterm keeps
//! it); without it, as on most terminals, both are ignored. Other controls are skipped.
//!
//! It is not a full screen: rows grow without limit, so a line feed never
//! scrolls a blank line in, nothing wraps, framed, encircled and ideogram
//! lines (SGR 51 to 65) are not drawn, and the output is read as UTF-8 with
//! invalid bytes replaced, so it cannot judge a raw C1 byte. Tests that
//! depend on those check the exact bytes as well.

#![allow(dead_code)]

use std::collections::BTreeMap;

/// The width `CSI K` erases to.
pub const SCREEN_WIDTH: usize = 40;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Pen {
    foreground: Option<String>,
    background: Option<String>,
    underline_color: Option<String>,
    bold: bool,
    faint: bool,
    italic: bool,
    /// xterm keeps single and double underline as two flags.
    underline: bool,
    double_underline: bool,
    blink: bool,
    inverse: bool,
    conceal: bool,
    strike: bool,
    overline: bool,
}

/// What an XTPUSHSGR with `parameters` (its body before `#`) saves, as
/// xterm's charproc.c computes the mask: everything (`None`) with no
/// parameter or a single empty one, else the items among 1 bold, 2 faint,
/// 3 italic, 4 underline, 5 blink, 7 inverse, 8 invisible, 9 crossed-out,
/// 21 double underline, 30 foreground and 31 background, with 10 and 11
/// read as 30 and 31. Any other item, 0 and an empty one among several
/// included, selects nothing, so `CSI 0 # p` and `CSI ; # p` save nothing.
fn push_mask(parameters: &str) -> Option<Vec<u32>> {
    if parameters.is_empty() {
        return None;
    }
    Some(
        parameters
            .split(';')
            .map(|item| match item.parse::<u32>().unwrap_or(0) {
                10 => 30,
                11 => 31,
                item => item,
            })
            .filter(|item| matches!(item, 1..=5 | 7..=9 | 21 | 30 | 31))
            .collect(),
    )
}

/// Restores what `mask` (from `push_mask`) selects of `saved`.
fn restore_masked(pen: &mut Pen, saved: &Pen, mask: &Option<Vec<u32>>) {
    let Some(mask) = mask else {
        *pen = saved.clone();
        return;
    };
    for item in mask {
        match item {
            1 => pen.bold = saved.bold,
            2 => pen.faint = saved.faint,
            3 => pen.italic = saved.italic,
            4 => pen.underline = saved.underline,
            21 => pen.double_underline = saved.double_underline,
            5 => pen.blink = saved.blink,
            7 => pen.inverse = saved.inverse,
            8 => pen.conceal = saved.conceal,
            9 => pen.strike = saved.strike,
            30 => pen.foreground = saved.foreground.clone(),
            31 => pen.background = saved.background.clone(),
            _ => {}
        }
    }
}

/// xterm keeps ten pushed renditions; a push past them only counts, and the
/// pop that uncounts it restores nothing.
const SGR_STACK_DEPTH: usize = 10;

impl Pen {
    fn signature(&self, link: bool) -> String {
        let mut parts = Vec::new();
        if let Some(color) = &self.foreground {
            parts.push(format!("fg={color}"));
        }
        if let Some(color) = &self.background {
            parts.push(format!("bg={color}"));
        }
        for (on, name) in [
            (self.bold, "bold"),
            (self.faint, "faint"),
            (self.italic, "italic"),
        ] {
            if on {
                parts.push(name.to_string());
            }
        }
        if self.double_underline {
            parts.push("double-underline".to_string());
        } else if self.underline {
            parts.push("underline".to_string());
        }
        for (on, name) in [
            (self.blink, "blink"),
            (self.inverse, "inverse"),
            (self.conceal, "conceal"),
            (self.strike, "strike"),
            (self.overline, "overline"),
        ] {
            if on {
                parts.push(name.to_string());
            }
        }
        if let Some(color) = &self.underline_color {
            parts.push(format!("ulcolor={color}"));
        }
        if link {
            parts.push("link".to_string());
        }
        parts.join(" ")
    }

    /// Applies the SGR parameters `body`; true when one of them is a reset
    /// (0 or empty), not counting the arguments of 38, 48 and 58.
    fn apply_sgr(&mut self, body: &str) -> bool {
        let mut reset = false;
        let parameters = body
            .split(';')
            .map(|parameter| parameter.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>();
        let mut idx = 0;
        while idx < parameters.len() {
            let code = parameters[idx];
            match code {
                0 => {
                    *self = Pen::default();
                    reset = true;
                }
                1 => self.bold = true,
                2 => self.faint = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 | 6 => self.blink = true,
                7 => self.inverse = true,
                8 => self.conceal = true,
                9 => self.strike = true,
                21 => self.double_underline = true,
                22 => {
                    self.bold = false;
                    self.faint = false;
                }
                23 => self.italic = false,
                24 => {
                    self.underline = false;
                    self.double_underline = false;
                }
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.conceal = false,
                29 => self.strike = false,
                30..=37 | 90..=97 => self.foreground = Some(code.to_string()),
                39 => self.foreground = None,
                40..=47 | 100..=107 => self.background = Some(code.to_string()),
                49 => self.background = None,
                53 => self.overline = true,
                55 => self.overline = false,
                59 => self.underline_color = None,
                38 | 48 | 58 => {
                    let (color, consumed) = match parameters.get(idx + 1) {
                        Some(5) => (parameters.get(idx + 2).map(|n| format!("{code};5;{n}")), 3),
                        Some(2) => (
                            parameters.get(idx + 4).map(|_| {
                                format!(
                                    "{code};2;{};{};{}",
                                    parameters[idx + 2],
                                    parameters[idx + 3],
                                    parameters[idx + 4]
                                )
                            }),
                            5,
                        ),
                        _ => (None, 2),
                    };
                    if let Some(color) = color {
                        match code {
                            38 => self.foreground = Some(color),
                            48 => self.background = Some(color),
                            _ => self.underline_color = Some(color),
                        }
                    }
                    idx += consumed;
                    continue;
                }
                _ => {}
            }
            idx += 1;
        }
        reset
    }
}

/// What a terminal shows after `output`.
pub struct Screen {
    cells: BTreeMap<(usize, usize), (char, String)>,
    /// Whether DECSCNM (reverse video for the whole screen) is set at the end.
    pub reverse_screen: bool,
}

impl Screen {
    pub fn read(output: &[u8], sgr0_closes_links: bool) -> Self {
        Self::read_as(output, sgr0_closes_links, false)
    }

    /// What xterm shows, keeping its SGR stack.
    pub fn read_with_sgr_stack(output: &[u8]) -> Self {
        Self::read_as(output, false, true)
    }

    fn read_as(output: &[u8], sgr0_closes_links: bool, sgr_stack: bool) -> Self {
        let mut stack: Vec<(Pen, Option<Vec<u32>>)> = Vec::new();
        let mut stack_used = 0usize;
        let text = String::from_utf8_lossy(output);
        let chars = text.chars().collect::<Vec<_>>();
        let mut cells = BTreeMap::new();
        let mut pen = Pen::default();
        let mut link = false;
        let mut reverse_screen = false;
        let (mut row, mut col) = (0usize, 0usize);
        let mut idx = 0;
        let run_c0 = |ch: char, row: &mut usize, col: &mut usize| match ch {
            '\r' => *col = 0,
            '\n' => *row += 1,
            '\x08' => *col = col.saturating_sub(1),
            _ => {}
        };
        while idx < chars.len() {
            let ch = chars[idx];
            if ch == '\x1b' {
                // C0 controls between ESC and the next byte are run in place.
                let mut next = idx + 1;
                while chars
                    .get(next)
                    .is_some_and(|c| (*c as u32) < 0x20 && !matches!(c, '\x18' | '\x1a' | '\x1b'))
                {
                    run_c0(chars[next], &mut row, &mut col);
                    next += 1;
                }
                match chars.get(next) {
                    Some('[') => {
                        let mut body = String::new();
                        let mut end = next + 1;
                        let mut final_byte = None;
                        while let Some(&c) = chars.get(end) {
                            end += 1;
                            if matches!(c, '\x18' | '\x1a') {
                                break;
                            }
                            if (c as u32) < 0x20 {
                                run_c0(c, &mut row, &mut col);
                                continue;
                            }
                            if ('\x40'..='\x7e').contains(&c) {
                                final_byte = Some(c);
                                break;
                            }
                            body.push(c);
                        }
                        match final_byte {
                            Some('m') if body.chars().all(|c| c.is_ascii_digit() || c == ';') => {
                                let resets = pen.apply_sgr(&body);
                                if sgr0_closes_links && resets {
                                    link = false;
                                }
                            }
                            Some('K') if body.is_empty() || body == "0" => {
                                let blank = Pen {
                                    background: pen.background.clone(),
                                    ..Pen::default()
                                };
                                for erased in col..SCREEN_WIDTH {
                                    cells.insert((row, erased), (' ', blank.signature(false)));
                                }
                            }
                            Some('h') if body == "?5" => reverse_screen = true,
                            Some('l') if body == "?5" => reverse_screen = false,
                            Some('p') if body == "!" => pen = Pen::default(),
                            Some('{' | 'p')
                                if sgr_stack
                                    && body.strip_suffix('#').is_some_and(|parameters| {
                                        parameters.chars().all(|c| c.is_ascii_digit() || c == ';')
                                    }) =>
                            {
                                if stack_used < SGR_STACK_DEPTH {
                                    stack
                                        .push((pen.clone(), push_mask(body.trim_end_matches('#'))));
                                }
                                stack_used += 1;
                            }
                            // A pop takes no parameter, and xterm pops
                            // whatever parameters it carries.
                            Some('}' | 'q')
                                if sgr_stack
                                    && stack_used > 0
                                    && body.strip_suffix('#').is_some_and(|parameters| {
                                        parameters.chars().all(|c| c.is_ascii_digit() || c == ';')
                                    }) =>
                            {
                                if stack_used <= SGR_STACK_DEPTH {
                                    if let Some((saved, mask)) = stack.pop() {
                                        restore_masked(&mut pen, &saved, &mask);
                                    }
                                }
                                stack_used -= 1;
                            }
                            _ => {}
                        }
                        idx = end;
                    }
                    Some(']') | Some('P') => {
                        let string_start = next + 1;
                        let mut end = string_start;
                        let mut payload = String::new();
                        while let Some(&c) = chars.get(end) {
                            if c == '\x07' {
                                end += 1;
                                break;
                            }
                            if c == '\x1b' && chars.get(end + 1) == Some(&'\\') {
                                end += 2;
                                break;
                            }
                            payload.push(c);
                            end += 1;
                        }
                        if chars[next] == ']' {
                            if let Some(rest) = payload.strip_prefix("8;") {
                                let uri = rest.split_once(';').map_or("", |(_, uri)| uri);
                                link = !uri.is_empty();
                            }
                        }
                        idx = end;
                    }
                    Some('c') => {
                        pen = Pen::default();
                        link = false;
                        reverse_screen = false;
                        idx = next + 1;
                    }
                    Some(_) => idx = next + 1,
                    None => idx = next,
                }
                continue;
            }
            if (ch as u32) < 0x20 || ch == '\x7f' {
                run_c0(ch, &mut row, &mut col);
                idx += 1;
                continue;
            }
            cells.insert((row, col), (ch, pen.signature(link)));
            col += 1;
            idx += 1;
        }
        Screen {
            cells,
            reverse_screen,
        }
    }

    /// Each row as runs of cells that share one rendition, `(row, text,
    /// rendition)`; a rendition is `""` for the default one.
    pub fn runs(&self) -> Vec<(usize, String, String)> {
        let mut runs: Vec<(usize, String, String)> = Vec::new();
        let mut last: Option<(usize, usize)> = None;
        for (&(row, col), (ch, signature)) in &self.cells {
            match runs.last_mut() {
                Some((run_row, text, run_signature))
                    if *run_row == row
                        && run_signature == signature
                        && last == Some((row, col.wrapping_sub(1))) =>
                {
                    text.push(*ch);
                }
                _ => runs.push((row, ch.to_string(), signature.clone())),
            }
            last = Some((row, col));
        }
        runs
    }

    /// The runs with the trailing blanks of each row dropped when they carry
    /// the default rendition, for compact expectations. Blanks between drawn
    /// cells stay.
    pub fn runs_trimmed(&self) -> Vec<(usize, String, String)> {
        let mut runs = self.runs();
        let mut idx = runs.len();
        while idx > 0 {
            idx -= 1;
            let (row, text, signature) = &runs[idx];
            let last_of_row = runs.get(idx + 1).is_none_or(|next| next.0 != *row);
            if last_of_row && signature.is_empty() && text.trim().is_empty() {
                runs.remove(idx);
            }
        }
        runs
    }
}

/// `runs` of `output` on an xterm-like terminal, each as `(row, text,
/// rendition)` with borrowed strings, for `assert_eq!`.
pub fn shown(output: &[u8]) -> Vec<(usize, String, String)> {
    Screen::read(output, false).runs_trimmed()
}

/// `runs` of `output` on xterm with its SGR stack.
pub fn shown_with_sgr_stack(output: &[u8]) -> Vec<(usize, String, String)> {
    Screen::read_with_sgr_stack(output).runs_trimmed()
}

/// Shorthand to write expected runs as `&[(row, "text", "rendition")]`.
pub fn runs(expected: &[(usize, &str, &str)]) -> Vec<(usize, String, String)> {
    expected
        .iter()
        .map(|(row, text, signature)| (*row, text.to_string(), signature.to_string()))
        .collect()
}
