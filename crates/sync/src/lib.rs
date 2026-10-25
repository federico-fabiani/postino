//! Reconcile one IMAP account into the encrypted local store.
//!
//! This crate is the bridge between [`postino_net`] (one bounded IMAP session) and
//! [`postino_store`] (the encrypted cache). It is deliberately small and headless:
//!
//! ```text
//! LIST ──▶ folders + special-use      ──▶ folder rows
//! SELECT ──▶ UIDVALIDITY / UIDNEXT    ──▶ reset on change, else resume the cursor
//! UID FETCH last+1.. ──▶ envelopes    ──▶ flags, dates, sizes
//! UID FETCH <uid> (BODY.PEEK[])       ──▶ postino-mime ──▶ message + attachments
//! UID FETCH all ──▶ full UID list     ──▶ delete what is gone, observe flag drift
//! ```
//!
//! # Guarantees
//!
//! - One store connection, owned by the caller, used on the caller thread. The async work runs on a
//!   private current-thread Tokio runtime inside [`sync_account`]; no global runtime, no task left
//!   behind.
//! - One transaction per message: the row, its attachment rows and `has_attachments` commit
//!   together, and the per-folder cursor advances only after that commit. A crash or a dropped
//!   connection resumes from the last committed UID without a gap; re-fetching one message is
//!   harmless because upserts are idempotent.
//! - `BODY.PEEK[]` only. A read never sets `\Seen` and sync never sends `STORE`, `EXPUNGE`, `COPY`,
//!   `MOVE` or `APPEND`: the server is read, never changed.
//! - Bodies are parsed by `postino-mime` with the sync limits; HTML is sanitized there, remote
//!   content is removed and a hostile part becomes a warning, not a failed run.
//! - Attachment bytes are written through the encrypted blob seam only when the part is complete and
//!   within `max_attachment_bytes`; otherwise the metadata is kept and a warning says why.
//! - Flags: server flags are applied on first insert only. Later syncs keep local `unread`/`starred`
//!   (M2 will push them) and report drift once per run. This is a deliberate M1 limitation.
//! - Cancellation is cooperative: [`sync_account`] takes an [`AtomicBool`] and checks it before each
//!   message, between batches and between folders. A cancelled run returns
//!   [`SyncError::Cancelled`] and keeps every message written so far.
//!
//! # What is not synced
//!
//! Server-side filters/rules, flag changes pushed to the server, expunge/delete on the server,
//! `CONDSTORE`/`QRESYNC` incrementality, `APPEND` of drafts and sent mail, full JWZ threading, and
//! folder deletion/renames (a vanished folder's local rows stay until P6 decides). The indexing pass
//! (`FTS5`) and the scheduled loop belong to later packages.
//!
//! # Message identity
//!
//! `msg_` plus 16 hex of `BLAKE3(account, Message-ID)` when the message has one, otherwise
//! `msg_{folder}_{uidvalidity}_{uid}`. The same Message-ID in two folders is one local message: the
//! later folder wins the `folder_id` and labels are preserved. The trade-off is documented in
//! `docs/architecture.md` §4.6.

#![deny(missing_docs)]

mod folder;
mod ids;
mod ingest;
mod status;
mod warnings;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use postino_net::{Flag, ImapConfig, NetError, Session, UidSet};
use postino_store as store;
use thiserror::Error;

pub use status::{STATUS_KEY_PREFIX, SyncPhase, SyncStatus, read_status, status_key};

/// One run's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncLimits {
    /// Envelopes requested per `UID FETCH`, default 100.
    pub batch: usize,
    /// Largest message, in bytes, that is downloaded and parsed, default 25 MiB.
    pub max_message_bytes: usize,
    /// Largest attachment part whose bytes are stored, default 25 MiB.
    pub max_attachment_bytes: u64,
    /// Safety valve: the most messages one run may ingest, default 50 000.
    pub max_messages: usize,
}

impl Default for SyncLimits {
    fn default() -> Self {
        Self { batch: 100, max_message_bytes: 25 * 1024 * 1024, max_attachment_bytes: 25 * 1024 * 1024, max_messages: 50_000 }
    }
}

