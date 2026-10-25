//! Response builders for scripted IMAP conversations.

/// Quote an IMAP string, escaping backslashes and quotes.
#[must_use]
pub fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// `NIL` for an absent value, or the quoted value.
fn text_or_nil(value: Option<&str>) -> String {
    match value {
        Some(value) => quoted(value),
        None => "NIL".to_owned(),
    }
}

/// Build a `* LIST` line with attributes, an optional delimiter and a name.
#[must_use]
pub fn list_line(attributes: &[&str], delimiter: Option<char>, name: &str) -> String {
    let delimiter = match delimiter {
        Some(ch) => quoted(&ch.to_string()),
        None => "NIL".to_owned(),
    };
    format!("* LIST ({}) {delimiter} {}", attributes.join(" "), quoted(name))
}

/// What a scripted `SELECT` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectStatus {
    /// `EXISTS`: number of messages in the mailbox.
    pub exists: u32,
    /// `RECENT`: number of recent messages.
    pub recent: u32,
    /// `UIDVALIDITY`.
    pub uid_validity: u32,
    /// `UIDNEXT`, when the server volunteers it.
    pub uid_next: Option<u32>,
}

impl SelectStatus {
    /// A mailbox with `exists` messages, none recent and no predicted `UIDNEXT`.
    #[must_use]
    pub fn new(exists: u32, uid_validity: u32) -> Self {
        Self { exists, recent: 0, uid_validity, uid_next: None }
    }
}

/// The untagged lines of a successful `SELECT` or `EXAMINE`.
#[must_use]
pub fn select_lines(status: &SelectStatus) -> Vec<String> {
    let mut lines = vec![
        format!("* {} EXISTS", status.exists),
        format!("* {} RECENT", status.recent),
        format!("* OK [UIDVALIDITY {}] UIDs valid", status.uid_validity),
    ];
    if let Some(uid_next) = status.uid_next {
        lines.push(format!("* OK [UIDNEXT {uid_next}] predicted next UID"));
    }
    lines.push("* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)".to_owned());
    lines.push("* OK [PERMANENTFLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft \\*)] permanent".to_owned());
    lines
}

/// Build a `* n FETCH (UID u BODY[] {len}\r\n<body>)` response, literal included.
///
/// A server answers `BODY.PEEK[]` with `BODY[]`; that spelling is what a client must parse.
#[must_use]
pub fn fetch_body_response(sequence: u32, uid: u32, body: &[u8]) -> Vec<u8> {
    let mut out = format!("* {sequence} FETCH (UID {uid} BODY[] {{{}}}\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(b")\r\n");
    out
}

/// One address as an IMAP `ENVELOPE` carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvelopeAddress<'a> {
    /// Display name, or `None` for `NIL`.
    pub name: Option<&'a str>,
    /// Local part.
    pub mailbox: &'a str,
    /// Domain or literal host name.
    pub host: &'a str,
}

impl<'a> EnvelopeAddress<'a> {
    /// An address with a display name.
    #[must_use]
    pub const fn named(name: &'a str, mailbox: &'a str, host: &'a str) -> Self {
        Self { name: Some(name), mailbox, host }
    }

    /// An address with no display name.
    #[must_use]
    pub const fn bare(mailbox: &'a str, host: &'a str) -> Self {
        Self { name: None, mailbox, host }
    }

    fn wire(&self) -> String {
        format!("({} NIL {} {})", text_or_nil(self.name), quoted(self.mailbox), quoted(self.host))
    }
}

fn address_list(address: Option<EnvelopeAddress<'_>>) -> String {
    match address {
        Some(address) => format!("({})", address.wire()),
        None => "NIL".to_owned(),
    }
}

/// The envelope fields a scripted `FETCH` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvelopeFields<'a> {
    /// The message UID.
    pub uid: u32,
    /// Wire flags, for example `["\\Seen", "\\Flagged"]`.
    pub flags: &'a [&'a str],
    /// `INTERNALDATE`, in `01-Jan-2024 10:00:00 +0000` form.
    pub internal_date: &'a str,
    /// RFC 822 size.
    pub size: u32,
    /// The header `Date` as the server reports it in `ENVELOPE`.
    pub date: &'a str,
    /// Subject exactly as the server reports it.
    pub subject: &'a str,
    /// `From`; it is also used as `Sender` and `Reply-To`.
    pub from: Option<EnvelopeAddress<'a>>,
    /// `To`.
    pub to: Option<EnvelopeAddress<'a>>,
    /// `Cc`.
    pub cc: Option<EnvelopeAddress<'a>>,
    /// `Message-ID`, angle brackets included when the server keeps them.
    pub message_id: Option<&'a str>,
}

/// Build one `* n FETCH (...)` line with a complete `ENVELOPE`.
#[must_use]
pub fn fetch_envelope_line(sequence: u32, fields: &EnvelopeFields<'_>) -> String {
    let flags = fields.flags.join(" ");
    let from = address_list(fields.from);
    let to = address_list(fields.to);
    let cc = address_list(fields.cc);
    format!(
        "* {sequence} FETCH (UID {} FLAGS ({flags}) INTERNALDATE {} RFC822.SIZE {} \
         ENVELOPE ({} {} {from} {from} {from} {to} {cc} NIL NIL {}))",
        fields.uid,
        quoted(fields.internal_date),
        fields.size,
        quoted(fields.date),
        quoted(fields.subject),
        text_or_nil(fields.message_id),
    )
}

/// Build a `* n FETCH (UID u FLAGS (...))` line, with no envelope.
#[must_use]
pub fn fetch_uid_flags_line(sequence: u32, uid: u32, flags: &[&str]) -> String {
    format!("* {sequence} FETCH (UID {uid} FLAGS ({}))", flags.join(" "))
}

/// Build an IDLE continuation line.
#[must_use]
pub fn idle_continuation(text: &str) -> String {
    if text.is_empty() { "+".to_owned() } else { format!("+ {text}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_only_what_imap_requires() {
        assert_eq!(quoted("plain"), "\"plain\"");
        assert_eq!(quoted("a \"quote\" and a \\"), "\"a \\\"quote\\\" and a \\\\\"");
    }

    #[test]
    fn a_list_line_has_attributes_delimiter_and_name() {
        assert_eq!(list_line(&["\\HasNoChildren", "\\Sent"], Some('/'), "Sent"), "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"");
        assert_eq!(list_line(&["\\Noselect"], None, "Archive"), "* LIST (\\Noselect) NIL \"Archive\"");
    }

    #[test]
    fn an_envelope_has_all_ten_rfc_fields() {
        let fields = EnvelopeFields {
            uid: 7,
            flags: &["\\Seen"],
            internal_date: "01-Jan-2024 10:00:00 +0000",
            size: 128,
            date: "Mon, 1 Jan 2024 10:00:00 +0000",
            subject: "Ciao",
            from: Some(EnvelopeAddress::named("Giulia", "giulia", "example.com")),
            to: Some(EnvelopeAddress::bare("me", "example.it")),
            cc: None,
            message_id: Some("<a@example.com>"),
        };
        let line = fetch_envelope_line(1, &fields);
        assert!(line.starts_with("* 1 FETCH (UID 7 FLAGS (\\Seen) INTERNALDATE \"01-Jan-2024 10:00:00 +0000\" RFC822.SIZE 128 ENVELOPE ("), "{line}");
        assert!(line.contains("((\"Giulia\" NIL \"giulia\" \"example.com\"))"), "{line}");
        assert!(line.contains("((NIL NIL \"me\" \"example.it\"))"), "{line}");
        assert!(line.ends_with("\"<a@example.com>\"))"), "{line}");
    }
}
