//! Schema v4 account configuration: one server row per account, an atomic account save and a
//! whole-account cache delete that never touches mailbox-wide labels.

use std::path::PathBuf;

use postino_store::{
    Account, AccountServer, AccountTls, Address, Attachment, DraftUpdate, Folder, FolderKind, Label, Message, MessageSummary, Store, StoreKey,
};

const ACCOUNT_A: &str = "acc_a";
const ACCOUNT_B: &str = "acc_b";

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
        self.dir.path().join("accounts.db")
    }

    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("open the store")
    }
}

fn account(id: &str, email: &str) -> Account {
    Account {
        id: id.to_owned(),
        name: email.split('@').next().unwrap_or(id).to_owned(),
        email: email.to_owned(),
        provider: "custom".to_owned(),
        signature: String::new(),
        created_at: 1_700_000_000_000,
    }
}

fn server(account_id: &str, port: u16) -> AccountServer {
    AccountServer {
        account_id: account_id.to_owned(),
        host: "localhost".to_owned(),
        port,
        tls: AccountTls::Implicit,
        username: format!("{account_id}@example.com"),
        created_at: 1_700_000_000_000,
        smtp: None,
    }
}

fn folder(account_id: &str, suffix: &str, name: &str, kind: FolderKind) -> Folder {
    Folder {
        id: format!("fld_{account_id}_{suffix}"),
        account_id: account_id.to_owned(),
        name: name.to_owned(),
        kind,
        unread: 0,
        total: 0,
        uid_validity: None,
        uid_next: None,
        last_sync_at: None,
    }
}

fn message(id: &str, account_id: &str, folder_id: &str) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: account_id.to_owned(),
            folder_id: folder_id.to_owned(),
            thread_id: format!("thread_{id}"),
            from: Address::new("Giulia", "giulia@example.com"),
            to: vec![Address::new("Me", "me@example.com")],
            subject: format!("subject {id}"),
            snippet: "body".to_owned(),
            date: 1_700_000_100_000,
            unread: true,
            starred: false,
            has_attachments: true,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "body".to_owned(),
        body_html: None,
        remote_images: 0,
        size: Some(4),
    }
}

fn attachment(id: &str) -> Attachment {
    Attachment { id: id.to_owned(), filename: "report.pdf".to_owned(), mime: "application/pdf".to_owned(), size: 4, blob_hash: None }
}

fn seed_two_accounts(store: &Store) {
    for id in [ACCOUNT_A, ACCOUNT_B] {
        store.upsert_account(&account(id, &format!("{id}@example.com"))).expect("account");
        store.upsert_account_server(&server(id, 993)).expect("server");
        let inbox = folder(id, "inbox", "INBOX", FolderKind::Inbox);
        let drafts = folder(id, "drafts", "Drafts", FolderKind::Drafts);
        store.upsert_folder(&inbox).expect("inbox");
        store.upsert_folder(&drafts).expect("drafts");

        let synced = message(&format!("msg_{id}"), id, &inbox.id);
        store.upsert_synced_message(&synced, &[attachment(&format!("att_{id}"))]).expect("synced message");
        store
            .save_draft(&DraftUpdate {
                id: format!("drf_{id}"),
                account_id: id.to_owned(),
                updated_at: 1_700_000_200_000,
                body_text: "bozza".to_owned(),
                to: Vec::new(),
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza".to_owned(),
                in_reply_to: None,
            })
            .expect("draft");
        store.set_meta(&format!("sync_status:{id}"), "{\"state\":\"idle\"}").expect("status");
    }
}

#[test]
fn server_config_round_trips_updates_and_deletes() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.upsert_account(&account(ACCOUNT_A, "a@example.com")).expect("account");

    assert_eq!(store.get_account_server(ACCOUNT_A).expect("read"), None);
    store.upsert_account_server(&server(ACCOUNT_A, 993)).expect("insert");
    let row = store.get_account_server(ACCOUNT_A).expect("read").expect("row");
    assert_eq!(row.host, "localhost");
    assert_eq!(row.port, 993);
    assert_eq!(row.tls, AccountTls::Implicit);
    assert_eq!(row.username, "acc_a@example.com");

    store
        .upsert_account_server(&AccountServer {
            host: "imap.example.com".to_owned(),
            port: 143,
            tls: AccountTls::StartTls,
            username: "me@example.com".to_owned(),
            ..server(ACCOUNT_A, 143)
        })
        .expect("update");
    let row = store.get_account_server(ACCOUNT_A).expect("read").expect("row");
    assert_eq!(row.host, "imap.example.com");
    assert_eq!(row.port, 143);
    assert_eq!(row.tls, AccountTls::StartTls);
    assert_eq!(row.username, "me@example.com");

    assert!(store.delete_account_server(ACCOUNT_A).expect("delete"));
    assert!(!store.delete_account_server(ACCOUNT_A).expect("delete again"));
    assert_eq!(store.get_account_server(ACCOUNT_A).expect("read"), None);
}

#[test]
fn save_account_writes_account_server_and_folders_together() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let folders = [folder(ACCOUNT_A, "inbox", "INBOX", FolderKind::Inbox), folder(ACCOUNT_A, "sent", "Sent", FolderKind::Sent)];
    // A server and a folder stamped with another account are corrected by the repository, so this
    // path cannot create a cross-account row.
    let mut mislabelled = folder(ACCOUNT_A, "trash", "Trash", FolderKind::Trash);
    mislabelled.account_id = ACCOUNT_B.to_owned();
    store
        .save_account(&account(ACCOUNT_A, "a@example.com"), &server(ACCOUNT_B, 993), &[folders[0].clone(), folders[1].clone(), mislabelled])
        .expect("save");

    assert!(store.get_account(ACCOUNT_A).expect("account").is_some());
    assert_eq!(store.get_account_server(ACCOUNT_A).expect("server").expect("row").port, 993);
    assert_eq!(store.get_account_server(ACCOUNT_B).expect("server"), None, "the server row was written for another account");
    let rows = store.list_folders(Some(ACCOUNT_A)).expect("folders");
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.account_id == ACCOUNT_A));
    assert!(rows.iter().any(|row| row.kind == FolderKind::Trash));
}

