//! MIME parsing, HTML sanitization, HTML-to-text and RFC 5322 serialization.
//!
//! This is a foundation crate: it has no network, storage, UI or engine dependency. Every input is
//! treated as hostile. The pipeline for received mail is
//!
//! ```text
//! bytes → parse (bounded) → typed headers and parts → sanitize HTML → text alternative
//! ```
//!
//! and for composed mail
//!
//! ```text
//! OutgoingMessage → validate headers → deterministic boundaries → RFC 5322 bytes + envelope
//! ```
//!
//! The parser is `mail-parser`; the HTML allowlist is `ammonia`; the serializer, quoted-printable,
//! base64, RFC 2047 and RFC 2231 encoders are deliberately ours. A library that folds headers or
//! writes boundaries would hide the injection and determinism rules that this crate has to own.
//!
//! See `README.md` for the contract, the allowlist and the limits. `docs/architecture.md`
//! describes where attachments go next (P4b).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![deny(missing_docs)]

mod blocks;
mod calendar;
mod compose;
mod encode;
mod error;
mod headers;
mod html_text;
mod limits;
mod mbox;
mod model;
mod parse;
pub mod pgp_mime;
mod sanitize;
mod serialize;
mod util;

pub use blocks::{
    Block, Body, BodyView, HeadingLevel, Image, Inline, Link, List, MAX_ALT_BYTES, MAX_BLOCKS, MAX_BODY_BYTES, MAX_DEPTH, MAX_LINK_BYTES,
    MAX_LIST_ITEMS, MAX_TABLE_COLUMNS, MAX_TABLE_ROWS, MAX_TEXT_CHARS, Style, Table, TableCell, TableRow, TextFallback, TextReason, TextRun,
    render_body,
};
pub use calendar::{
    CalendarInvite, InviteError, InviteWarning, MAX_CALENDAR_BYTES, MAX_CALENDAR_COMPONENTS, MAX_INVITE_ATTENDEES, MAX_INVITE_FIELD_CHARS,
    parse_invite, reply_calendar,
};
pub use compose::{ComposedEmail, MAX_COMPOSE_SOURCE_BYTES, compile_html_email};
pub use error::MimeError;
pub use headers::Headers;
pub use html_text::html_to_text;
pub use limits::Limits;
pub use mbox::{MAX_MBOX_BYTES, MAX_MBOX_MESSAGES, MboxError, MboxLimits, split_mbox, write_mbox};
pub use model::{Attachment, Disposition, Mailbox, ParsedMessage, Warning};
pub use parse::parse;
pub use sanitize::{Sanitized, sanitize_html};
pub use serialize::{
    Envelope, OutgoingAttachment, OutgoingMessage, Serialized, mime_for_filename, normalize_mime_type, sanitize_filename, serialize,
};
