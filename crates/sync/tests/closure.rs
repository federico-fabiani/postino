//! P5c data-integrity closure: moves, duplicate Message-IDs, UID discovery, vanished bodies,
//! cancellation during the flags scan, MIME warnings and an interrupted deletion pass.
//!
//! Every test here failed against the P5b code: it is the regression suite for the blind review.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use postino_store::{FlagUpdate, Label};
use postino_sync::{SyncError, SyncLimits, SyncPhase, read_status};
use postino_testkit::{MailboxScript, TestFolder, TestMessage, synthetic_message};
use support::{ACCOUNT_ID, Harness, RunningServer, account, folder, message_by_header, run, seeded, sync};

fn inbox(messages: Vec<TestMessage>) -> TestFolder {
    TestFolder::new("INBOX", 10).with_special_use("\\Inbox").with_messages(messages)
}

fn archive(messages: Vec<TestMessage>) -> TestFolder {
    TestFolder::new("Archive", 20).with_special_use("\\Archive").with_messages(messages)
}

fn label(store: &postino_store::Store, message_id: &str) {
    store
        .create_label(&Label {
            id: "lbl_1".to_owned(),
            account_id: Some(ACCOUNT_ID.to_owned()),
            name: "Lavoro".to_owned(),
            color: "forest".to_owned(),
        })
        .expect("create the label");
    store.apply_label("lbl_1", std::slice::from_ref(&message_id.to_owned())).expect("apply the label");
}

fn all_ids(store: &postino_store::Store) -> HashSet<String> {
    store.list_all_messages().expect("list messages").into_iter().map(|message| message.summary.id).collect()
}

#[test]
fn a_message_moved_between_folders_keeps_its_labels_and_read_state() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(vec![inbox(vec![TestMessage::synthetic(synthetic_message(1)).with_flags(&["\\Seen"])]), archive(Vec::new())]);
    sync(&store, &server).expect("first sync");
    server.finish();

    let message = message_by_header(&store, "synthetic-1@example.test");
    assert!(!message.summary.unread);
    label(&store, &message.summary.id);
    // A local read edit that the server does not know about. The move must not replace it with the
    // server's \Seen.
    store.set_flags(std::slice::from_ref(&message.summary.id), FlagUpdate { unread: Some(true), starred: None }).expect("local read edit");

    let server = RunningServer::start(vec![inbox(Vec::new()), archive(vec![TestMessage::synthetic(synthetic_message(1)).with_flags(&["\\Seen"])])]);
    let outcome = sync(&store, &server).expect("second sync");
    server.finish();

    assert_eq!(outcome.removed, 0, "the moved message was deleted from the source folder");
    assert_eq!(store.list_all_messages().expect("messages").len(), 1, "the move created a duplicate or lost the row");
    let moved = store.get_message(&message.summary.id).expect("read").expect("the message survived");
    assert_eq!(moved.summary.folder_id, folder(&store, "Archive").id);
    assert_eq!(store.labels_for(&message.summary.id).expect("labels"), vec!["lbl_1".to_owned()], "the label was lost with the old row");
    assert!(moved.summary.unread, "the local read state was replaced by the server's flags");
}

#[test]
fn max_messages_stops_after_the_source_folder_and_skips_deletion() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(vec![inbox(vec![TestMessage::synthetic(synthetic_message(1)).with_flags(&["\\Seen"])]), archive(Vec::new())]);
    sync(&store, &server).expect("first sync");
    server.finish();
    let message = message_by_header(&store, "synthetic-1@example.test");
    label(&store, &message.summary.id);

    // INBOX has a new UID 2; the old UID 1 moved to Archive. One message per run stops the run after
    // INBOX, before Archive is scanned. Nothing may be deleted on a partial scan.
    let server = RunningServer::start(vec![
        inbox(vec![TestMessage::synthetic(synthetic_message(2))]),
        archive(vec![TestMessage::synthetic(synthetic_message(1))]),
    ]);
    let limits = SyncLimits { max_messages: 1, ..SyncLimits::default() };
    let outcome = run(&store, &server, &limits, &AtomicBool::new(false)).expect("sync");
    server.finish();

    assert_eq!(outcome.fetched, 1);
    assert_eq!(outcome.removed, 0);
    assert!(outcome.warnings.iter().any(|warning| warning.contains("per-run limit")), "{:?}", outcome.warnings);
    let kept = store.get_message(&message.summary.id).expect("read").expect("a partial run deleted the moved message");
    assert_eq!(kept.summary.folder_id, folder(&store, "INBOX").id, "the row was re-homed by a folder that was never scanned");
    assert_eq!(store.labels_for(&message.summary.id).expect("labels"), vec!["lbl_1".to_owned()]);
}

