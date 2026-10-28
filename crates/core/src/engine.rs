//! The engine owns the mailbox data: in memory for the demo, in the encrypted store when one is
//! attached. The operation surface is identical for both.
//!
//! With a store the engine is **load once, write through**: [`Engine::open`] hydrates the mailbox
//! with one query per table, every mutating operation writes the repository first and only then
//! updates memory, and folder counts are recomputed locally instead of re-read. A failed write
//! leaves memory untouched: caller mistakes are [`OpError::InvalidArgs`] and storage failures are
//! [`OpError::Failed`], and each engine mutation is one store transaction.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::Utc;
use postino_net::{CertificateDer, NetError, SmtpEnvelope, SmtpError, SmtpLimits};
use postino_store::{FlagUpdate, OutboxEntry, OutboxState, PgpKeyInsert, Store};
use postino_sync::SyncError;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::model::{
    Account, AccountView, Address, Attachment, CalendarInviteView, DraftContent, Folder, FolderKind, InviteResponse, Label, LabelColor, LabelSummary,
    MailTls, Message, MessageSummary, PgpKey, PgpPublicKey, Provider, SavedSearch, SmtpSettings, ThreadSummary, snippet,
};
use crate::ops::{self, OpDef};
use crate::outbox::{
    APPEND_LEASE_MS, BACKOFF_MAX_MS, LEASE_MS, MAX_ATTEMPTS, MAX_SUBMISSION_BYTES, OUTBOX_RETENTION_MS, OutboxEnvelope, OutboxList, OutboxRunReport,
    OutboxView, SendDryRun, SendEnvelope, SendQueued, UNDO_MS, backoff_ms, clear_halt, read_halt, view, write_halt,
};
use crate::store_map;
use crate::vault::CredentialVault;

/// A caller-supplied id that does not exist. The engine's pre-checks and the store's `NotFound`
/// both map to this sentence, so a mistake is `InvalidArgs` whichever layer notices it first.
pub(crate) fn unknown_account(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown account {id:?}; use an id returned by accounts_list"))
}

pub(crate) fn unknown_folder(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown folder {id:?}; use an id returned by folders_list"))
}

pub(crate) fn unknown_message(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown message {id:?}; use an id returned by messages_list"))
}

fn not_stored_verify() -> crate::pgp_mime::PgpVerifyResult {
    crate::pgp_mime::PgpVerifyResult {
        state: crate::pgp_mime::PgpVerifyState::NotStored,
        kind: crate::pgp_mime::PgpMimeKind::None,
        key_id: None,
        fingerprint: None,
        signer: None,
        reason: Some("the raw message was not retained; refresh the message explicitly before verifying it".to_owned()),
    }
}

fn not_stored_decrypt() -> crate::pgp_mime::PgpDecryptResult {
    crate::pgp_mime::PgpDecryptResult {
        state: crate::pgp_mime::PgpDecryptState::NotStored,
        key_id: None,
        fingerprint: None,
        text: None,
        html: None,
        reason: Some("the raw message was not retained; refresh the message explicitly before decrypting it".to_owned()),
    }
}

pub(crate) fn unknown_thread(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown thread {id:?}; use an id returned by threads_list"))
}

pub(crate) fn unknown_saved_search(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown saved search {id:?}; use an id returned by saved_searches_list"))
}

fn draft_changed(id: &str) -> OpError {
    OpError::InvalidArgs(format!("draft {id:?} changed in another session; refresh the mailbox before editing its attachments"))
}

/// The one sentence for a draft held by its own journal row. It names the state and the two ways
/// out, and it never suggests cancelling a row that cannot be cancelled.
fn outbox_lock_message(what: &str, id: &str, state: OutboxState, action: &str) -> String {
    let exit = match state {
        OutboxState::Failed => "scartala dalla coda e riprova",
        OutboxState::Uncertain => "risolvila dalla coda: potrebbe essere già stata consegnata",
        _ => "attendi l'invio, oppure annullala finché è in coda",
    };
    format!("{what} {id:?} è nella coda ({state}) e non può essere {action}: {exit}")
}

pub(crate) fn unknown_label(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown label {id:?}; use an id returned by labels_list"))
}

const MAX_SUBJECT_BYTES: usize = 998;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RECIPIENTS_PER_FIELD: usize = 100;
/// The composer caps from P8: at most 20 files, 25 MiB each and 25 MiB in total.
pub const MAX_DRAFT_ATTACHMENTS: usize = 20;
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_ATTACHMENT_TOTAL_BYTES: u64 = 25 * 1024 * 1024;
/// A signature is plain text and small: 4 KiB is already a long signature.
pub const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
const MAX_LABEL_NAME_CHARS: usize = 48;
const MAX_LABEL_MESSAGES: usize = 500;
/// `thread_flag` changes at most this many messages in one call.
const MAX_THREAD_MESSAGES: usize = 500;

/// The `threads_list` query as the engine sees it, after the operation parser has validated types.
#[derive(Debug, Clone, Default)]
pub struct ThreadListRequest {
    pub account: Option<String>,
    pub folder: Option<String>,
    pub label: Option<String>,
    pub unread_only: bool,
    pub starred_only: bool,
    pub with_attachments: bool,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// One thread page plus the scope totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPageResult {
    pub threads: Vec<ThreadSummary>,
    pub total: u32,
    pub unread_threads: u32,
    pub unread_messages: u32,
}

/// One thread and an ascending page of its messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadDetailResult {
    pub thread: ThreadSummary,
    pub messages: Vec<MessageSummary>,
    pub total: u32,
    pub truncated: bool,
    pub resolved_id: String,
}

/// The result of a thread flag change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFlagResult {
    pub thread: ThreadSummary,
    pub changed: u32,
}

/// Slack added to an attachment's recorded size when its bytes are read back. A stored blob that is
/// larger than the metadata plus this slack is refused before it is decrypted: a corrupt or swapped
/// row must not turn into a multi-gigabyte allocation.
const ATTACHMENT_READ_SLACK: u64 = 1024 * 1024;

/// Why an operation failed. The message is meant for a person or an agent to read and act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    #[error("unknown operation {0:?}")]
    UnknownOp(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// The server rejected the account's credential. The message names the provider and the likely
    /// cause and never contains the password or any server-supplied text.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// DNS, TCP, TLS, timeout or disconnect failure while talking to the account's server.
    #[error("the server could not be reached: {0}")]
    Network(String),
    #[error("{0}")]
    Failed(String),
}

/// What a successful submission's local finalize did.
enum FinalizeOutcome {
    /// The draft moved to Sent and carries the delivered Message-ID. `server_uid` is `Some` when a
    /// server copy with that Message-ID was already cached and its UID was adopted: the caller must
    /// not APPEND a second copy.
    Moved { server_uid: Option<u32> },
    /// The draft was already moved or deleted: a repeated finalize after a crash is a no-op.
    NotADraft,
    /// The draft was edited after it was serialized. The message was delivered, but the newer draft
    /// must not be labelled with the delivered Message-ID; the server copy arrives with the sync.
    SnapshotChanged,
}

/// Everything `account_add` needs, including the password.
///
/// The operation registry builds this from [`crate::ops::AccountAddArgs`]; callers that are not the
/// registry (tests, and the desktop onboarding in P6b) build it directly. `extra_roots` is the only
/// test hook: it adds trust anchors for the connection check and can never disable verification.
/// There is no `skip_verify` switch and none will be added.
pub struct AccountSetup {
    pub provider: Provider,
    pub email: String,
    pub name: Option<String>,
    /// The password or app password. Zeroised when this setup is dropped.
    pub password: Zeroizing<String>,
    /// Required for [`Provider::Custom`]; refused for the two known providers.
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls: Option<MailTls>,
    /// The submission endpoint. Known providers get their preset when this is absent; custom
    /// accounts may omit it and then can read but not send. The password inside is optional: absent
    /// means "use the IMAP credential".
    pub smtp: Option<SmtpSetup>,
    /// The OAuth grant for an account that authorizes with XOAUTH2 instead of an app password.
    /// `None` is an app-password account, unchanged. When this is present, `password` must be empty:
    /// the engine refuses a setup that carries both, so an account can never fall back silently.
    pub oauth: Option<crate::oauth::OAuthGrant>,
    /// The access token the same code exchange produced, when the caller still has it. `add_account`
    /// probes with it instead of spending a second token request; it is never stored. `None` (a CLI
    /// or desktop that only kept the grant) makes `add_account` refresh once, as before.
    pub oauth_access: Option<crate::oauth::OAuthAccess>,
    /// Extra certificate authorities. Empty in production; tests pass the fake server's CA.
    pub extra_roots: Vec<CertificateDer<'static>>,
}

/// A submission endpoint and, optionally, a separate password.
pub struct SmtpSetup {
    pub host: String,
    pub port: u16,
    pub tls: MailTls,
    pub username: String,
    /// `None` means the IMAP credential is used for SMTP too.
    pub password: Option<Zeroizing<String>>,
}

/// The parent identity a composer needs to start a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTarget {
    /// The parent `Message-ID`, without angle brackets, when the message has one.
    pub parent_message_id: Option<String>,
    /// The parent's `Reply-To` list. Empty means "use From".
    pub reply_to: Vec<Address>,
}

/// Everything `account_add` needs **except** the password.
///
/// The CLI validates this with [`Engine::check_account_request`] before it reads the secret, so an
/// unknown provider, a bad address, a missing store or a duplicate account fails without consuming
/// stdin and without a keyring read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRequest {
    pub provider: Provider,
    pub email: String,
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls: Option<MailTls>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_tls: Option<MailTls>,
    pub smtp_username: Option<String>,
}

/// A fully validated account request, before the password is read or the server is contacted.
struct ResolvedAccount {
    id: String,
    provider: Provider,
    email: String,
    name: String,
    host: String,
    port: u16,
    tls: MailTls,
    smtp: Option<SmtpSettings>,
}

/// Change an existing account's SMTP endpoint without touching its IMAP settings.
///
/// This is deliberately a separate request from [`AccountSetup`]: adding an account probes both
/// servers and writes the whole tree, while updating submission settings must not replace cached
/// mail, labels or the IMAP credential. The operation is sensitive because it can carry a password.
#[derive(Clone)]
pub struct AccountUpdateRequest {
    pub id: String,
    /// Set the endpoint. `None` is a caller mistake unless `clear_smtp` is true.
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_tls: Option<MailTls>,
    pub smtp_username: Option<String>,
    /// A new SMTP password, or `None` to keep the existing SMTP secret / fall back to IMAP.
    pub smtp_password: Option<Zeroizing<String>>,
    /// Remove the submission endpoint and its separate credential.
    pub clear_smtp: bool,
}

pub struct Engine {
    accounts: Vec<Account>,
    folders: Vec<Folder>,
    /// List rows only. A store-backed engine does not hold a body, an HTML part or an attachment
    /// tree at open; [`Engine::message`] reads one message on demand and keeps a small cache.
    messages: Vec<MessageSummary>,
    /// Id → position in `messages`. Rebuilt after a structural change, so a one-message lookup is
    /// O(1) even with 100k rows and the list stays a contiguous slice for the UI.
    message_index: HashMap<String, usize>,
    /// The full synthetic messages of the demo engine. Empty for a store-backed engine.
    demo_messages: HashMap<String, Message>,
    /// True when this engine has no store at all. Unlike `store.is_none()`, this stays true/false
    /// when an outbox pass temporarily takes the connection out of the engine.
    in_memory: bool,
    /// A bounded cache of full messages read from the store, keyed by id and engine revision. Any
    /// write bumps the revision, so a stale body can never be served after a flag, label or draft
    /// change.
    body_cache: RefCell<VecDeque<(u64, Message)>>,
    revision: u64,
    labels: Vec<Label>,
    next_label_seq: u32,
    demo: bool,
    /// `None` for the demo and the empty engine; every mutation is memory-only there.
    store: Option<Store>,
    /// The OS-keyring seam. `None` for an engine that was opened without one; account and sync
    /// operations then fail with a readable sentence instead of touching a real vault.
    vault: Option<Arc<dyn CredentialVault>>,
    /// Extra trust anchors for IMAP connections. Empty in production. Tests use
    /// [`Engine::set_test_extra_roots`] to trust the fake server's throwaway CA; this can only add
    /// anchors and never disables verification.
    pub(crate) extra_roots: Vec<CertificateDer<'static>>,
    /// Test seam: stop right after the server's `250` and before the durable
    /// `append_pending` transition, as a process death would. Only a debug/test build has it; a
    /// release build compiles the field and the check out.
    #[cfg(debug_assertions)]
    crash_after_accept: bool,
    /// Test seam: run once between reading an outbox row and its conditional write, so a test can
    /// model a concurrent process without a timing race. Debug/test builds only.
    #[cfg(debug_assertions)]
    test_after_outbox_read: Option<Box<dyn FnOnce() + Send>>,
    /// Test clock: when present, the engine reads it instead of the wall clock for every claim. A
    /// test advances it while a fake server is inside a long transfer. Debug/test builds only.
    #[cfg(debug_assertions)]
    test_clock: Option<Arc<std::sync::atomic::AtomicI64>>,
    /// Attachment id → BLAKE3 hex hash of its stored blob bytes, for attachments that have bytes.
    ///
    /// Core's `Attachment` deliberately carries no `blob_hash`: it is part of the `message_get`
    /// output and of the draft flow, and a storage detail does not belong in that surface. The engine
    /// keeps the in-memory half of the mapping (hydrated by [`Engine::open`], updated after a blob
    /// write) and the repository row stays the source of truth.
    blob_hashes: HashMap<String, String>,
    /// Draft id → the store revision this engine loaded. The map is metadata-only and independent of
    /// the eight-entry body cache: a stale queue must still be refused after eviction.
    draft_snapshots: HashMap<String, u32>,
    /// Draft id → parent `Message-ID` for a reply, hydrated from `drafts.in_reply_to` on open.
    ///
    /// The display message deliberately has no reply field: a reply link belongs to the draft row,
    /// not to a message summary or to a body. The engine owns this small map and the serializer reads
    /// it; `save_draft` keeps the stored link when a patch omits it.
    reply_links: HashMap<String, String>,
    /// Saved searches in name order. The store is authoritative; this is the hydrated copy the UI
    /// lists every frame, refreshed after every CRUD call.
    saved_searches: Vec<SavedSearch>,
    /// Next demo saved-search number. A store-backed engine uses the store's monotonic meta counter
    /// instead, so deleting the highest row never reuses its id.
    next_saved_search_seq: u32,
    /// Computed saved-search counts, keyed by the engine revision. A count scan over the index is
    /// expensive at 100k, so the list reuses it until a write invalidates the revision.
    saved_counts: RefCell<Option<(u64, HashMap<String, SavedCount>)>>,
    /// Recent successful queries per account, newest first, at most eight. Persisted in the
    /// encrypted `meta` table for a store-backed engine and kept in memory for the demo.
    recent_searches: HashMap<String, Vec<String>>,
    /// P9c rules in `priority ASC, id ASC` order. The store is authoritative for a persistent
    /// engine; the demo keeps the same rows in memory.
    pub(crate) rules: Vec<crate::model::Rule>,
    /// Next demo rule number. A store-backed engine uses the store's monotonic meta counter.
    pub(crate) next_rule_seq: u32,
    /// Demo-only rule journal. A store-backed engine writes `rule_runs`/`rule_actions`.
    pub(crate) rule_runs: Vec<crate::model::RuleRun>,
    pub(crate) rule_run_actions: HashMap<String, Vec<postino_store::RuleActionRecord>>,
    pub(crate) next_rule_run_seq: u32,
    /// Demo-only `rules_applied_at`: messages a `new` run has already considered.
    pub(crate) demo_rules_applied: BTreeSet<String>,
    /// True when this engine may queue outbound mail. The desktop and the CLI default to true; the
    /// MCP server sets it to the `--allow-send` flag, so a sync-triggered rule pass cannot forward
    /// behind the MCP gate.
    pub(crate) allow_send: bool,
    /// Demo-only `all` page cursors, keyed by rule id.
    pub(crate) demo_rule_all_cursors: HashMap<String, (i64, String)>,
    /// Account id → the access token of an OAuth account and its expiry. Memory only: never written to
    /// the store, a log or an operation result, and dropped with the engine. A `RefCell` because reads
    /// (`test_account`, `sync_now`) take `&self` and still refresh once.
    pub(crate) oauth_access: RefCell<HashMap<String, crate::oauth::CachedAccess>>,
    /// Test-only override of the provider endpoints, set by [`Engine::set_test_oauth_endpoints`].
    #[cfg(debug_assertions)]
    pub(crate) test_oauth_endpoints: Option<(String, String, String)>,
    /// Test-only Gmail IMAP/SMTP endpoints, set by [`Engine::set_test_oauth_hosts`].
    #[cfg(debug_assertions)]
    pub(crate) test_oauth_hosts: Option<TestOAuthHosts>,
}

/// Test-only Gmail endpoints: IMAP host and port, plus the SMTP host and port when the preset has
/// one. Only `cfg(test)` code and integration tests through the public hook set it.
#[cfg(debug_assertions)]
pub(crate) type TestOAuthHosts = (String, u16, Option<(String, u16)>);

/// The computed half of a saved-search row.
#[derive(Debug, Clone, Default)]
struct SavedCount {
    valid: bool,
    error: Option<String>,
    total: u32,
    unread: u32,
    pending: bool,
}

pub(crate) const MAX_SAVED_SEARCHES: u32 = 12;
pub(crate) const MAX_SAVED_SEARCH_NAME_CHARS: usize = 48;
pub(crate) const MAX_RECENT_SEARCHES: usize = 8;

impl Engine {
    /// An engine with no accounts. Every list is empty until a store or demo data is attached.
    pub fn empty() -> Self {
        Self {
            accounts: Vec::new(),
            folders: Vec::new(),
            messages: Vec::new(),
            message_index: HashMap::new(),
            demo_messages: HashMap::new(),
            in_memory: true,
            body_cache: RefCell::new(VecDeque::new()),
            revision: 0,
            labels: Vec::new(),
            next_label_seq: 1,
            demo: false,
            store: None,
            vault: None,
            extra_roots: Vec::new(),
            #[cfg(debug_assertions)]
            crash_after_accept: false,
            #[cfg(debug_assertions)]
            test_after_outbox_read: None,
            #[cfg(debug_assertions)]
            test_clock: None,
            blob_hashes: HashMap::new(),
            draft_snapshots: HashMap::new(),
            reply_links: HashMap::new(),
            saved_searches: Vec::new(),
            next_saved_search_seq: 1,
            saved_counts: RefCell::new(None),
            recent_searches: HashMap::new(),
            rules: Vec::new(),
            next_rule_seq: 1,
            rule_runs: Vec::new(),
            rule_run_actions: HashMap::new(),
            next_rule_run_seq: 1,
            demo_rules_applied: BTreeSet::new(),
            allow_send: true,
            demo_rule_all_cursors: HashMap::new(),
            oauth_access: RefCell::new(HashMap::new()),
            #[cfg(debug_assertions)]
            test_oauth_endpoints: None,
            #[cfg(debug_assertions)]
            test_oauth_hosts: None,
        }
    }

    /// The M0 walking skeleton: a synthetic account with realistic mail, held in memory only.
    pub fn demo() -> Self {
        crate::demo::seed()
    }

    /// The M0 walking skeleton seeded as if the clock read `now`. A screenshot passes a fixed
    /// instant so the rendered dates never depend on the wall clock.
    pub fn demo_at(now: chrono::DateTime<Utc>) -> Self {
        crate::demo::seed_at(now)
    }

    /// Load the mailbox from `store` and keep it for write-through.
    ///
    /// The load is bounded by the table count, not by the mailbox: accounts, folders, orphan
    /// drafts, message rows with their labels and attachments grouped by message. A draft row
    /// without a message row is repaired into a real message row in one transaction *before* the
    /// mailbox is hydrated; if that repair fails the open fails with an actionable error instead of
    /// serving a draft that cannot be flagged, labelled or moved.
    ///
    /// This form has no credential vault: mail reads and writes work, while `account_add`,
    /// `account_test`, `account_remove` and sync answer with a readable error. The CLI uses
    /// [`Engine::open_with_vault`].
    pub fn open(store: Store) -> Result<Self, OpError> {
        Self::open_inner(store, None)
    }

    /// Load the mailbox and attach the keyring seam used by the account and sync operations.
    ///
    /// The store already owns the SQLCipher key; the vault holds only account credentials. Tests pass
    /// an in-memory vault; the CLI passes `postino-platform`'s keyring adapter. Nothing in this
    /// function touches a vault, so a test that only reads mail still cannot reach the real keyring.
    pub fn open_with_vault(store: Store, vault: Arc<dyn CredentialVault>) -> Result<Self, OpError> {
        Self::open_inner(store, Some(vault))
    }

    /// Add certificate authorities to every IMAP connection this engine opens.
    ///
    /// This exists for tests: the fake server trusts a throwaway CA and the client must be told about
    /// it. It augments the operating system's roots and cannot disable verification, and it is not an
    /// operation, so no caller can set it from JSON. Production builds leave it empty.
    pub fn set_test_extra_roots(&mut self, roots: Vec<CertificateDer<'static>>) {
        self.extra_roots = roots;
    }

    /// Point the Gmail OAuth provider at a scripted token endpoint. Tests only, same contract as
    /// [`Engine::set_test_extra_roots`]: it is a debug-build hook, not an operation, so no caller can
    /// redirect a production refresh from JSON. The browser authorize URL is replaced too, but it is
    /// never opened by the engine.
    #[cfg(debug_assertions)]
    pub fn set_test_oauth_endpoints(&mut self, authorize: String, token: String, revoke: String) {
        self.test_oauth_endpoints = Some((authorize, token, revoke));
    }

    /// Point a Gmail account at a local fake IMAP/SMTP server. Tests only, same contract as
    /// [`Engine::set_test_extra_roots`]. A debug-build hook, never reachable from JSON.
    #[cfg(debug_assertions)]
    pub fn set_test_oauth_hosts(&mut self, imap: (String, u16), smtp: Option<(String, u16)>) {
        self.test_oauth_hosts = Some((imap.0, imap.1, smtp));
    }

    /// Set whether this engine may queue outbound mail. The desktop and CLI leave this true; the MCP
    /// server sets it from `--allow-send` before serving, so `sync_now`'s automatic rule pass cannot
    /// forward behind the MCP gate.
    pub fn set_allow_send(&mut self, allow: bool) {
        self.allow_send = allow;
    }

    pub fn allow_send(&self) -> bool {
        self.allow_send
    }

    /// Stop after the SMTP server accepted the message and before the durable `append_pending`
    /// transition, as a process death at the narrowest crash window would.
    ///
    /// This is a test seam, not an operation: it can only make a pass stop earlier. The row stays
    /// `sending`, so the next pass after the lease turns it `uncertain` and never resubmits. The
    /// caller sees a failed pass and a journal that says the outcome is unknown.
    /// Stop after the SMTP server accepted the message and before the durable `append_pending`
    /// transition, as a process death at the narrowest crash window would.
    ///
    /// This is a test seam, not an operation: it can only make a pass stop earlier. The row stays
    /// `sending`, so the next pass after the lease turns it `uncertain` and never resubmits. The
    /// caller sees a failed pass and a journal that says the outcome is unknown. It exists only in a
    /// debug/test build (`debug_assertions`); a release build does not contain the field or the check.
    #[doc(hidden)]
    #[cfg(debug_assertions)]
    pub fn set_test_crash_after_accept(&mut self, enabled: bool) {
        self.crash_after_accept = enabled;
    }

    /// Test seam: run `hook` once between reading an outbox row and its conditional write. A test
    /// uses it to model a second process changing the row at exactly that instant without a timing
    /// race. Debug/test builds only.
    #[doc(hidden)]
    #[cfg(debug_assertions)]
    pub fn set_test_after_outbox_read(&mut self, hook: impl FnOnce() + Send + 'static) {
        self.test_after_outbox_read = Some(Box::new(hook));
    }

    /// Test seam: drive the engine's claim clock from a test, so a fake server can advance time
    /// while it holds a transfer open. Debug/test builds only.
    #[doc(hidden)]
    #[cfg(debug_assertions)]
    pub fn set_test_clock(&mut self, clock: Arc<std::sync::atomic::AtomicI64>) {
        self.test_clock = Some(clock);
    }

    /// The current instant for a claim. In a test with an injected clock it is exactly that value;
    /// otherwise it is the wall clock.
    fn clock_now(&self) -> i64 {
        #[cfg(debug_assertions)]
        if let Some(clock) = &self.test_clock {
            return clock.load(std::sync::atomic::Ordering::SeqCst);
        }
        Utc::now().timestamp_millis()
    }

    /// The instant to stamp on a lease/claim: never earlier than the caller's pass start, so a test
    /// that processes at a synthetic time keeps working, but a long transfer inside the pass is
    /// stamped with the time it actually happened, not with the pass start.
    fn claim_now(&self, pass_start: i64) -> i64 {
        self.clock_now().max(pass_start)
    }

    fn open_inner(store: Store, vault: Option<Arc<dyn CredentialVault>>) -> Result<Self, OpError> {
        let accounts = store.list_accounts().map_err(store_map::read_error)?.into_iter().map(store_map::account).collect::<Vec<_>>();
        let folders = store.list_folders(None).map_err(store_map::read_error)?.into_iter().map(store_map::folder).collect::<Vec<_>>();
        if !store.is_read_only() {
            repair_orphan_drafts(&store, &accounts, &folders)?;
        }
        // One list-row query and one attachment-hash query. Bodies, HTML parts and attachment rows
        // wait for a message open; a 100k mailbox must fit in a summary vector.
        let messages = store.list_all_summaries().map_err(store_map::read_error)?.into_iter().map(store_map::summary).collect::<Vec<_>>();
        let blob_hashes = store.list_blob_hashes().map_err(store_map::read_error)?;
        let labels = store.list_labels(None).map_err(store_map::read_error)?.into_iter().map(store_map::label).collect::<Result<Vec<_>, _>>()?;
        let label_seq =
            if store.is_read_only() { labels.iter().filter_map(label_number).max().unwrap_or(0) } else { repair_label_seq(&store, &labels)? };
        // Draft metadata only: no body, headers or attachments. The revision map is the stale-draft
        // guard, independent of the body cache.
        let drafts = store.list_draft_metadata().map_err(store_map::read_error)?;
        let reply_links =
            drafts.iter().filter_map(|draft| draft.in_reply_to.clone().map(|parent| (draft.id.clone(), parent))).collect::<HashMap<_, _>>();
        let draft_snapshots = drafts.into_iter().map(|draft| (draft.id, draft.revision)).collect::<HashMap<_, _>>();
        // P9a stores have messages but no thread cache. Build it once here, from the message rows,
        // so the first `threads_list` is a cache read and never a full derivation.
        store.ensure_thread_cache().map_err(store_map::read_error)?;
        let saved_searches = store.list_saved_searches(None).map_err(store_map::read_error)?.into_iter().map(store_map::saved_search).collect();
        let rules = store.list_all_rules().map_err(store_map::read_error)?.into_iter().map(store_map::rule).collect::<Vec<_>>();
        let mut recent_searches = HashMap::new();
        for account in &accounts {
            let recent = store.recent_searches(&account.id).map_err(store_map::read_error)?;
            if !recent.is_empty() {
                recent_searches.insert(account.id.clone(), recent);
            }
        }

        let mut engine = Self::from_summaries(accounts, folders, messages, labels, false, Some(store));
        engine.vault = vault;
        engine.next_label_seq = label_seq.saturating_add(1);
        engine.blob_hashes = blob_hashes;
        engine.reply_links = reply_links;
        engine.draft_snapshots = draft_snapshots;
        engine.saved_searches = saved_searches;
        engine.rules = rules;
        engine.next_rule_seq = engine
            .rules
            .iter()
            .filter_map(|rule| rule.id.strip_prefix("rule_").and_then(|rest| rest.parse::<u32>().ok()))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        engine.next_saved_search_seq = engine
            .saved_searches
            .iter()
            .filter_map(|saved| saved.id.strip_prefix("saved_").and_then(|rest| rest.parse::<u32>().ok()))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        engine.recent_searches = recent_searches;
        Ok(engine)
    }

    pub(crate) fn from_parts(
        accounts: Vec<Account>,
        folders: Vec<Folder>,
        messages: Vec<Message>,
        labels: Vec<Label>,
        demo: bool,
        store: Option<Store>,
    ) -> Self {
        let summaries = messages.iter().map(|message| message.summary.clone()).collect::<Vec<_>>();
        let in_memory = store.is_none();
        let mut engine = Self::from_summaries(accounts, folders, summaries, labels, demo, store);
        if in_memory {
            engine.demo_messages = messages.into_iter().map(|message| (message.summary.id.clone(), message)).collect();
        }
        engine
    }

