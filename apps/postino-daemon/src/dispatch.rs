//! The daemon's side of the IPC protocol: one operation registry, the live status, cancellation and
//! shutdown. No second operation table exists here; `op` is dispatched through `postino-core`.

use std::sync::Arc;
use std::time::Duration;

use postino_core::{OpError, SyncReport};
use postino_ipc::protocol::method;
use postino_ipc::server::BoxFuture;
use postino_ipc::{DispatchRequest, Dispatcher, OpRequest, RpcError, Session};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::DaemonState;

/// What `daemon_status` returns. It contains no token, credential, body, subject, address or query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonStatus {
    pub protocol: u32,
    pub version: String,
    pub pid: u32,
    pub started_at: i64,
    pub uptime_ms: i64,
    pub store_root: String,
    pub endpoint_kind: String,
    pub accounts_syncing: Vec<String>,
    pub outbox_next_wake: Option<i64>,
    pub index: Option<Value>,
    pub rules_running: bool,
    pub last_error: Option<String>,
    pub recovery: Option<String>,
    pub scheduler_wakes: u64,
    /// Explicit manual-sync requests the scheduler consumed (tray "Sincronizza ora").
    pub manual_syncs: u64,
}

impl Dispatcher for DaemonState {
    fn dispatch(&self, request: DispatchRequest) -> BoxFuture<Result<Value, RpcError>> {
        let state = self.clone();
        Box::pin(async move { state.handle(request).await })
    }

    fn cancel(&self, request_id: &str) -> bool {
        self.cancel_request(request_id)
    }
}

