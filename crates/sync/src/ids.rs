//! Stable local identifiers and the threading placeholder.
//!
//! A folder id is `fld_` plus 16 hex characters of `BLAKE3(account id, wire name)`. It survives a
//! display-name rename because the wire name is what the server uses, and it is scoped to the
//! account so two accounts can hold a folder with the same name.
//!
//! A message id is `msg_` plus 16 hex characters of `BLAKE3(account id, Message-ID)` when the
//! message has a Message-ID, otherwise `msg_{folder}_{uidvalidity}_{uid}`. The first form survives a
//! folder move, a UIDVALIDITY change and a re-download; the second is the only option for mail that
//! has no Message-ID (a malformed message or a local draft-like note).

/// Hash a field list with a NUL separator so `("ab", "c")` and `("a", "bc")` cannot collide.
fn short_hash(fields: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for field in fields {
        hasher.update(field.as_bytes());
        hasher.update(&[0]);
    }
    let digest = hasher.finalize().to_hex();
    let text = digest.as_str();
    text.get(..16).unwrap_or(text).to_owned()
}

/// The local id of one account's folder, stable across renames of the display name.
#[must_use]
pub(crate) fn folder_id(account_id: &str, wire_name: &str) -> String {
    format!("fld_{}", short_hash(&[account_id, wire_name]))
}

/// The local id of one message.
///
/// `message_id` must already be normalised by [`normalise_message_id`]. When it is `None` the id is
/// tied to the folder and UID, which is the honest fallback: such a message cannot be recognised
/// after a move or a UIDVALIDITY change.
#[must_use]
pub(crate) fn message_id(account_id: &str, folder_id: &str, uid_validity: u32, uid: u32, message_id: Option<&str>) -> String {
    match message_id {
        Some(message_id) => format!("msg_{}", short_hash(&[account_id, message_id])),
        None => format!("msg_{folder_id}_{uid_validity}_{uid}"),
    }
}

/// The deterministic id of one attachment: the message id plus the MIME part index.
#[must_use]
pub(crate) fn attachment_id(message_id: &str, index: usize) -> String {
    format!("att_{message_id}_{index}")
}

/// The three stable candidates for a message that has a `Message-ID`, in preference order.
///
/// 1. `msg_<hash>`: the mailbox-wide identity.
/// 2. `msg_<hash>_<uid>`: a second message with the same `Message-ID` in the same folder.
/// 3. `msg_<hash>_<uid>_<uidvalidity>`: the second candidate was already taken by another folder.
///
/// The caller tries them in order against the cached identity and the global message table. The
/// UID makes the choice deterministic while envelopes are processed in ascending UID order.
#[must_use]
pub(crate) fn message_id_variants(account_id: &str, message_id: &str, uid: u32, uid_validity: u32) -> [String; 3] {
    let hash = short_hash(&[account_id, message_id]);
    [format!("msg_{hash}"), format!("msg_{hash}_{uid}"), format!("msg_{hash}_{uid}_{uid_validity}")]
}

/// The threading placeholder: the first `References` entry, else `In-Reply-To`, else `fallback`.
///
/// `fallback` is the message's own header Message-ID when it has one, so a reply that references it
/// lands in the same thread; a message without a Message-ID uses its local id. Full JWZ threading
/// (normalising subjects, merging the forest of references) is P9. This value is stable and cheap,
/// and a thread view can rebuild the real tree from the stored ids later.
#[must_use]
pub(crate) fn thread_id(references: &[String], in_reply_to: Option<&str>, fallback: &str) -> String {
    for reference in references {
        if let Some(reference) = normalise_message_id(reference) {
            return reference;
        }
    }
    if let Some(reply_to) = in_reply_to.and_then(normalise_message_id) {
        return reply_to;
    }
    fallback.to_owned()
}

