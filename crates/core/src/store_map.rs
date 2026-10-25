//! The one translation layer between the core domain and `postino-store` rows.
//!
//! The store crate deliberately does not depend on `postino-core`, so every conversion lives here
//! and nowhere else: a store column change is a change in this file. Core carries no IMAP
//! identifiers yet, so a row written from here has `uid`, `message_id` and `size` empty; that is
//! safe because the engine only writes local drafts and draft submissions, which never have one.

use chrono::{DateTime, Utc};
use postino_store as store;
use postino_store::StoreError;

use crate::engine::OpError;
use crate::model::{Account, Address, Attachment, Folder, FolderKind, Label, LabelColor, MailTls, Message, MessageSummary, ServerSettings, snippet};

/// Unix milliseconds: the store's only date format (see the `postino-store` crate docs).
pub(crate) fn millis(date: DateTime<Utc>) -> i64 {
    date.timestamp_millis()
}

pub(crate) fn date(value: i64) -> Result<DateTime<Utc>, OpError> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .ok_or_else(|| OpError::Failed(format!("the local store holds a date this build cannot read ({value} ms since the epoch)")))
}

/// A failed write from the store.
///
/// Caller mistakes get a specific sentence and no disk advice; storage, I/O and corruption failures
/// get the storage advice. Only the latter may mention a rollback, and every repository write the
/// engine performs is one transaction, so nothing is half-applied.
pub(crate) fn write_error(error: StoreError) -> OpError {
    match &error {
        StoreError::NotFound { entity, id } => OpError::InvalidArgs(format!("unknown {entity} {id:?}; use an id returned by the list operation")),
        StoreError::AccountMismatch { folder_id, message_account, folder_account } => OpError::InvalidArgs(format!(
            "folder {folder_id:?} belongs to account {folder_account:?}, not {message_account:?}; use a folder from the message's own account"
        )),
        StoreError::Duplicate { entity, name } => OpError::InvalidArgs(format!("{entity} {name:?} already exists; choose another one")),
        StoreError::DuplicateAttachment { id } => {
            OpError::InvalidArgs(format!("attachment id {id:?} is repeated in the draft; give every attachment a unique id"))
        }
        StoreError::AttachmentOwnedByOther { id, owner } => {
            OpError::InvalidArgs(format!("attachment id {id:?} belongs to message {owner:?}; omit the id so the engine assigns a new one"))
        }
        StoreError::OutOfRange { field } => OpError::InvalidArgs(format!("{field} is outside the range the local store accepts")),
        StoreError::Encode(_) => OpError::Failed(format!("the local store could not encode the change: {error}. Memory is untouched.")),
        _ => OpError::Failed(format!(
            "the local store refused the change: {error}. The change was rolled back and memory is untouched; check that the store file is writable, that the key matches and that there is free disk space, then retry."
        )),
    }
}

/// A failed read while the engine loads its mailbox.
pub(crate) fn read_error(error: StoreError) -> OpError {
    OpError::Failed(format!(
        "the local store could not be read: {error}. The engine was not started; check the store file and the key in the OS keyring, then retry."
    ))
}

/// A failed blob read while the engine returns attachment bytes.
///
/// Corruption, a missing file and an oversized declared length are storage failures, not caller
/// mistakes: the message is readable and stays untouched, only its bytes are unavailable.
pub(crate) fn blob_read_error(error: StoreError) -> OpError {
    match &error {
        StoreError::CorruptBlob { .. } => OpError::Failed(format!(
            "the attachment's stored bytes could not be authenticated: {error}. Nothing was returned and the message is untouched; re-sync the attachment to recover it."
        )),
        StoreError::BlobMissing { .. } => OpError::Failed(format!(
            "the attachment's bytes are missing from the local blob store: {error}. The message itself is intact; re-sync the attachment to recover it."
        )),
        StoreError::BlobTooLarge { .. } => OpError::Failed(format!(
            "the attachment's stored length is larger than this read allows: {error}. The bytes were not returned; the message is untouched."
        )),
        StoreError::BadHash { .. } => {
            OpError::Failed(format!("the attachment metadata holds an invalid blob hash: {error}. The bytes were not read."))
        }
        _ => OpError::Failed(format!(
            "the local blob store could not be read: {error}. Check that the blob directory next to the database is readable and on a disk with free space, then retry."
        )),
    }
}

/// A failed blob write while the engine stores attachment bytes. The metadata write never ran.
pub(crate) fn blob_write_error(error: StoreError) -> OpError {
    OpError::Failed(format!(
        "the attachment bytes could not be stored: {error}. The attachment metadata was not changed; check free disk space and that the blob directory next to the database is writable, then retry."
    ))
}

/// A failed repair of draft rows that had no display row.
pub(crate) fn repair_error(error: StoreError) -> OpError {
    OpError::Failed(format!(
        "the local store has draft rows that could not be repaired into messages: {error}. The engine was not started; check the store file and the key in the OS keyring, then retry."
    ))
}

pub(crate) fn account(row: store::Account) -> Account {
    Account { id: row.id, name: row.name, email: row.email, provider: row.provider, signature: row.signature }
}

/// The public view of an IMAP endpoint. The store row's `created_at` is storage detail and stays
/// out of the operation surface.
pub(crate) fn server_settings(row: store::AccountServer) -> ServerSettings {
    ServerSettings { host: row.host, port: row.port, tls: mail_tls(row.tls), username: row.username }
}

pub(crate) fn account_tls(tls: MailTls) -> store::AccountTls {
    match tls {
        MailTls::Implicit => store::AccountTls::Implicit,
        MailTls::StartTls => store::AccountTls::StartTls,
    }
}

