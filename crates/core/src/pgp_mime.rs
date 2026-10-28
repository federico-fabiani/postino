//! PGP/MIME verification, decryption and outgoing signing/encryption (P12c2b).
//!
//! Structure lives in [`postino_mime::pgp_mime`]; this module owns the OpenPGP calls. Received raw
//! bytes come from the encrypted blob store (schema v9), never from a re-serialized body: a
//! signature only verifies against the exact bytes that were signed.
//!
//! Decrypted content is returned to the caller and never written to `body_text`, `body_html` or
//! the FTS index. A missing, revoked or expired recipient key aborts a send with a fixed error:
//! there is no cleartext fallback. Passphrases are borrowed for one call, copied into [zeroize]
//! buffers only where a library needs bytes, and never appear in an error, a log or a Debug impl.

use std::io::Read;

use pgp::composed::{ArmorOptions, Deserializable, DetachedSignature, Message, MessageBuilder, SignedPublicKey, SignedPublicSubKey, SignedSecretKey};
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::packet::{KeyFlags, Signature, SignatureType};
use pgp::types::{KeyDetails, Password};
use rand::rngs::OsRng;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::engine::OpError;
use crate::model::Account;
use crate::pgp::{MAX_PGP_KEY_BYTES, MAX_PGP_PASSPHRASE_BYTES};
use postino_mime::pgp_mime::{self, MICALG_SHA256, PgpMimeEntity};
use postino_store::Store;
use postino_sync::mime as postino_mime;

/// Largest raw message read back for verify/decrypt. Ingest bounds the stored blob by the same
/// order of magnitude; this is a last-resort ceiling, not a configuration knob.
pub const MAX_RAW_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Most recipients one encrypted send may address.
pub const MAX_PGP_RECIPIENTS: usize = 100;

/// Explicit verification outcome. There is no "probably signed" state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PgpVerifyState {
    /// At least one stored key validates the exact signed bytes.
    Verified,
    /// A key is stored for the message but the signature does not match.
    Bad,
    /// The bytes are readable but the result cannot be attributed safely.
    Unknown,
    /// No public key is stored for this account.
    NoKey,
    /// The message predates schema v9 or was not retained. Never a fake verify.
    NotStored,
}

/// Explicit decryption outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PgpDecryptState {
    /// The message decrypted and was re-parsed under the normal MIME limits.
    Decrypted,
    /// Every stored secret key is passphrase-protected and no passphrase was supplied.
    Locked,
    /// A passphrase was supplied but does not unlock the stored secret key.
    WrongPassphrase,
    /// No secret key is stored for this account.
    NoKey,
    /// The payload could not be decrypted or parsed safely.
    Unknown,
    /// The message predates schema v9 or was not retained.
    NotStored,
}

/// What the retained raw bytes are, independent of signature validity. `none` means the message is
/// readable but is not PGP/MIME; the UI shows no PGP affordance in that case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PgpMimeKind {
    None,
    Signed,
    Encrypted,
}

/// Result of `message_pgp_verify`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PgpVerifyResult {
    pub kind: PgpMimeKind,
    pub state: PgpVerifyState,
    /// Stored key row id, when the signature was attributed to one.
    pub key_id: Option<String>,
    pub fingerprint: Option<String>,
    /// Primary user id of the signing key, when known.
    pub signer: Option<String>,
    /// Fixed sentence for a non-verified state; never key material.
    pub reason: Option<String>,
}

/// Result of `message_pgp_decrypt`. The body is display-only and is never persisted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PgpDecryptResult {
    pub state: PgpDecryptState,
    /// Stored key row id, when the message was decrypted with one.
    pub key_id: Option<String>,
    pub fingerprint: Option<String>,
    /// Plain text of the decrypted body, already sanitized for HTML and bounded by MIME limits.
    pub text: Option<String>,
    /// Sanitized HTML of the decrypted body. Never written back to the message row or FTS.
    pub html: Option<String>,
    pub reason: Option<String>,
}

/// Sign/encrypt options carried by `message_send`, `send_schedule` and `send_dry_run`.
///
/// `encrypt` is a list of stored key ids; every recipient key must exist, be usable and not be
/// revoked or expired, otherwise the send aborts. The passphrase belongs to `sign` and is accepted
/// only from stdin or a no-echo prompt.
#[derive(Clone, Default, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PgpSendOptions {
    /// Stored secret key id used to sign.
    pub sign: Option<String>,
    /// Stored public key ids used to encrypt, one per recipient.
    #[serde(default)]
    pub encrypt: Vec<String>,
    /// Signing-key passphrase. Never accepted in argv, environment, logs or MCP arguments, and
    /// never serialized back out. It is also kept out of the published schema: a generic client
    /// must prompt for it through the CLI or the app, never through `postino run` or MCP.
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub passphrase: String,
}

