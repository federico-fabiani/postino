//! Shared daemon state: the single `Engine`, the event hub, the live scheduler counters and the
//! per-request cancellation table. The dispatcher and the scheduler both hold an `Arc<DaemonState>`;
//! no second `Engine` is ever constructed in this process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use postino_core::{Engine, OpDef};
use postino_ipc::{CancellationToken, DaemonPaths, EventHub, ServerConfig, SharedToken};
use postino_platform::{Notifier, Tray, TrayAction, TrayError};
use postino_store::{Store, StoreKey};
use tokio::sync::Notify;

use crate::settings::DaemonSettings;
use crate::tray_host::{AppLauncher, NoticePolicy};
use crate::{DaemonConfig, DaemonError};

/// Scheduler and request facts a status call can read without touching the engine.
#[derive(Debug, Default)]
pub(crate) struct LiveStatus {
    pub sync_active: bool,
    pub accounts_syncing: Vec<String>,
    pub outbox_next_wake: Option<i64>,
    pub last_sync_at: Option<i64>,
    pub scheduler_wakes: u64,
    pub last_error: Option<String>,
    pub recovery: Option<String>,
    pub gc_due: bool,
    pub index_error: bool,
    pub last_index: Option<postino_store::SearchIndexStatus>,
    /// The last sync failed for a network/session reason; the tray shows offline, not a badge.
    pub network_error: bool,
    /// At least one outbox row is failed or uncertain.
    pub outbox_failed: bool,
}

#[derive(Clone)]
pub struct DaemonState {
    pub(crate) engine: Arc<Mutex<Engine>>,
    pub(crate) hub: Arc<EventHub>,
    pub(crate) config: DaemonConfig,
    pub(crate) paths: DaemonPaths,
    pub(crate) token: SharedToken,
    pub(crate) store_root: String,
    pub(crate) started_at: i64,
    pub(crate) shutdown: CancellationToken,
    pub(crate) status: Arc<Mutex<LiveStatus>>,
    /// In-flight operations that can be cancelled: request id → flag.
    pub(crate) active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// Wakes the scheduler when a mutation queues or changes an outbox row.
    pub(crate) outbox_notify: Arc<Notify>,
    /// Kept so `daemon_status` and `ops_list` use exactly the registry the CLI and MCP see.
    pub(crate) registry: Arc<Vec<OpDef>>,
    pub(crate) wake_count: Arc<AtomicU64>,
    pub(crate) index_dirty: Arc<AtomicBool>,
    pub(crate) account_count: usize,
    /// Daemon-owned tray, if the platform seam was injected (tests use a recorder or none).
    pub(crate) tray: Option<Arc<dyn Tray>>,
    notifier: OnceLock<Option<Arc<dyn Notifier>>>,
    pub(crate) notices: Arc<Mutex<NoticePolicy>>,
    pub(crate) launcher: Arc<dyn AppLauncher>,
    pub(crate) sync_notify: Arc<Notify>,
    manual_syncs: Arc<AtomicU64>,
    settings: Arc<Mutex<DaemonSettings>>,
    tray_error_logged: Arc<AtomicBool>,
}

impl DaemonState {
    pub(crate) fn new(
        mut engine: Engine,
        hub: Arc<EventHub>,
        config: DaemonConfig,
        paths: DaemonPaths,
        token: SharedToken,
        store_root: String,
    ) -> Self {
        let started_at = config.started_at;
        let account_count = engine.accounts().len();
        let notices = Arc::new(Mutex::new(NoticePolicy::initialize(&mut engine)));
        let notifier = OnceLock::new();
        if config.notifier.is_some() {
            let _ = notifier.set(config.notifier.clone());
        }
        let settings = config.settings.clone();
        Self {
            engine: Arc::new(Mutex::new(engine)),
            hub,
            tray: config.tray.clone(),
            notifier,
            notices,
            launcher: Arc::clone(&config.app_launcher),
            sync_notify: Arc::new(Notify::new()),
            manual_syncs: Arc::new(AtomicU64::new(0)),
            settings: Arc::new(Mutex::new(settings)),
            tray_error_logged: Arc::new(AtomicBool::new(false)),
            config,
            paths,
            token,
            store_root,
            started_at,
            shutdown: CancellationToken::new(),
            status: Arc::new(Mutex::new(LiveStatus::default())),
            active: Arc::new(Mutex::new(HashMap::new())),
            outbox_notify: Arc::new(Notify::new()),
            registry: Arc::new(postino_core::op_defs()),
            wake_count: Arc::new(AtomicU64::new(0)),
            index_dirty: Arc::new(AtomicBool::new(true)),
            account_count,
        }
    }

