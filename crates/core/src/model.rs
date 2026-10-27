//! Domain types. They are plain data: no I/O, no UI toolkit, no hidden state.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A person or mailbox in a message header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Address {
    /// Display name. Omit it for a bare mailbox.
    #[serde(default)]
    pub name: String,
    pub email: String,
}

impl Address {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self { name: name.into(), email: email.into() }
    }

    /// Parse a comma or semicolon separated recipient list. `Name <mail>` and bare mail are
    /// accepted; malformed entries are reported instead of being silently dropped.
    pub fn parse_list(input: &str) -> Result<Vec<Self>, String> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        let mut angled = false;
        for character in input.chars() {
            match character {
                '"' if !angled => {
                    quoted = !quoted;
                    current.push(character);
                }
                '<' if !quoted => {
                    angled = true;
                    current.push(character);
                }
                '>' if !quoted => {
                    angled = false;
                    current.push(character);
                }
                ',' | ';' if !quoted && !angled => {
                    if !current.trim().is_empty() {
                        parts.push(current.trim().to_owned());
                    }
                    current.clear();
                }
                _ => current.push(character),
            }
        }
        if quoted || angled {
            return Err("Virgolette o parentesi angolari non chiuse".to_owned());
        }
        if !current.trim().is_empty() {
            parts.push(current.trim().to_owned());
        }
        parts.into_iter().map(|part| parse_address(&part)).collect()
    }

    /// Format a recipient list back into the editable `Name <mail>, mail` text.
    #[must_use]
    pub fn format_list(addresses: &[Self]) -> String {
        addresses
            .iter()
            .map(|address| {
                if address.name.trim().is_empty() {
                    address.email.clone()
                } else if address.name.chars().any(|character| matches!(character, ',' | ';' | '<' | '>')) {
                    format!("\"{}\" <{}>", address.name.replace('"', ""), address.email)
                } else {
                    format!("{} <{}>", address.name, address.email)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `Name <email>`, or just the address when there is no display name.
    pub fn display(&self) -> String {
        if self.name.trim().is_empty() { self.email.clone() } else { format!("{} <{}>", self.name, self.email) }
    }

    /// One or two initials for an avatar circle.
    pub fn initials(&self) -> String {
        let source = if self.name.trim().is_empty() { self.email.as_str() } else { self.name.as_str() };
        let mut letters =
            source.split_whitespace().filter_map(|word| word.chars().next()).filter(|c| c.is_alphanumeric()).take(2).collect::<String>();
        if letters.is_empty() {
            letters = source.chars().filter(|c| c.is_alphanumeric()).take(1).collect();
        }
        letters.to_uppercase()
    }
}

fn parse_address(raw: &str) -> Result<Address, String> {
    let trimmed = raw.trim();
    let (name, email) = if let Some(open) = trimmed.rfind('<') {
        let Some(close) = trimmed.rfind('>') else {
            return Err(format!("Indirizzo non valido: {trimmed}"));
        };
        if close < open || !trimmed.get(close + 1..).unwrap_or("").trim().is_empty() {
            return Err(format!("Indirizzo non valido: {trimmed}"));
        }
        let name = trimmed.get(..open).unwrap_or("").trim().trim_matches('"').trim().to_owned();
        let email = trimmed.get(open + 1..close).unwrap_or("").trim().to_owned();
        (name, email)
    } else {
        (String::new(), trimmed.to_owned())
    };
    if email.is_empty()
        || email.chars().any(char::is_whitespace)
        || email.chars().any(|character| character.is_control() || matches!(character, '<' | '>' | ',' | ';'))
    {
        return Err(format!("Indirizzo non valido: {trimmed}"));
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err(format!("Indirizzo non valido: {trimmed}"));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(format!("Indirizzo non valido: {trimmed}"));
    }
    if name.chars().any(char::is_control) {
        return Err(format!("Nome non valido: {trimmed}"));
    }
    Ok(Address::new(name, email))
}

/// The role a folder plays. `Custom` is an ordinary IMAP folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderKind {
    Inbox,
    Starred,
    Sent,
    Drafts,
    Archive,
    Spam,
    Trash,
    Custom,
}

impl FolderKind {
    /// Folders that stand for a provider role rather than a named mailbox.
    pub fn is_virtual(self) -> bool {
        matches!(self, Self::Starred)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    /// Display name shown in the account switcher.
    pub name: String,
    pub email: String,
    /// Human-readable provider label; M1 replaces it with the discovered settings.
    pub provider: String,
    /// Signature appended to a new message. It is plain text in M0.
    #[serde(default)]
    pub signature: String,
}

/// The closed autodiscovery table. `Custom` requires the caller to supply host, port and TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Aruba,
    Gmail,
    Custom,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aruba => "aruba",
            Self::Gmail => "gmail",
            Self::Custom => "custom",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "aruba" => Some(Self::Aruba),
            "gmail" => Some(Self::Gmail),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    /// The offline settings of a known provider, or `None` for `custom`.
    pub fn discovery(self) -> Option<Discovery> {
        match self {
            Self::Aruba => Some(Discovery { host: "imaps.aruba.it", port: 993, tls: MailTls::Implicit }),
            Self::Gmail => Some(Discovery { host: "imap.gmail.com", port: 993, tls: MailTls::Implicit }),
            Self::Custom => None,
        }
    }

    /// The offline SMTP submission endpoint of a known provider, or `None` for `custom`.
    ///
    /// Aruba and Gmail both submit over implicit TLS on 465 with the same app password used for
    /// IMAP. OAuth for Gmail remains a later milestone.
    pub fn smtp_discovery(self) -> Option<Discovery> {
        match self {
            Self::Aruba => Some(Discovery { host: "smtps.aruba.it", port: 465, tls: MailTls::Implicit }),
            Self::Gmail => Some(Discovery { host: "smtp.gmail.com", port: 465, tls: MailTls::Implicit }),
            Self::Custom => None,
        }
    }

    /// The likely cause of a rejected login, for an actionable auth error. Never includes a secret.
    pub fn login_advice(self) -> &'static str {
        match self {
            Self::Aruba => "Aruba needs IMAP enabled and an app password when two-factor authentication is on",
            Self::Gmail => "Gmail requires 2-Step Verification and an app password; OAuth arrives in P12",
            Self::Custom => "check the username, password, host, port and TLS mode, and that the server allows IMAP",
        }
    }
}

/// Settings the offline provider table knows before any network call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Discovery {
    pub host: &'static str,
    pub port: u16,
    pub tls: MailTls,
}