impl Drop for PgpSendOptions {
    fn drop(&mut self) {
        self.passphrase.zeroize();
    }
}

impl PgpSendOptions {
    /// Whether the caller asked for neither signing nor encryption.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.sign.is_none() && self.encrypt.is_empty()
    }
}

impl std::fmt::Debug for PgpSendOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgpSendOptions")
            .field("sign", &self.sign)
            .field("encrypt", &self.encrypt)
            .field("passphrase", &if self.passphrase.is_empty() { "[empty]" } else { "[redacted]" })
            .finish()
    }
}

/// Send-time PGP failures with fixed, non-secret sentences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgpSendError {
    UnknownKey,
    NoSecretKey,
    Locked,
    WrongPassphrase,
    RevokedKey,
    ExpiredKey,
    NoEncryptionKey,
    MalformedKey,
    MalformedMessage,
    TooManyRecipients,
    MissingRecipient,
}

impl PgpSendError {
    /// A sentence safe for an operation error and a log line.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::UnknownKey => "a PGP key id passed to this send does not exist for this account; use pgp_keys_list",
            Self::NoSecretKey => "the signing key has no stored secret material; import the private key again",
            Self::Locked => "the signing key is passphrase-protected; supply the passphrase from stdin or a no-echo prompt",
            Self::WrongPassphrase => "the passphrase does not unlock the signing key; nothing was queued",
            Self::RevokedKey => "a recipient key is revoked; the message was not queued and no cleartext was sent",
            Self::ExpiredKey => "a recipient key is expired; the message was not queued and no cleartext was sent",
            Self::NoEncryptionKey => "a recipient key has no usable encryption key; ask for a certificate with an encryption-capable key or subkey",
            Self::MalformedKey => "a stored PGP key could not be read; export and import it again",
            Self::MalformedMessage => "the outgoing message has no MIME entity to protect",
            Self::TooManyRecipients => "an encrypted send addresses more than 100 keys",
            Self::MissingRecipient => "an encrypted send needs at least one recipient key",
        }
    }
}

/// Verify a retained raw PGP/MIME message against the account's stored public keys.
pub fn verify(store: &Store, account_id: &str, raw: &[u8]) -> Result<PgpVerifyResult, OpError> {
    let limits = postino_mime::Limits::default();
    let entity = match pgp_mime::detect(raw, &limits) {
        Ok(Some(entity)) => entity,
        Ok(None) => return Ok(verify_state(PgpMimeKind::None, PgpVerifyState::Unknown, "the retained message is not PGP/MIME")),
        Err(error) => {
            return Ok(verify_state(PgpMimeKind::None, PgpVerifyState::Unknown, &format!("the retained message is not readable PGP/MIME: {error}")));
        }
    };
    let PgpMimeEntity::Signed { entity, signature, .. } = entity else {
        return Ok(verify_state(PgpMimeKind::Encrypted, PgpVerifyState::Unknown, "the retained message is encrypted, not signed; decrypt it first"));
    };
    let Some(signature) = parse_detached_signature(&signature) else {
        return Ok(verify_state(PgpMimeKind::Signed, PgpVerifyState::Unknown, "the detached signature is not readable OpenPGP data"));
    };
    let summaries = store.list_pgp_keys(account_id).map_err(crate::store_map::read_error)?;
    if summaries.is_empty() {
        return Ok(verify_state(PgpMimeKind::Signed, PgpVerifyState::NoKey, "no PGP key is stored for this account"));
    }
    let mut readable = 0usize;
    for summary in &summaries {
        let Some(armor) = store.get_pgp_public_armor(account_id, &summary.id).map_err(crate::store_map::read_error)? else { continue };
        let Ok(key) = crate::pgp::parse_public_key(&armor) else { continue };
        readable = readable.saturating_add(1);
        if signature_matches(&signature, &key, &entity) {
            return Ok(PgpVerifyResult {
                kind: PgpMimeKind::Signed,
                state: PgpVerifyState::Verified,
                key_id: Some(summary.id.clone()),
                fingerprint: Some(summary.fingerprint.clone()),
                signer: summary.user_id.clone(),
                reason: None,
            });
        }
    }
    if readable == 0 {
        return Ok(verify_state(PgpMimeKind::Signed, PgpVerifyState::Unknown, "the stored public keys could not be parsed"));
    }
    Ok(verify_state(PgpMimeKind::Signed, PgpVerifyState::Bad, "the signature does not match any stored key"))
}

