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
//! - `BODY.PEEK[]` only. A read never sets `\Seen` and [`sync_account`] never sends `STORE`,
//!   `EXPUNGE`, `COPY`, `MOVE` or `APPEND`: the server is read, never changed. The P7 submission
//!   helper [`append_sent`] is a separate call the engine makes after SMTP accepted a message, and it
//!   never submits.
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
mod importing;
mod ingest;
mod labels;
mod status;
mod submit;
mod warnings;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use postino_net::{Flag, ImapConfig, NetError, Session, UidFlags, UidSet};
use postino_store as store;
use thiserror::Error;

pub use folder::{PlannedFolder, plan_folders};
pub use importing::{Imported, import_message};
/// The MIME crate this bridge parses and serializes through. The engine reads PGP/MIME structure
/// through this re-export because the layer rule lets core depend on sync, not on the foundation
/// crate directly.
pub use postino_mime as mime;
pub use postino_mime::{
    CalendarInvite, InviteError, InviteWarning, Limits, MAX_CALENDAR_BYTES, MAX_CALENDAR_COMPONENTS, MAX_INVITE_ATTENDEES, MAX_INVITE_FIELD_CHARS,
    parse, parse_invite, reply_calendar,
};
pub use status::{STATUS_KEY_PREFIX, SyncPhase, SyncStatus, read_status, read_status_for, status_key};
pub use submit::{
    MimeError, SerializedSubmission, Submission, SubmissionAttachment, SubmissionEnvelope, SubmissionMailbox, append_sent, mime_for_filename,
    normalize_mime_type, sanitize_filename, serialize_submission, smtp_secret, store_raw, submit, verify_smtp,
};

/// The stable id this crate would give a message that carries `message_id` in `account_id`.
///
/// A locally submitted Sent copy uses it, so the server copy a later sync fetches has the same id and
/// updates that row with its UID instead of inserting a second one. `None` when the Message-ID is
/// unusable, in which case the local copy keeps its own id.
#[must_use]
pub fn local_message_id(account_id: &str, message_id: &str) -> Option<String> {
    ids::normalise_message_id(message_id).map(|normalised| ids::message_id(account_id, "", 1, 1, Some(&normalised)))
}

/// The normalised `Message-ID` (one pair of angle brackets removed) this crate stores, or `None`
/// when the value is unusable. A locally submitted message stores the same form a sync would.
#[must_use]
pub fn normalised_message_id(raw: &str) -> Option<String> {
    ids::normalise_message_id(raw)
}

/// UIDs requested per liveness/flags window. The network client caps untagged responses at 10 000;
/// 1 000 keeps every window small and a 50 000-message folder to fifty round trips.
const FLAG_SCAN_BATCH: u32 = 1_000;

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
    /// The server advertised Gmail's `X-GM-EXT-1`; labels are present and meaningful.
    pub(crate) gmail_labels: bool,
    /// A server sync treats the same Message-ID in another folder as a move. A local import must not
    /// move cached mail just because the file has the same Message-ID.
    pub(crate) allow_cross_folder_move: bool,
}

/// Mutable state shared by every folder of one run.
struct RunProgress {
    /// Messages this run may still ingest before `max_messages` stops it.
    remaining: usize,
    /// Every local id touched this run, in any folder. A missing UID whose id was seen elsewhere is
    /// a move, never a deletion.
    seen: HashSet<String>,
    /// Local ids whose server flags disagree with the local read/star state, deduplicated per run.
    drifted: HashSet<String>,
}

/// What one login-and-list probe learned about a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProbe {
    /// Selectable and non-selectable folders exactly as `LIST` returned them.
    pub folders: Vec<postino_net::RemoteFolder>,
    /// Capabilities the server advertised after authentication, upper-cased.
    pub capabilities: Vec<String>,
}

