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

mod compose;

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

/// Let the layout and the font atlas settle before asking for the surface.
const SCREENSHOT_SETTLE_FRAMES: u32 = 4;
/// A visible window replies within a frame or two. This bound turns a hidden or minimized window
/// into an actionable error instead of a hung process.
const SCREENSHOT_WAIT_FRAMES: u32 = 120;
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
    /// A link waiting for the confirmation sheet. The URL has already passed the allowlist.
    pending_link: Option<PendingLink>,
    /// The block view of the message currently open. Bodies are immutable while a message is open,
    /// so the pane parses once per body instead of once per frame. A sync refresh that replaces the
    /// row changes the fragment and invalidates this.
    body_cache: Option<BodyCache>,
    /// A dangerous attachment waiting for the explicit warning.
    attachment_warning: Option<PendingAttachment>,
    /// An attachment whose chosen path already exists and waits for overwrite confirmation.
    attachment_overwrite: Option<PendingAttachment>,
    /// Stable Save-button ids so a headless test can drive the keyboard path.
    attachment_save_ids: BTreeMap<String, Id>,
    /// A bounded, display-only attachment preview. The bytes are dropped when the sheet closes.
    attachment_preview: Option<AttachmentPreviewState>,
    /// A message override used only by deterministic screenshots (HTML plus attachment fixtures).
    screenshot_message: Option<Message>,
    /// The invite card shown for [`Self::screenshot_message`], when the scene is an invite.
    screenshot_invite: Option<CalendarInviteView>,
    /// One-entry cache for the parsed invite of the open message. The part cannot change while the
    /// message is open; a new selection replaces the entry and no re-read happens per frame.
    invite_cache: Option<(String, Option<CalendarInviteView>)>,
    /// True while the reading pane shows a received, valid invite that can be answered. The three
    /// invite commands read this rule; the card never carries its own enabled state.
    invite_ready: bool,
    /// The PGP verdict and any display-only decrypted body for the open message. Keyed by message
    /// id so a list refresh cannot show the previous message's badge.
    pgp_reading: Option<PgpReadingState>,
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
    /// The unsubscribe choices for the open message. Empty means the header was absent or invalid.
    unsubscribe_info: Option<UnsubscribeInfo>,
    /// The message whose headers were already checked; a negative result is cached too.
    unsubscribe_checked: Option<String>,
    /// The choice sheet is open. It never sends or opens anything until the person picks a target.
    show_unsubscribe: bool,
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
            pending_link: None,
            body_cache: None,
            attachment_warning: None,
            attachment_overwrite: None,
            attachment_save_ids: BTreeMap::new(),
            attachment_preview: None,
            screenshot_message: None,
            screenshot_invite: None,
            invite_cache: None,
            invite_ready: false,
            pgp_reading: None,
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
            unsubscribe_info: None,
            unsubscribe_checked: None,
            show_unsubscribe: false,
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

    /// Apply the state named on the command line. Every scene is deterministic: no timer, no
    /// pointer and no focus side effect may change it between the first frame and the capture.
    /// The settings capture shows a live-looking but deterministic background state. It never reads
    /// the registry or talks to a daemon: a docs image must not depend on the machine that builds it.
    fn seed_background_rows(&mut self) {
        let daemon = background::DaemonView {
            tray: true,
            notify_enabled: true,
            notify_previews: false,
            debounce_secs: 60,
            tray_available: true,
            tray_running: true,
        };
        let autostart = background::AutostartView {
            enabled: true,
            command: Some(r"C:\Program Files\Postino\postino-daemon.exe --store C:\Users\me\AppData\Roaming\Postino\mail.db".to_owned()),
        };
        self.background_rows = Some(background::Rows::read(Ok(daemon), Ok(autostart)));
    }

    fn seed_screenshot(&mut self, state: ScreenshotState) {
        self.screenshot_message = None;
        self.screenshot_invite = None;
        self.invite_cache = None;
        self.invite_ready = false;
        match state {
            ScreenshotState::Shell => {}
            ScreenshotState::Reading => {
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id);
                    // Show the steady state after the 600 ms read delay: message read, no undo
                    // toast racing the capture.
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                }
            }
            ScreenshotState::ReadingFull => {
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id);
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    self.reading_full = true;
                }
            }
            ScreenshotState::EmptyUnread => {
                let ids = self
                    .engine
                    .messages()
                    .iter()
                    .filter(|message| message.folder_id == "inbox" && message.unread)
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    let _ = self.op("message_flag", json!({ "id": id, "unread": false, "starred": null }));
                }
                self.refresh_labels();
                self.unread_only = true;
                self.clear_selection();
            }
            ScreenshotState::EmptyAttachments => {
                // Inbox mail with an attachment is moved out, so the attachments filter is the only
                // reason the list is empty. The copy names the filter, not an empty folder.
                let ids = self
                    .engine
                    .messages()
                    .iter()
                    .filter(|message| message.folder_id == "inbox" && message.has_attachments)
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    let _ = self.op("message_move", json!({ "id": id, "folder": "archive" }));
                }
                self.refresh_labels();
                self.attachments_only = true;
                self.clear_selection();
            }
            ScreenshotState::EmptyLabelUnread => {
                self.select_label("lbl_1".to_owned());
                let ids = self
                    .engine
                    .messages()
                    .iter()
                    .filter(|message| message.unread && message.labels.iter().any(|id| id == "lbl_1"))
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    let _ = self.op("message_flag", json!({ "id": id, "unread": false, "starred": null }));
                }
                self.refresh_labels();
                self.unread_only = true;
            }
            ScreenshotState::Compose => {
                let mut composer = ComposerState::new(ComposeMode::New);
                composer.to = "Giulia Ferri <giulia.ferri@example.com>".to_owned();
                composer.subject = "Preventivo sito web — bozza finale".to_owned();
                composer.body = "Ciao Giulia,\n\nti confermo il preventivo per il sito: 2.400 € + IVA.".to_owned();
                composer.attachments = vec![
                    Attachment {
                        id: "att_screenshot".to_owned(),
                        filename: "preventivo-postino.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 248_320,
                    },
                    Attachment {
                        id: "att_screenshot_2".to_owned(),
                        filename: "planimetria-ufficio.png".to_owned(),
                        mime: "image/png".to_owned(),
                        size: 1_048_576,
                    },
                ];
                composer.saved_at = Some(format_time(self.now_utc));
                self.start_composer(composer);
            }
            ScreenshotState::Palette => {
                self.show_palette = true;
                self.palette_focus = true;
                // A filtered palette is more useful than an empty query in a docs shot: it shows
                // the search, a handful of rows and the disabled reason line together.
                self.palette_query = "etichetta".to_owned();
                self.palette_selected = 0;
            }
            ScreenshotState::Labels => {
                // Keep the list visible at every size: the 980×600 shot must be able to prove the
                // header and "Mostra tutte" are not clipped. The reading-pane chips stay in the
                // label_menu scene.
                self.label_filter = self.labels_cache.iter().find(|label| label.name == "Lavoro").map(|label| label.id.clone());
                self.label_folder = None;
            }
            ScreenshotState::LabelEditor => {
                self.start_label_create();
                if let Some(editor) = self.label_editor.as_mut() {
                    editor.text = "Ufficio".to_owned();
                    editor.focus = true;
                }
            }
            ScreenshotState::LabelMenu => {
                self.label_filter = self.labels_cache.iter().find(|label| label.name == "Lavoro").map(|label| label.id.clone());
                self.label_folder = None;
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id.clone());
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    // The captured targets are the visible message; the Lavoro chip stays visible in
                    // the reading pane behind the modal scrim.
                    self.open_label_menu(vec![id]);
                }
            }
            ScreenshotState::OnboardingWelcome => self.seed_onboarding_screenshot(Stage::Welcome, ProviderChoice::Aruba, None, None, None),
            ScreenshotState::OnboardingProvider => self.seed_onboarding_screenshot(Stage::Provider, ProviderChoice::Gmail, None, None, None),
            ScreenshotState::OnboardingForm => self.seed_onboarding_screenshot(Stage::Credentials, ProviderChoice::Aruba, None, None, None),
            ScreenshotState::OnboardingError => self.seed_onboarding_screenshot(
                Stage::Credentials,
                ProviderChoice::Aruba,
                None,
                Some(ConnectionError {
                    kind: ErrorKind::Password,
                    message: "La password non è stata accettata da imaps.aruba.it. Controlla di averla digitata bene; se hai attivo il secondo fattore, serve una password per le app.".to_owned(),
                    focus: FocusField::Password,
                }),
                None,
            ),
            ScreenshotState::OnboardingFolders => self.seed_onboarding_screenshot(Stage::Folders, ProviderChoice::Aruba, Some(demo_folders_preview()), None, None),
            ScreenshotState::OnboardingSync => self.seed_onboarding_screenshot(
                Stage::Sync,
                ProviderChoice::Aruba,
                Some(demo_folders_preview()),
                None,
                Some(SyncProgress { folders_done: 3, folders_total: 7, messages: 128 }),
            ),
            ScreenshotState::Mailbox => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
            }
            ScreenshotState::Offline => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 87 * 60 * 1000;
                self.sync_view = SyncView::Offline { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id);
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                }
            }
            ScreenshotState::ReadingAttachments | ScreenshotState::LinkConfirm => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id.clone());
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    if let Some(mut message) = self.engine.message(&id) {
                        message.body_text = "Ciao Federico,\n\nho caricato il PDF del preventivo nella cartella condivisa.\n\nDettaglio: https://example.com/preventivo/postino-2026?token=anteprima\n\nScrivimi a mailto:giulia.ferri@example.com.\n\nGrazie,\nGiulia"
                            .to_owned();
                        // Stored truth, not a live tracker: the sanitizer removed the remote image and
                        // sync stored the count. This HTML is already the sanitized form the block
                        // renderer consumes: a heading, emphasis, a list, two safe anchors and a local
                        // `cid:` image shown as its alt text, with no remote `src` and no script.
                        message.body_html = Some(
                            concat!(
                                "<h1>Preventivo postino 2026</h1>",
                                "<p>Ciao Federico, ho caricato il <strong>PDF del preventivo</strong> nella cartella condivisa.</p>",
                                "<ul><li>Preventivo firmato</li><li>Dettaglio dei costi</li></ul>",
                                "<p><a href=\"https://example.com/preventivo/postino-2026?token=anteprima\">Apri il preventivo</a> oppure scrivimi a <a href=\"mailto:giulia.ferri@example.com\">giulia.ferri@example.com</a>.</p>",
                                "<p><img src=\"cid:logo@postino\" alt=\"Logo Postino\"></p>",
                            )
                            .to_owned(),
                        );
                        message.remote_images = 1;
                        if message.attachments.is_empty() {
                            message.attachments.push(Attachment {
                                id: "att_screenshot".to_owned(),
                                filename: "preventivo-postino.pdf".to_owned(),
                                mime: "application/pdf".to_owned(),
                                size: 248_320,
                            });
                            message.summary.has_attachments = true;
                        }
                        self.screenshot_message = Some(message);
                    }
                }
                if state == ScreenshotState::LinkConfirm {
                    self.pending_link = Some(PendingLink {
                        url: "https://example.com/preventivo/postino-2026?token=anteprima".to_owned(),
                        host: "example.com".to_owned(),
                        recipient: None,
                        mailto: false,
                        idn: false,
                        non_ascii: false,
                        backslash: false,
                        focus_cancel: false,
                    });
                }
            }
            ScreenshotState::ReadingInvite => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id.clone());
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    if let Some(mut message) = self.engine.message(&id) {
                        message.summary.subject = "Revisione preventivo".to_owned();
                        message.summary.from = Address::new("Giulia Ferri", "giulia.ferri@example.com");
                        message.body_text = "Ciao Federico,\n\nti confermo la revisione del preventivo giovedì mattina in studio. Se non riesci, rispondi pure con un tentativo e troviamo un'altra data.\n\nGiulia"
                            .to_owned();
                        message.body_html = None;
                        message.remote_images = 0;
                        message.attachments.clear();
                        message.summary.has_attachments = false;
                        self.screenshot_message = Some(message);
                        self.screenshot_invite = Some(CalendarInviteView {
                            method: "request".to_owned(),
                            summary: "Revisione preventivo".to_owned(),
                            organizer: Some("giulia.ferri@example.com".to_owned()),
                            attendees: vec!["giulia.ferri@example.com".to_owned(), "federico@example.it".to_owned()],
                            start: Some("2026-11-05T09:00:00".to_owned()),
                            end: Some("2026-11-05T10:00:00".to_owned()),
                            location: Some("Studio Postino, Via Roma 1".to_owned()),
                            recurrence: false,
                            timezone: Some("Europe/Rome".to_owned()),
                            valid: true,
                            invalid_reason: None,
                            cancelled: false,
                            can_reply: true,
                            warnings: vec!["Fuso orario dichiarato ma non applicato: gli orari sono quelli scritti nell'invito.".to_owned()],
                        });
                    }
                }
            }
            ScreenshotState::Settings => {
                // A settings capture must not inherit a selected/read message: the unread badge and
                // the inbox total have to be the same at every window size.
                self.clear_selection();
                self.close_reading();
                self.demo = false;
                self.seed_background_rows();
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.store_path = Some(PathBuf::from("C:/Users/demo/AppData/Local/Postino/mail.db"));
                self.show_accounts_dialog = true;
                self.settings_focus_pending = false;
                self.status.clear();
            }
            ScreenshotState::SettingsSignature => {
                self.clear_selection();
                self.close_reading();
                self.demo = false;
                self.seed_background_rows();
                self.show_accounts_dialog = true;
                self.settings_focus_pending = false;
                if let Some(account) = self.engine.accounts().first().cloned() {
                    self.signature_editor = Some(SignatureEditor {
                        account: account.id,
                        text: "Federico\nStudio Postino · example.it\n+39 000 000 0000".to_owned(),
                        focus: false,
                        error: None,
                    });
                }
                self.status.clear();
            }
            ScreenshotState::Outbox => {
                self.outbox = demo_outbox(self.now_utc.timestamp_millis());
                self.outbox_open = true;
                self.outbox_loaded = true;
                self.status.clear();
            }
            ScreenshotState::Search => {
                self.query = "from:giulia is:unread".to_owned();
                self.recording_recent = false;
                self.commit_search();
                self.status.clear();
            }
            ScreenshotState::SearchError => {
                self.query = "subject:\"unclosed".to_owned();
                self.recording_recent = false;
                self.commit_search();
                self.search_error = Some(("unclosed_quote".to_owned(), 8));
                self.status.clear();
            }
            ScreenshotState::Threads => {
                self.threads_view = true;
                self.clear_selection();
                self.close_reading();
                self.clear_thread_selection();
                self.threads_cache = None;
                self.list_cache = None;
                self.status.clear();
                self.toast = None;
            }
            ScreenshotState::Conversation => {
                self.threads_view = true;
                self.select_thread("thr_preventivo".to_owned());
                self.now = 1.0;
                self.maybe_complete_read();
                self.toast = None;
                self.status.clear();
            }
            ScreenshotState::SavedSearches => {
                if !self.engine.accounts().is_empty() && self.saved_searches.is_empty() {
                    for (name, query) in [("Da Giulia", "from:giulia"), ("Fatture", "label:Fatture"), ("Non letti", "is:unread")] {
                        let _ = self.op("saved_search_create", json!({ "name": name, "query": query }));
                    }
                }
                self.refresh_saved_searches();
                self.threads_view = false;
                if let Some(saved) = self.saved_searches.iter().find(|saved| saved.name == "Da Giulia").cloned() {
                    self.apply_saved_search(&saved.id);
                }
                self.status.clear();
                self.toast = None;
            }
            ScreenshotState::Rules | ScreenshotState::RuleEditor | ScreenshotState::RulePreview => {
                let account = self.engine.accounts().first().map(|account| account.id.clone());
                if let Some(account) = account {
                    let empty = self.engine.rules_list(Some(&account)).is_ok_and(|rules| rules.is_empty());
                    if empty {
                        for (name, query, actions, priority) in [
                            ("Da Giulia", "from:giulia", json!([{ "kind": "label", "label": "lbl_1" }]), 10),
                            ("Fatture", "subject:fattura", json!([{ "kind": "label", "label": "lbl_3" }]), 20),
                            ("Newsletter", "subject:newsletter", json!([{ "kind": "move", "folder": "archive" }]), 30),
                        ] {
                            let _ = self.op(
                                "rules_create",
                                json!({ "name": name, "query": query, "actions": actions, "priority": priority, "account": account }),
                            );
                        }
                        let _ = self.op("rules_run", json!({ "account": account, "scope": "new" }));
                    }
                    self.active_account = Some(account);
                }
                self.open_rules(false);
                if state == ScreenshotState::RuleEditor || state == ScreenshotState::RulePreview {
                    let actions = if state == ScreenshotState::RuleEditor {
                        // The 980x600 editor scrolls after one action row; keep the seeded screenshot
                        // and its seed in step instead of showing a second row behind the fold.
                        vec![postino_core::RuleAction::Move { folder: "archive".to_owned() }]
                    } else {
                        vec![postino_core::RuleAction::Move { folder: "archive".to_owned() }, postino_core::RuleAction::Read]
                    };
                    if let Some(page) = self.rules_page.as_mut() {
                        page.seed_editor(&self.engine, "Fatture e ricevute", "subject:fattura", &actions);
                    }
                    if state == ScreenshotState::RulePreview
                        && let Some(page) = self.rules_page.as_mut()
                    {
                        page.preview_now(&mut self.engine);
                    }
                }
                self.status.clear();
                self.toast = None;
            }
        }
    }

    fn seed_onboarding_screenshot(
        &mut self,
        stage: Stage,
        provider: ProviderChoice,
        preview: Option<FoldersPreview>,
        error: Option<ConnectionError>,
        sync: Option<SyncProgress>,
    ) {
        self.engine = Engine::empty();
        self.demo = false;
        self.store = None;
        self.store_path = None;
        self.writer_lock = None;
        self.active_account = None;
        self.folder.clear();
        self.labels_cache.clear();
        self.clear_selection();
        self.close_reading();
        self.sync_view = SyncView::Idle { last_sync: None };
        self.last_sync = None;
        self.sync_worker = None;
        self.boot_failure = None;
        self.status.clear();
        self.onboarding = Some(Onboarding::screenshot(stage, provider, preview, error, sync));
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
            self.attachment_preview = None;
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
        self.attachment_preview = None;
        self.unsubscribe_info = None;
        self.unsubscribe_checked = None;
        self.show_unsubscribe = false;
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
        if self.show_unsubscribe {
            if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
                self.show_unsubscribe = false;
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
            || self.pending_link.is_some()
            || self.attachment_warning.is_some()
            || self.attachment_overwrite.is_some()
            || self.attachment_preview.is_some()
            || self.pgp_prompt.is_some()
            || self.composer_preview
            || self.pgp_import.is_some()
            || self.pgp_delete.is_some()
            || self.template_editor.is_some()
            || self.template_delete.is_some()
            || self.identity_editor.is_some()
            || self.identity_delete.is_some()
            || self.show_unsubscribe
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

    /// The quiet loading line under the toolbar. It is painted only after [`LOADING_DELAY`]; the
    /// fade uses the same global animation switch, so reduced motion draws it at full alpha at once.
    fn loading_line(&mut self, ui: &Ui) {
        #[cfg(test)]
        probe::clear_loading(ui);
        if !self.loading_visible() {
            return;
        }
        let content = ui.ctx().content_rect();
        let chrome = if self.custom_chrome { ui_size::TITLE_BAR } else { 0.0 };
        let top = content.top() + chrome + ui_size::TOOLBAR;
        let rect = Rect::from_min_size(pos2(content.left(), top), Vec2::new(content.width(), 2.0));
        let progress = ui.ctx().animate_bool_with_time(Id::new("postino-loading"), true, theme::motion_seconds(ui.ctx(), 0.12));
        let t = theme::tokens(self.dark);
        let color = if self.dark { t.gold } else { t.gold_ink };
        let alpha = u8::try_from((progress.clamp(0.0, 1.0) * 255.0).round() as i32).unwrap_or(255);
        // A foreground-layer painter keeps the line above the list and sidebar panels, which are
        // added after this call and would otherwise paint over a hairline at their top edge.
        let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Middle, Id::new("postino-loading-line")));
        painter.rect_filled(rect, CornerRadius::same(0), Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha));
        #[cfg(test)]
        probe::loading(ui, rect, progress);
        ui.ctx().request_repaint();
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

    fn title_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top("titlebar")
            .exact_size(ui_size::TITLE_BAR)
            .frame(Frame::new().fill(theme::FOREST).inner_margin(Margin::symmetric(inset::MD, 0)))
            .show_separator_line(false)
            .show(ui, |ui| {
                let rect = ui.max_rect();
                let controls_width = 3.0 * 46.0;
                let drag_rect = Rect::from_min_max(rect.min, egui::pos2(rect.max.x - controls_width, rect.max.y));
                let drag = ui.interact(drag_rect, ui.id().with("titlebar-drag"), Sense::click_and_drag());
                if drag.drag_started() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    let maximized = ui.ctx().input(|input| input.viewport().maximized.unwrap_or(false));
                    ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                }
                let _ = drag.on_hover_cursor(CursorIcon::Grab);

                let controls_rect = Rect::from_min_max(egui::pos2(rect.max.x - controls_width, rect.min.y), rect.max);
                ui.scope_builder(UiBuilder::new().max_rect(controls_rect).layout(Layout::right_to_left(Align::Center)), |ui| {
                    if window_control(ui, icons::CLOSE, "Chiudi", true).clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                    }
                    let maximized = ui.ctx().input(|input| input.viewport().maximized.unwrap_or(false));
                    if window_control(ui, icons::SQUARE, if maximized { "Ripristina" } else { "Ingrandisci" }, false).clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                    }
                    if window_control(ui, icons::MINUS, "Riduci a icona", false).clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                });
            });
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

    fn toolbar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        egui::Panel::top("toolbar")
            .exact_size(ui_size::TOOLBAR)
            .frame(Frame::new().fill(t.chrome).inner_margin(Margin::symmetric(inset::LG, inset::MD)))
            .show_separator_line(true)
            .show(ui, |ui| {
                let rect = ui.max_rect();
                let total = rect.width();
                let side_width = (total * 0.24).min(260.0);
                let search_width = 420.0_f32.min((total - side_width * 2.0).max(200.0));
                let left_rect = Rect::from_min_max(rect.min, egui::pos2(rect.min.x + side_width, rect.max.y));
                let search_rect = Rect::from_center_size(rect.center(), Vec2::new(search_width, 40.0));
                let right_rect = Rect::from_min_max(egui::pos2(rect.max.x - side_width, rect.min.y), rect.max);

                ui.scope_builder(UiBuilder::new().max_rect(left_rect).layout(Layout::left_to_right(Align::Center)), |ui| {
                    logo(ui, 24.0);
                    ui.add_space(space::SM);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new("postino").font(FontId::new(15.0, theme::display())).color(t.text));
                        ui.label(typography::text(Role::Caption, "posta locale", t.text_faint));
                    });
                });
                ui.scope_builder(UiBuilder::new().max_rect(search_rect).layout(Layout::left_to_right(Align::Center)), |ui| {
                    let placeholder = self.search_placeholder();
                    let response = SearchField::new(&mut self.query, &placeholder).width(search_width).focus(self.focus_search).show(ui);
                    if self.focus_search {
                        self.focus_search = false;
                    }
                    self.search_focus = response.has_focus();
                    if response.changed() {
                        let typed = self.query.trim().to_owned();
                        self.saved_restore = None;
                        self.saved_folder_scope = false;
                        self.saved_error = None;
                        self.applied_saved = None;
                        if typed.is_empty() {
                            self.clear_search();
                        } else {
                            self.search_debounce_until = Some(self.now + 0.25);
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                        }
                    }
                    if response.has_focus() {
                        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                            self.recording_recent = true;
                            self.commit_search();
                        }
                        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                            self.cancel_search();
                        }
                    }
                });
                ui.scope_builder(UiBuilder::new().max_rect(right_rect).layout(Layout::right_to_left(Align::Center)), |ui| {
                    let compose = Button::primary("Scrivi").icon(icons::COMPOSE).shortcut("C").show(ui);
                    #[cfg(test)]
                    probe::compose_button(ui, compose.rect);
                    if compose.clicked() {
                        self.compose_new();
                    }
                    ui.add_space(space::SM);
                    let sync_label = if self.sync_running() {
                        "Aggiorna la posta · sincronizzazione già in corso"
                    } else if !self.can_add_account() {
                        "Aggiorna la posta · la modalità demo non ha un server"
                    } else {
                        "Aggiorna la posta"
                    };
                    if IconButton::new(icons::SYNC, sync_label).enabled(self.can_add_account()).show(ui).clicked() {
                        self.request_sync();
                    }
                    ui.add_space(space::SM);
                    let theme_icon = if self.dark { icons::SUN } else { icons::MOON };
                    if IconButton::new(theme_icon, "Cambia tema").show(ui).clicked() {
                        let dark = !self.dark;
                        self.set_dark(ui.ctx(), dark);
                    }
                });
            });
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

    fn list_panel(&mut self, ui: &mut Ui) {
        if self.threads_view {
            self.thread_list_panel(ui);
            return;
        }
        let t = theme::tokens(self.dark);
        #[cfg(test)]
        probe::clear_bulk_bar(ui);
        #[cfg(test)]
        probe::clear_empty_state(ui);
        // The panel width left after the sidebar and the reading pane. It includes the list's own
        // margin; the columns are laid out against the content width inside the panel.
        let list_panel_width = ui.available_width();
        let _ = list_panel_width;
        let now_utc = self.now_utc;
        let current_folder_name =
            self.engine.folders().iter().find(|folder| folder.id == self.folder).map_or_else(|| "Posta".to_owned(), |folder| folder.name.clone());
        let narrowed_folder_name =
            self.label_folder.as_deref().and_then(|id| self.engine.folders().iter().find(|folder| folder.id == id)).map(|folder| folder.name.clone());
        let label_filter = self
            .label_filter
            .as_deref()
            .and_then(|id| self.label_summary(id))
            .map(|label| (label.id.clone(), label.name.clone(), label_color(label.color, &t)));
        let (rows, total, unread) = self.visible_messages();
        self.prune_selection(&rows);
        let searching = !self.search_committed.trim().is_empty();
        let saved_account_wide = self.saved_folder_scope && !self.saved_query_folder;
        let search_error = self.search_error.clone();
        let search_capped = self.search_capped;
        let index_progress = self.index_progress.clone();
        let multi = self.selection.len() > 1;
        let mut clear_filter = false;
        // The header always names the scope, so the label counts cannot read as folder-local.
        let scope = if saved_account_wide {
            Some("in tutte le mail".to_owned())
        } else {
            label_filter.as_ref().map(|(_, _, _)| match &narrowed_folder_name {
                Some(name) => format!("in {name}"),
                None => "in tutte le cartelle".to_owned(),
            })
        };
        let meta = if searching {
            search_meta(total, search_capped, scope.as_deref())
        } else {
            match &scope {
                Some(scope) => format!("{} · {} · {scope}", unread_text(unread), messages_text(total)),
                None => format!("{} · {}", unread_text(unread), messages_text(total)),
            }
        };
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL, inset::LG))).show(ui, |ui| {
            if matches!(self.sync_view, SyncView::Offline { .. }) {
                self.offline_banner(ui, &t);
                ui.add_space(space::SM);
            }
            if multi {
                self.bulk_bar(ui, &t);
            } else {
                // The action is reserved first so it can never be clipped; the title and the scope
                // share the remaining width on two lines. At 1280 with the reading pane open the
                // list is about 375 px wide and the scope would not fit beside the title.
                let title = if saved_account_wide {
                    "Tutte le mail".to_owned()
                } else {
                    label_filter.as_ref().map_or(current_folder_name.clone(), |(_, name, _)| name.clone())
                };
                Sides::new().height(40.0).shrink_left().show(
                    ui,
                    |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.horizontal(|ui| {
                                if let Some((_, _, color)) = &label_filter {
                                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                    ui.painter().circle_filled(rect.center(), 3.0, *color);
                                    ui.add_space(space::XS);
                                }
                                ui.add(egui::Label::new(typography::strong(Role::Section, title.as_str(), t.text)).truncate().selectable(false));
                            });
                            let response =
                                ui.add(egui::Label::new(typography::text(Role::Meta, meta.as_str(), t.text_muted)).truncate().selectable(false));
                            response.on_hover_text(meta.as_str());
                        });
                    },
                    |ui| {
                        if label_filter.is_some() && Button::ghost("Mostra tutte").size(ButtonSize::Sm).show(ui).clicked() {
                            clear_filter = true;
                        }
                    },
                );
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    if Chip::new("Tutti").selected(!self.unread_only && !self.attachments_only).show(ui).clicked() {
                        self.unread_only = false;
                        self.attachments_only = false;
                    }
                    if Chip::new("Non letti").selected(self.unread_only).show(ui).clicked() {
                        self.unread_only = !self.unread_only;
                    }
                    if Chip::new("Allegati").icon(icons::PAPERCLIP).selected(self.attachments_only).show(ui).clicked() {
                        self.attachments_only = !self.attachments_only;
                    }
                });
            }
            if searching {
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    ui.label(typography::text(Role::Meta, format!("Risultati per “{}”", self.search_committed), t.text_muted));
                    if Button::ghost("Cancella").size(ButtonSize::Sm).show(ui).clicked() {
                        self.clear_search();
                    }
                });
                let chips = search_chips(&self.search_committed);
                if !chips.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for chip in chips.into_iter().take(4) {
                            if chip.starts_with("NOT ") {
                                ui.label(typography::text(Role::Caption, chip, t.text_muted));
                            } else if Chip::new(chip.as_str()).show(ui).clicked() {
                                self.query = chip;
                                self.recording_recent = true;
                                self.commit_search();
                            }
                        }
                    });
                }
                ui.label(typography::text(
                    Role::Caption,
                    "from: · to: · subject: · label: · folder: · is: · has: · after: · larger: — AND/OR/NOT, \"frase\", prefisso*",
                    t.text_faint,
                ));
            }
            if let Some((code, position)) = &search_error {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, search_error_text(code, *position), t.danger));
            }
            if self.saved_index_pending {
                ui.add_space(space::XS);
                ui.label(typography::text(
                    Role::Meta,
                    "Indice di ricerca in preparazione: la ricerca salvata sarà disponibile tra poco.",
                    t.text_muted,
                ));
            } else if self.saved_error.is_some() {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, "Ricerca salvata non valida: correggila o eliminala.", t.danger));
            }
            if let Some(index) = &index_progress {
                ui.add_space(space::XS);
                let line = index.last_error.as_deref().map_or_else(
                    || format!("Indice di ricerca in preparazione ({}/{})", index.indexed, index.total),
                    |error| format!("Indice di ricerca non disponibile: {error}"),
                );
                ui.label(typography::text(Role::Meta, line, if index.last_error.is_some() { t.danger } else { t.text_muted }));
            }
            if searching
                && rows.len() as u32 >= self.search_limit
                && total > rows.len()
                && Button::ghost("Mostra altri").size(ButtonSize::Sm).show(ui).clicked()
            {
                self.search_limit = self.search_limit.saturating_add(200).min(5_000);
                self.list_cache = None;
            }
            ui.add_space(space::MD);
            if rows.is_empty() && search_error.is_none() && index_progress.is_none() {
                let mut menu_action = None;
                let background = ui.interact(ui.available_rect_before_wrap(), ui.id().with("empty-list-menu"), Sense::click());
                let has_label_filter = label_filter.is_some();
                let sync_ready = self.sync_ready();
                let sync_reason = if self.demo { "La modalità demo non ha un server" } else { "Nessun account da sincronizzare" };
                background.context_menu(|ui| {
                    let items = [
                        menu_item("label_new"),
                        menu_item("label_clear_filter").enabled(has_label_filter).reason("Nessun filtro etichetta attivo"),
                        MenuItem::separator(),
                        MenuItem::new(command_label("sync", "Aggiorna la posta")).icon(icons::SYNC).enabled(sync_ready).reason(sync_reason),
                        menu_item("select_all").enabled(false).reason("Non ci sono messaggi da selezionare"),
                        menu_item("compose"),
                        MenuItem::new("Cancella ricerca").icon(icons::CLOSE).enabled(searching).reason("Nessuna ricerca attiva"),
                    ];
                    match Menu::new(&items).show(ui) {
                        Some(0) => {
                            menu_action = Some(EmptyMenuAction::NewLabel);
                            ui.close();
                        }
                        Some(1) => {
                            menu_action = Some(EmptyMenuAction::ClearLabel);
                            ui.close();
                        }
                        Some(3) if sync_ready => {
                            menu_action = Some(EmptyMenuAction::Sync);
                            ui.close();
                        }
                        Some(5) => {
                            menu_action = Some(EmptyMenuAction::Compose);
                            ui.close();
                        }
                        Some(6) => {
                            menu_action = Some(EmptyMenuAction::ClearSearch);
                            ui.close();
                        }
                        _ => {}
                    }
                });
                let label_name = label_filter.as_ref().map(|(_, name, _)| name.as_str());
                let view = self.empty_view(label_name, scope.as_deref());
                let mut state = EmptyState::new(view.icon, &view.title, &view.body);
                if let Some((label, _)) = view.primary {
                    state = state.action(label);
                }
                if let Some((label, _)) = view.secondary {
                    state = state.secondary(label);
                }
                #[cfg(test)]
                probe::empty_state(ui, &view.title, &view.body, view.primary.map(|(label, _)| label), view.secondary.map(|(label, _)| label));
                let response = state.show(ui);
                let mut action = None;
                if response.primary_clicked() {
                    action = view.primary.map(|(_, action)| action);
                } else if response.secondary_clicked() {
                    action = view.secondary.map(|(_, action)| action);
                }
                if let Some(action) = action {
                    self.apply_empty_action(action);
                }
                match menu_action {
                    Some(EmptyMenuAction::NewLabel) => self.start_label_create(),
                    Some(EmptyMenuAction::ClearLabel) => self.apply_empty_action(EmptyAction::ClearLabel),
                    Some(EmptyMenuAction::Compose) => self.compose_new(),
                    Some(EmptyMenuAction::ClearSearch) => self.clear_search(),
                    Some(EmptyMenuAction::Sync) => self.request_sync(),
                    None => {}
                }
                return;
            }

            let mut clicked = None;
            let mut double_clicked = None;
            let mut star_clicked = None;
            let mut context_clicked = None;
            let mut context_action = None;
            let modifiers = ui.input(|input| input.modifiers);
            let content_width = ui.available_width();
            // The date rule is measured on the list panel, before its margins: the plan names the
            // 499 pt list at 1280×720, of which ~467 pt is content.
            let columns = list_columns(list_panel_width);
            let show_avatars = show_list_avatars(content_width);
            #[cfg(test)]
            probe::list_width(ui, content_width, list_panel_width);
            #[cfg(test)]
            probe::date_column(ui, columns.date.is_some());
            #[cfg(test)]
            probe::clear_tooltips(ui);
            #[cfg(test)]
            probe::clear_message_rows(ui);
            #[cfg(test)]
            probe::clear_row_facts(ui);
            #[cfg(test)]
            probe::reset_subject_width(ui);
            self.row_ids.clear();
            let ctx = ui.ctx().clone();
            let _ = &ctx;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut focus_rects = Vec::new();
                // The table inserts `item_spacing.x` around every column; at five columns that is
                // 40 pt the subject cannot spare. The cells carry their own insets instead.
                ui.spacing_mut().item_spacing.x = 0.0;
                let mut table = egui_extras::TableBuilder::new(ui)
                    .id_salt("message-list")
                    .striped(false)
                    .resizable(false)
                    .sense(Sense::click())
                    .auto_shrink([false, false])
                    .cell_layout(Layout::left_to_right(Align::Center))
                    .column(egui_extras::Column::exact(12.0));
                if show_avatars {
                    table = table.column(egui_extras::Column::exact(30.0));
                }
                table = table.column(egui_extras::Column::exact(columns.sender));
                table = table.column(egui_extras::Column::remainder());
                if let Some(date) = columns.date {
                    table = table.column(egui_extras::Column::exact(date));
                }
                table
                    .column(egui_extras::Column::exact(28.0))
                    .header(24.0, |mut header| {
                        header.col(|_| {});
                        if show_avatars {
                            header.col(|_| {});
                        }
                        header.col(|ui| {
                            ui.add_space(space::XS);
                            ui.add(egui::Label::new(typography::text(Role::Caption, "MITTENTE", t.text_faint)).selectable(false));
                        });
                        header.col(|ui| {
                            ui.add_space(space::XS);
                            ui.add(egui::Label::new(typography::text(Role::Caption, "OGGETTO", t.text_faint)).selectable(false));
                        });
                        // The compact date sits between the subject and the star, as in every mail
                        // client; the header drops it with the column instead of leaving a hole.
                        if columns.date.is_some() {
                            header.col(|ui| {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.add(egui::Label::new(typography::text(Role::Caption, "DATA", t.text_faint)).selectable(false));
                                });
                            });
                        }
                        header.col(|_| {});
                    })
                    .body(|mut body| {
                        for message in &rows {
                            let focused = self.selected.as_deref() == Some(message.id.as_str());
                            let selected = self.selection.contains(&message.id);
                            let foreground = if selected { t.selected_text } else { t.text };
                            let row_font =
                                FontId::new(type_size::BODY_SMALL, if message.unread { theme::medium() } else { FontFamily::Proportional });
                            let mut sender_truncated = false;
                            let mut subject_truncated = false;
                            let date_label = format_when(message.date, now_utc);
                            let accessible = row_accessible_label(message, &date_label, selected);
                            body.row(ui_size::ROW, |mut row| {
                                row.set_selected(selected);
                                row.col(|ui| {
                                    if message.unread {
                                        // Gold ink on ivory; the light gold returns on the forest
                                        // selection where it measures 6.5:1.
                                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                        ui.painter().circle_filled(rect.center(), 3.0, if selected { t.gold } else { t.gold_ink });
                                    }
                                });
                                if show_avatars {
                                    row.col(|ui| {
                                        avatar(
                                            ui,
                                            24.0,
                                            &message.from.initials(),
                                            if selected { GOLD } else { t.panel_alt },
                                            if selected { theme::FOREST } else { t.text },
                                        );
                                    });
                                }
                                row.col(|ui| {
                                    let name =
                                        if message.from.name.trim().is_empty() { message.from.email.as_str() } else { message.from.name.as_str() };
                                    ui.add_space(space::XS);
                                    sender_truncated = !text_fits(ui, name, &row_font, ui.available_width());
                                    let text = RichText::new(name).font(row_font.clone()).color(foreground);
                                    ui.add(egui::Label::new(text).truncate().selectable(false));
                                });
                                row.col(|ui| {
                                    // The cell layout starts content at the top. Centre the two lines
                                    // explicitly so the subject never touches the selected row edge.
                                    let top = ((ui.available_height() - 28.0) * 0.5).max(0.0);
                                    ui.add_space(top);
                                    ui.add_space(space::XS);
                                    #[cfg(test)]
                                    probe::subject_width(ui, ui.available_width());
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        let subject_text =
                                            if message.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.subject.as_str() };
                                        subject_truncated = !text_fits(ui, subject_text, &row_font, ui.available_width());
                                        let subject = RichText::new(subject_text).font(row_font.clone()).color(foreground);
                                        ui.add(egui::Label::new(subject).truncate().selectable(false));
                                        let snippet = RichText::new(message.snippet.as_str()).size(type_size::META).color(if selected {
                                            t.selected_muted
                                        } else {
                                            t.text_muted
                                        });
                                        ui.add(egui::Label::new(snippet).truncate().selectable(false));
                                    });
                                });
                                if columns.date.is_some() {
                                    row.col(|ui| {
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            ui.add(
                                                egui::Label::new(typography::mono(
                                                    date_label.as_str(),
                                                    type_size::CAPTION,
                                                    if selected { t.selected_muted } else { t.text_faint },
                                                ))
                                                .selectable(false),
                                            );
                                        });
                                    });
                                }
                                row.col(|ui| {
                                    let star_label = if message.starred { "Togli la stella" } else { "Segna con stella" };
                                    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                                    if response.hovered() {
                                        let fill = if selected { tokens::mix(t.selected, t.selected_text, 0.12) } else { t.hover };
                                        ui.painter().rect_filled(rect, CornerRadius::same(radius::SM), fill);
                                    }
                                    if response.has_focus() {
                                        tokens::paint_focus_ring_on(ui, rect, radius::SM, &t, selected);
                                    }
                                    let star_color = if message.starred {
                                        if selected { t.gold } else { t.gold_ink }
                                    } else if selected {
                                        t.selected_muted
                                    } else {
                                        t.text_faint
                                    };
                                    let font = if message.starred { icons::font_filled(13.0) } else { icons::font(13.0) };
                                    let glyph = if message.starred { icons::STAR_FILL } else { icons::STAR };
                                    let galley = ui.painter().layout_no_wrap(glyph.to_owned(), font, star_color);
                                    ui.painter().galley(
                                        pos2(rect.center().x - galley.size().x * 0.5, rect.center().y - galley.size().y * 0.5),
                                        galley,
                                        Color32::PLACEHOLDER,
                                    );
                                    if response.clicked()
                                        || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter) || input.key_pressed(Key::Space)))
                                    {
                                        star_clicked = Some(message.id.clone());
                                    }
                                    if response.hovered() {
                                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                    }
                                    #[cfg(test)]
                                    probe::star(ui, &message.id, rect);
                                    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, star_label));
                                    response.on_hover_text(star_label);
                                });
                                let response = row.response();
                                self.row_ids.insert(message.id.clone(), response.id);
                                // The table reports its row rect from the measuring pass, which can be
                                // wider than the list. Clamp it to the visible content for the focus
                                // ring and for the pointer tests.
                                let row_rect = Rect::from_min_max(
                                    response.rect.min,
                                    pos2(response.rect.max.x.min(response.rect.min.x + content_width), response.rect.max.y),
                                );
                                #[cfg(test)]
                                probe::message_row(&ctx, &message.id, row_rect);
                                #[cfg(test)]
                                probe::row_facts(&ctx, &message.id, &accessible, response.has_focus());
                                // Accessibility: every row is a named button. The label carries all
                                // the facts a sighted user reads from the columns plus the Enter
                                // affordance, so a screen reader is not left with "message row".
                                response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, accessible.clone()));
                                if focused && self.scroll_to_selected {
                                    response.scroll_to_me(None);
                                }
                                if response.has_focus() {
                                    // The ring follows real keyboard focus, not the selection: a
                                    // selected-but-unfocused row must not look focused, and Tab can
                                    // land on the list without changing what is selected.
                                    focus_rects.push((row_rect.shrink(1.0), selected));
                                    #[cfg(test)]
                                    probe::focus_ring(&ctx, &message.id);
                                }
                                if response.secondary_clicked() {
                                    context_clicked = Some(message.id.clone());
                                }
                                if response.clicked() {
                                    clicked = Some((message.id.clone(), modifiers));
                                }
                                if double_activated(&response) {
                                    double_clicked = Some(message.id.clone());
                                }
                                let mut tooltip = String::new();
                                if sender_truncated {
                                    let name =
                                        if message.from.name.trim().is_empty() { message.from.email.as_str() } else { message.from.name.as_str() };
                                    tooltip.push_str(name);
                                }
                                if subject_truncated {
                                    if !tooltip.is_empty() {
                                        tooltip.push('\n');
                                    }
                                    tooltip.push_str(if message.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.subject.as_str() });
                                }
                                if !tooltip.is_empty() {
                                    // A tooltip on hover has the usual delay; a keyboard-focused row
                                    // shows it at once, so truncation is never information loss
                                    // without a mouse.
                                    if response.has_focus() && !response.hovered() {
                                        #[cfg(test)]
                                        probe::tooltip(&ctx, &tooltip);
                                        let _ = egui::Tooltip::for_widget(&response).show(|ui| {
                                            ui.label(typography::text(Role::BodySmall, tooltip.as_str(), t.text));
                                        });
                                    }
                                    response.clone().on_hover_text(tooltip);
                                }
                                let is_draft = self
                                    .engine
                                    .folders()
                                    .iter()
                                    .find(|folder| folder.id == message.folder_id)
                                    .is_some_and(|folder| folder.kind == FolderKind::Drafts);
                                let (items, actions) = row_context_menu(message, is_draft);
                                response.context_menu(|ui| {
                                    if let Some(index) = Menu::new(&items).show(ui)
                                        && let Some(Some(action)) = actions.get(index)
                                    {
                                        context_action = Some((message.id.clone(), *action));
                                        ui.close();
                                    }
                                });
                            });
                        }
                    });
                for (rect, on_selected) in focus_rects {
                    crate::ui::tokens::paint_focus_ring_on(ui, rect, radius::CONTROL, &t, on_selected);
                }
            });
            if self.focus_selected_row {
                if let Some(id) = self.selected.as_ref().and_then(|selected| self.row_ids.get(selected)) {
                    ui.ctx().memory_mut(|memory| memory.request_focus(*id));
                    self.focus_selected_row = false;
                } else if self.selected.is_none() {
                    self.focus_selected_row = false;
                }
            }
            self.scroll_to_selected = false;
            if let Some(id) = star_clicked {
                self.toggle_star_for(&id);
            } else {
                if let Some(id) = context_clicked
                    && !self.selection.contains(&id)
                {
                    self.focus_message(id);
                }
                let overlay = reading_overlay(ui.ctx().content_rect().width());
                if let Some(id) = double_clicked {
                    // A double-click is the physical "open and expand": on a wide window the pane
                    // grows to the full content area; on a narrow one the overlay is already full.
                    self.open_message(id);
                    self.reading_full = !overlay;
                } else if let Some((id, modifiers)) = clicked {
                    if modifiers.command {
                        self.toggle_selection(id);
                    } else if modifiers.shift {
                        self.extend_selection(id, &rows);
                    } else {
                        self.open_message(id);
                    }
                }
            }
            if let Some((id, action)) = context_action {
                self.run_row_action(&id, action);
            }
        });
        if clear_filter {
            self.clear_label_filter();
        }
    }

    /// The conversation list. It is a separate surface from the flat message table: fewer columns,
    /// one row per thread, and every action goes through the thread operations or `message_move`.
    fn thread_list_panel(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let current_folder_name =
            self.engine.folders().iter().find(|folder| folder.id == self.folder).map_or_else(|| "Posta".to_owned(), |folder| folder.name.clone());
        let label_filter = self
            .label_filter
            .as_deref()
            .and_then(|id| self.label_summary(id))
            .map(|label| (label.id.clone(), label.name.clone(), label_color(label.color, &t)));
        let (threads, total, unread_threads, unread_messages) = self.visible_threads();
        let searching = !self.search_committed.trim().is_empty();
        let saved_account_wide = self.saved_folder_scope && !self.saved_query_folder;
        let search_error = self.search_error.clone();
        let saved_error = self.saved_error.clone();
        let saved_pending = self.saved_index_pending;
        let index_progress = self.index_progress.clone();
        let mut clear_filter = false;
        let title = if saved_account_wide {
            "Tutte le mail".to_owned()
        } else {
            label_filter.as_ref().map_or(current_folder_name.clone(), |(_, name, _)| name.clone())
        };
        let meta = if searching {
            format!(
                "{} nei risultati · {} nei risultati",
                plural(usize::try_from(total).unwrap_or(usize::MAX), "conversazione", "conversazioni"),
                plural(usize::try_from(unread_messages).unwrap_or(usize::MAX), "messaggio non letto", "messaggi non letti")
            )
        } else {
            format!(
                "{} con messaggi non letti · {}",
                plural(usize::try_from(unread_threads).unwrap_or(usize::MAX), "conversazione", "conversazioni"),
                plural(usize::try_from(total).unwrap_or(usize::MAX), "conversazione", "conversazioni")
            )
        };
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL, inset::LG))).show(ui, |ui| {
            if matches!(self.sync_view, SyncView::Offline { .. }) {
                self.offline_banner(ui, &t);
                ui.add_space(space::SM);
            }
            Sides::new().height(40.0).shrink_left().show(
                ui,
                |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            if let Some((_, _, color)) = &label_filter {
                                let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                ui.painter().circle_filled(rect.center(), 3.0, *color);
                                ui.add_space(space::XS);
                            }
                            ui.add(egui::Label::new(typography::strong(Role::Section, title.as_str(), t.text)).truncate().selectable(false));
                        });
                        ui.add(egui::Label::new(typography::text(Role::Meta, meta.as_str(), t.text_muted)).truncate().selectable(false))
                            .on_hover_text(meta.as_str());
                    });
                },
                |ui| {
                    if label_filter.is_some() && Button::ghost("Mostra tutte").size(ButtonSize::Sm).show(ui).clicked() {
                        clear_filter = true;
                    }
                },
            );
            ui.add_space(space::XS);
            ui.horizontal(|ui| {
                if Chip::new("Tutti").selected(!self.unread_only && !self.attachments_only).show(ui).clicked() {
                    self.unread_only = false;
                    self.attachments_only = false;
                }
                if Chip::new("Non letti").selected(self.unread_only).show(ui).clicked() {
                    self.unread_only = !self.unread_only;
                }
                if Chip::new("Allegati").icon(icons::PAPERCLIP).selected(self.attachments_only).show(ui).clicked() {
                    self.attachments_only = !self.attachments_only;
                }
                if Chip::new("Messaggi").icon(icons::LIST).show(ui).clicked() {
                    self.toggle_threads_view();
                }
            });
            if searching {
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    ui.label(typography::text(Role::Meta, format!("Risultati per “{}”", self.search_committed), t.text_muted));
                    if Button::ghost("Cancella").size(ButtonSize::Sm).show(ui).clicked() {
                        self.cancel_search();
                    }
                });
                let chips = search_chips(&self.search_committed);
                if !chips.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for chip in chips.into_iter().take(4) {
                            if chip.starts_with("NOT ") {
                                ui.label(typography::text(Role::Caption, chip, t.text_muted));
                            } else if Chip::new(chip.as_str()).show(ui).clicked() {
                                self.query = chip;
                                self.recording_recent = true;
                                self.commit_search();
                            }
                        }
                    });
                }
            }
            if let Some((code, position)) = &search_error {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, search_error_text(code, *position), t.danger));
            }
            if saved_pending {
                ui.add_space(space::XS);
                ui.label(typography::text(
                    Role::Meta,
                    "Indice di ricerca in preparazione: la ricerca salvata sarà disponibile tra poco.",
                    t.text_muted,
                ));
            } else if let Some(_error) = &saved_error {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, "Ricerca salvata non valida: correggila o eliminala.", t.danger));
            }
            if let Some(index) = &index_progress {
                ui.add_space(space::XS);
                let line = index.last_error.as_deref().map_or_else(
                    || format!("Indice di ricerca in preparazione ({}/{})", index.indexed, index.total),
                    |error| format!("Indice di ricerca non disponibile: {error}"),
                );
                ui.label(typography::text(Role::Meta, line, if index.last_error.is_some() { t.danger } else { t.text_muted }));
            }
            if self.query.trim().is_empty() && self.search_focus && !self.recent_searches.is_empty() {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "RICERCHE RECENTI", t.text_faint));
                ui.horizontal_wrapped(|ui| {
                    let recent = self.recent_searches.clone();
                    for query in recent {
                        if Chip::new(query.as_str()).show(ui).clicked() {
                            self.query = query;
                            self.recording_recent = true;
                            self.commit_search();
                        }
                    }
                });
            }
            if !searching
                && usize::try_from(total).unwrap_or(usize::MAX) > threads.len()
                && Button::ghost("Carica altre conversazioni").size(ButtonSize::Sm).show(ui).clicked()
            {
                self.thread_list_limit = self.thread_list_limit.saturating_add(200).min(5_000);
                self.threads_cache = None;
                self.list_cache = None;
            }
            ui.add_space(space::MD);
            if threads.is_empty() && search_error.is_none() {
                let label_name = label_filter.as_ref().map(|(_, name, _)| name.as_str());
                let scope = label_filter
                    .as_ref()
                    .map(|_| if saved_account_wide { "in tutte le mail".to_owned() } else { format!("in {current_folder_name}") });
                let view = self.empty_view_in(label_name, scope.as_deref(), true);
                let mut state = EmptyState::new(view.icon, &view.title, &view.body);
                if let Some((label, _)) = view.primary {
                    state = state.action(label);
                }
                if let Some((label, _)) = view.secondary {
                    state = state.secondary(label);
                }
                #[cfg(test)]
                probe::empty_state(ui, &view.title, &view.body, view.primary.map(|(label, _)| label), view.secondary.map(|(label, _)| label));
                let response = state.show(ui);
                let action = if response.primary_clicked() {
                    view.primary.map(|(_, action)| action)
                } else if response.secondary_clicked() {
                    view.secondary.map(|(_, action)| action)
                } else {
                    None
                };
                if let Some(action) = action {
                    self.apply_empty_action(action);
                }
                return;
            }
            let mut action: Option<ThreadRowAction> = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for thread in &threads {
                    let selected = self.selected_thread.as_deref() == Some(thread.id.as_str());
                    let participants = {
                        let mut names = thread
                            .participants
                            .iter()
                            .take(2)
                            .map(|address| if address.name.trim().is_empty() { address.email.clone() } else { address.name.clone() })
                            .collect::<Vec<_>>()
                            .join(", ");
                        if thread.participants.len() > 2 {
                            names.push('…');
                        }
                        names
                    };
                    let meta = format_when(thread.last_date, self.now_utc);
                    let counts = messages_text(usize::try_from(thread.message_count).unwrap_or(usize::MAX));
                    let unread_detail = if thread.unread_count > 0 {
                        format!(" · {}", unread_text(usize::try_from(thread.unread_count).unwrap_or(usize::MAX)))
                    } else {
                        String::new()
                    };
                    let subtitle = format!("{participants} · {counts}{unread_detail}");
                    let subject = thread_subject(&thread.subject);
                    let subject = if subject.trim().is_empty() { "(Nessun oggetto)" } else { subject.as_str() };
                    let starred = thread.starred_count > 0;
                    let row =
                        ListRow::thread(subject, &subtitle, &meta).unread(thread.unread_count > 0).starred(starred).selected(selected).height(48.0);
                    let response = row.show(ui);
                    #[cfg(test)]
                    probe::row(ui, &format!("thread:{}", thread.id), response.rect);
                    #[cfg(test)]
                    probe::thread_star(ui, &thread.id, starred, response.rect);
                    let mut accessible = format!("{subject}. {subtitle}. {meta}.");
                    if thread.unread_count > 0 {
                        accessible.push_str(" Non letta.");
                    }
                    if starred {
                        accessible.push_str(" Con stella.");
                    }
                    if selected {
                        accessible.push_str(" Selezionata.");
                    }
                    accessible.push_str(" Premi Invio per aprire.");
                    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, accessible.clone()));
                    response.clone().on_hover_text(accessible);
                    if response.clicked() || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
                        action = Some(ThreadRowAction::Open(thread.id.clone()));
                    }
                    response.context_menu(|ui| {
                        let (star_id, _) = thread_star_menu(starred);
                        let items = [
                            menu_item("open"),
                            MenuItem::separator(),
                            menu_item("archive"),
                            menu_item("delete").danger(true),
                            MenuItem::separator(),
                            menu_item("read"),
                            menu_item("unread"),
                            menu_item(star_id),
                        ];
                        match Menu::new(&items).show(ui) {
                            Some(0) => {
                                action = Some(ThreadRowAction::Open(thread.id.clone()));
                                ui.close();
                            }
                            Some(2) => {
                                action = Some(ThreadRowAction::Archive(thread.id.clone()));
                                ui.close();
                            }
                            Some(3) => {
                                action = Some(ThreadRowAction::Delete(thread.id.clone()));
                                ui.close();
                            }
                            Some(5) => {
                                action = Some(ThreadRowAction::Flag(thread.id.clone(), Some(false), None));
                                ui.close();
                            }
                            Some(6) => {
                                action = Some(ThreadRowAction::Flag(thread.id.clone(), Some(true), None));
                                ui.close();
                            }
                            Some(7) => {
                                let (_, next) = thread_star_menu(starred);
                                action = Some(ThreadRowAction::Flag(thread.id.clone(), None, Some(next)));
                                ui.close();
                            }
                            _ => {}
                        }
                    });
                }
            });
            if let Some(action) = action {
                match action {
                    ThreadRowAction::Open(id) => self.select_thread(id),
                    ThreadRowAction::Archive(id) => {
                        if self.selected_thread.as_deref() != Some(id.as_str()) {
                            self.select_thread(id);
                        }
                        self.move_selected(FolderKind::Archive, "Conversazione archiviata", "Conversazioni archiviate");
                    }
                    ThreadRowAction::Delete(id) => {
                        if self.selected_thread.as_deref() != Some(id.as_str()) {
                            self.select_thread(id);
                        }
                        self.move_selected(FolderKind::Trash, "Conversazione spostata nel cestino", "Conversazioni spostate nel cestino");
                    }
                    ThreadRowAction::Flag(id, unread, starred) => {
                        if self.selected_thread.as_deref() != Some(id.as_str()) {
                            self.load_thread_page(id, false, None);
                        }
                        self.set_thread_flag(unread, starred);
                    }
                }
            }
        });
        if clear_filter {
            self.clear_label_filter();
        }
    }

    /// `full` is the overlay/full-screen path: the reading pane covers the list and offers
    /// "Torna alla posta". It is used for the explicit full-screen mode and below
    /// [`READING_PANE_MIN_WIDTH`], where a side pane cannot keep a usable list.
    fn reading_pane(&mut self, ui: &mut Ui, full: bool) {
        if !self.reading_open {
            return;
        }
        let t = theme::tokens(self.dark);
        if full {
            egui::CentralPanel::default()
                .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(inset::XXL, inset::XL)))
                .show(ui, |ui| self.reading_content(ui, &t, true));
        } else {
            let available = ui.ctx().content_rect().width();
            let default_width = reading_pane_width_with_ratio(available, self.reading_ratio);
            let max_width = reading_pane_max_width(available);
            let response = egui::Panel::right("reading")
                .resizable(true)
                .default_size(default_width)
                .min_size(READING_PANE_MIN_SIZE)
                .max_size(max_width)
                .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(inset::LG, inset::XL)))
                .show(ui, |ui| self.reading_content(ui, &t, false));
            // Remember the width the user dragged to. The fraction is relative to the width left
            // by the sidebar, so a bigger window keeps the same split instead of the same pixels.
            let remaining = (available - ui_size::SIDEBAR).max(1.0);
            let ratio = (response.response.rect.width() / remaining).clamp(0.1, 0.9);
            if (ratio - self.reading_ratio).abs() > 0.004 {
                self.reading_ratio = ratio;
                self.mark_settings_dirty();
            }
        }
    }

    /// The pinned attachment strip. It sits below the body scroll and above the status bar so the
    /// attachment row and its Save button are visible at 980×600 even when the body is long.
    fn reading_attachment_strip(&mut self, ui: &mut Ui, message: &Message, t: &Tokens, height: f32) -> Option<AttachmentStripAction> {
        let mut requested = None;
        let mut save_ids = BTreeMap::new();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        // The strip covers the last body line instead of letting it show through: the body scroll is
        // bounded above it, but a line at the boundary can still paint its descenders.
        ui.painter().rect_filled(rect, CornerRadius::same(0), t.panel);
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, t.border));
        let inner = rect.shrink2(Vec2::new(0.0, f32::from(inset::SM)));
        ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
            #[cfg(test)]
            probe::reading_attachments(ui, rect);
            ui.horizontal(|ui| {
                ui.label(typography::strong(Role::Caption, "ALLEGATI", t.text_faint));
                if self.demo {
                    ui.label(typography::text(Role::Caption, "La modalità demo non ha allegati reali", t.text_faint));
                }
            });
            ui.add_space(space::XS);
            egui::ScrollArea::vertical()
                .id_salt(("reading-attachments-scroll", message.summary.id.as_str()))
                .max_height(ui.available_height().max(20.0))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for attachment in &message.attachments {
                        let mut save_clicked = false;
                        let mut preview_clicked = false;
                        let display_name = reading::safe_filename(&attachment.filename);
                        Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::MD)).show(
                            ui,
                            |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(icons::icon(icons::PAPERCLIP, 16.0, t.text_muted));
                                    ui.add_space(space::SM);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        // Show the sanitized name: a sender-controlled bidi control
                                        // must not reorder the chip while the warning classifies the
                                        // sanitized Save name.
                                        ui.label(typography::strong(Role::Secondary, display_name.as_str(), t.text));
                                        ui.label(typography::text(
                                            Role::Caption,
                                            format!("{} · {}", attachment.mime, format_size(attachment.size)),
                                            t.text_faint,
                                        ));
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let save = Button::new("Salva")
                                            .icon(icons::DOWNLOAD)
                                            .size(ButtonSize::Sm)
                                            .enabled(!self.demo)
                                            .disabled_reason("La modalità demo non ha allegati reali")
                                            .show(ui);
                                        save_ids.insert(attachment.id.clone(), save.id);
                                        #[cfg(test)]
                                        probe::attachment_save(ui, &attachment.id, save.rect);
                                        #[cfg(test)]
                                        probe::attachment_save_enabled(ui, &attachment.id, !self.demo);
                                        if save.clicked() || (save.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
                                            save_clicked = true;
                                        }
                                        let preview = Button::new("Anteprima")
                                            .size(ButtonSize::Sm)
                                            .enabled(!self.demo)
                                            .disabled_reason("La modalità demo non ha allegati reali")
                                            .show(ui);
                                        #[cfg(test)]
                                        probe::attachment_preview(ui, &attachment.id, preview.rect);
                                        if preview.clicked() || (preview.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
                                            preview_clicked = true;
                                        }
                                    });
                                });
                            },
                        );
                        if preview_clicked {
                            requested = Some(AttachmentStripAction::Preview(attachment.clone()));
                        } else if save_clicked {
                            requested = Some(AttachmentStripAction::Save(attachment.clone()));
                        }
                        ui.add_space(space::SM);
                    }
                });
        });
        self.attachment_save_ids = save_ids;
        requested
    }

    /// The conversation banner above the open message: participants, counts and one card per
    /// message. Exactly one card is open at a time; J/K and the cards move it.
    fn conversation_strip(&mut self, ui: &mut Ui, t: &Tokens) {
        let Some(summary) = self.thread_summary.clone() else { return };
        let subject = thread_subject(&summary.subject);
        let messages = self.thread_messages.clone();
        let open_id = self.thread_open.clone().or_else(|| self.selected.clone());
        if self.thread_offset > 0 && Button::ghost("Carica messaggi precedenti").size(ButtonSize::Sm).show(ui).clicked() {
            self.load_older_thread_messages();
            return;
        }
        Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::XS)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(typography::strong(Role::Caption, "CONVERSAZIONE", t.text_faint));
                ui.add_space(space::XS);
                ui.add(egui::Label::new(typography::strong(Role::BodySmall, &subject, t.text)).truncate().selectable(false));
                ui.add_space(space::SM);
                ui.label(typography::text(
                    Role::Caption,
                    format!(
                        "{} · {}",
                        messages_text(usize::try_from(summary.message_count).unwrap_or(usize::MAX)),
                        unread_text(usize::try_from(summary.unread_count).unwrap_or(usize::MAX))
                    ),
                    t.text_muted,
                ));
            });
            ScrollArea::horizontal().id_salt("conversation-cards").max_height(30.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for message in &messages {
                        let open = open_id.as_deref() == Some(message.id.as_str());
                        let sender = if message.from.name.trim().is_empty() { message.from.email.clone() } else { message.from.name.clone() };
                        let short = if sender.chars().count() > 18 {
                            let mut text = sender.chars().take(17).collect::<String>();
                            text.push('…');
                            text
                        } else {
                            sender
                        };
                        let label = format!("{short} · {}", format_when(message.date, self.now_utc));
                        if Button::ghost(&label).size(ButtonSize::Sm).selected(open).show(ui).clicked() {
                            self.open_thread_message(message.id.clone());
                        }
                    }
                });
            });
        });
    }

    /// The block view of one message, parsed from the stored sanitized fragment once per body.
    /// The reading pane must never re-parse raw HTML: `body_html` is already the sanitized form.
    fn rendered_body(&mut self, message: &Message) -> Arc<body::View> {
        let stale = match self.body_cache.as_ref() {
            None => true,
            Some(cache) => {
                cache.message_id != message.summary.id || cache.html.as_deref() != message.body_html.as_deref() || cache.text != message.body_text
            }
        };
        if stale {
            let view = Arc::new(body::SubsetBodyRenderer.render(&body::StoredBody { html: message.body_html.as_deref(), text: &message.body_text }));
            self.body_cache = Some(BodyCache {
                message_id: message.summary.id.clone(),
                html: message.body_html.clone(),
                text: message.body_text.clone(),
                view: Arc::clone(&view),
            });
            return view;
        }
        match &self.body_cache {
            Some(cache) => Arc::clone(&cache.view),
            // `stale` already filled the cache; this arm only keeps the function total.
            None => Arc::new(body::View::Text { text: String::new(), reason: body::TextReason::NoHtml }),
        }
    }

    /// Verify the open message once and keep only what the pane shows. A message that is not
    /// PGP/MIME, a demo row or a pre-v9 row gets no badge: the app never invents a signature.
    fn sync_pgp_reading(&mut self, message: &Message) {
        if self.demo
            || self.screenshot_message.as_ref().is_some_and(|shot| shot.summary.id == message.summary.id)
            || self.pgp_checked.as_deref() == Some(message.summary.id.as_str())
        {
            return;
        }
        self.pgp_reading = None;
        self.pgp_prompt = None;
        self.pgp_checked = Some(message.summary.id.clone());
        let mut args = json!({ "id": message.summary.id });
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        let Ok(value) = self.op("message_pgp_verify", args) else { return };
        let Ok(result) = serde_json::from_value::<PgpVerifyResult>(value) else { return };
        if result.kind == PgpMimeKind::None {
            return;
        }
        self.pgp_reading =
            Some(PgpReadingState { message_id: message.summary.id.clone(), kind: result.kind, verify: Some(result), decrypt: None, view: None });
    }

    /// Read the unsubscribe headers once per open message. A missing or invalid header is cached as
    /// a negative result so the frame loop does not repeat the operation.
    fn sync_unsubscribe(&mut self, message: &Message) {
        if self.demo
            || self.screenshot_message.as_ref().is_some_and(|shot| shot.summary.id == message.summary.id)
            || self.unsubscribe_checked.as_deref() == Some(message.summary.id.as_str())
        {
            return;
        }
        self.unsubscribe_info = None;
        self.show_unsubscribe = false;
        let mut args = json!({ "id": message.summary.id });
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        // A transient daemon error must not hide the entry point until the message is reopened.
        let Ok(value) = self.op("message_unsubscribe", args) else { return };
        let Ok(info) = serde_json::from_value::<UnsubscribeInfo>(value) else { return };
        self.unsubscribe_checked = Some(message.summary.id.clone());
        if !info.targets.is_empty() {
            self.unsubscribe_info = Some(info);
        }
    }

    /// A `mailto:` choice becomes a normal draft, never a background request. The person still
    /// reviews the address, body and Send.
    fn unsubscribe_mailto(&mut self, target: &UnsubscribeTarget) {
        let address = target.target.split_once(':').map(|(_, rest)| rest).unwrap_or(target.target.as_str());
        let address = address.split('?').next().unwrap_or(address);
        self.close_reading();
        self.compose_new();
        self.composer.to = address.to_owned();
        self.composer.subject = "unsubscribe".to_owned();
        self.composer.body = "Please unsubscribe this address from your mailing list.".to_owned();
        self.composer.focus = Some(ComposerFocus::Body);
        self.composer.dirty = true;
        self.composer.last_edit = 0.0;
    }

    fn unsubscribe_web(&mut self, target: &UnsubscribeTarget, ui: &Ui) {
        let Some(link) = reading::find_links(&target.target).into_iter().next() else {
            self.push_info_toast("Il link di disiscrizione non è apribile.", ToastKind::Error);
            return;
        };
        self.show_unsubscribe = false;
        self.request_link_open(&link);
        ui.ctx().request_repaint();
    }

    /// The explicit choice sheet. It lists every target from the header and never performs one.
    fn unsubscribe_sheet(&mut self, ui: &mut Ui) {
        if !self.show_unsubscribe {
            return;
        }
        let Some(info) = self.unsubscribe_info.clone() else {
            self.show_unsubscribe = false;
            return;
        };
        let t = theme::tokens(self.dark);
        let mut close = false;
        let mut mailto: Option<UnsubscribeTarget> = None;
        let mut web: Option<UnsubscribeTarget> = None;
        let response = egui::Modal::new(Id::new("postino-unsubscribe"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(500.0);
                ui.label(typography::strong(Role::Emphasis, "Disiscrizione", t.text));
                ui.add_space(space::SM);
                ui.label(typography::text(
                    Role::Secondary,
                    "Il mittente ha indicato queste opzioni. Postino non invia richieste in background: scegli tu se scrivere o aprire la pagina.",
                    t.text_muted,
                ));
                ui.add_space(space::MD);
                ScrollArea::vertical().max_height(330.0).show(ui, |ui| {
                    for target in &info.targets {
                        Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::SM)).show(
                            ui,
                            |ui| {
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    let (label, action) = match target.method {
                                        UnsubscribeMethod::Mailto => ("Email", "Scrivi la disiscrizione"),
                                        UnsubscribeMethod::Https => ("Pagina web", "Apri la pagina"),
                                        UnsubscribeMethod::Http => ("Pagina web non cifrata", "Apri con avviso"),
                                    };
                                    ui.label(typography::text(Role::BodySmall, label, t.text));
                                    ui.add(egui::Label::new(typography::mono(target.target.as_str(), 11.0, t.text_faint)).truncate());
                                    if target.method == UnsubscribeMethod::Mailto {
                                        if Button::primary(action).size(ButtonSize::Sm).show(ui).clicked() {
                                            mailto = Some(target.clone());
                                        }
                                    } else if Button::new(action).size(ButtonSize::Sm).show(ui).clicked() {
                                        web = Some(target.clone());
                                    }
                                    if target.method == UnsubscribeMethod::Http {
                                        ui.label(typography::text(
                                            Role::Caption,
                                            "Il collegamento non usa TLS: aprilo solo se ti fidi del mittente.",
                                            t.danger,
                                        ));
                                    }
                                });
                            },
                        );
                        ui.add_space(space::XS);
                    }
                });
                if info.one_click {
                    ui.label(typography::text(
                        Role::Caption,
                        "Il mittente dichiara One-Click. Postino apre comunque la pagina: nessuna richiesta parte da sola.",
                        t.text_faint,
                    ));
                }
                ui.add_space(space::LG);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = Button::ghost("Chiudi").show(ui).clicked();
                });
            });
        if let Some(target) = mailto {
            self.show_unsubscribe = false;
            self.unsubscribe_mailto(&target);
        } else if let Some(target) = web {
            self.unsubscribe_web(&target, ui);
        } else if close || response.should_close() {
            self.show_unsubscribe = false;
        }
    }

    /// Ask the engine to decrypt with no passphrase first. An unprotected key opens immediately; a
    /// protected key answers `Locked` and only then the sheet asks for the secret.
    fn begin_pgp_decrypt(&mut self) {
        let Some(state) = self.pgp_reading.as_ref() else { return };
        if !matches!(state.kind, PgpMimeKind::Encrypted) {
            return;
        }
        let id = state.message_id.clone();
        let Some(result) = self.pgp_decrypt(&id, "") else { return };
        match result.state {
            PgpDecryptState::Decrypted => self.apply_pgp_decrypt(result),
            PgpDecryptState::Locked | PgpDecryptState::WrongPassphrase => {
                self.apply_pgp_decrypt(result);
                self.pgp_prompt = Some(PgpPromptState::new(PgpPromptPurpose::Decrypt));
            }
            _ => self.apply_pgp_decrypt(result),
        }
    }

    fn pgp_decrypt(&mut self, id: &str, passphrase: &str) -> Option<PgpDecryptResult> {
        let mut args = json!({ "id": id, "passphrase": passphrase });
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        match self.op("message_pgp_decrypt", args) {
            Ok(value) => match serde_json::from_value::<PgpDecryptResult>(value) {
                Ok(result) => Some(result),
                Err(_) => {
                    self.push_info_toast("Risposta di decifratura non leggibile.", ToastKind::Error);
                    None
                }
            },
            Err(_) => {
                self.push_info_toast("Decifratura non riuscita: la chiave o il messaggio non sono leggibili.", ToastKind::Error);
                None
            }
        }
    }

    fn apply_pgp_decrypt(&mut self, result: PgpDecryptResult) {
        let Some(state) = self.pgp_reading.as_mut() else { return };
        if result.state == PgpDecryptState::Decrypted {
            let text = result.text.as_deref().unwrap_or_default();
            let view = body::SubsetBodyRenderer.render(&body::StoredBody { html: result.html.as_deref(), text });
            state.view = Some(Arc::new(view));
        }
        state.decrypt = Some(result);
    }

    fn pgp_body_view(&self, message_id: &str) -> Option<Arc<body::View>> {
        let state = self.pgp_reading.as_ref().filter(|state| state.message_id == message_id)?;
        state.decrypt.as_ref().filter(|result| result.state == PgpDecryptState::Decrypted)?;
        state.view.clone()
    }

    /// The badge and the decrypt action. It reads the cached verdict, never the parsed body: the
    /// UI cannot tell a signed message from an unsigned one without the retained raw bytes.
    fn pgp_reading_row(&mut self, ui: &mut Ui, t: &Tokens, message_id: &str) {
        let Some(state) = self.pgp_reading.as_ref().filter(|state| state.message_id == message_id) else { return };
        ui.add_space(space::XS);
        let mut decrypt = false;
        let mut hide = false;
        let ((label, kind), tooltip) = if let Some(result) = state.decrypt.as_ref() {
            (
                match result.state {
                    PgpDecryptState::Decrypted => ("Decifrato · solo in questa vista", BadgeKind::Accent),
                    PgpDecryptState::Locked => ("Chiave bloccata", BadgeKind::Neutral),
                    PgpDecryptState::WrongPassphrase => ("Passphrase errata", BadgeKind::Danger),
                    PgpDecryptState::NoKey => ("Nessuna chiave privata", BadgeKind::Neutral),
                    PgpDecryptState::Unknown => ("Decifratura non riuscita", BadgeKind::Danger),
                    PgpDecryptState::NotStored => ("PGP non disponibile", BadgeKind::Neutral),
                },
                pgp_decrypt_tooltip(result),
            )
        } else if state.kind == PgpMimeKind::Encrypted {
            (("Crittografato", BadgeKind::Neutral), "Messaggio cifrato: decifralo per leggerlo.".to_owned())
        } else if let Some(result) = state.verify.as_ref() {
            (
                match result.state {
                    PgpVerifyState::Verified => ("Firma verificata", BadgeKind::Accent),
                    PgpVerifyState::Bad => ("Firma non valida", BadgeKind::Danger),
                    PgpVerifyState::NoKey => ("Firma: chiave assente", BadgeKind::Neutral),
                    PgpVerifyState::Unknown => ("PGP: stato sconosciuto", BadgeKind::Neutral),
                    PgpVerifyState::NotStored => ("PGP non disponibile", BadgeKind::Neutral),
                },
                pgp_verify_tooltip(result),
            )
        } else {
            (("PGP: stato sconosciuto", BadgeKind::Neutral), "Stato PGP non disponibile.".to_owned())
        };
        let _row = ui.horizontal_wrapped(|ui| {
            let badge = Badge::new(label).kind(kind).show(ui);
            if !tooltip.is_empty() {
                badge.on_hover_text(tooltip.as_str());
            }
            if state.kind == PgpMimeKind::Encrypted
                && state.decrypt.as_ref().is_none_or(|result| result.state != PgpDecryptState::Decrypted)
                && Button::ghost("Decifra").size(ButtonSize::Sm).show(ui).clicked()
            {
                decrypt = true;
            }
            if state.decrypt.as_ref().is_some_and(|result| result.state == PgpDecryptState::Decrypted) {
                ui.label(typography::text(Role::Caption, "Il testo decifrato non viene salvato né indicizzato.", t.text_faint));
                if Button::ghost("Nascondi").size(ButtonSize::Sm).show(ui).clicked() {
                    hide = true;
                }
            }
        });
        #[cfg(test)]
        probe::reading_pgp(ui, _row.response.rect);
        if hide
            && let Some(state) = self.pgp_reading.as_mut()
            && state.message_id == message_id
        {
            state.decrypt = None;
            state.view = None;
        }
        if decrypt {
            self.begin_pgp_decrypt();
        }
    }

    /// Parse the open message's calendar part once. A screenshot scene owns its card directly.
    fn reading_invite(&mut self, message: &Message) -> Option<CalendarInviteView> {
        if self.screenshot_message.as_ref().is_some_and(|screenshot| screenshot.summary.id == message.summary.id) {
            return self.screenshot_invite.clone();
        }
        if let Some((id, invite)) = &self.invite_cache
            && id == &message.summary.id
        {
            return invite.clone();
        }
        let invite = self.engine.message_invite(&message.summary.id).ok().flatten();
        self.invite_cache = Some((message.summary.id.clone(), invite.clone()));
        invite
    }

    /// Ask the engine for the reply draft, then open it in the composer. The engine writes the
    /// draft; this method never sends and never marks anything on a calendar.
    fn reply_to_invite(&mut self, response: InviteResponse) {
        if let Some(message_id) = self.selected.clone() {
            self.start_invite_reply(&message_id, response);
        }
    }

    fn reading_content(&mut self, ui: &mut Ui, t: &Tokens, full: bool) {
        let content_bottom = ui.max_rect().bottom();
        let selected = self.selected.clone();
        let message = self
            .screenshot_message
            .clone()
            .filter(|message| selected.as_deref() == Some(message.summary.id.as_str()))
            .or_else(|| selected.as_deref().and_then(|id| self.engine.message(id)));
        let now_utc = self.now_utc;
        let Some(message) = message else {
            self.close_reading();
            return;
        };
        self.sync_pgp_reading(&message);
        self.sync_unsubscribe(&message);
        let invite = self.reading_invite(&message);
        #[cfg(test)]
        probe::clear_actions(ui);
        #[cfg(test)]
        probe::clear_attachment_saves(ui);
        #[cfg(test)]
        probe::clear_invite_actions(ui);
        #[cfg(test)]
        probe::clear_links(ui);
        if self.selected_thread.is_some() && self.threads_view {
            self.conversation_strip(ui, t);
            ui.add_space(space::XS);
        }
        let is_draft = self
            .engine
            .folders()
            .iter()
            .find(|folder| folder.id == message.summary.folder_id)
            .is_some_and(|folder| folder.kind == FolderKind::Drafts);

        // The header is also the restore target: a double-click on a row expands reading, a
        // double-click here goes back. It is registered before the buttons so a button wins the
        // hit test; only the empty part of the header restores.
        let header_rect = Rect::from_min_size(ui.available_rect_before_wrap().min, Vec2::new(ui.available_width(), 36.0));
        let header_response = ui.interact(header_rect, ui.id().with("reading-header"), Sense::click());
        #[cfg(test)]
        probe::reading_header(ui, header_rect);

        let has_unsubscribe = self.unsubscribe_info.is_some();
        let (left_action, right_action) = Sides::new().height(36.0).shrink_left().show(
            ui,
            |ui| {
                let mut action = None;
                // The labels still fit the default 541 pt pane; the icon-only path is for the pane
                // at its 420 pt minimum and for the overlay header, not for the default layout.
                let compact = ui.available_width() < 400.0;
                // Below the pane breakpoint the reading view is an overlay with no list behind it,
                // so it must keep Back and the two triage actions a user reaches for most.
                let overlay = reading_overlay(ui.ctx().content_rect().width());
                if full {
                    if Button::ghost(command_label("back", "Torna alla posta"))
                        .icon(command_icon("back", icons::ARROW_LEFT))
                        .shortcut(command_shortcut("back", "Esc"))
                        .show(ui)
                        .clicked()
                    {
                        action = Some(ReadingAction::Back);
                    }
                    #[cfg(test)]
                    probe::action(ui, "back");
                    if overlay {
                        if IconButton::new(command_icon("archive", icons::ARCHIVE), command_label("archive", "Archivia")).show(ui).clicked() {
                            action = Some(ReadingAction::Archive);
                        }
                        #[cfg(test)]
                        probe::action(ui, "archive");
                        if IconButton::new(command_icon("delete", icons::TRASH), command_label("delete", "Elimina")).danger(true).show(ui).clicked() {
                            action = Some(ReadingAction::Delete);
                        }
                        #[cfg(test)]
                        probe::action(ui, "delete");
                    }
                } else if compact {
                    if IconButton::new(command_icon("archive", icons::ARCHIVE), command_label("archive", "Archivia")).show(ui).clicked() {
                        action = Some(ReadingAction::Archive);
                    }
                    #[cfg(test)]
                    probe::action(ui, "archive");
                    if IconButton::new(command_icon("delete", icons::TRASH), command_label("delete", "Elimina")).danger(true).show(ui).clicked() {
                        action = Some(ReadingAction::Delete);
                    }
                    #[cfg(test)]
                    probe::action(ui, "delete");
                } else {
                    if Button::new(command_label("archive", "Archivia"))
                        .icon(command_icon("archive", icons::ARCHIVE))
                        .shortcut(command_shortcut("archive", "E"))
                        .show(ui)
                        .clicked()
                    {
                        action = Some(ReadingAction::Archive);
                    }
                    #[cfg(test)]
                    probe::action(ui, "archive");
                    if Button::danger(command_label("delete", "Elimina"))
                        .icon(command_icon("delete", icons::TRASH))
                        .shortcut(command_shortcut("delete", "Canc"))
                        .show(ui)
                        .clicked()
                    {
                        action = Some(ReadingAction::Delete);
                    }
                    #[cfg(test)]
                    probe::action(ui, "delete");
                }
                ui.add_space(space::SM);
                if is_draft {
                    if IconButton::new(command_icon("edit_draft", icons::COMPOSE), command_label("edit_draft", "Modifica bozza")).show(ui).clicked() {
                        action = Some(ReadingAction::EditDraft);
                    }
                } else {
                    if IconButton::new(command_icon("reply", icons::REPLY), command_label("reply", "Rispondi")).show(ui).clicked() {
                        action = Some(ReadingAction::Reply);
                    }
                    if IconButton::new(command_icon("reply_all", icons::REPLY_ALL), command_label("reply_all", "Rispondi a tutti")).show(ui).clicked()
                    {
                        action = Some(ReadingAction::ReplyAll);
                    }
                    if IconButton::new(command_icon("forward", icons::FORWARD), command_label("forward", "Inoltra")).show(ui).clicked() {
                        action = Some(ReadingAction::Forward);
                    }
                    if !self.demo
                        && self.store.is_some()
                        && IconButton::new(command_icon("export_eml", icons::DOWNLOAD), command_label("export_eml", "Esporta .eml"))
                            .show(ui)
                            .clicked()
                    {
                        action = Some(ReadingAction::ExportEml);
                    }
                    #[cfg(test)]
                    probe::action(ui, "export_eml");
                    if has_unsubscribe && IconButton::new(icons::EYE_OFF, "Disiscriviti").show(ui).on_hover_text("Opzioni di disiscrizione").clicked()
                    {
                        action = Some(ReadingAction::Unsubscribe);
                    }
                }
                ui.add_space(space::XS);
                let star_id = if message.summary.starred { "unstar" } else { "star" };
                let star_icon = if message.summary.starred { icons::STAR_FILL } else { icons::STAR };
                if IconButton::new(star_icon, command_label(star_id, "Stella"))
                    .filled(message.summary.starred)
                    .accent(message.summary.starred)
                    .show(ui)
                    .clicked()
                {
                    action = Some(ReadingAction::Star);
                }
                let read_id = if message.summary.unread { "read" } else { "unread" };
                let read_icon = if message.summary.unread { icons::MARK_READ } else { icons::MARK_UNREAD };
                if IconButton::new(read_icon, command_label(read_id, "Letto")).show(ui).clicked() {
                    action = Some(ReadingAction::Read);
                }
                action
            },
            |ui| {
                let mut action = None;
                if !full {
                    if IconButton::new(command_icon("expand", icons::SQUARE), command_label("expand", "Apri a schermo intero")).show(ui).clicked() {
                        action = Some(ReadingAction::Expand);
                    }
                    if IconButton::new(command_icon("close", icons::CLOSE), command_label("close", "Chiudi lettura")).show(ui).clicked() {
                        action = Some(ReadingAction::Close);
                    }
                }
                action
            },
        );

        if double_activated(&header_response) {
            if reading_overlay(ui.ctx().content_rect().width()) {
                self.close_reading();
            } else {
                self.reading_full = false;
            }
            return;
        }

        ui.add_space(space::MD);
        let subject_text = if message.summary.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.summary.subject.as_str() };
        let subject = ui.label(typography::strong(Role::Page, subject_text, t.text));
        reading_context_menu(subject, message.summary.subject.clone(), message.summary.from.display());
        ui.add_space(space::SM);
        let to = message.summary.to.iter().map(|address| address.display()).collect::<Vec<_>>().join(", ");
        let cc = message.cc.iter().map(|address| address.display()).collect::<Vec<_>>().join(", ");
        Sides::new().height(if cc.is_empty() { 38.0 } else { 52.0 }).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    avatar(ui, 34.0, &message.summary.from.initials(), t.gold, theme::FOREST);
                    ui.add_space(space::SM);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        let sender = ui.label(typography::strong(Role::BodySmall, message.summary.from.display(), t.text));
                        reading_context_menu(sender, message.summary.subject.clone(), message.summary.from.display());
                        let to_line = format!("a {to}");
                        ui.add(egui::Label::new(typography::text(Role::Meta, to_line.as_str(), t.text_muted)).truncate())
                            .on_hover_text(to_line.as_str());
                        if !cc.is_empty() {
                            let cc_line = format!("Cc: {cc}");
                            let cc_response = ui.add(egui::Label::new(typography::text(Role::Meta, cc_line.as_str(), t.text_muted)).truncate());
                            let cc_response = cc_response.on_hover_text(cc_line.as_str());
                            #[cfg(test)]
                            probe::reading_cc(ui, cc_response.rect);
                            let _ = cc_response;
                        }
                    });
                });
            },
            |ui| {
                ui.label(typography::mono(format_when(message.summary.date, now_utc), 11.0, t.text_faint));
            },
        );
        let mut chip_filter = None;
        if !message.summary.labels.is_empty() {
            let labels = self.labels_cache.clone();
            ui.add_space(space::XS);
            ui.horizontal_wrapped(|ui| {
                for id in &message.summary.labels {
                    if let Some(label) = labels.iter().find(|label| &label.id == id)
                        && Chip::new(label.name.as_str()).dot(label_color(label.color, t)).show(ui).clicked()
                    {
                        chip_filter = Some(label.id.clone());
                    }
                }
            });
        }
        if let Some(id) = chip_filter {
            self.select_label(id);
            return;
        }
        self.pgp_reading_row(ui, t, message.summary.id.as_str());
        // The three reply commands are enabled only while a received, valid invite is on screen.
        self.invite_ready = invite.as_ref().is_some_and(|invite| invite.valid && !invite.cancelled && invite.can_reply);
        // The count is stored truth from sync, not a scan of `body_html`: the sanitizer removed the
        // remote references, so the HTML cannot tell the pane that anything was blocked.
        if let Some(remote) = remote_images_text(message.remote_images) {
            ui.add_space(space::XS);
            ui.horizontal(|ui| {
                ui.label(icons::icon(icons::EYE_OFF, 14.0, t.gold_ink));
                ui.add_space(space::SM);
                ui.label(typography::text(Role::Caption, remote, t.text_faint));
            });
        }
        ui.add_space(space::SM);
        ui.separator();
        ui.add_space(space::XS);
        let view = self.pgp_body_view(message.summary.id.as_str()).unwrap_or_else(|| self.rendered_body(&message));
        ui.label(typography::text(Role::Caption, body::caption(view.as_ref()), t.text_faint));
        ui.add_space(space::SM);
        // A pinned strip keeps the attachment and its Save button visible at 980×600, where the body
        // scroll would otherwise push it below the fold. One budget covers banner, header, chips and
        // strip: the region is reserved once, the body gets the space above the strip and the strip
        // sits exactly on the content bottom. Nothing can grow past the status bar.
        let available_height = (content_bottom - ui.cursor().top()).max(0.0);
        let strip_height = if message.attachments.is_empty() {
            0.0
        } else {
            reading_attachment_strip_height(message.attachments.len()).min((available_height - READING_BODY_MIN).max(0.0))
        };
        let gap = if strip_height > 0.0 { space::SM } else { 0.0 };
        let body_height = (available_height - strip_height - gap).max(0.0);
        let (region, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), available_height), Sense::hover());
        let strip_top = region.bottom() - strip_height;
        let body_rect = Rect::from_min_max(region.min, pos2(region.right(), (strip_top - gap).max(region.top())));
        // A whole message shares one link budget; the scanner also rejects a link over the byte
        // bound, so a hostile body cannot create unlimited widgets or a huge dialog.
        let mut link_budget = reading::MAX_LINKS_PER_MESSAGE;
        let mut invite_command = None;
        ui.scope_builder(UiBuilder::new().max_rect(body_rect), |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("reading-scroll", message.summary.id.as_str()))
                .max_height(body_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut clicked_link = None;
                    // The card lives inside the reserved body budget: a long organizer, a warning or a
                    // 2000-name attendee list scrolls with the body instead of pushing the pinned
                    // attachment strip and the remote-content line off the 980×600 pane.
                    if let Some(invite) = invite.as_ref() {
                        invite_command = reading_invite_card(ui, invite, t);
                        ui.add_space(space::SM);
                    }
                    match view.as_ref() {
                        body::View::Blocks(body) => {
                            clicked_link = body_ui::blocks(ui, body, t, &mut link_budget);
                        }
                        body::View::Text { text, .. } => {
                            for paragraph in text.split("\n\n") {
                                let paragraph = paragraph.trim();
                                if paragraph.is_empty() {
                                    continue;
                                }
                                if paragraph.starts_with('>') {
                                    Frame::new()
                                        .fill(t.panel_alt)
                                        .corner_radius(radius::CONTROL)
                                        .inner_margin(Margin::symmetric(inset::LG, inset::MD))
                                        .show(ui, |ui| {
                                            let (link, used) = link_paragraph(ui, paragraph, t, true, link_budget);
                                            if let Some(link) = link {
                                                clicked_link = Some(link);
                                            }
                                            link_budget = link_budget.saturating_sub(used);
                                        });
                                } else {
                                    let (link, used) = link_paragraph(ui, paragraph, t, false, link_budget);
                                    if let Some(link) = link {
                                        clicked_link = Some(link);
                                    }
                                    link_budget = link_budget.saturating_sub(used);
                                }
                                ui.add_space(space::LG);
                            }
                        }
                    }
                    // The last line breathes above the fade instead of ending exactly on the clip.
                    ui.add_space(space::SM);
                    if let Some(link) = clicked_link {
                        self.request_link_open(&link);
                    }
                });
        });
        // A hard scroll clip slices the next line through its glyphs. The fade turns that boundary
        // into a deliberate visual end without hiding a whole line.
        bottom_fade(ui, body_rect, t.panel);
        #[cfg(test)]
        probe::reading_body(ui, body_rect);
        if !message.attachments.is_empty()
            && let Some(action) = ui
                .scope_builder(UiBuilder::new().max_rect(Rect::from_min_max(pos2(region.left(), strip_top), region.max)), |ui| {
                    self.reading_attachment_strip(ui, &message, t, strip_height)
                })
                .inner
        {
            match action {
                AttachmentStripAction::Preview(attachment) => self.request_attachment_preview(ui.ctx(), &attachment),
                AttachmentStripAction::Save(attachment) => self.request_attachment_save(&attachment),
            }
        }

        if let Some(command) = invite_command {
            self.execute_command(command, ui.ctx());
            return;
        }
        let action = left_action.or(right_action);
        match action {
            Some(ReadingAction::Archive) => self.move_selected(FolderKind::Archive, "Messaggio archiviato", "Messaggi archiviati"),
            Some(ReadingAction::Delete) => self.move_selected(FolderKind::Trash, "Messaggio spostato nel cestino", "Messaggi spostati nel cestino"),
            Some(ReadingAction::Reply) => {
                let id = message.summary.id.clone();
                self.reply_to(&id, ComposeMode::Reply);
            }
            Some(ReadingAction::ReplyAll) => {
                let id = message.summary.id.clone();
                self.reply_to(&id, ComposeMode::ReplyAll);
            }
            Some(ReadingAction::ExportEml) => self.export_selected_eml(),
            Some(ReadingAction::Forward) => {
                let id = message.summary.id.clone();
                self.reply_to(&id, ComposeMode::Forward);
            }
            Some(ReadingAction::EditDraft) => {
                let id = message.summary.id.clone();
                self.edit_draft(&id);
            }
            Some(ReadingAction::Star) => self.toggle_star(),
            Some(ReadingAction::Read) => self.toggle_read(),
            Some(ReadingAction::Close) => self.close_reading(),
            Some(ReadingAction::Expand) => self.reading_full = true,
            Some(ReadingAction::Unsubscribe) => self.show_unsubscribe = self.unsubscribe_info.is_some(),
            Some(ReadingAction::Back) => {
                // In the narrow overlay the back button leaves the reading pane entirely; only the
                // wide window has a list to return to under the full-screen pane.
                if reading_overlay(ui.ctx().content_rect().width()) {
                    self.close_reading();
                } else {
                    self.reading_full = false;
                }
            }
            None => {}
        }
    }

    /// The only way a link reaches the injected browser: the allowlist runs first, then this sheet,
    /// then `confirm_pending_link` with a second allowlist check at the action boundary.
    fn request_link_open(&mut self, link: &ExtractedLink) {
        if !reading::is_allowed_url(&link.url) {
            self.push_info_toast("Questo link non è apribile: sono ammessi solo http, https e mailto.", ToastKind::Error);
            return;
        }
        self.pending_link = Some(PendingLink {
            url: link.url.clone(),
            host: link.host.clone(),
            recipient: link.recipient.clone(),
            mailto: link.mailto,
            idn: link.idn,
            non_ascii: link.non_ascii,
            backslash: link.backslash,
            focus_cancel: true,
        });
    }

    fn confirm_pending_link(&mut self) {
        let Some(pending) = self.pending_link.take() else {
            return;
        };
        // The sheet already showed the destination, but state can change between frames. Re-run the
        // allowlist at the opener boundary so no other path can hand the browser an unvetted URL.
        if !reading::is_allowed_url(&pending.url) {
            self.push_info_toast("Questo link non è più apribile: sono ammessi solo http, https e mailto.", ToastKind::Error);
            return;
        }
        match self.link_opener.open(&pending.url) {
            Ok(()) => self.push_info_toast("Link aperto nel browser", ToastKind::Success),
            Err(error) => self.push_info_toast(format!("Non è stato possibile aprire il link: {error}"), ToastKind::Error),
        }
    }

    fn link_confirmation(&mut self, ui: &mut Ui) {
        let Some(pending) = self.pending_link.clone() else {
            return;
        };
        let focus_cancel = pending.focus_cancel;
        if let Some(link) = self.pending_link.as_mut() {
            link.focus_cancel = false;
        }
        // The sheet never replaces the raw URL with a parsed one. The effective destination is
        // described honestly: a `mailto:` opens a mail client and names its recipient, an ASCII host
        // is the browser host, and a non-ASCII host is only the spelling in the link (the browser
        // will use punycode). The full URL sits last in a bounded scroll area.
        let mut body = if pending.mailto {
            let recipient = pending.recipient.as_deref().unwrap_or(pending.host.as_str());
            format!("Aprire questo indirizzo nel client di posta?\n\nDestinatario: {recipient}")
        } else if pending.non_ascii {
            format!("Aprire questo indirizzo nel browser?\n\nDominio nel link: {}", pending.host)
        } else {
            format!("Aprire questo indirizzo nel browser?\n\nHost di destinazione: {}", pending.host)
        };
        if pending.url.to_ascii_lowercase().starts_with("http://") {
            body.push_str("\n\nAttenzione: questo indirizzo non usa TLS. Il contenuto della pagina può essere letto o modificato sulla rete.");
        }
        if pending.backslash {
            body.push_str(
                "\n\nAttenzione: l'indirizzo contiene una barra rovesciata (\\). Il browser la tratta come separatore di percorso: il sito aperto è quello indicato sopra.",
            );
        }
        if pending.idn {
            if pending.non_ascii {
                if pending.mailto {
                    body.push_str(
                        "\n\nAttenzione: il dominio del destinatario contiene caratteri non ASCII e il client di posta lo userà nella forma punycode (xn--). Verifica che sia il destinatario che ti aspetti.",
                    );
                } else {
                    body.push_str(
                        "\n\nAttenzione: il dominio contiene caratteri non ASCII e il browser lo aprirà nella forma punycode (xn--). Verifica che sia il sito che ti aspetti.",
                    );
                }
            } else {
                body.push_str("\n\nAttenzione: il dominio usa la codifica punycode (xn--). Verifica che sia il sito che ti aspetti.");
            }
        }
        body.push_str("\n\nIndirizzo completo:\n");
        body.push_str(&pending.url);
        let mut confirm = false;
        let mut cancel = false;
        let response =
            egui::Modal::new(Id::new("postino-link-confirm")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(ui.ctx(), |ui| {
                let response = Dialog::new("Aprire il link?", &body, "Apri")
                    .secondary("Annulla")
                    .width(520.0)
                    .body_scroll_height(220.0)
                    .focus_secondary(focus_cancel)
                    .show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            });
        if confirm {
            self.confirm_pending_link();
        } else if cancel || response.should_close() {
            self.pending_link = None;
        }
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

    fn request_attachment_save(&mut self, attachment: &Attachment) {
        if self.demo {
            // A synthetic attachment was never given bytes; the Save button is disabled with the
            // same reason, and this guard keeps a programmatic path from reporting a fake corruption.
            self.push_info_toast("La modalità demo non ha allegati reali.", ToastKind::Info);
            return;
        }
        let bytes = match self.engine.attachment_bytes(&attachment.id) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.push_info_toast("L'allegato non è disponibile nell'archivio locale: il file è mancante o danneggiato.", ToastKind::Error);
                return;
            }
        };
        // Classify the *sanitized* suggestion: a trailing NBSP, thin space or bidi control on the
        // raw MIME name must not hide `.exe` from the warning.
        let suggested = reading::safe_filename(&attachment.filename);
        let pending = PendingAttachment {
            filename: suggested.clone(),
            bytes,
            path: None,
            dangerous: reading::dangerous_kind(&suggested),
            focus_cancel: true,
            overwrite_confirmed: false,
        };
        if pending.dangerous.is_some() {
            self.attachment_warning = Some(pending);
        } else {
            self.choose_attachment_path(pending);
        }
    }

    /// Open the bounded, display-only preview for one attachment. The operation is the same
    /// `attachment_get` the CLI and MCP expose, so the daemon path and the in-process path agree.
    fn request_attachment_preview(&mut self, ctx: &egui::Context, attachment: &Attachment) {
        if self.demo {
            self.push_info_toast("La modalità demo non ha allegati reali.", ToastKind::Info);
            return;
        }
        let value = match self.op("attachment_get", json!({ "id": attachment.id })) {
            Ok(value) => value,
            Err(_) => {
                self.attachment_preview = Some(AttachmentPreviewState {
                    attachment: attachment.clone(),
                    text: None,
                    texture: None,
                    error: Some("L'allegato non è disponibile nell'archivio locale: il file è mancante o danneggiato.".to_owned()),
                    truncated: false,
                });
                return;
            }
        };
        let result: AttachmentGet = match serde_json::from_value(value) {
            Ok(result) => result,
            Err(_) => {
                self.push_info_toast("La risposta dell'archivio per l'allegato non è leggibile.", ToastKind::Error);
                return;
            }
        };
        let bytes = match base64::engine::general_purpose::STANDARD.decode(result.data_base64.as_bytes()) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.push_info_toast("I byte dell'allegato non sono decodificabili.", ToastKind::Error);
                return;
            }
        };
        let mut state = AttachmentPreviewState { attachment: result.attachment.clone(), text: None, texture: None, error: None, truncated: false };
        if bytes.len() as u64 != result.attachment.size {
            state.error = Some("La dimensione dei byte non corrisponde ai metadati dell'allegato.".to_owned());
            self.attachment_preview = Some(state);
            return;
        }
        let mime = result.attachment.mime.to_ascii_lowercase();
        if mime.starts_with("text/") || mime == "application/json" || mime == "application/xml" {
            let (text, truncated) = preview_text(&bytes);
            state.text = Some(text);
            state.truncated = truncated;
        } else if mime.starts_with("image/") {
            match preview_image(&bytes) {
                Ok(color) => {
                    state.texture =
                        Some(ctx.load_texture(format!("attachment-preview-{}", result.attachment.id), color, egui::TextureOptions::LINEAR));
                }
                Err(error) => state.error = Some(error),
            }
        } else {
            state.error = Some("Anteprima non disponibile per questo tipo di file. Puoi salvarlo e aprirlo con un'altra applicazione.".to_owned());
        }
        self.attachment_preview = Some(state);
    }

    fn attachment_preview_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.attachment_preview.as_mut() else { return };
        let t = theme::tokens(self.dark);
        let mut close = false;
        let mut save = false;
        let response = egui::Modal::new(Id::new("postino-attachment-preview"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(720.0_f32.min(ui.ctx().content_rect().width() - 40.0));
                ui.label(typography::strong(Role::Emphasis, "Anteprima allegato", t.text));
                ui.add_space(space::XS);
                ui.label(typography::strong(Role::Secondary, reading::safe_filename(&state.attachment.filename), t.text));
                ui.label(typography::text(
                    Role::Caption,
                    format!("{} · {}", state.attachment.mime, format_size(state.attachment.size)),
                    t.text_faint,
                ));
                ui.add_space(space::MD);
                if let Some(error) = state.error.as_deref() {
                    ui.label(typography::text(Role::Secondary, error, t.text_muted));
                } else if let Some(text) = state.text.as_deref() {
                    ScrollArea::both().max_height(360.0).auto_shrink([false, false]).show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(text).family(FontFamily::Monospace).size(type_size::BODY_SMALL).color(t.text))
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    });
                    if state.truncated {
                        ui.label(typography::text(
                            Role::Caption,
                            "Anteprima limitata ai primi 64 KiB; il file salvato resta completo.",
                            t.text_faint,
                        ));
                    }
                } else if let Some(texture) = state.texture.as_ref() {
                    let size = texture.size_vec2();
                    let scale = (720.0 / size.x).min(360.0 / size.y).min(1.0);
                    ui.image((texture.id(), size * scale));
                }
                ui.add_space(space::LG);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = Button::primary("Chiudi").show(ui).clicked();
                    save = Button::new("Salva").icon(icons::DOWNLOAD).size(ButtonSize::Sm).show(ui).clicked();
                });
            });
        #[cfg(test)]
        probe::attachment_preview_modal(ui, response.response.rect);
        if save {
            if let Some(state) = self.attachment_preview.take() {
                self.request_attachment_save(&state.attachment);
            }
        } else if close || response.should_close() {
            self.attachment_preview = None;
        }
    }

    fn choose_attachment_path(&mut self, pending: PendingAttachment) {
        let suggested = reading::safe_filename(&pending.filename);
        match self.file_saver.choose_path(&suggested) {
            Ok(Some(path)) => {
                if path.is_dir() {
                    self.push_info_toast("La destinazione scelta è una cartella, non un file.", ToastKind::Error);
                    return;
                }
                let mut pending = pending;
                pending.path = Some(path);
                // The person can turn a harmless suggestion into `setup.exe` in the native dialog.
                // Re-classify the chosen path before any byte is written.
                let chosen = pending.path.as_deref().and_then(attachment_destination_kind);
                if chosen.is_some() && chosen != pending.dangerous {
                    pending.dangerous = chosen;
                    pending.focus_cancel = true;
                    self.attachment_warning = Some(pending);
                    return;
                }
                self.finish_attachment_destination(pending);
            }
            Ok(None) => self.status = "Salvataggio annullato".to_owned(),
            Err(error) => self.push_info_toast(format!("Non è stato possibile aprire la finestra di salvataggio: {error}"), ToastKind::Error),
        }
    }

    fn finish_attachment_destination(&mut self, mut pending: PendingAttachment) {
        let Some(path) = pending.path.as_ref() else {
            return;
        };
        pending.focus_cancel = true;
        if path.exists() {
            pending.overwrite_confirmed = false;
            self.attachment_overwrite = Some(pending);
        } else {
            self.write_attachment(pending);
        }
    }

    fn write_attachment(&mut self, pending: PendingAttachment) {
        let Some(path) = pending.path.as_ref() else {
            return;
        };
        // A plain `write` would silently replace a file another process created after the
        // confirmation. `write_new` creates exclusively unless the person confirmed the overwrite.
        let result =
            if pending.overwrite_confirmed { self.file_saver.write(path, &pending.bytes) } else { self.file_saver.write_new(path, &pending.bytes) };
        match result {
            Ok(()) => self.push_info_toast(format!("Allegato “{}” salvato", pending.filename), ToastKind::Success),
            Err(error) => {
                if !pending.overwrite_confirmed && path.exists() {
                    let mut pending = pending;
                    pending.focus_cancel = true;
                    pending.overwrite_confirmed = false;
                    self.attachment_overwrite = Some(pending);
                    return;
                }
                self.push_info_toast(format!("Non è stato possibile salvare “{}”: {error}", pending.filename), ToastKind::Error);
            }
        }
    }

    fn attachment_warning_dialog(&mut self, ui: &mut Ui) {
        let Some(pending) = self.attachment_warning.as_ref() else {
            return;
        };
        let filename = pending.filename.clone();
        let chosen = pending.path.as_ref().map(|path| path.display().to_string());
        let message = pending.dangerous.unwrap_or(DangerousKind::Executable).message();
        let focus_cancel = pending.focus_cancel;
        if let Some(pending) = self.attachment_warning.as_mut() {
            pending.focus_cancel = false;
        }
        let mut body = format!("“{}”\n\n{}", filename, message);
        if let Some(chosen) = chosen {
            body.push_str(&format!("\n\nDestinazione scelta: {chosen}"));
        }
        let mut confirm = false;
        let mut cancel = false;
        let response =
            egui::Modal::new(Id::new("postino-attachment-warning")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(ui.ctx(), |ui| {
                let response = Dialog::new("Salvare un file potenzialmente pericoloso?", &body, "Salva comunque")
                    .secondary("Annulla")
                    .danger(true)
                    .width(520.0)
                    .focus_secondary(focus_cancel)
                    .show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            });
        if confirm {
            if let Some(pending) = self.attachment_warning.take() {
                if pending.path.is_some() {
                    self.finish_attachment_destination(pending);
                } else {
                    self.choose_attachment_path(pending);
                }
            }
        } else if cancel || response.should_close() {
            self.attachment_warning = None;
            self.status = "Salvataggio annullato".to_owned();
        }
    }

    fn attachment_overwrite_dialog(&mut self, ui: &mut Ui) {
        let Some(pending) = self.attachment_overwrite.as_ref() else {
            return;
        };
        let path = pending.path.as_ref().map_or_else(|| pending.filename.clone(), |path| path.display().to_string());
        let focus_cancel = pending.focus_cancel;
        if let Some(pending) = self.attachment_overwrite.as_mut() {
            pending.focus_cancel = false;
        }
        let body = format!("Il file esiste già:\n\n{path}\n\nSostituirlo? I dati del file esistente saranno persi.");
        let mut confirm = false;
        let mut cancel = false;
        let response = egui::Modal::new(Id::new("postino-attachment-overwrite")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(
            ui.ctx(),
            |ui| {
                let response = Dialog::new("Sostituire il file?", &body, "Sostituisci")
                    .secondary("Annulla")
                    .danger(true)
                    .width(520.0)
                    .focus_secondary(focus_cancel)
                    .show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            },
        );
        if confirm {
            if let Some(mut pending) = self.attachment_overwrite.take() {
                pending.overwrite_confirmed = true;
                self.write_attachment(pending);
            }
        } else if cancel || response.should_close() {
            self.attachment_overwrite = None;
            self.status = "Salvataggio annullato".to_owned();
        }
    }

    fn account_menu(&mut self, ui: &mut Ui) {
        if !self.show_accounts_menu {
            return;
        }
        let t = theme::tokens(self.dark);
        let accounts = self.engine.accounts().to_vec();
        let active = self.active_account.clone();
        let mut switch = None;
        let mut add = false;
        let mut manage = false;
        let mut leave_demo = false;
        let can_return = self.demo_can_return();
        let response = egui::Modal::new(Id::new("postino-accounts"))
            .area(egui::Modal::default_area(Id::new("postino-accounts")).anchor(Align2::LEFT_TOP, Vec2::new(ui_size::SIDEBAR + 24.0, 96.0)))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(340.0);
                ui.label(typography::strong(Role::BodySmall, "Account", t.text));
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "La posta, le cartelle e la sincronizzazione seguono l'account attivo.", t.text_faint));
                ui.add_space(space::SM);
                if accounts.is_empty() {
                    ui.label(typography::text(Role::BodySmall, "Nessun account configurato.", t.text_muted));
                } else {
                    for account in &accounts {
                        let label = format!("{} <{}>", account.name, account.email);
                        let selected = active.as_deref() == Some(account.id.as_str());
                        let response = Button::ghost(&label).icon(icons::MAIL).full_width().selected(selected).show(ui);
                        if response.clicked() && !selected {
                            switch = Some(account.id.clone());
                        }
                    }
                }
                ui.add_space(space::SM);
                if can_return {
                    if Button::new("Configura un account").icon(icons::USER).full_width().show(ui).clicked() {
                        leave_demo = true;
                    }
                    ui.add_space(space::XS);
                    ui.label(typography::text(Role::Caption, "Torna all'archivio locale e aggiungi il tuo account.", t.text_faint));
                } else {
                    let add_button = Button::new("Aggiungi account")
                        .icon(icons::PLUS)
                        .full_width()
                        .enabled(self.can_add_account())
                        .disabled_reason(
                            "La modalità demo non ha un archivio locale: chiudi e riapri Postino senza la demo per aggiungere un account",
                        )
                        .show(ui);
                    if add_button.clicked() {
                        add = true;
                    }
                }
                if Button::ghost("Gestisci account…").icon(icons::SETTINGS).full_width().show(ui).clicked() {
                    manage = true;
                }
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "Esc chiude", t.text_faint));
            });
        if response.should_close() {
            self.show_accounts_menu = false;
        }
        if let Some(id) = switch {
            self.show_accounts_menu = false;
            self.switch_account(id);
        }
        if add {
            self.show_accounts_menu = false;
            self.onboarding = Some(Onboarding::add_account());
        }
        if leave_demo {
            self.show_accounts_menu = false;
            self.leave_demo();
        }
        if manage {
            self.show_accounts_menu = false;
            self.open_settings(ui.ctx(), self.account_switcher_id);
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

    fn close_licences(&mut self, ctx: &egui::Context) {
        self.show_licences = false;
        if let Some(id) = self.licences_return_focus.take() {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    fn account_last_sync(&self, account_id: &str) -> Option<i64> {
        self.engine
            .account_views()
            .ok()
            .and_then(|views| views.into_iter().find(|view| view.account.id == account_id))
            .and_then(|view| view.sync)
            .and_then(|status| status.last_success)
            .or_else(|| (self.active_account.as_deref() == Some(account_id)).then_some(self.last_sync).flatten())
    }

    fn open_data_folder(&mut self) {
        let Some(store_path) = self.store_path.as_ref() else {
            self.status = "La modalità demo non ha una cartella dati".to_owned();
            return;
        };
        let directory = postino_store::blob_root(store_path);
        if !directory.is_dir() {
            self.push_info_toast("La cartella degli allegati non esiste ancora.", ToastKind::Error);
            return;
        }
        if let Err(error) = self.folder_opener.open(&directory) {
            self.push_info_toast(format!("Non è stato possibile aprire la cartella: {error}"), ToastKind::Error);
        }
    }

    fn signature_editor_ui(&mut self, ui: &mut Ui, t: &Tokens, account: &Account, save_out: &mut Option<String>, cancel_out: &mut bool) {
        let Some(editor) = self.signature_editor.as_mut() else {
            return;
        };
        Frame::new()
            .fill(t.panel_alt)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::SM))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(typography::strong(Role::BodySmall, "Firma", t.text));
                    ui.label(typography::text(Role::Caption, "Testo semplice, solo per i nuovi messaggi", t.text_faint));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(typography::mono(
                            format!("{}/{} byte", editor.text.len(), postino_core::MAX_SIGNATURE_BYTES),
                            9.5,
                            if editor.text.len() > postino_core::MAX_SIGNATURE_BYTES { t.danger } else { t.text_faint },
                        ));
                    });
                });
                ui.add_space(space::XS);
                let edit = TextEdit::multiline(&mut editor.text)
                    .id_salt(("signature-editor", account.id.as_str()))
                    .hint_text("Federico\nfederico@example.it")
                    .font(FontId::new(type_size::BODY_SMALL, FontFamily::Proportional))
                    .text_color(t.text)
                    .frame(
                        Frame::new()
                            .fill(t.field)
                            .stroke(Stroke::new(1.0, t.border))
                            .corner_radius(radius::CONTROL)
                            .inner_margin(Margin::symmetric(inset::SM, inset::XS)),
                    )
                    .desired_width(f32::INFINITY)
                    .min_size(Vec2::new(0.0, 72.0));
                let response = edit.show(ui);
                if editor.focus {
                    response.response.request_focus();
                    editor.focus = false;
                }
                #[cfg(test)]
                probe::signature_editor(ui, &account.id, response.response.rect);
                let invalid_control = editor.text.chars().any(|character| character.is_control() && !matches!(character, '\n' | '\t'));
                let too_large = editor.text.len() > postino_core::MAX_SIGNATURE_BYTES;
                let problem = if too_large {
                    Some("La firma supera 4 KiB: accorciala prima di salvare.".to_owned())
                } else if invalid_control {
                    Some("La firma contiene un carattere di controllo: sono ammessi solo nuova riga e tabulazione.".to_owned())
                } else {
                    editor.error.clone()
                };
                if let Some(problem) = &problem {
                    ui.label(typography::text(Role::Caption, problem.as_str(), t.danger));
                }
                ui.horizontal(|ui| {
                    let save = Button::primary("Salva firma")
                        .icon(icons::SAVE)
                        .size(ButtonSize::Sm)
                        .enabled(problem.is_none())
                        .disabled_reason("Correggi la firma prima di salvarla")
                        .show(ui);
                    #[cfg(test)]
                    probe::signature_save(ui, &account.id, save.rect);
                    if save.clicked() {
                        *save_out = Some(account.id.clone());
                    }
                    let cancel = Button::ghost("Annulla").size(ButtonSize::Sm).show(ui);
                    #[cfg(test)]
                    probe::signature_cancel(ui, &account.id, cancel.rect);
                    if cancel.clicked() {
                        *cancel_out = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(typography::text(
                            Role::Caption,
                            "La modifica vale per i nuovi messaggi, non per le bozze già salvate.",
                            t.text_faint,
                        ));
                    });
                });
            });
    }

    fn accounts_dialog(&mut self, ui: &mut Ui) {
        if !self.show_accounts_dialog {
            return;
        }
        // The page is live: every open (and every reopen after a close, since both clear the cache)
        // reads the daemon and the platform once, then writes are confirmed by the next read.
        if self.background_rows.is_none() {
            self.refresh_background();
        }
        if !self.pgp_keys_loaded && !self.demo {
            self.refresh_pgp_keys();
        }
        self.refresh_templates();
        self.refresh_identities();
        // Escape closes the signature editor first; only a second Escape leaves Settings. Consuming
        // the key here keeps the modal's own close rule from seeing it.
        if self.signature_editor.is_some()
            && ui.ctx().input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
            && let Some(editor) = self.signature_editor.take()
        {
            self.signature_focus_account = Some(editor.account);
        }
        let t = theme::tokens(self.dark);
        let accounts = self.engine.accounts().to_vec();
        let last = accounts.len() == 1;
        let mut remove = None;
        let mut add = false;
        let mut leave_demo = false;
        let mut close = false;
        let mut theme_choice: Option<(bool, bool)> = None;
        let mut reduced = self.reduced_motion;
        let mut reset_pane = false;
        let mut open_folder = false;
        let mut open_rules = false;
        let mut licences = false;
        let mut shortcuts = false;
        let mut signature_open: Option<String> = None;
        let mut signature_save: Option<String> = None;
        let mut signature_cancel = false;
        let mut pgp_import = false;
        let mut pgp_delete: Option<PgpKey> = None;
        let mut pgp_export: Option<String> = None;
        let mut template_new = false;
        let mut template_edit: Option<MessageTemplate> = None;
        let mut template_delete: Option<MessageTemplate> = None;
        let mut identity_new = false;
        let mut identity_edit: Option<Identity> = None;
        let mut identity_delete: Option<Identity> = None;
        let focus_close = self.settings_focus_pending;
        self.settings_focus_pending = false;
        let window = ui.ctx().content_rect();
        let width = (window.width() - 2.0 * space::HUGE).clamp(520.0, 720.0);
        let scroll_height = (window.height() - 160.0).clamp(240.0, 520.0);
        let response = egui::Modal::new(Id::new("postino-settings"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XXL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(typography::strong(Role::Section, "Impostazioni", t.text));
                        ui.label(typography::text(Role::Meta, "Account, aspetto, dati e informazioni su Postino.", t.text_faint));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let button = Button::ghost("Chiudi").show(ui);
                        if focus_close {
                            button.request_focus();
                        }
                        if button.clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(space::SM);
                let _settings_scroll = egui::ScrollArea::vertical()
                    .id_salt("settings-scroll")
                    .max_height(scroll_height)
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .show(ui, |ui| {
                        ui.spacing_mut().scroll = crate::ui::tokens::scroll_style();
                        let _account_header = ui.label(typography::strong(Role::BodySmall, "ACCOUNT", t.text_faint));
                        #[cfg(test)]
                        probe::settings_section(ui, "ACCOUNT", _account_header.rect);
                        ui.add_space(space::XS);
                        if accounts.is_empty() {
                            ui.label(typography::text(Role::BodySmall, "Nessun account configurato.", t.text_muted));
                        }
                        for account in &accounts {
                            let active = self.active_account.as_deref() == Some(account.id.as_str());
                            let sync = self
                                .account_last_sync(&account.id)
                                .map_or_else(|| "Ultimo sync: mai".to_owned(), |ms| format!("Ultimo sync: {}", format_millis(ms, self.now_utc)));
                            Frame::new()
                                .fill(if active { t.panel_alt } else { t.panel })
                                .stroke(Stroke::new(1.0, if active { t.border } else { Color32::TRANSPARENT }))
                                .corner_radius(radius::CONTROL)
                                .inner_margin(Margin::symmetric(inset::LG, inset::XS))
                                .show(ui, |ui| {
                                    Sides::new().show(
                                        ui,
                                        |ui| {
                                            ui.horizontal(|ui| {
                                                ui.label(icons::icon(icons::MAIL, 16.0, if active { t.gold_ink } else { t.text_faint }));
                                                ui.add_space(space::SM);
                                                ui.vertical(|ui| {
                                                    ui.spacing_mut().item_spacing.y = 1.0;
                                                    ui.horizontal(|ui| {
                                                        ui.label(typography::text(Role::BodySmall, account.name.as_str(), t.text));
                                                        if active {
                                                            let badge = Badge::new("Attivo").kind(BadgeKind::Accent).show(ui);
                                                            badge.on_hover_text("L'account che stai leggendo");
                                                        }
                                                    });
                                                    let sync_line = format!("{} · {}", account.email, sync);
                                                    ui.add(
                                                        egui::Label::new(typography::text(Role::Caption, sync_line.as_str(), t.text_faint))
                                                            .truncate(),
                                                    )
                                                    .on_hover_text(sync_line.as_str());
                                                });
                                            });
                                        },
                                        |ui| {
                                            let signature_button = Button::ghost("Firma").size(ButtonSize::Sm).show(ui);
                                            #[cfg(test)]
                                            probe::signature_open_button(ui, &account.id, signature_button.rect);
                                            #[cfg(test)]
                                            probe::signature_open_button_id(ui, &account.id, signature_button.id);
                                            #[cfg(test)]
                                            crate::app::probe::signature_open_clicked(ui, signature_button.clicked());
                                            #[cfg(test)]
                                            crate::app::probe::signature_open_hover(
                                                ui,
                                                &account.id,
                                                signature_button.hovered(),
                                                ui.ctx().pointer_interact_pos(),
                                            );
                                            if self.signature_focus_account.as_deref() == Some(account.id.as_str()) {
                                                signature_button.request_focus();
                                                self.signature_focus_account = None;
                                            } else if signature_button.clicked() {
                                                if let Some(editor) = self.signature_editor.as_mut().filter(|editor| editor.account == account.id) {
                                                    // Clicking Firma while its editor is open focuses the text
                                                    // instead of being a dead button.
                                                    editor.focus = true;
                                                } else {
                                                    signature_open = Some(account.id.clone());
                                                }
                                            }
                                            let remove_button = Button::danger("Rimuovi")
                                                .size(ButtonSize::Sm)
                                                .enabled(self.can_add_account())
                                                .disabled_reason("La modalità demo non ha un archivio locale")
                                                .show(ui);
                                            #[cfg(test)]
                                            probe::account_remove_button(ui, &account.id, remove_button.rect);
                                            if remove_button.clicked() {
                                                remove = Some(AccountRemoveState {
                                                    id: account.id.clone(),
                                                    name: account.name.clone(),
                                                    email: account.email.clone(),
                                                    last,
                                                    focus_cancel: true,
                                                });
                                            }
                                        },
                                    );
                                });
                            if self.signature_editor.as_ref().is_some_and(|editor| editor.account == account.id) {
                                self.signature_editor_ui(ui, &t, account, &mut signature_save, &mut signature_cancel);
                            }
                            ui.add_space(space::XS);
                        }
                        if self.demo_can_return() {
                            let leave = Button::new("Torna all'archivio e configura un account").icon(icons::USER).size(ButtonSize::Sm).show(ui);
                            #[cfg(test)]
                            probe::settings_action(ui, "leave_demo", leave.rect);
                            if leave.clicked() {
                                leave_demo = true;
                            }
                        } else if Button::new("Aggiungi account")
                            .icon(icons::PLUS)
                            .enabled(self.can_add_account())
                            .disabled_reason(
                                "La modalità demo non ha un archivio locale: chiudi e riapri Postino senza la demo per aggiungere un account",
                            )
                            .show(ui)
                            .clicked()
                        {
                            add = true;
                        }

                        ui.add_space(space::XS);
                        let _appearance = ui.label(typography::strong(Role::BodySmall, "ASPETTO", t.text_faint));
                        #[cfg(test)]
                        probe::settings_section(ui, "ASPETTO", _appearance.rect);
                        ui.add_space(space::XS);
                        ui.horizontal(|ui| {
                            let system = self.system_theme;
                            for (label, icon, selected, choice) in [
                                ("Chiaro", icons::SUN, !system && !self.dark, Some((false, false))),
                                ("Scuro", icons::MOON, !system && self.dark, Some((false, true))),
                                ("Sistema", icons::DESKTOP, system, Some((true, self.dark))),
                            ] {
                                if Button::new(label).icon(icon).selected(selected).show(ui).clicked()
                                    && let Some(choice) = choice
                                {
                                    theme_choice = Some(choice);
                                }
                            }
                        });
                        ui.add_space(space::XS);
                        ui.horizontal(|ui| {
                            if Button::new("Riduci le animazioni").icon(icons::CLOCK).selected(reduced).show(ui).clicked() {
                                reduced = !reduced;
                            }
                            if Button::new("Reimposta larghezza lettura").icon(icons::SIDEBAR).show(ui).clicked() {
                                reset_pane = true;
                            }
                        });

                        ui.add_space(space::XS);
                        let _system = ui.label(typography::strong(Role::BodySmall, "SISTEMA", t.text_faint));
                        #[cfg(test)]
                        probe::settings_section(ui, "SISTEMA", _system.rect);
                        ui.add_space(space::XS);
                        if let Some(rows) = self.background_rows.clone() {
                            if let Some(error) = rows.error.as_deref() {
                                ui.label(typography::text(Role::Caption, error, t.danger));
                            }
                            for (key, title, description, row, change) in [
                                (
                                    "tray",
                                    "Barra delle applicazioni",
                                    "Icona e menu Apri, Sincronizza, Coda di invio ed Esci.",
                                    &rows.tray,
                                    BackgroundChange::Tray(!rows.tray.on),
                                ),
                                (
                                    "notify",
                                    "Notifiche",
                                    "Avvisi per nuova posta e invii non riusciti.",
                                    &rows.notify,
                                    BackgroundChange::Notify(!rows.notify.on),
                                ),
                                (
                                    "previews",
                                    "Anteprime nelle notifiche",
                                    "Mittente e oggetto nell'avviso; disattive, l'avviso non contiene dati.",
                                    &rows.previews,
                                    BackgroundChange::Previews(!rows.previews.on),
                                ),
                                (
                                    "autostart",
                                    "Avvio automatico",
                                    "Avvia il daemon all'accesso con questo utente.",
                                    &rows.autostart,
                                    BackgroundChange::Autostart(!rows.autostart.on),
                                ),
                            ] {
                                if let Some(change) = background_row(ui, key, title, description, row, change) {
                                    self.background_change = Some(change);
                                }
                            }
                        }

                        ui.add_space(space::XS);
                        let path = self
                            .store_path
                            .as_ref()
                            .map_or_else(|| "Modalità demo: nessun archivio locale".to_owned(), |path| path.display().to_string());
                        let blob = self.store_path.as_ref().map(|path| postino_store::blob_root(path).display().to_string());
                        ui.horizontal(|ui| {
                            let _data_header = ui.label(typography::strong(Role::BodySmall, "DATI", t.text_faint));
                            #[cfg(test)]
                            probe::settings_section(ui, "DATI", _data_header.rect);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let copy_blob = Button::ghost("Copia allegati")
                                    .size(ButtonSize::Sm)
                                    .icon(icons::COPY)
                                    .enabled(blob.is_some())
                                    .disabled_reason("La modalità demo non ha una cartella dati")
                                    .show(ui);
                                #[cfg(test)]
                                probe::settings_copy(ui, "blob", copy_blob.rect);
                                if copy_blob.clicked()
                                    && let Some(blob) = &blob
                                {
                                    ui.ctx().copy_text(blob.clone());
                                }
                                let copy_store = Button::ghost("Copia archivio").size(ButtonSize::Sm).icon(icons::COPY).show(ui);
                                #[cfg(test)]
                                probe::settings_copy(ui, "store", copy_store.rect);
                                if copy_store.clicked() {
                                    ui.ctx().copy_text(path.clone());
                                }
                            });
                        });
                        ui.add_space(space::XS);
                        ui.add(egui::Label::new(typography::mono(path.as_str(), type_size::CAPTION, t.text_faint)).selectable(true).wrap());
                        if let Some(blob) = &blob {
                            ui.add(egui::Label::new(typography::mono(blob.as_str(), type_size::CAPTION, t.text_faint)).selectable(true).wrap());
                        }
                        ui.add_space(space::XS);
                        let can_open = self.store_path.is_some();
                        ui.horizontal(|ui| {
                            let open_button = Button::new("Apri la cartella degli allegati")
                                .icon(icons::FOLDER)
                                .enabled(can_open)
                                .disabled_reason("La modalità demo non ha una cartella dati")
                                .show(ui);
                            #[cfg(test)]
                            probe::settings_action(ui, "open_folder", open_button.rect);
                            if open_button.clicked() {
                                open_folder = true;
                            }
                            let rules_button = Button::new("Regole locali").icon(icons::FILTER).show(ui);
                            #[cfg(test)]
                            probe::settings_action(ui, "rules", rules_button.rect);
                            if rules_button.clicked() {
                                open_rules = true;
                            }
                        });

                        ui.add_space(space::XS);
                        let _info_header = ui.label(typography::mono(
                            format!("INFORMAZIONI · Postino {}", env!("CARGO_PKG_VERSION")),
                            type_size::CAPTION,
                            t.text_muted,
                        ));
                        #[cfg(test)]
                        probe::settings_section(ui, "INFORMAZIONI", _info_header.rect);
                        ui.add_space(space::XS);
                        ui.horizontal(|ui| {
                            let licences_button = Button::ghost("Licenze e attribuzioni").icon(icons::INFO).show(ui);
                            #[cfg(test)]
                            probe::settings_action(ui, "licences", licences_button.rect);
                            if licences_button.clicked() {
                                licences = true;
                            }
                            let shortcuts_button = Button::ghost("Scorciatoie da tastiera").icon(icons::KEYBOARD).show(ui);
                            #[cfg(test)]
                            probe::settings_action(ui, "shortcuts", shortcuts_button.rect);
                            if shortcuts_button.clicked() {
                                shortcuts = true;
                            }
                        });

                        ui.add_space(space::MD);
                        let _pgp = ui.label(typography::strong(Role::BodySmall, "PGP", t.text_faint));
                        #[cfg(test)]
                        probe::settings_section(ui, "PGP", _pgp.rect);
                        ui.add_space(space::XS);
                        ui.label(typography::text(
                            Role::Meta,
                            "Le chiavi restano in questo archivio cifrato. La passphrase non viene salvata e serve a ogni uso.",
                            t.text_faint,
                        ));
                        if let Some(notice) = self.pgp_notice.as_deref() {
                            ui.label(typography::text(Role::Caption, notice, t.text_muted));
                        }
                        if self.pgp_keys.is_empty() {
                            ui.label(typography::text(Role::BodySmall, "Nessuna chiave PGP salvata.", t.text_muted));
                        }
                        for key in &self.pgp_keys {
                            Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::XS)).show(
                                ui,
                                |ui| {
                                    Sides::new().show(
                                        ui,
                                        |ui| {
                                            ui.vertical(|ui| {
                                                ui.spacing_mut().item_spacing.y = 1.0;
                                                ui.label(typography::text(
                                                    Role::BodySmall,
                                                    key.user_id.as_deref().unwrap_or("Chiave senza identità"),
                                                    t.text,
                                                ));
                                                let short = key.fingerprint.chars().take(16).collect::<String>();
                                                let short = if short.len() < key.fingerprint.len() { format!("{short}…") } else { short };
                                                ui.add(egui::Label::new(typography::mono(short, 11.0, t.text_faint)).truncate())
                                                    .on_hover_text(key.fingerprint.as_str());
                                            });
                                        },
                                        |ui| {
                                            if key.has_secret {
                                                ui.label(typography::text(
                                                    Role::Caption,
                                                    if key.protected { "Privata · protetta" } else { "Privata" },
                                                    t.text_faint,
                                                ));
                                            }
                                            let export = Button::ghost("Copia pubblica").size(ButtonSize::Sm).show(ui);
                                            #[cfg(test)]
                                            probe::settings_pgp_export(ui, key.id.as_str(), export.rect);
                                            if export.clicked() {
                                                pgp_export = Some(key.id.clone());
                                            }
                                            if Button::danger("Elimina").size(ButtonSize::Sm).show(ui).clicked() {
                                                pgp_delete = Some(key.clone());
                                            }
                                        },
                                    );
                                },
                            );
                            ui.add_space(space::XS);
                        }
                        if Button::new("Importa chiave PGP")
                            .icon(icons::PLUS)
                            .size(ButtonSize::Sm)
                            .enabled(!self.demo)
                            .disabled_reason("La modalità demo non ha un archivio locale su cui salvare le chiavi")
                            .show(ui)
                            .clicked()
                        {
                            pgp_import = true;
                        }

                        ui.add_space(space::MD);
                        let _templates = ui.label(typography::strong(Role::BodySmall, "MODELLI", t.text_faint));
                        #[cfg(test)]
                        probe::settings_section(ui, "MODELLI", _templates.rect);
                        ui.add_space(space::XS);
                        ui.label(typography::text(
                            Role::Meta,
                            "Testi pronti per il composer. Restano in questo archivio cifrato e non vengono inviati a un server finché non li usi in una bozza.",
                            t.text_faint,
                        ));
                        if self.templates.is_empty() {
                            ui.label(typography::text(Role::BodySmall, "Nessun modello salvato.", t.text_muted));
                        }
                        for template in &self.templates {
                            Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::XS)).show(
                                ui,
                                |ui| {
                                    Sides::new().show(
                                        ui,
                                        |ui| {
                                            ui.vertical(|ui| {
                                                ui.spacing_mut().item_spacing.y = 1.0;
                                                ui.label(typography::text(Role::BodySmall, template.name.as_str(), t.text));
                                                ui.add(
                                                    egui::Label::new(typography::text(
                                                        Role::Caption,
                                                        if template.subject.is_empty() { "Senza oggetto" } else { template.subject.as_str() },
                                                        t.text_faint,
                                                    ))
                                                    .truncate(),
                                                );
                                            });
                                        },
                                        |ui| {
                                            if Button::ghost("Modifica").size(ButtonSize::Sm).show(ui).clicked() {
                                                template_edit = Some(template.clone());
                                            }
                                            if Button::danger("Elimina").size(ButtonSize::Sm).show(ui).clicked() {
                                                template_delete = Some(template.clone());
                                            }
                                        },
                                    );
                                },
                            );
                            ui.add_space(space::XS);
                        }
                        if Button::new("Nuovo modello")
                            .icon(icons::PLUS)
                            .size(ButtonSize::Sm)
                            .enabled(!self.demo)
                            .disabled_reason("La modalità demo non ha un archivio locale su cui salvare i modelli")
                            .show(ui)
                            .clicked()
                        {
                            template_new = true;
                        }

                        ui.add_space(space::MD);
                        let _identities = ui.label(typography::strong(Role::BodySmall, "MITTENTI", t.text_faint));
                        #[cfg(test)]
                        probe::settings_section(ui, "MITTENTI", _identities.rect);
                        ui.add_space(space::XS);
                        ui.label(typography::text(
                            Role::Meta,
                            "Indirizzi alternativi per il campo Da. Il provider deve autorizzarli per l'invio; il client non cambia nulla sul server.",
                            t.text_faint,
                        ));
                        if !self.demo
                            && !self.identities_loaded
                            && let Some(notice) = &self.identities_notice
                        {
                            ui.label(typography::text(Role::Caption, notice.as_str(), t.danger));
                        }
                        if self.identities.is_empty() {
                            ui.label(typography::text(Role::BodySmall, "Nessun mittente alternativo.", t.text_muted));
                        }
                        for identity in &self.identities {
                            Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::XS)).show(
                                ui,
                                |ui| {
                                    Sides::new().show(
                                        ui,
                                        |ui| {
                                            ui.vertical(|ui| {
                                                ui.spacing_mut().item_spacing.y = 1.0;
                                                ui.label(typography::text(Role::BodySmall, format!("{} <{}>", identity.name, identity.email), t.text));
                                                ui.add(
                                                    egui::Label::new(typography::text(
                                                        Role::Caption,
                                                        if identity.signature.trim().is_empty() { "Senza firma" } else { "Firma impostata" },
                                                        t.text_faint,
                                                    ))
                                                    .truncate(),
                                                );
                                            });
                                        },
                                        |ui| {
                                            if Button::ghost("Modifica").size(ButtonSize::Sm).show(ui).clicked() {
                                                identity_edit = Some(identity.clone());
                                            }
                                            if Button::danger("Elimina").size(ButtonSize::Sm).show(ui).clicked() {
                                                identity_delete = Some(identity.clone());
                                            }
                                        },
                                    );
                                },
                            );
                            ui.add_space(space::XS);
                        }
                        if Button::new("Nuovo mittente")
                            .icon(icons::PLUS)
                            .size(ButtonSize::Sm)
                            .enabled(!self.demo)
                            .disabled_reason("La modalità demo non ha un archivio locale su cui salvare i mittenti")
                            .show(ui)
                            .clicked()
                        {
                            identity_new = true;
                        }
                    });
                #[cfg(test)]
                probe::settings_scroll(ui, _settings_scroll.inner_rect, _settings_scroll.content_size, _settings_scroll.state.offset);
                ui.add_space(space::SM);
                let _footer = ui.horizontal(|ui| {
                    ui.label(typography::text(Role::Caption, "Esc chiude e riporta al punto precedente", t.text_faint));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if Button::new("Chiudi").show(ui).clicked() {
                            close = true;
                        }
                    });
                });
                #[cfg(test)]
                probe::settings_footer(ui, _footer.response.rect);
            });
        // A toggle was clicked inside the modal closure; write it now and read the stored value back.
        self.apply_background_change();
        if let Some((system, dark)) = theme_choice {
            if system {
                self.set_system_theme(ui.ctx(), true);
            } else {
                self.set_dark(ui.ctx(), dark);
            }
        }
        if reduced != self.reduced_motion {
            self.set_reduced_motion(ui.ctx(), reduced);
        }
        if reset_pane {
            self.reading_ratio = crate::settings::DEFAULT_READING_PANE_RATIO;
            self.mark_settings_dirty();
            self.persist_settings();
            self.status = "Larghezza lettura reimpostata".to_owned();
        }
        if let Some(id) = pgp_export {
            self.export_pgp_key(&id, ui.ctx());
        }
        if let Some(key) = pgp_delete {
            self.pgp_delete = Some(PgpDeleteState { key, focus_cancel: true });
        }
        if pgp_import {
            self.open_pgp_import();
        }
        if template_new {
            self.open_template_editor(None);
        }
        if let Some(template) = template_edit {
            self.open_template_editor(Some(&template));
        }
        if let Some(template) = template_delete {
            self.template_delete = Some(template);
        }
        if identity_new {
            self.open_identity_editor(None);
        }
        if let Some(identity) = identity_edit {
            self.open_identity_editor(Some(&identity));
        }
        if let Some(identity) = identity_delete {
            self.identity_delete = Some(identity);
        }
        if let Some(account_id) = signature_open {
            let text =
                self.engine.accounts().iter().find(|account| account.id == account_id).map(|account| account.signature.clone()).unwrap_or_default();
            self.signature_editor = Some(SignatureEditor { account: account_id, text, focus: true, error: None });
        }
        if signature_cancel && let Some(editor) = self.signature_editor.take() {
            self.signature_focus_account = Some(editor.account);
        }
        if let Some(account_id) = signature_save {
            let text = self.signature_editor.as_ref().map(|editor| editor.text.clone()).unwrap_or_default();
            let result = self.op("account_signature_set", json!({ "account": account_id, "signature": text }));
            match result {
                Ok(_) => {
                    self.signature_editor = None;
                    self.signature_focus_account = Some(account_id);
                    self.status = "Firma salvata".to_owned();
                }
                Err(error) => {
                    if let Some(editor) = self.signature_editor.as_mut() {
                        editor.error = Some(signature_error_message(&error.to_string()));
                    }
                }
            }
        }
        if open_folder {
            self.open_data_folder();
        }
        if open_rules {
            self.close_settings(ui.ctx());
            self.open_rules(false);
        }
        if licences {
            self.open_licences(ui.ctx());
        }
        if shortcuts {
            self.close_settings(ui.ctx());
            self.show_shortcuts = true;
            self.shortcut_query.clear();
            self.shortcut_focus = true;
        }
        if close || response.should_close() {
            self.close_settings(ui.ctx());
        }
        if leave_demo {
            self.close_settings(ui.ctx());
            self.leave_demo();
            return;
        }
        if add {
            self.show_accounts_dialog = false;
            self.settings_return_focus = None;
            self.onboarding = Some(Onboarding::add_account());
        }
        if let Some(state) = remove {
            self.account_remove = Some(state);
        }
    }

    fn licences_dialog(&mut self, ui: &mut Ui) {
        if !self.show_licences {
            return;
        }
        let t = theme::tokens(self.dark);
        let focus_close = self.licences_focus_pending;
        self.licences_focus_pending = false;
        let width = (ui.ctx().content_rect().width() - 2.0 * space::HUGE).clamp(420.0, 640.0);
        let height = (ui.ctx().content_rect().height() - 2.0 * space::XL).clamp(280.0, 560.0);
        let mut close = false;
        let response = egui::Modal::new(Id::new("postino-licences"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XXL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.label(typography::strong(Role::Section, "Licenze e attribuzioni", t.text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let button = Button::ghost("Chiudi").show(ui);
                        if focus_close {
                            button.request_focus();
                        }
                        if button.clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(space::SM);
                ui.label(typography::text(Role::Meta, "Il documento generato da ATTRIBUTION.toml per questa build.", t.text_muted));
                ui.add_space(space::SM);
                egui::ScrollArea::vertical().id_salt("licences-scroll").max_height((height - 120.0).max(160.0)).auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.add(egui::Label::new(typography::mono(include_str!("../../../ATTRIBUTION.md"), 10.5, t.text_muted)).selectable(true));
                    },
                );
                ui.add_space(space::SM);
                ui.horizontal(|ui| {
                    ui.label(typography::text(Role::Caption, "Esc chiude", t.text_faint));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if Button::new("Chiudi").show(ui).clicked() {
                            close = true;
                        }
                    });
                });
            });
        if close || response.should_close() {
            self.close_licences(ui.ctx());
        }
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
        self.pgp_reading = None;
        self.pgp_checked = None;
        self.pgp_prompt = None;
        self.pgp_import = None;
        self.pgp_delete = None;
        self.unsubscribe_info = None;
        self.unsubscribe_checked = None;
        self.show_unsubscribe = false;
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
                self.pgp_reading = None;
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
                self.pgp_reading = None;
                self.refresh_pgp_keys();
                self.pgp_notice = Some("Chiave PGP eliminata.".to_owned());
            }
            Err(error) => self.pgp_notice = Some(format!("Eliminazione non riuscita: {error}")),
        }
    }

    fn pgp_prompt_dialog(&mut self, ui: &mut Ui) {
        let Some(mut prompt) = self.pgp_prompt.take() else { return };
        let t = theme::tokens(self.dark);
        let (title, body, action) = match prompt.purpose {
            PgpPromptPurpose::Decrypt => (
                "Passphrase della chiave PGP",
                "La passphrase apre la chiave privata solo per questa decifratura. Non viene salvata, non entra nel testo della mail e non viene indicizzata.",
                "Decifra",
            ),
            PgpPromptPurpose::Send => (
                "Firma con la chiave PGP",
                "La passphrase sblocca la chiave privata solo per questo invio. Il messaggio parte firmato: se la chiave non si apre, l'invio viene annullato.",
                "Firma e invia",
            ),
        };
        let mut confirm = false;
        let mut cancel = false;
        let focus = prompt.focus;
        prompt.focus = false;
        let response = egui::Modal::new(Id::new("postino-pgp-passphrase"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(440.0);
                ui.label(typography::strong(Role::Emphasis, title, t.text));
                ui.add_space(space::SM);
                ui.label(typography::text(Role::Secondary, body, t.text_muted));
                ui.add_space(space::MD);
                TextField::new(&mut prompt.value, "Passphrase").label("PASSPHRASE").password(true).width(ui.available_width()).focus(focus).show(ui);
                if let Some(error) = prompt.error.as_deref() {
                    ui.label(typography::text(Role::Meta, error, t.danger));
                }
                ui.add_space(space::LG);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    confirm = Button::primary(action).show(ui).clicked();
                    cancel = Button::ghost("Annulla").show(ui).clicked();
                });
                if ui.input(|input| input.key_pressed(Key::Enter)) && !prompt.value.is_empty() {
                    confirm = true;
                }
            });
        let escaped = response.should_close();
        if confirm && !prompt.value.is_empty() {
            let value = std::mem::take(&mut prompt.value);
            match prompt.purpose {
                PgpPromptPurpose::Decrypt => {
                    let Some(id) = self.pgp_reading.as_ref().map(|state| state.message_id.clone()) else { return };
                    match self.pgp_decrypt(&id, value.as_str()) {
                        Some(result) if result.state == PgpDecryptState::WrongPassphrase => {
                            prompt.error = Some("La passphrase non apre la chiave. Riprova.".to_owned());
                            prompt.focus = true;
                            self.pgp_prompt = Some(prompt);
                        }
                        Some(result) => self.apply_pgp_decrypt(result),
                        None => self.pgp_prompt = Some(prompt),
                    }
                }
                PgpPromptPurpose::Send => self.send_composer_with(Some(value.as_str())),
            }
        } else if cancel || escaped {
            self.pgp_prompt = None;
        } else {
            self.pgp_prompt = Some(prompt);
        }
    }

    fn pgp_import_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.pgp_import.as_mut() else { return };
        let t = theme::tokens(self.dark);
        let mut submit = false;
        let mut cancel = false;
        let focus = state.focus_armor;
        state.focus_armor = false;
        let response = egui::Modal::new(Id::new("postino-pgp-import"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(520.0);
                ui.label(typography::strong(Role::Emphasis, "Importa una chiave PGP", t.text));
                ui.add_space(space::SM);
                ui.label(typography::text(
                    Role::Secondary,
                    "Incolla un blocco armored pubblico o privato. Una chiave privata deve essere già protetta: la passphrase serve solo a verificarla e non viene salvata.",
                    t.text_muted,
                ));
                ui.add_space(space::MD);
                TextField::new(&mut state.armor, "-----BEGIN PGP PRIVATE KEY BLOCK-----")
                    .label("CHIAVE ARMORED")
                    .multiline(6)
                    .width(ui.available_width())
                    .focus(focus)
                    .show(ui);
                ui.add_space(space::SM);
                TextField::new(&mut state.passphrase, "Passphrase della chiave privata")
                    .label("PASSPHRASE · SOLO PER UNA CHIAVE PRIVATA")
                    .password(true)
                    .width(ui.available_width())
                    .show(ui);
                if let Some(error) = state.error.as_deref() {
                    ui.label(typography::text(Role::Meta, error, t.danger));
                }
                ui.add_space(space::LG);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let import = Button::primary("Importa").enabled(!state.busy && !state.armor.trim().is_empty());
                    submit = import.show(ui).clicked();
                    cancel = Button::ghost("Annulla").show(ui).clicked();
                });
            });
        if submit {
            let Some(mut state) = self.pgp_import.take() else { return };
            state.busy = true;
            self.submit_pgp_import(state);
        } else if cancel || response.should_close() {
            self.pgp_import = None;
        }
    }

    fn pgp_delete_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.pgp_delete.clone() else { return };
        let name = state.key.user_id.as_deref().unwrap_or("questa chiave");
        let body = format!(
            "Eliminare la chiave PGP “{name}” ({})? La chiave pubblica e, se presente, quella privata vengono rimosse da questo archivio cifrato. Le mail già firmate o cifrate restano nel loro stato, ma non potranno più essere verificate o decifrate con questa chiave.",
            state.key.fingerprint
        );
        let mut confirm = false;
        let mut cancel = false;
        let response =
            egui::Modal::new(Id::new("postino-pgp-delete")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(ui.ctx(), |ui| {
                let response = Dialog::new("Eliminare la chiave PGP?", body.as_str(), "Elimina")
                    .secondary("Annulla")
                    .danger(true)
                    .width(480.0)
                    .focus_secondary(state.focus_cancel)
                    .show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            });
        if confirm {
            self.pgp_delete = None;
            self.delete_pgp_key(&state.key.id);
        } else if cancel || response.should_close() {
            self.pgp_delete = None;
        }
    }

    fn template_editor_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.template_editor.as_mut() else { return };
        let t = theme::tokens(self.dark);
        let mut submit = false;
        let mut cancel = false;
        let focus_name = state.focus_name;
        state.focus_name = false;
        let title = if state.id.is_some() { "Modifica modello" } else { "Nuovo modello" };
        let response = egui::Modal::new(Id::new("postino-template-editor"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(520.0);
                ui.label(typography::strong(Role::Emphasis, title, t.text));
                ui.add_space(space::SM);
                ui.label(typography::text(
                    Role::Secondary,
                    "Il modello è testo locale: viene inserito nel composer e non viene inviato finché non salvi o invii una bozza.",
                    t.text_muted,
                ));
                ui.add_space(space::MD);
                TextField::new(&mut state.name, "Nome del modello").label("NOME").width(ui.available_width()).focus(focus_name).show(ui);
                ui.add_space(space::SM);
                TextField::new(&mut state.subject, "Oggetto predefinito").label("OGGETTO").width(ui.available_width()).show(ui);
                ui.add_space(space::SM);
                TextField::new(&mut state.body, "Testo del modello").label("TESTO").multiline(5).width(ui.available_width()).show(ui);
                if let Some(error) = state.error.as_deref() {
                    ui.label(typography::text(Role::Meta, error, t.danger));
                }
                ui.add_space(space::LG);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let save = Button::primary("Salva").enabled(!state.name.trim().is_empty());
                    submit = save.show(ui).clicked();
                    cancel = Button::ghost("Annulla").show(ui).clicked();
                });
            });
        if submit {
            self.submit_template_editor();
        } else if cancel || response.should_close() {
            self.template_editor = None;
        }
    }

    fn template_delete_dialog(&mut self, ui: &mut Ui) {
        let Some(template) = self.template_delete.clone() else { return };
        let body = format!(
            "Eliminare il modello «{}»? Il testo viene rimosso da questo archivio cifrato. Le bozze che lo hanno già usato non cambiano.",
            template.name
        );
        let mut confirm = false;
        let mut cancel = false;
        let response =
            egui::Modal::new(Id::new("postino-template-delete")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(ui.ctx(), |ui| {
                let response = Dialog::new("Eliminare il modello?", body.as_str(), "Elimina").secondary("Annulla").danger(true).width(480.0).show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            });
        if confirm {
            self.template_delete = None;
            self.delete_template(&template);
        } else if cancel || response.should_close() {
            self.template_delete = None;
        }
    }

    fn identity_editor_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.identity_editor.as_mut() else { return };
        let t = theme::tokens(self.dark);
        let mut submit = false;
        let mut cancel = false;
        let focus_name = state.focus_name;
        state.focus_name = false;
        let title = if state.id.is_some() { "Modifica mittente" } else { "Nuovo mittente" };
        let response = egui::Modal::new(Id::new("postino-identity-editor"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(520.0);
                ui.label(typography::strong(Role::Emphasis, title, t.text));
                ui.add_space(space::SM);
                ui.label(typography::text(
                    Role::Secondary,
                    "Il mittente resta in questo archivio cifrato. Il provider deve gia accettare l'indirizzo per l'invio: Postino non crea alias sul server.",
                    t.text_muted,
                ));
                ui.add_space(space::MD);
                TextField::new(&mut state.name, "Nome mostrato").label("NOME").width(ui.available_width()).focus(focus_name).show(ui);
                ui.add_space(space::SM);
                TextField::new(&mut state.email, "nome@esempio.it").label("INDIRIZZO").width(ui.available_width()).show(ui);
                ui.add_space(space::SM);
                TextField::new(&mut state.signature, "Firma per i messaggi scritti da questo indirizzo").label("FIRMA").multiline(4).width(ui.available_width()).show(ui);
                if let Some(error) = state.error.as_deref() {
                    ui.label(typography::text(Role::Meta, error, t.danger));
                }
                ui.add_space(space::LG);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let save = Button::primary("Salva").enabled(!state.name.trim().is_empty() && !state.email.trim().is_empty());
                    submit = save.show(ui).clicked();
                    cancel = Button::ghost("Annulla").show(ui).clicked();
                });
            });
        if submit {
            self.submit_identity_editor();
        } else if cancel || response.should_close() {
            self.identity_editor = None;
        }
    }

    fn identity_delete_dialog(&mut self, ui: &mut Ui) {
        let Some(identity) = self.identity_delete.clone() else { return };
        let body = format!(
            "Eliminare {} <{}>? Le bozze gia scritte da questo indirizzo non cambiano, ma non potranno essere inviate finche non scegli un mittente valido.",
            identity.name, identity.email
        );
        let mut confirm = false;
        let mut cancel = false;
        let response =
            egui::Modal::new(Id::new("postino-identity-delete")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(ui.ctx(), |ui| {
                let response =
                    Dialog::new("Eliminare il mittente?", body.as_str(), "Elimina").secondary("Annulla").danger(true).width(480.0).show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            });
        if confirm {
            self.delete_identity(&identity);
        } else if cancel || response.should_close() {
            self.identity_delete = None;
        }
    }

    fn account_remove_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.account_remove.clone() else {
            return;
        };
        let body = if state.last {
            format!(
                "L'account “{}” ({}) è l'ultimo. Rimuovendolo Postino elimina dal computer le mail scaricate, le cartelle e la password nel portachiavi, e torna alla configurazione iniziale. Le mail sul server non vengono toccate. La cache cifrata degli allegati può restare sul disco: non contiene nomi o indirizzi in chiaro.",
                state.name, state.email
            )
        } else {
            format!(
                "Rimuovendo “{}” ({}) Postino elimina da questo computer le mail scaricate, le cartelle e la password nel portachiavi. Le mail sul server non vengono toccate. La cache cifrata degli allegati può restare sul disco: non contiene nomi o indirizzi in chiaro.",
                state.name, state.email
            )
        };
        let mut confirm = false;
        let mut cancel = false;
        let focus_cancel = state.focus_cancel;
        if let Some(state) = self.account_remove.as_mut() {
            state.focus_cancel = false;
        }
        let response =
            egui::Modal::new(Id::new("postino-remove-account")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(ui.ctx(), |ui| {
                let response = Dialog::new("Rimuovere l'account?", &body, "Rimuovi")
                    .secondary("Annulla")
                    .danger(true)
                    .width(460.0)
                    .focus_secondary(focus_cancel)
                    .show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            });
        if confirm {
            self.account_remove = None;
            self.remove_account(&state);
        } else if cancel || response.should_close() {
            self.account_remove = None;
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let kind = self.status_kind();
        let composer_can_send = self.send_allowed_for_active_account();
        let configuring = self.onboarding.is_some();
        let recovering = self.boot_failure.is_some();
        let mut leave_demo = false;
        egui::Panel::bottom("status")
            .exact_size(ui_size::STATUS)
            .frame(Frame::new().fill(t.panel_alt).inner_margin(Margin::symmetric(inset::LG, inset::XS)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    let dot = match kind {
                        StatusKind::Demo => t.gold_ink,
                        StatusKind::Ready => t.success,
                        StatusKind::Offline => t.gold_ink,
                        StatusKind::Error => t.danger,
                    };
                    ui.painter().circle_filled(rect.center(), 3.0, dot);
                    match kind {
                        StatusKind::Demo => {
                            // The one demo notice in the product: a badge with one sentence of
                            // explanation, not four scattered milestone lines.
                            let badge = Badge::new("Modalità demo").kind(BadgeKind::Accent).show(ui);
                            badge.on_hover_text(DEMO_TOOLTIP);
                            if self.demo_can_return() {
                                ui.add_space(space::SM);
                                let back = Button::ghost("Torna all'archivio").size(ButtonSize::Sm).show(ui);
                                #[cfg(test)]
                                probe::demo_return_button(ui, back.rect);
                                if back.clicked() {
                                    leave_demo = true;
                                }
                            }
                        }
                        StatusKind::Ready => {
                            ui.label(typography::text(Role::Meta, if configuring { "Configurazione" } else { "Pronto" }, t.text_muted));
                        }
                        StatusKind::Offline => {
                            // The sync line starts with "Offline ·": one phrase, not two.
                        }
                        StatusKind::Error => {
                            if !(self.failure.is_none() && matches!(self.sync_view, SyncView::Error { .. })) {
                                ui.label(typography::text(Role::Meta, "Errore", t.danger));
                            }
                        }
                    }
                    ui.add_space(space::SM);
                    ui.label(typography::mono(format!("Postino {}", env!("CARGO_PKG_VERSION")), 10.5, t.text_faint));
                    // Who owns the store is part of the product surface: the window must never
                    // look like the only writer while a daemon holds the archive.
                    if !self.demo && self.boot_failure.is_none() {
                        ui.add_space(space::MD);
                        let color = match self.connection {
                            daemon::Connection::Connected(_) => t.success,
                            daemon::Connection::Reconnecting { .. } | daemon::Connection::Lost => t.gold_ink,
                            _ => t.text_muted,
                        };
                        let chip = ui.add(egui::Label::new(typography::text(Role::Meta, self.connection.label(), color)).truncate());
                        chip.on_hover_text(self.connection.detail());
                        if self.connection.can_start(self.demo) {
                            ui.add_space(space::XS);
                            if Button::ghost("Avvia daemon").size(ButtonSize::Sm).show(ui).clicked() {
                                self.request_start_daemon();
                            }
                        } else if self.connection.remote() {
                            ui.add_space(space::XS);
                            if Button::ghost("Ferma daemon").size(ButtonSize::Sm).show(ui).clicked() {
                                self.request_stop_daemon();
                            }
                        }
                    }
                    if self.boot_failure.is_none()
                        && self.onboarding.is_none()
                        && let Some(text) = self.sync_text()
                    {
                        ui.add_space(space::MD);
                        let color = match self.sync_view {
                            SyncView::Error { .. } => t.danger,
                            SyncView::Offline { .. } => t.gold_ink,
                            _ => t.text_muted,
                        };
                        ui.add(egui::Label::new(typography::text(Role::Meta, text, color)).truncate());
                        // The banner lives in the list panel. The list is hidden by the 980 overlay
                        // **and** by full-screen reading at any width, so the status bar must carry
                        // the one Retry in both cases.
                        let reading_hides_list = self.reading_open && (self.reading_full || reading_overlay(ui.ctx().content_rect().width()));
                        let reading_offline = reading_hides_list && matches!(self.sync_view, SyncView::Offline { .. });
                        if (self.sync_error_needs_retry() || reading_offline) && self.can_add_account() {
                            ui.add_space(space::XS);
                            let retry = Button::ghost("Riprova").size(ButtonSize::Sm).show(ui);
                            #[cfg(test)]
                            probe::retry(ui, retry.rect);
                            if retry.clicked() {
                                self.request_sync();
                            }
                        }
                    }
                    if !self.status.is_empty() {
                        ui.add_space(space::MD);
                        ui.add(egui::Label::new(typography::text(Role::Meta, self.status.as_str(), t.text_muted)).truncate());
                    }
                    let active_outbox = self
                        .outbox
                        .entries
                        .iter()
                        .filter(|entry| {
                            matches!(
                                entry.state,
                                postino_store::OutboxState::Queued
                                    | postino_store::OutboxState::Sending
                                    | postino_store::OutboxState::AppendPending
                                    | postino_store::OutboxState::Uncertain
                                    | postino_store::OutboxState::Failed
                            )
                        })
                        .count();
                    if active_outbox > 0 {
                        ui.add_space(space::SM);
                        let uncertain = self.outbox.entries.iter().any(|entry| entry.state == postino_store::OutboxState::Uncertain);
                        let label = format!("Coda · {active_outbox}");
                        let button = if uncertain {
                            Button::danger(label.as_str()).size(ButtonSize::Sm).show(ui)
                        } else {
                            Button::ghost(label.as_str()).size(ButtonSize::Sm).show(ui)
                        };
                        #[cfg(test)]
                        probe::outbox_button(ui, button.rect);
                        if button.clicked() {
                            self.refresh_outbox();
                            self.outbox_open = true;
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let hint = if recovering {
                            "Invio riprova · la modalità demo non tocca il file"
                        } else if configuring {
                            "Invio continua · Esc torna indietro"
                        } else if self.composer.open {
                            if composer_can_send {
                                "Ctrl+Invio invia · Esc chiude e conserva"
                            } else {
                                "Invio non disponibile · Esc chiude e conserva"
                            }
                        } else {
                            "Ctrl+K comandi · J/K naviga · ? scorciatoie"
                        };
                        ui.label(typography::text(Role::Meta, hint, t.text_faint));
                    });
                });
            });
        if leave_demo {
            self.leave_demo();
        }
    }

    fn about(&mut self, ui: &mut Ui) {
        if !self.show_about {
            return;
        }
        let t = theme::tokens(self.dark);
        let mut open = self.show_about;
        egui::Window::new("Informazioni su Postino").open(&mut open).collapsible(false).resizable(false).default_width(420.0).show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                logo(ui, 48.0);
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Postino").font(FontId::new(22.0, theme::display())).color(t.text));
                    ui.label(RichText::new(format!("Versione {}", env!("CARGO_PKG_VERSION"))).size(11.0).color(t.text_muted));
                });
            });
            ui.add_space(12.0);
            ui.label(
                RichText::new("Un client email locale, veloce e agent-first. La stessa superficie di operazioni alimenta app, CLI e MCP.")
                    .size(12.5)
                    .color(t.text_muted),
            );
            ui.add_space(14.0);
            for (name, hex, color) in [("Verde bosco", "#173D32", theme::FOREST), ("Avorio", "#F5EBD6", theme::IVORY), ("Oro caldo", "#E4B96A", GOLD)]
            {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), Sense::hover());
                    ui.painter().rect_filled(rect, CornerRadius::same(4), color);
                    ui.painter().rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
                    ui.label(RichText::new(name).size(12.0).color(t.text));
                    ui.label(RichText::new(hex).font(FontId::new(11.0, FontFamily::Monospace)).color(t.text_faint));
                });
            }
            ui.add_space(12.0);
            ui.label(RichText::new("Inter · JetBrains Mono (OFL).").size(10.5).color(t.text_faint));
            ui.label(RichText::new("MIT OR Apache-2.0").size(10.5).color(t.text_faint));
        });
        self.show_about = open;
    }

    /// The command palette. It is a modal layer, not a window: no title band, no close button, a
    /// scrim that blocks the shell, the search field on top, ten rows with an internal scroll and
    /// the keyboard hint at the bottom. The command table remains the only source of rows.
    fn command_palette(&mut self, ui: &mut Ui) {
        if !self.show_palette {
            return;
        }
        let t = theme::tokens(self.dark);
        let ctx = ui.ctx().clone();
        let mut chosen = None;
        // Consume the navigation keys before the text field sees them. Otherwise the caret
        // swallows the arrows and only the first command can ever run.
        let down = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowDown));
        let up = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowUp));
        let enter = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
        let navigated = down || up;
        if navigated {
            // egui records the arrow as a focus-navigation intent at the start of the pass, before
            // this handler consumes it; without this reset the field loses focus to the first row.
            ctx.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
        }
        let window = ui.ctx().content_rect();
        let outer_width = ui_size::PALETTE_WIDTH.min((window.width() - 2.0 * space::XL).max(240.0));
        let content_width = (outer_width - 2.0 * f32::from(inset::XL)).max(120.0);
        let top = window.height() * ui_size::PALETTE_TOP_RATIO;
        let id = Id::new("postino-palette");
        let frame = Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL));
        let response = egui::Modal::new(id)
            .area(egui::Modal::default_area(id).anchor(Align2::CENTER_TOP, Vec2::new(0.0, top)).constrain(false))
            .backdrop_color(theme::scrim(self.dark))
            .frame(frame)
            .show(&ctx, |ui| {
                ui.set_width(content_width);
                ui.spacing_mut().scroll = crate::ui::tokens::scroll_style();
                let search = TextField::new(&mut self.palette_query, "Cerca un comando…")
                    .id_salt("postino-palette-search")
                    .icon(icons::SEARCH)
                    .width(content_width)
                    .focus(self.palette_focus)
                    .show(ui);
                self.palette_focus = false;
                if search.changed() {
                    self.palette_selected = 0;
                }
                // A palette keeps the keyboard in its field. Clicking a row may move focus to the
                // button for one frame; the next frame is already typing again.
                if !search.has_focus() {
                    search.request_focus();
                }
                let filtered = filtered_commands(&self.palette_query);
                if filtered.is_empty() {
                    ui.add_space(space::MD);
                    ui.label(typography::text(Role::BodySmall, "Nessun comando corrisponde.", t.text_muted));
                } else {
                    self.palette_selected = self.palette_selected.min(filtered.len() - 1);
                    if down {
                        self.palette_selected = (self.palette_selected + 1) % filtered.len();
                    }
                    if up {
                        self.palette_selected = (self.palette_selected + filtered.len() - 1) % filtered.len();
                    }
                    if enter && let Some(command) = filtered.get(self.palette_selected) {
                        if self.command_enabled(command) {
                            chosen = Some(command.id);
                        } else {
                            // Enter on a disabled command must say why instead of doing nothing.
                            self.status = format!("{}: {}", command.label, command.rule.disabled_reason());
                        }
                    }
                    ui.add_space(space::SM);
                    let rows = filtered.len().min(10) as f32;
                    // Reserve the reason line before sizing the list: at 980×600 ten rows plus the
                    // reason must still end above the status bar.
                    let disabled_reason = filtered
                        .get(self.palette_selected)
                        .filter(|command| !self.command_enabled(command))
                        .and_then(|command| command.rule.reason());
                    let reason_height = if disabled_reason.is_some() { ui_size::PALETTE_REASON } else { 0.0 };
                    let available = (window.height() - ui_size::STATUS - top - ui_size::PALETTE_CHROME - reason_height).max(ui_size::PALETTE_ROW);
                    let height = (rows * ui_size::PALETTE_ROW).min(ui_size::PALETTE_RESULTS_MAX).min(available);
                    egui::ScrollArea::vertical().id_salt("palette-results").max_height(height).auto_shrink([false, true]).show(ui, |ui| {
                        for (index, command) in filtered.iter().enumerate() {
                            let enabled = self.command_enabled(command);
                            let selected = index == self.palette_selected;
                            let response = Button::ghost(command.label)
                                .icon(command.icon)
                                .shortcut(command.shortcut)
                                .full_width()
                                .selected(selected)
                                .enabled(enabled)
                                .disabled_reason(command.rule.disabled_reason())
                                .show(ui);
                            if chosen.is_none() && response.clicked() {
                                chosen = Some(command.id);
                            }
                            if selected && navigated {
                                response.scroll_to_me(None);
                            }
                        }
                    });
                    // Hover is not a visible affordance on the keyboard path: spell the refusal out
                    // under the list whenever the selected command is unavailable.
                    if let Some(reason) = disabled_reason {
                        ui.add_space(space::XS);
                        ui.label(typography::text(Role::Meta, reason, t.text_muted));
                    }
                }
                ui.add_space(space::XS);
                ui.add(egui::Separator::default().spacing(1.0));
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "↑↓ scegli · Invio esegue · Esc chiude", t.text_faint));
            });
        #[cfg(test)]
        probe::palette(ui, response.response.rect);
        if response.should_close() {
            self.show_palette = false;
        }
        if !self.show_palette {
            ctx.memory_mut(|memory| memory.stop_text_input());
        }
        if let Some(id) = chosen {
            self.execute_command(id, &ctx);
        }
    }

    fn shortcuts_sheet(&mut self, ui: &mut Ui) {
        if !self.show_shortcuts {
            return;
        }
        let t = theme::tokens(self.dark);
        let mut open = self.show_shortcuts;
        egui::Window::new("Scorciatoie")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(420.0)
            .show(ui.ctx(), |ui| {
                ui.label(typography::text(Role::Meta, "La posta si usa anche senza mouse.", t.text_muted));
                ui.add_space(space::SM);
                let search = ui.add_sized(
                    [ui.available_width(), 32.0],
                    egui::TextEdit::singleline(&mut self.shortcut_query).hint_text("Cerca una scorciatoia…"),
                );
                if self.shortcut_focus {
                    search.request_focus();
                    self.shortcut_focus = false;
                }
                ui.add_space(space::MD);
                let query = self.shortcut_query.trim().to_lowercase();
                let commands = COMMANDS
                    .iter()
                    .filter(|command| !command.shortcut.is_empty())
                    .filter(|command| {
                        query.is_empty() || command.label.to_lowercase().contains(&query) || command.shortcut.to_lowercase().contains(&query)
                    })
                    .collect::<Vec<_>>();
                if commands.is_empty() {
                    ui.label(typography::text(Role::BodySmall, "Nessuna scorciatoia corrisponde.", t.text_muted));
                } else {
                    egui::Grid::new("shortcuts-grid").num_columns(2).spacing([space::XXL, space::SM]).show(ui, |ui| {
                        for command in commands {
                            ui.label(typography::text(Role::BodySmall, command.label, t.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(typography::mono(command.shortcut, 10.5, t.text_faint));
                            });
                            ui.end_row();
                        }
                    });
                }
                ui.add_space(space::MD);
                ui.separator();
                ui.add_space(space::SM);
                ui.label(typography::strong(Role::BodySmall, "Sintassi di ricerca", t.text));
                ui.label(typography::text(
                    Role::Meta,
                    "from: to: cc: bcc: subject: label: folder: is:unread|read|starred|unstarred \
                     has:attachment before:2026-01-31 after:2026-01-01 larger:2m smaller:500k \
                     \"frase\" prefisso* AND OR NOT (…). Date UTC; Esc cancella la ricerca.",
                    t.text_muted,
                ));
            });
        self.show_shortcuts = open;
    }

    /// The searchable checkbox menu opened by `L`, by the row context menu and by the reading
    /// shortcut. It is modal: a scrim blocks the shell behind it, Escape or an outside click
    /// closes it, and the targets captured when it opened are the only messages it can touch.
    fn label_menu_window(&mut self, ui: &mut Ui) {
        if self.label_menu.is_none() {
            return;
        }
        // The selection, folder or filters changed while the menu was open: the user is no longer
        // looking at the targets they acted on, so drop the menu instead of labelling blind.
        if self.label_menu_is_stale() {
            self.label_menu = None;
            return;
        }
        let t = theme::tokens(self.dark);
        let labels = self.labels_cache.clone();
        let Some(state) = self.label_menu.as_ref() else {
            return;
        };
        let target = state.context.target.clone();
        let mut query = state.query.clone();
        let mut focus = state.focus;
        // One pass over the targets, not one scan per label × target.
        let mut applied_counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for message_id in &target {
            if let Some(message) = self.engine.message(message_id) {
                for id in &message.summary.labels {
                    *applied_counts.entry(id.clone()).or_insert(0) += 1;
                }
            }
        }
        let mut toggle = None;
        let mut create = false;
        #[cfg(test)]
        probe::clear_menu_rows(ui);
        let response = egui::Modal::new(Id::new("postino-label-menu"))
            // Over the list, not the reading pane: the chips behind stay visible and the menu sits
            // next to the rows it labels.
            .area(egui::Modal::default_area(Id::new("postino-label-menu")).anchor(Align2::LEFT_TOP, Vec2::new(ui_size::SIDEBAR + 24.0, 120.0)))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(304.0);
                ui.label(typography::strong(Role::BodySmall, "Etichetta", t.text));
                ui.add_space(space::XS);
                ui.label(typography::text(
                    Role::Meta,
                    format!("{} selezionat{}", messages_text(target.len()), if target.len() == 1 { "o" } else { "i" }),
                    t.text_muted,
                ));
                ui.add_space(space::SM);
                let focus_now = focus;
                focus = false;
                SearchField::new(&mut query, "Cerca etichetta…").width(304.0).focus(focus_now).show(ui);
                ui.add_space(space::SM);
                let lowercase = query.trim().to_lowercase();
                let filtered =
                    labels.iter().filter(|label| lowercase.is_empty() || label.name.to_lowercase().contains(&lowercase)).collect::<Vec<_>>();
                if labels.is_empty() {
                    Button::new("Nessuna etichetta")
                        .enabled(false)
                        .disabled_reason("Crea la prima etichetta con “Nuova etichetta…”")
                        .full_width()
                        .show(ui);
                } else if filtered.is_empty() {
                    ui.label(typography::text(Role::BodySmall, "Nessuna etichetta corrisponde.", t.text_muted));
                } else {
                    egui::ScrollArea::vertical().max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
                        for label in filtered {
                            let applied = applied_counts.get(&label.id).copied().unwrap_or(0);
                            let all = !target.is_empty() && applied == target.len();
                            let some = applied > 0 && !all;
                            // The coloured dot carries the label identity, matching the sidebar, the
                            // header and the reading chips; the trailing glyph carries the state.
                            let mut row = Button::ghost(label.name.as_str()).dot(label_color(label.color, &t)).full_width();
                            if all {
                                row = row.trailing_icon(icons::CHECK_CIRCLE);
                            } else if some {
                                row = row.trailing_icon(icons::MINUS);
                            }
                            let response = row.show(ui);
                            #[cfg(test)]
                            probe::menu_row(ui, &label.id, response.rect);
                            if response.clicked() {
                                toggle = Some(label.id.clone());
                            }
                        }
                    });
                }
                ui.add_space(space::XS);
                if Button::ghost("Nuova etichetta…").icon(icons::PLUS).full_width().show(ui).clicked() {
                    create = true;
                }
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "Le etichette sono locali.", t.text_faint));
            });
        if let Some(menu) = self.label_menu.as_mut() {
            menu.query = query;
            menu.focus = focus;
        }
        if response.should_close() {
            self.label_menu = None;
            ui.ctx().memory_mut(|memory| memory.stop_text_input());
        }
        if let Some(id) = toggle {
            self.toggle_label_target(&id);
        }
        if create {
            self.start_label_create_for(target);
        }
    }

    fn label_delete_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.label_delete.clone() else {
            return;
        };
        let targets = plural(state.count as usize, "messaggio", "messaggi");
        let body = format!(
            "L'etichetta “{}” sarà rimossa da {targets}. Le mail non vengono eliminate; le etichette restano su questo computer. Puoi annullare per 8 secondi.",
            state.name
        );
        let mut confirm = false;
        let mut cancel = false;
        // Focus the destructive action on the first frame so Enter confirms and Esc cancels with
        // no mouse. Later frames keep whatever the user tabbed to.
        let focus_primary = state.focus_primary;
        if let Some(state) = self.label_delete.as_mut() {
            state.focus_primary = false;
        }
        let response =
            egui::Modal::new(egui::Id::new("postino-label-delete")).backdrop_color(theme::scrim(self.dark)).frame(Frame::NONE).show(ui.ctx(), |ui| {
                let response =
                    Dialog::new("Eliminare l'etichetta?", &body, "Elimina").secondary("Annulla").danger(true).focus_primary(focus_primary).show(ui);
                confirm = response.primary;
                cancel = response.secondary;
            });
        if confirm {
            self.label_delete = None;
            self.delete_label(&state);
        } else if cancel || response.should_close() {
            self.label_delete = None;
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

    /// Drive the deterministic capture. Runs only when `--screenshot` built the app.
    fn capture_screenshot(&mut self, ctx: &egui::Context) {
        let Some(mode) = self.screenshot.as_mut() else {
            return;
        };
        if !mode.requested {
            mode.frames = mode.frames.saturating_add(1);
            if mode.frames >= SCREENSHOT_SETTLE_FRAMES {
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(UserData::default()));
                mode.requested = true;
            }
            ctx.request_repaint();
            return;
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(image) = image {
            let out = mode.out.clone();
            self.screenshot = None;
            match write_png(&out, &image) {
                Ok(()) => eprintln!("postino-app: screenshot saved to {}", out.display()),
                Err(error) => {
                    eprintln!("postino-app: cannot save screenshot: {error}");
                    std::process::exit(1);
                }
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        mode.waited = mode.waited.saturating_add(1);
        if mode.waited > SCREENSHOT_WAIT_FRAMES {
            eprintln!("postino-app: the window did not return a screenshot after {SCREENSHOT_WAIT_FRAMES} frames; is it hidden or minimized?");
            std::process::exit(1);
        }
        ctx.request_repaint();
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
enum ReadingAction {
    Archive,
    Delete,
    Reply,
    ReplyAll,
    Forward,
    ExportEml,
    EditDraft,
    Star,
    Read,
    Close,
    Expand,
    Back,
    Unsubscribe,
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

#[derive(Clone, Copy)]
enum EmptyMenuAction {
    NewLabel,
    ClearLabel,
    Compose,
    ClearSearch,
    Sync,
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

/// The folder preview a docs screenshot shows. It is synthetic, like the demo mailbox.
fn demo_folders_preview() -> FoldersPreview {
    FoldersPreview {
        folders: vec![
            FolderPreview { name: "Posta in arrivo".to_owned(), role: FolderRole::Inbox, aggregate: false },
            FolderPreview { name: "Inviati".to_owned(), role: FolderRole::Sent, aggregate: false },
            FolderPreview { name: "Bozze".to_owned(), role: FolderRole::Drafts, aggregate: false },
            FolderPreview { name: "Cestino".to_owned(), role: FolderRole::Trash, aggregate: false },
            FolderPreview { name: "Spam".to_owned(), role: FolderRole::Spam, aggregate: false },
            FolderPreview { name: "Archivio".to_owned(), role: FolderRole::Archive, aggregate: false },
            FolderPreview { name: "Tutte le mail".to_owned(), role: FolderRole::Custom, aggregate: true },
        ],
        warnings: vec!["Il server non dichiara una cartella Preferiti: i preferiti restano locali.".to_owned()],
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

/// A link the reader asked to open. The confirmation sheet is the only path to the opener.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingLink {
    url: String,
    /// The host or recipient domain, already normalized by `reading::find_links`.
    host: String,
    /// The explicit `mailto:` recipient, shown instead of a browser host.
    recipient: Option<String>,
    mailto: bool,
    idn: bool,
    non_ascii: bool,
    backslash: bool,
    /// Request focus on "Annulla" on the first frame, then let the keyboard move freely.
    focus_cancel: bool,
}

/// The block view of the open message, keyed by the stored fragment and text it was built from.
struct BodyCache {
    message_id: String,
    html: Option<String>,
    /// The stored text alternative the view was built from. An oversized body with empty stored
    /// text falls back to the parser's prefix, so the view text cannot be compared with
    /// `body_text`; the key must hold the input instead.
    text: String,
    view: Arc<body::View>,
}

/// The PGP state of the open message. `view` is the decrypted body, built once from the sanitized
/// text/html the engine returned; it is display-only and never written back to the store.
struct PgpReadingState {
    message_id: String,
    kind: PgpMimeKind,
    verify: Option<PgpVerifyResult>,
    decrypt: Option<PgpDecryptResult>,
    view: Option<Arc<body::View>>,
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

/// An attachment whose bytes are already read from the blob store and whose destination is being
/// confirmed. The bytes are kept only for the lifetime of the dialog.
#[derive(Clone, Debug)]
enum AttachmentStripAction {
    Preview(Attachment),
    Save(Attachment),
}

/// A display-only preview. The decoded text or texture is bounded and dropped when the sheet closes;
/// nothing is written back to the store and nothing is added to the search index.
struct AttachmentPreviewState {
    attachment: Attachment,
    text: Option<String>,
    texture: Option<egui::TextureHandle>,
    error: Option<String>,
    truncated: bool,
}

struct PendingAttachment {
    filename: String,
    bytes: Vec<u8>,
    /// `Some` once a path has been chosen, including while a post-dialog warning is on screen.
    path: Option<PathBuf>,
    /// The warning already accepted for this save, if any.
    dangerous: Option<DangerousKind>,
    /// Request focus on "Annulla" on the first frame of a sheet.
    focus_cancel: bool,
    /// The person already confirmed replacing this exact path.
    overwrite_confirmed: bool,
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

/// One action chosen from a conversation row (click or context menu).
enum ThreadRowAction {
    Open(String),
    Archive(String),
    Delete(String),
    Flag(String, Option<bool>, Option<bool>),
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

fn window_control(ui: &mut Ui, glyph: &str, label: &str, close: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(46.0, ui_size::TITLE_BAR), Sense::click());
    if response.hovered() {
        let fill = if close { theme::DANGER } else { theme::FOREST_SOFT };
        ui.painter().rect_filled(rect, CornerRadius::same(0), fill);
    }
    let galley = ui.painter().layout_no_wrap(glyph.to_owned(), icons::font(12.0), theme::IVORY);
    let pos = egui::pos2(rect.center().x - galley.size().x * 0.5, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(pos, galley, Color32::PLACEHOLDER);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, label));
    response.on_hover_text(label)
}

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

fn reading_context_menu(response: egui::Response, subject: String, sender: String) {
    response.context_menu(|ui| {
        let items = [
            MenuItem::new("Copia oggetto").icon(icons::COPY),
            MenuItem::new("Copia mittente").icon(icons::COPY),
            MenuItem::separator(),
            MenuItem::new("Apri in finestra").icon(icons::EXTERNAL).enabled(false).reason("La finestra separata non è ancora disponibile"),
            MenuItem::new("Mostra originale").icon(icons::EYE).enabled(false).reason("Il sorgente MIME non è ancora disponibile"),
        ];
        match Menu::new(&items).show(ui) {
            Some(0) => {
                ui.ctx().copy_text(subject);
                ui.close();
            }
            Some(1) => {
                ui.ctx().copy_text(sender);
                ui.close();
            }
            _ => {}
        }
    });
}

fn row_context_menu(message: &MessageSummary, is_draft: bool) -> (Vec<MenuItem<'static>>, Vec<Option<RowAction>>) {
    let mut items = Vec::new();
    let mut actions = Vec::new();
    items.push(menu_item("open"));
    actions.push(Some(RowAction::Open));
    if is_draft {
        items.push(menu_item("edit_draft"));
        actions.push(Some(RowAction::EditDraft));
    } else {
        items.push(menu_item("reply"));
        actions.push(Some(RowAction::Reply));
        items.push(menu_item("reply_all"));
        actions.push(Some(RowAction::ReplyAll));
        items.push(menu_item("forward"));
        actions.push(Some(RowAction::Forward));
    }
    items.push(MenuItem::separator());
    actions.push(None);
    if message.unread {
        items.push(menu_item("read"));
        actions.push(Some(RowAction::MarkRead));
    } else {
        items.push(menu_item("unread"));
        actions.push(Some(RowAction::MarkUnread));
    }
    if message.starred {
        items.push(menu_item("unstar").icon(icons::STAR_FILL));
        actions.push(Some(RowAction::Unstar));
    } else {
        items.push(menu_item("star"));
        actions.push(Some(RowAction::Star));
    }
    items.push(menu_item("label_menu"));
    actions.push(Some(RowAction::Label));
    items.push(MenuItem::separator());
    actions.push(None);
    items.push(menu_item("archive"));
    actions.push(Some(RowAction::Archive));
    items.push(menu_item("delete").danger(true));
    actions.push(Some(RowAction::Delete));
    (items, actions)
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

/// Up to eight leaf clauses of a parsed search query, in source order, for the chips under the
/// search field. A click replaces the query with that clause. An unparsable query has no chips; the
/// inline error explains why.
fn search_chips(query: &str) -> Vec<String> {
    let Ok(parsed) = postino_core::parse_search(query) else { return Vec::new() };
    fn collect(expr: &postino_core::SearchExpr, out: &mut Vec<String>) {
        match expr {
            postino_core::SearchExpr::And(children) | postino_core::SearchExpr::Or(children) => {
                for child in children {
                    if out.len() < 8 {
                        collect(child, out);
                    }
                }
            }
            postino_core::SearchExpr::Not(child) => {
                if out.len() < 8 {
                    out.push(format!("NOT ({})", child.canonical()));
                }
            }
            leaf => {
                if out.len() < 8 {
                    out.push(leaf.canonical());
                }
            }
        }
    }
    let mut chips = Vec::new();
    collect(&parsed.expr, &mut chips);
    chips
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
/// The star command a conversation row exposes. A starred thread removes the star; an unstarred
/// one adds it, so the label and the action can never disagree.
fn thread_star_menu(starred: bool) -> (&'static str, bool) {
    if starred { ("unstar", false) } else { ("star", true) }
}

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

fn search_meta(total: usize, capped: bool, scope: Option<&str>) -> String {
    let total_text = if capped { "primi 10.000 risultati".to_owned() } else { messages_text(total) };
    match scope {
        Some(scope) => format!("{total_text} · {scope}"),
        None => total_text,
    }
}

/// The Italian sentence for a search parse error. The code is stable English; the position is the
/// byte offset in the query, so the field can point at the exact token.
fn search_error_text(code: &str, position: usize) -> String {
    let reason = match code {
        "empty" => "la ricerca è vuota",
        "too_long" => "la ricerca supera i 1024 byte",
        "too_many_clauses" => "la ricerca ha più di 64 clausole",
        "too_deep" => "la ricerca ha più di 8 parentesi annidate",
        "too_many_terms" => "la ricerca ha più di 16 termini",
        "control" => "la ricerca contiene un carattere di controllo",
        "unclosed_quote" => "una virgoletta non è chiusa",
        "unclosed_paren" => "una parentesi non è chiusa",
        "unexpected_token" => "c'è un token inatteso",
        "dangling_operator" => "AND o OR non hanno un operando",
        "dangling_field" => "un campo non ha un valore",
        "negative_only" => "NOT richiede almeno un termine positivo",
        "bad_date" => "la data deve essere un giorno YYYY-MM-DD valido",
        "bad_size" => "la dimensione deve essere un numero con k, m o g",
        "bad_is" => "is: accetta unread, read, starred o unstarred",
        "bad_has" => "has: accetta attachment",
        _ => "la ricerca non è valida",
    };
    format!("Ricerca non valida: {reason} (posizione {position}). Correggi il testo o premi Esc per cancellarla.")
}

/// The remote-content line, or `None` when nothing was blocked. The count is the parser's stored
/// fact carried by sync; the sanitized HTML deliberately contains no reference the pane could rescan.
fn remote_images_text(count: u32) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 immagine remota bloccata · il contenuto arriva dal server e non viene caricato".to_owned()),
        count => Some(format!("{count} immagini remote bloccate · il contenuto arriva dal server e non viene caricato")),
    }
}

fn invite_when(invite: &CalendarInviteView) -> Option<String> {
    let pretty = |raw: &str| {
        let raw = raw.trim();
        let (body, utc) = match raw.strip_suffix('Z') {
            Some(body) => (body, true),
            None => (raw, false),
        };
        let body = body.replacen('T', " ", 1);
        if utc { format!("{body} UTC") } else { body }
    };
    match (invite.start.as_deref(), invite.end.as_deref()) {
        (Some(start), Some(end)) => Some(format!("Quando: {} – {}", pretty(start), pretty(end))),
        (Some(start), None) => Some(format!("Quando: {}", pretty(start))),
        _ => None,
    }
}

/// The reading-pane invite card. It only reports a click; the caller owns the draft. Buttons are
/// disabled with a visible reason when the event cannot be answered, and never rendered for a
/// cancelled or invalid event.
fn reading_invite_card(ui: &mut Ui, invite: &CalendarInviteView, t: &Tokens) -> Option<CommandId> {
    let headline = match invite.method.as_str() {
        "request" => "INVITO",
        "cancel" => "INVITO ANNULLATO",
        "reply" => "RISPOSTA A UN INVITO",
        _ => "EVENTO",
    };
    let unavailable = if invite.cancelled {
        Some("L'organizzatore ha annullato l'evento.")
    } else if !invite.valid {
        Some(invite.invalid_reason.as_deref().unwrap_or("L'evento non è valido."))
    } else if invite.organizer.is_none() {
        Some("Manca l'organizzatore: non c'è a chi rispondere.")
    } else if !invite.can_reply {
        Some("Questo messaggio non è una richiesta di partecipazione ricevuta: non si risponde.")
    } else {
        None
    };
    let mut command = None;
    let card = Frame::new()
        .fill(t.panel_alt)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(radius::CONTROL)
        .inner_margin(Margin::symmetric(inset::LG, inset::SM))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(icons::icon(icons::CALENDAR, 16.0, t.gold_ink));
                ui.add_space(space::SM);
                ui.label(typography::text(Role::Caption, headline, t.text_faint));
            });
            ui.add_space(space::XXS);
            ui.label(typography::strong(Role::BodySmall, invite.summary.as_str(), t.text));
            ui.add_space(space::XS);
            if let Some(organizer) = invite.organizer.as_deref() {
                ui.label(typography::text(Role::Meta, format!("Organizzatore: {organizer}"), t.text_muted));
            }
            let when = invite_when(invite);
            let has_when = when.is_some();
            if has_when || invite.location.is_some() {
                ui.horizontal_wrapped(|ui| {
                    if let Some(when) = when.as_deref() {
                        ui.label(typography::mono(when, 11.0, t.text_muted));
                    }
                    if let Some(location) = invite.location.as_deref() {
                        if has_when {
                            ui.label(typography::text(Role::Meta, "·", t.text_faint));
                        }
                        ui.label(typography::text(Role::Meta, format!("Luogo: {location}"), t.text_muted));
                    }
                });
            }
            for warning in &invite.warnings {
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icons::WARNING, 13.0, t.gold_ink));
                    ui.add_space(space::XS);
                    ui.label(typography::text(Role::Caption, warning.as_str(), t.gold_ink));
                });
            }
            if let Some(reason) = unavailable {
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icons::WARNING, 13.0, t.text_faint));
                    ui.add_space(space::XS);
                    ui.label(typography::text(Role::Caption, reason, t.text_faint));
                });
            }
            if invite.valid && !invite.cancelled {
                ui.add_space(space::XS);
                let enabled = invite.can_reply;
                ui.horizontal_wrapped(|ui| {
                    for id in [CommandId::InviteAccept, CommandId::InviteTentative, CommandId::InviteDecline] {
                        let label = command_label(id.as_str(), "Accetta");
                        let button = match id {
                            CommandId::InviteAccept => Button::new(label),
                            CommandId::InviteTentative => Button::ghost(label),
                            // The three buttons above are the whole set; the third reply is the
                            // refusal and is the only one that stays destructive.
                            _ => Button::danger(label),
                        };
                        let response = button.enabled(enabled).show(ui);
                        #[cfg(test)]
                        probe::invite_action(ui, id.as_str(), &response, enabled);
                        if response.clicked() {
                            command = Some(id);
                        }
                    }
                });
            }
            ui.add_space(space::XS);
            ui.horizontal(|ui| {
                ui.label(icons::icon(icons::INFO, 13.0, t.text_faint));
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "Nessuna risposta automatica: bozza da rivedere, calendario invariato.", t.text_faint));
            });
        });
    #[cfg(test)]
    probe::invite_card(ui, card.response.rect);
    #[cfg(not(test))]
    let _ = card.response.rect;
    command
}

