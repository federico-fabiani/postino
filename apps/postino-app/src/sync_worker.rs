//! One background thread for IMAP sync.
//!
//! The UI never blocks on the network. The worker owns its own store connection: on every run it
//! opens a [`Store`] for reconciliation and an [`Engine`] with the credential vault through
//! [`Engine::open_with_vault`], then reloads both from disk before the next command. It sends only
//! the four documented events and never touches egui.
//!
//! `Engine::sync_now` is deliberately not used: it creates an internal cancellation flag and offers
//! no progress callback, so a Cancel button would be a lie and a determinate progress bar would be
//! impossible. The worker drives `postino_sync::sync_account` directly with the engine's own account
//! and server rows and the same vault; that is the only way to keep cancellation and progress honest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use postino_core::SyncReport;
use postino_net::{CertificateDer, ImapConfig, NetError, Secret, TlsMode};
use postino_store::Store;
use postino_sync::{SyncError, SyncLimits, SyncPhase};

use crate::boot::StoreHandle;

/// One determinate progress sample. `folders_total` is zero until the server has listed its folders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncProgress {
    pub folders_done: u32,
    pub folders_total: u32,
    pub messages: u32,
}

impl SyncProgress {
    /// A fraction in `0.0..=1.0`, or `None` while the total is unknown.
    pub fn fraction(self) -> Option<f32> {
        if self.folders_total == 0 {
            return None;
        }
        let done = f32::from(u16::try_from(self.folders_done.min(self.folders_total)).unwrap_or(u16::MAX));
        let total = f32::from(u16::try_from(self.folders_total).unwrap_or(u16::MAX));
        Some((done / total).clamp(0.0, 1.0))
    }
}

/// What the worker sends to the UI. Account ids are opaque; no address, host or body is included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    Started { account: Option<String> },
    Progress { account: Option<String>, progress: SyncProgress },
    Finished { report: SyncReport },
    Failed { account: Option<String>, message: String, cancelled: bool, offline: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncCommand {
    SyncNow { account: Option<String> },
    Cancel,
    Stop,
}

/// The result the UI gets when it asks for a sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncRequest {
    Started,
    AlreadyRunning,
}

/// A failure already reduced to Italian copy and three honest flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncFailure {
    pub message: String,
    pub cancelled: bool,
    /// DNS, TCP, timeout or a closed connection: cached mail stays readable and the app says
    /// "offline" rather than "error".
    pub offline: bool,
}

impl SyncFailure {
    fn new(message: impl Into<String>, cancelled: bool, offline: bool) -> Self {
        Self { message: message.into(), cancelled, offline }
    }
}

pub type ProgressSink = dyn Fn(SyncProgress) + Send + Sync;

/// The seam tests replace: a backend blocks inside the worker thread and must respect `cancel`.
pub trait SyncBackend: Send {
    fn run(&mut self, account: Option<&str>, cancel: &Arc<AtomicBool>, progress: Arc<ProgressSink>) -> Result<SyncReport, SyncFailure>;
}

/// The production backend. It opens its own SQLCipher connection and its own engine inside the
/// worker thread, never sharing the UI's `Engine`.
pub struct StoreSyncBackend {
    handle: StoreHandle,
    extra_roots: Vec<CertificateDer<'static>>,
}

impl StoreSyncBackend {
    pub fn new(handle: StoreHandle, extra_roots: Vec<CertificateDer<'static>>) -> Self {
        Self { handle, extra_roots }
    }

