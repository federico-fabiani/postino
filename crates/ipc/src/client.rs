//! The client half. The CLI and MCP server use [`BlockingClient`]; the daemon's tests use [`Client`].

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::time::timeout;

use crate::endpoint::DaemonPaths;
use crate::frame::{FrameReader, MAX_FRAME_BYTES, write_frame};
use crate::protocol::{HelloParams, HelloResult, PROTOCOL_VERSION, RpcError, error_code, method, request_key};
use crate::token::TokenFile;
use crate::transport::{Stream, connect};
use crate::{IpcError, Result};

/// An async client with one request in flight at a time. Events that arrive between responses are
/// kept in a bounded local queue; a caller that never drains them sees the oldest ones only.
pub struct Client {
    reader: FrameReader<ReadHalf<Box<dyn Stream>>>,
    writer: WriteHalf<Box<dyn Stream>>,
    client_name: String,
    next_id: u64,
    protocol: u32,
    hello: HelloResult,
    pending_events: VecDeque<Value>,
    pending_responses: VecDeque<Value>,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("client_name", &self.client_name)
            .field("protocol", &self.protocol)
            .field("pid", &self.hello.pid)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Read the owner-only token, connect, and perform the handshake. A stale token, a missing
    /// daemon and a version mismatch are distinct readable failures.
    pub async fn connect(paths: &DaemonPaths, client_name: &str, connect_timeout: Duration, request_timeout: Duration) -> Result<Self> {
        Self::connect_with(paths, client_name, true, connect_timeout, request_timeout).await
    }

    /// Connect with an explicit send permission on the handshake. The MCP process passes its
    /// `--allow-send` flag so the daemon can suppress rule forwards during `sync_now`.
    pub async fn connect_with(
        paths: &DaemonPaths,
        client_name: &str,
        allow_send: bool,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        let token = TokenFile::new(paths.token_file.clone()).read()?;
        let stream = connect(&paths.endpoint, connect_timeout).await?;
        let (reader, writer) = tokio::io::split(stream);
        let hello_params = serde_json::to_value(HelloParams {
            protocol: PROTOCOL_VERSION,
            client: client_name.to_owned(),
            token: token.as_str().to_owned(),
            allow_send,
        })?;
        let mut client = Self {
            reader: FrameReader::new(reader, MAX_FRAME_BYTES),
            writer,
            client_name: client_name.to_owned(),
            next_id: 1,
            protocol: PROTOCOL_VERSION,
            hello: HelloResult {
                protocol: PROTOCOL_VERSION,
                daemon_version: String::new(),
                pid: 0,
                started_at: 0,
                store_root: String::new(),
                endpoint_kind: String::new(),
            },
            pending_events: VecDeque::new(),
            pending_responses: VecDeque::new(),
        };
        let id = client.next_id();
        client.send(id, method::HELLO, hello_params).await?;
        let value = client.read_until(id, request_timeout).await?;
        let hello: HelloResult = serde_json::from_value(value)?;
        client.hello = hello;
        Ok(client)
    }

    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    async fn send(&mut self, id: u64, method_name: &str, params: Value) -> Result<()> {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method_name, "params": params });
        write_frame(&mut self.writer, &request, MAX_FRAME_BYTES).await
    }

    /// Start a request and return its numeric id. The caller must consume the response with
    /// [`Client::wait_for`]; this is how a test observes an in-flight request and cancels it from a
    /// second connection.
    pub async fn start_request(&mut self, method_name: &str, params: Value) -> Result<u64> {
        let id = self.next_id();
        self.send(id, method_name, params).await?;
        Ok(id)
    }

    /// A test seam for duplicate-id and cancellation tests.
    #[doc(hidden)]
    pub async fn start_request_with_id(&mut self, id: u64, method_name: &str, params: Value) -> Result<u64> {
        self.next_id = self.next_id.max(id.saturating_add(1));
        self.send(id, method_name, params).await?;
        Ok(id)
    }

    pub async fn wait_for(&mut self, id: u64, request_timeout: Duration) -> Result<Value> {
        self.read_until(id, request_timeout).await
    }

    pub async fn call(&mut self, method_name: &str, params: Value, request_timeout: Duration) -> Result<Value> {
        let id = self.next_id();
        self.send(id, method_name, params).await?;
        self.read_until(id, request_timeout).await
    }

    pub async fn ping(&mut self, request_timeout: Duration) -> Result<Value> {
        self.call(method::PING, json!({}), request_timeout).await
    }

    /// Subscribe to the daemon's progress stream. Events are delivered by [`Client::next_event`].
    pub async fn subscribe(&mut self, request_timeout: Duration) -> Result<String> {
        let value = self.call(method::EVENTS_SUBSCRIBE, json!({}), request_timeout).await?;
        value
            .get("subscription")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| IpcError::Rpc(RpcError::new(error_code::INTERNAL, "events_subscribe returned no subscription id")))
    }

    /// Wait for the next event notification. Responses that do not match a current request are kept
    /// in a small queue so an unsolicited cancellation response is not lost.
    pub async fn next_event(&mut self, wait: Duration) -> Result<Option<Value>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let frame = match timeout(remaining, self.reader.next_frame()).await {
                Ok(Ok(Some(frame))) => frame,
                Ok(Ok(None)) => return Err(IpcError::Closed),
                Ok(Err(error)) => return Err(error),
                Err(_) => return Ok(None),
            };
            let value: Value = serde_json::from_slice(&frame)?;
            if value.get("method").and_then(Value::as_str) == Some(method::EVENT) {
                return Ok(value.get("params").and_then(|params| params.get("event")).cloned());
            }
            if value.get("id").is_some() {
                self.pending_responses.push_back(value);
            }
        }
    }

    async fn read_until(&mut self, id: u64, request_timeout: Duration) -> Result<Value> {
        let wanted = key_of(id);
        if let Some(position) = self.pending_responses.iter().position(|value| response_key(value).as_deref() == Some(wanted.as_str())) {
            let value = self.pending_responses.remove(position).ok_or(IpcError::Closed)?;
            return unwrap_response(value);
        }
        let deadline = tokio::time::Instant::now() + request_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(IpcError::Timeout("waiting for the daemon"));
            }
            let frame = match timeout(remaining, self.reader.next_frame()).await {
                Ok(Ok(Some(frame))) => frame,
                Ok(Ok(None)) => return Err(IpcError::Closed),
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(IpcError::Timeout("waiting for the daemon")),
            };
            let value: Value = serde_json::from_slice(&frame)?;
            if value.get("method").and_then(Value::as_str) == Some(method::EVENT) {
                if let Some(event) = value.get("params").and_then(|params| params.get("event")).cloned() {
                    if self.pending_events.len() >= crate::events::EVENT_CAPACITY {
                        self.pending_events.pop_front();
                    }
                    self.pending_events.push_back(event);
                }
                continue;
            }
            if value.get("method").and_then(Value::as_str) == Some(method::HEARTBEAT) {
                continue;
            }
            if value.get("error").and_then(|error| error.get("code")).and_then(Value::as_i64) == Some(error_code::OVERLOADED) {
                return Err(IpcError::Overloaded);
            }
            match response_key(&value) {
                Some(key) if key == wanted => return unwrap_response(value),
                Some(_) => {
                    if self.pending_responses.len() >= crate::events::EVENT_CAPACITY {
                        self.pending_responses.pop_front();
                    }
                    self.pending_responses.push_back(value);
                }
                None => {
                    return Err(IpcError::Rpc(RpcError::new(error_code::INVALID_REQUEST, "the daemon sent an invalid frame")));
                }
            }
        }
    }

    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    pub fn protocol(&self) -> u32 {
        self.protocol
    }
}

