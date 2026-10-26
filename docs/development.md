# Development

> **Status:** M0 + U0.5 labels + P3c engine on the encrypted store + P4a MIME crate + P4b encrypted
> attachment blobs + P5a IMAP client + P5b sync into the store + P2b states/accessibility + P6/P7
> app, SMTP and outbox + P8 compose attachments, signatures and reply headers ·
> **Last updated:** 2026-10-26

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
cargo run -p postino-app                      # desktop shell, default encrypted store
cargo run -p postino-app -- --demo            # synthetic mailbox, labelled in the status bar
cargo run -p postino-app -- --store D:\tmp\mail.db
cargo run -p postino-cli -- --demo info
cargo run -p postino-cli -- --demo tools
cargo run -p postino-cli -- --demo mcp        # stdio MCP server
cargo run -p postino-cli -- --store mail.db info
# the mail commands need an onboarded account; a fresh store has none until `postino account add`
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
crates/platform/    OS seams: store key and account credentials in the OS keyring
crates/store/       encrypted SQLCipher store, schema v4, repository, encrypted blob directory
crates/sync/        one IMAP account into the store: folders, UIDs, MIME, blobs, status
crates/testkit/     test-only fake IMAP server, TLS fixture and synthetic mail
apps/postino-app/   egui shell (demo, store, onboarding, sync worker)
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
cannot read the file. The keyring is the only secret seam (store key and `account:<id>:imap`
credentials), and tests never touch it: every store test uses a random `StoreKey` over a temp
directory. **Windows Credential Manager is the only native keyring backend in this build**; on macOS
and Linux the platform seam returns `PlatformError::Unsupported` until native keychains land with
packaging.

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
to another message. `postino-sync` writes received attachment bytes through the seam; the reading
pane's Save action arrives with P6c.

What is still demo or missing:

- The desktop app boots the encrypted store by default and `--demo` keeps the synthetic mailbox for
  screenshots and trials.
- A fresh store has no accounts: the app opens onboarding, the CLI uses `postino account add`.
  `postino --store <path> info` answers on a fresh store and the mail commands report that no account
  is configured.
- One app process owns the store and its background sync and send workers open their own connections
  in the same process; SQLite WAL and the busy timeout serialise them, and the outbox lease, the append
  claim and their guarded transitions make two passes (app + CLI) safe row by row. Every lease and
  append claim is stamped when it is taken, so a row leased after an earlier row spent time in SMTP, or
  an APPEND that follows a long submission, still owns a full window. There is no daemon, pipe or
  cross-process lock yet (M4): do not copy or move the database while a command runs, and do not treat
  two clients as two independent mailboxes.
- IMAP, MIME, attachment blobs, sync, real accounts, SMTP submission, the outbox, compose depth and
  encrypted search (attachment bytes, per-account signatures, reply/forward headers, P5-P9a) exist.
  Still missing: threads, saved searches and rules (P9b/P9c), the daemon and a scheduled send that runs
  with the app closed (M4), HTML rendering (M5), server-side signatures and Gmail OAuth (P12).
  `message_send` queues real mail; the desktop send worker (which polls the store every two seconds)
  and `postino send` process the outbox, so a future `--at` row waits until one of them runs.
  A plain `postino send` (no `--at`) waits the 5-second undo window and submits in the same command.
  MCP only queues.
- Sync writes `uid`, `Message-ID` and the RFC 822 size; a local draft keeps them empty until the
  submission is accepted, then the worker writes the Sent copy with the generated `Message-ID` so the
  next Sent sync adopts it instead of duplicating it — including when the server copy was cached first:
  the merge adopts the cached UID and skips the APPEND. A Sent copy moved back to Drafts is excluded
  from adoption (only Sent/Archive rows are adopted), so the user can still edit it.
- The database file lives wherever `--store` points; Postino does not create the parent directory.

A quick manual check that persistence works (unit level, no keyring):