    fn open_config(&self, account_id: Option<&str>) -> Result<(Store, postino_store::Account, ImapConfig, String), SyncFailure> {
        let engine =
            self.handle.open_engine().map_err(|error| SyncFailure::new(format!("Non riesco ad aprire l'archivio locale: {error}"), false, false))?;
        let account = match account_id {
            Some(id) => engine.accounts().iter().find(|account| account.id == id).cloned(),
            None if engine.accounts().len() == 1 => engine.accounts().first().cloned(),
            None if engine.accounts().is_empty() => {
                return Err(SyncFailure::new("Nessun account configurato.", false, false));
            }
            None => return Err(SyncFailure::new("Ci sono più account: scegli quello da sincronizzare.", false, false)),
        }
        .ok_or_else(|| SyncFailure::new("L'account scelto non esiste più nell'archivio locale.", false, false))?;
        let view =
            engine.account_view(&account).map_err(|error| SyncFailure::new(format!("Non riesco a leggere l'account: {error}"), false, false))?;
        let server = view.server.ok_or_else(|| SyncFailure::new("L'account non ha un server IMAP configurato.", false, false))?;
        let store =
            self.handle.open_store().map_err(|error| SyncFailure::new(format!("Non riesco ad aprire l'archivio locale: {error}"), false, false))?;
        let row = store
            .get_account(&account.id)
            .map_err(|error| SyncFailure::new(format!("Non riesco a leggere l'account: {error}"), false, false))?
            .ok_or_else(|| SyncFailure::new("L'account non è più nell'archivio locale.", false, false))?;
        let bytes = self
            .handle
            .vault()
            .load_imap_password(&account.id)
            .map_err(|error| SyncFailure::new(format!("Il portachiavi non fornisce la password: {error}"), false, false))?;
        let Some(bytes) = bytes else {
            return Err(SyncFailure::new("La password dell'account non è nel portachiavi: rimuovi l'account e aggiungilo di nuovo.", false, false));
        };
        let text = std::str::from_utf8(bytes.as_slice())
            .map_err(|_| SyncFailure::new("La password salvata non è leggibile: rimuovi l'account e aggiungilo di nuovo.", false, false))?;
        let tls = match server.tls {
            postino_core::MailTls::Implicit => TlsMode::Implicit,
            postino_core::MailTls::StartTls => TlsMode::StartTls,
        };
        let mut config = ImapConfig::new(server.host.clone(), server.port, tls, server.username.clone(), Secret::new(text));
        for root in self.extra_roots.clone() {
            config = config.with_extra_root(root);
        }
        Ok((store, row, config, account.id))
    }
}

