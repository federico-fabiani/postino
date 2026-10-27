//! Store ownership is path-independent: a symlink is canonicalised, and a hard-linked database is
//! refused because two names would otherwise derive two independent `.lock` files for one inode.

use std::path::Path;

use postino_store::{Store, StoreError, StoreKey};

fn seed(path: &Path) {
    let key = StoreKey::from_bytes([0x31; 32]);
    let store = Store::open(path, &key).expect("create store");
    drop(store);
}

#[test]
fn a_hard_linked_database_is_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = directory.path().join("mail.db");
    seed(&store);
    let alias = directory.path().join("alias.db");
    std::fs::hard_link(&store, &alias).expect("hard link");
    let key = StoreKey::from_bytes([0x31; 32]);
    match Store::open_locked(&store, &key) {
        Err(StoreError::HardLinked { .. }) => {}
        other => panic!("the hard-linked store was not refused: {}", other.err().map(|error| error.to_string()).unwrap_or_default()),
    }
}

#[cfg(unix)]
#[test]
fn a_symlinked_store_resolves_to_the_same_lock_and_endpoint() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = directory.path().join("mail.db");
    seed(&store);
    let alias = directory.path().join("alias.db");
    std::os::unix::fs::symlink(&store, &alias).expect("symlink");
    let key = StoreKey::from_bytes([0x31; 32]);
    let first = Store::open_locked(&store, &key).expect("canonical writer");
    match Store::open_locked(&alias, &key) {
        Err(StoreError::StoreBusy { .. }) => {}
        other => panic!("the symlink did not hit the canonical lock: {}", other.err().map(|error| error.to_string()).unwrap_or_default()),
    }
    drop(first);
}
