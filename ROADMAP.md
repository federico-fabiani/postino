# Postino roadmap

> **Current milestone:** M2 complete for SMTP/outbox and compose depth (P7/P8); M3 organize is
> complete for encrypted search (P9a), conversations/saved searches (P9b) and local rules (P9c).
> Attachment preview and HTML compose remain; the folder-thread view and the open/RSS halves of the
> 100k gate are still open · **Last updated:** 2026-10-27
>
> M1 is complete: the store, MIME, blobs, IMAP, sync, accounts and onboarding are real, and the
> desktop app reads and syncs a real mailbox from the encrypted cache. M2 has started with SMTP,
> the journaled outbox and independent Sent APPEND, and P8 closes compose depth. There is still no
> daemon: scheduled mail waits for the app or `postino send`, and an APPEND that keeps failing has no
> terminal give-up state (P9).

The full design is in [docs/architecture.md](docs/architecture.md). Each milestone ends with a
verifiable demo, not a promise.

## M0 — walking skeleton (done)

- [x] Cargo workspace, edition 2024, Rust 1.98+, MIT OR Apache-2.0.
- [x] `postino-core` with domain types, demo engine and typed operations.
- [x] JSON Schemas derived from Rust request types with `schemars`; unknown keys rejected.
- [x] `postino` CLI: `info`, `tools`, `schema`, `accounts`, `folders`, `messages`, `read`,
      `search`, `flag`, `run`, `mcp`.
- [x] Opt-in MCP server over stdio: `initialize`, `ping`, `tools/list`, `tools/call`, readable
      `isError` results, notifications ignored.
- [x] egui shell with forest/ivory/gold palette, folder rail, message table, reading pane, filters,
      read/starred toggles, command palette, dark theme, demo inbox.
- [x] Embedded Inter and JetBrains Mono with OFL attribution.
- [x] Headless UI render test, core operation tests, MCP protocol tests.
- [x] `cargo xtask ci`, layering check, asset hash check, docs, ADRs.
- [x] Verify: `cargo xtask ci` green; `postino --demo tools`; MCP `tools/list` and `tools/call`.

**Note:** on Windows GNU, `cargo test` needs `dlltool.exe` from MSYS2 MinGW on `PATH`. See
[development](docs/development.md).

## U0 — UI foundation (in progress, before M1)

The M0 shell proved the architecture but not the product. U0 builds the design system and the
interaction model before real mail makes them expensive to change. Full spec:
[docs/ui/plan.md](docs/ui/plan.md).

- [x] **U0.1 Foundations** — spacing/type/radius/control tokens, Phosphor icons, Button,
      IconButton, TextField, SearchField, Chip, Badge, ListRow, SectionHeader, EmptyState, Toast,
      Menu, Dialog, focus ring; live gallery (`postino-app --gallery`) and headless test.
- [x] **U0.2 Frame** — custom title bar on Windows/Linux, centred search, new toolbar hierarchy,
      sidebar/list/reading/status rebuilt from components, adaptive widths, 44/36 px rows.
      Screenshots: `docs/images/postino-u02.png`, `postino-u02-reading.png`, `postino-u02-full.png`,
      `postino-u02-dark.png`.
- [x] **U0.3 Interaction** — selection and multi-select, scroll-follow, full keyboard map, command
      table, palette with arrows/Enter/shortcuts, `?` sheet, context menus, batch bar, undo/toast,
      read-state policy, reading pane open/expand/Esc.
- [x] **U0.4 Compose** — composer component, draft operations, autosave, reply/forward prefill,
      local move to Sent, Drafts folder and counts. `draft_save` is in the registry; `draft_send`
      was the local placeholder and is replaced by `message_send` in P7. Attachments are
      metadata-only until M2. Screenshots: `docs/images/postino-u04.png`,
      `postino-u04-compose.png`, `postino-u04-compose-dark.png`.
- [x] **U0.5 Labels** — label registry with counts, create/rename/delete/apply/remove and filter in
      the engine, GUI, CLI and MCP; inline creation under the ETICHETTE header with a semantic colour
      row, a modal label menu with captured targets, reading-pane chips, and undo for rename, delete
      and apply/remove (creating a label is not an undo step). The reading pane is a resizable side
      pane at 1000 px and wider, and a full-width overlay below it. Labels stay local and in-memory
      until M1. Screenshots: `docs/images/postino-u05-labels.png`, `postino-u05-label-menu.png`.
