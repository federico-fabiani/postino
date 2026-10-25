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

use postino_core::{AccountSetup, CredentialVault, MailTls, Provider, VaultError};
use postino_net::{CertificateDer, ImapConfig, Secret, TlsMode};
use postino_store::{Account, AccountServer, AccountTls, Folder, FolderKind, Store, StoreKey};
use postino_testkit::{Conn, FakeServer, MailboxScript, TestFolder, TlsFixture, USER, serve_mailbox_with};
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
}

/// The credential seam used by every test. Nothing here can reach the real vault.
#[derive(Default)]
pub struct MemoryVault {
    secrets: Mutex<HashMap<String, Vec<u8>>>,
    fail_delete: AtomicBool,
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

    /// Make every later delete fail, to prove the `account_remove` failure window.
    pub fn fail_deletes(&self) {
        self.fail_delete.store(true, Ordering::SeqCst);
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
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(VaultError::new("the mock keyring refuses deletes"));
        }
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(account_id);
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
                    async move {
                        let _ = serve_mailbox_with(conn, folders, script).await;
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
