//! Shared sync-test scaffolding: a store on a temp directory, a fake mailbox on its own thread, and
//! the consistency checks every failure test repeats.
//!
//! The fake mailbox must live on another OS thread: `sync_account` builds its own current-thread
//! runtime and blocks, so it cannot be called from inside another Tokio runtime.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use postino_net::{CertificateDer, ImapConfig, Secret, TlsMode};
use postino_store as store;
use postino_store::{Account, Store, StoreKey};
use postino_sync::{SyncError, SyncLimits, SyncOutcome, sync_account};
use postino_testkit::{Conn, FakeServer, MailboxScript, PASSWORD, TestFolder, USER};

pub const ACCOUNT_ID: &str = "acc_p5b";

/// A temporary store whose key survives a close and reopen.
pub struct Harness {
    dir: tempfile::TempDir,
    key: [u8; 32],
}

impl Harness {
    pub fn new() -> Self {
        // A fixed test key is fine: every harness owns a fresh temp directory, and these tests never
        // touch the real keyring or a real store.
        Self { dir: tempfile::tempdir().expect("temp dir"), key: [0x5b; 32] }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("mail.db")
    }

    pub fn open(&self) -> Store {
        Store::open(&self.path(), &StoreKey::from_bytes(self.key)).expect("open the store")
    }

    /// Reopen the same database, as the process would after a restart.
    pub fn reopen(&self) -> Store {
        self.open()
    }
}

pub fn account() -> Account {
    Account {
        id: ACCOUNT_ID.to_owned(),
        name: "Studio Verdi".to_owned(),
        email: "studio@example.com".to_owned(),
        provider: "test".to_owned(),
        signature: String::new(),
        created_at: 1_700_000_000_000,
    }
}

pub fn seeded(harness: &Harness) -> Store {
    let store = harness.open();
    store.upsert_account(&account()).expect("account");
    store
}

/// A fake mailbox on its own OS thread, with the CA its client must trust.
pub struct RunningServer {
    port: u16,
    ca: CertificateDer<'static>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RunningServer {
    pub fn start(folders: Vec<TestFolder>) -> Self {
        Self::start_with(folders, MailboxScript::default())
    }

    pub fn start_with(folders: Vec<TestFolder>, script: MailboxScript) -> Self {
        Self::start_handler(move |conn| async move {
            let _ = postino_testkit::serve_mailbox_with(conn, folders, script).await;
        })
    }

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

    pub fn config(&self) -> ImapConfig {
        ImapConfig::new("localhost", self.port, TlsMode::Implicit, USER, Secret::new(PASSWORD))
            .with_timeout(Duration::from_secs(60))
            .with_extra_root(self.ca.clone())
    }

    /// Wait for the mailbox handler and surface its assertion failures.
    pub fn finish(mut self) {
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            panic!("the fake mailbox thread panicked");
        }
    }
}

pub fn run(store: &Store, server: &RunningServer, limits: &SyncLimits, cancel: &AtomicBool) -> Result<SyncOutcome, SyncError> {
    sync_account(store, &account(), &server.config(), limits, cancel)
}

pub fn sync(store: &Store, server: &RunningServer) -> Result<SyncOutcome, SyncError> {
    run(store, server, &SyncLimits::default(), &AtomicBool::new(false))
}

/// Assert the cache is internally consistent: every message has a body, every blob-backed attachment
/// has readable bytes, and no id is duplicated.
pub fn assert_consistent(store: &Store) {
    let messages = store.list_all_messages().expect("list messages");
    let mut ids = std::collections::HashSet::new();
    for message in &messages {
        assert!(ids.insert(message.summary.id.clone()), "duplicate message id {}", message.summary.id);
        assert!(!message.body_text.is_empty() || message.body_html.is_some(), "message {} was committed without a body", message.summary.id);
    }
    let attachments = store.list_all_attachments().expect("list attachments");
    for (message_id, rows) in attachments {
        for attachment in rows {
            if let Some(hash) = attachment.blob_hash {
                let bytes = store.blobs().get(&hash, attachment.size.saturating_add(1024)).expect("read the stored attachment");
                assert!(!bytes.is_empty() || attachment.size == 0, "attachment {message_id}/{} has no bytes", attachment.id);
            }
        }
    }
}

/// The folder row with this display name.
pub fn folder(store: &Store, name: &str) -> store::Folder {
    store
        .list_folders(Some(ACCOUNT_ID))
        .expect("list folders")
        .into_iter()
        .find(|folder| folder.name == name)
        .unwrap_or_else(|| panic!("no folder named {name:?}"))
}

/// The message whose header Message-ID (without angle brackets) is `message_id`.
pub fn message_by_header(store: &Store, message_id: &str) -> store::Message {
    store
        .list_all_messages()
        .expect("list all messages")
        .into_iter()
        .find(|message| message.message_id.as_deref() == Some(message_id))
        .unwrap_or_else(|| panic!("no message with Message-ID {message_id:?}"))
}

/// The messages of one folder, newest first.
pub fn messages_in(store: &Store, folder_id: &str) -> Vec<store::MessageSummary> {
    let filter = store::MessageFilter { folder_id: Some(folder_id.to_owned()), limit: 500, ..store::MessageFilter::default() };
    store.list_messages(&filter).expect("list messages").messages
}

/// Every message in the store, unfiltered and unpaged (for scale assertions).
pub fn all_message_ids(store: &Store) -> Vec<String> {
    store.list_all_messages().expect("list all messages").into_iter().map(|message| message.summary.id).collect()
}
