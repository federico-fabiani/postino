//! Reconciliation against the scripted mailbox: initial, incremental, UIDVALIDITY, deletion, flags,
//! limits, cancellation, network drops, `BODY.PEEK` and the offline read.

mod support;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, atomic::AtomicBool as SharedBool};

use postino_core::Engine;
use postino_store::{FlagUpdate, FolderKind, Label};
use postino_sync::{SyncError, SyncLimits, SyncPhase, read_status};
use postino_testkit::{
    FailPoint, MailboxScript, TestFolder, TestMessage, fetch_body_response, serve_mailbox, synthetic_html_message, synthetic_message,
    synthetic_message_with_attachment,
};
use support::{ACCOUNT_ID, Harness, RunningServer, account, assert_consistent, folder, message_by_header, run, seeded, sync};

const ATTACHMENT_BYTES: &[u8] = b"postino attachment bytes, stored encrypted";

fn messages(uids: impl IntoIterator<Item = u32>) -> Vec<TestMessage> {
    uids.into_iter().map(|uid| TestMessage::synthetic(synthetic_message(uid))).collect()
}

fn inbox(messages: Vec<TestMessage>) -> TestFolder {
    TestFolder::new("INBOX", 10).with_special_use("\\Inbox").with_messages(messages)
}

fn one_inbox(uid_validity: u32, messages: Vec<TestMessage>) -> Vec<TestFolder> {
    vec![TestFolder::new("INBOX", uid_validity).with_special_use("\\Inbox").with_messages(messages)]
}

#[test]
fn status_is_absent_before_the_first_run() {
    let harness = Harness::new();
    let store = seeded(&harness);
    assert_eq!(read_status(&store, &account()).expect("read status"), None);
}