    pub(crate) fn settings(&self) -> DaemonSettings {
        self.settings.lock().map(|settings| settings.clone()).unwrap_or_default()
    }

    pub(crate) fn tray_action(&self) -> Option<TrayAction> {
        self.tray.as_ref().and_then(|tray| tray.next_action())
    }

    pub(crate) fn install_notifier(&self, notifier: Option<Arc<dyn Notifier>>) {
        if let Some(notifier) = notifier {
            let _ = self.notifier.set(Some(notifier));
        }
    }

    pub(crate) fn notifier(&self) -> Option<Arc<dyn Notifier>> {
        self.notifier.get().and_then(|notifier| notifier.clone())
    }

    pub(crate) fn live_flags(&self) -> (bool, bool, bool) {
        self.status.lock().map(|status| (status.sync_active, status.network_error, status.outbox_failed)).unwrap_or((false, false, false))
    }

    pub(crate) fn set_network_error(&self, error: bool) {
        if let Ok(mut status) = self.status.lock() {
            status.network_error = error;
        }
        crate::tray_host::refresh(self);
    }

    pub(crate) fn set_outbox_failed(&self, failed: bool) {
        if let Ok(mut status) = self.status.lock() {
            status.outbox_failed = failed;
        }
    }

    pub(crate) fn note_tray_error(&self, error: TrayError) {
        if !self.tray_error_logged.swap(true, Ordering::Relaxed) {
            log::warn!("tray state could not be updated; the daemon keeps serving: {error}");
        }
    }

    pub(crate) fn request_sync(&self) {
        self.sync_notify.notify_one();
    }

    pub fn server_config(&self) -> Result<ServerConfig, DaemonError> {
        let mut config = ServerConfig::new(self.paths.endpoint.clone(), self.token.clone());
        config.daemon_version = self.config.daemon_version.clone();
        config.started_at = self.started_at;
        config.store_root = self.store_root.clone();
        config.handshake_timeout = self.config.handshake_timeout;
        config.idle_timeout = self.config.idle_timeout;
        config.max_connections = self.config.max_connections;
        Ok(config)
    }

    pub fn hub(&self) -> Arc<EventHub> {
        Arc::clone(&self.hub)
    }

    pub fn shutdown(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    pub fn publish(&self, kind: &str, data: serde_json::Value) {
        self.hub.publish(kind, data);
    }

    pub fn note_error(&self, sentence: impl Into<String>) {
        if let Ok(mut status) = self.status.lock() {
            status.last_error = Some(sentence.into());
        }
    }

    pub fn clear_error(&self) {
        if let Ok(mut status) = self.status.lock() {
            status.last_error = None;
        }
    }

    pub fn note_recovery(&self, sentence: impl Into<String>) {
        if let Ok(mut status) = self.status.lock() {
            status.recovery = Some(sentence.into());
        }
    }

    pub fn wake_scheduler(&self) {
        self.outbox_notify.notify_one();
    }

    pub fn mark_index_dirty(&self) {
        self.index_dirty.store(true, Ordering::SeqCst);
    }

    pub fn index_dirty(&self) -> bool {
        self.index_dirty.load(Ordering::SeqCst)
    }

    pub fn take_index_dirty(&self) -> bool {
        self.index_dirty.swap(false, Ordering::SeqCst)
    }

    pub fn count_wake(&self) {
        self.wake_count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut status) = self.status.lock() {
            status.scheduler_wakes = status.scheduler_wakes.saturating_add(1);
        }
    }

    /// The scheduler consumed an explicit manual-sync request (tray "Sincronizza ora"). Distinct
    /// from a timer wake: a test can prove the tray action reached the scheduler.
    pub fn count_manual_sync(&self) {
        self.manual_syncs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn manual_syncs(&self) -> u64 {
        self.manual_syncs.load(Ordering::Relaxed)
    }

    /// Register a cancellable operation. The returned flag must be passed to the engine and removed
    /// with [`DaemonState::finish_request`] even when the operation fails.
    pub fn begin_request(&self, request_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut active) = self.active.lock() {
            active.insert(request_id.to_owned(), Arc::clone(&flag));
        }
        flag
    }

    /// Cancel every live request. Called by `daemon_shutdown` before the server closes so a sync that
    /// is inside the engine stops at its next checkpoint instead of running past the grace period.
    pub fn cancel_all(&self) -> usize {
        let flags: Vec<Arc<AtomicBool>> = self.active.lock().map(|active| active.values().cloned().collect()).unwrap_or_default();
        for flag in &flags {
            flag.store(true, Ordering::SeqCst);
        }
        flags.len()
    }