/// Open the raw byte stream for a protocol-level test. Production callers use [`Client::connect`].
#[doc(hidden)]
pub async fn connect_raw(endpoint: &crate::Endpoint, connect_timeout: Duration) -> Result<Box<dyn Stream>> {
    connect(endpoint, connect_timeout).await
}

/// A synchronous facade for the CLI and MCP server. It owns a current-thread runtime and never
/// spawns a background thread of its own.
pub struct BlockingClient {
    runtime: tokio::runtime::Runtime,
    client: Client,
}

impl BlockingClient {
    pub fn connect(paths: &DaemonPaths, client_name: &str, connect_timeout: Duration, request_timeout: Duration) -> Result<Self> {
        Self::connect_with(paths, client_name, true, connect_timeout, request_timeout)
    }

    pub fn connect_with(
        paths: &DaemonPaths,
        client_name: &str,
        allow_send: bool,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let client = runtime.block_on(Client::connect_with(paths, client_name, allow_send, connect_timeout, request_timeout))?;
        Ok(Self { runtime, client })
    }

    pub fn hello(&self) -> &HelloResult {
        self.client.hello()
    }

    pub fn call(&mut self, method_name: &str, params: Value, request_timeout: Duration) -> Result<Value> {
        self.runtime.block_on(self.client.call(method_name, params, request_timeout))
    }

    pub fn ping(&mut self, request_timeout: Duration) -> Result<Value> {
        self.runtime.block_on(self.client.ping(request_timeout))
    }

    pub fn subscribe(&mut self, request_timeout: Duration) -> Result<String> {
        self.runtime.block_on(self.client.subscribe(request_timeout))
    }

    pub fn next_event(&mut self, wait: Duration) -> Result<Option<Value>> {
        self.runtime.block_on(self.client.next_event(wait))
    }

    pub fn client_name(&self) -> &str {
        self.client.client_name()
    }
}

fn key_of(id: u64) -> String {
    id.to_string()
}

fn response_key(value: &Value) -> Option<String> {
    value.get("id").map(request_key)
}

fn unwrap_response(value: Value) -> Result<Value> {
    if let Some(error) = value.get("error") {
        let error: RpcError =
            serde_json::from_value(error.clone()).unwrap_or_else(|_| RpcError::new(error_code::INTERNAL, "the daemon returned an unreadable error"));
        return Err(map_rpc_error(error));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| IpcError::Rpc(RpcError::new(error_code::INVALID_REQUEST, "the daemon returned neither result nor error")))
}

fn map_rpc_error(error: RpcError) -> IpcError {
    match error.code {
        error_code::UNAUTHORIZED => IpcError::Unauthorized,
        error_code::BUSY => IpcError::Busy(error.message),
        error_code::CANCELLED => IpcError::Cancelled,
        error_code::OVERLOADED => IpcError::Overloaded,
        error_code::VERSION => {
            let daemon = error
                .data
                .as_ref()
                .and_then(|data| data.get("daemon"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            let client = error
                .data
                .as_ref()
                .and_then(|data| data.get("client"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(PROTOCOL_VERSION);
            IpcError::Version { client, daemon }
        }
        _ => IpcError::Rpc(error),
    }
}
