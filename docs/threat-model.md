# Postino threat model

> **Status:** M0–M6 surfaces: MIME/HTML, encrypted blobs, IMAP/SMTP/outbox, OAuth, daemon/MCP,
> PGP/MIME and calendar invites · **Last updated:** 2026-10-28 · **Review:** before every
> release that touches networking, storage, HTML rendering or MCP.

## 1. What we protect

| Asset | Why it matters |
|---|---|
| Message bodies and attachments | Personal and professional content, often confidential |
| Credentials and OAuth tokens | Full mailbox access; a leak is worse than a message leak |
| Contacts and metadata | Social graph, relationships, timing, subjects |
| Drafts and outbox | Unfinished work and accidental disclosure risk |
| Local encryption key | Protects the cache at rest |
| The user's identity and reputation | An agent or bug that sends bad mail is hard to undo |
| The device | Mail is a path to phishing, exploits and credential theft |

## 2. Adversaries and trust boundaries

| Adversary | Capability | Boundary |
|---|---|---|
| Network attacker | MITM, downgrade, DNS spoofing | TLS to IMAP/SMTP; certificate validation; no insecure fallback |
| Malicious sender | Crafted MIME, HTML, links, attachments | MIME parser, sanitizer, sandboxed renderer, no scripts |
| Local thief | Reads disk, backups, swap | Encrypted DB, encrypted blobs, keyring-held key |
| Malicious local process | Same user, tries the daemon pipe | Per-user pipe, per-launch token, owner-only token file |
| Malicious agent / MCP client | Calls tools, composes, sends | Explicit send opt-in policy, dry-run, rate limits, audit log, no network listener |
| Supply chain | Compromised crate, font, installer | `cargo-deny`, pinned lockfile, asset hashes, signed releases |
| Provider | Sees the mailbox anyway | Out of scope; documented honestly |
| Compromised OS / kernel | Reads memory, keylogs | Out of scope |

## 3. Invariants

1. **No plaintext secret on disk.** Credentials and the store key live in the OS keyring. Logs
   redact tokens and addresses.
2. **TLS is not optional.** There is no "accept invalid certificate" switch. A private CA can be
   added explicitly, per account, with a warning.
3. **No remote content by default.** Images, fonts, CSS and tracking pixels are blocked until the
   user allows them for a sender. Loading is per-message, not global-by-accident.
4. **No script execution.** SMTP does not carry scripts; the renderer never enables JavaScript.
5. **HTML never reaches the application's privileged surface.** A webview renderer is a separate
   sandbox with no app IPC, a strict CSP and a local-only protocol for attachments.
6. **Header injection is rejected.** Addresses, subjects and custom headers are validated for
   CR/LF and control characters before serialization.
7. **Attachment paths cannot escape.** Filenames are sanitized, traversal is rejected, and saved
   files stay under the user's chosen folder.
8. **Bounded input.** MIME nesting, part count, header size, decoded attachment size, compression
   ratio, HTML size and image dimensions all have caps. Decompression is streaming and bounded.
9. **The server is authoritative.** The client never deletes mail from the server unless the user
   asks; a sync bug must not be able to empty a mailbox.
10. **MCP cannot listen.** Stdio only. The daemon pipe is local, same-user and token-authenticated.
11. **Sending is explicit and auditable.** Every submission has an outbox row (account, recipient
    count, state, attempts, Message-ID and timestamps); bodies are not logged. MCP sending is off
    unless the server is started with `--allow-send`. An uncertain outcome is never retried
    automatically.
12. **Fail closed.** A missing keyring, unreadable store, invalid root or malformed config stops the
    operation with an error; it never falls back to plaintext or an unconfined path.

## 4. Threats and mitigations

### 4.1 Malicious email

- MIME parser (`postino-mime`, P4a): raw size, header block, part count and multipart depth are
  bounded before and inside the parser; decoded part size, total attachment bytes and HTML size are
  capped. `message/rfc822` parts are not expanded. Malformed structure is repaired and recorded as
  warnings. A deterministic mutation corpus runs in `cargo xtask ci`
  (`crates/mime/tests/no_panic.rs`); `fuzz/` adds libFuzzer targets for `parse`, the HTML
  sanitizer/text converter, PGP/MIME structure detection and `.ics` parsing. The security workflow
  runs each target for 120 s on Linux; Windows cannot host `cargo-fuzz` (no GNU libFuzzer build and
  no MSVC `link.exe` here), so the local CI keeps the deterministic corpus and the fuzz build stays
  a Linux gate. A 120 s run is a smoke gate, not a proof.
- HTML (`postino-mime`, P4a): ammonia allowlist, no `<script>`, `<iframe>`, forms, SVG/MathML,
  `style`, `class`/`id` or `on*`; `javascript:`, `vbscript:`, `data:`, `file:`, relative and
  protocol-relative URLs never survive. Remote images are removed and `http`/`https` sources are
  counted; `cid:` images survive. CSS is unsupported, not partially sanitized.
- Links: show the destination, warn on punycode/IDN homographs, never auto-open.
- Attachments: never auto-execute; warn on executables and macro-enabled documents; optional
  ClamAV integration later.
- Phishing: display SPF/DKIM/DMARC results from `Authentication-Results`, flag mismatches, but do
  not claim certainty.
