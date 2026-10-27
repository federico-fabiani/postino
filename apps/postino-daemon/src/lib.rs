//! The Postino daemon: one process owns the encrypted store, the network sessions, the outbox
//! scheduler, local rules, the search-index batches and blob GC. The app, CLI and MCP server are
//! clients of the same operation registry over [`postino_ipc`].
//!
//! The library half exists so the daemon can be started by a test with an in-memory credential
//! vault and a throwaway TLS root. The binary in `src/main.rs` is the production shell: it reads the
//! store key from the OS keyring and never asks a test to do so.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod dispatch;
pub mod schedule;
pub mod state;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postino_core::{CredentialVault, Engine};
use postino_ipc::{DaemonPaths, Endpoint, IpcError, SharedToken, TokenFile};
use postino_net::CertificateDer;
use postino_store::{Store, StoreError, StoreKey, WriterLock};
use thiserror::Error;

pub use dispatch::DaemonStatus;
pub use state::DaemonState;

/// Why the daemon could not start or why a scheduler step failed.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("{0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Core(#[from] postino_core::OpError),
    #[error("the daemon runtime could not start: {0}")]
    Runtime(String),
    #[error("the daemon did not become ready: {0}")]
    Startup(String),
    #[error("the daemon timed out while {0}")]
    Timeout(&'static str),
    #[error("the daemon is already running{0}")]
    AlreadyRunning(String),
}

/// Everything the daemon needs. Production fills `vault`, `extra_roots` is empty; tests inject a
/// fixed key, an in-memory vault and the fake server's CA.
#[derive(Clone)]
pub struct DaemonConfig {
    pub store_path: PathBuf,
    pub key: Arc<StoreKey>,
    pub vault: Arc<dyn CredentialVault>,
    pub extra_roots: Vec<CertificateDer<'static>>,
    /// A test/diagnostic override. Production derives the name from the user and store path.
    pub endpoint: Option<Endpoint>,
    /// A test/diagnostic override. Production uses `<store>.token`.
    pub token_file: Option<PathBuf>,
    /// A fixed token for a test that must know it; production generates a fresh one per launch.
    pub token: Option<SharedToken>,
    pub sync_interval: Duration,
    pub sync_on_start: bool,
    /// Run the outbox pass immediately at daemon start. Production true; a crash-restart test turns
    /// it off in the child so the seeded crash state survives until the parent starts the replacement.
    pub outbox_on_start: bool,
    /// Maximum messages one engine-locked sync chunk ingests. The scheduler releases the engine
    /// between chunks so outbox/index work can run during a long initial sync.
    pub sync_chunk_messages: usize,
    /// Include a small per-run jitter in the sync interval. Tests turn it off for determinism.
    pub sync_jitter: bool,
    pub gc_interval: Duration,
    pub gc_batch: usize,
    pub index_tick: Duration,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_connections: usize,
    pub started_at: i64,
    pub daemon_version: String,
}

