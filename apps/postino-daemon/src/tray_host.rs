//! Daemon-owned tray, menu actions and privacy-preserving notices.
//!
//! The tray is started only after the daemon owns its store; a missing notification area is one log
//! line and nothing else. Menu actions run on the daemon runtime: open the sibling app, request a
//! sync, or stop the daemon. New-mail notices are deduplicated by one bounded scalar horizon (the
//! newest announced message date) plus a debounce window; the horizon lives in encrypted `meta`, so
//! a restart never repeats an old notice. Failure notices are deduplicated by outbox row id.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use postino_core::MessageSummary;
use postino_platform::{Notice, TrayAction, TrayLaunch, TrayState};

use crate::DaemonState;
use crate::settings::DaemonSettings;

pub const META_HORIZON: &str = "daemon.notify.horizon";
pub const META_FAILED: &str = "daemon.notify.failed";

const MAX_SCAN: usize = 5000;
const HORIZON_UNREADABLE: &str = "new-mail horizon could not be read; new-mail notices are skipped until restart";
const HORIZON_UNWRITABLE: &str = "new-mail horizon could not be persisted; a restart may repeat one notice";
const NEW_MAIL_TITLE: &str = "Nuova posta";
const NEW_MAIL_BODY: &str = "Hai nuova posta. Apri Postino per leggerla.";
const FAILURE_TITLE: &str = "Invio non riuscito";
const FAILURE_BODY: &str = "Apri Postino e controlla la Coda di invio.";

/// Which view the app should open after boot. The tray entry is the only caller that asks for the
/// outbox, so a plain "Apri Postino" still starts on the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppOpen {
    Mail,
    Outbox,
}

/// A launcher seam so a recorder test can prove which store and view the menu opens without
/// starting a GUI.
pub trait AppLauncher: Send + Sync {
    fn open(&self, store: &Path, view: AppOpen) -> Result<(), String>;
}

/// Production launcher: the sibling `postino-app` binary, never a shell and never a URL. Only used
/// when no window is listening on the daemon events; a connected window is activated instead.
#[derive(Debug, Default)]
pub struct ProcessLauncher;

impl AppLauncher for ProcessLauncher {
    fn open(&self, store: &Path, view: AppOpen) -> Result<(), String> {
        let Some(app) = app_binary() else {
            return Err("the app binary is not next to the daemon binary".to_owned());
        };
        let mut command = std::process::Command::new(app);
        command.arg("--store").arg(store);
        if view == AppOpen::Outbox {
            command.arg("--open").arg("outbox");
        }
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        command.spawn().map(|_| ()).map_err(|error| format!("the app process could not start: {}", error.kind()))
    }
}

fn app_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("POSTINO_APP_BIN").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let current = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "postino-app.exe" } else { "postino-app" };
    let sibling = current.parent()?.join(name);
    sibling.is_file().then_some(sibling)
}

/// New-mail and failure dedupe state. The new-mail horizon is one scalar (the newest announced
/// message date); only ids and that date are stored, never sender, subject or body.
#[derive(Debug)]
pub(crate) struct NoticePolicy {
    /// Newest message date already announced, epoch milliseconds. `None` means the encrypted horizon
    /// could not be read: new-mail notices stay silent instead of repeating the mailbox or pretending
    /// it was marked as seen.
    horizon_ms: Option<i64>,
    last_notice_ms: Option<i64>,
    last_failed: Option<String>,
}

/// A notice and the horizon it would advance to. The horizon moves only when the OS accepted it.
#[derive(Debug)]
pub(crate) struct NewMailNotice {
    pub(crate) notice: Notice,
    horizon_ms: i64,
    at_ms: i64,
}

impl Default for NoticePolicy {
    fn default() -> Self {
        Self { horizon_ms: Some(0), last_notice_ms: None, last_failed: None }
    }
}