fn signature_error_message(raw: &str) -> String {
    if raw.contains("at most 4096") || raw.contains("4096") {
        return "La firma supera 4 KiB: accorciala prima di salvare.".to_owned();
    }
    if raw.contains("control") {
        return "La firma contiene un carattere di controllo: sono ammessi solo nuova riga e tabulazione.".to_owned();
    }
    if raw.contains("unknown account") {
        return "L'account non esiste più: riapri le impostazioni.".to_owned();
    }
    "Non è stato possibile salvare la firma.".to_owned()
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

/// The columns of the message table at a given *list panel* width (before the 16 pt side margins).
/// The compact date is dropped only below 480 pt; the sender narrows below 560 pt so the subject
/// keeps its 260 pt floor at the 499 pt list the 1280×720 reading split leaves.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ListColumns {
    sender: f32,
    date: Option<f32>,
}

fn list_columns(list_width: f32) -> ListColumns {
    let sender = if list_width < ui_size::LIST_DATE_MIN + 80.0 { ui_size::LIST_SENDER_NARROW } else { ui_size::LIST_SENDER };
    if list_width < ui_size::LIST_DATE_MIN { ListColumns { sender, date: None } } else { ListColumns { sender, date: Some(ui_size::LIST_DATE) } }
}

/// The width the subject cell has at a given list panel width, with the fixed columns (unread
/// dot, sender, date, star) and the list margins removed. The table's column spacing is set to
/// zero, so it does not enter the sum. Kept next to [`list_columns`] so the 260 pt promise can be
/// tested without a window.
#[cfg(test)]
fn subject_width(list_width: f32) -> f32 {
    let columns = list_columns(list_width);
    // The dot, the sender, the 52 pt date and the 28 pt star, plus the table gutter and the 4 pt
    // inset in front of the subject text.
    let fixed = 12.0 + columns.sender + columns.date.unwrap_or(0.0) + 28.0 + TABLE_GUTTER + 4.0;
    (list_width - 2.0 * space::XL - fixed).max(0.0)
}

