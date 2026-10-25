# Development

> **Status:** M0 + U0.5 labels · **Last updated:** 2026-10-24

## 1. Toolchain

- Rust 1.95+ (edition 2024).
- On Windows the active host is `x86_64-pc-windows-gnu`. `cargo test` needs `dlltool.exe` from
  MSYS2 MinGW:

  ```powershell
  $env:PATH = "C:\msys64\mingw64\bin;$env:PATH"
  cargo test --workspace
  ```

  The MSVC toolchain may be installed, but it needs Visual Studio's `link.exe`, which is not on the
  PATH. Use GNU + MSYS2 for now.
- `postino-store` builds SQLCipher and OpenSSL from source. That needs MSYS2 `make` and `perl`, plus a
  parallel cold build:

  ```powershell
  C:\msys64\usr\bin\pacman.exe -S make       # once; perl is already installed
  $env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
  $env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
  ```

  Without `MAKEFLAGS`, `openssl-src` builds serially on Windows: ~14 min cold instead of ~4.5 min.
  Warm builds are seconds. See `docs/store-spike.md`.
- No Node, no npm, no browser build in M0.

## 2. Commands

```sh
cargo run -p postino-app                 # desktop shell, demo data
cargo run -p postino-cli -- --demo info
cargo run -p postino-cli -- --demo tools
cargo run -p postino-cli -- --demo mcp   # stdio MCP server
cargo xtask ci                           # fmt + clippy + tests + layers + assets
cargo xtask layers                       # dependency rules
cargo xtask assets                       # asset hashes and licences
cargo xtask assets --write               # regenerate ATTRIBUTION.md
```

`cargo xtask` is a Cargo alias in `.cargo/config.toml`.

## 3. Layout

```
crates/core/        domain, demo engine, operation registry
crates/platform/    OS seams: the store key in the Windows Credential Manager
crates/store/       encrypted SQLCipher store, schema v2, repository (not wired into the engine)
apps/postino-app/   egui shell
apps/postino-cli/   CLI + MCP server
xtask/              quality tasks
docs/               product, architecture, security, design, ADRs
assets/fonts/       Inter and JetBrains Mono (OFL)
assets/brand/       Postino mark and the original sketch
```

## 4. Adding an operation

1. Add the request struct in `crates/core/src/ops.rs`:
   ```rust
   #[derive(Debug, Clone, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct ThingArgs {
       pub id: String,
       #[serde(default)]
       pub verbose: bool,
   }
   ```
2. Add the result struct with `Serialize, Deserialize` if the CLI needs to decode it.
3. Add an `OpDef` in `op_defs()` with a precise title and description. `read_only` must be true
   for anything that does not mutate.
4. Add the match arm in `dispatch()` and a handler. Validate every input; return `OpError` with a
   message an agent can act on.
5. Add a core test. Add an MCP test only when the protocol surface changes.
6. Add a CLI subcommand if the operation is a daily one. `postino run` already covers the rest.
7. Update `ROADMAP.md` and, if the feature is user-visible, `docs/product.md`.

## 5. Adding an MCP tool

There is nothing separate to add. `tools/list` is generated from `Engine::tools()`, and
`tools/call` dispatches through `Engine::call`. If a tool needs a new annotation, extend `OpDef`;
do not fork the table.

## 6. Adding a crate

1. Add it to `Cargo.toml` `members` via `crates/*` or `apps/*`.
2. Add a `README.md` with its layer and responsibility.
3. Add its allowed workspace dependencies to `xtask/src/main.rs` `layers()`.
4. Run `cargo xtask layers`.
5. Add a test from day one.

Do not create empty crates "for later". The planned crates live in `docs/architecture.md` until
they have a reason to exist.

## 7. Adding an asset

1. Use only an openly licensed asset or one you created.
2. Add the file under `assets/`, plus its licence text if third-party.
3. Add an `[[asset]]` entry to `ATTRIBUTION.toml` with author, source, licence, licence file,
   SHA-256 and usage.
4. Run `cargo xtask assets --write`, then `cargo xtask assets`.
5. Never add fonts or icons without attribution. `cargo xtask assets` fails on a hash or licence
   mismatch.

## 8. Testing

- **Core:** operation behaviour, schema validity, pagination, flag counts, draft validation,
  address parsing, label CRUD/validation/caps and label filtering.
- **CLI:** `run` argument parsing, recipient flags, label subcommands, `messages --label` with and
  without `--folder`, `search --label`, MCP protocol (`initialize`, `tools/list`, `tools/call`,
  errors, notifications).