    fn from_summaries(
        accounts: Vec<Account>,
        folders: Vec<Folder>,
        messages: Vec<MessageSummary>,
        labels: Vec<Label>,
        demo: bool,
        store: Option<Store>,
    ) -> Self {
        let next_label_seq = labels.iter().filter_map(label_number).max().unwrap_or(0).saturating_add(1);
        let mut engine = Self {
            accounts,
            folders,
            messages,
            message_index: HashMap::new(),
            demo_messages: HashMap::new(),
            in_memory: store.is_none(),
            body_cache: RefCell::new(VecDeque::new()),
            revision: 0,
            labels,
            next_label_seq,
            demo,
            store,
            vault: None,
            extra_roots: Vec::new(),
            oauth_access: RefCell::new(HashMap::new()),
            #[cfg(debug_assertions)]
            test_oauth_endpoints: None,
            #[cfg(debug_assertions)]
            test_oauth_hosts: None,
            #[cfg(debug_assertions)]
            crash_after_accept: false,
            #[cfg(debug_assertions)]
            test_after_outbox_read: None,
            #[cfg(debug_assertions)]
            test_clock: None,
            blob_hashes: HashMap::new(),
            draft_snapshots: HashMap::new(),
            reply_links: HashMap::new(),
            saved_searches: Vec::new(),
            next_saved_search_seq: 1,
            saved_counts: RefCell::new(None),
            recent_searches: HashMap::new(),
            rules: Vec::new(),
            next_rule_seq: 1,
            rule_runs: Vec::new(),
            rule_run_actions: HashMap::new(),
            next_rule_run_seq: 1,
            demo_rules_applied: BTreeSet::new(),
            allow_send: true,
            demo_rule_all_cursors: HashMap::new(),
        };
        engine.rebuild_message_index();
        engine.recount();
        engine
    }

    /// The persisted search-index status. Read-only; a demo engine has no index.
    pub fn search_index_status(&self) -> Result<Option<postino_store::SearchIndexStatus>, OpError> {
        match &self.store {
            Some(store) => store.search_status().map(Some).map_err(store_map::read_error),
            None => Ok(None),
        }
    }

    /// One resumable index batch. The desktop shell calls this while showing the progress banner.
    pub fn advance_search_index(&self) -> Result<Option<postino_store::SearchIndexStatus>, OpError> {
        let Some(store) = &self.store else { return Ok(None) };
        let _ = store.ensure_search_index_batch();
        store.search_progress().map(Some).map_err(store_map::read_error)
    }

    /// One bounded blob-GC batch. The daemon runs it after a successful sync and on its idle timer;
    /// the store rechecks every reference before unlinking, so a row added concurrently is safe.
    pub fn gc_blobs(&self, limit: usize) -> Result<postino_store::BlobGcReport, OpError> {
        self.require_store("blob_gc")?.gc_unreferenced_blobs(limit).map_err(store_map::write_error)
    }