#[test]
fn initial_sync_caches_folders_messages_bodies_attachments_and_flags() {
    let harness = Harness::new();
    let store = seeded(&harness);

    let mut inbox_messages = Vec::new();
    inbox_messages.push(TestMessage::synthetic(synthetic_html_message(1)).with_flags(&["\\Seen"]));
    inbox_messages.push(TestMessage::synthetic(synthetic_message_with_attachment(2, "report.pdf", "application/pdf", ATTACHMENT_BYTES)));
    for uid in 3..=40 {
        let message = TestMessage::synthetic(synthetic_message(uid));
        inbox_messages.push(if uid == 3 { message.with_flags(&["\\Flagged"]) } else { message });
    }
    let folder_count = inbox_messages.len();
    let folders = vec![
        inbox(inbox_messages),
        TestFolder::new("Sent", 20).with_special_use("\\Sent").with_messages(messages(41..=48)),
        TestFolder::new("Trash", 30).with_special_use("\\Trash").with_messages(messages(49..=50)),
    ];
    let server = RunningServer::start(folders);
    let outcome = sync(&store, &server).expect("initial sync");
    server.finish();

    assert_eq!(outcome.account, ACCOUNT_ID);
    assert_eq!(outcome.folders, 3);
    assert_eq!(outcome.fetched, 50);
    assert_eq!(outcome.removed, 0);
    assert_eq!(outcome.attachments_stored, 1);
    assert!(outcome.warnings.iter().any(|warning| warning.contains("Drafts")), "{:?}", outcome.warnings);

    let folders = store.list_folders(Some(ACCOUNT_ID)).expect("folders");
    assert_eq!(folders.len(), 3);
    let inbox_row = folders.iter().find(|folder| folder.name == "INBOX").expect("inbox");
    assert_eq!(inbox_row.kind, FolderKind::Inbox);
    assert_eq!(inbox_row.uid_validity, Some(10));
    assert_eq!(inbox_row.uid_next, Some(41));
    assert_eq!(inbox_row.total, u32::try_from(folder_count).expect("count"));
    assert_eq!(inbox_row.unread, u32::try_from(folder_count).expect("count") - 1);
    assert!(inbox_row.last_sync_at.is_some());
    let sent = folders.iter().find(|folder| folder.name == "Sent").expect("sent");
    assert_eq!(sent.kind, FolderKind::Sent);
    assert_eq!(sent.total, 8);
    let trash = folders.iter().find(|folder| folder.name == "Trash").expect("trash");
    assert_eq!(trash.kind, FolderKind::Trash);
    assert_eq!(trash.total, 2);

    let state = store.get_sync_state(&inbox_row.id).expect("state").expect("row");
    assert_eq!(state.uid_next, 41);

    let all = store.list_all_messages().expect("messages");
    assert_eq!(all.len(), 50);
    let first = message_by_header(&store, "synthetic-1@example.test");
    assert!(first.summary.id.starts_with("msg_"));
    assert_eq!(first.summary.id.len(), 4 + 16);
    assert!(!first.summary.unread);
    assert_eq!(first.summary.from.name, "Synthetic Sender");
    assert_eq!(first.summary.from.email, "synthetic1@example.test");
    assert_eq!(first.summary.to.first().map(|address| address.email.as_str()), Some("postino@example.test"));
    assert_eq!(first.summary.subject, "Synthetic HTML message 1");
    assert_eq!(first.summary.date, 1_704_103_200_000);
    assert_eq!(first.summary.thread_id, "synthetic-1@example.test");
    assert_eq!(first.message_id.as_deref(), Some("synthetic-1@example.test"));
    assert!(!first.summary.snippet.is_empty());
    let html = first.body_html.as_deref().expect("sanitized html");
    assert!(html.contains("Hello"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("tracker.example.test"), "{html}");
    assert!(first.body_text.contains("number 1"), "{}", first.body_text);
    assert!(first.size.is_some_and(|size| size > 0));

    let second = message_by_header(&store, "synthetic-2@example.test");
    let attachments = store.attachments(&second.summary.id).expect("attachments");
    assert_eq!(attachments.len(), 1);
    let attachment = attachments.first().expect("one attachment");
    assert_eq!(attachment.filename, "report.pdf");
    assert_eq!(attachment.mime, "application/pdf");
    assert_eq!(attachment.size, u64::try_from(ATTACHMENT_BYTES.len()).expect("len"));
    let hash = attachment.blob_hash.as_deref().expect("blob hash");
    assert_eq!(store.blobs().get(hash, 1024 * 1024).expect("stored bytes").as_slice(), ATTACHMENT_BYTES);

    let third = message_by_header(&store, "synthetic-3@example.test");
    assert!(third.summary.starred);
    assert!(third.summary.unread);

    let status = read_status(&store, &account()).expect("status").expect("a status");
    assert_eq!(status.state, SyncPhase::Idle);
    assert!(status.last_success.is_some());
    assert_eq!(status.folders_total, 3);
    assert_eq!(status.folders_done, 3);
    assert_eq!(status.messages, 50);
    assert!(status.error.is_none());
    assert_consistent(&store);
}

#[test]
fn aggregate_views_do_not_steal_messages_from_real_folders() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let folders = vec![
        inbox(vec![TestMessage::synthetic(synthetic_message(1))]),
        TestFolder::new("All Mail", 50)
            .with_special_use("\\All")
            .with_messages(vec![TestMessage::synthetic(synthetic_message(1)), TestMessage::synthetic(synthetic_message(2))]),
        TestFolder::new("Starred", 60)
            .with_special_use("\\Flagged")
            .with_messages(vec![TestMessage::synthetic(synthetic_message(2)).with_flags(&["\\Flagged"])]),
    ];
    let server = RunningServer::start(folders);
    let outcome = sync(&store, &server).expect("sync");
    server.finish();

    assert_eq!(outcome.fetched, 1, "the aggregate views were fetched as mail folders");
    assert_eq!(outcome.folders, 1);
    assert!(outcome.warnings.iter().any(|warning| warning.contains("aggregate view")), "{:?}", outcome.warnings);
    let folders = store.list_folders(Some(ACCOUNT_ID)).expect("folders");
    assert_eq!(folders.len(), 3);
    let inbox_row = folders.iter().find(|folder| folder.name == "INBOX").expect("inbox");
    assert_eq!(inbox_row.total, 1);
    let all = folders.iter().find(|folder| folder.name == "All Mail").expect("all");
    assert_eq!(all.total, 0);
    let starred = folders.iter().find(|folder| folder.name == "Starred").expect("starred");
    assert_eq!(starred.kind, FolderKind::Starred);
    assert_eq!(starred.total, 0);
    assert_eq!(message_by_header(&store, "synthetic-1@example.test").summary.folder_id, inbox_row.id);
}

