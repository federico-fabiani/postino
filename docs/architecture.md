# Postino architecture

> **Status:** M0 walking skeleton + U0 UI foundation + engine on the encrypted store (P3c/P3d) + MIME
> crate (P4a) + encrypted attachment blobs (P4b) + IMAP client (P5a) + sync into the store (P5b) +
> accounts and credentials (P6a) + app on real mail (P6b/P6c) + remote-content count (P6d) + SMTP
> submission and the outbox (P7) + compose depth: attachment bytes, signatures per account and
> reply/forward headers (P8) + encrypted FTS5 search, the body-free read path and schema v7 (P9a) +
> conversation derivation/merge/alias, the conversation view and saved searches (P9b) + local rules
> with preview, idempotent runs, undo and forwarding through P7 (P9c) + PGP key storage and schema v8
> (P12c1) + exact PGP/MIME raw retention and schema v9 (P12c2a) + PGP/MIME verify/decrypt/send and the
> app surface (P12c2b/P12c2c) + the daemon, IPC and its clients (P10a1/P10a2) + tray, autostart and
> notifications (P10b1/P10b2) + the sanitized block renderer (P11a1/P11a2) + Gmail OAuth (P12a),
> invitations (P12d), packaging source (P12e) and fuzz/security CI (P12f) + P13a-P13i.
> **Last updated:** 2026-11-04 ·
> **Target:** a local-first, agent-first mail client that is fast on every desktop.

## 1. Shape today

One Cargo workspace, edition 2024, Rust 1.98+:

| Path | Role |
|---|---|
| `crates/core` | Domain types, the engine (demo or store-backed), the operation registry and its JSON Schemas |
| `crates/crypto` | HKDF-SHA256 subkeys and XChaCha20-Poly1305 sealing; the blob subkey (P4b) |
| `crates/mime` | MIME parsing, HTML sanitization, HTML-to-text and RFC 5322 serialization (P4a) |
| `crates/net` | IMAP4rev1 client (P5a) and SMTP submission client (P7): TLS, bounds, typed errors |
| `crates/platform` | OS seams: store key and account credentials in the OS keyring |
| `crates/store` | SQLCipher schema v9, migrations, the repository, FTS5 search, the conversation cache, saved searches, local rules and their journal, the outbox journal, the encrypted blob store, PGP keys and PGP/MIME raw blobs (P3b, P4b, P6a, P6d, P7, P9a, P9b, P9c, P12c1, P12c2a) |
| `crates/sync` | One account from IMAP into the store, plus submission serialization and the Sent append (P5b, P7) |
| `crates/testkit` | Test-only fake IMAP/SMTP servers, TLS fixture, response builders and synthetic mail (dev-dependency only) |
| `apps/postino-app` | egui/eframe desktop shell, design tokens, banded three-pane layout |
| `apps/postino-cli` | `postino` command line and the opt-in MCP server |
| `xtask` | `ci`, `layers`, `assets` |

The engine has two sources of truth, chosen at construction:

- `Engine::demo()` / `demo_at()` — synthetic mail in memory, no I/O. This is the `--demo` path the
  desktop app uses for screenshots, trials and the recovery screen.
- `Engine::open(store)` — the encrypted SQLCipher store, loaded once and written through on every
  mutation. The desktop app boots on it by default (onboarding when it has no account); the CLI
  reaches it with `--store <path>` or the default local path; the key comes from the OS keyring.

The UI, CLI and MCP read both through the same `Engine::call(name, args)` door. The MCP server and
the GUI never touch SQL directly. Mail is fetched from IMAP by `postino-sync`; SMTP submission and
the outbox are P7 (§4.9); since M4 the daemon owns the outbox scheduler, and the desktop send worker
and the CLI remain the in-process path when no daemon is running.

## 2. The seam that matters: operations, not functions

Every action is an operation:

```rust
pub struct OpDef {
    pub name: &'static str,          // messages_list
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value, // derived from the request type
    pub read_only: bool,
    pub destructive: bool,
}
```

- The request type is a Rust struct with `serde(deny_unknown_fields)` and `JsonSchema`.
- The schema is generated with `schemars`, never hand-written.
- The handler deserializes into that type and returns JSON.
- `Engine::tools()` is the only catalogue. `postino tools`, `postino run`, `postino mcp` and the
  eventual GUI command palette read it.

This is the same contract as the Crafting Apps, with one improvement: schemas are derived from
types, so a handler and its public schema cannot drift.

Current operations: `app_info`, `accounts_list`, `account_add`, `account_update`, `account_test`,
`account_remove`, `account_signature_set`, `sync_now`, `sync_status`, `folders_list`,
`messages_list`, `message_get`, `attachment_get`, `message_invite`, `invite_reply_draft`, `message_flag`, `message_move`, `draft_save`, `draft_attachment_add`,
`draft_attachment_remove`, `message_send`, `send_schedule`, `outbox_list`, `outbox_cancel`,
`outbox_retry`, `outbox_mark_sent`, `send_dry_run`, `search`, `search_status`, `search_reindex`,
`labels_list`, `label_create`, `label_update`, `label_delete`, `label_apply`. Later phases added the
thread, saved-search, rule, template, identity and PGP key groups, `message_export`/`message_import`,
the mbox pair and `message_unsubscribe`; together they are enough to prove the path through all three
clients. `draft_save` updates one draft in place and `body_format: "html"` compiles its Markdown-like source into email-safe HTML plus a text alternative (raw HTML is never accepted);
`draft_attachment_add/remove` own the attachment bytes; `message_send` queues it with the undo
window; `send_dry_run` returns the exact bytes without a row. Labels are local metadata: in memory in
the demo, in the encrypted store otherwise, and never synced to a server. `search` is SQL over the
encrypted FTS5 index; `search_status` is a read and `search_reindex` rebuilds the index offline.

## 3. The target shape

```
┌──────────────┐  ┌────────────┐  ┌──────────────┐
│ Postino App  │  │  postino   │  │ postino-mcp  │
│   (egui)     │  │   (CLI)    │  │   (stdio)    │
└──────┬───────┘  └─────┬──────┘  └──────┬───────┘
       └────────────────┼────────────────┘
              JSON-RPC over a local pipe
                   (per-user token)
                 ┌──────┴───────┐
                 │postino-daemon│  sync · scheduler · rules
                 └──────┬───────┘
        ┌───────────────┼────────────────┐
   ┌────┴─────┐   ┌─────┴─────┐   ┌──────┴─────┐
   │  store   │   │ imap/smtp │   │   crypto   │
   │ SQLCipher│   │  rustls   │   │  keyring   │
   └──────────┘   └───────────┘   └────────────┘
```

### Process model

One process owns the store and the network: `postino-daemon`. The app, CLI and MCP server are
clients. This avoids two writers, two sync loops and a draft race between the GUI and an agent.
The daemon:

- starts at login when the user opts in (per-user registry `Run` value on Windows, LaunchAgent,
  `systemd --user`) and lives in the tray;
- holds the SQLite connection in WAL mode and the IMAP/SMTP connections;
- runs the outbox scheduler and local rules;
- listens on a named pipe (Windows) or Unix domain socket, accepts the same user only, and
  authenticates with a fresh random token written to an owner-only file;
- speaks the same JSON-RPC 2.0 as MCP, over a different transport.

M0 runs everything in-process because there is nothing to synchronise yet. ADR-0002 records the
decision and the migration path.

### Process model, IPC and scheduler (P10a)

`postino-daemon` is the owner. It acquires `<store>.lock` with an OS advisory lock before SQLite is
opened, writes a readable owner record to `<store>.owner` (pid, launch token, start instant, role,
endpoint), opens the one write connection and builds the `Engine`. The lock is separate from SQLite:
a direct `postino --store` writer, the desktop app or a second daemon is refused with the owner's pid
before it can race a transaction. A crash needs no repair: the kernel releases the lock, the next
owner overwrites the stale record, and the fixed restart sentence says that every outbox row is kept
and an expired submission lease becomes `uncertain` rather than being resent. The app held the same
lock in P10a1 and showed a readable "the daemon owns the store" state instead of opening a second
writer; P10a2 replaced that with a daemon client.

The endpoint is derived from the current user id (SID on Windows, uid on Unix) and a BLAKE3 hash of
the absolute store path. Windows uses a named pipe whose DACL is one generic-all ACE for the current
user and `reject_remote_clients`; Unix uses a 0600 socket in a 0700 directory. A fresh 32-byte token
is written per launch to `<store>.token` with an owner-only ACL/mode; the first frame is
`hello {protocol, client, token}`, a bad token is one generic refusal, a version mismatch is readable,
and a stale token is replaced at the next start. There is no TCP listener, no WebSocket and no remote
variant in the endpoint type.

Frames are newline-delimited JSON-RPC 2.0 capped at 16 MiB in both directions. A reader drains an
over-long line without allocating it; a writer refuses to produce one. Each connection has eight
in-flight requests and a 64-value write queue, one in-flight mutation, at most four event
subscriptions, and the server caps total dispatches (16), subscriptions (64) and connections (32).
The 120-second idle timeout applies only when no request is in flight and no subscription is active;
while work is live the server sends a heartbeat and a 30-second write timeout closes a stalled reader.
An oversized response is replaced by a same-id `FRAME_TOO_LARGE` error without closing the
connection. Request ids are namespaced by connection, so a client cannot cancel another client's work;
`op {name,args}` dispatches the same registry as the CLI
and MCP; `daemon_status`, `daemon_shutdown`, `events_subscribe`, `cancel` and `ops_list` are control
methods. Progress notifications are counters only (sync folder/message counts, index x/y, outbox
state); a slow subscriber loses events instead of growing a backlog. `cancel` sets the engine's
cooperative sync flag, so it cannot bypass a completed write; an operation already inside a network
transfer stops at the next safe checkpoint.

The scheduler wakes on the next due instant, not on a poll loop. It runs the P7 outbox pass (due,
lease, append) as soon as a row is due or a mutation notifies it; sync on start and on a jittered
interval with no parallel run per account; P9c rules after each successful sync (inside
`Engine::sync_now_with_cancel`); one 500-row P9a index batch per tick while the index is pending, with
an `index` event per batch; and a bounded, resumable blob-GC batch after a successful sync and on the
GC interval. A sync is split into message-bounded chunks (default 1 000); the engine mutex is released
between chunks and the scheduler drains a due outbox row and one index batch before the next chunk.
GC rechecks `attachments.blob_hash` and `outbox.raw_blob_hash` for every candidate before unlinking,
so a draft, a sent snapshot or a forwarded attachment is never deleted.

Fallback matrix (P10a1):

| Client | Daemon absent | Daemon present |
|---|---|---|
| App | in-process, holds the writer lock | daemon client (P10a2): read-only store, mutations through `op`, view state preserved across reconnect |
| CLI read | local store, opened read-only, no writer lock | through `op` |
| CLI mutation | starts the daemon, then through `op`; `--in-process` locks and is refused if the daemon owns the store | through `op`; `--in-process` is refused |
| MCP read | local cache, opened read-only | through `op` |
| MCP mutation | readable tool error, never starts a daemon | through `op`, send gate still applied |

A 25 MiB attachment is ~35 MB of base64 and does not fit the 16 MiB IPC frame. P10a1 preflights the
request before writing a frame and refuses an oversized attachment with a specific readable sentence;
it also replaces an oversized response (for example a dry-run of a large draft) with a same-id
`FRAME_TOO_LARGE` error and keeps the connection. The effective over-IPC attachment size is roughly
12 MiB of bytes; an out-of-band blob path is still future work.

### Planned crates

| Crate | Responsibility |
|---|---|
| `postino-crypto` | HKDF-SHA256 subkeys, XChaCha20-Poly1305 seal/open, zeroising key type (P4b) |
| `postino-store` | SQLCipher schema, migrations, repository (P3b); encrypted content-addressed blobs (P4b); FTS5 search and schema v7 (P9a); thread cache and saved searches (P9b); local rules, candidate query and journal (P9c); PGP key rows and schema v8 (P12c1); raw PGP/MIME retention and schema v9 (P12c2a) |
| `postino-mime` | MIME parse/serialize, HTML-to-text, sanitization, header-injection guards (P4a; attachment blob storage was P4b) |
| `postino-net` | IMAP4rev1 client (P5a) and SMTP submission client (P7): TLS, bounded responses, testable against scripted servers. One bounded GET for the release feed behind `update_check` (P14a). Autodiscovery and QRESYNC later |
| `postino-sync` | Reconciliation (P5b) and submission serialization / Sent append (P7); scheduler later |
| `postino-compose` | block model → email-safe HTML, CSS inlining, signatures, templates |
| `postino-rules` | Sieve generation later; P9c's local rules deliberately live in `postino-store` and `postino-core`, not in a new crate |
| `postino-ipc` | JSON-RPC over pipe/socket, token, versioning |
| `postino-daemon` | the one writer: store ownership, scheduler, outbox, rules-after-sync, index batches, blob GC, IPC server (P10a) |
| `postino-platform` | keyring (P3b); tray, desktop notifications and per-user autostart (P10b1); paths and single-instance activation (P10b2) |
| `postino-testkit` | fake IMAP/SMTP, TLS fixture, response builders and synthetic messages (P5b; dev-dependency only) |

Layers: foundation (platform, store, mime, crypto) → network (net) and sync → features (compose,
rules) → engine → frontends. `postino-net` depends on no other workspace crate: it speaks to the
server, and the sync layer above it decides what to keep. `postino-core` stays as the façade and
operation registry. Nothing below the frontends depends on egui.

## 4. Data model

```
account(id, email, display_name, provider)
folder(id, account, name, kind, uidvalidity, uidnext, highest_modseq)
message(id, account, folder, uid, message_id, thread_id, from, to, cc, bcc,
        subject, date, size, flags, remote_images, blob_hash, auth_results,
        search_sender, search_to, search_cc, search_bcc, attachment_names,
        subject_norm, in_reply_to, references_json, ingested_at, rules_applied_at, origin)
thread(id, account, subject_norm, last_date, unread, message_count)
thread_alias(account, old_id, new_id)
saved_search(id, account, name, query, sort)
rule(id, account, name, name_key, enabled, priority, stop, query, actions_json)
rule_run(id, account, started_at, finished_at, dry, counts)
rule_action(run, ordinal, rule, message, action, value, changed)
message_fts(subject, search_sender, search_to, search_cc, search_bcc, body_text, attachment_names)
attachment(id, message, filename, mime, size, blob_hash)
draft(id, account, revision, updated_at, blob_hash, in_reply_to)
draft_seq(message_id, revision)
outbox(id, account, draft, idempotency_key, created_at, send_at, state, attempts, append_attempts, last_error, raw_blob_hash, envelope_json, lease_at)
rule(id, account, priority, enabled, match_json, action_json)
signature(id, account, identity, name, html)
contact(id, account, display_name, email, vcard)
sync_state(account, folder, uidvalidity, last_uid, modseq)
```

- P8 added no migration on this store: the draft reply link is the shipped `drafts.in_reply_to`, the
  account signature is the shipped `accounts.signature`, and a received `Reply-To` is cached under
  `meta` key `reply_to:<message_id>`. P9a's v7 now also stores the incoming `In-Reply-To` and
  `References` chain for P9b; `Reply-To` still rides `meta`. The future `signature` table for HTML
  signatures remains unimplemented; P13i instead stores up to 10 alternative From senders per account
  as encrypted `meta` rows, so no migration was needed.

- The **server is the source of truth**. The local database is a cache plus an operation log.
- Attachment bytes are content-addressed blobs: BLAKE3 of the plaintext names the file, zstd
  compresses it when that helps, and XChaCha20-Poly1305 seals it with a subkey of the store key. They
  live in `blobs/` next to the database, never in SQLCipher pages. Message bodies stay in the
  encrypted database in M1; §4.5 explains why.
- FTS5 indexes subject, people and extracted body text. Tokenization is Unicode-aware.
- Threading uses `References`/`In-Reply-To` with a JWZ-style fallback, plus provider thread ids
  when available.
- Nothing is deleted from the server unless the user asks. "Trash" is a local view and an IMAP
  move; expunge is explicit.

### 4.1 Implemented in P3b, revised in P3d: schema v3, keyring and repository

The first real persistence layer exists as two crates; P3c wires them into the engine.

**Key flow.** `crates/platform` owns the only keyring read in the product:

```
postino --store <path> ── load_or_create_key("default") ──▶ postino-platform
                                              │ service = "postino", user = scope
                                              ▼
                                   Windows Credential Manager
                                              │ 32 random bytes (getrandom) on first run
                                              ▼
                                          StoreKey
                                              │ as_bytes()
                                              ▼
                       postino_store::Store::open(path, &key)
```

