# AGENTS.md — rules for every agent and contributor

These rules bind every AI agent and human working in this repository. They come before any task.
If a rule here conflicts with a habit from another project, this file wins.

## 1. What Postino is

Postino is a local-first, agent-first mail client written in Rust. The desktop app, the `postino`
CLI and the `postino mcp` server are three clients of **one operation registry**. The engine runs on
synthetic demo mail in memory or on the encrypted local store (`postino --store <path>`); accounts,
credentials, IMAP sync, SMTP submission with the outbox and the desktop onboarding exist (P6/P7). The
daemon owns the store, the scheduler and the JSON-RPC IPC (P10a1); the desktop app is a daemon client
with reconnect and preserved view state (P10a2); the tray, autostart and notifications shipped in
P10b1, and single-instance activation and crash recovery in P10b2. The app boots on the encrypted
store with `--demo` as the synthetic fallback.

**The current milestone is the only source of truth for scope.** Read `ROADMAP.md` before
starting. Do not build a later milestone's features into the current one.

## 2. Non-negotiables

1. **Engine-first, headless-first.** Every user-visible feature must be reachable without the GUI:
   an engine operation, then a CLI path, then an MCP tool, then an end-to-end test. A feature is
   not done until its operation exists and has a test.
2. **One registry.** UI, CLI and MCP dispatch the same operation names and JSON Schemas. Never
   hand-write a second schema or a parallel command table.
3. **Rust core.** Network, storage, MIME, sync, rules and security are Rust. The egui shell is
   Rust. HTML email rendering stays behind a trait; a webview backend is a deliberate, sandboxed
   exception, never a dependency of the core.
4. **Never crash.** Mail, attachments and MCP calls are untrusted input.
   - Non-test code returns `Result` and propagates with `?`.
   - No `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!` or `unimplemented!` outside tests,
     unless the call is provably infallible and a comment says why.
   - No `unsafe` (`unsafe_code = "forbid"`).
   - No indexing or slicing with input-derived positions; use `get()`.
   - Bound recursion, allocations, decompression and attachment sizes.
   - Keep a last-resort panic guard around work that touches untrusted data.
5. **Privacy first.** No telemetry, no analytics, no cloud account, no remote AI call by default.
   The only network destinations are the user's own IMAP/SMTP/CalDAV servers. Logs must not contain
   message bodies, credentials or full addresses unless the user explicitly asks.
6. **Secrets never touch disk in plaintext.** OS keyring only. The future store is encrypted at
   rest; the future key lives in the keyring.
7. **MCP is opt-in.** It runs only when the user launches `postino mcp`, speaks stdio only, opens
   no port and stops at EOF. Tool failures are readable results, not crashes. Do not register the
   server with an agent or start it from an installer.
8. **HTML is hostile.** Sanitize on receipt, block remote content by default, never execute
   JavaScript, and render untrusted mail in a sandbox with no application IPC.
9. **Honest capabilities.** Do not claim a provider feature the client cannot deliver. IMAP sync,
   server-side filters, auto-reply and recipient-visible avatars have real limits; document them.
10. **Assets are attributed.** Every font, icon, image and logo has an entry in
    `ATTRIBUTION.toml` with licence and SHA-256. Only openly licensed or contributor-original
    assets. `cargo xtask assets` must pass.

## 3. Layering

The current workspace is deliberately small:

```
apps/postino-app   egui shell                     L3
apps/postino-cli   CLI + MCP server               L3
crates/core        domain + engine + operations   L2
crates/net         IMAP client (TLS, bounds)      L1.5 network
crates/store       encrypted store, repository    L1
crates/crypto      HKDF subkeys, XChaCha20-Poly1305 L1
crates/platform    keyring seam                   L1
crates/mime        MIME, HTML sanitizer, RFC 5322 L1
crates/ipc         JSON-RPC over pipe/socket      foundation/transport
apps/postino-daemon one writer + scheduler + IPC   L3
xtask              build/quality tasks            tooling
```

Rules enforced by `cargo xtask layers`:

- `postino-core` may depend only on `postino-store`, `postino-net` and `postino-sync` (the
  foundations it drives) and never on a UI toolkit (`egui`, `eframe`, `winit`, `rfd`). The store
  never depends back on core; sync may use core only as a dev-dependency for its offline-read test.
