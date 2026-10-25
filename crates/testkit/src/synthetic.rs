//! Deterministic synthetic messages for scale and reconciliation tests.
//!
//! Every message is a complete RFC 5322 document with a stable `Message-ID`, a fixed date, an ASCII
//! body and no external resources, so a test can compare bytes and a hash without a fixture file.
//! Addresses use `example.test`, which is reserved for tests and never a real mailbox.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Date every synthetic message claims in its `Date` header.
pub const DATE: &str = "Mon, 1 Jan 2024 10:00:00 +0000";
/// The same instant in IMAP `INTERNALDATE` form.
pub const INTERNAL_DATE: &str = "01-Jan-2024 10:00:00 +0000";
/// Host used by every synthetic address and `Message-ID`.
pub const HOST: &str = "example.test";

/// One generated message, with the envelope fields a fake server must report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticMessage {
    /// UID in the fake folder.
    pub uid: u32,
    /// `Message-ID` with angle brackets.
    pub message_id: String,
    /// `Subject`.
    pub subject: String,
    /// Display name of the sender.
    pub from_name: String,
    /// Local part of the sender.
    pub from_mailbox: String,
    /// Domain of the sender.
    pub from_host: &'static str,
    /// Header `Date`.
    pub date: &'static str,
    /// IMAP `INTERNALDATE` of the same instant.
    pub internal_date: &'static str,
    /// Plain-text body, when the message has one.
    pub body_text: String,
    /// Complete RFC 5322 bytes, CRLF line endings.
    pub raw: Vec<u8>,
}

impl SyntheticMessage {
    /// The envelope fields for [`crate::fetch_envelope_line`], with no flags.
    #[must_use]
    pub fn envelope(&self) -> crate::EnvelopeFields<'_> {
        crate::EnvelopeFields {
            uid: self.uid,
            flags: &[],
            internal_date: self.internal_date,
            size: u32::try_from(self.raw.len()).unwrap_or(u32::MAX),
            date: self.date,
            subject: &self.subject,
            from: Some(crate::EnvelopeAddress::named(&self.from_name, &self.from_mailbox, self.from_host)),
            to: Some(crate::EnvelopeAddress::named("Postino Test", "postino", HOST)),
            cc: None,
            message_id: Some(&self.message_id),
        }
    }
}

/// Generate message number `uid`: plain text, no attachments, no flags.
#[must_use]
pub fn synthetic_message(uid: u32) -> SyntheticMessage {
    let body = format!("Synthetic message number {uid}.\r\nThis body comes from the Postino fake IMAP server.\r\n");
    build(uid, &body, None, "text/plain; charset=\"utf-8\"", "Synthetic message")
}

/// Generate a message whose body is HTML with a script and a remote image, so a test can prove the
/// sanitizer ran on receipt. The plain-text alternative is empty: the MIME layer derives it.
#[must_use]
pub fn synthetic_html_message(uid: u32) -> SyntheticMessage {
    let html = format!(
        "<html><body><p>Hello <strong>number {uid}</strong>.</p>\
         <script>alert('postino')</script>\
         <img src=\"http://tracker.example.test/pixel-{uid}.png\" alt=\"pixel\">\
         </body></html>"
    );
    build(uid, &html, None, "text/html; charset=\"utf-8\"", "Synthetic HTML message")
}

/// Generate a `multipart/mixed` message with one base64 attachment.
#[must_use]
pub fn synthetic_message_with_attachment(uid: u32, filename: &str, mime: &str, bytes: &[u8]) -> SyntheticMessage {
    let body = format!("Synthetic message number {uid} with one attachment.\r\n");
    build(uid, &body, Some((filename, mime, bytes)), "text/plain; charset=\"utf-8\"", "Synthetic message with attachment")
}

/// Generate `count` messages with UIDs `1..=count`.
#[must_use]
pub fn synthetic_messages(count: u32) -> Vec<SyntheticMessage> {
    (1..=count).map(synthetic_message).collect()
}

fn build(uid: u32, body: &str, attachment: Option<(&str, &str, &[u8])>, content_type: &str, subject_prefix: &str) -> SyntheticMessage {
    let message_id = format!("<synthetic-{uid}@{HOST}>");
    let subject = format!("{subject_prefix} {uid}");
    let from_name = "Synthetic Sender".to_owned();
    let from_mailbox = format!("synthetic{uid}");
    let mut raw = String::new();
    raw.push_str(&format!("From: {from_name} <{from_mailbox}@{HOST}>\r\n"));
    raw.push_str(&format!("To: Postino Test <postino@{HOST}>\r\n"));
    raw.push_str(&format!("Subject: {subject}\r\n"));
    raw.push_str(&format!("Date: {DATE}\r\n"));
    raw.push_str(&format!("Message-ID: {message_id}\r\n"));
    raw.push_str("MIME-Version: 1.0\r\n");
    match attachment {
        None => {
            raw.push_str(&format!("Content-Type: {content_type}\r\n"));
            raw.push_str("Content-Transfer-Encoding: 8bit\r\n\r\n");
            raw.push_str(body);
        }
        Some((filename, mime, bytes)) => {
            let boundary = format!("postino-boundary-{uid}");
            raw.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"));
            raw.push_str(&format!("--{boundary}\r\n"));
            raw.push_str(&format!("Content-Type: {content_type}\r\n"));
            raw.push_str("Content-Transfer-Encoding: 8bit\r\n\r\n");
            raw.push_str(body);
            raw.push_str(&format!("--{boundary}\r\n"));
            raw.push_str(&format!("Content-Type: {mime}; name=\"{filename}\"\r\n"));
            raw.push_str(&format!("Content-Disposition: attachment; filename=\"{filename}\"\r\n"));
            raw.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
            for chunk in STANDARD.encode(bytes).as_bytes().chunks(76) {
                raw.push_str(std::str::from_utf8(chunk).unwrap_or_default());
                raw.push_str("\r\n");
            }
            raw.push_str(&format!("--{boundary}--\r\n"));
        }
    }
    SyntheticMessage {
        uid,
        message_id,
        subject,
        from_name,
        from_mailbox,
        from_host: HOST,
        date: DATE,
        internal_date: INTERNAL_DATE,
        body_text: body.to_owned(),
        raw: raw.into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_stable_and_distinct() {
        let first = synthetic_message(1);
        let again = synthetic_message(1);
        let second = synthetic_message(2);
        assert_eq!(first, again);
        assert_ne!(first.message_id, second.message_id);
        assert_eq!(first.message_id, "<synthetic-1@example.test>");
        assert!(first.raw.starts_with(b"From: Synthetic Sender <synthetic1@example.test>\r\n"));
        let raw = String::from_utf8(first.raw).unwrap_or_else(|error| panic!("{error}"));
        assert!(!raw.contains("\n\n"), "every line ends with CRLF, never a bare LF");
    }

    #[test]
    fn an_attachment_is_base64_and_multipart() {
        let message = synthetic_message_with_attachment(3, "report.pdf", "application/pdf", b"hello attachment");
        let raw = String::from_utf8(message.raw).unwrap_or_else(|error| panic!("{error}"));
        assert!(raw.contains("Content-Type: multipart/mixed; boundary=\"postino-boundary-3\""));
        assert!(raw.contains("Content-Disposition: attachment; filename=\"report.pdf\""));
        assert!(raw.contains(&STANDARD.encode("hello attachment")));
    }

    #[test]
    fn the_generator_produces_the_requested_count() {
        let messages = synthetic_messages(5);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages.first().map(|message| message.uid), Some(1));
        assert_eq!(messages.last().map(|message| message.uid), Some(5));
    }
}
