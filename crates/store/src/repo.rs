//! Repository methods: typed reads and transactional writes over the schema in [`crate`].
//!
//! The API mirrors the M0 operations so P3c is a mapping exercise, not a redesign: accounts,
//! folders, message list/get/flags/move/delete/counts, labels, attachments and drafts. Every batch
//! mutation runs in one transaction and either applies completely or not at all.

use std::collections::{BTreeSet, HashMap};

use rusqlite::types::{Type, Value};
use rusqlite::{ErrorCode, OptionalExtension, Row, Transaction, TransactionBehavior, params, params_from_iter};

use crate::model::{
    Account, AccountServer, AccountTls, Address, Attachment, Draft, DraftMeta, DraftUpdate, FlagUpdate, Folder, FolderKind, FolderMessageFlags,
    Label, LabelCount, MAX_REMOTE_IMAGES, Message, MessageCounts, MessageFilter, MessageFlags, MessageOrigin, MessagePage, MessageSummary,
    MessageThreading, OutboxEntry, OutboxState, SyncState, UidEntry,
};
use crate::threads;
use crate::{REMOTE_LABELS_KEY_PREFIX, REPLY_TO_KEY_PREFIX, SYNC_STATUS_KEY_PREFIX, Store, StoreError, UNSUBSCRIBE_KEY_PREFIX};

/// Largest page a caller can request. A mailbox list never needs more, and the cap bounds memory.
const MAX_PAGE: u32 = 500;

const ACCOUNT_COLUMNS: &str = "id, name, email, provider, signature, created_at";
const SERVER_COLUMNS: &str = "account_id, imap_host, imap_port, imap_tls, imap_username, created_at, smtp_host, smtp_port, smtp_tls, smtp_username";
const FOLDER_COLUMNS: &str = "id, account_id, name, kind, unread, total, uid_validity, uid_next, last_sync_at";
pub(crate) const SUMMARY_COLUMNS: &str = "m.id, m.account_id, m.folder_id, m.thread_id, m.from_name, m.from_email, m.to_json, m.subject, m.snippet, m.date, m.unread, m.starred, m.has_attachments";
pub(crate) const MESSAGE_DETAIL_COLUMNS: &str = "m.uid, m.message_id, m.cc_json, m.bcc_json, m.body_text, m.body_html, m.remote_images, m.size";
const LABEL_COLUMNS: &str = "l.id, l.account_id, l.name, l.color";
const DRAFT_COLUMNS: &str = "id, account_id, revision, updated_at, body_text, to_json, cc_json, bcc_json, subject, in_reply_to";
const ATTACHMENT_COLUMNS: &str = "id, filename, mime, size, blob_hash";
const OUTBOX_COLUMNS: &str = "id, account_id, draft_id, idempotency_key, created_at, send_at, state, attempts, append_attempts, last_error, raw_blob_hash, envelope_json, lease_at";

