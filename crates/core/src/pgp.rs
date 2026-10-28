//! OpenPGP key import parsing (P12c1).
//!
//! The caller has already bounded the armored input and the passphrase. This module decides whether
//! the armor is a public or private key, refuses an unprotected private key, verifies the passphrase
//! against every secret subkey and produces the canonical armor that the store keeps.
//!
//! Nothing here logs. Error messages are fixed sentences that never contain key material or the
//! passphrase, and a failed passphrase check happens before any store write.

use pgp::composed::{ArmorOptions, Deserializable, SignedPublicKey, SignedSecretKey};
use pgp::types::{KeyDetails, Password};
use zeroize::Zeroizing;

/// The largest armored key accepted, in bytes.
pub const MAX_PGP_KEY_BYTES: usize = 1024 * 1024;
/// The largest passphrase accepted, in bytes.
pub const MAX_PGP_PASSPHRASE_BYTES: usize = 256;
/// The most PGP key rows one account may hold.
pub const MAX_PGP_KEYS_PER_ACCOUNT: u32 = 64;

const PRIVATE_ARMOR: &str = "-----BEGIN PGP PRIVATE KEY BLOCK-----";
const PRIVATE_END: &str = "-----END PGP PRIVATE KEY BLOCK-----";
const PUBLIC_ARMOR: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
const PUBLIC_END: &str = "-----END PGP PUBLIC KEY BLOCK-----";

/// A key that passed the import checks, ready to be stored.
pub struct ParsedPgpKey {
    pub fingerprint: String,
    pub key_id: String,
    pub user_id: Option<String>,
    pub public_armor: String,
    /// Present only for a private key; still protected by the user's passphrase.
    pub secret_armor: Option<Zeroizing<String>>,
    pub protected: bool,
}

/// Import failures with fixed, non-secret messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgpImportError {
    TooLarge,
    MissingArmor,
    PassphraseRequired,
    PassphraseNotAllowed,
    PassphraseTooLong,
    NotProtected,
    WrongPassphrase,
    Malformed,
    MultipleKeys,
}

impl PgpImportError {
    /// A sentence safe to return to an agent or show to a person.
    pub fn message(self) -> &'static str {
        match self {
            Self::TooLarge => "the armored key is larger than 1 MiB; shorten it or export it without extra text",
            Self::MissingArmor => "the input is not an armored OpenPGP public or private key block",
            Self::PassphraseRequired => "this private key is passphrase-protected; supply the passphrase (it is never accepted on the command line)",
            Self::PassphraseNotAllowed => "a passphrase was supplied for a public key; public keys are imported without one",
            Self::PassphraseTooLong => "the passphrase is longer than 256 bytes",
            Self::NotProtected => "this private key is not passphrase-protected; add a passphrase to it before importing",
            Self::WrongPassphrase => "the passphrase does not unlock this private key; nothing was stored",
            Self::Malformed => "the armored key could not be parsed as a single OpenPGP key",
            Self::MultipleKeys => "the input contains more than one key or armor block; import exactly one key at a time",
        }
    }
}

/// Parse and verify one armored key. `passphrase` must be empty for a public key.
pub fn parse_import(armored: &str, passphrase: &[u8]) -> Result<ParsedPgpKey, PgpImportError> {
    if armored.len() > MAX_PGP_KEY_BYTES {
        return Err(PgpImportError::TooLarge);
    }
    if passphrase.len() > MAX_PGP_PASSPHRASE_BYTES {
        return Err(PgpImportError::PassphraseTooLong);
    }
    let trimmed = armored.trim_start();
    if trimmed.starts_with(PRIVATE_ARMOR) {
        parse_private(trimmed, passphrase)
    } else if trimmed.starts_with(PUBLIC_ARMOR) {
        if !passphrase.is_empty() {
            return Err(PgpImportError::PassphraseNotAllowed);
        }
        parse_public(trimmed)
    } else {
        Err(PgpImportError::MissingArmor)
    }
}

fn parse_public(armored: &str) -> Result<ParsedPgpKey, PgpImportError> {
    let key = parse_public_key(armored)?;
    let (public_armor, fingerprint, key_id, user_id) = describe_public(&key)?;
    Ok(ParsedPgpKey { fingerprint, key_id, user_id, public_armor, secret_armor: None, protected: false })
}

/// Parse one armored public key, bounded and with no trailing block or text.
pub(crate) fn parse_public_key(armored: &str) -> Result<SignedPublicKey, PgpImportError> {
    if armored.len() > MAX_PGP_KEY_BYTES {
        return Err(PgpImportError::TooLarge);
    }
    let block = single_armor_block(armored, PUBLIC_ARMOR, PUBLIC_END)?;
    let (keys, _headers) = SignedPublicKey::from_string_many(block).map_err(|_| PgpImportError::Malformed)?;
    one_key(keys)
}

