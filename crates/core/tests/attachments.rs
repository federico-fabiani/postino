//! P8 attachment bytes, caps, filename safety and reply headers.
//!
//! The store is the real SQLCipher file and the blob store next to it. No test opens a socket: a
//! queued send is enough to prove the exact RFC 5322 bytes, and the outbox row names the encrypted
//! blob the worker would submit.

mod support;

use std::sync::Arc;

use base64::Engine as _;
use postino_core::Engine;
use postino_store::{FolderKind, Store};
use serde_json::json;
use sha2::{Digest, Sha256};

use support::{ACCOUNT_ID, Harness, MemoryVault};

fn base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn seed(harness: &Harness) -> (Arc<MemoryVault>, Store) {
    let vault = MemoryVault::new();
    let store = harness.open();
    support::seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        1,
        1,
        &[("drafts", "Bozze", FolderKind::Drafts), ("sent", "Inviati", FolderKind::Sent)],
    );
    (vault, store)
}

fn new_draft(engine: &mut Engine) -> String {
    let value = engine
        .call(
            "draft_save",
            json!({
                "account": ACCOUNT_ID,
                "to": [{ "name": "Giulia", "email": "giulia@example.com" }],
                "subject": "Con allegato",
                "body_text": "Corpo"
            }),
        )
        .expect("draft_save");
    value["draft"]["summary"]["id"].as_str().expect("draft id").to_owned()
}

fn add(engine: &mut Engine, draft: &str, filename: &str, mime: Option<&str>, data: &[u8]) -> serde_json::Value {
    engine
        .call("draft_attachment_add", json!({ "draft": draft, "filename": filename, "mime": mime, "data_base64": base64(data) }))
        .expect("draft_attachment_add")
}

fn queued_raw(harness: &Harness, engine: &mut Engine, draft: &str) -> Vec<u8> {
    engine.call("message_send", json!({ "draft": draft })).expect("queue");
    let store = harness.open();
    let row = store.active_outbox_for_draft(draft).expect("outbox lookup").expect("outbox row");
    store.blobs().get(&row.raw_blob_hash, 64 * 1024 * 1024).expect("raw blob").to_vec()
}

#[test]
fn exact_bytes_survive_blob_draft_reopen_queue_and_mime() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);
    let content = (0..2 * 1024 * 1024 + 37).map(|index| (index % 251) as u8).collect::<Vec<_>>();
    let digest_before = format!("{:x}", Sha256::digest(&content));

    let added = add(&mut engine, &draft, "Relazione finale è pronta.pdf", Some("Application/PDF"), &content);
    assert_eq!(added["draft"]["attachments"][0]["filename"], "Relazione finale è pronta.pdf");
    assert_eq!(added["draft"]["attachments"][0]["mime"], "application/pdf");
    assert_eq!(added["draft"]["attachments"][0]["size"], content.len());
    let attachment_id = added["draft"]["attachments"][0]["id"].as_str().expect("attachment id").to_owned();
    assert_eq!(engine.attachment_bytes(&attachment_id).expect("stored bytes"), content);

    drop(engine);
    let mut reopened = Engine::open(harness.open()).expect("reopen");
    assert_eq!(reopened.attachment_bytes(&attachment_id).expect("bytes after reopen"), content);
    let raw = queued_raw(&harness, &mut reopened, &draft);
    assert!(raw.windows(15).any(|window| window == b"multipart/mixed"), "no multipart body: {}", String::from_utf8_lossy(&raw[..raw.len().min(600)]));

    let parsed = postino_sync::parse(&raw, &postino_sync::Limits::default()).expect("parse the queued MIME");
    let part = parsed.attachments.iter().find(|part| part.filename == "Relazione finale è pronta.pdf").expect("attachment part");
    assert_eq!(format!("{:x}", Sha256::digest(&part.data)), digest_before);
    assert_eq!(part.data, content, "the serialized bytes differ from the source");
    assert_eq!(part.mime, "application/pdf");

    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("Content-Disposition: attachment"), "{text}");
    assert!(text.contains("filename*=UTF-8''") || text.contains("filename*0*="), "the non-ASCII name was not RFC 2231 encoded: {text}");
    assert!(!text.contains("\r\nBcc:"), "Bcc leaked into the bytes");
}