// ---------------------------------------------------------------------------------------------
// Accounts and their IMAP server
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Insert or replace an account. The row is authoritative: every column is overwritten.
    pub fn upsert_account(&self, account: &Account) -> Result<(), StoreError> {
        upsert_account_row(&self.conn, account)?;
        Ok(())
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY name COLLATE NOCASE, id"))?;
        let rows = stmt.query_map([], row_to_account)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_account(&self, id: &str) -> Result<Option<Account>, StoreError> {
        Ok(self.conn.query_row(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = ?1"), [id], row_to_account).optional()?)
    }

    /// Replace one account's plain-text signature. Returns whether the account exists.
    ///
    /// This is deliberately narrower than [`Store::upsert_account`]: changing a signature must never
    /// rewrite the name, the address or the provider, and it must not need the row's `created_at`.
    pub fn set_account_signature(&self, id: &str, signature: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute("UPDATE accounts SET signature = ?2 WHERE id = ?1", params![id, signature])? > 0)
    }

    /// Persist a received message's `Reply-To` list in the existing `meta` table.
    ///
    /// The shipped schema has no column for it and P8 may not migrate. An empty list clears the
    /// key. The caller has already bounded and validated the addresses; this is a cache of a header,
    /// never a credential or a body.
    pub fn set_message_reply_to(&self, message_id: &str, reply_to: &[Address]) -> Result<(), StoreError> {
        let key = format!("{REPLY_TO_KEY_PREFIX}{message_id}");
        if reply_to.is_empty() {
            self.conn.execute("DELETE FROM meta WHERE key = ?1", [&key])?;
            return Ok(());
        }
        let value = serde_json::to_string(reply_to)?;
        self.conn
            .execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
        Ok(())
    }

    /// The persisted `Reply-To` list for one message, or an empty list when it has none.
    ///
    /// A damaged value is a readable store error, never a silent empty list: the caller can then
    /// fall back to `From` and say so if needed.
    pub fn message_reply_to(&self, message_id: &str) -> Result<Vec<Address>, StoreError> {
        let key = format!("{REPLY_TO_KEY_PREFIX}{message_id}");
        let raw: Option<String> = self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [&key], |row| row.get(0)).optional()?;
        match raw {
            Some(raw) => serde_json::from_str(&raw).map_err(|error| StoreError::Corrupt { table: "messages.reply_to", detail: error.to_string() }),
            None => Ok(Vec::new()),
        }
    }

    /// Write an account, its IMAP server configuration and its folders in **one transaction**.
    ///
    /// Onboarding (P6a) uses this after the credential is in the keyring and after the server has
    /// accepted the login: a failure leaves no account, no server row and no folder behind. The
    /// server row and every folder row are stamped with the account id, so a caller cannot create a
    /// row for another account through this path.
    pub fn save_account(&self, account: &Account, server: &AccountServer, folders: &[Folder]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        upsert_account_row(&tx, account)?;
        let server = AccountServer { account_id: account.id.clone(), ..server.clone() };
        upsert_account_server_row(&tx, &server)?;
        for folder in folders {
            let mut row = folder.clone();
            row.account_id = account.id.clone();
            upsert_folder_row(&tx, &row)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert or replace one account's IMAP server configuration.
    pub fn upsert_account_server(&self, server: &AccountServer) -> Result<(), StoreError> {
        upsert_account_server_row(&self.conn, server)?;
        Ok(())
    }

    pub fn get_account_server(&self, account_id: &str) -> Result<Option<AccountServer>, StoreError> {
        self.conn
            .query_row(&format!("SELECT {SERVER_COLUMNS} FROM account_servers WHERE account_id = ?1"), [account_id], row_to_account_server)
            .optional()
            .map_err(|error| corrupt_on_decode("account_servers", error))
    }

    /// Remove one server configuration. Returns whether a row was there.
    pub fn delete_account_server(&self, account_id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute("DELETE FROM account_servers WHERE account_id = ?1", [account_id])? > 0)
    }

    /// Remove an account and its local cache in **one transaction**: messages (and, by cascade,
    /// attachments and label assignments), folders (and sync cursors), drafts, the IMAP server
    /// configuration, the persisted sync status and finally the account row.
    ///
    /// Labels are mailbox-wide (`account_id IS NULL`) and are deliberately untouched. The caller
    /// deletes the keyring credential on its own schedule; this function never sees a secret.
    pub fn delete_account(&self, account_id: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)", [account_id], |row| row.get(0))?;
        if !exists {
            tx.rollback()?;
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM draft_seq
             WHERE message_id IN (SELECT id FROM messages WHERE account_id = ?1)
                OR message_id IN (SELECT id FROM drafts WHERE account_id = ?1)",
            [account_id],
        )?;
        tx.execute("DELETE FROM meta WHERE key IN (SELECT 'reply_to:' || id FROM messages WHERE account_id = ?1)", [account_id])?;
        tx.execute("DELETE FROM meta WHERE key IN (SELECT 'unsubscribe:' || id FROM messages WHERE account_id = ?1)", [account_id])?;
        tx.execute("DELETE FROM meta WHERE key IN (SELECT 'remote_labels:' || id FROM messages WHERE account_id = ?1)", [account_id])?;
        tx.execute("DELETE FROM meta WHERE key IN (SELECT 'rule_guard:' || id FROM messages WHERE account_id = ?1)", [account_id])?;
        tx.execute("DELETE FROM meta WHERE key LIKE ?1", [format!("rule_guard:{account_id}:%")])?;
        tx.execute("DELETE FROM meta WHERE key LIKE ?1", [format!("rule_forward:{account_id}:%")])?;
        tx.execute("DELETE FROM meta WHERE key IN (SELECT 'rule_all_cursor:' || id FROM rules WHERE account_id = ?1)", [account_id])?;
        tx.execute("DELETE FROM meta WHERE key IN (SELECT 'rule_run_mode:' || id FROM rule_runs WHERE account_id = ?1)", [account_id])?;
        tx.execute("DELETE FROM meta WHERE key IN (SELECT 'rule_run_undone:' || id FROM rule_runs WHERE account_id = ?1)", [account_id])?;
        tx.execute("DELETE FROM rule_runs WHERE account_id = ?1", [account_id])?;
        tx.execute("DELETE FROM messages WHERE account_id = ?1", [account_id])?;
        tx.execute("DELETE FROM drafts WHERE account_id = ?1", [account_id])?;
        tx.execute("DELETE FROM account_servers WHERE account_id = ?1", [account_id])?;
        tx.execute("DELETE FROM folders WHERE account_id = ?1", [account_id])?;
        let status_key = format!("{SYNC_STATUS_KEY_PREFIX}{account_id}");
        tx.execute("DELETE FROM meta WHERE key = ?1", [status_key])?;
        let recent_key = format!("{}{account_id}", crate::RECENT_SEARCHES_KEY_PREFIX);
        tx.execute("DELETE FROM meta WHERE key = ?1", [recent_key])?;
        tx.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
        tx.commit()?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------------------------

/// Largest number of outbox rows one list returns. The list is newest-first and is a history view,
/// not a correctness input: every state decision uses a targeted query. A bound keeps a damaged
/// store from turning a list into an allocation.
pub const MAX_OUTBOX_ROWS: u32 = 500;

impl Store {
    /// Queue one submission row, or revive the cancelled row with the same idempotency key.
    ///
    /// The key is stable per draft content, so two processes that queue the same draft compute the
    /// same key: the first inserts, the second gets [`StoreError::OutboxAlreadyActive`] instead of a
    /// second row. The caller treats that as the idempotent answer (it re-reads the live row). A
    /// cancelled row with the same key is reused, because a cancel is a discard of that content, not
    /// a history entry that blocks a later attempt.
    ///
    /// The partial unique index `outbox_active_draft` is the last line of defence: even with a
    /// different key (edited content racing a queue), a draft cannot have two `queued`/`sending`
    /// rows, so one pass cannot submit it twice.
    pub fn queue_outbox(&self, entry: &OutboxEntry) -> Result<OutboxEntry, StoreError> {
        // Immediate: the key lookup and the insert must not interleave with another process.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        require_account(&tx, &entry.account_id)?;
        if let Some((id, raw_state)) = tx
            .query_row("SELECT id, state FROM outbox WHERE idempotency_key = ?1 ORDER BY rowid DESC LIMIT 1", [&entry.idempotency_key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .optional()?
        {
            let state = parse_state(&raw_state)?;
            if state == OutboxState::Cancelled {
                tx.execute(
                    "UPDATE outbox SET created_at = ?2, send_at = ?3, state = 'queued', attempts = 0, append_attempts = 0, last_error = NULL,
                         raw_blob_hash = ?4, envelope_json = ?5, draft_id = ?6, lease_at = NULL
                     WHERE id = ?1 AND state = 'cancelled'",
                    params![id, entry.created_at, entry.send_at, entry.raw_blob_hash, entry.envelope_json, entry.draft_id.as_deref()],
                )?;
                let row = tx.query_row(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1"), [&id], row_to_outbox).optional()?;
                tx.commit()?;
                return row.ok_or(StoreError::NotFound { entity: "outbox row", id });
            }
            tx.rollback()?;
            return Err(StoreError::OutboxAlreadyActive { draft_id: entry.draft_id.clone().unwrap_or_default(), state });
        }
        let written = tx.execute(
            "INSERT INTO outbox(id, account_id, draft_id, idempotency_key, created_at, send_at, state, attempts, append_attempts, last_error, raw_blob_hash, envelope_json, lease_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL)",
            params![
                entry.id,
                entry.account_id,
                entry.draft_id.as_deref(),
                entry.idempotency_key,
                entry.created_at,
                entry.send_at,
                entry.state.as_str(),
                i64::from(entry.attempts),
                i64::from(entry.append_attempts),
                entry.last_error,
                entry.raw_blob_hash,
                entry.envelope_json,
            ],
        );
        if let Err(error) = written {
            // A constraint here means another writer queued the same draft between our read and our
            // write, or the active-draft index caught a different key for the same draft.
            tx.rollback()?;
            let state = match entry.draft_id.as_deref() {
                Some(draft_id) => self.active_state_for_draft(draft_id)?,
                None => None,
            };
            if let Some(state) = state {
                return Err(StoreError::OutboxAlreadyActive { draft_id: entry.draft_id.clone().unwrap_or_default(), state });
            }
            return Err(error.into());
        }
        let row = tx.query_row(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1"), [&entry.id], row_to_outbox).optional()?;
        tx.commit()?;
        row.ok_or(StoreError::NotFound { entity: "outbox row", id: entry.id.clone() })
    }

    pub fn get_outbox(&self, id: &str) -> Result<Option<OutboxEntry>, StoreError> {
        self.conn
            .query_row(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1"), [id], row_to_outbox)
            .optional()
            .map_err(|error| corrupt_on_decode("outbox", error))
    }

    /// The newest row with one idempotency key, whatever its state. A rule forward uses the key as
    /// its dedupe: two runs over the same message must resolve to the same row.
    pub fn outbox_by_key(&self, key: &str) -> Result<Option<OutboxEntry>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE idempotency_key = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1"),
                [key],
                row_to_outbox,
            )
            .optional()
            .map_err(|error| corrupt_on_decode("outbox", error))
    }

    /// The newest row for one draft, whatever its state. `created_at` is the authority (a revival
    /// updates it so the revived row becomes the newest); `rowid` only breaks a millisecond tie. This
    /// is the only query the dedupe and the draft lock use: it never scans a capped list.
    pub fn active_outbox_for_draft(&self, draft_id: &str) -> Result<Option<OutboxEntry>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE draft_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1"),
                [draft_id],
                row_to_outbox,
            )
            .optional()
            .map_err(|error| corrupt_on_decode("outbox", error))
    }

    /// The state of the row that occupies a draft's active slot, if any.
    pub fn active_state_for_draft(&self, draft_id: &str) -> Result<Option<OutboxState>, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM outbox
                 WHERE draft_id = ?1 AND state IN ('queued', 'sending', 'append_pending', 'uncertain', 'failed')
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [draft_id],
                |row| row.get(0),
            )
            .optional()?;
        raw.as_deref().map(parse_state).transpose()
    }

    /// The newest `limit` journal rows, optionally for one account, newest first. `limit` is clamped
    /// to 1..=[`MAX_OUTBOX_ROWS`]; `offset` skips that many rows for a second page.
    pub fn list_outbox(&self, account_id: Option<&str>, limit: u32) -> Result<Vec<OutboxEntry>, StoreError> {
        self.list_outbox_page(account_id, limit, 0)
    }

    /// One newest-first page of the journal. A history view never hides a new row behind old ones:
    /// a row queued now is the first element even when the table already holds a thousand rows.
    pub fn list_outbox_page(&self, account_id: Option<&str>, limit: u32, offset: u32) -> Result<Vec<OutboxEntry>, StoreError> {
        let limit = i64::from(limit.clamp(1, MAX_OUTBOX_ROWS));
        let offset = i64::from(offset);
        let rows = match account_id {
            Some(account) => {
                let mut stmt = self.conn.prepare(&format!(
                    "SELECT {OUTBOX_COLUMNS} FROM outbox WHERE account_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2 OFFSET ?3"
                ))?;
                let rows = stmt.query_map(params![account, limit, offset], row_to_outbox)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("outbox", error))?
            }
            None => {
                let mut stmt =
                    self.conn.prepare(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox ORDER BY created_at DESC, rowid DESC LIMIT ?1 OFFSET ?2"))?;
                let rows = stmt.query_map(params![limit, offset], row_to_outbox)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("outbox", error))?
            }
        };
        Ok(rows)
    }

    /// Claim the earliest `append_pending` row whose Sent append is due and unowned — either never
    /// claimed, or claimed by a worker whose lease expired — in one transaction.
    ///
    /// The claim writes `lease_at` and increments `append_attempts`, the counter the append backoff
    /// uses. Two passes (the app and the CLI, or two app instances) can therefore never APPEND the
    /// same row at the same time; the loser sees no claimable row. A crash during the append leaves
    /// the lease behind and the row is claimable again after `lease_ms`, exactly like a submission
    /// lease.
    pub fn claim_outbox_append(&self, now_ms: i64, lease_ms: i64) -> Result<Option<OutboxEntry>, StoreError> {
        let cutoff = now_ms.saturating_sub(lease_ms.max(0));
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let id = tx
            .query_row(
                "SELECT id FROM outbox
                 WHERE state = 'append_pending' AND send_at <= ?1 AND (lease_at IS NULL OR lease_at <= ?2)
                 ORDER BY send_at, rowid LIMIT 1",
                params![now_ms, cutoff],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(id) = id else {
            tx.rollback()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE outbox SET lease_at = ?2, append_attempts = append_attempts + 1
             WHERE id = ?1 AND state = 'append_pending' AND (lease_at IS NULL OR lease_at <= ?3)",
            params![id, now_ms, cutoff],
        )?;
        if changed == 0 {
            tx.rollback()?;
            return Ok(None);
        }
        let entry = tx.query_row(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1"), [&id], row_to_outbox).optional()?;
        tx.commit()?;
        Ok(entry)
    }

    /// The instant at which the next pass could have work: a queued row's due time, an unowned append
    /// row's due time, or a owned row's lease expiry. `None` when nothing is pending. One indexed
    /// row, not a list.
    pub fn next_outbox_wake(&self, lease_ms: i64) -> Result<Option<i64>, StoreError> {
        let lease_ms = lease_ms.max(0);
        Ok(self.conn.query_row(
            "SELECT MIN(CASE WHEN state IN ('sending', 'append_pending') AND lease_at IS NOT NULL THEN lease_at + ?1 ELSE send_at END)
             FROM outbox WHERE state IN ('queued', 'sending', 'append_pending')",
            [lease_ms],
            |row| row.get::<_, Option<i64>>(0),
        )?)
    }

    /// Turn every `sending` row whose **lease** is older than `lease_ms` into `uncertain` and return
    /// its id.
    ///
    /// The lease epoch is `lease_at`, written by [`Store::lease_next_outbox`]. A row scheduled for
    /// 09:00 and leased at 10:00 is fresh for the whole window: `send_at` is user-visible and has no
    /// bearing on ownership. A crash during `DATA` becomes uncertain after the window and is never
    /// queued again. A row written by a build before the column existed falls back to `send_at`.
    pub fn expire_stale_outbox(&self, now_ms: i64, lease_ms: i64) -> Result<Vec<String>, StoreError> {
        let cutoff = now_ms.saturating_sub(lease_ms.max(0));
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let ids = {
            let mut stmt = tx.prepare("SELECT id FROM outbox WHERE state = 'sending' AND COALESCE(lease_at, send_at) <= ?1 ORDER BY id")?;
            let rows = stmt.query_map([cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !ids.is_empty() {
            tx.execute(
                "UPDATE outbox SET state = 'uncertain', last_error = ?1, lease_at = NULL
                 WHERE state = 'sending' AND COALESCE(lease_at, send_at) <= ?2",
                params![UNCERTAIN_AFTER_CRASH, cutoff],
            )?;
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Lease the earliest due `queued` row in one transaction and return it with `attempts` already
    /// incremented. `None` when nothing is due. Only a `queued` row can be claimed; a cancelled,
    /// failed, uncertain or already-sending row is never picked up. `lease_at` is the instant the
    /// claim happened and the only epoch later expiry and transitions compare against.
    pub fn lease_next_outbox(&self, now_ms: i64) -> Result<Option<OutboxEntry>, StoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let id = tx
            .query_row(
                "SELECT id FROM outbox WHERE state = 'queued' AND send_at <= ?1 ORDER BY send_at, created_at, rowid LIMIT 1",
                [now_ms],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(id) = id else {
            tx.rollback()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE outbox SET state = 'sending', lease_at = ?2, attempts = attempts + 1 WHERE id = ?1 AND state = 'queued'",
            params![id, now_ms],
        )?;
        if changed == 0 {
            tx.rollback()?;
            return Ok(None);
        }
        let entry = tx.query_row(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1"), [&id], row_to_outbox).optional()?;
        tx.commit()?;
        Ok(entry)
    }

    /// SMTP accepted the message: only the Sent append remains. This transition is durable before
    /// the local Sent row or the network append is attempted. It is conditional on the caller still
    /// owning the submission lease: a worker whose lease expired must not resurrect a row another
    /// process is resolving. The append lease starts at the 250 (`append_lease`), so the worker that
    /// submitted the message is also the one that appends it, and two passes cannot append it
    /// concurrently. This is the first append attempt (`append_attempts = 1`); a retry increments it
    /// when it claims the row.
    pub fn mark_outbox_append_pending(&self, id: &str, lease_at: i64, append_lease: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'append_pending', last_error = NULL, lease_at = ?3, append_attempts = 1
             WHERE id = ?1 AND state = 'sending' AND lease_at = ?2",
            params![id, lease_at, append_lease],
        )? > 0)
    }

    /// The Sent append succeeded. Only an `append_pending` row claimed by this worker can reach
    /// `sent`; a second worker that lost the append claim cannot finish the first one's row.
    pub fn finish_outbox_append(&self, id: &str, lease_at: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'sent', lease_at = NULL WHERE id = ?1 AND state = 'append_pending' AND lease_at = ?2",
            params![id, lease_at],
        )? > 0)
    }

    /// A person confirmed that an uncertain message did reach the server. No network call.
    pub fn confirm_outbox_sent(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute("UPDATE outbox SET state = 'sent', lease_at = NULL WHERE id = ?1 AND state = 'uncertain'", [id])? > 0)
    }

    /// The submission outcome is unknown. Never auto-retried. Conditional on the lease.
    pub fn mark_outbox_uncertain(&self, id: &str, error: &str, lease_at: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'uncertain', last_error = ?3, lease_at = NULL WHERE id = ?1 AND state = 'sending' AND lease_at = ?2",
            params![id, lease_at, error],
        )? > 0)
    }

    /// A permanent error (or the attempt ceiling) stopped this row. Conditional on the lease.
    pub fn mark_outbox_failed(&self, id: &str, error: &str, lease_at: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'failed', last_error = ?3, lease_at = NULL WHERE id = ?1 AND state = 'sending' AND lease_at = ?2",
            params![id, lease_at, error],
        )? > 0)
    }

    /// Put a row back in the queue without spending an attempt. Used when the account is halted or
    /// when a local pre-check stopped before any network call. Conditional on the lease.
    pub fn release_outbox(&self, id: &str, send_at: i64, lease_at: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'queued', attempts = CASE WHEN attempts > 0 THEN attempts - 1 ELSE 0 END,
                 send_at = ?2, lease_at = NULL
             WHERE id = ?1 AND state = 'sending' AND lease_at = ?3",
            params![id, send_at, lease_at],
        )? > 0)
    }

    /// Put a row back in the queue with a new due time. Used for exponential backoff. Conditional on
    /// the lease.
    pub fn reschedule_outbox(&self, id: &str, send_at: i64, error: Option<&str>, lease_at: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'queued', last_error = ?3, send_at = ?4, lease_at = NULL
             WHERE id = ?1 AND state = 'sending' AND lease_at = ?2",
            params![id, lease_at, error, send_at],
        )? > 0)
    }

    /// Keep `append_pending` and make the append due later. The submission is never repeated.
    /// Conditional on the append claim, and it releases the claim so the row is claimable again at
    /// the new due time.
    pub fn reschedule_outbox_append(&self, id: &str, send_at: i64, error: Option<&str>, lease_at: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET last_error = ?3, send_at = ?4, lease_at = NULL
             WHERE id = ?1 AND state = 'append_pending' AND lease_at = ?2",
            params![id, lease_at, error, send_at],
        )? > 0)
    }

    /// Move a still-queued row's due time. The schedule editor and a test that models "the due time
    /// passed while the app was closed" use it; a leased or finished row is refused.
    pub fn set_outbox_due(&self, id: &str, send_at: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute("UPDATE outbox SET send_at = ?2 WHERE id = ?1 AND state = 'queued'", params![id, send_at])? > 0)
    }

    /// Discard a row that has not been delivered. `queued` is the undo window; `failed` is a
    /// permanent refusal, so no delivery happened and throwing the journal row away is safe. A
    /// `sending`, `append_pending`, `sent` or `uncertain` row is refused: it may have been delivered.
    pub fn cancel_outbox(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'cancelled', last_error = NULL, lease_at = NULL WHERE id = ?1 AND state IN ('queued', 'failed')",
            [id],
        )? > 0)
    }

    /// Manual retry of a `failed` or `uncertain` row: reset the attempt budget and make it due now.
    /// The caller owns the explicit confirmation; this method never decides policy. A failed row
    /// whose bytes are already submitted never reaches here: an append failure stays
    /// `append_pending`, which this method refuses.
    pub fn retry_outbox(&self, id: &str, now_ms: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'queued', attempts = 0, send_at = ?2, last_error = NULL, lease_at = NULL
             WHERE id = ?1 AND state IN ('failed', 'uncertain')",
            params![id, now_ms],
        )? > 0)
    }

    /// Delete at most `limit` terminal rows older than `retention_ms`, so the journal cannot grow
    /// forever. Only `sent`, `cancelled` and `failed` rows are candidates: everything else is live
    /// state or the record of a possible delivery.
    pub fn prune_outbox(&self, now_ms: i64, retention_ms: i64, limit: u32) -> Result<u32, StoreError> {
        let cutoff = now_ms.saturating_sub(retention_ms.max(0));
        let limit = i64::from(limit.clamp(1, MAX_OUTBOX_ROWS));
        let changed = self.conn.execute(
            "DELETE FROM outbox WHERE rowid IN (
                 SELECT rowid FROM outbox WHERE state IN ('sent', 'cancelled', 'failed') AND created_at < ?1
                 ORDER BY created_at LIMIT ?2
             )",
            params![cutoff, limit],
        )?;
        Ok(u32::try_from(changed).unwrap_or(u32::MAX))
    }
}

/// The fixed sentence written when a crash leaves a `sending` row behind. It never contains an
/// address, a subject, a server name or server text.
pub const UNCERTAIN_AFTER_CRASH: &str =
    "Postino si è fermato durante l'invio; il messaggio potrebbe essere stato accettato. Controlla Inviati prima di riprovare.";

