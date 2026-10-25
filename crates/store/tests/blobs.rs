//! P4b: the encrypted, content-addressed blob store.
//!
//! Every fixture is synthetic and uses `example.com` / `example.it`. Blobs are written to a
//! throwaway temp directory with a random key; nothing here touches the real keyring or the real
//! Postino data directory.

use std::path::{Path, PathBuf};

use postino_store::{BlobStore, FolderKind, Store, StoreError, StoreKey, blob_root};

const CANARY: &str = "CANARY-BLOB-PLAINTEXT-7f3a";

fn random_key() -> StoreKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    StoreKey::from_bytes(bytes)
}

/// The blob store a database at `<dir>/mail.db` uses: `<dir>/mail.db.blobs`.
fn blobs_in(dir: &Path, key: &StoreKey) -> BlobStore {
    BlobStore::open(blob_root(&dir.join("mail.db")), key).expect("open the blob store")
}

fn blob_path(store: &BlobStore, hash: &str) -> PathBuf {
    let shard = hash.get(..2).expect("a validated hash has two characters");
    store.root().join(shard).join(format!("{hash}.blob"))
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn temp_files(dir: &Path) -> Vec<PathBuf> {
    files_under(dir).into_iter().filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with(".tmp-"))).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

fn compressible() -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in 0..2000 {
        bytes.extend_from_slice(format!("Riga {line}: il preventivo per Studio Verdi è 4.800 euro + IVA.\n").as_bytes());
    }
    bytes
}

fn incompressible(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes
}

#[test]
fn bytes_round_trip_and_survive_a_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = random_key();
    let content = compressible();
    let hash = blobs_in(dir.path(), &key).put(&content).expect("put");
    assert_eq!(hash.len(), 64, "a BLAKE3 hex hash is 64 characters");
    assert!(hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)), "the hash is not lowercase hex: {hash}");

    // A new handle, exactly like the next process: same root, same key.
    let reopened = blobs_in(dir.path(), &key);
    assert!(reopened.has(&hash).expect("has"));
    let read = reopened.get(&hash, content.len() as u64).expect("get");
    assert_eq!(read.as_slice(), content.as_slice());
}

#[test]
fn identical_bytes_are_stored_once() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let content = b"lo stesso allegato, dieci volte";
    let first = store.put(content).expect("first put");
    let second = store.put(content).expect("second put");
    assert_eq!(first, second, "identical bytes produced two hashes");
    let third = store.put(b"other bytes").expect("third put");
    assert_ne!(first, third);

    let files = files_under(store.root());
    println!("dedup: hash={first} files={}", files.len());
    for file in &files {
        println!("  {}", file.display());
    }
    assert_eq!(files.len(), 2, "two distinct contents must leave exactly two blob files");
    assert!(files.contains(&blob_path(&store, &first)));
    assert!(files.contains(&blob_path(&store, &third)));
    assert!(temp_files(store.root()).is_empty(), "a put left a temp file behind");
}

#[test]
fn a_corrupt_file_is_replaced_by_the_next_put_of_the_same_bytes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let content = compressible();
    let hash = store.put(&content).expect("put");

    let path = blob_path(&store, &hash);
    let mut bytes = std::fs::read(&path).expect("read the blob");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).expect("corrupt the tag");
    assert!(matches!(store.get(&hash, content.len() as u64).err(), Some(StoreError::CorruptBlob { .. })), "the fixture is not corrupt");

    // Dedup must authenticate before it reports success and must heal a damaged file.
    assert_eq!(store.put(&content).expect("put over a corrupt file"), hash);
    assert_eq!(store.get(&hash, content.len() as u64).expect("read the repaired blob").as_slice(), content.as_slice());
    assert!(temp_files(store.root()).is_empty(), "the repair left a temp file behind");
}

#[test]
fn a_foreign_key_cannot_dedup_bytes_it_cannot_read() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = blobs_in(dir.path(), &random_key());
    let content = b"scritto con la chiave del primo store";
    let hash = first.put(content).expect("first put");

    let second = blobs_in(dir.path(), &random_key());
    assert!(second.put(content).is_ok(), "the second store must be able to write its own bytes");
    assert_eq!(second.get(&hash, 1024).expect("the second key reads its own bytes").as_slice(), content);
    assert!(matches!(first.get(&hash, 1024).err(), Some(StoreError::CorruptBlob { .. })), "the foreign key's file was not replaced");
}

