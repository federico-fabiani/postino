# Postino

**Local-first, agent-first mail.** A fast, native email client written in Rust, with one operation
registry shared by the desktop app, the CLI and an MCP server.

> **Status: M0 walking skeleton + U0 UI foundation + the engine on the encrypted store (P3c/P3d) +
> the MIME crate (P4a).** The shell, the operation registry, the CLI and the MCP server work against
> a synthetic demo mailbox, and `postino --store <path>` can now run the same operations on the
> encrypted SQLCipher database: accounts, folders, messages, labels, drafts and flags survive a
> restart. The desktop app still boots on the demo until onboarding (P6). MIME parsing,
> sanitization and serialization exist as `postino-mime` but are not wired into the engine yet;
> IMAP, SMTP and the background daemon land in M1-M4. No real account is configured, no mail leaves
> the machine, and the composer's send is a local move to Sent, never an SMTP submission.

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
- Compose window with To/Cc/Bcc, subject, signature, Markdown formatting, drag-and-drop attachment
  metadata, autosaved drafts and reply/reply-all/forward prefill. Send is a local move to Sent until
  M2 (no SMTP) and is undoable. `draft_save` is a patch on update: an omitted content field keeps the
  draft's current value, while an explicitly empty value clears it. The CLI sends only the flags the
  user passed and accepts `--account`; the app keeps sending the full content.
- Local labels with a semantic colour palette: create, rename and delete from the sidebar; apply and
  remove from the message menu (`L`), the command palette, the CLI and MCP; filter the list by label
  and see label chips in the reading pane. A label view spans every folder by default and a folder
  narrows it; ids are stable and never reused. Rename, delete and apply/remove are undoable in the
  app; creating a label is not an undo step. Labels are in-memory metadata until M1.
- Fifteen typed operations with JSON Schemas: `app_info`, `accounts_list`, `folders_list`,
  `messages_list`, `message_get`, `message_flag`, `message_move`, `draft_save`, `draft_send`,
  `search`, `labels_list`, `label_create`, `label_update`, `label_delete`, `label_apply`.
- `postino` CLI for humans and scripts, including friendly `draft save --send`, `label` and
  `postino run` for any operation.
- Opt-in MCP server over stdio: `postino mcp`. No port, no telemetry.
- The M1 persistence foundation, now wired into the engine: an encrypted SQLCipher store with schema
  v3 (draft revision sequence and label high-water mark), additive migrations and a typed repository
  (`postino-store`), plus the Windows Credential Manager key seam (`postino-platform`). `Engine::open`
  loads the mailbox once, repairs orphan drafts, writes every mutation through in one transaction
  and keeps store errors categorised; `Engine::demo()` is unchanged and the desktop app still uses it.
- The M1 MIME foundation as a crate (`postino-mime`, P4a): bounded RFC 5322/2045 parsing with
  recorded warnings, an allowlist HTML sanitizer that removes remote images (counting the
  `http`/`https` ones), HTML-to-text, and deterministic RFC 5322 serialization with header-injection
  guards and `Bcc` kept in the envelope. It is not wired into the engine or the app yet, so the
  shell still shows demo mail.
- Headless rendering tests of the shell, gallery, composer and label flows, operation-schema tests,
  MCP protocol tests, and store round-trip tests at both the repository and engine level.

![Postino composer: recipients, subject, Markdown formatting bar, body and attachment, with the draft save state in the footer](docs/images/postino-u04-compose.png)

![Postino labels: the Lavoro view with the mailbox-wide scope in the header, the filtered message list and the coloured label rows in the ETICHETTE sidebar](docs/images/postino-u05-labels.png)

![Postino label menu: the modal label picker with its scrim over the list, Lavoro already applied and visible in the reading-pane chip](docs/images/postino-u05-label-menu.png)

Everything below the UI is headless and has no egui dependency.

## Quick start

Requires Rust 1.98 or newer.

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
cargo run -p postino-cli -- --demo draft save --to "Giulia <giulia.ferri@example.com>" --subject "Ciao" --body "Ci sentiamo domani." --send
cargo run -p postino-cli -- --demo label list
cargo run -p postino-cli -- --demo label create "Ufficio" --color forest
cargo run -p postino-cli -- --demo messages --label lbl_1
cargo run -p postino-cli -- --demo messages --label lbl_1 --folder sent
cargo run -p postino-cli -- --demo search preventivo --label lbl_1

# Encrypted local store: the same operations, persisted across restarts
cargo run -p postino-cli -- --store "$env:TEMP\postino\mail.db" info
```

The app is `postino-app`; the command is `postino`. The product name stays Postino.

`--store` opens or creates the SQLCipher database at that path and takes the key from the OS keyring
(service `postino`, scope `default`); `--demo` and `--store` are mutually exclusive. First-run key
creation is serialised with `<app local data>/Postino/keyring.lock` and verified by reading the key
back. A fresh store has no accounts until onboarding (P6), so mail commands report that nothing is
configured; the store is exercised end to end by the test suite today. One process owns the database:
do not point two `postino --store` commands at the same file.

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

Without `--demo`, the engine has no accounts and the tools return empty lists. With
`--store <path>` the same tools read and write the encrypted database. Account onboarding, which
creates the first account and folder tree in a fresh store, is the next milestone (P6).

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
Windows import libraries. `postino-store` also builds SQLCipher and OpenSSL from source, so it needs
MSYS2 `make` and `perl`. Add both MSYS2 folders to `PATH` and let `make` use every core:

```powershell
C:\msys64\usr\bin\pacman.exe -S make   # once
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
$env:MAKEFLAGS = "-j$env:NUMBER_OF_PROCESSORS"
cargo test --workspace
```

If `cargo test` says `error calling dlltool`, the MinGW folder is missing from `PATH`. A cold
SQLCipher/OpenSSL build takes ~4.5 min with `MAKEFLAGS` and ~14 min without; warm builds are seconds.
See `docs/store-spike.md`.

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
- [Store spike](docs/store-spike.md): P3a encrypted-SQLite evaluation, numbers and decision.
- [Briefs](docs/briefs/): the package contracts, including P3c engine wiring.
- [Roadmap](ROADMAP.md): M0 to M6.
- [Decisions](docs/decisions/): UI, runtime and sync ADRs.

## Licence

MIT OR Apache-2.0. Fonts and logos are attributed in [ATTRIBUTION.md](ATTRIBUTION.md).

Postino is an independent project. It is not affiliated with any email provider.
