use std::panic::{AssertUnwindSafe, catch_unwind};

use chrono::{DateTime, Utc};
use mail_parser::{AddressList, DecodeProblems, HeaderForm, Message, MessageParser, PartFlags, PartKind};

use crate::html_text::html_to_text;
use crate::model::{Attachment, Disposition, Mailbox, ParsedMessage, Warning};
use crate::sanitize::sanitize_html;
use crate::util::{contains_ci, strip_controls, truncate_chars, unfold_lossy};
use crate::{Headers, Limits, MimeError};

/// Parse RFC 5322/2045 bytes into a safe, typed message.
///
/// The parser is tolerant: malformed structure is repaired where it can be and recorded in
/// [`ParsedMessage::warnings`]. The only hard failures are the configured size, part, depth and
/// header limits, an input that is not a message at all, or a panic caught by the last-resort
/// guard. HTML in the result is always sanitized; remote images are never kept.
pub fn parse(raw: &[u8], limits: &Limits) -> Result<ParsedMessage, MimeError> {
    let limits = limits.clamped();
    match catch_unwind(AssertUnwindSafe(|| parse_inner(raw, &limits))) {
        Ok(result) => result,
        Err(_) => Err(MimeError::Internal),
    }
}

fn parse_inner(raw: &[u8], limits: &Limits) -> Result<ParsedMessage, MimeError> {
    if raw.len() > limits.max_bytes {
        return Err(MimeError::TooLarge { size: raw.len(), limit: limits.max_bytes });
    }
    let header_bytes = header_block_len(raw);
    if header_bytes > limits.max_header_bytes {
        return Err(MimeError::TooLarge { size: header_bytes, limit: limits.max_header_bytes });
    }
    // The brief requires a pre-scan before the parser sees the bytes: even a parser with its own
    // depth limit should not be handed a message that declares hundreds of nested multiparts.
    let multiparts = multipart_headers(raw);
    if multiparts > limits.max_depth.saturating_mul(4) {
        return Err(MimeError::TooDeep { limit: limits.max_depth });
    }

    let parser = MessageParser::new().max_depth(limits.max_depth).max_parts(limits.max_parts).max_encoded_nesting(0);
    let message = parser.parse(raw).ok_or(MimeError::Malformed { reason: "the input has no header block" })?;

    let mut warnings = Vec::new();
    for part in message.parts() {
        record_part_flags(&part, &mut warnings);
    }

    let headers = collect_headers(&message, limits.max_header_bytes, &mut warnings);
    let from = collect_addresses(message.from());
    let to = collect_addresses(message.to());
    let cc = collect_addresses(message.cc());
    let bcc = collect_addresses(message.bcc());
    let reply_to = collect_addresses(message.reply_to());
    for (name, addresses) in [("from", &from), ("to", &to), ("cc", &cc), ("bcc", &bcc), ("reply-to", &reply_to)] {
        if addresses.is_empty() && headers.get(name).is_some() {
            // The field exists but no usable mailbox parsed out of it. The raw value stays in
            // `headers`, which is exactly the brief's "keep what parsed and expose the raw value".
            warn(&mut warnings, Warning::BadAddress);
        }
    }

    let subject = message.subject().map(strip_controls).unwrap_or_default();
    let date = message.date().and_then(|date| DateTime::<Utc>::from_timestamp(date.to_timestamp(), 0));
    let message_id = message.message_id().map(strip_controls).filter(|value| !value.is_empty());
    let in_reply_to = message.in_reply_to().and_then(|list| list.first()).map(strip_controls).filter(|value| !value.is_empty());
    let references = message.references().map(|list| list.iter().map(strip_controls).filter(|value| !value.is_empty()).collect()).unwrap_or_default();
    let list_id = list_id(&headers);

    let text_part = message.text_body().find(|part| {
        matches!(part.kind(), PartKind::Text)
            && part.has_known_transfer_encoding()
            && !declares_multipart(part)
            && (part.content_type().is_none() || part.is_content_type("text", "plain"))
    });
    let html_part =
        message.html_body().find(|part| matches!(part.kind(), PartKind::Html) && part.has_known_transfer_encoding() && !declares_multipart(part));

    let text_id = text_part.as_ref().map(|part| part.id());
    let html_id = html_part.as_ref().map(|part| part.id());

    let raw_text = text_part.as_ref().and_then(|part| read_text(part, limits.max_part_bytes, &mut warnings));
    let html = html_part.as_ref().and_then(|part| read_html(part, limits, &mut warnings));
    let (html, remote_images_blocked) = match html {
        Some(raw_html) => {
            let sanitized = sanitize_html(&raw_html, limits.max_html_bytes);
            if sanitized.remote_images_blocked > 0 {
                warn(&mut warnings, Warning::RemoteContentBlocked);
            }
            if sanitized.stripped || sanitized.failed {
                warn(&mut warnings, Warning::SanitizerStripped);
            }
            if sanitized.truncated {
                warn(&mut warnings, Warning::Truncated);
            }
            (Some(sanitized.html), sanitized.remote_images_blocked)
        }
        None => (None, 0),
    };
    let text = match (raw_text, html.as_deref()) {
        (Some(text), _) => Some(text),
        (None, Some(html)) => Some(html_to_text(html)),
        (None, None) => None,
    };

    let mut attachments = Vec::new();
    let mut total_bytes = 0usize;
    for part in message.root().parts() {
        if is_container(&part) {
            continue;
        }
        if text_id == Some(part.id()) || html_id == Some(part.id()) {
            continue;
        }
        if attachments.len() >= limits.max_parts {
            warn(&mut warnings, Warning::TooManyParts);
            break;
        }
        attachments.push(build_attachment(part, limits, &mut total_bytes, &mut warnings));
    }

    let has_calendar = message.root().parts().any(|part| part.is_content_type("text", "calendar"));

    Ok(ParsedMessage {
        headers,
        subject,
        from,
        to,
        cc,
        bcc,
        reply_to,
        date,
        message_id,
        in_reply_to,
        references,
        list_id,
        text,
        html,
        remote_images_blocked,
        warnings,
        attachments,
        is_multipart: matches!(message.root_part().kind(), PartKind::Multipart),
        has_calendar,
    })
}

