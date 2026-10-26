//! P7 send pipeline: exact bytes, envelope, Sent APPEND, crash windows and no duplicate submission.

mod support;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use postino_core::outbox::LEASE_MS;
use postino_core::{Address, DraftContent, Engine, OutboxList, OutboxRunReport};
use postino_store::{DraftUpdate, FolderKind, OutboxState};
use postino_testkit::{SmtpConn, expected_smtp_sasl};
use support::{ACCOUNT_ID, Harness, MemoryVault, RunningAppendServer, RunningSmtp, seed_account_with_smtp};

#[derive(Default)]
struct SmtpCapture {
    auth: usize,
    mails: Vec<String>,
    rcpts: Vec<String>,
    data: Vec<Vec<u8>>,
}

fn capture_of(capture: &Arc<Mutex<SmtpCapture>>) -> std::sync::MutexGuard<'_, SmtpCapture> {
    capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy)]
enum Reply {
    Success,
    Transient,
    Permanent,
    AuthFail,
    SizeTooSmall,
    DropAfterData,
}

async fn serve_smtp(mut conn: SmtpConn, capture: Arc<Mutex<SmtpCapture>>, reply: Reply) {
    let caps: &[&str] = match reply {
        Reply::SizeTooSmall => &["SIZE 10", "AUTH PLAIN"],
        _ => &["SIZE 52428800", "8BITMIME", "AUTH PLAIN LOGIN"],
    };
    conn.greet(caps).await;
    capture_of(&capture).auth += 1;
    if matches!(reply, Reply::AuthFail) {
        conn.expect_prefix("AUTH PLAIN ").await;
        conn.send_line("535 5.7.8 authentication credentials invalid").await;
        return;
    }
    conn.auth_plain_ok(&expected_smtp_sasl()).await;
    if matches!(reply, Reply::SizeTooSmall) {
        // The client must refuse before MAIL; returning here lets the test join the thread.
        return;
    }
    let mail = conn.expect_prefix("MAIL FROM:").await;
    capture_of(&capture).mails.push(mail);
    if matches!(reply, Reply::Permanent) {
        conn.send_line("550 5.1.8 sender rejected").await;
        return;
    }
    conn.send_line("250 2.1.0 ok").await;
    let mut rcpts = Vec::new();
    loop {
        let line = conn.read_line().await;
        if line == "DATA" {
            break;
        }
        assert!(line.starts_with("RCPT TO:"), "unexpected SMTP command {line:?}");
        if matches!(reply, Reply::Transient) && rcpts.is_empty() {
            conn.send_line("451 4.7.1 try again later").await;
            capture_of(&capture).rcpts.extend(rcpts);
            return;
        }
        conn.send_line("250 2.1.5 ok").await;
        rcpts.push(line);
    }
    capture_of(&capture).rcpts.extend(rcpts);
    conn.send_line("354 go ahead").await;
    let data = conn.read_data().await;
    capture_of(&capture).data.push(data);
    if matches!(reply, Reply::DropAfterData) {
        return;
    }
    conn.send_line("250 2.0.0 queued as 1").await;
    conn.expect("QUIT").await;
    conn.send_line("221 bye").await;
}

fn draft(to: Vec<Address>, cc: Vec<Address>, bcc: Vec<Address>, subject: &str, body: &str) -> DraftContent {
    DraftContent { to, cc, bcc, subject: subject.to_owned(), body_text: body.to_owned(), attachments: Vec::new() }
}

fn open_engine(harness: &Harness, vault: &Arc<MemoryVault>, smtp: &RunningSmtp, imap: &RunningAppendServer) -> Engine {
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![smtp.ca(), imap.ca()]);
    engine
}

fn folders(engine: &Engine, draft_id: &str) -> String {
    engine.message(draft_id).map(|message| message.summary.folder_id.clone()).unwrap_or_default()
}

fn outbox_state(engine: &Engine) -> OutboxList {
    engine.outbox_list(None, 500, 0).expect("outbox")
}

#[test]
fn the_exact_bytes_envelope_bcc_and_sent_append_are_written_once() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records.clone(), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);

    let saved = engine
        .save_draft(
            None,
            None,
            draft(
                vec![Address::new("Giulia", "giulia@example.com")],
                vec![Address::new("Marco", "marco@example.com")],
                vec![Address::new("Riservato", "segreto@example.com")],
                "Preventivo",
                "Ciao Giulia,\n\nil preventivo è pronto.\n",
            ),
        )
        .expect("draft");
    let draft_id = saved.summary.id.clone();
    let dry = engine.send_dry_run(&draft_id).expect("dry run");
    assert_eq!(dry.envelope.recipients.len(), 3, "the dry-run envelope must include Bcc");
    assert!(dry.envelope.recipients.iter().any(|recipient| recipient == "segreto@example.com"));
    // The dry run and the queue each used to call `Utc::now()`: a second boundary changed the Date
    // and Message-ID and made this exact-bytes assertion flaky. A deliberate delay proves the bytes
    // come from the draft, not from the clock.
    std::thread::sleep(std::time::Duration::from_millis(1_100));

    let queued = engine.message_send(&draft_id).expect("queue");
    let queued_again = engine.message_send(&draft_id).expect("a repeated queue before the undo window is idempotent");
    assert_eq!(queued_again.outbox.id, queued.outbox.id, "the repeated queue created a second row");
    assert_eq!(queued.outbox.state, OutboxState::Queued);
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_drafts", "a queued draft stays in Drafts during the undo window");
    assert!(outbox_state(&engine).entries.iter().any(|entry| entry.id == queued.outbox.id));
    // Before the undo window expires nothing is leased.
    let early = engine.process_outbox(queued.outbox.send_at.saturating_sub(1)).expect("early pass");
    assert_eq!(early.sent, 0);
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 0);

    let report = engine.process_outbox(queued.outbox.send_at).expect("send pass");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_sent");
    let rows = outbox_state(&engine);
    assert_eq!(rows.entries[0].state, OutboxState::Sent);

    smtp.finish();
    imap.finish();
    let capture = capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(capture.data.len(), 1, "the server received a different number of submissions");
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &dry.raw_base64).expect("base64");
    assert_eq!(capture.data[0], bytes, "the bytes the server received differ from the dry run");
    assert!(!capture.data[0].windows(13).any(|window| window == b"segreto@example"), "Bcc leaked into the DATA body");
    assert!(capture.rcpts.iter().any(|line| line.contains("segreto@example.com")), "Bcc is missing from RCPT");
    assert_eq!(capture.rcpts.len(), 3);
    assert_eq!(capture.auth, 1);
    assert!(capture.mails[0].contains("SIZE="), "SIZE was advertised but not used: {}", capture.mails[0]);

    let records = records.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(records.len(), 1, "the Sent append did not run exactly once");
    assert!(records[0].flags.contains("\\Seen"), "the Sent append did not set \\Seen: {}", records[0].flags);
    assert_eq!(records[0].bytes, bytes, "the Sent copy differs from the submitted bytes");
}

#[test]
fn a_restart_after_queue_sends_exactly_once() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records.clone(), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let draft_id = {
        let mut engine = open_engine(&harness, &vault, &smtp, &imap);
        let saved =
            engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Crash", "corpo")).expect("draft");
        let id = saved.summary.id.clone();
        engine.message_send(&id).expect("queue");
        id
    };
    // The process "crashes" with the row queued; a new engine opens the same store and processes it.
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let report = engine.process_outbox(i64::MAX / 2).expect("process after restart");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_sent");
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 1);
}

