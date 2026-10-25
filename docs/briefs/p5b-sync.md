# Brief — P5b sync and offline cache

> **Package:** P5b · **Depends on:** P5a (IMAP), P4a (MIME), P4b (blobs), P3d/P3e (store) ·
> **Next:** P6 onboarding and app on real data
> **Owner:** developer agent · **Review:** blind engineering + kill/resume evidence
>
> This package makes real mail appear in the encrypted store. It ships no UI and no operation yet:
> P6 adds onboarding, the operations and the screens that use them.

## Outcome

`postino-sync` reconciles one account from IMAP into the store: folders with special-use, envelopes,
bodies parsed by `postino-mime`, attachment bytes in the blob store, flags on first insert, UID
validity changes, deletions, a cancellable sync and a persistent status. The fake IMAP server moves
to a reusable `postino-testkit` crate so sync tests script real protocol exchanges.

## 1. `postino-testkit`

- Move `crates/net/tests/support` into `crates/testkit` (`postino-testkit`), a workspace crate used
  only as a dev-dependency. It exposes the scripted server, TLS test CA helper (`rcgen`), response
  builders (LIST, SELECT, FETCH with literals, IDLE) and a generator for N synthetic messages.
- `postino-net` tests use it; layering allows only test crates to depend on it.
- No production crate may depend on `postino-testkit`.

## 2. `postino-sync` API

```rust
pub struct SyncLimits {
    pub batch: usize,              // envelopes per FETCH, default 100
    pub max_message_bytes: usize,  // default 25 MiB
    pub max_attachment_bytes: u64, // per attachment, default 25 MiB
    pub max_messages: usize,       // per run, safety valve, default 50_000
}

pub struct SyncOutcome {
    pub account: String,
    pub folders: u32,
    pub fetched: u32,
    pub updated_flags: u32,
    pub removed: u32,
    pub attachments_stored: u32,
    pub warnings: Vec<String>,
}

pub fn sync_account(
    store: &Store,
    account: &store::Account,
    config: &net::ImapConfig,
    limits: &SyncLimits,
    cancel: &AtomicBool,
) -> Result<SyncOutcome, SyncError>;
```

- Blocking wrapper around a current-thread tokio runtime; the store stays on the caller thread. No
  global runtime and no detached task.
- `SyncError`: `Connect(NetError)`, `Auth`, `Store(StoreError)`, `Cancelled`, `NoAccount`, `Protocol`.
  A network failure is not fatal to the cache: the store is unchanged or consistent per message.
- One `Store` connection owned by the caller; every message upsert and its attachments are one
  transaction (extend the repository if needed, do not open a second connection).
- Cancellation is checked between batches and before each message; a cancelled run returns
  `Cancelled` with the messages written so far intact.

## 3. Reconciliation rules

**Folders.** LIST once; decode names; map `\Inbox`, `\Sent`, `\Drafts`, `\Trash`, `\Junk`,
`\Archive`, `\Flagged` to `FolderKind`; unknown selectable folders become `Custom`. Skip
`\Noselect`. Create the local rows for the account. Keep local labels and drafts untouched. If the
server has no Sent/Drafts, do not invent them silently; record a warning (P6 decides).

**UID validity.** SELECT each folder. If the server's `UIDVALIDITY` differs from the stored one,
delete the cached messages of that folder (and their label assignments; attachments cascade) and
start from scratch for that folder. Log the reset as a warning.

**New mail.** `UID FETCH <last+1:*> (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)` in batches, then
`UID FETCH <uid> (BODY.PEEK[])` per message. Parse with `postino-mime` using the sync limits. Upsert
one message: summary, body text/html, recipients, date, size, uid, message-id, thread id, labels
preserved on update, attachments metadata. Store attachment bytes **only** when the part is within
the limit, via the blob seam; a skipped attachment is a warning, not a failed message. Advance
`sync_state.uid_next` only after the message is committed, so a crash resumes without a gap.