impl DaemonState {
    async fn handle(&self, request: DispatchRequest) -> Result<Value, RpcError> {
        match request.method.as_str() {
            method::OPS_LIST => serde_json::to_value(self.registry.as_ref()).map_err(internal),
            method::DAEMON_STATUS => self.status().await,
            method::DAEMON_SHUTDOWN => {
                self.cancel_all();
                let state = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    state.shutdown.cancel();
                });
                Ok(json!({ "ok": true, "reason": "shutdown requested by a same-user client" }))
            }
            method::OP => self.operation(request).await,
            other => Err(RpcError::method_not_found(format!("method not found: {other}"))),
        }
    }

    async fn operation(&self, request: DispatchRequest) -> Result<Value, RpcError> {
        let request_id = request.request_id.clone();
        let session = Arc::clone(&request.session);
        let operation: OpRequest = serde_json::from_value(request.params).map_err(|error| RpcError::invalid_params(format!("op: {error}")))?;
        operation.validate()?;
        let Some(definition) = self.registry.iter().find(|definition| definition.name == operation.name) else {
            return Err(RpcError::method_not_found(format!("unknown operation {:?}", operation.name)));
        };
        if definition.sensitive && session.client == "mcp" {
            return Err(RpcError::operation(
                format!("operation {:?} is not available over MCP: it handles an account credential", operation.name),
                "sensitive",
            ));
        }
        if session.client == "mcp"
            && !session.allow_send
            && matches!(operation.name.as_str(), "message_send" | "send_schedule" | "outbox_retry" | "outbox_mark_sent" | "rules_run")
        {
            return Err(RpcError::operation(
                format!("operation {:?} is disabled for this MCP session: start `postino mcp --allow-send`", operation.name),
                "send_disabled",
            ));
        }
        let read_only = definition.read_only;
        let name = operation.name.clone();
        let args = operation.args;
        let _mutation = if read_only {
            None
        } else {
            if !session.begin_mutation() {
                return Err(RpcError::busy("one mutation at a time per client; wait for the first request"));
            }
            Some(MutationGuard(Arc::clone(&session)))
        };
        let result = if name == "sync_now" {
            self.manual_sync(&request_id, session.allow_send, args).await
        } else {
            let engine_name = name.clone();
            self.with_engine(move |engine| postino_core::ops::dispatch(engine, &engine_name, args)).await.map_err(internal)?
        };
        self.mark_index_dirty();
        self.wake_scheduler();
        if matches!(name.as_str(), "rules_run" | "rules_undo" | "rules_create" | "rules_update" | "rules_delete") {
            // A manual rules run moves messages and labels inside the engine; the clients reload.
            self.publish("rules", json!({ "source": name }));
        }
        match result {
            Ok(value) => {
                self.clear_error();
                Ok(value)
            }
            Err(error) => {
                let sentence = error_category(&error);
                self.note_error(sentence.to_owned());
                log::warn!("daemon operation {name} failed: {sentence}");
                Err(operation_error(error))
            }
        }
    }

    async fn manual_sync(&self, request_id: &str, allow_send: bool, args: Value) -> Result<Value, OpError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SyncArgs {
            account: Option<String>,
        }
        let args: SyncArgs = serde_json::from_value(args).map_err(|error| OpError::InvalidArgs(format!("op: {error}")))?;
        let flag = self.begin_request(request_id);
        let _sync_guard = SyncGuard { state: self.clone(), request_id: request_id.to_owned(), account: args.account.clone() };
        self.set_sync_active(true, args.account.as_deref());
        self.publish("sync", json!({ "phase": "started", "account": args.account }));
        let account = args.account.clone();
        let watched = match account.as_deref() {
            Some(id) => Some(id.to_owned()),
            None => self.account_ids().await.ok().filter(|ids| ids.len() == 1).and_then(|ids| ids.first().cloned()),
        };
        let watcher = watched.and_then(|id| self.spawn_sync_watcher(id));
        let chunk = self.config.sync_chunk_messages.max(1).min(u32::MAX as usize) as u32;
        let mut aggregate = SyncReport {
            account: account.clone().unwrap_or_default(),
            folders: 0,
            fetched: 0,
            updated_flags: 0,
            removed: 0,
            attachments_stored: 0,
            warnings: Vec::new(),
        };
        let result = loop {
            let chunk_flag = Arc::clone(&flag);
            let outcome = self
                .with_engine({
                    let account = account.clone();
                    move |engine| {
                        let previous = engine.allow_send();
                        engine.set_allow_send(previous && allow_send);
                        let result = engine.sync_now_with_cancel_limits(
                            account.as_deref(),
                            &chunk_flag,
                            postino_sync::SyncLimits { max_messages: usize::try_from(chunk).unwrap_or(usize::MAX), ..Default::default() },
                        );
                        engine.set_allow_send(previous);
                        result
                    }
                })
                .await;
            let chunk_result = match outcome {
                Ok(result) => result,
                Err(error) => break Err(OpError::Failed(error.to_string())),
            };
            let report = match chunk_result {
                Ok(report) => report,
                Err(error) => break Err(error),
            };
            if aggregate.account.is_empty() {
                aggregate.account = report.account.clone();
            }
            aggregate.folders = aggregate.folders.max(report.folders);
            aggregate.fetched = aggregate.fetched.saturating_add(report.fetched);
            aggregate.updated_flags = aggregate.updated_flags.saturating_add(report.updated_flags);
            aggregate.removed = aggregate.removed.saturating_add(report.removed);
            aggregate.attachments_stored = aggregate.attachments_stored.saturating_add(report.attachments_stored);
            aggregate.warnings.extend(report.warnings);
            if report.fetched < chunk {
                break Ok(aggregate);
            }
        };
        drop(watcher);
        if let Ok(mut status) = self.status.lock() {
            status.last_sync_at = Some(crate::now_ms());
        }
        match result {
            Ok(report) => {
                self.publish("sync", sync_event("finished", &report));
                if !report.account.is_empty() && self.rules_enabled(&report.account).await {
                    // The engine runs the post-sync rules pass inside `sync_now`: tell the clients
                    // the labels changed, without waiting for the next GC event.
                    self.publish("rules", json!({ "account": report.account, "source": "sync" }));
                }
                if let Ok(mut status) = self.status.lock() {
                    status.gc_due = true;
                }
                serde_json::to_value(report).map_err(|error| OpError::Failed(error.to_string()))
            }
            Err(error) => {
                self.publish("sync", json!({ "phase": "failed", "account": args.account, "error": error_category(&error) }));
                Err(error)
            }
        }
    }

    async fn status(&self) -> Result<Value, RpcError> {
        // `daemon_status` must answer while a long sync holds the engine. Try the lock; if it is
        // busy, report the counters the scheduler already published instead of blocking.
        let facts = match self.engine.try_lock() {
            Ok(engine) => {
                let index = engine.search_index_status().ok().flatten().map(index_value);
                let next_wake = engine.next_outbox_wake().ok().flatten();
                let syncs = engine.sync_statuses(None).map(|views| serde_json::to_value(views).unwrap_or(Value::Null)).unwrap_or(Value::Null);
                let accounts = engine.accounts().len();
                (index, next_wake, syncs, accounts)
            }
            Err(_) => {
                let cached = self.status.lock().ok().map(|status| (status.last_index.clone(), status.outbox_next_wake));
                let (index, next_wake) = cached.unwrap_or((None, None));
                (index.map(index_value), next_wake, Value::Null, self.account_count)
            }
        };
        let (index, next_wake, syncs, accounts) = facts;
        let status = self.status.lock().map(|status| {
            (status.accounts_syncing.clone(), status.last_error.clone(), status.recovery.clone(), status.scheduler_wakes, status.sync_active)
        });
        let (accounts_syncing, last_error, recovery, scheduler_wakes, rules_running) = status.unwrap_or_default();
        let status = DaemonStatus {
            protocol: postino_ipc::PROTOCOL_VERSION,
            version: self.config.daemon_version.clone(),
            pid: std::process::id(),
            started_at: self.started_at,
            uptime_ms: crate::now_ms().saturating_sub(self.started_at),
            store_root: self.store_root.clone(),
            endpoint_kind: self.paths.endpoint.kind().to_owned(),
            accounts_syncing,
            outbox_next_wake: next_wake,
            index,
            rules_running,
            last_error,
            recovery,
            scheduler_wakes,
            manual_syncs: self.manual_syncs(),
        };
        let mut value = serde_json::to_value(status).map_err(internal)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("accounts".to_owned(), json!(accounts));
            object.insert("sync".to_owned(), syncs);
        }
        Ok(value)
    }
}

