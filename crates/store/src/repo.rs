//! Repository methods: typed reads and transactional writes over the schema in [`crate`].
//!
//! The API mirrors the M0 operations so P3c is a mapping exercise, not a redesign: accounts,
//! folders, message list/get/flags/move/delete/counts, labels, attachments and drafts. Every batch
//! mutation runs in one transaction and either applies completely or not at all.

use std::collections::{BTreeSet, HashMap};

use rusqlite::types::{Type, Value};
use rusqlite::{ErrorCode, OptionalExtension, Row, params, params_from_iter};

use crate::model::{
    Account, AccountServer, AccountTls, Address, Attachment, Draft, DraftUpdate, FlagUpdate, Folder, FolderKind, FolderMessageFlags, Label,
    LabelCount, Message, MessageCounts, MessageFilter, MessageFlags, MessagePage, MessageSummary, SyncState, UidEntry,
};
use crate::{SYNC_STATUS_KEY_PREFIX, Store, StoreError};

/// Largest page a caller can request. A mailbox list never needs more, and the cap bounds memory.
const MAX_PAGE: u32 = 500;

const ACCOUNT_COLUMNS: &str = "id, name, email, provider, signature, created_at";
const SERVER_COLUMNS: &str = "account_id, imap_host, imap_port, imap_tls, imap_username, created_at";
const FOLDER_COLUMNS: &str = "id, account_id, name, kind, unread, total, uid_validity, uid_next, last_sync_at";
const SUMMARY_COLUMNS: &str = "m.id, m.account_id, m.folder_id, m.thread_id, m.from_name, m.from_email, m.to_json, m.subject, m.snippet, m.date, m.unread, m.starred, m.has_attachments";
const MESSAGE_DETAIL_COLUMNS: &str = "m.uid, m.message_id, m.cc_json, m.bcc_json, m.body_text, m.body_html, m.size";
const LABEL_COLUMNS: &str = "l.id, l.account_id, l.name, l.color";
const DRAFT_COLUMNS: &str = "id, account_id, revision, updated_at, body_text, to_json, cc_json, bcc_json, subject, in_reply_to";
const ATTACHMENT_COLUMNS: &str = "id, filename, mime, size, blob_hash";

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
        tx.execute("DELETE FROM messages WHERE account_id = ?1", [account_id])?;
        tx.execute("DELETE FROM drafts WHERE account_id = ?1", [account_id])?;
        tx.execute("DELETE FROM account_servers WHERE account_id = ?1", [account_id])?;
        tx.execute("DELETE FROM folders WHERE account_id = ?1", [account_id])?;
        let status_key = format!("{SYNC_STATUS_KEY_PREFIX}{account_id}");
        tx.execute("DELETE FROM meta WHERE key = ?1", [status_key])?;
        tx.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
        tx.commit()?;
        Ok(true)
    }
}

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
        let tx = self.conn.unchecked_transaction()?;
        upsert_message_row(&tx, message, true)?;
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

    /// Delete one message permanently. Attachments and label assignments go with it via
    /// `ON DELETE CASCADE`; the draft row and its revision counter are removed explicitly in the
    /// same transaction, so a later open cannot resurrect the draft. Returns `false` when the
    /// message row was already gone (stale draft rows are still cleaned up).
    pub fn delete_message(&self, id: &str) -> Result<bool, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let folder: Option<String> = tx.query_row("SELECT folder_id FROM messages WHERE id = ?1", [id], |row| row.get(0)).optional()?;
        tx.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM draft_seq WHERE message_id = ?1", [id])?;
        tx.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        if let Some(folder) = &folder {
            refresh_folder_counts(&tx, folder)?;
        }
        tx.commit()?;
        Ok(folder.is_some())
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
        upsert_message_row(&tx, message, false)?;
        replace_attachments_in_tx(&tx, &message.summary.id, attachments)?;
        tx.commit()?;
        Ok(())
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
        let mut removed = 0usize;
        for id in message_ids {
            let is_draft: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM drafts WHERE id = ?1)", [id], |row| row.get(0))?;
            if is_draft {
                continue;
            }
            removed += tx.execute("DELETE FROM messages WHERE id = ?1 AND folder_id = ?2", params![id, folder_id])?;
        }
        if removed > 0 {
            refresh_folder_counts(&tx, folder_id)?;
        }
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
        let removed =
            tx.execute("DELETE FROM messages WHERE folder_id = ?1 AND uid IS NOT NULL AND id NOT IN (SELECT id FROM drafts)", [folder_id])?;
        tx.execute("DELETE FROM sync_state WHERE folder_id = ?1", [folder_id])?;
        refresh_folder_counts(&tx, folder_id)?;
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
        let tx = self.conn.unchecked_transaction()?;
        validate_attachment_ids(&tx, &message.summary.id, attachments)?;
        upsert_message_row(&tx, message, true)?;
        replace_attachments_in_tx(&tx, &message.summary.id, attachments)?;
        let draft = draft_update_from_message(message);
        let saved = save_draft_row(&tx, &draft)?;
        refresh_folder_counts(&tx, &message.summary.folder_id)?;
        tx.commit()?;
        Ok(saved)
    }

    /// Move a draft to Sent in one transaction. The draft row is deleted because the message is no
    /// longer a draft, but `draft_seq` is kept so a move back to Drafts and a new save continue the
    /// revision sequence instead of restarting at 1.
    pub fn send_draft_message(&self, message: &Message) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        upsert_message_row(&tx, message, true)?;
        tx.execute("DELETE FROM drafts WHERE id = ?1", [&message.summary.id])?;
        refresh_folder_counts(&tx, &message.summary.folder_id)?;
        tx.commit()?;
        Ok(())
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

/// Upsert one IMAP server row. The caller owns the transaction boundary.
fn upsert_account_server_row(conn: &rusqlite::Connection, server: &AccountServer) -> Result<(), StoreError> {
    conn.execute(
        &format!(
            "INSERT INTO account_servers({SERVER_COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account_id) DO UPDATE SET
                 imap_host = excluded.imap_host,
                 imap_port = excluded.imap_port,
                 imap_tls = excluded.imap_tls,
                 imap_username = excluded.imap_username,
                 created_at = excluded.created_at"
        ),
        params![server.account_id, server.host, i64::from(server.port), server.tls.as_str(), server.username, server.created_at],
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
    tx.execute("UPDATE messages SET has_attachments = ?1 WHERE id = ?2", params![!attachments.is_empty(), message_id])?;
    Ok(())
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

fn draft_update_from_message(message: &Message) -> DraftUpdate {
    DraftUpdate {
        id: message.summary.id.clone(),
        account_id: message.summary.account_id.clone(),
        updated_at: message.summary.date,
        body_text: message.body_text.clone(),
        to: message.summary.to.clone(),
        cc: message.cc.clone(),
        bcc: message.bcc.clone(),
        subject: message.summary.subject.clone(),
        // Core has no reply linkage yet; M2 adds it with the compose work.
        in_reply_to: None,
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

fn row_to_account_server(row: &Row<'_>) -> rusqlite::Result<AccountServer> {
    let raw: String = row.get(3)?;
    let tls = AccountTls::parse(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(std::io::Error::other(format!("unknown IMAP TLS mode {raw:?}"))))
    })?;
    let port = u16::try_from(get_u32(row, 2)?).map_err(|error| rusqlite::Error::FromSqlConversionFailure(2, Type::Integer, Box::new(error)))?;
    Ok(AccountServer { account_id: row.get(0)?, host: row.get(1)?, port, tls, username: row.get(4)?, created_at: row.get(5)? })
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
