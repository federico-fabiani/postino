//! Domain types. They are plain data: no I/O, no UI toolkit, no hidden state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A person or mailbox in a message header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub name: String,
    pub email: String,
}

impl Address {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self { name: name.into(), email: email.into() }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
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
