//! The engine owns the mailbox data: in memory for the demo, in the encrypted store when one is
//! attached. The operation surface is identical for both.
//!
//! With a store the engine is **load once, write through**: [`Engine::open`] hydrates the mailbox
//! with one query per table, every mutating operation writes the repository first and only then
//! updates memory, and folder counts are recomputed locally instead of re-read. A failed write
//! leaves memory untouched: caller mistakes are [`OpError::InvalidArgs`] and storage failures are
//! [`OpError::Failed`], and each engine mutation is one store transaction.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::Utc;
use postino_net::{CertificateDer, ImapConfig, NetError, Secret, SmtpConfig, SmtpEnvelope, SmtpError, SmtpLimits};
use postino_store::{FlagUpdate, OutboxEntry, OutboxState, Store};
use postino_sync::{SyncError, SyncLimits};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::model::{
    Account, AccountView, Address, Attachment, DraftContent, Folder, FolderKind, Label, LabelColor, LabelSummary, MailTls, Message, MessageSummary,
    Provider, SmtpSettings, snippet,
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
const MAX_ATTACHMENTS: usize = 50;
const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
const MAX_LABEL_NAME_CHARS: usize = 48;
const MAX_LABEL_MESSAGES: usize = 500;

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
    messages: Vec<Message>,
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
    extra_roots: Vec<CertificateDer<'static>>,
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
}