impl NoticePolicy {
    /// Reopen from the two encrypted `meta` values. A missing horizon is the first run: adopt the
    /// current mailbox without notifying. A read failure is one fixed log sentence and leaves the
    /// horizon unknown, so nothing is marked seen and no success is faked.
    pub(crate) fn initialize(engine: &mut postino_core::Engine) -> Self {
        let failed = engine.meta_get(META_FAILED).ok().flatten().filter(|id| !id.is_empty());
        match engine.meta_get(META_HORIZON) {
            Ok(Some(raw)) => match raw.trim().parse::<i64>() {
                Ok(horizon) => Self { horizon_ms: Some(horizon), last_notice_ms: None, last_failed: failed },
                // App-owned data, not user input: a corrupt scalar re-adopts the mailbox.
                Err(_) => Self::adopt(engine, failed),
            },
            Ok(None) => Self::adopt(engine, failed),
            Err(_) => {
                log::warn!("{HORIZON_UNREADABLE}");
                Self { horizon_ms: None, last_notice_ms: None, last_failed: failed }
            }
        }
    }

    fn adopt(engine: &mut postino_core::Engine, failed: Option<String>) -> Self {
        let horizon = engine.messages().iter().map(|message| message.date.timestamp_millis()).max().unwrap_or(0);
        if engine.meta_set(META_HORIZON, &horizon.to_string()).is_err() {
            log::warn!("{HORIZON_UNWRITABLE}");
        }
        Self { horizon_ms: Some(horizon), last_notice_ms: None, last_failed: failed }
    }

    pub(crate) fn encoded_horizon(&self) -> String {
        self.horizon_ms.map_or_else(|| "0".to_owned(), |horizon| horizon.to_string())
    }

    pub(crate) fn failed(&self) -> Option<String> {
        self.last_failed.clone()
    }

    /// Build one notice for unread messages newer than the horizon. Nothing is committed here: the
    /// caller shows the notice, then calls [`NoticePolicy::accepted`].
    pub(crate) fn new_mail(&mut self, settings: &DaemonSettings, now_ms: i64, messages: &[MessageSummary]) -> Option<NewMailNotice> {
        let horizon = self.horizon_ms?;
        let fresh = messages.iter().filter(|message| message.unread && message.date.timestamp_millis() > horizon).take(MAX_SCAN).collect::<Vec<_>>();
        if fresh.is_empty() || !settings.notify_enabled || self.debounced(settings, now_ms) {
            // A debounced or disabled notice must not consume the horizon: the next pass can still
            // show the message.
            return None;
        }
        let horizon_ms = fresh.iter().map(|message| message.date.timestamp_millis()).max()?;
        let notice = if settings.notify_previews {
            let newest = fresh.iter().max_by_key(|message| message.date)?;
            let sender = if newest.from.name.trim().is_empty() { newest.from.email.as_str() } else { newest.from.name.as_str() };
            Notice::new(NEW_MAIL_TITLE, &format!("{sender}: {}", newest.subject))
        } else {
            Notice::new(NEW_MAIL_TITLE, NEW_MAIL_BODY)
        };
        Some(NewMailNotice { notice, horizon_ms, at_ms: now_ms })
    }

    /// Commit a notice the notifier accepted; returns the scalar to persist in encrypted `meta`.
    pub(crate) fn accepted(&mut self, notice: &NewMailNotice) -> String {
        self.horizon_ms = Some(notice.horizon_ms);
        self.last_notice_ms = Some(notice.at_ms);
        self.encoded_horizon()
    }

    /// One notice per failed outbox row. The same id is never announced twice.
    pub(crate) fn failure(&mut self, settings: &DaemonSettings, now_ms: i64, failed: Option<&str>) -> Option<Notice> {
        let Some(failed) = failed else {
            self.last_failed = None;
            return None;
        };
        if self.last_failed.as_deref() == Some(failed) {
            return None;
        }
        if !settings.notify_enabled || self.debounced(settings, now_ms) {
            // Leave `last_failed` untouched so a debounced failure can still be announced later.
            return None;
        }
        self.last_failed = Some(failed.to_owned());
        self.last_notice_ms = Some(now_ms);
        Some(Notice::new(FAILURE_TITLE, FAILURE_BODY))
    }

    fn debounced(&self, settings: &DaemonSettings, now_ms: i64) -> bool {
        let window = i64::try_from(settings.debounce_secs.max(60)).unwrap_or(60).saturating_mul(1000);
        self.last_notice_ms.is_some_and(|last| now_ms.saturating_sub(last) < window)
    }
}

