//! CLI ↔ daemon transcript: an existing command, status/start/stop, the `--in-process` refusal and
//! the MCP stdio server talking to a live daemon. No test opens the OS keyring: the daemon uses a
//! fixed test key and an in-memory vault, and a client only reads the token file.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use postino_core::{CredentialVault, VaultError};
use postino_daemon::{Daemon, DaemonConfig};
use postino_store::{Account, AccountServer, AccountTls, Folder, FolderKind, Store, StoreKey};
use zeroize::Zeroizing;

const KEY: [u8; 32] = [0x55; 32];
const ACCOUNT: &str = "acc_test";

#[derive(Default)]
struct Vault;

impl CredentialVault for Vault {
    fn store_imap_password(&self, _account_id: &str, _password: &[u8]) -> Result<(), VaultError> {
        Ok(())
    }
    fn load_imap_password(&self, _account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        Ok(Some(Zeroizing::new(b"password".to_vec())))
    }
    fn delete_imap_password(&self, _account_id: &str) -> Result<(), VaultError> {
        Ok(())
    }
    fn store_smtp_password(&self, _account_id: &str, _password: &[u8]) -> Result<(), VaultError> {
        Ok(())
    }
    fn load_smtp_password(&self, _account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        Ok(None)
    }
    fn delete_smtp_password(&self, _account_id: &str) -> Result<(), VaultError> {
        Ok(())
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: std::path::PathBuf,
    key: Arc<StoreKey>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        let key = Arc::new(StoreKey::from_bytes(KEY));
        let database = Store::open(&store, &key).expect("store");
        database
            .save_account(
                &Account {
                    id: ACCOUNT.to_owned(),
                    name: "Test".to_owned(),
                    email: "user@example.com".to_owned(),
                    provider: "custom".to_owned(),
                    signature: String::new(),
                    created_at: 1,
                },
                &AccountServer {
                    account_id: ACCOUNT.to_owned(),
                    host: "localhost".to_owned(),
                    port: 1,
                    tls: AccountTls::Implicit,
                    username: "user@example.com".to_owned(),
                    created_at: 1,
                    smtp: None,
                },
                &[
                    Folder {
                        id: "inbox".to_owned(),
                        account_id: ACCOUNT.to_owned(),
                        name: "Inbox".to_owned(),
                        kind: FolderKind::Inbox,
                        unread: 0,
                        total: 0,
                        uid_validity: Some(1),
                        uid_next: None,
                        last_sync_at: None,
                    },
                    Folder {
                        id: "drafts".to_owned(),
                        account_id: ACCOUNT.to_owned(),
                        name: "Drafts".to_owned(),
                        kind: FolderKind::Drafts,
                        unread: 0,
                        total: 0,
                        uid_validity: Some(1),
                        uid_next: None,
                        last_sync_at: None,
                    },
                ],
            )
            .expect("account");
        Self { _directory: directory, store, key }
    }

    fn daemon(&self) -> Daemon {
        let mut config = DaemonConfig::new(self.store.clone(), Arc::clone(&self.key), Arc::new(Vault));
        config.sync_on_start = false;
        config.sync_jitter = false;
        config.sync_interval = Duration::from_secs(3600);
        config.gc_interval = Duration::from_secs(3600);
        Daemon::spawn(config).expect("daemon")
    }

