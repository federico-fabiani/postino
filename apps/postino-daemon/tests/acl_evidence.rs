//! Manual ACL evidence for the token file and named pipe. Ignored by default because it shells out
//! to PowerShell for the human-readable ACL; the automated owner-only check lives in `postino-ipc`.

mod support;

use postino_daemon::Daemon;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual ACL evidence on the Windows host"]
async fn prints_the_token_acl_and_pipe_acl() {
    let fixture = support::Fixture::new();
    fixture.seed(1, None);
    let daemon = Daemon::spawn(fixture.config()).expect("daemon");
    let token = daemon.paths().token_file.display().to_string();
    let endpoint = daemon.paths().endpoint.describe();
    #[cfg(windows)]
    {
        let owner_only = postino_ipc::pipe_acl_is_owner_only(&endpoint).expect("read the live pipe DACL");
        println!("=== pipe DACL owner-only: {owner_only}");
        assert!(owner_only, "the live pipe DACL is not one ACE for the current user");
    }
    for (label, path) in [("token", token.as_str()), ("pipe", endpoint.as_str())] {
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("Get-Acl -Path '{path}' | Format-List")])
            .output()
            .expect("Get-Acl");
        println!("=== {label} {path}\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
    daemon.shutdown().expect("shutdown");
}
