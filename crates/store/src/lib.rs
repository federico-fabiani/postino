//! Encrypted local store.
//!
//! One SQLCipher database whose pages, WAL frames and temp store are encrypted with a 256-bit key
//! supplied by the caller. The key comes from the OS keyring in the app (`postino-platform`, under a
//! scope the app chooses); this crate never reads a keyring, a config file or the environment, which
//! is also why tests cannot touch the real vault.
//!
//! # Schema and dates
//!
//! `PRAGMA user_version` names the schema version; `MIGRATIONS` holds the ordered, additive
//! migrations. Version 2 is the M1 mailbox schema: accounts, folders, messages, attachments, labels,
//! message_labels, drafts, sync_state and the P3a `meta` table. Version 3 adds `draft_seq`, the
//! monotonic draft revision counter that survives a send and a move back to Drafts. Version 4 adds
//! `account_servers`, the IMAP endpoint of each account, with no password column anywhere. Version 5
//! adds the nullable SMTP endpoint columns and the `outbox` journal (P7). Version 6 adds the stored
//! remote-image count of each message (P6d), the count the MIME sanitizer removed on receipt. Version
//! 7 adds the derived search columns, the contentless-delete `message_fts` index and its triggers, and
//! the still-inert P9b/P9c tables (threads, aliases, saved searches and rules).
//!
//! Every date is a **Unix timestamp in milliseconds** (`i64`). The choice is deliberate:
//!
//! - it is the same unit as JavaScript, `SystemTime` arithmetic and the existing demo clock, so a
//!   client never has to guess between seconds and milliseconds;
//! - it is timezone-free, sortable as an integer and needs no formatting or locale in SQL;
//! - integer comparisons keep the `(folder_id, date DESC)` indexes usable, which a text timestamp
//!   would not;
//! - 64 bits cover ±292 million years, so there is no year-2038 cliff.
//!
//! IMAP `INTERNALDATE` and MIME `Date` are converted at the sync boundary; the store never sees a
//! string date. `NULL` is only used where the source may genuinely not know (`last_sync_at`,
//! `uid_validity`, `uid_next`), never as "zero".
//!
//! # Transactions
//!
//! Every repository mutation that touches more than one row runs in one transaction: label batch
//! apply/remove, label delete plus detach, attachment replacement, message move, draft save and the
//! flag batch. A draft save writes the display message, its attachments, the revision row and the
//! folder counts together, so a failure leaves no half-saved draft.
//!
//! # Blobs
//!
//! Attachment bytes are not rows. `Store::open` also opens `<db file>.blobs` (for `mail.db`, that is
//! `mail.db.blobs`), a content-addressed directory of XChaCha20-Poly1305 files sealed with an HKDF
//! subkey of the same store key (see [`BlobStore`] and `docs/architecture.md` §4.5). The database
//! keeps only the hash, and the hash is computed over the plaintext, so identical attachments are
//! stored once. The directory belongs to the file, so two stores in one folder never share it.

mod blob;
mod model;
mod repo;
mod saved;
mod search;
mod threads;

pub use blob::BlobStore;
pub use model::{
    Account, AccountServer, AccountTls, Address, Attachment, Draft, DraftMeta, DraftUpdate, FlagUpdate, Folder, FolderKind, FolderMessageFlags,
    Label, LabelCount, MAX_REMOTE_IMAGES, Message, MessageCounts, MessageFilter, MessageFlags, MessagePage, MessageSummary, OutboxEntry, OutboxState,
    SEARCH_TOTAL_CAP, SavedSearch, SearchExpr, SearchFilter, SearchHit, SearchIndexStatus, SearchPage, SearchReindexOutcome, SearchSort, SmtpServer,
    SyncState, ThreadDetail, ThreadFilter, ThreadPage, ThreadSummary, UidEntry,
};
pub use repo::{MAX_OUTBOX_ROWS, UNCERTAIN_AFTER_CRASH};
pub use saved::RECENT_SEARCHES_KEY_PREFIX;

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension};
use zeroize::{Zeroize, Zeroizing};

/// Schema version this build understands. Bump together with [`MIGRATIONS`].
const SCHEMA_VERSION: u32 = 7;

