# Brief — P4b encrypted attachment blobs

> **Package:** P4b · **Depends on:** P4a (MIME), P3b (store schema) · **Next:** P5 IMAP
> **Owner:** developer agent · **Review:** blind engineering
>
> Attachment bytes are the largest and most sensitive payload in a mailbox. They do not belong in
> the SQLCipher page store: they are write-once, content-addressed and streamed. This package
> builds that layer and the engine seam; no user-facing operation ships until sync ingests real
> attachments (P5b) and the reading pane offers Save (P6).

## Outcome

1. A new foundation crate `postino-crypto`: XChaCha20-Poly1305 sealing and HKDF subkeys.
2. `postino-store` gains a content-addressed blob store on disk, encrypted with a subkey derived
   from the store key, deduplicated by BLAKE3, with bounded reads and atomic writes.
3. `postino-core` can persist and read attachment bytes through that store, surviving a reopen.
4. No plaintext and no key material on disk; no path traversal from a hash; no unbounded
   decompression.

## 1. `postino-crypto` (foundation)

```rust
pub fn derive_key(master: &[u8; 32], context: &str) -> Result<Subkey, CryptoError>;
pub fn seal(key: &Subkey, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>;
pub fn open(key: &Subkey, aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError>;
```

- HKDF-SHA256, fixed salt `postino.v1`, `context` is a domain-separation label (`"blob"`).
- XChaCha20-Poly1305 with a random 24-byte nonce from the OS RNG; output is `nonce || ciphertext`.
- `Subkey` has a redacted `Debug`, no `Clone`, and zeroises on drop. No key in an error message.
- Errors are readable (`Rng`, `Open`, `TooShort`), never leak plaintext.
- Tests: round trip, tampered ciphertext/tag fails, wrong key fails, wrong AAD fails, two seals of
  the same plaintext differ (nonce), `Debug` is redacted.
- Layer entry, README. No dependency on other Postino crates.

## 2. Blob store in `postino-store`

```rust
pub struct BlobStore { /* root + subkey */ }
impl BlobStore {
    pub fn open(root: impl Into<PathBuf>, key: &StoreKey) -> Result<Self, StoreError>;
    pub fn put(&self, bytes: &[u8]) -> Result<String, StoreError>;       // hex BLAKE3, dedup
    pub fn get(&self, hash: &str, max_bytes: u64) -> Result<Zeroizing<Vec<u8>>, StoreError>;
    pub fn has(&self, hash: &str) -> Result<bool, StoreError>;
    pub fn remove(&self, hash: &str) -> Result<bool, StoreError>;
}
```

- `Store::open` creates/opens the blob directory as a sibling of the database
  (`<db parent>/blobs`), one directory shared by every store in that folder. Document the layout.
  A failure to create it fails `Store::open` rather than silently losing attachments later.
- Layout: `blobs/<first two hex chars>/<hash>.blob`; the hash is validated as 64 lowercase hex
  before any path is built (`..`, separators, upper case, short/long → `StoreError::BadHash`).
- File format: magic `PSTB`, version `1`, flags, `original_len: u64` LE, 24-byte nonce, ciphertext.
  AAD = magic || version || plaintext length || hash bytes, so a blob cannot be swapped with another
  and a truncated header cannot be accepted.
- Compress before sealing with zstd **only when it helps** (flag bit), and always enforce
  `max_bytes` against the declared `original_len` before decompressing: a 1 KiB file claiming
  10 GiB fails with `TooLarge`, and decompression uses a bounded buffer, never `decompress_all`
  without a cap.
- Atomic: write to a unique `.tmp-<random>` in the target directory with `create_new`, `sync_all`,
  then rename. A concurrent put of the same hash is a no-op success. A failed write removes its temp.
- Store errors are specific (`BadHash`, `BlobTooLarge`, `BlobMissing`, `CorruptBlob`, `Io`) and
  never contain plaintext.
- Tests: round trip, dedup (same bytes → same hash, one file), reopen with the same key, wrong key
  fails, tampered byte fails, swapped hash fails, oversized declarad length refused, non-hex hash
  refused, path traversal refused, temp files cleaned, concurrent puts of the same content succeed,
  and the blob file plus any WAL contains none of the plaintext substrings.

## 3. Engine seam

- `Engine` keeps the `Store` it already owns; add `attachment_bytes(&self, attachment_id: &str) ->
  Result<Vec<u8>, OpError>` and `store_attachment_bytes(&mut self, message_id: &str,
  attachment_id: &str, bytes: &[u8]) -> Result<(), OpError>` used by sync (P5b) and the reading pane
  (P6). Read path caps at the attachment's recorded size + 1 MiB slack; unknown attachment is a
  readable error.
- The write path updates the attachment row's `blob_hash` in the store and the in-memory
  attachment, in that order, and leaves both untouched on failure. A message's `has_attachments`
  stays truthful.
- Do not add operations, CLI subcommands or UI. This is plumbing with tests, and the report must say
  which package consumes each entry point.
- Demo engine (`store: None`) returns a readable error for both calls, like other store-only paths.

## 4. Tests

- Store-level unit/integration tests from §2, including the no-plaintext byte scan over the blob
  files and the database.
- Engine round trip: open a temp store, insert a message with two attachments, store bytes for one,
  drop the engine, reopen, read the bytes back and compare; `blob_hash` and `has_attachments`
  survive; reading an unknown attachment or the demo engine is an `OpError`, not a panic.
- A corrupted blob file (one flipped byte) surfaces as `CorruptBlob`/`Failed` and does not poison
  the message or the engine.
- Performance smoke: store and read a 4 MiB blob once, assert it completes (no artificial time
  assertion that can flake; just exercise the path).

## 5. Docs and gates

- `docs/architecture.md`: blob layout, header, AAD, compression rule, key derivation, what is
  encrypted where; note that message bodies still live in the encrypted DB in M1 and why.
- `crates/crypto/README.md` and `crates/store/README.md` updates.
- `docs/threat-model.md` §4.3: state exactly what is encrypted and what is not (swap, crash dumps,
  SQLCipher's internal key copy).
- `docs/store-spike.md`: append the blob outcome.
- README: one honest line (attachment bytes are encrypted at rest; the app does not ingest real
  mail yet).
- `cargo xtask ci` green, layers include `postino-crypto`, clippy `-D warnings`.

## 6. Constraints

- No IMAP, MIME parsing, UI, CLI or operation-registry changes.
- No `unwrap`/`expect`/`panic!` outside tests, no `unsafe`, rustfmt 150.
- New dependencies: `chacha20poly1305`, `hkdf`, `sha2`, `blake3`, `zstd` (bounded API only). Pin in
  the workspace manifest; report build cost.
- Temp files only in `%TEMP%\opencode\p4b-postino`; never delete outside it.
- Do not commit.

## 7. Report

Files, dependency lines and build cost, file format with a hexdump of a header, proof of dedup and
no-plaintext, tests, and any deviation with the reason.
