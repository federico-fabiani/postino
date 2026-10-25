# postino-crypto

**Layer:** foundation. Depends on no other workspace crate, no UI toolkit, no network, no store.

The small primitive set the store builds on: HKDF-SHA256 subkeys and XChaCha20-Poly1305 sealing.
`postino-store` derives a `blob` subkey from the store key and uses it for attachment bytes; nothing
else in the product calls this crate yet.

```
master key ──HKDF-SHA256("postino.v1", "blob")──▶ Subkey
plaintext + AAD ──XChaCha20-Poly1305(random 24-byte nonce)──▶ nonce || ciphertext || tag
```

## Contract

- **One subkey per purpose.** `derive_key(master, context)` uses HKDF-SHA256 with the fixed salt
  `postino.v1` and the context label as `info`. Labels are 1-64 printable ASCII characters; `"blob"`
  is the only one in use. Two labels never produce the same key, and changing the salt or a label
  makes every value sealed under it unreadable.
- **Random nonces, no counters.** `seal` draws a fresh 24-byte nonce from the OS CSPRNG for every
  call (`getrandom`). Two seals of the same plaintext differ. 192 random bits make a repeat
  impossible in practice; a counter would need durable state and would fail catastrophically after a
  restore from backup. A failing RNG is `CryptoError::Rng`, never a zero nonce.
- **AEAD with caller AAD.** `open` returns `CryptoError::Open` for a wrong key, a different AAD, or
  any modified byte of the nonce, ciphertext or tag. The error does not say which: that distinction
  is exactly what an attacker wants.
- **Keys never leak.** `Subkey` has no `Clone`, no byte accessor and a redacted `Debug`
  (`Subkey(<redacted>)`); its 32 bytes are zeroised on drop (`Zeroizing`). No error variant carries
  key material or plaintext; the tests assert that a `Debug` render and an error string contain
  neither.
- **Plaintext is returned zeroising.** `open` returns `Zeroizing<Vec<u8>>`, so the caller drops the
  bytes through a wiping buffer.
- **No `unwrap`/`expect`/`panic!` outside tests, no `unsafe`.** Every failure path is a typed error.

## What this crate does not claim

Three pieces of state are outside its reach and are recorded in `docs/threat-model.md` §4.3:

- `derive_key` builds an `Hkdf<Sha256>` value that holds the PRK-derived HMAC state until it drops.
  The crate exposes no way to wipe it, and hand-rolling HMAC-SHA256 to zeroise it would be a worse
  trade than the documented gap.
- the RustCrypto cipher expands the key into its own state for the duration of a call;
- `open` decrypts into a temporary buffer before the tag is verified, so a rejected ciphertext
  leaves a transient unauthenticated plaintext buffer to the allocator.

None of them is persisted, logged or returned. There is no key rotation, no key wrapping and no
password-based derivation. The master key is 32 random bytes from the OS keyring
(`postino-platform`), and this crate never reads a keyring.

## API

```rust
pub fn derive_key(master: &[u8; 32], context: &str) -> Result<Subkey, CryptoError>;
pub fn seal(key: &Subkey, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>;
pub fn open(key: &Subkey, aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError>;
```

`CryptoError`: `Rng`, `InvalidContext`, `Derive`, `Key`, `Seal`, `Open`, `TooShort { needed, found }`.
A value shorter than a nonce plus a tag is `TooShort` before any cipher work.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
cargo test -p postino-crypto
```

Eleven unit tests: round trip, a flipped ciphertext/tag/nonce byte, a wrong key, a different context
label, a wrong or empty AAD, two seals of the same plaintext (different nonce and bytes), short
values, redacted `Debug` and error text, context validation, and an empty plaintext.

## Dependencies

`chacha20poly1305` 0.10.1 (XChaCha20-Poly1305, RFC 8439 plus the extended-nonce construction),
`hkdf` 0.12.4 and `sha2` 0.10.9 for HKDF-SHA256, `getrandom` 0.3 for nonces, `zeroize` 1.9,
`thiserror` 2. All pure Rust; no C build of their own.