#[test]
fn a_crash_with_a_sending_lease_becomes_uncertain_and_never_connects() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    // No SMTP listener is started: any attempt would be a connection failure, not a submission.
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        1,
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Crash", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    // Simulate the process dying after the lease and before the network call.
    let leased = engine.outbox_list(None, 500, 0).expect("list").entries[0].send_at;
    let store = harness.open();
    assert!(store.lease_next_outbox(leased).expect("lease").is_some());
    drop(store);

    let report = engine.process_outbox(leased.saturating_add(LEASE_MS).saturating_add(1)).expect("stale pass");
    assert_eq!(report.uncertain, 1);
    let rows = outbox_state(&engine);
    assert_eq!(rows.entries[0].state, OutboxState::Uncertain);
    assert!(rows.entries[0].last_error.as_deref().is_some_and(|error| error.contains("potrebbe essere stato accettato")));
    // A second pass never picks it up.
    let report = engine.process_outbox(i64::MAX / 2).expect("second pass");
    assert_eq!(report.processed, 0);
    assert_eq!(rows.entries[0].id, queued.outbox.id);
    imap.finish();
}

#[test]
fn a_restart_between_accept_and_sent_append_never_resubmits() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start_script(2, records.clone(), |index| index > 0);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Append", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let queued = engine.message_send(&draft_id).expect("queue");
    let first = engine.process_outbox(queued.outbox.send_at).expect("first pass");
    assert_eq!(first.sent, 0);
    assert_eq!(first.append_pending, 1);
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::AppendPending);
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_sent", "the local Sent copy exists even before the server append");

    // Reopen the store: the row is append_pending, never queued, so it cannot be submitted again.
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let report = engine.process_outbox(row.send_at.saturating_add(60_000)).expect("append retry");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_eq!(outbox_state(&engine).entries[0].state, OutboxState::Sent);
    smtp.finish();
    imap.finish();
    let capture = capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(capture.data.len(), 1, "the restart resubmitted the message");
    assert_eq!(records.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(), 1);
}

#[test]
fn after_append_a_second_pass_opens_no_connection() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Done", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    engine.process_outbox(queued.outbox.send_at).expect("first pass");
    smtp.finish();
    imap.finish();
    let report = engine.process_outbox(queued.outbox.send_at.saturating_add(60_000)).expect("second pass");
    assert_eq!(report.processed, 0);
    assert_eq!(outbox_state(&engine).entries[0].state, OutboxState::Sent);
}

#[test]
fn a_transient_failure_is_retried_with_backoff_and_a_permanent_one_stops() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(2, move |index, conn| {
        let capture = smtp_capture.clone();
        let reply = if index == 0 { Reply::Transient } else { Reply::Success };
        async move { serve_smtp(conn, capture, reply).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Retry", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let first = engine.process_outbox(queued.outbox.send_at).expect("first attempt");
    assert_eq!(first.retried, 1, "{first:?}");
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Queued);
    assert_eq!(row.attempts, 1);
    assert!(row.send_at >= queued.outbox.send_at.saturating_add(30_000), "the backoff was not applied: {}", row.send_at);
    let second = engine.process_outbox(row.send_at).expect("second attempt");
    assert_eq!(second.sent, 1, "{second:?}");
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 1, "the refused attempt must not have sent DATA");
}

#[test]
fn a_permanent_failure_records_a_fixed_sentence_and_stops() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp = RunningSmtp::start(1, |_, conn| async move { serve_smtp(conn, Arc::new(Mutex::new(SmtpCapture::default())), Reply::Permanent).await });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Stop", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("attempt");
    assert_eq!(report.failed, 1);
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Failed);
    assert!(row.last_error.as_deref().is_some_and(|error| error.contains("550") && error.contains("mail")), "{row:?}");
    assert!(row.last_error.as_deref().is_some_and(|error| !error.contains("sender rejected")), "the server's own text must not be stored: {row:?}");
    smtp.finish();
    imap.finish();
}

#[test]
fn an_auth_failure_stops_the_account_and_does_not_touch_the_next_row() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::AuthFail).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let first =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Uno", "corpo")).expect("draft");
    let second =
        engine.save_draft(None, None, draft(vec![Address::new("", "two@example.com")], Vec::new(), Vec::new(), "Due", "corpo")).expect("draft");
    let first_send_at = engine.message_send(&first.summary.id).expect("queue").outbox.send_at;
    let second_send_at = engine.message_send(&second.summary.id).expect("queue").outbox.send_at;
    let report = engine.process_outbox(first_send_at.max(second_send_at)).expect("attempt");
    assert_eq!(report.failed, 1);
    assert_eq!(report.halted, vec![ACCOUNT_ID.to_owned()]);
    let rows = outbox_state(&engine);
    let first_row = rows.entries.iter().find(|entry| entry.subject == "Uno").expect("first");
    let second_row = rows.entries.iter().find(|entry| entry.subject == "Due").expect("second");
    assert_eq!(first_row.state, OutboxState::Failed);
    assert_eq!(second_row.state, OutboxState::Queued, "the second row must stay queued, not be attempted");
    assert_eq!(rows.halted.len(), 1);
    assert!(rows.halted[0].error.contains("autenticazione"));
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).auth, 1, "the halted account was contacted again");
}

#[test]
fn a_scheduled_row_waits_for_its_due_time_and_can_be_cancelled() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp = RunningSmtp::start(0, |_, _| async {});
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Domani", "corpo")).expect("draft");
    let at = chrono::Utc::now() + chrono::Duration::hours(2);
    let queued = engine.send_schedule(&saved.summary.id, at).expect("schedule");
    assert!(queued.outbox.send_at >= at.timestamp_millis() - 1_000);
    let report: OutboxRunReport = engine.process_outbox(chrono::Utc::now().timestamp_millis()).expect("early");
    assert_eq!(report.processed, 0);
    let cancelled = engine.outbox_cancel(&queued.outbox.id).expect("cancel");
    assert_eq!(cancelled.state, OutboxState::Cancelled);
    let report = engine.process_outbox(at.timestamp_millis()).expect("after due");
    assert_eq!(report.processed, 0);
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Cancelled);
    smtp.finish();
    imap.finish();
}

#[test]
fn a_dry_run_returns_bytes_without_a_row() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts)]);
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Dry", "corpo")).expect("draft");
    let dry = engine.send_dry_run(&saved.summary.id).expect("dry run");
    assert!(dry.bytes > 0);
    assert!(dry.raw_base64.starts_with("RnJvbT"));
    assert!(engine.outbox_list(None, 500, 0).expect("outbox").entries.is_empty(), "the dry run wrote an outbox row");
    assert_eq!(folders(&engine, &saved.summary.id), "fld_acc_test_drafts", "the dry run moved the draft");
}

#[test]
fn more_than_one_hundred_recipients_is_refused_before_the_queue() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts)]);
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    let recipients = (0..101).map(|index| Address::new("", format!("user{index}@example.com"))).collect();
    let error = engine.save_draft(None, None, draft(recipients, Vec::new(), Vec::new(), "Troppi", "corpo")).expect_err("too many recipients");
    assert!(error.to_string().contains("100"), "{error}");
    assert!(engine.outbox_list(None, 500, 0).expect("outbox").entries.is_empty());
}

#[test]
fn a_small_server_size_limit_stops_before_data() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp =
        RunningSmtp::start(1, |_, conn| async move { serve_smtp(conn, Arc::new(Mutex::new(SmtpCapture::default())), Reply::SizeTooSmall).await });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved = engine
        .save_draft(
            None,
            None,
            draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Grande", "un corpo abbastanza lungo da superare dieci byte"),
        )
        .expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("attempt");
    assert_eq!(report.failed, 1, "{report:?}");
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Failed);
    assert!(row.last_error.as_deref().is_some_and(|error| error.contains("grande")), "{row:?}");
    smtp.finish();
    imap.finish();
}