#[test]
fn duplicate_names_are_renamed_and_remove_deletes_the_row() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);
    add(&mut engine, &draft, "nota.txt", None, b"prima");
    let second = add(&mut engine, &draft, "nota.txt", None, b"seconda");
    assert_eq!(second["draft"]["attachments"][1]["filename"], "nota (2).txt");
    let ids = second["draft"]["attachments"]
        .as_array()
        .expect("attachments")
        .iter()
        .map(|row| row["id"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);

    let removed = engine.call("draft_attachment_remove", json!({ "draft": draft, "attachment": ids[0] })).expect("remove");
    assert_eq!(removed["draft"]["attachments"].as_array().map(Vec::len), Some(1));
    assert_eq!(engine.message(&draft).expect("draft").attachments.len(), 1);
    assert!(engine.attachment_bytes(&ids[0]).is_err(), "the removed row must not resolve to a blob");
    let stored = engine.attachment_bytes(&ids[1]).expect("the other blob survives");
    assert_eq!(stored, b"seconda");
}

#[test]
fn caps_count_and_total_are_enforced_before_the_write() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);
    for index in 0..postino_core::MAX_DRAFT_ATTACHMENTS {
        add(&mut engine, &draft, &format!("file-{index}.txt"), None, b"x");
    }
    let error = engine
        .call("draft_attachment_add", json!({ "draft": draft, "filename": "ventunesimo.txt", "data_base64": "eA==" }))
        .expect_err("21st attachment");
    assert!(error.to_string().contains("at most 20"), "{error}");

    let empty = new_draft(&mut engine);
    let added = add(&mut engine, &empty, "vuoto.bin", None, b"");
    assert_eq!(added["draft"]["attachments"][0]["size"], 0, "a zero-byte file is legal");
    assert_eq!(added["draft"]["attachments"][0]["mime"], "application/octet-stream");

    let max_file = vec![0x5Au8; postino_core::MAX_ATTACHMENT_BYTES as usize];
    let at_limit = add(&mut engine, &empty, "giusto.bin", None, &max_file);
    assert_eq!(at_limit["draft"]["attachments"][1]["size"], postino_core::MAX_ATTACHMENT_BYTES);
    let error = engine
        .call("draft_attachment_add", json!({ "draft": empty, "filename": "oltre-il-totale.bin", "data_base64": "eA==" }))
        .expect_err("the 25 MiB total cap already holds the one file");
    assert!(error.to_string().contains("oltre-il-totale.bin"), "the total error must name the file: {error}");

    let second = new_draft(&mut engine);

    let too_big = vec![0u8; (postino_core::MAX_ATTACHMENT_BYTES + 1) as usize];
    let error = engine
        .call("draft_attachment_add", json!({ "draft": second, "filename": "troppo.bin", "data_base64": base64(&too_big) }))
        .expect_err("over the per-file cap");
    assert!(error.to_string().contains("at most"), "{error}");
    assert!(error.to_string().contains("troppo.bin"), "the cap error must name the file: {error}");

    let total = new_draft(&mut engine);
    let half = vec![0u8; (postino_core::MAX_ATTACHMENT_TOTAL_BYTES / 2 + 1) as usize];
    add(&mut engine, &total, "meta.bin", None, &half);
    let error = engine
        .call("draft_attachment_add", json!({ "draft": total, "filename": "seconda-meta.bin", "data_base64": base64(&half) }))
        .expect_err("over the total cap");
    assert!(error.to_string().contains("seconda-meta.bin"), "the total error must name the file: {error}");
}

#[test]
fn filename_injection_traversal_reserved_names_and_bidi_are_repaired() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);

    let injected = engine
        .call("draft_attachment_add", json!({ "draft": draft, "filename": "ok\r\nBcc: attacker@example.com.txt", "data_base64": "eA==" }))
        .expect_err("header injection");
    assert!(injected.to_string().contains("line break") || injected.to_string().contains("NUL"), "{injected}");

    let traversal = add(&mut engine, &draft, "../../etc/passwd", None, b"x");
    assert_eq!(traversal["draft"]["attachments"][0]["filename"], "etcpasswd");
    let reserved = add(&mut engine, &draft, "CON.txt", None, b"x");
    assert_eq!(reserved["draft"]["attachments"][1]["filename"], "_CON.txt");
    let bidi = add(&mut engine, &draft, "fat\u{202e}tura.pdf", None, b"x");
    assert_eq!(bidi["draft"]["attachments"][2]["filename"], "fattura.pdf");

    let overlong = format!("{}.pdf", "a".repeat(400));
    let name = add(&mut engine, &draft, &overlong, None, b"x")["draft"]["attachments"][3]["filename"].as_str().expect("name").to_owned();
    assert!(name.len() <= 255, "the name is longer than 255 bytes: {}", name.len());
}

