//! P3c: the engine on the encrypted store.
//!
//! The fixture is the demo mailbox written into a throwaway SQLCipher database with a random key,
//! so the round trip exercises the same shape the UI and CLI already show. Nothing here touches the
//! real keyring, and every message uses `example.com` / `example.it`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use postino_core::{Account, Address, Attachment, DraftContent, Engine, Folder, FolderKind, Message, OpError};
use postino_store::{DraftUpdate, Store, StoreKey};
use serde_json::json;

const NOW: &str = "2026-10-24T10:42:00Z";

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

    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("open the store")
    }

    /// Write everything the engine exposes into the store, as onboarding (P6) and the M1 import
    /// will: accounts, folders, labels, then messages with their attachments and label ids.
    fn seed(&self, source: &Engine) {
        let store = self.open();
        for account in source.accounts() {
            store.upsert_account(&account_row(account)).expect("account row");
        }
        for folder in source.folders() {
            store.upsert_folder(&folder_row(folder)).expect("folder row");
        }
        for label in source.labels() {
            store
                .create_label(&postino_store::Label {
                    id: label.id.clone(),
                    account_id: None,
                    name: label.name.clone(),
                    color: label.color.as_str().to_owned(),
                })
                .expect("label row");
        }
        for summary in source.messages() {
            let Some(message) = source.message(&summary.id) else { continue };
            let account_id = source
                .folders()
                .iter()
                .find(|folder| folder.id == message.summary.folder_id)
                .map(|folder| folder.account_id.as_str())
                .expect("every fixture message lives in a folder");
            store.upsert_message(&message_row(&message, account_id)).expect("message row");
            store.replace_attachments(&message.summary.id, &attachment_rows(&message.attachments)).expect("attachment rows");
            for label in &message.summary.labels {
                store.apply_label(label, std::slice::from_ref(&message.summary.id)).expect("label assignment");
            }
        }
    }
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(NOW).expect("fixture instant").with_timezone(&Utc)
}

fn store_kind(kind: FolderKind) -> postino_store::FolderKind {
    match kind {
        FolderKind::Inbox => postino_store::FolderKind::Inbox,
        FolderKind::Starred => postino_store::FolderKind::Starred,
        FolderKind::Sent => postino_store::FolderKind::Sent,
        FolderKind::Drafts => postino_store::FolderKind::Drafts,
        FolderKind::Archive => postino_store::FolderKind::Archive,
        FolderKind::Spam => postino_store::FolderKind::Spam,
        FolderKind::Trash => postino_store::FolderKind::Trash,
        FolderKind::Custom => postino_store::FolderKind::Custom,
    }
}

fn store_address(address: &Address) -> postino_store::Address {
    postino_store::Address::new(address.name.clone(), address.email.clone())
}

fn account_row(account: &Account) -> postino_store::Account {
    postino_store::Account {
        id: account.id.clone(),
        name: account.name.clone(),
        email: account.email.clone(),
        provider: account.provider.clone(),
        signature: account.signature.clone(),
        created_at: 1_700_000_000_000,
    }
}

fn folder_row(folder: &Folder) -> postino_store::Folder {
    postino_store::Folder {
        id: folder.id.clone(),
        account_id: folder.account_id.clone(),
        name: folder.name.clone(),
        kind: store_kind(folder.kind),
        unread: folder.unread,
        total: folder.total,
        uid_validity: None,
        uid_next: None,
        last_sync_at: None,
    }
}

fn message_row(message: &Message, account_id: &str) -> postino_store::Message {
    postino_store::Message {
        summary: postino_store::MessageSummary {
            id: message.summary.id.clone(),
            account_id: account_id.to_owned(),
            folder_id: message.summary.folder_id.clone(),
            thread_id: message.summary.thread_id.clone(),
            from: store_address(&message.summary.from),
            to: message.summary.to.iter().map(store_address).collect(),
            subject: message.summary.subject.clone(),
            snippet: message.summary.snippet.clone(),
            date: message.summary.date.timestamp_millis(),
            unread: message.summary.unread,
            starred: message.summary.starred,
            has_attachments: message.summary.has_attachments,
            labels: Vec::new(),
        },
        uid: None,
        message_id: None,
        cc: message.cc.iter().map(store_address).collect(),
        bcc: message.bcc.iter().map(store_address).collect(),
        body_text: message.body_text.clone(),
        body_html: message.body_html.clone(),
        remote_images: message.remote_images,
        size: None,
    }
}

