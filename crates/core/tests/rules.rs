//! P9c end-to-end rules: ordering, stop, later-rule effects, idempotency, preview, undo, forward,
//! caps, failure isolation and the automatic pass after `sync_now` on a real encrypted store.

mod support;

use postino_core::{Engine, OpError};
use postino_store::{
    Account, AccountServer, AccountTls, Address, Folder, FolderKind, Label, Message, MessageSummary, RuleGuard, SmtpServer, Store, StoreKey,
};
use postino_testkit::{TestFolder, TestMessage, synthetic_message};
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
    fn raw(&self) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open(self.path()).expect("raw open");
        let hex = self.key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("raw key");
        connection
    }
    fn count(&self, table: &str) -> i64 {
        self.raw().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).expect("count")
    }
}

fn folder(id: &str, kind: FolderKind) -> Folder {
    Folder {
        id: id.to_owned(),
        account_id: "acc_1".to_owned(),
        name: id.to_owned(),
        kind,
        unread: 0,
        total: 0,
        uid_validity: Some(1),
        uid_next: Some(9),
        last_sync_at: None,
    }
}

fn message(id: &str, folder: &str, subject: &str, unread: bool) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: "acc_1".to_owned(),
            folder_id: folder.to_owned(),
            thread_id: format!("thr_{id}"),
            from: Address::new("Giulia Rossi", "giulia@example.com"),
            to: vec![Address::new("Studio", "studio@example.com")],
            subject: subject.to_owned(),
            snippet: subject.to_owned(),
            date: 1_800_000_000_000,
            unread,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: format!("Corpo di {subject}"),
        body_html: None,
        remote_images: 0,
        size: Some(subject.len() as u64),
    }
}

