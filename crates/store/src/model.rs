//! Plain data owned by the store.
//!
//! These types deliberately do not depend on `postino-core`: the engine maps between its domain
//! types and these rows in P3c. Dates are Unix milliseconds (see `crate` docs), sizes are bytes and
//! booleans are stored as `0`/`1` in SQLite but are `bool` here.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

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
    /// The submission endpoint. `None` for an account added before P7 or one that may only read.
    pub smtp: Option<SmtpServer>,
}

/// Where one account submits mail. The credential is in the keyring under `account:<id>:smtp`.
///
/// The username is separate from IMAP on purpose: providers such as Aruba may use the same address,
/// but a custom server can require a different login. Never a password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpServer {
    pub host: String,
    pub port: u16,
    pub tls: AccountTls,
    pub username: String,
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

/// Largest remote-image count a message may carry.
///
/// The MIME parser can count more images in a hostile message, but the reading pane only needs a
/// bounded fact and the database stores a bounded integer. Both the write and the read clamp to it.
pub const MAX_REMOTE_IMAGES: u32 = 10_000;

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
    /// Remote images the MIME sanitizer removed from `body_html` when the message was ingested.
    /// This is stored truth, never re-derived from the sanitized HTML (which no longer contains the
    /// references). `0` on old rows and on drafts.
    pub remote_images: u32,
    /// RFC 822 size in bytes, when the provider reports it.
    pub size: Option<u64>,
}

/// The threading headers stored with one message, ready for a `.eml` reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageThreading {
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

/// Why a message row exists. This is provenance, not its current folder: a Sent copy moved to
/// Archive is still `Sent`, and a received message moved to Sent is still `Received`. Rules only
/// consider `Received`, so moving a message afterwards cannot turn it into rule mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageOrigin {
    Received,
    Sent,
    Draft,
    Local,
}

impl MessageOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Sent => "sent",
            Self::Draft => "draft",
            Self::Local => "local",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "received" => Some(Self::Received),
            "sent" => Some(Self::Sent),
            "draft" => Some(Self::Draft),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
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

/// The state of one outbox row.
///
/// `Queued` and `Sending` are the only states a submitter may act on. `AppendPending` means SMTP
/// already accepted the message and only the IMAP Sent copy is missing; it is retried independently
/// and never resubmitted. `Uncertain` means the final `.` was written and no reply was read: only a
/// person may resolve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    /// Waiting for its due time.
    Queued,
    /// Leased by one worker; a crash here becomes `uncertain` after the lease window.
    Sending,
    /// Submitted and appended to Sent.
    Sent,
    /// Submitted, but the Sent append still has to succeed.
    AppendPending,
    /// The outcome of the submission is unknown. Never retried automatically.
    Uncertain,
    /// A permanent error or the attempt ceiling stopped the queue.
    Failed,
    /// Cancelled before submission.
    Cancelled,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::AppendPending => "append_pending",
            Self::Uncertain => "uncertain",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "sending" => Some(Self::Sending),
            "sent" => Some(Self::Sent),
            "append_pending" => Some(Self::AppendPending),
            "uncertain" => Some(Self::Uncertain),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

impl std::fmt::Display for OutboxState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One row of the submission journal.
///
/// `draft_id` is the local message this submission was built from. It is a real column, not an
/// envelope field, because the state machine needs it: "the newest row for this draft" decides
/// whether a queue is idempotent, and a partial unique index makes a second *active* row impossible.
/// `None` only on an imported or damaged row.
///
/// `envelope_json` is opaque to this crate: the engine writes the envelope, the subject, the local
/// draft id and the generated Message-ID there. It is encrypted with the rest of the database and is
/// never logged. `raw_blob_hash` names the exact RFC 5322 bytes in the blob store.
///
/// `lease_at` is the instant one worker claimed the row. It is the only lease epoch: `send_at` is
/// the user-visible due time and may be hours old while a live submission is still in `DATA`.
///
/// `append_attempts` counts Sent-append attempts separately from submission attempts: the two have
/// independent backoff budgets, and a failed append must never look like a failed submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    pub id: String,
    pub account_id: String,
    pub draft_id: Option<String>,
    pub idempotency_key: String,
    pub created_at: i64,
    pub send_at: i64,
    pub state: OutboxState,
    pub attempts: u32,
    pub append_attempts: u32,
    pub last_error: Option<String>,
    pub raw_blob_hash: String,
    pub envelope_json: String,
    pub lease_at: Option<i64>,
}