impl SyncLimits {
    /// Hard ceiling for one attachment, matching the blob store's format limit.
    pub const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;

    /// Keep every field usable: a zero batch would never advance, and an attachment cap above the
    /// blob format would turn "skip with a warning" into a fatal store error.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            batch: self.batch.clamp(1, 10_000),
            max_message_bytes: self.max_message_bytes.clamp(1, postino_mime::Limits::HARD_MAX_BYTES),
            max_attachment_bytes: self.max_attachment_bytes.min(Self::MAX_ATTACHMENT_BYTES),
            max_messages: self.max_messages.clamp(1, 1_000_000),
        }
    }
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Account id the run belonged to.
    pub account: String,
    /// Concrete selectable folders discovered and reconciled. Aggregate views (`\All`, `\Flagged`)
    /// get a local row but are not message folders and are not counted here.
    pub folders: u32,
    /// Messages ingested: new and refreshed.
    pub fetched: u32,
    /// Messages re-seen whose server flags differ from the local read/star state. Local flags are
    /// kept, so this counts drift observations, not writes.
    pub updated_flags: u32,
    /// Cached messages removed because their UID is gone from the server. A UIDVALIDITY reset is
    /// reported in the warnings, not here.
    pub removed: u32,
    /// Attachments whose bytes are in the blob store. A deduplicated put, which writes no new file,
    /// still counts: the bytes are available.
    pub attachments_stored: u32,
    /// Bounded, human-readable warnings: missing roles, UIDVALIDITY resets, skipped parts, drift.
    pub warnings: Vec<String>,
}

/// Everything that stops a sync, typed so a caller can act on it.
#[derive(Debug, Error)]
pub enum SyncError {
    /// A network, TLS, timeout or disconnect failure. The store is unchanged or consistent per
    /// message; the next run resumes from the last committed UID.
    #[error("sync stopped: {0}")]
    Connect(#[source] NetError),
    /// The server rejected the credentials or refused to accept them.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The local store refused a write or a read.
    #[error("sync could not use the local store: {0}")]
    Store(#[from] store::StoreError),
    /// The caller set the cancellation flag. Messages written before the stop are intact.
    #[error("sync cancelled; messages written before the stop are kept")]
    Cancelled,
    /// The account handed to [`sync_account`] is not in the store.
    #[error("account {0:?} is not in the local store")]
    NoAccount(String),
    /// The server answered something this reconciler refuses to interpret.
    #[error("sync cannot proceed: {0}")]
    Protocol(String),
}

impl From<NetError> for SyncError {
    fn from(error: NetError) -> Self {
        match error {
            NetError::Auth { reason } => Self::Auth(reason),
            other => Self::Connect(other),
        }
    }
}

impl SyncError {
    /// The fixed sentence stored in `sync_status:<account>`. It never contains a host name, an
    /// address or message content; the typed error carries the detail to the caller.
    fn status_message(&self) -> &'static str {
        match self {
            Self::Connect(_) => "could not talk to the server",
            Self::Auth(_) => "the server rejected the credentials",
            Self::Store(_) => "the local store refused a change",
            Self::Cancelled => "the sync was cancelled",
            Self::NoAccount(_) => "the account is not in the local store",
            Self::Protocol(_) => "the server sent a response this client could not use",
        }
    }
}

/// Run arguments shared by the folder loop.
pub(crate) struct Context<'a> {
    pub(crate) store: &'a store::Store,
    pub(crate) account: &'a store::Account,
    pub(crate) limits: &'a SyncLimits,
    pub(crate) cancel: &'a AtomicBool,
}