/// How the IMAP connection is protected. There is deliberately no plaintext mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MailTls {
    /// TLS from the first byte, usually port 993.
    Implicit,
    /// Plaintext greeting and `CAPABILITY`, then `STARTTLS`, usually port 143.
    StartTls,
}

impl MailTls {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implicit => "implicit",
            Self::StartTls => "starttls",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "implicit" => Some(Self::Implicit),
            "starttls" => Some(Self::StartTls),
            _ => None,
        }
    }
}

/// Where an account's mail comes from, as reported back to a caller. There is no password field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub tls: MailTls,
    pub username: String,
    /// The submission endpoint, when the account has one. `None` means "read only": sending answers
    /// with an actionable error instead of inventing a server.
    #[serde(default)]
    pub smtp: Option<SmtpSettings>,
}

/// Where an account submits mail, as reported back to a caller. Never a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub tls: MailTls,
    pub username: String,
}

/// One account with its IMAP endpoint and last sync status. This is what `accounts_list` returns:
/// provider, address, server and sync health, never a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    pub account: Account,
    pub server: Option<ServerSettings>,
    pub sync: Option<postino_sync::SyncStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub kind: FolderKind,
    pub unread: u32,
    pub total: u32,
}

/// The closed semantic label palette. Names resolve through the theme; a label never stores a
/// colour value, so the theme can change without touching stored data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LabelColor {
    Gold,
    Forest,
    Success,
    Danger,
    Muted,
}

impl LabelColor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gold => "gold",
            Self::Forest => "forest",
            Self::Success => "success",
            Self::Danger => "danger",
            Self::Muted => "muted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "gold" => Some(Self::Gold),
            "forest" => Some(Self::Forest),
            "success" => Some(Self::Success),
            "danger" => Some(Self::Danger),
            "muted" => Some(Self::Muted),
            _ => None,
        }
    }
}

