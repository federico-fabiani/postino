//! P6a: the account lifecycle on a real (fake) IMAP server.
//!
//! Every test uses a throwaway SQLCipher store, an in-memory credential vault and the shared
//! `postino-testkit` fake server with a throwaway CA. No test touches the real keyring, the real
//! `%LOCALAPPDATA%\Postino` or a real mail server.

mod support;

use std::sync::{Arc, Mutex};

use postino_core::{AccountUpdateRequest, CredentialVault, Engine, LabelColor, MailTls, OpError, Provider, SmtpSetup, SyncPhase};
use postino_store::{AccountServer, Address, FolderKind, Message, MessageSummary};
use postino_testkit::{
    FailPoint, MailboxScript, TestFolder, TestMessage, USER, expected_smtp_sasl, synthetic_message, synthetic_message_with_attachment,
};
use serde_json::json;
use support::{
    ACCOUNT_ID, Harness, MemoryVault, RunningServer, RunningSmtp, account_setup, refuse_list_with_secret, reject_login, seed_account,
    seed_account_with_smtp, start_handler,
};
use zeroize::Zeroizing;

const ATTACHMENT_BYTES: &[u8] = b"postino p6a attachment";

fn inbound(uid: u32) -> TestMessage {
    TestMessage::synthetic(synthetic_message(uid))
}

fn mailbox() -> Vec<TestFolder> {
    vec![
        TestFolder::new("INBOX", 10).with_special_use("\\Inbox").with_messages(vec![
            inbound(1),
            TestMessage::synthetic(synthetic_message_with_attachment(2, "report.pdf", "application/pdf", ATTACHMENT_BYTES)),
            inbound(3),
        ]),
        TestFolder::new("Sent", 20).with_special_use("\\Sent").with_messages(vec![inbound(4)]),
        TestFolder::new("Trash", 30).with_special_use("\\Trash"),
    ]
}

fn three_folders() -> [(&'static str, &'static str, FolderKind); 3] {
    [("inbox", "INBOX", FolderKind::Inbox), ("sent", "Sent", FolderKind::Sent), ("trash", "Trash", FolderKind::Trash)]
}

fn stored_message(id: &str, account_id: &str, folder_id: &str) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: account_id.to_owned(),
            folder_id: folder_id.to_owned(),
            thread_id: format!("thread_{id}"),
            from: Address::new("Giulia", "giulia@example.com"),
            to: vec![Address::new("Me", "me@example.com")],
            subject: format!("subject {id}"),
            snippet: "body".to_owned(),
            date: 1_700_000_100_000,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "body".to_owned(),
        body_html: None,
        remote_images: 0,
        size: Some(4),
    }
}

#[test]
fn account_add_writes_account_server_folders_and_credential_only_after_the_login() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let server = RunningServer::start(mailbox(), 1);
    let port = server.port();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");

    let view = engine.add_account(account_setup(&server)).expect("add the account");
    server.finish();

    assert_eq!(view.account.email, USER);
    assert_eq!(view.account.provider, "custom");
    assert!(view.account.id.starts_with("acc_"));
    let settings = view.server.as_ref().expect("server settings");
    assert_eq!(settings.host, "localhost");
    assert_eq!(settings.port, port);
    assert_eq!(settings.tls, MailTls::Implicit);
    assert_eq!(settings.username, USER);
    assert!(view.sync.is_none());

    let store = harness.open();
    assert_eq!(store.list_accounts().expect("accounts").len(), 1);
    assert_eq!(store.get_account_server(&view.account.id).expect("server").expect("row").port, port);
    let folders = store.list_folders(Some(&view.account.id)).expect("folders");
    assert_eq!(folders.len(), 3);
    assert!(folders.iter().any(|folder| folder.kind == FolderKind::Inbox));
    assert!(folders.iter().any(|folder| folder.kind == FolderKind::Sent));
    assert!(folders.iter().any(|folder| folder.kind == FolderKind::Trash));
    assert!(vault.contains(&view.account.id));
    assert_eq!(vault.password(&view.account.id).as_deref(), Some(postino_testkit::PASSWORD.as_bytes()));
    assert_eq!(engine.folders().len(), 3, "the engine did not refresh its folder tree");

    let json = serde_json::to_string(&view).expect("view JSON");
    assert!(!json.contains(postino_testkit::PASSWORD), "{json}");
    assert!(!json.contains("password"), "{json}");
}

