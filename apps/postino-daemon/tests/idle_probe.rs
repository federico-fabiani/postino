//! Manual idle probe. Ignored by default: run it on the Windows host to report CPU and RSS.
//! `cargo test -p postino-daemon --test idle_probe -- --ignored --nocapture`

mod support;

use std::time::Duration;

use postino_daemon::Daemon;
use postino_ipc::protocol::method;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual idle CPU/RSS probe"]
async fn reports_idle_cpu_and_rss() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let mut config = fixture.config();
    config.sync_interval = Duration::from_secs(3600);
    config.gc_interval = Duration::from_secs(3600);
    let daemon = Daemon::spawn(config).expect("daemon");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pid = std::process::id().to_string();
    let sample = || -> (f64, f64) {
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("$p = Get-Process -Id {pid}; \"$($p.CPU) $($p.WorkingSet64)\"")])
            .output()
            .expect("Get-Process");
        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.split_whitespace();
        (fields.next().and_then(|value| value.parse().ok()).unwrap_or(0.0), fields.next().and_then(|value| value.parse().ok()).unwrap_or(0.0))
    };
    let (cpu_start, rss_start) = sample();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let (cpu_end, rss_end) = sample();
    let cores = std::env::var("NUMBER_OF_PROCESSORS").ok().and_then(|value| value.parse::<f64>().ok()).unwrap_or(1.0);
    let cpu_percent = (cpu_end - cpu_start) / 10.0 / cores * 100.0;
    let mut client = fixture.client("idle-probe").await;
    let status = client.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(5)).await.expect("status");
    println!(
        "idle: cpu={cpu_percent:.3}% (delta {:.3}s over 10s, {cores} cores), rss_start={:.1} MiB, rss_end={:.1} MiB, scheduler_wakes={}",
        cpu_end - cpu_start,
        rss_start / (1024.0 * 1024.0),
        rss_end / (1024.0 * 1024.0),
        status["scheduler_wakes"].as_u64().unwrap_or(0)
    );
    drop(client);
    daemon.shutdown().expect("shutdown");
}