/// Start the tray if the user enabled it. Failure is a normal state: log once, daemon keeps serving.
/// Returns whether the icon is actually running, so the daemon does not poll an absent tray.
pub(crate) fn start(state: &DaemonState) -> bool {
    let settings = state.settings();
    if !settings.tray {
        return false;
    }
    let Some(tray) = &state.tray else {
        return false;
    };
    let launch = TrayLaunch { store: state.config.store_path.clone() };
    match tray.start(&launch) {
        Ok(()) => {
            state.install_notifier(tray.notifier());
            state.set_tray_running(true);
            state.note_tray_started();
            refresh(state);
            true
        }
        Err(error) => {
            state.set_tray_running(false);
            state.note_tray_start_failed(error);
            false
        }
    }
}

pub(crate) fn stop(state: &DaemonState) {
    if !state.tray_running() {
        return;
    }
    if let Some(tray) = &state.tray {
        tray.stop();
    }
    state.set_tray_running(false);
}

/// Keep the icon in sync with the preference. A failed start is retried on a slow cadence so a late
/// Explorer does not require a daemon restart; a successful start is never repeated.
fn reconcile(state: &DaemonState, next_attempt: &mut std::time::Instant) {
    if state.settings().tray {
        if !state.tray_running() && std::time::Instant::now() >= *next_attempt {
            start(state);
            *next_attempt = std::time::Instant::now() + Duration::from_secs(5);
        }
    } else {
        stop(state);
    }
}

/// Poll the bounded recorder channel and run menu actions. No busy loop and no unbounded queue.
pub(crate) async fn run(state: Arc<DaemonState>) {
    let mut next_attempt = std::time::Instant::now();
    loop {
        reconcile(&state, &mut next_attempt);
        while let Some(action) = state.tray_action() {
            match action {
                TrayAction::Open | TrayAction::OpenOutbox => {
                    let (view, activation) = if action == TrayAction::OpenOutbox { (AppOpen::Outbox, "outbox") } else { (AppOpen::Mail, "open") };
                    // A running window is the only one that may answer: focus it instead of
                    // starting a second process that would compete for the same store.
                    if state.activate_app(activation) {
                        log::info!("activation delivered to the running app");
                    } else {
                        match state.launcher.open(&state.config.store_path, view) {
                            Ok(()) => log::info!("postino-app launch requested from the tray"),
                            Err(error) => log::warn!("the app could not be started from the tray: {error}"),
                        }
                    }
                }
                TrayAction::SyncNow => {
                    log::info!("sync requested from the tray");
                    state.request_sync();
                }
                TrayAction::Quit => {
                    log::info!("shutdown requested from the tray");
                    // A hidden window exits on this event before the pipe disappears; a window that
                    // misses it reconnects, falls back to the offline state and reopens itself.
                    if state.activate_app("quit") {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    state.shutdown.cancel();
                }
            }
            refresh(&state);
        }
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(150)) => {}
        }
    }
    stop(&state);
}

pub(crate) fn refresh(state: &DaemonState) {
    let (syncing, network, outbox_failed) = state.live_flags();
    let next = if outbox_failed {
        TrayState::Failed
    } else if syncing {
        TrayState::Syncing
    } else if network {
        TrayState::Offline
    } else {
        TrayState::Quiet
    };
    let Some(tray) = &state.tray else {
        return;
    };
    if let Err(error) = tray.set_state(next) {
        state.note_tray_error(error);
    }
}

/// After a successful sync: notify one new unread message, then persist the scalar horizon before
/// the notice is shown so a crash cannot repeat it.
pub(crate) async fn after_sync(state: &DaemonState) {
    let messages =
        state.with_engine(|engine| engine.messages().iter().filter(|message| message.unread).take(MAX_SCAN).cloned().collect::<Vec<_>>()).await;
    let messages = match messages {
        Ok(messages) => messages,
        Err(_) => {
            log::warn!("new-mail notice skipped: the store read failed");
            return;
        }
    };
    let settings = state.settings();
    let now = crate::now_ms();
    let notice = match state.notices.lock() {
        Ok(mut policy) => policy.new_mail(&settings, now, &messages),
        Err(_) => return,
    };
    let Some(notice) = notice else {
        refresh(state);
        return;
    };
    if !send(state, &notice.notice) {
        // No OS notice means no horizon update: the next pass retries instead of silently losing it.
        refresh(state);
        return;
    }
    let encoded = match state.notices.lock() {
        Ok(mut policy) => policy.accepted(&notice),
        Err(_) => return,
    };
    persist(state, META_HORIZON, &encoded).await;
    refresh(state);
}