impl DaemonConfig {
    pub fn new(store_path: impl Into<PathBuf>, key: Arc<StoreKey>, vault: Arc<dyn CredentialVault>) -> Self {
        Self {
            store_path: store_path.into(),
            key,
            vault,
            extra_roots: Vec::new(),
            endpoint: None,
            token_file: None,
            token: None,
            sync_interval: Duration::from_secs(15 * 60),
            sync_on_start: true,
            outbox_on_start: true,
            sync_chunk_messages: 1_000,
            sync_jitter: true,
            gc_interval: Duration::from_secs(10 * 60),
            gc_batch: 64,
            index_tick: Duration::from_millis(200),
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(120),
            max_connections: 32,
            started_at: now_ms(),
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    fn paths(&self) -> Result<DaemonPaths, DaemonError> {
        let paths = DaemonPaths::for_store(&self.store_path)?;
        Ok(DaemonPaths {
            store: paths.store,
            endpoint: self.endpoint.clone().unwrap_or(paths.endpoint),
            token_file: self.token_file.clone().unwrap_or(paths.token_file),
        })
    }
}

/// A running daemon. Dropping it asks the scheduler to stop and joins the runtime thread; a client
/// can also stop it with `daemon_shutdown`.
pub struct Daemon {
    paths: DaemonPaths,
    started_at: i64,
    store_root: String,
    shutdown: postino_ipc::CancellationToken,
    active_requests: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
    thread: Option<std::thread::JoinHandle<Result<(), DaemonError>>>,
}

impl Daemon {
    /// Acquire the store, write the fresh token, bind the endpoint and start serving. The call
    /// returns only after the endpoint exists, so a caller can connect immediately.
    pub fn spawn(config: DaemonConfig) -> Result<Self, DaemonError> {
        let paths = config.paths()?;
        if let Some(parent) = paths.store.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| DaemonError::Startup(format!("creating {}: {error}", parent.display())))?;
        }
        let lock = WriterLock::acquire_as(&paths.store, "daemon", Some(postino_ipc::PROTOCOL_VERSION))?;
        let token = match config.token.clone() {
            Some(token) => token,
            None => SharedToken::generate()?,
        };
        let token_file = TokenFile::new(paths.token_file.clone());
        token_file.write(&token)?;
        let mut store = Store::open_with_lock(&paths.store, &config.key, lock)?;
        store.set_writer_endpoint(&paths.endpoint.describe())?;
        let mut engine = Engine::open_with_vault(store, Arc::clone(&config.vault))?;
        engine.set_test_extra_roots(config.extra_roots.clone());
        let store_root = paths.store.parent().map(|parent| parent.display().to_string()).unwrap_or_default();
        let state = Arc::new(DaemonState::new(engine, Arc::new(postino_ipc::EventHub::new()), config, paths.clone(), token, store_root.clone()));
        let shutdown = state.shutdown();
        let server_config = state.server_config()?;
        let dispatcher: Arc<dyn postino_ipc::Dispatcher> = state.clone();
        let server = postino_ipc::Server::new(server_config, dispatcher, state.hub());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), DaemonError>>();
        let thread_state = Arc::clone(&state);
        let thread_shutdown = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("postino-daemon".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(DaemonError::Runtime(error.to_string())));
                        return Err(DaemonError::Runtime(error.to_string()));
                    }
                };
                let result = runtime.block_on(async move {
                    let bound = match server.bind().await {
                        Ok(bound) => {
                            let _ = ready_tx.send(Ok(()));
                            bound
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(DaemonError::Ipc(error)));
                            return Ok(());
                        }
                    };
                    let scheduler = tokio::spawn(crate::schedule::run(Arc::clone(&thread_state)));
                    let result = bound.run(thread_shutdown.clone()).await;
                    thread_shutdown.cancel();
                    scheduler.abort();
                    let _ = scheduler.await;
                    result.map_err(DaemonError::Ipc)
                });
                // Give an in-flight blocking operation a bounded grace period; a cancelled sync
                // stops at its next checkpoint, but a stuck SMTP/IMAP transfer must not keep the
                // writer lock forever.
                runtime.shutdown_timeout(Duration::from_secs(5));
                result
            })
            .map_err(|error| DaemonError::Runtime(error.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self {
                paths,
                started_at: state.started_at(),
                store_root,
                shutdown,
                active_requests: state.active_requests(),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                shutdown.cancel();
                let _ = thread.join();
                token_file.remove();
                Err(error)
            }
            Err(_) => {
                shutdown.cancel();
                let _ = thread.join();
                token_file.remove();
                Err(DaemonError::Timeout("waiting for the daemon endpoint"))
            }
        }
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    pub fn token_file(&self) -> &Path {
        &self.paths.token_file
    }

    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    pub fn store_root(&self) -> &str {
        &self.store_root
    }

    /// Wait for the runtime thread to return after a `daemon_shutdown` or a signal. If the thread
    /// already exited, this returns its result.
    pub fn wait(mut self) -> Result<(), DaemonError> {
        match self.thread.take() {
            Some(thread) => thread.join().map_err(|_| DaemonError::Runtime("the daemon thread panicked".to_owned()))?,
            None => Ok(()),
        }
    }

    /// Ask the daemon to stop and wait for the runtime thread. Idempotent.
    pub fn shutdown(mut self) -> Result<(), DaemonError> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<(), DaemonError> {
        if let Ok(active) = self.active_requests.lock() {
            for flag in active.values() {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.shutdown.cancel();
        match self.thread.take() {
            Some(thread) => thread.join().map_err(|_| DaemonError::Runtime("the daemon thread panicked".to_owned()))?,
            None => Ok(()),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
}
