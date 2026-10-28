//! Map one fetched RFC 5322 message onto a store row and its attachments.
//!
//! The envelope carries the cheap list fields and the flags; the MIME parse is authoritative for the
//! body, the decoded headers and the attachment bytes. When a header is missing from the parse the
//! envelope is the fallback, so a message with no `Date` header still gets its `INTERNALDATE`.

use std::collections::HashMap;

use postino_mime::ParsedMessage;
use postino_net::{Address as EnvelopeAddress, Envelope, Flag};
use postino_store as store;

use crate::ids;
use crate::warnings::Warnings;
use crate::{Context, SyncError, SyncLimits};

/// One message that was written to the store.
pub(crate) struct Ingested {
    /// The stable local id.
    pub(crate) message_id: String,
    /// Whether this was the first time the message was cached.
    pub(crate) new: bool,
    /// Whether the server's flags disagree with the local read/star state.
    pub(crate) drift: bool,
    /// Attachment blobs written by this call.
    pub(crate) attachments_stored: u32,
}

/// The cached identity and flags of one folder, plus the ids inserted earlier in this run.
///
/// One folder-wide query supplies [`FolderIndex::cached`]; [`FolderIndex::inserted`] records what
/// this run has already written, so a second message with the same `Message-ID` in the same folder
/// sees the first one and gets a disambiguated id.
pub(crate) struct FolderIndex {
    cached: HashMap<String, CachedRow>,
    inserted: HashMap<String, u32>,
}

#[derive(Clone, Copy)]
struct CachedRow {
    uid: Option<u32>,
    unread: bool,
    starred: bool,
}

#[derive(Clone, Copy)]
struct Existing {
    unread: bool,
    starred: bool,
}

impl FolderIndex {
    /// Build the snapshot from the repository's one-query folder read.
    pub(crate) fn new(rows: &[store::FolderMessageFlags]) -> Self {
        let cached = rows.iter().map(|row| (row.message_id.clone(), CachedRow { uid: row.uid, unread: row.unread, starred: row.starred })).collect();
        Self { cached, inserted: HashMap::new() }
    }
}

