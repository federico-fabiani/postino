//! Import one RFC 5322 message from a file into a local folder.
//!
//! This reuses the same ingest path as a server fetch, so an imported message gets identical MIME
//! sanitization, attachment limits, body indexing and PGP raw retention. It deliberately does not
//! treat a matching Message-ID in another folder as a move: a person importing an `.eml` file must
//! never relocate cached mail.

use postino_net::{Envelope, Flag};
use postino_store as store;

use crate::ingest::{FolderIndex, ingest};
use crate::warnings::Warnings;
use crate::{Context, SyncError, SyncLimits};

/// The result of one local import.
pub struct Imported {
    /// Stable local id of the stored message.
    pub message_id: String,
    /// MIME sanitization and attachment warnings, already bounded.
    pub warnings: Vec<String>,
}

/// Parse and store one message in `folder`.
///
/// `uid` is synthetic: the folder is local and never synced, but the ingest path has a single
/// identity shape for every message and needs a stable number.
pub fn import_message(
    store: &store::Store,
    account: &store::Account,
    folder: &store::Folder,
    raw: &[u8],
    limits: &SyncLimits,
) -> Result<Imported, SyncError> {
    let limits = limits.clamped();
    if raw.len() > limits.max_message_bytes {
        return Err(SyncError::Protocol(format!("the message is {} bytes, over the {}-byte import limit", raw.len(), limits.max_message_bytes)));
    }
    let snapshot = store.folder_message_flags(&folder.id)?;
    let uid = snapshot
        .iter()
        .filter_map(|row| row.uid)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| SyncError::Protocol("the local import folder has exhausted its message numbers".to_owned()))?;
    let uid_validity = folder.uid_validity.unwrap_or(1);
    // The MIME parse inside `ingest` is authoritative; these are only the envelope fallbacks for a
    // file whose headers are missing or unusable.
    let envelope = Envelope {
        uid,
        flags: vec![Flag::Seen],
        internal_date: None,
        size: u32::try_from(raw.len()).unwrap_or(u32::MAX),
        subject: String::new(),
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        message_id: None,
        in_reply_to: None,
        labels: Vec::new(),
        references: Vec::new(),
        list_id: None,
    };
    let mut index = FolderIndex::new(&snapshot);
    let mut warnings = Warnings::default();
    let context = Context {
        store,
        account,
        limits: &limits,
        cancel: &std::sync::atomic::AtomicBool::new(false),
        gmail_labels: false,
        allow_cross_folder_move: false,
    };
    let ingested = ingest(&context, folder, &mut index, uid_validity, &envelope, raw, &mut warnings)?
        .ok_or_else(|| SyncError::Protocol("the imported message has no usable identity".to_owned()))?;
    Ok(Imported { message_id: ingested.message_id, warnings: warnings.into_vec() })
}