fn attachment_rows(attachments: &[Attachment]) -> Vec<postino_store::Attachment> {
    attachments
        .iter()
        .map(|attachment| postino_store::Attachment {
            id: attachment.id.clone(),
            filename: attachment.filename.clone(),
            mime: attachment.mime.clone(),
            size: attachment.size,
            blob_hash: None,
        })
        .collect()
}

fn sorted_by_id<T>(values: &[T], id: impl Fn(&T) -> &str) -> Vec<T>
where
    T: Clone,
{
    let mut values = values.to_vec();
    values.sort_by(|left, right| id(left).cmp(id(right)));
    values
}

fn draft(subject: &str, body: &str) -> DraftContent {
    DraftContent {
        to: vec![Address::new("Giulia Ferri", "giulia.ferri@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_owned(),
        body_text: body.to_owned(),
        attachments: Vec::new(),
        in_reply_to: None,
    }
}

fn attachment(id: &str) -> Attachment {
    Attachment { id: id.to_owned(), filename: format!("{id}.pdf"), mime: "application/pdf".to_owned(), size: 128 }
}

fn seed_two_accounts(fixture: &Fixture) {
    let store = fixture.open();
    for (id, name, email) in [("acc_a", "Account A", "a@example.com"), ("acc_b", "Account B", "b@example.com")] {
        store
            .upsert_account(&postino_store::Account {
                id: id.to_owned(),
                name: name.to_owned(),
                email: email.to_owned(),
                provider: "manual".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account");
        for (suffix, folder_name, kind) in [
            ("inbox", "Posta in arrivo", postino_store::FolderKind::Inbox),
            ("sent", "Inviati", postino_store::FolderKind::Sent),
            ("drafts", "Bozze", postino_store::FolderKind::Drafts),
        ] {
            store
                .upsert_folder(&postino_store::Folder {
                    id: format!("{id}_{suffix}"),
                    account_id: id.to_owned(),
                    name: folder_name.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                })
                .expect("folder");
        }
    }
}

#[test]
fn demo_shaped_mailbox_survives_a_round_trip() {
    let fixture = Fixture::new();
    let demo = Engine::demo_at(now());
    fixture.seed(&demo);

    let loaded = Engine::open(fixture.open()).expect("the engine loads the store");
    assert!(loaded.is_persistent());
    assert!(!loaded.is_demo(), "store data is real local data, not the synthetic demo");

    assert_eq!(loaded.accounts(), demo.accounts());
    assert_eq!(
        sorted_by_id(loaded.folders(), |folder| &folder.id),
        sorted_by_id(demo.folders(), |folder| &folder.id),
        "folder ids, kinds and counts"
    );
    assert_eq!(
        sorted_by_id(loaded.messages(), |message| &message.id),
        sorted_by_id(demo.messages(), |message| &message.id),
        "id, thread, folder, people, subject, snippet, date, flags and labels"
    );
    for summary in demo.messages() {
        let loaded_message = loaded.message(&summary.id).expect("the loaded body");
        let demo_message = demo.message(&summary.id).expect("the demo body");
        assert_eq!(loaded_message.body_text, demo_message.body_text, "body of {}", summary.id);
        assert_eq!(loaded_message.body_html, demo_message.body_html, "html of {}", summary.id);
        assert_eq!(loaded_message.attachments, demo_message.attachments, "attachments of {}", summary.id);
    }
    assert_eq!(sorted_by_id(loaded.labels(), |label| &label.id), sorted_by_id(demo.labels(), |label| &label.id));

    // Spot checks with literal values, so a mapping that round-trips a mistake still fails.
    let folder = |id: &str| loaded.folders().iter().find(|folder| folder.id == id).map(|folder| (folder.total, folder.unread));
    assert_eq!(folder("inbox"), Some((7, 4)));
    assert_eq!(folder("starred"), Some((4, 1)), "the virtual folder is recounted from the messages");
    assert_eq!(folder("drafts"), Some((1, 0)));
    let message = loaded.message("msg_001").expect("msg_001");
    assert_eq!(message.summary.from.display(), "Giulia Ferri <giulia.ferri@example.com>");
    assert_eq!(message.summary.to, vec![Address::new("Federico", "federico@example.it")]);
    assert!(message.summary.unread && message.summary.starred);
    assert_eq!(message.summary.labels, vec!["lbl_1"]);
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].filename, "preventivo-postino.pdf");
    assert_eq!(message.attachments[0].size, 248_320);
    assert!(message.body_text.contains("4.800 € + IVA"));
    assert_eq!(loaded.message("msg_005").and_then(|message| message.body_html), demo.message("msg_005").and_then(|m| m.body_html));
    for summary in demo.label_summaries() {
        assert_eq!(loaded.label_summary(&summary.id), Some(summary.clone()), "label counts for {}", summary.id);
    }
}

#[test]
fn draft_save_revision_and_send_survive_a_reopen() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));

    let draft_id = {
        let mut engine = Engine::open(fixture.open()).expect("engine");
        let created = engine.save_draft(None, None, draft("Bozza persistente", "Prima versione")).expect("new draft");
        let id = created.summary.id.clone();
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Drafts && folder.total == 2));

        let updated = engine.save_draft(None, Some(&id), draft("Bozza persistente", "Seconda versione")).expect("update");
        assert_eq!(updated.summary.id, id, "an update must not create a second draft");
        assert_eq!(updated.body_text, "Seconda versione");
        id
    };

    let reopened = Engine::open(fixture.open()).expect("reopen");
    let draft_message = reopened.message(&draft_id).expect("the draft survived the restart");
    assert_eq!(draft_message.body_text, "Seconda versione");
    assert_eq!(draft_message.summary.folder_id, "drafts");
    assert_eq!(fixture.open().get_draft(&draft_id).expect("draft row").expect("draft").revision, 2, "the store owns the revision");

    let mut engine = reopened;
    let sent = engine.send_draft(&draft_id).expect("local demo submission");
    assert_eq!(sent.folder_id, "sent");
    assert!(!sent.unread && !sent.starred);
    assert!(fixture.open().get_draft(&draft_id).expect("draft row").is_none(), "a sent draft is no longer a draft");
    drop(engine);

    let reloaded = Engine::open(fixture.open()).expect("second reopen");
    let message = reloaded.message(&draft_id).expect("the sent message survived");
    assert_eq!(message.summary.folder_id, "sent");
    assert_eq!(message.body_text, "Seconda versione");
    let folder = |id: &str| reloaded.folders().iter().find(|folder| folder.id == id).map(|folder| (folder.total, folder.unread));
    assert_eq!(folder("drafts"), Some((1, 0)), "only the demo draft is left");
    assert_eq!(folder("sent"), Some((3, 0)), "the two demo messages plus the submitted draft");
}