`load_or_create_key` fails closed: no vault, a failed read, a secret that is not 32 bytes or a
failed RNG is an error, never a derived key or a plaintext file. The whole load-or-create is
serialised with an OS file lock at `<app local data>/Postino/keyring.lock`: the first read and, on a
miss, re-read → generate → write → read-back all happen while the lock is held, so a second process
cannot observe a key between another process's write and read-back. A read-back mismatch adopts the
stable 32-byte key the vault actually holds, because an existing store may already be encrypted with
it; a vault that is still changing after one retry fails closed with `ReadBackMismatch`. `StoreKey`
is redacted in `Debug`, has no `Clone` and zeroises its bytes on drop; the SQLCipher pragma is built
in a zeroising string sized for the exact final length, so it never reallocates. SQLCipher's
internal copy of the key is the remaining gap, recorded in `docs/store-spike.md`. The store crate
never reads the keyring, which is what lets its tests run with a random in-memory key. Tests for the
platform crate install `keyring_core::mock`, pass a temporary lock path and never touch the real
vault or `%LOCALAPPDATA%\Postino`.

**Platform note.** Windows Credential Manager is the only native keyring backend in this build
(`windows-native-keyring-store`). On macOS and Linux `install_native_store` returns
`PlatformError::Unsupported`: the store key and every account credential fail closed at the seam,
there is no weaker file fallback, and there is no platform-equivalent default store yet. Native
macOS/Linux keychains land with the packaging milestone. `postino-platform` owns both scopes:
`default` for the store key, `account:<id>:imap` for an IMAP credential and `account:<id>:smtp` for an
optional separate submission credential.

**Schema v3** (`crates/store/src/lib.rs`): v2 plus `draft_seq(message_id PRIMARY KEY, revision)`,
which holds the draft revision high-water mark independently from the draft row. Migration v3 seeds
it from `drafts.revision`, so an existing store keeps its revision numbers.

**Schema v2** (`crates/store/src/lib.rs`, migration index 1, additive from the P3a `meta` table):

- `accounts`, `folders`, `messages`, `attachments`, `labels`, `message_labels`, `drafts`,
  `sync_state` plus `meta`.
- Foreign keys are enforced on every connection. Label deletion detaches assignments explicitly;
  message deletion cascades to attachments and assignments.
- `PRAGMA journal_mode = WAL`, `synchronous = FULL`, 5 s busy timeout, in-memory temp store.
- **All dates are Unix milliseconds** (`i64`). Milliseconds match `SystemTime` and the demo clock,
  sort and index as integers, need no locale, and avoid the 2038 problem of 32-bit seconds. The
  store never sees a formatted date; IMAP and MIME convert at the sync boundary. `NULL` is reserved
  for genuinely unknown values (`uid_validity`, `uid_next`, `last_sync_at`).
- Recipient lists are JSON arrays in `to_json` / `cc_json` / `bcc_json`, in the same shape as the
  core `Address`, so P3c maps without a second format.
- Hot-read indexes: `messages(folder_id, date DESC)`, `messages(account_id, date DESC)`,
  `message_labels(label_id)`, `attachments(message_id)`, `drafts(updated_at DESC)`,
  `folders(account_id)`. Mailbox-wide label names get a partial unique index because SQLite treats
  `NULL`s as distinct in `UNIQUE(account_id, name)`.

**Repository** (`crates/store/src/repo.rs`) mirrors today's operations with plain structs that do
not depend on `postino-core`: accounts upsert/list/get; folders upsert/list/get/recount; messages
upsert/get/list with folder, label, unread, starred and attachment filters plus limit/offset, flag
batch, move, delete and counts; labels CRUD with live counts, batch apply/remove, messages by label;
attachment replacement/listing; drafts save/get/list/delete with store-owned revisions; meta; and the
sync support added in P5b: `upsert_synced_message` (message and attachment rows in one transaction,
without a per-message recount), `message_flags`, `list_uids`, `delete_synced_messages` (never a local
draft), `reset_folder_messages` (a UIDVALIDITY reset that keeps local drafts) and the
`sync_state` cursor read/write/clear. Every multi-row mutation is one transaction, and a bad id rolls
the batch back. Message upserts leave labels and attachments alone unless the caller replaces them,
and a message cannot be placed in another account's folder (`AccountMismatch`).

`save_draft_message` is the one whole-draft transaction: it validates attachment ids, upserts the
message row, replaces attachments, writes the `drafts` revision and `draft_seq`, and refreshes the
Drafts folder counts before commit. `send_draft_message` moves the message to Sent and deletes only
the `drafts` row, keeping `draft_seq` for a later move back. `delete_message` removes the message,
its attachments/assignments by cascade, the `drafts` row and the sequence together. Label creation
raises `meta.label_seq` in the same transaction as the label row; deleting the highest label never
lowers it.

Folder `unread`/`total` are a cache refreshed from `messages` by mutations and repairable with
`recount_folder`. The list query returns `total`/`unread` for the filtered scope before pagination
and clamps pages to 1..=500 rows.

### 4.2 Implemented in P3c, hardened in P3d: the engine on the store

`Engine` holds `store: Option<postino_store::Store>`. `demo()` and `demo_at()` leave it `None` and
behave exactly as before; `open(store)` hydrates the mailbox and keeps the connection for
write-through. The store crate does not depend on core: `crates/core/src/store_map.rs` is the only
translation between domain types and store rows.

**Load once.** `Engine::open` issues one query per table: accounts, folders, summaries, attachment
id/hash metadata, labels and draft metadata. There is no per-message read. Since P9a the list rows are
`MessageSummary` values: bodies, HTML and full attachment rows are read only by `Engine::message(id)`,
which owns the result and keeps a revision-keyed cache (§4.11). Before hydrating, it repairs every
`drafts` row that has no
message row into a real message row in the Drafts folder of its account, in one transaction, using the
body and recipients from the revision row. If an account, folder or date cannot be repaired, the open
fails with an actionable error instead of serving a ghost draft. The label sequence is read from
`meta.label_seq`, seeded from `max(meta, existing label ids)` and written back when an older store lacks
it; the next label id is therefore never one that was already used.

**Write through.** Every mutating operation writes the repository first and only then updates
memory; a failed write leaves memory untouched. `save_draft` is one `save_draft_message`
transaction (message row, attachments, revision row and Drafts folder counts); a cross-message
attachment id or a repeated one is refused before the first write. `send_draft` is one
`send_draft_message` transaction that moves the message to the Sent folder of the draft's own
account and deletes only the draft row, keeping its revision sequence. `delete_message` removes the
draft row and sequence in the same transaction. Also wired in P3c: `set_flags`, `move_message`,
label create/update/delete and label apply/remove. Reads (`message_get`, `messages_list`, `search`,
`labels_list`) never mutate. Account and folder writes were the P6 seam; §4.7 fills it with
`account_add`, `account_remove` and the store transaction behind them.

Caller mistakes are `OpError::InvalidArgs` with a specific sentence whichever layer notices them
first: the engine's in-memory pre-checks and the `store_map` boundary agree on unknown account,
folder, message and label ids, duplicate names, account mismatches and repeated or cross-message
attachment ids. I/O, wrong key, corruption, schema-too-new and missing configuration are
`OpError::Failed` with the storage advice and a "rolled back, memory untouched" statement, which
every engine write can honour because each is one transaction.

**Counts.** The store keeps `folders.total`/`unread` current inside its own transactions; the engine
recomputes its in-memory counts after each mutation instead of re-reading the store. The virtual
`starred` folder is derived from message flags and is not a meaningful row in the store's folder
cache; the engine is authoritative for it.

**Residual windows.** Each engine mutation is one repository transaction, so a crash or disk
failure cannot leave the store one step ahead of memory within an operation. Activations and the
desktop app both use the store now; the engine writes attachment bytes only through the P4b seam, and
it writes local drafts with `uid`, `Message-ID` and `size` empty. Received mail is different:
`postino-sync` (P5b) fills those columns, parses the MIME body and stores attachment bytes through
the same seam. The keyring lock protects every keyring access, not the store file itself. P10a added
the independent `<store>.lock` writer guard and the daemon that holds it; with no daemon running the
app still owns the store in-process and its sync worker opens a second connection in that same process. The
repository returns folders and labels ordered by name; onboarding writes the server's folder tree in
that order.

**Still in memory / demo:** the desktop app, the composer's view state, and any engine built without
a store. **Still missing:** the block composer mode (M5), the optional sandboxed webview (P11b) and
the server-side Drafts APPEND. Server-side
Sieve/ManageSieve is not implemented and is out of scope for M3 (a possible later M6 item); local
rules (§4.13) never generate a Sieve script.
The default store path, the account/sync operations and their CLI/MCP bindings landed in P6a (§4.7);
the body-free read path and search landed in P9a (§4.11); conversations and saved searches landed in
P9b (§4.12); local rules landed in P9c (§4.13).

### 4.3 Implemented in P4a: the MIME crate

`postino-mime` is a foundation crate: it parses, sanitizes and serializes, and it opens no socket,
store or database. The engine still returns demo bodies until P4b wires `parse` into the repository.

```
raw bytes ──▶ parse (bounds, mail-parser) ──▶ Headers + body parts ──▶ sanitize HTML (ammonia)
                                                                  └──▶ html_to_text alternative
OutgoingMessage ──▶ validate headers ──▶ deterministic boundaries ──▶ RFC 5322 bytes + envelope
```

**Parsing.** `parse(raw, &Limits)` checks the raw size, the header-block size and a
`Content-Type: multipart/...` pre-scan before the parser runs; `mail-parser` is then configured with
the depth and part limits. Missing boundaries, truncated base64, unknown charsets and excessive
nesting become `Warning`s, not a failed message. `ParsedMessage` exposes decoded headers, standard
address fields, `text`, sanitized `html`, warnings, `remote_images_blocked` and bounded attachments;
`message/rfc822` parts stay metadata-only. Bodies and attachments are disjoint: a part chosen as a
body is never repeated as an attachment.

**Sanitization.** `ParsedMessage::html` is always ammonia allowlist output. Remote images are
removed; `http`/`https` sources are counted, `cid:` images survive with their `Content-ID`, and
`javascript:`, `data:`, `vbscript:`, `file:`, relative and protocol-relative URLs never survive.
CSS is not supported at all: this crate is the seam, not a second sanitizer. `html_to_text` is
public and gives the plain alternative when a message has only HTML. P6d carries
`remote_images_blocked` through sync into `messages.remote_images`; the count is metadata, not
content, and the sanitized HTML never keeps a reference from which it could be re-derived.

**Serialization.** `serialize(&OutgoingMessage)` owns header folding, RFC 2047 B-encoding,
quoted-printable, base64, RFC 2231 continuations, deterministic boundaries from a content hash and
the generated `Message-ID`. Header injection is `MimeError::HeaderInjection`, never a strip. `Bcc`
is validated, deduplicated into `Envelope.recipients`, and never appears in `raw`.

**Next.** P4b stores the bounded attachment bytes as encrypted blobs and exposes the engine seam;
P5b maps `ParsedMessage` onto store rows and feeds those bytes from `postino-sync`. The engine's own
local drafts still carry a plain-text body. P5 supplies the IMAP bytes and P7 uses `serialize` for
submission. The crate has no dependency on `postino-core`, so the mapping stays in the sync layer.

### 4.4 Implemented in P5a: the IMAP client

`postino-net` is the network layer and nothing above it. It owns one TCP connection, its TLS
session and the IMAP conversation; it does not choose what to download, write the store or run a
loop. Bodies leave it as bytes, unparsed.

```
ImapConfig ──▶ TCP ──▶ TLS ──▶ greeting ──▶ CAPABILITY ──▶ login ──▶ LIST / SELECT / FETCH / STORE / IDLE
                                            └─ STARTTLS ──▶ TLS ──▶ CAPABILITY (pre-TLS state discarded)
```

**TLS policy.** Two modes and no third: implicit TLS (993) and STARTTLS (143). There is no
plaintext mode and no option that skips verification. Verification is the operating system's
through `rustls-platform-verifier` — CryptoAPI on Windows, the Security framework on macOS, the
system trust store through WebPKI elsewhere — with `ring` as the crypto provider, so the crate
adds no OpenSSL and no C build beyond `ring`. `ImapConfig::extra_roots` adds trust anchors, which is
how the test server is trusted; it augments the platform verifier and cannot disable it. A
certificate failure reports the host and the verifier's reason, never the certificate bytes.

STARTTLS is not a suggestion: when the mode is selected and the server does not advertise
`STARTTLS`, the session stops with `Unsupported`. After the tagged `OK`, bytes already buffered on
the socket are plaintext sent before the handshake — a command-injection attempt — and are refused
rather than parsed. Capabilities collected before the upgrade are discarded and asked again over
TLS, so a server cannot downgrade what it claims.