impl Engine {
    /// An engine with no accounts. Every list is empty until a store or demo data is attached.
    pub fn empty() -> Self {
        Self {
            accounts: Vec::new(),
            folders: Vec::new(),
            messages: Vec::new(),
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
        repair_orphan_drafts(&store, &accounts, &folders)?;
        let mut attachment_rows = store.list_all_attachments().map_err(store_map::read_error)?;
        let mut blob_hashes = HashMap::new();
        let messages = store
            .list_all_messages()
            .map_err(store_map::read_error)?
            .into_iter()
            .map(|row| {
                let rows = attachment_rows.remove(&row.summary.id).unwrap_or_default();
                for attachment in &rows {
                    if let Some(hash) = &attachment.blob_hash {
                        blob_hashes.insert(attachment.id.clone(), hash.clone());
                    }
                }
                store_map::message(row, rows)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let labels = store.list_labels(None).map_err(store_map::read_error)?.into_iter().map(store_map::label).collect::<Result<Vec<_>, _>>()?;
        let label_seq = repair_label_seq(&store, &labels)?;

        let mut engine = Self::from_parts(accounts, folders, messages, labels, false, Some(store));
        engine.vault = vault;
        engine.next_label_seq = label_seq.saturating_add(1);
        engine.blob_hashes = blob_hashes;
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
        let next_label_seq = labels.iter().filter_map(label_number).max().unwrap_or(0).saturating_add(1);
        let mut engine = Self {
            accounts,
            folders,
            messages,
            labels,
            next_label_seq,
            demo,
            store,
            vault: None,
            extra_roots: Vec::new(),
            #[cfg(debug_assertions)]
            crash_after_accept: false,
            #[cfg(debug_assertions)]
            test_after_outbox_read: None,
            #[cfg(debug_assertions)]
            test_clock: None,
            blob_hashes: HashMap::new(),
        };
        engine.recount();
        engine
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

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn message(&self, id: &str) -> Option<&Message> {
        self.messages.iter().find(|message| message.summary.id == id)
    }

    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    pub fn label(&self, id: &str) -> Option<&Label> {
        self.labels.iter().find(|label| label.id == id)
    }

    /// Labels with mailbox-wide counts, computed in one pass over the messages so the work does
    /// not grow with labels × messages.
    pub fn label_summaries(&self) -> Vec<LabelSummary> {
        let mut counts: HashMap<&str, (u32, u32)> = HashMap::new();
        for message in &self.messages {
            for id in &message.summary.labels {
                let entry = counts.entry(id.as_str()).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(1);
                if message.summary.unread {
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
        let label = self.label(id)?;
        let (message_count, unread_count) = self
            .messages
            .iter()
            .filter(|message| message.summary.labels.iter().any(|candidate| candidate == id))
            .fold((0u32, 0u32), |(total, unread), message| {
                (total.saturating_add(1), unread.saturating_add(if message.summary.unread { 1 } else { 0 }))
            });
        Some(LabelSummary { id: label.id.clone(), name: label.name.clone(), color: label.color, message_count, unread_count })
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
        validate_account_password(&setup.password)?;
        let store = self.require_store("account_add")?;
        let vault = self.require_vault("account_add")?;
        let ResolvedAccount { id, provider, email, name, host, port, tls, smtp } = resolved;

        let mut config = ImapConfig::new(host.clone(), port, store_map::net_tls(tls), email.clone(), Secret::new(setup.password.as_str().to_owned()));
        let mut extra_roots = self.extra_roots.clone();
        extra_roots.extend(setup.extra_roots.clone());
        for root in &extra_roots {
            config = config.with_extra_root(root.clone());
        }
        let probe = postino_sync::probe(&config).map_err(|error| {
            account_error(
                error,
                provider,
                &id,
                &host,
                "checking the connection",
                "Nothing was written; check the address, host, password and TLS mode and retry.",
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
            let password = match setup.smtp.as_ref().and_then(|setup| setup.password.as_ref()) {
                Some(password) => password.clone(),
                None => setup.password.clone(),
            };
            validate_account_password(&password)?;
            let mut smtp_config = postino_net::SmtpConfig::new(
                smtp.host.clone(),
                smtp.port,
                store_map::net_tls(smtp.tls),
                smtp.username.clone(),
                Secret::new(password.as_str().to_owned()),
            );
            for root in &extra_roots {
                smtp_config = smtp_config.with_extra_root(root.clone());
            }
            postino_sync::verify_smtp(smtp_config).map_err(|error| {
                OpError::Auth(format!(
                    "the SMTP server {}:{} did not accept the credential ({}). Nothing was written; check the SMTP password, host, port and TLS mode.",
                    smtp.host,
                    smtp.port,
                    smtp_failure_reason(&error)
                ))
            })?;
            smtp_password = setup.smtp.as_ref().and_then(|setup| setup.password.clone());
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
        if let Err(error) = store.save_account(&account_row, &server_row, &folder_rows) {
            let imap_cleanup = vault.delete_imap_password(&id);
            let smtp_cleanup = vault.delete_smtp_password(&id);
            let outcome = match (imap_cleanup, smtp_cleanup) {
                (Ok(()), Ok(())) => "The credentials were removed again and nothing was written.".to_owned(),
                _ => "One or both credentials could not be removed again from the OS keyring; a retry with the same address overwrites them, or delete them in the OS credential manager.".to_owned(),
            };
            return Err(OpError::Failed(format!("the local store refused the account: {error}. {outcome}")));
        }

        // Durable writes are done; only now does memory change. A re-add with a fresh password also
        // clears a previous authentication halt: the user has just proved the credential works.
        let _ = clear_halt(store, &id);
        let account = store_map::account(account_row);
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
        let password = match request.smtp_password {
            Some(password) => {
                validate_account_password(&password)?;
                password
            }
            None => {
                let bytes = vault
                    .load_smtp_password(&account.id)
                    .map_err(|error| OpError::Failed(format!("the OS keyring could not provide the SMTP credential: {error}")))?
                    .or_else(|| vault.load_imap_password(&account.id).ok().flatten())
                    .ok_or_else(|| OpError::Failed(format!("account {:?} has no SMTP or IMAP credential; supply a new SMTP password", account.id)))?;
                let text = std::str::from_utf8(bytes.as_slice())
                    .map_err(|_| OpError::Failed(format!("the stored SMTP credential for account {:?} is not valid UTF-8", account.id)))?;
                Zeroizing::new(text.to_owned())
            }
        };
        let mut config = postino_net::SmtpConfig::new(
            smtp.host.clone(),
            smtp.port,
            store_map::net_tls(smtp.tls),
            smtp.username.clone(),
            Secret::new(password.as_str().to_owned()),
        );
        for root in &self.extra_roots {
            config = config.with_extra_root(root.clone());
        }
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
        if new_password && let Err(error) = vault.store_smtp_password(&account.id, password.as_bytes()) {
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
        let password = self.load_password(&account.id, "account_test")?;
        let config = imap_config(&server, &password, self.extra_roots.clone());
        let provider = Provider::parse(&account.provider).unwrap_or(Provider::Custom);
        let probe = postino_sync::probe(&config).map_err(|error| {
            account_error(error, provider, &account.id, &server.host, "checking the connection", "The account and its cached mail are unchanged.")
        })?;
        // A configured submission endpoint is checked too: after an SMTP password change "credentials
        // valid" must not be true while sending fails. Fixed sentences only: this server holds the
        // password and may echo it.
        if let Some(smtp) = server.smtp.clone() {
            let smtp_password = self.load_smtp_password(&account.id, "account_test")?;
            let mut smtp_config = SmtpConfig::new(
                smtp.host.clone(),
                smtp.port,
                store_map::net_tls(store_map::mail_tls(smtp.tls)),
                smtp.username.clone(),
                Secret::new(smtp_password.as_str().to_owned()),
            );
            for root in &self.extra_roots {
                smtp_config = smtp_config.with_extra_root(root.clone());
            }
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
    /// The order is deliberate: the store rows go first, the credential second. The only failure
    /// window therefore leaves an orphan keyring entry, never cached mail whose password is gone.
    /// An orphan secret is retryable (a re-add with the same address derives the same account id and
    /// the same scope) and is reported explicitly. Labels are mailbox-wide and stay.
    pub fn remove_account(&mut self, id: &str, confirm: bool) -> Result<AccountView, OpError> {
        let index = self.accounts.iter().position(|account| account.id == id).ok_or_else(|| unknown_account(id))?;
        if !confirm {
            return Err(OpError::InvalidArgs(format!(
                "removing account {id:?} also deletes its cached mail and its IMAP credential; pass confirm: true to proceed"
            )));
        }
        let account = self.accounts.get(index).cloned().ok_or_else(|| unknown_account(id))?;
        let view = self.account_view(&account)?;
        let store = self.require_store("account_remove")?;
        let vault = self.require_vault("account_remove")?;
        if !store.delete_account(&account.id).map_err(store_map::write_error)? {
            return Err(OpError::Failed(format!("the local store no longer has account {:?}; nothing was changed", account.id)));
        }
        let imap_credential = vault.delete_imap_password(&account.id);
        let smtp_credential = vault.delete_smtp_password(&account.id);
        let folder_ids =
            self.folders.iter().filter(|folder| folder.account_id == account.id).map(|folder| folder.id.clone()).collect::<BTreeSet<_>>();
        self.accounts.retain(|candidate| candidate.id != account.id);
        self.folders.retain(|folder| folder.account_id != account.id);
        self.messages.retain(|message| !folder_ids.contains(&message.summary.folder_id));
        self.recount();
        match (imap_credential, smtp_credential) {
            (Ok(()), Ok(())) => Ok(view),
            (imap, smtp) => Err(OpError::Failed(format!(
                "account {:?} and its cached mail were removed, but the OS keyring refused to delete a credential (IMAP: {}; SMTP: {}). The credential is now unreferenced; adding this address again overwrites it, or delete it in the OS credential manager.",
                account.id,
                credential_outcome(&imap),
                credential_outcome(&smtp)
            ))),
        }
    }

    fn load_smtp_password(&self, account_id: &str, operation: &str) -> Result<Zeroizing<String>, OpError> {
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
        let account = self.requested_account(account, "sync_now")?;
        let store = self.require_store("sync_now")?;
        let server = store.get_account_server(&account.id).map_err(store_map::read_error)?.ok_or_else(|| {
            OpError::Failed(format!("account {:?} has no IMAP server configuration; remove it with account_remove and add it again", account.id))
        })?;
        let row = store
            .get_account(&account.id)
            .map_err(store_map::read_error)?
            .ok_or_else(|| OpError::Failed(format!("account {:?} is not in the local store", account.id)))?;
        let password = self.load_password(&account.id, "sync_now")?;
        let config = imap_config(&server, &password, self.extra_roots.clone());
        let provider = Provider::parse(&account.provider).unwrap_or(Provider::Custom);
        let result = postino_sync::sync_account(store, &row, &config, &SyncLimits::default(), &AtomicBool::new(false)).map_err(|error| {
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

    fn find_account(&self, id: &str) -> Result<&Account, OpError> {
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

    fn load_password(&self, account_id: &str, operation: &str) -> Result<Zeroizing<String>, OpError> {
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

    fn require_vault(&self, operation: &str) -> Result<&dyn CredentialVault, OpError> {
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
    fn reload_mailbox(&mut self) -> Result<(), OpError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let folders = store.list_folders(None).map_err(store_map::read_error)?.into_iter().map(store_map::folder).collect::<Vec<_>>();
        let mut attachment_rows = store.list_all_attachments().map_err(store_map::read_error)?;
        let mut blob_hashes = HashMap::new();
        let messages = store
            .list_all_messages()
            .map_err(store_map::read_error)?
            .into_iter()
            .map(|row| {
                let rows = attachment_rows.remove(&row.summary.id).unwrap_or_default();
                for attachment in &rows {
                    if let Some(hash) = &attachment.blob_hash {
                        blob_hashes.insert(attachment.id.clone(), hash.clone());
                    }
                }
                store_map::message(row, rows)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.folders = folders;
        self.messages = messages;
        self.blob_hashes = blob_hashes;
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
            let before = message.summary.labels.len();
            message.summary.labels.retain(|candidate| candidate != id);
            if message.summary.labels.len() != before {
                affected = affected.saturating_add(1);
            }
        }
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
            if !self.messages.iter().any(|message| message.summary.id == **message_id) {
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
            if !ids.contains(message.summary.id.as_str()) {
                continue;
            }
            let has = message.summary.labels.iter().any(|candidate| candidate == id);
            if apply && !has {
                message.summary.labels.push(id.to_owned());
                changed = changed.saturating_add(1);
            } else if !apply && has {
                message.summary.labels.retain(|candidate| candidate != id);
                changed = changed.saturating_add(1);
            }
        }
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
        if !self.messages.iter().any(|message| message.summary.id == id) {
            return Err(unknown_message(id));
        }
        if let Some(store) = &self.store {
            store.set_flags(&[id.to_owned()], FlagUpdate { unread, starred }).map_err(store_map::write_error)?;
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.summary.id == id) else {
            return Err(OpError::Failed(format!("unknown message {id:?}")));
        };
        if let Some(unread) = unread {
            message.summary.unread = unread;
        }
        if let Some(starred) = starred {
            message.summary.starred = starred;
        }
        let summary = message.summary.clone();
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
        let current = self.messages.iter().find(|message| message.summary.id == id).ok_or_else(|| unknown_message(id))?;
        let current_kind = self.folders.iter().find(|folder| folder.id == current.summary.folder_id).map(|folder| folder.kind);
        if let Some(store) = &self.store {
            // The store, not this engine's snapshot, says where the message is. A stale view must not
            // move a row another process already sent, and a vanished row is not a movable message.
            let row = store
                .get_message(id)
                .map_err(store_map::read_error)?
                .ok_or_else(|| OpError::InvalidArgs(format!("message {id:?} no longer exists; refresh the mailbox")))?;
            if row.summary.folder_id != current.summary.folder_id {
                return Err(OpError::InvalidArgs(format!("message {id:?} changed in another session; refresh the mailbox before moving it")));
            }
        }
        if let Some(state) = self.outbox_lock_for(id, current_kind)? {
            return Err(OpError::InvalidArgs(outbox_lock_message("message", id, state, "spostato")));
        }
        if let Some(store) = &self.store {
            store.move_messages(&[id.to_owned()], &target_id).map_err(store_map::write_error)?;
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.summary.id == id) else {
            return Err(OpError::Failed(format!("unknown message {id:?}")));
        };
        message.summary.folder_id = target_id;
        let summary = message.summary.clone();
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
        validate_draft(&draft)?;
        let index = match id {
            Some(id) => {
                if id.trim().is_empty() {
                    return Err(OpError::InvalidArgs("draft id must not be empty".to_owned()));
                }
                let index = self
                    .messages
                    .iter()
                    .position(|message| message.summary.id == id)
                    .ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}; omit id to create a draft")))?;
                let kind = self.folders.iter().find(|folder| folder.id == self.messages[index].summary.folder_id).map(|folder| folder.kind);
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
                    if row.summary.folder_id != self.messages[index].summary.folder_id {
                        return Err(OpError::InvalidArgs(format!(
                            "message {id:?} changed in another session (it is no longer in this folder); refresh the mailbox before editing it"
                        )));
                    }
                }
                Some(index)
            }
            None => None,
        };
        let current_folder = index.and_then(|index| self.messages.get(index)).map(|message| message.summary.folder_id.as_str());
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
            Some(current) => current.summary.folder_id.clone(),
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
        let replaced_ids = index
            .and_then(|index| self.messages.get(index))
            .map(|message| message.attachments.iter().map(|attachment| attachment.id.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        let summary_snippet = snippet(&draft.body_text);
        // An update keeps what the draft form does not own: its thread, starred flag, labels and
        // HTML body. The composer patches `body_text`; a subject-only save must not destroy the
        // stored HTML part.
        let (thread_id, starred, labels, body_html) = match index.and_then(|index| self.messages.get(index)) {
            Some(current) => (current.summary.thread_id.clone(), current.summary.starred, current.summary.labels.clone(), current.body_html.clone()),
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

        let persisted = if let Some(store) = &self.store {
            let row = store_map::message_row(&saved, &account.id, None);
            store.save_draft_message(&row, &store_map::attachment_rows(&saved.attachments)).map_err(store_map::write_error)?;
            // The repository keeps the stored hash *and size* of an attachment whose id survived the
            // replacement. Mirror those rows, or memory would cap a 2 MiB blob at a stale size of 0
            // and refuse to read it until the next restart.
            Some(store.attachments(&saved.summary.id).map_err(store_map::read_error)?)
        } else {
            None
        };
        if let Some(rows) = persisted {
            for id in &replaced_ids {
                self.blob_hashes.remove(id);
            }
            for row in &rows {
                if let Some(hash) = &row.blob_hash {
                    self.blob_hashes.insert(row.id.clone(), hash.clone());
                }
            }
            saved.attachments = rows.into_iter().map(store_map::attachment).collect();
        } else {
            for id in &replaced_ids {
                if !saved.attachments.iter().any(|attachment| attachment.id == *id) {
                    self.blob_hashes.remove(id);
                }
            }
        }

        match index {
            Some(index) => {
                let Some(message) = self.messages.get_mut(index) else {
                    return Err(OpError::Failed(format!("unknown draft {message_id:?}")));
                };
                *message = saved.clone();
            }
            None => self.messages.push(saved.clone()),
        }
        self.recount();
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
        let now = Utc::now().timestamp_millis();
        self.queue_send(draft_id, now.saturating_add(UNDO_MS), now)
    }

    /// Queue a draft at an explicit instant. A time in the past is treated as "now".
    pub fn send_schedule(&mut self, draft_id: &str, send_at: chrono::DateTime<Utc>) -> Result<SendQueued, OpError> {
        let now = Utc::now().timestamp_millis();
        self.queue_send(draft_id, send_at.timestamp_millis().max(now), now)
    }

    /// Serialize a draft and return the exact bytes without touching the outbox.
    ///
    /// The same builder as a real send, so a dry run proves what would leave the machine. No network
    /// call and no row.
    pub fn send_dry_run(&self, draft_id: &str) -> Result<SendDryRun, OpError> {
        let (serialized, _account, _message) = self.submission_for_draft(draft_id)?;
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &serialized.raw);
        Ok(SendDryRun {
            raw_base64: encoded,
            bytes: serialized.raw.len(),
            envelope: SendEnvelope { from: serialized.envelope.from.clone(), recipients: serialized.envelope.recipients.clone() },
            subject: _message.summary.subject.clone(),
            message_id: serialized.message_id.clone(),
        })
    }

    fn queue_send(&mut self, draft_id: &str, send_at: i64, now: i64) -> Result<SendQueued, OpError> {
        let store = self.require_store("message_send")?;
        let (serialized, account, _message) = self.submission_for_draft(draft_id)?;
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
        // The draft revision is the submission snapshot: it changes on every save and never goes
        // backwards, so "the revision is still this one" means "the draft has not been edited since
        // it was serialized". It is also part of the idempotency key, which makes a deliberate
        // resend of an edited, moved-back draft a new row instead of a collision with the sent row.
        let draft_row = store.get_draft(draft_id).map_err(store_map::read_error)?;
        let revision = draft_row.as_ref().map(|draft| draft.revision);
        // The serialized bytes come from this engine's snapshot. If another process saved the draft
        // after this engine loaded it, the revision would describe the other process's content while
        // the bytes describe ours; refuse and let the caller reload. Compare the whole serialized
        // snapshot (body, headers, Date and attachment set), not a timestamp: two saves can land in
        // the same millisecond.
        if let Some(draft) = &draft_row
            && !self.draft_snapshot_matches(&_message, draft, store)?
        {
            return Err(OpError::InvalidArgs(format!("draft {draft_id:?} changed in another session; refresh the mailbox before sending it")));
        }
        // The newest row for this draft decides, and it is one indexed lookup, never a history scan.
        if let Some(existing) = store.active_outbox_for_draft(draft_id).map_err(store_map::read_error)? {
            match existing.state {
                OutboxState::Queued => {
                    // The same queue operation was retried before the undo window expired: return
                    // the existing row instead of inserting a duplicate submission.
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
            draft_id: draft_id.to_owned(),
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
    fn submission_for_draft(&self, draft_id: &str) -> Result<(postino_sync::SerializedSubmission, Account, Message), OpError> {
        let current = self.messages.iter().find(|message| message.summary.id == draft_id).cloned().ok_or_else(|| unknown_message(draft_id))?;
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
            in_reply_to: None,
            references: Vec::new(),
            message_id: None,
        };
        let serialized = postino_sync::serialize_submission(&submission)
            .map_err(|error| OpError::InvalidArgs(format!("the message cannot be serialized: {error}")))?;
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
            if let Some(envelope) = OutboxEnvelope::decode(&entry.envelope_json) {
                let _ = self.finalize_outbox_draft(&envelope.draft_id, Some(&envelope.message_id), envelope.draft_revision, &store);
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
        for _ in 0..100 {
            let claimed_at = self.claim_now(pass_start);
            let Some(entry) = store.claim_outbox_append(claimed_at, APPEND_LEASE_MS).map_err(store_map::read_error)? else {
                break;
            };
            let lease = entry.lease_at.unwrap_or(claimed_at);
            let now = self.claim_now(pass_start);
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
        let smtp_password = match self.load_smtp_password(&entry.account_id, "sending") {
            Ok(password) => password,
            Err(_) => return self.fail_leased(store, entry, lease, "la credenziale SMTP non è disponibile", report),
        };
        let mut config = SmtpConfig::new(
            smtp.host.clone(),
            smtp.port,
            store_map::net_tls(store_map::mail_tls(smtp.tls)),
            smtp.username.clone(),
            Secret::new(smtp_password.as_str().to_owned()),
        );
        config.limits = SmtpLimits { max_message: usize::try_from(MAX_SUBMISSION_BYTES).unwrap_or(usize::MAX), ..SmtpLimits::default() };
        for root in self.extra_roots.clone() {
            config = config.with_extra_root(root);
        }
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
                    &envelope.draft_id,
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
            let _ = store.reschedule_outbox_append(&entry.id, now_ms.saturating_add(backoff_ms(entry.append_attempts.max(1))), Some(sentence), lease);
            return Err(format!("riga {:?}: {sentence}; la copia in Inviati resta da fare.", entry.id));
        };
        let server = match store.get_account_server(&entry.account_id) {
            Ok(Some(server)) => server,
            Ok(None) => {
                let _ = store.reschedule_outbox_append(
                    &entry.id,
                    now_ms.saturating_add(backoff_ms(entry.append_attempts.max(1))),
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
                    now_ms.saturating_add(backoff_ms(entry.append_attempts.max(1))),
                    Some("i byte salvati del messaggio non sono leggibili"),
                    lease,
                );
                return Err(format!("outbox row {:?}: i byte salvati del messaggio non sono leggibili.", entry.id));
            }
        };
        let password = match self.load_password(&entry.account_id, "the Sent append") {
            Ok(password) => password,
            Err(_) => {
                let _ = store.reschedule_outbox_append(
                    &entry.id,
                    now_ms.saturating_add(backoff_ms(entry.append_attempts.max(1))),
                    Some("la credenziale IMAP non è disponibile"),
                    lease,
                );
                return Err(format!("outbox row {:?}: la credenziale IMAP non è disponibile.", entry.id));
            }
        };
        let mut config = imap_config(&server, &password, self.extra_roots.clone());
        config.limits.max_literal = config.limits.max_literal.max(raw.len().saturating_add(1024)).min(postino_net::Limits::HARD_MAX_LITERAL);
        if postino_sync::append_sent(&config, raw.as_slice()).is_err() {
            // No server text: the IMAP server holds the password and can echo it in a `NO`.
            let sentence = "la copia in Inviati non è stata salvata; il messaggio è già stato inviato e non viene rinviato";
            let _ = store.reschedule_outbox_append(&entry.id, now_ms.saturating_add(backoff_ms(entry.append_attempts.max(1))), Some(sentence), lease);
            return Err(format!("outbox row {:?}: {sentence}.", entry.id));
        }
        if !store.finish_outbox_append(&entry.id, lease).map_err(|_| format!("outbox row {:?}: lo stato non è aggiornabile.", entry.id))? {
            return Err(format!("la riga {:?} è cambiata mentre la copia in Inviati veniva salvata; stato lasciato com'è.", entry.id));
        }
        let _ = self.finalize_outbox_draft(&envelope.draft_id, Some(&envelope.message_id), envelope.draft_revision, store);
        Ok(())
    }

    /// Whether this engine's draft still serializes to the same bytes as the saved draft row.
    ///
    /// The `drafts` row carries the body, headers, revision and updated-at, but not the attachments;
    /// the attachment rows carry the metadata and stored blob hashes. The `updated_at` check catches
    /// any save by another engine (including an attachment-only one); the attachment rows are
    /// compared too because two saves can land in the same millisecond. `in_reply_to` is compared by
    /// refusing a stored linkage: this milestone's composer has no reply field and
    /// `submission_for_draft` never serializes one, so sending such a row would silently drop the
    /// linkage. Refusing is the honest result until the composer owns the field.
    fn draft_snapshot_matches(&self, message: &Message, draft: &postino_store::Draft, store: &Store) -> Result<bool, OpError> {
        if draft.in_reply_to.is_some() || !draft_matches_row(message, draft) || message.summary.date.timestamp_millis() != draft.updated_at {
            return Ok(false);
        }
        let rows = store.attachments(&message.summary.id).map_err(store_map::read_error)?;
        let mut stored = rows.into_iter().map(|row| (row.id, row.filename, row.mime, row.size, row.blob_hash)).collect::<Vec<_>>();
        let mut memory = message
            .attachments
            .iter()
            .map(|attachment| {
                (
                    attachment.id.clone(),
                    attachment.filename.clone(),
                    attachment.mime.clone(),
                    attachment.size,
                    self.blob_hashes.get(&attachment.id).cloned(),
                )
            })
            .collect::<Vec<_>>();
        stored.sort();
        memory.sort();
        Ok(stored == memory)
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
        draft_id: &str,
        message_id: Option<&str>,
        expected_revision: Option<u32>,
        store: &Store,
    ) -> Result<FinalizeOutcome, OpError> {
        let Some(message) = self.messages.iter().find(|message| message.summary.id == draft_id) else {
            return Ok(FinalizeOutcome::NotADraft);
        };
        let Some(folder) = self.folders.iter().find(|folder| folder.id == message.summary.folder_id) else {
            return Ok(FinalizeOutcome::NotADraft);
        };
        if folder.kind != FolderKind::Drafts {
            return Ok(FinalizeOutcome::NotADraft);
        }
        if let Some(expected) = expected_revision {
            match store.get_draft(draft_id).map_err(store_map::read_error)? {
                Some(draft) if draft.revision == expected && self.draft_snapshot_matches(message, &draft, store)? => {}
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
        let index =
            self.messages.iter().position(|message| message.summary.id == id).ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}")))?;
        let Some(current) = self.messages.get(index) else {
            return Err(OpError::InvalidArgs(format!("unknown draft {id:?}")));
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
            self.messages.retain(|message| message.summary.id != *duplicate);
            let prefix = format!("att_{duplicate}_");
            self.blob_hashes.retain(|attachment_id, _| !attachment_id.starts_with(&prefix));
        }

        let Some(slot) = self.messages.iter_mut().find(|message| message.summary.id == id) else {
            return Err(OpError::Failed(format!("unknown draft {id:?}")));
        };
        *slot = message;
        let summary = self
            .messages
            .iter()
            .find(|message| message.summary.id == id)
            .map(|message| message.summary.clone())
            .ok_or_else(|| OpError::Failed(format!("unknown draft {id:?}")))?;
        self.recount();
        Ok((summary, adopted_uid))
    }

    /// Call any operation by name with JSON arguments. This is the only door a frontend uses.
    pub fn call(&mut self, name: &str, args: Value) -> Result<Value, OpError> {
        ops::dispatch(self, name, args)
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
        let Some(message_index) = self.messages.iter().position(|message| message.summary.id == message_id) else {
            return Err(unknown_message(message_id));
        };
        let Some(attachment_index) =
            self.messages.get(message_index).and_then(|message| message.attachments.iter().position(|attachment| attachment.id == attachment_id))
        else {
            return Err(OpError::InvalidArgs(format!(
                "attachment {attachment_id:?} is not on message {message_id:?}; each attachment id belongs to exactly one message"
            )));
        };
        let hash = store.blobs().put(bytes).map_err(store_map::blob_write_error)?;
        store.set_attachment_blob_hash(message_id, attachment_id, Some(&hash), size).map_err(store_map::write_error)?;
        // Only now, after both durable writes, does memory change.
        self.blob_hashes.insert(attachment_id.to_owned(), hash);
        if let Some(attachment) = self.messages.get_mut(message_index).and_then(|message| message.attachments.get_mut(attachment_index)) {
            attachment.size = size;
        }
        Ok(())
    }

    /// The BLAKE3 hash of an attachment's stored bytes, when this engine knows one.
    ///
    /// `Attachment` carries no storage detail (see the `blob_hashes` field), so this is the readable
    /// half of the in-memory mapping. It is not an operation and has no JSON schema.
    pub fn attachment_hash(&self, attachment_id: &str) -> Option<&str> {
        self.blob_hashes.get(attachment_id).map(String::as_str)
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
    fn find_attachment(&self, attachment_id: &str) -> Result<&Attachment, OpError> {
        self.messages
            .iter()
            .flat_map(|message| message.attachments.iter())
            .find(|attachment| attachment.id == attachment_id)
            .ok_or_else(|| OpError::InvalidArgs(format!("unknown attachment {attachment_id:?}; attachment ids come from message_get")))
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
            if !self.messages.iter().any(|message| message.summary.id == candidate) {
                return Ok(candidate);
            }
        }
        Err(OpError::Failed(format!("could not allocate a new {prefix} id")))
    }

    pub(crate) fn recount(&mut self) {
        for folder in &mut self.folders {
            let (total, unread) = if folder.kind == FolderKind::Starred {
                self.messages.iter().filter(|message| message.summary.starred).fold((0u32, 0u32), |(total, unread), message| {
                    (total.saturating_add(1), unread.saturating_add(if message.summary.unread { 1 } else { 0 }))
                })
            } else {
                self.messages.iter().filter(|message| message.summary.folder_id == folder.id).fold((0u32, 0u32), |(total, unread), message| {
                    (total.saturating_add(1), unread.saturating_add(if message.summary.unread { 1 } else { 0 }))
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
        SmtpError::Rejected { code, stage, .. } => format!("il server ha rifiutato la fase {stage} con il codice {code}"),
        SmtpError::Transient { stage, .. } => format!("la fase {stage} è fallita in modo temporaneo"),
        SmtpError::Permanent { stage, .. } => format!("la fase {stage} è fallita in modo definitivo"),
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
    let Some(expected) = envelope.draft_revision else {
        return Ok(None);
    };
    match store.get_draft(&envelope.draft_id).map_err(store_map::read_error)? {
        None => Ok(Some("la bozza non esiste più; il messaggio non è stato inviato")),
        Some(draft) if draft.revision != expected => Ok(Some("la bozza è cambiata dopo l'accodamento; il messaggio non è stato inviato")),
        Some(_) => Ok(None),
    }
}

/// Whether this engine's draft snapshot still matches the saved draft row's content. `false` means
/// another process saved the draft after this engine loaded it.
fn draft_matches_row(message: &Message, draft: &postino_store::Draft) -> bool {
    fn same_addresses(engine: &[Address], row: &[postino_store::Address]) -> bool {
        engine.len() == row.len() && engine.iter().zip(row).all(|(left, right)| left.name == right.name && left.email == right.email)
    }
    message.body_text == draft.body_text
        && message.summary.subject == draft.subject
        && same_addresses(&message.summary.to, &draft.to)
        && same_addresses(&message.cc, &draft.cc)
        && same_addresses(&message.bcc, &draft.bcc)
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

fn imap_config(server: &postino_store::AccountServer, password: &Zeroizing<String>, extra_roots: Vec<CertificateDer<'static>>) -> ImapConfig {
    let mut config = ImapConfig::new(
        server.host.clone(),
        server.port,
        store_map::net_tls(store_map::mail_tls(server.tls)),
        server.username.clone(),
        Secret::new(password.as_str().to_owned()),
    );
    for root in extra_roots {
        config = config.with_extra_root(root);
    }
    config
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

fn is_bidi_or_format_control(character: char) -> bool {
    matches!(character, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
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

fn validate_addresses(field: &str, addresses: &[Address]) -> Result<(), OpError> {
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
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(OpError::InvalidArgs(format!("a draft can carry at most {MAX_ATTACHMENTS} attachments")));
    }
    for attachment in attachments {
        validate_header("attachment id", &attachment.id, 128)?;
        validate_header("attachment filename", &attachment.filename, 255)?;
        validate_header("attachment type", &attachment.mime, 127)?;
        if attachment.filename.trim().is_empty() || attachment.filename.contains(['/', '\\', ':']) {
            return Err(OpError::InvalidArgs("attachment filenames must be a plain file name".to_owned()));
        }
        if attachment.size > MAX_ATTACHMENT_BYTES {
            return Err(OpError::InvalidArgs(format!("attachment {:?} is larger than {MAX_ATTACHMENT_BYTES} bytes", attachment.filename)));
        }
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
        assert_eq!(engine.messages()[0].summary.date, now - chrono::Duration::minutes(8));
        assert!(engine.messages().iter().all(|message| message.summary.date < now));
    }

    #[test]
    fn flags_update_folder_counts() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].summary.id.clone();
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
