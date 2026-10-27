//! The daemon must never open a TCP port. This test lives in its own integration binary so the
//! test harness's own listeners cannot be mistaken for the daemon's.

mod support;

use std::time::Duration;

use postino_daemon::Daemon;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_opens_no_tcp_listener() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let daemon = Daemon::spawn(fixture.config()).expect("daemon");
    tokio::time::sleep(Duration::from_millis(500)).await;
    #[cfg(windows)]
    {
        let pid = std::process::id().to_string();
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Get-NetTCPConnection -OwningProcess {pid} -State Listen -ErrorAction SilentlyContinue | Measure-Object | Select-Object -ExpandProperty Count"),
            ])
            .output()
            .expect("run Get-NetTCPConnection");
        let count = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_eq!(count, "0", "the daemon process owns a TCP listener: {count}");
    }
    daemon.shutdown().expect("shutdown");
}
