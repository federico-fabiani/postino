# Postino threat model

> **Status:** M0 · **Last updated:** 2026-10-23 · **Review:** before every release that touches
> networking, storage, HTML rendering or MCP.

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

- MIME parser: streaming, depth-limited, part-count-limited, tolerant but recorded. Fuzz with
  `cargo-fuzz`; every finding becomes a synthetic regression test.
- HTML: allowlist sanitizer (`ammonia` or equivalent), no `<script>`, no `<iframe>`, no forms, no
  `javascript:`/`data:` navigation. CSS limited; remote URLs stripped.
- Links: show the destination, warn on punycode/IDN homographs, never auto-open.
- Attachments: never auto-execute; warn on executables and macro-enabled documents; optional
  ClamAV integration later.
- Phishing: display SPF/DKIM/DMARC results from `Authentication-Results`, flag mismatches, but do
  not claim certainty.

### 4.2 Account compromise

- OAuth2 PKCE with loopback redirect; no implicit flow, no password storage for OAuth providers.
- App passwords are supported for providers that require them (Aruba), stored in the keyring.
- Tokens are refreshed in memory; refresh tokens are stored encrypted in the keyring.
- A failed login is a readable error, never a silent retry loop that locks the account.

### 4.3 Local disk theft

- SQLCipher (or equivalent AEAD page encryption) for the metadata DB.
- Each blob is encrypted with XChaCha20-Poly1305 under a key derived from the master key.
- The master key is random, 256-bit, created on first run and stored in the keyring. A recovery
  phrase is an explicit, documented export, not a default.
- Swap and crash dumps are outside our control; the threat model says so.

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

- Saves are atomic (write temp, fsync, rename).
- The outbox is a journal; a crash replays idempotently.
- Drafts keep revisions and recover after restart.
- A sync bug must never delete server mail. Expunge is explicit.
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
