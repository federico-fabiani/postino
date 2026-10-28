//! Alternate senders: local metadata, bounded, and never a silent change to the From header.

mod support;

use postino_core::Engine;
use postino_store::FolderKind;
use serde_json::json;

use support::{ACCOUNT_ID, Harness, MemoryVault, seed_account_with_smtp};

fn engine() -> (Harness, std::sync::Arc<MemoryVault>, Engine) {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account_with_smtp(
        &store,
        &vault,
        ACCOUNT_ID,
        1,
        1,
        &[("inbox", "Inbox", FolderKind::Inbox), ("drafts", "Drafts", FolderKind::Drafts), ("sent", "Sent", FolderKind::Sent)],
    );
    let engine = Engine::open(store).expect("engine");
    (harness, vault, engine)
}

fn create(engine: &mut Engine, name: &str, email: &str) -> serde_json::Value {
    engine.call("identity_create", json!({ "name": name, "email": email, "signature": "Ada\nLaboratorio" })).expect("identity_create")
}

#[test]
fn identities_round_trip_and_are_scoped_to_the_account() {
    let (_harness, _vault, mut engine) = engine();
    assert_eq!(engine.call("identities_list", json!({})).expect("list"), json!([]));

    let created = create(&mut engine, "Ada", "ada@example.com");
    let id = created["id"].as_str().expect("identity id").to_owned();
    assert!(id.starts_with("idn_"));
    assert_eq!(created["account_id"], ACCOUNT_ID);
    assert_eq!(created["signature"], "Ada\nLaboratorio");
    let list = engine.call("identities_list", json!({})).expect("list");
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["email"], "ada@example.com");

    let duplicate = engine.call("identity_create", json!({ "name": "Altra", "email": "ADA@example.com" })).expect_err("duplicate");
    assert!(duplicate.to_string().contains("already"), "{duplicate}");
    let own = engine.call("identity_create", json!({ "name": "Io", "email": "user@example.com" })).expect_err("the account address");
    assert!(own.to_string().contains("account"), "{own}");

    let updated =
        engine.call("identity_update", json!({ "id": id, "name": "Ada L.", "email": "ada.l@example.com", "signature": "" })).expect("update");
    assert_eq!(updated["name"], "Ada L.");
    assert_eq!(updated["email"], "ada.l@example.com");
    assert_eq!(updated["signature"], "");
    assert_eq!(updated["created_at"], created["created_at"]);

    let deleted = engine.call("identity_delete", json!({ "id": id })).expect("delete");
    assert_eq!(deleted["identity"]["id"], id.as_str());
    assert_eq!(engine.call("identities_list", json!({})).expect("list"), json!([]));
    let missing = engine.call("identity_delete", json!({ "id": id })).expect_err("double delete");
    assert!(missing.to_string().contains("unknown identity"), "{missing}");
}

#[test]
fn identities_are_scoped_to_one_account() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    for id in ["acc_a", "acc_b"] {
        seed_account_with_smtp(&store, &vault, id, 1, 1, &[("inbox", "Inbox", FolderKind::Inbox), ("drafts", "Drafts", FolderKind::Drafts)]);
    }
    let mut engine = Engine::open(store).expect("engine");
    engine.call("identity_create", json!({ "account": "acc_a", "name": "Ada", "email": "ada@example.com" })).expect("create for A");
    assert_eq!(engine.call("identities_list", json!({ "account": "acc_a" })).expect("A").as_array().map(Vec::len), Some(1));
    assert_eq!(engine.call("identities_list", json!({ "account": "acc_b" })).expect("B"), json!([]));
    let missing_account = engine.call("identities_list", json!({})).expect_err("two accounts need an explicit account");
    assert!(missing_account.to_string().contains("account"), "{missing_account}");
}