/// Decrypt a retained raw PGP/MIME message with a per-use passphrase.
///
/// The returned body is display-only. The caller must not write it back to `body_text`,
/// `body_html` or FTS.
pub fn decrypt(store: &Store, account_id: &str, raw: &[u8], passphrase: &[u8]) -> Result<PgpDecryptResult, OpError> {
    if passphrase.len() > MAX_PGP_PASSPHRASE_BYTES {
        return Err(OpError::InvalidArgs("the passphrase is longer than 256 bytes".to_owned()));
    }
    let limits = postino_mime::Limits::default();
    let entity = match pgp_mime::detect(raw, &limits) {
        Ok(Some(entity)) => entity,
        Ok(None) => return Ok(decrypt_state(PgpDecryptState::Unknown, "the retained message is not PGP/MIME")),
        Err(error) => return Ok(decrypt_state(PgpDecryptState::Unknown, &format!("the retained message is not readable PGP/MIME: {error}"))),
    };
    let PgpMimeEntity::Encrypted { payload, .. } = entity else {
        return Ok(decrypt_state(PgpDecryptState::Unknown, "the retained message is signed, not encrypted"));
    };
    let secret = Zeroizing::new(passphrase.to_vec());
    let summaries = store.list_pgp_keys(account_id).map_err(crate::store_map::read_error)?;
    let secret_keys = summaries.iter().filter(|key| key.has_secret).collect::<Vec<_>>();
    if secret_keys.is_empty() {
        return Ok(decrypt_state(PgpDecryptState::NoKey, "no secret PGP key is stored for this account"));
    }
    let mut locked = false;
    let mut wrong = false;
    let mut readable = false;
    for summary in secret_keys {
        let Some(armor) = store.get_pgp_secret_armor(account_id, &summary.id).map_err(crate::store_map::read_error)? else { continue };
        let Ok(key) = crate::pgp::parse_secret_key(&armor) else { continue };
        readable = true;
        let protected = key.primary_key.secret_params().is_encrypted();
        if protected {
            if secret.is_empty() {
                locked = true;
                continue;
            }
            let password = Password::from(secret.as_slice());
            if !matches!(key.primary_key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))) {
                wrong = true;
                continue;
            }
        }
        let password = Password::from(secret.as_slice());
        let Some(data) = decrypt_payload(&payload, &password, &key) else { continue };
        let parsed = match postino_mime::parse(&data, &limits) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        return Ok(PgpDecryptResult {
            state: PgpDecryptState::Decrypted,
            key_id: Some(summary.id.clone()),
            fingerprint: Some(summary.fingerprint.clone()),
            text: parsed.text,
            html: parsed.html,
            reason: None,
        });
    }
    if wrong {
        return Ok(decrypt_state(PgpDecryptState::WrongPassphrase, "the passphrase does not unlock the stored secret key; nothing was decrypted"));
    }
    if locked {
        return Ok(decrypt_state(PgpDecryptState::Locked, "the stored secret key is passphrase-protected; supply the passphrase"));
    }
    if !readable {
        return Ok(decrypt_state(PgpDecryptState::Unknown, "the stored secret keys could not be parsed"));
    }
    Ok(decrypt_state(PgpDecryptState::Unknown, "no stored secret key could decrypt this message"))
}

