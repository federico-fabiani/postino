//! Plain data owned by the store.
//!
//! These types deliberately do not depend on `postino-core`: the engine maps between its domain
//! types and these rows in P3c. Dates are Unix milliseconds (see `crate` docs), sizes are bytes and
//! booleans are stored as `0`/`1` in SQLite but are `bool` here.

use serde::{Deserialize, Serialize};

/// A configured mail account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    /// Display name shown in the account switcher.
    pub name: String,
    pub email: String,
    /// Human-readable provider label until autodiscovery replaces it.
    pub provider: String,
    /// Signature appended to a new message; plain text for now.
    pub signature: String,
    /// Unix milliseconds.
    pub created_at: i64,
}

/// How the IMAP connection is protected. There is deliberately no plaintext mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountTls {
    /// TLS from the first byte, usually port 993.
    Implicit,
    /// Plaintext greeting and `CAPABILITY`, then `STARTTLS`, usually port 143.
    StartTls,
}

impl AccountTls {
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

/// Where one account's mail comes from.
///
/// There is no password column and no field that could hold one: the credential lives in the OS
/// keyring under `account:<id>:imap` and is never written to the encrypted database. The table is
/// deliberately one row per account, so a half-configured account cannot exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountServer {
    pub account_id: String,
    pub host: String,
    pub port: u16,
    pub tls: AccountTls,
    /// Login name. It is normally the address, but the column is separate so a future provider can
    /// use a different one.
    pub username: String,
    /// Unix milliseconds.
    pub created_at: i64,
}

/// The role a folder plays. `Custom` is an ordinary IMAP folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Starred => "starred",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Archive => "archive",
            Self::Spam => "spam",
            Self::Trash => "trash",
            Self::Custom => "custom",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "inbox" => Some(Self::Inbox),
            "starred" => Some(Self::Starred),
            "sent" => Some(Self::Sent),
            "drafts" => Some(Self::Drafts),
            "archive" => Some(Self::Archive),
            "spam" => Some(Self::Spam),
            "trash" => Some(Self::Trash),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

/// A folder row. `unread` and `total` are a cache maintained by the repository from `messages`;
/// [`crate::Store::recount_folder`] repairs them after a bulk import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub kind: FolderKind,
    pub unread: u32,
    pub total: u32,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    /// Unix milliseconds of the last successful sync.
    pub last_sync_at: Option<i64>,
}

/// A person or mailbox in a message header. The stored JSON is the same shape as the engine's
/// `Address`, so P3c can map with `serde_json` if it prefers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub name: String,
    pub email: String,
}

impl Address {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self { name: name.into(), email: email.into() }
    }
}

/// The list-row shape. Bodies and headers other than `from`/`to` are fetched with
/// [`crate::Store::get_message`], so a mailbox list stays cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: String,
    pub account_id: String,
    pub folder_id: String,
    pub thread_id: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub subject: String,
    pub snippet: String,
    /// Unix milliseconds.
    pub date: i64,
    pub unread: bool,
    pub starred: bool,
    pub has_attachments: bool,
    /// Label ids for display. Read-only here: [`crate::Store::upsert_message`] never changes label
    /// assignments, they are changed by apply/remove.
    pub labels: Vec<String>,
}

/// A message with its body. `body_html` is stored exactly as received; sanitization is the MIME
/// layer's job, not the store's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub summary: MessageSummary,
    pub uid: Option<u32>,
    pub message_id: Option<String>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub body_text: String,
    pub body_html: Option<String>,
    /// RFC 822 size in bytes, when the provider reports it.
    pub size: Option<u64>,
}

/// A partial flag update: `None` leaves the current value alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlagUpdate {
    pub unread: Option<bool>,
    pub starred: Option<bool>,
}

/// What a sync pass needs to know about a message that may already be cached: where it lives and
/// which flags the user sees. Reading only this keeps a per-message drift check cheap; the body and
/// label queries are not needed for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFlags {
    pub folder_id: String,
    pub unread: bool,
    pub starred: bool,
}

