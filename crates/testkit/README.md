# postino-testkit

**Layer:** test-only. Depends on `postino-net` (the types it scripts) and on the existing test stack
(`rcgen`, `rustls`, `tokio`, `tokio-rustls`, `base64`). No production crate may depend on it, and even
`postino-net` and `postino-sync` may use it only as a dev-dependency: `cargo xtask layers` fails
otherwise.

A fake IMAP server for integration tests: a real loopback socket, a real TLS handshake with a
throwaway `rcgen` CA, exact recording of every client line and assertions that fail the test, not the
client.

```rust
use postino_testkit::{FakeServer, TestFolder, TestMessage, TlsFixture, synthetic_message};

let tls = TlsFixture::generate();
let folders = vec![TestFolder::new("INBOX", 1)
    .with_special_use("\\Inbox")
    .with_messages(vec![TestMessage::synthetic(synthetic_message(1))])];
let server = FakeServer::start_tls(&tls, move |conn| async move {
    let conn = postino_testkit::serve_mailbox(conn, folders).await;
    assert!(conn.received().iter().all(|line| !line.contains("STORE")));
}).await;
let config = server.config(&tls); // localhost, implicit TLS, the test CA in extra_roots
server.finish().await;
```

## Pieces

| Item | Use |
|---|---|
| `FakeServer` / `Conn` | one listener, one connection; `expect`, `expect_prefix`, `send_line`, `send_bytes`, `delay`, `accept_tls`, `prepare`, `prepare_sasl`, `login_ok`, recorded lines |
| `TlsFixture` | CA + leaf for `localhost`; `ca()` for `ImapConfig::extra_roots`, `acceptor()` for STARTTLS |
| `USER` / `PASSWORD` / `expected_sasl` | the fixed identity every test uses (`AUTHENTICATE PLAIN`) |
| `list_line`, `select_lines`, `SelectStatus` | LIST and SELECT response builders |
| `fetch_envelope_line`, `EnvelopeFields`, `EnvelopeAddress` | a complete ten-field IMAP ENVELOPE |
| `fetch_body_response`, `fetch_uid_flags_line` | FETCH with a literal, and a flags-only FETCH |
| `idle_continuation` | the `+` line of an IDLE cycle |
| `SyntheticMessage`, `synthetic_message(s)`, `synthetic_html_message`, `synthetic_message_with_attachment` | deterministic RFC 5322 mail with fixed dates, `example.test` addresses and a stable `Message-ID` |
| `TestFolder`, `TestMessage`, `serve_mailbox`, `serve_mailbox_with` | an in-memory mailbox that answers LIST/SELECT/UID FETCH until LOGOUT, for scale tests |
| `MailboxScript`, `FailPoint` | drop the connection at a chosen SELECT/body/envelope fetch, or set a cancellation flag after the Nth body |

`serve_mailbox` answers `STORE`, `EXPUNGE`, `DELETE` and unknown commands with `BAD`: a reconciler
that mutates the server must fail the suite rather than pass silently.

A scripted server has to fail loudly, so this crate uses `expect`/`panic!` on a broken expectation.
That is deliberate and confined to test infrastructure.

## Tests in other crates

- `postino-net`: 111 tests, including the 54-case hostile corpus, all against this server.
- `postino-sync`: 16 reconciliation tests plus the 2 000/10 000-message scale tests.

The crate's own unit tests cover the builders and the mailbox's command parsing.
