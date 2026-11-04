//! "Never crash" is the product's central promise: mail, attachments and MCP calls are untrusted
//! input. This file feeds hostile argument sets to **every** operation in the registry and proves
//! that a malformed body is a typed `Err`, never a panic and never a silent success.
//!
//! The operation list comes from [`postino_core::ops::op_defs`], so a new operation is covered the
//! moment it is registered; nothing here hard-codes the names. The demo engine is the same synthetic,
//! in-memory mailbox the other core tests use, so no case reaches the network or the keyring.
//!
//! This is a bounded hostile sweep, not a fuzz corpus: about ten small bodies per operation, one of
//! which carries a 1 MiB string. It finishes in well under a second.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use postino_core::ops::{self, OpDef};
use postino_core::{Engine, OpError};
use serde_json::{Map, Value, json};

/// One hostile request body and what the registry must do with it.
struct HostileCase {
    label: &'static str,
    args: Value,
    /// `true` when the body is objectively malformed (an unknown key, a missing required key, a
    /// number where a string is declared, a payload that is not base64, an integer out of its
    /// declared range) and dispatch must refuse it. `false` means the case is merely hostile: a
    /// tolerant operation may accept it, so the only assertion is that it does not panic.
    must_refuse: bool,
}

/// The upper and lower bounds an integer property declares, when the schema declares them.
#[derive(Clone, Copy, Default)]
struct DeclaredBounds {
    min: Option<i128>,
    max: Option<i128>,
}

impl DeclaredBounds {
    fn refuses(self, value: i128) -> bool {
        self.min.is_some_and(|min| value < min) || self.max.is_some_and(|max| value > max)
    }
}

/// Build the hostile bodies for one operation from its own JSON Schema.
fn hostile_cases(def: &OpDef, big: &str) -> Vec<HostileCase> {
    let required = def.input_schema.get("required").and_then(Value::as_array).map_or(0, Vec::len);
    let mut cases = vec![
        // Missing everything. Refused when the schema declares a required key.
        HostileCase { label: "empty object", args: json!({}), must_refuse: required > 0 },
        // Every request struct uses #[serde(deny_unknown_fields)].
        HostileCase { label: "unknown field", args: json!({ "unknown_field": 1 }), must_refuse: true },
        // The body is an object by contract. `parse` refuses anything else before serde can read the
        // derived positional form, so the sequence is rejected for every operation, including the
        // ones whose fields are all optional.
        HostileCase { label: "array instead of object", args: json!([]), must_refuse: true },
    ];

    let Some(properties) = def.input_schema.get("properties").and_then(Value::as_object) else {
        return cases;
    };

    let mut string_key: Option<String> = None;
    let mut base64_key: Option<String> = None;
    let mut integer: Option<(String, DeclaredBounds)> = None;
    for (name, property) in properties {
        let types = declared_types(property);
        if types.iter().any(|kind| kind == "string") {
            if name.contains("base64") {
                base64_key.get_or_insert_with(|| name.clone());
            } else {
                string_key.get_or_insert_with(|| name.clone());
            }
        }
        if integer.is_none() && types.iter().any(|kind| kind == "integer") {
            integer = Some((name.clone(), declared_bounds(property)));
        }
    }

    if let Some(key) = &string_key {
        cases.push(HostileCase { label: "number for a string field", args: object_with(key, json!(123)), must_refuse: true });
        cases.push(HostileCase { label: "1 MiB string", args: object_with(key, Value::String(big.to_owned())), must_refuse: false });
        cases.push(HostileCase { label: "empty string", args: object_with(key, Value::String(String::new())), must_refuse: false });
        cases.push(HostileCase { label: "null for a string field", args: object_with(key, Value::Null), must_refuse: false });
    }

    if let Some(key) = &base64_key {
        cases.push(HostileCase { label: "invalid base64", args: object_with(key, json!("!!!!")), must_refuse: true });
    }

    if let Some((key, bounds)) = &integer {
        cases.push(HostileCase { label: "negative number", args: object_with(key, Value::from(-1)), must_refuse: bounds.refuses(-1) });
        cases.push(HostileCase {
            label: "very large number",
            args: object_with(key, Value::from(u64::MAX)),
            must_refuse: bounds.refuses(i128::from(u64::MAX)),
        });
    }

    cases
}

