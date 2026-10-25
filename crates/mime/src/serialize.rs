use std::collections::HashSet;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::MimeError;
use crate::encode::{base64_lines, encoded_words, is_clean_7bit, percent_encode, quoted_printable};
use crate::model::Mailbox;
use crate::util::{hex_lower, normalize_crlf, truncate_chars};

/// A message ready to be serialized.
///
/// This is the compose-side type, deliberately separate from [`crate::ParsedMessage`]: a draft has
/// no raw bytes and no decoded attachments yet. Validation happens in [`serialize`], never in a
/// constructor, so an agent can build a message and receive one actionable error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// The single author mailbox.
    pub from: Mailbox,
    /// Primary recipients.
    pub to: Vec<Mailbox>,
    /// Carbon-copy recipients. They appear in the serialized headers.
    pub cc: Vec<Mailbox>,
    /// Blind carbon-copy recipients. They never appear in the serialized bytes; only in
    /// [`Envelope::recipients`].
    pub bcc: Vec<Mailbox>,
    /// Subject before RFC 2047 encoding.
    pub subject: String,
    /// Message date, written as an RFC 5322 date in UTC.
    pub date: DateTime<Utc>,
    /// Plain-text body. It is normalized to CRLF.
    pub text: String,
    /// Optional HTML body. It is serialized as given, normalized to CRLF; sanitization happens on
    /// the receiving side, because a compose layer may legitimately produce markup that the reader
    /// allowlist does not render.
    pub html: Option<String>,
    /// Attachments in order.
    pub attachments: Vec<OutgoingAttachment>,
    /// `In-Reply-To` identifier, with or without angle brackets.
    pub in_reply_to: Option<String>,
    /// `References` identifiers, with or without angle brackets.
    pub references: Vec<String>,
    /// Caller-provided `Message-ID`, with or without angle brackets. When absent, one is derived
    /// deterministically from the content hash and the sender domain.
    pub message_id: Option<String>,
}

/// An attachment to serialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    /// File name before sanitization. CR, LF and NUL are a hard error.
    pub filename: String,
    /// `type/subtype`; parameters are not accepted.
    pub mime: String,
    /// Raw bytes; base64 is applied by the serializer.
    pub data: Vec<u8>,
    /// Optional `Content-ID` (without angle brackets). When present the part is `inline`.
    pub content_id: Option<String>,
}

/// The bytes and the SMTP envelope of a serialized message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serialized {
    /// RFC 5322 bytes with CRLF line endings. `Bcc` never appears here.
    pub raw: Vec<u8>,
    /// Envelope information for the transport, including blind recipients.
    pub envelope: Envelope,
}

/// The SMTP envelope: who the server should treat as sender and recipients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Bare sender address.
    pub from: String,
    /// Deduplicated bare `To` + `Cc` + `Bcc` addresses, in that order.
    pub recipients: Vec<String>,
}

/// Longest accepted subject before RFC 2047 encoding. A mail header carrying more than this is not
/// a subject, it is an abuse of the compose path.
const MAX_SUBJECT_BYTES: usize = 16 * 1024;
/// Longest accepted display name.
const MAX_DISPLAY_NAME_BYTES: usize = 1024;
/// Longest accepted `Message-ID`/`Content-ID`/reference token.
const MAX_ID_BYTES: usize = 900;
/// Most references accepted in one message.
const MAX_REFERENCES: usize = 1000;

