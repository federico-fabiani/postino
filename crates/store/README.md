# postino-store

**Layer:** foundation. Depends on `postino-crypto` (the blob subkey) and on no other workspace crate;
no UI toolkit.

The encrypted local store. One SQLCipher database with schema **v9**, a migration runner on
`PRAGMA user_version`, a typed repository for the mailbox and outbox, and a content-addressed encrypted
blob store for attachment bytes. The engine drives it through `Engine::open` (P3c), the attachment
seam (P4b) and the outbox (P7); `postino-sync` (P5b) is the writer for received mail and P7's Sent
serialization. The store stays independent of `postino-core` and never reads a keyring.

`docs/architecture.md` §4.1-§4.5 describe the schema, key flow and blob format;
`docs/store-spike.md` records why SQLCipher and how the blobs were added.

## Contract

- **The key is handed in.** `Store::open(path, &StoreKey)` takes 32 bytes. This crate never reads the
  OS keyring, the environment or a config file. `postino-platform` owns the keyring; tests use a
  random in-memory key, so they cannot touch the Windows Credential Manager.
- **The key never leaks.** `Debug` is `<redacted>`, the bytes are zeroised on drop, and the SQLCipher
  pragma is built in a zeroising string. The key is not on disk: a test scans the database, WAL and
  SHM for the key bytes.
- **Blobs are a sibling directory named after the file.** `Store::open` creates and opens
  `<db file>.blobs` (`mail.db` → `mail.db.blobs`) and derives the blob subkey with HKDF-SHA256
  (salt `postino.v1`, label `blob`). A directory that cannot be created fails the open instead of
  losing attachments later. The namespace belongs to one database file, so two stores in one folder
  never share it. Moving a store means moving the database and its `.blobs` directory together;
  renaming only one of them hides the attachments (`BlobMissing` on read).
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
  attachment replacement, whole-draft save and any message upsert that changes folders run in one
  transaction. A bad id rolls the whole batch back. The sync bulk path
  (`upsert_synced_message`) writes the message and its attachment rows in one transaction but
  deliberately skips the folder recount, because a full count per message would make a large import
  quadratic; sync recounts once per folder. `reset_folder_messages` and `delete_synced_messages` are
  one transaction each and refresh counts themselves.
- **Upserts preserve what they do not own.** `upsert_message` replaces the message row only; label
  assignments and attachment rows survive a sync refresh. `save_draft_row` keeps the stored
  `in_reply_to` when the update passes `None`; clearing it needs an explicit field, which does not
  exist yet. `replace_attachments` is the explicit replacement and keeps
  `messages.has_attachments` in step; for an attachment id that survives it keeps both the stored
  `blob_hash` and the stored `size`, because the bytes on disk are canonical and a stale caller size
  would shrink the read cap below the blob.

## Blobs

The full format is in `docs/architecture.md` §4.5. The essentials:

```
mail.db                              SQLCipher database
mail.db.blobs/<first two hex chars>/<64 lowercase hex>.blob
  PSTB | version 1 | flags | original_len u64 LE | 24-byte nonce | ciphertext || tag
```

- `BlobStore::open(root, &StoreKey)` derives the subkey; `put(bytes)` returns the lowercase BLAKE3
  hex of the plaintext. `get(hash, max_bytes)` authenticates, verifies the content hash and returns
  `Zeroizing<Vec<u8>>`; `has` and `remove` are the other two operations.
- The hash is validated as exactly 64 lowercase hex characters before any path is built. `..`,
  separators, Windows path and stream forms, trailing dots/spaces, uppercase hex and short/long
  values are `BadHash`; no path is ever assembled from an unvalidated value, and the error reports
  only the length, never the rejected string.
- The AAD is `magic || version || flags || original_len || hash bytes`. A blob cannot be renamed to
  another hash, truncated, extended, re-flagged or swapped with another. A wrong key is the same
  `CorruptBlob` as a flipped byte, because an attacker must not learn which one they achieved.
- `get` also recomputes BLAKE3 over the decrypted bytes and compares them with the name: a payload
  sealed under a false name by someone who has the subkey is `CorruptBlob`, not content.
