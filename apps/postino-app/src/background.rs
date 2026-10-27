//! Background rows in Impostazioni: the daemon-owned tray, notifications, previews and the
//! per-user autostart entry. The app never invents their state: every row is read from the daemon or
//! from the platform, written explicitly, and disabled with a visible reason when it cannot work.
//!
//! Reads and writes are behind [`Bridge`] so a headless test can prove what the page would ask for
//! without a daemon, an OS tray or the registry.

use std::path::PathBuf;
use std::sync::Arc;

use postino_ipc::protocol::method;
use postino_platform::{Autostart, AutostartStatus, RegistryAutostart, autostart_command};
use serde::{Deserialize, Serialize};

use crate::daemon;

/// A partial settings write. Mirror of the daemon's patch: only the named field changes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub tray: Option<bool>,
    pub notify_enabled: Option<bool>,
    pub notify_previews: Option<bool>,
    pub debounce_secs: Option<u64>,
}

/// What the daemon persists and reports back.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonView {
    pub tray: bool,
    pub notify_enabled: bool,
    pub notify_previews: bool,
    pub debounce_secs: u64,
    pub tray_available: bool,
    pub tray_running: bool,
}

/// Autostart as the platform reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutostartView {
    pub enabled: bool,
    pub command: Option<String>,
}

/// One row: its stored value, whether the switch can be used, and the sentence that says why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub on: bool,
    pub available: bool,
    pub reason: Option<String>,
    /// A truthful state line under the switch (exact autostart command, tray failure).
    pub detail: Option<String>,
}

impl Row {
    fn off(reason: impl Into<String>) -> Self {
        Self { on: false, available: false, reason: Some(reason.into()), detail: None }
    }