#[test]
fn a_lying_mime_falls_back_and_base64_is_bounded() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);

    let lie = add(&mut engine, &draft, "contratto.pdf", Some("text/plain; charset=utf-8"), b"%PDF");
    assert_eq!(lie["draft"]["attachments"][0]["mime"], "application/pdf", "a parameterised declaration must not be stored raw");
    let valid_lie = add(&mut engine, &draft, "evil.exe", Some("image/png"), b"MZ");
    assert_eq!(valid_lie["draft"]["attachments"][1]["mime"], "application/octet-stream", "a syntactically valid lie must not override the extension");
    let matching_hint = add(&mut engine, &draft, "foto.png", Some("IMAGE/PNG"), b"\x89PNG");
    assert_eq!(matching_hint["draft"]["attachments"][2]["mime"], "image/png", "a hint that agrees with the extension is accepted");
    let invalid = engine
        .call("draft_attachment_add", json!({ "draft": draft, "filename": "x.bin", "data_base64": "not base64!!" }))
        .expect_err("invalid alphabet");
    assert!(invalid.to_string().contains("base64"), "{invalid}");

    let huge = "A".repeat(35_000_000);
    let error = engine
        .call("draft_attachment_add", json!({ "draft": draft, "filename": "huge.bin", "data_base64": huge }))
        .expect_err("over the base64 bound");
    assert!(error.to_string().contains("at most"), "{error}");
}

#[test]
fn a_metadata_only_attachment_refuses_send_with_the_filename() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let value = engine
        .call(
            "draft_save",
            json!({
                "to": [{ "name": "Giulia", "email": "giulia@example.com" }],
                "subject": "Senza byte",
                "body_text": "Corpo",
                "attachments": [{ "filename": "manca.pdf", "mime": "application/pdf", "size": 12 }]
            }),
        )
        .expect("metadata-only draft");
    let draft = value["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
    let error = engine.call("message_send", json!({ "draft": draft })).expect_err("send must refuse");
    assert!(error.to_string().contains("manca.pdf"), "the refusal must name the file: {error}");
    assert_eq!(engine.message(&draft).map(|message| message.summary.folder_id.clone()), Some("fld_acc_test_drafts".to_owned()));
}

#[test]
fn a_twenty_five_mib_attachment_added_by_the_operation_serializes_without_allocating_31_times() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);
    let data = vec![0xA5u8; postino_core::MAX_ATTACHMENT_BYTES as usize];
    let added = add(&mut engine, &draft, "grande.bin", None, &data);
    assert_eq!(added["draft"]["attachments"][0]["size"], postino_core::MAX_ATTACHMENT_BYTES);
    let dry = engine.call("send_dry_run", json!({ "draft": draft })).expect("serialize the 25 MiB attachment");
    assert!(dry["bytes"].as_u64().is_some_and(|bytes| bytes > postino_core::MAX_ATTACHMENT_BYTES));
    let raw = base64::engine::general_purpose::STANDARD.decode(dry["raw_base64"].as_str().expect("base64")).expect("raw");
    assert!(raw.windows(15).any(|window| window == b"multipart/mixed"));
    let parsed = postino_sync::parse(
        &raw,
        &postino_sync::Limits { max_bytes: 64 * 1024 * 1024, max_part_bytes: 32 * 1024 * 1024, ..postino_sync::Limits::default() },
    )
    .expect("parse");
    assert_eq!(parsed.attachments.first().map(|part| part.data.len()), Some(data.len()));
}

