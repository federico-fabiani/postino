//! Local compose templates: persisted in encrypted meta, bounded, and available to the CLI, MCP and
//! the composer. No network and no provider-side template is involved.

mod support;

use postino_core::Engine;
use serde_json::json;
use support::Harness;

fn create(engine: &mut Engine, name: &str, subject: &str, body: &str) -> serde_json::Value {
    engine.call("template_create", json!({ "name": name, "subject": subject, "body": body })).expect("template_create")
}

#[test]
fn templates_round_trip_are_ordered_and_survive_a_reopen() {
    let harness = Harness::new();
    let mut engine = Engine::open(harness.open()).expect("engine");
    assert_eq!(engine.call("templates_list", json!({})).expect("list"), json!([]));

    let first = create(&mut engine, "Preventivo", "Il tuo preventivo", "Ciao,\n\nil preventivo è in allegato.");
    let id = first["id"].as_str().expect("id").to_owned();
    assert_eq!(first["name"], "Preventivo");
    assert_eq!(first["body"], "Ciao,\n\nil preventivo è in allegato.");
    create(&mut engine, "auguri", "Buone feste", "Auguri!");
    let list = engine.call("templates_list", json!({})).expect("list");
    assert_eq!(list.as_array().map(Vec::len), Some(2));
    assert_eq!(list[0]["name"], "auguri", "names are ordered ignoring case");
    assert_eq!(list[1]["name"], "Preventivo");

    let updated = engine.call("template_update", json!({ "id": id, "subject": "Preventivo aggiornato", "body": "Nuovo corpo" })).expect("update");
    assert_eq!(updated["name"], "Preventivo");
    assert_eq!(updated["subject"], "Preventivo aggiornato");
    assert_eq!(updated["created_at"], first["created_at"], "the creation instant never changes");

    drop(engine);
    let mut reopened = Engine::open(harness.open()).expect("reopen");
    let list = reopened.call("templates_list", json!({})).expect("list after reopen");
    assert_eq!(list.as_array().map(Vec::len), Some(2));
    assert_eq!(list[1]["body"], "Nuovo corpo");

    let deleted = reopened.call("template_delete", json!({ "id": id })).expect("delete");
    assert_eq!(deleted, json!({ "id": id, "deleted": true }));
    assert_eq!(reopened.call("templates_list", json!({})).expect("list").as_array().map(Vec::len), Some(1));
    let missing = reopened.call("template_update", json!({ "id": "tpl_999", "name": "x" })).expect_err("unknown id");
    assert!(missing.to_string().contains("unknown template"), "{missing}");
}

#[test]
fn template_bounds_and_demo_mode_are_explicit() {
    let harness = Harness::new();
    let mut engine = Engine::open(harness.open()).expect("engine");
    let error = engine.call("template_create", json!({ "name": "  ", "subject": "", "body": "" })).expect_err("empty name");
    assert!(error.to_string().contains("must not be empty"), "{error}");
    let error = engine.call("template_create", json!({ "name": "x", "subject": "", "body": "a\u{0}b" })).expect_err("NUL in body");
    assert!(error.to_string().contains("control characters"), "{error}");
    let error = engine.call("template_create", json!({ "name": "x", "subject": "", "body": "a".repeat(16 * 1024 + 1) })).expect_err("oversized body");
    assert!(error.to_string().contains("limited"), "{error}");

    for index in 0..20 {
        create(&mut engine, &format!("Modello {index:02}"), "", "Corpo");
    }
    let error = engine.call("template_create", json!({ "name": "Ventunesimo", "subject": "", "body": "" })).expect_err("cap");
    assert!(error.to_string().contains("at most 20"), "{error}");

    let mut demo = Engine::demo();
    assert_eq!(demo.call("templates_list", json!({})).expect("demo list"), json!([]));
    let error = demo.call("template_create", json!({ "name": "x", "subject": "", "body": "" })).expect_err("demo write");
    assert!(error.to_string().contains("needs a local store"), "{error}");
}

#[test]
fn template_operations_are_in_the_registry_with_the_right_annotations() {
    let definitions = postino_core::op_defs();
    for name in ["templates_list", "template_create", "template_update", "template_delete"] {
        let definition = definitions.iter().find(|definition| definition.name == name).unwrap_or_else(|| panic!("{name} is missing"));
        assert!(definition.input_schema.is_object(), "{name} has no schema");
        assert!(!definition.sensitive, "{name} must not be sensitive");
    }
    let list = definitions.iter().find(|definition| definition.name == "templates_list").expect("list");
    assert!(list.read_only);
    assert!(!list.destructive);
    let delete = definitions.iter().find(|definition| definition.name == "template_delete").expect("delete");
    assert!(delete.destructive);
}