#[test]
fn labels_flags_and_moves_survive_a_reopen() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let mut engine = Engine::open(fixture.open()).expect("engine");

    assert_eq!(engine.set_label("lbl_2", &["msg_001".to_owned()], true).expect("apply").1, 1);
    assert_eq!(engine.set_label("lbl_1", &["msg_003".to_owned()], false).expect("remove").1, 1);
    engine.move_message("msg_004", "archive").expect("move");
    let flagged = engine.set_flags("msg_007", Some(false), Some(true)).expect("flags");
    assert!(!flagged.unread && flagged.starred);
    let before = engine.folders().to_vec();
    drop(engine);

    let mut engine = Engine::open(fixture.open()).expect("reopen");
    assert_eq!(engine.message("msg_001").map(|message| message.summary.labels.clone()), Some(vec!["lbl_1".to_owned(), "lbl_2".to_owned()]));
    assert_eq!(engine.message("msg_003").map(|message| message.summary.labels.clone()), Some(Vec::new()));
    assert_eq!(engine.message("msg_004").map(|message| message.summary.folder_id.clone()), Some("archive".to_owned()));
    let flagged = engine.message("msg_007").expect("msg_007");
    assert!(!flagged.summary.unread && flagged.summary.starred);
    assert_eq!(engine.folders(), before.as_slice(), "folder counts are preserved exactly");

    // The same flag write through the reopened engine still updates the cached counts.
    engine.set_flags("msg_007", Some(true), None).expect("mark unread");
    assert!(fixture.open().get_message("msg_007").expect("row").expect("message").summary.unread, "the flag write reached the store");
}

