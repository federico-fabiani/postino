# postino-store

**Layer:** foundation. Depends on no other workspace crate; no UI toolkit.

Encrypted local store. The spike proof for P3a: a SQLCipher database with a 256-bit key supplied by
the caller, WAL, a fail-closed codec check and one migration runner. `docs/store-spike.md` holds the
measurements and the decision.

## Contract

- The key is a `StoreKey` (`[u8; 32]`). This crate never reads the OS keyring, the environment or a
  config file: the app stores the key in the keyring (`postino-platform`, P3b) and hands it over.
  Tests use a random in-memory key, so they cannot touch the real Windows Credential Manager.
- `StoreKey`'s `Debug` output is redacted. A key must never reach a log line.
- Open fails closed (`StoreError::NotEncrypted`) if the linked SQLite has no codec, instead of
  silently writing plaintext.
- Pages, WAL frames and the temp store are encrypted. SQLCipher is built with
  `SQLITE_TEMP_STORE=2`, so temporary tables stay in memory.
- A wrong key is reported as `WrongKeyOrCorrupt`: SQLCipher cannot tell the two apart, and the error
  does not pretend otherwise.
- Migrations are `MIGRATIONS`: index `n - 1` moves `PRAGMA user_version` from `n - 1` to `n`, each
  in its own transaction. A database from a newer build is refused.

## Not here yet (P3b)

Message, folder, draft, outbox and sync schemas; repository API; FTS5 indexes; blob storage; key
generation and the keyring round trip. The engine still runs on the in-memory demo.

## Tests

```sh
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
cargo test -p postino-store
```

The integration tests open a database in a temp dir, write and read a row, reopen with the same key,
refuse a wrong key and assert the file header is not `SQLite format 3`, nor does any file contain the
written plaintext.

## Build prerequisite (Windows GNU)

`rusqlite`'s `bundled-sqlcipher-vendored-openssl` builds OpenSSL from source. It needs MSYS2 `make`
and `perl` on `PATH`, and a parallel cold build:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
```

Without `MAKEFLAGS`, `openssl-src` builds serially on Windows: ~14 min cold instead of ~4.5 min.