#[test]
fn an_attachment_edit_from_a_stale_engine_is_refused_without_overwriting_the_newer_draft() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut first = Engine::open(store).expect("first engine");
    let draft = new_draft(&mut first);
    let mut second = Engine::open(harness.open()).expect("second engine");
    second.call("draft_save", json!({ "id": draft, "body_text": "testo scritto dall'altro processo" })).expect("second engine save");
    second.call("draft_attachment_add", json!({ "draft": draft, "filename": "altro.txt", "data_base64": "eA==" })).expect("second engine attachment");

    let error = first
        .call("draft_attachment_add", json!({ "draft": draft, "filename": "stale.txt", "data_base64": "eQ==" }))
        .expect_err("the stale engine must not overwrite the newer draft");
    assert!(error.to_string().contains("changed in another session"), "{error}");
    let stored = harness.open();
    let message = stored.get_message(&draft).expect("message").expect("row");
    assert_eq!(message.body_text, "testo scritto dall'altro processo");
    assert_eq!(stored.attachments(&draft).expect("attachments").len(), 1);
    assert_eq!(stored.attachments(&draft).expect("attachments")[0].filename, "altro.txt");

    // A Sent copy cannot be resurrected in Drafts by a stale attachment edit.
    second.send_draft(&draft).expect("second engine sends the draft");
    let error = first
        .call("draft_attachment_remove", json!({ "draft": draft, "attachment": "altro.txt" }))
        .expect_err("the stale engine must not touch the Sent row");
    assert!(error.to_string().contains("changed in another session"), "{error}");
    let after = harness.open();
    let message = after.get_message(&draft).expect("message").expect("row");
    assert_ne!(message.summary.folder_id, "fld_acc_test_drafts", "the Sent row was moved back to Drafts");
    assert!(after.get_draft(&draft).expect("draft row").is_none(), "a draft row was resurrected");
}

#[test]
fn a_metadata_only_attachment_name_uses_the_import_naming_rule() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let value = engine
        .call(
            "draft_save",
            json!({
                "to": [{ "name": "Giulia", "email": "giulia@example.com" }],
                "attachments": [
                    { "filename": "CON.txt", "mime": "text/plain", "size": 1 },
                    { "filename": "CON.txt", "mime": "text/plain", "size": 1 },
                    { "filename": "no\tta.txt", "mime": "text/plain", "size": 1 },
                    { "filename": "evil.exe", "mime": "image/png", "size": 1 }
                ]
            }),
        )
        .expect("metadata draft");
    let names = value["draft"]["attachments"]
        .as_array()
        .expect("attachments")
        .iter()
        .map(|attachment| attachment["filename"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["_CON.txt", "_CON (2).txt", "nota.txt", "evil.exe"]);
    assert_eq!(value["draft"]["attachments"][3]["mime"], "application/octet-stream", "metadata MIME follows the same extension rule");
    let error = engine
        .call(
            "draft_save",
            json!({ "to": [{ "email": "g@example.com" }], "attachments": [{ "filename": "bad\r\nBcc: x@example.com", "mime": "text/plain", "size": 1 }] }),
        )
        .expect_err("metadata must not smuggle a header through a filename");
    assert!(error.to_string().contains("control") || error.to_string().contains("line break"), "{error}");
}

#[test]
fn a_corrupt_blob_refuses_send_with_the_filename() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);
    let added = add(&mut engine, &draft, "corrotto.pdf", None, b"byte che saranno corrotti");
    let attachment_id = added["draft"]["attachments"][0]["id"].as_str().expect("attachment id").to_owned();
    let hash = engine.attachment_hash(&attachment_id).expect("blob hash").to_owned();
    let path = postino_store::blob_root(&harness.path()).join(&hash[..2]).join(format!("{hash}.blob"));
    std::fs::write(&path, b"non e' un blob cifrato").expect("corrupt the stored blob");
    let error = engine.call("message_send", json!({ "draft": draft })).expect_err("send must refuse a corrupt blob");
    assert!(error.to_string().contains("corrotto.pdf"), "the refusal must name the file: {error}");
}

#[test]
fn reply_linkage_is_stored_serialized_and_survives_reopen() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    let value = engine
        .call(
            "draft_save",
            json!({
                "to": [{ "name": "Giulia", "email": "giulia@example.com" }],
                "subject": "Re: Preventivo",
                "body_text": "Risposta",
                "in_reply_to": "<parent@example.com>"
            }),
        )
        .expect("reply draft");
    let draft = value["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
    assert_eq!(engine.draft_reply_link(&draft), Some("parent@example.com"));
    // A later metadata-only patch keeps the link when the field is omitted.
    engine.call("draft_save", json!({ "id": draft, "subject": "Re: Preventivo aggiornato" })).expect("patch without the link");
    assert_eq!(engine.draft_reply_link(&draft), Some("parent@example.com"));

    drop(engine);
    let mut reopened = Engine::open(harness.open()).expect("reopen");
    assert_eq!(reopened.draft_reply_link(&draft), Some("parent@example.com"));
    let dry = reopened.call("send_dry_run", json!({ "draft": draft })).expect("dry run");
    let raw = base64::engine::general_purpose::STANDARD.decode(dry["raw_base64"].as_str().expect("base64")).expect("raw");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("In-Reply-To: <parent@example.com>"), "In-Reply-To missing: {text}");
    assert!(text.contains("References: <parent@example.com>"), "References missing: {text}");

    // P7 used to refuse any stored linkage; queueing must now succeed with the same headers.
    reopened.call("message_send", json!({ "draft": draft })).expect("queue a reply");
}

