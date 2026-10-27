//! `postino daemon autostart status|off` must work without a daemon binary and from a relative
//! `--store`. Debug builds redirect the Run key to a throwaway `HKCU\Software\Postino\Tests` key, so
//! no test reads or writes the real `Postino` autostart value.

#![cfg(windows)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn test_key() -> String {
    format!(r"Software\Postino\Tests\cli-autostart-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst))
}

fn run(args: &[&str], key: &str, daemon_bin: &Path) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_postino"))
        .args(args)
        .env("POSTINO_AUTOSTART_KEY", key)
        .env("POSTINO_DAEMON_BIN", daemon_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run the postino binary");
    (output.status.code().unwrap_or(-1), String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

fn reg(args: &[&str]) -> bool {
    Command::new("reg.exe").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|status| status.success())
}

struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = reg(&["delete", &format!(r"HKCU\{}", self.0), "/f"]);
    }
}

#[test]
fn status_and_off_do_not_need_the_daemon_binary() {
    let key = test_key();
    let _cleanup = Cleanup(key.clone());
    let directory = tempfile::tempdir().expect("tempdir");
    let missing = directory.path().join("no-postino-daemon-here.exe");
    // A relative store: `off` must not depend on where the CLI was started from either.
    fn args(command: &'static str) -> Vec<&'static str> {
        vec!["--store", "autostart-relative-mail.db", "daemon", "autostart", command]
    }

    let (code, stdout, stderr) = run(&args("status"), &key, &missing);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Autostart non attivo"), "{stdout}");

    assert!(reg(&["add", &format!(r"HKCU\{key}"), "/v", "Postino", "/d", "stale-command", "/f"]), "seed the test value");
    let (code, stdout, stderr) = run(&args("off"), &key, &missing);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Autostart disattivato"), "{stdout}");
    assert!(!reg(&["query", &format!(r"HKCU\{key}"), "/v", "Postino"]), "off must remove the stored value");
}

#[test]
fn on_still_refuses_a_missing_daemon_binary() {
    let key = test_key();
    let _cleanup = Cleanup(key.clone());
    let directory = tempfile::tempdir().expect("tempdir");
    let missing = directory.path().join("no-postino-daemon-here.exe");
    let (code, _, stderr) = run(&["--store", "autostart-relative-mail.db", "daemon", "autostart", "on"], &key, &missing);
    assert_ne!(code, 0, "on must not write a command for a binary that does not exist");
    assert!(stderr.contains("POSTINO_DAEMON_BIN"), "{stderr}");
}
