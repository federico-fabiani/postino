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
//! `account_servers`, the IMAP endpoint of each account, with no password column anywhere.
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

pub use blob::BlobStore;
pub use model::{
    Account, AccountServer, AccountTls, Address, Attachment, Draft, DraftUpdate, FlagUpdate, Folder, FolderKind, FolderMessageFlags, Label,
    LabelCount, Message, MessageCounts, MessageFilter, MessageFlags, MessagePage, MessageSummary, SyncState, UidEntry,
};

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode, OptionalExtension};
use zeroize::{Zeroize, Zeroizing};

/// Schema version this build understands. Bump together with [`MIGRATIONS`].
const SCHEMA_VERSION: u32 = 4;

/// `meta` key that stores the highest label number ever allocated. It only ever grows, so a deleted
/// label id is never handed out again.
pub const LABEL_SEQ_KEY: &str = "label_seq";

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
        let blobs = BlobStore::open(blob_root(path), key)?;
        Ok(Self { conn, blobs })
    }

    /// The encrypted attachment blob store that lives next to this database.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
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
