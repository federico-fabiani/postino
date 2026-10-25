# postino-sync

Reconcile one IMAP account into the encrypted local store. Layer: sync, above the foundation
(`postino-store`) and the network (`postino-net`), below the engine. It opens no UI, registers no
operation, runs no daemon and schedules nothing.

```rust
let cancel = AtomicBool::new(false);
let outcome = postino_sync::sync_account(
    &store,               // one connection, owned by the caller
    &account,             // an accounts row
    &imap_config,         // postino-net: host, TLS, credentials, bounds
    &SyncLimits::default(),
    &cancel,
)?;
for warning in &outcome.warnings {
    eprintln!("{warning}");
}
```

`sync_account` blocks: it builds a private current-thread Tokio runtime, speaks to the server, writes
through the caller's `Store` on the caller's thread, and returns. There is no global runtime and no
detached task. `read_status(&store, &account)` reads the last status without the network.

## API

| Item | Meaning |
|---|---|
| `SyncLimits` | `batch` (100), `max_message_bytes` (25 MiB), `max_attachment_bytes` (25 MiB), `max_messages` (50 000); `clamped()` keeps every value usable |
| `SyncOutcome` | account, folders discovered, messages fetched, drift observations, rows removed, blobs stored, bounded warnings |
| `sync_account` | the run; returns `Ok(SyncOutcome)` or `Err(SyncError)` |
| `SyncError` | `Connect(NetError)`, `Auth(String)`, `Store(StoreError)`, `Cancelled`, `NoAccount(String)`, `Protocol(String)` |
| `SyncStatus` / `SyncPhase` / `read_status` / `status_key` | the persisted `meta` record, `sync_status:<account_id>` |

## What one run does

1. `LIST "" "*"` once; skip `\Noselect`; map RFC 6154 special-use to `FolderKind`; a folder named
   `INBOX` is the inbox even without the attribute. Unknown selectable folders are `Custom`.
   Missing `\Sent`/`\Drafts` produce warnings, never invented folders. `\All` and `\Flagged` get a
   row but are not fetched: they are aggregate views whose messages live in a real folder.
2. `SELECT` each folder. A changed `UIDVALIDITY` drops that folder's cached server messages, label
   assignments and cursor in one transaction; rows without a UID (local drafts and locally sent
   mail) survive. A first sync starts at UID 1.
3. Fetch envelopes in bounded UID windows, then `BODY.PEEK[]` per message. Parse with
   `postino-mime` under the sync limits. Upsert the message, recipients, bodies, flags, `uid`,
   `Message-ID`, thread placeholder and attachment metadata in one transaction; write attachment
   bytes through the encrypted blob seam only when the part is complete and within the limit.
4. Advance `sync_state.uid_next` only after the transaction commits.
5. Fetch the full UID list per folder, remove cached rows whose UID is gone in one transaction
   (never a local draft), and observe flag drift without writing it. The network client has no
   flags-only `FETCH`, so the list reuses the envelope request and discards the extra fields.
6. Recount the folder once, record `last_sync_at`, and update the status.

`BODY.PEEK[]` only: sync never sends `STORE`, `EXPUNGE`, `COPY`, `MOVE` or `APPEND`.

## Reconciliation decisions

- **Message id.** `msg_` + 16 hex of `BLAKE3(account, Message-ID)`, or
  `msg_{folder}_{uidvalidity}_{uid}` without a usable `Message-ID`. The same `Message-ID` in two
  folders is one local message: the later folder wins `folder_id`, labels survive, the other copy is
  not a second row. The trade-off (a row can be removed while the other folder still holds the mail,
  until that folder is refetched from scratch) is in `docs/architecture.md` §4.6.
- **Folder id.** `fld_` + 16 hex of `BLAKE3(account, wire name)`: display-name renames do not orphan
  the cache.
- **Thread id.** First `References` entry, else `In-Reply-To`, else the message's own `Message-ID`
  (or its local id). Full JWZ threading is P9.
- **Flags.** Server flags apply on first insert only. Local `unread`/`starred` are never overwritten;
  each difference is counted in `updated_flags` and produces one warning per run. M1 flags are
  local-only; M2 pushes them.
- **Deletion.** Only local rows are deleted, after a complete UID list, in one transaction. A run
  stopped by `max_messages` has an incomplete view and skips the deletion pass.
- **Counts.** `upsert_synced_message` deliberately does not recount the folder per message; the run
  recounts once per folder. A crash can leave counts stale until the next run or `recount_folder`.
- **Status.** Written at start, after each folder and at the end. `error` is a fixed sentence
  (`"could not talk to the server"`, ...), never a server name, address or content. A crash leaves
  `syncing`; the next run resets it.
- **Cancellation.** Checked before connecting, before each folder, between batches and before each
  message. A cancelled run returns `SyncError::Cancelled`; committed messages stay and the next run
  resumes from the cursor.

## What is not synced

Server-side filters/rules, flag push, expunge/delete on the server, `APPEND` of drafts or sent mail,
`CONDSTORE`/`QRESYNC`, folder removal/rename reconciliation, full threading, FTS5 indexing, a
scheduler/IDLE worker, and any operation, CLI, MCP tool or UI. Onboarding and those surfaces are P6.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
cargo test -p postino-sync
$env:POSTINO_SLOW_TESTS = "1"; cargo test -p postino-sync --test scale ten_thousand -- --nocapture
```

`crates/sync/tests/sync.rs` (16 tests): initial sync of 3 special-use folders and 50 messages (HTML
sanitized, one encrypted attachment, flags, counts, `uid_next`, status), incremental UIDs with a
label preserved, UIDVALIDITY reset with another folder untouched, deletion of a vanished UID, flag
drift, oversized attachment, oversized message, malformed message, cancellation + reopen + resume,
three disconnect points (before the first message, mid-batch, after the new-mail pass), `BODY.PEEK`-only
with no server mutation, the testkit wire builder, and an offline read through `Engine::open`.

`crates/sync/tests/scale.rs`: 2 000 messages in CI with a 180 s budget; 10 000 behind
`POSTINO_SLOW_TESTS=1` with a 1 800 s budget. Measured on the Windows GNU host, debug profile
(opt-level 1, dependencies at 2):

| Messages | Wall time | Peak working set |
|---|---|---|
| 2 000 | 10.9-11.7 s | 22.9 MB |
| 10 000 | 47.4-55.7 s | 38.6 MB |

Unit tests cover ids, limits, status JSON, warnings and the folder mapping.

## Build cost

No new third-party crates: `postino-sync` reuses `postino-store`, `postino-net`, `postino-mime`,
`blake3`, `serde`, `serde_json`, `thiserror`, `tokio` and `log`, all already in the workspace. The
dev-dependencies are `postino-testkit`, `postino-core` (the offline engine test) and `tempfile`.
A cold `cargo test -p postino-sync --no-run` took 1 min 34 s on the Windows GNU host, almost all of
it the already-required SQLCipher/OpenSSL build; a warm incremental build is a few seconds.
