//! The tray seam: a native icon with a fixed menu, owned by the daemon.
//!
//! The daemon never draws the tray itself. [`Win32Tray`] is the only implementation that touches the
//! notification area; [`RecorderTray`] records state and replays actions for headless tests. The menu
//! has exactly [`TrayAction`]'s entries, in order, so a test cannot drift from the real icon.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

/// The four states the icon distinguishes. No counters, no badge: state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrayState {
    /// No sync running, nothing failed, outbox idle.
    Quiet = 0,
    /// A sync is running now.
    Syncing = 1,
    /// The last background network step failed; mail is still readable.
    Offline = 2,
    /// The outbox has an uncertain or failed submission.
    Failed = 3,
}

impl TrayState {
    /// Tooltip text. Bounded and free of message content by construction.
    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Quiet => "Postino - in attesa",
            Self::Syncing => "Postino - sincronizzazione in corso",
            Self::Offline => "Postino - offline",
            Self::Failed => "Postino - invio non riuscito",
        }
    }

    /// Stable name used by the recorder and tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Syncing => "syncing",
            Self::Offline => "offline",
            Self::Failed => "failed",
        }
    }
}

/// One menu entry. The order of the variants is the menu order.
///
/// P10b1 has no outbox entry: opening a specific view needs app activation, which is P10b2. A menu
/// item that silently opened the same window as `Apri Postino` would be a dead entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Launch the sibling app against the same store.
    Open,
    /// Ask the daemon to sync every account now.
    SyncNow,
    /// Launch the app straight into the outbox.
    OpenOutbox,
    /// Ask the daemon to stop.
    Quit,
}

impl TrayAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Apri Postino",
            Self::SyncNow => "Sincronizza ora",
            Self::OpenOutbox => "Coda di invio",
            Self::Quit => "Esci",
        }
    }

    /// Stable identifier for tests and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::SyncNow => "sync_now",
            Self::OpenOutbox => "outbox",
            Self::Quit => "quit",
        }
    }
}

/// What the tray needs to launch the app. The store path identifies the daemon's store; the app
/// binary is the sibling of the daemon binary, never a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayLaunch {
    pub store: PathBuf,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TrayError {
    #[error("the notification area is not available: {0}")]
    Unavailable(String),
    #[error("the tray is not supported on this platform")]
    Unsupported,
    #[error("the tray has been stopped")]
    Stopped,
}

/// The OS notification-area seam. Implementations must be safe to call from the daemon scheduler
/// thread; the Windows implementation only posts to its own window.
pub trait Tray: Send + Sync {
    fn start(&self, launch: &TrayLaunch) -> Result<(), TrayError>;
    fn set_state(&self, state: TrayState) -> Result<(), TrayError>;
    /// Pop one queued menu action. Never blocks.
    fn next_action(&self) -> Option<TrayAction>;
    /// The notifier bound to this icon, when the platform can show one.
    fn notifier(&self) -> Option<std::sync::Arc<dyn crate::notify::Notifier>> {
        None
    }
    /// Remove the icon and stop the window thread. Idempotent.
    fn stop(&self);
}

/// Headless tray: records the last state and replays a bounded queue of actions.
#[derive(Debug, Default)]
pub struct RecorderTray {
    state: Mutex<Option<TrayState>>,
    actions: Mutex<VecDeque<TrayAction>>,
    started: AtomicBool,
    fail_start: AtomicBool,
    launches: Mutex<Vec<TrayLaunch>>,
}

impl RecorderTray {
    pub fn new() -> Self {
        Self::default()
    }

    /// A tray whose start fails, standing in for a session without Explorer.
    pub fn failing() -> Self {
        let tray = Self::default();
        tray.fail_start.store(true, Ordering::SeqCst);
        tray
    }

    /// Queue a click as the native menu would deliver it.
    pub fn push_action(&self, action: TrayAction) {
        if let Ok(mut actions) = self.actions.lock() {
            if actions.len() >= 64 {
                actions.pop_front();
            }
            actions.push_back(action);
        }
    }

    pub fn state(&self) -> Option<TrayState> {
        self.state.lock().ok().and_then(|state| *state)
    }

    pub fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    pub fn launches(&self) -> Vec<TrayLaunch> {
        self.launches.lock().map(|launches| launches.clone()).unwrap_or_default()
    }
}

impl Tray for RecorderTray {
    fn start(&self, launch: &TrayLaunch) -> Result<(), TrayError> {
        if self.fail_start.load(Ordering::SeqCst) {
            return Err(TrayError::Unavailable("the recorder is configured to fail".to_owned()));
        }
        if let Ok(mut launches) = self.launches.lock() {
            launches.push(launch.clone());
        }
        self.started.store(true, Ordering::SeqCst);
        if let Ok(mut state) = self.state.lock() {
            *state = Some(TrayState::Quiet);
        }
        Ok(())
    }

    fn set_state(&self, state: TrayState) -> Result<(), TrayError> {
        if let Ok(mut current) = self.state.lock() {
            *current = Some(state);
        }
        Ok(())
    }

    fn next_action(&self) -> Option<TrayAction> {
        self.actions.lock().ok().and_then(|mut actions| actions.pop_front())
    }

    fn stop(&self) {
        self.started.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_records_state_and_replays_actions_in_order() {
        let tray = RecorderTray::new();
        assert!(tray.start(&TrayLaunch { store: PathBuf::from("mail.db") }).is_ok());
        assert!(tray.started());
        assert_eq!(tray.state(), Some(TrayState::Quiet));
        tray.push_action(TrayAction::Open);
        tray.push_action(TrayAction::Quit);
        assert_eq!(tray.next_action(), Some(TrayAction::Open));
        assert_eq!(tray.next_action(), Some(TrayAction::Quit));
        assert_eq!(tray.next_action(), None);
        assert!(tray.set_state(TrayState::Syncing).is_ok());
        assert_eq!(tray.state(), Some(TrayState::Syncing));
        tray.stop();
        assert!(!tray.started());
    }

    #[test]
    fn menu_has_the_four_p10b2_entries() {
        let labels: Vec<&str> =
            [TrayAction::Open, TrayAction::SyncNow, TrayAction::OpenOutbox, TrayAction::Quit].into_iter().map(TrayAction::label).collect();
        assert_eq!(labels, ["Apri Postino", "Sincronizza ora", "Coda di invio", "Esci"]);
        let ids: Vec<&str> =
            [TrayAction::Open, TrayAction::SyncNow, TrayAction::OpenOutbox, TrayAction::Quit].into_iter().map(TrayAction::as_str).collect();
        assert_eq!(ids, ["open", "sync_now", "outbox", "quit"]);
    }
}
