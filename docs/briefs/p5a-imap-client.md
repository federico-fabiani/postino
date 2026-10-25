# Brief — P5a IMAP client and fake server

> **Package:** P5a · **Depends on:** P3d (store), P4a (MIME, not yet called) · **Next:** P5b sync
> **Owner:** developer agent · **Review:** blind engineering + hostile-server corpus
>
> No engine, store, CLI, UI or attachment work. This package is the protocol client and the
> scripted server that proves it, nothing more.

## Outcome

`postino-net` speaks enough IMAP4rev1 to read a real mailbox: greeting, CAPABILITY, LOGIN and
AUTHENTICATE PLAIN, LIST with special-use, SELECT/EXAMINE, UID FETCH of envelopes/flags/sizes and
full bodies with `BODY.PEEK`, STORE flags, NOOP, IDLE, LOGOUT. It is async (tokio), TLS-first, bounds
every input, and never logs credentials. A fake server in the test module proves each path,
including hostile and malformed responses.

## 1. Crate and layers

- New crate `crates/net` (`postino-net`), layer between foundation and sync. It may depend on
  `postino-mime`? **No** — bodies stay bytes at this layer; parsing is P5b. It must not depend on
  core, store, platform, egui or app.
- Dependencies: `tokio` (`rt`, `net`, `time`, `io-util`, `macros`), `tokio-rustls`, `rustls`,
  `rustls-platform-verifier`, `base64`, `thiserror`, `log`, `zeroize`. Exact pins in the workspace
  manifest; report build cost. No OpenSSL.
- `xtask layers`: allow `postino-net` → nothing internal. Keep it out of the UI dependency rule.

## 2. Public API

```rust
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,                 // Implicit | StartTls
    pub username: String,
    pub password: Secret,             // zeroised, redacted Debug, never logged
    pub timeout: Duration,            // per command; default 30 s
    pub limits: Limits,               // line, literal, untagged, folder count
}

pub struct Session { /* stream, tag counter, capabilities, selected mailbox */ }

impl Session {
    pub async fn connect(config: ImapConfig) -> Result<Self, NetError>;
    pub fn capabilities(&self) -> &Capabilities;
    pub async fn login(&mut self) -> Result<(), NetError>;          // AUTHENTICATE PLAIN, else LOGIN
    pub async fn list(&mut self) -> Result<Vec<RemoteFolder>, NetError>;
    pub async fn examine(&mut self, mailbox: &str) -> Result<MailboxStatus, NetError>; // read-only
    pub async fn select(&mut self, mailbox: &str) -> Result<MailboxStatus, NetError>;
    pub async fn fetch_envelopes(&mut self, uids: UidSet) -> Result<Vec<Envelope>, NetError>;
    pub async fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, NetError>;
    pub async fn store_flags(&mut self, uids: UidSet, flags: &[Flag], mode: FlagMode) -> Result<(), NetError>;
    pub async fn noop(&mut self) -> Result<(), NetError>;
    pub async fn idle(&mut self, wait: Duration) -> Result<Vec<IdleEvent>, NetError>; // one cycle, ends with DONE
    pub async fn logout(self) -> Result<(), NetError>;
}
```

- `RemoteFolder { name, delimiter, attributes: Vec<String>, special_use: Option<SpecialUse> }`;
  decode modified UTF-7 in names (`&` escapes), keep the wire name for later commands.
- `MailboxStatus { exists, recent, uid_validity, uid_next, unseen, highest_modseq: Option<u64> }`.
- `Envelope { uid, flags, internal_date, size, subject, from, to, cc, bcc, reply_to, message_id,
  in_reply_to, references, list_id }` — raw decoded strings, no MIME parsing.
- `UidSet` supports explicit lists and ranges (`1:*, 42, 100:200`); reject empty and reversed ranges.
- `FlagMode::{Add, Remove, Replace}` maps to `+FLAGS`, `-FLAGS`, `FLAGS` with `.SILENT` where
  appropriate. Never issue a bare `STORE` without `UID`.