**Command surface.** `CAPABILITY`, `AUTHENTICATE PLAIN` (preferred) or `LOGIN`, `ID` when the server
advertises it (RFC 2971, so an account's access history can name the client), `LIST "" "*"`,
`SELECT`/`EXAMINE`, `UID FETCH ... (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)`,
`UID FETCH <uid> (BODY.PEEK[])`, `UID STORE` with `+FLAGS.SILENT`/`-FLAGS.SILENT`/`FLAGS.SILENT`,
`NOOP`, `IDLE` with a single `DONE` cycle, `LOGOUT`. Tags are `A0001`… and an unexpected tag is a
`Protocol` error. Bodies are never fetched with `BODY[]`: `BODY.PEEK[]` keeps reading from mutating
`\Seen`. Modified UTF-7 is decoded for display and the wire name is kept for subsequent commands.
A FETCH is checked back against the request: a body whose UID differs is refused, as is an envelope
whose UID is outside the requested set, so a confused server cannot hand over the wrong message. The
`UID FLAGS` liveness scan is the exception: it skips an out-of-set UID, because the `<UIDNEXT>:*`
probe legitimately returns the last live message (RFC 3501 §6.4.8) and an unsolicited flag update
carries a UID nobody asked for.
`SELECT` reads `READ-ONLY`/`READ-WRITE` from the tagged response as well as from untagged ones.

**Bounds.** A response line is at most 64 KiB, a literal 25 MiB by default (configurable, hard
ceiling 64 MiB), untagged responses per command 10 000, folders per `LIST` 5 000, a command line
8 KiB, and one command may consume at most 64 MiB of response bytes (hard ceiling 128 MiB). Fixed
parser limits cap nesting at 32 levels, literals per response at 4 096, atoms and mailbox names at
1 KiB, and numbers at 20 digits; a number is parsed, never wrapped. Exceeding a bound is
`TooLarge` or `Protocol`, never a hang or an unbounded allocation. Every phase has a timeout
(DNS, connect, TLS handshake, greeting, each command, `DONE`); a timeout closes the session and the
next call fails as `Closed` instead of reading a half-command. A bound or parse error is a
desynchronisation too — the client stopped inside a response — so it closes the session and drops the
socket rather than reading the next command from the middle of the previous one. `IDLE` ends normally
only when the wait expires with a quiet socket: a missing `+`, a half-read line or literal, a
malformed response and a bound error all close it, and its `DONE` half enforces the same untagged cap.

**Security.** The password is a `Secret` that zeroises on drop and prints `[redacted]`, and it is
dropped after a successful login. The SASL payload and the `LOGIN` line are built in `Zeroizing`
buffers. `ImapConfig`'s `Debug` keeps the transport and redacts the username as well, because an
address is personal data. Logs carry the tag and the command verb, never arguments, addresses,
bodies or credentials. Server text in errors is cut to printable ASCII.

**Not implemented.** Sync loops, store writes, engine operations, `COPY`/`MOVE`/`EXPUNGE`,
QRESYNC/CONDSTORE beyond reading `HIGHESTMODSEQ` when the server volunteers it, SORT/THREAD,
METADATA, ACL, SIEVE, NOTIFY, ESEARCH, `BODYSTRUCTURE` and per-part fetches, SASL mechanisms other
than PLAIN, LOGIN and XOAUTH2, autodiscovery and MIME parsing. The fake server and its TLS
fixture live in `postino-testkit` (P5b), a dev-dependency shared with `postino-sync`; the reconciler
that consumes this client is `postino-sync`, not part of this crate.

### 4.5 Implemented in P4b: encrypted attachment blobs

Attachment bytes are the largest and most sensitive payload in a mailbox and they are write-once.
They do not belong in SQLCipher pages: `postino-store` keeps one file per distinct content next to
the database and seals it with a subkey derived from the same store key.

```
D:\mail\mail.db                      SQLCipher: metadata, subjects, bodies, rows
D:\mail\mail.db.blobs\0a\0a3f…c9.blob  XChaCha20-Poly1305: one attachment content, once
```

**Layout.** `Store::open` creates and opens `<db file>.blobs` after the schema is readable and
before the engine sees the store; a directory that cannot be created fails the open instead of
losing attachments later. Every blob is `mail.db.blobs/<first two hex chars>/<hash>.blob`, where the
hash is the lowercase BLAKE3 hex of the **plaintext**. The namespace is derived from the database
file, not its folder, so two stores in one directory never share blobs or keys. Moving a store means
moving the database and its `.blobs` directory together; renaming only the database starts an empty
blob directory, and the old attachment rows read as `BlobMissing`. A relative database path resolves
next to itself (`mail.db` → `mail.db.blobs`). The hash is validated as exactly 64 lowercase hex
characters before any path is assembled, so `..`, separators, Windows stream and UNC forms, trailing
dots/spaces and uppercase hex are `BadHash` and never reach the filesystem; the error reports only
the length, never the rejected string.

**Header, version 1.**

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `PSTB` |
| 4 | 1 | version `1` |
| 5 | 1 | flags: bit 0 = the payload is zstd-compressed; every other bit must be zero |
| 6 | 8 | `original_len`, `u64` little-endian: the plaintext length, never the payload length |
| 14 | 24 | XChaCha20-Poly1305 nonce |
| 38 | n | ciphertext with a 16-byte Poly1305 tag |

**Associated data.** `magic || version || flags || original_len || hash bytes`. The hash binds the
file to its name, so a blob cannot be renamed to another hash or swapped with another attachment;
the length binds the plaintext size, so a truncated or extended file is rejected instead of being
decrypted with the wrong size. The flags byte is in the AAD even though a minimal AAD could omit it:
flags decide whether the payload is decompressed, and authenticating the length but not the
interpretation would let a local attacker flip one clear byte and change what the reader returns.

**Compression.** zstd level 3, used only when the compressed frame is smaller than the plaintext
(flag bit 0). Decompression always goes through `zstd::bulk::Decompressor` with a buffer of exactly
the declared plaintext length; `decompress_all` is never used. A blob that declares more than the
caller's `max_bytes` is refused **before** decryption, and `MAX_BLOB_BYTES` (64 MiB) is a hard
ceiling whatever the caller passes. The engine reads with `recorded size + 1 MiB`, so a corrupted
length field cannot become an allocation.

**Dedup and integrity.** The file name is the content hash, but neither `put` nor `get` trusts it.
`put` authenticates an existing file and compares its plaintext with the new bytes before it reports
a dedup hit; a corrupt or foreign file at that name (wrong key, damaged tag, planted content) is
healed by the publish, which makes `put` the repair path and removes the old "remove it and retry"
recovery step. `get` recomputes BLAKE3 over the decrypted plaintext and compares it with the
requested name, so content sealed under a false name by someone who holds the subkey is
`CorruptBlob`, not mail. Both checks run inside the same bounded read, with the declared length capped
before allocation. The price of an authenticated dedup hit is one decrypt-and-hash of the existing
file, bounded by the candidate length; it is the trade for a blob namespace that heals itself without
a separate repair command.

**Keys.** The blob key is `HKDF-SHA256(salt = "postino.v1", ikm = store key, info = "blob")`, held
in `postino_crypto::Subkey`: no `Clone`, redacted `Debug`, zeroised on drop. A nonce is 24 random
bytes from the OS RNG for every seal. A different store key, a different label or a different AAD is
`CorruptBlob`; the error never says which, and it never contains plaintext or key bytes. Key
rotation does not exist yet: rotating the store key makes every existing blob unreadable, and that
is recorded in `docs/threat-model.md`.

**Writes and durability.** A put writes `.tmp-<random>` in the target shard with `create_new`,
`sync_all`s it and publishes it with `hard_link`, which creates the final name atomically and never
removes an existing one: a concurrent reader sees either the complete old file or the complete new
one. An existing name holding the same bytes is a dedup hit; invalid or foreign bytes are healed with
a replacing rename (the one path where a reader can observe a gap, so `get` retries a missing name
for up to 8 ms before reporting `BlobMissing`). A filesystem without hard links falls back to the
replacing rename for every publish. A failed write removes its temp. After publishing, the shard
directory is fsynced on Unix; Windows has no portable directory fsync and this crate adds no unsafe
Win32 call, so it reopens the published file and flushes it. The remaining Windows window is the
directory entry: a power loss in the microseconds after the publish can lose the name, which reads as
`BlobMissing` on the next open (never wrong bytes) because the engine commits the attachment row only
after the put returns. Identical bytes are stored once (dedup), and a put of already verified bytes
writes nothing.

**What is encrypted where.**

- The database: pages, WAL frames and temp store through SQLCipher. Subjects, addresses, bodies,
  flags, labels, attachment names/types/sizes and their hashes are ciphertext.
- The blob: everything after the header. The header (magic, version, flags, original length, nonce)
  and the file name are clear by design: a reader has to bound the file before it allocates or asks
  for the key, and the hash has to be the name for dedup to work. A local attacker therefore learns
  an attachment's size class and can confirm whether a candidate file is in the mailbox; they do not
  learn the content.
- **Message bodies stay in the encrypted database in M1.** A body is small (kilobytes to a few
  hundred kilobytes), is read in random order by the list and reading paths, and is about to feed
  FTS5. Moving it into a blob would add an open/seek/decrypt round trip and a second failure mode
  without a size win. Only the write-once, streamed payloads leave the page store; P9 may move raw
  messages for index rebuilds, and §4.5 would then grow a second context label.
- Not recoverable by Postino, and documented rather than hidden: swap and crash dumps, SQLCipher's
  internal key copy, the cipher's expanded key during a call, the transient buffer of a rejected
  ciphertext and orphaned blob files. `docs/threat-model.md` §4.3 is the authoritative list.

**Engine seam (no operation).** `Engine::attachment_bytes(attachment_id)` reads through the store
(P6 reading pane), capped at the recorded size plus 1 MiB. `Engine::store_attachment_bytes` takes
`(message_id, attachment_id, bytes)` and is repository-first, memory-second: put the blob, update the
attachment row's `blob_hash` and exact `size`, then memory. A failed row write leaves the row and the
message untouched; the blob file may remain as an unreferenced, deduplicated file (and is reused by
the next attempt). `has_attachments` never changes on this path because the row already exists. A
draft re-save replaces the attachment rows from metadata that carries no hash and may carry a stale
size; the repository keeps the stored hash **and** the stored size for an id that survives, and the
engine re-reads the rows after the write so memory cannot disagree with the bytes on disk. The demo
engine and a store-less engine answer with a readable `OpError::Failed`. Consumers: sync (P5b) now
calls the write for every ingested attachment; the reading pane (P6c) calls the read to preview
(P13b) or save.

**Cleanup.** Deleting a message leaves its blob file, because another message may reference the same
content; the daemon's bounded, resumable GC batches sweep unreferenced hashes (P10a1).
`BlobStore::remove` exists for an explicit delete and for tests, not as a repair step.

### 4.6 Implemented in P5b, hardened in P5c: syncing an account into the store

`postino-sync` is the reconciler between one `postino-net` session and the encrypted repository. It
ships no operation, CLI command, scheduler or screen: `sync_account(store, account, config, limits,
cancel)` is called by whoever owns those in P6. It runs a private current-thread Tokio runtime and
blocks; the `Store` stays on the caller thread and one connection serves the whole run.

```text
LIST "" "*"                 ──▶ folders, special-use, \Noselect skipped
SELECT <wire name>          ──▶ UIDVALIDITY, UIDNEXT (a hint only)
  UIDVALIDITY changed       ──▶ drop that folder's cached messages, cursor = 1
UID FETCH <hint>:* (UID FLAGS)       ──▶ probe the true maximum UID
UID FETCH <cursor>:<end> (…ENVELOPE) ──▶ explicit windows below the true maximum
UID FETCH <uid> (BODY.PEEK[])        ──▶ postino-mime ──▶ one transaction: message + attachments
UID FETCH <a>:<b> (UID FLAGS)        ──▶ full liveness/flags scan, 1 000 UIDs per window
deferred deletion                    ──▶ after every folder: drop missing ids seen nowhere
recount                              ──▶ folder counts, last_sync_at
```

**Remote-content count.** The sanitizer returns `ParsedMessage::remote_images_blocked`, and the
message transaction writes `messages.remote_images` (capped at 10 000) beside the body. The stored
HTML has no remote `src`, so this count is the only surviving fact: the reading pane renders it from
the message and never scans `body_html`, and `message_get` exposes it. No remote URL is fetched,
logged or re-added anywhere in the flow; a re-sync replaces the count with the newly parsed one.

**UID discovery and the cursor.** `UIDNEXT` is a hint, never truth: a missing or stale value hides
mail above a gap, and `EXISTS`/`RECENT` are counts, not liveness. After `SELECT` the reconciler
probes `UID FETCH <max(cursor, UIDNEXT)>:* (UID FLAGS)`; the highest UID returned is the true upper
bound, and UIDs below the hint are the RFC 3501 §6.4.8 `*` artifact that do not extend the new-mail
range. If the probe is empty and the hint is ahead of the cursor, the bound is `hint - 1`; with no
`UIDNEXT` the bound is `cursor - 1` and there is no new mail. New mail is fetched in explicit windows
of `batch` UIDs below that bound, never with an open `*` end, so a response cannot exceed the
untagged cap. `sync_state.uid_next` (the next UID to fetch) advances after each committed message and
after each completed window; `folders.uid_next` holds the server's advertised value. `EXISTS 0` does
not skip a folder. A crash between the message transaction and the cursor write re-fetches one
message; the upsert is idempotent.

**Folders.** `LIST` once. `\Noselect` is skipped; `\Inbox`, `\Sent`, `\Drafts`, `\Trash`, `\Junk`,
`\Archive` and `\Flagged` map to the store roles, and a folder named `INBOX` is the inbox even when
the server omits the attribute (RFC 3501 fixes the name). Everything else selectable is `Custom`.
Roles are never guessed from names: a server with no `\Sent` or no `\Drafts` gets a warning instead of
an invented folder. `\All` and `\Flagged` are aggregate views (RFC 6154): their local rows are created
so the role is visible, but their messages are not fetched, because they contain mail that already
lives in a concrete folder and a mailbox-wide `Message-ID` identity would otherwise move every
message into the view. The local folder id is `fld_` plus 16 hex of `BLAKE3(account, wire name)`, so a
display-name rename does not orphan the cache. Local labels and drafts are untouched. A server's
`\Flagged` folder is stored as `FolderKind::Starred`; the engine's virtual starred view is derived
from message flags, so P6 must decide which of the two the sidebar shows.

**Message ids.** The hash input is the exact trimmed Message-ID: one pair of angle brackets removed,
nothing else changed (no case-folding, no first-token shortcut, no character substitution). `msg_`
plus 16 hex of `BLAKE3(account, value)` is the mailbox-wide identity; a message without a usable
header uses `msg_{folder}_{uidvalidity}_{uid}`. A second distinct message with the same Message-ID
in the same folder cannot share that id: envelopes are processed in ascending UID order, so the
second gets `msg_<hash>_<uid>`, and if that is taken (a duplicate in another folder with the same
UID) `msg_<hash>_<uid>_<uidvalidity>`. The choice is deterministic and stable while the UID stays;
if every candidate is taken the message is skipped with a warning rather than overwriting another
row. The same Message-ID in another folder is a move: the row keeps its id and labels and the later
folder wins `folder_id`. A message without a Message-ID that moves gets a new id, so its labels
cannot follow; the old row is removed only after the new copy exists, at the end of a complete run.

**Threading.** Sync stores the bounded `References` chain and `In-Reply-To` (no angle brackets) with
the message, and writes a provisional thread id (first reference, else parent, else own Message-ID or
local id). `postino-store` re-derives the id from the cached conversations in the same transaction
(P9b, §4.12): phantom roots, late-parent joins, merges and aliases live in the store, not in the IMAP
client. The sync layer never invents a server-side thread; IMAP `THREAD` is not used.

**Flags.** Server flags are applied only when a message is first inserted: `\Seen` becomes
`unread = false`, `\Flagged` becomes `starred = true`. On later runs the local `unread`/`starred` are
kept because M1 flags are local-only and M2 will push them. The full UID/flags scan feeds drift
detection in memory: one `folder_message_flags` query returns every cached id, UID and local
read/star state, and each `UID FETCH a:b (UID FLAGS)` window is compared with that snapshot. There is
no per-message flag query. `SyncOutcome::updated_flags` counts distinct messages per run; one warning
says local flags were kept. Sync never sends `STORE`, so a run cannot change the user's mail state on
the server.

**UIDVALIDITY.** A different value means every cached UID describes a mailbox state that no longer
exists. `reset_folder_messages` deletes the folder's server messages, attachments and label
assignments in one transaction (blob files stay because another message may share the content),
clears the folder's `sync_state` and recounts the folder; the run records a warning and fetches from
UID 1. Rows without a UID survive every reset: local drafts and locally sent mail were never on the
server, and rebuilding its cache must not discard them.

**Deletions are deferred and account-wide.** A per-folder deletion pass cannot tell a deleted message
from one moved to a folder processed later: it would drop the row, and its labels, before the target
copy was read. So each folder proposes `missing = cached_uids - live_uids` and the run deletes only
after every folder has been scanned completely. Every id ingested anywhere in the run is tracked; a
missing id seen in another folder is a move and is never deleted. A run stopped by `max_messages`, a
cancellation or a folder that could not be selected deletes nothing. Each folder's deletion is one
`delete_synced_messages` transaction (local drafts and rows without a UID are never candidates), and
the server is never touched: no `EXPUNGE`, no `STORE`, no `DELETE`.

**Attachments.** Each MIME part gets a deterministic id `att_{message_id}_{index}`. Bytes go through
`Store::blobs().put` only when the part was decoded completely and is within
`SyncLimits::max_attachment_bytes`; the row then carries the BLAKE3 hash and the exact stored size. An
oversized or truncated part keeps its metadata with `blob_hash = NULL` and produces a warning: the
message is still readable. The effective single-part cap is the smaller of the sync limit and the MIME
crate's 32 MiB hard part limit; the blob format's 64 MiB ceiling is the storage backstop.

**MIME warnings.** Every `ParsedMessage.warnings` entry is mapped into the run warnings, prefixed with
the local message id and deduplicated per run. A part-limit or depth warning therefore says that the
attachment list may be incomplete instead of letting a truncated message look whole; a truncated body
or a sanitizer strip is visible for the same reason.

**Status.** `meta` key `sync_status:<account_id>` holds
`{"state":"idle|syncing|error","last_attempt":ms,"last_success":ms|null,"error":sentence|null,
"folders_done":n,"folders_total":n,"messages":n}`. It is written when a run starts, after each folder
and at the end; a crash leaves `syncing` and the next run resets it. `error` is a fixed sentence for
the failure category, never a server name, an address or message text; `read_status(store, account)`
is the read-only helper for P6 and the CLI. `messages` counts messages ingested by the run, including
those committed before a cancellation.

**Cancellation.** The caller passes an `AtomicBool`; it is checked before connecting, before each
folder, between new-mail batches, between UID/flags scan windows and before each message. A cancelled
run returns `SyncError::Cancelled` and every committed message stays. The next run resumes from
`sync_state.uid_next`; the whole-account deletion is skipped because no run that stopped early deletes
anything.

**Limits and counts.** `SyncLimits` defaults are 100 envelopes per fetch, 25 MiB per message, 25 MiB
per attachment and 50 000 messages per run; every field is clamped to a usable range and the message
cap is further clamped to the network client's `max_literal`, so sync never chooses a body the session
would refuse. A message over the message cap is skipped with a warning and the cursor advances; a run
that reaches `max_messages` stops with a warning and resumes next time. Folder counts are refreshed
once per folder (`recount_folder`), not once per message: a full scan per upsert would make a large
import quadratic. The price is that a crash can leave `folders.unread`/`total` stale until the next
run or an explicit `recount_folder`.

**Not synced.** Server-side filters and rules, flag changes pushed out, `EXPUNGE`, `APPEND` of drafts
or sent mail, `CONDSTORE`/`QRESYNC` increments, a folder-removal pass (a folder deleted on the server
keeps its cached rows until a later package decides) and any periodic
scheduler. Threading is derived locally from the synced references (P9b), not requested from the
server; there is no `THREAD` command. P9a adds the FTS trigger path to every synced message; P6a adds the operations and the CLI/MCP bindings; P6b adds the worker, so a run starts on
launch, on Refresh, or from onboarding. P10a adds the daemon's interval scheduler; the desktop worker
stays in-process in P10a1.

**Evidence.** The scripted server moved to `postino-testkit`, and the sync tests script real protocol
exchanges: initial sync, incremental UIDs, UIDVALIDITY, deletion, flag drift, oversized attachment,
message limit, a move that keeps labels and read state, duplicate Message-IDs, missing and low
`UIDNEXT`, `EXISTS 0`, an empty mailbox, an expunged body, cancellation during the flags scan, MIME
warnings (a 300-part message), `max_messages` exhaustion with no deletion, three disconnect points,
cancellation plus reopen, `BODY.PEEK`-only and an offline read through `Engine::open`. On the Windows
GNU host in a debug build (opt-level 1, dependencies at 2), a 2 000-message sync took 9.1 s with a
23.3 MB peak working set; 10 000 messages (`cargo test … -- --ignored ten_thousand_messages`) took
37.3 s with 34.8 MB. The old per-folder deletion re-fetched 20 envelope windows at 2 000 messages;
the flag scan costs one probe plus two 1 000-UID windows, which is why the run got faster.

### 4.7 Implemented in P6a: accounts, credentials and sync operations

The backend that turns Postino from a demo into a client for a real mailbox. The CLI, MCP and the
operation registry are complete: a person can add an Aruba or Gmail account with an app password,
test it, sync it into the encrypted store, read the cached mail and remove it. P6b adds the desktop
onboarding, the boot on the store and the background worker (§4.8).

**Credential scopes.** `postino-platform` owns every keyring access under service `postino`:

| Scope | Secret | Used by |
|---|---|---|
| `default` | the 256-bit SQLCipher store key | `postino --store` / the default store |
| `account:<id>:imap` | one account's IMAP password or app password | `account_add`, `account_test`, `sync_now`, `account_remove` |
| `account:<id>:smtp` | optional separate SMTP password; absent means the IMAP one is used | `account_add`, `message_send`, `send_schedule`, `outbox_retry`, `account_remove` |

`Keyring` is a handle with an explicit lock path: production uses `Keyring::system()` (the lock lives
next to the app data), tests use `Keyring::at(temp)` and never take the real lock. Every read, write
and delete takes the same first-run file lock; a write is read back before it reports success. Loaded
secrets are `Zeroizing<Vec<u8>>`, and no `PlatformError` variant carries secret bytes. `account_scope`
accepts only ASCII letters, digits, `_` and `-` in the id, so two accounts cannot share a scope
through a `:` in an id. The store has no password column and no credential table.

The engine never calls a keyring crate: `postino-core` defines `CredentialVault`
(`store_imap_password` / `load_imap_password` / `delete_imap_password`, addressed by account id) and
the CLI implements it by mapping the id to the platform scope. Core's tests pass an in-memory vault;
the CLI integration tests never construct the real adapter. This keeps the layer table honest and
makes "no test touches the keyring" structural, not a convention.

**Schema v4** (`crates/store/src/lib.rs`, additive from v3):

```sql
CREATE TABLE account_servers(
    account_id    TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    imap_host     TEXT NOT NULL,
    imap_port     INTEGER NOT NULL CHECK(imap_port BETWEEN 1 AND 65535),
    imap_tls      TEXT NOT NULL CHECK(imap_tls IN ('implicit', 'starttls')),
    imap_username TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
```

`Store::save_account(account, server, folders)` writes all three in one transaction: onboarding cannot
leave an account without an endpoint or a half-written folder tree. `Store::delete_account(id)` runs
one transaction: messages (attachments and label assignments cascade), drafts, the server row, the
folder rows (sync cursors cascade), the `sync_status:<id>` meta value and finally the account. Labels
are mailbox-wide (`account_id IS NULL`) and are never touched. Foreign keys and `\Noselect` handling
are unchanged from P5b.

**Autodiscovery.** Offline only, no DNS/SRV probing:

| Provider | Host | Port | TLS | Note |
|---|---|---|---|---|
| `aruba` | `imaps.aruba.it` | 993 | implicit | app password when 2FA is on; IMAP must be enabled |
| `gmail` | `imap.gmail.com` | 993 | implicit | 2-Step Verification + app password, or OAuth (P12a) |
| `custom` | caller | caller | caller | host, port and TLS are required; there is no plaintext mode |

Known providers refuse a `host`/`port`/`tls` override instead of silently ignoring it. The operation
validates a bare address (one `@`, no display name, no controls, 320 bytes), a non-empty password with
no control characters (1024 bytes), a display name (256 characters) and the custom endpoint before any
network call. A rejected login names the provider and the likely cause and never echoes the password.

**`account_add` flow.** Validate → reject a duplicate address (case-insensitive) → `probe`
(TCP + TLS + login + `LIST` + `LOGOUT`, no store write) → plan the folder rows with the same
special-use mapping sync uses → **store the credential** → `Store::save_account` (account + server +
folders, one transaction) → memory. The credential goes first on purpose: a crash between the two
durable writes leaves an orphan keyring entry, which a retry with the same address (and therefore the
same deterministic `acc_<16 hex>` id and scope) reuses, instead of an account that can never sync. If
the store transaction fails, the credential is deleted again before the error returns; a cleanup that
also fails is reported explicitly. A failed login or an unreachable, wrongly-configured server writes
nothing at all: no account, no server row, no folder, no credential.

**`account_remove` order.** The store cache is deleted first; the credential second. The only failure
window is therefore an orphan credential (reported; re-adding the same address reuses the scope), never
cached mail whose password is already gone. `confirm: true` is mandatory; labels stay; a failure of the
cache delete aborts before the keyring so the account remains fully usable. Attachment bytes are
content-addressed and may be shared with a message of another account, so removing the account deletes
their rows but not the blob files themselves; the unreferenced-file sweep belongs to the daemon's GC
batches (P10a1).

**`sync_now`.** Load the server row and the credential, build an `ImapConfig`, call
`postino_sync::sync_account` with the engine's store connection and the default limits, then reload
folders, messages, attachments and blob hashes from the store into memory. Labels, the label sequence
and the account list are not reloaded: sync never changes them and assignments live in the store, so a
local label survives. A failed run is reported after the same reload, so memory shows every message
committed before the stop and never loses a cached one. `account_test` is the same probe without the
sync and without any write.

**Operations and annotations.** `accounts_list` returns `AccountView { account, server, sync }`:
provider, address, host/port/TLS/username and the persisted status, never a secret. The status is a
cache: a malformed `sync_status` value is returned as `sync: null` with a warning (the operation's
`warnings` array; the CLI prints it), and it never blocks listing or `account_remove`. `account_add`
and `account_remove` are **sensitive** (`OpDef.sensitive`); the MCP server filters them out of
`tools/list` and refuses `tools/call` for them before parsing the arguments, so a password cannot
appear in a refusal. The CLI validates the account request and the engine before it reads a secret;
`--password-stdin` is refused when stdin is a terminal (the no-echo prompt is the interactive path),
there is no `--password` argv flag, and `postino run` refuses sensitive operations. `read_only` is set
on `accounts_list`, `account_test` and `sync_status`; `destructive` on `account_remove`. `sync_now` and
`account_add` are writes. `sync_status` is offline. MCP marks `account_test` and `sync_now` as
`openWorldHint: true`; everything else is local. Read-only CLI commands with no default store report
no account without creating the file or touching the keyring; `Store::open_read_only` is the
deterministic read seam (an old schema must be opened read-write once), and an explicit `--store`
that does not exist is a usage error for them.

**Tests and limits.** The fake server gained `FakeServer::start_tls_many` so one stored port can serve
add, test and several sync runs; `postino-sync` exposes `probe`, `plan_folders` and
`read_status_for` without making the store or net APIs public. The engine test
suite covers add success, wrong password, unreachable host, duplicate address, a hostile server
echoing the password in `LIST`, broken credential on test, a damaged status that still lists and
removes, a 3-folder initial sync with an attachment, incremental sync with a surviving label, a
failed sync that keeps the cache, remove isolation, the remove failure window, a failed account
transaction that rolls the credential back, the read-only store seam and the demo errors. No test
uses a real vault, a real account or `%LOCALAPPDATA%\Postino`.

### 4.8 Implemented in P6b: onboarding, boot and the sync worker

The desktop app is a client of the same store as the CLI.

**Boot.** Without `--demo`, the app opens `Engine::data_dir()/mail.db` (or `--store`), reads the
store key from the OS keyring and loads the mailbox once. A failure at any stage (data directory,
keyring, key, schema) shows a recovery card with the path, the failing stage, a Retry button and an
explicit “Apri la modalità demo”; it never panics and never silently starts on an empty mailbox. A
store with no account opens onboarding; a store with an account opens the mailbox, shows the cache
immediately and starts one background refresh. `--demo` is the synthetic fallback and is labelled as
such, so a screenshot can never be mistaken for real mail.

**Onboarding.** Welcome → provider (Aruba, Gmail, custom host/port/TLS) → credentials (masked
password in a zeroising buffer, Caps Lock hint, no password in labels) → folders with the server's
roles and Italian warnings → first sync with determinate progress, Cancel and “Continua in
background” → mailbox. Back exists at every stage, Esc uses the unsaved guard, Enter continues, focus
moves to the field that failed. The account probe runs on a worker thread so the UI never blocks; a
failed probe writes nothing (P6a). The credential is zeroised when the flow succeeds, is cancelled or
is dropped.

**Worker.** One thread owns its own SQLCipher connection and `Engine::open_with_vault`, with command
(`SyncNow`, `Cancel`, `Stop`) and event (`Started`, `Progress`, `Finished`, `Failed`) channels. It
drives `postino_sync::sync_account` directly, polls the persisted status for progress and reloads
its engine from disk before reporting. Duplicate requests coalesce and a second UI request says
“Sincronizzazione già in corso”. The UI thread polls events, never calls egui from the worker and
reloads the account without losing selection, filters or the open message. Events from an account the
user switched away from are ignored for the UI while the store is still refreshed. With no daemon
the app runs two connections in one process, SQLite WAL and the busy timeout serialise them, and the
app holds the store's writer lock so a daemon cannot start beside it; with a daemon running the app is
a client (P10a2) and opens no writer.

**Mailbox.** Folders, messages and counts come from the store; the account switcher carries the
active identity and offers add/manage; Refresh runs the worker; the status bar shows syncing with
progress, “Aggiornato alle HH:MM”, offline with the last sync and an error with Retry. An offline
banner never empties the list. Account removal is destructive, explains the local-cache and keyring
consequences, and returns to onboarding when the last account goes.

**Deferred to P6c.** Link opening with a scheme allowlist, attachment Save through the blob seam,
the full settings/licences surface and the remaining reading-pane affordances. SMTP, HTML rendering
and OAuth landed later in P7, P11a and P12a. The daemon, tray and notifications arrived in M4.

### 4.9 Implemented in P7: SMTP submission and the outbox

Postino now sends real mail. The desktop composer, the CLI and MCP all reach the same six-ish
operations; the submission is a journaled transaction, not a call buried in the UI.

**SMTP client (`postino-net::smtp`).** One TCP connection, implicit TLS or STARTTLS with the same OS
verifier and extra-roots seam as IMAP, then `EHLO`, `AUTH PLAIN`/`LOGIN`, `MAIL FROM`, one `RCPT TO`
per recipient, `DATA`, the dot-stuffed body and the final `.`, `RSET`, `NOOP`, `QUIT`. Capabilities
parsed from `EHLO`: `SIZE` (refused locally before `MAIL`), `8BITMIME` (a body with 8-bit bytes is
refused when it is absent), `SMTPUTF8` (only when the headers or envelope need it and the server
advertises it), `DSN` (optional `RET`/`ENVID`/`NOTIFY`, used only when advertised) and `PIPELINING`
(parsed, never used: every command waits for its reply). A response line is at most 8 KiB, a whole
reply 64 KiB, a message 40 MiB, a transaction 100 recipients; every phase has a timeout. Data is
CRLF-only, NUL is refused, and dot-stuffing is applied to the exact persisted bytes.

**Typed failure and the one dangerous window.** `SmtpError` distinguishes `Connect`, `Tls`, `Auth`,
`Rejected { code, stage, message }`, local `Transient`/`Permanent`, `Timeout`, `Closed`, `TooLarge`,
`Unsupported`, `Protocol` and `Uncertain`. Before the final `.`, 4xx is transient and 5xx permanent;
`Auth` is a credential stop whose reason is built locally (the server has just received the password
and may echo it). Once the final `.` is written, every failure becomes `Uncertain`, including a 4xx/5xx
reply: the server may have accepted the message even if its `250` never arrived, so the outbox never
retries it automatically and a person resolves it.

**No server text in the journal.** SMTP replies and IMAP `NO`/`BAD` text are never stored, printed or
returned over MCP. `outbox.last_error` is a fixed typed sentence (stage + code only) and the APPEND
failure stores a fixed sentence too; the CLI and the MCP `outbox_list` therefore carry exactly what the
database holds. A hostile server that echoes the password, the body or the recipient list in a
refusal cannot turn the outbox into a prompt-injection channel or a secret store; tests at RCPT, at
DATA-end and at APPEND assert the absence of each.

**Schema v5.** Additive from v4: nullable `smtp_host`, `smtp_port`, `smtp_tls`, `smtp_username` on
`account_servers`, and

```sql
CREATE TABLE outbox(
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT,                    -- the local draft this submission came from
    idempotency_key TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    send_at INTEGER NOT NULL,         -- user-visible due time
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    append_attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    raw_blob_hash TEXT NOT NULL,
    envelope_json TEXT NOT NULL,
    lease_at INTEGER                  -- written when a worker claims the row
);
CREATE INDEX outbox_due ON outbox(state, send_at);
CREATE INDEX outbox_draft ON outbox(draft_id, created_at DESC);
CREATE UNIQUE INDEX outbox_active_draft ON outbox(draft_id)
    WHERE draft_id IS NOT NULL
      AND state IN ('queued', 'sending', 'append_pending', 'uncertain', 'failed');
```

`draft_id` is a column, not only an envelope field: the newest row by `created_at` (a revival updates it)
decides whether a queue is idempotent, and the partial unique index makes a second **active** row for
one draft impossible — a failed row must be discarded before a new queue, an uncertain row is a possible
delivery, and only `cancelled`/`sent` free the slot. `idempotency_key` is stable per draft revision and
content (`draft:<draft_id>:<revision>:<raw_blob_hash[..16]>`), so two processes that queue the same
unchanged draft are one row; a cancelled row with the same key is revived in place, which is what makes
cancel → send again work without a duplicate, and an edited draft is a new key. `envelope_json` carries
the sender, the deduplicated `To`+`Cc`+`Bcc` envelope, the subject, the generated `Message-ID`, the local
draft id and the draft revision; it is encrypted with the database and never logged. `raw_blob_hash`
names the exact RFC 5322 bytes in the encrypted blob store. The queue path writes the blob first and
then inserts the row in one transaction: a crash between them leaves an unreferenced, deduplicated
blob, never a row whose bytes are missing.

**Deterministic bytes.** `send_dry_run` and `message_send` run the same serializer on the same draft
snapshot, and the `Date` header is the draft's own last-saved instant, not the wall clock. The
`Message-ID` is derived from a digest that includes that date, so the dry run, the queued blob and a
resend after a cancel are byte-identical; a scheduled message carries the instant it was last edited.

**State machine.** `queued -> sending -> append_pending -> sent`; `failed`, `cancelled` and
`uncertain` are the other terminal or human-resolved states. A lease changes one due `queued` row to
`sending`, writes `lease_at` and increments `attempts` in one immediate transaction. Each lease is
stamped with the instant that row is actually claimed, never with the pass-start value, so a row
leased after an earlier row spent half an hour in SMTP still gets a full window. `lease_at` — not the
user-visible `send_at` — is the lease epoch: a row scheduled for 09:00 and leased at 10:00 is fresh
for the whole window. The lease is **45 minutes**, strictly longer than the 30-minute body-transfer
budget the network client enforces (`postino_net::MAX_BODY_TRANSFER`); a transfer slower than that is
aborted by the client, so a *live* submission can never outlive its own lease and be expired as a
crash. A `sending` row whose lease is older than `LEASE_MS` becomes `uncertain` on the next pass, with
“Postino si è fermato durante l’invio; il messaggio potrebbe essere stato accettato. Controlla Inviati
prima di riprovare.” Every post-lease transition (append_pending, sent, failed, uncertain, requeue,
release) is conditional on `state = 'sending' AND lease_at = <the lease this worker holds>`: a worker
whose lease expired records the outcome and never overwrites the next owner. Queueing also refuses a
draft whose saved content no longer matches this engine's snapshot — body, headers, last-saved
instant and the attachment set (ids, names, MIME types, sizes, blob hashes; a stored reply linkage
this build cannot serialize is refused too) — and a submission is only sent while the draft still has
the **revision stored in its envelope**: a stale connection (or a lock bypassed by an edit) fails the
row with a fixed sentence instead of delivering old bytes and labelling the newer draft as delivered.
The finalize repeats the same content check. Transient failures go back to `queued` with exponential
backoff 30 s, 60 s, 2 min … capped at 1 h for at most eight attempts; permanent failures become
`failed` with a fixed sentence; an authentication failure fails the current row and writes a
per-account halt, so the rest of that account’s queue is not attempted. A manual retry requires
`confirm: true`; an uncertain row can also be marked sent, but the conditional write is checked: if a
concurrent retry changed the row first, the operation fails with “the outbox row changed state” and
does **not** move the draft. A locally unreadable blob or missing credential fails that row and the
pass continues.

**Draft lock and the sent-row policy.** A journal row holds its message only while the message is still
in Drafts and the row can still deliver it (`queued`, `sending`, `append_pending`, `uncertain`,
`failed`). A Sent copy is ordinary mail: it can be moved, archived or deleted, and a batch over sent
mail does not abort. A `failed` row (a permanent refusal, so no delivery happened) can be discarded with
`outbox_cancel` — the CLI and the UI both offer it — which makes the draft editable again; `uncertain`
keeps the strict policy and can only be retried or marked sent with explicit confirmation. A `sent` row
never locks, so a Sent copy can be moved back to Drafts — but sending it again needs an **edit**: the
engine compares the draft revision with the sent envelope and refuses an unedited copy (“already sent”),
because that is exactly the state left by a crash between the server’s `250` and the local move. An
edited draft is a new revision, gets a new idempotency key and is a deliberate resend. Any edit or move
also verifies the store’s current folder for the id first: a stale engine whose row another process
already sent is refused with “changed in another session” instead of overwriting the Sent copy.

**Sent APPEND is independent and owned.** After the `250` the row becomes `append_pending` durably
with a fresh append claim (`lease_at`) stamped at that instant, not at the pass start; the worker that
submitted the message also appends it, so a long submission cannot leave the append claim already
expired and two passes cannot APPEND the same row concurrently. A crashed append claim is claimable
again after the same 45-minute window, and each claim increments a separate `append_attempts` counter.
Then the draft moves to the local Sent folder, and the worker discovers the server’s `\Sent` folder
from `LIST` special-use and APPENDs the same bytes with `\Seen`. A failed APPEND reschedules
`append_pending` with its own exponential backoff (30 s, 60 s, 2 min … capped at 1 h) and never touches
SMTP; a **damaged envelope** is a fixed failure with backoff, never a false `sent`. There is no
terminal give-up state for an APPEND that keeps failing (an IMAP server without `\Sent`, or a
credential that stays wrong): the row retries at the one-hour cap and the journal keeps saying the
message was submitted and its server copy is missing. A manual “stop retrying” action is P8/P9 work;
the row is safe either way because it can never resubmit over SMTP. A crash between `250` and the
transition is `sending` and becomes `uncertain`, never a resend. A crash after `append_pending` retries
only the APPEND. The IMAP APPEND has no client idempotency token, so a crash after the server accepted
the literal but before its tagged `OK` can leave a duplicate Sent copy on the server (P8/P9 may remove
it). Locally, the Sent copy carries the generated `Message-ID`; a sync adopts a local row with that
Message-ID and no UID from **Sent or Archive** (never Drafts or Trash: a Sent copy moved back to Drafts
is being edited), so a server copy that moved to Archive still reconciles instead of inserting a second
one. In the other order, a provider auto-save (or a sync between the `250` and the local move) that
already cached the server copy is removed in the same transaction as the local move; the surviving
local row **adopts that copy’s UID** and the worker skips the APPEND, because the server already has
the message. The folder therefore still holds one row, identified by the server's own UID even if the
Sent cursor is already past it.

**Credentials.** IMAP stays under `account:<id>:imap`. A separate SMTP password is stored under
`account:<id>:smtp`; when that scope is absent the IMAP credential is used, which is the normal Aruba
and Gmail app-password setup. `account_add` probes both servers before writing, stores the credentials
only after both accept, deletes them again if the store transaction fails, and `account_remove`
deletes both scopes after the cache. No password enters the database, the outbox, a log, an MCP
result or argv. Aruba is `smtps.aruba.it:465 implicit`, Gmail `smtp.gmail.com:465 implicit`; a custom
account may omit SMTP and then reads but does not send.

**Operations, CLI, MCP, UI.** `message_send` queues with a 5 s undo window; `send_schedule` queues at
an RFC 3339 instant; `outbox_list` is offline, read-only, newest-first and pageable (`limit` 1-500,
`offset`); terminal rows older than **30 days** are pruned in bounded batches, so the history is bounded
in time as well as by the page. `outbox_cancel` accepts `queued` and `failed` (a discard: no delivery
happened); `outbox_retry` requires `confirm: true` for `failed`/`uncertain`; `outbox_mark_sent` resolves
an uncertain row locally and refuses if the row changed first; `send_dry_run` returns base64 bytes and
envelope without a row. `message_send` is not marked destructive: sending is the product’s purpose. MCP
gates `message_send`, `send_schedule`, `outbox_retry` and `outbox_mark_sent` behind
`postino mcp --allow-send`, lists them disabled with the reason otherwise, never exposes
`account_add`/`account_remove`, and only queues: the MCP server does not drain the outbox. The CLI adds
`postino send --draft <id> [--at …] [--dry-run]` and `postino outbox list [--limit N] [--offset N] |
cancel|retry|mark-sent`; a plain `postino send` (no `--at`) waits out the 5-second undo window and runs
one outbox pass, while a future `--at` leaves the row for the daemon scheduler (or the next app/CLI
pass when no daemon is running). The
desktop app has a send worker on its own thread, a 5-second undo toast, and an outbox dialog
(`Ctrl+Shift+O`, status-bar `Coda · N`) that shows every state, the fixed error sentence, the warnings,
a “Scarta” action for a failed row and the two uncertain resolutions. The app polls the store every two
seconds and caches the next wake-up time from one indexed query, so a row queued by the CLI or MCP after
boot is sent when due; when the poll sees a row reach `sent`/`append_pending` (or a row disappear) it
reloads the mailbox from the store (keeping every view state) instead of leaving the draft in Drafts and
Sent stale. A “load older” control for the journal is P8/P9 work; the operation and the CLI already page.
The demo mailbox has no SMTP endpoint, so Send is disabled with a reason and neither the button nor
`Ctrl+Enter` attempts a submission; a real account without an SMTP endpoint gets its own sentence, not
the demo copy.

**Evidence.** The fake SMTP server covers greeting, `EHLO`, AUTH PLAIN/LOGIN success and failure,
4xx/5xx per stage, `SIZE 0`, SIZE refusal, SMTPUTF8/8BITMIME, DSN, dot-stuffing, a bare CR refused,
slow replies, a timeout and a drop after the final dot, STARTTLS and implicit TLS with the test CA, a
wrong hostname refused, and a transcript with no plaintext password or body. The pipeline suite asserts
the exact bytes and envelope (a dry run separated from the queue by more than a second is identical),
Bcc only in RCPT, the `\Seen` Sent APPEND, a 25 MiB attachment with ten recipients, the recipient/SIZE
caps, scheduled/cancel, dry run without a row, backoff, permanent/auth halts, cancel → queue → queue
producing **one** row and one DATA, the R1/R2/revert revival where the newest row is the revived one and
a stale revision is never submitted, the draft lock across a cancelled history, a failed row discarded,
edited and resent, a sent row moved back to Drafts that needs an edit before a resend, a Sent copy moved
and archived, 500 history rows not hiding a new one (plus `limit`/`offset` paging), the local Sent row
carrying the Message-ID, a sync-before-finalize cache merged into one local row, a hostile
RCPT/DATA/APPEND echo absent from the journal, CLI output and MCP output, three passes over a damaged
append envelope that never report `sent`, and the crash windows: after queue, after `sending`, after the
server's `250` and before the durable transition (a test seam fails there; reopening turns `sending`
into `uncertain`, opens no connection and leaves one DATA total), and after APPEND. Two-process tests
cover a live lease with an old `send_at` (the second process neither expires nor leases it), a claimed
append that a second pass does not touch, a `mark_sent` that loses the race, a stale engine that cannot
overwrite another process's move, a second process that reopens a crashed `sending` row, and two engines
allocating distinct draft ids. The CLI suite proves that a plain `postino send` (no `--at`) submits and
that a future `--at` only queues. The tests prove these windows; they do not prove atomicity IMAP and
SMTP do not offer.

