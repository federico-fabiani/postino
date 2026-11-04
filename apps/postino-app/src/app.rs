//! The egui shell. Panels are deliberately plain: folder rail, message table, reading pane.
//! Every action goes through `Engine::call`, the same operation registry the CLI and MCP expose.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Datelike, Local, Utc};
use eframe::egui::containers::Sides;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Frame, Id, Key, Layout, Margin, Modifiers, Rect, ResizeDirection,
    RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder, UserData, Vec2, ViewportCommand, WidgetInfo, WidgetType, pos2,
};
use postino_core::{
    Account, Address, Attachment, AttachmentGet, BodyFormat, CalendarInviteView, DraftSave, Engine, FolderKind, Identity, InviteResponse, LabelApply,
    LabelColor, LabelCreate, LabelDelete, LabelSummary, LabelUpdate, MAX_EML_BYTES, Message, MessageExport, MessageImport, MessageSummary,
    MessageTemplate, MessagesList, OutboxList, OutboxRunReport, OutboxView, PgpDecryptResult, PgpDecryptState, PgpKey, PgpMimeKind, PgpPublicKey,
    PgpVerifyResult, PgpVerifyState, SavedSearch, SearchResult, SendQueued, SyncPhase, ThreadGetResult, ThreadSummary, ThreadsListResult,
    UnsubscribeInfo, UnsubscribeMethod, UnsubscribeTarget,
};
use postino_net::CertificateDer;
use serde_json::Value;
use serde_json::json;
use zeroize::Zeroizing;

use crate::background;
use crate::body;
use crate::body::BodyRenderer;
use crate::body_ui;
use crate::boot::{self, BootFailure, Launch, StoreHandle};
use crate::daemon;
use crate::onboarding::{self, ConnectionError, ErrorKind, FocusField, FolderPreview, FolderRole, FoldersPreview, Onboarding, ProviderChoice, Stage};
use crate::reading::{self, DangerousKind, ExtractedLink};
use crate::rules_ui::RulesPage;
use crate::seams::{
    CancellingFileSaver, FilePicker, FileSaver, FolderOpener, LinkOpener, RfdFilePicker, RfdFileSaver, SystemFolderOpener, SystemLinkOpener,
    UnavailableFilePicker, UnavailableFolderOpener, UnavailableLinkOpener,
};
use crate::send_worker::{SendEvent, SendWorker, StoreSendBackend};
use crate::settings::Settings;
use crate::sync_worker::{SyncEvent, SyncProgress, SyncRequest, SyncWorker};
use crate::theme::{self, GOLD, IVORY, Tokens};
use crate::ui::icons;
use crate::ui::tokens;
use crate::ui::tokens::{inset, radius, size as ui_size, space, type_size};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{
    AUTOSAVE_DELAY, Badge, BadgeKind, Button, ButtonSize, Chip, ComposeMode, Composer, ComposerFocus, ComposerState, DEMO_SEND_REASON, Dialog,
    EmptyState, IconButton, IconButtonSize, ListRow, Menu, MenuItem, NO_SMTP_SEND_REASON, SearchField, SectionHeader, TextField, Toast, ToastKind,
    ToastResult,
};

mod accounts;
mod compose;
mod list;
mod reading_pane;
mod screenshot;
mod settings_dialogs;
mod shell;

/// Which view the command line asked for. Only the tray passes `Outbox`; a desktop launch keeps
/// the mail list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StartupView {
    #[default]
    Mail,
    Outbox,
}

impl StartupView {
    /// The one `--open` vocabulary, shared by the parser and the launch path so a new view cannot
    /// parse but never open.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "outbox" => Some(Self::Outbox),
            _ => None,
        }
    }
}

/// A tray request delivered over the daemon event subscription. It focuses one process instead of
/// starting a second one, and it is the only way a hidden window comes back without a tray click
/// landing on a dead icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    Open,
    Outbox,
    Quit,
}

impl Activation {
    /// The bounded vocabulary the daemon publishes; anything else is ignored.
    pub fn parse(action: &str) -> Option<Self> {
        match action {
            "open" => Some(Self::Open),
            "outbox" => Some(Self::Outbox),
            "quit" => Some(Self::Quit),
            _ => None,
        }
    }
}

/// One row the user toggled, applied only after the settings page reads the result back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundChange {
    Tray(bool),
    Notify(bool),
    Previews(bool),
    Autostart(bool),
}
/// The shell scenes `--screenshot` can render. Add a label here and a branch in
/// [`PostinoApp::seed_screenshot`] to grow the set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotState {
    Shell,
    Reading,
    ReadingFull,
    Compose,
    Palette,
    Labels,
    LabelMenu,
    LabelEditor,
    EmptyUnread,
    EmptyAttachments,
    EmptyLabelUnread,
    OnboardingWelcome,
    OnboardingProvider,
    OnboardingForm,
    OnboardingError,
    OnboardingFolders,
    OnboardingSync,
    Mailbox,
    Offline,
    ReadingAttachments,
    ReadingInvite,
    LinkConfirm,
    Settings,
    SettingsSignature,
    Outbox,
    Search,
    SearchError,
    Threads,
    Conversation,
    SavedSearches,
    Rules,
    RuleEditor,
    RulePreview,
}

impl ScreenshotState {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "shell" => Ok(Self::Shell),
            "reading" => Ok(Self::Reading),
            "reading_full" => Ok(Self::ReadingFull),
            "compose" => Ok(Self::Compose),
            "palette" => Ok(Self::Palette),
            "labels" => Ok(Self::Labels),
            "label_menu" => Ok(Self::LabelMenu),
            "label_editor" => Ok(Self::LabelEditor),
            "empty_unread" => Ok(Self::EmptyUnread),
            "empty_attachments" => Ok(Self::EmptyAttachments),
            "empty_label_unread" => Ok(Self::EmptyLabelUnread),
            "onboarding_welcome" => Ok(Self::OnboardingWelcome),
            "onboarding_provider" => Ok(Self::OnboardingProvider),
            "onboarding_form" => Ok(Self::OnboardingForm),
            "onboarding_error" => Ok(Self::OnboardingError),
            "onboarding_folders" => Ok(Self::OnboardingFolders),
            "onboarding_sync" => Ok(Self::OnboardingSync),
            "mailbox" => Ok(Self::Mailbox),
            "offline" => Ok(Self::Offline),
            "reading_attachments" => Ok(Self::ReadingAttachments),
            "reading_invite" => Ok(Self::ReadingInvite),
            "link_confirm" => Ok(Self::LinkConfirm),
            "settings" => Ok(Self::Settings),
            "settings_signature" => Ok(Self::SettingsSignature),
            "outbox" => Ok(Self::Outbox),
            "search" => Ok(Self::Search),
            "search_error" => Ok(Self::SearchError),
            "threads" => Ok(Self::Threads),
            "conversation" => Ok(Self::Conversation),
            "saved_searches" => Ok(Self::SavedSearches),
            "rules" => Ok(Self::Rules),
            "rule_editor" => Ok(Self::RuleEditor),
            "rule_preview" => Ok(Self::RulePreview),
            other => Err(format!(
                "unknown screenshot state {other:?}; expected one of: shell, reading, reading_full, compose, palette, labels, label_menu, label_editor, \
                 empty_unread, empty_attachments, empty_label_unread, onboarding_welcome, onboarding_provider, onboarding_form, onboarding_error, \
                 onboarding_folders, onboarding_sync, mailbox, offline, reading_attachments, reading_invite, link_confirm, settings, settings_signature, outbox, \
                 search, search_error, threads, conversation, saved_searches, rules, rule_editor, rule_preview"
            )),
        }
    }
}

/// A screenshot request in flight. The event can only arrive on a later frame, so the mode counts
/// frames both before the request and while waiting for the reply.
struct ScreenshotMode {
    out: PathBuf,
    frames: u32,
    requested: bool,
    waited: u32,
}

/// The instant every screenshot formats its dates against. Frozen so a capture does not change
/// when the wall clock crosses a minute, and quoted verbatim in `docs/development.md`.
pub const SCREENSHOT_CLOCK: &str = "2026-10-24T10:42:00Z";

/// Parse [`SCREENSHOT_CLOCK`]. A literal cannot fail in practice, but the parser returns a reason
/// instead of panicking so a future edit to the constant cannot crash a capture.
pub fn screenshot_clock() -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(SCREENSHOT_CLOCK)
        .map(|date| date.with_timezone(&Utc))
        .map_err(|error| format!("invalid screenshot clock {SCREENSHOT_CLOCK:?}: {error}"))
}

/// Below this available width a side reading pane leaves no usable list, so reading becomes a
/// full-width overlay with a back button.
const READING_PANE_MIN_WIDTH: f32 = 1000.0;
/// The pane opens at 52% of the content width *after* the 240 pt sidebar, resizes between 420 px
/// and 62%, and stops shrinking the list below 460 px whenever the geometry allows it.
const READING_PANE_DEFAULT_RATIO: f32 = 0.52;
/// How often the tick asks the store whether the outbox has work. The app is the only scheduler in
/// this build, and a row can be written by another process.
const OUTBOX_POLL_SECONDS: f64 = 2.0;
const READING_PANE_MAX_RATIO: f32 = 0.62;
const READING_PANE_MIN_SIZE: f32 = 420.0;
/// The message list shows avatars only when its column is at least this wide (`docs/ui/plan.md`).
const AVATAR_MIN_LIST_WIDTH: f32 = 720.0;
/// The composer switches from a sheet to a centred card only when there is room for both the
/// declared minimum window and a card that does not touch the status bar.
const COMPOSER_CARD_MIN_WIDTH: f32 = 1000.0;
const COMPOSER_CARD_MIN_HEIGHT: f32 = 700.0;
/// The reading body keeps at least this much room when an attachment strip shares the pane.
const READING_BODY_MIN: f32 = 48.0;
/// `label_apply` accepts at most 500 ids per call; undoing a delete restores in these batches.
const LABEL_APPLY_CHUNK: usize = 500;

/// Which process command is in flight. `Start` releases the writer lock before spawning, `Stop`
/// takes it back after the daemon exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonJob {
    Start,
    Stop,
}

/// A start/stop request running off the UI thread, polled once per frame.
struct DaemonJobState {
    kind: DaemonJob,
    receiver: std::sync::mpsc::Receiver<Result<(), String>>,
}

/// One counter from a daemon event body; a missing or negative value is zero, never a panic.
fn count(data: &Value, key: &str) -> u32 {
    data.get(key).and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok()).unwrap_or_default()
}

/// The search-index status carried by an `index` event. The daemon publishes the full shape; the
/// counters are enough for the banner if a future daemon omits the optional fields, so the build
/// never appears stalled because one field was missing.
fn index_status(data: &Value) -> Option<postino_core::SearchStatusResult> {
    if let Ok(status) = serde_json::from_value::<postino_core::SearchStatusResult>(data.clone()) {
        return Some(status);
    }
    let state = data.get("state").and_then(Value::as_str)?;
    Some(postino_core::SearchStatusResult {
        ready: data.get("ready").and_then(Value::as_bool).unwrap_or(false),
        indexed: count(data, "indexed"),
        total: count(data, "total"),
        last_error: data.get("last_error").and_then(Value::as_str).map(str::to_owned),
        size_bytes: data.get("size_bytes").and_then(Value::as_u64).unwrap_or_default(),
        state: state.to_owned(),
        updated_at: data.get("updated_at").and_then(Value::as_i64).unwrap_or_default(),
    })
}

pub struct PostinoApp {
    engine: Engine,
    folder: String,
    /// The account whose mail is on screen. Every folder view is scoped to it.
    active_account: Option<String>,
    /// `Some` only for a normal launch that opened the encrypted store. The sync worker and the
    /// post-sync reload use this handle; a demo session has none.
    store: Option<StoreHandle>,
    store_path: Option<PathBuf>,
    /// The store handle a demo opened from onboarding or recovery can return to. `None` for a
    /// real `--demo` launch and after the user has left the demo.
    demo_return: Option<StoreHandle>,
    /// The store's single-writer lock when the app is in its in-process mode. P10a2 replaces this
    /// with a daemon client; until then the app owns the store and refuses to start beside a daemon.
    writer_lock: Option<postino_store::WriterLock>,
    /// True for the synthetic mailbox, including a screenshot that seeds the store UI.
    demo: bool,
    /// The command line choice, so the recovery screen can retry the same store.
    launch: Launch,
    boot_failure: Option<BootFailure>,
    onboarding: Option<Onboarding>,
    sync_worker: Option<SyncWorker>,
    sync_view: SyncView,
    /// The outbox thread and the last read of the journal. Both are absent/empty in the demo.
    send_worker: Option<SendWorker>,
    outbox: OutboxList,
    outbox_open: bool,
    outbox_confirm: Option<OutboxConfirm>,
    outbox_loaded: bool,
    /// The last time the tick polled the store for outbox work, in egui seconds.
    last_outbox_poll: f64,
    /// The instant the next queued row becomes due, in Unix milliseconds.
    pending_send: Option<i64>,
    /// The account the running worker was asked to sync. Used to ignore late events from an account
    /// the user has switched away from.
    sync_account: Option<String>,
    /// An account creation running on its own thread (it probes the server through the engine, so
    /// the UI must not block on it).
    add_account_job: Option<std::sync::mpsc::Receiver<Result<(String, String), String>>>,
    /// Last successful sync instant, kept across a running attempt so an offline failure can still
    /// say when the mailbox was last updated.
    last_sync: Option<i64>,
    show_accounts_menu: bool,
    show_accounts_dialog: bool,
    /// The P9c Rules page. A full-page modal over the shell; opening it is a command, not a new
    /// route, so the mailbox keeps its selection and scroll state.
    rules_page: Option<RulesPage>,
    account_remove: Option<AccountRemoveState>,
    /// Extra trust anchors for the fake server in tests. Empty in production.
    extra_roots: Vec<CertificateDer<'static>>,
    /// Keyboard focus and the message shown in the reading pane.
    selected: Option<String>,
    /// The rows painted as selected. It always contains `selected` when non-empty.
    selection: BTreeSet<String>,
    /// Shift-click and Shift+arrow range anchor.
    anchor: Option<String>,
    /// Active label id. Alone it spans every folder; `label_folder` narrows it to one folder.
    label_filter: Option<String>,
    /// Folder the active label is narrowed to, or `None` for every folder.
    label_folder: Option<String>,
    /// Cached `labels_list` result. Refreshed after every label or unread mutation, so a frame
    /// never re-decodes the registry through JSON three times.
    labels_cache: Vec<LabelSummary>,
    label_editor: Option<LabelEditor>,
    label_delete: Option<LabelDeleteState>,
    label_menu: Option<LabelMenuState>,
    pending_read: Option<PendingRead>,
    toast: Option<ToastState>,
    query: String,
    /// The query the result list is actually showing. Typing starts a 250 ms debounce; Enter
    /// commits immediately and Esc clears both. The field and the results never disagree.
    search_committed: String,
    /// Wall-clock seconds (`self.now`) at which the debounce expires.
    search_debounce_until: Option<f64>,
    /// The last parse error, with the byte position the pencil points at.
    search_error: Option<(String, usize)>,
    /// The last index status seen while a search was waiting for the FTS build. `None` means ready.
    index_progress: Option<postino_core::SearchStatusResult>,
    /// The boot status check ran once.
    index_checked: bool,
    /// The last search total was capped at 10 000.
    search_capped: bool,
    /// Page size the search list requests; `Mostra altri` grows it by 200 up to 5 000.
    search_limit: u32,
    /// The last list/search read, keyed by its full signature and the engine revision. A frame that
    /// changes nothing reuses it instead of querying SQL again.
    list_cache: Option<(String, u64, Vec<MessageSummary>, usize, usize, bool)>,
    /// Folder views group by conversation by default; a search starts flat unless the user toggles
    /// the threaded view. `threads_view_before_search` restores the folder choice when the query
    /// is cleared.
    threads_view: bool,
    threads_view_before_search: Option<bool>,
    /// The last `threads_list` read, keyed like `list_cache`.
    threads_cache: Option<(String, u64, Vec<ThreadSummary>, u32, u32, u32)>,
    /// How many conversation rows the threaded list has loaded; "Carica altre" grows it by 200.
    thread_list_limit: u32,
    /// The conversation shown in the reading pane and the newest page of its messages.
    selected_thread: Option<String>,
    thread_messages: Vec<MessageSummary>,
    thread_summary: Option<ThreadSummary>,
    thread_total: u32,
    /// Offset of the first loaded message. Older pages are prepended; the newest page is loaded first.
    thread_offset: u32,
    thread_open: Option<String>,
    /// Saved searches and the last eight queries, kept in the engine's account order.
    saved_searches: Vec<SavedSearch>,
    recent_searches: Vec<String>,
    /// The engine revision the saved-search rows were last refreshed at, plus a 2 s poll for a
    /// second process's CRUD.
    saved_searches_revision: Option<u64>,
    last_saved_refresh: f64,
    /// The saved search currently applied, for the sidebar's selected state.
    applied_saved: Option<String>,
    saved_editor: Option<SavedSearchEditor>,
    /// Snapshot restored by Esc after a saved search replaced the query and filters.
    saved_restore: Option<SavedRestore>,
    /// True while a saved search owns the scope; `saved_query_folder` says whether its query names a
    /// folder. A query without `folder:` is account-wide and the folder is not highlighted.
    saved_folder_scope: bool,
    saved_query_folder: bool,
    /// Why the applied saved search is stale, shown inline until the next search.
    saved_error: Option<String>,
    /// True while a saved search is invalid only because the FTS index is still building.
    saved_index_pending: bool,
    /// Record the committed query in the recent list. Only Enter, apply and an explicit commit set
    /// this; the 250 ms debounce must not fill the history with prefixes.
    recording_recent: bool,
    /// Reveal the RICERCHE section on the next sidebar frame (after save or apply).
    scroll_saved_into_view: bool,
    /// The sidebar scroll offset computed from the last RICERCHE reveal, applied next frame.
    sidebar_scroll_target: Option<f32>,
    /// True while the centred search field owns the keyboard; the empty-focus recent chips read it.
    search_focus: bool,
    unread_only: bool,
    attachments_only: bool,
    dark: bool,
    status: String,
    /// One Italian sentence for the current operation failure. While it is set the status bar says
    /// `Errore` and a toast carries the sentence; the raw engine error never reaches the UI.
    failure: Option<Failure>,
    /// A list read that already reported its failure must not re-open the same toast every frame
    /// after the user dismisses it.
    list_error_ack: bool,
    /// The folder/filters the acknowledgement belongs to. A new view retries the read and may
    /// report a new failure.
    list_signature: String,
    /// When a long operation started, for the delayed loading line.
    loading: Option<f64>,
    show_about: bool,
    show_palette: bool,
    palette_query: String,
    palette_focus: bool,
    palette_selected: usize,
    focus_search: bool,
    show_shortcuts: bool,
    shortcut_query: String,
    shortcut_focus: bool,
    scroll_to_selected: bool,
    now: f64,
    /// The wall clock every date is formatted against. Normal mode updates it each frame; a
    /// screenshot freezes it so two runs a minute apart render identical bytes.
    now_utc: DateTime<Utc>,
    custom_chrome: bool,
    reading_open: bool,
    reading_full: bool,
    /// The saved fraction of the width left by the sidebar. The pane is resizable; the last width
    /// that fits the clamps is written back to settings.
    reading_ratio: f32,
    reduced_motion: bool,
    /// `None` in screenshot mode and in tests, so a docs capture never reads or writes the
    /// user's preferences.
    settings_path: Option<PathBuf>,
    settings_dirty: bool,
    settings_saved_at: f64,
    composer: ComposerState,
    /// The email-safe HTML preview sheet over the composer. It is display-only and rebuilds from
    /// the source every frame.
    composer_preview: bool,
    /// The widget that owned the keyboard when the composer opened. Closing the modal returns
    /// focus there instead of dropping the user on the shell with no focus at all.
    composer_return_focus: Option<Id>,
    composer_focus_pending: bool,
    /// The egui id of each visible message row, so `J`/`K` can move real keyboard focus and the
    /// truncation tooltips also work without a mouse.
    row_ids: BTreeMap<String, Id>,
    focus_selected_row: bool,
    /// Deterministic PNG capture, set only by `--screenshot`.
    screenshot: Option<ScreenshotMode>,
    /// The operating-system seams. Tests replace the defaults with recorders; production launch
    /// installs the browser, rfd and file-manager adapters.
    link_opener: Arc<dyn LinkOpener>,
    file_saver: Arc<dyn FileSaver>,
    file_picker: Arc<dyn FilePicker>,
    folder_opener: Arc<dyn FolderOpener>,
    /// Everything the reading pane owns while a message is open: the cached body, the invite,
    /// the pending link, the attachment sheets and the unsubscribe choice. The struct lives in
    /// `reading_pane.rs`, next to the code that reads it.
    reading: reading_pane::ReadingUi,
    /// A message override used only by deterministic screenshots (HTML plus attachment fixtures).
    screenshot_message: Option<Message>,
    /// The invite card shown for [`Self::screenshot_message`], when the scene is an invite.
    screenshot_invite: Option<CalendarInviteView>,
    /// True while the reading pane shows a received, valid invite that can be answered. The three
    /// invite commands read this rule; the card never carries its own enabled state.
    invite_ready: bool,
    /// The last message id a verify was attempted for, including a non-PGP or failed result, so the
    /// pane never re-runs the operation on every frame.
    pgp_checked: Option<String>,
    /// The one passphrase sheet: decrypting the open message or sending a signed draft.
    pgp_prompt: Option<PgpPromptState>,
    /// The account's public key rows, loaded when Settings or the composer needs them.
    pgp_keys: Vec<PgpKey>,
    pgp_keys_loaded: bool,
    /// After a key-list failure, wait before retrying so a disconnected daemon is not polled every
    /// frame. A user import/delete or a send with a missing key forces a new attempt.
    pgp_keys_retry_at: f64,
    /// The key import sheet. Key material and the passphrase stay in this state only.
    pgp_import: Option<PgpImportState>,
    /// The key waiting for the delete confirmation.
    pgp_delete: Option<PgpDeleteState>,
    /// A one-line result under the PGP settings section.
    pgp_notice: Option<String>,
    /// Local compose templates. Global to the profile, loaded once for the composer and Settings.
    templates: Vec<MessageTemplate>,
    templates_loaded: bool,
    /// The create/edit template sheet. `id: None` creates a new row.
    template_editor: Option<TemplateEditorState>,
    /// The template waiting for the delete confirmation.
    template_delete: Option<MessageTemplate>,
    /// Alternate senders for the active account, loaded once for the composer and Settings.
    identities: Vec<Identity>,
    identities_loaded: bool,
    identities_retry_at: f64,
    /// The create/edit identity sheet. `id: None` creates a new row.
    identity_editor: Option<IdentityEditorState>,
    /// The identity waiting for the delete confirmation.
    identity_delete: Option<Identity>,
    /// A one-line result under the identities settings section.
    identities_notice: Option<String>,
    /// The sidebar account row, used to return focus when settings closes.
    account_switcher_id: Option<Id>,
    /// The widget to restore when the settings dialog closes.
    settings_return_focus: Option<Id>,
    settings_focus_pending: bool,
    /// Per-account plain-text signature editor in Settings → Account. `None` while collapsed.
    signature_editor: Option<SignatureEditor>,
    /// The account whose signature button should get focus after Escape cancelled the editor.
    signature_focus_account: Option<String>,
    /// Request focus on the sidebar account row on the next frame. Used after closing settings.
    focus_account_switcher: bool,
    /// The attribution document is a nested dialog, not a browser or a shell.
    show_licences: bool,
    licences_return_focus: Option<Id>,
    licences_focus_pending: bool,
    /// Whether the theme follows the operating system instead of `dark`.
    system_theme: bool,
    /// Who owns the store: a live daemon this window writes through, or this window itself.
    connection: daemon::Connection,
    /// The one write path while a daemon owns the store. `None` in-process and in demo mode.
    daemon_link: Option<daemon::Link>,
    /// The subscription that turns daemon events into view updates, with bounded reconnects.
    daemon_events: Option<daemon::EventWorker>,
    /// A start/stop request in flight: the window must never freeze for a process spawn.
    daemon_job: Option<DaemonJobState>,
    /// The daemon/platform bridge behind the four background rows. Tests inject a recorder.
    background: Box<dyn background::Bridge>,
    /// The last successful read; `None` until the settings page opens.
    background_rows: Option<background::Rows>,
    /// A toggle waiting to be written to the daemon or the platform.
    background_change: Option<BackgroundChange>,
    /// The command line asked for a view other than the mail list.
    startup_open: StartupView,
    /// The one-time close-to-tray explanation is open.
    close_prompt: bool,
    /// Set only for an explicit exit, so eframe's next close request is not intercepted again.
    allow_close: bool,
    /// The first-hide notice was accepted; closing now hides without asking again.
    tray_notice_shown: bool,
    /// The window is minimized to the tray, so a lost daemon must bring it back instead of leaving
    /// an orphan process with no icon.
    hidden: bool,
    /// A tray activation parsed from the daemon events, applied once per frame with the context.
    pending_activation: Option<Activation>,
}

/// Runs one user action behind `catch_unwind`. A panic in a handler is contained here instead of
/// escaping through the frame and closing the window with unsaved work in it.
fn guarded<R>(action: impl FnOnce() -> R) -> Option<R> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).ok()
}

impl PostinoApp {
    /// Normal desktop launch. The command line chose demo or a store; a failure is a recovery
    /// screen, never a panic and never an empty mailbox.
    pub fn launch(cc: &eframe::CreationContext<'_>, custom_chrome: bool, launch: Launch, startup: StartupView) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let settings_path = Settings::path();
        let settings = settings_path.as_deref().map_or_else(Settings::default, Settings::load_from);
        theme::apply(&cc.egui_ctx, settings.dark);
        let reduced = settings.reduced_motion || crate::settings::reduced_motion_from_env();
        theme::apply_motion(&cc.egui_ctx, reduced);
        let now = Utc::now();
        let mut app = match boot::boot(launch.clone(), boot::KeySource::Keyring, now) {
            Ok(booted) => Self::from_booted(booted, custom_chrome, now, launch, Vec::new()),
            Err(failure) => Self::with_engine(Engine::empty(), None, false, Some(failure), launch, custom_chrome, now, Vec::new()),
        };
        app.reading_ratio = settings.reading_pane_ratio;
        app.apply_settings(settings);
        app.reduced_motion = reduced;
        app.settings_path = settings_path;
        app.link_opener = Arc::new(SystemLinkOpener);
        app.file_saver = Arc::new(RfdFileSaver);
        if !app.demo {
            app.file_picker = Arc::new(RfdFilePicker);
        }
        app.folder_opener = Arc::new(SystemFolderOpener);
        if app.system_theme
            && let Some(theme) = cc.egui_ctx.system_theme()
        {
            app.dark = theme == egui::Theme::Dark;
            theme::apply(&cc.egui_ctx, app.dark);
        }
        // A later launch shows the cache first and refreshes it in the background; first-run
        // onboarding has no account yet, and the demo has no server.
        if !app.demo
            && app.onboarding.is_none()
            && let Some(account) = app.active_account.clone()
        {
            app.start_sync_for(Some(account));
        }
        // One path for the command line, the tray activation and the unit tests: a startup view is
        // applied only after the first boot, including when the daemon connects a moment later.
        app.apply_launch_view(startup);
        app
    }
    fn from_booted(booted: boot::Booted, custom_chrome: bool, now: DateTime<Utc>, launch: Launch, extra_roots: Vec<CertificateDer<'static>>) -> Self {
        let boot::Booted { engine, store, demo, writer_lock, daemon } = booted;
        let mut app = Self::with_engine(engine, store, demo, None, launch, custom_chrome, now, extra_roots);
        app.writer_lock = writer_lock;
        app.attach_daemon(daemon);
        app
    }

    /// Publish who owns the store. A live daemon replaces the local sync/send workers with the IPC
    /// client and removes every local write path; `None` restores the in-process mode.
    fn attach_daemon(&mut self, hello: Option<daemon::Hello>) {
        self.stop_sync_worker();
        self.send_worker = None;
        self.daemon_events = None;
        self.daemon_link = None;
        let Some(handle) = self.store.clone().filter(|_| !self.demo) else {
            self.connection = if self.demo { daemon::Connection::Demo } else { daemon::Connection::InProcess };
            return;
        };
        match hello {
            Some(hello) => {
                let path = handle.path().to_path_buf();
                match daemon::Link::connect(&path) {
                    Ok(link) => self.daemon_link = Some(link),
                    Err(message) => {
                        // The daemon answered the probe but the channel will not open: stay a
                        // client, tell the truth, and let the next probe/command retry.
                        self.status = message;
                        self.connection = daemon::Connection::Lost;
                        return;
                    }
                }
                if let Ok(paths) = daemon::paths(&path) {
                    self.daemon_events = Some(daemon::EventWorker::start(Box::new(daemon::IpcTransport::new(paths))));
                }
                self.sync_worker = Some(SyncWorker::start(Box::new(crate::sync_worker::DaemonSyncBackend::new(path))));
                self.connection = daemon::Connection::Connected(hello);
            }
            None => {
                self.sync_worker =
                    Some(SyncWorker::start(Box::new(crate::sync_worker::StoreSyncBackend::new(handle.clone(), self.extra_roots.clone()))));
                self.send_worker = Some(SendWorker::start(Box::new(StoreSendBackend::new(handle, self.extra_roots.clone()))));
                self.connection = daemon::Connection::InProcess;
            }
        }
    }

    /// The one write path. With a daemon it is `op {name, args}` over `postino-ipc`, exactly the
    /// schema the CLI and MCP dispatch; without one it is the in-process registry. A read may fall
    /// back to the read-only store while the channel is down; a write never does.
    fn op(&mut self, name: &str, args: Value) -> Result<Value, postino_core::OpError> {
        if self.daemon_link.is_some() {
            let read_only = daemon::is_read_only(name);
            let attempt = match self.daemon_link.as_mut() {
                Some(link) => link.call(name, args.clone()),
                None => Err(daemon::absent_sentence()),
            };
            match attempt {
                Ok(value) => {
                    if !read_only {
                        // The list, thread and saved-search caches are keyed on the local engine
                        // revision, and that revision never advances while a daemon owns the store:
                        // a flag/read/star/label write through `op` must reopen the read-only engine
                        // or the list keeps painting the row it wrote to.
                        self.reload_engine_after_sync(false);
                    }
                    return Ok(value);
                }
                Err(message) => {
                    if read_only && let Ok(value) = self.engine.call(name, args) {
                        return Ok(value);
                    }
                    return Err(postino_core::OpError::Failed(message));
                }
            }
        }
        self.engine.call(name, args)
    }

    /// Drain the daemon subscription once per frame. A dropped channel is a state, not a failure
    /// toast: the mailbox stays readable and the composer keeps its draft. A hidden window whose
    /// daemon is gone reopens itself, because from then on there is no tray icon that could.
    fn poll_daemon(&mut self, ctx: &egui::Context) {
        let events = self.daemon_events.as_mut().map(daemon::EventWorker::poll).unwrap_or_default();
        for event in events {
            match event {
                daemon::Event::Connected(hello) => {
                    let was_live = self.connection.is_live();
                    self.connection = daemon::Connection::Connected(hello);
                    if !was_live {
                        self.reload_engine_after_sync(false);
                        self.status = "Daemon collegato: questa finestra scrive attraverso il canale locale.".to_owned();
                        // The first launch may have probed before the daemon answered: a pending
                        // startup view is applied once, now that the channel is real.
                        self.after_launch();
                    }
                }
                daemon::Event::Reconnecting { attempt } => {
                    self.connection = daemon::Connection::Reconnecting { attempt };
                }
                daemon::Event::Lost => {
                    self.connection = daemon::Connection::Lost;
                    self.status = if self.hidden {
                        "Il daemon non risponde: la finestra è tornata in primo piano, la posta resta leggibile e le modifiche sono sospese."
                            .to_owned()
                    } else {
                        "Il daemon non risponde: la posta resta leggibile, le modifiche sono sospese.".to_owned()
                    };
                    self.reload_engine_after_sync(false);
                    if self.hidden {
                        self.restore_window(ctx);
                    }
                }
                daemon::Event::Daemon(value) => self.apply_daemon_event(value),
            }
        }
        if let Some(activation) = self.pending_activation.take() {
            self.run_activation(ctx, activation);
        }
    }