#[test]
fn content_planted_under_a_false_name_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = random_key();
    let store = blobs_in(dir.path(), &key);
    let subkey = postino_crypto::derive_key(key.as_bytes(), "blob").expect("derived blob subkey");

    // A writer who has the subkey can produce a perfectly valid file. Here the name belongs to
    // "innocent bytes" while the sealed payload is something else: the AEAD authenticates, and only
    // recomputing BLAKE3 over the plaintext catches it.
    let innocent = b"the bytes the name promises";
    let planted = b"something else entirely";
    let name = blake3::hash(innocent).to_hex().to_string();
    let sealed = seal_with(&subkey, &name, planted);
    let path = blob_path(&store, &name);
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard directory");
    std::fs::write(&path, &sealed).expect("plant the file");

    let error = store.get(&name, 1024).expect_err("a false name must not be served");
    assert!(matches!(error, StoreError::CorruptBlob { .. }), "{error}");
}

/// Build a version-1 blob file for `planted` with AAD and name `hash`, using the real subkey.
/// The format is written here independently of the crate, so a drift in either side fails the test.
fn seal_with(subkey: &postino_crypto::Subkey, hash: &str, planted: &[u8]) -> Vec<u8> {
    let mut hash_bytes = [0u8; 32];
    let raw = hash.as_bytes();
    for (slot, pair) in hash_bytes.iter_mut().zip(raw.as_chunks::<2>().0) {
        let high = (pair[0] as char).to_digit(16).expect("validated hex") as u8;
        let low = (pair[1] as char).to_digit(16).expect("validated hex") as u8;
        *slot = (high << 4) | low;
    }
    let mut aad = Vec::new();
    aad.extend_from_slice(b"PSTB");
    aad.push(1);
    aad.push(0);
    aad.extend_from_slice(&(planted.len() as u64).to_le_bytes());
    aad.extend_from_slice(&hash_bytes);
    let sealed = postino_crypto::seal(subkey, &aad, planted).expect("seal");
    let mut file = Vec::with_capacity(38 + sealed.len());
    file.extend_from_slice(b"PSTB");
    file.push(1);
    file.push(0);
    file.extend_from_slice(&(planted.len() as u64).to_le_bytes());
    file.extend_from_slice(&sealed);
    file
}

#[test]
fn a_flag_or_in_cap_length_flip_breaks_the_aad() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let text = compressible();
    let text_hash = store.put(&text).expect("put text");
    let text_path = blob_path(&store, &text_hash);
    let text_file = std::fs::read(&text_path).expect("read");
    assert_eq!(text_file[5], 1, "the fixture must be compressed");

    let random = incompressible(2048);
    let random_hash = store.put(&random).expect("put random");
    let random_path = blob_path(&store, &random_hash);
    let random_file = std::fs::read(&random_path).expect("read");
    assert_eq!(random_file[5], 0, "the fixture must be uncompressed");

    // Compressed payload with the bit cleared: the length still matches, so only the AAD flags bind it.
    let mut cleared = text_file.clone();
    cleared[5] = 0;
    std::fs::write(&text_path, &cleared).expect("clear the flag");
    assert_auth_failure(store.get(&text_hash, text.len() as u64).expect_err("a cleared flag was accepted"));

    // Uncompressed payload with the bit set.
    let mut set = random_file.clone();
    set[5] = 1;
    std::fs::write(&random_path, &set).expect("set the flag");
    assert_auth_failure(store.get(&random_hash, random.len() as u64).expect_err("a set flag was accepted"));

    // A length inside the caller's cap still has to match the AAD and the payload. The longer value
    // reaches decryption (the file is not longer than its declared length), so authentication is the
    // check that must reject it.
    for delta in [64u64, 1024] {
        let mut longer = text_file.clone();
        let declared = (text.len() as u64).saturating_add(delta);
        longer[6..14].copy_from_slice(&declared.to_le_bytes());
        std::fs::write(&text_path, &longer).expect("extend the declared length");
        let error = store.get(&text_hash, declared).expect_err("a different length must not authenticate");
        assert_auth_failure(error);
    }
    let mut shorter = text_file.clone();
    shorter[6..14].copy_from_slice(&(text.len() as u64 / 2).to_le_bytes());
    std::fs::write(&text_path, &shorter).expect("shrink the declared length");
    assert!(matches!(store.get(&text_hash, text.len() as u64).err(), Some(StoreError::CorruptBlob { .. })), "a shorter declared length was accepted");

    std::fs::write(&text_path, &text_file).expect("restore");
    assert_eq!(store.get(&text_hash, text.len() as u64).expect("restored").as_slice(), text.as_slice());
}

