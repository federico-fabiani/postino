//! P12c2b: PGP/MIME verify/decrypt, sign/encrypt sends and the registry contract.
//!
//! Keys are generated in-process (RSA 2048, passphrase-protected) so no fixture secret can be
//! mistaken for a real one. Nothing here opens a socket.

mod support;

use pgp::composed::{
    ArmorOptions, Deserializable, DetachedSignature, EncryptionCaps, KeyType, Message, MessageBuilder, SecretKeyParamsBuilder, SignedPublicKey,
    SignedSecretKey, SubkeyParamsBuilder,
};
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::types::{KeyDetails, Password};
use postino_core::{Engine, PgpDecryptState, PgpSendOptions, PgpVerifyState};
use postino_store::{FolderKind, Store};
use rand::rngs::OsRng;
use serde_json::json;
use support::{ACCOUNT_ID, Harness, MemoryVault, seed_account, seed_account_with_smtp};

const PASSPHRASE: &str = "postino-test-pgp-passphrase";
const BODY: &str = "Corpo firmato\r\nseconda riga\r\n";
const PLAINTEXT_MARKER: &str = "SEGRETO-DECIFRATO-XYZ";

struct Keypair {
    secret_armor: String,
    public_armor: String,
    fingerprint: String,
}

fn keypair() -> Keypair {
    let key = SecretKeyParamsBuilder::default()
        .key_type(KeyType::Rsa(2048))
        .can_certify(true)
        .can_sign(true)
        .can_encrypt(EncryptionCaps::All)
        .primary_user_id("Postino PGP Test <pgp@example.com>".into())
        .passphrase(Some(PASSPHRASE.to_owned()))
        .build()
        .expect("key params")
        .generate(OsRng)
        .expect("generate a test key");
    Keypair {
        secret_armor: key.to_armored_string(ArmorOptions::default()).expect("secret armor"),
        public_armor: key.to_public_key().to_armored_string(ArmorOptions::default()).expect("public armor"),
        fingerprint: key.fingerprint().to_string().to_uppercase(),
    }
}

/// A certificate whose primary key signs/certifies and whose subkey carries encryption or signing.
fn keypair_with_subkey(signing_subkey: bool) -> Keypair {
    let mut subkey = SubkeyParamsBuilder::default();
    subkey.key_type(KeyType::Rsa(2048)).passphrase(Some(PASSPHRASE.to_owned()));
    if signing_subkey {
        subkey.can_sign(true);
    } else {
        subkey.can_encrypt(EncryptionCaps::All);
    }
    let key = SecretKeyParamsBuilder::default()
        .key_type(KeyType::Rsa(2048))
        .can_certify(true)
        .can_sign(true)
        .primary_user_id("Postino PGP Subkey <pgp-subkey@example.com>".into())
        .passphrase(Some(PASSPHRASE.to_owned()))
        .subkeys(vec![subkey.build().expect("subkey params")])
        .build()
        .expect("key params")
        .generate(OsRng)
        .expect("generate a test key");
    Keypair {
        secret_armor: key.to_armored_string(ArmorOptions::default()).expect("secret armor"),
        public_armor: key.to_public_key().to_armored_string(ArmorOptions::default()).expect("public armor"),
        fingerprint: key.fingerprint().to_string().to_uppercase(),
    }
}

fn entity(body: &str) -> Vec<u8> {
    format!("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n\r\n{body}").into_bytes()
}

fn headers(subject: &str) -> String {
    format!(
        "From: A <a@example.com>\r\nTo: B <b@example.com>\r\nSubject: {subject}\r\nDate: Tue, 27 Oct 2026 10:00:00 +0000\r\nMessage-ID: <{subject}@example.com>\r\nMIME-Version: 1.0\r\n"
    )
}

