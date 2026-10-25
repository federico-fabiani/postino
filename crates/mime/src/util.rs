/// Normalise every line ending to CRLF without changing anything else.
pub(crate) fn normalize_crlf(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            other => out.push(other),
        }
    }
    out
}

/// The longest prefix of `input` that fits in `max_bytes` and ends on a character boundary.
pub(crate) fn truncate_chars(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input.get(..end).unwrap_or("")
}

/// Replace every control character with a space and trim the edges. Header text is shown in a UI
/// and written to logs; a CR or NUL must never survive that trip.
pub(crate) fn strip_controls(input: &str) -> String {
    input.chars().map(|character| if character.is_control() { ' ' } else { character }).collect::<String>().trim().to_owned()
}

/// ASCII case-insensitive substring search, used only for fixed needles in bounded input.
pub(crate) fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

/// Lowercase hexadecimal, no separators.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        if let Some(high) = HEX.get(usize::from(byte >> 4)) {
            out.push(char::from(*high));
        }
        if let Some(low) = HEX.get(usize::from(byte & 0x0f)) {
            out.push(char::from(*low));
        }
    }
    out
}

/// A MIME token character (`type`, `subtype`, parameter names).
pub(crate) fn is_token_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(character)
}

/// RFC 5987 `attr-char`, safe unescaped in a `filename*` value.
pub(crate) fn is_attr_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte)
}

/// Unfold a raw header value and decode it lossily. Only a fallback for headers the parser could
/// not decode; it must never return controls.
pub(crate) fn unfold_lossy(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut unfolded = String::with_capacity(text.len());
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with([' ', '\t']) {
            if !unfolded.ends_with(' ') {
                unfolded.push(' ');
            }
            unfolded.push_str(line.trim_start());
        } else {
            if !unfolded.is_empty() && !unfolded.ends_with(' ') {
                unfolded.push(' ');
            }
            unfolded.push_str(line);
        }
    }
    strip_controls(&unfolded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_all_line_endings() {
        assert_eq!(normalize_crlf("a\nb\r\nc\rd"), "a\r\nb\r\nc\r\nd");
        assert_eq!(normalize_crlf(""), "");
    }

    #[test]
    fn truncates_on_a_character_boundary() {
        assert_eq!(truncate_chars("città", 5), "citt");
        assert_eq!(truncate_chars("città", 6), "città");
        assert_eq!(truncate_chars("città", 0), "");
        assert_eq!(truncate_chars("città", 100), "città");
    }

    #[test]
    fn strips_control_characters() {
        assert_eq!(strip_controls("a\r\nb\0c"), "a  b c");
        assert_eq!(strip_controls("  plain  "), "plain");
    }

    #[test]
    fn finds_ascii_case_insensitively() {
        assert!(contains_ci(b"Content-Type: Multipart/Mixed", b"content-type:"));
        assert!(contains_ci(b"X", b"x"));
        assert!(!contains_ci(b"X", b"xy"));
        assert!(!contains_ci(b"", b"x"));
    }

    #[test]
    fn hex_is_lowercase_and_short() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    #[test]
    fn unfolds_folded_header_bytes() {
        assert_eq!(unfold_lossy(b"one\r\n two\r\n\tthree"), "one two three");
    }
}
