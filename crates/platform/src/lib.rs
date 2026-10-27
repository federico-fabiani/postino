//! Platform seams that the rest of Postino must not implement itself.
//!
//! This crate owns every OS keyring access: the 256-bit key that encrypts the local store and the
//! IMAP credentials of every account. Secrets live in the operating system's vault (Windows
//! Credential Manager, the login keychain, Secret Service) under service [`KEYRING_SERVICE`], with
//! the *scope* as the keyring user name:
//!
//! | Scope | Secret |
//! |---|---|
//! | [`STORE_SCOPE`] (`default`) | the SQLCipher store key |
//! | `account:<id>:imap` | one account's IMAP password or app password |
//! | `account:<id>:smtp` | an optional, separate SMTP password; absent means the IMAP one is used |
//!
//! The store crate never reads a keyring: it only accepts bytes, which keeps the encrypted-store
//! tests off the real vault. Account credentials never leave this crate except as a zeroising
//! buffer that the engine hands to an IMAP session.
//!
//! The key is never formatted, logged or compared by its `Debug` output. [`StoreKey`] zeroises its
//! bytes when dropped; SQLCipher's internal copy of the key is outside this crate's reach and is
//! documented as a remaining risk in `docs/store-spike.md`. [`Keyring::store_secret`] reads the
//! value back before reporting success, and no error variant carries secret bytes.
//!
//! # Serialisation
//!
//! Two processes can both observe `NoEntry` and generate different keys. The loser would keep a key
//! that is not in the vault, making its store unreadable later. Every load-or-create therefore takes
//! an OS file lock at `<app local data>/Postino/keyring.lock` for the whole operation: the first
//! read, and on a miss the re-read → generate → write → read-back, all happen while holding it. A
//! process that finds a key simply returns it under the lock, so it can never read K1 between
//! another process's write and read-back.
//!
//! The same lock serialises credential writes and deletes, so an account password cannot be
//! observed half-written by another Postino process. The lock is advisory but shared by every
//! Postino process on the machine because the path is derived from the app data directory, not the
//! database path.
//!
//! On a read-back mismatch (only possible if something rewrote the vault without the lock) the vault
//! is authoritative for the store key: the stored key is re-read and adopted when it is a valid
//! 256-bit key, because a store may already be encrypted with it. A vault that is still changing
//! after one retry fails closed with [`PlatformError::ReadBackMismatch`]. Generic secrets have no
//! such adoption: a value that does not read back is an error, never silently accepted.
//!
//! # Tests
//!
//! Tests install `keyring_core::mock`, build a [`Keyring::at`] with a temporary lock path and never
//! touch the real vault or `%LOCALAPPDATA%\Postino`.
//!
//! The same rule applies to the OS surfaces added in P10b1: [`tray`], [`autostart`] and [`notify`]
//! are seams with recorder implementations. Headless tests never need Explorer, the notification
//! area or the real Run key; the Windows adapter lives only in `src/win32.rs`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod autostart;
pub mod notify;
pub mod tray;
#[cfg(windows)]
mod win32;

pub use autostart::{Autostart, AutostartError, AutostartStatus, RecorderAutostart, RegistryAutostart, autostart_command};
pub use notify::{MAX_NOTICE_BODY_CHARS, MAX_NOTICE_TITLE_CHARS, Notice, Notifier, NotifyError, RecorderNotifier};
pub use tray::{RecorderTray, Tray, TrayAction, TrayError, TrayLaunch, TrayState};
#[cfg(windows)]
pub use win32::Win32Tray;

/// Replace `destination` with `source` in one step. On Windows this is `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING`; elsewhere it is `rename`. Both files must be on the same volume.
pub fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        win32::replace_file(source, destination)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(source, destination)
    }
}

use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use keyring_core::{Entry, Error as KeyringError};
use zeroize::{Zeroize, Zeroizing};

/// The keyring service under which Postino credentials live.
pub const KEYRING_SERVICE: &str = "postino";

/// Scope of the encrypted store key. Changing it creates a different key and therefore a different
/// store.
pub const STORE_SCOPE: &str = "default";

/// Prefix and suffix of the per-account IMAP credential scope.
pub const ACCOUNT_SCOPE_PREFIX: &str = "account:";
/// Suffix of the per-account IMAP credential scope.
pub const ACCOUNT_SCOPE_SUFFIX: &str = ":imap";
/// Suffix of the per-account SMTP credential scope. When it is absent the engine falls back to the
/// IMAP credential, which is what Aruba and Gmail app passwords do in practice.
pub const ACCOUNT_SMTP_SCOPE_SUFFIX: &str = ":smtp";