- [x] **U0.5 Polish** — empty/loading/error states, reduced motion, accessibility pass, final
      light/dark screenshots and docs (P2a/P2b).
- [x] Verify: every component in the gallery in light/dark; full keyboard-only workflow; no dead
      actions; no ad-hoc literals in screens; `cargo xtask ci` green.

## M1 — read real mail

- [x] SQLCipher store, migrations, WAL, key in the OS keyring (P3a-P3c); the CLI and the desktop
      app both run on it, with `--demo` as the synthetic fallback.
- [x] MIME parsing, allowlist HTML sanitization, HTML-to-text and RFC 5322 serialization as the
      `postino-mime` crate (P4a): bounded input, warnings, header-injection guards, deterministic
      bytes, ten synthetic fixtures and a mutation corpus.
- [x] Encrypted attachment blob storage and the engine seam (P4b); received MIME is parsed by
      `postino-sync` (P5b). Local drafts still carry a plain-text body.
- [x] Accounts, credentials and sync operations (P6a): `accounts_list`, `account_add`,
      `account_test`, `account_remove`, `sync_now` and `sync_status`; OS-keyring credentials under
      `account:<id>:imap`, schema v4 `account_servers`, offline Aruba/Gmail autodiscovery, the
      credential-first add and cache-first remove order, and sensitive-operation filtering in MCP.
      The CLI and the desktop app can add, test, sync and remove an account.
- [x] Account onboarding UI and the app on real data (P6b): welcome/provider/credentials/folders/
      first-sync flow, Aruba and Gmail app-password guidance, recovery screen, background sync
      worker, account switcher and offline banner.
- [x] IMAP: CAPABILITY, LOGIN/AUTH, LIST, SELECT, UID FETCH envelope/flags/body, IDLE, as the
      `postino-net` client crate (P5a): TLS-only, bounded, typed errors, and a 54-case hostile corpus
      against the scripted TLS server now shared as `postino-testkit`.
- [x] Sync into the encrypted store as the `postino-sync` crate (P5b): folder discovery with
      special-use, UIDVALIDITY resets, incremental UID windows, MIME bodies, attachment blobs,
      deletion pass, flags on first insert with drift reporting, cancellable runs and a persistent
      status. 111 net tests plus the sync suite, a 2 000-message test in CI and 10 000 behind
      `POSTINO_SLOW_TESTS=1`. `sync_now`/`sync_status`, the CLI and the desktop worker call it.
- [x] Local cache and offline read: a synced mailbox opened through `Engine::open` serves bodies and
      attachment bytes with the network off (proved by the sync suite). The folder tree comes from
      the server's special-use attributes.
- [x] Wire sync to the operation registry, the CLI and the desktop worker (P6a/P6b).
- [x] Message list and reading pane fed by the store, not demo data.
- [x] Sync status and errors the user can act on: syncing, updated at, offline with the last sync, error with Retry.
- [x] Verify: 10 000-message sync in 36.5 s / 34.8 MB peak working set on the Windows GNU host,
      cancel mid-sync and resume without duplicates, read a message with the network off, and add an
      account through the desktop onboarding against the scripted server.

## M2 — write real mail

- [x] SMTP submission with STARTTLS/implicit TLS, AUTH, SMTPUTF8, DSN, SIZE and 8BITMIME (P7):
      `postino-net::smtp` with typed transient/permanent/uncertain errors, bounded lines/replies/
      messages, dot-stuffing and a fake server covering each stage.
- [x] Compose To/Cc/Bcc, subject, body, attachments and signatures per identity (P8): real attachment
      bytes through the encrypted blob store, a 20-file / 25 MiB-per-file / 25 MiB-total cap, filename
      sanitization and duplicate renaming, picker and drag-and-drop intake, a removable row with the
      Italian total, and a ≤ 4 KiB plain-text signature editor per account in Settings. `draft_save`
      stays metadata-only; `draft_attachment_add/remove` own the bytes.
- [x] Reply/reply-all/forward with correct recipients, subject prefix, deterministic UTC quote and
      `In-Reply-To`/`References` from the parent `Message-ID` (P8). `Reply-To` is honored, the account's
      own address is removed, Bcc is never copied, the signature is not duplicated in a reply, and
      forwarded attachments carry their bytes. Full thread derivation and the conversation view stay M3.
