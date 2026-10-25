//! An in-memory mailbox that answers LIST/SELECT/FETCH for a whole test run.
//!
//! Hand-written scripts are the right tool for a protocol edge; a 2 000-message reconciliation
//! test needs a responder that stays correct while the client walks batches. [`serve_mailbox`]
//! answers the subset of IMAP that `postino-net` and `postino-sync` speak, from a [`TestFolder`]
//! list, and records every client line on the [`Conn`] it returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::responses::{
    EnvelopeAddress, EnvelopeFields, SelectStatus, fetch_body_response, fetch_envelope_line, idle_continuation, list_line, select_lines,
};
use crate::server::Conn;
use crate::synthetic::SyntheticMessage;

/// One message in a fake folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestMessage {
    /// UID inside the folder.
    pub uid: u32,
    /// Wire flags, for example `"\\Seen"`.
    pub flags: Vec<String>,
    /// Complete RFC 5322 bytes.
    pub raw: Vec<u8>,
    /// `INTERNALDATE`, in `01-Jan-2024 10:00:00 +0000` form.
    pub internal_date: String,
    /// Header `Date`.
    pub date: String,
    /// Subject.
    pub subject: String,
    /// Sender display name.
    pub from_name: String,
    /// Sender local part.
    pub from_mailbox: String,
    /// Sender domain.
    pub from_host: String,
    /// `Message-ID`, angle brackets included.
    pub message_id: Option<String>,
}

impl TestMessage {
    /// Build a message from a [`SyntheticMessage`].
    #[must_use]
    pub fn synthetic(message: SyntheticMessage) -> Self {
        Self {
            uid: message.uid,
            flags: Vec::new(),
            raw: message.raw,
            internal_date: message.internal_date.to_owned(),
            date: message.date.to_owned(),
            subject: message.subject,
            from_name: message.from_name,
            from_mailbox: message.from_mailbox,
            from_host: message.from_host.to_owned(),
            message_id: Some(message.message_id),
        }
    }

    /// Set the wire flags.
    #[must_use]
    pub fn with_flags(mut self, flags: &[&str]) -> Self {
        self.flags = flags.iter().map(|flag| (*flag).to_owned()).collect();
        self
    }

    /// The `FETCH` line this message answers with.
    #[must_use]
    pub fn envelope_line(&self, sequence: u32) -> String {
        let name = (!self.from_name.is_empty()).then_some(self.from_name.as_str());
        let flags = self.flags.iter().map(String::as_str).collect::<Vec<_>>();
        let fields = EnvelopeFields {
            uid: self.uid,
            flags: &flags,
            internal_date: &self.internal_date,
            size: u32::try_from(self.raw.len()).unwrap_or(u32::MAX),
            date: &self.date,
            subject: &self.subject,
            from: Some(EnvelopeAddress { name, mailbox: &self.from_mailbox, host: &self.from_host }),
            to: Some(EnvelopeAddress::named("Postino Test", "postino", "example.test")),
            cc: None,
            message_id: self.message_id.as_deref(),
        };
        fetch_envelope_line(sequence, &fields)
    }
}

/// One folder in a fake mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFolder {
    /// Decoded display name.
    pub name: String,
    /// Name exactly as the server reports it and the client sends it back.
    pub wire_name: String,
    /// Special-use attribute, for example `"\\Sent"`.
    pub special_use: Option<String>,
    /// Hierarchy delimiter.
    pub delimiter: char,
    /// `UIDVALIDITY`.
    pub uid_validity: u32,
    /// Messages, kept sorted by UID.
    pub messages: Vec<TestMessage>,
    /// Whether `SELECT` succeeds (`false` for a `\Noselect` container).
    pub selectable: bool,
}

impl TestFolder {
    /// A selectable folder with no messages.
    #[must_use]
    pub fn new(name: &str, uid_validity: u32) -> Self {
        Self {
            name: name.to_owned(),
            wire_name: name.to_owned(),
            special_use: None,
            delimiter: '/',
            uid_validity,
            messages: Vec::new(),
            selectable: true,
        }
    }

    /// Give the folder an RFC 6154 special-use attribute.
    #[must_use]
    pub fn with_special_use(mut self, attribute: &str) -> Self {
        self.special_use = Some(attribute.to_owned());
        self
    }

    /// Fill the folder, sorting by UID.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<TestMessage>) -> Self {
        self.messages = messages;
        self.messages.sort_by_key(|message| message.uid);
        self
    }

    /// Make the folder a `\Noselect` container.
    #[must_use]
    pub fn noselect(mut self) -> Self {
        self.selectable = false;
        self
    }

    /// Append one message and keep the UID order.
    pub fn add(&mut self, message: TestMessage) {
        self.messages.push(message);
        self.messages.sort_by_key(|message| message.uid);
    }

    /// The UID the server will assign next.
    #[must_use]
    pub fn uid_next(&self) -> u32 {
        self.messages.iter().map(|message| message.uid).max().unwrap_or(0).saturating_add(1)
    }
}

