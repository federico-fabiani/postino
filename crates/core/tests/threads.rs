//! P9b end-to-end operations: conversation rows, thread flags, aliases and saved searches.
//!
//! The store-backed tests go through `Engine::open` and the operation registry, so the assertions
//! cover the JSON shapes the CLI and MCP return, not only the repository.

use std::path::PathBuf;

use postino_core::{Engine, OpError};
use postino_store::{Account, Address, Folder, FolderKind, Message, MessageSummary, Store, StoreKey};
use serde_json::json;

const ACCOUNT: &str = "acc_p9b";
const INBOX: &str = "fld_p9b_inbox";
const ARCHIVE: &str = "fld_p9b_archive";
const T0: i64 = 1_700_000_000_000;

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

    fn engine(&self) -> Engine {
        Engine::open(self.open()).expect("engine")
    }
}

fn seed(store: &Store) {
    store
        .upsert_account(&Account {
            id: ACCOUNT.to_owned(),
            name: "Studio".to_owned(),
            email: "studio@example.com".to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
            created_at: T0,
        })
        .expect("account");
    for (id, name, kind) in [(INBOX, "INBOX", FolderKind::Inbox), (ARCHIVE, "Archive", FolderKind::Archive)] {
        store
            .upsert_folder(&Folder {
                id: id.to_owned(),
                account_id: ACCOUNT.to_owned(),
                name: name.to_owned(),
                kind,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: Some(1),
                last_sync_at: None,
            })
            .expect("folder");
    }
}

fn message(id: &str, folder: &str, message_id: &str, subject: &str, date: i64) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: ACCOUNT.to_owned(),
            folder_id: folder.to_owned(),
            thread_id: format!("provisional_{id}"),
            from: Address::new("Giulia Ferri", "giulia@example.com"),
            to: vec![Address::new("Federico", "federico@example.it")],
            subject: subject.to_owned(),
            snippet: format!("anteprima {id}"),
            date,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(message_id.to_owned()),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: format!("corpo {id}"),
        body_html: None,
        remote_images: 0,
        size: Some(128),
    }
}

fn insert(store: &Store, row: &Message, parent: Option<&str>, references: &[&str]) {
    let refs = references.iter().map(|reference| (*reference).to_owned()).collect::<Vec<_>>();
    store.upsert_synced_message_with_refs(row, &[], parent, &refs).expect("message");
}

fn threaded_fixture() -> (Fixture, Engine) {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let mut root = message("msg_root", INBOX, "<root@example.com>", "Preventivo sito", T0);
    root.summary.starred = true;
    let attachment = postino_store::Attachment {
        id: "att_root".to_owned(),
        filename: "preventivo.pdf".to_owned(),
        mime: "application/pdf".to_owned(),
        size: 1024,
        blob_hash: None,
    };
    store.upsert_synced_message_with_refs(&root, std::slice::from_ref(&attachment), None, &[]).expect("root with attachment");
    let reply = message("msg_reply", INBOX, "<reply@example.com>", "Re: Preventivo sito", T0 + 3_600_000);
    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);
    let archived = message("msg_archived", ARCHIVE, "<archived@example.com>", "Re: Preventivo sito", T0 + 7_200_000);
    insert(&store, &archived, Some("root@example.com"), &["<root@example.com>", "<reply@example.com>"]);
    drop(store);
    let engine = fixture.engine();
    (fixture, engine)
}

