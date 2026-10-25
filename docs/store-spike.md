# Store spike — encrypted SQLite on Windows GNU

> **Package:** P3a · **Date:** 2026-10-25 · **Verdict:** **go**, with one added build prerequisite
> and a known binary cost.

## Question

Can Postino get an encrypted, local SQLite store on the active `x86_64-pc-windows-gnu` host without a
fragile build or an enormous binary? Encryption at rest is not optional: bodies and metadata must not
be plaintext on disk, and the key lives in the OS keyring.

## Answer

Yes. `rusqlite` 0.41 with `bundled-sqlcipher-vendored-openssl` compiles a self-contained SQLCipher
4.19.0 (OpenSSL 3.6.3, static) with no runtime DLLs, passes the encryption proof, and costs
**+8.1 MB** over plain SQLite and **4.4 min** for a cold build when `make` runs parallel. The
committed proof is `crates/store`: open with a 256-bit key, write/read, reopen, refuse a wrong key,
run one migration.

Everything below was measured on this host: Windows, `x86_64-pc-windows-gnu`, rustc 1.98.1, MSYS2
gcc 16.2.0, make 4.4.1, perl 5.42.3.

## Candidates

| # | Candidate | Build on this host | Cold build | Release exe | Verdict |
|---|---|---|---|---|---|
| 1a | `rusqlite` + `bundled-sqlcipher-vendored-openssl` | ok after `pacman -S make` | 4m25s (`MAKEFLAGS=-j16`), 13m56s serial | 11,128,864 B | **chosen** |
| 1b | `rusqlite` + `bundled-sqlcipher` (system OpenSSL) | ok with `OPENSSL_DIR` | 57.1 s | 3,161,384 B + 5,495,630 B DLL | rejected: runtime DLL |
| 2 | `libsql` 0.10 (`core`, `encryption`) | failed | 21.8 s to failure | — | rejected: needs cmake + generator |
| 3 | `rusqlite` + AEAD blob layer | ok | 52.5 s | 2,984,774 B | rejected: plaintext metadata |
| — | empty binary, same profile | — | 3.9 s | 396,460 B | baseline |

### 1a — SQLCipher with vendored OpenSSL (chosen)

```toml
rusqlite = { version = "0.41", default-features = false, features = ["bundled-sqlcipher-vendored-openssl"] }
```

Cold build **without** `make` fails after 40.6 s, before any Rust is compiled:

```
running "make" "depend"
cargo:warning=building OpenSSL dependencies: Command 'make' not found. Is make installed?
cargo:warning=openssl-src: failed to build OpenSSL from source
Error building OpenSSL dependencies:
    Command failed: "make" "depend"
```

`perl` was already installed but not on `PATH`; add `C:\msys64\usr\bin`. With both on `PATH`:

- cold, serial (default): **13m56s** — `openssl-src` 300.6.1 does not forward `CARGO_MAKEFLAGS` on
  Windows, so `make` runs with one job.
- cold, `MAKEFLAGS=-j16`: **4m25s** (265.4 s), same 11,128,864 B binary.
- warm rebuild after editing the probe: **3.7 s**.
- the repo crate's first `cargo test -p postino-store` (dev profile, OpenSSL, `-j16`): **276.7 s**.

Runtime: no non-system DLL imports; the exe runs with `PATH=C:\Windows\System32` only. SQLCipher is
statically linked, so there is no `libcrypto-3-x64.dll` to ship or to lose.

Crypto proof (probe, release build):

```
cipher=4.19.0 community
journal_mode=wal
created rows=1
first16=e4 4f 64 b7 3b 34 1e be 3d b8 1f 72 1c b6 62 82
size=8192
reopened rows=1
migrated user_version=1
without_key=Some("file is not a database")
```

`SQLite format 3\0` is `53 51 4c 69 74 65 20 66 6f 72 6d 61 74 20 33 00`; the encrypted header is a
random per-database salt. FTS5 is compiled in (`SQLITE_ENABLE_FTS5`) and works on the encrypted
database (`fts5_hits=1`), which P9 needs.

One API trap worth recording: `PRAGMA key = ?` with a **bound parameter** is silently ignored when
the file already exists, because SQLCipher reads the pragma at prepare time. The store therefore
uses the raw-key literal form, `PRAGMA key = "x'<64 hex>'"`, which needs no escaping because the
input is hex. Two tests pin it.

