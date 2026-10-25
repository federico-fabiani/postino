use std::fmt;

use chrono::{DateTime, Utc};

use crate::Headers;

/// A person or mailbox in a header. `name` is the decoded display name when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// Decoded display name, without quotes.
    pub name: Option<String>,
    /// Bare address, `local@domain`.
    pub address: String,
}

/// Where an attachment belongs in the rendered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disposition {
    /// Referenced by the body (typically through `cid:`) and shown inline.
    Inline,
    /// Offered as a download.
    Attachment,
}

/// A decoded attachment.
///
/// `data` is bounded by [`crate::Limits::max_part_bytes`] and by
/// [`crate::Limits::max_total_attachment_bytes`]. `truncated` says that `data` does not hold the
/// whole part: it is too large, the total attachment budget is exhausted, or the part is a
/// `message/rfc822` attachment that this crate intentionally does not expand. `size` is always the
/// decoded size the server reported for the part, even when `data` is empty.
#[derive(Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Sanitized file name. Never empty, never `.`/`..`, never contains a path separator.
    pub filename: String,
    /// Lowercase `type/subtype`; `application/octet-stream` when the message did not name a valid one.
    pub mime: String,
    /// `Content-ID` without angle brackets, when the part has one.
    pub content_id: Option<String>,
    /// Inline or attachment.
    pub disposition: Disposition,
    /// Decoded size in bytes.
    pub size: u64,
    /// Decoded bytes, empty when `truncated` is true for a part that was never materialised.
    pub data: Vec<u8>,
    /// Whether `data` is incomplete.
    pub truncated: bool,
}

impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field("filename", &self.filename)
            .field("mime", &self.mime)
            .field("content_id", &self.content_id)
            .field("disposition", &self.disposition)
            .field("size", &self.size)
            .field("data_len", &self.data.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Something tolerated, but worth recording.
///
/// Warnings are deduplicated per message. They are how "best effort" stays honest: the message is
/// still readable, and the caller knows exactly which bound or malformed structure it hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Warning {
    /// The parser stopped at the part limit; later parts are missing.
    TooManyParts,
    /// Multipart nesting was cut at the depth limit.
    TooDeep,
    /// A decoded part exceeded the per-part limit and its bytes were dropped.
    PartTooLarge,
    /// The total attachment budget was exhausted; later attachment bytes were dropped.
    TotalAttachmentLimit,
    /// The input, or a body derived from it, was cut short.
    Truncated,
    /// A charset was unknown or the bytes were invalid for it; decoding was lossy.
    BadCharset,
    /// A dangerous HTML construct was removed by the allowlist sanitizer.
    SanitizerStripped,
    /// Remote images were removed from the HTML.
    RemoteContentBlocked,
    /// A part declared a transfer encoding this crate does not understand; it became an attachment.
    UnknownEncoding,
    /// Base64 or quoted-printable bytes were malformed; decoding kept going.
    MalformedTransferEncoding,
    /// A multipart boundary was missing or unterminated.
    MissingBoundary,
    /// An address list was malformed; the entries that parsed were kept and the raw header remains
    /// available in [`ParsedMessage::headers`].
    BadAddress,
    /// An attachment name was empty or unusable after decoding; a fallback name was used.
    BadAttachmentName,
    /// A part content type was not a valid `type/subtype`; `application/octet-stream` was assumed.
    BadContentType,
    /// A header name was empty or contained a control character; the field was skipped.
    BadHeader,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooManyParts => "part limit reached; later parts were not parsed",
            Self::TooDeep => "multipart nesting limit reached; deeper content was not parsed",
            Self::PartTooLarge => "a decoded part exceeded the size limit and its bytes were dropped",
            Self::TotalAttachmentLimit => "the attachment byte budget was exhausted; later bytes were dropped",
            Self::Truncated => "the message or one of its bodies was truncated",
            Self::BadCharset => "a charset was unknown or invalid; text was decoded lossily",
            Self::SanitizerStripped => "dangerous HTML was removed by the sanitizer",
            Self::RemoteContentBlocked => "remote images were removed from the HTML",
            Self::UnknownEncoding => "a part used an unknown transfer encoding and was kept as an attachment",
            Self::MalformedTransferEncoding => "base64 or quoted-printable data was malformed",
            Self::MissingBoundary => "a multipart boundary was missing or unterminated",
            Self::BadAddress => "an address list was malformed; only the entries that parsed are exposed",
            Self::BadAttachmentName => "an attachment name was unusable; a fallback name was used",
            Self::BadContentType => "a part content type was invalid; application/octet-stream was assumed",
            Self::BadHeader => "a header name was empty or malformed; the field was skipped",
        };
        f.write_str(text)
    }
}

/// A message decoded by [`crate::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    /// Every header field, decoded and unfolded.
    pub headers: Headers,
    /// RFC 2047-decoded subject; empty when absent.
    pub subject: String,
    /// `From` mailboxes, groups expanded.
    pub from: Vec<Mailbox>,
    /// `To` mailboxes, groups expanded.
    pub to: Vec<Mailbox>,
    /// `Cc` mailboxes, groups expanded.
    pub cc: Vec<Mailbox>,
    /// `Bcc` mailboxes, groups expanded.
    pub bcc: Vec<Mailbox>,
    /// `Reply-To` mailboxes, groups expanded.
    pub reply_to: Vec<Mailbox>,
    /// Parsed `Date`, normalised to UTC.
    pub date: Option<DateTime<Utc>>,
    /// `Message-ID` without angle brackets.
    pub message_id: Option<String>,
    /// First `In-Reply-To` identifier, without angle brackets.
    pub in_reply_to: Option<String>,
    /// `References` identifiers, without angle brackets.
    pub references: Vec<String>,
    /// `List-Id` token, or the full decoded field when it is not in angle brackets.
    pub list_id: Option<String>,
    /// Best plain-text body. When the message has only HTML this is [`crate::html_to_text`] of the
    /// sanitized HTML.
    pub text: Option<String>,
    /// Sanitized HTML, or `None` when the message has no HTML part.
    pub html: Option<String>,
    /// Number of `http`/`https` images that were removed. Protocol-relative sources are removed
    /// too, but counted only when they reach the attribute filter.
    pub remote_images_blocked: usize,
    /// Bounds hit and malformed structures tolerated, deduplicated.
    pub warnings: Vec<Warning>,
    /// Attachments and inline parts, in document order. A part chosen as a body is never repeated
    /// here.
    pub attachments: Vec<Attachment>,
    /// Whether the root part is a multipart container.
    pub is_multipart: bool,
    /// Whether any part declares `text/calendar`.
    pub has_calendar: bool,
}