#[test]
fn a_draft_row_without_a_display_row_is_still_loaded() {
    let fixture = Fixture::new();
    {
        let store = fixture.open();
        store
            .upsert_account(&postino_store::Account {
                id: "acc_test".to_owned(),
                name: "Test".to_owned(),
                email: "test@example.com".to_owned(),
                provider: "manual".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account");
        store
            .upsert_folder(&postino_store::Folder {
                id: "drafts".to_owned(),
                account_id: "acc_test".to_owned(),
                name: "Bozze".to_owned(),
                kind: postino_store::FolderKind::Drafts,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("drafts folder");
        store
            .save_draft(&DraftUpdate {
                id: "drf_1".to_owned(),
                account_id: "acc_test".to_owned(),
                updated_at: now().timestamp_millis(),
                body_text: "Solo la riga drafts".to_owned(),
                to: vec![postino_store::Address::new("Giulia", "giulia.ferri@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza orfana".to_owned(),
                in_reply_to: None,
            })
            .expect("draft row");
    }

    let mut engine = Engine::open(fixture.open()).expect("open");
    let message = engine.message("drf_1").expect("the draft row is visible as mail");
    assert_eq!(message.summary.folder_id, "drafts");
    assert_eq!(message.summary.subject, "Bozza orfana");
    assert_eq!(message.summary.to, vec![Address::new("Giulia", "giulia.ferri@example.com")]);
    assert_eq!(message.body_text, "Solo la riga drafts");
    assert_eq!(engine.folders().iter().find(|folder| folder.id == "drafts").map(|folder| folder.total), Some(1));

    // The orphan is now a real message row in the store, not a memory-only ghost: store counts,
    // flags and label writes work against it.
    let store = fixture.open();
    let repaired = store.get_message("drf_1").expect("message row").expect("the repair wrote a display row");
    assert_eq!(repaired.summary.folder_id, "drafts");
    assert_eq!(store.counts(Some("drafts")).expect("counts"), postino_store::MessageCounts { total: 1, unread: 0 });
    assert_eq!(store.get_folder("drafts").expect("folder").expect("drafts").total, 1);
    drop(store);
    engine.set_flags("drf_1", Some(true), None).expect("the repaired draft accepts a flag write");
    assert!(fixture.open().get_message("drf_1").expect("row").expect("message").summary.unread);
}

#[test]
fn an_orphan_draft_with_an_unknown_account_fails_the_open() {
    let fixture = Fixture::new();
    // An orphan that names an unknown account cannot be built through `save_draft`: the repository
    // requires the account to exist. Insert it with a raw connection, exactly as an older build's
    // crash or a corrupt store could have left it, with foreign keys off for this fixture write.
    drop(fixture.open());
    let connection = raw_connection(&fixture);
    connection.execute_batch("PRAGMA foreign_keys = OFF;").expect("disable foreign keys for the fixture row");
    connection
        .execute(
            "INSERT INTO drafts(id, account_id, revision, updated_at, body_text, to_json, cc_json, bcc_json, subject, in_reply_to)
             VALUES('drf_orphan', 'acc_missing', 1, ?1, 'Corpo', '[]', '[]', '[]', 'Bozza orfana', NULL)",
            [now().timestamp_millis()],
        )
        .expect("an orphan draft row with an unknown account");
    drop(connection);

    let error = Engine::open(fixture.open()).err().expect("an orphan with an unknown account must fail the open");
    assert!(
        matches!(&error, OpError::Failed(detail) if detail.contains("drf_orphan") && detail.contains("acc_missing")),
        "the failure must name the draft and the account: {error}"
    );
    assert!(fixture.open().get_message("drf_orphan").expect("message row").is_none(), "a failed repair wrote a display row");
}

#[test]
fn an_orphan_draft_without_a_drafts_folder_fails_the_open() {
    let fixture = Fixture::new();
    {
        let store = fixture.open();
        store
            .upsert_account(&postino_store::Account {
                id: "acc_test".to_owned(),
                name: "Test".to_owned(),
                email: "test@example.com".to_owned(),
                provider: "manual".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account");
        // No folder row: `save_draft` itself only needs the account, so this is a repository-built
        // orphan rather than a raw row.
        store
            .save_draft(&DraftUpdate {
                id: "drf_no_folder".to_owned(),
                account_id: "acc_test".to_owned(),
                updated_at: now().timestamp_millis(),
                body_text: "Corpo".to_owned(),
                to: Vec::new(),
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza senza cartella".to_owned(),
                in_reply_to: None,
            })
            .expect("orphan draft row");
    }

    let error = Engine::open(fixture.open()).err().expect("an orphan without a Drafts folder must fail the open");
    assert!(
        matches!(&error, OpError::Failed(detail) if detail.contains("drf_no_folder") && detail.contains("acc_test") && detail.contains("Drafts")),
        "the failure must name the draft, the account and the missing folder: {error}"
    );
    assert!(fixture.open().get_message("drf_no_folder").expect("message row").is_none(), "a failed repair wrote a display row");
}

#[test]
fn a_repaired_draft_keeps_a_label_and_a_flag_across_a_reopen() {
    let fixture = Fixture::new();
    {
        let store = fixture.open();
        store
            .upsert_account(&postino_store::Account {
                id: "acc_test".to_owned(),
                name: "Test".to_owned(),
                email: "test@example.com".to_owned(),
                provider: "manual".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account");
        store
            .upsert_folder(&postino_store::Folder {
                id: "drafts".to_owned(),
                account_id: "acc_test".to_owned(),
                name: "Bozze".to_owned(),
                kind: postino_store::FolderKind::Drafts,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("drafts folder");
        store
            .create_label(&postino_store::Label { id: "lbl_7".to_owned(), account_id: None, name: "Riparata".to_owned(), color: "forest".to_owned() })
            .expect("label");
        store
            .save_draft(&DraftUpdate {
                id: "drf_repaired".to_owned(),
                account_id: "acc_test".to_owned(),
                updated_at: now().timestamp_millis(),
                body_text: "Corpo".to_owned(),
                to: vec![postino_store::Address::new("Giulia", "giulia.ferri@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza riparata".to_owned(),
                in_reply_to: None,
            })
            .expect("orphan draft row");
    }

    let mut engine = Engine::open(fixture.open()).expect("the orphan is repaired on open");
    assert_eq!(engine.message("drf_repaired").map(|message| message.summary.folder_id.clone()), Some("drafts".to_owned()));
    assert_eq!(engine.set_label("lbl_7", &["drf_repaired".to_owned()], true).expect("apply a label").1, 1);
    let flagged = engine.set_flags("drf_repaired", Some(true), Some(true)).expect("set a flag");
    assert!(flagged.unread && flagged.starred);
    let drafts = engine.folders().iter().find(|folder| folder.id == "drafts").expect("drafts folder");
    let engine_counts = (drafts.total, drafts.unread);
    assert_eq!(engine_counts, (1, 1), "the engine counts did not follow the writes");
    drop(engine);

    let store = fixture.open();
    let stored = store.get_message("drf_repaired").expect("row").expect("repaired message row");
    assert_eq!(stored.summary.labels, vec!["lbl_7".to_owned()], "the label write did not reach the store");
    assert!(stored.summary.unread && stored.summary.starred, "the flag write did not reach the store");
    let stored_folder = store.get_folder("drafts").expect("folder").expect("drafts folder");
    assert_eq!((stored_folder.total, stored_folder.unread), engine_counts, "the store folder counts disagree with the engine after the writes");
    assert_eq!(
        store.counts(Some("drafts")).expect("live counts"),
        postino_store::MessageCounts { total: 1, unread: 1 },
        "the live message counts disagree with the folder cache"
    );
    drop(store);

    let reopened = Engine::open(fixture.open()).expect("reopen the store");
    let message = reopened.message("drf_repaired").expect("the repaired draft survived the reopen");
    assert_eq!(message.summary.labels, vec!["lbl_7".to_owned()], "the label was lost on reopen");
    assert!(message.summary.unread && message.summary.starred, "the flag was lost on reopen");
    let drafts = reopened.folders().iter().find(|folder| folder.id == "drafts").expect("drafts folder");
    assert_eq!((drafts.total, drafts.unread), engine_counts, "the reopened engine counts changed");
    let stored_folder = fixture.open().get_folder("drafts").expect("folder").expect("drafts folder");
    assert_eq!(
        (stored_folder.total, stored_folder.unread),
        (drafts.total, drafts.unread),
        "the store folder counts disagree with the reopened engine"
    );
}

#[test]
fn a_repo_failure_surfaces_and_leaves_memory_unchanged() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let mut engine = Engine::open(fixture.open()).expect("engine");

    // A second connection removes the row behind the engine's back: the engine still has it, the
    // store no longer does. This is a caller-visible unknown id, not a disk problem.
    let intruder = fixture.open();
    assert!(intruder.delete_message("msg_002").expect("delete the row behind the engine"));

    let folders_before = engine.folders().to_vec();
    let messages_before = engine.summary("msg_002").expect("in memory").clone();
    let error = engine.set_flags("msg_002", Some(true), None).expect_err("the store must refuse the flag");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("unknown message")), "{error}");
    assert!(!error.to_string().contains("free disk space"), "a caller mistake must not be reported as a disk problem");
    assert_eq!(engine.summary("msg_002"), Some(&messages_before), "the message was mutated in memory anyway");
    assert_eq!(engine.folders(), folders_before.as_slice(), "counts were mutated after a failed write");

    let labels_before = engine.label_summaries();
    let error = engine.set_label("lbl_1", &["msg_002".to_owned()], false).expect_err("the store must refuse the label batch");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("unknown message")), "{error}");
    assert_eq!(engine.label_summaries(), labels_before);
    assert_eq!(engine.summary("msg_002").map(|message| message.labels.clone()), Some(vec!["lbl_1".to_owned()]));
}

#[test]
fn storage_failures_are_failed_with_the_storage_advice() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let mut engine = Engine::open(fixture.open()).expect("engine");
    let before = engine.message("msg_001").expect("msg_001");

    // A real storage failure: the table disappears under the engine. This is not a caller mistake,
    // so it must say the store refused the change and that memory is untouched.
    let connection = raw_connection(&fixture);
    connection.execute_batch("DROP TABLE messages").expect("drop the messages table");
    drop(connection);
    let error = engine.set_flags("msg_001", Some(true), None).expect_err("a missing table must fail");
    assert!(
        matches!(&error, OpError::Failed(detail)
            if detail.contains("local store refused") && detail.contains("rolled back") && detail.contains("free disk space")),
        "{error}"
    );
    assert_eq!(engine.message("msg_001"), Some(before), "memory changed after a storage failure");
}

fn raw_connection(fixture: &Fixture) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(fixture.path()).expect("raw open");
    let hex = fixture.key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("apply the key");
    connection
}

#[test]
fn a_failed_draft_save_leaves_store_rows_and_revision_unchanged() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let mut engine = Engine::open(fixture.open()).expect("engine");

    let mut first_content = draft("Bozza A", "Prima versione");
    first_content.attachments.push(attachment("att_a"));
    let first = engine.save_draft(None, None, first_content).expect("first draft");
    let mut second_content = draft("Bozza B", "Seconda versione");
    second_content.attachments.push(attachment("att_b"));
    let second = engine.save_draft(None, None, second_content).expect("second draft");

    let before_message = engine.message(&first.summary.id).expect("in-memory draft");
    let store_before = fixture.open();
    let before_row = store_before.get_message(&first.summary.id).expect("row").expect("first row");
    let before_revision = store_before.get_draft(&first.summary.id).expect("draft").expect("first draft row").revision;
    let before_attachments = store_before.attachments(&first.summary.id).expect("attachments");
    drop(store_before);

    // The update tries to re-use an attachment id owned by the second draft. The store must refuse
    // it before writing anything, and memory must stay exactly as it was.
    let mut conflicting = draft("Bozza A", "Non deve essere salvata");
    conflicting.attachments.push(attachment("att_b"));
    let error = engine.save_draft(None, Some(&first.summary.id), conflicting).expect_err("cross-message attachment id");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("belongs to message") && detail.contains("omit")), "{error}");
    assert!(!error.to_string().contains("free disk space"), "a caller mistake must not carry disk advice");
    assert_eq!(engine.message(&first.summary.id), Some(before_message), "memory changed after a failed draft save");

    let mut duplicated = draft("Bozza A", "Duplicato");
    duplicated.attachments.push(attachment("att_dup"));
    duplicated.attachments.push(attachment("att_dup"));
    let error = engine.save_draft(None, Some(&first.summary.id), duplicated).expect_err("duplicate attachment id");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("repeated")), "{error}");

    let store_after = fixture.open();
    assert_eq!(store_after.get_message(&first.summary.id).expect("row").expect("first row"), before_row);
    assert_eq!(store_after.get_draft(&first.summary.id).expect("draft").expect("first draft row").revision, before_revision);
    assert_eq!(store_after.attachments(&first.summary.id).expect("attachments"), before_attachments);
    assert_eq!(store_after.get_draft(&second.summary.id).expect("draft").expect("second draft row").revision, 1);
}

#[test]
fn a_subject_only_patch_keeps_the_html_body_and_the_reply_link() {
    const HTML: &str = "<p>Corpo <strong>HTML</strong></p>";

    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let id = {
        let mut engine = Engine::open(fixture.open()).expect("engine");
        engine.save_draft(None, None, draft("Oggetto originale", "Corpo originale")).expect("draft").summary.id
    };

    // Put the two fields the composer does not own into the store: an HTML part on the message row
    // and the reply linkage on the revision row.
    {
        let store = fixture.open();
        let mut row = store.get_message(&id).expect("row").expect("draft row");
        row.body_html = Some(HTML.to_owned());
        store.upsert_message(&row).expect("store the HTML body");
        let draft_row = store.get_draft(&id).expect("draft row").expect("draft");
        store
            .save_draft(&DraftUpdate {
                id: id.clone(),
                account_id: draft_row.account_id,
                updated_at: draft_row.updated_at.saturating_add(1),
                body_text: draft_row.body_text,
                to: draft_row.to,
                cc: draft_row.cc,
                bcc: draft_row.bcc,
                subject: draft_row.subject,
                in_reply_to: Some("msg_001@example.com".to_owned()),
            })
            .expect("link the reply");
    }

    let mut engine = Engine::open(fixture.open()).expect("reopen");
    assert_eq!(engine.message(&id).and_then(|message| message.body_html), Some(HTML.to_owned()), "the HTML body did not survive the engine load");
    // The registry patch path: only `subject` is present, so `draft_content` keeps the rest.
    let patched = engine.call("draft_save", json!({ "id": id.clone(), "subject": "Solo l'oggetto cambia" })).expect("patch the subject");
    assert_eq!(patched["draft"]["summary"]["subject"], "Solo l'oggetto cambia");
    assert_eq!(patched["draft"]["body_html"], json!(HTML), "the subject-only patch cleared body_html in memory");
    assert_eq!(patched["draft"]["body_text"], "Corpo originale");
    drop(engine);

    let reopened = Engine::open(fixture.open()).expect("second reopen");
    let message = reopened.message(&id).expect("the draft survived");
    assert_eq!(message.body_html.as_deref(), Some(HTML), "the subject-only patch cleared body_html in the store");
    assert_eq!(message.body_text, "Corpo originale");
    let stored = fixture.open().get_draft(&id).expect("draft row").expect("revision row");
    assert_eq!(stored.in_reply_to.as_deref(), Some("msg_001@example.com"), "the subject-only patch cleared in_reply_to");
    assert_eq!(stored.revision, 3);
}

#[test]
fn two_accounts_route_drafts_and_sent_to_the_owning_account() {
    let fixture = Fixture::new();
    seed_two_accounts(&fixture);
    let mut engine = Engine::open(fixture.open()).expect("engine");

    let created = engine.save_draft(Some("acc_b"), None, draft("Per B", "Corpo")).expect("draft for B");
    assert_eq!(created.summary.folder_id, "acc_b_drafts", "the draft must live in B's Drafts");
    let error = engine.save_draft(Some("acc_a"), Some(&created.summary.id), draft("Spostata", "Corpo")).expect_err("account mismatch on update");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("belongs to account")), "{error}");
    let sent = engine.send_draft(&created.summary.id).expect("send from B");
    assert_eq!(sent.folder_id, "acc_b_sent", "B's draft must not move into A's Sent");

    let store = fixture.open();
    let row = store.get_message(&created.summary.id).expect("row").expect("sent row");
    assert_eq!(row.summary.account_id, "acc_b");
    assert_eq!(row.summary.folder_id, "acc_b_sent");
    assert_eq!(store.get_folder("acc_a_sent").expect("A sent").expect("A sent").total, 0, "A's Sent was touched");
    assert_eq!(store.get_folder("acc_a_drafts").expect("A drafts").expect("A drafts").total, 0, "A's Drafts was touched");
    assert_eq!(store.get_folder("acc_b_sent").expect("B sent").expect("B sent").total, 1);
    drop(store);

    let error = engine.save_draft(None, None, draft("Senza account", "Corpo")).expect_err("two accounts need an explicit one");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("accounts") && detail.contains("account")), "{error}");
    let error = engine.save_draft(Some("acc_missing"), None, draft("Sconosciuto", "Corpo")).expect_err("unknown account");
    assert!(matches!(&error, OpError::InvalidArgs(detail) if detail.contains("unknown account")), "{error}");
}