/// Length of a store key in bytes (AES-256 for SQLCipher).
const STORE_KEY_BYTES: usize = 32;

/// Longest accepted scope. Scopes become the keyring `user` (and part of the Windows target name),
/// so they are bounded and printable.
const MAX_SCOPE_CHARS: usize = 128;

/// Longest accepted account id inside a credential scope.
const MAX_ACCOUNT_ID_CHARS: usize = 64;

/// How many generate → write → read-back attempts happen inside the first-run lock.
const CREATE_ATTEMPTS: usize = 2;

/// A 256-bit store key.
///
/// There is deliberately no `Clone` and no `Debug` that shows bytes: the key is only useful as
/// [`StoreKey::as_bytes`], and a key must never reach a log line. The bytes are zeroised on drop.
pub struct StoreKey([u8; STORE_KEY_BYTES]);

impl StoreKey {
    /// Wrap 32 bytes, for example after a length-checked read from the vault.
    pub const fn from_bytes(bytes: [u8; STORE_KEY_BYTES]) -> Self {
        Self(bytes)
    }

    /// The raw key bytes, for handing to `postino_store::Store::open`.
    pub fn as_bytes(&self) -> &[u8; STORE_KEY_BYTES] {
        &self.0
    }

    /// Draw a fresh key from the OS CSPRNG. Failure is an error, never an all-zero fallback.
    pub fn generate() -> Result<Self, PlatformError> {
        // The guard wipes the buffer even if the random source fails part-way.
        let mut bytes = Zeroizing::new([0u8; STORE_KEY_BYTES]);
        getrandom::fill(&mut *bytes)?;
        Ok(Self(*bytes))
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreKey(<redacted>)")
    }
}

impl Drop for StoreKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl PartialEq for StoreKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for StoreKey {}

/// Errors from the platform key seam. They are safe to show a user and to log: no variant carries
/// key material or a raw credential.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("keyring scope must be 1-{MAX_SCOPE_CHARS} printable characters and contain no control characters")]
    InvalidScope,
    #[error("an account id must be 1-{MAX_ACCOUNT_ID_CHARS} ASCII letters, digits, '_' or '-' to form a keyring scope")]
    InvalidAccount,
    #[error("the OS keyring is unavailable: {detail}")]
    Store { detail: String },
    // The platform error is reduced to its display string on purpose: some keyring errors carry the
    // raw credential blob, and that blob may contain the secret. No variant may retain those bytes.
    #[error("the OS keyring could not be read for scope {scope:?}: {detail}")]
    Vault { scope: String, detail: String },
    #[error("the OS random source failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("the keyring entry for scope {scope:?} does not hold a 256-bit store key")]
    Malformed { scope: String },
    #[error("the app data directory is unavailable, so the keyring lock cannot be placed")]
    DataDir,
    #[error("the keyring lock at {path:?} could not be taken: {detail}")]
    Lock { path: String, detail: String },
    #[error("the OS keyring did not return the secret just written for scope {scope:?}; refusing to use a value that is not in the vault")]
    ReadBackMismatch { scope: String },
    #[error("this platform has no supported OS keyring; Postino refuses to keep secrets on disk")]
    Unsupported,
}

/// The credential scope of one account's IMAP password: `account:<id>:imap`.
///
/// The id is restricted to ASCII letters, digits, `_` and `-`, so it can never introduce a `:` that
/// makes two different accounts share a scope.
pub fn account_scope(account_id: &str) -> Result<String, PlatformError> {
    account_scoped(account_id, ACCOUNT_SCOPE_SUFFIX)
}

/// The credential scope of one account's SMTP password: `account:<id>:smtp`.
///
/// It uses the same validation as [`account_scope`]; a missing entry means the engine falls back to
/// the IMAP credential and never copies one secret into the other's scope.
pub fn account_smtp_scope(account_id: &str) -> Result<String, PlatformError> {
    account_scoped(account_id, ACCOUNT_SMTP_SCOPE_SUFFIX)
}