fn collect_headers(message: &Message<'_>, max_value: usize, warnings: &mut Vec<Warning>) -> Headers {
    let mut headers = Headers::new();
    for header in message.headers().iter() {
        let name = header.raw_name().trim();
        if name.is_empty() || name.chars().any(|character| character == ':' || character.is_control()) {
            warn(warnings, Warning::BadHeader);
            continue;
        }
        let decoded = header.parse_as(HeaderForm::Text).value().as_text().map(str::to_owned).unwrap_or_else(|| unfold_lossy(header.raw_value()));
        let value = truncate_chars(&decoded, max_value);
        if value.len() != decoded.len() {
            warn(warnings, Warning::Truncated);
        }
        headers.push(truncate_chars(name, 256).to_owned(), strip_controls(value));
    }
    headers
}

fn collect_addresses(list: Option<AddressList<'_>>) -> Vec<Mailbox> {
    let mut mailboxes = Vec::new();
    let Some(list) = list else {
        return mailboxes;
    };
    for mailbox in list.mailboxes() {
        let Some(address) = mailbox.address() else {
            continue;
        };
        let address = strip_controls(address);
        if address.is_empty() {
            continue;
        }
        let name = mailbox.name().map(strip_controls).filter(|name| !name.is_empty());
        mailboxes.push(Mailbox { name, address });
    }
    mailboxes
}

fn list_id(headers: &Headers) -> Option<String> {
    let value = headers.get("list-id")?.trim();
    if let (Some(open), Some(close)) = (value.find('<'), value.rfind('>'))
        && close > open
        && let Some(id) = value.get(open + 1..close).map(str::trim).filter(|id| !id.is_empty())
    {
        return Some(id.to_owned());
    }
    (!value.is_empty()).then(|| value.to_owned())
}

fn read_text(part: &mail_parser::MessagePart<'_>, max_bytes: usize, warnings: &mut Vec<Warning>) -> Option<String> {
    if part.decoded_len() > max_bytes {
        warn(warnings, Warning::PartTooLarge);
        warn(warnings, Warning::Truncated);
        return part.text_prefix_bytes(max_bytes).map(|text| text.into_owned());
    }
    let (text, problems) = part.text_checked()?;
    record_decode_problems(problems, warnings);
    Some(text.into_owned())
}

