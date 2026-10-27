//! P12c1: PGP key store operations through the single registry, on a throwaway SQLCipher store.

mod support;

use std::fs;

use pgp::composed::Deserializable;
use postino_core::{Engine, OpError};
use postino_store::{FolderKind, PgpKeyInsert};
use serde_json::json;
use support::{ACCOUNT_ID, Harness, MemoryVault, seed_account};

const PRIVATE_KEY: &str = include_str!("../../../testdata/pgp/secret-passphrase.asc");
const PUBLIC_KEY: &str = include_str!("../../../testdata/pgp/public.asc");
const UNPROTECTED_KEY: &str = include_str!("../../../testdata/pgp/secret-unprotected.asc");
const PASSPHRASE: &str = "postino-test-passphrase";
const FINGERPRINT: &str = "D529DDA09F683768022096EBA6079AAB69465D6D";

fn fixture() -> (Harness, Engine) {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "INBOX", FolderKind::Inbox)]);
    let engine = Engine::open_with_vault(store, vault).expect("engine");
    (harness, engine)
}

fn import(engine: &mut Engine, armored: &str, passphrase: &str) -> Result<serde_json::Value, OpError> {
    engine.call("pgp_key_import", json!({ "armored_key": armored, "passphrase": passphrase }))
}

fn list(engine: &mut Engine) -> serde_json::Value {
    engine.call("pgp_keys_list", json!({})).expect("pgp_keys_list")
}