```sh
cargo test -p postino-cli cli_store_keeps_a_draft_across_a_reopen -- --nocapture
cargo test -p postino-core --test engine_store -- --nocapture
cargo test -p postino-core --test engine_blobs -- --nocapture
```

The send pipeline has its own fake servers; no keyring or real account is touched:

```sh
cargo test -p postino-net --test smtp
cargo test -p postino-core --test send -- --test-threads=1
cargo test -p postino-store --test outbox
cargo test -p postino-cli cli_send_queues_submits_and_reports_a_pending_sent_append
```

`postino-core --test send` includes a 25 MiB attachment case and four restart points; it takes about
40 s on the Windows GNU host. `--test-threads=1` keeps the fake servers' ports and assertions readable.

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
- **Store:** schema upgrade from v1, migration idempotence, the v6→v7 search index and inert tables,
  repository round trips, counts after read/move/delete, atomic batches, draft revisions across reopen,
  wrong key, schema-too-new and a no-plaintext scan over the database, WAL and SHM; plus the
  one-query-per-table mailbox hydration used by `Engine::open`.
- **Search:** migration v6→v7, an FTS5 capability probe (`contentless_delete=1`), insert/update/delete
  reindexing without flag churn, attachment names and diacritics, bm25 and date order, filters,
  pagination, hostile/metacharacter input, the 1 MiB body bound, drift and `search_reindex`, encrypted
  shadow tables with no sidecar, the query grammar matrix and `parse(print(ast)) == ast`, op/CLI/MCP
  parity, the headless search screens at 980×600 light/dark, and the 20k perf gate. The 100k fixture is
  `POSTINO_SLOW_TESTS=1 cargo test -p postino-core --test search_perf -- --ignored one_hundred_thousand_messages`
  and prints open/search/backfill numbers.
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
- **Compose depth (P8):** exact bytes through base64 → encrypted blob → draft → reopen → P7 outbox →
  MIME (SHA-256 equal), duplicate/traversal/reserved/bidi filenames, the 20-file/25 MiB-per-file/
  25 MiB-total caps, a 0-byte file and exactly 25 MiB, invalid/oversized base64, missing and corrupt
  blob refusals naming the file, forward bytes and a missing forward blob, per-account signature
  persistence and 4 KiB/control limits, the reply-all/Reply-To/Bcc/dedupe/subject/quote matrices,
  header round trip and the stale external-link refusal, a synced `Reply-To` in `meta`, CLI
  `draft attach/detach` and `account signature`, the MCP schema with `data_base64` and no `path`, and
  headless picker/drop/keyboard/980×600 light/dark UI tests. The review closure adds the pristine-reply
  Send/Save/Esc test, type→attach/remove→send/close body preservation, failed-forward blocking and
  recovery, stale two-engine attachment edits, valid MIME lies, a 25 MiB base64 capacity regression,
  GUI filename refusal, strip-drop dedupe, signature false-positive/byte-count/focus/close, demo
  local remove, large-parent save copy, metadata naming parity, bounded MCP lines and bounded CLI
  signature stdin. No test opens a native dialog or browser.
- **Platform:** keyring load-or-create against `keyring_core::mock` only; the native vault is a
  manual, tagged smoke test and never part of the default suite.
- **UI:** headless render with `Context::run_ui` and `PostinoApp::show`, including the gallery,
  composer, sidebar label registry, label filter, inline create/rename, the multi-select label
  undo path, the empty-state resolver and its three rendered filter states, contrast tokens and
  computed button hints, accessible row names and star targets, keyboard focus, the auto-read
  cancellation, the settings round trip and the plain/Shift J/K anchor. The test asserts the shell
  paints at least one shape. P6c adds the link scanner and its action-boundary recheck, IDN and
  dangerous-extension classification, exact attachment bytes, overwrite refusal then confirmation,
  cancel/missing blob, the keyboard Save path, the link sheet and its recorder, every settings
  section at 980×600 in both themes, focus return, the injected folder opener, one identity and one
  Retry, Cc layout, the stored remote-image line with no URL painted, and compose/reply identity. No UI test
  opens a browser, a native dialog or a shell.
