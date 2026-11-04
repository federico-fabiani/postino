//! Test-only fake IMAP and SMTP servers, TLS fixture, response builders and synthetic mail.
//!
//! This crate exists for `postino-net`, `postino-sync` and `postino-core` integration tests: it
//! listens on a real loopback socket, presents a certificate from a throwaway `rcgen` CA, asserts
//! the exact command lines a client writes and answers with the bytes a test scripts. It is a
//! **dev-dependency only**; no production crate may link it (`cargo xtask layers` enforces that).
//!
//! A scripted server has to fail loudly, so helper code here uses `expect`/`panic!` on purpose: a
//! mismatched expectation is a broken test, not a runtime condition. That is the one exception to
//! the repository's "no panic outside tests" rule, and it is confined to this crate.
//!
//! ```no_run
//! use postino_testkit::{FakeServer, TestFolder, TestMessage, TlsFixture, synthetic_message};
//!
//! # async fn run() {
//! let tls = TlsFixture::generate();
//! let folders = vec![TestFolder::new("INBOX", 1).with_messages(vec![TestMessage::synthetic(synthetic_message(1))])];
//! let server = FakeServer::start_tls(&tls, move |conn| async move {
//!     let _ = postino_testkit::serve_mailbox(conn, folders).await;
//! }).await;
//! println!("listening on {}", server.port());
//! server.finish().await;
//! # }
//! ```

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![deny(missing_docs)]

// This crate is test support: a scripted server that swallows a mismatched expectation turns a
// broken client green (see the contract above), so the four modules that assert the wire protocol
// keep `expect`/`panic!` on purpose. The crate-level deny above stays in force everywhere else,
// including any new module, so an accidental panic still has to be a decision, not an oversight.
mod mailbox;
#[allow(clippy::expect_used, clippy::panic)]
mod oauth;
mod responses;
#[allow(clippy::expect_used, clippy::panic)]
mod server;
#[allow(clippy::expect_used, clippy::panic)]
mod smtp;
mod synthetic;
#[allow(clippy::expect_used, clippy::panic)]
mod tls;

pub use mailbox::{FailPoint, MailboxScript, TestFolder, TestMessage, serve_mailbox, serve_mailbox_with, serve_mailbox_with_xoauth2};
pub use oauth::{FakeOAuth, TokenReply, expected_xoauth2, token_json};
pub use responses::{
    EnvelopeAddress, EnvelopeFields, SelectStatus, fetch_body_response, fetch_envelope_line, fetch_uid_flags_line, idle_continuation, list_line,
    quoted, select_lines,
};
pub use server::{Conn, FakeServer, PASSWORD, USER, config_with_timeout, expected_sasl};
pub use smtp::{FakeSmtp, SMTP_PASSWORD, SMTP_USER, SmtpConn, expected_smtp_login_password, expected_smtp_login_user, expected_smtp_sasl};
pub use synthetic::{SyntheticMessage, synthetic_html_message, synthetic_message, synthetic_message_with_attachment, synthetic_messages};
pub use tls::TlsFixture;