    /// Read one internal `meta` value. Used by the daemon for notification watermarks; the demo
    /// engine has no store and answers `None`. Keys and values are app-owned, never user input.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>, OpError> {
        if key.is_empty() || key.chars().count() > 128 {
            return Err(OpError::InvalidArgs("an internal meta key must be 1-128 characters".to_owned()));
        }
        match &self.store {
            Some(store) => store.get_meta(key).map_err(store_map::read_error),
            None => Ok(None),
        }
    }

    /// Write one internal `meta` value. Bounded so a watermark list cannot grow without limit.
    pub fn meta_set(&mut self, key: &str, value: &str) -> Result<(), OpError> {
        if key.is_empty() || key.chars().count() > 128 {
            return Err(OpError::InvalidArgs("an internal meta key must be 1-128 characters".to_owned()));
        }
        if value.chars().count() > 16 * 1024 {
            return Err(OpError::InvalidArgs("an internal meta value is too long".to_owned()));
        }
        match &self.store {
            Some(store) => store.set_meta(key, value).map_err(store_map::write_error),
            None => Ok(()),
        }
    }
    /// True for the synthetic demo mailbox. Data written by a store-backed engine is real local
    /// data even while no server is connected.
    pub fn is_demo(&self) -> bool {
        self.demo
    }

    /// True when mutations are written to the encrypted store rather than kept in memory.
    pub fn is_persistent(&self) -> bool {
        self.store.is_some()
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    pub fn messages(&self) -> &[MessageSummary] {
        &self.messages
    }

    /// The list row for one message, if it exists. O(1), no body.
    pub fn summary(&self, id: &str) -> Option<&MessageSummary> {
        self.message_index.get(id).and_then(|index| self.messages.get(*index))
    }

    /// One full message, owned. A store-backed engine reads it from SQL and keeps a small cache; the
    /// demo engine returns its synthetic copy. A storage error reads as `None` here; operations use
    /// [`Engine::message_result`] to report the cause.
    pub fn message(&self, id: &str) -> Option<Message> {
        self.message_result(id).ok().flatten()
    }

    /// The monotic revision of the in-memory view. The desktop app keys its search/list cache on
    /// `(signature, revision)` and every write or reload bumps it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn message_result(&self, id: &str) -> Result<Option<Message>, OpError> {
        if let Some(message) = self.demo_messages.get(id) {
            return Ok(Some(message.clone()));
        }
        {
            let cache = self.body_cache.borrow();
            if let Some((_, cached)) = cache.iter().rev().find(|(revision, message)| *revision == self.revision && message.summary.id == id) {
                return Ok(Some(cached.clone()));
            }
        }
        let Some(store) = &self.store else { return Ok(None) };
        let Some((row, attachments)) = store.get_message_with_attachments(id).map_err(store_map::read_error)? else { return Ok(None) };
        let message = store_map::message(row, attachments)?;
        self.cache_message(&message);
        Ok(Some(message))
    }

    fn cache_message(&self, message: &Message) {
        let mut cache = self.body_cache.borrow_mut();
        if cache.front().is_none_or(|(revision, _)| *revision != self.revision) {
            cache.clear();
        }
        cache.retain(|(_, cached)| cached.summary.id != message.summary.id);
        cache.push_back((self.revision, message.clone()));
        while cache.len() > 8 {
            cache.pop_front();
        }
    }

    fn rebuild_message_index(&mut self) {
        self.message_index = self.messages.iter().enumerate().map(|(index, message)| (message.id.clone(), index)).collect();
    }

    pub(crate) fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.body_cache.borrow_mut().clear();
    }

    fn remember_draft_revision(&mut self, id: &str, revision: u32) {
        self.draft_snapshots.insert(id.to_owned(), revision);
    }

    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    /// The parent `Message-ID` of a reply draft, without angle brackets.
    ///
    /// The link lives on the draft row, not on the message summary; the composer reads it when it
    /// reopens a draft so the next save cannot silently drop the linkage.
    pub fn draft_reply_link(&self, draft_id: &str) -> Option<&str> {
        self.reply_links.get(draft_id).map(String::as_str)
    }

    pub fn label(&self, id: &str) -> Option<&Label> {
        self.labels.iter().find(|label| label.id == id)
    }

    /// Labels with mailbox-wide counts. A store-backed engine reads the live counts from the store;
    /// the demo computes them in one pass over its summaries.
    pub fn label_summaries(&self) -> Vec<LabelSummary> {
        if let Some(store) = &self.store
            && let Ok(rows) = store.list_labels(None)
        {
            let mut summaries = rows
                .into_iter()
                .filter_map(|row| {
                    let color = LabelColor::parse(&row.label.color)?;
                    Some(LabelSummary {
                        id: row.label.id,
                        name: row.label.name,
                        color,
                        message_count: row.message_count,
                        unread_count: row.unread_count,
                    })
                })
                .collect::<Vec<_>>();
            summaries.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()).then(left.id.cmp(&right.id)));
            return summaries;
        }
        let mut counts: HashMap<&str, (u32, u32)> = HashMap::new();
        for message in &self.messages {
            for id in &message.labels {
                let entry = counts.entry(id.as_str()).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(1);
                if message.unread {
                    entry.1 = entry.1.saturating_add(1);
                }
            }
        }
        self.labels
            .iter()
            .map(|label| {
                let (message_count, unread_count) = counts.get(label.id.as_str()).copied().unwrap_or((0, 0));
                LabelSummary { id: label.id.clone(), name: label.name.clone(), color: label.color, message_count, unread_count }
            })
            .collect()
    }

    pub fn label_summary(&self, id: &str) -> Option<LabelSummary> {
        if let Some(store) = &self.store
            && let Ok(rows) = store.list_labels(None)
            && let Some(row) = rows.into_iter().find(|row| row.label.id == id)
        {
            let color = LabelColor::parse(&row.label.color).or_else(|| self.label(id).map(|label| label.color))?;
            return Some(LabelSummary {
                id: row.label.id,
                name: row.label.name,
                color,
                message_count: row.message_count,
                unread_count: row.unread_count,
            });
        }
        let label = self.label(id)?;
        let (message_count, unread_count) = self
            .messages
            .iter()
            .filter(|message| message.labels.iter().any(|candidate| candidate == id))
            .fold((0u32, 0u32), |(total, unread), message| (total.saturating_add(1), unread.saturating_add(if message.unread { 1 } else { 0 })));
        Some(LabelSummary { id: label.id.clone(), name: label.name.clone(), color: label.color, message_count, unread_count })
    }

    // -----------------------------------------------------------------------------------------
    // Threads (P9b)
    // -----------------------------------------------------------------------------------------

    /// Threads with at least one message in scope. Row counts are mailbox-wide.
    pub fn threads_list(&self, request: &ThreadListRequest) -> Result<ThreadPageResult, OpError> {
        let account = self.requested_account(request.account.as_deref(), "threads_list")?;
        let folder = match request.folder.as_deref() {
            Some(value) => Some(self.resolve_thread_folder(value, &account.id)?),
            None => None,
        };
        let label = match request.label.as_deref() {
            Some(value) => Some(self.resolve_thread_label(value)?),
            None => None,
        };
        let (folder_id, starred) = match folder {
            Some(folder) if folder.kind == FolderKind::Starred => (None, Some(true)),
            Some(folder) => (Some(folder.id), request.starred_only.then_some(true)),
            None => (None, request.starred_only.then_some(true)),
        };
        let limit = request.limit.unwrap_or(50).clamp(1, 200);
        let offset = request.offset.unwrap_or(0).min(5_000);
        if self.store.is_some() {
            let store = self.require_thread_cache()?;
            let page = store
                .list_threads(&postino_store::ThreadFilter {
                    account_id: Some(account.id.clone()),
                    folder_id,
                    label_id: label.as_ref().map(|label| label.id.clone()),
                    unread: request.unread_only.then_some(true),
                    starred,
                    has_attachments: request.with_attachments.then_some(true),
                    limit,
                    offset,
                })
                .map_err(store_map::read_error)?;
            return Ok(ThreadPageResult {
                threads: page.threads.into_iter().map(store_map::thread_summary).collect(),
                total: page.total,
                unread_threads: page.unread_threads,
                unread_messages: page.unread_messages,
            });
        }
        Ok(self.demo_threads(&account.id, folder_id.as_deref(), label.as_ref().map(|label| label.id.as_str()), request, starred, limit, offset))
    }

    /// One thread with an ascending page of its messages. An old merged id resolves through aliases.
    pub fn thread_get(&self, account: Option<&str>, id: &str, limit: Option<u32>, offset: Option<u32>) -> Result<ThreadDetailResult, OpError> {
        let account = self.requested_account(account, "thread_get")?;
        let limit = limit.unwrap_or(50).clamp(1, 200);
        let offset = offset.unwrap_or(0).min(5_000);
        if self.store.is_some() {
            let store = self.require_thread_cache()?;
            let detail = store.get_thread(&account.id, id, limit, offset).map_err(store_map::read_error)?.ok_or_else(|| unknown_thread(id))?;
            let total = detail.total;
            return Ok(ThreadDetailResult {
                thread: store_map::thread_summary(detail.thread),
                messages: detail.messages.into_iter().map(store_map::summary).collect(),
                total,
                truncated: total > offset.saturating_add(limit),
                resolved_id: store.canonical_thread(&account.id, id).map_err(store_map::read_error)?,
            });
        }
        let scoped = self.demo_thread_messages(&account.id, id);
        if scoped.is_empty() {
            return Err(unknown_thread(id));
        }
        let total = u32::try_from(scoped.len()).unwrap_or(u32::MAX);
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let take = usize::try_from(limit).unwrap_or(0);
        let messages = scoped.iter().skip(start).take(take).map(|message| (*message).clone()).collect::<Vec<_>>();
        Ok(ThreadDetailResult {
            thread: self.demo_thread_summary(&account.id, id).ok_or_else(|| unknown_thread(id))?,
            truncated: total > offset.saturating_add(limit),
            resolved_id: id.to_owned(),
            messages,
            total,
        })
    }

    /// Set the read/star state of every message in one thread. At most 500 messages; idempotent.
    pub fn thread_flag(&mut self, account: Option<&str>, id: &str, unread: Option<bool>, starred: Option<bool>) -> Result<ThreadFlagResult, OpError> {
        if unread.is_none() && starred.is_none() {
            return Err(OpError::InvalidArgs("provide unread and/or starred".to_owned()));
        }
        let account = self.requested_account(account, "thread_flag")?;
        let (ids, before, resolved) = if let Some(store) = &self.store {
            let ids = store.thread_message_ids(&account.id, id).map_err(store_map::read_error)?;
            let before = store.thread_flags(&account.id, id).map_err(store_map::read_error)?;
            let resolved = store.canonical_thread(&account.id, id).map_err(store_map::read_error)?;
            (ids, before, resolved)
        } else {
            let scoped = self.demo_thread_messages(&account.id, id);
            if scoped.is_empty() {
                return Err(unknown_thread(id));
            }
            let before = scoped.iter().map(|message| (message.id.clone(), message.unread, message.starred)).collect();
            (scoped.iter().map(|message| message.id.clone()).collect(), before, id.to_owned())
        };
        if ids.is_empty() {
            return Err(unknown_thread(id));
        }
        if ids.len() > MAX_THREAD_MESSAGES {
            return Err(OpError::InvalidArgs(format!(
                "thread {resolved:?} has {} messages; thread_flag changes at most {MAX_THREAD_MESSAGES} at once",
                ids.len()
            )));
        }
        let changed = before
            .iter()
            .filter(|(_, was_unread, was_starred)| {
                unread.is_some_and(|wanted| wanted != *was_unread) || starred.is_some_and(|wanted| wanted != *was_starred)
            })
            .count();
        if changed > 0 {
            if let Some(store) = &self.store {
                store.set_flags(&ids, postino_store::FlagUpdate { unread, starred }).map_err(store_map::write_error)?;
                let touched = ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
                for message in &mut self.messages {
                    if !touched.contains(message.id.as_str()) {
                        continue;
                    }
                    if let Some(wanted) = unread {
                        message.unread = wanted;
                    }
                    if let Some(wanted) = starred {
                        message.starred = wanted;
                    }
                }
                for message in self.demo_messages.values_mut() {
                    if !touched.contains(message.summary.id.as_str()) {
                        continue;
                    }
                    if let Some(wanted) = unread {
                        message.summary.unread = wanted;
                    }
                    if let Some(wanted) = starred {
                        message.summary.starred = wanted;
                    }
                }
                self.bump_revision();
                self.recount();
            } else {
                for (message_id, was_unread, was_starred) in &before {
                    let next_unread = unread.unwrap_or(*was_unread);
                    let next_starred = starred.unwrap_or(*was_starred);
                    if next_unread != *was_unread || next_starred != *was_starred {
                        self.set_flags(message_id, unread, starred)?;
                    }
                }
            }
        }
        let thread = self.thread_summary(&account.id, &resolved).ok_or_else(|| unknown_thread(id))?;
        Ok(ThreadFlagResult { thread, changed: u32::try_from(changed).unwrap_or(u32::MAX) })
    }

    /// One thread row by id, for the operation results and the UI.
    pub fn thread_summary(&self, account_id: &str, id: &str) -> Option<ThreadSummary> {
        if let Some(store) = &self.store {
            return store.get_thread(account_id, id, 1, 0).ok().flatten().map(|detail| store_map::thread_summary(detail.thread));
        }
        self.demo_thread_summary(account_id, id)
    }

    fn resolve_thread_folder(&self, value: &str, account: &str) -> Result<Folder, OpError> {
        let wanted = value.to_lowercase();
        self.folders
            .iter()
            .find(|folder| account == folder.account_id && (folder.id == value || folder.name.to_lowercase() == wanted))
            .cloned()
            .ok_or_else(|| unknown_folder(value))
    }

    fn resolve_thread_label(&self, value: &str) -> Result<Label, OpError> {
        let wanted = value.to_lowercase();
        self.labels.iter().find(|label| label.id == value || label.name.to_lowercase() == wanted).cloned().ok_or_else(|| unknown_label(value))
    }

    fn demo_thread_messages<'a>(&'a self, account_id: &str, id: &str) -> Vec<&'a MessageSummary> {
        let mut messages = self
            .messages
            .iter()
            .filter(|message| {
                message.thread_id == id
                    && self.folders.iter().find(|folder| folder.id == message.folder_id).is_some_and(|folder| folder.account_id == account_id)
            })
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| (message.date, message.id.clone()));
        messages
    }

    fn demo_thread_summary(&self, account_id: &str, id: &str) -> Option<ThreadSummary> {
        let messages = self.demo_thread_messages(account_id, id);
        let first = *messages.first()?;
        let latest = *messages.last()?;
        let mut participants: Vec<Address> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        let mut unread_count = 0u32;
        let mut starred_count = 0u32;
        for message in &messages {
            unread_count = unread_count.saturating_add(u32::from(message.unread));
            starred_count = starred_count.saturating_add(u32::from(message.starred));
            if participants.len() < 5 && !participants.iter().any(|known| known.email.eq_ignore_ascii_case(&message.from.email)) {
                participants.push(message.from.clone());
            }
            for label in &message.labels {
                if labels.len() < 8 && !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
        }
        Some(ThreadSummary {
            id: id.to_owned(),
            account_id: account_id.to_owned(),
            subject: first.subject.clone(),
            snippet: latest.snippet.clone(),
            participants,
            last_date: latest.date,
            message_count: u32::try_from(messages.len()).unwrap_or(u32::MAX),
            unread_count,
            starred_count,
            has_attachments: messages.iter().any(|message| message.has_attachments),
            latest_message_id: Some(latest.id.clone()),
            labels,
            state: if unread_count > 0 { "unread".to_owned() } else { "read".to_owned() },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn demo_threads(
        &self,
        account_id: &str,
        folder_id: Option<&str>,
        label_id: Option<&str>,
        request: &ThreadListRequest,
        starred: Option<bool>,
        limit: u32,
        offset: u32,
    ) -> ThreadPageResult {
        let in_scope = |message: &MessageSummary| {
            self.folders.iter().find(|folder| folder.id == message.folder_id).is_some_and(|folder| folder.account_id == account_id)
                && folder_id.is_none_or(|id| message.folder_id == id)
                && label_id.is_none_or(|id| message.labels.iter().any(|label| label == id))
                && (!request.unread_only || message.unread)
                && starred.is_none_or(|wanted| message.starred == wanted)
                && (!request.with_attachments || message.has_attachments)
        };
        let ids = self.messages.iter().filter(|message| in_scope(message)).map(|message| message.thread_id.clone()).collect::<BTreeSet<_>>();
        let mut threads = ids.iter().filter_map(|id| self.demo_thread_summary(account_id, id)).collect::<Vec<_>>();
        threads.sort_by(|left, right| right.last_date.cmp(&left.last_date).then(left.id.cmp(&right.id)));
        let total = u32::try_from(threads.len()).unwrap_or(u32::MAX);
        let unread_threads = u32::try_from(threads.iter().filter(|thread| thread.unread_count > 0).count()).unwrap_or(u32::MAX);
        let unread_messages = threads.iter().fold(0u32, |sum, thread| sum.saturating_add(thread.unread_count));
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let take = usize::try_from(limit).unwrap_or(0);
        threads = threads.into_iter().skip(start).take(take).collect();
        ThreadPageResult { threads, total, unread_threads, unread_messages }
    }

    // -----------------------------------------------------------------------------------------
    // PGP keys (P12c1)
    // -----------------------------------------------------------------------------------------

    /// Key metadata for one account. The secret armor is never read back; the response carries
    /// fingerprints and flags, not key material.
    pub fn pgp_keys_list(&self, account: Option<&str>) -> Result<Vec<PgpKey>, OpError> {
        let account = self.requested_account(account, "pgp_keys_list")?;
        let store = self.require_store("pgp_keys_list")?;
        let keys = store.list_pgp_keys(&account.id).map_err(store_map::read_error)?;
        Ok(keys.into_iter().map(PgpKey::from).collect())
    }

    /// Parse, verify and store one armored key. A private key must be passphrase-protected and the
    /// passphrase must unlock it before anything is written; a public key needs no passphrase.
    pub fn pgp_key_import(&mut self, account: Option<&str>, armored: &str, passphrase: &[u8]) -> Result<PgpKey, OpError> {
        let account = self.requested_account(account, "pgp_key_import")?;
        let store = self.require_store("pgp_key_import")?;
        let parsed = crate::pgp::parse_import(armored, passphrase).map_err(|error| OpError::InvalidArgs(error.message().to_owned()))?;
        let id = store.allocate_pgp_key_id().map_err(store_map::write_error)?;
        let now = Utc::now().timestamp_millis();
        let insert = PgpKeyInsert {
            id: id.clone(),
            account_id: account.id.clone(),
            fingerprint: parsed.fingerprint.clone(),
            key_id: parsed.key_id.clone(),
            user_id: parsed.user_id.clone(),
            public_armor: parsed.public_armor,
            secret_armor: parsed.secret_armor,
            protected: parsed.protected,
            created_at: now,
            updated_at: now,
        };
        store.insert_pgp_key_limited(&insert, crate::pgp::MAX_PGP_KEYS_PER_ACCOUNT).map_err(store_map::write_error)?;
        self.bump_revision();
        Ok(PgpKey {
            id,
            account_id: account.id,
            fingerprint: parsed.fingerprint,
            key_id: parsed.key_id,
            user_id: parsed.user_id,
            protected: parsed.protected,
            has_secret: parsed.protected,
            created_at: now,
            updated_at: now,
        })
    }

    /// Delete one key of one account. The deleted metadata is returned, never the key material.
    pub fn pgp_key_delete(&mut self, account: Option<&str>, id: &str) -> Result<PgpKey, OpError> {
        let account = self.requested_account(account, "pgp_key_delete")?;
        let store = self.require_store("pgp_key_delete")?;
        let key = store
            .get_pgp_key(&account.id, id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::InvalidArgs(format!("unknown PGP key {id:?}; use an id returned by pgp_keys_list")))?;
        if !store.delete_pgp_key(&account.id, id).map_err(store_map::write_error)? {
            return Err(OpError::InvalidArgs(format!("unknown PGP key {id:?}; use an id returned by pgp_keys_list")));
        }
        self.bump_revision();
        Ok(PgpKey::from(key))
    }

    /// The public armor of one stored key, scoped to its account. A private key is exported as its
    /// public certificate only.
    pub fn pgp_key_export_public(&self, account: Option<&str>, id: &str) -> Result<PgpPublicKey, OpError> {
        let account = self.requested_account(account, "pgp_key_export_public")?;
        let store = self.require_store("pgp_key_export_public")?;
        let key = store
            .get_pgp_key(&account.id, id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::InvalidArgs(format!("unknown PGP key {id:?}; use an id returned by pgp_keys_list")))?;
        let public = store
            .export_pgp_public(&account.id, id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::InvalidArgs(format!("unknown PGP key {id:?}; use an id returned by pgp_keys_list")))?;
        Ok(PgpPublicKey { fingerprint: public.fingerprint, key_id: public.key_id, public_armor: public.public_armor, user_id: key.user_id })
    }

    /// Verify one retained PGP/MIME message against the account's stored keys.
    ///
    /// The bytes come from the schema v9 raw blob, never from a re-serialized body. A pre-v9 row
    /// reports `not_stored`: there is nothing to verify and this operation never fakes a result.
    pub fn message_pgp_verify(&self, message_id: &str, account: Option<&str>) -> Result<crate::pgp_mime::PgpVerifyResult, OpError> {
        let account = self.requested_account(account, "message_pgp_verify")?;
        let current = self.message_result(message_id)?.ok_or_else(|| unknown_message(message_id))?;
        if self.folders.iter().find(|folder| folder.id == current.summary.folder_id).is_none_or(|folder| folder.account_id != account.id) {
            return Err(OpError::InvalidArgs(format!("message {message_id:?} belongs to another account")));
        }
        let Some(store) = self.store.as_ref() else { return Ok(not_stored_verify()) };
        let Some(hash) = store.message_raw_blob_hash(message_id).map_err(store_map::read_error)? else { return Ok(not_stored_verify()) };
        let raw = store.blobs().get(&hash, crate::pgp_mime::MAX_RAW_MESSAGE_BYTES).map_err(store_map::blob_read_error)?;
        crate::pgp_mime::verify(store, &account.id, &raw)
    }

    /// Decrypt one retained PGP/MIME message with a per-use passphrase.
    ///
    /// The decrypted body is returned to this caller only. It never overwrites `body_text` or
    /// `body_html` and is never indexed: a later `message_get` still sees the encrypted body.
    pub fn message_pgp_decrypt(
        &self,
        message_id: &str,
        account: Option<&str>,
        passphrase: &[u8],
    ) -> Result<crate::pgp_mime::PgpDecryptResult, OpError> {
        let account = self.requested_account(account, "message_pgp_decrypt")?;
        let current = self.message_result(message_id)?.ok_or_else(|| unknown_message(message_id))?;
        if self.folders.iter().find(|folder| folder.id == current.summary.folder_id).is_none_or(|folder| folder.account_id != account.id) {
            return Err(OpError::InvalidArgs(format!("message {message_id:?} belongs to another account")));
        }
        let Some(store) = self.store.as_ref() else { return Ok(not_stored_decrypt()) };
        let Some(hash) = store.message_raw_blob_hash(message_id).map_err(store_map::read_error)? else { return Ok(not_stored_decrypt()) };
        let raw = store.blobs().get(&hash, crate::pgp_mime::MAX_RAW_MESSAGE_BYTES).map_err(store_map::blob_read_error)?;
        crate::pgp_mime::decrypt(store, &account.id, &raw, passphrase)
    }

    // -----------------------------------------------------------------------------------------
    // Saved searches (P9b)
    // -----------------------------------------------------------------------------------------

    /// Saved searches for one account with their computed counts. Counts are cached until the next
    /// engine write, so a frame loop does not rescan the index.
    pub fn saved_searches_list(&self, account: Option<&str>) -> Result<Vec<SavedSearch>, OpError> {
        let account = self.requested_account(account, "saved_searches_list")?;
        self.saved_search_views(&account.id)
    }

    /// The recent successful queries of one account, newest first.
    pub fn recent_searches(&self, account: Option<&str>) -> Result<Vec<String>, OpError> {
        let account = self.requested_account(account, "saved_searches_list")?;
        Ok(self.recent_searches.get(&account.id).cloned().unwrap_or_default())
    }

    /// Parse and resolve a query without running it. The UI uses this before recording a recent
    /// query, so a typo or a deleted label does not enter the history.
    pub fn validate_search_query(&self, query: &str, account: Option<&str>) -> Result<(), OpError> {
        let account = self.requested_account(account, "search")?;
        crate::ops::validate_search_query(self, query, Some(&account.id))
    }

    /// Re-read one account's saved-search metadata and recent list from the store. Used by the
    /// operation and the desktop before listing, so a second process's CRUD is visible without a
    /// restart; the computed counts are invalidated only when the rows actually changed.
    pub fn reload_saved_searches(&mut self, account: Option<&str>) -> Result<(), OpError> {
        let account = self.requested_account(account, "saved_searches_list")?;
        let Some(store) = &self.store else {
            self.recent_searches.remove(&account.id);
            return Ok(());
        };
        let rows = store.list_saved_searches(Some(&account.id)).map_err(store_map::read_error)?;
        let mut next = rows.into_iter().map(store_map::saved_search).collect::<Vec<_>>();
        next.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()).then(left.id.cmp(&right.id)));
        let current = self.saved_searches.iter().filter(|saved| saved.account_id == account.id).cloned().collect::<Vec<_>>();
        let changed = current.len() != next.len()
            || current
                .iter()
                .zip(next.iter())
                .any(|(left, right)| left.id != right.id || left.name != right.name || left.query != right.query || left.sort != right.sort);
        if changed {
            self.saved_searches.retain(|saved| saved.account_id != account.id);
            self.saved_searches.extend(next);
            self.saved_searches.sort_by(|left, right| {
                left.account_id.cmp(&right.account_id).then(left.name.to_lowercase().cmp(&right.name.to_lowercase())).then(left.id.cmp(&right.id))
            });
            *self.saved_counts.borrow_mut() = None;
        }
        let recent = store.recent_searches(&account.id).map_err(store_map::read_error)?;
        if recent.is_empty() {
            self.recent_searches.remove(&account.id);
        } else {
            self.recent_searches.insert(account.id, recent);
        }
        Ok(())
    }

    fn saved_search_views(&self, account_id: &str) -> Result<Vec<SavedSearch>, OpError> {
        let selected = self.saved_searches.iter().filter(|saved| saved.account_id == account_id).cloned().collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let revision = self.revision;
        let mut cache = self.saved_counts.borrow_mut();
        let fresh = matches!(cache.as_ref(), Some((cached, _)) if *cached == revision);
        if !fresh {
            *cache = Some((revision, HashMap::new()));
        }
        let counts = cache.as_mut().map(|(_, counts)| counts).ok_or_else(|| OpError::Failed("the saved-search cache is unavailable".to_owned()))?;
        let mut views = Vec::with_capacity(selected.len());
        for mut saved in selected {
            let needs_count = counts.get(&saved.id).is_none_or(|count| count.pending);
            if needs_count {
                let count = self.saved_search_count(&saved);
                counts.insert(saved.id.clone(), count);
            }
            if let Some(count) = counts.get(&saved.id) {
                saved.valid = count.valid;
                saved.error.clone_from(&count.error);
                saved.total = count.total;
                saved.unread = count.unread;
                saved.index_pending = count.pending;
            }
            views.push(saved);
        }
        Ok(views)
    }

    fn saved_search_count(&self, saved: &SavedSearch) -> SavedCount {
        let sort = match saved.sort.as_str() {
            "date" => crate::ops::SearchSort::Date,
            _ => crate::ops::SearchSort::Relevance,
        };
        let args = |unread_only: bool| crate::ops::SearchArgs {
            query: saved.query.clone(),
            account: Some(saved.account_id.clone()),
            folder: None,
            label: None,
            limit: Some(1),
            offset: Some(0),
            unread_only,
            with_attachments: false,
            sort: Some(sort),
        };
        let total = match crate::ops::execute_search(self, args(false)) {
            Ok(result) if result.index.is_none() => result.total,
            Ok(_) => return SavedCount { valid: true, error: None, total: 0, unread: 0, pending: true },
            Err(OpError::InvalidArgs(error)) => return SavedCount { valid: false, error: Some(error), total: 0, unread: 0, pending: false },
            Err(error) => return SavedCount { valid: false, error: Some(error.to_string()), total: 0, unread: 0, pending: false },
        };
        let unread = match crate::ops::execute_search(self, args(true)) {
            Ok(result) => result.total,
            Err(_) => 0,
        };
        SavedCount {
            valid: true,
            error: None,
            total: u32::try_from(total).unwrap_or(u32::MAX),
            unread: u32::try_from(unread).unwrap_or(u32::MAX),
            pending: false,
        }
    }

    pub fn create_saved_search(&mut self, account: Option<&str>, name: &str, query: &str, sort: Option<&str>) -> Result<SavedSearch, OpError> {
        let account = self.requested_account(account, "saved_search_create")?;
        let name = validate_saved_name(name)?;
        let query = validate_saved_query(self, &account.id, query)?;
        let sort = validate_saved_sort(sort)?;
        let count = self.saved_searches.iter().filter(|saved| saved.account_id == account.id).count();
        if u32::try_from(count).unwrap_or(u32::MAX) >= MAX_SAVED_SEARCHES {
            return Err(OpError::InvalidArgs(format!(
                "account {:?} already has {MAX_SAVED_SEARCHES} saved searches; delete one before adding another",
                account.id
            )));
        }
        if let Some(store) = &self.store {
            let stored = store.count_saved_searches(&account.id).map_err(store_map::read_error)?;
            if stored >= MAX_SAVED_SEARCHES {
                return Err(OpError::InvalidArgs(format!(
                    "account {:?} already has {MAX_SAVED_SEARCHES} saved searches; delete one before adding another",
                    account.id
                )));
            }
        }
        if self.saved_searches.iter().any(|saved| saved.account_id == account.id && saved.name.eq_ignore_ascii_case(&name)) {
            return Err(OpError::InvalidArgs(format!("a saved search named {name:?} already exists; names are unique ignoring case")));
        }
        let id = match &self.store {
            Some(store) => store.allocate_saved_search_id().map_err(store_map::write_error)?,
            None => format!("saved_{}", self.next_saved_search_seq),
        };
        let now = Utc::now().timestamp_millis();
        let saved = SavedSearch {
            id,
            account_id: account.id.clone(),
            name,
            query,
            sort,
            valid: true,
            error: None,
            total: 0,
            unread: 0,
            index_pending: false,
        };
        if let Some(store) = &self.store {
            store
                .create_saved_search_limited(
                    &postino_store::SavedSearch {
                        id: saved.id.clone(),
                        account_id: saved.account_id.clone(),
                        name: saved.name.clone(),
                        query: saved.query.clone(),
                        sort: saved.sort.clone(),
                        created_at: now,
                        updated_at: now,
                    },
                    MAX_SAVED_SEARCHES,
                )
                .map_err(store_map::write_error)?;
        } else {
            self.next_saved_search_seq = self.next_saved_search_seq.saturating_add(1);
        }
        let id = saved.id.clone();
        self.saved_searches.push(saved);
        self.bump_revision();
        self.saved_search_views(&account.id)?
            .into_iter()
            .find(|saved| saved.id == id)
            .ok_or_else(|| OpError::Failed("the new saved search disappeared".to_owned()))
    }

    pub fn update_saved_search(
        &mut self,
        account: Option<&str>,
        id: &str,
        name: Option<&str>,
        query: Option<&str>,
        sort: Option<&str>,
    ) -> Result<SavedSearch, OpError> {
        if name.is_none() && query.is_none() && sort.is_none() {
            return Err(OpError::InvalidArgs("provide name, query and/or sort".to_owned()));
        }
        let account = self.requested_account(account, "saved_search_update")?;
        if !self.saved_searches.iter().any(|saved| saved.id == id && saved.account_id == account.id) {
            self.reload_saved_searches(Some(&account.id))?;
        }
        let index = self.saved_searches.iter().position(|saved| saved.id == id).ok_or_else(|| unknown_saved_search(id))?;
        if self.saved_searches.get(index).is_none_or(|saved| saved.account_id != account.id) {
            return Err(unknown_saved_search(id));
        }
        let account_id = account.id.clone();
        let current_name = self.saved_searches.get(index).map(|saved| saved.name.clone()).unwrap_or_default();
        let resolved_name = match name {
            Some(name) => validate_saved_name(name)?,
            None => current_name,
        };
        if self.saved_searches.iter().any(|saved| saved.id != id && saved.account_id == account_id && saved.name.eq_ignore_ascii_case(&resolved_name))
        {
            return Err(OpError::InvalidArgs(format!("a saved search named {resolved_name:?} already exists; names are unique ignoring case")));
        }
        let resolved_query = match query {
            Some(query) => validate_saved_query(self, &account_id, query)?,
            None => self.saved_searches.get(index).map(|saved| saved.query.clone()).unwrap_or_default(),
        };
        let resolved_sort = match sort {
            Some(sort) => validate_saved_sort(Some(sort))?,
            None => self.saved_searches.get(index).map(|saved| saved.sort.clone()).unwrap_or_else(|| "relevance".to_owned()),
        };
        if let Some(store) = &self.store {
            store.update_saved_search(id, Some(&resolved_name), Some(&resolved_query), Some(&resolved_sort)).map_err(store_map::write_error)?;
        }
        if let Some(saved) = self.saved_searches.get_mut(index) {
            saved.name = resolved_name;
            saved.query = resolved_query;
            saved.sort = resolved_sort;
        }
        self.bump_revision();
        self.saved_search_views(&account_id)?.into_iter().find(|saved| saved.id == id).ok_or_else(|| unknown_saved_search(id))
    }

    pub fn delete_saved_search(&mut self, account: Option<&str>, id: &str) -> Result<SavedSearch, OpError> {
        let account = self.requested_account(account, "saved_search_delete")?;
        if !self.saved_searches.iter().any(|saved| saved.id == id && saved.account_id == account.id) {
            self.reload_saved_searches(Some(&account.id))?;
        }
        let index =
            self.saved_searches.iter().position(|saved| saved.id == id && saved.account_id == account.id).ok_or_else(|| unknown_saved_search(id))?;
        if let Some(store) = &self.store {
            store.delete_saved_search(id).map_err(store_map::write_error)?;
        }
        let removed = self.saved_searches.remove(index);
        self.bump_revision();
        Ok(removed)
    }

    /// Remember one successful query. Duplicates move to the front; the list stays at eight entries.
    pub fn record_recent_search(&mut self, account: Option<&str>, query: &str) -> Result<(), OpError> {
        let account = self.requested_account(account, "saved_searches_list")?;
        let query = query.trim();
        if query.is_empty() || crate::search::parse(query).is_err() {
            return Ok(());
        }
        let entry = self.recent_searches.entry(account.id.clone()).or_default();
        entry.retain(|candidate| candidate != query);
        entry.insert(0, query.to_owned());
        entry.truncate(MAX_RECENT_SEARCHES);
        let recent = entry.clone();
        if let Some(store) = &self.store {
            store.set_recent_searches(&account.id, &recent).map_err(store_map::write_error)?;
        }
        Ok(())
    }

    /// Remove one account's recent list. Returns whether anything was stored.
    pub fn clear_recent_searches(&mut self, account: Option<&str>) -> Result<bool, OpError> {
        let account = self.requested_account(account, "saved_search_recent_clear")?;
        let had = self.recent_searches.remove(&account.id).is_some_and(|recent| !recent.is_empty());
        if let Some(store) = &self.store {
            let stored = store.clear_recent_searches(&account.id).map_err(store_map::write_error)?;
            return Ok(had || stored);
        }
        Ok(had)
    }

    /// Every configured account with its IMAP server and last sync status. The demo account has
    /// neither, and no view ever contains a credential. A damaged status is reported as `null`; use
    /// [`Engine::account_views_report`] to also receive the warning.
    pub fn account_views(&self) -> Result<Vec<AccountView>, OpError> {
        self.account_views_report().map(|(views, _warnings)| views)
    }

    /// Every account view plus one warning per damaged `sync_status` value.
    ///
    /// Status is a cache: a malformed JSON value makes that account look never-synced and produces a
    /// warning, but it never fails the listing. Removal uses the same tolerant view, so a broken
    /// status cannot block cleanup.
    pub fn account_views_report(&self) -> Result<(Vec<AccountView>, Vec<String>), OpError> {
        let mut views = Vec::with_capacity(self.accounts.len());
        let mut warnings = Vec::new();
        for account in &self.accounts {
            let (view, warning) = self.account_view_with_warning(account)?;
            views.push(view);
            if let Some(warning) = warning {
                warnings.push(warning);
            }
        }
        Ok((views, warnings))
    }

    /// One account with its server settings and persisted sync status. A damaged status is `None`.
    pub fn account_view(&self, account: &Account) -> Result<AccountView, OpError> {
        self.account_view_with_warning(account).map(|(view, _warning)| view)
    }

    fn account_view_with_warning(&self, account: &Account) -> Result<(AccountView, Option<String>), OpError> {
        let Some(store) = &self.store else {
            return Ok((AccountView { account: account.clone(), server: None, sync: None }, None));
        };
        let server = store.get_account_server(&account.id).map_err(store_map::read_error)?.map(store_map::server_settings);
        let (sync, warning) = match postino_sync::read_status_for(store, &account.id) {
            Ok(status) => (status, None),
            Err(error) => (
                None,
                Some(format!(
                    "the sync status for account {:?} is unreadable ({error}); showing it as never synced. Run a sync or remove the stale sync_status key.",
                    account.id
                )),
            ),
        };
        Ok((AccountView { account: account.clone(), server, sync }, warning))
    }

    /// Validate everything `account_add` can check without the password or the network.
    ///
    /// The CLI calls this before reading the secret, so an unknown provider, a bad address or name,
    /// a missing store/vault or a duplicate account fails without consuming stdin or touching the
    /// keyring. It performs no I/O.
    pub fn check_account_request(&self, request: &AccountRequest) -> Result<(), OpError> {
        self.resolve_account(request).map(|_| ())
    }

    /// Shared validation for [`Engine::check_account_request`] and [`Engine::add_account`].
    fn resolve_account(&self, request: &AccountRequest) -> Result<ResolvedAccount, OpError> {
        let email = validate_account_email(&request.email)?;
        let name = validate_account_name(request.name.as_deref(), &email)?;
        let (host, port, tls) = resolve_server(request.provider, request.host.as_deref(), request.port, request.tls)?;
        let smtp = resolve_smtp(
            request.provider,
            request.smtp_host.as_deref(),
            request.smtp_port,
            request.smtp_tls,
            request.smtp_username.as_deref(),
            &email,
        )?;
        #[cfg(debug_assertions)]
        let (host, port, tls, smtp) = match &self.test_oauth_hosts {
            Some((imap_host, imap_port, smtp_host)) if matches!(request.provider, Provider::Gmail) => (
                imap_host.clone(),
                *imap_port,
                MailTls::Implicit,
                smtp_host.as_ref().map(|(host, port)| SmtpSettings {
                    host: host.clone(),
                    port: *port,
                    tls: MailTls::Implicit,
                    username: email.clone(),
                }),
            ),
            _ => (host, port, tls, smtp),
        };
        self.require_store("account_add")?;
        self.require_vault("account_add")?;
        if let Some(existing) = self.accounts.iter().find(|account| account.email.eq_ignore_ascii_case(&email)) {
            return Err(OpError::InvalidArgs(format!(
                "an account for {email:?} already exists ({:?}); remove it with account_remove before adding it again",
                existing.id
            )));
        }
        let id = account_id(&email);
        if self.accounts.iter().any(|account| account.id == id) {
            return Err(OpError::InvalidArgs(format!(
                "account id {id:?} is already taken by a different address; remove that account before adding {email:?}"
            )));
        }
        Ok(ResolvedAccount { id, provider: request.provider, email, name, host, port, tls, smtp })
    }

    /// Add an IMAP account: validate, prove the login on the real server, store the credential,
    /// then write the account, its server row and its folder tree in one store transaction.
    ///
    /// Nothing is written before the server accepts the login. The credential is written **first**
    /// on purpose: a crash between the two durable writes leaves an unreferenced keyring entry,
    /// which a retry with the same address reuses, instead of an account whose password was never
    /// stored and can therefore never sync. If the store transaction fails, the credential is
    /// deleted again before the error is returned; memory is untouched either way.
    pub fn add_account(&mut self, setup: AccountSetup) -> Result<AccountView, OpError> {
        let request = AccountRequest {
            provider: setup.provider,
            email: setup.email.clone(),
            name: setup.name.clone(),
            host: setup.host.clone(),
            port: setup.port,
            tls: setup.tls,
            smtp_host: setup.smtp.as_ref().map(|smtp| smtp.host.clone()),
            smtp_port: setup.smtp.as_ref().map(|smtp| smtp.port),
            smtp_tls: setup.smtp.as_ref().map(|smtp| smtp.tls),
            smtp_username: setup.smtp.as_ref().map(|smtp| smtp.username.clone()),
        };
        let resolved = self.resolve_account(&request)?;
        let oauth = setup.oauth.clone();
        match &oauth {
            Some(grant) => {
                grant.validate().map_err(|reason| OpError::InvalidArgs(format!("the OAuth authorization was refused: {reason}")))?;
                if !setup.password.is_empty() {
                    return Err(OpError::InvalidArgs(
                        "an OAuth account must not also carry an app password; add it either with OAuth or with an app password, never both"
                            .to_owned(),
                    ));
                }
                if self.oauth_provider_for(resolved.provider.as_str()).is_none() {
                    return Err(OpError::InvalidArgs(format!(
                        "provider {} has no OAuth authorization in this build; add it with an app password or choose Gmail",
                        resolved.provider.as_str()
                    )));
                }
            }
            None => validate_account_password(&setup.password)?,
        }
        let store = self.require_store("account_add")?;
        let vault = self.require_vault("account_add")?;
        let ResolvedAccount { id, provider, email, name, host, port, tls, smtp } = resolved;

        let mut extra_roots = self.extra_roots.clone();
        extra_roots.extend(setup.extra_roots.clone());
        // The grant is probed before anything is written. When the caller still holds the token the
        // code exchange produced, use it: refreshing immediately spends a second token request and
        // rotates a refresh token the provider just issued. Only an absent or stale hint refreshes,
        // so an expired or revoked grant still fails with an empty keyring and an empty store.
        let hint = setup.oauth_access.clone();
        let (oauth_grant, oauth_token, oauth_cached) = match oauth {
            Some(grant) => match hint.filter(|access| access.is_fresh_now()) {
                Some(access) => {
                    let cached = crate::oauth::CachedAccess::from_hint(&access);
                    (Some(grant), Some(Zeroizing::new(access.token().to_owned())), Some(cached))
                }
                None => {
                    let seam = self
                        .oauth_provider_for(provider.as_str())
                        .ok_or_else(|| OpError::InvalidArgs(format!("provider {} has no OAuth authorization in this build", provider.as_str())))?;
                    let refreshed = self.refresh_grant(seam, &grant, &id, "adding the account")?;
                    let access = refreshed.access();
                    let cached = refreshed.cached();
                    let rotated = refreshed.rotated.unwrap_or(grant);
                    (Some(rotated), Some(access), Some(cached))
                }
            },
            None => (None, None, None),
        };
        let imap_credential = match &oauth_token {
            Some(token) => crate::oauth::Credential::OAuth(token.clone()),
            None => crate::oauth::Credential::Password(setup.password.clone()),
        };
        let config = crate::oauth::imap_config_for(host.clone(), port, tls, email.clone(), imap_credential, extra_roots.clone());
        let probe = postino_sync::probe(&config).map_err(|error| {
            account_error(
                error,
                provider,
                &id,
                &host,
                "checking the connection",
                if oauth_grant.is_some() {
                    "Nothing was written; check that IMAP is enabled for the account and that the authorization was granted, then retry."
                } else {
                    "Nothing was written; check the address, host, password and TLS mode and retry."
                },
            )
        })?;
        if probe.folders.iter().all(|folder| !folder.is_selectable()) {
            return Err(OpError::Failed(format!(
                "the server {host} listed no selectable folder; check that IMAP is enabled for this account. Nothing was written."
            )));
        }
        let (planned, _warnings) = postino_sync::plan_folders(&id, &probe.folders);
        if planned.is_empty() {
            return Err(OpError::Failed(format!("the server {host} listed no usable folder; nothing was written")));
        }

        // The submission endpoint is probed before anything is written, exactly like IMAP. This is
        // the difference between a queue that fails on every attempt and a queue that works.
        let mut smtp_password = None;
        if let Some(smtp) = &smtp {
            let credential = match &oauth_token {
                Some(token) => crate::oauth::Credential::OAuth(token.clone()),
                None => {
                    let password = match setup.smtp.as_ref().and_then(|setup| setup.password.as_ref()) {
                        Some(password) => password.clone(),
                        None => setup.password.clone(),
                    };
                    validate_account_password(&password)?;
                    crate::oauth::Credential::Password(password)
                }
            };
            let smtp_config =
                crate::oauth::smtp_config(smtp.host.clone(), smtp.port, smtp.tls, smtp.username.clone(), credential, extra_roots.clone());
            postino_sync::verify_smtp(smtp_config).map_err(|error| {
                OpError::Auth(format!(
                    "the SMTP server {}:{} did not accept the credential ({}). Nothing was written; {}",
                    smtp.host,
                    smtp.port,
                    smtp_failure_reason(&error),
                    if oauth_grant.is_some() {
                        "check that the authorization includes the mail scope and retry."
                    } else {
                        "check the SMTP password, host, port and TLS mode."
                    }
                ))
            })?;
            smtp_password = if oauth_grant.is_some() { None } else { setup.smtp.as_ref().and_then(|setup| setup.password.clone()) };
        }

        let created_at = Utc::now().timestamp_millis();
        let account_row = postino_store::Account {
            id: id.clone(),
            name,
            email: email.clone(),
            provider: provider.as_str().to_owned(),
            signature: String::new(),
            created_at,
        };
        let server_row = postino_store::AccountServer {
            account_id: id.clone(),
            host: host.clone(),
            port,
            tls: store_map::account_tls(tls),
            username: email.clone(),
            created_at,
            smtp: smtp.as_ref().map(|smtp| postino_store::SmtpServer {
                host: smtp.host.clone(),
                port: smtp.port,
                tls: store_map::account_tls(smtp.tls),
                username: smtp.username.clone(),
            }),
        };
        let folder_rows = planned.into_iter().map(|folder| folder.row).collect::<Vec<_>>();

        // The credential is written **first** on purpose, then the account rows. An OAuth account
        // stores one grant and no password at all; a stale password entry is removed so a later read
        // cannot prefer it over the grant.
        match &oauth_grant {
            Some(grant) => {
                if let Err(error) = vault.store_oauth_grant(&id, grant) {
                    return Err(OpError::Failed(format!("the OAuth grant could not be stored in the OS keyring: {error}. Nothing was written.")));
                }
                let _ = vault.delete_imap_password(&id);
                let _ = vault.delete_smtp_password(&id);
            }
            None => {
                if let Err(error) = vault.store_imap_password(&id, setup.password.as_bytes()) {
                    return Err(OpError::Failed(format!("the IMAP credential could not be stored in the OS keyring: {error}. Nothing was written.")));
                }
                let smtp_credential = match smtp_password.as_ref() {
                    Some(password) => vault.store_smtp_password(&id, password.as_bytes()).map_err(|error| {
                        let cleanup = vault.delete_imap_password(&id);
                        let mut sentence = format!("the SMTP credential could not be stored in the OS keyring: {error}.");
                        if cleanup.is_ok() {
                            sentence.push_str(" The IMAP credential was removed again; nothing was written.");
                        } else {
                            sentence.push_str(" The IMAP credential could not be removed again; delete it in the OS credential manager.");
                        }
                        OpError::Failed(sentence)
                    }),
                    // A separate SMTP secret that used to exist would shadow the IMAP fallback, so remove
                    // it. Deleting a missing entry is a no-op.
                    None => vault.delete_smtp_password(&id).map_err(|error| {
                        let _ = vault.delete_imap_password(&id);
                        OpError::Failed(format!(
                            "the old SMTP credential could not be removed from the OS keyring ({error}); the IMAP credential was removed again and nothing was written."
                        ))
                    }),
                };
                smtp_credential?;
            }
        }
        if let Err(error) = store.save_account(&account_row, &server_row, &folder_rows) {
            let imap_cleanup = vault.delete_imap_password(&id);
            let smtp_cleanup = vault.delete_smtp_password(&id);
            let oauth_cleanup = vault.delete_oauth_grant(&id);
            let outcome = match (imap_cleanup, smtp_cleanup, oauth_cleanup) {
                (Ok(()), Ok(()), Ok(())) => "The credentials were removed again and nothing was written.".to_owned(),
                _ => "One or more credentials could not be removed again from the OS keyring; a retry with the same address overwrites them, or delete them in the OS credential manager.".to_owned(),
            };
            return Err(OpError::Failed(format!("the local store refused the account: {error}. {outcome}")));
        }

        // Durable writes are done; only now does memory change. A re-add with a fresh password also
        // clears a previous authentication halt: the user has just proved the credential works.
        let _ = clear_halt(store, &id);
        let account = store_map::account(account_row);
        if let Some(cached) = oauth_cached {
            self.oauth_access.borrow_mut().insert(id.clone(), cached);
        }
        self.accounts.push(account.clone());
        self.folders.extend(folder_rows.iter().cloned().map(store_map::folder));
        self.recount();
        self.account_view(&account)
    }

    /// Change an account's SMTP endpoint. The IMAP settings, cached mail and labels are untouched.
    ///
    /// Setting an endpoint probes it with the new password when one is supplied, otherwise with the
    /// existing SMTP secret or the IMAP credential. Clearing it removes the endpoint and the separate
    /// SMTP secret. A failed probe writes nothing.
    pub fn update_account(&mut self, request: AccountUpdateRequest) -> Result<AccountView, OpError> {
        let account = self.find_account(&request.id)?.clone();
        let store = self.require_store("account_update")?;
        let vault = self.require_vault("account_update")?;
        let mut server = store
            .get_account_server(&account.id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::Failed(format!("account {:?} has no server configuration; remove it and add it again", account.id)))?;
        if request.clear_smtp {
            // Every endpoint field is checked: a direct operation call must not silently ignore a
            // port or a login that was meant for the endpoint being removed.
            if request.smtp_host.is_some()
                || request.smtp_port.is_some()
                || request.smtp_tls.is_some()
                || request.smtp_username.is_some()
                || request.smtp_password.is_some()
            {
                return Err(OpError::InvalidArgs("clear_smtp cannot be combined with a new SMTP endpoint, login or password".to_owned()));
            }
            server.smtp = None;
            store.upsert_account_server(&server).map_err(store_map::write_error)?;
            let cleanup = vault.delete_smtp_password(&account.id);
            let _ = clear_halt(store, &account.id);
            if let Err(error) = cleanup {
                return Err(OpError::Failed(format!(
                    "the SMTP endpoint was removed, but its credential could not be deleted from the OS keyring: {error}"
                )));
            }
            return self.account_view(&account);
        }
        if request.smtp_host.is_none() && request.smtp_port.is_none() && request.smtp_tls.is_none() && request.smtp_username.is_none() {
            return Err(OpError::InvalidArgs("provide the SMTP endpoint fields or clear_smtp: true".to_owned()));
        }
        let provider = Provider::parse(&account.provider).unwrap_or(Provider::Custom);
        let smtp = resolve_smtp(
            provider,
            request.smtp_host.as_deref(),
            request.smtp_port,
            request.smtp_tls,
            request.smtp_username.as_deref(),
            &account.email,
        )?
        .ok_or_else(|| OpError::InvalidArgs("provide smtp_host, smtp_port and smtp_tls together".to_owned()))?;
        let new_password = request.smtp_password.is_some();
        let oauth = self.oauth_grant(&account.id)?;
        if new_password && oauth.is_some() {
            return Err(OpError::InvalidArgs(format!(
                "account {:?} authorizes with OAuth; a separate SMTP password cannot be added to it. Remove the account and add it again with an app password if you want password submission.",
                account.id
            )));
        }
        let password = match request.smtp_password {
            Some(password) => {
                validate_account_password(&password)?;
                Some(password)
            }
            None if oauth.is_some() => None,
            None => {
                let bytes = vault
                    .load_smtp_password(&account.id)
                    .map_err(|error| OpError::Failed(format!("the OS keyring could not provide the SMTP credential: {error}")))?
                    .or_else(|| vault.load_imap_password(&account.id).ok().flatten())
                    .ok_or_else(|| OpError::Failed(format!("account {:?} has no SMTP or IMAP credential; supply a new SMTP password", account.id)))?;
                let text = std::str::from_utf8(bytes.as_slice())
                    .map_err(|_| OpError::Failed(format!("the stored SMTP credential for account {:?} is not valid UTF-8", account.id)))?;
                Some(Zeroizing::new(text.to_owned()))
            }
        };
        let credential = match &password {
            Some(password) => crate::oauth::Credential::Password(password.clone()),
            None => crate::oauth::Credential::OAuth(self.oauth_access(&account.id, "updating the SMTP endpoint")?),
        };
        let config = crate::oauth::smtp_config(smtp.host.clone(), smtp.port, smtp.tls, smtp.username.clone(), credential, self.extra_roots.clone());
        postino_sync::verify_smtp(config).map_err(|error| {
            OpError::Auth(format!(
                "the SMTP server {}:{} did not accept the credential ({}). Nothing was written; check the SMTP password, host, port and TLS mode.",
                smtp.host,
                smtp.port,
                smtp_failure_reason(&error)
            ))
        })?;
        // Keep the previous secret so a failed row write can put it back: the endpoint must never
        // end up paired with a password the server has not accepted.
        let previous_secret = if new_password { vault.load_smtp_password(&account.id).ok().flatten().map(|bytes| bytes.to_vec()) } else { None };
        if new_password
            && let Some(password) = &password
            && let Err(error) = vault.store_smtp_password(&account.id, password.as_bytes())
        {
            return Err(OpError::Failed(format!(
                "the SMTP credential could not be stored in the OS keyring: {error}. The endpoint was not changed."
            )));
        }
        let previous = server.smtp.clone();
        server.smtp = Some(postino_store::SmtpServer {
            host: smtp.host.clone(),
            port: smtp.port,
            tls: store_map::account_tls(smtp.tls),
            username: smtp.username.clone(),
        });
        if let Err(error) = store.upsert_account_server(&server) {
            server.smtp = previous;
            let _ = store.upsert_account_server(&server);
            let detail = if new_password {
                let rollback = match previous_secret {
                    Some(secret) => vault.store_smtp_password(&account.id, &secret),
                    None => vault.delete_smtp_password(&account.id),
                };
                if rollback.is_ok() {
                    " The previous credential was restored."
                } else {
                    " The previous credential could not be restored; set it again from the settings."
                }
            } else {
                ""
            };
            return Err(OpError::Failed(format!("{}{detail}", store_map::write_error(error))));
        }
        let _ = clear_halt(store, &account.id);
        self.account_view(&account)
    }

    /// Log in with the stored credential, list folders and return the count and capabilities.    ///
    /// Read-only: nothing is written, no mailbox is selected and no message is fetched. A broken
    /// credential is an [`OpError::Auth`] that names the provider and the likely cause and leaves the
    /// account and its cache exactly as they were.
    pub fn test_account(&self, id: &str) -> Result<crate::ops::AccountTest, OpError> {
        let account = self.find_account(id)?;
        let store = self.require_store("account_test")?;
        let server = store.get_account_server(&account.id).map_err(store_map::read_error)?.ok_or_else(|| {
            OpError::Failed(format!("account {:?} has no IMAP server configuration; remove it with account_remove and add it again", account.id))
        })?;
        let credential = self.credential(&account.id, "account_test")?;
        let config = crate::oauth::imap_config(&server, credential, self.extra_roots.clone());
        let provider = Provider::parse(&account.provider).unwrap_or(Provider::Custom);
        let probe = postino_sync::probe(&config).map_err(|error| {
            account_error(error, provider, &account.id, &server.host, "checking the connection", "The account and its cached mail are unchanged.")
        })?;
        // A configured submission endpoint is checked too: after an SMTP password change "credentials
        // valid" must not be true while sending fails. Fixed sentences only: this server holds the
        // password and may echo it.
        if let Some(smtp) = server.smtp.clone() {
            let smtp_credential = self.smtp_credential(&account.id, "account_test")?;
            let smtp_config = crate::oauth::smtp_config(
                smtp.host.clone(),
                smtp.port,
                store_map::mail_tls(smtp.tls),
                smtp.username.clone(),
                smtp_credential,
                self.extra_roots.clone(),
            );
            if let Err(error) = postino_sync::verify_smtp(smtp_config) {
                let reason = smtp_failure_reason(&error);
                let sentence = format!(
                    "the SMTP server {}:{} did not accept the submission credential ({reason}). The account and its cached mail are unchanged.",
                    smtp.host, smtp.port
                );
                return Err(if error.is_authentication() { OpError::Auth(sentence) } else { OpError::Network(sentence) });
            }
        }
        let folders = u32::try_from(probe.folders.iter().filter(|folder| folder.is_selectable()).count()).unwrap_or(u32::MAX);
        Ok(crate::ops::AccountTest { account: account.id.clone(), folders, capabilities: probe.capabilities })
    }

    /// Remove an account, its keyring credential and its local cache.
    ///
    /// Removal is idempotent and truthful: once the local account rows are gone the call succeeds,
    /// even when Google could not be told to revoke the grant or the OS keyring refused to delete a
    /// credential. Those two cases add a bounded warning, because the secret is then unreferenced or
    /// the grant stays valid until it expires. A retry after a successful removal reports
    /// `removed: false` instead of "unknown account".
    ///
    /// The order is deliberate: the store rows go first, the credential second. The only failure
    /// window therefore leaves an orphan keyring entry, never cached mail whose password is gone.
    pub fn remove_account(&mut self, id: &str, confirm: bool) -> Result<crate::ops::AccountRemove, OpError> {
        if !confirm {
            return Err(OpError::InvalidArgs(format!(
                "removing account {id:?} also deletes its cached mail and its IMAP credential; pass confirm: true to proceed"
            )));
        }
        let store = self.require_store("account_remove")?;
        let vault = self.require_vault("account_remove")?;
        let Some(index) = self.accounts.iter().position(|account| account.id == id) else {
            // A retry after a removal that already deleted the local row is a success, not "unknown
            // account". A row that is in the store but not in memory is a different problem.
            if store.list_accounts().map_err(store_map::read_error)?.iter().any(|row| row.id == id) {
                return Err(OpError::Failed(format!(
                    "account {id:?} is in the local store but was not loaded by the engine; restart Postino and retry"
                )));
            }
            return Ok(crate::ops::AccountRemove { account: id.to_owned(), removed: false, warnings: Vec::new() });
        };
        let account = self.accounts.get(index).cloned().ok_or_else(|| unknown_account(id))?;
        // The row going away is the commit point: if it was already absent, finish the cleanup below.
        let _ = store.delete_account(&account.id).map_err(store_map::write_error)?;
        // Revoke before deleting the grant: revocation needs the refresh token, and the provider is
        // the one from the account row. A revocation failure is a warning, never a removal error.
        let mut warnings = Vec::new();
        if let Some(warning) = self.revoke_oauth(&account.id) {
            warnings.push(warning);
        }
        let imap_credential = vault.delete_imap_password(&account.id);
        let smtp_credential = vault.delete_smtp_password(&account.id);
        let oauth_credential = vault.delete_oauth_grant(&account.id);
        self.oauth_access.borrow_mut().remove(&account.id);
        let folder_ids =
            self.folders.iter().filter(|folder| folder.account_id == account.id).map(|folder| folder.id.clone()).collect::<BTreeSet<_>>();
        self.accounts.retain(|candidate| candidate.id != account.id);
        self.folders.retain(|folder| folder.account_id != account.id);
        self.messages.retain(|message| !folder_ids.contains(&message.folder_id));
        self.saved_searches.retain(|saved| saved.account_id != account.id);
        self.rules.retain(|rule| rule.account_id != account.id);
        self.rule_runs.retain(|run| run.account_id != account.id);
        self.rule_run_actions.retain(|run_id, _| self.rule_runs.iter().any(|run| run.id == *run_id));
        self.demo_rules_applied.clear();
        self.recent_searches.remove(&account.id);
        *self.saved_counts.borrow_mut() = None;
        self.rebuild_message_index();
        let live = self.messages.iter().map(|message| message.id.clone()).collect::<BTreeSet<_>>();
        self.demo_messages.retain(|id, _| live.contains(id));
        self.reply_links.retain(|draft_id, _| live.contains(draft_id));
        self.draft_snapshots.retain(|draft_id, _| live.contains(draft_id));
        self.bump_revision();
        self.recount();
        if !(imap_credential.is_ok() && smtp_credential.is_ok() && oauth_credential.is_ok()) {
            warnings.push(format!(
                "account {:?} and its cached mail were removed, but the OS keyring refused to delete a credential (IMAP: {}; SMTP: {}; OAuth: {}). The credential is now unreferenced; adding this address again overwrites it, or delete it in the OS credential manager.",
                account.id,
                credential_outcome(&imap_credential),
                credential_outcome(&smtp_credential),
                credential_outcome(&oauth_credential)
            ));
        }
        Ok(crate::ops::AccountRemove { account: account.id, removed: true, warnings })
    }

    pub(crate) fn load_smtp_password(&self, account_id: &str, operation: &str) -> Result<Zeroizing<String>, OpError> {
        let vault = self.require_vault(operation)?;
        let bytes = vault
            .load_smtp_password(account_id)
            .map_err(|error| OpError::Failed(format!("the OS keyring could not provide the SMTP credential for account {account_id:?}: {error}")))?
            .or_else(|| vault.load_imap_password(account_id).ok().flatten());
        let Some(bytes) = bytes else {
            return Err(OpError::Failed(format!(
                "account {account_id:?} has no SMTP or IMAP credential in the OS keyring; add the account again with its password"
            )));
        };
        let text = std::str::from_utf8(bytes.as_slice()).map_err(|_| {
            OpError::Failed(format!("the stored SMTP credential for account {account_id:?} is not valid UTF-8; add the account again"))
        })?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    /// Run one IMAP sync for one account (or the only account) and refresh memory from the store.
    ///
    /// A failed run returns the typed error after reloading the cache, so memory shows every message
    /// that was committed before the failure and never loses one. Labels are reloaded from the store
    /// and are preserved by the reconciler, so a sync does not detach them.
    pub fn sync_now(&mut self, account: Option<&str>) -> Result<crate::ops::SyncReport, OpError> {
        self.sync_now_with_cancel(account, &AtomicBool::new(false))
    }

    /// The same sync with a caller-owned cancellation flag. The daemon registers the flag with the
    /// request id before dispatch, so a `cancel` frame can stop a long folder or body transfer. A
    /// cancelled sync keeps every message committed before the stop, exactly like the desktop
    /// worker; the caller maps [`postino_sync::SyncError::Cancelled`] to a readable sentence.
    pub fn sync_now_with_cancel(&mut self, account: Option<&str>, cancel: &AtomicBool) -> Result<crate::ops::SyncReport, OpError> {
        self.sync_now_with_cancel_limits(account, cancel, postino_sync::SyncLimits::default())
    }

    /// A sync with a per-run message ceiling. The daemon uses a chunk ceiling and calls this again
    /// while the cursor still has mail, so the engine mutex is released between chunks and the
    /// outbox/index can run during a long initial sync.
    pub fn sync_now_with_cancel_limits(
        &mut self,
        account: Option<&str>,
        cancel: &AtomicBool,
        limits: postino_sync::SyncLimits,
    ) -> Result<crate::ops::SyncReport, OpError> {
        let account = self.requested_account(account, "sync_now")?;
        let store = self.require_store("sync_now")?;
        let server = store.get_account_server(&account.id).map_err(store_map::read_error)?.ok_or_else(|| {
            OpError::Failed(format!("account {:?} has no IMAP server configuration; remove it with account_remove and add it again", account.id))
        })?;
        let row = store
            .get_account(&account.id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::Failed(format!("account {:?} is not in the local store", account.id)))?;
        let credential = self.credential(&account.id, "sync_now")?;
        let config = crate::oauth::imap_config(&server, credential, self.extra_roots.clone());
        let provider = Provider::parse(&account.provider).unwrap_or(Provider::Custom);
        let result = postino_sync::sync_account(store, &row, &config, &limits, cancel).map_err(|error| {
            account_error(
                error,
                provider,
                &account.id,
                &server.host,
                "syncing",
                "Messages committed before the stop are kept; the next run resumes from the last committed message.",
            )
        });
        let reload = self.reload_mailbox();
        // P9c runs after a sync, on received mail only. A rule failure is journaled and returned by
        // `rules_run`; it must never turn a successful sync into a failed one.
        if self.rules.iter().any(|rule| rule.account_id == account.id && rule.enabled) {
            let _ = self.run_rules(Some(&account.id), crate::model::RuleRunScope::New);
        }
        finish_sync(result, reload).map(crate::ops::SyncReport::from)
    }

    /// The persisted status of one account, or of every account.
    pub fn sync_statuses(&self, account: Option<&str>) -> Result<Vec<crate::ops::SyncStatusView>, OpError> {
        let store = self.require_store("sync_status")?;
        let accounts = match account {
            Some(id) => vec![self.find_account(id)?.clone()],
            None => self.accounts.clone(),
        };
        accounts
            .iter()
            .map(|account| {
                let status = postino_sync::read_status_for(store, &account.id).map_err(store_map::read_error)?;
                Ok(crate::ops::SyncStatusView { account: account.id.clone(), email: account.email.clone(), status })
            })
            .collect()
    }

    pub(crate) fn find_account(&self, id: &str) -> Result<&Account, OpError> {
        self.accounts.iter().find(|account| account.id == id).ok_or_else(|| unknown_account(id))
    }

    /// Resolve an optional account argument: one id if given, the only account if there is one, and
    /// a readable choice when there are several.
    fn requested_account(&self, id: Option<&str>, operation: &str) -> Result<Account, OpError> {
        if let Some(id) = id {
            return self.find_account(id).cloned();
        }
        match self.accounts.as_slice() {
            [] => Err(OpError::Failed(format!("no account is configured; add one with account_add before {operation}"))),
            [account] => Ok(account.clone()),
            accounts => Err(OpError::InvalidArgs(format!(
                "the mailbox has {} accounts; pass `account` with one of {}",
                accounts.len(),
                accounts.iter().map(|account| format!("{:?}", account.id)).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// The resolved account id, for operations whose result names the account it touched.
    pub(crate) fn requested_account_id(&self, id: Option<&str>, operation: &str) -> Result<String, OpError> {
        self.requested_account(id, operation).map(|account| account.id)
    }

    pub(crate) fn load_password(&self, account_id: &str, operation: &str) -> Result<Zeroizing<String>, OpError> {
        let vault = self.require_vault(operation)?;
        let bytes = vault
            .load_imap_password(account_id)
            .map_err(|error| OpError::Failed(format!("the OS keyring could not provide the IMAP credential for account {account_id:?}: {error}")))?;
        let Some(bytes) = bytes else {
            return Err(OpError::Failed(format!(
                "account {account_id:?} has no IMAP credential in the OS keyring; remove it with account_remove and add it again"
            )));
        };
        let text = std::str::from_utf8(bytes.as_slice()).map_err(|_| {
            OpError::Failed(format!("the stored IMAP credential for account {account_id:?} is not valid UTF-8; remove the account and add it again"))
        })?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    fn require_store(&self, operation: &str) -> Result<&Store, OpError> {
        match &self.store {
            Some(store) => Ok(store),
            None if self.demo => Err(OpError::Failed(format!(
                "the demo mailbox is in memory and has no IMAP account or local cache; {operation} needs a local store (postino --store <path>)"
            ))),
            None => Err(OpError::Failed(format!("this engine has no local store; {operation} needs one (postino --store <path>)"))),
        }
    }

    /// The store connection when this engine has one. Read paths that also work on the demo use it
    /// to choose SQL over the in-memory fallback.
    pub(crate) fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// The store, or a readable explanation when a read-only store has never built the conversation
    /// cache. The caller is a thread read; it must not silently show an empty mailbox.
    fn require_thread_cache(&self) -> Result<&Store, OpError> {
        let store = self.require_store("threads_list")?;
        if store.is_read_only() && store.thread_cache_count().map_err(store_map::read_error)? == 0 {
            return Err(OpError::Failed(
                "the conversation cache is empty; open this store read-write once to build it (any mail write or search_reindex does)".to_owned(),
            ));
        }
        Ok(store)
    }

    /// The store, or the same readable demo/empty explanation the write operations use.
    pub(crate) fn require_store_for_read(&self, operation: &str) -> Result<&Store, OpError> {
        self.require_store(operation)
    }

    pub(crate) fn require_vault(&self, operation: &str) -> Result<&dyn CredentialVault, OpError> {
        match &self.vault {
            Some(vault) => Ok(vault.as_ref()),
            None => Err(OpError::Failed(format!(
                "this engine has no credential vault, so {operation} cannot read or write the OS keyring; open it with Engine::open_with_vault"
            ))),
        }
    }

    /// Re-read folders, messages, attachments and blob hashes from the store after a sync. Labels,
    /// the label sequence and the account list are untouched: sync never changes them, and a label
    /// assignment lives in the store anyway.
    pub(crate) fn reload_mailbox(&mut self) -> Result<(), OpError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let folders = store.list_folders(None).map_err(store_map::read_error)?.into_iter().map(store_map::folder).collect::<Vec<_>>();
        let messages = store.list_all_summaries().map_err(store_map::read_error)?.into_iter().map(store_map::summary).collect::<Vec<_>>();
        let blob_hashes = store.list_blob_hashes().map_err(store_map::read_error)?;
        let drafts = store.list_draft_metadata().map_err(store_map::read_error)?;
        let reply_links =
            drafts.iter().filter_map(|draft| draft.in_reply_to.clone().map(|parent| (draft.id.clone(), parent))).collect::<HashMap<_, _>>();
        self.draft_snapshots = drafts.into_iter().map(|draft| (draft.id, draft.revision)).collect::<HashMap<_, _>>();
        self.folders = folders;
        self.messages = messages;
        self.rebuild_message_index();
        self.blob_hashes = blob_hashes;
        self.reply_links = reply_links;
        self.bump_revision();
        self.recount();
        Ok(())
    }

    /// Create a local label. The name is trimmed, must be 1-48 characters with no control, bidi
    /// or format characters, and is unique case-insensitively. Ids are never reused.
    pub fn create_label(&mut self, name: &str, color: LabelColor) -> Result<LabelSummary, OpError> {
        let name = validate_label_name(name)?;
        if self.label_name_taken(&name, None) {
            return Err(OpError::InvalidArgs(format!("a label named {name:?} already exists; label names are unique ignoring case")));
        }
        let id = self.next_label_id()?;
        if let Some(store) = &self.store {
            store.create_label(&store_map::label_row(&id, &name, color)).map_err(store_map::write_error)?;
        }
        // Only now is the id spent: a failed write must not leave a hole in the sequence.
        self.next_label_seq = self.next_label_seq.saturating_add(1);
        self.labels.push(Label { id: id.clone(), name, color });
        self.bump_revision();
        self.label_summary(&id).ok_or_else(|| OpError::Failed(format!("could not read back the new label {id:?}")))
    }

    /// Rename and/or recolour a label. At least one field is required; the id never changes.
    pub fn update_label(&mut self, id: &str, name: Option<&str>, color: Option<LabelColor>) -> Result<LabelSummary, OpError> {
        if name.is_none() && color.is_none() {
            return Err(OpError::InvalidArgs("provide name and/or color".to_owned()));
        }
        let index = self.labels.iter().position(|label| label.id == id).ok_or_else(|| unknown_label(id))?;
        let new_name = match name {
            Some(name) => {
                let name = validate_label_name(name)?;
                if self.label_name_taken(&name, Some(id)) {
                    return Err(OpError::InvalidArgs(format!("a label named {name:?} already exists; label names are unique ignoring case")));
                }
                Some(name)
            }
            None => None,
        };
        let current = self.labels.get(index).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))?;
        let resolved_name = new_name.clone().unwrap_or_else(|| current.name.clone());
        let resolved_color = color.unwrap_or(current.color);
        if let Some(store) = &self.store {
            store.update_label(id, Some(&resolved_name), Some(resolved_color.as_str())).map_err(store_map::write_error)?;
        }
        let label = self.labels.get_mut(index).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))?;
        label.name = resolved_name;
        label.color = resolved_color;
        self.bump_revision();
        self.label_summary(id).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))
    }

    /// Delete a label and detach it from every message. Returns the removed label and the number
    /// of messages that carried it. Mail is never touched.
    pub fn delete_label(&mut self, id: &str) -> Result<(Label, usize), OpError> {
        let index = self.labels.iter().position(|label| label.id == id).ok_or_else(|| unknown_label(id))?;
        if let Some(store) = &self.store {
            store.delete_label(id).map_err(store_map::write_error)?;
        }
        let label = self.labels.remove(index);
        let mut affected = 0usize;
        for message in &mut self.messages {
            let before = message.labels.len();
            message.labels.retain(|candidate| candidate != id);
            if message.labels.len() != before {
                affected = affected.saturating_add(1);
            }
        }
        for message in self.demo_messages.values_mut() {
            message.summary.labels.retain(|candidate| candidate != id);
        }
        self.bump_revision();
        Ok((label, affected))
    }

    /// Apply or remove one label on a batch of messages. The call is atomic: every message id is
    /// validated before anything changes, and repeating it changes nothing.
    pub fn set_label(&mut self, id: &str, message_ids: &[String], apply: bool) -> Result<(LabelSummary, usize), OpError> {
        if self.label(id).is_none() {
            return Err(unknown_label(id));
        }
        if message_ids.is_empty() {
            return Err(OpError::InvalidArgs("message_ids must contain at least one message id".to_owned()));
        }
        if message_ids.len() > MAX_LABEL_MESSAGES {
            return Err(OpError::InvalidArgs(format!("label_apply accepts at most {MAX_LABEL_MESSAGES} message ids")));
        }
        let ids = message_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
        for message_id in &ids {
            if !self.messages.iter().any(|message| message.id == **message_id) {
                return Err(unknown_message(message_id));
            }
        }
        if let Some(store) = &self.store {
            let batch = ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
            let result = if apply { store.apply_label(id, &batch) } else { store.remove_label(id, &batch) };
            result.map_err(store_map::write_error)?;
        }
        let mut changed = 0usize;
        for message in &mut self.messages {
            if !ids.contains(message.id.as_str()) {
                continue;
            }
            let has = message.labels.iter().any(|candidate| candidate == id);
            if apply && !has {
                message.labels.push(id.to_owned());
                changed = changed.saturating_add(1);
            } else if !apply && has {
                message.labels.retain(|candidate| candidate != id);
                changed = changed.saturating_add(1);
            }
        }
        for message in self.demo_messages.values_mut() {
            if !ids.contains(message.summary.id.as_str()) {
                continue;
            }
            let has = message.summary.labels.iter().any(|candidate| candidate == id);
            if apply && !has {
                message.summary.labels.push(id.to_owned());
            } else if !apply && has {
                message.summary.labels.retain(|candidate| candidate != id);
            }
        }
        self.bump_revision();
        let summary = self.label_summary(id).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))?;
        Ok((summary, changed))
    }

    fn label_name_taken(&self, name: &str, except: Option<&str>) -> bool {
        let name = name.to_lowercase();
        self.labels.iter().filter(|label| except != Some(label.id.as_str())).any(|label| label.name.to_lowercase() == name)
    }

    /// The next free label id. The sequence is only advanced once the label is stored, so a failed
    /// write does not burn an id.
    fn next_label_id(&self) -> Result<String, OpError> {
        if self.next_label_seq == u32::MAX {
            return Err(OpError::Failed("could not allocate a new label id".to_owned()));
        }
        let id = format!("lbl_{}", self.next_label_seq);
        if self.labels.iter().any(|label| label.id == id) {
            return Err(OpError::Failed(format!("could not allocate a new label id: {id:?} is already taken")));
        }
        Ok(id)
    }

    /// Change read/starred flags. At least one flag must be present.
    pub fn set_flags(&mut self, id: &str, unread: Option<bool>, starred: Option<bool>) -> Result<MessageSummary, OpError> {
        if unread.is_none() && starred.is_none() {
            return Err(OpError::InvalidArgs("provide unread and/or starred".to_owned()));
        }
        if !self.messages.iter().any(|message| message.id == id) {
            return Err(unknown_message(id));
        }
        if let Some(store) = &self.store {
            store.set_flags(&[id.to_owned()], FlagUpdate { unread, starred }).map_err(store_map::write_error)?;
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.id == id) else {
            return Err(OpError::Failed(format!("unknown message {id:?}")));
        };
        if let Some(unread) = unread {
            message.unread = unread;
        }
        if let Some(starred) = starred {
            message.starred = starred;
        }
        if let Some(full) = self.demo_messages.get_mut(id) {
            if let Some(unread) = unread {
                full.summary.unread = unread;
            }
            if let Some(starred) = starred {
                full.summary.starred = starred;
            }
        }
        let summary = message.clone();
        self.bump_revision();
        self.recount();
        Ok(summary)
    }

    /// Move one message to a regular folder. Virtual folders such as `starred` are views, not
    /// destinations.
    ///
    /// Only a message that is still in Drafts and has a journal row that can still deliver it is
    /// held back: a Sent copy (including the one a submission created) stays movable, archivable
    /// and deletable, and a cancelled or sent row never locks anything.
    pub fn move_message(&mut self, id: &str, folder_id: &str) -> Result<MessageSummary, OpError> {
        let target = self.folders.iter().find(|folder| folder.id == folder_id).ok_or_else(|| unknown_folder(folder_id))?;
        if target.kind.is_virtual() {
            return Err(OpError::InvalidArgs(format!("folder {folder_id:?} is virtual and cannot receive messages")));
        }
        let target_id = target.id.clone();
        let target_kind = target.kind;
        let current = self.messages.iter().find(|message| message.id == id).ok_or_else(|| unknown_message(id))?;
        let current_kind = self.folders.iter().find(|folder| folder.id == current.folder_id).map(|folder| folder.kind);
        if let Some(store) = &self.store {
            // The store, not this engine's snapshot, says where the message is. A stale view must not
            // move a row another process already sent, and a vanished row is not a movable message.
            let row = store
                .get_message(id)
                .map_err(store_map::read_error)?
                .ok_or_else(|| OpError::InvalidArgs(format!("message {id:?} no longer exists; refresh the mailbox")))?;
            if row.summary.folder_id != current.folder_id {
                return Err(OpError::InvalidArgs(format!("message {id:?} changed in another session; refresh the mailbox before moving it")));
            }
        }
        if let Some(state) = self.outbox_lock_for(id, current_kind)? {
            return Err(OpError::InvalidArgs(outbox_lock_message("message", id, state, "spostato")));
        }
        if let Some(store) = &self.store {
            store.move_messages(&[id.to_owned()], &target_id).map_err(store_map::write_error)?;
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.id == id) else {
            return Err(OpError::Failed(format!("unknown message {id:?}")));
        };
        message.folder_id = target_id.clone();
        if let Some(full) = self.demo_messages.get_mut(id) {
            full.summary.folder_id = target_id;
        }
        let summary = message.clone();
        if target_kind == FolderKind::Drafts {
            let revision = self.store.as_ref().map(|store| store.draft_revision(id)).transpose().map_err(store_map::read_error)?;
            if let Some(Some(revision)) = revision {
                self.remember_draft_revision(id, revision);
            }
        }
        self.bump_revision();
        self.recount();
        Ok(summary)
    }

    /// Create or update a draft in the Drafts folder, replacing every content field.
    ///
    /// `account` chooses the account on create: it may be omitted when the mailbox has exactly one
    /// account and is required when several exist. An update keeps the draft's own account and
    /// folder and rejects an explicit `account` that disagrees. An empty recipient list is allowed
    /// while composing; sending validates it separately. Header injection is refused here, before
    /// any state changes.
    ///
    /// With a store the message, attachments, revision and folder counts are one transaction. A
    /// failure returns before memory changes and leaves the previous rows exactly as they were.
    pub fn save_draft(&mut self, account: Option<&str>, id: Option<&str>, draft: DraftContent) -> Result<Message, OpError> {
        let draft = normalize_draft_attachment_names(draft);
        validate_draft(&draft)?;
        // Validate the reply linkage before any write. `None` means "keep what the row has"; the
        // store's COALESCE enforces that, and this map is updated from the row it returns.
        let requested_link = match draft.in_reply_to.as_deref() {
            Some(raw) => Some(normalize_reply_parent(raw)?),
            None => None,
        };
        let index = match id {
            Some(id) => {
                if id.trim().is_empty() {
                    return Err(OpError::InvalidArgs("draft id must not be empty".to_owned()));
                }
                let index = self
                    .messages
                    .iter()
                    .position(|message| message.id == id)
                    .ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}; omit id to create a draft")))?;
                let kind = self.folders.iter().find(|folder| folder.id == self.messages[index].folder_id).map(|folder| folder.kind);
                if kind != Some(FolderKind::Drafts) {
                    return Err(OpError::InvalidArgs(format!("message {id:?} is not a draft")));
                }
                if let Some(state) = self.outbox_lock_for(id, Some(FolderKind::Drafts))? {
                    return Err(OpError::InvalidArgs(outbox_lock_message("draft", id, state, "modificata")));
                }
                // The store is the authority on where the row is now: another process may have sent
                // or moved it since this engine loaded it. A stale Drafts copy must not overwrite the
                // Sent copy (it would move it back and clear its Message-ID).
                if let Some(store) = &self.store {
                    let row = store
                        .get_message(id)
                        .map_err(store_map::read_error)?
                        .ok_or_else(|| OpError::InvalidArgs(format!("draft {id:?} no longer exists; refresh the mailbox")))?;
                    if row.summary.folder_id != self.messages[index].folder_id {
                        return Err(OpError::InvalidArgs(format!(
                            "message {id:?} changed in another session (it is no longer in this folder); refresh the mailbox before editing it"
                        )));
                    }
                }
                Some(index)
            }
            None => None,
        };
        let current_full = match index {
            Some(index) => {
                let current_id =
                    self.messages.get(index).map(|message| message.id.clone()).ok_or_else(|| OpError::Failed("the draft disappeared".to_owned()))?;
                self.message_result(&current_id)?
            }
            None => None,
        };
        let current_folder = index.and_then(|index| self.messages.get(index)).map(|message| message.folder_id.as_str());
        let current_account =
            current_folder.and_then(|folder_id| self.folders.iter().find(|folder| folder.id == folder_id)).map(|folder| folder.account_id.clone());
        let account = match (index, current_account) {
            (Some(_), Some(account_id)) => {
                if let Some(requested) = account
                    && requested != account_id
                {
                    return Err(OpError::InvalidArgs(format!(
                        "draft {id:?} belongs to account {account_id:?}; omit account or pass {account_id:?} to update it"
                    )));
                }
                self.accounts
                    .iter()
                    .find(|candidate| candidate.id == account_id)
                    .cloned()
                    .ok_or_else(|| OpError::Failed(format!("the local store has draft {id:?} in a folder for unknown account {account_id:?}")))?
            }
            (Some(_), None) => {
                return Err(OpError::Failed(format!("the local store has draft {id:?} in an unknown folder")));
            }
            _ => self.draft_account(account)?,
        };
        let draft_folder = match index.and_then(|index| self.messages.get(index)) {
            Some(current) => current.folder_id.clone(),
            None => self
                .folders
                .iter()
                .find(|folder| folder.account_id == account.id && folder.kind == FolderKind::Drafts)
                .map(|folder| folder.id.clone())
                .ok_or_else(|| OpError::Failed(format!("account {:?} has no Drafts folder", account.id)))?,
        };

        let message_id = match (index, id) {
            (Some(_), Some(id)) => id.to_owned(),
            _ => match &self.store {
                // A store-wide transactional allocator: two processes never receive `draft_1`.
                Some(store) => store.allocate_draft_id().map_err(store_map::write_error)?,
                // The demo has no store and no second writer.
                None => self.next_id("draft")?,
            },
        };
        let attachments = draft
            .attachments
            .into_iter()
            .enumerate()
            .map(|(position, mut attachment)| {
                if attachment.id.trim().is_empty() {
                    attachment.id = format!("{message_id}_att_{position}");
                }
                attachment
            })
            .collect::<Vec<_>>();
        validate_unique_attachment_ids(&attachments)?;
        let has_attachments = !attachments.is_empty();
        // The ids this draft carries *now*: an id that disappears from the list must stop resolving
        // to a stored blob, because the repository deletes its row.
        let replaced_ids = current_full
            .as_ref()
            .map(|message| message.attachments.iter().map(|attachment| attachment.id.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        let summary_snippet = snippet(&draft.body_text);
        // An update keeps what the draft form does not own: its thread, starred flag, labels and
        // HTML body. The composer patches `body_text`; a subject-only save must not destroy the
        // stored HTML part.
        let (thread_id, starred, labels, body_html) = match index.and_then(|index| self.messages.get(index)) {
            Some(current) => (
                current.thread_id.clone(),
                current.starred,
                current.labels.clone(),
                current_full.as_ref().and_then(|message| message.body_html.clone()),
            ),
            None => (format!("thr_{message_id}"), false, Vec::new(), None),
        };
        let mut saved = Message {
            summary: MessageSummary {
                id: message_id.clone(),
                thread_id,
                folder_id: draft_folder,
                from: Address::new(account.name.clone(), account.email.clone()),
                to: draft.to,
                subject: draft.subject,
                snippet: summary_snippet,
                date: Utc::now(),
                unread: false,
                starred,
                has_attachments,
                labels,
            },
            cc: draft.cc,
            bcc: draft.bcc,
            body_text: draft.body_text,
            body_html,
            remote_images: 0,
            attachments,
        };

        let mut saved_revision = None;
        let persisted = if let Some(store) = &self.store {
            let row = store_map::message_row(&saved, &account.id, None);
            let stored = store
                .save_draft_message_linked(&row, &store_map::attachment_rows(&saved.attachments), requested_link.as_deref())
                .map_err(store_map::write_error)?;
            saved_revision = Some(stored.revision);
            // The repository keeps the stored hash *and size* of an attachment whose id survived the
            // replacement. Mirror those rows, or memory would cap a 2 MiB blob at a stale size of 0
            // and refuse to read it until the next restart.
            Some((store.attachments(&saved.summary.id).map_err(store_map::read_error)?, stored.in_reply_to))
        } else {
            None
        };
        let persisted_link = match &persisted {
            Some((rows, link)) => {
                for id in &replaced_ids {
                    self.blob_hashes.remove(id);
                }
                for row in rows {
                    if let Some(hash) = &row.blob_hash {
                        self.blob_hashes.insert(row.id.clone(), hash.clone());
                    }
                }
                saved.attachments = rows.iter().cloned().map(store_map::attachment).collect();
                link.clone()
            }
            None => {
                for id in &replaced_ids {
                    if !saved.attachments.iter().any(|attachment| attachment.id == *id) {
                        self.blob_hashes.remove(id);
                    }
                }
                // Demo: no store means no COALESCE, so the request itself is the new value and an
                // omitted link keeps the in-memory one.
                requested_link.or_else(|| self.reply_links.get(&saved.summary.id).cloned())
            }
        };
        match &persisted_link {
            Some(link) => {
                self.reply_links.insert(saved.summary.id.clone(), link.clone());
            }
            None => {
                self.reply_links.remove(&saved.summary.id);
            }
        }

        match index {
            Some(index) => {
                let Some(message) = self.messages.get_mut(index) else {
                    return Err(OpError::Failed(format!("unknown draft {message_id:?}")));
                };
                *message = saved.summary.clone();
            }
            None => {
                self.messages.push(saved.summary.clone());
                self.rebuild_message_index();
            }
        }
        if self.in_memory {
            self.demo_messages.insert(saved.summary.id.clone(), saved.clone());
        }
        self.bump_revision();
        self.cache_message(&saved);
        self.recount();
        if let Some(revision) = saved_revision {
            self.remember_draft_revision(&saved.summary.id, revision);
        }
        Ok(saved)
    }

    /// Pick the account for a new draft. One account is implied; several require an explicit id.
    fn draft_account(&self, requested: Option<&str>) -> Result<Account, OpError> {
        if let Some(requested) = requested {
            return self
                .accounts
                .iter()
                .find(|account| account.id == requested)
                .cloned()
                .ok_or_else(|| OpError::InvalidArgs(format!("unknown account {requested:?}; run accounts_list to see the configured ids")));
        }
        match self.accounts.as_slice() {
            [] => Err(OpError::Failed("no account is configured".to_owned())),
            [account] => Ok(account.clone()),
            accounts => Err(OpError::InvalidArgs(format!(
                "the mailbox has {} accounts; pass `account` to draft_save to choose where the draft lives (for example {:?})",
                accounts.len(),
                accounts.first().map(|account| account.id.as_str()).unwrap_or_default()
            ))),
        }
    }

    /// Queue a draft for immediate submission, with the undo window.
    ///
    /// The draft is serialized and its exact bytes are persisted in the encrypted blob store before
    /// the outbox row exists. The draft stays in Drafts while the row is queued, so Undo is a
    /// cancellation rather than a restore; a queued draft cannot be edited or moved until it is
    /// cancelled or resolved.
    pub fn message_send(&mut self, draft_id: &str) -> Result<SendQueued, OpError> {
        self.message_send_with_pgp(draft_id, &crate::pgp_mime::PgpSendOptions::default())
    }

    /// Queue a draft with explicit PGP sign/encrypt options.
    ///
    /// Any PGP failure aborts before a blob or an outbox row is written: there is no plaintext
    /// fallback.
    pub fn message_send_with_pgp(&mut self, draft_id: &str, pgp: &crate::pgp_mime::PgpSendOptions) -> Result<SendQueued, OpError> {
        let now = Utc::now().timestamp_millis();
        self.queue_send(draft_id, pgp, now.saturating_add(UNDO_MS), now)
    }

    /// Queue a draft at an explicit instant. A time in the past is treated as "now".
    pub fn send_schedule(&mut self, draft_id: &str, send_at: chrono::DateTime<Utc>) -> Result<SendQueued, OpError> {
        self.send_schedule_with_pgp(draft_id, send_at, &crate::pgp_mime::PgpSendOptions::default())
    }

    /// Schedule a draft with explicit PGP sign/encrypt options.
    pub fn send_schedule_with_pgp(
        &mut self,
        draft_id: &str,
        send_at: chrono::DateTime<Utc>,
        pgp: &crate::pgp_mime::PgpSendOptions,
    ) -> Result<SendQueued, OpError> {
        let now = Utc::now().timestamp_millis();
        self.queue_send(draft_id, pgp, send_at.timestamp_millis().max(now), now)
    }

    /// Serialize a draft and return the exact bytes without touching the outbox.
    ///
    /// The same builder as a real send, so a dry run proves what would leave the machine. No network
    /// call and no row.
    pub fn send_dry_run(&self, draft_id: &str) -> Result<SendDryRun, OpError> {
        self.send_dry_run_with_pgp(draft_id, &crate::pgp_mime::PgpSendOptions::default())
    }

    /// Dry run with explicit PGP sign/encrypt options.
    pub fn send_dry_run_with_pgp(&self, draft_id: &str, pgp: &crate::pgp_mime::PgpSendOptions) -> Result<SendDryRun, OpError> {
        let (serialized, _account, _message) = self.submission_for_draft(draft_id, pgp)?;
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &serialized.raw);
        Ok(SendDryRun {
            raw_base64: encoded,
            bytes: serialized.raw.len(),
            envelope: SendEnvelope { from: serialized.envelope.from.clone(), recipients: serialized.envelope.recipients.clone() },
            subject: _message.summary.subject.clone(),
            message_id: serialized.message_id.clone(),
        })
    }

    fn queue_send(&mut self, draft_id: &str, pgp: &crate::pgp_mime::PgpSendOptions, send_at: i64, now: i64) -> Result<SendQueued, OpError> {
        let store = self.require_store("message_send")?;
        let draft_row = store.get_draft(draft_id).map_err(store_map::read_error)?;
        let revision = draft_row.as_ref().map(|draft| draft.revision);
        if let Some(revision) = revision
            && self.draft_snapshots.get(draft_id).is_some_and(|snapshot| *snapshot != revision)
        {
            return Err(OpError::InvalidArgs(format!("draft {draft_id:?} changed in another session; refresh the mailbox before sending it")));
        }
        let (serialized, account, _message) = self.submission_for_draft(draft_id, pgp)?;
        // Re-check after the body read: another process may have saved between the snapshot check and
        // serialization. The revision is store-owned and changes on every save.
        if let Some(revision) = revision
            && store.draft_revision(draft_id).map_err(store_map::read_error)? != Some(revision)
        {
            return Err(OpError::InvalidArgs(format!("draft {draft_id:?} changed in another session; refresh the mailbox before sending it")));
        }
        let server = store
            .get_account_server(&account.id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::Failed(format!("account {:?} has no server configuration; remove it and add it again", account.id)))?;
        if server.smtp.is_none() {
            return Err(OpError::Failed(format!(
                "account {:?} has no SMTP endpoint, so it can read but not send. Remove it and add it again with an SMTP host, or edit the account configuration.",
                account.id
            )));
        }
        // The newest row for this draft decides, and it is one indexed lookup, never a history scan.
        if let Some(existing) = store.active_outbox_for_draft(draft_id).map_err(store_map::read_error)? {
            match existing.state {
                OutboxState::Queued => {
                    // The same queue operation was retried before the undo window expired: return
                    // the existing row instead of inserting a duplicate submission. Only bytes that
                    // hash to the row's blob are the same submission: a plaintext row must never be
                    // returned for a signed or encrypted request, which would deliver cleartext.
                    let outgoing_hash = blake3::hash(&serialized.raw).to_hex().to_string();
                    if existing.raw_blob_hash != outgoing_hash {
                        return Err(OpError::InvalidArgs(format!(
                            "draft {draft_id:?} is already queued with different content or PGP protection; cancel that row with `postino outbox cancel {}` before sending it again",
                            existing.id
                        )));
                    }
                    let (view, _warning) = view(&existing);
                    return Ok(SendQueued { outbox: view, undo_until: Some(existing.send_at) });
                }
                OutboxState::Cancelled => {}
                OutboxState::Sending => {
                    return Err(OpError::InvalidArgs(format!("draft {draft_id:?} is being submitted right now; watch it in the outbox")));
                }
                OutboxState::AppendPending => {
                    return Err(OpError::InvalidArgs(format!(
                        "draft {draft_id:?} was already submitted and is waiting for the Sent copy; it cannot be queued again"
                    )));
                }
                OutboxState::Sent => {
                    // A sent row is the record of a delivery. It blocks the draft state that was
                    // delivered: the crash window where the local move did not run, and a Sent copy
                    // the user moved back to Drafts without editing. A draft edited since the send
                    // (a new revision) is a deliberate resend, so it may queue as a new revision.
                    let sent_revision = OutboxEnvelope::decode(&existing.envelope_json).and_then(|envelope| envelope.draft_revision);
                    let edited_since = matches!((sent_revision, revision), (Some(sent), Some(current)) if sent != current);
                    if !edited_since {
                        return Err(OpError::InvalidArgs(format!(
                            "draft {draft_id:?} was already sent; edit it (a new revision) or create a new draft to send it again"
                        )));
                    }
                }
                OutboxState::Uncertain => {
                    return Err(OpError::InvalidArgs(format!(
                        "draft {draft_id:?} has an uncertain submission; resolve it from the outbox (mark it sent or retry manually) before queueing it again"
                    )));
                }
                OutboxState::Failed => {
                    return Err(OpError::InvalidArgs(format!(
                        "draft {draft_id:?} has a failed submission; discard that row with outbox_cancel or retry it from the outbox instead of queueing a second copy"
                    )));
                }
            }
        }
        let raw_hash = store.blobs().put(&serialized.raw).map_err(store_map::blob_write_error)?;
        // Stable per draft revision and content: a second process that queues the same unchanged
        // draft computes the same key, and the unique index makes the second insert a no-op instead
        // of a delivery.
        let revision_key = revision.map_or_else(|| "0".to_owned(), |revision| revision.to_string());
        let idempotency_key = format!("draft:{draft_id}:{revision_key}:{}", raw_hash.get(..16).unwrap_or(raw_hash.as_str()));
        let id = {
            let digest = blake3::hash(idempotency_key.as_bytes()).to_hex();
            format!("ob_{}", digest.as_str().get(..16).unwrap_or(digest.as_str()))
        };
        let payload = OutboxEnvelope {
            from: serialized.envelope.from.clone(),
            recipients: serialized.envelope.recipients.clone(),
            subject: _message.summary.subject.clone(),
            message_id: serialized.message_id.clone(),
            draft_id: Some(draft_id.to_owned()),
            draft_revision: revision,
        };
        let envelope_json = payload.encode().map_err(|error| OpError::Failed(format!("could not encode the envelope: {error}")))?;
        let entry = OutboxEntry {
            id,
            account_id: account.id.clone(),
            draft_id: Some(draft_id.to_owned()),
            idempotency_key,
            created_at: now,
            send_at,
            state: OutboxState::Queued,
            attempts: 0,
            append_attempts: 0,
            last_error: None,
            raw_blob_hash: raw_hash,
            envelope_json,
            lease_at: None,
        };
        match store.queue_outbox(&entry) {
            Ok(row) => {
                if let Some(revision) = revision {
                    self.remember_draft_revision(draft_id, revision);
                }
                let (view, _warning) = view(&row);
                Ok(SendQueued { outbox: view, undo_until: Some(row.send_at) })
            }
            Err(postino_store::StoreError::OutboxAlreadyActive { state, .. }) => {
                // Another process queued the same draft between our lookup and our insert. The
                // idempotent answer is that row, never a second submission.
                let existing = store
                    .active_outbox_for_draft(draft_id)
                    .map_err(store_map::read_error)?
                    .ok_or_else(|| OpError::Failed(format!("draft {draft_id:?} was queued concurrently and its row is not readable")))?;
                if state == OutboxState::Queued {
                    let (view, _warning) = view(&existing);
                    return Ok(SendQueued { outbox: view, undo_until: Some(existing.send_at) });
                }
                Err(OpError::InvalidArgs(format!("draft {draft_id:?} changed state while queueing ({state}); refresh the outbox")))
            }
            Err(error) => Err(store_map::write_error(error)),
        }
    }

    /// Build the exact RFC 5322 bytes for one draft. Attachment bytes come from the encrypted blob
    /// store; a metadata-only attachment is refused before any network or queue row.
    ///
    /// The `Date` header is the draft's own last-saved instant, not the wall clock. That is what makes
    /// `send_dry_run` and `message_send` produce the same bytes even a second apart: the dry run,
    /// the queued blob and a resend after a cancel all serialize the same draft state the same way.
    /// The Message-ID is derived from that digest, so it is stable too. A scheduled message therefore
    /// carries the instant it was last edited; the header dates the message, not the queue.
    fn submission_for_draft(
        &self,
        draft_id: &str,
        pgp: &crate::pgp_mime::PgpSendOptions,
    ) -> Result<(postino_sync::SerializedSubmission, Account, Message), OpError> {
        let current = self.message_result(draft_id)?.ok_or_else(|| unknown_message(draft_id))?;
        let folder = self
            .folders
            .iter()
            .find(|folder| folder.id == current.summary.folder_id)
            .ok_or_else(|| OpError::Failed(format!("draft {draft_id:?} lives in an unknown folder")))?;
        if folder.kind != FolderKind::Drafts {
            return Err(OpError::InvalidArgs(format!("message {draft_id:?} is not a draft")));
        }
        let account = self
            .accounts
            .iter()
            .find(|account| account.id == folder.account_id)
            .cloned()
            .ok_or_else(|| OpError::Failed(format!("draft {draft_id:?} belongs to unknown account {:?}", folder.account_id)))?;
        if current.summary.to.is_empty() && current.cc.is_empty() && current.bcc.is_empty() {
            return Err(OpError::InvalidArgs("a message needs at least one recipient".to_owned()));
        }
        let mut attachments = Vec::with_capacity(current.attachments.len());
        for attachment in &current.attachments {
            let data = self.attachment_bytes(&attachment.id).map_err(|error| {
                OpError::Failed(format!(
                    "attachment {:?} has no stored bytes ({error}); attachments without content cannot be sent. Remove it or re-attach the file.",
                    attachment.filename
                ))
            })?;
            attachments.push(postino_sync::SubmissionAttachment {
                filename: attachment.filename.clone(),
                mime: attachment.mime.clone(),
                data,
                content_id: None,
            });
        }
        let submission = postino_sync::Submission {
            from: postino_sync::SubmissionMailbox { name: Some(account.name.clone()), address: account.email.clone() },
            to: current.summary.to.iter().map(mailbox).collect(),
            cc: current.cc.iter().map(mailbox).collect(),
            bcc: current.bcc.iter().map(mailbox).collect(),
            subject: current.summary.subject.clone(),
            date: current.summary.date,
            text: current.body_text.clone(),
            html: current.body_html.clone(),
            attachments,
            in_reply_to: self.reply_links.get(draft_id).cloned(),
            // P8 has the parent Message-ID but not the parent's own References list. The parent id
            // alone is a valid single-entry chain and is what P9 will replace with the full list
            // persisted from the incoming message.
            references: self.reply_links.get(draft_id).cloned().into_iter().collect(),
            message_id: None,
            extra_headers: Vec::new(),
        };
        let serialized = postino_sync::serialize_submission(&submission)
            .map_err(|error| OpError::InvalidArgs(format!("the message cannot be serialized: {error}")))?;
        let mut serialized = serialized;
        if !pgp.is_plain() {
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| OpError::Failed("the in-memory demo has no encrypted store, so it cannot sign or encrypt mail".to_owned()))?;
            serialized.raw = crate::pgp_mime::apply_send(store, &account, serialized.raw, pgp)?;
        }
        if serialized.raw.len() as u64 > MAX_SUBMISSION_BYTES {
            return Err(OpError::InvalidArgs(format!(
                "the message is {} bytes; this build submits at most {MAX_SUBMISSION_BYTES} bytes",
                serialized.raw.len()
            )));
        }
        if serialized.envelope.recipients.is_empty() {
            return Err(OpError::InvalidArgs("the message has no valid recipient".to_owned()));
        }
        if serialized.envelope.recipients.len() > 100 {
            return Err(OpError::InvalidArgs(format!(
                "the message has {} recipients; this build accepts at most 100",
                serialized.envelope.recipients.len()
            )));
        }
        Ok((serialized, account, current))
    }

    /// The outbox state that holds this message back, or `None` when it may be edited or moved.
    ///
    /// Only a message that is still in Drafts can be held: the journal row exists to deliver that
    /// draft. A Sent copy, including the one a successful submission created, is ordinary mail and
    /// must stay movable and archivable. The partial unique index guarantees at most one active row
    /// per draft, so this is a direct state query, not a list scan.
    fn outbox_lock_for(&self, id: &str, folder_kind: Option<FolderKind>) -> Result<Option<OutboxState>, OpError> {
        if folder_kind != Some(FolderKind::Drafts) {
            return Ok(None);
        }
        match &self.store {
            Some(store) => store.active_state_for_draft(id).map_err(store_map::read_error).map(|state| state.filter(|state| state.blocks_draft())),
            None => Ok(None),
        }
    }

    /// The instant at which the outbox next has work for a worker: a queued or Sent-append row's due
    /// time, or a `sending` row's lease expiry. `None` when nothing is pending. One indexed query,
    /// so a scheduler never has to hold the journal in memory or trust a cached list.
    pub fn next_outbox_wake(&self) -> Result<Option<i64>, OpError> {
        match &self.store {
            Some(store) => store.next_outbox_wake(LEASE_MS).map_err(store_map::read_error),
            None => Ok(None),
        }
    }

    /// One page of journal rows and the accounts whose queue is stopped. A demo engine has no outbox
    /// and says so; a damaged envelope is a warning, never a listing failure. `limit` is clamped to
    /// 1..=500 and `offset` pages backwards (newest first); terminal rows older than 30 days are
    /// pruned, so the history is bounded in time as well as by the page.
    pub fn outbox_list(&self, account: Option<&str>, limit: u32, offset: u32) -> Result<OutboxList, OpError> {
        let Some(store) = &self.store else {
            return Ok(OutboxList {
                entries: Vec::new(),
                halted: Vec::new(),
                warnings: vec!["la casella demo non ha una coda: l'invio reale richiede un account su archivio (postino --store <path>)".to_owned()],
            });
        };
        if let Some(account) = account {
            self.find_account(account)?;
        }
        let limit = limit.clamp(1, postino_store::MAX_OUTBOX_ROWS);
        let rows = store.list_outbox_page(account, limit, offset).map_err(store_map::read_error)?;
        let mut warnings = Vec::new();
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let (entry, warning) = view(row);
            entries.push(entry);
            if let Some(warning) = warning {
                warnings.push(warning);
            }
        }
        let mut halted = Vec::new();
        let mut accounts = rows.iter().map(|row| row.account_id.clone()).collect::<BTreeSet<_>>();
        if let Some(account) = account {
            accounts.insert(account.to_owned());
        }
        for account in accounts {
            match read_halt(store, &account) {
                Ok(Some(halt)) => halted.push(halt),
                Ok(None) => {}
                Err(error) => warnings.push(format!("lo stato di fermo per l'account {account:?} non è leggibile: {error}")),
            }
        }
        Ok(OutboxList { entries, halted, warnings })
    }

    /// Cancel a row that has not been leased yet, or discard a permanently failed one.
    ///
    /// A `failed` row is a permanent refusal: no delivery happened, so the journal row can go and the
    /// draft becomes editable again. After `sending`, `append_pending` or `uncertain` only the
    /// uncertain policy applies, because the message may have been delivered.
    pub fn outbox_cancel(&mut self, id: &str) -> Result<OutboxView, OpError> {
        let store = self.require_store("outbox_cancel")?;
        let entry = store
            .get_outbox(id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::InvalidArgs(format!("unknown outbox row {id:?}; use an id returned by outbox_list")))?;
        if !matches!(entry.state, OutboxState::Queued | OutboxState::Failed) {
            return Err(OpError::InvalidArgs(format!("outbox row {id:?} is {}, not queued or failed; it cannot be discarded", entry.state.as_str())));
        }
        if !store.cancel_outbox(id).map_err(store_map::write_error)? {
            return Err(OpError::Failed(format!("outbox row {id:?} could not be discarded; it may have just been leased")));
        }
        let entry = store.get_outbox(id).map_err(store_map::read_error)?.unwrap_or(entry);
        let (view, _warning) = view(&entry);
        Ok(view)
    }

    /// Manual retry of a failed or uncertain row. `confirm` is mandatory; an uncertain row may have
    /// been delivered, so the caller has to say so explicitly.
    pub fn outbox_retry(&mut self, id: &str, confirm: bool) -> Result<OutboxView, OpError> {
        let store = self.require_store("outbox_retry")?;
        let entry = store
            .get_outbox(id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::InvalidArgs(format!("unknown outbox row {id:?}; use an id returned by outbox_list")))?;
        if !matches!(entry.state, OutboxState::Failed | OutboxState::Uncertain) {
            return Err(OpError::InvalidArgs(format!(
                "outbox row {id:?} is {}, not failed or uncertain; only those states can be retried",
                entry.state.as_str()
            )));
        }
        if !confirm {
            let warning = if entry.state == OutboxState::Uncertain {
                "this submission may already have reached the server; retrying could send it twice. Pass confirm: true only after checking Sent."
            } else {
                "retrying sends the stored bytes again. Pass confirm: true to proceed."
            };
            return Err(OpError::InvalidArgs(warning.to_owned()));
        }
        clear_halt(store, &entry.account_id).map_err(store_map::write_error)?;
        if !store.retry_outbox(id, Utc::now().timestamp_millis()).map_err(store_map::write_error)? {
            return Err(OpError::Failed(format!("outbox row {id:?} changed state before the retry; refresh the outbox")));
        }
        let entry = store.get_outbox(id).map_err(store_map::read_error)?.unwrap_or(entry);
        let (view, _warning) = view(&entry);
        Ok(view)
    }

    /// Resolve an uncertain row as delivered without contacting the server. `confirm` is mandatory.
    pub fn outbox_mark_sent(&mut self, id: &str, confirm: bool) -> Result<OutboxView, OpError> {
        let Some(store) = self.store.take() else {
            return Err(OpError::Failed(
                "the demo mailbox has no outbox; real submission needs a store-backed account (postino --store <path>)".to_owned(),
            ));
        };
        let result = (|| {
            let entry = store
                .get_outbox(id)
                .map_err(store_map::read_error)?
                .ok_or_else(|| OpError::InvalidArgs(format!("unknown outbox row {id:?}; use an id returned by outbox_list")))?;
            if entry.state != OutboxState::Uncertain {
                return Err(OpError::InvalidArgs(format!(
                    "outbox row {id:?} is {}, not uncertain; only an uncertain submission can be marked sent",
                    entry.state.as_str()
                )));
            }
            if !confirm {
                return Err(OpError::InvalidArgs(
                    "marking the row sent records a local decision and never contacts the server; pass confirm: true to proceed".to_owned(),
                ));
            }
            // A test can model a concurrent process changing the row at this exact instant.
            #[cfg(debug_assertions)]
            if let Some(hook) = self.test_after_outbox_read.take() {
                hook();
            }
            if !store.confirm_outbox_sent(id).map_err(store_map::write_error)? {
                // A concurrent retry (or another resolver) changed the row between the read and the
                // confirm. Moving the draft now would label a queued or otherwise live submission as
                // delivered; leave everything alone and tell the caller to refresh.
                return Err(OpError::Failed("the outbox row changed state before it could be marked sent; refresh the outbox".to_owned()));
            }
            if let Some(envelope) = OutboxEnvelope::decode(&entry.envelope_json)
                && let Some(draft_id) = envelope.draft_id.as_deref()
            {
                let _ = self.finalize_outbox_draft(Some(draft_id), Some(&envelope.message_id), envelope.draft_revision, &store);
            }
            let entry = store.get_outbox(id).map_err(store_map::read_error)?.unwrap_or(entry);
            let (view, _warning) = view(&entry);
            Ok(view)
        })();
        self.store = Some(store);
        result
    }

    /// Process every due row once: expire stale leases, submit `queued` rows, append `append_pending`
    /// rows. The caller decides when to run it; the desktop worker and the CLI both use this method.
    ///
    /// A crash during `DATA` becomes `uncertain` after [`LEASE_MS`]; it is never retried
    /// automatically. An authentication failure stops the account's queue with an actionable halt.
    pub fn process_outbox(&mut self, now_ms: i64) -> Result<OutboxRunReport, OpError> {
        let Some(store) = self.store.take() else {
            return Err(OpError::Failed(
                "the demo mailbox has no outbox; real submission needs a store-backed account (postino --store <path>)".to_owned(),
            ));
        };
        let result = self.process_outbox_inner(&store, now_ms);
        self.store = Some(store);
        result
    }

    fn process_outbox_inner(&mut self, store: &Store, now_ms: i64) -> Result<OutboxRunReport, OpError> {
        let mut report = OutboxRunReport::default();
        let pass_start = now_ms;
        let stale = store.expire_stale_outbox(pass_start, LEASE_MS).map_err(store_map::write_error)?;
        for id in stale {
            report.uncertain = report.uncertain.saturating_add(1);
            report.errors.push(format!("outbox row {id:?}: {}.", postino_store::UNCERTAIN_AFTER_CRASH));
        }
        // Terminal history is real history, not live state: keep a bounded window and delete only
        // rows old enough to be irrelevant. A queued, sending, append_pending or uncertain row is
        // never a candidate.
        let _ = store.prune_outbox(pass_start, OUTBOX_RETENTION_MS, 200);
        let mut halted = BTreeSet::new();
        for _ in 0..20 {
            // Each lease is stamped with the instant the row is actually claimed. A row leased after
            // another row spent 30 minutes in SMTP must not carry the pass-start time; otherwise a
            // concurrent pass at pass_start + LEASE + 1 expires a transfer that is still running.
            let leased_at = self.claim_now(pass_start);
            let Some(entry) = store.lease_next_outbox(leased_at).map_err(store_map::read_error)? else {
                break;
            };
            let lease = entry.lease_at.unwrap_or(leased_at);
            if let Ok(Some(halt)) = read_halt(store, &entry.account_id) {
                halted.insert(halt.account.clone());
                let now = self.claim_now(pass_start);
                store.release_outbox(&entry.id, now.saturating_add(60_000), lease).map_err(store_map::write_error)?;
                report.errors.push(format!("account {:?} fermato: {}", halt.account, halt.error));
                continue;
            }
            report.processed = report.processed.saturating_add(1);
            self.process_outbox_entry(store, &entry, lease, pass_start, &mut report)?;
        }
        // The Sent append is independent of submission. A row in append_pending is never leased as
        // queued, so it can never be submitted twice here. Each append is claimed (its lease and its
        // attempt count are written) before the network: a second process cannot append the same row
        // concurrently, and a crash leaves a claim that expires.
        let mut appended = BTreeSet::new();
        for _ in 0..100 {
            let claimed_at = self.claim_now(pass_start);
            let Some(entry) = store.claim_outbox_append(claimed_at, APPEND_LEASE_MS).map_err(store_map::read_error)? else {
                break;
            };
            let lease = entry.lease_at.unwrap_or(claimed_at);
            let now = self.claim_now(pass_start);
            if !appended.insert(entry.id.clone()) {
                // A slow network failure can leave the row due again while this pass is still
                // running. Never append the same row twice in one pass.
                let _ = store.reschedule_outbox_append(
                    &entry.id,
                    now.saturating_add(backoff_ms(entry.append_attempts.max(1))),
                    Some("la copia in Inviati resta da fare"),
                    lease,
                );
                break;
            }
            report.processed = report.processed.saturating_add(1);
            match self.append_entry(store, &entry, lease, now) {
                Ok(()) => report.sent = report.sent.saturating_add(1),
                Err(error) => {
                    report.append_pending = report.append_pending.saturating_add(1);
                    report.errors.push(error);
                }
            }
        }
        report.halted = halted.into_iter().collect();
        let rows = store.list_outbox(None, 500).map_err(store_map::read_error)?;
        report.entries = rows.iter().map(|row| view(row).0).collect();
        Ok(report)
    }

    /// One leased submission. `lease` is the epoch the row was claimed with: every transition below
    /// is conditional on it, so a worker whose lease expired cannot overwrite the next owner.
    ///
    /// Every sentence this writes is fixed. A hostile SMTP server has just received the password and
    /// the body; its reply text never becomes a journal field, a CLI line or an MCP result.
    fn process_outbox_entry(
        &mut self,
        store: &Store,
        entry: &OutboxEntry,
        lease: i64,
        pass_start: i64,
        report: &mut OutboxRunReport,
    ) -> Result<(), OpError> {
        let Some(envelope) = OutboxEnvelope::decode(&entry.envelope_json) else {
            return self.fail_leased(store, entry, lease, "l'inviluppo salvato non è leggibile", report);
        };
        // The submission snapshot: the draft must still be the one that was serialized. A stale
        // connection (or a lock bypassed by an edit) must not deliver old bytes and then label the
        // newer draft as the delivered message.
        if let Some(reason) = stale_submission_reason(store, &envelope)? {
            return self.fail_leased(store, entry, lease, reason, report);
        }
        let server = match store.get_account_server(&entry.account_id) {
            Ok(Some(server)) => server,
            Ok(None) => return self.fail_leased(store, entry, lease, "l'account non è più configurato", report),
            Err(error) => return Err(store_map::read_error(error)),
        };
        let Some(smtp) = server.smtp.clone() else {
            return self.fail_leased(store, entry, lease, "l'account non ha un endpoint SMTP", report);
        };
        // A local read failure happens before any network call: it is a failure, not an uncertain
        // submission, and it must not stop the rest of the pass.
        let raw = match store.blobs().get(&entry.raw_blob_hash, MAX_SUBMISSION_BYTES.saturating_add(1024 * 1024)) {
            Ok(raw) => raw,
            Err(_) => return self.fail_leased(store, entry, lease, "i byte salvati del messaggio non sono leggibili", report),
        };
        let smtp_credential = match self.smtp_credential(&entry.account_id, "sending") {
            Ok(credential) => credential,
            Err(_) => return self.fail_leased(store, entry, lease, "la credenziale SMTP non è disponibile", report),
        };
        let mut config = crate::oauth::smtp_config(
            smtp.host.clone(),
            smtp.port,
            store_map::mail_tls(smtp.tls),
            smtp.username.clone(),
            smtp_credential,
            self.extra_roots.clone(),
        );
        config.limits = SmtpLimits { max_message: usize::try_from(MAX_SUBMISSION_BYTES).unwrap_or(usize::MAX), ..SmtpLimits::default() };
        let message = SmtpEnvelope { from: envelope.from.clone(), recipients: envelope.recipients.clone() };
        match postino_sync::submit(config, &message, raw.as_slice()) {
            Ok(_delivery) => {
                #[cfg(debug_assertions)]
                if self.crash_after_accept {
                    // Test seam: the process dies after the server's 250 and before the durable
                    // transition. Nothing below may run; the row stays `sending`.
                    return Err(OpError::Failed(format!(
                        "test crash after the server accepted outbox row {:?}; the journal still says sending",
                        entry.id
                    )));
                }
                // The append claim starts when the server actually accepted the message, not when the
                // pass started: a 30-minute submission must not leave its APPEND with an almost
                // expired claim.
                let append_claim = self.claim_now(pass_start);
                if !store.mark_outbox_append_pending(&entry.id, lease, append_claim).map_err(store_map::write_error)? {
                    report.errors.push(format!(
                        "la riga {:?} è stata inviata ma un altro processo l'ha presa in carico; lo stato non è stato toccato",
                        entry.id
                    ));
                    return Ok(());
                }
                let already_on_server = match self.finalize_outbox_draft(
                    envelope.draft_id.as_deref(),
                    Some(&envelope.message_id),
                    envelope.draft_revision,
                    store,
                ) {
                    Ok(FinalizeOutcome::Moved { server_uid: Some(_) }) => true,
                    Ok(FinalizeOutcome::Moved { server_uid: None } | FinalizeOutcome::NotADraft) => false,
                    Ok(FinalizeOutcome::SnapshotChanged) => {
                        report.errors.push(format!(
                            "la riga {:?} è stata inviata ma la bozza è cambiata nel frattempo: resta in Bozze, la copia in Inviati arriverà con la sincronizzazione",
                            entry.id
                        ));
                        false
                    }
                    Err(error) => {
                        report.errors.push(format!("la riga {:?} è stata inviata ma la copia locale in Inviati non è riuscita: {error}", entry.id));
                        false
                    }
                };
                if already_on_server {
                    // A provider auto-save (or a sync before this move) already put the message in
                    // Sent, and the local row now carries that UID. Appending again would duplicate
                    // the server copy, so finish the row locally.
                    if store.finish_outbox_append(&entry.id, append_claim).map_err(store_map::write_error)? {
                        report.sent = report.sent.saturating_add(1);
                    } else {
                        report.errors.push(format!("la riga {:?} è già sul server ma il suo stato è cambiato; aggiorna la coda", entry.id));
                    }
                    return Ok(());
                }
                let updated = store.get_outbox(&entry.id).map_err(store_map::read_error)?.unwrap_or_else(|| entry.clone());
                let append_now = self.claim_now(pass_start);
                // This worker owns the append claim it just wrote.
                match self.append_entry(store, &updated, append_claim, append_now) {
                    Ok(()) => report.sent = report.sent.saturating_add(1),
                    Err(error) => {
                        report.append_pending = report.append_pending.saturating_add(1);
                        report.errors.push(error);
                    }
                }
            }
            Err(error) => {
                let sentence = smtp_error_sentence(&error);
                let now = self.claim_now(pass_start);
                if error.is_uncertain() {
                    if store.mark_outbox_uncertain(&entry.id, &sentence, lease).map_err(store_map::write_error)? {
                        report.uncertain = report.uncertain.saturating_add(1);
                        report.errors.push(format!("outbox row {:?}: {sentence}", entry.id));
                    } else {
                        report.errors.push(lost_lease_sentence(entry));
                    }
                } else if error.is_authentication() {
                    let halt = "l'autenticazione SMTP è stata rifiutata: aggiorna la password dell'account e riprova la riga della coda";
                    store.mark_outbox_failed(&entry.id, halt, lease).map_err(store_map::write_error)?;
                    write_halt(store, &entry.account_id, halt, now).map_err(store_map::write_error)?;
                    report.failed = report.failed.saturating_add(1);
                    report.errors.push(format!("account {:?} fermato: {halt}", entry.account_id));
                } else if error.is_transient() && entry.attempts < MAX_ATTEMPTS {
                    let delay = backoff_ms(entry.attempts);
                    if !store
                        .reschedule_outbox(&entry.id, now.saturating_add(delay).min(now.saturating_add(BACKOFF_MAX_MS)), Some(&sentence), lease)
                        .map_err(store_map::write_error)?
                    {
                        report.errors.push(lost_lease_sentence(entry));
                        return Ok(());
                    }
                    report.retried = report.retried.saturating_add(1);
                } else {
                    if store.mark_outbox_failed(&entry.id, &sentence, lease).map_err(store_map::write_error)? {
                        report.failed = report.failed.saturating_add(1);
                        report.errors.push(format!("outbox row {:?} failed: {sentence}", entry.id));
                    } else {
                        report.errors.push(lost_lease_sentence(entry));
                    }
                }
            }
        }
        Ok(())
    }

    /// Fail one leased row with a fixed sentence and keep the pass going.
    fn fail_leased(&self, store: &Store, entry: &OutboxEntry, lease: i64, reason: &str, report: &mut OutboxRunReport) -> Result<(), OpError> {
        if store.mark_outbox_failed(&entry.id, reason, lease).map_err(store_map::write_error)? {
            report.failed = report.failed.saturating_add(1);
            report.errors.push(format!("riga {:?} non riuscita: {reason}.", entry.id));
        } else {
            report.errors.push(lost_lease_sentence(entry));
        }
        Ok(())
    }

    /// One claimed Sent append. `lease` is the append claim this worker wrote; every state change is
    /// conditional on it. A damaged envelope is a fixed failure with backoff, never a false `sent`.
    fn append_entry(&mut self, store: &Store, entry: &OutboxEntry, lease: i64, now_ms: i64) -> Result<(), String> {
        let Some(envelope) = OutboxEnvelope::decode(&entry.envelope_json) else {
            let sentence = "l'inviluppo salvato non è leggibile";
            let _ = store.reschedule_outbox_append(
                &entry.id,
                self.claim_now(now_ms).saturating_add(backoff_ms(entry.append_attempts.max(1))),
                Some(sentence),
                lease,
            );
            return Err(format!("riga {:?}: {sentence}; la copia in Inviati resta da fare.", entry.id));
        };
        let server = match store.get_account_server(&entry.account_id) {
            Ok(Some(server)) => server,
            Ok(None) => {
                let _ = store.reschedule_outbox_append(
                    &entry.id,
                    self.claim_now(now_ms).saturating_add(backoff_ms(entry.append_attempts.max(1))),
                    Some("l'account non è più configurato"),
                    lease,
                );
                return Err(format!("outbox row {:?}: l'account non è più configurato.", entry.id));
            }
            Err(_) => return Err(format!("outbox row {:?}: la configurazione dell'account non è leggibile.", entry.id)),
        };
        let raw = match store.blobs().get(&entry.raw_blob_hash, MAX_SUBMISSION_BYTES.saturating_add(1024 * 1024)) {
            Ok(raw) => raw,
            Err(_) => {
                let _ = store.reschedule_outbox_append(
                    &entry.id,
                    self.claim_now(now_ms).saturating_add(backoff_ms(entry.append_attempts.max(1))),
                    Some("i byte salvati del messaggio non sono leggibili"),
                    lease,
                );
                return Err(format!("outbox row {:?}: i byte salvati del messaggio non sono leggibili.", entry.id));
            }
        };
        let credential = match self.credential(&entry.account_id, "the Sent append") {
            Ok(credential) => credential,
            Err(_) => {
                let _ = store.reschedule_outbox_append(
                    &entry.id,
                    self.claim_now(now_ms).saturating_add(backoff_ms(entry.append_attempts.max(1))),
                    Some("la credenziale IMAP non è disponibile"),
                    lease,
                );
                return Err(format!("outbox row {:?}: la credenziale IMAP non è disponibile.", entry.id));
            }
        };
        let mut config = crate::oauth::imap_config(&server, credential, self.extra_roots.clone());
        config.limits.max_literal = config.limits.max_literal.max(raw.len().saturating_add(1024)).min(postino_net::Limits::HARD_MAX_LITERAL);
        if postino_sync::append_sent(&config, raw.as_slice()).is_err() {
            // No server text: the IMAP server holds the password and can echo it in a `NO`.
            let sentence = "la copia in Inviati non è stata salvata; il messaggio è già stato inviato e non viene rinviato";
            let _ = store.reschedule_outbox_append(
                &entry.id,
                self.claim_now(now_ms).saturating_add(backoff_ms(entry.append_attempts.max(1))),
                Some(sentence),
                lease,
            );
            return Err(format!("outbox row {:?}: {sentence}.", entry.id));
        }
        if !store.finish_outbox_append(&entry.id, lease).map_err(|_| format!("outbox row {:?}: lo stato non è aggiornabile.", entry.id))? {
            return Err(format!("la riga {:?} è cambiata mentre la copia in Inviati veniva salvata; stato lasciato com'è.", entry.id));
        }
        let _ = self.finalize_outbox_draft(envelope.draft_id.as_deref(), Some(&envelope.message_id), envelope.draft_revision, store);
        Ok(())
    }

    /// Move a draft to Sent only after the submission was accepted. A draft that is gone or already
    /// moved is a no-op, so this is safe to repeat after a crash. `message_id` is the generated
    /// Message-ID, carried into the Sent row so a later sync reconciles instead of duplicating.
    ///
    /// `expected_revision` is the draft revision that was serialized. If the draft changed after the
    /// (already successful) submission, the current message is not moved and not labelled with the
    /// delivered Message-ID: the server copy will arrive with the next sync, and the newer draft stays
    /// a draft. `store` is the connection the caller holds while [`Engine::store`] is temporarily
    /// taken.
    fn finalize_outbox_draft(
        &mut self,
        draft_id: Option<&str>,
        message_id: Option<&str>,
        expected_revision: Option<u32>,
        store: &Store,
    ) -> Result<FinalizeOutcome, OpError> {
        // A rule forward has no local draft: the server Sent copy is the only local trace, and a
        // later sync caches it. There is nothing to move and nothing to label.
        let Some(draft_id) = draft_id else { return Ok(FinalizeOutcome::NotADraft) };
        let Some(summary) = self.summary(draft_id) else {
            return Ok(FinalizeOutcome::NotADraft);
        };
        let Some(folder) = self.folders.iter().find(|folder| folder.id == summary.folder_id) else {
            return Ok(FinalizeOutcome::NotADraft);
        };
        if folder.kind != FolderKind::Drafts {
            return Ok(FinalizeOutcome::NotADraft);
        }
        if let Some(expected) = expected_revision {
            match store.draft_revision(draft_id).map_err(store_map::read_error)? {
                Some(revision) if revision == expected => {}
                _ => return Ok(FinalizeOutcome::SnapshotChanged),
            }
        }
        let (_, server_uid) = self.finish_draft(draft_id, message_id, Some(store))?;
        Ok(FinalizeOutcome::Moved { server_uid })
    }

    /// Local move of a submitted draft to the Sent folder of its own account.
    ///
    /// This is deliberately not an operation any more: `message_send` queues and submits, and the
    /// worker calls this only after the server accepted the message. A store-backed engine persists
    /// the move and keeps the draft revision sequence for a later move back.
    ///
    /// Without a Message-ID (the demo) the row keeps its id. The submission path calls
    /// [`Engine::finish_draft`] with the generated id so the Sent row can adopt the sync identity.
    /// `persist` overrides the engine's own connection; the outbox pass holds the store outside
    /// `self` while it runs, so a finalize there must write through that handle, not skip the disk.
    pub fn send_draft(&mut self, id: &str) -> Result<MessageSummary, OpError> {
        self.finish_draft(id, None, None).map(|(summary, _)| summary)
    }

    /// Move the draft to Sent. Returns the summary and, when a server copy with the same Message-ID
    /// was already cached, the UID it adopted (which means the server already has this message).
    fn finish_draft(&mut self, id: &str, message_id: Option<&str>, persist: Option<&Store>) -> Result<(MessageSummary, Option<u32>), OpError> {
        let current = if let Some(store) = persist {
            // During an outbox pass the store is outside `self`; read the body with that explicit
            // handle instead of the engine cache, which is empty for a reloaded process.
            let (row, attachments) = store
                .get_message_with_attachments(id)
                .map_err(store_map::read_error)?
                .ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}")))?;
            store_map::message(row, attachments)?
        } else {
            self.message_result(id)?.ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}")))?
        };
        let folder = self
            .folders
            .iter()
            .find(|folder| folder.id == current.summary.folder_id)
            .ok_or_else(|| OpError::Failed(format!("the draft {id:?} lives in unknown folder {:?}", current.summary.folder_id)))?;
        if folder.kind != FolderKind::Drafts {
            return Err(OpError::InvalidArgs(format!("message {id:?} is not a draft")));
        }
        if current.summary.to.is_empty() && current.cc.is_empty() && current.bcc.is_empty() {
            return Err(OpError::InvalidArgs("a message needs at least one recipient".to_owned()));
        }
        validate_addresses("to", &current.summary.to)?;
        validate_addresses("cc", &current.cc)?;
        validate_addresses("bcc", &current.bcc)?;
        validate_header("subject", &current.summary.subject, MAX_SUBJECT_BYTES)?;
        let account_id = folder.account_id.clone();
        let sent = self
            .folders
            .iter()
            .find(|folder| folder.account_id == account_id && folder.kind == FolderKind::Sent)
            .map(|folder| folder.id.clone())
            .ok_or_else(|| OpError::Failed(format!("account {account_id:?} has no Sent folder")))?;

        let mut message = current.clone();
        message.summary.folder_id = sent;
        message.summary.date = Utc::now();
        message.summary.unread = false;
        message.summary.starred = false;
        message.summary.snippet = snippet(&message.body_text);
        message.summary.has_attachments = !message.attachments.is_empty();
        // Store the Message-ID in the same normalised form a sync writes, so the reconciliation
        // compares equal strings.
        let stored_message_id = message_id.and_then(postino_sync::normalised_message_id);

        // The pass holds the connection outside `self`, so it is passed in explicitly.
        let store = persist.or(self.store.as_ref());
        // A locally sent message must carry the thread a later sync would derive: its own
        // Message-ID for a root, the parent thread (or phantom parent) for a reply. Otherwise a Sent
        // re-fetch would move it to another id without a merge.
        if let Some(message_id) = stored_message_id.as_deref() {
            let parent = self.reply_links.get(id).cloned();
            message.summary.thread_id = match (parent, store) {
                (Some(parent), Some(store)) => store.thread_id_for_message_id(&account_id, &parent).ok().flatten().unwrap_or(parent),
                (Some(parent), None) => parent,
                (None, Some(store)) => {
                    store.thread_id_for_message_id(&account_id, message_id).ok().flatten().unwrap_or_else(|| message_id.to_owned())
                }
                (None, None) => message_id.to_owned(),
            };
        }
        let mut merged_duplicate = None;
        let mut adopted_uid = None;
        if let Some(store) = store {
            // A provider can auto-save the submitted message, or a sync can fetch it between the
            // server's 250 and this move. If the same Message-ID is already in this Sent folder with
            // a UID, that row is the delivered message: keep the local copy (it is the composed
            // version), adopt the server UID and remove the duplicate in the same transaction
            // instead of showing it twice.
            let duplicate = match stored_message_id.as_deref() {
                Some(message_id) => {
                    store.synced_message_for_id(&account_id, &message.summary.folder_id, message_id).map_err(store_map::read_error)?
                }
                None => None,
            };
            // One transaction: move the display row to Sent and drop the draft row. `draft_seq` is
            // deliberately kept, so a move back to Drafts continues the revision sequence. The
            // generated Message-ID travels with the row so a later Sent sync recognises it.
            let row = store_map::message_row(&message, &account_id, stored_message_id.as_deref());
            adopted_uid = store.send_draft_message(&row, duplicate.as_deref()).map_err(store_map::write_error)?;
            merged_duplicate = duplicate;
        }
        if let Some(duplicate) = &merged_duplicate {
            self.messages.retain(|message| message.id != *duplicate);
            self.rebuild_message_index();
            let prefix = format!("att_{duplicate}_");
            self.blob_hashes.retain(|attachment_id, _| !attachment_id.starts_with(&prefix));
            self.reply_links.remove(duplicate);
            self.draft_snapshots.remove(duplicate);
        }
        self.reply_links.remove(id);
        self.draft_snapshots.remove(id);

        let Some(index) = self.messages.iter().position(|message| message.id == id) else {
            return Err(OpError::Failed(format!("unknown draft {id:?}")));
        };
        let Some(slot) = self.messages.get_mut(index) else {
            return Err(OpError::Failed(format!("unknown draft {id:?}")));
        };
        *slot = message.summary.clone();
        if self.in_memory {
            self.demo_messages.insert(id.to_owned(), message.clone());
        }
        self.bump_revision();
        self.cache_message(&message);
        let summary = message.summary;
        self.recount();
        Ok((summary, adopted_uid))
    }

    /// Call any operation by name with JSON arguments. This is the only door a frontend uses.
    pub fn call(&mut self, name: &str, args: Value) -> Result<Value, OpError> {
        ops::dispatch(self, name, args)
    }

    /// Metadata for one attachment, including its sanitized filename and MIME type. The bytes are
    /// read separately through [`Engine::attachment_bytes`].
    pub fn attachment_metadata(&self, attachment_id: &str) -> Result<Attachment, OpError> {
        self.find_attachment(attachment_id)
    }

    /// Read the stored bytes of one attachment through the encrypted blob store.
    ///
    /// The read is capped at the attachment's recorded size plus [`ATTACHMENT_READ_SLACK`], so a
    /// damaged or swapped blob that declares a huge length is refused before it is decrypted. An
    /// unknown attachment id is a caller mistake; a demo or store-less engine has no bytes at all.
    ///
    /// Consumed by the reading pane (P6) when it previews or saves an attachment.
    pub fn attachment_bytes(&self, attachment_id: &str) -> Result<Vec<u8>, OpError> {
        let store = self.attachment_store()?;
        let attachment = self.find_attachment(attachment_id)?;
        let Some(hash) = self.blob_hashes.get(attachment_id) else {
            return Err(OpError::Failed(format!(
                "attachment {attachment_id:?} has no stored bytes; sync ingests attachment bytes in P5b and the engine only stores bytes a caller hands to store_attachment_bytes"
            )));
        };
        let max_bytes = attachment.size.saturating_add(ATTACHMENT_READ_SLACK);
        let bytes = store.blobs().get(hash, max_bytes).map_err(store_map::blob_read_error)?;
        Ok(bytes.to_vec())
    }

    /// Store the bytes of one attachment of one message in the encrypted blob store.
    ///
    /// The order is repository first, memory second: the blob file is written, then the attachment
    /// row receives the hash and the exact byte length, then memory reflects both. A failed row write
    /// leaves the row and the message exactly as they were; the blob file may stay behind as an
    /// unreferenced, deduplicated file, which is harmless. `has_attachments` never changes here: the
    /// attachment row already exists, so the flag was already true.
    ///
    /// Consumed by sync (P5b) when it ingests real attachments and by the reading pane (P6) when it
    /// saves one.
    pub fn store_attachment_bytes(&mut self, message_id: &str, attachment_id: &str, bytes: &[u8]) -> Result<(), OpError> {
        let store = self.attachment_store()?;
        let size = bytes.len() as u64;
        if size > MAX_ATTACHMENT_BYTES {
            return Err(OpError::InvalidArgs(format!(
                "attachment bytes are {size} bytes; this build stores at most {MAX_ATTACHMENT_BYTES} bytes per attachment"
            )));
        }
        let mut current = self.message_result(message_id)?.ok_or_else(|| unknown_message(message_id))?;
        let Some(attachment) = current.attachments.iter().find(|attachment| attachment.id == attachment_id) else {
            return Err(OpError::InvalidArgs(format!(
                "attachment {attachment_id:?} is not on message {message_id:?}; each attachment id belongs to exactly one message"
            )));
        };
        let _ = attachment;
        let hash = store.blobs().put(bytes).map_err(store_map::blob_write_error)?;
        store.set_attachment_blob_hash(message_id, attachment_id, Some(&hash), size).map_err(store_map::write_error)?;
        // Only now, after both durable writes, does memory change.
        self.blob_hashes.insert(attachment_id.to_owned(), hash);
        if let Some(attachment) = current.attachments.iter_mut().find(|attachment| attachment.id == attachment_id) {
            attachment.size = size;
        }
        if let Some(full) = self.demo_messages.get_mut(message_id)
            && let Some(attachment) = full.attachments.iter_mut().find(|attachment| attachment.id == attachment_id)
        {
            attachment.size = size;
        }
        self.bump_revision();
        self.cache_message(&current);
        Ok(())
    }

    /// The BLAKE3 hash of an attachment's stored bytes, when this engine knows one.
    ///
    /// `Attachment` carries no storage detail (see the `blob_hashes` field), so this is the readable
    /// half of the in-memory mapping. It is not an operation and has no JSON schema.
    pub fn attachment_hash(&self, attachment_id: &str) -> Option<&str> {
        self.blob_hashes.get(attachment_id).map(String::as_str)
    }

    /// The parent identity for a reply: its `Message-ID` and `Reply-To` list.
    ///
    /// A reply is only useful if the composer can thread it and honour the sender's requested
    /// mailbox. The store has the `Message-ID`; `Reply-To` lives in the existing `meta` table because
    /// P8 may not migrate the message schema. The demo has deterministic ids and no Reply-To.
    pub fn reply_target(&self, message_id: &str) -> Result<ReplyTarget, OpError> {
        if self.summary(message_id).is_none() {
            return Err(unknown_message(message_id));
        }
        match &self.store {
            Some(store) => {
                let parent = store.message_header_id(message_id).map_err(store_map::read_error)?;
                let reply_to = store.message_reply_to(message_id).map_err(store_map::read_error)?;
                Ok(ReplyTarget { parent_message_id: parent, reply_to: reply_to.iter().map(store_map::address).collect() })
            }
            None => Ok(ReplyTarget { parent_message_id: Some(format!("{message_id}@demo.postino.example")), reply_to: Vec::new() }),
        }
    }

    /// The invite card for one message, or `None` when it has no readable `text/calendar` part.
    ///
    /// The part is located by declared type or `.ics` filename, its size is capped before any read,
    /// and the bytes go through the existing hash-checked blob seam. A malformed or hostile
    /// calendar falls back to a normal attachment: it is not an error and it never panics.
    pub fn message_invite(&self, id: &str) -> Result<Option<CalendarInviteView>, OpError> {
        Ok(self.read_invite(id)?.as_ref().map(invite_view))
    }

    fn read_invite(&self, id: &str) -> Result<Option<postino_sync::CalendarInvite>, OpError> {
        let message = self.message_result(id)?.ok_or_else(|| unknown_message(id))?;
        let Some(attachment) = message.attachments.iter().find(|attachment| is_calendar_part(attachment)) else {
            return Ok(None);
        };
        if attachment.size > postino_sync::MAX_CALENDAR_BYTES as u64 {
            return Ok(None);
        }
        let Ok(bytes) = self.attachment_bytes(&attachment.id) else {
            return Ok(None);
        };
        if bytes.len() > postino_sync::MAX_CALENDAR_BYTES {
            return Ok(None);
        }
        let Some(mut invite) = postino_sync::parse_invite(&bytes).ok() else {
            return Ok(None);
        };
        // A copy in Sent or Drafts is an outgoing record, not an invitation to answer: the card stays
        // readable and the three actions stay disabled.
        if self
            .folders
            .iter()
            .find(|folder| folder.id == message.summary.folder_id)
            .is_some_and(|folder| matches!(folder.kind, FolderKind::Sent | FolderKind::Drafts))
        {
            invite.can_reply = false;
        }
        Ok(Some(invite))
    }

    /// Create a draft that answers an invite. It is intentionally narrow: only the organizer is a
    /// recipient, nothing is sent, and no calendar is updated. The person still edits and sends it.
    pub fn invite_reply_draft(&mut self, message_id: &str, response: InviteResponse) -> Result<Message, OpError> {
        let message = self.message_result(message_id)?.ok_or_else(|| unknown_message(message_id))?;
        let invite = self.read_invite(message_id)?.ok_or_else(|| {
            OpError::InvalidArgs(format!(
                "message {message_id:?} has no readable text/calendar invite; the attachment stays available and no reply is drafted"
            ))
        })?;
        if !invite.valid {
            let reason = invite.invalid_reason.as_deref().unwrap_or("the event is not valid");
            return Err(OpError::InvalidArgs(format!("the invite cannot be answered: {reason}")));
        }
        let outgoing = self
            .folders
            .iter()
            .find(|folder| folder.id == message.summary.folder_id)
            .is_some_and(|folder| matches!(folder.kind, FolderKind::Sent | FolderKind::Drafts));
        if outgoing {
            return Err(OpError::InvalidArgs("only a received invitation can be answered; a copy in Sent or Drafts is informational".to_owned()));
        }
        if invite.cancelled || invite.method != "REQUEST" {
            return Err(OpError::InvalidArgs(
                "only a valid METHOD:REQUEST with an organizer can be answered; CANCEL and REPLY are informational".to_owned(),
            ));
        }
        let organizer =
            invite.organizer.clone().ok_or_else(|| OpError::InvalidArgs("the invite has no organizer; there is nobody to answer".to_owned()))?;
        if !invite.can_reply {
            return Err(OpError::InvalidArgs(
                "only a valid METHOD:REQUEST with an organizer can be answered; CANCEL and REPLY are informational".to_owned(),
            ));
        }
        let account = self
            .folders
            .iter()
            .find(|folder| folder.id == message.summary.folder_id)
            .and_then(|folder| self.accounts.iter().find(|account| account.id == folder.account_id))
            .cloned()
            .ok_or_else(|| OpError::Failed(format!("message {message_id:?} is not in a folder of a known account")))?;
        let parent = self.reply_target(message_id)?.parent_message_id;
        let subject = format!("{}: {}", response.subject_prefix(), invite.summary);
        let body = invite_reply_body(&invite, response);
        let draft = DraftContent {
            to: vec![Address { name: String::new(), email: organizer }],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject,
            body_text: body,
            attachments: Vec::new(),
            in_reply_to: parent,
        };
        let saved = self.save_draft(Some(&account.id), None, draft)?;
        // The reply part needs a store blob. The demo engine has synthetic mail only, so it keeps the
        // text draft; a stored account always gets the METHOD:REPLY attachment.
        if self.store.is_some() {
            let calendar = postino_sync::reply_calendar(&invite, &account.email, response.partstat(), Utc::now());
            return self.add_draft_attachment(&saved.summary.id, "invito.ics", Some("text/calendar"), calendar.as_bytes());
        }
        Ok(saved)
    }

    /// Add one attachment with its exact bytes to a draft.
    ///
    /// The filename is sanitized with the same rule as the serializer, a duplicate is renamed
    /// (`name (2).ext`), the declared MIME is validated and falls back to a conservative extension
    /// table, and the caps are checked before the blob is written. The blob is written first and the
    /// draft row, the attachment rows and the revision are one store transaction; a failed row write
    /// leaves an unreferenced blob, never a row that claims bytes it does not have.
    pub fn add_draft_attachment(&mut self, draft_id: &str, filename: &str, mime: Option<&str>, data: &[u8]) -> Result<Message, OpError> {
        let store = self.require_store("draft_attachment_add")?;
        let index = self.draft_index(draft_id, "allegato")?;
        let current = self.message_result(draft_id)?.ok_or_else(|| unknown_message(draft_id))?;
        self.ensure_draft_current(&current)?;
        if filename.chars().any(|character| matches!(character, '\r' | '\n' | '\0')) {
            return Err(OpError::InvalidArgs(format!("attachment file name {filename:?} contains a line break or NUL; header injection is refused")));
        }
        let sanitized = postino_sync::sanitize_filename(filename);
        if sanitized.is_empty() {
            return Err(OpError::InvalidArgs(format!(
                "attachment file name {filename:?} is empty after sanitization; it must not be a path, a dot name or only controls"
            )));
        }
        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if size > MAX_ATTACHMENT_BYTES {
            return Err(OpError::InvalidArgs(format!(
                "attachment {sanitized:?} is {size} bytes; this build accepts at most {MAX_ATTACHMENT_BYTES} bytes per file"
            )));
        }
        if current.attachments.len() >= MAX_DRAFT_ATTACHMENTS {
            return Err(OpError::InvalidArgs(format!(
                "a draft can carry at most {MAX_DRAFT_ATTACHMENTS} attachments; remove one before adding {sanitized:?}"
            )));
        }
        let total = current.attachments.iter().map(|attachment| attachment.size).sum::<u64>();
        if total.saturating_add(size) > MAX_ATTACHMENT_TOTAL_BYTES {
            return Err(OpError::InvalidArgs(format!(
                "attachments would total {} bytes; this build accepts at most {MAX_ATTACHMENT_TOTAL_BYTES} bytes per message (adding {sanitized:?})",
                total.saturating_add(size)
            )));
        }
        let filename = unique_attachment_name(&sanitized, &current.attachments);
        let derived = postino_sync::mime_for_filename(&filename);
        // The declared type is a hint, never a promise: accept it only when it agrees with the
        // extension table, otherwise the extension wins. A caller cannot turn `evil.exe` into
        // `image/png` or `text/html`.
        let mime = mime.and_then(postino_sync::normalize_mime_type).filter(|declared| declared == derived).unwrap_or_else(|| derived.to_owned());
        let attachment_id = next_attachment_id(draft_id, &current.attachments);
        let hash = store.blobs().put(data).map_err(store_map::blob_write_error)?;
        let rows = self.attachment_rows(
            current.attachments.iter().cloned().chain(std::iter::once(Attachment { id: attachment_id.clone(), filename, mime, size })),
            Some((&attachment_id, &hash)),
        );
        let mut saved = current.clone();
        saved.attachments = rows.iter().cloned().map(store_map::attachment).collect();
        saved.summary.has_attachments = true;
        saved.summary.date = Utc::now();
        let link = self.reply_links.get(draft_id).cloned();
        let row = store_map::message_row(&saved, &self.draft_account_id(draft_id)?, None);
        let saved_draft = store.save_draft_message_linked(&row, &rows, link.as_deref()).map_err(store_map::write_error)?;
        // Durable rows are written; only now does memory change.
        self.blob_hashes.insert(attachment_id, hash);
        if let Some(slot) = self.messages.get_mut(index) {
            *slot = saved.summary.clone();
        }
        if self.in_memory {
            self.demo_messages.insert(saved.summary.id.clone(), saved.clone());
        }
        self.remember_draft_revision(&saved.summary.id, saved_draft.revision);
        self.bump_revision();
        self.cache_message(&saved);
        self.recount();
        Ok(saved)
    }

    /// Remove one attachment row from a draft. The deduplicated blob stays until a later GC.
    pub fn remove_draft_attachment(&mut self, draft_id: &str, attachment: &str) -> Result<Message, OpError> {
        let store = self.require_store("draft_attachment_remove")?;
        let index = self.draft_index(draft_id, "allegato")?;
        let current = self.message_result(draft_id)?.ok_or_else(|| unknown_message(draft_id))?;
        self.ensure_draft_current(&current)?;
        let position = current
            .attachments
            .iter()
            .position(|candidate| candidate.id == attachment)
            .or_else(|| current.attachments.iter().position(|candidate| candidate.filename == attachment))
            .ok_or_else(|| {
                OpError::InvalidArgs(format!(
                    "attachment {attachment:?} is not on draft {draft_id:?}; use an id or a filename returned by message_get"
                ))
            })?;
        let removed = current.attachments.get(position).cloned().ok_or_else(|| OpError::Failed("the attachment disappeared".to_owned()))?;
        let mut saved = current.clone();
        saved.attachments.remove(position);
        saved.summary.has_attachments = !saved.attachments.is_empty();
        saved.summary.date = Utc::now();
        let rows = self.attachment_rows(saved.attachments.iter().cloned(), None);
        let link = self.reply_links.get(draft_id).cloned();
        let row = store_map::message_row(&saved, &self.draft_account_id(draft_id)?, None);
        let saved_draft = store.save_draft_message_linked(&row, &rows, link.as_deref()).map_err(store_map::write_error)?;
        self.blob_hashes.remove(&removed.id);
        if let Some(slot) = self.messages.get_mut(index) {
            *slot = saved.summary.clone();
        }
        if self.in_memory {
            self.demo_messages.insert(saved.summary.id.clone(), saved.clone());
        }
        self.remember_draft_revision(&saved.summary.id, saved_draft.revision);
        self.bump_revision();
        self.cache_message(&saved);
        self.recount();
        Ok(saved)
    }

    /// Copy one attachment from a message to a draft, reusing the encrypted blob.
    ///
    /// Forwarding never re-encodes the bytes in a JSON payload: the engine reads the source through
    /// the same capped blob path as the reading pane and writes the new row. A missing or corrupt
    /// source blob is refused with the source filename, never silently dropped.
    pub fn copy_attachment_to_draft(&mut self, draft_id: &str, source_attachment: &str) -> Result<Message, OpError> {
        let source = self.find_attachment(source_attachment)?;
        let data = self.attachment_bytes(source_attachment).map_err(|error| {
            OpError::Failed(format!("attachment {:?} cannot be copied ({error}); the draft keeps its other attachments", source.filename))
        })?;
        self.add_draft_attachment(draft_id, &source.filename, Some(&source.mime), &data)
    }

    /// Set the plain-text signature of one account.
    ///
    /// The signature is body text, not a header and not an attachment: up to 4 KiB, newline and tab
    /// only (no CR), and it is applied to new messages only because an existing draft already owns
    /// the text it saved.
    pub fn set_account_signature(&mut self, account_id: &str, signature: &str) -> Result<AccountView, OpError> {
        let index = self.accounts.iter().position(|account| account.id == account_id).ok_or_else(|| unknown_account(account_id))?;
        validate_signature(signature)?;
        if let Some(store) = &self.store
            && !store.set_account_signature(account_id, signature).map_err(store_map::write_error)?
        {
            return Err(unknown_account(account_id));
        }
        let account = self.accounts.get_mut(index).ok_or_else(|| unknown_account(account_id))?;
        account.signature = signature.to_owned();
        let account = account.clone();
        self.account_view(&account)
    }

    /// The store rows for a draft's attachments, with the known blob hashes (and one new hash).
    fn attachment_rows(&self, attachments: impl IntoIterator<Item = Attachment>, added: Option<(&str, &str)>) -> Vec<postino_store::Attachment> {
        attachments
            .into_iter()
            .map(|attachment| postino_store::Attachment {
                blob_hash: match added {
                    Some((id, hash)) if id == attachment.id => Some(hash.to_owned()),
                    _ => self.blob_hashes.get(&attachment.id).cloned(),
                },
                id: attachment.id,
                filename: attachment.filename,
                mime: attachment.mime,
                size: attachment.size,
            })
            .collect()
    }

    /// One draft's index in memory, with the same folder and outbox checks every edit uses.
    fn draft_index(&self, draft_id: &str, action: &str) -> Result<usize, OpError> {
        let index = self
            .message_index
            .get(draft_id)
            .copied()
            .ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {draft_id:?}; omit id to create a draft")))?;
        let kind =
            self.messages.get(index).and_then(|message| self.folders.iter().find(|folder| folder.id == message.folder_id)).map(|folder| folder.kind);
        if kind != Some(FolderKind::Drafts) {
            return Err(OpError::InvalidArgs(format!("message {draft_id:?} is not a draft")));
        }
        if let Some(state) = self.outbox_lock_for(draft_id, Some(FolderKind::Drafts))? {
            return Err(OpError::InvalidArgs(outbox_lock_message("draft", draft_id, state, action)));
        }
        Ok(index)
    }

    /// Refuse an attachment edit when the store moved on since this engine loaded the draft.
    ///
    /// The store is the authority: another process may have saved a newer body, replaced the
    /// attachment set, sent the draft or moved it. Writing this engine's stale snapshot would
    /// overwrite that work or resurrect a Sent row in Drafts. The same snapshot comparison used by
    /// `message_send` makes the answer precise.
    fn ensure_draft_current(&self, current: &Message) -> Result<(), OpError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let changed = || draft_changed(&current.summary.id);
        let row = store.get_message(&current.summary.id).map_err(store_map::read_error)?.ok_or_else(&changed)?;
        if row.summary.folder_id != current.summary.folder_id {
            return Err(draft_changed(&current.summary.id));
        }
        let revision = store.draft_revision(&current.summary.id).map_err(store_map::read_error)?.ok_or_else(&changed)?;
        if self.draft_snapshots.get(&current.summary.id).is_some_and(|snapshot| *snapshot != revision) {
            return Err(draft_changed(&current.summary.id));
        }
        Ok(())
    }

    fn draft_account_id(&self, draft_id: &str) -> Result<String, OpError> {
        let folder_id = self.summary(draft_id).map(|message| message.folder_id.clone()).ok_or_else(|| unknown_message(draft_id))?;
        self.folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .map(|folder| folder.account_id.clone())
            .ok_or_else(|| OpError::Failed(format!("draft {draft_id:?} lives in an unknown folder")))
    }

    /// The store, or a readable reason why this engine cannot touch attachment bytes.
    fn attachment_store(&self) -> Result<&Store, OpError> {
        match &self.store {
            Some(store) => Ok(store),
            None if self.demo => Err(OpError::Failed(
                "the demo mailbox has no attachment bytes; open a local store (postino --store <path>) to use attachments".to_owned(),
            )),
            None => Err(OpError::Failed("this engine has no local store, so attachment bytes are unavailable; open it with Engine::open".to_owned())),
        }
    }

    /// One attachment by its globally unique id, with a readable error when it does not exist.
    fn find_attachment(&self, attachment_id: &str) -> Result<Attachment, OpError> {
        {
            let cache = self.body_cache.borrow();
            if let Some(found) = cache
                .iter()
                .filter(|(revision, _)| *revision == self.revision)
                .flat_map(|(_, message)| message.attachments.iter())
                .find(|attachment| attachment.id == attachment_id)
            {
                return Ok(found.clone());
            }
        }
        if let Some(found) =
            self.demo_messages.values().flat_map(|message| message.attachments.iter()).find(|attachment| attachment.id == attachment_id)
        {
            return Ok(found.clone());
        }
        if let Some(store) = &self.store
            && let Some((_, attachment)) = store.find_attachment(attachment_id).map_err(store_map::read_error)?
        {
            return Ok(store_map::attachment(attachment));
        }
        Err(OpError::InvalidArgs(format!("unknown attachment {attachment_id:?}; attachment ids come from message_get")))
    }

    /// The operation catalogue, with JSON Schemas. CLI and MCP share it.
    pub fn tools(&self) -> Vec<OpDef> {
        ops::op_defs()
    }

    /// The platform data directory. The CLI's `--store` path is explicit until onboarding (P6)
    /// chooses the default location; this only reports where Postino keeps its data.
    pub fn data_dir() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().join("Postino"))
    }

    fn next_id(&self, prefix: &str) -> Result<String, OpError> {
        let base = self.messages.len().saturating_add(1);
        for offset in 0..10_000 {
            let candidate = format!("{prefix}_{}", base.saturating_add(offset));
            if !self.message_index.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
        Err(OpError::Failed(format!("could not allocate a new {prefix} id")))
    }

    pub(crate) fn recount(&mut self) {
        for folder in &mut self.folders {
            let (total, unread) = if folder.kind == FolderKind::Starred {
                self.messages.iter().filter(|message| message.starred).fold((0u32, 0u32), |(total, unread), message| {
                    (total.saturating_add(1), unread.saturating_add(if message.unread { 1 } else { 0 }))
                })
            } else {
                self.messages.iter().filter(|message| message.folder_id == folder.id).fold((0u32, 0u32), |(total, unread), message| {
                    (total.saturating_add(1), unread.saturating_add(if message.unread { 1 } else { 0 }))
                })
            };
            folder.total = total;
            folder.unread = unread;
        }
    }
}

