//! P4b: attachment bytes through the engine seam.
//!
//! The engine owns the store and the blob directory next to it. These tests drive the seam the way
//! P5b (sync) and P6 (reading pane) will: store bytes for one attachment, drop the engine, reopen,
//! read them back. Fixtures are synthetic and use `example.com` / `example.it`.

use std::path::{Path, PathBuf};

use postino_core::{Engine, OpError};
use postino_store::{FolderKind, Store, StoreKey};
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;

struct Fixture {
    dir: tempfile::TempDir,
    key: StoreKey,
}

impl Fixture {
    fn new() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS random source");
        Self { dir: tempfile::tempdir().expect("temp dir"), key: StoreKey::from_bytes(bytes) }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("mail.db")
    }

    fn blob_root(&self) -> PathBuf {
        self.dir.path().join("blobs")
    }

    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("open the store")
    }

    /// One account, an inbox and one message with two attachments.
    fn seed(&self) {
        let store = self.open();
        store
            .upsert_account(&postino_store::Account {
                id: "acc_1".to_owned(),
                name: "Studio Verdi".to_owned(),
                email: "studio@example.com".to_owned(),
                provider: "demo".to_owned(),
                signature: String::new(),
                created_at: NOW,
            })
            .expect("account");
        store
            .upsert_folder(&postino_store::Folder {
                id: "inbox".to_owned(),
                account_id: "acc_1".to_owned(),
                name: "Posta in arrivo".to_owned(),
                kind: FolderKind::Inbox,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("folder");
        store
            .upsert_folder(&postino_store::Folder {
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
        store
            .upsert_message(&postino_store::Message {
                summary: postino_store::MessageSummary {
                    id: "msg_1".to_owned(),
                    account_id: "acc_1".to_owned(),
                    folder_id: "inbox".to_owned(),
                    thread_id: "thr_1".to_owned(),
                    from: postino_store::Address::new("Giulia Ferri", "giulia.ferri@example.com"),
                    to: vec![postino_store::Address::new("Federico", "federico@example.it")],
                    subject: "Preventivo e allegati".to_owned(),
                    snippet: "Il preventivo".to_owned(),
                    date: NOW,
                    unread: true,
                    starred: false,
                    has_attachments: true,
                    labels: Vec::new(),
                },
                uid: None,
                message_id: None,
                cc: Vec::new(),
                bcc: Vec::new(),
                body_text: "Corpo del messaggio".to_owned(),
                body_html: None,
                size: None,
            })
            .expect("message");
        store
            .replace_attachments(
                "msg_1",
                &[
                    postino_store::Attachment {
                        id: "att_1".to_owned(),
                        filename: "preventivo.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 24,
                        blob_hash: None,
                    },
                    postino_store::Attachment {
                        id: "att_2".to_owned(),
                        filename: "foto.png".to_owned(),
                        mime: "image/png".to_owned(),
                        size: 16,
                        blob_hash: None,
                    },
                ],
            )
            .expect("attachments");
    }
}

fn blob_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(blob_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "blob") {
            files.push(path);
        }
    }
    files
}

fn pseudorandom(len: usize) -> Vec<u8> {
    // A deterministic xorshift keeps the 4 MiB smoke reproducible without shipping a fixture file.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

#[test]
fn attachment_bytes_survive_a_reopen() {
    let fixture = Fixture::new();
    fixture.seed();
    let content = b"il contenuto vero dell'allegato";

    let hash = {
        let mut engine = Engine::open(fixture.open()).expect("engine");
        assert!(engine.message("msg_1").expect("msg_1").summary.has_attachments, "the fixture lost its attachments");
        engine.store_attachment_bytes("msg_1", "att_1", content).expect("store the bytes");
        assert_eq!(engine.attachment_bytes("att_1").expect("read the bytes"), content);
        let hash = engine.attachment_hash("att_1").expect("the engine knows the hash").to_owned();
        assert_eq!(hash.len(), 64);
        let attachment = &engine.message("msg_1").expect("msg_1").attachments[0];
        assert_eq!(attachment.size, content.len() as u64, "the in-memory size did not follow the stored bytes");

        let row = fixture.open().find_attachment("att_1").expect("find").expect("row");
        assert_eq!(row.1.blob_hash.as_deref(), Some(hash.as_str()), "the hash did not reach the repository");
        assert_eq!(row.1.size, content.len() as u64);
        hash
    };

    // Everything the engine knew before is gone; this is a fresh process reading the same files.
    let reopened = Engine::open(fixture.open()).expect("reopen");
    assert_eq!(reopened.attachment_bytes("att_1").expect("read after reopen"), content);
    assert_eq!(reopened.attachment_hash("att_1"), Some(hash.as_str()), "the hash was not hydrated from the attachment row");
    let message = reopened.message("msg_1").expect("msg_1");
    assert!(message.summary.has_attachments, "has_attachments is false after reopen");
    assert_eq!(message.attachments[0].size, content.len() as u64);
    assert_eq!(reopened.attachment_bytes("att_1").expect("second read"), content);
    assert!(blob_files(&fixture.blob_root()).len() == 1, "one content must leave exactly one blob file");
}

#[test]
fn a_message_without_stored_bytes_is_a_readable_error() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");

    let error = engine.attachment_bytes("att_2").expect_err("att_2 has no bytes");
    assert!(matches!(&error, OpError::Failed(detail) if detail.contains("no stored bytes")), "{error}");

    let error = engine.attachment_bytes("att_missing").expect_err("unknown attachment");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("unknown attachment")), "{error}");

    let error = engine.store_attachment_bytes("msg_missing", "att_1", b"x").expect_err("unknown message");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("unknown message")), "{error}");

    let error = engine.store_attachment_bytes("msg_1", "att_missing", b"x").expect_err("unknown attachment on that message");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("is not on message")), "{error}");

    // A failed attempt changed nothing.
    assert!(engine.message("msg_1").expect("msg_1").summary.has_attachments);
    assert_eq!(engine.attachment_hash("att_1"), None);
    assert!(blob_files(&fixture.blob_root()).is_empty(), "a rejected write still wrote a blob");
}

