//! Minimal MCP server over newline-delimited JSON-RPC 2.0 on stdio.
//!
//! It is opt-in: only `postino mcp` starts it, it opens no port and it stops at EOF. Tool names,
//! descriptions and input schemas come from the same operation registry the CLI and the GUI use.

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use postino_core::Engine;
use serde_json::{Value, json};

const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve(engine: &mut Engine) -> Result<()> {
    let stdin = std::io::stdin();
    let lines = stdin.lock().lines();
    let mut stdout = std::io::stdout();
    for line in lines {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(engine, &request),
            Err(error) => Some(error_response(Value::Null, -32700, &format!("parse error: {error}"))),
        };
        if let Some(response) = response {
            writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn handle(engine: &mut Engine, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return id.map(|id| error_response(id, -32600, "invalid request: missing method"));
    };
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let is_notification = id.is_none();

    let outcome = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list(engine)),
        "tools/call" => tools_call(engine, &params),
        "notifications/initialized" | "notifications/cancelled" => return None,
        other => Err((-32601, format!("method not found: {other}"))),
    };

    if is_notification {
        return None;
    }
    let id = id.unwrap_or(Value::Null);
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_response(id, code, &message),
    })
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(SUPPORTED_PROTOCOLS[0]);
    let protocol = if SUPPORTED_PROTOCOLS.contains(&requested) { requested } else { SUPPORTED_PROTOCOLS[0] };
    json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "postino",
            "title": "Postino",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Postino is a local-first, agent-first mail client. Tools share the same operation registry as the desktop app and the CLI. This server is in M0 demo mode unless it was started against a configured store, so the mailbox is synthetic."
    })
}

fn tools_list(engine: &Engine) -> Value {
    let tools = engine
        .tools()
        .into_iter()
        // A sensitive operation handles a credential. It is not advertised and cannot be called over
        // MCP; the CLI and the desktop app are the only clients that may pass a password.
        .filter(|def| !def.sensitive)
        .map(|def| {
            json!({
                "name": def.name,
                "title": def.title,
                "description": def.description,
                "inputSchema": def.input_schema,
                "annotations": {
                    "readOnlyHint": def.read_only,
                    "destructiveHint": def.destructive,
                    "idempotentHint": def.read_only || matches!(def.name, "message_flag" | "label_apply" | "label_update"),
                    "openWorldHint": matches!(def.name, "account_test" | "sync_now")
                }
            })
        })
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

fn tools_call(engine: &mut Engine, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| (-32602, "tools/call needs a string `name`".to_owned()))?;
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err((-32602, "tools/call `arguments` must be a JSON object".to_owned()));
    }
    if engine.tools().into_iter().any(|def| def.name == name && def.sensitive) {
        return Ok(sensitive_refusal(name));
    }

    match engine.call(name, arguments) {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": value
            }))
        }
        Err(error) => Ok(json!({
            "content": [{ "type": "text", "text": error.to_string() }],
            "isError": true
        })),
    }
}

