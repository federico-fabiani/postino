# Postino

**Local-first, agent-first mail.** A fast, native email client written in Rust, with one operation
registry shared by the desktop app, the CLI and an MCP server.

> **Status: M0 walking skeleton + U0 UI foundation + the engine on the encrypted store (P3c/P3d) +
> the MIME crate (P4a) + encrypted attachment blobs (P4b) + the IMAP client (P5a) + sync into the
> store (P5b/P5c) + accounts, credentials and sync operations (P6a) + onboarding and the app on real
> data (P6b) + the text reading pane, safe links, attachment Save and full settings (P6c).** The
> desktop app opens the encrypted SQLCipher store by default, offers a guided Aruba/Gmail/custom
> onboarding with a real connection test and background sync, and reads cached mail offline; `--demo`
> keeps the synthetic mailbox for screenshots and trials. The CLI and the MCP
> server use the same operation registry: `postino --store <path>` (or the default
> `%LOCALAPPDATA%\Postino\mail.db` on Windows) runs the same operations on the encrypted database,
> and the CLI can add a real Aruba or Gmail account with an app password, test it, sync it and remove
> it. Passwords live only in the OS keyring; **Windows Credential Manager is the only keyring backend
> in this build**, and macOS/Linux fail at the platform seam until their keychains land with
> packaging. MIME parsing, sanitization and serialization exist as `postino-mime`;
> `postino-net` is an IMAP4rev1 client that logs in over TLS, lists folders, selects, fetches
> envelopes and `BODY.PEEK[]` bodies, stores flags and idles; and `postino-sync` reconciles one
> account into the encrypted store: folders with special-use, UIDVALIDITY resets, incremental UID
> fetches, MIME bodies, attachment bytes in the blob store, deletions, flags on first insert, a
> cancellable run and a persistent status. Attachment bytes are encrypted at rest in a
> content-addressed blob directory next to the database. Gmail OAuth is P12 and SMTP and the
> background daemon land in M2-M4: no mail is sent, and the composer's send is still a local move to
> Sent.

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
- A text reading pane with sender, recipients, Cc, date, subject and label chips, a quiet “Vista
  testo” note, and safe links. Only absolute `http`, `https` and `mailto` are clickable; the full
  destination and the effective host appear in the confirmation sheet, and only **Apri** hands it to
  the system browser. A punycode (`xn--`) or non-ASCII host warns first, and userinfo, backslash
  tricks and `mailto:` Cc/Bcc queries are refused. No `javascript:`, `file:`, `data:`, `vbscript:`
  or relative target is ever a link.
- Attachment Save through the native Save dialog writes the exact blob bytes, warns before saving an
  executable or macro-enabled file (including a `.exe` chosen in the dialog), refuses to replace an
  existing file until confirmed, and reports a missing blob as a readable toast. In `--demo` Save is
  visibly disabled because the synthetic mailbox has no attachment bytes. Settings covers accounts
  (add, remove, active, last sync per account), appearance (light/dark/system, reduced motion,
  reading width), data (store and blob paths with a copy action, open the blob folder) and version,
  licences and shortcuts; every section fits 980×600 and returns focus to the account switcher on
  `Esc`.
- Twenty typed operations with JSON Schemas: `app_info`, `accounts_list`, `account_add`,
  `account_test`, `account_remove`, `sync_now`, `sync_status`, `folders_list`, `messages_list`,
  `message_get`, `message_flag`, `message_move`, `draft_save`, `draft_send`, `search`, `labels_list`,
  `label_create`, `label_update`, `label_delete`, `label_apply`.
- Real accounts from the CLI: Aruba and Gmail are discovered offline (Gmail needs 2FA + an app
  password; OAuth is P12), `custom` takes host/port/TLS, a no-echo prompt or `--password-stdin` is the
  only way in, and MCP cannot add or remove an account. Passwords live in the OS keyring, never in
  the database, logs or tool output.
- `postino` CLI for humans and scripts, including friendly `draft save --send`, `label` and
  `postino run` for any operation.