/// One folder's cached identity and local flags, read in a single query.
///
/// Sync uses this snapshot for duplicate-Message-ID disambiguation, drift comparison and the
/// deletion candidates, so a folder-sized pass costs one statement, not one per message. `uid` is
/// `None` for a row that was never on the server (a local draft or locally sent mail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderMessageFlags {
    pub message_id: String,
    pub uid: Option<u32>,
    pub unread: bool,
    pub starred: bool,
}

/// The per-folder sync cursor.
///
/// `uid_next` is the next UID the reconciler still has to fetch: the highest committed UID plus one,
/// advanced only after a message transaction succeeds. It is deliberately separate from
/// [`Folder::uid_next`], which is the raw `UIDNEXT` the server advertised at `SELECT` time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    pub folder_id: String,
    pub uid_next: u32,
    pub highest_modseq: Option<u64>,
    pub last_error: Option<String>,
}

/// One cached server UID: the identity a deletion pass compares against the server's UID list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UidEntry {
    pub message_id: String,
    pub uid: u32,
}

/// The message-list query. Folder, label and flag filters combine with `AND`; `None` means "every".
/// `limit` is clamped to 1..=500 (over the cap included) and `offset` is a row offset, not a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFilter {
    pub folder_id: Option<String>,
    pub label_id: Option<String>,
    pub unread: Option<bool>,
    pub starred: Option<bool>,
    pub has_attachments: Option<bool>,
    pub limit: u32,
    pub offset: u32,
}

impl Default for MessageFilter {
    fn default() -> Self {
        Self { folder_id: None, label_id: None, unread: None, starred: None, has_attachments: None, limit: 50, offset: 0 }
    }
}

/// One page plus the totals for the filter before pagination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagePage {
    pub messages: Vec<MessageSummary>,
    pub total: u32,
    /// Unread rows in the filtered scope. `0` when the filter itself asks for read-only rows.
    pub unread: u32,
}

/// Per-folder (or mailbox-wide) totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageCounts {
    pub total: u32,
    pub unread: u32,
}

/// Attachment metadata. The bytes live outside the database (encrypted blobs arrive with P4); the
/// store only records the name, type, size and optional content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
    /// Content hash of the stored blob, when one exists.
    pub blob_hash: Option<String>,
}

/// A label. `account_id = None` is a mailbox-wide label; otherwise it belongs to one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub id: String,
    pub account_id: Option<String>,
    pub name: String,
    /// Semantic colour name (`gold`, `forest`, ...). The palette is the engine's concern.
    pub color: String,
}

/// A label with its live message counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelCount {
    pub label: Label,
    pub message_count: u32,
    pub unread_count: u32,
}

/// A saved draft, including the store-owned revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: String,
    pub account_id: String,
    pub revision: u32,
    /// Unix milliseconds.
    pub updated_at: i64,
    pub body_text: String,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub in_reply_to: Option<String>,
}

/// The editable fields of a draft. The store owns `revision` and increments it on every save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftUpdate {
    pub id: String,
    pub account_id: String,
    /// Unix milliseconds.
    pub updated_at: i64,
    pub body_text: String,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    /// Reply linkage. `None` keeps the stored value on an update; there is no explicit clear yet,
    /// so a field that does not own the link cannot destroy it.
    pub in_reply_to: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_folder_kind_round_trips_through_its_name() {
        for kind in [
            FolderKind::Inbox,
            FolderKind::Starred,
            FolderKind::Sent,
            FolderKind::Drafts,
            FolderKind::Archive,
            FolderKind::Spam,
            FolderKind::Trash,
            FolderKind::Custom,
        ] {
            assert_eq!(FolderKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(FolderKind::parse("Inbox"), None);
        assert_eq!(FolderKind::parse(""), None);
    }

    #[test]
    fn every_tls_mode_round_trips_through_its_name() {
        for tls in [AccountTls::Implicit, AccountTls::StartTls] {
            assert_eq!(AccountTls::parse(tls.as_str()), Some(tls));
        }
        assert_eq!(AccountTls::parse("Implicit"), None);
        assert_eq!(AccountTls::parse("plain"), None, "there is no plaintext mode");
    }
}