### 4.10 Implemented in P8: compose depth, attachment bytes, signatures and reply headers

**Attachment intake.** `draft_attachment_add { draft, filename, mime?, data_base64 }` is the one
byte-accepting operation. Before the decode (the JSON string itself was already allocated by the
transport) it checks the base64 string against
`ceil(25 MiB / 3) × 4` characters, decodes with the standard alphabet, then applies the composer
caps: 20 attachments, 25 MiB per file and 25 MiB total. The filename passes
`postino_mime::sanitize_filename` (the same rule the serializer applies): CR/LF/NUL are refused,
traversal separators, controls, bidi/format controls and trailing dots/spaces are removed, a Windows
device stem is renamed with `_`, and the result is bounded to 255 bytes. A duplicate is renamed
`name (2).ext`; the extension table decides the MIME type, an unknown extension is
`application/octet-stream`, and a caller's bare `type/subtype` is accepted only when it agrees with
that table (so a valid lie such as `evil.exe` + `image/png` cannot change the emitted type; the bytes
are never inspected). The engine writes the blob first, then one `save_draft_message_linked`
transaction writes the message row, the attachment rows (with the new hash), the draft revision and
the folder counts. A failed row write leaves an unreferenced deduplicated blob, never a row that
claims bytes it does not have. Before either attachment write the engine compares the draft's stored
revision with its metadata-only snapshot map (loaded at open and kept through cache eviction); a draft
saved, sent or moved by another process is refused with “changed in another session; refresh the
mailbox” instead of overwriting the newer body or resurrecting a Sent row. `draft_attachment_remove` deletes the row and
leaves the blob for the daemon's GC. The desktop picker/drop reads a path locally with the same 25 MiB bound and calls the
engine method; CLI `draft attach` reads the path and calls the operation with base64; MCP receives
base64 and never a path. The metadata-only `draft_save` path runs the same sanitizer and duplicate
renaming on its attachment names, so a caller cannot create reserved, tabbed or duplicate chips
before the bytes arrive.

