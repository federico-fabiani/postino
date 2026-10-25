//! Test-only scaffolding for the CLI and MCP suites: an in-memory credential vault and a fake IMAP
//! server with a throwaway CA. Nothing here can touch the real keyring or a real mail server.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use postino_core::{CredentialVault, VaultError};
use postino_net::CertificateDer;
use postino_store::{Account, AccountServer, AccountTls, Store};
use postino_testkit::{Conn, FakeServer, TestFolder, USER, serve_mailbox};
use zeroize::Zeroizing;

#[derive(Default)]
pub struct MemoryVault {
    secrets: Mutex<HashMap<String, Vec<u8>>>,
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

    pub fn len(&self) -> usize {
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len()
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
        self.secrets.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(account_id);
        Ok(())
    }
}

pub struct RunningServer {
    port: u16,
    ca: CertificateDer<'static>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RunningServer {
    /// Serve `folders` for each of `connections` sequential sessions on one port.
    pub fn start(folders: Vec<TestFolder>, connections: usize) -> Self {
        let tls = postino_testkit::TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("server runtime");
            runtime.block_on(async move {
                let server = FakeServer::start_tls_many(&tls, connections, move |conn| {
                    let folders = folders.clone();
                    async move {
                        let _ = serve_mailbox(conn, folders).await;
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

    /// One connection with a custom handler, for a hostile or broken server.
    pub fn start_handler<F, Fut>(handler: F) -> Self
    where
        F: FnOnce(Conn) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let tls = postino_testkit::TlsFixture::generate();
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
        Self { port, ca, handle: Some(handle) }
    }

    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            panic!("the fake mailbox thread panicked");
        }
    }
}

/// Accept the login and answer `LIST` with `NO <secret>`: the hostile server from the security
/// review that repeats the password it just received.
pub async fn refuse_list_with_secret(mut conn: Conn, secret: String) {
    conn.prepare_sasl().await;
    conn.expect_prefix("A0003 LIST").await;
    conn.send_line(&format!("A0003 NO {secret}")).await;
}

/// Seed the account and endpoint `sync_now`/MCP need. The sync run discovers the folders itself.
pub fn seed_account(store: &Store, vault: &MemoryVault, id: &str, port: u16) {
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
    vault.set(id, postino_testkit::PASSWORD.as_bytes());
}

/// A mailbox with three folders and two messages, enough for a real sync run.
pub fn three_folders() -> Vec<TestFolder> {
    vec![
        TestFolder::new("INBOX", 10).with_special_use("\\Inbox").with_messages(vec![
            postino_testkit::TestMessage::synthetic(postino_testkit::synthetic_message(1)),
            postino_testkit::TestMessage::synthetic(postino_testkit::synthetic_message(2)),
        ]),
        TestFolder::new("Sent", 20)
            .with_special_use("\\Sent")
            .with_messages(vec![postino_testkit::TestMessage::synthetic(postino_testkit::synthetic_message(3))]),
        TestFolder::new("Trash", 30).with_special_use("\\Trash"),
    ]
}