#[test]
fn draft_revisions_never_go_backwards_across_send_and_move_back() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let mut engine = Engine::open(fixture.open()).expect("engine");

    let created = engine.save_draft(None, None, draft("Revisioni", "Prima")).expect("draft");
    let id = created.summary.id.clone();
    assert_eq!(fixture.open().get_draft(&id).expect("draft").expect("row").revision, 1);
    engine.save_draft(None, Some(&id), draft("Revisioni", "Seconda")).expect("second save");
    assert_eq!(fixture.open().get_draft(&id).expect("draft").expect("row").revision, 2);

    engine.send_draft(&id).expect("send");
    assert!(fixture.open().get_draft(&id).expect("draft").is_none());
    engine.move_message(&id, "drafts").expect("move back to Drafts");
    engine.save_draft(None, Some(&id), draft("Revisioni", "Terza")).expect("save after move back");
    assert_eq!(fixture.open().get_draft(&id).expect("draft").expect("row").revision, 3, "the revision restarted after send");
}

#[test]
fn deleting_a_message_removes_its_draft_across_a_reopen() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let id = {
        let mut engine = Engine::open(fixture.open()).expect("engine");
        engine.save_draft(None, None, draft("Da cancellare", "Corpo")).expect("draft").summary.id
    };
    assert!(fixture.open().delete_message(&id).expect("delete"));
    assert!(fixture.open().get_draft(&id).expect("draft").is_none());

    let reopened = Engine::open(fixture.open()).expect("reopen");
    assert!(reopened.message(&id).is_none(), "the deleted draft came back after reopen");
    assert_eq!(reopened.folders().iter().find(|folder| folder.id == "drafts").map(|folder| folder.total), Some(1), "only the demo draft remains");
}