/// Serialize a message to RFC 5322 bytes and an envelope.
///
/// Deterministic: two identical messages produce identical bytes, boundaries and generated
/// `Message-ID`. Every header value is validated first; a CR, LF, NUL or other control character is
/// [`MimeError::HeaderInjection`], never stripped. `Bcc` is validated and then kept out of `raw`.
pub fn serialize(message: &OutgoingMessage) -> Result<Serialized, MimeError> {
    let from = format_mailbox("From", &message.from)?;
    let to = format_mailboxes("To", &message.to)?;
    let cc = format_mailboxes("Cc", &message.cc)?;
    let bcc = format_mailboxes("Bcc", &message.bcc)?;
    let subject = encode_subject(&message.subject)?;
    if message.references.len() > MAX_REFERENCES {
        return Err(MimeError::Encode { reason: "more than 1000 References entries" });
    }
    let in_reply_to = message.in_reply_to.as_deref().map(normalize_message_id).transpose()?;
    let references = message.references.iter().map(|reference| normalize_message_id(reference)).collect::<Result<Vec<_>, _>>()?;
    let prepared = message.attachments.iter().map(prepare_attachment).collect::<Result<Vec<_>, _>>()?;

    let text = normalize_crlf(&message.text);
    let html = message.html.as_deref().map(normalize_crlf);
    let (encoded_text, text_encoding) = encode_text_body(&text);
    let encoded_html = html.as_deref().map(encode_text_body);
    let encoded_attachments = prepared.iter().map(|attachment| (base64_lines(&attachment.data), attachment)).collect::<Vec<_>>();

    let digest = {
        let mut addressed = Vec::with_capacity(1 + to.len() + cc.len() + bcc.len());
        addressed.push(from.clone());
        addressed.extend(to.iter().cloned());
        addressed.extend(cc.iter().cloned());
        addressed.extend(bcc.iter().cloned());
        content_digest(message, &addressed, &text, html.as_deref(), &prepared)
    };
    let base = digest.get(..32).unwrap_or(digest.as_str()).to_owned();
    let message_id = match message.message_id.as_deref() {
        Some(provided) => normalize_message_id(provided)?,
        None => generated_message_id(&base, &message.from.address),
    };

    let mut bodies: Vec<&str> = vec![encoded_text.as_str()];
    if let Some((encoded, _)) = encoded_html.as_ref() {
        bodies.push(encoded.as_str());
    }
    bodies.extend(encoded_attachments.iter().map(|(encoded, _)| encoded.as_str()));

    let mixed_boundary = format!("=_postino_{base}_mixed");
    let alternative_boundary = format!("=_postino_{base}_alt");
    let mixed_boundary = unique_boundary(&mixed_boundary, &bodies);
    let alternative_boundary = unique_boundary(&alternative_boundary, &bodies);

    let mut writer = Writer::default();
    writer.header("From", &from)?;
    if !to.is_empty() {
        writer.header("To", &to.join(", "))?;
    }
    if !cc.is_empty() {
        writer.header("Cc", &cc.join(", "))?;
    }
    writer.header("Subject", &subject)?;
    writer.header("Date", &format_date(message.date))?;
    writer.header("Message-ID", &message_id)?;
    if let Some(in_reply_to) = in_reply_to.as_deref() {
        writer.header("In-Reply-To", in_reply_to)?;
    }
    if !references.is_empty() {
        writer.header("References", &references.join(" "))?;
    }
    writer.header("MIME-Version", "1.0")?;

    if prepared.is_empty() {
        match encoded_html.as_ref() {
            Some((encoded_html, html_encoding)) => {
                writer.header("Content-Type", &format!("multipart/alternative; boundary=\"{alternative_boundary}\""))?;
                writer.blank_line();
                write_multipart_body(
                    &mut writer,
                    &alternative_boundary,
                    &[
                        TextPart { mime: "text/plain", encoding: text_encoding, body: &encoded_text },
                        TextPart { mime: "text/html", encoding: html_encoding, body: encoded_html },
                    ],
                )?;
            }
            None => {
                writer.header("Content-Type", "text/plain; charset=utf-8")?;
                writer.header("Content-Transfer-Encoding", text_encoding)?;
                writer.blank_line();
                writer.bytes(encoded_text.as_bytes());
            }
        }
    } else {
        writer.header("Content-Type", &format!("multipart/mixed; boundary=\"{mixed_boundary}\""))?;
        writer.blank_line();
        writer.bytes(format!("--{mixed_boundary}\r\n").as_bytes());
        match encoded_html.as_ref() {
            Some((encoded_html, html_encoding)) => {
                writer.header("Content-Type", &format!("multipart/alternative; boundary=\"{alternative_boundary}\""))?;
                writer.blank_line();
                write_multipart_body(
                    &mut writer,
                    &alternative_boundary,
                    &[
                        TextPart { mime: "text/plain", encoding: text_encoding, body: &encoded_text },
                        TextPart { mime: "text/html", encoding: html_encoding, body: encoded_html },
                    ],
                )?;
            }
            None => {
                write_text_part(&mut writer, "text/plain", text_encoding, &encoded_text)?;
                writer.bytes(b"\r\n");
            }
        }
        for (encoded, attachment) in &encoded_attachments {
            writer.bytes(format!("--{mixed_boundary}\r\n").as_bytes());
            write_attachment_part(&mut writer, attachment, encoded)?;
            writer.bytes(b"\r\n");
        }
        writer.bytes(format!("--{mixed_boundary}--\r\n").as_bytes());
    }

    let envelope = envelope(message);
    Ok(Serialized { raw: writer.finish(), envelope })
}

