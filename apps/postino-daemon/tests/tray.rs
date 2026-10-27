//! Daemon-owned tray and notification seams, exercised through recorders. No test touches the real
//! notification area, the registry Run key or a GUI process.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postino_daemon::tray_host::{AppLauncher, AppOpen};
use postino_ipc::client::Client;
use postino_ipc::protocol::method;
use postino_platform::{RecorderTray, TrayAction};
use serde_json::json;

#[derive(Default)]
struct RecordingLauncher {
    opened: Mutex<Vec<(PathBuf, AppOpen)>>,
}

impl AppLauncher for RecordingLauncher {
    fn open(&self, store: &Path, view: AppOpen) -> Result<(), String> {
        self.opened.lock().map_err(|_| "recorder poisoned".to_owned())?.push((store.to_path_buf(), view));
        Ok(())
    }
}

impl RecordingLauncher {
    fn opened(&self) -> Vec<(PathBuf, AppOpen)> {
        self.opened.lock().map(|opened| opened.clone()).unwrap_or_default()
    }
}

async fn next_activation(client: &mut Client) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let event = tokio::time::timeout(Duration::from_secs(2), client.next_event(Duration::from_millis(200)))
            .await
            .expect("an event within the deadline")
            .expect("read event")
            .expect("an event body");
        if event["kind"] == "activation" {
            return event;
        }
    }
    panic!("no activation event arrived");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tray_actions_activate_the_running_window_instead_of_starting_a_second_one() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let tray = Arc::new(RecorderTray::new());
    let launcher = Arc::new(RecordingLauncher::default());
    let mut config = fixture.config();
    config.tray = Some(tray.clone());
    config.app_launcher = launcher.clone();
    let daemon = postino_daemon::Daemon::spawn(config).expect("daemon");
    assert!(support::wait_for(Duration::from_secs(5), || tray.started()).await, "the recorder tray was not started");

    // No window is listening yet: the tray starts exactly one process.
    tray.push_action(TrayAction::Open);
    assert!(
        support::wait_for(Duration::from_secs(5), || launcher.opened() == vec![(fixture.store_path.clone(), AppOpen::Mail)]).await,
        "an absent window is started from the tray"
    );
    let opened_before = launcher.opened();

    // A window connects and subscribes: from now on the tray delivers an activation and never
    // starts a second process that would contend for the same store.
    let mut client = Client::connect(&fixture.paths(), "tray-test", Duration::from_secs(2), Duration::from_secs(5)).await.expect("client");
    client.subscribe(Duration::from_secs(5)).await.expect("subscribe");
    tray.push_action(TrayAction::Open);
    let event = next_activation(&mut client).await;
    assert_eq!(event["data"]["action"], "open");
    tray.push_action(TrayAction::OpenOutbox);
    let event = next_activation(&mut client).await;
    assert_eq!(event["data"]["action"], "outbox");
    assert_eq!(launcher.opened(), opened_before, "a connected window must be focused, not duplicated");

    let before = manual_syncs(&mut client).await;
    tray.push_action(TrayAction::SyncNow);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut after = before;
    while std::time::Instant::now() < deadline {
        after = manual_syncs(&mut client).await;
        if after > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(after, before + 1, "the tray sync action must reach the scheduler exactly once, not ride a timer wake");

    // Quit tells the window to exit, then stops the daemon: no process is left with no icon.
    tray.push_action(TrayAction::Quit);
    let event = next_activation(&mut client).await;
    assert_eq!(event["data"]["action"], "quit");
    drop(client);
    tokio::task::spawn_blocking(move || daemon.wait()).await.expect("join task").expect("daemon exit");
    assert_eq!(launcher.opened(), opened_before, "quit never starts a process");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_settings_switch_starts_and_stops_the_tray_icon() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let tray = Arc::new(RecorderTray::new());
    let mut config = fixture.config();
    config.tray = Some(tray.clone());
    let directory = tempfile::tempdir().expect("tempdir");
    config.settings_path = Some(directory.path().join("daemon.json"));
    let daemon = postino_daemon::Daemon::spawn(config).expect("daemon");
    assert!(support::wait_for(Duration::from_secs(5), || tray.started()).await, "the tray must start when the setting is on");
    let launches = tray.launches().len();

    let mut client = Client::connect(&fixture.paths(), "tray-switch", Duration::from_secs(2), Duration::from_secs(5)).await.expect("client");
    client.call(method::DAEMON_SETTINGS_SET, json!({ "tray": false }), Duration::from_secs(5)).await.expect("disable the tray");
    assert!(support::wait_for(Duration::from_secs(5), || !tray.started()).await, "turning the tray off must stop the icon, not only persist a bool");
    client.call(method::DAEMON_SETTINGS_SET, json!({ "tray": true }), Duration::from_secs(5)).await.expect("enable the tray");
    assert!(
        support::wait_for(Duration::from_secs(5), || tray.started() && tray.launches().len() > launches).await,
        "turning the tray back on must start the icon again"
    );
    drop(client);
    daemon.shutdown().expect("shutdown");
}

