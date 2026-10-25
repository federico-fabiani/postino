# Postino threat model

> **Status:** M0 + M1 MIME parser (P4a) + encrypted attachment blobs (P4b) + IMAP sync into the store
> (P5b) · **Last updated:** 2026-10-25 · **Review:** before every release that touches networking,
> storage, HTML rendering or MCP.

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
| Supply chain | Compromised crate, font, installer | `cargo-deny`/`cargo-audit`, pinned lockfile, asset hashes, signed releases |
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
11. **Sending is auditable.** Every send records account, identity, recipients count, message id,
    time and the tool that requested it. Bodies are not logged.
12. **Fail closed.** A missing keyring, unreadable store, invalid root or malformed config stops the
    operation with an error; it never falls back to plaintext or an unconfined path.

## 4. Threats and mitigations

### 4.1 Malicious email

- MIME parser (`postino-mime`, P4a): raw size, header block, part count and multipart depth are
  bounded before and inside the parser; decoded part size, total attachment bytes and HTML size are
  capped. `message/rfc822` parts are not expanded. Malformed structure is repaired and recorded as
  warnings. A mutation corpus runs in CI; `cargo-fuzz` and the network-fed corpus remain M6 work.
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

### 4.3 Local disk theft

Exactly what is encrypted, and with what:

- **The database** (`<path>`): SQLCipher encrypts every page and WAL frame, and the temp store is
  configured in memory so temp tables never reach disk. Subjects, addresses, bodies, flags, labels,
  folder names, draft revisions, attachment names/types/sizes and their blob hashes are ciphertext on
  disk. The file begins with a random per-database salt, not `SQLite format 3`. Sync adds the cached
  server UIDs, the local cursor and the status JSON (`sync_status:<account>`) to the same encrypted
  pages; the status contains counts and a failure category, not the server's name or any content.
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
  deleted message also leaves its blob because another message may share the content.
- **Backups.** A backup of the folder copies the database and the blobs. It is unreadable without the
  keyring entry, and there is no passphrase export yet, so the keyring entry is a single point of
  failure for restores.

The disk-level guarantee is therefore: **no message content and no key material is recoverable from
the files Postino writes** (the database and the blob files), while metadata shape (attachment size
class, content equality, file count) is visible to someone who can read the folder without the key.
The guarantee does not extend to memory the operating system may have paged out; see the swap and
crash-dump entry above.

### 4.4 Malicious agent

- MCP is opt-in. The default posture is read + draft; sending requires an explicit server flag and
  is recorded.
- `dry_run` returns the exact RFC 822 bytes and the envelope recipients.
- Idempotency keys make retries safe. A per-call recipient cap and a configurable rate limit prevent
  a runaway loop.
- `postino pause` / `--no-send` stops the server from sending without killing sync.
- The agent never receives credentials; it receives operations.
- Tool errors are structured and actionable. An agent cannot use error text to probe paths outside
  its roots.

### 4.5 Supply chain

- `Cargo.lock` committed; `cargo-deny` for licences and advisories; `cargo-audit` in CI.
- Minimal dependency surface; no npm in M0. If a webview renderer arrives, it is an OS component,
  not a bundled browser, and it gets its own sandbox review.
- Fonts and logos are hashed and attributed; `cargo xtask assets` fails on drift.
- Release artefacts are signed; Windows and macOS installers are code-signed and notarized.

### 4.6 Data loss

- Saves are atomic: write a temp file, fsync it, then publish (rename or hard link).
- The outbox is a journal; a crash replays idempotently.
- Drafts keep revisions and recover after restart.
- A sync bug must never delete server mail. Expunge is explicit. `postino-sync` never issues a
  mutating command; its deletion pass only removes rows from the local cache after a complete UID
  list, and it skips that pass when a run stopped at the message limit.
- Sync resumes without gaps: the per-folder cursor advances only after a message transaction commits,
  so a crash re-fetches one message at most. A `UIDVALIDITY` change discards that folder's cache but
  keeps local drafts, which were never on the server.
- Backups are the user's responsibility, but export/import is a first-class feature.

## 5. Out of scope

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