### 1b — SQLCipher with the system OpenSSL (rejected)

```toml
rusqlite = { version = "0.41", default-features = false, features = ["bundled-sqlcipher"] }
# plus $env:OPENSSL_DIR = "C:\msys64\mingw64"
```

Builds in **57.1 s** and encrypts correctly, but `libsqlite3-sys` links `libcrypto` as a DLL on
Windows. The exe imports `libcrypto-3-x64.dll` and dies with `-1073741515` (`0xC0000135`,
`STATUS_DLL_NOT_FOUND`) when the MSYS2 `bin` directory is not on `PATH`. Shipping means bundling a
5.5 MB third-party DLL and making `OPENSSL_DIR` a build requirement. Rejected: it trades a one-time
build cost for a permanent packaging and support cost.

### 2 — libsql / turso (rejected)

```toml
libsql = { version = "0.10", default-features = false, features = ["core", "encryption"] }
```

`libsql`'s `encryption` feature maps to `libsql-ffi/multiple-ciphers` (SQLite3MultipleCiphers), which
is a good crypto choice, but `libsql-ffi` builds it with **cmake**. It fails in 21.8 s:

```
error: failed to run custom build command for `libsql-ffi v0.10.0`
thread 'main' panicked at cmake-0.1.58/src/lib.rs:1132:5:
no valid generator found for GNU toolchain; MSYS or MinGW must be installed
```

There is no `cmake`, `ninja` or `make` on the host by default, so this candidate needs more host
tooling than 1a, plus the whole async `libsql` stack for a local file. Rejected.

`turso` 0.9.0-pre.1 was inspected, not built: it exposes `Builder::with_encryption(EncryptionOpts)`
and `experimental_encryption(true)` behind a `pure-rust-crypto` feature. A pre-release engine whose
own API calls the encryption experimental is not a defensible base for a mail store in v0.1. Keep it
on the watch list: it is the obvious way to drop 8 MB and the C toolchain if it stabilises.

### 3 — plain SQLite plus an AEAD blob layer (rejected)

```toml
rusqlite = { version = "0.41", default-features = false, features = ["bundled"] }
```

