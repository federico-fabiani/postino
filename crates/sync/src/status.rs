//! The persistent sync status: one JSON value under `meta`, readable without starting a sync.
//!
//! The status never carries a server name, an address, a credential or message content. The error
//! field is a fixed sentence for the failure category; the typed [`crate::SyncError`] returned by
//! the run carries the detail to whoever asked for it, and never to the database.

use postino_store as store;
use serde::{Deserialize, Serialize};

/// `meta` key prefix; the account id follows. The convention lives in the store crate because
/// `Store::delete_account` removes the same key with the account.
pub const STATUS_KEY_PREFIX: &str = store::SYNC_STATUS_KEY_PREFIX;

/// The phase of the last attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncPhase {
    /// No run is in progress and the last one either succeeded or was never started.
    Idle,
    /// A run is in progress. A crash leaves this state until the next run resets it.
    Syncing,
    /// The last attempt stopped with an error or was cancelled.
    Error,
}

/// What the last sync attempt did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    /// Phase of the last attempt.
    pub state: SyncPhase,
    /// Unix milliseconds when the last run started.
    pub last_attempt: i64,
    /// Unix milliseconds when a run last completed successfully, or `null`.
    pub last_success: Option<i64>,
    /// A fixed sentence for the failure category, never server text. `null` after a success.
    pub error: Option<String>,
    /// Folders processed by this run.
    pub folders_done: u32,
    /// Folders the server listed at the start of this run.
    pub folders_total: u32,
    /// Messages ingested by this run: new and refreshed.
    pub messages: u32,
}

/// The `meta` key for one account.
#[must_use]
pub fn status_key(account_id: &str) -> String {
    format!("{STATUS_KEY_PREFIX}{account_id}")
}

/// Read the last status of one account id without touching the network.
///
/// This is the engine/CLI form: it does not need a full [`store::Account`] row. `None` when the
/// account has never synced.
///
/// # Errors
///
/// A value that was written by a future build, or damaged on disk, is a [`store::StoreError::Encode`]
/// rather than a silently empty status: a caller that shows sync health must not invent it. A sync
/// run itself tolerates an unreadable status and starts from zero.
pub fn read_status_for(store: &store::Store, account_id: &str) -> Result<Option<SyncStatus>, store::StoreError> {
    let Some(raw) = store.get_meta(&status_key(account_id))? else {
        return Ok(None);
    };
    serde_json::from_str(&raw).map(Some).map_err(store::StoreError::from)
}

/// Read the last status without touching the network. `None` when the account has never synced.
///
/// # Errors
///
/// A value that was written by a future build, or damaged on disk, is a [`store::StoreError::Encode`]
/// rather than a silently empty status: a caller that shows sync health must not invent it. A sync
/// run itself tolerates an unreadable status and starts from zero.
pub fn read_status(store: &store::Store, account: &store::Account) -> Result<Option<SyncStatus>, store::StoreError> {
    read_status_for(store, &account.id)
}

/// Write one status value, overwriting the previous one.
pub(crate) fn write_status(store: &store::Store, account: &store::Account, status: &SyncStatus) -> Result<(), store::StoreError> {
    let raw = serde_json::to_string(status)?;
    store.set_meta(&status_key(&account.id), &raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_json_is_the_documented_shape() {
        let status = SyncStatus {
            state: SyncPhase::Syncing,
            last_attempt: 1_700_000_000_000,
            last_success: None,
            error: None,
            folders_done: 2,
            folders_total: 8,
            messages: 40,
        };
        let value = serde_json::to_value(&status).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value,
            serde_json::json!({
                "state": "syncing",
                "last_attempt": 1_700_000_000_000i64,
                "last_success": null,
                "error": null,
                "folders_done": 2,
                "folders_total": 8,
                "messages": 40
            })
        );
        let parsed: SyncStatus = serde_json::from_value(value).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(parsed, status);
    }

    #[test]
    fn state_names_are_lowercase() {
        for (phase, name) in [(SyncPhase::Idle, "idle"), (SyncPhase::Syncing, "syncing"), (SyncPhase::Error, "error")] {
            assert_eq!(serde_json::to_value(phase).unwrap_or_default(), serde_json::Value::String(name.to_owned()));
        }
    }

    #[test]
    fn the_key_is_scoped_by_an_opaque_id_only() {
        assert_eq!(status_key("acc_1"), "sync_status:acc_1");
    }
}
