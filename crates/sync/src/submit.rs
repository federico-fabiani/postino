//! Submission helpers shared by the engine and the outbox worker.
//!
//! This module is the only place above the network client that knows how a composed message becomes
//! SMTP bytes. It owns three seams:
//!
//! - [`serialize_submission`] maps the engine's plain-data submission onto `postino-mime`, which
//!   validates headers, folds them, encodes bodies and attachments and returns the exact RFC 5322
//!   bytes plus the envelope. The engine persists those bytes before any network call.
//! - [`submit`] opens one SMTP session, authenticates, submits and quits. The typed
//!   [`postino_net::SmtpError`] is returned unchanged: the outbox above it decides whether a failure
//!   is transient, permanent or uncertain.
//! - [`append_sent`] discovers the server's `\Sent` folder and APPENDs the same bytes with `\Seen`.
//!   It never submits; a failure here is retried independently and the message is never resent.
//!
//! The module blocks the caller and starts a private current-thread runtime, exactly like
//! [`crate::sync_account`]. It owns no store connection and never logs a body, an address or a
//! credential.

use postino_net::{Flag, ImapConfig, Secret, Session, SmtpClient, SmtpConfig, SmtpEnvelope, SmtpError, SpecialUse};
use postino_store as store;

pub use postino_mime::serialize as serialize_submission;
pub use postino_mime::{
    Envelope as SubmissionEnvelope, Mailbox as SubmissionMailbox, MimeError, OutgoingAttachment as SubmissionAttachment,
    OutgoingMessage as Submission, Serialized as SerializedSubmission,
};
pub use postino_mime::{mime_for_filename, normalize_mime_type, sanitize_filename};

use crate::SyncError;

/// Authenticate against one SMTP endpoint without sending a message. Used by `account_add` and by
/// the settings probe: a bad submission credential is caught before anything is queued.
///
/// # Errors
///
/// The typed [`SmtpError`] from the network client: DNS, TCP, TLS, greeting, `EHLO`, `AUTH`, timeout,
/// close or a bound. No password, body or recipient is ever part of it.
pub fn verify_smtp(config: SmtpConfig) -> Result<postino_net::SmtpCapabilities, SmtpError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| SmtpError::Protocol { reason: format!("could not start the SMTP probe runtime: {error}") })?;
    runtime.block_on(SmtpClient::verify(config))
}

/// One complete SMTP transaction: connect, `EHLO`, authenticate, `MAIL`/`RCPT`/`DATA`, `QUIT`.
///
/// The caller has already persisted `raw` and decided that this attempt is safe. The returned
/// delivery means the server accepted the message; any error after the final dot is
/// [`SmtpError::Uncertain`] and must not be retried automatically.
///
/// # Errors
///
/// [`SmtpError`] with the stage and, for a server refusal, only the numeric code. No server text is
/// carried: a hostile server has received the password (and, at DATA, the body) and can echo either.
pub fn submit(config: SmtpConfig, envelope: &SmtpEnvelope, raw: &[u8]) -> Result<postino_net::Delivery, SmtpError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| SmtpError::Protocol { reason: format!("could not start the SMTP runtime: {error}") })?;
    runtime.block_on(async {
        let mut client = SmtpClient::connect(config).await?;
        client.authenticate().await?;
        let delivery = client.send(envelope, raw, None).await;
        // Only a clean transaction is worth a QUIT. A failed or uncertain one may have lost the
        // stream already; the client's Drop closes it either way.
        if delivery.is_ok() {
            let _ = client.quit().await;
        }
        delivery
    })
}

/// Append one already-submitted message to the account's Sent folder with `\Seen`.
///
/// The folder is discovered from `LIST` special-use attributes, never guessed from a name. The
/// literal bound is raised to the message size (up to the client hard ceiling), because a 25 MiB
/// attachment is larger than the default literal cap. Nothing is submitted here.
///
/// # Errors
///
/// [`SyncError::Connect`] / [`SyncError::Auth`] for the IMAP session, or [`SyncError::Protocol`] when
/// the server lists no `\Sent` folder or refuses the append.
pub fn append_sent(config: &ImapConfig, raw: &[u8]) -> Result<(), SyncError> {
    let mut config = config.clone();
    let needed = raw.len().saturating_add(1024);
    config.limits.max_literal = config.limits.max_literal.max(needed).min(postino_net::Limits::HARD_MAX_LITERAL);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| SyncError::Protocol(format!("could not start the Sent append runtime: {error}")))?;
    runtime.block_on(async move {
        let mut session = Session::connect(config).await.map_err(SyncError::from)?;
        session.login().await.map_err(SyncError::from)?;
        let folders = session.list().await.map_err(SyncError::from)?;
        let sent = folders
            .iter()
            .find(|folder| folder.special_use == Some(SpecialUse::Sent) && folder.is_selectable())
            .map(|folder| folder.wire_name.clone());
        let Some(sent) = sent else {
            let _ = session.logout().await;
            return Err(SyncError::Protocol(
                "the server lists no selectable \\Sent folder; the message was submitted but is not cached on the server".to_owned(),
            ));
        };
        let result = session.append(&sent, &[Flag::Seen], raw).await.map_err(SyncError::from);
        let _ = session.logout().await;
        result
    })
}

/// Persist the raw bytes in the encrypted blob store and return their content hash.
///
/// This is a thin convenience for the engine's queue path; the row insert is separate by design, so
/// a crash between the two leaves an unreferenced content-addressed blob, never a row without bytes.
///
/// # Errors
///
/// The store's own error when the blob cannot be written or authenticated.
pub fn store_raw(store: &store::Store, raw: &[u8]) -> Result<String, store::StoreError> {
    store.blobs().put(raw)
}

/// A password for SMTP: the separate secret when present, otherwise the IMAP one. Never formats or
/// returns the bytes outside the zeroising buffer.
pub fn smtp_secret(password: &str) -> Secret {
    Secret::new(password.to_owned())
}