/// Parse and store one message. `Ok(None)` means the message was skipped and a warning explains why.
///
/// The message row, every attachment row and `has_attachments` are one transaction. Blob files are
/// written before the row transaction; a failure after a blob write leaves an unreferenced,
/// content-addressed file, which is harmless and reused by the next attempt.
pub(crate) fn ingest(
    context: &Context<'_>,
    folder: &store::Folder,
    index: &mut FolderIndex,
    uid_validity: u32,
    envelope: &Envelope,
    raw: &[u8],
    warnings: &mut Warnings,
) -> Result<Option<Ingested>, SyncError> {
    let store = context.store;
    let account = context.account;
    let limits = context.limits;
    let uid = envelope.uid;
    let mut parsed = match postino_mime::parse(raw, &mime_limits(limits)) {
        Ok(parsed) => parsed,
        Err(error) => {
            warnings.push(format!("message UID {uid} could not be parsed and was skipped: {error}"));
            return Ok(None);
        }
    };

    let normalised = normalise_id(&parsed, envelope, uid, warnings);
    let Some((id, existing)) = choose_identity(store, folder, index, &account.id, normalised.as_deref(), uid, uid_validity)? else {
        warnings.push(format!("message UID {uid} was skipped: every stable id candidate is taken by another message"));
        return Ok(None);
    };
    // MIME warnings are the honest record of a body or attachment list that was cut: part limits,
    // depth limits, truncated bodies, sanitizer strips and blocked remote content all land here.
    for warning in &parsed.warnings {
        warnings.push(format!("message {id}: {warning}"));
    }
    // A reply points at a Message-ID, so a root's thread id must be its own Message-ID for children
    // to link to it. Only a message without one falls back to the local id.
    let thread = ids::thread_id(&parsed.references, parsed.in_reply_to.as_deref(), normalised.as_deref().unwrap_or(&id));

    let server_unread = !envelope.flags.contains(&Flag::Seen);
    let server_starred = envelope.flags.contains(&Flag::Flagged);
    let (unread, starred, new, drift) = match existing {
        Some(local) => (local.unread, local.starred, false, local.unread != server_unread || local.starred != server_starred),
        None => (server_unread, server_starred, true, false),
    };

    let body_text = match parsed.text.take() {
        Some(text) if !text.trim().is_empty() => text,
        _ => parsed.html.as_deref().map(postino_mime::html_to_text).unwrap_or_default(),
    };
    let date = parsed
        .date
        .map(|date| date.timestamp_millis())
        .or_else(|| envelope.internal_date.map(|date| date.timestamp_millis()))
        .unwrap_or_else(crate::now_ms);
    let size = if envelope.size > 0 { u64::from(envelope.size) } else { u64::try_from(raw.len()).unwrap_or(u64::MAX) };

    let subject = if parsed.subject.is_empty() { envelope.subject.clone() } else { parsed.subject.clone() };
    let from = parsed
        .from
        .first()
        .map(parsed_address)
        .or_else(|| envelope.from.first().map(envelope_address))
        .unwrap_or_else(|| store::Address::new(String::new(), String::new()));
    let to = address_list(&parsed, |message| &message.to, &envelope.to);
    let cc = address_list(&parsed, |message| &message.cc, &envelope.cc);
    let bcc = address_list(&parsed, |message| &message.bcc, &envelope.bcc);
    let reply_to = parsed.reply_to.iter().map(parsed_address).collect::<Vec<_>>();

    let mut attachments = Vec::with_capacity(parsed.attachments.len());
    let mut stored = 0u32;
    for (index, part) in parsed.attachments.iter().enumerate() {
        let attachment_id = ids::attachment_id(&id, index);
        let over_limit = part.size > limits.max_attachment_bytes;
        let mut row =
            store::Attachment { id: attachment_id, filename: part.filename.clone(), mime: part.mime.clone(), size: part.size, blob_hash: None };
        if part.truncated || over_limit {
            let reason = if over_limit {
                format!("it is {} bytes, over the {}-byte per-attachment limit", part.size, limits.max_attachment_bytes)
            } else {
                "the MIME parser did not decode all of it".to_owned()
            };
            warnings.push(format!(
                "message UID {uid}: attachment {:?} was kept as metadata but its bytes were not stored because {reason}",
                warning_name(&part.filename)
            ));
        } else {
            let hash = store.blobs().put(&part.data).map_err(SyncError::Store)?;
            row.size = u64::try_from(part.data.len()).unwrap_or(u64::MAX);
            row.blob_hash = Some(hash);
            stored = stored.saturating_add(1);
        }
        attachments.push(row);
    }

    // The sanitized HTML no longer contains the remote references, so the parser's count is the only
    // place the fact exists. Cap it before it reaches the bounded store column and the reading pane;
    // a hostile message cannot make this overflow or grow without bound.
    let remote_images = u32::try_from(parsed.remote_images_blocked).unwrap_or(u32::MAX).min(store::MAX_REMOTE_IMAGES);

    // PGP/MIME is the only received mail whose exact bytes cannot be rebuilt from the parsed row:
    // verification needs the signed entity as it arrived (CRLF, folding, the detached signature).
    // The blob is written before the row, like an attachment, and the row only names it afterwards.
    // `max_message_bytes` bounds it already: the MIME parse above refused a larger entity, so this
    // branch can never keep bytes the run was allowed to fetch but not store.
    let raw_blob_hash = if is_pgp_mime(&parsed) { Some(store.blobs().put(raw).map_err(SyncError::Store)?) } else { None };

    let row = store::Message {
        summary: store::MessageSummary {
            id: id.clone(),
            account_id: account.id.clone(),
            folder_id: folder.id.clone(),
            thread_id: thread,
            from,
            to,
            subject,
            snippet: snippet(&body_text),
            date,
            unread,
            starred,
            has_attachments: !attachments.is_empty(),
            labels: Vec::new(),
        },
        uid: Some(uid),
        message_id: normalised,
        cc,
        bcc,
        body_text,
        body_html: parsed.html.take(),
        remote_images,
        size: Some(size),
    };
    let (in_reply_to, references) = ids::thread_headers(&parsed.references, parsed.in_reply_to.as_deref());
    store.upsert_synced_message_with_refs(&row, &attachments, in_reply_to.as_deref(), &references).map_err(SyncError::Store)?;
    // NULL is the honest `Not stored` state for ordinary mail and for a row cached before v9; an
    // explicit refresh may fetch the entity again, but nothing here re-derives it from the body.
    store.set_message_raw_blob_hash(&id, raw_blob_hash.as_deref()).map_err(SyncError::Store)?;
    store.set_message_reply_to(&id, &reply_to).map_err(SyncError::Store)?;
    // Gmail labels are display metadata, not folders. On a Gmail session an empty list still
    // reconciles, so a label removed on the server also disappears locally.
    if context.gmail_labels {
        crate::labels::apply(store, &account.id, &id, &envelope.labels)?;
    }
    // RFC 2369/8058 unsubscribe targets are header-only. They live beside the reply-to override in
    // encrypted `meta`, within the same small bound as a header value, so no schema migration is
    // needed and a later message can be deleted without leaving them behind.
    let unsubscribe: Vec<&str> = parsed.headers.get_all("List-Unsubscribe").take(4).map(str::trim).filter(|value| !value.is_empty()).collect();
    if !unsubscribe.is_empty() {
        // RFC 8058 spells the field `List-Unsubscribe=One-Click`; accept that exact token, not a
        // string that merely contains `one-click` (such as `No-One-Click`).
        let one_click = parsed.headers.get_all("List-Unsubscribe-Post").any(|value| {
            value.split(',').any(|part| {
                part.split_once('=').is_some_and(|(name, token)| {
                    name.trim().eq_ignore_ascii_case("List-Unsubscribe") && token.trim().eq_ignore_ascii_case("One-Click")
                })
            })
        });
        let value = serde_json::json!({ "targets": unsubscribe, "one_click": one_click }).to_string();
        if value.len() <= 4096 {
            store.set_meta(&format!("{}{id}", store::UNSUBSCRIBE_KEY_PREFIX), &value).map_err(SyncError::Store)?;
        }
    }
    // P9c's forward loop guard. The headers are recorded in the encrypted `meta` table because v7
    // has no header column and P9c must not migrate; a normal message writes nothing.
    let guard = store::RuleGuard {
        auto_submitted: parsed.headers.get("Auto-Submitted").is_some(),
        x_postino_rule: parsed.headers.get("X-Postino-Rule").is_some(),
    };
    if guard.any() {
        store.set_rule_guard(&account.id, &id, guard).map_err(SyncError::Store)?;
    }
    index.inserted.insert(id.clone(), uid);
    Ok(Some(Ingested { message_id: id, new, drift, attachments_stored: stored }))
}