- IMAP responses (`postino-net`, P5a): the server is untrusted input even after authentication.
  Lines are capped at 64 KiB, literals at 25 MiB by default (hard ceiling 64 MiB), untagged
  responses per command at 10 000, folders at 5 000, command lines at 8 KiB and the response for
  one command at 64 MiB. Nesting, atoms, flags, addresses and numeric digits have fixed caps; a
  number is parsed, never wrapped. A truncated literal, a missing `*`, a bad tag, `NIL` where a
  number is required and a negative number are typed errors, and a 54-case corpus runs in CI with a
  three-second bound per case. A timeout closes the session, so the next call cannot read the tail
  of a half-command. Whole bodies are fetched with `BODY.PEEK[]`, so a read never sets `\Seen`.
- IMAP commands (`postino-net`, P5a): mailbox names, user names and passwords are quoted with `"`
  and backslash escaping; CR, LF and NUL are refused instead of escaped, so no value can become a
  second command. A UID set is parsed, not expanded, so `1:*` on a huge mailbox is one range.
- Sync (`postino-sync`, P5b): reconciliation is read-only against the server. Bodies are fetched only
  with `BODY.PEEK[]`, so reading never sets `\Seen`, and the reconciler never sends `STORE`,
  `EXPUNGE`, `COPY`, `MOVE` or `APPEND`; the scripted server answers those with `BAD` so a regression
  fails the suite. Attachment bytes are accepted only for a part the MIME parser decoded completely
  and whose decoded size is within `max_attachment_bytes` (default 25 MiB); a larger or truncated
  part is kept as metadata with no bytes and a warning. The effective single-part cap is the smaller
  of that limit and the MIME crate's 32 MiB hard part limit; the blob format refuses anything above
  64 MiB. Attachment names come from the server, are sanitized by the MIME parser before they become
  row data, and are reduced to printable text and 80 characters before they enter a warning. Sync
  opens no file and never launches a viewer: an attachment is bytes in the encrypted blob store
  until the user asks for it. Message bodies are parsed with the sync limits, HTML is sanitized on
  receipt and remote images are removed, and a malformed message becomes a warning rather than a
  failed run. The status record (`meta` key `sync_status:<account_id>`) carries counts and a fixed
  failure sentence, never a server name, an address, a credential or message content; the typed
  `SyncError` returned to the caller may name the host, exactly as a session error does. Logs carry
  account and folder ids and counts, never subjects, addresses or bodies.
- Compose attachment intake (P8) is the other byte path: `draft_attachment_add` bounds the base64
  string before the decode (the JSON string itself is already allocated by the transport), decodes at
  most 25 MiB, checks the per-file and 25 MiB total caps,
  sanitizes the name (controls, traversal, bidi/format controls, Windows device names) and accepts
  the declared MIME only as a hint that must agree with the extension table. It never opens or
  executes a file, never inspects the bytes to
  guess a type, and stores only a content hash and the encrypted blob. A missing or corrupt blob is a
  send-time refusal naming the file, not a silent drop. Removing a row does not delete a blob another
  draft may share; the unreferenced sweep remains P9 work and is documented as such.

### 4.2 Account compromise

- OAuth2 PKCE with loopback redirect; no implicit flow, no password storage for OAuth providers.
- App passwords are supported for providers that require them (Aruba), stored in the keyring.
- Tokens are refreshed in memory; refresh tokens are stored encrypted in the keyring.
- A failed login is a readable error, never a silent retry loop that locks the account.
- TLS is mandatory (`postino-net`, P5a): implicit TLS or STARTTLS, and no configuration that skips
  certificate verification. Verification uses the operating system's verifier through
  `rustls-platform-verifier`; extra trust anchors exist for tests and only add to it. A missing
  `STARTTLS` capability stops the session before a credential is written, and plaintext already
  buffered after the STARTTLS `OK` is treated as a command-injection attempt and refused. A
  `LOGINDISABLED` capability is a clear `Auth` error, not a retry.
- Passwords (`postino-net`, P5a): held in a `Secret` that zeroises on drop and prints `[redacted]`,
  dropped after a successful login, and written only inside `AUTHENTICATE PLAIN` or `LOGIN`; command
  lines and SASL payloads live in `Zeroizing` buffers. Logs carry tags and command verbs, never
  arguments, credentials, addresses or bodies. Authentication errors say what failed, never echo
  the password.
- **Keyring scopes (P6a/P7).** Service `postino`; scope `default` holds the store key,
  `account:<id>:imap` holds one account's IMAP credential and `account:<id>:smtp` an optional separate
  SMTP credential. `postino-platform` owns every read, write and delete, takes the shared keyring lock
  for the whole operation, reads a written value back before reporting success and returns
  `Zeroizing<Vec<u8>>`. When the SMTP scope is absent the engine uses the IMAP credential for
  submission and never copies one secret into the other's scope. The account id is restricted to
  ASCII letters, digits, `_` and `-`, so a crafted id cannot merge two scopes with a `:`.
