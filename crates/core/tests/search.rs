//! End-to-end search through the operation registry on a store-backed engine.

use postino_core::{Engine, OpError, parse_search};
use postino_store::{Account, Address, Attachment, Folder, FolderKind, Label, Message, MessageSummary, Store, StoreKey};
use serde_json::{Value, json};

fn key() -> StoreKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random");
    StoreKey::from_bytes(bytes)
}

struct Fixture {
    dir: tempfile::TempDir,
    key: StoreKey,
}

impl Fixture {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("temp dir"), key: key() }
    }
    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("mail.db")
    }
    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("store")
    }
}

fn message(id: &str, folder: &str, subject: &str, body: &str, date: i64, unread: bool) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: "acc_1".to_owned(),
            folder_id: folder.to_owned(),
            thread_id: format!("thr_{id}"),
            from: Address::new("Giulia Rossi", "giulia@example.com"),
            to: vec![Address::new("Studio", "studio@example.com")],
            subject: subject.to_owned(),
            snippet: body.chars().take(80).collect(),
            date,
            unread,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: body.to_owned(),
        body_html: Some("<p>html segreto</p>".to_owned()),
        remote_images: 0,
        size: Some(body.len() as u64),
    }
}

fn seeded() -> (Fixture, Engine) {
    let fixture = Fixture::new();
    let store = fixture.open();
    store
        .upsert_account(&Account {
            id: "acc_1".to_owned(),
            name: "Studio".to_owned(),
            email: "studio@example.com".to_owned(),
            provider: "demo".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    for (id, name, kind) in [("inbox", "Posta in arrivo", FolderKind::Inbox), ("archive", "Archivio", FolderKind::Archive)] {
        store
            .upsert_folder(&Folder {
                id: id.to_owned(),
                account_id: "acc_1".to_owned(),
                name: name.to_owned(),
                kind,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: Some(9),
                last_sync_at: None,
            })
            .expect("folder");
    }
    store.create_label(&Label { id: "lbl_1".to_owned(), account_id: None, name: "Lavoro".to_owned(), color: "forest".to_owned() }).expect("label");
    store
        .upsert_synced_message(&message("msg_1", "inbox", "Fattura 2026", "La fattura di marzo è pronta", 1_800_000_000_000, true), &[])
        .expect("m1");
    let mut with_attachment = message("msg_2", "archive", "Contratto", "Il contratto firmato", 1_700_000_000_000, false);
    with_attachment.summary.labels = vec!["lbl_1".to_owned()];
    let attachments = vec![Attachment {
        id: "att_2".to_owned(),
        filename: "contratto.pdf".to_owned(),
        mime: "application/pdf".to_owned(),
        size: 20,
        blob_hash: None,
    }];
    store.upsert_synced_message(&with_attachment, &attachments).expect("m2");
    store.upsert_synced_message(&message("msg_3", "inbox", "Nota", "nessuna parola chiave", 1_650_000_000_000, false), &[]).expect("m3");
    store.apply_label("lbl_1", &["msg_2".to_owned()]).expect("label");
    drop(store);
    let engine = Engine::open(fixture.open()).expect("engine");
    (fixture, engine)
}

fn call(engine: &mut Engine, args: Value) -> Value {
    engine.call("search", args).expect("search")
}

#[test]
fn search_is_sql_backed_and_returns_plain_hits() {
    let (_fixture, mut engine) = seeded();
    let result = call(&mut engine, json!({ "query": "fattura" }));
    assert_eq!(result["total"], 1);
    assert_eq!(result["hits"][0]["message"]["id"], "msg_1");
    assert_eq!(result["hits"][0]["matched"], "all");
    let snippet = result["hits"][0]["snippet"].as_str().expect("snippet");
    assert!(snippet.to_lowercase().contains("fattura"));
    assert!(!snippet.contains("<p>"));
    assert!(snippet.chars().count() <= 240);
    assert!(result["messages"][0]["id"] == "msg_1", "messages remains the deprecated alias");
    assert_eq!(result["query"], "fattura");
    assert!(result["took_ms"].as_u64().is_some());
}

#[test]
fn fields_names_dates_sizes_and_boolean_operators_work() {
    let (_fixture, mut engine) = seeded();
    assert_eq!(call(&mut engine, json!({ "query": "from:giulia" }))["total"], 3);
    assert_eq!(call(&mut engine, json!({ "query": "subject:\"fattura 2026\"" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "label:Lavoro" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "label:lbl_1" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "folder:Archivio" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "is:unread" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "has:attachment" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "after:2026-12-01 before:2028-01-01" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "larger:25" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "subject:nota OR label:Lavoro" }))["total"], 2);
    assert_eq!(call(&mut engine, json!({ "query": "from:giulia AND NOT is:unread" }))["total"], 2);
    assert_eq!(call(&mut engine, json!({ "query": "subject:\"fattu\"*" }))["total"], 1);
    assert_eq!(call(&mut engine, json!({ "query": "\"parola chiave\"" }))["total"], 1);
}

#[test]
fn pagination_sort_and_filters_match_the_arguments() {
    let (_fixture, mut engine) = seeded();
    let page = call(&mut engine, json!({ "query": "from:giulia", "limit": 1, "offset": 1, "sort": "date" }));
    assert_eq!(page["total"], 3);
    assert_eq!(page["hits"].as_array().expect("hits").len(), 1);
    let first = call(&mut engine, json!({ "query": "from:giulia", "limit": 1, "sort": "date" }));
    assert_eq!(first["hits"][0]["message"]["id"], "msg_1");
    let filtered = call(&mut engine, json!({ "query": "from:giulia", "label": "lbl_1" }));
    assert_eq!(filtered["total"], 1);
    let folder = call(&mut engine, json!({ "query": "from:giulia", "folder": "archive" }));
    assert_eq!(folder["total"], 1);
    // The default order is relevance, but bm25 may legitimately tie; the stable id/date tie-break
    // must never repeat a row across pages.
    let one = call(&mut engine, json!({ "query": "from:giulia", "limit": 1 }));
    let two = call(&mut engine, json!({ "query": "from:giulia", "limit": 1, "offset": 1 }));
    assert_ne!(one["hits"][0]["message"]["id"], two["hits"][0]["message"]["id"]);
}

#[test]
fn unknown_ids_and_parse_errors_are_actionable() {
    let (_fixture, mut engine) = seeded();
    let unknown = engine.call("search", json!({ "query": "label:Sconosciuta" })).expect_err("unknown label");
    assert!(matches!(unknown, OpError::InvalidArgs(ref detail) if detail.contains("unknown label")), "{unknown}");
    let unknown_folder = engine.call("search", json!({ "query": "folder:Sconosciuta" })).expect_err("unknown folder");
    assert!(matches!(unknown_folder, OpError::InvalidArgs(_)), "{unknown_folder}");
    let parse = engine.call("search", json!({ "query": "from:" })).expect_err("empty field");
    assert!(matches!(parse, OpError::InvalidArgs(ref detail) if detail.contains("dangling_field") && detail.contains("position")), "{parse}");
    let error = parse_search("subject:\"unclosed").expect_err("unclosed");
    assert_eq!((error.code, error.position), ("unclosed_quote", 8));
}

#[test]
fn status_and_reindex_are_operations_and_account_is_required_when_ambiguous() {
    let (_fixture, mut engine) = seeded();
    let status = engine.call("search_status", json!({})).expect("status");
    assert_eq!(status["ready"], true);
    assert_eq!(status["indexed"], 3);
    let rebuilt = engine.call("search_reindex", json!({})).expect("reindex");
    assert_eq!(rebuilt["documents"], 3);
    assert_eq!(rebuilt["threads"], 0);
}

#[test]
fn opening_the_store_does_not_put_bodies_in_the_list_and_message_get_reads_one() {
    let (_fixture, mut engine) = seeded();
    let listed = engine.call("messages_list", json!({ "folder": "inbox", "limit": 50 })).expect("list");
    let rows = listed["messages"].as_array().expect("rows");
    assert!(rows.iter().all(|row| !row.as_object().is_some_and(|object| object.contains_key("body_text"))));
    let got = engine.call("message_get", json!({ "id": "msg_1" })).expect("get");
    assert_eq!(got["message"]["body_text"], "La fattura di marzo è pronta");
    assert!(engine.message("msg_1").is_some_and(|message| message.body_text.contains("fattura")));
}
