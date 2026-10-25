# Postino roadmap

> **Current milestone:** U0 — UI foundation (M0 skeleton done) · **Last updated:** 2026-10-25

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
      local move to Sent, Drafts folder and counts. `draft_save` and `draft_send` are in the
      CLI/MCP registry; attachments are metadata-only until M2. Screenshots: `docs/images/postino-u04.png`,
      `postino-u04-compose.png`, `postino-u04-compose-dark.png`.
- [x] **U0.5 Labels** — label registry with counts, create/rename/delete/apply/remove and filter in
      the engine, GUI, CLI and MCP; inline creation under the ETICHETTE header with a semantic colour
      row, a modal label menu with captured targets, reading-pane chips, and undo for rename, delete
      and apply/remove (creating a label is not an undo step). The reading pane is a resizable side
      pane at 1000 px and wider, and a full-width overlay below it. Labels stay local and in-memory
      until M1. Screenshots: `docs/images/postino-u05-labels.png`, `postino-u05-label-menu.png`.
- [ ] **U0.5 Polish** — empty/loading/error states, reduced motion, accessibility pass, final
      light/dark screenshots and docs.
- [ ] Verify: every component in the gallery in light/dark; full keyboard-only workflow; no dead
      actions; no ad-hoc literals in screens; `cargo xtask ci` green.

## M1 — read real mail

- [x] SQLCipher store, migrations, WAL, key in the OS keyring (P3a-P3c; the CLI runs on it, the app
      still boots on demo until P6).
- [x] MIME parsing, allowlist HTML sanitization, HTML-to-text and RFC 5322 serialization as the
      `postino-mime` crate (P4a): bounded input, warnings, header-injection guards, deterministic
      bytes, ten synthetic fixtures and a mutation corpus.
- [ ] Attachment blob storage and MIME wiring into the engine/store (P4b).
- [ ] Account onboarding with autodiscovery and Aruba app-password flow.
- [x] IMAP: CAPABILITY, LOGIN/AUTH, LIST, SELECT, UID FETCH envelope/flags/body, IDLE, as the
      `postino-net` client crate (P5a): TLS-only, bounded, typed errors, scripted TLS server and a
      53-case hostile corpus. The sync loop, cache and offline mode are still open.
- [ ] Local cache and offline mode; folder tree from special-use attributes.
- [ ] Message list and reading pane fed by the store, not demo data.
- [ ] Sync status and errors the user can act on.
- [ ] Verify: 10,000-message sync under a time/memory budget; kill mid-sync and resume without
      duplicates; read a message with the network off.

## M2 — write real mail

- [ ] SMTP submission with STARTTLS/implicit TLS, AUTH, SMTPUTF8, DSN.
- [ ] Compose To/Cc/Bcc, subject, body, attachments, signatures per identity.
- [ ] Reply/reply-all/forward with correct recipients and quoting.
- [ ] Draft autosave with revisions, crash recovery, `APPEND` to Drafts.
- [ ] Outbox with idempotency keys; `APPEND` to Sent; duplicate detection by `Message-ID`.
- [ ] Verify: end-to-end to a test mailbox; RFC 822 golden files; header injection rejected;
      crash between SMTP accept and Sent append does not send twice.

## M3 — organize

- [ ] Threading (References/In-Reply-To, JWZ fallback), thread view.
- [ ] FTS5 search with operators and ranking.
- [ ] Flags, move, archive, delete, spam, undo.
- [ ] Tags/labels, local rules (move, tag, read, delete, forward), rule preview.
- [ ] Attachment preview/save with path confinement.
- [ ] Verify: thread fixtures, 100k-message search under 100 ms for the first page, rule
      idempotency, no path escape.

## M4 — live in the background

- [ ] `postino-daemon` owns the store, sync and scheduler.
- [ ] JSON-RPC over a named pipe/UDS with a per-launch token; autostart and tray.
- [ ] App/CLI/MCP become clients; "daemon not running" is a normal state.
- [ ] Scheduled send, undo send, local rules, notifications.
- [ ] Single-instance and crash recovery.
- [ ] Verify: two clients (app + CLI) mutate one store; scheduled mail sends with the app closed;
      daemon restart loses no outbox entry.

## M5 — make it professional

- [ ] `BodyRenderer` with the sanitized subset; optional sandboxed webview backend.
- [ ] Block composer and advanced HTML/Tailwind mode; live preview.
- [ ] Email-safe HTML compiler (tables + inline CSS), plain-text alternative.
- [ ] Templates, signatures, identities, remote-content policy, tracking-pixel stripping.
- [ ] Dark theme polish, accessibility pass, Italian/English catalogs.
- [ ] Verify: render a 20-newsletter corpus on Windows/macOS/Linux; sanitizer tests; no remote
      request in the default profile.

## M6 — ship and automate

- [ ] Gmail OAuth (bring-your-own client id or verified app), labels mapping.
- [ ] Sieve/ManageSieve for server-side filters and vacation responder where supported.
- [ ] PGP via an external agent/library; S/MIME later.
- [ ] `.ics` invites, unsubscribe, export/import `.eml`/`.mbox`, Thunderbird import.
- [ ] MCP compact mode, resources, send guardrails, audit log.
- [ ] Packaging: MSI, DMG, AppImage/deb/rpm, signed releases.
- [ ] Security review against `docs/threat-model.md`; fuzz MIME/HTML; `cargo-deny`/`audit`.
- [ ] Verify: clean install on a fresh VM; agent sends a dry-run and a real message; offline
      restore from backup.

## Not planned

- A Postino sync server.
- Recipient-visible avatars without BIMI.
- Auto-reply or auto-forward while the daemon is not running.
- A web app as the primary product.
- Blue or violet as the brand accent.