/// The AAD must be what rejects a flipped flag or an in-cap length, not a downstream length or hash
/// check: if the field leaves the AAD, the detail changes even when the read still fails.
fn assert_auth_failure(error: StoreError) {
    match &error {
        StoreError::CorruptBlob { detail, .. } if detail.starts_with("the ciphertext did not authenticate") => {}
        other => panic!("expected an authentication failure, got {other}"),
    }
}

#[test]
fn a_wrong_key_cannot_read_the_bytes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let content = b"allegato cifrato con la chiave del primo store";
    let hash = blobs_in(dir.path(), &random_key()).put(content).expect("put");

    let other = blobs_in(dir.path(), &random_key());
    let error = other.get(&hash, 1 << 20).expect_err("another key must not decrypt the blob");
    assert!(matches!(error, StoreError::CorruptBlob { .. }), "{error}");
    assert!(!error.to_string().contains(CANARY));
}

#[test]
fn a_tampered_byte_or_a_truncated_file_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let content = compressible();
    let hash = store.put(&content).expect("put");
    let path = blob_path(&store, &hash);
    let original = std::fs::read(&path).expect("read the blob");

    // The tag, the ciphertext and the nonce are all authenticated.
    for position in [original.len() - 1, original.len() / 2, 14] {
        let mut tampered = original.clone();
        tampered[position] ^= 0x01;
        std::fs::write(&path, &tampered).expect("tamper");
        let error = store.get(&hash, content.len() as u64).expect_err("a modified byte must not authenticate");
        assert!(matches!(error, StoreError::CorruptBlob { .. }), "byte {position}: {error}");
    }

    // A truncated header is a corrupt blob, not a short read that returns empty bytes.
    std::fs::write(&path, &original[..20]).expect("truncate");
    assert!(matches!(store.get(&hash, content.len() as u64).err(), Some(StoreError::CorruptBlob { .. })));

    std::fs::write(&path, &original).expect("restore");
    assert_eq!(store.get(&hash, content.len() as u64).expect("restored").as_slice(), content.as_slice());
}

#[test]
fn a_blob_swapped_under_another_hash_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let first = b"primo contenuto";
    let second = b"secondo contenuto";
    let first_hash = store.put(first).expect("first");
    let second_hash = store.put(second).expect("second");
    assert_ne!(first_hash, second_hash);

    let first_path = blob_path(&store, &first_hash);
    let second_path = blob_path(&store, &second_hash);
    std::fs::copy(&first_path, &second_path).expect("swap the files");

    let error = store.get(&second_hash, 1 << 20).expect_err("a swapped blob must not authenticate");
    assert!(matches!(error, StoreError::CorruptBlob { .. }), "{error}");
}

#[test]
fn a_declared_length_over_the_cap_is_refused_before_decryption() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let hash = store.put(b"un allegato minuscolo che dichiara dieci gigabyte").expect("put");
    let path = blob_path(&store, &hash);
    let mut bytes = std::fs::read(&path).expect("read");
    let declared = 10 * 1024 * 1024 * 1024u64;
    bytes[6..14].copy_from_slice(&declared.to_le_bytes());
    std::fs::write(&path, &bytes).expect("patch the header");

    let error = store.get(&hash, 1024 * 1024).expect_err("10 GiB must be refused");
    match error {
        StoreError::BlobTooLarge { declared: found, allowed } => {
            assert_eq!(found, declared);
            assert_eq!(allowed, 1024 * 1024);
        }
        other => panic!("expected BlobTooLarge, got {other}"),
    }

    // The hard ceiling applies even when the caller passes a bigger cap.
    let error = store.get(&hash, u64::MAX).expect_err("the hard ceiling must be enforced");
    assert!(matches!(error, StoreError::BlobTooLarge { .. }), "{error}");
}

