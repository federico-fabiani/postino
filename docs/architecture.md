# Postino architecture

> **Status:** M0 walking skeleton + U0 UI foundation + engine on the encrypted store (P3c) + MIME
> crate (P4a) · **Last updated:** 2026-10-25 · **Target:** a local-first, agent-first mail client
> that is fast on every desktop.

## 1. Shape today

One Cargo workspace, edition 2024, Rust 1.98+:

| Path | Role |
|---|---|
| `crates/core` | Domain types, the engine (demo or store-backed), the operation registry and its JSON Schemas |
| `crates/mime` | MIME parsing, HTML sanitization, HTML-to-text and RFC 5322 serialization (P4a) |
| `crates/platform` | OS seams; today the store key in the Windows Credential Manager |
| `crates/store` | SQLCipher schema v2, migrations and the repository; the engine's persistence layer since P3c |
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
| `postino-store` | SQLCipher schema, migrations, repository (P3b); FTS5 and encrypted blobs later |
| `postino-mime` | MIME parse/serialize, HTML-to-text, sanitization, header-injection guards (P4a; attachment blobs P4b) |
| `postino-net` | IMAP (IDLE, QRESYNC), SMTP submission, autodiscovery |
| `postino-sync` | reconciliation, outbox with idempotency keys, drafts, flags, threading |
| `postino-compose` | block model → email-safe HTML, CSS inlining, signatures, templates |
| `postino-rules` | local rules and Sieve generation |
| `postino-ipc` | JSON-RPC over pipe/socket, token, versioning |
| `postino-platform` | keyring (P3b); paths, notifications, tray, autostart, single instance later |
| `postino-testkit` | fake IMAP/SMTP, MIME fixtures, golden messages |

Layers: foundation (platform, store, mime, crypto) → network/sync → features (compose, rules) →
engine → frontends. `postino-core` stays as the façade and operation registry. Nothing below the
frontends depends on egui.

## 4. Data model

```
account(id, email, display_name, provider)
folder(id, account, name, kind, uidvalidity, uidnext, highest_modseq)
message(id, account, folder, uid, message_id, thread_id, from, to, cc, bcc,
        subject, date, size, flags, blob_hash, auth_results)
thread(id, account, subject_norm, last_date, unread, message_count)
attachment(id, message, filename, mime, size, blob_hash)
draft(id, account, revision, updated_at, blob_hash, in_reply_to)
outbox(id, account, idempotency_key, created_at, send_at, attempts, last_error)
rule(id, account, priority, enabled, match_json, action_json)
signature(id, account, identity, name, html)
contact(id, account, display_name, email, vcard)
sync_state(account, folder, uidvalidity, last_uid, modseq)
```

- The **server is the source of truth**. The local database is a cache plus an operation log.
- Bodies are content-addressed blobs (`blake3`), zstd-compressed and encrypted with
  XChaCha20-Poly1305. Large attachments live outside the DB.
- FTS5 indexes subject, people and extracted body text. Tokenization is Unicode-aware.
- Threading uses `References`/`In-Reply-To` with a JWZ-style fallback, plus provider thread ids
  when available.
- Nothing is deleted from the server unless the user asks. "Trash" is a local view and an IMAP
  move; expunge is explicit.

### 4.1 Implemented in P3b: schema v2, keyring and repository

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
failed RNG is an error, never a derived key or a plaintext file. `StoreKey` is redacted in `Debug`,
has no `Clone` and zeroises its bytes on drop; the SQLCipher pragma is built in a zeroising string.
SQLCipher's internal copy of the key is the remaining gap, recorded in `docs/store-spike.md`. The
store crate never reads the keyring, which is what lets its tests run with a random in-memory key.
Tests for the platform crate install `keyring_core::mock` and never touch the real vault.

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

Folder `unread`/`total` are a cache refreshed from `messages` by mutations and repairable with
`recount_folder`. The list query returns `total`/`unread` for the filtered scope before pagination
and clamps pages to 1..=500 rows.

### 4.2 Implemented in P3c: the engine on the store

`Engine` holds `store: Option<postino_store::Store>`. `demo()` and `demo_at()` leave it `None` and
behave exactly as before; `open(store)` hydrates the mailbox and keeps the connection for
write-through. The store crate does not depend on core: `crates/core/src/store_map.rs` is the only
translation between domain types and store rows.

**Load once.** `Engine::open` issues one query per table: accounts, folders, messages (with labels
in a second grouped query), attachments grouped by message, labels and drafts. There is no
per-message read. A draft that exists only as a `drafts` row is materialised into its Drafts folder,
so the revision log can never hide mail.

**Write through.** Every mutating operation writes the repository first and only then updates
memory; a failed write is an `OpError::Failed` with the store's reason and an instruction, and
memory is untouched. Wired in P3c: `save_draft` (message row, attachment rows, revision row),
`send_draft` (move to Sent plus delete the draft row), `set_flags`, `move_message`, label
create/update/delete and label apply/remove. Reads (`message_get`, `messages_list`, `search`,
`labels_list`) never mutate. Account and folder writes remain a P6 seam: onboarding will call
`upsert_account`/`upsert_folder` before the first engine load.

**Counts.** The store keeps `folders.total`/`unread` current inside its own transactions; the engine
recomputes its in-memory counts after each mutation instead of re-reading the store. The virtual
`starred` folder is derived from message flags and is not a meaningful row in the store's folder
cache; the engine is authoritative for it.

**Residual windows.** Each repository call is individually transactional, but `save_draft` composes
three and `send_draft` two. A crash or disk failure between them can leave the store one step ahead
of memory; the next save repairs the draft, and the message row is always written before the
revision row. Attachment bytes, IMAP `uid`/`Message-ID` and `size` are not written by the engine yet
(P4/P5). There is no cross-process locking story beyond SQLite's WAL and busy timeout: one process
owns the store until the daemon (M4), and the desktop app does not open it yet. The repository
returns folders and labels ordered by name, so a store engine sees alphabetical order where the demo
saw creation order; P6 decides the real folder-tree order when onboarding writes it.

**Still in memory / demo:** the desktop app, the composer's view state, and any engine built without
a store. **Still missing:** onboarding and the default store path (P6), IMAP writers for
`sync_state` (P5), MIME wiring into the engine and encrypted attachment blobs (P4b), FTS5 (P9),
outbox (P7), the daemon (M4).

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

**Next.** P4b stores the bounded attachment bytes as encrypted blobs and maps `ParsedMessage` onto
the core `Address`/`Attachment` shapes; P5 supplies the IMAP bytes and P7 uses `serialize` for
submission. The crate has no dependency on `postino-core`, so the mapping stays in the engine
layer.

## 5. Synchronization

IMAP is the synchronization protocol. There is no Postino server.

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

- TLS only; certificate validation cannot be disabled from the UI.
- Credentials in the OS keyring; OAuth2 PKCE with a loopback redirect for providers that require it.
- Store encrypted at rest; the key is generated on first run and kept in the keyring.
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
| 2026-10-25 | major | P4a: `postino-mime` parses, sanitizes and serializes MIME with bounds, warnings and injection guards |