- Opt-in MCP server over stdio: `postino mcp`. No port, no telemetry.
- The M1 persistence foundation, now wired into the engine: an encrypted SQLCipher store with schema
  v4 (draft revision sequence, label high-water mark, per-account IMAP endpoint), additive migrations
  and a typed repository (`postino-store`), plus the OS keyring seam (`postino-platform`). `Engine::open`
  loads the mailbox once, repairs orphan drafts, writes every mutation through in one transaction
  and keeps store errors categorised; `Engine::demo()` is unchanged and the desktop app still uses it.
- The M1 MIME foundation as a crate (`postino-mime`, P4a): bounded RFC 5322/2045 parsing with
  recorded warnings, an allowlist HTML sanitizer that removes remote images (counting the
  `http`/`https` ones), HTML-to-text, and deterministic RFC 5322 serialization with header-injection
  guards and `Bcc` kept in the envelope. It is wired into sync and the engine; the desktop reading
  pane shows its text alternative and does not render HTML yet.
- Encrypted attachment bytes (P4b): `postino-crypto` derives an HKDF-SHA256 `blob` subkey from the
  store key and seals bytes with XChaCha20-Poly1305; `postino-store` keeps each distinct attachment
  content once under `<db file>.blobs/<xx>/<hash>.blob` (BLAKE3 name, zstd only when it helps,
  authenticated dedup that heals a damaged file, bounded reads, atomic writes), and
  `attachments.blob_hash` finally points at something. The engine exposes `attachment_bytes` and
  `store_attachment_bytes`, with tests across a reopen; sync writes the bytes and the P6c reading
  pane reads them through `attachment_bytes` when a person saves an attachment.
- The M1 network foundation as a crate (`postino-net`, P5a): an IMAP4rev1 client over implicit TLS
  or STARTTLS, with the operating system's certificate verifier and no way to switch verification
  off. It logs in with `AUTHENTICATE PLAIN` or `LOGIN`, lists folders with special-use and modified
  UTF-7 names, selects and examines, fetches envelopes and whole bodies with `BODY.PEEK[]` (never
  setting `\Seen`), stores flags by UID, runs one `IDLE` cycle and logs out. Every line, literal,
  command and response is bounded, the password is zeroised and redacted, and a scripted TLS server
  in `postino-testkit` proves each path, including a 54-case hostile corpus. It is not wired into the
  app or a scheduler yet.
- The M1 synchronization crate (`postino-sync`, P5b): `sync_account` reconciles one IMAP account into
  one encrypted store. It lists folders once and maps RFC 6154 special-use (`\Inbox`, `\Sent`,
  `\Drafts`, `\Trash`, `\Junk`, `\Archive`, `\Flagged`), resets a folder when `UIDVALIDITY` changes,
  fetches only new UIDs in bounded windows, parses each body with `postino-mime`, stores attachment
  bytes through the encrypted blob seam only within the per-attachment limit, removes cached messages
  whose UID disappeared, applies server flags on first insert and reports later drift without
  overwriting local read/star state. One transaction per message and a cursor that advances only after
  commit make a crash or a cancellation resume without a gap. Runs are cancellable, bounded by
  `SyncLimits`, and leave a JSON status under `meta`. Tested against the scripted server (initial,
  incremental, UIDVALIDITY, deletion, flags, oversized attachments, three network-drop points,
  cancellation + reopen, `BODY.PEEK`-only and an offline read through `Engine::open`), with a
  2 000-message test in CI and 10 000 behind `POSTINO_SLOW_TESTS=1`. P6a wires it to `sync_now`, so
  the CLI (`postino sync run`) and MCP can run it on demand; there is still no scheduler or daemon.
- Headless rendering tests of the shell, gallery, composer and label flows, the reading pane's
  link/attachment/settings paths with injected opener/saver recorders, operation-schema tests, MCP
  protocol tests, and store round-trip tests at both the repository and engine level.