- **No password in the database, logs or MCP.** `account_servers` has no password column; the
  operation result `AccountView` has no secret; `SyncStatus` carries a fixed error sentence, counts
  and timestamps only. `account_add`, `account_update` and `account_remove` are `sensitive`: the MCP
  server filters them out of `tools/list` and refuses `tools/call` before parsing arguments, so a
  password in a crafted call is never echoed. The CLI reads the password from a no-echo prompt or one
  `--password-stdin` / `--smtp-password-stdin` line; there is no `--password` argv flag, and the
  generic `postino run` refuses a sensitive operation so JSON arguments cannot put a password in
  shell history. The JSON bridge inside the engine keeps one transient copy of the password for the
  duration of the call; it is dropped immediately and never formatted, but it is not zeroised (see
  the swap/crash-dump entry).
- **Account add writes nothing on failure.** The IMAP login and `LIST` and the optional SMTP login run
  before any durable write. The credentials are stored first and the account, server and folders
  commit in one transaction; if that transaction fails, both credentials are deleted again. The
  remaining window is an orphan credential, which a retry with the same address reuses, not an account
  that can never authenticate.
- **Account remove order.** The local cache is deleted first and both keyring entries second. The only
  failure window leaves an orphan credential, reported and overwritable by adding the same address
  again; it never leaves cached mail whose password is gone. `confirm: true` is required, and a failed
  cache delete aborts before the keyring, leaving the account usable.

### 4.3 Local disk theft

Exactly what is encrypted, and with what:

- **The database** (`<path>`): SQLCipher encrypts every page and WAL frame, and the temp store is
  configured in memory so temp tables never reach disk. Subjects, addresses, bodies, flags, labels,
  folder names, draft revisions, attachment names/types/sizes and their blob hashes are ciphertext on
  disk. The file begins with a random per-database salt, not `SQLite format 3`. Sync adds the cached
  server UIDs, the local cursor and the status JSON (`sync_status:<account>`) to the same encrypted
  pages; the status contains counts and a failure category, not the server's name or any content.
  The `account_servers` row (host, port, TLS mode, username) is ciphertext too; there is no password
  column anywhere in the schema.
- **Attachment bytes** (`<db file>.blobs/<xx>/<hash>.blob`, for example `mail.db.blobs/…`):
  XChaCha20-Poly1305 under a subkey
  derived with HKDF-SHA256 (salt `postino.v1`, label `blob`) from the same 256-bit store key. The file
  is `PSTB`, version 1: magic, version, flags, original length, a 24-byte random nonce, and the
  ciphertext with a 16-byte tag. The AAD binds magic, version, flags, plaintext length and the BLAKE3
  hash of the plaintext, so a file cannot be renamed to another hash, swapped with another blob,
  truncated or extended. zstd compression happens before sealing and is authenticated by the flags
  byte; decompression is bounded by the declared length, which is checked against the caller's cap
  before decryption.
- **The master key**: 32 random bytes, generated on first run and kept in the OS keyring (Windows
  Credential Manager under service `postino`). Postino never writes it to a file, a config or a log.
  `StoreKey` and the derived `Subkey` redact `Debug` and zeroise on drop. The store crate never reads
  the keyring; a wrong key fails closed as `WrongKeyOrCorrupt` (database) or `CorruptBlob` (blob),
  with no plaintext fallback.
- **Explicit clearing.** A recovery phrase is an exportable feature, not a default; there is none
  yet, which means a lost keyring entry is lost mail. Key rotation does not exist: rotating the store
  key would leave every existing blob unreadable.

What is **not** encrypted, and the consequence:

- **The blob header.** Magic, version, flags, original length and nonce are clear so a reader can
  validate and bound a file before it allocates or asks for the key. Someone who can list the folder
  learns the approximate size of each attachment, the fact that compression was used, and how many
  distinct attachment contents exist. They learn no names, types, addresses or content.
- **The blob file name.** It is the BLAKE3 hash of the plaintext. That leaks equality (the same
  content stored once is visibly one file, shared by every message that carries it) and allows a
  *content-confirmation* attack: an attacker holding a candidate document can hash it and learn
  whether that exact document is in the mailbox. It does not reveal the content or the key. Encrypting
  the name too would break dedup and directory listing, which is a deliberate trade.
- **Swap and crash dumps.** Keys and decrypted content are in process memory while Postino runs. The
  OS may page that memory, include it in a hibernation image or write a crash dump, and a disk thief
  can read that. Postino does not lock pages, disable paging or promise to. Full-disk encryption and
  an OS that does not dump secrets are the mitigations, and they are the user's.
- **SQLCipher's internal key copy.** The connection keeps its own key schedule and page cache; Rust
  code cannot zeroise it. `postino-crypto` also keeps three transient copies that Rust cannot wipe:
  the `Hkdf<Sha256>` value holds the PRK-derived HMAC state until it drops, the cipher expands the
  subkey for the duration of a call, and a *rejected* ciphertext leaves a decrypted buffer to the
  allocator before the tag check fails. None of them is written to disk by Postino, logged or
  returned, and closing them would mean a custom in-place AEAD construction and hand-rolled HMAC.
- **Orphaned blobs.** A crash between writing a blob and committing the attachment row leaves an
  encrypted, deduplicated file with no row pointing at it. There is no garbage collector yet; a
  deleted message also leaves its blob because another message may share the content. Removing an
  account deletes its rows but not those files: the bytes are encrypted and deduplicated, and the
  sweep belongs to P9/the daemon. A disk thief therefore still sees encrypted bytes for attachments
  that were removed, but cannot read them.