/// Hash an address into a stable local account id. The same address always maps to the same id,
/// which makes a retry after a failed add reuse one keyring scope instead of leaving a second secret
/// behind. The address is case-folded for the id; the stored address keeps the caller's spelling.
fn account_id(email: &str) -> String {
    let digest = blake3::hash(email.trim().to_lowercase().as_bytes()).to_hex();
    let text = digest.as_str();
    format!("acc_{}", text.get(..16).unwrap_or(text))
}

/// Validate and normalise an account address. It must be one bare mailbox, not a display name or a
/// list: the login name is the address.
fn validate_account_email(raw: &str) -> Result<String, OpError> {
    let email = raw.trim();
    if email.is_empty() {
        return Err(OpError::InvalidArgs("email must not be empty".to_owned()));
    }
    if email.len() > 320 {
        return Err(OpError::InvalidArgs("email is longer than 320 bytes".to_owned()));
    }
    if email.chars().any(|character| character.is_control() || character.is_whitespace()) || email.contains(['<', '>', ',', ';']) {
        return Err(OpError::InvalidArgs(format!("{email:?} is not a bare email address; use one address without a display name")));
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err(OpError::InvalidArgs(format!("{email:?} is not a valid email address; it needs one @")));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(OpError::InvalidArgs(format!("{email:?} is not a valid email address")));
    }
    Ok(email.to_owned())
}

