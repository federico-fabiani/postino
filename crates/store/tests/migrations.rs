//! Migration lifecycle: additive upgrade from v1, idempotent reopen, newer-schema refusal.

use std::path::Path;

use postino_store::{Account, Store, StoreError, StoreKey};
use rusqlite::Connection;

fn random_key() -> StoreKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    StoreKey::from_bytes(bytes)
}

/// A raw SQLCipher connection, used to build a v1 database and to forge a newer `user_version`.
fn raw_open(path: &Path, key: &StoreKey) -> Connection {
    let connection = Connection::open(path).expect("raw open");
    let hex = key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("apply the key");
    connection
}

fn account(id: &str) -> Account {
    Account {
        id: id.to_owned(),
        name: "Studio Verdi".to_owned(),
        email: "studio@example.com".to_owned(),
        provider: "demo".to_owned(),
        signature: "Cordiali saluti".to_owned(),
        created_at: 1_700_000_000_000,
    }
}

#[test]
fn upgrades_a_v1_database_additively() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    // A database exactly as the shipped v1 build left it: the meta table and user_version = 1.
    {
        let connection = raw_open(&path, &key);
        connection
            .execute_batch(
                "CREATE TABLE meta(
                     key   TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 ) WITHOUT ROWID;
                 PRAGMA user_version = 1;",
            )
            .expect("create v1");
        connection.execute("INSERT INTO meta(key, value) VALUES('schema', 'v1')", []).expect("seed v1");
    }

    {
        let store = Store::open(&path, &key).expect("upgrade v1 to v4");
        assert_eq!(store.schema_version().expect("version"), 4);
        assert_eq!(store.get_meta("schema").expect("meta").as_deref(), Some("v1"), "the upgrade lost meta data");
        store.upsert_account(&account("acc_1")).expect("the v2 tables exist");
        store.set_meta("schema", "v4").expect("write");
    }

    let store = Store::open(&path, &key).expect("reopen");
    assert_eq!(store.schema_version().expect("version"), 4);
    assert!(store.get_account("acc_1").expect("account").is_some());
    assert_eq!(store.get_meta("schema").expect("meta").as_deref(), Some("v4"));
}

#[test]
fn upgrades_a_v2_database_and_seeds_the_draft_sequence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();
    let draft = |body: &str| postino_store::DraftUpdate {
        id: "drf_1".to_owned(),
        account_id: "acc_1".to_owned(),
        updated_at: 1_700_000_000_000,
        body_text: body.to_owned(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Bozza v2".to_owned(),
        in_reply_to: None,
    };

    {
        let store = Store::open(&path, &key).expect("open v4");
        store.upsert_account(&account("acc_1")).expect("account");
        store.save_draft(&draft("prima")).expect("draft");
        store.save_draft(&draft("seconda")).expect("second revision");
        // Make the file look like the shipped v2 schema: the migration had not created draft_seq
        // or the v4 server table.
        let connection = raw_open(&path, &key);
        connection.execute_batch("DROP TABLE draft_seq; DROP TABLE account_servers; PRAGMA user_version = 2;").expect("forge v2");
    }

    {
        let store = Store::open(&path, &key).expect("upgrade v2 to v4");
        assert_eq!(store.schema_version().expect("version"), 4);
        let saved = store.save_draft(&draft("terza")).expect("the seeded sequence must continue");
        assert_eq!(saved.revision, 3, "the v2 draft revision did not seed draft_seq");
    }
}

