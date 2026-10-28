//! `.eml` import and export through the operation registry.

mod support;

use base64::Engine as _;
use postino_core::{Engine, FolderKind as CoreFolderKind};
use postino_store::FolderKind;
use serde_json::json;

use support::{ACCOUNT_ID, Harness, MemoryVault, seed_account};

#[test]
fn an_imported_message_round_trips_its_body_and_attachment_through_export() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let mut engine = Engine::open(store).expect("engine");

    let source = postino_testkit::synthetic_message_with_attachment(1, "nota.txt", "text/plain", b"allegato importato\n");
    let imported = engine
        .call("message_import", json!({ "account": ACCOUNT_ID, "eml_base64": base64::engine::general_purpose::STANDARD.encode(&source.raw) }))
        .expect("message_import");
    let message = &imported["message"];
    assert_eq!(message["summary"]["subject"], "Synthetic message with attachment 1");
    assert_eq!(message["summary"]["folder_id"], format!("fld_import_{ACCOUNT_ID}"));
    assert_eq!(message["attachments"][0]["filename"], "nota.txt");
    assert_eq!(imported["warnings"], json!([]));

    let exported = engine.call("message_export", json!({ "id": message["summary"]["id"], "account": ACCOUNT_ID })).expect("message_export");
    assert_eq!(exported["exact"], false, "an ordinary row has no retained v9 raw bytes");
    assert!(
        exported["filename"].as_str().unwrap_or_default().contains("synthetic-message-with-attachment"),
        "the filename should describe the mail: {}",
        exported["filename"]
    );
    let raw = base64::engine::general_purpose::STANDARD.decode(exported["eml_base64"].as_str().expect("base64")).expect("decode export");
    let parsed = postino_sync::parse(&raw, &postino_sync::Limits::default()).expect("parse the exported .eml");
    assert!(parsed.text.as_deref().unwrap_or("").contains("Synthetic message number 1"));
    let attachment = parsed.attachments.iter().find(|part| part.filename == "nota.txt").expect("attachment part");
    assert_eq!(attachment.data, b"allegato importato\n");
}

#[test]
fn retained_v9_raw_bytes_are_exported_byte_for_byte() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let mut engine = Engine::open(store).expect("engine");
    let source = postino_testkit::synthetic_message(1);
    let imported = engine
        .call("message_import", json!({ "account": ACCOUNT_ID, "eml_base64": base64::engine::general_purpose::STANDARD.encode(&source.raw) }))
        .expect("message_import");
    let id = imported["message"]["summary"]["id"].as_str().expect("id");

    let store = harness.open();
    let hash = store.blobs().put(&source.raw).expect("raw blob");
    store.set_message_raw_blob_hash(id, Some(&hash)).expect("retain raw");

    let exported = engine.call("message_export", json!({ "id": id, "account": ACCOUNT_ID })).expect("message_export");
    assert_eq!(exported["exact"], true);
    let raw = base64::engine::general_purpose::STANDARD.decode(exported["eml_base64"].as_str().expect("base64")).expect("decode export");
    assert_eq!(raw, source.raw, "retained bytes must not be reconstructed");
}

#[test]
fn a_server_folder_named_import_is_never_used_as_the_local_import_target() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox), ("import", "Import", FolderKind::Custom)]);
    let mut server_import = store.get_folder(&format!("fld_{ACCOUNT_ID}_import")).expect("read folder").expect("seeded folder");
    server_import.uid_validity = Some(7);
    store.upsert_folder(&server_import).expect("mark as a synced folder");
    let mut engine = Engine::open(store).expect("engine");

    let source = postino_testkit::synthetic_message(1);
    let imported = engine
        .call("message_import", json!({ "account": ACCOUNT_ID, "eml_base64": base64::engine::general_purpose::STANDARD.encode(&source.raw) }))
        .expect("message_import");
    assert_eq!(imported["message"]["summary"]["folder_id"], format!("fld_import_{ACCOUNT_ID}"));
    let store = harness.open();
    let server_import = store.get_folder(&format!("fld_{ACCOUNT_ID}_import")).expect("read folder").expect("server folder");
    assert_eq!(server_import.total, 0, "the import wrote a synthetic UID into a synced folder");
}

#[test]
fn export_keeps_the_threading_headers_of_a_reply() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let mut engine = Engine::open(store).expect("engine");
    let raw = b"From: Ada <ada@example.com>\r\nTo: Bruno <bruno@example.com>\r\nDate: Tue, 27 Oct 2026 10:00:00 +0000\r\nMessage-ID: <reply@example.com>\r\nIn-Reply-To: <parent@example.com>\r\nReferences: <root@example.com> <parent@example.com>\r\nSubject: Re: preventivo\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nGrazie.\r\n";
    let imported = engine
        .call("message_import", json!({ "account": ACCOUNT_ID, "eml_base64": base64::engine::general_purpose::STANDARD.encode(raw) }))
        .expect("message_import");
    let id = imported["message"]["summary"]["id"].as_str().expect("id");
    let exported = engine.call("message_export", json!({ "id": id, "account": ACCOUNT_ID })).expect("message_export");
    let raw = base64::engine::general_purpose::STANDARD.decode(exported["eml_base64"].as_str().expect("base64")).expect("decode");
    let parsed = postino_sync::parse(&raw, &postino_sync::Limits::default()).expect("parse export");
    assert_eq!(parsed.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(parsed.references, vec!["root@example.com".to_owned(), "parent@example.com".to_owned()]);
}

#[test]
fn export_refuses_a_message_from_another_account() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    seed_account(&store, &vault, "acc_other", 2, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let mut engine = Engine::open(store).expect("engine");
    let source = postino_testkit::synthetic_message(1);
    let imported = engine
        .call("message_import", json!({ "account": ACCOUNT_ID, "eml_base64": base64::engine::general_purpose::STANDARD.encode(&source.raw) }))
        .expect("message_import");
    let id = imported["message"]["summary"]["id"].as_str().expect("id");
    let error = engine.call("message_export", json!({ "id": id, "account": "acc_other" })).expect_err("cross-account export");
    assert!(error.to_string().contains("another account"), "{error}");
}

#[test]
fn an_eml_payload_above_the_transport_budget_is_refused_before_decoding() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let mut engine = Engine::open(store).expect("engine");
    let encoded = "A".repeat((postino_core::MAX_EML_BYTES.div_ceil(3) * 4) as usize + 4);
    let error = engine.call("message_import", json!({ "account": ACCOUNT_ID, "eml_base64": encoded })).expect_err("oversized payload");
    assert!(error.to_string().contains("too large"), "{error}");
}

#[test]
fn an_empty_file_is_refused_before_the_store_is_touched() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox)]);
    let mut engine = Engine::open(store).expect("engine");
    let error = engine.call("message_import", json!({ "account": ACCOUNT_ID, "eml_base64": "" })).expect_err("empty import");
    assert!(error.to_string().contains("empty"), "{error}");
    assert_eq!(engine.folders().iter().filter(|folder| folder.kind == CoreFolderKind::Custom).count(), 0, "a refused import created a folder");
}