    fn on(on: bool) -> Self {
        Self { on, available: true, reason: None, detail: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rows {
    pub tray: Row,
    pub notify: Row,
    pub previews: Row,
    pub autostart: Row,
    /// The icon was installed when the rows were read; close-to-tray needs both this and the switch.
    pub tray_running: bool,
    /// The last read or write failed; shown once above the rows, never as a fake value.
    pub error: Option<String>,
}

/// Demo mode has no store and no daemon, so no autostart entry can point anywhere. The row says
/// this instead of failing with the "daemon binary is not next to the app" sentence.
pub const DEMO_AUTOSTART: &str = "La modalità demo non ha un archivio locale: l'avvio automatico richiede un archivio aperto.";

impl Rows {
    pub fn unavailable(error: Option<String>) -> Self {
        let reason = "Richiede il daemon: avvialo con `postino daemon start`, poi riapri questa pagina.";
        Self { tray: Row::off(reason), notify: Row::off(reason), previews: Row::off(reason), autostart: Row::off(reason), tray_running: false, error }
    }

    /// Build the four rows from what was actually read. `autostart` carries the platform error
    /// verbatim (denied registry, unsupported platform) instead of hiding it behind a dead switch.
    pub fn read(daemon: Result<DaemonView, String>, autostart: Result<AutostartView, String>) -> Self {
        let mut rows = match daemon {
            Ok(view) => {
                let mut tray = Row::on(view.tray);
                if !view.tray_available {
                    tray.available = false;
                    tray.reason = Some("Questa piattaforma non ha una barra delle applicazioni per Postino.".to_owned());
                } else if view.tray && !view.tray_running {
                    tray.detail = Some("L'area di notifica non risponde: il daemon resta attivo, l'icona no.".to_owned());
                }
                let mut previews = Row::on(view.notify_previews);
                if !view.notify_enabled {
                    previews.available = false;
                    previews.reason = Some("Attiva prima le notifiche.".to_owned());
                }
                Self {
                    tray,
                    notify: Row::on(view.notify_enabled),
                    previews,
                    autostart: Row::off(""),
                    tray_running: view.tray && view.tray_running,
                    error: None,
                }
            }
            Err(error) => {
                let reason = "Richiede il daemon: avvialo con `postino daemon start`, poi riapri questa pagina.";
                Self {
                    tray: Row::off(reason),
                    notify: Row::off(reason),
                    previews: Row::off(reason),
                    autostart: Row::off(""),
                    tray_running: false,
                    error: Some(error),
                }
            }
        };
        rows.autostart = match autostart {
            Ok(view) => {
                let mut row = Row::on(view.enabled);
                row.detail = view.command.filter(|_| view.enabled).map(|command| format!("Comando: {command}"));
                row
            }
            Err(reason) => Row::off(reason),
        };
        rows
    }
}

/// Close-to-tray is allowed only when the daemon owns the store and its icon is really installed;
/// otherwise the window closing must exit instead of leaving an unreachable process.
pub fn close_action(tray_enabled: bool, tray_running: bool, daemon_owned: bool, notice_shown: bool) -> CloseAction {
    if !daemon_owned || !tray_enabled || !tray_running {
        return CloseAction::Exit;
    }
    if notice_shown { CloseAction::Hide } else { CloseAction::Ask }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    Hide,
    Exit,
    Ask,
}

/// The reads and writes the rows need. A recorder implements this in tests.
pub trait Bridge: Send {
    fn daemon(&mut self) -> Result<DaemonView, String>;
    fn patch(&mut self, patch: Patch) -> Result<DaemonView, String>;
    fn autostart(&mut self) -> Result<AutostartView, String>;
    fn autostart_set(&mut self, enabled: bool) -> Result<AutostartView, String>;
}

/// Production bridge: a second IPC connection to the daemon and the real HKCU Run value. Neither
/// side starts the daemon; when it is absent the rows say so.
pub struct LiveBridge {
    store: Option<PathBuf>,
    planned_command: Option<String>,
    autostart: Arc<dyn Autostart>,
    link: Option<daemon::Link>,
}

impl LiveBridge {
    pub fn new(store: Option<PathBuf>) -> Self {
        let executable = daemon::binary_path().ok();
        let planned_command = match (&executable, &store) {
            (Some(executable), Some(store)) => autostart_command(executable, store).ok(),
            _ => None,
        };
        Self {
            store,
            planned_command: planned_command.clone(),
            autostart: Arc::new(RegistryAutostart::for_current_user(planned_command.unwrap_or_default())),
            link: None,
        }
    }

    fn call(&mut self, name: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        self.call_bounded(name, args, daemon::CALL_TIMEOUT)
    }

    fn call_bounded(&mut self, name: &str, args: serde_json::Value, timeout: std::time::Duration) -> Result<serde_json::Value, String> {
        let store = self.store.as_ref().ok_or_else(|| "La modalità demo non ha un daemon: apri un archivio locale.".to_owned())?;
        let link = match self.link.as_mut() {
            Some(link) => link,
            None => self.link.insert(daemon::Link::connect(store)?),
        };
        let result = link.call_bounded(name, args, timeout);
        if result.is_err() {
            // The daemon may have restarted; the next read reconnects instead of reusing a dead pipe.
            self.link = None;
        }
        result
    }

    fn autostart_view(status: AutostartStatus) -> AutostartView {
        AutostartView { enabled: status.enabled, command: status.command }
    }
}

impl Bridge for LiveBridge {
    fn daemon(&mut self) -> Result<DaemonView, String> {
        let value = self.call_bounded(method::DAEMON_SETTINGS, serde_json::json!({}), daemon::READ_TIMEOUT)?;
        serde_json::from_value(value).map_err(|error| format!("Il daemon ha risposto con impostazioni illeggibili: {error}"))
    }

    fn patch(&mut self, patch: Patch) -> Result<DaemonView, String> {
        let value = serde_json::to_value(patch).map_err(|error| format!("Impostazioni non codificabili: {error}"))?;
        let value = self.call(method::DAEMON_SETTINGS_SET, value)?;
        serde_json::from_value(value).map_err(|error| format!("Il daemon ha risposto con impostazioni illeggibili: {error}"))
    }

    fn autostart(&mut self) -> Result<AutostartView, String> {
        self.autostart.status().map(Self::autostart_view).map_err(|error| format!("Autostart non disponibile: {error}"))
    }

    fn autostart_set(&mut self, enabled: bool) -> Result<AutostartView, String> {
        if self.store.is_none() {
            return Err(DEMO_AUTOSTART.to_owned());
        }
        if enabled && self.planned_command.is_none() {
            return Err("Autostart non disponibile: il percorso del daemon non è accanto all'app. Usa `postino daemon autostart on`.".to_owned());
        }
        let result = if enabled { self.autostart.enable() } else { self.autostart.disable() };
        result.map(Self::autostart_view).map_err(|error| format!("Autostart non disponibile: {error}"))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct RecorderBridge {
        pub daemon: Option<DaemonView>,
        pub autostart: Result<AutostartView, String>,
        pub writes: Vec<Patch>,
        pub autostart_writes: Vec<bool>,
        pub fail_write: Option<String>,
    }

    impl Default for RecorderBridge {
        fn default() -> Self {
            Self {
                daemon: None,
                autostart: Ok(AutostartView { enabled: false, command: None }),
                writes: Vec::new(),
                autostart_writes: Vec::new(),
                fail_write: None,
            }
        }
    }

    impl RecorderBridge {
        pub fn with_rows() -> Self {
            Self {
                daemon: Some(DaemonView {
                    tray: true,
                    notify_enabled: true,
                    notify_previews: false,
                    debounce_secs: 60,
                    tray_available: true,
                    tray_running: true,
                }),
                autostart: Ok(AutostartView { enabled: false, command: None }),
                ..Self::default()
            }
        }
    }

    impl Bridge for RecorderBridge {
        fn daemon(&mut self) -> Result<DaemonView, String> {
            self.daemon.clone().ok_or_else(|| "Il daemon non è in esecuzione.".to_owned())
        }

        fn patch(&mut self, patch: Patch) -> Result<DaemonView, String> {
            self.writes.push(patch.clone());
            if let Some(error) = &self.fail_write {
                return Err(error.clone());
            }
            let mut view = self.daemon.clone().unwrap_or_default();
            if let Some(value) = patch.tray {
                view.tray = value;
            }
            if let Some(value) = patch.notify_enabled {
                view.notify_enabled = value;
            }
            if let Some(value) = patch.notify_previews {
                view.notify_previews = value;
            }
            if let Some(value) = patch.debounce_secs {
                view.debounce_secs = value.max(60);
            }
            if !view.notify_enabled {
                view.notify_previews = false;
            }
            self.daemon = Some(view.clone());
            Ok(view)
        }

        fn autostart(&mut self) -> Result<AutostartView, String> {
            self.autostart.clone()
        }

        fn autostart_set(&mut self, enabled: bool) -> Result<AutostartView, String> {
            self.autostart_writes.push(enabled);
            let mut view = self.autostart.clone().unwrap_or_default();
            view.enabled = enabled;
            view.command = enabled.then(|| "\"postino-daemon.exe\" --store \"mail.db\"".to_owned());
            self.autostart = Ok(view.clone());
            Ok(view)
        }
    }

    #[test]
    fn demo_autostart_is_refused_with_the_missing_archive_not_the_missing_binary() {
        let mut bridge = LiveBridge::new(None);
        let error = bridge.autostart_set(true).expect_err("the demo has no archive to start at login");
        assert!(error.contains("archivio locale"), "the demo reason must name the missing archive: {error}");
        assert!(!error.contains("accanto all'app"), "the demo must not get the missing-binary sentence: {error}");
    }

    #[test]
    fn a_wedged_daemon_cannot_freeze_the_settings_read_for_two_minutes() {
        use postino_ipc::protocol::RpcError;
        use postino_ipc::server::{BoxFuture, DispatchRequest, Dispatcher, Server, ServerConfig};
        use postino_ipc::{CancellationToken, DaemonPaths, EventHub, SharedToken, TokenFile};
        use serde_json::{Value, json};
        use std::time::Duration;

        /// Accepts the handshake and then never answers a request: the wedge the settings page
        /// must survive instead of blocking a frame for the 120 s write timeout.
        struct Wedged;

        impl Dispatcher for Wedged {
            fn dispatch(&self, _request: DispatchRequest) -> BoxFuture<Result<Value, RpcError>> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(json!({}))
                })
            }
        }

        let Ok(directory) = tempfile::tempdir() else { return };
        let store = directory.path().join("mail.db");
        let paths = DaemonPaths::for_store(&store).expect("paths");
        let token = SharedToken::generate().expect("token");
        TokenFile::new(paths.token_file.clone()).write(&token).expect("token file");
        let mut config = ServerConfig::new(paths.endpoint.clone(), token);
        config.handshake_timeout = Duration::from_secs(2);
        let shutdown = CancellationToken::new();
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
        let _server = runtime.spawn(Server::new(config, Arc::new(Wedged), Arc::new(EventHub::new())).run(shutdown.clone()));
        std::thread::sleep(Duration::from_millis(300));

        let mut bridge = LiveBridge::new(Some(store));
        let started = std::time::Instant::now();
        let error = bridge.daemon().expect_err("a daemon that never answers must time out");
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(8), "the settings read must be bounded well below the write timeout, took {elapsed:?}");
        assert!(!error.contains("potrebbe essere stata applicata"), "a settings read is read-only and cannot be an uncertain mutation: {error}");
        assert!(daemon::is_read_only(postino_ipc::protocol::method::DAEMON_SETTINGS));
        shutdown.cancel();
        runtime.shutdown_timeout(Duration::from_millis(500));
    }