![Postino composer: recipients, subject, Markdown formatting bar, body and attachment, with the draft save state in the footer](docs/images/postino-u04-compose.png)

![Postino labels: the Lavoro view with the mailbox-wide scope in the header, the filtered message list and the coloured label rows in the ETICHETTE sidebar](docs/images/postino-u05-labels.png)

![Postino label menu: the modal label picker with its scrim over the list, Lavoro already applied and visible in the reading-pane chip](docs/images/postino-u05-label-menu.png)

![Postino reading pane: sender and recipients, label chip, the text view note, a safe link and a pinned attachment strip with Save](docs/images/postino-p6c-reading-attachments.png)

![Postino reading pane at 980×600: the text view and the pinned attachment strip with Save above the status bar](docs/images/postino-p6c-reading-attachments-980.png)

![Postino settings: accounts with the active badge, appearance choices, store and blob paths, last sync, and the licences and shortcut entries](docs/images/postino-p6c-settings.png)

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

# A real account: the password is prompted without echo (or pipe one line with --password-stdin)
cargo run -p postino-cli -- account add --provider aruba --email me@example.it --name "Federico"
cargo run -p postino-cli -- account list
cargo run -p postino-cli -- account test acc_0123456789abcdef
cargo run -p postino-cli -- sync run
cargo run -p postino-cli -- sync status
cargo run -p postino-cli -- account remove acc_0123456789abcdef --yes
```

The app is `postino-app`; the command is `postino`. The product name stays Postino.

Without `--store` and without `--demo`, the CLI uses the default encrypted store at
`%LOCALAPPDATA%\Postino\mail.db` on Windows and takes the key from the OS keyring (service
`postino`, scope `default`). **Windows is the only keyring backend in this build**: macOS and Linux
fail at the `postino-platform` seam with `PlatformError::Unsupported` until their native keychains
land with packaging, so there is no platform-equivalent default store yet. `--store` picks another
SQLCipher database; `--demo` and `--store` are mutually exclusive. First-run key creation is
serialised with `<app local data>/Postino/keyring.lock` and verified by reading the key back. An IMAP
credential is stored under `account:<id>:imap` in the same keyring, never in the database; the CLI's
prompt does not echo and there is no `--password` argument. One process owns the database: do not
point two `postino --store` commands at the same file.

Read-only commands (`info`, `tools`, `schema`, `account list`, `account test`, `sync status`, mail
reads) never create a missing default store and never touch the keyring for it: they report that no
account is configured. An explicit `--store` on a read-only command must already exist, otherwise it
is a usage error. Mutations (`account add`, `sync run`, account removal, draft/label/flag writes) may
create the default store. Exit codes are 0 for success, 1 for an operation failure and 2 for a usage
error, including `OpError::InvalidArgs` (missing `--yes`, an invalid address/name/endpoint, an unknown
id) and malformed `postino run` arguments. The `add | test | list | remove` account commands,
`sync run | status` and the desktop onboarding all use the same encrypted store; Gmail OAuth is P12
and no SMTP connection is opened.

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

Without `--demo` and `--store`, `postino mcp` opens the default store. The tools are the same
registry as the CLI, but `account_add` and `account_remove` are marked sensitive: they are absent
from `tools/list` and refused by `tools/call`, so an agent can neither set nor see a password and
cannot create or remove an account. `accounts_list`, `account_test`, `sync_now` and `sync_status` are
available; none of their results contains a credential.

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
  filters are provider features. The reading pane shows the plain-text alternative: it does not
  render HTML and never loads remote images. The MIME sanitizer removes remote images before storage,
  but the blocked-image count is not surfaced in the pane yet: carrying it through sync and the store
  is the P6d follow-up after the schema v5 work. In `--demo` the synthetic attachments have no bytes, so
  Save is disabled with that reason rather than reporting a corrupt file. A link reaches the system
  browser only after the confirmation sheet, and Save writes attachment bytes to the chosen path
  without opening the file. See [docs/product.md](docs/product.md).

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
