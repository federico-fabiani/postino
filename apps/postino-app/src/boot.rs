//! Where the desktop app gets its mailbox, and what happens when it cannot.
//!
//! A normal launch opens the same encrypted store as the CLI (`<data dir>/mail.db` unless
//! `--store` names another file). The store key and every IMAP credential come from the operating
//! system keyring through [`PlatformVault`]; nothing here logs, formats or stores a secret. A
//! failure is a [`BootFailure`] that names the path and the stage, never a panic and never an empty
//! mailbox pretending to be the user's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use postino_core::{CredentialVault, Engine, VaultError};
use postino_platform::{Keyring, STORE_SCOPE};
use postino_store::{Store, StoreError, StoreKey, WriterLock};
use zeroize::Zeroizing;

/// What the command line asked for. `Store(None)` is the standard local mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    Demo,
    Store(Option<PathBuf>),
}

/// Where the 256-bit store key comes from. Production always uses the OS keyring; tests inject a
/// fixed key and an in-memory vault so no suite can touch the real credential manager.
pub enum KeySource {
    Keyring,
    #[cfg(test)]
    Fixed {
        key: Arc<StoreKey>,
        vault: Arc<dyn CredentialVault>,
    },
}

/// The booted app's mailbox plus the handle the sync worker needs to open its own connection.
pub struct Booted {
    pub engine: Engine,
    pub store: Option<StoreHandle>,
    pub demo: bool,
    /// The store's single-writer lock. The app holds it until it exits or retries into another
    /// store; the sync worker's extra connection is in the same process and is not a second owner.
    /// `None` whenever a daemon owns the store: the window is a client and must never hold it.
    pub writer_lock: Option<WriterLock>,
    /// The live daemon that answered during boot, if any. `Some` means the store is opened
    /// read-only and every mutation goes through `postino-ipc`.
    pub daemon: Option<crate::daemon::Hello>,
}

/// An openable store: the path, a copyable key handle and the credential seam. Opening a second
/// connection is how the sync worker and the post-sync reload stay off the UI thread.
///
/// `read_only` is set when a daemon owns the store: the window then opens `Store::open_read_only`
/// for reads and never acquires the writer lock.
#[derive(Clone)]
pub struct StoreHandle {
    path: PathBuf,
    key: Arc<StoreKey>,
    vault: Arc<dyn CredentialVault>,
    read_only: bool,
}

impl StoreHandle {
    pub fn new(path: PathBuf, key: Arc<StoreKey>, vault: Arc<dyn CredentialVault>) -> Self {
        Self { path, key, vault, read_only: false }
    }

    /// The handle the window keeps while a daemon owns the store.
    pub fn read_only(path: PathBuf, key: Arc<StoreKey>, vault: Arc<dyn CredentialVault>) -> Self {
        Self { path, key, vault, read_only: true }
    }

    /// Whether every write must go through the daemon instead of this handle.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The same store opened for reading only: used when a daemon takes over the write side.
    pub fn as_read_only(&self) -> Self {
        Self { path: self.path.clone(), key: Arc::clone(&self.key), vault: Arc::clone(&self.vault), read_only: true }
    }

    /// The same store opened for writing: used when this window takes the lock back.
    pub fn as_writable(&self) -> Self {
        Self { path: self.path.clone(), key: Arc::clone(&self.key), vault: Arc::clone(&self.vault), read_only: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn vault(&self) -> Arc<dyn CredentialVault> {
        Arc::clone(&self.vault)
    }

    /// Open a fresh engine on this store. Used after a sync so the UI sees the committed rows while
    /// keeping every piece of view state (selection, filters, open message).
    pub fn open_engine(&self) -> Result<Engine, String> {
        Engine::open_with_vault(self.open_store()?, Arc::clone(&self.vault)).map_err(|error| error.to_string())
    }

    pub fn open_store(&self) -> Result<Store, String> {
        let key = StoreKey::from_bytes(*self.key.as_bytes());
        if self.read_only {
            Store::open_read_only(&self.path, &key).map_err(|error| error.to_string())
        } else {
            Store::open(&self.path, &key).map_err(|error| error.to_string())
        }
    }
}

/// Which step failed. The recovery screen uses it to explain the path, the keyring and what a person
/// can do, in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootStage {
    DataDirectory,
    Keyring,
    /// Another Postino process owns the store and is not a daemon that answers on the channel.
    Daemon,
    Store,
    Engine,
}

impl BootStage {
    pub fn title(self) -> &'static str {
        match self {
            Self::DataDirectory => "La cartella dati non è disponibile",
            Self::Keyring => "Il portachiavi di sistema non risponde",
            Self::Daemon => "L'archivio è in uso da un altro processo",
            Self::Store => "L'archivio locale non si apre",
            Self::Engine => "L'archivio è aperto ma non si può leggere",
        }
    }