async fn manual_syncs(client: &mut Client) -> u64 {
    client.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(5)).await.expect("status")["manual_syncs"].as_u64().unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_outbox_failure_notifies_once_without_pii() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    support::seed_outbox_state(&fixture, "out_1", "failed");
    let tray = Arc::new(RecorderTray::new());
    let notifier = Arc::new(postino_platform::RecorderNotifier::new());
    let mut config = fixture.config();
    config.tray = Some(tray.clone());
    config.notifier = Some(notifier.clone());
    let daemon = postino_daemon::Daemon::spawn(config).expect("daemon");
    assert!(support::wait_for(Duration::from_secs(5), || !notifier.notices().is_empty()).await, "a failed outbox row did not produce a notice");
    let notices = notifier.notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].body.contains("Coda di invio"));
    assert!(!notices[0].body.contains("out_1"));
    assert!(!notices[0].body.contains('@'));
    daemon.shutdown().expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tray_that_cannot_start_never_stops_the_daemon() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let mut config = fixture.config();
    config.tray = Some(Arc::new(RecorderTray::failing()));
    let daemon = postino_daemon::Daemon::spawn(config).expect("daemon");
    let mut client = Client::connect(&fixture.paths(), "no-tray", Duration::from_secs(2), Duration::from_secs(5)).await.expect("client");
    let status = client.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(5)).await.expect("status");
    assert!(status["pid"].as_u64().is_some());
    drop(client);
    daemon.shutdown().expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_bridge_reads_writes_and_persists_every_row() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let Ok(dir) = tempfile::tempdir() else { return };
    let path = dir.path().join("daemon.json");
    let mut config = fixture.config();
    config.settings_path = Some(path.clone());
    config.settings = postino_daemon::DaemonSettings::load(&path);
    let daemon = postino_daemon::Daemon::spawn(config).expect("daemon");
    let mut client = Client::connect(&fixture.paths(), "settings-test", Duration::from_secs(2), Duration::from_secs(5)).await.expect("client");

    let read = client.call(method::DAEMON_SETTINGS, json!({}), Duration::from_secs(5)).await.expect("read settings");
    assert_eq!(read["tray"], json!(true));
    assert_eq!(read["notify_enabled"], json!(true));
    assert_eq!(read["notify_previews"], json!(false));
    assert_eq!(read["tray_available"], json!(false), "this fixture injects no tray seam");
    assert_eq!(read["tray_running"], json!(false));

    let written = client
        .call(method::DAEMON_SETTINGS_SET, json!({ "tray": false, "notify_previews": true, "debounce_secs": 5 }), Duration::from_secs(5))
        .await
        .expect("write settings");
    assert_eq!(written["tray"], json!(false));
    assert_eq!(written["notify_previews"], json!(true));
    assert_eq!(written["debounce_secs"], json!(60), "a hand-edited debounce below the floor is clamped, not rejected");
    assert_eq!(
        postino_daemon::DaemonSettings::load(&path),
        postino_daemon::DaemonSettings { tray: false, notify_enabled: true, notify_previews: true, debounce_secs: 60 }
    );

    let again = client.call(method::DAEMON_SETTINGS, json!({}), Duration::from_secs(5)).await.expect("read back");
    assert_eq!(again, written);

    let typo = client.call(method::DAEMON_SETTINGS_SET, json!({ "trayy": true }), Duration::from_secs(5)).await;
    assert!(
        matches!(typo, Err(postino_ipc::IpcError::Rpc(ref error)) if error.code == postino_ipc::error_code::INVALID_PARAMS),
        "an unknown setting must be refused: {typo:?}"
    );
    let empty = client.call(method::DAEMON_SETTINGS_SET, json!({}), Duration::from_secs(5)).await;
    assert!(empty.is_err(), "an empty patch must not pretend it saved anything");

    drop(client);
    daemon.shutdown().expect("shutdown");
}