- zstd level 3 is used only when the compressed frame is smaller than the plaintext (flag bit 0).
  Reads check the declared length against the caller's `max_bytes` **before** decryption, cap it at
  `MAX_BLOB_BYTES` (64 MiB), and decompress with `zstd::bulk::Decompressor` into a buffer of exactly
  the declared length. `decompress_all` is never used.
- Dedup does not trust the name. `put` authenticates and compares an existing file before it reports
  a dedup hit; a corrupt or foreign file at that name is replaced by the atomic write, so `put` is
  also the heal path. There is no separate "repair" operation. The cost is one authenticated read of
  the existing file (bounded by the candidate length), which is the trade for a namespace that can
  heal itself.
- Writes go through `.tmp-<random>` with `create_new` and `sync_all`, then publish with `hard_link`:
  the final name appears atomically and an existing name is never removed, so a concurrent reader
  sees the old complete file or the new one, never a gap. A name that already holds the same bytes is
  a dedup hit; a name with invalid or foreign bytes is healed with a replacing rename. A filesystem
  without hard links falls back to that rename for every publish, and `get` retries a momentarily
  missing name for a few milliseconds before reporting `BlobMissing`. A failed write removes its temp.
- After a publish the shard directory is fsynced on Unix; on Windows the published file is reopened
  and flushed, which leaves a microsecond directory-entry window after the publish. The failure mode
  is a `BlobMissing` on the next read, never wrong bytes, and the engine commits the attachment row
  only after the put returns.
- Errors: `BadHash`, `BlobMissing`, `CorruptBlob`, `BlobTooLarge`, `BlobRoot`, `Io`, `Compression`,
  `Random`, `Crypto`. None carries plaintext or key material.
- The blob directory has no garbage collector yet: deleting a message leaves its blob, because
  another message may reference the same content.

## Schema v9

v9 is additive from v8: nullable `messages.raw_blob_hash`, a reference into the encrypted blob store
for the exact received bytes of a `multipart/signed` or `multipart/encrypted` message. `postino-sync`
writes a blob first and names it afterwards, exactly like an attachment; ordinary mail leaves the
column NULL. NULL is the honest `Not stored` state for every row cached before v9 and for ordinary
mail, and nothing re-derives the bytes from the parsed body. `Store::set_message_raw_blob_hash` writes
or clears the reference and `Store::message_raw_blob_hash` reads it. A failed v9 keeps `user_version =
8`; an older binary refuses a v9 file as too new.

## Schema v8

v8 is additive from v7: the `pgp_keys` table (`id`, `account_id` foreign key with cascade delete,
uppercase `fingerprint` with a unique index, `key_id`, optional `user_id`, `public_armor`, optional
`secret_armor`, `protected`, `created_at`, `updated_at`) and an account/date index. A `CHECK` permits
secret armor only on a protected row. The secret column lives only inside SQLCipher and remains
passphrase-protected; import writes it and no read path (list, get, export) selects it again, and
`PgpKeyInsert` redacts it in `Debug`.
Import rejects a duplicate fingerprint, caps a key at 1 MiB, a passphrase at 256 bytes and an account
at 64 keys in one immediate transaction. No passphrase and no plaintext secret is stored. The
library decision is ADR-0007.

## Schema v7

v7 is additive from v6: derived search columns on `messages` (`search_sender`, `search_to`,
`search_cc`, `search_bcc`, `attachment_names`, `subject_norm`, `in_reply_to`, `references_json`,
`ingested_at`, `rules_applied_at`), the contentless-delete `message_fts` index and rank table with
their triggers, and the `threads`, `thread_aliases`, `saved_searches`, `rules`, `rule_runs` and
`rule_actions` tables. P9c extended the unshipped v7 in place: `messages.origin`
(`received`/`sent`/`draft`/`local`, default `received`) is written on insert and on the draft→Sent
transition, never by a move; `rules.name_key` is the Unicode lowercase name used by a unique
`rules(account_id,name_key)` index because SQLite `NOCASE` is ASCII-only. `Store::open` adds both
columns and backfills existing rows idempotently. P9b uses the conversation cache and saved searches;
P9c uses the rule tables, their journal and the candidate indexes
(`messages(account_id,origin,rules_applied_at,ingested_at,id)` for `new`,
`messages(account_id,origin,ingested_at,id)` for `all`, plus
`threads(account_id,last_date DESC,id)`, `messages(account_id,message_id)`,
`messages(account_id,subject_norm,date)`, `thread_aliases(account_id,new_id)` and the unique
`rules(account_id,name COLLATE NOCASE)`) so an older v7 store gains them without a version bump; a
read-only open never writes them.

