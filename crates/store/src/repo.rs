//! Repository methods: typed reads and transactional writes over the schema in [`crate`].
//!
//! The API mirrors the M0 operations so P3c is a mapping exercise, not a redesign: accounts,
//! folders, message list/get/flags/move/delete/counts, labels, attachments and drafts. Every batch
//! mutation runs in one transaction and either applies completely or not at all.

use std::collections::{BTreeSet, HashMap};

use rusqlite::types::{Type, Value};
use rusqlite::{ErrorCode, OptionalExtension, Row, params, params_from_iter};

use crate::model::{
    Account, Address, Attachment, Draft, DraftUpdate, FlagUpdate, Folder, FolderKind, Label, LabelCount, Message, MessageCounts, MessageFilter,
    MessagePage, MessageSummary,
};
use crate::{Store, StoreError};

/// Largest page a caller can request. A mailbox list never needs more, and the cap bounds memory.
const MAX_PAGE: u32 = 500;

const ACCOUNT_COLUMNS: &str = "id, name, email, provider, signature, created_at";
const FOLDER_COLUMNS: &str = "id, account_id, name, kind, unread, total, uid_validity, uid_next, last_sync_at";
const SUMMARY_COLUMNS: &str = "m.id, m.account_id, m.folder_id, m.thread_id, m.from_name, m.from_email, m.to_json, m.subject, m.snippet, m.date, m.unread, m.starred, m.has_attachments";
const MESSAGE_DETAIL_COLUMNS: &str = "m.uid, m.message_id, m.cc_json, m.bcc_json, m.body_text, m.body_html, m.size";
const LABEL_COLUMNS: &str = "l.id, l.account_id, l.name, l.color";
const DRAFT_COLUMNS: &str = "id, account_id, revision, updated_at, body_text, to_json, cc_json, bcc_json, subject, in_reply_to";
const ATTACHMENT_COLUMNS: &str = "id, filename, mime, size, blob_hash";

