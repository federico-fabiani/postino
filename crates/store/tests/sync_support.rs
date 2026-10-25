//! Sync support in the repository: the per-message bulk upsert, sync cursors, UID listing, the
//! deletion pass and the UIDVALIDITY reset. Every mutation must be one transaction and must never
//! destroy a local draft.

use std::path::PathBuf;

use postino_store::{
    Account, Address, Attachment, FlagUpdate, Folder, FolderKind, Label, Message, MessageFlags, MessageSummary, Store, StoreError, StoreKey,
    SyncState,
};

const ACCOUNT: &str = "acc_sync";
const INBOX: &str = "fld_inbox";
const ARCHIVE: &str = "fld_archive";
const DRAFTS: &str = "fld_drafts";

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
        self.dir.path().join("sync.db")
    }

    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("open the store")
    }
}

fn folder(id: &str, name: &str, kind: FolderKind) -> Folder {
    Folder {
        id: id.to_owned(),
        account_id: ACCOUNT.to_owned(),
        name: name.to_owned(),
        kind,
        unread: 0,
        total: 0,
        uid_validity: Some(1),
        uid_next: Some(1),
        last_sync_at: None,
    }
}

fn seed(store: &Store) {
    store
        .upsert_account(&Account {
            id: ACCOUNT.to_owned(),
            name: "Studio Verdi".to_owned(),
            email: "studio@example.com".to_owned(),
            provider: "test".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    for (id, name, kind) in [(INBOX, "INBOX", FolderKind::Inbox), (ARCHIVE, "Archive", FolderKind::Archive), (DRAFTS, "Drafts", FolderKind::Drafts)] {
        store.upsert_folder(&folder(id, name, kind)).expect("folder");
    }
}

fn message(id: &str, folder_id: &str, uid: Option<u32>) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: ACCOUNT.to_owned(),
            folder_id: folder_id.to_owned(),
            thread_id: format!("thread_{id}"),
            from: Address::new("Giulia", "giulia@example.com"),
            to: vec![Address::new("Me", "me@example.com")],
            subject: format!("subject {id}"),
            snippet: "body".to_owned(),
            date: 1_700_000_100_000,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid,
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "body".to_owned(),
        body_html: None,
        size: Some(128),
    }
}

fn attachment(id: &str) -> Attachment {
    Attachment { id: id.to_owned(), filename: "report.pdf".to_owned(), mime: "application/pdf".to_owned(), size: 4, blob_hash: Some("a".repeat(64)) }
}

#[test]
fn sync_state_round_trips_and_clears() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    assert_eq!(store.get_sync_state(INBOX).expect("read"), None);

    store.set_sync_state(&SyncState { folder_id: INBOX.to_owned(), uid_next: 42, highest_modseq: Some(9_000), last_error: None }).expect("write");
    let state = store.get_sync_state(INBOX).expect("read").expect("a row");
    assert_eq!(state.uid_next, 42);
    assert_eq!(state.highest_modseq, Some(9_000));

    store
        .set_sync_state(&SyncState { folder_id: INBOX.to_owned(), uid_next: 43, highest_modseq: None, last_error: Some("retry".to_owned()) })
        .expect("update");
    let state = store.get_sync_state(INBOX).expect("read").expect("a row");
    assert_eq!(state.uid_next, 43);
    assert_eq!(state.highest_modseq, None);
    assert_eq!(state.last_error.as_deref(), Some("retry"));

    store.clear_sync_state(INBOX).expect("clear");
    assert_eq!(store.get_sync_state(INBOX).expect("read"), None);

    let error = store
        .set_sync_state(&SyncState { folder_id: "fld_missing".to_owned(), uid_next: 1, highest_modseq: None, last_error: None })
        .expect_err("unknown folder");
    assert!(matches!(error, StoreError::NotFound { entity: "folder", .. }), "{error:?}");
}

#[test]
fn one_transaction_writes_the_message_and_its_attachments() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let mut row = message("msg_one", INBOX, Some(7));
    row.summary.has_attachments = true;
    store.upsert_synced_message(&row, &[attachment("att_msg_one_0")]).expect("upsert");

    let stored = store.get_message("msg_one").expect("read").expect("message");
    assert_eq!(stored.uid, Some(7));
    assert!(stored.summary.has_attachments);
    let attachments = store.attachments("msg_one").expect("attachments");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments.first().map(|attachment| attachment.blob_hash.clone()), Some(Some("a".repeat(64))));
    // The bulk path does not refresh counts; the caller does it once per folder.
    assert_eq!(store.counts(Some(INBOX)).expect("counts").total, 1);
    let counts = store.recount_folder(INBOX).expect("recount");
    assert_eq!(counts.total, 1);
    assert_eq!(counts.unread, 1);

    // A repeated attachment id violates the primary key: the message row must be rolled back with it.
    let duplicate = [attachment("att_msg_two_0"), attachment("att_msg_two_0")];
    let error = store.upsert_synced_message(&message("msg_two", INBOX, Some(8)), &duplicate).expect_err("duplicate attachment id");
    assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
    assert_eq!(store.get_message("msg_two").expect("read"), None, "the failed upsert left a message behind");
}