/// `meta` key that stores the highest label number ever allocated. It only ever grows, so a deleted
/// label id is never handed out again.
pub const LABEL_SEQ_KEY: &str = "label_seq";

/// Prefix of the `meta` key that keeps a received message's `Reply-To` list.
///
/// P8 needs the header for reply/reply-all, but the shipped message schema has no `reply_to`
/// column and this package must not migrate. The value is the same JSON address list the message
/// tables already use, keyed by local message id; P9 widens the schema and can move it into a
/// column. The key is removed with its message.
pub const REPLY_TO_KEY_PREFIX: &str = "reply_to:";

/// Prefix of the `meta` key that holds one account's persisted sync status. The account id follows.
/// `postino-sync` writes the JSON value; `Store::delete_account` removes it with the account.
pub const SYNC_STATUS_KEY_PREFIX: &str = "sync_status:";

/// Migrations applied in order: index `n - 1` moves `PRAGMA user_version` from `n - 1` to `n`.
///
/// Never edit a shipped migration: an existing store already ran it. Add a new entry instead.
const MIGRATIONS: &[&str] = &[
    // v1 (P3a): the proof only needed a key/value table.
    "CREATE TABLE meta(
         key   TEXT PRIMARY KEY,
         value TEXT NOT NULL
     ) WITHOUT ROWID;",
    // v2 (P3b): the M1 mailbox schema.
    r#"
CREATE TABLE accounts(
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    email      TEXT NOT NULL,
    provider   TEXT NOT NULL,
    signature  TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);

CREATE TABLE folders(
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    unread       INTEGER NOT NULL DEFAULT 0 CHECK(unread >= 0),
    total        INTEGER NOT NULL DEFAULT 0 CHECK(total >= 0),
    uid_validity INTEGER NULL,
    uid_next     INTEGER NULL,
    last_sync_at INTEGER NULL
);
CREATE INDEX folders_account ON folders(account_id);

CREATE TABLE messages(
    id              TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id       TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid             INTEGER NULL,
    message_id      TEXT NULL,
    thread_id       TEXT NOT NULL,
    from_name       TEXT NOT NULL,
    from_email      TEXT NOT NULL,
    to_json         TEXT NOT NULL,
    cc_json         TEXT NOT NULL,
    bcc_json        TEXT NOT NULL,
    subject         TEXT NOT NULL,
    snippet         TEXT NOT NULL,
    date            INTEGER NOT NULL,
    unread          INTEGER NOT NULL DEFAULT 0 CHECK(unread IN (0, 1)),
    starred         INTEGER NOT NULL DEFAULT 0 CHECK(starred IN (0, 1)),
    has_attachments INTEGER NOT NULL DEFAULT 0 CHECK(has_attachments IN (0, 1)),
    body_text       TEXT NOT NULL,
    body_html       TEXT NULL,
    size            INTEGER NULL
);
CREATE INDEX messages_folder_date ON messages(folder_id, date DESC);
CREATE INDEX messages_account_date ON messages(account_id, date DESC);

CREATE TABLE attachments(
    id         TEXT PRIMARY KEY,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename   TEXT NOT NULL,
    mime       TEXT NOT NULL,
    size       INTEGER NOT NULL CHECK(size >= 0),
    blob_hash  TEXT NULL
);
CREATE INDEX attachments_message ON attachments(message_id);

CREATE TABLE labels(
    id         TEXT PRIMARY KEY,
    account_id TEXT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    color      TEXT NOT NULL,
    UNIQUE(account_id, name)
);
-- SQLite treats NULLs as distinct in UNIQUE(account_id, name), so the table constraint alone would
-- allow duplicate mailbox-wide label names. This partial index closes that hole.
CREATE UNIQUE INDEX labels_global_name ON labels(name) WHERE account_id IS NULL;

CREATE TABLE message_labels(
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    label_id   TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY(message_id, label_id)
) WITHOUT ROWID;
CREATE INDEX message_labels_label ON message_labels(label_id);

CREATE TABLE drafts(
    id          TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    revision    INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1),
    updated_at  INTEGER NOT NULL,
    body_text   TEXT NOT NULL,
    to_json     TEXT NOT NULL,
    cc_json     TEXT NOT NULL,
    bcc_json    TEXT NOT NULL,
    subject     TEXT NOT NULL,
    in_reply_to TEXT NULL
);
CREATE INDEX drafts_updated ON drafts(updated_at DESC);

