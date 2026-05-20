//! Style parser and ANSI color rendering utilities.

use std::collections::BTreeMap;

/// Terminal style attributes applied to highlighted spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// Optional foreground color.
    pub foreground: Option<Rgb>,
    /// Optional background color.
    pub background: Option<Rgb>,
    /// Emit bold SGR when true.
    pub bold: bool,
    /// Emit blink SGR when true.
    pub blink: bool,
    /// Emit inverse-video SGR when true.
    pub invert: bool,
    /// Emit italic SGR when true.
    pub italic: bool,
    /// Emit strikethrough SGR when true.
    pub strike: bool,
    /// Emit underline SGR when true.
    pub underline: bool,
}

/// RGB color value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

/// ANSI color depth used when emitting styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// Approximate RGB colors to the 16-color ANSI palette.
    Ansi16,
    /// Emit true-color `38;2` and `48;2` sequences.
    TrueColor,
    /// Approximate RGB colors to the xterm 256-color palette.
    Xterm256,
}

impl Style {
    /// Parses a style specification without a named palette.
    pub fn parse(spec: &str) -> Result<Self, String> {
        Self::parse_with_palette(spec, None)
    }

    /// Parses a style specification with optional named palette colors.
    pub fn parse_with_palette(
        spec: &str,
        palette: Option<&BTreeMap<String, Rgb>>,
    ) -> Result<Self, String> {
        let mut style = Self::default();

        for token in spec.split_whitespace() {
            let token = token.to_ascii_lowercase();
            match token.as_str() {
                "bold" => style.bold = true,
                "blink" => style.blink = true,
                "invert" => style.invert = true,
                "italic" => style.italic = true,
                "strike" => style.strike = true,
                "underline" => style.underline = true,
                _ if token.starts_with("f#") => {
                    set_color(&mut style.foreground, parse_rgb(&token[2..])?, "foreground")?;
                }
                _ if token.starts_with("b#") => {
                    set_color(&mut style.background, parse_rgb(&token[2..])?, "background")?;
                }
                _ if token.starts_with("f.") => {
                    let color = resolve_palette_color(palette, &token[2..])?;
                    set_color(&mut style.foreground, color, "foreground")?;
                }
                _ if token.starts_with("b.") => {
                    let color = resolve_palette_color(palette, &token[2..])?;
                    set_color(&mut style.background, color, "background")?;
                }
                _ => return Err(format!("unsupported style token '{token}'")),
            }
        }

        if style.is_empty() {
            return Err("style must set at least one attribute".to_string());
        }

        Ok(style)
    }

    /// Returns true when no color or text attribute is set.
    pub fn is_empty(&self) -> bool {
        self.foreground.is_none()
            && self.background.is_none()
            && !self.bold
            && !self.blink
            && !self.invert
            && !self.italic
            && !self.strike
            && !self.underline
    }

    /// Merges another style, replacing colors and OR-ing text attributes.
    pub fn merge_from(&mut self, other: &Self) {
        if other.foreground.is_some() {
            self.foreground = other.foreground;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
        self.bold |= other.bold;
        self.blink |= other.blink;
        self.invert |= other.invert;
        self.italic |= other.italic;
        self.strike |= other.strike;
        self.underline |= other.underline;
    }

    /// Emits a true-color ANSI SGR start sequence for this style.
    pub fn ansi_start(&self) -> String {
        self.ansi_start_with_mode(ColorMode::TrueColor)
    }

    /// Emits an ANSI SGR start sequence using the requested color mode.
    pub fn ansi_start_with_mode(&self, mode: ColorMode) -> String {
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
        if let Some(color) = self.foreground {
            parts.push(color.ansi_fg(mode));
        }
        if let Some(color) = self.background {
            parts.push(color.ansi_bg(mode));
        }
        format!("\x1b[{}m", parts.join(";"))
    }
}

impl Rgb {
    fn ansi_fg(self, mode: ColorMode) -> String {
        match mode {
            ColorMode::Ansi16 => ansi16_code(self, false).to_string(),
            ColorMode::TrueColor => format!("38;2;{};{};{}", self.r, self.g, self.b),
            ColorMode::Xterm256 => format!("38;5;{}", rgb_to_xterm256(self)),
        }
    }

