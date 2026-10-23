# Postino

**Local-first, agent-first mail.** A fast, native email client written in Rust, with one operation
registry shared by the desktop app, the CLI and an MCP server.

> **Status: M0, walking skeleton.** The shell, the operation registry, the CLI and the MCP server
> work against a synthetic demo mailbox. IMAP, SMTP, SQLite and the background daemon land in M1-M4.
> No real account is configured and no mail leaves the machine yet.

![Postino M0 desktop shell: folder rail, message list and reading pane in forest, ivory and gold](docs/images/postino-m0.png)

```
Desktop  ─┐
CLI      ─┼──  one operation registry  ──  postino-core (Rust)
MCP      ─┘
```

## What M0 already does

- A native egui shell with the Postino palette: forest `#173D32`, ivory `#F5EBD6`, warm gold
  `#E4B96A`.
- Folder rail, message table, reading pane, filters, read/starred toggles, command palette
  (`Ctrl+K`), dark theme and a demo inbox with realistic threads and attachments.
- Seven typed operations with JSON Schemas: `app_info`, `accounts_list`, `folders_list`,
  `messages_list`, `message_get`, `message_flag`, `search`.
- `postino` CLI for humans and scripts, including `postino run` for any operation.
- Opt-in MCP server over stdio: `postino mcp`. No port, no telemetry.
- Headless rendering test of the shell, operation-schema tests, MCP protocol tests and CLI tests.

Everything below the UI is headless and has no egui dependency.

## Quick start

Requires Rust 1.95 or newer.

```sh
# Desktop app (demo mailbox)
cargo run -p postino-app

# CLI
cargo run -p postino-cli -- --demo info
cargo run -p postino-cli -- --demo folders
cargo run -p postino-cli -- --demo messages --folder inbox --limit 5
cargo run -p postino-cli -- --demo read msg_001
cargo run -p postino-cli -- --demo search fattura
cargo run -p postino-cli -- --demo run messages_list folder=inbox limit=2
```

The app is `postino-app`; the command is `postino`. The product name stays Postino.

### MCP

```sh
cargo run -p postino-cli -- --demo mcp
```

The server speaks newline-delimited JSON-RPC 2.0 on stdin/stdout, implements `initialize`,
`ping`, `tools/list` and `tools/call`, and stops at EOF. It is never started automatically.

Example client configuration:

```json
{
  "mcpServers": {
    "postino": {
      "command": "postino",
      "args": ["--demo", "mcp"]
    }
  }
}
```

Without `--demo`, the engine has no accounts and the tools return empty lists. A real store and
account onboarding arrive in M1.

## Quality gates

```sh
cargo xtask ci
```

Runs `cargo fmt --check`, clippy with `-D warnings`, the test suite, the layering check and the
asset check. `cargo xtask layers` enforces that `postino-core` stays headless. `cargo xtask assets`
verifies every font and logo against `ATTRIBUTION.toml`; `cargo xtask assets --write` regenerates
`ATTRIBUTION.md`.

### Windows note

The repository currently builds with the GNU Rust toolchain, which needs `dlltool.exe` for the
Windows import libraries. If `cargo test` says `error calling dlltool`, add MSYS2's MinGW bin
folder to `PATH` for that shell:

```powershell
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"
cargo test --workspace
```

The MSVC toolchain is also installed on some machines, but it needs Visual Studio's `link.exe`.
The GNU toolchain plus MSYS2 is the path documented here.

## Design

- **Engine-first.** Every feature is an operation with a typed request, a JSON Schema and an
  end-to-end test. The GUI is one client, not the centre.
- **Local-first.** Mail is cached, indexed and searched on the machine. No Postino account, no
  cloud, no telemetry.
- **Rust core.** The network, storage, MIME and security code is Rust. The egui shell is Rust too.
  HTML email rendering is isolated behind a trait; a sandboxed webview backend can be added for
  fidelity without letting a browser engine into the rest of the app.
- **Honest limits.** IMAP is the sync protocol; the server stays the source of truth. Avatars shown
  to recipients depend on BIMI or a signature, not on this client. Auto-reply and server-side
  filters are provider features. See [docs/product.md](docs/product.md).

## Documentation

- [Product](docs/product.md): vision, scope, non-goals, honest limits.
- [Architecture](docs/architecture.md): current M0 shape and the target design.
- [MCP](docs/mcp.md): the stdio server, tools and protocol.
- [Threat model](docs/threat-model.md): what Postino protects and what it does not.
- [Security policy](SECURITY.md): how to report a vulnerability.
- [Design](docs/design.md): palette, contrast, type, logo.
- [Development](docs/development.md): toolchain, layout, testing.
- [Roadmap](ROADMAP.md): M0 to M6.
- [Decisions](docs/decisions/): UI, runtime and sync ADRs.

## Licence

MIT OR Apache-2.0. Fonts and logos are attributed in [ATTRIBUTION.md](ATTRIBUTION.md).

Postino is an independent project. It is not affiliated with any email provider.
