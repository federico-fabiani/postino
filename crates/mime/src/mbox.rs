//! Bounded mbox splitting and writing.
//!
//! An mbox file is a concatenation of messages, each introduced by a `From ` separator line. A
//! body line that starts with `From ` is stored with one extra `>`; reading removes that one `>`.
//! Both directions are bounded before allocating and never trust the separator alone: a candidate
//! separator is accepted only when the following line looks like an RFC 5322 header, so prose such
//! as `From the desk of Ada` inside a message is not mistaken for a new message.

use chrono::{DateTime, Utc};
use thiserror::Error;

/// Largest mbox a caller should hand to [`split_mbox`] with [`MboxLimits::default`].
pub const MAX_MBOX_BYTES: usize = 32 * 1024 * 1024;
/// Largest number of messages [`MboxLimits::default`] accepts.
pub const MAX_MBOX_MESSAGES: usize = 2000;
/// Largest separator line accepted. Real separators are shorter than a normal header line.
const MAX_SEPARATOR_LINE_BYTES: usize = 1024;

/// Everything mbox splitting refuses, with a message a person or agent can act on.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MboxError {
    /// The file is larger than the configured bound.
    #[error("the mbox is {size} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// Actual size.
        size: usize,
        /// Configured limit.
        limit: usize,
    },
    /// The file has no `From ` separator line at the start.
    #[error("the file does not start with an mbox `From ` separator line")]
    MissingSeparator,
    /// Bytes other than whitespace or a UTF-8 BOM precede the first separator.
    #[error("the file has content before the first mbox `From ` separator line")]
    ContentBeforeSeparator,
    /// More messages than the configured bound were found.
    #[error("the mbox has more than {limit} messages")]
    TooManyMessages {
        /// Configured limit.
        limit: usize,
    },
    /// A line looks like a separator but the message after it has no usable header block.
    #[error("line {line} looks like an mbox separator but the message after it has no valid header block")]
    AmbiguousSeparator {
        /// One-based line number.
        line: usize,
    },
    /// One message slice was empty after unquoting.
    #[error("mbox message {index} is empty")]
    EmptyMessage {
        /// One-based position in the file.
        index: usize,
    },
}

/// Bounds applied before the file is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MboxLimits {
    /// Largest accepted file, in bytes.
    pub max_bytes: usize,
    /// Largest accepted number of messages.
    pub max_messages: usize,
}

impl Default for MboxLimits {
    fn default() -> Self {
        Self { max_bytes: MAX_MBOX_BYTES, max_messages: MAX_MBOX_MESSAGES }
    }
}

/// Split an mbox into the exact bytes of each message, with one level of mboxrd quoting removed.
///
/// The returned messages still start with their RFC 5322 headers and include their final newline.
/// A file without a separator, with content before the first separator, with too many messages or
/// over the byte bound is refused as a whole; a partial import never happens.
pub fn split_mbox(raw: &[u8], limits: MboxLimits) -> Result<Vec<Vec<u8>>, MboxError> {
    if raw.len() > limits.max_bytes {
        return Err(MboxError::TooLarge { size: raw.len(), limit: limits.max_bytes });
    }
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let max_messages = limits.max_messages.max(1);
    let mut starts = Vec::new();
    let mut line_start = 0;
    let mut first = None;
    let mut line_number = 1usize;
    while line_start < raw.len() {
        let line_end = line_end(raw, line_start);
        let next = (line_end + 1).min(raw.len());
        let line = &raw[line_start..line_end];
        if line.len() <= MAX_SEPARATOR_LINE_BYTES && line.starts_with(b"From ") {
            let header_block = looks_like_message(raw, next);
            if plausible_separator(line) && header_block {
                if first.is_none() {
                    first = Some(line_start);
                }
                starts.push(line_start);
                if starts.len() > max_messages {
                    return Err(MboxError::TooManyMessages { limit: max_messages });
                }
            } else if header_block || plausible_separator(line) {
                // A plausible separator without a complete header block, or a `From ` line with an
                // unrecognised date followed by a real header block, is a damaged boundary rather
                // than prose. Refusing it is safer than silently merging two messages.
                if starts.is_empty() && raw[..line_start].iter().any(|byte| !byte.is_ascii_whitespace()) {
                    return Err(MboxError::ContentBeforeSeparator);
                }
                return Err(MboxError::AmbiguousSeparator { line: line_number });
            }
        }
        if next == raw.len() {
            break;
        }
        line_start = next;
        line_number += 1;
    }
    let first = first.ok_or(MboxError::MissingSeparator)?;
    if raw[..first].iter().any(|byte| !byte.is_ascii_whitespace()) {
        return Err(MboxError::ContentBeforeSeparator);
    }
    let mut messages = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().enumerate() {
        let body_start = (line_end(raw, *start) + 1).min(raw.len());
        let body_end = starts.get(index + 1).copied().unwrap_or(raw.len());
        if body_start >= body_end {
            return Err(MboxError::EmptyMessage { index: index + 1 });
        }
        let message = unquote_mboxrd(&raw[body_start..body_end]);
        if message.is_empty() {
            return Err(MboxError::EmptyMessage { index: index + 1 });
        }
        messages.push(message);
    }
    Ok(messages)
}