#[test]
fn a_twenty_five_mib_attachment_and_ten_recipients_go_through() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let recipients = (0..9).map(|index| Address::new("", format!("user{index}@example.com"))).collect::<Vec<_>>();
    let mut content = draft(recipients, Vec::new(), vec![Address::new("", "bcc@example.com")], "Allegato grande", "Vedi allegato.");
    content.attachments = vec![postino_core::Attachment {
        id: "att_big".to_owned(),
        filename: "grande.bin".to_owned(),
        mime: "application/octet-stream".to_owned(),
        size: 0,
    }];
    let saved = engine.save_draft(None, None, content).expect("draft");
    let content = vec![0xABu8; 25 * 1024 * 1024];
    engine.store_attachment_bytes(&saved.summary.id, "att_big", &content).expect("attachment bytes");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("send");
    assert_eq!(report.sent, 1, "{report:?}");
    smtp.finish();
    imap.finish();
    let capture = capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(capture.rcpts.len(), 10, "the tenth recipient is the Bcc");
    assert!(capture.data[0].len() > 25 * 1024 * 1024);
    assert_eq!(capture.data.len(), 1);
}

#[test]
fn an_uncertain_outcome_is_never_retried_automatically_and_manual_retry_is_explicit() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp =
        RunningSmtp::start(1, |_, conn| async move { serve_smtp(conn, Arc::new(Mutex::new(SmtpCapture::default())), Reply::DropAfterData).await });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Incerto", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("attempt");
    assert_eq!(report.uncertain, 1);
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Uncertain);
    let second = engine.process_outbox(i64::MAX / 2).expect("second pass");
    assert_eq!(second.processed, 0, "an uncertain row was processed automatically");
    let error = engine.outbox_retry(&row.id, false).expect_err("confirmation is mandatory");
    assert!(error.to_string().contains("confirm"), "{error}");
    let reset = engine.outbox_retry(&row.id, true).expect("manual retry");
    assert_eq!(reset.state, OutboxState::Queued);
    smtp.finish();
    imap.finish();
}

#[test]
fn a_manual_mark_sent_moves_the_draft_without_network() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)]);
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Deciso", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let store = harness.open();
    let leased = store.lease_next_outbox(queued.outbox.send_at).expect("lease").expect("row");
    store.mark_outbox_uncertain(&queued.outbox.id, "unknown", leased.lease_at.unwrap_or_default()).expect("uncertain");
    drop(store);
    let marked = engine.outbox_mark_sent(&queued.outbox.id, true).expect("mark sent");
    assert_eq!(marked.state, OutboxState::Sent);
    assert_eq!(folders(&engine, &saved.summary.id), "fld_acc_test_sent");
}

// ---------------------------------------------------------------------------------------------
// Closure regressions: duplicate submission, server-controlled text, leases, locks, crashes.
// ---------------------------------------------------------------------------------------------

/// The journal as the CLI and MCP see it: a serialized `outbox_list` result.
fn outbox_json(engine: &Engine) -> String {
    serde_json::to_string(&engine.outbox_list(None, 500, 0).expect("outbox")).expect("serialize the outbox")
}

/// A fake SMTP server that answers the first `RCPT` with a refusal echoing `text`.
async fn serve_hostile_rcpt(mut conn: SmtpConn, text: String) {
    conn.greet(&["SIZE 52428800", "AUTH PLAIN"]).await;
    conn.auth_plain_ok(&expected_smtp_sasl()).await;
    conn.expect_prefix("MAIL FROM:").await;
    conn.send_line("250 2.1.0 ok").await;
    conn.expect_prefix("RCPT TO:").await;
    conn.send_line(&format!("550 5.7.1 {text}")).await;
}

/// A fake SMTP server that accepts the message and then refuses it after the final dot, echoing
/// `text` in the reply.
async fn serve_hostile_data(mut conn: SmtpConn, text: String) {
    conn.greet(&["SIZE 52428800", "AUTH PLAIN"]).await;
    conn.auth_plain_ok(&expected_smtp_sasl()).await;
    conn.expect_prefix("MAIL FROM:").await;
    conn.send_line("250 2.1.0 ok").await;
    conn.expect_prefix("RCPT TO:").await;
    conn.send_line("250 2.1.5 ok").await;
    conn.expect("DATA").await;
    conn.send_line("354 go ahead").await;
    let _ = conn.read_data().await;
    conn.send_line(&format!("554 5.6.0 {text}")).await;
}

#[test]
fn cancelling_then_queueing_twice_submits_exactly_once() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    // Exactly one connection: a second submission would fail loudly instead of hiding.
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Annulla", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let first = engine.message_send(&draft_id).expect("queue");
    engine.outbox_cancel(&first.outbox.id).expect("cancel");
    let second = engine.message_send(&draft_id).expect("queue after the cancel");
    let third = engine.message_send(&draft_id).expect("the repeated queue is the same request");
    assert_eq!(second.outbox.id, third.outbox.id, "a repeated queue created a second row");
    assert_eq!(second.outbox.state, OutboxState::Queued);
    assert_eq!(engine.outbox_list(None, 500, 0).expect("outbox").entries.len(), 1, "the cancelled row was not reused");
    let report = engine.process_outbox(second.outbox.send_at).expect("pass");
    assert_eq!(report.sent, 1, "{report:?}");
    smtp.finish();
    imap.finish();
    let capture = capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(capture.data.len(), 1, "one pass submitted the message twice");
    assert_eq!(capture.auth, 1);
}

#[test]
fn a_live_row_blocks_editing_and_moving_even_after_a_cancelled_one() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp = RunningSmtp::start(0, |_, _| async {});
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent), ("trash", "Trash", FolderKind::Trash)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Blocco", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let first = engine.message_send(&draft_id).expect("queue");
    engine.outbox_cancel(&first.outbox.id).expect("cancel");
    engine.message_send(&draft_id).expect("a second live row");
    let edit = engine
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Blocco", "cambiato"))
        .expect_err("a queued draft must not be editable");
    assert!(edit.to_string().contains("coda (queued)"), "{edit}");
    let moved = engine.move_message(&draft_id, "fld_acc_test_trash").expect_err("a queued draft must not be movable");
    assert!(moved.to_string().contains("coda (queued)"), "{moved}");
    smtp.finish();
    imap.finish();
}

#[test]
fn a_hostile_rcpt_echo_never_reaches_the_journal() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let secret = postino_testkit::SMTP_PASSWORD.to_owned();
    let body = "il corpo riservato della bozza";
    let recipient = "segreto@example.com";
    let echo = format!("{secret} {body} {recipient}");
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let echo = echo.clone();
        async move { serve_hostile_rcpt(conn, echo).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved = engine.save_draft(None, None, draft(vec![Address::new("", recipient)], Vec::new(), Vec::new(), "Ostile", body)).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("attempt");
    assert_eq!(report.failed, 1, "{report:?}");
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Failed);
    let sentence = row.last_error.clone().expect("a reason");
    assert!(sentence.contains("rcpt") && sentence.contains("550"), "the stage and code must survive: {sentence}");
    for forbidden in [secret.as_str(), body, recipient] {
        assert!(!sentence.contains(forbidden), "the journal kept server text: {sentence}");
        assert!(!outbox_json(&engine).contains(forbidden), "outbox_list exposed server text");
        assert!(!report.errors.join(" ").contains(forbidden), "the CLI report exposed server text");
    }
    smtp.finish();
    imap.finish();
}