#[test]
fn duplicate_message_ids_in_one_folder_get_distinct_rows() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let first = TestMessage::synthetic(synthetic_message(1)).with_message_id("<duplicate@example.test>");
    let second = TestMessage::synthetic(synthetic_message(2)).with_message_id("<duplicate@example.test>");
    let server = RunningServer::start(vec![inbox(vec![first.clone(), second.clone()])]);
    let outcome = sync(&store, &server).expect("first sync");
    server.finish();

    assert_eq!(outcome.fetched, 2);
    let rows = store.list_all_messages().expect("messages");
    assert_eq!(rows.len(), 2, "two distinct messages collapsed into one row");
    let first_row = rows.iter().find(|row| row.body_text.contains("number 1")).expect("first body");
    let second_row = rows.iter().find(|row| row.body_text.contains("number 2")).expect("second body");
    assert_eq!(first_row.summary.subject, "Synthetic message 1");
    assert_eq!(second_row.summary.subject, "Synthetic message 2");
    assert_ne!(first_row.summary.id, second_row.summary.id);
    assert_eq!(first_row.summary.id.len(), 20, "the first duplicate keeps the plain hash id");
    assert_eq!(second_row.summary.id, format!("{}_2", first_row.summary.id));

    label(&store, &first_row.summary.id);
    let inbox_row = folder(&store, "INBOX");
    // A normal run has no new mail; a lost cursor forces both messages through ingest again, which
    // is where the disambiguation has to be stable.
    for _ in 0..2 {
        store.clear_sync_state(&inbox_row.id).expect("reset the cursor");
        let server = RunningServer::start(vec![inbox(vec![first.clone(), second.clone()])]);
        let outcome = sync(&store, &server).expect("re-sync");
        server.finish();
        assert_eq!(outcome.fetched, 2);
        let rows = store.list_all_messages().expect("messages");
        assert_eq!(rows.len(), 2, "the re-sync collapsed or duplicated a row");
        assert_eq!(all_ids(&store).len(), 2);
        assert_eq!(store.labels_for(&first_row.summary.id).expect("labels"), vec!["lbl_1".to_owned()], "the label moved to the other duplicate");
        assert!(store.labels_for(&second_row.summary.id).expect("labels").is_empty());
    }
}

#[test]
fn a_missing_uidnext_is_discovered_up_to_uid_250() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server =
        RunningServer::start_with(vec![inbox(vec![TestMessage::synthetic(synthetic_message(250))])], MailboxScript::default().without_uidnext());
    let outcome = sync(&store, &server).expect("sync");
    server.finish();

    assert_eq!(outcome.fetched, 1, "UID 250 was hidden because the server omitted UIDNEXT");
    let row = message_by_header(&store, "synthetic-250@example.test");
    assert_eq!(row.uid, Some(250));
    let inbox_row = folder(&store, "INBOX");
    assert_eq!(store.get_folder(&inbox_row.id).expect("folder").expect("row").uid_next, None);
    assert_eq!(store.get_sync_state(&inbox_row.id).expect("state").expect("row").uid_next, 251);
}

#[test]
fn a_low_uidnext_does_not_hide_or_delete_a_higher_live_uid() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let messages = vec![TestMessage::synthetic(synthetic_message(1)), TestMessage::synthetic(synthetic_message(250))];
    let server = RunningServer::start(vec![inbox(messages.clone())]);
    sync(&store, &server).expect("first sync");
    server.finish();
    assert_eq!(store.list_all_messages().expect("messages").len(), 2);

    let server = RunningServer::start_with(vec![inbox(messages)], MailboxScript::default().with_uid_next_hint(3));
    let outcome = sync(&store, &server).expect("second sync");
    server.finish();

    assert_eq!(outcome.removed, 0, "a live UID above a stale UIDNEXT was deleted");
    assert_eq!(store.list_all_messages().expect("messages").len(), 2);
    assert!(store.get_message(&message_by_header(&store, "synthetic-250@example.test").summary.id).expect("read").is_some());
}

#[test]
fn an_exists_zero_does_not_hide_a_live_message() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server =
        RunningServer::start_with(vec![inbox(vec![TestMessage::synthetic(synthetic_message(1))])], MailboxScript::default().with_exists_hint(0));
    let outcome = sync(&store, &server).expect("sync");
    server.finish();
    assert_eq!(outcome.fetched, 1, "EXISTS 0 was treated as liveness");
    assert!(store.get_message(&message_by_header(&store, "synthetic-1@example.test").summary.id).expect("read").is_some());
}

#[test]
fn an_empty_mailbox_syncs_cleanly() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(vec![inbox(Vec::new())]);
    let outcome = sync(&store, &server).expect("sync");
    server.finish();
    assert_eq!(outcome.fetched, 0);
    assert!(store.list_all_messages().expect("messages").is_empty());
    assert_eq!(read_status(&store, &account()).expect("status").expect("a status").state, SyncPhase::Idle);
}

