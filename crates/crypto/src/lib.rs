//! Postino cryptography: the small, boring primitive set the store builds on.
//!
//! Two operations and one key type:
//!
//! - [`derive_key`] turns the 256-bit master key into a purpose-bound [`Subkey`] with HKDF-SHA256,
//!   a fixed salt ([`SALT`]) and a caller-chosen context label. Two labels never produce the same
//!   subkey, so a blob key cannot decrypt a different subsystem's ciphertext.
//! - [`seal`] encrypts with XChaCha20-Poly1305 and a fresh 24-byte nonce from the operating system's
//!   CSPRNG. The returned bytes are `nonce || ciphertext || tag`, one self-contained value.
//! - [`open`] authenticates with the caller's AAD and returns zeroising plaintext.
//!
//! # Key handling
//!
//! [`Subkey`] has a redacted `Debug`, deliberately does not implement `Clone`, and zeroises its 32
//! bytes when dropped. No error variant carries key material or plaintext: [`CryptoError`] messages
//! name the failure, never a byte.
//!
//! Two copies are outside this crate's reach and are documented in `docs/threat-model.md`:
//!
//! - the RustCrypto cipher keeps its own expanded key state while a call runs;
//! - `open` decrypts into a temporary buffer before the tag is checked, so a rejected ciphertext
//!   leaves a transient unauthenticated plaintext buffer to the allocator.
//!
//! Neither is persisted, logged or returned. Closing them needs a custom in-place construction and
//! is not worth the risk of hand-rolling the AEAD composition.
//!
//! # Nonces
//!
//! The nonce is random, not a counter. 192 random bits make a collision after 2^96 messages
//! impossible in practice, which is the same argument XChaCha20 is designed for; a counter would
//! need durable state and would fail catastrophically after a restore from backup.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

/// The HKDF salt. It is fixed for the whole product: the context label, not the salt, separates
/// subsystems. Changing it invalidates every derived subkey and therefore every stored blob.
pub const SALT: &[u8] = b"postino.v1";

/// Length of an XChaCha20-Poly1305 nonce in bytes.
pub const NONCE_LEN: usize = 24;

/// Length of the Poly1305 tag in bytes.
pub const TAG_LEN: usize = 16;

/// Longest accepted context label. Labels are short ASCII names such as `blob`.
const MAX_CONTEXT_LEN: usize = 64;

/// A 256-bit subkey derived from the store key.
///
/// There is no `Clone` and no byte accessor on purpose: a subkey exists to be used by [`seal`] and
/// [`open`] and must not be copied into a longer-lived place. The bytes are zeroised on drop.
pub struct Subkey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for Subkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Subkey(<redacted>)")
    }
}

/// Why a cryptographic call failed. Messages are safe to show a user and to log.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The OS random source refused to produce a nonce.
    #[error("the OS random source failed while drawing a nonce: {0}")]
    Rng(#[from] getrandom::Error),
    /// The context label is not a short printable ASCII name.
    #[error("an HKDF context label must be 1-{MAX_CONTEXT_LEN} printable ASCII characters")]
    InvalidContext,
    /// HKDF refused to expand into 32 bytes, which cannot happen for a valid SHA-256 HKDF.
    #[error("the subkey could not be derived")]
    Derive,
    /// The subkey was not 32 bytes. Only a programming error can reach this.
    #[error("the subkey is not a 256-bit key")]
    Key,
    /// The plaintext was too large for XChaCha20-Poly1305 (about 256 GiB).
    #[error("the plaintext is too large to seal")]
    Seal,
    /// The ciphertext did not authenticate: wrong key, wrong AAD or a modified blob.
    #[error("the ciphertext did not authenticate: wrong key, wrong associated data or damaged bytes")]
    Open,
    /// The sealed value is shorter than a nonce plus a tag, so it cannot be a sealed message.
    #[error("a sealed value needs at least {needed} bytes (nonce and tag); this one has {found}")]
    TooShort { needed: usize, found: usize },
}

/// Derive a purpose-bound subkey from the 256-bit master key.
///
/// `context` is a domain-separation label, for example `"blob"`. It is validated so a label cannot
/// be empty, oversized or contain control characters that would make two labels look alike in a log.
pub fn derive_key(master: &[u8; 32], context: &str) -> Result<Subkey, CryptoError> {
    if context.is_empty() || context.len() > MAX_CONTEXT_LEN || !context.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(CryptoError::InvalidContext);
    }
    let mut okm = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(SALT), master).expand(context.as_bytes(), &mut okm[..]).map_err(|_| CryptoError::Derive)?;
    Ok(Subkey(okm))
}