#[test]
fn a_hostile_data_echo_never_reaches_the_journal() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let secret = postino_testkit::SMTP_PASSWORD.to_owned();
    let body = "testo del messaggio da non ripetere";
    let echo = format!("{secret} {body}");
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let echo = echo.clone();
        async move { serve_hostile_data(conn, echo).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Ostile", body)).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("attempt");
    assert_eq!(report.uncertain, 1, "{report:?}");
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Uncertain);
    let sentence = row.last_error.clone().expect("a reason");
    for forbidden in [secret.as_str(), body] {
        assert!(!sentence.contains(forbidden), "the journal kept server text: {sentence}");
        assert!(!outbox_json(&engine).contains(forbidden), "outbox_list exposed server text");
        assert!(!report.errors.join(" ").contains(forbidden), "the CLI report exposed server text");
    }
    smtp.finish();
    imap.finish();
}

#[test]
fn a_hostile_append_echo_never_reaches_the_journal() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let secret = postino_testkit::PASSWORD.to_owned();
    let body = "il corpo che il server IMAP non deve conservare";
    let imap = RunningAppendServer::start_refusing(1, format!("{secret} {body}"));
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Ostile", body)).expect("draft");
    let draft_id = saved.summary.id.clone();
    let queued = engine.message_send(&draft_id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("attempt");
    assert_eq!(report.append_pending, 1, "{report:?}");
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::AppendPending);
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_sent", "the submitted message is in Sent");
    let sentence = row.last_error.clone().expect("a reason");
    for forbidden in [secret.as_str(), body] {
        assert!(!sentence.contains(forbidden), "the journal kept IMAP server text: {sentence}");
        assert!(!outbox_json(&engine).contains(forbidden), "outbox_list exposed IMAP server text");
        assert!(!report.errors.join(" ").contains(forbidden), "the CLI report exposed IMAP server text");
    }
    smtp.finish();
    imap.finish();
}

#[test]
fn a_second_process_does_not_expire_a_live_lease_on_an_old_scheduled_row() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(Some(release_rx));
    let smtp = RunningSmtp::start(1, move |_, mut conn| {
        let capture = smtp_capture.clone();
        let reached = reached_tx.clone();
        let release = release_rx.lock().ok().and_then(|mut guard| guard.take());
        async move {
            conn.greet(&["SIZE 52428800", "AUTH PLAIN"]).await;
            conn.auth_plain_ok(&expected_smtp_sasl()).await;
            conn.expect_prefix("MAIL FROM:").await;
            conn.send_line("250 ok").await;
            conn.expect_prefix("RCPT TO:").await;
            conn.send_line("250 ok").await;
            conn.expect("DATA").await;
            conn.send_line("354 go ahead").await;
            let data = conn.read_data().await;
            capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.push(data);
            let _ = reached.send(());
            // Hold the transaction open while the second process runs its own pass.
            if let Some(release) = release {
                let _ = release.recv();
            }
            conn.send_line("250 2.0.0 queued as 1").await;
            conn.expect("QUIT").await;
            conn.send_line("221 bye").await;
        }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine_a = open_engine(&harness, &vault, &smtp, &imap);
    let saved = engine_a
        .save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Programmato", "corpo"))
        .expect("draft");
    let queued = engine_a.message_send(&saved.summary.id).expect("queue");
    // The row was scheduled two hours ago and the app was closed: its due time is old, its lease is
    // not.
    let due = queued.outbox.send_at;
    let scheduled = due.saturating_sub(2 * 60 * 60 * 1_000);
    assert!(harness.open().set_outbox_due(&queued.outbox.id, scheduled).expect("due time"));
    let lease_time = due.saturating_add(1_000);
    let sender = std::thread::spawn(move || {
        let report = engine_a.process_outbox(lease_time).expect("first process");
        (report, engine_a)
    });
    reached_rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the first process did not reach DATA");

    // A second process with its own connection runs while the first is inside DATA.
    let mut engine_b = open_engine(&harness, &vault, &smtp, &imap);
    let other = engine_b.process_outbox(lease_time.saturating_add(1_000)).expect("second process");
    assert_eq!(other.uncertain, 0, "a live lease was expired by the second process: {other:?}");
    assert_eq!(other.processed, 0, "the second process touched a leased row: {other:?}");
    let row = outbox_state(&engine_b).entries[0].clone();
    assert_eq!(row.state, OutboxState::Sending, "the second process changed the row: {row:?}");

    let _ = release_tx.send(());
    let (report, engine_a) = sender.join().expect("the first process panicked");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_eq!(outbox_state(&engine_a).entries[0].state, OutboxState::Sent);
    // Keep the engine alive until the assertion below so its store connection stays open.
    drop(engine_a);
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 1, "the second process opened a second submission");
}

#[test]
fn a_permanent_failure_can_be_discarded_edited_and_resent() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(2, move |index, conn| {
        let capture = smtp_capture.clone();
        let reply = if index == 0 { Reply::Permanent } else { Reply::Success };
        async move { serve_smtp(conn, capture, reply).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved = engine
        .save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Rifiutata", "prima versione"))
        .expect("draft");
    let draft_id = saved.summary.id.clone();
    let queued = engine.message_send(&draft_id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("attempt");
    assert_eq!(report.failed, 1, "{report:?}");
    let failed = outbox_state(&engine).entries[0].clone();
    assert_eq!(failed.state, OutboxState::Failed);
    assert!(
        engine
            .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "two@example.com")], Vec::new(), Vec::new(), "Rifiutata", "corretta"))
            .is_err(),
        "a failed row must lock the draft until it is discarded"
    );
    let discarded = engine.outbox_cancel(&failed.id).expect("a permanent failure is a discard");
    assert_eq!(discarded.state, OutboxState::Cancelled);
    engine
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "two@example.com")], Vec::new(), Vec::new(), "Rifiutata", "corretta"))
        .expect("the draft is editable after the discard");
    let again = engine.message_send(&draft_id).expect("queue the corrected draft");
    let report = engine.process_outbox(again.outbox.send_at).expect("second attempt");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_sent");
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 1, "the refused attempt must not have sent DATA");
}

#[test]
fn a_sent_message_stays_movable_and_a_batch_does_not_abort() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[
            ("drafts", "Drafts", FolderKind::Drafts),
            ("sent", "Sent", FolderKind::Sent),
            ("archive", "Archive", FolderKind::Archive),
            ("trash", "Trash", FolderKind::Trash),
        ],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved = engine
        .save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Archiviata", "corpo"))
        .expect("draft");
    let draft_id = saved.summary.id.clone();
    let queued = engine.message_send(&draft_id).expect("queue");
    engine.process_outbox(queued.outbox.send_at).expect("send");
    // A batch over a Sent row and an ordinary draft row: neither may abort it.
    engine.move_message(&draft_id, "fld_acc_test_archive").expect("a Sent copy must be movable");
    let other = engine
        .save_draft(None, None, draft(vec![Address::new("", "two@example.com")], Vec::new(), Vec::new(), "Bozza", "corpo"))
        .expect("second draft");
    engine.move_message(&other.summary.id, "fld_acc_test_trash").expect("an ordinary draft moves");
    engine.move_message(&draft_id, "fld_acc_test_trash").expect("a Sent copy moves to Trash");
    assert_eq!(engine.message(&draft_id).map(|message| message.summary.folder_id.clone()), Some("fld_acc_test_trash".to_owned()));
    smtp.finish();
    imap.finish();
}