    /// A handle that can cancel requests without keeping the `Engine` alive. The daemon handle uses
    /// it on drop so the store's writer lock is released even if the IPC shutdown path has not run.
    pub(crate) fn active_requests(&self) -> Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> {
        Arc::clone(&self.active)
    }

    pub fn finish_request(&self, request_id: &str) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(request_id);
        }
    }

    /// Cancel an in-flight request. Returns true only when a registered operation was still live.
    pub fn cancel_request(&self, request_id: &str) -> bool {
        let flag = self.active.lock().ok().and_then(|active| active.get(request_id).cloned());
        match flag {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    pub fn is_sync_active(&self) -> bool {
        self.status.lock().map(|status| status.sync_active).unwrap_or(false)
    }

    pub(crate) fn set_sync_active(&self, active: bool, account: Option<&str>) {
        if let Ok(mut status) = self.status.lock() {
            status.sync_active = active;
            match (active, account) {
                (true, Some(account)) => {
                    if !status.accounts_syncing.iter().any(|entry| entry == account) {
                        status.accounts_syncing.push(account.to_owned());
                    }
                }
                (false, Some(account)) => status.accounts_syncing.retain(|entry| entry != account),
                (false, None) => status.accounts_syncing.clear(),
                (true, None) => {}
            }
        }
        crate::tray_host::refresh(self);
    }

    /// Poll the persisted sync status on its own read-only connection while a sync runs. The engine
    /// owns the write connection; SQLite WAL lets this reader see committed folder/message progress
    /// and the scheduler turns it into bounded `sync` events. The watcher is a stop flag plus one
    /// short-lived thread, never a second writer.
    pub(crate) fn spawn_sync_watcher(&self, account: String) -> Option<SyncWatcher> {
        let key = StoreKey::from_bytes(*self.config.key.as_bytes());
        let store = Store::open_read_only(&self.config.store_path, &key).ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let state = self.clone();
        let handle = std::thread::Builder::new()
            .name("postino-sync-progress".to_owned())
            .spawn(move || {
                let mut previous = (u32::MAX, u32::MAX, u32::MAX);
                while !thread_stop.load(Ordering::Relaxed) {
                    if let Ok(Some(status)) = postino_sync::read_status_for(&store, &account)
                        && status.state == postino_sync::SyncPhase::Syncing
                    {
                        let sample = (status.folders_done, status.folders_total, status.messages);
                        if sample != previous {
                            state.publish(
                                "sync",
                                serde_json::json!({
                                    "phase": "progress",
                                    "account": account,
                                    "folders_done": status.folders_done,
                                    "folders_total": status.folders_total,
                                    "messages": status.messages,
                                }),
                            );
                            previous = sample;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
            })
            .ok()?;
        Some(SyncWatcher { stop, handle: Some(handle) })
    }

    /// Whether the account has at least one enabled rule, so a sync ran the engine's post-sync
    /// rules pass and the clients must reload labels without waiting for the next blob GC.
    pub(crate) async fn rules_enabled(&self, account: &str) -> bool {
        let account = account.to_owned();
        let probe = self
            .with_engine(move |engine| engine.rules_list(Some(&account)).map(|rules| rules.iter().any(|rule| rule.enabled)).unwrap_or(false))
            .await;
        matches!(probe, Ok(true))
    }

    /// Run a blocking engine operation on the blocking pool while holding the one engine mutex.
    /// A panic inside a handler becomes a readable failure; it never tears down the daemon.
    pub(crate) async fn with_engine<T, F>(&self, operation: F) -> Result<T, DaemonError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Engine) -> T + Send + 'static,
    {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || {
            let mut guard = match engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            operation(&mut guard)
        })
        .await
        .map_err(|error| DaemonError::Runtime(format!("engine task ended unexpectedly: {error}")))
    }

    pub(crate) async fn account_ids(&self) -> Result<Vec<String>, DaemonError> {
        self.with_engine(|engine| engine.accounts().iter().map(|account| account.id.clone()).collect()).await
    }
}

impl std::fmt::Debug for DaemonState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("DaemonState").field("store_root", &self.store_root).field("started_at", &self.started_at).finish_non_exhaustive()
    }
}

/// Stops and joins the sync-progress reader. Dropping it is the normal end of a sync.
pub(crate) struct SyncWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SyncWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