/// The table's own vertical scroll gutter, measured on the real layout. It is taken out of the
/// subject so the 260 pt promise is about the text that is actually drawn, not the column label.
#[cfg(test)]
const TABLE_GUTTER: f32 = 8.0;

/// Would `text` have to be truncated to fit `width` at this font? Used to attach a tooltip only
/// when something is actually cut off.
fn text_fits(ui: &Ui, text: &str, font: &FontId, width: f32) -> bool {
    ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x <= width
}

/// egui counts a third quick click as a triple, even when the previous double-click was on another
/// widget. A row double-click followed by a header double-click must still restore, so both the
/// double and the triple activation mean "the same gesture happened again".
fn double_activated(response: &egui::Response) -> bool {
    response.double_clicked() || response.triple_clicked()
}

fn show_list_avatars(list_width: f32) -> bool {
    list_width >= AVATAR_MIN_LIST_WIDTH
}

/// Below the breakpoint the reading pane is a full-width overlay, not a resizable side pane.
fn reading_overlay(available: f32) -> bool {
    available < READING_PANE_MIN_WIDTH
}

/// The height reserved for the pinned attachment strip. Four rows scroll inside the 200 px cap.
fn reading_attachment_strip_height(rows: usize) -> f32 {
    (24.0 + rows as f32 * 52.0 + 2.0 * f32::from(inset::SM)).min(200.0)
}