## Schema v6

v6 is additive from v5: `messages.remote_images INTEGER NOT NULL DEFAULT 0 CHECK(remote_images
BETWEEN 0 AND 10000)`, the number of `http`/`https` images the MIME sanitizer removed from the stored
HTML on receipt. Existing rows read `0`; the column is a read-only derived fact, never accepted from a
draft or an agent. The repository clamps at 10 000 on both write and read, so a damaged row cannot
produce an unbounded count.

v5 was additive from v4: nullable `smtp_host`, `smtp_port`, `smtp_tls`, `smtp_username` on
`account_servers`, and the `outbox` journal with `outbox_due(state, send_at)`, `outbox_draft(draft_id,
created_at DESC)` and the partial unique index `outbox_active_draft(draft_id) WHERE state IN
('queued','sending','append_pending','uncertain','failed')`. There is still no password column: the
IMAP and optional SMTP secrets live in the OS keyring.

| Table | Purpose |
|---|---|
| `accounts` | id, name, email, provider, signature, created_at |
| `account_servers` | IMAP endpoint, plus the nullable SMTP endpoint (P7) |
| `folders` | account, name, kind, cached unread/total, `uid_validity`, `uid_next`, `last_sync_at` |
| `messages` | envelope, JSON recipient lists, flags, body text/HTML, stored blocked-image count, size, optional IMAP uid, derived search text, threading headers, provenance `origin` (P9a/P9b/P9c) and the optional raw PGP/MIME blob hash (P12c2a) |
| `message_fts` | contentless-delete FTS5 index over the search columns, keyed by `messages.rowid` (P9a) |
| `message_fts_rank` | rowid → date/id tie-break for a global bm25 page (P9a) |
| `threads` | conversation cache: subject, last date, mailbox-wide counts, attachments, latest message (P9b) |
| `thread_aliases` | old merged thread id → current id (P9b) |
| `saved_searches` | account, name (unique NOCASE), query, sort and timestamps (P9b) |
| `rules` | account, name, Unicode-lowercase `name_key` (unique at open), enabled, priority, stop, query, action JSON (P9c) |
| `rule_runs` | run id, account, start/finish, dry, rule/matched/changed/error counts (P9c) |
| `rule_actions` | run ordinal, rule/message ids, action name, ids/folders/labels/booleans JSON, changed (P9c) |
| `attachments` | metadata: filename, mime, size, optional blob hash; the bytes live in `blobs/` |
| `labels` | account-scoped or mailbox-wide, name, semantic colour name |
| `message_labels` | assignment join, `PRIMARY KEY(message_id, label_id)`, cascade both ways |
| `drafts` | revision, updated_at, body, recipients, subject, in_reply_to |
| `draft_seq` | message id → highest revision ever written; survives a send and a move back |
| `outbox` | draft id, content-stable idempotency key, state, submission and append attempt counters, due time, lease epoch, fixed error, raw blob hash, envelope JSON (P7) |
| `sync_state` | per-folder sync cursor: next UID to fetch, `highest_modseq`, `last_error` |
| `meta` | key/value store from P3a; holds `label_seq`, `draft_id_seq`, `saved_search_seq`, `rule_seq`, `rule_run_seq`, `recent_searches:<account>`, `reply_to:<message>`, `rule_guard:<account>:<message>`, `rule_forward:<account>:<rule>:<message>`, `rule_all_cursor:<rule>`, `rule_run_mode:<run>`, `rule_run_undone:<run>`, `search_index`, `search_updated_at`, `sync_status:<account>` and `outbox_halt:<account>` |