// ---------------------------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Insert or replace a folder. `unread` and `total` are a cache: message mutations refresh them
    /// and [`Store::recount_folder`] repairs them after a bulk import.
    pub fn upsert_folder(&self, folder: &Folder) -> Result<(), StoreError> {
        upsert_folder_row(&self.conn, folder)?;
        Ok(())
    }

    /// Folders for one account, or every folder when `account_id` is `None`, ordered by name.
    pub fn list_folders(&self, account_id: Option<&str>) -> Result<Vec<Folder>, StoreError> {
        let rows = match account_id {
            Some(account) => {
                let mut stmt =
                    self.conn.prepare(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE account_id = ?1 ORDER BY name COLLATE NOCASE, id"))?;
                let rows = stmt.query_map([account], row_to_folder)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("folders", error))?
            }
            None => {
                let mut stmt = self.conn.prepare(&format!("SELECT {FOLDER_COLUMNS} FROM folders ORDER BY account_id, name COLLATE NOCASE, id"))?;
                let rows = stmt.query_map([], row_to_folder)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("folders", error))?
            }
        };
        Ok(rows)
    }

    /// Delete a folder only when no message references it. Used to undo a freshly created local
    /// import folder whose whole archive was rejected.
    pub fn delete_empty_folder(&self, id: &str) -> Result<bool, StoreError> {
        let deleted = self.conn.execute("DELETE FROM folders WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM messages WHERE folder_id = ?1)", [id])?;
        Ok(deleted > 0)
    }

    pub fn get_folder(&self, id: &str) -> Result<Option<Folder>, StoreError> {
        self.conn
            .query_row(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE id = ?1"), [id], row_to_folder)
            .optional()
            .map_err(|error| corrupt_on_decode("folders", error))
    }

    /// Recompute and store a folder's `unread`/`total` from `messages`, then return them.
    /// Rebuild the cached unread/total counts of every folder of one account. Rules update many
    /// messages in one pass; recounting after the pass avoids a folder scan per action.
    pub fn recount_account_folders(&self, account_id: &str) -> Result<u32, StoreError> {
        let ids = {
            let mut stmt = self.conn.prepare("SELECT id FROM folders WHERE account_id = ?1")?;
            stmt.query_map([account_id], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let tx = self.conn.unchecked_transaction()?;
        for id in &ids {
            refresh_folder_counts(&tx, id)?;
        }
        tx.commit()?;
        u32::try_from(ids.len()).map_err(|_| StoreError::OutOfRange { field: "folder count" })
    }

    pub fn recount_folder(&self, id: &str) -> Result<MessageCounts, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        require_folder(&tx, id)?;
        refresh_folder_counts(&tx, id)?;
        let counts = folder_counts(&tx, id)?;
        tx.commit()?;
        Ok(counts)
    }
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Insert or replace a message row.
    ///
    /// Label assignments and attachment metadata live in other tables and are deliberately left
    /// untouched: a sync refresh must not lose local labels. Attachment presence is refreshed by
    /// [`Store::replace_attachments`].
    pub fn upsert_message(&self, message: &Message) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let previous: Option<String> =
            tx.query_row("SELECT thread_id FROM messages WHERE id = ?1", [&message.summary.id], |row| row.get(0)).optional()?;
        upsert_message_row(&tx, message, true)?;
        threads::refresh_thread_in_tx(&tx, &message.summary.account_id, &message.summary.thread_id)?;
        if let Some(previous) = previous
            && previous != message.summary.thread_id
        {
            threads::refresh_thread_in_tx(&tx, &message.summary.account_id, &previous)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError> {
        let message = self
            .conn
            .query_row(&format!("SELECT {SUMMARY_COLUMNS}, {MESSAGE_DETAIL_COLUMNS} FROM messages m WHERE m.id = ?1"), [id], row_to_message)
            .optional()
            .map_err(|error| corrupt_on_decode("messages", error))?;
        let Some(mut message) = message else { return Ok(None) };
        message.summary.labels = self.labels_for(id)?;
        Ok(Some(message))
    }

    /// One message with its labels **and** attachment metadata. This is the owned read behind
    /// `Engine::message(id)`: the mailbox list holds summaries, so a body and its attachment rows
    /// are read only when somebody opens the message.
    /// The threading headers stored with a message: its parent `Message-ID` and the bounded
    /// reference chain, both as they appeared on the wire.
    pub fn message_threading(&self, id: &str) -> Result<Option<MessageThreading>, StoreError> {
        let row = self
            .conn
            .query_row("SELECT in_reply_to, references_json FROM messages WHERE id = ?1", [id], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .optional()
            .map_err(|error| corrupt_on_decode("messages", error))?;
        let Some((in_reply_to, references_json)) = row else { return Ok(None) };
        let references = match references_json {
            Some(raw) => {
                serde_json::from_str(&raw).map_err(|error| StoreError::Corrupt { table: "messages.references_json", detail: error.to_string() })?
            }
            None => Vec::new(),
        };
        Ok(Some(MessageThreading { in_reply_to, references }))
    }

    pub fn get_message_with_attachments(&self, id: &str) -> Result<Option<(Message, Vec<Attachment>)>, StoreError> {
        let Some(message) = self.get_message(id)? else { return Ok(None) };
        let attachments = self.attachments(id)?;
        Ok(Some((message, attachments)))
    }

    /// One page of message rows plus the totals for the filter before pagination.
    ///
    /// `filter.limit` is clamped to `1..=MAX_PAGE`; `offset` is a row offset. `total` and `unread`
    /// describe the filtered scope, so a `unread = Some(true)` filter reports `unread = total`.
    pub fn list_messages(&self, filter: &MessageFilter) -> Result<MessagePage, StoreError> {
        let (where_sql, values) = message_where(filter);
        let (total, unread): (i64, i64) = self.conn.query_row(
            &format!("SELECT COUNT(*), COALESCE(SUM(m.unread), 0) FROM messages m{where_sql}"),
            params_from_iter(values.iter()),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let limit = filter.limit.clamp(1, MAX_PAGE);
        let mut page_values = values;
        page_values.push(Value::Integer(i64::from(limit)));
        page_values.push(Value::Integer(i64::from(filter.offset)));
        let mut stmt =
            self.conn.prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM messages m{where_sql} ORDER BY m.date DESC, m.id ASC LIMIT ? OFFSET ?"))?;
        let rows = stmt.query_map(params_from_iter(page_values.iter()), row_to_summary)?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("messages", error))?;
        drop(stmt);
        let labels = self.labels_by_message(&messages.iter().map(|message| message.id.clone()).collect::<Vec<_>>())?;
        for message in &mut messages {
            if let Some(ids) = labels.get(&message.id) {
                message.labels.clone_from(ids);
            }
        }
        Ok(MessagePage { messages, total: to_count(total)?, unread: to_count(unread)? })
    }

    /// Every message with its body and label ids, newest first.
    ///
    /// This is the mailbox-hydration read: one query for the message rows and one grouped query for
    /// their labels, no matter how many messages there are. It is deliberately unpaged because the
    /// caller asked for the whole mailbox; agent-facing lists keep using [`Store::list_messages`],
    /// whose page size is capped.
    pub fn list_all_messages(&self) -> Result<Vec<Message>, StoreError> {
        let mut stmt =
            self.conn.prepare(&format!("SELECT {SUMMARY_COLUMNS}, {MESSAGE_DETAIL_COLUMNS} FROM messages m ORDER BY m.date DESC, m.id ASC"))?;
        let rows = stmt.query_map([], row_to_message)?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("messages", error))?;
        drop(stmt);
        let labels = self.labels_by_message_all()?;
        for message in &mut messages {
            if let Some(ids) = labels.get(&message.summary.id) {
                message.summary.labels.clone_from(ids);
            }
        }
        Ok(messages)
    }

    /// Every message summary with its label ids, newest first, **without bodies**.
    ///
    /// This is the mailbox-hydration read: one query for the list-row columns and one grouped query
    /// for their labels, no matter how many messages there are. A body, HTML part or attachment row
    /// is read only through [`Store::get_message_with_attachments`] when the caller opens a message.
    pub fn list_all_summaries(&self) -> Result<Vec<MessageSummary>, StoreError> {
        let mut stmt =
            self.conn.prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM messages m INDEXED BY messages_list_cover ORDER BY m.date DESC, m.id ASC"))?;
        let rows = stmt.query_map([], row_to_summary)?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("messages", error))?;
        drop(stmt);
        let labels = self.labels_by_message_all()?;
        for message in &mut messages {
            if let Some(ids) = labels.get(&message.id) {
                message.labels.clone_from(ids);
            }
        }
        Ok(messages)
    }

    /// Totals for one folder, or for the whole mailbox when `folder_id` is `None`.
    pub fn counts(&self, folder_id: Option<&str>) -> Result<MessageCounts, StoreError> {
        let (total, unread): (i64, i64) = match folder_id {
            Some(id) => self.conn.query_row("SELECT COUNT(*), COALESCE(SUM(unread), 0) FROM messages WHERE folder_id = ?1", [id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?,
            None => self.conn.query_row("SELECT COUNT(*), COALESCE(SUM(unread), 0) FROM messages", [], |row| Ok((row.get(0)?, row.get(1)?)))?,
        };
        Ok(MessageCounts { total: to_count(total)?, unread: to_count(unread)? })
    }

    /// Change read/starred flags on a batch. All ids must exist; the batch is one transaction.
    pub fn set_flags(&self, ids: &[String], update: FlagUpdate) -> Result<u32, StoreError> {
        if ids.is_empty() || (update.unread.is_none() && update.starred.is_none()) {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut touched = BTreeSet::new();
        let mut thread_touches: BTreeSet<(String, String)> = BTreeSet::new();
        let mut changed = 0usize;
        for id in ids {
            let row: Option<(String, String)> =
                tx.query_row("SELECT folder_id, thread_id FROM messages WHERE id = ?1", [id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
            let Some((folder, thread)) = row else {
                return Err(StoreError::NotFound { entity: "message", id: id.clone() });
            };
            let account: String = tx.query_row("SELECT account_id FROM folders WHERE id = ?1", [&folder], |row| row.get(0))?;
            tx.execute(
                "UPDATE messages SET unread = COALESCE(?1, unread), starred = COALESCE(?2, starred) WHERE id = ?3",
                params![update.unread, update.starred, id],
            )?;
            if update.unread.is_some() {
                touched.insert(folder);
            }
            if update.unread.is_some() || update.starred.is_some() {
                thread_touches.insert((account, thread));
            }
            changed += 1;
        }
        for folder in touched {
            refresh_folder_counts(&tx, &folder)?;
        }
        for (account, thread) in thread_touches {
            threads::refresh_thread_in_tx(&tx, &account, &thread)?;
        }
        tx.commit()?;
        changed_count(changed)
    }

    /// Move a batch to another folder. All ids and the destination must exist; one transaction.
    pub fn move_messages(&self, ids: &[String], folder_id: &str) -> Result<u32, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let folder_account: Option<String> =
            tx.query_row("SELECT account_id FROM folders WHERE id = ?1", [folder_id], |row| row.get(0)).optional()?;
        let Some(folder_account) = folder_account else {
            return Err(StoreError::NotFound { entity: "folder", id: folder_id.to_owned() });
        };
        let mut touched = BTreeSet::from([folder_id.to_owned()]);
        let mut moved = 0usize;
        for id in ids {
            let previous: Option<(String, String)> =
                tx.query_row("SELECT folder_id, account_id FROM messages WHERE id = ?1", [id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
            let Some((previous, message_account)) = previous else {
                return Err(StoreError::NotFound { entity: "message", id: id.clone() });
            };
            if message_account != folder_account {
                return Err(StoreError::AccountMismatch { folder_id: folder_id.to_owned(), message_account, folder_account: folder_account.clone() });
            }
            tx.execute("UPDATE messages SET folder_id = ?1 WHERE id = ?2", params![folder_id, id])?;
            touched.insert(previous);
            moved += 1;
        }
        for folder in touched {
            refresh_folder_counts(&tx, &folder)?;
        }
        tx.commit()?;
        changed_count(moved)
    }

    /// Move one rule candidate only while it is still received mail in `expected_folder`. The
    /// check and the update share one transaction, so a concurrent move to Sent/Drafts/Trash cannot
    /// be overwritten by a rule that read the row a moment earlier.
    pub fn rule_move_message(&self, id: &str, expected_folder: &str, folder_id: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let target_account: Option<String> =
            tx.query_row("SELECT account_id FROM folders WHERE id = ?1", [folder_id], |row| row.get(0)).optional()?;
        let Some(target_account) = target_account else {
            return Err(StoreError::NotFound { entity: "folder", id: folder_id.to_owned() });
        };
        let row: Option<(String, String, String)> = tx
            .query_row("SELECT m.folder_id, m.account_id, m.origin FROM messages m WHERE m.id = ?1", [id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        let Some((previous, account, origin)) = row else {
            return Err(StoreError::NotFound { entity: "message", id: id.to_owned() });
        };
        if account != target_account {
            return Err(StoreError::AccountMismatch { folder_id: folder_id.to_owned(), message_account: account, folder_account: target_account });
        }
        if previous != expected_folder || origin != MessageOrigin::Received.as_str() {
            return Ok(false);
        }
        tx.execute("UPDATE messages SET folder_id = ?1 WHERE id = ?2", params![folder_id, id])?;
        // The rule pass recounts every folder once at the end; a folder scan per moved message would
        // make a 2 000-message run quadratic. A move does not change a thread's unread/starred
        // counts, and the run refreshes the thread cache once if a flag changed.
        tx.commit()?;
        Ok(true)
    }

    /// Set flags on one rule candidate only while it is still received mail in `expected_folder`.
    pub fn rule_set_flags(&self, id: &str, expected_folder: &str, update: FlagUpdate) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let row: Option<(String, String)> =
            tx.query_row("SELECT folder_id, origin FROM messages WHERE id = ?1", [id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        let Some((folder, origin)) = row else {
            return Err(StoreError::NotFound { entity: "message", id: id.to_owned() });
        };
        if folder != expected_folder || origin != MessageOrigin::Received.as_str() {
            return Ok(false);
        }
        tx.execute(
            "UPDATE messages SET unread = COALESCE(?1, unread), starred = COALESCE(?2, starred) WHERE id = ?3",
            params![update.unread, update.starred, id],
        )?;
        // Folder counts and the thread cache are repaired once after the rule pass.
        tx.commit()?;
        Ok(true)
    }

    /// Add or remove one label on a rule candidate only while it is still received mail in
    /// `expected_folder`. Returns whether a row changed.
    pub fn rule_set_label(&self, label_id: &str, id: &str, expected_folder: &str, apply: bool) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        require_label(&tx, label_id)?;
        let label_account: Option<Option<String>> =
            tx.query_row("SELECT account_id FROM labels WHERE id = ?1", [label_id], |row| row.get(0)).optional()?;
        let row: Option<(String, String, String)> = tx
            .query_row(
                "SELECT m.folder_id, m.origin, f.account_id FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((folder, origin, account)) = row else {
            return Err(StoreError::NotFound { entity: "message", id: id.to_owned() });
        };
        if label_account.flatten().is_some_and(|label_account| label_account != account) {
            return Ok(false);
        }
        if folder != expected_folder || origin != MessageOrigin::Received.as_str() {
            return Ok(false);
        }
        let changed = if apply {
            tx.execute("INSERT OR IGNORE INTO message_labels(message_id, label_id) VALUES(?1, ?2)", params![id, label_id])?
        } else {
            tx.execute("DELETE FROM message_labels WHERE message_id = ?1 AND label_id = ?2", params![id, label_id])?
        };
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Delete one message permanently. Attachments and label assignments go with it via
    /// `ON DELETE CASCADE`; the draft row and its revision counter are removed explicitly in the
    /// same transaction, so a later open cannot resurrect the draft. Returns `false` when the
    /// message row was already gone (stale draft rows are still cleaned up).
    pub fn delete_message(&self, id: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let row: Option<(String, String, String)> = tx
            .query_row(
                "SELECT m.folder_id, m.thread_id, f.account_id FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        tx.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM draft_seq WHERE message_id = ?1", [id])?;
        tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{REPLY_TO_KEY_PREFIX}{id}")])?;
        tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{UNSUBSCRIBE_KEY_PREFIX}{id}")])?;
        tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{REMOTE_LABELS_KEY_PREFIX}{id}")])?;
        if let Some((_, _, account)) = &row {
            tx.execute("DELETE FROM meta WHERE key = ?1", [format!("rule_guard:{account}:{id}")])?;
        }
        tx.execute("DELETE FROM meta WHERE key = ?1", [format!("{}{id}", crate::rules::RULE_GUARD_KEY_PREFIX)])?;
        tx.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        if let Some((folder, thread, account)) = &row {
            refresh_folder_counts(&tx, folder)?;
            threads::refresh_thread_in_tx(&tx, account, thread)?;
        }
        tx.commit()?;
        Ok(row.is_some())
    }

    /// Insert or replace one synced message and its attachment metadata in a single transaction.
    ///
    /// This is the sync bulk path: folder `total`/`unread` are deliberately **not** refreshed per
    /// message, because a count scan per message would make a large import quadratic. The caller
    /// recounts the folder once after the pass ([`Store::recount_folder`]). A failure rolls the
    /// message row and every attachment row back together; blob files already written are
    /// unreferenced but harmless, because the blob store is content-addressed.
    pub fn upsert_synced_message(&self, message: &Message, attachments: &[Attachment]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let previous: Option<String> =
            tx.query_row("SELECT thread_id FROM messages WHERE id = ?1", [&message.summary.id], |row| row.get(0)).optional()?;
        upsert_message_row(&tx, message, false)?;
        replace_attachments_in_tx(&tx, &message.summary.id, attachments)?;
        threads::refresh_thread_in_tx(&tx, &message.summary.account_id, &message.summary.thread_id)?;
        if let Some(previous) = previous
            && previous != message.summary.thread_id
        {
            threads::refresh_thread_in_tx(&tx, &message.summary.account_id, &previous)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The sync bulk path with the message's threading headers. `in_reply_to` is the normalised
    /// parent `Message-ID`; `references` is the bounded chain in the order it appeared. Both are
    /// written in the same transaction as the message row and its attachments, so P9b never sees a
    /// cached message whose chain is missing. The thread id is derived here, from the cache and the
    /// message's own headers, and the cache rows it touches are refreshed before the commit.
    pub fn upsert_synced_message_with_refs(
        &self,
        message: &Message,
        attachments: &[Attachment],
        in_reply_to: Option<&str>,
        references: &[String],
    ) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let previous: Option<String> =
            tx.query_row("SELECT thread_id FROM messages WHERE id = ?1", [&message.summary.id], |row| row.get(0)).optional()?;
        upsert_message_row(&tx, message, false)?;
        replace_attachments_in_tx(&tx, &message.summary.id, attachments)?;
        let references_json = if references.is_empty() { None } else { Some(serde_json::to_string(references)?) };
        tx.execute(
            "UPDATE messages SET in_reply_to = ?1, references_json = ?2 WHERE id = ?3",
            params![in_reply_to, references_json, message.summary.id],
        )?;
        let subject_norm = normalise_subject(&message.summary.subject);
        threads::assign_thread_in_tx(
            &tx,
            &message.summary.account_id,
            &message.summary.id,
            message.message_id.as_deref(),
            in_reply_to,
            references,
            message.summary.date,
            &message.summary.subject,
            &subject_norm,
            previous.as_deref(),
        )?;
        if let Some(previous) = previous
            && tx
                .query_row("SELECT thread_id FROM messages WHERE id = ?1", [&message.summary.id], |row| row.get::<_, String>(0))
                .optional()?
                .is_some_and(|current| current != previous)
        {
            threads::refresh_thread_in_tx(&tx, &message.summary.account_id, &previous)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record, or clear, the raw blob of a received message.
    ///
    /// `Some(hash)` names a blob that already exists in the store's blob directory and holds the
    /// exact RFC 5322 bytes of a `multipart/signed` or `multipart/encrypted` message. `None` clears
    /// the column, so ordinary mail and a re-sync that is no longer PGP/MIME keep no stale bytes
    /// that could verify the wrong message. A pre-v9 row stays NULL until a refresh stores one.
    pub fn set_message_raw_blob_hash(&self, id: &str, hash: Option<&str>) -> Result<(), StoreError> {
        let changed = self.conn.execute("UPDATE messages SET raw_blob_hash = ?1 WHERE id = ?2", params![hash, id])?;
        if changed == 0 {
            return Err(StoreError::NotFound { entity: "message", id: id.to_owned() });
        }
        Ok(())
    }

    /// The raw blob hash of a received message, or `None` when no exact bytes were kept.
    ///
    /// `None` is the honest `Not stored` state: an ordinary message, a row cached before v9, or a
    /// PGP/MIME message whose bytes were rejected. It is never re-derived from the parsed body.
    pub fn message_raw_blob_hash(&self, id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.conn.query_row("SELECT raw_blob_hash FROM messages WHERE id = ?1", [id], |row| row.get::<_, Option<String>>(0)).optional()?.flatten())
    }

    /// The folder and local read/star flags of a cached message, or `None` when it is new.
    pub fn message_flags(&self, id: &str) -> Result<Option<MessageFlags>, StoreError> {
        self.conn
            .query_row("SELECT folder_id, unread, starred FROM messages WHERE id = ?1", [id], |row| {
                Ok(MessageFlags { folder_id: row.get(0)?, unread: row.get(1)?, starred: row.get(2)? })
            })
            .optional()
            .map_err(|error| corrupt_on_decode("messages", error))
    }

    /// The ingestion instant of one row, for the `all` page cursor.
    pub fn message_ingested_at(&self, id: &str) -> Result<Option<i64>, StoreError> {
        Ok(self.conn.query_row("SELECT ingested_at FROM messages WHERE id = ?1", [id], |row| row.get(0)).optional()?)
    }

    /// Why one row exists. Used by rules and by the archived-Sent regression tests.
    pub fn message_origin(&self, id: &str) -> Result<Option<MessageOrigin>, StoreError> {
        let raw: Option<String> = self.conn.query_row("SELECT origin FROM messages WHERE id = ?1", [id], |row| row.get(0)).optional()?;
        Ok(raw.as_deref().and_then(MessageOrigin::parse))
    }

    /// The account a label belongs to, or `None` for a mailbox-wide label.
    pub fn label_account_id(&self, id: &str) -> Result<Option<Option<String>>, StoreError> {
        self.conn.query_row("SELECT account_id FROM labels WHERE id = ?1", [id], |row| row.get(0)).optional().map_err(StoreError::Sqlite)
    }

    /// The RFC 5322 `Message-ID` of one row, without reading its body or attachments.
    pub fn message_header_id(&self, id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.conn.query_row("SELECT message_id FROM messages WHERE id = ?1", [id], |row| row.get(0)).optional()?)
    }

    /// The id of a locally written Sent/Archive row in this account that carries `message_id` and no
    /// UID yet.
    ///
    /// The submission path stores the generated Message-ID on the local Sent copy; a sync that fetches
    /// the same message finds it here and updates it with the UID instead of inserting a second copy.
    /// Account-wide (not folder-scoped) so a server copy that moved to Sent/Archive still reconciles,
    /// but deliberately **not** Drafts or Trash: a Sent copy the user moved back to Drafts is being
    /// edited, and a sync must neither move it back to Sent nor delete it. `uid IS NULL` is what makes
    /// the lookup safe: a row already bound to a server UID uses the normal identity rules.
    pub fn local_message_for_id(&self, account_id: &str, message_id: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT m.id FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE m.account_id = ?1 AND m.message_id = ?2 AND m.uid IS NULL
                   AND f.kind IN ('sent', 'archive')
                 ORDER BY m.id LIMIT 1",
                params![account_id, message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| corrupt_on_decode("messages", error))
    }

    /// The id of a server-backed row in this folder with the same Message-ID, if any.
    ///
    /// The submission path calls this before writing the local Sent copy: a provider that auto-saved
    /// the message (or a sync that ran between the server's `250` and the local move) has already
    /// created the row, and writing a second one would show the message twice.
    pub fn synced_message_for_id(&self, account_id: &str, folder_id: &str, message_id: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT id FROM messages
                 WHERE account_id = ?1 AND folder_id = ?2 AND message_id = ?3 AND uid IS NOT NULL
                 ORDER BY uid, id LIMIT 1",
                params![account_id, folder_id, message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| corrupt_on_decode("messages", error))
    }

    /// The cached UIDs of one folder, ascending, ignoring rows without a UID (local drafts).
    ///
    /// The deletion pass compares this list with the server's and removes the difference. Message
    /// rows with `uid = NULL` were never on the server and are not candidates.
    pub fn list_uids(&self, folder_id: &str) -> Result<Vec<UidEntry>, StoreError> {
        require_folder(&self.conn, folder_id)?;
        let mut stmt = self.conn.prepare("SELECT id, uid FROM messages WHERE folder_id = ?1 AND uid IS NOT NULL ORDER BY uid, id")?;
        let rows = stmt.query_map([folder_id], |row| Ok(UidEntry { message_id: row.get(0)?, uid: get_u32(row, 1)? }))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("messages", error))
    }

    /// Every message row of one folder with its UID and local read/star flags, in one query.
    ///
    /// This is the sync snapshot: duplicate `Message-ID` disambiguation, drift comparison and the
    /// deletion candidates all read from it, so a folder pass costs one statement instead of one
    /// lookup per message. Rows without a UID (local drafts and locally sent mail) are included so
    /// the caller can protect them; they are never deletion candidates.
    pub fn folder_message_flags(&self, folder_id: &str) -> Result<Vec<FolderMessageFlags>, StoreError> {
        require_folder(&self.conn, folder_id)?;
        let mut stmt = self.conn.prepare("SELECT id, uid, unread, starred FROM messages WHERE folder_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([folder_id], |row| {
            Ok(FolderMessageFlags { message_id: row.get(0)?, uid: get_opt_u32(row, 1)?, unread: row.get(2)?, starred: row.get(3)? })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("messages", error))
    }

    /// Delete cached messages that are gone from the server, in one transaction.
    ///
    /// A local draft is never deleted here even when its id is requested: a folder can hold local
    /// drafts that the server has never seen, and a deletion pass must not be able to destroy work
    /// that was never uploaded. Only rows in `folder_id` are touched; attachment rows and label
    /// assignments go with the message via `ON DELETE CASCADE`. Returns the number removed and
    /// refreshes the folder counts.
    pub fn delete_synced_messages(&self, folder_id: &str, message_ids: &[String]) -> Result<u32, StoreError> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        require_folder(&tx, folder_id)?;
        let account: String = tx.query_row("SELECT account_id FROM folders WHERE id = ?1", [folder_id], |row| row.get(0))?;
        let mut removed = 0usize;
        let mut touched_threads = Vec::new();
        for id in message_ids {
            let is_draft: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM drafts WHERE id = ?1)", [id], |row| row.get(0))?;
            if is_draft {
                continue;
            }
            let thread: Option<String> = tx
                .query_row("SELECT thread_id FROM messages WHERE id = ?1 AND folder_id = ?2", params![id, folder_id], |row| row.get(0))
                .optional()?;
            let deleted = tx.execute("DELETE FROM messages WHERE id = ?1 AND folder_id = ?2", params![id, folder_id])?;
            if deleted > 0 {
                tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{REPLY_TO_KEY_PREFIX}{id}")])?;
                tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{UNSUBSCRIBE_KEY_PREFIX}{id}")])?;
                tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{REMOTE_LABELS_KEY_PREFIX}{id}")])?;
                tx.execute("DELETE FROM meta WHERE key IN (?1, ?2)", params![format!("rule_guard:{account}:{id}"), format!("rule_guard:{id}")])?;
                if let Some(thread) = thread {
                    touched_threads.push(thread);
                }
            }
            removed += deleted;
        }
        if removed > 0 {
            refresh_folder_counts(&tx, folder_id)?;
        }
        threads::refresh_threads_in_tx(&tx, &account, touched_threads)?;
        tx.commit()?;
        changed_count(removed)
    }

    /// Drop every cached server message of a folder and its sync cursor, in one transaction.
    ///
    /// This is the `UIDVALIDITY` reset: the server renumbered the folder, so every cached UID is
    /// meaningless. Label assignments and attachment rows cascade; blob files stay (another message
    /// may share the content and there is no collector yet). Rows without a UID were never on the
    /// server and survive: local drafts and locally sent mail must not be discarded by a rebuild of
    /// the server cache. Folder counts are refreshed in the same transaction.
    pub fn reset_folder_messages(&self, folder_id: &str) -> Result<u32, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        require_folder(&tx, folder_id)?;
        let account: String = tx.query_row("SELECT account_id FROM folders WHERE id = ?1", [folder_id], |row| row.get(0))?;
        let mut touched_threads = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT thread_id FROM messages
                 WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)",
            )?;
            let rows = stmt.query_map([folder_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                touched_threads.push(row?);
            }
        }
        tx.execute(
            "DELETE FROM meta WHERE key IN (
                 SELECT 'reply_to:' || id FROM messages
                 WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)
             )",
            [folder_id],
        )?;
        tx.execute(
            "DELETE FROM meta WHERE key IN (
                 SELECT 'unsubscribe:' || id FROM messages
                 WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)
             )",
            [folder_id],
        )?;
        tx.execute(
            "DELETE FROM meta WHERE key IN (
                 SELECT 'remote_labels:' || id FROM messages
                 WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)
             )",
            [folder_id],
        )?;
        tx.execute(
            "DELETE FROM meta WHERE key IN (
                 SELECT 'rule_guard:' || ?2 || ':' || id FROM messages
                 WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)
             ) OR key IN (
                 SELECT 'rule_guard:' || id FROM messages
                 WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)
             )",
            params![folder_id, account],
        )?;
        let removed =
            tx.execute("DELETE FROM messages WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)", [folder_id])?;
        tx.execute("DELETE FROM sync_state WHERE folder_id = ?1", [folder_id])?;
        refresh_folder_counts(&tx, folder_id)?;
        threads::refresh_threads_in_tx(&tx, &account, touched_threads)?;
        tx.commit()?;
        changed_count(removed)
    }

    /// The per-folder sync cursor, or `None` when the folder has never been synced.
    pub fn get_sync_state(&self, folder_id: &str) -> Result<Option<SyncState>, StoreError> {
        self.conn
            .query_row("SELECT folder_id, uid_next, highest_modseq, last_error FROM sync_state WHERE folder_id = ?1", [folder_id], |row| {
                Ok(SyncState { folder_id: row.get(0)?, uid_next: get_u32(row, 1)?, highest_modseq: get_opt_u64(row, 2)?, last_error: row.get(3)? })
            })
            .optional()
            .map_err(|error| corrupt_on_decode("sync_state", error))
    }

    /// Write the per-folder sync cursor. The folder must exist; one transaction, upsert semantics.
    pub fn set_sync_state(&self, state: &SyncState) -> Result<(), StoreError> {
        let highest = state.highest_modseq.map(|value| to_i64(value, "highest_modseq")).transpose()?;
        let tx = self.conn.unchecked_transaction()?;
        require_folder(&tx, &state.folder_id)?;
        tx.execute(
            "INSERT INTO sync_state(folder_id, uid_next, highest_modseq, last_error) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(folder_id) DO UPDATE SET
                 uid_next = excluded.uid_next,
                 highest_modseq = excluded.highest_modseq,
                 last_error = excluded.last_error",
            params![state.folder_id, i64::from(state.uid_next), highest, state.last_error],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Remove one folder's sync cursor, leaving its messages alone.
    pub fn clear_sync_state(&self, folder_id: &str) -> Result<(), StoreError> {
        self.conn.execute("DELETE FROM sync_state WHERE folder_id = ?1", [folder_id])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Create a label. `id` is caller-owned and stable; a duplicate name (case-sensitive, in the
    /// same account scope) is [`StoreError::Duplicate`]. The `label_seq` high-water mark is raised
    /// to this id's number in the same transaction, so deleting the highest label and reopening
    /// cannot hand the id out again.
    pub fn create_label(&self, label: &Label) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(account) = &label.account_id {
            require_account(&tx, account)?;
        }
        tx.execute(
            "INSERT INTO labels(id, account_id, name, color) VALUES(?1, ?2, ?3, ?4)",
            params![label.id, label.account_id, label.name, label.color],
        )
        .map_err(|error| duplicate("label", &label.name, error))?;
        let label_number = label.id.strip_prefix("lbl_").and_then(|suffix| suffix.parse::<u32>().ok()).unwrap_or(0);
        let stored: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", [crate::LABEL_SEQ_KEY], |row| row.get(0)).optional()?;
        let current = stored.and_then(|value| value.parse::<u32>().ok()).unwrap_or(0);
        let high = current.max(label_number);
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![crate::LABEL_SEQ_KEY, high.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Rename and/or recolour a label. `None` keeps the current value; the updated row is returned.
    pub fn update_label(&self, id: &str, name: Option<&str>, color: Option<&str>) -> Result<Label, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx
            .execute("UPDATE labels SET name = COALESCE(?1, name), color = COALESCE(?2, color) WHERE id = ?3", params![name, color, id])
            .map_err(|error| duplicate("label", name.unwrap_or_default(), error))?;
        if changed == 0 {
            return Err(StoreError::NotFound { entity: "label", id: id.to_owned() });
        }
        let label = get_label(&tx, id)?.ok_or_else(|| StoreError::NotFound { entity: "label", id: id.to_owned() })?;
        tx.commit()?;
        Ok(label)
    }

    /// Delete a label and every assignment in the same transaction. Messages stay untouched.
    /// Returns `false` when the label was already gone.
    pub fn delete_label(&self, id: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM message_labels WHERE label_id = ?1", [id])?;
        let removed = tx.execute("DELETE FROM labels WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(removed == 1)
    }

    pub fn get_label(&self, id: &str) -> Result<Option<Label>, StoreError> {
        get_label(&self.conn, id)
    }

    /// Find a label by display name, the identity Gmail's `X-GM-LABELS` carries.
    ///
    /// Engine-created labels are mailbox-wide (`account_id IS NULL`); a label made by an older
    /// account-scoped path is still reused before creating a duplicate.
    pub fn label_named(&self, account_id: &str, name: &str) -> Result<Option<Label>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {LABEL_COLUMNS} FROM labels l
                     WHERE l.name = ?2 COLLATE NOCASE AND (l.account_id IS NULL OR l.account_id = ?1)
                     ORDER BY l.account_id IS NULL DESC LIMIT 1"
                ),
                params![account_id, name],
                row_to_label,
            )
            .optional()
            .map_err(StoreError::Sqlite)
    }

    /// Return the mailbox-wide label called `name`, creating it with a deterministic provider id
    /// when a sync sees it for the first time. The id is not numeric, so it can never collide with
    /// the `lbl_<n>` sequence the user-facing create path allocates.
    pub fn ensure_account_label(&self, account_id: &str, name: &str, color: &str) -> Result<Label, StoreError> {
        if let Some(label) = self.label_named(account_id, name)? {
            return Ok(label);
        }
        let digest = blake3::hash(name.to_lowercase().as_bytes()).to_hex().to_string();
        let id = format!("lbl_g_{}", &digest[..24]);
        self.conn
            .execute(
                "INSERT OR IGNORE INTO labels(id, account_id, name, color) SELECT ?1, NULL, ?2, ?3 WHERE EXISTS(SELECT 1 FROM accounts WHERE id = ?4)",
                params![id, name, color, account_id],
            )
            .map_err(StoreError::Sqlite)?;
        self.label_named(account_id, name)?.ok_or_else(|| StoreError::NotFound { entity: "account", id: account_id.to_owned() })
    }

    /// Total number of labels, both mailbox-wide and account-scoped. Sync uses it to bound how many
    /// labels a hostile provider can mint across many messages.
    pub fn label_count(&self) -> Result<usize, StoreError> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM labels", [], |row| row.get(0))?;
        usize::try_from(count).map_err(|_| StoreError::Corrupt { table: "labels", detail: "negative label count".to_owned() })
    }

    /// Labels visible to an account: mailbox-wide labels plus that account's own labels. Counts are
    /// live (computed from `message_labels`), never cached.
    pub fn list_labels(&self, account_id: Option<&str>) -> Result<Vec<LabelCount>, StoreError> {
        let sql = format!(
            "SELECT {LABEL_COLUMNS},
                    (SELECT COUNT(*) FROM message_labels ml WHERE ml.label_id = l.id),
                    (SELECT COUNT(*) FROM message_labels ml JOIN messages m ON m.id = ml.message_id
                     WHERE ml.label_id = l.id AND m.unread = 1)
             FROM labels l"
        );
        let rows = match account_id {
            Some(account) => {
                let mut stmt =
                    self.conn.prepare(&format!("{sql} WHERE l.account_id IS NULL OR l.account_id = ?1 ORDER BY l.name COLLATE NOCASE, l.id"))?;
                let rows = stmt.query_map([account], row_to_label_count)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("labels", error))?
            }
            None => {
                let mut stmt = self.conn.prepare(&format!("{sql} ORDER BY l.name COLLATE NOCASE, l.id"))?;
                let rows = stmt.query_map([], row_to_label_count)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("labels", error))?
            }
        };
        Ok(rows)
    }

    /// Apply one label to a batch of messages in one transaction. Applying twice is a no-op, and a
    /// single unknown message id rolls the whole batch back. Returns the number of new assignments.
    pub fn apply_label(&self, label_id: &str, message_ids: &[String]) -> Result<u32, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        require_label(&tx, label_id)?;
        let mut added = 0usize;
        for id in message_ids {
            require_message(&tx, id)?;
            let changed = tx.execute("INSERT OR IGNORE INTO message_labels(message_id, label_id) VALUES(?1, ?2)", params![id, label_id])?;
            if changed == 1 {
                added += 1;
            }
        }
        tx.commit()?;
        changed_count(added)
    }

    /// Remove one label from a batch in one transaction. Removing a missing assignment is a no-op;
    /// a single unknown message id rolls the whole batch back. Returns the number removed.
    pub fn remove_label(&self, label_id: &str, message_ids: &[String]) -> Result<u32, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        require_label(&tx, label_id)?;
        let mut removed = 0usize;
        for id in message_ids {
            require_message(&tx, id)?;
            let changed = tx.execute("DELETE FROM message_labels WHERE message_id = ?1 AND label_id = ?2", params![id, label_id])?;
            if changed == 1 {
                removed += 1;
            }
        }
        tx.commit()?;
        changed_count(removed)
    }

    /// Every message carrying a label, newest first. Unlike [`Store::list_messages`] this is not
    /// paginated; agent-facing callers should page with a label filter instead.
    pub fn messages_by_label(&self, label_id: &str) -> Result<Vec<MessageSummary>, StoreError> {
        require_label(&self.conn, label_id)?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m
             WHERE EXISTS(SELECT 1 FROM message_labels ml WHERE ml.message_id = m.id AND ml.label_id = ?1)
             ORDER BY m.date DESC, m.id ASC"
        ))?;
        let rows = stmt.query_map([label_id], row_to_summary)?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("messages", error))?;
        drop(stmt);
        let labels = self.labels_by_message(&messages.iter().map(|message| message.id.clone()).collect::<Vec<_>>())?;
        for message in &mut messages {
            if let Some(ids) = labels.get(&message.id) {
                message.labels.clone_from(ids);
            }
        }
        Ok(messages)
    }

    /// Label ids attached to one message, ordered by id.
    pub fn labels_for(&self, message_id: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT label_id FROM message_labels WHERE message_id = ?1 ORDER BY label_id")?;
        let rows = stmt.query_map([message_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn labels_by_message(&self, message_ids: &[String]) -> Result<HashMap<String, Vec<String>>, StoreError> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; message_ids.len()].join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT message_id, label_id FROM message_labels WHERE message_id IN ({placeholders}) ORDER BY message_id, label_id"
        ))?;
        let rows = stmt.query_map(params_from_iter(message_ids.iter()), |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut labels: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (message_id, label_id) = row?;
            labels.entry(message_id).or_default().push(label_id);
        }
        Ok(labels)
    }

    /// Every label assignment in one query, keyed by message. Used by [`Store::list_all_messages`],
    /// where a per-message `IN` list would grow with the mailbox and eventually hit SQLite's bound
    /// parameter limit.
    fn labels_by_message_all(&self) -> Result<HashMap<String, Vec<String>>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT message_id, label_id FROM message_labels ORDER BY message_id, label_id")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut labels: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (message_id, label_id) = row?;
            labels.entry(message_id).or_default().push(label_id);
        }
        Ok(labels)
    }
}

