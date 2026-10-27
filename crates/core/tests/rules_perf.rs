//! P9c rule-run evidence. The 20k fixture runs in CI; 100k is opt-in with
//! `POSTINO_SLOW_TESTS=1`.
//!
//! The fixture seeds a large old mailbox with raw SQL, then creates one rule and inserts 2 000
//! matching messages after its `created_at`. The measured paths are the ones `sync_now` and the UI
//! call: a `new` page with 2 000 actions, an empty `new` page (the common post-sync case), an `all`
//! page for one rule, and an `all` page for ten rules. Numbers are printed and the architecture table
//! is updated from them; this test does not silently claim a target it does not measure.

use std::path::Path;
use std::time::Instant;

use postino_core::Engine;
use postino_store::{Account, Folder, FolderKind, Rule as StoredRule, Store, StoreKey};
use rusqlite::{Connection, params};

fn key() -> StoreKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random");
    StoreKey::from_bytes(bytes)
}

fn raw_open(path: &Path, key: &StoreKey) -> Connection {
    let connection = Connection::open(path).expect("raw open");
    let hex = key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("apply the key");
    connection
}

fn seed_old(path: &Path, key: &StoreKey, count: u32) {
    let store = Store::open(path, key).expect("schema");
    store
        .upsert_account(&Account {
            id: "acc_perf".to_owned(),
            name: "Perf".to_owned(),
            email: "perf@example.com".to_owned(),
            provider: "demo".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    store
        .upsert_folder(&Folder {
            id: "inbox".to_owned(),
            account_id: "acc_perf".to_owned(),
            name: "INBOX".to_owned(),
            kind: FolderKind::Inbox,
            unread: 0,
            total: 0,
            uid_validity: Some(1),
            uid_next: Some(count.saturating_add(1)),
            last_sync_at: None,
        })
        .expect("folder");
    drop(store);
    let mut connection = raw_open(path, key);
    let tx = connection.transaction().expect("seed transaction");
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO messages(
                     id, account_id, folder_id, uid, message_id, thread_id, from_name, from_email,
                     to_json, cc_json, bcc_json, subject, snippet, date, unread, starred,
                     has_attachments, body_text, body_html, remote_images, size,
                     search_sender, search_to, search_cc, search_bcc, attachment_names, subject_norm,
                     ingested_at, origin
                 ) VALUES(?1, 'acc_perf', 'inbox', ?2, ?3, ?4, 'Giulia Rossi', 'giulia@example.com',
                          '[{\"name\":\"Studio\",\"email\":\"studio@example.com\"}]', '[]', '[]', ?5, ?6, ?7, 1, 0,
                          0, ?8, NULL, 0, ?9, 'Giulia Rossi giulia@example.com',
                          'Studio studio@example.com', '', '', '', ?10, 1, 'received')",
            )
            .expect("prepare insert");
        let body = "parola ".repeat(256);
        for index in 0..count {
            let id = format!("old_{index:06}");
            insert
                .execute(params![
                    id,
                    i64::from(index),
                    format!("<old{index}@example.com>"),
                    format!("thr_old_{index}"),
                    format!("Nota vecchia {index}"),
                    "anteprima",
                    1_700_000_000_000_i64 + i64::from(index),
                    body,
                    body.len() as i64,
                    "nota vecchia",
                ])
                .expect("insert old message");
        }
    }
    tx.commit().expect("commit old seed");
}

fn seed_rule_and_new(path: &Path, key: &StoreKey, rules: u32, new_count: u32) -> i64 {
    let store = Store::open(path, key).expect("rule store");
    let created_at = chrono::Utc::now().timestamp_millis();
    for index in 0..rules {
        store
            .create_rule_limited(
                &StoredRule {
                    id: format!("rule_{index}"),
                    account_id: "acc_perf".to_owned(),
                    name: format!("Perf {index}"),
                    enabled: true,
                    priority: i64::from(index),
                    stop: false,
                    query: "subject:Perf".to_owned(),
                    actions_json: r#"[{"kind":"read"}]"#.to_owned(),
                    created_at,
                    updated_at: created_at,
                },
                100,
            )
            .expect("rule");
    }
    drop(store);
    let mut connection = raw_open(path, key);
    let tx = connection.transaction().expect("new seed transaction");
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO messages(
                     id, account_id, folder_id, uid, message_id, thread_id, from_name, from_email,
                     to_json, cc_json, bcc_json, subject, snippet, date, unread, starred,
                     has_attachments, body_text, body_html, remote_images, size,
                     search_sender, search_to, search_cc, search_bcc, attachment_names, subject_norm,
                     ingested_at, origin
                 ) VALUES(?1, 'acc_perf', 'inbox', ?2, ?3, ?4, 'Giulia Rossi', 'giulia@example.com',
                          '[{\"name\":\"Studio\",\"email\":\"studio@example.com\"}]', '[]', '[]', ?5, ?6, ?7, 1, 0,
                          0, ?8, NULL, 0, ?9, 'Giulia Rossi giulia@example.com',
                          'Studio studio@example.com', '', '', '', ?10, ?11, 'received')",
            )
            .expect("prepare new insert");
        let body = "parola ".repeat(256);
        for index in 0..new_count {
            let id = format!("new_{index:05}");
            insert
                .execute(params![
                    id,
                    i64::from(10_000_000 + index),
                    format!("<new{index}@example.com>"),
                    format!("thr_new_{index}"),
                    format!("Perf {index}"),
                    "anteprima",
                    created_at + i64::from(index),
                    body,
                    body.len() as i64,
                    "perf",
                    created_at + 1 + i64::from(index),
                ])
                .expect("insert new message");
        }
    }
    tx.commit().expect("commit new seed");
    created_at
}