#[test]
fn a_reply_link_rejects_controls_and_an_overlong_id() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let mut engine = Engine::open(store).expect("engine");
    for link in ["a\r\nBcc: x@example.com", "<a@example.com> <b@example.com>", "", &"x".repeat(901)] {
        let error = engine.call("draft_save", json!({ "to": [{ "email": "g@example.com" }], "in_reply_to": link })).expect_err("invalid reply link");
        assert!(error.to_string().contains("in_reply_to"), "{link:?}: {error}");
    }
}

#[test]
fn the_signature_is_per_account_bounded_and_does_not_touch_an_existing_draft() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    support::seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Bozze", FolderKind::Drafts)]);
    store
        .upsert_account(&postino_store::Account {
            id: "acc_other".to_owned(),
            name: "Altro".to_owned(),
            email: "altro@example.com".to_owned(),
            provider: "custom".to_owned(),
            signature: "Altro".to_owned(),
            created_at: 1,
        })
        .expect("second account");
    let mut engine = Engine::open(store).expect("engine");
    let draft = new_draft(&mut engine);
    let before = engine.message(&draft).expect("draft").body_text.clone();

    let set =
        engine.call("account_signature_set", json!({ "account": ACCOUNT_ID, "signature": "Federico\nfederico@example.it" })).expect("signature");
    assert_eq!(set["account"]["account"]["signature"], "Federico\nfederico@example.it");
    assert_eq!(engine.message(&draft).expect("draft").body_text, before, "an existing draft must keep its text");
    let other = engine.accounts().iter().find(|account| account.id == "acc_other").expect("other");
    assert_eq!(other.signature, "Altro", "the wrong account was changed");

    let too_long = "è".repeat(2_100);
    let error = engine.call("account_signature_set", json!({ "account": ACCOUNT_ID, "signature": too_long })).expect_err("4 KiB cap");
    assert!(error.to_string().contains("4096") || error.to_string().contains("4 KiB"), "{error}");
    let error = engine.call("account_signature_set", json!({ "account": ACCOUNT_ID, "signature": "ok\rno" })).expect_err("CR");
    assert!(error.to_string().contains("control"), "{error}");

    drop(engine);
    let mut reopened = Engine::open(harness.open()).expect("reopen");
    assert_eq!(
        reopened.accounts().iter().find(|account| account.id == ACCOUNT_ID).map(|account| account.signature.as_str()),
        Some("Federico\nfederico@example.it")
    );
    reopened.call("account_signature_set", json!({ "account": ACCOUNT_ID, "signature": "Nuova firma\tcon tab" })).expect("tab is allowed");
}

#[test]
fn reply_target_carries_the_parent_id_and_the_persisted_reply_to() {
    let harness = Harness::new();
    let (_vault, store) = seed(&harness);
    let store_message = postino_store::Message {
        summary: postino_store::MessageSummary {
            id: "msg_parent".to_owned(),
            account_id: ACCOUNT_ID.to_owned(),
            folder_id: "fld_acc_test_drafts".to_owned(),
            thread_id: "thr_parent".to_owned(),
            from: postino_store::Address::new("Giulia", "giulia@example.com"),
            to: vec![postino_store::Address::new("Federico", "test@example.com")],
            subject: "Preventivo".to_owned(),
            snippet: "Preventivo".to_owned(),
            date: 1_700_000_000_000,
            unread: false,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: None,
        message_id: Some("parent@example.com".to_owned()),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "Ciao".to_owned(),
        body_html: None,
        remote_images: 0,
        size: None,
    };
    store.upsert_message(&store_message).expect("parent row");
    store.set_message_reply_to("msg_parent", &[postino_store::Address::new("Segreteria", "segreteria@example.com")]).expect("reply-to row");
    let engine = Engine::open(store).expect("engine");
    let target = engine.reply_target("msg_parent").expect("reply target");
    assert_eq!(target.parent_message_id.as_deref(), Some("parent@example.com"));
    assert_eq!(target.reply_to, vec![postino_core::Address::new("Segreteria", "segreteria@example.com")]);
}
