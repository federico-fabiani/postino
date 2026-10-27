//! The desktop window as a client of `postino-daemon`.
//!
//! One writer per store. When a daemon answers on this store, the window keeps the encrypted store
//! open **read-only**, holds no writer lock and sends every mutation through `postino-ipc`; the
//! daemon owns the write connection, the network sessions, the outbox lease and the index. Without
//! a daemon the window keeps the P10a1 in-process mode, visibly labelled "Solo questa finestra",
//! and holds the writer lock exactly as before.
//!
//! Nothing here starts or stops a process on its own: [`start`] and [`stop`] run only from an
//! explicit user command. A lost connection is a normal state with a bounded backoff, never a
//! crash: the view state and the composer live in the app, so a reconnect only replaces the
//! engine.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use postino_ipc::client::BlockingClient;
use postino_ipc::{DaemonPaths, IpcError};
use serde_json::{Value, json};

/// How the handshake names this window. The daemon admits it for sensitive operations and for
/// sends; MCP is the only client that may be refused.
pub const CLIENT_NAME: &str = "app";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(900);
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// A settings read is a small encrypted-file read. It must never freeze a frame behind a wedged
/// daemon: the settings page shows "unreachable" and the page stays interactive.
pub const READ_TIMEOUT: Duration = Duration::from_millis(1500);
const EVENT_WAIT: Duration = Duration::from_millis(250);
/// Bounded reconnect backoff: five steps, then the last one repeats. Never a busy loop.
const BACKOFF_MS: [u64; 5] = [250, 500, 1_000, 2_000, 4_000];
/// After this many failed attempts the state stops saying "reconnecting" and says "lost".
const LOST_AFTER: u32 = 4;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Windows `CREATE_NO_WINDOW`: the daemon is a console binary and must not flash a console.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The daemon identity from the handshake. No token, no endpoint, no path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub pid: u32,
    pub version: String,
}

/// What the window knows about its own mode. Rendered as one status-bar chip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Connection {
    /// Synthetic mailbox: no store, no daemon, no IPC.
    Demo,
    /// The window owns the store and the writer lock.
    InProcess,
    /// A live daemon answers; the window is read/write through IPC only.
    Connected(Hello),
    /// The connection dropped and the worker is retrying with a bounded backoff.
    Reconnecting { attempt: u32 },
    /// Every bounded attempt failed. The window stays read-only and keeps retrying slowly.
    Lost,
}

impl Connection {
    /// True when the store belongs to a daemon, so this window may not open it for writing even
    /// while the channel is down.
    pub fn remote(&self) -> bool {
        !matches!(self, Self::Demo | Self::InProcess)
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Connected(_))
    }

    /// The status-bar chip. Short by design: one phrase, no pid.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Demo => "Modalità demo",
            Self::InProcess => "Solo questa finestra",
            Self::Connected(_) => "Daemon",
            Self::Reconnecting { .. } => "Daemon · riconnessione",
            Self::Lost => "Daemon · non raggiungibile",
        }
    }

    /// The one sentence that explains the mode, used by the status bar hover and the palette.
    pub fn detail(&self) -> String {
        match self {
            Self::Demo => "Archive sintetico: nessun archivio locale, nessuna rete, nessun daemon.".to_owned(),
            Self::InProcess => {
                "Questa finestra possiede l'archivio: sincronizza, invia e applica le regole da sola. Con la finestra chiusa non succede nulla."
                    .to_owned()
            }
            Self::Connected(hello) => format!(
                "L'archivio è del daemon {} (pid {}): questa finestra legge l'archivio e scrive solo attraverso il canale locale.",
                hello.version, hello.pid
            ),
            Self::Reconnecting { attempt } => {
                format!("Canale locale interrotto, tentativo {attempt}. La posta resta leggibile; le modifiche ripartono quando il daemon risponde.")
            }
            Self::Lost => {
                "Il daemon non risponde più. La posta resta leggibile; riavvialo con «Avvia daemon» oppure riapri l'archivio solo in questa finestra."
                    .to_owned()
            }
        }
    }

    /// Starting a daemon needs an explicit command, an archive and no live daemon.
    pub fn can_start(&self, demo: bool) -> bool {
        !demo && !self.is_live()
    }

    /// Stopping needs a live daemon: while reconnecting the channel cannot carry the request.
    pub fn can_stop(&self) -> bool {
        self.is_live()
    }

    pub fn reason_start(&self, demo: bool) -> Option<&'static str> {
        if demo {
            Some("La modalità demo non avvia processi")
        } else if self.is_live() {
            Some("Il daemon è già in esecuzione")
        } else {
            None
        }
    }

    pub fn reason_stop(&self) -> Option<&'static str> {
        if self.can_stop() { None } else { Some("Nessun daemon collegato") }
    }
}

