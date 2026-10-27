# ADR-0007: OpenPGP keys in the encrypted store

- Status: accepted
- Date: 2026-10-27
- Scope: P12c1 (`postino-store`, `postino-core`, `postino-cli`, `postino-mcp`); P12c2 adds MIME
  verification, decryption, signing and encryption

## Context

P12c needs OpenPGP key handling without a keyserver, a cloud service, WKD, S/MIME, remote AI or a
plaintext downgrade. The P12c brief left the library open and required a written dependency review.
The product also has a hard rule that a credential secret never reaches disk in plaintext: the OS
keyring is for the store key and mail passwords, and the encrypted SQLCipher store is the only place
that can hold a key larger than a credential.

## Decision

- **Library: `pgp = "=0.21.0"`, `default-features = false`.** It is the only new direct
  dependency. The crate is dual licensed MIT OR Apache-2.0, pure Rust and exposes parsing, armor,
  fingerprints, S2K/passphrase unlocking and the packet types the MIME phase needs. `default-features
  = false` removes exactly one optional feature, `bzip2`; it does **not** remove `flate2`, which is a
  non-optional dependency of `pgp` and keeps the compressed-data packet path in the build. `pgp`
  requests `flate2` with `default-features = false, features = ["zlib-rs"]`; the workspace also builds
  `flate2` for `png`/`image`, so feature unification adds the default `rust_backend`
  (`miniz_oxide`/`crc32fast`) next to `zlib-rs`. Both backends are pure Rust, and neither `pgp` nor
  `flate2` compiles a C library. The resolved graph from `Cargo.lock` (2026-10-27) that `pgp` brings
  in is:
  - cryptography: `aead 0.5.2`, `aes 0.8.4`, `aes-gcm 0.10.3`, `aes-kw 0.2.1`, `argon2 0.5.3`,
    `blake2 0.10.6`, `blowfish 0.9.1`, `camellia 0.1.0`, `cast5 0.11.1`, `cfb-mode 0.8.2`,
    `crc24 0.1.6`, `curve25519-dalek 4.1.3`, `cx448 0.1.1`, `des 0.8.1`, `dsa 0.6.3`, `eax 0.5.0`,
    `ecdsa 0.16.9`, `ed25519-dalek 2.2.0`, `elliptic-curve 0.13.8`, `hkdf 0.12.4`, `hmac 0.12.1`,
    `idea 0.5.1`, `k256 0.13.4`, `md-5 0.10.6`, `num-bigint-dig 0.8.6`, `ocb3 0.1.0`,
    `p256`/`p384`/`p521 0.13.x`, `polyval 0.6.2`, `ripemd 0.1.3`, `rsa 0.9.10`, `sha1 0.10.7`,
    `sha1-checked 0.10.0`, `sha2 0.10.9`, `sha3 0.10.9`, `signature 2.2.0`, `subtle 2.6.1`,
    `twofish 0.7.1`, `universal-hash 0.5.1`, `x25519-dalek 2.0.1`;
  - support: `base64 0.22.1`, `bitfields 1.0.3` and `bitfields-impl`, `bytes 1.12.1`,
    `derive_builder 0.20.2`, `derive_more 2.1.1`, `flate2 1.1.10`, `zlib-rs 0.6.8`,
    `miniz_oxide 0.9.1`, `adler2 2.0.1`, `simd-adler32 0.3.10`, `crc32fast 1.5.3`, `log 0.4.34`,
    `nom 8.0.0`, `rand 0.8.8`, `rand_core 0.6.4`, `replace_with 0.1.8`, `smallvec 1.16.3`,
    `snafu 0.9.2` and `snafu-derive 0.9.2`, `zeroize 1.9.1` and `zeroize_derive 1.5.0`.
  - Licences: MIT OR Apache-2.0 for almost all of the above; BSD-3-Clause for `curve25519-dalek`,
    `ed25519-dalek`, `x25519-dalek`, `cx448` and `subtle`; `Zlib` for `zlib-rs`; MIT OR Zlib OR
    Apache-2.0 for `miniz_oxide`; 0BSD OR MIT OR Apache-2.0 for `adler2`. No copyleft, no C
    library, no network client and no key-server code. Versions come from the existing lockfile; no
    second cryptography stack is introduced. The dependency is pinned exactly so an armor or packet
    parser change is a deliberate upgrade.