Indexes cover the hot reads: `messages(folder_id, date DESC)`, `messages(account_id, date DESC)`,
`messages(account_id, thread_id, date DESC)`, `messages(account_id, message_id)`,
`messages(account_id, subject_norm, date)`, `messages(account_id, origin, rules_applied_at,
ingested_at, id)`, `messages(account_id, origin, ingested_at, id)`, the covering
`messages_list_cover`, `threads(account_id, last_date DESC, id)`, `thread_aliases(account_id,
new_id)`, `message_labels(label_id)`, `attachments(message_id)`, `drafts(updated_at DESC)`,
`folders(account_id)` and `outbox(state, send_at)`. `labels` has `UNIQUE(account_id, name)` plus a
partial unique index on mailbox-wide names, because SQLite treats `NULL`s as distinct in a plain
unique constraint.

## Repository

Plain data structs in `model.rs` (no `postino-core` dependency), methods in `repo.rs`:

- accounts: `upsert_account`, `list_accounts`, `get_account`.
- folders: `upsert_folder`, `list_folders`, `get_folder`, `recount_folder`.
- messages: `upsert_message`, `get_message`, `list_messages` (folder/label/all, unread/starred/
  attachment filters, limit/offset), `list_all_messages`, `counts`, `set_flags`, `move_messages`,
  `delete_message`.
- sync support: `upsert_synced_message` (message + attachments in one transaction, no per-message
  recount), `message_flags` (folder and local flags only), `list_uids`, `delete_synced_messages`
  (never deletes a local draft), `reset_folder_messages` (UIDVALIDITY reset; keeps rows without a
  server UID, so local drafts and locally sent mail survive), `get_sync_state`/`set_sync_state`/
  `clear_sync_state`. The cursor's `uid_next` is the next UID to fetch; `folders.uid_next` is the
  server's advertised `UIDNEXT`, a different value.
- labels: `create_label`, `update_label`, `delete_label`, `get_label`, `list_labels` (live counts),
  `apply_label`, `remove_label`, `messages_by_label`, `labels_for`.
- rules (P9c): `list_rules`, `list_all_rules`, `get_rule`, `create_rule_limited`,
  `update_rule(rule, expected_updated_at)` (atomic revision check, `StaleRule`), `delete_rule`
  (also clears the rule cursor), `allocate_rule_id`; `rule_candidates` (received origin only,
  `ingested_at`/`rules_applied_at`/`after` filters, typed search expression),
  `rule_message_state` (folder, origin, unread, starred, labels), `mark_rules_applied`,
  `message_origin`, `message_ingested_at`; checked action methods `rule_move_message`,
  `rule_set_flags`, `rule_set_label` (origin and expected folder re-read in the transaction);
  `create_rule_run`, `finish_rule_run`, `append_rule_action`, `list_rule_runs`, `get_rule_run`,
  `rule_run_actions`, `mark_rule_run_undone`, `prune_rule_runs`, `allocate_rule_run_id`; the
  encrypted loop guard `set_rule_guard`/`rule_guard` and its bulk cleanup helpers; and the durable
  forward marker plus per-rule `all` cursor (`rule_forward_marker`/`set_rule_forward_marker`,
  `rule_all_cursor`/`set_rule_all_cursor`).
- attachments: `replace_attachments`, `attachments`, `list_all_attachments`, `find_attachment`
  (one row plus its message id) and `set_attachment_blob_hash(message, attachment, hash, size)`,
  which the engine calls after a blob write. `replace_attachments` preserves the hash **and size** of
  an id that survives the replacement; clearing it is explicit.
- hydration: `list_all_messages` plus `list_all_attachments` load a whole mailbox in one query per
  table, with labels and attachments grouped by message; `Engine::open` uses exactly this pair.
