//! P9c store contract: rule order and cap, received-mail candidates, journal retention and the
//! encrypted loop guard.

use postino_store::{
    Account, Address, Folder, FolderKind, Message, MessageSummary, RULE_GUARD_KEY_PREFIX, Rule, RuleActionRecord, RuleCandidateFilter, RuleGuard,
    RuleRun, SEARCH_TOTAL_CAP, SearchExpr, Store, StoreError, StoreKey,
};

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

fn message(id: &str, folder: &str, subject: &str) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: "acc_1".to_owned(),
            folder_id: folder.to_owned(),
            thread_id: format!("thr_{id}"),
            from: Address::new("Giulia", "giulia@example.com"),
            to: vec![Address::new("Studio", "studio@example.com")],
            subject: subject.to_owned(),
            snippet: subject.to_owned(),
            date: 1_800_000_000_000,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: subject.to_owned(),
        body_html: None,
        remote_images: 0,
        size: Some(subject.len() as u64),
    }
}

fn seeded() -> (Fixture, Store) {
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
    for folder in [
        folder("inbox", FolderKind::Inbox),
        folder("archive", FolderKind::Archive),
        folder("drafts", FolderKind::Drafts),
        folder("sent", FolderKind::Sent),
        folder("trash", FolderKind::Trash),
    ] {
        store.upsert_folder(&folder).expect("folder");
    }
    store.upsert_synced_message(&message("msg_inbox", "inbox", "Fattura 2026"), &[]).expect("inbox");
    store.upsert_synced_message(&message("msg_archive", "archive", "Fattura vecchia"), &[]).expect("archive");
    store.upsert_synced_message(&message("msg_draft", "drafts", "Fattura bozza"), &[]).expect("draft");
    store.upsert_synced_message(&message("msg_sent", "sent", "Fattura inviata"), &[]).expect("sent");
    store.upsert_synced_message(&message("msg_trash", "trash", "Fattura cestinata"), &[]).expect("trash");
    // Deterministic ingestion order, because `upsert_synced_message` stamps the wall clock.
    let raw = fixture.raw();
    for (id, ingested) in [("msg_inbox", 1_000i64), ("msg_archive", 2_000), ("msg_draft", 3_000), ("msg_sent", 4_000), ("msg_trash", 5_000)] {
        raw.execute("UPDATE messages SET ingested_at = ?1 WHERE id = ?2", rusqlite::params![ingested, id]).expect("ingested_at");
    }
    (fixture, store)
}

fn rule(id: &str, name: &str, priority: i64) -> Rule {
    Rule {
        id: id.to_owned(),
        account_id: "acc_1".to_owned(),
        name: name.to_owned(),
        enabled: true,
        priority,
        stop: false,
        query: "fattura".to_owned(),
        actions_json: r#"[{"kind":"read"}]"#.to_owned(),
        created_at: 1_000,
        updated_at: 1_000,
    }
}

#[test]
fn rules_are_ordered_unique_and_capped_at_one_hundred() {
    let (_fixture, store) = seeded();
    store.create_rule_limited(&rule("rule_2", "B", 0), 100).expect("rule 2");
    store.create_rule_limited(&rule("rule_1", "A", 5), 100).expect("rule 1");
    store.create_rule_limited(&rule("rule_3", "C", 0), 100).expect("rule 3");
    let ids = store.list_rules("acc_1").expect("rules").into_iter().map(|rule| rule.id).collect::<Vec<_>>();
    assert_eq!(ids, ["rule_2", "rule_3", "rule_1"], "priority first, then id (lexicographic)");

    let duplicate = store.create_rule_limited(&rule("rule_4", "a", 0), 100);
    assert!(matches!(duplicate, Err(StoreError::Duplicate { .. })), "names are case-insensitively unique");
    assert_eq!(store.count_rules("acc_1").expect("count"), 3);

    for index in 4..=100 {
        store.create_rule_limited(&rule(&format!("rule_{index}"), &format!("R{index}"), 0), 100).expect("within the cap");
    }
    let over = store.create_rule_limited(&rule("rule_101", "R101", 0), 100);
    assert!(matches!(over, Err(StoreError::RuleLimit { limit: 100 })), "{over:?}");
}