/// Fade the last pixels of a scrollable reading body into the panel. A scroll area clips at an
/// arbitrary pixel; without this a line can be sliced horizontally at the attachment strip.
fn bottom_fade(ui: &Ui, rect: Rect, panel: Color32) {
    let height = 14.0_f32.min(rect.height() * 0.4);
    if height <= 0.0 {
        return;
    }
    let top = rect.bottom() - height;
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(pos2(rect.left(), top), Color32::TRANSPARENT);
    mesh.colored_vertex(pos2(rect.right(), top), Color32::TRANSPARENT);
    mesh.colored_vertex(pos2(rect.left(), rect.bottom()), panel);
    mesh.colored_vertex(pos2(rect.right(), rect.bottom()), panel);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
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

/// What a screen reader hears for one message row. It repeats the visual facts — sender, subject,
/// unread, attachment, date, selection — and names the Enter affordance, so the row is usable
/// without seeing which column the pointer is over.
fn row_accessible_label(message: &MessageSummary, date: &str, selected: bool) -> String {
    let sender = if message.from.name.trim().is_empty() { message.from.email.as_str() } else { message.from.name.as_str() };
    let subject = if message.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.subject.as_str() };
    let mut label = format!("{sender}. {subject}.");
    if message.unread {
        label.push_str(" Non letto.");
    }
    if message.has_attachments {
        label.push_str(" Con allegati.");
    }
    label.push(' ');
    label.push_str(date);
    label.push('.');
    if selected {
        label.push_str(" Selezionato.");
    }
    label.push_str(" Premi Invio per aprire.");
    label
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

const MAX_PREVIEW_TEXT: usize = 64 * 1024;
const MAX_PREVIEW_IMAGE_DIMENSION: u32 = 2048;
const MAX_PREVIEW_IMAGE_ALLOC: u64 = 32 * 1024 * 1024;

/// Text previews are bounded before decoding and control characters are replaced so a hostile
/// attachment cannot reorder the interface with bidi controls.
fn preview_text(bytes: &[u8]) -> (String, bool) {
    let truncated = bytes.len() > MAX_PREVIEW_TEXT;
    let slice = &bytes[..bytes.len().min(MAX_PREVIEW_TEXT)];
    let decoded = String::from_utf8_lossy(slice);
    let text = decoded
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' {
                character
            } else if character.is_control() || matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') {
                '\u{FFFD}'
            } else {
                character
            }
        })
        .collect();
    (text, truncated)
}

