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

/// Remove every v7 object from a current store so a test can forge the schema a shipped build
/// left behind. The order matters: the FTS triggers reference the virtual table.
const DROP_V7: &str = "
DROP TRIGGER IF EXISTS messages_fts_insert;
DROP TRIGGER IF EXISTS messages_fts_delete;
DROP TRIGGER IF EXISTS messages_fts_update;
DROP TABLE IF EXISTS message_fts;
DROP TABLE IF EXISTS message_fts_rank;
DROP INDEX IF EXISTS messages_account_thread_date;
DROP INDEX IF EXISTS messages_account_message_id;
DROP INDEX IF EXISTS messages_account_subject_norm_date;
DROP INDEX IF EXISTS messages_list_cover;
DROP INDEX IF EXISTS messages_rules_new;
DROP INDEX IF EXISTS messages_rules_all;
DROP TABLE IF EXISTS thread_aliases;
DROP TABLE IF EXISTS threads;
DROP TABLE IF EXISTS saved_searches;
DROP TABLE IF EXISTS rules;
DROP TABLE IF EXISTS rule_runs;
DROP TABLE IF EXISTS rule_actions;
ALTER TABLE messages DROP COLUMN search_sender;
ALTER TABLE messages DROP COLUMN search_to;
ALTER TABLE messages DROP COLUMN search_cc;
ALTER TABLE messages DROP COLUMN search_bcc;
ALTER TABLE messages DROP COLUMN attachment_names;
ALTER TABLE messages DROP COLUMN subject_norm;
ALTER TABLE messages DROP COLUMN in_reply_to;
ALTER TABLE messages DROP COLUMN references_json;
ALTER TABLE messages DROP COLUMN ingested_at;
ALTER TABLE messages DROP COLUMN rules_applied_at;
ALTER TABLE messages DROP COLUMN origin;
DELETE FROM meta WHERE key = 'search_index';
DELETE FROM meta WHERE key = 'search_updated_at';
DELETE FROM meta WHERE key = 'message_origin_backfilled';
";

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
        let store = Store::open(&path, &key).expect("upgrade v1 to v6");
        assert_eq!(store.schema_version().expect("version"), 7);
        assert_eq!(store.get_meta("schema").expect("meta").as_deref(), Some("v1"), "the upgrade lost meta data");
        store.upsert_account(&account("acc_1")).expect("the v2 tables exist");
        store.set_meta("schema", "v4").expect("write");
    }

    let store = Store::open(&path, &key).expect("reopen");
    assert_eq!(store.schema_version().expect("version"), 7);
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
        let store = Store::open(&path, &key).expect("open v5");
        store.upsert_account(&account("acc_1")).expect("account");
        store.save_draft(&draft("prima")).expect("draft");
        store.save_draft(&draft("seconda")).expect("second revision");
        // Make the file look like the shipped v2 schema: the migration had not created draft_seq,
        // the v4 server table, the v5 outbox or the v6 remote-image column.
        let connection = raw_open(&path, &key);
        connection.execute_batch(DROP_V7).expect("drop v7 objects");
        connection
            .execute_batch(
                "DROP TABLE draft_seq; DROP TABLE account_servers; DROP TABLE outbox;
                 ALTER TABLE messages DROP COLUMN remote_images; PRAGMA user_version = 2;",
            )
            .expect("forge v2");
    }

    {
        let store = Store::open(&path, &key).expect("upgrade v2 to v6");
        assert_eq!(store.schema_version().expect("version"), 7);
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
        remote_images: 0,
        size: Some(5),
    };

    {
        let store = Store::open(&path, &key).expect("open v6");
        store.upsert_account(&account("acc_1")).expect("account");
        store.upsert_folder(&folder).expect("folder");
        store.upsert_synced_message(&message, &[]).expect("message");
        // Make the file look like the shipped v3 schema: the v4 table and the v6 column did not exist.
        let connection = raw_open(&path, &key);
        connection.execute_batch(DROP_V7).expect("drop v7 objects");
        connection
            .execute_batch(
                "DROP TABLE account_servers; DROP TABLE outbox;
                 ALTER TABLE messages DROP COLUMN remote_images; PRAGMA user_version = 3;",
            )
            .expect("forge v3");
    }

    {
        let store = Store::open(&path, &key).expect("upgrade v3 to v6");
        assert_eq!(store.schema_version().expect("version"), 7);
        let rows = store.list_all_messages().expect("messages");
        assert_eq!(rows.len(), 1, "the upgrade lost mail");
        assert_eq!(rows[0].remote_images, 0, "a v3 row did not default the remote-image count to 0");
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
                smtp: None,
            })
            .expect("the v4 table accepts a server");
    }
}