/// A way to break a scripted mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailPoint {
    /// Close the connection when the Nth `SELECT`/`EXAMINE` arrives (1-based).
    OnSelect(u32),
    /// Close the connection instead of answering the Nth body fetch (1-based).
    OnBody(u32),
    /// Close the connection instead of answering the Nth envelope fetch (1-based).
    OnEnvelopeFetch(u32),
}

/// What a scripted mailbox should do beyond answering commands.
#[derive(Default, Clone)]
pub struct MailboxScript {
    /// Close the connection at this point.
    pub fail: Option<FailPoint>,
    /// Set the flag when the Nth body fetch arrives, before it is answered (1-based). The client
    /// commits that message and stops before the next one.
    pub cancel_after_body: Option<(u32, Arc<AtomicBool>)>,
}

impl MailboxScript {
    /// Close the connection at `point`.
    #[must_use]
    pub fn fail(mut self, point: FailPoint) -> Self {
        self.fail = Some(point);
        self
    }

    /// Set `cancel` right after the client has received the `after`th body.
    #[must_use]
    pub fn cancel_after_body(mut self, after: u32, cancel: Arc<AtomicBool>) -> Self {
        self.cancel_after_body = Some((after, cancel));
        self
    }
}

/// Answer IMAP commands from `folders` until the client logs out or closes the connection.
///
/// The handler is deliberately strict: `STORE`, `EXPUNGE`, `COPY`, `MOVE` and `APPEND` are answered
/// `BAD`, so a sync run that mutates the server fails the test instead of passing silently.
pub async fn serve_mailbox(conn: Conn, folders: Vec<TestFolder>) -> Conn {
    serve_mailbox_with(conn, folders, MailboxScript::default()).await
}

/// [`serve_mailbox`] with failure and cancellation hooks.
pub async fn serve_mailbox_with(mut conn: Conn, folders: Vec<TestFolder>, script: MailboxScript) -> Conn {
    conn.prepare_sasl().await;
    let mut selected: Option<usize> = None;
    let mut selects = 0u32;
    let mut bodies = 0u32;
    let mut envelopes = 0u32;
    loop {
        let Some(line) = conn.read_line_opt().await else { return conn };
        let Some((tag, rest)) = line.split_once(' ') else { return conn };
        let mut pieces = rest.splitn(2, ' ');
        let verb = pieces.next().unwrap_or_default();
        let arguments = pieces.next().unwrap_or_default();
        match verb {
            "LOGOUT" => {
                conn.send_line("* BYE closing").await;
                conn.send_line(&format!("{tag} OK logout completed")).await;
                return conn;
            }
            "CAPABILITY" => {
                conn.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN IDLE").await;
                conn.send_line(&format!("{tag} OK done")).await;
            }
            "NOOP" => conn.send_line(&format!("{tag} OK done")).await,
            "LIST" => {
                for folder in &folders {
                    let mut attributes = vec![if folder.selectable { "\\HasNoChildren" } else { "\\Noselect" }];
                    if let Some(special_use) = &folder.special_use {
                        attributes.push(special_use);
                    }
                    conn.send_line(&list_line(&attributes, Some(folder.delimiter), &folder.wire_name)).await;
                }
                conn.send_line(&format!("{tag} OK list completed")).await;
            }
            "SELECT" | "EXAMINE" => {
                selects = selects.saturating_add(1);
                if script.fail == Some(FailPoint::OnSelect(selects)) {
                    return conn;
                }
                let Some(name) = parse_mailbox(arguments) else {
                    conn.send_line(&format!("{tag} BAD invalid mailbox name")).await;
                    continue;
                };
                let Some(index) = folders.iter().position(|folder| folder.selectable && folder.wire_name == name) else {
                    conn.send_line(&format!("{tag} NO [NONEXISTENT] no such mailbox")).await;
                    continue;
                };
                let folder = &folders[index];
                let status = SelectStatus {
                    exists: u32::try_from(folder.messages.len()).unwrap_or(u32::MAX),
                    recent: 0,
                    uid_validity: folder.uid_validity,
                    uid_next: Some(folder.uid_next()),
                };
                for response in select_lines(&status) {
                    conn.send_line(&response).await;
                }
                conn.send_line(&format!("{tag} OK [READ-WRITE] {} completed", if verb == "SELECT" { "SELECT" } else { "EXAMINE" })).await;
                selected = Some(index);
            }
            "UID" => {
                let Some(rest) = arguments.strip_prefix("FETCH ") else {
                    conn.send_line(&format!("{tag} BAD unsupported UID command")).await;
                    continue;
                };
                let Some((set, items)) = rest.split_once(' ') else {
                    conn.send_line(&format!("{tag} BAD missing FETCH items")).await;
                    continue;
                };
                let Some(folder) = selected.and_then(|index| folders.get(index)) else {
                    conn.send_line(&format!("{tag} NO [NONEXISTENT] no mailbox selected")).await;
                    continue;
                };
                if items.contains("BODY.PEEK[]") {
                    bodies = bodies.saturating_add(1);
                    if script.fail == Some(FailPoint::OnBody(bodies)) {
                        return conn;
                    }
                    let Some(uid) = set.parse::<u32>().ok().filter(|uid| *uid != 0) else {
                        conn.send_line(&format!("{tag} BAD invalid UID")).await;
                        continue;
                    };
                    let Some((sequence, message)) = folder.messages.iter().enumerate().find(|(_, message)| message.uid == uid) else {
                        conn.send_line(&format!("{tag} OK no such message")).await;
                        continue;
                    };
                    // Set the flag before answering: the client has already passed its check for
                    // this message, so it commits it and stops before the next one. Setting it after
                    // the write would be a race with the client's next check.
                    if let Some((after, cancel)) = &script.cancel_after_body
                        && *after == bodies
                    {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    let sequence = u32::try_from(sequence).unwrap_or(u32::MAX).saturating_add(1);
                    conn.send_bytes(&fetch_body_response(sequence, uid, &message.raw)).await;
                    conn.send_line(&format!("{tag} OK done")).await;
                    continue;
                }
                envelopes = envelopes.saturating_add(1);
                if script.fail == Some(FailPoint::OnEnvelopeFetch(envelopes)) {
                    return conn;
                }
                let Some(ranges) = parse_ranges(set) else {
                    conn.send_line(&format!("{tag} BAD invalid UID set")).await;
                    continue;
                };
                for (index, message) in folder.messages.iter().enumerate() {
                    if !ranges.iter().any(|(start, end)| message.uid >= *start && message.uid <= *end) {
                        continue;
                    }
                    let sequence = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
                    conn.send_line(&message.envelope_line(sequence)).await;
                }
                conn.send_line(&format!("{tag} OK done")).await;
            }
            "IDLE" => {
                conn.send_line(&idle_continuation("idling")).await;
                conn.expect("DONE").await;
                conn.send_line(&format!("{tag} OK IDLE terminated")).await;
            }
            "STORE" => conn.send_line(&format!("{tag} BAD sync must not change server flags")).await,
            other => conn.send_line(&format!("{tag} BAD unsupported command {other}")).await,
        }
    }
}

/// Parse a quoted or atom mailbox name, refusing an unterminated string.
fn parse_mailbox(argument: &str) -> Option<String> {
    let argument = argument.trim();
    if let Some(rest) = argument.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => out.push(chars.next()?),
                '"' => return Some(out),
                other => out.push(other),
            }
        }
        return None;
    }
    let name = argument.split(' ').next()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Parse a UID set into inclusive ranges. `*` is `u32::MAX` so a membership test stays simple.