/// A password or app password: non-empty, bounded and free of control characters. Leading and
/// trailing spaces are legal and are never trimmed. The value is never included in an error.
fn validate_account_password(password: &str) -> Result<(), OpError> {
    if password.is_empty() {
        return Err(OpError::InvalidArgs("password must not be empty".to_owned()));
    }
    if password.len() > 1024 {
        return Err(OpError::InvalidArgs("password is longer than 1024 bytes".to_owned()));
    }
    if password.chars().any(char::is_control) {
        return Err(OpError::InvalidArgs("password must not contain control characters".to_owned()));
    }
    Ok(())
}

/// The display name: trimmed, 1-256 characters, no control characters; omitted means the local part.
fn validate_account_name(name: Option<&str>, email: &str) -> Result<String, OpError> {
    let fallback = email.split('@').next().unwrap_or(email);
    let name = match name {
        Some(name) if !name.trim().is_empty() => name.trim(),
        _ => fallback,
    };
    if name.chars().count() > 256 {
        return Err(OpError::InvalidArgs("name is longer than 256 characters".to_owned()));
    }
    if name.chars().any(char::is_control) {
        return Err(OpError::InvalidArgs("name must not contain control characters".to_owned()));
    }
    Ok(name.to_owned())
}

/// Resolve the offline provider table against explicit settings. Known providers own their endpoint;
/// `custom` requires host, port and TLS and has no defaults.
fn resolve_server(provider: Provider, host: Option<&str>, port: Option<u16>, tls: Option<MailTls>) -> Result<(String, u16, MailTls), OpError> {
    match provider.discovery() {
        Some(discovery) => {
            if host.is_some() || port.is_some() || tls.is_some() {
                return Err(OpError::InvalidArgs(format!(
                    "provider {} uses {}:{} with {} TLS; omit host, port and tls, or use provider custom",
                    provider.as_str(),
                    discovery.host,
                    discovery.port,
                    discovery.tls.as_str()
                )));
            }
            Ok((discovery.host.to_owned(), discovery.port, discovery.tls))
        }
        None => {
            let host = host.ok_or_else(|| OpError::InvalidArgs("provider custom requires host, port and tls".to_owned()))?;
            let port = port.ok_or_else(|| OpError::InvalidArgs("provider custom requires host, port and tls".to_owned()))?;
            let tls = tls.ok_or_else(|| OpError::InvalidArgs("provider custom requires host, port and tls".to_owned()))?;
            let host = validate_host(host)?;
            if port == 0 {
                return Err(OpError::InvalidArgs("port must be between 1 and 65535".to_owned()));
            }
            Ok((host, port, tls))
        }
    }
}