- [ ] Draft autosave with revisions, crash recovery, `APPEND` to Drafts. Autosave/revisions and crash
      recovery exist; the server-side Drafts APPEND is not implemented.
- [x] Outbox with a stable per-revision idempotency key, a `draft_id` column with a partial unique
      index over every active state, a `lease_at` epoch with guarded transitions, backoff, scheduled
      send, undo, cancel/discard and an uncertain state; `APPEND` to Sent with `\Seen`, its own claim
      and independent retries (P7). A duplicate `message_send` after a cancel is one row and one
      delivery; a revived row is the newest by `created_at`; a submission never leaves the machine if
      the draft changed after it was serialized (body, headers, last-saved instant and attachment set).
      A reply draft's `In-Reply-To`/`References` are serialized and compared with its hydrated link; a
      link written by a process this engine never loaded is a stale snapshot and is refused. The local
      Sent copy carries the generated `Message-ID`, and a sync adopts it instead of inserting a second
      copy, whether the local copy or the server copy was written first; in the server-first order the
      merge adopts the cached UID and skips the APPEND. A Sent copy moved back to Drafts is never
      adopted or deleted by a Sent sync.
- [x] Verify: crash between SMTP accept and Sent append does not send twice; a test seam fails after
      the server's `250` and before the durable transition, and reopening proves `sending` →
      `uncertain`, no connection and one DATA. The fake server proves the exact bytes/envelope across
      a second boundary, Bcc only in RCPT, restarts at every point, a two-process live lease, a claimed
      append a second pass leaves alone, a `mark_sent` that loses its race, a stale engine that cannot
      overwrite another process's move, 25 MiB + 10 recipients, SIZE refusal, backoff/persist/auth-halt,
      a damaged append envelope that is never `sent`, and that an uncertain row is never retried
      automatically. The CLI suite proves plain `postino send` submits while a future `--at` only
      queues.
- [x] Verify P8: exact attachment bytes survive blob → draft → reopen → queue → MIME (SHA-256 equal
      before and after), filename injection/traversal/reserved/bidi, duplicate renaming, the 20/25 MiB
      caps, base64 bounds, missing/corrupt blobs naming the file, forward bytes and missing-forward
      refusal, signature per account and 4 KiB/control limits, reply-all/Reply-To/Bcc/dedupe/subject/
      quote/headers matrices, CLI attach/detach and signature, MCP schemas with no path, picker/drop
      recorders, keyboard Enter and 980×600 light/dark renders of the composer and the editor.
- [ ] Verify end-to-end to a real test mailbox (needs a provider account and app password).

## M3 — organize

- [x] Threading (P9b): account-scoped root reference from `References`/`In-Reply-To`, one-level
      resolution with phantom roots, late-parent joins, order-independent merges into the smallest id
      and aliases, duplicate-Message-ID canonicalisation and the bounded subject fallback. Cached
      `threads` rows with mailbox-wide counts, `threads_list`/`thread_get`/`thread_flag`, the
      one-open-card conversation view with thread archive/delete in one undo, CLI/MCP parity and
      20k/100k evidence. 100k account-wide list P50 80.7 ms / P95 82.1 ms and
      100 real 3-reference inserts in 307 ms (P50 2.7 ms, P95 6.0 ms); the folder view missed the
      target (356-359 ms P95) and is recorded in the architecture table.
- [x] Saved searches (P9b): RICERCHE under ETICHETTE, ≤12 named queries with computed stale-aware
      counts, apply/replace/Esc-restore, recent queries in encrypted `meta`, rename/edit/delete with
      undo, CLI and MCP CRUD.
- [x] FTS5 search with operators and ranking (P9a): encrypted contentless-delete index, schema v7,
      the documented grammar with a recursive-descent parser and canonical printer, typed bound SQL
      with bm25/date order, plain snippets, 200-hit/5k-offset caps, `search_status`/`search_reindex`,
      CLI/MCP/UI parity and the body-free read path. 100k first-page `search` op P50 51.0 ms / P95 53.3 ms
      (store page 34–40 ms); open missed the 150 ms target (268–357 ms) and is recorded in the
      architecture table; RSS is unverified (154.7–156.5 MiB is the seeding process, not an opened
      mailbox).
