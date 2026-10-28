//! Minimal MCP server over newline-delimited JSON-RPC 2.0 on stdio.
//!
//! It is opt-in: only `postino mcp` starts it, it opens no port and it stops at EOF. Tool names,
//! descriptions and input schemas come from the same operation registry the CLI and the GUI use.

use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use postino_core::Engine;
use serde_json::{Value, json};

const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// A per-session ceiling for tool and resource calls. It is deliberately generous for normal agent
/// work and small enough to stop a runaway loop before it drains a mailbox or floods the audit log.
const MAX_CALLS_PER_WINDOW: u32 = 300;
const CALL_WINDOW: Duration = Duration::from_secs(60);

/// A single compact description is enough for an agent that already has the schema; full mode keeps
/// the registry's long, actionable text.
const COMPACT_DESCRIPTION_CHARS: usize = 180;

/// The audit file is rotated once it reaches this size. Operation names and resource ids are
/// metadata, never message bodies or credentials.
const MAX_AUDIT_BYTES: u64 = 8 * 1024 * 1024;

/// A legitimate 25 MiB attachment is a ~35 MB base64 string inside JSON. Anything above that plus
/// headroom is not a request this server can serve, and must not become an unbounded `String`.
const MAX_REQUEST_LINE: usize = (postino_core::MAX_ATTACHMENT_BYTES as usize).div_ceil(3) * 4 + 2 * 1024 * 1024;

enum BoundedLine {
    Eof,
    Line(Vec<u8>),
    TooLong,
}

/// Operations that can create a network submission or resolve a delivered one. They are listed but
/// disabled unless the server was started with `--allow-send`; `outbox_retry` is included because it
/// can send stored bytes, `outbox_mark_sent` because it records a delivery decision that removes the
/// uncertainty a person should resolve, and `rules_run` because an enabled rule may forward. A
/// `rules_create`/`rules_update` payload that carries a forward action is refused before parsing.
const SEND_GATED: [&str; 5] = ["message_send", "send_schedule", "outbox_retry", "outbox_mark_sent", "rules_run"];

const SEND_DISABLED_REASON: &str = "sending is disabled: start `postino mcp --allow-send` to allow message_send, send_schedule, outbox_retry, outbox_mark_sent and rules_run (including rule forwards)";

/// The smallest mailbox surface the MCP handlers need. `Engine` is the in-process implementation;
/// the CLI's daemon client is a second one. Tool names, descriptions and schemas still come from the
/// one registry.
pub trait McpBackend {
    fn tools(&self) -> Vec<postino_core::OpDef>;
    fn call(&mut self, name: &str, args: Value) -> std::result::Result<Value, String>;
    fn rule_has_forward(&self, id: &str) -> std::result::Result<Option<bool>, String>;
    fn set_allow_send(&mut self, allow_send: bool);
}

impl McpBackend for Engine {
    fn tools(&self) -> Vec<postino_core::OpDef> {
        Engine::tools(self)
    }

    fn call(&mut self, name: &str, args: Value) -> std::result::Result<Value, String> {
        postino_core::ops::dispatch(self, name, args).map_err(|error| error.to_string())
    }

    fn rule_has_forward(&self, id: &str) -> std::result::Result<Option<bool>, String> {
        Engine::rule_has_forward(self, id).map_err(|error| error.to_string())
    }

    fn set_allow_send(&mut self, allow_send: bool) {
        Engine::set_allow_send(self, allow_send);
    }
}

/// Deployment options for one MCP process. The audit path is kept as an owned path so it can be
/// derived from the store or the platform data directory before stdin is opened.
#[derive(Debug, Clone, Default)]
pub struct McpOptions {
    pub allow_send: bool,
    pub compact: bool,
    pub audit: Option<PathBuf>,
}

pub fn serve(engine: &mut Engine, options: McpOptions) -> Result<()> {
    serve_backend(engine, options)
}

pub fn serve_backend<B: McpBackend + ?Sized>(backend: &mut B, options: McpOptions) -> Result<()> {
    backend.set_allow_send(options.allow_send);
    let mut session = Session::new(options);
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    loop {
        let line = match read_bounded_line(&mut reader, MAX_REQUEST_LINE) {
            Ok(BoundedLine::Eof) => break,
            Ok(BoundedLine::Line(line)) => line,
            Ok(BoundedLine::TooLong) => {
                let response = error_response(Value::Null, -32600, &format!("request line is longer than {MAX_REQUEST_LINE} bytes"));
                writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
                stdout.flush()?;
                continue;
            }
            Err(error) => return Err(anyhow::Error::new(error)).context("reading stdin"),
        };
        let Ok(line) = String::from_utf8(line) else {
            let response = error_response(Value::Null, -32700, "parse error: request is not UTF-8");
            writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
            stdout.flush()?;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle_session(backend, &request, &mut session),
            Err(error) => Some(error_response(Value::Null, -32700, &format!("parse error: {error}"))),
        };
        if let Some(response) = response {
            writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// State that belongs to one stdio session: the send gate, compact listing, rate guardrail and the
/// append-only audit trail. It never holds a message body or a credential.
struct Session {
    allow_send: bool,
    compact: bool,
    rate: RateLimiter,
    audit: AuditLog,
}

impl Session {
    fn new(options: McpOptions) -> Self {
        Self {
            allow_send: options.allow_send,
            compact: options.compact,
            rate: RateLimiter::new(MAX_CALLS_PER_WINDOW, CALL_WINDOW),
            audit: AuditLog::new(options.audit.as_deref()),
        }
    }
}

struct RateLimiter {
    limit: u32,
    window: Duration,
    calls: std::collections::VecDeque<Instant>,
}

impl RateLimiter {
    fn new(limit: u32, window: Duration) -> Self {
        Self { limit: limit.max(1), window, calls: std::collections::VecDeque::new() }
    }

    fn allow(&mut self) -> bool {
        let now = Instant::now();
        while self.calls.front().is_some_and(|at| now.duration_since(*at) >= self.window) {
            self.calls.pop_front();
        }
        if self.calls.len() >= self.limit as usize {
            return false;
        }
        self.calls.push_back(now);
        true
    }

    fn refusal(&self) -> String {
        format!(
            "rate limit exceeded: at most {} MCP calls per {} seconds; wait and retry. No operation was performed.",
            self.limit,
            self.window.as_secs()
        )
    }
}

/// Best-effort append-only JSONL audit. A broken audit file must not turn a correct read into a
/// protocol failure, but the failure is visible on stderr and the server keeps serving.
struct AuditLog {
    path: Option<std::path::PathBuf>,
    writer: Option<BufWriter<std::fs::File>>,
    written: u64,
}

impl AuditLog {
    fn new(path: Option<&Path>) -> Self {
        let Some(path) = path else { return Self { path: None, writer: None, written: 0 } };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("postino mcp: cannot create the audit directory {}: {error}", parent.display());
            return Self { path: None, writer: None, written: 0 };
        }
        let mut log = Self { path: Some(path.to_path_buf()), writer: None, written: 0 };
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() >= MAX_AUDIT_BYTES) {
            log.rotate_file();
        }
        log.open();
        log
    }

    fn open(&mut self) {
        let Some(path) = self.path.as_deref() else { return };
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(file) => {
                self.written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                self.writer = Some(BufWriter::new(file));
            }
            Err(error) => {
                eprintln!("postino mcp: cannot open the audit log {}: {error}", path.display());
                self.writer = None;
            }
        }
    }

    fn rotate_file(&mut self) {
        let Some(path) = self.path.as_deref() else { return };
        let rotated = path.with_extension("jsonl.1");
        if let Err(error) = std::fs::rename(path, &rotated) {
            eprintln!("postino mcp: cannot rotate the audit log {}: {error}", path.display());
        }
    }

    fn rotate(&mut self) {
        self.writer = None;
        self.rotate_file();
        self.written = 0;
        self.open();
    }

    fn record(&mut self, event: &str, name: &str, outcome: &str) {
        if self.writer.is_none() {
            return;
        }
        let name = name.chars().take(512).collect::<String>();
        let line = json!({
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "event": event,
            "name": name,
            "outcome": outcome
        });
        let mut text = line.to_string();
        text.push('\n');
        if self.written.saturating_add(text.len() as u64) > MAX_AUDIT_BYTES {
            self.rotate();
        }
        let Some(writer) = self.writer.as_mut() else { return };
        if writer.write_all(text.as_bytes()).is_err() || writer.flush().is_err() {
            eprintln!("postino mcp: the audit log write failed; the operation result is unchanged");
            self.writer = None;
        } else {
            self.written = self.written.saturating_add(text.len() as u64);
        }
    }
}