/// Reconcile one account from IMAP into `store`.
///
/// The function blocks the calling thread, owns no store connection of its own, and starts and
/// stops its own current-thread runtime. It returns after `LOGOUT`, or with a typed error that says
/// what failed; every message committed before that point is durable and the next call resumes.
///
/// # Errors
///
/// [`SyncError`] covers connection/TLS/timeouts, authentication, store failures, cancellation, an
/// account missing from the store, and a response the reconciler will not interpret.
pub fn sync_account(
    store: &store::Store,
    account: &store::Account,
    config: &ImapConfig,
    limits: &SyncLimits,
    cancel: &AtomicBool,
) -> Result<SyncOutcome, SyncError> {
    if store.get_account(&account.id)?.is_none() {
        return Err(SyncError::NoAccount(account.id.clone()));
    }
    let mut limits = limits.clamped();
    // Never choose to download a body the network layer would refuse: the two bounds are separate
    // knobs, so the effective message cap is the smaller one.
    limits.max_message_bytes = limits.max_message_bytes.min(config.limits.clamped().max_literal);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| SyncError::Protocol(format!("could not start the sync runtime: {error}")))?;
    runtime.block_on(run(store, account, config, &limits, cancel))
}

async fn run(
    store: &store::Store,
    account: &store::Account,
    config: &ImapConfig,
    limits: &SyncLimits,
    cancel: &AtomicBool,
) -> Result<SyncOutcome, SyncError> {
    // A damaged status is a cache, not a reason to refuse to sync: start from zero.
    let previous = read_status(store, account).ok().flatten();
    let mut state = SyncStatus {
        state: SyncPhase::Syncing,
        last_attempt: now_ms(),
        last_success: previous.and_then(|previous| previous.last_success),
        error: None,
        folders_done: 0,
        folders_total: 0,
        messages: 0,
    };
    status::write_status(store, account, &state)?;

    let result = run_session(store, account, config, limits, cancel, &mut state).await;
    match result {
        Ok(outcome) => {
            state.state = SyncPhase::Idle;
            state.last_success = Some(now_ms());
            state.error = None;
            state.messages = outcome.fetched;
            status::write_status(store, account, &state)?;
            Ok(outcome)
        }
        Err(error) => {
            state.state = SyncPhase::Error;
            state.error = Some(error.status_message().to_owned());
            if let Err(store_error) = status::write_status(store, account, &state) {
                log::warn!("sync: could not record the error status: {store_error}");
            }
            Err(error)
        }
    }
}

async fn run_session(
    store: &store::Store,
    account: &store::Account,
    config: &ImapConfig,
    limits: &SyncLimits,
    cancel: &AtomicBool,
    state: &mut SyncStatus,
) -> Result<SyncOutcome, SyncError> {
    let mut outcome = SyncOutcome {
        account: account.id.clone(),
        folders: 0,
        fetched: 0,
        updated_flags: 0,
        removed: 0,
        attachments_stored: 0,
        warnings: Vec::new(),
    };
    let mut warnings = warnings::Warnings::default();
    if cancel.load(Ordering::Relaxed) {
        return Err(SyncError::Cancelled);
    }

    let mut session = Session::connect(config.clone()).await.map_err(SyncError::from)?;
    session.login().await.map_err(SyncError::from)?;
    let remote = session.list().await.map_err(SyncError::from)?;
    let targets = folder::discover(store, account, &remote, &mut warnings)?;
    if targets.is_empty() {
        return Err(SyncError::Protocol("the server listed no selectable folder".to_owned()));
    }
    outcome.folders = u32::try_from(targets.len()).unwrap_or(u32::MAX);
    state.folders_total = outcome.folders;
    status::write_status(store, account, state)?;
    log::info!("sync: account {}: {} folders to reconcile", account.id, outcome.folders);

    let context = Context { store, account, limits, cancel };
    let mut remaining = limits.max_messages;
    let mut exhausted = false;
    let mut done = 0u32;
    for target in &targets {
        if cancel.load(Ordering::Relaxed) {
            return Err(SyncError::Cancelled);
        }
        if remaining == 0 {
            exhausted = true;
            break;
        }
        let stats = sync_folder(&context, &mut session, target, &mut warnings, &mut remaining, state).await?;
        outcome.fetched = outcome.fetched.saturating_add(stats.fetched);
        outcome.updated_flags = outcome.updated_flags.saturating_add(stats.updated_flags);
        outcome.removed = outcome.removed.saturating_add(stats.removed);
        outcome.attachments_stored = outcome.attachments_stored.saturating_add(stats.attachments_stored);
        done = done.saturating_add(1);
        state.folders_done = done;
        state.messages = outcome.fetched;
        status::write_status(store, account, state)?;
        log::debug!("sync: folder {}: {} fetched, {} removed, {} drift", target.row.id, stats.fetched, stats.removed, stats.updated_flags);
        if stats.exhausted {
            exhausted = true;
            break;
        }
    }
    if exhausted {
        warnings.push(format!(
            "stopped after processing {} messages (the per-run limit); the rest of the mailbox is fetched on the next run",
            limits.max_messages
        ));
    }
    if outcome.updated_flags > 0 {
        warnings.push(format!(
            "{} messages differ from the server's read/starred flags; local flags were kept (M1 flags are local-only)",
            outcome.updated_flags
        ));
    }

    let _ = session.logout().await;
    recount_account(store, account);
    outcome.warnings = warnings.into_vec();
    log::info!(
        "sync: account {}: {} folders done, {} fetched, {} removed, {} attachments",
        account.id,
        done,
        outcome.fetched,
        outcome.removed,
        outcome.attachments_stored
    );
    Ok(outcome)
}