    pub fn advice(self) -> &'static str {
        match self {
            Self::DataDirectory => {
                "Postino non sa dove salvare i dati. Controlla che la cartella del profilo esista e che ci sia spazio su disco, poi riprova."
            }
            Self::Keyring => {
                "La chiave dell'archivio vive nel portachiavi di sistema. Sbloccalo o controlla di essere nella sessione giusta, poi riprova: nessuna mail viene toccata."
            }
            Self::Daemon => {
                "Un altro processo Postino sta scrivendo l'archivio e non risponde sul canale locale: due processi non devono scrivere insieme. Chiudi l'altra finestra con `postino daemon stop` e riapri l'app."
            }
            Self::Store => {
                "Il file è danneggiato oppure la chiave nel portachiavi non è quella con cui è stato cifrato. Non cancellarlo: riprova dopo aver sbloccato il portachiavi."
            }
            Self::Engine => {
                "Il database si è aperto ma contiene righe che questa versione non sa leggere. Aggiorna Postino o riprova; le mail restano nel file."
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::DataDirectory => "Cartella dati",
            Self::Keyring => "Portachiavi",
            Self::Daemon => "Daemon",
            Self::Store => "Archivio cifrato",
            Self::Engine => "Lettura dell'archivio",
        }
    }
}

/// A readable store/key failure. It carries the path and the stage, never a key, a password or a
/// message body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootFailure {
    pub path: Option<PathBuf>,
    pub stage: BootStage,
    pub reason: String,
}

impl BootFailure {
    fn new(stage: BootStage, path: Option<PathBuf>, reason: impl Into<String>) -> Self {
        Self { path, stage, reason: reason.into() }
    }

    /// The path line, or the fact that no path could be chosen.
    pub fn path_text(&self) -> String {
        self.path.as_ref().map_or_else(|| "Il percorso dell'archivio non è disponibile.".to_owned(), |path| path.display().to_string())
    }
}

/// Open the mailbox the command line asked for. `now` seeds the synthetic demo mail with a stable
/// instant; store mode never reads it.
pub fn boot(launch: Launch, keys: KeySource, now: DateTime<Utc>) -> Result<Booted, BootFailure> {
    boot_with_lock(launch, keys, now, None)
}