fn sign(entity: &[u8], secret_armor: &str) -> String {
    let (key, _) = SignedSecretKey::from_string(secret_armor).expect("secret key");
    let password = Password::from(PASSPHRASE);
    assert!(matches!(key.primary_key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))), "the test key must unlock");
    let signed = entity.strip_suffix(b"\r\n").unwrap_or(entity);
    DetachedSignature::sign_binary_data(OsRng, &key.primary_key, &password, HashAlgorithm::Sha256, signed)
        .expect("sign")
        .to_armored_string(ArmorOptions::default())
        .expect("signature armor")
}
fn encrypt(entity: &[u8], public_armor: &str) -> String {
    let (key, _) = SignedPublicKey::from_string(public_armor).expect("public key");
    let mut builder = MessageBuilder::from_bytes("message", entity.to_vec()).seipd_v1(OsRng, SymmetricKeyAlgorithm::AES256);
    builder.encrypt_to_key(OsRng, &key).expect("encrypt to the test key");
    builder.to_armored_string(OsRng, ArmorOptions::default()).expect("payload armor")
}

fn signed_raw(message_headers: &str, entity: &[u8], secret_armor: &str) -> Vec<u8> {
    let signed = entity.strip_suffix(b"\r\n").unwrap_or(entity);
    let signature = sign(entity, secret_armor);
    let part =
        postino_sync::mime::pgp_mime::serialize_signed(signed, &signature, postino_sync::mime::pgp_mime::MICALG_SHA256).expect("signed entity");
    let mut raw = message_headers.as_bytes().to_vec();
    raw.extend_from_slice(&part);
    raw
}

fn signed_raw_with_signature(message_headers: &str, entity: &[u8], signature_armor: &str) -> Vec<u8> {
    let part =
        postino_sync::mime::pgp_mime::serialize_signed(entity, signature_armor, postino_sync::mime::pgp_mime::MICALG_SHA256).expect("signed entity");
    let mut raw = message_headers.as_bytes().to_vec();
    raw.extend_from_slice(&part);
    raw
}

fn encrypted_raw(message_headers: &str, entity: &[u8], public_armor: &str) -> Vec<u8> {
    let payload = encrypt(entity, public_armor);
    let part = postino_sync::mime::pgp_mime::serialize_encrypted(&payload).expect("encrypted entity");
    let mut raw = message_headers.as_bytes().to_vec();
    raw.extend_from_slice(&part);
    raw
}

fn message_row(id: &str) -> postino_store::Message {
    postino_store::Message {
        summary: postino_store::MessageSummary {
            id: id.to_owned(),
            account_id: ACCOUNT_ID.to_owned(),
            folder_id: format!("fld_{ACCOUNT_ID}_inbox"),
            thread_id: format!("thread_{id}"),
            from: postino_store::Address::new("Giulia", "giulia@example.com"),
            to: vec![postino_store::Address::new("Me", "me@example.com")],
            subject: "PGP test".to_owned(),
            snippet: "original".to_owned(),
            date: 1_700_000_100_000,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: "Original visible body".to_owned(),
        body_html: None,
        remote_images: 0,
        size: None,
    }
}

fn store_raw(store: &Store, id: &str, raw: &[u8]) {
    store.upsert_message(&message_row(id)).expect("message row");
    let hash = store.blobs().put(raw).expect("raw blob");
    store.set_message_raw_blob_hash(id, Some(&hash)).expect("raw hash");
}

fn received_fixture(raw: &[u8], key: Option<&Keypair>) -> (Harness, Engine) {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "INBOX", FolderKind::Inbox)]);
    store_raw(&store, "msg_pgp", raw);
    let mut engine = Engine::open_with_vault(store, vault).expect("engine");
    if let Some(key) = key {
        engine.call("pgp_key_import", json!({ "armored_key": key.secret_armor, "passphrase": PASSPHRASE })).expect("import the private key");
    }
    (harness, engine)
}

fn send_fixture() -> (Harness, Engine, Keypair) {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account_with_smtp(&store, &vault, ACCOUNT_ID, 1, 1, &[("drafts", "Bozze", FolderKind::Drafts), ("sent", "Inviati", FolderKind::Sent)]);
    let key = keypair();
    let mut engine = Engine::open_with_vault(store, vault).expect("engine");
    engine.call("pgp_key_import", json!({ "armored_key": key.secret_armor, "passphrase": PASSPHRASE })).expect("import the private key");
    (harness, engine, key)
}

