/// Converts untrusted metadata into printable terminal text.
pub(crate) fn escape_untrusted(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character
                if is_bidirectional_control(character) || is_unicode_line_separator(character) =>
            {
                escaped.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character if character.is_control() && (character as u32) <= 0xff => {
                escaped.push_str(&format!("\\x{:02x}", character as u32));
            }
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn is_unicode_line_separator(character: char) -> bool {
    matches!(character, '\u{2028}' | '\u{2029}')
}

fn is_bidirectional_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_terminal_and_bidirectional_controls_without_changing_unicode_text() {
        assert_eq!(
            super::escape_untrusted("router\u{1b}]0;spoof\u{7}\n\u{2028}\u{2029}\u{202e}é"),
            "router\\x1b]0;spoof\\x07\\n\\u{2028}\\u{2029}\\u{202e}é"
        );
    }
}