#[test]
fn a_wrong_password_writes_nothing_and_never_echoes_the_secret() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let server = start_handler(reject_login);
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let mut setup = account_setup(&server);
    setup.password = Zeroizing::new("definitely-wrong-app-password".to_owned());

    let error = engine.add_account(setup).expect_err("the login must fail");
    server.finish();

    assert!(matches!(error, OpError::Auth(_)), "{error}");
    let text = error.to_string();
    assert!(text.contains("rejected the credential"), "{text}");
    assert!(text.contains("custom"), "the error does not name the provider: {text}");
    assert!(!text.contains("definitely-wrong-app-password"), "{text}");
    assert!(!text.contains(postino_testkit::PASSWORD), "{text}");
    assert!(!text.contains("invalid credentials"), "the auth error echoed server text: {text}");

    let store = harness.open();
    assert!(store.list_accounts().expect("accounts").is_empty(), "a failed login wrote an account");
    assert!(store.list_folders(None).expect("folders").is_empty(), "a failed login wrote folders");
    assert_eq!(vault.len(), 0, "a failed login stored a credential");
    assert!(engine.accounts().is_empty());
}

#[test]
fn an_unreachable_host_writes_nothing() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
    let port = probe.local_addr().expect("address").port();
    drop(probe);
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let setup = postino_core::AccountSetup {
        provider: Provider::Custom,
        email: "user@example.com".to_owned(),
        name: None,
        password: Zeroizing::new("not-used".to_owned()),
        host: Some("127.0.0.1".to_owned()),
        port: Some(port),
        tls: Some(MailTls::Implicit),
        smtp: None,
        extra_roots: Vec::new(),
    };

    let error = engine.add_account(setup).expect_err("the connection must fail");
    assert!(matches!(error, OpError::Network(_)), "{error}");
    assert!(harness.open().list_accounts().expect("accounts").is_empty());
    assert_eq!(vault.len(), 0);
}

#[test]
fn a_duplicate_email_is_rejected_before_the_network() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let server = RunningServer::start(mailbox(), 1);
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let first = engine.add_account(account_setup(&server)).expect("first add");
    let mut duplicate = account_setup(&server);
    server.finish();

    duplicate.email = USER.to_uppercase();
    duplicate.name = Some("Second copy".to_owned());
    let error = engine.add_account(duplicate).expect_err("a duplicate email must be refused");
    let text = error.to_string();
    assert!(text.contains("already exists"), "{text}");
    assert!(text.contains(&first.account.id), "{text}");
    assert_eq!(engine.accounts().len(), 1);
    assert_eq!(vault.len(), 1);
}

#[test]
fn account_test_reports_folders_capabilities_and_survives_a_broken_credential() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let server = RunningServer::start(mailbox(), 1);
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, server.port(), &three_folders());
    let folder_id = format!("fld_{ACCOUNT_ID}_inbox");
    store.upsert_message(&stored_message("msg_cached", ACCOUNT_ID, &folder_id)).expect("cached message");
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![server.ca()]);

    let result = engine.test_account(ACCOUNT_ID).expect("the stored password works");
    server.finish();
    assert_eq!(result.account, ACCOUNT_ID);
    assert_eq!(result.folders, 3);
    assert!(result.capabilities.iter().any(|capability| capability == "IMAP4REV1"), "{:?}", result.capabilities);

    // A second server rejects every login. The account and its cache must be exactly as they were.
    let rejecting = start_handler(reject_login);
    let store = harness.open();
    store
        .upsert_account_server(&AccountServer {
            account_id: ACCOUNT_ID.to_owned(),
            host: "localhost".to_owned(),
            port: rejecting.port(),
            tls: postino_store::AccountTls::Implicit,
            username: USER.to_owned(),
            created_at: 1_700_000_000_000,
            smtp: None,
        })
        .expect("point at the rejecting server");
    vault.set(ACCOUNT_ID, b"wrong-password");
    engine.set_test_extra_roots(vec![rejecting.ca()]);
    let error = engine.test_account(ACCOUNT_ID).expect_err("the broken credential must fail");
    rejecting.finish();
    assert!(matches!(error, OpError::Auth(_)), "{error}");
    assert!(!error.to_string().contains("wrong-password"), "{error}");
    assert_eq!(engine.accounts().len(), 1);
    assert!(engine.message("msg_cached").is_some(), "the cached mail was touched");
    assert!(store.get_account(ACCOUNT_ID).expect("account").is_some());
    assert!(store.get_message("msg_cached").expect("message").is_some());
}