fn seeded(smtp: bool) -> (Fixture, Engine) {
    let fixture = Fixture::new();
    let store = fixture.open();
    store
        .upsert_account(&Account {
            id: "acc_1".to_owned(),
            name: "Studio".to_owned(),
            email: "studio@example.com".to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    store
        .upsert_account_server(&AccountServer {
            account_id: "acc_1".to_owned(),
            host: "localhost".to_owned(),
            port: 993,
            tls: AccountTls::Implicit,
            username: "studio@example.com".to_owned(),
            created_at: 1_700_000_000_000,
            smtp: smtp.then(|| SmtpServer {
                host: "localhost".to_owned(),
                port: 465,
                tls: AccountTls::Implicit,
                username: "studio@example.com".to_owned(),
            }),
        })
        .expect("server");
    for folder in [
        folder("inbox", FolderKind::Inbox),
        folder("archive", FolderKind::Archive),
        folder("trash", FolderKind::Trash),
        folder("drafts", FolderKind::Drafts),
        folder("sent", FolderKind::Sent),
    ] {
        store.upsert_folder(&folder).expect("folder");
    }
    store
        .create_label(&Label { id: "lbl_lavoro".to_owned(), account_id: None, name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    for (id, folder, subject, unread) in
        [("msg_1", "inbox", "Fattura 2026", true), ("msg_2", "inbox", "Notizia", true), ("msg_3", "inbox", "Fattura vecchia", false)]
    {
        store.upsert_synced_message(&message(id, folder, subject, unread), &[]).expect("message");
    }
    let raw = fixture.raw();
    for (index, id) in ["msg_1", "msg_2", "msg_3"].iter().enumerate() {
        raw.execute("UPDATE messages SET ingested_at = ?1 WHERE id = ?2", rusqlite::params![1_000 + index as i64, id]).expect("ingested");
    }
    drop(store);
    let engine = Engine::open(fixture.open()).expect("engine");
    (fixture, engine)
}

fn call(engine: &mut Engine, name: &str, args: Value) -> Value {
    engine.call(name, args).expect(name)
}

fn create(engine: &mut Engine, name: &str, query: &str, actions: Value, priority: i64, stop: bool) -> Value {
    call(
        engine,
        "rules_create",
        json!({
            "name": name,
            "query": query,
            "actions": actions,
            "priority": priority,
            "stop": stop
        }),
    )
}

fn run(engine: &mut Engine, scope: &str) -> Value {
    call(engine, "rules_run", json!({ "scope": scope }))
}

/// Make the seeded mail look as if it arrived after the rules were created. Rules only apply to
/// mail ingested after their own `created_at`, so a test that wants a match must arrange that
/// order explicitly.
fn ingest_base_after_rules(fixture: &Fixture) {
    let created: i64 = fixture.raw().query_row("SELECT MAX(created_at) FROM rules", [], |row| row.get(0)).expect("created");
    fixture
        .raw()
        .execute("UPDATE messages SET ingested_at = ?1 WHERE id != 'msg_old'", rusqlite::params![created.saturating_add(1)])
        .expect("ingested");
}

#[test]
fn priority_and_stop_are_observable() {
    let (_fixture, mut engine) = seeded(true);
    let stop = create(&mut engine, "Stop", "subject:Fattura", json!([{ "kind": "read" }]), 0, true);
    assert_eq!(stop["rule"]["id"], "rule_1");
    create(&mut engine, "Later", "from:giulia", json!([{ "kind": "label", "label": "lbl_lavoro" }]), 1, false);
    ingest_base_after_rules(&_fixture);
    let result = run(&mut engine, "new");
    assert_eq!(result["run"]["matched"], 3, "{result}");
    // msg_1 and msg_3 match the stop rule and are read; only msg_2 (not stopped) gets the label.
    let listed = call(&mut engine, "messages_list", json!({ "folder": "inbox" }));
    for row in listed["messages"].as_array().expect("rows") {
        match row["id"].as_str().unwrap_or_default() {
            "msg_1" | "msg_3" => assert_eq!(row["unread"], false, "{row}"),
            "msg_2" => assert_eq!(row["labels"], json!(["lbl_lavoro"]), "{row}"),
            other => panic!("unexpected row {other}"),
        }
    }
    let second = run(&mut engine, "new");
    assert_eq!(second["run"]["matched"], 0, "a second new run is a no-op");
    assert_eq!(second["run"]["changed"], 0);
}

#[test]
fn a_later_rule_sees_an_earlier_move_and_label() {
    let (_fixture, mut engine) = seeded(true);
    create(&mut engine, "Etichetta", "subject:Fattura", json!([{ "kind": "label", "label": "lbl_lavoro" }]), 0, false);
    create(&mut engine, "Archivia", "label:Lavoro", json!([{ "kind": "move", "folder": "archive" }]), 1, false);
    ingest_base_after_rules(&_fixture);
    let result = run(&mut engine, "new");
    assert!(result["errors"].as_array().is_some_and(Vec::is_empty), "{result}");
    let archived = call(&mut engine, "messages_list", json!({ "folder": "archive" }));
    let ids = archived["messages"].as_array().expect("rows").iter().filter_map(|row| row["id"].as_str()).collect::<Vec<_>>();
    assert_eq!(ids, ["msg_1", "msg_3"], "the later rule queried the label the first rule applied: {archived}");
}

#[test]
fn preview_writes_nothing() {
    let (fixture, mut engine) = seeded(true);
    let before = fixture
        .raw()
        .query_row("SELECT folder_id, unread, rules_applied_at FROM messages WHERE id = 'msg_1'", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, Option<i64>>(2)?))
        })
        .expect("before");
    let preview = call(
        &mut engine,
        "rules_preview",
        json!({
            "query": "subject:\"Fattura 2026\"",
            "actions": [
                { "kind": "move", "folder": "archive" },
                { "kind": "label", "label": "lbl_lavoro" },
                { "kind": "read" },
                { "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }
            ]
        }),
    );
    assert_eq!(preview["total"], 1, "{preview}");
    assert_eq!(preview["rows"][0]["message"]["id"], "msg_1");
    assert_eq!(preview["rows"][0]["effects"].as_array().map(Vec::len), Some(4));
    assert_eq!(preview["rows"][0]["effects"][0]["status"], "apply");
    let after = fixture
        .raw()
        .query_row("SELECT folder_id, unread, rules_applied_at FROM messages WHERE id = 'msg_1'", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, Option<i64>>(2)?))
        })
        .expect("after");
    assert_eq!(before, after, "preview changed the message");
    assert_eq!(fixture.count("rule_runs"), 0, "preview journaled a run");
    assert_eq!(fixture.count("rule_actions"), 0);
    assert_eq!(fixture.count("outbox"), 0);
    let labels: i64 =
        fixture.raw().query_row("SELECT COUNT(*) FROM message_labels WHERE message_id = 'msg_1'", [], |row| row.get(0)).expect("labels");
    assert_eq!(labels, 0);
}

