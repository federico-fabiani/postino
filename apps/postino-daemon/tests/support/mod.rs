//! Shared fixtures for the daemon integration tests. Every test uses a temporary directory and an
//! in-memory credential vault; no test touches the OS keyring or `%LOCALAPPDATA%\Postino`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postino_core::{CredentialVault, OAuthGrant, VaultError};
use postino_daemon::{Daemon, DaemonConfig};
use postino_ipc::DaemonPaths;
use postino_ipc::client::Client;
use postino_store::{Account, AccountServer, AccountTls, Folder, FolderKind, SmtpServer, Store, StoreKey};
use zeroize::Zeroizing;

pub const KEY: [u8; 32] = [0x42; 32];
pub const ACCOUNT: &str = "acc_test";

#[derive(Clone, Default)]
pub struct MockVault {
    secrets: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl MockVault {
    pub fn with(imap: &str, smtp: Option<&str>) -> Self {
        let vault = Self::default();
        vault.store_imap_password(ACCOUNT, imap.as_bytes()).expect("imap secret");
        if let Some(smtp) = smtp {
            vault.store_smtp_password(ACCOUNT, smtp.as_bytes()).expect("smtp secret");
        }
        vault
    }

    pub fn oauth_grant(&self, account_id: &str) -> Option<OAuthGrant> {
        let bytes = self.secrets.lock().ok()?.get(&format!("{account_id}:oauth")).cloned()?;
        OAuthGrant::from_bytes(&bytes).ok()
    }

    pub fn has_imap_password(&self, account_id: &str) -> bool {
        self.secrets.lock().map(|secrets| secrets.contains_key(&format!("{account_id}:imap"))).unwrap_or(false)
    }
}

impl CredentialVault for MockVault {
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.insert(format!("{account_id}:imap"), password.to_vec());
        Ok(())
    }

    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        Ok(self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.get(&format!("{account_id}:imap")).cloned().map(Zeroizing::new))
    }

    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError> {
        self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.remove(&format!("{account_id}:imap"));
        Ok(())
    }

    fn store_smtp_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.insert(format!("{account_id}:smtp"), password.to_vec());
        Ok(())
    }

    fn load_smtp_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        Ok(self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.get(&format!("{account_id}:smtp")).cloned().map(Zeroizing::new))
    }

    fn delete_smtp_password(&self, account_id: &str) -> Result<(), VaultError> {
        self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.remove(&format!("{account_id}:smtp"));
        Ok(())
    }

    fn store_oauth_grant(&self, account_id: &str, grant: &OAuthGrant) -> Result<(), VaultError> {
        self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.insert(format!("{account_id}:oauth"), grant.to_bytes().to_vec());
        Ok(())
    }

    fn load_oauth_grant(&self, account_id: &str) -> Result<Option<OAuthGrant>, VaultError> {
        let bytes = self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.get(&format!("{account_id}:oauth")).cloned();
        match bytes {
            Some(bytes) => OAuthGrant::from_bytes(&bytes).map(Some),
            None => Ok(None),
        }
    }

    fn delete_oauth_grant(&self, account_id: &str) -> Result<(), VaultError> {
        self.secrets.lock().map_err(|_| VaultError::new("test vault poisoned"))?.remove(&format!("{account_id}:oauth"));
        Ok(())
    }
}

pub struct Fixture {
    pub _directory: tempfile::TempDir,
    pub store_path: std::path::PathBuf,
    pub key: Arc<StoreKey>,
    pub vault: Arc<MockVault>,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let store_path = directory.path().join("mail.db");
        Self { _directory: directory, store_path, key: Arc::new(StoreKey::from_bytes(KEY)), vault: Arc::new(MockVault::default()) }
    }

    pub fn with_vault(mut self, vault: MockVault) -> Self {
        self.vault = Arc::new(vault);
        self
    }

    pub fn seed(&self, imap_port: u16, smtp: Option<(u16, bool)>) {
        let store = Store::open(&self.store_path, &self.key).expect("seed store");
        let account = Account {
            id: ACCOUNT.to_owned(),
            name: "Test".to_owned(),
            email: "user@example.com".to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
            created_at: 1,
        };
        let server = AccountServer {
            account_id: ACCOUNT.to_owned(),
            host: "localhost".to_owned(),
            port: imap_port,
            tls: AccountTls::Implicit,
            username: "user@example.com".to_owned(),
            created_at: 1,
            smtp: smtp.map(|(port, _starttls)| SmtpServer {
                host: "localhost".to_owned(),
                port,
                tls: AccountTls::Implicit,
                username: "user@example.com".to_owned(),
            }),
        };
        let folders = vec![
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
            Folder {
                id: "sent".to_owned(),
                account_id: ACCOUNT.to_owned(),
                name: "Sent".to_owned(),
                kind: FolderKind::Sent,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: None,
                last_sync_at: None,
            },
        ];
        store.save_account(&account, &server, &folders).expect("account");
    }

    pub fn config(&self) -> DaemonConfig {
        let mut config = DaemonConfig::new(self.store_path.clone(), Arc::clone(&self.key), Arc::clone(&self.vault) as Arc<dyn CredentialVault>);
        config.sync_on_start = false;
        config.sync_jitter = false;
        config.sync_interval = Duration::from_secs(3600);
        config.gc_interval = Duration::from_secs(3600);
        config.index_tick = Duration::from_millis(50);
        config
    }

    pub fn paths(&self) -> DaemonPaths {
        DaemonPaths::for_store(&self.store_path).expect("paths")
    }

    pub async fn client(&self, name: &str) -> Client {
        Client::connect(&self.paths(), name, Duration::from_secs(2), Duration::from_secs(5)).await.expect("client")
    }
}