struct PreparedAttachment {
    filename: String,
    mime: String,
    content_id: Option<String>,
    data: Vec<u8>,
}

struct TextPart<'a> {
    mime: &'a str,
    encoding: &'a str,
    body: &'a str,
}

fn write_multipart_body(writer: &mut Writer, boundary: &str, parts: &[TextPart<'_>]) -> Result<(), MimeError> {
    for part in parts {
        writer.bytes(format!("--{boundary}\r\n").as_bytes());
        write_text_part(writer, part.mime, part.encoding, part.body)?;
        writer.bytes(b"\r\n");
    }
    writer.bytes(format!("--{boundary}--\r\n").as_bytes());
    Ok(())
}

fn write_text_part(writer: &mut Writer, mime: &str, encoding: &str, body: &str) -> Result<(), MimeError> {
    writer.header("Content-Type", &format!("{mime}; charset=utf-8"))?;
    writer.header("Content-Transfer-Encoding", encoding)?;
    writer.blank_line();
    writer.bytes(body.as_bytes());
    Ok(())
}

fn write_attachment_part(writer: &mut Writer, attachment: &PreparedAttachment, encoded_body: &str) -> Result<(), MimeError> {
    writer.header("Content-Type", &attachment.mime)?;
    writer.header("Content-Transfer-Encoding", "base64")?;
    let disposition = if attachment.content_id.is_some() { "inline" } else { "attachment" };
    let mut value = format!("{disposition}; ");
    value.push_str(&filename_parameter(&attachment.filename));
    writer.header("Content-Disposition", &value)?;
    if let Some(content_id) = attachment.content_id.as_deref() {
        writer.header("Content-ID", &format!("<{content_id}>"))?;
    }
    writer.blank_line();
    writer.bytes(encoded_body.as_bytes());
    Ok(())
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn header(&mut self, name: &'static str, value: &str) -> Result<(), MimeError> {
        validate_header_value(name, value)?;
        self.write_folded(name, value);
        Ok(())
    }

    fn write_folded(&mut self, name: &'static str, value: &str) {
        let mut queue = value;
        let mut line = format!("{name}:");
        let mut name_only = true;
        while !queue.is_empty() {
            let available = 78usize.saturating_sub(line.len().saturating_add(1));
            if queue.len() <= available {
                line.push(' ');
                line.push_str(queue);
                break;
            }
            let mut cut = available.min(queue.len());
            while cut > 0 && !queue.is_char_boundary(cut) {
                cut -= 1;
            }
            let space = queue.get(..cut).and_then(|head| head.rfind(' '));
            match space {
                Some(space) if space > 0 => {
                    line.push(' ');
                    line.push_str(queue.get(..space).unwrap_or_default());
                    self.bytes(line.as_bytes());
                    self.bytes(b"\r\n");
                    line = String::new();
                    queue = queue.get(space + 1..).unwrap_or_default();
                    name_only = false;
                }
                _ if name_only => {
                    // A single word longer than the first line (an encoded word): fold before it.
                    self.bytes(line.as_bytes());
                    self.bytes(b"\r\n");
                    line = String::new();
                    queue = queue.trim_start_matches(' ');
                    name_only = false;
                }
                _ => {
                    line.push(' ');
                    line.push_str(queue);
                    break;
                }
            }
        }
        if line.is_empty() {
            return;
        }
        self.bytes(line.as_bytes());
        self.bytes(b"\r\n");
    }

    fn blank_line(&mut self) {
        self.bytes(b"\r\n");
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn finish(self) -> Vec<u8> {
        self.out
    }
}

fn validate_header_value(name: &'static str, value: &str) -> Result<(), MimeError> {
    reject_controls(name, value)?;
    if !value.is_ascii() {
        return Err(MimeError::Encode { reason: "a header value reached the writer without RFC 2047 encoding" });
    }
    Ok(())
}

fn reject_controls(name: &'static str, value: &str) -> Result<(), MimeError> {
    if value.chars().any(char::is_control) {
        return Err(MimeError::HeaderInjection { header: name });
    }
    Ok(())
}

fn format_date(date: DateTime<Utc>) -> String {
    format!("{} +0000", date.format("%a, %d %b %Y %H:%M:%S"))
}

fn format_mailboxes(name: &'static str, mailboxes: &[Mailbox]) -> Result<Vec<String>, MimeError> {
    mailboxes.iter().map(|mailbox| format_mailbox(name, mailbox)).collect()
}

fn format_mailbox(name: &'static str, mailbox: &Mailbox) -> Result<String, MimeError> {
    validate_address(name, &mailbox.address)?;
    let Some(display_name) = mailbox.name.as_deref().map(str::trim).filter(|display_name| !display_name.is_empty()) else {
        return Ok(mailbox.address.clone());
    };
    reject_controls(name, display_name)?;
    if display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(MimeError::Encode { reason: "a display name is longer than 1024 bytes" });
    }
    let display_name =
        if display_name.is_ascii() && display_name.len() <= 60 { quote_display_name(display_name) } else { encoded_words(display_name) };
    Ok(format!("{display_name} <{}>", mailbox.address))
}

fn quote_display_name(name: &str) -> String {
    const SPECIALS: &str = "()<>[]:;@\\,.\"";
    if name.chars().any(|character| SPECIALS.contains(character)) {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        name.to_owned()
    }
}

fn validate_address(header: &'static str, address: &str) -> Result<(), MimeError> {
    if address.chars().any(char::is_control) {
        return Err(MimeError::HeaderInjection { header });
    }
    let invalid = |reason: &'static str| MimeError::InvalidAddress { address: address.to_owned(), reason };
    if address.is_empty() {
        return Err(invalid("it is empty"));
    }
    if !address.is_ascii() {
        return Err(invalid("non-ASCII addresses are not supported yet"));
    }
    if address.chars().any(char::is_whitespace) {
        return Err(invalid("it contains whitespace"));
    }
    if address.len() > 320 {
        return Err(invalid("it is longer than 320 bytes"));
    }
    if address.chars().any(|character| "<>,;:\"()[]\\".contains(character)) {
        return Err(invalid("it contains a structural character"));
    }
    let Some((local, domain)) = address.split_once('@') else {
        return Err(invalid("it is not local@domain"));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(invalid("it must contain exactly one @ with text on both sides"));
    }
    if local.len() > 64 || domain.len() > 255 {
        return Err(invalid("the local part or domain is too long"));
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err(invalid("the local part has an empty label"));
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(invalid("the domain has an empty label"));
    }
    if !local.chars().all(|character| character.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~.".contains(character)) {
        return Err(invalid("the local part contains an unsupported character"));
    }
    if !domain.chars().all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '.') {
        return Err(invalid("the domain contains an unsupported character"));
    }
    Ok(())
}