fn validate_host(raw: &str) -> Result<String, OpError> {
    let host = raw.trim();
    if host.is_empty() || host.len() > 255 {
        return Err(OpError::InvalidArgs("host must be 1-255 characters".to_owned()));
    }
    if host.chars().any(|character| character.is_control() || character.is_whitespace()) || host.contains(['/', '@']) {
        return Err(OpError::InvalidArgs(format!("{host:?} is not a host name or address")));
    }
    Ok(host.to_owned())
}

/// Resolve the SMTP endpoint from the provider table and the caller's fields.
///
/// Known providers own their endpoint and login: a caller may omit everything and get the preset.
/// `custom` has no default; supplying only part of the endpoint is an error instead of a guess.
fn resolve_smtp(
    provider: Provider,
    host: Option<&str>,
    port: Option<u16>,
    tls: Option<MailTls>,
    username: Option<&str>,
    email: &str,
) -> Result<Option<SmtpSettings>, OpError> {
    match provider.smtp_discovery() {
        Some(discovery) => {
            let differs = host.is_some_and(|host| !host.trim().eq_ignore_ascii_case(discovery.host))
                || port.is_some_and(|port| port != discovery.port)
                || tls.is_some_and(|tls| tls != discovery.tls);
            if differs {
                return Err(OpError::InvalidArgs(format!(
                    "provider {} submits through {}:{} with {} TLS; omit smtp_host, smtp_port and smtp_tls, or use provider custom",
                    provider.as_str(),
                    discovery.host,
                    discovery.port,
                    discovery.tls.as_str()
                )));
            }
            if let Some(username) = username
                && !username.trim().eq_ignore_ascii_case(email)
            {
                return Err(OpError::InvalidArgs(format!(
                    "provider {} uses the account address as its SMTP login; omit smtp_username or pass {email:?}",
                    provider.as_str()
                )));
            }
            Ok(Some(SmtpSettings { host: discovery.host.to_owned(), port: discovery.port, tls: discovery.tls, username: email.to_owned() }))
        }
        None => {
            if host.is_none() && port.is_none() && tls.is_none() {
                return Ok(None);
            }
            let host =
                host.ok_or_else(|| OpError::InvalidArgs("a custom SMTP endpoint requires smtp_host, smtp_port and smtp_tls together".to_owned()))?;
            let port =
                port.ok_or_else(|| OpError::InvalidArgs("a custom SMTP endpoint requires smtp_host, smtp_port and smtp_tls together".to_owned()))?;
            let tls =
                tls.ok_or_else(|| OpError::InvalidArgs("a custom SMTP endpoint requires smtp_host, smtp_port and smtp_tls together".to_owned()))?;
            if port == 0 {
                return Err(OpError::InvalidArgs("smtp_port must be between 1 and 65535".to_owned()));
            }
            let username = username.unwrap_or(email).trim().to_owned();
            if username.is_empty() || username.len() > 320 || username.chars().any(char::is_control) {
                return Err(OpError::InvalidArgs("smtp_username must be 1-320 characters with no control characters".to_owned()));
            }
            Ok(Some(SmtpSettings { host: validate_host(host)?, port, tls, username }))
        }
    }
}

