# postino-sync

Reconcile one IMAP account into the encrypted local store, and provide the submission serialization
and Sent append that P7 uses. Layer: sync, above the foundation (`postino-store`) and the network
(`postino-net`), below the engine. It opens no UI, registers no operation, runs no daemon and
schedules nothing.

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
| `Submission` / `serialize_submission` | a composed message mapped to `postino-mime`: exact RFC 5322 bytes, envelope, Message-ID |
| `submit` | one SMTP transaction with the typed `SmtpError` (P7) |
| `verify_smtp` | the `account_add` probe: connect, authenticate, quit |
| `append_sent` | discover the server's `\Sent` folder from `LIST` and `APPEND` the bytes with `\Seen` |

## Submission helpers (P7)

`postino-sync` is the only crate above `postino-net` that knows how a composed message becomes SMTP
bytes. `Submission` is plain data (from, To/Cc/Bcc, subject, date, text/HTML, attachments, reply
linkage); `serialize_submission` calls `postino-mime::serialize`, which validates and folds headers,
encodes bodies/attachments and keeps Bcc out of the raw bytes, in the envelope only. The engine writes
the returned bytes to the encrypted blob store before queueing.

`submit` connects, authenticates, calls `SmtpClient::send` and quits on success. It returns the
network client's `SmtpError` unchanged so the engine can distinguish transient, permanent and
uncertain. `append_sent` opens IMAP, logs in, lists folders, finds the `\Sent` special-use folder and
APPENDs the same bytes with `\Seen`; it raises the literal bound to the message size (hard ceiling
64 MiB). It never submits. `verify_smtp` is used by `account_add` before any durable write. All three
block the caller and own a private current-thread runtime; none logs a body, address or password.

## What one run does

1. `LIST "" "*"` once; skip `\Noselect`; map RFC 6154 special-use to `FolderKind`; a folder named
   `INBOX` is the inbox even without the attribute. Unknown selectable folders are `Custom`.
   Missing `\Sent`/`\Drafts` produce warnings, never invented folders. `\All` and `\Flagged` get a
   row but are not fetched: they are aggregate views whose messages live in a real folder.
2. `SELECT` each folder. A changed `UIDVALIDITY` drops that folder's cached server messages, label
   assignments and cursor in one transaction; rows without a UID (local drafts and locally sent
   mail) survive. A first sync starts at UID 1.
3. Probe the true maximum UID with `UID FETCH <max(cursor, UIDNEXT)>:* (UID FLAGS)`. `UIDNEXT` is a
   hint: a missing, stale or low value must not hide mail, and `EXISTS`/`RECENT` are not liveness.
4. Fetch envelopes in explicit UID windows below that maximum, then `BODY.PEEK[]` per message. Parse
   with `postino-mime` under the sync limits. Upsert the message, recipients, bodies, blocked remote
   image count, flags, `uid`, `Message-ID`, thread placeholder and attachment metadata in one
   transaction; write attachment bytes through the encrypted blob seam only when the part is complete
   and within the limit. A `multipart/signed` or `multipart/encrypted` entity also keeps the exact
   received bytes in one blob named by `messages.raw_blob_hash` (schema v9); ordinary mail keeps
   none. A body that vanished between the envelope and the fetch is a per-message skip with a warning.
5. Advance `sync_state.uid_next` only after the transaction commits.
6. Fetch `UID FLAGS` for the whole folder in 1 000-UID windows (the only liveness source), compare it
   with one `folder_message_flags` snapshot in memory, and propose the ids whose UID is gone.
7. Only after every folder has been scanned completely, delete the proposed ids that were not seen in
   any folder this run, one transaction per folder. A move is therefore never a delete. A run stopped
   by `max_messages`, a cancellation or a failed folder deletes nothing.
8. Recount each folder once, record `last_sync_at`, and update the status.

MIME warnings (`ParsedMessage.warnings`) are mapped into the run warnings, prefixed with the message
id and deduplicated: a part-limit or depth warning says the attachment list may be incomplete instead
of letting a truncated message look whole.

`BODY.PEEK[]` only: `sync_account` never sends `STORE`, `EXPUNGE`, `COPY`, `MOVE` or `APPEND`. The
submission helper `append_sent` (P7) is a separate call the engine makes after SMTP accepted a
message; it APPENDs to `\Sent` and never submits. A synced Sent copy whose `Message-ID` matches a
locally written row with no UID adopts that row instead of inserting a second one, account-wide (a
server copy that moved to another folder still reconciles); when the server copy was cached *before*
the local move, the finalize removes it in the same transaction.

## Reconciliation decisions

- **Message id.** The hash input is the exact trimmed `Message-ID` (one pair of angle brackets
  removed, nothing else changed: no case-folding, no first-token shortcut, no substitution). The
  plain id is `msg_` + 16 hex of `BLAKE3(account, value)`; no usable header falls back to
  `msg_{folder}_{uidvalidity}_{uid}`. A second distinct message with the same `Message-ID` **in the
  same folder** gets `msg_<hash>_<uid>`, then `msg_<hash>_<uid>_<uidvalidity>` if that is taken
  (ascending UID order makes this stable). The same id in **another** folder is a move: `folder_id`
  changes, labels and local flags stay. If every candidate is taken the message is skipped with a
  warning rather than overwriting a row. A message without a `Message-ID` that moves gets a new id,
  so its labels cannot follow; the old row is removed only after the new copy exists.