/// One sentence for a transport failure. Every variant is already free of tokens, paths and
/// message content; this only turns it into Italian and says what the user can do.
pub fn describe(error: &IpcError) -> String {
    match error {
        IpcError::Unauthorized => "Il daemon ha rifiutato questa finestra: riavvialo e riprova.".to_owned(),
        IpcError::Version { .. } => "La versione del daemon non è compatibile con questa finestra: aggiorna Postino.".to_owned(),
        IpcError::Busy(message) => format!("Il daemon sta completando un'altra richiesta: {message}"),
        IpcError::Cancelled => "Richiesta annullata.".to_owned(),
        IpcError::Overloaded => "Il daemon ha troppe richieste in corso: riprova tra un istante.".to_owned(),
        IpcError::FrameTooLarge { limit } => {
            format!("La richiesta o la risposta supera il limite di {limit} byte del canale locale: allega il file dalla CLI `postino` o riducilo.")
        }
        IpcError::InsecureTokenFile(_) => "Il file del token del daemon non è privato: il canale locale è rifiutato.".to_owned(),
        IpcError::TokenFile(_) => "Il token del daemon non è disponibile: avvia il daemon e riprova.".to_owned(),
        IpcError::Rpc(error) => error.message.clone(),
        other if other.is_absent() => absent_sentence(),
        other => other.to_string(),
    }
}

/// The one sentence for "there is no daemon", used by the boot probe and by every refusal.
pub fn absent_sentence() -> String {
    "Il daemon non è in esecuzione: avvia `postino daemon start` oppure continua solo in questa finestra.".to_owned()
}

/// The answer to a mutation whose reply was lost: the same request is never sent twice, because a
/// second `account_add`/`draft_save`/`message_send` would apply it twice. The window says what is
/// uncertain and leaves the verification to the person.
pub fn uncertain_sentence(name: &str) -> String {
    format!(
        "Il canale locale si è interrotto durante `{name}`: la modifica potrebbe essere stata applicata. Riapri l'elenco per verificare prima di ripetere."
    )
}

/// The transport the window needs. A trait keeps the tests off the real pipe and keeps the app
/// free of a second runtime: [`IpcTransport`] wraps the blocking IPC client.
pub trait Transport: Send {
    /// Connect and subscribe to the daemon events. Called again after every loss.
    fn connect(&mut self) -> Result<Hello, String>;
    /// Send one operation. `retry` is true only for an operation the registry calls read-only: a
    /// mutation whose answer was lost must never be replayed automatically.
    fn call(&mut self, name: &str, args: Value, retry: bool) -> Result<Value, String>;
    /// Same contract as [`Transport::call`] with an explicit bound. The default keeps recorder
    /// transports simple; the real client passes the bound to the pipe read.
    fn call_bounded(&mut self, name: &str, args: Value, retry: bool, timeout: Duration) -> Result<Value, String> {
        let _ = timeout;
        self.call(name, args, retry)
    }
    fn status(&mut self) -> Result<Value, String>;
    fn shutdown(&mut self) -> Result<(), String>;
    fn next_event(&mut self, wait: Duration) -> Result<Option<Value>, String>;
}

/// The real client: one `postino-ipc` connection, reconnected lazily after a loss.
pub struct IpcTransport {
    paths: DaemonPaths,
    client: Option<BlockingClient>,
}

impl IpcTransport {
    pub fn new(paths: DaemonPaths) -> Self {
        Self { paths, client: None }
    }

    fn ensure(&mut self) -> Result<&mut BlockingClient, String> {
        if self.client.is_none() {
            let client = BlockingClient::connect_with(&self.paths, CLIENT_NAME, true, CONNECT_TIMEOUT, CALL_TIMEOUT)
                .map_err(|error| absent_or(&self.paths, &error))?;
            self.client = Some(client);
        }
        match self.client.as_mut() {
            Some(client) => Ok(client),
            // The branch above always fills a `None`, so this is unreachable without a bug; it is
            // an error, never a panic.
            None => Err(absent_sentence()),
        }
    }
}

