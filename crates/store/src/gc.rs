//! Bounded, resumable blob garbage collection.
//!
//! A blob is referenced when an attachment row, an outbox row or a received PGP/MIME message names
//! its hash. Drafts are message rows with attachment rows, so a draft attachment is covered; a
//! P7/P9c outbox snapshot is covered by `outbox.raw_blob_hash`; the exact bytes sync kept for
//! `multipart/signed` and `multipart/encrypted` mail are covered by `messages.raw_blob_hash`. A
//! sweep never trusts a cached list: it rechecks each candidate before deletion, because a forward, a
//! new attachment or a re-synced raw entity can be written between the scan and the unlink.
//!
//! The *deletion* is bounded by `limit`; the *scan* visits every shard directory. That is the
//! resumable shape: a call that deletes nothing this time will see the remaining files next time.
//! The daemon runs one bounded batch after a successful sync and on its idle timer, never in a loop.

use std::collections::HashSet;

use rusqlite::params;

use crate::{Store, StoreError};

/// What one bounded sweep did. `kept` is the number of unreferenced blobs left for a later batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlobGcReport {
    pub scanned: u32,
    pub removed: u32,
    pub referenced: u32,
    pub kept: u32,
}

impl Store {
    /// Delete at most `limit` unreferenced blobs. A read-only connection refuses the sweep instead
    /// of half-performing it.
    pub fn gc_unreferenced_blobs(&self, limit: usize) -> Result<BlobGcReport, StoreError> {
        if self.read_only {
            return Err(StoreError::ReadOnly);
        }
        let limit = limit.min(4096);
        let referenced = self.referenced_blob_hashes()?;
        let hashes = self.blobs().list_hashes()?;
        let mut report = BlobGcReport::default();
        for hash in hashes {
            report.scanned = report.scanned.saturating_add(1);
            if referenced.contains(&hash) || self.blob_is_referenced(&hash)? {
                report.referenced = report.referenced.saturating_add(1);
                continue;
            }
            if usize::try_from(report.removed).unwrap_or(usize::MAX) >= limit {
                report.kept = report.kept.saturating_add(1);
                continue;
            }
            if self.blobs().remove(&hash)? {
                report.removed = report.removed.saturating_add(1);
            } else {
                report.kept = report.kept.saturating_add(1);
            }
        }
        Ok(report)
    }