pub(crate) fn mail_tls(tls: store::AccountTls) -> MailTls {
    match tls {
        store::AccountTls::Implicit => MailTls::Implicit,
        store::AccountTls::StartTls => MailTls::StartTls,
    }
}

pub(crate) fn net_tls(tls: MailTls) -> postino_net::TlsMode {
    match tls {
        MailTls::Implicit => postino_net::TlsMode::Implicit,
        MailTls::StartTls => postino_net::TlsMode::StartTls,
    }
}

pub(crate) fn folder(row: store::Folder) -> Folder {
    Folder { id: row.id, account_id: row.account_id, name: row.name, kind: folder_kind(row.kind), unread: row.unread, total: row.total }
}

fn folder_kind(kind: store::FolderKind) -> FolderKind {
    match kind {
        store::FolderKind::Inbox => FolderKind::Inbox,
        store::FolderKind::Starred => FolderKind::Starred,
        store::FolderKind::Sent => FolderKind::Sent,
        store::FolderKind::Drafts => FolderKind::Drafts,
        store::FolderKind::Archive => FolderKind::Archive,
        store::FolderKind::Spam => FolderKind::Spam,
        store::FolderKind::Trash => FolderKind::Trash,
        store::FolderKind::Custom => FolderKind::Custom,
    }
}

pub(crate) fn label(row: store::LabelCount) -> Result<Label, OpError> {
    let color = LabelColor::parse(&row.label.color)
        .ok_or_else(|| OpError::Failed(format!("the local store holds a label colour {:?} this build does not know", row.label.color)))?;
    Ok(Label { id: row.label.id, name: row.label.name, color })
}

pub(crate) fn label_row(id: &str, name: &str, color: LabelColor) -> store::Label {
    // Labels are mailbox-wide until onboarding gets per-account identities (P6).
    store::Label { id: id.to_owned(), account_id: None, name: name.to_owned(), color: color.as_str().to_owned() }
}

pub(crate) fn message(row: store::Message, attachments: Vec<store::Attachment>) -> Result<Message, OpError> {
    Ok(Message {
        summary: MessageSummary {
            id: row.summary.id,
            thread_id: row.summary.thread_id,
            folder_id: row.summary.folder_id,
            from: address(&row.summary.from),
            to: row.summary.to.iter().map(address).collect(),
            subject: row.summary.subject,
            snippet: row.summary.snippet,
            date: date(row.summary.date)?,
            unread: row.summary.unread,
            starred: row.summary.starred,
            has_attachments: row.summary.has_attachments,
            labels: row.summary.labels,
        },
        cc: row.cc.iter().map(address).collect(),
        bcc: row.bcc.iter().map(address).collect(),
        body_text: row.body_text,
        body_html: row.body_html,
        attachments: attachments.into_iter().map(attachment).collect(),
    })
}

/// Build a store row from a core message. `account_id` is not on the core summary; the caller
/// knows it from the folder the message lives in.
pub(crate) fn message_row(message: &Message, account_id: &str) -> store::Message {
    store::Message {
        summary: store::MessageSummary {
            id: message.summary.id.clone(),
            account_id: account_id.to_owned(),
            folder_id: message.summary.folder_id.clone(),
            thread_id: message.summary.thread_id.clone(),
            from: store_address(&message.summary.from),
            to: message.summary.to.iter().map(store_address).collect(),
            subject: message.summary.subject.clone(),
            snippet: message.summary.snippet.clone(),
            date: millis(message.summary.date),
            unread: message.summary.unread,
            starred: message.summary.starred,
            has_attachments: message.summary.has_attachments,
            labels: message.summary.labels.clone(),
        },
        uid: None,
        message_id: None,
        cc: message.cc.iter().map(store_address).collect(),
        bcc: message.bcc.iter().map(store_address).collect(),
        body_text: message.body_text.clone(),
        body_html: message.body_html.clone(),
        size: None,
    }
}

pub(crate) fn attachment_rows(attachments: &[Attachment]) -> Vec<store::Attachment> {
    attachments
        .iter()
        .map(|attachment| store::Attachment {
            id: attachment.id.clone(),
            filename: attachment.filename.clone(),
            mime: attachment.mime.clone(),
            size: attachment.size,
            // The store preserves the hash of an attachment id that survives a draft re-save (P4b),
            // so a metadata-only mapping cannot silently detach already stored bytes.
            blob_hash: None,
        })
        .collect()
}

/// Rebuild a draft that only exists as a `drafts` row, so a store cannot silently lose a draft
/// whose display row is missing.
pub(crate) fn draft_message(draft: &store::Draft, account: &Account, folder_id: &str) -> Result<Message, OpError> {
    let to = draft.to.iter().map(address).collect::<Vec<_>>();
    Ok(Message {
        summary: MessageSummary {
            id: draft.id.clone(),
            thread_id: format!("thr_{}", draft.id),
            folder_id: folder_id.to_owned(),
            from: Address::new(account.name.clone(), account.email.clone()),
            to,
            subject: draft.subject.clone(),
            snippet: snippet(&draft.body_text),
            date: date(draft.updated_at)?,
            unread: false,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        cc: draft.cc.iter().map(address).collect(),
        bcc: draft.bcc.iter().map(address).collect(),
        body_text: draft.body_text.clone(),
        body_html: None,
        attachments: Vec::new(),
    })
}

fn address(row: &store::Address) -> Address {
    Address::new(row.name.clone(), row.email.clone())
}

fn store_address(address: &Address) -> store::Address {
    store::Address::new(address.name.clone(), address.email.clone())
}

pub(crate) fn attachment(row: store::Attachment) -> Attachment {
    Attachment { id: row.id, filename: row.filename, mime: row.mime, size: row.size }
}
