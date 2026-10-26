//! Search performance evidence. The 20k fixture runs in CI; 100k is opt-in with
//! `POSTINO_SLOW_TESTS=1`.
//!
//! It measures the two numbers P9a promises: opening the encrypted store does not read bodies, and
//! the first search page stays under 100 ms on the development host. The test prints the numbers
//! instead of hiding a miss; a missed target is a report item, not a skipped assertion.

use std::path::Path;
use std::time::Instant;

use postino_core::Engine;
use postino_store::{Account, Folder, FolderKind, SearchExpr, SearchFilter, Store, StoreKey};
use rusqlite::{Connection, params};

const BODY_BYTES: usize = 2 * 1024;

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

fn seed(path: &Path, key: &StoreKey, count: u32) {
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
                          '[{\"name\":\"Studio\",\"email\":\"studio@example.com\"}]', '[]', '[]', ?5, ?6, ?7, ?8, 0, ?9,
                          ?10, NULL, 0, ?11, 'Giulia Rossi giulia@example.com',
                          'Studio studio@example.com', '', '', ?12, ?13, ?14)",
            )
            .expect("prepare insert");
        let filler = "parola ".repeat(BODY_BYTES / 8);
        for index in 0..count {
            let id = format!("msg_{index:06}");
            let date = 1_800_000_000_000_i64.saturating_sub(i64::from(index) * 60_000);
            let subject = if index % 10 == 0 { format!("Rapporto mensile {index}") } else { format!("Nota {index}") };
            let body = if index % 10 == 0 { format!("{filler} rapporto unico{index} fine") } else { format!("{filler} nota unica{index} fine") };
            let attachment = if index % 5 == 0 { format!("allegato-{index}.pdf") } else { String::new() };
            insert
                .execute(params![
                    id,
                    i64::from(index),
                    format!("<{index}@example.com>"),
                    format!("thr_{index}"),
                    subject,
                    "anteprima",
                    date,
                    if index % 7 == 0 { 1 } else { 0 },
                    if attachment.is_empty() { 0 } else { 1 },
                    body,
                    body.len() as i64,
                    attachment,
                    "rapporto mensile",
                    date,
                ])
                .expect("insert message");
            if !attachment.is_empty() {
                tx.execute(
                    "INSERT INTO attachments(id, message_id, filename, mime, size, blob_hash) VALUES(?1, ?2, ?3, 'application/pdf', 1024, NULL)",
                    params![format!("att_{index}"), format!("msg_{index:06}"), attachment],
                )
                .expect("insert attachment");
            }
        }
    }
    tx.commit().expect("commit seed");
}

fn timed_search(store: &Store, runs: usize) -> (Vec<f64>, usize) {
    let filter = SearchFilter { expr: Some(SearchExpr::Match("\"rapporto\"".to_owned())), limit: 50, ..SearchFilter::default() };
    let mut times = Vec::with_capacity(runs);
    let mut total = 0;
    for _ in 0..runs {
        let started = Instant::now();
        let page = store.search(&filter).expect("search");
        times.push(started.elapsed().as_secs_f64() * 1_000.0);
        total = page.total as usize;
    }
    times.sort_by(|left, right| left.total_cmp(right));
    (times, total)
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
    seed(&path, &key, count);
    let seed_seconds = seed_started.elapsed().as_secs_f64();

    let open_started = Instant::now();
    let engine = Engine::open(Store::open(&path, &key).expect("open")).expect("engine");
    let open_ms = open_started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(engine.messages().len(), count as usize, "the fixture lost rows");

    let store = Store::open(&path, &key).expect("store");
    let summaries_started = Instant::now();
    let summaries = store.list_all_summaries().expect("summaries");
    eprintln!("{label}: list_all_summaries={} rows {:?}", summaries.len(), summaries_started.elapsed());
    let (times, total) = timed_search(&store, 5);
    let p50 = percentile(&times, 0.50);
    let p95 = percentile(&times, 0.95);
    let date_filter = SearchFilter {
        expr: Some(SearchExpr::Match("\"rapporto\"".to_owned())),
        sort: postino_store::SearchSort::Date,
        limit: 1,
        ..SearchFilter::default()
    };
    let date_started = Instant::now();
    let _ = store.search(&date_filter).expect("date search");
    let date_ms = date_started.elapsed().as_secs_f64() * 1_000.0;
    let reindex_started = Instant::now();
    let reindex = store.search_reindex().expect("reindex");
    let reindex_ms = reindex_started.elapsed().as_secs_f64() * 1_000.0;
    eprintln!(
        "{label}: messages={count} seed={seed_seconds:.1}s open={open_ms:.1}ms search p50={p50:.1}ms p95={p95:.1}ms date-limit1={date_ms:.1}ms reindex={reindex_ms:.1}ms docs={} total={total} times={times:?}",
        reindex.documents
    );
    assert!(p95 < 100.0, "{label}: first-page P95 is {p95:.1} ms, over the 100 ms budget");
    assert_eq!(total, (count as usize).div_ceil(10), "the fixture's matching set changed");
}

#[test]
fn twenty_thousand_messages_search_under_the_first_page_budget() {
    run_fixture(20_000, "20k");
}

#[test]
#[ignore = "slow: set POSTINO_SLOW_TESTS=1 and run --ignored one_hundred_thousand_messages"]
fn one_hundred_thousand_messages() {
    if std::env::var("POSTINO_SLOW_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: POSTINO_SLOW_TESTS is not 1");
        return;
    }
    run_fixture(100_000, "100k");
}