#[test]
fn a_crash_after_the_server_accepted_is_uncertain_and_never_resubmits() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Crash", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let queued = engine.message_send(&draft_id).expect("queue");
    // The seam dies after the server's 250 and before the durable append_pending write.
    engine.set_test_crash_after_accept(true);
    let error = engine.process_outbox(queued.outbox.send_at).expect_err("the test seam must stop the pass");
    assert!(error.to_string().contains("test crash"), "{error}");
    assert_eq!(outbox_state(&engine).entries[0].state, OutboxState::Sending, "the crash left the journal honest");
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_drafts", "the local copy moved without a durable transition");
    drop(engine);

    // A second process, before the lease expires: nothing to do, nothing to expire.
    let mut second = open_engine(&harness, &vault, &smtp, &imap);
    let before = second.process_outbox(queued.outbox.send_at.saturating_add(1_000)).expect("early pass");
    assert_eq!((before.processed, before.uncertain), (0, 0), "{before:?}");
    // After the lease the row is uncertain, and no connection is opened.
    let after = second.process_outbox(queued.outbox.send_at.saturating_add(LEASE_MS).saturating_add(1)).expect("late pass");
    assert_eq!(after.uncertain, 1, "{after:?}");
    assert_eq!(after.processed, 0);
    let row = outbox_state(&second).entries[0].clone();
    assert_eq!(row.state, OutboxState::Uncertain);
    assert!(row.last_error.as_deref().is_some_and(|sentence| sentence.contains("Inviati")), "{row:?}");
    let third = second.process_outbox(i64::MAX / 2).expect("never picked up");
    assert_eq!(third.processed, 0);
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 1, "the crash window resubmitted the message");
}

#[test]
fn a_new_row_is_visible_after_five_hundred_history_rows() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    // Five hundred terminal rows: an oldest-first window would hide everything queued now.
    for index in 0..500u32 {
        store
            .queue_outbox(&postino_store::OutboxEntry {
                id: format!("ob_old_{index:03}"),
                account_id: ACCOUNT_ID.to_owned(),
                draft_id: Some(format!("draft_old_{index}")),
                idempotency_key: format!("draft:old:{index}"),
                created_at: i64::from(index),
                send_at: i64::from(index),
                state: OutboxState::Sent,
                attempts: 1,
                append_attempts: 1,
                last_error: None,
                raw_blob_hash: "a".repeat(64),
                envelope_json: "{}".to_owned(),
                lease_at: None,
            })
            .expect("history row");
    }
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Recente", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let listed = outbox_state(&engine);
    assert!(listed.entries.iter().any(|entry| entry.id == queued.outbox.id), "the new row is not listable");
    let report = engine.process_outbox(queued.outbox.send_at).expect("send");
    assert_eq!(report.sent, 1, "{report:?}");
    // `postino send` looks its row up in the pass result: it must be there after 500 older rows.
    assert!(report.entries.iter().any(|entry| entry.id == queued.outbox.id), "the pass result hides the row");
    smtp.finish();
    imap.finish();
}

#[test]
fn an_unreadable_blob_fails_that_row_and_the_pass_continues() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let broken =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Rotta", "corpo")).expect("draft");
    let intact =
        engine.save_draft(None, None, draft(vec![Address::new("", "two@example.com")], Vec::new(), Vec::new(), "Intera", "corpo")).expect("draft");
    let broken_row = engine.message_send(&broken.summary.id).expect("queue");
    let intact_row = engine.message_send(&intact.summary.id).expect("queue");
    let hash = store.get_outbox(&broken_row.outbox.id).expect("row").expect("row").raw_blob_hash;
    assert!(store.blobs().remove(&hash).expect("remove the stored bytes"));
    let report = engine.process_outbox(broken_row.outbox.send_at.max(intact_row.outbox.send_at)).expect("pass");
    assert_eq!(report.failed, 1, "{report:?}");
    assert_eq!(report.sent, 1, "an unreadable blob stopped the whole pass: {report:?}");
    let rows = outbox_state(&engine);
    let broken_row = rows.entries.iter().find(|entry| entry.id == broken_row.outbox.id).expect("broken row");
    assert_eq!(broken_row.state, OutboxState::Failed);
    assert!(broken_row.last_error.as_deref().is_some_and(|sentence| sentence.contains("non sono leggibili")), "{broken_row:?}");
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 1);
}

#[test]
fn the_local_sent_copy_carries_the_message_id_a_sync_will_find() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(1, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Identità", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let dry = engine.send_dry_run(&draft_id).expect("dry run");
    let queued = engine.message_send(&draft_id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("send");
    assert_eq!(report.sent, 1, "{report:?}");
    eprintln!("errors: {:?}", report.errors);
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_sent");
    smtp.finish();
    imap.finish();
    // A fresh connection, as the next process would open it.
    let reopened = harness.open();
    let stored = reopened.get_message(&draft_id).expect("message").expect("the Sent copy");
    assert_eq!(stored.summary.folder_id, "fld_acc_test_sent");
    assert!(stored.uid.is_none(), "a local copy has no server UID yet");
    let expected = postino_sync::normalised_message_id(&dry.message_id).expect("the generated Message-ID");
    assert_eq!(stored.message_id.as_deref(), Some(expected.as_str()), "the Sent copy lost the generated Message-ID");
    assert!(postino_sync::local_message_id(ACCOUNT_ID, &dry.message_id).is_some(), "the Message-ID must be usable as a sync identity");
}

// ---------------------------------------------------------------------------------------------
// Second closure pass: R2/R3, append ownership, sent-row policy, external processes, pagination.
// ---------------------------------------------------------------------------------------------

fn store_message(folder_id: &str, id: &str, message_id: &str, subject: &str) -> postino_store::Message {
    postino_store::Message {
        summary: postino_store::MessageSummary {
            id: id.to_owned(),
            account_id: ACCOUNT_ID.to_owned(),
            folder_id: folder_id.to_owned(),
            thread_id: format!("thr_{id}"),
            from: postino_store::Address::new("Postino Test", postino_testkit::USER),
            to: vec![postino_store::Address::new("", "one@example.com")],
            subject: subject.to_owned(),
            snippet: "corpo".to_owned(),
            date: 1_700_000_000_000,
            unread: false,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(7),
        message_id: Some(message_id.to_owned()),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "corpo".to_owned(),
        body_html: None,
        remote_images: 0,
        size: None,
    }
}

#[test]
fn a_revived_row_is_the_newest_row_and_a_stale_snapshot_is_never_submitted() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp = RunningSmtp::start(0, |_, _| async {});
    let imap = RunningAppendServer::start(0, Arc::new(Mutex::new(Vec::new())), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent), ("trash", "Trash", FolderKind::Trash)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved = engine
        .save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "R1", "prima versione"))
        .expect("draft");
    let draft_id = saved.summary.id.clone();
    let first = engine.message_send(&draft_id).expect("queue R1");
    engine.outbox_cancel(&first.outbox.id).expect("cancel R1");
    engine
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "R1", "seconda versione"))
        .expect("edit");
    let second = engine.message_send(&draft_id).expect("queue R2");
    engine.outbox_cancel(&second.outbox.id).expect("cancel R2");

    // Revive R1 (same content key) with a newer created_at but its old rowid: the newest row must be
    // the revived queued one, not the later-rowid cancelled R2.
    let mut revived = store.get_outbox(&first.outbox.id).expect("row").expect("R1");
    revived.created_at = second.outbox.created_at.saturating_add(10_000);
    revived.send_at = revived.created_at;
    store.queue_outbox(&revived).expect("revive R1");
    let newest = store.active_outbox_for_draft(&draft_id).expect("query").expect("row");
    assert_eq!(newest.id, first.outbox.id, "a later-rowid cancelled row hid the revived queued row");
    assert_eq!(newest.state, OutboxState::Queued);

    // The queued revision is the first one; the draft on disk is the second. A fresh engine must
    // lock it, and the pass must refuse to submit the stale bytes.
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let edit = engine
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "R1", "terza"))
        .expect_err("a queued draft must not be editable");
    assert!(edit.to_string().contains("coda (queued)"), "{edit}");
    let moved = engine.move_message(&draft_id, "fld_acc_test_trash").expect_err("a queued draft must not be movable");
    assert!(moved.to_string().contains("coda (queued)"), "{moved}");
    let report = engine.process_outbox(revived.send_at).expect("pass");
    assert_eq!(report.failed, 1, "{report:?}");
    let row = outbox_state(&engine).entries.iter().find(|entry| entry.id == first.outbox.id).expect("row").clone();
    assert_eq!(row.state, OutboxState::Failed);
    assert!(row.last_error.as_deref().is_some_and(|sentence| sentence.contains("cambiata dopo l'accodamento")), "{row:?}");
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_drafts", "a stale snapshot moved the draft");
    smtp.finish();
    imap.finish();
}

