//! The wire protocol: JSON-RPC 2.0 request/response envelopes and the small control methods.
//!
//! The daemon's operation registry is reached through [`method::OP`]: one request whose params are
//! `{"name": "...", "args": {...}}`. The operation names, descriptions and JSON Schemas still come
//! from `postino-core`; this crate never keeps a second list.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Wire version. Bump only for a breaking envelope or handshake change; an operation schema change
/// is compatible because clients read `ops_list`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Control methods handled by the transport itself.
pub mod method {
    pub const HELLO: &str = "hello";
    pub const PING: &str = "ping";
    pub const EVENT: &str = "event";
    /// A server-to-client liveness notification while work is in flight or a subscription is live.
    pub const HEARTBEAT: &str = "heartbeat";
    pub const EVENTS_SUBSCRIBE: &str = "events_subscribe";
    pub const CANCEL: &str = "cancel";
    /// The operation registry. `params` is [`crate::protocol::OpRequest`].
    pub const OP: &str = "op";
    /// The operation metadata: names, titles, descriptions, input schemas and annotations.
    pub const OPS_LIST: &str = "ops_list";
    pub const DAEMON_STATUS: &str = "daemon_status";
    pub const DAEMON_SHUTDOWN: &str = "daemon_shutdown";
}

/// Error codes. They are stable enough for a client to branch on and readable enough to print.
pub mod error_code {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// Bad token, bad peer, or a missing hello. The message is fixed and generic.
    pub const UNAUTHORIZED: i64 = -32001;
    /// A mutation is already in flight for this client.
    pub const BUSY: i64 = -32002;
    /// The frame is larger than [`crate::MAX_FRAME_BYTES`].
    pub const FRAME_TOO_LARGE: i64 = -32003;
    pub const VERSION: i64 = -32004;
    pub const CANCELLED: i64 = -32005;
    /// Too many connections or requests.
    pub const OVERLOADED: i64 = -32006;
    pub const SHUTTING_DOWN: i64 = -32007;
    /// The daemon understood the operation and it failed. `data.kind` names the category.
    pub const OPERATION: i64 = -32010;
}

/// One JSON-RPC 2.0 request. Unknown keys are refused before any handler sees the request.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl RpcRequest {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.jsonrpc != "2.0" {
            return Err(RpcError::invalid_request("jsonrpc must be \"2.0\""));
        }
        if self.method.is_empty() || self.method.len() > 64 {
            return Err(RpcError::invalid_request("method must be 1-64 characters"));
        }
        Ok(())
    }
}

/// The first frame must be a `hello` request with these params.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    pub protocol: u32,
    /// A short label such as `cli`, `mcp`, `app` or `test`.
    pub client: String,
    pub token: String,
    /// The caller's send permission. Defaults to true for the CLI/app; the MCP process sends its
    /// `--allow-send` flag so the daemon can suppress rule forwards even while running a sync.
    #[serde(default = "allow_send_default")]
    pub allow_send: bool,
}

const fn allow_send_default() -> bool {
    true
}

impl HelloParams {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.client.is_empty() || self.client.len() > 64 || !self.client.bytes().all(|byte| byte.is_ascii_graphic() || byte == b' ') {
            return Err(RpcError::invalid_params("client must be 1-64 printable ASCII characters"));
        }
        if self.token.len() > 256 {
            return Err(RpcError::invalid_params("token is too long"));
        }
        Ok(())
    }
}

/// The daemon's answer to a successful handshake. It contains no secret and no path beyond the
/// store root the caller already knows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloResult {
    pub protocol: u32,
    pub daemon_version: String,
    pub pid: u32,
    pub started_at: i64,
    pub store_root: String,
    pub endpoint_kind: String,
}

/// The registry request. One operation per call.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpRequest {
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

impl OpRequest {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.name.is_empty() || self.name.len() > 64 {
            return Err(RpcError::invalid_params("name must be 1-64 characters"));
        }
        if !self.args.is_null() && !self.args.is_object() {
            return Err(RpcError::invalid_params("args must be a JSON object"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub request: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribe {
    /// Optional filter, for example `["sync", "index", "outbox"]`. Unknown kinds are ignored.
    #[serde(default)]
    pub kinds: Vec<String>,
}

/// One progress event. The daemon assigns `seq` monotonically and keeps only a bounded window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventParams {
    pub subscription: String,
    pub event: Value,
}

/// A JSON-RPC error object, the same shape MCP and the CLI already print.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(error_code::INVALID_REQUEST, message)
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(error_code::INVALID_PARAMS, message)
    }

    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self::new(error_code::METHOD_NOT_FOUND, message)
    }

    pub fn unauthorized() -> Self {
        Self::new(error_code::UNAUTHORIZED, "the daemon refused this client")
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(error_code::BUSY, message)
    }

    pub fn cancelled() -> Self {
        Self::new(error_code::CANCELLED, "the request was cancelled")
    }

    pub fn overloaded() -> Self {
        Self::new(error_code::OVERLOADED, "too many clients or requests: try again")
    }

    pub fn operation(message: impl Into<String>, kind: &str) -> Self {
        Self::new(error_code::OPERATION, message).with_data(json!({ "kind": kind }))
    }
}

/// Whether a method is part of the transport contract rather than the operation registry. A client
/// may still ask the daemon for `ops_list` without opening a store.
pub fn is_known_method(name: &str) -> bool {
    matches!(
        name,
        method::HELLO
            | method::PING
            | method::EVENT
            | method::EVENTS_SUBSCRIBE
            | method::CANCEL
            | method::OP
            | method::OPS_LIST
            | method::DAEMON_STATUS
            | method::DAEMON_SHUTDOWN
    )
}

pub const fn supported_protocols() -> &'static [u32] {
    &[PROTOCOL_VERSION]
}

/// Whether a request-shaped value carries an id. Used by the server to tell a request from a
/// notification without trusting serde's `Option<Value>` treatment of `null`.
pub fn has_request_id(value: &Value) -> bool {
    value.get("id").is_some_and(|id| !id.is_null())
}

/// The stable key used by `cancel`: a string id is used verbatim, every other id is its JSON text.
pub fn request_key(id: &Value) -> String {
    id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string())
}
