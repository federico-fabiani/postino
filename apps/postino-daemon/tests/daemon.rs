//! End-to-end daemon tests through the real local transport. Every test starts a real daemon on a
//! temporary store; no test touches the OS keyring or the default data directory.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use postino_daemon::Daemon;
use postino_ipc::client::Client;
use postino_ipc::protocol::method;
use postino_ipc::{IpcError, SharedToken, TokenFile};
use postino_store::{OutboxEntry, OutboxState, Store};
use serde_json::{Value, json};
use support::{ACCOUNT, Fixture, MockVault, seed_blobs, seed_messages, seed_outbox_state, wait_for};

async fn op(client: &mut Client, name: &str, args: Value) -> Value {
    client
        .call(method::OP, json!({ "name": name, "args": args }), Duration::from_secs(15))
        .await
        .unwrap_or_else(|error| panic!("operation {name} failed: {error}"))
}

fn draft_id(value: &Value) -> String {
    value["draft"]["summary"]["id"].as_str().expect("draft id").to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_mutate_one_store_through_the_daemon() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    let daemon = support::start(&fixture).await;
    let mut first = fixture.client("client-a").await;
    let mut second = fixture.client("client-b").await;
    let one = op(
        &mut first,
        "draft_save",
        json!({
            "account": ACCOUNT,
            "to": [{ "name": "", "email": "one@example.com" }],
            "subject": "From A",
            "body_text": "first client"
        }),
    )
    .await;
    let two = op(
        &mut second,
        "draft_save",
        json!({
            "account": ACCOUNT,
            "to": [{ "name": "", "email": "two@example.com" }],
            "subject": "From B",
            "body_text": "second client"
        }),
    )
    .await;
    let one = draft_id(&one);
    let two = draft_id(&two);
    assert_ne!(one, two);
    drop(first);
    drop(second);
    daemon.shutdown().expect("shutdown");
    let store = Store::open_read_only(&fixture.store_path, &fixture.key).expect("read-only store");
    assert!(store.get_message(&one).expect("one").is_some());
    assert!(store.get_message(&two).expect("two").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_daemons_cannot_own_one_store() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    let first = support::start(&fixture).await;
    match Daemon::spawn(fixture.config()) {
        Err(postino_daemon::DaemonError::Store(postino_store::StoreError::StoreBusy { .. })) => {}
        Err(error) => panic!("the second daemon failed for another reason: {error}"),
        Ok(second) => {
            second.shutdown().expect("shutdown second");
            panic!("two daemons owned one store");
        }
    }
    first.shutdown().expect("shutdown first");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_owns_the_store_and_a_direct_writer_is_refused() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    let daemon = support::start(&fixture).await;
    match Store::open_locked(&fixture.store_path, &fixture.key) {
        Err(postino_store::StoreError::StoreBusy { .. }) => {}
        other => panic!("the daemon did not hold the writer lock: {}", other.err().map(|error| error.to_string()).unwrap_or_default()),
    }
    drop(daemon);
    Store::open_locked(&fixture.store_path, &fixture.key).expect("the lock is released on shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_rotates_the_token_and_refuses_the_stale_one() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    let first = support::start(&fixture).await;
    let first_token = std::fs::read_to_string(first.token_file()).expect("token file").trim().to_owned();
    first.shutdown().expect("shutdown");
    let second = support::start(&fixture).await;
    let second_token = std::fs::read_to_string(second.token_file()).expect("token file").trim().to_owned();
    assert_ne!(first_token, second_token);
    TokenFile::new(second.token_file().to_path_buf()).write(&SharedToken::from_hex(&first_token).expect("token")).expect("stale token");
    let refused = Client::connect(&fixture.paths(), "stale", Duration::from_secs(2), Duration::from_secs(2)).await;
    assert!(matches!(refused, Err(IpcError::Unauthorized)), "{refused:?}");
    TokenFile::new(second.token_file().to_path_buf()).write(&SharedToken::from_hex(&second_token).expect("token")).expect("restore token");
    second.shutdown().expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scheduled_message_is_sent_with_the_app_closed() {
    let tls = postino_testkit::TlsFixture::generate();
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let handler_capture = Arc::clone(&captured);
    let smtp = postino_testkit::FakeSmtp::start_tls(&tls, move |mut conn| {
        let captured = Arc::clone(&handler_capture);
        async move {
            conn.greet(&["SIZE 1024000", "AUTH PLAIN LOGIN"]).await;
            conn.auth_plain_ok(&postino_testkit::expected_smtp_sasl()).await;
            loop {
                let line = conn.read_line().await;
                if line == "DATA" {
                    conn.send_line("354 go").await;
                    let data = conn.read_data().await;
                    captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(data);
                    conn.send_line("250 queued").await;
                    break;
                }
                conn.send_line("250 ok").await;
            }
            conn.expect("QUIT").await;
            conn.send_line("221 bye").await;
        }
    })
    .await;
    let fixture = Fixture::new().with_vault(MockVault::with(postino_testkit::PASSWORD, Some(postino_testkit::SMTP_PASSWORD)));
    fixture.seed(1, Some((smtp.port(), false)));
    let mut config = fixture.config();
    config.extra_roots = vec![tls.ca()];
    let daemon = Daemon::spawn(config).expect("daemon");
    let mut client = fixture.client("scheduler").await;
    let draft = op(
        &mut client,
        "draft_save",
        json!({
            "account": ACCOUNT,
            "to": [{ "name": "", "email": "dest@example.com" }],
            "subject": "Scheduled with the app closed",
            "body_text": "This message must leave while no client is connected."
        }),
    )
    .await;
    let draft = draft_id(&draft);
    let send_at = (Utc::now() + chrono::Duration::milliseconds(1500)).to_rfc3339_opts(SecondsFormat::Millis, true);
    op(&mut client, "send_schedule", json!({ "draft": draft, "send_at": send_at })).await;
    drop(client);
    let sent = wait_for(Duration::from_secs(12), || !captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_empty()).await;
    daemon.shutdown().expect("shutdown");
    smtp.finish().await;
    assert!(sent, "the daemon did not submit the scheduled message while no app was open");
    let data = captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(String::from_utf8_lossy(&data[0]).contains("Scheduled with the app closed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_run_after_a_sync_and_mark_the_new_message() {
    let tls = postino_testkit::TlsFixture::generate();
    let folders = vec![
        postino_testkit::TestFolder::new("INBOX", 1)
            .with_messages(vec![postino_testkit::TestMessage::synthetic(postino_testkit::synthetic_message(1))]),
    ];
    let server = postino_testkit::FakeServer::start_tls(&tls, move |conn| {
        let folders = folders.clone();
        async move {
            let _ = postino_testkit::serve_mailbox(conn, folders).await;
        }
    })
    .await;
    let fixture = Fixture::new().with_vault(MockVault::with(postino_testkit::PASSWORD, None));
    fixture.seed(server.port(), None);
    let mut config = fixture.config();
    config.extra_roots = vec![tls.ca()];
    let daemon = Daemon::spawn(config).expect("daemon");
    let mut client = fixture.client("rules").await;
    op(
        &mut client,
        "rules_create",
        json!({
            "account": ACCOUNT,
            "name": "Mark synthetic",
            "query": "subject:\"Synthetic message 1\"",
            "actions": [{ "kind": "read" }]
        }),
    )
    .await;
    let report = op(&mut client, "sync_now", json!({ "account": ACCOUNT })).await;
    assert_eq!(report["fetched"], 1, "{report}");
    let folders = op(&mut client, "folders_list", json!({ "account": ACCOUNT })).await;
    let inbox = folders["folders"]
        .as_array()
        .and_then(|folders| folders.iter().find(|folder| folder["name"] == "INBOX"))
        .and_then(|folder| folder["id"].as_str())
        .expect("synced INBOX")
        .to_owned();
    let list = op(&mut client, "messages_list", json!({ "folder": inbox })).await;
    let messages = list["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "{list}");
    assert_eq!(messages[0]["unread"], false, "the rule did not mark the message read: {list}");
    let runs = op(&mut client, "rules_runs", json!({})).await;
    assert!(runs["runs"].as_array().is_some_and(|runs| !runs.is_empty()), "{runs}");
    drop(client);
    daemon.shutdown().expect("shutdown");
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_index_advances_in_batches_and_reports_progress() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    seed_messages(&fixture, 1200);
    let daemon = support::start(&fixture).await;
    let mut client = fixture.client("index").await;
    client.subscribe(Duration::from_secs(2)).await.expect("subscribe");
    let mut saw_batch = false;
    let mut saw_ready = false;
    for _ in 0..40 {
        let Some(event) = client.next_event(Duration::from_millis(500)).await.expect("event") else { continue };
        if event["kind"] == "index" {
            let indexed = event["data"]["indexed"].as_u64().unwrap_or(0);
            let total = event["data"]["total"].as_u64().unwrap_or(0);
            if total == 1200 && indexed < total {
                saw_batch = true;
            }
            if event["data"]["ready"] == true {
                saw_ready = true;
                break;
            }
        }
    }
    assert!(saw_batch || saw_ready, "the daemon emitted no index progress");
    let status = client.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(10)).await.expect("status");
    assert_eq!(status["index"]["total"], 1200, "{status}");
    assert_eq!(status["index"]["ready"], true, "{status}");
    drop(client);
    daemon.shutdown().expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_gc_removes_only_the_orphan() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    let (referenced, orphan) = seed_blobs(&fixture);
    let mut config = fixture.config();
    config.gc_interval = Duration::from_millis(200);
    let daemon = Daemon::spawn(config).expect("daemon");
    let mut client = fixture.client("gc").await;
    client.subscribe(Duration::from_secs(2)).await.expect("subscribe");
    let mut removed = false;
    for _ in 0..30 {
        let Some(event) = client.next_event(Duration::from_millis(300)).await.expect("event") else { continue };
        if event["kind"] == "gc" && event["data"]["removed"].as_u64().unwrap_or(0) >= 1 {
            removed = true;
            break;
        }
    }
    assert!(removed, "the daemon did not run a bounded GC batch");
    drop(client);
    daemon.shutdown().expect("shutdown");
    let store = Store::open_read_only(&fixture.store_path, &fixture.key).expect("read-only store");
    assert!(store.blobs().has(&referenced).expect("referenced"));
    assert!(!store.blobs().has(&orphan).expect("orphan"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_mcp_session_without_allow_send_cannot_forward_during_sync() {
    let tls = postino_testkit::TlsFixture::generate();
    let folders = vec![
        postino_testkit::TestFolder::new("INBOX", 1)
            .with_messages(vec![postino_testkit::TestMessage::synthetic(postino_testkit::synthetic_message(1))]),
    ];
    let server = postino_testkit::FakeServer::start_tls(&tls, move |conn| {
        let folders = folders.clone();
        async move {
            let _ = postino_testkit::serve_mailbox(conn, folders).await;
        }
    })
    .await;
    let smtp_capture: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let smtp_handler = Arc::clone(&smtp_capture);
    let smtp = postino_testkit::FakeSmtp::start_tls(&tls, move |mut conn| {
        let captured = Arc::clone(&smtp_handler);
        async move {
            conn.greet(&["SIZE 1024000", "AUTH PLAIN LOGIN"]).await;
            conn.auth_plain_ok(&postino_testkit::expected_smtp_sasl()).await;
            loop {
                let Some(line) = conn.read_line_opt().await else { break };
                if line == "DATA" {
                    conn.send_line("354 go").await;
                    let data = conn.read_data().await;
                    captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(data);
                    conn.send_line("250 queued").await;
                    break;
                }
                conn.send_line("250 ok").await;
            }
        }
    })
    .await;
    let fixture = Fixture::new().with_vault(MockVault::with(postino_testkit::PASSWORD, Some(postino_testkit::PASSWORD)));
    fixture.seed(server.port(), Some((smtp.port(), false)));
    let mut config = fixture.config();
    config.extra_roots = vec![tls.ca()];
    let daemon = Daemon::spawn(config).expect("daemon");
    let paths = fixture.paths();
    let mut admin = Client::connect(&paths, "admin", Duration::from_secs(2), Duration::from_secs(10)).await.expect("admin");
    op(
        &mut admin,
        "rules_create",
        json!({
            "account": ACCOUNT,
            "name": "Forward synthetic",
            "query": "subject:\"Synthetic message 1\"",
            "actions": [{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]
        }),
    )
    .await;
    let mut mcp = Client::connect_with(&paths, "mcp", false, Duration::from_secs(2), Duration::from_secs(10)).await.expect("mcp");
    let report = op(&mut mcp, "sync_now", json!({ "account": ACCOUNT })).await;
    assert_eq!(report["fetched"], 1, "{report}");
    let outbox = op(&mut mcp, "outbox_list", json!({})).await;
    assert!(outbox["entries"].as_array().is_some_and(Vec::is_empty), "an MCP sync without --allow-send queued a forward: {outbox}");
    let mut allowed = Client::connect_with(&paths, "mcp", true, Duration::from_secs(2), Duration::from_secs(10)).await.expect("mcp allowed");
    op(&mut allowed, "rules_run", json!({ "account": ACCOUNT, "scope": "all" })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let outbox = op(&mut allowed, "outbox_list", json!({})).await;
    assert!(outbox["entries"].as_array().is_some_and(|entries| entries.len() == 1), "an allow-send rule run did not queue the forward: {outbox}");
    drop(mcp);
    drop(allowed);
    drop(admin);
    daemon.shutdown().expect("shutdown");
    smtp.finish().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disconnect_during_sync_releases_the_active_sync_marker() {
    let tls = postino_testkit::TlsFixture::generate();
    let messages: Vec<postino_testkit::TestMessage> =
        postino_testkit::synthetic_messages(20).into_iter().map(postino_testkit::TestMessage::synthetic).collect();
    let folders = vec![postino_testkit::TestFolder::new("INBOX", 1).with_messages(messages)];
    let server = postino_testkit::FakeServer::start_tls_many(&tls, 3, move |conn| {
        let folders = folders.clone();
        async move {
            let _ = postino_testkit::serve_mailbox(conn, folders).await;
        }
    })
    .await;
    let fixture = Fixture::new().with_vault(MockVault::with(postino_testkit::PASSWORD, None));
    fixture.seed(server.port(), None);
    let mut config = fixture.config();
    config.extra_roots = vec![tls.ca()];
    config.sync_chunk_messages = 20;
    let daemon = Daemon::spawn(config).expect("daemon");
    let mut client = fixture.client("drop").await;
    let _id = client.start_request(method::OP, json!({ "name": "sync_now", "args": { "account": ACCOUNT } })).await.expect("sync");
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(client);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut fresh = fixture.client("fresh").await;
    let status = fresh.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(10)).await.expect("status");
    assert!(status["accounts_syncing"].as_array().is_some_and(Vec::is_empty), "the disconnected sync leaked its active marker: {status}");
    let report = op(&mut fresh, "sync_now", json!({ "account": ACCOUNT })).await;
    assert!(report["account"].is_string(), "the engine stuck after a disconnect: {report}");
    drop(fresh);
    daemon.shutdown().expect("shutdown");
    drop(server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_during_a_sync_is_bounded_and_releases_the_lock() {
    let tls = postino_testkit::TlsFixture::generate();
    let messages: Vec<postino_testkit::TestMessage> =
        postino_testkit::synthetic_messages(100).into_iter().map(postino_testkit::TestMessage::synthetic).collect();
    let folders = vec![postino_testkit::TestFolder::new("INBOX", 1).with_messages(messages)];
    let server = postino_testkit::FakeServer::start_tls(&tls, move |conn| {
        let folders = folders.clone();
        async move {
            let _ = postino_testkit::serve_mailbox(conn, folders).await;
        }
    })
    .await;
    let fixture = Fixture::new().with_vault(MockVault::with(postino_testkit::PASSWORD, None));
    fixture.seed(server.port(), None);
    let mut config = fixture.config();
    config.extra_roots = vec![tls.ca()];
    config.sync_on_start = true;
    config.sync_chunk_messages = 1;
    let daemon = Daemon::spawn(config).expect("daemon");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = std::time::Instant::now();
    daemon.shutdown().expect("shutdown");
    assert!(started.elapsed() < Duration::from_secs(10), "shutdown took {:?}", started.elapsed());
    assert!(postino_store::writer_lock_is_free(&fixture.store_path), "the shutdown kept the writer lock");
    drop(server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_due_outbox_row_runs_between_sync_chunks() {
    let tls = postino_testkit::TlsFixture::generate();
    let messages: Vec<postino_testkit::TestMessage> =
        postino_testkit::synthetic_messages(30).into_iter().map(postino_testkit::TestMessage::synthetic).collect();
    let folders = vec![postino_testkit::TestFolder::new("INBOX", 1).with_messages(messages)];
    let imap = postino_testkit::FakeServer::start_tls(&tls, move |conn| {
        let folders = folders.clone();
        async move {
            let _ = postino_testkit::serve_mailbox(conn, folders).await;
        }
    })
    .await;
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let handler_capture = Arc::clone(&captured);
    let smtp = postino_testkit::FakeSmtp::start_tls(&tls, move |mut conn| {
        let captured = Arc::clone(&handler_capture);
        async move {
            conn.greet(&["SIZE 1024000", "AUTH PLAIN LOGIN"]).await;
            conn.auth_plain_ok(&postino_testkit::expected_smtp_sasl()).await;
            loop {
                let line = conn.read_line().await;
                if line == "DATA" {
                    conn.send_line("354 go").await;
                    let data = conn.read_data().await;
                    captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(data);
                    conn.send_line("250 queued").await;
                    break;
                }
                conn.send_line("250 ok").await;
            }
            conn.expect("QUIT").await;
            conn.send_line("221 bye").await;
        }
    })
    .await;
    let fixture = Fixture::new().with_vault(MockVault::with(postino_testkit::PASSWORD, Some(postino_testkit::SMTP_PASSWORD)));
    fixture.seed(imap.port(), Some((smtp.port(), false)));
    seed_outbox_state(&fixture, "during_sync", "queued_future");
    {
        let store = Store::open(&fixture.store_path, &fixture.key).expect("outbox store");
        store.set_outbox_due("during_sync", 1).expect("make due");
    }
    let mut config = fixture.config();
    config.extra_roots = vec![tls.ca()];
    config.sync_on_start = true;
    config.sync_chunk_messages = 1;
    config.outbox_on_start = false;
    let daemon = Daemon::spawn(config).expect("daemon");
    let sent = wait_for(Duration::from_secs(20), || !captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_empty()).await;
    daemon.shutdown().expect("shutdown");
    smtp.finish().await;
    imap.finish().await;
    assert!(sent, "the due outbox row waited for the whole sync instead of running between chunks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scheduler_does_not_spin_when_idle() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    let mut config = fixture.config();
    config.sync_interval = Duration::from_secs(3600);
    config.gc_interval = Duration::from_secs(3600);
    let daemon = Daemon::spawn(config).expect("daemon");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut client = fixture.client("idle").await;
    let status = client.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(5)).await.expect("status");
    let wakes = status["scheduler_wakes"].as_u64().unwrap_or(u64::MAX);
    assert!(wakes <= 5, "the scheduler woke {wakes} times in two idle seconds: {status}");
    drop(client);
    daemon.shutdown().expect("shutdown");
}

fn outbox_row(store: &Store, id: &str, send_at: i64) -> OutboxEntry {
    let raw = store.blobs().put(id.as_bytes()).expect("outbox blob");
    OutboxEntry {
        id: id.to_owned(),
        account_id: ACCOUNT.to_owned(),
        draft_id: None,
        idempotency_key: format!("key-{id}"),
        created_at: Utc::now().timestamp_millis(),
        send_at,
        state: OutboxState::Queued,
        attempts: 0,
        append_attempts: 0,
        last_error: None,
        raw_blob_hash: raw,
        envelope_json: format!(
            r#"{{"from":"user@example.com","recipients":["dest@example.com"],"subject":"{id}","message_id":"<{id}@example.com>"}}"#
        ),
        lease_at: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_at_every_p7_crash_point_keeps_rows_and_never_resends_uncertain_mail() {
    let fixture = Fixture::new();
    fixture.seed(1, None);
    let smtp_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("SMTP trap");
    smtp_listener.set_nonblocking(true).expect("nonblocking");
    let smtp_port = smtp_listener.local_addr().expect("port").port();
    // Point the account at the trap so any submission attempt would be accepted and visible.
    {
        let store = Store::open(&fixture.store_path, &fixture.key).expect("seed store");
        let mut server = store.get_account_server(ACCOUNT).expect("read").expect("server");
        server.smtp = Some(postino_store::SmtpServer {
            host: "127.0.0.1".to_owned(),
            port: smtp_port,
            tls: postino_store::AccountTls::Implicit,
            username: "user@example.com".to_owned(),
        });
        store.upsert_account_server(&server).expect("smtp endpoint");
        let now = Utc::now().timestamp_millis();
        let lease_old = now - postino_core::outbox::LEASE_MS - 1_000;
        let lease_now = now;
        let states = ["queued_future", "sending_fresh", "sending_stale", "append_pending", "uncertain", "failed", "sent", "cancelled"];
        for id in states {
            let entry = outbox_row(&store, id, if id == "queued_future" { now + 3_600_000 } else { 1 });
            store.queue_outbox(&entry).expect("queue");
            match id {
                "sending_fresh" => {
                    assert!(store.lease_next_outbox(lease_now).expect("lease").is_some());
                }
                "sending_stale" => {
                    assert!(store.lease_next_outbox(lease_old).expect("lease").is_some());
                }
                "append_pending" => {
                    let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
                    let lease = leased.lease_at.unwrap_or(lease_now);
                    assert!(store.mark_outbox_append_pending(&leased.id, lease, postino_core::outbox::APPEND_LEASE_MS).expect("append"));
                }
                "uncertain" => {
                    let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
                    let lease = leased.lease_at.unwrap_or(lease_now);
                    assert!(store.mark_outbox_uncertain(&leased.id, "crash", lease).expect("uncertain"));
                }
                "failed" => {
                    let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
                    let lease = leased.lease_at.unwrap_or(lease_now);
                    assert!(store.mark_outbox_failed(&leased.id, "failed", lease).expect("failed"));
                }
                "sent" => {
                    let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
                    let lease = leased.lease_at.unwrap_or(lease_now);
                    assert!(store.mark_outbox_uncertain(&leased.id, "delivered", lease).expect("uncertain"));
                    assert!(store.confirm_outbox_sent(&leased.id).expect("sent"));
                }
                "cancelled" => {
                    assert!(store.cancel_outbox(id).expect("cancel"));
                }
                _ => {}
            }
        }
    }
    let daemon = support::start(&fixture).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    // No submission may reach the trap: none of these rows is a due queued row, and an expired
    // sending lease becomes uncertain instead of being resent.
    match smtp_listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("the daemon connected to SMTP for a crash-state outbox row"),
        Err(error) => panic!("SMTP trap failed: {error}"),
    }
    daemon.shutdown().expect("shutdown");
    let store = Store::open_read_only(&fixture.store_path, &fixture.key).expect("read-only store");
    let rows = store.list_outbox(None, 500).expect("outbox");
    assert_eq!(rows.len(), 8, "a restart deleted an outbox row: {rows:?}");
    let state = |id: &str| rows.iter().find(|row| row.id == id).map(|row| row.state);
    assert_eq!(state("queued_future"), Some(OutboxState::Queued));
    assert_eq!(state("sending_fresh"), Some(OutboxState::Sending));
    assert_eq!(state("sending_stale"), Some(OutboxState::Uncertain));
    assert_eq!(state("uncertain"), Some(OutboxState::Uncertain));
    assert_eq!(state("failed"), Some(OutboxState::Failed));
    assert_eq!(state("sent"), Some(OutboxState::Sent));
    assert_eq!(state("cancelled"), Some(OutboxState::Cancelled));
}