impl OutboxState {
    /// Whether this row still owns a draft: a new queue for the same draft is refused and the draft
    /// cannot be edited or moved. `Sent` is not active (the draft is already in Sent), and
    /// `Cancelled` is a discard.
    #[must_use]
    pub fn blocks_draft(self) -> bool {
        matches!(self, Self::Queued | Self::Sending | Self::AppendPending | Self::Uncertain | Self::Failed)
    }

    /// Whether a database row in this state prevents a second row for the same draft from being
    /// queued. Only a row that can still deliver the message counts.
    #[must_use]
    pub fn occupies_draft_slot(self) -> bool {
        matches!(self, Self::Queued | Self::Sending)
    }
}

/// The per-folder sync cursor.///
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
    pub account_id: Option<String>,
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
        Self { account_id: None, folder_id: None, label_id: None, unread: None, starred: None, has_attachments: None, limit: 50, offset: 0 }
    }
}

/// The order of a search page. Relevance is bm25 ascending; date is newest first. Both end in the
/// stable `(date DESC, id ASC)` tie-break so paging cannot repeat or skip a row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// bm25 rank, then newest first.
    #[default]
    Relevance,
    /// Newest first.
    Date,
}

/// One typed branch of a search filter.
///
/// The core crate owns the query language and translates its AST into this tree: it resolves a
/// label or folder name to an id and escapes every text term as an FTS5 expression. The store never
/// receives a fragment of SQL and never interpolates a caller string into a statement: `Match` is
/// bound as the value of `MATCH ?`, and every other variant becomes a fixed predicate with a bound
/// parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchExpr {
    And(Vec<SearchExpr>),
    Or(Vec<SearchExpr>),
    Not(Box<SearchExpr>),
    /// A complete, already-quoted FTS5 expression. It is always bound, never concatenated into SQL.
    Match(String),
    Folder(String),
    Label(String),
    Unread(bool),
    Starred(bool),
    HasAttachments(bool),
    /// Messages on or after this Unix-millisecond instant.
    After(i64),
    /// Messages strictly before this Unix-millisecond instant.
    Before(i64),
    /// RFC 822 size strictly larger than this many bytes.
    Larger(u64),
    /// RFC 822 size strictly smaller than this many bytes.
    Smaller(u64),
}

/// What to search, how to order the page and which accounts are in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFilter {
    /// One account, or every account when `None`. The engine refuses an ambiguous multi-account
    /// search before it reaches the store.
    pub account_id: Option<String>,
    /// `None` is a filter-only query: no FTS `MATCH` runs.
    pub expr: Option<SearchExpr>,
    pub sort: SearchSort,
    /// Clamped to 1..=200 by the repository.
    pub limit: u32,
    /// Clamped to 0..=5_000 by the repository.
    pub offset: u32,
}

impl Default for SearchFilter {
    fn default() -> Self {
        Self { account_id: None, expr: None, sort: SearchSort::Relevance, limit: 50, offset: 0 }
    }
}

/// The thread-list query. A scope filter keeps a thread only when at least one of its messages
/// matches; the counts on the row stay mailbox-wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFilter {
    pub account_id: Option<String>,
    pub folder_id: Option<String>,
    pub label_id: Option<String>,
    pub unread: Option<bool>,
    pub starred: Option<bool>,
    pub has_attachments: Option<bool>,
    pub limit: u32,
    pub offset: u32,
}

impl Default for ThreadFilter {
    fn default() -> Self {
        Self { account_id: None, folder_id: None, label_id: None, unread: None, starred: None, has_attachments: None, limit: 50, offset: 0 }
    }
}