- **Schema v8 adds `pgp_keys`:** `id`, `account_id` (foreign key, cascade delete), uppercase
  `fingerprint` (unique), `key_id`, optional `user_id`, `public_armor`, optional `secret_armor`,
  `protected`, `created_at`, `updated_at`. A `CHECK` ties `secret_armor` and `protected` together, so
  a row either has a passphrase-protected secret or no secret. The secret armor lives only inside
  SQLCipher and stays passphrase-protected even there. The store never returns secret columns to the
  engine, the CLI, MCP or a log line; `PgpKeyInsert` redacts them in `Debug`.
- **The keyring is not used for PGP secrets.** Windows Credential Manager caps a credential at 2560
  bytes, which cannot hold a typical armored private key, and the brief forbids a plaintext key file.
  The store key remains in the keyring; the key material is encrypted by SQLCipher below it.
- **Import verifies the passphrase before writing.** `parse_import` caps the input at 1 MiB, accepts
  exactly one armor block containing exactly one public or private key (`from_string_many` plus a
  footer scan that rejects a second block or trailing text), requires every private primary key and
  secret subkey to be encrypted, and calls `SecretKey::unlock` with a `Password` built from a
  zeroising buffer. A wrong or missing passphrase returns a fixed message and leaves no row. The
  passphrase is capped at 256 bytes, never stored, never placed in an operation result and never
  logged. Private armor is re-armored canonically; public armor is re-armored without the passphrase.
- **Duplicate fingerprints are refused**, across accounts, by the unique index on
  `pgp_keys.fingerprint`; only that named constraint is translated to `DuplicatePgpFingerprint`
  (and then to `InvalidArgs` at the operation boundary), while any other constraint failure stays a
  `Sqlite` error. Importing the same key again is a conflict, not an update. Each account is capped
  at 64 keys inside one immediate transaction, so the cap cannot race.
- **Operations follow the single registry:** `pgp_keys_list` (read-only, metadata only),
  `pgp_key_import` (sensitive, passphrase from stdin or a no-echo prompt), `pgp_key_delete`
  (destructive), `pgp_key_export_public` (read-only, public half only). MCP neither lists nor
  dispatches the sensitive import. The CLI has no `--passphrase` argument; `--passphrase-stdin`
  reads one bounded line, otherwise a TTY prompt is used.
- **Out of scope for P12c1:** message verification/signature status, decryption, signing, composer
  encryption, revocation refresh, keyserver/WKD discovery, S/MIME and any automatic key fetch. P12c2
  owns MIME and UI; this phase stores and manages keys only.

## Consequences

- A protected key can be listed and exported publicly without its passphrase. Decryption and signing
  will ask for the passphrase per use in P12c2; this phase does not cache it.
- A lost passphrase makes the secret unusable but leaves the public half exportable. Deletion removes
  the row and its secret in one store transaction.
- Migration v7→v8 is additive and runs in one transaction; a failed v8 keeps `user_version = 7` and
  leaves the v7 tables untouched. A successful v8 store is refused by an older binary as
  `SchemaTooNew`, which is the normal append-only contract; this phase ships no downgrade path.
- Limits: no key discovery, no automatic revocation import, no WKD, no keyserver, no S/MIME, no
  plaintext fallback, 1 MiB per key, 64 keys per account and a 256-byte passphrase. The settings
  surface remains a CLI/MCP contract in this phase; the app consumes it in P12c2.