- `IdleEvent::{Exists(u32), Expunge(u32), Recent(u32), Bye(String), Other}`.
- `NetError` variants: `Dns`, `Connect`, `Tls`, `Auth`, `Timeout`, `Closed`, `Protocol`, `ServerNo`,
  `ServerBad`, `TooLarge { what, limit }`, `Unsupported`. Messages actionable, no password, no
  server data beyond a bounded/sanitized reason.

## 3. Protocol rules

- One command at a time; tag `A0001`… and reject an unexpected tag. Untagged responses belong to the
  command in flight, except during IDLE.
- Greeting: `* OK` (or `PREAUTH`) accepted; `* BYE`/`* PREAUTH` handled; anything else is `Protocol`.
- CAPABILITY before and after STARTTLS and after login if the server advertises it. Only advertise
  what the server lists; require `IMAP4rev1` and `UIDPLUS`? No: `UIDPLUS` is optional here.
- STARTTLS: issue it, require `OK`, upgrade, then re-issue CAPABILITY and discard pre-TLS state.
  Refuse a plaintext session when `TlsMode` is implicit and the server speaks plaintext, and refuse
  STARTTLS when the capability is absent.
- LOGIN: quote strings correctly; AUTHENTICATE PLAIN when `AUTH=PLAIN`. Never send a password
  before TLS. `LOGINDISABLED` → clear `Auth` error.
- LIST: `LIST "" "*"`, parse flags, delimiter, name and literals. Special-use from attributes
  (`\Inbox`, `\Sent`, `\Drafts`, `\Trash`, `\Junk`, `\Archive`, `\Flagged`, `\All`, `\Noselect`).
- SELECT/EXAMINE: parse `EXISTS`, `RECENT`, `UIDVALIDITY`, `UIDNEXT`, `UNSEEN`, `PERMANENTFLAGS`,
  `READ-ONLY`. A missing `UIDVALIDITY` is `Protocol`; a `NO` is a readable folder error.
- FETCH: commands use `UID FETCH`; bodies use `BODY.PEEK[]` **exactly** (never `BODY[]`), so reading
  never sets `\Seen`. Envelopes use `(UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)`. Parse partial
  literals and literals split across TCP reads; cap each literal and the total per command.