- **Store:** schema upgrade from v1, migration idempotence, repository round trips, counts after
  read/move/delete, atomic batches, draft revisions across reopen, wrong key, schema-too-new and a
  no-plaintext scan over the database, WAL and SHM.
- **Platform:** keyring load-or-create against `keyring_core::mock` only; the native vault is a
  manual, tagged smoke test and never part of the default suite.
- **UI:** headless render with `Context::run_ui` and `PostinoApp::show`, including the gallery,
  composer, sidebar label registry, label filter, inline create/rename and the multi-select label
  undo path. The test asserts the shell paints at least one shape.
- **Future:** a fake IMAP/SMTP server, MIME fixtures, property tests for parsers, fuzzing for MIME
  and HTML.

Run the full suite before a commit:

```sh
cargo xtask ci
```

If `cargo xtask ci` fails on `dlltool`, fix the PATH first; the error is environmental.

## 9. Looking at the UI

The debug binary opens a window:

```sh
cargo run -p postino-app
```

Deterministic screenshots render the real shell without mouse or keyboard injection:

```sh
cargo run -p postino-app -- --screenshot reading --out docs/images/postino-u04.png
cargo run -p postino-app -- --screenshot compose --out docs/images/postino-u04-compose.png
cargo run -p postino-app -- --screenshot compose --dark --out docs/images/postino-u04-compose-dark.png
cargo run -p postino-app -- --screenshot labels --out docs/images/postino-u05-labels.png
cargo run -p postino-app -- --screenshot label_menu --out docs/images/postino-u05-label-menu.png
cargo run -p postino-app -- --screenshot labels --size 980x600 --out tmp-labels-980.png
```

The states are `shell` (demo inbox, no selection), `reading` (first inbox message open), `compose`
(the composer prefilled with the docs fixture), `palette` (command palette open), `labels` (Lavoro
filter active, list visible), `label_menu` (a labelled message open with the modal label menu above
its captured target) and `label_editor` (the inline create editor open under the ETICHETTE header
with a name typed). `--dark` applies the dark theme. `--gallery` is a separate component review
window, not a screenshot state, and cannot be combined with `--screenshot`.

`--size WxH` sets the **logical** viewport. The default is 1280 × 720 and the minimum is 980 × 600.
The PNG has the physical pixel size of that viewport: 1280 × 720 at 100% display scaling and
1600 × 900 at 125%. The committed docs images were captured at 125%. Rendering is deterministic on
one machine at one display scale; do not assume byte-identical images across display scales, GPUs
or platforms.

Screenshot mode seeds its state and freezes the clock at `2026-10-24T10:42:00Z`; demo messages are
seeded relative to the same instant, so two captures a minute apart are byte-identical. The PNG is
written atomically: the encoder writes `<out>.tmp` and renames it over the destination, so a failed
capture never leaves a truncated image.

The command still needs a native GL window, so it is not a headless CI renderer. It captures its
own viewport with `ViewportCommand::Screenshot`, never injects input and never calls `PrintWindow`,
but a window must be able to open and paint on the desktop. A watchdog exits 1 with a message after
20 s if the window never returns a surface; closing the window before the capture also exits 1.
Flag errors exit 2, print the reason to stderr and write nothing.

Only `--screenshot <state>`, `--out <path>`, `--dark`, `--native-chrome`, `--size WxH` and
`--gallery` are accepted. Unknown flags, duplicate flags, `--flag=value` forms, a missing value and
a value that is another flag are all refused. `--native-chrome` keeps the operating system title
bar; on macOS the native title bar is always used. Screenshots otherwise use the same chrome rule
as a normal startup.

Only the images regenerated by the commands above come from the harness. The older `postino-m0`,
`postino-u02*` and UI-kit images predate it and are not reproduced by it. Regenerating a committed
image invalidates its SHA-256 in `ATTRIBUTION.toml`: update the affected `sha256` entries, run
`cargo xtask assets --write` to regenerate `ATTRIBUTION.md`, then run `cargo xtask assets` (or
`cargo xtask ci`) to verify the hashes.

## 10. Style

- Rust 2024, `rustfmt` with `max_width = 150`.
- Clippy with `-D warnings`.
- No `unwrap`/`expect`/`panic!` in non-test code.
- Prefer small functions and plain data over trait hierarchies.
- Comments explain *why*, not *what*. Link to the relevant ADR or doc when a decision is subtle.
- Keep `postino-core` free of UI, filesystem and network dependencies until a crate exists for
  them.
