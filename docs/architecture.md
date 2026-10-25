# Postino architecture

> **Status:** M0 walking skeleton + U0 UI foundation + engine on the encrypted store (P3c/P3d) + MIME
> crate (P4a) + encrypted attachment blobs (P4b) + IMAP client (P5a) + sync into the store (P5b) ·
> **Last updated:** 2026-10-25 ·
> **Target:** a local-first, agent-first mail client that is fast on every desktop.

## 1. Shape today

One Cargo workspace, edition 2024, Rust 1.98+:

| Path | Role |
|---|---|
| `crates/core` | Domain types, the engine (demo or store-backed), the operation registry and its JSON Schemas |
| `crates/crypto` | HKDF-SHA256 subkeys and XChaCha20-Poly1305 sealing; the blob subkey (P4b) |
| `crates/mime` | MIME parsing, HTML sanitization, HTML-to-text and RFC 5322 serialization (P4a) |
| `crates/platform` | OS seams: store key and account credentials in the OS keyring |
| `crates/store` | SQLCipher schema v4, migrations, the repository and the encrypted blob store (P3b, P4b, P6a) |
| `crates/sync` | One account from IMAP into the store: folders, UIDVALIDITY, incremental fetch, MIME, blobs, deletions, status (P5b) |
| `crates/testkit` | Test-only fake IMAP server, TLS fixture, response builders and synthetic mail (dev-dependency only) |
| `apps/postino-app` | egui/eframe desktop shell, design tokens, banded three-pane layout |
| `apps/postino-cli` | `postino` command line and the opt-in MCP server |
| `xtask` | `ci`, `layers`, `assets` |

The engine has two sources of truth, chosen at construction:

- `Engine::demo()` / `demo_at()` — synthetic mail in memory, no I/O. This is what the desktop app
  still boots on until onboarding (P6b).
- `Engine::open(store)` — the encrypted SQLCipher store, loaded once and written through on every
  mutation. The CLI reaches it with `--store <path>` or the default local path; the key comes from the
  OS keyring.

The UI, CLI and MCP read both through the same `Engine::call(name, args)` door. The MCP server and
the GUI never touch SQL directly. Nothing is fetched from a server and nothing is sent: IMAP is P5,
SMTP is P7.

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

Current operations: `app_info`, `accounts_list`, `folders_list`, `messages_list`, `message_get`,
`message_flag`, `message_move`, `draft_save`, `draft_send`, `search`, `labels_list`, `label_create`,
`label_update`, `label_delete`, `label_apply`. They are enough to prove the path through all three
clients. `draft_save` updates one draft in place and `draft_send` is a local demo until SMTP arrives
in M2; neither opens a network connection. Labels are local metadata: in memory in the demo, in the
encrypted store otherwise, and never synced to a server.

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

- starts at login (Task Scheduler, LaunchAgent, `systemd --user`) and lives in the tray;
- holds the SQLite connection in WAL mode and the IMAP/SMTP connections;
- runs the outbox scheduler and local rules;
- listens on a named pipe (Windows) or Unix domain socket, accepts the same user only, and
  authenticates with a fresh random token written to an owner-only file;
- speaks the same JSON-RPC 2.0 as MCP, over a different transport.

M0 runs everything in-process because there is nothing to synchronise yet. ADR-0002 records the
decision and the migration path.

### Planned crates

| Crate | Responsibility |
|---|---|
| `postino-crypto` | HKDF-SHA256 subkeys, XChaCha20-Poly1305 seal/open, zeroising key type (P4b) |
| `postino-store` | SQLCipher schema, migrations, repository (P3b); encrypted content-addressed blobs (P4b); FTS5 later |
| `postino-mime` | MIME parse/serialize, HTML-to-text, sanitization, header-injection guards (P4a; attachment blob storage was P4b) |
| `postino-net` | IMAP4rev1 client (P5a): TLS, bounded responses, testable against a scripted server. SMTP submission, autodiscovery, QRESYNC later |
| `postino-sync` | Reconciliation (P5b: folders, UIDVALIDITY, incremental UID fetch, MIME, blobs, deletions, status); outbox, scheduler, full threading later |
| `postino-compose` | block model → email-safe HTML, CSS inlining, signatures, templates |
| `postino-rules` | local rules and Sieve generation |
| `postino-ipc` | JSON-RPC over pipe/socket, token, versioning |
| `postino-platform` | keyring (P3b); paths, notifications, tray, autostart, single instance later |
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
        subject, date, size, flags, blob_hash, auth_results)