/// Read one newline-delimited request with a hard ceiling. An over-long line is consumed (so the
/// next request can still be served) but never stored in full.
fn read_bounded_line(reader: &mut impl BufRead, max: usize) -> std::io::Result<BoundedLine> {
    let mut out = Vec::new();
    let mut too_long = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(if out.is_empty() && !too_long {
                BoundedLine::Eof
            } else if too_long {
                BoundedLine::TooLong
            } else {
                BoundedLine::Line(out)
            });
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(buffer.len());
        if !too_long {
            if out.len().saturating_add(take) > max {
                too_long = true;
                out.clear();
            } else {
                out.extend_from_slice(buffer.get(..take).unwrap_or_default());
            }
        }
        if newline.is_some() {
            reader.consume(take.saturating_add(1));
            return Ok(if too_long { BoundedLine::TooLong } else { BoundedLine::Line(out) });
        }
        reader.consume(take);
    }
}

fn handle_session<B: McpBackend + ?Sized>(backend: &mut B, request: &Value, session: &mut Session) -> Option<Value> {
    let id = request.get("id").cloned();
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return id.map(|id| error_response(id, -32600, "invalid request: missing method"));
    };
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let is_notification = id.is_none();
    // JSON-RPC notifications never receive a response, including the rate-limit and resource-error
    // branches below. Nothing in Postino's MCP surface needs a notification to perform work.
    if is_notification {
        return None;
    }

    let outcome = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list(backend, session.allow_send, session.compact)),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("<missing>").to_owned();
            if !session.rate.allow() {
                session.audit.record("tool", &name, "rate_limited");
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id.unwrap_or(Value::Null),
                    "result": {
                        "content": [{ "type": "text", "text": session.rate.refusal() }],
                        "isError": true
                    }
                }));
            }
            let result = tools_call(backend, &params, session.allow_send);
            let outcome = match &result {
                Ok(value) if value.get("isError").and_then(Value::as_bool) == Some(true) => "error",
                Ok(_) => "ok",
                Err(_) => "error",
            };
            session.audit.record("tool", &name, outcome);
            result
        }
        "resources/list" => Ok(resources_list()),
        "resources/templates/list" => Ok(resource_templates()),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("<missing>").to_owned();
            if !session.rate.allow() {
                session.audit.record("resource", &uri, "rate_limited");
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id.unwrap_or(Value::Null),
                    "error": { "code": -32000, "message": session.rate.refusal() }
                }));
            }
            let result = resources_read(backend, &params);
            let outcome = if result.is_ok() { "ok" } else { "error" };
            session.audit.record("resource", &uri, outcome);
            result
        }
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
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "listChanged": false, "subscribe": false }
        },
        "serverInfo": {
            "name": "postino",
            "title": "Postino",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Postino is a local-first, agent-first mail client. Tools share the same operation registry as the desktop app and the CLI. The mailbox is the configured store when the server was started with one, otherwise it is synthetic demo data. message_send and send_schedule only queue a submission: the message leaves the machine when the Postino daemon processes the outbox (or, without a daemon, the desktop app or `postino send`). This server never drains the queue itself. Use outbox_list to see the journal; outbox_cancel discards a queued or failed row; an uncertain row needs a person. Sending tools are refused unless the server was started with --allow-send."
    })
}

fn tools_list<B: McpBackend + ?Sized>(backend: &B, allow_send: bool, compact: bool) -> Value {
    let tools = backend
        .tools()
        .into_iter()
        // A sensitive operation handles a credential. It is not advertised and cannot be called over
        // MCP; the CLI and the desktop app are the only clients that may pass a password.
        .filter(|def| !def.sensitive)
        .map(|def| {
            let gated = SEND_GATED.contains(&def.name) && !allow_send;
            let description = if gated { format!("{} DISABLED: {SEND_DISABLED_REASON}", def.description) } else { def.description.to_owned() };
            if compact {
                let short = compact_description(&description);
                let mut schema = def.input_schema;
                compact_schema(&mut schema);
                let mut tool = json!({
                    "name": def.name,
                    "description": short,
                    "inputSchema": schema
                });
                if let Some(object) = tool.as_object_mut() {
                    object.insert("enabled".to_owned(), json!(!gated));
                    if gated {
                        object.insert("disabledReason".to_owned(), json!(SEND_DISABLED_REASON));
                    }
                }
                return tool;
            }
            let mut tool = json!({
                "name": def.name,
                "title": def.title,
                "description": description,
                "inputSchema": def.input_schema,
                "annotations": {
                    "readOnlyHint": def.read_only,
                    "destructiveHint": def.destructive,
                    "idempotentHint": def.read_only || matches!(def.name, "message_flag" | "label_apply" | "label_update" | "thread_flag"),
                    "openWorldHint": matches!(def.name, "account_test" | "sync_now" | "outbox_retry" | "rules_run")
                }
            });
            if let Some(object) = tool.as_object_mut() {
                object.insert("enabled".to_owned(), json!(!gated));
                if gated {
                    object.insert("disabledReason".to_owned(), json!(SEND_DISABLED_REASON));
                }
            }
            tool
        })
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

/// Keep the first sentence, or a bounded prefix when the text has no sentence break. The full
/// registry text stays available without `--compact`.
fn compact_description(description: &str) -> String {
    let first = description.split_inclusive(['.', '!', '?']).next().unwrap_or(description).trim();
    let first = if first.is_empty() { description.trim() } else { first };
    if first.chars().count() <= COMPACT_DESCRIPTION_CHARS {
        return first.to_owned();
    }
    let mut short = first.chars().take(COMPACT_DESCRIPTION_CHARS.saturating_sub(1)).collect::<String>();
    short.push('…');
    short
}

/// Strip prose that a schema-aware agent can infer from the registry's compact description. Types,
/// properties, required fields and enums stay untouched, so the schema remains usable.
fn compact_schema(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("description");
            object.remove("title");
            for child in object.values_mut() {
                compact_schema(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                compact_schema(item);
            }
        }
        _ => {}
    }
}

