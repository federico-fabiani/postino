//! Local rules, their candidate query and their journal (P9c).
//!
//! The rule condition is the documented search language, translated by `postino-core` into a typed
//! [`SearchExpr`]. This module turns that tree into fixed SQL with bound values, scoped to received
//! mail: `messages.origin` must be `received`, so a Sent copy moved to Archive is not a candidate even
//! though its folder looks ordinary; Trash, Drafts and Sent folders are excluded separately.
//!
//! The journal is deliberately opaque about mail content. `rule_actions.value_json` carries only
//! ids, folder ids, label ids and booleans; a forward action records the outbox row id, never a
//! recipient or a subject. Runs are kept 20 per account and deleted oldest-first, together with
//! their actions.

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, TransactionBehavior, params, params_from_iter};

use crate::model::{
    FolderKind, MAX_RULE_CANDIDATES, MessageOrigin, Rule, RuleActionRecord, RuleCandidateFilter, RuleCandidatePage, RuleGuard, RuleMessageState,
    RuleRun, SEARCH_TOTAL_CAP,
};
use crate::repo::{SUMMARY_COLUMNS, corrupt_on_decode, duplicate, row_to_summary};
use crate::search::expr_sql;
use crate::{Store, StoreError};

/// Prefix of the encrypted `meta` key that marks a received message carrying a loop guard header.
pub const RULE_GUARD_KEY_PREFIX: &str = "rule_guard:";
/// Prefix of the durable forward-dedupe marker, independent of outbox retention.
pub const RULE_FORWARD_KEY_PREFIX: &str = "rule_forward:";
/// Prefix of the per-rule `all` page cursor.
pub const RULE_ALL_CURSOR_KEY_PREFIX: &str = "rule_all_cursor:";
/// Prefix of the encrypted `meta` key that remembers a run's mode (`new` or `all`).
pub const RULE_RUN_MODE_KEY_PREFIX: &str = "rule_run_mode:";
/// Prefix of the encrypted `meta` key that marks a run reversed by `rules_undo`.
pub const RULE_RUN_UNDONE_KEY_PREFIX: &str = "rule_run_undone:";

const RULE_COLUMNS: &str = "id, account_id, name, enabled, priority, stop, query, actions_json, created_at, updated_at";
const RUN_COLUMNS: &str = "id, account_id, started_at, finished_at, dry, rule_count, matched, changed, errors";
const ACTION_COLUMNS: &str = "run_id, ordinal, rule_id, message_id, action, value_json, changed";

impl Store {
    // -----------------------------------------------------------------------------------------
    // Rule CRUD
    // -----------------------------------------------------------------------------------------