    fn run_activation(&mut self, ctx: &egui::Context, activation: Activation) {
        match activation {
            Activation::Open => self.restore_window(ctx),
            Activation::Outbox => {
                self.open_outbox();
                self.restore_window(ctx);
            }
            Activation::Quit => {
                // An explicit tray exit closes the window even when it was hidden, so the daemon's
                // shutdown never leaves a process behind with no icon.
                self.allow_close = true;
                self.close_prompt = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn restore_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    /// The same store-backed views the in-process workers refresh, driven by the daemon's bounded
    /// events instead of a polling loop.
    fn apply_daemon_event(&mut self, event: Value) {
        let kind = event.get("kind").and_then(Value::as_str).unwrap_or_default();
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        match kind {
            "activation" => {
                // The tray asks the window it found, never a second process. A quit is explicit, so
                // it does not go through the close prompt.
                if let Some(activation) = data.get("action").and_then(Value::as_str).and_then(Activation::parse) {
                    self.pending_activation = Some(activation);
                }
            }
            "sync" => {
                let phase = data.get("phase").and_then(Value::as_str).unwrap_or_default();
                let account = data.get("account").and_then(Value::as_str).map(str::to_owned);
                match phase {
                    "started" => self.apply_sync_event(SyncEvent::Started { account }),
                    "progress" => {
                        let number = |key: &str| data.get(key).and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok());
                        let progress = SyncProgress {
                            folders_done: number("folders_done").unwrap_or_default(),
                            folders_total: number("folders_total").unwrap_or_default(),
                            messages: number("messages").unwrap_or_default(),
                        };
                        self.apply_sync_event(SyncEvent::Progress { account, progress });
                    }
                    "finished" => {
                        let report = postino_core::SyncReport {
                            account: account.clone().unwrap_or_default(),
                            folders: count(&data, "folders"),
                            fetched: count(&data, "fetched"),
                            updated_flags: count(&data, "updated_flags"),
                            removed: count(&data, "removed"),
                            attachments_stored: count(&data, "attachments_stored"),
                            warnings: Vec::new(),
                        };
                        self.apply_sync_event(SyncEvent::Finished { report });
                    }
                    "failed" => {
                        let category = data.get("error").and_then(Value::as_str).unwrap_or_default();
                        let offline = category == "server unreachable";
                        let message = match category {
                            "authentication failed" => "Il server ha rifiutato le credenziali.",
                            "server unreachable" => "Il server non è raggiungibile: la posta è quella locale.",
                            "shutting down" => "Il daemon si sta chiudendo.",
                            _ => "Sincronizzazione non riuscita.",
                        };
                        self.apply_sync_event(SyncEvent::Failed { account, message: message.to_owned(), cancelled: false, offline });
                    }
                    _ => {}
                }
            }
            "outbox" => self.last_outbox_poll = 0.0,
            "index" => {
                // The daemon publishes the full status shape; a partial payload still moves the
                // banner forward instead of being dropped silently.
                if let Some(status) = index_status(&data) {
                    self.index_progress = Some(status);
                    self.index_checked = true;
                } else {
                    log::debug!("the daemon published an index event without a readable status");
                }
            }
            // Rules and blob GC change messages, labels or attachments without an op of this
            // window: reload the read-only engine so the list, thread and reading pane agree.
            "rules" | "gc" => self.reload_engine_after_sync(false),
            _ => {}
        }
    }

    /// Start the daemon from an explicit command. Releases this window's writer lock first: the
    /// daemon must be the only writer from the instant it opens the store.
    fn request_start_daemon(&mut self) {
        if let Some(reason) = self.connection.reason_start(self.demo) {
            self.status = reason.to_owned();
            return;
        }
        if self.daemon_job.is_some() {
            self.status = "Un comando sul daemon è già in corso.".to_owned();
            return;
        }
        let Some(path) = self.store_path.clone() else {
            self.status = "Non c'è un archivio da affidare al daemon.".to_owned();
            return;
        };
        self.stop_sync_worker();
        self.send_worker = None;
        self.writer_lock = None;
        if let Some(handle) = self.store.clone() {
            match handle.as_read_only().open_engine() {
                Ok(engine) => {
                    self.store = Some(handle.as_read_only());
                    self.engine = engine;
                }
                Err(message) => {
                    self.recover_after_failed_start(format!("Non riesco a riaprire l'archivio mentre avvio il daemon: {message}"));
                    return;
                }
            }
        }
        if let Ok(link) = daemon::Link::connect(&path) {
            self.daemon_link = Some(link);
        }
        self.connection = daemon::Connection::Reconnecting { attempt: 1 };
        self.status = "Avvio del daemon…".to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        let spawn_path = path.clone();
        let spawned = std::thread::Builder::new().name("postino-app-daemon-start".to_owned()).spawn(move || {
            let _ = sender.send(daemon::start(&spawn_path));
        });
        if spawned.is_err() {
            self.recover_after_failed_start("Non riesco ad avviare il processo del daemon.".to_owned());
            return;
        }
        self.daemon_job = Some(DaemonJobState { kind: DaemonJob::Start, receiver });
    }

    /// Every failed "Avvia daemon" path returns here. The worker was already silenced and the writer
    /// lock released, so the window must come back as a coherent whole: in-process with its sync and
    /// send workers when the archive is free, read-only and honestly labelled when another writer
    /// still owns it. Never a state with no workers and a label stuck on "riconnessione".
    fn recover_after_failed_start(&mut self, message: String) {
        let Some(path) = self.store_path.clone() else {
            self.report_failure(message, false);
            return;
        };
        let mut restored = false;
        if let Some(handle) = self.store.clone()
            && let Ok(lock) = postino_store::WriterLock::acquire_as(&path, "app", None)
        {
            match handle.as_writable().open_engine() {
                Ok(engine) => {
                    self.store = Some(handle.as_writable());
                    self.engine = engine;
                    self.writer_lock = Some(lock);
                    self.attach_daemon(None);
                    restored = true;
                }
                Err(error) => log::warn!("the archive could not be reopened for writing after a failed daemon start: {error}"),
            }
        }
        if !restored {
            if let Some(handle) = self.store.clone() {
                match handle.as_read_only().open_engine() {
                    Ok(engine) => {
                        self.store = Some(handle.as_read_only());
                        self.engine = engine;
                    }
                    Err(error) => log::warn!("the archive could not be reopened read-only after a failed daemon start: {error}"),
                }
            }
            self.writer_lock = None;
            self.connection = daemon::Connection::Lost;
            if self.daemon_link.is_none() {
                self.daemon_link = daemon::Link::connect(&path).ok();
            }
            if let Ok(paths) = daemon::paths(&path) {
                self.stop_sync_worker();
                self.send_worker = None;
                self.daemon_events = Some(daemon::EventWorker::start(Box::new(daemon::IpcTransport::new(paths))));
                self.sync_worker = Some(SyncWorker::start(Box::new(crate::sync_worker::DaemonSyncBackend::new(path))));
            }
        }
        self.report_failure(message, false);
    }

    /// Stop the daemon from an explicit command, then take the store back in this window if the
    /// lock is free. A daemon that does not answer leaves the window read-only and says so.
    fn request_stop_daemon(&mut self) {
        if let Some(reason) = self.connection.reason_stop() {
            self.status = reason.to_owned();
            return;
        }
        if self.daemon_job.is_some() {
            self.status = "Un comando sul daemon è già in corso.".to_owned();
            return;
        }
        let Some(path) = self.store_path.clone() else {
            return;
        };
        let Ok(mut link) = daemon::Link::connect(&path) else {
            self.status = "Il canale locale non è disponibile: riavvia il daemon dalla CLI.".to_owned();
            return;
        };
        self.status = "Chiusura del daemon…".to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        let spawn_path = path.clone();
        let spawned = std::thread::Builder::new().name("postino-app-daemon-stop".to_owned()).spawn(move || {
            let result = daemon::stop(&mut link, &spawn_path);
            let _ = sender.send(result);
        });
        if spawned.is_err() {
            self.status = "Non riesco a fermare il daemon.".to_owned();
            return;
        }
        self.daemon_job = Some(DaemonJobState { kind: DaemonJob::Stop, receiver });
    }

    fn poll_daemon_job(&mut self) {
        let result = {
            let Some(job) = &self.daemon_job else {
                return;
            };
            match job.receiver.try_recv() {
                Ok(result) => Some((job.kind, result)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((job.kind, Err("L'operazione sul daemon si è interrotta.".to_owned()))),
            }
        };
        let Some((kind, result)) = result else {
            return;
        };
        self.daemon_job = None;
        match (kind, result) {
            (DaemonJob::Start, Ok(())) => self.adopt_daemon_client(),
            (DaemonJob::Start, Err(message)) => self.recover_after_failed_start(format!("Il daemon non è partito: {message}")),
            (DaemonJob::Stop, Ok(())) => self.take_over_in_process(),
            (DaemonJob::Stop, Err(message)) => {
                self.report_failure(format!("Il daemon non si è fermato: {message}"), false);
                self.reload_engine_after_sync(false);
            }
        }
    }

    /// Reconnect to a daemon that has just started, keeping the selected folder, the filters, the
    /// open message, the thread view and the composer: only the engine is replaced.
    fn adopt_daemon_client(&mut self) {
        let Some(handle) = self.store.clone() else {
            self.status = "L'archivio non è più aperto in questa finestra.".to_owned();
            return;
        };
        let Some(path) = self.store_path.clone() else {
            return;
        };
        let Some(hello) = daemon::probe(&path) else {
            self.status = "Il daemon non risponde ancora: riprova tra un istante.".to_owned();
            return;
        };
        let handle = handle.as_read_only();
        match handle.open_engine() {
            Ok(engine) => {
                self.store = Some(handle);
                self.engine = engine;
                self.attach_daemon(Some(hello));
                self.keep_view_state();
                self.status = "Il daemon ora possiede l'archivio: la finestra scrive attraverso il canale locale.".to_owned();
            }
            Err(message) => self.report_failure(format!("L'archivio non si riapre in sola lettura: {message}"), false),
        }
    }

    /// The explicit stop succeeded: take the writer lock back in this window.
    fn take_over_in_process(&mut self) {
        let Some(handle) = self.store.clone() else {
            return;
        };
        let path = handle.path().to_path_buf();
        match postino_store::WriterLock::acquire_as(&path, "app", None) {
            Ok(lock) => match handle.as_writable().open_engine() {
                Ok(engine) => {
                    self.store = Some(handle.as_writable());
                    self.engine = engine;
                    self.writer_lock = Some(lock);
                    self.attach_daemon(None);
                    self.keep_view_state();
                    self.status = "Daemon fermato: l'archivio è di questa finestra.".to_owned();
                }
                Err(message) => self.report_failure(format!("L'archivio non si riapre per scrivere: {message}"), false),
            },
            Err(postino_store::StoreError::StoreBusy { .. }) => {
                self.status = "Il daemon possiede ancora l'archivio: riprova tra un istante.".to_owned();
            }
            Err(error) => self.report_failure(format!("Non riesco a riprendere l'archivio: {error}"), false),
        }
    }

    fn daemon_status_text(&mut self) -> String {
        match self.daemon_link.as_mut().map(daemon::Link::status) {
            Some(Ok(value)) => format!(
                "daemon {} · pid {} · {} account · index {} · ultimo errore: {}",
                value.get("version").and_then(Value::as_str).unwrap_or("?"),
                value.get("pid").and_then(Value::as_u64).unwrap_or_default(),
                value.get("accounts").and_then(Value::as_u64).unwrap_or_default(),
                value.pointer("/index/state").and_then(Value::as_str).unwrap_or("?"),
                value.get("last_error").and_then(Value::as_str).unwrap_or("nessuno"),
            ),
            Some(Err(message)) => message,
            None => self.connection.detail(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn with_engine(
        engine: Engine,
        store: Option<StoreHandle>,
        demo: bool,
        boot_failure: Option<BootFailure>,
        launch: Launch,
        custom_chrome: bool,
        now: DateTime<Utc>,
        extra_roots: Vec<CertificateDer<'static>>,
    ) -> Self {
        let labels_cache = engine.label_summaries();
        let active_account = engine.accounts().first().map(|account| account.id.clone());
        let folder = default_folder_for(&engine, active_account.as_deref());
        let sync_view = if demo { SyncView::Idle { last_sync: None } } else { sync_view_for(&engine, active_account.as_deref()) };
        let last_sync = last_sync_from(&sync_view);
        let onboarding = (!demo && boot_failure.is_none() && engine.accounts().is_empty()).then(Onboarding::first_run);
        let store_path = store.as_ref().map(|handle| handle.path().to_path_buf());
        let sync_worker =
            store.as_ref().map(|handle| SyncWorker::start(Box::new(crate::sync_worker::StoreSyncBackend::new(handle.clone(), extra_roots.clone()))));
        let send_worker = store.as_ref().map(|handle| SendWorker::start(Box::new(StoreSendBackend::new(handle.clone(), extra_roots.clone()))));
        let saved_searches = active_account.as_deref().and_then(|account| engine.saved_searches_list(Some(account)).ok()).unwrap_or_default();
        let recent_searches = active_account.as_deref().and_then(|account| engine.recent_searches(Some(account)).ok()).unwrap_or_default();
        let saved_revision = engine.revision();
        // The bridge owns the store path for its daemon link; the shell keeps the same path.
        let background = Box::new(background::LiveBridge::new(store_path.clone()));
        Self {
            engine,
            folder,
            active_account,
            store,
            store_path,
            demo_return: None,
            writer_lock: None,
            demo,
            launch,
            boot_failure,
            onboarding,
            sync_worker,
            sync_view,
            send_worker,
            outbox: empty_outbox(),
            outbox_open: false,
            outbox_confirm: None,
            outbox_loaded: false,
            last_outbox_poll: 0.0,
            pending_send: None,
            sync_account: None,
            add_account_job: None,
            last_sync,
            show_accounts_menu: false,
            show_accounts_dialog: false,
            rules_page: None,
            account_remove: None,
            extra_roots,
            selected: None,
            selection: BTreeSet::new(),
            anchor: None,
            label_filter: None,
            label_folder: None,
            labels_cache,
            label_editor: None,
            label_delete: None,
            label_menu: None,
            pending_read: None,
            toast: None,
            query: String::new(),
            search_committed: String::new(),
            search_debounce_until: None,
            search_error: None,
            list_cache: None,
            threads_view: true,
            threads_view_before_search: None,
            threads_cache: None,
            thread_list_limit: 200,
            selected_thread: None,
            thread_messages: Vec::new(),
            thread_summary: None,
            thread_total: 0,
            thread_offset: 0,
            thread_open: None,
            saved_searches,
            recent_searches,
            saved_searches_revision: Some(saved_revision),
            last_saved_refresh: 0.0,
            applied_saved: None,
            saved_editor: None,
            saved_restore: None,
            saved_folder_scope: false,
            saved_query_folder: false,
            saved_error: None,
            saved_index_pending: false,
            recording_recent: false,
            scroll_saved_into_view: false,
            sidebar_scroll_target: None,
            search_focus: false,
            index_progress: None,
            index_checked: false,
            search_capped: false,
            search_limit: 200,
            unread_only: false,
            attachments_only: false,
            dark: false,
            status: String::new(),
            failure: None,
            list_error_ack: false,
            list_signature: String::new(),
            loading: None,
            show_about: false,
            show_palette: false,
            palette_query: String::new(),
            palette_focus: false,
            palette_selected: 0,
            focus_search: false,
            show_shortcuts: false,
            shortcut_query: String::new(),
            shortcut_focus: false,
            scroll_to_selected: false,
            now: 0.0,
            now_utc: now,
            custom_chrome,
            reading_open: false,
            reading_full: false,
            reading_ratio: READING_PANE_DEFAULT_RATIO,
            reduced_motion: false,
            settings_path: None,
            settings_dirty: false,
            settings_saved_at: 0.0,
            composer: ComposerState::closed(),
            composer_preview: false,
            composer_return_focus: None,
            composer_focus_pending: false,
            row_ids: BTreeMap::new(),
            focus_selected_row: false,
            screenshot: None,
            link_opener: Arc::new(UnavailableLinkOpener),
            file_saver: Arc::new(CancellingFileSaver),
            file_picker: Arc::new(UnavailableFilePicker),
            folder_opener: Arc::new(UnavailableFolderOpener),
            reading: reading_pane::ReadingUi::default(),
            screenshot_message: None,
            screenshot_invite: None,
            invite_ready: false,
            pgp_checked: None,
            pgp_prompt: None,
            pgp_keys: Vec::new(),
            pgp_keys_loaded: false,
            pgp_keys_retry_at: 0.0,
            pgp_import: None,
            pgp_delete: None,
            pgp_notice: None,
            templates: Vec::new(),
            templates_loaded: false,
            template_editor: None,
            template_delete: None,
            identities: Vec::new(),
            identities_loaded: false,
            identities_retry_at: 0.0,
            identity_editor: None,
            identity_delete: None,
            identities_notice: None,
            account_switcher_id: None,
            settings_return_focus: None,
            settings_focus_pending: false,
            signature_editor: None,
            signature_focus_account: None,
            focus_account_switcher: false,
            show_licences: false,
            licences_return_focus: None,
            licences_focus_pending: false,
            system_theme: false,
            connection: if demo { daemon::Connection::Demo } else { daemon::Connection::InProcess },
            daemon_link: None,
            daemon_events: None,
            daemon_job: None,
            background,
            background_rows: None,
            background_change: None,
            startup_open: StartupView::Mail,
            close_prompt: false,
            allow_close: false,
            tray_notice_shown: false,
            hidden: false,
            pending_activation: None,
        }
    }

    /// Build the shell in the exact state a docs screenshot shows. Used only by `--screenshot`;
    /// normal startup keeps its own constructor so nothing here runs in a user session.
    pub fn screenshot(
        cc: &eframe::CreationContext<'_>,
        state: ScreenshotState,
        dark: bool,
        custom_chrome: bool,
        out: PathBuf,
        now: DateTime<Utc>,
    ) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, dark);
        // A docs image must not catch the composer or palette mid fade-in: with no animation the
        // first laid-out frame is already the final one. A blinking caret would make the frame
        // depend on how long the GPU took to start, so screenshot mode keeps it visible.
        cc.egui_ctx.all_styles_mut(|style| {
            style.animation_time = 0.0;
            style.visuals.text_cursor.blink = false;
        });
        let mut app = Self::demo_at(custom_chrome, now);
        app.dark = dark;
        app.seed_screenshot(state);
        app.screenshot = Some(ScreenshotMode { out, frames: 0, requested: false, waited: 0 });
        app
    }

    #[cfg(test)]
    fn demo(custom_chrome: bool) -> Self {
        Self::demo_at(custom_chrome, Utc::now())
    }

    fn demo_at(custom_chrome: bool, now: DateTime<Utc>) -> Self {
        let engine = Engine::demo_at(now);
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut app = Self::with_engine(engine, None, true, None, Launch::Demo, custom_chrome, now, Vec::new());
        // Existing unit tests exercise the flat message table; the product default is threaded.
        // A screenshot or a P9b test asks for `threads_view` explicitly.
        #[cfg(test)]
        {
            app.threads_view = false;
        }
        app
    }

    /// The launch path for `--open`: the parser produces a [`StartupView`], the window applies it
    /// after the boot. Reused by the tray activation, which must focus the journal it opens.
    fn apply_launch_view(&mut self, startup: StartupView) {
        self.startup_open = startup;
        self.after_launch();
    }

    /// Honour `--open` and a tray activation: one path, applied after the boot, so the journal is
    /// refreshed against a live engine whether the daemon answered before or after the first frame.
    fn after_launch(&mut self) {
        self.apply_startup_view();
    }

    /// Honour `--open`: the tray's Coda di invio item launches the app straight onto the journal.
    fn apply_startup_view(&mut self) {
        if self.startup_open == StartupView::Outbox {
            self.open_outbox();
        }
    }

    /// Adopt a loaded settings file. The account and the folder are only accepted when the engine
    /// knows them; a settings file from another mailbox falls back to the first account and its
    /// inbox instead of opening a blank view.
    fn apply_settings(&mut self, settings: Settings) {
        self.dark = settings.dark;
        self.system_theme = settings.system_theme;
        if let Some(account) = settings.account
            && self.engine.accounts().iter().any(|candidate| candidate.id == account)
        {
            self.active_account = Some(account);
        }
        if self.active_account.is_none() {
            self.active_account = self.engine.accounts().first().map(|account| account.id.clone());
        }
        let folder_ok = self
            .engine
            .folders()
            .iter()
            .find(|folder| folder.id == settings.folder)
            .is_some_and(|folder| self.active_account.as_deref() == Some(folder.account_id.as_str()));
        self.folder = if folder_ok { settings.folder } else { default_folder_for(&self.engine, self.active_account.as_deref()) };
        self.reading_ratio = settings.reading_pane_ratio;
        self.reduced_motion = settings.reduced_motion;
        self.tray_notice_shown = settings.tray_notice_shown;
        if !self.demo {
            self.sync_view = sync_view_for(&self.engine, self.active_account.as_deref());
            self.last_sync = last_sync_from(&self.sync_view);
        }
    }

    fn settings_snapshot(&self) -> Settings {
        Settings {
            version: crate::settings::SETTINGS_VERSION,
            dark: self.dark,
            system_theme: self.system_theme,
            reading_pane_ratio: self.reading_ratio,
            folder: self.folder.clone(),
            reduced_motion: self.reduced_motion,
            account: self.active_account.clone(),
            tray_notice_shown: self.tray_notice_shown,
        }
    }

    fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.settings_saved_at = self.now;
    }

    fn persist_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        if let Err(error) = self.settings_snapshot().save_to(&path) {
            // The path is not mail data, but keep the message generic: a settings failure must not
            // become a product error dialog on the next launch.
            log::warn!("settings could not be saved: {error}");
        }
        self.settings_dirty = false;
        self.settings_saved_at = self.now;
    }

    /// Read the four background rows. This never starts the daemon: a missing daemon is a row-level
    /// reason, not a reason to launch a process behind the user's back.
    fn refresh_background(&mut self) {
        // Demo mode has no store and no daemon path, so the autostart row is disabled with its own
        // reason instead of the wrong "daemon binary next to the app" sentence.
        let autostart = if self.demo { Err(background::DEMO_AUTOSTART.to_owned()) } else { self.background.autostart() };
        // Without a live daemon the three daemon rows are unavailable, but autostart is a platform
        // value and is still read: it does not need the daemon to exist.
        let daemon = if self.connection.remote() { self.background.daemon() } else { Err("Il daemon non è in esecuzione.".to_owned()) };
        self.background_rows = Some(background::Rows::read(daemon, autostart));
    }

    /// Apply the toggle the user chose, then read back. The write is explicit; a failure leaves the
    /// previous value visible and prints the daemon's sentence above the rows.
    fn apply_background_change(&mut self) {
        let Some(change) = self.background_change.take() else {
            return;
        };
        let result = match change {
            BackgroundChange::Tray(on) => self.background.patch(background::Patch { tray: Some(on), ..Default::default() }).map(|_| ()),
            BackgroundChange::Notify(on) => self.background.patch(background::Patch { notify_enabled: Some(on), ..Default::default() }).map(|_| ()),
            BackgroundChange::Previews(on) => {
                self.background.patch(background::Patch { notify_previews: Some(on), ..Default::default() }).map(|_| ())
            }
            BackgroundChange::Autostart(on) => self.background.autostart_set(on).map(|_| ()),
        };
        match result {
            Ok(_) => {
                self.status = "Impostazione salvata.".to_owned();
                self.refresh_background();
            }
            Err(error) => {
                if let Some(rows) = self.background_rows.as_mut() {
                    rows.error = Some(error.clone());
                } else {
                    self.background_rows = Some(background::Rows::unavailable(Some(error.clone())));
                }
                self.status = error;
            }
        }
    }

    /// Intercept eframe's close request. Without a daemon-owned store the window exits as before.
    /// With one, hiding is allowed only when the preference is on and the icon is really running.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if self.allow_close || self.close_prompt || self.screenshot.is_some() {
            return;
        }
        if !ctx.input(|input| input.viewport().close_requested()) || !self.connection.remote() {
            return;
        }
        self.refresh_background();
        let rows = self.background_rows.as_ref();
        let tray_enabled = rows.is_some_and(|rows| rows.tray.available && rows.tray.on);
        let tray_running = rows.is_some_and(|rows| rows.tray_running);
        match background::close_action(tray_enabled, tray_running, self.connection.remote(), self.tray_notice_shown) {
            background::CloseAction::Exit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            background::CloseAction::Hide => {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.hide_to_tray(ctx);
            }
            background::CloseAction::Ask => {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.close_prompt = true;
            }
        }
    }

    /// Hide the window and remember that the explanation was accepted. The app keeps running
    /// because the daemon keeps the store, the tray offers the way back and `Esci` is explicit.
    fn hide_to_tray(&mut self, ctx: &egui::Context) {
        self.close_prompt = false;
        self.hidden = true;
        if !self.tray_notice_shown {
            self.tray_notice_shown = true;
            self.mark_settings_dirty();
            self.persist_settings();
        }
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    /// The first close-to-tray explanation. It is shown once per profile, never on every close.
    fn close_notice(&mut self, ui: &mut egui::Ui) {
        if !self.close_prompt {
            return;
        }
        let response = crate::ui::widgets::Dialog::new(
            "Postino resta nella barra delle applicazioni",
            "Il daemon continua a sincronizzare e la finestra si nasconde. Per chiudere davvero usa Esci dal menu Postino o dalla barra delle applicazioni.",
            "Nascondi nella barra",
        )
        .secondary("Esci da Postino")
        .show(ui);
        if response.primary {
            let ctx = ui.ctx().clone();
            self.hide_to_tray(&ctx);
        } else if response.secondary {
            self.close_prompt = false;
            self.allow_close = true;
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        theme::apply(ctx, self.dark);
        theme::apply_motion(ctx, self.reduced_motion);
    }

    fn set_dark(&mut self, ctx: &egui::Context, dark: bool) {
        self.dark = dark;
        self.system_theme = false;
        self.apply_theme(ctx);
        self.mark_settings_dirty();
        self.persist_settings();
    }

    fn set_system_theme(&mut self, ctx: &egui::Context, system: bool) {
        self.system_theme = system;
        if system && let Some(theme) = ctx.system_theme() {
            self.dark = theme == egui::Theme::Dark;
        }
        self.apply_theme(ctx);
        self.mark_settings_dirty();
        self.persist_settings();
    }

    /// Keep the explicit styles aligned with the operating system while the preference is System.
    fn follow_system_theme(&mut self, ctx: &egui::Context) {
        if !self.system_theme {
            return;
        }
        if let Some(theme) = ctx.system_theme() {
            let dark = theme == egui::Theme::Dark;
            if dark != self.dark {
                self.dark = dark;
                self.apply_theme(ctx);
            }
        }
    }

    fn set_reduced_motion(&mut self, ctx: &egui::Context, reduced: bool) {
        self.reduced_motion = reduced;
        self.apply_theme(ctx);
        self.mark_settings_dirty();
        self.persist_settings();
    }

    fn active_account(&self) -> Option<&Account> {
        let id = self.active_account.as_deref()?;
        self.engine.accounts().iter().find(|account| account.id == id)
    }

    fn folder(&self, id: &str) -> Option<&postino_core::Folder> {
        self.engine.folders().iter().find(|folder| folder.id == id)
    }

    fn folder_kind(&self, id: &str) -> Option<FolderKind> {
        self.folder(id).map(|folder| folder.kind)
    }

    /// The folder of a role in the active account. Aggregate/starred rows are only returned when
    /// that exact role is asked for.
    fn folder_for_kind(&self, kind: FolderKind) -> Option<String> {
        let active = self.active_account.as_deref()?;
        self.folder_for_kind_in(active, kind)
    }

    fn folder_for_kind_in(&self, account: &str, kind: FolderKind) -> Option<String> {
        self.engine.folders().iter().find(|folder| folder.account_id == account && folder.kind == kind).map(|folder| folder.id.clone())
    }

    fn active_folder_ids(&self) -> BTreeSet<String> {
        let Some(active) = self.active_account.as_deref() else {
            return BTreeSet::new();
        };
        self.engine.folders().iter().filter(|folder| folder.account_id == active).map(|folder| folder.id.clone()).collect()
    }

    fn can_add_account(&self) -> bool {
        !self.demo && (self.store.is_some() || self.screenshot.is_some())
    }

    /// A demo opened from first-run onboarding or from the boot-recovery card is a detour, not a
    /// final choice: the original store is still reachable. A real `--demo` launch has no archive
    /// to return to, so it does not advertise one.
    fn demo_can_return(&self) -> bool {
        self.demo && self.launch != Launch::Demo
    }

    fn sync_ready(&self) -> bool {
        !self.demo && (self.store.is_some() || self.screenshot.is_some()) && self.active_account.is_some()
    }

    fn sync_running(&self) -> bool {
        matches!(self.sync_view, SyncView::Running(_)) || self.sync_worker.as_ref().is_some_and(SyncWorker::is_running)
    }

    fn request_sync(&mut self) {
        if self.demo || self.store.is_none() {
            self.status = "La modalità demo non ha un server da aggiornare".to_owned();
            return;
        }
        let Some(account) = self.active_account.clone() else {
            self.status = "Nessun account da sincronizzare".to_owned();
            return;
        };
        let Some(worker) = self.sync_worker.as_mut() else {
            self.status = "Il servizio di sincronizzazione non è disponibile".to_owned();
            return;
        };
        match worker.request(Some(account.clone())) {
            SyncRequest::Started => {
                self.sync_account = Some(account);
                self.sync_view = SyncView::Running(SyncProgress::default());
                self.status = "Sincronizzazione avviata".to_owned();
            }
            SyncRequest::AlreadyRunning => self.status = "Sincronizzazione già in corso".to_owned(),
        }
    }

    fn cancel_sync(&mut self) {
        if let Some(worker) = &self.sync_worker {
            worker.cancel();
            self.status = "Annullamento della sincronizzazione…".to_owned();
        }
    }

    /// Drain worker events once per frame. No event ever clears the list: a reload only replaces
    /// the engine while the selection, filters and open message stay as they are.
    fn handle_sync_events(&mut self) {
        let events = {
            let Some(worker) = &mut self.sync_worker else {
                return;
            };
            worker.poll()
        };
        for event in events {
            self.apply_sync_event(event);
        }
    }

    /// One sync outcome, whether it came from this window's worker (in-process mode) or from a
    /// `sync` event the daemon published (client mode). Both replace the engine and keep the view.
    fn apply_sync_event(&mut self, event: SyncEvent) {
        let event_account = match &event {
            SyncEvent::Started { account } => account.clone(),
            SyncEvent::Progress { account, .. } => account.clone(),
            SyncEvent::Finished { report } => Some(report.account.clone()),
            SyncEvent::Failed { account, .. } => account.clone(),
        };
        let active = self.active_account.clone();
        let applies = event_account.is_none() || event_account.as_deref() == active.as_deref();
        match event {
            SyncEvent::Started { .. } => {
                if applies && !matches!(self.sync_view, SyncView::Running(_)) {
                    if self.sync_account.is_none() {
                        self.sync_account = event_account;
                    }
                    self.sync_view = SyncView::Running(SyncProgress::default());
                }
            }
            SyncEvent::Progress { progress, .. } => {
                if applies {
                    self.sync_view = SyncView::Running(progress);
                    if let Some(onboarding) = self.onboarding.as_mut() {
                        onboarding.set_sync_progress(progress);
                    }
                }
            }
            SyncEvent::Finished { report } => {
                self.reload_engine_after_sync(true);
                self.clear_sync_account(event_account.as_deref());
                if !applies {
                    return;
                }
                let last = self.last_sync.or_else(|| Some(self.now_utc.timestamp_millis()));
                self.last_sync = last;
                self.sync_view = SyncView::Idle { last_sync: last };
                self.status = format!(
                    "Sincronizzazione completata · {}",
                    plural(usize::try_from(report.fetched).unwrap_or(usize::MAX), "messaggio aggiornato", "messaggi aggiornati")
                );
                if let Some(onboarding) = self.onboarding.as_mut() {
                    onboarding.sync_finished();
                }
                self.finish_onboarding_if_synced();
            }
            SyncEvent::Failed { message, cancelled, offline, .. } => {
                self.reload_engine_after_sync(false);
                self.clear_sync_account(event_account.as_deref());
                if !applies {
                    return;
                }
                if cancelled {
                    self.sync_view = SyncView::Idle { last_sync: self.last_sync };
                    self.status = "Sincronizzazione annullata".to_owned();
                } else {
                    self.sync_view = if offline {
                        SyncView::Offline { last_sync: self.last_sync }
                    } else {
                        SyncView::Error { reason: message.clone(), last_sync: self.last_sync }
                    };
                    self.status = message.clone();
                }
                if let Some(onboarding) = self.onboarding.as_mut() {
                    onboarding.sync_failed(message, cancelled);
                }
            }
        }
    }

    fn clear_sync_account(&mut self, event_account: Option<&str>) {
        if event_account.is_none() || self.sync_account.as_deref() == event_account {
            self.sync_account = None;
        }
    }

    fn poll_add_account(&mut self) {
        let result = {
            let Some(receiver) = &self.add_account_job else {
                return;
            };
            match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("La creazione dell'account si è interrotta. Riprova.".to_owned())),
            }
        };
        let Some(result) = result else {
            return;
        };
        self.add_account_job = None;
        match result {
            Ok((id, email)) => {
                self.reload_engine_after_sync(false);
                if !self.engine.accounts().iter().any(|account| account.id == id) {
                    if let Some(onboarding) = self.onboarding.as_mut() {
                        onboarding.account_failed("L'account è stato creato ma l'archivio non lo rilegge. Riapri l'app o riprova.".to_owned());
                    }
                    return;
                }
                if let Some(onboarding) = self.onboarding.as_mut() {
                    onboarding.account_created(&id, &email);
                }
                self.active_account = Some(id.clone());
                self.folder = default_folder_for(&self.engine, Some(&id));
                self.labels_cache = self.engine.label_summaries();
                self.ensure_sync_worker();
                self.start_first_sync(&id);
            }
            Err(message) => {
                if let Some(onboarding) = self.onboarding.as_mut() {
                    onboarding.account_failed(message);
                }
            }
        }
    }

    fn finish_onboarding_if_synced(&mut self) {
        let Some(onboarding) = self.onboarding.as_ref() else {
            return;
        };
        if !onboarding.sync_is_finished() {
            return;
        }
        let account = onboarding.account_id().map(str::to_owned);
        self.onboarding = None;
        if let Some(account) = account {
            self.enter_account(account);
        }
        self.status = "Account pronto".to_owned();
    }

    /// Reload the whole engine from the store after a sync. The old engine stays on screen if the
    /// reopen fails, so a failed refresh is a message, never an empty mailbox.
    fn reload_engine_after_sync(&mut self, report_failure: bool) {
        let Some(store) = &self.store else {
            return;
        };
        match store.open_engine() {
            Ok(engine) => {
                self.engine = engine;
                self.labels_cache = self.engine.label_summaries();
                self.threads_cache = None;
                self.list_cache = None;
                self.refresh_saved_searches();
                self.keep_view_state();
            }
            Err(error) => {
                if report_failure {
                    self.report_failure(format!("La sincronizzazione è riuscita ma l'archivio non si riapre: {error}"), false);
                } else {
                    log::warn!("the store could not be reopened after a failed sync: {error}");
                }
            }
        }
    }

    /// Keep the selected id, the open message and every filter; repair only what no longer exists.
    fn keep_view_state(&mut self) {
        if let Some(account) = self.active_account.clone()
            && !self.engine.accounts().iter().any(|candidate| candidate.id == account)
        {
            self.active_account = self.engine.accounts().first().map(|candidate| candidate.id.clone());
        }
        let folder_exists = self
            .engine
            .folders()
            .iter()
            .find(|folder| folder.id == self.folder)
            .is_some_and(|folder| self.active_account.as_deref() == Some(folder.account_id.as_str()));
        if !folder_exists {
            self.folder = default_folder_for(&self.engine, self.active_account.as_deref());
        }
        self.selection.retain(|id| self.engine.message(id).is_some());
        if self.selected.as_deref().is_some_and(|id| self.engine.message(id).is_none()) {
            self.selected = None;
            self.reading_open = false;
            self.reading_full = false;
            self.pending_read = None;
        }
        if let Some(id) = self.selected.clone()
            && self.selection.is_empty()
        {
            self.selection.insert(id);
        }
        self.row_ids.clear();
    }

    fn switch_account(&mut self, id: String) {
        if !self.engine.accounts().iter().any(|account| account.id == id) {
            self.status = "Account non trovato".to_owned();
            return;
        }
        self.active_account = Some(id);
        self.folder = default_folder_for(&self.engine, self.active_account.as_deref());
        self.reset_pgp_account_state();
        self.clear_search();
        self.unread_only = false;
        self.attachments_only = false;
        self.label_filter = None;
        self.label_folder = None;
        self.clear_selection();
        self.close_reading();
        self.clear_thread_selection();
        self.threads_cache = None;
        self.labels_cache = self.engine.label_summaries();
        self.refresh_saved_searches();
        if self.sync_worker.as_ref().is_some_and(SyncWorker::is_running) && self.sync_account.as_deref() == self.active_account.as_deref() {
            self.sync_view = SyncView::Running(SyncProgress::default());
        } else {
            self.sync_view = sync_view_for(&self.engine, self.active_account.as_deref());
        }
        self.last_sync = last_sync_from(&self.sync_view);
        if !self.sync_worker.as_ref().is_some_and(SyncWorker::is_running) {
            self.sync_account = None;
        }
        self.mark_settings_dirty();
        self.persist_settings();
    }

    /// Enter an account after onboarding: switch to it, choose its inbox, keep the worker running.
    fn enter_account(&mut self, id: String) {
        self.active_account = Some(id.clone());
        self.folder = default_folder_for(&self.engine, self.active_account.as_deref());
        self.reset_pgp_account_state();
        self.clear_search();
        self.unread_only = false;
        self.attachments_only = false;
        self.label_filter = None;
        self.label_folder = None;
        self.clear_selection();
        self.close_reading();
        self.clear_thread_selection();
        self.threads_cache = None;
        self.labels_cache = self.engine.label_summaries();
        let view = sync_view_for(&self.engine, Some(&id));
        self.last_sync = last_sync_from(&view);
        if self.sync_worker.as_ref().is_some_and(SyncWorker::is_running) && self.sync_account.as_deref() == Some(id.as_str()) {
            self.sync_view = SyncView::Running(SyncProgress::default());
        } else {
            self.sync_view = view;
        }
        self.mark_settings_dirty();
        self.persist_settings();
    }