#[test]
fn sync_now_caches_three_folders_refreshes_memory_and_fetches_only_new_mail() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mailbox = Arc::new(Mutex::new(mailbox()));
    let server = RunningServer::start_mutable(mailbox.clone(), 2, |_| MailboxScript::default());
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, server.port(), &three_folders());
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![server.ca()]);

    let report = engine.sync_now(None).expect("first sync");
    assert_eq!(report.account, ACCOUNT_ID);
    assert_eq!(report.folders, 3);
    assert_eq!(report.fetched, 4);
    assert_eq!(report.attachments_stored, 1);
    assert_eq!(engine.messages().len(), 4, "memory was not refreshed from the store");
    let statuses = engine.sync_statuses(None).expect("status");
    let status = statuses.first().and_then(|view| view.status.as_ref()).expect("status row");
    assert_eq!(status.state, SyncPhase::Idle);
    assert_eq!(status.folders_total, 3);
    assert!(status.last_success.is_some());

    let attachment = engine.messages().iter().find_map(|message| message.attachments.first()).expect("attachment");
    assert_eq!(engine.attachment_bytes(&attachment.id).expect("stored bytes").as_slice(), ATTACHMENT_BYTES);

    // A local label must survive the next run's memory refresh.
    let label = engine.create_label("Cliente", LabelColor::Gold).expect("label");
    let labelled = engine.messages().first().expect("a message").summary.id.clone();
    engine.set_label(&label.id, std::slice::from_ref(&labelled), true).expect("label applied");

    {
        let mut mailbox = mailbox.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let inbox = mailbox.iter_mut().find(|folder| folder.wire_name == "INBOX").expect("inbox");
        inbox.messages.push(inbound(5));
    }
    let report = engine.sync_now(Some(ACCOUNT_ID)).expect("incremental sync");
    server.finish();
    assert_eq!(report.fetched, 1, "the second run re-fetched cached mail");
    assert_eq!(engine.messages().len(), 5);
    let refreshed = engine.message(&labelled).expect("the labelled message");
    assert!(refreshed.summary.labels.contains(&label.id), "the sync detached a local label");
    assert_eq!(harness.open().labels_for(&labelled).expect("stored labels"), vec![label.id.clone()]);
}

#[test]
fn a_failed_sync_keeps_the_previous_cache_and_records_the_error() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mailbox = Arc::new(Mutex::new(mailbox()));
    let server = RunningServer::start_mutable(mailbox.clone(), 2, |index| {
        if index == 1 { MailboxScript::default().fail(FailPoint::OnSelect(1)) } else { MailboxScript::default() }
    });
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, server.port(), &three_folders());
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![server.ca()]);

    engine.sync_now(None).expect("first sync");
    assert_eq!(engine.messages().len(), 4);
    let error = engine.sync_now(None).expect_err("the second connection drops before any folder");
    server.finish();
    assert!(matches!(error, OpError::Network(_)), "{error}");
    assert_eq!(engine.messages().len(), 4, "a failed run lost the cache in memory");
    let store = harness.open();
    assert_eq!(store.list_all_messages().expect("messages").len(), 4, "a failed run lost the cache on disk");
    let statuses = engine.sync_statuses(Some(ACCOUNT_ID)).expect("status");
    let status = statuses.first().and_then(|view| view.status.as_ref()).expect("status row");
    assert_eq!(status.state, SyncPhase::Error);
    assert!(status.last_success.is_some(), "the previous success was forgotten");
    assert!(status.error.as_deref().is_some_and(|error| !error.is_empty()));
}