/// Write messages as an mbox using a deterministic `From - <date>` separator per message.
///
/// Every line that could look like a separator is quoted with one extra `>`, including lines that
/// already start with `>`. A message without a final newline gets one so the next separator starts
/// on its own line. A round-trip through [`split_mbox`] preserves every other byte.
#[must_use]
pub fn write_mbox(messages: &[(Vec<u8>, DateTime<Utc>)]) -> Vec<u8> {
    let capacity = messages.iter().map(|(raw, _)| raw.len().saturating_add(64)).sum();
    let mut out = Vec::with_capacity(capacity);
    for (raw, date) in messages {
        out.extend_from_slice(b"From - ");
        out.extend_from_slice(date.format("%a %b %e %H:%M:%S %Y").to_string().as_bytes());
        out.push(b'\n');
        append_quoted(&mut out, raw);
        if !out.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out
}

fn line_end(raw: &[u8], start: usize) -> usize {
    raw[start..].iter().position(|byte| *byte == b'\n').map_or(raw.len(), |offset| start + offset)
}

fn looks_like_message(raw: &[u8], start: usize) -> bool {
    let mut position = start;
    let mut headers = 0usize;
    let mut previous_was_header = false;
    while position < raw.len() && position.saturating_sub(start) <= 64 * 1024 {
        let end = line_end(raw, position);
        let line = raw[position..end].strip_suffix(b"\r").unwrap_or(&raw[position..end]);
        if line.is_empty() {
            return headers >= 1;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if !previous_was_header {
                return false;
            }
        } else if header_name(line).is_some() {
            headers += 1;
            previous_was_header = true;
        } else {
            return false;
        }
        if end == raw.len() {
            return false;
        }
        position = end + 1;
    }
    false
}

fn header_name(line: &[u8]) -> Option<&[u8]> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    if colon == 0 {
        return None;
    }
    let name = &line[..colon];
    name.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-').then_some(name)
}

fn plausible_separator(line: &[u8]) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(rest) = line.strip_prefix(b"From ") else { return false };
    let mut tokens = rest.split(|byte| byte.is_ascii_whitespace()).filter(|token| !token.is_empty());
    if tokens.next().is_none() {
        return false;
    }
    let mut month = false;
    let mut year = false;
    let mut time = false;
    for token in tokens.take(16) {
        month |= matches!(token, b"Jan" | b"Feb" | b"Mar" | b"Apr" | b"May" | b"Jun" | b"Jul" | b"Aug" | b"Sep" | b"Oct" | b"Nov" | b"Dec");
        year |= token.len() == 4 && token.iter().all(u8::is_ascii_digit) && token != b"0000";
        time |= token.len() == 8
            && token.get(2) == Some(&b':')
            && token.get(5) == Some(&b':')
            && token.iter().enumerate().all(|(index, byte)| index == 2 || index == 5 || byte.is_ascii_digit());
    }
    month && year && time
}

fn unquote_mboxrd(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut line_start = 0;
    while line_start < bytes.len() {
        let end = line_end(bytes, line_start);
        let mut line = &bytes[line_start..end];
        let after_newline = bytes.get(end).copied().unwrap_or(b'\n');
        if line.starts_with(b">") {
            let mut rest = line;
            while let Some(next) = rest.strip_prefix(b">") {
                rest = next;
            }
            if is_from_line(rest) {
                line = &line[1..];
            }
        }
        out.extend_from_slice(line);
        if end < bytes.len() {
            out.push(after_newline);
        }
        line_start = (end + 1).min(bytes.len());
        if line_start == bytes.len() {
            break;
        }
    }
    out
}

fn append_quoted(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut line_start = 0;
    while line_start < bytes.len() {
        let end = line_end(bytes, line_start);
        let line = &bytes[line_start..end];
        let mut rest = line;
        while let Some(next) = rest.strip_prefix(b">") {
            rest = next;
        }
        if is_from_line(rest) {
            out.push(b'>');
        }
        out.extend_from_slice(line);
        if end < bytes.len() {
            out.push(b'\n');
        }
        line_start = (end + 1).min(bytes.len());
        if line_start == bytes.len() {
            break;
        }
    }
}