#[test]
fn upgrades_a_v5_database_and_defaults_every_remote_count_to_zero() {
    use postino_store::{Address, Folder, FolderKind, Message, MessageSummary};

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();
    let folder = Folder {
        id: "fld_inbox".to_owned(),
        account_id: "acc_1".to_owned(),
        name: "INBOX".to_owned(),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 2,
        uid_validity: Some(10),
        uid_next: Some(12),
        last_sync_at: None,
    };
    let message = |id: &str, uid: u32| Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: "acc_1".to_owned(),
            folder_id: "fld_inbox".to_owned(),
            thread_id: format!("thread_{id}"),
            from: Address::new("Giulia", "giulia@example.com"),
            to: vec![Address::new("Me", "me@example.com")],
            subject: format!("Prima della v6 {id}"),
            snippet: "corpo".to_owned(),
            date: 1_700_000_100_000,
            unread: false,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(uid),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "corpo".to_owned(),
        body_html: Some("<p>corpo</p>".to_owned()),
        remote_images: 0,
        size: Some(5),
    };

    {
        let store = Store::open(&path, &key).expect("open v6");
        store.upsert_account(&account("acc_1")).expect("account");
        store.upsert_folder(&folder).expect("folder");
        store.upsert_synced_message(&message("msg_1", 1), &[]).expect("first message");
        store.upsert_synced_message(&message("msg_2", 2), &[]).expect("second message");
        // Forge the shipped v5 shape: the v6 column did not exist yet, every other table does.
        let connection = raw_open(&path, &key);
        connection.execute_batch(DROP_V7).expect("drop v7 objects");
        connection.execute_batch("ALTER TABLE messages DROP COLUMN remote_images; PRAGMA user_version = 5;").expect("forge v5");
    }

    let store = Store::open(&path, &key).expect("upgrade v5 to v6");
    assert_eq!(store.schema_version().expect("version"), 7);
    let rows = store.list_all_messages().expect("messages");
    assert_eq!(rows.len(), 2, "the upgrade lost mail");
    assert!(rows.iter().all(|row| row.remote_images == 0), "a v5 row did not default the count to 0: {rows:?}");
    assert!(rows.iter().all(|row| row.body_text == "corpo" && row.body_html.is_some()), "the upgrade lost a body");
    drop(store);

    // Reopening a migrated store keeps the zero counts and does not add the column again.
    let reopened = Store::open(&path, &key).expect("reopen v6");
    assert_eq!(reopened.schema_version().expect("version"), 7);
    assert!(reopened.list_all_messages().expect("messages").iter().all(|row| row.remote_images == 0));
}

#[test]
fn a_failed_v6_migration_rolls_back_and_keeps_the_v5_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    {
        let store = Store::open(&path, &key).expect("open v6");
        store.upsert_account(&account("acc_1")).expect("account");
    }
    {
        // A v5 file that already has an unrelated `remote_images` column makes the additive v6
        // statement fail. The migration transaction must roll back: the version stays 5 and the
        // account row survives.
        let connection = raw_open(&path, &key);
        connection.execute_batch(DROP_V7).expect("drop v7 objects");
        connection
            .execute_batch(
                "ALTER TABLE messages DROP COLUMN remote_images;
                 ALTER TABLE messages ADD COLUMN remote_images TEXT;
                 PRAGMA user_version = 5;",
            )
            .expect("forge the failing v5 fixture");
    }

    let error = Store::open(&path, &key).err().expect("the duplicate column must abort the v6 migration");
    assert!(error.to_string().contains("duplicate column"), "{error}");

    let connection = raw_open(&path, &key);
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0)).expect("version");
    assert_eq!(version, 5, "the failed v6 transaction advanced the schema version");
    let accounts: u32 = connection.query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0)).expect("accounts");
    assert_eq!(accounts, 1, "the failed migration rolled back more than itself");
    let column_type: String = connection
        .query_row("SELECT type FROM pragma_table_info('messages') WHERE name = 'remote_images'", [], |row| row.get(0))
        .expect("the forged column");
    assert_eq!(column_type, "TEXT", "the failed migration left a partial v6 column behind");
}

