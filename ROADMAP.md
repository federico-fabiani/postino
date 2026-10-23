# Postino roadmap

> **Current milestone:** M0 — walking skeleton · **Last updated:** 2026-10-23

The full design is in [docs/architecture.md](docs/architecture.md). Each milestone ends with a
verifiable demo, not a promise.

## M0 — walking skeleton (done)

- [x] Cargo workspace, edition 2024, Rust 1.95+, MIT OR Apache-2.0.
- [x] `postino-core` with domain types, demo engine and seven typed operations.
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

## U0 — UI foundation (next, before M1)

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
- [ ] **U0.3 Interaction** — selection and multi-select, scroll-follow, full keyboard map, command
      table, palette with arrows/Enter/shortcuts, `?` sheet, context menus, batch bar, undo/toast,
      read-state policy, reading pane open/expand/Esc.
- [ ] **U0.4 Compose** — composer component, draft operations, autosave, reply/forward prefill,
      demo send, Drafts folder.
- [ ] **U0.5 Labels and polish** — label create/apply/filter, empty/loading/error states, reduced
      motion, accessibility pass, final screenshots and docs.
- [ ] Verify: every component in the gallery in light/dark; full keyboard-only workflow; no dead
      actions; no ad-hoc literals in screens; `cargo xtask ci` green.

## M1 — read real mail

- [ ] SQLCipher store, migrations, WAL, key in the OS keyring.
- [ ] MIME parser/serializer, sanitized HTML, plain-text alternative, attachment storage.
- [ ] Account onboarding with autodiscovery and Aruba app-password flow.
- [ ] IMAP: CAPABILITY, LOGIN/AUTH, LIST, SELECT, UID FETCH envelope/flags/body, IDLE.
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