- **Backups.** A backup of the folder copies the database and the blobs. It is unreadable without the
  keyring entry, and there is no passphrase export yet, so the keyring entry is a single point of
  failure for restores.

The disk-level guarantee is therefore: **no message content and no key material is recoverable from
the files Postino writes** (the database and the blob files), while metadata shape (attachment size
class, content equality, file count) is visible to someone who can read the folder without the key.
The guarantee does not extend to memory the operating system may have paged out; see the swap and
crash-dump entry above.

### 4.4 Malicious agent

- MCP is opt-in. The default posture is read + draft; `message_send`, `send_schedule`,
  `outbox_retry` and `outbox_mark_sent` are listed as disabled and refused unless
  `postino mcp --allow-send`. The server only queues: it never runs the outbox pass.
- `send_dry_run` returns the exact RFC 822 bytes and the envelope recipients without queueing.
- Idempotency keys make a retried queue operation safe: the key is stable per draft content and a
  partial unique index makes a second queued/sending row for one draft impossible. The outbox and the
  100-recipient/40 MiB caps bound a runaway loop; a per-call rate limit and audit log are M6 work.
- The agent never receives credentials; it receives operations. `account_add` and `account_remove`
  are `sensitive`: they are absent from `tools/list` and `tools/call` refuses them before parsing the
  arguments, so an agent can neither create an account nor ask for a password to be stored.
  `account_test` uses the existing keyring credential and returns only a folder count and
  capabilities; `sync_status` and `accounts_list` never include a secret.
- Tool errors are structured and actionable. An agent cannot use error text to probe paths outside
  its roots.
- `draft_attachment_add` accepts `data_base64`, never a path: an MCP client can store the bytes it
  already holds, but it cannot make the server read a file, a directory or another root. The base64
  length is checked against the 25 MiB decoded cap **before** the decode, an invalid alphabet is
  refused, and the decoded size is checked again; a malformed declared MIME is not copied into a
  header, and the filename is sanitized by the same rule as the serializer. The bytes go only to the
  encrypted blob store. `account_signature_set` is plain text, ≤ 4 KiB, and never a credential.
- P8's `Reply-To` cache lives in the existing `meta` table, keyed by local message id, and is removed
  with its message or account. It is header text, not a body or a credential; it is never logged.

### 4.5 Supply chain

- `Cargo.lock` committed; `cargo deny check` in the security workflow covers licences and RustSec
  advisories. The workflow also runs the libFuzzer targets; it is a Linux gate.
- Minimal dependency surface; no npm in M0. If a webview renderer arrives, it is an OS component,
  not a bundled browser, and it gets its own sandbox review.
- Fonts and logos are hashed and attributed; `cargo xtask assets` fails on drift.
- Release artefacts are signed; Windows and macOS installers are code-signed and notarized.

### 4.6 Data loss

- Saves are atomic: write a temp file, fsync it, then publish (rename or hard link).
- The outbox is a journal: the exact bytes are stored before the network, a lease/attempts record
  survives a crash, and an uncertain outcome is surfaced rather than replayed. The Sent APPEND is
  retried independently and never resubmits.
- Drafts keep revisions and recover after restart.
- A sync bug must never delete server mail. Expunge is explicit. `postino-sync` never issues a
  mutating command; its deletion pass only removes rows from the local cache after a complete UID
  list, and it skips that pass when a run stopped at the message limit.
- Sync resumes without gaps: the per-folder cursor advances only after a message transaction commits,
  so a crash re-fetches one message at most. A `UIDVALIDITY` change discards that folder's cache but
  keeps local drafts, which were never on the server.
- Backups are the user's responsibility, but export/import is a first-class feature.

### 4.7 SMTP submission and the outbox (P7)

- **TLS is mandatory.** SMTP uses implicit TLS or STARTTLS with the same OS verifier and no skip
  switch. A STARTTLS mode whose server does not advertise the extension fails with `Unsupported`;
  plaintext bytes buffered after the `220` are refused as a command-injection attempt. A wrong
  hostname fails certificate verification, proved by a test that connects to `127.0.0.1` with a
  `localhost` certificate.
- **The password is never in the message.** `SmtpConfig`'s `Debug` redacts host identity and password,
  `Secret` zeroises on drop and prints `[redacted]`, the SASL payload and `AUTH LOGIN` lines live in
  `Zeroizing` buffers, and no log line carries a command argument. The fake-server transcript test
  asserts the plaintext password appears in no command line and no body line is recorded as a command.
- **Bounds.** 8 KiB per reply line, 64 KiB per reply, 40 MiB per message, 100 recipients, a timeout
  for every phase, no bare LF and no bare CR (a `\r` must begin a CRLF), no NUL, dot-stuffing exactly
  once. `SIZE 0` is "no fixed maximum" per RFC 1870, not a zero-byte ceiling. The body and the APPEND
  literal are written in chunks with one 30 s timeout per chunk, so a dead connection is caught
  quickly, and the whole transfer has a 30-minute budget (`MAX_BODY_TRANSFER`), so a trickle is bounded
  too. A hostile server cannot make the client allocate an unbounded reply or body.