impl Transport for IpcTransport {
    fn connect(&mut self) -> Result<Hello, String> {
        self.client = None;
        let client = self.ensure()?;
        client.subscribe(STATUS_TIMEOUT).map_err(|error| describe(&error))?;
        let hello = client.hello();
        Ok(Hello { pid: hello.pid, version: hello.daemon_version.clone() })
    }

    fn call(&mut self, name: &str, args: Value, retry: bool) -> Result<Value, String> {
        self.call_bounded(name, args, retry, CALL_TIMEOUT)
    }

    fn call_bounded(&mut self, name: &str, args: Value, retry: bool, timeout: Duration) -> Result<Value, String> {
        let params = json!({ "name": name, "args": args });
        let method = postino_ipc::protocol::method::OP;
        // A retry of a mutation whose answer never arrived can apply it twice (a second account, a
        // second draft, a second send). Only a read is replayed; a mutation reports the uncertainty.
        let client = self.ensure()?;
        let attempt = client.call(method, params.clone(), timeout);
        match attempt {
            Ok(value) => Ok(value),
            Err(error) if error.is_retryable() && retry => {
                // A dropped connection is normal: retry once on a fresh one.
                self.client = None;
                self.ensure()?.call(method, params, timeout).map_err(|error| describe(&error))
            }
            Err(error) if error.is_retryable() => {
                self.client = None;
                Err(uncertain_sentence(name))
            }
            Err(error) => Err(describe(&error)),
        }
    }

    fn status(&mut self) -> Result<Value, String> {
        let client = self.ensure()?;
        client.call(postino_ipc::protocol::method::DAEMON_STATUS, json!({}), STATUS_TIMEOUT).map_err(|error| describe(&error))
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let client = self.ensure()?;
        client.call(postino_ipc::protocol::method::DAEMON_SHUTDOWN, json!({}), STATUS_TIMEOUT).map(|_| ()).map_err(|error| describe(&error))
    }

    fn next_event(&mut self, wait: Duration) -> Result<Option<Value>, String> {
        let client = self.ensure()?;
        client.next_event(wait).map_err(|error| describe(&error))
    }
}

/// The synchronous write path of the window: one operation, sent as `op {name, args}` exactly as
/// the CLI and MCP do. Never a local write, never a second registry.
pub struct Link {
    transport: Box<dyn Transport>,
}

impl Link {
    pub fn connect(store: &Path) -> Result<Self, String> {
        Ok(Self { transport: Box::new(IpcTransport::new(paths(store)?)) })
    }

    /// Test seam: the same write path with a recorder or a fake daemon.
    #[cfg(test)]
    pub fn with_transport(transport: Box<dyn Transport>) -> Self {
        Self { transport }
    }

    pub fn call(&mut self, name: &str, args: Value) -> Result<Value, String> {
        self.transport.call(name, args, is_read_only(name))
    }

    /// A read with a short bound: settings must not hold a frame behind a wedged daemon.
    pub fn call_bounded(&mut self, name: &str, args: Value, timeout: Duration) -> Result<Value, String> {
        self.transport.call_bounded(name, args, is_read_only(name), timeout)
    }

    pub fn status(&mut self) -> Result<Value, String> {
        self.transport.status()
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        self.transport.shutdown()
    }
}

/// One reaction from the event thread. `Daemon` carries the event body the daemon published.
#[derive(Debug)]
pub enum Event {
    Connected(Hello),
    Reconnecting { attempt: u32 },
    Lost,
    Daemon(Value),
}

/// The subscription thread: subscribe, forward every event, reconnect with a bounded backoff.
pub struct EventWorker {
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EventWorker {
    pub fn start(transport: Box<dyn Transport>) -> Self {
        let (sender, events) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new().name("postino-app-events".to_owned()).spawn(move || event_loop(transport, sender, thread_stop)).ok();
        Self { events, stop, handle }
    }

    /// Non-blocking: one frame drains what the thread produced since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return events,
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            log::warn!("the daemon event thread ended abnormally");
        }
    }
}

