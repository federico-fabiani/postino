//! Keyring tests. Every test installs `keyring_core::mock` (or a deliberately broken store) as the
//! default credential store *before* calling the platform API, so no test can reach the real
//! Windows Credential Manager. The native path is exercised only by the app at runtime and by a
//! manual, tagged smoke test before release.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use keyring_core::api::CredentialStoreApi;
use keyring_core::{CredentialStore, Entry, Error as KeyringError};
use postino_platform::{KEYRING_SERVICE, PlatformError, load_or_create_key};

/// The default store is process-wide, so tests that replace it must not overlap.
static VAULT: Mutex<()> = Mutex::new(());

fn vault() -> MutexGuard<'static, ()> {
    // A failed assertion in one test must not poison the rest of the suite.
    VAULT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn install_mock() {
    let store: Arc<CredentialStore> = keyring_core::mock::Store::new().expect("mock credential store");
    keyring_core::set_default_store(store);
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
        Err(KeyringError::NoStorageAccess(Box::new(std::io::Error::other("vault is locked"))))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn secret(scope: &str) -> Vec<u8> {
    Entry::new(KEYRING_SERVICE, scope).expect("entry").get_secret().unwrap_or_else(|error| panic!("the vault holds no secret for {scope:?}: {error}"))
}

#[test]
fn create_is_idempotent_and_returns_the_same_key() {
    let _guard = vault();
    install_mock();

    let first = load_or_create_key("store").expect("first load creates the key");
    let second = load_or_create_key("store").expect("second load returns the stored key");
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

    let key = load_or_create_key("fresh").expect("create");
    // A separate Entry, as a second process would build, reads the same secret.
    let fresh = secret("fresh");
    assert_eq!(fresh.as_slice(), key.as_bytes());
}

#[test]
fn scopes_are_independent() {
    let _guard = vault();
    install_mock();

    let first = load_or_create_key("account-a").expect("create a");
    let second = load_or_create_key("account-b").expect("create b");
    assert_ne!(first.as_bytes(), second.as_bytes());
    assert_eq!(secret("account-a").as_slice(), first.as_bytes());
    assert_eq!(secret("account-b").as_slice(), second.as_bytes());
}

#[test]
fn a_missing_vault_is_a_readable_error() {
    let _guard = vault();
    let store: Arc<CredentialStore> = Arc::new(BrokenStore);
    keyring_core::set_default_store(store);

    let error = load_or_create_key("store").expect_err("an unavailable vault must fail closed");
    assert!(matches!(error, PlatformError::Store { .. }), "{error}");
    let text = error.to_string();
    assert!(text.contains("keyring is unavailable"), "the error does not say the vault is unavailable: {text}");
    assert!(text.contains("vault is locked"), "the error hides the platform cause: {text}");
}

#[test]
fn a_read_failure_is_a_readable_error() {
    let _guard = vault();
    install_mock();

    load_or_create_key("read").expect("create");
    let entry = Entry::new(KEYRING_SERVICE, "read").expect("entry");
    let mock = entry.as_any().downcast_ref::<keyring_core::mock::Cred>().expect("mock credential");
    mock.set_error(KeyringError::NoStorageAccess(Box::new(std::io::Error::other("vault went away"))));

    let error = load_or_create_key("read").expect_err("a read failure must not fall back to a new key");
    assert!(matches!(error, PlatformError::Vault { .. }), "{error}");
    assert!(error.to_string().contains("vault went away"), "{error}");
}

#[test]
fn a_secret_of_the_wrong_length_is_refused() {
    let _guard = vault();
    install_mock();

    Entry::new(KEYRING_SERVICE, "short").expect("entry").set_secret(&[7u8; 31]).expect("write a short secret");
    let error = load_or_create_key("short").expect_err("a short secret is not a store key");
    assert!(matches!(error, PlatformError::Malformed { .. }), "{error}");
    assert!(error.to_string().contains("256-bit"), "{error}");
}