    fn remove_account(&mut self, state: &AccountRemoveState) {
        if self.active_account.as_deref() == Some(state.id.as_str()) {
            self.cancel_sync();
        }
        match self.op("account_remove", json!({ "id": state.id, "confirm": true })) {
            Ok(_) => self.after_account_removal(&state.id, None),
            Err(error) => {
                // The engine removes the store rows before the credential. A keyring refusal can
                // therefore leave a readable warning while the account is already gone.
                if self.engine.accounts().iter().any(|account| account.id == state.id) {
                    self.report_failure(format!("Non è stato possibile rimuovere l'account: {}", op_error_copy(&error)), false);
                    return;
                }
                self.after_account_removal(
                    &state.id,
                    Some(format!("Account rimosso, ma il portachiavi non ha eliminato la password: {}", op_error_copy(&error))),
                );
            }
        }
    }

    fn after_account_removal(&mut self, removed: &str, warning: Option<String>) {
        self.account_remove = None;
        if self.sync_account.as_deref() == Some(removed) {
            self.sync_account = None;
        }
        self.labels_cache = self.engine.label_summaries();
        self.clear_thread_selection();
        self.threads_cache = None;
        self.thread_list_limit = 200;
        self.saved_searches.clear();
        self.recent_searches.clear();
        self.applied_saved = None;
        self.saved_editor = None;
        self.saved_restore = None;
        self.saved_searches_revision = None;
        self.saved_index_pending = false;
        self.saved_error = None;
        self.saved_folder_scope = false;
        self.saved_query_folder = false;
        if self.active_account.as_deref() == Some(removed) {
            self.active_account = self.engine.accounts().first().map(|account| account.id.clone());
        }
        if self.engine.accounts().is_empty() {
            self.stop_sync_worker();
            self.active_account = None;
            self.folder.clear();
            self.clear_selection();
            self.close_reading();
            self.clear_search();
            self.label_filter = None;
            self.label_folder = None;
            self.sync_view = SyncView::Idle { last_sync: None };
            self.last_sync = None;
            self.onboarding = Some(Onboarding::first_run());
            self.show_accounts_dialog = false;
            self.status = "Account rimosso · configura un nuovo account".to_owned();
        } else {
            self.folder = default_folder_for(&self.engine, self.active_account.as_deref());
            self.clear_selection();
            self.close_reading();
            self.refresh_saved_searches();
            self.sync_view = sync_view_for(&self.engine, self.active_account.as_deref());
            self.last_sync = last_sync_from(&self.sync_view);
            self.status = warning.unwrap_or_else(|| "Account rimosso".to_owned());
        }
        self.mark_settings_dirty();
        self.persist_settings();
    }

    fn stop_sync_worker(&mut self) {
        if let Some(worker) = self.sync_worker.as_mut() {
            worker.stop();
        }
        self.sync_worker = None;
        self.sync_account = None;
    }

    fn ensure_sync_worker(&mut self) {
        if self.sync_worker.is_some() {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        let backend: Box<dyn crate::sync_worker::SyncBackend> = if self.daemon_link.is_some() {
            Box::new(crate::sync_worker::DaemonSyncBackend::new(store.path().to_path_buf()))
        } else {
            Box::new(crate::sync_worker::StoreSyncBackend::new(store.clone(), self.extra_roots.clone()))
        };
        self.sync_worker = Some(SyncWorker::start(backend));
    }

    fn open_demo(&mut self) {
        self.stop_sync_worker();
        self.send_worker = None;
        self.daemon_events = None;
        self.daemon_link = None;
        self.daemon_job = None;
        self.connection = daemon::Connection::Demo;
        if !self.demo {
            // Keep the handle the store launch already opened. Leaving the demo then reopens the
            // archive without asking the keyring again; the recovery path, which never had a
            // handle, falls back to a full boot retry.
            self.demo_return = self.store.take();
        }
        self.engine = Engine::demo_at(self.now_utc);
        self.demo = true;
        self.store = None;
        self.store_path = None;
        self.boot_failure = None;
        self.onboarding = None;
        self.show_accounts_dialog = false;
        self.show_accounts_menu = false;
        self.active_account = self.engine.accounts().first().map(|account| account.id.clone());
        self.folder = default_folder_for(&self.engine, self.active_account.as_deref());
        self.labels_cache = self.engine.label_summaries();
        self.clear_selection();
        self.close_reading();
        self.clear_thread_selection();
        self.threads_cache = None;
        self.thread_list_limit = 200;
        self.saved_searches.clear();
        self.recent_searches.clear();
        self.applied_saved = None;
        self.saved_editor = None;
        self.saved_restore = None;
        self.saved_searches_revision = None;
        self.saved_index_pending = false;
        self.saved_error = None;
        self.saved_folder_scope = false;
        self.saved_query_folder = false;
        self.clear_search();
        self.refresh_saved_searches();
        self.unread_only = false;
        self.attachments_only = false;
        self.label_filter = None;
        self.label_folder = None;
        self.sync_view = SyncView::Idle { last_sync: None };
        self.last_sync = None;
        self.status = "Modalità demo".to_owned();
        self.mark_settings_dirty();
        self.persist_settings();
    }

    fn adopt_booted(&mut self, booted: boot::Booted, launch: Launch) {
        let replacement = Self::from_booted(booted, self.custom_chrome, self.now_utc, launch, self.extra_roots.clone());
        let settings_path = self.settings_path.clone();
        let settings = self.settings_snapshot();
        *self = replacement;
        self.settings_path = settings_path;
        self.apply_settings(settings);
    }

    fn retry_boot(&mut self) {
        let launch = self.launch.clone();
        let lock = self.writer_lock.take();
        match boot::boot_with_lock(launch.clone(), boot::KeySource::Keyring, self.now_utc, lock) {
            Ok(booted) => self.adopt_booted(booted, launch),
            Err(failure) => {
                // The old engine is still open. If boot failed after releasing the lock, take it back
                // so a daemon cannot start beside the window during the error screen.
                if self.writer_lock.is_none()
                    && let Some(path) = self.store_path.as_deref()
                {
                    self.writer_lock = postino_store::WriterLock::acquire_as(path, "app", None).ok();
                }
                self.boot_failure = Some(failure);
            }
        }
    }

    /// Leave the synthetic mailbox and go back to the archive the session was launched with.
    /// The first-run welcome and the boot-recovery card both offer the demo; neither may become a
    /// one-way door. A real `--demo` launch has no archive and therefore no visible exit.
    fn leave_demo(&mut self) {
        if !self.demo_can_return() {
            return;
        }
        let launch = self.launch.clone();
        let settings_path = self.settings_path.clone();
        let settings = self.settings_snapshot();
        let lock = self.writer_lock.take();
        let reopened = match (self.demo_return.take(), lock) {
            (Some(store), Some(writer_lock)) => match store.open_engine() {
                Ok(engine) => Some(boot::Booted { engine, store: Some(store), demo: false, writer_lock: Some(writer_lock), daemon: None }),
                Err(_) => None,
            },
            (Some(store), None) if store.is_read_only() => {
                // Entered the demo while a daemon owned the store: come back as a client, never as
                // a second writer.
                let hello = daemon::probe(store.path());
                match (store.open_engine(), hello) {
                    (Ok(engine), Some(hello)) => {
                        Some(boot::Booted { engine, store: Some(store), demo: false, writer_lock: None, daemon: Some(hello) })
                    }
                    _ => None,
                }
            }
            (store, None) => {
                self.demo_return = store;
                None
            }
            (None, Some(_)) => None,
        };
        match reopened {
            Some(booted) => {
                *self = Self::from_booted(booted, self.custom_chrome, self.now_utc, launch, self.extra_roots.clone());
                self.settings_path = settings_path;
                self.apply_settings(settings);
            }
            None => self.retry_boot(),
        }
    }

    /// The one place onboarding actions become engine calls, worker commands and view changes.
    fn handle_onboarding_action(&mut self, action: onboarding::Action) {
        match action {
            onboarding::Action::None => {}
            onboarding::Action::OpenDemo => self.open_demo(),
            onboarding::Action::Cancel => {
                self.onboarding = None;
                if self.engine.accounts().is_empty() {
                    self.open_demo();
                } else {
                    self.status = "Configurazione annullata".to_owned();
                }
            }
            onboarding::Action::AddAccount(setup) => {
                let Some(handle) = self.store.clone() else {
                    if let Some(onboarding) = self.onboarding.as_mut() {
                        onboarding.account_failed("La modalità demo non ha un archivio locale.".to_owned());
                    }
                    return;
                };
                let (sender, receiver) = std::sync::mpsc::channel();
                self.add_account_job = Some(receiver);
                let daemon_route =
                    if self.daemon_link.is_some() { self.store_path.as_deref().and_then(|path| daemon::paths(path).ok()) } else { None };
                std::thread::spawn(move || {
                    let result = match daemon_route {
                        // The daemon admits this window for the sensitive operation: the password
                        // travels only inside the same-user channel, never on a command line.
                        Some(paths) => daemon::add_account(&paths, &setup),
                        None => match handle.open_engine() {
                            Ok(mut engine) => engine.add_account(*setup).map(|view| (view.account.id, view.account.email)).map_err(add_account_copy),
                            Err(error) => Err(format!("Non riesco ad aprire l'archivio locale: {error}")),
                        },
                    };
                    let _ = sender.send(result);
                });
            }
            onboarding::Action::StartSync(id) => {
                if let Some(onboarding) = self.onboarding.as_mut() {
                    onboarding.sync_started();
                }
                self.start_sync_for(Some(id));
            }
            onboarding::Action::CancelSync => self.cancel_sync(),
            onboarding::Action::Finish(id) => {
                self.onboarding = None;
                self.enter_account(id);
                self.status = "Account pronto".to_owned();
            }
        }
    }

    fn start_first_sync(&mut self, id: &str) {
        if let Some(onboarding) = self.onboarding.as_mut() {
            onboarding.sync_started();
        }
        self.start_sync_for(Some(id.to_owned()));
    }

    fn start_sync_for(&mut self, account: Option<String>) {
        let Some(worker) = self.sync_worker.as_mut() else {
            let message = "La modalità demo non ha un server da aggiornare".to_owned();
            self.status = message.clone();
            if let Some(onboarding) = self.onboarding.as_mut() {
                onboarding.sync_failed(message, false);
            }
            return;
        };
        match worker.request(account.clone()) {
            SyncRequest::Started => {
                self.sync_account = account.or_else(|| self.active_account.clone());
                self.sync_view = SyncView::Running(SyncProgress::default());
                self.status = "Sincronizzazione avviata".to_owned();
            }
            SyncRequest::AlreadyRunning => {
                let message = "C'è già una sincronizzazione in corso. Riprova tra poco.".to_owned();
                self.status = "Sincronizzazione già in corso".to_owned();
                if let Some(onboarding) = self.onboarding.as_mut() {
                    onboarding.sync_failed(message, false);
                }
            }
        }
    }

    fn sync_text(&self) -> Option<String> {
        if self.demo {
            return None;
        }
        match &self.sync_view {
            SyncView::Running(progress) => Some(if progress.folders_total > 0 {
                format!(
                    "Sincronizzazione… {}/{} cartelle · {}",
                    progress.folders_done,
                    progress.folders_total,
                    plural(usize::try_from(progress.messages).unwrap_or(usize::MAX), "messaggio", "messaggi")
                )
            } else {
                "Sincronizzazione…".to_owned()
            }),
            SyncView::Idle { last_sync: Some(ms) } => Some(format!("Aggiornato alle {}", format_millis(*ms, self.now_utc))),
            SyncView::Idle { last_sync: None } => Some("Mai sincronizzato".to_owned()),
            SyncView::Offline { last_sync: Some(ms) } => Some(format!("Offline · ultimo sync {}", format_millis(*ms, self.now_utc))),
            SyncView::Offline { last_sync: None } => Some("Offline · nessuna sincronizzazione".to_owned()),
            SyncView::Error { reason, .. } => Some(format!("Errore · {reason}")),
        }
    }

    fn sync_error_needs_retry(&self) -> bool {
        matches!(self.sync_view, SyncView::Error { .. })
    }

    fn account_unread(&self, account_id: &str) -> u32 {
        self.engine
            .folders()
            .iter()
            .filter(|folder| folder.account_id == account_id && folder.kind != FolderKind::Starred)
            .fold(0, |total, folder| total.saturating_add(folder.unread))
    }

    /// The starred view is a virtual folder: the core count is mailbox-wide, while the sidebar and
    /// list must show only the active account. Compute the same filtered totals here.
    fn starred_counts(&self, account_id: &str) -> (u32, u32) {
        let folders =
            self.engine.folders().iter().filter(|folder| folder.account_id == account_id).map(|folder| folder.id.as_str()).collect::<BTreeSet<_>>();
        self.engine
            .messages()
            .iter()
            .filter(|message| folders.contains(message.folder_id.as_str()) && message.starred)
            .fold((0, 0), |(total, unread), message| (total.saturating_add(1), unread.saturating_add(if message.unread { 1 } else { 0 })))
    }

    /// The honest tri-state shown by the status bar. `Errore` is a real failure that has not been
    /// dismissed; `Pronto` is a store-backed engine; `Demo` is synthetic mail in memory.
    fn status_kind(&self) -> StatusKind {
        let sync_error = matches!(self.sync_view, SyncView::Error { .. });
        if self.failure.is_some() || sync_error || self.boot_failure.is_some() {
            return StatusKind::Error;
        }
        if self.demo {
            return StatusKind::Demo;
        }
        if matches!(self.sync_view, SyncView::Offline { .. }) {
            return StatusKind::Offline;
        }
        StatusKind::Ready
    }

    /// One place for every operation failure: an Italian sentence, a red status dot and a single
    /// toast with a dismiss control. The raw engine error stays out of the UI.
    fn report_failure(&mut self, message: impl Into<String>, retry_list: bool) {
        self.failure = Some(Failure { message: message.into(), retry_list });
        self.toast = None;
        // The failure owns the message surface; a stale confirmation in the status bar would read
        // like a second, contradictory account of the same operation.
        self.status.clear();
    }

    fn dismiss_failure(&mut self) {
        if self.failure.as_ref().is_some_and(|failure| failure.retry_list) {
            self.list_error_ack = true;
        }
        self.failure = None;
    }

    fn retry_failure(&mut self) {
        self.failure = None;
        self.list_error_ack = false;
        let (rows, _, _) = self.visible_messages();
        if self.failure.is_none() {
            self.status = "Elenco aggiornato".to_owned();
            if rows.is_empty() {
                self.status = "Elenco aggiornato · nessun messaggio".to_owned();
            }
        }
    }

    /// Mark a long operation as running. The line only appears after [`LOADING_DELAY`], so a
    /// synchronous local call that returns within one frame never flickers.
    fn begin_loading(&mut self) {
        if self.loading.is_none() {
            self.loading = Some(self.now);
        }
    }

    fn end_loading(&mut self) {
        self.loading = None;
    }

    fn loading_visible(&self) -> bool {
        self.loading.is_some_and(|started| self.now - started >= LOADING_DELAY)
    }

    /// The filter-aware empty state. The order is search → label → unread → attachments → drafts →
    /// folder, so a filter is always named as the cause and a folder is never called empty when a
    /// filter produced the result. Each branch offers at most two ways out, and every action changes
    /// something visible.
    fn empty_view(&self, label_name: Option<&str>, label_scope: Option<&str>) -> EmptyView {
        self.empty_view_in(label_name, label_scope, false)
    }

    /// The same resolver for the threaded default: only the nouns and the folder sentence change,
    /// so unread/attachments/label/search keep their "Mostra tutti"/"Togli filtro" way out.
    fn empty_view_in(&self, label_name: Option<&str>, label_scope: Option<&str>, threads: bool) -> EmptyView {
        let query = self.query.trim();
        let scope = label_scope.unwrap_or("in questa cartella");
        let any = if threads { "Nessuna conversazione" } else { "Nessun messaggio" };
        let unread_title = if threads { "Nessuna conversazione non letta" } else { "Nessun messaggio non letto" };
        let attachments_title = if threads { "Nessuna conversazione con allegati" } else { "Nessun messaggio con allegati" };
        if !query.is_empty() {
            let search_scope = match (label_name, label_scope) {
                (Some(name), Some(folder)) if folder != "in tutte le cartelle" => format!("nell'etichetta “{name}” {folder}"),
                (Some(name), _) => format!("nell'etichetta “{name}”"),
                _ => "in tutte le cartelle".to_owned(),
            };
            let primary = ("Cancella ricerca", EmptyAction::ClearSearch);
            let secondary = label_name.map(|_| ("Togli filtro etichetta", EmptyAction::ClearLabel));
            let body = if threads {
                format!("Nessuna conversazione corrisponde a “{query}” {search_scope}.")
            } else {
                format!("Nessun messaggio corrisponde a “{query}” {search_scope}.")
            };
            return EmptyView {
                icon: icons::SEARCH,
                title: if threads { "Nessuna conversazione" } else { "Nessun risultato" }.to_owned(),
                body,
                primary: Some(primary),
                secondary,
            };
        }
        if let Some(name) = label_name {
            if self.unread_only {
                return EmptyView {
                    icon: icons::MARK_UNREAD,
                    title: format!("{unread_title} con l'etichetta “{name}”"),
                    body: format!(
                        "{} con l'etichetta “{name}” {scope}.",
                        if threads { "Nessuna conversazione non letta" } else { "Nessun messaggio non letto" }
                    ),
                    primary: Some(("Togli filtro “non letti”", EmptyAction::ClearUnread)),
                    secondary: None,
                };
            }
            if self.attachments_only {
                return EmptyView {
                    icon: icons::PAPERCLIP,
                    title: attachments_title.to_owned(),
                    body: format!(
                        "{} con l'etichetta “{name}” {scope}.",
                        if threads { "Nessuna conversazione con allegati" } else { "Nessun messaggio con allegati" }
                    ),
                    primary: Some(("Togli filtro “allegati”", EmptyAction::ClearAttachments)),
                    secondary: Some(("Togli filtro etichetta", EmptyAction::ClearLabel)),
                };
            }
            return EmptyView {
                icon: icons::TAG,
                title: any.to_owned(),
                body: format!("{any} con l'etichetta “{name}” {scope}."),
                primary: Some(("Togli filtro etichetta", EmptyAction::ClearLabel)),
                secondary: Some(("Mostra tutti", EmptyAction::ShowAll)),
            };
        }
        if self.unread_only {
            return EmptyView {
                icon: icons::MARK_UNREAD,
                title: unread_title.to_owned(),
                body: if threads {
                    "In questa cartella non ci sono conversazioni non lette.".to_owned()
                } else {
                    "In questa cartella non ci sono messaggi non letti.".to_owned()
                },
                primary: Some(("Mostra tutti", EmptyAction::ShowAll)),
                secondary: None,
            };
        }
        if self.attachments_only {
            return EmptyView {
                icon: icons::PAPERCLIP,
                title: attachments_title.to_owned(),
                body: if threads {
                    "In questa cartella non ci sono conversazioni con allegati.".to_owned()
                } else {
                    "In questa cartella non ci sono messaggi con allegati.".to_owned()
                },
                primary: Some(("Mostra tutti", EmptyAction::ShowAll)),
                secondary: None,
            };
        }
        if self.folder_kind(&self.folder) == Some(FolderKind::Drafts) {
            return EmptyView {
                icon: icons::DRAFTS,
                title: "Nessuna bozza".to_owned(),
                body: "Le bozze si salvano da sole mentre scrivi.".to_owned(),
                primary: Some(("Scrivi", EmptyAction::Compose)),
                secondary: None,
            };
        }
        if self.folder_kind(&self.folder) == Some(FolderKind::Inbox) {
            return EmptyView {
                icon: icons::INBOX,
                title: any.to_owned(),
                body: if threads { "La posta in arrivo non ha conversazioni." } else { "La posta in arrivo è vuota." }.to_owned(),
                primary: None,
                secondary: None,
            };
        }
        EmptyView {
            icon: icons::INBOX,
            title: any.to_owned(),
            body: if threads { "Questa cartella non ha conversazioni." } else { "Questa cartella è vuota." }.to_owned(),
            primary: self.folder_for_kind(FolderKind::Inbox).is_some().then_some(("Vai a Posta in arrivo", EmptyAction::Inbox)),
            secondary: None,
        }
    }

    fn apply_empty_action(&mut self, action: EmptyAction) {
        match action {
            EmptyAction::ShowAll => {
                self.clear_search();
                self.unread_only = false;
                self.attachments_only = false;
                self.label_filter = None;
                self.label_folder = None;
                self.clear_selection();
                self.reading_open = false;
            }
            EmptyAction::ClearLabel => {
                // The label filter goes; an explicit folder narrowing stays, so the header visibly
                // changes instead of looking untouched.
                self.label_filter = None;
                self.label_folder = None;
            }
            EmptyAction::ClearUnread => self.unread_only = false,
            EmptyAction::ClearAttachments => self.attachments_only = false,
            EmptyAction::ClearSearch => self.clear_search(),
            EmptyAction::Compose => self.compose_new(),
            EmptyAction::Inbox => {
                if let Some(folder) = self.folder_for_kind(FolderKind::Inbox) {
                    self.select_folder(folder);
                }
            }
        }
        self.list_error_ack = false;
    }

    fn select_folder(&mut self, folder: impl Into<String>) {
        let folder = folder.into();
        if !self
            .engine
            .folders()
            .iter()
            .find(|candidate| candidate.id == folder)
            .is_some_and(|candidate| self.active_account.as_deref() == Some(candidate.account_id.as_str()))
        {
            self.status = "Cartella non disponibile per questo account".to_owned();
            return;
        }
        self.folder = folder;
        // A folder click narrows an active label view to the intersection instead of dropping the
        // filter; "Mostra tutte" is the explicit way back to the whole mail list.
        if self.label_filter.is_some() {
            self.label_folder = Some(self.folder.clone());
        }
        self.label_editor = None;
        self.label_menu = None;
        self.clear_search();
        self.clear_selection();
        self.reading_open = false;
        self.reading_full = false;
        self.clear_thread_selection();
        self.threads_cache = None;
        self.thread_list_limit = 200;
        self.list_error_ack = false;
        self.mark_settings_dirty();
        self.persist_settings();
    }

    fn refresh_labels(&mut self) {
        self.begin_loading();
        self.labels_cache = self.engine.label_summaries();
        self.end_loading();
    }

    fn label_summary(&self, id: &str) -> Option<&LabelSummary> {
        self.labels_cache.iter().find(|label| label.id == id)
    }

    // -----------------------------------------------------------------------------------------
    // Conversations and saved searches (P9b)
    // -----------------------------------------------------------------------------------------

    /// The conversation the keyboard actions operate on, when the threaded view is active.
    fn active_thread(&self) -> Option<&str> {
        if self.threads_view { self.selected_thread.as_deref() } else { None }
    }

    fn refresh_saved_searches(&mut self) {
        let account = self.active_account.clone();
        let _ = self.engine.reload_saved_searches(account.as_deref());
        self.saved_searches = account.as_deref().and_then(|account| self.engine.saved_searches_list(Some(account)).ok()).unwrap_or_default();
        self.recent_searches = account.as_deref().and_then(|account| self.engine.recent_searches(Some(account)).ok()).unwrap_or_default();
        self.saved_searches_revision = Some(self.engine.revision());
        self.last_saved_refresh = self.now;
        if let Some(applied) = self.applied_saved.clone() {
            match self.saved_searches.iter().find(|saved| saved.id == applied) {
                Some(saved) => {
                    self.saved_index_pending = saved.index_pending;
                    if saved.index_pending {
                        self.saved_error = None;
                    } else if !saved.valid {
                        self.saved_error = saved.error.clone().or_else(|| Some("query".to_owned()));
                    } else {
                        self.saved_error = None;
                    }
                }
                None => {
                    self.applied_saved = None;
                    self.saved_error = None;
                    self.saved_index_pending = false;
                }
            }
        } else {
            self.saved_index_pending = false;
        }
    }

    /// Refresh saved-search counts after any engine write and at least every two seconds for a
    /// second process's CRUD. The engine's own count cache makes the first form cheap.
    fn refresh_saved_searches_if_stale(&mut self) {
        let revision_changed = self.saved_searches_revision != Some(self.engine.revision());
        let poll_due = self.now - self.last_saved_refresh >= 2.0;
        if revision_changed || poll_due {
            self.refresh_saved_searches();
        }
    }

    fn refresh_recent_searches(&mut self) {
        let account = self.active_account.clone();
        self.recent_searches = account.as_deref().and_then(|account| self.engine.recent_searches(Some(account)).ok()).unwrap_or_default();
    }

    fn start_saved_search_save(&mut self) {
        self.label_editor = None;
        if self.search_committed.trim().is_empty() {
            self.status = "Scrivi una ricerca prima di salvarla".to_owned();
            return;
        }
        if self.saved_searches.len() >= 12 {
            self.status = "Hai già 12 ricerche salvate: eliminane una prima di aggiungerne un'altra".to_owned();
            return;
        }
        let mut name = self.search_committed.chars().take(40).collect::<String>();
        if name.trim().is_empty() {
            name = "Nuova ricerca".to_owned();
        }
        self.saved_editor = Some(SavedSearchEditor {
            mode: SavedSearchEditorMode::Create,
            name,
            query: self.search_committed.clone(),
            sort: "relevance".to_owned(),
            error: None,
            focus: true,
        });
    }

    fn start_saved_search_rename(&mut self, id: &str) {
        self.label_editor = None;
        let Some(saved) = self.saved_searches.iter().find(|saved| saved.id == id).cloned() else { return };
        self.saved_editor = Some(SavedSearchEditor {
            mode: SavedSearchEditorMode::Update(saved.id),
            name: saved.name,
            query: saved.query,
            sort: saved.sort,
            error: None,
            focus: true,
        });
    }

    fn cancel_saved_search_editor(&mut self) {
        self.saved_editor = None;
    }

    fn commit_saved_search_editor(&mut self) {
        let Some(editor) = self.saved_editor.as_ref() else { return };
        let mode = match &editor.mode {
            SavedSearchEditorMode::Create => None,
            SavedSearchEditorMode::Update(id) => Some(id.clone()),
        };
        let name = editor.name.trim().to_owned();
        let query = editor.query.trim().to_owned();
        let sort = editor.sort.clone();
        if name.is_empty() || query.is_empty() {
            if let Some(editor) = self.saved_editor.as_mut() {
                editor.error = Some("Servono un nome e una ricerca".to_owned());
            }
            return;
        }
        let account = self.active_account.clone();
        let previous = mode.as_deref().and_then(|id| self.saved_searches.iter().find(|saved| saved.id == id).cloned());
        let update_account = previous.as_ref().map(|saved| saved.account_id.clone()).or_else(|| account.clone());
        let result = match mode {
            Some(id) => self.op("saved_search_update", json!({ "id": id, "account": update_account, "name": name, "query": query, "sort": sort })),
            None => self.op("saved_search_create", json!({ "account": account, "name": name, "query": query, "sort": sort })),
        };
        match result {
            Ok(_) => {
                self.saved_editor = None;
                self.refresh_saved_searches();
                self.scroll_saved_into_view = true;
                self.status = "Ricerca salvata".to_owned();
                if let Some(previous) = previous {
                    self.push_toast(
                        "Ricerca aggiornata",
                        UndoAction::SavedSearchUpdate {
                            id: previous.id,
                            account: previous.account_id,
                            name: previous.name,
                            query: previous.query,
                            sort: previous.sort,
                        },
                    );
                }
            }
            Err(postino_core::OpError::InvalidArgs(message)) => {
                if let Some(editor) = self.saved_editor.as_mut() {
                    editor.error = Some(saved_editor_error(&message));
                }
            }
            Err(_) => {
                self.report_failure("Non è stato possibile salvare la ricerca.", false);
            }
        }
    }

    fn delete_saved_search(&mut self, id: &str) {
        let Some(saved) = self.saved_searches.iter().find(|saved| saved.id == id).cloned() else { return };
        match self.op("saved_search_delete", json!({ "id": id, "account": saved.account_id })) {
            Ok(_) => {
                self.refresh_saved_searches();
                let message = format!("Ricerca “{}” eliminata", saved.name);
                self.status = message.clone();
                self.push_toast(
                    message,
                    UndoAction::SavedSearchDelete { name: saved.name, query: saved.query, sort: saved.sort, account: saved.account_id },
                );
            }
            Err(_) => self.report_failure("Non è stato possibile eliminare la ricerca.", false),
        }
    }

    fn apply_saved_search(&mut self, id: &str) {
        let Some(saved) = self.saved_searches.iter().find(|saved| saved.id == id).cloned() else { return };
        if self.saved_restore.is_none() {
            self.saved_restore = Some(SavedRestore {
                folder: self.folder.clone(),
                label_filter: self.label_filter.clone(),
                label_folder: self.label_folder.clone(),
                query: self.query.clone(),
                search_committed: self.search_committed.clone(),
                unread_only: self.unread_only,
                attachments_only: self.attachments_only,
                threads_view: self.threads_view,
                threads_view_before_search: self.threads_view_before_search,
                saved_folder_scope: self.saved_folder_scope,
                saved_query_folder: self.saved_query_folder,
            });
        }
        self.saved_folder_scope = true;
        self.saved_query_folder = query_has_folder(&saved.query);
        self.applied_saved = Some(saved.id.clone());
        self.scroll_saved_into_view = true;
        self.saved_index_pending = saved.index_pending;
        self.saved_error = if saved.index_pending || saved.valid { None } else { saved.error.clone().or_else(|| Some("query".to_owned())) };
        self.label_filter = None;
        self.label_folder = None;
        self.unread_only = false;
        self.attachments_only = false;
        self.query = saved.query.clone();
        self.search_committed = saved.query.clone();
        self.search_debounce_until = None;
        self.search_limit = 200;
        self.search_error = None;
        self.threads_view = false;
        self.threads_view_before_search = None;
        self.clear_thread_selection();
        self.list_cache = None;
        self.list_signature.clear();
        if saved.valid && !saved.index_pending {
            let account = self.active_account.clone();
            self.remember_search(account.as_deref(), &saved.query);
            self.refresh_recent_searches();
        }
        self.status = format!("Ricerca “{}”", saved.name);
    }

    /// Esc after a saved search: put the query, filters and threaded view back exactly as they were.
    fn restore_saved_search(&mut self) -> bool {
        let Some(restore) = self.saved_restore.take() else { return false };
        self.folder = restore.folder;
        self.label_filter = restore.label_filter;
        self.label_folder = restore.label_folder;
        self.query = restore.query;
        self.search_committed = restore.search_committed;
        self.unread_only = restore.unread_only;
        self.attachments_only = restore.attachments_only;
        self.threads_view = restore.threads_view;
        self.threads_view_before_search = restore.threads_view_before_search;
        self.saved_folder_scope = restore.saved_folder_scope;
        self.saved_query_folder = restore.saved_query_folder;
        self.saved_error = None;
        self.saved_index_pending = false;
        self.applied_saved = None;
        self.saved_editor = None;
        self.search_debounce_until = None;
        self.search_error = None;
        self.search_limit = 200;
        self.list_cache = None;
        self.list_signature.clear();
        self.threads_cache = None;
        self.clear_thread_selection();
        true
    }

    fn toggle_threads_view(&mut self) {
        self.threads_view = !self.threads_view;
        self.threads_view_before_search = None;
        if self.threads_view {
            self.status = "Conversazioni".to_owned();
            if let Some(id) = self.selected.clone()
                && let Some(thread) = self.engine.messages().iter().find(|message| message.id == id).map(|message| message.thread_id.clone())
            {
                self.select_thread(thread);
            }
        } else {
            self.status = "Messaggi".to_owned();
            self.clear_thread_selection();
        }
        self.list_cache = None;
        self.threads_cache = None;
    }

    fn clear_thread_selection(&mut self) {
        self.selected_thread = None;
        self.thread_messages.clear();
        self.thread_summary = None;
        self.thread_total = 0;
        self.thread_open = None;
    }

    fn select_thread(&mut self, id: String) {
        self.load_thread_page(id, true, None);
    }

    /// Load one conversation. `open` chooses and expands the latest unread card; without it the
    /// thread is only selected (J/K before Enter). `offset` loads a specific page; `None` loads the
    /// newest page so the latest unread is visible even in a thread larger than the 200-message cap.
    fn load_thread_page(&mut self, id: String, open: bool, offset: Option<u32>) {
        let account = self.active_account.clone();
        let summary = match self.op("thread_get", json!({ "id": id, "account": account, "limit": 1, "offset": 0 })) {
            Ok(value) => match serde_json::from_value::<ThreadGetResult>(value) {
                Ok(detail) => detail,
                Err(_) => {
                    self.report_failure("Non è stato possibile leggere la conversazione.", false);
                    return;
                }
            },
            Err(error) => {
                self.report_failure(op_error_copy(&error), false);
                return;
            }
        };
        let total = summary.total;
        let start = offset.unwrap_or_else(|| total.saturating_sub(200));
        let page = match self.op("thread_get", json!({ "id": summary.resolved_id, "account": account, "limit": 200, "offset": start })) {
            Ok(value) => match serde_json::from_value::<ThreadGetResult>(value) {
                Ok(detail) => detail,
                Err(_) => {
                    self.report_failure("Non è stato possibile leggere la conversazione.", false);
                    return;
                }
            },
            Err(error) => {
                self.report_failure(op_error_copy(&error), false);
                return;
            }
        };
        self.selected_thread = Some(page.resolved_id.clone());
        self.thread_summary = Some(page.thread.clone());
        self.thread_messages = page.messages;
        self.thread_total = page.total;
        self.thread_offset = start.min(page.total);
        self.thread_open = None;
        if open {
            let open_id = self
                .thread_messages
                .iter()
                .rev()
                .find(|message| message.unread)
                .or_else(|| self.thread_messages.last())
                .map(|message| message.id.clone());
            if let Some(open_id) = open_id {
                self.thread_open = Some(open_id.clone());
                self.open_message(open_id);
            } else {
                self.close_reading();
            }
        } else {
            if let Some(latest) = self.thread_messages.last().map(|message| message.id.clone()) {
                self.selected = Some(latest.clone());
                self.select_only(latest);
            }
            self.close_reading();
        }
        self.threads_cache = None;
    }

    /// Prepend the page before the first loaded card. The open card and scroll position stay put.
    fn load_older_thread_messages(&mut self) {
        let Some(id) = self.selected_thread.clone() else { return };
        if self.thread_offset == 0 {
            return;
        }
        let account = self.active_account.clone();
        let offset = self.thread_offset.saturating_sub(200);
        match self.op("thread_get", json!({ "id": id, "account": account, "limit": 200, "offset": offset })) {
            Ok(value) => match serde_json::from_value::<ThreadGetResult>(value) {
                Ok(detail) => {
                    let known = self.thread_messages.iter().map(|message| message.id.clone()).collect::<BTreeSet<_>>();
                    let mut older = detail.messages.into_iter().filter(|message| !known.contains(&message.id)).collect::<Vec<_>>();
                    older.append(&mut self.thread_messages);
                    self.thread_messages = older;
                    self.thread_offset = offset.min(detail.total);
                    self.thread_total = detail.total;
                    self.thread_summary = Some(detail.thread);
                }
                Err(_) => self.report_failure("Non è stato possibile leggere i messaggi precedenti.", false),
            },
            Err(error) => self.report_failure(op_error_copy(&error), false),
        }
    }

    fn open_thread_message(&mut self, id: String) {
        self.thread_open = Some(id.clone());
        self.open_message(id);
    }

    /// J/K inside a conversation moves the open card, not the thread list. At the top of the loaded
    /// page it pulls the previous page in first.
    fn step_thread_message(&mut self, delta: i32) {
        if self.thread_messages.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|id| self.thread_messages.iter().position(|message| &message.id == id));
        if delta < 0 && self.thread_offset > 0 && current.is_none_or(|index| index == 0) {
            self.load_older_thread_messages();
        }
        let current = self.selected.as_ref().and_then(|id| self.thread_messages.iter().position(|message| &message.id == id));
        let next = match current {
            Some(index) => {
                let index =
                    i32::try_from(index).unwrap_or(0).saturating_add(delta).clamp(0, i32::try_from(self.thread_messages.len() - 1).unwrap_or(0));
                usize::try_from(index).unwrap_or(0)
            }
            None => 0,
        };
        if let Some(message) = self.thread_messages.get(next).map(|message| message.id.clone()) {
            self.open_thread_message(message);
        }
    }