fn account_scoped(account_id: &str, suffix: &str) -> Result<String, PlatformError> {
    let valid = !account_id.is_empty()
        && account_id.chars().count() <= MAX_ACCOUNT_ID_CHARS
        && account_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        return Err(PlatformError::InvalidAccount);
    }
    let scope = format!("{ACCOUNT_SCOPE_PREFIX}{account_id}{suffix}");
    validate_scope(&scope)?;
    Ok(scope)
}

/// A handle to the OS keyring with an explicit first-run lock path.
///
/// Production code uses [`Keyring::system`] (the lock lives next to the app data). Tests use
/// [`Keyring::at`] with a temporary path, so a test run can never take the real lock or touch the
/// real vault. Every method validates the scope before it reaches the keyring.
#[derive(Debug, Clone)]
pub struct Keyring {
    lock_path: PathBuf,
}

impl Keyring {
    /// The keyring whose lock lives under the platform data directory.
    pub fn system() -> Result<Self, PlatformError> {
        Ok(Self { lock_path: default_lock_path()? })
    }

    /// A keyring whose serialisation lock lives at `lock_path`. Tests and embedders only.
    pub fn at(lock_path: impl Into<PathBuf>) -> Self {
        Self { lock_path: lock_path.into() }
    }

    /// The lock file every operation on this keyring takes.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Load the store key for `scope`, creating and storing one on first use.
    ///
    /// `scope` names the secret inside the `postino` keyring service. Use [`STORE_SCOPE`] for the
    /// local store. The function fails closed: when the vault cannot be read or written there is no
    /// plaintext fallback and no derived key.
    pub fn load_or_create_key(&self, scope: &str) -> Result<StoreKey, PlatformError> {
        validate_scope(scope)?;
        let entry = keyring_entry(scope)?;
        // Hold the lock across the first read too. A fast-path read outside the lock could observe
        // another process's key between its write and its read-back, while this process then wrote a
        // second key over it.
        let _lock = lock_first_run(&self.lock_path)?;
        match entry.get_secret() {
            Ok(secret) => return store_key_from(scope, Zeroizing::new(secret)),
            Err(KeyringError::NoEntry) => {}
            Err(error) => return Err(vault_error(scope, &error)),
        }
        for _ in 0..CREATE_ATTEMPTS {
            let key = StoreKey::generate()?;
            entry.set_secret(key.as_bytes()).map_err(|error| vault_error(scope, &error))?;
            let read_back = read_key(scope, &entry)?;
            if read_back == key {
                return Ok(key);
            }
            // Something rewrote the vault without our lock, or the vault ignores writes. It is
            // authoritative: a store may already be encrypted with the key it holds, so adopt a
            // stable value rather than generating a second one. A vault that is still moving after
            // one retry fails closed below.
            if read_key(scope, &entry)? == read_back {
                return Ok(read_back);
            }
        }
        Err(PlatformError::ReadBackMismatch { scope: scope.to_owned() })
    }

    /// Create or replace one secret, then read it back before reporting success.
    ///
    /// The whole write happens under the first-run lock. A value that the vault does not return
    /// exactly is [`PlatformError::ReadBackMismatch`]: the caller must not assume the credential is
    /// usable. The bytes are never formatted, logged or included in an error.
    pub fn store_secret(&self, scope: &str, secret: &[u8]) -> Result<(), PlatformError> {
        validate_scope(scope)?;
        let entry = keyring_entry(scope)?;
        let _lock = lock_first_run(&self.lock_path)?;
        entry.set_secret(secret).map_err(|error| vault_error(scope, &error))?;
        match entry.get_secret() {
            Ok(read_back) => {
                let read_back = Zeroizing::new(read_back);
                if read_back.as_slice() == secret { Ok(()) } else { Err(PlatformError::ReadBackMismatch { scope: scope.to_owned() }) }
            }
            Err(error) => Err(vault_error(scope, &error)),
        }
    }

    /// Read one secret, or `None` when the vault has no entry for `scope`.
    ///
    /// The returned buffer zeroises itself when dropped. Callers must not format, log or clone it
    /// without the same guarantee.
    pub fn load_secret(&self, scope: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
        validate_scope(scope)?;
        let entry = keyring_entry(scope)?;
        let _lock = lock_first_run(&self.lock_path)?;
        match entry.get_secret() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(vault_error(scope, &error)),
        }
    }

    /// Delete one secret. Deleting a missing entry is a success, so callers can retry a cleanup.
    pub fn delete_secret(&self, scope: &str) -> Result<(), PlatformError> {
        validate_scope(scope)?;
        let entry = keyring_entry(scope)?;
        let _lock = lock_first_run(&self.lock_path)?;
        match entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(vault_error(scope, &error)),
        }
    }
}

