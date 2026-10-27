# Brief — P12c PGP

> **Package:** P12c1 key store/ops, P12c2a raw retention (v9), P12c2b MIME/core/CLI, P12c2c app UI.
> **Depends on:** P12c1, then P12c2a, then P12c2b, then P12c2c.
> **Review:** blind crypto/credential review each; one fix round each. **Parallel:** P12f fuzz only.

## Decisions

- `pgp = 0.21.0` (MIT OR Apache-2.0, pure Rust, `default-features = false`) is the only new
  dependency; review the transitive `argon2`/crypto crates and record it in the ADR.
- Private keys are passphrase-protected and stored only inside the SQLCipher store blob (schema
  **v8**); the passphrase is never stored, never in argv/logs/errors/screenshots. The OS keyring is
  not used for key secrets (Windows Credential Manager 2 560-byte limit).
- No keyserver, WKD, auto-fetch or cloud. No S/MIME. No silent plaintext downgrade.
- Decrypt/sign/verify statuses are explicit: Verified / Bad / Unknown / Locked / No key.
- Existing accounts and v1–v7 databases migrate normally; v8 rollback keeps the previous version.

## P12c1 — key store and operations

- Store v8: key rows with fingerprint, public/secret armored or binary bytes, protected flag,
  created/updated, account scope; secret material in SQLCipher (and still passphrase-protected).
  Bounds: 1 MiB per key, 64 keys/account, 256-byte passphrase, no unbounded armor parse.
- Operations: `pgp_keys_list` (read-only), `pgp_key_import` (sensitive; armored private/public key +
  passphrase from stdin/no-echo), `pgp_key_delete` (destructive), `pgp_key_export_public` (read-only,
  public only). Registry schemas/annotations; MCP excludes sensitive import; CLI must not accept the
  passphrase in argv; no key material in logs.
- Zeroise passphrases/temporary secret buffers; no `unwrap/expect/panic` outside tests; all parsing
  bounded; duplicate fingerprint refused; wrong passphrase leaves no row; store v8 migration and
  rollback tests.
- CLI `postino pgp keys list|import|delete|export-public`; MCP parity for non-sensitive ops; app UI
  key manager belongs to P12c2.

## P12c2a — raw retention (schema v9)

Received PGP/MIME cannot be verified from the current parsed body: sync drops the raw entity, so
exact signed bytes, CRLF and header folding are irrecoverable. Do not reconstruct them.

- Schema v9 adds nullable `messages.raw_blob_hash`; ingest stores the bounded raw message in the
  existing encrypted blob store only for `multipart/signed` or `multipart/encrypted`.
- GC must see the raw hash (`referenced_blob_hashes`/`blob_is_referenced`); migration v8→v9 and
  rollback; bounds from `max_message_bytes`.
- Old rows without raw bytes report `Not stored`; an explicit refresh may fetch again, never a fake
  verify. No schema work for outgoing mail: `queue_send` already journals exact bytes.

## P12c2b — MIME/core/CLI

- `crates/mime` stays structure-only: detect PGP/MIME, extract signed entity/signature/encrypted
  payload, and serialize RFC 3156 entities; no crypto in mime.
- Core uses `pgp` to verify/decrypt received raw bytes and sign/encrypt outgoing bytes. Operations:
  `message_pgp_verify`, `message_pgp_decrypt`; send options `PgpSendOptions` on `message_send`,
  `send_schedule`, `send_dry_run` instead of separate draft sign/encrypt ops (avoids a TOCTOU).
- Missing/revoked/expired recipient key aborts the send; no cleartext fallback. Decrypted bodies
  never overwrite `body_text`/`body_html` and never enter FTS. Passphrase per use from no-echo/stdin,
  zeroised, never argv/env/log; fixed errors; decrypt output re-parsed under existing MIME limits.
- Tests: exact-byte verify, one-byte tamper → Bad, CRLF preservation, unknown/locked/wrong
  passphrase, decrypt, sign, encrypt, sign+encrypt, v1–v9 migration/rollback, GC keeps raw, outbox
  blob equality, no-downgrade, log redaction, CLI/MCP. No network.

## P12c2c — app UI

- Reading badges (Verified/Bad/Unknown/Locked/Not stored), decrypt action, composer sign/encrypt
  toggles and key manager; 980×600 light/dark; decrypted HTML goes through the existing sanitizer,
  no WebView path. No keyserver/auto-fetch.

## Limits

PGP is local only; no key discovery, no revocation fetch, no S/MIME, no HTML signing; key UX is a
settings table with import/delete, not a full key manager. Document the v8 key migration, the v9 raw
retention migration, the `Not stored` state for pre-v9 received mail and the passphrase-per-use model.

## Report (max 15 lines per package)

What changed, files, package tests/result, dependency/ADR, limits, deviations.