// ---------------------------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Replace every attachment row for a message in one transaction and refresh
    /// `messages.has_attachments`. Metadata only: bytes live outside the database.
    pub fn replace_attachments(&self, message_id: &str, attachments: &[Attachment]) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        require_message(&tx, message_id)?;
        replace_attachments_in_tx(&tx, message_id, attachments)?;
        tx.commit()?;
        Ok(())
    }

    /// Attachment metadata for a message, in the order it was written (MIME part order).
    pub fn attachments(&self, message_id: &str) -> Result<Vec<Attachment>, StoreError> {
        require_message(&self.conn, message_id)?;
        let mut stmt = self.conn.prepare(&format!("SELECT {ATTACHMENT_COLUMNS} FROM attachments WHERE message_id = ?1 ORDER BY rowid"))?;
        let rows = stmt.query_map([message_id], row_to_attachment)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every stored attachment hash, keyed by attachment id, in one small query. The engine hydrates
    /// this map instead of every attachment row: opening a 100k mailbox must not build an attachment
    /// tree, and the hashes are only needed when bytes are read or a draft is snapshotted.
    pub fn list_blob_hashes(&self) -> Result<HashMap<String, String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id, blob_hash FROM attachments WHERE blob_hash IS NOT NULL")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// Every attachment row grouped by message id in one query, in MIME part order. Messages
    /// without attachments are absent from the map. This is the hydration half of
    /// [`Store::list_all_messages`].
    pub fn list_all_attachments(&self) -> Result<HashMap<String, Vec<Attachment>>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("SELECT message_id, {ATTACHMENT_COLUMNS} FROM attachments ORDER BY message_id, rowid"))?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row_to_attachment_at(row, 1)?)))?;
        let mut attachments: HashMap<String, Vec<Attachment>> = HashMap::new();
        for row in rows {
            let (message_id, attachment) = row?;
            attachments.entry(message_id).or_default().push(attachment);
        }
        Ok(attachments)
    }

    /// One attachment row by its globally unique id, paired with the id of the message it belongs
    /// to. The engine reads attachment bytes through this lookup.
    pub fn find_attachment(&self, id: &str) -> Result<Option<(String, Attachment)>, StoreError> {
        self.conn
            .query_row(&format!("SELECT message_id, {ATTACHMENT_COLUMNS} FROM attachments WHERE id = ?1"), [id], |row| {
                Ok((row.get(0)?, row_to_attachment_at(row, 1)?))
            })
            .optional()
            .map_err(|error| corrupt_on_decode("attachments", error))
    }

    /// Point an attachment at stored blob bytes and record the exact size of those bytes.
    ///
    /// The blob store hashes the plaintext, so `blob_hash` is both the file name and the integrity
    /// check. The size is written in the same statement because the read path uses it to cap the
    /// blob: a MIME-declared size can disagree with the bytes a caller stored, and the bytes are the
    /// canonical content. `None` clears the pointer without deleting the deduplicated blob.
    ///
    /// The attachment must belong to `message_id`: another message's row is
    /// [`StoreError::AttachmentOwnedByOther`], never a silent no-op.
    pub fn set_attachment_blob_hash(&self, message_id: &str, attachment_id: &str, blob_hash: Option<&str>, size: u64) -> Result<(), StoreError> {
        let size = to_i64(size, "attachment size")?;
        let owner: Option<String> =
            self.conn.query_row("SELECT message_id FROM attachments WHERE id = ?1", [attachment_id], |row| row.get(0)).optional()?;
        match owner {
            Some(owner) if owner == message_id => {}
            Some(owner) => return Err(StoreError::AttachmentOwnedByOther { id: attachment_id.to_owned(), owner }),
            None => return Err(StoreError::NotFound { entity: "attachment", id: attachment_id.to_owned() }),
        }
        self.conn.execute("UPDATE attachments SET blob_hash = ?1, size = ?2 WHERE id = ?3", params![blob_hash, size, attachment_id])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Drafts
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Save a draft revision row. A new id starts at revision 1; an existing id increments the
    /// store-owned sequence by one and takes every editable field. The sequence is written in the
    /// same transaction, so it survives a send and a later move back to Drafts.
    ///
    /// This writes only the revision row. A caller saving a real draft uses
    /// [`Store::save_draft_message`], which also writes the display message, attachments and folder
    /// counts in the same transaction; using this method alone is the half-write pattern that
    /// `Engine::open` can only repair after the fact.
    pub fn save_draft(&self, draft: &DraftUpdate) -> Result<Draft, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        require_account(&tx, &draft.account_id)?;
        let saved = save_draft_row(&tx, draft)?;
        tx.commit()?;
        Ok(saved)
    }

    /// Save a whole draft: message row, attachments, revision row and Drafts folder counts in one
    /// `BEGIN IMMEDIATE` transaction. `message.summary.id` is the draft id and
    /// `message.summary.account_id`/`folder_id` must agree with a real folder.
    ///
    /// Every attachment id is validated before the first write: ids repeated in the request and ids
    /// owned by another message are [`StoreError::DuplicateAttachment`] and
    /// [`StoreError::AttachmentOwnedByOther`], never a raw UNIQUE constraint. A `None` reply link
    /// keeps the stored one, so a field the caller does not own cannot clear it. On any error the
    /// transaction rolls back and the previous rows are exactly as they were.
    pub fn save_draft_message(&self, message: &Message, attachments: &[Attachment]) -> Result<Draft, StoreError> {
        self.save_draft_message_linked(message, attachments, None)
    }

    /// [`Store::save_draft_message`] with an explicit reply linkage.
    ///
    /// `in_reply_to` is the parent `Message-ID` without angle brackets. `None` keeps whatever the
    /// stored draft row already has, matching the column's `COALESCE` rule: a later patch that does
    /// not own the field cannot clear it.
    pub fn save_draft_message_linked(&self, message: &Message, attachments: &[Attachment], in_reply_to: Option<&str>) -> Result<Draft, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        validate_attachment_ids(&tx, &message.summary.id, attachments)?;
        upsert_message_row(&tx, message, true)?;
        replace_attachments_in_tx(&tx, &message.summary.id, attachments)?;
        let draft = draft_update_from_message(message, in_reply_to);
        let saved = save_draft_row(&tx, &draft)?;
        refresh_folder_counts(&tx, &message.summary.folder_id)?;
        threads::refresh_thread_in_tx(&tx, &message.summary.account_id, &message.summary.thread_id)?;
        tx.commit()?;
        Ok(saved)
    }

    /// Move a draft to Sent in one transaction. The draft row is deleted because the message is no
    /// longer a draft, but `draft_seq` is kept so a move back to Drafts and a new save continue the
    /// revision sequence instead of restarting at 1.
    ///
    /// `message.message_id` is the generated Message-ID. Storing it is what lets a later Sent sync
    /// recognise this locally written row (`uid IS NULL`) as the same message instead of inserting a
    /// second copy; the id itself stays stable.
    ///
    /// `merge_duplicate` names a server-backed row in the same folder with the same Message-ID (a
    /// provider auto-save or a sync that ran before this move). That row is the message already on the
    /// server, so the local row **adopts its UID** and the duplicate is removed in the same
    /// transaction: one local row that keeps the server identity, and the caller can skip the APPEND
    /// (the server copy already exists). The returned UID is `Some(uid)` when this happened.
    pub fn send_draft_message(&self, message: &Message, merge_duplicate: Option<&str>) -> Result<Option<u32>, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let old_thread: Option<String> =
            tx.query_row("SELECT thread_id FROM messages WHERE id = ?1", [&message.summary.id], |row| row.get(0)).optional()?;
        let adopted_uid = match merge_duplicate {
            Some(duplicate) if duplicate != message.summary.id => {
                tx.query_row("SELECT uid FROM messages WHERE id = ?1", [duplicate], |row| row.get::<_, Option<u32>>(0)).optional()?.flatten()
            }
            _ => None,
        };
        let mut touched_threads = vec![message.summary.thread_id.clone()];
        if let Some(old_thread) = old_thread {
            touched_threads.push(old_thread);
        }
        if let Some(duplicate) = merge_duplicate
            && duplicate != message.summary.id
            && let Some(thread) =
                tx.query_row("SELECT thread_id FROM messages WHERE id = ?1", [duplicate], |row| row.get::<_, String>(0)).optional()?
        {
            touched_threads.push(thread);
        }
        upsert_message_row(&tx, message, true)?;
        // The local draft became a Sent copy. `upsert_message_row` deliberately preserves origin on
        // an update, so this transition is the one place that makes it `sent`.
        tx.execute("UPDATE messages SET origin = 'sent' WHERE id = ?1", [&message.summary.id])?;
        if let Some(uid) = adopted_uid {
            tx.execute("UPDATE messages SET uid = ?2 WHERE id = ?1", params![message.summary.id, uid])?;
        }
        tx.execute("DELETE FROM drafts WHERE id = ?1", [&message.summary.id])?;
        if let Some(duplicate) = merge_duplicate
            && duplicate != message.summary.id
        {
            tx.execute("DELETE FROM messages WHERE id = ?1", [duplicate])?;
            tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{REPLY_TO_KEY_PREFIX}{duplicate}")])?;
            tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{UNSUBSCRIBE_KEY_PREFIX}{duplicate}")])?;
            tx.execute("DELETE FROM meta WHERE key = ?1", [&format!("{REMOTE_LABELS_KEY_PREFIX}{duplicate}")])?;
        }
        refresh_folder_counts(&tx, &message.summary.folder_id)?;
        threads::refresh_threads_in_tx(&tx, &message.summary.account_id, touched_threads)?;
        tx.commit()?;
        Ok(adopted_uid)
    }

    pub fn get_draft(&self, id: &str) -> Result<Option<Draft>, StoreError> {
        get_draft(&self.conn, id)
    }

    /// Allocate a readable, store-wide draft id (`draft_<n>`).
    ///
    /// One immediate transaction reads a `meta` high-water mark, seeds it once from the largest id a
    /// build before this allocator already wrote (messages, drafts and the revision table), and
    /// increments it. Two processes therefore never receive the same id; the id is the primary key of
    /// both the message and the outbox `draft_id`, so a collision would overwrite a draft or conflate
    /// two submissions. Existing drafts keep their ids and the sequence continues above them.
    pub fn allocate_draft_id(&self) -> Result<String, StoreError> {
        const KEY: &str = "draft_id_seq";
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", [KEY], |row| row.get(0)).optional()?;
        let stored = stored.and_then(|raw| raw.parse::<i64>().ok()).unwrap_or(0);
        let highest: Option<i64> = tx.query_row(
            "SELECT MAX(n) FROM (
                 SELECT CAST(substr(id, 7) AS INTEGER) AS n FROM messages WHERE id GLOB 'draft_[0-9]*'
                 UNION ALL
                 SELECT CAST(substr(id, 7) AS INTEGER) AS n FROM drafts WHERE id GLOB 'draft_[0-9]*'
                 UNION ALL
                 SELECT CAST(substr(message_id, 7) AS INTEGER) AS n FROM draft_seq WHERE message_id GLOB 'draft_[0-9]*'
             ) WHERE n > 0",
            [],
            |row| row.get(0),
        )?;
        let next = stored.max(highest.unwrap_or(0)).saturating_add(1);
        tx.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![KEY, next.to_string()],
        )?;
        tx.commit()?;
        Ok(format!("draft_{next}"))
    }

    /// Draft metadata only: id, account, revision, update time and reply link. No body, headers or
    /// attachments. `Engine::open` uses this to keep a stale-draft snapshot without reading a draft
    /// body; the body is read only when a draft is opened or serialized.
    pub fn list_draft_metadata(&self) -> Result<Vec<DraftMeta>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id, account_id, revision, updated_at, in_reply_to FROM drafts ORDER BY updated_at DESC, id ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok(DraftMeta { id: row.get(0)?, account_id: row.get(1)?, revision: get_u32(row, 2)?, updated_at: row.get(3)?, in_reply_to: row.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The current revision of one draft, if it still exists.
    pub fn draft_revision(&self, id: &str) -> Result<Option<u32>, StoreError> {
        self.conn.query_row("SELECT revision FROM drafts WHERE id = ?1", [id], |row| get_u32(row, 0)).optional().map_err(StoreError::from)
    }

    /// Drafts for one account (or every draft), newest update first.
    pub fn list_drafts(&self, account_id: Option<&str>) -> Result<Vec<Draft>, StoreError> {
        let rows = match account_id {
            Some(account) => {
                let mut stmt =
                    self.conn.prepare(&format!("SELECT {DRAFT_COLUMNS} FROM drafts WHERE account_id = ?1 ORDER BY updated_at DESC, id ASC"))?;
                let rows = stmt.query_map([account], row_to_draft)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("drafts", error))?
            }
            None => {
                let mut stmt = self.conn.prepare(&format!("SELECT {DRAFT_COLUMNS} FROM drafts ORDER BY updated_at DESC, id ASC"))?;
                let rows = stmt.query_map([], row_to_draft)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("drafts", error))?
            }
        };
        Ok(rows)
    }

    /// Draft rows that have no message row yet. [`Store::repair_orphan_drafts`] turns them into
    /// display rows during engine open, so a crash between the two writes of an old build cannot
    /// leave a draft that is visible but cannot be flagged, labelled or moved.
    pub fn list_orphan_drafts(&self) -> Result<Vec<Draft>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DRAFT_COLUMNS} FROM drafts d
             WHERE NOT EXISTS(SELECT 1 FROM messages m WHERE m.id = d.id)
             ORDER BY d.updated_at DESC, d.id ASC"
        ))?;
        let rows = stmt.query_map([], row_to_draft)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| corrupt_on_decode("drafts", error))
    }

    /// Materialise orphan drafts as message rows in one transaction. Messages that already exist are
    /// left untouched. Returns the number of rows written.
    pub fn repair_orphan_drafts(&self, messages: &[Message]) -> Result<u32, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut repaired = 0usize;
        for message in messages {
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)", [&message.summary.id], |row| row.get(0))?;
            if !exists {
                upsert_message_row(&tx, message, true)?;
                threads::refresh_thread_in_tx(&tx, &message.summary.account_id, &message.summary.thread_id)?;
                repaired += 1;
            }
        }
        tx.commit()?;
        changed_count(repaired)
    }

    /// Delete a draft permanently: the revision row and its sequence go in one transaction. Sending
    /// a draft uses [`Store::send_draft_message`] instead because the sequence must survive.
    /// Returns `false` when the draft was already gone.
    pub fn delete_draft(&self, id: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let removed = tx.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM draft_seq WHERE message_id = ?1", [id])?;
        tx.commit()?;
        Ok(removed == 1)
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Upsert an account row. The caller owns the transaction boundary.
fn upsert_account_row(conn: &rusqlite::Connection, account: &Account) -> Result<(), StoreError> {
    conn.execute(
        &format!(
            "INSERT INTO accounts({ACCOUNT_COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 email = excluded.email,
                 provider = excluded.provider,
                 signature = excluded.signature,
                 created_at = excluded.created_at"
        ),
        params![account.id, account.name, account.email, account.provider, account.signature, account.created_at],
    )?;
    Ok(())
}

/// Upsert one IMAP and SMTP server row. The caller owns the transaction boundary.
fn upsert_account_server_row(conn: &rusqlite::Connection, server: &AccountServer) -> Result<(), StoreError> {
    let smtp = server.smtp.as_ref();
    conn.execute(
        &format!(
            "INSERT INTO account_servers({SERVER_COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(account_id) DO UPDATE SET
                 imap_host = excluded.imap_host,
                 imap_port = excluded.imap_port,
                 imap_tls = excluded.imap_tls,
                 imap_username = excluded.imap_username,
                 created_at = excluded.created_at,
                 smtp_host = excluded.smtp_host,
                 smtp_port = excluded.smtp_port,
                 smtp_tls = excluded.smtp_tls,
                 smtp_username = excluded.smtp_username"
        ),
        params![
            server.account_id,
            server.host,
            i64::from(server.port),
            server.tls.as_str(),
            server.username,
            server.created_at,
            smtp.map(|smtp| smtp.host.as_str()),
            smtp.map(|smtp| i64::from(smtp.port)),
            smtp.map(|smtp| smtp.tls.as_str()),
            smtp.map(|smtp| smtp.username.as_str()),
        ],
    )?;
    Ok(())
}

/// Upsert a folder row. The caller owns the transaction boundary.
fn upsert_folder_row(conn: &rusqlite::Connection, folder: &Folder) -> Result<(), StoreError> {
    conn.execute(
        &format!(
            "INSERT INTO folders({FOLDER_COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 account_id = excluded.account_id,
                 name = excluded.name,
                 kind = excluded.kind,
                 unread = excluded.unread,
                 total = excluded.total,
                 uid_validity = excluded.uid_validity,
                 uid_next = excluded.uid_next,
                 last_sync_at = excluded.last_sync_at"
        ),
        params![
            folder.id,
            folder.account_id,
            folder.name,
            folder.kind.as_str(),
            i64::from(folder.unread),
            i64::from(folder.total),
            folder.uid_validity.map(i64::from),
            folder.uid_next.map(i64::from),
            folder.last_sync_at
        ],
    )?;
    Ok(())
}

/// Upsert a message row inside an open transaction and keep the folder `total`/`unread` cache in
/// step. The caller owns the transaction boundary.
fn upsert_message_row(tx: &rusqlite::Transaction<'_>, message: &Message, refresh_counts: bool) -> Result<(), StoreError> {
    let to_json = serde_json::to_string(&message.summary.to)?;
    let cc_json = serde_json::to_string(&message.cc)?;
    let bcc_json = serde_json::to_string(&message.bcc)?;
    let size = message.size.map(|value| to_i64(value, "message size")).transpose()?;
    let remote_images = i64::from(message.remote_images.min(MAX_REMOTE_IMAGES));
    let folder_row: Option<(String, String)> = tx
        .query_row("SELECT account_id, kind FROM folders WHERE id = ?1", [&message.summary.folder_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((folder_account, folder_kind)) = folder_row else {
        return Err(StoreError::NotFound { entity: "folder", id: message.summary.folder_id.clone() });
    };
    let origin = match folder_kind.as_str() {
        "drafts" => MessageOrigin::Draft,
        "sent" => MessageOrigin::Sent,
        _ => MessageOrigin::Received,
    };
    if folder_account != message.summary.account_id {
        return Err(StoreError::AccountMismatch {
            folder_id: message.summary.folder_id.clone(),
            message_account: message.summary.account_id.clone(),
            folder_account,
        });
    }
    let search_sender = address_text(Some(&message.summary.from));
    let search_to = address_list_text(&message.summary.to);
    let search_cc = address_list_text(&message.cc);
    let search_bcc = address_list_text(&message.bcc);
    let attachment_names = attachment_names_in_tx(tx, &message.summary.id)?.join(" ");
    let subject_norm = normalise_subject(&message.summary.subject);
    let previous: Option<(String, bool)> = tx
        .query_row("SELECT folder_id, unread FROM messages WHERE id = ?1", [&message.summary.id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    tx.execute(
        "INSERT INTO messages(
             id, account_id, folder_id, uid, message_id, thread_id, from_name, from_email,
             to_json, cc_json, bcc_json, subject, snippet, date, unread, starred,
             has_attachments, body_text, body_html, remote_images, size,
             search_sender, search_to, search_cc, search_bcc, attachment_names, subject_norm,
             ingested_at, origin
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
                  ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             folder_id = excluded.folder_id,
             uid = excluded.uid,
             message_id = excluded.message_id,
             thread_id = excluded.thread_id,
             from_name = excluded.from_name,
             from_email = excluded.from_email,
             to_json = excluded.to_json,
             cc_json = excluded.cc_json,
             bcc_json = excluded.bcc_json,
             subject = excluded.subject,
             snippet = excluded.snippet,
             date = excluded.date,
             unread = excluded.unread,
             starred = excluded.starred,
             has_attachments = excluded.has_attachments,
             body_text = excluded.body_text,
             body_html = excluded.body_html,
             remote_images = excluded.remote_images,
             size = excluded.size,
             search_sender = excluded.search_sender,
             search_to = excluded.search_to,
             search_cc = excluded.search_cc,
             search_bcc = excluded.search_bcc,
             attachment_names = excluded.attachment_names,
             subject_norm = excluded.subject_norm",
        params![
            message.summary.id,
            message.summary.account_id,
            message.summary.folder_id,
            message.uid.map(i64::from),
            message.message_id,
            message.summary.thread_id,
            message.summary.from.name,
            message.summary.from.email,
            to_json,
            cc_json,
            bcc_json,
            message.summary.subject,
            message.summary.snippet,
            message.summary.date,
            message.summary.unread,
            message.summary.starred,
            message.summary.has_attachments,
            message.body_text,
            message.body_html,
            remote_images,
            size,
            search_sender,
            search_to,
            search_cc,
            search_bcc,
            attachment_names,
            subject_norm,
            crate::now_ms(),
            origin.as_str(),
        ],
    )?;
    // Counts only move when the folder or the read flag changes; a body/subject refresh during a
    // sync must not rescan the folder for every message.
    let folder_changed = previous.as_ref().is_none_or(|(folder, _)| folder != &message.summary.folder_id);
    let unread_changed = previous.as_ref().is_none_or(|(_, unread)| *unread != message.summary.unread);
    if !refresh_counts {
        return Ok(());
    }
    if folder_changed || unread_changed {
        refresh_folder_counts(tx, &message.summary.folder_id)?;
    }
    if let Some((previous, _)) = previous
        && previous != message.summary.folder_id
    {
        refresh_folder_counts(tx, &previous)?;
    }
    Ok(())
}

/// Replace attachment rows and `has_attachments` inside an open transaction.
///
/// A draft re-save carries no blob hash (the core attachment is metadata only), so the hash **and
/// the size** of an attachment id that survives the replacement are kept: the bytes on disk are
/// canonical, and a stale caller size would push the stored blob above the read cap (`size + slack`)
/// and make it unreadable. Dropping an attachment id drops its pointer; the blob itself stays
/// because another message may reference the same content.
fn replace_attachments_in_tx(tx: &rusqlite::Transaction<'_>, message_id: &str, attachments: &[Attachment]) -> Result<(), StoreError> {
    let mut stored: HashMap<String, (String, i64)> = HashMap::new();
    {
        let mut stmt = tx.prepare("SELECT id, blob_hash, size FROM attachments WHERE message_id = ?1 AND blob_hash IS NOT NULL")?;
        let rows = stmt.query_map([message_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)))?;
        for row in rows {
            let (id, hash, size) = row?;
            stored.insert(id, (hash, size));
        }
    }
    tx.execute("DELETE FROM attachments WHERE message_id = ?1", [message_id])?;
    for attachment in attachments {
        let caller_size = to_i64(attachment.size, "attachment size")?;
        let (blob_hash, size) = match stored.get(&attachment.id) {
            Some((stored_hash, stored_size)) if attachment.blob_hash.as_ref().is_none_or(|incoming| incoming == stored_hash) => {
                (Some(stored_hash.clone()), *stored_size)
            }
            _ => (attachment.blob_hash.clone(), caller_size),
        };
        tx.execute(
            "INSERT INTO attachments(id, message_id, filename, mime, size, blob_hash) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![attachment.id, message_id, attachment.filename, attachment.mime, size, blob_hash],
        )?;
    }
    let names = attachments.iter().map(|attachment| attachment.filename.as_str()).collect::<Vec<_>>().join(" ");
    // Both columns are derived from the rows just written. Naming `attachment_names` here is what
    // re-runs the FTS trigger after a sync or a draft re-save; `has_attachments` alone is not indexed.
    tx.execute("UPDATE messages SET has_attachments = ?1, attachment_names = ?2 WHERE id = ?3", params![!attachments.is_empty(), names, message_id])?;
    Ok(())
}

/// The sender/recipient text the FTS table indexes. Display names and addresses are both useful, so
/// both are kept; the tokenizer does not care about the separator.
fn address_text(address: Option<&Address>) -> String {
    match address {
        Some(address) if address.name.trim().is_empty() => address.email.clone(),
        Some(address) if address.name.trim() == address.email.trim() => address.name.clone(),
        Some(address) => format!("{} {}", address.name, address.email),
        None => String::new(),
    }
}

fn address_list_text(addresses: &[Address]) -> String {
    addresses.iter().map(|address| address_text(Some(address))).collect::<Vec<_>>().join(", ")
}

/// Normalise a subject for P9b's fallback comparison: strip repeated `Re:`/`Fwd:`/`Fw:` prefixes,
/// lowercase, collapse whitespace and bound the length. It is derived data, never accepted from a
/// caller. P9a never read the column; P9b compares it only after every reference is absent.
fn normalise_subject(subject: &str) -> String {
    let mut rest = subject.trim();
    loop {
        let lower = rest.to_lowercase();
        let prefix = if lower.starts_with("re:") {
            3
        } else if lower.starts_with("fwd:") {
            4
        } else if lower.starts_with("fw:") {
            3
        } else {
            break;
        };
        match rest.get(prefix..) {
            Some(after) => rest = after.trim_start(),
            None => break,
        }
    }
    let mut text = String::new();
    for word in rest.split_whitespace() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.extend(word.chars().flat_map(char::to_lowercase));
        if text.chars().count() >= 512 {
            break;
        }
    }
    text.chars().take(512).collect()
}

fn attachment_names_in_tx(tx: &rusqlite::Transaction<'_>, message_id: &str) -> Result<Vec<String>, StoreError> {
    let mut stmt = tx.prepare("SELECT filename FROM attachments WHERE message_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map([message_id], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Validate attachment ids before a draft write: duplicates inside the request and ids owned by a
/// different message are caller errors with a specific reason, not a UNIQUE constraint.
fn validate_attachment_ids(tx: &rusqlite::Transaction<'_>, message_id: &str, attachments: &[Attachment]) -> Result<(), StoreError> {
    let mut seen = BTreeSet::new();
    for attachment in attachments {
        if !seen.insert(attachment.id.as_str()) {
            return Err(StoreError::DuplicateAttachment { id: attachment.id.clone() });
        }
        let owner: Option<String> =
            tx.query_row("SELECT message_id FROM attachments WHERE id = ?1", [&attachment.id], |row| row.get(0)).optional()?;
        if let Some(owner) = owner
            && owner != message_id
        {
            return Err(StoreError::AttachmentOwnedByOther { id: attachment.id.clone(), owner });
        }
    }
    Ok(())
}

fn draft_update_from_message(message: &Message, in_reply_to: Option<&str>) -> DraftUpdate {
    DraftUpdate {
        id: message.summary.id.clone(),
        account_id: message.summary.account_id.clone(),
        updated_at: message.summary.date,
        body_text: message.body_text.clone(),
        to: message.summary.to.clone(),
        cc: message.cc.clone(),
        bcc: message.bcc.clone(),
        subject: message.summary.subject.clone(),
        in_reply_to: in_reply_to.map(str::to_owned),
    }
}

/// Write the revision row and its sequence inside an open transaction.
fn save_draft_row(tx: &rusqlite::Transaction<'_>, draft: &DraftUpdate) -> Result<Draft, StoreError> {
    let to_json = serde_json::to_string(&draft.to)?;
    let cc_json = serde_json::to_string(&draft.cc)?;
    let bcc_json = serde_json::to_string(&draft.bcc)?;
    let revision = next_draft_revision(tx, &draft.id)?;
    tx.execute(
        "INSERT INTO drafts(id, account_id, revision, updated_at, body_text, to_json, cc_json, bcc_json, subject, in_reply_to)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
             account_id = excluded.account_id,
             revision = excluded.revision,
             updated_at = excluded.updated_at,
             body_text = excluded.body_text,
             to_json = excluded.to_json,
             cc_json = excluded.cc_json,
             bcc_json = excluded.bcc_json,
             subject = excluded.subject,
             -- `None` keeps the stored link: a subject-only patch must not clear it. Clearing needs
             -- an explicit field, which does not exist yet.
             in_reply_to = COALESCE(excluded.in_reply_to, drafts.in_reply_to)",
        params![
            draft.id,
            draft.account_id,
            revision,
            draft.updated_at,
            draft.body_text,
            to_json,
            cc_json,
            bcc_json,
            draft.subject,
            draft.in_reply_to
        ],
    )?;
    tx.execute(
        "INSERT INTO draft_seq(message_id, revision) VALUES(?1, ?2)
         ON CONFLICT(message_id) DO UPDATE SET revision = excluded.revision",
        params![draft.id, revision],
    )?;
    get_draft(tx, &draft.id)?.ok_or_else(|| StoreError::NotFound { entity: "draft", id: draft.id.clone() })
}

/// The next revision is one more than the high-water mark in `draft_seq`, or the old `drafts` row
/// for a store that predates v3 and still has to be repaired.
fn next_draft_revision(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<u32, StoreError> {
    let sequenced: Option<u32> = tx.query_row("SELECT revision FROM draft_seq WHERE message_id = ?1", [id], |row| get_u32(row, 0)).optional()?;
    let stored: Option<u32> = tx.query_row("SELECT revision FROM drafts WHERE id = ?1", [id], |row| get_u32(row, 0)).optional()?;
    let previous = match (sequenced, stored) {
        (Some(sequenced), Some(stored)) => sequenced.max(stored),
        (Some(sequenced), None) => sequenced,
        (None, Some(stored)) => stored,
        (None, None) => 0,
    };
    previous.checked_add(1).ok_or(StoreError::OutOfRange { field: "draft revision" })
}

fn message_where(filter: &MessageFilter) -> (String, Vec<Value>) {
    let mut sql = String::from(" WHERE 1 = 1");
    let mut values = Vec::new();
    if let Some(account) = &filter.account_id {
        sql.push_str(" AND m.account_id = ?");
        values.push(Value::Text(account.clone()));
    }
    if let Some(folder) = &filter.folder_id {
        sql.push_str(" AND m.folder_id = ?");
        values.push(Value::Text(folder.clone()));
    }
    if let Some(label) = &filter.label_id {
        sql.push_str(" AND EXISTS(SELECT 1 FROM message_labels ml WHERE ml.message_id = m.id AND ml.label_id = ?)");
        values.push(Value::Text(label.clone()));
    }
    for (flag, column) in [(filter.unread, "m.unread"), (filter.starred, "m.starred"), (filter.has_attachments, "m.has_attachments")] {
        if let Some(flag) = flag {
            sql.push_str(&format!(" AND {column} = ?"));
            values.push(Value::Integer(if flag { 1 } else { 0 }));
        }
    }
    (sql, values)
}

fn refresh_folder_counts(tx: &rusqlite::Transaction<'_>, folder_id: &str) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE folders SET
             total = (SELECT COUNT(*) FROM messages WHERE folder_id = ?1),
             unread = (SELECT COUNT(*) FROM messages WHERE folder_id = ?1 AND unread = 1)
         WHERE id = ?1",
        [folder_id],
    )?;
    Ok(())
}

fn folder_counts(conn: &rusqlite::Connection, folder_id: &str) -> Result<MessageCounts, StoreError> {
    let (total, unread): (i64, i64) =
        conn.query_row("SELECT COUNT(*), COALESCE(SUM(unread), 0) FROM messages WHERE folder_id = ?1", [folder_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    Ok(MessageCounts { total: to_count(total)?, unread: to_count(unread)? })
}

fn require_account(conn: &rusqlite::Connection, id: &str) -> Result<(), StoreError> {
    require(conn, "accounts", "account", id)
}

fn require_folder(conn: &rusqlite::Connection, id: &str) -> Result<(), StoreError> {
    require(conn, "folders", "folder", id)
}

fn require_message(conn: &rusqlite::Connection, id: &str) -> Result<(), StoreError> {
    require(conn, "messages", "message", id)
}

fn require_label(conn: &rusqlite::Connection, id: &str) -> Result<(), StoreError> {
    require(conn, "labels", "label", id)
}

fn require(conn: &rusqlite::Connection, table: &str, entity: &'static str, id: &str) -> Result<(), StoreError> {
    // `table` is always a literal from the callers above, never input.
    let exists: bool = conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id = ?1)"), [id], |row| row.get(0))?;
    if exists { Ok(()) } else { Err(StoreError::NotFound { entity, id: id.to_owned() }) }
}

fn get_label(conn: &rusqlite::Connection, id: &str) -> Result<Option<Label>, StoreError> {
    conn.query_row(&format!("SELECT {LABEL_COLUMNS} FROM labels l WHERE l.id = ?1"), [id], row_to_label).optional().map_err(StoreError::Sqlite)
}

fn get_draft(conn: &rusqlite::Connection, id: &str) -> Result<Option<Draft>, StoreError> {
    conn.query_row(&format!("SELECT {DRAFT_COLUMNS} FROM drafts WHERE id = ?1"), [id], row_to_draft)
        .optional()
        .map_err(|error| corrupt_on_decode("drafts", error))
}

pub(crate) fn duplicate(entity: &'static str, name: &str, error: rusqlite::Error) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::ConstraintViolation => {
            StoreError::Duplicate { entity, name: name.to_owned() }
        }
        _ => StoreError::Sqlite(error),
    }
}

fn to_i64(value: u64, field: &'static str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::OutOfRange { field })
}

pub(crate) fn to_count(value: i64) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::OutOfRange { field: "row count" })
}

fn changed_count(value: usize) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::OutOfRange { field: "changed rows" })
}

