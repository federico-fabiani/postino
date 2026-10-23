# AGENTS.md — rules for every agent and contributor

These rules bind every AI agent and human working in this repository. They come before any task.
If a rule here conflicts with a habit from another project, this file wins.

## 1. What Postino is

Postino is a local-first, agent-first mail client written in Rust. The desktop app, the `postino`
CLI and the `postino mcp` server are three clients of **one operation registry**. M0 is a walking
skeleton: synthetic demo mail in memory, no IMAP, no SMTP, no database yet.

**The current milestone is the only source of truth for scope.** Read `ROADMAP.md` before
starting. Do not build M1 features into M0.

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
apps/postino-app   egui shell            L3
apps/postino-cli   CLI + MCP server      L3
crates/core        domain + operations   L1
xtask              build/quality tasks   tooling
```

Rules enforced by `cargo xtask layers`:

- `postino-core` depends on no other workspace crate and never on a UI toolkit (`egui`, `eframe`,
  `winit`, `rfd`).
- `postino-app` and `postino-cli` may depend on `postino-core` and on libraries, never on each other.
- New crates need a layer and a README before they need code. Planned crates are listed in
  `docs/architecture.md`; do not create speculative stubs.

## 4. Operations

- Names are `snake_case` (`messages_list`): MCP clients reject dots.
- Every request struct uses `#[serde(deny_unknown_fields)]`. Typos fail loudly.
- Every operation has a precise title, a description an agent can act on, a JSON Schema derived
  from the request type, and `read_only` / `destructive` annotations.
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

The active Rust host is `x86_64-pc-windows-gnu`. `cargo test` needs `dlltool.exe`; add MSYS2's
MinGW bin folder to `PATH` in the shell:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"
cargo test --workspace
```

The MSVC toolchain is installed but lacks `link.exe`; do not switch to it without installing Visual
Studio Build Tools. `cargo xtask ci` prints a clear error when `dlltool` is missing.

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