/// The JSON types schemars declared for one property (`Option<T>` is `["T", "null"]`).
fn declared_types(property: &Value) -> Vec<String> {
    match property.get("type") {
        Some(Value::String(kind)) => vec![kind.clone()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// Read the bounds a property declares, falling back to the fixed-width integer `format`.
fn declared_bounds(property: &Value) -> DeclaredBounds {
    let format = property.get("format").and_then(Value::as_str).unwrap_or_default();
    let (format_min, format_max) = match format {
        "uint8" => (Some(0), Some(u8::MAX as i128)),
        "uint16" => (Some(0), Some(u16::MAX as i128)),
        "uint32" => (Some(0), Some(u32::MAX as i128)),
        "uint64" => (Some(0), Some(u64::MAX as i128)),
        "int8" => (Some(i8::MIN as i128), Some(i8::MAX as i128)),
        "int16" => (Some(i16::MIN as i128), Some(i16::MAX as i128)),
        "int32" => (Some(i32::MIN as i128), Some(i32::MAX as i128)),
        "int64" => (Some(i64::MIN as i128), Some(i64::MAX as i128)),
        // `usize`/`isize` carry no width, so a range cannot be assumed.
        _ => (None, None),
    };
    DeclaredBounds {
        min: property.get("minimum").and_then(integer_value).or(format_min),
        max: property.get("maximum").and_then(integer_value).or(format_max),
    }
}

/// A schema bound, accepting it whether serde_json stored it as an integer or a float.
fn integer_value(value: &Value) -> Option<i128> {
    value.as_i64().map(i128::from).or_else(|| value.as_u64().map(i128::from)).or_else(|| {
        let number = value.as_f64()?;
        (number.fract() == 0.0 && number >= i128::MIN as f64 && number <= i128::MAX as f64).then_some(number as i128)
    })
}

fn object_with(key: &str, value: Value) -> Value {
    let mut map = Map::new();
    map.insert(key.to_owned(), value);
    Value::Object(map)
}

/// A short, single-line rendering of a body or a result for a failure message.
fn preview(value: &Value) -> String {
    let text = value.to_string();
    let clipped: String = text.chars().take(160).collect();
    if clipped.chars().count() < text.chars().count() { format!("{clipped}... ({} bytes)", text.len()) } else { clipped }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic payload that is not text".to_owned()
    }
}

/// Dispatch one case, converting a panic into a recorded finding instead of aborting the sweep.
/// Returns `true` when the call panicked.
fn run_case(engine: &mut Engine, def: &OpDef, case: &HostileCase, findings: &mut Vec<String>) -> bool {
    let label = format!("{} [{}]", def.name, case.label);
    let outcome = catch_unwind(AssertUnwindSafe(|| ops::dispatch(engine, def.name, case.args.clone())));
    let panicked = outcome.is_err();
    match outcome {
        Err(payload) => findings.push(format!("PANIC: {label} args={} payload={}", preview(&case.args), panic_text(payload))),
        Ok(Ok(value)) if case.must_refuse => {
            findings.push(format!("SILENT SUCCESS: {label} args={} returned {}", preview(&case.args), preview(&value)))
        }
        Ok(Err(error)) => {
            let message = error.to_string();
            if message.trim().is_empty() {
                findings.push(format!("EMPTY ERROR: {label} returned an error with no message"));
            }
            if case.label == "unknown field" && matches!(error, OpError::UnknownOp(_)) {
                findings.push(format!("UNREGISTERED: {label}: op_defs() lists an operation that dispatch does not know: {message}"));
            }
        }
        Ok(Ok(_)) => {}
    }
    panicked
}

#[test]
fn no_operation_panics_and_no_malformed_body_is_accepted() {
    let started = Instant::now();
    let defs = ops::op_defs();
    assert!(!defs.is_empty(), "the operation registry is empty");

    let mut engine = Engine::demo();
    let big = "A".repeat(1024 * 1024);
    let mut findings = Vec::new();
    let mut dispatches = 0usize;
    let mut panics = 0usize;
    let mut smallest = usize::MAX;
    let mut largest = 0usize;

    for def in &defs {
        let cases = hostile_cases(def, &big);
        smallest = smallest.min(cases.len());
        largest = largest.max(cases.len());
        for case in &cases {
            if run_case(&mut engine, def, case, &mut findings) {
                panics += 1;
            }
            dispatches += 1;
        }
    }

    println!(
        "panic_hunt: {} operations, {dispatches} hostile argument sets ({} to {largest} per operation), \
         {panics} panic(s), {:.2} s",
        defs.len(),
        smallest,
        started.elapsed().as_secs_f64()
    );

    assert!(dispatches >= defs.len() * 3, "only {dispatches} dispatches for {} operations", defs.len());
    assert!(started.elapsed().as_secs() < 60, "the hostile sweep took {:?}; it must stay well under a minute", started.elapsed());
    assert!(panics == 0, "{panics} operation call(s) panicked on hostile input");
    assert!(findings.is_empty(), "the operation surface violated its contract:\n{}", findings.join("\n"));
}

#[test]
fn an_unknown_operation_is_a_readable_error_not_a_panic() {
    let mut engine = Engine::demo();
    let long_name = "x".repeat(1024);
    for name in ["", "nope", "Account.Add", "messages list", "messages-list", " ", long_name.as_str()] {
        let outcome = catch_unwind(AssertUnwindSafe(|| ops::dispatch(&mut engine, name, json!({}))));
        let result = outcome.unwrap_or_else(|payload| panic!("dispatch panicked on unknown name {name:?}: {}", panic_text(payload)));
        let error = result.expect_err("an unknown operation name must never succeed");
        let message = error.to_string();
        assert!(matches!(&error, OpError::UnknownOp(_)), "unknown name {name:?} produced {error:?}, not UnknownOp");
        assert!(!message.trim().is_empty(), "unknown name {name:?} produced an empty message");
        if !name.trim().is_empty() {
            assert!(message.contains(name), "unknown name {name:?} is not named in its message: {message}");
        }
    }
}

#[test]
fn every_operation_def_is_documented_and_named_for_mcp() {
    let defs = ops::op_defs();
    let mut seen = std::collections::BTreeSet::new();
    for def in &defs {
        assert!(!def.title.trim().is_empty(), "{} has an empty title", def.name);
        assert!(!def.description.trim().is_empty(), "{} has an empty description", def.name);
        assert!(is_snake_case(def.name), "{} is not snake_case; MCP clients reject dots and other punctuation", def.name);
        assert!(!def.name.contains('.'), "{} contains a dot; MCP clients reject dots", def.name);
        assert!(seen.insert(def.name), "operation name {} is registered twice", def.name);
        assert!(def.input_schema.is_object(), "{} has no JSON Schema for its arguments", def.name);
        assert_eq!(
            def.input_schema.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "{}: the schema accepts unknown argument keys, but the request struct uses deny_unknown_fields",
            def.name
        );
    }
    assert_eq!(seen.len(), defs.len(), "the registry has duplicate operation names");
    println!("panic_hunt: {} operation definitions carry a title, a description and a snake_case name", defs.len());
}

/// `snake_case`: lowercase ASCII letters, digits and single underscores, starting with a letter.
fn is_snake_case(name: &str) -> bool {
    name.chars().next().is_some_and(|first| first.is_ascii_lowercase())
        && name.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        && !name.contains("__")
        && !name.ends_with('_')
}