#[test]
fn label_ids_do_not_go_backwards_across_a_reopen() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    {
        let mut engine = Engine::open(fixture.open()).expect("engine");
        engine.delete_label("lbl_3").expect("delete the highest label");
    }
    let created = {
        let mut engine = Engine::open(fixture.open()).expect("reopen");
        engine.create_label("Nuova", postino_core::LabelColor::Forest).expect("create after reopen")
    };
    assert_eq!(created.id, "lbl_4", "a deleted label id came back after reopen");

    // An older store without the meta row is repaired from the surviving ids on open.
    fixture.open().set_meta(postino_store::LABEL_SEQ_KEY, "0").expect("forge an old meta value");
    let mut engine = Engine::open(fixture.open()).expect("second reopen");
    let next = engine.create_label("Altra", postino_core::LabelColor::Muted).expect("create after repair");
    assert_eq!(next.id, "lbl_5");
    assert_eq!(fixture.open().get_meta(postino_store::LABEL_SEQ_KEY).expect("meta").as_deref(), Some("5"));
}

#[test]
fn engine_writes_never_land_in_plaintext() {
    const SUBJECT: &str = "CANARY-ENGINE-SUBJECT-5d17";
    const BODY: &str = "CANARY-ENGINE-BODY-5d17";
    const RECIPIENT: &str = "canary.engine@example.com";

    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let mut engine = Engine::open(fixture.open()).expect("engine");
    let mut content = draft(SUBJECT, BODY);
    content.to = vec![Address::new("Canary", RECIPIENT)];
    engine.save_draft(None, None, content).expect("draft");
    drop(engine);

    let files = std::fs::read_dir(fixture.dir.path())
        .expect("read the store directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert!(!files.is_empty(), "no store files to scan");
    for path in &files {
        let bytes = std::fs::read(path).expect("read a store file");
        for canary in [SUBJECT, BODY, RECIPIENT] {
            assert!(!contains(&bytes, canary.as_bytes()), "plaintext {canary:?} leaked into {}", path.display());
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}