CREATE TABLE sync_state(
    folder_id      TEXT PRIMARY KEY REFERENCES folders(id) ON DELETE CASCADE,
    uid_next       INTEGER NOT NULL,
    highest_modseq INTEGER NULL,
    last_error     TEXT NULL
) WITHOUT ROWID;
"#,
    // v3 (P3d): the revision counter is independent from the draft row, so sending a draft and
    // moving it back to Drafts cannot restart the sequence at 1. Existing revisions seed the table.
    r#"
CREATE TABLE draft_seq(
    message_id TEXT PRIMARY KEY,
    revision   INTEGER NOT NULL CHECK(revision >= 1)
);

INSERT INTO draft_seq(message_id, revision) SELECT id, revision FROM drafts;
"#,
    // v4 (P6a): where an account's mail comes from. The credential is deliberately absent: it lives
    // in the OS keyring (scope `account:<id>:imap`), never in the encrypted database.
    r#"
CREATE TABLE account_servers(
    account_id    TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    imap_host     TEXT NOT NULL,
    imap_port     INTEGER NOT NULL CHECK(imap_port BETWEEN 1 AND 65535),
    imap_tls      TEXT NOT NULL CHECK(imap_tls IN ('implicit', 'starttls')),
    imap_username TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
"#,
    // v5 (P7): SMTP submission endpoints and the outbox journal. The four SMTP columns are nullable
    // because an existing account has no endpoint until it is re-added or edited; an account without
    // one can still read mail and refuses to send with an actionable sentence. No password column.
    //
    // `draft_id` names the local draft the submission was built from. The partial unique index makes
    // a second *active* row for one draft impossible: `queued`, `sending`, `append_pending`,
    // `uncertain` and `failed` all still own the draft (a failed row must be discarded before a new
    // queue, an uncertain row is a possible delivery), so a cancelled or sent row is the only way the
    // slot frees up. `idempotency_key` is stable per draft revision and content hash, so the same
    // queue request from two processes is one row, not two.
    //
    // `lease_at` is the lease epoch, written when a worker claims the row (submission or Sent
    // append). `send_at` is the user-visible due time: a row scheduled for 09:00 and leased at 10:00
    // is fresh for the whole lease window because the window is measured from `lease_at`, never from
    // `send_at`.
    r#"
ALTER TABLE account_servers ADD COLUMN smtp_host TEXT;
ALTER TABLE account_servers ADD COLUMN smtp_port INTEGER;
ALTER TABLE account_servers ADD COLUMN smtp_tls TEXT;
ALTER TABLE account_servers ADD COLUMN smtp_username TEXT;
CREATE TABLE outbox(
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT,                    -- the local draft this submission came from
    idempotency_key TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    send_at INTEGER NOT NULL,         -- user-visible due time
    state TEXT NOT NULL,              -- queued | sending | sent | append_pending | uncertain | failed | cancelled
    attempts INTEGER NOT NULL DEFAULT 0,
    append_attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    raw_blob_hash TEXT NOT NULL,
    envelope_json TEXT NOT NULL,
    lease_at INTEGER                  -- written when a worker claims the row
);
CREATE INDEX outbox_due ON outbox(state, send_at);
CREATE INDEX outbox_draft ON outbox(draft_id, created_at DESC);
CREATE UNIQUE INDEX outbox_active_draft ON outbox(draft_id)
    WHERE draft_id IS NOT NULL AND state IN ('queued', 'sending', 'append_pending', 'uncertain', 'failed');
"#,
    // v6 (P6d): the number of remote images the MIME sanitizer removed from the stored HTML. It is
    // additive on purpose: an existing row keeps working with the `0` default, and the count is a
    // read-only derived fact, never accepted from a draft or an agent. The CHECK bounds a hostile or
    // damaged value at the storage boundary; the repository clamps at both ends as well.
    r#"
ALTER TABLE messages ADD COLUMN remote_images INTEGER NOT NULL DEFAULT 0
    CHECK(remote_images BETWEEN 0 AND 10000);
"#,
    // v7 (P9a): the search columns, the FTS5 index and the inert P9b/P9c tables.
    //
    // The indexed text is derived by the repository from data it already receives: sender and
    // recipient display strings, attachment names and a normalised subject. The triggers copy those
    // columns into a contentless-delete FTS5 table keyed by `messages.rowid`; a flag, folder or date
    // update does not name an indexed column and therefore never reindexes. Body text is bounded to
    // 262144 characters, which is at most 1 MiB of UTF-8, so a hostile body cannot make the index
    // grow without bound.
    //
    // The probe (crates/store/tests/fts_probe.rs) proves the bundled SQLCipher 4.19 / SQLite 3.53.4
    // accepts `contentless_delete=1`; delete+insert is the supported update form, which is exactly
    // what the update trigger does. Snippets are computed in Rust from the plain body, so the index
    // stores no content and can never return an HTML marker to the UI.
    //
    // `threads`, `thread_aliases`, `saved_searches`, `rules` and the rule journal are created empty
    // and inert so P9b/P9c need no second migration. Nothing in P9a reads or writes them.
    r#"
ALTER TABLE messages ADD COLUMN search_sender TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN search_to TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN search_cc TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN search_bcc TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN attachment_names TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN subject_norm TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN in_reply_to TEXT NULL;
ALTER TABLE messages ADD COLUMN references_json TEXT NULL;
ALTER TABLE messages ADD COLUMN ingested_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN rules_applied_at INTEGER NULL;

CREATE INDEX messages_account_thread_date ON messages(account_id, thread_id, date DESC);
-- A covering list index: the mailbox-open query reads summary columns only and never touches the
-- 2 KiB body pages that sit in the table row. It is derived data, like the FTS index.
CREATE INDEX messages_list_cover ON messages(
    date DESC, id, folder_id, thread_id, account_id, from_name, from_email, to_json,
    subject, snippet, unread, starred, has_attachments
);

CREATE VIRTUAL TABLE message_fts USING fts5(
    subject,
    search_sender,
    search_to,
    search_cc,
    search_bcc,
    body_text,
    attachment_names,
    content='',
    contentless_delete=1,
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3'
);

CREATE TABLE message_fts_rank(
    rowid    INTEGER PRIMARY KEY,
    date_key INTEGER NOT NULL,
    id_key   TEXT NOT NULL
);

CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO message_fts(rowid, subject, search_sender, search_to, search_cc, search_bcc, body_text, attachment_names)
    VALUES (new.rowid, new.subject, new.search_sender, new.search_to, new.search_cc, new.search_bcc,
            substr(new.body_text, 1, 262144), new.attachment_names);
    INSERT INTO message_fts_rank(rowid, date_key, id_key) VALUES (new.rowid, new.date, new.id)
    ON CONFLICT(rowid) DO UPDATE SET date_key = excluded.date_key, id_key = excluded.id_key;
    INSERT INTO meta(key, value) VALUES('search_updated_at', CAST(strftime('%s','now') AS INTEGER) * 1000)
    ON CONFLICT(key) DO UPDATE SET value = excluded.value;
END;

CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    DELETE FROM message_fts WHERE rowid = old.rowid;
    DELETE FROM message_fts_rank WHERE rowid = old.rowid;
    INSERT INTO meta(key, value) VALUES('search_updated_at', CAST(strftime('%s','now') AS INTEGER) * 1000)
    ON CONFLICT(key) DO UPDATE SET value = excluded.value;
END;

CREATE TRIGGER messages_fts_update AFTER UPDATE OF subject, search_sender, search_to, search_cc,
    search_bcc, body_text, attachment_names, date, id ON messages BEGIN
    DELETE FROM message_fts WHERE rowid = old.rowid;
    INSERT INTO message_fts(rowid, subject, search_sender, search_to, search_cc, search_bcc, body_text, attachment_names)
    VALUES (new.rowid, new.subject, new.search_sender, new.search_to, new.search_cc, new.search_bcc,
            substr(new.body_text, 1, 262144), new.attachment_names);
    INSERT INTO message_fts_rank(rowid, date_key, id_key) VALUES (new.rowid, new.date, new.id)
    ON CONFLICT(rowid) DO UPDATE SET date_key = excluded.date_key, id_key = excluded.id_key;
    INSERT INTO meta(key, value) VALUES('search_updated_at', CAST(strftime('%s','now') AS INTEGER) * 1000)
    ON CONFLICT(key) DO UPDATE SET value = excluded.value;
END;

CREATE TABLE threads(
    account_id        TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    id                TEXT NOT NULL,
    subject           TEXT NOT NULL DEFAULT '',
    last_date         INTEGER NOT NULL DEFAULT 0,
    message_count     INTEGER NOT NULL DEFAULT 0,
    unread_count      INTEGER NOT NULL DEFAULT 0,
    starred_count     INTEGER NOT NULL DEFAULT 0,
    has_attachments   INTEGER NOT NULL DEFAULT 0,
    latest_message_id TEXT NULL,
    PRIMARY KEY(account_id, id)
);

CREATE TABLE thread_aliases(
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    old_id     TEXT NOT NULL,
    new_id     TEXT NOT NULL,
    PRIMARY KEY(account_id, old_id)
);

CREATE TABLE saved_searches(
    id         TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    query      TEXT NOT NULL,
    sort       TEXT NOT NULL DEFAULT 'relevance',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX saved_searches_name ON saved_searches(account_id, name COLLATE NOCASE);

CREATE TABLE rules(
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1,
    priority     INTEGER NOT NULL DEFAULT 0,
    stop         INTEGER NOT NULL DEFAULT 0,
    query        TEXT NOT NULL,
    actions_json TEXT NOT NULL DEFAULT '[]',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX rules_priority ON rules(account_id, priority, id);

CREATE TABLE rule_runs(
    id          TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER NULL,
    dry         INTEGER NOT NULL DEFAULT 0,
    rule_count  INTEGER NOT NULL DEFAULT 0,
    matched     INTEGER NOT NULL DEFAULT 0,
    changed     INTEGER NOT NULL DEFAULT 0,
    errors      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX rule_runs_recent ON rule_runs(account_id, started_at DESC);

CREATE TABLE rule_actions(
    run_id      TEXT NOT NULL REFERENCES rule_runs(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    rule_id     TEXT NOT NULL,
    message_id  TEXT NOT NULL,
    action      TEXT NOT NULL,
    value_json  TEXT NOT NULL DEFAULT 'null',
    changed     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(run_id, ordinal)
);

INSERT INTO meta(key, value)
SELECT 'search_index',
       '{"state":"pending","last_rowid":0,"indexed":0,"total":' || count(*) || ',"updated_at":0,"last_error":null}'
FROM messages
WHERE 1
ON CONFLICT(key) DO UPDATE SET value = excluded.value;
"#,
];

/// A 256-bit store key.
///
/// `Debug` is redacted on purpose and the bytes are zeroised on drop: a key must never reach a log
/// line or stay in freed memory.
#[derive(PartialEq, Eq)]
pub struct StoreKey([u8; 32]);

impl StoreKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes, so a caller can hand them to another component without a formatted copy.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Render the key as SQLCipher's raw-key pragma.
    ///
    /// SQLCipher reads the pragma when the statement is prepared, before bound parameters are
    /// available, so a bound `PRAGMA key = ?` is silently ignored on an existing file. The hex-only
    /// literal cannot need escaping. The buffer is sized for the exact final length, so the key is
    /// never copied into a larger, unzeroised allocation; the string is zeroised when dropped.
    fn pragma(&self) -> Zeroizing<String> {
        use fmt::Write as _;
        const PREFIX: &str = "PRAGMA key = \"x'";
        const SUFFIX: &str = "'\";";
        let mut sql = Zeroizing::new(String::with_capacity(PREFIX.len() + self.0.len() * 2 + SUFFIX.len()));
        sql.push_str(PREFIX);
        for byte in &self.0 {
            // Writing into a String is infallible, and the buffer has room for every byte.
            let _ = write!(sql, "{byte:02x}");
        }
        sql.push_str(SUFFIX);
        sql
    }
}

impl Drop for StoreKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreKey(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("store is not encrypted: this build of SQLite has no codec")]
    NotEncrypted,
    #[error("store cannot be opened: wrong key or damaged file")]
    WrongKeyOrCorrupt,
    #[error("store schema version {found} is newer than this build ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("store schema version {found} is older than this build ({supported}); open it read-write once to migrate")]
    SchemaTooOld { found: u32, supported: u32 },
    #[error("{entity} {id:?} does not exist")]
    NotFound { entity: &'static str, id: String },
    #[error("folder {folder_id:?} belongs to account {folder_account:?}, not {message_account:?}")]
    AccountMismatch { folder_id: String, message_account: String, folder_account: String },
    #[error("{entity} {name:?} already exists")]
    Duplicate { entity: &'static str, name: String },
    #[error("attachment id {id:?} is repeated in the same draft; every attachment needs a unique id")]
    DuplicateAttachment { id: String },
    #[error("attachment id {id:?} belongs to message {owner:?}; omit the id so the engine assigns a new one")]
    AttachmentOwnedByOther { id: String, owner: String },
    #[error("outbox already holds a {state} row for draft {draft_id:?}")]
    OutboxAlreadyActive { draft_id: String, state: OutboxState },
    #[error("store row in {table} is not readable: {detail}")]
    Corrupt { table: &'static str, detail: String },
    #[error("store value cannot be encoded: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("{field} does not fit the store's 64-bit integer range")]
    OutOfRange { field: &'static str },
    #[error("a blob id must be exactly 64 lowercase hex characters; this one is {length} characters long")]
    BadHash { length: usize },
    #[error("blob {hash} is not in the blob store")]
    BlobMissing { hash: String },
    #[error("blob {hash} is corrupt: {detail}")]
    CorruptBlob { hash: String, detail: &'static str },
    #[error("a blob declares {declared} bytes but this read allows {allowed}; it was not decrypted")]
    BlobTooLarge { declared: u64, allowed: u64 },
    #[error("the blob directory {path} exists but is not a directory")]
    BlobRoot { path: String },
    #[error("could not create a temporary file in {path}")]
    TempName { path: String },
    #[error("blob store I/O failed at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("blob compression failed: {0}")]
    Compression(#[source] std::io::Error),
    #[error("the OS random source failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("blob cryptography failed: {0}")]
    Crypto(#[from] postino_crypto::CryptoError),
}

/// The wall clock in Unix milliseconds for derived write timestamps (`ingested_at`).
///
/// A clock before the epoch is stored as `0` rather than failing a mail write; rules compare
/// `ingested_at` with their own creation instant, and `0` means "before any rule".
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
}

fn translate(error: rusqlite::Error) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::NotADatabase => StoreError::WrongKeyOrCorrupt,
        _ => StoreError::Sqlite(error),
    }
}

/// An open, migrated store. One connection in WAL mode plus the blob directory named after the
/// database file; the daemon will own both in M4.
pub struct Store {
    conn: Connection,
    blobs: BlobStore,
    /// The database file this connection owns, for a truthful size report. FTS5 shadow tables live
    /// in this file; there is no index sidecar.
    path: PathBuf,
}

/// The blob directory that belongs to a database file: `mail.db` → `mail.db.blobs`.
///
/// Deriving from the file, not from its folder, means two databases in one directory never share a
/// blob namespace. Moving a store means moving the database file and its `.blobs` directory
/// together; renaming only one of them hides the attachments (the store then starts an empty blob
/// directory and the old rows read as `BlobMissing`).
pub fn blob_root(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    let mut name = path.file_name().map_or_else(|| OsString::from("store"), std::ffi::OsStr::to_os_string);
    name.push(".blobs");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

impl Store {
    /// Open or create the database at `path`, apply pending migrations and open its blob directory.
    ///
    /// The database has to be readable before the blob directory is created, so a wrong key cannot
    /// leave a stray `mail.db.blobs/` next to somebody else's database. A blob directory that cannot
    /// be created fails the open: losing attachments later is worse than failing now.
    pub fn open(path: &Path, key: &StoreKey) -> Result<Self, StoreError> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch(&key.pragma()).map_err(translate)?;
        // Fail closed if the codec is missing: without SQLCipher the pragma is a no-op and the file
        // would be written in plaintext.
        let cipher: Option<String> = conn.query_row("PRAGMA cipher_version", [], |row| row.get(0)).optional().map_err(translate)?;
        if cipher.as_deref().unwrap_or_default().is_empty() {
            return Err(StoreError::NotEncrypted);
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // One process owns the store, so every transaction takes the write lock up front. That makes
        // `unchecked_transaction` (which reads this setting) `BEGIN IMMEDIATE` for the whole draft
        // save instead of a deferred transaction that can fail halfway through.
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
        // `journal_mode = WAL` reads page 1, so on an existing file this is where a wrong key
        // surfaces as NotADatabase.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )
        .map_err(translate)?;
        migrate(&mut conn)?;
        // The thread cache is derived data. Its page is ordered by `last_date` and scoped by account;
        // without this index a 100k mailbox sorts the whole cache on every list. This is cache
        // maintenance, not a schema migration: `user_version` stays 7 and an existing store gains
        // the index on its next normal open. A read-only open never creates it.
        conn.execute_batch("CREATE INDEX IF NOT EXISTS threads_account_date ON threads(account_id, last_date DESC, id);").map_err(translate)?;
        let blobs = BlobStore::open(blob_root(path), key)?;
        Ok(Self { conn, blobs, path: path.to_path_buf() })
    }

    /// The encrypted attachment blob store that lives next to this database.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Open an existing, already-migrated store **without** write access.
    ///
    /// This is the seam for read-only commands and for deterministic write-failure tests: the
    /// connection is opened with SQLite's read-only flag, the key is applied, the cipher version and
    /// schema version are checked, and every write then fails with `SQLITE_READONLY`. It never runs a
    /// migration or a `PRAGMA` that writes; an old schema is [`StoreError::SchemaTooOld`] and must be
    /// opened read-write once. The blob directory next to the database is opened through the normal
    /// blob seam (which may create it if it is missing), but no blob is written by this call.
    pub fn open_read_only(path: &Path, key: &StoreKey) -> Result<Self, StoreError> {
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.execute_batch(&key.pragma()).map_err(translate)?;
        let cipher: Option<String> = conn.query_row("PRAGMA cipher_version", [], |row| row.get(0)).optional().map_err(translate)?;
        if cipher.as_deref().unwrap_or_default().is_empty() {
            return Err(StoreError::NotEncrypted);
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
        conn.execute_batch("PRAGMA foreign_keys = ON;").map_err(translate)?;
        let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(translate)?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew { found: version, supported: SCHEMA_VERSION });
        }
        if version < SCHEMA_VERSION {
            return Err(StoreError::SchemaTooOld { found: version, supported: SCHEMA_VERSION });
        }
        let blobs = BlobStore::open(blob_root(path), key)?;
        Ok(Self { conn, blobs, path: path.to_path_buf() })
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self.conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    pub fn set_meta(&self, name: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (name, value),
        )?;
        Ok(())
    }

    pub fn get_meta(&self, name: &str) -> Result<Option<String>, StoreError> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [name], |row| row.get(0)).optional()?)
    }
}

/// Apply every pending migration, one transaction each.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    // The first real read is also the key check: a wrong key fails here with NotADatabase.
    let current: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(translate)?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew { found: current, supported: SCHEMA_VERSION });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as u32 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_key() {
        let key = StoreKey::from_bytes([0xab; 32]);
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "StoreKey(<redacted>)");
        assert!(!rendered.contains("ab"));
    }

    #[test]
    fn pragma_uses_the_raw_key_form() {
        let key = StoreKey::from_bytes([0x0f; 32]);
        assert_eq!(key.pragma().as_str(), "PRAGMA key = \"x'0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f'\";");
    }

    #[test]
    fn pragma_fits_its_allocation_exactly() {
        let key = StoreKey::from_bytes([0xab; 32]);
        let sql = key.pragma();
        assert_eq!(sql.capacity(), sql.len(), "the pragma buffer reallocated and left an unzeroised copy behind");
        assert_eq!(sql.len(), 83);
    }

    #[test]
    fn schema_version_matches_the_migration_list() {
        assert_eq!(MIGRATIONS.len(), SCHEMA_VERSION as usize, "bump the schema version and append a migration together");
    }
}