fn mailbox(address: &Address) -> postino_sync::SubmissionMailbox {
    postino_sync::SubmissionMailbox { name: (!address.name.trim().is_empty()).then(|| address.name.clone()), address: address.email.clone() }
}

/// Reduce an SMTP failure to a bounded, fixed sentence for the outbox row, the CLI and the UI.
///
/// **No server text, ever.** The server has just received the password and the whole body, so a
/// `550`, a `451` or a `535` may echo either one; the sentence names the stage, the code and what a
/// person can do, and nothing the peer chose. The same rule applies to [`SmtpError::Auth`], whose
/// reason is built by the network client without server text.
fn smtp_stage_sentence(stage: &str) -> &'static str {
    match stage {
        "mail" | "mail_from" => "il mittente",
        "rcpt" => "il destinatario",
        "data" => "il contenuto del messaggio",
        "auth" | "login" => "l'accesso al server",
        _ => "la connessione al server",
    }
}

fn smtp_error_sentence(error: &SmtpError) -> String {
    match error {
        SmtpError::Uncertain { .. } => "l'esito è sconosciuto: controlla Inviati prima di riprovare".to_owned(),
        SmtpError::Auth { .. } => "il server ha rifiutato la credenziale SMTP; aggiorna la password e riprova".to_owned(),
        SmtpError::TooLarge { .. } => "il messaggio è più grande di quanto il server accetti".to_owned(),
        SmtpError::Unsupported { .. } => "il server non offre un'estensione SMTP necessaria".to_owned(),
        SmtpError::Timeout { .. } => "il server non ha risposto in tempo".to_owned(),
        SmtpError::Closed { .. } => "la connessione si è chiusa prima della conferma del server".to_owned(),
        SmtpError::Tls { .. } => "la connessione TLS con il server SMTP è fallita".to_owned(),
        SmtpError::Dns { .. } => "il nome del server SMTP non è stato risolto".to_owned(),
        SmtpError::Connect { .. } => "il server SMTP non è raggiungibile".to_owned(),
        SmtpError::Rejected { code, stage, .. } => format!("il server ha rifiutato {} con il codice {code}", smtp_stage_sentence(stage)),
        SmtpError::Transient { stage, .. } => format!("{} non ha risposto: riprova più tardi", smtp_stage_sentence(stage)),
        SmtpError::Permanent { stage, .. } => format!("{} è stato rifiutato in modo definitivo", smtp_stage_sentence(stage)),
        SmtpError::Protocol { .. } => "lo scambio SMTP non può proseguire".to_owned(),
    }
}

/// The sentence for a worker that lost its lease: another process owns the row now.
/// The sentence for a worker that lost its lease: another process owns the row now.
fn lost_lease_sentence(entry: &OutboxEntry) -> String {
    format!("la riga {:?} è cambiata in un altro processo; questo passaggio non l'ha sovrascritta.", entry.id)
}

/// Why a leased submission must not leave the machine: the draft it was built from is gone or has a
/// different store revision. `None` when the row predates the snapshot field or still matches.
fn stale_submission_reason(store: &Store, envelope: &OutboxEnvelope) -> Result<Option<&'static str>, OpError> {
    let (Some(draft_id), Some(expected)) = (envelope.draft_id.as_deref(), envelope.draft_revision) else {
        return Ok(None);
    };
    match store.get_draft(draft_id).map_err(store_map::read_error)? {
        None => Ok(Some("la bozza non esiste più; il messaggio non è stato inviato")),
        Some(draft) if draft.revision != expected => Ok(Some("la bozza è cambiata dopo l'accodamento; il messaggio non è stato inviato")),
        Some(_) => Ok(None),
    }
}

/// A fixed sentence for an SMTP probe failure. Server-supplied text is never forwarded: a hostile
/// server has just received the password and can echo it.
fn smtp_failure_reason(error: &postino_net::SmtpError) -> &'static str {
    match error {
        postino_net::SmtpError::Auth { .. } => "the server rejected the password",
        postino_net::SmtpError::Tls { .. } => "TLS verification failed",
        postino_net::SmtpError::Dns { .. } => "the host could not be resolved",
        postino_net::SmtpError::Connect { .. } => "the connection was refused",
        postino_net::SmtpError::Timeout { .. } => "the server did not answer in time",
        postino_net::SmtpError::Unsupported { .. } => "the server does not offer a required extension",
        postino_net::SmtpError::Rejected { .. } => "the server refused the login",
        postino_net::SmtpError::Closed { .. } => "the connection closed",
        postino_net::SmtpError::TooLarge { .. } => "the server's limits are too small",
        postino_net::SmtpError::Uncertain { .. } => "the outcome is unknown",
        postino_net::SmtpError::Transient { .. } | postino_net::SmtpError::Permanent { .. } | postino_net::SmtpError::Protocol { .. } => {
            "the exchange could not be completed"
        }
    }
}

