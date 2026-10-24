# Brief — M1 storage spike

> **Package:** P3a (spike, timeboxed) · **Depends on:** U0.5 · **Owner:** developer agent
> **Output:** a go/no-go report plus the smallest committed proof, not the full store

## Question

Can Postino get an encrypted, local SQLite store on the active `x86_64-pc-windows-gnu` host without
making the build fragile or the binary enormous?

## Constraints

- Rust 1.95+, Windows GNU host, MSYS2 MinGW on `PATH`. CI is `cargo xtask ci`.
- Encryption at rest is not optional: mail bodies and metadata must not be plaintext on disk.
- The key lives in the OS keyring, never in a config file, never in logs.
- No network dependency at build time beyond crates.io.
- The app must keep working with the in-memory demo while the store is wired in.

## Candidates to evaluate

1. `rusqlite` with `bundled-sqlcipher-vendored-openssl` (or `bundled-sqlcipher` if OpenSSL is already
   resolvable), WAL, `PRAGMA key`, migrations.
2. `libsql`/`turso` if it supports encryption and vendored builds on GNU.
3. A plain `rusqlite` with an application-level AEAD blob layer, only if 1 and 2 fail: document the
   threat-model gap (metadata, indexes, FTS leakage).

Also evaluate `keyring` (Windows Credential Manager backend) and whether it works in a headless test
without touching the real user vault (tests must not write real credentials).

## Deliverable

- A short `docs/store-spike.md`: candidates, commands, build time, binary size delta, test result,
  risks, recommendation.
- The smallest code that proves the choice: open an encrypted DB in a temp dir, write/read a row,
  reopen with the key, fail without the key, run one migration. Keep it behind a `postino-store`
  crate skeleton with a README and a layer entry, or a `#[cfg(test)]` proof if the crate would be
  premature. Do not wire it into the engine yet.
- `cargo xtask ci` green. No secrets in fixtures; use a random test key held in memory.

## Out of scope

Schema for messages/folders/drafts, repository API, sync state, migration runner beyond one table,
keyring UI. Those are P3b.

## Report format

Go/no-go, the exact dependency lines, measured build time and size, the failure modes observed on
this host, and what P3b should do first.