pub(crate) fn corrupt_on_decode(table: &'static str, error: rusqlite::Error) -> StoreError {
    match error {
        rusqlite::Error::FromSqlConversionFailure(_, _, source) => StoreError::Corrupt { table, detail: source.to_string() },
        other => StoreError::Sqlite(other),
    }
}

fn decode_error(index: usize, error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
}

fn decode_addresses(row: &Row<'_>, index: usize) -> rusqlite::Result<Vec<Address>> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| decode_error(index, error))
}

pub(crate) fn get_u32(row: &Row<'_>, index: usize) -> rusqlite::Result<u32> {
    let value: i64 = row.get(index)?;
    u32::try_from(value).map_err(|error| rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error)))
}

fn get_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|error| rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error)))
}

fn get_opt_u32(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u32>> {
    match row.get::<_, Option<i64>>(index)? {
        Some(value) => {
            u32::try_from(value).map(Some).map_err(|error| rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error)))
        }
        None => Ok(None),
    }
}

fn get_opt_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    match row.get::<_, Option<i64>>(index)? {
        Some(value) => {
            u64::try_from(value).map(Some).map_err(|error| rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error)))
        }
        None => Ok(None),
    }
}

fn row_to_account(row: &Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account { id: row.get(0)?, name: row.get(1)?, email: row.get(2)?, provider: row.get(3)?, signature: row.get(4)?, created_at: row.get(5)? })
}