#[test]
fn threads_list_and_thread_get_return_the_documented_shape() {
    let (_fixture, mut engine) = threaded_fixture();
    let listed = engine.call("threads_list", json!({ "folder": INBOX })).expect("threads");
    assert_eq!(listed["total"], 1, "{listed}");
    let thread = &listed["threads"][0];
    assert_eq!(thread["id"], "root@example.com");
    assert_eq!(thread["message_count"], 3, "counts are mailbox-wide");
    assert_eq!(thread["unread_count"], 3);
    assert_eq!(thread["state"], "unread");
    assert_eq!(thread["participants"][0]["email"], "giulia@example.com");
    assert!(thread["snippet"].as_str().is_some_and(|snippet| !snippet.is_empty()));
    assert_eq!(listed["unread_threads"], 1);
    assert_eq!(listed["unread_messages"], 3);

    let detail = engine.call("thread_get", json!({ "id": "root@example.com" })).expect("thread");
    assert_eq!(detail["total"], 3);
    assert_eq!(detail["truncated"], false);
    assert_eq!(detail["resolved_id"], "root@example.com");
    let messages = detail["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert!(messages.windows(2).all(|pair| pair[0]["date"].as_str() <= pair[1]["date"].as_str()), "messages must ascend");
    assert_eq!(messages[0]["id"], "msg_root");
    assert_eq!(detail["thread"]["subject"], "Preventivo sito", "the earliest subject names the conversation");

    let paged = engine.call("thread_get", json!({ "id": "root@example.com", "limit": 2 })).expect("page");
    assert_eq!(paged["messages"].as_array().map(Vec::len), Some(2));
    assert_eq!(paged["truncated"], true);
    assert_eq!(paged["total"], 3);
}

#[test]
fn thread_flag_changes_every_message_once_and_resolves_an_old_id() {
    let (_fixture, mut engine) = threaded_fixture();
    let first = engine.call("thread_flag", json!({ "id": "root@example.com", "unread": false })).expect("flag");
    assert_eq!(first["changed"], 3);
    assert_eq!(first["thread"]["unread_count"], 0);
    assert_eq!(first["thread"]["state"], "read");
    let again = engine.call("thread_flag", json!({ "id": "root@example.com", "unread": false })).expect("idempotent");
    assert_eq!(again["changed"], 0);
    engine.call("thread_flag", json!({ "id": "root@example.com", "starred": true })).expect("star");
    let listed = engine.call("threads_list", json!({ "folder": INBOX })).expect("threads");
    assert_eq!(listed["threads"][0]["starred_count"], 3);
    let named = engine.call("thread_flag", json!({ "id": "root@example.com" })).expect_err("a flag is required");
    assert!(matches!(named, OpError::InvalidArgs(_)));
    let unknown = engine.call("thread_get", json!({ "id": "missing@example.com" })).expect_err("unknown thread");
    assert!(matches!(unknown, OpError::InvalidArgs(_)));
}

#[test]
fn a_merged_thread_resolves_the_old_id_through_the_operation() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let alpha = message("msg_alpha", INBOX, "<alpha@example.com>", "Alfa", T0);
    insert(&store, &alpha, None, &[]);
    let zulu = message("msg_zulu", INBOX, "<zulu@example.com>", "Zulu", T0 + 1000);
    insert(&store, &zulu, None, &[]);
    let bridge = message("msg_bridge", INBOX, "<bridge@example.com>", "Ponte", T0 + 2000);
    insert(&store, &bridge, None, &["<zulu@example.com>", "<alpha@example.com>"]);
    drop(store);
    let mut engine = fixture.engine();

    let old = engine.call("thread_get", json!({ "id": "zulu@example.com" })).expect("old id");
    assert_eq!(old["resolved_id"], "alpha@example.com");
    assert_eq!(old["thread"]["id"], "alpha@example.com");
    assert_eq!(old["total"], 3);
    let flagged = engine.call("thread_flag", json!({ "id": "zulu@example.com", "starred": true })).expect("old id flag");
    assert_eq!(flagged["changed"], 3, "an alias flags the current thread, not a deleted row");
}

