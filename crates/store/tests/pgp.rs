//! PGP key rows: scoping, caps, duplicate fingerprints and delete/export behavior.

use postino_store::{Account, PgpKeyInsert, Store, StoreError, StoreKey};
use zeroize::Zeroizing;

fn key() -> StoreKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    StoreKey::from_bytes(bytes)
}

fn account(id: &str) -> Account {
    Account {
        id: id.to_owned(),
        name: "Studio Verdi".to_owned(),
        email: "studio@example.com".to_owned(),
        provider: "demo".to_owned(),
        signature: String::new(),
        created_at: 1_700_000_000_000,
    }
}

fn row(id: &str, account_id: &str, fingerprint: &str, secret: bool) -> PgpKeyInsert {
    PgpKeyInsert {
        id: id.to_owned(),
        account_id: account_id.to_owned(),
        fingerprint: fingerprint.to_owned(),
        key_id: "0123456789ABCDEF".to_owned(),
        user_id: Some("Postino Test <test@example.com>".to_owned()),
        public_armor: format!("-----BEGIN PGP PUBLIC KEY BLOCK-----\n{fingerprint}\n-----END PGP PUBLIC KEY BLOCK-----\n"),
        secret_armor: secret
            .then(|| Zeroizing::new(format!("-----BEGIN PGP PRIVATE KEY BLOCK-----\n{fingerprint}\n-----END PGP PRIVATE KEY BLOCK-----\n"))),
        protected: secret,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

#[test]
fn insert_list_export_and_delete_are_account_scoped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &key()).expect("open");
    store.upsert_account(&account("acc_a")).expect("account a");
    store.upsert_account(&account("acc_b")).expect("account b");

    store.insert_pgp_key_limited(&row("pgp_1", "acc_a", "AAAA", true), 64).expect("private key");
    store.insert_pgp_key_limited(&row("pgp_2", "acc_b", "BBBB", false), 64).expect("public key");

    let a = store.list_pgp_keys("acc_a").expect("list a");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].fingerprint, "AAAA");
    assert!(a[0].has_secret && a[0].protected);
    let b = store.list_pgp_keys("acc_b").expect("list b");
    assert_eq!(b.len(), 1);
    assert!(!b[0].has_secret && !b[0].protected);

    // Export returns only the public half, even for a row that holds a private key.
    let exported = store.export_pgp_public("acc_a", "pgp_1").expect("export").expect("present");
    assert_eq!(exported.fingerprint, "AAAA");
    assert!(exported.public_armor.contains("PUBLIC KEY BLOCK"));
    assert!(!exported.public_armor.contains("PRIVATE"));
    assert!(store.export_pgp_public("acc_b", "pgp_1").expect("scoped export").is_none());
    assert!(store.get_pgp_key("acc_b", "pgp_1").expect("scoped get").is_none());

    // Deleting through the wrong account removes nothing; the owner can delete once.
    assert!(!store.delete_pgp_key("acc_b", "pgp_1").expect("scoped delete"));
    assert!(store.delete_pgp_key("acc_a", "pgp_1").expect("delete"));
    assert!(!store.delete_pgp_key("acc_a", "pgp_1").expect("second delete"));
    assert!(store.list_pgp_keys("acc_a").expect("list after delete").is_empty());
}

#[test]
fn a_duplicate_fingerprint_is_refused_across_accounts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &key()).expect("open");
    store.upsert_account(&account("acc_a")).expect("account a");
    store.upsert_account(&account("acc_b")).expect("account b");
    store.insert_pgp_key_limited(&row("pgp_1", "acc_a", "AAAA", true), 64).expect("first");

    let error = store.insert_pgp_key_limited(&row("pgp_2", "acc_b", "AAAA", false), 64).expect_err("duplicate");
    assert!(matches!(error, StoreError::DuplicatePgpFingerprint { .. }), "{error}");
    assert!(store.list_pgp_keys("acc_b").expect("list b").is_empty(), "the refused row was written");
}

#[test]
fn the_per_account_cap_is_enforced_atomically() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &key()).expect("open");
    store.upsert_account(&account("acc_a")).expect("account a");
    for n in 0..64 {
        store
            .insert_pgp_key_limited(&row(&format!("pgp_{n}"), "acc_a", &format!("{n:040X}"), false), 64)
            .unwrap_or_else(|error| panic!("insert {n}: {error}"));
    }
    let error = store.insert_pgp_key_limited(&row("pgp_overflow", "acc_a", "FFFF", false), 64).expect_err("cap");
    assert!(matches!(error, StoreError::PgpKeyLimit { limit: 64 }), "{error}");
    assert_eq!(store.count_pgp_keys("acc_a").expect("count"), 64);
}

#[test]
fn only_the_fingerprint_constraint_is_reported_as_a_duplicate() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &key()).expect("open");
    store.upsert_account(&account("acc_a")).expect("account a");

    let mut invalid = row("pgp_2", "acc_a", "BBBB", true);
    invalid.protected = false;
    let error = store.insert_pgp_key_limited(&invalid, 64).expect_err("CHECK constraint");
    assert!(matches!(error, StoreError::Sqlite(_)), "a non-fingerprint constraint was remapped: {error}");
    assert_eq!(store.count_pgp_keys("acc_a").expect("count"), 0);
}

#[test]
fn secret_armor_is_stored_but_never_exported() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mail.db");
    let store_key = key();
    let secret = "-----BEGIN PGP PRIVATE KEY BLOCK-----\nSENTINEL-SECRET-ARMOR\n-----END PGP PRIVATE KEY BLOCK-----\n";
    {
        let store = Store::open(&path, &store_key).expect("open");
        store.upsert_account(&account("acc_a")).expect("account");
        let mut insert = row("pgp_1", "acc_a", "AAAA", true);
        insert.secret_armor = Some(Zeroizing::new(secret.to_owned()));
        store.insert_pgp_key_limited(&insert, 64).expect("insert");
        let exported = store.export_pgp_public("acc_a", "pgp_1").expect("export").expect("present");
        assert!(!exported.public_armor.contains("SENTINEL-SECRET-ARMOR"));
    }

    let connection = rusqlite::Connection::open(&path).expect("raw open");
    let hex = store_key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("key");
    let stored: String = connection.query_row("SELECT secret_armor FROM pgp_keys WHERE id = 'pgp_1'", [], |row| row.get(0)).expect("secret row");
    assert_eq!(stored, secret, "the encrypted store must keep the protected secret armor");
}