fn is_from_line(line: &[u8]) -> bool {
    // mboxrd quotes `From ` only. A bare `From` is ordinary body text and must survive a round-trip.
    line.starts_with(b"From ")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn date() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 27, 10, 0, 0).single().expect("a valid date")
    }

    #[test]
    fn splits_two_messages_and_unquotes_a_body_separator() {
        let raw = b"From - Mon Oct 26 10:00:00 2026\nFrom: a@example.com\nSubject: one\n\n>From the desk\nFrom - Mon Oct 26 11:00:00 2026\nFrom: b@example.com\nSubject: two\n\nbody\n";
        let messages = split_mbox(raw, MboxLimits::default()).expect("split");
        assert_eq!(messages.len(), 2);
        assert!(messages[0].starts_with(b"From: a@example.com\n"));
        assert!(messages[0].ends_with(b"From the desk\n"));
        assert!(messages[1].starts_with(b"From: b@example.com\n"));
        assert!(messages[1].ends_with(b"body\n"));
    }

    #[test]
    fn prose_that_starts_with_from_is_not_a_separator() {
        let raw = b"From - Mon Oct 26 10:00:00 2026\nFrom: a@example.com\nSubject: prose\n\nFrom the desk of Ada\nnot a header: still prose\n";
        let messages = split_mbox(raw, MboxLimits::default()).expect("split");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].windows(20).any(|window| window == b"From the desk of Ada"));
    }

    #[test]
    fn write_then_split_round_trips_quoted_lines() {
        let first = b"From: a@example.com\r\nSubject: one\r\n\r\nFrom the top\r\n>From already quoted\r\n".to_vec();
        let second = b"From: b@example.com\nSubject: two\n\nbody".to_vec();
        let boxed = write_mbox(&[(first.clone(), date()), (second.clone(), date())]);
        let messages = split_mbox(&boxed, MboxLimits::default()).expect("split written mbox");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], first);
        assert_eq!(messages[1], [second.as_slice(), b"\n"].concat());
    }

    #[test]
    fn refuses_a_missing_separator_and_a_leading_garbage_prefix() {
        assert_eq!(split_mbox(b"From: a@example.com\n\nbody\n", MboxLimits::default()), Err(MboxError::MissingSeparator));
        assert_eq!(
            split_mbox(b"garbage\nFrom - Mon Oct 26 10:00:00 2026\nFrom: a@example.com\nSubject: x\n\nbody\n", MboxLimits::default()),
            Err(MboxError::ContentBeforeSeparator)
        );
    }

    #[test]
    fn accepts_a_bom_and_crlf_separators_without_losing_the_boundary() {
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice(
            b"From - Mon Oct 26 10:00:00 2026\r\nFrom: a@example.com\r\nSubject: one\r\n\r\nbody one\r\nFrom - Mon Oct 26 11:00:00 2026\r\nFrom: b@example.com\r\nSubject: two\r\n\r\nbody two\r\n",
        );
        let messages = split_mbox(&raw, MboxLimits::default()).expect("split BOM/CRLF mbox");
        assert_eq!(messages.len(), 2);
        assert!(messages[0].windows(10).any(|window| window == b"Subject: o"));
        assert!(messages[1].windows(10).any(|window| window == b"Subject: t"));
    }

    #[test]
    fn an_unparsable_from_line_before_a_header_block_is_refused_not_merged() {
        let raw = b"From - Mon Oct 26 10:00:00 2026\nFrom: a@example.com\nSubject: one\n\nbody\nFrom Ada Lovelace\nFrom: b@example.com\nSubject: two\n\nbody two\n";
        assert_eq!(split_mbox(raw, MboxLimits::default()), Err(MboxError::AmbiguousSeparator { line: 6 }));
    }

    #[test]
    fn a_single_header_message_is_still_a_message() {
        let raw = b"From - Mon Oct 26 10:00:00 2026\nSubject: solo\n\nbody\n";
        let messages = split_mbox(raw, MboxLimits::default()).expect("split a one-header message");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with(b"Subject: solo\n"));
    }

    #[test]
    fn a_plausible_separator_with_a_damaged_header_block_is_refused_not_merged() {
        let raw = b"From - Mon Oct 26 10:00:00 2026\nFrom: a@example.com\nSubject: one\n\nbody\nFrom - Mon Oct 26 11:00:00 2026\n_bad-header: two\nSubject: two\n\nbody\n";
        assert_eq!(split_mbox(raw, MboxLimits::default()), Err(MboxError::AmbiguousSeparator { line: 6 }));
    }

    #[test]
    fn enforces_the_message_and_byte_bounds_before_allocating_messages() {
        let raw = b"From - Mon Oct 26 10:00:00 2026\nFrom: a@example.com\nSubject: one\n\none\nFrom - Mon Oct 26 11:00:00 2026\nFrom: b@example.com\nSubject: two\n\ntwo\n";
        assert_eq!(split_mbox(raw, MboxLimits { max_bytes: MAX_MBOX_BYTES, max_messages: 1 }), Err(MboxError::TooManyMessages { limit: 1 }));
        assert_eq!(split_mbox(raw, MboxLimits { max_bytes: 16, max_messages: 10 }), Err(MboxError::TooLarge { size: raw.len(), limit: 16 }));
    }
}