- **A live transfer can never be expired as a crash.** The submission lease and the append claim are
  both 45 minutes, strictly longer than the 30-minute body budget: a transfer that would exceed the
  budget is aborted by the client, so the journal never marks a *running* submission `uncertain` (which
  would invite a duplicate manual retry). Each lease and each append claim is stamped with the instant
  it is taken, not with the start of the pass: a later row in a batch and an APPEND that follows a long
  submission each get a full window. A 40 MiB message needs about 23 KiB/s to fit.
- **No server text is stored.** A permanent or transient failure stores a fixed typed sentence (stage
  and code only) in the encrypted outbox row; the APPEND failure does the same, and `SmtpError::Auth`
  and `SmtpError::Rejected` themselves carry no server text. A hostile SMTP or IMAP server has received
  the password (and, at DATA, the whole body) and can echo either in a `NO`/`550`: none of that text
  reaches `outbox.last_error`, `outbox_list`, the CLI stdout or an MCP result. Tests at RCPT, DATA-end
  and APPEND assert the password, the body and the recipient list are absent from every one of them.
- **The uncertain window.** Once the final `.` is written, a refusal, a timeout, a close or a
  malformed reply is `Uncertain`: the server has the bytes, so the client never guesses. The row is
  never retried automatically; the UI says “Invio incerto: controlla in Inviati” and offers a manual
  retry (which may send twice) or “Segna come inviato” (a local decision that proves nothing). This is
  the deliberate trade for never silently duplicating a message or losing one without a record.
- **Concurrent processes cannot duplicate a delivery or a Sent copy.** The `draft_id` partial unique
  index plus a content/revision idempotency key make a second active row for one draft impossible; a
  lease change is conditional on the owner; the Sent append has its own claim, stamped when the `250`
  arrives, so two passes cannot APPEND the same row at once; `outbox_mark_sent` refuses if a concurrent
  retry changed the row first. A stale engine also cannot overwrite a Sent copy: an edit or move
  verifies the store's current folder for the id and is refused with “changed in another session”, and
  a Sent copy moved back to Drafts is never adopted by a Sent sync (only Sent/Archive uid-null rows
  are).
- **Crash windows.** A row is leased to `sending` with its own `lease_at` in a transaction before any
  network call; a crash leaves `sending`, and after the 45-minute lease it becomes `uncertain`, never
  queued. Every post-lease transition is conditional on the lease still being the worker's, so two
  processes cannot overwrite each other. A crash between the server's `250` and the durable
  `append_pending` write is the narrow window that a test seam fails on: reopening turns the row
  `uncertain` and opens no connection. A crash after `append_pending` retries only the APPEND. The IMAP
  APPEND itself has no idempotency token: a crash after the server stored the literal but before its
  tagged `OK` can leave a duplicate server copy. P7 documents that protocol limit; it does not claim
  atomic APPEND.
- **Credentials.** IMAP under `account:<id>:imap`; optional separate SMTP under `account:<id>:smtp`;
  absent SMTP falls back to the IMAP credential. Both are in the OS keyring only. `account_add`
  probes both servers before writing and deletes both on a failed store transaction; `account_remove`
  deletes both after the cache. The database has no password column, and the outbox has no envelope
  address list beyond the encrypted JSON.
- **MCP send toggle.** `message_send`, `send_schedule`, `outbox_retry` and `outbox_mark_sent` are
  refused and listed as disabled unless `postino mcp --allow-send` was passed at start. `send_dry_run`,
  `outbox_list` and `outbox_cancel` stay available; a dry run is read-only and a cancel discards a
  queued or permanently failed submission without sending. The MCP server only queues: it never runs
  the outbox pass, so a queued message leaves only when the desktop app or `postino send` processes it.
  There is still no rate limit or audit log beyond the outbox row: M6 adds the audit log and a compact
  tool mode, and P8/P9 add richer guardrails.
- **Demo is honest.** The demo mailbox has no SMTP endpoint; Send is disabled with a visible reason
  and `message_send` refuses. A demo send never claims to leave the machine.

### 4.8 Search and the FTS index (P9a)

- **The index is in SQLCipher.** FTS5 and its shadow tables live in the encrypted database; there is
  no sidecar file and no plaintext copy. `VACUUM` is forbidden because FTS keys on `messages.rowid`.
- **Contentless index.** The index stores terms, not content; snippets are computed in Rust from the
  plain `body_text` row, are capped at 240 characters and strip bidi/format controls so a hostile
  subject or body cannot spoof the row order. `body_html` is never read by search, so a sanitizer bug
  cannot be re-opened through a search result.
- **Bound, escaped MATCH.** Core quotes every value as an FTS5 phrase (doubling `"`) and the store binds
  it as `MATCH ?`; label/folder names resolve to local ids in core and folder names are resolved inside
  the requested account. No caller text is concatenated into SQL or a MATCH expression. A syntax error
  is an error result, not a crash or a query that runs.
- **Bounded work.** Query 1024 B, 64 clauses, depth 8, 16 terms; 200 hits, offset 5 000; at most 16 384
  characters of body per snippet (≤ 64 KiB of UTF-8); 1 MiB of body indexed per message. A hostile
  message cannot grow an index row without bound or make one search allocate message-sized buffers.