fn encode_subject(subject: &str) -> Result<String, MimeError> {
    reject_controls("Subject", subject)?;
    if subject.len() > MAX_SUBJECT_BYTES {
        return Err(MimeError::Encode { reason: "the subject is longer than 16384 bytes" });
    }
    let too_long = subject.split(' ').any(|word| word.len() > 70);
    if subject.is_ascii() && !too_long { Ok(subject.to_owned()) } else { Ok(encoded_words(subject)) }
}

fn normalize_message_id(value: &str) -> Result<String, MimeError> {
    let trimmed = value.trim();
    let inner = trimmed.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')).unwrap_or(trimmed);
    if inner.is_empty()
        || inner.len() > MAX_ID_BYTES
        || !inner.is_ascii()
        || inner.chars().any(|character| character.is_control() || character.is_whitespace() || character == '<' || character == '>')
    {
        return Err(MimeError::InvalidMessageId { value: value.to_owned() });
    }
    Ok(format!("<{inner}>"))
}

fn generated_message_id(digest: &str, from: &str) -> String {
    let domain = from.rsplit_once('@').map(|(_, domain)| domain).unwrap_or("localhost");
    format!("<{digest}@{domain}>")
}

fn prepare_attachment(input: &OutgoingAttachment) -> Result<PreparedAttachment, MimeError> {
    let filename = clean_outgoing_filename(&input.filename)?;
    let mime = validate_mime_type(&input.mime)?;
    let content_id = match input.content_id.as_deref() {
        Some(value) => Some(normalize_content_id(value)?),
        None => None,
    };
    Ok(PreparedAttachment { filename, mime, content_id, data: input.data.clone() })
}