#[test]
fn upgrades_a_v3_database_and_keeps_its_mail() {
    use postino_store::{AccountServer, AccountTls, Address, Folder, FolderKind, Message, MessageSummary};

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();
    let folder = Folder {
        id: "fld_inbox".to_owned(),
        account_id: "acc_1".to_owned(),
        name: "INBOX".to_owned(),
        kind: FolderKind::Inbox,
        unread: 1,
        total: 1,
        uid_validity: Some(10),
        uid_next: Some(11),
        last_sync_at: None,
    };
    let message = Message {
        summary: MessageSummary {
            id: "msg_1".to_owned(),
            account_id: "acc_1".to_owned(),
            folder_id: "fld_inbox".to_owned(),
            thread_id: "thread_1".to_owned(),
            from: Address::new("Giulia", "giulia@example.com"),
            to: vec![Address::new("Me", "me@example.com")],
            subject: "Prima della v4".to_owned(),
            snippet: "corpo".to_owned(),
            date: 1_700_000_100_000,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some("<one@example.com>".to_owned()),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "corpo".to_owned(),
        body_html: None,
        size: Some(5),
    };

    {
        let store = Store::open(&path, &key).expect("open v4");
        store.upsert_account(&account("acc_1")).expect("account");
        store.upsert_folder(&folder).expect("folder");
        store.upsert_synced_message(&message, &[]).expect("message");
        // Make the file look like the shipped v3 schema: the v4 table did not exist yet.
        let connection = raw_open(&path, &key);
        connection.execute_batch("DROP TABLE account_servers; PRAGMA user_version = 3;").expect("forge v3");
    }

    {
        let store = Store::open(&path, &key).expect("upgrade v3 to v4");
        assert_eq!(store.schema_version().expect("version"), 4);
        assert_eq!(store.list_all_messages().expect("messages").len(), 1, "the upgrade lost mail");
        assert_eq!(store.get_folder("fld_inbox").expect("folder").expect("row").total, 1);
        assert_eq!(store.get_account_server("acc_1").expect("server"), None, "a v3 store starts with no server row");
        store
            .upsert_account_server(&AccountServer {
                account_id: "acc_1".to_owned(),
                host: "imaps.example.com".to_owned(),
                port: 993,
                tls: AccountTls::Implicit,
                username: "me@example.com".to_owned(),
                created_at: 1_700_000_000_000,
            })
            .expect("the v4 table accepts a server");
    }
}

#[test]
fn reopening_does_not_run_a_migration_twice() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    for round in 0..3 {
        let store = Store::open(&path, &key).unwrap_or_else(|error| panic!("open round {round}: {error}"));
        assert_eq!(store.schema_version().expect("version"), 4);
        store.set_meta("rounds", &round.to_string()).expect("write");
        // Dropping the store closes the connection and checkpoints the WAL.
    }

    let store = Store::open(&path, &key).expect("final open");
    assert_eq!(store.get_meta("rounds").expect("read").as_deref(), Some("2"));
}

#[test]
fn a_newer_schema_is_refused_with_its_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    {
        let store = Store::open(&path, &key).expect("open v4");
        store.set_meta("canary", "still here").expect("write");
    }
    {
        let connection = raw_open(&path, &key);
        connection.pragma_update(None, "user_version", 99).expect("forge a newer version");
    }

    let error = Store::open(&path, &key).err().expect("a newer schema must not be opened");
    assert!(matches!(error, StoreError::SchemaTooNew { found: 99, supported: 4 }), "{error}");
}

#[test]
fn foreign_keys_are_enforced_and_cascade() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();
    let store = Store::open(&path, &key).expect("open");

    // A folder without its account is rejected by the foreign key, not silently written.
    let orphan = postino_store::Folder {
        id: "orphan".to_owned(),
        account_id: "missing_account".to_owned(),
        name: "Orfana".to_owned(),
        kind: postino_store::FolderKind::Custom,
        unread: 0,
        total: 0,
        uid_validity: None,
        uid_next: None,
        last_sync_at: None,
    };
    let error = store.upsert_folder(&orphan).expect_err("an orphan folder must be refused");
    assert!(error.to_string().contains("FOREIGN KEY"), "{error}");

    store.upsert_account(&account("acc_1")).expect("account");
    store.upsert_folder(&postino_store::Folder { account_id: "acc_1".to_owned(), ..orphan }).expect("folder");
    drop(store);

    // Deleting the account cascades to folders, messages, attachments, assignments and drafts.
    {
        let connection = raw_open(&path, &key);
        connection.execute("DELETE FROM accounts WHERE id = 'acc_1'", []).expect("delete the account");
    }
    let store = Store::open(&path, &key).expect("reopen");
    assert!(store.list_folders(None).expect("folders").is_empty(), "the folder survived its account");
}
