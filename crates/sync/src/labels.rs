//! Map Gmail's `X-GM-LABELS` onto local labels.
//!
//! Gmail returns system labels (`\Inbox`, `\Sent`, ...) and the user's labels in the same list.
//! Folders already represent the system ones, so only custom labels become local labels. The sync
//! remembers which ids it applied in encrypted `meta`, so a later re-sync can remove a label the
//! sender no longer has without touching a label the person applied by hand.

use postino_store as store;

use crate::SyncError;

const MAX_REMOTE_LABELS: usize = 64;
const MAX_LABEL_NAME_CHARS: usize = 48;
const LABEL_COLOR: &str = "muted";

/// Apply the remote labels to one message and remember the auto-applied ids.
pub(crate) fn apply(store: &store::Store, account_id: &str, message_id: &str, remote: &[String]) -> Result<(), SyncError> {
    let mut desired: Vec<String> = Vec::new();
    for raw in remote {
        let name = raw.trim();
        if name.starts_with('\\') || !valid_name(name) {
            continue;
        }
        let label = store.ensure_account_label(account_id, name, LABEL_COLOR).map_err(SyncError::Store)?;
        if !desired.contains(&label.id) {
            desired.push(label.id);
        }
        if desired.len() == MAX_REMOTE_LABELS {
            break;
        }
    }

    let key = format!("{}{message_id}", store::REMOTE_LABELS_KEY_PREFIX);
    let previous: Vec<String> =
        store.get_meta(&key).map_err(SyncError::Store)?.and_then(|value| serde_json::from_str(&value).ok()).unwrap_or_default();
    let ids = [message_id.to_owned()];

    for label_id in previous.iter().filter(|label_id| !desired.contains(label_id)) {
        // A label deleted by the person is already gone; removing its assignment is a no-op.
        if store.get_label(label_id).map_err(SyncError::Store)?.is_some() {
            store.remove_label(label_id, &ids).map_err(SyncError::Store)?;
        }
    }
    for label_id in desired.iter().filter(|label_id| !previous.contains(label_id)) {
        store.apply_label(label_id, &ids).map_err(SyncError::Store)?;
    }

    if desired.is_empty() {
        store.delete_meta(&key).map_err(SyncError::Store)?;
    } else {
        let value = serde_json::to_string(&desired)
            .map_err(|error| SyncError::Store(store::StoreError::Corrupt { table: "meta", detail: error.to_string() }))?;
        store.set_meta(&key, &value).map_err(SyncError::Store)?;
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_LABEL_NAME_CHARS
        && !name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}'
                )
        })
}