    /// Every message of the open conversation, paged from the store. Bulk actions cover the whole
    /// thread, not the visible window; the 500-message mutation cap is enforced here too.
    fn thread_all_messages(&mut self) -> Result<Vec<MessageSummary>, String> {
        let Some(id) = self.active_thread().map(str::to_owned) else {
            return Err("Nessuna conversazione selezionata".to_owned());
        };
        if self.thread_total > 500 {
            return Err("La conversazione supera i 500 messaggi: usa i singoli messaggi".to_owned());
        }
        let account = self.active_account.clone();
        let mut all = Vec::new();
        let mut offset = 0u32;
        while all.len() < usize::try_from(self.thread_total).unwrap_or(usize::MAX) {
            let value = self
                .engine
                .call("thread_get", json!({ "id": id, "account": account, "limit": 200, "offset": offset }))
                .map_err(|error| op_error_copy(&error))?;
            let detail = serde_json::from_value::<ThreadGetResult>(value).map_err(|_| "Risposta della conversazione non valida".to_owned())?;
            let received = detail.messages.len();
            all.extend(detail.messages);
            if received == 0 {
                break;
            }
            offset = offset.saturating_add(u32::try_from(received).unwrap_or(u32::MAX));
        }
        Ok(all)
    }

    fn thread_all_ids(&mut self) -> Result<Vec<String>, String> {
        Ok(self.thread_all_messages()?.into_iter().map(|message| message.id).collect())
    }

    /// A single-message flag can change the open conversation's unread/star totals. Refresh the
    /// banner and the loaded cards from the engine instead of leaving the cached summary stale.
    fn refresh_open_thread(&mut self) {
        let Some(id) = self.selected_thread.clone() else { return };
        let account = self.active_account.clone();
        let offset = self.thread_offset;
        if let Ok(value) = self.op("thread_get", json!({ "id": id, "account": account, "limit": 200, "offset": offset }))
            && let Ok(detail) = serde_json::from_value::<ThreadGetResult>(value)
        {
            self.thread_summary = Some(detail.thread);
            self.thread_messages = detail.messages;
            self.thread_total = detail.total;
            self.threads_cache = None;
        }
    }

    fn set_thread_flag(&mut self, unread: Option<bool>, starred: Option<bool>) {
        let Some(id) = self.active_thread().map(str::to_owned) else { return };
        let before = match self.thread_all_messages() {
            Ok(messages) => messages.into_iter().map(|message| (message.id, message.unread, message.starred)).collect::<Vec<_>>(),
            Err(message) => {
                self.status = message;
                return;
            }
        };
        match self.op("thread_flag", json!({ "id": id, "unread": unread, "starred": starred })) {
            Ok(value) => {
                self.pending_read = None;
                let mut unread_changes = Vec::new();
                let mut starred_changes = Vec::new();
                for (message_id, was_unread, was_starred) in &before {
                    if unread.is_some_and(|wanted| wanted != *was_unread) {
                        unread_changes.push((message_id.clone(), *was_unread));
                    }
                    if starred.is_some_and(|wanted| wanted != *was_starred) {
                        starred_changes.push((message_id.clone(), *was_starred));
                    }
                }
                for message in &mut self.thread_messages {
                    if let Some(wanted) = unread {
                        message.unread = wanted;
                    }
                    if let Some(wanted) = starred {
                        message.starred = wanted;
                    }
                }
                if let Ok(detail) = serde_json::from_value::<postino_core::ThreadFlagOutput>(value) {
                    self.thread_summary = Some(detail.thread);
                }
                if !unread_changes.is_empty() {
                    let message = if unread == Some(true) { "Conversazione non letta" } else { "Conversazione letta" }.to_owned();
                    self.status = message.clone();
                    self.push_toast(message, UndoAction::Unread(unread_changes));
                } else if !starred_changes.is_empty() {
                    let message =
                        if starred == Some(true) { "Stella aggiunta alla conversazione" } else { "Stella rimossa dalla conversazione" }.to_owned();
                    self.status = message.clone();
                    self.push_toast(message, UndoAction::Starred(starred_changes));
                } else {
                    self.status = "Nessun messaggio da aggiornare".to_owned();
                }
                self.refresh_labels();
                self.threads_cache = None;
                self.list_cache = None;
            }
            Err(_) => self.report_failure("Non è stato possibile aggiornare la conversazione.", false),
        }
    }

    /// The search field names the active label, so the placeholder never claims a mailbox-wide
    /// scope while the list is filtered.
    fn search_placeholder(&self) -> String {
        match self.label_filter.as_deref().and_then(|id| self.label_summary(id)) {
            Some(label) => format!("Cerca in {}…", label.name),
            None => "Cerca in tutte le mail…".to_owned(),
        }
    }

    fn select_label(&mut self, id: String) {
        self.label_filter = Some(id);
        self.label_folder = None;
        self.label_editor = None;
        self.label_menu = None;
        self.clear_search();
        self.unread_only = false;
        self.attachments_only = false;
        self.clear_selection();
        self.reading_open = false;
        self.reading_full = false;
        self.clear_thread_selection();
        self.threads_cache = None;
        self.thread_list_limit = 200;
    }

    fn clear_label_filter(&mut self) {
        self.label_filter = None;
        self.label_folder = None;
    }

    fn start_label_create(&mut self) {
        self.saved_editor = None;
        self.start_label_create_for(Vec::new());
    }

    /// The sidebar `+` action. While the editor is open it only returns focus to the field, so a
    /// second click cannot silently throw away a half-typed name.
    fn start_label_create_or_focus(&mut self) {
        if let Some(editor) = self.label_editor.as_mut() {
            editor.focus = true;
            return;
        }
        self.start_label_create();
    }

    /// Create a label and, when the message label menu asked for it, apply it to the targets that
    /// were captured when the menu opened. Creation itself is not an undo step.
    fn start_label_create_for(&mut self, apply_to: Vec<String>) {
        self.label_menu = None;
        self.label_editor = Some(LabelEditor {
            mode: LabelEditorMode::Create,
            text: String::new(),
            color: LabelColor::Gold,
            error: None,
            focus: true,
            focused: true,
            swatch_focused: false,
            apply_to,
        });
        self.label_delete = None;
    }

    fn start_label_rename(&mut self, id: &str) {
        self.saved_editor = None;
        let Some(label) = self.engine.label(id) else {
            self.report_failure("Etichetta non trovata.", false);
            return;
        };
        self.label_menu = None;
        self.label_editor = Some(LabelEditor {
            mode: LabelEditorMode::Rename(id.to_owned()),
            text: label.name.clone(),
            color: label.color,
            error: None,
            focus: true,
            focused: true,
            swatch_focused: false,
            apply_to: Vec::new(),
        });
        self.label_delete = None;
    }

    fn cancel_label_editor(&mut self) {
        self.label_editor = None;
        self.label_menu = None;
    }

    /// Commit the inline field. A duplicate or invalid name keeps the field open with an Italian
    /// reason on the field itself and requests focus again.
    fn commit_label_editor(&mut self) {
        let Some(editor) = self.label_editor.take() else {
            return;
        };
        let previous = match &editor.mode {
            LabelEditorMode::Create => None,
            LabelEditorMode::Rename(id) => self.engine.label(id).map(|label| (label.name.clone(), label.color)),
        };
        let color = editor.color.as_str();
        let result = match &editor.mode {
            LabelEditorMode::Create => self
                .engine
                .call("label_create", json!({ "name": editor.text, "color": color }))
                .and_then(|value| serde_json::from_value::<LabelCreate>(value).map_err(|error| postino_core::OpError::Failed(error.to_string())))
                .map(|created| created.label),
            LabelEditorMode::Rename(id) => self
                .engine
                .call("label_update", json!({ "id": id, "name": editor.text, "color": color }))
                .and_then(|value| serde_json::from_value::<LabelUpdate>(value).map_err(|error| postino_core::OpError::Failed(error.to_string())))
                .map(|updated| updated.label),
        };
        match result {
            Ok(label) => {
                self.refresh_labels();
                match &editor.mode {
                    LabelEditorMode::Create => {
                        self.status = format!("Etichetta “{}” creata", label.name);
                        // Creation is explicitly not undoable: a stale toast from another action
                        // must not claim it can be reversed.
                        self.toast = None;
                        if !editor.apply_to.is_empty() {
                            self.apply_label(&editor.apply_to, &label.id, true);
                        }
                    }
                    LabelEditorMode::Rename(_) => {
                        self.status = format!("Etichetta rinominata in “{}”", label.name);
                        if let Some((name, previous_color)) = previous
                            && (name != label.name || previous_color != label.color)
                        {
                            self.push_toast(
                                format!("Etichetta rinominata in “{}”", label.name),
                                UndoAction::LabelRename { id: label.id.clone(), name, color: previous_color },
                            );
                        }
                    }
                }
            }
            Err(error) => {
                self.label_editor = Some(LabelEditor {
                    mode: editor.mode,
                    text: editor.text,
                    color: editor.color,
                    error: Some(label_error_message(&error.to_string())),
                    focus: true,
                    focused: true,
                    swatch_focused: false,
                    apply_to: editor.apply_to,
                });
            }
        }
    }

    fn request_label_delete(&mut self, id: &str) {
        let Some(label) = self.engine.label(id) else {
            self.report_failure("Etichetta non trovata.", false);
            return;
        };
        let message_ids = self
            .engine
            .messages()
            .iter()
            .filter(|message| message.labels.iter().any(|candidate| candidate == id))
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        self.label_delete = Some(LabelDeleteState {
            id: id.to_owned(),
            name: label.name.clone(),
            count: u32::try_from(message_ids.len()).unwrap_or(0),
            message_ids,
            focus_primary: true,
        });
        self.label_editor = None;
        self.label_menu = None;
    }

    fn delete_label(&mut self, state: &LabelDeleteState) {
        match self.op("label_delete", json!({ "id": state.id })) {
            Ok(value) => match serde_json::from_value::<LabelDelete>(value) {
                Ok(deleted) => {
                    self.refresh_labels();
                    if self.label_filter.as_deref() == Some(state.id.as_str()) {
                        self.clear_label_filter();
                    }
                    let removed = plural(deleted.affected, "messaggio", "messaggi");
                    let message = format!("Etichetta “{}” eliminata · rimossa da {removed}", deleted.label.name);
                    self.status = message.clone();
                    self.push_toast(
                        message,
                        UndoAction::LabelDelete { name: deleted.label.name, color: deleted.label.color, message_ids: state.message_ids.clone() },
                    );
                }
                Err(_) => self.report_failure("Non è stato possibile eliminare l'etichetta.", false),
            },
            Err(_) => self.report_failure("Non è stato possibile eliminare l'etichetta.", false),
        }
    }

    /// The selection, folder and filters the label menu was opened against. The menu closes rather
    /// than applying a label to a message the user no longer sees selected.
    fn label_menu_context(&self, target: Vec<String>) -> LabelMenuContext {
        LabelMenuContext {
            target,
            selection: self.action_ids(),
            folder: self.folder.clone(),
            label_filter: self.label_filter.clone(),
            label_folder: self.label_folder.clone(),
            query: self.search_committed.clone(),
            unread_only: self.unread_only,
            attachments_only: self.attachments_only,
        }
    }

    fn label_menu_is_stale(&self) -> bool {
        let Some(menu) = self.label_menu.as_ref() else {
            return false;
        };
        menu.context.selection != self.action_ids()
            || menu.context.folder != self.folder
            || menu.context.label_filter != self.label_filter
            || menu.context.label_folder != self.label_folder
            || menu.context.query != self.search_committed
            || menu.context.unread_only != self.unread_only
            || menu.context.attachments_only != self.attachments_only
    }

    fn open_label_menu(&mut self, target: Vec<String>) {
        if target.is_empty() {
            self.status = "Seleziona un messaggio per applicare un'etichetta".to_owned();
            return;
        }
        let context = self.label_menu_context(target);
        self.label_menu = Some(LabelMenuState { context, query: String::new(), focus: true });
        self.label_editor = None;
        self.label_delete = None;
    }

    /// One click applies the label to every target that lacks it; a second removes it everywhere.
    fn toggle_label_target(&mut self, id: &str) {
        if self.label_menu_is_stale() {
            self.label_menu = None;
            self.status = "La selezione è cambiata: riapri il menu etichetta".to_owned();
            return;
        }
        let Some(menu) = self.label_menu.as_ref() else {
            return;
        };
        let target = menu.context.target.clone();
        let all = target
            .iter()
            .all(|message_id| self.engine.message(message_id).is_some_and(|message| message.summary.labels.iter().any(|label| label == id)));
        self.apply_label(&target, id, !all);
    }

    fn apply_label(&mut self, ids: &[String], id: &str, apply: bool) {
        if ids.is_empty() {
            return;
        }
        let Some(label) = self.engine.label(id).cloned() else {
            self.report_failure("Etichetta non trovata.", false);
            return;
        };
        let before = ids
            .iter()
            .filter_map(|message_id| {
                self.engine.message(message_id).map(|message| (message_id.clone(), message.summary.labels.iter().any(|candidate| candidate == id)))
            })
            .filter(|(_, had)| *had != apply)
            .map(|(message_id, had)| (message_id, id.to_owned(), had))
            .collect::<Vec<_>>();
        if before.is_empty() {
            self.status = if apply { "Etichetta già applicata" } else { "Etichetta non presente" }.to_owned();
            return;
        }
        match self.op("label_apply", json!({ "id": id, "message_ids": ids, "apply": apply })) {
            Ok(value) => match serde_json::from_value::<LabelApply>(value) {
                Ok(result) => {
                    self.refresh_labels();
                    let count = plural(result.changed, "messaggio", "messaggi");
                    let message = format!("Etichetta “{}” {} {count}", label.name, if apply { "applicata a" } else { "rimossa da" });
                    self.status = message.clone();
                    self.push_toast(message, UndoAction::Labels(before));
                }
                Err(_) => self.report_failure("Non è stato possibile applicare l'etichetta.", false),
            },
            Err(_) => self.report_failure("Non è stato possibile applicare l'etichetta.", false),
        }
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.anchor = None;
        self.selected = None;
        self.pending_read = None;
    }

    fn selection_ids(&self) -> Vec<String> {
        if self.selection.is_empty() { self.selected.iter().cloned().collect() } else { self.selection.iter().cloned().collect() }
    }

    /// Keyboard shortcuts and context menus apply to the whole selection. The reading pane always
    /// acts on its focused message only.
    fn action_ids(&self) -> Vec<String> {
        self.selection_ids()
    }

    fn select_only(&mut self, id: String) {
        self.selection.clear();
        self.selection.insert(id.clone());
        self.anchor = Some(id.clone());
        self.selected = Some(id);
    }

    /// Move the highlight without opening the reading pane. `J`/`K` use this while the pane is
    /// closed, so browsing never marks anything read.
    fn focus_message(&mut self, id: String) {
        if self.selected.as_deref() != Some(id.as_str()) {
            self.pending_read = None;
        }
        if self.reading_open {
            self.close_reading();
        }
        self.select_only(id);
    }

    /// Select and open a message. Read state is delayed by 600 ms so a quick pass through the
    /// list does not clear every unread marker.
    fn open_message(&mut self, id: String) {
        let same = self.selected.as_deref() == Some(id.as_str());
        if !same {
            self.pending_read = None;
            self.reading.attachment_preview = None;
        }
        self.select_only(id.clone());
        self.reading_open = true;
        let unread = self.engine.message(&id).is_some_and(|message| message.summary.unread);
        let already_pending = self.pending_read.as_ref().is_some_and(|pending| pending.id == id);
        if unread && !already_pending {
            self.pending_read = Some(PendingRead { id, at: self.now + 0.6 });
        }
    }

    fn toggle_selection(&mut self, id: String) {
        if !self.selection.remove(&id) {
            self.selection.insert(id.clone());
            self.anchor = Some(id.clone());
            self.selected = Some(id);
        } else if self.selected.as_deref() == Some(id.as_str()) {
            self.selected = self.selection.iter().next_back().cloned();
            if self.anchor.as_deref() == Some(id.as_str()) {
                self.anchor = self.selected.clone();
            }
        }
        if self.selection.is_empty() {
            self.anchor = None;
            self.selected = None;
            self.close_reading();
        }
        self.close_reading_if_multi();
    }

    fn extend_selection(&mut self, id: String, rows: &[MessageSummary]) {
        let anchor = self.anchor.clone().or_else(|| self.selected.clone());
        let Some(anchor) = anchor else {
            self.focus_message(id);
            return;
        };
        let Some(from) = rows.iter().position(|message| message.id == anchor) else {
            self.focus_message(id);
            return;
        };
        let Some(to) = rows.iter().position(|message| message.id == id) else {
            return;
        };
        let (start, end) = if from <= to { (from, to) } else { (to, from) };
        self.selection = rows[start..=end].iter().map(|message| message.id.clone()).collect();
        self.selected = Some(id);
        self.close_reading_if_multi();
    }

    fn select_all(&mut self, rows: &[MessageSummary]) {
        if rows.is_empty() {
            return;
        }
        self.selection = rows.iter().map(|message| message.id.clone()).collect();
        let focus = self.selected.as_ref().and_then(|id| rows.iter().find(|message| &message.id == id)).map(|message| message.id.clone());
        self.selected = focus.or_else(|| rows.first().map(|message| message.id.clone()));
        self.anchor = rows.first().map(|message| message.id.clone());
        self.close_reading_if_multi();
    }

    fn close_reading_if_multi(&mut self) {
        if self.selection.len() > 1 {
            self.close_reading();
        }
    }

    fn prune_selection(&mut self, rows: &[MessageSummary]) {
        if self.selection.is_empty() {
            return;
        }
        let visible: BTreeSet<&str> = rows.iter().map(|message| message.id.as_str()).collect();
        self.selection.retain(|id| visible.contains(id.as_str()));
        if self.selection.is_empty() {
            self.selected = None;
            self.anchor = None;
            self.pending_read = None;
            return;
        }
        // The focus row is always inside the batch: a plain J/K collapses to it, Shift+J/K keeps
        // the whole range selected. A row that left the list moves the focus to the last visible
        // selection.
        if let Some(selected) = self.selected.as_deref()
            && !self.selection.contains(selected)
        {
            self.selected = self.selection.iter().next_back().cloned();
        }
        if let Some(anchor) = self.anchor.as_deref()
            && !self.selection.contains(anchor)
        {
            self.anchor = self.selected.clone();
        }
    }

    fn row_targets(&self, id: &str) -> Vec<String> {
        if self.selection.len() > 1 && self.selection.contains(id) { self.selection_ids() } else { vec![id.to_owned()] }
    }

    fn run_row_action(&mut self, id: &str, action: RowAction) {
        let targets = self.row_targets(id);
        match action {
            RowAction::Open => self.open_message(id.to_owned()),
            RowAction::Reply => self.reply_to(id, ComposeMode::Reply),
            RowAction::ReplyAll => self.reply_to(id, ComposeMode::ReplyAll),
            RowAction::Forward => self.reply_to(id, ComposeMode::Forward),
            RowAction::EditDraft => self.edit_draft(id),
            RowAction::MarkRead => self.set_unread_many(&targets, false),
            RowAction::MarkUnread => self.set_unread_many(&targets, true),
            RowAction::Label => self.open_label_menu(targets),
            RowAction::Star => self.set_starred_many(&targets, true),
            RowAction::Unstar => self.set_starred_many(&targets, false),
            RowAction::Archive => self.move_many_to_kind(&targets, FolderKind::Archive, "Messaggio archiviato", "Messaggi archiviati"),
            RowAction::Delete => {
                self.move_many_to_kind(&targets, FolderKind::Trash, "Messaggio spostato nel cestino", "Messaggi spostati nel cestino")
            }
        }
    }