/// A local label. `id` is stable for the life of the label and is never reused after deletion;
/// messages hold ids, never a copy of the name. Labels are in-memory demo metadata until the M1
/// store exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Label {
    pub id: String,
    pub name: String,
    pub color: LabelColor,
}

/// A label with its mailbox-wide message counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelSummary {
    pub id: String,
    pub name: String,
    pub color: LabelColor,
    pub message_count: u32,
    pub unread_count: u32,
}

/// One conversation row. Counts are mailbox-wide; a folder or label view only decides which
/// threads are listed. `state` is `unread` when any message is unread, otherwise `read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    pub account_id: String,
    pub subject: String,
    pub snippet: String,
    /// Unique senders, earliest first, at most five.
    pub participants: Vec<Address>,
    pub last_date: DateTime<Utc>,
    pub message_count: u32,
    pub unread_count: u32,
    pub starred_count: u32,
    pub has_attachments: bool,
    pub latest_message_id: Option<String>,
    /// Union of the labels carried by the thread's messages, at most eight.
    pub labels: Vec<String>,
    pub state: String,
}

/// A saved search as the UI and agents see it. `valid: false` means the stored query no longer
/// resolves (a deleted label or folder); the row stays visible instead of becoming a dead entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSearch {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub query: String,
    pub sort: String,
    pub valid: bool,
    pub error: Option<String>,
    pub total: u32,
    pub unread: u32,
    /// True while the full-text index is still building. Counts are zero and the row is not stale;
    /// it becomes valid as soon as the index is ready.
    #[serde(default)]
    pub index_pending: bool,
}

/// One action of a local rule (P9c). The tag is the action; a rule carries 1-5 actions and at most
/// one of each kind. A forward carries 1-10 recipients and is only ever queued through P7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleAction {
    /// Move the message to a regular folder of the same account.
    Move { folder: String },
    /// Add a local label.
    Label { label: String },
    /// Mark the message read.
    Read,
    /// Mark the message unread.
    Unread,
    /// Move the message to the account's Trash folder.
    Trash,
    /// Forward the message through the outbox, 1-10 recipients.
    Forward { to: Vec<Address> },
}

impl RuleAction {
    /// The journal/storage tag, never localized.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Move { .. } => "move",
            Self::Label { .. } => "label",
            Self::Read => "read",
            Self::Unread => "unread",
            Self::Trash => "trash",
            Self::Forward { .. } => "forward",
        }
    }
}

/// One local rule as the operations and the UI see it. `valid` is computed against the current
/// folders and labels: a deleted target is shown as stale and fails at run time, never silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub enabled: bool,
    pub priority: i64,
    pub stop: bool,
    /// The documented search language.
    pub query: String,
    pub actions: Vec<RuleAction>,
    pub created_at: i64,
    pub updated_at: i64,
    pub valid: bool,
    pub error: Option<String>,
}

/// Which messages a run considers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleRunScope {
    /// Messages no earlier `new` run has applied.
    #[default]
    New,
    /// Every received message since the rule was created, up to the 2 000-message cap.
    All,
}

/// One run summary. The journal holds ids, folders, labels and booleans only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleRun {
    pub id: String,
    pub account_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub mode: RuleRunScope,
    pub rule_count: u32,
    pub matched: u32,
    pub changed: u32,
    pub errors: u32,
    pub undone: bool,
}

/// How an action would change one previewed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffectStatus {
    /// The action would change the message.
    Apply,
    /// The message is already in that state.
    NoOp,
    /// The action is deliberately skipped, for example a forward loop guard.
    Skip,
}

/// One action's effect in `rules_preview`. The UI localizes the action and the status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePreviewEffect {
    pub action: RuleAction,
    pub status: RuleEffectStatus,
}

/// One previewed message and the effects the hypothetical rule would have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePreviewRow {
    pub message: MessageSummary,
    pub effects: Vec<RulePreviewEffect>,
}