/// Log in once, list the folders and log out, without touching the store.
///
/// This is the onboarding and `account_test` path: it proves the host, TLS mode and credential before
/// anything is written. It blocks the calling thread and runs its own current-thread runtime, exactly
/// like [`sync_account`]. No message is selected or fetched, so the probe cannot change mail state.
///
/// # Errors
///
/// [`SyncError::Auth`] when the server rejects the credential, [`SyncError::Connect`] for DNS, TCP,
/// TLS and timeout failures, and [`SyncError::Protocol`] when no session runtime can start.
pub fn probe(config: &ImapConfig) -> Result<ServerProbe, SyncError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| SyncError::Protocol(format!("could not start the probe runtime: {error}")))?;
    runtime.block_on(async {
        let mut session = Session::connect(config.clone()).await.map_err(SyncError::from)?;
        session.login().await.map_err(SyncError::from)?;
        let capabilities = session.capabilities().iter().map(str::to_owned).collect();
        let folders = session.list().await.map_err(SyncError::from)?;
        let _ = session.logout().await;
        Ok(ServerProbe { folders, capabilities })
    })
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

    let gmail_labels = session.capabilities().x_gm_ext();
    let context = Context { store, account, limits, cancel, gmail_labels, allow_cross_folder_move: true };
    let mut progress = RunProgress { remaining: limits.max_messages, seen: HashSet::new(), drifted: HashSet::new() };
    let mut exhausted = false;
    let mut done = 0u32;
    let mut all_scanned = true;
    let mut deferred: Vec<(String, Vec<String>)> = Vec::new();
    for target in &targets {
        if cancel.load(Ordering::Relaxed) {
            return Err(SyncError::Cancelled);
        }
        if progress.remaining == 0 {
            exhausted = true;
            break;
        }
        let stats = sync_folder(&context, &mut session, target, &mut warnings, &mut progress, state).await?;
        outcome.fetched = outcome.fetched.saturating_add(stats.fetched);
        outcome.attachments_stored = outcome.attachments_stored.saturating_add(stats.attachments_stored);
        let missing_count = stats.missing.len();
        if stats.scan_complete {
            if !stats.missing.is_empty() {
                deferred.push((target.row.id.clone(), stats.missing));
            }
        } else {
            all_scanned = false;
        }
        done = done.saturating_add(1);
        state.folders_done = done;
        state.messages = outcome.fetched;
        status::write_status(store, account, state)?;
        log::debug!("sync: folder {}: {} fetched, {} missing", target.row.id, stats.fetched, missing_count);
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

    // Deletion is a whole-account decision. Only after every folder has been scanned can a UID that
    // disappeared from one folder be told apart from a message that moved to another; a partial run
    // deletes nothing.
    let complete = !exhausted && usize::try_from(done).unwrap_or(usize::MAX) == targets.len() && all_scanned;
    if complete {
        for (folder_id, missing) in deferred {
            let ids = missing.into_iter().filter(|id| !progress.seen.contains(id)).collect::<Vec<_>>();
            if ids.is_empty() {
                continue;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(SyncError::Cancelled);
            }
            outcome.removed = outcome.removed.saturating_add(store.delete_synced_messages(&folder_id, &ids)?);
        }
    } else if !deferred.is_empty() {
        warnings.push("the deletion pass was skipped because the run stopped before every folder was scanned".to_owned());
    }

    outcome.updated_flags = u32::try_from(progress.drifted.len()).unwrap_or(u32::MAX);
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
    attachments_stored: u32,
    exhausted: bool,
    /// Whether the full UID/flags scan finished. Only such a folder may propose deletions.
    scan_complete: bool,
    /// Cached ids whose UID is gone from the server. Applied at the end of a complete run.
    missing: Vec<String>,
}