- **Folder id.** `fld_` + 16 hex of `BLAKE3(account, wire name)`: display-name renames do not orphan
  the cache.
- **Thread id.** First `References` entry, else `In-Reply-To`, else the message's own `Message-ID`
  (or its local id). Full JWZ threading is P9.
- **Flags.** Server flags apply on first insert only. Local `unread`/`starred` are never overwritten;
  each difference is counted in `updated_flags` and produces one warning per run. The comparison uses
  one folder query, not one per message. M1 flags are local-only; M2 pushes them.
- **Deletion.** Deferred and account-wide: a per-folder pass cannot tell a deleted message from one
  moved to a folder processed later. Only a complete run that scanned every folder deletes, and only
  ids not seen anywhere in the run. Each folder's removal is one transaction; local drafts and rows
  without a UID are never candidates.
- **UIDNEXT.** A hint only. The `<hint>:*` probe discovers the real maximum; explicit windows stay
  below it. `EXISTS 0` does not skip a folder and a low `UIDNEXT` does not delete live mail.
- **Counts.** `upsert_synced_message` deliberately does not recount the folder per message; the run
  recounts once per folder. A crash can leave counts stale until the next run or `recount_folder`.
- **Status.** Written at start, after each folder and at the end. `error` is a fixed sentence
  (`"could not talk to the server"`, ...), never a server name, address or content. A crash leaves
  `syncing`; the next run resets it.
- **Cancellation.** Checked before connecting, before each folder, between new-mail windows, between
  UID/flags scan windows and before each message. A cancelled run returns `SyncError::Cancelled`,
  keeps committed messages, writes the error status and deletes nothing.

## What is not synced

Server-side filters/rules, flag push, expunge/delete on the server, `APPEND` of drafts (Sent is
written by the P7 submission helper, not by `sync_account`), `CONDSTORE`/`QRESYNC`, folder
removal/rename reconciliation, full threading, FTS5 indexing, a scheduler/IDLE worker, and any
operation, CLI, MCP tool or UI. Onboarding and those surfaces are P6.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
cargo test -p postino-sync

# The 10 000-message test is #[ignore] on purpose: a normal run reports it ignored instead of
# green without running it. Set the variable and ask for ignored tests by name.
$env:POSTINO_SLOW_TESTS = "1"
cargo test -p postino-sync --test scale -- --ignored ten_thousand_messages --nocapture
```

`crates/sync/tests/sync.rs` (17 tests): initial sync of 3 special-use folders and 50 messages (HTML
sanitized, one encrypted attachment, flags, counts, `uid_next`, status), incremental UIDs with a
label preserved, UIDVALIDITY reset with another folder untouched, deletion of a vanished UID, flag
drift, oversized attachment, oversized message, malformed message, cancellation + reopen + resume,
three disconnect points (before the first message, mid-batch, during the flags scan), `BODY.PEEK`-only
with no server mutation, the testkit wire builder, and an offline read through `Engine::open`.

`crates/sync/tests/closure.rs` (11 tests, the P5c regressions): a move keeps its label and local read
state, a run that stops after the source folder leaves the moved row (with and without a
`Message-ID`), two identical `Message-ID`s produce two stable rows, a missing `UIDNEXT` finds UID 250,
a low/stale `UIDNEXT` neither hides nor deletes a higher live UID, `EXISTS 0` does not hide a message,
an empty mailbox is clean, one expunged body is a warning and later folders still sync, cancellation
reaches the UID/flags scan, a 300-part message reports the MIME part-limit warning with its id, and
`max_messages` stops the run without deleting.

`crates/sync/tests/scale.rs`: 2 000 messages in the normal run with a 180 s budget; 10 000 with the
`#[ignore]` above and a 1 800 s budget. Measured on the Windows GNU host, debug profile (opt-level 1,
dependencies at 2):

| Messages | Wall time | Peak working set |
|---|---|---|
| 2 000 | 9.1 s | 23.3 MB |
| 10 000 | 37.3 s | 34.8 MB |

The new scan is cheaper than the P5b deletion pass it replaces: at 2 000 messages it is one probe
plus two 1 000-UID `UID FLAGS` windows, against twenty envelope windows before, so the run went from
11.7 s to 9.1 s.

Unit tests (25) cover ids and duplicate candidates, limits, status JSON, warning deduplication, MIME
limits and the folder mapping.

## Build cost

No new third-party crates: `postino-sync` reuses `postino-store`, `postino-net`, `postino-mime`,
`blake3`, `serde`, `serde_json`, `thiserror`, `tokio` and `log`, all already in the workspace. The
dev-dependencies are `postino-testkit`, `postino-core` (the offline engine test) and `tempfile`.
A cold `cargo test -p postino-sync --no-run` took 1 min 34 s on the Windows GNU host, almost all of
it the already-required SQLCipher/OpenSSL build; a warm incremental build is a few seconds.
