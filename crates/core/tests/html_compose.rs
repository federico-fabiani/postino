//! Email-safe HTML composition through `draft_save`: source is compiled, never accepted as raw
//! HTML, and the serialized message carries a plain-text alternative without Markdown markers.

mod support;

use base64::Engine as _;
use postino_core::Engine;
use postino_store::FolderKind;
use serde_json::json;

use support::{ACCOUNT_ID, Harness, MemoryVault, seed_account};

#[test]
fn an_html_draft_compiles_inline_css_and_serializes_both_parts() {
    let mut engine = Engine::demo();
    let saved = engine
        .call(
            "draft_save",
            json!({
                "to": [{ "name": "Ada", "email": "ada@example.com" }],
                "subject": "Preventivo",
                "body_text": "**Ciao** Ada,\n\n| Voce | Importo |\n| --- | ---: |\n| Progetto | 120 |\n",
                "body_format": "html"
            }),
        )
        .expect("save an HTML draft");
    let id = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
    let html = saved["draft"]["body_html"].as_str().expect("compiled HTML");
    assert!(html.contains("data-postino-compose=\"v1\""), "{html}");
    assert!(html.contains("<strong>Ciao</strong>"), "{html}");
    assert!(html.contains("<table"), "the pipe table must compile");
    assert!(!html.contains("**Ciao**"), "the Markdown markers must not leak into HTML");
    assert_eq!(saved["draft"]["body_text"], "**Ciao** Ada,\n\n| Voce | Importo |\n| --- | ---: |\n| Progetto | 120 |\n");

    let patched = engine.call("draft_save", json!({ "id": id, "subject": "Preventivo rivisto" })).expect("patch the subject");
    assert_eq!(patched["draft"]["body_html"], json!(html), "a subject-only patch must keep the compiled part");

    let dry = engine.call("send_dry_run", json!({ "draft": id })).expect("dry-run the composed draft");
    let raw = base64::engine::general_purpose::STANDARD
        .decode(dry["raw_base64"].as_str().unwrap_or_else(|| panic!("raw base64: {dry:?}")))
        .expect("decode raw");
    let raw = String::from_utf8(raw).expect("UTF-8 message");
    assert!(raw.contains("multipart/alternative"), "{raw}");
    assert!(raw.contains("text/plain"), "{raw}");
    assert!(raw.contains("text/html"), "{raw}");
    assert!(raw.contains("Ciao Ada"), "{raw}");
    assert!(!raw.contains("**Ciao**"), "the text alternative must not carry source markers: {raw}");

    let changed = engine
        .call("draft_save", json!({ "id": id, "body_text": "**Nuovo** testo, senza formato esplicito" }))
        .expect("patch the source without a format");
    let changed_html = changed["draft"]["body_html"].as_str().expect("recompiled HTML");
    assert!(changed_html.contains("<strong>Nuovo</strong>"), "a compiler-owned draft recompiles when its source changes: {changed_html}");
    assert!(!changed_html.contains("**Nuovo**"));

    let plain = engine.call("draft_save", json!({ "id": id, "body_format": "plain" })).expect("remove the HTML part");
    assert!(plain["draft"]["body_html"].is_null(), "plain must remove the HTML part");
}

#[test]
fn export_rebuilds_the_compiled_parts_instead_of_leaking_the_source() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "Posta in arrivo", FolderKind::Inbox), ("drafts", "Bozze", FolderKind::Drafts)]);
    let mut engine = Engine::open(store).expect("engine");
    let saved = engine
        .call(
            "draft_save",
            json!({
                "account": ACCOUNT_ID,
                "to": [{ "name": "Ada", "email": "ada@example.com" }],
                "subject": "Bozza HTML",
                "body_text": "**Ciao** Ada,\n\n| Voce | Importo |\n| --- | ---: |\n| Progetto | 120 |\n",
                "body_format": "html"
            }),
        )
        .expect("save an HTML draft");
    let id = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
    let exported = engine.call("message_export", json!({ "id": id, "account": ACCOUNT_ID })).expect("export the draft");
    let raw = base64::engine::general_purpose::STANDARD.decode(exported["eml_base64"].as_str().expect("base64")).expect("decode");
    let parsed = postino_sync::parse(&raw, &postino_sync::Limits::default()).expect("parse the exported .eml");
    let text = parsed.text.as_deref().unwrap_or_default();
    let html = parsed.html.as_deref().unwrap_or_default();
    assert!(text.contains("Ciao Ada"), "the text alternative must be generated: {text}");
    assert!(!text.contains("**Ciao**"), "the exported text must not carry source markers: {text}");
    assert!(html.contains("<strong>Ciao</strong>"), "{html}");
    assert!(html.contains("<table"), "{html}");
}
