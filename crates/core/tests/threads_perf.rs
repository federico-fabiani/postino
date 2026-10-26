//! P9b conversation-list evidence. The 20k fixture runs in CI; 100k is opt-in with
//! `POSTINO_SLOW_TESTS=1`.
//!
//! The fixture writes message rows with raw SQL, exactly like a P9a store: the thread cache is empty
//! and `Engine::open` has to build it once. The timed reads then measure the cache the UI actually
//! hits. Numbers are printed; a missed target fails the test and belongs in the report.

use std::path::Path;
use std::time::Instant;

use postino_core::Engine;
use postino_store::{Account, Folder, FolderKind, Store, StoreKey};
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

/// `grouped` messages share one thread (`group@example.com`); every other message is its own root.
fn seed(path: &Path, key: &StoreKey, count: u32, grouped: u32) {
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
    connection.execute_batch("PRAGMA foreign_keys = ON;").expect("foreign keys");
    let tx = connection.transaction().expect("seed transaction");
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO messages(
                     id, account_id, folder_id, uid, message_id, thread_id, from_name, from_email,
                     to_json, cc_json, bcc_json, subject, snippet, date, unread, starred,
                     has_attachments, body_text, body_html, remote_images, size,
                     search_sender, search_to, search_cc, search_bcc, attachment_names, subject_norm, ingested_at
                 ) VALUES(?1, 'acc_perf', 'inbox', ?2, ?3, ?4, 'Giulia Rossi', 'giulia@example.com',
                          '[{\"name\":\"Studio\",\"email\":\"studio@example.com\"}]', '[]', '[]', ?5, ?6, ?7, ?8, 0, 0,
                          ?9, NULL, 0, ?10, 'Giulia Rossi giulia@example.com',
                          'Studio studio@example.com', '', '', ?11, ?12, ?13)",
            )
            .expect("prepare insert");
        let body = "parola ".repeat(256);
        for index in 0..count {
            let id = format!("msg_{index:06}");
            let date = 1_700_000_000_000_i64.saturating_add(i64::from(index) * 60_000);
            let grouped_message = index < grouped;
            let thread = if grouped_message { "group@example.com".to_owned() } else { format!("thr_{index}") };
            let subject = if grouped_message { "Re: Discussione lunga".to_owned() } else { format!("Nota {index}") };
            insert
                .execute(params![
                    id,
                    i64::from(index),
                    format!("<{index}@example.com>"),
                    thread,
                    subject,
                    "anteprima",
                    date,
                    0,
                    body,
                    body.len() as i64,
                    "",
                    "discussione lunga",
                    date,
                ])
                .expect("insert message");
        }
    }
    tx.commit().expect("commit seed");
}

fn percentile(times: &[f64], fraction: f64) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    let index = ((times.len() as f64 - 1.0) * fraction).round() as usize;
    times.get(index).copied().unwrap_or(0.0)
}

fn run_fixture(count: u32, label: &str) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mail.db");
    let key = key();
    let seed_started = Instant::now();
    seed(&path, &key, count, 50);
    let seed_seconds = seed_started.elapsed().as_secs_f64();

    let open_started = Instant::now();
    let mut engine = Engine::open(Store::open(&path, &key).expect("open")).expect("engine");
    let open_ms = open_started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(engine.messages().len(), count as usize, "the fixture lost rows");

    let mut account_times = Vec::new();
    let mut folder_times = Vec::new();
    let mut total_threads = 0u32;
    for _ in 0..5 {
        let started = Instant::now();
        let listed = engine.call("threads_list", serde_json::json!({ "limit": 50 })).expect("threads_list");
        account_times.push(started.elapsed().as_secs_f64() * 1_000.0);
        total_threads = u32::try_from(listed["total"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);

        let started = Instant::now();
        let _ = engine.call("threads_list", serde_json::json!({ "folder": "inbox", "limit": 50 })).expect("folder threads");
        folder_times.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    account_times.sort_by(|left, right| left.total_cmp(right));
    folder_times.sort_by(|left, right| left.total_cmp(right));
    let account_p50 = percentile(&account_times, 0.50);
    let account_p95 = percentile(&account_times, 0.95);
    let folder_p50 = percentile(&folder_times, 0.50);
    let folder_p95 = percentile(&folder_times, 0.95);

    let mut thread_get_times = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let detail = engine.call("thread_get", serde_json::json!({ "id": "group@example.com", "limit": 50 })).expect("thread_get");
        thread_get_times.push(started.elapsed().as_secs_f64() * 1_000.0);
        assert_eq!(detail["messages"].as_array().map(Vec::len), Some(50), "the grouped thread lost messages");
    }
    thread_get_times.sort_by(|left, right| left.total_cmp(right));
    let get_p50 = percentile(&thread_get_times, 0.50);
    let get_p95 = percentile(&thread_get_times, 0.95);

    let cache_rows: i64 = raw_open(&path, &key).query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0)).expect("thread cache count");
    eprintln!(
        "{label}: seed={seed_seconds:.1}s open(with thread cache)={open_ms:.1}ms threads={total_threads} cache_rows={cache_rows} \
         threads_list(account) P50={account_p50:.1}ms P95={account_p95:.1}ms \
         threads_list(folder) P50={folder_p50:.1}ms P95={folder_p95:.1}ms \
         thread_get(50) P50={get_p50:.1}ms P95={get_p95:.1}ms"
    );
    assert!(account_p95 < 100.0, "{label}: threads_list account P95 is {account_p95:.1} ms, over the 100 ms budget");
    assert!(get_p95 < 50.0, "{label}: thread_get 50 P95 is {get_p95:.1} ms, over the 50 ms budget");
    // The folder view is reported, not gated: it crosses the covering message index to decide which
    // conversations are in scope, and the brief's budget is measured on the account-wide list.
    let _ = (folder_p50, folder_p95);
}

#[test]
fn twenty_thousand_threads_list_under_the_budget() {
    run_fixture(20_000, "20k");
}

#[test]
#[ignore = "slow: set POSTINO_SLOW_TESTS=1 and run --ignored one_hundred_thousand_threads"]
fn one_hundred_thousand_threads() {
    if std::env::var("POSTINO_SLOW_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: POSTINO_SLOW_TESTS is not 1");
        return;
    }
    run_fixture(100_000, "100k");
}
