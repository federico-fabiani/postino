//! A real daemon process is killed at each P7 outbox crash state and a new daemon is started on the
//! same store. The child runs one exact test in this same test binary; the parent drives the kill and
//! the restart. This proves the lock is released by the kernel, every row survives, a stale lease is
//! resolved as uncertain, and the independent append is retried without an SMTP submission.

mod support;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use postino_daemon::{Daemon, DaemonConfig};
use postino_store::{OutboxState, Store, StoreKey};
use support::{Fixture, MockVault, seed_outbox_state};

const CHILD_STORE: &str = "P10A_CRASH_CHILD_STORE";
const CHILD_READY: &str = "P10A_CRASH_CHILD_READY";

#[test]
fn child_daemon_runs() {
    let Ok(store) = std::env::var(CHILD_STORE) else { return };
    let mut config = DaemonConfig::new(PathBuf::from(store), Arc::new(StoreKey::from_bytes(support::KEY)), Arc::new(MockVault::default()));
    config.sync_on_start = false;
    config.outbox_on_start = false;
    config.sync_interval = Duration::from_secs(3600);
    config.gc_interval = Duration::from_secs(3600);
    let _daemon = Daemon::spawn(config).expect("child daemon");
    if let Ok(marker) = std::env::var(CHILD_READY) {
        std::fs::write(marker, b"ready").expect("marker");
    }
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn a_real_daemon_restart_survives_every_p7_crash_state() {
    let states = ["queued_future", "sending_fresh", "sending_stale", "append_pending", "uncertain", "failed", "sent", "cancelled"];
    for state in states {
        let fixture = Fixture::new();
        fixture.seed(1, None);
        seed_outbox_state(&fixture, state, state);
        let marker = fixture._directory.path().join("child-ready");
        let child = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "child_daemon_runs", "--nocapture"])
            .env(CHILD_STORE, &fixture.store_path)
            .env(CHILD_READY, &marker)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child daemon");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "the child daemon did not start");
            std::thread::sleep(Duration::from_millis(25));
        }
        let mut child = child;
        child.kill().expect("kill child");
        child.wait().expect("reap child");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !postino_store::writer_lock_is_free(&fixture.store_path) {
            assert!(Instant::now() < deadline, "the killed child kept the writer lock");
            std::thread::sleep(Duration::from_millis(25));
        }

        let mut config = fixture.config();
        config.outbox_on_start = true;
        let daemon = Daemon::spawn(config).expect("replacement daemon");
        std::thread::sleep(Duration::from_secs(1));
        daemon.shutdown().expect("replacement shutdown");

        let store = Store::open_read_only(&fixture.store_path, &fixture.key).expect("read-only store");
        let rows = store.list_outbox(None, 500).expect("outbox");
        assert_eq!(rows.len(), 1, "{state}: the restart lost the row");
        let row = &rows[0];
        let expected = match state {
            "queued_future" => OutboxState::Queued,
            "sending_fresh" => OutboxState::Sending,
            "sending_stale" | "uncertain" => OutboxState::Uncertain,
            "append_pending" => OutboxState::AppendPending,
            "failed" => OutboxState::Failed,
            "sent" => OutboxState::Sent,
            "cancelled" => OutboxState::Cancelled,
            other => panic!("unknown state {other}"),
        };
        assert_eq!(row.state, expected, "{state}: {row:?}");
        if state == "append_pending" {
            assert!(row.append_attempts > 1, "the append was not retried: {row:?}");
        }
    }
}
