# Development

> **Status:** M0 · **Last updated:** 2026-10-23

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

- **Core:** operation behaviour, schema validity, pagination, flag counts, unknown arguments.
- **CLI:** MCP protocol (`initialize`, `tools/list`, `tools/call`, errors, notifications).
- **UI:** headless render with `Context::run_ui` and `PostinoApp::show`. The test asserts the shell
  paints at least one shape.
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

For a quick visual check on Windows, start the built binary and capture the screen with
`System.Drawing` (the maintainers use this during development). The shell is deterministic enough
that the demo inbox is a useful fixture. A future `egui_kittest` screenshot harness will make this
part of CI.

## 10. Style

- Rust 2024, `rustfmt` with `max_width = 150`.
- Clippy with `-D warnings`.
- No `unwrap`/`expect`/`panic!` in non-test code.
- Prefer small functions and plain data over trait hierarchies.
- Comments explain *why*, not *what*. Link to the relevant ADR or doc when a decision is subtle.
- Keep `postino-core` free of UI, filesystem and network dependencies until a crate exists for
  them.