- drafts: `save_draft_message` is the whole-draft API. It writes the message row, its attachments,
  the revision row and the Drafts folder counts in one transaction, so a failure leaves no
  half-saved draft. `send_draft_message` moves the message to Sent and deletes only the `drafts`
  row, keeping `draft_seq` for a later move back. `delete_draft` removes the revision row and its
  sequence; `delete_message` does the same plus the message itself. Reads are `get_draft` and
  `list_drafts`. `save_draft` is only the low-level revision-row write used by tests and fixtures,
  not a draft API: using it alone recreates the half-write pattern this crate removed.
- orphan repair: `list_orphan_drafts` finds `drafts` rows with no message row;
  `repair_orphan_drafts` materialises them in one transaction, and `Engine::open` runs that repair
  before hydration so a crash from an older build cannot leave a ghost draft.
- outbox: `queue_outbox` (insert, or revive a cancelled row with the same revision/content key),
  `get_outbox`, `active_outbox_for_draft` (newest by `created_at`, then `rowid`),
  `active_state_for_draft`, `list_outbox` / `list_outbox_page` (newest first, bounded, pageable),
  `claim_outbox_append`, `next_outbox_wake`, `lease_next_outbox`, `expire_stale_outbox`,
  `mark_outbox_append_pending` (writes the append claim), `finish_outbox_append`,
  `mark_outbox_uncertain`, `mark_outbox_failed`, `reschedule_outbox`, `reschedule_outbox_append`,
  `release_outbox`, `set_outbox_due`, `cancel_outbox`, `retry_outbox`, `confirm_outbox_sent` and
  `prune_outbox`. Every read-modify-write is `BEGIN IMMEDIATE`; `lease_at` is the epoch for both a
  submission and a claimed Sent append (never `send_at`), and every post-lease transition is
  conditional on the state and the caller's lease, so a worker whose lease expired cannot overwrite the
  next owner, and two passes cannot append the same row. A claim is stamped with the instant the row is
  leased / the append starts, not with a pass-start value. `attempts` counts submissions and
  `append_attempts` counts Sent appends: `mark_outbox_append_pending` sets the latter to 1 and each
  `claim_outbox_append` increments it, so the two backoffs are independent. `cancel_outbox` updates a
  `queued` or `failed` row and never a leased one, so a cancel and a lease cannot both win.
- drafts: `allocate_draft_id` hands out `draft_<n>` from one immediate transaction seeded from the
  largest existing id, so two processes never collide; `local_message_for_id` (account-wide across
  **Sent/Archive** rows with `uid IS NULL`; never Drafts or Trash) and `synced_message_for_id` (folder,
  with a UID) are the two reconciliation lookups, and `send_draft_message` can remove a server-backed
  duplicate in the same transaction as the local move, transferring the duplicate's UID to the surviving
  row (the caller then skips the APPEND).
- meta: `get_meta`, `set_meta`. `label_seq` is the label id high-water mark, `draft_id_seq` the draft
  id allocator; `draft_seq` lives in its own table because it is per draft. The engine adds
  `outbox_halt:<account>` for an account whose
  queue stopped on an authentication failure.