- `postino-crypto`, `postino-mime`, `postino-platform` and `postino-store` are foundation: no
  workspace dependency and no UI. `postino-net` is network: it depends on no other workspace crate.
  `postino-ipc` is foundation/transport: no workspace crate and no UI. `postino-daemon` is L3: it may
  use core, ipc, net, platform, store and the testkit only as a dev-dependency, never egui.
- The credential seam is explicit: core defines `CredentialVault` and never calls a keyring crate;
  `postino-platform` owns the OS keyring and the `account:<id>:imap` / `account:<id>:smtp` scopes; the
  CLI adapts one to the other. Do not move a keyring call into core or a password into the store.
- `postino-app` and `postino-cli` may depend on `postino-core` and on libraries, never on each other.
  The CLI may use `postino-net` only in tests (the fake server types).
- New crates need a layer and a README before they need code. Planned crates are listed in
  `docs/architecture.md`; do not create speculative stubs.

## 4. Operations

- Names are `snake_case` (`messages_list`): MCP clients reject dots.
- Every request struct uses `#[serde(deny_unknown_fields)]`. Typos fail loudly.
- Every operation has a precise title, a description an agent can act on, a JSON Schema derived
  from the request type, and `read_only` / `destructive` annotations.
- An operation that handles a credential is `sensitive`: the MCP server neither lists nor dispatches
  it, and the CLI reads the secret from a no-echo prompt or stdin, never from argv. The CLI's generic
  `postino run` refuses sensitive operations for the same reason.
- Reads never mutate. Opening a message must not silently mark it read; the UI or CLI calls
  `message_flag` explicitly.
- Errors say what was wrong and what is allowed. Do not leak whether an unrelated path or account
  exists when a confinement boundary is in force.

## 5. UI rules

The UI foundation is specified in `docs/ui/plan.md`. It is not optional decoration: it is the
contract that keeps the app from becoming a pile of one-off patches.

- The palette is the product: forest `#173D32`, ivory `#F5EBD6`, warm gold `#E4B96A`.
- Gold on ivory is decorative only (1.55:1). Body text is forest on ivory (10.1:1) or ivory on
  forest. Never use gold for body text on a light surface. Use `GOLD_INK` for a gold star or link
  that must carry meaning on ivory.
- Inter for interface text, JetBrains Mono for timestamps, identifiers and metadata.
- **Screens compose components; they do not hand-roll them.** Spacing, type, radii, control heights
  and icon sizes come from tokens. If a screen needs a new value, add it to the tokens first.
- **State belongs to the surface that owns it.** A screen's transient state — open sheets, caches,
  pending confirmations, focus requests — is a struct defined in that screen's module and stored as
  one field on `PostinoApp` (`reading: ReadingUi` in `app/reading_pane.rs`, `composer:
  ComposerState`). Add a field to that struct, not to `PostinoApp`. A new screen starts by defining
  its state struct; it does not add loose fields to the app. Global state (engine, store, daemon
  link, theme, selection, filters) stays on `PostinoApp`. Never copy a value another surface already
  owns: pass it in, or move it deliberately, once.
- **One command table.** Every UI action has an id, label, icon, shortcut, enabled rule and handler
  in the UI command table. Palette, menus, context menus, tooltips and buttons read it. Never
  maintain a second list in the palette.
- The app must work without a mouse: selection, multi-select, open, archive, star, label, compose,
  send, undo. `Ctrl+K` opens the command palette, `J`/`K` move, `Enter` opens, `Esc` goes back,
  `?` shows the shortcut sheet.
- Text inside list rows is not selectable; the row is the click target. The reading pane is where
  text is selected.
- No dead buttons. An action either works or is visibly unavailable with a reason.
- Every new screen needs a headless render test. `PostinoApp::show` exists so a test can render the
  real shell without a window. The UI gallery (`postino-app --gallery`) is the component review
  surface.
- Do not introduce blue or violet as a primary accent. They are reserved for semantic states only
  when a warm alternative would be ambiguous.

## 6. Code and tests

- `cargo fmt --all` before every commit; clippy with `-D warnings`.
- Scope commands to the crates you touched while iterating. Run the workspace sweep before a
  commit.
- Test at four zooms: property/unit, crate integration, end-to-end operation, and a headless UI
  render when the UI changed.
- A crash fix gets a synthetic regression test that failed before the fix.
- Never commit secrets, real mail, real addresses or personal data. Demo data uses
  `example.com` / `example.it`.
- No handwritten JavaScript or TypeScript in M0. If a webview renderer is added in M5, it is a
  sandboxed viewer, not a second application.

## 7. Windows development note

