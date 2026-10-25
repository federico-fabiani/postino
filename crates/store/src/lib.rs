//! Encrypted local store.
//!
//! One SQLCipher database whose pages, WAL frames and temp store are encrypted with a 256-bit key
//! supplied by the caller. The key comes from the OS keyring in the app; this crate never reads a
//! keyring, a config file or the environment, which is also why tests cannot touch the real vault.

use std::fmt;
use std::path::Path;

use rusqlite::{Connection, ErrorCode, OptionalExtension};

/// Schema version this build understands. Bump together with [`MIGRATIONS`].
const SCHEMA_VERSION: u32 = 1;

/// Migrations applied in order: index `n - 1` moves `PRAGMA user_version` from `n - 1` to `n`.
const MIGRATIONS: &[&str] = &["CREATE TABLE meta(
         key   TEXT PRIMARY KEY,
         value TEXT NOT NULL
     ) WITHOUT ROWID;"];

/// A 256-bit store key.
///
/// `Debug` is redacted on purpose: a key must never reach a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct StoreKey([u8; 32]);

impl StoreKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Render the key as SQLCipher's raw-key pragma.
    ///
    /// SQLCipher reads the pragma when the statement is prepared, before bound parameters are
    /// available, so a bound `PRAGMA key = ?` is silently ignored on an existing file. The hex-only
    /// literal cannot need escaping.
    fn pragma(&self) -> String {
        use fmt::Write as _;
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            // Writing into a String is infallible.
            let _ = write!(hex, "{byte:02x}");
        }
        format!("PRAGMA key = \"x'{hex}'\";")
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
}

fn translate(error: rusqlite::Error) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::NotADatabase => StoreError::WrongKeyOrCorrupt,
        _ => StoreError::Sqlite(error),
    }
}

/// An open, migrated store. One connection in WAL mode.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open or create the database at `path` and apply pending migrations.
    pub fn open(path: &Path, key: &StoreKey) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(&key.pragma()).map_err(translate)?;
        // Fail closed if the codec is missing: without SQLCipher the pragma is a no-op and the file
        // would be written in plaintext.
        let cipher: Option<String> = conn.query_row("PRAGMA cipher_version", [], |row| row.get(0)).optional().map_err(translate)?;
        if cipher.as_deref().unwrap_or_default().is_empty() {
            return Err(StoreError::NotEncrypted);
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // `journal_mode = WAL` reads page 1, so on an existing file this is where a wrong key
        // surfaces as NotADatabase.
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )
        .map_err(translate)?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        // The first real read is also the key check: a wrong key fails here with NotADatabase.
        let current: u32 = self.conn.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(translate)?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew { found: current, supported: SCHEMA_VERSION });
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let version = index as u32 + 1;
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        Ok(())
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
        assert_eq!(key.pragma(), "PRAGMA key = \"x'0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f'\";");
    }
}
