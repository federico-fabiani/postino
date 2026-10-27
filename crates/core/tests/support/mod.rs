//! Shared scaffolding for the P6a account lifecycle tests.
//!
//! A store in a throwaway directory, an in-memory credential vault (the real keyring is never
//! touched), and a fake IMAP server on its own thread with a test CA. The server can accept several
//! connections on one port, so a test can add an account, sync it twice and fail a third run against
//! the same stored endpoint.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::future::Future;

use postino_core::{AccountSetup, CredentialVault, MailTls, OAuthGrant, Provider, VaultError};
use postino_net::{CertificateDer, ImapConfig, Secret, SmtpConfig, TlsMode};
use postino_store::{Account, AccountServer, AccountTls, Folder, FolderKind, SmtpServer, Store, StoreKey};
use postino_testkit::{
    Conn, FakeServer, FakeSmtp, MailboxScript, SmtpConn, TestFolder, TlsFixture, USER, serve_mailbox_with, serve_mailbox_with_xoauth2,
};
use zeroize::Zeroizing;

pub const ACCOUNT_ID: &str = "acc_test";

/// A throwaway SQLCipher store with a random key.
pub struct Harness {
    dir: tempfile::TempDir,
    key: StoreKey,
}

impl Harness {
    pub fn new() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS random source");
        Self { dir: tempfile::tempdir().expect("temp dir"), key: StoreKey::from_bytes(bytes) }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("mail.db")
    }

    pub fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("open the store")
    }

    /// Open the same database read-only after it has been created and migrated by [`Harness::open`].
    pub fn read_only(&self) -> Store {
        Store::open_read_only(&self.path(), &self.key).expect("open the store read-only")
    }
}

/// The credential seam used by every test. Nothing here can reach the real vault.
#[derive(Default)]
pub struct MemoryVault {
    secrets: Mutex<HashMap<String, Vec<u8>>>,
    fail_delete: AtomicBool,
    fail_load: AtomicBool,
    deletions: Mutex<Vec<String>>,
}

impl MemoryVault {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set(&self, account_id: &str, password: &[u8]) {
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(account_id.to_owned(), password.to_vec());
    }

    pub fn contains(&self, account_id: &str) -> bool {
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains_key(account_id)
    }

    pub fn password(&self, account_id: &str) -> Option<Vec<u8>> {
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(account_id).cloned()
    }

    pub fn len(&self) -> usize {
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len()
    }

    /// Every account id whose credential deletion was attempted, in order.
    pub fn deletions(&self) -> Vec<String> {
        self.deletions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Make every later delete fail, to prove the `account_remove` failure window.
    pub fn fail_deletes(&self) {
        self.fail_delete.store(true, Ordering::SeqCst);
    }

    /// Make every later OAuth grant read fail, to prove removal does not depend on the keyring.
    pub fn fail_loads(&self) {
        self.fail_load.store(true, Ordering::SeqCst);
    }
}

impl CredentialVault for MemoryVault {
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        self.set(account_id, password);
        Ok(())
    }

    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        Ok(self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(account_id).cloned().map(Zeroizing::new))
    }

    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError> {
        self.deletions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(account_id.to_owned());
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(VaultError::new("the mock keyring refuses deletes"));
        }
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(account_id);
        Ok(())
    }

    fn store_smtp_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(format!("smtp:{account_id}"), password.to_vec());
        Ok(())
    }

    fn load_smtp_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        Ok(self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&format!("smtp:{account_id}")).cloned().map(Zeroizing::new))
    }

    fn delete_smtp_password(&self, account_id: &str) -> Result<(), VaultError> {
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&format!("smtp:{account_id}"));
        Ok(())
    }

    fn store_oauth_grant(&self, account_id: &str, grant: &OAuthGrant) -> Result<(), VaultError> {
        let bytes = grant.to_bytes();
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(format!("oauth:{account_id}"), bytes.to_vec());
        Ok(())
    }

    fn load_oauth_grant(&self, account_id: &str) -> Result<Option<OAuthGrant>, VaultError> {
        if self.fail_load.load(Ordering::SeqCst) {
            return Err(VaultError::new("the mock keyring refuses reads"));
        }
        let bytes = self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&format!("oauth:{account_id}")).cloned();
        match bytes {
            Some(bytes) => OAuthGrant::from_bytes(&bytes).map(Some),
            None => Ok(None),
        }
    }

    fn delete_oauth_grant(&self, account_id: &str) -> Result<(), VaultError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(VaultError::new("the mock keyring refuses deletes"));
        }
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&format!("oauth:{account_id}"));
        Ok(())
    }
}