/// The list-row shape. Bodies are fetched separately so a mailbox list stays cheap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: String,
    pub thread_id: String,
    pub folder_id: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub subject: String,
    pub snippet: String,
    pub date: DateTime<Utc>,
    pub unread: bool,
    pub starred: bool,
    pub has_attachments: bool,
    /// Label ids. The engine owns the registry; a row never carries a copy of the name.
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Attachment metadata. The bytes live outside the database, in the encrypted blob store next to
/// it, and the engine exposes them through `Engine::attachment_bytes`; a `Message` never carries
/// multi-megabyte payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
}

/// The editable state of a draft. M0 stores it as a message in the Drafts folder; M1 moves it to
/// the encrypted draft blob without changing the operation surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftContent {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub body_text: String,
    pub attachments: Vec<Attachment>,
    /// Parent `Message-ID` for a reply, without angle brackets. `None` keeps the stored linkage on
    /// an update; a new draft simply has none.
    pub in_reply_to: Option<String>,
}

/// A message with its body. `body_html` is kept exactly as received; M0 renders `body_text`
/// and the webview-backed renderer arrives in M5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub summary: MessageSummary,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub body_text: String,
    pub body_html: Option<String>,
    /// Remote images the MIME sanitizer removed when this message was ingested, as stored by sync.
    /// It is a read-only derived fact: `draft_save` does not accept it and `0` means "none removed".
    /// `serde(default)` keeps older JSON and MCP callers valid.
    #[serde(default)]
    pub remote_images: u32,
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn is_in_trash(&self) -> bool {
        self.summary.folder_id == "trash"
    }
}

/// A parsed `text/calendar` invite, reduced to what the card and an agent need. It is derived from
/// the attachment on every read: nothing is stored, expanded or added to a calendar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalendarInviteView {
    /// `request`, `cancel`, `reply` or `other`.
    pub method: String,
    pub summary: String,
    /// Organizer mailbox, without the `mailto:` prefix.
    pub organizer: Option<String>,
    /// Attendee mailboxes, bounded and de-duplicated.
    pub attendees: Vec<String>,
    /// Normalized start (`2026-11-05T09:00:00Z`) or a date-only value.
    pub start: Option<String>,
    /// Normalized end, when the event declares it.
    pub end: Option<String>,
    pub location: Option<String>,
    /// The event declares a recurrence rule: it is warned about, never expanded.
    pub recurrence: bool,
    /// The declared `TZID`, kept honest: no timezone database is applied.
    pub timezone: Option<String>,
    /// The event has the fields a reply needs.
    pub valid: bool,
    /// Why the event is not valid, in a short sentence.
    pub invalid_reason: Option<String>,
    /// The organizer cancelled the event.
    pub cancelled: bool,
    /// A reply draft is offered only for a valid `REQUEST` with an organizer.
    pub can_reply: bool,
    pub warnings: Vec<String>,
}

/// The answer a person picked on the invite card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InviteResponse {
    Accept,
    Tentative,
    Decline,
}

impl InviteResponse {
    /// The `PARTSTAT` value carried by the `METHOD:REPLY` part.
    pub fn partstat(self) -> &'static str {
        match self {
            Self::Accept => "ACCEPTED",
            Self::Tentative => "TENTATIVE",
            Self::Decline => "DECLINED",
        }
    }

    /// The fixed draft subject prefix.
    pub fn subject_prefix(self) -> &'static str {
        match self {
            Self::Accept => "Accettato",
            Self::Tentative => "Tentativo",
            Self::Decline => "Rifiutato",
        }
    }
}