#[test]
fn rule_names_are_unique_with_unicode_lowercase() {
    let (_fixture, store) = seeded();
    store.create_rule_limited(&rule("rule_1", "Città", 0), 100).expect("first");
    let duplicate = store.create_rule_limited(&rule("rule_2", "CITTÀ", 0), 100);
    assert!(matches!(duplicate, Err(StoreError::Duplicate { .. })), "NOCASE alone does not fold Unicode: {duplicate:?}");
}

#[test]
fn candidates_are_received_mail_in_ingestion_order() {
    let (_fixture, store) = seeded();
    let page = store
        .rule_candidates(&RuleCandidateFilter {
            account_id: "acc_1".to_owned(),
            expr: Some(SearchExpr::Match("\"fattura\"".to_owned())),
            since: 0,
            new_only: false,
            after: None,
            limit: 50,
        })
        .expect("candidates");
    assert_eq!(page.total, 2, "drafts, Sent and Trash are never candidates");
    assert_eq!(page.messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), ["msg_inbox", "msg_archive"]);

    store.mark_rules_applied(&["msg_inbox".to_owned()], 10).expect("applied");
    let next = store
        .rule_candidates(&RuleCandidateFilter { account_id: "acc_1".to_owned(), expr: None, since: 0, new_only: true, after: None, limit: 50 })
        .expect("new candidates");
    assert_eq!(next.messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), ["msg_archive"]);

    let after = store
        .rule_candidates(&RuleCandidateFilter { account_id: "acc_1".to_owned(), expr: None, since: 1_500, new_only: false, after: None, limit: 50 })
        .expect("since");
    assert_eq!(after.messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), ["msg_archive"]);
    assert_eq!(SEARCH_TOTAL_CAP, 10_000);
}

#[test]
fn the_journal_keeps_the_newest_twenty_runs_and_their_actions() {
    let (_fixture, store) = seeded();
    for index in 1..=25 {
        let id = store.allocate_rule_run_id().expect("run id");
        store
            .create_rule_run(&RuleRun {
                id: id.clone(),
                account_id: "acc_1".to_owned(),
                started_at: 1_000 + index,
                finished_at: None,
                mode: if index % 2 == 0 { "all".to_owned() } else { "new".to_owned() },
                rule_count: 1,
                matched: 1,
                changed: 1,
                errors: 0,
                undone: false,
            })
            .expect("run");
        store
            .append_rule_action(&RuleActionRecord {
                run_id: id.clone(),
                ordinal: 1,
                rule_id: "rule_1".to_owned(),
                message_id: "msg_inbox".to_owned(),
                action: "move".to_owned(),
                value_json: r#"{"from":"inbox","to":"archive"}"#.to_owned(),
                changed: true,
            })
            .expect("action");
        store.finish_rule_run(&id, 2_000 + index, 1, 1, 0).expect("finish");
    }
    let pruned = store.prune_rule_runs("acc_1", 20).expect("prune");
    assert_eq!(pruned, 5);
    let runs = store.list_rule_runs("acc_1", 20).expect("runs");
    assert_eq!(runs.len(), 20);
    assert_eq!(runs.first().map(|run| run.started_at), Some(1_025));
    assert_eq!(runs.last().map(|run| run.started_at), Some(1_006));
    let newest = runs.first().expect("newest");
    let actions = store.rule_run_actions(&newest.id).expect("actions");
    assert_eq!(actions.len(), 1);
    assert!(actions[0].value_json.contains("archive"));
    assert!(!actions[0].value_json.contains("Fattura"), "the journal must not carry a subject");
    assert!(store.mark_rule_run_undone(&newest.id, 9_000).expect("mark"));
    assert!(!store.mark_rule_run_undone(&newest.id, 9_001).expect("second mark"));
    assert!(store.get_rule_run(&newest.id).expect("get").is_some_and(|run| run.undone));
}