#[test]
fn thread_scope_filters_keep_mailbox_wide_counts() {
    let (_fixture, mut engine) = threaded_fixture();
    let archive = engine.call("threads_list", json!({ "folder": ARCHIVE })).expect("archive");
    assert_eq!(archive["total"], 1);
    assert_eq!(archive["threads"][0]["message_count"], 3);
    assert_eq!(archive["threads"][0]["unread_count"], 3);

    engine.call("thread_flag", json!({ "id": "root@example.com", "unread": false })).expect("read all");
    let unread = engine.call("threads_list", json!({ "folder": INBOX, "unread_only": true })).expect("unread");
    assert_eq!(unread["total"], 0);
    let read = engine.call("threads_list", json!({ "folder": INBOX, "starred_only": false })).expect("read");
    assert_eq!(read["total"], 1);

    let attachments = engine.call("threads_list", json!({ "folder": INBOX, "with_attachments": true })).expect("attachments");
    assert_eq!(attachments["total"], 1, "the thread with the PDF is the only attachment thread");
    let starred = engine.call("threads_list", json!({ "folder": INBOX, "starred_only": true })).expect("starred");
    assert_eq!(starred["total"], 1, "one message in the thread is starred from the fixture");
}

#[test]
fn saving_a_search_computes_counts_and_a_deleted_label_makes_it_stale() {
    let mut engine = Engine::demo();
    let created = engine.call("saved_search_create", json!({ "name": "Fatture", "query": "label:Fatture", "sort": "date" })).expect("create");
    let id = created["search"]["id"].as_str().expect("id").to_owned();
    assert_eq!(created["search"]["valid"], true, "{created}");
    assert!(created["search"]["total"].as_u64().unwrap_or(0) > 0);

    let duplicate = engine.call("saved_search_create", json!({ "name": "fatture", "query": "is:unread" })).expect_err("case-insensitive duplicate");
    assert!(matches!(duplicate, OpError::InvalidArgs(_)));

    let listed = engine.call("saved_searches_list", json!({})).expect("list");
    assert_eq!(listed["searches"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["searches"][0]["sort"], "date");

    engine.call("label_delete", json!({ "id": "lbl_3" })).expect("delete the label");
    let stale = engine.call("saved_searches_list", json!({})).expect("list");
    assert_eq!(stale["searches"][0]["valid"], false, "{stale}");
    assert!(stale["searches"][0]["error"].as_str().is_some_and(|error| error.contains("unknown label")));
    assert_eq!(stale["searches"][0]["total"], 0);

    let updated = engine.call("saved_search_update", json!({ "id": id, "name": "Senza etichetta", "query": "is:unread" })).expect("update");
    assert_eq!(updated["search"]["valid"], true);
    assert_eq!(updated["search"]["name"], "Senza etichetta");
    let deleted = engine.call("saved_search_delete", json!({ "id": id })).expect("delete");
    assert_eq!(deleted["search"]["id"], id);
    assert_eq!(engine.call("saved_searches_list", json!({})).expect("list")["searches"].as_array().map(Vec::len), Some(0));
    assert!(matches!(engine.call("saved_search_delete", json!({ "id": id })).expect_err("gone"), OpError::InvalidArgs(_)));
}

#[test]
fn the_saved_search_caps_and_validation_are_enforced() {
    let mut engine = Engine::demo();
    for index in 0..12 {
        engine.call("saved_search_create", json!({ "name": format!("Ricerca {index}"), "query": "is:unread" })).expect("create");
    }
    let refused = engine.call("saved_search_create", json!({ "name": "Tredici", "query": "is:unread" })).expect_err("cap");
    assert!(matches!(refused, OpError::InvalidArgs(ref detail) if detail.contains("12")));

    for (args, reason) in [
        (json!({ "name": "", "query": "is:unread" }), "empty name"),
        (json!({ "name": "x".repeat(49), "query": "is:unread" }), "long name"),
        (json!({ "name": "Bidi\u{202e}", "query": "is:unread" }), "bidi name"),
        (json!({ "name": "Vuota", "query": "   " }), "empty query"),
        (json!({ "name": "Rotta", "query": "subject:\"unclosed" }), "unclosed query"),
        (json!({ "name": "Etichetta", "query": "label:Inesistente" }), "unknown label"),
        (json!({ "name": "Sort", "query": "is:unread", "sort": "random" }), "bad sort"),
    ] {
        let error = engine.call("saved_search_create", args).expect_err(reason);
        assert!(matches!(error, OpError::InvalidArgs(_)), "{reason}: {error:?}");
    }
}

#[test]
fn recent_searches_keep_the_last_eight_and_clear() {
    let mut engine = Engine::demo();
    for index in 0..10 {
        engine.record_recent_search(None, &format!("oggetto{index}")).expect("record");
    }
    let recent = engine.recent_searches(None).expect("recent");
    assert_eq!(recent.len(), 8);
    assert_eq!(recent[0], "oggetto9", "newest first");
    assert_eq!(recent[7], "oggetto2");
    engine.record_recent_search(None, "oggetto5").expect("dedupe");
    let recent = engine.recent_searches(None).expect("recent");
    assert_eq!(recent[0], "oggetto5");
    assert_eq!(recent.iter().filter(|query| query.as_str() == "oggetto5").count(), 1);
    engine.record_recent_search(None, "subject:\"unclosed").expect("an invalid query is not recorded");
    assert!(!engine.recent_searches(None).expect("recent").iter().any(|query| query.contains("unclosed")));
    assert!(engine.clear_recent_searches(None).expect("clear"));
    assert!(engine.recent_searches(None).expect("recent").is_empty());
}

#[test]
fn saved_searches_and_recent_queries_survive_a_store_reopen() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let root = message("msg_root", INBOX, "<root@example.com>", "Conteggi", T0);
    insert(&store, &root, None, &[]);
    let reply = message("msg_reply", INBOX, "<reply@example.com>", "Re: Conteggi", T0 + 1000);
    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);
    drop(store);
    let mut engine = fixture.engine();
    engine.call("search_reindex", json!({})).expect("build the index for the count");
    let created = engine.call("saved_search_create", json!({ "name": "Non letti", "query": "is:unread" })).expect("create");
    let id = created["search"]["id"].as_str().expect("id").to_owned();
    engine.record_recent_search(None, "is:unread").expect("recent");
    drop(engine);

    let mut reopened = fixture.engine();
    let listed = reopened.call("saved_searches_list", json!({})).expect("list");
    assert_eq!(listed["searches"][0]["id"], id);
    assert_eq!(listed["searches"][0]["total"], 2, "counts are recomputed after the reopen");
    assert_eq!(listed["recent"][0], "is:unread");

    reopened.call("saved_search_delete", json!({ "id": id })).expect("delete");
    reopened.call("saved_search_recent_clear", json!({})).expect("clear");
    drop(reopened);
    let mut third = fixture.engine();
    assert_eq!(third.call("saved_searches_list", json!({})).expect("list")["searches"].as_array().map(Vec::len), Some(0));
    assert!(third.recent_searches(None).expect("recent").is_empty());
}