- [x] Flags, move, archive, delete and undo; spam classification is out of scope for M3.
- [x] Local rules (P9c): a validated query plus 1-5 actions (move, label, read/unread, trash,
      forward), `priority ASC, id ASC` with `stop`, ≤100 rules and ≤5 actions per account. `new`
      runs after `sync_now` or on demand on received-origin mail ingested after the rule was created;
      a message is marked applied only when every action completed, so a failed/deferred action is
      retried. `all` reads up to 2 000 candidates per rule and keeps a per-rule cursor that advances
      through a larger backlog, retries a deferred page and wraps at the end. Actions apply
      immediately in rule order so later rules see earlier effects, repeats are no-ops, a failed
      target is journaled without stopping the later rules, and `sync_now` never fails because of a
      rule. The journal holds ids/folders/labels/booleans only, keeps 20 runs, and undo reverses
      move/label/read/trash only on messages still in the state the run left (including starred).
      A forward is one P7 outbox row keyed `rule:<rule>:<message>` with a durable dedupe marker
      independent of outbox pruning, carries `Auto-Submitted`/`X-Postino-Rule`, skips loop-guarded
      mail, and queues at most 50 new rows per run while deferring the rest. MCP gates `rules_run`,
      forward payloads, enabling a stored forward rule and the `sync_now` rule pass behind
      `--allow-send`. Rules page, preview, CLI/MCP parity, 20k/100k evidence and 980×600 light/dark
      captures.
- [ ] Attachment preview (Save with path confinement shipped in P6c).
- [ ] Verify: thread fixtures, rule idempotency, no path escape. The search, real-ingest and
      account-wide thread halves of the 100k gate are proven; the folder thread view and open/RSS
      halves remain open. P9c proves rule priority/stop, later-rule effects, idempotency, preview
      writes nothing, state-checked undo (including starred), forward dedupe across an outbox prune,
      loop guard, the 50-new-forward budget and the 51st retry, no-SMTP retry, failure isolation,
      archived-Sent exclusion, the new-vs-old horizon, two-page `all` progress, stale-rule updates,
      guard cleanup and 20k/100k rule-run numbers.

## M4 — live in the background

- [x] `postino-daemon` owns the store, sync and scheduler (P10a1).
- [x] JSON-RPC over a named pipe/UDS with a per-launch token; tray, autostart and notifications (P10b1).
- [x] CLI/MCP become clients; "daemon not running" is a normal state. App client connection (P10a2).
- [x] Scheduled send, undo send, automatic local rules while the app is closed, notifications (P10a1/P10b1).
- [x] Single-instance activation and crash recovery (P10b2/P10a1).
- [x] Verify: two clients (CLI + MCP) mutate one store; scheduled mail sends with the app closed;
      daemon restart loses no outbox entry; tray/autostart/notifications have recorder evidence.

## M5 — make it professional

- [x] `BodyRenderer` with the sanitized subset (P11a1/P11a2); optional sandboxed webview backend is P11b.
- [ ] Block composer and advanced HTML/Tailwind mode; live preview.
- [ ] Email-safe HTML compiler (tables + inline CSS), plain-text alternative.
- [ ] Templates, signatures, identities, remote-content policy, tracking-pixel stripping.
- [ ] Dark theme polish, accessibility pass, Italian/English catalogs.
- [ ] Verify: render a 20-newsletter corpus on Windows/macOS/Linux; sanitizer tests; no remote
      request in the default profile.

## M6 — ship and automate

- [x] Gmail OAuth with a bring-your-own Desktop client and XOAUTH2 (P12a).
- [ ] Gmail label mapping.
- [ ] Sieve/ManageSieve for server-side filters and vacation responder where supported (deferred:
      no confirmed provider; Gmail has none).
- [ ] PGP via an external agent/library; S/MIME later.
- [ ] `.ics` invites (read card + organizer-only reply draft done in P12d), unsubscribe, export/import `.eml`/`.mbox`, Thunderbird import.
- [ ] MCP compact mode, resources, send guardrails, audit log.
- [x] Windows per-user packaging source: MSI/ZIP, manifest, external signing (P12e).
- [ ] Signing/clean-VM verification and DMG/AppImage/deb/rpm.
- [ ] Security review against `docs/threat-model.md`; fuzz MIME/HTML; `cargo-deny`/`audit`.
- [ ] Verify: clean install on a fresh VM; agent sends a dry-run and a real message; offline
      restore from backup.

## Not planned

- A Postino sync server.
- Recipient-visible avatars without BIMI.
- Auto-reply or auto-forward while the daemon is not running.
- A web app as the primary product.
- Blue or violet as the brand accent.