/// Reduce one header value to the exact trimmed Message-ID, or `None` when it is unusable.
///
/// One pair of angle brackets is removed and the surrounding whitespace is trimmed; nothing else is
/// changed. There is no case-folding and no character substitution, so two raw values that differ
/// produce two different identities. An empty value, one over 998 characters and one containing a
/// control character are refused (the caller falls back to the folder/UID identity).
#[must_use]
pub(crate) fn normalise_message_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let inner = trimmed.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')).unwrap_or(trimmed);
    if inner.is_empty() || inner.len() > 998 || inner.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    Some(inner.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_ids_are_scoped_and_stable() {
        let first = folder_id("acc_1", "INBOX");
        assert_eq!(first, folder_id("acc_1", "INBOX"));
        assert_ne!(first, folder_id("acc_2", "INBOX"));
        assert_ne!(first, folder_id("acc_1", "Archive"));
        assert!(first.starts_with("fld_"));
        assert_eq!(first.len(), 4 + 16);
    }

    #[test]
    fn a_message_id_is_stable_across_folders_and_uidvalidity() {
        let in_inbox = message_id("acc_1", "fld_inbox", 1, 7, Some("a@example.com"));
        let in_archive = message_id("acc_1", "fld_archive", 99, 300, Some("a@example.com"));
        assert_eq!(in_inbox, in_archive, "a Message-ID identifies the message, not its folder");
        assert_ne!(in_inbox, message_id("acc_2", "fld_inbox", 1, 7, Some("a@example.com")));
    }

    #[test]
    fn a_message_without_a_message_id_falls_back_to_the_uid_identity() {
        let id = message_id("acc_1", "fld_inbox", 42, 7, None);
        assert_eq!(id, "msg_fld_inbox_42_7");
        assert_ne!(id, message_id("acc_1", "fld_inbox", 43, 7, None));
    }

    #[test]
    fn message_ids_are_normalised_or_refused() {
        assert_eq!(normalise_message_id(" <a@example.com> ").as_deref(), Some("a@example.com"));
        assert_eq!(normalise_message_id("a@example.com").as_deref(), Some("a@example.com"));
        assert_eq!(
            normalise_message_id("<one@example.com> <two@example.com>").as_deref(),
            Some("one@example.com> <two@example.com"),
            "the exact interior is hashed, never the first token"
        );
        assert_eq!(normalise_message_id(""), None);
        assert_eq!(normalise_message_id("<>"), None);
        assert_eq!(normalise_message_id("has space").as_deref(), Some("has space"));
        assert_eq!(normalise_message_id("bad\u{0}id"), None);
        assert_eq!(normalise_message_id(&"x".repeat(999)), None);
        assert_ne!(normalise_message_id("<A@example.com>"), normalise_message_id("<a@example.com>"), "case is part of the identity");
    }

    #[test]
    fn duplicate_message_ids_get_deterministic_candidates() {
        let variants = message_id_variants("acc_1", "a@example.com", 2, 10);
        assert_eq!(variants[0].len(), 20);
        assert_eq!(variants[1], format!("{}_2", variants[0]));
        assert_eq!(variants[2], format!("{}_2_10", variants[0]));
        assert_eq!(variants, message_id_variants("acc_1", "a@example.com", 2, 10));
        assert_ne!(variants[0], message_id_variants("acc_1", "a2@example.com", 2, 10)[0]);
    }

    #[test]
    fn the_thread_is_the_first_reference_then_the_reply_then_the_message() {
        let references = vec!["<root@example.com>".to_owned(), "<middle@example.com>".to_owned()];
        assert_eq!(thread_id(&references, Some("<parent@example.com>"), "msg_local"), "root@example.com");
        assert_eq!(thread_id(&[], Some("<parent@example.com>"), "msg_local"), "parent@example.com");
        assert_eq!(thread_id(&[], None, "msg_local"), "msg_local");
        assert_eq!(
            thread_id(&["<bad\u{0}id>".to_owned(), "<good@example.com>".to_owned()], None, "msg_local"),
            "good@example.com",
            "an unusable reference is skipped, not substituted"
        );
    }

    #[test]
    fn attachment_ids_are_per_part_and_unique() {
        assert_eq!(attachment_id("msg_abc", 0), "att_msg_abc_0");
        assert_ne!(attachment_id("msg_abc", 0), attachment_id("msg_abc", 1));
    }
}