#[test]
fn incremental_sync_fetches_only_new_uids_and_keeps_labels() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(one_inbox(10, messages(1..=3)));
    sync(&store, &server).expect("first sync");
    server.finish();

    let inbox_row = folder(&store, "INBOX");
    let first = message_by_header(&store, "synthetic-1@example.test");
    store
        .create_label(&Label {
            id: "lbl_1".to_owned(),
            account_id: Some(ACCOUNT_ID.to_owned()),
            name: "Lavoro".to_owned(),
            color: "forest".to_owned(),
        })
        .expect("label");
    store.apply_label("lbl_1", std::slice::from_ref(&first.summary.id)).expect("apply");

    let second_folders = one_inbox(10, messages(1..=4));
    let server = RunningServer::start_handler(move |conn| async move {
        let conn = serve_mailbox(conn, second_folders).await;
        let first_fetch = conn.received().iter().find(|line| line.contains("UID FETCH") && line.contains("ENVELOPE")).expect("an envelope fetch");
        assert!(first_fetch.contains("UID FETCH 4 "), "the second run must start at UID 4, got {first_fetch}");
        assert!(!first_fetch.contains("UID FETCH 1"), "the second run re-fetched the old window: {first_fetch}");
    });
    let outcome = sync(&store, &server).expect("second sync");
    server.finish();

    assert_eq!(outcome.fetched, 1);
    assert_eq!(outcome.removed, 0);
    let all = store.list_all_messages().expect("messages");
    assert_eq!(all.len(), 4);
    let ids = all.iter().map(|message| message.summary.id.clone()).collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), 4, "the sync created a duplicate");
    assert_eq!(store.labels_for(&first.summary.id).expect("labels"), vec!["lbl_1".to_owned()]);
    assert_eq!(store.get_sync_state(&inbox_row.id).expect("state").expect("row").uid_next, 5);
    assert_consistent(&store);
}

