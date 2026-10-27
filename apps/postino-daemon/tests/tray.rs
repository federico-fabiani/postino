//! Daemon-owned tray and notification seams, exercised through recorders. No test touches the real
//! notification area, the registry Run key or a GUI process.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postino_daemon::tray_host::AppLauncher;
use postino_ipc::client::Client;
use postino_ipc::protocol::method;
use postino_platform::{RecorderTray, TrayAction};
use serde_json::json;

#[derive(Default)]
struct RecordingLauncher {
    opened: Mutex<Vec<PathBuf>>,
}

impl AppLauncher for RecordingLauncher {
    fn open(&self, store: &Path) -> Result<(), String> {
        self.opened.lock().map_err(|_| "recorder poisoned".to_owned())?.push(store.to_path_buf());
        Ok(())
    }
}

impl RecordingLauncher {
    fn opened(&self) -> Vec<PathBuf> {
        self.opened.lock().map(|opened| opened.clone()).unwrap_or_default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tray_menu_actions_open_the_app_sync_and_stop_the_daemon() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let tray = Arc::new(RecorderTray::new());
    let launcher = Arc::new(RecordingLauncher::default());
    let mut config = fixture.config();
    config.tray = Some(tray.clone());
    config.app_launcher = launcher.clone();
    let daemon = postino_daemon::Daemon::spawn(config).expect("daemon");
    assert!(support::wait_for(Duration::from_secs(5), || tray.started()).await, "the recorder tray was not started");

    tray.push_action(TrayAction::Open);
    assert!(
        support::wait_for(Duration::from_secs(5), || launcher.opened() == vec![fixture.store_path.clone()]).await,
        "the open action did not launch the app with the daemon store"
    );

    let mut client = Client::connect(&fixture.paths(), "tray-test", Duration::from_secs(2), Duration::from_secs(5)).await.expect("client");
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
    drop(client);

    tray.push_action(TrayAction::Quit);
    tokio::task::spawn_blocking(move || daemon.wait()).await.expect("join task").expect("daemon exit");
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