    fn bulk_bar(&mut self, ui: &mut Ui, t: &Tokens) {
        let ids = self.action_ids();
        if ids.len() < 2 {
            return;
        }
        #[cfg(test)]
        probe::bulk_bar(ui, ui.available_rect_before_wrap());
        let all_starred = ids.iter().all(|id| self.engine.message(id).is_some_and(|message| message.summary.starred));
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(typography::strong(Role::BodySmall, format!("{} selezionati", ids.len()), t.text));
            ui.add_space(space::MD);
            if IconButton::new(command_icon("read", icons::MARK_READ), command_label("read", "Segna come letto"))
                .size(IconButtonSize::Sm)
                .show(ui)
                .clicked()
            {
                action = Some(BulkAction::Read);
            }
            if IconButton::new(command_icon("unread", icons::MARK_UNREAD), command_label("unread", "Segna come non letto"))
                .size(IconButtonSize::Sm)
                .show(ui)
                .clicked()
            {
                action = Some(BulkAction::Unread);
            }
            if all_starred {
                if IconButton::new(icons::STAR_FILL, command_label("unstar", "Togli stella"))
                    .filled(true)
                    .accent(true)
                    .size(IconButtonSize::Sm)
                    .show(ui)
                    .clicked()
                {
                    action = Some(BulkAction::Unstar);
                }
            } else if IconButton::new(command_icon("star", icons::STAR), command_label("star", "Aggiungi stella"))
                .size(IconButtonSize::Sm)
                .show(ui)
                .clicked()
            {
                action = Some(BulkAction::Star);
            }
            ui.add_space(space::XS);
            if IconButton::new(command_icon("archive", icons::ARCHIVE), command_label("archive", "Archivia"))
                .size(IconButtonSize::Sm)
                .show(ui)
                .clicked()
            {
                action = Some(BulkAction::Archive);
            }
            if IconButton::new(command_icon("delete", icons::TRASH), command_label("delete", "Elimina"))
                .danger(true)
                .size(IconButtonSize::Sm)
                .show(ui)
                .clicked()
            {
                action = Some(BulkAction::Delete);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if Button::ghost(command_label("clear_selection", "Deseleziona")).size(ButtonSize::Sm).show(ui).clicked() {
                    action = Some(BulkAction::Clear);
                }
            });
        });
        match action {
            Some(BulkAction::Read) => self.set_unread_many(&ids, false),
            Some(BulkAction::Unread) => self.set_unread_many(&ids, true),
            Some(BulkAction::Star) => self.set_starred_many(&ids, true),
            Some(BulkAction::Unstar) => self.set_starred_many(&ids, false),
            Some(BulkAction::Archive) => self.move_many_to_kind(&ids, FolderKind::Archive, "Messaggio archiviato", "Messaggi archiviati"),
            Some(BulkAction::Delete) => {
                self.move_many_to_kind(&ids, FolderKind::Trash, "Messaggio spostato nel cestino", "Messaggi spostati nel cestino")
            }
            Some(BulkAction::Clear) => self.clear_selection(),
            None => {}
        }
    }

    fn close_reading(&mut self) {
        self.reading_open = false;
        self.reading_full = false;
        self.pending_read = None;
        self.invite_ready = false;
        self.reading.attachment_preview = None;
        self.reading.unsubscribe_info = None;
        self.reading.unsubscribe_checked = None;
        self.reading.show_unsubscribe = false;
    }

    fn move_selected(&mut self, kind: FolderKind, confirmation: &str, plural: &str) {
        if self.active_thread().is_some() {
            let ids = match self.thread_all_ids() {
                Ok(ids) => ids,
                Err(message) => {
                    self.status = message;
                    return;
                }
            };
            self.move_many_to_kind(&ids, kind, confirmation, plural);
            return;
        }
        let ids = self.action_ids();
        self.move_many_to_kind(&ids, kind, confirmation, plural);
    }

    fn move_many_to_kind(&mut self, ids: &[String], kind: FolderKind, confirmation: &str, plural: &str) {
        let Some(folder) = self.folder_for_kind(kind) else {
            self.status = match kind {
                FolderKind::Trash => "Il server non ha una cartella Cestino".to_owned(),
                FolderKind::Archive => "Il server non ha una cartella Archivio".to_owned(),
                _ => "Il server non ha una cartella adatta".to_owned(),
            };
            return;
        };
        self.move_many(ids, &folder, confirmation, plural);
    }

    fn move_many(&mut self, ids: &[String], folder: &str, confirmation: &str, plural: &str) {
        let mut moved = Vec::new();
        let mut failed = false;
        for id in ids {
            let Some(previous) = self.engine.message(id).map(|message| message.summary.folder_id.clone()) else {
                failed = true;
                break;
            };
            match self.op("message_move", json!({ "id": id, "folder": folder })) {
                Ok(_) => moved.push((id.clone(), previous)),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if !moved.is_empty() {
            let message = if moved.len() == 1 { confirmation.to_owned() } else { plural.to_owned() };
            self.status = message.clone();
            self.clear_selection();
            self.clear_thread_selection();
            self.threads_cache = None;
            self.list_cache = None;
            self.reading_open = false;
            self.reading_full = false;
            // A partial batch still gets its undo: the messages that moved are recoverable.
            self.push_toast(message, UndoAction::Move(moved));
        }
        if failed {
            self.failure = Some(Failure {
                message: "Non è stato possibile spostare un messaggio; quelli già spostati sono annullabili.".to_owned(),
                retry_list: false,
            });
            self.status.clear();
        }
    }

    fn step_selection(&mut self, delta: i32, extend: bool) {
        if self.threads_view {
            if self.selected_thread.is_some() && self.reading_open && !extend {
                self.step_thread_message(delta);
            } else {
                self.step_thread_row(delta);
            }
            return;
        }
        let (rows, _, _) = self.visible_messages();
        if rows.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|id| rows.iter().position(|message| &message.id == id));
        let next = match current {
            Some(index) => {
                let index = i32::try_from(index).unwrap_or(0).saturating_add(delta).clamp(0, i32::try_from(rows.len() - 1).unwrap_or(0));
                usize::try_from(index).unwrap_or(0)
            }
            None => 0,
        };
        let Some(message) = rows.get(next) else {
            return;
        };
        if extend {
            self.extend_selection(message.id.clone(), &rows);
        } else if self.reading_open {
            self.open_message(message.id.clone());
        } else {
            // Plain J/K move the focus and collapse to that row, as in a standard list. Shift+J/K
            // is the way to keep a batch and its anchor alive.
            self.focus_message(message.id.clone());
        }
        self.scroll_to_selected = true;
        self.focus_selected_row = true;
    }

    /// J/K in the conversation list moves the selected thread without opening it. Enter opens the
    /// latest unread card through the `open` command.
    fn step_thread_row(&mut self, delta: i32) {
        let (threads, _, _, _) = self.visible_threads();
        if threads.is_empty() {
            return;
        }
        let current = self.selected_thread.as_ref().and_then(|id| threads.iter().position(|thread| &thread.id == id));
        let next = match current {
            Some(index) => {
                let index = i32::try_from(index).unwrap_or(0).saturating_add(delta).clamp(0, i32::try_from(threads.len() - 1).unwrap_or(0));
                usize::try_from(index).unwrap_or(0)
            }
            None => 0,
        };
        let Some(thread) = threads.get(next) else { return };
        self.load_thread_page(thread.id.clone(), false, None);
        self.scroll_to_selected = true;
    }

    fn set_flag_for(&mut self, id: &str, unread: Option<bool>, starred: Option<bool>) -> bool {
        match self.op("message_flag", json!({ "id": id, "unread": unread, "starred": starred })) {
            Ok(_) => {
                // Any explicit flag action wins over the 600 ms auto-read. Otherwise pressing U
                // right after opening would be overwritten by the timer.
                if self.pending_read.as_ref().is_some_and(|pending| pending.id == id) {
                    self.pending_read = None;
                }
                true
            }
            Err(_) => {
                self.report_failure("Non è stato possibile aggiornare il messaggio.", false);
                false
            }
        }
    }

    fn toggle_star(&mut self) {
        if self.active_thread().is_some() {
            let starred = self.thread_summary.as_ref().is_some_and(|thread| thread.starred_count > 0);
            self.set_thread_flag(None, Some(!starred));
            return;
        }
        let ids = self.action_ids();
        if ids.is_empty() {
            return;
        }
        let all_starred = ids.iter().all(|id| self.engine.message(id).is_some_and(|message| message.summary.starred));
        self.set_starred_many(&ids, !all_starred);
    }

    fn set_starred(&mut self, starred: bool) {
        if self.active_thread().is_some() {
            self.set_thread_flag(None, Some(starred));
            return;
        }
        let ids = self.action_ids();
        self.set_starred_many(&ids, starred);
    }

    fn toggle_star_for(&mut self, id: &str) {
        let Some(starred) = self.engine.message(id).map(|message| message.summary.starred) else {
            return;
        };
        self.set_starred_for(id, !starred);
    }

    fn set_starred_for(&mut self, id: &str, starred: bool) {
        self.set_starred_many(&[id.to_owned()], starred);
    }

    fn set_starred_many(&mut self, ids: &[String], starred: bool) {
        // An explicit flag action always wins over the pending auto-read, even when the flag is
        // already in the requested state and no engine call is needed.
        for id in ids {
            if self.pending_read.as_ref().is_some_and(|pending| &pending.id == id) {
                self.pending_read = None;
            }
        }
        let mut changes = Vec::new();
        for id in ids {
            let Some(previous) = self.engine.message(id).map(|message| message.summary.starred) else {
                continue;
            };
            if previous == starred {
                continue;
            }
            if !self.set_flag_for(id, None, Some(starred)) {
                return;
            }
            changes.push((id.clone(), previous));
        }
        if changes.is_empty() {
            return;
        }
        let message = if changes.len() == 1 {
            if starred { "Stella aggiunta" } else { "Stella rimossa" }.to_owned()
        } else if starred {
            format!("Stella aggiunta a {}", plural(changes.len(), "messaggio", "messaggi"))
        } else {
            format!("Stella rimossa da {}", plural(changes.len(), "messaggio", "messaggi"))
        };
        self.status = message.clone();
        self.push_toast(message, UndoAction::Starred(changes));
        self.refresh_open_thread();
    }

    fn toggle_read(&mut self) {
        if self.active_thread().is_some() {
            let unread = self.thread_summary.as_ref().is_none_or(|thread| thread.unread_count == 0);
            self.set_thread_flag(Some(unread), None);
            return;
        }
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(unread) = self.engine.message(&id).map(|message| message.summary.unread) else {
            return;
        };
        self.set_unread_for(&id, !unread);
    }

    fn set_unread_for(&mut self, id: &str, unread: bool) {
        self.set_unread_many(&[id.to_owned()], unread);
    }

    fn set_unread_many(&mut self, ids: &[String], unread: bool) {
        // U on an already-unread message is still an explicit action: it must cancel the timer.
        for id in ids {
            if self.pending_read.as_ref().is_some_and(|pending| &pending.id == id) {
                self.pending_read = None;
            }
        }
        let mut changes = Vec::new();
        for id in ids {
            let Some(previous) = self.engine.message(id).map(|message| message.summary.unread) else {
                continue;
            };
            if previous == unread {
                continue;
            }
            if !self.set_flag_for(id, Some(unread), None) {
                return;
            }
            changes.push((id.clone(), previous));
        }
        if changes.is_empty() {
            return;
        }
        let message = if changes.len() == 1 {
            if unread { "Messaggio non letto" } else { "Messaggio letto" }.to_owned()
        } else if unread {
            format!("{} segnati come non letti", plural(changes.len(), "messaggio", "messaggi"))
        } else {
            format!("{} segnati come letti", plural(changes.len(), "messaggio", "messaggi"))
        };
        self.status = message.clone();
        self.push_toast(message, UndoAction::Unread(changes));
        self.refresh_labels();
        self.refresh_open_thread();
    }

    fn mark_read_after_delay(&mut self, id: &str) {
        self.set_unread_for(id, false);
    }

    fn maybe_complete_read(&mut self) {
        let Some(pending) = self.pending_read.take() else {
            return;
        };
        if self.now >= pending.at {
            self.mark_read_after_delay(&pending.id);
        } else {
            self.pending_read = Some(pending);
        }
    }

    fn push_toast(&mut self, message: impl Into<String>, undo: UndoAction) {
        self.failure = None;
        self.toast = Some(ToastState { message: message.into(), undo: Some(undo), until: self.now + 8.0, kind: ToastKind::Info });
    }

    /// A one-shot confirmation or failure without an undo step. The error variant is dismissable;
    /// the success/info variants disappear after eight seconds.
    fn push_info_toast(&mut self, message: impl Into<String>, kind: ToastKind) {
        self.failure = None;
        self.toast = Some(ToastState { message: message.into(), undo: None, until: self.now + 8.0, kind });
    }

    fn undo_last(&mut self) {
        let Some(toast) = self.toast.take() else {
            return;
        };
        let Some(undo) = toast.undo else {
            return;
        };
        let mut failed = false;
        match undo {
            UndoAction::Unread(changes) => {
                for (id, unread) in changes {
                    if self.op("message_flag", json!({ "id": id, "unread": unread, "starred": null })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::Starred(changes) => {
                for (id, starred) in changes {
                    if self.op("message_flag", json!({ "id": id, "unread": null, "starred": starred })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::Move(changes) => {
                for (id, folder) in changes {
                    if self.op("message_move", json!({ "id": id, "folder": folder })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::CancelSend(id) => {
                if self.op("outbox_cancel", json!({ "id": id })).is_err() {
                    failed = true;
                } else {
                    self.refresh_outbox();
                }
            }
            UndoAction::Labels(changes) => {
                for (message_id, label_id, was_applied) in changes {
                    if self.op("label_apply", json!({ "id": label_id, "message_ids": [message_id], "apply": was_applied })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::LabelDelete { name, color, message_ids } => match self.op("label_create", json!({ "name": name, "color": color })) {
                Ok(value) => match serde_json::from_value::<LabelCreate>(value) {
                    Ok(created) => {
                        let ids = message_ids.iter().filter(|id| self.engine.message(id).is_some()).cloned().collect::<Vec<_>>();
                        // A deleted label can carry more assignments than one `label_apply` call
                        // accepts (500), so restore in chunks instead of failing halfway.
                        for chunk in ids.chunks(LABEL_APPLY_CHUNK) {
                            if self.op("label_apply", json!({ "id": created.label.id, "message_ids": chunk, "apply": true })).is_err() {
                                failed = true;
                                break;
                            }
                        }
                    }
                    Err(_) => failed = true,
                },
                Err(_) => failed = true,
            },
            UndoAction::LabelRename { id, name, color } => {
                if self.op("label_update", json!({ "id": id, "name": name, "color": color })).is_err() {
                    failed = true;
                }
            }
            UndoAction::SavedSearchDelete { name, query, sort, account } => {
                if self.op("saved_search_create", json!({ "account": account, "name": name, "query": query, "sort": sort })).is_err() {
                    failed = true;
                } else {
                    self.refresh_saved_searches();
                }
            }
            UndoAction::SavedSearchUpdate { id, account, name, query, sort } => {
                if self
                    .engine
                    .call("saved_search_update", json!({ "id": id, "account": account, "name": name, "query": query, "sort": sort }))
                    .is_err()
                {
                    failed = true;
                } else {
                    self.refresh_saved_searches();
                }
            }
        }
        if failed {
            self.report_failure("Non è stato possibile annullare l'azione.", false);
        } else {
            self.refresh_labels();
            self.refresh_open_thread();
            self.status = "Annullato".to_owned();
        }
    }

    /// Report a list failure once. `visible_messages` runs on every frame, so re-reporting after
    /// the user dismisses the toast would trap them in a loop; a retry or a view change clears the
    /// acknowledgement.
    fn report_list_failure(&mut self) {
        if self.list_error_ack || self.failure.is_some() {
            return;
        }
        self.report_failure("Non è stato possibile leggere i messaggi. Riprova.", true);
    }

    fn visible_messages(&mut self) -> (Vec<MessageSummary>, usize, usize) {
        // A programmatic query (tests, a palette action) commits immediately; typing sets a debounce.
        if self.search_debounce_until.is_none() && self.query.trim() != self.search_committed {
            self.commit_search();
        }
        // Commit a typed query once the 250 ms debounce expires. Enter commits earlier.
        if let Some(until) = self.search_debounce_until
            && self.now >= until
        {
            self.search_debounce_until = None;
            self.commit_search();
        }
        // A changed folder or filter is a fresh read: let it report a failure even if the previous
        // view's toast was dismissed.
        let signature = format!(
            "{}|{}|{}|{}|{:?}|{:?}|{}",
            self.folder,
            self.search_committed,
            self.unread_only,
            self.attachments_only,
            self.label_filter,
            self.label_folder,
            self.active_account.as_deref().unwrap_or_default()
        );
        if signature != self.list_signature {
            self.list_signature = signature.clone();
            self.list_error_ack = false;
            self.list_cache = None;
        }
        if let Some((cached_signature, revision, rows, total, unread, capped)) = &self.list_cache
            && *cached_signature == signature
            && *revision == self.engine.revision()
        {
            self.search_capped = *capped;
            return (rows.clone(), *total, *unread);
        }
        let searching = !self.search_committed.trim().is_empty();
        // A label view spans every folder until the user narrows it. A plain view is one folder.
        let label_scoped = self.label_filter.is_some();
        let list_folder = if label_scoped { self.label_folder.clone() } else { Some(self.folder.clone()) };
        let search_folder = if self.saved_folder_scope {
            // A saved search owns its scope: `folder:` inside the query is the only folder filter.
            None
        } else if label_scoped {
            self.label_folder.clone()
        } else {
            Some(self.folder.clone())
        };
        let account = self.active_account.clone();
        if account.is_none() || (list_folder.as_deref().is_none_or(str::is_empty) && self.label_filter.is_none()) {
            return (Vec::new(), 0, 0);
        }
        let (name, args) = if searching {
            (
                "search",
                json!({
                    "query": self.search_committed,
                    "account": account,
                    "label": self.label_filter,
                    "folder": search_folder,
                    "unread_only": self.unread_only,
                    "with_attachments": self.attachments_only,
                    "limit": self.search_limit
                }),
            )
        } else {
            (
                "messages_list",
                json!({
                    "account": account,
                    "folder": list_folder,
                    "unread_only": self.unread_only,
                    "with_attachments": self.attachments_only,
                    "label": self.label_filter,
                    "limit": 200
                }),
            )
        };
        let active_folders = self.active_folder_ids();
        if searching && let Err(error) = postino_core::parse_search(&self.search_committed) {
            self.search_error = Some((error.code.to_owned(), error.position));
            self.search_capped = false;
            let empty = (Vec::new(), 0, 0);
            self.list_cache = Some((signature, self.engine.revision(), Vec::new(), 0, 0, false));
            return empty;
        }
        let result = match self.op(name, args) {
            Ok(value) if searching => match serde_json::from_value::<SearchResult>(value) {
                Ok(result) => {
                    self.search_error = None;
                    if let Some(index) = result.index {
                        let ready = index.ready;
                        self.index_progress = Some(index);
                        if !ready {
                            self.list_cache = None;
                            self.search_capped = false;
                            return (Vec::new(), 0, 0);
                        }
                    } else {
                        self.index_progress = None;
                    }
                    let mut rows = result
                        .hits
                        .into_iter()
                        .map(|hit| {
                            let mut message = hit.message;
                            // The list shows the matched sentence from the body, not the stored
                            // first-line snippet.
                            message.snippet = hit.snippet;
                            message
                        })
                        .collect::<Vec<_>>();
                    rows.retain(|message| active_folders.contains(&message.folder_id));
                    let unread = rows.iter().filter(|message| message.unread).count();
                    let total = result.total;
                    self.search_capped = result.total_capped;
                    Some((rows, total, unread, result.total_capped))
                }
                Err(_) => {
                    self.report_list_failure();
                    Some((Vec::new(), 0, 0, false))
                }
            },
            Ok(value) => match serde_json::from_value::<MessagesList>(value) {
                Ok(mut result) => {
                    result.messages.retain(|message| active_folders.contains(&message.folder_id));
                    if self.folder_kind(&self.folder) == Some(FolderKind::Starred) {
                        let (total, unread) = self.starred_counts(self.active_account.as_deref().unwrap_or_default());
                        result.total = usize::try_from(total).unwrap_or(usize::MAX);
                        result.unread = usize::try_from(unread).unwrap_or(usize::MAX);
                    }
                    self.index_progress = None;
                    self.search_capped = false;
                    Some((result.messages, result.total, result.unread, false))
                }
                Err(_) => {
                    self.report_list_failure();
                    Some((Vec::new(), 0, 0, false))
                }
            },
            Err(error) => {
                if searching {
                    if let postino_core::OpError::InvalidArgs(message) = &error
                        && let Some(rest) = message.strip_prefix("search query: ")
                        && let Some((code, position)) = rest.split_once(" at position ")
                        && let Ok(position) = position.parse::<usize>()
                    {
                        self.search_error = Some((code.to_owned(), position));
                    } else {
                        self.report_failure("Non è stato possibile leggere i messaggi. Riprova.", true);
                    }
                } else {
                    self.report_list_failure();
                }
                self.search_capped = false;
                Some((Vec::new(), 0, 0, false))
            }
        };
        let (rows, total, unread, capped) = result.unwrap_or_default();
        self.list_cache = Some((signature, self.engine.revision(), rows.clone(), total, unread, capped));
        (rows, total, unread)
    }

    /// The threaded list. A folder view asks SQL for the cached conversation page; a search groups
    /// the flat hits by thread, because `threads_list` has no query argument and the brief keeps
    /// search flat unless the user asks otherwise.
    fn visible_threads(&mut self) -> (Vec<ThreadSummary>, u32, u32, u32) {
        let signature = format!(
            "{}|{}|{}|{}|{:?}|{:?}|{}|threads|{}",
            self.folder,
            self.search_committed,
            self.unread_only,
            self.attachments_only,
            self.label_filter,
            self.label_folder,
            self.active_account.as_deref().unwrap_or_default(),
            self.thread_list_limit
        );
        if let Some((cached, revision, rows, total, unread_threads, unread_messages)) = &self.threads_cache
            && *cached == signature
            && *revision == self.engine.revision()
        {
            return (rows.clone(), *total, *unread_threads, *unread_messages);
        }
        let searching = !self.search_committed.trim().is_empty();
        if searching {
            let (messages, _, _) = self.visible_messages();
            let mut threads = group_threads(&messages, self.active_account.as_deref().unwrap_or_default());
            threads.sort_by(|left, right| right.last_date.cmp(&left.last_date).then(left.id.cmp(&right.id)));
            let total = u32::try_from(threads.len()).unwrap_or(u32::MAX);
            let unread_threads = u32::try_from(threads.iter().filter(|thread| thread.unread_count > 0).count()).unwrap_or(u32::MAX);
            let unread_messages = threads.iter().fold(0u32, |sum, thread| sum.saturating_add(thread.unread_count));
            self.threads_cache = Some((signature, self.engine.revision(), threads.clone(), total, unread_threads, unread_messages));
            return (threads, total, unread_threads, unread_messages);
        }
        let label_scoped = self.label_filter.is_some();
        let list_folder = if label_scoped { self.label_folder.clone() } else { Some(self.folder.clone()) };
        let account = self.active_account.clone();
        if account.is_none() || (list_folder.as_deref().is_none_or(str::is_empty) && self.label_filter.is_none()) {
            return (Vec::new(), 0, 0, 0);
        }
        let args = json!({
            "account": account,
            "folder": list_folder,
            "label": self.label_filter,
            "unread_only": self.unread_only,
            "with_attachments": self.attachments_only,
            "limit": 200
        });
        let wanted = self.thread_list_limit.max(200);
        let mut rows = Vec::new();
        let mut total = 0u32;
        let mut unread_threads = 0u32;
        let mut unread_messages = 0u32;
        let mut offset = 0u32;
        while u32::try_from(rows.len()).unwrap_or(u32::MAX) < wanted {
            let mut page_args = args.clone();
            page_args["offset"] = json!(offset);
            match self.op("threads_list", page_args) {
                Ok(value) => match serde_json::from_value::<ThreadsListResult>(value) {
                    Ok(result) => {
                        total = result.total;
                        unread_threads = result.unread_threads;
                        unread_messages = result.unread_messages;
                        let received = result.threads.len();
                        rows.extend(result.threads);
                        if received < 200 || u32::try_from(rows.len()).unwrap_or(u32::MAX) >= total {
                            break;
                        }
                        offset = offset.saturating_add(u32::try_from(received).unwrap_or(u32::MAX));
                    }
                    Err(_) => {
                        self.report_list_failure();
                        return (Vec::new(), 0, 0, 0);
                    }
                },
                Err(_) => {
                    self.report_list_failure();
                    return (Vec::new(), 0, 0, 0);
                }
            }
        }
        if total == 0 {
            total = u32::try_from(rows.len()).unwrap_or(0);
        }
        self.threads_cache = Some((signature, self.engine.revision(), rows.clone(), total, unread_threads, unread_messages));
        (rows, total, unread_threads, unread_messages)
    }

    /// Keep a pending FTS index moving without blocking the frame: one 500-row batch per tick while
    /// the banner is visible. A demo or an error state stops; the next boot/search retries.
    fn poll_search_index(&mut self, ctx: &egui::Context) {
        if !self.engine.is_persistent() {
            return;
        }
        if !self.index_checked {
            self.index_checked = true;
            if let Ok(Some(status)) = self.engine.search_index_status()
                && !status.ready
            {
                self.index_progress = Some(postino_core::SearchStatusResult::from_store(status));
            }
        }
        let should_build = self.index_progress.as_ref().is_some_and(|status| !status.ready && status.state != "error");
        if should_build && self.daemon_link.is_some() {
            // The daemon owns the index batches and publishes `index` events; a read-only client
            // must not advance the index itself. The banner keeps polling the status it can read.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
            return;
        }
        if should_build && let Ok(Some(status)) = self.engine.advance_search_index() {
            self.index_progress = Some(postino_core::SearchStatusResult::from_store(status));
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
            if self.index_progress.as_ref().is_some_and(|status| status.ready) {
                self.index_progress = None;
                self.saved_searches_revision = None;
                self.refresh_saved_searches();
            }
        }
    }

    /// Remember a recent query. With a daemon the window must not write the store: the list stays
    /// per-session and the daemon persists the queries its own search op records.
    fn remember_search(&mut self, account: Option<&str>, query: &str) {
        if self.daemon_link.is_some() {
            return;
        }
        let _ = self.engine.record_recent_search(account, query);
    }

    /// Forget one account's recent queries: the registry op when a daemon owns the store.
    fn clear_recent_searches(&mut self, account: Option<&str>) -> Result<bool, String> {
        if self.daemon_link.is_some() {
            let value = self.op("saved_search_recent_clear", serde_json::json!({ "account": account })).map_err(|error| error.to_string())?;
            return Ok(value.get("removed").and_then(Value::as_bool).unwrap_or_default());
        }
        self.engine.clear_recent_searches(account).map_err(|error| op_error_copy(&error))
    }

    /// Commit the typed search immediately (Enter). Empty text clears the search.
    fn commit_search(&mut self) {
        let was_empty = self.search_committed.trim().is_empty();
        let record_recent = self.recording_recent;
        self.recording_recent = false;
        let query = self.query.trim().to_owned();
        self.query = query.clone();
        self.search_committed = query;
        self.search_limit = 200;
        self.search_debounce_until = None;
        self.list_cache = None;
        self.threads_cache = None;
        self.search_error = None;
        self.index_progress = None;
        if self.search_committed.is_empty() {
            self.list_signature.clear();
            if let Some(previous) = self.threads_view_before_search.take() {
                self.threads_view = previous;
            }
        } else {
            self.saved_error = None;
            if was_empty {
                self.threads_view_before_search.get_or_insert(self.threads_view);
                self.threads_view = false;
            }
            if record_recent && self.engine.validate_search_query(&self.search_committed, self.active_account.as_deref()).is_ok() {
                let account = self.active_account.clone();
                let query = self.search_committed.clone();
                self.remember_search(account.as_deref(), &query);
                self.refresh_recent_searches();
            }
        }
    }

    /// Esc: after a saved search, restore the query and filters; otherwise clear the search.
    fn cancel_search(&mut self) {
        if self.restore_saved_search() {
            return;
        }
        self.clear_search();
    }

    fn clear_search(&mut self) {
        self.query.clear();
        self.search_committed.clear();
        self.search_limit = 200;
        self.search_debounce_until = None;
        self.search_error = None;
        self.list_cache = None;
        self.threads_cache = None;
        self.list_signature.clear();
        self.index_progress = None;
        self.search_capped = false;
        self.saved_error = None;
        self.saved_restore = None;
        self.saved_folder_scope = false;
        self.applied_saved = None;
        if let Some(previous) = self.threads_view_before_search.take() {
            self.threads_view = previous;
        }
        // A flat search hit is still the open message when the folder returns to conversations:
        // select its thread and load the page so the row highlight and the pane agree. Without
        // this the next J/K jumps to the first conversation and the open hit has no row.
        if self.threads_view && self.reading_open {
            self.select_open_message_thread();
        }
    }

    fn select_open_message_thread(&mut self) {
        let Some(open_id) = self.selected.clone() else { return };
        let Some(thread_id) = self.engine.message(&open_id).map(|message| message.summary.thread_id.clone()) else { return };
        if self.selected_thread.as_deref() == Some(thread_id.as_str()) {
            return;
        }
        let account = self.active_account.clone();
        let summary = match self.op("thread_get", json!({ "id": thread_id, "account": account, "limit": 1, "offset": 0 })) {
            Ok(value) => match serde_json::from_value::<ThreadGetResult>(value) {
                Ok(detail) => detail,
                Err(_) => return,
            },
            Err(_) => return,
        };
        let start = summary.total.saturating_sub(200);
        let page = match self.op("thread_get", json!({ "id": summary.resolved_id, "account": account, "limit": 200, "offset": start })) {
            Ok(value) => match serde_json::from_value::<ThreadGetResult>(value) {
                Ok(detail) => detail,
                Err(_) => return,
            },
            Err(_) => return,
        };
        self.selected_thread = Some(page.resolved_id.clone());
        self.thread_summary = Some(page.thread);
        self.thread_messages = page.messages;
        self.thread_total = page.total;
        self.thread_offset = start.min(page.total);
        self.thread_open = Some(open_id.clone());
        self.select_only(open_id);
        self.threads_cache = None;
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Ctrl+Q is the advertised quit shortcut: it works on every page and in front of every
        // modal, because a person must always be able to leave the application from the keyboard.
        if ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::Q)) {
            self.allow_close = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        if self.composer_preview {
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
                self.composer_preview = false;
                ctx.memory_mut(|memory| memory.stop_text_input());
            }
            return;
        }
        if self.reading.show_unsubscribe {
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
                self.reading.show_unsubscribe = false;
                ctx.memory_mut(|memory| memory.stop_text_input());
            }
            return;
        }
        // Onboarding and account dialogs own their keyboard. The shell must not steal J/K/Enter
        // while a first-run form or a modal is in front of it.
        if self.boot_failure.is_some()
            || self.onboarding.is_some()
            || self.show_accounts_menu
            || self.show_accounts_dialog
            || self.account_remove.is_some()
            || self.show_licences
            || self.reading.pending_link.is_some()
            || self.reading.attachment_warning.is_some()
            || self.reading.attachment_overwrite.is_some()
            || self.reading.attachment_preview.is_some()
            || self.pgp_prompt.is_some()
            || self.composer_preview
            || self.pgp_import.is_some()
            || self.pgp_delete.is_some()
            || self.template_editor.is_some()
            || self.template_delete.is_some()
            || self.identity_editor.is_some()
            || self.identity_delete.is_some()
            || self.reading.show_unsubscribe
            || self.rules_page.is_some()
        {
            return;
        }
        // The saved-search drawer owns Escape and Enter while it is open.
        if self.saved_editor.is_some() {
            let submit = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
            let cancel = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
            if submit {
                self.commit_saved_search_editor();
            }
            if cancel {
                self.cancel_saved_search_editor();
                ctx.memory_mut(|memory| memory.stop_text_input());
            }
            return;
        }
        // The inline label field owns Enter/Escape only while it really has focus. Otherwise a
        // composer field or the global map keeps the keys instead of a hidden editor swallowing
        // them.
        if self.label_editor.as_ref().is_some_and(|editor| editor.focused) {
            let submit = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
            let cancel = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
            if submit {
                self.commit_label_editor();
            }
            if cancel {
                self.cancel_label_editor();
                ctx.memory_mut(|memory| memory.stop_text_input());
            }
            return;
        }
        // A focused colour swatch keeps Enter/Space/arrows for itself. Escape still cancels.
        if self.label_editor.as_ref().is_some_and(|editor| editor.swatch_focused) {
            let cancel = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
            if cancel {
                self.cancel_label_editor();
                ctx.memory_mut(|memory| memory.stop_text_input());
            }
            return;
        }
        // The modal consumes Escape itself; the label menu closes on Escape or its close button.
        if self.label_delete.is_some() {
            return;
        }
        if self.label_menu.is_some() {
            let close = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
            if close {
                self.label_menu = None;
                ctx.memory_mut(|memory| memory.stop_text_input());
            }
            return;
        }
        if self.composer.open {
            let send = ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::Enter));
            let escape = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
            if send {
                self.send_composer();
            }
            if escape && self.composer.open {
                self.close_composer();
            }
            return;
        }
        let mut palette = false;
        let mut escape = false;
        let mut shortcuts = false;
        let mut enter = false;
        let mut open = false;
        let mut next = false;
        let mut previous = false;
        let mut mark_unread = false;
        let mut star = false;
        let mut archive = false;
        let mut delete = false;
        let mut undo = false;
        let mut extend = false;
        let mut toggle = false;
        let mut select_all = false;
        let mut focus_search = false;
        let mut compose = false;
        let mut reply = false;
        let mut reply_all = false;
        let mut forward = false;
        let mut labels = false;
        let mut rename_label = false;
        let mut outbox = false;
        let mut invite_accept = false;
        let mut invite_tentative = false;
        let mut invite_decline = false;
        // Only a real text field may swallow the single-key map. `egui_wants_keyboard_input()`
        // is also true for a focused row or button, and clicking a label left the app deaf to
        // F2/L/J/K/? until Esc.
        let typing = ctx.text_edit_focused();
        // Enter opens a message only from the list (or from nowhere); a focused star or toolbar
        // control keeps Enter for its own activation.
        let focused_widget = ctx.memory(|memory| memory.focused());
        let row_focused = focused_widget.is_none() || focused_widget.is_some_and(|id| self.row_ids.values().any(|row| *row == id));
        ctx.input(|input| {
            palette = input.modifiers.command && input.key_pressed(Key::K);
            escape = input.key_pressed(Key::Escape);
            shortcuts = !typing && !input.modifiers.command && input.key_pressed(Key::Questionmark);
            enter = !typing && !input.modifiers.command && input.key_pressed(Key::Enter) && row_focused;
            open = !typing && !input.modifiers.command && input.key_pressed(Key::O);
            next = !typing && !input.modifiers.command && (input.key_pressed(Key::J) || input.key_pressed(Key::ArrowDown));
            previous = !typing && !input.modifiers.command && (input.key_pressed(Key::K) || input.key_pressed(Key::ArrowUp));
            extend = input.modifiers.shift;
            mark_unread = !typing && !input.modifiers.command && input.key_pressed(Key::U);
            star = !typing && !input.modifiers.command && input.key_pressed(Key::S);
            archive = !typing && !input.modifiers.command && input.key_pressed(Key::E);
            delete = !typing && !input.modifiers.command && input.key_pressed(Key::Delete);
            undo = !typing && !input.modifiers.command && input.key_pressed(Key::Z);
            toggle = !typing && !input.modifiers.command && row_focused && (input.key_pressed(Key::Space) || input.key_pressed(Key::X));
            select_all = !typing && input.modifiers.command && input.key_pressed(Key::A);
            focus_search = !typing && !input.modifiers.command && input.key_pressed(Key::Slash);
            compose = !typing && !input.modifiers.command && input.key_pressed(Key::C);
            reply = !typing && !input.modifiers.command && input.key_pressed(Key::R);
            reply_all = !typing && !input.modifiers.command && input.key_pressed(Key::A);
            forward = !typing && !input.modifiers.command && input.key_pressed(Key::F);
            labels = !typing && !input.modifiers.command && input.key_pressed(Key::L);
            rename_label = !typing && !input.modifiers.command && input.key_pressed(Key::F2);
            outbox = !typing && input.modifiers.command && input.modifiers.shift && input.key_pressed(Key::O);
            invite_accept = !typing && input.modifiers.command && input.modifiers.shift && input.key_pressed(Key::Num1);
            invite_tentative = !typing && input.modifiers.command && input.modifiers.shift && input.key_pressed(Key::Num2);
            invite_decline = !typing && input.modifiers.command && input.modifiers.shift && input.key_pressed(Key::Num3);
        });
        if palette {
            self.show_palette = !self.show_palette;
            self.palette_focus = self.show_palette;
            if self.show_palette {
                self.palette_query.clear();
                self.palette_selected = 0;
                self.show_shortcuts = false;
                self.show_about = false;
            }
        }
        if shortcuts {
            self.show_shortcuts = !self.show_shortcuts;
            self.show_palette = false;
            self.show_about = false;
            if self.show_shortcuts {
                self.shortcut_query.clear();
                self.shortcut_focus = true;
            }
        }
        if escape {
            if self.show_palette {
                self.show_palette = false;
                ctx.memory_mut(|memory| memory.stop_text_input());
            } else if self.show_shortcuts {
                self.show_shortcuts = false;
                ctx.memory_mut(|memory| memory.stop_text_input());
            } else if self.show_about {
                self.show_about = false;
            } else if self.outbox_open {
                self.outbox_open = false;
                self.outbox_confirm = None;
            } else if typing {
                ctx.memory_mut(|memory| memory.stop_text_input());
            } else if self.reading_full && !reading_overlay(ctx.content_rect().width()) {
                self.reading_full = false;
            } else if self.reading_open {
                self.close_reading();
            } else {
                self.clear_selection();
            }
        }
        if self.show_palette || self.show_shortcuts || self.show_about {
            return;
        }
        if rename_label && let Some(id) = self.label_filter.clone() {
            self.start_label_rename(&id);
        }
        if focus_search {
            self.focus_search = true;
        }
        if outbox {
            self.refresh_outbox();
            self.outbox_open = true;
        }
        if self.reading_open && self.invite_ready {
            if invite_accept {
                self.execute_command(CommandId::InviteAccept, ctx);
            } else if invite_tentative {
                self.execute_command(CommandId::InviteTentative, ctx);
            } else if invite_decline {
                self.execute_command(CommandId::InviteDecline, ctx);
            }
        }
        if compose {
            self.compose_new();
        }
        if let Some(id) = self.selected.clone() {
            if reply {
                self.reply_to(&id, ComposeMode::Reply);
            }
            if reply_all {
                self.reply_to(&id, ComposeMode::ReplyAll);
            }
            if forward {
                self.reply_to(&id, ComposeMode::Forward);
            }
        }
        if labels {
            let ids = self.action_ids();
            self.open_label_menu(ids);
        }
        if select_all {
            let (rows, _, _) = self.visible_messages();
            self.select_all(&rows);
        }
        if toggle && let Some(id) = self.selected.clone() {
            self.toggle_selection(id);
        }
        if enter {
            if self.threads_view {
                if self.reading_open && self.selected_thread.is_some() && self.thread_summary.is_some() {
                    if let Some(card) = self.thread_open.clone().or_else(|| self.selected.clone()) {
                        self.open_thread_message(card);
                    }
                } else if let Some(thread) = self.selected_thread.clone() {
                    self.select_thread(thread);
                }
            } else if self.reading_open {
                if !reading_overlay(ctx.content_rect().width()) {
                    self.reading_full = true;
                }
            } else if let Some(id) = self.selected.clone() {
                self.open_message(id);
            }
        }
        if open && let Some(id) = self.selected.clone() {
            self.open_message(id);
        }
        if mark_unread {
            self.mark_unread();
        }
        if star {
            self.toggle_star();
        }
        if archive {
            self.move_selected(FolderKind::Archive, "Messaggio archiviato", "Messaggi archiviati");
        }
        if delete {
            self.move_selected(FolderKind::Trash, "Messaggio spostato nel cestino", "Messaggi spostati nel cestino");
        }
        if undo {
            self.undo_last();
        }
        if next {
            self.step_selection(1, extend);
        }
        if previous {
            self.step_selection(-1, extend);
        }
    }

    fn mark_unread(&mut self) {
        if self.active_thread().is_some() {
            self.set_thread_flag(Some(true), None);
            return;
        }
        let ids = self.action_ids();
        self.set_unread_many(&ids, true);
    }

    fn mark_all_read(&mut self, folder: &str) {
        let starred = self.folder_kind(folder) == Some(FolderKind::Starred);
        let active_folders = self.active_folder_ids();
        let ids = self
            .engine
            .messages()
            .iter()
            .filter(|message| message.unread)
            .filter(|message| active_folders.contains(&message.folder_id))
            .filter(|message| if starred { message.starred } else { message.folder_id == folder })
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        self.set_unread_many(&ids, false);
    }

    fn account(&self) -> Option<&Account> {
        self.active_account()
    }

    fn tick(&mut self, ctx: &egui::Context) {
        self.follow_system_theme(ctx);
        // A screenshot owns a frozen clock: the frame budget, not the wall clock, decides when the
        // capture happens, and every date must render against the same instant twice in a row.
        if self.screenshot.is_none() {
            self.now = ctx.input(|input| input.time);
            self.now_utc = Utc::now();
        }
        self.maybe_complete_read();
        if self.composer.open && self.composer.dirty {
            let elapsed = self.now - self.composer.last_edit;
            if elapsed >= AUTOSAVE_DELAY {
                let _ = self.save_composer(false);
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64((AUTOSAVE_DELAY - elapsed).max(0.0)));
            }
        }
        if let Some(pending) = &self.pending_read {
            ctx.request_repaint_after(Duration::from_secs_f64((pending.at - self.now).max(0.0)));
        }
        let expired = self.toast.as_ref().is_some_and(|toast| self.now >= toast.until);
        if expired {
            self.toast = None;
        } else if let Some(toast) = &self.toast {
            ctx.request_repaint_after(Duration::from_secs_f64((toast.until - self.now).max(0.0)));
        }
        // Debounce a pane drag: the final fraction is written once the user stops moving, and again
        // on exit. Screenshots and tests own no settings file and never write one.
        if self.settings_dirty && self.screenshot.is_none() && self.now - self.settings_saved_at >= SETTINGS_SAVE_DELAY {
            self.persist_settings();
        }
        if let Some(started) = self.loading
            && self.now - started < LOADING_DELAY
        {
            ctx.request_repaint_after(Duration::from_secs_f64(LOADING_DELAY - (self.now - started)));
        }
        if self.sync_worker.as_ref().is_some_and(SyncWorker::is_running) || matches!(self.sync_view, SyncView::Running(_)) {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if self.send_worker.as_ref().is_some_and(SendWorker::is_running) {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        if let Some(at) = self.pending_send {
            let seconds = ((at - self.now_utc.timestamp_millis()).max(0) as f64 / 1000.0).max(0.0);
            ctx.request_repaint_after(Duration::from_secs_f64(seconds.min(1.0)));
        }
        if self.add_account_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        self.handle_sync_events();
        self.poll_daemon(ctx);
        self.poll_daemon_job();
        if self.daemon_job.is_some() || self.connection.remote() && !self.connection.is_live() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn toast_overlay(&mut self, ctx: &egui::Context) {
        // A failure is one toast with a dismiss control and, only for the list, a retry. It replaces
        // the undo toast instead of stacking two messages about the same operation.
        if let Some(failure) = self.failure.clone() {
            let toast = Toast::new(failure.message.as_str()).kind(ToastKind::Error).dismiss();
            let toast = if failure.retry_list { toast.action("Riprova") } else { toast };
            match toast.show_overlay(ctx, "postino-toast") {
                Some(ToastResult::Action) => self.retry_failure(),
                Some(ToastResult::Dismiss) => self.dismiss_failure(),
                _ => {}
            }
            return;
        }
        let Some(state) = &self.toast else {
            return;
        };
        let message = state.message.clone();
        let kind = state.kind;
        let has_undo = state.undo.is_some();
        let toast = Toast::new(&message).kind(kind);
        let toast = if has_undo { toast.undo("Annulla") } else { toast.dismiss() };
        match toast.show_overlay(ctx, "postino-toast") {
            Some(ToastResult::Undo) => self.undo_last(),
            Some(ToastResult::Dismiss) => self.toast = None,
            _ => {}
        }
    }

    fn boot_recovery(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let Some(failure) = self.boot_failure.clone() else {
            return;
        };
        // Enter retries only when no control owns the keyboard; otherwise it must activate the
        // focused button (for example "Apri la modalità demo") like every other surface.
        let enter =
            ui.ctx().memory(|memory| memory.focused()).is_none() && ui.ctx().input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
        if enter {
            self.retry_boot();
            return;
        }
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg)).show(ui, |ui| {
            let viewport = ui.available_rect_before_wrap();
            let width = (viewport.width() - 2.0 * space::HUGE).clamp(360.0, 660.0);
            let card = Rect::from_center_size(viewport.center(), Vec2::new(width, (viewport.height() - 2.0 * space::MD).max(360.0)));
            ui.scope_builder(UiBuilder::new().max_rect(card), |ui| {
                Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XXL)).show(
                    ui,
                    |ui| {
                        ui.set_width((width - 2.0 * f32::from(inset::XXL)).max(300.0));
                        ui.horizontal(|ui| {
                            ui.label(icons::icon(icons::WARNING, 22.0, t.danger));
                            ui.add_space(space::SM);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(typography::text(Role::Caption, "POSTINO · ARCHIVIO", t.text_faint));
                                ui.label(typography::strong(Role::Section, failure.stage.title(), t.text));
                            });
                        });
                        ui.add_space(space::LG);
                        ui.label(typography::text(Role::Body, failure.stage.advice(), t.text_muted));
                        ui.add_space(space::LG);
                        ui.label(typography::text(Role::Meta, format!("Percorso · {}", failure.path_text()), t.text_muted));
                        ui.add_space(space::XS);
                        ui.label(typography::text(Role::Meta, format!("Passaggio · {}", failure.stage.label()), t.text_faint));
                        ui.add_space(space::XS);
                        ui.label(typography::text(
                            Role::Meta,
                            "Il dettaglio tecnico resta nel registro dell'app: questa schermata non mostra messaggi grezzi del sistema.",
                            t.text_faint,
                        ));
                        ui.add_space(space::MD);
                        ui.label(typography::text(
                            Role::Meta,
                            "Il file non viene cancellato: le mail già scaricate restano sul disco e si riaprono quando il portachiavi torna disponibile.",
                            t.text_muted,
                        ));
                        ui.add_space(space::XL);
                        ui.horizontal(|ui| {
                            if Button::primary("Riprova").icon(icons::SYNC).show(ui).clicked() {
                                self.retry_boot();
                            }
                            if Button::new("Apri la modalità demo").icon(icons::MAIL).show(ui).clicked() {
                                self.open_demo();
                            }
                        });
                        ui.add_space(space::SM);
                        ui.label(typography::text(Role::Caption, "Invio riprova · la modalità demo non tocca il file", t.text_faint));
                    },
                );
            });
        });
    }

    fn onboarding_screen(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let dark = self.dark;
        let roots = self.extra_roots.clone();
        let Some(onboarding) = self.onboarding.as_mut() else {
            return;
        };
        let action = egui::CentralPanel::default().frame(Frame::new().fill(t.bg)).show(ui, |ui| onboarding.show(ui, dark, roots));
        if !matches!(action.inner, onboarding::Action::None) {
            self.handle_onboarding_action(action.inner);
        }
    }

    fn resize_handles(&self, ctx: &egui::Context) {
        if !self.custom_chrome {
            return;
        }
        let maximized = ctx.input(|input| input.viewport().maximized.unwrap_or(false));
        if maximized {
            return;
        }
        let Some(pos) = ctx.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        let rect = ctx.content_rect();
        let edge = 5.0;
        let left = pos.x <= rect.left() + edge;
        let right = pos.x >= rect.right() - edge;
        let top = pos.y <= rect.top() + edge;
        let bottom = pos.y >= rect.bottom() - edge;
        let direction = match (left, right, top, bottom) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (_, true, true, _) => Some(ResizeDirection::NorthEast),
            (true, _, _, true) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, ..) => Some(ResizeDirection::West),
            (_, true, ..) => Some(ResizeDirection::East),
            (_, _, true, _) => Some(ResizeDirection::North),
            (_, _, _, true) => Some(ResizeDirection::South),
            _ => None,
        };
        let Some(direction) = direction else {
            return;
        };
        ctx.set_cursor_icon(match direction {
            ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
            ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
            ResizeDirection::West | ResizeDirection::East => CursorIcon::ResizeHorizontal,
            ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        });
        if ctx.input(|input| input.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
        }
    }

    /// The account switcher in the sidebar header. It is a two-line account row, not a message row:
    /// clicking it opens the account menu, and the keyboard reaches it with Tab and Enter.
    fn sidebar_account(&mut self, ui: &mut Ui) {
        let Some(account) = self.active_account().cloned() else {
            return;
        };
        let unread = self.account_unread(&account.id);
        let provider = account.provider.trim();
        // The provider stays in the accessible name and tooltip: the provider suffix would push the
        // account name under the unread badge in the 240 px sidebar.
        let title = account.name.clone();
        let meta = if unread > 0 { unread_text(unread as usize) } else { String::new() };
        let row = ListRow::account(title.as_str(), account.email.as_str(), meta.as_str()).icon(icons::USER).height(40.0);
        let response = row.show(ui);
        let accessible = if provider.is_empty() {
            format!("Account {} — {} · {}. Premi Invio per cambiare account.", title, account.email, account_unread_tooltip(unread))
        } else {
            format!("Account {} ({provider}) — {} · {}. Premi Invio per cambiare account.", title, account.email, account_unread_tooltip(unread))
        };
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, accessible.clone()));
        let response = response.on_hover_text(accessible);
        self.account_switcher_id = Some(response.id);
        if self.focus_account_switcher {
            response.request_focus();
            self.focus_account_switcher = false;
        }
        #[cfg(test)]
        probe::identity(ui, response.rect);
        if response.clicked() || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
            self.show_accounts_menu = true;
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        egui::Panel::left("folders")
            .resizable(false)
            .exact_size(ui_size::SIDEBAR)
            .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(inset::LG, inset::XL)))
            .show_separator_line(false)
            .show(ui, |ui| {
                #[cfg(test)]
                probe::sidebar_panel(ui, ui.max_rect());
                let labels = self.labels_cache.clone();
                let saved_searches = self.saved_searches.clone();
                let editor_open = self.label_editor.is_some() || self.saved_editor.is_some();
                let mut folder_action = None;
                let mut label_action = None;
                let mut saved_action = None;
                let mut editor_action = LabelEditorAction::None;

                // Panels nest in the order they are added. The label drawer is a bottom panel, and
                // the folder/label list scrolls in what remains: opening the editor shrinks the
                // viewport and never moves the list.
                if self.label_editor.is_some() || self.saved_editor.is_some() {
                    let drawer = egui::Panel::bottom("sidebar-editor")
                        .exact_size(ui_size::LABEL_EDITOR_DRAWER)
                        .frame(Frame::new().fill(t.panel_alt).inner_margin(Margin::symmetric(0, inset::MD)))
                        .show_separator_line(false)
                        .show(ui, |ui| {
                            if self.saved_editor.is_some() {
                                editor_action = saved_search_editor_widget(ui, &mut self.saved_editor, &t);
                            } else {
                                let Some(editor) = self.label_editor.as_mut() else {
                                    return;
                                };
                                let editor_scope = ui.scope(|ui| label_editor_widget(ui, editor, &t));
                                #[cfg(test)]
                                probe::editor(ui, editor_scope.response.rect);
                                // The drawer is a pinned surface: the editor's own focus request is
                                // enough, no scroll-to-rect is allowed to move the navigation.
                                editor_action = editor_scope.inner;
                            }
                        });
                    ui.painter().hline(drawer.response.rect.x_range(), drawer.response.rect.top(), Stroke::new(1.0, t.border));
                    #[cfg(test)]
                    probe::editor_drawer(ui, drawer.response.rect);
                }

                #[cfg(test)]
                probe::clear_rows(ui);
                self.sidebar_account(ui);
                ui.add_space(space::XS);
                // A visible bar, local to the sidebar: egui's default is a 2 pt floating sliver
                // that only appears on hover, which at 980×600 hid the overflow instead of
                // revealing it. Text colour on ivory is 10:1, and on the dark panel 12:1.
                ui.spacing_mut().scroll = sidebar_scroll_style();
                let sidebar_offset = self.sidebar_scroll_target.take();
                let mut saved_header_rect: Option<Rect> = None;
                let mut save_header_scroll = false;
                let mut sidebar_scroll = egui::ScrollArea::vertical()
                    .id_salt("sidebar-scroll")
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible);
                if let Some(offset) = sidebar_offset {
                    sidebar_scroll = sidebar_scroll.vertical_scroll_offset(offset);
                }
                let scroll = sidebar_scroll.show(ui, |ui| {
                    // Rows sit flush instead of carrying a two-pixel gap each.
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let header = ui.scope(|ui| {
                        ui.spacing_mut().interact_size.y = 20.0;
                        SectionHeader::new("CARTELLE").show(ui);
                    });
                    #[cfg(test)]
                    probe::sidebar_header(ui, "CARTELLE", header.response.rect);
                    #[cfg(not(test))]
                    let _ = header;
                    ui.add_space(space::XS);
                    let folders = self
                        .engine
                        .folders()
                        .iter()
                        .filter(|folder| self.active_account.as_deref() == Some(folder.account_id.as_str()))
                        .cloned()
                        .collect::<Vec<_>>();
                    for folder in folders {
                        let (total, unread) =
                            if folder.kind == FolderKind::Starred { self.starred_counts(&folder.account_id) } else { (folder.total, folder.unread) };
                        let meta = if folder.kind == FolderKind::Drafts && total > 0 {
                            total.to_string()
                        } else if unread > 0 {
                            unread.to_string()
                        } else {
                            String::new()
                        };
                        let row = ListRow::folder(folder.name.as_str(), meta.as_str())
                            .icon(folder_icon(folder.kind))
                            .selected(if self.saved_folder_scope && !self.saved_query_folder {
                                false
                            } else if self.label_filter.is_some() {
                                self.label_folder.as_deref() == Some(folder.id.as_str())
                            } else {
                                self.folder == folder.id
                            })
                            .height(32.0);
                        let response = row.show(ui);
                        #[cfg(test)]
                        probe::row(ui, &folder.name, response.rect);
                        if response.clicked() {
                            self.select_folder(folder.id.clone());
                        }
                        response.context_menu(|ui| {
                            let items = [
                                menu_item("label_new"),
                                MenuItem::new("Rinomina").enabled(false).reason("La modifica delle cartelle arriva con IMAP"),
                                MenuItem::new("Elimina").enabled(false).reason("La modifica delle cartelle arriva con IMAP"),
                                MenuItem::separator(),
                                menu_item("mark_folder_read"),
                            ];
                            match Menu::new(&items).show(ui) {
                                Some(0) => {
                                    label_action = Some(LabelAction::New);
                                    ui.close();
                                }
                                Some(4) => {
                                    folder_action = Some((folder.id.clone(), FolderAction::MarkAllRead));
                                    ui.close();
                                }
                                _ => {}
                            }
                        });
                    }

                    ui.add_space(space::LG);
                    if ui
                        .scope(|ui| {
                            ui.spacing_mut().interact_size.y = 20.0;
                            SectionHeader::new("ETICHETTE").count(labels.len()).action(icons::PLUS, "Nuova etichetta").show(ui)
                        })
                        .inner
                        .is_some_and(|response| response.clicked())
                    {
                        label_action = Some(LabelAction::New);
                    }
                    ui.add_space(space::XS);
                    for label in &labels {
                        let meta = if label.unread_count > 0 { label.unread_count.to_string() } else { String::new() };
                        let detail = format!(
                            "{} · {} · conteggi su tutte le cartelle",
                            messages_text(label.message_count as usize),
                            unread_text(label.unread_count as usize)
                        );
                        let selected = self.label_filter.as_deref() == Some(label.id.as_str());
                        let row = ListRow::folder(&label.name, &meta)
                            .subtitle(&detail)
                            .dot(label_dot(label.color, &t, selected))
                            .selected(selected)
                            .height(32.0);
                        let id = label.id.clone();
                        let response = row.show(ui);
                        #[cfg(test)]
                        probe::row(ui, &label.name, response.rect);
                        if response.clicked() || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
                            self.select_label(id.clone());
                        }
                        response.context_menu(|ui| {
                            let items = [menu_item("label_rename"), menu_item("label_delete").danger(true)];
                            match Menu::new(&items).show(ui) {
                                Some(0) => {
                                    label_action = Some(LabelAction::Rename(id.clone()));
                                    ui.close();
                                }
                                Some(1) => {
                                    label_action = Some(LabelAction::Delete(id.clone()));
                                    ui.close();
                                }
                                _ => {}
                            }
                        });
                    }
                    if labels.is_empty() && !editor_open {
                        ui.label(typography::text(Role::Caption, "Nessuna etichetta. Usa + per crearne una.", t.text_faint));
                    }

                    ui.add_space(space::LG);
                    let can_save = !self.search_committed.trim().is_empty() && saved_searches.len() < 12;
                    let save_reason = if self.search_committed.trim().is_empty() {
                        "Scrivi una ricerca prima di salvarla"
                    } else {
                        "Hai già 12 ricerche salvate: eliminane una prima di aggiungerne un'altra"
                    };
                    let search_header = ui.scope(|ui| {
                        ui.spacing_mut().interact_size.y = 20.0;
                        SectionHeader::new("RICERCHE")
                            .count(saved_searches.len())
                            .action(icons::PLUS, "Salva ricerca")
                            .action_enabled(can_save)
                            .action_reason(save_reason)
                            .show(ui)
                    });
                    saved_header_rect = Some(search_header.response.rect);
                    if self.scroll_saved_into_view {
                        self.scroll_saved_into_view = false;
                        save_header_scroll = true;
                    }
                    if search_header.inner.is_some_and(|response| response.clicked()) {
                        saved_action = Some(SavedAction::Save);
                    }
                    ui.add_space(space::XS);
                    for saved in &saved_searches {
                        let meta = if !saved.valid {
                            String::new()
                        } else if saved.unread > 0 {
                            saved.unread.to_string()
                        } else {
                            saved.total.to_string()
                        };
                        let detail = if saved.index_pending {
                            "Indice di ricerca in preparazione".to_owned()
                        } else if saved.valid {
                            format!("{} · {} non letti", messages_text(saved.total as usize), unread_text(saved.unread as usize))
                        } else {
                            "Ricerca non valida: correggila o eliminala".to_owned()
                        };
                        let selected = self.applied_saved.as_deref() == Some(saved.id.as_str());
                        let dot = if saved.index_pending {
                            t.text_faint
                        } else if saved.valid {
                            t.gold_ink
                        } else {
                            t.danger
                        };
                        let row = ListRow::folder(&saved.name, &meta).subtitle(&detail).dot(dot).selected(selected).height(32.0);
                        let id = saved.id.clone();
                        let response = row.show(ui);
                        #[cfg(test)]
                        probe::row(ui, &format!("saved:{}", saved.name), response.rect);
                        if response.clicked() || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
                            saved_action = Some(SavedAction::Apply(id.clone()));
                        }
                        if response.hovered() || response.has_focus() {
                            response.clone().on_hover_text(if saved.index_pending {
                                format!("{}\nIndice di ricerca in preparazione", saved.name)
                            } else if saved.valid {
                                format!("{}\n{}\n{}", saved.name, saved.query, detail)
                            } else {
                                format!("{}\nRicerca non valida: correggila o eliminala", saved.name)
                            });
                        }
                        response.context_menu(|ui| {
                            let items = [menu_item("saved_search_rename"), menu_item("saved_search_delete").danger(true)];
                            match Menu::new(&items).show(ui) {
                                Some(0) => {
                                    saved_action = Some(SavedAction::Rename(id.clone()));
                                    ui.close();
                                }
                                Some(1) => {
                                    saved_action = Some(SavedAction::Delete(id.clone()));
                                    ui.close();
                                }
                                _ => {}
                            }
                        });
                    }
                    if saved_searches.is_empty() && !editor_open {
                        ui.label(typography::text(Role::Caption, "Nessuna ricerca salvata. Cerca qualcosa e usa +.", t.text_faint));
                    }
                });
                if save_header_scroll && let Some(header) = saved_header_rect {
                    let content_top = scroll.inner_rect.top() - scroll.state.offset.y;
                    self.sidebar_scroll_target = Some((header.top() - content_top - 4.0).max(0.0));
                    ui.ctx().request_repaint();
                }
                #[cfg(test)]
                probe::scroll(ui, scroll.inner_rect, scroll.content_size, scroll.state.offset);
                #[cfg(not(test))]
                let _ = scroll;

                if let Some((folder, FolderAction::MarkAllRead)) = folder_action {
                    self.mark_all_read(&folder);
                }
                match label_action {
                    // `+` while editing only returns focus to the field; the draft stays.
                    Some(LabelAction::New) => self.start_label_create_or_focus(),
                    Some(LabelAction::Rename(id)) => self.start_label_rename(&id),
                    Some(LabelAction::Delete(id)) => self.request_label_delete(&id),
                    None => {}
                }
                match editor_action {
                    LabelEditorAction::Save => {
                        if self.saved_editor.is_some() {
                            self.commit_saved_search_editor();
                        } else {
                            self.commit_label_editor();
                        }
                    }
                    LabelEditorAction::Cancel => {
                        if self.saved_editor.is_some() {
                            self.cancel_saved_search_editor();
                        } else {
                            self.cancel_label_editor();
                        }
                    }
                    LabelEditorAction::None => {}
                }
                match saved_action {
                    Some(SavedAction::Save) => self.start_saved_search_save(),
                    Some(SavedAction::Rename(id)) => self.start_saved_search_rename(&id),
                    Some(SavedAction::Delete(id)) => self.delete_saved_search(&id),
                    Some(SavedAction::Apply(id)) => self.apply_saved_search(&id),
                    None => {}
                }
            });
    }

    fn offline_banner(&mut self, ui: &mut Ui, t: &Tokens) {
        Frame::new()
            .fill(t.panel_alt)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::MD))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icons::WARNING, 16.0, t.gold_ink));
                    ui.add_space(space::SM);
                    let detail = match self.sync_view {
                        SyncView::Offline { last_sync: Some(ms) } => {
                            format!("Sei offline · i messaggi salvati restano disponibili · ultimo sync {}", format_millis(ms, self.now_utc))
                        }
                        _ => "Sei offline · i messaggi salvati restano disponibili".to_owned(),
                    };
                    ui.add(egui::Label::new(typography::text(Role::Meta, detail, t.text_muted)).truncate());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let retry = Button::ghost("Riprova").size(ButtonSize::Sm).show(ui);
                        #[cfg(test)]
                        probe::retry(ui, retry.rect);
                        if retry.clicked() {
                            self.request_sync();
                        }
                    });
                });
            });
    }

    /// Export the selected message through `message_export` and write the exact returned bytes.
    /// The native Save dialog owns overwrite confirmation; a cancelled dialog writes nothing.
    fn export_selected_eml(&mut self) {
        if self.demo || self.store.is_none() {
            self.push_info_toast("La modalità demo non esporta .eml: serve un archivio locale.", ToastKind::Error);
            return;
        }
        let Some(id) = self.selected.clone() else {
            self.status = "Seleziona prima un messaggio".to_owned();
            return;
        };
        let value = match self.op("message_export", json!({ "id": id, "account": self.active_account })) {
            Ok(value) => value,
            Err(_) => {
                self.push_info_toast("Non è stato possibile preparare l'esportazione del messaggio.", ToastKind::Error);
                return;
            }
        };
        let exported: MessageExport = match serde_json::from_value(value) {
            Ok(exported) => exported,
            Err(_) => {
                self.push_info_toast("La risposta dell'archivio per l'esportazione non è leggibile.", ToastKind::Error);
                return;
            }
        };
        let bytes = match base64::engine::general_purpose::STANDARD.decode(exported.eml_base64.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.push_info_toast("I byte .eml non sono decodificabili.", ToastKind::Error);
                return;
            }
        };
        let suggested = reading::safe_filename(&exported.filename);
        match self.file_saver.choose_path(&suggested) {
            Ok(Some(path)) => {
                if path.is_dir() {
                    self.push_info_toast("La destinazione scelta è una cartella, non un file.", ToastKind::Error);
                    return;
                }
                match self.file_saver.write(&path, &bytes) {
                    Ok(()) => self.push_info_toast(
                        if exported.exact {
                            "Messaggio esportato con i byte originali.".to_owned()
                        } else {
                            "Messaggio esportato e ricostruito dai dati salvati.".to_owned()
                        },
                        ToastKind::Success,
                    ),
                    Err(error) => self.push_info_toast(format!("Non è stato possibile scrivere il file .eml: {error}"), ToastKind::Error),
                }
            }
            Ok(None) => self.status = "Esportazione annullata".to_owned(),
            Err(error) => self.push_info_toast(format!("Non è stato possibile aprire la finestra di salvataggio: {error}"), ToastKind::Error),
        }
    }

    /// Import one chosen `.eml` through `message_import`, then reload the mailbox and open the copy.
    /// The engine bounds and sanitizes the file exactly as a server fetch would.
    fn import_eml(&mut self) {
        if self.demo || self.store.is_none() {
            self.push_info_toast("La modalità demo non importa .eml: serve un archivio locale.", ToastKind::Error);
            return;
        }
        let path = match self.file_picker.choose_files() {
            Ok(paths) if paths.is_empty() => return,
            Ok(paths) if paths.len() == 1 => paths[0].clone(),
            Ok(_) => {
                self.push_info_toast("Scegli un solo file .eml per volta.", ToastKind::Info);
                return;
            }
            Err(error) => {
                self.push_info_toast(format!("Non è stato possibile aprire la scelta dei file: {error}"), ToastKind::Error);
                return;
            }
        };
        let size = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.push_info_toast(format!("Non è stato possibile leggere il file scelto: {error}"), ToastKind::Error);
                return;
            }
        };
        if size > MAX_EML_BYTES {
            self.push_info_toast(format!("Il file .eml supera il limite di {} MiB.", MAX_EML_BYTES / (1024 * 1024)), ToastKind::Error);
            return;
        }
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                self.push_info_toast(format!("Non è stato possibile leggere il file scelto: {error}"), ToastKind::Error);
                return;
            }
        };
        let mut raw = Vec::new();
        if let Err(error) = file.take(MAX_EML_BYTES.saturating_add(1)).read_to_end(&mut raw) {
            self.push_info_toast(format!("Non è stato possibile leggere il file scelto: {error}"), ToastKind::Error);
            return;
        }
        if raw.len() as u64 > MAX_EML_BYTES {
            self.push_info_toast(format!("Il file .eml supera il limite di {} MiB.", MAX_EML_BYTES / (1024 * 1024)), ToastKind::Error);
            return;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let value = match self.op("message_import", json!({ "account": self.active_account, "eml_base64": encoded })) {
            Ok(value) => value,
            Err(_) => {
                self.push_info_toast("Il file non è un messaggio RFC 5322 importabile.", ToastKind::Error);
                return;
            }
        };
        let imported: MessageImport = match serde_json::from_value(value) {
            Ok(imported) => imported,
            Err(_) => {
                self.push_info_toast("La risposta dell'archivio per l'importazione non è leggibile.", ToastKind::Error);
                return;
            }
        };
        let folder = imported.message.summary.folder_id.clone();
        let id = imported.message.summary.id.clone();
        self.reload_engine_after_sync(true);
        self.select_folder(folder);
        self.open_message(id);
        if imported.warnings.is_empty() {
            self.push_info_toast("Messaggio importato nella cartella Import.", ToastKind::Success);
        } else {
            self.push_info_toast(
                format!("Messaggio importato con {} avvisi MIME: il contenuto sospeso è stato neutralizzato.", imported.warnings.len()),
                ToastKind::Info,
            );
        }
    }

    fn open_rules(&mut self, new: bool) {
        let Some(account) = self.active_account.clone().or_else(|| self.engine.accounts().first().map(|account| account.id.clone())) else {
            self.status = "Aggiungi un account prima di creare regole".to_owned();
            return;
        };
        let mut page = RulesPage::new(&mut self.engine, &account);
        if new {
            page.start_new(&self.engine);
        }
        self.rules_page = Some(page);
    }

    fn open_settings(&mut self, ctx: &egui::Context, opener: Option<Id>) {
        let return_focus = opener.or(self.account_switcher_id).or_else(|| ctx.memory(|memory| memory.focused()));
        self.settings_return_focus = return_focus;
        self.settings_focus_pending = true;
        self.show_accounts_menu = false;
        self.show_accounts_dialog = true;
        self.show_licences = false;
        // Every open re-reads the rows: a tray or autostart change made by the CLI in the meantime
        // must never be answered from the previous visit.
        self.background_rows = None;
        self.background_change = None;
    }

    fn close_settings(&mut self, ctx: &egui::Context) {
        self.show_accounts_dialog = false;
        self.show_licences = false;
        // Closing Settings discards an unsaved signature editor; reopening must not show stale text.
        self.signature_editor = None;
        self.signature_focus_account = None;
        // A dead icon or a daemon restart is invisible while the page is closed; the next open must
        // read it again instead of serving the last bool.
        self.background_rows = None;
        self.background_change = None;
        self.focus_account_switcher = true;
        if let Some(id) = self.settings_return_focus.take() {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    fn open_licences(&mut self, ctx: &egui::Context) {
        self.licences_return_focus = ctx.memory(|memory| memory.focused());
        self.licences_focus_pending = true;
        self.show_licences = true;
    }

    /// Load the local compose templates once. A failure leaves the composer without a template menu,
    /// never with a dead entry; the CLI and MCP can still create one.
    fn refresh_templates(&mut self) {
        if self.templates_loaded {
            return;
        }
        self.reload_templates();
    }

    fn reload_templates(&mut self) {
        self.templates_loaded = true;
        match self.op("templates_list", json!({})) {
            Ok(value) => {
                if let Ok(templates) = serde_json::from_value::<Vec<MessageTemplate>>(value) {
                    self.templates = templates;
                }
            }
            Err(_) => self.templates.clear(),
        }
    }

    fn open_template_editor(&mut self, template: Option<&MessageTemplate>) {
        self.template_editor = Some(match template {
            Some(template) => TemplateEditorState {
                id: Some(template.id.clone()),
                name: template.name.clone(),
                subject: template.subject.clone(),
                body: template.body.clone(),
                focus_name: true,
                error: None,
            },
            None => TemplateEditorState { id: None, name: String::new(), subject: String::new(), body: String::new(), focus_name: true, error: None },
        });
    }

    fn submit_template_editor(&mut self) {
        let Some(editor) = self.template_editor.clone() else { return };
        let result = match editor.id.as_deref() {
            Some(id) => self.op("template_update", json!({ "id": id, "name": editor.name, "subject": editor.subject, "body": editor.body })),
            None => self.op("template_create", json!({ "name": editor.name, "subject": editor.subject, "body": editor.body })),
        };
        match result {
            Ok(_) => {
                let name = editor.name.trim().to_owned();
                self.template_editor = None;
                self.reload_templates();
                self.push_info_toast(format!("Modello salvato: {name}"), ToastKind::Info);
            }
            Err(error) => {
                if let Some(editor) = self.template_editor.as_mut() {
                    editor.error = Some(format!("Il modello non è stato salvato: {error}"));
                }
            }
        }
    }

    fn delete_template(&mut self, template: &MessageTemplate) {
        match self.op("template_delete", json!({ "id": template.id })) {
            Ok(_) => {
                self.reload_templates();
                self.push_info_toast(format!("Modello eliminato: {}", template.name), ToastKind::Info);
            }
            Err(error) => self.push_info_toast(format!("Il modello non è stato eliminato: {error}"), ToastKind::Error),
        }
    }

    fn refresh_identities(&mut self) {
        if self.identities_loaded || self.demo || self.now < self.identities_retry_at {
            return;
        }
        self.reload_identities();
    }

    fn reload_identities(&mut self) {
        if self.demo {
            self.identities.clear();
            self.identities_loaded = true;
            return;
        }
        let mut args = json!({});
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        match self.op("identities_list", args) {
            Ok(value) => match serde_json::from_value::<Vec<Identity>>(value) {
                Ok(identities) => {
                    self.identities = identities;
                    self.identities_loaded = true;
                    self.identities_retry_at = 0.0;
                    self.identities_notice = None;
                }
                Err(_) => {
                    self.identities_retry_at = self.now + 5.0;
                    self.identities_notice = Some("L'elenco dei mittenti non è leggibile.".to_owned());
                }
            },
            Err(error) => {
                self.identities_retry_at = self.now + 5.0;
                self.identities_notice = Some(format!("Mittenti non disponibili: {error}"));
            }
        }
    }

    fn open_identity_editor(&mut self, identity: Option<&Identity>) {
        self.identity_editor = Some(match identity {
            Some(identity) => IdentityEditorState {
                id: Some(identity.id.clone()),
                name: identity.name.clone(),
                email: identity.email.clone(),
                signature: identity.signature.clone(),
                focus_name: true,
                error: None,
            },
            None => {
                IdentityEditorState { id: None, name: String::new(), email: String::new(), signature: String::new(), focus_name: true, error: None }
            }
        });
    }

    fn submit_identity_editor(&mut self) {
        let Some(editor) = self.identity_editor.clone() else { return };
        let mut args = match editor.id.as_deref() {
            Some(id) => json!({ "id": id, "name": editor.name, "email": editor.email, "signature": editor.signature }),
            None => json!({ "name": editor.name, "email": editor.email, "signature": editor.signature }),
        };
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        let result = match editor.id.as_deref() {
            Some(_) => self.op("identity_update", args),
            None => self.op("identity_create", args),
        };
        match result {
            Ok(_) => {
                let name = editor.name.trim().to_owned();
                self.identity_editor = None;
                self.reload_identities();
                self.push_info_toast(format!("Mittente salvato: {name}"), ToastKind::Info);
            }
            Err(error) => {
                if let Some(editor) = self.identity_editor.as_mut() {
                    editor.error = Some(format!("Il mittente non è stato salvato: {error}"));
                }
            }
        }
    }

    fn delete_identity(&mut self, identity: &Identity) {
        let mut args = json!({ "id": identity.id });
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        match self.op("identity_delete", args) {
            Ok(_) => {
                self.identity_delete = None;
                self.reload_identities();
                self.push_info_toast(format!("Mittente eliminato: {}", identity.name), ToastKind::Info);
            }
            Err(error) => self.push_info_toast(format!("Il mittente non è stato eliminato: {error}"), ToastKind::Error),
        }
    }

    /// Read the account's key rows once. A failure becomes a sentence in Settings; it never hides
    /// the existing rows or claims the account has no keys.
    /// Keys, verdicts and the passphrase sheet belong to one account. Switching accounts must not
    /// show the previous account's keys or keep a prompt that would sign with the wrong identity.
    fn reset_pgp_account_state(&mut self) {
        self.pgp_keys.clear();
        self.pgp_keys_loaded = false;
        self.pgp_keys_retry_at = 0.0;
        self.reading.pgp_reading = None;
        self.pgp_checked = None;
        self.pgp_prompt = None;
        self.pgp_import = None;
        self.pgp_delete = None;
        self.reading.unsubscribe_info = None;
        self.reading.unsubscribe_checked = None;
        self.reading.show_unsubscribe = false;
        self.identities.clear();
        self.identities_loaded = false;
        self.identities_retry_at = 0.0;
        self.identity_editor = None;
        self.identity_delete = None;
        self.identities_notice = None;
    }

    fn refresh_pgp_keys(&mut self) {
        if self.pgp_keys_loaded || self.now < self.pgp_keys_retry_at {
            return;
        }
        let mut args = json!({});
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        match self.op("pgp_keys_list", args) {
            Ok(value) => match serde_json::from_value::<Vec<PgpKey>>(value) {
                Ok(keys) => {
                    self.pgp_keys = keys;
                    self.pgp_keys_loaded = true;
                    self.pgp_keys_retry_at = 0.0;
                    self.pgp_notice = None;
                }
                Err(_) => {
                    self.pgp_keys_retry_at = self.now + 5.0;
                    self.pgp_notice = Some("L'elenco delle chiavi non è leggibile.".to_owned());
                }
            },
            Err(error) => {
                self.pgp_keys_retry_at = self.now + 5.0;
                self.pgp_notice = Some(format!("Chiavi PGP non disponibili: {error}"));
            }
        }
    }

    fn open_pgp_import(&mut self) {
        self.pgp_import = Some(PgpImportState {
            armor: Zeroizing::new(String::new()),
            passphrase: Zeroizing::new(String::new()),
            focus_armor: true,
            error: None,
            busy: false,
        });
    }

    fn submit_pgp_import(&mut self, mut state: PgpImportState) {
        let mut args = json!({ "armored_key": state.armor.as_str() });
        if !state.passphrase.is_empty() {
            args["passphrase"] = json!(state.passphrase.as_str());
        }
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        match self.op("pgp_key_import", args) {
            Ok(_) => {
                self.pgp_import = None;
                self.pgp_keys_loaded = false;
                self.pgp_keys_retry_at = 0.0;
                self.pgp_checked = None;
                self.reading.pgp_reading = None;
                self.refresh_pgp_keys();
                self.pgp_notice = Some("Chiave PGP importata in questo archivio cifrato.".to_owned());
            }
            Err(error) => {
                state.busy = false;
                state.error = Some(format!("Import non riuscito: {error}"));
                self.pgp_import = Some(state);
            }
        }
    }

    /// Export is public-only: the button copies the armor, never the secret, and the operation has
    /// no secret path at all.
    fn export_pgp_key(&mut self, id: &str, ctx: &egui::Context) {
        let mut args = json!({ "id": id });
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        match self.op("pgp_key_export_public", args) {
            Ok(value) => match serde_json::from_value::<PgpPublicKey>(value) {
                Ok(key) => {
                    ctx.copy_text(key.public_armor);
                    self.pgp_notice = Some("Chiave pubblica copiata negli appunti.".to_owned());
                }
                Err(_) => self.pgp_notice = Some("La chiave pubblica esportata non è leggibile.".to_owned()),
            },
            Err(error) => self.pgp_notice = Some(format!("Esportazione non riuscita: {error}")),
        }
    }

    fn delete_pgp_key(&mut self, id: &str) {
        let mut args = json!({ "id": id });
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        match self.op("pgp_key_delete", args) {
            Ok(_) => {
                self.pgp_keys_loaded = false;
                self.pgp_keys_retry_at = 0.0;
                self.pgp_checked = None;
                self.reading.pgp_reading = None;
                self.refresh_pgp_keys();
                self.pgp_notice = Some("Chiave PGP eliminata.".to_owned());
            }
            Err(error) => self.pgp_notice = Some(format!("Eliminazione non riuscita: {error}")),
        }
    }

    fn command_enabled(&self, command: &Command) -> bool {
        match command.rule {
            CommandRule::Always => true,
            CommandRule::Message => self.selected.is_some(),
            CommandRule::ActiveLabel => self.label_filter.is_some(),
            CommandRule::LabelFilter => self.label_filter.is_some(),
            CommandRule::Sync => self.sync_ready(),
            CommandRule::Store => self.can_add_account(),
            CommandRule::Invite => self.reading_open && self.selected.is_some() && self.invite_ready,
            CommandRule::StoredMessage => self.selected.is_some() && !self.demo && self.store.is_some(),
        }
    }

    fn execute_command(&mut self, id: CommandId, ctx: &egui::Context) {
        if guarded(|| self.execute_command_inner(id, ctx)).is_none() {
            let label = command_label(id.as_str(), "comando");
            self.push_info_toast(format!("«{label}» non è andata a buon fine. Postino resta aperto."), ToastKind::Error);
        }
    }

    fn execute_command_inner(&mut self, id: CommandId, ctx: &egui::Context) {
        self.show_palette = false;
        ctx.memory_mut(|memory| memory.stop_text_input());
        match id {
            CommandId::Compose => self.compose_new(),
            CommandId::Reply => {
                if let Some(message) = self.selected.clone() {
                    self.reply_to(&message, ComposeMode::Reply);
                }
            }
            CommandId::ReplyAll => {
                if let Some(message) = self.selected.clone() {
                    self.reply_to(&message, ComposeMode::ReplyAll);
                }
            }
            CommandId::InviteAccept => self.reply_to_invite(InviteResponse::Accept),
            CommandId::InviteTentative => self.reply_to_invite(InviteResponse::Tentative),
            CommandId::InviteDecline => self.reply_to_invite(InviteResponse::Decline),
            CommandId::Forward => {
                if let Some(message) = self.selected.clone() {
                    self.reply_to(&message, ComposeMode::Forward);
                }
            }
            CommandId::EditDraft => {
                if let Some(message) = self.selected.clone() {
                    self.edit_draft(&message);
                }
            }
            CommandId::Send => self.send_composer(),
            CommandId::Outbox => {
                self.open_outbox();
            }
            CommandId::ExportEml => self.export_selected_eml(),
            CommandId::ImportEml => self.import_eml(),
            CommandId::Open => {
                if self.threads_view {
                    if self.reading_open && self.selected_thread.is_some() && self.thread_summary.is_some() {
                        if let Some(card) = self.thread_open.clone().or_else(|| self.selected.clone()) {
                            self.open_thread_message(card);
                        }
                    } else if let Some(thread) = self.selected_thread.clone() {
                        self.select_thread(thread);
                    }
                } else if let Some(message) = self.selected.clone() {
                    self.open_message(message);
                }
            }
            CommandId::Expand => {
                if self.threads_view {
                    if self.reading_open && self.selected_thread.is_some() && self.thread_summary.is_some() {
                        self.reading_full = true;
                    } else if let Some(thread) = self.selected_thread.clone() {
                        self.select_thread(thread);
                        self.reading_full = true;
                    }
                } else if let Some(message) = self.selected.clone() {
                    self.open_message(message);
                    self.reading_full = true;
                }
            }
            CommandId::Read => {
                if let Some(message) = self.selected.clone() {
                    self.set_unread_for(&message, false);
                }
            }
            CommandId::Unread => self.mark_unread(),
            CommandId::Star => self.set_starred(true),
            CommandId::Unstar => self.set_starred(false),
            CommandId::Archive => self.move_selected(FolderKind::Archive, "Messaggio archiviato", "Messaggi archiviati"),
            CommandId::Delete => self.move_selected(FolderKind::Trash, "Messaggio spostato nel cestino", "Messaggi spostati nel cestino"),
            CommandId::LabelMenu => {
                let ids = self.action_ids();
                self.open_label_menu(ids);
            }
            CommandId::LabelNew => self.start_label_create(),
            CommandId::LabelRename => {
                if let Some(id) = self.label_filter.clone() {
                    self.start_label_rename(&id);
                }
            }
            CommandId::LabelDelete => {
                if let Some(id) = self.label_filter.clone() {
                    self.request_label_delete(&id);
                }
            }
            CommandId::LabelClearFilter => {
                if self.label_filter.is_some() {
                    self.clear_label_filter();
                } else {
                    self.status = "Nessun filtro etichetta attivo".to_owned();
                }
            }
            CommandId::Inbox => {
                if let Some(folder) = self.folder_for_kind(FolderKind::Inbox) {
                    self.select_folder(folder);
                } else {
                    self.status = "Il server non ha una cartella Posta in arrivo".to_owned();
                }
            }
            CommandId::Starred => {
                if let Some(folder) = self.folder_for_kind(FolderKind::Starred) {
                    self.select_folder(folder);
                } else {
                    self.status = "Il server non espone una cartella Preferiti".to_owned();
                }
            }
            CommandId::Theme => {
                let dark = !self.dark;
                self.set_dark(ctx, dark);
            }
            CommandId::ReducedMotion => {
                let reduced = !self.reduced_motion;
                self.set_reduced_motion(ctx, reduced);
                self.status = if reduced { "Animazioni ridotte" } else { "Animazioni attive" }.to_owned();
            }
            CommandId::About => self.show_about = true,
            CommandId::Shortcuts => {
                self.show_shortcuts = true;
                self.shortcut_query.clear();
                self.shortcut_focus = true;
            }
            CommandId::Palette => self.show_palette = true,
            CommandId::SelectAll => {
                let (rows, _, _) = self.visible_messages();
                self.select_all(&rows);
            }
            CommandId::Toggle => {
                if let Some(message) = self.selected.clone() {
                    self.toggle_selection(message);
                }
            }
            CommandId::ClearSelection => self.clear_selection(),
            CommandId::FocusSearch => self.focus_search = true,
            CommandId::ThreadsToggle => self.toggle_threads_view(),
            CommandId::SavedSearchSave => self.start_saved_search_save(),
            CommandId::SavedSearchRename => {
                if let Some(id) = self.applied_saved.clone() {
                    self.start_saved_search_rename(&id);
                } else {
                    self.status = "Seleziona prima una ricerca salvata".to_owned();
                }
            }
            CommandId::SavedSearchDelete => {
                if let Some(id) = self.applied_saved.clone() {
                    self.delete_saved_search(&id);
                } else {
                    self.status = "Seleziona prima una ricerca salvata".to_owned();
                }
            }
            CommandId::SavedSearchClearRecent => {
                let account = self.active_account.clone();
                match self.clear_recent_searches(account.as_deref()) {
                    Ok(true) => {
                        self.refresh_recent_searches();
                        self.status = "Ricerche recenti cancellate".to_owned();
                    }
                    Ok(false) => self.status = "Nessuna ricerca recente da cancellare".to_owned(),
                    Err(_) => self.report_failure("Non è stato possibile cancellare le ricerche recenti.", false),
                }
            }
            CommandId::Rules => self.open_rules(false),
            CommandId::RulesNew => self.open_rules(true),
            CommandId::Next => self.step_selection(1, false),
            CommandId::Previous => self.step_selection(-1, false),
            CommandId::ExtendNext => self.step_selection(1, true),
            CommandId::ExtendPrevious => self.step_selection(-1, true),
            CommandId::MarkFolderRead => {
                let folder = self.folder.clone();
                self.mark_all_read(&folder);
            }
            CommandId::Back => self.reading_full = false,
            CommandId::Close => {
                if self.reading_full {
                    self.reading_full = false;
                } else {
                    self.close_reading();
                }
            }
            CommandId::Undo => self.undo_last(),
            CommandId::Sync => self.request_sync(),
            CommandId::DaemonStart => self.request_start_daemon(),
            CommandId::DaemonStop => self.request_stop_daemon(),
            CommandId::Quit => {
                // Explicit exit: the close handler must not turn this into a hide.
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            CommandId::DaemonStatus => self.status = self.daemon_status_text(),
            CommandId::Settings => self.open_settings(ctx, self.account_switcher_id),
            CommandId::AddAccount => {
                if self.can_add_account() {
                    self.onboarding = Some(Onboarding::add_account());
                } else {
                    self.status = "La modalità demo non ha un archivio locale".to_owned();
                }
            }
        }
    }
}

impl PostinoApp {
    /// One frame of the whole shell. Public so a headless test can render it without a window.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        // The root UI can be wider than the window at fractional display scaling. Pin the whole
        // shell to the real content rect so right-aligned controls and the status bar stay inside.
        self.handle_close_request(ui.ctx());
        let content = ui.ctx().content_rect();
        ui.scope_builder(UiBuilder::new().max_rect(content), |ui| {
            self.show_inner(ui);
            self.close_notice(ui);
        });
    }

    fn show_inner(&mut self, ui: &mut egui::Ui) {
        #[cfg(test)]
        {
            probe::clear_identity(ui);
            probe::clear_retries(ui);
            probe::clear_dialog_buttons(ui);
            probe::clear_thread_stars(ui);
        }
        self.poll_add_account();
        self.handle_sync_events();
        self.handle_send_events();
        // The desktop app is the only scheduler this build has, and a row can be created by the CLI,
        // by MCP or by another instance. Poll the store every two seconds so a row queued after boot
        // is seen when it becomes due, and reload the mailbox when another process delivered a row.
        if !self.outbox_loaded || self.now - self.last_outbox_poll >= OUTBOX_POLL_SECONDS {
            self.outbox_loaded = true;
            self.last_outbox_poll = self.now;
            self.poll_outbox();
        } else {
            self.pump_outbox();
        }
        let composer_was_open = self.composer.open;
        self.capture_screenshot(ui.ctx());
        self.tick(ui.ctx());
        self.poll_search_index(ui.ctx());
        self.refresh_saved_searches_if_stale();
        self.resize_handles(ui.ctx());
        self.handle_shortcuts(ui.ctx());
        if self.boot_failure.is_some() {
            self.status_bar(ui);
            if self.custom_chrome {
                self.title_bar(ui);
            }
            self.boot_recovery(ui);
            self.toast_overlay(ui.ctx());
            return;
        }
        if self.onboarding.is_some() {
            self.status_bar(ui);
            if self.custom_chrome {
                self.title_bar(ui);
            }
            self.onboarding_screen(ui);
            self.toast_overlay(ui.ctx());
            return;
        }
        // egui 0.36 nests panels in the order they are added. The status bar goes first so it is
        // the outermost panel and spans the full width; the title bar and toolbar sit above the
        // three columns.
        self.status_bar(ui);
        if self.custom_chrome {
            self.title_bar(ui);
        }
        self.toolbar(ui);
        self.loading_line(ui);
        self.sidebar(ui);
        // Below 1000 px the side pane cannot keep a usable list, so reading reuses the full path
        // as an overlay. At or above it the pane opens at 52% of the width left by the sidebar,
        // resizes between 420 and 62%, and is capped so the list keeps its 460 pt floor.
        let overlay = reading_overlay(ui.ctx().content_rect().width());
        let full = self.reading_open && (self.reading_full || overlay);
        if full {
            self.reading_pane(ui, true);
        } else {
            self.reading_pane(ui, false);
            self.list_panel(ui);
        }
        self.about(ui);
        self.command_palette(ui);
        self.shortcuts_sheet(ui);
        self.label_menu_window(ui);
        self.label_delete_dialog(ui);
        self.account_menu(ui);
        self.accounts_dialog(ui);
        if let Some(mut page) = self.rules_page.take() {
            if page.show(ui, &mut self.engine) {
                // Rules can move mail and apply labels. The sidebar reads `labels_cache`, so the
                // counts must be refreshed when the page closes, not only after a mail action.
                self.refresh_labels();
                self.threads_cache = None;
                self.list_cache = None;
            } else {
                self.rules_page = Some(page);
            }
        }
        self.outbox_dialog(ui);
        self.outbox_confirm_dialog(ui);
        self.account_remove_dialog(ui);
        self.pgp_import_dialog(ui);
        self.pgp_delete_dialog(ui);
        self.template_editor_dialog(ui);
        self.template_delete_dialog(ui);
        self.identity_editor_dialog(ui);
        self.identity_delete_dialog(ui);
        self.unsubscribe_sheet(ui);
        self.link_confirmation(ui);
        self.attachment_warning_dialog(ui);
        self.attachment_overwrite_dialog(ui);
        self.attachment_preview_dialog(ui);
        self.licences_dialog(ui);
        // The composer owns the keyboard while it is open. Capture the widget that had it before
        // the composer requests focus, so closing returns the user exactly where they were.
        if self.composer.open && self.composer_focus_pending {
            self.composer_return_focus = ui.ctx().memory(|memory| memory.focused());
            self.composer_focus_pending = false;
        }
        self.composer_window(ui);
        self.composer_preview_modal(ui);
        // The passphrase sheet belongs to the composer when it signs: draw it after the composer so
        // it sits on top instead of behind the composer modal.
        self.pgp_prompt_dialog(ui);
        if composer_was_open
            && !self.composer.open
            && let Some(id) = self.composer_return_focus.take()
        {
            ui.ctx().memory_mut(|memory| memory.request_focus(id));
        }
        self.toast_overlay(ui.ctx());
        if self.index_progress.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        }
    }
}