#[test]
fn a_sent_message_moved_back_to_drafts_needs_an_edit_before_it_can_be_resent() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(2, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(2, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Resend", "prima")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let queued = engine.message_send(&draft_id).expect("queue");
    assert_eq!(engine.process_outbox(queued.outbox.send_at).expect("send").sent, 1);
    engine.move_message(&draft_id, "fld_acc_test_drafts").expect("move the Sent copy back to Drafts");
    let refused = engine.message_send(&draft_id).expect_err("an unedited moved-back draft must not resend");
    assert!(refused.to_string().contains("already sent"), "{refused}");
    engine
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Resend", "seconda"))
        .expect("editing a moved-back draft");
    let again = engine.message_send(&draft_id).expect("an edited revision is a deliberate resend");
    assert_ne!(again.outbox.id, queued.outbox.id, "the resend reused the sent row");
    let report = engine.process_outbox(again.outbox.send_at).expect("second send");
    assert_eq!(report.sent, 1, "{report:?}");
    let rows = outbox_state(&engine);
    assert_eq!(rows.entries.iter().filter(|entry| entry.state == OutboxState::Sent).count(), 2, "{rows:?}");
    smtp.finish();
    imap.finish();
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 2);
}

#[test]
fn an_outbox_mark_sent_that_loses_the_race_does_not_move_the_draft() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp =
        RunningSmtp::start(1, |_, conn| async move { serve_smtp(conn, Arc::new(Mutex::new(SmtpCapture::default())), Reply::DropAfterData).await });
    let imap = RunningAppendServer::start(0, Arc::new(Mutex::new(Vec::new())), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Race", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let queued = engine.message_send(&draft_id).expect("queue");
    assert_eq!(engine.process_outbox(queued.outbox.send_at).expect("uncertain").uncertain, 1);
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::Uncertain);

    // A concurrent process retries the row between the engine's read and its confirmation.
    let other = harness.open();
    let raced_id = row.id.clone();
    engine.set_test_after_outbox_read(move || {
        other.retry_outbox(&raced_id, 1_700_000_000_000).expect("the concurrent retry");
    });
    let error = engine.outbox_mark_sent(&row.id, true).expect_err("a lost race must not be reported as sent");
    assert!(error.to_string().contains("changed state"), "{error}");
    assert_eq!(folders(&engine, &draft_id), "fld_acc_test_drafts", "a lost race moved the draft to Sent");
    let stored = harness.open().get_outbox(&row.id).expect("row").expect("row");
    assert_eq!(stored.state, OutboxState::Queued);
    smtp.finish();
    imap.finish();
}

#[test]
fn a_claimed_append_is_not_appended_by_a_second_pass() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp = RunningSmtp::start(0, |_, _| async {});
    // Zero connections: any APPEND from this test would be a connection error, not a silent pass.
    let imap = RunningAppendServer::start(0, Arc::new(Mutex::new(Vec::new())), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Append", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let lease = store.lease_next_outbox(queued.outbox.send_at).expect("lease").expect("row").lease_at.expect("lease");
    let append_lease = queued.outbox.send_at;
    assert!(store.mark_outbox_append_pending(&queued.outbox.id, lease, append_lease).expect("append pending"));
    // The row carries a live append claim, as if another process had written it. Its pass and this
    // one must not append it concurrently.
    let report = engine.process_outbox(append_lease.saturating_add(60_000)).expect("pass");
    assert_eq!(report.processed, 0, "the pass processed a row another process owns: {report:?}");
    assert_eq!(report.sent, 0);
    let row = outbox_state(&engine).entries[0].clone();
    assert_eq!(row.state, OutboxState::AppendPending);
    assert_eq!(row.id, queued.outbox.id);
    smtp.finish();
    imap.finish();
}

#[test]
fn a_stale_engine_cannot_overwrite_a_message_another_process_moved() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        1,
        1,
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent), ("trash", "Trash", FolderKind::Trash)],
    );
    let mut stale = Engine::open_with_vault(harness.open(), vault.clone()).expect("stale engine");
    let saved =
        stale.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Stale", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    // A second process moves it to Sent (the finalize half of a submission).
    let mut fresh = Engine::open_with_vault(harness.open(), vault).expect("fresh engine");
    fresh.send_draft(&draft_id).expect("the other process moves it");
    let edit = stale
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Stale", "tardi"))
        .expect_err("a stale Drafts save must not overwrite the Sent row");
    assert!(edit.to_string().contains("changed in another session"), "{edit}");
    let moved = stale.move_message(&draft_id, "fld_acc_test_trash").expect_err("a stale move must be refused");
    assert!(moved.to_string().contains("changed in another session"), "{moved}");
    let stored = harness.open().get_message(&draft_id).expect("message").expect("row");
    assert_eq!(stored.summary.folder_id, "fld_acc_test_sent", "the stale save destroyed the Sent row");
}

#[test]
fn a_stale_engine_cannot_queue_a_draft_another_process_edited() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts)]);
    let mut stale = Engine::open_with_vault(harness.open(), vault.clone()).expect("stale engine");
    let saved =
        stale.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Fresca", "prima")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let mut fresh = Engine::open_with_vault(harness.open(), vault).expect("fresh engine");
    fresh
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Fresca", "seconda"))
        .expect("the other process edits");
    let error = stale.message_send(&draft_id).expect_err("a stale snapshot must not be queued");
    assert!(error.to_string().contains("changed in another session"), "{error}");
    assert!(harness.open().list_outbox(None, 50).expect("outbox").is_empty(), "the stale queue wrote a row");
}

#[test]
fn two_engines_allocate_distinct_draft_ids() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts)]);
    let mut first = Engine::open_with_vault(harness.open(), vault.clone()).expect("first engine");
    let mut second = Engine::open_with_vault(harness.open(), vault).expect("second engine");
    let a =
        first.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Uno", "corpo")).expect("first draft");
    let b = second
        .save_draft(None, None, draft(vec![Address::new("", "two@example.com")], Vec::new(), Vec::new(), "Due", "corpo"))
        .expect("second draft");
    assert_ne!(a.summary.id, b.summary.id, "two processes received the same draft id");
    assert_eq!(harness.open().list_drafts(None).expect("drafts").len(), 2, "the second save overwrote the first draft");
}