/// A fake mailbox on one port that accepts a fixed number of connections sequentially.
pub struct RunningServer {
    port: u16,
    ca: CertificateDer<'static>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RunningServer {
    /// Serve `folders` unchanged for every connection.
    pub fn start(folders: Vec<TestFolder>, connections: usize) -> Self {
        Self::start_mutable(Arc::new(Mutex::new(folders)), connections, |_| MailboxScript::default())
    }

    /// Serve the current snapshot of `mailbox` for each connection, with a per-connection script.
    pub fn start_mutable<F>(mailbox: Arc<Mutex<Vec<TestFolder>>>, connections: usize, script: F) -> Self
    where
        F: Fn(usize) -> MailboxScript + Send + 'static,
    {
        Self::start_impl(mailbox, connections, script, None)
    }

    /// Serve an account that authenticates with `AUTHENTICATE XOAUTH2` and this access token.
    pub fn start_oauth(folders: Vec<TestFolder>, connections: usize, token: &str) -> Self {
        Self::start_impl(Arc::new(Mutex::new(folders)), connections, |_| MailboxScript::default(), Some(token.to_owned()))
    }

    fn start_impl<F>(mailbox: Arc<Mutex<Vec<TestFolder>>>, connections: usize, script: F, oauth: Option<String>) -> Self
    where
        F: Fn(usize) -> MailboxScript + Send + 'static,
    {
        let tls = TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("server runtime");
            runtime.block_on(async move {
                let counter = Arc::new(AtomicUsize::new(0));
                let server = FakeServer::start_tls_many(&tls, connections, move |conn| {
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    let folders = mailbox.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
                    let script = script(index);
                    let oauth = oauth.clone();
                    async move {
                        match oauth {
                            Some(token) => {
                                let _ = serve_mailbox_with_xoauth2(conn, folders, script, &token).await;
                            }
                            None => {
                                let _ = serve_mailbox_with(conn, folders, script).await;
                            }
                        }
                    }
                })
                .await;
                sender.send(server.port()).expect("send the server port");
                server.finish().await;
            });
        });
        let port = receiver.recv().expect("server port");
        Self { port, ca, handle: Some(handle) }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn ca(&self) -> CertificateDer<'static> {
        self.ca.clone()
    }

    pub fn config(&self) -> ImapConfig {
        ImapConfig::new("localhost", self.port, TlsMode::Implicit, USER, Secret::new(postino_testkit::PASSWORD))
            .with_timeout(Duration::from_secs(30))
            .with_extra_root(self.ca.clone())
    }

    /// Wait for every accepted connection and surface assertion failures.
    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            panic!("the fake mailbox thread panicked");
        }
    }
}

/// A fake SMTP submission server on one port, implicit TLS and the test CA.
pub struct RunningSmtp {
    port: u16,
    ca: CertificateDer<'static>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RunningSmtp {
    /// Accept `connections` sequential SMTP sessions. The handler receives the zero-based index so a
    /// backoff test can answer differently on the second attempt.
    pub fn start<F, Fut>(connections: usize, handler: F) -> Self
    where
        F: FnMut(usize, SmtpConn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let tls = TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("SMTP server runtime");
            runtime.block_on(async move {
                let server = FakeSmtp::start_tls_many(&tls, connections, handler).await;
                sender.send(server.port()).expect("send the SMTP port");
                server.finish().await;
            });
        });
        let port = receiver.recv().expect("SMTP server port");
        Self { port, ca, handle: Some(handle) }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn ca(&self) -> CertificateDer<'static> {
        self.ca.clone()
    }

    pub fn config(&self) -> SmtpConfig {
        SmtpConfig::new("localhost", self.port, TlsMode::Implicit, USER, Secret::new(postino_testkit::SMTP_PASSWORD))
            .with_timeout(Duration::from_secs(10))
            .with_extra_root(self.ca.clone())
    }

    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            panic!("the fake SMTP thread panicked");
        }
    }
}