/// After an outbox pass and at startup: keep the failed icon state and announce a new failed row.
pub(crate) async fn after_outbox(state: &DaemonState) {
    let rows = state.with_engine(|engine| engine.outbox_list(None, 200, 0)).await;
    let rows = match rows {
        Ok(Ok(rows)) => rows,
        Ok(Err(_)) => {
            log::warn!("outbox notice scan skipped: the store read failed");
            return;
        }
        Err(_) => {
            log::warn!("outbox notice scan skipped: the engine was busy");
            return;
        }
    };
    let failed = rows
        .entries
        .iter()
        .find(|entry| matches!(entry.state, postino_store::OutboxState::Failed | postino_store::OutboxState::Uncertain))
        .map(|entry| entry.id.clone());
    state.set_outbox_failed(failed.is_some());
    let settings = state.settings();
    let now = crate::now_ms();
    let (notice, encoded, changed) = match state.notices.lock() {
        Ok(mut policy) => {
            let previous = policy.failed();
            let notice = policy.failure(&settings, now, failed.as_deref());
            (notice, policy.failed(), previous != policy.failed())
        }
        Err(_) => return,
    };
    if changed {
        match encoded {
            Some(id) => persist(state, META_FAILED, &id).await,
            None => persist(state, META_FAILED, "").await,
        }
    }
    if let Some(notice) = notice {
        let _ = send(state, &notice);
    }
    refresh(state);
}

async fn persist(state: &DaemonState, key: &'static str, value: &str) {
    let value = value.to_owned();
    if state.with_engine(move |engine| engine.meta_set(key, &value)).await.is_err() {
        log::warn!("daemon notice state could not be persisted");
    }
}

