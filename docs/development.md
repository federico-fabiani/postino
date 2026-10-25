# Development

> **Status:** M0 + U0.5 labels + P3c engine on the encrypted store + P4a MIME crate + P4b encrypted
> attachment blobs + P5a IMAP client + P5b sync into the store + P2b states/accessibility ·
> **Last updated:** 2026-10-25

## 1. Toolchain

- Rust 1.98+ (edition 2024). The MIME crate's parser, `mail-parser` 1.0, sets the floor.
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
cargo run -p postino-app                      # desktop shell, demo data (until P6)
cargo run -p postino-cli -- --demo info
cargo run -p postino-cli -- --demo tools
cargo run -p postino-cli -- --demo mcp        # stdio MCP server
cargo run -p postino-cli -- --store mail.db info
# the mail commands need an onboarded account; a fresh store has none until P6
cargo xtask ci                                # fmt + clippy + tests + layers + assets
cargo xtask layers                            # dependency rules
cargo xtask assets                            # asset hashes and licences
cargo xtask assets --write                    # regenerate ATTRIBUTION.md
```

`cargo xtask` is a Cargo alias in `.cargo/config.toml`.

## 3. Layout

```
crates/core/        domain, engine (demo or store-backed), operation registry, store_map
crates/crypto/      HKDF-SHA256 subkeys, XChaCha20-Poly1305 seal/open, zeroising key type
crates/mime/        MIME parse/serialize, HTML sanitization, HTML-to-text, header guards
crates/net/         IMAP4rev1 client over TLS, bounded and typed
crates/platform/    OS seams: the store key in the Windows Credential Manager
crates/store/       encrypted SQLCipher store, schema v3, repository, encrypted blob directory
crates/sync/        one IMAP account into the store: folders, UIDs, MIME, blobs, status
crates/testkit/     test-only fake IMAP server, TLS fixture and synthetic mail
apps/postino-app/   egui shell (demo data until P6)
apps/postino-cli/   CLI + MCP server
xtask/              quality tasks
docs/               product, architecture, security, design, ADRs
assets/fonts/       Inter and JetBrains Mono (OFL)
assets/brand/       Postino mark and the original sketch
```

## 3.1 Running on the encrypted store

`postino --store <path>` opens (or creates) the SQLCipher database at `<path>` and loads the
mailbox into the engine. The key is `postino_platform::load_or_create_key("default")`: 32 random
bytes created on first use and kept in the OS keyring under service `postino`, user `default`. The
key is never printed, logged or written to disk by Postino; a different scope or a different key
cannot read the file. The keyring is the only secret seam, and tests never touch it: every store
test uses a random `StoreKey` over a temp directory.

What is persisted today: accounts, folders with their cached counts, messages with recipients,
bodies, flags and labels, attachment metadata, labels, message/label links and draft revisions.
Writes are write-through: the repository commits first, the engine updates memory second, and a
failed write returns `OpError::Failed` without changing memory. Moving, flagging, labelling,
renaming labels and saving/sending drafts all survive a restart.

Attachment bytes live next to the database, not in it: `Store::open` also creates and opens
`<db file>.blobs` (`mail.db` → `mail.db.blobs`), one encrypted file per distinct content
(`mail.db.blobs/<xx>/<BLAKE3>.blob`, see `docs/architecture.md` §4.5). The blob key is derived from
the same store key with HKDF-SHA256 (label `blob`), so a store without its keyring entry loses its
attachments too. Back up or move the database file and its `.blobs` directory together; renaming one
without the other leaves the attachment rows pointing at `BlobMissing`. A damaged blob is
`CorruptBlob`; the engine reports it as `OpError::Failed` and leaves the message intact. `put`
authenticates an existing file before it deduplicates, so storing the same bytes again repairs a
damaged or foreign file; there is no separate repair command. A draft re-save keeps the stored hash
and size of an attachment that survives, so a stale metadata size cannot shrink the read cap. There
is no garbage collector yet: deleting a message leaves its blob, because the same content may belong
to another message. Nothing in the CLI, the app or the operations calls the seam yet; sync (P5b) and
the reading pane (P6) will.

What is still demo or missing:

- The desktop app always boots `Engine::demo_at`; the store is CLI-only until P6 onboarding chooses
  the default path and the account it belongs to.
- A fresh store has no accounts and no folders, and nothing creates them yet. Onboarding is P6. Until
  then a fresh store answers `--store <path> info` and the mail commands report that no account is
  configured. The test suite seeds a store through the `postino-store` repository to exercise the
  flow end to end.
- One process owns the store. There is no daemon, no pipe and no second writer (M4); the CLI and a
  future app must not open the same file at once. Do not run two `postino --store` commands against
  one database.
- No IMAP, MIME, SMTP, FTS5, attachment bytes in the UI or operations, or scheduled send.
  `draft_send` is still a local demo move to Sent and contacts no server. Attachment bytes have a
  storage seam and tests but no caller yet.
- `uid`, `Message-ID`, RFC 822 size and `in_reply_to` are not written by the engine; the columns
  exist for P4/P5.
- The database file lives wherever `--store` points; Postino does not create the parent directory.

A quick manual check that persistence works (unit level, no keyring):

```sh
cargo test -p postino-cli cli_store_keeps_a_draft_across_a_reopen -- --nocapture
cargo test -p postino-core --test engine_store -- --nocapture
cargo test -p postino-core --test engine_blobs -- --nocapture
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
  no-plaintext scan over the database, WAL and SHM; plus the one-query-per-table mailbox hydration
  used by `Engine::open`.