#[test]
fn hashes_are_validated_before_any_path_is_built() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let outside = dir.path().join("escaped.blob");
    let bad = [
        String::new(),
        "..".to_owned(),
        "../../../etc/passwd".to_owned(),
        "..\\..\\windows\\system32".to_owned(),
        format!("..{}", "0".repeat(62)),
        format!("{}/{}", "0".repeat(31), "0".repeat(32)),
        format!("{}\\{}", "0".repeat(31), "0".repeat(32)),
        "A".repeat(64),
        "g".repeat(64),
        "0".repeat(63),
        "0".repeat(65),
        // Windows path shapes that must never become a path component.
        r"C:\Windows\System32\config\SAM".to_owned(),
        r"\\?\C:\Windows\System32\config\SAM".to_owned(),
        r"\\server\share\mail.db.blobs\00\blob".to_owned(),
        "name:stream".to_owned(),
        "trailing-dot.".to_owned(),
        "trailing-space ".to_owned(),
        "PROGRA~1.DB".to_owned(),
        format!("{}.", "0".repeat(63)),
        format!("{} ", "0".repeat(63)),
        r"C:\db.blobs\00\blob".to_owned(),
    ];
    for hash in &bad {
        assert!(matches!(store.get(hash, 1024).err(), Some(StoreError::BadHash { .. })), "get accepted {hash:?}");
        assert!(matches!(store.has(hash).err(), Some(StoreError::BadHash { .. })), "has accepted {hash:?}");
        assert!(matches!(store.remove(hash).err(), Some(StoreError::BadHash { .. })), "remove accepted {hash:?}");
    }
    // A rejected id must not come back in the error text: an agent reading it could learn a path.
    let error = store.get(r"C:\Windows\System32\config\SAM", 1024).expect_err("a Windows path is not a hash");
    let rendered = error.to_string();
    assert!(!rendered.contains("Windows") && !rendered.contains(r"C:\"), "the error echoed the rejected input: {rendered}");
    assert!(!outside.exists(), "a rejected hash created a file outside the blob root");
    assert!(files_under(store.root()).is_empty(), "a rejected hash wrote inside the blob root");
}

#[test]
fn a_failed_write_removes_its_temp_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let content = b"scrittura che fallisce";
    let hash = store.put(content).expect("put");
    let path = blob_path(&store, &hash);
    std::fs::remove_file(&path).expect("remove the blob");
    std::fs::create_dir(&path).expect("a directory in the blob's place makes the rename fail");

    let error = store.put(content).expect_err("a rename over a directory must fail");
    assert!(matches!(error, StoreError::Io { .. }), "{error}");
    assert!(temp_files(store.root()).is_empty(), "the failed write left a temp file behind");
    assert!(path.is_dir(), "the fixture directory disappeared");

    std::fs::remove_dir(&path).expect("remove the fixture directory");
    assert_eq!(store.put(content).expect("a clean retry succeeds"), hash);
    assert!(temp_files(store.root()).is_empty(), "the retry left a temp file behind");
}

#[test]
fn concurrent_puts_of_the_same_content_succeed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let content = std::sync::Arc::new(compressible());

    let hashes = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = &store;
            let content = std::sync::Arc::clone(&content);
            handles.push(scope.spawn(move || {
                let hash = store.put(&content[..]).expect("concurrent put");
                let read = store.get(&hash, content.len() as u64 + 1024).expect("concurrent get");
                assert_eq!(read.len(), content.len(), "a concurrent reader saw a partial blob");
                hash
            }));
        }
        handles.into_iter().map(|handle| handle.join().expect("a writer thread panicked")).collect::<Vec<_>>()
    });

    let first = hashes.first().cloned().expect("eight hashes");
    assert!(hashes.iter().all(|hash| hash == &first), "the same bytes produced different hashes");
    assert_eq!(files_under(store.root()).len(), 1, "the same bytes left more than one file");
    assert!(temp_files(store.root()).is_empty(), "a concurrent put left a temp file behind");
}