/// Load the store key for `scope` from the system keyring, creating it on first use.
///
/// This is the production convenience form of [`Keyring::load_or_create_key`].
pub fn load_or_create_key(scope: &str) -> Result<StoreKey, PlatformError> {
    validate_scope(scope)?;
    Keyring::system()?.load_or_create_key(scope)
}

/// Create or replace one secret in the system keyring.
pub fn store_secret(scope: &str, secret: &[u8]) -> Result<(), PlatformError> {
    validate_scope(scope)?;
    Keyring::system()?.store_secret(scope, secret)
}

/// Read one secret from the system keyring.
pub fn load_secret(scope: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
    validate_scope(scope)?;
    Keyring::system()?.load_secret(scope)
}

/// Delete one secret from the system keyring.
pub fn delete_secret(scope: &str) -> Result<(), PlatformError> {
    validate_scope(scope)?;
    Keyring::system()?.delete_secret(scope)
}

fn read_key(scope: &str, entry: &Entry) -> Result<StoreKey, PlatformError> {
    let secret = entry.get_secret().map_err(|error| vault_error(scope, &error))?;
    store_key_from(scope, Zeroizing::new(secret))
}

fn default_lock_path() -> Result<PathBuf, PlatformError> {
    let dirs = directories::BaseDirs::new().ok_or(PlatformError::DataDir)?;
    Ok(dirs.data_local_dir().join("Postino").join("keyring.lock"))
}

fn lock_first_run(path: &Path) -> Result<File, PlatformError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| PlatformError::Lock { path: path.display().to_string(), detail: error.to_string() })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| PlatformError::Lock { path: path.display().to_string(), detail: error.to_string() })?;
    file.lock_exclusive().map_err(|error| PlatformError::Lock { path: path.display().to_string(), detail: error.to_string() })?;
    Ok(file)
}

fn vault_error(scope: &str, error: &KeyringError) -> PlatformError {
    PlatformError::Vault { scope: scope.to_owned(), detail: error.to_string() }
}

fn store_key_from(scope: &str, secret: Zeroizing<Vec<u8>>) -> Result<StoreKey, PlatformError> {
    // Copy into the fixed array; the Zeroizing buffer is wiped when it drops.
    let bytes: [u8; STORE_KEY_BYTES] = secret.as_slice().try_into().map_err(|_| PlatformError::Malformed { scope: scope.to_owned() })?;
    Ok(StoreKey::from_bytes(bytes))
}

fn validate_scope(scope: &str) -> Result<(), PlatformError> {
    let valid = !scope.is_empty() && scope.chars().count() <= MAX_SCOPE_CHARS && !scope.chars().any(|character| character.is_control());
    if valid { Ok(()) } else { Err(PlatformError::InvalidScope) }
}

/// Build the keyring entry, installing the native store on first use.
///
/// Tests call `keyring_core::set_default_store(keyring_core::mock::Store::new()?)` first and this
/// function then uses the mock; the production path is the same code.
fn keyring_entry(scope: &str) -> Result<Entry, PlatformError> {
    if keyring_core::get_default_store().is_none() {
        install_native_store()?;
    }
    Entry::new(KEYRING_SERVICE, scope).map_err(|error| PlatformError::Store { detail: error.to_string() })
}

#[cfg(windows)]
fn install_native_store() -> Result<(), PlatformError> {
    let store = windows_native_keyring_store::Store::new().map_err(|error| PlatformError::Store { detail: error.to_string() })?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(not(windows))]