#[test]
fn a_store_saved_search_outlives_its_label_and_is_marked_invalid() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store
        .create_label(&postino_store::Label { id: "lbl_x".to_owned(), account_id: None, name: "Lavoro".to_owned(), color: "gold".to_owned() })
        .expect("label");
    drop(store);
    let mut engine = fixture.engine();
    engine.call("saved_search_create", json!({ "name": "Lavoro", "query": "label:Lavoro" })).expect("create");
    engine.call("label_delete", json!({ "id": "lbl_x" })).expect("delete label");
    let listed = engine.call("saved_searches_list", json!({})).expect("list");
    assert_eq!(listed["searches"][0]["valid"], false);
    assert!(listed["searches"][0]["error"].as_str().is_some_and(|error| !error.is_empty()));
}

#[test]
fn the_new_operations_have_schemas_and_the_documented_annotations() {
    let defs = postino_core::op_defs();
    let names = [
        "threads_list",
        "thread_get",
        "thread_flag",
        "saved_searches_list",
        "saved_search_create",
        "saved_search_update",
        "saved_search_delete",
        "saved_search_recent_clear",
    ];
    for name in names {
        let def = defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("{name} is missing"));
        assert!(def.input_schema.is_object(), "{name} has no schema");
        assert!(!def.description.is_empty(), "{name} has no description");
        assert!(!def.sensitive, "{name} must not be sensitive");
    }
    for (name, read_only, destructive) in [
        ("threads_list", true, false),
        ("thread_get", true, false),
        ("thread_flag", false, false),
        ("saved_searches_list", true, false),
        ("saved_search_create", false, false),
        ("saved_search_update", false, false),
        ("saved_search_delete", false, true),
        ("saved_search_recent_clear", false, false),
    ] {
        let def = defs.iter().find(|def| def.name == name).expect(name);
        assert_eq!(def.read_only, read_only, "{name} read_only");
        assert_eq!(def.destructive, destructive, "{name} destructive");
    }
    let mut engine = Engine::demo();
    let bad = engine.call("threads_list", json!({ "unknown": true })).expect_err("unknown fields are refused");
    assert!(matches!(bad, OpError::InvalidArgs(_)));
}

