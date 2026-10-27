# Brief — P12c PGP

> **Package:** P12c1 key store/ops, then P12c2 MIME/app. **Depends on:** P12c1 for P12c2.
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

## P12c2 — MIME and app

- Verify signed messages, decrypt encrypted messages, sign and encrypt outgoing drafts; preserve the
  exact signed bytes; explicit status badges and a locked-key reason; refuse to send unencrypted when
  the person asked to encrypt; no auto-decrypt of hostile/broken input.
- Operations/CLI for `message_pgp_verify`, `message_pgp_decrypt`, `draft_pgp_sign`,
  `draft_pgp_encrypt` (passphrase-sensitive where applicable; MCP exclusion); app reading badges and
  composer toggles; 980×600 light/dark; no new HTML path.
- Tests: test vectors, tamper/bad signature, wrong/missing key, locked key, wrong passphrase, MIME
  round-trip, exact signed bytes, no plaintext downgrade, no secret in logs, store v8 migration,
  CLI/MCP, 980 UI. No network.

## Limits

PGP is local only; no key discovery, no revocation fetch, no S/MIME, no HTML signing; key UX is a
settings table with import/delete, not a full key manager. Document the v8 migration and the
passphrase-per-use model.

## Report (max 15 lines per package)

What changed, files, package tests/result, dependency/ADR, limits, deviations.