#[test]
fn the_v5_outbox_columns_arrive_with_the_migration() {
    use postino_store::{OutboxEntry, OutboxState};

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();
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
    }
    let store = Store::open(&path, &key).expect("v1 to the current schema");
    store.upsert_account(&account("acc_1")).expect("account");
    let row = |id: &str, key: &str| OutboxEntry {
        id: id.to_owned(),
        account_id: "acc_1".to_owned(),
        draft_id: Some("draft_1".to_owned()),
        idempotency_key: key.to_owned(),
        created_at: 1_000,
        send_at: 1_000,
        state: OutboxState::Queued,
        attempts: 0,
        append_attempts: 0,
        last_error: None,
        raw_blob_hash: "a".repeat(64),
        envelope_json: "{}".to_owned(),
        lease_at: None,
    };
    store.queue_outbox(&row("ob_1", "draft:1:hash")).expect("the migrated table takes a row with a draft id");
    let duplicate = store.queue_outbox(&row("ob_2", "draft:1:other")).expect_err("the active-draft index arrived with the migration");
    assert!(matches!(duplicate, StoreError::OutboxAlreadyActive { .. }), "{duplicate}");
    let leased = store.lease_next_outbox(1_000).expect("lease").expect("row");
    assert_eq!(leased.draft_id.as_deref(), Some("draft_1"));
    assert_eq!(leased.lease_at, Some(1_000), "the lease epoch column arrived with the migration");
}

#[test]
fn upgrades_a_v6_database_to_v7_with_the_search_columns_and_index() {
    use postino_store::{Address, Folder, FolderKind, Message, MessageOrigin, MessageSummary, SearchExpr, SearchFilter};

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();
    let folder = Folder {
        id: "fld_inbox".to_owned(),
        account_id: "acc_1".to_owned(),
        name: "INBOX".to_owned(),
        kind: FolderKind::Inbox,
        unread: 0,
        total: 1,
        uid_validity: Some(1),
        uid_next: Some(2),
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
            subject: "Verbale riunione".to_owned(),
            snippet: "Il verbale".to_owned(),
            date: 1_700_000_100_000,
            unread: false,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some("<one@example.com>".to_owned()),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "Il verbale della riunione".to_owned(),
        body_html: None,
        remote_images: 0,
        size: Some(32),
    };
    let sent_folder = Folder {
        id: "fld_sent".to_owned(),
        account_id: "acc_1".to_owned(),
        name: "Sent".to_owned(),
        kind: FolderKind::Sent,
        unread: 0,
        total: 1,
        uid_validity: Some(1),
        uid_next: Some(2),
        last_sync_at: None,
    };
    let mut sent = message.clone();
    sent.summary.id = "msg_sent".to_owned();
    sent.summary.folder_id = "fld_sent".to_owned();
    sent.summary.subject = "Lettera inviata".to_owned();
    sent.summary.snippet = "Una lettera".to_owned();
    sent.body_text = "Una lettera inviata".to_owned();
    sent.uid = Some(1);
    sent.message_id = Some("<sent@example.com>".to_owned());
    {
        let store = Store::open(&path, &key).expect("open v7");
        store.upsert_account(&account("acc_1")).expect("account");
        store.upsert_folder(&folder).expect("folder");
        store.upsert_folder(&sent_folder).expect("sent folder");
        store.upsert_synced_message(&message, &[]).expect("message");
        store.upsert_synced_message(&sent, &[]).expect("sent message");
        let connection = raw_open(&path, &key);
        connection.execute_batch(DROP_V7).expect("forge v6 objects");
        connection.execute_batch("PRAGMA user_version = 6;").expect("forge v6 version");
    }

    let store = Store::open(&path, &key).expect("upgrade v6 to v7");
    assert_eq!(store.schema_version().expect("version"), 7);
    let pending = store.search_status().expect("pending status");
    assert!(!pending.ready, "the migrated v7 index must start pending");
    assert_eq!(pending.total, 2);
    store.ensure_search_index().expect("build the pending v7 index");
    let status = store.search_status().expect("search status");
    assert!(status.ready, "the v7 index was not built on open: {status:?}");
    assert_eq!((status.indexed, status.total), (2, 2));
    assert!(store.get_meta("search_index").expect("meta").is_some(), "v7 did not write meta.search_index");
    let page = store.search(&SearchFilter { expr: Some(SearchExpr::Match("\"verbale\"".to_owned())), ..SearchFilter::default() }).expect("search");
    assert_eq!(page.total, 1);
    assert_eq!(page.hits[0].message.id, "msg_1");
    assert!(page.hits[0].body_text.contains("verbale"));
    assert_eq!(store.message_origin("msg_1").expect("inbox origin"), Some(MessageOrigin::Received));
    assert_eq!(store.message_origin("msg_sent").expect("sent origin"), Some(MessageOrigin::Sent), "a v6 Sent copy must be classified");
    drop(store);

    let connection = raw_open(&path, &key);
    for column in [
        "search_sender",
        "search_to",
        "search_cc",
        "search_bcc",
        "attachment_names",
        "subject_norm",
        "in_reply_to",
        "references_json",
        "ingested_at",
        "rules_applied_at",
    ] {
        let found: u32 = connection
            .query_row("SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = ?1", [column], |row| row.get(0))
            .expect("table info");
        assert_eq!(found, 1, "v7 did not add messages.{column}");
    }
    // The inert P9b/P9c tables exist and are empty.
    for table in ["threads", "thread_aliases", "saved_searches", "rules", "rule_runs", "rule_actions"] {
        let count: u32 = connection
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |row| row.get(0))
            .expect("sqlite_master");
        assert_eq!(count, 1, "v7 did not create the inert table {table}");
        let rows: u32 = connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).expect("count");
        assert_eq!(rows, 0, "the inert table {table} must start empty");
    }
    let cover: u32 = connection
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'messages_list_cover'", [], |row| row.get(0))
        .expect("cover index");
    assert_eq!(cover, 1, "v7 did not create the covering list index");
    let rank: u32 = connection
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'message_fts_rank'", [], |row| row.get(0))
        .expect("rank table");
    assert_eq!(rank, 1, "v7 did not create the FTS rank table");
}

