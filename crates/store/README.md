# postino-store

**Layer:** foundation. Depends on no other workspace crate; no UI toolkit.

The encrypted local store. One SQLCipher database with schema **v2**, a migration runner on
`PRAGMA user_version`, and a typed repository for the M1 mailbox. The engine drives it through
`Engine::open` (P3c): the store stays independent of `postino-core` and never reads a keyring.

`docs/architecture.md` describes the schema and key flow; `docs/store-spike.md` records why SQLCipher.

## Contract

- **The key is handed in.** `Store::open(path, &StoreKey)` takes 32 bytes. This crate never reads the
  OS keyring, the environment or a config file. `postino-platform` owns the keyring; tests use a
  random in-memory key, so they cannot touch the Windows Credential Manager.
- **`StoreKey` never leaks.** `Debug` is `<redacted>`, the bytes are zeroised on drop, and the
  SQLCipher pragma is built in a zeroising string. The key is not on disk: a test scans the database,
  WAL and SHM for the key bytes.
- **Fail closed.** A missing codec is `NotEncrypted`; a wrong key or damaged file is
  `WrongKeyOrCorrupt`; a database from a newer build is `SchemaTooNew`. There is no plaintext mode.
- **Pragmas on open:** `foreign_keys = ON`, `journal_mode = WAL`, `synchronous = FULL`, 5 s busy
  timeout. SQLCipher is built with `SQLITE_TEMP_STORE=2`, so temp tables stay in memory.
- **Migrations are append-only.** `MIGRATIONS[n - 1]` moves `user_version` from `n - 1` to `n`, each
  in its own transaction. Never edit a shipped migration; add the next one.
- **Dates are Unix milliseconds** (`i64`), not seconds and not text. Milliseconds match
  `SystemTime`, the demo clock and JavaScript; integers sort and index correctly; 64 bits have no
  2038 problem. IMAP/MIME dates are converted at the sync boundary.
- **Transactions.** Label delete + detach, label batch apply/remove, flag batch, message move,
  attachment replacement, draft save and any message upsert that changes folders run in one
  transaction. A bad id rolls the whole batch back.
- **Upserts preserve what they do not own.** `upsert_message` replaces the message row only; label
  assignments and attachment rows survive a sync refresh. `replace_attachments` is the explicit
  replacement and keeps `messages.has_attachments` in step.

## Schema v2

| Table | Purpose |
|---|---|
| `accounts` | id, name, email, provider, signature, created_at |
| `folders` | account, name, kind, cached unread/total, `uid_validity`, `uid_next`, `last_sync_at` |
| `messages` | envelope, JSON recipient lists, flags, body text/HTML, size, optional IMAP uid |
| `attachments` | metadata only: filename, mime, size, optional blob hash |
| `labels` | account-scoped or mailbox-wide, name, semantic colour name |
| `message_labels` | assignment join, `PRIMARY KEY(message_id, label_id)`, cascade both ways |
| `drafts` | revision, updated_at, body, recipients, subject, in_reply_to |
| `sync_state` | per-folder uid_next, highest_modseq, last_error (IMAP writers arrive with P5) |
| `meta` | key/value store from P3a |

Indexes cover the hot reads: `messages(folder_id, date DESC)`, `messages(account_id, date DESC)`,
`message_labels(label_id)`, `attachments(message_id)`, `drafts(updated_at DESC)` and
`folders(account_id)`. `labels` has `UNIQUE(account_id, name)` plus a partial unique index on
mailbox-wide names, because SQLite treats `NULL`s as distinct in a plain unique constraint.

## Repository

Plain data structs in `model.rs` (no `postino-core` dependency), methods in `repo.rs`:

- accounts: `upsert_account`, `list_accounts`, `get_account`.
- folders: `upsert_folder`, `list_folders`, `get_folder`, `recount_folder`.
- messages: `upsert_message`, `get_message`, `list_messages` (folder/label/all, unread/starred/
  attachment filters, limit/offset), `list_all_messages`, `counts`, `set_flags`, `move_messages`,
  `delete_message`.
- labels: `create_label`, `update_label`, `delete_label`, `get_label`, `list_labels` (live counts),
  `apply_label`, `remove_label`, `messages_by_label`, `labels_for`.
- attachments: `replace_attachments`, `attachments`, `list_all_attachments`.
- hydration: `list_all_messages` plus `list_all_attachments` load a whole mailbox in one query per
  table, with labels and attachments grouped by message; `Engine::open` uses exactly this pair.
- drafts: `save_draft` (revision++), `get_draft`, `list_drafts`, `delete_draft`.
- meta: `get_meta`, `set_meta`.

Folder `unread`/`total` are a cache maintained from `messages` on every mutation; `recount_folder`
repairs them after a bulk import. List totals describe the filtered scope before pagination.
`list_messages` clamps the page to 1..=500 rows.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
cargo test -p postino-store
```

- `tests/encryption.rs`: keyless reads, wrong key, reopen, no plaintext in DB or WAL.
- `tests/migrations.rs`: additive v1 → v2 upgrade with meta preserved, idempotent reopen,
  `SchemaTooNew`, foreign-key enforcement and cascade.
- `tests/repository.rs`: the full round trip (account, folders, message, two recipients, attachment,
  two labels), filters/paging, counts after read/move/delete, cached-count repair, atomic batches,
  label delete detach, draft revisions across reopen, reopen persistence, unknown-id errors and the
  no-plaintext scan over DB/WAL/SHM with repository data.

## Not here yet

Desktop-app wiring (P6 onboarding), IMAP sync writers, MIME parsing, FTS5, encrypted attachment blobs
(P4), outbox (P7), daemon (M4). `sync_state` has its table but no repository writer until P5. The
engine (P3c) is a client of this crate; this crate never depends on it.

## Build prerequisite (Windows GNU)

`rusqlite`'s `bundled-sqlcipher-vendored-openssl` builds OpenSSL from source. It needs MSYS2 `make`
and `perl` on `PATH`, and a parallel cold build:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
```

Without `MAKEFLAGS`, `openssl-src` builds serially on Windows: ~14 min cold instead of ~4.5 min.
