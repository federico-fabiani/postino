use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;

use crate::NetError;
use crate::error::protocol;

/// Longest accepted mailbox name, in wire bytes.
pub(crate) const MAX_NAME_BYTES: usize = 1024;

/// Decode a mailbox name from IMAP's modified UTF-7 (RFC 3501 §5.1.3).
///
/// Returns `None` when the name is not valid modified UTF-7: an unterminated `&` run, a base64
/// payload that does not decode, or a run that is not whole UTF-16 code units. The caller keeps the
/// wire form in that case, so nothing is lost and nothing is guessed.
#[must_use]
pub fn decode_utf7(name: &str) -> Option<String> {
    if name.len() > MAX_NAME_BYTES {
        return None;
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(amp) = rest.find('&') {
        let (plain, tail) = rest.split_at_checked(amp)?;
        out.push_str(plain);
        let after = tail.get(1..)?;
        if let Some(literal) = after.strip_prefix('-') {
            out.push('&');
            rest = literal;
            continue;
        }
        let end = after.find('-')?;
        let encoded = after.get(..end)?;
        let decoded = decode_base64_run(encoded)?;
        out.push_str(&decoded);
        rest = after.get(end + 1..)?;
        if out.len() > MAX_NAME_BYTES * 4 {
            return None;
        }
    }
    out.push_str(rest);
    Some(out)
}

/// Encode a mailbox name into IMAP's modified UTF-7.
///
/// Printable US-ASCII except `&` is copied; `&` becomes `&-`; every other character is encoded as
/// UTF-16BE in modified base64 between `&` and `-`.
#[must_use]
pub fn encode_utf7(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();
    for ch in name.chars() {
        if ch == '&' {
            flush_base64(&mut pending, &mut out);
            out.push_str("&-");
        } else if ch.is_ascii() && !ch.is_ascii_control() {
            flush_base64(&mut pending, &mut out);
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            pending.extend_from_slice(ch.encode_utf16(&mut units));
        }
    }
    flush_base64(&mut pending, &mut out);
    out
}

fn flush_base64(pending: &mut Vec<u16>, out: &mut String) {
    if pending.is_empty() {
        return;
    }
    let mut bytes = Vec::with_capacity(pending.len() * 2);
    for unit in pending.drain(..) {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    let encoded = STANDARD_NO_PAD.encode(&bytes);
    out.push('&');
    for ch in encoded.chars() {
        out.push(if ch == '/' { ',' } else { ch });
    }
    out.push('-');
}

fn decode_base64_run(encoded: &str) -> Option<String> {
    if encoded.is_empty() || encoded.len() % 4 == 1 || encoded.len() > MAX_NAME_BYTES {
        return None;
    }
    let mut standard = String::with_capacity(encoded.len());
    for ch in encoded.chars() {
        standard.push(if ch == ',' { '/' } else { ch });
    }
    let bytes = STANDARD_NO_PAD.decode(standard.as_bytes()).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let (pairs, _) = bytes.as_chunks::<2>();
    for pair in pairs {
        units.push(u16::from_be_bytes(*pair));
    }
    String::from_utf16(&units).ok()
}

/// Quote a value as an IMAP `astring`, refusing anything that could split the command.
///
/// Values are always quoted: a mailbox called `INBOX` and one called `IN BOX` then travel the same
/// path, and no name can be mistaken for a command token. `what` names the value in the error
/// (`mailbox`, `username`, `password`).
pub(crate) fn astring(value: &str, what: &'static str) -> Result<String, NetError> {
    if value.len() > MAX_NAME_BYTES {
        return Err(NetError::TooLarge { what, limit: MAX_NAME_BYTES });
    }
    if value.bytes().any(|byte| byte == 0 || byte == b'\r' || byte == b'\n') {
        return Err(protocol(format!("{what} contains a CR, LF or NUL byte")));
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_rfc3501_examples() {
        assert_eq!(decode_utf7("~peter/mail/&U,BTFw-/&ZeVnLIqe-").as_deref(), Some("~peter/mail/台北/日本語"));
        assert_eq!(decode_utf7("&Jjo-").as_deref(), Some("☺"));
        assert_eq!(decode_utf7("&-").as_deref(), Some("&"));
        assert_eq!(decode_utf7("INBOX").as_deref(), Some("INBOX"));
        assert_eq!(decode_utf7("").as_deref(), Some(""));
    }

    #[test]
    fn round_trips_non_ascii_names() {
        for name in ["Posta inviata", "Ångström", "日本語", "a&b", "☺ ☺", "Ünïcödé/Sub"] {
            let wire = encode_utf7(name);
            assert!(wire.is_ascii(), "{wire}");
            assert_eq!(decode_utf7(&wire).as_deref(), Some(name), "{wire}");
        }
    }

    #[test]
    fn rejects_malformed_runs_instead_of_guessing() {
        assert_eq!(decode_utf7("&Jjo"), None);
        assert_eq!(decode_utf7("&!!-"), None);
        assert_eq!(decode_utf7("&A-"), None);
        assert_eq!(decode_utf7("&&-"), None);
        assert_eq!(decode_utf7("&-&-").as_deref(), Some("&&"));
    }

    #[test]
    fn quotes_every_value() {
        assert_eq!(astring("INBOX", "mailbox").as_deref(), Ok("\"INBOX\""));
        assert_eq!(astring("Sent Items", "mailbox").as_deref(), Ok("\"Sent Items\""));
        assert_eq!(astring("", "mailbox").as_deref(), Ok("\"\""));
        assert_eq!(astring("a\"b", "mailbox").as_deref(), Ok("\"a\\\"b\""));
        assert_eq!(astring("a\\b", "mailbox").as_deref(), Ok("\"a\\\\b\""));
        assert_eq!(astring("&Jjo!", "mailbox").as_deref(), Ok("\"&Jjo!\""));
    }

    #[test]
    fn refuses_command_injection() {
        let cases = ["a\r\nB0001 DELETE \"INBOX\"", "a\nb", "a\rb", "a\0b"];
        for case in cases {
            let error = astring(case, "mailbox").err();
            assert!(matches!(error, Some(NetError::Protocol { .. })), "{case:?}");
        }
    }

    #[test]
    fn bounds_the_name() {
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(astring(&long, "mailbox"), Err(NetError::TooLarge { .. })));
    }
}