#[test]
fn a_failed_v7_migration_rolls_back_and_keeps_v6() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();
    {
        let store = Store::open(&path, &key).expect("open v7");
        store.upsert_account(&account("acc_1")).expect("account");
        let connection = raw_open(&path, &key);
        connection.execute_batch(DROP_V7).expect("forge v6 objects");
        // One v7 column already exists, so the additive v7 migration must fail and roll back.
        connection
            .execute_batch("ALTER TABLE messages ADD COLUMN search_sender TEXT; PRAGMA user_version = 6;")
            .expect("forge the failing v6 fixture");
    }
    let error = Store::open(&path, &key).err().expect("the duplicate column must abort the v7 migration");
    assert!(error.to_string().contains("duplicate column"), "{error}");
    let connection = raw_open(&path, &key);
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0)).expect("version");
    assert_eq!(version, 6, "the failed v7 transaction advanced the schema version");
    let accounts: u32 = connection.query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0)).expect("accounts");
    assert_eq!(accounts, 1, "the failed migration rolled back more than itself");
    let fts: u32 = connection
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'message_fts'", [], |row| row.get(0))
        .expect("fts table");
    assert_eq!(fts, 0, "the failed v7 migration left the FTS table behind");
}

#[test]
fn reopening_does_not_run_a_migration_twice() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    for round in 0..3 {
        let store = Store::open(&path, &key).unwrap_or_else(|error| panic!("open round {round}: {error}"));
        assert_eq!(store.schema_version().expect("version"), 7);
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
        let store = Store::open(&path, &key).expect("open v6");
        store.set_meta("canary", "still here").expect("write");
    }
    {
        let connection = raw_open(&path, &key);
        connection.pragma_update(None, "user_version", 99).expect("forge a newer version");
    }

    let error = Store::open(&path, &key).err().expect("a newer schema must not be opened");
    assert!(matches!(error, StoreError::SchemaTooNew { found: 99, supported: 7 }), "{error}");
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