**Blob lifecycle.** The blob store is content-addressed and encrypted; two drafts that forward or
attach the same content share one file. Removing an attachment or deleting a draft frees the row, not
necessarily the disk file; the daemon's GC sweeps unreferenced blobs. Send reads every
attachment through `attachment_bytes`, capped at the recorded size plus 1 MiB, and refuses a missing
or corrupt blob with the filename before writing an outbox row. A 25 MiB attachment plus ten
recipients is covered by the P7 submission test; the P8 add path accepts exactly 25 MiB and refuses
one byte more.

**Signature storage.** `accounts.signature` is the plain-text signature, capped at 4 KiB with only
newline and tab allowed as controls. `account_signature_set` updates that column in one statement and
the in-memory account; it does not touch drafts, messages or the keyring. The composer reads the
active account's signature when it creates a new message, and `compose_reply` reads it when it
prefills a reply or forward; an existing draft keeps the text it saved. The signature is not a MIME
part, an attachment or HTML.

**Reply headers and Reply-To.** A reply stores the parent `Message-ID` in the existing
`drafts.in_reply_to` column (normalised without brackets, ASCII, ≤ 900 bytes, no controls or
whitespace); a save that omits the field keeps the stored link, exactly like the column's `COALESCE`.
`submission_for_draft` emits `In-Reply-To: <parent>` and `References: <parent>` — a draft submission
still emits the parent id alone; v7 stores the full incoming chain for P9b — and the P7 snapshot check
now compares the hydrated link
instead of refusing every linked draft. A different process that writes a link this engine never
hydrated still makes the snapshot stale: refusing is the honest result, because those bytes would
silently drop a header. Received `Reply-To` lists are cached in the existing `meta` table under
`reply_to:<message_id>` because P8 may not migrate; sync writes it, account/message deletion removes
it, and `Engine::reply_target` returns it with the parent id. The reply prefill uses `Reply-To` for
`To`, removes the account's own address case-insensitively, adds the original To/Cc minus the account
and minus To for reply-all, never reads Bcc, prefixes the subject once, quotes in UTC and drops a
second copy of the signature when the parent already carried it. Forward copies the parent attachment
metadata and the engine copies each blob to a new row; a missing blob names the file and blocks Send.

### 4.11 Implemented in P9a: the body-free read path and encrypted search

Search is SQL on the encrypted database, and opening a mailbox no longer means loading every body.
The two changes ship together because the in-memory search that FTS replaces was the reason
`Engine::open` held bodies in the first place.

**Read path.** A store-backed `Engine` holds `accounts`, `folders`, `labels` and a
`Vec<MessageSummary>`; `Engine::open` issues one covering-index list query, one label query, one
attachment id/hash query, one draft-metadata query (id, revision, updated_at, in_reply_to) and the
orphan-draft repair query. It never selects `body_text`, `body_html` or full attachment rows.
`messages()` returns the summaries. `Engine::message(id)` is an owned read: it looks in an eight-entry
cache keyed by the engine revision and otherwise calls `Store::get_message_with_attachments`. Every
write and reload bumps the revision, so a stale body cannot survive a flag, label, draft or sync
change. Drafts are **not** preloaded: a dedicated `draft_snapshots` map (id → store revision) loaded
from metadata is the stale-draft guard and is independent of body-cache eviction. Draft bodies are read
only when a draft is opened or serialized. `messages_list` uses `Store::list_messages` with folder,
label, account, unread, starred and attachment filters, so the list is a bounded SQL page.
`label_summaries` reads live counts from the store. ADR-0004 records the split. In memory, the demo
engine keeps its full synthetic messages and answers through the same API.

**Schema v7** is additive after v6, in one transaction:

```sql
ALTER TABLE messages ADD COLUMN search_sender TEXT NOT NULL DEFAULT '';
-- …search_to, search_cc, search_bcc, attachment_names, subject_norm,
--   in_reply_to, references_json, ingested_at, rules_applied_at, origin
-- P9c extended the unshipped v7 in place: `messages.origin` (received/sent/draft/local)
-- and `rules.name_key` (Unicode lowercase) are added idempotently by `Store::open`.
CREATE INDEX messages_account_thread_date ON messages(account_id, thread_id, date DESC);
CREATE INDEX messages_list_cover ON messages(date DESC, id, folder_id, thread_id, account_id,
    from_name, from_email, to_json, subject, snippet, unread, starred, has_attachments);
CREATE VIRTUAL TABLE message_fts USING fts5(
    subject, search_sender, search_to, search_cc, search_bcc, body_text, attachment_names,
    content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 2', prefix='2 3');
CREATE TABLE message_fts_rank(rowid INTEGER PRIMARY KEY, date_key INTEGER NOT NULL, id_key TEXT NOT NULL);
```