/// Decode an image attachment with strict dimensions and allocation bounds. The full bytes remain
/// available through Save; the preview is only a scaled texture.
fn preview_image(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "Il formato dell'immagine non è riconoscibile.".to_owned())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PREVIEW_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_PREVIEW_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_PREVIEW_IMAGE_ALLOC);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| "L'immagine è troppo grande o non è decodificabile.".to_owned())?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba.as_raw()))
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

/// The warning class of the file the person actually chose, using the same sanitizer and
/// classifier as the suggested name.
fn attachment_destination_kind(path: &Path) -> Option<DangerousKind> {
    let name = path.file_name()?.to_string_lossy();
    // The sanitized name preserves a truncated extension; classifying the raw name too catches a
    // path the dialog returned with a long stem that the suggestion would shorten.
    reading::dangerous_kind(&reading::safe_filename(&name)).or_else(|| reading::dangerous_kind(&name))
}

/// Encode the viewport surface exactly as eframe captured it. `ColorImage` is sRGB with the same
/// channel order as PNG's RGBA, so no conversion is involved.
///
/// The bytes go to `<out>.tmp` first and are renamed over the destination, so a failed or killed
/// capture never leaves a truncated PNG behind. The temp file is removed on error.
fn write_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    let mut temp_os = path.as_os_str().to_os_string();
    temp_os.push(".tmp");
    let temp = PathBuf::from(temp_os);
    if let Err(error) = encode_png(&temp, image) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("{}: {error}", path.display())
    })
}

fn encode_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    let width = u32::try_from(image.size[0]).map_err(|_| "the screenshot is wider than a PNG allows".to_owned())?;
    let height = u32::try_from(image.size[1]).map_err(|_| "the screenshot is taller than a PNG allows".to_owned())?;
    let pixels = image.pixels.iter().flat_map(|color| color.to_array()).collect::<Vec<u8>>();
    let file = std::fs::File::create(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| format!("{}: {error}", path.display()))?;
    writer.write_image_data(&pixels).map_err(|error| format!("{}: {error}", path.display()))?;
    writer.finish().map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
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
mod tests;