/// Retry an existing app into the same store without dropping the writer lock it already owns.
pub fn boot_with_lock(launch: Launch, keys: KeySource, now: DateTime<Utc>, existing_lock: Option<WriterLock>) -> Result<Booted, BootFailure> {
    match launch {
        Launch::Demo => Ok(Booted { engine: Engine::demo_at(now), store: None, demo: true, writer_lock: None, daemon: None }),
        Launch::Store(path) => {
            let (path, key, vault) = match keys {
                KeySource::Keyring => {
                    let path = match path {
                        Some(path) => path,
                        None => default_store_path()?,
                    };
                    let keyring = Keyring::system().map_err(|error| BootFailure::new(BootStage::Keyring, Some(path.clone()), error.to_string()))?;
                    let key = keyring
                        .load_or_create_key(STORE_SCOPE)
                        .map_err(|error| BootFailure::new(BootStage::Keyring, Some(path.clone()), error.to_string()))?;
                    let vault: Arc<dyn CredentialVault> = Arc::new(PlatformVault::new(keyring));
                    (path, Arc::new(StoreKey::from_bytes(*key.as_bytes())), vault)
                }
                #[cfg(test)]
                KeySource::Fixed { key, vault } => {
                    let path = match path {
                        Some(path) => path,
                        None => default_store_path()?,
                    };
                    (path, key, vault)
                }
            };
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|error| {
                    BootFailure::new(BootStage::DataDirectory, Some(path.clone()), format!("Non riesco a creare {}: {error}", parent.display()))
                })?;
            }
            let path = postino_store::canonical_store_path(&path)
                .map_err(|error| BootFailure::new(BootStage::Store, Some(path.clone()), error.to_string()))?;
            // A live daemon owns the store: probe before taking any lock. The window then opens the
            // store read-only and never writes beside it (P10a2). Absent is normal and means the
            // documented in-process mode.
            if let Some(daemon) = crate::daemon::probe(&path) {
                let handle = StoreHandle::read_only(path.clone(), Arc::clone(&key), Arc::clone(&vault));
                let store = handle.open_store().map_err(|error| BootFailure::new(BootStage::Store, Some(path.clone()), error))?;
                let engine = Engine::open_with_vault(store, Arc::clone(&vault))
                    .map_err(|error| BootFailure::new(BootStage::Engine, Some(path.clone()), error.to_string()))?;
                return Ok(Booted { engine, store: Some(handle), demo: false, writer_lock: None, daemon: Some(daemon) });
            }
            let store_key = StoreKey::from_bytes(*key.as_bytes());
            let writer_lock = match existing_lock {
                Some(lock) if lock.path() == postino_store::lock_path(&path) => lock,
                other => {
                    drop(other);
                    match WriterLock::acquire_as(&path, "app", None) {
                        Ok(lock) => lock,
                        Err(StoreError::StoreBusy { owner, .. }) => {
                            return Err(BootFailure::new(
                                BootStage::Daemon,
                                Some(path.clone()),
                                format!(
                                    "Un altro processo Postino possiede già l'archivio{owner}. Ferma il daemon con `postino daemon stop` e riapri l'app."
                                ),
                            ));
                        }
                        Err(error) => return Err(BootFailure::new(BootStage::Store, Some(path.clone()), error.to_string())),
                    }
                }
            };
            // The lock is held outside the Store handle so `retry_boot` can move it into the
            // replacement app instead of trying to acquire it twice in one process.
            let store = Store::open(&path, &store_key).map_err(|error| BootFailure::new(BootStage::Store, Some(path.clone()), error.to_string()))?;
            let engine = Engine::open_with_vault(store, Arc::clone(&vault))
                .map_err(|error| BootFailure::new(BootStage::Engine, Some(path.clone()), error.to_string()))?;
            let handle = StoreHandle::new(path, key, vault);
            Ok(Booted { engine, store: Some(handle), demo: false, writer_lock: Some(writer_lock), daemon: None })
        }
    }
}

/// The same default path as the CLI: `%LOCALAPPDATA%\Postino\mail.db` and its platform equivalents.
pub fn default_store_path() -> Result<PathBuf, BootFailure> {
    let dir = Engine::data_dir()
        .ok_or_else(|| BootFailure::new(BootStage::DataDirectory, None, "Il sistema non espone una cartella dati locale per l'utente corrente."))?;
    Ok(dir.join("mail.db"))
}

/// The production [`CredentialVault`]: every account id maps to the platform scope
/// `account:<id>:imap`, takes the shared keyring lock, and returns only redacted errors.
pub struct PlatformVault {
    keyring: Keyring,
}

impl PlatformVault {
    pub fn new(keyring: Keyring) -> Self {
        Self { keyring }
    }

    fn scope(account_id: &str) -> Result<String, VaultError> {
        postino_platform::account_scope(account_id).map_err(|error| VaultError::new(error.to_string()))
    }
}

