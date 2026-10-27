//! The local JSON-RPC server: handshake, bounded framing, one connection per client, bounded
//! per-connection request and event queues.
//!
//! Dispatch is delegated to a [`Dispatcher`]. The transport owns the methods that are pure
//! connection state (`hello`, `ping`, `events_subscribe`, `cancel`); the daemon owns `op`,
//! `ops_list`, `daemon_status` and `daemon_shutdown`.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::endpoint::DaemonPaths;
use crate::events::EventHub;
use crate::frame::{FrameReader, MAX_FRAME_BYTES, write_frame};
use crate::protocol::{CancelRequest, EventParams, EventsSubscribe, HelloParams, HelloResult, RpcError, RpcRequest, error_code, method, request_key};
use crate::token::SharedToken;
use crate::transport::{Listener, Stream};
use crate::{Endpoint, IpcError};

/// How many requests one connection may have in flight. The daemon also allows only one mutation per
/// client; this cap protects reads and control frames.
pub const PER_CONNECTION_IN_FLIGHT: usize = 8;
/// How many responses or notifications may wait for one slow socket before events are dropped.
pub const WRITE_QUEUE: usize = 64;
/// How many connections the server accepts at once. Beyond this a client gets a readable overload
/// error and is closed; the daemon never queues an unbounded number of sessions.
pub const MAX_CONNECTIONS: usize = 32;

/// A cooperative cancellation flag that survives clones. It wakes waiters; a waiter that starts
/// after the flag is set still returns immediately.
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Per-connection state the daemon's dispatcher can use to enforce one in-flight mutation.
pub struct Session {
    pub client: String,
    pub connection: u64,
    pub allow_send: bool,
    mutation_in_flight: AtomicBool,
}

impl Session {
    pub fn new(client: String, connection: u64, allow_send: bool) -> Self {
        Self { client, connection, allow_send, mutation_in_flight: AtomicBool::new(false) }
    }

    /// Try to become the one mutation this client may have in flight.
    pub fn begin_mutation(&self) -> bool {
        self.mutation_in_flight.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }

    pub fn end_mutation(&self) {
        self.mutation_in_flight.store(false, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Session").field("client", &self.client).field("connection", &self.connection).finish_non_exhaustive()
    }
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// One request handed to the daemon.
pub struct DispatchRequest {
    pub method: String,
    pub params: Value,
    pub request_id: String,
    pub session: Arc<Session>,
}

/// The daemon side of the protocol.
pub trait Dispatcher: Send + Sync + 'static {
    fn dispatch(&self, request: DispatchRequest) -> BoxFuture<Result<Value, RpcError>>;

    /// Stop a request if it has not finished. Returns whether a live request matched.
    fn cancel(&self, request_id: &str) -> bool {
        let _ = request_id;
        false
    }
}

/// Everything the transport needs to accept clients. The daemon fills the handshake fields.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub endpoint: Endpoint,
    pub token: SharedToken,
    pub protocol: u32,
    pub daemon_version: String,
    pub started_at: i64,
    pub store_root: String,
    pub max_connections: usize,
    /// Total dispatches in flight across every connection. The engine mutex serialises writes; this
    /// cap keeps read requests from filling the blocking pool without bound.
    pub global_in_flight: usize,
    /// At most this many event subscriptions per connection.
    pub max_subscriptions_per_connection: usize,
    /// At most this many event subscriptions across all connections.
    pub max_subscriptions: usize,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
}

impl ServerConfig {
    pub fn new(endpoint: Endpoint, token: SharedToken) -> Self {
        Self {
            endpoint,
            token,
            protocol: crate::protocol::PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at: 0,
            store_root: String::from("."),
            max_connections: MAX_CONNECTIONS,
            global_in_flight: 16,
            max_subscriptions_per_connection: 4,
            max_subscriptions: 64,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(120),
        }
    }
}

pub struct Server {
    config: ServerConfig,
    dispatcher: Arc<dyn Dispatcher>,
    hub: Arc<EventHub>,
    connection_sequence: AtomicU64,
    active_connections: Arc<AtomicUsize>,
    active_subscriptions: Arc<AtomicUsize>,
}

