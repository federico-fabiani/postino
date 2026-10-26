use postino_store::{Store, StoreError, StoreKey};

const CANARY: &str = "postino-store-canary-plaintext";

fn random_key() -> StoreKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    StoreKey::from_bytes(bytes)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn file_and_wal_carry_no_plaintext() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    let store = Store::open(&path, &key).expect("open");
    store.set_meta("canary", CANARY).expect("write");
    assert_eq!(store.get_meta("canary").expect("read").as_deref(), Some(CANARY));

    let db = std::fs::read(&path).expect("db file");
    assert_ne!(&db[..16], b"SQLite format 3\0", "the database header is plaintext");
    assert!(!contains(&db, CANARY.as_bytes()), "plaintext leaked into the database pages");

    // The WAL exists while the connection is open; SQLCipher encrypts its frames too.
    if let Ok(wal) = std::fs::read(path.with_extension("db-wal")) {
        assert!(!contains(&wal, CANARY.as_bytes()), "plaintext leaked into the WAL");
    }
}

#[test]
fn a_connection_without_the_key_cannot_read() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    {
        let store = Store::open(&path, &random_key()).expect("open");
        store.set_meta("canary", CANARY).expect("write");
    }

    let plain = rusqlite::Connection::open(&path).expect("sqlite opens the file");
    let row: Result<String, _> = plain.query_row("SELECT value FROM meta WHERE key = 'canary'", [], |row| row.get(0));
    assert!(row.is_err(), "a keyless connection read the encrypted store");
}

#[test]
fn reopens_with_the_same_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    {
        let store = Store::open(&path, &key).expect("open");
        store.set_meta("greeting", "ciao").expect("write");
    }

    let store = Store::open(&path, &key).expect("reopen");
    assert_eq!(store.get_meta("greeting").expect("read").as_deref(), Some("ciao"));
}

#[test]
fn a_wrong_key_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");

    {
        let store = Store::open(&path, &random_key()).expect("open");
        store.set_meta("greeting", "ciao").expect("write");
    }

    let error = Store::open(&path, &random_key()).err().expect("a wrong key must not open the store");
    assert!(matches!(error, StoreError::WrongKeyOrCorrupt), "{error}");
}

#[test]
fn migration_runs_once_and_is_recorded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let key = random_key();

    {
        let store = Store::open(&path, &key).expect("open");
        assert_eq!(store.schema_version().expect("version"), 5);
        store.set_meta("schema", "m1").expect("write");
    }

    let store = Store::open(&path, &key).expect("reopen");
    assert_eq!(store.schema_version().expect("version"), 5, "the migration ran twice");
    assert_eq!(store.get_meta("schema").expect("read").as_deref(), Some("m1"));
    assert_eq!(store.get_meta("missing").expect("read"), None);
}