    fn run(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> (i32, String, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_postino"));
        command.arg("--store").arg(&self.store).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        command.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn postino");
        if let Some(text) = stdin {
            child.stdin.take().expect("stdin").write_all(text.as_bytes()).expect("write stdin");
        }
        let output = child.wait_with_output().expect("postino output");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

#[test]
fn existing_commands_status_stop_and_mcp_go_through_the_daemon() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon();

    // A read-only existing command uses the daemon, not a local store connection.
    let (code, stdout, stderr) = fixture.run(&["--json", "accounts"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("user@example.com"), "{stdout}");

    // A mutation writes through the daemon and is visible on disk after it stops.
    let (code, stdout, stderr) =
        fixture.run(&["--json", "draft", "save", "--to", "dest@example.com", "--subject", "Through the daemon", "--body", "hello"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("\"draft\""), "{stdout}");

    let (code, stdout, stderr) = fixture.run(&["--json", "daemon", "status"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("\"pid\""), "{stdout}");

    // `daemon start` is idempotent while the endpoint is live.
    let (code, stdout, stderr) = fixture.run(&["--json", "daemon", "start"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("already_running"), "{stdout}");

    // MCP stays on stdio and becomes a daemon client; draft_save is not a send-gated tool.
    let mcp_input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"draft_save\",\"arguments\":{\"to\":[{\"name\":\"\",\"email\":\"mcp@example.com\"}],\"subject\":\"From MCP\",\"body_text\":\"via daemon\"}}}\n"
    );
    let (code, stdout, stderr) = fixture.run(&["mcp"], Some(mcp_input), &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("From MCP"), "{stdout}");
    assert!(!stdout.contains("isError"), "{stdout}");

    // A direct writer is refused while the daemon owns the store. `--in-process` takes the writer
    // lock even for a read-only command.
    let (code, _stdout, stderr) = fixture.run(&["--in-process", "accounts"], None, &[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("owned") || stderr.contains("possiede"), "{stderr}");
    let (code, _stdout, stderr) =
        fixture.run(&["--in-process", "draft", "save", "--to", "direct@example.com", "--subject", "Direct", "--body", "x"], None, &[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("owned") || stderr.contains("possiede"), "{stderr}");

    let (code, stdout, stderr) = fixture.run(&["--json", "daemon", "stop"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("false"), "{stdout}");
    daemon.wait().expect("daemon stops");
    drop(Store::open_locked(&fixture.store, &fixture.key).expect("the writer lock is released after daemon stop"));
    let store = Store::open_read_only(&fixture.store, &fixture.key).expect("read-only");
    let rows = store.list_messages(&Default::default()).expect("messages");
    assert!(rows.total >= 1, "the daemon mutation is not on disk: {rows:?}");
    let (code, _stdout, stderr) = fixture.run(&["--json", "daemon", "status"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_large_attachment_over_ipc_is_refused_readably() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon();
    let (code, stdout, stderr) = fixture.run(&["--json", "draft", "save", "--to", "dest@example.com", "--subject", "Big", "--body", "x"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    let draft: serde_json::Value = serde_json::from_str(&stdout).expect("draft JSON");
    let draft_id = draft["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
    let path = fixture._directory.path().join("large.bin");
    std::fs::write(&path, vec![0u8; 25 * 1024 * 1024]).expect("large file");
    let path_text = path.to_string_lossy().into_owned();
    let (code, _stdout, stderr) = fixture.run(&["--json", "draft", "attach", &draft_id, &path_text], None, &[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("16 MiB"), "{stderr}");
    // The connection is still usable after the preflight refusal.
    let (code, stdout, stderr) = fixture.run(&["--json", "accounts"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("user@example.com"), "{stdout}");
    daemon.shutdown().expect("shutdown");
}

#[test]
fn a_large_dry_run_response_is_refused_readably_and_the_connection_survives() {
    let fixture = Fixture::new();
    let store = Store::open(&fixture.store, &fixture.key).expect("store");
    let mut server = store.get_account_server(ACCOUNT).expect("read").expect("account server");
    server.smtp =
        Some(postino_store::SmtpServer { host: "localhost".to_owned(), port: 1, tls: AccountTls::Implicit, username: "user@example.com".to_owned() });
    store.upsert_account_server(&server).expect("smtp endpoint");
    let mut engine = postino_core::Engine::open_with_vault(store, Arc::new(Vault)).expect("engine");
    let draft = engine
        .save_draft(
            Some(ACCOUNT),
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("", "dest@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Large dry run".to_owned(),
                body_text: "x".to_owned(),
                attachments: Vec::new(),
                in_reply_to: None,
            },
        )
        .expect("draft");
    engine.add_draft_attachment(&draft.summary.id, "large.bin", Some("application/octet-stream"), &vec![0u8; 20 * 1024 * 1024]).expect("attachment");
    let draft_id = draft.summary.id;
    drop(engine);
    let daemon = fixture.daemon();
    let (code, _stdout, stderr) = fixture.run(&["--json", "send", "--draft", &draft_id, "--dry-run"], None, &[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("16 MiB") && stderr.contains("risposta"), "{stderr}");
    let (code, stdout, stderr) = fixture.run(&["--json", "accounts"], None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("user@example.com"), "{stdout}");
    daemon.shutdown().expect("shutdown");
}

#[test]
fn a_mutation_without_a_daemon_refuses_instead_of_writing_a_second_copy() {
    let fixture = Fixture::new();
    let (code, _stdout, stderr) = fixture.run(
        &["--json", "draft", "save", "--to", "dest@example.com", "--subject", "No daemon", "--body", "x"],
        None,
        &[("POSTINO_DAEMON_BIN", r"C:\definitely\missing\postino-daemon.exe")],
    );
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("non è in esecuzione"), "{stderr}");
    let (code, _stdout, stderr) = fixture.run(&["daemon", "start"], None, &[("POSTINO_DAEMON_BIN", r"C:\definitely\missing\postino-daemon.exe")]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("not a file") || stderr.contains("non ha aperto"), "{stderr}");
    let store = Store::open_read_only(&fixture.store, &fixture.key).expect("read-only");
    let rows = store.list_messages(&postino_store::MessageFilter::default()).expect("messages");
    assert_eq!(rows.total, 0, "a refused mutation wrote a row");
}