fn two_keys_in_one_armor(armored: &str) -> String {
    use base64::Engine;
    let mut lines = armored.lines();
    let begin = lines.next().expect("armor begin");
    let mut body = String::new();
    for line in lines {
        if line.starts_with("-----END") {
            break;
        }
        if line.is_empty() || line.starts_with('=') {
            continue;
        }
        body.push_str(line);
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(body).expect("armor body");
    let mut doubled = decoded.clone();
    doubled.extend_from_slice(&decoded);
    let encoded = base64::engine::general_purpose::STANDARD.encode(doubled);
    let wrapped = encoded.as_bytes().chunks(64).map(|chunk| std::str::from_utf8(chunk).unwrap_or_default()).collect::<Vec<_>>().join("\n");
    format!("{begin}\n\n{wrapped}\n-----END PGP PUBLIC KEY BLOCK-----\n")
}

fn stored_key(n: u32) -> PgpKeyInsert {
    PgpKeyInsert {
        id: format!("pgp_{}", n + 1),
        account_id: ACCOUNT_ID.to_owned(),
        fingerprint: format!("{n:040X}"),
        key_id: "0123456789ABCDEF".to_owned(),
        user_id: None,
        public_armor: format!("-----BEGIN PGP PUBLIC KEY BLOCK-----\n{n}\n-----END PGP PUBLIC KEY BLOCK-----\n"),
        secret_armor: None,
        protected: false,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

#[test]
fn import_list_export_and_delete_round_trip() {
    let (_harness, mut engine) = fixture();
    let imported = import(&mut engine, PRIVATE_KEY, PASSPHRASE).expect("import");
    assert_eq!(imported["fingerprint"], FINGERPRINT);
    assert_eq!(imported["protected"], true);
    assert_eq!(imported["has_secret"], true);
    let id = imported["id"].as_str().expect("id").to_owned();

    let listed = list(&mut engine);
    assert_eq!(listed.as_array().expect("keys").len(), 1);
    let rendered = listed.to_string();
    assert!(!rendered.contains("PRIVATE KEY BLOCK"), "the list leaked secret armor");
    assert!(!rendered.contains(PASSPHRASE), "the list leaked the passphrase");

    let exported = engine.call("pgp_key_export_public", json!({ "id": id })).expect("export");
    assert_eq!(exported["fingerprint"], FINGERPRINT);
    let armor = exported["public_armor"].as_str().expect("armor");
    assert!(armor.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----"));
    assert!(!armor.contains("PRIVATE"));

    let deleted = engine.call("pgp_key_delete", json!({ "id": id })).expect("delete");
    assert_eq!(deleted["fingerprint"], FINGERPRINT);
    assert_eq!(list(&mut engine).as_array().expect("keys").len(), 0);
    assert!(matches!(
        engine.call("pgp_key_delete", json!({ "id": id })).expect_err("second delete"),
        OpError::InvalidArgs(message) if message.contains("unknown PGP key")
    ));
}

#[test]
fn wrong_or_missing_passphrase_leaves_no_row() {
    let (_harness, mut engine) = fixture();
    let wrong = import(&mut engine, PRIVATE_KEY, "not the passphrase").expect_err("wrong passphrase");
    assert!(matches!(wrong, OpError::InvalidArgs(ref message) if message.contains("does not unlock")), "{wrong:?}");
    assert!(!format!("{wrong:?}").contains(PASSPHRASE));
    let missing = import(&mut engine, PRIVATE_KEY, "").expect_err("missing passphrase");
    assert!(matches!(missing, OpError::InvalidArgs(ref message) if message.contains("passphrase-protected")), "{missing:?}");
    assert_eq!(list(&mut engine).as_array().expect("keys").len(), 0);

    import(&mut engine, PRIVATE_KEY, PASSPHRASE).expect("correct passphrase");
    assert_eq!(list(&mut engine).as_array().expect("keys").len(), 1);
}

#[test]
fn a_duplicate_fingerprint_is_refused_and_keeps_the_first_row() {
    let (_harness, mut engine) = fixture();
    let first = import(&mut engine, PRIVATE_KEY, PASSPHRASE).expect("first import");
    let duplicate = import(&mut engine, PUBLIC_KEY, "").expect_err("duplicate fingerprint");
    assert!(matches!(duplicate, OpError::InvalidArgs(ref message) if message.contains("already stored")), "{duplicate:?}");
    assert_eq!(list(&mut engine)[0]["id"], first["id"]);
    assert_eq!(list(&mut engine).as_array().expect("keys").len(), 1);
}

#[test]
fn public_import_works_and_refuses_a_passphrase() {
    let (_harness, mut engine) = fixture();
    let imported = import(&mut engine, PUBLIC_KEY, "").expect("public import");
    assert_eq!(imported["has_secret"], false);
    assert_eq!(imported["protected"], false);
    let refused = import(&mut engine, PUBLIC_KEY, PASSPHRASE).expect_err("passphrase for a public key");
    assert!(matches!(refused, OpError::InvalidArgs(message) if message.contains("public key")));
}

#[test]
fn bounds_refuse_oversize_keys_passphrases_and_malformed_armor() {
    let (_harness, mut engine) = fixture();
    let oversize = format!("{}\n{}", PUBLIC_KEY, "A".repeat(1024 * 1024));
    let error = import(&mut engine, &oversize, "").expect_err("oversize key");
    assert!(matches!(error, OpError::InvalidArgs(message) if message.contains("larger than 1 MiB")));

    let long_passphrase = "p".repeat(257);
    let error = import(&mut engine, PRIVATE_KEY, &long_passphrase).expect_err("oversize passphrase");
    assert!(matches!(error, OpError::InvalidArgs(message) if message.contains("longer than 256 bytes")));

    let error = import(&mut engine, "not a key", "").expect_err("malformed");
    assert!(matches!(error, OpError::InvalidArgs(message) if message.contains("not an armored OpenPGP")));
    assert_eq!(list(&mut engine).as_array().expect("keys").len(), 0);
}

#[test]
fn an_unprotected_private_key_is_refused() {
    let (_harness, mut engine) = fixture();
    let error = import(&mut engine, UNPROTECTED_KEY, "whatever").expect_err("unprotected");
    assert!(matches!(error, OpError::InvalidArgs(ref message) if message.contains("not passphrase-protected")), "{error:?}");
    assert_eq!(list(&mut engine).as_array().expect("keys").len(), 0);
}

#[test]
fn the_database_files_never_contain_the_passphrase_or_secret_armor() {
    let (harness, mut engine) = fixture();
    import(&mut engine, PRIVATE_KEY, PASSPHRASE).expect("import");
    drop(engine);

    let secret_body = PRIVATE_KEY.lines().max_by_key(|line| line.len()).expect("secret body line").to_owned();
    let mut inspected = 0;
    for entry in fs::read_dir(harness.path().parent().expect("store dir")).expect("read dir") {
        let path = entry.expect("entry").path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).expect("read file");
        inspected += 1;
        for needle in [PASSPHRASE.as_bytes(), secret_body.as_bytes(), b"PRIVATE KEY BLOCK"] {
            assert!(!bytes.windows(needle.len()).any(|window| window == needle), "{} contains plaintext key material", path.display());
        }
    }
    assert!(inspected > 0, "the store files were not inspected");
}

#[test]
fn more_than_one_key_or_armor_block_is_refused() {
    let (_harness, mut engine) = fixture();
    let concatenated = format!("{PUBLIC_KEY}\n{PUBLIC_KEY}");
    let error = import(&mut engine, &concatenated, "").expect_err("two armor blocks");
    assert!(matches!(error, OpError::InvalidArgs(ref message) if message.contains("more than one key")), "{error:?}");

    let doubled = two_keys_in_one_armor(PUBLIC_KEY);
    let error = import(&mut engine, &doubled, "").expect_err("two keys in one armor block");
    assert!(matches!(error, OpError::InvalidArgs(ref message) if message.contains("more than one key")), "{error:?}");

    let private_pair = format!("{PRIVATE_KEY}\n{PRIVATE_KEY}");
    let error = import(&mut engine, &private_pair, PASSPHRASE).expect_err("two private armor blocks");
    assert!(matches!(error, OpError::InvalidArgs(ref message) if message.contains("more than one key")), "{error:?}");

    let trailing = format!("{PUBLIC_KEY}\nnot part of the key\n");
    let error = import(&mut engine, &trailing, "").expect_err("trailing text");
    assert!(matches!(error, OpError::InvalidArgs(ref message) if message.contains("single OpenPGP key")), "{error:?}");
    assert_eq!(list(&mut engine).as_array().expect("keys").len(), 0);
}

#[test]
fn the_stored_secret_is_encrypted_and_an_empty_passphrase_does_not_unlock_it() {
    let (harness, mut engine) = fixture();
    import(&mut engine, PRIVATE_KEY, PASSPHRASE).expect("import");
    drop(engine);

    let connection = rusqlite::Connection::open(harness.path()).expect("raw open");
    let hex = harness.store_key().as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("key");
    let stored: String = connection.query_row("SELECT secret_armor FROM pgp_keys LIMIT 1", [], |row| row.get(0)).expect("secret row");
    let (key, _headers) = pgp::composed::SignedSecretKey::from_string(&stored).expect("stored armor parses");
    assert!(key.primary_key.secret_params().is_encrypted(), "the primary secret is not encrypted at rest");
    assert!(key.secret_subkeys.iter().all(|subkey| subkey.key.secret_params().is_encrypted()));
    let empty = pgp::types::Password::from(&b""[..]);
    assert!(!matches!(key.primary_key.unlock(&empty, |_, _| Ok(())), Ok(Ok(()))), "the empty passphrase unlocked the secret");
    for subkey in &key.secret_subkeys {
        assert!(!matches!(subkey.key.unlock(&empty, |_, _| Ok(())), Ok(Ok(()))), "the empty passphrase unlocked a subkey");
    }
}

#[test]
fn the_engine_refuses_import_past_the_64_key_cap() {
    let harness = Harness::new();
    let store = harness.open();
    let vault = MemoryVault::new();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "INBOX", FolderKind::Inbox)]);
    for n in 0..64 {
        store.insert_pgp_key_limited(&stored_key(n), 64).unwrap_or_else(|error| panic!("seed {n}: {error}"));
    }
    let mut engine = Engine::open_with_vault(store, vault).expect("engine");
    let error = import(&mut engine, PUBLIC_KEY, "").expect_err("cap");
    assert!(matches!(error, OpError::InvalidArgs(ref message) if message.contains("64 PGP keys")), "{error:?}");
}

#[test]
fn only_import_is_sensitive_and_methods_are_annotated() {
    let defs = postino_core::op_defs();
    let find = |name: &str| defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("missing {name}"));
    for name in ["pgp_keys_list", "pgp_key_delete", "pgp_key_export_public"] {
        let def = find(name);
        assert!(!def.sensitive, "{name} handles no passphrase");
        assert!(def.input_schema.is_object(), "{name} has no schema");
    }
    let import = find("pgp_key_import");
    assert!(import.sensitive, "import handles a passphrase and must be hidden from MCP");
    assert!(find("pgp_keys_list").read_only);
    assert!(find("pgp_key_export_public").read_only);
    assert!(find("pgp_key_delete").destructive);
    assert!(!import.read_only);
}