struct MutationGuard(Arc<Session>);

impl Drop for MutationGuard {
    fn drop(&mut self) {
        self.0.end_mutation();
    }
}

/// Clears the request's cancellation entry and the sync-active marker even if the dispatch task is
/// aborted by a disconnect, shutdown or idle timeout.
struct SyncGuard {
    state: DaemonState,
    request_id: String,
    account: Option<String>,
}

impl Drop for SyncGuard {
    fn drop(&mut self) {
        self.state.finish_request(&self.request_id);
        self.state.set_sync_active(false, self.account.as_deref());
    }
}

fn sync_event(phase: &str, report: &SyncReport) -> Value {
    json!({
        "phase": phase,
        "account": report.account,
        "folders": report.folders,
        "fetched": report.fetched,
        "updated_flags": report.updated_flags,
        "removed": report.removed,
        "attachments_stored": report.attachments_stored,
    })
}

fn index_value(status: postino_store::SearchIndexStatus) -> Value {
    json!({
        "state": status.state,
        "indexed": status.indexed,
        "total": status.total,
        "ready": status.ready,
        "last_error": status.last_error,
        "size_bytes": status.size_bytes,
        "updated_at": status.updated_at,
    })
}

pub(crate) fn error_category(error: &OpError) -> &'static str {
    match error {
        OpError::UnknownOp(_) => "unknown operation",
        OpError::InvalidArgs(_) => "invalid arguments",
        OpError::Auth(_) => "authentication failed",
        OpError::Network(_) => "server unreachable",
        OpError::Failed(_) => "operation failed",
    }
}

fn operation_error(error: OpError) -> RpcError {
    let kind = match &error {
        OpError::UnknownOp(_) => "unknown_operation",
        OpError::InvalidArgs(_) => "invalid_args",
        OpError::Auth(_) => "auth",
        OpError::Network(_) => "network",
        OpError::Failed(_) => "failed",
    };
    let code = match &error {
        OpError::UnknownOp(_) => postino_ipc::error_code::METHOD_NOT_FOUND,
        OpError::InvalidArgs(_) => postino_ipc::error_code::INVALID_PARAMS,
        _ => postino_ipc::error_code::OPERATION,
    };
    RpcError::new(code, error.to_string()).with_data(json!({ "kind": kind }))
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(postino_ipc::error_code::INTERNAL, error.to_string())
}