/// The one-line preview used by the list. It is deliberately short and never includes a header.
pub(crate) fn snippet(body: &str) -> String {
    let line = body.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let mut chars = line.chars();
    let text = chars.by_ref().take(120).collect::<String>();
    if chars.next().is_some() { format!("{text}…") } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_message_json_defaults_the_remote_count_to_zero() {
        let mut value = serde_json::json!({
            "summary": {
                "id": "msg_1",
                "thread_id": "thr_1",
                "folder_id": "inbox",
                "from": { "name": "Giulia", "email": "giulia@example.com" },
                "to": [],
                "subject": "Notizie",
                "snippet": "corpo",
                "date": "2026-10-24T10:42:00Z",
                "unread": false,
                "starred": false,
                "has_attachments": false,
                "labels": []
            },
            "cc": [],
            "bcc": [],
            "body_text": "corpo",
            "body_html": null,
            "attachments": []
        });
        let older: Message = serde_json::from_value(value.clone()).expect("an older message JSON");
        assert_eq!(older.remote_images, 0, "a missing field must read as zero");

        value["remote_images"] = serde_json::json!(2);
        let newer: Message = serde_json::from_value(value).expect("the current message JSON");
        assert_eq!(newer.remote_images, 2);
        let encoded = serde_json::to_value(&newer).expect("serialize");
        assert_eq!(encoded["remote_images"], serde_json::json!(2), "message_get must expose the field");
    }

    #[test]
    fn parses_bare_and_named_addresses() {
        let addresses =
            Address::parse_list("Giulia <giulia@example.com>, marco@example.com; \"Studio, Verdi\" <info@example.it>").expect("addresses");
        assert_eq!(addresses.len(), 3);
        assert_eq!(addresses[0].name, "Giulia");
        assert_eq!(addresses[1].email, "marco@example.com");
        assert_eq!(addresses[2].name, "Studio, Verdi");
    }

    #[test]
    fn refuses_incomplete_or_injected_addresses() {
        assert!(Address::parse_list("Giulia <giulia@example.com").is_err());
        assert!(Address::parse_list("Giulia <giulia@example.com>\nBcc: x@example.com").is_err());
        assert!(Address::parse_list("not-an-address").is_err());
    }

    #[test]
    fn formats_and_round_trips_addresses() {
        let input = "Giulia <giulia@example.com>, marco@example.com";
        let addresses = Address::parse_list(input).expect("addresses");
        let text = Address::format_list(&addresses);
        assert!(text.contains("Giulia <giulia@example.com>"));
        assert!(text.contains("marco@example.com"));
        assert_eq!(Address::parse_list(&text).expect("round trip").len(), 2);
    }

    #[test]
    fn every_label_color_round_trips_through_serde_and_parse() {
        for color in [LabelColor::Gold, LabelColor::Forest, LabelColor::Success, LabelColor::Danger, LabelColor::Muted] {
            let name = color.as_str();
            assert_eq!(LabelColor::parse(name), Some(color));
            let encoded = serde_json::to_string(&color).expect("serialize a label colour");
            assert_eq!(encoded, format!("\"{name}\""));
            let decoded: LabelColor = serde_json::from_str(&encoded).expect("deserialize a label colour");
            assert_eq!(decoded, color);
        }
        assert_eq!(LabelColor::parse("Gold"), None);
        assert_eq!(LabelColor::parse("blue"), None);
    }

    #[test]
    fn the_autodiscovery_table_is_the_documented_one() {
        let aruba = Provider::Aruba.discovery().expect("aruba is known");
        assert_eq!((aruba.host, aruba.port, aruba.tls), ("imaps.aruba.it", 993, MailTls::Implicit));
        let gmail = Provider::Gmail.discovery().expect("gmail is known");
        assert_eq!((gmail.host, gmail.port, gmail.tls), ("imap.gmail.com", 993, MailTls::Implicit));
        let aruba = Provider::Aruba.smtp_discovery().expect("aruba submits mail");
        assert_eq!((aruba.host, aruba.port, aruba.tls), ("smtps.aruba.it", 465, MailTls::Implicit));
        let gmail = Provider::Gmail.smtp_discovery().expect("gmail submits mail");
        assert_eq!((gmail.host, gmail.port, gmail.tls), ("smtp.gmail.com", 465, MailTls::Implicit));
        assert!(Provider::Gmail.login_advice().contains("app password"));
        assert!(Provider::Custom.discovery().is_none());
        assert!(Provider::Custom.smtp_discovery().is_none());
        for provider in [Provider::Aruba, Provider::Gmail, Provider::Custom] {
            assert_eq!(Provider::parse(provider.as_str()), Some(provider));
            assert_eq!(serde_json::to_value(provider).expect("provider JSON"), serde_json::json!(provider.as_str()));
        }
        assert_eq!(Provider::parse("Aruba"), None);
        for tls in [MailTls::Implicit, MailTls::StartTls] {
            assert_eq!(MailTls::parse(tls.as_str()), Some(tls));
            assert_eq!(serde_json::to_value(tls).expect("tls JSON"), serde_json::json!(tls.as_str()));
        }
    }
}