fn resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": "postino://mailbox/status",
                "name": "Mailbox status",
                "description": "Accounts and their last sync status, without credentials.",
                "mimeType": "application/json"
            },
            {
                "uri": "postino://folders",
                "name": "Folders",
                "description": "Folders with unread and total counts.",
                "mimeType": "application/json"
            },
            {
                "uri": "postino://messages/recent",
                "name": "Recent messages",
                "description": "The newest 50 inbox summaries. Bodies are read with the message resource.",
                "mimeType": "application/json"
            }
        ]
    })
}

fn resource_templates() -> Value {
    json!({
        "resourceTemplates": [
            {
                "uriTemplate": "postino://message/{id}",
                "name": "Message",
                "description": "One stored message with its body, recipients and attachment metadata.",
                "mimeType": "application/json"
            },
            {
                "uriTemplate": "postino://message/{id}/text",
                "name": "Message text",
                "description": "The stored plain-text body of one message.",
                "mimeType": "text/plain"
            },
            {
                "uriTemplate": "postino://message/{id}/html",
                "name": "Message HTML",
                "description": "The sanitized HTML body of one message; plain-text mail is wrapped in a preformatted block.",
                "mimeType": "text/html"
            },
            {
                "uriTemplate": "postino://thread/{id}",
                "name": "Thread",
                "description": "One conversation with an ascending page of message summaries.",
                "mimeType": "application/json"
            }
        ]
    })
}

fn resources_read<B: McpBackend + ?Sized>(backend: &mut B, params: &Value) -> Result<Value, (i64, String)> {
    let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| (-32602, "resources/read needs a string `uri`".to_owned()))?;
    match uri {
        "postino://mailbox/status" => {
            let accounts = resource_call(backend, "accounts_list", json!({}), uri)?;
            let status = resource_call(backend, "sync_status", json!({}), uri)?;
            json_resource(uri, json!({ "accounts": accounts, "sync": status }))
        }
        "postino://folders" => {
            let folders = resource_call(backend, "folders_list", json!({}), uri)?;
            json_resource(uri, folders)
        }
        "postino://messages/recent" => {
            // `messages_list` resolves an omitted folder through `FolderKind::Inbox`; real synced
            // inbox ids are `fld_<hash>`, never the demo literal "inbox".
            let messages = resource_call(backend, "messages_list", json!({ "limit": 50 }), uri)?;
            json_resource(uri, messages)
        }
        _ => {
            if let Some(rest) = uri.strip_prefix("postino://message/") {
                let (id, view) = if let Some(id) = rest.strip_suffix("/html") {
                    (id, ResourceView::Html)
                } else if let Some(id) = rest.strip_suffix("/text") {
                    (id, ResourceView::Text)
                } else {
                    (rest, ResourceView::Json)
                };
                if !valid_resource_id(id) {
                    return Err((-32602, format!("invalid message resource id in {uri:?}")));
                }
                let value = resource_call(backend, "message_get", json!({ "id": id }), uri)?;
                return message_resource(uri, &value, view);
            }
            if let Some(id) = uri.strip_prefix("postino://thread/") {
                if !valid_resource_id(id) {
                    return Err((-32602, format!("invalid thread resource id in {uri:?}")));
                }
                let value = resource_call(backend, "thread_get", json!({ "id": id }), uri)?;
                return json_resource(uri, value);
            }
            Err((-32002, format!("resource not found: {uri}")))
        }
    }
}

enum ResourceView {
    Json,
    Text,
    Html,
}

fn resource_call<B: McpBackend + ?Sized>(backend: &mut B, operation: &str, args: Value, uri: &str) -> Result<Value, (i64, String)> {
    backend.call(operation, args).map_err(|error| (-32002, format!("resource {uri:?} is not available: {error}")))
}

fn json_resource(uri: &str, value: Value) -> Result<Value, (i64, String)> {
    let text = serde_json::to_string(&value).map_err(|error| (-32603, format!("serializing resource {uri:?}: {error}")))?;
    Ok(json!({
        "contents": [{ "uri": uri, "mimeType": "application/json", "text": text }]
    }))
}

fn message_resource(uri: &str, value: &Value, view: ResourceView) -> Result<Value, (i64, String)> {
    let message = value.get("message").ok_or_else(|| (-32603, format!("message resource {uri:?} has no message object")))?;
    match view {
        ResourceView::Json => json_resource(uri, value.clone()),
        ResourceView::Text => {
            let text = message.get("body_text").and_then(Value::as_str).unwrap_or_default();
            Ok(json!({
                "contents": [{ "uri": uri, "mimeType": "text/plain", "text": text }]
            }))
        }
        ResourceView::Html => {
            let html = message.get("body_html").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| {
                let text = message.get("body_text").and_then(Value::as_str).unwrap_or_default();
                format!("<pre>{}</pre>", escape_html(text))
            });
            Ok(json!({
                "contents": [{ "uri": uri, "mimeType": "text/html", "text": html }]
            }))
        }
    }
}

fn valid_resource_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn tools_call<B: McpBackend + ?Sized>(backend: &mut B, params: &Value, allow_send: bool) -> Result<Value, (i64, String)> {
    backend.set_allow_send(allow_send);
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| (-32602, "tools/call needs a string `name`".to_owned()))?;
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err((-32602, "tools/call `arguments` must be a JSON object".to_owned()));
    }
    if backend.tools().into_iter().any(|def| def.name == name && def.sensitive) {
        return Ok(sensitive_refusal(name));
    }
    // A signing passphrase is never accepted over MCP, not even with --allow-send. The CLI and the
    // desktop app take it from a no-echo prompt; an agent must not hold it.
    if matches!(name, "message_send" | "send_schedule" | "send_dry_run") && requests_pgp_passphrase(&arguments) {
        return Ok(pgp_passphrase_refusal(name));
    }
    if SEND_GATED.contains(&name) && !allow_send {
        return Ok(send_disabled_refusal(name));
    }
    // A forward action can be created or updated without a network call, but it arms a future one.
    // Refuse the payload before the typed parser sees it: the CLI and the desktop can still create
    // the rule locally.
    if !allow_send && matches!(name, "rules_create" | "rules_update") && requests_rule_forward(&arguments) {
        return Ok(send_disabled_refusal(name));
    }
    if !allow_send && name == "rules_update" {
        match rule_update_enables_forward(backend, &arguments) {
            Ok(true) => return Ok(send_disabled_refusal(name)),
            Ok(false) => {}
            Err(error) => {
                return Ok(json!({
                    "content": [{ "type": "text", "text": format!("operation {name:?} is refused: {error}; start `postino mcp --allow-send` to update it") }],
                    "isError": true
                }));
            }
        }
    }

    match backend.call(name, arguments) {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": value
            }))
        }
        Err(error) => Ok(json!({
            "content": [{ "type": "text", "text": error }],
            "isError": true
        })),
    }
}