#[test]
fn a_uidvalidity_change_replaces_one_folder_and_keeps_another() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let first_folders = vec![inbox(messages(1..=2)), TestFolder::new("Archive", 20).with_special_use("\\Archive").with_messages(messages([101]))];
    let server = RunningServer::start(first_folders);
    sync(&store, &server).expect("first sync");
    server.finish();

    let old = message_by_header(&store, "synthetic-1@example.test");
    let archived = message_by_header(&store, "synthetic-101@example.test");
    store
        .create_label(&Label {
            id: "lbl_1".to_owned(),
            account_id: Some(ACCOUNT_ID.to_owned()),
            name: "Lavoro".to_owned(),
            color: "forest".to_owned(),
        })
        .expect("label");
    store.apply_label("lbl_1", &[old.summary.id.clone(), archived.summary.id.clone()]).expect("apply");

    let second_folders = vec![
        TestFolder::new("INBOX", 11).with_special_use("\\Inbox").with_messages(messages([3, 4])),
        TestFolder::new("Archive", 20).with_special_use("\\Archive").with_messages(messages([101])),
    ];
    let server = RunningServer::start(second_folders);
    let outcome = sync(&store, &server).expect("second sync");
    server.finish();

    assert_eq!(outcome.fetched, 2);
    assert!(outcome.warnings.iter().any(|warning| warning.contains("UIDVALIDITY changed")), "{:?}", outcome.warnings);
    assert_eq!(store.get_message(&old.summary.id).expect("read"), None, "the old UID identity survived the reset");
    assert!(store.labels_for(&old.summary.id).expect("labels").is_empty());
    assert!(message_by_header(&store, "synthetic-3@example.test").summary.unread);
    assert!(message_by_header(&store, "synthetic-4@example.test").summary.unread);
    assert_eq!(store.labels_for(&archived.summary.id).expect("labels"), vec!["lbl_1".to_owned()], "another folder's label was lost");
    let archive = folder(&store, "Archive");
    assert_eq!(archive.uid_validity, Some(20));
    assert_eq!(archive.total, 1);
    let inbox_row = folder(&store, "INBOX");
    assert_eq!(inbox_row.uid_validity, Some(11));
    assert_eq!(inbox_row.uid_next, Some(5));
    assert_eq!(store.get_sync_state(&inbox_row.id).expect("state").expect("row").uid_next, 5);
    assert_consistent(&store);
}

#[test]
fn a_deletion_pass_removes_the_uid_that_disappeared() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(one_inbox(10, messages(1..=3)));
    sync(&store, &server).expect("first sync");
    server.finish();

    let removed_message = message_by_header(&store, "synthetic-2@example.test");
    store
        .create_label(&Label {
            id: "lbl_1".to_owned(),
            account_id: Some(ACCOUNT_ID.to_owned()),
            name: "Lavoro".to_owned(),
            color: "forest".to_owned(),
        })
        .expect("label");
    store.apply_label("lbl_1", std::slice::from_ref(&removed_message.summary.id)).expect("apply");
    let inbox_row = folder(&store, "INBOX");

    let server = RunningServer::start(one_inbox(10, messages([1, 3])));
    let outcome = sync(&store, &server).expect("second sync");
    server.finish();

    assert_eq!(outcome.removed, 1);
    assert_eq!(outcome.fetched, 0);
    assert_eq!(store.get_message(&removed_message.summary.id).expect("read"), None);
    assert!(store.labels_for(&removed_message.summary.id).expect("labels").is_empty());
    assert_eq!(store.list_all_messages().expect("messages").len(), 2);
    assert_eq!(store.counts(Some(&inbox_row.id)).expect("counts").total, 2);
    assert_eq!(store.get_folder(&inbox_row.id).expect("folder").expect("row").total, 2);
    assert_eq!(store.get_sync_state(&inbox_row.id).expect("state").expect("row").uid_next, 4);
    assert_consistent(&store);
}

