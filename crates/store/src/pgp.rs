//! OpenPGP key rows (P12c1).
//!
//! A row carries the fingerprint, the public armor and, for a private key, the still
//! passphrase-protected secret armor. Both armors are bounded by the caller before they reach this
//! module. Signing and decryption need the secret armor, so [`Store::get_pgp_secret_armor`] returns
//! it only to the engine's crypto path; no operation lists, exports or logs it.
//!
//! The schema enforces one fingerprint per store and `protected` agreement with the presence of a
//! secret. The per-account cap is checked inside the same immediate transaction as the insert.

use rusqlite::{OptionalExtension, params};
use zeroize::Zeroizing;

use crate::model::{PgpKeyInsert, PgpKeySummary, PgpPublicKeyRecord};
use crate::{Store, StoreError};

const SUMMARY_COLUMNS: &str = "id, account_id, fingerprint, key_id, user_id, protected, secret_armor IS NOT NULL, created_at, updated_at";

impl Store {
    /// Key metadata for one account, oldest first. Secret armor is never selected.
    pub fn list_pgp_keys(&self, account_id: &str) -> Result<Vec<PgpKeySummary>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM pgp_keys WHERE account_id = ?1 ORDER BY created_at ASC, id ASC"))?;
        let rows = stmt.query_map([account_id], row_to_summary)?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row?);
        }
        Ok(keys)
    }

    /// One key's metadata, scoped to the account that owns it.
    pub fn get_pgp_key(&self, account_id: &str, id: &str) -> Result<Option<PgpKeySummary>, StoreError> {
        self.conn
            .query_row(&format!("SELECT {SUMMARY_COLUMNS} FROM pgp_keys WHERE account_id = ?1 AND id = ?2"), params![account_id, id], row_to_summary)
            .optional()
            .map_err(StoreError::from)
    }

    pub fn count_pgp_keys(&self, account_id: &str) -> Result<u32, StoreError> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM pgp_keys WHERE account_id = ?1", [account_id], |row| row.get(0))?;
        crate::repo::to_count(count)
    }

    /// Allocate a store-wide monotonic key id (`pgp_<n>`). A meta high-water mark is seeded once from
    /// existing rows, so a deleted id is never handed out again to a different row.
    pub fn allocate_pgp_key_id(&self) -> Result<String, StoreError> {
        const KEY: &str = "pgp_key_seq";
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", [KEY], |row| row.get(0)).optional()?;
        let stored = stored.and_then(|raw| raw.parse::<i64>().ok()).unwrap_or(0);
        let highest: Option<i64> =
            tx.query_row("SELECT MAX(CAST(substr(id, 5) AS INTEGER)) FROM pgp_keys WHERE id GLOB 'pgp_[0-9]*'", [], |row| row.get(0))?;
        let next = stored.max(highest.unwrap_or(0)).saturating_add(1);
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![KEY, next.to_string()],
        )?;
        tx.commit()?;
        Ok(format!("pgp_{next}"))
    }

    /// Insert one key atomically, refusing a duplicate fingerprint or an account over `limit`.
    pub fn insert_pgp_key_limited(&self, key: &PgpKeyInsert, limit: u32) -> Result<(), StoreError> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM pgp_keys WHERE account_id = ?1", [&key.account_id], |row| row.get(0))?;
        let count = crate::repo::to_count(count)?;
        if count >= limit {
            return Err(StoreError::PgpKeyLimit { limit });
        }
        let secret = key.secret_armor.as_deref().map(|armor| armor.as_str());
        let result = tx.execute(
            "INSERT INTO pgp_keys(id, account_id, fingerprint, key_id, user_id, public_armor, secret_armor, protected, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                key.id,
                key.account_id,
                key.fingerprint,
                key.key_id,
                key.user_id,
                key.public_armor,
                secret,
                key.protected,
                key.created_at,
                key.updated_at
            ],
        );
        if let Err(error) = result {
            if is_fingerprint_conflict(&error) {
                return Err(StoreError::DuplicatePgpFingerprint { fingerprint: key.fingerprint.clone() });
            }
            return Err(StoreError::from(error));
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete one key scoped to its account. Returns whether a row was removed.
    pub fn delete_pgp_key(&self, account_id: &str, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute("DELETE FROM pgp_keys WHERE account_id = ?1 AND id = ?2", params![account_id, id])? == 1)
    }

    /// The public half of one key, scoped to its account. Never returns secret armor.
    pub fn export_pgp_public(&self, account_id: &str, id: &str) -> Result<Option<PgpPublicKeyRecord>, StoreError> {
        self.conn
            .query_row("SELECT fingerprint, key_id, public_armor FROM pgp_keys WHERE account_id = ?1 AND id = ?2", params![account_id, id], |row| {
                Ok(PgpPublicKeyRecord { fingerprint: row.get(0)?, key_id: row.get(1)?, public_armor: row.get(2)? })
            })
            .optional()
            .map_err(StoreError::from)
    }

    /// The public armor of one key, for verification and recipient encryption.
    pub fn get_pgp_public_armor(&self, account_id: &str, id: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row("SELECT public_armor FROM pgp_keys WHERE account_id = ?1 AND id = ?2", params![account_id, id], |row| row.get(0))
            .optional()
            .map_err(StoreError::from)
    }

    /// The still-protected secret armor of one key, for the engine's sign/decrypt path only.
    ///
    /// The result is zeroised on drop. Callers must not serialize it, log it or return it from an
    /// operation: no operation here does.
    pub fn get_pgp_secret_armor(&self, account_id: &str, id: &str) -> Result<Option<Zeroizing<String>>, StoreError> {
        let armor: Option<String> = self
            .conn
            .query_row("SELECT secret_armor FROM pgp_keys WHERE account_id = ?1 AND id = ?2", params![account_id, id], |row| row.get(0))
            .optional()
            .map_err(StoreError::from)?;
        Ok(armor.map(Zeroizing::new))
    }
}

fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<PgpKeySummary> {
    Ok(PgpKeySummary {
        id: row.get(0)?,
        account_id: row.get(1)?,
        fingerprint: row.get(2)?,
        key_id: row.get(3)?,
        user_id: row.get(4)?,
        protected: row.get(5)?,
        has_secret: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn is_fingerprint_conflict(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, Some(message))
            if failure.code == rusqlite::ErrorCode::ConstraintViolation && message.contains("pgp_keys.fingerprint")
    )
}