#[test]
fn compression_is_used_only_when_it_helps() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());

    let text = compressible();
    let text_hash = store.put(&text).expect("put text");
    let text_file = std::fs::read(blob_path(&store, &text_hash)).expect("read the text blob");
    assert_eq!(text_file[5], 1, "compressible text was not compressed");
    assert!(text_file.len() < text.len(), "the compressed blob is not smaller ({} vs {})", text_file.len(), text.len());

    let random = incompressible(4096);
    let random_hash = store.put(&random).expect("put random");
    let random_file = std::fs::read(blob_path(&store, &random_hash)).expect("read the random blob");
    assert_eq!(random_file[5], 0, "incompressible bytes were compressed anyway");
    assert_eq!(random_file.len(), 38 + random.len() + 16, "an uncompressed blob is header + plaintext + tag");

    assert_eq!(store.get(&text_hash, text.len() as u64).expect("read text").as_slice(), text.as_slice());
    assert_eq!(store.get(&random_hash, random.len() as u64).expect("read random").as_slice(), random.as_slice());
}

#[test]
fn the_header_carries_magic_version_flags_length_and_nonce() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let content = b"intestazione da ispezionare";
    let hash = store.put(content).expect("put");
    let bytes = std::fs::read(blob_path(&store, &hash)).expect("read the blob");

    assert_eq!(&bytes[..4], b"PSTB", "the magic is wrong");
    assert_eq!(bytes[4], 1, "the version is wrong");
    assert_eq!(bytes[5] & !1, 0, "unknown flag bits");
    assert_eq!(u64::from_le_bytes(bytes[6..14].try_into().expect("eight length bytes")), content.len() as u64);
    assert!(bytes[14..38].iter().any(|byte| *byte != 0), "the nonce is all zeroes");

    let mut dump = String::new();
    for (index, byte) in bytes.iter().take(38).enumerate() {
        if index % 16 == 0 {
            dump.push_str(&format!("\n{index:04x}  "));
        }
        dump.push_str(&format!("{byte:02x} "));
    }
    println!("header hexdump (38 bytes, ciphertext starts at 0x0026):{dump}");
    assert_eq!(bytes.len(), 38 + content.len() + 16);
}

#[test]
fn remove_deletes_the_file_and_reports_a_missing_blob() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let hash = store.put(b"da cancellare").expect("put");
    assert!(store.remove(&hash).expect("remove"));
    assert!(!store.has(&hash).expect("has"));
    assert!(!store.remove(&hash).expect("removing twice"));
    assert!(matches!(store.get(&hash, 1024).err(), Some(StoreError::BlobMissing { .. })));
    assert!(files_under(store.root()).is_empty(), "the shard directory was not cleaned up");
}

#[test]
fn the_blob_and_the_database_carry_no_plaintext() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = random_key();
    let content = format!("{CANARY}-{}", "ripetuto ".repeat(400)).into_bytes();

    let store = Store::open(&dir.path().join("mail.db"), &key).expect("open the store");
    // The blob is compressed (repetitive), which must still not leak the canary anywhere.
    let hash = store.blobs().put(&content).expect("put");
    assert!(store.blobs().has(&hash).expect("has"));
    // An uncompressed blob too: random bytes cannot be compressed, so they sit in the file as
    // ciphertext only.
    let random = incompressible(2048);
    let random_hash = store.blobs().put(&random).expect("put random");
    assert_eq!(std::fs::read(blob_path(store.blobs(), &random_hash)).expect("read")[5], 0);
    store.set_meta("canary", CANARY).expect("write a database canary");

    // The WAL and SHM exist only while a connection has the database open: scan before dropping it,
    // or the test would prove nothing about them.
    let files = files_under(dir.path());
    let names = files.iter().map(|path| path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default()).collect::<Vec<_>>();
    assert!(names.iter().any(|name| name.ends_with("-wal")), "the WAL was not present while the store was open: {names:?}");
    assert!(names.iter().any(|name| name.ends_with("-shm")), "the SHM was not present while the store was open: {names:?}");
    assert!(files.len() >= 4, "expected the database, WAL, SHM and two blobs, found {}", files.len());
    for path in &files {
        let bytes = std::fs::read(path).expect("read a store file");
        assert!(!contains(&bytes, CANARY.as_bytes()), "plaintext leaked into {}", path.display());
        assert!(!contains(&bytes, key.as_bytes()), "the store key leaked into {}", path.display());
        assert!(!path.to_string_lossy().contains(CANARY), "a plaintext canary appears in a path");
    }

    // The bytes are still readable through the API: encryption, not loss.
    drop(store);
    let store = Store::open(&dir.path().join("mail.db"), &key).expect("reopen");
    let hash = store.blobs().put(&content).expect("dedup after reopen");
    assert_eq!(store.blobs().get(&hash, content.len() as u64).expect("get").as_slice(), content.as_slice());
}

