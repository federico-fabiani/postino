//! IMAP4rev1 client.
//!
//! This is a network crate: it opens one TCP connection, speaks TLS to it and exchanges IMAP
//! commands. It does not parse MIME, write the store, run a sync loop or know what a message looks
//! like to the engine. Bodies stay `Vec<u8>` until P5b.
//!
//! ```text
//! ImapConfig ──▶ Session::connect ──▶ greeting ──▶ CAPABILITY ──▶ [STARTTLS ──▶ CAPABILITY]
//!                                               └─▶ login ──▶ list / select / fetch / store / idle
//! ```
//!
//! # Contract
//!
//! - **TLS only.** [`TlsMode::Implicit`] or [`TlsMode::StartTls`]; there is no plaintext mode and no
//!   way to disable certificate verification. Verification is the operating system's
//!   (`rustls-platform-verifier`), optionally augmented by extra trust anchors for tests
//!   ([`ImapConfig::extra_roots`]).
//! - **Bounded.** Every line, literal, untagged response and command is capped by [`Limits`];
//!   exhausting a bound is a typed error, never a hang or an unbounded allocation.
//! - **Typed.** Every failure is a [`NetError`], every timeout closes the session, and a password
//!   lives in a [`Secret`] that zeroises itself and prints as `[redacted]`. Logs carry tags and
//!   command verbs, never arguments, bodies or credentials.
//! - **Honest.** Only the commands below exist. QRESYNC, CONDSTORE, SORT/THREAD, METADATA, ACL,
//!   OAuth (SASL XOAUTH2), S/MIME, SMTP and autodiscovery are not implemented; unknown untagged
//!   responses are collected as [`response`] noise and ignored rather than trusted.
//!
//! See `README.md` for the command grammar, the bounds and the fake server that proves each path.

#![deny(missing_docs)]

mod capabilities;
mod config;
mod error;
mod fetch;
mod folder;
mod limits;
mod mailbox;
mod message;
mod parser;
mod response;
mod secret;
mod session;
mod tls;
mod uid;
mod wire;

pub use capabilities::Capabilities;
pub use config::{ImapConfig, TlsMode};
pub use error::NetError;
pub use folder::{MailboxStatus, RemoteFolder, SpecialUse};
pub use limits::Limits;
pub use mailbox::{decode_utf7, encode_utf7};
pub use message::{Address, Envelope, Flag, FlagMode, IdleEvent, UidFlags};
pub use rustls::pki_types::CertificateDer;
pub use secret::Secret;
pub use session::Session;
pub use uid::UidSet;