This is the cheapest build (52.5 s, 2,984,774 B) and it would encrypt message bodies, but the file
header is `SQLite format 3\0` and the schema, indexes, subjects, addresses, dates, flags, folder
names and FTS5 terms stay readable. Encrypting every column individually still leaks row counts,
sizes and timing, and destroys indexed search because the index terms are the plaintext. That
contradicts `docs/threat-model.md` §4.3 ("SQLCipher (or equivalent AEAD page encryption) for the
metadata DB") and the brief's "metadata must not be plaintext". Rejected on the requirement, not on
effort.

## keyring (Windows Credential Manager)

`keyring` 4.2.0 with `windows-native-keyring-store` 1.1.0 works headless: a 32-byte secret round
trips through the real vault and deletes cleanly (`store_status=Ok(())`, `roundtrip_equal=true`,
build 12.6 s). The probe used a throwaway service name and deleted the credential.

Tests must not touch the real vault. The `keyring` 4 "v1" wrapper installs the native store on first
use, so a test cannot replace it. The app should depend on `keyring-core` 1.0 directly:

```toml
[dependencies]
keyring-core = "1"

[target.'cfg(windows)'.dependencies]
windows-native-keyring-store = "1"
```

Production calls `keyring_core::set_default_store(windows_native_keyring_store::Store::new()?)`; a
test calls `keyring_core::set_default_store(keyring_core::mock::Store::new()?)` and the same code
path then runs in memory. Both round trips were verified (`mock_roundtrip=true`,
`native_roundtrip=true`). The real key flow is exercised later by a manual, tagged smoke test with a
per-run service name and a delete-in-guard teardown; it never joins the default suite.

## Size and time budget

| Measure | Value |
|---|---|
| empty release exe, same profile | 396,460 B |
| plain SQLite release exe | 2,984,774 B |
| encrypted release exe | 11,128,864 B |
| encryption delta | **+8,144,090 B (×3.73)** |
| cold build, serial | 13m56s |
| cold build, `-j16` | 4m25s |
| warm build | 3.7 s |
| crate test binary (debug, integration) | 50,492,769 B |

A 11 MB floor for the store client is acceptable for a native mail app; the 4.4 min cold build is a
one-time, cacheable cost. Both are single numbers to monitor, not blockers.

## Risks

1. **Build prerequisite.** `make` and `perl` must be on `PATH`; `make` is not in the documented
   toolchain yet (`docs/development.md` is updated by this package). Without `MAKEFLAGS`, cold builds
   serialise for ~14 min. CI should set `MAKEFLAGS=-j%NUMBER_OF_PROCESSORS%`.
2. **Binary size.** +8.1 MB, almost all static OpenSSL. If it becomes a problem, the libtomcrypt
   crypto provider or a future stable `turso` are the exits; do not shrink by dropping page
   encryption.
3. **Key lifetime.** `StoreKey` is redacted in `Debug`, but keys and SQLCipher's internal copy are
   not zeroised yet. P3b owns `zeroize` and the keyring boundary.
4. **Wrong key vs corruption.** SQLCipher cannot distinguish them; `StoreError::WrongKeyOrCorrupt`
   says so instead of guessing.
5. **Supply chain.** SQLCipher 4.19.0 community + OpenSSL 3.6.3 + `sqlcipher-src` are new C sources;
   `cargo-deny`/`cargo-audit` and the asset/licence gates still apply. The build logs an SQLCipher
   ERROR line to stderr on a wrong key (page number only, no data).
6. **Not wired in.** The engine still uses the in-memory demo. This is a proof, not the M1 store.

## Recommendation

Adopt **SQLCipher via `rusqlite` 0.41 with `bundled-sqlcipher-vendored-openssl`**, key handed in by
the app, `journal_mode=WAL`, `synchronous=FULL`, `SQLITE_TEMP_STORE=2` (in memory), one migration
runner on `user_version`. Keep `keyring-core` + `windows-native-keyring-store` for the real key and
`keyring_core::mock` for tests. Do not adopt libsql, and do not fall back to column-level AEAD.

## P3b outcome

> **Package:** P3b · **Date:** 2026-10-25 · **Verdict:** the spike's recommendation shipped, with the
> keyring boundary and the zeroise gap closed as far as user-space allows.

What changed since the spike:

- **`crates/platform`** now owns the keyring. `load_or_create_key(scope)` reads 32 bytes from the
  Windows Credential Manager (`service = "postino"`, `user = scope`), creates them with `getrandom`
  on first run, and fails closed when the vault is unavailable. It depends on `keyring-core` 1.0.1
  plus `windows-native-keyring-store` 1.1.0 (the split the spike recommended), so tests install
  `keyring_core::mock` and the same code path runs in memory. No test touches the real vault. The
  default suite covers idempotent creation, reload, a fresh handle, scope isolation, a missing
  vault, a read failure and a wrong-length secret; the native path remains a manual smoke test.
- **Key lifetime, risk 3.** `StoreKey` in both crates has no `Clone`, redacts `Debug`, and zeroises
  its bytes on drop (`zeroize` 1.9). The raw-key pragma is built inside a zeroising `String` sized
  for its exact final length, so formatting never reallocates and leaves an unzeroised copy behind.
  The SQLCipher connection keeps its own copy of the key internally; that copy is outside our reach
  and is still a documented gap, not a solved one.
- **Schema v2** in `postino-store`: accounts, folders, messages, attachments, labels,
  message_labels, drafts and sync_state, additive from the v1 `meta` table. Foreign keys on, WAL,
  `synchronous = FULL`, 5 s busy timeout, Unix-millisecond dates (rationale in the crate docs).
  The migration runner was extended, not rewritten: existing v1 files upgrade in place and keep
  their meta rows.
- **Repository** with plain structs and transactional batches (labels, flags, move, attachments,
  drafts). Message upserts preserve labels and attachments; replacement is explicit.
- **No plaintext, now with real rows.** The P3a canary test covered `meta`. P3b writes an account,
  folder, message, recipient, label, attachment metadata and draft with canaries, scans the database
  and its WAL/SHM for all of them, and also asserts that the store key itself does not appear in the
  file bytes. Wrong key and schema-too-new still fail closed.
- **Not wired in.** The engine, CLI and MCP still run the in-memory demo. P3c maps the operations;
  IMAP writers for `sync_state`, FTS5 and blob files are later packages.

The build cost did not move: `keyring-core`, `windows-native-keyring-store` and `zeroize` are small
Rust crates, and the warm `postino-platform` + `postino-store` test suite runs in well under a
second. SQLCipher/OpenSSL still dominate a cold build, exactly as measured above.