/// Parse one armored secret key without unlocking it. The passphrase is checked by the caller.
pub(crate) fn parse_secret_key(armored: &str) -> Result<SignedSecretKey, PgpImportError> {
    if armored.len() > MAX_PGP_KEY_BYTES {
        return Err(PgpImportError::TooLarge);
    }
    let block = single_armor_block(armored, PRIVATE_ARMOR, PRIVATE_END)?;
    let (keys, _headers) = SignedSecretKey::from_string_many(block).map_err(|_| PgpImportError::Malformed)?;
    one_key(keys)
}

fn parse_private(armored: &str, passphrase: &[u8]) -> Result<ParsedPgpKey, PgpImportError> {
    if passphrase.is_empty() {
        return Err(PgpImportError::PassphraseRequired);
    }
    let key = parse_secret_key(armored)?;
    if !key.primary_key.secret_params().is_encrypted() || key.secret_subkeys.iter().any(|subkey| !subkey.key.secret_params().is_encrypted()) {
        return Err(PgpImportError::NotProtected);
    }
    let password = Password::from(passphrase);
    if !matches!(key.primary_key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))) {
        return Err(PgpImportError::WrongPassphrase);
    }
    for subkey in &key.secret_subkeys {
        if !matches!(subkey.key.unlock(&password, |_, _| Ok(())), Ok(Ok(()))) {
            return Err(PgpImportError::WrongPassphrase);
        }
    }
    let fingerprint = key.fingerprint().to_string().to_uppercase();
    let key_id = key.legacy_key_id().to_string().to_uppercase();
    let user_id = primary_user_id(&key.details);
    let public_armor = key.to_public_key().to_armored_string(ArmorOptions::default()).map_err(|_| PgpImportError::Malformed)?;
    let secret_armor = key.to_armored_string(ArmorOptions::default()).map_err(|_| PgpImportError::Malformed)?;
    if public_armor.len() > MAX_PGP_KEY_BYTES || secret_armor.len() > MAX_PGP_KEY_BYTES {
        return Err(PgpImportError::TooLarge);
    }
    Ok(ParsedPgpKey { fingerprint, key_id, user_id, public_armor, secret_armor: Some(Zeroizing::new(secret_armor)), protected: true })
}

/// Cut the first armor block and refuse anything after its footer. The OpenPGP reader stops at the
/// first footer, so a second block (or trailing text) would otherwise be ignored silently.
fn single_armor_block<'a>(armored: &'a str, begin: &str, end: &str) -> Result<&'a str, PgpImportError> {
    let start = armored.find(begin).ok_or(PgpImportError::MissingArmor)?;
    let body_start = start + begin.len();
    let end_at = armored[body_start..].find(end).map(|offset| body_start + offset).ok_or(PgpImportError::Malformed)?;
    let block_end = end_at + end.len();
    let trailing = &armored[block_end..];
    if trailing.trim().is_empty() {
        Ok(&armored[start..block_end])
    } else if trailing.contains("-----BEGIN PGP") {
        Err(PgpImportError::MultipleKeys)
    } else {
        Err(PgpImportError::Malformed)
    }
}

/// Require exactly one key from a bounded armor block.
fn one_key<T, E>(mut keys: impl Iterator<Item = Result<T, E>>) -> Result<T, PgpImportError> {
    let first = keys.next().ok_or(PgpImportError::Malformed)?.map_err(|_| PgpImportError::Malformed)?;
    if keys.next().is_some() {
        return Err(PgpImportError::MultipleKeys);
    }
    Ok(first)
}

type PublicDescription = (String, String, String, Option<String>);

fn describe_public(key: &SignedPublicKey) -> Result<PublicDescription, PgpImportError> {
    let fingerprint = key.fingerprint().to_string().to_uppercase();
    let key_id = key.legacy_key_id().to_string().to_uppercase();
    let user_id = primary_user_id(&key.details);
    let public_armor = key.to_armored_string(ArmorOptions::default()).map_err(|_| PgpImportError::Malformed)?;
    if public_armor.len() > MAX_PGP_KEY_BYTES {
        return Err(PgpImportError::TooLarge);
    }
    Ok((public_armor, fingerprint, key_id, user_id))
}

fn primary_user_id(details: &pgp::composed::SignedKeyDetails) -> Option<String> {
    details.users.first().and_then(|user| user.id.as_str()).map(|id| id.chars().take(256).collect())
}

/// Whether an armored block is a private key. Used by the CLI to decide whether to ask for a
/// passphrase without parsing OpenPGP.
pub fn is_private_armor(armored: &str) -> bool {
    armored.trim_start().starts_with(PRIVATE_ARMOR)
}