#[test]
fn server_flags_are_applied_on_insert_and_local_edits_stay_with_a_drift_warning() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let folders = vec![inbox(vec![
        TestMessage::synthetic(synthetic_message(1)).with_flags(&["\\Seen"]),
        TestMessage::synthetic(synthetic_message(2)).with_flags(&["\\Flagged"]),
        TestMessage::synthetic(synthetic_message(3)),
    ])];
    let server = RunningServer::start(folders);
    sync(&store, &server).expect("first sync");
    server.finish();

    let first = message_by_header(&store, "synthetic-1@example.test");
    let second = message_by_header(&store, "synthetic-2@example.test");
    let third = message_by_header(&store, "synthetic-3@example.test");
    assert!(!first.summary.unread);
    assert!(second.summary.starred);
    assert!(third.summary.unread);

    // The user reads the first message and unstars the second. M1 flags are local-only.
    store.set_flags(std::slice::from_ref(&first.summary.id), FlagUpdate { unread: Some(true), starred: None }).expect("flag");
    store.set_flags(std::slice::from_ref(&second.summary.id), FlagUpdate { unread: None, starred: Some(false) }).expect("flag");

    let folders = vec![inbox(vec![
        TestMessage::synthetic(synthetic_message(1)).with_flags(&["\\Seen"]),
        TestMessage::synthetic(synthetic_message(2)).with_flags(&["\\Flagged"]),
        TestMessage::synthetic(synthetic_message(3)),
    ])];
    let server = RunningServer::start_handler(move |conn| async move {
        let conn = serve_mailbox(conn, folders).await;
        assert!(conn.received().iter().all(|line| !line.contains("STORE")), "sync pushed flags to the server");
    });
    let outcome = sync(&store, &server).expect("second sync");
    server.finish();

    assert_eq!(outcome.fetched, 0);
    assert_eq!(outcome.updated_flags, 2);
    let drift: Vec<_> = outcome.warnings.iter().filter(|warning| warning.contains("differ from the server's read/starred flags")).collect();
    assert_eq!(drift.len(), 1, "one drift warning per run, got {:?}", outcome.warnings);
    let first_after = message_by_header(&store, "synthetic-1@example.test");
    let second_after = message_by_header(&store, "synthetic-2@example.test");
    let third_after = message_by_header(&store, "synthetic-3@example.test");
    assert!(first_after.summary.unread, "a local read edit was overwritten");
    assert!(!second_after.summary.starred, "a local star edit was overwritten");
    assert!(third_after.summary.unread);
}

#[test]
fn an_attachment_over_the_limit_keeps_metadata_and_warns() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let oversized = vec![0x42u8; 4096];
    let folders = vec![inbox(vec![TestMessage::synthetic(synthetic_message_with_attachment(1, "big.bin", "application/octet-stream", &oversized))])];
    let server = RunningServer::start(folders);
    let limits = SyncLimits { max_attachment_bytes: 1024, ..SyncLimits::default() };
    let outcome = run(&store, &server, &limits, &AtomicBool::new(false)).expect("sync");
    server.finish();

    assert_eq!(outcome.fetched, 1);
    assert_eq!(outcome.attachments_stored, 0);
    assert!(outcome.warnings.iter().any(|warning| warning.contains("over the 1024-byte per-attachment limit")), "{:?}", outcome.warnings);
    let stored = message_by_header(&store, "synthetic-1@example.test");
    assert!(stored.summary.has_attachments);
    assert!(stored.body_text.contains("number 1"));
    let attachments = store.attachments(&stored.summary.id).expect("attachments");
    assert_eq!(attachments.len(), 1);
    let attachment = attachments.first().expect("attachment");
    assert_eq!(attachment.size, 4096);
    assert!(attachment.blob_hash.is_none(), "an oversized attachment was stored anyway");
    assert_consistent(&store);
}

#[test]
fn cancellation_keeps_committed_messages_and_a_reopen_resumes_without_duplicates() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let cancel = Arc::new(SharedBool::new(false));
    let script = MailboxScript::default().cancel_after_body(3, cancel.clone());
    let server = RunningServer::start_with(one_inbox(10, messages(1..=6)), script);
    let limits = SyncLimits { batch: 2, ..SyncLimits::default() };
    let error = run(&store, &server, &limits, &cancel).expect_err("the run must stop");
    assert!(matches!(error, SyncError::Cancelled), "{error:?}");
    server.finish();

    assert_eq!(store.list_all_messages().expect("messages").len(), 3, "the cancelled run lost or invented messages");
    assert_consistent(&store);
    let inbox_row = folder(&store, "INBOX");
    assert_eq!(store.get_sync_state(&inbox_row.id).expect("state").expect("row").uid_next, 4);
    let status = read_status(&store, &account()).expect("status").expect("a status");
    assert_eq!(status.state, SyncPhase::Error);
    assert_eq!(status.error.as_deref(), Some("the sync was cancelled"));
    assert_eq!(status.messages, 3);
    drop(store);

    let store = harness.reopen();
    let server = RunningServer::start(one_inbox(10, messages(1..=6)));
    let outcome = sync(&store, &server).expect("resume");
    server.finish();
    assert_eq!(outcome.fetched, 3);
    assert_eq!(store.list_all_messages().expect("messages").len(), 6);
    let ids = support::all_message_ids(&store).into_iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), 6, "the resume duplicated a message");
    assert_consistent(&store);
    let status = read_status(&store, &account()).expect("status").expect("a status");
    assert_eq!(status.state, SyncPhase::Idle);
    assert!(status.last_success.is_some());
}

