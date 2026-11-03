# postino-net

IMAP4rev1 and SMTP submission clients. Network layer, not a sync engine: each opens one connection,
speaks TLS, exchanges commands and returns typed data. It opens no store, parses no MIME and runs no
loop.

```rust
let config = ImapConfig::new("imap.example.com", 993, TlsMode::Implicit, "me@example.com", Secret::new(app_password));
let mut session = Session::connect(config).await?;
session.login().await?;
for folder in session.list().await? {
    println!("{} ({:?})", folder.name, folder.special_use);
}
let status = session.select("INBOX").await?;
let envelopes = session.fetch_envelopes(UidSet::parse("1:*")?).await?;
let body = session.fetch_body(envelopes[0].uid).await?;
session.logout().await?;
```

## API

| Call | Wire |
|---|---|
| `Session::connect` | TCP + TLS (implicit or STARTTLS), greeting, `CAPABILITY` |
| `login` | `AUTHENTICATE PLAIN` when advertised, else `LOGIN`; then `ID ("name" "Postino" …)` when the server advertises `ID` |
| `list` | `LIST "" "*"` |
| `select` / `examine` | `SELECT "X"` / `EXAMINE "X"` |
| `fetch_envelopes` | `UID FETCH <set> (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)` |
| `fetch_uid_flags` | `UID FETCH <set> (UID FLAGS)`: the folder liveness scan, no envelope |
| `fetch_body` | `UID FETCH <uid> (BODY.PEEK[])` |
| `fetch_body_opt` | the same, but `None` when the UID has vanished instead of a protocol error |
| `store_flags` | `UID STORE <set> +FLAGS.SILENT (...)` / `-FLAGS.SILENT` / `FLAGS.SILENT` |
| `noop` | `NOOP` |
| `idle` | `IDLE`, continuation, untagged events for at most `wait`, `DONE`, tagged `OK` |
| `append` | `APPEND "mailbox" (\Seen) {literal}` after the `+`, then the bytes and the tagged reply |
| `logout` | best-effort `LOGOUT`, then close |

Tags are `A0001`, `A0002`, ...; a response with any other tag is a `Protocol` error and closes the
session. `BODY.PEEK[]` is used for bodies, never `BODY[]`: reading a message never sets `\Seen`.
`UID STORE` is always `UID` and always silent. `fetch_body` refuses a body whose UID is not the one
it asked for, and `fetch_envelopes` refuses a UID outside the requested set: a server that returns
the wrong message must not be believed. `fetch_uid_flags` instead skips a UID outside the requested
set: the `<UIDNEXT>:*` liveness probe legitimately gets the last live message back (RFC 3501
§6.4.8), and an unsolicited flag update carries a UID nobody asked for. `fetch_body_opt` returns `None` when the server answers a fetch of an expunged UID with a
bare `OK` and no `FETCH`: a vanished message is a caller decision, not a protocol failure; the strict
`fetch_body` keeps its old `Protocol` error for callers that need it. `select` honours
`OK [READ-ONLY]` on the tagged response, not only on an untagged one.

`RemoteFolder` carries both the decoded `name` (modified UTF-7 resolved) and the exact `wire_name`:
pass `wire_name` back to `select`/`examine`, because re-encoding a decoded name can change bytes the
server considers meaningful.

`Envelope` is the IMAP `ENVELOPE` only. `references` and `list_id` are always empty here: RFC 3501's
`ENVELOPE` has no slot for them, and fetching them needs a second `BODY.PEEK[HEADER.FIELDS ...]`
that belongs to the sync package.

## SMTP submission

`SmtpClient` is the P7 sender. It connects with implicit TLS (465) or STARTTLS (587), reads the
greeting, `EHLO`s, `AUTH PLAIN` or `LOGIN`s, then runs one transaction: `MAIL FROM`, one `RCPT TO` per
recipient, `DATA`, the dot-stuffed body and the final `.`. `RSET`, `NOOP` and `QUIT` are available.
`SmtpClient::verify` is the `account_add` probe (connect, authenticate, quit).

Capabilities are parsed from the final `EHLO`: `SIZE` (checked before `MAIL`), `8BITMIME` (a body with
8-bit bytes is refused when absent), `SMTPUTF8` (only when headers or envelope need it and the server
advertises it), `DSN` (`RET`/`ENVID`/`NOTIFY`, only when advertised) and `PIPELINING` (parsed, never
used: every command waits for its reply).

`SmtpError` distinguishes `Connect`, `Tls`, `Auth`, `Rejected { code, stage, message }`, local
`Transient`/`Permanent`, `Timeout`, `Closed`, `TooLarge`, `Unsupported`, `Protocol` and `Uncertain`.
4xx is transient, 5xx permanent; everything after the final `.` is written is `Uncertain`, because the
server may have accepted the message even if no `250` arrived. The outbox never retries it
automatically.

Bounds: 8 KiB per reply line, 64 KiB per reply, 40 MiB per message, 100 recipients, a per-phase
timeout, CRLF-only data, no NUL, exact dot-stuffing. The password is a `Secret`, the SASL payloads
live in `Zeroizing` buffers, and logs carry the stage only.

## TLS policy

- Only `TlsMode::Implicit` and `TlsMode::StartTls`: no plaintext mode exists.
- Verification is the operating system's, through `rustls-platform-verifier` (CryptoAPI on Windows,
  Security framework on macOS, system trust store through WebPKI on Linux). The crypto provider is
  `ring`.