/// Pick the stable local id for one message.
///
/// The plain `msg_<hash>` is the mailbox-wide identity. If it is already cached in **this** folder
/// with a different UID, the message is a second copy with the same `Message-ID`: try
/// `msg_<hash>_<uid>`, then `msg_<hash>_<uid>_<uidvalidity>`. If the plain id exists in another
/// folder it is a move, so the id and the local flags are reused. A locally written Sent/Archive row
/// with the same `Message-ID` and no UID (the Sent copy of a submitted draft) is preferred,
/// account-wide, so a server copy that arrives before or after the local move — and even after moving
/// to Archive — is one row; if the server copy was already cached first, the stale local row is
/// deleted. Drafts and Trash rows are never adopted: a Sent copy moved back to Drafts is being edited.
/// `None` means every candidate is taken; the caller skips the message rather than overwrite another
/// row.
fn choose_identity(
    store: &store::Store,
    folder: &store::Folder,
    index: &FolderIndex,
    account_id: &str,
    message_id: Option<&str>,
    uid: u32,
    uid_validity: u32,
) -> Result<Option<(String, Option<Existing>)>, SyncError> {
    let Some(message_id) = message_id else {
        let id = ids::message_id(account_id, &folder.id, uid_validity, uid, None);
        let existing = index.cached.get(&id).map(|row| Existing { unread: row.unread, starred: row.starred });
        return Ok(Some((id, existing)));
    };
    let variants = ids::message_id_variants(account_id, message_id, uid, uid_validity);
    // A locally written Sent/Archive row (the Sent copy of a submitted draft) has no UID. It is found
    // first so that a provider auto-save or a sync that ran before the local move adopts it instead of
    // creating a second row; account-wide, so a server copy that moved to Archive is still the same
    // message. Drafts/Trash are excluded by the store lookup: a moved-back Sent copy is being edited.
    let local = store.local_message_for_id(account_id, message_id)?;
    if let Some(local_id) = &local
        && let Some(cached) = index.cached.get(&variants[0])
        && cached.uid.is_some()
    {
        // The server copy is already cached under the plain identity: the uid-null row is a duplicate
        // left by an older finalize. Remove it and let the normal identity win.
        store.delete_message(local_id).map_err(SyncError::Store)?;
    }
    let local = if let Some(cached) = index.cached.get(&variants[0]) { if cached.uid.is_some() { None } else { local } } else { local };
    for (position, variant) in variants.iter().enumerate() {
        if let Some(row) = index.cached.get(variant) {
            if row.uid == Some(uid) {
                return Ok(Some((variant.clone(), Some(Existing { unread: row.unread, starred: row.starred }))));
            }
            continue;
        }
        if let Some(other) = index.inserted.get(variant) {
            if *other == uid {
                return Ok(Some((variant.clone(), None)));
            }
            continue;
        }
        if let Some(existing) = store.message_flags(variant)? {
            if position == 0 && existing.folder_id != folder.id {
                // The same Message-ID in another folder is a move: keep the row and its labels.
                return Ok(Some((variant.clone(), Some(Existing { unread: existing.unread, starred: existing.starred }))));
            }
            continue;
        }
        if position == 0
            && let Some(local) = &local
            && let Some(flags) = store.message_flags(local)?
        {
            // The local row is the same message as this server copy: give it the UID (and, through
            // the upsert, the folder it arrived in) instead of inserting a duplicate.
            return Ok(Some((local.clone(), Some(Existing { unread: flags.unread, starred: flags.starred }))));
        }
        return Ok(Some((variant.clone(), None)));
    }
    Ok(None)
}

