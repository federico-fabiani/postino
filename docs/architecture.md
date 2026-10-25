# Postino architecture

> **Status:** M0 walking skeleton + U0 UI foundation + engine on the encrypted store (P3c/P3d) + MIME
> crate (P4a) + encrypted attachment blobs (P4b) + IMAP client (P5a) · **Last updated:** 2026-10-25 ·
> **Target:** a local-first, agent-first mail client that is fast on every desktop.

## 1. Shape today

One Cargo workspace, edition 2024, Rust 1.98+:

| Path | Role |
|---|---|
| `crates/core` | Domain types, the engine (demo or store-backed), the operation registry and its JSON Schemas |
| `crates/crypto` | HKDF-SHA256 subkeys and XChaCha20-Poly1305 sealing; the blob subkey (P4b) |
| `crates/mime` | MIME parsing, HTML sanitization, HTML-to-text and RFC 5322 serialization (P4a) |
| `crates/platform` | OS seams; today the store key in the Windows Credential Manager |
| `crates/store` | SQLCipher schema v3, migrations, the repository and the encrypted blob store (P3b, P4b) |
| `apps/postino-app` | egui/eframe desktop shell, design tokens, banded three-pane layout |
| `apps/postino-cli` | `postino` command line and the opt-in MCP server |
| `xtask` | `ci`, `layers`, `assets` |

The engine has two sources of truth, chosen at construction:

- `Engine::demo()` / `demo_at()` — synthetic mail in memory, no I/O. This is what the desktop app
  still boots on until onboarding (P6).
- `Engine::open(store)` — the encrypted SQLCipher store, loaded once and written through on every
  mutation. The CLI reaches it with `--store <path>`; the key comes from the OS keyring.

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
| `postino-sync` | reconciliation, outbox with idempotency keys, drafts, flags, threading |
| `postino-compose` | block model → email-safe HTML, CSS inlining, signatures, templates |
| `postino-rules` | local rules and Sieve generation |
| `postino-ipc` | JSON-RPC over pipe/socket, token, versioning |
| `postino-platform` | keyring (P3b); paths, notifications, tray, autostart, single instance later |
| `postino-testkit` | fake IMAP/SMTP, MIME fixtures, golden messages |

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
attachment replacement/listing; drafts save/get/list/delete with store-owned revisions; meta.
Every multi-row mutation is one transaction, and a bad id rolls the batch back. Message upserts
leave labels and attachments alone unless the caller replaces them, and a message cannot be placed
in another account's folder (`AccountMismatch`).

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
`labels_list`) never mutate. Account and folder writes remain a P6 seam: onboarding will call
`upsert_account`/`upsert_folder` before the first engine load.

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
app are still demo-only; the engine writes attachment bytes only through the P4b seam (sync does not
call it yet) and IMAP `uid`/`Message-ID` and `size` are not written by the engine yet (P5), and the
keyring lock protects first-run key creation, not the store file itself. There is no cross-process
locking story beyond SQLite's WAL, busy timeout and the keyring lock: one process owns the store
until the daemon (M4), and the desktop app does not open it yet. The repository returns folders and
labels ordered by name, so a store engine sees alphabetical order where the demo saw creation order;
P6 decides the real folder-tree order when onboarding writes it.

**Still in memory / demo:** the desktop app, the composer's view state, and any engine built without
a store. **Still missing:** onboarding and the default store path (P6), IMAP writers for
`sync_state` (P5), MIME wiring into the engine and the sync call that feeds the P4b attachment seam
(P5b), FTS5 (P9), outbox (P7), the daemon (M4).

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
mapping `ParsedMessage` onto the core `Address`/`Attachment` shapes and feeding those bytes is P5b.
P5 supplies the IMAP bytes and P7 uses `serialize` for submission. The crate has no dependency on
`postino-core`, so the mapping stays in the engine layer.

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

**Bounds.** A response line is at most 64 KiB, a literal 25 MiB by default (configurable, hard
ceiling 64 MiB), untagged responses per command 10 000, folders per `LIST` 5 000, a command line
8 KiB, and one command may consume at most 64 MiB of response bytes (hard ceiling 128 MiB). Fixed
parser limits cap nesting at 32 levels, literals per response at 4 096, atoms and mailbox names at
1 KiB, and numbers at 20 digits; a number is parsed, never wrapped. Exceeding a bound is
`TooLarge` or `Protocol`, never a hang or an unbounded allocation. Every phase has a timeout
(DNS, connect, TLS handshake, greeting, each command, `DONE`); a timeout closes the session and the
next call fails as `Closed` instead of reading a half-command.

**Security.** The password is a `Secret` that zeroises on drop and prints `[redacted]`, and it is
dropped after a successful login. The SASL payload and the `LOGIN` line are built in `Zeroizing`
buffers. Logs carry the tag and the command verb, never arguments, addresses, bodies or
credentials. Server text in errors is cut to printable ASCII.

