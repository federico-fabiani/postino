//! mbox folder import and export through the operation registry.

mod support;

use base64::Engine as _;
use chrono::{TimeZone, Utc};
use postino_core::Engine;
use postino_store::{Address, Folder, FolderKind, Message, MessageSummary};
use postino_sync::mime::{split_mbox, write_mbox};
use serde_json::json;

use support::{ACCOUNT_ID, Harness, MemoryVault, seed_account};

fn date(day: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, 10, 0, 0).single().expect("a valid date")
}

fn seed(harness: &Harness) -> Engine {
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    Engine::open(store).expect("engine")
}

fn row(id: &str, folder: &str, from: Address) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: ACCOUNT_ID.to_owned(),
            folder_id: folder.to_owned(),
            thread_id: format!("thread_{id}"),
            from,
            to: vec![Address::new("Me", "me@example.com")],
            subject: format!("subject {id}"),
            snippet: "body".to_owned(),
            date: 1_700_000_100_000,
            unread: false,
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
fn an_mbox_imports_every_message_and_exports_as_one_file() {
    let harness = Harness::new();
    let mut engine = seed(&harness);
    let first = postino_testkit::synthetic_message(1);
    let second = postino_testkit::synthetic_message(2);
    let raw = write_mbox(&[(first.raw.clone(), date(26)), (second.raw.clone(), date(27))]);

    let imported = engine
        .call("folder_import_mbox", json!({ "account": ACCOUNT_ID, "mbox_base64": base64::engine::general_purpose::STANDARD.encode(&raw) }))
        .expect("folder_import_mbox");
    assert_eq!(imported["folder"]["id"], format!("fld_import_{ACCOUNT_ID}"));
    assert_eq!(imported["imported"], 2);
    assert_eq!(imported["skipped"], 0);
    assert_eq!(engine.messages().len(), 2);

    let exported = engine
        .call("folder_export_mbox", json!({ "folder": format!("fld_import_{ACCOUNT_ID}"), "account": ACCOUNT_ID }))
        .expect("folder_export_mbox");
    assert_eq!(exported["messages"], 2);
    assert_eq!(exported["exact_messages"], 0);
    assert_eq!(exported["reconstructed_messages"], 2);
    assert!(exported["filename"].as_str().unwrap_or_default().ends_with(".mbox"));
    let bytes = base64::engine::general_purpose::STANDARD.decode(exported["mbox_base64"].as_str().expect("base64")).expect("decode");
    let messages = split_mbox(&bytes, postino_sync::mime::MboxLimits::default()).expect("split export");
    assert_eq!(messages.len(), 2);
    for (part, expected) in messages.iter().zip([1, 2]) {
        let parsed = postino_sync::parse(part, &postino_sync::Limits::default()).expect("parse exported message");
        assert_eq!(parsed.subject, format!("Synthetic message {expected}"));
    }
}

#[test]
fn thunderbird_quoting_and_a_named_folder_round_trip() {
    let harness = Harness::new();
    let mut engine = seed(&harness);
    let raw = b"From - Mon Oct 26 10:00:00 2026\nFrom: Ada <ada@example.com>\nTo: Bruno <bruno@example.com>\nSubject: Thunderbird\n\n>From the archive\nFrom the desk of Ada\n";
    let imported = engine
        .call(
            "folder_import_mbox",
            json!({
                "account": ACCOUNT_ID,
                "folder": "Thunderbird Inbox",
                "mbox_base64": base64::engine::general_purpose::STANDARD.encode(raw)
            }),
        )
        .expect("folder_import_mbox");
    assert_eq!(imported["folder"]["name"], "Thunderbird Inbox");
    let folder = imported["folder"]["id"].as_str().expect("folder id");
    assert!(folder.starts_with("fld_mbox_"), "{folder}");
    assert_eq!(imported["imported"], 1);

    let messages = engine.call("messages_list", json!({ "folder": folder, "account": ACCOUNT_ID })).expect("messages_list");
    let id = messages["messages"][0]["id"].as_str().expect("message id");
    let message = engine.message(id).expect("message_get");
    let body = message.body_text.as_str();
    assert!(body.contains("From the archive"), "{body}");
    assert!(body.contains("From the desk of Ada"), "{body}");

    let exported = engine.call("folder_export_mbox", json!({ "folder": "Thunderbird Inbox", "account": ACCOUNT_ID })).expect("folder_export_mbox");
    let bytes = base64::engine::general_purpose::STANDARD.decode(exported["mbox_base64"].as_str().expect("base64")).expect("decode");
    let round_trip = split_mbox(&bytes, postino_sync::mime::MboxLimits::default()).expect("split export");
    assert_eq!(round_trip.len(), 1);
    let parsed = postino_sync::parse(&round_trip[0], &postino_sync::Limits::default()).expect("parse");
    assert_eq!(parsed.text.as_deref().map(|text| text.replace("\r\n", "\n")), Some("From the archive\nFrom the desk of Ada\n".to_owned()));
}