#[test]
fn undo_reverses_unchanged_messages_and_skips_a_message_changed_after_the_run() {
    let (fixture, mut engine) = seeded(true);
    create(
        &mut engine,
        "Organizza",
        "subject:Fattura",
        json!([
            { "kind": "move", "folder": "archive" },
            { "kind": "label", "label": "lbl_lavoro" },
            { "kind": "read" }
        ]),
        0,
        false,
    );
    ingest_base_after_rules(&fixture);
    let result = run(&mut engine, "new");
    let run_id = result["run"]["id"].as_str().expect("run id").to_owned();
    // msg_3 is already read; only msg_1 changes for all three actions.
    let changed_message = call(&mut engine, "messages_list", json!({ "folder": "archive" }));
    assert_eq!(changed_message["messages"].as_array().map(Vec::len), Some(2));
    // A user move after the run makes that message deliberately untouched by undo.
    call(&mut engine, "message_move", json!({ "id": "msg_3", "folder": "trash" }));
    let undo = call(&mut engine, "rules_undo", json!({ "id": run_id }));
    assert_eq!(undo["restored"], 3, "{undo}");
    assert!(undo["skipped"].as_u64().is_some_and(|skipped| skipped >= 1), "{undo}");
    let inbox = call(&mut engine, "messages_list", json!({ "folder": "inbox" }));
    let ids = inbox["messages"].as_array().expect("rows").iter().filter_map(|row| row["id"].as_str()).collect::<Vec<_>>();
    assert!(ids.contains(&"msg_1"), "msg_1 was restored: {inbox}");
    assert!(!ids.contains(&"msg_3"), "the changed message stays where the user put it");
    let trash = call(&mut engine, "messages_list", json!({ "folder": "trash" }));
    assert_eq!(trash["messages"][0]["id"], "msg_3");
    let again = engine.call("rules_undo", json!({ "id": run_id })).expect_err("one-way undo");
    assert!(matches!(again, OpError::InvalidArgs(ref detail) if detail.contains("already undone")), "{again}");
}