fn new_draft(engine: &mut Engine, body: &str) -> String {
    let saved = engine
        .call("draft_save", json!({ "account": ACCOUNT_ID, "to": [{ "email": "giulia@example.com" }], "subject": "PGP send", "body_text": body }))
        .expect("draft_save");
    saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned()
}

fn key_id(engine: &mut Engine, fingerprint: &str) -> String {
    let keys = engine.call("pgp_keys_list", json!({})).expect("keys");
    keys.as_array()
        .and_then(|keys| keys.iter().find(|key| key["fingerprint"] == fingerprint))
        .and_then(|key| key["id"].as_str())
        .expect("stored key id")
        .to_owned()
}

#[test]
fn exact_bytes_verify_and_one_byte_tamper() {
    let key = keypair();
    let body = BODY;
    let raw = signed_raw(&headers("signed"), &entity(body), &key.secret_armor);
    assert!(raw.windows(2).any(|window| window == b"\r\n"));
    assert!(!raw.windows(2).any(|window| window == b"\n\n"), "the raw bytes must stay CRLF");

    let (harness, mut engine) = received_fixture(&raw, Some(&key));
    let verified = engine.call("message_pgp_verify", json!({ "id": "msg_pgp" })).expect("verify");
    assert_eq!(verified["state"], json!(PgpVerifyState::Verified));
    assert_eq!(verified["fingerprint"], key.fingerprint);
    assert_eq!(verified["reason"], serde_json::Value::Null);

    let store = harness.open();
    let tampered_at = raw.windows(6).position(|window| window == b"Corpo ").expect("marker") + 1;
    let mut tampered = raw.clone();
    tampered[tampered_at] ^= 0x01;
    let hash = store.blobs().put(&tampered).expect("tampered blob");
    store.set_message_raw_blob_hash("msg_pgp", Some(&hash)).expect("tampered hash");
    let bad = engine.call("message_pgp_verify", json!({ "id": "msg_pgp" })).expect("verify tampered");
    assert_eq!(bad["state"], json!(PgpVerifyState::Bad));
    assert!(!bad.to_string().contains(PASSPHRASE));
}

#[test]
fn verify_reports_no_key_and_not_stored_without_faking_a_result() {
    let key = keypair();
    let raw = signed_raw(&headers("signed"), &entity(BODY), &key.secret_armor);
    let (_harness, mut engine) = received_fixture(&raw, None);
    let no_key = engine.call("message_pgp_verify", json!({ "id": "msg_pgp" })).expect("verify without keys");
    assert_eq!(no_key["state"], json!(PgpVerifyState::NoKey));

    let harness = Harness::new();
    let vault = MemoryVault::new();
    let store = harness.open();
    seed_account(&store, &vault, ACCOUNT_ID, 1, &[("inbox", "INBOX", FolderKind::Inbox)]);
    store.upsert_message(&message_row("msg_old")).expect("pre-v9 row");
    let mut engine = Engine::open_with_vault(store, vault).expect("engine");
    let not_stored = engine.call("message_pgp_verify", json!({ "id": "msg_old" })).expect("verify a pre-v9 row");
    assert_eq!(not_stored["state"], json!(PgpVerifyState::NotStored));
    let decrypted = engine.call("message_pgp_decrypt", json!({ "id": "msg_old", "passphrase": PASSPHRASE })).expect("decrypt a pre-v9 row");
    assert_eq!(decrypted["state"], json!(PgpDecryptState::NotStored));
    assert!(decrypted["text"].is_null());
}