fn validate_mime_type(value: &str) -> Result<String, MimeError> {
    let trimmed = value.trim();
    let Some((ctype, subtype)) = trimmed.split_once('/') else {
        return Err(MimeError::InvalidMimeType { value: value.to_owned() });
    };
    if ctype.is_empty()
        || subtype.is_empty()
        || subtype.contains('/')
        || !ctype.chars().all(crate::util::is_token_char)
        || !subtype.chars().all(crate::util::is_token_char)
    {
        return Err(MimeError::InvalidMimeType { value: value.to_owned() });
    }
    Ok(format!("{}/{}", ctype.to_ascii_lowercase(), subtype.to_ascii_lowercase()))
}

fn normalize_content_id(value: &str) -> Result<String, MimeError> {
    let trimmed = value.trim();
    let inner = trimmed.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')).unwrap_or(trimmed);
    if inner.is_empty()
        || inner.len() > MAX_ID_BYTES
        || !inner.is_ascii()
        || inner.chars().any(|character| character.is_control() || character.is_whitespace() || character == '<' || character == '>')
    {
        return Err(MimeError::InvalidMessageId { value: value.to_owned() });
    }
    Ok(inner.to_owned())
}

fn clean_outgoing_filename(raw: &str) -> Result<String, MimeError> {
    if raw.chars().any(|character| matches!(character, '\r' | '\n' | '\0')) {
        return Err(MimeError::HeaderInjection { header: "Content-Disposition" });
    }
    let cleaned = sanitize_filename(raw);
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err(MimeError::InvalidAttachmentName { value: raw.to_owned(), reason: "it is empty, `.` or `..` after sanitization" });
    }
    Ok(cleaned)
}

/// Sanitize a file name for use as one path component: no separators, no control characters,
/// no trailing dots or spaces, at most 255 bytes.
pub(crate) fn sanitize_filename(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_control() || matches!(character, '/' | '\\' | ':') {
            continue;
        }
        cleaned.push(character);
    }
    let cleaned = truncate_chars(&cleaned, 255);
    let cleaned = cleaned.trim_matches(|character: char| character == ' ' || character == '\t' || character == '.');
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." { String::new() } else { cleaned.to_owned() }
}

/// Sanitize an attachment name received in a message. Unlike the serializer this is lossy: a
/// hostile name must not fail the whole message, it just gets a fallback.
pub(crate) fn clean_parsed_filename(raw: &str) -> Option<String> {
    let cleaned = sanitize_filename(raw);
    (!cleaned.is_empty()).then_some(cleaned)
}

fn filename_parameter(filename: &str) -> String {
    if filename.is_ascii() && filename.len() <= 48 && !filename.contains(['"', '\\']) {
        return format!("filename=\"{filename}\"");
    }
    let encoded = percent_encode(filename);
    if encoded.len() <= 48 {
        return format!("filename*=UTF-8''{encoded}");
    }
    let mut segments = Vec::new();
    let mut start = 0usize;
    while start < encoded.len() {
        let mut end = start.saturating_add(48).min(encoded.len());
        while end > start.saturating_add(1) && splits_triplet(&encoded, end) {
            end -= 1;
        }
        if let Some(segment) = encoded.get(start..end) {
            segments.push(segment.to_owned());
        }
        start = end;
    }
    let mut parameters = Vec::with_capacity(segments.len());
    for (index, segment) in segments.iter().enumerate() {
        if index == 0 {
            parameters.push(format!("filename*0*=UTF-8''{segment}"));
        } else {
            parameters.push(format!("filename*{index}*={segment}"));
        }
    }
    parameters.join("; ")
}

/// Whether `position` falls inside a `%XX` triplet of an RFC 5987 encoded value.
fn splits_triplet(encoded: &str, position: usize) -> bool {
    let bytes = encoded.as_bytes();
    (position >= 1 && bytes.get(position - 1) == Some(&b'%')) || (position >= 2 && bytes.get(position - 2) == Some(&b'%'))
}

fn content_digest(message: &OutgoingMessage, addressed: &[String], text: &str, html: Option<&str>, attachments: &[PreparedAttachment]) -> String {
    let mut digest = ContentDigest::new();
    for value in addressed {
        digest.field(2, value.as_bytes());
    }
    digest.field(3, message.subject.as_bytes());
    digest.field(4, &message.date.timestamp().to_be_bytes());
    digest.field(5, text.as_bytes());
    if let Some(html) = html {
        digest.field(6, html.as_bytes());
    }
    for attachment in attachments {
        digest.field(7, attachment.filename.as_bytes());
        digest.field(8, attachment.mime.as_bytes());
        if let Some(content_id) = attachment.content_id.as_deref() {
            digest.field(9, content_id.as_bytes());
        }
        digest.field(10, &attachment.data);
    }
    if let Some(in_reply_to) = message.in_reply_to.as_deref() {
        digest.field(11, in_reply_to.as_bytes());
    }
    for reference in &message.references {
        digest.field(12, reference.as_bytes());
    }
    digest.finish()
}