impl CredentialVault for PlatformVault {
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        let scope = Self::scope(account_id)?;
        self.keyring.store_secret(&scope, password).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        let scope = Self::scope(account_id)?;
        self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = Self::scope(account_id)?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn store_smtp_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        let scope = postino_platform::account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.store_secret(&scope, password).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_smtp_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        let scope = postino_platform::account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn delete_smtp_password(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = postino_platform::account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use postino_store::{Account as StoreAccount, Folder as StoreFolder, FolderKind as StoreFolderKind};

    use super::*;

    struct MemoryVault {
        secrets: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl MemoryVault {
        fn new() -> Arc<Self> {
            Arc::new(Self { secrets: Mutex::new(HashMap::new()) })
        }
    }

    impl CredentialVault for MemoryVault {
        fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
            if let Ok(mut secrets) = self.secrets.lock() {
                secrets.insert(account_id.to_owned(), password.to_vec());
            }
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

        fn store_smtp_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
            if let Ok(mut secrets) = self.secrets.lock() {
                secrets.insert(format!("smtp:{account_id}"), password.to_vec());
            }
            Ok(())
        }

        fn load_smtp_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
            Ok(self.secrets.lock().ok().and_then(|secrets| secrets.get(&format!("smtp:{account_id}")).cloned()).map(Zeroizing::new))
        }

        fn delete_smtp_password(&self, account_id: &str) -> Result<(), VaultError> {
            if let Ok(mut secrets) = self.secrets.lock() {
                secrets.remove(&format!("smtp:{account_id}"));
            }
            Ok(())
        }
    }

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join("opencode").join("p6b-postino").join(name);
        let _ = std::fs::create_dir_all(&path);
        path
    }

    fn fixed(key: [u8; 32]) -> (Arc<StoreKey>, Arc<dyn CredentialVault>) {
        (Arc::new(StoreKey::from_bytes(key)), MemoryVault::new())
    }

    fn seed_account(path: &Path, key: &StoreKey) {
        let store = Store::open(path, key).expect("the store opens");
        store
            .upsert_account(&StoreAccount {
                id: "acc_test".to_owned(),
                name: "Postino Test".to_owned(),
                email: "test@example.com".to_owned(),
                provider: "aruba".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account row");
        store
            .upsert_folder(&StoreFolder {
                id: "fld_test".to_owned(),
                account_id: "acc_test".to_owned(),
                name: "Posta in arrivo".to_owned(),
                kind: StoreFolderKind::Inbox,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("folder row");
    }

    #[test]
    fn demo_is_synthetic_and_labeled() {
        let (key, vault) = fixed([7; 32]);
        let booted = boot(Launch::Demo, KeySource::Fixed { key, vault }, Utc::now()).expect("demo");
        assert!(booted.demo);
        assert!(booted.engine.is_demo());
        assert!(booted.store.is_none());
        assert!(!booted.engine.accounts().is_empty());
    }

    #[test]
    fn an_empty_store_boots_with_no_accounts() {
        let path = directory("boot-empty").join("mail.db");
        let _ = std::fs::remove_file(&path);
        let (key, vault) = fixed([0x21; 32]);
        let booted = boot(Launch::Store(Some(path)), KeySource::Fixed { key, vault }, Utc::now()).expect("empty store");
        assert!(!booted.demo);
        assert!(!booted.engine.is_demo());
        assert!(booted.engine.accounts().is_empty());
        assert!(booted.store.is_some());
    }

    #[test]
    fn a_store_account_reopens_as_a_real_mailbox() {
        let path = directory("boot-account").join("mail.db");
        let _ = std::fs::remove_file(&path);
        let (key, vault) = fixed([0x22; 32]);
        seed_account(&path, &key);
        let booted = boot(Launch::Store(Some(path)), KeySource::Fixed { key, vault }, Utc::now()).expect("store");
        assert_eq!(booted.engine.accounts().len(), 1);
        assert_eq!(booted.engine.accounts()[0].email, "test@example.com");
        assert!(!booted.engine.is_demo());
        assert_eq!(booted.engine.accounts().len(), 1);
    }

    #[test]
    fn a_daemon_owned_store_is_a_readable_boot_failure_not_a_second_writer() {
        let path = directory("boot-daemon-owned").join("mail.db");
        let _ = std::fs::remove_file(&path);
        seed_account(&path, &StoreKey::from_bytes([0x26; 32]));
        let lock = WriterLock::acquire_as(&path, "daemon", Some(1)).expect("daemon lock");
        let (key, vault) = fixed([0x26; 32]);
        let error = match boot(Launch::Store(Some(path)), KeySource::Fixed { key, vault }, Utc::now()) {
            Err(error) => error,
            Ok(_) => panic!("the daemon-owned store booted a second writer"),
        };
        assert_eq!(error.stage, BootStage::Daemon, "{error:?}");
        assert!(error.reason.contains("daemon") || error.reason.contains("processo"), "{}", error.reason);
        drop(lock);
    }

    #[test]
    fn a_wrong_key_is_a_readable_store_failure() {
        let path = directory("boot-wrong-key").join("mail.db");
        let _ = std::fs::remove_file(&path);
        seed_account(&path, &StoreKey::from_bytes([0x23; 32]));
        let result = boot(
            Launch::Store(Some(path.clone())),
            KeySource::Fixed { key: Arc::new(StoreKey::from_bytes([0x24; 32])), vault: MemoryVault::new() },
            Utc::now(),
        );
        let error = match result {
            Ok(_) => panic!("a different key must not open the file"),
            Err(error) => error,
        };
        assert_eq!(error.stage, BootStage::Store);
        assert_eq!(error.path.as_deref(), Some(path.as_path()));
        assert!(!error.reason.is_empty());
        assert!(error.path_text().contains("mail.db"));
    }

    #[test]
    fn opening_a_second_engine_keeps_the_same_rows() {
        let path = directory("boot-second-engine").join("mail.db");
        let _ = std::fs::remove_file(&path);
        let (key, vault) = fixed([0x25; 32]);
        seed_account(&path, &key);
        let booted = boot(Launch::Store(Some(path)), KeySource::Fixed { key, vault }, Utc::now()).expect("store");
        let handle = booted.store.expect("handle");
        let reopened = handle.open_engine().expect("a second connection");
        assert_eq!(reopened.accounts().len(), 1);
    }

    #[test]
    fn a_live_daemon_answers_the_reads_writes_and_status_the_window_uses() {
        let directory = directory("boot-live-channel");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("the test directory exists");
        let path = directory.join("mail.db");
        let (key, vault) = fixed([0x73; 32]);
        seed_account(&path, &key);
        let mut config = postino_daemon::DaemonConfig::new(path.clone(), Arc::clone(&key), Arc::clone(&vault));
        config.sync_on_start = false;
        config.sync_jitter = false;
        config.outbox_on_start = false;
        let daemon = postino_daemon::Daemon::spawn(config).expect("the daemon starts");

        // The window's write path is the CLI's path: `op {name, args}` over the same-user channel.
        let mut link = crate::daemon::Link::connect(&path).expect("the channel opens lazily");
        let accounts = link.call("accounts_list", serde_json::json!({})).expect("a read through the daemon");
        assert_eq!(accounts["accounts"].as_array().map(Vec::len), Some(1), "the daemon serves the seeded account");
        let written = link
            .call("account_signature_set", serde_json::json!({ "account": "acc_test", "signature": "Da Postino" }))
            .expect("a write through the daemon");
        assert!(written.is_object(), "the daemon returns a typed result");
        let status = link.status().expect("daemon_status");
        assert!(status["pid"].as_u64().is_some_and(|pid| pid > 0));
        assert!(status["store_root"].as_str().is_some_and(|root| !root.is_empty()));
        // Reading and writing through the channel never means taking the store for this process.
        assert!(WriterLock::acquire_as(&path, "other", None).is_err(), "the daemon is still the writer");

        daemon.shutdown().expect("the daemon stops");
    }

    #[test]
    fn a_live_daemon_makes_the_window_a_read_only_client_without_the_writer_lock() {
        let path = directory("boot-daemon-client").join("mail.db");
        let _ = std::fs::remove_file(&path);
        let (key, vault) = fixed([0x72; 32]);
        seed_account(&path, &key);
        let mut config = postino_daemon::DaemonConfig::new(path.clone(), Arc::clone(&key), Arc::clone(&vault));
        config.sync_on_start = false;
        config.sync_jitter = false;
        config.outbox_on_start = false;
        let daemon = postino_daemon::Daemon::spawn(config).expect("the daemon starts");

        let booted = boot(Launch::Store(Some(path.clone())), KeySource::Fixed { key: Arc::clone(&key), vault: Arc::clone(&vault) }, Utc::now())
            .expect("a daemon-owned store boots");
        assert!(booted.writer_lock.is_none(), "the window must not take the writer lock beside a daemon");
        assert!(booted.daemon.is_some(), "the boot must know which daemon it is talking to");
        let handle = booted.store.clone().expect("handle");
        assert!(handle.is_read_only(), "the window opens the store read-only");
        assert_eq!(booted.engine.accounts().len(), 1);
        // The daemon still owns the lock: another writer is refused while the window is a client.
        assert!(WriterLock::acquire_as(&path, "other", None).is_err());
        // A read-only engine refuses a write instead of forking the store.
        let mut engine = handle.open_engine().expect("a read-only engine");
        assert!(engine.call("message_flag", serde_json::json!({ "id": "missing", "unread": true })).is_err());
        drop(booted);
        daemon.shutdown().expect("the daemon stops");
        assert!(WriterLock::acquire_as(&path, "app", None).is_ok(), "the lock is free after a clean daemon stop");
    }
}