/// One background setting row: title and explanation on the left, a truthful on/off button on the
/// right. An unavailable row keeps its switch disabled and prints the reason from the daemon or the
/// platform instead of pretending the value is off.
fn background_row(
    ui: &mut Ui,
    key: &str,
    title: &str,
    description: &str,
    row: &background::Row,
    toggled: BackgroundChange,
) -> Option<BackgroundChange> {
    let t = tokens::tokens(ui.visuals().dark_mode);
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(typography::strong(Role::Body, title, t.text));
            ui.label(typography::text(Role::Caption, description, t.text_muted));
            if let Some(note) = row.reason.as_deref().or(row.detail.as_deref()) {
                ui.label(typography::text(Role::Meta, note, t.text_faint));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let response = Button::new(if row.on { "Attivo" } else { "Disattivo" })
                .selected(row.on)
                .enabled(row.available)
                .disabled_reason(row.reason.as_deref().unwrap_or("Questa impostazione non è disponibile"))
                .show(ui);
            #[cfg(test)]
            probe::settings_action(ui, key, response.rect);
            #[cfg(not(test))]
            let _ = key;
            clicked = response.clicked();
        });
    });
    clicked.then_some(toggled)
}

impl eframe::App for PostinoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_sync_worker();
        // Write the last pane width and folder even if the debounce had not fired yet. A capture
        // never owns a settings path, so this cannot touch the user's file.
        if self.screenshot.is_none() && self.settings_dirty {
            self.persist_settings();
        }
    }
}