- **Test infrastructure:** the fake IMAP/SMTP servers and TLS fixture live in `postino-testkit` and
  run in CI. Property tests for the network parsers and fuzzing for MIME remain future work.

### Desktop OS seams (P6c, P8)

The desktop shell is the only place that leaves Rust for a user action. Each exit is a trait in
`apps/postino-app/src/seams.rs`; production installs the real adapter, tests and screenshots install
an unavailable or recording one:

- `LinkOpener` — `webbrowser` in production; `RecordingLinkOpener` in tests. The UI allowlists the
  scheme in `reading::find_links`, shows the host/recipient line, and rechecks the URL again in
  `confirm_pending_link` at the opener boundary, so a recorder only ever sees an allowed URL the
  person confirmed.
- `FileSaver` — `rfd`'s native Save dialog plus an exclusive `write_new` (or `write` after the
  overwrite confirmation) in production; `RecordingFileSaver` in tests. The app classifies the
  sanitized suggested name, re-classifies the chosen path, checks overwrite itself and writes the
  exact blob bytes once. A failed new write removes its own partial file; an overwrite writes a
  sibling temp file and renames it, so a failed write cannot truncate the original.
  `CancellingFileSaver` is the default in the demo/screenshot constructor so a stray click cannot
  open a dialog; a real `--demo` launch also disables Save because the synthetic attachment has no
  bytes.
- `FilePicker` — `rfd`'s multi-file picker in production; `RecordingFilePicker` in tests and an
  unavailable picker in the demo/screenshot constructors. It returns paths only: the app bounds each
  file (metadata and bytes read, 25 MiB), reads it locally and hands the bytes to the engine, so the
  engine and MCP still accept bytes and never a path. A cancelled picker returns an empty list and
  writes nothing. `egui::DroppedFile` uses the same reader; the composer component itself never opens
  a file.
- `FolderOpener` — the platform file manager in production; `RecordingFolderOpener` in tests and an
  unavailable adapter elsewhere. It only receives the blob directory derived from the store path.

The CLI's `draft attach` error names the path the caller explicitly supplied; that is local command
output, not a log line. The desktop app names only the file in its toast. Neither path is included in
any `log::` call.

