//! Folder discovery: LIST once, map special-use, create the local rows.
//!
//! The store keeps a folder's display name and role, never its wire name; the wire name is what
//! `SELECT` needs, so a target carries both for the duration of one run.

use std::collections::HashSet;

use postino_net::{RemoteFolder, SpecialUse};
use postino_store as store;

use crate::ids;
use crate::warnings::Warnings;

/// One folder to reconcile: the row that identifies it locally and the name the server expects.
pub(crate) struct TargetFolder {
    pub(crate) row: store::Folder,
    pub(crate) wire_name: String,
}

/// Discover the selectable folders, create or refresh their local rows, and warn about missing roles.
///
/// Local `unread`/`total` caches, UID state and `last_sync_at` are preserved from the existing row:
/// LIST is not a message scan. A `\Noselect` container is skipped; its children are separate LIST
/// entries and are kept. Labels and drafts are not touched by this function.
///
/// `\All` and `\Flagged` are aggregate views in RFC 6154: they contain messages that already live in
/// a concrete folder. Their rows are created (the role is displayed) but their messages are not
/// fetched: with a mailbox-wide `Message-ID` identity, syncing a view would move every message into
/// it and empty the folders the user actually browses.
pub(crate) fn discover(
    store: &store::Store,
    account: &store::Account,
    remote: &[RemoteFolder],
    warnings: &mut Warnings,
) -> Result<Vec<TargetFolder>, store::StoreError> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for folder in remote {
        if !folder.is_selectable() {
            continue;
        }
        let id = ids::folder_id(&account.id, &folder.wire_name);
        if !seen.insert(id.clone()) {
            // A server may list the same mailbox twice with different attributes; the first wins.
            continue;
        }
        let kind = kind_for(folder);
        let existing = store.get_folder(&id)?;
        let row = store::Folder {
            id,
            account_id: account.id.clone(),
            name: folder.name.clone(),
            kind,
            unread: existing.as_ref().map_or(0, |row| row.unread),
            total: existing.as_ref().map_or(0, |row| row.total),
            uid_validity: existing.as_ref().and_then(|row| row.uid_validity),
            uid_next: existing.as_ref().and_then(|row| row.uid_next),
            last_sync_at: existing.as_ref().and_then(|row| row.last_sync_at),
        };
        store.upsert_folder(&row)?;
        if is_aggregate_view(folder) {
            warnings.push(format!(
                "folder {:?} is a server aggregate view (\\All or \\Flagged); its messages are cached under their real folders",
                folder.name
            ));
            continue;
        }
        targets.push(TargetFolder { row, wire_name: folder.wire_name.clone() });
    }
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    if !targets.iter().any(|target| target.row.kind == store::FolderKind::Inbox) {
        warnings.push("the server lists no \\Inbox folder and no mailbox named INBOX; this account has no inbox");
    }
    if !targets.iter().any(|target| target.row.kind == store::FolderKind::Sent) {
        warnings.push("the server lists no folder with the \\Sent attribute; sent mail is not cached");
    }
    if !targets.iter().any(|target| target.row.kind == store::FolderKind::Drafts) {
        warnings.push("the server lists no folder with the \\Drafts attribute; the server's drafts are not cached");
    }
    Ok(targets)
}

/// Whether the folder is an aggregate view rather than a place mail is filed.
pub(crate) fn is_aggregate_view(folder: &RemoteFolder) -> bool {
    matches!(folder.special_use, Some(SpecialUse::All | SpecialUse::Flagged))
}

/// Map an RFC 6154 special-use attribute to the store's role.
///
/// INBOX is the one name-based case: RFC 3501 fixes its name and a server is not required to send a
/// special-use attribute for it. Everything else is a `Custom` folder unless the server labelled it;
/// guessing roles from names would invent a Sent folder the user does not have.
pub(crate) fn kind_for(folder: &RemoteFolder) -> store::FolderKind {
    match folder.special_use {
        Some(SpecialUse::Inbox) => store::FolderKind::Inbox,
        Some(SpecialUse::Sent) => store::FolderKind::Sent,
        Some(SpecialUse::Drafts) => store::FolderKind::Drafts,
        Some(SpecialUse::Trash) => store::FolderKind::Trash,
        Some(SpecialUse::Junk) => store::FolderKind::Spam,
        Some(SpecialUse::Archive) => store::FolderKind::Archive,
        Some(SpecialUse::Flagged) => store::FolderKind::Starred,
        Some(SpecialUse::All) | None => {
            if folder.name.eq_ignore_ascii_case("INBOX") || folder.wire_name.eq_ignore_ascii_case("INBOX") {
                store::FolderKind::Inbox
            } else {
                store::FolderKind::Custom
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, special_use: Option<SpecialUse>, attributes: &[&str]) -> RemoteFolder {
        RemoteFolder {
            name: name.to_owned(),
            wire_name: name.to_owned(),
            delimiter: Some('/'),
            attributes: attributes.iter().map(|attribute| (*attribute).to_owned()).collect(),
            special_use,
        }
    }

    #[test]
    fn special_use_maps_to_the_store_roles() {
        assert_eq!(kind_for(&remote("Posta", Some(SpecialUse::Inbox), &[])), store::FolderKind::Inbox);
        assert_eq!(kind_for(&remote("Sent", Some(SpecialUse::Sent), &[])), store::FolderKind::Sent);
        assert_eq!(kind_for(&remote("Bozze", Some(SpecialUse::Drafts), &[])), store::FolderKind::Drafts);
        assert_eq!(kind_for(&remote("Cestino", Some(SpecialUse::Trash), &[])), store::FolderKind::Trash);
        assert_eq!(kind_for(&remote("Spam", Some(SpecialUse::Junk), &[])), store::FolderKind::Spam);
        assert_eq!(kind_for(&remote("Archivio", Some(SpecialUse::Archive), &[])), store::FolderKind::Archive);
        assert_eq!(kind_for(&remote("Preferiti", Some(SpecialUse::Flagged), &[])), store::FolderKind::Starred);
        assert_eq!(kind_for(&remote("Tutto", Some(SpecialUse::All), &[])), store::FolderKind::Custom);
    }

    #[test]
    fn inbox_is_recognised_by_name_when_the_server_omits_the_attribute() {
        assert_eq!(kind_for(&remote("INBOX", None, &[])), store::FolderKind::Inbox);
        assert_eq!(kind_for(&remote("inbox", None, &[])), store::FolderKind::Inbox);
        assert_eq!(kind_for(&remote("Inbox/2024", None, &[])), store::FolderKind::Custom);
        assert_eq!(kind_for(&remote("Sent", None, &[])), store::FolderKind::Custom, "roles are never guessed from names");
    }

    #[test]
    fn aggregate_views_are_not_message_folders() {
        assert!(is_aggregate_view(&remote("All Mail", Some(SpecialUse::All), &[])));
        assert!(is_aggregate_view(&remote("Starred", Some(SpecialUse::Flagged), &[])));
        assert!(!is_aggregate_view(&remote("INBOX", Some(SpecialUse::Inbox), &[])));
        assert!(!is_aggregate_view(&remote("Archive", Some(SpecialUse::Archive), &[])));
        assert!(!is_aggregate_view(&remote("Custom", None, &[])));
    }

    #[test]
    fn a_folder_id_is_scoped_to_the_account() {
        assert_ne!(ids::folder_id("acc_1", "INBOX"), ids::folder_id("acc_2", "INBOX"));
    }
}