#[test]
fn an_unexportable_message_is_skipped_and_counted_instead_of_aborting_the_folder() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let inbox = format!("fld_{ACCOUNT_ID}_inbox");
    store.upsert_message(&row("msg_ok", &inbox, Address::new("Ada", "ada@example.com"))).expect("valid row");
    store.upsert_message(&row("msg_no_from", &inbox, Address::new("", ""))).expect("empty From row");
    let mut engine = Engine::open(store).expect("engine");
    let exported = engine.call("folder_export_mbox", json!({ "folder": inbox, "account": ACCOUNT_ID })).expect("export the rest of the folder");
    assert_eq!(exported["messages"], 1);
    assert_eq!(exported["skipped"], 1);
    assert_eq!(exported["warnings"].as_array().map(Vec::len), Some(1));
    let bytes = base64::engine::general_purpose::STANDARD.decode(exported["mbox_base64"].as_str().expect("base64")).expect("decode");
    let messages = split_mbox(&bytes, postino_sync::mime::MboxLimits::default()).expect("split");
    assert_eq!(messages.len(), 1);
}

#[test]
fn a_folder_over_the_message_cap_is_refused_before_writing() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let inbox = format!("fld_{ACCOUNT_ID}_inbox");
    for index in 0..=postino_sync::mime::MAX_MBOX_MESSAGES {
        store.upsert_message(&row(&format!("msg_{index}"), &inbox, Address::new("Ada", "ada@example.com"))).expect("row");
    }
    let mut engine = Engine::open(store).expect("engine");
    let error = engine.call("folder_export_mbox", json!({ "folder": inbox, "account": ACCOUNT_ID })).expect_err("over the importer message cap");
    assert!(error.to_string().contains("an mbox can carry at most 2000"), "{error}");
}

#[test]
fn a_synced_folder_named_import_is_not_hijacked_by_the_importer() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    store
        .upsert_folder(&Folder {
            id: "fld_server_import".to_owned(),
            account_id: ACCOUNT_ID.to_owned(),
            name: "Import".to_owned(),
            kind: FolderKind::Custom,
            unread: 0,
            total: 0,
            uid_validity: Some(1),
            uid_next: Some(1),
            last_sync_at: None,
        })
        .expect("server folder");
    let mut engine = Engine::open(store).expect("engine");
    let raw = write_mbox(&[(postino_testkit::synthetic_message(1).raw.clone(), date(26))]);
    let error = engine
        .call(
            "folder_import_mbox",
            json!({ "account": ACCOUNT_ID, "folder": "Import", "mbox_base64": base64::engine::general_purpose::STANDARD.encode(&raw) }),
        )
        .expect_err("the server folder must win");
    assert!(error.to_string().contains("server folder named"), "{error}");
    assert!(engine.folders().iter().all(|folder| folder.id != format!("fld_import_{ACCOUNT_ID}")));
}

#[test]
fn an_invalid_mbox_creates_no_folder() {
    let harness = Harness::new();
    let mut engine = seed(&harness);
    let error = engine
        .call(
            "folder_import_mbox",
            json!({ "account": ACCOUNT_ID, "mbox_base64": base64::engine::general_purpose::STANDARD.encode(b"From: ada@example.com\n\nnot an mbox\n") }),
        )
        .expect_err("invalid mbox");
    assert!(error.to_string().contains("not a usable mbox"), "{error}");
    assert!(engine.folders().iter().all(|folder| folder.id != format!("fld_import_{ACCOUNT_ID}")));
}
