//! A small terminal cell model for tests that judge output by what a
//! terminal shows: the text in each cell and the colors it is drawn in.
//!
//! It covers what the cursor-move tests send: printable UTF-8 (wide and
//! combining characters), CR, LF, BS, HT, `ESC 7`, `ESC 8`, `ESC M`, RIS,
//! and CSI `A B C D E F G H f d J K r s u m`, DEC private modes 7 (autowrap),
//! 45 and 1045 (reverse wraparound, as xterm reads them) and 47, 1047 and 1049
//! (alternate screen). Erases fill with the current background. Anything else
//! is parsed and ignored, as a terminal ignores what it does not know.
//!
//! It is a focused model, not a general terminal oracle: one pair of scroll
//! margins serves both screens, left and right margins (mode 69) and
//! XTSAVE/XTRESTORE are not followed, and a blank cell takes only the
//! background, never inverse or underline. The tests that turn on those
//! judge by bytes: the read must come out as released 1.4.1 writes it.
#![allow(dead_code)]

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            attrs: Attrs::default(),
        }
    }
}

#[derive(Clone, Debug)]
struct Row {
    cells: Vec<Cell>,
    wrapped: bool,
}

impl Row {
    fn new(cols: usize, attrs: Attrs) -> Self {
        Self {
            cells: vec![
                Cell {
                    ch: ' ',
                    attrs: Attrs {
                        bg: attrs.bg,
                        ..Attrs::default()
                    },
                };
                cols
            ],
            wrapped: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Screen {
    rows: usize,
    cols: usize,
    main: Vec<Row>,
    alt: Vec<Row>,
    on_alt: bool,
    x: usize,
    y: usize,
    pending_wrap: bool,
    attrs: Attrs,
    top: usize,
    bottom: usize,
    autowrap: bool,
    reverse_wrap: bool,
    reverse_wrap_any: bool,
    saved: Option<(usize, usize, Attrs)>,
}

impl Screen {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            main: (0..rows)
                .map(|_| Row::new(cols, Attrs::default()))
                .collect(),
            alt: (0..rows)
                .map(|_| Row::new(cols, Attrs::default()))
                .collect(),
            on_alt: false,
            x: 0,
            y: 0,
            pending_wrap: false,
            attrs: Attrs::default(),
            top: 0,
            bottom: rows - 1,
            autowrap: true,
            reverse_wrap: false,
            reverse_wrap_any: false,
            saved: None,
        }
    }

    /// A terminal that was already in xterm's reverse wraparound mode 1045
    /// before the program started.
    pub fn with_reverse_wrap_any(mut self) -> Self {
        self.reverse_wrap_any = true;
        self
    }

    pub fn rendered(rows: usize, cols: usize, bytes: &[u8]) -> Self {
        let mut screen = Self::new(rows, cols);
        screen.feed(bytes);
        screen
    }

    fn buf(&mut self) -> &mut Vec<Row> {
        if self.on_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    fn view(&self) -> &Vec<Row> {
        if self.on_alt { &self.alt } else { &self.main }
    }

    /// The text of a row, without trailing blanks.
    pub fn text(&self, row: usize) -> String {
        let text: String = self.view()[row].cells.iter().map(|cell| cell.ch).collect();
        text.trim_end().to_string()
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.view()[row].cells[col]
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// The columns of a row whose foreground is `color`.
    pub fn columns_in(&self, row: usize, color: Color) -> Vec<usize> {
        (0..self.cols)
            .filter(|col| self.cell(row, *col).attrs.fg == color)
            .collect()
    }

    /// Every cell that differs in attributes from `other`, as (row, column).
    pub fn attribute_differences(&self, other: &Screen) -> Vec<(usize, usize)> {
        let mut differences = Vec::new();
        for row in 0..self.rows {
            for col in 0..self.cols {
                if self.cell(row, col).attrs != other.cell(row, col).attrs {
                    differences.push((row, col));
                }
            }
        }
        differences
    }

    pub fn same_text(&self, other: &Screen) -> bool {
        (0..self.rows).all(|row| self.text(row) == other.text(row))
    }

    fn blank_attrs(&self) -> Attrs {
        Attrs {
            bg: self.attrs.bg,
            ..Attrs::default()
        }
    }

    fn erase(&mut self, row: usize, from: usize, to: usize) {
        let blank = Cell {
            ch: ' ',
            attrs: self.blank_attrs(),
        };
        let cols = self.cols;
        for col in from.min(cols)..to.min(cols) {
            self.buf()[row].cells[col] = blank;
        }
    }

    fn scroll_up(&mut self) {
        let (top, bottom, cols, attrs) = (self.top, self.bottom, self.cols, self.attrs);
        let buf = self.buf();
        buf.remove(top);
        buf.insert(bottom, Row::new(cols, attrs));
    }

    fn scroll_down(&mut self) {
        let (top, bottom, cols, attrs) = (self.top, self.bottom, self.cols, self.attrs);
        let buf = self.buf();
        buf.remove(bottom);
        buf.insert(top, Row::new(cols, attrs));
    }

    fn line_feed(&mut self) {
        if self.y == self.bottom {
            self.scroll_up();
        } else if self.y + 1 < self.rows {
            self.y += 1;
        }
        self.pending_wrap = false;
    }

    fn reverse_index(&mut self) {
        if self.y == self.top {
            self.scroll_down();
        } else if self.y > 0 {
            self.y -= 1;
        }
        self.pending_wrap = false;
    }

    fn back(&mut self) {
        if self.x > 0 {
            self.x -= 1;
        } else if self.y > 0 {
            let above_wrapped = self.view()[self.y - 1].wrapped;
            if self.reverse_wrap_any || (self.reverse_wrap && above_wrapped) {
                self.y -= 1;
                self.x = self.cols - 1;
            }
        }
        self.pending_wrap = false;
    }

    fn put(&mut self, ch: char) {
        let width = char_width(ch);
        if width == 0 {
            return;
        }
        if self.pending_wrap && self.autowrap {
            let y = self.y;
            self.buf()[y].wrapped = true;
            self.x = 0;
            self.line_feed();
        }
        if width == 2 && self.x + 1 >= self.cols {
            if !self.autowrap {
                return;
            }
            let y = self.y;
            self.buf()[y].wrapped = true;
            self.x = 0;
            self.line_feed();
        }
        let (x, y, attrs) = (self.x, self.y, self.attrs);
        self.buf()[y].cells[x] = Cell { ch, attrs };
        if width == 2 {
            self.buf()[y].cells[x + 1] = Cell { ch: '\0', attrs };
        }
        if self.x + width >= self.cols {
            self.x = self.cols - 1;
            self.pending_wrap = self.autowrap;
        } else {
            self.x += width;
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            i += 1;
            match ch {
                '\x1b' => i = self.escape(&chars, i),
                '\r' => {
                    self.x = 0;
                    self.pending_wrap = false;
                }
                '\n' | '\x0b' | '\x0c' => self.line_feed(),
                '\x08' => self.back(),
                '\t' => {
                    self.x = ((self.x / 8 + 1) * 8).min(self.cols - 1);
                    self.pending_wrap = false;
                }
                ch if (ch as u32) < 0x20 || ch == '\x7f' => {}
                ch if (0x80..0xa0).contains(&(ch as u32)) => {}
                ch => self.put(ch),
            }
        }
    }

    fn escape(&mut self, chars: &[char], mut i: usize) -> usize {
        let Some(&next) = chars.get(i) else {
            return i;
        };
        i += 1;
        match next {
            '[' => self.csi(chars, i),
            ']' | 'P' | 'X' | '^' | '_' => {
                while i < chars.len() {
                    match chars[i] {
                        '\x07' => return i + 1,
                        '\x1b' => {
                            return if chars.get(i + 1) == Some(&'\\') {
                                i + 2
                            } else {
                                i
                            };
                        }
                        _ => i += 1,
                    }
                }
                i
            }
            '7' => {
                self.saved = Some((self.x, self.y, self.attrs));
                i
            }
            '8' => {
                let (x, y, attrs) = self.saved.unwrap_or((0, 0, Attrs::default()));
                (self.x, self.y, self.attrs) = (x, y, attrs);
                self.pending_wrap = false;
                i
            }
            'M' => {
                self.reverse_index();
                i
            }
            'D' => {
                self.line_feed();
                i
            }
            'E' => {
                self.x = 0;
                self.line_feed();
                i
            }
            'c' => {
                *self = Self::new(self.rows, self.cols);
                i
            }
            // An escape with intermediates, such as `ESC ( B`, `ESC # 6` or
            // `ESC % G`: the intermediates, then one final byte.
            '\x20'..='\x2f' => {
                while chars
                    .get(i)
                    .is_some_and(|ch| ('\x20'..='\x2f').contains(ch))
                {
                    i += 1;
                }
                (i + 1).min(chars.len())
            }
            _ => i,
        }
    }

    fn csi(&mut self, chars: &[char], mut i: usize) -> usize {
        let private = chars
            .get(i)
            .copied()
            .filter(|marker| matches!(marker, '?' | '>' | '<' | '='));
        if private.is_some() {
            i += 1;
        }
        let mut params = String::new();
        let mut intermediate = false;
        while i < chars.len() {
            let ch = chars[i];
            i += 1;
            match ch {
                '0'..='9' | ';' | ':' => params.push(ch),
                '\x20'..='\x2f' => intermediate = true,
                '\x40'..='\x7e' => {
                    if !intermediate {
                        self.dispatch(private, &params, ch);
                    }
                    return i;
                }
                '\x18' | '\x1a' => return i,
                '\x1b' => return i - 1,
                _ => {}
            }
        }
        i
    }

    fn dispatch(&mut self, private: Option<char>, params: &str, final_char: char) {
        let numbers: Vec<usize> = params
            .split(';')
            .take(30)
            .map(|p| {
                p.split(':')
                    .next()
                    .unwrap_or("")
                    .parse()
                    .unwrap_or(0)
                    .min(65_535)
            })
            .collect();
        let n = |index: usize| numbers.get(index).copied().filter(|v| *v > 0).unwrap_or(1);
        if private == Some('?') {
            if matches!(final_char, 'h' | 'l') {
                let on = final_char == 'h';
                for mode in &numbers {
                    match mode {
                        7 => self.autowrap = on,
                        45 => self.reverse_wrap = on,
                        1045 => self.reverse_wrap_any = on,
                        47 | 1047 | 1049 if on != self.on_alt => {
                            self.on_alt = on;
                            if on {
                                self.alt = (0..self.rows)
                                    .map(|_| Row::new(self.cols, Attrs::default()))
                                    .collect();
                            }
                        }
                        _ => {}
                    }
                }
            }
            return;
        }
        if private.is_some() {
            return;
        }
        if final_char != 'm' {
            self.pending_wrap = false;
        }
        let up_limit = if self.y >= self.top { self.top } else { 0 };
        let down_limit = if self.y <= self.bottom {
            self.bottom
        } else {
            self.rows - 1
        };
        match final_char {
            'A' => self.y = self.y.saturating_sub(n(0)).max(up_limit),
            'B' => self.y = (self.y + n(0)).min(down_limit),
            'C' => self.x = (self.x + n(0)).min(self.cols - 1),
            'D' => {
                for _ in 0..n(0) {
                    self.back();
                }
            }
            'E' => {
                self.y = (self.y + n(0)).min(down_limit);
                self.x = 0;
            }
            'F' => {
                self.y = self.y.saturating_sub(n(0)).max(up_limit);
                self.x = 0;
            }
            'G' => self.x = (n(0) - 1).min(self.cols - 1),
            'H' | 'f' => {
                self.y = (n(0) - 1).min(self.rows - 1);
                self.x = (n(1) - 1).min(self.cols - 1);
            }
            'd' => self.y = (n(0) - 1).min(self.rows - 1),
            'J' => {
                let y = self.y;
                match numbers.first().copied().unwrap_or(0) {
                    0 => {
                        self.erase(y, self.x, self.cols);
                        for row in y + 1..self.rows {
                            self.erase(row, 0, self.cols);
                        }
                    }
                    1 => {
                        for row in 0..y {
                            self.erase(row, 0, self.cols);
                        }
                        self.erase(y, 0, self.x + 1);
                    }
                    2 => {
                        for row in 0..self.rows {
                            self.erase(row, 0, self.cols);
                        }
                    }
                    _ => {}
                }
            }
            'K' => {
                let y = self.y;
                match numbers.first().copied().unwrap_or(0) {
                    0 => self.erase(y, self.x, self.cols),
                    1 => self.erase(y, 0, self.x + 1),
                    2 => self.erase(y, 0, self.cols),
                    _ => {}
                }
            }
            'r' => {
                let top = n(0) - 1;
                let bottom = numbers
                    .get(1)
                    .copied()
                    .filter(|v| *v > 0)
                    .unwrap_or(self.rows)
                    .min(self.rows)
                    - 1;
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.x = 0;
                    self.y = 0;
                }
            }
            's' if params.is_empty() => self.saved = Some((self.x, self.y, self.attrs)),
            'u' => {
                let (x, y, attrs) = self.saved.unwrap_or((0, 0, Attrs::default()));
                (self.x, self.y, self.attrs) = (x, y, attrs);
            }
            'm' => self.sgr(params),
            _ => {}
        }
    }

    fn sgr(&mut self, params: &str) {
        let values: Vec<u16> = if params.is_empty() {
            vec![0]
        } else {
            params
                .split(';')
                .map(|p| p.split(':').next().unwrap_or("").parse().unwrap_or(0))
                .collect()
        };
        let mut i = 0;
        while i < values.len() {
            let v = values[i];
            match v {
                0 => self.attrs = Attrs::default(),
                1 => self.attrs.bold = true,
                4 => self.attrs.underline = true,
                7 => self.attrs.reverse = true,
                22 => self.attrs.bold = false,
                24 => self.attrs.underline = false,
                27 => self.attrs.reverse = false,
                30..=37 => self.attrs.fg = Color::Indexed((v - 30) as u8),
                39 => self.attrs.fg = Color::Default,
                40..=47 => self.attrs.bg = Color::Indexed((v - 40) as u8),
                49 => self.attrs.bg = Color::Default,
                90..=97 => self.attrs.fg = Color::Indexed((v - 82) as u8),
                100..=107 => self.attrs.bg = Color::Indexed((v - 92) as u8),
                38 | 48 => {
                    let color = match values.get(i + 1) {
                        Some(5) => {
                            let color = Color::Indexed(*values.get(i + 2).unwrap_or(&0) as u8);
                            i += 2;
                            color
                        }
                        Some(2) => {
                            let part = |k: usize| *values.get(i + k).unwrap_or(&0) as u8;
                            let color = Color::Rgb(part(2), part(3), part(4));
                            i += 4;
                            color
                        }
                        _ => Color::Default,
                    };
                    if v == 38 {
                        self.attrs.fg = color;
                    } else {
                        self.attrs.bg = color;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn char_width(ch: char) -> usize {
    let code = ch as u32;
    if (0x0300..=0x036f).contains(&code) || ch == '\0' {
        return 0;
    }
    let wide = (0x1100..=0x115f).contains(&code)
        || (0x2e80..=0xa4cf).contains(&code)
        || (0xac00..=0xd7a3).contains(&code)
        || (0xf900..=0xfaff).contains(&code)
        || (0xfe30..=0xfe4f).contains(&code)
        || (0xff00..=0xff60).contains(&code)
        || (0xffe0..=0xffe6).contains(&code)
        || (0x1f300..=0x1faff).contains(&code)
        || (0x20000..=0x3fffd).contains(&code);
    if wide { 2 } else { 1 }
}