#[test]
fn decrypt_round_trip_and_failure_states_never_touch_the_stored_body_or_fts() {
    let key = keypair();
    let secret_body = format!("{BODY}{PLAINTEXT_MARKER}\r\n");
    let raw = encrypted_raw(&headers("encrypted"), &entity(&secret_body), &key.public_armor);
    let (harness, mut engine) = received_fixture(&raw, Some(&key));

    let locked = engine.call("message_pgp_decrypt", json!({ "id": "msg_pgp" })).expect("empty passphrase");
    assert_eq!(locked["state"], json!(PgpDecryptState::Locked));

    let wrong = engine.call("message_pgp_decrypt", json!({ "id": "msg_pgp", "passphrase": "not the passphrase" })).expect("wrong passphrase");
    assert_eq!(wrong["state"], json!(PgpDecryptState::WrongPassphrase));
    assert!(!wrong.to_string().contains(PASSPHRASE), "the wrong passphrase must not be echoed");
    assert!(wrong["text"].is_null());

    let decrypted = engine.call("message_pgp_decrypt", json!({ "id": "msg_pgp", "passphrase": PASSPHRASE })).expect("decrypt");
    assert_eq!(decrypted["state"], json!(PgpDecryptState::Decrypted));
    let text = decrypted["text"].as_str().expect("decrypted text");
    assert!(text.contains(PLAINTEXT_MARKER), "the decrypted body is missing: {text}");
    assert!(text.contains("seconda riga"));

    let store = harness.open();
    let row = store.get_message("msg_pgp").expect("get_message").expect("the message row still exists");
    assert_eq!(row.body_text, "Original visible body");
    drop(store);
    let hits = engine.call("search", json!({ "query": PLAINTEXT_MARKER })).expect("search");
    assert_eq!(hits["total"], json!(0), "decrypted text must never enter FTS: {hits}");
    assert_eq!(engine.call("message_get", json!({ "id": "msg_pgp" })).expect("message_get")["message"]["body_text"], "Original visible body");
}

fn decrypt_blob(harness: &Harness, draft: &str, key: &Keypair) -> Vec<u8> {
    let store = harness.open();
    let row = store.active_outbox_for_draft(draft).expect("outbox lookup").expect("queued row");
    let blob = store.blobs().get(&row.raw_blob_hash, postino_core::MAX_RAW_MESSAGE_BYTES).expect("outbox raw blob");
    let reparsed = store.blobs().put(&blob).expect("content address");
    assert_eq!(reparsed, row.raw_blob_hash, "the outbox row must point at exactly these bytes");

    let limits = postino_sync::mime::Limits::default();
    let detected = postino_sync::mime::pgp_mime::detect(&blob, &limits).expect("detect").expect("PGP/MIME");
    let postino_sync::mime::pgp_mime::PgpMimeEntity::Encrypted { payload, .. } = detected else { panic!("the queued message is not encrypted") };
    let (strict, _) = SignedSecretKey::from_string(&key.secret_armor).expect("secret key");
    let password = Password::from(PASSPHRASE);
    assert!(matches!(strict.primary_key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))), "the test key must unlock");
    for subkey in &strict.secret_subkeys {
        assert!(matches!(subkey.key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))), "the test subkey must unlock");
    }
    let (message, _) = Message::from_armor(std::io::Cursor::new(payload)).expect("payload");
    let mut decrypted = message.decrypt(&password, &strict).expect("decrypt the outbox blob");
    if decrypted.is_compressed() {
        decrypted = decrypted.decompress().expect("decompress");
    }
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut decrypted, &mut data).expect("read the decrypted entity");
    data
}