#[test]
fn forward_is_one_outbox_row_with_loop_headers_and_undo_reports_it() {
    let (fixture, mut engine) = seeded(true);
    create(&mut engine, "Inoltra", "subject:Fattura", json!([{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]), 0, false);
    ingest_base_after_rules(&fixture);
    let first = run(&mut engine, "new");
    assert!(first["errors"].as_array().is_some_and(Vec::is_empty), "{first}");
    assert_eq!(fixture.count("outbox"), 2, "one row per matched message");
    let second = run(&mut engine, "new");
    assert_eq!(second["run"]["matched"], 0, "{second}");
    let all = run(&mut engine, "all");
    assert_eq!(all["run"]["matched"], 2);
    assert_eq!(all["run"]["changed"], 0, "a repeated all run changes nothing");
    assert_eq!(fixture.count("outbox"), 2, "the deterministic key dedupes every repeat");
    let store = fixture.open();
    let row = store.list_outbox(None, 10).expect("rows").into_iter().next().expect("row");
    assert!(row.draft_id.is_none(), "a rule forward is not a draft");
    assert!(row.idempotency_key.starts_with("rule:rule_1:msg_"), "{}", row.idempotency_key);
    let raw = store.blobs().get(&row.raw_blob_hash, 256 * 1024).expect("bytes");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("Auto-Submitted: auto-forwarded"), "{text}");
    assert!(text.contains("X-Postino-Rule: rule_1"), "{text}");
    assert!(!text.contains("dest@example.com") || text.contains("To: dest@example.com"), "the recipient is in To, not a leak");
    drop(store);
    let run_id = first["run"]["id"].as_str().expect("run id").to_owned();
    let undo = call(&mut engine, "rules_undo", json!({ "id": run_id }));
    let outbox_ids = undo["forwards"].as_array().expect("forward ids");
    assert_eq!(outbox_ids.len(), 2, "{undo}");
    assert!(outbox_ids.iter().all(|id| id.as_str().is_some_and(|id| id.starts_with("ob_"))), "{undo}");
}