    fn ansi_bg(self, mode: ColorMode) -> String {
        match mode {
            ColorMode::Ansi16 => ansi16_code(self, true).to_string(),
            ColorMode::TrueColor => format!("48;2;{};{};{}", self.r, self.g, self.b),
            ColorMode::Xterm256 => format!("48;5;{}", rgb_to_xterm256(self)),
        }
    }
}

fn ansi16_code(color: Rgb, background: bool) -> u8 {
    let fg_code = ansi16_fg_code(color);
    if background {
        match fg_code {
            30..=37 => fg_code + 10,
            90..=97 => fg_code + 10,
            _ => fg_code,
        }
    } else {
        fg_code
    }
}

fn ansi16_fg_code(color: Rgb) -> u8 {
    let max = color.r.max(color.g).max(color.b);
    let min = color.r.min(color.g).min(color.b);

    if max < 48 {
        return 30;
    }
    if max - min < 32 {
        return if max < 160 { 90 } else { 97 };
    }
    if color.r >= 220 && color.g >= 220 && color.b < 120 {
        return 93;
    }
    if color.r >= 200 && color.g >= 100 && color.b < 120 {
        return 33;
    }
    if color.r >= 180 && color.b >= 150 && color.g < 200 {
        return 95;
    }
    if color.g >= 180 && color.b >= 180 && color.r < 140 {
        return 96;
    }
    if color.b >= 160 && color.r < 140 {
        return 94;
    }
    if color.g >= 160 && color.r < 180 {
        return 92;
    }
    if color.r >= 160 && color.g < 180 {
        return 91;
    }
    if color.r >= 140 && color.b >= 120 {
        return 35;
    }
    if color.g >= 120 && color.b >= 120 {
        return 36;
    }
    if color.r >= 120 && color.g >= 80 {
        return 33;
    }
    if color.b >= 120 {
        return 34;
    }
    if color.g >= 120 {
        return 32;
    }
    if color.r >= 120 {
        return 31;
    }
    37
}

/// Parses a named color palette from `#rrggbb` string values.
pub fn parse_palette(input: &BTreeMap<String, String>) -> Result<BTreeMap<String, Rgb>, String> {
    let mut palette = BTreeMap::new();
    for (name, value) in input {
        let name = name.to_ascii_lowercase();
        if !is_palette_name(&name) {
            return Err(format!(
                "palette color name '{name}' must contain only alphanumerics, dashes, and underscores"
            ));
        }
        if matches!(
            name.as_str(),
            "fg" | "bg" | "blink" | "bold" | "invert" | "italic" | "strike" | "underline"
        ) {
            return Err(format!("palette color name '{name}' is reserved"));
        }
        if palette.contains_key(&name) {
            return Err(format!("palette color '{name}' is duplicated"));
        }
        let hex = value
            .trim()
            .strip_prefix('#')
            .ok_or_else(|| format!("palette color '{name}' must be in #123abc format"))?;
        palette.insert(name, parse_rgb(hex)?);
    }
    Ok(palette)
}

fn set_color(slot: &mut Option<Rgb>, color: Rgb, target: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("style accepts only one {target} color"));
    }
    *slot = Some(color);
    Ok(())
}

fn resolve_palette_color(
    palette: Option<&BTreeMap<String, Rgb>>,
    name: &str,
) -> Result<Rgb, String> {
    let palette = palette
        .ok_or_else(|| format!("palette color '{name}' used, but no palette was specified"))?;
    palette
        .get(name)
        .copied()
        .ok_or_else(|| format!("palette color '{name}' not found"))
}

fn is_palette_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn parse_rgb(hex: &str) -> Result<Rgb, String> {
    if hex.len() != 6 {
        return Err(format!("expected 6 hex digits in '#{hex}'"));
    }
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid hex color '#{hex}'"));
    }
    let r = parse_channel(&hex[0..2])?;
    let g = parse_channel(&hex[2..4])?;
    let b = parse_channel(&hex[4..6])?;
    Ok(Rgb { r, g, b })
}

fn parse_channel(hex: &str) -> Result<u8, String> {
    u8::from_str_radix(hex, 16).map_err(|_| format!("invalid hex color channel '{hex}'"))
}

fn rgb_to_xterm256(color: Rgb) -> u8 {
    const RGB_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    const GRAY_STEPS: [u8; 24] = [
        8, 18, 28, 38, 48, 58, 68, 78, 88, 98, 108, 118, 128, 138, 148, 158, 168, 178, 188, 198,
        208, 218, 228, 238,
    ];

    fn nearest(value: u8, steps: &[u8]) -> usize {
        let Some((first, rest)) = steps.split_first() else {
            return 0;
        };
        let mut nearest_idx = 0;
        let mut nearest_diff = value.abs_diff(*first);
        for (idx, step) in rest.iter().enumerate() {
            let diff = value.abs_diff(*step);
            if diff < nearest_diff {
                nearest_idx = idx + 1;
                nearest_diff = diff;
            }
        }
        nearest_idx
    }

    fn distance(a: Rgb, b: Rgb) -> u32 {
        let r = i32::from(a.r) - i32::from(b.r);
        let g = i32::from(a.g) - i32::from(b.g);
        let b = i32::from(a.b) - i32::from(b.b);
        (r * r + g * g + b * b) as u32
    }

    let rgb_index = [
        nearest(color.r, &RGB_STEPS),
        nearest(color.g, &RGB_STEPS),
        nearest(color.b, &RGB_STEPS),
    ];
    let rgb_color = Rgb {
        r: RGB_STEPS[rgb_index[0]],
        g: RGB_STEPS[rgb_index[1]],
        b: RGB_STEPS[rgb_index[2]],
    };

    let gray_index = nearest(
        ((u16::from(color.r) + u16::from(color.g) + u16::from(color.b)) / 3) as u8,
        &GRAY_STEPS,
    );
    let gray = GRAY_STEPS[gray_index];
    let gray_color = Rgb {
        r: gray,
        g: gray,
        b: gray,
    };

    if distance(color, gray_color) < distance(color, rgb_color) {
        return 232 + gray_index as u8;
    }

    16 + (36 * rgb_index[0] as u8) + (6 * rgb_index[1] as u8) + rgb_index[2] as u8
}

#[cfg(test)]
mod tests {
    use super::Style;

    #[test]
    fn xterm256_gray_steps_do_not_allocate_on_each_lookup() {
        let source = include_str!("style.rs");
        let function_source = source
            .split("fn rgb_to_xterm256")
            .nth(1)
            .expect("function exists");

        assert!(function_source.contains("const GRAY_STEPS"));
        assert!(!function_source.contains("collect::<Vec"));
    }

    #[test]
    fn malformed_non_ascii_rgb_color_returns_error_without_panic() {
        let result = std::panic::catch_unwind(|| Style::parse("f#€€"));

        assert!(result.is_ok(), "malformed local style should not panic");
        assert!(result.expect("parse completed").is_err());
    }
}