fn row_to_outbox(row: &Row<'_>) -> rusqlite::Result<OutboxEntry> {
    let raw: String = row.get(6)?;
    let state = OutboxState::parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(6, Type::Text, Box::new(std::io::Error::other(format!("unknown outbox state {raw:?}"))))
    })?;
    Ok(OutboxEntry {
        id: row.get(0)?,
        account_id: row.get(1)?,
        draft_id: row.get(2)?,
        idempotency_key: row.get(3)?,
        created_at: row.get(4)?,
        send_at: row.get(5)?,
        state,
        attempts: get_u32(row, 7)?,
        append_attempts: get_u32(row, 8)?,
        last_error: row.get(9)?,
        raw_blob_hash: row.get(10)?,
        envelope_json: row.get(11)?,
        lease_at: row.get(12)?,
    })
}

fn parse_state(raw: &str) -> Result<OutboxState, StoreError> {
    OutboxState::parse(raw).ok_or_else(|| StoreError::Corrupt { table: "outbox", detail: format!("unknown state {raw:?}") })
}

fn row_to_account_server(row: &Row<'_>) -> rusqlite::Result<AccountServer> {
    let raw: String = row.get(3)?;
    let tls = AccountTls::parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(std::io::Error::other(format!("unknown IMAP TLS mode {raw:?}"))))
    })?;
    let port = u16::try_from(get_u32(row, 2)?).map_err(|error| rusqlite::Error::FromSqlConversionFailure(2, Type::Integer, Box::new(error)))?;
    let smtp = row_to_smtp_server(row)?;
    Ok(AccountServer { account_id: row.get(0)?, host: row.get(1)?, port, tls, username: row.get(4)?, created_at: row.get(5)?, smtp })
}