    /// Rules for one account in execution order (`priority ASC, id ASC`).
    pub fn list_rules(&self, account_id: &str) -> Result<Vec<Rule>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("SELECT {RULE_COLUMNS} FROM rules WHERE account_id = ?1 ORDER BY priority ASC, id ASC"))?;
        let rows = stmt.query_map([account_id], row_to_rule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("rules", error))
    }

    /// Every rule, for the engine's open path. Per-account execution still calls [`Store::list_rules`].
    pub fn list_all_rules(&self) -> Result<Vec<Rule>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("SELECT {RULE_COLUMNS} FROM rules ORDER BY account_id ASC, priority ASC, id ASC"))?;
        let rows = stmt.query_map([], row_to_rule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("rules", error))
    }

    pub fn get_rule(&self, id: &str) -> Result<Option<Rule>, StoreError> {
        self.conn
            .query_row(&format!("SELECT {RULE_COLUMNS} FROM rules WHERE id = ?1"), [id], row_to_rule)
            .optional()
            .map_err(|error| corrupt_on_decode("rules", error))
    }

    pub fn count_rules(&self, account_id: &str) -> Result<u32, StoreError> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM rules WHERE account_id = ?1", [account_id], |row| row.get(0))?;
        crate::repo::to_count(count)
    }

    /// Insert one rule, refusing the `limit + 1`th in the same transaction as the count.
    pub fn create_rule_limited(&self, rule: &Rule, limit: u32) -> Result<(), StoreError> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM rules WHERE account_id = ?1", [&rule.account_id], |row| row.get(0))?;
        if u32::try_from(count).unwrap_or(u32::MAX) >= limit {
            return Err(StoreError::RuleLimit { limit });
        }
        tx.execute(
            &format!("INSERT INTO rules({RULE_COLUMNS}, name_key) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
            params![
                rule.id,
                rule.account_id,
                rule.name,
                rule.enabled,
                rule.priority,
                rule.stop,
                rule.query,
                rule.actions_json,
                rule.created_at,
                rule.updated_at,
                rule.name.to_lowercase()
            ],
        )
        .map_err(|error| duplicate("rule", &rule.name, error))?;
        tx.commit()?;
        Ok(())
    }

    /// Replace every editable field only when the row still has `expected_updated_at`. The caller
    /// merged absent fields from the current row, so a stale editor cannot silently undo another
    /// process's action list.
    pub fn update_rule(&self, rule: &Rule, expected_updated_at: i64) -> Result<Rule, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE rules SET name = ?1, name_key = ?2, enabled = ?3, priority = ?4, stop = ?5, query = ?6, actions_json = ?7, updated_at = ?8
                 WHERE id = ?9 AND account_id = ?10 AND updated_at = ?11",
                params![
                    rule.name,
                    rule.name.to_lowercase(),
                    rule.enabled,
                    rule.priority,
                    rule.stop,
                    rule.query,
                    rule.actions_json,
                    rule.updated_at,
                    rule.id,
                    rule.account_id,
                    expected_updated_at
                ],
            )
            .map_err(|error| duplicate("rule", &rule.name, error))?;
        if changed == 0 {
            return Err(StoreError::StaleRule { id: rule.id.clone() });
        }
        self.get_rule(&rule.id)?.ok_or_else(|| StoreError::NotFound { entity: "rule", id: rule.id.clone() })
    }

    pub fn delete_rule(&self, id: &str) -> Result<bool, StoreError> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let removed = tx.execute("DELETE FROM rules WHERE id = ?1", [id])? == 1;
        tx.execute("DELETE FROM meta WHERE key = ?1", [format!("{RULE_ALL_CURSOR_KEY_PREFIX}{id}")])?;
        tx.commit()?;
        Ok(removed)
    }

    /// A store-wide monotonic rule id (`rule_<n>`). A deleted id is never reused.
    pub fn allocate_rule_id(&self) -> Result<String, StoreError> {
        const KEY: &str = "rule_seq";
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", [KEY], |row| row.get(0)).optional()?;
        let stored = stored.and_then(|raw| raw.parse::<i64>().ok()).unwrap_or(0);
        let highest: Option<i64> =
            tx.query_row("SELECT MAX(CAST(substr(id, 6) AS INTEGER)) FROM rules WHERE id GLOB 'rule_[0-9]*'", [], |row| row.get(0))?;
        let next = stored.max(highest.unwrap_or(0)).saturating_add(1);
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![KEY, next.to_string()],
        )?;
        tx.commit()?;
        Ok(format!("rule_{next}"))
    }

    // -----------------------------------------------------------------------------------------
    // Candidates and message state
    // -----------------------------------------------------------------------------------------

    /// Received messages matching one rule condition, oldest first. Drafts, Sent copies and Trash
    /// rows are never candidates: an automatic rule must not edit a message the user is writing or
    /// resurrect one they discarded.
    pub fn rule_candidates(&self, filter: &RuleCandidateFilter) -> Result<RuleCandidatePage, StoreError> {
        let limit = filter.limit.clamp(1, MAX_RULE_CANDIDATES);
        let mut values = vec![Value::Text(filter.account_id.clone()), Value::Integer(filter.since)];
        let mut where_sql =
            "m.account_id = ? AND m.origin = 'received' AND m.ingested_at >= ? AND f.kind NOT IN ('drafts', 'sent', 'trash')".to_owned();
        if filter.new_only {
            where_sql.push_str(" AND m.rules_applied_at IS NULL");
        }
        if let Some((ingested_at, id)) = &filter.after {
            values.push(Value::Integer(*ingested_at));
            values.push(Value::Integer(*ingested_at));
            values.push(Value::Text(id.clone()));
            where_sql.push_str(" AND (m.ingested_at > ? OR (m.ingested_at = ? AND m.id > ?))");
        }
        if let Some(expr) = &filter.expr {
            let mut depth = 0u8;
            let sql = expr_sql(expr, &mut values, &mut depth)?;
            where_sql.push_str(&format!(" AND ({sql})"));
        }

        let mut count_values = values.clone();
        count_values.push(Value::Integer(i64::from(SEARCH_TOTAL_CAP) + 1));
        let counted: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM (
                     SELECT m.id FROM messages m JOIN folders f ON f.id = m.folder_id
                     WHERE {where_sql} LIMIT ?
                 )"
            ),
            params_from_iter(count_values.iter()),
            |row| row.get(0),
        )?;
        let total_capped = counted > i64::from(SEARCH_TOTAL_CAP);
        let total = u32::try_from(counted.min(i64::from(SEARCH_TOTAL_CAP))).unwrap_or(SEARCH_TOTAL_CAP);

        values.push(Value::Integer(i64::from(limit)));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS}
             FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE {where_sql}
             ORDER BY m.ingested_at ASC, m.id ASC
             LIMIT ?"
        ))?;
        let mut messages = stmt
            .query_map(params_from_iter(values.iter()), row_to_summary)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| corrupt_on_decode("messages", error))?;
        drop(stmt);
        let ids = messages.iter().map(|message| message.id.clone()).collect::<Vec<_>>();
        let labels = self.labels_by_message(&ids)?;
        for message in &mut messages {
            if let Some(ids) = labels.get(&message.id) {
                message.labels.clone_from(ids);
            }
        }
        Ok(RuleCandidatePage { messages, total, total_capped })
    }

    /// The current folder, folder kind, read state and labels of one message. `None` when the
    /// message vanished between the candidate query and the action.
    pub fn rule_message_state(&self, id: &str) -> Result<Option<RuleMessageState>, StoreError> {
        let row: Option<(String, String, String, bool, bool)> = self
            .conn
            .query_row(
                "SELECT m.folder_id, f.kind, m.origin, m.unread, m.starred
                 FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        let Some((folder_id, kind, origin, unread, starred)) = row else { return Ok(None) };
        let Some(folder_kind) = FolderKind::parse(&kind) else {
            return Err(StoreError::Corrupt { table: "folders", detail: format!("unknown folder kind {kind:?}") });
        };
        let Some(origin) = MessageOrigin::parse(&origin) else {
            return Err(StoreError::Corrupt { table: "messages", detail: format!("unknown message origin {origin:?}") });
        };
        let labels = self.labels_for(id)?;
        Ok(Some(RuleMessageState { folder_id, folder_kind, origin, unread, starred, labels }))
    }

    /// Mark a bounded page of new received messages as handled by a `new` run. Messages beyond the
    /// cap stay NULL and are picked up next time; an `all` run never calls this.
    pub fn mark_rules_applied(&self, ids: &[String], at: i64) -> Result<u32, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut changed = 0u32;
        for id in ids {
            changed = changed.saturating_add(
                u32::try_from(tx.execute("UPDATE messages SET rules_applied_at = ?1 WHERE id = ?2 AND rules_applied_at IS NULL", params![at, id])?)
                    .unwrap_or(0),
            );
        }
        tx.commit()?;
        Ok(changed)
    }

    // -----------------------------------------------------------------------------------------
    // Journal
    // -----------------------------------------------------------------------------------------

    pub fn create_rule_run(&self, run: &RuleRun) -> Result<(), StoreError> {
        self.conn.execute(
            &format!("INSERT INTO rule_runs({RUN_COLUMNS}) VALUES(?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8)"),
            params![run.id, run.account_id, run.started_at, run.finished_at, run.rule_count, run.matched, run.changed, run.errors],
        )?;
        self.set_meta(&format!("{RULE_RUN_MODE_KEY_PREFIX}{}", run.id), &run.mode)
    }

    pub fn finish_rule_run(&self, id: &str, finished_at: i64, matched: u32, changed: u32, errors: u32) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE rule_runs SET finished_at = ?1, matched = ?2, changed = ?3, errors = ?4 WHERE id = ?5",
            params![finished_at, matched, changed, errors, id],
        )?;
        Ok(())
    }

    pub fn append_rule_action(&self, action: &RuleActionRecord) -> Result<(), StoreError> {
        self.conn.execute(
            &format!("INSERT INTO rule_actions({ACTION_COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
            params![action.run_id, action.ordinal, action.rule_id, action.message_id, action.action, action.value_json, action.changed],
        )?;
        Ok(())
    }

    /// Runs for one account, newest first. `limit` is clamped to 1..=20.
    pub fn list_rule_runs(&self, account_id: &str, limit: u32) -> Result<Vec<RuleRun>, StoreError> {
        let limit = i64::from(limit.clamp(1, 20));
        let mut stmt =
            self.conn.prepare(&format!("SELECT {RUN_COLUMNS} FROM rule_runs WHERE account_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT ?2"))?;
        let rows = stmt
            .query_map(params![account_id, limit], |row| {
                let dry: bool = row.get(4)?;
                Ok((
                    RuleRun {
                        id: row.get(0)?,
                        account_id: row.get(1)?,
                        started_at: row.get(2)?,
                        finished_at: row.get(3)?,
                        mode: "new".to_owned(),
                        rule_count: row.get(5)?,
                        matched: row.get(6)?,
                        changed: row.get(7)?,
                        errors: row.get(8)?,
                        undone: false,
                    },
                    dry,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut runs = Vec::with_capacity(rows.len());
        for (mut run, _dry) in rows {
            run.mode = self.get_meta(&format!("{RULE_RUN_MODE_KEY_PREFIX}{}", run.id))?.unwrap_or_else(|| "new".to_owned());
            run.undone = self.get_meta(&format!("{RULE_RUN_UNDONE_KEY_PREFIX}{}", run.id))?.is_some();
            runs.push(run);
        }
        Ok(runs)
    }

    pub fn get_rule_run(&self, id: &str) -> Result<Option<RuleRun>, StoreError> {
        type RunRow = (String, String, i64, Option<i64>, bool, u32, u32, u32, u32);
        let row: Option<RunRow> = self
            .conn
            .query_row(&format!("SELECT {RUN_COLUMNS} FROM rule_runs WHERE id = ?1"), [id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?))
            })
            .optional()?;
        let Some((id, account_id, started_at, finished_at, _dry, rule_count, matched, changed, errors)) = row else { return Ok(None) };
        let mode = self.get_meta(&format!("{RULE_RUN_MODE_KEY_PREFIX}{id}"))?.unwrap_or_else(|| "new".to_owned());
        let undone = self.get_meta(&format!("{RULE_RUN_UNDONE_KEY_PREFIX}{id}"))?.is_some();
        Ok(Some(RuleRun { id, account_id, started_at, finished_at, mode, rule_count, matched, changed, errors, undone }))
    }

    /// Every action of one run in execution order.
    pub fn rule_run_actions(&self, run_id: &str) -> Result<Vec<RuleActionRecord>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("SELECT {ACTION_COLUMNS} FROM rule_actions WHERE run_id = ?1 ORDER BY ordinal ASC"))?;
        let rows = stmt.query_map([run_id], |row| {
            Ok(RuleActionRecord {
                run_id: row.get(0)?,
                ordinal: row.get(1)?,
                rule_id: row.get(2)?,
                message_id: row.get(3)?,
                action: row.get(4)?,
                value_json: row.get(5)?,
                changed: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("rule_actions", error))
    }

    /// Mark a run as reversed. Returns false when it was already undone.
    pub fn mark_rule_run_undone(&self, id: &str, at: i64) -> Result<bool, StoreError> {
        let key = format!("{RULE_RUN_UNDONE_KEY_PREFIX}{id}");
        if self.get_meta(&key)?.is_some() {
            return Ok(false);
        }
        self.set_meta(&key, &at.to_string())?;
        Ok(true)
    }

    /// Keep the newest `keep` non-dry runs of one account and delete the rest with their actions and
    /// metadata. Returns the number of deleted runs.
    pub fn prune_rule_runs(&self, account_id: &str, keep: u32) -> Result<u32, StoreError> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let stale = {
            let mut stmt = tx.prepare(
                "SELECT id FROM rule_runs WHERE account_id = ?1 AND dry = 0
                 ORDER BY started_at DESC, rowid DESC LIMIT -1 OFFSET ?2",
            )?;
            stmt.query_map(params![account_id, i64::from(keep)], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in &stale {
            tx.execute(
                "DELETE FROM meta WHERE key IN (?1, ?2)",
                params![format!("{RULE_RUN_MODE_KEY_PREFIX}{id}"), format!("{RULE_RUN_UNDONE_KEY_PREFIX}{id}")],
            )?;
            tx.execute("DELETE FROM rule_runs WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(u32::try_from(stale.len()).unwrap_or(u32::MAX))
    }

    /// A store-wide monotonic run id (`rule_run_<n>`).
    pub fn allocate_rule_run_id(&self) -> Result<String, StoreError> {
        const KEY: &str = "rule_run_seq";
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", [KEY], |row| row.get(0)).optional()?;
        let stored = stored.and_then(|raw| raw.parse::<i64>().ok()).unwrap_or(0);
        let highest: Option<i64> =
            tx.query_row("SELECT MAX(CAST(substr(id, 10) AS INTEGER)) FROM rule_runs WHERE id GLOB 'rule_run_[0-9]*'", [], |row| row.get(0))?;
        let next = stored.max(highest.unwrap_or(0)).saturating_add(1);
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![KEY, next.to_string()],
        )?;
        tx.commit()?;
        Ok(format!("rule_run_{next}"))
    }

    // -----------------------------------------------------------------------------------------
    // Loop guard, forward dedupe and the `all` cursor
    // -----------------------------------------------------------------------------------------

    fn guard_key(account_id: &str, message_id: &str) -> String {
        format!("{RULE_GUARD_KEY_PREFIX}{account_id}:{message_id}")
    }

    fn forward_key(account_id: &str, rule_id: &str, message_id: &str) -> String {
        format!("{RULE_FORWARD_KEY_PREFIX}{account_id}:{rule_id}:{message_id}")
    }

    /// Remember that a received message carried `Auto-Submitted` and/or `X-Postino-Rule`. The row
    /// lives in the encrypted `meta` table because v7 has no header column. The account is part of
    /// the key so bulk sync deletion and account removal can sweep every guard, including orphans.
    pub fn set_rule_guard(&self, account_id: &str, message_id: &str, guard: RuleGuard) -> Result<(), StoreError> {
        let key = Self::guard_key(account_id, message_id);
        if !guard.any() {
            self.conn.execute("DELETE FROM meta WHERE key = ?1", [&key])?;
            return Ok(());
        }
        let value = format!("{}{}", if guard.auto_submitted { '1' } else { '0' }, if guard.x_postino_rule { '1' } else { '0' });
        self.set_meta(&key, &value)
    }

    /// The loop guard of one message, read only when a forward action actually runs.
    pub fn rule_guard(&self, account_id: &str, message_id: &str) -> Result<RuleGuard, StoreError> {
        let key = Self::guard_key(account_id, message_id);
        let Some(raw) = self.get_meta(&key)? else { return Ok(RuleGuard::default()) };
        let bytes = raw.as_bytes();
        Ok(RuleGuard { auto_submitted: bytes.first() == Some(&b'1'), x_postino_rule: bytes.get(1) == Some(&b'1') })
    }

    /// Delete every guard row of one message. A bulk sync deletion or folder reset calls this for
    /// each removed id; the old un-scoped key is removed too so a pre-fix store does not leak it.
    pub fn delete_rule_guards(&self, account_id: &str, message_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM meta WHERE key IN (?1, ?2)",
            params![Self::guard_key(account_id, message_id), format!("{RULE_GUARD_KEY_PREFIX}{message_id}")],
        )?;
        Ok(())
    }

    /// Delete every guard row of an account, including orphans left by an older bulk deletion.
    pub fn delete_account_rule_guards(&self, account_id: &str) -> Result<u32, StoreError> {
        let prefix = format!("{RULE_GUARD_KEY_PREFIX}{account_id}:");
        let changed = self.conn.execute("DELETE FROM meta WHERE key LIKE ?1", [format!("{prefix}%")])?;
        // A v7 file written before P9c used `rule_guard:<message>`; remove any that still point at a
        // row of this account, and any orphaned key the old build left behind for this account is
        // best-effort (the old key had no account component).
        self.conn.execute("DELETE FROM meta WHERE key IN (SELECT 'rule_guard:' || id FROM messages WHERE account_id = ?1)", [account_id])?;
        u32::try_from(changed).map_err(|_| StoreError::OutOfRange { field: "deleted guard rows" })
    }

    /// The durable forward dedupe marker for one rule/message. It survives outbox pruning.
    pub fn rule_forward_marker(&self, account_id: &str, rule_id: &str, message_id: &str) -> Result<Option<String>, StoreError> {
        self.get_meta(&Self::forward_key(account_id, rule_id, message_id))
    }

    /// Remember the outbox row that carried a rule forward. The value is the row id only.
    pub fn set_rule_forward_marker(&self, account_id: &str, rule_id: &str, message_id: &str, outbox_id: &str) -> Result<(), StoreError> {
        self.set_meta(&Self::forward_key(account_id, rule_id, message_id), outbox_id)
    }

    /// The `all` page cursor of one rule: the last `(ingested_at, id)` already considered.
    pub fn rule_all_cursor(&self, rule_id: &str) -> Result<Option<(i64, String)>, StoreError> {
        let key = format!("{RULE_ALL_CURSOR_KEY_PREFIX}{rule_id}");
        let Some(raw) = self.get_meta(&key)? else { return Ok(None) };
        let value: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        let Some(ingested_at) = value.get("ingested_at").and_then(serde_json::Value::as_i64) else { return Ok(None) };
        let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else { return Ok(None) };
        Ok(Some((ingested_at, id.to_owned())))
    }

    /// Store or clear the `all` page cursor. A clear means the next run starts at the beginning.
    pub fn set_rule_all_cursor(&self, rule_id: &str, cursor: Option<(i64, &str)>) -> Result<(), StoreError> {
        let key = format!("{RULE_ALL_CURSOR_KEY_PREFIX}{rule_id}");
        match cursor {
            Some((ingested_at, id)) => {
                let value = serde_json::json!({ "ingested_at": ingested_at, "id": id });
                self.set_meta(&key, &serde_json::to_string(&value)?)
            }
            None => {
                self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
                Ok(())
            }
        }
    }

    /// Accounts whose rules are enabled, for the periodic and post-sync run.
    pub fn accounts_with_rules(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT account_id FROM rules WHERE enabled = 1 ORDER BY account_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(StoreError::Sqlite)
    }
}

fn row_to_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        enabled: row.get(3)?,
        priority: row.get(4)?,
        stop: row.get(5)?,
        query: row.get(6)?,
        actions_json: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}