- `ImapConfig::extra_roots` adds trust anchors for tests. It augments verification; there is no
  switch that turns verification off.
- STARTTLS is mandatory when selected: no `STARTTLS` capability is `Unsupported`. After the `OK`,
  any plaintext already buffered on the socket is a command-injection attempt and is refused.
  Capabilities collected before the upgrade are discarded.
- A certificate failure reports the host and the verifier's reason, never the certificate.

## Bounds

| Bound | Default | Hard ceiling |
|---|---|---|
| Response line | 64 KiB | 64 KiB |
| Literal (one message body) | 25 MiB | 64 MiB |
| Untagged responses per command | 10 000 | 10 000 |
| Folders per `LIST` | 5 000 | 5 000 |
| Command line | 8 KiB | 8 KiB |
| Total response per command | 64 MiB | 128 MiB |

There are also fixed limits: 32 list-nesting levels, 4 096 literals per response, 1 024 bytes per
atom, 1 024 bytes per mailbox name, 256 addresses per envelope list, 256 flags per list, 20 digits
per number. A number is parsed, never wrapped; `NIL` where a number is required, a negative value, a
missing status word and an unexpected tag are all `Protocol`.

Every phase has a timeout (`ImapConfig::timeout`, 30 s by default): DNS, TCP connect, TLS handshake,
greeting, each command and the `DONE` half of `IDLE`. A timeout closes the session: the next call is
`Closed` instead of a half-read command. `IDLE`'s *wait* is not a timeout: it ends with `DONE` and the
tagged `OK`, and is clamped to 29 minutes (RFC 2177 asks servers to hold at least 30).

A bound or a parse error is a desynchronisation: the client stopped in the middle of a response, so
it closes the session, drops the socket (a server must not keep a dead `IDLE`) and refuses every later
call as `Closed`. A complete tagged `NO`/`BAD` — including an authentication rejection — leaves the
session usable; so does a request refused locally before anything was written (an empty UID set, an
over-long flag list). For `IDLE` that means
only a quiet expiry of the wait ends the cycle normally; a timeout waiting for the `+`, a timeout
while a line or literal is half-read, a malformed response and a bound error all close the session,
and the `DONE` half enforces the same untagged-response cap as every other command.

When a host resolves to several addresses, each attempt gets a fair share of the connect budget,
capped at five seconds, so a black-holed IPv6 route cannot consume the timeout the IPv4 address
needs.

## Security

- The password lives in `Secret`: zeroised on drop, `Debug`/`Display` print `[redacted]`. It is
  dropped (and zeroised) after a successful login. `ImapConfig`'s `Debug` prints the host, port and
  TLS mode, and redacts both the username and the password: an address is personal data.
- Logs (`log::debug!`) carry the tag and the command verb only: no arguments, no bodies, no
  credentials.
- The SASL payload and the `LOGIN` line are built in `Zeroizing` buffers.
- `LOGINDISABLED` is a clear `Auth` error, not a retry; a `NO`/`BAD` on authentication becomes
  `Auth` with the server's code in the reason.
- Error text is reduced to 200 bytes of printable ASCII, so a hostile server cannot inject control
  characters or unbounded data into a log.

## Tests

`cargo test -p postino-net`. Two zooms:

- Unit tests in every module: UTF-7 vectors, `UidSet` grammar, flag parsing, the response parser,
  literal-marker detection, limits, redaction. 59 tests.
- Integration tests against `postino-testkit`, a scripted TLS server with a throwaway `rcgen` CA
  (`extra_roots`), real sockets, exact command assertions, literals split across reads, delays,
  disconnects and STARTTLS mid-session:
  `connection.rs`, `auth.rs`, `mailbox.rs`, `fetch.rs` (including a UID/FLAGS-only fetch and a
  vanished body), `idle.rs`, `tls.rs`, `failures.rs` and a 54-case hostile corpus in `hostile.rs`
  (bad tags, missing `*`, unterminated literals, huge and negative numbers, `NIL` where a number
  belongs, deep nesting, oversized lines and more). Every corpus case must return a typed error
  within three seconds.

All integration tests use real TLS, not a bypass: the fake server presents a certificate for
`localhost` signed by the test CA, and the client verifies it through the same platform verifier as
production.

## Not implemented

- Sync: this crate has no loop and no store writes. `postino-sync` is the consumer; `COPY`/`MOVE`/
  `EXPUNGE` and server-side flag pushes are still missing. `APPEND` exists for the P7 Sent copy.
- SMTP `PIPELINING` and OAuth; `AUTH` mechanisms other than PLAIN and LOGIN; autodiscovery.
- `QRESYNC`/`CONDSTORE` beyond parsing `HIGHESTMODSEQ` when the server reports it; no `VANISHED`.
- `SORT`, `THREAD`, `METADATA`, `ACL`, `SIEVE`, `NOTIFY`, `ESEARCH`.
- `BODYSTRUCTURE` and per-part fetches; bodies are whole messages at this layer.

## Build

Pure Rust: `ring` supplies the crypto, `rustls` and `tokio-rustls` the TLS, `tokio` the runtime.
No OpenSSL, no cmake, no nasm. `rcgen` is test-only. On the Windows GNU host the crate builds with
the MSYS2 toolchain already required by the workspace.
