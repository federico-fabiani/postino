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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub kind: FolderKind,
    pub unread: u32,
    pub total: u32,
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
}

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
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn is_in_trash(&self) -> bool {
        self.summary.folder_id == "trash"
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
}
