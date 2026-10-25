//! P3c: the engine on the encrypted store.
//!
//! The fixture is the demo mailbox written into a throwaway SQLCipher database with a random key,
//! so the round trip exercises the same shape the UI and CLI already show. Nothing here touches the
//! real keyring, and every message uses `example.com` / `example.it`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use postino_core::{Account, Address, Attachment, DraftContent, Engine, Folder, FolderKind, Message, OpError};
use postino_store::{DraftUpdate, Store, StoreKey};

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
        for message in source.messages() {
            let account_id = source
                .folders()
                .iter()
                .find(|folder| folder.id == message.summary.folder_id)
                .map(|folder| folder.account_id.as_str())
                .expect("every fixture message lives in a folder");
            store.upsert_message(&message_row(message, account_id)).expect("message row");
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
        sorted_by_id(loaded.messages(), |message| &message.summary.id),
        sorted_by_id(demo.messages(), |message| &message.summary.id),
        "id, thread, folder, people, subject, snippet, date, flags, labels, body and attachments"
    );
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
    assert_eq!(
        loaded.message("msg_005").and_then(|message| message.body_html.as_deref()),
        demo.message("msg_005").and_then(|m| m.body_html.as_deref())
    );
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
        let created = engine.save_draft(None, draft("Bozza persistente", "Prima versione")).expect("new draft");
        let id = created.summary.id.clone();
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Drafts && folder.total == 2));

        let updated = engine.save_draft(Some(&id), draft("Bozza persistente", "Seconda versione")).expect("update");
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
    assert_eq!(engine.message("msg_004").map(|message| message.summary.folder_id.as_str()), Some("archive"));
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

    let engine = Engine::open(fixture.open()).expect("open");
    let message = engine.message("drf_1").expect("the draft row is visible as mail");
    assert_eq!(message.summary.folder_id, "drafts");
    assert_eq!(message.summary.subject, "Bozza orfana");
    assert_eq!(message.summary.to, vec![Address::new("Giulia", "giulia.ferri@example.com")]);
    assert_eq!(message.body_text, "Solo la riga drafts");
    assert_eq!(engine.folders().iter().find(|folder| folder.id == "drafts").map(|folder| folder.total), Some(1));
}

#[test]
fn a_repo_failure_surfaces_and_leaves_memory_unchanged() {
    let fixture = Fixture::new();
    fixture.seed(&Engine::demo_at(now()));
    let mut engine = Engine::open(fixture.open()).expect("engine");

    // A second connection removes the row behind the engine's back: the engine still has it, the
    // store no longer does. This is the shape of any write that the store refuses.
    let intruder = fixture.open();
    assert!(intruder.delete_message("msg_002").expect("delete the row behind the engine"));

    let folders_before = engine.folders().to_vec();
    let messages_before = engine.message("msg_002").cloned().expect("in memory");
    let error = engine.set_flags("msg_002", Some(true), None).expect_err("the store must refuse the flag");
    assert!(matches!(&error, OpError::Failed(detail) if detail.contains("local store refused")), "{error}");
    assert_eq!(engine.message("msg_002"), Some(&messages_before), "the message was mutated in memory anyway");
    assert_eq!(engine.folders(), folders_before.as_slice(), "counts were mutated after a failed write");

    let labels_before = engine.label_summaries();
    let error = engine.set_label("lbl_1", &["msg_002".to_owned()], false).expect_err("the store must refuse the label batch");
    assert!(matches!(&error, OpError::Failed(detail) if detail.contains("local store refused")), "{error}");
    assert_eq!(engine.label_summaries(), labels_before);
    assert_eq!(engine.message("msg_002").map(|message| message.summary.labels.clone()), Some(vec!["lbl_1".to_owned()]));
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
    engine.save_draft(None, content).expect("draft");
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