impl Drop for EventWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn event_loop(mut transport: Box<dyn Transport>, events: Sender<Event>, stop: Arc<AtomicBool>) {
    let mut attempt = 0u32;
    let mut need_connect = true;
    while !stop.load(Ordering::Relaxed) {
        if need_connect {
            match transport.connect() {
                Ok(hello) => {
                    attempt = 0;
                    need_connect = false;
                    if events.send(Event::Connected(hello)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    attempt = attempt.saturating_add(1);
                    let index = usize::try_from(attempt.saturating_sub(1)).unwrap_or(0).min(BACKOFF_MS.len() - 1);
                    let wait = Duration::from_millis(BACKOFF_MS[index]);
                    let event = if attempt >= LOST_AFTER { Event::Lost } else { Event::Reconnecting { attempt } };
                    if events.send(event).is_err() {
                        return;
                    }
                    sleep_unless_stopped(&stop, wait);
                    continue;
                }
            }
        }
        match transport.next_event(EVENT_WAIT) {
            Ok(Some(event)) => {
                if events.send(Event::Daemon(event)).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(_) => {
                need_connect = true;
                attempt = 0;
                if events.send(Event::Reconnecting { attempt: 1 }).is_err() {
                    return;
                }
                sleep_unless_stopped(&stop, Duration::from_millis(BACKOFF_MS[0]));
            }
        }
    }
}

fn sleep_unless_stopped(stop: &AtomicBool, wait: Duration) {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Per-store endpoint and token file, from `postino-ipc`: the app derives no name of its own.
pub fn paths(store: &Path) -> Result<DaemonPaths, String> {
    DaemonPaths::for_store(store).map_err(|error| format!("Il canale locale non è disponibile: {error}"))
}

/// The identity of a live daemon, or `None` when none answers. Used by the boot probe: absent is
/// normal, not an error.
pub fn probe(store: &Path) -> Option<Hello> {
    let paths = paths(store).ok()?;
    let client = BlockingClient::connect(&paths, CLIENT_NAME, CONNECT_TIMEOUT, STATUS_TIMEOUT).ok()?;
    let hello = client.hello();
    Some(Hello { pid: hello.pid, version: hello.daemon_version.clone() })
}

/// Start the daemon on this store, only from an explicit user action. Waits for the channel and
/// reports a readable sentence; a binary that exits at once is not a silent success.
pub fn start(store: &Path) -> Result<(), String> {
    if probe(store).is_some() {
        return Ok(());
    }
    start_binary(store, &binary_path()?)
}

/// The spawn-and-wait half of `start`, split out so a test can prove that a missing binary is one
/// readable sentence without writing to the process environment.
fn start_binary(store: &Path, binary: &Path) -> Result<(), String> {
    let mut command = Command::new(binary);
    command.arg("--store").arg(store).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| format!("Non riesco ad avviare `{}`: {error}", binary.display()))?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if probe(store).is_some() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("Il daemon si è chiuso subito ({status}). Controlla la chiave dell'archivio nel portachiavi."));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err("Il daemon non ha aperto il canale locale entro 15 secondi: potrebbe essere ancora in avvio.".to_owned())
}

/// Ask the daemon to stop and wait until this store has no daemon. Only from an explicit command.
pub fn stop(link: &mut Link, store: &Path) -> Result<(), String> {
    link.shutdown()?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if probe(store).is_none() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err("Il daemon non ha chiuso il canale locale entro 10 secondi.".to_owned())
}

/// The daemon binary next to this executable, or the test hook. Never a silent fallback: a
/// missing binary is one readable sentence.
pub(crate) fn binary_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("POSTINO_DAEMON_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("`{}` non è un file eseguibile.", path.display()));
    }
    let name = if cfg!(windows) { "postino-daemon.exe" } else { "postino-daemon" };
    let current = std::env::current_exe().map_err(|error| format!("Non conosco la cartella dell'app: {error}"))?;
    let candidate = current.parent().map(|parent| parent.join(name)).ok_or_else(|| "Non conosco la cartella dell'app.".to_owned())?;
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!("`postino-daemon` non è accanto all'app ({}): compila `cargo build -p postino-daemon`.", candidate.display()))
    }
}