#[derive(Default)]
struct FolderStats {
    fetched: u32,
    updated_flags: u32,
    removed: u32,
    attachments_stored: u32,
    exhausted: bool,
}

async fn sync_folder(
    context: &Context<'_>,
    session: &mut Session,
    target: &folder::TargetFolder,
    warnings: &mut warnings::Warnings,
    remaining: &mut usize,
    state: &mut SyncStatus,
) -> Result<FolderStats, SyncError> {
    let store = context.store;
    let batch = u32::try_from(context.limits.batch).unwrap_or(u32::MAX).max(1);
    let mut stats = FolderStats::default();

    let selected = match session.select(&target.wire_name).await {
        Ok(status) => status,
        Err(NetError::ServerNo { .. } | NetError::ServerBad { .. }) => {
            warnings.push(format!("folder {:?} could not be selected and was skipped", target.row.name));
            return Ok(stats);
        }
        Err(error) => return Err(SyncError::from(error)),
    };
    let uid_validity = selected.uid_validity;
    let horizon = selected.uid_next.map(|next| next.saturating_sub(1)).filter(|highest| *highest > 0);

    // UIDVALIDITY is the folder's identity: a change means every cached UID is about some other
    // mailbox state, so the messages are dropped and the folder starts from UID 1.
    let mut cursor = 1u32;
    if target.row.uid_validity != Some(uid_validity) {
        let removed = store.reset_folder_messages(&target.row.id)?;
        if let Some(previous) = target.row.uid_validity {
            warnings.push(format!(
                "folder {:?}: UIDVALIDITY changed from {previous} to {uid_validity}; {removed} cached messages were removed and the folder is fetched again",
                target.row.name
            ));
        } else if removed > 0 {
            warnings.push(format!(
                "folder {:?}: the stored UIDVALIDITY was missing; {removed} cached messages were discarded and the folder is fetched again",
                target.row.name
            ));
        }
    } else if let Some(state) = store.get_sync_state(&target.row.id)? {
        cursor = state.uid_next.max(1);
    }

    // Read the row again after a possible reset: the counts in the discovery snapshot are stale.
    let mut row = store.get_folder(&target.row.id)?.unwrap_or_else(|| target.row.clone());
    row.uid_validity = Some(uid_validity);
    row.uid_next = selected.uid_next;
    store.upsert_folder(&row)?;

    // New mail: bounded UID windows. `UID FETCH n:*` would include the last message even when it is
    // below `n` (RFC 3501 §6.4.8), so the end of each window is explicit. A mailbox the server just
    // reported as empty is skipped: a large `UIDNEXT` after a mass deletion must not turn into a
    // loop over a billion empty windows.
    let empty_mailbox = selected.exists == 0;
    let mut drifted = std::collections::HashSet::new();
    'batches: while !empty_mailbox && horizon.is_none_or(|highest| cursor <= highest) {
        if context.cancel.load(Ordering::Relaxed) {
            return Err(SyncError::Cancelled);
        }
        if *remaining == 0 {
            stats.exhausted = true;
            break;
        }
        let end = match horizon {
            Some(highest) => cursor.saturating_add(batch.saturating_sub(1)).min(highest),
            None => cursor.saturating_add(batch.saturating_sub(1)),
        };
        let set = UidSet::range(cursor, end).map_err(SyncError::from)?;
        let mut envelopes = session.fetch_envelopes(set).await.map_err(SyncError::from)?;
        envelopes.sort_by_key(|envelope| envelope.uid);
        if envelopes.is_empty() {
            if horizon.is_none() {
                break;
            }
            cursor = end.saturating_add(1);
            if cursor == 0 {
                break;
            }
            continue;
        }
        for envelope in envelopes {
            if context.cancel.load(Ordering::Relaxed) {
                return Err(SyncError::Cancelled);
            }
            if *remaining == 0 {
                stats.exhausted = true;
                break 'batches;
            }
            *remaining = (*remaining).saturating_sub(1);
            let uid = envelope.uid;
            if usize::try_from(envelope.size).unwrap_or(usize::MAX) > context.limits.max_message_bytes {
                warnings.push(format!(
                    "message UID {uid} is {} bytes, over the {}-byte message limit; it was skipped",
                    envelope.size, context.limits.max_message_bytes
                ));
            } else {
                let raw = session.fetch_body(uid).await.map_err(SyncError::from)?;
                if let Some(ingested) = ingest::ingest(context, &row, uid_validity, &envelope, &raw, warnings)? {
                    stats.fetched = stats.fetched.saturating_add(1);
                    stats.attachments_stored = stats.attachments_stored.saturating_add(ingested.attachments_stored);
                    if ingested.drift {
                        drifted.insert(ingested.message_id.clone());
                    }
                    state.messages = state.messages.saturating_add(1);
                    log::debug!("sync: message {} stored (new: {})", ingested.message_id, ingested.new);
                }
            }
            cursor = cursor.max(uid.saturating_add(1));
            // The cursor advances only after the message transaction committed. A crash before this
            // write re-fetches the message; the upsert is idempotent.
            store.set_sync_state(&store::SyncState {
                folder_id: row.id.clone(),
                uid_next: cursor,
                highest_modseq: selected.highest_modseq,
                last_error: None,
            })?;
        }
        if cursor == 0 {
            break;
        }
    }

    // Deletions are computed from a complete UID list. A run that hit the message limit does not
    // have one, so it never deletes: a truncated pass must not be able to empty the cache. The
    // network client has no flags-only FETCH, so the list uses the envelope items it exposes and the
    // extra fields are discarded.
    if !stats.exhausted {
        let server = if empty_mailbox { HashMap::new() } else { fetch_server_flags(session, horizon, batch).await? };
        let cached = store.list_uids(&row.id)?;
        let mut missing = Vec::new();
        for entry in &cached {
            match server.get(&entry.uid) {
                Some(flags) => {
                    if let Some(local) = store.message_flags(&entry.message_id)? {
                        let unread = !flags.contains(&Flag::Seen);
                        let starred = flags.contains(&Flag::Flagged);
                        if local.unread != unread || local.starred != starred {
                            drifted.insert(entry.message_id.clone());
                        }
                    }
                }
                None => missing.push(entry.message_id.clone()),
            }
        }
        stats.removed = store.delete_synced_messages(&row.id, &missing)?;
    }
    stats.updated_flags = u32::try_from(drifted.len()).unwrap_or(u32::MAX);

    // Counts are a cache refreshed once per folder, not once per message.
    let counts = store.recount_folder(&row.id)?;
    let mut updated = store.get_folder(&row.id)?.unwrap_or_else(|| row.clone());
    updated.unread = counts.unread;
    updated.total = counts.total;
    updated.uid_validity = Some(uid_validity);
    updated.uid_next = selected.uid_next;
    if !stats.exhausted {
        updated.last_sync_at = Some(now_ms());
    }
    store.upsert_folder(&updated)?;
    store.set_sync_state(&store::SyncState {
        folder_id: row.id.clone(),
        uid_next: cursor.max(1),
        highest_modseq: selected.highest_modseq,
        last_error: None,
    })?;
    Ok(stats)
}