#[test]
fn message_flags_reads_only_the_state_a_sync_needs() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_message(&message("msg_one", INBOX, Some(3))).expect("message");
    store.set_flags(&["msg_one".to_owned()], FlagUpdate { unread: Some(false), starred: Some(true) }).expect("flags");

    let flags = store.message_flags("msg_one").expect("read").expect("a row");
    assert_eq!(flags, MessageFlags { folder_id: INBOX.to_owned(), unread: false, starred: true });
    assert_eq!(store.message_flags("msg_missing").expect("read"), None);
}

#[test]
fn the_uid_list_ignores_rows_without_a_uid() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_message(&message("msg_one", INBOX, Some(3))).expect("message");
    store.upsert_message(&message("msg_two", INBOX, Some(1))).expect("message");
    store.upsert_message(&message("msg_local", INBOX, None)).expect("local row");

    let uids = store.list_uids(INBOX).expect("list");
    assert_eq!(uids.iter().map(|entry| entry.uid).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(uids.first().map(|entry| entry.message_id.as_str()), Some("msg_two"));
    assert!(store.list_uids("fld_missing").is_err());
}

#[test]
fn a_deletion_pass_removes_messages_and_assignments_but_never_a_local_draft() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    store.upsert_message(&message("msg_one", INBOX, Some(1))).expect("message");
    store.upsert_message(&message("msg_two", INBOX, Some(2))).expect("message");
    store.upsert_message(&message("msg_other", ARCHIVE, Some(9))).expect("message");
    // A local draft lives in Drafts, has no UID and must survive any deletion pass.
    store.save_draft_message(&message("msg_draft", DRAFTS, None), &[]).expect("draft");
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store.apply_label("lbl_1", &["msg_one".to_owned(), "msg_two".to_owned()]).expect("apply");
    store.replace_attachments("msg_two", &[attachment("att_msg_two_0")]).expect("attachment");

    let removed = store.delete_synced_messages(INBOX, &["msg_one".to_owned(), "msg_two".to_owned(), "msg_draft".to_owned()]).expect("delete");
    assert_eq!(removed, 2, "the local draft is not a deletion candidate");
    assert_eq!(store.get_message("msg_one").expect("read"), None);
    assert_eq!(store.get_message("msg_two").expect("read"), None);
    assert!(store.attachments("msg_two").is_err(), "the attachment row must cascade with its message");
    assert!(store.labels_for("msg_two").expect("labels").is_empty());
    assert!(store.get_message("msg_draft").expect("read").is_some(), "the local draft was destroyed");
    assert!(store.get_draft("msg_draft").expect("draft").is_some());
    assert!(store.get_message("msg_other").expect("read").is_some(), "another folder was touched");
    assert_eq!(store.counts(Some(INBOX)).expect("counts").total, 0);
    assert_eq!(store.get_folder(INBOX).expect("folder").expect("row").total, 0);
}

#[test]
fn a_uidvalidity_reset_clears_one_folder_and_keeps_local_drafts_and_labels_elsewhere() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    store.upsert_message(&message("msg_one", INBOX, Some(1))).expect("message");
    store.upsert_message(&message("msg_two", INBOX, Some(2))).expect("message");
    store.upsert_message(&message("msg_other", ARCHIVE, Some(9))).expect("message");
    store.save_draft_message(&message("msg_draft", INBOX, None), &[]).expect("draft");
    // A message that was sent locally has no UID and no drafts row: it was never on the server.
    store.upsert_message(&message("msg_local_sent", INBOX, None)).expect("local sent message");
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store.apply_label("lbl_1", &["msg_one".to_owned(), "msg_other".to_owned()]).expect("apply");
    store.set_sync_state(&SyncState { folder_id: INBOX.to_owned(), uid_next: 3, highest_modseq: None, last_error: None }).expect("state");

    let removed = store.reset_folder_messages(INBOX).expect("reset");
    assert_eq!(removed, 2);
    assert_eq!(store.get_message("msg_one").expect("read"), None);
    assert!(store.labels_for("msg_one").expect("labels").is_empty());
    assert_eq!(store.get_sync_state(INBOX).expect("state"), None);
    assert!(store.get_message("msg_draft").expect("read").is_some(), "a local draft is not a server message");
    assert!(store.get_draft("msg_draft").expect("draft").is_some());
    assert!(store.get_message("msg_local_sent").expect("read").is_some(), "locally sent mail has no server UID and must survive");
    let other = store.get_message("msg_other").expect("read").expect("message");
    assert_eq!(other.summary.folder_id, ARCHIVE);
    assert_eq!(store.labels_for("msg_other").expect("labels"), vec!["lbl_1".to_owned()]);
    assert_eq!(store.get_folder(INBOX).expect("folder").expect("row").total, 2, "only the two local-only rows remain");
    assert!(store.get_message("msg_one").expect("read").is_none());
    assert_eq!(store.get_folder(ARCHIVE).expect("folder").expect("row").total, 1);
}