- **Privacy.** Query text, subjects, addresses and snippets never enter a log. The UI shows the user's
  own query and positional error; the CLI/MCP error names the error code and position, not the query.
  Search results are local; no request leaves the machine.
- **Repairability.** The index is derived data (`meta.search_index` + 500-row batches, plus a rank
  table maintained by the same triggers). `search_status` compares missing and extra rowids in both
  the FTS and rank tables (same-count drift included), records a failed build's `last_error`, and the
  database+WAL size; `search_reindex` rebuilds from the encrypted rows. Opening a pending index does
  not block mail; the shell advances batches. `search_reindex` is a single-process maintenance action:
  there is no cross-process advisory lock, so do not run it from the desktop and MCP at once. A failed
  v7 migration rolls back to v6. P9b owns the `threads`, `thread_aliases` and `saved_searches`
  tables; P9c owns `rules`, `rule_runs` and `rule_actions` (§4.10).
- **Residual.** SQLite's row store keeps a 2 KiB body inline in `messages`; a covering summary index
  keeps those pages out of the open scan, but 100k summaries still take 268–357 ms to load and decode in
  the measured debug build. The app holds summaries and a small body cache, not a second index. The
  open target is missed and reported; the RSS target is not established (the 154.7–156.5 MiB figure is the
  seeding test process's high-water mark, not an opened mailbox).

### 4.9 Conversations and saved searches (P9b)

- **Untrusted references.** `References` and `In-Reply-To` are bounded before they become cache keys:
  one angle-bracket pair is trimmed, controls are refused, each reference is at most 512 bytes and the
  chain is capped at 32 entries, deduplicated, and a self-reference is ignored. Alias chains are
  bounded at 16 with a visited set, so a damaged `thread_aliases` row cannot make resolution loop. All
  values are bound; no reference text is interpolated into SQL.
- **Merge safety.** The cache update, the message moves, the alias and the row deletion are one
  transaction. A merge into the smallest id is deterministic for the same candidate set; a late
  reference that resolves to a deleted id follows the alias. An empty thread row is deleted rather
  than left as a stale conversation, and a submitted draft refreshes its old draft thread in the same
  transaction, so a send cannot leave a ghost conversation. A message with more than 500 members
  refuses `thread_flag` instead of changing part of it; the desktop pages a thread from the store for
  bulk actions and refuses over 500 with a sentence. A partial `message_move` batch keeps its undo
  toast for the rows that moved and reports the failure separately.
- **Subject fallback.** Only a message with no references and no parent, a reply/forward prefix, a
  normalised subject of at least 8 bytes, and exactly one thread inside the 14-day window attaches.
  Two matches drop the fallback; subject text is never used to merge two existing threads, so a
  generic "Re: update" cannot fuse unrelated conversations. A no-reference message that already has a
  thread keeps it on re-fetch, so a later second same-subject thread cannot detach it.
- **Saved queries are local metadata.** Names and query text live in SQLCipher; at most 12 per account,
  unique ignoring case, and the query is parsed and resolved before it is stored. The cap and the
  insert share one immediate transaction, so two processes cannot both create the 13th. Update and
  delete validate the owning account in a multi-account mailbox. A query whose label or folder
  disappears is shown as invalid with a reason and no counts, never silently broadened; a pending
  index is a separate `index_pending` state, not an invalid query. The eight recent queries live in
  `meta.recent_searches:<account>` inside the encrypted database, only explicit commits are recorded,
  and an explicit clear command removes them. **Deleting an account deletes its recent-search meta
  key**; the desktop clears its saved/recent rows before showing the remaining account. No query,
  subject or address is logged; saved-search errors name the stale label or folder to the caller but
  never reach a log.
- **Counts are derived.** `total`/`unread` use the same bound search path as `search`, capped at
  10 000, and are cached only by engine revision. A pending index yields zero counts with
  `index_pending:true` instead of a wrong number or a false “delete it” instruction.
- **Cache writes and indexes.** A read-only store is never repaired: `Engine::open` skips the orphan
  draft and thread-cache builds, and a thread read on an empty read-only cache returns an actionable
  “open read-write once” error. Normal open creates the derived indexes idempotently; `user_version`
  stays 7.
- **Residual.** A conversation folder view at 100k crosses the covering message index (356–359 ms P95
  on the debug host) and misses the 100 ms budget; it is reported, not hidden. Mail cached by P9a
  keeps its P9a placeholder thread ids permanently in this package (there is no full re-derivation
  operation), and pre-P9 `subject_norm` values were not normalised with the prefix stripper. The
  one-time P9a cache build adds ~2 s to the first open of a 100k store and nothing to later opens.
  Thread-row participants/labels are bounded to 2 000 message rows and 4 000 label rows per page.

### 4.10 Local rules (P9c)

- **A rule can change or forward mail, so it is treated as a stored plan, not a query macro.** The
  condition is parsed with the same bounded search grammar, and action targets are resolved against
  the current account on save. The save path refuses 0 or >5 actions, a duplicate kind, read+unread,
  virtual/Drafts/Sent move targets, unknown or other-account labels, and forward with 0 or >10
  recipients. A target deleted later is visible as `valid:false` and fails that action at run time
  with a fixed sentence; it never falls back to a broader folder or a different label. A rule is
  enabled or disabled, never silently applied after the user turned it off. Names are unique with
  Unicode lowercase via `rules.name_key`, not only SQLite's ASCII `NOCASE`.
- **Candidates use message provenance, not just the current folder.** `messages.origin` is
  `received`, `sent`, `draft` or `local`, written when the row enters the store and by the draft→Sent
  transition; a later move never rewrites it. Rules require `origin = 'received'` and a folder that
  is not Drafts, Sent or Trash, so an archived Sent copy cannot be labelled or moved as received mail.
  The rule's `created_at` is still a horizon: `ingested_at >= rule.created_at`, so a rule created
  today cannot rewrite mailbox history. A `new` run also requires `rules_applied_at IS NULL`.
- **A failed action is not consumed.** A `new` message is marked applied only when every action
  completed; an action failure (stale target, missing SMTP endpoint, concurrent change) or a deferred
  forward leaves it pending for the next run. The final message is eventually handled after the user
  fixes the endpoint or target, instead of silently disappearing from the new-mail pass.
- **The backlog advances and the forward budget is honest.** `all` reads at most 2 000 candidates per
  rule per page and stores an encrypted per-rule cursor. A full page advances it, a page ending in a
  deferred forward keeps it for a retry, and a short page clears it so the next run wraps. The caps are
  bound in the store query, not only in the UI. Repeating `all` cannot duplicate a move/label/flag; a
  forward is deduped by both the deterministic `rule:<rule>:<message>` outbox key and a durable
  encrypted `rule_forward:<account>:<rule>:<message>` marker that survives outbox pruning. Only a new
  outbox row consumes the 50-per-run budget; over-budget forwards are reported as deferred, not as
  errors and not as applied, so the 51st message is forwarded by the next run.
- **Immediate actions are atomic enough to retry.** A move, label, flag or trash action is a single
  store transaction that re-reads origin and the expected folder, so a concurrent move to
  Sent/Drafts/Trash is skipped rather than overwritten. A failing action increments the run's error
  count and aborts only that rule's remaining actions for that message; later rules still run. The
  error list is capped at 200 sentences plus an omitted-count line while the run keeps the exact
  count. `sync_now` never fails because a rule failed.
- **A concurrent editor cannot silently overwrite a rule.** `rules_update` reloads the store row,
  refuses a stale `updated_at` with “changed in another session”, validates the merged action list and
  writes with `WHERE updated_at = expected`.
- **Forward is P7, not a second SMTP client.** The action serializes through `postino-sync` and
  `postino-mime`, stores the exact bytes in the encrypted blob store and inserts one outbox row; it
  never opens a socket. The row has no local draft, so the submission path skips the draft finalize.
  The message carries `Auto-Submitted: auto-forwarded` and `X-Postino-Rule: <rule_id>`. Incoming mail
  carrying either header is recorded in the encrypted `meta` key `rule_guard:<account>:<message>` by
  sync and is skipped for forwarding; a guarded message does not consume the 50-forward budget. A
  missing SMTP endpoint fails the action with a readable sentence instead of queueing an undeliverable
  row. MCP gates `rules_run` behind `--allow-send`, refuses a forward payload in rule create/update
  before parsing, and refuses an enable-only update that would arm a stored forward rule. The same
  `allow_send` is carried into `sync_now`, whose automatic rule pass records a forward as deferred
  instead of queueing it; the sync still succeeds.
- **The journal cannot leak mail.** `rule_runs` holds account/mode/counts/times. `rule_actions` holds
  run/rule/message ids, the action name, a changed boolean and a JSON value of folder ids, label ids
  and booleans, plus the post-action folder/unread/starred/label state used by undo. It never stores a
  subject, an address, a query, a body or an attachment name. Run errors name rule/message/target ids.
  The newest 20 non-dry runs per account are kept; deleting a run cascades its actions and the mode
  and undone meta keys. Deleting an account deletes its rules, runs, actions, forward markers and
  guard meta, including orphan guard keys; a bulk sync deletion and a folder reset sweep the guards of
  the rows they remove.
- **Undo is state-checked and one-way.** The last journaled post-action state of each message is
  compared with the live state before anything is reversed. A message moved, flagged, starred or
  labelled after the run is skipped entirely, not partially rewound. A forward is never undone: the
  undo result reports the outbox id, and cancelling that row remains an explicit outbox action. A run
  is marked undone in encrypted meta; a second undo is refused. `rules_undo` is annotated destructive
  because it moves, flags and labels mail and has no redo. Undo does not resurrect a deleted folder:
  the reverse move fails and is counted as skipped.
- **Bounded rules.** ≤100 rules per account, 1–5 actions, 1–10 forward recipients, 2 000 candidates
  per rule per page, 50 new forwards per run, 20 retained runs, 200 returned error sentences. Rule
  names and actions live only in SQLCipher; no query, subject, address or body is logged. The only
  network path a rule can reach is the user's own SMTP server through P7, and only after the explicit
  run (MCP: `--allow-send`).
- **Residual.** A local rule is not a server filter: P10a's daemon runs the P9c rules after each
  successful sync while it is running, so a rule acts with Postino closed but only while the daemon is
  alive; it still does not apply to a phone or another mail client. Forwarding can still reach an
  external address the user configured; the loop guard only stops this client's own forwards and
  messages that declare `Auto-Submitted` or `X-Postino-Rule`. `all` reads 2 000 candidates per page, so
  a backlog larger than that needs repeated runs; the cursor advances and wraps, but a single run is
  still bounded. Forward recall is out of scope. A rule target deleted and later re-created with the
  same id is treated as the same target, as ids are never reused by this build.

### 4.11 Daemon, IPC and the single writer (P10a)

- **Token file.** `<store>.token` is 32 random bytes, rewritten at every daemon launch, with mode 0600
  on Unix and a one-ACE DACL for the current user SID on Windows. The client refuses a file that is not
  owner-only. The token is never in argv, environment, a log line or `daemon_status`; a wrong token is
  the same generic refusal as a missing one. Residual: any process already running as the same user can
  read the file, connect and act as that user. That is the trust boundary, not a new escalation: the
  same process can read the store key from the keyring and the database itself.
- **Pipe/socket.** Windows creates the named pipe with an explicit DACL (current user only) and
  `reject_remote_clients`; Unix binds a 0600 socket inside a 0700 per-user directory. The endpoint name
  is the user id plus a hash of the canonical store path, so two stores never share a channel. The
  store path is canonicalised before the lock, token and endpoint are derived; a hard-linked database
  is refused because two names would otherwise have two independent locks. On Linux the accepted socket
  peer's uid is checked with `SO_PEERCRED`, and a pre-created/symlinked fallback runtime directory is
  rejected. There is no TCP, WebSocket or remote endpoint variant. Residual: a same-user debugger can
  attach to the daemon process; operating-system process isolation is out of scope.
- **Framing and queues.** Both directions cap a frame at 16 MiB. The reader drains an oversized line
  without storing it, a writer refuses to emit one, and an oversized response becomes a same-id
  `FRAME_TOO_LARGE` error without closing the connection. Eight in-flight requests and a 64-value
  write queue per connection, 16 dispatches globally, 32 connections, one in-flight mutation per
  client, at most four subscriptions per connection and 64 globally, a 64-event subscription window,
  a 10 s handshake timeout, a 120 s idle timeout that applies only when no work/subscription is live,
  a 15 s heartbeat while work is live and a 30 s write timeout for a stalled reader. A client cannot
  hold a slot by sending a frame slowly forever or by opening unlimited subscriptions; it cannot make
  the daemon allocate the frame twice or queue an unbounded backlog.
- **Operation dispatch.** `op` is the same registry, schema and `deny_unknown_fields` parser as the CLI
  and MCP. Sensitive account operations are refused for `client = "mcp"` at the daemon too, in addition
  to the MCP tool filter. A per-session `allow_send` flag is carried in the handshake, so an MCP
  `sync_now` cannot queue a rule forward without `--allow-send`; the client-side gate fails closed when
  a rule cannot be inspected. Request ids are namespaced by connection; a client cannot cancel another
  client's work, and a disconnected request is cleaned up by RAII. Every request is bounded before the
  engine lock; the engine mutex serialises one writer and the scheduler, and a panic in a handler
  becomes an internal error, not a daemon death.
- **Cancellation and shutdown.** `cancel` sets a cooperative flag checked by sync before each message,
  between batches and between folders; it cannot interrupt an in-flight SMTP `DATA` or bypass a write
  that already committed. Any token holder can call `daemon_shutdown`; that is the user's authority,
  not a privilege boundary. The daemon stops accepting, cancels subscribers, drops the writer lock and
  leaves every outbox row in the store.
- **Scheduler/GC.** The scheduler sleeps to the next due instant; no busy loop. One bounded GC batch
  rechecks `attachments.blob_hash` and `outbox.raw_blob_hash` before each unlink, so a draft, a sent
  snapshot or a forwarded attachment cannot be collected. A failed sync/GC is retried after a delay,
  never in a tight loop.
- **Logs and status.** Logs and `daemon_status` carry counters, pids, categories and the store root;
  never a token, body, subject, address, query or credential. `last_error` is the category sentence,
  not raw server text.
- **Out of scope here.** Tray, autostart and notifications are P10b; this build makes no claim about
  them and installs no OS integration.

- A compromised operating system, kernel, firmware or keylogger.
- A compromised mail provider.
- Physical coercion.
- Zero-day exploits in the OS webview or TLS stack. We reduce exposure (no JS, no remote content,
  sandbox) but cannot prove absence.
- Perfect anonymity. Postino is not a Tor mail client and does not pretend to be.
- "Unhackable". No software is.

## 6. Security review triggers

Run a review, and update this document, when any of these change:

- a new network destination or protocol;
- authentication or token storage;
- the at-rest encryption scheme or key handling;
- the HTML renderer or webview sandbox;
- the MCP tool surface, especially send/delete/forward;
- the daemon IPC transport or token model;
- a new binary dependency with `unsafe` or a parsing surface;
- attachment handling or file writes.

## 7. Reporting

Do not open a public issue for a suspected vulnerability. Contact the maintainer privately, include
a minimal reproduction and the affected version, and allow time for a fix before disclosure.
