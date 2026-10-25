//! Test-only fake IMAP server, TLS fixture, response builders and synthetic mail.
//!
//! This crate exists for `postino-net` and `postino-sync` integration tests: it listens on a real
//! loopback socket, presents a certificate from a throwaway `rcgen` CA, asserts the exact command
//! lines a client writes and answers with the bytes a test scripts. It is a **dev-dependency only**;
//! no production crate may link it (`cargo xtask layers` enforces that).
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

#![deny(missing_docs)]

mod mailbox;
mod responses;
mod server;
mod synthetic;
mod tls;

pub use mailbox::{FailPoint, MailboxScript, TestFolder, TestMessage, serve_mailbox, serve_mailbox_with};
pub use responses::{
    EnvelopeAddress, EnvelopeFields, SelectStatus, fetch_body_response, fetch_envelope_line, fetch_uid_flags_line, idle_continuation, list_line,
    quoted, select_lines,
};
pub use server::{Conn, FakeServer, PASSWORD, USER, config_with_timeout, expected_sasl};
pub use synthetic::{SyntheticMessage, synthetic_html_message, synthetic_message, synthetic_message_with_attachment, synthetic_messages};
pub use tls::TlsFixture;