pub async fn wait_for<F>(timeout: Duration, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

pub async fn start(fixture: &Fixture) -> Daemon {
    Daemon::spawn(fixture.config()).expect("daemon")
}

/// Seed `count` received rows in the inbox so the first daemon index pass has several batches to
/// build. The FTS triggers may already have indexed them; the persisted index state still starts
/// `pending`, so `ensure_search_index_batch` walks them 500 at a time.
pub fn seed_messages(fixture: &Fixture, count: u32) {
    let store = Store::open(&fixture.store_path, &fixture.key).expect("seed store");
    for index in 0..count {
        let id = format!("msg_{index:05}");
        let message = postino_store::Message {
            summary: postino_store::MessageSummary {
                id: id.clone(),
                account_id: ACCOUNT.to_owned(),
                folder_id: "inbox".to_owned(),
                thread_id: format!("thr_{index:05}"),
                from: postino_store::Address::new("Synthetic Sender", "synthetic@example.com"),
                to: vec![postino_store::Address::new("Postino Test", "postino@example.com")],
                subject: format!("Synthetic message {index}"),
                snippet: format!("Body {index}"),
                date: 1_700_000_000_000 + i64::from(index),
                unread: true,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            uid: Some(index.saturating_add(1)),
            message_id: Some(format!("<seed-{index}@example.com>")),
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: format!("Synthetic body {index}"),
            body_html: None,
            remote_images: 0,
            size: Some(32),
        };
        store.upsert_synced_message(&message, &[]).expect("seed message");
    }
}

/// Seed one outbox row in a P7 crash state. The caller uses a fresh store per state so
/// `lease_next_outbox` cannot select a different row.
pub fn seed_outbox_state(fixture: &Fixture, id: &str, state: &str) {
    let store = Store::open(&fixture.store_path, &fixture.key).expect("seed store");
    let now = chrono::Utc::now().timestamp_millis();
    let raw = store.blobs().put(id.as_bytes()).expect("outbox blob");
    let envelope = format!(r#"{{"from":"user@example.com","recipients":["dest@example.com"],"subject":"{id}","message_id":"<{id}@example.com>"}}"#);
    let send_at = if state == "queued_future" { now + 3_600_000 } else { 1 };
    store
        .queue_outbox(&postino_store::OutboxEntry {
            id: id.to_owned(),
            account_id: ACCOUNT.to_owned(),
            draft_id: None,
            idempotency_key: format!("key-{id}"),
            created_at: now,
            send_at,
            state: postino_store::OutboxState::Queued,
            attempts: 0,
            append_attempts: 0,
            last_error: None,
            raw_blob_hash: raw,
            envelope_json: envelope,
            lease_at: None,
        })
        .expect("queue");
    let lease_now = now;
    let lease_old = now - postino_core::outbox::LEASE_MS - 1_000;
    match state {
        "sending_fresh" => {
            store.lease_next_outbox(lease_now).expect("lease").expect("row");
        }
        "sending_stale" => {
            store.lease_next_outbox(lease_old).expect("lease").expect("row");
        }
        "append_pending" => {
            let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
            let lease = leased.lease_at.unwrap_or(lease_now);
            store.mark_outbox_append_pending(&leased.id, lease, postino_core::outbox::APPEND_LEASE_MS).expect("append");
        }
        "uncertain" => {
            let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
            let lease = leased.lease_at.unwrap_or(lease_now);
            store.mark_outbox_uncertain(&leased.id, "crash", lease).expect("uncertain");
        }
        "failed" => {
            let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
            let lease = leased.lease_at.unwrap_or(lease_now);
            store.mark_outbox_failed(&leased.id, "failed", lease).expect("failed");
        }
        "sent" => {
            let leased = store.lease_next_outbox(lease_now).expect("lease").expect("row");
            let lease = leased.lease_at.unwrap_or(lease_now);
            store.mark_outbox_uncertain(&leased.id, "delivered", lease).expect("uncertain");
            store.confirm_outbox_sent(&leased.id).expect("sent");
        }
        "cancelled" => {
            store.cancel_outbox(id).expect("cancel");
        }
        _ => {}
    }
}

/// Put one orphan and one referenced blob in the store. The referenced one is attached to a message
/// row; the orphan is not. A GC sweep must remove exactly the orphan.
pub fn seed_blobs(fixture: &Fixture) -> (String, String) {
    let store = Store::open(&fixture.store_path, &fixture.key).expect("seed store");
    let referenced = store.blobs().put(b"referenced bytes").expect("referenced blob");
    let orphan = store.blobs().put(b"orphan bytes").expect("orphan blob");
    let message = postino_store::Message {
        summary: postino_store::MessageSummary {
            id: "msg_attachment".to_owned(),
            account_id: ACCOUNT.to_owned(),
            folder_id: "inbox".to_owned(),
            thread_id: "thr_attachment".to_owned(),
            from: postino_store::Address::new("Synthetic Sender", "synthetic@example.com"),
            to: vec![postino_store::Address::new("Postino Test", "postino@example.com")],
            subject: "Attachment".to_owned(),
            snippet: String::new(),
            date: 1_700_000_000_000,
            unread: false,
            starred: false,
            has_attachments: true,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some("<attachment@example.com>".to_owned()),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: String::new(),
        body_html: None,
        remote_images: 0,
        size: Some(0),
    };
    let attachment = postino_store::Attachment {
        id: "att_1".to_owned(),
        filename: "a.txt".to_owned(),
        mime: "text/plain".to_owned(),
        size: 16,
        blob_hash: Some(referenced.clone()),
    };
    store.upsert_synced_message(&message, std::slice::from_ref(&attachment)).expect("attachment row");
    (referenced, orphan)
}