#[test]
fn a_draft_keeps_the_selected_sender_and_send_resolves_it() {
    let (_harness, _vault, mut engine) = engine();
    let identity = create(&mut engine, "Ada", "ada@example.com");
    let id = identity["id"].as_str().expect("identity id");

    let saved = engine
        .call(
            "draft_save",
            json!({
                "from": id,
                "to": [{ "name": "Grace", "email": "grace@example.com" }],
                "subject": "Dal laboratorio",
                "body_text": "Ciao Grace"
            }),
        )
        .expect("save from the identity");
    assert_eq!(saved["draft"]["summary"]["from"]["email"], "ada@example.com");
    let draft_id = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();

    let dry = engine.call("send_dry_run", json!({ "draft": draft_id })).expect("dry run");
    assert_eq!(dry["envelope"]["from"], "ada@example.com");
    assert_eq!(dry["envelope"]["recipients"], json!(["grace@example.com"]));

    // A subject-only patch must keep the sender that was chosen when the draft was created.
    let patched = engine.call("draft_save", json!({ "id": draft_id, "subject": "Dal laboratorio, rivisto" })).expect("patch");
    assert_eq!(patched["draft"]["summary"]["from"]["email"], "ada@example.com");

    let unknown = engine.call("identity_create", json!({ "name": "X", "email": "x@example.com" })).expect("create X");
    let x_id = unknown["id"].as_str().expect("id");
    let stale = engine
        .call("draft_save", json!({ "from": x_id, "to": [{ "name": "Grace", "email": "grace@example.com" }], "subject": "Da X", "body_text": "x" }))
        .expect("save from X");
    let stale_id = stale["draft"]["summary"]["id"].as_str().expect("stale draft id");
    engine.call("identity_delete", json!({ "id": x_id })).expect("delete X");
    let unknown_from = engine.call("draft_save", json!({ "id": draft_id, "subject": "Da X", "from": x_id })).expect_err("deleted sender");
    assert!(unknown_from.to_string().contains("unknown sender"), "{unknown_from}");
    let no_fallback = engine.call("send_dry_run", json!({ "draft": stale_id })).expect_err("a deleted sender never falls back to the account");
    assert!(no_fallback.to_string().contains("neither the account address nor a configured identity"), "{no_fallback}");
}

#[test]
fn identities_are_bounded_and_demo_mode_refuses_writes() {
    let (_harness, _vault, mut engine) = engine();
    let empty = engine.call("identity_create", json!({ "name": " ", "email": "a@example.com" })).expect_err("empty name");
    assert!(empty.to_string().contains("must not be empty"), "{empty}");
    let bad = engine.call("identity_create", json!({ "name": "Ada", "email": "not a mailbox" })).expect_err("invalid address");
    assert!(bad.to_string().contains("email"), "{bad}");
    let control = engine.call("identity_create", json!({ "name": "Ada", "email": "ada@example.com", "signature": "a\u{0}b" })).expect_err("NUL");
    assert!(control.to_string().contains("control characters"), "{control}");

    for index in 0..10 {
        create(&mut engine, &format!("Mittente {index:02}"), &format!("sender{index}@example.com"));
    }
    let cap = engine.call("identity_create", json!({ "name": "Undicesimo", "email": "eleventh@example.com" })).expect_err("cap");
    assert!(cap.to_string().contains("at most 10"), "{cap}");

    let mut demo = Engine::demo();
    assert_eq!(demo.call("identities_list", json!({})).expect("demo list"), json!([]));
    let error = demo.call("identity_create", json!({ "name": "Ada", "email": "ada@example.com" })).expect_err("demo write");
    assert!(error.to_string().contains("needs a local store"), "{error}");

    let definitions = postino_core::op_defs();
    for name in ["identities_list", "identity_create", "identity_update", "identity_delete"] {
        let definition = definitions.iter().find(|definition| definition.name == name).unwrap_or_else(|| panic!("{name} is missing"));
        assert!(definition.input_schema.is_object(), "{name} has no schema");
        assert!(!definition.sensitive, "{name} must not be sensitive");
    }
    let list = definitions.iter().find(|definition| definition.name == "identities_list").expect("list");
    assert!(list.read_only);
    let delete = definitions.iter().find(|definition| definition.name == "identity_delete").expect("delete");
    assert!(delete.destructive);
}