fn install_native_store() -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported)
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use keyring_core::api::{CredentialApi, CredentialStoreApi};
    use keyring_core::{Credential, CredentialPersistence, Entry};

    use super::*;

    /// The default store is process-wide, so tests that replace it must not overlap.
    static VAULT: Mutex<()> = Mutex::new(());

    fn vault() -> MutexGuard<'static, ()> {
        VAULT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A key that another process (or a broken vault) leaves in the credential: valid, stable, and
    /// never the one this process generated.
    const FOREIGN_KEY: [u8; STORE_KEY_BYTES] = [0x55; STORE_KEY_BYTES];

    fn install_mock() {
        let store: Arc<keyring_core::CredentialStore> = keyring_core::mock::Store::new().expect("mock credential store");
        keyring_core::set_default_store(store);
    }

    /// Every keyring test passes its own lock path. Nothing may fall back to
    /// `%LOCALAPPDATA%\Postino\keyring.lock`, so the suite never writes outside its temp directory.
    fn temp_lock() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("Postino").join("keyring.lock");
        (dir, path)
    }

    fn secret(scope: &str) -> Vec<u8> {
        Entry::new(KEYRING_SERVICE, scope).expect("entry").get_secret().unwrap_or_else(|error| panic!("no secret for {scope:?}: {error}"))
    }

    /// A credential that accepts writes and then always returns a different, stable key.
    struct ForeignKeyCred {
        writes: Arc<AtomicUsize>,
    }

    impl CredentialApi for ForeignKeyCred {
        fn set_secret(&self, _secret: &[u8]) -> keyring_core::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn get_secret(&self) -> keyring_core::Result<Vec<u8>> {
            if self.writes.load(Ordering::SeqCst) == 0 { Err(keyring_core::Error::NoEntry) } else { Ok(FOREIGN_KEY.to_vec()) }
        }

        fn delete_credential(&self) -> keyring_core::Result<()> {
            Ok(())
        }

        fn get_credential(&self) -> keyring_core::Result<Option<Arc<Credential>>> {
            Ok(None)
        }

        fn get_specifiers(&self) -> Option<(String, String)> {
            Some((KEYRING_SERVICE.to_owned(), "foreign".to_owned()))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct ForeignKeyStore {
        writes: Arc<AtomicUsize>,
    }

    impl CredentialStoreApi for ForeignKeyStore {
        fn vendor(&self) -> String {
            "postino-test".to_owned()
        }

        fn id(&self) -> String {
            "postino-test.foreign".to_owned()
        }

        fn build(&self, _service: &str, _user: &str, _modifiers: Option<&HashMap<&str, &str>>) -> keyring_core::Result<Entry> {
            let credential: Arc<Credential> = Arc::new(ForeignKeyCred { writes: self.writes.clone() });
            Ok(Entry::new_with_credential(credential))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn persistence(&self) -> CredentialPersistence {
            CredentialPersistence::ProcessOnly
        }
    }

    /// A credential whose every read returns a different valid 32-byte value.
    struct UnstableCred {
        writes: Arc<AtomicUsize>,
        reads: Arc<AtomicUsize>,
    }

    impl CredentialApi for UnstableCred {
        fn set_secret(&self, _secret: &[u8]) -> keyring_core::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn get_secret(&self) -> keyring_core::Result<Vec<u8>> {
            if self.writes.load(Ordering::SeqCst) == 0 {
                return Err(keyring_core::Error::NoEntry);
            }
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(vec![0x40 + (read % 0x40) as u8; STORE_KEY_BYTES])
        }

        fn delete_credential(&self) -> keyring_core::Result<()> {
            Ok(())
        }

        fn get_credential(&self) -> keyring_core::Result<Option<Arc<Credential>>> {
            Ok(None)
        }

        fn get_specifiers(&self) -> Option<(String, String)> {
            Some((KEYRING_SERVICE.to_owned(), "unstable".to_owned()))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct UnstableStore {
        writes: Arc<AtomicUsize>,
        reads: Arc<AtomicUsize>,
    }

    impl CredentialStoreApi for UnstableStore {
        fn vendor(&self) -> String {
            "postino-test".to_owned()
        }

        fn id(&self) -> String {
            "postino-test.unstable".to_owned()
        }

        fn build(&self, _service: &str, _user: &str, _modifiers: Option<&HashMap<&str, &str>>) -> keyring_core::Result<Entry> {
            let credential: Arc<Credential> = Arc::new(UnstableCred { writes: self.writes.clone(), reads: self.reads.clone() });
            Ok(Entry::new_with_credential(credential))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn persistence(&self) -> CredentialPersistence {
            CredentialPersistence::ProcessOnly
        }
    }

    /// A store that refuses to build entries, standing in for a locked or absent vault.
    struct BrokenStore;

    impl CredentialStoreApi for BrokenStore {
        fn vendor(&self) -> String {
            "postino-test".to_owned()
        }

        fn id(&self) -> String {
            "postino-test.broken".to_owned()
        }

        fn build(&self, _service: &str, _user: &str, _modifiers: Option<&HashMap<&str, &str>>) -> keyring_core::Result<Entry> {
            Err(keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("vault is locked"))))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn debug_never_prints_the_key() {
        let key = StoreKey::from_bytes([0xab; STORE_KEY_BYTES]);
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "StoreKey(<redacted>)");
        assert!(!rendered.contains("ab"));
    }

    #[test]
    fn generated_keys_are_32_random_bytes() {
        let first = StoreKey::generate().expect("OS random source");
        let second = StoreKey::generate().expect("OS random source");
        assert_eq!(first.as_bytes().len(), STORE_KEY_BYTES);
        assert_ne!(first.as_bytes(), second.as_bytes());
        assert_ne!(first.as_bytes(), &[0u8; STORE_KEY_BYTES]);
    }

    #[test]
    fn scopes_are_validated_before_the_vault_is_touched() {
        for scope in ["", "a\nb", "a\u{7}b"] {
            let error = load_or_create_key(scope).expect_err("an invalid scope must be refused");
            assert!(matches!(error, PlatformError::InvalidScope), "{error}");
        }
        let long = "a".repeat(MAX_SCOPE_CHARS + 1);
        assert!(matches!(load_or_create_key(&long), Err(PlatformError::InvalidScope)));
    }

    #[test]
    fn account_scopes_are_bounded_and_unambiguous() {
        assert_eq!(account_scope("acc_0123456789abcdef").expect("scope"), "account:acc_0123456789abcdef:imap");
        assert_eq!(account_smtp_scope("acc_0123456789abcdef").expect("scope"), "account:acc_0123456789abcdef:smtp");
        for id in ["", "a:b", "a/b", "a b", "a\nb", "../x"] {
            assert!(matches!(account_scope(id), Err(PlatformError::InvalidAccount)), "{id:?}");
            assert!(matches!(account_smtp_scope(id), Err(PlatformError::InvalidAccount)), "{id:?}");
        }
        let long = "a".repeat(MAX_ACCOUNT_ID_CHARS + 1);
        assert!(matches!(account_scope(&long), Err(PlatformError::InvalidAccount)));
        assert!(matches!(account_smtp_scope(&long), Err(PlatformError::InvalidAccount)));
    }

    #[test]
    fn first_run_creates_one_key_and_a_reload_returns_it() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);

        let first = keyring.load_or_create_key("store").expect("first load creates the key");
        assert!(lock.exists(), "the load did not take its lock at the path it was given");
        let second = keyring.load_or_create_key("store").expect("second load returns the stored key");
        assert_eq!(first.as_bytes(), second.as_bytes(), "load_or_create_key generated a second key");

        let stored = secret("store");
        assert_eq!(stored.len(), 32, "the vault secret is not a 256-bit key");
        assert_eq!(stored.as_slice(), first.as_bytes(), "the stored secret differs from the returned key");

        let credentials = Entry::search(&HashMap::from([("service", KEYRING_SERVICE), ("user", "store")])).expect("search");
        let matching = credentials
            .iter()
            .filter(|entry| entry.get_specifiers().is_some_and(|(service, user)| service == KEYRING_SERVICE && user == "store"))
            .count();
        assert_eq!(matching, 1, "the first load left more than one credential");
    }

    #[test]
    fn a_fresh_handle_reads_the_same_key() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();

        let key = Keyring::at(&lock).load_or_create_key("fresh").expect("create");
        // A separate Entry, as a second process would build, reads the same secret.
        assert_eq!(secret("fresh").as_slice(), key.as_bytes());
        assert!(lock.exists());
    }

    #[test]
    fn scopes_are_independent() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);

        let first = keyring.load_or_create_key("account-a").expect("create a");
        let second = keyring.load_or_create_key("account-b").expect("create b");
        assert_ne!(first.as_bytes(), second.as_bytes());
        assert_eq!(secret("account-a").as_slice(), first.as_bytes());
        assert_eq!(secret("account-b").as_slice(), second.as_bytes());
    }

    #[test]
    fn a_missing_vault_is_a_readable_error() {
        let _guard = vault();
        let store: Arc<keyring_core::CredentialStore> = Arc::new(BrokenStore);
        keyring_core::set_default_store(store);
        let (_dir, lock) = temp_lock();

        let error = Keyring::at(&lock).load_or_create_key("store").expect_err("an unavailable vault must fail closed");
        assert!(matches!(error, PlatformError::Store { .. }), "{error}");
        let text = error.to_string();
        assert!(text.contains("keyring is unavailable"), "the error does not say the vault is unavailable: {text}");
        assert!(text.contains("vault is locked"), "the error hides the platform cause: {text}");
    }

    #[test]
    fn a_read_failure_is_a_readable_error() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);

        keyring.load_or_create_key("read").expect("create");
        let entry = Entry::new(KEYRING_SERVICE, "read").expect("entry");
        let mock = entry.as_any().downcast_ref::<keyring_core::mock::Cred>().expect("mock credential");
        mock.set_error(keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("vault went away"))));

        let error = keyring.load_or_create_key("read").expect_err("a read failure must not fall back to a new key");
        assert!(matches!(error, PlatformError::Vault { .. }), "{error}");
        assert!(error.to_string().contains("vault went away"), "{error}");
    }

    #[test]
    fn a_secret_of_the_wrong_length_is_refused() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();

        Entry::new(KEYRING_SERVICE, "short").expect("entry").set_secret(&[7u8; 31]).expect("write a short secret");
        let error = Keyring::at(&lock).load_or_create_key("short").expect_err("a short secret is not a store key");
        assert!(matches!(error, PlatformError::Malformed { .. }), "{error}");
        assert!(error.to_string().contains("256-bit"), "{error}");
    }

    #[test]
    fn the_first_run_lock_is_exclusive() {
        let (_dir, lock) = temp_lock();
        let (holding, held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let holder_path = lock.clone();
        let holder = std::thread::spawn(move || {
            let file = lock_first_run(&holder_path).expect("the first thread takes the lock");
            holding.send(()).expect("signal the lock is held");
            released.recv().expect("wait for the release");
            drop(file);
        });
        held.recv().expect("the first thread must report the lock");

        let probe = OpenOptions::new().read(true).write(true).open(&lock).expect("open the same lock file");
        assert!(!probe.try_lock_exclusive().expect("try the held lock"), "a second handle took the first-run lock while it was held");
        release.send(()).expect("release the holder");
        holder.join().expect("the holder thread joins");

        assert!(probe.try_lock_exclusive().expect("try the released lock"), "the lock was still held after its owner dropped it");
        FileExt::unlock(&probe).expect("release the probe");
    }

    #[test]
    fn an_existing_key_is_read_only_after_taking_the_lock() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);
        let key = keyring.load_or_create_key("locked").expect("create");
        let held = lock_first_run(&lock).expect("hold the first-run lock in the test");

        let (started, started_rx) = std::sync::mpsc::channel();
        let (done, done_rx) = std::sync::mpsc::channel();
        let reader = keyring.clone();
        let reader_thread = std::thread::spawn(move || {
            started.send(()).expect("signal the reader is about to load");
            let result = reader.load_or_create_key("locked");
            done.send(result.map(|key| key.as_bytes().to_vec())).expect("send the loaded key");
        });
        started_rx.recv().expect("the reader started");
        assert!(
            done_rx.recv_timeout(std::time::Duration::from_millis(250)).is_err(),
            "the stored key was read while another process held the first-run lock"
        );

        drop(held);
        let loaded = done_rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the loader returns after the release").expect("load");
        assert_eq!(loaded, key.as_bytes().to_vec(), "the reader returned a different key");
        reader_thread.join().expect("the reader joins");
    }

    #[test]
    fn a_foreign_key_in_the_vault_is_adopted_after_a_read_back_mismatch() {
        let _guard = vault();
        let writes = Arc::new(AtomicUsize::new(0));
        let store: Arc<keyring_core::CredentialStore> = Arc::new(ForeignKeyStore { writes: writes.clone() });
        keyring_core::set_default_store(store);
        let (_dir, lock) = temp_lock();

        let key = Keyring::at(&lock).load_or_create_key("foreign").expect("the key the vault actually holds must be adopted");
        assert_eq!(key.as_bytes(), &FOREIGN_KEY, "the read-back mismatch did not adopt the stored key");
        assert_eq!(writes.load(Ordering::SeqCst), 1, "the mismatch path generated a second key");
        assert!(lock.exists(), "adoption did not happen under the first-run lock");
    }

    #[test]
    fn an_unstable_vault_fails_closed_after_one_retry() {
        let _guard = vault();
        let writes = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let store: Arc<keyring_core::CredentialStore> = Arc::new(UnstableStore { writes: writes.clone(), reads: reads.clone() });
        keyring_core::set_default_store(store);
        let (_dir, lock) = temp_lock();

        let error = Keyring::at(&lock).load_or_create_key("unstable").expect_err("a key that is never read back must not be returned");
        assert!(matches!(error, PlatformError::ReadBackMismatch { .. }), "{error}");
        assert_eq!(writes.load(Ordering::SeqCst), 2, "the unstable path must retry exactly once");
        assert_eq!(reads.load(Ordering::SeqCst), 4, "each attempt reads back and confirms once");
        assert!(lock.exists(), "the failure must still leave the lock at the given path");
    }

    #[test]
    fn secrets_round_trip_update_and_delete() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);
        let scope = account_scope("acc_secret").expect("scope");

        keyring.store_secret(&scope, b"first app password").expect("store");
        assert_eq!(keyring.load_secret(&scope).expect("load").as_deref().map(Vec::as_slice), Some(b"first app password".as_slice()));
        assert!(lock.exists(), "storing a secret did not take the keyring lock");

        keyring.store_secret(&scope, b"second app password").expect("update");
        assert_eq!(keyring.load_secret(&scope).expect("load").as_deref().map(Vec::as_slice), Some(b"second app password".as_slice()));

        keyring.delete_secret(&scope).expect("delete");
        assert!(keyring.load_secret(&scope).expect("load after delete").is_none());
        keyring.delete_secret(&scope).expect("deleting a missing secret is a success");
    }

    #[test]
    fn two_account_scopes_are_independent() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);
        let first = account_scope("a").expect("scope a");
        let second = account_scope("b").expect("scope b");

        keyring.store_secret(&first, b"secret-for-a").expect("store a");
        keyring.store_secret(&second, b"secret-for-b").expect("store b");
        assert_eq!(keyring.load_secret(&first).expect("load a").as_deref().map(Vec::as_slice), Some(b"secret-for-a".as_slice()));
        assert_eq!(keyring.load_secret(&second).expect("load b").as_deref().map(Vec::as_slice), Some(b"secret-for-b".as_slice()));

        keyring.delete_secret(&first).expect("delete a");
        assert!(keyring.load_secret(&first).expect("load a after delete").is_none());
        assert_eq!(
            keyring.load_secret(&second).expect("load b after deleting a").as_deref().map(Vec::as_slice),
            Some(b"secret-for-b".as_slice()),
            "the two account scopes are not independent"
        );
    }

    #[test]
    fn a_missing_secret_is_none_and_an_empty_secret_round_trips() {
        let _guard = vault();
        install_mock();
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);

        assert!(keyring.load_secret("account:acc_none:imap").expect("missing").is_none());
        keyring.store_secret("account:acc_empty:imap", b"").expect("store an empty secret");
        assert_eq!(keyring.load_secret("account:acc_empty:imap").expect("load").as_deref().map(Vec::as_slice), Some(b"".as_slice()));
    }

    #[test]
    fn secret_scopes_are_validated_and_errors_never_carry_the_secret() {
        let _guard = vault();
        let store: Arc<keyring_core::CredentialStore> = Arc::new(BrokenStore);
        keyring_core::set_default_store(store);
        let (_dir, lock) = temp_lock();
        let keyring = Keyring::at(&lock);

        assert!(matches!(keyring.store_secret("bad\nscope", b"x"), Err(PlatformError::InvalidScope)));
        let secret = b"never-print-this-app-password";
        let error = keyring.store_secret("account:acc_broken:imap", secret).expect_err("the vault is unavailable");
        let text = error.to_string();
        assert!(!text.contains("never-print-this-app-password"), "{text}");
        assert!(text.contains("keyring is unavailable"), "{text}");
    }

    #[test]
    fn a_generic_secret_that_does_not_read_back_is_refused() {
        let _guard = vault();
        let writes = Arc::new(AtomicUsize::new(0));
        let store: Arc<keyring_core::CredentialStore> = Arc::new(ForeignKeyStore { writes });
        keyring_core::set_default_store(store);
        let (_dir, lock) = temp_lock();

        let error = Keyring::at(&lock).store_secret("account:acc_foreign:imap", b"my password").expect_err("a different value must not be accepted");
        assert!(matches!(error, PlatformError::ReadBackMismatch { .. }), "{error}");
        assert!(!error.to_string().contains("my password"), "{error}");
    }
}