    #[test]
    fn every_row_reads_back_the_written_value() {
        let mut bridge = RecorderBridge::with_rows();
        let rows = Rows::read(bridge.daemon(), bridge.autostart());
        assert!(rows.tray.on && rows.tray.available);
        assert!(!rows.previews.on && rows.previews.available);

        let view = bridge.patch(Patch { notify_previews: Some(true), tray: Some(false), ..Patch::default() }).expect("write");
        assert!(view.notify_previews);
        assert!(!view.tray);
        let rows = Rows::read(bridge.daemon(), bridge.autostart());
        assert!(!rows.tray.on);
        assert!(rows.previews.on);
    }

    #[test]
    fn previews_are_disabled_with_a_reason_while_notifications_are_off() {
        let daemon = DaemonView { notify_enabled: false, notify_previews: true, ..DaemonView::default() };
        let rows = Rows::read(Ok(daemon), Ok(AutostartView::default()));
        assert!(rows.notify.available && !rows.notify.on);
        assert!(!rows.previews.available);
        assert_eq!(rows.previews.reason.as_deref(), Some("Attiva prima le notifiche."));
    }

    #[test]
    fn tray_enabled_but_not_running_is_named_instead_of_pretending() {
        let daemon = DaemonView { tray: true, tray_available: true, tray_running: false, ..DaemonView::default() };
        let rows = Rows::read(Ok(daemon), Err("Autostart non disponibile: accesso negato".to_owned()));
        assert!(rows.tray.available && rows.tray.on);
        assert!(rows.tray.detail.as_deref().is_some_and(|detail| detail.contains("area di notifica")));
        assert!(!rows.autostart.available);
        assert!(rows.autostart.reason.as_deref().is_some_and(|reason| reason.contains("accesso negato")));
    }