/// Show one notice. `false` means no OS surface accepted it, so the caller must not record it as seen.
fn send(state: &DaemonState, notice: &Notice) -> bool {
    match state.notifier() {
        Some(notifier) => match notifier.notify(notice) {
            Ok(()) => {
                log::info!("notification shown from the daemon tray");
                true
            }
            Err(error) => {
                log::warn!("notification could not be shown: {error}");
                false
            }
        },
        None => {
            log::info!("notification skipped: the tray is not available");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postino_core::Address;
    use postino_platform::{Notifier, RecorderNotifier};

    fn engine_at(path: &std::path::Path, key: &postino_store::StoreKey) -> postino_core::Engine {
        let store = postino_store::Store::open(path, key).expect("open the test store");
        postino_core::Engine::open(store).expect("open the test engine")
    }

    fn message(id: &str, sender: &str, subject: &str, date: i64) -> MessageSummary {
        MessageSummary {
            id: id.to_owned(),
            thread_id: format!("t-{id}"),
            folder_id: "inbox".to_owned(),
            from: Address::new(sender, "sender@example.com"),
            to: vec![Address::new("", "me@example.com")],
            subject: subject.to_owned(),
            snippet: "snippet".to_owned(),
            date: chrono::DateTime::from_timestamp_millis(date).unwrap_or_default(),
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        }
    }

    fn notice_for(policy: &mut NoticePolicy, settings: &DaemonSettings, now: i64, messages: &[MessageSummary]) -> NewMailNotice {
        policy.new_mail(settings, now, messages).expect("one new-mail notice")
    }

    #[test]
    fn previews_off_notice_has_no_sender_or_subject() {
        let mut policy = NoticePolicy::default();
        let messages = vec![message("m1", "Alice", "Private subject", 1_000)];
        let draft = notice_for(&mut policy, &DaemonSettings::default(), 10_000, &messages);
        assert!(!draft.notice.body.contains("Alice"));
        assert!(!draft.notice.body.contains("Private subject"));
        assert_eq!(draft.notice.body, NEW_MAIL_BODY);
    }

    #[test]
    fn previews_on_include_sender_and_subject() {
        let mut policy = NoticePolicy::default();
        let messages = vec![message("m1", "Alice", "Private subject", 1_000)];
        let settings = DaemonSettings { notify_previews: true, ..DaemonSettings::default() };
        let draft = notice_for(&mut policy, &settings, 10_000, &messages);
        assert_eq!(draft.notice.body, "Alice: Private subject");
    }

    #[test]
    fn horizon_dedupes_and_debounce_limits_notices() {
        let mut policy = NoticePolicy::default();
        let settings = DaemonSettings::default();
        let first = vec![message("m1", "Alice", "One", 1_000)];
        let draft = notice_for(&mut policy, &settings, 100_000, &first);
        assert_eq!(policy.accepted(&draft), "1000");
        assert!(policy.new_mail(&settings, 200_000, &first).is_none(), "an accepted message must not repeat");

        let second = vec![message("m2", "Bob", "Two", 200_000)];
        assert!(policy.new_mail(&settings, 120_000, &second).is_none(), "inside the debounce window");
        let draft = notice_for(&mut policy, &settings, 160_000, &second);
        assert_eq!(policy.accepted(&draft), "200000");
    }

    #[test]
    fn a_large_mailbox_keeps_one_bounded_horizon() {
        let mut policy = NoticePolicy::default();
        let messages = (0..600).map(|index| message(&format!("m{index:03}"), "Alice", "Subject", 1_000 + i64::from(index))).collect::<Vec<_>>();
        let settings = DaemonSettings::default();
        let draft = notice_for(&mut policy, &settings, 10_000_000, &messages);
        assert_eq!(draft.notice.body, NEW_MAIL_BODY);
        let encoded = policy.accepted(&draft);
        assert_eq!(encoded, "1599");
        assert!(encoded.chars().count() <= 20, "the horizon is one scalar, not an id list above meta's 16 KiB limit");

        // Restart: the stored scalar must not re-announce the same 600 messages.
        let mut reopened = NoticePolicy { horizon_ms: encoded.parse().ok(), last_notice_ms: None, last_failed: None };
        assert!(reopened.new_mail(&settings, 100_000_000, &messages).is_none(), "a restart must not repeat announced mail");

        // One genuinely newer message still produces exactly one notice.
        let newer = vec![message("m600", "Bob", "New", 1_600)];
        assert!(reopened.new_mail(&settings, 100_000_000, &newer).is_some());
    }

    #[test]
    fn failure_notice_is_one_per_row() {
        let mut policy = NoticePolicy::default();
        let settings = DaemonSettings::default();
        let notice = policy.failure(&settings, 100_000, Some("out_1")).expect("one failure notice");
        assert_eq!(notice.body, FAILURE_BODY);
        assert!(policy.failure(&settings, 1_000_000, Some("out_1")).is_none());
        assert!(policy.failure(&settings, 1_000_000, Some("out_2")).is_some());
        assert!(policy.failure(&settings, 1_000_000, None).is_none());
    }

    #[test]
    fn recorder_notifier_keeps_pii_free_payload() {
        let notifier = RecorderNotifier::new();
        let notice = Notice::new(NEW_MAIL_TITLE, NEW_MAIL_BODY);
        assert!(notifier.notify(&notice).is_ok());
        let notices = notifier.notices();
        assert_eq!(notices.len(), 1);
        assert!(!notices[0].body.contains('@'));
    }

    #[test]
    fn stored_horizon_survives_reopen_and_never_repeats() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mail.db");
        let key = postino_store::StoreKey::from_bytes([9u8; 32]);
        drop(engine_at(&path, &key));
        {
            let mut engine = engine_at(&path, &key);
            engine.meta_set(META_HORIZON, "1000").expect("store the horizon");
        }

        let mut engine = engine_at(&path, &key);
        let mut policy = NoticePolicy::initialize(&mut engine);
        assert!(
            policy.new_mail(&DaemonSettings::default(), 10_000, &[message("m1", "Alice", "Old", 1_000)]).is_none(),
            "a restart must not repeat announced mail"
        );
        assert!(policy.new_mail(&DaemonSettings::default(), 200_000, &[message("m2", "Bob", "New", 100_000)]).is_some());
    }

    #[test]
    fn first_run_adopts_the_mailbox_scalar() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mail.db");
        let key = postino_store::StoreKey::from_bytes([8u8; 32]);
        let mut engine = engine_at(&path, &key);
        let policy = NoticePolicy::initialize(&mut engine);
        assert_eq!(policy.encoded_horizon(), "0");
        assert_eq!(engine.meta_get(META_HORIZON).expect("read meta").as_deref(), Some("0"));
    }
}