/// Apply `PgpSendOptions` to already-serialized RFC 5322 bytes.
///
/// Returns the exact bytes to journal: signing wraps the MIME entity, encryption wraps the signed
/// entity, and the outer message headers are copied unchanged. Any failure leaves the input
/// untouched and is reported before a blob or an outbox row is written.
pub fn apply_send(store: &Store, account: &Account, raw: Vec<u8>, options: &PgpSendOptions) -> Result<Vec<u8>, OpError> {
    if options.is_plain() {
        return Ok(raw);
    }
    if options.encrypt.len() > MAX_PGP_RECIPIENTS {
        return Err(send_error(PgpSendError::TooManyRecipients));
    }
    if options.sign.is_none() && options.encrypt.is_empty() {
        return Err(send_error(PgpSendError::MissingRecipient));
    }
    if options.passphrase.len() > MAX_PGP_PASSPHRASE_BYTES {
        return Err(OpError::InvalidArgs("the passphrase is longer than 256 bytes".to_owned()));
    }
    let passphrase = Zeroizing::new(options.passphrase.as_bytes().to_vec());
    let signing = match options.sign.as_deref() {
        Some(id) => Some(unlock_signing_key(store, account, id, passphrase.as_slice())?),
        None => None,
    };
    let now = chrono::Utc::now().timestamp();
    let mut recipients = Vec::with_capacity(options.encrypt.len());
    for id in &options.encrypt {
        recipients.push(recipient_key(store, account, id, now)?);
    }
    let parts = pgp_mime::split_outgoing(&raw).map_err(|_| send_error(PgpSendError::MalformedMessage))?;
    let mut entity = parts.entity;
    if let Some((key, password)) = signing {
        let signature = DetachedSignature::sign_binary_data(OsRng, &key.primary_key, &password, HashAlgorithm::Sha256, entity.as_slice())
            .map_err(|_| send_error(PgpSendError::MalformedKey))?;
        let armor = signature.to_armored_string(ArmorOptions::default()).map_err(|_| send_error(PgpSendError::MalformedKey))?;
        entity = pgp_mime::serialize_signed(&entity, &armor, MICALG_SHA256).map_err(|_| send_error(PgpSendError::MalformedMessage))?;
    }
    if !recipients.is_empty() {
        let mut builder = MessageBuilder::from_bytes("message", entity).seipd_v1(OsRng, SymmetricKeyAlgorithm::AES256);
        for key in &recipients {
            if let Some(subkey) = encryption_subkey(key, now) {
                builder.encrypt_to_key(OsRng, subkey).map_err(|_| send_error(PgpSendError::MalformedKey))?;
            } else if primary_can_encrypt(key, now) {
                builder.encrypt_to_key(OsRng, key).map_err(|_| send_error(PgpSendError::MalformedKey))?;
            } else {
                return Err(send_error(PgpSendError::NoEncryptionKey));
            }
        }
        let armor = builder.to_armored_string(OsRng, ArmorOptions::default()).map_err(|_| send_error(PgpSendError::MalformedKey))?;
        entity = pgp_mime::serialize_encrypted(&armor).map_err(|_| send_error(PgpSendError::MalformedMessage))?;
    }
    let mut out = parts.message_headers;
    out.extend_from_slice(&entity);
    Ok(out)
}

fn unlock_signing_key(store: &Store, account: &Account, id: &str, passphrase: &[u8]) -> Result<(SignedSecretKey, Password), OpError> {
    let summary = store.get_pgp_key(&account.id, id).map_err(crate::store_map::read_error)?.ok_or_else(|| send_error(PgpSendError::UnknownKey))?;
    if !summary.has_secret {
        return Err(send_error(PgpSendError::NoSecretKey));
    }
    let armor =
        store.get_pgp_secret_armor(&account.id, id).map_err(crate::store_map::read_error)?.ok_or_else(|| send_error(PgpSendError::NoSecretKey))?;
    let key = crate::pgp::parse_secret_key(&armor).map_err(|_| send_error(PgpSendError::MalformedKey))?;
    let password = Password::from(passphrase);
    if key.primary_key.secret_params().is_encrypted() {
        if passphrase.is_empty() {
            return Err(send_error(PgpSendError::Locked));
        }
        if !matches!(key.primary_key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))) {
            return Err(send_error(PgpSendError::WrongPassphrase));
        }
    }
    Ok((key, password))
}

fn recipient_key(store: &Store, account: &Account, id: &str, now: i64) -> Result<SignedPublicKey, OpError> {
    if store.get_pgp_key(&account.id, id).map_err(crate::store_map::read_error)?.is_none() {
        return Err(send_error(PgpSendError::UnknownKey));
    }
    let Some(armor) = store.get_pgp_public_armor(&account.id, id).map_err(crate::store_map::read_error)? else {
        return Err(send_error(PgpSendError::UnknownKey));
    };
    if armor.len() > MAX_PGP_KEY_BYTES {
        return Err(send_error(PgpSendError::MalformedKey));
    }
    let key = crate::pgp::parse_public_key(&armor).map_err(|_| send_error(PgpSendError::MalformedKey))?;
    if !key.details.revocation_signatures.is_empty() {
        return Err(send_error(PgpSendError::RevokedKey));
    }
    if key.verify_bindings().is_err() {
        return Err(send_error(PgpSendError::MalformedKey));
    }
    if key_is_expired(&key, now) {
        return Err(send_error(PgpSendError::ExpiredKey));
    }
    Ok(key)
}

fn key_is_expired(key: &SignedPublicKey, now: i64) -> bool {
    let created = i64::from(key.primary_key.created_at().as_secs());
    let Some(expiry) = key
        .details
        .users
        .iter()
        .flat_map(|user| user.signatures.iter())
        .filter_map(|signature| signature.key_expiration_time().map(|duration| duration.as_secs()))
        .min()
    else {
        return false;
    };
    let seconds = i64::from(expiry);
    seconds > 0 && now > created.saturating_add(seconds)
}