    fn referenced_blob_hashes(&self) -> Result<HashSet<String>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT blob_hash FROM attachments WHERE blob_hash IS NOT NULL AND blob_hash <> ''
             UNION
             SELECT raw_blob_hash FROM outbox WHERE raw_blob_hash IS NOT NULL AND raw_blob_hash <> ''
             UNION
             SELECT raw_blob_hash FROM messages WHERE raw_blob_hash IS NOT NULL AND raw_blob_hash <> ''",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut hashes = HashSet::new();
        for row in rows {
            hashes.insert(row?);
        }
        Ok(hashes)
    }

    fn blob_is_referenced(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM attachments WHERE blob_hash = ?1
                 UNION ALL
                 SELECT 1 FROM outbox WHERE raw_blob_hash = ?1
                 UNION ALL
                 SELECT 1 FROM messages WHERE raw_blob_hash = ?1
             )",
            params![hash],
            |row| row.get::<_, bool>(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, Address, Attachment, Folder, FolderKind, Message, MessageSummary, OutboxEntry, OutboxState};
    use crate::{StoreKey, blob_root};

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mail.db");
        let store = Store::open(&path, &StoreKey::from_bytes([9u8; 32])).expect("store");
        store
            .upsert_account(&Account {
                id: "acc".to_owned(),
                name: "Test".to_owned(),
                email: "test@example.com".to_owned(),
                provider: "custom".to_owned(),
                signature: String::new(),
                created_at: 1,
            })
            .expect("account");
        store
            .upsert_folder(&Folder {
                id: "inbox".to_owned(),
                account_id: "acc".to_owned(),
                name: "Inbox".to_owned(),
                kind: FolderKind::Inbox,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: None,
                last_sync_at: None,
            })
            .expect("folder");
        (directory, store)
    }

    fn message() -> Message {
        Message {
            summary: MessageSummary {
                id: "msg".to_owned(),
                account_id: "acc".to_owned(),
                folder_id: "inbox".to_owned(),
                thread_id: "thr".to_owned(),
                from: Address::new("A", "a@example.com"),
                to: vec![Address::new("B", "b@example.com")],
                subject: "Prova".to_owned(),
                snippet: String::new(),
                date: 1,
                unread: false,
                starred: false,
                has_attachments: true,
                labels: Vec::new(),
            },
            uid: Some(1),
            message_id: Some("<m@example.com>".to_owned()),
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: "body".to_owned(),
            body_html: None,
            remote_images: 0,
            size: Some(4),
        }
    }

    #[test]
    fn gc_removes_only_unreferenced_blobs_and_keeps_outbox_snapshots() {
        let (_directory, store) = store();
        let attachment_hash = store.blobs().put(b"attachment bytes").expect("attachment blob");
        let outbox_hash = store.blobs().put(b"serialized message").expect("outbox blob");
        let orphan_hash = store.blobs().put(b"orphan bytes").expect("orphan blob");
        let attachment = Attachment {
            id: "att".to_owned(),
            filename: "a.txt".to_owned(),
            mime: "text/plain".to_owned(),
            size: 16,
            blob_hash: Some(attachment_hash.clone()),
        };
        store.upsert_synced_message(&message(), std::slice::from_ref(&attachment)).expect("message");
        store
            .queue_outbox(&OutboxEntry {
                id: "out".to_owned(),
                account_id: "acc".to_owned(),
                draft_id: None,
                idempotency_key: "key".to_owned(),
                created_at: 1,
                send_at: 1,
                state: OutboxState::Queued,
                attempts: 0,
                append_attempts: 0,
                last_error: None,
                raw_blob_hash: outbox_hash.clone(),
                envelope_json: r#"{"from":"a@example.com","recipients":["b@example.com"],"subject":"x","message_id":"<i@example.com>"}"#.to_owned(),
                lease_at: None,
            })
            .expect("outbox");
        let report = store.gc_unreferenced_blobs(16).expect("gc");
        assert_eq!(report.removed, 1);
        assert_eq!(report.kept, 0);
        assert!(store.blobs().has(&attachment_hash).expect("attachment"));
        assert!(store.blobs().has(&outbox_hash).expect("outbox"));
        assert!(!store.blobs().has(&orphan_hash).expect("orphan"));
        assert!(!blob_root(&_directory.path().join("mail.db")).join(&orphan_hash[..2]).join(format!("{orphan_hash}.blob")).exists());
    }

    #[test]
    fn gc_keeps_the_raw_blob_of_a_pgp_mime_message() {
        let (_directory, store) = store();
        store.upsert_synced_message(&message(), &[]).expect("message");
        let raw_hash = store.blobs().put(b"multipart/signed exact bytes").expect("raw blob");
        store.set_message_raw_blob_hash("msg", Some(&raw_hash)).expect("raw hash");
        let report = store.gc_unreferenced_blobs(16).expect("gc");
        assert_eq!(report.removed, 0, "the sweep deleted the raw entity of a cached message");
        assert_eq!(report.referenced, 1);
        assert_eq!(store.message_raw_blob_hash("msg").expect("read raw"), Some(raw_hash.clone()));
        assert!(store.blobs().has(&raw_hash).expect("raw"));

        // Clearing the column (ordinary mail, or a row that is no longer PGP/MIME) releases it.
        store.set_message_raw_blob_hash("msg", None).expect("clear");
        assert_eq!(store.message_raw_blob_hash("msg").expect("read cleared"), None);
        let report = store.gc_unreferenced_blobs(16).expect("gc after clear");
        assert_eq!(report.removed, 1);
        assert!(!store.blobs().has(&raw_hash).expect("raw removed"));
    }

    #[test]
    fn gc_is_bounded_and_resumable() {
        let (_directory, store) = store();
        let first = store.blobs().put(b"one").expect("one");
        let second = store.blobs().put(b"two").expect("two");
        let report = store.gc_unreferenced_blobs(1).expect("first batch");
        assert_eq!(report.removed, 1);
        assert_eq!(report.kept, 1);
        let report = store.gc_unreferenced_blobs(1).expect("second batch");
        assert_eq!(report.removed, 1);
        assert!(!store.blobs().has(&first).expect("first"));
        assert!(!store.blobs().has(&second).expect("second"));
    }
}