**Not implemented.** Sync loops, store writes, engine operations, `APPEND`/`COPY`/`MOVE`/`EXPUNGE`,
QRESYNC/CONDSTORE beyond reading `HIGHESTMODSEQ` when the server volunteers it, SORT/THREAD,
METADATA, ACL, SIEVE, NOTIFY, ESEARCH, `BODYSTRUCTURE` and per-part fetches, OAuth/SASL XOAUTH2 and
any mechanism other than PLAIN, SMTP, autodiscovery and MIME parsing. The fake server lives in
`crates/net/tests/support/`; it is promoted to `postino-testkit` only when SMTP needs it.

### 4.5 Implemented in P4b: encrypted attachment blobs

Attachment bytes are the largest and most sensitive payload in a mailbox and they are write-once.
They do not belong in SQLCipher pages: `postino-store` keeps one file per distinct content next to
the database and seals it with a subkey derived from the same store key.

```
D:\mail\mail.db                      SQLCipher: metadata, subjects, bodies, rows
D:\mail\blobs\0a\0a3f…c9.blob        XChaCha20-Poly1305: one attachment content, once
```

**Layout.** `Store::open` creates and opens `<db parent>/blobs` after the schema is readable and
before the engine sees the store; a directory that cannot be created fails the open instead of
losing attachments later. Every blob is `blobs/<first two hex chars>/<hash>.blob`, where the hash is
the lowercase BLAKE3 hex of the **plaintext**. One directory is shared by every store in the same
folder, which is what lets dedup work across accounts and keeps a folder backup self-contained. The
hash is validated as exactly 64 lowercase hex characters before any path is assembled, so `..`,
separators, uppercase hex and short/long values are `BadHash` and never reach the filesystem.
A relative database path resolves to `./blobs`. Two databases with **different** keys in one folder
therefore share a directory neither can fully read (the other's blobs are `CorruptBlob`): keep one
key per folder.

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

**Keys.** The blob key is `HKDF-SHA256(salt = "postino.v1", ikm = store key, info = "blob")`, held
in `postino_crypto::Subkey`: no `Clone`, redacted `Debug`, zeroised on drop. A nonce is 24 random
bytes from the OS RNG for every seal. A different store key, a different label or a different AAD is
`CorruptBlob`; the error never says which, and it never contains plaintext or key bytes. Key
rotation does not exist yet: rotating the store key makes every existing blob unreadable, and that
is recorded in `docs/threat-model.md`.

**Writes.** A put writes `.tmp-<random>` in the target shard with `create_new`, `sync_all`s it and
renames it over `<hash>.blob`. A failed write removes its temp. A rename that loses a race against
an identical put is a success because the name is the content; eight concurrent puts of the same
bytes leave one file and no temp. Identical bytes are stored once (dedup), and a put of existing
bytes writes nothing.

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
attachment row's `blob_hash` and exact `size`, then the in-memory size and the engine's hash map. A
failed row write leaves the row and the message untouched; the blob file may remain as an
unreferenced, deduplicated file. `has_attachments` never changes on this path because the row
already exists. The demo engine and a store-less engine answer with a readable `OpError::Failed`.
Consumers: sync (P5b) calls the write for every ingested attachment; the reading pane (P6) calls the
read to preview or save.

**Recovery.** `put` deduplicates on the file's existence, so a damaged file has to be removed with
`BlobStore::remove(hash)` before the same content can be written again. The engine's read path never
mutates. There is no garbage collection yet: deleting a message leaves its blob file, because
another message may reference the same content; P9 or the daemon will sweep unreferenced hashes.

## 5. Synchronization

IMAP is the synchronization protocol. There is no Postino server.

P5a ships the client (`postino-net`): one connection, TLS, the commands in §4.4, typed data out.
P5b adds everything below: the loop, the store writes, reconciliation and the scheduler binding.

- Discover settings with RFC 6186 SRV records, autoconfig, then a manual fallback.
- One connection per account for IDLE; separate short-lived connections for bulk fetch.
- `UIDVALIDITY` changes invalidate the folder cache. `CONDSTORE`/`QRESYNC` where supported;
  otherwise UID ranges and flag fetches.
- Fetch envelopes and flags first; bodies on demand, with a bounded prefetch of recent mail.
- Local intents (send, flag, move, draft save) are optimistic and queued in `outbox` with an
  idempotency key. On reconnect, the daemon replays them and reconciles.
- Sent mail is `APPEND`ed to the Sent folder, deduplicated by `Message-ID` when the provider also
  saves it.
- Drafts are appended with a stable client `Message-ID` and revision; IMAP has no atomic update, so
  two devices can briefly create two copies. This is a protocol limit, documented rather than
  hidden.
- Two devices converge through the server: IDLE pushes within seconds when the network and
  provider cooperate; otherwise polling (default 60 s) is the floor.

ADR-0003 records the decision and the rejected alternatives (POP3, a custom sync service, Gmail
API as the primary protocol).

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
| 2026-10-25 | major | P5a: `postino-net` speaks IMAP4rev1 over implicit TLS or STARTTLS with OS verification, bounded lines/literals/commands, typed errors and a scripted hostile-server corpus; the sync loop and store writes are still missing |
