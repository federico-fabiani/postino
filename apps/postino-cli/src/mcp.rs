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
        .map(|def| {
            json!({
                "name": def.name,
                "title": def.title,
                "description": def.description,
                "inputSchema": def.input_schema,
                "annotations": {
                    "readOnlyHint": def.read_only,
                    "destructiveHint": def.destructive,
                    "idempotentHint": def.read_only || def.name == "message_flag",
                    "openWorldHint": false
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
}
