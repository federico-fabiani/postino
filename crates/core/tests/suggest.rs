//! `search_suggest` and `correspondents_suggest` through the operation registry.

use postino_core::Engine;
use postino_store::{Account, Address, Folder, FolderKind, Label, Message, MessageSummary, Store, StoreKey};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    key: StoreKey,
}

impl Fixture {
    fn new() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS random");
        Self { dir: tempfile::tempdir().expect("temp dir"), key: StoreKey::from_bytes(bytes) }
    }

    fn open(&self) -> Store {
        Store::open(&self.dir.path().join("mail.db"), &self.key).expect("store")
    }
}

fn message(id: &str, from: (&str, &str), date: i64) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: "acc_1".to_owned(),
            folder_id: "inbox".to_owned(),
            thread_id: format!("thr_{id}"),
            from: Address::new(from.0, from.1),
            to: vec![Address::new("Studio Verdi", "studio@example.com")],
            subject: "Oggetto".to_owned(),
            snippet: String::new(),
            date,
            unread: false,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "Corpo".to_owned(),
        body_html: None,
        remote_images: 0,
        size: Some(4),
    }
}

fn seeded() -> (Fixture, Engine) {
    let fixture = Fixture::new();
    let store = fixture.open();
    store
        .upsert_account(&Account {
            id: "acc_1".to_owned(),
            name: "Studio Verdi".to_owned(),
            email: "studio@example.com".to_owned(),
            provider: "demo".to_owned(),
            signature: String::new(),
            created_at: 1,
        })
        .expect("account");
    store
        .upsert_folder(&Folder {
            id: "inbox".to_owned(),
            account_id: "acc_1".to_owned(),
            name: "Posta in arrivo".to_owned(),
            kind: FolderKind::Inbox,
            unread: 0,
            total: 0,
            uid_validity: Some(1),
            uid_next: Some(9),
            last_sync_at: None,
        })
        .expect("inbox");
    store
        .upsert_folder(&Folder {
            id: "archive".to_owned(),
            account_id: "acc_1".to_owned(),
            name: "Archivio".to_owned(),
            kind: FolderKind::Archive,
            unread: 0,
            total: 0,
            uid_validity: Some(1),
            uid_next: Some(9),
            last_sync_at: None,
        })
        .expect("archive");
    store.create_label(&Label { id: "lbl_1".to_owned(), account_id: None, name: "Lavoro".to_owned(), color: "forest".to_owned() }).expect("label");
    store.upsert_synced_message(&message("msg_1", ("Giulia Rossi", "giulia@example.com"), 100), &[]).expect("m1");
    store.upsert_synced_message(&message("msg_2", ("Marco Neri", "marco@example.it"), 200), &[]).expect("m2");
    drop(store);
    let engine = Engine::open(fixture.open()).expect("engine");
    (fixture, engine)
}

fn call(engine: &mut Engine, name: &str, args: Value) -> Value {
    engine.call(name, args).expect(name)
}

fn inserts(result: &Value) -> Vec<String> {
    result["suggestions"].as_array().expect("suggestions").iter().map(|row| row["insert"].as_str().expect("insert").to_owned()).collect()
}

#[test]
fn correspondents_come_newest_first_without_the_account_itself() {
    let (_fixture, mut engine) = seeded();
    let result = call(&mut engine, "correspondents_suggest", json!({}));
    let rows = result["correspondents"].as_array().expect("rows");
    assert_eq!(rows.len(), 2, "the sender and one other person, never studio@example.com: {result}");
    assert_eq!(rows[0]["address"], "marco@example.it");
    assert_eq!(rows[0]["name"], "Marco Neri");
    assert_eq!(rows[0]["last_date"], 200);
    assert_eq!(rows[1]["address"], "giulia@example.com");

    let filtered = call(&mut engine, "correspondents_suggest", json!({ "query": "GIU" }));
    assert_eq!(filtered["correspondents"].as_array().expect("rows").len(), 1);
    assert_eq!(filtered["correspondents"][0]["address"], "giulia@example.com");
    assert_eq!(filtered["correspondents"][0]["name"], "Giulia Rossi");
}

#[test]
fn a_field_prefix_completes_to_the_clause_that_names_it() {
    let (_fixture, mut engine) = seeded();
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "fro" }))), vec!["from:"]);
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "subj" }))), vec!["subject:"]);
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "zzz" }))), Vec::<String>::new());
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "" }))), Vec::<String>::new());
}

#[test]
fn people_labels_folders_and_closed_values_complete_from_local_data() {
    let (_fixture, mut engine) = seeded();
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "from:giu" }))), vec!["from:\"giulia@example.com\""]);
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "label:lav" }))), vec!["label:\"Lavoro\""]);
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "folder:arc" }))), vec!["folder:\"Archivio\""]);
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "is:un" }))), vec!["is:unread", "is:unstarred"]);
    // A subject is free text: there is nothing to offer and nothing to guess.
    assert_eq!(inserts(&call(&mut engine, "search_suggest", json!({ "token": "subject:fa" }))), Vec::<String>::new());
    // Every offered clause parses together with a term.
    for token in ["is:un", "after:7", "larger:1", "label:lav", "folder:arc", "from:giu"] {
        for insert in inserts(&call(&mut engine, "search_suggest", json!({ "token": token }))) {
            postino_core::parse_search(&format!("x {insert}")).expect(&insert);
        }
    }
}

#[test]
fn a_typo_in_the_request_is_refused_and_the_demo_mailbox_answers_too() {
    let (_fixture, mut engine) = seeded();
    assert!(engine.call("search_suggest", json!({ "token": "fro", "query": "fro" })).is_err(), "unknown keys fail loudly");

    let mut demo = Engine::demo();
    let result = call(&mut demo, "correspondents_suggest", json!({ "query": "" }));
    assert!(!result["correspondents"].as_array().expect("rows").is_empty(), "the synthetic mailbox has people");
}