#[test]
fn demo_threads_group_the_preventivo_messages_and_flag_the_whole_thread() {
    let mut engine = Engine::demo();
    let listed = engine.call("threads_list", json!({ "folder": "inbox" })).expect("threads");
    let thread = listed["threads"].as_array().expect("threads").iter().find(|thread| thread["id"] == "thr_preventivo").expect("preventivo thread");
    assert_eq!(thread["message_count"], 3);
    assert_eq!(thread["unread_count"], 2);
    let detail = engine.call("thread_get", json!({ "id": "thr_preventivo" })).expect("thread");
    assert_eq!(detail["messages"].as_array().map(Vec::len), Some(3));
    let flagged = engine.call("thread_flag", json!({ "id": "thr_preventivo", "unread": false })).expect("flag");
    assert_eq!(flagged["changed"], 2);
    let again = engine.call("thread_flag", json!({ "id": "thr_preventivo", "unread": false })).expect("idempotent");
    assert_eq!(again["changed"], 0);
}

#[test]
fn a_store_thread_flag_refuses_a_thread_over_the_cap() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let root = message("msg_root", INBOX, "<root@example.com>", "Lunga", T0);
    insert(&store, &root, None, &[]);
    for index in 0..500u32 {
        let id = format!("msg_child_{index}");
        let row = message(&id, INBOX, &format!("<child-{index}@example.com>"), "Re: Lunga", T0 + i64::from(index) + 1);
        insert(&store, &row, Some("root@example.com"), &["<root@example.com>"]);
    }
    drop(store);
    let mut engine = fixture.engine();
    let error = engine.call("thread_flag", json!({ "id": "root@example.com", "unread": false })).expect_err("501 messages");
    assert!(matches!(error, OpError::InvalidArgs(ref detail) if detail.contains("500")));
    // The thread itself is still readable.
    let detail = engine.call("thread_get", json!({ "id": "root@example.com", "limit": 1 })).expect("read");
    assert_eq!(detail["total"], 501);
}

#[test]
fn store_replies_keep_the_smallest_merged_id_across_the_operation_surface() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let root = message("msg_root", INBOX, "<root@example.com>", "Filo", T0);
    insert(&store, &root, None, &[]);
    let reply = message("msg_reply", INBOX, "<reply@example.com>", "Re: Filo", T0 + 1000);
    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);
    // A later message that cites both roots merges them; the reply follows the smallest id.
    let merged = message("msg_merged", INBOX, "<zzz@example.com>", "Ponte", T0 + 2000);
    insert(&store, &merged, None, &["<zzz@example.com>", "<root@example.com>"]);
    let bridge_reply = message("msg_bridge_reply", INBOX, "<bridge-reply@example.com>", "Re: Ponte", T0 + 3000);
    insert(&store, &bridge_reply, None, &["<zzz@example.com>"]);
    let stored = store.get_message("msg_bridge_reply").expect("read").expect("message");
    assert_eq!(stored.summary.thread_id, "root@example.com", "a reference to the bridge resolves through its message row");
    drop(store);
    let mut engine = fixture.engine();
    let detail = engine.call("thread_get", json!({ "id": "root@example.com" })).expect("thread");
    assert_eq!(detail["resolved_id"], "root@example.com");
    assert_eq!(detail["total"], 4);
}