// ---------------------------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Insert or replace an account. The row is authoritative: every column is overwritten.
    pub fn upsert_account(&self, account: &Account) -> Result<(), StoreError> {
        self.conn.execute(
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

    pub fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let mut stmt = self.conn.prepare(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY name COLLATE NOCASE, id"))?;
        let rows = stmt.query_map([], row_to_account)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_account(&self, id: &str) -> Result<Option<Account>, StoreError> {
        Ok(self.conn.query_row(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = ?1"), [id], row_to_account).optional()?)
    }
}

// ---------------------------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Insert or replace a folder. `unread` and `total` are a cache: message mutations refresh them
    /// and [`Store::recount_folder`] repairs them after a bulk import.
    pub fn upsert_folder(&self, folder: &Folder) -> Result<(), StoreError> {
        self.conn.execute(
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

    pub fn get_folder(&self, id: &str) -> Result<Option<Folder>, StoreError> {
        self.conn
            .query_row(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE id = ?1"), [id], row_to_folder)
            .optional()
            .map_err(|error| corrupt_on_decode("folders", error))
    }

    /// Recompute and store a folder's `unread`/`total` from `messages`, then return them.
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
        let to_json = serde_json::to_string(&message.summary.to)?;
        let cc_json = serde_json::to_string(&message.cc)?;
        let bcc_json = serde_json::to_string(&message.bcc)?;
        let size = message.size.map(|value| to_i64(value, "message size")).transpose()?;
        let tx = self.conn.unchecked_transaction()?;
        let folder_account: Option<String> =
            tx.query_row("SELECT account_id FROM folders WHERE id = ?1", [&message.summary.folder_id], |row| row.get(0)).optional()?;
        let Some(folder_account) = folder_account else {
            return Err(StoreError::NotFound { entity: "folder", id: message.summary.folder_id.clone() });
        };
        if folder_account != message.summary.account_id {
            return Err(StoreError::AccountMismatch {
                folder_id: message.summary.folder_id.clone(),
                message_account: message.summary.account_id.clone(),
                folder_account,
            });
        }
        let previous: Option<(String, bool)> = tx
            .query_row("SELECT folder_id, unread FROM messages WHERE id = ?1", [&message.summary.id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        tx.execute(
            "INSERT INTO messages(
                 id, account_id, folder_id, uid, message_id, thread_id, from_name, from_email,
                 to_json, cc_json, bcc_json, subject, snippet, date, unread, starred,
                 has_attachments, body_text, body_html, size
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
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
                 size = excluded.size",
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
                size
            ],
        )?;
        // Counts only move when the folder or the read flag changes; a body/subject refresh during a
        // sync must not rescan the folder for every message.
        let folder_changed = previous.as_ref().is_none_or(|(folder, _)| folder != &message.summary.folder_id);
        let unread_changed = previous.as_ref().is_none_or(|(_, unread)| *unread != message.summary.unread);
        if folder_changed || unread_changed {
            refresh_folder_counts(&tx, &message.summary.folder_id)?;
        }
        if let Some((previous, _)) = previous
            && previous != message.summary.folder_id
        {
            refresh_folder_counts(&tx, &previous)?;
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
        let mut changed = 0usize;
        for id in ids {
            let folder: Option<String> = tx.query_row("SELECT folder_id FROM messages WHERE id = ?1", [id], |row| row.get(0)).optional()?;
            let Some(folder) = folder else {
                return Err(StoreError::NotFound { entity: "message", id: id.clone() });
            };
            tx.execute(
                "UPDATE messages SET unread = COALESCE(?1, unread), starred = COALESCE(?2, starred) WHERE id = ?3",
                params![update.unread, update.starred, id],
            )?;
            if update.unread.is_some() {
                touched.insert(folder);
            }
            changed += 1;
        }
        for folder in touched {
            refresh_folder_counts(&tx, &folder)?;
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

    /// Delete one message. Attachments and label assignments go with it via `ON DELETE CASCADE`.
    /// Returns `false` when the id was already gone.
    pub fn delete_message(&self, id: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let folder: Option<String> = tx.query_row("SELECT folder_id FROM messages WHERE id = ?1", [id], |row| row.get(0)).optional()?;
        let Some(folder) = folder else { return Ok(false) };
        tx.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        refresh_folder_counts(&tx, &folder)?;
        tx.commit()?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Create a label. `id` is caller-owned and stable; a duplicate name (case-sensitive, in the
    /// same account scope) is [`StoreError::Duplicate`].
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

    fn labels_by_message(&self, message_ids: &[String]) -> Result<HashMap<String, Vec<String>>, StoreError> {
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
        tx.execute("DELETE FROM attachments WHERE message_id = ?1", [message_id])?;
        for attachment in attachments {
            let size = to_i64(attachment.size, "attachment size")?;
            tx.execute(
                "INSERT INTO attachments(id, message_id, filename, mime, size, blob_hash) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![attachment.id, message_id, attachment.filename, attachment.mime, size, attachment.blob_hash],
            )?;
        }
        tx.execute("UPDATE messages SET has_attachments = ?1 WHERE id = ?2", params![!attachments.is_empty(), message_id])?;
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
}

// ---------------------------------------------------------------------------------------------
// Drafts
// ---------------------------------------------------------------------------------------------

impl Store {
    /// Save a draft. A new id starts at revision 1; an existing id increments `revision` by one and
    /// takes every editable field. The stored row is returned so the caller sees the new revision.
    pub fn save_draft(&self, draft: &DraftUpdate) -> Result<Draft, StoreError> {
        let to_json = serde_json::to_string(&draft.to)?;
        let cc_json = serde_json::to_string(&draft.cc)?;
        let bcc_json = serde_json::to_string(&draft.bcc)?;
        let tx = self.conn.unchecked_transaction()?;
        require_account(&tx, &draft.account_id)?;
        tx.execute(
            "INSERT INTO drafts(id, account_id, revision, updated_at, body_text, to_json, cc_json, bcc_json, subject, in_reply_to)
             VALUES(?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 account_id = excluded.account_id,
                 revision = drafts.revision + 1,
                 updated_at = excluded.updated_at,
                 body_text = excluded.body_text,
                 to_json = excluded.to_json,
                 cc_json = excluded.cc_json,
                 bcc_json = excluded.bcc_json,
                 subject = excluded.subject,
                 in_reply_to = excluded.in_reply_to",
            params![draft.id, draft.account_id, draft.updated_at, draft.body_text, to_json, cc_json, bcc_json, draft.subject, draft.in_reply_to],
        )?;
        let saved = get_draft(&tx, &draft.id)?.ok_or_else(|| StoreError::NotFound { entity: "draft", id: draft.id.clone() })?;
        tx.commit()?;
        Ok(saved)
    }

    pub fn get_draft(&self, id: &str) -> Result<Option<Draft>, StoreError> {
        get_draft(&self.conn, id)
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

    /// Delete a draft. Returns `false` when it was already gone.
    pub fn delete_draft(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute("DELETE FROM drafts WHERE id = ?1", [id])? == 1)
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

fn message_where(filter: &MessageFilter) -> (String, Vec<Value>) {
    let mut sql = String::from(" WHERE 1 = 1");
    let mut values = Vec::new();
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

fn duplicate(entity: &'static str, name: &str, error: rusqlite::Error) -> StoreError {
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

fn to_count(value: i64) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::OutOfRange { field: "row count" })
}

fn changed_count(value: usize) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::OutOfRange { field: "changed rows" })
}

fn corrupt_on_decode(table: &'static str, error: rusqlite::Error) -> StoreError {
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

fn get_u32(row: &Row<'_>, index: usize) -> rusqlite::Result<u32> {
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

fn row_to_summary(row: &Row<'_>) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        id: row.get(0)?,
        account_id: row.get(1)?,
        folder_id: row.get(2)?,
        thread_id: row.get(3)?,
        from: Address { name: row.get(4)?, email: row.get(5)? },
        to: decode_addresses(row, 6)?,
        subject: row.get(7)?,
        snippet: row.get(8)?,
        date: row.get(9)?,
        unread: row.get(10)?,
        starred: row.get(11)?,
        has_attachments: row.get(12)?,
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
        size: get_opt_u64(row, 19)?,
    })
}

fn row_to_label(row: &Row<'_>) -> rusqlite::Result<Label> {
    Ok(Label { id: row.get(0)?, account_id: row.get(1)?, name: row.get(2)?, color: row.get(3)? })
}

fn row_to_label_count(row: &Row<'_>) -> rusqlite::Result<LabelCount> {
    Ok(LabelCount { label: row_to_label(row)?, message_count: get_u32(row, 4)?, unread_count: get_u32(row, 5)? })
}

fn row_to_attachment(row: &Row<'_>) -> rusqlite::Result<Attachment> {
    Ok(Attachment { id: row.get(0)?, filename: row.get(1)?, mime: row.get(2)?, size: get_u64(row, 3)?, blob_hash: row.get(4)? })
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