/// Fetch every server UID and its flags, in bounded windows.
async fn fetch_server_flags(session: &mut Session, horizon: Option<u32>, batch: u32) -> Result<HashMap<u32, Vec<Flag>>, SyncError> {
    let mut server: HashMap<u32, Vec<Flag>> = HashMap::new();
    let mut start = 1u32;
    loop {
        if let Some(highest) = horizon
            && start > highest
        {
            break;
        }
        let end = match horizon {
            Some(highest) => start.saturating_add(batch.saturating_sub(1)).min(highest),
            None => start.saturating_add(batch.saturating_sub(1)),
        };
        let set = UidSet::range(start, end).map_err(SyncError::from)?;
        let envelopes = session.fetch_envelopes(set).await.map_err(SyncError::from)?;
        if envelopes.is_empty() {
            if horizon.is_none() {
                break;
            }
            start = end.saturating_add(1);
            if start == 0 {
                break;
            }
            continue;
        }
        for envelope in envelopes {
            server.insert(envelope.uid, envelope.flags);
        }
        start = end.saturating_add(1);
        if start == 0 {
            break;
        }
    }
    Ok(server)
}

/// Refresh every folder count of one account, best effort. A sync that stops on a network error can
/// still repair the cache counters it changed, without masking the original error.
fn recount_account(store: &store::Store, account: &store::Account) {
    let folders = match store.list_folders(Some(&account.id)) {
        Ok(folders) => folders,
        Err(error) => {
            log::warn!("sync: could not list folders to recount them: {error}");
            return;
        }
    };
    for folder in folders {
        if let Err(error) = store.recount_folder(&folder.id) {
            log::warn!("sync: could not recount folder {}: {error}", folder.id);
        }
    }
}