impl SyncBackend for StoreSyncBackend {
    fn run(&mut self, account: Option<&str>, cancel: &Arc<AtomicBool>, progress: Arc<ProgressSink>) -> Result<SyncReport, SyncFailure> {
        let (store, row, config, account_id) = self.open_config(account)?;
        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let done = Arc::clone(&done);
            let handle = self.handle.clone();
            let account_id = account_id.clone();
            std::thread::spawn(move || {
                let Ok(store) = handle.open_store() else {
                    return;
                };
                let mut last = SyncProgress::default();
                while !done.load(Ordering::Relaxed) {
                    if let Ok(Some(status)) = postino_sync::read_status_for(&store, &account_id)
                        && status.state == SyncPhase::Syncing
                    {
                        let sample =
                            SyncProgress { folders_done: status.folders_done, folders_total: status.folders_total, messages: status.messages };
                        if sample != last {
                            progress(sample);
                            last = sample;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
            })
        };
        let result = postino_sync::sync_account(&store, &row, &config, &SyncLimits::default(), cancel);
        done.store(true, Ordering::Relaxed);
        let _ = watcher.join();
        // The worker reloads its own engine from the store before reporting, so a successful run is
        // proven readable from disk, not just acknowledged by the network layer.
        let reload = self.handle.open_engine();
        match result {
            Ok(outcome) => match reload {
                Ok(_) => Ok(SyncReport::from(outcome)),
                Err(error) => Err(SyncFailure::new(format!("La sincronizzazione è riuscita ma l'archivio non si riapre: {error}"), false, false)),
            },
            Err(error) => Err(failure_from_sync(error)),
        }
    }
}

/// The daemon-backed sync (P10a2). The window asks the daemon for `sync_now` over `postino-ipc`;
/// the daemon owns the IMAP session, the write connection and the store lock. Cancellation is a
/// second connection: the protocol's `cancel` is accepted across connections, so the window never
/// waits for the running request to return before it can be stopped.
pub struct DaemonSyncBackend {
    store: std::path::PathBuf,
}

impl DaemonSyncBackend {
    pub fn new(store: std::path::PathBuf) -> Self {
        Self { store }
    }
}

impl SyncBackend for DaemonSyncBackend {
    fn run(&mut self, account: Option<&str>, cancel: &Arc<AtomicBool>, _progress: Arc<ProgressSink>) -> Result<SyncReport, SyncFailure> {
        let paths = crate::daemon::paths(&self.store).map_err(|message| SyncFailure::new(message, false, true))?;
        let mut client = postino_ipc::client::BlockingClient::connect_with(
            &paths,
            crate::daemon::CLIENT_NAME,
            true,
            Duration::from_secs(2),
            Duration::from_secs(600),
        )
        .map_err(|error| SyncFailure::new(crate::daemon::describe(&error), false, error.is_absent()))?;
        let params = serde_json::json!({ "name": "sync_now", "args": { "account": account } });
        let id = client
            .start_request(postino_ipc::protocol::method::OP, params)
            .map_err(|error| SyncFailure::new(crate::daemon::describe(&error), false, error.is_absent()))?;

        // Stop the daemon's request through a second connection while the first one is still
        // waiting. A failure here only means the sync runs to the end; it never invents a result.
        let cancel_flag = Arc::clone(cancel);
        let cancel_paths = paths.clone();
        let request = id.to_string();
        let done = Arc::new(AtomicBool::new(false));
        let done_thread = Arc::clone(&done);
        let canceller = std::thread::Builder::new().name("postino-app-cancel".to_owned()).spawn(move || {
            loop {
                if done_thread.load(Ordering::Relaxed) {
                    return;
                }
                if !cancel_flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                if let Ok(mut second) = postino_ipc::client::BlockingClient::connect(
                    &cancel_paths,
                    crate::daemon::CLIENT_NAME,
                    Duration::from_secs(2),
                    Duration::from_secs(10),
                ) {
                    let _ = second.call(postino_ipc::protocol::method::CANCEL, serde_json::json!({ "request": request }), Duration::from_secs(10));
                }
                // The cancelled request returns on its own; this thread has nothing left to do.
                return;
            }
        });

        let result = client.wait_for(id, Duration::from_secs(600));
        done.store(true, Ordering::Relaxed);
        drop(canceller);
        match result {
            Ok(value) => serde_json::from_value::<SyncReport>(value)
                .map_err(|_| SyncFailure::new("Il daemon ha risposto in un modo che questa finestra non sa leggere.", false, false)),
            Err(error) => {
                if matches!(error, postino_ipc::IpcError::Cancelled) {
                    Err(SyncFailure::new("Sincronizzazione annullata. I messaggi già salvati restano disponibili.", true, false))
                } else {
                    Err(SyncFailure::new(crate::daemon::describe(&error), false, error.is_absent()))
                }
            }
        }
    }
}

/// Reduce a typed sync error to Italian copy. Server-supplied text is never shown: a hostile or
/// broken server can echo a credential in a tagged reply.
pub fn failure_from_sync(error: SyncError) -> SyncFailure {
    match error {
        SyncError::Auth(_) => SyncFailure::new("Il server ha rifiutato le credenziali. Controlla la password o l'app password.", false, false),
        SyncError::Connect(NetError::Dns { .. }) => SyncFailure::new("Non trovo il server. Controlla la connessione.", false, true),
        SyncError::Connect(NetError::Connect { .. }) => SyncFailure::new("Il server non risponde. Controlla la connessione.", false, true),
        SyncError::Connect(NetError::Timeout { .. }) => SyncFailure::new("Il server non risponde in tempo. Riprova tra poco.", false, true),
        SyncError::Connect(NetError::Closed { .. }) => SyncFailure::new("La connessione si è chiusa prima della fine.", false, true),
        SyncError::Connect(NetError::Tls { .. }) => {
            SyncFailure::new("La connessione TLS non è riuscita. Controlla host, porta e modalità TLS.", false, false)
        }
        SyncError::Connect(NetError::ServerNo { .. } | NetError::ServerBad { .. }) => {
            SyncFailure::new("Il server ha rifiutato la sessione IMAP. Controlla che IMAP sia attivo e che la password sia corretta.", false, false)
        }
        SyncError::Connect(_) | SyncError::Protocol(_) => {
            SyncFailure::new("Il server ha risposto in un modo che Postino non sa usare. Riprova più tardi.", false, false)
        }
        SyncError::Store(_) => SyncFailure::new("L'archivio locale ha rifiutato un aggiornamento.", false, false),
        SyncError::Cancelled => SyncFailure::new("Sincronizzazione annullata. I messaggi già salvati restano disponibili.", true, false),
        SyncError::NoAccount(_) => SyncFailure::new("L'account non è più nell'archivio locale.", false, false),
    }
}

/// Read one persisted status without a network call. The fixed English sentence written by the sync
/// crate becomes the same Italian category the live failure path uses.
pub fn persisted_reason(error: &str) -> String {
    if error.contains("rejected the credentials") {
        "credenziali rifiutate".to_owned()
    } else if error.contains("could not talk to the server") {
        "server non raggiungibile".to_owned()
    } else if error.contains("local store") {
        "archivio locale non disponibile".to_owned()
    } else if error.contains("cancelled") {
        "sincronizzazione annullata".to_owned()
    } else if error.contains("not in the local store") {
        "account non trovato".to_owned()
    } else if error.contains("could not use") {
        "risposta del server non utilizzabile".to_owned()
    } else {
        "sincronizzazione non riuscita".to_owned()
    }
}

/// The worker handle owned by the UI. The thread exits on `Stop`, on a dropped sender or when the
/// app drops the handle; no task outlives the window.
pub struct SyncWorker {
    commands: Sender<SyncCommand>,
    events: Receiver<SyncEvent>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    running: bool,
}

impl SyncWorker {
    pub fn start(backend: Box<dyn SyncBackend>) -> Self {
        let (commands_tx, commands) = std::sync::mpsc::channel();
        let (events_tx, events) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let handle = std::thread::spawn(move || worker_loop(backend, commands, events_tx, thread_cancel));
        Self { commands: commands_tx, events, cancel, handle: Some(handle), running: false }
    }

    /// Ask for a sync. A second request while the first is running is refused; the UI turns that
    /// refusal into the documented sentence instead of queueing a surprise run.
    pub fn request(&mut self, account: Option<String>) -> SyncRequest {
        if self.running {
            return SyncRequest::AlreadyRunning;
        }
        if self.commands.send(SyncCommand::SyncNow { account }).is_err() {
            return SyncRequest::AlreadyRunning;
        }
        self.running = true;
        SyncRequest::Started
    }

    /// Cooperative cancel. The shared flag reaches the sync loop before the queued command does, so
    /// a network phase that is already in flight stops at the next safe point.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.commands.send(SyncCommand::Cancel);
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Drain the event channel. Called once per frame; never blocks.
    pub fn poll(&mut self) -> Vec<SyncEvent> {
        let mut events = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(event) => {
                    if matches!(event, SyncEvent::Finished { .. } | SyncEvent::Failed { .. }) {
                        self.running = false;
                    }
                    events.push(event);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = false;
                    break;
                }
            }
        }
        events
    }

    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.commands.send(SyncCommand::Stop);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            log::warn!("the sync worker panicked; the mailbox on disk is unchanged");
        }
    }
}

