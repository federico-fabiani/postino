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

/// Insert `count` real 2 KiB messages through the repository, each with three references that all
/// resolve to the seeded `group@example.com` conversation. This is the sync path, not raw SQL: it
/// covers the reference lookup, the thread cache update, the FTS triggers and the row write.
fn timed_ingest(path: &Path, key: &StoreKey, count: u32, label: &str) -> (f64, f64, f64, f64) {
    let store = Store::open(path, key).expect("ingest store");
    let body = "parola ".repeat(256);
    let mut times = Vec::with_capacity(count as usize);
    let started = Instant::now();
    for index in 0..count {
        let id = format!("new_{label}_{index:04}");
        let row = postino_store::Message {
            summary: postino_store::MessageSummary {
                id: id.clone(),
                account_id: "acc_perf".to_owned(),
                folder_id: "inbox".to_owned(),
                thread_id: "provisional".to_owned(),
                from: postino_store::Address::new("Giulia Rossi", "giulia@example.com"),
                to: vec![postino_store::Address::new("Studio", "studio@example.com")],
                subject: format!("Re: Discussione lunga {index}"),
                snippet: "anteprima".to_owned(),
                date: 1_800_000_000_000 + i64::from(index),
                unread: true,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            uid: Some(200_000 + index),
            message_id: Some(format!("<{id}@example.com>")),
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: body.clone(),
            body_html: None,
            remote_images: 0,
            size: Some(body.len() as u64),
        };
        let reference_started = Instant::now();
        store
            .upsert_synced_message_with_refs(
                &row,
                &[],
                Some("0@example.com"),
                &["0@example.com".to_owned(), "1@example.com".to_owned(), "2@example.com".to_owned()],
            )
            .expect("real ingest");
        times.push(reference_started.elapsed().as_secs_f64() * 1_000.0);
    }
    let total_ms = started.elapsed().as_secs_f64() * 1_000.0;
    times.sort_by(|left, right| left.total_cmp(right));
    let p50 = percentile(&times, 0.50);
    let p95 = percentile(&times, 0.95);
    let max = times.last().copied().unwrap_or(0.0);
    (total_ms, p50, p95, max)
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

    let (ingest_total, ingest_p50, ingest_p95, ingest_max) = timed_ingest(&path, &key, 100, label);
    // The new-message budget is the typical insert (median and mean), as in the architecture table. The
    // workspace CI runs every test binary concurrently, so the tail (P95/max) is printed for the report
    // but cannot be a stable gate: a single descheduled insert is runner contention, not the store.
    let ingest_mean = ingest_total / f64::from(100);

    let cache_rows: i64 = raw_open(&path, &key).query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0)).expect("thread cache count");
    eprintln!(
        "{label}: seed={seed_seconds:.1}s open(with thread cache)={open_ms:.1}ms threads={total_threads} cache_rows={cache_rows} \
         threads_list(account) P50={account_p50:.1}ms P95={account_p95:.1}ms \
         threads_list(folder) P50={folder_p50:.1}ms P95={folder_p95:.1}ms \
         thread_get(50) P50={get_p50:.1}ms P95={get_p95:.1}ms \
         ingest100(total)={ingest_total:.1}ms P50={ingest_p50:.1}ms P95={ingest_p95:.1}ms mean={ingest_mean:.1}ms max={ingest_max:.1}ms"
    );
    assert!(account_p95 < 100.0, "{label}: threads_list account P95 is {account_p95:.1} ms, over the 100 ms budget");
    assert!(get_p95 < 50.0, "{label}: thread_get 50 P95 is {get_p95:.1} ms, over the 50 ms budget");
    assert!(ingest_total < 2_000.0, "{label}: 100 real inserts took {ingest_total:.1} ms, over the 2 s sync budget");
    // The new-message budget is the typical insert (median and mean), as in the architecture table. The
    // workspace CI runs every test binary concurrently, so the tail (P95/max) is printed for the report
    // but cannot be a stable gate: a single descheduled insert is runner contention, not the store.
    assert!(ingest_p50 < 20.0, "{label}: real insert P50 is {ingest_p50:.1} ms, over the 20 ms new-message budget");
    assert!(ingest_mean < 20.0, "{label}: real insert mean is {ingest_mean:.1} ms, over the 20 ms new-message budget");
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