- **Blobs:** round trip and reopen, dedup, wrong key, tampered ciphertext/tag/nonce, a swapped file,
  an oversized declared length, traversal and non-hex hashes, temp cleanup, eight concurrent puts,
  compression only when it helps, header layout, remove/has and a no-plaintext scan over the
  database and every blob file (including the key bytes). `cargo test -p postino-store --test blobs`.
- **Crypto:** HKDF subkeys, seal/open round trip, tampered values, wrong key and AAD, nonce
  uniqueness, redacted `Debug`. `cargo test -p postino-crypto`.
- **MIME:** ten synthetic fixtures (plain, alternative, HTML newsletter, nested mixed, RFC 2047,
  RFC 2231, quoted-printable, XSS, malformed, injection), the allowlist scheme/attribute rules,
  serializer round trip and injection rejection, Bcc absence, deterministic bytes, bounds and a
  mutation corpus that must not panic. `cargo test -p postino-mime`.
- **Engine on the store:** a demo-shaped fixture written to a temp store and reopened (accounts,
  folders, counts, messages, recipients, labels, attachments, flags), mutations surviving a reopen
  (draft revision, send, label apply/remove, move, read/star), a repository failure surfacing as
  `OpError` with memory unchanged, and a no-plaintext scan of the engine's own writes. The CLI test
  creates a draft in a temp store, reopens and lists it.
- **Engine attachment seam:** one message with two attachments, bytes stored for one, engine dropped
  and reopened, bytes read back and compared, `blob_hash` and `has_attachments` intact; a one-byte
  corruption surfaces as `OpError::Failed` without poisoning the message; the demo engine, an unknown
  attachment and an attachment without bytes are readable errors; a 4 MiB blob exercises the bounded
  path without a timing assertion.
- **Platform:** keyring load-or-create against `keyring_core::mock` only; the native vault is a
  manual, tagged smoke test and never part of the default suite.
- **UI:** headless render with `Context::run_ui` and `PostinoApp::show`, including the gallery,
  composer, sidebar label registry, label filter, inline create/rename, the multi-select label
  undo path, the empty-state resolver and its three rendered filter states, contrast tokens and
  computed button hints, accessible row names and star targets, keyboard focus, the auto-read
  cancellation, the settings round trip and the plain/Shift J/K anchor. The test asserts the shell
  paints at least one shape.
- **Future:** a fake IMAP/SMTP server, property tests for the network parsers, fuzzing for MIME and
  HTML.

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
cargo run -p postino-app -- --screenshot empty_unread --out docs/images/postino-p2b-empty-unread.png
cargo run -p postino-app -- --screenshot empty_attachments --out docs/images/postino-p2b-empty-attachments.png
cargo run -p postino-app -- --screenshot empty_label_unread --out docs/images/postino-p2b-empty-label-unread.png
cargo run -p postino-app -- --screenshot shell --size 980x600 --out tmp-shell-980.png
```

The states are `shell` (demo inbox, no selection), `reading` (first inbox message open),
`reading_full` (the same message with the pane expanded), `compose` (the composer prefilled with
the docs fixture), `palette` (command palette open), `labels` (Lavoro filter active, list visible),
`label_menu` (a labelled message open with the modal label menu above its captured target),
`label_editor` (the inline create editor open under the ETICHETTE header with a name typed),
`empty_unread` (the inbox with every message read and the unread filter on), `empty_attachments`
(inbox mail with attachments moved out and the attachments filter on) and `empty_label_unread`
(Lavoro selected, its unread mail read, the unread filter on). `--dark` applies the dark theme.
`--gallery` is a separate component review window, not a screenshot state, and cannot be combined
with `--screenshot`.

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

## 9.1 Preferences

The app owns one small settings file, not eframe storage:
`%APPDATA%\Postino\settings.json` on Windows, `$XDG_CONFIG_HOME/Postino/settings.json` (or
`~/.config/Postino/settings.json`) elsewhere. It holds exactly five fields: theme (`dark`), the
reading-pane fraction of the width left by the sidebar (`reading_pane_ratio`), the last folder id
(`folder`) and `reduced_motion`, plus the schema `version`. Missing, truncated, unknown-version or
out-of-range values fall back to the defaults (light, 0.52, `inbox`, motion on) without a dialog.
The file is written atomically (`.tmp` then rename) at most once a second while a pane is dragged
and once on exit; renaming, deleting or corrupting it is safe. It never carries draft text, message
bodies, addresses or credentials, and a test asserts that.

`POSTINO_REDUCE_MOTION=1` forces reduced motion for one session (the persisted preference works
too, and "Riduci le animazioni" in the palette toggles it). Reduced motion sets
`style.animation_time` to 0: every animation resolves on its first frame. Screenshot mode
deliberately disables animations and never reads or writes this file.

Only the images regenerated by the commands above come from the harness. The older `postino-m0` and
UI-kit images predate it and are not reproduced by it. Regenerating a committed image invalidates
its SHA-256 in `ATTRIBUTION.toml`: update the affected `sha256` entries, run
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