#[test]
fn account_remove_deletes_its_credential_and_cache_and_leaves_the_other_account() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    let folders = [("inbox", "INBOX", FolderKind::Inbox)];
    seed_account(&store, &vault, "acc_a", 993, &folders);
    seed_account(&store, &vault, "acc_b", 993, &folders);
    store.upsert_message(&stored_message("msg_a", "acc_a", "fld_acc_a_inbox")).expect("message a");
    store.upsert_message(&stored_message("msg_b", "acc_b", "fld_acc_b_inbox")).expect("message b");
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let label = engine.create_label("Cliente", LabelColor::Forest).expect("label");
    engine.set_label(&label.id, &["msg_a".to_owned(), "msg_b".to_owned()], true).expect("label both");

    let error = engine.remove_account("acc_a", false).expect_err("an unconfirmed remove must be refused");
    assert!(matches!(error, OpError::InvalidArgs(_)), "{error}");
    assert!(engine.accounts().iter().any(|account| account.id == "acc_a"));

    let removed = engine.remove_account("acc_a", true).expect("remove");
    assert_eq!(removed.account.id, "acc_a");
    assert_eq!(engine.accounts().len(), 1);
    assert_eq!(engine.accounts()[0].id, "acc_b");
    assert!(engine.message("msg_a").is_none());
    assert!(engine.message("msg_b").is_some(), "the other account's mail was removed");
    assert!(engine.label(&label.id).is_some(), "a mailbox-wide label was removed");
    assert!(engine.message("msg_b").is_some_and(|message| message.summary.labels.contains(&label.id)), "the other label was lost");

    assert!(!vault.contains("acc_a"), "the credential survived");
    assert!(vault.contains("acc_b"));
    let store = harness.open();
    assert!(store.get_account("acc_a").expect("account").is_none());
    assert_eq!(store.get_account_server("acc_a").expect("server"), None);
    assert!(store.list_folders(Some("acc_a")).expect("folders").is_empty());
    assert!(store.get_message("msg_a").expect("message").is_none());
    assert!(store.get_message("msg_b").expect("message").is_some());
    assert_eq!(store.labels_for("msg_b").expect("labels"), vec![label.id]);
    assert_eq!(store.list_labels(None).expect("labels").len(), 1);
}

#[test]
fn sync_now_requires_an_id_when_several_accounts_exist_and_status_lists_them_all() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, "acc_a", 993, &[("inbox", "INBOX", FolderKind::Inbox)]);
    seed_account(&store, &vault, "acc_b", 993, &[("inbox", "INBOX", FolderKind::Inbox)]);
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");

    let error = engine.sync_now(None).expect_err("two accounts need an explicit id");
    let text = error.to_string();
    assert!(matches!(error, OpError::InvalidArgs(_)), "{text}");
    assert!(text.contains("acc_a") && text.contains("acc_b"), "{text}");
    let statuses = engine.sync_statuses(None).expect("status lists every account");
    assert_eq!(statuses.len(), 2);
    assert!(statuses.iter().all(|view| view.status.is_none()));
}

#[test]
fn a_server_that_echoes_the_password_in_a_list_response_leaks_nothing() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let server = start_handler(|conn| async move {
        refuse_list_with_secret(conn, postino_testkit::PASSWORD.to_owned()).await;
    });
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");

    let error = engine.add_account(account_setup(&server)).expect_err("the LIST refusal must fail the add");
    server.finish();
    let text = error.to_string();
    assert!(matches!(error, OpError::Network(_)), "{error}");
    assert!(text.contains("listing folders"), "the phase is not named: {text}");
    assert!(text.contains("custom"), "the provider is not named: {text}");
    assert!(!text.contains(postino_testkit::PASSWORD), "the server text leaked into the error: {text}");
    assert!(!text.contains("A0003 NO"), "the raw response leaked into the error: {text}");
    assert!(engine.accounts().is_empty());
    assert_eq!(vault.len(), 0);
}