#[test]
fn the_rule_candidate_queries_use_their_covering_indexes() {
    let (fixture, _store) = seeded();
    let raw = fixture.raw();
    let plan = |sql: &str| {
        let mut stmt = raw.prepare(sql).expect("prepare plan");
        stmt.query_map([], |row| row.get::<_, String>(3)).expect("plan rows").collect::<rusqlite::Result<Vec<_>>>().expect("plan")
    };
    let new_plan = plan(
        "EXPLAIN QUERY PLAN
         SELECT m.id FROM messages m JOIN folders f ON f.id = m.folder_id
         WHERE m.account_id = 'acc_1' AND m.origin = 'received' AND m.rules_applied_at IS NULL
           AND m.ingested_at >= 0 AND f.kind NOT IN ('drafts','sent','trash')
         ORDER BY m.ingested_at ASC, m.id ASC LIMIT 2000",
    );
    assert!(new_plan.iter().any(|line| line.contains("messages_rules_new")), "{new_plan:?}");
    let all_plan = plan(
        "EXPLAIN QUERY PLAN
         SELECT m.id FROM messages m JOIN folders f ON f.id = m.folder_id
         WHERE m.account_id = 'acc_1' AND m.origin = 'received' AND m.ingested_at >= 0
           AND (m.ingested_at > 1000 OR (m.ingested_at = 1000 AND m.id > 'msg_inbox'))
           AND f.kind NOT IN ('drafts','sent','trash')
         ORDER BY m.ingested_at ASC, m.id ASC LIMIT 2000",
    );
    assert!(all_plan.iter().any(|line| line.contains("messages_rules_all")), "{all_plan:?}");
}

#[test]
fn bulk_sync_deletion_reset_and_account_removal_clean_loop_guards() {
    let (fixture, store) = seeded();
    store.set_rule_guard("acc_1", "msg_inbox", RuleGuard { auto_submitted: true, x_postino_rule: false }).expect("guard");
    store.delete_synced_messages("inbox", &["msg_inbox".to_owned()]).expect("bulk delete");
    assert!(store.get_meta("rule_guard:acc_1:msg_inbox").expect("meta").is_none());

    store.set_rule_guard("acc_1", "msg_archive", RuleGuard { auto_submitted: false, x_postino_rule: true }).expect("guard");
    store.reset_folder_messages("archive").expect("reset");
    assert!(store.get_meta("rule_guard:acc_1:msg_archive").expect("meta").is_none());

    store.set_rule_guard("acc_1", "msg_trash", RuleGuard { auto_submitted: true, x_postino_rule: true }).expect("guard");
    store.set_rule_forward_marker("acc_1", "rule_1", "msg_trash", "ob_historical").expect("marker");
    store.set_meta("rule_guard:acc_1:orphan", "11").expect("orphan");
    store.delete_account("acc_1").expect("account delete");
    let guards: i64 = fixture.raw().query_row("SELECT COUNT(*) FROM meta WHERE key LIKE 'rule_guard:acc_1:%'", [], |row| row.get(0)).expect("guards");
    assert_eq!(guards, 0, "account removal must sweep orphan guard keys too");
    let markers: i64 =
        fixture.raw().query_row("SELECT COUNT(*) FROM meta WHERE key LIKE 'rule_forward:acc_1:%'", [], |row| row.get(0)).expect("markers");
    assert_eq!(markers, 0, "account removal deletes its durable forward markers");
}

#[test]
fn the_loop_guard_lives_in_encrypted_meta_and_dies_with_the_message() {
    let (fixture, store) = seeded();
    store.set_rule_guard("acc_1", "msg_inbox", RuleGuard { auto_submitted: true, x_postino_rule: false }).expect("guard");
    store.set_rule_guard("acc_1", "msg_archive", RuleGuard { auto_submitted: false, x_postino_rule: true }).expect("guard");
    assert!(store.rule_guard("acc_1", "msg_inbox").expect("read").auto_submitted);
    assert!(store.rule_guard("acc_1", "msg_archive").expect("read").x_postino_rule);
    assert!(!store.rule_guard("acc_1", "msg_draft").expect("read").any());
    assert!(store.get_meta(&format!("{RULE_GUARD_KEY_PREFIX}acc_1:msg_inbox")).expect("meta").is_some());

    store.delete_message("msg_inbox").expect("delete");
    assert!(store.get_meta(&format!("{RULE_GUARD_KEY_PREFIX}acc_1:msg_inbox")).expect("meta").is_none());
    assert_eq!(fixture.raw().query_row("SELECT COUNT(*) FROM meta WHERE key LIKE 'rule_guard:%'", [], |row| row.get::<_, i64>(0)).expect("rows"), 1);
}