/// Seal `plaintext` under `key` and bind it to `aad`.
///
/// The output is `nonce || ciphertext`; the Poly1305 tag is the last 16 bytes of the ciphertext.
/// A fresh random nonce is drawn for every call, so sealing the same plaintext twice yields two
/// different values.
pub fn seal(key: &Subkey, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key.0[..]).map_err(|_| CryptoError::Key)?;
    let ciphertext = cipher.encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad }).map_err(|_| CryptoError::Seal)?;
    let mut sealed = Vec::with_capacity(NONCE_LEN.saturating_add(ciphertext.len()));
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// Authenticate and decrypt a value produced by [`seal`] with the same `key` and `aad`.
///
/// The plaintext is returned in a [`Zeroizing`] buffer: the caller never owns an unzeroised copy.
pub fn open(key: &Subkey, aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let Some(nonce_bytes) = sealed.get(..NONCE_LEN) else {
        return Err(CryptoError::TooShort { needed: NONCE_LEN + TAG_LEN, found: sealed.len() });
    };
    let Some(nonce) = <[u8; NONCE_LEN]>::try_from(nonce_bytes).ok() else {
        return Err(CryptoError::TooShort { needed: NONCE_LEN + TAG_LEN, found: sealed.len() });
    };
    let Some(ciphertext) = sealed.get(NONCE_LEN..) else {
        return Err(CryptoError::TooShort { needed: NONCE_LEN + TAG_LEN, found: sealed.len() });
    };
    if ciphertext.len() < TAG_LEN {
        return Err(CryptoError::TooShort { needed: NONCE_LEN + TAG_LEN, found: sealed.len() });
    }
    let cipher = XChaCha20Poly1305::new_from_slice(&key.0[..]).map_err(|_| CryptoError::Key)?;
    let plaintext = cipher.decrypt(&XNonce::from(nonce), Payload { msg: ciphertext, aad }).map_err(|_| CryptoError::Open)?;
    Ok(Zeroizing::new(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: [u8; 32] = [7u8; 32];
    const AAD: &[u8] = b"postino test aad";
    const PLAINTEXT: &[u8] = b"Allegato riservato: 4.800 euro + IVA";

    fn key(context: &str) -> Subkey {
        derive_key(&MASTER, context).expect("derive a subkey")
    }

    fn sealed() -> Vec<u8> {
        seal(&key("blob"), AAD, PLAINTEXT).expect("seal")
    }

    #[test]
    fn round_trips_the_plaintext() {
        let opened = open(&key("blob"), AAD, &sealed()).expect("open");
        assert_eq!(opened.as_slice(), PLAINTEXT);
    }

    #[test]
    fn a_tampered_ciphertext_or_tag_is_rejected() {
        let sealed = sealed();
        let last = sealed.len() - 1;
        for position in [NONCE_LEN, NONCE_LEN + 4, last] {
            let mut tampered = sealed.clone();
            tampered[position] ^= 0x01;
            let error = open(&key("blob"), AAD, &tampered).expect_err("a modified byte must not authenticate");
            assert!(matches!(error, CryptoError::Open), "{error}");
        }
    }

    #[test]
    fn a_wrong_key_is_rejected() {
        let sealed = sealed();
        let other = derive_key(&[9u8; 32], "blob").expect("another subkey");
        assert!(matches!(open(&other, AAD, &sealed).err(), Some(CryptoError::Open)));
    }

    #[test]
    fn a_different_context_label_is_a_different_key() {
        let sealed = sealed();
        let other = key("other");
        assert!(matches!(open(&other, AAD, &sealed).err(), Some(CryptoError::Open)));
    }

    #[test]
    fn a_wrong_aad_is_rejected() {
        let sealed = sealed();
        assert!(matches!(open(&key("blob"), b"other aad", &sealed).err(), Some(CryptoError::Open)));
        assert!(matches!(open(&key("blob"), &[], &sealed).err(), Some(CryptoError::Open)));
    }

    #[test]
    fn two_seals_of_the_same_plaintext_differ() {
        let first = sealed();
        let second = sealed();
        assert_ne!(first, second, "the nonce is not fresh");
        assert_ne!(&first[..NONCE_LEN], &second[..NONCE_LEN], "the nonce repeated");
        assert_eq!(open(&key("blob"), AAD, &first).expect("first").as_slice(), PLAINTEXT);
        assert_eq!(open(&key("blob"), AAD, &second).expect("second").as_slice(), PLAINTEXT);
    }

    #[test]
    fn a_short_sealed_value_is_refused() {
        for length in [0usize, 1, NONCE_LEN, NONCE_LEN + TAG_LEN - 1] {
            let bytes = vec![0u8; length];
            let error = open(&key("blob"), AAD, &bytes).expect_err("a short value cannot be a sealed message");
            assert!(matches!(error, CryptoError::TooShort { .. }), "{error}");
        }
    }

    #[test]
    fn the_debug_output_is_redacted() {
        let rendered = format!("{:?}", key("blob"));
        assert_eq!(rendered, "Subkey(<redacted>)");
        assert!(!rendered.contains("07"));
        let error = open(&key("blob"), AAD, b"not a sealed value at all").expect_err("failure");
        let text = error.to_string();
        assert!(!text.contains(MASTER.iter().map(|byte| format!("{byte:02x}")).collect::<String>().as_str()));
        assert!(!text.contains("Allegato"));
    }

    #[test]
    fn context_labels_are_validated() {
        for context in ["", " ", "blob\n", "bl\u{00e8}b"] {
            assert!(matches!(derive_key(&MASTER, context).err(), Some(CryptoError::InvalidContext)), "{context:?}");
        }
        let long = "a".repeat(MAX_CONTEXT_LEN + 1);
        assert!(matches!(derive_key(&MASTER, &long).err(), Some(CryptoError::InvalidContext)));
        assert!(derive_key(&MASTER, &"a".repeat(MAX_CONTEXT_LEN)).is_ok());
    }

    #[test]
    fn different_contexts_derive_different_keys() {
        // Two labels that both pass validation must still separate the key space; comparing
        // ciphertexts is the only public way to see it, so seal with one and open with the other.
        let first = key("blob");
        let second = key("blob2");
        let sealed = seal(&first, b"", b"x").expect("seal");
        assert!(matches!(open(&second, b"", &sealed).err(), Some(CryptoError::Open)));
    }

    #[test]
    fn seal_is_deterministic_only_in_its_output_length() {
        let first = seal(&key("blob"), b"", b"").expect("seal empty");
        assert_eq!(first.len(), NONCE_LEN + TAG_LEN);
        assert_eq!(open(&key("blob"), b"", &first).expect("open empty").len(), 0);
    }
}