/// What one APPEND delivered, for the Sent-copy assertions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendRecord {
    pub flags: String,
    pub bytes: Vec<u8>,
}

/// A fake IMAP server that answers LOGIN, LIST and APPEND. It never serves envelopes: the send path
/// only needs the Sent folder and the append.
pub struct RunningAppendServer {
    port: u16,
    ca: CertificateDer<'static>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RunningAppendServer {
    pub fn start(connections: usize, records: Arc<Mutex<Vec<AppendRecord>>>, with_sent: bool) -> Self {
        Self::start_script(connections, records, move |_| with_sent)
    }

    /// Accept the login, list a `\Sent` folder, and answer the `APPEND` with `NO <echo>`.
    ///
    /// Models the review's hostile server: it has received the IMAP password and can repeat it (or
    /// the message) in the refusal. The engine must store a fixed sentence, not this text.
    pub fn start_refusing(connections: usize, echo: String) -> Self {
        let tls = TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("APPEND server runtime");
            runtime.block_on(async move {
                let counter = Arc::new(AtomicUsize::new(0));
                let server = FakeServer::start_tls_many(&tls, connections, move |mut conn| {
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    let echo = echo.clone();
                    async move {
                        assert_eq!(index, 0);
                        conn.prepare_sasl().await;
                        conn.expect("A0003 LIST \"\" \"*\"").await;
                        conn.send_line("* LIST (\\HasNoChildren) \"/\" \"INBOX\"").await;
                        conn.send_line("* LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"").await;
                        conn.send_line("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"").await;
                        conn.send_line("A0003 OK listed").await;
                        let line = conn.expect_prefix("A0004 APPEND ").await;
                        let size = line
                            .rsplit_once('{')
                            .and_then(|(_, rest)| rest.trim_end_matches('}').parse::<usize>().ok())
                            .expect("an APPEND literal size");
                        conn.send_line("+ ready").await;
                        let _ = conn.read_exact_bytes(size).await;
                        let _ = conn.read_line_opt().await;
                        conn.send_line(&format!("A0004 NO {echo}")).await;
                        let _ = conn.read_line_opt().await;
                        let _ = conn.send_line("A0005 OK").await;
                    }
                })
                .await;
                sender.send(server.port()).expect("send the APPEND port");
                server.finish().await;
            });
        });
        let port = receiver.recv().expect("APPEND server port");
        Self { port, ca, handle: Some(handle) }
    }

    pub fn start_script<F>(connections: usize, records: Arc<Mutex<Vec<AppendRecord>>>, mut with_sent: F) -> Self
    where
        F: FnMut(usize) -> bool + Send + 'static,
    {
        let tls = TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let with_sent = (0..connections).map(&mut with_sent).collect::<Vec<_>>();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("APPEND server runtime");
            runtime.block_on(async move {
                let records = records;
                let counter = Arc::new(AtomicUsize::new(0));
                let server = FakeServer::start_tls_many(&tls, connections, move |mut conn| {
                    let records = records.clone();
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    let with_sent = with_sent.get(index).copied().unwrap_or(false);
                    async move {
                        conn.prepare_sasl().await;
                        conn.expect("A0003 LIST \"\" \"*\"").await;
                        conn.send_line("* LIST (\\HasNoChildren) \"/\" \"INBOX\"").await;
                        conn.send_line("* LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"").await;
                        if with_sent {
                            conn.send_line("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"").await;
                        }
                        conn.send_line("A0003 OK listed").await;
                        if !with_sent {
                            conn.expect("A0004 LOGOUT").await;
                            conn.send_line("* BYE").await;
                            conn.send_line("A0004 OK").await;
                            return;
                        }
                        let line = conn.expect_prefix("A0004 APPEND ").await;
                        assert!(line.contains("\\Seen"), "the Sent append did not request \\Seen: {line}");
                        let size = line
                            .rsplit_once('{')
                            .and_then(|(_, rest)| rest.trim_end_matches('}').parse::<usize>().ok())
                            .expect("an APPEND literal size");
                        let flags =
                            line.split_once('(').and_then(|(_, rest)| rest.split_once(')')).map(|(flags, _)| flags.to_owned()).unwrap_or_default();
                        conn.send_line("+ ready").await;
                        let bytes = conn.read_exact_bytes(size).await;
                        let trailer = conn.read_line_opt().await;
                        assert_eq!(trailer.as_deref(), Some(""), "the APPEND literal was not followed by CRLF");
                        records.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(AppendRecord { flags, bytes });
                        conn.send_line("A0004 OK [APPENDUID 1 1] appended").await;
                        conn.expect("A0005 LOGOUT").await;
                        conn.send_line("* BYE").await;
                        conn.send_line("A0005 OK").await;
                    }
                })
                .await;
                sender.send(server.port()).expect("send the APPEND port");
                server.finish().await;
            });
        });
        let port = receiver.recv().expect("APPEND server port");
        Self { port, ca, handle: Some(handle) }
    }