#[test]
fn a_drop_before_the_first_message_leaves_an_empty_consistent_cache() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start_with(one_inbox(10, messages(1..=3)), MailboxScript::default().fail(FailPoint::OnSelect(1)));
    let error = sync(&store, &server).expect_err("the server vanished");
    assert!(matches!(error, SyncError::Connect(_)), "{error:?}");
    server.finish();
    assert!(store.list_all_messages().expect("messages").is_empty());
    assert_consistent(&store);

    let server = RunningServer::start(one_inbox(10, messages(1..=3)));
    let outcome = sync(&store, &server).expect("resume");
    server.finish();
    assert_eq!(outcome.fetched, 3);
    assert_eq!(store.list_all_messages().expect("messages").len(), 3);
    assert_consistent(&store);
}

#[test]
fn a_drop_mid_batch_keeps_the_committed_prefix_and_resumes() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start_with(one_inbox(10, messages(1..=3)), MailboxScript::default().fail(FailPoint::OnBody(2)));
    let limits = SyncLimits { batch: 3, ..SyncLimits::default() };
    let error = run(&store, &server, &limits, &AtomicBool::new(false)).expect_err("the server vanished");
    assert!(matches!(error, SyncError::Connect(_)), "{error:?}");
    server.finish();
    assert_eq!(store.list_all_messages().expect("messages").len(), 1, "the committed prefix was lost");
    assert_consistent(&store);

    let server = RunningServer::start(one_inbox(10, messages(1..=3)));
    let outcome = sync(&store, &server).expect("resume");
    server.finish();
    assert_eq!(outcome.fetched, 2);
    assert_eq!(store.list_all_messages().expect("messages").len(), 3);
    assert_consistent(&store);
}

#[test]
fn a_drop_after_the_new_mail_pass_keeps_every_committed_message() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start_with(one_inbox(10, messages(1..=3)), MailboxScript::default().fail(FailPoint::OnEnvelopeFetch(2)));
    let error = sync(&store, &server).expect_err("the deletion pass lost the connection");
    assert!(matches!(error, SyncError::Connect(_)), "{error:?}");
    server.finish();
    assert_eq!(store.list_all_messages().expect("messages").len(), 3);
    assert_consistent(&store);

    let server = RunningServer::start(one_inbox(10, messages(1..=3)));
    let outcome = sync(&store, &server).expect("resume");
    server.finish();
    assert_eq!(outcome.fetched, 0);
    assert_eq!(outcome.removed, 0);
    assert_eq!(store.list_all_messages().expect("messages").len(), 3);
    assert_consistent(&store);
}

#[test]
fn sync_only_reads_with_body_peek() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let folders = one_inbox(10, messages(1..=3));
    let server = RunningServer::start_handler(move |conn| async move {
        let conn = serve_mailbox(conn, folders).await;
        let lines = conn.received();
        assert!(lines.iter().all(|line| !line.contains("STORE")), "sync sent STORE: {lines:?}");
        assert!(lines.iter().all(|line| !line.contains("BODY[]")), "sync fetched with BODY[]: {lines:?}");
        assert!(lines.iter().all(|line| !line.contains("EXPUNGE") && !line.contains("DELETE")), "sync mutated the server: {lines:?}");
        assert_eq!(lines.iter().filter(|line| line.contains("BODY.PEEK[]")).count(), 3, "every body must use BODY.PEEK[]: {lines:?}");
    });
    let outcome = sync(&store, &server).expect("sync");
    server.finish();
    assert_eq!(outcome.fetched, 3);
}

