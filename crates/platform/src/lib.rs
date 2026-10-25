//! Platform seams that the rest of Postino must not implement itself.
//!
//! Today the crate owns one secret: the 256-bit key that encrypts the local store. The key is
//! generated with the operating system's CSPRNG, written to the OS keyring (Windows Credential
//! Manager under service [`KEYRING_SERVICE`], the caller's scope as the user name) and handed back
//! to the caller as a [`StoreKey`]. The store crate never reads a keyring: it only accepts bytes,
//! which keeps the encrypted-store tests off the real vault.
//!
//! The key is never formatted, logged or compared by its `Debug` output. [`StoreKey`] zeroises its
//! bytes when dropped; SQLCipher's internal copy of the key is outside this crate's reach and is
//! documented as a remaining risk in `docs/store-spike.md`.
//!
//! # First-run serialisation
//!
//! Two processes can both observe `NoEntry` and generate different keys. The loser would keep a key
//! that is not in the vault, making its store unreadable later. First-run creation therefore takes
//! an OS file lock at `<app local data>/Postino/keyring.lock` and, while holding it, re-reads the
//! vault, generates a key, writes it and reads it back. A read-back mismatch is retried once inside
//! the lock and then fails closed. The lock is advisory but shared by every Postino process on the
//! machine because the path is derived from the app data directory, not the database path.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use keyring_core::{Entry, Error as KeyringError};
use zeroize::{Zeroize, Zeroizing};

/// The keyring service under which Postino credentials live.
pub const KEYRING_SERVICE: &str = "postino";

/// Length of a store key in bytes (AES-256 for SQLCipher).
const STORE_KEY_BYTES: usize = 32;

/// Longest accepted scope. Scopes become the keyring `user` (and part of the Windows target name),
/// so they are bounded and printable.
const MAX_SCOPE_CHARS: usize = 128;

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
    #[error("the OS keyring did not return the key just written for scope {scope:?}; refusing to use a key that is not in the vault")]
    ReadBackMismatch { scope: String },
    #[error("this platform has no supported OS keyring; Postino refuses to keep the store key on disk")]
    Unsupported,
}

/// Load the store key for `scope`, creating and storing one on first use.
///
/// `scope` names the secret inside the `postino` keyring service. Use a stable value such as
/// `"store"` or the account id; changing it creates a different key and therefore a different
/// store. The function fails closed: when the vault cannot be read or written there is no
/// plaintext fallback and no derived key.
///
/// A normal load takes no lock. First-run creation is serialised across processes with the OS file
/// lock described in the crate docs, and the key is read back before it is returned.
pub fn load_or_create_key(scope: &str) -> Result<StoreKey, PlatformError> {
    validate_scope(scope)?;
    let lock_path = default_lock_path()?;
    load_or_create_key_at(scope, &lock_path)
}

fn load_or_create_key_at(scope: &str, lock_path: &Path) -> Result<StoreKey, PlatformError> {
    let entry = keyring_entry(scope)?;
    match entry.get_secret() {
        Ok(secret) => return store_key_from(scope, Zeroizing::new(secret)),
        Err(KeyringError::NoEntry) => {}
        Err(error) => return Err(vault_error(scope, &error)),
    }

    // First run. Hold the lock across re-read → generate → write → read-back so a second process
    // either sees our key or makes us see its key, never two unverified keys.
    let _lock = lock_first_run(lock_path)?;
    match entry.get_secret() {
        Ok(secret) => return store_key_from(scope, Zeroizing::new(secret)),
        Err(KeyringError::NoEntry) => {}
        Err(error) => return Err(vault_error(scope, &error)),
    }
    for _ in 0..CREATE_ATTEMPTS {
        let key = StoreKey::generate()?;
        entry.set_secret(key.as_bytes()).map_err(|error| vault_error(scope, &error))?;
        let stored = entry.get_secret().map_err(|error| vault_error(scope, &error))?;
        let read_back = store_key_from(scope, Zeroizing::new(stored))?;
        if read_back == key {
            return Ok(key);
        }
    }
    Err(PlatformError::ReadBackMismatch { scope: scope.to_owned() })
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

    /// A credential that accepts writes but always reads back a different 32-byte secret.
    struct MismatchCred {
        writes: Arc<AtomicUsize>,
    }

    impl CredentialApi for MismatchCred {
        fn set_secret(&self, _secret: &[u8]) -> keyring_core::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn get_secret(&self) -> keyring_core::Result<Vec<u8>> {
            if self.writes.load(Ordering::SeqCst) == 0 { Err(keyring_core::Error::NoEntry) } else { Ok(vec![0x55; STORE_KEY_BYTES]) }
        }

        fn delete_credential(&self) -> keyring_core::Result<()> {
            Ok(())
        }

        fn get_credential(&self) -> keyring_core::Result<Option<Arc<Credential>>> {
            Ok(None)
        }

        fn get_specifiers(&self) -> Option<(String, String)> {
            Some((KEYRING_SERVICE.to_owned(), "mismatch".to_owned()))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct MismatchStore {
        writes: Arc<AtomicUsize>,
    }

    impl CredentialStoreApi for MismatchStore {
        fn vendor(&self) -> String {
            "postino-test".to_owned()
        }

        fn id(&self) -> String {
            "postino-test.mismatch".to_owned()
        }

        fn build(&self, _service: &str, _user: &str, _modifiers: Option<&HashMap<&str, &str>>) -> keyring_core::Result<Entry> {
            let credential: Arc<Credential> = Arc::new(MismatchCred { writes: self.writes.clone() });
            Ok(Entry::new_with_credential(credential))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn persistence(&self) -> CredentialPersistence {
            CredentialPersistence::ProcessOnly
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
    fn a_read_back_mismatch_retries_once_and_fails_closed() {
        let _guard = vault();
        let writes = Arc::new(AtomicUsize::new(0));
        let store: Arc<keyring_core::CredentialStore> = Arc::new(MismatchStore { writes: writes.clone() });
        keyring_core::set_default_store(store);
        let dir = tempfile::tempdir().expect("temp dir");
        let lock_path = dir.path().join("Postino").join("keyring.lock");

        let error = load_or_create_key_at("mismatch", &lock_path).expect_err("a key that is not in the vault must never be returned");
        assert!(matches!(error, PlatformError::ReadBackMismatch { .. }), "{error}");
        assert!(!error.to_string().contains("55"), "the error leaked key bytes: {error}");
        assert_eq!(writes.load(Ordering::SeqCst), 2, "the mismatch path must retry exactly once");
        assert!(lock_path.exists(), "the first-run lock file was not created");
    }
}