The repository derives sender/recipient text, the attachment-name list and `subject_norm`; the
triggers copy those values and at most 262 144 characters of `body_text` (≤ 1 MiB of UTF-8) into the
index. The same insert/delete/update triggers maintain `message_fts_rank`, so `bm25, date DESC,
id ASC` is a global order inside SQLite before `LIMIT/OFFSET`; the page then joins only the selected
rows to `messages`. An `AFTER UPDATE OF` includes the indexed columns, `date` and `id`, so a date
change reorders the rank row; flag and folder updates do not reindex. The pre-flight probe
(`crates/store/tests/fts_probe.rs`) proves the bundled SQLCipher 4.19 / SQLite 3.53.4 accepts
`contentless_delete=1` and that delete+insert is the supported update form. FTS5 shadow tables live
in the SQLCipher file; there is no sidecar and no content in the index, so snippets cannot leak HTML.
`meta.search_index` holds `{state,last_rowid,indexed,total,updated_at,last_error}`; every FTS mutation
also stamps `meta.search_updated_at`. `Store::open` **does not** force a build. `ensure_search_index`
finishes a pending build in 500-row transactions, `ensure_search_index_batch` advances one batch for
the shell/search path, and `search_reindex` deletes all and rebuilds. `search_status` reports state,
counts, row-id drift (missing and extra rows, FTS and rank), `last_error`, the database+WAL size and the
last index update. A failed batch records `state:error` and a readable `last_error`; the mailbox still
opens. `search_reindex` is a single-process maintenance action: SQLite serialises the writes, but a
second builder can see a half-built index, so do not run it from the desktop and MCP at once. Nothing
runs `VACUUM`, because FTS keys on `messages.rowid`.

`threads`, `thread_aliases`, `saved_searches`, `rules`, `rule_runs` and `rule_actions` are created empty
in v7 so P9b/P9c would need no second migration. P9a did not read or write them; P9b owns `threads`,
`thread_aliases` and `saved_searches`; P9c owns `rules`, `rule_runs` and `rule_actions`. P9c also
creates `rules(account_id,name COLLATE NOCASE)` as a `UNIQUE INDEX IF NOT EXISTS` on normal open, the
same derived/index-maintenance seam P9b used: `user_version` stays 7 and a read-only open creates
nothing.

**Query language.** `postino-core::search` is a hand-written recursive-descent parser with one AST and
one canonical printer:

```
query := or; or := and (OR and)*; and := unary (AND? unary)*; unary := NOT unary | primary
primary := '(' query ')' | field ':' value | phrase | term
field := from|to|cc|bcc|subject|label|folder|is|has|before|after|larger|smaller
```

Bare terms search every text column; `*` is a prefix; quoted values support `\"`; `AND`/`OR`/`NOT`
are uppercase and juxtaposition is AND; `word:` is a field only for the documented names. Dates are
`YYYY-MM-DD` UTC (`after` inclusive, `before` exclusive), sizes accept `k`/`m`/`g`. The caps are 1024
bytes, 64 clauses, depth 8 and 16 terms. Every failure is `{code, position}`; the CLI and MCP render
an English sentence and the UI an Italian one, without echoing the query. The printer fully quotes
values and parenthesises composite nodes, so `parse(print(ast)) == ast`; a property matrix covers
fields, phrases, prefixes, Unicode, escapes, lowercase `or`, `Re:` and `http://`.

**Execution.** Core resolves `label:`/`folder:` names to ids and escapes every value as an FTS5
phrase. The store receives a typed `SearchExpr`; SQL fragments are fixed and every value is bound, so
no caller text reaches `MATCH` or a statement by interpolation. For a text query on one account, the
store orders by `bm25, date_key DESC, id_key ASC` inside FTS5 (the rank table), then takes the page and
joins only those rows to `messages`; total is a separate FTS+rank count, exact up to 10 000 and flagged
with `total_capped`. A mixed query (text plus folder/label/flag/date/size) compiles to typed SQL
predicates and per-leaf `MATCH` subqueries; a filter-only query never runs `MATCH`. The unread and
attachment chips are filters in that same scope, not a post-filter of the page; a plain folder view
passes `folder`, a label view passes the optional label folder. Hits are capped at 200 and offset at
5 000. Snippets are built in Rust from the subject or the first 16 384 characters of `body_text`
(at most 64 KiB of UTF-8), normalised to one line, stripped of bidi/format controls and at most 240
characters; the store never reads `body_html` for search. The desktop app debounces 250 ms, commits on
Enter, clears on Esc and caches results by `(signature, revision)`; it shows the parsed clauses as
chips (at most four, two rows), the Italian parse position, plain snippets, `primi 10.000 risultati`
when capped, and a scope-aware empty state. A pending index shows `Indice di ricerca in preparazione
(x/y)` and the shell advances one 500-row batch per tick; an error shows the stored sentence.

**Demo semantics.** The demo engine answers through the same operation shape but evaluates the AST in
Rust over its synthetic messages: matching is case-insensitive substring rather than FTS tokens,
`larger:`/`smaller:` match nothing because demo rows carry no RFC 822 size, and results are date-sorted
regardless of `sort`. This is a demo-only divergence, stated here rather than hidden.

**Performance.** Fixture: 100 000 messages, 2 KiB plain bodies, 20% attachment names, one account;
20 000 in CI and 100 000 behind `POSTINO_SLOW_TESTS=1`. On the Windows GNU host (debug, opt-level 1,
SQLCipher 4.19 / SQLite 3.53.4), five runs after the index was built:

| Measure | 20k | 100k | P9a target |
|---|---|---|---|
| First relevance page, store P50 | 1.7–2.3 ms | 34.3–39.5 ms | < 100 ms |
| First relevance page, store P95 | 6.0–7.7 ms | 35.8–54.9 ms | < 100 ms |
| First relevance page, `search` op P50 | 3.1–3.3 ms | 51.0 ms | < 100 ms |
| First relevance page, `search` op P95 | 10.2–11.3 ms | 53.3 ms | < 100 ms |
| `open` including summaries | 59–77 ms | 268–357 ms | < 150 ms |
| `sort:"date"` first page | 49–59 ms | 251–316 ms | not gated, reported |
| Seed + FTS build | 3.1–3.3 s | 16.5–18.0 s | backfill reported |
| Full `search_reindex` | 2.6 s | 17.6–20.7 s | backfill reported |
| FTS+rank index bytes (`dbstat`) | 23.0 MB | 115.2 MB | reported |
| 1 000 drafts, 4 KiB bodies, open | 18.6 ms | — | no body read |
| Peak working set in the seeding test process | — | 154.7–156.5 MiB | < 120 MiB |

The range is several debug runs on the same Windows GNU host; the CI profile is opt-level 1 with
dependencies at 2. The relevance target is met, including the 2 000-identical-body oldest-first
regression whose offsets 0/200/400 return disjoint newest-first pages. **The open target is missed**
(268–357 ms vs 150): the covering index keeps body pages out of the scan, but decrypting and decoding
100k summary rows and building the engine's Vec/index/counts remains ~200 ms plus engine work. A
compact summary/body table is the next step; P9a reports the miss rather than claiming it. The
`sort:"date"` path is slower because it joins every match to read the date. **RSS is unverified, not
established as missed:** 154.7–156.5 MiB is the seeding test process's high-water mark, not an opened
mailbox's working set. Index bytes are measured with `dbstat`; new-message latency and a 100-message
incremental sync were not measured in P9a and are not claimed.



### 4.12 Implemented in P9b: conversations and saved searches

A conversation is identified by an account-scoped **root reference**, not by a local message id.
`postino-store::threads` derives it when a message is written, in the same transaction as the row:
the first `References` entry that resolves to a cached thread wins; otherwise the first usable
reference is the phantom root; otherwise the `In-Reply-To` parent thread, then the parent itself,
then the message's own `Message-ID`, then its local id. A reference is trimmed of one angle-bracket
pair, refused when it contains a control or exceeds 512 bytes, the chain is capped at 32 entries and
deduplicated, and a self-reference is ignored. A reference resolves through the smallest local
`Message-ID` copy, an existing thread row or the alias table; alias chains are bounded at 16 so a
damaged table cannot loop. Two resolved threads merge into the lexicographically smallest id, every
message moves, the old row is deleted and an `thread_aliases(old_id,new_id)` entry keeps old links
working; `thread_get` follows it and returns `resolved_id`. A message whose own `Message-ID` names a
phantom thread adopts it, which is how a late parent joins the replies that were waiting for it.
Unresolved sibling references are aliased to the chosen root, so a root that arrives later joins the
same conversation. Ids change only on a merge; a locally sent draft keeps the id its final row will
have, so the send does not create a second conversation.

The subject fallback is deliberately narrow: no `References` and no `In-Reply-To`, a `Re:`/`Fwd:`/`Fw:`
prefix, a normalised subject of at least 8 bytes, and exactly one distinct thread with that subject
inside a ±14-day window. It attaches this message to that thread; it never merges two threads, and two
matching threads drop the fallback. A no-reference message that already has a thread keeps it on a
re-fetch, so a second same-subject thread appearing later cannot detach the first reply. A reopen reads
the stored ids, and a `UIDVALIDITY` reset re-derives the same ids from the same headers. A locally sent
message takes its own `Message-ID` (or the parent thread, or the phantom parent) as the thread before
its row is written, and `send_draft_message` refreshes the old draft thread as well as the new one, so a
later Sent re-fetch derives the same id and no ghost row survives. Mail cached by P9a keeps its P9a
placeholder ids; P9b does not re-read every pre-P9 message at open (that would add a full mailbox
derivation to the startup path). `search_reindex` rebuilds the cache rows, not the ids; **the pre-P9
placeholder ids are permanent until a future full re-derivation operation**, which this package does not
ship.

`threads` is a cache. Counts are mailbox-wide, the subject is the earliest message's subject, and
`latest_message_id` is the row a click opens. Ingest, flag changes, deletes, merges and draft sends
refresh only the affected rows (a send refreshes both the old draft thread and the new thread);
`Store::refresh_all_threads` rebuilds the table from `messages` in one transaction, and `Engine::open`
runs it once when a P9a store has messages but an empty cache. A read-only open skips that write and a
thread read answers with an actionable "open read-write once" error if the cache is empty. A folder or
label view lists threads with at least one message in scope without narrowing the counts on the row.
`Store::list_threads` reads a page from the cache and the latest snippet per row **after** the `LIMIT`,
so a 100k cache never joins every message before the page. Normal open creates the derived indexes
idempotently: `threads(account_id,last_date DESC,id)`, `messages(account_id,message_id)` for the
reference lookup, `messages(account_id,subject_norm,date)` for the subject fallback and
`thread_aliases(account_id,new_id)` for merge redirects. This is derived-cache maintenance, not a
schema version (`user_version` stays 7); a read-only open never creates them. The reference lookup is
the difference between a mailbox scan per reference and a point lookup on every synced message.

**Operations.** `threads_list {account?,folder?,label?,unread_only?,starred_only?,with_attachments?,
limit?,offset?}` returns `{threads,total,unread_threads,unread_messages}` with participants, subject,
snippet, date, state, counts and labels. `thread_get {id,account?,limit?,offset?}` returns ascending
message summaries (≤200), `total`, `truncated` and `resolved_id`; the desktop loads the newest page
first and prepends earlier pages on request, and an account-wide list includes local drafts (folder
views do not). `thread_flag {id,account?,unread?,starred?}` changes every message in the thread (≤500),
is idempotent and reports the refreshed row plus the real transition count; the engine updates its
summary vector and folder/label/star counts in the same call. Archive/delete stay `message_move` over
the thread's ids, so one undo covers the batch, including the messages that moved when a later id in a
partial batch failed. All three are local and offline; none is sensitive, and MCP marks `thread_flag`
idempotent.

**UI.** Folder views list one row per conversation by default: participants, subject, date, message
and unread counts, mailbox-wide labels. A search starts flat and the **Conversazioni o messaggi**
command groups its hits instead; the grouped header says the counts describe the results. Opening a row
shows the conversation banner at the top of the reading pane, one compact card per message and exactly
one expanded message: `J`/`K` move the open card (pulling the previous page at its top edge), `Enter`
opens the selected card, `R`/`A`/`F` reply/reply-all/forward it, `E`/`U`/`S` act on the whole thread,
and `Esc` goes back to the list. The thread list loads 200 conversations at a time with “Carica altre
conversazioni”; a thread over the page loads its newest 200 messages and prepends older pages. A thread
over 500 messages refuses a bulk move/flag with a sentence instead of acting on part of it. At 980×600
the reading pane is the full overlay the P2b layout already uses.

**Saved searches.** `saved_searches` holds at most 12 names per account (unique ignoring case), 1–48
characters, with the documented query language and `sort` `relevance`/`date`; the cap is checked in the
same immediate transaction as the insert, so a desktop and an MCP process cannot both create the 13th,
and ids come from a monotonic store counter (never reused). Create, update and delete are account-scoped
when the mailbox has several accounts. Create and update parse and resolve the query now, so an unclosed
quote, an unknown label or an unknown folder is refused instead of stored; a label or folder deleted
later leaves the row visible as `valid:false` with `error`, never a dead entry. Counts (`total`,
`unread`) are computed by the same search path as `search`, cached by engine revision, and a pending FTS
index is `valid:true,index_pending:true` with zero counts, not an invalid query; the desktop refreshes
the row when the index completes. Applying a saved search replaces the query, clears the label filter
and the unread/attachment chips, and the query owns its scope: a `folder:` clause filters it and a query
without one is account-wide, titled “Tutte le mail” and with no folder highlight. `Esc` restores the
query, folder, chips, filters and threaded mode. The last eight successful queries live in
`meta.recent_searches:<account>` inside SQLCipher; only Enter, a chip and an applied search record one,
so the 250 ms debounce cannot fill the history with prefixes. Deleting an account deletes that key and
its saved-search rows, and the desktop clears both lists. Rename, edit and delete are undoable; CRUD
leaves the packaged store in the encrypted database.

**Evidence.** The P9b fixture has 100 000 messages, 2 KiB bodies and 50 messages in one grouped
thread; 20 000 runs in CI and 100 000 behind `POSTINO_SLOW_TESTS=1`. The ingest measurement seeds the
mailbox with raw SQL and then writes 100 real 2 KiB messages through
`upsert_synced_message_with_refs`, each with three references that resolve to the grouped thread. On
the Windows GNU host (debug, SQLCipher 4.19 / SQLite 3.53.4), five runs after the cache was built:

| Measure | 20k | 100k | P9b target |
|---|---|---|---|
| `threads_list` account-wide P50 | 12.0 ms | 80.7 ms | < 100 ms |
| `threads_list` account-wide P95 | 14.2 ms | 82.1 ms | < 100 ms |
| `threads_list` folder view P50 | 69.1 ms | 356.1 ms | reported |
| `threads_list` folder view P95 | 69.9 ms | 358.6 ms | reported |
| `thread_get` 50 P50 | 0.4 ms | 0.4 ms | < 50 ms || `thread_get` 50 P95 | 1.2 ms | 1.2 ms | < 50 ms |
| 100 real inserts with 3 refs, total | 308 ms | 307 ms | < 2 s |
| A real 2 KiB insert P50 / P95 | 2.7 / 4.5 ms | 2.7 / 6.0 ms | P50 < 20 ms |
| First open of a P9a store (thread cache build) | 386 ms | 1 956 ms | one-time, reported |
| Later open | unchanged from P9a | unchanged from P9a | not re-measured |

The account-wide list, `thread_get` and the real-ingest targets are met. **The folder view misses
<100 ms at 100k:** deciding which conversations have a message in a folder crosses the covering
message index (100k entries) and then counts the matching thread cache; a denormalised folder scope in
the cache is the next step. The P9a upgrade cost is a one-time cache build, not part of a steady-state
open. There is no new dependency and no schema version bump; normal open creates the derived indexes
idempotently, and the FTS index is not consulted by thread listing. Two bounds are deliberate: the
participant/label facts of a page are read from at most 2 000 message rows and 4 000 label rows, so a
very large page may omit late participants, and rows written by P9a keep the old `subject_norm` without
a stripped prefix, so the subject fallback can miss pre-P9 mail until a future backfill.

### 4.13 Implemented in P9c: local rules

A local rule is one validated search query plus 1–5 actions. The kinds are `move(folder)`,
`label(label)`, `read`, `unread`, `trash` and `forward(1–10 addresses)`, at most one of each; `read`
and `unread` exclude each other. Names are 1–48 characters, unique per account ignoring case. SQLite
`NOCASE` is ASCII-only, so `rules.name_key` stores Unicode lowercase and a unique
`rules(account_id,name_key)` index enforces the rule the UI and docs promise; both columns and both
indexes are added idempotently at normal open to the unshipped v7 file (not a schema version). An
account holds at most 100 rules. Execution order is `priority ASC, id ASC`; `stop` makes later rules
skip a message this rule matched. Folders, labels and recipients are validated on save; folder and
label targets accept an id or an exact name, and a label must belong to the rule's account. A target
deleted later leaves the row visible as `valid:false` with a readable error and fails at run time; it
never corrupts the run. `move` refuses virtual folders, Drafts and Sent, so a rule cannot turn received
mail into a draft or a second Sent copy.