impl Server {
    pub fn new(config: ServerConfig, dispatcher: Arc<dyn Dispatcher>, hub: Arc<EventHub>) -> Self {
        Self {
            config,
            dispatcher,
            hub,
            connection_sequence: AtomicU64::new(0),
            active_connections: Arc::new(AtomicUsize::new(0)),
            active_subscriptions: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Bind the pipe/socket and return a server that is visibly listening. The daemon uses this to
    /// report "ready" only after the endpoint exists.
    pub async fn bind(self) -> Result<BoundServer, IpcError> {
        let listener = Listener::bind(&self.config.endpoint).await?;
        let global = Arc::new(Semaphore::new(self.config.global_in_flight.max(1)));
        Ok(BoundServer {
            config: self.config,
            dispatcher: self.dispatcher,
            hub: self.hub,
            connection_sequence: self.connection_sequence,
            active_connections: self.active_connections,
            listener,
            global,
            active_subscriptions: self.active_subscriptions,
        })
    }

    /// Bind and serve until `shutdown` fires. A bind failure is returned before any client sees a
    /// half-open daemon.
    pub async fn run(self, shutdown: CancellationToken) -> Result<(), IpcError> {
        self.bind().await?.run(shutdown).await
    }

    pub fn paths_for(store: &std::path::Path) -> Result<DaemonPaths, IpcError> {
        DaemonPaths::for_store(store)
    }
}

/// A bound endpoint. Dropping it removes a Unix socket and closes the Windows pipe name.
pub struct BoundServer {
    config: ServerConfig,
    dispatcher: Arc<dyn Dispatcher>,
    hub: Arc<EventHub>,
    connection_sequence: AtomicU64,
    active_connections: Arc<AtomicUsize>,
    listener: Listener,
    global: Arc<Semaphore>,
    active_subscriptions: Arc<AtomicUsize>,
}

impl BoundServer {
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), IpcError> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = self.listener.accept() => {
                    let stream = match accepted {
                        Ok(stream) => stream,
                        Err(error) => {
                            if shutdown.is_cancelled() {
                                break;
                            }
                            // A transient pipe error must not stop the scheduler and scheduled mail.
                            log::warn!("daemon listener accept failed: {error}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let active = self.active_connections.fetch_add(1, Ordering::SeqCst);
                    if active >= self.config.max_connections {
                        self.active_connections.fetch_sub(1, Ordering::SeqCst);
                        reject_overloaded(stream).await;
                        continue;
                    }
                    let guard = ConnectionGuard { active: Arc::clone(&self.active_connections) };
                    let connection = self.connection_sequence.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                    let config = self.config.clone();
                    let dispatcher = Arc::clone(&self.dispatcher);
                    let hub = Arc::clone(&self.hub);
                    let global = Arc::clone(&self.global);
                    let subscriptions = Arc::clone(&self.active_subscriptions);
                    connections.spawn(async move {
                        let _guard = guard;
                        if let Err(error) = handle_connection(stream, connection, config, dispatcher, hub, global, subscriptions).await {
                            log::debug!("daemon connection ended: {error}");
                        }
                    });
                }
            }
            while let Some(finished) = connections.try_join_next() {
                if let Err(error) = finished {
                    log::warn!("daemon connection task ended unexpectedly: {error}");
                }
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }
}

struct ConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn reject_overloaded(mut stream: Box<dyn Stream>) {
    let response = json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": RpcError::overloaded(),
    });
    let _ = write_frame(&mut stream, &response, MAX_FRAME_BYTES).await;
    // Let the client read the refusal before the pipe instance closes; Windows otherwise reports a
    // broken pipe and the caller loses the readable overload.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn error_response(id: Value, error: RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A response that would overflow the frame limit becomes a readable, same-id error. The connection
/// stays open; the caller can retry a narrower operation or use the out-of-band path planned for
/// P10a2. The dispatcher's successful side effect (if any) is not rolled back.
fn bounded_response(id: Value, response: Value) -> Value {
    match serde_json::to_vec(&response) {
        Ok(payload) if payload.len() <= MAX_FRAME_BYTES => response,
        _ => error_response(
            id,
            RpcError::new(error_code::FRAME_TOO_LARGE, "the response exceeds the 16 MiB IPC frame; use a smaller page or the local process")
                .with_data(json!({ "kind": "frame_too_large" })),
        ),
    }
}

async fn handle_connection(
    stream: Box<dyn Stream>,
    connection: u64,
    config: ServerConfig,
    dispatcher: Arc<dyn Dispatcher>,
    hub: Arc<EventHub>,
    global: Arc<Semaphore>,
    active_subscriptions: Arc<AtomicUsize>,
) -> Result<(), IpcError> {
    let (reader_half, mut writer_half) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader_half, MAX_FRAME_BYTES);
    let connection_cancel = CancellationToken::new();
    let _cancel_guard = CancelOnDrop(connection_cancel.clone());

    // The first frame is the handshake. It has its own short timeout: an accepted socket that never
    // speaks must not occupy a connection slot for the whole idle window.
    let frame = match timeout(config.handshake_timeout, reader.next_frame()).await {
        Ok(Ok(Some(frame))) => frame,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(error)) => {
            let _ = write_frame(&mut writer_half, &error_response(Value::Null, RpcError::invalid_request(error.to_string())), MAX_FRAME_BYTES).await;
            return Ok(());
        }
        Err(_) => {
            let _ = write_frame(
                &mut writer_half,
                &error_response(Value::Null, RpcError::new(error_code::INVALID_REQUEST, "handshake timed out")),
                MAX_FRAME_BYTES,
            )
            .await;
            return Ok(());
        }
    };
    let hello = match parse_request(&frame) {
        Ok(request) => request,
        Err(error) => {
            let _ = write_frame(&mut writer_half, &error_response(Value::Null, error), MAX_FRAME_BYTES).await;
            return Ok(());
        }
    };
    let hello_id = hello.id.clone().unwrap_or(Value::Null);
    if hello.method != method::HELLO {
        let _ = write_frame(&mut writer_half, &error_response(hello_id, RpcError::unauthorized()), MAX_FRAME_BYTES).await;
        return Ok(());
    }
    let params: HelloParams = match serde_json::from_value(hello.params) {
        Ok(params) => params,
        Err(error) => {
            let _ =
                write_frame(&mut writer_half, &error_response(hello_id, RpcError::invalid_params(format!("hello params: {error}"))), MAX_FRAME_BYTES)
                    .await;
            return Ok(());
        }
    };
    if let Err(error) = params.validate() {
        let _ = write_frame(&mut writer_half, &error_response(hello_id, error), MAX_FRAME_BYTES).await;
        return Ok(());
    }
    // Token before version: a caller without the secret learns nothing about the daemon's version.
    if !config.token.verify(&params.token) {
        log::debug!("daemon rejected a client with an invalid token");
        let _ = write_frame(&mut writer_half, &error_response(hello_id, RpcError::unauthorized()), MAX_FRAME_BYTES).await;
        return Ok(());
    }
    if params.protocol != config.protocol {
        let error = RpcError::new(
            error_code::VERSION,
            format!("protocol version mismatch: the client speaks {}, the daemon speaks {}", params.protocol, config.protocol),
        )
        .with_data(json!({ "client": params.protocol, "daemon": config.protocol }));
        let _ = write_frame(&mut writer_half, &error_response(hello_id, error), MAX_FRAME_BYTES).await;
        return Ok(());
    }