#[test]
fn a_loop_guarded_message_is_not_forwarded_and_a_missing_smtp_endpoint_fails_readably() {
    let (_fixture, mut engine) = seeded(true);
    create(&mut engine, "Inoltra", "subject:Fattura", json!([{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]), 0, false);
    ingest_base_after_rules(&_fixture);
    let store = _fixture.open();
    store.set_rule_guard("msg_1", RuleGuard { auto_submitted: true, x_postino_rule: false }).expect("guard");
    drop(store);
    let result = run(&mut engine, "new");
    assert!(result["errors"].as_array().is_some_and(Vec::is_empty), "{result}");
    assert_eq!(_fixture.count("outbox"), 1, "only msg_3 was forwarded");
    let preview = call(
        &mut engine,
        "rules_preview",
        json!({ "query": "subject:Fattura", "actions": [{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }] }),
    );
    let statuses =
        preview["rows"].as_array().expect("rows").iter().map(|row| row["effects"][0]["status"].as_str().unwrap_or_default()).collect::<Vec<_>>();
    assert_eq!(statuses, ["skip", "apply"], "the preview shows the loop guard: {preview}");

    let (no_smtp, mut engine) = seeded(false);
    create(&mut engine, "Inoltra", "subject:Fattura", json!([{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]), 0, false);
    ingest_base_after_rules(&no_smtp);
    let result = run(&mut engine, "new");
    assert!(result["run"]["errors"].as_u64().is_some_and(|errors| errors >= 1), "{result}");
    assert!(result["errors"][0].as_str().is_some_and(|error| error.contains("no SMTP endpoint")), "{result}");
    assert_eq!(no_smtp.count("outbox"), 0);
}

#[test]
fn action_and_recipient_caps_and_a_stale_target_are_readable() {
    let (_fixture, mut engine) = seeded(true);
    let too_many_actions = [
        json!({ "kind": "move", "folder": "archive" }),
        json!({ "kind": "label", "label": "lbl_lavoro" }),
        json!({ "kind": "read" }),
        json!({ "kind": "trash" }),
        json!({ "kind": "forward", "to": [{ "name": "", "email": "a@example.com" }] }),
        json!({ "kind": "unread" }),
    ];
    let error =
        engine.call("rules_create", json!({ "name": "Troppe", "query": "subject:Fattura", "actions": too_many_actions })).expect_err("six actions");
    assert!(matches!(error, OpError::InvalidArgs(ref detail) if detail.contains("at most 5")), "{error}");

    let duplicate = json!([{ "kind": "read" }, { "kind": "read" }]);
    let error =
        engine.call("rules_create", json!({ "name": "Doppia", "query": "subject:Fattura", "actions": duplicate })).expect_err("duplicate kind");
    assert!(matches!(error, OpError::InvalidArgs(ref detail) if detail.contains("only one read")), "{error}");

    let eleven = (0..11).map(|index| json!({ "name": "", "email": format!("dest{index}@example.com") })).collect::<Vec<_>>();
    let error = engine
        .call("rules_create", json!({ "name": "Inoltro", "query": "subject:Fattura", "actions": [{ "kind": "forward", "to": eleven }] }))
        .expect_err("eleven recipients");
    assert!(matches!(error, OpError::InvalidArgs(ref detail) if detail.contains("1-10")), "{error}");

    let other = call(&mut engine, "label_create", json!({ "name": "Altro", "color": "forest" }));
    let other_id = other["label"]["id"].as_str().expect("other label").to_owned();
    create(&mut engine, "Etichetta", "subject:Fattura", json!([{ "kind": "label", "label": "lbl_lavoro" }, { "kind": "read" }]), 0, false);
    create(&mut engine, "Altro", "subject:Fattura", json!([{ "kind": "label", "label": other_id }]), 1, false);
    ingest_base_after_rules(&_fixture);
    call(&mut engine, "label_delete", json!({ "id": "lbl_lavoro" }));
    let listed = call(&mut engine, "rules_list", json!({}));
    let stale = listed["rules"].as_array().expect("rules").iter().find(|rule| rule["name"] == "Etichetta").expect("stale rule");
    assert_eq!(stale["valid"], false, "{listed}");
    assert!(stale["error"].as_str().is_some_and(|error| error.contains("unknown label")), "{listed}");
    let result = run(&mut engine, "new");
    assert!(result["errors"][0].as_str().is_some_and(|error| error.contains("no longer exists")), "{result}");
    assert!(result["run"]["errors"].as_u64().is_some_and(|errors| errors >= 1));
    // The failed label aborts the read in the same rule; the second rule still runs.
    let listed = call(&mut engine, "messages_list", json!({ "folder": "inbox" }));
    for row in listed["messages"].as_array().expect("rows") {
        let id = row["id"].as_str().unwrap_or_default();
        if id == "msg_1" || id == "msg_3" {
            assert_eq!(row["labels"], json!([other_id]), "the later rule did not run: {row}");
            if id == "msg_1" {
                assert_eq!(row["unread"], true, "the failed rule's read action was not aborted: {row}");
            }
        } else {
            assert_eq!(row["unread"], true, "{row}");
        }
    }
    let journal = call(&mut engine, "rules_runs", json!({ "limit": 20 }));
    let encoded = serde_json::to_string(&journal).expect("json");
    for forbidden in ["Fattura", "giulia@example.com", "Corpo di", "subject:Fattura"] {
        assert!(!encoded.contains(forbidden), "the journal leaked {forbidden}: {encoded}");
    }
    let run_id = result["run"]["id"].as_str().expect("run id");
    let actions = _fixture.open().rule_run_actions(run_id).expect("actions");
    let encoded = actions.iter().map(|action| format!("{} {} {}", action.rule_id, action.message_id, action.value_json)).collect::<String>();
    for forbidden in ["Fattura", "giulia@example.com", "Corpo di", "subject:Fattura"] {
        assert!(!encoded.contains(forbidden), "a rule action leaked {forbidden}: {encoded}");
    }
}

#[test]
fn only_mail_ingested_after_the_rule_was_created_is_processed() {
    let (fixture, mut engine) = seeded(true);
    // A pre-rule message: ingested long before the rule exists.
    let store = fixture.open();
    store.upsert_synced_message(&message("msg_old", "inbox", "Vecchia fattura", true), &[]).expect("old");
    drop(store);
    fixture.raw().execute("UPDATE messages SET ingested_at = 1 WHERE id = 'msg_old'", []).expect("old time");
    create(&mut engine, "Letta", "subject:fattura", json!([{ "kind": "read" }]), 0, false);
    ingest_base_after_rules(&fixture);
    let first = run(&mut engine, "all");
    assert!(first["run"]["changed"].as_u64().is_some_and(|changed| changed >= 1), "{first}");
    let old = fixture.raw().query_row("SELECT unread FROM messages WHERE id = 'msg_old'", [], |row| row.get::<_, bool>(0)).expect("old");
    assert!(old, "a rule must not apply retroactively to mail ingested before it existed");

    // A `new` pass marks the base corpus, then a genuinely new message is the only candidate.
    let _ = run(&mut engine, "new");
    let store = fixture.open();
    store.upsert_synced_message(&message("msg_new", "inbox", "Nuova fattura", true), &[]).expect("new");
    drop(store);
    let second = run(&mut engine, "new");
    assert_eq!(second["run"]["matched"], 1, "{second}");
    let new = fixture.raw().query_row("SELECT unread FROM messages WHERE id = 'msg_new'", [], |row| row.get::<_, bool>(0)).expect("new");
    assert!(!new, "mail ingested after the rule is read");
}

#[test]
fn a_run_queues_at_most_fifty_forwards() {
    let (fixture, mut engine) = seeded(true);
    create(
        &mut engine,
        "Inoltra",
        "subject:Inoltrami",
        json!([{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]),
        0,
        false,
    );
    let store = fixture.open();
    for index in 0..51 {
        store.upsert_synced_message(&message(&format!("msg_cap_{index:03}"), "inbox", "Inoltrami", true), &[]).expect("message");
    }
    drop(store);
    let result = run(&mut engine, "all");
    assert_eq!(result["run"]["matched"], 51, "{result}");
    assert_eq!(fixture.count("outbox"), 50);
    assert!(
        result["errors"].as_array().is_some_and(|errors| errors.iter().any(|error| error.as_str().is_some_and(|text| text.contains("50 forwards")))),
        "{result}"
    );
}

#[test]
fn a_manual_all_run_is_capped_at_two_thousand_candidates() {
    let (fixture, mut engine) = seeded(true);
    create(&mut engine, "Leggi", "subject:Cap", json!([{ "kind": "read" }]), 0, false);
    let store = fixture.open();
    for index in 0..2_001 {
        store.upsert_synced_message(&message(&format!("msg_cap_{index:04}"), "inbox", "Cap", true), &[]).expect("message");
    }
    drop(store);
    let result = run(&mut engine, "all");
    assert_eq!(result["run"]["matched"], 2_000, "{result}");
    let untouched: bool = fixture.raw().query_row("SELECT unread FROM messages WHERE id = 'msg_cap_2000'", [], |row| row.get(0)).expect("last");
    assert!(untouched, "the 2 001st candidate is outside the manual cap");
}

#[test]
fn rule_crud_schemas_and_validation() {
    let (_fixture, mut engine) = seeded(true);
    let created = create(&mut engine, "Fatture", "subject:Fattura", json!([{ "kind": "read" }]), 7, false);
    assert_eq!(created["rule"]["valid"], true);
    assert_eq!(created["rule"]["priority"], 7);
    let listed = call(&mut engine, "rules_list", json!({}));
    assert_eq!(listed["rules"].as_array().map(Vec::len), Some(1));
    let updated = call(&mut engine, "rules_update", json!({ "id": "rule_1", "enabled": false, "priority": 2 }));
    assert_eq!(updated["rule"]["enabled"], false);
    assert_eq!(updated["rule"]["priority"], 2);
    let deleted = call(&mut engine, "rules_delete", json!({ "id": "rule_1" }));
    assert_eq!(deleted["rule"]["name"], "Fatture");
    let empty = call(&mut engine, "rules_list", json!({}));
    assert_eq!(empty["rules"].as_array().map(Vec::len), Some(0));

    let unknown = engine
        .call("rules_create", json!({ "name": "X", "query": "subject:Fattura", "actions": [{ "kind": "read" }], "extra": true }))
        .expect_err("unknown key");
    assert!(matches!(unknown, OpError::InvalidArgs(_)), "{unknown}");
    let bad_query = engine.call("rules_create", json!({ "name": "X", "query": "from:", "actions": [{ "kind": "read" }] })).expect_err("bad query");
    assert!(matches!(bad_query, OpError::InvalidArgs(_)), "{bad_query}");
    let bad_folder = engine
        .call("rules_create", json!({ "name": "X", "query": "subject:Fattura", "actions": [{ "kind": "move", "folder": "drafts" }] }))
        .expect_err("draft destination");
    assert!(matches!(bad_folder, OpError::InvalidArgs(ref detail) if detail.contains("never edits a draft")), "{bad_folder}");

    let defs = postino_core::op_defs();
    let find = |name: &str| defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("{name} is missing"));
    for name in ["rules_list", "rules_create", "rules_update", "rules_delete", "rules_preview", "rules_run", "rules_undo", "rules_runs"] {
        let def = find(name);
        assert!(def.input_schema.is_object(), "{name} has no schema");
        assert!(!def.sensitive, "{name} handles no credential");
    }
    assert!(find("rules_list").read_only);
    assert!(find("rules_preview").read_only);
    assert!(find("rules_runs").read_only);
    assert!(!find("rules_run").read_only);
    assert!(find("rules_run").destructive);
    assert!(find("rules_delete").destructive);
    assert!(!find("rules_undo").destructive);
}

#[test]
fn sync_now_runs_new_rules_and_never_fails_because_of_them() {
    let harness = support::Harness::new();
    let store = harness.open();
    let vault = support::MemoryVault::new();
    let server = support::RunningServer::start(
        vec![TestFolder::new("INBOX", 10).with_special_use("\\Inbox").with_messages(vec![TestMessage::synthetic(synthetic_message(1))])],
        1,
    );
    support::seed_account(&store, &vault, support::ACCOUNT_ID, server.port(), &[("inbox", "INBOX", FolderKind::Inbox)]);
    store.create_label(&Label { id: "lbl_sync".to_owned(), account_id: None, name: "Sync".to_owned(), color: "forest".to_owned() }).expect("label");
    let mut engine = Engine::open_with_vault(store, vault).expect("engine");
    engine.set_test_extra_roots(vec![server.ca()]);
    create(&mut engine, "Da sync", "from:synthetic", json!([{ "kind": "label", "label": "lbl_sync" }]), 0, false);

    let synced = call(&mut engine, "sync_now", json!({}));
    assert_eq!(synced["outcome"]["fetched"], 1);
    server.finish();
    let listed = call(&mut engine, "messages_list", json!({}));
    assert_eq!(listed["messages"][0]["labels"], json!(["lbl_sync"]), "the post-sync pass did not label the new message");
    let runs = call(&mut engine, "rules_runs", json!({}));
    assert_eq!(runs["runs"].as_array().map(Vec::len), Some(1), "sync_now journaled no rule run: {runs}");
    assert_eq!(runs["runs"][0]["mode"], "new");
    assert!(runs["runs"][0]["changed"].as_u64().is_some_and(|changed| changed >= 1));
}

#[test]
fn the_demo_engine_runs_and_undoes_rules_too() {
    let mut engine = Engine::demo();
    let labels = call(&mut engine, "labels_list", json!({}));
    let label = labels["labels"][0]["id"].as_str().expect("a demo label").to_owned();
    create(&mut engine, "Demo", "postino", json!([{ "kind": "label", "label": label }]), 0, false);
    let result = run(&mut engine, "new");
    assert!(result["run"]["matched"].as_u64().is_some_and(|matched| matched > 0), "{result}");
    let second = run(&mut engine, "new");
    assert_eq!(second["run"]["matched"], 0);
    let run_id = result["run"]["id"].as_str().expect("run id").to_owned();
    let undo = call(&mut engine, "rules_undo", json!({ "id": run_id }));
    assert!(undo["restored"].as_u64().is_some_and(|restored| restored > 0), "{undo}");
}