/// One cached conversation. `message_count`, `unread_count` and `starred_count` are mailbox-wide;
/// a folder or label view only decides which threads are listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub account_id: String,
    pub id: String,
    pub subject: String,
    pub snippet: String,
    /// Unique senders, earliest first, at most [`MAX_THREAD_PARTICIPANTS`].
    pub participants: Vec<Address>,
    /// Unix milliseconds.
    pub last_date: i64,
    pub message_count: u32,
    pub unread_count: u32,
    pub starred_count: u32,
    pub has_attachments: bool,
    pub latest_message_id: Option<String>,
    /// Union of the labels carried by the thread's messages, at most [`MAX_THREAD_LABELS`].
    pub labels: Vec<String>,
    /// `unread` when any message is unread, otherwise `read`.
    pub state: String,
}

/// The largest participant list a thread row carries to the UI.
pub const MAX_THREAD_PARTICIPANTS: usize = 5;
/// The largest label union a thread row carries to the UI.
pub const MAX_THREAD_LABELS: usize = 8;
/// One thread page plus the totals for the scope before pagination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPage {
    pub threads: Vec<ThreadSummary>,
    pub total: u32,
    pub unread_threads: u32,
    /// Sum of the mailbox-wide unread counts of the listed threads.
    pub unread_messages: u32,
}

/// One thread with a page of its messages, ascending by date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadDetail {
    pub thread: ThreadSummary,
    pub messages: Vec<MessageSummary>,
    pub total: u32,
}

/// A saved search. `query` is the documented search language; `sort` is `relevance` or `date`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSearch {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub query: String,
    pub sort: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// A PGP key row exactly as it is inserted. `secret_armor` is passphrase-protected OpenPGP armor and
/// is only ever bound into the encrypted store; it is never selected back into a caller-facing value.
/// Debug redacts it so a stray log line cannot print key material.
pub struct PgpKeyInsert {
    pub id: String,
    pub account_id: String,
    pub fingerprint: String,
    pub key_id: String,
    pub user_id: Option<String>,
    pub public_armor: String,
    pub secret_armor: Option<Zeroizing<String>>,
    pub protected: bool,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

impl std::fmt::Debug for PgpKeyInsert {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgpKeyInsert")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("fingerprint", &self.fingerprint)
            .field("key_id", &self.key_id)
            .field("user_id", &self.user_id)
            .field("public_armor_len", &self.public_armor.len())
            .field("secret_armor", &self.secret_armor.as_ref().map(|_| "[redacted]"))
            .field("protected", &self.protected)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// The non-secret metadata of a stored PGP key, safe to return from a list operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgpKeySummary {
    pub id: String,
    pub account_id: String,
    pub fingerprint: String,
    pub key_id: String,
    pub user_id: Option<String>,
    pub protected: bool,
    pub has_secret: bool,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// The public half of a stored key, as returned by an export operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgpPublicKeyRecord {
    pub fingerprint: String,
    pub key_id: String,
    pub public_armor: String,
}

/// One search result row. `body_text` is a bounded prefix of the stored plain-text body (64 KiB at
/// most) and exists only so the engine can build a plain-text snippet in Rust; `body_html` is never
/// read by search. `rank` is `bm25` ascending and is internal to ordering.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub message: MessageSummary,
    pub body_text: String,
    pub rank: f64,
}

/// One search page plus the exact total up to the documented cap.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total: u32,
    /// True when more than [`SEARCH_TOTAL_CAP`] rows match, so `total` is a lower bound.
    pub total_capped: bool,
}

/// Exact totals are computed up to this many rows; beyond it the engine reports a capped total
/// instead of scanning a whole mailbox for a number nobody reads.
pub const SEARCH_TOTAL_CAP: u32 = 10_000;