Keeping the seam at the action boundary, not inside the engine, is deliberate: the scanner, filename
rules and warnings stay pure and testable, and no native dialog, browser or shell runs in CI. The
only new dependencies are `rfd` and `webbrowser`, both in `apps/postino-app` only.

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
cargo run -p postino-app -- --screenshot onboarding_welcome --out docs/images/postino-p6b-onboarding-welcome.png
cargo run -p postino-app -- --screenshot onboarding_provider --out docs/images/postino-p6b-onboarding-provider.png
cargo run -p postino-app -- --screenshot onboarding_form --out docs/images/postino-p6b-onboarding-form.png
cargo run -p postino-app -- --screenshot onboarding_error --out docs/images/postino-p6b-onboarding-error.png
cargo run -p postino-app -- --screenshot onboarding_folders --out docs/images/postino-p6b-onboarding-folders.png
cargo run -p postino-app -- --screenshot onboarding_sync --out docs/images/postino-p6b-onboarding-sync.png
cargo run -p postino-app -- --screenshot mailbox --out docs/images/postino-p6b-mailbox.png
cargo run -p postino-app -- --screenshot offline --out docs/images/postino-p6b-offline.png
cargo run -p postino-app -- --screenshot onboarding_form --dark --out docs/images/postino-p6b-onboarding-form-dark.png
cargo run -p postino-app -- --screenshot onboarding_folders --dark --out docs/images/postino-p6b-onboarding-folders-dark.png
cargo run -p postino-app -- --screenshot mailbox --dark --out docs/images/postino-p6b-mailbox-dark.png
cargo run -p postino-app -- --screenshot offline --dark --out docs/images/postino-p6b-offline-dark.png
cargo run -p postino-app -- --screenshot mailbox --size 980x600 --out docs/images/postino-p6b-mailbox-980.png
cargo run -p postino-app -- --screenshot onboarding_folders --size 980x600 --out docs/images/postino-p6b-onboarding-folders-980.png
cargo run -p postino-app -- --screenshot reading_attachments --out docs/images/postino-p6c-reading-attachments.png
cargo run -p postino-app -- --screenshot reading_attachments --size 980x600 --out docs/images/postino-p6c-reading-attachments-980.png
cargo run -p postino-app -- --screenshot link_confirm --out docs/images/postino-p6c-link-confirm.png
cargo run -p postino-app -- --screenshot link_confirm --size 980x600 --out docs/images/postino-p6c-link-confirm-980.png
cargo run -p postino-app -- --screenshot settings --out docs/images/postino-p6c-settings.png
cargo run -p postino-app -- --screenshot settings --dark --out docs/images/postino-p6c-settings-dark.png
cargo run -p postino-app -- --screenshot settings --size 980x600 --out docs/images/postino-p6c-settings-980.png
cargo run -p postino-app -- --screenshot settings --size 980x600 --dark --out docs/images/postino-p6c-settings-980-dark.png
cargo run -p postino-app -- --screenshot compose --size 980x600 --out docs/images/postino-p8-compose-980.png
cargo run -p postino-app -- --screenshot compose --size 980x600 --dark --out docs/images/postino-p8-compose-980-dark.png
cargo run -p postino-app -- --screenshot settings_signature --size 980x600 --out docs/images/postino-p8-signature-980.png
cargo run -p postino-app -- --screenshot settings_signature --size 980x600 --dark --out docs/images/postino-p8-signature-980-dark.png
```

The states are `shell` (demo inbox, no selection), `reading` (first inbox message open),
`reading_full` (the same message with the pane expanded), `compose` (the composer prefilled with
the docs fixture and two attachments with the P8 total), `palette` (command palette open), `labels`
(Lavoro filter active, list visible),
`label_menu` (a labelled message open with the modal label menu above its captured target),
`label_editor` (the inline create editor open under the ETICHETTE header with a name typed),
`empty_unread` (the inbox with every message read and the unread filter on), `empty_attachments`
(inbox mail with attachments moved out and the attachments filter on), `empty_label_unread`
(Lavoro selected, its unread mail read, the unread filter on), `onboarding_welcome`, `onboarding_provider`,
`onboarding_form`, `onboarding_error`, `onboarding_folders`, `onboarding_sync`, `mailbox` (the shell
with a seeded updated-at state), `offline` (a cached message open under the offline banner),
`reading_attachments` (a deterministic sanitized message with one stored blocked remote image, a safe
link and one PDF attachment in the pinned strip), `link_confirm` (the same message under the
full-destination sheet, which shows the host/recipient line as well as the exact URL), `settings`
(the full settings dialog) and `settings_signature` (the same dialog with the per-account Firma editor
open and a seeded signature). The onboarding, mailbox, P6c, P6d and P8 scenes are deterministic
fixtures; they do not read a store or touch the network, and the P6c/P8 scenes install the
unavailable/cancelling seams so no native dialog or browser can open. The `reading_attachments`
message is **stored truth**: its HTML is already sanitized (no remote `src`, no tracker) and
`remote_images = 1` is the count sync would have written from the parser, so the capture neither
parses nor fetches anything live. `--dark` applies the dark theme.
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

Only `--screenshot <state>`, `--out <path>`, `--dark`, `--native-chrome`, `--size WxH`,
`--gallery`, `--demo` and `--store <path>` are accepted. Unknown flags, duplicate flags,
`--flag=value` forms, a missing value and a value that is another flag are all refused. `--demo` and
`--store` are mutually exclusive; a screenshot always uses the deterministic demo mailbox/none of
the user's data, so `--store` is refused with `--screenshot`. `--native-chrome` keeps the operating system title
bar; on macOS the native title bar is always used. Screenshots otherwise use the same chrome rule
as a normal startup.

## 9.1 Preferences

The app owns one small settings file, not eframe storage:
`%APPDATA%\Postino\settings.json` on Windows, `$XDG_CONFIG_HOME/Postino/settings.json` (or
`~/.config/Postino/settings.json`) elsewhere. It holds exactly six fields: theme (`dark`), the
reading-pane fraction of the width left by the sidebar (`reading_pane_ratio`), the last folder id
(`folder`), `reduced_motion`, the opaque active account id (`account`) and the schema `version`.
Missing, truncated, unknown-version or out-of-range values fall back to the defaults (light, 0.52,
`inbox`, motion on, no account) without a dialog. The file is written atomically (`.tmp` then rename)
immediately for theme, folder, reduced motion and account changes, at most once a second while a pane
is dragged and once on exit; renaming, deleting or corrupting it is safe. It never carries draft text,
message bodies, addresses or credentials, and a test asserts that.

`POSTINO_REDUCE_MOTION=1` forces reduced motion for one session (the persisted preference works
too, and "Riduci le animazioni" in the palette toggles it). Reduced motion sets
`style.animation_time` to 0: every animation resolves on its first frame. Screenshot mode
deliberately disables animations and never reads or writes this file.

Only the images regenerated by the commands above come from the harness. The older `postino-m0` and
UI-kit images predate it and are not reproduced by it. Regenerating a committed image invalidates
its SHA-256 in `ATTRIBUTION.toml`: update the affected `sha256` entries, run
`cargo xtask assets --write` to regenerate `ATTRIBUTION.md`, then run `cargo xtask assets` (or
`cargo xtask ci`) to verify the hashes.

## 9.2 Store, keyring and sync

A normal launch opens `<data dir>/mail.db` (the same default as the CLI) or the file named by
`--store`. The 256-bit SQLCipher key and every `account:<id>:imap` credential live in the OS keyring
under the `postino` service; the app implements the engine's `CredentialVault` over
`postino-platform`, so no password reaches a log, the settings file or the database. `--demo` keeps
the synthetic in-memory mailbox and is the explicit fallback on the recovery screen. A store with
accounts shows its cache immediately and starts one background refresh for the active account. When
the store, the keyring or the data directory cannot be opened, the app shows the path, the failing
stage and a retry; it does not start an empty mailbox.

Sync runs on one worker thread with its own store connection and its own
`Engine::open_with_vault`. The UI sends `SyncNow` / `Cancel` / `Stop`; the worker sends
`Started` / `Progress` / `Finished` / `Failed` and never calls egui. `Engine::sync_now` has no cancel
flag or progress hook, so the worker drives `postino_sync::sync_account` with the account row and the
same vault, and polls the persisted sync status for determinate progress. After a run the UI reopens
its engine from the store without clearing selection, filters or the open message; a connectivity
failure keeps the cached list and turns the offline state on, while a credential or store failure
shows its own reason with Retry. The tests cover a real fake-server onboarding, the test CA hook,
cancellation, progress, a second request, and reload preservation.

## 10. Style

- Rust 2024, `rustfmt` with `max_width = 150`.
- Clippy with `-D warnings`.
- No `unwrap`/`expect`/`panic!` in non-test code.
- Prefer small functions and plain data over trait hierarchies.
- Comments explain *why*, not *what*. Link to the relevant ADR or doc when a decision is subtle.
- Keep `postino-core` free of UI, filesystem and network dependencies until a crate exists for
  them.