fn row_to_smtp_server(row: &Row<'_>) -> rusqlite::Result<Option<crate::model::SmtpServer>> {
    let host: Option<String> = row.get(6)?;
    let port: Option<i64> = row.get(7)?;
    let tls: Option<String> = row.get(8)?;
    let username: Option<String> = row.get(9)?;
    let (host, port, tls, username) = match (host, port, tls, username) {
        (None, None, None, None) => return Ok(None),
        (Some(host), Some(port), Some(tls), Some(username)) => (host, port, tls, username),
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                6,
                Type::Text,
                Box::new(std::io::Error::other("an SMTP endpoint has only some of its four columns")),
            ));
        }
    };
    let port = u16::try_from(port).map_err(|error| rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(error)))?;
    let tls = AccountTls::parse(&tls)
        .ok_or_else(|| rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(std::io::Error::other("unknown SMTP TLS mode"))))?;
    Ok(Some(crate::model::SmtpServer { host, port, tls, username }))
}

fn row_to_folder(row: &Row<'_>) -> rusqlite::Result<Folder> {
    let raw: String = row.get(3)?;
    let kind = FolderKind::parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(std::io::Error::other(format!("unknown folder kind {raw:?}"))))
    })?;
    Ok(Folder {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        kind,
        unread: get_u32(row, 4)?,
        total: get_u32(row, 5)?,
        uid_validity: get_opt_u32(row, 6)?,
        uid_next: get_opt_u32(row, 7)?,
        last_sync_at: row.get(8)?,
    })
}