fn read_html(part: &mail_parser::MessagePart<'_>, limits: &Limits, warnings: &mut Vec<Warning>) -> Option<String> {
    if part.decoded_len() > limits.max_html_bytes {
        warn(warnings, Warning::Truncated);
        return part.text_prefix_bytes(limits.max_html_bytes).map(|text| text.into_owned());
    }
    let (text, problems) = part.text_checked()?;
    record_decode_problems(problems, warnings);
    Some(text.into_owned())
}

fn build_attachment(part: mail_parser::MessagePart<'_>, limits: &Limits, total_bytes: &mut usize, warnings: &mut Vec<Warning>) -> Attachment {
    let mime = part_mime(&part, warnings);
    let raw_name = part.attachment_name().map(str::to_owned);
    let filename = match raw_name.as_deref().and_then(crate::serialize::clean_parsed_filename) {
        Some(name) => name,
        None => {
            if raw_name.is_some() {
                warn(warnings, Warning::BadAttachmentName);
            }
            default_filename(&mime)
        }
    };
    let content_id = part.content_id().map(strip_controls).filter(|id| !id.is_empty());
    let disposition = if part.is_inline() { Disposition::Inline } else { Disposition::Attachment };
    let size = part.decoded_len() as u64;

    let mut data = Vec::new();
    let mut truncated = false;
    let nested_message =
        matches!(part.kind(), PartKind::Message(_)) || part.is_content_type("message", "rfc822") || part.is_content_type("message", "global");
    if nested_message {
        // A nested message is an attachment by policy: metadata only, never expanded.
        truncated = true;
    } else if part.decoded_len() > limits.max_part_bytes {
        truncated = true;
        warn(warnings, Warning::PartTooLarge);
        warn(warnings, Warning::Truncated);
    } else if total_bytes.saturating_add(part.decoded_len()) > limits.max_total_attachment_bytes {
        truncated = true;
        warn(warnings, Warning::TotalAttachmentLimit);
    } else {
        let (decoded, problems) = part.decoded_checked();
        record_decode_problems(problems, warnings);
        if decoded.len() > limits.max_part_bytes {
            let keep = limits.max_part_bytes.min(decoded.len());
            if let Some(prefix) = decoded.get(..keep) {
                data.extend_from_slice(prefix);
            }
            truncated = true;
            warn(warnings, Warning::PartTooLarge);
            warn(warnings, Warning::Truncated);
        } else {
            data = decoded.into_owned();
        }
        *total_bytes = total_bytes.saturating_add(data.len());
    }

    Attachment { filename, mime, content_id, disposition, size, data, truncated }
}

fn part_mime(part: &mail_parser::MessagePart<'_>, warnings: &mut Vec<Warning>) -> String {
    let Some(content_type) = part.content_type() else {
        return if matches!(part.kind(), PartKind::Message(_)) { "message/rfc822".to_owned() } else { "text/plain".to_owned() };
    };
    let ctype = content_type.ctype().trim().to_ascii_lowercase();
    let Some(subtype) = content_type.subtype().map(|subtype| subtype.trim().to_ascii_lowercase()) else {
        warn(warnings, Warning::BadContentType);
        return "application/octet-stream".to_owned();
    };
    if ctype.is_empty() || subtype.is_empty() || !ctype.chars().all(crate::util::is_token_char) || !subtype.chars().all(crate::util::is_token_char) {
        warn(warnings, Warning::BadContentType);
        return "application/octet-stream".to_owned();
    }
    format!("{ctype}/{subtype}")
}

fn default_filename(mime: &str) -> String {
    let extension = match mime {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/calendar" => "ics",
        "application/zip" => "zip",
        "message/rfc822" => "eml",
        _ => return "attachment".to_owned(),
    };
    format!("attachment.{extension}")
}

fn record_decode_problems(problems: DecodeProblems, warnings: &mut Vec<Warning>) {
    if problems.contains(DecodeProblems::UNKNOWN_CHARSET) || problems.contains(DecodeProblems::MALFORMED_CHARSET) {
        warn(warnings, Warning::BadCharset);
    }
    if problems.contains(DecodeProblems::MALFORMED_TRANSFER_ENCODING) {
        warn(warnings, Warning::MalformedTransferEncoding);
    }
    if problems.contains(DecodeProblems::UNKNOWN_TRANSFER_ENCODING) {
        warn(warnings, Warning::UnknownEncoding);
    }
}

