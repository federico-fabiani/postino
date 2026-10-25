use crate::util::is_attr_char;

const BASE64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A body is "clean 7bit" when it is ASCII printable plus CRLF/tab, with lines under 999 bytes.
pub(crate) fn is_clean_7bit(text: &str) -> bool {
    if !text.is_ascii() {
        return false;
    }
    let mut line_len = 0usize;
    for byte in text.bytes() {
        match byte {
            b'\r' => line_len = 0,
            b'\n' => line_len = 0,
            b'\t' | 0x20..=0x7e => line_len = line_len.saturating_add(1),
            _ => return false,
        }
        if line_len > 998 {
            return false;
        }
    }
    true
}

/// Quoted-printable with CRLF line endings, 75-character body lines and soft breaks at `=`.
pub(crate) fn quoted_printable(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len().saturating_add(input.len() / 3));
    let mut line_len = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let Some(&byte) = bytes.get(index) else {
            break;
        };
        if byte == b'\r' {
            if bytes.get(index + 1) == Some(&b'\n') {
                out.push_str("\r\n");
                line_len = 0;
                index = index.saturating_add(2);
                continue;
            }
            out.push_str("=0D");
            line_len = line_len.saturating_add(3);
            index = index.saturating_add(1);
            continue;
        }
        if byte == b'\n' {
            out.push_str("\r\n");
            line_len = 0;
            index = index.saturating_add(1);
            continue;
        }
        let at_line_end = bytes.get(index + 1).is_none() || (bytes.get(index + 1) == Some(&b'\r') && bytes.get(index + 2) == Some(&b'\n'));
        let literal = (0x21..=0x7e).contains(&byte) && byte != b'=' || (byte == b' ' || byte == b'\t') && !at_line_end;
        let token_len = if literal { 1 } else { 3 };
        if line_len.saturating_add(token_len) > 75 {
            out.push_str("=\r\n");
            line_len = 0;
        }
        if literal {
            out.push(char::from(byte));
        } else {
            push_hex_byte(&mut out, byte);
        }
        line_len = line_len.saturating_add(token_len);
        index = index.saturating_add(1);
    }
    out
}

fn push_hex_byte(out: &mut String, byte: u8) {
    out.push('=');
    out.push(hex_digit(byte >> 4));
    out.push(hex_digit(byte & 0x0f));
}

fn hex_digit(value: u8) -> char {
    char::from(if value < 10 { b'0' + value } else { b'A' + (value - 10) })
}

/// Base64 with 76-character lines separated by CRLF. No trailing CRLF is added.
pub(crate) fn base64_lines(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().saturating_div(3).saturating_add(1).saturating_mul(78));
    let mut line_len = 0usize;
    for chunk in data.chunks(3) {
        let a = chunk.first().copied().unwrap_or(0);
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c);
        let encoded = [
            base64_digit((triple >> 18) & 0x3f),
            base64_digit((triple >> 12) & 0x3f),
            base64_digit((triple >> 6) & 0x3f),
            base64_digit(triple & 0x3f),
        ];
        let keep = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for (offset, digit) in encoded.iter().enumerate() {
            if line_len == 76 {
                out.push_str("\r\n");
                line_len = 0;
            }
            if offset < keep {
                out.push(char::from(*digit));
            } else {
                out.push('=');
            }
            line_len = line_len.saturating_add(1);
        }
    }
    out
}

/// One padded base64 word with no line breaks, for RFC 2047.
pub(crate) fn base64_word(data: &[u8]) -> String {
    base64_lines(data).replace("\r\n", "")
}

fn base64_digit(value: u32) -> u8 {
    BASE64_ALPHABET.get(value as usize).copied().unwrap_or(b'A')
}

/// RFC 2047 B-encoded words, each at most 75 characters, split on character boundaries.
pub(crate) fn encoded_words(text: &str) -> String {
    const MAX_BYTES: usize = 45;
    let mut out = String::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = start.saturating_add(MAX_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text
                .get(start..)
                .and_then(|rest| rest.chars().next())
                .map_or(text.len(), |character| start.saturating_add(character.len_utf8()).min(text.len()));
        }
        let chunk = text.get(start..end).unwrap_or_default();
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str("=?UTF-8?B?");
        out.push_str(&base64_word(chunk.as_bytes()));
        out.push_str("?=");
        start = end;
    }
    out
}

/// RFC 5987 percent-encoding with UTF-8, uppercase hex and only `attr-char` left bare.
pub(crate) fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if is_attr_char(*byte) {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_clean_7bit() {
        assert!(is_clean_7bit("Ciao\r\ncome stai?\r\n"));
        assert!(is_clean_7bit(""));
        assert!(!is_clean_7bit("Città"));
        assert!(!is_clean_7bit("nul\0"));
        assert!(!is_clean_7bit(&"x".repeat(999)));
        assert!(is_clean_7bit(&"x".repeat(998)));
    }

    #[test]
    fn encodes_quoted_printable() {
        assert_eq!(quoted_printable("Città"), "Citt=C3=A0");
        assert_eq!(quoted_printable("a=b"), "a=3Db");
        assert_eq!(quoted_printable("fine \r\nspazio "), "fine=20\r\nspazio=20");
        assert_eq!(quoted_printable("riga\r\n"), "riga\r\n");
        assert_eq!(quoted_printable("\0"), "=00");
    }

    #[test]
    fn quoted_printable_lines_stay_within_76() {
        let input = "à".repeat(200);
        let encoded = quoted_printable(&input);
        for line in encoded.split("\r\n") {
            assert!(line.len() <= 76, "{} chars: {line}", line.len());
        }
        assert!(encoded.contains("=\r\n"));
    }

    #[test]
    fn quoted_printable_round_trips_through_base64_helpers() {
        // Decoding is exercised end to end by the serializer round-trip test; here we only assert
        // that no literal non-ASCII byte leaked into the encoded form.
        let encoded = quoted_printable("Città è bella");
        assert!(encoded.is_ascii());
        assert!(encoded.contains("=C3=A8"));
    }

    #[test]
    fn encodes_base64_with_lines() {
        assert_eq!(base64_lines(b"abc"), "YWJj");
        assert_eq!(base64_lines(b"ab"), "YWI=");
        assert_eq!(base64_lines(b"a"), "YQ==");
        assert_eq!(base64_lines(b""), "");
        let encoded = base64_lines(&[0u8; 120]);
        let lines = encoded.split("\r\n").collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), 76);
        assert_eq!(lines[2].len(), 8);
        assert_eq!(base64_word(b"abc"), "YWJj");
    }

    #[test]
    fn encodes_rfc2047_words() {
        assert_eq!(encoded_words("Città"), "=?UTF-8?B?Q2l0dMOg?=");
        assert_eq!(encoded_words(""), "");
        let encoded = encoded_words("Un nome molto lungo con accenti à è ì ò ù e un finale lungo");
        let words = encoded.split(' ').collect::<Vec<_>>();
        assert!(words.len() > 1);
        for word in words {
            assert!(word.starts_with("=?UTF-8?B?") && word.ends_with("?="));
            assert!(word.len() <= 75, "{}: {word}", word.len());
        }
    }

    #[test]
    fn percent_encodes_rfc5987() {
        assert_eq!(percent_encode("Relazione finale €.pdf"), "Relazione%20finale%20%E2%82%AC.pdf");
        assert_eq!(percent_encode("simple-name_1.pdf"), "simple-name_1.pdf");
    }
}