struct ContentDigest {
    hasher: Sha256,
}

impl ContentDigest {
    fn new() -> Self {
        Self { hasher: Sha256::new() }
    }

    fn field(&mut self, tag: u8, bytes: &[u8]) {
        self.hasher.update([tag]);
        self.hasher.update((bytes.len() as u64).to_be_bytes());
        self.hasher.update(bytes);
    }

    fn finish(self) -> String {
        hex_lower(&self.hasher.finalize())
    }
}

fn unique_boundary(base: &str, bodies: &[&str]) -> String {
    let mut token = base.to_owned();
    let mut counter = 0usize;
    while bodies.iter().any(|body| body.contains(token.as_str())) {
        counter = counter.saturating_add(1);
        token = format!("{base}_{counter}");
    }
    token
}

fn encode_text_body(text: &str) -> (String, &'static str) {
    if is_clean_7bit(text) { (text.to_owned(), "7bit") } else { (quoted_printable(text), "quoted-printable") }
}

fn envelope(message: &OutgoingMessage) -> Envelope {
    let mut seen = HashSet::new();
    let mut recipients = Vec::new();
    for mailbox in message.to.iter().chain(&message.cc).chain(&message.bcc) {
        let key = mailbox.address.to_ascii_lowercase();
        if seen.insert(key) {
            recipients.push(mailbox.address.clone());
        }
    }
    Envelope { from: message.from.address.clone(), recipients }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_filename("C:\\temp\\x.pdf"), "Ctempx.pdf");
        assert_eq!(sanitize_filename("  ..  "), "");
        assert_eq!(sanitize_filename("ok.pdf"), "ok.pdf");
        assert!(sanitize_filename(&"è".repeat(200)).len() <= 255);
    }

    #[test]
    fn folds_long_ascii_headers() {
        let mut writer = Writer::default();
        writer.header("Subject", &"parola ".repeat(20)).expect("header");
        let raw = String::from_utf8(writer.finish()).expect("utf8");
        for line in raw.split("\r\n").filter(|line| !line.is_empty()) {
            assert!(line.len() <= 78, "{}: {line}", line.len());
        }
        assert!(raw.contains("\r\n "));
    }

    #[test]
    fn rejects_control_characters_in_headers() {
        let mut writer = Writer::default();
        assert!(matches!(writer.header("Subject", "ok\r\nBcc: x@example.com"), Err(MimeError::HeaderInjection { .. })));
    }

    #[test]
    fn keeps_ascii_subjects_readable() {
        assert_eq!(encode_subject("Ciao a tutti").expect("subject"), "Ciao a tutti");
        assert!(encode_subject("Città").expect("subject").starts_with("=?UTF-8?B?"));
    }

    #[test]
    fn message_ids_are_wrapped_once() {
        assert_eq!(normalize_message_id("abc@example.com").expect("id"), "<abc@example.com>");
        assert_eq!(normalize_message_id("<abc@example.com>").expect("id"), "<abc@example.com>");
        assert!(normalize_message_id("a b").is_err());
        assert!(normalize_message_id("<a@b").is_err());
    }

    #[test]
    fn addresses_are_validated() {
        assert!(validate_address("To", "a@example.com").is_ok());
        assert!(validate_address("To", "a@example.com").is_ok());
        assert!(validate_address("To", "a@example.com\r\nBcc: b@example.com").is_err());
        assert!(validate_address("To", "not-an-address").is_err());
        assert!(validate_address("To", "a b@example.com").is_err());
        assert!(validate_address("To", "a@@example.com").is_err());
        assert!(validate_address("To", "à@example.com").is_err());
    }

    #[test]
    fn rfc2231_splits_without_breaking_triplets() {
        let parameter = filename_parameter("Relazione finale dell'ufficio tecnico è pronta.pdf");
        assert!(parameter.starts_with("filename*0*=UTF-8''"));
        assert!(parameter.contains("; filename*1*="));
        assert!(!parameter.contains("%%"));
        for segment in parameter.split("; ") {
            assert!(segment.len() <= 72, "{segment}");
        }
    }
}