/// Turn a sync/probe failure into an actionable operation error with a tail that states what was and
/// was not written.
///
/// **No server-originated text is forwarded.** A hostile server has just received the credential and
/// can repeat it (or the SASL payload) in a tagged `NO`/`BAD` or a malformed response; the sentence
/// names the provider, account id, configured host and a fixed phase instead. `Auth` keeps the
/// provider advice but not the server's reason.
/// A redacted phrase for a keyring cleanup result: `Ok`, or the adapter's sentence (which never
/// contains secret bytes).
fn credential_outcome(result: &Result<(), crate::vault::VaultError>) -> String {
    match result {
        Ok(()) => "removed".to_owned(),
        Err(error) => format!("not removed: {error}"),
    }
}

fn account_error(error: SyncError, provider: Provider, account_id: &str, host: &str, phase: &'static str, tail: &str) -> OpError {
    match error {
        SyncError::Auth(_) => OpError::Auth(format!(
            "the {} server {host} rejected the credential for account {account_id:?} while authenticating. {}. {tail}",
            provider.as_str(),
            provider.login_advice()
        )),
        SyncError::Connect(source) => OpError::Network(format!(
            "could not talk to the {} server {host} for account {account_id:?} while {}. {tail}",
            provider.as_str(),
            net_phase(&source)
        )),
        SyncError::Protocol(_) => OpError::Failed(format!(
            "the {} server {host} sent a response this client could not use for account {account_id:?} while {phase}. {tail}",
            provider.as_str()
        )),
        SyncError::Store(error) => OpError::Failed(format!("sync could not use the local store: {error}. {tail}")),
        SyncError::Cancelled => OpError::Failed("the sync was cancelled; messages written before the stop are kept".to_owned()),
        SyncError::NoAccount(id) => OpError::Failed(format!("account {id:?} is not in the local store")),
    }
}

/// A fixed phase for a network failure. Never includes the server's response text.
fn net_phase(error: &NetError) -> &'static str {
    match error {
        NetError::Dns { .. } => "resolving the server name",
        NetError::Connect { .. } => "opening the connection",
        NetError::Tls { .. } => "the TLS handshake",
        NetError::Auth { .. } => "authenticating",
        NetError::Timeout { phase, .. } | NetError::Closed { phase } => phase_name(phase),
        NetError::Protocol { .. } => "reading a response",
        NetError::ServerNo { command, .. } | NetError::ServerBad { command, .. } => command_phase(command),
        NetError::TooLarge { .. } => "reading a response that exceeded a bound",
        NetError::Unsupported { .. } => "negotiating a required capability",
        NetError::OAuthConsent { .. } => "waiting for the browser authorization",
        NetError::OAuthRejected { .. } | NetError::OAuthGrant { .. } => "refreshing the OAuth token",
        NetError::OAuthToken { .. } => "reading the OAuth token response",
    }
}

fn phase_name(phase: &str) -> &'static str {
    match phase {
        "dns" => "resolving the server name",
        "connect" => "opening the connection",
        "tls" => "the TLS handshake",
        "greeting" => "waiting for the greeting",
        "capability" => "reading capabilities",
        "auth" | "login" => "authenticating",
        "list" => "listing folders",
        "select" | "examine" => "opening a folder",
        "fetch" | "fetch flags" | "fetch body" => "fetching mail",
        "store" => "changing flags",
        "idle" => "waiting for new mail",
        "logout" => "closing the connection",
        _ => "a network step",
    }
}

fn command_phase(command: &str) -> &'static str {
    // The network crate passes the wire verb (uppercase) and sometimes "UID FETCH"; match both.
    match command.to_ascii_lowercase().as_str() {
        "list" => "listing folders",
        "select" | "examine" => "opening a folder",
        "uid fetch" | "fetch" => "fetching mail",
        "uid store" | "store" => "changing flags",
        "capability" => "reading capabilities",
        "authenticate" | "login" => "authenticating",
        "logout" => "closing the connection",
        _ => "running a command",
    }
}

/// Finish a sync so a reload failure cannot hide the sync cause.
///
/// On success a reload error is the operation error (there is no cause to preserve). On failure the
/// sync error wins and the reload failure is appended to it, so a person or agent sees the auth,
/// network or store reason that actually stopped the run.
fn finish_sync(result: Result<postino_sync::SyncOutcome, OpError>, reload: Result<(), OpError>) -> Result<postino_sync::SyncOutcome, OpError> {
    match result {
        Ok(outcome) => {
            reload?;
            Ok(outcome)
        }
        Err(error) => match reload {
            Ok(()) => Err(error),
            Err(reload) => Err(with_reload_note(error, reload)),
        },
    }
}

fn with_reload_note(error: OpError, reload: OpError) -> OpError {
    let note = format!(" The local cache could not be reloaded after the failed run: {reload}");
    match error {
        OpError::UnknownOp(message) => OpError::UnknownOp(format!("{message}{note}")),
        OpError::InvalidArgs(message) => OpError::InvalidArgs(format!("{message}{note}")),
        OpError::Auth(message) => OpError::Auth(format!("{message}{note}")),
        OpError::Network(message) => OpError::Network(format!("{message}{note}")),
        OpError::Failed(message) => OpError::Failed(format!("{message}{note}")),
    }
}

/// Materialise every `drafts` row that has no display row, in one store transaction, before the
/// mailbox is hydrated. Unknown accounts/folders and unreadable dates fail the open with the reason.
fn repair_orphan_drafts(store: &Store, accounts: &[Account], folders: &[Folder]) -> Result<(), OpError> {
    let drafts = store.list_orphan_drafts().map_err(store_map::read_error)?;
    if drafts.is_empty() {
        return Ok(());
    }
    let mut messages = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let account = accounts
            .iter()
            .find(|account| account.id == draft.account_id)
            .ok_or_else(|| OpError::Failed(format!("the local store has draft {:?} for unknown account {:?}", draft.id, draft.account_id)))?;
        let folder_id = folders
            .iter()
            .find(|folder| folder.account_id == draft.account_id && folder.kind == FolderKind::Drafts)
            .map(|folder| folder.id.clone())
            .ok_or_else(|| {
                OpError::Failed(format!("the local store has draft {:?} but no Drafts folder for account {:?}", draft.id, draft.account_id))
            })?;
        let message = store_map::draft_message(&draft, account, &folder_id)?;
        messages.push(store_map::message_row(&message, &account.id, None));
    }
    store.repair_orphan_drafts(&messages).map_err(store_map::repair_error)?;
    Ok(())
}

/// Read the persisted label high-water mark, raise it to whatever labels already exist and repair an
/// older store that has no `label_seq` row. The returned number is the highest id ever allocated.
fn repair_label_seq(store: &Store, labels: &[Label]) -> Result<u32, OpError> {
    let raw = store.get_meta(postino_store::LABEL_SEQ_KEY).map_err(store_map::read_error)?;
    let stored = raw.as_deref().and_then(|value| value.parse::<u32>().ok()).unwrap_or(0);
    let existing = labels.iter().filter_map(label_number).max().unwrap_or(0);
    let high = stored.max(existing);
    if raw.as_deref().and_then(|value| value.parse::<u32>().ok()) != Some(high) {
        store.set_meta(postino_store::LABEL_SEQ_KEY, &high.to_string()).map_err(store_map::read_error)?;
    }
    Ok(high)
}

fn label_number(label: &Label) -> Option<u32> {
    label.id.strip_prefix("lbl_").and_then(|suffix| suffix.parse::<u32>().ok())
}

fn validate_label_name(name: &str) -> Result<String, OpError> {
    let name = name.trim();
    let characters = name.chars().count();
    if characters == 0 {
        return Err(OpError::InvalidArgs("a label name must have 1-48 characters after trimming".to_owned()));
    }
    if characters > MAX_LABEL_NAME_CHARS {
        return Err(OpError::InvalidArgs(format!("a label name must have 1-{MAX_LABEL_NAME_CHARS} characters after trimming")));
    }
    if name.chars().any(char::is_control) {
        return Err(OpError::InvalidArgs("a label name must not contain control characters".to_owned()));
    }
    if name.chars().any(is_bidi_or_format_control) {
        return Err(OpError::InvalidArgs(
            "a label name must not contain bidi or format control characters (U+061C, U+200B-U+200F, U+202A-U+202E, U+2060-U+206F, U+FEFF)"
                .to_owned(),
        ));
    }
    Ok(name.to_owned())
}

pub(crate) fn is_bidi_or_format_control(character: char) -> bool {
    matches!(character, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

/// A saved search is label-like: 1-48 characters, no controls and no bidi/format tricks.
fn validate_saved_name(name: &str) -> Result<String, OpError> {
    let name = name.trim();
    let characters = name.chars().count();
    if characters == 0 {
        return Err(OpError::InvalidArgs("a saved-search name must have 1-48 characters after trimming".to_owned()));
    }
    if characters > MAX_SAVED_SEARCH_NAME_CHARS {
        return Err(OpError::InvalidArgs(format!("a saved-search name must have 1-{MAX_SAVED_SEARCH_NAME_CHARS} characters after trimming")));
    }
    if name.chars().any(char::is_control) {
        return Err(OpError::InvalidArgs("a saved-search name must not contain control characters".to_owned()));
    }
    if name.chars().any(is_bidi_or_format_control) {
        return Err(OpError::InvalidArgs(
            "a saved-search name must not contain bidi or format control characters (U+061C, U+200B-U+200F, U+202A-U+202E, U+2060-U+206F, U+FEFF)"
                .to_owned(),
        ));
    }
    Ok(name.to_owned())
}

fn validate_saved_sort(sort: Option<&str>) -> Result<String, OpError> {
    match sort.map(str::trim).filter(|sort| !sort.is_empty()) {
        None => Ok("relevance".to_owned()),
        Some("relevance") => Ok("relevance".to_owned()),
        Some("date") => Ok("date".to_owned()),
        Some(other) => Err(OpError::InvalidArgs(format!("sort must be \"relevance\" or \"date\", not {other:?}"))),
    }
}

/// A saved query is validated against the current registry: a deleted label or folder is refused on
/// save, and a row that outlives one becomes visibly stale in `saved_searches_list`.
fn validate_saved_query(engine: &Engine, account: &str, query: &str) -> Result<String, OpError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(OpError::InvalidArgs("a saved search needs a non-empty query".to_owned()));
    }
    crate::ops::validate_search_query(engine, query, Some(account))?;
    Ok(query.to_owned())
}

fn validate_draft(draft: &DraftContent) -> Result<(), OpError> {
    validate_addresses("to", &draft.to)?;
    validate_addresses("cc", &draft.cc)?;
    validate_addresses("bcc", &draft.bcc)?;
    validate_header("subject", &draft.subject, MAX_SUBJECT_BYTES)?;
    if draft.body_text.len() > MAX_BODY_BYTES {
        return Err(OpError::InvalidArgs(format!("body_text is larger than {MAX_BODY_BYTES} bytes")));
    }
    validate_attachments(&draft.attachments)
}

pub(crate) fn validate_addresses(field: &str, addresses: &[Address]) -> Result<(), OpError> {
    if addresses.len() > MAX_RECIPIENTS_PER_FIELD {
        return Err(OpError::InvalidArgs(format!("{field} has more than {MAX_RECIPIENTS_PER_FIELD} recipients")));
    }
    for address in addresses {
        validate_header("recipient name", &address.name, 256)?;
        validate_header("recipient email", &address.email, 320)?;
        let email = address.email.trim();
        let local = email.split_once('@').map(|(local, _)| local).unwrap_or("");
        let domain = email.split_once('@').map(|(_, domain)| domain).unwrap_or("");
        if local.is_empty() || domain.is_empty() || email.chars().any(char::is_whitespace) || email.contains(['<', '>', ',', ';']) {
            return Err(OpError::InvalidArgs(format!("{field} contains an invalid email address: {:?}", address.email)));
        }
    }
    Ok(())
}

fn validate_header(field: &str, value: &str, max_bytes: usize) -> Result<(), OpError> {
    if value.chars().any(|character| character.is_control() && character != '\t') {
        return Err(OpError::InvalidArgs(format!("{field} must not contain line breaks or control characters")));
    }
    if value.len() > max_bytes {
        return Err(OpError::InvalidArgs(format!("{field} is longer than {max_bytes} bytes")));
    }
    Ok(())
}

/// Normalise a parent `Message-ID` for `drafts.in_reply_to`: one pair of angle brackets is accepted
/// and removed, the value is bounded, ASCII and free of controls and whitespace, so the serializer
/// can emit it without a second decision.
fn normalize_reply_parent(raw: &str) -> Result<String, OpError> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')).unwrap_or(trimmed);
    if inner.is_empty() {
        return Err(OpError::InvalidArgs("in_reply_to must name the parent Message-ID".to_owned()));
    }
    if inner.len() > 900 {
        return Err(OpError::InvalidArgs("in_reply_to is longer than 900 bytes".to_owned()));
    }
    if !inner.is_ascii() || inner.chars().any(|character| character.is_control() || character.is_whitespace() || character == '<' || character == '>')
    {
        return Err(OpError::InvalidArgs(
            "in_reply_to must be one Message-ID without whitespace, controls, non-ASCII text or a second bracket pair".to_owned(),
        ));
    }
    Ok(inner.to_owned())
}

/// A signature is body text: bounded, no CR and no other control character. Newline and tab are the
/// only allowed controls, because the editor is plain text and a tab can be a deliberate indent.
fn validate_signature(signature: &str) -> Result<(), OpError> {
    if signature.len() > MAX_SIGNATURE_BYTES {
        return Err(OpError::InvalidArgs(format!("signature is {} bytes; this build accepts at most {MAX_SIGNATURE_BYTES}", signature.len())));
    }
    if let Some(character) = signature.chars().find(|character| character.is_control() && !matches!(character, '\n' | '\t')) {
        return Err(OpError::InvalidArgs(format!(
            "signature must not contain control characters except newline and tab (found U+{:04X})",
            u32::from(character)
        )));
    }
    Ok(())
}

/// Apply the import naming rule to metadata supplied through `draft_save`: sanitize every name and
/// rename duplicates. Bytes still come only from `draft_attachment_add`; this keeps a metadata-only
/// row from carrying a device name, a tab or a duplicate that the import path would have repaired.
fn normalize_draft_attachment_names(mut draft: DraftContent) -> DraftContent {
    let mut normalized: Vec<Attachment> = Vec::with_capacity(draft.attachments.len());
    for mut attachment in draft.attachments.drain(..) {
        // CR/LF/NUL are a hard error on the import path and in the serializer; keep that rule for
        // metadata too (validate_draft rejects the untouched name below).
        let sanitized = if attachment.filename.chars().any(|character| matches!(character, '\r' | '\n' | '\0')) {
            attachment.filename.clone()
        } else {
            postino_sync::sanitize_filename(&attachment.filename)
        };
        attachment.filename = unique_attachment_name(&sanitized, &normalized);
        let derived = postino_sync::mime_for_filename(&attachment.filename);
        if postino_sync::normalize_mime_type(&attachment.mime).as_deref() != Some(derived) {
            attachment.mime = derived.to_owned();
        }
        normalized.push(attachment);
    }
    draft.attachments = normalized;
    draft
}

/// One attachment that can be read as a calendar invite. The MIME type is authoritative; a bare
/// `.ics` name is accepted only for the generic types a scanner or a sender may attach.
fn is_calendar_part(attachment: &Attachment) -> bool {
    if attachment.mime.eq_ignore_ascii_case("text/calendar") {
        return true;
    }
    attachment.filename.len() <= 128
        && attachment.filename.to_ascii_lowercase().ends_with(".ics")
        && matches!(attachment.mime.to_ascii_lowercase().as_str(), "application/ics" | "application/octet-stream" | "text/x-vcalendar")
}

fn invite_view(invite: &postino_sync::CalendarInvite) -> CalendarInviteView {
    let method = match invite.method.as_str() {
        "REQUEST" => "request",
        "CANCEL" => "cancel",
        "REPLY" => "reply",
        _ => "other",
    };
    CalendarInviteView {
        method: method.to_owned(),
        summary: invite.summary.clone(),
        organizer: invite.organizer.clone(),
        attendees: invite.attendees.clone(),
        start: invite.start.clone(),
        end: invite.end.clone(),
        location: invite.location.clone(),
        recurrence: invite.recurrence,
        timezone: invite.timezone.clone(),
        valid: invite.valid,
        invalid_reason: invite.invalid_reason.clone(),
        cancelled: invite.cancelled,
        can_reply: invite.can_reply,
        warnings: invite.warnings.iter().map(|warning| warning.message().to_owned()).collect(),
    }
}

/// The fixed text of an invite reply. It is a draft, so it says so: no automatic response, no
/// calendar update, and only the organizer on the To line.
fn invite_reply_body(invite: &postino_sync::CalendarInvite, response: InviteResponse) -> String {
    let answer = match response {
        InviteResponse::Accept => "Ho accettato l'invito.",
        InviteResponse::Tentative => "Ho segnato l'invito come tentativo.",
        InviteResponse::Decline => "Ho rifiutato l'invito.",
    };
    let mut body = format!("{answer}\n\nOggetto: {}\n", invite.summary);
    if let Some(start) = invite.start.as_deref() {
        match invite.end.as_deref() {
            Some(end) => body.push_str(&format!("Quando: {start} — {end}\n")),
            None => body.push_str(&format!("Quando: {start}\n")),
        }
    }
    if let Some(location) = invite.location.as_deref() {
        body.push_str(&format!("Luogo: {location}\n"));
    }
    body.push_str("\nNessuna risposta automatica: rivedi e invia questa bozza. Nessun calendario è stato aggiornato.");
    body
}

/// Rename a duplicate to `name (2).ext`, keeping the extension and the 255-byte bound.
fn unique_attachment_name(name: &str, existing: &[Attachment]) -> String {
    if !existing.iter().any(|attachment| attachment.filename.eq_ignore_ascii_case(name)) {
        return name.to_owned();
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    for counter in 2..=10_000u32 {
        let candidate = format!("{stem} ({counter}){extension}");
        let candidate = postino_sync::sanitize_filename(&candidate);
        if !candidate.is_empty() && !existing.iter().any(|attachment| attachment.filename.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
    }
    // A draft has at most 20 attachments, so this is only reachable with a damaged row set.
    format!("{stem}-{}{extension}", existing.len().saturating_add(1))
}

/// A draft-scoped attachment id that is free on this draft. The prefix is the draft id, so an id can
/// never collide with another message's attachment.
fn next_attachment_id(draft_id: &str, existing: &[Attachment]) -> String {
    for counter in 0..10_000usize {
        let candidate = format!("{draft_id}_att_{counter}");
        if !existing.iter().any(|attachment| attachment.id == candidate) {
            return candidate;
        }
    }
    format!("{draft_id}_att_overflow")
}

fn validate_unique_attachment_ids(attachments: &[Attachment]) -> Result<(), OpError> {
    let mut seen = BTreeSet::new();
    for attachment in attachments {
        if !seen.insert(attachment.id.as_str()) {
            return Err(OpError::InvalidArgs(format!(
                "attachment id {:?} is repeated in the draft; give every attachment a unique id",
                attachment.id
            )));
        }
    }
    Ok(())
}

fn validate_attachments(attachments: &[Attachment]) -> Result<(), OpError> {
    if attachments.len() > MAX_DRAFT_ATTACHMENTS {
        return Err(OpError::InvalidArgs(format!("a draft can carry at most {MAX_DRAFT_ATTACHMENTS} attachments")));
    }
    let mut total = 0u64;
    for attachment in attachments {
        validate_header("attachment id", &attachment.id, 128)?;
        validate_header("attachment filename", &attachment.filename, 255)?;
        validate_header("attachment type", &attachment.mime, 127)?;
        if attachment.filename.trim().is_empty() || attachment.filename.contains(['/', '\\', ':']) {
            return Err(OpError::InvalidArgs("attachment filenames must be a plain file name".to_owned()));
        }
        if attachment.filename.chars().any(is_bidi_or_format_control) {
            return Err(OpError::InvalidArgs("attachment filenames must not contain bidi or format control characters".to_owned()));
        }
        if attachment.size > MAX_ATTACHMENT_BYTES {
            return Err(OpError::InvalidArgs(format!(
                "attachment {:?} is {} bytes; this build accepts at most {MAX_ATTACHMENT_BYTES} bytes per file",
                attachment.filename, attachment.size
            )));
        }
        total = total.saturating_add(attachment.size);
    }
    if total > MAX_ATTACHMENT_TOTAL_BYTES {
        return Err(OpError::InvalidArgs(format!(
            "the attachments total {total} bytes; this build accepts at most {MAX_ATTACHMENT_TOTAL_BYTES} bytes per message"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use serde_json::json;

    use super::*;

    #[test]
    fn a_reload_failure_does_not_hide_the_sync_cause() {
        let cause = OpError::Auth("the server rejected the credential".to_owned());
        let error = finish_sync(Err(cause), Err(OpError::Failed("the local store could not be read".to_owned()))).expect_err("both fail");
        assert!(matches!(error, OpError::Auth(_)), "{error}");
        assert!(error.to_string().contains("rejected the credential"), "{error}");
        assert!(error.to_string().contains("could not be reloaded"), "the reload failure is not mentioned: {error}");
    }

    #[test]
    fn demo_engine_has_accounts_and_mail() {
        let engine = Engine::demo();
        assert!(engine.is_demo());
        assert_eq!(engine.accounts().len(), 1);
        assert!(engine.messages().len() >= 10);
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Inbox));
    }

    #[test]
    fn demo_dates_are_relative_to_the_requested_instant() {
        let now = DateTime::parse_from_rfc3339("2026-10-24T10:42:00Z").expect("fixture").with_timezone(&Utc);
        let engine = Engine::demo_at(now);
        assert_eq!(engine.messages()[0].date, now - chrono::Duration::minutes(8));
        assert!(engine.messages().iter().all(|message| message.date < now));
    }

    #[test]
    fn flags_update_folder_counts() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].id.clone();
        let before = engine.folders().iter().find(|f| f.id == "inbox").map_or(0, |f| f.unread);
        let was_unread = engine.message(&id).is_some_and(|m| m.summary.unread);
        let summary = engine.set_flags(&id, Some(!was_unread), None).expect("flag");
        assert_eq!(summary.unread, !was_unread);
        let after = engine.folders().iter().find(|f| f.id == "inbox").map_or(0, |f| f.unread);
        assert_eq!(after, if was_unread { before.saturating_sub(1) } else { before.saturating_add(1) });
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        let mut engine = Engine::demo();
        let error = engine.call("messages_list", json!({ "nope": true })).expect_err("unknown key");
        assert!(matches!(error, OpError::InvalidArgs(_)));
    }

    #[test]
    fn unknown_operation_is_reported() {
        let mut engine = Engine::demo();
        assert!(matches!(engine.call("mail_steal", json!({})), Err(OpError::UnknownOp(_))));
    }

    fn draft(subject: &str, body: &str) -> DraftContent {
        DraftContent {
            to: vec![Address::new("Giulia Ferri", "giulia.ferri@example.com")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.to_owned(),
            body_text: body.to_owned(),
            attachments: Vec::new(),
            in_reply_to: None,
        }
    }

    #[test]
    fn saving_twice_updates_the_same_draft_and_the_folder_count() {
        let mut engine = Engine::demo();
        let before = engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        let first = engine.save_draft(None, None, draft("Ciao", "Prima versione")).expect("new draft");
        assert_eq!(first.summary.folder_id, "drafts");
        let after_first = engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        assert_eq!(after_first, before.saturating_add(1));

        let mut changed = draft("Ciao", "Seconda versione");
        changed.cc.push(Address::new("Marco Bianchi", "marco.bianchi@example.com"));
        let second = engine.save_draft(None, Some(&first.summary.id), changed).expect("update");
        assert_eq!(second.summary.id, first.summary.id);
        assert_eq!(second.body_text, "Seconda versione");
        assert_eq!(second.cc.len(), 1);
        let after_second = engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        assert_eq!(after_second, after_first, "an update must not create a second draft");
    }

    #[test]
    fn sending_a_draft_moves_it_to_sent_and_undo_can_move_it_back() {
        let mut engine = Engine::demo();
        let draft = engine.save_draft(None, None, draft("Ciao", "Corpo")).expect("draft");
        let id = draft.summary.id;
        let sent = engine.send_draft(&id).expect("send");
        assert_eq!(sent.folder_id, "sent");
        assert!(!sent.unread);
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Sent && folder.total >= 1));
        engine.move_message(&id, "drafts").expect("undo move");
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Drafts && folder.total >= 1));
    }

    #[test]
    fn drafts_refuse_header_injection_and_invalid_addresses() {
        let mut engine = Engine::demo();
        let mut injected = draft("Ciao", "Corpo");
        injected.subject = "Ciao\nBcc: attacker@example.com".to_owned();
        assert!(matches!(engine.save_draft(None, None, injected), Err(OpError::InvalidArgs(_))));

        let mut bad_address = draft("Ciao", "Corpo");
        bad_address.to = vec![Address::new("Giulia", "not-an-address")];
        assert!(matches!(engine.save_draft(None, None, bad_address), Err(OpError::InvalidArgs(_))));
    }

    #[test]
    fn an_empty_draft_cannot_be_sent() {
        let mut engine = Engine::demo();
        let mut empty = draft("", "");
        empty.to.clear();
        let draft = engine.save_draft(None, None, empty).expect("draft");
        assert!(matches!(engine.send_draft(&draft.summary.id), Err(OpError::InvalidArgs(_))));
    }

    #[test]
    fn a_sent_message_is_not_a_draft_any_more() {
        let mut engine = Engine::demo();
        let draft = engine.save_draft(None, None, draft("Ciao", "Corpo")).expect("draft");
        let id = draft.summary.id;
        engine.send_draft(&id).expect("send");
        assert!(matches!(engine.send_draft(&id), Err(OpError::InvalidArgs(_))));
    }

    #[test]
    fn deleting_the_highest_label_does_not_reuse_its_id() {
        let mut engine = Engine::demo();
        engine.delete_label("lbl_3").expect("delete the highest label");
        let created = engine.create_label("Nuova", LabelColor::Forest).expect("create after delete");
        assert_eq!(created.id, "lbl_4", "a deleted id must never come back");
        let next = engine.create_label("Altra", LabelColor::Muted).expect("second create");
        assert_eq!(next.id, "lbl_5");
        assert!(engine.delete_label("lbl_1").is_ok());
        let after_low_delete = engine.create_label("Terza", LabelColor::Gold).expect("create after low delete");
        assert_eq!(after_low_delete.id, "lbl_6", "the sequence also ignores deleted low ids");
    }

    #[test]
    fn label_summaries_count_each_label_in_one_pass() {
        let mut engine = Engine::demo();
        engine.set_flags("msg_001", Some(false), None).expect("mark read");
        engine.set_label("lbl_2", &["msg_004".to_owned(), "msg_005".to_owned()], true).expect("apply Personale");

        let summaries = engine.label_summaries();
        let lavoro = summaries.iter().find(|label| label.id == "lbl_1").expect("Lavoro");
        assert_eq!((lavoro.message_count, lavoro.unread_count), (6, 1));
        let personale = summaries.iter().find(|label| label.id == "lbl_2").expect("Personale");
        assert_eq!((personale.message_count, personale.unread_count), (4, 1));
        let fatture = summaries.iter().find(|label| label.id == "lbl_3").expect("Fatture");
        assert_eq!((fatture.message_count, fatture.unread_count), (1, 1), "a message can feed two labels");
    }

    #[test]
    fn label_names_reject_bidi_controls_and_keep_accents() {
        let mut engine = Engine::demo();
        assert!(matches!(engine.create_label("A\u{202e}B", LabelColor::Gold), Err(OpError::InvalidArgs(_))));
        assert!(matches!(engine.create_label("Zero\u{200b}width", LabelColor::Gold), Err(OpError::InvalidArgs(_))));
        let accented = engine.create_label("Città", LabelColor::Gold).expect("accented names are ordinary text");
        assert_eq!(accented.name, "Città");
        let emoji = engine.create_label("Casa 🏡", LabelColor::Gold).expect("emoji are allowed");
        assert_eq!(emoji.name, "Casa 🏡");
    }
}