impl Drop for SyncWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker_loop(backend: Box<dyn SyncBackend>, commands: Receiver<SyncCommand>, events: Sender<SyncEvent>, cancel: Arc<AtomicBool>) {
    let mut backend = backend;
    let mut pending: Option<Option<String>> = None;
    loop {
        let command = match pending.take() {
            Some(account) => SyncCommand::SyncNow { account },
            None => match commands.recv() {
                Ok(command) => command,
                Err(_) => return,
            },
        };
        match command {
            SyncCommand::Stop => return,
            SyncCommand::Cancel => cancel.store(true, Ordering::SeqCst),
            SyncCommand::SyncNow { account } => {
                let mut stop = false;
                // Coalesce everything that arrived while the worker was idle: several SyncNow
                // commands become one run, a Cancel is honoured, Stop wins over both.
                loop {
                    match commands.try_recv() {
                        Ok(SyncCommand::SyncNow { account }) => pending = Some(account),
                        Ok(SyncCommand::Cancel) => cancel.store(true, Ordering::SeqCst),
                        Ok(SyncCommand::Stop) => {
                            stop = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            stop = true;
                            break;
                        }
                    }
                }
                if stop {
                    return;
                }
                cancel.store(false, Ordering::SeqCst);
                if events.send(SyncEvent::Started { account: account.clone() }).is_err() {
                    return;
                }
                let progress = {
                    let events = events.clone();
                    let account = account.clone();
                    let sink: Arc<ProgressSink> = Arc::new(move |progress: SyncProgress| {
                        let _ = events.send(SyncEvent::Progress { account: account.clone(), progress });
                    });
                    sink
                };
                let result = backend.run(account.as_deref(), &cancel, progress);
                cancel.store(false, Ordering::SeqCst);
                let event = match result {
                    Ok(report) => SyncEvent::Finished { report },
                    Err(failure) => SyncEvent::Failed {
                        account: account.clone(),
                        message: failure.message,
                        cancelled: failure.cancelled,
                        offline: failure.offline,
                    },
                };
                if events.send(event).is_err() {
                    return;
                }
                // A command that arrived during the run is handled next. Multiple sync requests
                // collapse into the most recent one, so a burst can never start a chain of runs.
                loop {
                    match commands.try_recv() {
                        Ok(SyncCommand::SyncNow { account }) => pending = Some(account),
                        Ok(SyncCommand::Cancel) => cancel.store(true, Ordering::SeqCst),
                        Ok(SyncCommand::Stop) => return,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::RecvTimeoutError;

    use super::*;

    enum Script {
        Ok { progress: Vec<SyncProgress> },
        Fail { message: &'static str, offline: bool },
        WaitForCancel,
    }

    struct TestBackend {
        script: Script,
        reported_progress: std::sync::mpsc::Sender<SyncProgress>,
    }

    impl SyncBackend for TestBackend {
        fn run(&mut self, account: Option<&str>, cancel: &Arc<AtomicBool>, progress: Arc<ProgressSink>) -> Result<SyncReport, SyncFailure> {
            match &self.script {
                Script::Ok { progress: samples } => {
                    for sample in samples {
                        progress(*sample);
                        let _ = self.reported_progress.send(*sample);
                    }
                    Ok(SyncReport {
                        account: account.unwrap_or("acc_test").to_owned(),
                        folders: 3,
                        fetched: 12,
                        updated_flags: 0,
                        removed: 0,
                        attachments_stored: 0,
                        warnings: Vec::new(),
                    })
                }
                Script::Fail { message, offline } => Err(SyncFailure::new(*message, false, *offline)),
                Script::WaitForCancel => {
                    for _ in 0..200 {
                        if cancel.load(Ordering::SeqCst) {
                            return Err(failure_from_sync(SyncError::Cancelled));
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(SyncFailure::new("il test non ha ricevuto l'annullamento", false, false))
                }
            }
        }
    }

    fn worker(script: Script) -> (SyncWorker, std::sync::mpsc::Receiver<SyncProgress>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (SyncWorker::start(Box::new(TestBackend { script, reported_progress: tx })), rx)
    }

    fn next(worker: &mut SyncWorker) -> SyncEvent {
        match worker.events.recv_timeout(Duration::from_secs(2)) {
            Ok(event) => {
                if matches!(event, SyncEvent::Finished { .. } | SyncEvent::Failed { .. }) {
                    worker.running = false;
                }
                event
            }
            Err(error) => panic!("no worker event: {error}"),
        }
    }

    #[test]
    fn a_sync_reports_started_progress_and_finished() {
        let samples =
            vec![SyncProgress { folders_done: 1, folders_total: 3, messages: 4 }, SyncProgress { folders_done: 3, folders_total: 3, messages: 12 }];
        let (mut worker, reported) = worker(Script::Ok { progress: samples.clone() });
        assert_eq!(worker.request(Some("acc_test".to_owned())), SyncRequest::Started);
        assert!(matches!(next(&mut worker), SyncEvent::Started { .. }));
        assert!(matches!(next(&mut worker), SyncEvent::Progress { .. }));
        assert!(matches!(next(&mut worker), SyncEvent::Progress { .. }));
        match next(&mut worker) {
            SyncEvent::Finished { report } => assert_eq!(report.fetched, 12),
            other => panic!("unexpected event {other:?}"),
        }
        assert!(!worker.is_running());
        assert_eq!(reported.recv_timeout(Duration::from_secs(1)).expect("first sample"), samples[0]);
    }

    #[test]
    fn a_second_request_while_running_is_refused() {
        let (mut worker, _) = worker(Script::WaitForCancel);
        assert_eq!(worker.request(None), SyncRequest::Started);
        assert_eq!(worker.request(None), SyncRequest::AlreadyRunning);
        assert!(matches!(next(&mut worker), SyncEvent::Started { .. }));
        worker.cancel();
        match next(&mut worker) {
            SyncEvent::Failed { cancelled, message, .. } => {
                assert!(cancelled);
                assert!(message.contains("annullata"));
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(!worker.is_running());
    }

    #[test]
    fn an_offline_failure_is_flagged_and_says_so() {
        let (mut worker, _) = worker(Script::Fail { message: "Il server non risponde. Controlla la connessione.", offline: true });
        assert_eq!(worker.request(None), SyncRequest::Started);
        assert!(matches!(next(&mut worker), SyncEvent::Started { .. }));
        match next(&mut worker) {
            SyncEvent::Failed { offline, cancelled, message, .. } => {
                assert!(offline);
                assert!(!cancelled);
                assert!(message.contains("non risponde"));
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn queued_requests_collapse_into_one_run() {
        let (mut worker, _) = worker(Script::WaitForCancel);
        assert_eq!(worker.request(None), SyncRequest::Started);
        assert!(matches!(next(&mut worker), SyncEvent::Started { .. }));
        // The UI refuses the second request; the raw channel is the worker's own coalescing test.
        worker.commands.send(SyncCommand::SyncNow { account: None }).expect("queued request");
        worker.cancel();
        assert!(matches!(next(&mut worker), SyncEvent::Failed { cancelled: true, .. }));
        assert!(matches!(next(&mut worker), SyncEvent::Started { .. }), "the queued request runs once after the first");
        worker.cancel();
        assert!(matches!(next(&mut worker), SyncEvent::Failed { cancelled: true, .. }));
        assert!(!worker.is_running());
    }

    #[test]
    fn progress_fraction_is_determinate_only_with_a_total() {
        assert_eq!(SyncProgress::default().fraction(), None);
        assert_eq!(SyncProgress { folders_done: 1, folders_total: 4, messages: 0 }.fraction(), Some(0.25));
        assert_eq!(SyncProgress { folders_done: 9, folders_total: 4, messages: 0 }.fraction(), Some(1.0));
    }

    #[test]
    fn a_cancelled_sync_is_not_reported_as_an_error() {
        let failure = failure_from_sync(SyncError::Cancelled);
        assert!(failure.cancelled);
        assert!(!failure.offline);
    }

    #[test]
    fn network_failures_are_offline_and_tls_is_not() {
        let dns = failure_from_sync(SyncError::Connect(NetError::Dns { host: "mail.example".to_owned(), reason: "no".to_owned() }));
        assert!(dns.offline, "DNS is a connectivity failure");
        let tls = failure_from_sync(SyncError::Connect(NetError::Tls { host: "mail.example".to_owned(), reason: "bad cert".to_owned() }));
        assert!(!tls.offline, "a certificate problem is not being offline");
        assert!(tls.message.contains("TLS"));
    }

    #[test]
    fn the_event_channel_never_carries_an_address_or_a_password() {
        let failure = failure_from_sync(SyncError::Auth("user@example.com secret".to_owned()));
        assert!(!failure.message.contains('@'));
        assert!(!failure.message.contains("secret"));
        assert_eq!(persisted_reason("the server rejected the credentials"), "credenziali rifiutate");
        assert_eq!(persisted_reason("could not talk to the server"), "server non raggiungibile");
    }

    #[test]
    fn the_worker_stops_at_eof_and_joins() {
        let (mut worker, _) = worker(Script::Ok { progress: Vec::new() });
        drop(worker.commands.send(SyncCommand::SyncNow { account: None }));
        let _ = worker.poll();
        worker.stop();
    }

    #[test]
    fn recv_timeout_is_used_only_by_the_test_helper() {
        // Keeps the import meaningful without changing the production path.
        let (_tx, rx) = std::sync::mpsc::channel::<u8>();
        assert!(matches!(rx.recv_timeout(Duration::from_millis(1)), Err(RecvTimeoutError::Timeout)));
    }
}