#[test]
fn a_damaged_sync_status_is_a_warning_and_does_not_block_listing_or_removal() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 993, &[("inbox", "INBOX", FolderKind::Inbox)]);
    store.set_meta(&format!("sync_status:{ACCOUNT_ID}"), "{not-json").expect("damaged status");
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");

    let (views, warnings) = engine.account_views_report().expect("a damaged status must not fail the listing");
    assert_eq!(views.len(), 1);
    assert!(views.first().is_some_and(|view| view.sync.is_none()));
    assert!(warnings.iter().any(|warning| warning.contains(ACCOUNT_ID)), "{warnings:?}");
    let listed = engine.call("accounts_list", json!({})).expect("accounts_list");
    assert!(listed["accounts"][0]["sync"].is_null());
    assert!(!listed["warnings"].as_array().expect("warnings").is_empty());

    let removed = engine.remove_account(ACCOUNT_ID, true).expect("a damaged status must not block removal");
    assert_eq!(removed.account.id, ACCOUNT_ID);
    assert!(engine.accounts().is_empty());
    assert!(!vault.contains(ACCOUNT_ID));
}

#[test]
fn a_failed_account_transaction_removes_the_just_stored_credential_and_writes_no_rows() {
    let harness = Harness::new();
    {
        let store = harness.open();
        // `Engine::open` repairs the label sequence; seed the value so the read-only open below does
        // not need a write.
        store.set_meta(postino_store::LABEL_SEQ_KEY, "0").expect("label sequence");
    }
    let vault = MemoryVault::new();
    let server = RunningServer::start(mailbox(), 1);
    let mut engine = Engine::open_with_vault(harness.read_only(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![server.ca()]);

    let error = engine.add_account(account_setup(&server)).expect_err("the read-only store must refuse the account transaction");
    server.finish();
    let text = error.to_string();
    assert!(text.contains("the local store refused the account"), "{text}");
    assert!(text.contains("credentials were removed again"), "the cleanup did not run: {text}");
    assert_eq!(vault.len(), 0, "the credential survived a failed account transaction");
    let deletions = vault.deletions();
    assert_eq!(deletions.len(), 1, "the credential deletion was not recorded");

    let store = harness.open();
    assert!(store.list_accounts().expect("accounts").is_empty());
    assert!(store.list_folders(None).expect("folders").is_empty());
    assert!(store.get_account_server(&deletions[0]).expect("server").is_none());
}

#[test]
fn a_failed_credential_delete_leaves_no_cached_mail_without_a_password() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 993, &[("inbox", "INBOX", FolderKind::Inbox)]);
    store.upsert_message(&stored_message("msg_1", ACCOUNT_ID, &format!("fld_{ACCOUNT_ID}_inbox"))).expect("message");
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    vault.fail_deletes();

    let error = engine.remove_account(ACCOUNT_ID, true).expect_err("the keyring refused the delete");
    let text = error.to_string();
    assert!(text.contains("cached mail were removed"), "{text}");
    assert!(text.contains("keyring refused"), "{text}");

    // The documented order: the mail cache went first, so the failure window is an orphan credential,
    // never cached mail whose password is gone.
    assert!(engine.accounts().is_empty());
    assert!(engine.message("msg_1").is_none());
    assert!(harness.open().get_account(ACCOUNT_ID).expect("account").is_none());
    assert!(vault.contains(ACCOUNT_ID), "the failed delete should leave the credential for a retry");
}