/// The health of the FTS index, as persisted in `meta.search_index` and measured on read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchIndexStatus {
    /// `pending`, `building`, `ready`, `drift` or `error`.
    pub state: String,
    /// FTS documents present.
    pub indexed: u32,
    /// Message rows that should be indexed.
    pub total: u32,
    /// True when the state is `ready`, no FTS/rank row is missing or extra, and the counts agree.
    pub ready: bool,
    pub last_error: Option<String>,
    /// Size of the encrypted database file plus WAL and SHM in bytes. FTS has no sidecar; use
    /// `dbstat` when the index-only bytes are needed.
    pub size_bytes: u64,
    /// Unix milliseconds of the last index update (an FTS trigger or a build batch).
    pub updated_at: i64,
}

/// What one reindex run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchReindexOutcome {
    pub documents: u32,
    pub took_ms: u64,
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

/// Metadata of a draft, without its body. The engine keeps one per draft so a stale queue can be
/// refused even after the eight-entry body cache has evicted the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftMeta {
    pub id: String,
    pub account_id: String,
    pub revision: u32,
    /// Unix milliseconds from the `drafts` row.
    pub updated_at: i64,
    pub in_reply_to: Option<String>,
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

// ---------------------------------------------------------------------------------------------
// Rules (P9c)
// ---------------------------------------------------------------------------------------------

/// One stored rule. `actions_json` is the engine-owned JSON array of 1-5 actions; this crate never
/// interprets it, and the journal never stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub enabled: bool,
    /// Lower runs first.
    pub priority: i64,
    /// When true, later rules skip a message this rule matched.
    pub stop: bool,
    /// The documented search language, validated by core before it is stored.
    pub query: String,
    /// The engine's serialized action list. Never a subject, a body or a log line.
    pub actions_json: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// One journaled rule run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRun {
    pub id: String,
    pub account_id: String,
    /// Unix milliseconds.
    pub started_at: i64,
    /// Unix milliseconds; `None` while the run is in flight or after a crash.
    pub finished_at: Option<i64>,
    /// `new` or `all`.
    pub mode: String,
    pub rule_count: u32,
    /// Rule/message pairs the conditions matched.
    pub matched: u32,
    /// Actions that actually changed a message.
    pub changed: u32,
    pub errors: u32,
    /// True when `rules_undo` already reversed this run.
    pub undone: bool,
}

/// One journaled action. `value_json` carries ids, folders, labels and booleans only: never a
/// subject, an address, a query or a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleActionRecord {
    pub run_id: String,
    pub ordinal: u32,
    pub rule_id: String,
    pub message_id: String,
    /// `move`, `label`, `read`, `trash` or `forward`.
    pub action: String,
    pub value_json: String,
    pub changed: bool,
}

/// The received-mail candidate filter for one rule or for the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleCandidateFilter {
    pub account_id: String,
    /// The translated search expression; `None` matches every received message.
    pub expr: Option<SearchExpr>,
    /// Only messages ingested at or after this instant.
    pub since: i64,
    /// Only messages that no earlier `new` run has already applied.
    pub new_only: bool,
    /// Continue strictly after this `(ingested_at, id)` position. Used by the `all` cursor.
    pub after: Option<(i64, String)>,
    /// Clamped to 1..=[`MAX_RULE_CANDIDATES`].
    pub limit: u32,
}

/// One candidate page plus the exact total up to the documented cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleCandidatePage {
    pub messages: Vec<MessageSummary>,
    pub total: u32,
    pub total_capped: bool,
}

/// The largest candidate page a rule run or preview can request.
pub const MAX_RULE_CANDIDATES: u32 = 2_000;

/// The current folder, origin, read/star state and labels of one candidate. Undo compares against
/// this; actions re-check it before touching a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMessageState {
    pub folder_id: String,
    pub folder_kind: FolderKind,
    pub origin: MessageOrigin,
    pub unread: bool,
    pub starred: bool,
    pub labels: Vec<String>,
}

/// The loop guard persisted for a received message that carried `Auto-Submitted` or
/// `X-Postino-Rule`. Stored in the encrypted `meta` table, never as a message column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuleGuard {
    pub auto_submitted: bool,
    /// The incoming message already carries the header this client adds to rule forwards.
    pub x_postino_rule: bool,
}

impl RuleGuard {
    #[must_use]
    pub fn any(self) -> bool {
        self.auto_submitted || self.x_postino_rule
    }
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