**Candidates and provenance.** `messages.origin` records why a row exists: `received`, `sent`, `draft`
or `local`. Sync writes it from the folder it ingested, `send_draft_message` turns the local draft into
`sent`, and a later move never rewrites it. Rules require `origin = 'received'` **and** a folder that is
not Drafts, Sent or Trash: an archived Sent copy is still `sent` and is never a candidate, while a
received message moved to Sent stops matching until it is moved back. Existing v7 rows are classified
once at open from their current folder; this is an in-place extension of the unshipped v7 schema.
Execution is per rule, in order, and each rule queries the store again so it sees the earlier rule's
moves, labels and flags. The rule's own `created_at` is the horizon: `messages.ingested_at >=
rules.created_at`, so a new rule never rewrites mail that arrived before it existed.

`new` (the default, and the pass that follows `sync_now`) additionally requires `rules_applied_at IS
NULL`; it takes one bounded universe of 2 000 new received messages and runs every rule over it. A
message is marked with `rules_applied_at` only when **every action completed**: an action failure or a
deferred forward leaves it pending, so fixing the SMTP endpoint or the stale target retries it on the
next `new` run. `all` gives each rule its own 2 000-candidate page ordered by `ingested_at,id`, so a
rule created today is not starved by an older rule's longer history. Each rule keeps an encrypted
`rule_all_cursor:<rule>` position; a full page advances it, a page that ended with a deferred forward
keeps it so the next run retries that page, and a short page clears it so the following run starts
again at the beginning. Repeated `all` runs therefore advance through a backlog larger than 2 000
instead of reading the same page forever. The `all` query is served by
`messages(account_id,origin,ingested_at,id)`; `new` by
`messages(account_id,origin,rules_applied_at,ingested_at,id)`. The demo engine mirrors the same
completion, cursor and action semantics without a store horizon.

**Actions and idempotency.** Every action is applied immediately. A move/label/read/trash action that
is already in its target state is a no-op and writes no journal row. Store-side checked methods
(`rule_move_message`, `rule_set_flags`, `rule_set_label`) re-read origin and the expected folder in the
same transaction, so a concurrent move to Sent/Drafts/Trash is skipped rather than overwritten; after
the rule itself moves a message to Trash, its remaining actions see `folder_kind = Trash` and stop.
A forward resolves to one outbox row with the deterministic key `rule:<rule_id>:<message_id>` and a
durable encrypted marker `rule_forward:<account>:<rule>:<message>`; the marker survives the 30-day
outbox prune, so a repeated run never sends the same message twice. Only a newly created outbox row
consumes the 50-forwards-per-run budget. Once the budget is spent, the remaining forward actions are
recorded as skipped/deferred (not as errors), later actions still run, and the message stays pending
for the next run. A failing action increments the run's error count, records a fixed sentence naming
rule, message and target ids, and aborts only that rule's remaining actions for that message. The
returned error list is capped at 200 sentences plus one “N more errors omitted” line; the run keeps the
exact count. A later rule still runs. A rule failure never fails `sync_now`: the run is journaled and
the sync report is unchanged.

**Forward through P7.** The engine builds a standalone submission (no draft, so there is no draft to
move to Sent), serializes it through the same `postino-sync`/`postino-mime` path as compose, stores
the exact bytes in the encrypted blob store and inserts one `outbox` row. `OutboxEnvelope.draft_id`
is optional for this row; submission and the Sent append use the stored bytes and `finalize` is
skipped. The message carries `Auto-Submitted: auto-forwarded` and `X-Postino-Rule: <rule_id>`. Sync
records either incoming header in the encrypted `meta` key `rule_guard:<account>:<message>`; a forward
on a guarded message is skipped and does not consume the budget. Deleting one message, a bulk sync
deletion, a UIDVALIDITY reset and account removal all sweep the guard keys (account removal also
sweeps orphan keys left by an older bulk pass). A missing SMTP endpoint fails the action with a
sentence instead of queueing an undeliverable row. MCP refuses `rules_run` without `--allow-send`,
refuses a `rules_create`/`rules_update` payload containing a forward action before the typed parser
runs, and refuses an enable-only `rules_update` of a stored forward rule the same way. The MCP engine
also carries `allow_send` into `sync_now`, whose automatic `new` pass records a forward as deferred
instead of queueing it; the sync itself still succeeds. The CLI and desktop leave the gate open.

**Cross-process updates.** `rules_update` re-reads the store row before merging absent fields, refuses
the edit when the engine's in-memory `updated_at` no longer matches (“changed in another session;
reload…”), validates the merged action list (not only a supplied `actions` value), and writes with
`UPDATE … WHERE updated_at = expected`. A stale editor therefore cannot overwrite a newer action list
from another process.

**Journal and undo.** `rule_runs` stores the mode, start/finish, rule count, matched pairs, changed
actions, error count and (in `meta`) whether undo already reversed it; the newest 20 non-dry runs per
account are kept and older runs are deleted with their actions. `rule_actions` stores run/rule/message
ids, the action name, a changed flag and a JSON value with ids, folders, labels and booleans only:
never a subject, address, query or body. Each changed action's value also carries the message's
post-action state (folder id, unread and starred booleans, label ids). `rules_undo` groups the run by
message, compares the live state with that last snapshot, and skips every action for a message changed
after the run; an unchanged message is reversed newest-action-first. Starring a message after the run
therefore protects it too. A forward is never undone: the undo result reports its outbox id so the
user can cancel the row. A run can be undone once; undo is a one-way meta marker, not a second
journal. The demo removes the action log of a run dropped by the 20-run retention.

**Preview.** `rules_preview {account?, query, actions, limit?, rule?}` writes nothing: no journal, no
`rules_applied_at`, no flag, move, label or outbox row. Without `rule` it previews every received
message as an explicitly hypothetical new rule; with `rule` it applies that rule's `created_at`
horizon, matching what a run would consider. It returns the first 50 matches and one typed effect per
action (`apply`, `no_op`, `skip`). A forward is `skip` when the account has no SMTP endpoint or the
message is loop-guarded, and a move updates the simulated folder kind, so a later `trash` after a move
to Trash is `no_op` rather than `apply`.

**Operations and UI.** `rules_list`, `rules_create`, `rules_update`, `rules_delete`, `rules_preview`,
`rules_run`, `rules_undo` and `rules_runs` all use `deny_unknown_fields`, schemas derived from the
types, and `read_only`/`destructive` annotations. `rules_undo` is destructive: it moves, flags and
labels mail and has no redo. `rules_run` is destructive/open-world and `rules_delete` is destructive.
The desktop Rules page is reachable from Settings and the palette (`Regole locali`, `Nuova regola`),
lists rules, edits condition/actions/priority/stop/enabled, shows the first 20 preview matches, offers
Save/Cancel/Delete, run with confirmation and Undo, and lists all 20 retained runs in a scroll area.
The Undo button belongs to the run that produced the notice, not to whatever run is newest. Forward is
disabled with a visible reason when the account has no P7 SMTP endpoint; the demo has none. The page is
a 980×600 modal built from the same components and tokens as Settings. It is part of the modal guard:
`E`, `Delete`, `Z`, `J`/`K`, `Enter` and the palette do not reach the mailbox behind it, and `Esc`
closes only the page, leaving the selection and reading pane untouched. P9c adds no crate or
dependency, no new SMTP client and no schema version: the v7 tables, the in-place origin/name-key
columns and idempotent indexes carry it.

**Evidence.** `crates/core/tests/rules_perf.rs` seeds a 20k (CI) or 100k
(`POSTINO_SLOW_TESTS=1`) mailbox with raw SQL, creates a rule, then inserts 2 000 matching messages
after its `created_at`. The measured paths are the ones `sync_now` and the UI call; each row is five
runs on the Windows GNU host, debug build:

| Measure | 20k P50 / P95 | 100k P50 / P95 |
|---|---|---|
| `new`, 2 000 matches with read actions | 7 261 / 7 201 ms | 10 040 / 9 993 ms |
| `new`, empty page (the common post-sync pass) | 67 / 67 ms | 327 / 316 ms |
| `all`, one rule, 2 000-candidate page | 184 / 174 ms | 455 / 424 ms |
| `all`, ten rules, ten pages | 1 161 / 1 162 ms | 1 417 / 1 422 ms |

The write actions dominate a full page; the query-only and empty passes stay under 0.5 s at 100k. The
fixture is intentionally write-heavy (2 000 unread messages changed per page), so these numbers are an
upper bound for a read-only rule and a lower bound for a page of forwards. `crates/store/tests/rules.rs`
asserts both candidate queries use `messages_rules_new` / `messages_rules_all` in `EXPLAIN QUERY PLAN`.

### 4.14 Implemented in P12c1: OpenPGP keys in the encrypted store

Schema v8 is additive from v7: one `pgp_keys` table keyed by account, with an uppercase fingerprint
unique across the store, key id, optional primary user id, public armor, optional secret armor,
`protected`, created/updated timestamps and a `CHECK` that only a protected row may carry a secret.
The secret column is written by import and never selected again; list, get and export do not read it,
and it is never returned to the engine, CLI, MCP or logs. PGP secrets do not use the OS keyring: the SQLCipher store
is the encryption boundary, and the private key stays passphrase-protected even inside it. Parsing is
bounded to 1 MiB and re-armored; import verifies every secret packet with the passphrase (256-byte
cap) before writing, refuses unprotected private keys, and treats a duplicate fingerprint as a
conflict. Imports are capped at 64 keys per account in one immediate transaction. The registry adds
`pgp_keys_list` (read-only, metadata), `pgp_key_import` (sensitive), `pgp_key_delete` (destructive)
and `pgp_key_export_public` (read-only); MCP excludes the import. The library decision and its
licence review are in `docs/decisions/0007-pgp-key-storage.md`. P12c1 does not verify, decrypt, sign,
encrypt, fetch keys or downgrade to plaintext; P12c2 added the MIME and UI layers.

### 4.15 Implemented in P12c2a: exact bytes for received PGP/MIME

Schema v9 is additive from v8: nullable `messages.raw_blob_hash`, a reference into the existing
encrypted content-addressed blob store, with no new table, index or dependency. `postino-sync`
decides retention from the root media type of the parsed entity: `multipart/signed` and
`multipart/encrypted` keep the exact received RFC 5322 bytes (CRLF, header folding and the detached
signature part are what a verifier needs), and ordinary mail keeps nothing. The blob is written
through the P4b seam before the message row names it, like an attachment; `max_message_bytes` is the
bound, because the MIME parse already refuses a larger entity and the run skips an over-limit message
whole. The raw column is not a second body: `body_text`/`body_html` and FTS still come from the MIME
parse, an encrypted entity shows no plaintext until P12c2b decrypts it, and nothing re-derives the
bytes from the parsed body. `Store::message_raw_blob_hash` returning `None` is the honest
`Not stored` state for ordinary mail and for every row cached before v9; a refresh may fetch the
entity again. A move, an edit and a de-flag never touch the column, and the GC adds
`messages.raw_blob_hash` to both the referenced set and the per-candidate recheck, so a sweep cannot
delete bytes a cached PGP/MIME message still needs. Migration v8→v9 runs in one transaction
and a failed v9 keeps `user_version = 8`; an older binary refuses a v9 file as too new. P12c2a adds
no crypto, no operation, no CLI/UI surface and no way to verify a message from a body that was
re-parsed locally.

### 4.16 Implemented in P12c2b: PGP/MIME verification, decryption and protected sends

`postino-mime::pgp_mime` is structure only: it recognizes RFC 3156 `multipart/signed` and
`multipart/encrypted` with a PGP protocol, keeps the exact first part of a signed message (the byte
string the signature covers, not a re-serialized body), transfer-decodes the detached signature or
the encrypted payload, and serializes outgoing entities with a boundary and CRLF contract that keeps
the signed bytes stable. It never parses OpenPGP and never decides whether a signature is good. The
usual MIME limits apply before parsing: body size, header size, part size, exactly two parts.

`postino-core::pgp_mime` owns the OpenPGP calls and reads received bytes from the v9 raw blob.
`message_pgp_verify` (read-only, offline) reports `verified` with key id, fingerprint and signer,
`bad` on a mismatch, `no_key` when the account has no stored public key, `unknown` when the entity or
the stored key cannot be read, and `not_stored` for rows that predate v9; it never fakes a result
from the parsed body. The result also carries a `kind` (`none`, `signed`, `encrypted`) independent of
signature validity, so a client can offer "decrypt" without treating an encrypted message as a
failed signature. A detached signature is checked against the primary key and every signing
subkey, so a certificate that signs with a subkey is not reported as `bad`. `message_pgp_decrypt` is
`sensitive`: the per-use passphrase comes from stdin or a no-echo prompt, is zeroised, and never
appears in argv, the environment, a log or a result. The decrypted payload is decompressed through a
bounded reader and re-parsed under the normal MIME limits; HTML goes through the existing sanitizer,
and the text/html are returned to the caller only. They never overwrite `body_text`/`body_html` and
never enter FTS, so `message_get` and `search` keep seeing the encrypted row.

`PgpSendOptions { sign, encrypt, passphrase }` is an optional field of `message_send`,
`send_schedule` and `send_dry_run`, not a separate draft operation: the draft is serialized once and
the same bytes are protected, hashed and journaled, so there is no window in which the draft can
change between "protect" and "send". Signing wraps the MIME entity in `multipart/signed` with a
SHA-256 detached signature; encryption wraps the signed entity in `multipart/encrypted` and targets
the newest non-revoked, unexpired encryption-capable subkey (falling back to a primary key that
carries an encryption flag); the outer RFC 5322 headers are copied unchanged. A retry that finds an
existing queued row returns it only when the newly serialized bytes have the same BLAKE3 hash as the
row's blob: a plaintext row can never satisfy a signed or encrypted request. A missing, unreadable,
revoked or expired recipient key, a locked or wrongly unlocked signing key, or a demo store (no
encrypted key material) aborts before a blob or an outbox row is written: there is no cleartext
fallback. A send addresses at most 100 keys. The passphrase is skipped by `Serialize`, redacted by
`Debug` and zeroised on drop; MCP and `postino run` refuse any send request that carries one, and MCP
never lists or dispatches `message_pgp_decrypt` or `pgp_key_import`. The CLI adds
`postino pgp message verify|decrypt`, `postino pgp keys ...` and
`postino send --sign <id> --encrypt <id>... [--passphrase-stdin]`.

The desktop app (P12c2c) calls `message_pgp_verify` once when a message opens and renders the cached
verdict, not a guess from the body: a verified signature, a bad signature, a missing key, an unknown
state, and an encrypted message each have a distinct badge, with the signer and fingerprint in the
tooltip. "Decifra" first tries an unprotected key, then opens the one passphrase sheet; a `locked` or
`wrong_passphrase` result stays visible and the sheet can be retried. The decrypted text/html is
parsed by the same sanitizer and the same `SubsetBodyRenderer` as stored mail, cached only in memory
for the open message, and never written to the row or FTS. A "Nascondi" action drops that plaintext
again, and importing or deleting a key invalidates the open verdict instead of leaving a stale badge.
The composer has a per-send sign choice
over secret keys and a multi-select encryption menu over stored keys; the protected signing key opens
the same sheet and the passphrase is inserted only into that `message_send` call. Settings lists the
account's keys with import, public-armor copy and delete; import accepts public or passphrase-protected
private armor and keeps the secret material out of the list.

Limits: keys are local only, with no discovery or revocation fetch; verification does not consult
revocation or expiry (the send path does, because it chooses a key), and there is no S/MIME, no HTML
signing and no key UX beyond the settings table. Received mail cached before v9 has no raw bytes and
reports `not_stored` until it is refreshed; because the bytes are gone the app cannot tell whether
such a row was PGP at all, so it shows no PGP badge rather than a false one.

## 5. Synchronization


IMAP is the synchronization protocol. There is no Postino server.

P5a ships the client (`postino-net`): one connection, TLS, the commands in §4.4, typed data out. P5b
ships the reconciler (`postino-sync`) described in §4.6: one account, one store connection, one run at
a time. P6a wires it to the engine: `sync_now` is the only operation that calls it, and the CLI, MCP
and the desktop worker reach it through the same registry. P10a's daemon runs it on start, on a
jittered interval and on a manual request; the app's in-process worker remains for P10a1.

The target loop, still incomplete:

- Discover settings with RFC 6186 SRV records, autoconfig, then a manual fallback.
- One connection per account for IDLE; separate short-lived connections for bulk fetch.
- `UIDVALIDITY` changes invalidate the folder cache. `CONDSTORE`/`QRESYNC` where supported; otherwise
  UID ranges and flag fetches (P5b implements the UID-range half, no QRESYNC).
- Fetch envelopes and flags first; bodies on demand, with a bounded prefetch of recent mail. P5b
  downloads each body during the reconciliation pass; on-demand reads are P6.
- Send is optimistic and journaled in `outbox` with an idempotency key (P7); flags, moves and draft
  saves are still local-only. Replaying them on reconnect is not implemented.
- Sent mail is `APPEND`ed to the server's Sent folder with `\Seen` (P7) and retried independently of
  submission. A local Sent row and the server copy with the same `Message-ID` are one row, including
  the provider auto-save and Sent/Archive move orders; when the server copy was cached first, the local
  row adopts its UID and the APPEND is skipped. A copy moved back to Drafts is a draft being edited and
  is left alone by a Sent sync. Deduplicating two copies that the *server* already holds is P8/P9.
- Drafts are appended with a stable client `Message-ID` and revision; IMAP has no atomic update, so
  two devices can briefly create two copies. This is a protocol limit, documented rather than
  hidden. Not implemented.
- Two devices converge through the server: IDLE pushes within seconds when the network and provider
  cooperate; otherwise polling (default 60 s) is the floor. P5b is a one-shot run; the daemon added
  the interval scheduler (P10a) and the sync loop still polls — the client's single-cycle `IDLE` is
  not wired into a push loop yet.

ADR-0003 records the decision and the rejected alternatives (POP3, a custom sync service, Gmail API
as the primary protocol).

## 6. Rendering mail

The document format is HTML, and the only faithful renderer is a browser engine. The core does not
pretend otherwise.

```rust
pub trait BodyRenderer {
    fn render(&self, html: &SanitizedHtml, width: u32) -> BodyView;
}
```

- **M0-M4:** a `TextBodyRenderer`; the reading pane showed the stored plain-text alternative.
- **Shipped (P11a1/P11a2):** a bounded, sanitized block model rendered in egui — paragraphs,
  headings, emphasis/strong/code, lists, quotes, simple tables, links and `cid:` alt text as text —
  with the text fallback kept for damaged, empty or oversized bodies. Remote images stay blocked and
  are counted.
- **P11b (open):** an optional `WebviewBodyRenderer` behind a feature flag. It uses the OS webview
  (WebView2 / WKWebView / WebKitGTK) in a dedicated, sandboxed surface: no JavaScript, no
  application IPC, a strict CSP, remote content blocked, and a custom protocol for local
  attachments. The rest of the app stays egui.
- **Planned (M5):** the composer gains two modes, **base** (block editor: paragraph, heading, list,
  quote, divider, image, button, signature) and **advanced** (HTML source with a constrained Tailwind
  subset, linting and live preview). Both compile to table-based, inline-styled, email-safe HTML with a
  plain-text alternative.

This is the one deliberate exception to "no webview", and it is isolated behind a trait. See
ADR-0001.

## 7. Security architecture

- TLS only; certificate validation cannot be disabled from the UI. The IMAP client verifies with
  the operating system's verifier, refuses STARTTLS downgrades, and has no plaintext mode at all.
- Sync is read-only against the server: bodies arrive only through `BODY.PEEK[]`, and the reconciler
  never sends `STORE`, `EXPUNGE`, `COPY`, `MOVE` or `APPEND`. Its status record holds counts and a
  fixed failure sentence, never a server name, an address, a credential or message text.
- Credentials in the OS keyring; OAuth2 PKCE with a loopback redirect for providers that require it.
- Store encrypted at rest: SQLCipher for the database, XChaCha20-Poly1305 under an HKDF subkey for
  attachment blobs; the key is generated on first run and kept in the keyring.
- HTML sanitized with an allowlist; remote images and tracking pixels blocked by default; no
  scripts.
- Header injection rejected at compose time (no CR/LF in addresses or subjects).
- Attachment names sanitized; traversal, zip bombs, decompression bombs and oversized MIME parts
  bounded.
- Link display shows the real destination; IDN/punycode warnings.
- Authentication results (SPF/DKIM/DMARC) surfaced, not silently trusted.
- No telemetry. The app never phones home. Update checks, if ever added, are manual and opt-in.
- MCP is stdio-only and local. The daemon pipe is same-user and token-authenticated. Tool and
  resource calls are capped at 300 per rolling minute and every call is appended to a JSONL audit log
  (timestamp, event, name, outcome; no arguments, bodies or credentials). `--compact` reduces the
  advertised tool payload without removing schemas; resources expose only read-only mailbox data.

The full threat model is in `docs/threat-model.md`.

## 8. Quality gates

`cargo xtask ci` runs formatting, clippy (`-D warnings`), tests, the layering check and the asset
check. The UI has a headless render test; the operation registry has a schema/name test; MCP has
protocol tests. Every future feature adds an end-to-end operation test. The security workflow
(`.github/workflows/security.yml`) is separate: it runs `cargo deny check` against `deny.toml` and
the four libFuzzer targets in `fuzz/` for 120 s each on Linux, plus a 5,000-run Windows job with MSVC
AddressSanitizer. Local Windows runs use `fuzz/run-windows.ps1`; the local gate keeps the deterministic
mutation corpus so `cargo xtask ci` stays fast.

## 9. Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-23 | major | M0: workspace, core registry, demo engine, egui shell, CLI, MCP, docs |
| 2026-10-25 | major | P3a: encrypted SQLCipher store proof and the build decision |
| 2026-10-25 | major | P3b: platform keyring seam, schema v2, repository and tests |
| 2026-10-25 | major | P3c: engine loads the store once and writes through; CLI `--store`; demo unchanged |
| 2026-10-25 | major | P3d: schema v3 (`draft_seq`, `label_seq`), atomic draft save/send, draft patch semantics, per-account Sent, orphan repair, keyring first-run lock, categorised store errors |
| 2026-10-25 | major | P4a: `postino-mime` parses, sanitizes and serializes MIME with bounds, warnings and injection guards |
| 2026-10-25 | major | P4b: `postino-crypto` derives HKDF subkeys and seals with XChaCha20-Poly1305; `postino-store` gains a deduplicated, compressed, encrypted blob directory for attachment bytes and the engine gains the attachment read/write seam |
| 2026-10-25 | minor | P4c: blob review closure — per-database-file blob namespace, authenticated dedup that heals a corrupt file, BLAKE3 verification on read, durable publish, size-preserving attachment patches, no echoed ids |
| 2026-10-25 | major | P5a: `postino-net` speaks IMAP4rev1 over implicit TLS or STARTTLS with OS verification, bounded lines/literals/commands, typed errors and a scripted hostile-server corpus; the sync loop and store writes are still missing |
| 2026-10-25 | major | P5b: `postino-testkit` moves the scripted server and TLS CA out of the net tests; `postino-sync` reconciles folders, UIDVALIDITY, incremental UID windows, MIME bodies, attachment blobs, deletions, flags and a persistent status with cancellation. No operation, CLI or UI calls it yet. |
| 2026-10-25 | major | P6b: M1 onboarding — autodiscovery, the Aruba app-password flow, and an app that boots on the encrypted store and syncs a real mailbox in the background |
| 2026-10-26 | minor | P6c: M1 reading and settings — text reading pane driven by the stored remote-content count, safe-link confirmation, attachment Save, full settings and chrome cleanup |
| 2026-10-26 | major | P7: SMTP submission over implicit TLS/STARTTLS, schema v5 outbox journal, exact-bytes-before-network queue, lease/backoff/auth-halt state machine, uncertain-after-DATA policy, independent Sent APPEND with `\Seen`, `message_send`/`send_schedule`/outbox operations, CLI send/outbox, MCP `--allow-send`, desktop worker and outbox dialog |
| 2026-10-26 | major | P8: M2 compose depth — real attachment bytes through blob → draft → queue → MIME, per-account signatures, reply/reply-all/forward recipients and threading headers, 20/25 MiB caps with no schema change |
| 2026-10-26 | minor | P6d: schema v6 `messages.remote_images`, `ParsedMessage::remote_images_blocked` carried through sync and the store, `message_get` field and the Italian reading-pane line from stored truth; no HTML rendering and no remote fetch |
| 2026-10-26 | major | P9a: body-free store read path (summaries + owned cached `message`), schema v7 search columns and contentless-delete FTS5, recursive-descent query language with canonical printer, typed bound execution with bm25 ranking, `search`/`search_status`/`search_reindex`, CLI/MCP/UI parity, 100k evidence |
| 2026-10-26 | major | P9b: reference-chain conversation derivation with phantom joins, order-independent merges and aliases, bounded subject fallback, cached thread rows with mailbox-wide counts, thread operations, one-open-card conversation view at 980, saved searches with stale-aware computed counts, recent queries in encrypted meta, CLI/MCP parity, 20k/100k evidence |
| 2026-10-26 | major | P9b closure: `messages(account_id,message_id)` and `subject_norm` indexes (1.29 s/ref → 2.7 ms/insert), sent-draft ghost removed, store-backed thread flag counts, live thread pages and paging, subject-fallback re-fetch stability, saved-search freshness/account cleanup/atomic cap/account scope, conversation Enter, partial-batch undo, read-only cache guard |
| 2026-10-27 | major | P9c: local rules on the v7 tables (no migration), validated query + 1–5 actions, priority/stop, received-only new-vs-all runs with `ingested_at`/`rules_applied_at`, 2 000-candidate and 50-forward caps, idempotent actions, journal with ids/booleans only and 20-run retention, state-checked undo, standalone rule forward through the P7 outbox with `Auto-Submitted`/`X-Postino-Rule` loop guard in encrypted meta, eight operations, CLI/MCP/UI parity and 980 rules screenshots |
| 2026-10-27 | minor | P9c closure: MCP `--allow-send` now also gates sync-triggered forwards and enabling a stored forward rule; the rules page joins the modal guard; `all` keeps a per-rule cursor; `new` marks only completed actions; the 50-forward budget counts only new deduped rows and defers the rest; a durable forward marker survives outbox pruning; `messages.origin` and Unicode `name_key` extend v7 in place; checked store actions re-read origin/folder; `rules_update` is revision-checked; guard keys are account-scoped and bulk-cleaned; candidate indexes and 20k/100k evidence; starred-aware undo, bounded errors, preview horizon/SMTP effects, all 20 runs in the UI and `rules_undo` marked destructive |
| 2026-10-27 | major | P10a1: `postino-ipc` (per-user named pipe/DACL or 0600 UDS, per-launch owner-only token, 16 MiB JSON-RPC frames, bounded queues, cancel, heartbeat), `postino-daemon` (independent `<store>.lock`/owner record, outbox recovery at every P7 state, sync/rules/index batches/GC scheduler, `daemon_status`), single-writer refusal for direct CLI/app writers, CLI and MCP daemon clients, absent-daemon fallback matrix; tray/autostart/notifications remain P10b and the app client is P10a2 |
| 2026-10-27 | major | P10b1: platform `Tray`/`Autostart`/`Notifier` seams with recorders and one scoped Win32 module (`Shell_NotifyIcon`, hidden daemon window, ICO asset), daemon-owned tray/menu state, per-user HKCU Run autostart and `postino daemon autostart status|on|off`, privacy-preserving debounced notices with one encrypted scalar horizon, atomic `%APPDATA%\Postino\daemon.json`, release GUI subsystem and bounded log; ADR-0006 records the Run-key deviation from Task Scheduler; app settings/close-to-tray remain P10b2 |
| 2026-10-27 | major | P10a2: the desktop app becomes a daemon client — it opens the store read-only, mutates through `op`, reconnects after a daemon restart and preserves the selected view |
| 2026-10-27 | minor | P10b2: app settings for tray, close-to-tray and autostart, plus activation of the running instance and crash recovery |
| 2026-10-27 | major | P11a1: bounded sanitized block model and parser in `crates/mime`, fed by the stored sanitized subset |
| 2026-10-27 | major | P11a2: the reading pane renders those blocks in egui with the link sheet and text fallback inside the 980×600 budget; an optional sandboxed WebView2 backend remains P11b |
| 2026-10-27 | minor | P12d: parse-only `icalendar 0.17.17` behind the MIME bounds (256 KiB, 1 000 components, 2 000 attendees, 64-character organizer/attendee lines), `message_invite` read and organizer-only `invite_reply_draft` with a METHOD:REPLY part that preserves DTSTART/DTEND parameters and SEQUENCE, a reading-pane invite card driven by the shared command table, CLI/MCP parity and one 980 screenshot; only received mail is answerable; the synthetic demo has no calendar part, so the full text+calendar reply is exercised on the encrypted store; no calendar store, RRULE expansion, timezone database, auto-send or schema migration |
| 2026-10-27 | major | P12a: Gmail OAuth/XOAUTH2 with tokens in the OS keyring, IMAP/SMTP auth over the same credential seam and app onboarding |
| 2026-10-27 | minor | P12e: per-user Windows MSI and portable ZIP with signing and a manifest |
| 2026-10-27 | major | P12c1: OpenPGP key storage (schema v8), key operations and the CLI surface |
| 2026-10-27 | major | P12c2a: schema v9 retains the raw signed/encrypted bytes and the daemon's GC sweeps the unreferenced ones in bounded batches |
| 2026-10-28 | major | P12c2b: PGP/MIME verify, decrypt and protected sends through the MIME layer, operations and CLI |
| 2026-10-28 | minor | P12c2c: PGP reading badges, composer sign/encrypt toggles and the key manager table in the app |
| 2026-10-28 | major | P12f: standalone libFuzzer targets for MIME/HTML/PGP-MIME/calendar outside the workspace, a `security` workflow with `cargo deny check`, 120 s per target on Linux and a native Windows MSVC AddressSanitizer job, `fuzz/run-windows.ps1`, and rustls 0.23.46/tokio 1.53.3 security updates |
| 2026-10-28 | minor | P13a: MCP read-only resources for mailbox status, folders, recent messages and message/thread templates; `--compact` tool listings; a 300-call rolling rate guardrail and a JSONL audit log with 8 MiB rotation, no arguments or bodies |
| 2026-10-28 | minor | P13b: `attachment_get` returns bounded exact bytes through the same registry; the reading pane adds a 64 KiB text / 2048 px image preview, hostile controls replaced, image dimensions and allocation capped, and the sheet drops its plaintext when closed |
| 2026-10-28 | minor | P13c: local compose templates in the encrypted `meta` table (no schema migration, ≤ 20 rows) with `templates_list`/`template_create`/`template_update`/`template_delete`, a composer insertion that never discards existing text, and Settings create/edit/delete |
| 2026-10-28 | minor | P13d: ingest records bounded RFC 2369/8058 `List-Unsubscribe` headers in encrypted `meta`, deleted with the message; `message_unsubscribe` classifies at most four `mailto`/`https`/`http` targets and the app offers an explicit draft or the normal link confirmation, never a background request |
| 2026-10-28 | minor | P13e: with `X-GM-EXT-1`, sync reads `X-GM-LABELS` in envelopes and in the liveness scan and maps custom labels to mailbox-wide labels (system labels stay folders); applied ids live in encrypted `meta`, so later syncs remove only their own labels, and names/counts are bounded with control/bidi characters rejected |
| 2026-10-28 | minor | P13f: `message_export` returns the exact retained v9 raw bytes or rebuilds a sanitized `.eml`, `message_import` runs a chosen file through the ordinary ingest path into a local **Import** folder without treating a duplicate Message-ID as a move, and the CLI/app/MCP share the same 10 MiB bound so base64 stays inside the 16 MiB daemon frame |
| 2026-10-28 | minor | P13g: `folder_export_mbox`/`folder_import_mbox` add bounded Thunderbird-style mbox transfer; the splitter validates every separator against the following header before any folder is created, removes one mboxrd quote layer, imports into a named local folder through the ordinary ingest path, reports skip counts and keeps the same 10 MiB daemon-frame bound |
| 2026-10-28 | minor | P13h: `draft_save.body_format` compiles the Markdown-like source into table-based HTML with inline CSS and a matching text alternative; the composer toggles plain/HTML and previews the exact output, the engine recompiles compiler-owned drafts at send time, raw HTML is never accepted and the source is capped at 1 MiB |
| 2026-10-28 | minor | P13i: up to 10 alternative From senders per account live in encrypted `meta` (index + row per identity, no schema migration); `identities_list`/`identity_create`/`identity_update`/`identity_delete`, `draft_save.from`, a composer **Da** picker with signature swap, Settings management, and a send-time check that the draft From is the account or a current identity |
