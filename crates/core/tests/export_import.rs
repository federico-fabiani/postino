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