    #[test]
    fn a_missing_daemon_disables_the_rows_with_the_command_that_fixes_it() {
        let rows = Rows::read(Err("Il daemon non è in esecuzione.".to_owned()), Ok(AutostartView::default()));
        for row in [&rows.tray, &rows.notify, &rows.previews] {
            assert!(!row.available);
            assert!(row.reason.as_deref().is_some_and(|reason| reason.contains("postino daemon start")));
        }
        // Autostart is a platform fact, not a daemon one: the row still answers without the daemon.
        assert!(rows.autostart.available);
        assert!(rows.error.is_some());
    }

    #[test]
    fn close_hides_only_with_a_running_daemon_tray_and_exits_otherwise() {
        assert_eq!(close_action(true, true, true, true), CloseAction::Hide);
        assert_eq!(close_action(true, true, true, false), CloseAction::Ask);
        assert_eq!(close_action(false, true, true, true), CloseAction::Exit);
        assert_eq!(close_action(true, false, true, true), CloseAction::Exit);
        assert_eq!(close_action(true, true, false, true), CloseAction::Exit);
    }

    #[test]
    fn autostart_reads_the_exact_command_and_writes_once() {
        let mut bridge = RecorderBridge::with_rows();
        let view = bridge.autostart_set(true).expect("enable");
        assert!(view.enabled);
        assert!(view.command.as_deref().is_some_and(|command| command.contains("--store")));
        assert_eq!(bridge.autostart_writes, vec![true]);
        assert!(bridge.autostart().expect("read back").enabled);
    }
}