#[test]
fn store_open_creates_the_blob_directory_next_to_the_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &random_key()).expect("open");
    assert_eq!(store.blobs().root(), dir.path().join("mail.db.blobs"));
    assert!(store.blobs().root().is_dir());
}

#[test]
fn store_open_fails_when_the_blob_path_is_a_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("mail.db.blobs"), b"not a directory").expect("a file in the blob directory's place");
    let error = match Store::open(&dir.path().join("mail.db"), &random_key()) {
        Ok(_) => panic!("a file at the blob root must fail the open"),
        Err(error) => error,
    };
    assert!(matches!(error, StoreError::BlobRoot { .. }), "{error}");
}

#[test]
fn the_blob_root_is_derived_from_the_database_file() {
    assert_eq!(blob_root(Path::new("mail.db")), Path::new("mail.db.blobs"));
    assert_eq!(blob_root(Path::new("/postino/mail.db")), Path::new("/postino/mail.db.blobs"));
    assert_eq!(blob_root(Path::new("/postino/archive")), Path::new("/postino/archive.blobs"));
}

#[test]
fn two_databases_in_one_folder_never_share_a_blob_namespace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = random_key();
    let first = Store::open(&dir.path().join("first.db"), &key).expect("first store");
    let second = Store::open(&dir.path().join("second.db"), &key).expect("second store");
    assert_eq!(first.blobs().root(), dir.path().join("first.db.blobs"));
    assert_eq!(second.blobs().root(), dir.path().join("second.db.blobs"));
    assert_ne!(first.blobs().root(), second.blobs().root());

    let hash = first.blobs().put(b"only the first store's bytes").expect("put");
    assert!(first.blobs().has(&hash).expect("has"));
    assert!(!second.blobs().has(&hash).expect("has"), "the second store can see the first store's namespace");
}

// ---------------------------------------------------------------------------------------------
// The repository half: attachment rows point at blobs.
// ---------------------------------------------------------------------------------------------

fn attachment(id: &str, size: u64) -> postino_store::Attachment {
    postino_store::Attachment { id: id.to_owned(), filename: format!("{id}.pdf"), mime: "application/pdf".to_owned(), size, blob_hash: None }
}

fn seed_message(store: &Store) {
    store
        .upsert_account(&postino_store::Account {
            id: "acc_1".to_owned(),
            name: "Studio Verdi".to_owned(),
            email: "studio@example.com".to_owned(),
            provider: "demo".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    store
        .upsert_folder(&postino_store::Folder {
            id: "inbox".to_owned(),
            account_id: "acc_1".to_owned(),
            name: "Posta in arrivo".to_owned(),
            kind: FolderKind::Inbox,
            unread: 0,
            total: 0,
            uid_validity: None,
            uid_next: None,
            last_sync_at: None,
        })
        .expect("folder");
    store
        .upsert_message(&postino_store::Message {
            summary: postino_store::MessageSummary {
                id: "msg_1".to_owned(),
                account_id: "acc_1".to_owned(),
                folder_id: "inbox".to_owned(),
                thread_id: "thr_1".to_owned(),
                from: postino_store::Address::new("Giulia Ferri", "giulia.ferri@example.com"),
                to: vec![postino_store::Address::new("Federico", "federico@example.it")],
                subject: "Preventivo".to_owned(),
                snippet: "Il preventivo".to_owned(),
                date: 1_700_000_000_000,
                unread: false,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            uid: None,
            message_id: None,
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: "Corpo".to_owned(),
            body_html: None,
            size: None,
        })
        .expect("message");
    store.replace_attachments("msg_1", &[attachment("att_1", 10), attachment("att_2", 20)]).expect("attachments");
}

#[test]
fn an_attachment_row_records_its_hash_and_size() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &random_key()).expect("open");
    seed_message(&store);
    let content = b"il contenuto dell'allegato";
    let hash = store.blobs().put(content).expect("put");
    store.set_attachment_blob_hash("msg_1", "att_1", Some(&hash), content.len() as u64).expect("set the hash");

    let (owner, row) = store.find_attachment("att_1").expect("find").expect("row");
    assert_eq!(owner, "msg_1");
    assert_eq!(row.blob_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(row.size, content.len() as u64, "the row must record the stored size, not the MIME-declared one");
    assert!(store.find_attachment("att_missing").expect("find").is_none());
}

#[test]
fn a_hash_cannot_be_written_through_another_message() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &random_key()).expect("open");
    seed_message(&store);
    let zeroes = "0".repeat(64);
    let error = store.set_attachment_blob_hash("msg_other", "att_1", Some(&zeroes), 1).expect_err("an attachment is owned by one message");
    assert!(matches!(error, StoreError::AttachmentOwnedByOther { .. }), "{error}");
    let error = store.set_attachment_blob_hash("msg_1", "att_missing", None, 0).expect_err("an unknown attachment");
    assert!(matches!(error, StoreError::NotFound { .. }), "{error}");
}