#[derive(Clone, Copy)]
enum BulkAction {
    Read,
    Unread,
    Star,
    Unstar,
    Archive,
    Delete,
    Clear,
}

#[derive(Clone, Copy)]
enum FolderAction {
    MarkAllRead,
}

/// What one empty-state button does. The label lives in [`EmptyView`]; the action is data so the
/// resolver can be tested without a window and the screen never matches on copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmptyAction {
    ShowAll,
    ClearLabel,
    ClearUnread,
    ClearAttachments,
    ClearSearch,
    Compose,
    Inbox,
}

/// The copy and the up-to-two ways out of an empty list. Bookkeeping is resolved from the active
/// filters, never from the folder alone.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EmptyView {
    icon: &'static str,
    title: String,
    body: String,
    primary: Option<(&'static str, EmptyAction)>,
    secondary: Option<(&'static str, EmptyAction)>,
}

/// One sentence, attached to the only demo notice in the product.
const DEMO_TOOLTIP: &str = "Stai lavorando su dati sintetici: nessun server è collegato e nessun messaggio esce da questo computer.";
/// The loading line appears only after this delay, so local work that returns in a frame never
/// flashes a progress indicator.
const LOADING_DELAY: f64 = 0.2;
/// Settings are written at most this often while a pane is being dragged.
const SETTINGS_SAVE_DELAY: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusKind {
    Demo,
    Ready,
    Offline,
    Error,
}

#[cfg(test)]
fn status_kind(demo: bool, failure: bool) -> StatusKind {
    if failure {
        StatusKind::Error
    } else if demo {
        StatusKind::Demo
    } else {
        StatusKind::Ready
    }
}

/// The inbox of one account, or its first real folder. A store account always has an inbox because
/// `account_add` refuses a server without one; a half-configured row still lands somewhere visible.
fn default_folder_for(engine: &Engine, account: Option<&str>) -> String {
    let Some(account) = account else {
        return String::new();
    };
    let folders = engine.folders().iter().filter(|folder| folder.account_id == account).collect::<Vec<_>>();
    folders
        .iter()
        .find(|folder| folder.kind == FolderKind::Inbox)
        .or_else(|| folders.iter().find(|folder| folder.kind != FolderKind::Starred))
        .or_else(|| folders.first())
        .map_or_else(String::new, |folder| folder.id.clone())
}

/// The persisted sync health of one account as the three honest states the status bar shows.
fn sync_view_for(engine: &Engine, account: Option<&str>) -> SyncView {
    let Some(account) = account else {
        return SyncView::Idle { last_sync: None };
    };
    let Ok(views) = engine.account_views() else {
        return SyncView::Error { reason: "l'archivio non si legge".to_owned(), last_sync: None };
    };
    let Some(view) = views.into_iter().find(|view| view.account.id == account) else {
        return SyncView::Idle { last_sync: None };
    };
    let Some(status) = view.sync else {
        return SyncView::Idle { last_sync: None };
    };
    let last = status.last_success;
    match status.state {
        SyncPhase::Idle => SyncView::Idle { last_sync: last },
        SyncPhase::Syncing => SyncView::Error { reason: "sincronizzazione interrotta".to_owned(), last_sync: last },
        SyncPhase::Error => {
            let error = status.error.unwrap_or_default();
            if error.contains("could not talk to the server") {
                SyncView::Offline { last_sync: last }
            } else if error.contains("rejected the credentials") {
                SyncView::Error { reason: "credenziali rifiutate".to_owned(), last_sync: last }
            } else if error.contains("local store") {
                SyncView::Error { reason: "archivio locale non disponibile".to_owned(), last_sync: last }
            } else if error.contains("cancelled") {
                SyncView::Idle { last_sync: last }
            } else {
                SyncView::Error { reason: crate::sync_worker::persisted_reason(&error), last_sync: last }
            }
        }
    }
}

fn last_sync_from(view: &SyncView) -> Option<i64> {
    match view {
        SyncView::Idle { last_sync } | SyncView::Offline { last_sync } | SyncView::Error { last_sync, .. } => *last_sync,
        SyncView::Running(_) => None,
    }
}

fn format_millis(ms: i64, now: DateTime<Utc>) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(|| "—".to_owned(), |date| format_sync_time(date, now))
}

/// A sync instant in the user's timezone. A time without a date is only unambiguous today: a sync
/// from yesterday must say so instead of showing "12:30" as if it just happened.
fn format_sync_time(date: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let local = date.with_timezone(&Local);
    let now = now.with_timezone(&Local);
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else if local.year() == now.year() {
        local.format("%d/%m %H:%M").to_string()
    } else {
        local.format("%d/%m/%y %H:%M").to_string()
    }
}

/// The account badge counts every folder of the account, not the open folder. Both the tooltip and
/// the accessible name use this exact sentence.
fn account_unread_tooltip(unread: u32) -> String {
    match unread {
        0 => "Nessuna non letta in tutte le cartelle".to_owned(),
        1 => "1 non letta in tutte le cartelle".to_owned(),
        count => format!("{count} non lette in tutte le cartelle"),
    }
}

/// Short Italian copy for an engine failure. The raw engine sentence never reaches the status bar.
fn op_error_copy(error: &postino_core::OpError) -> String {
    match error {
        postino_core::OpError::Auth(_) => "credenziali rifiutate".to_owned(),
        postino_core::OpError::Network(_) => "server non raggiungibile".to_owned(),
        postino_core::OpError::InvalidArgs(_) => "dati non validi".to_owned(),
        postino_core::OpError::UnknownOp(_) => "operazione non disponibile".to_owned(),
        postino_core::OpError::Failed(_) => "l'archivio locale ha rifiutato l'operazione".to_owned(),
    }
}

/// The account-creation copy shown inside the onboarding card. It keeps the specific class and never
/// the raw engine sentence, which may mention internals.
fn add_account_copy(error: postino_core::OpError) -> String {
    match error {
        postino_core::OpError::Auth(_) => "Il server ha rifiutato le credenziali durante la creazione. Controlla la password.".to_owned(),
        postino_core::OpError::Network(_) => "Il server non è raggiungibile in questo momento. Riprova.".to_owned(),
        postino_core::OpError::InvalidArgs(_) => "I dati dell'account non sono validi. Controlla i campi.".to_owned(),
        postino_core::OpError::UnknownOp(_) => "La creazione dell'account non è disponibile in questa versione.".to_owned(),
        postino_core::OpError::Failed(_) => "La creazione dell'account non è riuscita. Controlla il portachiavi e riprova.".to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncView {
    /// No run in progress. `last_sync` is the persisted success instant, if any.
    Idle {
        last_sync: Option<i64>,
    },
    Running(SyncProgress),
    /// The last attempt could not reach the server. Cached mail stays readable.
    Offline {
        last_sync: Option<i64>,
    },
    Error {
        reason: String,
        last_sync: Option<i64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PgpPromptPurpose {
    Decrypt,
    Send,
}

/// A passphrase held only while its sheet is open. It is never put in `Settings`, a draft, a log or
/// a toast; the operation call serializes it once and the engine owns the zeroised copy.
struct PgpPromptState {
    purpose: PgpPromptPurpose,
    value: Zeroizing<String>,
    focus: bool,
    error: Option<String>,
}

impl PgpPromptState {
    fn new(purpose: PgpPromptPurpose) -> Self {
        Self { purpose, value: Zeroizing::new(String::new()), focus: true, error: None }
    }
}

/// The key import sheet. A private key is imported only with its passphrase; the armor is held here
/// until the operation returns and is never shown in the list.
struct PgpImportState {
    armor: Zeroizing<String>,
    passphrase: Zeroizing<String>,
    focus_armor: bool,
    error: Option<String>,
    busy: bool,
}

#[derive(Clone)]
struct PgpDeleteState {
    key: PgpKey,
    focus_cancel: bool,
}

/// The create/edit sheet for one local template. The body is plain text and never leaves the
/// encrypted store except inside a draft the person explicitly saves or sends.
#[derive(Clone)]
struct TemplateEditorState {
    id: Option<String>,
    name: String,
    subject: String,
    body: String,
    focus_name: bool,
    error: Option<String>,
}

/// The create/edit sheet for one alternate sender. The signature is plain text and the address
/// must already be authorised by the provider for submission.
#[derive(Clone)]
struct IdentityEditorState {
    id: Option<String>,
    name: String,
    email: String,
    signature: String,
    focus_name: bool,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AccountRemoveState {
    id: String,
    name: String,
    email: String,
    /// Removing the last account returns to onboarding; the copy says so.
    last: bool,
    /// Request focus on "Annulla" on the first frame only. The destructive button never gets Enter.
    focus_cancel: bool,
}

/// The per-account signature being edited in Settings. `None` in the app means collapsed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SignatureEditor {
    account: String,
    text: String,
    /// Request focus on the multiline field on the first frame only.
    focus: bool,
    error: Option<String>,
}

/// The one current failure: the sentence the toast shows and whether reloading the list is a
/// meaningful retry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Failure {
    message: String,
    retry_list: bool,
}

#[derive(Clone, Copy)]
enum RowAction {
    Open,
    Reply,
    ReplyAll,
    Forward,
    EditDraft,
    MarkRead,
    MarkUnread,
    Label,
    Star,
    Unstar,
    Archive,
    Delete,
}

struct PendingRead {
    id: String,
    at: f64,
}

struct ToastState {
    message: String,
    undo: Option<UndoAction>,
    until: f64,
    kind: ToastKind,
}

/// A pending resolution of one outbox row. The dialog states the consequence before the call.
#[derive(Clone, Debug)]
enum OutboxConfirm {
    Retry { id: String, subject: String, uncertain: bool },
    MarkSent { id: String, subject: String },
}

fn empty_outbox() -> OutboxList {
    OutboxList { entries: Vec::new(), halted: Vec::new(), warnings: Vec::new() }
}

/// A deterministic journal for the docs screenshot and the headless render test. It is never shown
/// as real mail: the dialog carries the "Dati demo" badge.
fn demo_outbox(now: i64) -> OutboxList {
    use postino_store::OutboxState;
    OutboxList {
        entries: vec![
            OutboxView {
                id: "ob_demo_1".to_owned(),
                account: "acc_demo".to_owned(),
                state: OutboxState::Uncertain,
                created_at: now - 90_000,
                send_at: now - 80_000,
                attempts: 1,
                last_error: Some(
                    "Postino si è fermato durante l'invio; il messaggio potrebbe essere stato accettato. Controlla Inviati prima di riprovare."
                        .to_owned(),
                ),
                recipient_count: 2,
                subject: "Preventivo sito web — bozza finale".to_owned(),
                message_id: "<demo-uncertain@example.it>".to_owned(),
                draft_id: Some("draft_1".to_owned()),
                undo_until: None,
            },
            OutboxView {
                id: "ob_demo_2".to_owned(),
                account: "acc_demo".to_owned(),
                state: OutboxState::Queued,
                created_at: now - 30_000,
                send_at: now + 3_000,
                attempts: 0,
                last_error: None,
                recipient_count: 3,
                subject: "Riunione di lunedì".to_owned(),
                message_id: "<demo-queued@example.it>".to_owned(),
                draft_id: Some("draft_2".to_owned()),
                undo_until: Some(now + 3_000),
            },
            OutboxView {
                id: "ob_demo_3".to_owned(),
                account: "acc_demo".to_owned(),
                state: OutboxState::Failed,
                created_at: now - 3_600_000,
                send_at: now - 3_500_000,
                attempts: 2,
                last_error: Some("Il server ha rifiutato il destinatario con il codice 550: casella inesistente.".to_owned()),
                recipient_count: 1,
                subject: "Fattura 2026-041".to_owned(),
                message_id: "<demo-failed@example.it>".to_owned(),
                draft_id: None,
                undo_until: None,
            },
            OutboxView {
                id: "ob_demo_4".to_owned(),
                account: "acc_demo".to_owned(),
                state: OutboxState::Sent,
                created_at: now - 7_200_000,
                send_at: now - 7_100_000,
                attempts: 1,
                last_error: None,
                recipient_count: 2,
                subject: "Riepilogo settimanale".to_owned(),
                message_id: "<demo-sent@example.it>".to_owned(),
                draft_id: None,
                undo_until: None,
            },
        ],
        halted: Vec::new(),
        warnings: Vec::new(),
    }
}

enum UndoAction {
    Unread(Vec<(String, bool)>),
    Starred(Vec<(String, bool)>),
    Move(Vec<(String, String)>),
    /// Cancel a queued submission during its undo window.
    CancelSend(String),
    /// `(message id, label id, whether the label was applied before the action)`.
    Labels(Vec<(String, String, bool)>),
    LabelDelete {
        name: String,
        color: LabelColor,
        message_ids: Vec<String>,
    },
    /// Restore the name and colour the label had before the rename.
    LabelRename {
        id: String,
        name: String,
        color: LabelColor,
    },
    /// Recreate a saved search the user deleted, with the same query and sort.
    SavedSearchDelete {
        name: String,
        query: String,
        sort: String,
        account: String,
    },
    /// Restore the previous name/query/sort of a saved search after an edit.
    SavedSearchUpdate {
        id: String,
        account: String,
        name: String,
        query: String,
        sort: String,
    },
}

enum LabelEditorMode {
    Create,
    Rename(String),
}

/// The saved-search drawer. `Create` saves the current query; `Update` edits an existing row.
enum SavedSearchEditorMode {
    Create,
    Update(String),
}

struct SavedSearchEditor {
    mode: SavedSearchEditorMode,
    name: String,
    query: String,
    sort: String,
    error: Option<String>,
    /// Request focus on the next render.
    focus: bool,
}

/// What Esc puts back after a saved search replaced the query, folder and filters.
#[derive(Clone)]
struct SavedRestore {
    folder: String,
    label_filter: Option<String>,
    label_folder: Option<String>,
    query: String,
    search_committed: String,
    unread_only: bool,
    attachments_only: bool,
    threads_view: bool,
    threads_view_before_search: Option<bool>,
    saved_folder_scope: bool,
    saved_query_folder: bool,
}

struct LabelEditor {
    mode: LabelEditorMode,
    text: String,
    color: LabelColor,
    error: Option<String>,
    /// Request focus on the next render.
    focus: bool,
    /// The field actually owns the keyboard, so only then intercept Enter/Escape.
    focused: bool,
    /// A colour swatch owns the keyboard, so Enter/Space picks a colour instead of saving.
    swatch_focused: bool,
    /// Messages captured by the label menu; a successful creation applies the new label to them.
    apply_to: Vec<String>,
}

#[derive(Clone)]
struct LabelDeleteState {
    id: String,
    name: String,
    count: u32,
    message_ids: Vec<String>,
    /// Request keyboard focus on the confirm button on the first frame.
    focus_primary: bool,
}

/// The selection, folder and filters the label menu was opened against.
#[derive(Clone, PartialEq, Eq)]
struct LabelMenuContext {
    /// The messages the menu may touch.
    target: Vec<String>,
    /// The visible selection at open time; if it changes, the menu closes.
    selection: Vec<String>,
    folder: String,
    label_filter: Option<String>,
    label_folder: Option<String>,
    query: String,
    unread_only: bool,
    attachments_only: bool,
}

struct LabelMenuState {
    context: LabelMenuContext,
    query: String,
    focus: bool,
}

enum LabelAction {
    New,
    Rename(String),
    Delete(String),
}

enum SavedAction {
    Save,
    Rename(String),
    Delete(String),
    Apply(String),
}

#[cfg(test)]
pub(crate) use crate::commands::command;
pub(crate) use crate::commands::{
    COMMANDS, Command, CommandId, CommandRule, command_icon, command_label, command_shortcut, filtered_commands, menu_item,
};

/// A hostile body can carry a 10 KB URL. The inline label is capped; the hover tooltip and the
/// confirmation sheet still show the full destination.
pub(crate) fn truncate_link_label(label: &str) -> String {
    if label.chars().count() <= reading::MAX_LINK_LABEL_CHARS {
        return label.to_owned();
    }
    let mut shown = label.chars().take(reading::MAX_LINK_LABEL_CHARS.saturating_sub(1)).collect::<String>();
    shown.push('…');
    shown
}

/// A hover tooltip is capped; the sheet has the full URL in a scrollable body.
pub(crate) fn truncate_tooltip_url(url: &str) -> String {
    if url.chars().count() <= reading::MAX_LINK_TOOLTIP_CHARS {
        return url.to_owned();
    }
    let mut shown = url.chars().take(reading::MAX_LINK_TOOLTIP_CHARS.saturating_sub(1)).collect::<String>();
    shown.push('…');
    shown
}

/// Render one paragraph with clickable links. Only `reading::find_links` decides what is a link;
/// everything else stays ordinary selectable text. The caller opens the confirmation sheet.
///
/// Returns the clicked link and how many link widgets this paragraph consumed from the message
/// budget.
fn link_paragraph(ui: &mut Ui, paragraph: &str, t: &Tokens, muted: bool, limit: usize) -> (Option<ExtractedLink>, usize) {
    let links =
        if limit >= reading::MAX_LINKS_PER_PARAGRAPH { reading::find_links(paragraph) } else { reading::find_links_limited(paragraph, limit) };
    if links.is_empty() {
        let color = if muted { t.text_muted } else { t.text };
        ui.add(egui::Label::new(typography::text(Role::Body, paragraph, color)).selectable(true));
        return (None, 0);
    }
    let used = links.len();
    let text_color = if muted { t.text_muted } else { t.text };
    let font = FontId::new(type_size::BODY, FontFamily::Proportional);
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let mut cursor = 0;
        for link in links {
            if let Some(before) = paragraph.get(cursor..link.range.start)
                && !before.is_empty()
            {
                ui.add(
                    egui::Label::new(RichText::new(before).font(font.clone()).color(text_color)).wrap_mode(egui::TextWrapMode::Wrap).selectable(true),
                );
            }
            let Some(label) = paragraph.get(link.range.clone()) else {
                continue;
            };
            let shown = truncate_link_label(label);
            let rich = RichText::new(shown).font(font.clone()).color(t.gold_ink).underline();
            let response = ui.add(egui::Link::new(rich)).on_hover_text(truncate_tooltip_url(&link.url));
            #[cfg(test)]
            probe::link_widget(ui, &link.url, response.rect);
            if response.clicked() || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
                clicked = Some(link.clone());
            }
            cursor = link.range.end;
        }
        if let Some(after) = paragraph.get(cursor..)
            && !after.is_empty()
        {
            ui.add(egui::Label::new(RichText::new(after).font(font).color(text_color)).wrap_mode(egui::TextWrapMode::Wrap).selectable(true));
        }
    });
    (clicked, used)
}

fn folder_icon(kind: FolderKind) -> &'static str {
    match kind {
        FolderKind::Inbox => icons::INBOX,
        FolderKind::Starred => icons::STAR,
        FolderKind::Sent => icons::SENT,
        FolderKind::Drafts => icons::DRAFTS,
        FolderKind::Archive => icons::ARCHIVE,
        FolderKind::Spam => icons::WARNING,
        FolderKind::Trash => icons::TRASH,
        FolderKind::Custom => icons::FOLDER,
    }
}

/// Italian singular/plural for a count: `plural(1, "messaggio", "messaggi")` → "1 messaggio".
pub(crate) fn plural(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 { format!("1 {singular}") } else { format!("{count} {plural}") }
}

fn messages_text(count: usize) -> String {
    plural(count, "messaggio", "messaggi")
}

fn unread_text(unread: usize) -> String {
    plural(unread, "non letto", "non letti")
}

/// The search header: an exact total, or the explicit 10 000 prefix when more rows match.
/// True when a query contains a `folder:` clause. Used by a saved search to decide whether its
/// scope owns a folder or is account-wide, and by the header's "Tutte le mail" label.
fn query_has_folder(query: &str) -> bool {
    fn walk(expr: &postino_core::SearchExpr) -> bool {
        match expr {
            postino_core::SearchExpr::Folder(_) => true,
            postino_core::SearchExpr::And(children) | postino_core::SearchExpr::Or(children) => children.iter().any(walk),
            postino_core::SearchExpr::Not(child) => walk(child),
            _ => false,
        }
    }
    postino_core::parse_search(query).is_ok_and(|parsed| walk(&parsed.expr))
}

/// Strip repeated reply/forward prefixes for a conversation title. The stored subject stays
/// untouched; this is a display decision, and an empty result falls back to the original.
fn thread_subject(subject: &str) -> String {
    let mut rest = subject.trim();
    loop {
        let lower = rest.to_lowercase();
        let prefix = if lower.starts_with("re:") {
            3
        } else if lower.starts_with("fwd:") {
            4
        } else if lower.starts_with("fw:") {
            3
        } else {
            break;
        };
        let Some(after) = rest.get(prefix..) else { break };
        rest = after.trim_start();
        if rest.is_empty() {
            break;
        }
    }
    if rest.is_empty() { subject.trim().to_owned() } else { rest.to_owned() }
}

/// Group flat search hits (or any summary slice) into conversation rows. Counts describe the slice,
/// not the whole mailbox: this is the threaded search view, and the header says so.
fn group_threads(messages: &[MessageSummary], account: &str) -> Vec<ThreadSummary> {
    let mut grouped: BTreeMap<String, Vec<&MessageSummary>> = BTreeMap::new();
    for message in messages {
        grouped.entry(message.thread_id.clone()).or_default().push(message);
    }
    grouped
        .into_iter()
        .filter_map(|(id, mut rows)| {
            rows.sort_by(|left, right| left.date.cmp(&right.date).then(left.id.cmp(&right.id)));
            let first = *rows.first()?;
            let latest = *rows.last()?;
            let mut participants: Vec<Address> = Vec::new();
            let mut labels: Vec<String> = Vec::new();
            let mut unread_count = 0u32;
            let mut starred_count = 0u32;
            for message in &rows {
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
                id,
                account_id: account.to_owned(),
                subject: thread_subject(&first.subject),
                snippet: latest.snippet.clone(),
                participants,
                last_date: latest.date,
                message_count: u32::try_from(rows.len()).unwrap_or(u32::MAX),
                unread_count,
                starred_count,
                has_attachments: rows.iter().any(|message| message.has_attachments),
                latest_message_id: Some(latest.id.clone()),
                labels,
                state: if unread_count > 0 { "unread".to_owned() } else { "read".to_owned() },
            })
        })
        .collect()
}

/// Translate the verify verdict into the one line a person needs. The engine's `reason` is for
/// diagnostics and is English; the UI never shows it raw.
fn pgp_verify_tooltip(result: &PgpVerifyResult) -> String {
    match result.state {
        PgpVerifyState::Verified => {
            let signer = result.signer.as_deref().unwrap_or("chiave salvata");
            let fingerprint = result.fingerprint.as_deref().unwrap_or("");
            if fingerprint.is_empty() {
                format!("Firma valida per i byte originali, da {signer}.")
            } else {
                format!("Firma valida per i byte originali, da {signer} · {fingerprint}.")
            }
        }
        PgpVerifyState::Bad => "La firma non corrisponde a nessuna chiave pubblica salvata.".to_owned(),
        PgpVerifyState::NoKey => "Nessuna chiave pubblica salvata per questo account.".to_owned(),
        PgpVerifyState::Unknown => "I byte originali non sono leggibili come PGP/MIME firmato.".to_owned(),
        PgpVerifyState::NotStored => "I byte originali non sono conservati: la firma non è verificabile.".to_owned(),
    }
}

fn pgp_decrypt_tooltip(result: &PgpDecryptResult) -> String {
    match result.state {
        PgpDecryptState::Decrypted => "Testo decifrato visibile solo in questa finestra.".to_owned(),
        PgpDecryptState::Locked => "La chiave privata richiede una passphrase.".to_owned(),
        PgpDecryptState::WrongPassphrase => "La passphrase non apre la chiave privata.".to_owned(),
        PgpDecryptState::NoKey => "Nessuna chiave privata salvata per questo account.".to_owned(),
        PgpDecryptState::Unknown => "La decifratura non è riuscita: il messaggio o la chiave non sono leggibili.".to_owned(),
        PgpDecryptState::NotStored => "I byte originali non sono conservati: il messaggio non è decifrabile.".to_owned(),
    }
}

/// Translate the engine's English label errors into UI copy. A duplicate name is the common case;
/// the rest keeps the reason category without leaking internal wording.
fn label_error_message(raw: &str) -> String {
    let raw = raw.strip_prefix("invalid arguments: ").unwrap_or(raw);
    if raw.contains("already exists") {
        "Esiste già un'etichetta con questo nome.".to_owned()
    } else if raw.contains("1-48") {
        "Il nome deve avere da 1 a 48 caratteri.".to_owned()
    } else if raw.contains("control") || raw.contains("bidi") || raw.contains("format") {
        "Il nome contiene caratteri non consentiti.".to_owned()
    } else {
        "Impossibile salvare l'etichetta.".to_owned()
    }
}

/// Resolve a stored semantic colour through the active theme. `forest` follows the body text so
/// the dot stays visible in the dark theme instead of disappearing into the surface. Gold is ink
/// on ivory (4.99:1) and the light gold only on forest, where the selected dot is drawn.
fn label_color(color: LabelColor, t: &Tokens) -> Color32 {
    match color {
        LabelColor::Gold => t.gold_ink,
        LabelColor::Forest => t.text,
        LabelColor::Success => t.success,
        LabelColor::Danger => t.danger,
        LabelColor::Muted => t.text_faint,
    }
}

/// The same colour on a selected forest row. Forest becomes ivory; the other semantic colours use
/// their on-forest variants so a label keeps its identity instead of turning gold.
fn label_dot(color: LabelColor, t: &Tokens, selected: bool) -> Color32 {
    if !selected {
        return label_color(color, t);
    }
    match color {
        LabelColor::Gold => t.gold,
        LabelColor::Forest => t.selected_text,
        LabelColor::Success => t.selected_success,
        LabelColor::Danger => t.selected_danger,
        LabelColor::Muted => t.selected_muted,
    }
}

/// The five semantic swatches of the label editor, in palette order.
const LABEL_COLORS: [(LabelColor, &str); 5] = [
    (LabelColor::Gold, "Oro"),
    (LabelColor::Forest, "Bosco"),
    (LabelColor::Success, "Successo"),
    (LabelColor::Danger, "Rosso"),
    (LabelColor::Muted, "Neutro"),
];

/// What the pointer controls of the inline editor asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LabelEditorAction {
    None,
    Save,
    Cancel,
}

/// The inline create/rename editor: title, one-line field, error, colour row, hint and the two
/// pointer controls. It sits directly under the ETICHETTE header so none of those parts can scroll
/// out of view.
fn label_editor_widget(ui: &mut Ui, editor: &mut LabelEditor, t: &Tokens) -> LabelEditorAction {
    let mut action = LabelEditorAction::None;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let title = match &editor.mode {
            LabelEditorMode::Create => "Nuova etichetta",
            LabelEditorMode::Rename(_) => "Rinomina etichetta",
        };
        ui.label(typography::text(Role::Meta, title, t.text_muted));
        let focus = editor.focus;
        editor.focus = false;
        let error = editor.error.clone();
        let mut field = TextField::new(&mut editor.text, "Nome etichetta").width(ui.available_width()).focus(focus);
        if let Some(error) = error.as_deref() {
            field = field.error(error);
        }
        editor.focused = field.show(ui).has_focus();
        ui.add_space(space::XS);
        editor.swatch_focused = label_color_swatches(ui, &mut editor.color, t);
        ui.label(typography::text(Role::Caption, "Invio salva · Esc annulla\nTab poi ←/→ e Invio", t.text_faint));
        ui.add_space(space::XS);
        ui.horizontal(|ui| {
            let save = Button::primary("Salva").size(ButtonSize::Sm).show(ui);
            #[cfg(test)]
            probe::editor_save(ui, save.rect);
            if save.clicked() {
                action = LabelEditorAction::Save;
            }
            let cancel = Button::ghost("Annulla").size(ButtonSize::Sm).show(ui);
            #[cfg(test)]
            probe::editor_cancel(ui, cancel.rect);
            if cancel.clicked() {
                action = LabelEditorAction::Cancel;
            }
        });
    });
    action
}