    /// The first connection receives the APPEND literal, signals `reached` and blocks until `release`
    /// fires before answering `OK`. A test uses it to observe the row's append claim while the append
    /// is still in flight. `connections` is the number the fake server accepts (use 1 here).
    pub fn start_blocking(
        connections: usize,
        records: Arc<Mutex<Vec<AppendRecord>>>,
        reached: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        let tls = TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let release = Arc::new(Mutex::new(Some(release)));
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("APPEND server runtime");
            runtime.block_on(async move {
                let server = FakeServer::start_tls_many(&tls, connections, move |mut conn| {
                    let records = records.clone();
                    let reached = reached.clone();
                    let release = release.lock().ok().and_then(|mut guard| guard.take());
                    async move {
                        conn.prepare_sasl().await;
                        conn.expect("A0003 LIST \"\" \"*\"").await;
                        conn.send_line("* LIST (\\HasNoChildren) \"/\" \"INBOX\"").await;
                        conn.send_line("* LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"").await;
                        conn.send_line("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"").await;
                        conn.send_line("A0003 OK listed").await;
                        let line = conn.expect_prefix("A0004 APPEND ").await;
                        let size = line
                            .rsplit_once('{')
                            .and_then(|(_, rest)| rest.trim_end_matches('}').parse::<usize>().ok())
                            .expect("an APPEND literal size");
                        conn.send_line("+ ready").await;
                        let bytes = conn.read_exact_bytes(size).await;
                        let trailer = conn.read_line_opt().await;
                        assert_eq!(trailer.as_deref(), Some(""), "the APPEND literal was not followed by CRLF");
                        records.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(AppendRecord { flags: "\\Seen".to_owned(), bytes });
                        let _ = reached.send(());
                        if let Some(release) = release {
                            let _ = release.recv();
                        }
                        conn.send_line("A0004 OK [APPENDUID 1 1] appended").await;
                        conn.expect("A0005 LOGOUT").await;
                        conn.send_line("* BYE").await;
                        conn.send_line("A0005 OK").await;
                    }
                })
                .await;
                sender.send(server.port()).expect("send the APPEND port");
                server.finish().await;
            });
        });
        let port = receiver.recv().expect("APPEND server port");
        Self { port, ca, handle: Some(handle) }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn ca(&self) -> CertificateDer<'static> {
        self.ca.clone()
    }

    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            panic!("the fake APPEND thread panicked");
        }
    }
}