/// Scan the raw action list without deserializing the request. A forward action is the only rule
/// payload that can eventually submit mail, and the MCP gate must reject it before parsing.
fn requests_rule_forward(arguments: &Value) -> bool {
    arguments
        .get("actions")
        .and_then(Value::as_array)
        .is_some_and(|actions| actions.iter().any(|action| action.get("kind").and_then(Value::as_str) == Some("forward")))
}

/// An enable-only `rules_update` can arm a stored forward rule without ever sending an `actions`
/// payload. Read the rule before the typed parser sees the request and refuse it without
/// `--allow-send`.
fn rule_update_enables_forward<B: McpBackend + ?Sized>(backend: &B, arguments: &Value) -> std::result::Result<bool, String> {
    if arguments.get("enabled").and_then(Value::as_bool) != Some(true) {
        return Ok(false);
    }
    let Some(id) = arguments.get("id").and_then(Value::as_str) else { return Ok(false) };
    match backend.rule_has_forward(id)? {
        Some(has_forward) => Ok(has_forward),
        // Unknown rule or an unreadable list: fail closed. The update is refused rather than
        // arming a forward we could not inspect.
        None => Err(format!("cannot verify whether rule {id:?} has a forward action")),
    }
}

fn send_disabled_refusal(name: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": format!("operation {name:?} is disabled: {SEND_DISABLED_REASON}") }],
        "isError": true
    })
}

/// True when a send request carries a non-empty PGP signing passphrase.
fn requests_pgp_passphrase(arguments: &Value) -> bool {
    arguments
        .get("pgp")
        .and_then(Value::as_object)
        .and_then(|pgp| pgp.get("passphrase"))
        .and_then(Value::as_str)
        .is_some_and(|passphrase| !passphrase.is_empty())
}