#[test]
fn a_vanished_body_is_skipped_with_a_warning_and_later_folders_sync() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start_with(
        vec![
            inbox(vec![TestMessage::synthetic(synthetic_message(1)), TestMessage::synthetic(synthetic_message(2))]),
            archive(vec![TestMessage::synthetic(synthetic_message(3))]),
        ],
        MailboxScript::default().vanish_body(2),
    );
    let outcome = sync(&store, &server).expect("one vanished body must not abort the account");
    server.finish();

    assert_eq!(outcome.fetched, 2, "the first message and the later folder must both be cached");
    assert!(store.get_message(&message_by_header(&store, "synthetic-1@example.test").summary.id).expect("read").is_some());
    assert!(store.get_message(&message_by_header(&store, "synthetic-3@example.test").summary.id).expect("read").is_some());
    assert!(store.list_all_messages().expect("messages").iter().all(|message| !message.summary.subject.contains("message 2")));
    assert!(outcome.warnings.iter().any(|warning| warning.contains("UID 2") && warning.to_lowercase().contains("vanish")), "{:?}", outcome.warnings);
}

#[test]
fn cancellation_reaches_the_uid_flags_scan() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let messages = (1..=1_100u32).map(|uid| TestMessage::synthetic(synthetic_message(uid))).collect::<Vec<_>>();
    let cancel = Arc::new(AtomicBool::new(false));
    let script = MailboxScript::default().cancel_after_flags(2, cancel.clone());
    let server = RunningServer::start_with(vec![inbox(messages)], script);
    let error = run(&store, &server, &SyncLimits::default(), &cancel).expect_err("the scan must observe the token");
    assert!(matches!(error, SyncError::Cancelled), "{error:?}");
    server.finish();

    assert_eq!(store.list_all_messages().expect("messages").len(), 1_100, "committed messages were lost");
    let status = read_status(&store, &account()).expect("status").expect("a status");
    assert_eq!(status.state, SyncPhase::Error);
    assert_eq!(status.error.as_deref(), Some("the sync was cancelled"));
}

fn many_parts_message(uid: u32, parts: usize) -> TestMessage {
    let message_id = format!("<many-{uid}@example.test>");
    let mut raw = String::new();
    raw.push_str(&format!("From: Synthetic Sender <many{uid}@example.test>\r\n"));
    raw.push_str("To: Postino Test <postino@example.test>\r\n");
    raw.push_str(&format!("Subject: Many parts {uid}\r\n"));
    raw.push_str("Date: Mon, 1 Jan 2024 10:00:00 +0000\r\n");
    raw.push_str(&format!("Message-ID: {message_id}\r\n"));
    raw.push_str("MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"many\"\r\n\r\n");
    for index in 0..parts {
        raw.push_str("--many\r\nContent-Type: text/plain\r\n\r\n");
        raw.push_str(&format!("part {index}\r\n"));
    }
    raw.push_str("--many--\r\n");
    TestMessage {
        uid,
        flags: Vec::new(),
        raw: raw.into_bytes(),
        internal_date: "01-Jan-2024 10:00:00 +0000".to_owned(),
        date: "Mon, 1 Jan 2024 10:00:00 +0000".to_owned(),
        subject: format!("Many parts {uid}"),
        from_name: "Synthetic Sender".to_owned(),
        from_mailbox: format!("many{uid}"),
        from_host: "example.test".to_owned(),
        message_id: Some(message_id),
    }
}

#[test]
fn mime_warnings_are_kept_with_the_message_id() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(vec![inbox(vec![many_parts_message(1, 300)])]);
    let outcome = sync(&store, &server).expect("sync");
    server.finish();

    assert_eq!(outcome.fetched, 1);
    let row = message_by_header(&store, "many-1@example.test");
    assert!(
        outcome.warnings.iter().any(|warning| warning.contains(&row.summary.id) && warning.contains("part limit reached")),
        "the MIME warning was discarded: {:?}",
        outcome.warnings
    );
}

#[test]
fn a_move_without_a_message_id_is_not_removed_when_the_run_stops_early() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let mut first = TestMessage::synthetic(synthetic_message(1));
    first.message_id = None;
    first.raw = String::from_utf8_lossy(&first.raw).replace("Message-ID: <synthetic-1@example.test>\r\n", "").into_bytes();
    let server = RunningServer::start(vec![inbox(vec![first.clone()]), archive(Vec::new())]);
    sync(&store, &server).expect("first sync");
    server.finish();
    let inbox_row = folder(&store, "INBOX");
    let old_id = store.list_all_messages().expect("messages").first().expect("one row").summary.id.clone();

    // A run that stops after INBOX: the old row has no Message-ID, the new copy was not read yet.
    // It must stay until the Archive copy exists.
    let server = RunningServer::start(vec![inbox(vec![TestMessage::synthetic(synthetic_message(2))]), archive(vec![first])]);
    let limits = SyncLimits { max_messages: 1, ..SyncLimits::default() };
    let outcome = run(&store, &server, &limits, &AtomicBool::new(false)).expect("second sync");
    server.finish();
    assert_eq!(outcome.removed, 0);
    let kept = store.get_message(&old_id).expect("read").expect("the old no-id row was deleted before the new copy existed");
    assert_eq!(kept.summary.folder_id, inbox_row.id);
}