#[test]
fn a_damaged_append_envelope_is_never_reported_as_sent() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp = RunningSmtp::start(0, |_, _| async {});
    let imap = RunningAppendServer::start(0, Arc::new(Mutex::new(Vec::new())), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let now = 1_800_000_000_000;
    store
        .queue_outbox(&postino_store::OutboxEntry {
            id: "ob_bad_append".to_owned(),
            account_id: ACCOUNT_ID.to_owned(),
            draft_id: Some("draft_missing".to_owned()),
            idempotency_key: "draft:bad:append".to_owned(),
            created_at: now,
            send_at: now,
            state: OutboxState::AppendPending,
            attempts: 1,
            append_attempts: 1,
            last_error: None,
            raw_blob_hash: "a".repeat(64),
            envelope_json: "{}".to_owned(),
            lease_at: None,
        })
        .expect("a damaged append row");
    // Three passes, each at the row's due time: a damaged envelope is a fixed failure with backoff,
    // never a false `sent`, and the backoff counter grows instead of pinning the retry at 30 s.
    let mut at = now;
    for pass in 0..3 {
        let report = engine.process_outbox(at).expect("pass");
        assert_eq!(report.sent, 0, "pass {pass} reported the damaged row as sent: {report:?}");
        let row = store.get_outbox("ob_bad_append").expect("row").expect("row");
        assert_eq!(row.state, OutboxState::AppendPending);
        assert!(row.last_error.as_deref().is_some_and(|sentence| sentence.contains("inviluppo salvato non è leggibile")), "{row:?}");
        assert_eq!(row.append_attempts, u32::try_from(pass + 2).expect("attempt"), "the seeded append attempt was not counted");
        at = row.send_at;
    }
    smtp.finish();
    imap.finish();
}

#[test]
fn a_synced_sent_copy_is_merged_into_the_local_one() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let capture = Arc::new(Mutex::new(SmtpCapture::default()));
    let smtp_capture = capture.clone();
    let smtp = RunningSmtp::start(1, move |_, conn| {
        let capture = smtp_capture.clone();
        async move { serve_smtp(conn, capture, Reply::Success).await }
    });
    // Zero connections: the merge must adopt the cached server copy instead of APPENDing a second
    // one. Any APPEND from this test would be a connection error, not a silent pass.
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(0, records.clone(), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Sync", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let dry = engine.send_dry_run(&draft_id).expect("dry run");
    let generated = postino_sync::normalised_message_id(&dry.message_id).expect("Message-ID");
    // A provider auto-save (or a sync between the 250 and the local move) already cached the server
    // copy in Sent with UID 7.
    store.upsert_synced_message(&store_message("fld_acc_test_sent", "msg_cached", &generated, "Sync"), &[]).expect("the cached server copy");
    let queued = engine.message_send(&draft_id).expect("queue");
    let report = engine.process_outbox(queued.outbox.send_at).expect("send");
    assert_eq!(report.sent, 1, "{report:?}");
    smtp.finish();
    imap.finish();
    // One local row: the draft moved to Sent, the cached duplicate was removed in the same
    // transaction, and the surviving row carries the UID the server already assigned. Without the
    // UID transfer the Sent cursor is already past UID 7, so no later sync could adopt it.
    let filter =
        postino_store::MessageFilter { folder_id: Some("fld_acc_test_sent".to_owned()), limit: 50, ..postino_store::MessageFilter::default() };
    let rows = harness.open().list_messages(&filter).expect("sent messages").messages;
    assert_eq!(rows.len(), 1, "the local finalize left two Sent rows: {rows:?}");
    assert_eq!(rows[0].id, draft_id);
    let stored = harness.open().get_message(&draft_id).expect("message").expect("row");
    assert_eq!(stored.uid, Some(7), "the merged local row did not adopt the cached server UID");
    assert!(harness.open().get_message("msg_cached").expect("duplicate").is_none(), "the cached duplicate survived the merge");
    assert!(records.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_empty(), "the merge APPENDed a message the server already had");
    assert_eq!(capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).data.len(), 1);
}

#[test]
fn each_lease_in_a_pass_uses_the_claim_time() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let clock = Arc::new(AtomicI64::new(0));
    let smtp_clock = clock.clone();
    let (second_reached_tx, second_reached_rx) = std::sync::mpsc::channel::<()>();
    let second_reached = Arc::new(Mutex::new(Some(second_reached_tx)));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let smtp = RunningSmtp::start(2, move |index, mut conn| {
        let clock = smtp_clock.clone();
        let reached = if index == 1 { second_reached.lock().ok().and_then(|mut guard| guard.take()) } else { None };
        let release = if index == 1 { release_rx.lock().ok().and_then(|mut guard| guard.take()) } else { None };
        async move {
            conn.greet(&["SIZE 52428800", "AUTH PLAIN"]).await;
            conn.auth_plain_ok(&expected_smtp_sasl()).await;
            conn.expect_prefix("MAIL FROM:").await;
            conn.send_line("250 ok").await;
            conn.expect_prefix("RCPT TO:").await;
            conn.send_line("250 ok").await;
            conn.expect("DATA").await;
            conn.send_line("354 go ahead").await;
            let _ = conn.read_data().await;
            if index == 0 {
                // The first row takes 30 minutes of simulated time inside DATA.
                clock.fetch_add(30 * 60 * 1_000, Ordering::SeqCst);
            } else {
                if let Some(reached) = reached {
                    let _ = reached.send(());
                }
                // Hold the second transaction open while the test reads the row's lease.
                if let Some(release) = release {
                    let _ = release.recv();
                }
            }
            conn.send_line("250 2.0.0 queued as 1").await;
            conn.expect("QUIT").await;
            conn.send_line("221 bye").await;
        }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let imap = RunningAppendServer::start(2, records, true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let first = engine
        .save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Prima", "corpo"))
        .expect("first draft");
    let second = engine
        .save_draft(None, None, draft(vec![Address::new("", "two@example.com")], Vec::new(), Vec::new(), "Seconda", "corpo"))
        .expect("second draft");
    let first = engine.message_send(&first.summary.id).expect("first queue");
    let second = engine.message_send(&second.summary.id).expect("second queue");
    // Both are due at the same instant, as a batch.
    let t0 = first.outbox.send_at.max(second.outbox.send_at);
    store.set_outbox_due(&first.outbox.id, t0).expect("first due time");
    store.set_outbox_due(&second.outbox.id, t0).expect("second due time");
    clock.store(t0, Ordering::SeqCst);
    engine.set_test_clock(clock);
    let sender = std::thread::spawn(move || {
        let report = engine.process_outbox(t0).expect("the batch");
        (report, engine)
    });
    second_reached_rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the second transfer did not reach DATA");
    let rows = harness.open().list_outbox(None, 50).expect("outbox");
    let sending = rows.iter().find(|row| row.state == OutboxState::Sending).expect("no row was still being submitted");
    assert_eq!(
        sending.lease_at,
        Some(t0 + 30 * 60 * 1_000),
        "the second row in the batch reused the pass-start lease instead of the time it was claimed"
    );
    release_tx.send(()).expect("release the second transfer");
    let (report, engine) = sender.join().expect("the batch panicked");
    assert_eq!(report.sent, 2, "{report:?}");
    assert_eq!(report.uncertain, 0, "{report:?}");
    assert_eq!(outbox_state(&engine).entries.len(), 2);
    for row in outbox_state(&engine).entries {
        assert_eq!(row.state, OutboxState::Sent, "{row:?}");
    }
    drop(engine);
    smtp.finish();
    imap.finish();
}

#[test]
fn a_long_submission_claims_its_append_with_the_claim_time() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let clock = Arc::new(AtomicI64::new(0));
    let smtp_clock = clock.clone();
    let smtp = RunningSmtp::start(1, move |_, mut conn| {
        let clock = smtp_clock.clone();
        async move {
            conn.greet(&["SIZE 52428800", "AUTH PLAIN"]).await;
            conn.auth_plain_ok(&expected_smtp_sasl()).await;
            conn.expect_prefix("MAIL FROM:").await;
            conn.send_line("250 ok").await;
            conn.expect_prefix("RCPT TO:").await;
            conn.send_line("250 ok").await;
            conn.expect("DATA").await;
            conn.send_line("354 go ahead").await;
            let _ = conn.read_data().await;
            // The submission takes 30 minutes of simulated time before its 250.
            clock.fetch_add(30 * 60 * 1_000, Ordering::SeqCst);
            conn.send_line("250 2.0.0 queued as 1").await;
            conn.expect("QUIT").await;
            conn.send_line("221 bye").await;
        }
    });
    let records = Arc::new(Mutex::new(Vec::new()));
    let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let imap = RunningAppendServer::start_blocking(1, records.clone(), reached_tx, release_rx);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Lenta", "corpo")).expect("draft");
    let queued = engine.message_send(&saved.summary.id).expect("queue");
    let t0 = queued.outbox.send_at;
    clock.store(t0, Ordering::SeqCst);
    engine.set_test_clock(clock);
    let id = queued.outbox.id.clone();
    let sender = std::thread::spawn(move || {
        let report = engine.process_outbox(t0).expect("the slow pass");
        (report, engine)
    });
    reached_rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the append did not start");
    let row = harness.open().get_outbox(&id).expect("row").expect("row");
    assert_eq!(row.state, OutboxState::AppendPending, "{row:?}");
    assert_eq!(row.append_attempts, 1, "{row:?}");
    assert_eq!(row.lease_at, Some(t0 + 30 * 60 * 1_000), "the append claim kept the pass-start time instead of the instant the 250 arrived");
    release_tx.send(()).expect("release the append");
    let (report, engine) = sender.join().expect("the slow pass panicked");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_eq!(outbox_state(&engine).entries[0].state, OutboxState::Sent);
    assert_eq!(records.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(), 1);
    drop(engine);
    smtp.finish();
    imap.finish();
}

#[test]
fn a_stale_engine_cannot_queue_a_draft_whose_attachments_changed() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts)]);
    let mut stale = Engine::open_with_vault(harness.open(), vault.clone()).expect("stale engine");
    let mut content = draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Allegato", "corpo");
    content.attachments =
        vec![postino_core::Attachment { id: "att_nota".to_owned(), filename: "nota.txt".to_owned(), mime: "text/plain".to_owned(), size: 5 }];
    let saved = stale.save_draft(None, None, content).expect("draft");
    let draft_id = saved.summary.id.clone();
    stale.store_attachment_bytes(&draft_id, "att_nota", b"nota\n").expect("attachment bytes");
    // Same body, subject and addresses: only the attachment set changes, which the old body/header
    // comparison did not cover.
    let mut fresh = Engine::open_with_vault(harness.open(), vault).expect("fresh engine");
    fresh
        .save_draft(None, Some(&draft_id), draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Allegato", "corpo"))
        .expect("the other process removes the attachment");
    let error = stale.message_send(&draft_id).expect_err("a stale attachment snapshot must not be queued");
    assert!(error.to_string().contains("changed in another session"), "{error}");
    assert!(harness.open().list_outbox(None, 50).expect("outbox").is_empty(), "the stale queue wrote a row");
}