The active Rust host is `x86_64-pc-windows-gnu`. `cargo test` needs `dlltool.exe`; the store builds
SQLCipher and OpenSSL from source and needs `make` and `perl`. Add MSYS2's MinGW and MSYS2 bin
folders to `PATH` in the shell:

```powershell
C:\msys64\usr\bin\pacman.exe -S make       # once
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
cargo test --workspace
```

Without `MAKEFLAGS`, `openssl-src` builds serially on Windows (~14 min cold instead of ~4.5 min).
The VS 2022 BuildTools instance has the C++ workload, Windows SDK and AddressSanitizer for native
fuzzing (`fuzz/run-windows.ps1`); product builds stay on GNU, so do not switch the default toolchain
casually. `cargo xtask ci` prints a clear error when `dlltool`, `make` or `perl` is missing.

## 8. Commits

- One logical change per commit, only when the user asks.
- Commit only green states: fmt, clippy, tests, layers, assets.
- Do not commit `target/`, `dist/`, databases, logs or downloaded corpora.

## 9. Lessons learned

Append one line when significant time is lost to an issue likely to recur. Format:
`- [area] short fact. fix/avoid: short action.`

- [Windows/Rust] GNU host needs `dlltool.exe` from MSYS2 MinGW for test binaries. fix/avoid:
  prepend `C:\msys64\mingw64\bin` to `PATH` or install mingw-w64.
- [egui 0.36] `SidePanel`/`TopBottomPanel` were unified into `Panel::{left,right,top,bottom}`;
  `eframe::App` implements `ui(&mut self, &mut Ui, &mut Frame)`, not `update(&Context, ...)`.
  fix/avoid: use the 0.36 API; panels are shown on a `Ui`, not a `Context`.
- [egui 0.36] Panels nest in the order added (first = outermost). A bottom status bar added after
  the side panels spans only the centre. fix/avoid: add the bottom panel before the side panels.
- [egui/Windows] An undecorated eframe window can stay blank because it starts hidden and never gets
  a redraw. fix/avoid: set `ViewportBuilder::with_visible(true)` when using
  `with_decorations(false)`.
- [egui/fonts] Inter maps several Phosphor PUA codepoints to stylistic alternates, so a shared
  proportional fallback renders letters instead of icons. fix/avoid: load Phosphor in a dedicated
  `FontFamily` and use it for every icon, never the proportional fallback.
- [Windows/DPI] A DPI-unaware PowerShell host virtualizes `GetWindowRect`/`PrintWindow`, so a
  125%-scaled app looks clipped and off-centre in captures. fix/avoid: call `SetProcessDPIAware()`
  before capturing.
- [egui/layout] At fractional display scaling the root UI can be wider than the real content rect,
  pushing right-aligned controls and the status bar off-screen. fix/avoid: wrap `show` in
  `scope_builder(UiBuilder::new().max_rect(ctx.content_rect()))`.
- [egui/TableBuilder] cell content is laid out from the top even when the cell uses
  `Align::Center`; a two-line subject touches the selected row edge. fix/avoid: add an explicit
  top inset before the vertical block.
- [egui/TextEdit] a focused single-line field swallows Up/Down before the palette can use them.
  fix/avoid: `ctx.input_mut(|i| i.consume_key(...))` before drawing the field.
- [egui/Window] `Window::show` returns `Option<InnerResponse<Option<R>>>`, so the closure result is
  nested twice. fix/avoid: unwrap both levels or the action type looks wrong.
- [egui/TextEdit] `TextEditOutput.cursor_range` uses the `CharIndex` newtype, and restored cursor
  state must be stored under `ui.make_persistent_id(id_salt)`. fix/avoid: use `.0` and the same id.
- [egui/DroppedFile] `DroppedFileHandle` is an `Arc<dyn DroppedFile>`: the path is a `path()`
  method, not a field. fix/avoid: call `file.path().to_path_buf()`.
- [egui/TextField] `horizontal_centered` inside a vertical layout stretches the field to the whole
  remaining height, pushing the rest of the form off-screen. fix/avoid: use `horizontal`, which
  already centres items vertically and stays content-sized.
- [Windows/screenshots] `PrintWindow` returns an all-black bitmap for the glow surface, and injected
  input does nothing on a disconnected desktop. fix/avoid: capture the client area from the screen
  with a DPI-aware process; seed UI state for the shot instead of clicking.
