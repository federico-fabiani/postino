use thiserror::Error;

/// Everything this crate refuses, with a message an agent or a log can act on.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MimeError {
    /// The raw message or one of its structures is larger than the configured hard limit.
    #[error("input is {size} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// Actual or requested size.
        size: usize,
        /// Configured limit.
        limit: usize,
    },
    /// The part count exceeds the configured limit.
    #[error("message has more than {limit} MIME parts")]
    TooManyParts {
        /// Configured limit.
        limit: usize,
    },
    /// Multipart nesting exceeds the configured limit.
    #[error("message nests multiparts deeper than {limit} levels")]
    TooDeep {
        /// Configured limit.
        limit: usize,
    },
    /// A header value contains a character that could start a new header.
    #[error("header {header} contains a control character; CR, LF and NUL are never allowed in a header")]
    HeaderInjection {
        /// Header being written.
        header: &'static str,
    },
    /// A mailbox is not a usable `local@domain` address.
    #[error("invalid mailbox address {address:?}: {reason}")]
    InvalidAddress {
        /// The address as given.
        address: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// A MIME type is not a single `type/subtype` token.
    #[error("invalid MIME type {value:?}: expected a single type/subtype token")]
    InvalidMimeType {
        /// The value as given.
        value: String,
    },
    /// An attachment name is empty, `.`/`..`, or otherwise unusable.
    #[error("invalid attachment filename {value:?}: {reason}")]
    InvalidAttachmentName {
        /// The value as given.
        value: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// A caller-provided `Message-ID` is not a single `<token>` value.
    #[error("invalid Message-ID {value:?}: expected one token inside angle brackets")]
    InvalidMessageId {
        /// The value as given.
        value: String,
    },
    /// The input is not an RFC 5322 message at all.
    #[error("not a parseable RFC 5322 message: {reason}")]
    Malformed {
        /// Why parsing had to stop.
        reason: &'static str,
    },
    /// The serializer could not assemble a valid message.
    #[error("could not encode the message: {reason}")]
    Encode {
        /// What failed.
        reason: &'static str,
    },
    /// A last-resort panic guard caught a failure inside a dependency.
    #[error("the MIME parser failed internally; the message was rejected")]
    Internal,
}