    let hello_result = HelloResult {
        protocol: config.protocol,
        daemon_version: config.daemon_version.clone(),
        pid: std::process::id(),
        started_at: config.started_at,
        store_root: config.store_root.clone(),
        endpoint_kind: config.endpoint.kind().to_owned(),
    };
    write_frame(&mut writer_half, &result_response(hello_id, serde_json::to_value(hello_result)?), MAX_FRAME_BYTES).await?;

    let session = Arc::new(Session::new(params.client, connection, params.allow_send));
    let (write_tx, mut write_rx) = mpsc::channel::<Value>(WRITE_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(value) = write_rx.recv().await {
            let written = timeout(Duration::from_secs(30), write_frame(&mut writer_half, &value, MAX_FRAME_BYTES)).await;
            if !matches!(written, Ok(Ok(()))) {
                break;
            }
        }
    });

    let permits = Arc::new(Semaphore::new(PER_CONNECTION_IN_FLIGHT));
    let active_requests: Arc<std::sync::Mutex<HashSet<String>>> = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let _active_cleanup = ActiveCleanup { dispatcher: Arc::clone(&dispatcher), active: Arc::clone(&active_requests) };
    let mut tasks = JoinSet::new();
    let mut subscriptions = JoinSet::new();
    let mut subscription_sequence = 0u64;
    let mut subscription_count = 0usize;
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(15), Duration::from_secs(15));

    enum Wake {
        Frame(Result<Option<Vec<u8>>, IpcError>),
        Task,
        Subscription,
        Heartbeat,
        Idle,
    }

    loop {
        let busy = !tasks.is_empty() || !subscriptions.is_empty();
        let read = reader.next_frame();
        tokio::pin!(read);
        let wake = if busy {
            tokio::select! {
                frame = &mut read => Wake::Frame(frame),
                _ = tasks.join_next(), if !tasks.is_empty() => Wake::Task,
                _ = subscriptions.join_next(), if !subscriptions.is_empty() => Wake::Subscription,
                _ = heartbeat.tick() => Wake::Heartbeat,
            }
        } else {
            tokio::select! {
                frame = &mut read => Wake::Frame(frame),
                _ = tasks.join_next(), if !tasks.is_empty() => Wake::Task,
                _ = subscriptions.join_next(), if !subscriptions.is_empty() => Wake::Subscription,
                _ = tokio::time::sleep(config.idle_timeout) => Wake::Idle,
            }
        };
        let frame = match wake {
            Wake::Frame(Ok(Some(frame))) => frame,
            Wake::Frame(Ok(None)) => break,
            Wake::Frame(Err(IpcError::FrameTooLarge { limit })) => {
                let error = RpcError::new(error_code::FRAME_TOO_LARGE, format!("a frame exceeds the {limit} byte limit"));
                if write_tx.send(error_response(Value::Null, error)).await.is_err() {
                    break;
                }
                // The oversized line was drained through its newline; the next frame is safe.
                continue;
            }
            Wake::Frame(Err(error)) => {
                log::debug!("daemon read ended: {error}");
                break;
            }
            Wake::Task | Wake::Subscription => continue,
            Wake::Heartbeat => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |duration| duration.as_millis() as i64);
                let _ = write_tx.try_send(json!({ "jsonrpc": "2.0", "method": method::HEARTBEAT, "params": { "now_ms": now } }));
                continue;
            }
            Wake::Idle => break,
        };
        let request = match parse_request(&frame) {
            Ok(request) => request,
            Err(error) => {
                if write_tx.send(error_response(Value::Null, error)).await.is_err() {
                    break;
                }
                continue;
            }
        };
        let Some(id) = request.id.clone() else {
            // A notification without a method in the operation registry has no work to do.
            continue;
        };
        match request.method.as_str() {
            method::PING => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |duration| duration.as_millis() as i64);
                if write_tx.send(result_response(id, json!({ "ok": true, "now_ms": now }))).await.is_err() {
                    break;
                }
            }
            method::EVENTS_SUBSCRIBE => {
                let params: EventsSubscribe = match serde_json::from_value(if request.params.is_null() { json!({}) } else { request.params }) {
                    Ok(params) => params,
                    Err(error) => {
                        let _ = write_tx.send(error_response(id, RpcError::invalid_params(format!("events_subscribe: {error}")))).await;
                        continue;
                    }
                };
                if subscription_count >= config.max_subscriptions_per_connection {
                    if write_tx.send(error_response(id, RpcError::overloaded())).await.is_err() {
                        break;
                    }
                    continue;
                }
                let previous = active_subscriptions.fetch_add(1, Ordering::SeqCst);
                if previous >= config.max_subscriptions {
                    active_subscriptions.fetch_sub(1, Ordering::SeqCst);
                    if write_tx.send(error_response(id, RpcError::overloaded())).await.is_err() {
                        break;
                    }
                    continue;
                }
                subscription_count = subscription_count.saturating_add(1);
                subscription_sequence = subscription_sequence.saturating_add(1);
                let subscription = format!("sub-{connection}-{subscription_sequence}");
                let receiver = hub.subscribe();
                let sink = write_tx.clone();
                let stop = connection_cancel.clone();
                let name = subscription.clone();
                let counter = Arc::clone(&active_subscriptions);
                let guard = SubscriptionGuard { counter };
                subscriptions.spawn(async move {
                    let _guard = guard;
                    forward_events(receiver, sink, stop, name, params.kinds).await;
                });
                if write_tx.send(result_response(id, json!({ "subscription": subscription }))).await.is_err() {
                    break;
                }
            }
            method::CANCEL => {
                let cancel: CancelRequest = match serde_json::from_value(request.params) {
                    Ok(cancel) => cancel,
                    Err(error) => {
                        let _ = write_tx.send(error_response(id, RpcError::invalid_params(format!("cancel: {error}")))).await;
                        continue;
                    }
                };
                if cancel.request.len() > 128 {
                    let _ = write_tx.send(error_response(id, RpcError::invalid_params("request id is too long"))).await;
                    continue;
                }
                let cancelled = dispatcher.cancel(&format!("{connection}:{}", cancel.request));
                if write_tx.send(result_response(id, json!({ "cancelled": cancelled }))).await.is_err() {
                    break;
                }
            }
            _ => {
                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        if write_tx.send(error_response(id, RpcError::overloaded())).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let global_permit = match Arc::clone(&global).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        if write_tx.send(error_response(id, RpcError::overloaded())).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let key = format!("{connection}:{}", request_key(&id));
                let inserted = active_requests.lock().map(|mut active| active.insert(key.clone())).unwrap_or(false);
                if !inserted {
                    if write_tx
                        .send(error_response(id, RpcError::new(error_code::INVALID_REQUEST, "a request with this id is already in flight")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                let dispatcher = Arc::clone(&dispatcher);
                let session = Arc::clone(&session);
                let sink = write_tx.clone();
                let active_requests = Arc::clone(&active_requests);
                let method = request.method;
                let params = request.params;
                tasks.spawn(async move {
                    let result = dispatcher.dispatch(DispatchRequest { method, params, request_id: key.clone(), session }).await;
                    if let Ok(mut active) = active_requests.lock() {
                        active.remove(&key);
                    }
                    let response = match result {
                        Ok(value) => result_response(id.clone(), value),
                        Err(error) => error_response(id.clone(), error),
                    };
                    let response = bounded_response(id, response);
                    let _ = sink.send(response).await;
                    drop(permit);
                    drop(global_permit);
                });
            }
        }
        while let Some(finished) = tasks.try_join_next() {
            if let Err(error) = finished {
                log::warn!("daemon request task ended unexpectedly: {error}");
            }
        }
        while let Some(finished) = subscriptions.try_join_next() {
            if let Err(error) = finished {
                log::debug!("daemon event forwarder ended: {error}");
            }
        }
    }

    // A disconnected client cancels its in-flight work. The RAII guard also runs when a shutdown
    // aborts this task, so a blocking sync is always asked to stop and its dispatch cleanup runs.
    connection_cancel.cancel();
    tasks.abort_all();
    subscriptions.abort_all();
    drop(write_tx);
    let _ = timeout(Duration::from_secs(2), writer).await;
    Ok(())
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct ActiveCleanup {
    dispatcher: Arc<dyn Dispatcher>,
    active: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl Drop for ActiveCleanup {
    fn drop(&mut self) {
        if let Ok(active) = self.active.lock() {
            for key in active.iter() {
                self.dispatcher.cancel(key);
            }
        }
    }
}

struct SubscriptionGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn forward_events(
    mut receiver: tokio::sync::broadcast::Receiver<Value>,
    sink: mpsc::Sender<Value>,
    stop: CancellationToken,
    subscription: String,
    kinds: Vec<String>,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            event = receiver.recv() => match event {
                Ok(event) => {
                    if !kinds.is_empty() && !kinds.iter().any(|kind| event.get("kind").and_then(Value::as_str) == Some(kind.as_str())) {
                        continue;
                    }
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": method::EVENT,
                        "params": EventParams { subscription: subscription.clone(), event },
                    });
                    // A slow client loses events; the daemon does not grow a backlog for it.
                    if sink.try_send(notification).is_err() {
                        continue;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    log::debug!("daemon event subscriber {subscription} lagged by {dropped} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

fn parse_request(frame: &[u8]) -> Result<RpcRequest, RpcError> {
    let value: Value = serde_json::from_slice(frame).map_err(|error| RpcError::new(error_code::PARSE, format!("parse error: {error}")))?;
    let request: RpcRequest =
        serde_json::from_value(value).map_err(|error| RpcError::new(error_code::INVALID_REQUEST, format!("invalid request: {error}")))?;
    request.validate()?;
    if let Some(id) = &request.id
        && !id.is_string()
        && !id.is_number()
        && !id.is_null()
    {
        return Err(RpcError::invalid_request("id must be a string, a number or null"));
    }
    if let Some(id) = &request.id
        && request_key(id).len() > 128
    {
        return Err(RpcError::invalid_request("id is longer than 128 characters"));
    }
    Ok(request)
}

/// A small helper for tests and for `DaemonPaths` callers that already have a store path.
pub fn paths_for_store(store: &std::path::Path) -> Result<DaemonPaths, IpcError> {
    DaemonPaths::for_store(store)
}