#[test]
fn a_synced_mailbox_is_readable_offline_through_the_engine() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let folders =
        vec![inbox(vec![TestMessage::synthetic(synthetic_message_with_attachment(1, "report.pdf", "application/pdf", b"offline attachment bytes"))])];
    let server = RunningServer::start(folders);
    sync(&store, &server).expect("sync");
    server.finish();

    let message = store.list_all_messages().expect("messages").into_iter().next().expect("one message");
    let attachment = store.attachments(&message.summary.id).expect("attachments").into_iter().next().expect("one attachment");
    let inbox_id = folder(&store, "INBOX").id;
    drop(store);

    // The server is gone: its thread is joined and its listener closed.
    let mut engine = Engine::open(harness.reopen()).expect("open the engine on the store");
    let listed = engine.call("messages_list", serde_json::json!({ "account": ACCOUNT_ID, "folder": inbox_id })).expect("list");
    assert_eq!(listed.get("total").and_then(serde_json::Value::as_u64), Some(1), "{listed}");
    let read = engine.call("message_get", serde_json::json!({ "id": message.summary.id })).expect("read");
    let body = read.pointer("/message/body_text").and_then(serde_json::Value::as_str).expect("body text");
    assert!(body.contains("number 1"), "{body}");
    assert_eq!(engine.attachment_bytes(&attachment.id).expect("attachment bytes"), b"offline attachment bytes");
}

#[test]
fn a_message_over_the_message_limit_is_skipped_with_a_warning() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(one_inbox(10, messages(1..=2)));
    let limits = SyncLimits { max_message_bytes: 32, ..SyncLimits::default() };
    let outcome = run(&store, &server, &limits, &AtomicBool::new(false)).expect("sync");
    server.finish();
    assert_eq!(outcome.fetched, 0);
    assert!(outcome.warnings.iter().any(|warning| warning.contains("over the 32-byte message limit")), "{:?}", outcome.warnings);
    assert!(store.list_all_messages().expect("messages").is_empty());
    // The cursor still advanced, so the next run does not stall on the same messages.
    let inbox_row = folder(&store, "INBOX");
    assert_eq!(store.get_sync_state(&inbox_row.id).expect("state").expect("row").uid_next, 3);
}

#[test]
fn a_parse_failure_is_a_warning_not_a_failed_run() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let mut broken = TestMessage::synthetic(synthetic_message(1));
    let mut raw = String::from("From: broken@example.test\r\nSubject: too deep\r\n");
    for index in 0..65 {
        raw.push_str(&format!("Content-Type: multipart/mixed; boundary=\"b{index}\"\r\n"));
    }
    raw.push_str("\r\n--b0--\r\n");
    broken.raw = raw.into_bytes();
    let folders = one_inbox(10, vec![broken, TestMessage::synthetic(synthetic_message(2))]);
    let server = RunningServer::start(folders);
    let outcome = sync(&store, &server).expect("a malformed message does not stop the run");
    server.finish();
    assert_eq!(outcome.fetched, 1);
    assert!(outcome.warnings.iter().any(|warning| warning.contains("could not be parsed")), "{:?}", outcome.warnings);
    assert_consistent(&store);
}

/// `fetch_body_response` is re-exported by the testkit so a scripted server can answer a body fetch;
/// keep one direct use here so a refactor of that builder is visible in this suite too.
#[test]
fn the_testkit_body_builder_keeps_its_wire_shape() {
    let response = fetch_body_response(1, 7, b"abc");
    assert!(response.starts_with(b"* 1 FETCH (UID 7 BODY[] {3}\r\nabc"));
    assert!(response.ends_with(b")\r\n"));
}