pub(crate) fn row_to_summary(row: &Row<'_>) -> rusqlite::Result<MessageSummary> {
    row_to_summary_at(row, 0)
}

pub(crate) fn row_to_summary_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        id: row.get(offset)?,
        account_id: row.get(offset + 1)?,
        folder_id: row.get(offset + 2)?,
        thread_id: row.get(offset + 3)?,
        from: Address { name: row.get(offset + 4)?, email: row.get(offset + 5)? },
        to: decode_addresses(row, offset + 6)?,
        subject: row.get(offset + 7)?,
        snippet: row.get(offset + 8)?,
        date: row.get(offset + 9)?,
        unread: row.get(offset + 10)?,
        starred: row.get(offset + 11)?,
        has_attachments: row.get(offset + 12)?,
        labels: Vec::new(),
    })
}

fn row_to_message(row: &Row<'_>) -> rusqlite::Result<Message> {
    let summary = row_to_summary(row)?;
    Ok(Message {
        summary,
        uid: get_opt_u32(row, 13)?,
        message_id: row.get(14)?,
        cc: decode_addresses(row, 15)?,
        bcc: decode_addresses(row, 16)?,
        body_text: row.get(17)?,
        body_html: row.get(18)?,
        remote_images: get_u32(row, 19)?.min(MAX_REMOTE_IMAGES),
        size: get_opt_u64(row, 20)?,
    })
}

fn row_to_label(row: &Row<'_>) -> rusqlite::Result<Label> {
    Ok(Label { id: row.get(0)?, account_id: row.get(1)?, name: row.get(2)?, color: row.get(3)? })
}

fn row_to_label_count(row: &Row<'_>) -> rusqlite::Result<LabelCount> {
    Ok(LabelCount { label: row_to_label(row)?, message_count: get_u32(row, 4)?, unread_count: get_u32(row, 5)? })
}

fn row_to_attachment(row: &Row<'_>) -> rusqlite::Result<Attachment> {
    row_to_attachment_at(row, 0)
}

/// Decode `ATTACHMENT_COLUMNS` starting at `offset`, so a joined query can put a key column first.
fn row_to_attachment_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: row.get(offset)?,
        filename: row.get(offset + 1)?,
        mime: row.get(offset + 2)?,
        size: get_u64(row, offset + 3)?,
        blob_hash: row.get(offset + 4)?,
    })
}

fn row_to_draft(row: &Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: row.get(0)?,
        account_id: row.get(1)?,
        revision: get_u32(row, 2)?,
        updated_at: row.get(3)?,
        body_text: row.get(4)?,
        to: decode_addresses(row, 5)?,
        cc: decode_addresses(row, 6)?,
        bcc: decode_addresses(row, 7)?,
        subject: row.get(8)?,
        in_reply_to: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreKey;

    /// A trigger-injected failure is the only way to reach the write stages behind the attachment
    /// pre-check, and it needs the private connection: hence a unit test here, not in `tests/`.
    fn fixture() -> (tempfile::TempDir, Store) {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("OS random source");
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("store.db"), &StoreKey::from_bytes(key)).expect("open the store");
        store
            .upsert_account(&Account {
                id: "acc_1".to_owned(),
                name: "Studio Verdi".to_owned(),
                email: "studio@example.com".to_owned(),
                provider: "demo".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account");
        store
            .upsert_folder(&Folder {
                id: "drafts".to_owned(),
                account_id: "acc_1".to_owned(),
                name: "Bozze".to_owned(),
                kind: FolderKind::Drafts,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("drafts folder");
        (dir, store)
    }

    fn message() -> Message {
        Message {
            summary: MessageSummary {
                id: "drf_rollback".to_owned(),
                account_id: "acc_1".to_owned(),
                folder_id: "drafts".to_owned(),
                thread_id: "thr_drf_rollback".to_owned(),
                from: Address::new("Studio Verdi", "studio@example.com"),
                to: vec![Address::new("Giulia", "giulia@example.com")],
                subject: "Bozza atomica".to_owned(),
                snippet: "Corpo".to_owned(),
                date: 1_700_000_000_000,
                unread: false,
                starred: false,
                has_attachments: true,
                labels: Vec::new(),
            },
            uid: None,
            message_id: None,
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: "Corpo".to_owned(),
            body_html: None,
            remote_images: 0,
            size: None,
        }
    }

    fn attachment() -> Attachment {
        Attachment {
            id: "att_rollback".to_owned(),
            filename: "preventivo.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            size: 10,
            blob_hash: None,
        }
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |row| row.get(0)).expect("count rows")
    }

    fn folder_totals(store: &Store) -> (u32, u32) {
        let folder = store.get_folder("drafts").expect("folder").expect("drafts folder");
        (folder.total, folder.unread)
    }

    fn assert_nothing_written(store: &Store, when: &str) {
        assert_eq!(count(store, "SELECT COUNT(*) FROM messages WHERE id = 'drf_rollback'"), 0, "the message row survived {when}");
        assert_eq!(count(store, "SELECT COUNT(*) FROM attachments WHERE message_id = 'drf_rollback'"), 0, "the attachment row survived {when}");
        assert!(store.get_draft("drf_rollback").expect("read the draft row").is_none(), "the draft row survived {when}");
        assert_eq!(count(store, "SELECT COUNT(*) FROM draft_seq WHERE message_id = 'drf_rollback'"), 0, "the sequence row survived {when}");
    }

    #[test]
    fn a_temporary_drafts_trigger_rolls_back_every_stage_of_the_save() {
        let (_dir, store) = fixture();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER p3e_abort_draft_insert BEFORE INSERT ON drafts
                 BEGIN
                     SELECT RAISE(ABORT, 'p3e injected draft failure');
                 END;",
            )
            .expect("install the drafts trigger");
        let folder_before = folder_totals(&store);
        let message = message();
        let attachment = attachment();

        let error = store.save_draft_message(&message, std::slice::from_ref(&attachment)).expect_err("the trigger must abort the save");
        assert!(matches!(error, StoreError::Sqlite(_)), "{error}");
        assert_nothing_written(&store, "after the drafts trigger aborted");
        assert_eq!(folder_totals(&store), folder_before, "the folder count cache moved after the rollback");
        assert_eq!(store.counts(Some("drafts")).expect("counts"), MessageCounts::default());

        store.conn.execute_batch("DROP TRIGGER p3e_abort_draft_insert;").expect("drop the trigger");
        let saved = store.save_draft_message(&message, std::slice::from_ref(&attachment)).expect("the same save succeeds without the trigger");
        assert_eq!(saved.revision, 1);
        assert!(store.get_message("drf_rollback").expect("message").is_some());
        assert_eq!(store.attachments("drf_rollback").expect("attachments"), vec![attachment]);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM draft_seq WHERE message_id = 'drf_rollback'"), 1);
        assert_eq!(folder_totals(&store), (1, 0), "the successful save did not refresh the folder cache");
    }

    #[test]
    fn a_failure_after_the_revision_row_rolls_back_the_earlier_stages() {
        let (_dir, store) = fixture();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER p3e_abort_sequence_insert BEFORE INSERT ON draft_seq
                 BEGIN
                     SELECT RAISE(ABORT, 'p3e injected sequence failure');
                 END;",
            )
            .expect("install the sequence trigger");
        let folder_before = folder_totals(&store);
        let message = message();
        let attachment = attachment();

        let error = store.save_draft_message(&message, std::slice::from_ref(&attachment)).expect_err("the trigger must abort the save");
        assert!(matches!(error, StoreError::Sqlite(_)), "{error}");
        assert_nothing_written(&store, "after the sequence trigger aborted");
        assert_eq!(folder_totals(&store), folder_before, "the folder count cache moved after the rollback");

        store.conn.execute_batch("DROP TRIGGER p3e_abort_sequence_insert;").expect("drop the trigger");
        assert_eq!(store.save_draft_message(&message, std::slice::from_ref(&attachment)).expect("save without the trigger").revision, 1);
    }
}