/// The newest binding signature decides a subkey's key flags and expiry.
fn newest_binding(signatures: &[Signature]) -> Option<&Signature> {
    signatures
        .iter()
        .filter(|signature| signature.typ() == Some(SignatureType::SubkeyBinding))
        .max_by_key(|signature| signature.created().map(|created| created.as_secs()))
}

/// The newest non-revoked, unexpired encryption-capable subkey, if the certificate has one.
fn encryption_subkey(key: &SignedPublicKey, now: i64) -> Option<&SignedPublicSubKey> {
    key.public_subkeys
        .iter()
        .filter_map(|subkey| {
            let binding = newest_binding(&subkey.signatures)?;
            let flags = binding.key_flags();
            if !(flags.encrypt_comms() || flags.encrypt_storage()) {
                return None;
            }
            if subkey.signatures.iter().any(|signature| signature.typ() == Some(SignatureType::SubkeyRevocation)) {
                return None;
            }
            let created = i64::from(subkey.key.created_at().as_secs());
            if binding.key_expiration_time().is_some_and(|expiry| {
                let seconds = i64::from(expiry.as_secs());
                seconds > 0 && now > created.saturating_add(seconds)
            }) {
                return None;
            }
            Some((subkey, created))
        })
        .max_by_key(|(_, created)| *created)
        .map(|(subkey, _)| subkey)
}

/// Whether the primary key itself may encrypt. Most modern keys delegate encryption to a subkey.
fn primary_can_encrypt(key: &SignedPublicKey, now: i64) -> bool {
    let flags = primary_key_flags(key).unwrap_or_default();
    (flags.encrypt_comms() || flags.encrypt_storage()) && key.details.revocation_signatures.is_empty() && !key_is_expired(key, now)
}

/// The key flags from the newest self-signature or direct signature.
fn primary_key_flags(key: &SignedPublicKey) -> Option<KeyFlags> {
    key.details
        .users
        .iter()
        .flat_map(|user| user.signatures.iter())
        .chain(key.details.direct_signatures.iter())
        .max_by_key(|signature| signature.created().map(|created| created.as_secs()))
        .map(Signature::key_flags)
}

/// A detached signature may have been made by the primary key or by a signing subkey.
fn signature_matches(signature: &DetachedSignature, key: &SignedPublicKey, entity: &[u8]) -> bool {
    if signature.verify(&key.primary_key, entity).is_ok() {
        return true;
    }
    key.public_subkeys.iter().any(|subkey| signature.verify(&subkey.key, entity).is_ok())
}

fn decrypt_payload(payload: &[u8], password: &Password, key: &SignedSecretKey) -> Option<Vec<u8>> {
    let message = parse_pgp_message(payload)?;
    let mut decrypted = message.decrypt(password, key).ok()?;
    if decrypted.is_compressed() {
        decrypted = decrypted.decompress().ok()?;
    }
    let limit = postino_mime::Limits::default().max_bytes;
    let mut data = Vec::new();
    decrypted.take(u64::try_from(limit).ok()?.saturating_add(1)).read_to_end(&mut data).ok()?;
    if data.len() > limit {
        return None;
    }
    Some(data)
}

fn parse_pgp_message(payload: &[u8]) -> Option<Message<'_>> {
    if payload.starts_with(b"-----BEGIN PGP MESSAGE-----") {
        let text = std::str::from_utf8(payload).ok()?.trim();
        if !text.ends_with("-----END PGP MESSAGE-----") {
            return None;
        }
        Message::from_string(text).ok().map(|(message, _headers)| message)
    } else {
        Message::from_bytes(payload).ok()
    }
}

fn parse_detached_signature(signature: &[u8]) -> Option<DetachedSignature> {
    if signature.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
        let text = std::str::from_utf8(signature).ok()?;
        DetachedSignature::from_string(text).ok().map(|(signature, _headers)| signature)
    } else {
        DetachedSignature::from_bytes(signature).ok()
    }
}

fn send_error(error: PgpSendError) -> OpError {
    OpError::InvalidArgs(error.message().to_owned())
}

fn verify_state(kind: PgpMimeKind, state: PgpVerifyState, reason: &str) -> PgpVerifyResult {
    PgpVerifyResult { kind, state, key_id: None, fingerprint: None, signer: None, reason: Some(reason.to_owned()) }
}

fn decrypt_state(state: PgpDecryptState, reason: &str) -> PgpDecryptResult {
    PgpDecryptResult { state, key_id: None, fingerprint: None, text: None, html: None, reason: Some(reason.to_owned()) }
}