/// A saved-search editor error in Italian. The engine sentence stays in the API/CLI; the UI never
/// echoes an English engine string back to a person.
fn saved_editor_error(message: &str) -> String {
    if message.contains("already exists") {
        "Esiste già una ricerca con questo nome.".to_owned()
    } else if message.contains("unknown label") || message.contains("unknown folder") {
        "La ricerca cita un'etichetta o una cartella che non esiste.".to_owned()
    } else if message.contains("search query") {
        "La ricerca non è valida: controlla virgolette, parentesi e campi.".to_owned()
    } else if message.contains("saved searches") {
        "Hai già 12 ricerche salvate: eliminane una prima di aggiungerne un'altra.".to_owned()
    } else {
        "Non è stato possibile salvare la ricerca: correggi il nome o la query.".to_owned()
    }
}

/// The saved-search drawer. It shares the sidebar drawer slot with the label editor and returns the
/// same save/cancel action so the shell has one keyboard path.
fn saved_search_editor_widget(ui: &mut Ui, editor: &mut Option<SavedSearchEditor>, t: &Tokens) -> LabelEditorAction {
    let mut action = LabelEditorAction::None;
    let Some(editor) = editor.as_mut() else { return action };
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let title = match &editor.mode {
            SavedSearchEditorMode::Create => "Salva ricerca",
            SavedSearchEditorMode::Update(_) => "Modifica ricerca",
        };
        ui.label(typography::text(Role::Meta, title, t.text_muted));
        let focus = editor.focus;
        editor.focus = false;
        let mut name = TextField::new(&mut editor.name, "Nome").width(ui.available_width()).focus(focus);
        if let Some(error) = editor.error.as_deref() {
            name = name.error(error);
        }
        name.show(ui);
        TextField::new(&mut editor.query, "Ricerca").width(ui.available_width()).show(ui);
        ui.horizontal(|ui| {
            if Chip::new("Rilevanza").selected(editor.sort == "relevance").show(ui).clicked() {
                editor.sort = "relevance".to_owned();
            }
            if Chip::new("Data").selected(editor.sort == "date").show(ui).clicked() {
                editor.sort = "date".to_owned();
            }
        });
        ui.add_space(space::XS);
        ui.horizontal(|ui| {
            let save = Button::primary("Salva").size(ButtonSize::Sm).show(ui);
            #[cfg(test)]
            probe::editor_save(ui, save.rect);
            if save.clicked() {
                action = LabelEditorAction::Save;
            }
            let cancel = Button::ghost("Annulla").size(ButtonSize::Sm).show(ui);
            #[cfg(test)]
            probe::editor_cancel(ui, cancel.rect);
            if cancel.clicked() {
                action = LabelEditorAction::Cancel;
            }
        });
    });
    action
}

/// A row of five colour swatches. Each target is 28 px with its colour name on hover; click
/// selects, Tab reaches them, Left/Right move between them and Enter/Space pick the focused one.
/// The selected swatch carries a ring, not only colour.
fn label_color_swatches(ui: &mut Ui, selected: &mut LabelColor, t: &Tokens) -> bool {
    let ids = LABEL_COLORS.iter().enumerate().map(|(index, _)| ui.make_persistent_id(("label-color", index))).collect::<Vec<_>>();
    let mut focused = false;
    ui.horizontal(|ui| {
        for (index, (color, name)) in LABEL_COLORS.iter().enumerate() {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
            let id = ids.get(index).copied().unwrap_or_else(|| ui.make_persistent_id(("label-color", index)));
            let response = ui.interact(rect, id, Sense::click());
            let active = *selected == *color;
            if active {
                ui.painter().circle_stroke(rect.center(), 10.0, Stroke::new(2.0, t.gold_ink));
            }
            ui.painter().circle_filled(rect.center(), 6.5, label_color(*color, t));
            if response.has_focus() {
                focused = true;
                crate::ui::tokens::paint_focus_ring(ui, rect, radius::PILL, t);
                let left = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowLeft));
                let right = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowRight));
                if left || right {
                    let step = if right { 1 } else { LABEL_COLORS.len() - 1 };
                    let next = (index + step) % LABEL_COLORS.len();
                    if let Some(next) = ids.get(next) {
                        ui.ctx().memory_mut(|memory| memory.request_focus(*next));
                    }
                }
            }
            if response.clicked() {
                *selected = *color;
                response.request_focus();
            }
            response.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, true, active, *name));
            response.on_hover_text(*name);
        }
    });
    focused
}

/// The sidebar uses the shared always-visible scroll bar: at 980×600 an overflowing list with
/// only a hover-revealed sliver looked broken.
fn sidebar_scroll_style() -> egui::style::ScrollStyle {
    crate::ui::tokens::scroll_style()
}

/// The reading pane takes 52% of the content width left by the sidebar, clamped to 420 pt and 62%,
/// and it yields space back to the list whenever the list would fall below its 460 pt floor. At
/// 1280×720 this is a ~541 pt pane and a ~499 pt list instead of the old 666/374 split.
#[cfg(test)]
fn reading_pane_width(available: f32) -> f32 {
    reading_pane_width_with_ratio(available, READING_PANE_DEFAULT_RATIO)
}

/// The pane width for a stored fraction. The clamps still win: a settings file cannot squeeze the
/// list below its floor or the pane below 420 pt.
fn reading_pane_width_with_ratio(available: f32, ratio: f32) -> f32 {
    let remaining = (available - ui_size::SIDEBAR).max(0.0);
    if remaining <= 0.0 {
        return READING_PANE_MIN_SIZE;
    }
    let max = reading_pane_max_width(available);
    let ratio = if ratio.is_finite() { ratio.clamp(0.1, 0.9) } else { READING_PANE_DEFAULT_RATIO };
    let preferred = (remaining * ratio).clamp(READING_PANE_MIN_SIZE, max);
    let list_cap = (remaining - ui_size::LIST_MIN).max(READING_PANE_MIN_SIZE);
    preferred.min(list_cap)
}

/// The widest the resizable pane may become: 62% of the content left by the sidebar, capped so a
/// user drag cannot squeeze the list below its 460 pt floor (where the 420 pt pane minimum allows).
fn reading_pane_max_width(available: f32) -> f32 {
    let remaining = (available - ui_size::SIDEBAR).max(0.0);
    let ratio = (remaining * READING_PANE_MAX_RATIO).max(READING_PANE_MIN_SIZE);
    ratio.min((remaining - ui_size::LIST_MIN).max(READING_PANE_MIN_SIZE))
}

/// egui counts a third quick click as a triple, even when the previous double-click was on another
/// widget. A row double-click followed by a header double-click must still restore, so both the
/// double and the triple activation mean "the same gesture happened again".
fn double_activated(response: &egui::Response) -> bool {
    response.double_clicked() || response.triple_clicked()
}

/// Below the breakpoint the reading pane is a full-width overlay, not a resizable side pane.
fn reading_overlay(available: f32) -> bool {
    available < READING_PANE_MIN_WIDTH
}
fn avatar(ui: &mut Ui, size: f32, initials: &str, bg: Color32, fg: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().circle_filled(rect.center(), size * 0.5, bg);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, initials, FontId::new(size * 0.36, theme::medium()), fg);
}

fn logo(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(6), theme::FOREST);
    let inset_x = size * 0.24;
    let top = rect.top() + size * 0.31;
    let bottom = rect.bottom() - size * 0.29;
    let left = rect.left() + inset_x;
    let right = rect.right() - inset_x;
    let stroke = Stroke::new((size * 0.045).max(1.2), IVORY);
    ui.painter().line_segment([egui::pos2(left, top), egui::pos2(left, bottom)], stroke);
    ui.painter().line_segment([egui::pos2(right, top), egui::pos2(right, bottom)], stroke);
    ui.painter().line_segment([egui::pos2(left, bottom), egui::pos2(right, bottom)], stroke);
    let mid = egui::pos2(rect.center().x, top + (bottom - top) * 0.52);
    ui.painter().line_segment([egui::pos2(left, top), mid], stroke);
    ui.painter().line_segment([egui::pos2(right, top), mid], stroke);
    let gold = Stroke::new(stroke.width, GOLD);
    let chevron_y = bottom - size * 0.02;
    let half = size * 0.12;
    ui.painter().line_segment([egui::pos2(rect.center().x - half, chevron_y - size * 0.07), egui::pos2(rect.center().x, chevron_y)], gold);
    ui.painter().line_segment([egui::pos2(rect.center().x + half, chevron_y - size * 0.07), egui::pos2(rect.center().x, chevron_y)], gold);
}

fn format_when(date: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let local = date.with_timezone(&Local);
    let now = now.with_timezone(&Local);
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else if local.year() == now.year() {
        local.format("%d/%m").to_string()
    } else {
        // The compact year keeps the column at 44 pt: `dd/MM/yy`, not the old `dd/MM/YYYY`.
        local.format("%d/%m/%y").to_string()
    }
}

/// A clock time in the user's timezone. Display stays local; only the source of "now" is the
/// app-owned clock.
fn format_time(date: DateTime<Utc>) -> String {
    date.with_timezone(&Local).format("%H:%M").to_string()
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", format!("{:.1}", bytes as f64 / 1024.0).replace('.', ","))
    } else {
        format!("{} MB", format!("{:.1}", bytes as f64 / (1024.0 * 1024.0)).replace('.', ","))
    }
}

#[cfg(test)]
pub(crate) mod probe {
    //! Deterministic per-frame facts a headless test can assert on: which rows were drawn, where
    //! they were and which actions the reading toolbar offered. Nothing is recorded in a shipped
    //! build; the call sites are `#[cfg(test)]` statements.

    use std::collections::{BTreeMap, BTreeSet};

    use eframe::egui::{Context, Id, Rect, Ui, Vec2};

    fn id(key: &str) -> Id {
        Id::new(("postino-probe", key))
    }

    pub fn clear_rows(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("rows"), BTreeMap::<String, Rect>::new()));
    }

    pub fn row(ui: &Ui, name: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("rows")).insert(name.to_owned(), rect);
        });
    }

    pub fn rows(ctx: &Context) -> BTreeMap<String, Rect> {
        ctx.data(|data| data.get_temp(id("rows"))).unwrap_or_default()
    }

    pub fn scroll(ui: &Ui, viewport: Rect, content: Vec2, offset: Vec2) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("scroll-view"), viewport);
            data.insert_temp(id("scroll-content"), content);
            data.insert_temp(id("scroll-offset"), offset.y);
        });
    }

    pub fn scroll_view(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("scroll-view")))
    }

    pub fn scroll_content(ctx: &Context) -> Option<Vec2> {
        ctx.data(|data| data.get_temp(id("scroll-content")))
    }

    pub fn scroll_offset(ctx: &Context) -> Option<f32> {
        ctx.data(|data| data.get_temp(id("scroll-offset")))
    }

    pub fn sidebar_panel(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("sidebar-panel"), rect));
    }

    pub fn sidebar_panel_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("sidebar-panel")))
    }

    pub fn clear_identity(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("identity-blocks"), Vec::<Rect>::new()));
    }

    pub fn identity(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| {
            let mut blocks = data.get_temp::<Vec<Rect>>(id("identity-blocks")).unwrap_or_default();
            blocks.push(rect);
            data.insert_temp(id("identity-blocks"), blocks);
        });
    }

    pub fn identity_count(ctx: &Context) -> usize {
        ctx.data(|data| data.get_temp::<Vec<Rect>>(id("identity-blocks"))).map_or(0, |blocks| blocks.len())
    }

    pub fn clear_retries(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("retry-buttons"), Vec::<Rect>::new()));
    }

    pub fn retry(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| {
            let mut buttons = data.get_temp::<Vec<Rect>>(id("retry-buttons")).unwrap_or_default();
            buttons.push(rect);
            data.insert_temp(id("retry-buttons"), buttons);
        });
    }

    pub fn retry_count(ctx: &Context) -> usize {
        ctx.data(|data| data.get_temp::<Vec<Rect>>(id("retry-buttons"))).map_or(0, |buttons| buttons.len())
    }

    pub fn retry_rects(ctx: &Context) -> Vec<Rect> {
        ctx.data(|data| data.get_temp::<Vec<Rect>>(id("retry-buttons"))).unwrap_or_default()
    }

    pub fn clear_links(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("link-widgets"), BTreeMap::<String, Rect>::new()));
        ui.ctx().data_mut(|data| data.insert_temp(id("link-focus"), BTreeMap::<String, Id>::new()));
    }

    pub fn link_widget(ui: &Ui, url: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("link-widgets")).insert(url.to_owned(), rect);
        });
    }

    /// The widget id of a painted link, so a test can put keyboard focus on it.
    pub fn link_focus(ui: &Ui, url: &str, widget: Id) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Id>>(id("link-focus")).insert(url.to_owned(), widget);
        });
    }

    pub fn link_id(ctx: &Context, url: &str) -> Option<Id> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Id>>(id("link-focus"))).and_then(|links| links.get(url).copied())
    }

    pub fn link_rect(ctx: &Context, url: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("link-widgets"))).and_then(|links| links.get(url).copied())
    }

    pub fn link_count(ctx: &Context) -> usize {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("link-widgets"))).map_or(0, |links| links.len())
    }

    pub fn clear_attachment_saves(ui: &Ui) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("attachment-saves"), BTreeMap::<String, Rect>::new());
            data.insert_temp(id("attachment-previews"), BTreeMap::<String, Rect>::new());
        });
    }

    pub fn clear_invite_actions(ui: &Ui) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("invite-actions"), BTreeMap::<String, Rect>::new());
            data.insert_temp(id("invite-action-enabled"), BTreeMap::<String, bool>::new());
            data.insert_temp(id("invite-action-ids"), BTreeMap::<String, Id>::new());
            data.insert_temp(id("invite-card"), None::<Rect>);
        });
    }

    pub fn invite_action(ui: &Ui, action: &str, widget: &eframe::egui::Response, enabled: bool) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("invite-actions")).insert(action.to_owned(), widget.rect);
            data.get_temp_mut_or_default::<BTreeMap<String, bool>>(id("invite-action-enabled")).insert(action.to_owned(), enabled);
            data.get_temp_mut_or_default::<BTreeMap<String, Id>>(id("invite-action-ids")).insert(action.to_owned(), widget.id);
        });
    }

    pub fn invite_action_id(ctx: &Context, action: &str) -> Option<Id> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Id>>(id("invite-action-ids"))).and_then(|rows| rows.get(action).copied())
    }

    pub fn invite_action_rect(ctx: &Context, action: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("invite-actions"))).and_then(|rows| rows.get(action).copied())
    }

    pub fn invite_action_is_enabled(ctx: &Context, action: &str) -> Option<bool> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, bool>>(id("invite-action-enabled"))).and_then(|rows| rows.get(action).copied())
    }

    pub fn invite_card(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("invite-card"), Some(rect)));
    }

    pub fn invite_card_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<Option<Rect>>(id("invite-card"))).flatten()
    }

    pub fn attachment_save(ui: &Ui, attachment_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("attachment-saves")).insert(attachment_id.to_owned(), rect);
        });
    }

    pub fn attachment_save_rect(ctx: &Context, attachment_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("attachment-saves"))).and_then(|rows| rows.get(attachment_id).copied())
    }

    pub fn attachment_preview(ui: &Ui, attachment_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("attachment-previews")).insert(attachment_id.to_owned(), rect);
        });
    }

    pub fn attachment_preview_rect(ctx: &Context, attachment_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("attachment-previews"))).and_then(|rows| rows.get(attachment_id).copied())
    }

    pub fn composer_preview_modal(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(Id::new("probe-composer-preview-modal"), rect));
    }

    pub fn composer_preview_modal_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(Id::new("probe-composer-preview-modal")))
    }

    pub fn attachment_preview_modal(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("attachment-preview-modal"), rect));
    }

    pub fn attachment_preview_modal_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("attachment-preview-modal")))
    }

    pub fn sidebar_header(ui: &Ui, name: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("sidebar-headers")).insert(name.to_owned(), rect);
        });
    }

    pub fn sidebar_header_rect(ctx: &Context, name: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("sidebar-headers"))).and_then(|rows| rows.get(name).copied())
    }

    pub fn editor(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("editor"), rect));
    }

    pub fn editor_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("editor")))
    }

    pub fn editor_drawer(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("editor-drawer"), rect));
    }

    pub fn editor_drawer_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("editor-drawer")))
    }

    pub fn composer_card(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-card"), rect));
    }

    pub fn composer_card_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-card")))
    }

    pub fn composer_surface(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-surface"), rect));
    }

    pub fn composer_surface_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-surface")))
    }

    pub fn outbox_button(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("outbox-button"), rect));
    }

    pub fn outbox_button_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("outbox-button")))
    }

    pub fn outbox_card(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("outbox-card"), rect));
    }

    pub fn outbox_card_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("outbox-card")))
    }

    pub fn outbox_footer(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("outbox-footer"), rect));
    }

    pub fn outbox_footer_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("outbox-footer")))
    }

    pub fn demo_return_button(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("demo-return-button"), rect));
    }

    pub fn demo_return_button_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("demo-return-button")))
    }

    pub fn thread_star(ui: &Ui, thread_id: &str, starred: bool, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, (bool, Rect)>>(id("thread-stars")).insert(thread_id.to_owned(), (starred, rect));
        });
    }

    pub fn thread_star_state(ctx: &Context, thread_id: &str) -> Option<(bool, Rect)> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, (bool, Rect)>>(id("thread-stars"))).and_then(|stars| stars.get(thread_id).copied())
    }

    pub fn clear_thread_stars(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("thread-stars"), BTreeMap::<String, (bool, Rect)>::new()));
    }

    pub fn composer_send(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-send"), rect));
    }
    pub fn composer_send_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-send")))
    }

    pub fn composer_cancel(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-cancel"), rect));
    }

    pub fn composer_cancel_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-cancel")))
    }

    pub fn composer_footer_state(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-footer-state"), rect));
    }

    pub fn composer_footer_state_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-footer-state")))
    }

    pub fn composer_attach(ui: &Ui, rect: Rect, enabled: bool) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("composer-attach"), rect);
            data.insert_temp(id("composer-attach-enabled"), enabled);
        });
    }

    pub fn composer_attach_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-attach")))
    }

    pub fn composer_attach_id(ui: &Ui, widget: Id) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-attach-id"), widget));
    }

    pub fn composer_attach_widget_id(ctx: &Context) -> Option<Id> {
        ctx.data(|data| data.get_temp(id("composer-attach-id")))
    }

    pub fn composer_attach_enabled(ctx: &Context) -> Option<bool> {
        ctx.data(|data| data.get_temp(id("composer-attach-enabled")))
    }

    pub fn composer_attach_clicked(ui: &Ui, clicked: bool) {
        if clicked {
            let previous = ui.ctx().data(|data| data.get_temp::<u32>(id("composer-attach-clicked")).unwrap_or(0));
            ui.ctx().data_mut(|data| data.insert_temp(id("composer-attach-clicked"), previous.saturating_add(1)));
        }
    }

    pub fn composer_attach_click_count(ctx: &Context) -> u32 {
        ctx.data(|data| data.get_temp(id("composer-attach-clicked"))).unwrap_or(0)
    }

    pub fn composer_attachment_remove(ui: &Ui, attachment_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("composer-attachment-removes")).insert(attachment_id.to_owned(), rect);
        });
    }

    pub fn composer_attachment_remove_rect(ctx: &Context, attachment_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("composer-attachment-removes"))).and_then(|rows| rows.get(attachment_id).copied())
    }

    pub fn composer_attachment_remove_id(ui: &Ui, attachment_id: &str, widget: Id) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Id>>(id("composer-attachment-remove-ids")).insert(attachment_id.to_owned(), widget);
        });
    }

    pub fn composer_attachment_remove_widget_id(ctx: &Context, attachment_id: &str) -> Option<Id> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Id>>(id("composer-attachment-remove-ids"))).and_then(|rows| rows.get(attachment_id).copied())
    }

    pub fn composer_attachment_remove_clicked(ui: &Ui, attachment_id: &str, clicked: bool) {
        if clicked {
            ui.ctx().data_mut(|data| {
                data.get_temp_mut_or_default::<BTreeSet<String>>(id("composer-attachment-remove-clicks")).insert(attachment_id.to_owned());
            });
        }
    }

    pub fn composer_attachment_remove_was_clicked(ctx: &Context, attachment_id: &str) -> bool {
        ctx.data(|data| data.get_temp::<BTreeSet<String>>(id("composer-attachment-remove-clicks"))).is_some_and(|rows| rows.contains(attachment_id))
    }

    pub fn signature_open_button(ui: &Ui, account_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("signature-open-buttons")).insert(account_id.to_owned(), rect);
        });
    }

    pub fn signature_open_button_id(ui: &Ui, account_id: &str, widget: Id) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Id>>(id("signature-open-button-ids")).insert(account_id.to_owned(), widget);
        });
    }

    pub fn signature_open_button_widget_id(ctx: &Context, account_id: &str) -> Option<Id> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Id>>(id("signature-open-button-ids"))).and_then(|rows| rows.get(account_id).copied())
    }

    pub fn signature_open_button_rect(ctx: &Context, account_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("signature-open-buttons"))).and_then(|rows| rows.get(account_id).copied())
    }

    pub fn signature_open_clicked(ui: &Ui, clicked: bool) {
        if clicked {
            let previous = ui.ctx().data(|data| data.get_temp::<u32>(id("signature-open-clicks")).unwrap_or(0));
            ui.ctx().data_mut(|data| data.insert_temp(id("signature-open-clicks"), previous.saturating_add(1)));
        }
    }

    pub fn signature_open_click_count(ctx: &Context) -> u32 {
        ctx.data(|data| data.get_temp(id("signature-open-clicks"))).unwrap_or(0)
    }

    pub fn signature_open_hover(ui: &Ui, account_id: &str, hovered: bool, pointer: Option<eframe::egui::Pos2>) {
        let layer = pointer.and_then(|pos| ui.ctx().layer_id_at(pos));
        let layer = layer.map_or_else(|| "none".to_owned(), |layer| format!("{layer:?}"));
        let clip = ui.clip_rect();
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, (bool, Option<eframe::egui::Pos2>, Rect, String)>>(id("signature-open-hover"))
                .insert(account_id.to_owned(), (hovered, pointer, clip, layer));
        });
    }

    pub fn signature_open_hover_state(ctx: &Context, account_id: &str) -> Option<(bool, Option<eframe::egui::Pos2>, Rect, String)> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, (bool, Option<eframe::egui::Pos2>, Rect, String)>>(id("signature-open-hover")))
            .and_then(|rows| rows.get(account_id).cloned())
    }

    pub fn signature_editor(ui: &Ui, account_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("signature-editors")).insert(account_id.to_owned(), rect);
        });
    }

    pub fn signature_editor_rect(ctx: &Context, account_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("signature-editors"))).and_then(|rows| rows.get(account_id).copied())
    }

    pub fn signature_save(ui: &Ui, account_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("signature-save-buttons")).insert(account_id.to_owned(), rect);
        });
    }

    pub fn signature_save_rect(ctx: &Context, account_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("signature-save-buttons"))).and_then(|rows| rows.get(account_id).copied())
    }

    pub fn signature_cancel(ui: &Ui, account_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("signature-cancel-buttons")).insert(account_id.to_owned(), rect);
        });
    }

    pub fn signature_cancel_rect(ctx: &Context, account_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("signature-cancel-buttons"))).and_then(|rows| rows.get(account_id).copied())
    }

    pub fn composer_scroll(ui: &Ui, offset: f32, viewport: Rect, content: Vec2) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("composer-scroll-offset"), offset);
            data.insert_temp(id("composer-scroll-view"), viewport);
            data.insert_temp(id("composer-scroll-content"), content);
        });
    }

    pub fn composer_scroll_state(ctx: &Context) -> Option<(f32, Rect, Vec2)> {
        ctx.data(|data| {
            let offset = data.get_temp(id("composer-scroll-offset"))?;
            let viewport = data.get_temp(id("composer-scroll-view"))?;
            let content = data.get_temp(id("composer-scroll-content"))?;
            Some((offset, viewport, content))
        })
    }

    pub fn list_width(ui: &Ui, content: f32, panel: f32) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("list-content-width"), content);
            data.insert_temp(id("list-panel-width"), panel);
        });
    }

    pub fn list_widths(ctx: &Context) -> Option<(f32, f32)> {
        ctx.data(|data| {
            let content = data.get_temp(id("list-content-width"))?;
            let panel = data.get_temp(id("list-panel-width"))?;
            Some((content, panel))
        })
    }

    pub fn date_column(ui: &Ui, visible: bool) {
        ui.ctx().data_mut(|data| data.insert_temp(id("date-column"), visible));
    }

    pub fn date_column_visible(ctx: &Context) -> Option<bool> {
        ctx.data(|data| data.get_temp(id("date-column")))
    }

    /// One fact block per message row: the accessible name and whether it held real keyboard focus
    /// this frame.
    pub fn row_facts(ctx: &Context, message_id: &str, label: &str, focused: bool) {
        ctx.data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, (String, bool)>>(id("row-facts"))
                .insert(message_id.to_owned(), (label.to_owned(), focused));
        });
    }

    pub fn clear_row_facts(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("row-facts"), BTreeMap::<String, (String, bool)>::new()));
        ui.ctx().data_mut(|data| data.insert_temp(id("focus-rings"), Vec::<String>::new()));
        data_star_clear(ui);
    }

    pub fn focus_ring(ctx: &Context, message_id: &str) {
        ctx.data_mut(|data| data.get_temp_mut_or_default::<Vec<String>>(id("focus-rings")).push(message_id.to_owned()));
    }

    pub fn focus_rings(ctx: &Context) -> Vec<String> {
        ctx.data(|data| data.get_temp(id("focus-rings"))).unwrap_or_default()
    }

    fn data_star_clear(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("star-rects"), BTreeMap::<String, Rect>::new()));
    }

    pub fn row_facts_for(ctx: &Context, message_id: &str) -> Option<(String, bool)> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, (String, bool)>>(id("row-facts"))).and_then(|rows| rows.get(message_id).cloned())
    }

    pub fn focused_rows(ctx: &Context) -> Vec<String> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, (String, bool)>>(id("row-facts")))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, (_, focused))| focused.then_some(id))
            .collect()
    }

    pub fn star(ui: &Ui, message_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("star-rects")).insert(message_id.to_owned(), rect);
        });
    }

    pub fn star_rect(ctx: &Context, message_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("star-rects"))).and_then(|stars| stars.get(message_id).copied())
    }

    pub fn subject_width(ui: &Ui, width: f32) {
        ui.ctx().data_mut(|data| {
            let current = data.get_temp::<f32>(id("subject-width")).unwrap_or(f32::MAX);
            data.insert_temp(id("subject-width"), current.min(width));
        });
    }

    pub fn reset_subject_width(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("subject-width"), f32::MAX));
    }

    pub fn subject_width_value(ctx: &Context) -> Option<f32> {
        ctx.data(|data| data.get_temp(id("subject-width")))
    }

    pub fn bulk_bar(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("bulk-bar"), Some(rect)));
    }

    pub fn clear_bulk_bar(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("bulk-bar"), Option::<Rect>::None));
    }

    pub fn bulk_bar_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<Option<Rect>>(id("bulk-bar")))?
    }

    #[derive(Clone)]
    pub struct EmptyProbe {
        pub title: String,
        pub body: String,
        pub primary: Option<String>,
        pub secondary: Option<String>,
    }

    pub fn empty_state(ui: &Ui, title: &str, body: &str, primary: Option<&str>, secondary: Option<&str>) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(
                id("empty"),
                Some(EmptyProbe {
                    title: title.to_owned(),
                    body: body.to_owned(),
                    primary: primary.map(str::to_owned),
                    secondary: secondary.map(str::to_owned),
                }),
            );
        });
    }

    pub fn clear_empty_state(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("empty"), Option::<EmptyProbe>::None));
    }

    pub fn empty_state_view(ctx: &Context) -> Option<EmptyProbe> {
        ctx.data(|data| data.get_temp::<Option<EmptyProbe>>(id("empty")))?
    }

    pub fn loading(ui: &Ui, rect: Rect, alpha: f32) {
        ui.ctx().data_mut(|data| data.insert_temp(id("loading"), Some((rect, alpha))));
    }

    pub fn loading_line(ctx: &Context) -> Option<(Rect, f32)> {
        ctx.data(|data| data.get_temp::<Option<(Rect, f32)>>(id("loading")))?
    }

    pub fn clear_loading(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("loading"), Option::<(Rect, f32)>::None));
    }

    pub fn clear_tooltips(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("tooltips"), Vec::<String>::new()));
    }

    pub fn tooltip(ctx: &Context, text: &str) {
        ctx.data_mut(|data| data.get_temp_mut_or_default::<Vec<String>>(id("tooltips")).push(text.to_owned()));
    }

    pub fn tooltips(ctx: &Context) -> Vec<String> {
        ctx.data(|data| data.get_temp(id("tooltips"))).unwrap_or_default()
    }

    pub fn reading_header(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("reading-header"), rect));
    }

    pub fn reading_header_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("reading-header")))
    }

    pub fn reading_pgp(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("reading-pgp"), rect));
    }

    pub fn reading_pgp_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("reading-pgp")))
    }

    pub fn composer_preview_button(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-preview-button"), rect));
    }

    pub fn composer_preview_button_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-preview-button")))
    }

    pub fn composer_pgp(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-pgp"), rect));
    }

    pub fn composer_pgp_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-pgp")))
    }

    pub fn composer_from(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("composer-from"), rect));
    }

    pub fn composer_from_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("composer-from")))
    }

    pub fn settings_pgp_export(ui: &Ui, key_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(Id::new(("settings-pgp-export", key_id)), rect));
    }

    pub fn settings_pgp_export_rect(ctx: &Context, key_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp(Id::new(("settings-pgp-export", key_id))))
    }

    pub fn reading_cc(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("reading-cc"), rect));
    }

    pub fn reading_cc_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("reading-cc")))
    }

    pub fn reading_attachments(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("reading-attachments"), rect));
    }

    pub fn reading_attachments_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("reading-attachments")))
    }

    pub fn reading_body(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("reading-body"), rect));
    }

    pub fn reading_body_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("reading-body")))
    }

    pub fn attachment_save_enabled(ui: &Ui, attachment_id: &str, enabled: bool) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, bool>>(id("attachment-save-enabled")).insert(attachment_id.to_owned(), enabled);
        });
    }

    pub fn attachment_save_is_enabled(ctx: &Context, attachment_id: &str) -> Option<bool> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, bool>>(id("attachment-save-enabled"))).and_then(|rows| rows.get(attachment_id).copied())
    }

    pub fn settings_section(ui: &Ui, name: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("settings-sections")).insert(name.to_owned(), rect);
        });
    }

    pub fn settings_sections(ctx: &Context) -> BTreeMap<String, Rect> {
        ctx.data(|data| data.get_temp(id("settings-sections"))).unwrap_or_default()
    }

    pub fn settings_footer(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("settings-footer"), rect));
    }

    pub fn settings_footer_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("settings-footer")))
    }

    pub fn settings_copy(ui: &Ui, key: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("settings-copy")).insert(key.to_owned(), rect);
        });
    }

    pub fn settings_copy_rect(ctx: &Context, key: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("settings-copy"))).and_then(|rows| rows.get(key).copied())
    }

    pub fn settings_action(ui: &Ui, key: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("settings-actions")).insert(key.to_owned(), rect);
        });
    }

    pub fn settings_action_rect(ctx: &Context, key: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("settings-actions"))).and_then(|rows| rows.get(key).copied())
    }

    pub fn settings_scroll(ui: &Ui, viewport: Rect, content: Vec2, offset: Vec2) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("settings-scroll-view"), viewport);
            data.insert_temp(id("settings-scroll-content"), content);
            data.insert_temp(id("settings-scroll-offset"), offset.y);
        });
    }

    pub fn settings_scroll_view(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("settings-scroll-view")))
    }

    pub fn settings_scroll_offset(ctx: &Context) -> Option<f32> {
        ctx.data(|data| data.get_temp(id("settings-scroll-offset")))
    }

    pub fn account_remove_button(ui: &Ui, account_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("account-remove-buttons")).insert(account_id.to_owned(), rect);
        });
    }

    pub fn account_remove_button_rect(ctx: &Context, account_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("account-remove-buttons"))).and_then(|rows| rows.get(account_id).copied())
    }

    #[derive(Clone, Copy)]
    pub struct DialogButton {
        pub rect: Rect,
        pub id: Id,
    }

    pub fn clear_dialog_buttons(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("dialog-buttons"), BTreeMap::<String, DialogButton>::new()));
    }

    pub fn dialog_button(ui: &Ui, title: &str, primary: bool, button: DialogButton) {
        let key = format!("{title}|{}", if primary { "primary" } else { "secondary" });
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, DialogButton>>(id("dialog-buttons")).insert(key, button);
        });
    }

    pub fn dialog_button_rect(ctx: &Context, title: &str, primary: bool) -> Option<Rect> {
        dialog_button_probe(ctx, title, primary).map(|button| button.rect)
    }

    pub fn dialog_button_focus(ctx: &Context, title: &str, primary: bool) -> Option<Id> {
        dialog_button_probe(ctx, title, primary).map(|button| button.id)
    }

    fn dialog_button_probe(ctx: &Context, title: &str, primary: bool) -> Option<DialogButton> {
        let key = format!("{title}|{}", if primary { "primary" } else { "secondary" });
        ctx.data(|data| data.get_temp::<BTreeMap<String, DialogButton>>(id("dialog-buttons")).and_then(|buttons| buttons.get(&key).copied()))
    }

    pub fn compose_button(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("compose-button"), rect));
    }

    pub fn compose_button_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("compose-button")))
    }

    pub fn palette(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("palette"), rect));
    }

    pub fn palette_rect(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("palette")))
    }

    pub fn message_row(ctx: &Context, message_id: &str, rect: Rect) {
        ctx.data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("message-rows")).insert(message_id.to_owned(), rect);
        });
    }

    pub fn message_rows(ctx: &Context) -> BTreeMap<String, Rect> {
        ctx.data(|data| data.get_temp(id("message-rows"))).unwrap_or_default()
    }

    pub fn clear_message_rows(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("message-rows"), BTreeMap::<String, Rect>::new()));
    }

    pub fn clear_menu_rows(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("menu-rows"), BTreeMap::<String, Rect>::new()));
    }

    pub fn menu_row(ui: &Ui, label_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("menu-rows")).insert(label_id.to_owned(), rect);
        });
    }

    pub fn menu_rows(ctx: &Context) -> BTreeMap<String, Rect> {
        ctx.data(|data| data.get_temp(id("menu-rows"))).unwrap_or_default()
    }

    pub fn editor_save(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("editor-save"), rect));
    }

    pub fn saved_at(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("editor-save")))
    }

    pub fn editor_cancel(ui: &Ui, rect: Rect) {
        ui.ctx().data_mut(|data| data.insert_temp(id("editor-cancel"), rect));
    }

    pub fn cancel_at(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("editor-cancel")))
    }

    pub fn clear_actions(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("actions"), Vec::<&'static str>::new()));
    }

    pub fn action(ui: &Ui, action: &'static str) {
        ui.ctx().data_mut(|data| data.get_temp_mut_or_default::<Vec<&'static str>>(id("actions")).push(action));
    }

    pub fn actions(ctx: &Context) -> Vec<&'static str> {
        ctx.data(|data| data.get_temp(id("actions"))).unwrap_or_default()
    }
}

#[cfg(test)]
use compose::{draft_save_error_message, pgp_send_request, send_error_message};
#[cfg(test)]
use list::{list_columns, search_chips, search_error_text, search_meta, show_list_avatars, subject_width, thread_star_menu};
#[cfg(test)]
use reading_pane::{MAX_PREVIEW_TEXT, preview_text};
#[cfg(test)]
use reading_pane::{PendingAttachment, PendingLink, PgpReadingState};
#[cfg(test)]
use screenshot::write_png;

#[cfg(test)]
mod tests;