**Message ids.** Stable across syncs and UIDVALIDITY changes: `msg_` + the first 16 hex of
BLAKE3(account id, Message-ID header) when a Message-ID exists; otherwise
`msg_{folder}_{uidvalidity}_{uid}`. The same Message-ID in two folders maps to one local message; the
later folder wins the folder id, labels are preserved. Document the trade-off.

**Threading.** `thread_id` = the first `References` entry, else `In-Reply-To`, else the message id.
Full JWZ threading is P9; this is a stable placeholder.

**Flags.** Apply server flags when a message is first inserted. On later syncs **do not overwrite**
local `unread`/`starred`: in M1 flags are local-only and M2 will push them. Record the drift in the
docs and in the outcome (a warning when they differ, once per run). This is a deliberate, honest
limitation.

**Deletions.** Fetch the full UID list per folder (`UID FETCH 1:* (UID FLAGS)`) and remove cached
messages whose UID is gone, except local drafts. Do it after the new-mail pass, in one transaction,
so a crash does not lose mail. Never expunge on the server.

**Batch and limits.** Stop at `max_messages` with a warning. A single message larger than the limit
is skipped with a warning, not a failed run.

## 4. Status

Persist JSON under `meta` key `sync_status:<account_id>`:

```json
{"state":"idle|syncing|error","last_attempt":<ms>,"last_success":<ms>,"error":null,
 "folders_done":0,"folders_total":8,"messages":0}
```

Written at start, after each folder and at the end. A crash leaves `syncing`; the next run resets it.
Read-only helper `read_status(store, account)` for P6 and the CLI. No credentials, server names or
message content in the status.

## 5. Tests (fake server)

1. Initial sync of 3 folders with Inbox/Sent/Trash special-use, 50 messages, envelope fields, bodies,
   one attachment, flags → store rows, counts, MIME bodies sanitized, blob stored, uid_next right.
2. Incremental: second run fetches only new UIDs; no duplicates; label on an existing message
   survives.
3. UIDVALIDITY change: cached messages of that folder are replaced, other folders untouched, labels
   on messages in other folders survive.
4. Deletion: server removes a UID, sync removes the local message and its assignments.
5. Flags on first insert; a local read/star edit is **not** overwritten on the next sync and produces
   a drift warning.
6. Attachment over the limit is skipped with a warning; the message is still stored.
7. Cancellation after N messages → `Cancelled`, reopen and resume → complete mailbox, no duplicates.
8. Network drop before the first message, mid-batch and after a committed message → typed error,
   cache consistent, resume works.
9. `BODY.PEEK` only: the fake server asserts no `BODY[]` and no `\Seen` STORE.
10. 2 000-message sync in CI with a generous time budget; a 10 000-message test behind
    `POSTINO_SLOW_TESTS=1`, documented with measured time and peak memory.
11. Offline read: after a successful sync, close the server, open an engine on the store and assert
    bodies and attachment bytes are readable.

## 6. Docs

- `docs/architecture.md`: sync pipeline, stable ids, UIDVALIDITY, flag policy, status, cancellation,
  what is not synced (server filters, server flags out, expunge, threading depth).
- `docs/threat-model.md`: attachment limits, server-controlled names, no auto-open, TLS already P5a.
- README and ROADMAP: M1 sync exists as a crate and is tested against the fake server; the app still
  runs on demo data until P6. Be explicit that no operation/CLI/UI is wired yet.

## 7. Out of scope

Onboarding, credentials, autodiscovery, operations, CLI, UI, app on real data, IDLE worker loop,
daemon, SMTP, outbox, rules, FTS5, full threading, QRESYNC, server flag push.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no `unwrap`/`expect`/`panic!` outside tests, layers
updated for `postino-sync` and `postino-testkit`. Temp files only in
`%TEMP%\opencode\p5b-postino`; do not commit.

## Report

API, reconciliation decisions, test map, kill/resume evidence, 2k/10k numbers if run, and any
deviation with the reason.