/// Draft attachments travel as base64 inside `op`, on the same 16 MiB frame as the CLI. A payload
/// that cannot fit is refused before the write, with the sentence the user needs.
pub fn base64_payload(bytes: &[u8]) -> Result<String, String> {
    use base64::Engine as _;
    // 16 MiB frame; base64 expands by 4/3, so 12 MiB would already fill the frame before the JSON
    // envelope. 10 MiB keeps ~2.4 MiB of margin and refuses the rest here, not on the wire.
    const MAX_INLINE: usize = 10 * 1024 * 1024;
    if bytes.len() > MAX_INLINE {
        return Err(format!(
            "Il file supera i {MAX_INLINE} byte allegabili dal canale locale ({} byte): allega il file dalla CLI `postino`.",
            bytes.len()
        ));
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Add an account through the daemon: the same `account_add` op the CLI dispatches, with the
/// password carried only inside the same-user channel and never written or logged here.
pub fn add_account(paths: &DaemonPaths, setup: &postino_core::AccountSetup) -> Result<(String, String), String> {
    let tls = setup.tls.as_ref().map(serde_json::to_value).transpose().map_err(|error| error.to_string())?;
    let smtp = setup.smtp.as_ref();
    let smtp_tls = smtp.map(|smtp| serde_json::to_value(smtp.tls)).transpose().map_err(|error| error.to_string())?;
    let args = json!({
        "provider": serde_json::to_value(setup.provider).map_err(|error| error.to_string())?,
        "email": setup.email,
        "name": setup.name,
        "password": setup.password.as_str(),
        "host": setup.host,
        "port": setup.port,
        "tls": tls,
        "smtp_host": smtp.map(|smtp| smtp.host.clone()),
        "smtp_port": smtp.map(|smtp| smtp.port),
        "smtp_tls": smtp_tls,
        "smtp_username": smtp.map(|smtp| smtp.username.clone()),
        "smtp_password": smtp.and_then(|smtp| smtp.password.as_ref()).map(|password| password.as_str()),
    });
    let mut client = BlockingClient::connect_with(paths, CLIENT_NAME, true, CONNECT_TIMEOUT, CALL_TIMEOUT).map_err(|error| describe(&error))?;
    let value = client
        .call(postino_ipc::protocol::method::OP, json!({ "name": "account_add", "args": args }), CALL_TIMEOUT)
        .map_err(|error| describe(&error))?;
    let account = value.get("account").ok_or_else(|| "Il daemon ha aggiunto l'account ma non lo ha restituito: riapri la finestra.".to_owned())?;
    let id = account.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let email = account.get("email").and_then(Value::as_str).unwrap_or_default().to_owned();
    if id.is_empty() {
        return Err("Il daemon ha aggiunto l'account ma non lo ha restituito: riapri la finestra.".to_owned());
    }
    Ok((id, email))
}

/// One sentence for a client that cannot reach a daemon. An absent endpoint, or a store that never
/// had a token file written, is the documented "not running" state, not a crash.
pub fn absent_or(paths: &DaemonPaths, error: &IpcError) -> String {
    let never_started = matches!(error, IpcError::TokenFile(_)) && !paths.token_file.exists();
    if error.is_absent() || never_started { absent_sentence() } else { describe(error) }
}

/// Whether an operation only reads. The classification comes from the one registry in
/// `postino-core`: the app keeps no second list of names. The daemon-owned settings read is a
/// control method, not an operation, and is read-only by construction: a dropped pipe may be
/// retried and can never be reported as an uncertain mutation.
pub fn is_read_only(name: &str) -> bool {
    use std::sync::OnceLock;
    if name == postino_ipc::protocol::method::DAEMON_SETTINGS {
        return true;
    }
    static READ_ONLY: OnceLock<Vec<&'static str>> = OnceLock::new();
    let names = READ_ONLY.get_or_init(|| postino_core::op_defs().into_iter().filter(|def| def.read_only).map(|def| def.name).collect());
    names.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A transport that records every operation name and retry flag and answers with a scripted
    /// value.
    struct Recorder {
        calls: Arc<Mutex<Vec<(String, Value, bool)>>>,
        connects: Arc<Mutex<u32>>,
        fail_connects: u32,
        events: Vec<Value>,
    }

    impl Transport for Recorder {
        fn connect(&mut self) -> Result<Hello, String> {
            let mut connects = match self.connects.lock() {
                Ok(connects) => connects,
                Err(_) => return Err("poisoned".to_owned()),
            };
            *connects += 1;
            if *connects <= self.fail_connects {
                return Err(absent_sentence());
            }
            Ok(Hello { pid: 4242, version: "0.1.0".to_owned() })
        }

        fn call(&mut self, name: &str, args: Value, retry: bool) -> Result<Value, String> {
            self.calls.lock().map_err(|_| "poisoned".to_owned())?.push((name.to_owned(), args, retry));
            Ok(json!({ "ok": true }))
        }

        fn status(&mut self) -> Result<Value, String> {
            Ok(json!({ "pid": 4242 }))
        }

        fn shutdown(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn next_event(&mut self, _wait: Duration) -> Result<Option<Value>, String> {
            Ok(self.events.pop())
        }
    }

    #[allow(clippy::type_complexity)]
    fn recorder(fail_connects: u32, events: Vec<Value>) -> (Box<dyn Transport>, Arc<Mutex<Vec<(String, Value, bool)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = Recorder { calls: Arc::clone(&calls), connects: Arc::new(Mutex::new(0)), fail_connects, events };
        (Box::new(transport), calls)
    }

    fn drain(worker: &mut EventWorker, at_least: usize) -> Vec<Event> {
        let mut collected = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while collected.len() < at_least && Instant::now() < deadline {
            collected.extend(worker.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        collected
    }

    #[test]
    fn a_link_sends_the_operation_and_the_args_it_was_given() {
        let (transport, calls) = recorder(0, Vec::new());
        let mut link = Link::with_transport(transport);
        let value = link.call("message_flag", json!({ "id": "m1", "unread": false })).unwrap_or(Value::Null);
        assert_eq!(value, json!({ "ok": true }));
        let _ = link.call("accounts_list", json!({}));
        let calls = calls.lock().map(|calls| calls.clone()).unwrap_or_default();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "message_flag");
        assert_eq!(calls[0].1["unread"], json!(false));
        assert!(!calls[0].2, "a mutation is never replayed: the registry says message_flag writes");
        assert!(calls[1].2, "a read may be retried once on a fresh connection");
    }

    /// A real channel: the daemon stops, then a write whose answer can never arrive is reported as
    /// uncertain instead of being replayed, while a read keeps its documented retry.
    #[test]
    fn a_lost_channel_never_replays_a_mutation() {
        let directory = std::env::temp_dir().join("opencode").join("p10a2-postino").join("lost-mutation");
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::create_dir_all(&directory);
        let path = directory.join("mail.db");
        let key = Arc::new(postino_store::StoreKey::from_bytes([0x31; 32]));
        let vault: Arc<dyn postino_core::CredentialVault> = crate::test_support::MemoryVault::new();
        {
            let store = postino_store::Store::open(&path, key.as_ref()).expect("seed store");
            store
                .upsert_account(&postino_store::Account {
                    id: "acc_lost".to_owned(),
                    name: "Postino Test".to_owned(),
                    email: "test@example.com".to_owned(),
                    provider: "custom".to_owned(),
                    signature: String::new(),
                    created_at: 1_700_000_000_000,
                })
                .expect("account row");
        }
        let mut config = postino_daemon::DaemonConfig::new(path.clone(), Arc::clone(&key), Arc::clone(&vault));
        config.sync_on_start = false;
        config.sync_jitter = false;
        config.outbox_on_start = false;
        let daemon = postino_daemon::Daemon::spawn(config).expect("the daemon starts");
        let mut link = Link::connect(&path).expect("the channel opens");
        assert!(link.call("accounts_list", json!({})).is_ok());
        daemon.shutdown().expect("the daemon stops");

        let error = link.call("account_signature_set", json!({ "account": "acc_lost", "signature": "x" }));
        let Err(sentence) = error else {
            panic!("a daemon that stopped before answering cannot confirm a mutation");
        };
        assert!(sentence.contains("potrebbe essere stata applicata"), "unexpected mutation sentence: {sentence}");
        assert!(!sentence.to_lowercase().contains("token"), "the sentence must not mention the token file");
        // A read is allowed to try again; with no daemon the second attempt is the absent sentence.
        let read = link.call("accounts_list", json!({}));
        assert!(read.is_err());
    }

    #[test]
    fn an_absent_daemon_is_one_readable_sentence() {
        let directory = std::env::temp_dir().join("opencode").join("p10a2-postino").join("absent");
        let _ = std::fs::create_dir_all(&directory);
        let store = directory.join("mail.db");
        let error = Link::connect(&store).and_then(|mut link| link.call("accounts_list", json!({})));
        let Err(sentence) = error else {
            // A daemon really answered on this endpoint: the sentence cannot be exercised here.
            return;
        };
        assert!(sentence.contains("non è in esecuzione"), "unexpected sentence: {sentence}");
        assert!(!sentence.contains("token"), "the refusal must not mention the token file: {sentence}");
    }

    #[test]
    fn the_worker_connects_after_bounded_failures_and_forwards_events() {
        let (transport, _) = recorder(2, vec![json!({ "kind": "outbox", "data": { "id": "o1" } })]);
        let mut worker = EventWorker::start(transport);
        let events = drain(&mut worker, 3);
        worker.stop();
        assert!(events.iter().any(|event| matches!(event, Event::Reconnecting { .. })));
        assert!(events.iter().any(|event| matches!(event, Event::Connected(hello) if hello.pid == 4242)));
        assert!(events.iter().any(|event| matches!(event, Event::Daemon(value) if value["kind"] == "outbox")));
    }

    #[test]
    fn the_worker_says_lost_after_the_bounded_attempts() {
        let (transport, _) = recorder(10, Vec::new());
        let mut worker = EventWorker::start(transport);
        let events = drain(&mut worker, 4);
        worker.stop();
        assert!(events.iter().any(|event| matches!(event, Event::Lost)));
    }

    #[test]
    fn a_stopped_worker_joins_and_stops_producing_events() {
        let (transport, _) = recorder(0, Vec::new());
        let mut worker = EventWorker::start(transport);
        let _ = drain(&mut worker, 1);
        worker.stop();
        assert!(worker.poll().is_empty());
    }

    #[test]
    fn the_mode_sentences_never_carry_a_secret() {
        let states = [
            Connection::Demo,
            Connection::InProcess,
            Connection::Connected(Hello { pid: 7, version: "0.1.0".to_owned() }),
            Connection::Reconnecting { attempt: 2 },
            Connection::Lost,
        ];
        for state in states {
            let text = state.detail();
            assert!(!text.contains("token"), "mode sentence leaks a token: {text}");
            assert!(!text.contains("password"), "mode sentence leaks a password: {text}");
        }
    }

    #[test]
    fn a_start_needs_an_explicit_command_and_a_store() {
        assert!(Connection::InProcess.can_start(false));
        assert!(!Connection::InProcess.can_start(true));
        assert!(!Connection::Connected(Hello { pid: 1, version: "0.1.0".to_owned() }).can_start(false));
        assert_eq!(Connection::Demo.reason_start(true), Some("La modalità demo non avvia processi"));
        assert!(Connection::InProcess.reason_stop().is_some());
        assert!(Connection::Connected(Hello { pid: 1, version: "0.1.0".to_owned() }).reason_stop().is_none());
    }

    #[test]
    fn a_missing_daemon_binary_is_one_readable_sentence() {
        let directory = std::env::temp_dir().join("opencode").join("p10a2-postino").join("missing-binary");
        let _ = std::fs::create_dir_all(&directory);
        let missing = directory.join("no-such-postino-daemon.exe");
        let error = start_binary(&directory.join("mail.db"), &missing).expect_err("a missing binary cannot start");
        assert!(error.contains("Non riesco ad avviare"), "{error}");
        assert!(!error.to_lowercase().contains("token"), "{error}");
    }

    #[test]
    fn an_oversized_attachment_is_refused_before_the_frame() {
        // The refusal happens on the exact boundary, below the point where base64 fills the frame.
        let boundary = vec![0u8; 10 * 1024 * 1024];
        let encoded = base64_payload(&boundary).expect("10 MiB fits the frame with margin");
        assert!(encoded.len() < postino_ipc::MAX_FRAME_BYTES, "the base64 payload must leave room for the JSON envelope");
        assert!(base64_payload(&vec![0u8; 10 * 1024 * 1024 + 1]).is_err());
        assert!(base64_payload(&vec![0u8; 13 * 1024 * 1024]).is_err());
        let small = base64_payload(b"hello");
        assert_eq!(small.unwrap_or_default(), "aGVsbG8=");
    }
}