#[test]
fn sign_dry_run_and_encrypt_send_keep_exact_outbox_bytes() {
    let (harness, mut engine, fixture_key) = send_fixture();
    let key = keypair();

    // The fixture engine already imported its own key; import this second pair for this test.
    let imported =
        engine.call("pgp_key_import", json!({ "armored_key": key.secret_armor, "passphrase": PASSPHRASE })).expect("import the second private key");
    let sign_key = imported["id"].as_str().expect("id").to_owned();
    assert_ne!(key_id(&mut engine, &fixture_key.fingerprint), sign_key, "two generated keys must be distinct");

    let draft = new_draft(&mut engine, "Testo in chiaro\r\n");
    let options = PgpSendOptions { sign: Some(sign_key.clone()), encrypt: Vec::new(), passphrase: PASSPHRASE.to_owned() };
    let dry = engine.call("send_dry_run", json!({ "draft": draft, "pgp": { "sign": sign_key, "passphrase": PASSPHRASE } })).expect("signed dry run");
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(dry["raw_base64"].as_str().expect("base64")).expect("decode");
    let detected = postino_sync::mime::pgp_mime::detect(&raw, &postino_sync::mime::Limits::default()).expect("detect").expect("PGP/MIME");
    let postino_sync::mime::pgp_mime::PgpMimeEntity::Signed { entity: signed, signature, .. } = detected else { panic!("the dry run is not signed") };
    let (signature, _) = DetachedSignature::from_string(std::str::from_utf8(&signature).expect("armor")).expect("signature");
    let (public, _) = SignedPublicKey::from_string(&key.public_armor).expect("public");
    assert!(signature.verify(&public, &signed).is_ok(), "the dry-run signature does not verify against the exact signed bytes");
    assert!(dry["raw_base64"].as_str().is_some_and(|raw| !raw.is_empty()));
    assert!(harness.open().active_outbox_for_draft(&draft).expect("lookup").is_none(), "a dry run must not queue");
    assert!(!format!("{options:?}").contains(PASSPHRASE), "the Debug form leaked the passphrase");

    // Encryption to the imported public key, then decrypt the journaled bytes.
    let encrypt_draft = new_draft(&mut engine, &format!("Testo cifrato\r\n{PLAINTEXT_MARKER}"));
    let queued = engine.call("message_send", json!({ "draft": encrypt_draft, "pgp": { "encrypt": [sign_key] } })).expect("encrypted send");
    assert_eq!(queued["outbox"]["state"], "queued");
    let inner = decrypt_blob(&harness, &encrypt_draft, &key);
    assert!(String::from_utf8_lossy(&inner).contains(PLAINTEXT_MARKER), "the queued body was not encrypted/decrypted correctly");

    // Sign+encrypt: decrypting yields an inner multipart/signed entity.
    let both = new_draft(&mut engine, "Firmato e cifrato\r\n");
    engine
        .call("message_send", json!({ "draft": both, "pgp": { "sign": sign_key, "encrypt": [sign_key], "passphrase": PASSPHRASE } }))
        .expect("sign+encrypt send");
    let inner = decrypt_blob(&harness, &both, &key);
    assert!(matches!(
        postino_sync::mime::pgp_mime::detect(&inner, &postino_sync::mime::Limits::default()).expect("detect inner"),
        Some(postino_sync::mime::pgp_mime::PgpMimeEntity::Signed { .. })
    ));
}

#[test]
fn encryption_uses_the_encryption_subkey_when_the_primary_cannot_encrypt() {
    let (harness, mut engine, fixture_key) = send_fixture();
    let key = keypair_with_subkey(false);
    let imported =
        engine.call("pgp_key_import", json!({ "armored_key": key.secret_armor, "passphrase": PASSPHRASE })).expect("import the subkey certificate");
    let encryption_id = imported["id"].as_str().expect("id").to_owned();
    assert_ne!(key_id(&mut engine, &fixture_key.fingerprint), encryption_id, "the two certificates must be distinct");

    let draft = new_draft(&mut engine, &format!("Solo chiave di cifratura\r\n{PLAINTEXT_MARKER}"));
    engine.call("message_send", json!({ "draft": draft, "pgp": { "encrypt": [encryption_id] } })).expect("encrypt to the encryption subkey");
    let inner = decrypt_blob(&harness, &draft, &key);
    assert!(String::from_utf8_lossy(&inner).contains(PLAINTEXT_MARKER), "the queued body was not encrypted to the subkey");
}