#[test]
fn the_demo_engine_answers_account_and_sync_operations_with_a_readable_error() {
    let mut engine = Engine::demo();
    let error = engine.call("sync_now", json!({})).expect_err("the demo has no store");
    assert!(error.to_string().contains("demo"), "{error}");
    let error = engine.call("sync_status", json!({})).expect_err("the demo has no store");
    assert!(error.to_string().contains("demo"), "{error}");
    let error = engine.call("account_test", json!({ "id": "acc_demo" })).expect_err("the demo has no store");
    assert!(error.to_string().contains("demo"), "{error}");
    let error = engine.call("account_remove", json!({ "id": "acc_demo", "confirm": true })).expect_err("the demo has no store");
    assert!(error.to_string().contains("demo"), "{error}");

    let setup = postino_core::AccountSetup {
        provider: Provider::Custom,
        email: "me@example.com".to_owned(),
        name: None,
        password: Zeroizing::new("not-used".to_owned()),
        host: Some("localhost".to_owned()),
        port: Some(1),
        tls: Some(MailTls::Implicit),
        smtp: None,
        extra_roots: Vec::new(),
    };
    let error = engine.add_account(setup).expect_err("the demo has no store");
    assert!(error.to_string().contains("demo"), "{error}");

    let accounts = engine.call("accounts_list", json!({})).expect("the demo lists its synthetic account");
    assert_eq!(accounts["accounts"].as_array().and_then(|rows| rows.first()).and_then(|row| row["server"].as_str()), None);
}

#[test]
fn account_add_validates_input_before_anything_is_written() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let base = || postino_core::AccountSetup {
        provider: Provider::Custom,
        email: "user@example.com".to_owned(),
        name: None,
        password: Zeroizing::new("password".to_owned()),
        host: Some("localhost".to_owned()),
        port: Some(993),
        tls: Some(MailTls::Implicit),
        smtp: None,
        extra_roots: Vec::new(),
    };

    let mut setup = base();
    setup.email = "not-an-address".to_owned();
    assert!(matches!(engine.add_account(setup), Err(OpError::InvalidArgs(_))));

    let mut setup = base();
    setup.email = "a@b@c".to_owned();
    assert!(matches!(engine.add_account(setup), Err(OpError::InvalidArgs(_))), "two @ must be refused");

    let mut setup = base();
    setup.password = Zeroizing::new(String::new());
    let error = engine.add_account(setup).expect_err("an empty password is refused");
    assert!(error.to_string().contains("password must not be empty"), "{error}");

    let mut setup = base();
    setup.password = Zeroizing::new("line\nbreak".to_owned());
    assert!(matches!(engine.add_account(setup), Err(OpError::InvalidArgs(_))));

    let mut setup = base();
    setup.host = None;
    let error = engine.add_account(setup).expect_err("custom needs a host");
    assert!(error.to_string().contains("requires host, port and tls"), "{error}");

    let mut setup = base();
    setup.provider = Provider::Aruba;
    setup.host = Some("imap.example.com".to_owned());
    setup.port = Some(993);
    setup.tls = Some(MailTls::Implicit);
    let error = engine.add_account(setup).expect_err("a known provider owns its endpoint");
    assert!(error.to_string().contains("imaps.aruba.it"), "{error}");

    assert!(engine.accounts().is_empty());
    assert_eq!(vault.len(), 0);
}

#[test]
fn account_add_probes_stores_and_removes_the_smtp_credential() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    let imap = RunningServer::start(mailbox(), 1);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH PLAIN LOGIN", "SIZE 1024000"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect("QUIT").await;
        conn.send_line("221 bye").await;
    });
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca()]);
    let mut setup = account_setup(&imap);
    setup.smtp = Some(SmtpSetup {
        host: "localhost".to_owned(),
        port: smtp.port(),
        tls: MailTls::Implicit,
        username: USER.to_owned(),
        password: Some(Zeroizing::new(postino_testkit::SMTP_PASSWORD.to_owned())),
    });
    let view = engine.add_account(setup).expect("account with SMTP");
    let server = view.server.expect("server settings");
    let endpoint = server.smtp.expect("SMTP endpoint");
    assert_eq!((endpoint.host.as_str(), endpoint.port, endpoint.tls), ("localhost", smtp.port(), MailTls::Implicit));
    assert_eq!(endpoint.username, USER);
    assert_eq!(
        vault.load_smtp_password(&view.account.id).expect("load SMTP").as_deref().map(Vec::as_slice),
        Some(postino_testkit::SMTP_PASSWORD.as_bytes())
    );
    engine.remove_account(&view.account.id, true).expect("remove");
    assert!(vault.load_smtp_password(&view.account.id).expect("load after remove").is_none());
    smtp.finish();
    imap.finish();
}