/// Unix milliseconds, the store's only date unit.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_have_the_documented_defaults() {
        let limits = SyncLimits::default();
        assert_eq!(limits.batch, 100);
        assert_eq!(limits.max_message_bytes, 25 * 1024 * 1024);
        assert_eq!(limits.max_attachment_bytes, 25 * 1024 * 1024);
        assert_eq!(limits.max_messages, 50_000);
    }

    #[test]
    fn limits_are_clamped_to_usable_values() {
        let clamped = SyncLimits { batch: 0, max_message_bytes: 0, max_attachment_bytes: u64::MAX, max_messages: 0 }.clamped();
        assert_eq!(clamped.batch, 1);
        assert_eq!(clamped.max_message_bytes, 1);
        assert_eq!(clamped.max_attachment_bytes, SyncLimits::MAX_ATTACHMENT_BYTES);
        assert_eq!(clamped.max_messages, 1);

        let huge = SyncLimits { batch: usize::MAX, max_message_bytes: usize::MAX, max_attachment_bytes: 0, max_messages: usize::MAX }.clamped();
        assert_eq!(huge.batch, 10_000);
        assert_eq!(huge.max_message_bytes, postino_mime::Limits::HARD_MAX_BYTES);
        assert_eq!(huge.max_attachment_bytes, 0);
        assert_eq!(huge.max_messages, 1_000_000);
    }

    #[test]
    fn net_auth_failures_become_auth_errors() {
        let error = SyncError::from(NetError::Auth { reason: "nope".to_owned() });
        assert!(matches!(error, SyncError::Auth(_)), "{error:?}");
        let error = SyncError::from(NetError::Closed { phase: "select" });
        assert!(matches!(error, SyncError::Connect(NetError::Closed { .. })), "{error:?}");
    }

    #[test]
    fn status_sentences_never_contain_a_host_or_an_address() {
        let errors = [
            SyncError::Connect(NetError::Connect { host: "imap.example.com".to_owned(), port: 993, reason: "no".to_owned() }),
            SyncError::Auth("user@example.com said no".to_owned()),
            SyncError::Protocol("bad".to_owned()),
            SyncError::Cancelled,
            SyncError::NoAccount("acc_1".to_owned()),
        ];
        for error in errors {
            let sentence = error.status_message();
            assert!(!sentence.contains("example"), "{sentence}");
            assert!(!sentence.contains('@'), "{sentence}");
        }
    }

    #[test]
    fn now_is_after_the_epoch() {
        assert!(now_ms() > 1_600_000_000_000);
    }
}