fn declares_multipart(part: &mail_parser::MessagePart<'_>) -> bool {
    part.content_type().is_some_and(|content_type| content_type.ctype().eq_ignore_ascii_case("multipart"))
}

/// A real multipart container, or a part that *declares* `multipart/*` but was parsed as a leaf
/// because its boundary was missing. The second kind must never become a body or an attachment.
fn is_container(part: &mail_parser::MessagePart<'_>) -> bool {
    matches!(part.kind(), PartKind::Multipart) || declares_multipart(part)
}

fn record_part_flags(part: &mail_parser::MessagePart<'_>, warnings: &mut Vec<Warning>) {
    let flags = part.flags();
    if declares_multipart_without_children(part) {
        warn(warnings, Warning::MissingBoundary);
    }
    if flags.contains(PartFlags::LIMIT_REACHED) {
        warn(warnings, Warning::TooManyParts);
    }
    if flags.contains(PartFlags::NESTING_LIMIT) {
        warn(warnings, Warning::TooDeep);
    }
    if flags.intersects(PartFlags::MISSING_DELIMITER) || flags.intersects(PartFlags::FALLBACK_DELIMITER) || flags.intersects(PartFlags::UNTERMINATED)
    {
        warn(warnings, Warning::MissingBoundary);
    }
    if flags.contains(PartFlags::NO_BLANK_LINE) {
        warn(warnings, Warning::Truncated);
    }
}

fn declares_multipart_without_children(part: &mail_parser::MessagePart<'_>) -> bool {
    declares_multipart(part) && !matches!(part.kind(), PartKind::Multipart)
}

fn warn(warnings: &mut Vec<Warning>, warning: Warning) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

/// End offset of the header block, including the blank line. A message without one reports its
/// whole length, which makes the header limit refuse it.
fn header_block_len(raw: &[u8]) -> usize {
    let mut position = 0usize;
    while position < raw.len() {
        let Some(rest) = raw.get(position..) else {
            break;
        };
        let Some(offset) = rest.iter().position(|byte| *byte == b'\n') else {
            return raw.len();
        };
        let line_end = position + offset;
        let line = raw.get(position..line_end).unwrap_or_default();
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return line_end.saturating_add(1).min(raw.len());
        }
        position = line_end.saturating_add(1);
    }
    raw.len()
}

/// Count declared `Content-Type: multipart/...` fields, folded continuations included.
fn multipart_headers(raw: &[u8]) -> usize {
    let mut count = 0usize;
    let mut continuation = false;
    for line in raw.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let is_continuation = line.first().is_some_and(|byte| *byte == b' ' || *byte == b'\t');
        if is_continuation {
            if continuation && contains_ci(line, b"multipart/") {
                count = count.saturating_add(1);
                continuation = false;
            }
            continue;
        }
        continuation = line.get(..13).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"content-type:"));
        if continuation && contains_ci(line, b"multipart/") {
            count = count.saturating_add(1);
            continuation = false;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_block_includes_the_blank_line() {
        assert_eq!(header_block_len(b"A: b\r\n\r\nbody"), 8);
        assert_eq!(header_block_len(b"A: b\n\nbody"), 6);
        assert_eq!(header_block_len(b"A: b\r\nbody"), 10);
        assert_eq!(header_block_len(b""), 0);
    }

    #[test]
    fn counts_folded_multipart_headers() {
        let raw = b"Content-Type: multipart/mixed;\r\n boundary=x\r\nContent-Type:\r\n multipart/alternative\r\n\r\n";
        assert_eq!(multipart_headers(raw), 2);
        assert_eq!(multipart_headers(b"Subject: multipart/mixed is not a header value\r\n\r\n"), 0);
    }

    #[test]
    fn mime_without_a_subtype_falls_back() {
        let raw = b"Content-Type: application\r\n\r\nbody";
        let message = MessageParser::new().parse(raw).expect("message");
        let part = message.root_part();
        let mut warnings = Vec::new();
        assert_eq!(part_mime(&part, &mut warnings), "application/octet-stream");
        assert!(warnings.contains(&Warning::BadContentType));
    }
}
