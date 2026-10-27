//! The outbox journal: the engine-side types and policy helpers.
//!
//! The state machine itself lives in `postino-store` (leasing and transitions) and in the engine
//! methods that call it. This module owns the constants, the JSON envelope stored in
//! `outbox.envelope_json`, the read-only views returned by the operations, and the backoff policy.
//!
//! Two rules are load-bearing and documented here:
//!
//! - **A row is leased before any network call.** A crash can therefore only leave `sending`, never
//!   a message sent without a journal row. A stale `sending` row becomes `uncertain`, never queued.
//! - **The exact bytes are persisted first.** The queue path serializes, writes the encrypted blob
//!   and only then inserts the row. A retry sends the same bytes; editing the draft cannot change a
//!   queued submission.

use postino_store::OutboxEntry;
use serde::{Deserialize, Serialize};

/// Default undo window before the first attempt, in milliseconds (5 s).
pub const UNDO_MS: i64 = 5_000;
/// A `sending` row whose lease is older than this is treated as uncertain after a crash; the same
/// window owns a claimed Sent append.
///
/// It must be longer than the longest **supported** transfer. The network client aborts a single
/// body/APPEND literal after [`postino_net::MAX_BODY_TRANSFER`] (30 minutes), and connect + TLS +
/// auth + reply add at most a few command timeouts. 45 minutes leaves 15 minutes of slack, so a live
/// submission can never outlive its lease, while a crashed one is resolved in well under an hour.
/// A transfer slower than ~23 KiB/s for the 40 MiB ceiling is refused by the client budget instead
/// of being silently allowed to run past the lease.
pub const LEASE_MS: i64 = 45 * 60 * 1_000;
/// Lease for one Sent append. Same window as a submission: an APPEND of the same 40 MiB message has
/// the same bounded transfer time, and a second worker must not append the same row concurrently.
pub const APPEND_LEASE_MS: i64 = LEASE_MS;
/// First retry delay, in milliseconds.
pub const BACKOFF_BASE_MS: i64 = 30 * 1_000;
/// Maximum retry delay, in milliseconds.
pub const BACKOFF_MAX_MS: i64 = 60 * 60 * 1_000;
/// Attempt ceiling: the eighth failure stops the row.
pub const MAX_ATTEMPTS: u32 = 8;
/// Hard ceiling for one serialized submission. A 25 MiB attachment becomes about 33 MiB after
/// base64, so the ceiling is above that and below the blob store's 64 MiB.
pub const MAX_SUBMISSION_BYTES: u64 = 40 * 1024 * 1024;
/// `meta` key prefix for an account whose queue stopped on an authentication failure.
pub const HALT_KEY_PREFIX: &str = "outbox_halt:";
/// How long a terminal journal row (`sent`, `cancelled`, `failed`) is kept. Pruning is bounded and
/// runs with each pass; history is a convenience, not a correctness input.
pub const OUTBOX_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

/// A queued or finished submission as a caller sees it. No body, no credential, no server host: the
/// addresses are reduced to a count and the subject is the user's own text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxView {
    pub id: String,
    pub account: String,
    pub state: postino_store::OutboxState,
    pub created_at: i64,
    pub send_at: i64,
    pub attempts: u32,
    pub last_error: Option<String>,
    /// Number of envelope recipients, including Bcc. The list itself never leaves the envelope.
    pub recipient_count: usize,
    pub subject: String,
    pub message_id: String,
    /// The local draft this row was built from. `None` when the row was imported or damaged.
    pub draft_id: Option<String>,
    /// The end of the undo window, for a row that is still cancellable.
    pub undo_until: Option<i64>,
}

/// One account whose queue is stopped by an authentication failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxHalt {
    pub account: String,
    pub error: String,
    pub at: i64,
}

/// The outbox list plus the stopped accounts and any damaged-row warnings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxList {
    pub entries: Vec<OutboxView>,
    #[serde(default)]
    pub halted: Vec<OutboxHalt>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A queued submission and the instant after which cancellation is no longer possible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendQueued {
    pub outbox: OutboxView,
    /// Present only while the row is still `queued` and cancellable.
    pub undo_until: Option<i64>,
}

/// The exact bytes and envelope a send would use, without writing a queue row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendDryRun {
    pub raw_base64: String,
    pub bytes: usize,
    pub envelope: SendEnvelope,
    pub subject: String,
    pub message_id: String,
}

/// The SMTP envelope: the sender and every recipient, Bcc included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendEnvelope {
    pub from: String,
    pub recipients: Vec<String>,
}

/// What one outbox processing pass did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutboxRunReport {
    pub processed: u32,
    pub sent: u32,
    pub append_pending: u32,
    pub retried: u32,
    pub failed: u32,
    pub uncertain: u32,
    /// Accounts whose queue was already stopped by an authentication failure.
    pub halted: Vec<String>,
    /// Fixed, redacted sentences for rows that could not proceed.
    pub errors: Vec<String>,
    /// The rows after the pass, for the UI to refresh without a second query.
    pub entries: Vec<OutboxView>,
}

impl OutboxRunReport {
    /// Whether the pass left work that is due now. The UI uses it to avoid a busy loop.
    pub fn has_due_work(&self) -> bool {
        self.retried > 0 || self.append_pending > 0 || self.sent > 0
    }
}