#[test]
fn replacing_attachments_keeps_the_stored_hash_and_size_of_a_kept_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("mail.db"), &random_key()).expect("open");
    seed_message(&store);
    let content = b"allegato uno";
    let hash = store.blobs().put(content).expect("put");
    store.set_attachment_blob_hash("msg_1", "att_1", Some(&hash), content.len() as u64).expect("set");

    // A draft re-save re-sends the attachment metadata with no hash and a stale size of zero.
    store.replace_attachments("msg_1", &[attachment("att_1", 0)]).expect("replace");
    let rows = store.attachments("msg_1").expect("attachments");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "att_1");
    assert_eq!(rows[0].blob_hash.as_deref(), Some(hash.as_str()), "a metadata-only replace detached the stored bytes");
    assert_eq!(rows[0].size, content.len() as u64, "a stale caller size shrank the row below the stored blob");
    assert_eq!(store.blobs().get(&hash, rows[0].size + 1024).expect("the stored bytes stay readable").as_slice(), content);

    // Clearing is explicit.
    store.set_attachment_blob_hash("msg_1", "att_1", None, 0).expect("clear");
    assert_eq!(store.attachments("msg_1").expect("attachments")[0].blob_hash, None);
    assert!(store.blobs().has(&hash).expect("has"), "clearing the pointer deleted the deduplicated blob");
}

/// The directory fsync after the publish has no observable effect a test can assert without simulating
/// power loss. This exercises the Unix path end to end: a failure in `File::open(dir).sync_all()`
/// would surface as an error from `put`. On Windows the published-file flush runs on every publish
/// and is covered by the rest of this file.
#[cfg(unix)]
#[test]
fn a_put_syncs_the_shard_directory_after_the_rename() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    let content = b"durable bytes on a unix filesystem";
    let hash = store.put(content).expect("put with a shard-directory fsync");
    assert_eq!(store.get(&hash, 1024).expect("get").as_slice(), content);
}

/// Eight writers race to publish the same fresh content, round after round, while every writer also
/// reads it back immediately. A replacing publish on Windows can unlink the old name a scheduling
/// quantum before the new one appears; this failed with `BlobMissing` before the publish moved to
/// `hard_link` (and the read gained its bounded retry). The round count is a compromise: enough
/// interleavings to hit the window, small enough for the default suite.
#[test]
fn concurrent_publish_and_read_never_hide_the_blob() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = blobs_in(dir.path(), &random_key());
    for round in 0..100u32 {
        let content = std::sync::Arc::new(round_content(4096 + round as usize));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let store = &store;
                let content = std::sync::Arc::clone(&content);
                scope.spawn(move || {
                    let hash = store.put(&content[..]).expect("concurrent put");
                    let read = store.get(&hash, content.len() as u64 + 1024).expect("concurrent get saw no blob");
                    assert_eq!(read.len(), content.len());
                });
            }
        });
    }
}

fn round_content(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}
