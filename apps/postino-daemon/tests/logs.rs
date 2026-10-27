//! A capture logger proves the daemon's own log records contain no token, address, subject or body.

mod support;

use std::sync::Mutex;
use std::time::Duration;

use log::{LevelFilter, Log, Metadata, Record};
use postino_daemon::Daemon;
use postino_ipc::protocol::method;
use serde_json::{Value, json};

static RECORDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl Log for Capture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        if let Ok(mut records) = RECORDS.lock() {
            records.push(format!("{} {}", record.level(), record.args()));
        }
    }

    fn flush(&self) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_logs_never_contain_the_token_a_subject_body_or_address() {
    log::set_boxed_logger(Box::new(Capture)).expect("logger");
    log::set_max_level(LevelFilter::Trace);
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let daemon = Daemon::spawn(fixture.config()).expect("daemon");
    let token = std::fs::read_to_string(daemon.token_file()).expect("token file").trim().to_owned();
    let mut client = fixture.client("logs").await;
    let subject = "Unique log subject 9f0c2a";
    let body = "Unique log body 70b1e4";
    let draft: Value = client
        .call(
            method::OP,
            json!({ "name": "draft_save", "args": {
                "account": support::ACCOUNT,
                "to": [{ "name": "", "email": "unique-log-recipient@example.com" }],
                "subject": subject,
                "body_text": body
            }}),
            Duration::from_secs(5),
        )
        .await
        .expect("draft");
    assert!(draft["draft"]["summary"]["id"].is_string());
    let _ = client.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(5)).await.expect("status");
    drop(client);
    daemon.shutdown().expect("shutdown");
    let records = RECORDS.lock().expect("records").join("\n");
    assert!(!records.contains(&token), "the token reached a log line: {records}");
    assert!(!records.contains(subject), "a subject reached a log line: {records}");
    assert!(!records.contains(body), "a body reached a log line: {records}");
    assert!(!records.contains("unique-log-recipient@example.com"), "an address reached a log line: {records}");
}
