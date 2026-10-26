//! Saved searches and the recent-query list.
//!
//! A saved search belongs to one account and stores the documented query language, not a compiled
//! plan: a label or folder that disappears later makes the row visibly stale instead of silently
//! returning the wrong mail. The database enforces `UNIQUE(account_id, name COLLATE NOCASE)`; the
//! engine enforces the per-account cap before it writes.
//!
//! Recent queries live in the encrypted `meta` table under `recent_searches:<account>` as a bounded
//! JSON array. They are the only history this feature keeps, they never leave the encrypted store
//! and the explicit clear command removes them.

use rusqlite::{OptionalExtension, params};

use crate::model::SavedSearch;
use crate::repo::duplicate;
use crate::{Store, StoreError};

/// Prefix of the `meta` key holding one account's recent queries.
pub const RECENT_SEARCHES_KEY_PREFIX: &str = "recent_searches:";

const SAVED_COLUMNS: &str = "id, account_id, name, query, sort, created_at, updated_at";

impl Store {
    /// Saved searches, by name ignoring case, for one account or every account.
    pub fn list_saved_searches(&self, account_id: Option<&str>) -> Result<Vec<SavedSearch>, StoreError> {
        let mut saved = Vec::new();
        match account_id {
            Some(account) => {
                let mut stmt = self
                    .conn
                    .prepare(&format!("SELECT {SAVED_COLUMNS} FROM saved_searches WHERE account_id = ?1 ORDER BY name COLLATE NOCASE ASC, id ASC"))?;
                let rows = stmt.query_map([account], row_to_saved)?;
                for row in rows {
                    saved.push(row?);
                }
            }
            None => {
                let mut stmt =
                    self.conn.prepare(&format!("SELECT {SAVED_COLUMNS} FROM saved_searches ORDER BY account_id, name COLLATE NOCASE ASC, id ASC"))?;
                let rows = stmt.query_map([], row_to_saved)?;
                for row in rows {
                    saved.push(row?);
                }
            }
        }
        Ok(saved)
    }

    pub fn get_saved_search(&self, id: &str) -> Result<Option<SavedSearch>, StoreError> {
        self.conn
            .query_row(&format!("SELECT {SAVED_COLUMNS} FROM saved_searches WHERE id = ?1"), [id], row_to_saved)
            .optional()
            .map_err(StoreError::from)
    }

    pub fn count_saved_searches(&self, account_id: &str) -> Result<u32, StoreError> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM saved_searches WHERE account_id = ?1", [account_id], |row| row.get(0))?;
        crate::repo::to_count(count)
    }

    pub fn create_saved_search(&self, saved: &SavedSearch) -> Result<(), StoreError> {
        self.conn
            .execute(
                &format!("INSERT INTO saved_searches({SAVED_COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
                params![saved.id, saved.account_id, saved.name, saved.query, saved.sort, saved.created_at, saved.updated_at],
            )
            .map_err(|error| duplicate("saved search", &saved.name, error))?;
        Ok(())
    }

    /// Change the editable fields. `None` keeps the current value; the id and account never change.
    pub fn update_saved_search(&self, id: &str, name: Option<&str>, query: Option<&str>, sort: Option<&str>) -> Result<SavedSearch, StoreError> {
        let Some(current) = self.get_saved_search(id)? else {
            return Err(StoreError::NotFound { entity: "saved search", id: id.to_owned() });
        };
        let name = name.unwrap_or(&current.name);
        let query = query.unwrap_or(&current.query);
        let sort = sort.unwrap_or(&current.sort);
        self.conn
            .execute(
                "UPDATE saved_searches SET name = ?1, query = ?2, sort = ?3, updated_at = ?4 WHERE id = ?5",
                params![name, query, sort, crate::now_ms(), id],
            )
            .map_err(|error| duplicate("saved search", name, error))?;
        self.get_saved_search(id)?.ok_or_else(|| StoreError::NotFound { entity: "saved search", id: id.to_owned() })
    }

    pub fn delete_saved_search(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute("DELETE FROM saved_searches WHERE id = ?1", [id])? == 1)
    }

    /// The last successful queries of one account, newest first. A damaged value reads as empty.
    pub fn recent_searches(&self, account_id: &str) -> Result<Vec<String>, StoreError> {
        let key = format!("{RECENT_SEARCHES_KEY_PREFIX}{account_id}");
        let Some(raw) = self.get_meta(&key)? else { return Ok(Vec::new()) };
        let recent: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
        Ok(recent.into_iter().take(8).collect())
    }

    /// Replace the recent list. The caller has already deduplicated and bounded it.
    pub fn set_recent_searches(&self, account_id: &str, recent: &[String]) -> Result<(), StoreError> {
        let key = format!("{RECENT_SEARCHES_KEY_PREFIX}{account_id}");
        let encoded = serde_json::to_string(recent)?;
        self.set_meta(&key, &encoded)
    }

    /// Remove one account's recent list. Returns whether a value existed.
    pub fn clear_recent_searches(&self, account_id: &str) -> Result<bool, StoreError> {
        let key = format!("{RECENT_SEARCHES_KEY_PREFIX}{account_id}");
        Ok(self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])? == 1)
    }
}

fn row_to_saved(row: &rusqlite::Row<'_>) -> rusqlite::Result<SavedSearch> {
    Ok(SavedSearch {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        query: row.get(3)?,
        sort: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}
