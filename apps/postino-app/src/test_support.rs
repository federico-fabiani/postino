//! Test-only scaffolding for the desktop app: an in-memory vault, a fake IMAP server with a
//! throwaway CA, and a store seeded with an account. Nothing here touches the real keyring or a real
//! server.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postino_core::{CredentialVault, VaultError};
use postino_net::CertificateDer;
use postino_store::{Account, AccountServer, AccountTls, Store};
use postino_testkit::{FakeServer, TestFolder, USER, serve_mailbox};
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
        if let Ok(mut secrets) = self.secrets.lock() {
            secrets.insert(account_id.to_owned(), password.to_vec());
        }
    }

    pub fn contains(&self, account_id: &str) -> bool {
        self.secrets.lock().is_ok_and(|secrets| secrets.contains_key(account_id))
    }

    pub fn len(&self) -> usize {
        self.secrets.lock().map_or(0, |secrets| secrets.len())
    }
}

impl CredentialVault for MemoryVault {
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        self.set(account_id, password);
        Ok(())
    }

    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        Ok(self.secrets.lock().ok().and_then(|secrets| secrets.get(account_id).cloned()).map(Zeroizing::new))
    }

    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError> {
        if let Ok(mut secrets) = self.secrets.lock() {
            secrets.remove(account_id);
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Mode {
    Mailbox(Vec<TestFolder>),
    AuthFailure,
    Silent(Duration),
}

pub struct RunningServer {
    port: u16,
    ca: CertificateDer<'static>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// A TLS listener with a certificate the client is not told to trust. The handshake must fail on the
/// client side; unlike the shared testkit server, this one does not assert a successful handshake.
pub struct UntrustedTlsServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl UntrustedTlsServer {
    pub fn start() -> Self {
        let fixture = postino_testkit::TlsFixture::generate();
        let acceptor = fixture.acceptor();
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("TLS server runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("TLS listener");
                let port = listener.local_addr().expect("TLS address").port();
                sender.send(port).expect("send the TLS port");
                let (stream, _) = listener.accept().await.expect("TLS accept");
                let _ = acceptor.accept(stream).await;
            });
        });
        let port = receiver.recv().expect("TLS port");
        Self { port, handle: Some(handle) }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A listener that accepts one TCP connection and answers with a non-TLS byte stream. The client
/// must report a TLS failure without a server-side assertion or a certificate fixture.
pub struct PlainTcpServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PlainTcpServer {
    pub fn start() -> Self {
        use std::io::Write;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("plain listener");
        let port = listener.local_addr().expect("plain address").port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
                let _ = stream.flush();
            }
        });
        Self { port, handle: Some(handle) }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl RunningServer {
    /// Serve `folders` for each of `connections` sequential sessions on one port.
    pub fn mailbox(folders: Vec<TestFolder>, connections: usize) -> Self {
        Self::start(Mode::Mailbox(folders), connections)
    }

    /// Reject every login with a tagged NO. The client must classify this without showing the
    /// server's own text.
    pub fn auth_failure(connections: usize) -> Self {
        Self::start(Mode::AuthFailure, connections)
    }

    /// Complete TLS, then never send the greeting. Used to prove a timeout is a specific error.
    pub fn silent(connections: usize) -> Self {
        Self::start(Mode::Silent(Duration::from_secs(2)), connections)
    }

    fn start(mode: Mode, connections: usize) -> Self {
        let tls = postino_testkit::TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("server runtime");
            runtime.block_on(async move {
                let server = FakeServer::start_tls_many(&tls, connections, move |mut conn| {
                    let mode = mode.clone();
                    async move {
                        match mode {
                            Mode::Mailbox(folders) => {
                                let _ = serve_mailbox(conn, folders).await;
                            }
                            Mode::AuthFailure => {
                                conn.send_line("* OK fake IMAP ready").await;
                                let capability = conn.read_line().await;
                                let tag = capability.split(' ').next().unwrap_or("A0001").to_owned();
                                conn.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN").await;
                                conn.send_line(&format!("{tag} OK done")).await;
                                let authenticate = conn.read_line().await;
                                let auth_tag = authenticate.split(' ').next().unwrap_or("A0002").to_owned();
                                conn.send_line("+ ").await;
                                let _ = conn.read_line().await;
                                conn.send_line(&format!("{auth_tag} NO [AUTHENTICATIONFAILED] invalid credentials")).await;
                                while conn.read_line_opt().await.is_some() {}
                            }
                            Mode::Silent(delay) => {
                                conn.delay(delay).await;
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

    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            panic!("the fake mailbox thread panicked");
        }
    }
}

/// Seed the account and endpoint sync/onboarding need. No folder rows: a real sync discovers them.
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

/// A mailbox with three folders and three messages.
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