#[test]
fn the_demo_engine_reports_that_it_has_no_attachment_bytes() {
    let mut engine = Engine::demo();
    let error = engine.attachment_bytes("att_1").expect_err("the demo has no bytes");
    assert!(matches!(&error, OpError::Failed(detail) if detail.contains("demo")), "{error}");
    let error = engine.store_attachment_bytes("msg_001", "att_1", b"x").expect_err("the demo cannot store bytes");
    assert!(matches!(&error, OpError::Failed(detail) if detail.contains("demo")), "{error}");
    assert_eq!(engine.attachment_hash("att_1"), None);
}

#[test]
fn a_corrupted_blob_surfaces_as_a_readable_failure_and_does_not_poison_the_engine() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");
    engine.store_attachment_bytes("msg_1", "att_1", b"bytes che verranno corrotti").expect("store");

    let files = blob_files(&fixture.blob_root());
    assert_eq!(files.len(), 1, "expected exactly one blob");
    let path = files.first().expect("the blob file");
    let mut bytes = std::fs::read(path).expect("read the blob");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(path, &bytes).expect("flip one byte of the tag");

    let error = engine.attachment_bytes("att_1").expect_err("a corrupt blob must not decrypt");
    assert!(matches!(&error, OpError::Failed(detail) if detail.contains("could not be authenticated") && detail.contains("untouched")), "{error}");

    // The message is not poisoned: it still reads, keeps its metadata, and accepts writes.
    let message = engine.call("message_get", json!({ "id": "msg_1" })).expect("the message is still readable");
    assert_eq!(message["message"]["summary"]["has_attachments"], json!(true));
    let flagged = engine.set_flags("msg_1", Some(false), None).expect("a flag write still works");
    assert!(!flagged.unread);
    assert_eq!(engine.attachment_hash("att_1").map(str::len), Some(64), "the metadata should still point at the hash");

    // Recovery is explicit: a damaged file has to be removed before the same content can be stored
    // again, because `put` deduplicates on the file's existence.
    let hash = engine.attachment_hash("att_1").expect("hash").to_owned();
    fixture.open().blobs().remove(&hash).expect("remove the damaged blob");
    engine.store_attachment_bytes("msg_1", "att_1", b"bytes che verranno corrotti").expect("store again");
    assert_eq!(engine.attachment_bytes("att_1").expect("read the repaired blob"), b"bytes che verranno corrotti");
}

#[test]
fn a_four_mib_attachment_goes_through_the_seam_once() {
    let fixture = Fixture::new();
    fixture.seed();
    let content = pseudorandom(4 * 1024 * 1024);
    let mut engine = Engine::open(fixture.open()).expect("engine");

    // The recorded size has to cover the bytes: the read path caps at size + 1 MiB.
    engine.store_attachment_bytes("msg_1", "att_2", &content).expect("store 4 MiB");
    let read = engine.attachment_bytes("att_2").expect("read 4 MiB");
    assert_eq!(read.len(), content.len());
    assert_eq!(read, content);
    assert_eq!(blob_files(&fixture.blob_root()).len(), 1);
}

#[test]
fn a_draft_re_save_keeps_the_stored_bytes_of_a_kept_attachment() {
    use postino_core::{Address, DraftContent};

    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");
    // The fixture message is not a draft; make a real draft through the operation path.
    let created = engine
        .save_draft(
            None,
            None,
            DraftContent {
                to: vec![Address::new("Giulia Ferri", "giulia.ferri@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza con allegato".to_owned(),
                body_text: "Corpo".to_owned(),
                attachments: vec![postino_core::Attachment {
                    id: String::new(),
                    filename: "contratto.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 0,
                }],
            },
        )
        .expect("draft");
    let draft_id = created.summary.id.clone();
    let attachment_id = created.attachments.first().expect("attachment").id.clone();
    engine.store_attachment_bytes(&draft_id, &attachment_id, b"contratto firmato").expect("store the draft attachment");

    // A subject-only patch keeps the attachment list (and therefore the stored bytes).
    let patched = engine.call("draft_save", json!({ "id": draft_id.clone(), "subject": "Solo l'oggetto" })).expect("patch");
    assert_eq!(patched["draft"]["summary"]["subject"], "Solo l'oggetto");
    assert_eq!(engine.attachment_bytes(&attachment_id).expect("the bytes survived the patch"), b"contratto firmato");
    assert_eq!(engine.attachment_hash(&attachment_id).map(str::len), Some(64));

    // Dropping the attachment drops the pointer and the in-memory hash with it.
    engine.call("draft_save", json!({ "id": draft_id.clone(), "attachments": [] })).expect("remove the attachment");
    assert_eq!(engine.attachment_hash(&attachment_id), None, "the in-memory hash outlived its row");
    let error = engine.attachment_bytes(&attachment_id).expect_err("the attachment is gone");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("unknown attachment")), "{error}");
}