#[test]
fn account_update_adds_and_clears_the_smtp_endpoint_without_touching_mail() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)]);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&postino_testkit::expected_sasl()).await;
        conn.expect("QUIT").await;
        conn.send_line("221 bye").await;
    });
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![smtp.ca()]);
    let view = engine
        .update_account(AccountUpdateRequest {
            id: ACCOUNT_ID.to_owned(),
            smtp_host: Some("localhost".to_owned()),
            smtp_port: Some(smtp.port()),
            smtp_tls: Some(MailTls::Implicit),
            smtp_username: Some(USER.to_owned()),
            smtp_password: None,
            clear_smtp: false,
        })
        .expect("add SMTP");
    let endpoint = view.server.expect("server").smtp.expect("SMTP endpoint");
    assert_eq!(endpoint.port, smtp.port());
    assert_eq!(vault.password(ACCOUNT_ID).as_deref(), Some(postino_testkit::PASSWORD.as_bytes()), "the IMAP credential was changed");
    let cleared = engine
        .update_account(AccountUpdateRequest {
            id: ACCOUNT_ID.to_owned(),
            smtp_host: None,
            smtp_port: None,
            smtp_tls: None,
            smtp_username: None,
            smtp_password: None,
            clear_smtp: true,
        })
        .expect("clear SMTP");
    assert!(cleared.server.expect("server").smtp.is_none());
    smtp.finish();
}

#[test]
fn account_update_refuses_to_clear_and_set_smtp_at_once() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    seed_account(&harness.open(), &vault, ACCOUNT_ID, 1, &[]);
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    let base = AccountUpdateRequest {
        id: ACCOUNT_ID.to_owned(),
        smtp_host: None,
        smtp_port: None,
        smtp_tls: None,
        smtp_username: None,
        smtp_password: None,
        clear_smtp: true,
    };
    for request in [
        AccountUpdateRequest { smtp_host: Some("smtp.example.com".to_owned()), ..base.clone() },
        AccountUpdateRequest { smtp_port: Some(587), ..base.clone() },
        AccountUpdateRequest { smtp_tls: Some(MailTls::StartTls), ..base.clone() },
        AccountUpdateRequest { smtp_username: Some(USER.to_owned()), ..base.clone() },
        AccountUpdateRequest { smtp_password: Some(Zeroizing::new("secret".to_owned())), ..base.clone() },
    ] {
        let error = engine.update_account(request).expect_err("clear_smtp with endpoint fields must be refused");
        assert!(matches!(error, OpError::InvalidArgs(_)), "{error}");
        assert!(error.to_string().contains("clear_smtp"), "{error}");
    }
}

#[test]
fn account_test_probes_a_configured_smtp_endpoint_without_echoing_it() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let imap = RunningServer::start(mailbox(), 1);
    // The submission server rejects the credential and repeats the password it received.
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["SIZE 1024000", "AUTH PLAIN"]).await;
        conn.expect_prefix("AUTH PLAIN ").await;
        conn.send_line(&format!("535 5.7.8 {}", postino_testkit::SMTP_PASSWORD)).await;
    });
    let store = harness.open();
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        imap.port(),
        smtp.port(),
        &[("inbox", "INBOX", FolderKind::Inbox), ("sent", "Sent", FolderKind::Sent)],
    );
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca()]);
    let error = engine.test_account(ACCOUNT_ID).expect_err("the SMTP credential is refused");
    imap.finish();
    smtp.finish();
    assert!(matches!(error, OpError::Auth(_)), "{error}");
    assert!(error.to_string().contains("SMTP"), "{error}");
    assert!(!error.to_string().contains(postino_testkit::SMTP_PASSWORD), "the server echo leaked: {error}");
    assert!(!error.to_string().contains("535 5.7.8"), "the server text leaked: {error}");
}