/// Whether the root entity is PGP/MIME: `multipart/signed` or `multipart/encrypted`.
///
/// Structure only: the root media type is read from the parsed headers, and no packet, key or
/// signature is inspected. Prepared mail that is signed but not OpenPGP (S/MIME) still keeps its
/// bytes, because retention is keyed on the container and the cost is one bounded blob; P12c2b
/// splits the signed entity and its signature out of the same bytes.
fn is_pgp_mime(parsed: &ParsedMessage) -> bool {
    parsed.headers.get("Content-Type").is_some_and(|value| {
        let media = value.split(';').next().unwrap_or_default().trim();
        media.eq_ignore_ascii_case("multipart/signed") || media.eq_ignore_ascii_case("multipart/encrypted")
    })
}

/// The MIME bounds for a sync run: the per-message cap and the per-attachment cap, both clamped.
fn mime_limits(limits: &SyncLimits) -> postino_mime::Limits {
    let max_message = limits.max_message_bytes.min(postino_mime::Limits::HARD_MAX_BYTES);
    let max_part = usize::try_from(limits.max_attachment_bytes).unwrap_or(usize::MAX).min(postino_mime::Limits::HARD_MAX_PART_BYTES);
    postino_mime::Limits {
        max_bytes: max_message,
        max_part_bytes: max_part,
        max_total_attachment_bytes: max_part.saturating_mul(4).min(postino_mime::Limits::HARD_MAX_TOTAL_ATTACHMENT_BYTES),
        ..postino_mime::Limits::default()
    }
}

/// Pick the Message-ID the parse and the envelope agree on, warning when the header is unusable.
fn normalise_id(parsed: &ParsedMessage, envelope: &Envelope, uid: u32, warnings: &mut Warnings) -> Option<String> {
    let candidate = parsed.message_id.as_deref().or(envelope.message_id.as_deref());
    let normalised = candidate.and_then(ids::normalise_message_id);
    if candidate.is_some() && normalised.is_none() {
        warnings.push(format!("message UID {uid}: the Message-ID header is unusable; the local id is tied to the folder and UID"));
    }
    normalised
}