/// A stored account with both an IMAP and an SMTP endpoint.
pub fn seed_account_with_smtp(store: &Store, vault: &MemoryVault, id: &str, imap_port: u16, smtp_port: u16, folders: &[(&str, &str, FolderKind)]) {
    store
        .upsert_account(&Account {
            id: id.to_owned(),
            name: "Postino Test".to_owned(),
            email: USER.to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    store
        .upsert_account_server(&AccountServer {
            account_id: id.to_owned(),
            host: "localhost".to_owned(),
            port: imap_port,
            tls: AccountTls::Implicit,
            username: USER.to_owned(),
            created_at: 1_700_000_000_000,
            smtp: Some(SmtpServer { host: "localhost".to_owned(), port: smtp_port, tls: AccountTls::Implicit, username: USER.to_owned() }),
        })
        .expect("server");
    for (suffix, name, kind) in folders {
        store
            .upsert_folder(&Folder {
                id: format!("fld_{id}_{suffix}"),
                account_id: id.to_owned(),
                name: (*name).to_owned(),
                kind: *kind,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("folder");
    }
    vault.set(id, postino_testkit::PASSWORD.as_bytes());
    vault.store_smtp_password(id, postino_testkit::SMTP_PASSWORD.as_bytes()).expect("SMTP password");
}

/// The setup the CLI would build for provider `custom` against a local fake server.
pub fn account_setup(server: &RunningServer) -> AccountSetup {
    AccountSetup {
        provider: Provider::Custom,
        email: USER.to_owned(),
        name: Some("Postino Test".to_owned()),
        password: Zeroizing::new(postino_testkit::PASSWORD.to_owned()),
        host: Some("localhost".to_owned()),
        port: Some(server.port()),
        tls: Some(MailTls::Implicit),
        smtp: None,
        oauth: None,
        oauth_access: None,
        extra_roots: vec![server.ca()],
    }
}

/// A stored account and endpoint, as `account_add` would have written them.
pub fn seed_account(store: &Store, vault: &MemoryVault, id: &str, port: u16, folders: &[(&str, &str, FolderKind)]) {
    store
        .upsert_account(&Account {
            id: id.to_owned(),
            name: "Postino Test".to_owned(),
            email: USER.to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    store
        .upsert_account_server(&AccountServer {
            account_id: id.to_owned(),
            host: "localhost".to_owned(),
            port,
            tls: AccountTls::Implicit,
            username: USER.to_owned(),
            created_at: 1_700_000_000_000,
            smtp: None,
        })
        .expect("server");
    for (suffix, name, kind) in folders {
        store
            .upsert_folder(&Folder {
                id: format!("fld_{id}_{suffix}"),
                account_id: id.to_owned(),
                name: (*name).to_owned(),
                kind: *kind,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("folder");
    }
    vault.set(id, postino_testkit::PASSWORD.as_bytes());
}

/// A server that rejects every login with a readable `NO`, without using the scripted mailbox.
pub async fn reject_login(mut conn: Conn) {
    conn.send_line("* OK fake IMAP ready").await;
    conn.expect("A0001 CAPABILITY").await;
    conn.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN").await;
    conn.send_line("A0001 OK done").await;
    conn.expect("A0002 AUTHENTICATE PLAIN").await;
    conn.send_line("+ ").await;
    let payload = conn.read_line().await;
    // The wrong password is still a secret: the test asserts it is never echoed, not what it is.
    assert!(!payload.is_empty());
    conn.send_line("A0002 NO [AUTHENTICATIONFAILED] invalid credentials").await;
}

/// A server that accepts the login and then answers `LIST` with `NO <secret>`.
///
/// This models the review's hostile server: it has just received the password and repeats it in a
/// tagged response. The engine must not forward the response text.
pub async fn refuse_list_with_secret(mut conn: Conn, secret: String) {
    conn.prepare_sasl().await;
    conn.expect_prefix("A0003 LIST").await;
    conn.send_line(&format!("A0003 NO {secret}")).await;
}

/// A one-connection server with a custom handler.
pub fn start_handler<F, Fut>(handler: F) -> RunningServer
where
    F: FnOnce(Conn) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let tls = TlsFixture::generate();
    let ca = tls.ca();
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("server runtime");
        runtime.block_on(async move {
            let server = FakeServer::start_tls(&tls, handler).await;
            sender.send(server.port()).expect("send the server port");
            server.finish().await;
        });
    });
    let port = receiver.recv().expect("server port");
    RunningServer { port, ca, handle: Some(handle) }
}