#[test]
fn verification_accepts_a_signature_made_by_a_signing_subkey() {
    let key = keypair_with_subkey(true);
    let entity = entity(BODY);
    let (secret, _) = SignedSecretKey::from_string(&key.secret_armor).expect("secret key");
    let password = Password::from(PASSPHRASE);
    let Some(subkey) = secret.secret_subkeys.first() else { panic!("the fixture must have a signing subkey") };
    assert!(matches!(subkey.key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))), "the signing subkey must unlock");
    let signature =
        DetachedSignature::sign_binary_data(OsRng, &subkey.key, &password, HashAlgorithm::Sha256, entity.as_slice()).expect("sign with the subkey");
    let armor = signature.to_armored_string(ArmorOptions::default()).expect("signature armor");
    let raw = signed_raw_with_signature(&headers("subkey"), &entity, &armor);

    let (_harness, mut engine) = received_fixture(&raw, Some(&key));
    let verified = engine.call("message_pgp_verify", json!({ "id": "msg_pgp" })).expect("verify");
    assert_eq!(verified["state"], json!(PgpVerifyState::Verified), "a valid signing-subkey signature must not be reported bad: {verified}");
}

#[test]
fn a_queued_plaintext_row_cannot_be_reused_for_a_protected_send() {
    let (harness, mut engine, fixture_key) = send_fixture();
    let encryption_id = key_id(&mut engine, &fixture_key.fingerprint);
    let draft = new_draft(&mut engine, "Mai in chiaro\r\n");
    engine.call("message_send", json!({ "draft": draft })).expect("plain queue");
    let before = harness.open().active_outbox_for_draft(&draft).expect("lookup").expect("queued row");

    let error = engine
        .call("message_send", json!({ "draft": draft, "pgp": { "encrypt": [encryption_id] } }))
        .expect_err("a protected send must not reuse a plaintext row");
    assert!(error.to_string().contains("already queued"), "{error}");
    let after = harness.open().active_outbox_for_draft(&draft).expect("lookup").expect("queued row");
    assert_eq!(before.raw_blob_hash, after.raw_blob_hash, "the queued bytes must not change");
}

#[test]
fn missing_or_public_recipient_key_aborts_without_a_cleartext_fallback() {
    let (harness, mut engine, _fixture_key) = send_fixture();
    let draft = new_draft(&mut engine, "Mai in chiaro\r\n");
    let failed =
        engine.call("message_send", json!({ "draft": draft, "pgp": { "encrypt": ["pgp_999"] } })).expect_err("an unknown recipient key must abort");
    assert!(failed.to_string().contains("does not exist"), "{failed}");
    assert!(harness.open().active_outbox_for_draft(&draft).expect("lookup").is_none(), "no outbox row may exist");
    assert!(!engine.call("outbox_list", json!({})).expect("outbox").to_string().contains(&draft), "the aborted draft must not be journaled");

    let dry_failed =
        engine.call("send_dry_run", json!({ "draft": draft, "pgp": { "encrypt": ["pgp_999"] } })).expect_err("a dry run must fail the same way");
    assert!(dry_failed.to_string().contains("does not exist"), "{dry_failed}");

    let key = keypair();
    let public = engine.call("pgp_key_import", json!({ "armored_key": key.public_armor })).expect("import public");
    let public_id = public["id"].as_str().expect("id").to_owned();
    let refused = engine
        .call("message_send", json!({ "draft": draft, "pgp": { "sign": public_id, "passphrase": PASSPHRASE } }))
        .expect_err("a public-only key cannot sign");
    assert!(refused.to_string().contains("no stored secret"), "{refused}");
    assert!(harness.open().active_outbox_for_draft(&draft).expect("lookup").is_none());
}

#[test]
fn registry_annotations_and_sensitive_exclusion_contract() {
    let defs = postino_core::ops::op_defs();
    let find = |name: &str| defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("{name} is missing"));
    let verify = find("message_pgp_verify");
    assert!(verify.read_only);
    assert!(!verify.sensitive);
    let decrypt = find("message_pgp_decrypt");
    assert!(decrypt.sensitive);
    assert!(decrypt.read_only);
    assert!(decrypt.description.contains("stdin"));
    let send = find("message_send");
    assert!(send.input_schema["properties"]["pgp"].is_object());
}