fn parsed_address(mailbox: &postino_mime::Mailbox) -> store::Address {
    store::Address::new(mailbox.name.clone().unwrap_or_default(), mailbox.address.clone())
}

fn envelope_address(address: &EnvelopeAddress) -> store::Address {
    let mailbox = address.mailbox.clone().unwrap_or_default();
    let host = address.host.clone().unwrap_or_default();
    let email = if host.is_empty() { mailbox } else { format!("{mailbox}@{host}") };
    store::Address::new(address.name.clone().unwrap_or_default(), email)
}

/// Prefer the decoded MIME list; fall back to the envelope when the message had no such header.
fn address_list(
    parsed: &ParsedMessage,
    mine: impl Fn(&ParsedMessage) -> &[postino_mime::Mailbox],
    theirs: &[EnvelopeAddress],
) -> Vec<store::Address> {
    let list = mine(parsed);
    if list.is_empty() { theirs.iter().map(envelope_address).collect() } else { list.iter().map(parsed_address).collect() }
}

/// A short, collapsed preview for the list row. `postino-core` owns the display policy; sync stores
/// a bounded plain-text prefix so the engine can show a list without reading the body.
fn snippet(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= 160 {
            break;
        }
    }
    out.chars().take(160).collect()
}

/// A server-controlled attachment name reduced to printable text for a warning.
fn warning_name(name: &str) -> String {
    name.chars().map(|ch| if ch.is_control() { '?' } else { ch }).take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snippet_collapses_whitespace_and_is_bounded() {
        assert_eq!(snippet("hello\r\n\tworld  "), "hello world");
        let long = "x".repeat(400);
        assert_eq!(snippet(&long).len(), 160);
        assert_eq!(snippet("   "), "");
    }

    #[test]
    fn mime_limits_follow_the_sync_limits_and_the_hard_ceilings() {
        let limits = SyncLimits { max_message_bytes: 1024, max_attachment_bytes: 2048, ..SyncLimits::default() };
        let mime = mime_limits(&limits);
        assert_eq!(mime.max_bytes, 1024);
        assert_eq!(mime.max_part_bytes, 2048);
        assert_eq!(mime.max_total_attachment_bytes, 8192);

        let huge = SyncLimits { max_message_bytes: usize::MAX, max_attachment_bytes: u64::MAX, ..SyncLimits::default() };
        let mime = mime_limits(&huge);
        assert_eq!(mime.max_bytes, postino_mime::Limits::HARD_MAX_BYTES);
        assert_eq!(mime.max_part_bytes, postino_mime::Limits::HARD_MAX_PART_BYTES);
        assert_eq!(mime.max_total_attachment_bytes, postino_mime::Limits::HARD_MAX_TOTAL_ATTACHMENT_BYTES);
    }

    fn is_pgp_mime_for(content_type: &str) -> bool {
        let raw = format!("From: a@example.com\r\nContent-Type: {content_type}\r\n\r\nbody\r\n");
        let parsed = postino_mime::parse(raw.as_bytes(), &postino_mime::Limits::default()).expect("parse");
        is_pgp_mime(&parsed)
    }

    #[test]
    fn raw_retention_is_decided_by_the_root_media_type() {
        assert!(is_pgp_mime_for("multipart/signed; protocol=\"application/pgp-signature\"; micalg=pgp-sha256; boundary=b"));
        assert!(is_pgp_mime_for("Multipart/Encrypted; protocol=\"application/pgp-encrypted\"; boundary=b"));
        assert!(!is_pgp_mime_for("multipart/mixed; boundary=b"));
        assert!(!is_pgp_mime_for("text/plain"));
    }

    #[test]
    fn an_envelope_address_joins_mailbox_and_host() {
        let address =
            EnvelopeAddress { name: Some("Giulia".to_owned()), adl: None, mailbox: Some("giulia".to_owned()), host: Some("example.com".to_owned()) };
        let mapped = envelope_address(&address);
        assert_eq!(mapped.name, "Giulia");
        assert_eq!(mapped.email, "giulia@example.com");

        let bare = EnvelopeAddress { name: None, adl: None, mailbox: Some("group".to_owned()), host: None };
        assert_eq!(envelope_address(&bare).email, "group");
    }
}