#[test]
fn a_draft_with_a_reply_linkage_is_not_sent_as_a_plain_message() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts)]);
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    let saved =
        engine.save_draft(None, None, draft(vec![Address::new("", "one@example.com")], Vec::new(), Vec::new(), "Risposta", "corpo")).expect("draft");
    let draft_id = saved.summary.id.clone();
    let row = store.get_draft(&draft_id).expect("draft").expect("row");
    // Another writer links the draft to a parent. This build's composer has no reply field and the
    // serializer sends no `In-Reply-To`, so the engine must refuse instead of dropping the link.
    store
        .save_draft(&DraftUpdate {
            id: row.id,
            account_id: row.account_id,
            updated_at: row.updated_at,
            body_text: row.body_text,
            to: row.to,
            cc: row.cc,
            bcc: row.bcc,
            subject: row.subject,
            in_reply_to: Some("<parent@example.com>".to_owned()),
        })
        .expect("link the reply");
    let error = engine.message_send(&draft_id).expect_err("a linked draft must not be sent as a plain message");
    assert!(error.to_string().contains("changed in another session"), "{error}");
    assert!(harness.open().list_outbox(None, 50).expect("outbox").is_empty(), "the linked draft wrote a row");
}

#[test]
fn append_retries_back_off_thirty_then_sixty_then_two_minutes() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    let smtp = RunningSmtp::start(0, |_, _| async {});
    let imap = RunningAppendServer::start(0, Arc::new(Mutex::new(Vec::new())), true);
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = open_engine(&harness, &vault, &smtp, &imap);
    let now = 1_800_000_000_000;
    store
        .queue_outbox(&postino_store::OutboxEntry {
            id: "ob_backoff".to_owned(),
            account_id: ACCOUNT_ID.to_owned(),
            draft_id: Some("draft_missing".to_owned()),
            idempotency_key: "draft:backoff".to_owned(),
            created_at: now,
            send_at: now,
            state: OutboxState::AppendPending,
            attempts: 1,
            append_attempts: 0,
            last_error: None,
            raw_blob_hash: "a".repeat(64),
            envelope_json: "{}".to_owned(),
            lease_at: None,
        })
        .expect("an append row");
    let expected = [30_000, 60_000, 120_000];
    let mut at = now;
    for (attempt, delay) in expected.iter().enumerate() {
        let report = engine.process_outbox(at).expect("append attempt");
        assert_eq!(report.sent, 0, "pass {attempt} reported a damaged append as sent: {report:?}");
        let row = store.get_outbox("ob_backoff").expect("row").expect("row");
        assert_eq!(row.state, OutboxState::AppendPending, "{row:?}");
        assert_eq!(row.append_attempts, u32::try_from(attempt + 1).expect("attempt"), "the append attempt counter did not grow");
        assert_eq!(row.send_at, at.saturating_add(*delay), "the append backoff was not exponential");
        assert_eq!(row.lease_at, None, "a rescheduled append kept its claim");
        at = row.send_at;
    }
    assert_eq!(store.get_outbox("ob_backoff").expect("row").expect("row").append_attempts, 3);
    smtp.finish();
    imap.finish();
}

#[test]
fn the_outbox_list_pages_newest_first() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Drafts", FolderKind::Drafts)]);
    for index in 0..5u32 {
        let mut row = postino_store::OutboxEntry {
            id: format!("ob_page_{index}"),
            account_id: ACCOUNT_ID.to_owned(),
            draft_id: Some(format!("draft_page_{index}")),
            idempotency_key: format!("draft:page:{index}"),
            created_at: i64::from(index),
            send_at: i64::from(index),
            state: OutboxState::Sent,
            attempts: 1,
            append_attempts: 1,
            last_error: None,
            raw_blob_hash: "a".repeat(64),
            envelope_json: "{}".to_owned(),
            lease_at: None,
        };
        row.created_at = i64::from(index);
        store.queue_outbox(&row).expect("history row");
    }
    let engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    let page = engine.outbox_list(None, 2, 1).expect("page");
    assert_eq!(page.entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), vec!["ob_page_3", "ob_page_2"]);
    let last = engine.outbox_list(None, 2, 4).expect("last page");
    assert_eq!(last.entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), vec!["ob_page_0"]);
}