/// A readable refusal that does not repeat the arguments (which may contain a password).
fn sensitive_refusal(name: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": format!("operation {name:?} is not available over MCP: it handles an account credential and is available only from the CLI or the desktop app")
        }],
        "isError": true
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(engine: &mut Engine, method: &str) -> Value {
        handle(engine, &json!({ "jsonrpc": "2.0", "id": 1, "method": method })).expect("response")
    }

    #[test]
    fn tools_list_contains_the_registry() {
        let mut engine = Engine::demo();
        let response = request(&mut engine, "tools/list");
        let tools = response["result"]["tools"].as_array().expect("tools");
        assert!(tools.iter().any(|tool| tool["name"] == "messages_list"));
        assert!(tools.iter().all(|tool| tool["inputSchema"].is_object()));
    }

    #[test]
    fn tools_call_returns_text_and_structured_content() {
        let mut engine = Engine::demo();
        let response = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "messages_list", "arguments": { "folder": "inbox", "limit": 2 } }
            }),
        )
        .expect("response");
        assert!(response["result"]["content"][0]["text"].is_string());
        assert!(response["result"]["structuredContent"]["messages"].is_array());
        assert!(response.get("error").is_none());
    }

    #[test]
    fn a_tool_failure_is_an_is_error_result() {
        let mut engine = Engine::demo();
        let response = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "message_get", "arguments": { "id": "missing" } }
            }),
        )
        .expect("response");
        assert_eq!(response["result"]["isError"], json!(true));
        assert!(response["result"]["structuredContent"].is_null());
    }

    #[test]
    fn draft_tools_are_exposed_and_the_local_send_works() {
        let mut engine = Engine::demo();
        let tools = request(&mut engine, "tools/list");
        let definitions = tools["result"]["tools"].as_array().expect("tools");
        let draft_send = definitions.iter().find(|tool| tool["name"] == "draft_send").expect("draft_send");
        assert_eq!(draft_send["title"], "Move a draft to Sent (local)");
        assert!(
            draft_send["description"]
                .as_str()
                .is_some_and(|description| { description.contains("No SMTP") && description.contains("local and durable on a store") })
        );

        let saved = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "draft_save",
                    "arguments": {
                        "to": [{ "name": "Giulia", "email": "giulia.ferri@example.com" }],
                        "subject": "Bozza via MCP",
                        "body_text": "Corpo"
                    }
                }
            }),
        )
        .expect("response");
        let id = saved["result"]["structuredContent"]["draft"]["summary"]["id"].as_str().expect("id").to_owned();
        let sent = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": { "name": "draft_send", "arguments": { "id": id } }
            }),
        )
        .expect("response");
        assert_eq!(sent["result"]["structuredContent"]["message"]["folder_id"], "sent");
    }

    #[test]
    fn label_tools_are_exposed_and_round_trip_through_messages_list() {
        let mut engine = Engine::demo();
        let tools = request(&mut engine, "tools/list");
        let definitions = tools["result"]["tools"].as_array().expect("tools");
        for name in ["labels_list", "label_create", "label_update", "label_delete", "label_apply"] {
            assert!(definitions.iter().any(|tool| tool["name"] == name), "{name} is missing");
        }
        let list = definitions.iter().find(|tool| tool["name"] == "labels_list").expect("labels_list");
        assert_eq!(list["annotations"]["readOnlyHint"], json!(true));
        let apply = definitions.iter().find(|tool| tool["name"] == "label_apply").expect("label_apply");
        assert_eq!(apply["annotations"]["readOnlyHint"], json!(false));
        assert_eq!(apply["annotations"]["idempotentHint"], json!(true));
        assert!(
            apply["description"].as_str().is_some_and(|description| description.contains("local metadata")),
            "the tool description must say labels are local metadata"
        );

        let applied = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": { "name": "label_apply", "arguments": { "id": "lbl_2", "message_ids": ["msg_013"], "apply": true } }
            }),
        )
        .expect("response");
        assert_eq!(applied["result"]["structuredContent"]["changed"], 1);

        let filtered = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": { "name": "messages_list", "arguments": { "folder": "trash", "label": "lbl_2" } }
            }),
        )
        .expect("response");
        let rows = filtered["result"]["structuredContent"]["messages"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "msg_013");

        let removed = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "tools/call",
                "params": { "name": "label_apply", "arguments": { "id": "lbl_2", "message_ids": ["msg_013"], "apply": false } }
            }),
        )
        .expect("response");
        assert_eq!(removed["result"]["structuredContent"]["changed"], 1);
    }

    #[test]
    fn unknown_label_filter_is_a_readable_tool_error() {
        let mut engine = Engine::demo();
        let response = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": { "name": "messages_list", "arguments": { "label": "lbl_missing" } }
            }),
        )
        .expect("response");
        assert_eq!(response["result"]["isError"], json!(true));
        assert!(response["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("unknown label")));
    }

    #[test]
    fn unknown_search_label_is_a_readable_tool_error() {
        let mut engine = Engine::demo();
        let response = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": { "name": "search", "arguments": { "query": "fattura", "label": "lbl_missing" } }
            }),
        )
        .expect("response");
        assert_eq!(response["result"]["isError"], json!(true));
        assert!(response["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("unknown label")));
    }

    #[test]
    fn unknown_methods_are_json_rpc_errors() {
        let mut engine = Engine::demo();
        let response = request(&mut engine, "mail/steal");
        assert_eq!(response["error"]["code"], json!(-32601));
    }

    #[test]
    fn notifications_get_no_answer() {
        let mut engine = Engine::demo();
        let response = handle(&mut engine, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        assert!(response.is_none());
    }

    #[test]
    fn sensitive_operations_are_absent_and_rejected_without_echoing_arguments() {
        let mut engine = Engine::demo();
        let tools = request(&mut engine, "tools/list");
        let names = tools["result"]["tools"].as_array().expect("tools").iter().filter_map(|tool| tool["name"].as_str()).collect::<Vec<_>>();
        assert!(!names.contains(&"account_add"), "account_add is sensitive and must not be listed");
        assert!(!names.contains(&"account_remove"), "account_remove is sensitive and must not be listed");
        for name in ["accounts_list", "account_test", "sync_now", "sync_status"] {
            assert!(names.contains(&name), "{name} must stay available over MCP");
        }

        for name in ["account_add", "account_remove"] {
            let response = handle(
                &mut engine,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 20,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": { "password": "super-secret-password" } }
                }),
            )
            .expect("response");
            assert_eq!(response["result"]["isError"], json!(true), "{name}");
            let text = response["result"]["content"][0]["text"].as_str().expect("text");
            assert!(text.contains("not available over MCP"), "{text}");
            assert!(!text.contains("super-secret-password"), "the refusal echoed the arguments: {text}");
            assert!(response["result"]["structuredContent"].is_null());
        }
    }

    #[test]
    fn account_test_does_not_forward_a_server_echo_of_the_password() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mail.db");
        let mut key_bytes = [0u8; 32];
        getrandom::fill(&mut key_bytes).expect("OS random source");
        let store = postino_store::Store::open(&path, &postino_store::StoreKey::from_bytes(key_bytes)).expect("store");
        let vault = crate::test_support::MemoryVault::new();
        let server = crate::test_support::RunningServer::start_handler(|conn| async move {
            crate::test_support::refuse_list_with_secret(conn, postino_testkit::PASSWORD.to_owned()).await;
        });
        crate::test_support::seed_account(&store, &vault, "acc_mcp", server.port());
        let mut engine = Engine::open_with_vault(store, vault).expect("engine");
        engine.set_test_extra_roots(vec![server.ca()]);

        let response = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 30,
                "method": "tools/call",
                "params": { "name": "account_test", "arguments": { "id": "acc_mcp" } }
            }),
        )
        .expect("response");
        server.finish();
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("listing folders"), "the phase is not named: {text}");
        assert!(!text.contains(postino_testkit::PASSWORD), "the server echo leaked into the MCP result: {text}");
        assert!(!text.contains("A0003 NO"), "the raw response leaked into the MCP result: {text}");
    }

    #[test]
    fn sync_now_and_sync_status_work_over_mcp_without_a_secret_in_the_response() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mail.db");
        let mut key_bytes = [0u8; 32];
        getrandom::fill(&mut key_bytes).expect("OS random source");
        let store = postino_store::Store::open(&path, &postino_store::StoreKey::from_bytes(key_bytes)).expect("store");
        let vault = crate::test_support::MemoryVault::new();
        let server = crate::test_support::RunningServer::start(crate::test_support::three_folders(), 1);
        crate::test_support::seed_account(&store, &vault, "acc_mcp", server.port());
        let mut engine = Engine::open_with_vault(store, vault.clone()).expect("engine");
        engine.set_test_extra_roots(vec![server.ca()]);

        let synced = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": { "name": "sync_now", "arguments": {} }
            }),
        )
        .expect("response");
        assert!(synced.get("error").is_none(), "{synced}");
        assert_eq!(synced["result"]["structuredContent"]["outcome"]["account"], "acc_mcp");
        assert_eq!(synced["result"]["structuredContent"]["outcome"]["folders"], 3);
        assert_eq!(synced["result"]["structuredContent"]["outcome"]["fetched"], 3);

        let status = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 22,
                "method": "tools/call",
                "params": { "name": "sync_status", "arguments": {} }
            }),
        )
        .expect("response");
        assert_eq!(status["result"]["structuredContent"]["accounts"][0]["status"]["state"], "idle");
        assert!(status["result"]["structuredContent"]["accounts"][0]["status"]["last_success"].is_number());

        let accounts = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 23,
                "method": "tools/call",
                "params": { "name": "accounts_list", "arguments": {} }
            }),
        )
        .expect("response");
        assert_eq!(accounts["result"]["structuredContent"]["accounts"][0]["server"]["host"], "localhost");
        let encoded = serde_json::to_string(&accounts).expect("JSON");
        assert!(!encoded.contains(postino_testkit::PASSWORD), "an MCP response contained the password");
        server.finish();
    }
}