async fn sync_folder(
    context: &Context<'_>,
    session: &mut Session,
    target: &folder::TargetFolder,
    warnings: &mut warnings::Warnings,
    progress: &mut RunProgress,
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
    let hint = selected.uid_next;

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
    row.uid_next = hint;
    store.upsert_folder(&row)?;

    // One query for the folder's cached identities and local flags: duplicate-Message-ID
    // disambiguation, drift comparison and deletion candidates all read this snapshot.
    let snapshot = store.folder_message_flags(&row.id)?;
    let mut index = ingest::FolderIndex::new(&snapshot);

    // UIDNEXT is a hint, never truth: a stale or low value hides mail above it, and EXISTS/RECENT
    // are counts, not liveness. One `<hint>:*` probe returns the last live UID. UIDs below the hint
    // are the RFC 3501 §6.4.8 `*` artifact; they do not extend the new-mail range.
    let probe_hint = cursor.max(hint.unwrap_or(cursor));
    if context.cancel.load(Ordering::Relaxed) {
        return Err(SyncError::Cancelled);
    }
    let probe_set = UidSet::parse(&format!("{probe_hint}:*")).map_err(SyncError::from)?;
    let probe = session.fetch_uid_flags(probe_set).await.map_err(SyncError::from)?;
    let probe_max = probe.iter().map(|entry| entry.uid).max();
    let true_max = match probe_max {
        Some(max) => max,
        None if probe_hint > cursor => probe_hint - 1,
        None => cursor.saturating_sub(1),
    };

    // New mail: explicit windows below the true maximum. `UID FETCH n:*` would include the last
    // message even when it is below `n`, so every window has an explicit end.
    'batches: while cursor <= true_max {
        if context.cancel.load(Ordering::Relaxed) {
            return Err(SyncError::Cancelled);
        }
        if progress.remaining == 0 {
            stats.exhausted = true;
            break;
        }
        let end = cursor.saturating_add(batch.saturating_sub(1)).min(true_max);
        let set = UidSet::range(cursor, end).map_err(SyncError::from)?;
        let mut envelopes = session.fetch_envelopes(set).await.map_err(SyncError::from)?;
        envelopes.sort_by_key(|envelope| envelope.uid);
        if envelopes.is_empty() {
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
            if progress.remaining == 0 {
                stats.exhausted = true;
                break 'batches;
            }
            progress.remaining = progress.remaining.saturating_sub(1);
            let uid = envelope.uid;
            if usize::try_from(envelope.size).unwrap_or(usize::MAX) > context.limits.max_message_bytes {
                warnings.push(format!(
                    "message UID {uid} is {} bytes, over the {}-byte message limit; it was skipped",
                    envelope.size, context.limits.max_message_bytes
                ));
            } else {
                match session.fetch_body_opt(uid).await.map_err(SyncError::from)? {
                    Some(raw) => {
                        if let Some(ingested) = ingest::ingest(context, &row, &mut index, uid_validity, &envelope, &raw, warnings)? {
                            stats.fetched = stats.fetched.saturating_add(1);
                            stats.attachments_stored = stats.attachments_stored.saturating_add(ingested.attachments_stored);
                            if ingested.drift {
                                progress.drifted.insert(ingested.message_id.clone());
                            }
                            progress.seen.insert(ingested.message_id.clone());
                            state.messages = state.messages.saturating_add(1);
                            log::debug!("sync: message {} stored (new: {})", ingested.message_id, ingested.new);
                        }
                    }
                    None => warnings.push(format!("message UID {uid} vanished before its body could be fetched; it was skipped")),
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

    // Counts are a cache refreshed once per folder, not once per message. A run that hit the
    // message limit has no complete UID list, so it proposes no deletion.
    if stats.exhausted {
        let counts = store.recount_folder(&row.id)?;
        let mut updated = store.get_folder(&row.id)?.unwrap_or_else(|| row.clone());
        updated.unread = counts.unread;
        updated.total = counts.total;
        updated.uid_validity = Some(uid_validity);
        updated.uid_next = hint;
        store.upsert_folder(&updated)?;
        store.set_sync_state(&store::SyncState {
            folder_id: row.id.clone(),
            uid_next: cursor.max(1),
            highest_modseq: selected.highest_modseq,
            last_error: None,
        })?;
        return Ok(stats);
    }
    cursor = cursor.max(true_max.saturating_add(1));

    // Full UID/flags scan: the only source of liveness. A folder whose scan did not finish proposes
    // no deletion, and the whole run aborts before any deletion anyway.
    let live = scan_flags(session, context.cancel, true_max).await?;
    let mut missing = Vec::new();
    for cached in &snapshot {
        let Some(uid) = cached.uid else {
            continue;
        };
        match live.get(&uid) {
            Some(entry) => {
                let unread = !entry.flags.contains(&Flag::Seen);
                let starred = entry.flags.contains(&Flag::Flagged);
                if cached.unread != unread || cached.starred != starred {
                    progress.drifted.insert(cached.message_id.clone());
                }
                // The liveness scan already carries X-GM-LABELS on Gmail, so a label dropped from
                // an existing message is reconciled without re-fetching its body.
                if context.gmail_labels {
                    labels::apply(store, &context.account.id, &cached.message_id, &entry.labels)?;
                }
            }
            None => missing.push(cached.message_id.clone()),
        }
    }
    stats.missing = missing;
    stats.scan_complete = true;

    let counts = store.recount_folder(&row.id)?;
    let mut updated = store.get_folder(&row.id)?.unwrap_or_else(|| row.clone());
    updated.unread = counts.unread;
    updated.total = counts.total;
    updated.uid_validity = Some(uid_validity);
    updated.uid_next = hint;
    updated.last_sync_at = Some(now_ms());
    store.upsert_folder(&updated)?;
    store.set_sync_state(&store::SyncState {
        folder_id: row.id.clone(),
        uid_next: cursor.max(1),
        highest_modseq: selected.highest_modseq,
        last_error: None,
    })?;
    Ok(stats)
}

/// Fetch every server UID and its flags in bounded windows, aborting between windows on cancel.
async fn scan_flags(session: &mut Session, cancel: &AtomicBool, max_uid: u32) -> Result<HashMap<u32, UidFlags>, SyncError> {
    let mut live: HashMap<u32, UidFlags> = HashMap::new();
    if max_uid == 0 {
        return Ok(live);
    }
    let mut start = 1u32;
    while start <= max_uid {
        if cancel.load(Ordering::Relaxed) {
            return Err(SyncError::Cancelled);
        }
        let end = start.saturating_add(FLAG_SCAN_BATCH.saturating_sub(1)).min(max_uid);
        let set = UidSet::range(start, end).map_err(SyncError::from)?;
        for entry in session.fetch_uid_flags(set).await.map_err(SyncError::from)? {
            live.insert(entry.uid, entry);
        }
        if end == u32::MAX {
            break;
        }
        start = end.saturating_add(1);
    }
    Ok(live)
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