/// The JSON stored in `outbox.envelope_json`. Every field is the engine's own copy, in the encrypted
/// database; the recipients are needed to submit and the subject is needed to list the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboxEnvelope {
    pub from: String,
    pub recipients: Vec<String>,
    pub subject: String,
    pub message_id: String,
    /// The local draft this row was built from. `None` for a P9c rule forward: it is a standalone
    /// submission with no draft to move to Sent. A row written by an older build always has one.
    #[serde(default)]
    pub draft_id: Option<String>,
    /// The store revision of the draft when it was serialized. The submission only leaves the
    /// machine while the draft still has this revision; a stale connection cannot deliver old bytes
    /// and then label a newer draft as the delivered message. `None` for a row written by a build
    /// before this field existed or for a rule forward with no draft: those rows keep the old
    /// best-effort behaviour.
    #[serde(default)]
    pub draft_revision: Option<u32>,
}

impl OutboxEnvelope {
    pub(crate) fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub(crate) fn decode(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

/// Exponential backoff for the attempt that just failed. `attempts` is the value after the lease
/// increment: the first failure is attempt 1 and waits [`BACKOFF_BASE_MS`].
pub fn backoff_ms(attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(20);
    BACKOFF_BASE_MS.saturating_mul(1i64.checked_shl(shift).unwrap_or(i64::MAX)).min(BACKOFF_MAX_MS)
}

/// The view of one row. A damaged envelope is not fatal: the row is still visible, with a warning
/// that says why the subject and recipient count are missing.
pub(crate) fn view(entry: &OutboxEntry) -> (OutboxView, Option<String>) {
    let decoded = OutboxEnvelope::decode(&entry.envelope_json);
    let warning = decoded.is_none().then(|| format!("la riga {:?} ha un inviluppo illeggibile: è visibile ma non può essere inviata", entry.id));
    let (subject, message_id, draft_id, recipient_count) = match decoded {
        Some(envelope) => {
            let count = envelope.recipients.len();
            (envelope.subject, envelope.message_id, envelope.draft_id, count)
        }
        None => (String::new(), String::new(), None, 0),
    };
    let undo_until = (entry.state == postino_store::OutboxState::Queued).then_some(entry.send_at);
    (
        OutboxView {
            id: entry.id.clone(),
            account: entry.account_id.clone(),
            state: entry.state,
            created_at: entry.created_at,
            send_at: entry.send_at,
            attempts: entry.attempts,
            last_error: entry.last_error.clone(),
            recipient_count,
            subject,
            message_id,
            draft_id,
            undo_until,
        },
        warning,
    )
}

pub(crate) fn halt_key(account_id: &str) -> String {
    format!("{HALT_KEY_PREFIX}{account_id}")
}

pub(crate) fn read_halt(store: &postino_store::Store, account_id: &str) -> Result<Option<OutboxHalt>, postino_store::StoreError> {
    let Some(raw) = store.get_meta(&halt_key(account_id))? else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<OutboxHalt>(&raw).ok())
}

pub(crate) fn write_halt(store: &postino_store::Store, account_id: &str, error: &str, at: i64) -> Result<(), postino_store::StoreError> {
    let halt = OutboxHalt { account: account_id.to_owned(), error: error.to_owned(), at };
    let encoded = serde_json::to_string(&halt).unwrap_or_else(|_| "{}".to_owned());
    store.set_meta(&halt_key(account_id), &encoded)
}

pub(crate) fn clear_halt(store: &postino_store::Store, account_id: &str) -> Result<(), postino_store::StoreError> {
    store.set_meta(&halt_key(account_id), "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_to_one_hour_and_stops() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(3), 120_000);
        assert_eq!(backoff_ms(7), 1_920_000);
        assert_eq!(backoff_ms(8), BACKOFF_MAX_MS);
        assert_eq!(backoff_ms(99), BACKOFF_MAX_MS);
    }

    #[test]
    fn the_envelope_round_trips_without_a_body() {
        let envelope = OutboxEnvelope {
            from: "sender@example.com".to_owned(),
            recipients: vec!["a@example.com".to_owned(), "b@example.com".to_owned()],
            subject: "Prova".to_owned(),
            message_id: "<id@example.com>".to_owned(),
            draft_id: Some("draft_1".to_owned()),
            draft_revision: Some(3),
        };
        let raw = envelope.encode().expect("encode");
        assert_eq!(OutboxEnvelope::decode(&raw), Some(envelope));
        assert!(!raw.contains("body"));
    }

    #[test]
    fn an_envelope_from_an_older_build_has_no_revision() {
        let raw = r#"{"from":"a@example.com","recipients":["b@example.com"],"subject":"x","message_id":"<i@example.com>","draft_id":"draft_1"}"#;
        let envelope = OutboxEnvelope::decode(raw).expect("decode");
        assert_eq!(envelope.draft_revision, None);
    }

    #[test]
    fn the_lease_outlives_the_longest_supported_transfer() {
        // The client aborts a body/APPEND literal after MAX_BODY_TRANSFER; the lease must be strictly
        // longer so a live transfer can never be expired as a crash, with room for connect/TLS/auth.
        let budget = i64::try_from(postino_net::MAX_BODY_TRANSFER.as_millis()).expect("millis");
        assert!(LEASE_MS >= budget + 10 * 60 * 1_000, "LEASE_MS {LEASE_MS} is not above the body budget {budget}");
    }

    #[test]
    fn backoff_never_overflows() {
        assert!(backoff_ms(u32::MAX) <= BACKOFF_MAX_MS);
    }
}