#[test]
fn delete_account_removes_its_cache_and_keeps_labels_and_other_accounts() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed_two_accounts(&store);
    let label = Label { id: "lbl_global".to_owned(), account_id: None, name: "Cliente".to_owned(), color: "gold".to_owned() };
    store.create_label(&label).expect("label");
    store.apply_label(&label.id, &[format!("msg_{ACCOUNT_A}"), format!("msg_{ACCOUNT_B}")]).expect("assignments");

    assert!(store.delete_account(ACCOUNT_A).expect("delete"));

    assert!(store.get_account(ACCOUNT_A).expect("account").is_none());
    assert_eq!(store.get_account_server(ACCOUNT_A).expect("server"), None);
    assert!(store.list_folders(Some(ACCOUNT_A)).expect("folders").is_empty());
    assert!(store.get_message(&format!("msg_{ACCOUNT_A}")).expect("message").is_none());
    assert!(!store.list_all_attachments().expect("attachments").contains_key(&format!("msg_{ACCOUNT_A}")));
    assert!(store.get_draft(&format!("drf_{ACCOUNT_A}")).expect("draft").is_none());
    assert_eq!(store.get_meta(&format!("sync_status:{ACCOUNT_A}")).expect("status"), None);

    // The other account is untouched, and the mailbox-wide label survives with both its counts and
    // its one remaining assignment.
    assert!(store.get_account(ACCOUNT_B).expect("account").is_some());
    assert_eq!(store.get_account_server(ACCOUNT_B).expect("server").expect("row").port, 993);
    assert_eq!(store.list_folders(Some(ACCOUNT_B)).expect("folders").len(), 2);
    assert!(store.get_message(&format!("msg_{ACCOUNT_B}")).expect("message").is_some());
    assert!(store.get_draft(&format!("drf_{ACCOUNT_B}")).expect("draft").is_some());
    assert_eq!(store.get_meta(&format!("sync_status:{ACCOUNT_B}")).expect("status").as_deref(), Some("{\"state\":\"idle\"}"));
    let labels = store.list_labels(None).expect("labels");
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].label.id, label.id);
    assert_eq!(labels[0].message_count, 1);
    assert_eq!(store.labels_for(&format!("msg_{ACCOUNT_B}")).expect("labels"), vec![label.id]);

    // The removed account's draft revision sequence is gone too; the other account's stays.
    let connection = rusqlite::Connection::open(fixture.path()).expect("raw connection");
    let hex = fixture.key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("key");
    let sequences: i64 = connection.query_row("SELECT COUNT(*) FROM draft_seq", [], |row| row.get(0)).expect("draft_seq count");
    assert_eq!(sequences, 1, "the removed account's draft sequence survived");
}

#[test]
fn deleting_an_unknown_account_changes_nothing() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed_two_accounts(&store);
    assert!(!store.delete_account("acc_missing").expect("delete"));
    assert_eq!(store.list_accounts().expect("accounts").len(), 2);
    assert_eq!(store.list_all_messages().expect("messages").len(), 2);
}

#[test]
fn a_read_only_store_refuses_writes_and_rejects_an_old_schema() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.upsert_account(&account(ACCOUNT_A, "a@example.com")).expect("seed");
    drop(store);

    let read_only = Store::open_read_only(&fixture.path(), &fixture.key).expect("read-only open");
    assert_eq!(read_only.schema_version().expect("version"), 7);
    assert_eq!(read_only.list_accounts().expect("read").len(), 1);
    let error = read_only.upsert_account(&account(ACCOUNT_B, "b@example.com")).expect_err("a write must fail on a read-only store");
    assert!(error.to_string().contains("readonly") || error.to_string().contains("read-only"), "{error}");
    assert_eq!(read_only.list_accounts().expect("read after the refused write").len(), 1);

    // A store that still needs a migration cannot be opened read-only: the caller must open it
    // read-write once rather than get a half-migrated reader.
    drop(read_only);
    {
        let connection = rusqlite::Connection::open(fixture.path()).expect("raw connection");
        let hex = fixture.key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\"; PRAGMA user_version = 3;")).expect("forge v3");
    }
    let error = Store::open_read_only(&fixture.path(), &fixture.key).err().expect("an old schema must be refused read-only");
    assert!(matches!(error, postino_store::StoreError::SchemaTooOld { found: 3, supported: 7 }), "{error}");
}

#[test]
fn the_schema_has_no_password_column() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let connection = rusqlite::Connection::open(fixture.path()).expect("raw open");
    let hex = fixture.key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\"; PRAGMA foreign_keys = ON;")).expect("key");
    let mut stmt = connection.prepare("SELECT name FROM pragma_table_info('account_servers') ORDER BY cid").expect("columns");
    let columns = stmt.query_map([], |row| row.get::<_, String>(0)).expect("names").collect::<rusqlite::Result<Vec<_>>>().expect("rows");
    assert_eq!(
        columns,
        vec![
            "account_id",
            "imap_host",
            "imap_port",
            "imap_tls",
            "imap_username",
            "created_at",
            "smtp_host",
            "smtp_port",
            "smtp_tls",
            "smtp_username"
        ]
    );
    assert!(!columns.iter().any(|column| column.contains("password") || column.contains("secret")));
    drop(stmt);
    drop(connection);
    drop(store);
}