Folder `unread`/`total` are a cache maintained from `messages` on every mutation; `recount_folder`
repairs them after a bulk import. List totals describe the filtered scope before pagination.
`list_messages` clamps the page to 1..=500 rows.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
cargo test -p postino-store
```

- `tests/encryption.rs`: keyless reads, wrong key, reopen, no plaintext in DB or WAL.
- `tests/blobs.rs`: blob round trip and reopen, dedup (one file for identical bytes), dedup over a
  corrupt file (heals it), a foreign key's file (replaced, then readable), wrong key, tampered
  ciphertext/tag/nonce, a flag or in-cap length flip, content planted under a false name, a swapped
  file, an oversized declared length, non-hex/Windows/traversal ids without echo, temp cleanup on
  success and on a failed rename, eight concurrent puts, compression only when it helps, the header
  layout and hexdump, remove/has, a no-plaintext scan of the database, WAL and SHM while the store is
  open (plus every blob file and the key bytes), per-file blob roots, and the attachment-row hash and
  size behaviour.
- `tests/migrations.rs`: additive v1 → v3 upgrade with meta preserved, v2 → v3 `draft_seq`
  seeding, idempotent reopen, `SchemaTooNew`, foreign-key enforcement and cascade.
- `tests/repository.rs`: the full round trip (account, folders, message, two recipients, attachment,
  two labels), filters/paging, counts after read/move/delete, cached-count repair, atomic batches,
  label delete detach, draft revisions across reopen, reopen persistence, unknown-id errors and the
  no-plaintext scan over DB/WAL/SHM with repository data.
- `tests/sync_support.rs`: the sync cursor round trip, the one-transaction message+attachment upsert
  (including a rollback on a duplicate attachment id), `message_flags`, the UID list ignoring local
  rows, a deletion pass that cascades assignments/attachments but keeps local drafts, and a
  UIDVALIDITY reset that leaves other folders, their labels and local drafts untouched.
- `tests/rules.rs`: rule ordering, the 100-rule cap and Unicode name uniqueness; candidate origin,
  horizon, `after` and ordering; `EXPLAIN QUERY PLAN` uses both rule indexes; the 20-run journal
  retention; guard cleanup on message delete, bulk sync delete, folder reset and account removal;
  and a durable forward marker that survives an outbox prune.
- `tests/lock_crash.rs`: a child test process holds the writer lock, the parent kills it, and the next
  writer acquires the store; the owner record is separate from the locked file.
- unit tests in `src/repo.rs`: a temporary SQLite trigger aborts the `drafts` or `draft_seq` insert,
  proving the whole save rolls back (message, attachment, draft, sequence and folder counts) and
  that the same save succeeds once the trigger is dropped.

## Writer lock and blob GC (P10a)

`Store::open_locked(path, key)` canonicalises the path before acquiring `<store>.lock` with `fs4` and
records `<store>.owner` (pid, launch token, start instant, role, endpoint). A second writer is
`StoreError::StoreBusy` with the owner's pid; a crash releases the OS lock and the next owner
overwrites the stale record. A symlink resolves to the same canonical lock; a hard-linked database is
refused with `StoreError::HardLinked` because two names would otherwise have two independent locks for
one inode. The lock file is separate from the owner record because Windows byte-range locks are
mandatory: a locked file cannot be read to report who holds it. `Store::open` remains the unlocked form
for tests and read paths; the daemon and the app use `open_locked`/`WriterLock`, and the CLI's
read-only command uses `Store::open_read_only` (or `open_read_only_locked` for `--in-process`).

`Store::gc_unreferenced_blobs(limit)` lists `<root>/<shard>/<hash>.blob`, builds the referenced set from
`attachments.blob_hash`, `outbox.raw_blob_hash` and `messages.raw_blob_hash`, rechecks each candidate
immediately before unlinking, and deletes at most `limit` (≤4096) per call. It is bounded and
resumable: the next call sees the remaining files. A read-only store refuses it. It never deletes a
file whose name is not exactly 64 lowercase hex characters plus `.blob`.

## Not here yet

Desktop-app daemon wiring (P10a2), raw-message blobs for ordinary mail (P12c2a keeps them only for
PGP/MIME), key rotation. P9c's rules live here as data and
candidate queries; their action engine and forward-through-P7 policy live in `postino-core`.
`sync_state` and the sync-status `meta` key have writers now: `postino-sync` (P5b) is the client of this
crate for received mail. Parsing MIME is not here: sync parses and hands over rows and blob hashes. The
engine (P3c, P4b) is another client; this crate never depends on it. The daemon owns scheduling, not
this crate; the GC sweep is the one storage-side maintenance entry point.

## Build prerequisite (Windows GNU)

`rusqlite`'s `bundled-sqlcipher-vendored-openssl` builds OpenSSL from source. It needs MSYS2 `make`
and `perl` on `PATH`, and a parallel cold build:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
```

Without `MAKEFLAGS`, `openssl-src` builds serially on Windows: ~14 min cold instead of ~4.5 min.
The blob layer adds `zstd-sys` (bundled C zstd) and blake3's assembly; both use the same MSYS2 gcc
and add about 19 s to a dev rebuild of this crate.