fn parse_ranges(spec: &str) -> Option<Vec<(u32, u32)>> {
    if spec.is_empty() {
        return None;
    }
    let mut ranges = Vec::new();
    for part in spec.split(',') {
        let (start, end) = match part.split_once(':') {
            None => {
                let uid = parse_uid(part)?;
                (uid, uid)
            }
            Some((start, end)) => (parse_uid(start)?, parse_uid(end)?),
        };
        if start == 0 || start > end {
            return None;
        }
        ranges.push((start, end));
    }
    Some(ranges)
}

fn parse_uid(text: &str) -> Option<u32> {
    if text == "*" { Some(u32::MAX) } else { text.parse().ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_sets_are_inclusive_ranges() {
        assert_eq!(parse_ranges("3"), Some(vec![(3, 3)]));
        assert_eq!(parse_ranges("1:5"), Some(vec![(1, 5)]));
        assert_eq!(parse_ranges("1:*,9"), Some(vec![(1, u32::MAX), (9, 9)]));
        assert_eq!(parse_ranges("0"), None);
        assert_eq!(parse_ranges("5:3"), None);
        assert_eq!(parse_ranges(""), None);
    }

    #[test]
    fn mailbox_names_unquote_and_unescape() {
        assert_eq!(parse_mailbox("\"INBOX\"").as_deref(), Some("INBOX"));
        assert_eq!(parse_mailbox("\"a \\\"b\\\"\"").as_deref(), Some("a \"b\""));
        assert_eq!(parse_mailbox("INBOX").as_deref(), Some("INBOX"));
        assert_eq!(parse_mailbox("\"unterminated"), None);
    }

    #[test]
    fn folders_keep_messages_sorted_and_report_uid_next() {
        let mut folder = TestFolder::new("INBOX", 7);
        assert_eq!(folder.uid_next(), 1);
        folder.add(TestMessage::synthetic(crate::synthetic_message(4)));
        folder.add(TestMessage::synthetic(crate::synthetic_message(2)));
        assert_eq!(folder.messages.iter().map(|message| message.uid).collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(folder.uid_next(), 5);
    }
}
