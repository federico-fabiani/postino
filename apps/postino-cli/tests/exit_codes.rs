//! CLI exit codes: 0 success, 1 operation failure, 2 usage.
//!
//! These cases are deliberately shell-free: every command either runs on the synthetic demo mailbox
//! or fails argument parsing before `open_engine`, so no test can open the real default store or take
//! the real keyring.

use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_postino"));
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = command.spawn().expect("spawn the postino binary");
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(text.as_bytes()).expect("write stdin");
    }
    let output = child.wait_with_output().expect("wait for postino");
    let code = output.status.code().expect("the process exited with a code");
    (code, String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn help_is_a_success() {
    let (code, stdout, _) = run(&["--help"], None);
    assert_eq!(code, 0);
    assert!(stdout.contains("account"), "{stdout}");
}

#[test]
fn a_missing_required_argument_is_a_usage_error() {
    let (code, _, stderr) = run(&["account", "add", "--email", "me@example.com"], None);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--provider"), "{stderr}");
}

#[test]
fn invalid_arguments_and_unknown_ids_are_usage_errors() {
    // Missing --yes: the operation rejects confirm=false.
    let (code, _, stderr) = run(&["--demo", "account", "remove", "acc_demo"], None);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("confirm"), "{stderr}");

    // An invalid address is rejected before any password is read (stdin is null).
    let (code, _, stderr) = run(
        &[
            "--demo",
            "account",
            "add",
            "--provider",
            "custom",
            "--email",
            "not-an-address",
            "--host",
            "localhost",
            "--port",
            "1",
            "--tls",
            "implicit",
            "--password-stdin",
        ],
        None,
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("email"), "{stderr}");

    // Control characters in the display name.
    let (code, _, stderr) = run(
        &[
            "--demo",
            "account",
            "add",
            "--provider",
            "custom",
            "--email",
            "me@example.com",
            "--name",
            "bad\nname",
            "--host",
            "localhost",
            "--port",
            "1",
            "--tls",
            "implicit",
            "--password-stdin",
        ],
        None,
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("name"), "{stderr}");

    // An empty custom host is refused before the store is consulted.
    let (code, _, stderr) = run(
        &[
            "--demo",
            "account",
            "add",
            "--provider",
            "custom",
            "--email",
            "me@example.com",
            "--host",
            "",
            "--port",
            "1",
            "--tls",
            "implicit",
            "--password-stdin",
        ],
        None,
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("host"), "{stderr}");

    let (code, _, stderr) = run(&["--demo", "account", "test", "acc_missing"], None);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("unknown account"), "{stderr}");
}

#[test]
fn malformed_run_arguments_are_a_usage_error() {
    let (code, _, stderr) = run(&["--demo", "run", "messages_list", "--args", "{bad"], None);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--args is not JSON"), "{stderr}");
}

#[test]
fn conflicting_mode_flags_are_a_usage_error() {
    let (code, _, stderr) = run(&["--demo", "--store", "somewhere.db", "info"], None);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--store"), "{stderr}");
}

#[test]
fn an_operation_failure_exits_with_one_and_the_demo_never_opens_a_store() {
    let (code, _, stderr) = run(
        &[
            "--demo",
            "account",
            "add",
            "--provider",
            "custom",
            "--email",
            "me@example.com",
            "--host",
            "localhost",
            "--port",
            "993",
            "--tls",
            "implicit",
            "--password-stdin",
        ],
        Some("not-used\n"),
    );
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("demo"), "{stderr}");
    assert!(!stderr.contains("not-used"), "{stderr}");
}

#[test]
fn account_list_on_the_demo_succeeds_and_prints_no_secret() {
    let (code, stdout, stderr) = run(&["--demo", "account", "list"], None);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("federico@example.it"), "{stdout}");
    assert!(!stdout.to_lowercase().contains("password"), "{stdout}");
}