- [PowerShell] `Set-Content` / `Get-Content -Raw` re-encode UTF-8 as ANSI and corrupt accented text.
  fix/avoid: use the edit/write tools, or `[System.IO.File]::ReadAllText/WriteAllText` with UTF-8.
- [agents] a delegated agent cleaned its scratch files with a wildcard in a shared temp directory and
  removed other sessions' files. fix/avoid: give every agent its own temp subfolder and forbid
  deletes outside it in the brief.
- [egui/screenshots] formatting with `Local::now()`/`Utc::now()` makes PNG captures change every
  minute and breaks attribution hashes. fix/avoid: own the clock in the app, freeze it in screenshot
  mode, and seed demo dates from the same instant.
- [Windows/OpenSSL] `openssl-src` needs MSYS2 `make` + `perl` and, on Windows, ignores
  `CARGO_MAKEFLAGS` so a cold build runs serially (~14 min). fix/avoid: `pacman -S make`, put
  `C:\msys64\usr\bin` on `PATH`, set `MAKEFLAGS=-j$env:NUMBER_OF_PROCESSORS` (~4.5 min).
- [SQLCipher/rusqlite] a bound `PRAGMA key = ?` is read at prepare time and silently ignored on an
  existing file ("file is not a database"). fix/avoid: literal raw key `PRAGMA key = "x'<hex>'"`,
  never `conn.pragma_update(..., "key", ...)`.
- [cargo] a shared test helper stored as `tests/support.rs` is compiled as its own test binary, not
  as a module of the tests that declare it. fix/avoid: keep it at `tests/support/mod.rs` and declare
  `mod support;` in each integration test file.
- [SQLite/FTS5] `contentless_delete=1` accepts DELETE and full-row INSERT but rejects `UPDATE` of a
  subset of columns (`cannot UPDATE a subset of columns`). fix/avoid: the update trigger does
  delete+insert; never write a partial `UPDATE message_fts`.
- [SQLite/FTS5] joining every FTS match to `messages` before `ORDER BY bm25` pays one encrypted-row
  lookup per match (26 ms at 20k, 125 ms at 100k). fix/avoid: rank and page with FTS plus a small
  trigger-maintained `message_fts_rank(rowid,date_key,id_key)` table; do **not** apply a global
  date/id tie-break to a truncated Rust candidate window — that repeats pages when bm25 ties exceed it.
- [SQLite/WAL] the main database file alone under-reports size: committed FTS pages can still be in
  `mail.db-wal` (4 KiB vs 241 KiB in the review probe). fix/avoid: sum main+wal+shm for status and
  measure the index with `dbstat`, not the file size alone.
- [Windows/fs4] an exclusive byte-range lock is mandatory: another handle cannot read the locked file,
  so owner metadata stored in `<store>.lock` is unreadable while it is held. fix/avoid: keep the lock
  file empty and write pid/endpoint to a separate `<store>.owner` file.
- [Windows/named pipes] `Get-Acl` cannot address `\\.\pipe\...`; the pipe security must be asserted
  from the explicit SDDL/DACL before creation. fix/avoid: create with
  `ServerOptions::create_with_security_attributes_raw` and a one-ACE SDDL; test the SDDL and the token
  file ACL, not `Get-Acl` on the pipe.
- [Windows/named pipes] a listener that creates only `MAX_CONNECTIONS` instances dies when the last
  spare is consumed, and the client sees `ERROR_PIPE_BUSY`/`ERROR_PIPE_CLOSING` if instances are not
  pre-created fast enough. fix/avoid: `max_instances(254)`, create the next instance before returning
  the accepted one, retry connect on 231/232, and keep accept errors non-fatal.
- [Windows/canonicalize] `std::fs::canonicalize` returns a `\\?\` verbatim path, which changes lock,
  token and endpoint names and breaks visible path comparisons. fix/avoid: strip the local `\\?\`
  prefix (and map `\\?\UNC\` back to `\\`) before deriving names.
- [fs4/Windows] a hard link to a SQLCipher file derives a different `.lock` path for the same inode.
  fix/avoid: refuse a database with more than one link (`GetFileInformationByHandle` on Windows,
  `nlink` on Unix) before taking the lock.
- [cargo-fuzz/Windows] `cargo fuzz` defaults to the GNU host target; MSVC fuzzing also needs the VC
  C++/ASAN components, Windows SDK `LIB` paths and the ASan DLL on `PATH`. fix/avoid: run
  `fuzz/run-windows.ps1`, which calls `vcvars64.bat` and passes `--target x86_64-pc-windows-msvc`.