thread(id, account, subject_norm, last_date, unread, message_count)
attachment(id, message, filename, mime, size, blob_hash)
draft(id, account, revision, updated_at, blob_hash, in_reply_to)
draft_seq(message_id, revision)
outbox(id, account, idempotency_key, created_at, send_at, attempts, last_error)
rule(id, account, priority, enabled, match_json, action_json)
signature(id, account, identity, name, html)
contact(id, account, display_name, email, vcard)
sync_state(account, folder, uidvalidity, last_uid, modseq)
```

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
`default` for the store key and `account:<id>:imap` for an IMAP credential.

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

**Load once.** `Engine::open` issues one query per table: accounts, folders, messages (with labels
in a second grouped query), attachments grouped by message and labels. There is no per-message read.
Before hydrating, it repairs every `drafts` row that has no message row into a real message row in
the Drafts folder of its account, in one transaction, using the body and recipients from the
revision row. If an account, folder or date cannot be repaired, the open fails with an actionable
error instead of serving a ghost draft. The label sequence is read from `meta.label_seq`, seeded
from `max(meta, existing label ids)` and written back when an older store lacks it; the next label
id is therefore never one that was already used.

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
app are still demo-only; the engine writes attachment bytes only through the P4b seam, and it writes
local drafts with `uid`, `Message-ID` and `size` empty. Received mail is different: `postino-sync`
(P5b) fills those columns, parses the MIME body and stores attachment bytes through the same seam.
The keyring lock protects first-run key creation, not the store file itself. There is no cross-process
locking story beyond SQLite's WAL, busy timeout and the keyring lock: one process owns the store
until the daemon (M4), and the desktop app does not open it yet. The repository returns folders and
labels ordered by name, so a store engine sees alphabetical order where the demo saw creation order;
P6b decides the real folder-tree order when onboarding writes it.

**Still in memory / demo:** the desktop app, the composer's view state, and any engine built without
a store. **Still missing:** the onboarding UI (P6b) and its `sync_now` binding, FTS5 (P9), outbox
(P7), the daemon (M4). The default store path, the account/sync operations and their CLI/MCP bindings
landed in P6a (§4.7).

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
public and gives the plain alternative when a message has only HTML.

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

**Command surface.** `CAPABILITY`, `AUTHENTICATE PLAIN` (preferred) or `LOGIN`, `LIST "" "*"`,
`SELECT`/`EXAMINE`, `UID FETCH ... (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)`,
`UID FETCH <uid> (BODY.PEEK[])`, `UID STORE` with `+FLAGS.SILENT`/`-FLAGS.SILENT`/`FLAGS.SILENT`,
`NOOP`, `IDLE` with a single `DONE` cycle, `LOGOUT`. Tags are `A0001`… and an unexpected tag is a
`Protocol` error. Bodies are never fetched with `BODY[]`: `BODY.PEEK[]` keeps reading from mutating
`\Seen`. Modified UTF-7 is decoded for display and the wire name is kept for subsequent commands.
A FETCH is checked back against the request: a body whose UID differs is refused, as is an envelope
whose UID is outside the requested set, so a confused server cannot hand over the wrong message.
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

**Not implemented.** Sync loops, store writes, engine operations, `APPEND`/`COPY`/`MOVE`/`EXPUNGE`,
QRESYNC/CONDSTORE beyond reading `HIGHESTMODSEQ` when the server volunteers it, SORT/THREAD,
METADATA, ACL, SIEVE, NOTIFY, ESEARCH, `BODYSTRUCTURE` and per-part fetches, OAuth/SASL XOAUTH2 and
any mechanism other than PLAIN, SMTP, autodiscovery and MIME parsing. The fake server and its TLS
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
calls the write for every ingested attachment; the reading pane (P6) will call the read to preview or
save.

**Cleanup.** There is no garbage collection yet: deleting a message leaves its blob file, because
another message may reference the same content; P9 or the daemon will sweep unreferenced hashes.
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

**Threading.** `thread_id` is the first `References` entry, else `In-Reply-To`, else the message's own
Message-ID (or the local id when it has none), all without angle brackets. That is a stable
placeholder that links a reply to a root; full JWZ threading is P9.

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
keeps its cached rows until P6b decides), full JWZ threading, FTS5 and any scheduled loop. P6a adds the
operations and the CLI/MCP bindings (§4.7); the UI binding is P6b, and there is still no scheduler.

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

The backend that turns Postino from a demo into a client for a real mailbox. There is no onboarding
screen yet (P6b), but the CLI, MCP and the operation registry are complete: a person can add an Aruba
or Gmail account with an app password, test it, sync it into the encrypted store, read the cached
mail and remove it. The desktop app still boots on the demo until P6b.

**Credential scopes.** `postino-platform` owns every keyring access under service `postino`:

| Scope | Secret | Used by |
|---|---|---|
| `default` | the 256-bit SQLCipher store key | `postino --store` / the default store |
| `account:<id>:imap` | one account's IMAP password or app password | `account_add`, `account_test`, `sync_now`, `account_remove` |

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
| `gmail` | `imap.gmail.com` | 993 | implicit | 2-Step Verification + app password; OAuth is P12 |
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
their rows but not the blob files themselves; the unreferenced-file sweep is still P9/daemon work.

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

## 5. Synchronization

IMAP is the synchronization protocol. There is no Postino server.

P5a ships the client (`postino-net`): one connection, TLS, the commands in §4.4, typed data out. P5b
ships the reconciler (`postino-sync`) described in §4.6: one account, one store connection, one run at
a time. P6a wires it to the engine: `sync_now` is the only operation that calls it, and the CLI and
MCP reach it through the same registry. It is still not scheduled: there is no loop and no daemon, so
a sync runs only when a caller asks for one.

The target loop, most of which does not exist yet:

- Discover settings with RFC 6186 SRV records, autoconfig, then a manual fallback.
- One connection per account for IDLE; separate short-lived connections for bulk fetch.
- `UIDVALIDITY` changes invalidate the folder cache. `CONDSTORE`/`QRESYNC` where supported; otherwise
  UID ranges and flag fetches (P5b implements the UID-range half, no QRESYNC).
- Fetch envelopes and flags first; bodies on demand, with a bounded prefetch of recent mail. P5b
  downloads each body during the reconciliation pass; on-demand reads are P6.
- Local intents (send, flag, move, draft save) are optimistic and queued in `outbox` with an
  idempotency key. On reconnect, the daemon replays them and reconciles. Not implemented.
- Sent mail is `APPEND`ed to the Sent folder, deduplicated by `Message-ID` when the provider also
  saves it. Not implemented.
- Drafts are appended with a stable client `Message-ID` and revision; IMAP has no atomic update, so
  two devices can briefly create two copies. This is a protocol limit, documented rather than
  hidden. Not implemented.
- Two devices converge through the server: IDLE pushes within seconds when the network and provider
  cooperate; otherwise polling (default 60 s) is the floor. Not implemented; P5b is a one-shot run
  with no scheduler.

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

- **M0-M4:** a `TextBodyRenderer` and a small, sanitized HTML subset (paragraphs, emphasis, links,
  lists, simple tables, images blocked). Complex mail has an "open in browser" action.
- **M5:** an optional `WebviewBodyRenderer` behind a feature flag. It uses the OS webview
  (WebView2 / WKWebView / WebKitGTK) in a dedicated, sandboxed surface: no JavaScript, no
  application IPC, a strict CSP, remote content blocked, and a custom protocol for local
  attachments. The rest of the app stays egui.
- The composer has two modes: **base** (block editor: paragraph, heading, list, quote, divider,
  image, button, signature) and **advanced** (HTML source with a constrained Tailwind subset,
  linting and live preview). Both compile to table-based, inline-styled, email-safe HTML with a
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
- MCP is stdio-only and local. The daemon pipe is same-user and token-authenticated.

The full threat model is in `docs/threat-model.md`.

## 8. Quality gates

`cargo xtask ci` runs formatting, clippy (`-D warnings`), tests, the layering check and the asset
check. The UI has a headless render test; the operation registry has a schema/name test; MCP has
protocol tests. Every future feature adds an end-to-end operation test.

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