- STORE: UID only, quiet where possible, surface a `NO` as `ServerNo` including the response code.
- IDLE: `IDLE`, wait `+`, collect untagged events for at most `wait` (default 29 min, below the
  server's 30-minute limit), then `DONE` and require the tagged `OK`. A disconnect during IDLE is
  `Closed`, not a panic. No nested IDLE; calling it while selected is required.
- Logout: best effort `LOGOUT`, then close; a broken connection is not an error on the way out.

## 4. Bounds and security

- Max line 64 KiB, max literal 25 MiB (configurable, hard capped at 64 MiB), max untagged responses
  per command 10 000, max folders 5 000, max command size 8 KiB. Exceeding a bound is `TooLarge` or
  `Protocol`, never a hang or an unbounded allocation.
- Timeouts on connect, TLS handshake, each command and IDLE; a timeout closes the session and is
  reported, never retried silently.
- The password lives in `Secret` (zeroise on drop, redacted Debug). No `log::debug!` of command
  lines that contain credentials; log tags and commands **without arguments** only.
- TLS verification is on by default via `rustls-platform-verifier`; tests inject a CA root. No
  `danger_accept_invalid_certs` in the public API.
- Certificate failure reports the host and the reason, not the certificate bytes.
- No `unwrap`/`expect`/`panic!` outside tests, no `unsafe`.

## 5. Fake server (test module)

`crates/net/tests/support/` (promote to `postino-testkit` only when SMTP needs it):

- A tokio TCP listener with a scripted exchange: expected client lines (or predicates), raw server
  responses, optional delays and disconnects; can serve TLS with a `rcgen` CA.
- Helpers to build untagged FETCH responses with literals, LIST lines with special-use, and IDLE
  continuations.
- Tests:
  1. greeting → capability → login → list → select → fetch envelopes → fetch body → store flags →
     logout, asserting the exact commands sent (especially `BODY.PEEK`).
  2. LOGIN failure, AUTHENTICATE PLAIN, LOGINDISABLED, PREAUTH.
  3. LIST with special-use, delimiter `NIL`, modified UTF-7 names, `\Noselect`.
  4. SELECT/EXAMINE status fields, missing UIDVALIDITY, NO on select.
  5. FETCH literals split across reads, two messages in one response, unsolicited EXISTS,
     an oversized literal refused, a malformed response refused.
  6. IDLE: continuation, EXISTS then EXPUNGE, DONE + tagged OK, server disconnect, timeout.
  7. TLS: implicit and STARTTLS with a test CA; wrong hostname refused; plaintext against implicit
     refused; STARTTLS not advertised refused.
  8. Timeout and abrupt close at every phase produce a typed error and no panic.
- A deterministic malformed-response corpus (≥ 30 cases: bad tags, missing `*`, literals without
  terminator, huge numbers, NIL where a number is required, negative values, nested lists) asserted
  to return an error rather than panic or hang.

## 6. Docs and honesty

- `docs/architecture.md`: net crate section, TLS policy, bounds, what is not implemented (QRESYNC,
  SORT/THREAD, METADATA, ACL, S/MIME, OAuth).
- `docs/threat-model.md` §4.1/4.2: TLS verification, password handling, literal bounds.
- README: one honest line (IMAP client exists and is tested against a fake server; the app still
  runs on demo mail until onboarding).
- ROADMAP: tick the IMAP client line only after review; leave sync/offline unchecked.

## 7. Out of scope

Sync loops, store writes, engine operations, CLI commands, UI, onboarding, autodiscovery, OAuth,
SMTP, MIME parsing, threading, rules, notifications.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, layers updated, tests above, no secrets in logs or
fixtures. Temp files only in `%TEMP%\opencode\p5a-postino`; do not commit.

## Report

Dependency lines and build cost, API, command grammar covered, test map, TLS evidence, bounds, and
any deviation with the reason.

## Implementation status (P5a)

Delivered as specified. Decisions and deviations, all deliberate and documented in the crate:

- **`Envelope::references` and `list_id` are always empty.** RFC 3501's `ENVELOPE` carries neither;
  filling them needs a second `BODY.PEEK[HEADER.FIELDS (REFERENCES LIST-ID)]` fetch, which the brief
  assigns to P5b. The fields exist so sync can fill them without changing the type.
- **`MailboxStatus` has two extra fields**, `read_only` and `permanent_flags`: §3 requires parsing
  `READ-ONLY` and `PERMANENTFLAGS`, and the brief's field list had nowhere to put them.
- **`\Noselect` stays in `RemoteFolder::attributes`**, not in `SpecialUse`: it is a selectability
  attribute, not a folder role (RFC 6154 lists the eight special-use values). `is_selectable()`
  exposes it.
- **`RemoteFolder::wire_name`** is added because re-encoding a decoded modified UTF-7 name can change
  bytes the server treats as meaningful; `select`/`examine` take the wire name.
- **`ImapConfig::extra_roots`** is the injectable test CA. It augments the platform verifier through
  `rustls-platform-verifier`'s `new_with_extra_roots`; there is no skip-verification path.
- **`Limits::max_response_bytes`** (64 MiB, ceiling 128 MiB) bounds the total for one command, as §3
  asks ("cap each literal and the total per command"). Fixed internal caps: 32 nesting levels, 4 096
  literals per response, 1 KiB atoms and names, 256 addresses and flags per list, 20 digits per
  number.
- **`NetError::Protocol` also covers a request the client refuses to build** (a CR/LF/NUL in a name,
  an empty UID set). The variant list is unchanged; the reason says which side is at fault.
- **Connect budget is shared fairly** across resolved addresses (capped at 5 s each) so a
  black-holed IPv6 route cannot consume the timeout the IPv4 address needs.
- **STARTTLS with pipelined plaintext after the `OK` is a `Protocol` error**, not a silent discard:
  the bytes are a command-injection attempt.
- **No OpenSSL, no cmake, no nasm**: `ring` is the crypto provider for `rustls`, so the crate builds
  on the Windows GNU host with the MSYS2 toolchain the workspace already needs.

