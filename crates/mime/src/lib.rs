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

#![deny(missing_docs)]

mod encode;
mod error;
mod headers;
mod html_text;
mod limits;
mod model;
mod parse;
mod sanitize;
mod serialize;
mod util;

pub use error::MimeError;
pub use headers::Headers;
pub use html_text::html_to_text;
pub use limits::Limits;
pub use model::{Attachment, Disposition, Mailbox, ParsedMessage, Warning};
pub use parse::parse;
pub use sanitize::{Sanitized, sanitize_html};
pub use serialize::{
    Envelope, OutgoingAttachment, OutgoingMessage, Serialized, mime_for_filename, normalize_mime_type, sanitize_filename, serialize,
};
