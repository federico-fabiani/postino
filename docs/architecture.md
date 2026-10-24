# Postino architecture

> **Status:** M0 walking skeleton + U0 UI foundation · **Last updated:** 2026-10-24 · **Target:** a
> local-first, agent-first mail client that is fast on every desktop.

## 1. Shape today

One Cargo workspace, edition 2024, Rust 1.95+:

| Path | Role |
|---|---|
| `crates/core` | Domain types, the in-memory demo engine, the operation registry and its JSON Schemas |
| `apps/postino-app` | egui/eframe desktop shell, design tokens, banded three-pane layout |
| `apps/postino-cli` | `postino` command line and the opt-in MCP server |
| `xtask` | `ci`, `layers`, `assets` |

M0 has **no I/O**. `Engine::demo()` builds a synthetic account with folders, threads, bodies and
attachments. The UI, CLI and MCP all read it through the same `Engine::call(name, args)` door.
Nothing is persisted, nothing is fetched, nothing is sent.

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
in M2; neither opens a network connection. Labels live in the demo engine's memory, count their
messages on read and are not synced anywhere until the M1 store.

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
| `postino-store` | SQLCipher schema, migrations, FTS5, content-addressed encrypted blobs |
| `postino-mime` | MIME parse/serialize, HTML-to-text, sanitization, header-injection guards |
| `postino-net` | IMAP (IDLE, QRESYNC), SMTP submission, autodiscovery |
| `postino-sync` | reconciliation, outbox with idempotency keys, drafts, flags, threading |
| `postino-compose` | block model → email-safe HTML, CSS inlining, signatures, templates |
| `postino-rules` | local rules and Sieve generation |
| `postino-ipc` | JSON-RPC over pipe/socket, token, versioning |
| `postino-platform` | paths, keyring, notifications, tray, autostart, single instance |
| `postino-testkit` | fake IMAP/SMTP, MIME fixtures, golden messages |

Layers: foundation (store, mime, crypto) → network/sync → features (compose, rules) → engine →
frontends. `postino-core` stays as the façade and operation registry. Nothing below the frontends
depends on egui.

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