fn percentile(times: &[f64], fraction: f64) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    let index = ((times.len() as f64 - 1.0) * fraction).round() as usize;
    times.get(index).copied().unwrap_or(0.0)
}

#[test]
fn rule_runs_are_measured_at_20k_and_100k() {
    let count = if std::env::var("POSTINO_SLOW_TESTS").is_ok() { 100_000 } else { 20_000 };
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("rules.db");
    let key = key();
    seed_old(&path, &key, count - 2_000);
    let first_created = seed_rule_and_new(&path, &key, 1, 2_000);
    let mut engine = Engine::open(Store::open(&path, &key).expect("store")).expect("engine");

    let mut new_actions = Vec::new();
    let mut new_empty = Vec::new();
    let mut all_one = Vec::new();
    for _ in 0..5 {
        {
            let connection = raw_open(&path, &key);
            connection.execute("UPDATE messages SET rules_applied_at = NULL, unread = 1 WHERE id LIKE 'new_%'", []).expect("reset new rows");
            connection.execute("DELETE FROM meta WHERE key LIKE 'rule_all_cursor:%'", []).expect("reset all cursor");
        }
        let started = Instant::now();
        let result = engine.call("rules_run", serde_json::json!({ "scope": "new" })).expect("new run");
        new_actions.push(started.elapsed().as_secs_f64() * 1_000.0);
        assert!(result["run"]["matched"].as_u64().is_some_and(|matched| matched >= 2_000), "{result}");
        let started = Instant::now();
        let result = engine.call("rules_run", serde_json::json!({ "scope": "new" })).expect("empty new run");
        new_empty.push(started.elapsed().as_secs_f64() * 1_000.0);
        assert_eq!(result["run"]["matched"], 0, "{result}");
        let started = Instant::now();
        let result = engine.call("rules_run", serde_json::json!({ "scope": "all" })).expect("all run");
        all_one.push(started.elapsed().as_secs_f64() * 1_000.0);
        assert_eq!(result["run"]["matched"], 2_000, "{result}");
    }

    {
        let store = Store::open(&path, &key).expect("extra rules");
        let created_at = first_created;
        for index in 1..10 {
            store
                .create_rule_limited(
                    &StoredRule {
                        id: format!("rule_{index}"),
                        account_id: "acc_perf".to_owned(),
                        name: format!("Perf extra {index}"),
                        enabled: true,
                        priority: i64::from(index),
                        stop: false,
                        query: "subject:Perf".to_owned(),
                        actions_json: r#"[{"kind":"read"}]"#.to_owned(),
                        created_at,
                        updated_at: created_at,
                    },
                    100,
                )
                .expect("extra rule");
        }
    }
    let mut all_ten = Vec::new();
    for _ in 0..3 {
        raw_open(&path, &key).execute("DELETE FROM meta WHERE key LIKE 'rule_all_cursor:%'", []).expect("reset all cursor");
        let started = Instant::now();
        let result = engine.call("rules_run", serde_json::json!({ "scope": "all" })).expect("ten-rule all run");
        all_ten.push(started.elapsed().as_secs_f64() * 1_000.0);
        assert!(result["run"]["matched"].as_u64().is_some_and(|matched| matched >= 20_000), "{result}");
    }

    println!(
        "rules perf {}k: new+2000 P50 {:.1} ms P95 {:.1} ms; new-empty P50 {:.1} ms P95 {:.1} ms; all-1 P50 {:.1} ms P95 {:.1} ms; all-10 P50 {:.1} ms P95 {:.1} ms",
        count / 1_000,
        percentile(&new_actions, 0.5),
        percentile(&new_actions, 0.95),
        percentile(&new_empty, 0.5),
        percentile(&new_empty, 0.95),
        percentile(&all_one, 0.5),
        percentile(&all_one, 0.95),
        percentile(&all_ten, 0.5),
        percentile(&all_ten, 0.95),
    );
}