fn pgp_passphrase_refusal(name: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": format!("operation {name:?} is refused: a PGP passphrase is never accepted over MCP; sign from the CLI or the desktop app with a no-echo prompt")
        }],
        "isError": true
    })
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

    fn raw_handle<B: McpBackend + ?Sized>(backend: &mut B, request: &Value, allow_send: bool) -> Option<Value> {
        let mut session = Session::new(McpOptions { allow_send, ..McpOptions::default() });
        handle_session(backend, request, &mut session)
    }

    /// Existing protocol tests exercise the normal, send-enabled server.
    fn handle(engine: &mut Engine, request: &Value) -> Option<Value> {
        raw_handle(engine, request, true)
    }

    fn request(engine: &mut Engine, method: &str) -> Value {
        handle(engine, &json!({ "jsonrpc": "2.0", "id": 1, "method": method })).expect("response")
    }

    #[test]
    fn the_request_line_is_bounded_and_an_oversized_line_is_drained() {
        let mut input = std::io::Cursor::new(b"12345678901234567\nshort\n".to_vec());
        assert!(matches!(read_bounded_line(&mut input, 16), Ok(BoundedLine::TooLong)));
        match read_bounded_line(&mut input, 16).expect("second line") {
            BoundedLine::Line(line) => assert_eq!(line, b"short"),
            _ => panic!("the line after an oversized one must still be readable"),
        }
        assert!(matches!(read_bounded_line(&mut input, 16).expect("eof"), BoundedLine::Eof));
    }

    #[test]
    fn tools_list_contains_the_registry() {
        let mut engine = Engine::demo();
        let response = request(&mut engine, "tools/list");
        let tools = response["result"]["tools"].as_array().expect("tools");
        assert!(tools.iter().any(|tool| tool["name"] == "messages_list"));
        for name in ["search", "search_status", "search_reindex"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap_or_else(|| panic!("{name} is missing"));
            assert!(tool["inputSchema"].is_object());
            assert!(tool["description"].as_str().is_some_and(|description| !description.is_empty()));
        }
        assert!(tools.iter().all(|tool| tool["inputSchema"].is_object()));
        assert!(tools.iter().all(|tool| tool["name"] != "account_add" && tool["name"] != "account_remove"), "sensitive tools are never listed");
        assert!(tools.iter().all(|tool| tool["name"] != "pgp_key_import"), "the passphrase-handling import is never listed");
        assert!(tools.iter().all(|tool| tool["name"] != "message_pgp_decrypt"), "the passphrase-handling decrypt is never listed");
        assert!(tools.iter().any(|tool| tool["name"] == "message_pgp_verify"), "verification takes no secret and stays available");

        for name in ["pgp_keys_list", "pgp_key_delete", "pgp_key_export_public"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap_or_else(|| panic!("{name} is missing"));
            assert!(tool["inputSchema"].is_object());
        }
    }

    #[test]
    fn pgp_import_is_refused_without_echoing_the_key_or_passphrase() {
        let mut engine = Engine::demo();
        let refused = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": { "name": "pgp_key_import", "arguments": {
                    "armored_key": "SENTINEL-ARMORED-KEY",
                    "passphrase": "SENTINEL-PASSPHRASE"
                }}
            }),
        )
        .expect("a response");
        let text = refused.to_string();
        assert_eq!(refused["result"]["isError"], json!(true));
        assert!(!text.contains("SENTINEL"), "a refusal must not repeat the arguments: {text}");
        assert!(text.contains("credential"), "the refusal explains why: {text}");
    }

    #[test]
    fn the_invite_tools_expose_the_registry_schemas_and_annotations() {
        let mut engine = Engine::demo();
        let response = request(&mut engine, "tools/list");
        let tools = response["result"]["tools"].as_array().expect("tools");
        let read = tools.iter().find(|tool| tool["name"] == "message_invite").expect("message_invite is listed");
        assert_eq!(read["annotations"]["readOnlyHint"], json!(true));
        assert_eq!(read["annotations"]["destructiveHint"], json!(false));
        assert_eq!(read["inputSchema"]["required"], json!(["id"]));
        assert!(read["inputSchema"]["properties"]["id"].is_object());

        let write = tools.iter().find(|tool| tool["name"] == "invite_reply_draft").expect("invite_reply_draft is listed");
        assert_eq!(write["annotations"]["readOnlyHint"], json!(false), "it writes a draft");
        assert_eq!(write["annotations"]["destructiveHint"], json!(false));
        assert_eq!(write["inputSchema"]["required"], json!(["message", "response"]));
        assert!(write["description"].as_str().is_some_and(|description| description.contains("organizer")));
        assert!(!tools.iter().any(|tool| tool["name"] == "account_add"), "invite tools are not a credential path");
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
    fn send_tools_are_exposed_and_gated_by_allow_send() {
        let mut engine = Engine::demo();
        let tools = request(&mut engine, "tools/list");
        let definitions = tools["result"]["tools"].as_array().expect("tools");
        for name in ["message_send", "send_schedule", "outbox_list", "outbox_cancel", "outbox_retry", "outbox_mark_sent", "send_dry_run"] {
            let tool = definitions.iter().find(|tool| tool["name"] == name).expect(name);
            assert_eq!(tool["enabled"], json!(true), "{name} is enabled with --allow-send");
        }
        let send = definitions.iter().find(|tool| tool["name"] == "message_send").expect("message_send");
        assert_eq!(send["annotations"]["openWorldHint"], json!(false), "queueing is a local write; the drain is the app or the CLI");
        assert_eq!(send["annotations"]["destructiveHint"], json!(false), "sending is the product's purpose, not a destructive action");

        let disabled = raw_handle(&mut engine, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }), false).expect("response");
        let definitions = disabled["result"]["tools"].as_array().expect("tools");
        let send = definitions.iter().find(|tool| tool["name"] == "message_send").expect("message_send");
        assert_eq!(send["enabled"], json!(false));
        assert!(send["description"].as_str().is_some_and(|text| text.contains("disabled")));
        assert!(send["disabledReason"].as_str().is_some_and(|text| text.contains("--allow-send")));

        let refused = raw_handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "message_send", "arguments": { "draft": "draft_1" } }
            }),
            false,
        )
        .expect("response");
        assert_eq!(refused["result"]["isError"], json!(true));
        assert!(refused["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("--allow-send")));
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
        assert!(!names.contains(&"account_update"), "account_update is sensitive and must not be listed");
        for name in ["accounts_list", "account_test", "sync_now", "sync_status"] {
            assert!(names.contains(&name), "{name} must stay available over MCP");
        }

        for name in ["account_add", "account_remove", "account_update"] {
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
    fn outbox_mark_sent_is_gated_like_sending() {
        let mut engine = Engine::demo();
        let disabled = raw_handle(&mut engine, &json!({ "jsonrpc": "2.0", "id": 40, "method": "tools/list" }), false).expect("response");
        let tools = disabled["result"]["tools"].as_array().expect("tools");
        for name in ["message_send", "send_schedule", "outbox_retry", "outbox_mark_sent"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).expect(name);
            assert_eq!(tool["enabled"], json!(false), "{name} must be gated by --allow-send");
            assert!(tool["disabledReason"].as_str().is_some_and(|text| text.contains("--allow-send")));
        }
        // A row may be cancelled without the send toggle: it never contacts a server.
        let cancel = tools.iter().find(|tool| tool["name"] == "outbox_cancel").expect("outbox_cancel");
        assert_eq!(cancel["enabled"], json!(true));
        let refused = raw_handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 41,
                "method": "tools/call",
                "params": { "name": "outbox_mark_sent", "arguments": { "id": "ob_1", "confirm": true } }
            }),
            false,
        )
        .expect("response");
        assert_eq!(refused["result"]["isError"], json!(true));
    }

    #[test]
    fn outbox_list_never_returns_server_text() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mail.db");
        let mut key_bytes = [0u8; 32];
        getrandom::fill(&mut key_bytes).expect("OS random source");
        let store = postino_store::Store::open(&path, &postino_store::StoreKey::from_bytes(key_bytes)).expect("store");
        let vault = crate::test_support::MemoryVault::new();
        let secret = postino_testkit::SMTP_PASSWORD.to_owned();
        let body = "il corpo che il server SMTP non deve dettare".to_owned();
        let recipient = "echo@example.com";
        let smtp = crate::test_support::EchoingSmtp::start(format!("{secret} {body} {recipient}"));
        crate::test_support::seed_smtp_account(&store, &vault, "acc_mcp", 1, smtp.port());
        for (id, name, kind) in
            [("fld_acc_mcp_drafts", "Bozze", postino_store::FolderKind::Drafts), ("fld_acc_mcp_sent", "Inviati", postino_store::FolderKind::Sent)]
        {
            store
                .upsert_folder(&postino_store::Folder {
                    id: id.to_owned(),
                    account_id: "acc_mcp".to_owned(),
                    name: name.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                })
                .expect("folder");
        }
        let mut engine = Engine::open_with_vault(store, vault).expect("engine");
        engine.set_test_extra_roots(vec![smtp.ca()]);
        let saved = engine
            .call(
                "draft_save",
                json!({
                    "to": [{ "name": "", "email": recipient }],
                    "subject": "Ostile",
                    "body_text": body
                }),
            )
            .expect("draft_save");
        let draft = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
        engine.call("message_send", json!({ "draft": draft })).expect("queue");
        let send_at = engine.outbox_list(None, 500, 0).expect("outbox").entries.first().expect("row").send_at;
        let report = engine.process_outbox(send_at).expect("pass");
        assert_eq!(report.failed, 1, "{report:?}");
        smtp.finish();

        let response = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "tools/call",
                "params": { "name": "outbox_list", "arguments": {} }
            }),
        )
        .expect("response");
        let encoded = serde_json::to_string(&response).expect("JSON");
        assert!(encoded.contains("550"), "the row must still say what happened: {encoded}");
        for forbidden in [secret.as_str(), body.as_str(), recipient] {
            assert!(!encoded.contains(forbidden), "an MCP response contained server text: {encoded}");
        }
    }

    #[test]
    fn attachment_and_signature_operations_are_exposed_without_a_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mail.db");
        let mut key_bytes = [0u8; 32];
        getrandom::fill(&mut key_bytes).expect("OS random source");
        let store = postino_store::Store::open(&path, &postino_store::StoreKey::from_bytes(key_bytes)).expect("store");
        store
            .upsert_account(&postino_store::Account {
                id: "acc_mcp".to_owned(),
                name: "Postino Test".to_owned(),
                email: "test@example.com".to_owned(),
                provider: "custom".to_owned(),
                signature: String::new(),
                created_at: 1,
            })
            .expect("account");
        for (suffix, name, kind) in [("drafts", "Bozze", postino_store::FolderKind::Drafts), ("sent", "Inviati", postino_store::FolderKind::Sent)] {
            store
                .upsert_folder(&postino_store::Folder {
                    id: format!("fld_acc_mcp_{suffix}"),
                    account_id: "acc_mcp".to_owned(),
                    name: name.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                })
                .expect("folder");
        }
        let mut engine = Engine::open(store).expect("engine");

        let tools = request(&mut engine, "tools/list");
        let definitions = tools["result"]["tools"].as_array().expect("tools");
        let add = definitions.iter().find(|tool| tool["name"] == "draft_attachment_add").expect("attachment add");
        assert_eq!(add["annotations"]["readOnlyHint"], json!(false));
        assert_eq!(add["annotations"]["destructiveHint"], json!(false));
        let properties = add["inputSchema"]["properties"].as_object().expect("properties");
        assert!(properties.contains_key("data_base64"), "{properties:?}");
        assert!(!properties.contains_key("path"), "MCP must never read a local path");
        assert!(!definitions.iter().any(|tool| tool["name"] == "account_add"), "sensitive ops stay absent");

        let saved = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 50, "method": "tools/call",
                "params": { "name": "draft_save", "arguments": {
                    "account": "acc_mcp",
                    "to": [{ "name": "Giulia", "email": "giulia@example.com" }],
                    "subject": "Da MCP",
                    "body_text": "Corpo"
                }}
            }),
        )
        .expect("response");
        let draft = saved["result"]["structuredContent"]["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();

        let added = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 51, "method": "tools/call",
                "params": { "name": "draft_attachment_add", "arguments": {
                    "draft": draft,
                    "filename": "nota.txt",
                    "data_base64": "Y2lhbyBkYSBNQ1A="
                }}
            }),
        )
        .expect("response");
        assert!(added.get("error").is_none(), "{added}");
        assert_eq!(added["result"]["structuredContent"]["draft"]["attachments"][0]["filename"], "nota.txt");
        assert_eq!(added["result"]["structuredContent"]["draft"]["attachments"][0]["size"], 11);

        let removed = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 52, "method": "tools/call",
                "params": { "name": "draft_attachment_remove", "arguments": { "draft": draft, "attachment": "nota.txt" }}
            }),
        )
        .expect("response");
        assert_eq!(removed["result"]["structuredContent"]["draft"]["attachments"].as_array().map(Vec::len), Some(0));

        let signature = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 53, "method": "tools/call",
                "params": { "name": "account_signature_set", "arguments": { "account": "acc_mcp", "signature": "Postino\nMCP" }}
            }),
        )
        .expect("response");
        assert_eq!(signature["result"]["structuredContent"]["account"]["account"]["signature"], "Postino\nMCP");
    }

    #[test]
    fn the_attachment_payload_is_bounded_before_decode_over_mcp() {
        let mut engine = Engine::demo();
        let response = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 60, "method": "tools/call",
                "params": { "name": "draft_attachment_add", "arguments": {
                    "draft": "draft_1", "filename": "x.bin", "data_base64": "not base64"
                }}
            }),
        )
        .expect("response");
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("base64") || text.contains("no local store") || text.contains("demo"), "{text}");
    }

    #[test]
    fn threads_and_saved_search_tools_are_exposed_and_round_trip_through_mcp() {
        let mut engine = Engine::demo();
        let tools = request(&mut engine, "tools/list");
        let definitions = tools["result"]["tools"].as_array().expect("tools");
        for name in [
            "threads_list",
            "thread_get",
            "thread_flag",
            "saved_searches_list",
            "saved_search_create",
            "saved_search_update",
            "saved_search_delete",
            "saved_search_recent_clear",
        ] {
            let tool = definitions.iter().find(|tool| tool["name"] == name).unwrap_or_else(|| panic!("{name} is missing"));
            assert!(tool["inputSchema"].is_object());
            assert!(tool["annotations"]["readOnlyHint"].is_boolean());
        }
        let flag = definitions.iter().find(|tool| tool["name"] == "thread_flag").expect("thread_flag");
        assert_eq!(flag["annotations"]["idempotentHint"], json!(true));
        assert!(definitions.iter().all(|tool| tool["name"] != "account_add"), "sensitive tools stay absent");

        let listed = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 70, "method": "tools/call",
                "params": { "name": "threads_list", "arguments": { "folder": "inbox" } }
            }),
        )
        .expect("response");
        let threads = listed["result"]["structuredContent"]["threads"].as_array().expect("threads");
        assert!(threads.iter().any(|thread| thread["id"] == "thr_preventivo"), "{listed}");

        let detail = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 71, "method": "tools/call",
                "params": { "name": "thread_get", "arguments": { "id": "thr_preventivo" } }
            }),
        )
        .expect("response");
        assert_eq!(detail["result"]["structuredContent"]["messages"].as_array().map(Vec::len), Some(3));

        let flag = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 72, "method": "tools/call",
                "params": { "name": "thread_flag", "arguments": { "id": "thr_preventivo", "unread": false } }
            }),
        )
        .expect("response");
        assert_eq!(flag["result"]["structuredContent"]["changed"], 2);
        let again = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 73, "method": "tools/call",
                "params": { "name": "thread_flag", "arguments": { "id": "thr_preventivo", "unread": false } }
            }),
        )
        .expect("response");
        assert_eq!(again["result"]["structuredContent"]["changed"], 0);

        let created = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 74, "method": "tools/call",
                "params": { "name": "saved_search_create", "arguments": { "name": "Fatture", "query": "label:Fatture" } }
            }),
        )
        .expect("response");
        assert!(created.get("error").is_none(), "{created}");
        let id = created["result"]["structuredContent"]["search"]["id"].as_str().expect("id").to_owned();
        let saved = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 75, "method": "tools/call",
                "params": { "name": "saved_searches_list", "arguments": {} }
            }),
        )
        .expect("response");
        assert_eq!(saved["result"]["structuredContent"]["searches"][0]["name"], "Fatture");
        let deleted = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 76, "method": "tools/call",
                "params": { "name": "saved_search_delete", "arguments": { "id": id } }
            }),
        )
        .expect("response");
        assert_eq!(deleted["result"]["structuredContent"]["search"]["name"], "Fatture");
        let cleared = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 77, "method": "tools/call",
                "params": { "name": "saved_search_recent_clear", "arguments": {} }
            }),
        )
        .expect("response");
        assert!(cleared.get("error").is_none(), "{cleared}");

        let stale = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 78, "method": "tools/call",
                "params": { "name": "saved_search_create", "arguments": { "name": "Manca", "query": "label:Inesistente" } }
            }),
        )
        .expect("response");
        assert_eq!(stale["result"]["isError"], json!(true));
        assert!(stale["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("unknown label")));
    }

    #[test]
    fn rule_tools_are_exposed_and_forward_is_gated_before_parsing() {
        let mut engine = Engine::demo();
        let tools = request(&mut engine, "tools/list");
        let definitions = tools["result"]["tools"].as_array().expect("tools");
        for name in ["rules_list", "rules_create", "rules_update", "rules_delete", "rules_preview", "rules_run", "rules_undo", "rules_runs"] {
            let tool = definitions.iter().find(|tool| tool["name"] == name).unwrap_or_else(|| panic!("{name} is missing"));
            assert!(tool["inputSchema"].is_object(), "{name} has no schema");
            assert!(tool["annotations"]["readOnlyHint"].is_boolean(), "{name} has no annotation");
        }
        assert_eq!(definitions.iter().find(|tool| tool["name"] == "rules_preview").expect("preview")["annotations"]["readOnlyHint"], true);
        assert_eq!(definitions.iter().find(|tool| tool["name"] == "rules_run").expect("run")["annotations"]["destructiveHint"], true);
        assert_eq!(definitions.iter().find(|tool| tool["name"] == "rules_run").expect("run")["annotations"]["openWorldHint"], true);

        let disabled = raw_handle(&mut engine, &json!({ "jsonrpc": "2.0", "id": 80, "method": "tools/list" }), false).expect("response");
        let definitions = disabled["result"]["tools"].as_array().expect("tools");
        let run = definitions.iter().find(|tool| tool["name"] == "rules_run").expect("rules_run");
        assert_eq!(run["enabled"], false);
        assert!(run["disabledReason"].as_str().is_some_and(|text| text.contains("--allow-send")));

        // The forward action is refused before the typed request is parsed: this payload has no
        // name, so a parser error would prove the gate ran too late.
        let refused = raw_handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 81, "method": "tools/call",
                "params": { "name": "rules_create", "arguments": {
                    "actions": [{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]
                }}
            }),
            false,
        )
        .expect("response");
        assert_eq!(refused["result"]["isError"], true);
        assert!(refused["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("--allow-send")), "{refused}");

        let refused_run = raw_handle(
            &mut engine,
            &json!({ "jsonrpc": "2.0", "id": 82, "method": "tools/call", "params": { "name": "rules_run", "arguments": { "bogus": true } } }),
            false,
        )
        .expect("response");
        assert_eq!(refused_run["result"]["isError"], true);
        assert!(refused_run["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("--allow-send")), "{refused_run}");

        // With --allow-send the same rule is accepted locally, and a non-forward rule is always
        // available over MCP.
        let created = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 83, "method": "tools/call",
                "params": { "name": "rules_create", "arguments": {
                    "name": "Inoltro",
                    "query": "subject:prova",
                    "actions": [{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]
                }}
            }),
        )
        .expect("response");
        assert!(created.get("error").is_none(), "{created}");
        assert_eq!(created["result"]["structuredContent"]["rule"]["actions"][0]["kind"], "forward");
    }

    #[test]
    fn enabling_a_stored_forward_rule_is_refused_before_parsing_without_allow_send() {
        let mut engine = Engine::demo();
        let created = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 90, "method": "tools/call",
                "params": { "name": "rules_create", "arguments": {
                    "name": "Inoltro",
                    "query": "subject:prova",
                    "actions": [{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]
                }}
            }),
        )
        .expect("response");
        let id = created["result"]["structuredContent"]["rule"]["id"].as_str().expect("rule id").to_owned();
        handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 91, "method": "tools/call",
                "params": { "name": "rules_update", "arguments": { "id": id, "enabled": false } }
            }),
        )
        .expect("disable");
        let refused = raw_handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 92, "method": "tools/call",
                "params": { "name": "rules_update", "arguments": { "id": id, "enabled": true, "bogus": true } }
            }),
            false,
        )
        .expect("response");
        assert_eq!(refused["result"]["isError"], true);
        let text = refused["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("--allow-send"), "the refusal ran before the typed parser: {text}");
        assert!(!text.contains("bogus"), "the refusal must not parse or echo the payload: {text}");
    }

    #[test]
    fn sync_now_does_not_forward_without_allow_send_and_does_with_it() {
        fn engine_with_forward_rule(allow_send: bool) -> (Engine, crate::test_support::RunningServer, tempfile::TempDir) {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join("mail.db");
            let mut key_bytes = [0u8; 32];
            getrandom::fill(&mut key_bytes).expect("OS random source");
            let store = postino_store::Store::open(&path, &postino_store::StoreKey::from_bytes(key_bytes)).expect("store");
            let vault = crate::test_support::MemoryVault::new();
            let server = crate::test_support::RunningServer::start(crate::test_support::three_folders(), 1);
            crate::test_support::seed_account(&store, &vault, "acc_mcp", server.port());
            let mut account_server = store.get_account_server("acc_mcp").expect("server read").expect("server row");
            account_server.smtp = Some(postino_store::SmtpServer {
                host: "localhost".to_owned(),
                port: 465,
                tls: postino_store::AccountTls::Implicit,
                username: "user@example.com".to_owned(),
            });
            store.upsert_account_server(&account_server).expect("SMTP endpoint");
            let mut engine = Engine::open_with_vault(store, vault).expect("engine");
            engine.set_test_extra_roots(vec![server.ca()]);
            let created = handle(
                &mut engine,
                &json!({
                    "jsonrpc": "2.0", "id": 93, "method": "tools/call",
                    "params": { "name": "rules_create", "arguments": {
                        "name": "Inoltro",
                        "query": "from:synthetic",
                        "actions": [{ "kind": "forward", "to": [{ "name": "", "email": "dest@example.com" }] }]
                    }}
                }),
            )
            .expect("create");
            assert!(created.get("error").is_none(), "{created}");
            engine.set_allow_send(allow_send);
            (engine, server, dir)
        }

        for (allow_send, expected) in [(false, 0usize), (true, 2usize)] {
            let (mut engine, server, _dir) = engine_with_forward_rule(allow_send);
            let response = raw_handle(
                &mut engine,
                &json!({
                    "jsonrpc": "2.0", "id": 94, "method": "tools/call",
                    "params": { "name": "sync_now", "arguments": {} }
                }),
                allow_send,
            )
            .expect("response");
            assert!(response.get("error").is_none(), "{response}");
            server.finish();
            let outbox = engine.outbox_list(None, 500, 0).expect("outbox");
            assert_eq!(outbox.entries.len(), expected, "allow_send={allow_send}: {response}");
        }
    }

    #[test]
    fn initialize_does_not_claim_a_demo_mailbox() {
        let mut engine = Engine::demo();
        let response = request(&mut engine, "initialize");
        let instructions = response["result"]["instructions"].as_str().expect("instructions");
        assert!(!instructions.contains("M0 demo mode"), "{instructions}");
        assert!(instructions.contains("queue"), "the queue-only contract must be stated: {instructions}");
        assert!(instructions.contains("--allow-send"), "{instructions}");
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

    #[test]
    fn a_failed_rule_inspection_fails_closed_when_enabling_a_forward_rule() {
        struct Failing;
        impl McpBackend for Failing {
            fn tools(&self) -> Vec<postino_core::OpDef> {
                postino_core::op_defs()
            }
            fn call(&mut self, _name: &str, _args: Value) -> std::result::Result<Value, String> {
                panic!("the update reached the backend");
            }
            fn rule_has_forward(&self, _id: &str) -> std::result::Result<Option<bool>, String> {
                Err("rules_list is unavailable".to_owned())
            }
            fn set_allow_send(&mut self, _allow_send: bool) {}
        }
        let mut backend = Failing;
        let response = raw_handle(
            &mut backend,
            &json!({
                "jsonrpc": "2.0",
                "id": 30,
                "method": "tools/call",
                "params": { "name": "rules_update", "arguments": { "id": "rule_1", "enabled": true } }
            }),
            false,
        )
        .expect("response");
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(response["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("refused")), "{response}");
    }

    #[test]
    fn compact_tools_keep_the_schema_but_drop_prose_and_annotations() {
        let mut engine = Engine::demo();
        let full = raw_handle(&mut engine, &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }), true).expect("full");
        let mut session = Session::new(McpOptions { allow_send: true, compact: true, ..McpOptions::default() });
        let compact = handle_session(&mut engine, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }), &mut session).expect("compact");
        let full_tools = full["result"]["tools"].as_array().expect("full tools");
        let compact_tools = compact["result"]["tools"].as_array().expect("compact tools");
        assert_eq!(full_tools.len(), compact_tools.len());
        assert!(compact_tools.iter().all(|tool| tool.get("title").is_none() && tool.get("annotations").is_none()));
        let messages = compact_tools.iter().find(|tool| tool["name"] == "messages_list").expect("messages_list");
        assert!(messages["inputSchema"]["properties"]["folder"].is_object(), "field schemas stay available");
        assert!(messages["inputSchema"]["properties"].get("description").is_none());
        let compact_json = serde_json::to_string(&compact).expect("JSON");
        let full_json = serde_json::to_string(&full).expect("JSON");
        assert!(compact_json.len() < full_json.len(), "compact mode must reduce the payload");
        assert!(!compact_json.contains("\"description\":\"Folder id"), "nested schema prose is removed");
    }

    #[test]
    fn resources_expose_a_message_thread_and_mailbox_metadata() {
        let mut engine = Engine::demo();
        let list = request(&mut engine, "resources/list");
        assert!(
            list["result"]["resources"]
                .as_array()
                .is_some_and(|resources| resources.iter().any(|resource| resource["uri"] == "postino://mailbox/status"))
        );
        let templates = request(&mut engine, "resources/templates/list");
        let templates = templates["result"]["resourceTemplates"].as_array().expect("templates");
        assert!(templates.iter().any(|template| template["uriTemplate"] == "postino://message/{id}/html"));
        assert!(templates.iter().any(|template| template["uriTemplate"] == "postino://thread/{id}"));

        let message = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "resources/read",
                "params": { "uri": "postino://message/msg_001" }
            }),
        )
        .expect("message resource");
        assert_eq!(message["result"]["contents"][0]["mimeType"], "application/json");
        assert!(message["result"]["contents"][0]["text"].as_str().is_some_and(|text| text.contains("msg_001")));

        let text = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "resources/read",
                "params": { "uri": "postino://message/msg_001/text" }
            }),
        )
        .expect("text resource");
        assert_eq!(text["result"]["contents"][0]["mimeType"], "text/plain");

        let threads = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "threads_list", "arguments": {} }
            }),
        )
        .expect("threads");
        let thread_id = threads["result"]["structuredContent"]["threads"][0]["id"].as_str().expect("thread id");
        let thread = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 5, "method": "resources/read",
                "params": { "uri": format!("postino://thread/{thread_id}") }
            }),
        )
        .expect("thread resource");
        assert!(thread["result"]["contents"][0]["text"].as_str().is_some_and(|text| text.contains(thread_id)));

        let invalid = handle(
            &mut engine,
            &json!({
                "jsonrpc": "2.0", "id": 6, "method": "resources/read",
                "params": { "uri": "postino://message/../secret" }
            }),
        )
        .expect("invalid resource");
        assert_eq!(invalid["error"]["code"], -32602);
    }

    #[test]
    fn a_notification_never_receives_a_rate_limit_response_and_the_recent_resource_omits_a_folder() {
        struct Capturing {
            engine: Engine,
            calls: Vec<(String, Value)>,
        }
        impl McpBackend for Capturing {
            fn tools(&self) -> Vec<postino_core::OpDef> {
                postino_core::op_defs()
            }
            fn call(&mut self, name: &str, args: Value) -> std::result::Result<Value, String> {
                self.calls.push((name.to_owned(), args.clone()));
                self.engine.call(name, args).map_err(|error| error.to_string())
            }
            fn rule_has_forward(&self, _id: &str) -> std::result::Result<Option<bool>, String> {
                Ok(None)
            }
            fn set_allow_send(&mut self, allow_send: bool) {
                self.engine.set_allow_send(allow_send);
            }
        }
        let mut backend = Capturing { engine: Engine::demo(), calls: Vec::new() };
        let response = raw_handle(
            &mut backend,
            &json!({
                "jsonrpc": "2.0", "method": "tools/call",
                "params": { "name": "message_get", "arguments": { "id": "msg_001" } }
            }),
            false,
        );
        assert!(response.is_none(), "a notification must not receive a JSON-RPC response");
        assert!(backend.calls.is_empty(), "a notification must not perform work");

        let recent = raw_handle(
            &mut backend,
            &json!({ "jsonrpc": "2.0", "id": 41, "method": "resources/read", "params": { "uri": "postino://messages/recent" } }),
            false,
        )
        .expect("recent resource");
        assert!(recent["result"]["contents"][0]["text"].as_str().is_some_and(|text| text.contains("msg_")));
        let (name, args) = backend.calls.iter().find(|(name, _)| name == "messages_list").expect("messages_list call");
        assert_eq!(name, "messages_list");
        assert!(args.get("folder").is_none(), "the recent resource must use the inbox kind, not a literal id: {args}");
        assert_eq!(args["limit"], 50);
    }

    #[test]
    fn the_preview_and_audit_sheets_rotate_the_log_at_the_documented_boundary() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("audit.jsonl");
        std::fs::write(&path, vec![b'x'; MAX_AUDIT_BYTES as usize]).expect("full log");
        let mut log = AuditLog::new(Some(&path));
        log.record("tool", "messages_list", "ok");
        assert!(path.with_extension("jsonl.1").exists(), "the old log must be rotated");
        let current = std::fs::read_to_string(&path).expect("new log");
        assert!(current.contains("messages_list"), "{current}");
        assert!(current.len() < MAX_AUDIT_BYTES as usize);
    }

    #[test]
    fn audit_log_records_names_and_outcomes_without_arguments_and_the_rate_limit_stops_a_loop() {
        let directory = tempfile::tempdir().expect("tempdir");
        let audit_path = directory.path().join("audit.jsonl");
        let mut session = Session::new(McpOptions { allow_send: true, audit: Some(audit_path.clone()), ..McpOptions::default() });
        session.rate = RateLimiter::new(1, Duration::from_secs(60));
        let mut engine = Engine::demo();
        let call = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "message_get", "arguments": { "id": "SENTINEL-ARGUMENT" } }
        });
        let first = handle_session(&mut engine, &call, &mut session).expect("first");
        assert_eq!(first["result"]["isError"], true, "the missing id is a normal tool error");
        let second = handle_session(&mut engine, &call, &mut session).expect("second");
        assert_eq!(second["result"]["isError"], true);
        assert!(second["result"]["content"][0]["text"].as_str().is_some_and(|text| text.contains("rate limit")));
        let log = std::fs::read_to_string(&audit_path).expect("audit log");
        assert!(log.contains("\"name\":\"message_get\""), "{log}");
        assert!(log.contains("\"outcome\":\"error\""), "{log}");
        assert!(log.contains("\"outcome\":\"rate_limited\""), "{log}");
        assert!(!log.contains("SENTINEL-ARGUMENT"), "arguments never enter the audit log: {log}");
        assert_eq!(log.lines().count(), 2);
    }
}
