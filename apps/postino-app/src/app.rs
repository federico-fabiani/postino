//! The egui shell. Panels are deliberately plain: folder rail, message table, reading pane.
//! Every action goes through `Engine::call`, the same operation registry the CLI and MCP expose.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, Utc};
use eframe::egui::containers::Sides;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Frame, Id, Key, Layout, Margin, Modifiers, Rect, ResizeDirection,
    RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder, UserData, Vec2, ViewportCommand, WidgetInfo, WidgetType, pos2,
};
use postino_core::{
    Account, Address, Attachment, DraftSave, Engine, FolderKind, LabelApply, LabelColor, LabelCreate, LabelDelete, LabelSummary, LabelUpdate,
    Message, MessageSummary, MessagesList, OutboxList, OutboxRunReport, OutboxView, SavedSearch, SearchResult, SendQueued, SyncPhase,
    ThreadGetResult, ThreadSummary, ThreadsListResult,
};
use postino_net::CertificateDer;
use serde_json::json;

use crate::boot::{self, BootFailure, Launch, StoreHandle};
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
#[cfg(test)]
use crate::ui::widgets::ComposerFocus;
use crate::ui::widgets::{
    AUTOSAVE_DELAY, Badge, BadgeKind, Button, ButtonSize, Chip, ComposeMode, Composer, ComposerState, DEMO_SEND_REASON, Dialog, EmptyState,
    IconButton, IconButtonSize, ListRow, Menu, MenuItem, NO_SMTP_SEND_REASON, SearchField, SectionHeader, TextField, Toast, ToastKind, ToastResult,
};

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
                 onboarding_folders, onboarding_sync, mailbox, offline, reading_attachments, link_confirm, settings, settings_signature, outbox, \
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
    /// A dangerous attachment waiting for the explicit warning.
    attachment_warning: Option<PendingAttachment>,
    /// An attachment whose chosen path already exists and waits for overwrite confirmation.
    attachment_overwrite: Option<PendingAttachment>,
    /// Stable Save-button ids so a headless test can drive the keyboard path.
    attachment_save_ids: BTreeMap<String, Id>,
    /// A message override used only by deterministic screenshots (HTML plus attachment fixtures).
    screenshot_message: Option<Message>,
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
}

impl PostinoApp {
    /// Normal desktop launch. The command line chose demo or a store; a failure is a recovery
    /// screen, never a panic and never an empty mailbox.
    pub fn launch(cc: &eframe::CreationContext<'_>, custom_chrome: bool, launch: Launch) -> Self {
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
        app
    }

    fn from_booted(booted: boot::Booted, custom_chrome: bool, now: DateTime<Utc>, launch: Launch, extra_roots: Vec<CertificateDer<'static>>) -> Self {
        let boot::Booted { engine, store, demo, writer_lock } = booted;
        let mut app = Self::with_engine(engine, store, demo, None, launch, custom_chrome, now, extra_roots);
        app.writer_lock = writer_lock;
        app
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
            attachment_warning: None,
            attachment_overwrite: None,
            attachment_save_ids: BTreeMap::new(),
            screenshot_message: None,
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
    fn seed_screenshot(&mut self, state: ScreenshotState) {
        self.screenshot_message = None;
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
                    let _ = self.engine.call("message_flag", json!({ "id": id, "unread": false, "starred": null }));
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
                    let _ = self.engine.call("message_move", json!({ "id": id, "folder": "archive" }));
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
                    let _ = self.engine.call("message_flag", json!({ "id": id, "unread": false, "starred": null }));
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
                        // sync stored the count. This HTML is already the sanitized form and contains
                        // no remote `src`, so a screenshot can never load anything.
                        message.body_html = Some(
                            "<p>Ciao Federico,</p><p>ho caricato il PDF del preventivo nella cartella condivisa.</p>".to_owned(),
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
            ScreenshotState::Settings => {
                // A settings capture must not inherit a selected/read message: the unread badge and
                // the inbox total have to be the same at every window size.
                self.clear_selection();
                self.close_reading();
                self.demo = false;
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
                        let _ = self.engine.call("saved_search_create", json!({ "name": name, "query": query }));
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
                            let _ = self.engine.call(
                                "rules_create",
                                json!({ "name": name, "query": query, "actions": actions, "priority": priority, "account": account }),
                            );
                        }
                        let _ = self.engine.call("rules_run", json!({ "account": account, "scope": "new" }));
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
                        continue;
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
                        continue;
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
        match self.engine.call("account_remove", json!({ "id": state.id, "confirm": true })) {
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
        self.sync_worker = Some(SyncWorker::start(Box::new(crate::sync_worker::StoreSyncBackend::new(store.clone(), self.extra_roots.clone()))));
    }

    fn open_demo(&mut self) {
        self.stop_sync_worker();
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
                Ok(engine) => Some(boot::Booted { engine, store: Some(store), demo: false, writer_lock: Some(writer_lock) }),
                Err(_) => None,
            },
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
                std::thread::spawn(move || {
                    let result = match handle.open_engine() {
                        Ok(mut engine) => engine.add_account(*setup).map(|view| (view.account.id, view.account.email)).map_err(add_account_copy),
                        Err(error) => Err(format!("Non riesco ad aprire l'archivio locale: {error}")),
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
            Some(id) => {
                self.engine.call("saved_search_update", json!({ "id": id, "account": update_account, "name": name, "query": query, "sort": sort }))
            }
            None => self.engine.call("saved_search_create", json!({ "account": account, "name": name, "query": query, "sort": sort })),
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
        match self.engine.call("saved_search_delete", json!({ "id": id, "account": saved.account_id })) {
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
            let _ = self.engine.record_recent_search(account.as_deref(), &saved.query);
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
        let summary = match self.engine.call("thread_get", json!({ "id": id, "account": account, "limit": 1, "offset": 0 })) {
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
        let page = match self.engine.call("thread_get", json!({ "id": summary.resolved_id, "account": account, "limit": 200, "offset": start })) {
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
        match self.engine.call("thread_get", json!({ "id": id, "account": account, "limit": 200, "offset": offset })) {
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
        if let Ok(value) = self.engine.call("thread_get", json!({ "id": id, "account": account, "limit": 200, "offset": offset }))
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
        match self.engine.call("thread_flag", json!({ "id": id, "unread": unread, "starred": starred })) {
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
        match self.engine.call("label_delete", json!({ "id": state.id })) {
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
        match self.engine.call("label_apply", json!({ "id": id, "message_ids": ids, "apply": apply })) {
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
            match self.engine.call("message_move", json!({ "id": id, "folder": folder })) {
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
        match self.engine.call("message_flag", json!({ "id": id, "unread": unread, "starred": starred })) {
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
                    if self.engine.call("message_flag", json!({ "id": id, "unread": unread, "starred": null })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::Starred(changes) => {
                for (id, starred) in changes {
                    if self.engine.call("message_flag", json!({ "id": id, "unread": null, "starred": starred })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::Move(changes) => {
                for (id, folder) in changes {
                    if self.engine.call("message_move", json!({ "id": id, "folder": folder })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::CancelSend(id) => {
                if self.engine.call("outbox_cancel", json!({ "id": id })).is_err() {
                    failed = true;
                } else {
                    self.refresh_outbox();
                }
            }
            UndoAction::Labels(changes) => {
                for (message_id, label_id, was_applied) in changes {
                    if self.engine.call("label_apply", json!({ "id": label_id, "message_ids": [message_id], "apply": was_applied })).is_err() {
                        failed = true;
                        break;
                    }
                }
            }
            UndoAction::LabelDelete { name, color, message_ids } => match self.engine.call("label_create", json!({ "name": name, "color": color })) {
                Ok(value) => match serde_json::from_value::<LabelCreate>(value) {
                    Ok(created) => {
                        let ids = message_ids.iter().filter(|id| self.engine.message(id).is_some()).cloned().collect::<Vec<_>>();
                        // A deleted label can carry more assignments than one `label_apply` call
                        // accepts (500), so restore in chunks instead of failing halfway.
                        for chunk in ids.chunks(LABEL_APPLY_CHUNK) {
                            if self.engine.call("label_apply", json!({ "id": created.label.id, "message_ids": chunk, "apply": true })).is_err() {
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
                if self.engine.call("label_update", json!({ "id": id, "name": name, "color": color })).is_err() {
                    failed = true;
                }
            }
            UndoAction::SavedSearchDelete { name, query, sort, account } => {
                if self.engine.call("saved_search_create", json!({ "account": account, "name": name, "query": query, "sort": sort })).is_err() {
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
        let result = match self.engine.call(name, args) {
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
            match self.engine.call("threads_list", page_args) {
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
                let _ = self.engine.record_recent_search(self.active_account.as_deref(), &self.search_committed);
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
        let summary = match self.engine.call("thread_get", json!({ "id": thread_id, "account": account, "limit": 1, "offset": 0 })) {
            Ok(value) => match serde_json::from_value::<ThreadGetResult>(value) {
                Ok(detail) => detail,
                Err(_) => return,
            },
            Err(_) => return,
        };
        let start = summary.total.saturating_sub(200);
        let page = match self.engine.call("thread_get", json!({ "id": summary.resolved_id, "account": account, "limit": 200, "offset": start })) {
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

    fn start_composer(&mut self, mut state: ComposerState) {
        if self.composer.open && self.composer.dirty && !self.save_composer(false) {
            return;
        }
        state.open = true;
        // Over the modal, the first empty field owns the keyboard. A reply with every field filled
        // falls back to the body.
        state.focus = Some(state.first_empty_focus());
        self.composer = state;
        self.composer_focus_pending = true;
        self.show_palette = false;
        self.show_shortcuts = false;
        self.show_about = false;
        self.label_menu = None;
    }

    fn compose_new(&mut self) {
        let Some(account) = self.account().cloned() else {
            self.status = "Nessun account configurato".to_owned();
            return;
        };
        let mut state = ComposerState::new(ComposeMode::New);
        state.body = postino_core::new_message_body(&account.signature);
        self.start_composer(state);
    }

    fn reply_to(&mut self, id: &str, mode: ComposeMode) {
        let Some(message) = self.engine.message(id) else {
            self.status = "Messaggio non trovato".to_owned();
            return;
        };
        let Some(account) = self.account().cloned() else {
            self.status = "Nessun account configurato".to_owned();
            return;
        };
        let target = self.engine.reply_target(id).unwrap_or(postino_core::ReplyTarget { parent_message_id: None, reply_to: Vec::new() });
        let reply_mode = match mode {
            ComposeMode::Forward => postino_core::ReplyMode::Forward,
            ComposeMode::ReplyAll => postino_core::ReplyMode::ReplyAll,
            _ => postino_core::ReplyMode::Reply,
        };
        let prefill = postino_core::compose_reply(&account, &message, target.parent_message_id.as_deref(), &target.reply_to, reply_mode);
        let mut state = ComposerState::new(mode);
        state.to = Address::format_list(&prefill.to);
        state.cc = Address::format_list(&prefill.cc);
        state.show_cc = !prefill.cc.is_empty();
        state.show_bcc = false;
        state.subject = prefill.subject;
        state.body = prefill.body;
        state.in_reply_to = prefill.in_reply_to;
        state.forward_sources =
            if mode == ComposeMode::Forward { prefill.attachments.iter().map(|attachment| attachment.id.clone()).collect() } else { Vec::new() };
        state.attachments = prefill.attachments;
        // A prefill is content: Send, Salva bozza and Esc must create the draft even if the person
        // has not typed anything yet. The old guard treated it as pristine and silently discarded it.
        state.dirty = true;
        state.last_edit = self.now;
        self.start_composer(state);
    }

    fn edit_draft(&mut self, id: &str) {
        let Some(message) = self.engine.message(id) else {
            self.status = "Bozza non trovata".to_owned();
            return;
        };
        let is_draft = self
            .engine
            .folders()
            .iter()
            .find(|folder| folder.id == message.summary.folder_id)
            .is_some_and(|folder| folder.kind == FolderKind::Drafts);
        if !is_draft {
            self.status = "Questo messaggio non è una bozza".to_owned();
            return;
        }
        let mut state = ComposerState::new(ComposeMode::Draft);
        state.draft_id = Some(message.summary.id.clone());
        state.to = Address::format_list(&message.summary.to);
        state.cc = Address::format_list(&message.cc);
        state.bcc = Address::format_list(&message.bcc);
        state.show_cc = !message.cc.is_empty();
        state.show_bcc = !message.bcc.is_empty();
        state.subject = message.summary.subject.clone();
        state.body = message.body_text.clone();
        state.attachments = message.attachments.clone();
        state.in_reply_to = self.engine.draft_reply_link(&message.summary.id).map(str::to_owned);
        state.saved_at = Some(format_time(message.summary.date));
        self.start_composer(state);
    }

    fn save_composer(&mut self, notify: bool) -> bool {
        if !self.composer.open {
            return false;
        }
        if !self.composer.dirty && self.composer.draft_id.is_none() {
            let has_content = !self.composer.to.trim().is_empty()
                || !self.composer.cc.trim().is_empty()
                || !self.composer.bcc.trim().is_empty()
                || !self.composer.subject.trim().is_empty()
                || !self.composer.body.trim().is_empty()
                || !self.composer.attachments.is_empty()
                || self.composer.in_reply_to.is_some();
            if !has_content {
                if notify {
                    self.status = "Nessuna modifica da salvare".to_owned();
                }
                return true;
            }
        }
        if !self.composer.dirty && self.composer.draft_id.is_some() {
            if notify {
                self.status = "Bozza già salvata".to_owned();
            }
            return true;
        }
        let (to, cc, bcc) = match self.composer.recipients() {
            Ok(recipients) => recipients,
            Err(error) => {
                self.composer.mark_failed(error, self.now);
                if notify {
                    self.status = "Controlla gli indirizzi della bozza".to_owned();
                }
                return false;
            }
        };
        let pending_forward = if self.composer.draft_id.is_none() && !self.composer.forward_sources.is_empty() {
            self.composer.forward_sources.clone()
        } else {
            Vec::new()
        };
        let attachments_for_save = if pending_forward.is_empty() { self.composer.attachments.clone() } else { Vec::new() };
        let mut args = json!({
            "to": to,
            "cc": cc,
            "bcc": bcc,
            "subject": self.composer.subject,
            "body_text": self.composer.body,
            "attachments": attachments_for_save,
        });
        if let Some(link) = &self.composer.in_reply_to
            && let Some(object) = args.as_object_mut()
        {
            object.insert("in_reply_to".to_owned(), json!(link));
        }
        if let Some(id) = &self.composer.draft_id
            && let Some(object) = args.as_object_mut()
        {
            object.insert("id".to_owned(), json!(id));
        } else if let Some(account) = &self.active_account
            && let Some(object) = args.as_object_mut()
        {
            object.insert("account".to_owned(), json!(account));
        }
        let result = self.engine.call("draft_save", args);
        match result {
            Ok(value) => match serde_json::from_value::<DraftSave>(value) {
                Ok(saved) => {
                    let id = saved.draft.summary.id.clone();
                    self.composer.draft_id = Some(id.clone());
                    let forward_metadata = self.composer.attachments.clone();
                    self.composer.attachments = saved.draft.attachments;
                    let copy = if pending_forward.is_empty() {
                        ForwardCopy::default()
                    } else {
                        self.copy_forward_attachments(&id, &pending_forward, &forward_metadata)
                    };
                    if let Some(notice) = copy.notice {
                        // The body is saved, but the attachment set is incomplete. Keep the composer
                        // dirty so the state does not claim a complete save, and block Send until the
                        // named rows are removed.
                        self.composer.attachment_failure = Some(notice.clone());
                        self.composer.failed_attachment_ids = copy.failed_ids;
                        self.composer.dirty = true;
                        self.composer.last_edit = self.now;
                        self.composer.notice = Some(notice);
                    } else if self.composer.failed_attachment_ids.is_empty() {
                        self.composer.mark_saved(format_time(self.now_utc));
                        self.composer.attachment_failure = None;
                    } else {
                        // A previous failed forward row is still present; the body save is fine but
                        // the draft is not complete. Keep the blocking sentence and the dirty state.
                        self.composer.dirty = true;
                        self.composer.last_edit = self.now;
                    }
                    if notify {
                        self.status = if self.composer.attachment_failure.is_some() {
                            "Bozza salvata, ma un allegato non è stato copiato".to_owned()
                        } else {
                            "Bozza salvata".to_owned()
                        };
                    }
                    true
                }
                Err(_) => {
                    let message = draft_save_error_message(&self.composer.attachment_failure, "Non è stato possibile salvare la bozza.");
                    self.composer.mark_failed(message, self.now);
                    false
                }
            },
            Err(error) => {
                let message = draft_save_error_message(&self.composer.attachment_failure, &error.to_string());
                self.composer.mark_failed(message, self.now);
                if notify {
                    self.status = "La bozza non è stata salvata".to_owned();
                }
                false
            }
        }
    }

    /// Copy a forwarded message's attachment bytes into the new draft.
    ///
    /// The first save deliberately writes no metadata rows for them: a row without bytes would be
    /// visible before the copy. Every successful copy replaces the composer's list with the stored
    /// rows; a failed source becomes a metadata-only row with a fresh id, which is reported and makes
    /// the next send refuse instead of silently dropping it.
    fn copy_forward_attachments(&mut self, draft_id: &str, sources: &[String], sources_metadata: &[Attachment]) -> ForwardCopy {
        let mut copied = Vec::new();
        let mut failures = Vec::new();
        for source in sources {
            match self.engine.copy_attachment_to_draft(draft_id, source) {
                Ok(draft) => copied = draft.attachments,
                Err(error) => {
                    let metadata = sources_metadata.iter().find(|attachment| attachment.id == *source).cloned().unwrap_or(Attachment {
                        id: String::new(),
                        filename: "allegato".to_owned(),
                        mime: "application/octet-stream".to_owned(),
                        size: 0,
                    });
                    failures.push((metadata, error.to_string()));
                }
            }
        }
        let mut attachments = copied;
        let mut failed_ids = Vec::new();
        for (index, (metadata, _)) in failures.iter().enumerate() {
            let mut metadata = metadata.clone();
            // A real id makes the row removable before it is persisted. It is also valid for the
            // next draft_save, which turns the metadata-only row into a visible blocker.
            metadata.id = format!("{draft_id}_local_att_{index}");
            failed_ids.push(metadata.id.clone());
            attachments.push(metadata);
        }
        self.composer.attachments = attachments;
        self.composer.forward_sources.clear();
        if failures.is_empty() {
            return ForwardCopy::default();
        }
        let first = failures.first().map(|(attachment, error)| format!("{} ({error})", attachment.filename)).unwrap_or_default();
        let more = failures.len().saturating_sub(1);
        let suffix = if more == 0 {
            ""
        } else if more == 1 {
            " e un altro file"
        } else {
            " e altri file"
        };
        ForwardCopy { notice: Some(format!("Allegato non copiato: {first}{suffix}. Rimuovilo per poter inviare.")), failed_ids }
    }

    fn send_composer(&mut self) {
        if !self.composer.open {
            return;
        }
        // The Send button, Ctrl+Enter and the composer's own enabled rule must agree: a demo or an
        // account without an SMTP endpoint has no path to a submission, so the shortcut must not
        // attempt one and fail later.
        if !self.send_allowed_for_active_account() {
            let reason = if self.demo { DEMO_SEND_REASON } else { NO_SMTP_SEND_REASON };
            self.composer.notice = Some(reason.to_owned());
            self.status = reason.to_owned();
            return;
        }
        if !self.save_composer(true) {
            return;
        }
        if let Some(failure) = self.composer.attachment_failure.clone() {
            self.composer.mark_failed(failure, self.now);
            return;
        }
        let Some(id) = self.composer.draft_id.clone() else {
            self.composer.mark_failed("Aggiungi almeno un destinatario prima di inviare", self.now);
            return;
        };
        match self.engine.call("message_send", json!({ "draft": id })) {
            Ok(value) => match serde_json::from_value::<SendQueued>(value) {
                Ok(queued) => {
                    let row = queued.queued.outbox;
                    let until = queued.queued.undo_until;
                    self.composer = ComposerState::closed();
                    self.status = "Messaggio in coda: puoi annullarlo entro pochi secondi".to_owned();
                    self.pending_send = Some(row.send_at);
                    let seconds = until.map_or(5, |at| ((at - self.now_utc.timestamp_millis()).max(0) as f64 / 1000.0).ceil() as u64);
                    self.push_send_toast(
                        format!("Messaggio in coda. Invio tra {seconds} secondi."),
                        UndoAction::CancelSend(row.id),
                        until.map_or(self.now + 5.0, |at| self.now + ((at - self.now_utc.timestamp_millis()).max(0) as f64 / 1000.0)),
                    );
                    self.refresh_outbox();
                }
                Err(_) => self.composer.mark_failed("Non è stato possibile leggere la coda di invio.", self.now),
            },
            Err(error) => {
                let message = send_error_message(&error.to_string(), self.demo);
                self.composer.mark_failed(message, self.now);
            }
        }
    }

    /// Queue the due rows on the worker thread. The undo window has already elapsed when the caller
    /// asks; a second request while a pass runs is ignored.
    fn request_send(&mut self) {
        let Some(worker) = self.send_worker.as_mut() else {
            return;
        };
        if worker.request(self.now_utc.timestamp_millis()) {
            self.pending_send = None;
        }
    }

    fn handle_send_events(&mut self) {
        let Some(worker) = self.send_worker.as_mut() else {
            return;
        };
        let events = worker.poll();
        if events.is_empty() {
            return;
        }
        let mut failed = None;
        let mut moved = false;
        for event in events {
            match event {
                SendEvent::Finished(report) => {
                    // A submitted message is moved to Sent even when the server APPEND is still
                    // pending, so both counts mean "the local draft may have moved".
                    moved |= report.sent > 0 || report.append_pending > 0;
                    self.after_outbox_run(&report);
                }
                SendEvent::Failed(message) => failed = Some(message),
            }
        }
        if moved {
            // The worker wrote through its own connection: the draft moved to Sent and the Sent row
            // carries the Message-ID. Reload the mailbox (keeping every view state) or the UI keeps
            // showing a draft that no longer exists and a Sent folder that is missing the copy.
            self.reload_engine_after_sync(false);
            self.labels_cache = self.engine.label_summaries();
        }
        self.refresh_outbox();
        if let Some(message) = failed {
            self.push_info_toast(message, ToastKind::Error);
        }
        if let Some(next) = self.next_outbox_due() {
            self.pending_send = Some(next);
        }
    }

    fn after_outbox_run(&mut self, report: &OutboxRunReport) {
        if report.sent > 0 {
            self.status = "Messaggio inviato".to_owned();
        }
        if report.uncertain > 0 {
            self.push_info_toast("Invio incerto: controlla la cartella Inviati del provider prima di riprovare.", ToastKind::Error);
        } else if report.failed > 0 {
            let reason = report.errors.first().cloned().unwrap_or_else(|| "L'invio non è riuscito.".to_owned());
            self.push_info_toast(reason, ToastKind::Error);
        } else if !report.halted.is_empty() {
            self.push_info_toast("L'invio è fermo per un account: controlla le credenziali nella coda.", ToastKind::Error);
        } else if report.sent > 0 {
            self.push_info_toast("Messaggio inviato", ToastKind::Success);
        }
    }

    /// Refresh the cached journal. Returns whether a row reached a delivered state or vanished since
    /// the previous refresh: another process may have sent a draft or marked an uncertain row sent,
    /// and the mailbox (not only the journal) must be reloaded.
    fn refresh_outbox(&mut self) -> bool {
        if self.demo {
            return false;
        }
        let Ok(outbox) = self.engine.outbox_list(None, 500, 0) else {
            return false;
        };
        let previous = std::mem::replace(&mut self.outbox, empty_outbox());
        let delivered =
            |state: postino_store::OutboxState| matches!(state, postino_store::OutboxState::Sent | postino_store::OutboxState::AppendPending);
        let transitioned = outbox
            .entries
            .iter()
            .any(|entry| delivered(entry.state) && previous.entries.iter().find(|old| old.id == entry.id).is_none_or(|old| !delivered(old.state)));
        let vanished = previous.entries.iter().any(|old| !outbox.entries.iter().any(|entry| entry.id == old.id));
        self.outbox = outbox;
        transitioned || vanished
    }

    /// One scheduler tick: refresh the journal, reload the mailbox if another process delivered
    /// something, then cache the next wake time and ask the worker if it is due. The 2 s poll (and
    /// the cached `pending_send`) is the only scheduler; no per-frame database query.
    fn poll_outbox(&mut self) {
        if self.refresh_outbox() {
            // Keep every view state: the existing post-sync reload repairs the selection and folder.
            self.reload_engine_after_sync(false);
            self.labels_cache = self.engine.label_summaries();
            self.refresh_outbox();
        }
        self.pending_send = self.next_outbox_due();
        self.pump_outbox();
    }

    fn next_outbox_due(&self) -> Option<i64> {
        // The store answers directly: a queued or unowned append row's due time, or a leased row's
        // lease expiry. The cached list is a display surface, never the scheduler.
        self.engine.next_outbox_wake().ok().flatten()
    }

    /// Queue and process a send from a worker when the cached wake time is due. Used by launch
    /// recovery, by the undo deadline and by the 2 s poll.
    fn pump_outbox(&mut self) {
        if self.send_worker.as_ref().is_some_and(|worker| worker.is_running()) {
            return;
        }
        let now = self.now_utc.timestamp_millis();
        if self.pending_send.is_some_and(|at| at <= now) {
            self.request_send();
        }
    }

    fn push_send_toast(&mut self, message: impl Into<String>, undo: UndoAction, until: f64) {
        self.failure = None;
        self.toast = Some(ToastState { message: message.into(), undo: Some(undo), until, kind: ToastKind::Info });
    }

    fn close_composer(&mut self) {
        if !self.composer.open {
            return;
        }
        if self.composer.dirty && !self.save_composer(true) {
            return;
        }
        self.composer = ComposerState::closed();
    }

    fn discard_composer(&mut self) {
        if !self.composer.open {
            return;
        }
        if let Some(id) = self.composer.draft_id.clone()
            && let Some(message) = self.engine.message(&id)
        {
            let account = self.folder(&message.summary.folder_id).map(|folder| folder.account_id.clone());
            let previous = message.summary.folder_id.clone();
            let Some(trash) = account.as_deref().and_then(|account| self.folder_for_kind_in(account, FolderKind::Trash)) else {
                self.composer.mark_failed("Il server non ha una cartella Cestino.", self.now);
                return;
            };
            match self.engine.call("message_move", json!({ "id": id, "folder": trash })) {
                Ok(_) => {
                    self.composer = ComposerState::closed();
                    self.status = "Bozza spostata nel cestino".to_owned();
                    self.push_toast("Bozza scartata", UndoAction::Move(vec![(id, previous)]));
                }
                Err(_) => self.composer.mark_failed("Non è stato possibile spostare la bozza nel cestino.", self.now),
            }
            return;
        }
        self.composer = ComposerState::closed();
        self.status = "Bozza scartata".to_owned();
    }

    /// Open the injected picker and attach every chosen file. A cancellation is a no-op; a demo or
    /// store-less session refuses before the picker (and therefore before any filesystem read).
    fn pick_composer_files(&mut self) {
        if !self.composer.open {
            return;
        }
        if self.demo || self.store.is_none() {
            self.push_info_toast("La modalità demo non allega file: serve un account su un archivio locale cifrato.", ToastKind::Error);
            return;
        }
        match self.file_picker.choose_files() {
            Ok(paths) if paths.is_empty() => {}
            Ok(paths) => self.attach_composer_files(paths),
            Err(_) => self.push_info_toast("Non è stato possibile aprire la scelta dei file.", ToastKind::Error),
        }
    }

    /// Read, bound and attach a batch of files through the same engine path the CLI and MCP use.
    ///
    /// Every file is checked by metadata and then by the number of bytes actually read, so a file
    /// that grows between the two checks is refused before decoding. Failures are collected into one
    /// toast; the draft keeps the attachments that did succeed.
    fn attach_composer_files(&mut self, mut paths: Vec<PathBuf>) {
        if !self.composer.open || paths.is_empty() {
            return;
        }
        // A drop can be reported by more than one rectangle; a picker can return the same path
        // twice. Canonicalize for the key (or keep the literal path when it cannot be resolved) so
        // one file is attached once.
        let mut seen = BTreeSet::new();
        paths.retain(|path| seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())));
        if paths.is_empty() {
            return;
        }
        if self.demo || self.store.is_none() {
            self.push_info_toast("La modalità demo non allega file: serve un account su un archivio locale cifrato.", ToastKind::Error);
            return;
        }
        let mut was_dirty = self.composer.dirty;
        if self.composer.draft_id.is_none() {
            // A first attachment needs a draft id. Save the metadata-only draft first; the composer
            // is already open and its content is what the person sees.
            self.composer.dirty = true;
            self.composer.last_edit = self.now;
            if !self.save_composer(false) {
                self.push_info_toast("Non è stato possibile preparare la bozza per gli allegati.", ToastKind::Error);
                return;
            }
            // The initial save persisted the body; only an attachment-only change remains.
            was_dirty = false;
        }
        let Some(draft_id) = self.composer.draft_id.clone() else {
            return;
        };
        let mut failures = Vec::new();
        let mut added = 0usize;
        for path in paths {
            let filename = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            match read_attachment_file(&path) {
                Ok(bytes) => match self.engine.add_draft_attachment(&draft_id, &filename, None, &bytes) {
                    Ok(draft) => {
                        self.composer.attachments = draft.attachments;
                        added = added.saturating_add(1);
                    }
                    Err(error) => failures.push(format!("{filename}: {error}")),
                },
                Err(error) => failures.push(format!("{filename}: {error}")),
            }
        }
        if added > 0 {
            if was_dirty {
                // The attachment operation persisted the old body snapshot; the text the person just
                // typed is still unsaved. Keep the composer dirty so Send/Close/autosave save it.
                self.composer.dirty = true;
                self.composer.last_edit = self.now;
            } else {
                self.composer.mark_saved(format_time(self.now_utc));
            }
            self.status = if added == 1 { "Allegato aggiunto".to_owned() } else { format!("{added} allegati aggiunti") };
        }
        if !failures.is_empty() {
            let first = failures.first().cloned().unwrap_or_default();
            let more = failures.len().saturating_sub(1);
            let suffix = if more == 0 { String::new() } else { format!(" (e altri {more})") };
            self.push_info_toast(format!("File non allegato: {first}{suffix}"), ToastKind::Error);
        }
    }

    /// Remove one attachment row. A demo or unsaved row is removed from the composer list only;
    /// with a store the operation deletes the row and the composer adopts the returned draft.
    fn remove_composer_attachment(&mut self, attachment: &str) {
        let pending_forward = self.composer.forward_sources.iter().any(|source| source == attachment);
        if pending_forward {
            self.composer.attachments.retain(|candidate| candidate.id != attachment);
            self.composer.forward_sources.retain(|source| source != attachment);
            self.composer.touch(self.now);
            return;
        }
        let failed = self.composer.failed_attachment_ids.iter().any(|id| id == attachment) || attachment.is_empty();
        let local_only = failed || self.demo || self.store.is_none() || self.composer.draft_id.is_none();
        if local_only {
            self.composer.attachments.retain(|candidate| candidate.id != attachment);
            self.composer.failed_attachment_ids.retain(|id| id != attachment);
            if self.composer.failed_attachment_ids.is_empty() {
                self.composer.attachment_failure = None;
            }
            self.composer.touch(self.now);
            self.status = "Allegato rimosso".to_owned();
            return;
        }
        let was_dirty = self.composer.dirty;
        let Some(draft_id) = self.composer.draft_id.clone() else {
            return;
        };
        match self.engine.remove_draft_attachment(&draft_id, attachment) {
            Ok(draft) => {
                self.composer.attachments = draft.attachments;
                if self.composer.failed_attachment_ids.is_empty() {
                    self.composer.attachment_failure = None;
                }
                if was_dirty {
                    self.composer.dirty = true;
                    self.composer.last_edit = self.now;
                } else {
                    self.composer.mark_saved(format_time(self.now_utc));
                }
                self.status = "Allegato rimosso".to_owned();
            }
            Err(_) => self.push_info_toast("Non è stato possibile rimuovere l'allegato.", ToastKind::Error),
        }
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
    fn reading_attachment_strip(&mut self, ui: &mut Ui, message: &Message, t: &Tokens, height: f32) -> Option<Attachment> {
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
                                    });
                                });
                            },
                        );
                        if save_clicked {
                            requested = Some(attachment.clone());
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
        #[cfg(test)]
        probe::clear_actions(ui);
        #[cfg(test)]
        probe::clear_attachment_saves(ui);
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
        ui.label(typography::text(Role::Caption, "Vista testo · il rendering HTML arriverà con il visualizzatore isolato.", t.text_faint));
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
        ui.scope_builder(UiBuilder::new().max_rect(body_rect), |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("reading-scroll", message.summary.id.as_str()))
                .max_height(body_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut clicked_link = None;
                    for paragraph in message.body_text.split("\n\n") {
                        let paragraph = paragraph.trim();
                        if paragraph.is_empty() {
                            continue;
                        }
                        if paragraph.starts_with('>') {
                            Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::MD)).show(
                                ui,
                                |ui| {
                                    let (link, used) = link_paragraph(ui, paragraph, t, true, link_budget);
                                    if let Some(link) = link {
                                        clicked_link = Some(link);
                                    }
                                    link_budget = link_budget.saturating_sub(used);
                                },
                            );
                        } else {
                            let (link, used) = link_paragraph(ui, paragraph, t, false, link_budget);
                            if let Some(link) = link {
                                clicked_link = Some(link);
                            }
                            link_budget = link_budget.saturating_sub(used);
                        }
                        ui.add_space(space::LG);
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
            && let Some(attachment) = ui
                .scope_builder(UiBuilder::new().max_rect(Rect::from_min_max(pos2(region.left(), strip_top), region.max)), |ui| {
                    self.reading_attachment_strip(ui, &message, t, strip_height)
                })
                .inner
        {
            self.request_attachment_save(&attachment);
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
    }

    fn close_settings(&mut self, ctx: &egui::Context) {
        self.show_accounts_dialog = false;
        self.show_licences = false;
        // Closing Settings discards an unsaved signature editor; reopening must not show stale text.
        self.signature_editor = None;
        self.signature_focus_account = None;
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

    /// Whether an account has a submission endpoint. The demo never does, and a store account is
    /// read-only until SMTP is configured; the composer disables Send with a reason instead of
    /// queueing a row that cannot leave.
    fn smtp_available_for(&self, account_id: &str) -> bool {
        if self.demo {
            return false;
        }
        self.engine
            .account_views()
            .ok()
            .and_then(|views| views.into_iter().find(|view| view.account.id == account_id))
            .and_then(|view| view.server)
            .is_some_and(|server| server.smtp.is_some())
    }

    /// Whether the active account can submit at all. The composer, its Send button and Ctrl+Enter
    /// all read this one rule. A failed account lookup is a "no" with the same visible reason.
    fn send_allowed_for_active_account(&self) -> bool {
        if self.demo {
            return false;
        }
        self.account().is_some_and(|account| self.smtp_available_for(&account.id))
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

    /// The submission journal. Rows are the product's honesty surface: every state is named, a
    /// failure keeps the server's sentence, and an uncertain row never offers a silent retry.
    fn outbox_dialog(&mut self, ui: &mut Ui) {
        if !self.outbox_open {
            return;
        }
        let t = theme::tokens(self.dark);
        let window = ui.ctx().content_rect();
        let width = (window.width() - 2.0 * space::HUGE).clamp(560.0, 780.0);
        // The card must end above the status bar even at 980×600. The frame is capped first, then
        // the list takes only the room left after the fixed header and footer.
        let card_height = (window.height() - ui_size::STATUS - 2.0 * space::XXL).clamp(280.0, 500.0);
        let mut close = false;
        let mut open_settings = false;
        let mut action = None;
        let response = egui::Modal::new(Id::new("postino-outbox"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XXL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(width);
                ui.set_max_height(card_height);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            ui.label(typography::strong(Role::Section, "Coda di invio", t.text));
                            if self.demo {
                                Badge::new("Dati demo").kind(BadgeKind::Accent).show(ui);
                            }
                        });
                        ui.label(typography::text(
                            Role::Meta,
                            "Il messaggio è salvato prima di partire. Puoi annullare finché è in coda; dopo l'invio la copia in Inviati viene ritentata da sola. Postino invia solo mentre l'app o il comando postino send è in esecuzione.",
                            t.text_faint,
                        ));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if Button::ghost("Chiudi").show(ui).clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(space::SM);
                if !self.outbox.halted.is_empty() {
                    for halt in self.outbox.halted.clone() {
                        Frame::new()
                            .fill(t.danger.gamma_multiply(0.12))
                            .stroke(Stroke::new(1.0, t.danger))
                            .corner_radius(radius::CONTROL)
                            .inner_margin(Margin::symmetric(inset::LG, inset::SM))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(icons::icon(icons::WARNING, ui_size::ICON_SM, t.danger));
                                    ui.vertical(|ui| {
                                        ui.label(typography::strong(Role::BodySmall, format!("Invio fermo per {}", halt.account), t.danger));
                                        ui.label(typography::text(Role::Caption, halt.error.as_str(), t.text_muted));
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if Button::ghost("Apri impostazioni").size(ButtonSize::Sm).show(ui).clicked() {
                                            open_settings = true;
                                        }
                                    });
                                });
                            });
                        ui.add_space(space::XS);
                    }
                }
                if self.outbox.entries.is_empty() {
                    ui.add_space(space::LG);
                    ui.vertical_centered(|ui| {
                        ui.label(icons::icon(icons::SEND, 28.0, t.text_faint));
                        ui.add_space(space::SM);
                        ui.label(typography::strong(Role::Emphasis, "Nessun messaggio in coda", t.text));
                        ui.label(typography::text(Role::BodySmall, "Le bozze restano in Bozze finché non scegli Invia.", t.text_muted));
                    });
                    ui.add_space(space::LG);
                } else {
                    let list_height = (ui.available_height() - 44.0).max(80.0);
                    ScrollArea::vertical().id_salt("outbox-scroll").max_height(list_height).auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().scroll = crate::ui::tokens::scroll_style();
                        for entry in self.outbox.entries.clone() {
                            Self::outbox_row(ui, &t, &entry, self.now_utc.timestamp_millis(), self.now_utc, &mut action);
                            ui.add_space(space::XS);
                        }
                    });
                }
                // A damaged row is shown with "Senza oggetto" and no explanation unless its warning
                // reaches the screen; the CLI prints these, so the journal must too.
                for warning in self.outbox.warnings.clone() {
                    ui.add_space(space::XS);
                    ui.horizontal(|ui| {
                        ui.label(icons::icon(icons::WARNING, ui_size::ICON_SM, t.text_muted));
                        ui.label(typography::text(Role::Caption, warning, t.text_muted));
                    });
                }
                ui.add_space(space::SM);
                let _footer = ui.horizontal(|ui| {
                    ui.label(typography::text(Role::Caption, "Ctrl+Shift+O apre questa coda · Esc chiude", t.text_faint));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if Button::ghost("Aggiorna").icon(icons::SYNC).show(ui).clicked() {
                            self.outbox_loaded = false;
                        }
                    });
                });
                #[cfg(test)]
                probe::outbox_footer(ui, _footer.response.rect);
            });
        #[cfg(test)]
        probe::outbox_card(ui, response.response.rect);
        if close || response.should_close() {
            self.outbox_open = false;
            self.outbox_confirm = None;
            ui.ctx().memory_mut(|memory| memory.stop_text_input());
        }
        if open_settings {
            self.outbox_open = false;
            self.outbox_confirm = None;
            self.open_settings(ui.ctx(), None);
        }
        match action {
            Some(OutboxUiAction::Cancel(id)) => match self.engine.call("outbox_cancel", json!({ "id": id })) {
                Ok(_) => {
                    self.refresh_outbox();
                    self.push_info_toast("Invio annullato: la bozza resta in Bozze.", ToastKind::Success);
                }
                Err(_) => self.push_info_toast("Non è stato possibile annullare: l'invio potrebbe essere già partito.", ToastKind::Error),
            },
            Some(OutboxUiAction::Discard(id)) => match self.engine.call("outbox_cancel", json!({ "id": id })) {
                Ok(_) => {
                    self.refresh_outbox();
                    self.push_info_toast("Riga scartata: la bozza resta in Bozze e si può correggere.", ToastKind::Success);
                }
                Err(_) => self.push_info_toast("Non è stato possibile scartare la riga: aggiorna la coda.", ToastKind::Error),
            },
            Some(OutboxUiAction::Retry { id, subject, uncertain }) => {
                self.outbox_confirm = Some(OutboxConfirm::Retry { id, subject, uncertain });
            }
            Some(OutboxUiAction::MarkSent { id, subject }) => {
                self.outbox_confirm = Some(OutboxConfirm::MarkSent { id, subject });
            }
            None => {}
        }
    }

    fn outbox_row(
        ui: &mut Ui,
        t: &crate::theme::Tokens,
        entry: &OutboxView,
        now_ms: i64,
        now_utc: DateTime<Utc>,
        action: &mut Option<OutboxUiAction>,
    ) {
        let (label, kind) = outbox_state_label(entry.state);
        Frame::new()
            .fill(t.panel_alt)
            .stroke(Stroke::new(1.0, if entry.state == postino_store::OutboxState::Uncertain { t.danger } else { t.border }))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::SM))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            let subject = if entry.subject.trim().is_empty() { "Senza oggetto" } else { entry.subject.as_str() };
                            ui.add(egui::Label::new(typography::strong(Role::BodySmall, subject, t.text)).truncate());
                            Badge::new(label).kind(kind).show(ui);
                        });
                        let recipients = plural(entry.recipient_count, "destinatario", "destinatari");
                        let when = if entry.state == postino_store::OutboxState::Queued && entry.send_at > now_ms {
                            format!("parte {} · richiede Postino aperto", format_millis(entry.send_at, now_utc))
                        } else {
                            format_millis(entry.send_at, now_utc)
                        };
                        ui.label(typography::text(Role::Caption, format!("{recipients} · {} · tentativi {}", when, entry.attempts), t.text_faint));
                        if !entry.message_id.is_empty() {
                            ui.add(egui::Label::new(typography::mono(entry.message_id.as_str(), type_size::CAPTION, t.text_faint)).truncate());
                        }
                        if let Some(error) = &entry.last_error {
                            ui.add_space(space::XS);
                            ui.label(typography::text(Role::Caption, error.as_str(), t.danger));
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match entry.state {
                        postino_store::OutboxState::Queued => {
                            if Button::ghost("Annulla").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::Cancel(entry.id.clone()));
                            }
                        }
                        postino_store::OutboxState::Uncertain => {
                            if Button::danger("Riprova manualmente").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::Retry { id: entry.id.clone(), subject: entry.subject.clone(), uncertain: true });
                            }
                            if Button::ghost("Segna come inviato").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::MarkSent { id: entry.id.clone(), subject: entry.subject.clone() });
                            }
                        }
                        postino_store::OutboxState::Failed => {
                            let clicked = Button::primary("Riprova").size(ButtonSize::Sm).show(ui).clicked();
                            if clicked {
                                *action = Some(OutboxUiAction::Retry { id: entry.id.clone(), subject: entry.subject.clone(), uncertain: false });
                            }
                            if Button::ghost("Scarta").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::Discard(entry.id.clone()));
                            }
                        }
                        _ => {}
                    });
                });
            });
    }

    fn outbox_confirm_dialog(&mut self, ui: &mut Ui) {
        let Some(confirm) = self.outbox_confirm.clone() else {
            return;
        };
        let t = theme::tokens(self.dark);
        let (title, body, confirm_label, dangerous) = match &confirm {
            OutboxConfirm::Retry { uncertain, subject, .. } => (
                "Riprovare l'invio?",
                if *uncertain {
                    format!(
                        "«{subject}» ha un esito incerto: il server potrebbe averlo già accettato. Riprovare può consegnarlo due volte. Controlla prima la cartella Inviati del provider."
                    )
                } else {
                    format!("«{subject}» verrà rimesso in coda con gli stessi byte salvati. Il server potrebbe rifiutarlo di nuovo.")
                },
                "Riprova",
                *uncertain,
            ),
            OutboxConfirm::MarkSent { subject, .. } => (
                "Segnare come inviato?",
                format!(
                    "«{subject}» verrà marcato come inviato senza contattare il server. Postino non può dimostrare che sia arrivato: controlla la cartella Inviati del provider."
                ),
                "Segna come inviato",
                false,
            ),
        };
        let mut close = false;
        let mut accept = false;
        let response = egui::Modal::new(Id::new("postino-outbox-confirm"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XXL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(440.0);
                ui.label(typography::strong(Role::Section, title, t.text));
                ui.add_space(space::SM);
                ui.label(typography::text(Role::BodySmall, body, t.text_muted));
                ui.add_space(space::LG);
                ui.horizontal(|ui| {
                    if Button::ghost("Annulla").show(ui).clicked() {
                        close = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = confirm_label;
                        let button = if dangerous { Button::danger(label) } else { Button::primary(label) };
                        if button.show(ui).clicked() {
                            accept = true;
                        }
                    });
                });
            });
        if close || response.should_close() {
            self.outbox_confirm = None;
            return;
        }
        if !accept {
            return;
        }
        let result = match &confirm {
            OutboxConfirm::Retry { id, .. } => self.engine.call("outbox_retry", json!({ "id": id, "confirm": true })),
            OutboxConfirm::MarkSent { id, .. } => self.engine.call("outbox_mark_sent", json!({ "id": id, "confirm": true })),
        };
        self.outbox_confirm = None;
        let marked_sent = matches!(confirm, OutboxConfirm::MarkSent { .. });
        match result {
            Ok(_) => {
                if marked_sent {
                    // `outbox_mark_sent` moves the local draft to Sent; the UI engine still has it in
                    // Drafts until the mailbox is reloaded from the store.
                    self.reload_engine_after_sync(false);
                    self.labels_cache = self.engine.label_summaries();
                }
                self.refresh_outbox();
                self.outbox_loaded = false;
                self.pending_send = self.next_outbox_due();
                self.push_info_toast("Coda aggiornata.", ToastKind::Success);
            }
            Err(_) => self.push_info_toast("Non è stato possibile aggiornare la coda.", ToastKind::Error),
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
            let result = self.engine.call("account_signature_set", json!({ "account": account_id, "signature": text }));
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

    fn composer_window(&mut self, ui: &mut Ui) {
        if !self.composer.open {
            return;
        }
        let Some(account) = self.account().cloned() else {
            self.composer = ComposerState::closed();
            return;
        };
        let t = theme::tokens(self.dark);
        let window = ui.ctx().content_rect();
        let body_area = Rect::from_min_max(window.min, pos2(window.max.x, (window.max.y - ui_size::STATUS).max(window.min.y)));
        let (card, _sheet) = composer_card_rect(window, body_area);
        let margin = Vec2::new(f32::from(inset::XL), f32::from(inset::LG));
        let inner = Rect::from_min_size(card.min + margin, (card.size() - margin * 2.0).max(Vec2::ZERO));
        #[cfg(test)]
        probe::composer_card(ui, card);
        let frame = Frame::new()
            .fill(t.panel)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::WINDOW)
            .inner_margin(Margin::symmetric(inset::XL, inset::LG));
        // A modal layer, not a window: the scrim dims the shell for the whole content area, the
        // backdrop swallows every click and no title bar can be dragged over the toolbar.
        let id = Id::new("postino-composer");
        let area = egui::Modal::default_area(id).anchor(Align2::LEFT_TOP, card.min - window.min).constrain(false);
        let smtp_available = self.smtp_available_for(&account.id);
        let attachments_available = !self.demo && self.store.is_some();
        let response = egui::Modal::new(id).area(area).backdrop_color(theme::scrim(self.dark)).frame(frame).show(ui.ctx(), |ui| {
            ui.set_min_size(inner.size());
            ui.set_max_width(inner.width());
            ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
                Composer::new(&mut self.composer, &account)
                    .send_available(smtp_available)
                    .demo(self.demo)
                    .attachments_available(attachments_available)
                    .show(ui)
            })
            .inner
        });
        #[cfg(test)]
        probe::composer_surface(ui, response.response.rect);
        let action = response.inner;
        if action.discard {
            self.discard_composer();
            return;
        }
        if action.close {
            self.close_composer();
            return;
        }
        if let Some(attachment) = action.remove_attachment.clone() {
            self.remove_composer_attachment(&attachment);
        }
        if action.pick_files {
            self.pick_composer_files();
        }
        if !action.dropped_files.is_empty() {
            self.attach_composer_files(action.dropped_files);
        }
        if action.send {
            self.send_composer();
        } else if action.save {
            let _ = self.save_composer(true);
        }
        if !self.composer.open {
            ui.ctx().memory_mut(|memory| memory.stop_text_input());
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
        }
    }

    fn execute_command(&mut self, id: &'static str, ctx: &egui::Context) {
        self.show_palette = false;
        ctx.memory_mut(|memory| memory.stop_text_input());
        match id {
            "compose" => self.compose_new(),
            "reply" => {
                if let Some(message) = self.selected.clone() {
                    self.reply_to(&message, ComposeMode::Reply);
                }
            }
            "reply_all" => {
                if let Some(message) = self.selected.clone() {
                    self.reply_to(&message, ComposeMode::ReplyAll);
                }
            }
            "forward" => {
                if let Some(message) = self.selected.clone() {
                    self.reply_to(&message, ComposeMode::Forward);
                }
            }
            "edit_draft" => {
                if let Some(message) = self.selected.clone() {
                    self.edit_draft(&message);
                }
            }
            "send" => self.send_composer(),
            "outbox" => {
                self.refresh_outbox();
                self.outbox_open = true;
                self.status.clear();
            }
            "open" => {
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
            "expand" => {
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
            "read" => {
                if let Some(message) = self.selected.clone() {
                    self.set_unread_for(&message, false);
                }
            }
            "unread" => self.mark_unread(),
            "star" => self.set_starred(true),
            "unstar" => self.set_starred(false),
            "archive" => self.move_selected(FolderKind::Archive, "Messaggio archiviato", "Messaggi archiviati"),
            "delete" => self.move_selected(FolderKind::Trash, "Messaggio spostato nel cestino", "Messaggi spostati nel cestino"),
            "label_menu" => {
                let ids = self.action_ids();
                self.open_label_menu(ids);
            }
            "label_new" => self.start_label_create(),
            "label_rename" => {
                if let Some(id) = self.label_filter.clone() {
                    self.start_label_rename(&id);
                }
            }
            "label_delete" => {
                if let Some(id) = self.label_filter.clone() {
                    self.request_label_delete(&id);
                }
            }
            "label_clear_filter" => {
                if self.label_filter.is_some() {
                    self.clear_label_filter();
                } else {
                    self.status = "Nessun filtro etichetta attivo".to_owned();
                }
            }
            "inbox" => {
                if let Some(folder) = self.folder_for_kind(FolderKind::Inbox) {
                    self.select_folder(folder);
                } else {
                    self.status = "Il server non ha una cartella Posta in arrivo".to_owned();
                }
            }
            "starred" => {
                if let Some(folder) = self.folder_for_kind(FolderKind::Starred) {
                    self.select_folder(folder);
                } else {
                    self.status = "Il server non espone una cartella Preferiti".to_owned();
                }
            }
            "theme" => {
                let dark = !self.dark;
                self.set_dark(ctx, dark);
            }
            "reduced_motion" => {
                let reduced = !self.reduced_motion;
                self.set_reduced_motion(ctx, reduced);
                self.status = if reduced { "Animazioni ridotte" } else { "Animazioni attive" }.to_owned();
            }
            "about" => self.show_about = true,
            "shortcuts" => {
                self.show_shortcuts = true;
                self.shortcut_query.clear();
                self.shortcut_focus = true;
            }
            "palette" => self.show_palette = true,
            "select_all" => {
                let (rows, _, _) = self.visible_messages();
                self.select_all(&rows);
            }
            "toggle" => {
                if let Some(message) = self.selected.clone() {
                    self.toggle_selection(message);
                }
            }
            "clear_selection" => self.clear_selection(),
            "focus_search" => self.focus_search = true,
            "threads_toggle" => self.toggle_threads_view(),
            "saved_search_save" => self.start_saved_search_save(),
            "saved_search_rename" => {
                if let Some(id) = self.applied_saved.clone() {
                    self.start_saved_search_rename(&id);
                } else {
                    self.status = "Seleziona prima una ricerca salvata".to_owned();
                }
            }
            "saved_search_delete" => {
                if let Some(id) = self.applied_saved.clone() {
                    self.delete_saved_search(&id);
                } else {
                    self.status = "Seleziona prima una ricerca salvata".to_owned();
                }
            }
            "saved_search_clear_recent" => {
                let account = self.active_account.clone();
                match self.engine.clear_recent_searches(account.as_deref()) {
                    Ok(true) => {
                        self.refresh_recent_searches();
                        self.status = "Ricerche recenti cancellate".to_owned();
                    }
                    Ok(false) => self.status = "Nessuna ricerca recente da cancellare".to_owned(),
                    Err(_) => self.report_failure("Non è stato possibile cancellare le ricerche recenti.", false),
                }
            }
            "rules" => self.open_rules(false),
            "rules_new" => self.open_rules(true),
            "next" => self.step_selection(1, false),
            "previous" => self.step_selection(-1, false),
            "extend_next" => self.step_selection(1, true),
            "extend_previous" => self.step_selection(-1, true),
            "mark_folder_read" => {
                let folder = self.folder.clone();
                self.mark_all_read(&folder);
            }
            "back" => self.reading_full = false,
            "close" => {
                if self.reading_full {
                    self.reading_full = false;
                } else {
                    self.close_reading();
                }
            }
            "undo" => self.undo_last(),
            "sync" => self.request_sync(),
            "settings" => self.open_settings(ctx, self.account_switcher_id),
            "add_account" => {
                if self.can_add_account() {
                    self.onboarding = Some(Onboarding::add_account());
                } else {
                    self.status = "La modalità demo non ha un archivio locale".to_owned();
                }
            }
            _ => {}
        }
    }
}

impl PostinoApp {
    /// One frame of the whole shell. Public so a headless test can render it without a window.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        // The root UI can be wider than the window at fractional display scaling. Pin the whole
        // shell to the real content rect so right-aligned controls and the status bar stay inside.
        let content = ui.ctx().content_rect();
        ui.scope_builder(UiBuilder::new().max_rect(content), |ui| self.show_inner(ui));
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
        self.link_confirmation(ui);
        self.attachment_warning_dialog(ui);
        self.attachment_overwrite_dialog(ui);
        self.licences_dialog(ui);
        // The composer owns the keyboard while it is open. Capture the widget that had it before
        // the composer requests focus, so closing returns the user exactly where they were.
        if self.composer.open && self.composer_focus_pending {
            self.composer_return_focus = ui.ctx().memory(|memory| memory.focused());
            self.composer_focus_pending = false;
        }
        self.composer_window(ui);
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
    EditDraft,
    Star,
    Read,
    Close,
    Expand,
    Back,
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

/// An attachment whose bytes are already read from the blob store and whose destination is being
/// confirmed. The bytes are kept only for the lifetime of the dialog.
#[derive(Clone, Debug)]
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

/// A failed forward copy. `notice` is the user-facing sentence and `failed_ids` names the
/// composer-local metadata rows so Remove can find them before a store save assigns ids.
#[derive(Default)]
struct ForwardCopy {
    notice: Option<String>,
    failed_ids: Vec<String>,
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

enum OutboxUiAction {
    Cancel(String),
    Retry {
        id: String,
        subject: String,
        uncertain: bool,
    },
    MarkSent {
        id: String,
        subject: String,
    },
    /// Throw away a permanently failed journal row. No delivery happened, so the draft becomes
    /// editable again. Never offered for an uncertain row.
    Discard(String),
}

fn outbox_state_label(state: postino_store::OutboxState) -> (&'static str, BadgeKind) {
    use postino_store::OutboxState;
    match state {
        OutboxState::Queued => ("In coda", BadgeKind::Neutral),
        OutboxState::Sending => ("Invio in corso", BadgeKind::Accent),
        OutboxState::Sent => ("Inviato", BadgeKind::Neutral),
        OutboxState::AppendPending => ("Copia in Inviati", BadgeKind::Accent),
        OutboxState::Uncertain => ("Invio incerto", BadgeKind::Danger),
        OutboxState::Failed => ("Non riuscito", BadgeKind::Danger),
        OutboxState::Cancelled => ("Annullato", BadgeKind::Neutral),
    }
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

/// What a command needs to be enabled. The palette, the context menus and the keyboard map read
/// the same rule, so a disabled action always explains itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandRule {
    Always,
    Message,
    ActiveLabel,
    LabelFilter,
    /// Sync needs a store-backed account. It is disabled with a reason in the demo.
    Sync,
    /// Adding or removing accounts needs the encrypted store; the demo is memory-only.
    Store,
}

impl CommandRule {
    /// `None` when the command is always available, so a caller can show a reason only when one
    /// exists instead of matching on an empty string.
    fn reason(self) -> Option<&'static str> {
        match self {
            Self::Always => None,
            Self::Message => Some("Seleziona prima un messaggio"),
            Self::ActiveLabel => Some("Seleziona prima un'etichetta"),
            Self::LabelFilter => Some("Nessun filtro etichetta attivo"),
            Self::Sync => Some("Aggiungi un account per sincronizzare"),
            Self::Store => Some("La modalità demo non ha un archivio locale"),
        }
    }

    fn disabled_reason(self) -> &'static str {
        self.reason().unwrap_or("")
    }
}

#[derive(Clone, Copy)]
struct Command {
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    shortcut: &'static str,
    rule: CommandRule,
    in_palette: bool,
}

const COMMANDS: &[Command] = &[
    Command { id: "compose", label: "Scrivi", icon: icons::COMPOSE, shortcut: "C", rule: CommandRule::Always, in_palette: true },
    Command { id: "open", label: "Apri messaggio", icon: icons::MAIL, shortcut: "Invio", rule: CommandRule::Message, in_palette: true },
    Command { id: "reply", label: "Rispondi", icon: icons::REPLY, shortcut: "R", rule: CommandRule::Message, in_palette: true },
    Command { id: "reply_all", label: "Rispondi a tutti", icon: icons::REPLY_ALL, shortcut: "A", rule: CommandRule::Message, in_palette: true },
    Command { id: "forward", label: "Inoltra", icon: icons::FORWARD, shortcut: "F", rule: CommandRule::Message, in_palette: true },
    Command { id: "edit_draft", label: "Modifica bozza", icon: icons::COMPOSE, shortcut: "", rule: CommandRule::Message, in_palette: false },
    Command { id: "send", label: "Invia", icon: icons::SEND, shortcut: "Ctrl+Invio", rule: CommandRule::Always, in_palette: false },
    Command { id: "outbox", label: "Coda di invio", icon: icons::SEND, shortcut: "Ctrl+Shift+O", rule: CommandRule::Always, in_palette: true },
    Command { id: "expand", label: "Apri a schermo intero", icon: icons::SQUARE, shortcut: "", rule: CommandRule::Message, in_palette: true },
    Command { id: "read", label: "Segna come letto", icon: icons::MARK_READ, shortcut: "", rule: CommandRule::Message, in_palette: true },
    Command { id: "unread", label: "Segna come non letto", icon: icons::MARK_UNREAD, shortcut: "U", rule: CommandRule::Message, in_palette: true },
    Command { id: "star", label: "Aggiungi stella", icon: icons::STAR, shortcut: "S", rule: CommandRule::Message, in_palette: true },
    Command { id: "unstar", label: "Togli stella", icon: icons::STAR, shortcut: "S", rule: CommandRule::Message, in_palette: true },
    Command { id: "archive", label: "Archivia", icon: icons::ARCHIVE, shortcut: "E", rule: CommandRule::Message, in_palette: true },
    Command { id: "delete", label: "Elimina", icon: icons::TRASH, shortcut: "Canc", rule: CommandRule::Message, in_palette: true },
    Command { id: "label_menu", label: "Etichetta", icon: icons::TAG, shortcut: "L", rule: CommandRule::Message, in_palette: true },
    Command { id: "label_new", label: "Nuova etichetta", icon: icons::PLUS, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: "label_rename",
        label: "Rinomina etichetta",
        icon: icons::COMPOSE,
        shortcut: "F2",
        rule: CommandRule::ActiveLabel,
        in_palette: true,
    },
    Command { id: "label_delete", label: "Elimina etichetta", icon: icons::TRASH, shortcut: "", rule: CommandRule::ActiveLabel, in_palette: true },
    Command {
        id: "label_clear_filter",
        label: "Togli filtro etichetta",
        icon: icons::FILTER,
        shortcut: "",
        rule: CommandRule::LabelFilter,
        in_palette: true,
    },
    Command { id: "inbox", label: "Vai a Posta in arrivo", icon: icons::INBOX, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: "starred", label: "Vai a Preferiti", icon: icons::STAR, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: "theme", label: "Cambia tema", icon: icons::MOON, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: "about", label: "Informazioni su Postino", icon: icons::INFO, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: "shortcuts", label: "Scorciatoie da tastiera", icon: icons::KEYBOARD, shortcut: "?", rule: CommandRule::Always, in_palette: true },
    Command { id: "select_all", label: "Seleziona tutto", icon: icons::LIST, shortcut: "Ctrl+A", rule: CommandRule::Always, in_palette: true },
    Command {
        id: "toggle",
        label: "Seleziona o deseleziona",
        icon: icons::CHECK,
        shortcut: "X / Spazio",
        rule: CommandRule::Message,
        in_palette: true,
    },
    Command { id: "clear_selection", label: "Deseleziona", icon: icons::X_CIRCLE, shortcut: "Esc", rule: CommandRule::Always, in_palette: true },
    Command { id: "focus_search", label: "Cerca nelle mail", icon: icons::SEARCH, shortcut: "/", rule: CommandRule::Always, in_palette: true },
    Command { id: "threads_toggle", label: "Conversazioni o messaggi", icon: icons::LIST, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: "saved_search_save", label: "Salva ricerca", icon: icons::TAG, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: "saved_search_rename",
        label: "Rinomina ricerca",
        icon: icons::COMPOSE,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command { id: "saved_search_delete", label: "Elimina ricerca", icon: icons::TRASH, shortcut: "", rule: CommandRule::Always, in_palette: false },
    Command { id: "rules", label: "Regole locali", icon: icons::FILTER, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: "rules_new", label: "Nuova regola", icon: icons::PLUS, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: "saved_search_clear_recent",
        label: "Cancella ricerche recenti",
        icon: icons::SEARCH,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: "next",
        label: "Sposta il focus in basso",
        icon: icons::CHEVRON_DOWN,
        shortcut: "J / ↓",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: "previous",
        label: "Sposta il focus in alto",
        icon: icons::CHEVRON_UP,
        shortcut: "K / ↑",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: "extend_next",
        label: "Estendi selezione in basso",
        icon: icons::CHEVRON_DOWN,
        shortcut: "Shift+J / Shift+↓",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: "extend_previous",
        label: "Estendi selezione in alto",
        icon: icons::CHEVRON_UP,
        shortcut: "Shift+K / Shift+↑",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command { id: "close", label: "Chiudi lettura", icon: icons::CLOSE, shortcut: "Esc", rule: CommandRule::Always, in_palette: false },
    Command { id: "undo", label: "Annulla ultima azione", icon: icons::UNDO, shortcut: "Z", rule: CommandRule::Always, in_palette: false },
    Command { id: "palette", label: "Apri i comandi", icon: icons::COMMAND, shortcut: "Ctrl+K", rule: CommandRule::Always, in_palette: false },
    Command { id: "sync", label: "Aggiorna la posta", icon: icons::SYNC, shortcut: "", rule: CommandRule::Sync, in_palette: true },
    Command { id: "settings", label: "Impostazioni", icon: icons::SETTINGS, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: "add_account", label: "Aggiungi account", icon: icons::PLUS, shortcut: "", rule: CommandRule::Store, in_palette: true },
    Command { id: "reduced_motion", label: "Riduci le animazioni", icon: icons::CLOCK, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: "mark_folder_read",
        label: "Segna tutto come letto",
        icon: icons::MARK_READ,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command { id: "back", label: "Torna alla posta", icon: icons::ARROW_LEFT, shortcut: "Esc", rule: CommandRule::Always, in_palette: false },
];

fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

fn command_label(id: &str, fallback: &'static str) -> &'static str {
    command(id).map_or(fallback, |command| command.label)
}

fn command_icon(id: &str, fallback: &'static str) -> &'static str {
    command(id).map_or(fallback, |command| command.icon)
}

fn command_shortcut(id: &str, fallback: &'static str) -> &'static str {
    command(id).map_or(fallback, |command| command.shortcut)
}

fn menu_item(id: &str) -> MenuItem<'static> {
    match command(id) {
        Some(Command { rule: CommandRule::Sync, .. }) => MenuItem::new(command_label(id, "Aggiorna"))
            .icon(command_icon(id, icons::SYNC))
            .enabled(false)
            .reason(command(id).and_then(|command| command.rule.reason()).unwrap_or("")),
        Some(command) => MenuItem::new(command.label).icon(command.icon).shortcut(command.shortcut),
        None => MenuItem::new("Azione non disponibile").enabled(false).reason("Comando non registrato"),
    }
}

fn filtered_commands(query: &str) -> Vec<&'static Command> {
    let query = query.trim().to_lowercase();
    COMMANDS
        .iter()
        .filter(|command| command.in_palette)
        .filter(|command| query.is_empty() || command.label.to_lowercase().contains(&query) || command.shortcut.to_lowercase().contains(&query))
        .collect()
}

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
fn truncate_link_label(label: &str) -> String {
    if label.chars().count() <= reading::MAX_LINK_LABEL_CHARS {
        return label.to_owned();
    }
    let mut shown = label.chars().take(reading::MAX_LINK_LABEL_CHARS.saturating_sub(1)).collect::<String>();
    shown.push('…');
    shown
}

/// A hover tooltip is capped; the sheet has the full URL in a scrollable body.
fn truncate_tooltip_url(url: &str) -> String {
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

/// Translate a save failure without losing the actionable limit. A failed forward copy already has a
/// precise sentence and must not be replaced by the generic save line.
fn draft_save_error_message(attachment_failure: &Option<String>, raw: &str) -> String {
    if let Some(failure) = attachment_failure {
        return failure.clone();
    }
    let raw = raw.strip_prefix("invalid arguments: ").unwrap_or(raw);
    if raw.contains("subject is longer") {
        return "L'oggetto è troppo lungo (massimo 998 byte): accorcialo prima di salvare.".to_owned();
    }
    if raw.contains("body_text is larger") {
        return "Il corpo è troppo lungo (massimo 1 MiB): accorcia il testo o la citazione prima di salvare.".to_owned();
    }
    if raw.contains("attachments") && raw.contains("at most") {
        return "Gli allegati superano un limite (20 file, 25 MiB per file, 25 MiB totali).".to_owned();
    }
    if raw.contains("in_reply_to") {
        return "Il collegamento alla conversazione non è valido: riapri la risposta dal messaggio originale.".to_owned();
    }
    "Non è stato possibile salvare la bozza. Controlla oggetto, destinatari e allegati.".to_owned()
}

/// Pull the attachment filename out of the engine's send refusal. The filename is already
/// sanitized; a quoted name is shown with its quotes escaped rather than dropped.
fn missing_attachment_name(raw: &str) -> Option<String> {
    let rest = raw.split("attachment \"").nth(1)?;
    let end = rest.find("\" has no stored bytes")?;
    rest.get(..end).map(|name| name.replace("\\\"", "\""))
}

/// Translate an engine send failure into Italian copy. The raw sentence never reaches the shell.
fn send_error_message(raw: &str, demo: bool) -> String {
    if demo || raw.contains("demo mailbox") {
        return "La modalità demo non invia messaggi: aggiungi un account reale con un server SMTP.".to_owned();
    }
    if raw.contains("no SMTP endpoint") {
        return "Questo account non ha un server di invio (SMTP): rimuovilo e aggiungilo con i dati SMTP.".to_owned();
    }
    if raw.contains("no stored bytes") || raw.contains("attachments without content") {
        return match missing_attachment_name(raw) {
            Some(name) => format!("Allegato \"{name}\" mancante o illeggibile: rimuovilo o riattaccalo prima di inviare."),
            None => "Un allegato è mancante o illeggibile: rimuovilo o riattaccalo prima di inviare.".to_owned(),
        };
    }
    if raw.contains("at least one recipient") {
        return "Aggiungi almeno un destinatario prima di inviare.".to_owned();
    }
    if raw.contains("100") && raw.contains("recipients") {
        return "Il messaggio ha troppi destinatari: il limite è 100.".to_owned();
    }
    if raw.contains("failed submission") {
        return "L'ultimo invio di questa bozza è stato rifiutato: scartalo dalla coda e correggi la bozza.".to_owned();
    }
    if raw.contains("already sent") {
        return "Questa bozza è già stata inviata: crea una nuova bozza per inviarla di nuovo.".to_owned();
    }
    if raw.contains("queued") || raw.contains("outbox") {
        return "La bozza è già in coda: annulla l'invio dalla coda oppure attendi l'esito.".to_owned();
    }
    "Non è stato possibile mettere il messaggio in coda. Riprova.".to_owned()
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

/// The composer surface rectangle. A window of at least 1000×700 gets a centred card capped to the
/// content height and never touching the status bar; anything smaller gets a sheet with LG margins
/// over the body area so its controls stay reachable at 980×600.
fn composer_card_rect(window: Rect, body: Rect) -> (Rect, bool) {
    let card = window.width() >= COMPOSER_CARD_MIN_WIDTH && window.height() >= COMPOSER_CARD_MIN_HEIGHT;
    if !card {
        return (body.shrink(space::LG), true);
    }
    let margin = Vec2::new(space::XXL, space::XL);
    let size = Vec2::new(
        ui_size::COMPOSER_CARD_WIDTH.min((body.width() - 2.0 * margin.x).max(0.0)),
        ui_size::COMPOSER_HEIGHT.min((body.height() - 2.0 * margin.y).max(0.0)),
    );
    (Rect::from_center_size(body.center(), size), false)
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

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", format!("{:.1}", bytes as f64 / 1024.0).replace('.', ","))
    } else {
        format!("{} MB", format!("{:.1}", bytes as f64 / (1024.0 * 1024.0)).replace('.', ","))
    }
}

/// Read one attachment with the P8 cap enforced before and during the read. The error never includes
/// the full path: a toast names the file, not the folder the person happens to keep it in.
fn read_attachment_file(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    let metadata = std::fs::metadata(path).map_err(|_| "il file non è leggibile".to_owned())?;
    if !metadata.is_file() {
        return Err("non è un file".to_owned());
    }
    if metadata.len() > postino_core::MAX_ATTACHMENT_BYTES {
        return Err(format!("supera {}", format_size(postino_core::MAX_ATTACHMENT_BYTES)));
    }
    let file = std::fs::File::open(path).map_err(|_| "il file non è leggibile".to_owned())?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(postino_core::MAX_ATTACHMENT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "la lettura del file è fallita".to_owned())?;
    if bytes.len() as u64 > postino_core::MAX_ATTACHMENT_BYTES {
        return Err(format!("supera {}", format_size(postino_core::MAX_ATTACHMENT_BYTES)));
    }
    Ok(bytes)
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
    }

    pub fn link_widget(ui: &Ui, url: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("link-widgets")).insert(url.to_owned(), rect);
        });
    }

    pub fn link_rect(ctx: &Context, url: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("link-widgets"))).and_then(|links| links.get(url).copied())
    }

    pub fn link_count(ctx: &Context) -> usize {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("link-widgets"))).map_or(0, |links| links.len())
    }

    pub fn clear_attachment_saves(ui: &Ui) {
        ui.ctx().data_mut(|data| data.insert_temp(id("attachment-saves"), BTreeMap::<String, Rect>::new()));
    }

    pub fn attachment_save(ui: &Ui, attachment_id: &str, rect: Rect) {
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<BTreeMap<String, Rect>>(id("attachment-saves")).insert(attachment_id.to_owned(), rect);
        });
    }

    pub fn attachment_save_rect(ctx: &Context, attachment_id: &str) -> Option<Rect> {
        ctx.data(|data| data.get_temp::<BTreeMap<String, Rect>>(id("attachment-saves"))).and_then(|rows| rows.get(attachment_id).copied())
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
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use postino_core::CredentialVault;
    use postino_store::{
        Account as StoreAccount, AccountServer, AccountTls, Address as StoreAddress, Attachment as StoreAttachment, Folder as StoreFolder,
        FolderKind as StoreFolderKind,
    };
    use postino_store::{Message as StoreMessage, MessageSummary as StoreMessageSummary};

    use crate::boot::{BootStage, KeySource};
    use crate::test_support::{
        MemoryVault, RecordingFileSaver, RecordingFolderOpener, RecordingLinkOpener, RunningServer, RunningSmtp, directory, three_folders,
    };

    use super::*;

    /// One text run actually painted by a frame: the galley text, its rectangle and the clip that
    /// applied to it. `clip` is what makes a layout assertion clip-aware: a section painted outside
    /// its scroll viewport still appears in `output.shapes`, but its clip no longer contains it.
    #[derive(Debug)]
    struct PaintedRun {
        text: String,
        rect: Rect,
        clip: Rect,
    }

    fn painted_runs(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) -> Vec<PaintedRun> {
        fn collect(shape: &egui::epaint::Shape, clip: Rect, runs: &mut Vec<PaintedRun>) {
            match shape {
                egui::epaint::Shape::Text(shape) => {
                    let rect = Rect::from_min_size(shape.pos, shape.galley.size());
                    runs.push(PaintedRun { text: shape.galley.text().to_owned(), rect, clip });
                }
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, clip, runs);
                    }
                }
                _ => {}
            }
        }
        let mut runs = Vec::new();
        // A modal animates in; tests need its settled layout, not a translated frame mid-animation.
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        // A modal is laid out on its own layer and enters with a short offset; the first frames
        // create the area and settle it, then the content paints. Real windows run them, so tests
        // read the settled frame.
        for _ in 0..4 {
            let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| app.show(ui));
            runs.clear();
            for clipped in &output.shapes {
                collect(&clipped.shape, clipped.clip_rect, &mut runs);
            }
            output.textures_delta.clear();
        }
        runs
    }

    /// The text actually painted by one frame, for assertions that a settings section or a warning
    /// reached the screen rather than only the state.
    fn painted_text(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) -> Vec<String> {
        painted_runs(ctx, app, size).into_iter().map(|run| run.text).collect()
    }

    fn frame_at(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, events: Vec<egui::Event>) {
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), events, ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();
    }
    fn frame(ctx: &egui::Context, app: &mut PostinoApp, events: Vec<egui::Event>) {
        frame_at(ctx, app, Vec2::new(1200.0, 800.0), events);
    }

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    /// A dropped file for a headless frame. The bytes are never read by the composer; the screen
    /// opens the path itself through the same bounded reader as the picker.
    #[derive(Debug)]
    struct TestDroppedFile(PathBuf);

    impl egui::DroppedFile for TestDroppedFile {
        fn path(&self) -> &std::path::Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|error| error.to_string())
        }
    }

    fn frame_with_drop(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, pos: egui::Pos2, paths: Vec<PathBuf>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events: vec![egui::Event::PointerMoved(pos)],
            dropped_files: paths.into_iter().map(|path| Arc::new(TestDroppedFile(path)) as egui::DroppedFileHandle).collect(),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();
    }

    fn ready_app() -> (egui::Context, PostinoApp) {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let app = PostinoApp::demo(true);
        (ctx, app)
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    /// A real press/release pair on the widget whose rect the last frame recorded. The two events
    /// arrive in consecutive frames, like a physical mouse, so egui can resolve the click target
    /// from a settled layout.
    fn click_at(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) {
        frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos), pointer(pos, true)]);
        frame_at(ctx, app, size, vec![pointer(pos, false)]);
    }

    fn click(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2) {
        click_at(ctx, app, pos, Vec2::new(1200.0, 800.0));
    }

    /// A click on a modal that has just appeared: move, press and release arrive on separate frames,
    /// so egui resolves the hover before the button sees the press.
    fn click_settled(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) {
        frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos)]);
        frame_at(ctx, app, size, vec![pointer(pos, true)]);
        frame_at(ctx, app, size, vec![pointer(pos, false)]);
        frame_at(ctx, app, size, vec![]);
    }

    /// A click whose release-frame platform output is returned, for clipboard assertions.
    fn click_at_output(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) -> egui::PlatformOutput {
        frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos), pointer(pos, true)]);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events: vec![pointer(pos, false)],
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();
        output.platform_output
    }

    fn copied_text(output: &egui::PlatformOutput) -> Option<String> {
        output.commands.iter().find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
    }

    /// Let a modal animate in and register its probes without injecting input.
    fn settle_at(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) {
        for _ in 0..4 {
            frame_at(ctx, app, size, Vec::new());
        }
    }

    /// Click a dialog button by the probe the last settled frame recorded.
    fn click_dialog_button(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, title: &str, primary: bool) {
        settle_at(ctx, app, size);
        let rect = probe::dialog_button_rect(ctx, title, primary).unwrap_or_else(|| panic!("the {title:?} button was not probed"));
        click_at(ctx, app, rect.center(), size);
    }

    /// Two press/release pairs inside the double-click window, like a physical mouse. The second
    /// press resolves against the layout of the frame after the first click, as it does for a user.
    fn double_click_at(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) {
        frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos), pointer(pos, true)]);
        frame_at(ctx, app, size, vec![pointer(pos, false)]);
        // Let a pane opened by the first click settle before the second press resolves its hit.
        frame_at(ctx, app, size, vec![]);
        frame_at(ctx, app, size, vec![pointer(pos, true)]);
        frame_at(ctx, app, size, vec![pointer(pos, false)]);
    }

    /// Small wheel steps are applied without easing, so one frame scrolls deterministically.
    fn wheel(pos: egui::Pos2, steps: usize) -> Vec<egui::Event> {
        wheel_by(pos, steps, -6.0)
    }

    fn wheel_by(pos: egui::Pos2, steps: usize, step: f32) -> Vec<egui::Event> {
        let mut events = vec![egui::Event::PointerMoved(pos)];
        events.extend((0..steps).map(|_| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, step),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
        }));
        events
    }

    #[test]
    fn shell_renders_one_frame() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let mut app = PostinoApp::demo(true);
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        let painted = !output.shapes.is_empty();
        // A headless test has no painter to consume the font-atlas upload.
        output.textures_delta.clear();
        assert!(painted, "the shell painted nothing");
    }

    #[test]
    fn dark_theme_renders_one_frame() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, true);
        let mut app = PostinoApp::demo(true);
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        let painted = !output.shapes.is_empty();
        output.textures_delta.clear();
        assert!(painted, "the dark shell painted nothing");
    }

    #[test]
    fn demo_starts_without_a_selection() {
        let app = PostinoApp::demo(true);
        assert_eq!(app.folder, "inbox");
        assert!(app.selected.is_none());
        assert!(!app.reading_open);
    }

    #[test]
    fn selecting_opens_the_reading_pane() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages()[0].id.clone();
        app.open_message(id.clone());
        assert_eq!(app.selected.as_deref(), Some(id.as_str()));
        assert!(app.reading_open);
        assert!(!app.reading_full);
    }

    #[test]
    fn focusing_does_not_open_the_reading_pane() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages()[0].id.clone();
        app.focus_message(id.clone());
        assert_eq!(app.selected.as_deref(), Some(id.as_str()));
        assert!(!app.reading_open);
    }

    #[test]
    fn opening_an_unread_message_marks_it_read_after_the_delay_and_undoes() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages().iter().find(|message| message.unread).expect("an unread demo message").id.clone();
        app.now = 10.0;
        app.open_message(id.clone());
        assert!(app.engine.message(&id).is_some_and(|message| message.summary.unread), "opening must not mark read immediately");
        app.now = 10.7;
        app.maybe_complete_read();
        assert!(app.engine.message(&id).is_some_and(|message| !message.summary.unread), "the 600 ms timer must mark the message read");
        assert!(app.toast.is_some(), "the read action must offer an undo");
        app.undo_last();
        assert!(app.engine.message(&id).is_some_and(|message| message.summary.unread), "undo must restore the unread state");
    }

    #[test]
    fn keyboard_navigates_opens_expands_and_closes() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();
        let first = rows[0].id.clone();
        let second = rows[1].id.clone();

        frame(&ctx, &mut app, vec![key(Key::J, Modifiers::NONE)]);
        assert_eq!(app.selected.as_deref(), Some(first.as_str()), "J focuses the first row");
        assert!(!app.reading_open, "focusing must not open the pane");

        frame(&ctx, &mut app, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(app.selected.as_deref(), Some(second.as_str()), "ArrowDown moves to the next row");
        assert!(!app.reading_open);

        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.reading_open, "Enter opens the focused row");
        assert!(!app.reading_full);

        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.reading_full, "a second Enter expands the pane");

        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.reading_open, "Esc first returns from full content");
        assert!(!app.reading_full);

        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.reading_open, "a second Esc closes the reading pane");
    }

    #[test]
    fn multi_selection_toggles_extends_and_selects_all() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();

        app.select_only(rows[0].id.clone());
        app.toggle_selection(rows[1].id.clone());
        assert_eq!(app.selection.len(), 2, "Ctrl-click adds a second row");
        assert_eq!(app.selected.as_deref(), Some(rows[1].id.as_str()), "the last toggled row takes focus");
        assert!(!app.reading_open, "multi-select closes the single-message pane");

        app.extend_selection(rows[3].id.clone(), &rows);
        assert_eq!(app.selection.len(), 3, "Shift-click extends from the anchor");
        assert!(app.selection.contains(&rows[3].id));

        app.select_all(&rows);
        assert_eq!(app.selection.len(), rows.len());
        assert!(!app.reading_open);
        app.clear_selection();
        assert!(app.selection.is_empty());
        assert!(app.selected.is_none());
    }

    #[test]
    fn multi_selection_renders_the_bulk_bar() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();
        app.select_all(&rows);
        frame(&ctx, &mut app, vec![]);
        assert!(app.selection.len() > 1);
        assert!(!app.reading_open);
    }

    #[test]
    fn shift_arrow_extends_the_selection_headlessly() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();

        frame(&ctx, &mut app, vec![key(Key::J, Modifiers::NONE)]);
        assert_eq!(app.selected.as_deref(), Some(rows[0].id.as_str()));
        frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::SHIFT), key(Key::J, Modifiers::SHIFT)]);
        assert_eq!(app.selection.len(), 2, "Shift+J extends from the anchor");
        assert!(app.selection.contains(&rows[0].id));
        assert!(app.selection.contains(&rows[1].id));
        assert!(!app.reading_open);
    }

    #[test]
    fn moving_a_batch_is_one_undo_step() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();
        let ids = vec![rows[0].id.clone(), rows[1].id.clone()];
        app.select_only(rows[0].id.clone());
        app.toggle_selection(rows[1].id.clone());
        app.move_many(&ids, "archive", "Messaggio archiviato", "Messaggi archiviati");
        assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id == "archive")));
        assert!(app.selection.is_empty(), "moving clears the selection");
        app.undo_last();
        assert!(
            ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id != "archive")),
            "one undo restores the whole batch"
        );
    }

    #[test]
    fn batch_flags_share_one_undo_step() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let ids = app.engine.messages().iter().filter(|message| message.unread).take(2).map(|message| message.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        app.set_unread_many(&ids, false);
        assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| !message.summary.unread)));
        app.undo_last();
        assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.unread)), "one undo restores every flag change");
    }

    #[test]
    fn the_demo_composer_disables_attaching_with_a_reason_and_never_opens_a_picker() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.compose_new();
        frame(&ctx, &mut app, vec![]);
        assert_eq!(probe::composer_attach_enabled(&ctx), Some(false), "the demo must not pretend it can attach");
        let picker = crate::test_support::RecordingFilePicker::new();
        app.file_picker = picker.clone();
        let rect = probe::composer_attach_rect(&ctx).expect("the Allega file button");
        click(&ctx, &mut app, rect.center());
        assert_eq!(picker.calls(), 0, "a disabled Allega button must not open a dialog");
        assert!(app.composer.attachments.is_empty());
    }

    fn picker_app(name: &str) -> PostinoApp {
        booted_app(name, Vec::new(), |store| seed_account_rows(store, "acc_a", "a@example.com", None))
    }

    #[test]
    fn the_picker_attaches_exact_bytes_lists_the_total_and_removes_the_row() {
        let mut app = picker_app("p8-picker");
        let directory = test_directory("p8-picker-files");
        let path = directory.join("contratto.pdf");
        let bytes = b"%PDF-1.7 esatto";
        std::fs::write(&path, bytes).expect("fixture file");
        let picker = crate::test_support::RecordingFilePicker::new();
        picker.set_paths(vec![path]);
        app.file_picker = picker.clone();

        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.compose_new();
        app.composer.to = "Giulia <giulia@example.com>".to_owned();
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let attach = probe::composer_attach_rect(&ctx).expect("the Allega file button");
        let card = probe::composer_card_rect(&ctx).expect("the composer card");
        assert_eq!(probe::composer_attach_enabled(&ctx), Some(true), "a store-backed composer must allow attaching");
        assert!(card.contains_rect(attach), "the attach button is outside the card: {attach:?} in {card:?}");
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        click_settled(&ctx, &mut app, attach.center(), Vec2::new(980.0, 600.0));
        assert!(probe::composer_attach_click_count(&ctx) > 0, "the click never reached the attach button");

        assert_eq!(picker.calls(), 1, "attach rect {attach:?} in card {card:?}");
        let draft = app.composer.draft_id.clone().expect("the picker must create the draft");
        assert_eq!(app.composer.attachments.len(), 1);
        assert_eq!(app.composer.attachments[0].filename, "contratto.pdf");
        assert_eq!(app.engine.attachment_bytes(&app.composer.attachments[0].id).expect("stored bytes"), bytes);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("1 allegato")), "the total is missing: {text:?}");

        let remove = probe::composer_attachment_remove_rect(&ctx, &app.composer.attachments[0].id).expect("the remove button");
        click_settled(&ctx, &mut app, remove.center(), Vec2::new(980.0, 600.0));
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        assert!(app.composer.attachments.is_empty(), "the row was not removed");
        assert!(app.engine.message(&draft).expect("draft").attachments.is_empty());

        let before = app.composer.attachments.len();
        picker.cancel();
        app.pick_composer_files();
        assert_eq!(app.composer.attachments.len(), before, "a cancelled picker adds nothing");
        assert_eq!(picker.calls(), 2, "the picker was not asked a second time");
    }

    #[test]
    fn a_failed_or_missing_pick_is_one_readable_toast_and_writes_no_row() {
        let mut app = picker_app("p8-picker-fail");
        let directory = test_directory("p8-picker-missing");
        let missing = directory.join("non-esiste.pdf");
        let picker = crate::test_support::RecordingFilePicker::new();
        picker.set_paths(vec![missing]);
        app.file_picker = picker;
        app.compose_new();
        app.composer.to = "Giulia <giulia@example.com>".to_owned();
        let before = app.composer.attachments.len();
        app.pick_composer_files();
        assert_eq!(app.composer.attachments.len(), before);
        let toast = app.toast.as_ref().map(|toast| toast.message.clone()).expect("one toast");
        assert!(toast.contains("non-esiste.pdf"), "the toast must name the file: {toast}");
        assert!(!toast.contains("p8-picker-missing"), "the toast must not leak the path: {toast}");
    }

    #[test]
    fn dropping_files_on_the_composer_uses_the_same_bounded_path() {
        let mut app = picker_app("p8-drop");
        let directory = test_directory("p8-drop-files");
        let path = directory.join("nota.txt");
        std::fs::write(&path, b"drop bytes").expect("fixture file");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.compose_new();
        app.composer.to = "Giulia <giulia@example.com>".to_owned();
        let size = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, size, vec![]);
        let card = probe::composer_card_rect(&ctx).expect("the composer card");
        frame_with_drop(&ctx, &mut app, size, card.center(), vec![path]);
        frame_at(&ctx, &mut app, size, vec![]);
        assert_eq!(app.composer.attachments.len(), 1, "the dropped file was not attached");
        assert_eq!(app.composer.attachments[0].filename, "nota.txt");
        assert_eq!(app.engine.attachment_bytes(&app.composer.attachments[0].id).expect("bytes"), b"drop bytes");
    }

    #[test]
    fn forwarding_copies_attachment_bytes_and_a_missing_blob_is_reported_and_blocks_send() {
        let mut app = booted_app("p8-forward", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            store
                .replace_attachments(
                    "msg_acc_a_1",
                    &[postino_store::Attachment {
                        id: "att_parent".to_owned(),
                        filename: "preventivo.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 0,
                        blob_hash: None,
                    }],
                )
                .expect("attachment row");
        });
        app.engine.store_attachment_bytes("msg_acc_a_1", "att_parent", b"byte del forward").expect("source bytes");
        app.reply_to("msg_acc_a_1", ComposeMode::Forward);
        app.composer.touch(0.0);
        assert!(app.save_composer(false), "the forward draft must save");
        let draft = app.composer.draft_id.clone().expect("forward draft");
        let attachments = app.engine.message(&draft).expect("draft").attachments.clone();
        assert_eq!(attachments.len(), 1, "the forwarded attachment was dropped");
        assert_eq!(app.engine.attachment_bytes(&attachments[0].id).expect("copied bytes"), b"byte del forward");
        assert_eq!(attachments[0].filename, "preventivo.pdf");

        let second = booted_app("p8-forward-missing", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            store
                .replace_attachments(
                    "msg_acc_a_1",
                    &[postino_store::Attachment {
                        id: "att_parent".to_owned(),
                        filename: "manca.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 4,
                        blob_hash: None,
                    }],
                )
                .expect("attachment row");
        });
        let mut second = second;
        second.reply_to("msg_acc_a_1", ComposeMode::Forward);
        second.composer.touch(0.0);
        assert!(second.save_composer(false));
        let draft = second.composer.draft_id.clone().expect("forward draft");
        let notice = second.composer.notice.clone().expect("the failed copy is reported");
        assert!(notice.contains("manca.pdf"), "{notice}");
        // The row is in the composer; the next save (what Send does first) persists it and keeps
        // the send blocked on the missing bytes.
        second.composer.to = "Giulia <giulia@example.com>".to_owned();
        second.composer.touch(0.0);
        assert!(second.save_composer(false));
        assert_eq!(second.engine.message(&draft).expect("draft").attachments.len(), 1, "the failed row stays visible");
        let error = second.engine.call("message_send", json!({ "draft": draft })).expect_err("send must refuse");
        assert!(error.to_string().contains("manca.pdf"), "{error}");
    }

    #[test]
    fn a_pristine_reply_sends_from_a_real_click_and_saves_before_close() {
        let ca = postino_testkit::TlsFixture::generate().ca();
        let mut app = smtp_app("p8-pristine-reply", 1, ca);
        let source = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Origine".to_owned(),
                    body_text: "Corpo originale".to_owned(),
                    attachments: Vec::new(),
                    in_reply_to: None,
                },
            )
            .expect("source draft");
        app.reply_to(&source.summary.id, ComposeMode::Reply);
        assert!(app.composer.dirty, "a prefill must be content even before the first keystroke");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, size, vec![]);
        let send = probe::composer_send_rect(&ctx).expect("the Send button");
        click_settled(&ctx, &mut app, send.center(), size);
        assert!(!app.composer.open, "Send did not close the composer: {:?}", app.composer.save_error);
        let queued = app.engine.outbox_list(None, 500, 0).expect("outbox");
        assert_eq!(queued.entries.len(), 1, "an unedited reply did not create an outbox row");

        let second = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Seconda origine".to_owned(),
                    body_text: "Corpo".to_owned(),
                    attachments: Vec::new(),
                    in_reply_to: None,
                },
            )
            .expect("second source");
        app.reply_to(&second.summary.id, ComposeMode::Reply);
        assert!(app.save_composer(true), "Salva bozza must create the pristine reply");
        assert!(app.composer.draft_id.is_some());
        assert!(app.engine.messages().iter().any(|message| message.subject == "Re: Seconda origine"));

        let third = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Terza origine".to_owned(),
                    body_text: "Corpo".to_owned(),
                    attachments: Vec::new(),
                    in_reply_to: None,
                },
            )
            .expect("third source");
        app.reply_to(&third.summary.id, ComposeMode::Reply);
        frame_at(&ctx, &mut app, size, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.composer.open, "Esc did not close the reply");
        assert!(app.engine.messages().iter().any(|message| message.subject == "Re: Terza origine"), "Esc discarded the reply");
    }

    #[test]
    fn typing_then_attaching_or_removing_does_not_lose_the_body() {
        let ca = postino_testkit::TlsFixture::generate().ca();
        let mut app = smtp_app("p8-dirty-attach", 1, ca);
        let source = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Bozza da conservare".to_owned(),
                    body_text: "corpo vecchio".to_owned(),
                    attachments: Vec::new(),
                    in_reply_to: None,
                },
            )
            .expect("source draft");
        app.edit_draft(&source.summary.id);
        app.composer.body = "NUOVO TESTO".to_owned();
        app.composer.touch(1.0);
        let directory = test_directory("p8-dirty-attach-files");
        let path = directory.join("nota.txt");
        std::fs::write(&path, b"allegato").expect("fixture");
        app.attach_composer_files(vec![path]);
        assert!(app.composer.dirty, "attaching must not claim the unsaved body is saved");
        let attachment = app.composer.attachments[0].id.clone();
        app.remove_composer_attachment(&attachment);
        assert!(app.composer.dirty, "removing must not claim the unsaved body is saved");
        app.send_composer();
        assert!(!app.composer.open, "send failed: {:?}", app.composer.save_error);
        assert_eq!(app.engine.message(&source.summary.id).expect("draft").body_text, "NUOVO TESTO");
        assert_eq!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.len(), 1);

        let second = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Bozza chiusa".to_owned(),
                    body_text: "corpo vecchio".to_owned(),
                    attachments: Vec::new(),
                    in_reply_to: None,
                },
            )
            .expect("second draft");
        app.edit_draft(&second.summary.id);
        app.composer.body = "TESTO PRIMA DELLA CHIUSURA".to_owned();
        app.composer.touch(2.0);
        let path = directory.join("seconda.txt");
        std::fs::write(&path, b"secondo").expect("fixture");
        app.attach_composer_files(vec![path]);
        app.close_composer();
        assert_eq!(app.engine.message(&second.summary.id).expect("closed draft").body_text, "TESTO PRIMA DELLA CHIUSURA");
    }

    #[test]
    fn a_failed_forward_copy_blocks_send_until_the_row_is_removed() {
        let ca = postino_testkit::TlsFixture::generate().ca();
        let mut app = smtp_app("p8-forward-block", 1, ca);
        let source = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Con allegato mancante".to_owned(),
                    body_text: "Corpo".to_owned(),
                    attachments: vec![postino_core::Attachment {
                        id: "source_missing".to_owned(),
                        filename: "manca.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 4,
                    }],
                    in_reply_to: None,
                },
            )
            .expect("source draft");
        app.reply_to(&source.summary.id, ComposeMode::Forward);
        app.composer.to = "Giulia <giulia@example.com>".to_owned();
        app.send_composer();
        assert!(app.composer.open, "a failed forward copy must keep the composer open");
        let failure = app.composer.attachment_failure.clone().expect("a blocking failure sentence");
        assert!(failure.contains("manca.pdf"), "{failure}");
        assert!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.is_empty(), "the failed attachment was silently dropped");
        assert_eq!(app.composer.failed_attachment_ids.len(), 1);

        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, size, vec![]);
        let failed_id = app.composer.failed_attachment_ids[0].clone();
        let remove = probe::composer_attachment_remove_rect(&ctx, &failed_id).expect("the failed row remove button");
        let remove_id = probe::composer_attachment_remove_widget_id(&ctx, &failed_id).expect("the remove widget id");
        let card = probe::composer_card_rect(&ctx).expect("card");
        assert!(card.contains_rect(remove), "failed remove outside card: {remove:?} in {card:?}");
        ctx.memory_mut(|memory| memory.request_focus(remove_id));
        frame_at(&ctx, &mut app, size, vec![]);
        assert!(ctx.memory(|memory| memory.has_focus(remove_id)), "the failed row remove button cannot take keyboard focus");
        frame_at(&ctx, &mut app, size, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(probe::composer_attachment_remove_was_clicked(&ctx, &failed_id), "Enter on the remove button did not reach it");
        assert!(app.composer.attachment_failure.is_none(), "removing the failed row did not unblock send (rect {remove:?})");
        app.composer.to = "Giulia <giulia@example.com>".to_owned();
        app.send_composer();
        assert!(!app.composer.open, "send after removal failed: {:?}", app.composer.save_error);
        assert_eq!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.len(), 1);
    }

    #[test]
    fn a_missing_attachment_send_error_names_the_file_and_the_good_one_stays() {
        let ca = postino_testkit::TlsFixture::generate().ca();
        let mut app = smtp_app("p8-send-name", 1, ca);
        let source = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Due allegati".to_owned(),
                    body_text: "Corpo".to_owned(),
                    attachments: vec![postino_core::Attachment {
                        id: "missing".to_owned(),
                        filename: "manca.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 4,
                    }],
                    in_reply_to: None,
                },
            )
            .expect("draft with a missing row");
        app.engine.add_draft_attachment(&source.summary.id, "buono.txt", None, b"ok").expect("good blob");
        app.edit_draft(&source.summary.id);
        app.send_composer();
        let error = app.composer.save_error.clone().expect("a readable send refusal");
        assert!(error.contains("manca.pdf"), "the filename is missing from the refusal: {error}");
        assert!(error.contains("mancante o illeggibile"), "{error}");
        assert_eq!(app.engine.message(&source.summary.id).expect("draft").attachments.len(), 2, "the good attachment was dropped");
        assert!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.is_empty());
    }

    #[test]
    fn a_drop_on_the_attachment_strip_is_added_once() {
        let mut app = picker_app("p8-drop-strip");
        let directory = test_directory("p8-drop-strip-files");
        let path = directory.join("striscia.txt");
        std::fs::write(&path, b"una volta").expect("fixture");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.compose_new();
        app.composer.to = "Giulia <giulia@example.com>".to_owned();
        let size = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, size, vec![]);
        let card = probe::composer_card_rect(&ctx).expect("composer card");
        frame_with_drop(&ctx, &mut app, size, pos2(card.center().x, card.max.y - 60.0), vec![path]);
        frame_at(&ctx, &mut app, size, vec![]);
        assert_eq!(app.composer.attachments.len(), 1, "a drop over the strip was added twice");
        assert_eq!(app.composer.attachments[0].filename, "striscia.txt");
    }

    #[test]
    fn a_large_reply_subject_or_body_has_a_readable_save_refusal() {
        let mut app = picker_app("p8-large-reply");
        let source = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "x".repeat(998),
                    body_text: "Corpo".to_owned(),
                    attachments: Vec::new(),
                    in_reply_to: None,
                },
            )
            .expect("source");
        app.reply_to(&source.summary.id, ComposeMode::Reply);
        app.composer.touch(1.0);
        assert!(!app.save_composer(true), "the oversized Re: subject must be refused");
        let error = app.composer.save_error.clone().expect("readable error");
        assert!(error.contains("998"), "{error}");
        assert!(!error.contains("Non è stato possibile salvare la bozza. Controlla"), "a generic error hid the limit: {error}");
        assert!(draft_save_error_message(&None, "invalid arguments: body_text is larger than 1048576 bytes").contains("1 MiB"));
    }

    #[test]
    fn a_demo_draft_attachment_can_be_removed_locally_without_a_store() {
        let mut app = PostinoApp::demo(true);
        let draft = app
            .engine
            .save_draft(
                None,
                None,
                postino_core::DraftContent {
                    to: vec![postino_core::Address::new("Giulia", "giulia.ferri@example.com")],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    subject: "Bozza demo".to_owned(),
                    body_text: "Corpo".to_owned(),
                    attachments: vec![postino_core::Attachment {
                        id: "att_demo".to_owned(),
                        filename: "demo.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 4,
                    }],
                    in_reply_to: None,
                },
            )
            .expect("demo draft");
        app.edit_draft(&draft.summary.id);
        app.remove_composer_attachment("att_demo");
        assert!(app.composer.attachments.is_empty(), "the demo remove button did nothing");
        assert!(app.composer.dirty);
        assert!(app.save_composer(false));
        assert!(app.engine.message(&draft.summary.id).expect("draft").attachments.is_empty());
    }

    #[test]
    fn the_attach_button_is_reachable_from_the_keyboard() {
        let mut app = picker_app("p8-attach-keyboard");
        let directory = test_directory("p8-attach-keyboard-files");
        let path = directory.join("tastiera.txt");
        std::fs::write(&path, b"keyboard bytes").expect("fixture");
        let picker = crate::test_support::RecordingFilePicker::new();
        picker.set_paths(vec![path]);
        app.file_picker = picker.clone();
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.compose_new();
        app.composer.to = "Giulia <giulia@example.com>".to_owned();
        let size = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, size, vec![]);
        let widget = probe::composer_attach_widget_id(&ctx).expect("the attach widget id");
        ctx.memory_mut(|memory| memory.request_focus(widget));
        frame_at(&ctx, &mut app, size, vec![]);
        assert!(ctx.memory(|memory| memory.has_focus(widget)), "the attach button did not take keyboard focus");
        frame_at(&ctx, &mut app, size, vec![key(Key::Enter, Modifiers::NONE)]);
        frame_at(&ctx, &mut app, size, vec![]);
        assert_eq!(picker.calls(), 1, "Enter on the focused attach button did not open the picker");
        assert_eq!(app.composer.attachments.len(), 1);
        assert_eq!(app.engine.attachment_bytes(&app.composer.attachments[0].id).expect("bytes"), b"keyboard bytes");
    }

    #[test]
    fn the_signature_editor_saves_per_account_and_escape_returns_to_the_account_row() {
        let mut app = booted_app("p8-signature", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.open_settings(&ctx, None);
        let size = Vec2::new(980.0, 600.0);
        settle_at(&ctx, &mut app, size);
        let open = probe::signature_open_button_rect(&ctx, "acc_b").expect("the Firma button");
        click_at(&ctx, &mut app, open.center(), size);
        settle_at(&ctx, &mut app, size);
        assert!(
            probe::signature_open_click_count(&ctx) > 0,
            "the Firma button did not receive the click (rect {open:?}, remove rect {:?}, hover {:?}, remove state {:?}, settings {:?})",
            probe::account_remove_button_rect(&ctx, "acc_b"),
            probe::signature_open_hover_state(&ctx, "acc_b"),
            app.account_remove.is_some(),
            probe::settings_sections(&ctx)
        );
        assert_eq!(app.signature_editor.as_ref().map(|editor| editor.account.as_str()), Some("acc_b"));
        let editor_rect = probe::signature_editor_rect(&ctx, "acc_b").expect("the signature text area");
        let appearance = probe::settings_sections(&ctx).get("ASPETTO").copied().expect("the appearance section");
        assert!(editor_rect.max.y <= appearance.min.y + 1.0, "the signature editor crosses into ASPETTO: {editor_rect:?} vs {appearance:?}");
        assert!(probe::signature_cancel_rect(&ctx, "acc_b").is_some(), "the cancel button is missing");
        if let Some(editor) = app.signature_editor.as_mut() {
            editor.text = "Firma B\nb@example.com".to_owned();
        }
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let save = probe::signature_save_rect(&ctx, "acc_b").expect("Salva firma");
        click_at(&ctx, &mut app, save.center(), size);
        settle_at(&ctx, &mut app, size);
        assert!(app.signature_editor.is_none(), "the editor did not close after Save");
        assert_eq!(
            app.engine.accounts().iter().find(|account| account.id == "acc_b").map(|account| account.signature.as_str()),
            Some("Firma B\nb@example.com")
        );
        assert_eq!(
            app.engine.accounts().iter().find(|account| account.id == "acc_a").map(|account| account.signature.as_str()),
            Some(""),
            "the other account was changed"
        );

        app.switch_account("acc_b".to_owned());
        app.compose_new();
        assert!(app.composer.body.contains("Firma B"), "the new message did not use the saved signature: {}", app.composer.body);

        app.composer = ComposerState::closed();
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let open = probe::signature_open_button_rect(&ctx, "acc_a").expect("the Firma button");
        let open_id = probe::signature_open_button_widget_id(&ctx, "acc_a").expect("the Firma widget id");
        click_at(&ctx, &mut app, open.center(), size);
        settle_at(&ctx, &mut app, size);
        assert!(app.signature_editor.is_some());
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.signature_editor.is_none(), "Escape must close the editor");
        assert!(app.show_accounts_dialog, "Escape must not close Settings with the editor open");
        assert_eq!(app.signature_focus_account, None, "the focus request must be spent on the account button");
        assert!(ctx.memory(|memory| memory.has_focus(open_id)), "Escape did not return focus to the Firma button");

        // Closing Settings discards an unsaved editor instead of showing stale text on reopen.
        let open = probe::signature_open_button_rect(&ctx, "acc_a").expect("the Firma button");
        click_at(&ctx, &mut app, open.center(), size);
        settle_at(&ctx, &mut app, size);
        if let Some(editor) = app.signature_editor.as_mut() {
            editor.text = "testo mai salvato".to_owned();
        }
        app.close_settings(&ctx);
        assert!(app.signature_editor.is_none());
        app.open_settings(&ctx, None);
        assert!(app.signature_editor.is_none(), "closing Settings kept the unsaved editor text");
    }

    #[test]
    fn composer_attachments_and_the_signature_editor_render_at_980_light_and_dark() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            let mut app = PostinoApp::demo_at(true, screenshot_clock().expect("clock"));
            app.dark = dark;
            let mut composer = ComposerState::new(ComposeMode::New);
            composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
            composer.subject = "Allegati".to_owned();
            composer.attachments = vec![
                Attachment {
                    id: "att_p8_1".to_owned(),
                    filename: "preventivo-postino.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 248_320,
                },
                Attachment {
                    id: "att_p8_2".to_owned(),
                    filename: "planimetria-ufficio.png".to_owned(),
                    mime: "image/png".to_owned(),
                    size: 1_048_576,
                },
            ];
            app.composer = composer;
            frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
            let card = probe::composer_card_rect(&ctx).expect("the composer card");
            let send = probe::composer_send_rect(&ctx).expect("the Send button");
            assert!(card.contains_rect(send), "Send is outside the composer at 980x600: {send:?} in {card:?}");
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(
                text.iter().any(|line| line.contains("2 allegati")),
                "the total is missing in {} mode: {text:?}",
                if dark { "dark" } else { "light" }
            );
            assert!(text.iter().any(|line| line.contains("preventivo-postino.pdf")), "the first attachment is missing: {text:?}");

            app.composer = ComposerState::closed();
            app.seed_screenshot(ScreenshotState::SettingsSignature);
            frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(text.iter().any(|line| line.contains("Firma")), "the signature editor is missing: {text:?}");
            assert!(text.iter().any(|line| line.contains("byte")), "the byte count is missing: {text:?}");
        }
    }

    #[test]
    fn compose_opens_with_the_signature_and_renders_headlessly() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.compose_new();
        assert!(app.composer.open);
        assert_eq!(app.composer.mode, ComposeMode::New);
        assert!(app.composer.body.contains("Federico"));
        assert_eq!(app.composer.focus, Some(ComposerFocus::To));
        frame(&ctx, &mut app, vec![]);
        assert!(app.composer.open);
    }

    #[test]
    fn c_opens_the_composer_and_escape_keeps_the_draft() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![key(Key::C, Modifiers::NONE)]);
        assert!(app.composer.open, "C opens the composer");
        app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        app.composer.subject = "Bozza da tastiera".to_owned();
        app.composer.body = "Corpo".to_owned();
        app.composer.touch(0.0);
        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.composer.open, "Esc closes the composer");
        assert!(app.engine.messages().iter().any(|message| message.subject == "Bozza da tastiera"), "Esc must save the draft before closing");
    }

    #[test]
    fn ctrl_enter_in_the_demo_does_not_claim_a_submission() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.compose_new();
        app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        app.composer.subject = "Invio da tastiera".to_owned();
        app.composer.body = "Corpo".to_owned();
        app.composer.touch(0.0);
        frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::Enter, Modifiers::COMMAND)]);
        assert!(app.composer.open, "the demo must keep the composer open instead of pretending the message left");
        // The shortcut reads the same enabled rule as the button and shows the reason instead of
        // doing nothing. There is no save error, because no submission was attempted.
        assert!(app.composer.save_error.is_none(), "the shortcut attempted a disabled send: {:?}", app.composer.save_error);
        assert_eq!(app.composer.notice.as_deref(), Some(DEMO_SEND_REASON));
        let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        assert!(text.iter().any(|line| line.contains("La modalità demo non invia")), "the refusal is not visible: {text:?}");
        assert!(text.iter().any(|line| line.contains("Invio non disponibile")), "the status bar still promises Ctrl+Invio: {text:?}");
        assert!(
            !app.engine.messages().iter().any(|message| message.subject == "Invio da tastiera" && message.folder_id == "sent"),
            "the demo moved the message to Sent without a submission"
        );
    }

    #[test]
    fn ctrl_enter_without_smtp_names_the_missing_server() {
        let mut app = booted_app("send-no-smtp-notice", Vec::new(), |store| {
            seed_account_rows(store, "acc_no_smtp", "no-smtp@example.com", None);
        });
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.compose_new();
        app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        app.composer.subject = "Senza SMTP".to_owned();
        app.send_composer();
        assert_eq!(app.composer.notice.as_deref(), Some(NO_SMTP_SEND_REASON));
        let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        assert!(text.iter().any(|line| line.contains("Questo account non ha un server SMTP")), "{text:?}");
    }

    #[test]
    fn reply_and_forward_prefill_the_composer() {
        let mut app = PostinoApp::demo(true);
        let with_attachment = app
            .engine
            .messages()
            .iter()
            .find(|message| message.has_attachments)
            .map(|message| message.id.clone())
            .expect("a demo message with an attachment");

        app.reply_to(&with_attachment, ComposeMode::Reply);
        assert!(app.composer.open);
        assert_eq!(app.composer.mode, ComposeMode::Reply);
        assert!(app.composer.to.contains("giulia.ferri@example.com"));
        assert!(app.composer.subject.starts_with("Re:"));
        assert!(app.composer.body.contains("ha scritto:"));
        assert!(app.composer.attachments.is_empty(), "reply does not copy the original attachments");

        app.composer = ComposerState::closed();
        app.reply_to(&with_attachment, ComposeMode::Forward);
        assert_eq!(app.composer.mode, ComposeMode::Forward);
        assert!(app.composer.to.is_empty());
        assert!(app.composer.subject.starts_with("Fwd:"));
        assert_eq!(app.composer.attachments.len(), 1, "forward copies the attachment metadata");
    }

    #[test]
    fn an_invalid_recipient_blocks_saving_and_keeps_the_composer_open() {
        let mut app = PostinoApp::demo(true);
        app.compose_new();
        app.composer.to = "Giulia <non-una-mail>".to_owned();
        app.composer.subject = "Non salvabile".to_owned();
        app.composer.touch(1.0);
        assert!(!app.save_composer(true));
        assert!(app.composer.open);
        assert!(app.composer.save_error.is_some(), "the composer explains why it did not save");
        assert!(!app.engine.messages().iter().any(|message| message.subject == "Non salvabile"));
    }

    #[test]
    fn autosave_updates_one_draft_in_place() {
        let mut app = PostinoApp::demo(true);
        let before = app.engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        app.compose_new();
        app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        app.composer.subject = "Bozza di prova".to_owned();
        app.composer.body = "Prima versione".to_owned();
        app.composer.touch(1.0);
        assert!(app.save_composer(false));
        let id = app.composer.draft_id.clone().expect("draft id");
        app.composer.body = "Seconda versione".to_owned();
        app.composer.touch(2.0);
        assert!(app.save_composer(false));
        assert_eq!(app.composer.draft_id.as_deref(), Some(id.as_str()));
        assert_eq!(app.engine.message(&id).map(|message| message.body_text), Some("Seconda versione".to_owned()));
        let after = app.engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        assert_eq!(after, before + 1, "autosave must not duplicate the draft");
    }

    #[test]
    fn closing_keeps_the_draft_and_the_demo_send_is_refused_honestly() {
        let mut app = PostinoApp::demo(true);
        app.compose_new();
        app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        app.composer.subject = "Invio demo".to_owned();
        app.composer.body = "Corpo".to_owned();
        app.composer.touch(1.0);
        app.close_composer();
        assert!(!app.composer.open);
        let id = app.engine.messages().iter().find(|message| message.subject == "Invio demo").map(|message| message.id.clone()).expect("saved draft");
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.clone()), Some("drafts".to_owned()));

        app.edit_draft(&id);
        app.send_composer();
        assert!(app.composer.open, "the demo send must not close the composer as if the message left");
        assert!(app.composer.save_error.is_none(), "the demo attempted a submission the button refuses: {:?}", app.composer.save_error);
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.clone()), Some("drafts".to_owned()));
    }

    #[test]
    fn reply_all_excludes_the_account_address() {
        let mut app = PostinoApp::demo(true);
        let sent = app
            .engine
            .messages()
            .iter()
            .find(|message| message.folder_id == "sent" && message.to.len() > 1)
            .map(|message| message.id.clone())
            .expect("a sent message with several recipients");
        app.reply_to(&sent, ComposeMode::ReplyAll);
        assert!(app.composer.to.contains("marco.bianchi@example.com"));
        assert!(app.composer.to.contains("giulia.ferri@example.com"));
        assert!(!app.composer.to.contains("federico@example.it"));
        assert!(!app.composer.cc.contains("federico@example.it"));
    }

    #[test]
    fn palette_arrows_and_enter_run_the_selected_command() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();
        app.focus_message(rows[0].id.clone());

        frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::K, Modifiers::COMMAND)]);
        assert!(app.show_palette, "Ctrl+K opens the palette");

        frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::NONE), key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(app.palette_selected, 1, "ArrowDown moves the palette selection");

        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(!app.show_palette, "Enter runs the selected command and closes the palette");
        assert!(app.reading_open && !app.reading_full, "the second command is 'Apri messaggio'");
    }

    #[test]
    fn palette_explains_a_disabled_selection_and_enter_is_not_silent() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.show_palette = true;
        app.palette_query = "rinomina".to_owned();
        app.palette_selected = 0;
        frame(&ctx, &mut app, vec![]);
        let rename = command("label_rename").expect("label_rename is registered");
        assert!(!app.command_enabled(rename));
        assert_eq!(rename.rule.reason(), Some("Seleziona prima un'etichetta"), "the palette can paint a visible reason");

        // The line under the list is a real painted galley, not only a hover tooltip.
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        let painted = output.shapes.iter().any(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => text.galley.text().contains("Seleziona prima un'etichetta"),
            _ => false,
        });
        output.textures_delta.clear();
        assert!(painted, "the disabled reason must be visible under the palette list");

        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.show_palette, "Enter on a disabled command must not close the palette");
        assert!(app.status.contains("Seleziona prima un'etichetta"), "Enter must explain the refusal: {}", app.status);
    }

    #[test]
    fn archiving_closes_the_pane_and_moves_the_message() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages()[0].id.clone();
        app.open_message(id.clone());
        app.move_selected(FolderKind::Archive, "archiviato", "archiviati");
        assert!(app.selected.is_none());
        assert!(!app.reading_open);
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.clone()), Some("archive".to_owned()));
    }

    #[test]
    fn palette_filters_commands_and_keeps_every_row() {
        let all = filtered_commands("");
        assert!(all.len() > 4, "the palette should list more than the first command");
        let search = filtered_commands("stella");
        assert!(search.iter().any(|command| command.id == "star"));
        assert!(search.iter().all(|command| command.label.to_lowercase().contains("stella")));
    }

    #[test]
    fn the_compact_date_fits_its_column() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(400.0, 200.0))), ..Default::default() };
        let mut width = 0.0;
        let mut output = ctx.run_ui(raw, |ui| {
            let galley = ui.painter().layout_no_wrap("24/10/26".to_owned(), FontId::new(type_size::CAPTION, FontFamily::Monospace), Color32::WHITE);
            width = galley.size().x;
        });
        output.textures_delta.clear();
        assert!(width <= ui_size::LIST_DATE, "dd/MM/yy measures {width} pt, more than the {} pt column", ui_size::LIST_DATE);
    }

    #[test]
    fn screenshot_states_parse_and_reject_unknown_labels() {
        for (name, state) in [
            ("shell", ScreenshotState::Shell),
            ("reading", ScreenshotState::Reading),
            ("reading_full", ScreenshotState::ReadingFull),
            ("compose", ScreenshotState::Compose),
            ("palette", ScreenshotState::Palette),
            ("labels", ScreenshotState::Labels),
            ("label_menu", ScreenshotState::LabelMenu),
            ("label_editor", ScreenshotState::LabelEditor),
            ("empty_unread", ScreenshotState::EmptyUnread),
            ("empty_attachments", ScreenshotState::EmptyAttachments),
            ("empty_label_unread", ScreenshotState::EmptyLabelUnread),
            ("onboarding_welcome", ScreenshotState::OnboardingWelcome),
            ("onboarding_provider", ScreenshotState::OnboardingProvider),
            ("onboarding_form", ScreenshotState::OnboardingForm),
            ("onboarding_error", ScreenshotState::OnboardingError),
            ("onboarding_folders", ScreenshotState::OnboardingFolders),
            ("onboarding_sync", ScreenshotState::OnboardingSync),
            ("mailbox", ScreenshotState::Mailbox),
            ("offline", ScreenshotState::Offline),
            ("reading_attachments", ScreenshotState::ReadingAttachments),
            ("link_confirm", ScreenshotState::LinkConfirm),
            ("settings", ScreenshotState::Settings),
            ("settings_signature", ScreenshotState::SettingsSignature),
            ("outbox", ScreenshotState::Outbox),
            ("search", ScreenshotState::Search),
            ("search_error", ScreenshotState::SearchError),
            ("rules", ScreenshotState::Rules),
            ("rule_editor", ScreenshotState::RuleEditor),
            ("rule_preview", ScreenshotState::RulePreview),
        ] {
            assert_eq!(ScreenshotState::parse(name), Ok(state), "{name} must be a known scene");
        }
        let error = ScreenshotState::parse("nope").expect_err("nope is not a scene");
        assert!(error.contains("unknown screenshot state"), "{error}");
        assert!(error.contains("empty_label_unread"), "{error}");
    }

    #[test]
    fn the_screenshot_clock_is_a_fixed_instant() {
        let frozen = screenshot_clock().expect("the screenshot clock parses");
        assert_eq!(frozen.format("%Y-%m-%dT%H:%M:%SZ").to_string(), SCREENSHOT_CLOCK);
        let app = PostinoApp::demo_at(true, frozen);
        assert_eq!(app.now_utc, frozen);
        assert_eq!(app.engine.messages()[0].date, frozen - chrono::Duration::minutes(8), "demo dates must follow the seam");
    }

    #[test]
    fn frozen_clock_formats_dates_without_the_wall_clock() {
        let frozen = screenshot_clock().expect("the screenshot clock parses");
        let date = DateTime::parse_from_rfc3339("2026-10-24T09:30:00Z").expect("the fixture date parses").with_timezone(&Utc);
        let first = format_when(date, frozen);
        // Real time moving on must not change what the same message renders as.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(format_when(date, frozen), first, "the real clock must not leak into a screenshot date");
        let same_day = format_when(date, date + chrono::Duration::minutes(5));
        assert_eq!(same_day, date.with_timezone(&Local).format("%H:%M").to_string(), "a same-day instant keeps the time format");
        assert_ne!(format_when(date, date + chrono::Duration::days(400)), first, "another year shows the full date");
    }

    #[test]
    fn screenshot_mode_never_advances_the_clock() {
        let frozen = screenshot_clock().expect("the screenshot clock parses");
        let mut app = PostinoApp::demo_at(true, frozen);
        app.screenshot = Some(ScreenshotMode { out: PathBuf::from("unused.png"), frames: 0, requested: false, waited: 0 });
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        frame(&ctx, &mut app, Vec::new());
        assert_eq!(app.now_utc, frozen, "a screenshot must render against its own clock, not the wall clock");
    }

    #[test]
    fn png_writer_round_trips_a_two_by_two_image() {
        let pixels = [Color32::from_rgb(23, 61, 50), Color32::from_rgb(245, 235, 214), Color32::from_rgb(228, 185, 106), Color32::from_rgb(0, 0, 0)];
        let image = egui::ColorImage::new([2, 2], pixels.to_vec());
        let directory = std::env::temp_dir().join("postino-app-p01-tests");
        std::fs::create_dir_all(&directory).expect("a private temp folder");
        let path = directory.join(format!("png-round-trip-{}.png", std::process::id()));
        write_png(&path, &image).expect("the PNG writer");

        let file = std::fs::File::open(&path).expect("the written PNG");
        let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().expect("a PNG reader");
        let mut buffer = vec![0; reader.output_buffer_size().expect("an RGBA buffer")];
        let info = reader.next_frame(&mut buffer).expect("the PNG frame");
        assert_eq!((info.width, info.height), (2, 2));
        let expected = pixels.iter().flat_map(|pixel| pixel.to_array()).collect::<Vec<_>>();
        assert_eq!(&buffer[..info.buffer_size()], &expected[..]);
        assert!(!PathBuf::from(format!("{}.tmp", path.display())).exists(), "the atomic write must remove its temp file");
        std::fs::remove_file(&path).expect("remove the test image");
    }

    #[test]
    fn unread_header_is_singular_for_one() {
        assert_eq!(unread_text(1), "1 non letto");
        assert_eq!(unread_text(0), "0 non letti");
        assert_eq!(unread_text(3), "3 non letti");
        assert_eq!(messages_text(1), "1 messaggio");
        assert_eq!(messages_text(6), "6 messaggi");
    }

    #[test]
    fn label_errors_are_italian_ui_copy() {
        assert_eq!(
            label_error_message("invalid arguments: a label named \"lavoro\" already exists; label names are unique ignoring case"),
            "Esiste già un'etichetta con questo nome."
        );
        assert_eq!(label_error_message("invalid arguments: label names are 1-48 characters"), "Il nome deve avere da 1 a 48 caratteri.");
        assert!(!label_error_message("failed for an unknown reason").contains("failed"));
    }

    #[test]
    fn responsive_breakpoints_keep_the_list_usable() {
        let pane = reading_pane_width(1280.0);
        assert!((pane - (1280.0 - ui_size::SIDEBAR) * 0.52).abs() < 0.01, "the pane opens at 52% of the width left by the sidebar");
        assert!(1280.0 - ui_size::SIDEBAR - pane >= ui_size::LIST_MIN, "the list keeps its 460 pt floor");
        assert!(
            1280.0 - ui_size::SIDEBAR - reading_pane_max_width(1280.0) >= ui_size::LIST_MIN,
            "even a full drag of the pane cannot squeeze the list below 460"
        );
        assert!(pane < 1280.0 * 0.52, "the old window-relative 52% left a 374 pt list");
        assert_eq!(reading_pane_width(800.0), READING_PANE_MIN_SIZE, "it never goes below 420");
        assert!(reading_pane_width(1280.0) <= (1280.0 - ui_size::SIDEBAR) * READING_PANE_MAX_RATIO);
        assert!(reading_overlay(980.0), "980x600 must use the overlay");
        assert!(!reading_overlay(1000.0), "1000 px switches to the side pane");
        assert!(!show_list_avatars(719.0));
        assert!(show_list_avatars(720.0));
        // The compact date stays until the panel itself is below 480 pt.
        assert_eq!(list_columns(479.0).date, None, "below 480 the date column is dropped before the subject");
        assert_eq!(list_columns(479.0).sender, ui_size::LIST_SENDER_NARROW);
        assert_eq!(list_columns(480.0).date, Some(ui_size::LIST_DATE));
        assert_eq!(list_columns(559.0).sender, ui_size::LIST_SENDER_NARROW);
        assert_eq!(list_columns(560.0).sender, ui_size::LIST_SENDER);
        assert_eq!(list_columns(1280.0).date, Some(52.0), "the compact date needs 52 pt");
        // The 1280×720 reading split leaves a 499 pt list; the subject keeps its floor there.
        let list = 1280.0 - ui_size::SIDEBAR - reading_pane_width(1280.0);
        assert!((list - 499.0).abs() < 2.0, "the list is about 499 pt, got {list}");
        assert!(subject_width(list) >= 260.0, "the subject gets {} pt at a 499 pt list", subject_width(list));
    }

    #[test]
    fn selected_label_dots_keep_their_identity() {
        let light = theme::tokens(false);
        assert_eq!(label_dot(LabelColor::Gold, &light, true), light.gold);
        assert_eq!(label_dot(LabelColor::Forest, &light, true), light.selected_text);
        let seeded = [LabelColor::Gold, LabelColor::Success, LabelColor::Danger];
        for (index, color) in seeded.iter().enumerate() {
            let dot = label_dot(*color, &light, true);
            assert!(crate::ui::tokens::contrast_ratio(dot, light.selected) >= 3.0, "{color:?} dot is invisible on the selected row");
            assert!(
                seeded.iter().enumerate().all(|(other, candidate)| other == index || label_dot(*candidate, &light, true) != dot),
                "the three seeded label colours must stay distinct on selection"
            );
        }
        assert_eq!(label_dot(LabelColor::Success, &light, false), light.success);
    }

    fn frame_time(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, events: Vec<egui::Event>, time: f64) {
        let raw =
            egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), time: Some(time), events, ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();
    }

    #[test]
    fn the_empty_state_names_each_filter_and_never_blames_the_folder() {
        let mut app = PostinoApp::demo(true);

        let inbox = app.empty_view(None, None);
        assert_eq!(inbox.title, "Nessun messaggio");
        assert_eq!(inbox.body, "La posta in arrivo è vuota.");
        assert!(inbox.primary.is_none(), "an honestly empty inbox needs no way out");

        app.unread_only = true;
        let unread = app.empty_view(None, None);
        assert_eq!(unread.title, "Nessun messaggio non letto");
        assert_eq!(unread.primary.map(|(label, _)| label), Some("Mostra tutti"));
        assert!(unread.body.contains("non letti"));
        assert!(!unread.body.contains("è vuota"), "a filter must never be reported as an empty folder");

        app.unread_only = false;
        app.attachments_only = true;
        let attachments = app.empty_view(None, None);
        assert_eq!(attachments.title, "Nessun messaggio con allegati");
        assert_eq!(attachments.primary.map(|(label, _)| label), Some("Mostra tutti"));
        assert!(!attachments.body.contains("è vuota"));

        app.attachments_only = false;
        app.select_label("lbl_1".to_owned());
        let label = app.empty_view(Some("Lavoro"), Some("in tutte le cartelle"));
        assert_eq!(label.title, "Nessun messaggio");
        assert_eq!(label.body, "Nessun messaggio con l'etichetta “Lavoro” in tutte le cartelle.");
        assert_eq!(label.primary.map(|(label, _)| label), Some("Togli filtro etichetta"));
        assert_eq!(label.secondary.map(|(label, _)| label), Some("Mostra tutti"));

        app.unread_only = true;
        let label_unread = app.empty_view(Some("Lavoro"), Some("in tutte le cartelle"));
        assert_eq!(label_unread.title, "Nessun messaggio non letto con l'etichetta “Lavoro”");
        assert_eq!(label_unread.primary.map(|(label, _)| label), Some("Togli filtro “non letti”"));
        assert!(!label_unread.body.contains("è vuota"));

        app.unread_only = false;
        app.query = "nessun-risultato".to_owned();
        let search = app.empty_view(None, None);
        assert_eq!(search.title, "Nessun risultato");
        assert_eq!(search.body, "Nessun messaggio corrisponde a “nessun-risultato” in tutte le cartelle.");
        assert_eq!(search.primary.map(|(label, _)| label), Some("Cancella ricerca"));
        let search_in_label = app.empty_view(Some("Lavoro"), Some("in tutte le cartelle"));
        assert!(search_in_label.body.contains("nell'etichetta “Lavoro”"), "{}", search_in_label.body);

        app.query.clear();
        app.clear_label_filter();
        app.select_folder("drafts");
        let drafts = app.empty_view(None, None);
        assert_eq!(drafts.title, "Nessuna bozza");
        assert_eq!(drafts.primary.map(|(label, _)| label), Some("Scrivi"));

        app.select_folder("archive");
        let folder = app.empty_view(None, None);
        assert_eq!(folder.body, "Questa cartella è vuota.");
        assert_eq!(folder.primary.map(|(label, _)| label), Some("Vai a Posta in arrivo"));
    }

    #[test]
    fn the_three_filter_empty_states_render_and_offer_a_way_out() {
        let (ctx, _) = ready_app();
        let wide = Vec2::new(1280.0, 720.0);
        let cases = [
            (
                ScreenshotState::EmptyUnread,
                "Nessun messaggio non letto",
                "non ci sono messaggi non letti",
                Some("Mostra tutti"),
                None,
                EmptyAction::ShowAll,
            ),
            (
                ScreenshotState::EmptyAttachments,
                "Nessun messaggio con allegati",
                "non ci sono messaggi con allegati",
                Some("Mostra tutti"),
                None,
                EmptyAction::ShowAll,
            ),
            (
                ScreenshotState::EmptyLabelUnread,
                "Nessun messaggio non letto con l'etichetta “Lavoro”",
                "con l'etichetta “Lavoro”",
                Some("Togli filtro “non letti”"),
                None,
                EmptyAction::ClearUnread,
            ),
        ];
        for (state, title, body, primary, secondary, action) in cases {
            let mut app = PostinoApp::demo(true);
            app.seed_screenshot(state);
            frame_at(&ctx, &mut app, wide, vec![]);
            frame_at(&ctx, &mut app, wide, vec![]);
            let view = probe::empty_state_view(&ctx).expect("the empty state is painted");
            assert_eq!(view.title, title, "{state:?}");
            assert!(view.body.contains(body), "{state:?} body is {:?}", view.body);
            assert!(!view.body.contains("è vuota"), "{state:?} must not blame the folder");
            assert_eq!(view.primary.as_deref(), primary, "{state:?}");
            assert_eq!(view.secondary.as_deref(), secondary, "{state:?}");
            assert!(app.visible_messages().0.is_empty(), "{state:?} must really be empty");
            app.apply_empty_action(action);
            frame_at(&ctx, &mut app, wide, vec![]);
            assert!(!app.visible_messages().0.is_empty(), "{state:?} action must reveal mail");
            assert!(probe::empty_state_view(&ctx).is_none(), "{state:?} empty state must disappear");
        }
    }

    #[test]
    fn rows_are_named_buttons_and_the_star_is_a_28_pt_control() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1280.0, 720.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        let (rows, _, _) = app.visible_messages();
        let unread = rows.iter().find(|message| message.unread).expect("a demo unread message");
        let label = probe::row_facts_for(&ctx, &unread.id).expect("the row registered its accessible name").0;
        let sender = if unread.from.name.trim().is_empty() { unread.from.email.as_str() } else { unread.from.name.as_str() };
        assert!(label.contains(sender), "the name names the sender: {label}");
        assert!(label.contains(unread.subject.as_str()), "the name names the subject: {label}");
        assert!(label.contains("Non letto"), "{label}");
        assert!(label.contains("Premi Invio per aprire"), "{label}");
        assert!(label.contains(&format_when(unread.date, app.now_utc)), "the name carries the date: {label}");
        if unread.has_attachments {
            assert!(label.contains("Con allegati"), "{label}");
        }

        let star = probe::star_rect(&ctx, &unread.id).expect("the star rectangle");
        assert!(star.width() >= 28.0 && star.height() >= 28.0, "the star target is {star:?}, smaller than 28×28");

        // A click on the star toggles it and never opens the message behind it.
        click_at(&ctx, &mut app, star.center(), wide);
        assert_eq!(app.engine.message(&unread.id).map(|message| message.summary.starred), Some(!unread.starred));
        assert!(!app.reading_open, "a star click must not open the message");

        // A near miss at the star's edge still lands in the target, not on the row.
        let corner = star.min + Vec2::new(1.0, 1.0);
        let starred = app.engine.message(&unread.id).is_some_and(|message| message.summary.starred);
        click_at(&ctx, &mut app, corner, wide);
        assert_eq!(app.engine.message(&unread.id).map(|message| message.summary.starred), Some(!starred));
        assert!(!app.reading_open);

        // A selected row says so.
        app.select_only(unread.id.clone());
        frame_at(&ctx, &mut app, wide, vec![]);
        let selected = probe::row_facts_for(&ctx, &unread.id).expect("the selected row").0;
        assert!(selected.contains("Selezionato."), "{selected}");
    }

    #[test]
    fn the_focus_ring_follows_keyboard_focus_and_tab_reaches_the_list() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1280.0, 720.0);
        frame_at(&ctx, &mut app, wide, vec![]);

        // Selected but not keyboard-focused: no ring.
        let first = app.visible_messages().0[0].id.clone();
        app.select_only(first.clone());
        frame_at(&ctx, &mut app, wide, vec![]);
        assert!(probe::focused_rows(&ctx).is_empty(), "a programmatic selection must not paint a focus ring");
        assert!(probe::focus_rings(&ctx).is_empty(), "no row held keyboard focus");

        // J requests real keyboard focus on the row.
        app.clear_selection();
        frame_at(&ctx, &mut app, wide, vec![key(Key::J, Modifiers::NONE)]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let focused = probe::focused_rows(&ctx);
        assert_eq!(focused.first().map(String::as_str), Some(first.as_str()), "J focuses the row: {focused:?}");
        assert!(probe::focus_rings(&ctx).contains(&first), "the focus ring is painted on the focused row");
        assert_eq!(ctx.memory(|memory| memory.focused()), app.row_ids.get(&first).copied(), "the row widget owns focus");

        // Tab from a fresh app eventually reaches the list without changing the selection.
        let (ctx, mut app) = ready_app();
        frame_at(&ctx, &mut app, wide, vec![]);
        let mut reached = false;
        for _ in 0..80 {
            frame_at(&ctx, &mut app, wide, vec![key(Key::Tab, Modifiers::NONE)]);
            if !probe::focused_rows(&ctx).is_empty() {
                reached = true;
                break;
            }
        }
        assert!(reached, "Tab never reached a message row");
    }

    #[test]
    fn u_right_after_opening_cancels_the_auto_read() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1200.0, 800.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        let id = app.engine.messages().iter().find(|message| message.unread).map(|message| message.id.clone()).expect("an unread demo message");
        frame_time(&ctx, &mut app, wide, vec![], 10.0);
        app.open_message(id.clone());
        assert!(app.pending_read.is_some(), "opening starts the auto-read timer");
        frame_time(&ctx, &mut app, wide, vec![key(Key::U, Modifiers::NONE)], 10.01);
        assert!(app.pending_read.is_none(), "U cancels the pending auto-read");
        frame_time(&ctx, &mut app, wide, vec![], 12.0);
        app.maybe_complete_read();
        assert!(app.engine.message(&id).is_some_and(|message| message.summary.unread), "U must win over the timer");
    }

    #[test]
    fn a_star_also_cancels_the_auto_read() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages().iter().find(|message| message.unread).map(|message| message.id.clone()).expect("an unread demo message");
        app.now = 5.0;
        app.open_message(id.clone());
        app.toggle_star_for(&id);
        assert!(app.pending_read.is_none(), "an explicit flag action cancels the auto-read");
        app.now = 6.0;
        app.maybe_complete_read();
        assert!(app.engine.message(&id).is_some_and(|message| message.summary.unread));
    }

    #[test]
    fn plain_j_k_collapse_and_shift_extends_from_the_anchor() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1200.0, 800.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        let (rows, _, _) = app.visible_messages();
        app.select_only(rows[0].id.clone());
        frame_at(&ctx, &mut app, wide, vec![]);

        frame_at(&ctx, &mut app, wide, vec![egui::Event::ModifiersChanged(Modifiers::SHIFT), key(Key::J, Modifiers::SHIFT)]);
        assert_eq!(app.selection.len(), 2, "Shift+J extends from the anchor");
        assert_eq!(app.anchor.as_deref(), Some(rows[0].id.as_str()));
        let bulk = probe::bulk_bar_rect(&ctx).expect("the bulk bar is visible while the anchor is alive");
        assert!(bulk.height() > 0.0);

        frame_at(&ctx, &mut app, wide, vec![key(Key::J, Modifiers::SHIFT)]);
        assert_eq!(app.selection.len(), 3, "Shift+J extends the range from the anchor: {:?}", app.selection);
        frame_at(&ctx, &mut app, wide, vec![key(Key::K, Modifiers::SHIFT)]);
        assert_eq!(app.selection.len(), 2, "Shift+K contracts the range from the anchor");
        assert!(app.selection.contains(&rows[0].id) && app.selection.contains(&rows[1].id));

        // Plain J moves the focus and collapses to that row; the batch and the bulk bar go.
        frame_at(&ctx, &mut app, wide, vec![egui::Event::ModifiersChanged(Modifiers::NONE), key(Key::J, Modifiers::NONE)]);
        assert_eq!(app.selected.as_deref(), Some(rows[2].id.as_str()), "plain J moves the focus");
        assert_eq!(app.selection.len(), 1, "plain J collapses the selection");
        assert_eq!(app.anchor.as_deref(), Some(rows[2].id.as_str()), "the anchor follows the collapsed focus");
        assert!(
            probe::bulk_bar_rect(&ctx).is_none(),
            "the bulk bar disappears with the batch: selection {:?}, bulk {:?}",
            app.selection,
            probe::bulk_bar_rect(&ctx)
        );
        assert!(!app.reading_open);
    }

    #[test]
    fn the_reading_pane_width_is_restored_from_settings() {
        let (ctx, mut app) = ready_app();
        let size = Vec2::new(1600.0, 900.0);
        app.reading_ratio = 0.55;
        let id = app.visible_messages().0[0].id.clone();
        app.open_message(id);
        for _ in 0..3 {
            frame_at(&ctx, &mut app, size, vec![]);
        }
        let (content, panel) = probe::list_widths(&ctx).expect("the list width");
        let remaining = size.x - ui_size::SIDEBAR;
        let pane = size.x - ui_size::SIDEBAR - panel;
        assert!((pane / remaining - 0.55).abs() < 0.02, "the stored 0.55 fraction opens the pane at {pane} of {remaining} ({content} list content)");
        assert!((app.reading_ratio - 0.55).abs() < 0.02, "the measured fraction is written back: {}", app.reading_ratio);
        assert!((app.settings_snapshot().reading_pane_ratio - app.reading_ratio).abs() < f32::EPSILON, "the snapshot would persist the ratio");
    }

    #[test]
    fn the_reading_pane_clamps_a_hostile_fraction() {
        for ratio in [-1.0, 0.0, 0.99, f32::NAN] {
            let width = reading_pane_width_with_ratio(1600.0, ratio);
            let remaining = 1600.0 - ui_size::SIDEBAR;
            assert!(width >= READING_PANE_MIN_SIZE, "{ratio} produced {width}");
            assert!(1600.0 - ui_size::SIDEBAR - width >= ui_size::LIST_MIN, "{ratio} squeezed the list: {width}");
            assert!(width <= remaining * READING_PANE_MAX_RATIO + 0.01, "{ratio} produced an oversized pane: {width}");
        }
    }

    #[test]
    fn settings_round_trip_through_the_app_and_never_include_a_draft() {
        let mut app = PostinoApp::demo(true);
        app.compose_new();
        app.composer.subject = "OGGETTO SEGRETO".to_owned();
        app.composer.body = "CORPO SEGRETO".to_owned();
        app.composer.touch(0.0);
        app.dark = true;
        app.system_theme = true;
        app.folder = "archive".to_owned();
        app.reading_ratio = 0.44;
        app.reduced_motion = true;
        let path = crate::settings::test_directory().join(format!("app-round-trip-{}.json", std::process::id()));
        app.settings_path = Some(path.clone());
        app.persist_settings();

        let loaded = Settings::load_from(&path);
        assert!(loaded.dark);
        assert!(loaded.system_theme, "the OS-following choice is a preference");
        assert_eq!(loaded.folder, "archive");
        assert_eq!(loaded.account.as_deref(), Some("acc_demo"), "the active account is a preference");
        assert!((loaded.reading_pane_ratio - 0.44).abs() < f32::EPSILON);
        assert!(loaded.reduced_motion);
        let text = std::fs::read_to_string(&path).expect("the settings file");
        assert!(!text.contains("SEGRETO"), "draft text must never be persisted: {text}");
        let _ = std::fs::remove_file(&path);

        let mut restored = PostinoApp::demo(true);
        restored.apply_settings(loaded);
        assert_eq!(restored.folder, "archive");
        assert_eq!(restored.active_account.as_deref(), Some("acc_demo"));
        assert!(restored.dark);
        assert!(restored.system_theme);
        assert!(restored.reduced_motion);

        // A folder or account the engine does not know falls back to the current account and its
        // inbox.
        let unknown = Settings { folder: "cartella-inesistente".to_owned(), account: Some("acc_inesistente".to_owned()), ..Settings::default() };
        restored.apply_settings(unknown);
        assert_eq!(restored.folder, "inbox");
        assert_eq!(restored.active_account.as_deref(), Some("acc_demo"));
    }

    #[test]
    fn reduced_motion_turns_every_animation_off() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        theme::apply_motion(&ctx, false);
        assert!(theme::motion_seconds(&ctx, 0.14) > 0.0);
        assert!((ctx.global_style().animation_time - theme::ANIMATION_TIME).abs() < f32::EPSILON);

        theme::apply_motion(&ctx, true);
        assert_eq!(theme::motion_seconds(&ctx, 0.14), 0.0);
        assert_eq!(ctx.global_style().animation_time, 0.0);
    }

    #[test]
    fn the_status_bar_has_three_honest_states() {
        assert_eq!(status_kind(true, false), StatusKind::Demo);
        assert_eq!(status_kind(false, false), StatusKind::Ready);
        assert_eq!(status_kind(true, true), StatusKind::Error);
        assert_eq!(status_kind(false, true), StatusKind::Error);

        let mut app = PostinoApp::demo(true);
        assert_eq!(app.status_kind(), StatusKind::Demo);
        app.demo = false;
        app.sync_view = SyncView::Offline { last_sync: Some(1_700_000_000_000) };
        assert_eq!(app.status_kind(), StatusKind::Offline);
        app.sync_view = SyncView::Error { reason: "credenziali rifiutate".to_owned(), last_sync: None };
        assert_eq!(app.status_kind(), StatusKind::Error);
        app.sync_view = SyncView::Idle { last_sync: None };
        app.demo = true;
        app.report_failure("Non è stato possibile leggere i messaggi. Riprova.", true);
        assert_eq!(app.status_kind(), StatusKind::Error);
        assert!(app.toast.is_none(), "an error is a toast, not an undo toast");
        app.dismiss_failure();
        assert_eq!(app.status_kind(), StatusKind::Demo);
        assert!(app.list_error_ack, "a dismissed list failure is not re-opened for the same view");
        app.query = "cambio-vista".to_owned();
        let _ = app.visible_messages();
        assert!(!app.list_error_ack, "a new filter is a fresh read and may report again");
        app.query.clear();
    }

    #[test]
    fn an_error_is_one_dismissable_toast_and_never_a_raw_status_line() {
        let (ctx, mut app) = ready_app();
        // The toast slides in; the probe and the click must see the same settled rectangle.
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let wide = Vec2::new(1200.0, 800.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        let message = "Non è stato possibile aggiornare il messaggio.";
        app.report_failure(message, false);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        assert!(!app.status.contains(message), "the failure must not also become a status line: {}", app.status);
        crate::ui::widgets::toast_probe::clear(&ctx);
        frame_at(&ctx, &mut app, wide, vec![]);
        let buttons = crate::ui::widgets::toast_probe::take(&ctx);
        let dismiss = buttons.iter().find(|(label, _)| label == "Chiudi").map(|(_, rect)| *rect).expect("the error toast has a dismiss control");
        assert!(!buttons.iter().any(|(label, _)| label == "Riprova"), "a non-list failure has no meaningful retry");
        click_at(&ctx, &mut app, dismiss.center(), wide);
        assert!(app.failure.is_none(), "dismiss clears the failure");
        assert_eq!(app.status_kind(), StatusKind::Demo);

        // The list failure is the one case with a meaningful retry.
        app.report_failure("Non è stato possibile leggere i messaggi. Riprova.", true);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        crate::ui::widgets::toast_probe::clear(&ctx);
        frame_at(&ctx, &mut app, wide, vec![]);
        let buttons = crate::ui::widgets::toast_probe::take(&ctx);
        let retry = buttons.iter().find(|(label, _)| label == "Riprova").map(|(_, rect)| *rect).expect("the list failure offers a retry");
        click_at(&ctx, &mut app, retry.center(), wide);
        assert!(app.failure.is_none());
        assert_eq!(app.status, "Elenco aggiornato");
    }

    #[test]
    fn the_loading_line_waits_for_the_delay_and_never_flickers() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        theme::apply_motion(&ctx, true);
        let mut app = PostinoApp::demo(true);
        let wide = Vec2::new(1200.0, 800.0);
        frame_time(&ctx, &mut app, wide, vec![], 0.0);

        app.begin_loading();
        frame_time(&ctx, &mut app, wide, vec![], 0.1);
        assert!(probe::loading_line(&ctx).is_none(), "a short operation must not flash the line");
        app.end_loading();

        // Work that stays open past the delay is shown, at full alpha under reduced motion.
        app.begin_loading();
        frame_time(&ctx, &mut app, wide, vec![], 0.31);
        let (rect, alpha) = probe::loading_line(&ctx).expect("the delayed loading line");
        assert!(rect.height() > 0.0);
        assert_eq!(alpha, 1.0, "reduced motion shows the line at once");
        app.end_loading();
        frame_time(&ctx, &mut app, wide, vec![], 0.42);
        assert!(probe::loading_line(&ctx).is_none());
    }

    #[test]
    fn sync_is_enabled_only_for_a_store_account() {
        assert!(command("refresh").is_none(), "the enabled no-op must not exist");
        let sync = command("sync").expect("sync is registered");
        assert_eq!(sync.label, "Aggiorna la posta");
        let app = PostinoApp::demo(true);
        assert!(!app.command_enabled(sync));
        assert!(sync.rule.reason().is_some_and(|reason| reason.contains("account")));
    }

    #[test]
    fn the_compact_date_formats_are_the_documented_ones() {
        let frozen = screenshot_clock().expect("the screenshot clock");
        let this_year = DateTime::parse_from_rfc3339("2026-03-04T08:15:00Z").expect("a date").with_timezone(&Utc);
        let older = DateTime::parse_from_rfc3339("2024-12-31T08:15:00Z").expect("a date").with_timezone(&Utc);
        assert_eq!(format_when(frozen, frozen), frozen.with_timezone(&Local).format("%H:%M").to_string(), "today shows HH:MM");
        assert_eq!(format_when(this_year, frozen), this_year.with_timezone(&Local).format("%d/%m").to_string(), "this year shows dd/MM");
        assert_eq!(format_when(older, frozen), older.with_timezone(&Local).format("%d/%m/%y").to_string(), "older shows dd/MM/yy");
        assert_eq!(format_when(older, frozen).len(), 8, "the compact year is two digits");
    }

    #[test]
    fn there_is_exactly_one_demo_notice_and_no_milestone_code_in_the_copy() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        for source in ["app.rs", "ui/widgets/composer.rs", "ui/gallery.rs", "rules_ui.rs"] {
            let path = format!("{root}/{source}");
            let text = std::fs::read_to_string(&path).expect("the source file");
            let production = text.split_once("\nmod tests {").map_or(text.as_str(), |(before, _)| before);
            for forbidden in
                ["M0 + U0.5", "Locale · offline", "HTML in M5", "metadati in M0", "memoria fino a M1", "dopo U0.5", "store M1", "SMTP (P7)"]
            {
                assert!(!production.contains(forbidden), "{source} still contains {forbidden:?}");
            }
        }
        let demo = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../crates/core/src/demo.rs")).expect("the demo data");
        assert!(!demo.contains("modalità M0"), "the demo body still names the milestone");
    }

    #[test]
    fn every_label_colour_is_legible_on_ivory_and_on_forest() {
        let light = theme::tokens(false);
        let dark = theme::tokens(true);
        for (color, name) in LABEL_COLORS {
            let on_ivory = crate::ui::tokens::contrast_ratio(label_color(color, &light), theme::IVORY);
            assert!(on_ivory >= 3.0, "{name} on ivory is {on_ivory:.2}:1");
            let on_forest = crate::ui::tokens::contrast_ratio(label_dot(color, &light, true), theme::FOREST);
            assert!(on_forest >= 3.0, "{name} on the selected forest row is {on_forest:.2}:1");
            let on_dark = crate::ui::tokens::contrast_ratio(label_color(color, &dark), theme::DARK_PANEL);
            assert!(on_dark >= 3.0, "{name} on the dark panel is {on_dark:.2}:1");
        }
        assert_eq!(label_color(LabelColor::Gold, &light), theme::GOLD_INK, "gold uses its ink on a light surface");
        assert_eq!(label_color(LabelColor::Gold, &dark), theme::GOLD, "the dark theme keeps the light gold");
    }

    #[test]
    fn sidebar_shows_the_label_registry_and_filters_the_list() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let labels = app.labels_cache.clone();
        assert_eq!(labels.len(), 3);
        assert!(labels.iter().any(|label| label.name == "Lavoro" && label.message_count == 6 && label.unread_count == 2));

        app.select_label("lbl_1".to_owned());
        let (rows, total, unread) = app.visible_messages();
        assert_eq!(total, 6, "a label view spans every folder by default");
        assert_eq!(unread, 2);
        assert!(rows.iter().all(|message| message.labels.contains(&"lbl_1".to_owned())));
        app.open_message(rows[0].id.clone());
        frame(&ctx, &mut app, vec![]);
        assert!(app.reading_open, "the labelled message opens with its chips");

        app.select_folder("inbox");
        assert_eq!(app.label_filter.as_deref(), Some("lbl_1"), "a folder click narrows instead of dropping the label");
        assert_eq!(app.label_folder.as_deref(), Some("inbox"));
        assert_eq!(app.visible_messages().1, 2, "the intersection with Posta in arrivo is two messages");

        app.clear_label_filter();
        assert!(app.visible_messages().1 >= 4, "Mostra tutte returns to the folder view");
    }

    #[test]
    fn inline_label_create_and_duplicate_errors_keep_the_field_open() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.push_toast("Messaggio archiviato", UndoAction::Move(Vec::new()));
        assert!(app.toast.is_some(), "the stale toast must exist before the create");
        app.start_label_create();
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Ufficio".to_owned();
            editor.color = LabelColor::Forest;
        }
        app.commit_label_editor();
        assert!(app.label_editor.is_none());
        assert!(app.engine.labels().iter().any(|label| label.name == "Ufficio" && label.color == LabelColor::Forest));
        assert!(app.status.contains("creata"));
        assert!(app.toast.is_none(), "creating a label is not undoable and clears a stale toast");

        app.start_label_create();
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "lavoro".to_owned();
        }
        app.commit_label_editor();
        let editor = app.label_editor.as_ref().expect("duplicate keeps the field open");
        assert_eq!(editor.error.as_deref(), Some("Esiste già un'etichetta con questo nome."));
        assert!(editor.focus, "focus returns to the field after the error");
        frame(&ctx, &mut app, vec![]);

        app.start_label_rename("lbl_1");
        assert_eq!(app.label_editor.as_ref().map(|editor| editor.text.as_str()), Some("Lavoro"));
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Lavoro clienti".to_owned();
            editor.color = LabelColor::Danger;
        }
        app.commit_label_editor();
        assert_eq!(app.engine.label("lbl_1").map(|label| label.name.as_str()), Some("Lavoro clienti"));
        assert_eq!(app.engine.label("lbl_1").map(|label| label.color), Some(LabelColor::Danger));
        assert!(matches!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()), Some(UndoAction::LabelRename { .. })), "rename offers an undo");
        app.undo_last();
        assert_eq!(app.engine.label("lbl_1").map(|label| label.name.as_str()), Some("Lavoro"), "undo restores the previous name");
        assert_eq!(app.engine.label("lbl_1").map(|label| label.color), Some(LabelColor::Gold), "undo restores the previous colour");
    }

    #[test]
    fn label_menu_applies_to_a_multi_selection_as_one_undo_step() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();
        let ids = vec![rows[0].id.clone(), rows[1].id.clone()];
        app.select_only(rows[0].id.clone());
        app.toggle_selection(rows[1].id.clone());
        frame(&ctx, &mut app, vec![key(Key::L, Modifiers::NONE)]);
        assert_eq!(app.label_menu.as_ref().map(|menu| menu.context.target.len()), Some(2), "L opens the menu for the whole selection");
        app.open_label_menu(ids.clone());
        frame(&ctx, &mut app, vec![]);
        app.toggle_label_target("lbl_2");
        assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.labels.contains(&"lbl_2".to_owned()))));
        app.undo_last();
        assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| !message.summary.labels.contains(&"lbl_2".to_owned()))));
    }

    #[test]
    fn deleting_a_label_is_confirmed_and_undoable() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.start_label_create();
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Temporanea".to_owned();
        }
        app.commit_label_editor();
        let id = app.engine.labels().iter().find(|label| label.name == "Temporanea").expect("created").id.clone();
        app.apply_label(&["msg_001".to_owned()], &id, true);
        app.request_label_delete(&id);
        let state = app.label_delete.clone().expect("confirmation dialog");
        assert_eq!(state.count, 1);
        frame(&ctx, &mut app, vec![]);
        app.delete_label(&state);
        assert!(app.engine.label(&id).is_none());
        assert!(app.engine.message("msg_001").is_some_and(|message| !message.summary.labels.contains(&id)));
        app.undo_last();
        let restored = app.engine.labels().iter().find(|label| label.name == "Temporanea").expect("restored label");
        assert_ne!(restored.id, id, "a delete undo recreates the label with a fresh id, never the retired one");
        assert!(app.engine.message("msg_001").is_some_and(|message| message.summary.labels.contains(&restored.id)));
    }

    #[test]
    fn delete_dialog_confirms_with_enter_and_cancels_with_escape() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);

        app.request_label_delete("lbl_2");
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.label_delete.is_none(), "Enter closes the dialog");
        assert!(app.engine.label("lbl_2").is_none(), "Enter confirms the delete");

        app.request_label_delete("lbl_3");
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.label_delete.is_none(), "Esc closes the dialog");
        assert!(app.engine.label("lbl_3").is_some(), "Esc cancels without deleting");
    }

    #[test]
    fn label_menu_removal_from_a_multi_selection_is_one_undo_step() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();
        let ids = vec![rows[0].id.clone(), rows[1].id.clone()];
        app.open_label_menu(ids.clone());
        app.toggle_label_target("lbl_2");
        assert!(ids.iter().all(|id| has_label(&app, id, "lbl_2")), "the first click applies the label");
        app.toggle_label_target("lbl_2");
        assert!(ids.iter().all(|id| !has_label(&app, id, "lbl_2")), "the second click removes it");
        app.undo_last();
        assert!(ids.iter().all(|id| has_label(&app, id, "lbl_2")), "one undo restores the whole batch");
    }

    #[test]
    fn label_menu_targets_are_captured_and_stale_selection_closes_it() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let (rows, _, _) = app.visible_messages();
        app.select_only(rows[0].id.clone());
        app.open_label_menu(app.action_ids());
        assert_eq!(app.label_menu.as_ref().map(|menu| menu.context.target.clone()), Some(vec![rows[0].id.clone()]));

        app.select_only(rows[1].id.clone());
        assert!(app.label_menu_is_stale(), "the menu notices the selection changed");
        frame(&ctx, &mut app, vec![]);
        assert!(app.label_menu.is_none(), "a stale menu closes instead of labelling the wrong message");
    }

    #[test]
    fn label_menu_row_click_toggles_the_label_through_the_widget() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let target = vec![app.visible_messages().0[0].id.clone()];
        app.open_label_menu(target.clone());
        // Two frames: the modal area settles its anchored position on the first one, and egui
        // resolves a click against the previous frame's widget rects.
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![]);
        let row = probe::menu_rows(&ctx).get("lbl_2").copied().expect("the Personale row");
        click(&ctx, &mut app, row.center());
        assert!(
            has_label(&app, &target[0], "lbl_2"),
            "clicking {row:?} applies the label (status: {}, menu open: {})",
            app.status,
            app.label_menu.is_some()
        );
        frame(&ctx, &mut app, vec![]);
        let row = probe::menu_rows(&ctx).get("lbl_2").copied().expect("the menu stays open after a toggle");
        click(&ctx, &mut app, row.center());
        assert!(!has_label(&app, &target[0], "lbl_2"), "a second click removes it");
    }

    #[test]
    fn label_menu_closes_on_escape_and_on_an_outside_click() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let target = vec![app.visible_messages().0[0].id.clone()];

        app.open_label_menu(target.clone());
        frame(&ctx, &mut app, vec![]);
        assert!(app.label_menu.is_some());
        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.label_menu.is_none(), "Escape closes the menu");

        app.open_label_menu(target);
        frame(&ctx, &mut app, vec![]);
        assert!(app.label_menu.is_some());
        click(&ctx, &mut app, egui::pos2(900.0, 700.0));
        assert!(app.label_menu.is_none(), "a click outside the menu closes it");
    }

    #[test]
    fn creating_from_the_label_menu_applies_to_the_captured_targets() {
        let mut app = PostinoApp::demo(true);
        let (rows, _, _) = app.visible_messages();
        let targets = vec![rows[0].id.clone(), rows[1].id.clone()];
        app.open_label_menu(targets.clone());
        app.start_label_create_for(targets.clone());
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Cliente".to_owned();
            editor.color = LabelColor::Forest;
        }
        app.commit_label_editor();
        let created = app.engine.labels().iter().find(|label| label.name == "Cliente").expect("created label");
        assert_eq!(created.color, LabelColor::Forest);
        let created_id = created.id.clone();
        assert!(targets.iter().all(|id| has_label(&app, id, &created_id)), "the new label lands on the captured messages");
        assert!(matches!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()), Some(UndoAction::Labels(_))), "the apply is one undo step");
        app.undo_last();
        assert!(targets.iter().all(|id| !has_label(&app, id, &created_id)));
        assert!(app.engine.labels().iter().any(|label| label.name == "Cliente"), "undo removes the assignment, not the label");
    }

    #[test]
    fn label_menu_renders_with_an_empty_registry() {
        let (ctx, mut app) = ready_app();
        for id in ["lbl_1", "lbl_2", "lbl_3"] {
            app.engine.call("label_delete", json!({ "id": id })).expect("delete seeded label");
        }
        app.refresh_labels();
        assert!(app.labels_cache.is_empty());
        let (rows, _, _) = app.visible_messages();
        app.open_label_menu(vec![rows[0].id.clone()]);
        frame(&ctx, &mut app, vec![]);
        assert!(app.label_menu.is_some(), "the menu stays open to offer creation");
    }

    #[test]
    fn search_unread_and_attachment_filters_apply_before_the_page() {
        let mut app = booted_app("p9a-search-filters", Vec::new(), |store| {
            seed_account_rows(store, "acc_f", "f@example.com", None);
            for index in 0..250u32 {
                let id = format!("msg_f_{index:03}");
                let mut row = store_message("fld_acc_f_inbox", "acc_f", &id, "Comune");
                row.summary.date = 1_700_000_000_000 + i64::from(index);
                row.summary.unread = index < 10;
                row.summary.has_attachments = index < 5;
                row.body_text = "comune nel corpo".to_owned();
                store.upsert_message(&row).expect("message");
                if index < 5 {
                    seed_attachment(store, &id, &format!("att_{index}"), "nota.txt");
                }
            }
        });
        app.query = "comune".to_owned();
        app.unread_only = true;
        app.commit_search();
        let (rows, total, _) = app.visible_messages();
        assert_eq!(total, 10, "the unread filter must be part of the search page, not a post-filter");
        assert_eq!(rows.len(), 10);
        assert!(rows.iter().all(|message| message.unread));

        app.unread_only = false;
        app.attachments_only = true;
        app.commit_search();
        let (rows, total, _) = app.visible_messages();
        assert_eq!(total, 5);
        assert!(rows.iter().all(|message| message.has_attachments));
    }

    #[test]
    fn search_is_scoped_to_the_current_folder() {
        let mut app = booted_app("p9a-search-folder", Vec::new(), |store| {
            seed_account_rows(store, "acc_s", "s@example.com", None);
            for (folder, prefix) in [("fld_acc_s_inbox", "inbox"), ("fld_acc_s_sent", "sent")] {
                for index in 0..3u32 {
                    let id = format!("msg_{prefix}_{index}");
                    let mut row = store_message(folder, "acc_s", &id, "Comune");
                    row.summary.date = 1_700_000_000_000 + i64::from(index);
                    row.body_text = "comune nel corpo".to_owned();
                    store.upsert_message(&row).expect("message");
                }
            }
        });
        app.folder = "fld_acc_s_sent".to_owned();
        app.query = "comune".to_owned();
        app.commit_search();
        let (rows, total, _) = app.visible_messages();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|message| message.folder_id == "fld_acc_s_sent"), "search escaped the current folder");
    }

    #[test]
    fn search_meta_shows_the_cap_and_index_progress_has_a_banner() {
        assert_eq!(search_meta(10_000, true, None), "primi 10.000 risultati");
        assert_eq!(search_meta(3, false, Some("in Lavoro")), "3 messaggi · in Lavoro");
        let (ctx, mut app) = ready_app();
        app.query = "preventivo".to_owned();
        app.commit_search();
        let _ = app.visible_messages();
        app.index_progress = Some(postino_core::SearchStatusResult {
            ready: false,
            indexed: 500,
            total: 1_000,
            last_error: None,
            size_bytes: 4096,
            state: "building".to_owned(),
            updated_at: 1,
        });
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Indice di ricerca in preparazione (500/1000)")), "{text:?}");
    }

    #[test]
    fn a_parse_error_does_not_also_render_the_empty_state() {
        let (ctx, mut app) = ready_app();
        app.query = "subject:\"unclosed".to_owned();
        app.commit_search();
        let _ = app.visible_messages();
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Ricerca non valida")), "{text:?}");
        assert!(!text.iter().any(|line| line.contains("Nessun risultato")), "the invalid query showed an empty state: {text:?}");
    }

    #[test]
    fn search_debounces_commits_on_enter_and_explains_parse_errors() {
        let mut app = PostinoApp::demo(true);
        app.now = 10.0;
        app.query = "preventivo".to_owned();
        app.search_debounce_until = Some(10.1);
        assert_eq!(app.search_committed, "");
        app.now = 10.2;
        let (_rows, _total, _) = app.visible_messages();
        assert_eq!(app.search_committed, "preventivo");

        app.query = "fattura".to_owned();
        app.commit_search();
        assert_eq!(app.search_committed, "fattura");
        assert!(app.search_debounce_until.is_none());

        app.query = "subject:\"unclosed".to_owned();
        app.commit_search();
        let (rows, _, _) = app.visible_messages();
        assert!(rows.is_empty());
        let (code, position) = app.search_error.clone().expect("parse error");
        assert_eq!((code.as_str(), position), ("unclosed_quote", 8));
        assert!(search_error_text("unclosed_quote", position).contains("posizione 8"));

        app.clear_search();
        assert!(app.query.is_empty() && app.search_committed.is_empty() && app.search_error.is_none());
        assert!(search_chips("from:giulia is:unread label:Lavoro").len() == 3);
        assert_eq!(search_chips("from:giulia AND subject:\"fattura\""), vec!["from:\"giulia\"".to_owned(), "subject:\"fattura\"".to_owned()]);
        let chips = search_chips("from:giulia NOT from:spam");
        assert!(chips.iter().any(|chip| chip.starts_with("NOT (")), "{chips:?}");
    }

    #[test]
    fn search_scenes_render_at_980_light_and_dark() {
        for dark in [false, true] {
            let clock = screenshot_clock().expect("the frozen screenshot clock");
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            let mut app = PostinoApp::demo_at(true, clock);
            app.dark = dark;
            app.seed_screenshot(ScreenshotState::Search);
            assert!(!app.threads_view, "the first search is flat, not threaded");
            frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(text.iter().any(|line| line.contains("Risultati per")), "search results are missing at 980: {text:?}");
            assert!(text.iter().any(|line| line.contains("from:")), "the syntax help is missing at 980: {text:?}");
            assert!(text.iter().any(|line| line == "MITTENTE"), "the flat result table is missing: {text:?}");
            assert!(!text.iter().any(|line| line.contains("CONVERSAZIONE")), "the search stayed threaded: {text:?}");

            app.seed_screenshot(ScreenshotState::SearchError);
            frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(text.iter().any(|line| line.contains("Ricerca non valida")), "the parse error is missing at 980: {text:?}");
        }
    }

    #[test]
    fn filter_and_search_intersect() {
        let mut app = PostinoApp::demo(true);
        app.select_label("lbl_1".to_owned());
        app.query = "preventivo".to_owned();
        let (rows, total, unread) = app.visible_messages();
        assert_eq!(total, 3, "three Lavoro messages mention the quote");
        assert_eq!(unread, 2);
        assert!(rows.iter().all(|message| message.labels.contains(&"lbl_1".to_owned())));

        app.select_label("lbl_3".to_owned());
        app.query = "preventivo".to_owned();
        assert_eq!(app.visible_messages().1, 0, "the same search under Fatture has no results");
    }

    #[test]
    fn the_search_placeholder_names_the_active_label() {
        let mut app = PostinoApp::demo(true);
        assert_eq!(app.search_placeholder(), "Cerca in tutte le mail…");
        app.select_label("lbl_1".to_owned());
        assert_eq!(app.search_placeholder(), "Cerca in Lavoro…");
        app.select_label("lbl_3".to_owned());
        assert_eq!(app.search_placeholder(), "Cerca in Fatture…");
    }

    #[test]
    fn label_cache_tracks_auto_read_batch_read_and_undo() {
        let mut app = PostinoApp::demo(true);
        app.now = 10.0;
        assert_eq!(label_unread(&app, "lbl_1"), Some(2));
        let unread = app
            .engine
            .messages()
            .iter()
            .find(|message| message.unread && message.labels.iter().any(|id| id == "lbl_1"))
            .map(|message| message.id.clone())
            .expect("an unread Lavoro message");
        app.open_message(unread);
        app.now = 10.7;
        app.maybe_complete_read();
        assert_eq!(label_unread(&app, "lbl_1"), Some(1), "auto-read updates the sidebar counts");
        app.undo_last();
        assert_eq!(label_unread(&app, "lbl_1"), Some(2), "undo updates them back");

        let remaining = app
            .engine
            .messages()
            .iter()
            .filter(|message| message.unread && message.labels.iter().any(|id| id == "lbl_1"))
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        app.set_unread_many(&remaining, false);
        assert_eq!(label_unread(&app, "lbl_1"), Some(0), "a batch read updates the cache once");
    }

    #[test]
    fn palette_clear_filter_is_disabled_until_a_label_is_active() {
        let mut app = PostinoApp::demo(true);
        let clear = command("label_clear_filter").expect("label_clear_filter is registered");
        assert_eq!(clear.label, "Togli filtro etichetta");
        assert!(!app.command_enabled(clear));
        assert_eq!(clear.rule.disabled_reason(), "Nessun filtro etichetta attivo");
        let rename = command("label_rename").expect("label_rename is registered");
        assert!(!app.command_enabled(rename));
        assert_eq!(rename.rule.disabled_reason(), "Seleziona prima un'etichetta");

        app.select_label("lbl_1".to_owned());
        assert!(app.command_enabled(clear));
        assert!(app.command_enabled(rename));
        let ctx = egui::Context::default();
        app.execute_command("label_clear_filter", &ctx);
        assert!(app.label_filter.is_none(), "the enabled command changes the view");
    }

    #[test]
    fn f2_renames_the_active_label() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.select_label("lbl_1".to_owned());
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![key(Key::F2, Modifiers::NONE)]);
        assert!(
            matches!(app.label_editor.as_ref().map(|editor| &editor.mode), Some(LabelEditorMode::Rename(id)) if id == "lbl_1"),
            "F2 opens the rename editor on the active label"
        );
        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.label_editor.is_none());
    }

    #[test]
    fn shortcuts_survive_a_focused_label_row() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.select_label("lbl_1".to_owned());
        frame(&ctx, &mut app, vec![]);
        let row = probe::rows(&ctx).get("Lavoro").copied().expect("the Lavoro row is on screen");
        click(&ctx, &mut app, row.center());
        assert!(app.label_editor.is_none());
        assert!(ctx.memory(|memory| memory.focused()).is_some(), "the clicked row must hold keyboard focus for this test to mean anything");
        frame(&ctx, &mut app, vec![key(Key::F2, Modifiers::NONE)]);
        assert!(
            matches!(app.label_editor.as_ref().map(|editor| &editor.mode), Some(LabelEditorMode::Rename(id)) if id == "lbl_1"),
            "F2 must still work while a non-text widget has focus"
        );
    }

    #[test]
    fn a_focused_text_field_still_suppresses_the_single_key_map() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.select_label("lbl_1".to_owned());
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![key(Key::Slash, Modifiers::NONE)]);
        frame(&ctx, &mut app, vec![]);
        assert!(ctx.text_edit_focused(), "the search field owns the keyboard");
        frame(&ctx, &mut app, vec![key(Key::F2, Modifiers::NONE)]);
        assert!(app.label_editor.is_none(), "a focused text field must still swallow F2");
    }

    #[test]
    fn plus_while_editing_keeps_the_draft() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.start_label_create();
        frame(&ctx, &mut app, vec![]);
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Ufficio".to_owned();
        }
        app.start_label_create_or_focus();
        let editor = app.label_editor.as_ref().expect("the editor stays open");
        assert_eq!(editor.text, "Ufficio", "pressing + must not discard the draft");
        assert!(editor.focus, "it returns focus to the field");
    }

    #[test]
    fn editor_pointer_controls_save_and_cancel() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.start_label_create();
        frame(&ctx, &mut app, vec![]);
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Da mouse".to_owned();
        }
        frame(&ctx, &mut app, vec![]);
        let save = probe::saved_at(&ctx).expect("the Salva control is on screen");
        click(&ctx, &mut app, save.center());
        assert!(app.label_editor.is_none(), "Salva commits the field");
        assert!(app.labels_cache.iter().any(|label| label.name == "Da mouse"));

        app.start_label_create();
        frame(&ctx, &mut app, vec![]);
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Scartata".to_owned();
        }
        frame(&ctx, &mut app, vec![]);
        let cancel = probe::cancel_at(&ctx).expect("the Annulla control is on screen");
        click(&ctx, &mut app, cancel.center());
        assert!(app.label_editor.is_none(), "Annulla closes the editor");
        assert!(!app.labels_cache.iter().any(|label| label.name == "Scartata"));
    }

    #[test]
    fn inline_editor_enter_commits_and_escape_cancels() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.start_label_create();
        frame(&ctx, &mut app, vec![]);
        assert!(app.label_editor.as_ref().is_some_and(|editor| editor.focused), "the field owns the keyboard");
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Da tastiera".to_owned();
        }
        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.label_editor.is_none(), "Enter saves");
        assert!(app.labels_cache.iter().any(|label| label.name == "Da tastiera"));

        app.start_label_create();
        frame(&ctx, &mut app, vec![]);
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Scartata".to_owned();
        }
        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.label_editor.is_none(), "Esc cancels");
        assert!(!app.labels_cache.iter().any(|label| label.name == "Scartata"));
    }

    #[test]
    fn deleting_a_label_with_more_than_five_hundred_assignments_restores_them_all() {
        let mut app = PostinoApp::demo(true);
        let mut ids = Vec::new();
        for index in 0..501 {
            let value = app.engine.call("draft_save", json!({ "subject": format!("Bozza bulk {index}"), "body_text": "x" })).expect("draft saved");
            let saved = serde_json::from_value::<DraftSave>(value).expect("draft response");
            ids.push(saved.draft.summary.id);
        }
        let value = app.engine.call("label_create", json!({ "name": "Bulk", "color": "muted" })).expect("label created");
        let label = serde_json::from_value::<LabelCreate>(value).expect("label response").label;
        for chunk in ids.chunks(LABEL_APPLY_CHUNK) {
            app.engine.call("label_apply", json!({ "id": label.id, "message_ids": chunk, "apply": true })).expect("label applied");
        }
        app.refresh_labels();
        app.request_label_delete(&label.id);
        let state = app.label_delete.clone().expect("confirmation dialog");
        assert_eq!(state.count, 501);
        app.delete_label(&state);
        assert!(app.engine.label(&label.id).is_none());
        app.undo_last();
        let restored = app.engine.labels().iter().find(|candidate| candidate.name == "Bulk").expect("label restored");
        assert_ne!(restored.id, label.id, "the recreated label gets a new id");
        assert!(ids.iter().all(|id| has_label(&app, id, &restored.id)), "undo restores all 501 assignments through the 500-id chunk cap");
        assert!(ids.iter().all(|id| !has_label(&app, id, &label.id)), "the retired id carries no assignment");
        assert_eq!(app.labels_cache.iter().find(|candidate| candidate.id == restored.id).map(|candidate| candidate.message_count), Some(501));
    }

    #[test]
    fn narrow_window_uses_the_reading_overlay_and_returns_to_the_list() {
        let (ctx, mut app) = ready_app();
        let narrow = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, narrow, vec![]);
        let id = app.visible_messages().0[0].id.clone();
        app.open_message(id.clone());
        frame_at(&ctx, &mut app, narrow, vec![]);
        assert!(app.reading_open, "clicking a row opens the overlay");
        assert!(!app.reading_full, "the overlay is derived from the width, not a separate mode");
        frame_at(&ctx, &mut app, narrow, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.reading_open, "Esc returns to the list");
        assert_eq!(app.selected.as_deref(), Some(id.as_str()), "the row stays selected");
    }

    #[test]
    fn narrow_reading_overlay_keeps_triage_actions() {
        let (ctx, mut app) = ready_app();
        let narrow = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, narrow, vec![]);
        let id = app.visible_messages().0[0].id.clone();
        app.open_message(id);
        frame_at(&ctx, &mut app, narrow, vec![]);
        let actions = probe::actions(&ctx);
        assert!(actions.contains(&"back"), "the overlay keeps Back: {actions:?}");
        assert!(actions.contains(&"archive"), "the overlay keeps Archivia: {actions:?}");
        assert!(actions.contains(&"delete"), "the overlay keeps Elimina: {actions:?}");

        app.close_reading();
        frame(&ctx, &mut app, vec![]);
        let wide = app.visible_messages().0[0].id.clone();
        app.open_message(wide);
        frame(&ctx, &mut app, vec![]);
        let actions = probe::actions(&ctx);
        assert!(actions.contains(&"archive") && actions.contains(&"delete"), "the wide pane keeps its triage actions: {actions:?}");
    }

    #[test]
    fn narrow_sidebar_keeps_the_editor_in_a_pinned_drawer() {
        let (ctx, mut app) = ready_app();
        let narrow = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, narrow, vec![]);
        let offset_before = probe::scroll_offset(&ctx).expect("the sidebar scroll offset");
        assert_eq!(offset_before, 0.0, "the sidebar starts at the top");

        app.start_label_create();
        for _ in 0..20 {
            frame_at(&ctx, &mut app, narrow, vec![]);
        }

        let view = probe::scroll_view(&ctx).expect("the sidebar scroll area");
        let panel = probe::sidebar_panel_rect(&ctx).expect("the sidebar panel");
        let drawer = probe::editor_drawer_rect(&ctx).expect("the editor drawer");
        let editor = probe::editor_rect(&ctx).expect("the editor content");
        let header = probe::sidebar_header_rect(&ctx, "CARTELLE").expect("the CARTELLE header");
        let save = probe::saved_at(&ctx).expect("the editor save button");
        let cancel = probe::cancel_at(&ctx).expect("the editor cancel button");

        assert!(
            panel.min.x <= drawer.min.x && panel.max.x + 2.0 >= drawer.max.x && panel.min.y <= drawer.min.y && panel.max.y >= drawer.max.y,
            "the drawer stays inside the sidebar: {drawer:?} vs {panel:?}"
        );
        assert!(drawer.min.y <= editor.min.y && drawer.max.y >= editor.max.y, "the editor is fully inside its drawer");
        assert!(view.max.y <= drawer.min.y + 0.5, "the drawer sits below the scrolling list: {view:?} vs {drawer:?}");
        assert!(drawer.max.y <= panel.max.y + 0.5, "the drawer ends inside the sidebar: {drawer:?} vs {panel:?}");
        assert!(drawer.contains_rect(save) && drawer.contains_rect(cancel), "Save and Cancel are inside the drawer");
        assert!(header.min.y >= view.min.y - 1.0 && header.max.y <= view.max.y, "the folder header stays visible at the top");
        assert!(header.min.y <= view.min.y + 4.0, "opening the editor must not push CARTELLE down");
        assert_eq!(probe::scroll_offset(&ctx), Some(offset_before), "opening the editor must not change the list offset");

        // The error line is the tallest editor state; it must still fit without clipping the controls.
        if let Some(editor) = app.label_editor.as_mut() {
            editor.text = "Lavoro".to_owned();
            editor.error = Some("Esiste già un'etichetta con questo nome.".to_owned());
        }
        for _ in 0..4 {
            frame_at(&ctx, &mut app, narrow, vec![]);
        }
        let drawer = probe::editor_drawer_rect(&ctx).expect("the editor drawer with an error");
        let save = probe::saved_at(&ctx).expect("the editor save button with an error");
        assert!(drawer.contains_rect(save), "an error line must not clip Save: {save:?} vs {drawer:?}");

        let content = probe::scroll_content(&ctx).expect("the scroll content size");
        assert!(content.y > view.height(), "with the editor open the sidebar overflows and shows a scroll bar");
        let style = sidebar_scroll_style();
        assert_eq!(style.dormant_handle_opacity, 1.0, "the sidebar bar must be drawn without hovering");
        assert!(style.foreground_color, "its handle must use the text colour, not the panel fill");
        assert!(style.floating_width >= 4.0, "it must be more than a hairline");

        // A single wheel gesture brings Cestino into the list viewport; the drawer does not move.
        frame_at(&ctx, &mut app, narrow, wheel(view.center(), 16));
        frame_at(&ctx, &mut app, narrow, vec![]);
        frame_at(&ctx, &mut app, narrow, vec![]);
        let rows = probe::rows(&ctx);
        let trash = rows.get("Cestino").copied().expect("Cestino is in the sidebar");
        assert!(
            trash.top() >= view.top() - 1.0 && trash.bottom() <= view.bottom() + 1.0,
            "one wheel gesture reaches Cestino while the editor stays open: {trash:?} vs {view:?}"
        );
        let drawer_after = probe::editor_drawer_rect(&ctx).expect("the drawer does not move");
        assert_eq!(drawer, drawer_after, "scrolling the list must not move the drawer");
        let last = rows.get("Fatture").copied().expect("the last label row");
        assert!(last.bottom() > view.bottom(), "the label rows sit below the fold, so only the scroll bar reaches them");

        frame_at(&ctx, &mut app, narrow, wheel(view.center(), 80));
        frame_at(&ctx, &mut app, narrow, vec![]);
        let scrolled = probe::rows(&ctx);
        let fatture = scrolled.get("Fatture").copied().expect("the last label row is still drawn");
        assert!(fatture.bottom() <= view.bottom() + 1.0, "a wheel scroll brings the last label row into view: {fatture:?} vs {view:?}");
    }

    #[test]
    fn composer_sheet_fits_the_minimum_window_and_scrolls_its_body() {
        let (ctx, mut app) = ready_app();
        let narrow = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, narrow, vec![]);
        app.compose_new();
        app.composer.subject = "Preventivo".to_owned();
        app.composer.body = (0..80).map(|line| format!("Riga {line} di una bozza lunga")).collect::<Vec<_>>().join("\n");
        app.composer.touch(0.0);
        for _ in 0..3 {
            frame_at(&ctx, &mut app, narrow, vec![]);
        }

        let content = ctx.content_rect();
        let body_area = Rect::from_min_max(content.min, pos2(content.max.x, content.max.y - ui_size::STATUS));
        let card = probe::composer_card_rect(&ctx).expect("the composer surface");
        assert!(card.width() > 900.0, "below 1000×700 the composer is a sheet: {card:?}");
        assert!(
            card.min.x >= body_area.min.x && card.max.x <= body_area.max.x,
            "the sheet stays inside the content rect horizontally: {card:?} vs {body_area:?}"
        );
        assert!(
            card.min.y >= body_area.min.y && card.max.y <= body_area.max.y,
            "the sheet never goes under the status bar: {card:?} vs {body_area:?}"
        );

        let send = probe::composer_send_rect(&ctx).expect("the Send button");
        let cancel = probe::composer_cancel_rect(&ctx).expect("the Scarta button");
        for (name, rect) in [("Send", send), ("Cancel", cancel)] {
            assert!(
                rect.min.x >= card.min.x && rect.max.x <= card.max.x && rect.min.y >= card.min.y && rect.max.y <= card.max.y,
                "{name} must be inside the sheet: {rect:?} vs {card:?}"
            );
        }

        let (offset, viewport, body) = probe::composer_scroll_state(&ctx).expect("the composer body scroll");
        assert!(body.y > viewport.height() + 1.0, "a long draft makes the body scroll inside the sheet: {body:?} vs {viewport:?}");
        frame_at(&ctx, &mut app, narrow, wheel(viewport.center(), 6));
        frame_at(&ctx, &mut app, narrow, vec![]);
        let (scrolled, _, _) = probe::composer_scroll_state(&ctx).expect("the composer body scroll");
        assert!(scrolled > offset, "the wheel moves the body, not the header or the footer: {scrolled} vs {offset}");
    }

    #[test]
    fn composer_modal_blocks_the_background_and_escape_saves_or_stops() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1280.0, 720.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let row = probe::message_rows(&ctx).values().next().copied().expect("a message row");
        app.compose_new();
        app.composer.subject = "Bozza aperta".to_owned();
        app.composer.touch(0.0);
        for _ in 0..3 {
            frame_at(&ctx, &mut app, wide, vec![]);
        }

        // Left of the centred card is the scrim. The row behind must not open.
        click_at(&ctx, &mut app, pos2(row.left() + 8.0, row.center().y), wide);
        assert!(app.composer.open, "clicking the scrim never closes the composer");
        assert!(app.selected.is_none(), "the scrim swallows the background click");
        assert_eq!(app.composer.subject, "Bozza aperta", "the background Scrivi button cannot replace the open composer");

        // An invalid recipient makes the unsaved guard refuse to close on Esc.
        app.composer.to = "non-una-mail".to_owned();
        app.composer.touch(0.0);
        frame_at(&ctx, &mut app, wide, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.composer.open, "Esc keeps an unsaved, invalid draft open");
        assert!(app.composer.save_error.is_some(), "the composer says why it did not save");
        assert!(!app.engine.messages().iter().any(|message| message.subject == "Bozza aperta"), "a refused save must not create the draft");

        app.composer = ComposerState::closed();
        app.compose_new();
        frame_at(&ctx, &mut app, wide, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.composer.open, "Esc closes an empty, unchanged composer");
    }

    #[test]
    fn composer_focus_enters_the_first_empty_field_and_returns_on_close() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1200.0, 800.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let button = probe::compose_button_rect(&ctx).expect("the toolbar Scrivi button");
        click_at(&ctx, &mut app, button.center(), wide);
        assert!(app.composer.open, "the toolbar button opens the composer");
        let previous = app.composer_return_focus.expect("the widget that opened the composer");
        assert!(ctx.text_edit_focused(), "focus enters the first empty field (A)");

        frame_at(&ctx, &mut app, wide, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.composer.open);
        frame_at(&ctx, &mut app, wide, vec![]);
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(previous), "focus returns to the Scrivi button");
    }

    #[test]
    fn palette_is_modal_and_closes_on_an_outside_click_without_touching_the_list() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1200.0, 800.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let row = probe::message_rows(&ctx).values().next().copied().expect("a message row");
        app.show_palette = true;
        app.palette_focus = true;
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        assert!(app.show_palette);
        assert!(ctx.text_edit_focused(), "the palette keeps the keyboard in its search field");

        // The palette is centred at 560 pt: the row's left edge is outside it.
        click_at(&ctx, &mut app, pos2(row.left() + 4.0, row.center().y), wide);
        assert!(!app.show_palette, "an outside click closes the modal palette");
        assert!(app.selected.is_none(), "the background row never receives the click");
    }

    #[test]
    fn palette_arrows_keep_focus_in_the_search_field() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.show_palette = true;
        app.palette_focus = true;
        frame(&ctx, &mut app, vec![]);
        assert!(ctx.text_edit_focused());
        frame(&ctx, &mut app, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(app.palette_selected, 1, "arrows still move the selected command");
        assert!(ctx.text_edit_focused(), "the field keeps the keyboard after arrow navigation");
    }

    #[test]
    fn palette_fits_the_minimum_window_with_ten_rows() {
        let (ctx, mut app) = ready_app();
        let narrow = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, narrow, vec![]);
        frame_at(&ctx, &mut app, narrow, vec![]);
        app.show_palette = true;
        app.palette_focus = true;
        app.palette_query.clear();
        for _ in 0..3 {
            frame_at(&ctx, &mut app, narrow, vec![]);
        }
        assert!(filtered_commands("").len() > 10, "the empty query needs the ten-row cap");
        let rect = probe::palette_rect(&ctx).expect("the palette surface");
        assert!(rect.width() <= narrow.x - 2.0 * space::XL + 0.5, "the palette is min(560, available - 2*XL): {rect:?}");
        assert!(rect.min.y >= 0.0 && rect.max.y <= narrow.y - ui_size::STATUS, "the palette stays above the status bar: {rect:?}");
        assert!(rect.height() <= 502.0, "ten rows are capped with an internal scroll: {rect:?}");
    }

    #[test]
    fn reading_pane_keeps_the_list_wide_and_tooltips_the_truncated_subject() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1280.0, 720.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        let first = app.visible_messages().0[0].id.clone();
        app.open_message(first);
        frame_at(&ctx, &mut app, wide, vec![]);

        let (content, panel) = probe::list_widths(&ctx).expect("the list width");
        assert!(content >= ui_size::LIST_MIN, "the list content is {content} pt, below the 460 pt floor");
        assert!(panel >= content, "the panel includes the list margin: {panel} vs {content}");
        assert!((panel - 499.0).abs() < 2.0, "the reading split leaves a ~499 pt list at 1280×720, got {panel} (content {content})");
        assert_eq!(probe::date_column_visible(&ctx), Some(true), "a 499 pt list keeps the compact date column");
        let subject = probe::subject_width_value(&ctx).expect("the subject cell width");
        assert!(subject >= 260.0, "the subject gets {subject} pt at a {panel} pt list and {content} pt of content, below the 260 pt floor");

        // A long subject is the case the tooltip exists for. Drafts have a real subject and are a
        // folder the demo can add to deterministically.
        let long = "Un oggetto davvero lungo che non entra nella colonna del soggetto e deve essere troncato";
        let value = app.engine.call("draft_save", json!({ "subject": long, "body_text": "x" })).expect("a long draft");
        let draft = serde_json::from_value::<DraftSave>(value).expect("draft response").draft.summary.id;
        app.select_folder("drafts");
        frame_at(&ctx, &mut app, wide, vec![]);
        app.open_message(draft);
        app.focus_selected_row = true;
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let tips = probe::tooltips(&ctx);
        assert!(tips.iter().any(|tip| tip == long), "a keyboard-focused truncated subject carries its full text as a tooltip: {tips:?}");

        app.close_reading();
        frame_at(&ctx, &mut app, Vec2::new(1600.0, 900.0), vec![]);
        assert_eq!(probe::date_column_visible(&ctx), Some(true), "the date returns once the list has room");
    }

    #[test]
    fn double_click_opens_and_expands_and_the_reading_header_restores() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1200.0, 800.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let id = app.visible_messages().0[0].id.clone();
        let row = probe::message_rows(&ctx).get(&id).copied().expect("the first message row");
        double_click_at(&ctx, &mut app, pos2(row.left() + 30.0, row.center().y), wide);
        assert!(app.reading_open, "a row double-click opens the message");
        assert!(app.reading_full, "a row double-click expands the reading pane on a wide window");

        frame_at(&ctx, &mut app, wide, vec![]);
        let header = probe::reading_header_rect(&ctx).expect("the reading header");
        double_click_at(&ctx, &mut app, header.center(), wide);
        assert!(app.reading_open && !app.reading_full, "a header double-click restores the split view");

        // On a narrow window the same gesture uses the full-width overlay and the header closes it.
        app.close_reading();
        let narrow = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, narrow, vec![]);
        frame_at(&ctx, &mut app, narrow, vec![]);
        frame_at(&ctx, &mut app, narrow, vec![]);
        let row = probe::message_rows(&ctx).get(&id).copied().expect("the first message row");
        // On a narrow window the first click already replaces the list with the full-width overlay;
        // keep the gesture away from the label chips the overlay shows at its left edge.
        let pos = pos2(row.left() + row.width() * 0.6, row.center().y);
        double_click_at(&ctx, &mut app, pos, narrow);
        assert!(app.reading_open, "a narrow row double-click opens the overlay");
        frame_at(&ctx, &mut app, narrow, vec![]);
        let header = probe::reading_header_rect(&ctx).expect("the overlay header");
        double_click_at(&ctx, &mut app, header.center(), narrow);
        assert!(!app.reading_open, "a header double-click on the overlay returns to the list");
    }

    fn conversations_app() -> (egui::Context, PostinoApp) {
        let (ctx, mut app) = ready_app();
        app.threads_view = true;
        app.threads_cache = None;
        (ctx, app)
    }

    #[test]
    fn conversation_rows_are_a_real_surface_and_the_toggle_returns_to_messages() {
        let (ctx, mut app) = conversations_app();
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let rows = probe::rows(&ctx);
        assert!(rows.contains_key("thread:thr_preventivo"), "the conversation row was not painted: {:?}", rows.keys().collect::<Vec<_>>());
        assert!(!rows.keys().any(|key| key.starts_with("msg_")), "the flat rows must not be painted in the threaded view");
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Preventivo")), "the thread subject is missing: {text:?}");
        assert!(text.iter().any(|line| line.contains("3 messaggi")), "the thread count is missing: {text:?}");

        app.toggle_threads_view();
        assert!(!app.threads_view);
        assert!(app.selected_thread.is_none());
        app.close_reading();
        assert!(!app.reading_open && !app.reading_full);
        assert!(!app.visible_messages().0.is_empty(), "the flat view has no messages");
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let flat = probe::message_rows(&ctx);
        assert!(flat.keys().any(|key| key.starts_with("msg_")), "the flat message table did not come back: {:?}", flat.keys().collect::<Vec<_>>());
    }

    #[test]
    fn a_first_run_demo_can_return_to_onboarding_and_settings() {
        let mut app = booted_app("demo-return-onboarding", Vec::new(), |_| {});
        assert!(app.onboarding.is_some());
        app.open_demo();
        assert!(app.demo && app.demo_can_return());
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(980.0, 600.0);
        settle_at(&ctx, &mut app, size);
        let back = probe::demo_return_button_rect(&ctx).expect("the demo status bar offers a way back");
        click_at(&ctx, &mut app, back.center(), size);
        frame_at(&ctx, &mut app, size, Vec::new());
        assert!(!app.demo, "the demo exit did not reopen the store");
        assert!(app.store.is_some());
        assert!(app.onboarding.is_some(), "an empty store returns to first-run onboarding");
        assert_eq!(app.onboarding.as_ref().map(Onboarding::stage), Some(Stage::Welcome));

        // Settings is the second visible path; it must not dead-end in the demo either.
        let mut app = booted_app("demo-return-settings", Vec::new(), |_| {});
        app.open_demo();
        app.open_settings(&ctx, None);
        settle_at(&ctx, &mut app, size);
        let leave = probe::settings_action_rect(&ctx, "leave_demo").expect("Settings offers the archive return");
        click_at(&ctx, &mut app, leave.center(), size);
        frame_at(&ctx, &mut app, size, Vec::new());
        assert!(!app.demo && app.store.is_some() && app.onboarding.is_some());
    }

    #[test]
    fn a_recovery_demo_can_return_to_the_original_store() {
        let mut app = booted_app("demo-return-recovery", Vec::new(), |_| {});
        app.boot_failure = Some(BootFailure {
            path: Some(PathBuf::from("C:/Users/demo/AppData/Local/Postino/mail.db")),
            stage: BootStage::Keyring,
            reason: "locked".to_owned(),
        });
        app.onboarding = None;
        app.open_demo();
        assert!(app.demo && app.boot_failure.is_none());
        app.leave_demo();
        assert!(!app.demo);
        assert!(app.boot_failure.is_none(), "the retained handle reopens without asking the keyring again");
        assert!(app.store.is_some());
        assert!(app.onboarding.is_some(), "the store still has no account");
    }

    #[test]
    fn a_conversation_attachment_save_stays_inside_the_980_window_in_both_themes() {
        for dark in [false, true] {
            let clock = screenshot_clock().expect("the frozen screenshot clock");
            let mut app = PostinoApp::demo_at(true, clock);
            app.seed_screenshot(ScreenshotState::Conversation);
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            let size = Vec2::new(980.0, 600.0);
            settle_at(&ctx, &mut app, size);
            let content = ctx.content_rect();
            let strip = probe::reading_attachments_rect(&ctx).expect("the conversation attachment strip");
            let save = probe::attachment_save_rect(&ctx, "att_001").expect("the attachment Save");
            let body = probe::reading_body_rect(&ctx).expect("the reading body");
            assert!(content.contains_rect(strip), "dark={dark}: strip {strip:?} leaves the window");
            assert!(content.contains_rect(save), "dark={dark}: Save {save:?} leaves the window");
            assert!(strip.contains_rect(save), "dark={dark}: Save {save:?} is outside the strip {strip:?}");
            assert!(strip.height() >= 80.0, "dark={dark}: the strip cannot show its row: {strip:?}");
            assert!(body.bottom() <= strip.top(), "dark={dark}: the body viewport overlaps the strip: {body:?} {strip:?}");
            assert!(save.bottom() <= size.y - ui_size::STATUS, "dark={dark}: Save is under the status bar: {save:?}");
        }
    }

    #[test]
    fn the_composer_keeps_its_640_card_and_the_footer_stays_separate_at_1280() {
        for dark in [false, true] {
            let clock = screenshot_clock().expect("the frozen screenshot clock");
            let mut app = PostinoApp::demo_at(true, clock);
            app.seed_screenshot(ScreenshotState::Compose);
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            let size = Vec2::new(1280.0, 720.0);
            settle_at(&ctx, &mut app, size);
            let card = probe::composer_card_rect(&ctx).expect("the composer card");
            let surface = probe::composer_surface_rect(&ctx).expect("the composer surface");
            let send = probe::composer_send_rect(&ctx).expect("the Send button");
            let cancel = probe::composer_cancel_rect(&ctx).expect("Scarta");
            let state = probe::composer_footer_state_rect(&ctx).expect("the saved state");
            assert!((card.width() - ui_size::COMPOSER_CARD_WIDTH).abs() < 1.0, "dark={dark}: card {card:?}");
            assert!(surface.width() <= card.width() + 4.5, "dark={dark}: the modal grew past its card: {surface:?} {card:?}");
            assert!(surface.contains_rect(card), "dark={dark}: the card escaped the modal area: {surface:?} {card:?}");
            assert!((card.center().x - ctx.content_rect().center().x).abs() < 1.0, "dark={dark}: {card:?}");
            assert!(card.contains_rect(send), "dark={dark}: Send {send:?} is outside {card:?}");
            assert!(card.contains_rect(cancel), "dark={dark}: Scarta {cancel:?} is outside {card:?}");
            assert!(state.right() <= cancel.left(), "dark={dark}: saved state {state:?} runs under Scarta {cancel:?}");
        }
    }

    #[test]
    fn the_outbox_card_keeps_its_footer_inside_at_980_in_both_themes() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            let mut app = PostinoApp::demo_at(true, Utc::now());
            app.outbox = demo_outbox(app.now_utc.timestamp_millis());
            app.outbox_open = true;
            app.outbox_loaded = true;
            let size = Vec2::new(980.0, 600.0);
            settle_at(&ctx, &mut app, size);
            let card = probe::outbox_card_rect(&ctx).expect("the outbox card");
            let footer = probe::outbox_footer_rect(&ctx).expect("the outbox footer");
            assert!(ctx.content_rect().contains_rect(card), "dark={dark}: card {card:?}");
            assert!(card.contains_rect(footer), "dark={dark}: footer {footer:?} is outside card {card:?}");
            assert!(card.bottom() <= size.y - ui_size::STATUS, "dark={dark}: the card reaches the status bar: {card:?}");
            let text = painted_text(&ctx, &mut app, size);
            assert!(text.iter().any(|line| line.contains("richiede Postino aperto")), "dark={dark}: {text:?}");
            assert!(text.iter().any(|line| line.contains("Postino invia solo mentre")), "dark={dark}: {text:?}");
        }
    }

    #[test]
    fn the_threaded_filter_empty_states_offer_the_resolver_action() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let cases = [
            (ScreenshotState::EmptyUnread, "Nessuna conversazione non letta", "Mostra tutti", EmptyAction::ShowAll),
            (ScreenshotState::EmptyAttachments, "Nessuna conversazione con allegati", "Mostra tutti", EmptyAction::ShowAll),
            (
                ScreenshotState::EmptyLabelUnread,
                "Nessuna conversazione non letta con l'etichetta “Lavoro”",
                "Togli filtro “non letti”",
                EmptyAction::ClearUnread,
            ),
        ];
        for (state, title, primary, action) in cases {
            let mut app = PostinoApp::demo(true);
            app.seed_screenshot(state);
            app.threads_view = true;
            app.threads_cache = None;
            app.list_cache = None;
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            let view = probe::empty_state_view(&ctx).expect("the threaded empty state");
            assert_eq!(view.title, title, "{state:?}");
            assert_eq!(view.primary.as_deref(), Some(primary), "{state:?}");
            assert!(!text.iter().any(|line| line.contains("Nessun messaggio")), "{state:?}: flat copy leaked: {text:?}");
            assert!(!view.body.contains("è vuota"), "{state:?} must not blame the folder");
            app.apply_empty_action(action);
            assert!(!app.visible_threads().0.is_empty(), "{state:?}: the way out revealed no conversation");
        }
    }

    #[test]
    fn a_threaded_parse_error_does_not_also_render_the_empty_state() {
        let (ctx, mut app) = ready_app();
        app.query = "subject:\"unclosed".to_owned();
        app.commit_search();
        app.threads_view = true;
        app.threads_cache = None;
        let _ = app.visible_threads();
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Ricerca non valida")), "{text:?}");
        assert!(!text.iter().any(|line| line.contains("Nessuna conversazione")), "the invalid query showed a threaded empty state: {text:?}");
    }

    #[test]
    fn opening_a_message_refreshes_the_conversation_unread_count() {
        let mut app = PostinoApp::demo(true);
        app.threads_view = true;
        app.select_thread("thr_preventivo".to_owned());
        let before = app.thread_summary.as_ref().expect("the thread summary").unread_count;
        let open = app.thread_messages.iter().find(|message| message.unread).expect("an unread card").id.clone();
        app.mark_read_after_delay(&open);
        let after = app.thread_summary.as_ref().expect("the refreshed summary").unread_count;
        assert_eq!(after + 1, before, "the banner kept the stale unread count");
        assert!(
            app.thread_messages.iter().find(|message| message.id == open).is_some_and(|message| !message.unread),
            "the loaded card still claims to be unread"
        );
    }

    #[test]
    fn thread_rows_show_their_star_and_the_menu_matches_it() {
        let (ctx, mut app) = conversations_app();
        let wide = Vec2::new(1280.0, 720.0);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let (starred, row) = probe::thread_star_state(&ctx, "thr_preventivo").expect("the starred thread row");
        assert!(starred, "the Preventivo thread has a starred message and must show it");
        assert!(ctx.content_rect().contains_rect(row));
        let other = app
            .visible_threads()
            .0
            .into_iter()
            .find(|thread| thread.id != "thr_preventivo" && thread.starred_count == 0)
            .expect("an unstarred thread")
            .id;
        let (unstarred, _) = probe::thread_star_state(&ctx, &other).expect("an unstarred thread row");
        assert!(!unstarred);
        assert_eq!(thread_star_menu(true), ("unstar", false));
        assert_eq!(thread_star_menu(false), ("star", true));
        assert_eq!(command_label("unstar", "x"), "Togli stella");
        assert_eq!(command_label("star", "x"), "Aggiungi stella");
    }

    #[test]
    fn clearing_a_search_reconnects_the_open_hit_to_its_thread() {
        let mut app = PostinoApp::demo(true);
        app.threads_view = true;
        app.query = "from:giulia".to_owned();
        app.commit_search();
        let hit = app.visible_messages().0.first().expect("a search hit").id.clone();
        app.open_message(hit.clone());
        assert!(!app.threads_view && app.selected_thread.is_none());
        app.clear_search();
        assert!(app.threads_view, "the folder returns to conversations");
        assert!(app.reading_open, "the open hit stays readable");
        assert_eq!(app.selected.as_deref(), Some(hit.as_str()));
        assert_eq!(app.thread_open.as_deref(), Some(hit.as_str()));
        let thread = app.selected_thread.as_deref().expect("the hit's thread is selected");
        assert!(app.thread_messages.iter().any(|message| message.id == hit && message.thread_id == thread));
    }

    #[test]
    fn the_shell_paints_one_product_wordmark() {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        app.seed_screenshot(ScreenshotState::Shell);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        for size in [Vec2::new(980.0, 600.0), Vec2::new(1280.0, 720.0)] {
            let text = painted_text(&ctx, &mut app, size);
            assert_eq!(text.iter().filter(|line| line.as_str() == "postino").count(), 1, "two product marks at {size:?}: {text:?}");
        }
    }

    #[test]
    fn attachment_sizes_use_the_italian_decimal_comma() {
        assert_eq!(format_size(248_320), "242,5 KB");
    }

    #[test]
    fn the_conversation_opens_the_latest_unread_card_and_jk_moves_it() {
        let (ctx, mut app) = conversations_app();
        app.select_thread("thr_preventivo".to_owned());
        assert!(app.reading_open);
        assert_eq!(app.thread_messages.len(), 3);
        assert_eq!(app.thread_total, 3);
        assert_eq!(app.selected.as_deref(), Some("msg_001"), "the latest unread message is the open card");
        assert_eq!(app.thread_open.as_deref(), Some("msg_001"));
        app.step_selection(-1, false);
        assert_eq!(app.selected.as_deref(), Some("msg_002"), "K moves to the previous card");
        assert_eq!(app.thread_open.as_deref(), Some("msg_002"));

        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("CONVERSAZIONE")), "the conversation banner is missing at 980: {text:?}");
        assert!(text.iter().any(|line| line.contains("Preventivo")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("Giulia")), "the sender cards are missing: {text:?}");
    }

    #[test]
    fn a_thread_archive_moves_every_card_in_one_undo() {
        let mut app = PostinoApp::demo(true);
        app.threads_view = true;
        app.select_thread("thr_preventivo".to_owned());
        app.move_selected(FolderKind::Archive, "Conversazione archiviata", "Conversazioni archiviate");
        for id in ["msg_001", "msg_002", "msg_003"] {
            assert_eq!(app.engine.message(id).map(|message| message.summary.folder_id), Some("archive".to_owned()), "{id}");
        }
        assert!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some(), "the archive must offer one undo");
        app.undo_last();
        assert_eq!(app.engine.message("msg_001").map(|message| message.summary.folder_id), Some("inbox".to_owned()));
        assert_eq!(app.engine.message("msg_002").map(|message| message.summary.folder_id), Some("sent".to_owned()));
        assert_eq!(app.engine.message("msg_003").map(|message| message.summary.folder_id), Some("inbox".to_owned()));
    }

    #[test]
    fn a_thread_flag_marks_every_card_and_an_idempotent_repeat_changes_nothing() {
        let mut app = PostinoApp::demo(true);
        app.threads_view = true;
        app.select_thread("thr_preventivo".to_owned());
        app.mark_unread();
        assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
        app.toggle_read();
        assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| !message.summary.unread)));
        app.toggle_read();
        assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
    }

    #[test]
    fn search_is_flat_until_the_user_toggles_conversations() {
        let (_ctx, mut app) = conversations_app();
        app.query = "preventivo".to_owned();
        app.commit_search();
        assert!(!app.threads_view, "a new search starts flat");
        app.toggle_threads_view();
        assert!(app.threads_view);
        let (threads, total, _, _) = app.visible_threads();
        assert_eq!(total, 1);
        assert_eq!(threads[0].id, "thr_preventivo");
        app.clear_search();
        assert!(app.threads_view, "clearing the search restores the folder's threaded default");
    }

    #[test]
    fn a_saved_search_clears_the_label_keeps_folder_scope_only_with_folder_and_esc_restores() {
        let (_ctx, mut app) = conversations_app();
        app.select_label("lbl_1".to_owned());
        app.query = "preventivo".to_owned();
        app.commit_search();
        app.engine.call("saved_search_create", json!({ "name": "Da Giulia", "query": "from:giulia" })).expect("save");
        app.refresh_saved_searches();
        let saved = app.saved_searches.iter().find(|saved| saved.name == "Da Giulia").cloned().expect("saved");
        let folder_before = app.folder.clone();
        let query_before = app.query.clone();
        let threads_before = app.threads_view;
        app.apply_saved_search(&saved.id);
        assert_eq!(app.query, "from:giulia");
        assert!(app.label_filter.is_none(), "applying a saved search clears the label filter");
        assert!(app.saved_folder_scope);
        assert!(!app.threads_view, "a saved search is a search view and starts flat");
        assert_eq!(app.applied_saved.as_deref(), Some(saved.id.as_str()));
        assert_eq!(app.folder, folder_before);
        app.cancel_search();
        assert_eq!(app.query, query_before);
        assert_eq!(app.label_filter.as_deref(), Some("lbl_1"));
        assert!(app.applied_saved.is_none());
        assert_eq!(app.threads_view, threads_before, "the view mode before the saved search comes back");

        app.clear_search();
        assert!(app.threads_view, "the empty search restores the folder's threaded default");
        app.apply_saved_search(&saved.id);
        assert!(!app.threads_view);
        app.cancel_search();
        assert!(app.threads_view, "Esc restores the folder view when the saved search started from it");

        app.engine.call("saved_search_create", json!({ "name": "Inviati", "query": "folder:sent" })).expect("folder query");
        app.refresh_saved_searches();
        let folder_saved = app.saved_searches.iter().find(|saved| saved.name == "Inviati").cloned().expect("saved");
        app.folder = "inbox".to_owned();
        app.apply_saved_search(&folder_saved.id);
        let (rows, _, _) = app.visible_messages();
        assert!(!rows.is_empty(), "the saved folder query returned nothing");
        assert!(rows.iter().all(|message| message.folder_id == "sent"), "the saved query's folder is the only folder filter");
    }

    #[test]
    fn saved_search_delete_offers_an_undo_that_restores_the_row() {
        let (_ctx, mut app) = conversations_app();
        app.engine.call("saved_search_create", json!({ "name": "Fatture", "query": "label:Fatture" })).expect("save");
        app.refresh_saved_searches();
        let id = app.saved_searches[0].id.clone();
        app.delete_saved_search(&id);
        assert!(app.saved_searches.is_empty());
        app.undo_last();
        assert_eq!(app.saved_searches.len(), 1);
        assert_eq!(app.saved_searches[0].name, "Fatture");
    }

    #[test]
    fn saved_search_rename_is_undoable() {
        let (_ctx, mut app) = conversations_app();
        app.engine.call("saved_search_create", json!({ "name": "Prima", "query": "is:unread" })).expect("save");
        app.refresh_saved_searches();
        let id = app.saved_searches[0].id.clone();
        app.start_saved_search_rename(&id);
        if let Some(editor) = app.saved_editor.as_mut() {
            editor.name = "Dopo".to_owned();
        }
        app.commit_saved_search_editor();
        assert_eq!(app.saved_searches[0].name, "Dopo");
        app.undo_last();
        assert_eq!(app.saved_searches[0].name, "Prima");
    }

    #[test]
    fn saved_search_drawer_saves_the_current_query_and_rejects_an_empty_name() {
        let (ctx, mut app) = conversations_app();
        app.query = "is:unread".to_owned();
        app.commit_search();
        app.start_saved_search_save();
        assert!(app.saved_editor.is_some());
        if let Some(editor) = app.saved_editor.as_mut() {
            editor.name.clear();
        }
        app.commit_saved_search_editor();
        assert!(app.saved_searches.is_empty(), "an empty name writes nothing");
        assert!(app.saved_editor.as_ref().and_then(|editor| editor.error.as_deref()).is_some());
        if let Some(editor) = app.saved_editor.as_mut() {
            editor.name = "Non letti".to_owned();
        }
        app.commit_saved_search_editor();
        assert_eq!(app.saved_searches.len(), 1);
        assert_eq!(app.saved_searches[0].name, "Non letti");
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("RICERCHE")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("Non letti")), "{text:?}");
    }

    #[test]
    fn the_thread_and_saved_scenes_render_at_980_light_and_dark() {
        for dark in [false, true] {
            let clock = screenshot_clock().expect("the frozen screenshot clock");
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            for (state, expected) in [
                (ScreenshotState::Threads, "Preventivo"),
                (ScreenshotState::Conversation, "CONVERSAZIONE"),
                (ScreenshotState::SavedSearches, "RICERCHE"),
            ] {
                let mut app = PostinoApp::demo_at(true, clock);
                app.dark = dark;
                app.seed_screenshot(state);
                frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
                let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
                assert!(text.iter().any(|line| line.contains(expected)), "{state:?} at 980 (dark={dark}) is missing {expected:?}: {text:?}");
            }
        }
    }

    #[test]
    fn keyboard_only_thread_flow_moves_opens_a_card_and_closes() {
        let (ctx, mut app) = conversations_app();
        frame(&ctx, &mut app, vec![key(Key::J, Modifiers::NONE)]);
        assert_eq!(app.selected_thread.as_deref(), Some("thr_preventivo"));
        assert!(!app.reading_open, "J only selects the thread row");
        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.reading_open);
        assert_eq!(app.selected.as_deref(), Some("msg_001"));
        assert_eq!(app.thread_messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), vec!["msg_003", "msg_002", "msg_001"]);
        frame(&ctx, &mut app, vec![key(Key::K, Modifiers::NONE)]);
        assert_eq!(app.selected.as_deref(), Some("msg_002"));
        assert_eq!(app.thread_open.as_deref(), Some("msg_002"));
        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.reading_open);
        assert_eq!(app.selected_thread.as_deref(), Some("thr_preventivo"), "Esc goes back to the list, not away from the thread");
    }

    #[test]
    fn thread_actions_after_j_target_the_selected_conversation() {
        let mut app = PostinoApp::demo(true);
        app.threads_view = true;
        app.select_thread("thr_preventivo".to_owned());
        app.close_reading();
        app.step_selection(1, false);
        let target = app.selected_thread.clone().expect("a second thread");
        assert_ne!(target, "thr_preventivo");
        let target_ids = app.thread_messages.iter().map(|message| message.id.clone()).collect::<Vec<_>>();
        assert!(!target_ids.is_empty());
        app.move_selected(FolderKind::Archive, "Conversazione archiviata", "Conversazioni archiviate");
        assert!(target_ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id == "archive")));
        assert!(
            ["msg_001", "msg_002", "msg_003"].iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id != "archive"))
        );
        assert!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some());
        app.undo_last();
        assert!(target_ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id != "archive")));
    }

    #[test]
    fn thread_flag_after_j_updates_the_page_and_offers_undo_each_time() {
        let mut app = PostinoApp::demo(true);
        app.threads_view = true;
        app.select_thread("thr_preventivo".to_owned());
        app.close_reading();
        app.mark_unread();
        assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
        app.toggle_read();
        assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| !message.summary.unread)));
        app.toggle_read();
        assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
        let undo_steps = app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some();
        assert!(undo_steps, "the third flag must still offer undo");
        app.set_starred(true);
        assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.starred)));
    }

    #[test]
    fn enter_opens_the_selected_card_and_reply_targets_it() {
        let (ctx, mut app) = conversations_app();
        app.select_thread("thr_preventivo".to_owned());
        app.step_thread_message(-1);
        assert_eq!(app.selected.as_deref(), Some("msg_002"));
        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(app.selected.as_deref(), Some("msg_002"), "Enter must not jump back to the latest unread");
        assert_eq!(app.thread_open.as_deref(), Some("msg_002"));
        frame(&ctx, &mut app, vec![key(Key::R, Modifiers::NONE)]);
        assert!(app.composer.open, "R opens the reply composer");
        assert!(app.composer.in_reply_to.as_deref().is_some_and(|parent| parent.contains("msg_002")), "{:?}", app.composer.in_reply_to);
    }

    #[test]
    fn a_partial_batch_move_still_offers_undo_for_the_moved_messages() {
        let mut app = PostinoApp::demo(true);
        app.threads_view = false;
        app.move_many(&["msg_001".to_owned(), "missing".to_owned()], "archive", "Messaggio archiviato", "Messaggi archiviati");
        assert_eq!(app.engine.message("msg_001").map(|message| message.summary.folder_id), Some("archive".to_owned()));
        assert!(app.failure.is_some(), "the missing row is reported");
        assert!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some(), "the moved message is undoable");
        app.undo_last();
        assert_eq!(app.engine.message("msg_001").map(|message| message.summary.folder_id), Some("inbox".to_owned()));
    }

    #[test]
    fn a_large_thread_opens_on_its_newest_page_and_loads_older_messages() {
        let mut app = booted_app("p9b-large-thread", Vec::new(), |store| {
            seed_account_rows(store, "acc_big", "big@example.com", None);
            for index in 0..250u32 {
                let id = format!("msg_big_{index:03}");
                let mut row = store_message("fld_acc_big_inbox", "acc_big", &id, &format!("Discussione {index}"));
                row.summary.thread_id = "big@example.com".to_owned();
                row.summary.date = 1_700_000_000_000 + i64::from(index);
                row.message_id = Some(format!("<big-{index}@example.com>"));
                store.upsert_synced_message_with_refs(&row, &[], Some("big@example.com"), &["big@example.com".to_owned()]).expect("message");
            }
            store.recount_folder("fld_acc_big_inbox").expect("counts");
        });
        app.threads_view = true;
        app.folder = "fld_acc_big_inbox".to_owned();
        app.thread_list_limit = 200;
        app.select_thread("big@example.com".to_owned());
        assert_eq!(app.thread_total, 250);
        assert_eq!(app.thread_messages.len(), 200);
        assert_eq!(app.thread_offset, 50);
        assert_eq!(app.selected.as_deref(), Some("msg_big_249"));
        app.load_older_thread_messages();
        assert_eq!(app.thread_messages.len(), 250);
        assert_eq!(app.thread_offset, 0);
    }

    #[test]
    fn a_large_thread_list_loads_the_next_page() {
        let mut app = booted_app("p9b-large-list", Vec::new(), |store| {
            seed_account_rows(store, "acc_list", "list@example.com", None);
            for index in 0..200u32 {
                let id = format!("msg_thread_{index:03}");
                let mut row = store_message("fld_acc_list_inbox", "acc_list", &id, &format!("Filo {index}"));
                row.summary.thread_id = format!("thread-{index}");
                row.summary.date = 1_700_000_000_000 + i64::from(index);
                store.upsert_message(&row).expect("message");
            }
            store.recount_folder("fld_acc_list_inbox").expect("counts");
        });
        app.threads_view = true;
        app.folder = "fld_acc_list_inbox".to_owned();
        app.thread_list_limit = 200;
        let (rows, total, _, _) = app.visible_threads();
        assert_eq!(total, 201);
        assert_eq!(rows.len(), 200);
        app.thread_list_limit = 400;
        app.threads_cache = None;
        let (rows, total, _, _) = app.visible_threads();
        assert_eq!(total, 201);
        assert_eq!(rows.len(), 201);
    }

    #[test]
    fn deleting_a_label_used_by_a_saved_search_marks_it_invalid_on_the_next_refresh() {
        let (_ctx, mut app) = conversations_app();
        app.engine.call("saved_search_create", json!({ "name": "Fatture", "query": "label:Fatture" })).expect("save");
        app.refresh_saved_searches();
        assert!(app.saved_searches[0].valid);
        app.engine.call("label_delete", json!({ "id": "lbl_3" })).expect("delete label");
        app.refresh_saved_searches_if_stale();
        assert!(!app.saved_searches[0].valid);
        assert!(!app.saved_searches[0].index_pending);
    }

    #[test]
    fn applying_an_account_wide_saved_search_clears_chips_and_names_the_scope() {
        let (ctx, mut app) = conversations_app();
        app.unread_only = true;
        app.engine.call("saved_search_create", json!({ "name": "Da Giulia", "query": "from:giulia" })).expect("save");
        app.refresh_saved_searches();
        let saved = app.saved_searches[0].clone();
        app.apply_saved_search(&saved.id);
        assert!(!app.unread_only && !app.attachments_only, "the saved search replaces the chips too");
        assert!(app.saved_folder_scope && !app.saved_query_folder);
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Tutte le mail")), "the account-wide scope is not named: {text:?}");
    }

    #[test]
    fn removing_an_account_prunes_its_saved_and_recent_ui_rows() {
        let mut app = booted_app("p9b-remove-account", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        app.active_account = Some("acc_a".to_owned());
        app.engine.call("saved_search_create", json!({ "account": "acc_a", "name": "Segreta", "query": "is:unread" })).expect("save");
        app.engine.record_recent_search(Some("acc_a"), "subject:segreto").expect("recent");
        app.refresh_saved_searches();
        assert_eq!(app.saved_searches.len(), 1);
        assert_eq!(app.recent_searches.len(), 1);
        app.selected_thread = Some("thr_x".to_owned());
        app.engine.call("account_remove", json!({ "id": "acc_a", "confirm": true })).expect("remove");
        app.after_account_removal("acc_a", None);
        assert!(app.saved_searches.is_empty(), "the removed account's saved searches stay in the sidebar");
        assert!(app.recent_searches.is_empty(), "the removed account's recent chips stay");
        assert!(app.selected_thread.is_none());
        assert_eq!(app.active_account.as_deref(), Some("acc_b"));
    }

    fn test_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join("opencode").join("p6c-fix2-postino").join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&path);
        let _ = std::fs::remove_file(path.join("mail.db"));
        let _ = std::fs::remove_dir_all(path.join("mail.db.blobs"));
        path
    }

    fn fresh_store(name: &str) -> (PathBuf, Arc<postino_store::StoreKey>, Arc<MemoryVault>) {
        let directory = test_directory(name);
        let path = directory.join("mail.db");
        (path, Arc::new(postino_store::StoreKey::from_bytes([0x5a; 32])), MemoryVault::new())
    }

    fn store_message(folder: &str, account_id: &str, id: &str, subject: &str) -> StoreMessage {
        StoreMessage {
            summary: StoreMessageSummary {
                id: id.to_owned(),
                account_id: account_id.to_owned(),
                folder_id: folder.to_owned(),
                thread_id: format!("thr_{id}"),
                from: StoreAddress::new("Mittente", "mittente@example.com"),
                to: vec![StoreAddress::new("Postino Test", "test@example.com")],
                subject: subject.to_owned(),
                snippet: format!("{subject} — anteprima"),
                date: 1_700_000_000_000,
                unread: true,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            uid: None,
            message_id: None,
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: format!("{subject}\n\nCorpo salvato nell'archivio locale."),
            body_html: None,
            remote_images: 0,
            size: None,
        }
    }

    fn seed_attachment(store: &postino_store::Store, message_id: &str, id: &str, filename: &str) {
        store
            .replace_attachments(
                message_id,
                &[StoreAttachment {
                    id: id.to_owned(),
                    filename: filename.to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 11,
                    blob_hash: None,
                }],
            )
            .expect("attachment row");
    }

    /// Seed one account, its inbox folder and one local message. `port` adds the IMAP server row.
    fn seed_account_rows(store: &postino_store::Store, id: &str, email: &str, port: Option<u16>) {
        store
            .upsert_account(&StoreAccount {
                id: id.to_owned(),
                name: format!("Account {id}"),
                email: email.to_owned(),
                provider: "custom".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account row");
        if let Some(port) = port {
            store
                .upsert_account_server(&AccountServer {
                    account_id: id.to_owned(),
                    host: "localhost".to_owned(),
                    port,
                    tls: AccountTls::Implicit,
                    username: email.to_owned(),
                    created_at: 1_700_000_000_000,
                    smtp: None,
                })
                .expect("server row");
        }
        let folder = format!("fld_{id}_inbox");
        for (suffix, name, kind) in [
            ("inbox", "Posta in arrivo", StoreFolderKind::Inbox),
            ("sent", "Inviati", StoreFolderKind::Sent),
            ("drafts", "Bozze", StoreFolderKind::Drafts),
            ("trash", "Cestino", StoreFolderKind::Trash),
            ("archive", "Archivio", StoreFolderKind::Archive),
            ("starred", "Preferiti", StoreFolderKind::Starred),
        ] {
            store
                .upsert_folder(&StoreFolder {
                    id: format!("fld_{id}_{suffix}"),
                    account_id: id.to_owned(),
                    name: name.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                })
                .expect("folder row");
        }
        store.upsert_message(&store_message(&folder, id, &format!("msg_{id}_1"), &format!("Messaggio di {id}"))).expect("message row");
    }

    fn booted_app(name: &str, roots: Vec<CertificateDer<'static>>, seed: impl FnOnce(&postino_store::Store)) -> PostinoApp {
        let (path, key, vault) = fresh_store(name);
        {
            let store = postino_store::Store::open(&path, key.as_ref()).expect("seed store");
            seed(&store);
        }
        let vault_dyn: Arc<dyn CredentialVault> = vault;
        let booted =
            boot::boot(Launch::Store(Some(path.clone())), KeySource::Fixed { key: key.clone(), vault: vault_dyn }, Utc::now()).expect("boot");
        let mut app = PostinoApp::from_booted(booted, true, Utc::now(), Launch::Store(Some(path)), roots);
        // Legacy store tests exercise the flat table; P9b tests opt in to the threaded default.
        app.threads_view = false;
        app
    }

    fn wait_for_sync(app: &mut PostinoApp) {
        for _ in 0..1500 {
            app.handle_sync_events();
            if !app.sync_running() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the sync did not finish");
    }

    fn set_test_password(app: &PostinoApp, account: &str) {
        let store = app.store.as_ref().expect("a store-backed app");
        store.vault().store_imap_password(account, postino_testkit::PASSWORD.as_bytes()).expect("the test vault stores the password");
    }

    #[test]
    fn an_empty_store_boots_into_onboarding_not_a_fake_mailbox() {
        let mut app = booted_app("app-empty-store", Vec::new(), |_| {});
        assert!(!app.demo);
        assert!(app.engine.accounts().is_empty());
        assert!(app.onboarding.is_some());
        assert_eq!(app.onboarding.as_ref().map(Onboarding::stage), Some(Stage::Welcome));
        assert_eq!(app.onboarding.as_ref().map(Onboarding::mode), Some(onboarding::Mode::FirstRun));
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        let text = output.shapes.iter().any(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => text.galley.text().contains("Aggiungi il tuo account"),
            _ => false,
        });
        output.textures_delta.clear();
        assert!(text, "the welcome screen must offer the first-run action");
    }

    #[test]
    fn a_store_failure_renders_the_recovery_screen() {
        let mut app = PostinoApp::with_engine(
            Engine::empty(),
            None,
            false,
            Some(BootFailure {
                path: Some(PathBuf::from("C:/Users/demo/AppData/Local/Postino/mail.db")),
                stage: BootStage::Keyring,
                reason: "the keyring is locked".to_owned(),
            }),
            Launch::Store(None),
            true,
            Utc::now(),
            Vec::new(),
        );
        assert_eq!(app.status_kind(), StatusKind::Error);
        assert!(app.onboarding.is_none());
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        let text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        output.textures_delta.clear();
        assert!(text.iter().any(|line| line.contains("portachiavi")), "the recovery screen must name the keyring: {text:?}");
        assert!(!text.iter().any(|line| line.contains("the keyring is locked")), "a raw engine sentence reached the recovery card: {text:?}");
        app.open_demo();
        assert!(app.demo);
        assert!(app.boot_failure.is_none());
        assert!(app.engine.is_demo());
    }

    #[test]
    fn demo_is_still_a_labelled_fallback() {
        let app = PostinoApp::demo(true);
        assert!(app.demo);
        assert_eq!(app.status_kind(), StatusKind::Demo);
        assert!(app.sync_text().is_none(), "the demo has no sync state to claim");
    }

    #[test]
    fn switching_and_removing_accounts_keeps_data_and_credentials_isolated() {
        let mut app = booted_app("app-accounts", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        assert_eq!(app.engine.accounts().len(), 2);
        assert_eq!(app.active_account.as_deref(), Some("acc_a"));
        assert!(app.visible_messages().0.iter().any(|message| message.subject.contains("acc_a")));
        assert!(!app.visible_messages().0.iter().any(|message| message.subject.contains("acc_b")));

        app.switch_account("acc_b".to_owned());
        assert_eq!(app.active_account.as_deref(), Some("acc_b"));
        assert_eq!(app.folder, "fld_acc_b_inbox");
        assert!(app.visible_messages().0.iter().any(|message| message.subject.contains("acc_b")));
        assert!(!app.visible_messages().0.iter().any(|message| message.subject.contains("acc_a")));

        let state = AccountRemoveState {
            id: "acc_b".to_owned(),
            name: "Account acc_b".to_owned(),
            email: "b@example.com".to_owned(),
            last: false,
            focus_cancel: true,
        };
        app.remove_account(&state);
        assert_eq!(app.engine.accounts().len(), 1);
        assert_eq!(app.active_account.as_deref(), Some("acc_a"));
        assert!(app.engine.messages().iter().all(|message| !message.id.contains("acc_b")));

        let state = AccountRemoveState {
            id: "acc_a".to_owned(),
            name: "Account acc_a".to_owned(),
            email: "a@example.com".to_owned(),
            last: true,
            focus_cancel: true,
        };
        app.remove_account(&state);
        assert_eq!(app.engine.accounts().len(), 0);
        assert!(app.onboarding.is_some(), "removing the last account returns to onboarding");
        assert!(app.active_account.is_none());
        assert!(app.sync_worker.is_none());
    }

    #[test]
    fn a_second_sync_request_says_it_is_already_running() {
        let server = RunningServer::mailbox(three_folders(), 1);
        let port = server.port();
        let mut app = booted_app("app-second-sync", vec![server.ca()], |store| {
            seed_account_rows(store, "acc_sync", postino_testkit::USER, Some(port));
        });
        set_test_password(&app, "acc_sync");
        app.request_sync();
        assert!(matches!(app.sync_view, SyncView::Running(_)));
        app.request_sync();
        assert_eq!(app.status, "Sincronizzazione già in corso");
        wait_for_sync(&mut app);
        server.finish();
    }

    #[test]
    fn a_failed_sync_is_offline_and_never_empties_the_list() {
        let mut app = booted_app("app-offline", Vec::new(), |store| {
            seed_account_rows(store, "acc_offline", postino_testkit::USER, Some(1));
        });
        set_test_password(&app, "acc_offline");
        app.last_sync = Some(1_700_000_000_000);
        let before = app.visible_messages().0.len();
        assert_eq!(before, 1);
        app.request_sync();
        wait_for_sync(&mut app);
        assert!(matches!(app.sync_view, SyncView::Offline { last_sync: Some(1_700_000_000_000) }), "{:?}", app.sync_view);
        assert_eq!(app.visible_messages().0.len(), before, "an offline sync keeps the cached mail");
        assert_eq!(app.status_kind(), StatusKind::Offline, "offline is its own honest state, not a red error");
    }

    #[test]
    fn a_real_onboarding_creates_the_account_and_first_sync() {
        let server = RunningServer::mailbox(three_folders(), 3);
        let roots = vec![server.ca()];
        let port = server.port();
        let mut app = booted_app("app-onboarding-real", roots.clone(), |_| {});
        {
            let onboarding = app.onboarding.as_mut().expect("first run");
            onboarding.seed_for_test(Stage::Credentials, ProviderChoice::Custom, None, None);
            onboarding.set_custom("localhost", &port.to_string());
            onboarding.set_fields("Postino Test", postino_testkit::USER, postino_testkit::PASSWORD);
            onboarding.set_probe_timeout(Duration::from_secs(3));
            onboarding.start_probe(roots.clone(), false);
        }
        for _ in 0..500 {
            let onboarding = app.onboarding.as_mut().expect("onboarding");
            onboarding.poll_probe();
            if !onboarding.is_testing() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let action = {
            let onboarding = app.onboarding.as_mut().expect("onboarding");
            assert!(onboarding.connection_error().is_none(), "{:?}", onboarding.connection_error());
            let action = onboarding.advance(roots.clone());
            assert!(matches!(action, onboarding::Action::None));
            onboarding.advance(roots.clone())
        };
        assert!(matches!(action, onboarding::Action::AddAccount(_)));
        app.handle_onboarding_action(action);
        for _ in 0..1500 {
            app.poll_add_account();
            app.handle_sync_events();
            if app.onboarding.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.onboarding.is_none(), "the first sync closes the wizard");
        assert_eq!(app.engine.accounts().len(), 1);
        assert_eq!(app.engine.messages().len(), 3);
        assert!(app.engine.messages().iter().all(|message| message.folder_id != "inbox"));
        assert!(matches!(app.sync_view, SyncView::Idle { last_sync: Some(_) }));
        server.finish();
    }

    #[test]
    fn a_successful_sync_reloads_without_losing_selection_filters_or_the_open_message() {
        let server = RunningServer::mailbox(three_folders(), 2);
        let port = server.port();
        let mut app = booted_app("app-reload", vec![server.ca()], |store| {
            seed_account_rows(store, "acc_reload", postino_testkit::USER, Some(port));
        });
        set_test_password(&app, "acc_reload");
        app.request_sync();
        wait_for_sync(&mut app);
        let rows = app.visible_messages().0;
        let selected = rows.first().map(|message| message.id.clone()).expect("synced mail");
        app.open_message(selected.clone());
        app.query = "Synthetic".to_owned();
        app.unread_only = true;
        assert!(app.reading_open);
        app.request_sync();
        wait_for_sync(&mut app);
        assert_eq!(app.selected.as_deref(), Some(selected.as_str()), "the open message survives the reload");
        assert!(app.reading_open);
        assert_eq!(app.query, "Synthetic");
        assert!(app.unread_only);
        assert!(app.engine.message(&selected).is_some());
        server.finish();
    }

    #[test]
    fn compose_uses_the_active_account_identity_and_drafts_folder() {
        let mut app = booted_app("app-compose-identity", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        app.switch_account("acc_b".to_owned());
        app.compose_new();
        assert_eq!(app.account().map(|account| account.id.as_str()), Some("acc_b"));
        app.composer.to = "Destinatario <dest@example.com>".to_owned();
        app.composer.subject = "Bozza dell'account attivo".to_owned();
        app.composer.body = "Corpo".to_owned();
        app.composer.touch(0.0);
        assert!(app.save_composer(false));
        let draft = app.composer.draft_id.clone().expect("a saved draft");
        assert_eq!(app.engine.message(&draft).map(|message| message.summary.folder_id.clone()), Some("fld_acc_b_drafts".to_owned()));
        assert_eq!(
            app.engine.message(&draft).map(|message| message.summary.to.first().map(|address| address.email.clone())),
            Some(Some("dest@example.com".to_owned()))
        );
    }

    #[test]
    fn the_account_menu_and_settings_dialog_render_at_the_minimum_size() {
        let mut app = booted_app("app-account-menu", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.show_accounts_dialog = true;
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        app.show_accounts_dialog = false;
        app.show_accounts_menu = true;
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        app.show_accounts_menu = false;
        app.account_remove = Some(AccountRemoveState {
            id: "acc_b".to_owned(),
            name: "Account acc_b".to_owned(),
            email: "b@example.com".to_owned(),
            last: false,
            focus_cancel: true,
        });
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        assert!(app.account_remove.is_some(), "Esc was not pressed: the confirmation stays open");
    }

    #[test]
    fn a_store_mailbox_renders_at_the_minimum_size_and_in_dark() {
        let mut app = booted_app("app-mailbox-980", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
        });
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        for dark in [false, true] {
            theme::apply(&ctx, dark);
            let raw =
                egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| app.show(ui));
            assert!(!output.shapes.is_empty(), "the store-backed mailbox painted nothing (dark={dark})");
            output.textures_delta.clear();
        }
    }

    #[test]
    fn the_starred_view_is_scoped_to_the_active_account() {
        let mut app = booted_app("app-starred", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        app.switch_account("acc_b".to_owned());
        let starred = app.visible_messages().0.first().map(|message| message.id.clone()).expect("an acc_b message");
        app.set_starred_for(&starred, true);
        assert_eq!(app.starred_counts("acc_b"), (1, 1));
        app.switch_account("acc_a".to_owned());
        assert_eq!(app.starred_counts("acc_a"), (0, 0));
        app.select_folder("fld_acc_a_starred");
        assert!(app.visible_messages().0.is_empty(), "another account's star must not leak into Preferiti");
    }

    fn seeded_attachment_app(name: &str, filename: &str, bytes: Option<&[u8]>) -> (PostinoApp, String, Attachment) {
        let filename = filename.to_owned();
        let mut app = booted_app(name, Vec::new(), move |store| {
            seed_account_rows(store, "acc_save", "save@example.com", None);
            seed_attachment(store, "msg_acc_save_1", "att_save", &filename);
        });
        let message_id = "msg_acc_save_1".to_owned();
        let attachment = app.engine.message(&message_id).and_then(|message| message.attachments.first().cloned()).expect("the seeded attachment");
        if let Some(bytes) = bytes {
            app.engine.store_attachment_bytes(&message_id, &attachment.id, bytes).expect("store the test blob");
        }
        (app, message_id, attachment)
    }

    #[test]
    fn saving_an_attachment_writes_exact_blob_bytes() {
        let path = directory("save-exact").join("report.pdf");
        let _ = std::fs::remove_file(&path);
        let (mut app, message_id, attachment) = seeded_attachment_app("p6c-save-exact", "report.pdf", Some(b"blob esatto\nseconda riga"));
        let saver = RecordingFileSaver::new();
        saver.set_path(Some(path.clone()));
        app.file_saver = saver.clone();
        app.open_message(message_id);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        settle_at(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        let save = probe::attachment_save_rect(&ctx, &attachment.id).expect("the attachment Save button");
        click_at(&ctx, &mut app, save.center(), Vec2::new(1200.0, 800.0));
        assert!(!app.attachment_warning.is_some(), "a PDF needs no warning");
        assert!(!app.attachment_overwrite.is_some(), "a new path needs no overwrite confirmation");
        assert_eq!(std::fs::read(&path).expect("the saved file"), b"blob esatto\nseconda riga");
        assert_eq!(saver.writes(), vec![(path.clone(), b"blob esatto\nseconda riga".to_vec())]);
        assert_eq!(app.toast.as_ref().map(|toast| toast.kind), Some(ToastKind::Success));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_existing_file_is_not_overwritten_until_confirmed() {
        let path = directory("save-overwrite").join("report.pdf");
        std::fs::write(&path, b"vecchio contenuto").expect("an existing file");
        let (mut app, _, attachment) = seeded_attachment_app("p6c-overwrite", "report.pdf", Some(b"nuovo contenuto"));
        let saver = RecordingFileSaver::new();
        saver.set_path(Some(path.clone()));
        app.file_saver = saver.clone();
        app.request_attachment_save(&attachment);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Sostituire il file?")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("I dati del file esistente saranno persi")), "the overwrite body must name the loss: {text:?}");
        assert_eq!(std::fs::read(&path).expect("old content is intact"), b"vecchio contenuto");
        assert!(saver.writes().is_empty());
        click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Sostituire il file?", true);
        assert_eq!(std::fs::read(&path).expect("new content"), b"nuovo contenuto");
        assert_eq!(app.toast.as_ref().map(|toast| toast.kind), Some(ToastKind::Success));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dangerous_attachments_warn_before_choosing_a_path() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        for (filename, kind) in [("setup.exe\u{00A0}", DangerousKind::Executable), ("report.docm", DangerousKind::Macro)] {
            let path = directory("save-warning").join(format!("{kind:?}.bin"));
            let _ = std::fs::remove_file(&path);
            let (mut app, _, attachment) = seeded_attachment_app(&format!("p6c-warning-{kind:?}"), filename, Some(b"warning bytes"));
            let saver = RecordingFileSaver::new();
            saver.set_path(Some(path.clone()));
            app.file_saver = saver.clone();
            app.request_attachment_save(&attachment);
            let warning = app.attachment_warning.as_ref().expect("a warning before the native dialog");
            assert_eq!(warning.dangerous, Some(kind));
            assert_eq!(saver.choose_calls(), 0, "the path must not be chosen before the warning");
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(text.iter().any(|line| line.contains("Salvare un file potenzialmente pericoloso?")), "{text:?}");
            assert!(text.iter().any(|line| line.contains("setup.exe") || line.contains("report.docm")), "{text:?}");
            assert!(text.iter().any(|line| line.contains(kind.message())), "the warning body must carry the reason: {text:?}");
            click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Salvare un file potenzialmente pericoloso?", true);
            assert_eq!(saver.choose_calls(), 1, "the path is chosen only after the warning");
            assert_eq!(std::fs::read(&path).expect("saved after confirmation"), b"warning bytes");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn a_dangerous_extension_chosen_in_the_native_dialog_is_warned_again() {
        let path = directory("save-chosen-exe").join("chosen.exe");
        let _ = std::fs::remove_file(&path);
        let (mut app, _, attachment) = seeded_attachment_app("p6c-chosen-exe", "report.pdf", Some(b"chosen bytes"));
        let saver = RecordingFileSaver::new();
        saver.set_path(Some(path.clone()));
        app.file_saver = saver.clone();
        app.request_attachment_save(&attachment);
        assert_eq!(saver.choose_calls(), 1);
        let warning = app.attachment_warning.as_ref().expect("the chosen path must be re-classified");
        assert_eq!(warning.dangerous, Some(DangerousKind::Executable));
        assert_eq!(warning.path.as_deref(), Some(path.as_path()));
        assert!(saver.writes().is_empty(), "nothing is written before the second confirmation");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Destinazione scelta")), "{text:?}");
        click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Salvare un file potenzialmente pericoloso?", true);
        assert_eq!(std::fs::read(&path).expect("saved after the second confirmation"), b"chosen bytes");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cancelling_a_save_writes_nothing_and_a_missing_blob_is_readable() {
        let path = directory("save-cancel").join("report.pdf");
        let (mut app, _, attachment) = seeded_attachment_app("p6c-cancel", "report.pdf", Some(b"bytes"));
        let saver = RecordingFileSaver::new();
        saver.set_path(None);
        app.file_saver = saver.clone();
        app.request_attachment_save(&attachment);
        assert_eq!(saver.choose_calls(), 1);
        assert!(saver.writes().is_empty());
        assert_eq!(app.status, "Salvataggio annullato");

        let (mut missing, _, attachment) = seeded_attachment_app("p6c-missing", "report.pdf", None);
        let saver = RecordingFileSaver::new();
        saver.set_path(Some(path));
        missing.file_saver = saver.clone();
        missing.request_attachment_save(&attachment);
        assert_eq!(saver.choose_calls(), 0);
        assert!(saver.writes().is_empty());
        let toast = missing.toast.as_ref().expect("a readable missing-blob toast");
        assert_eq!(toast.kind, ToastKind::Error);
        assert!(toast.message.contains("non è disponibile"));
    }

    #[test]
    fn demo_save_is_disabled_and_never_reports_a_corrupt_attachment() {
        let (ctx, mut app) = ready_app();
        let (message_id, attachment) = app
            .engine
            .messages()
            .iter()
            .filter(|message| message.has_attachments)
            .map(|message| message.id.clone())
            .find_map(|id| app.engine.message(&id))
            .and_then(|message| message.attachments.first().map(|attachment| (message.summary.id.clone(), attachment.clone())))
            .expect("a demo attachment");
        app.open_message(message_id);
        settle_at(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        assert_eq!(probe::attachment_save_is_enabled(&ctx, &attachment.id), Some(false), "demo Save must be visibly disabled");
        let save = probe::attachment_save_rect(&ctx, &attachment.id).expect("the disabled Save rect");
        click_at(&ctx, &mut app, save.center(), Vec2::new(1200.0, 800.0));
        assert!(app.attachment_warning.is_none());
        assert!(app.attachment_overwrite.is_none());
        assert!(app.toast.is_none(), "a disabled button must not open a misleading error");
        let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        assert!(text.iter().any(|line| line.contains("La modalità demo non ha allegati reali")), "{text:?}");

        app.request_attachment_save(&attachment);
        let toast = app.toast.as_ref().expect("the demo reason");
        assert_eq!(toast.kind, ToastKind::Info);
        assert!(toast.message.contains("La modalità demo non ha allegati reali"));
    }

    #[test]
    fn the_attachment_save_button_is_keyboard_reachable() {
        let path = directory("save-keyboard").join("report.pdf");
        let _ = std::fs::remove_file(&path);
        let (mut app, message_id, attachment) = seeded_attachment_app("p6c-keyboard-save", "report.pdf", Some(b"keyboard bytes"));
        let saver = RecordingFileSaver::new();
        saver.set_path(Some(path.clone()));
        app.file_saver = saver.clone();
        app.open_message(message_id);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert!(probe::attachment_save_rect(&ctx, &attachment.id).is_some());
        let save_id = app.attachment_save_ids.get(&attachment.id).copied().expect("the Save button id");
        ctx.memory_mut(|memory| memory.request_focus(save_id));
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(saver.writes().len(), 1, "Enter on the focused Save button saves");
        assert_eq!(std::fs::read(&path).expect("the keyboard-saved file"), b"keyboard bytes");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_link_requires_confirmation_and_reaches_the_injected_opener_exactly() {
        let mut app = PostinoApp::demo(true);
        let opener = RecordingLinkOpener::new();
        app.link_opener = opener.clone();
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(980.0, 600.0);
        let link = reading::find_links("Vai su https://example.com/preventivo?token=abc, grazie.").pop().expect("a link");
        app.request_link_open(&link);
        assert_eq!(app.pending_link.as_ref().map(|pending| pending.url.as_str()), Some("https://example.com/preventivo?token=abc"));
        assert!(opener.opened().is_empty(), "the sheet has not been confirmed");
        click_dialog_button(&ctx, &mut app, size, "Aprire il link?", true);
        assert_eq!(opener.opened(), vec!["https://example.com/preventivo?token=abc".to_owned()]);
        assert!(app.pending_link.is_none());

        // The secondary button, not a hand-cleared field, is the cancel path.
        app.request_link_open(&link);
        click_dialog_button(&ctx, &mut app, size, "Aprire il link?", false);
        assert!(app.pending_link.is_none());
        assert_eq!(opener.opened().len(), 1, "Annulla calls nothing");

        // The action boundary rechecks even when a pending value is set directly.
        app.pending_link = Some(PendingLink {
            url: "javascript:alert(1)".to_owned(),
            host: "example.com".to_owned(),
            recipient: None,
            mailto: false,
            idn: false,
            non_ascii: false,
            backslash: false,
            focus_cancel: false,
        });
        app.confirm_pending_link();
        assert_eq!(opener.opened().len(), 1, "an unvetted URL never reaches the opener");
        assert_eq!(app.toast.as_ref().map(|toast| toast.kind), Some(ToastKind::Error));

        let bad = ExtractedLink {
            range: 0..10,
            url: "javascript:alert(1)".to_owned(),
            host: "example.com".to_owned(),
            recipient: None,
            mailto: false,
            idn: false,
            non_ascii: false,
            backslash: false,
        };
        app.request_link_open(&bad);
        assert!(app.pending_link.is_none(), "a hostile scheme never reaches the sheet");
    }

    #[test]
    fn the_link_sheet_shows_the_full_destination_and_the_effective_host() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let mut app = PostinoApp::demo(true);
        let link = reading::find_links("https://xn--bcher-kva.example/preventivo").pop().expect("an IDN link");
        app.request_link_open(&link);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("xn--bcher-kva.example/preventivo")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("Host di destinazione: xn--bcher-kva.example")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("punycode")), "the IDN warning must be visible");

        // A backslash ends the authority in a browser, so the sheet names evil.example and warns.
        let backslash = reading::find_links("https://evil.example\\@bank.example/").pop().expect("a backslash link");
        app.pending_link = None;
        app.request_link_open(&backslash);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Host di destinazione: evil.example")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("barra rovesciata")), "{text:?}");

        // A Cyrillic host has no literal xn-- label but the browser punycodes it. The line names the
        // spelling in the link, not a host the browser will contact.
        let cyrillic = reading::find_links("https://аpple.com/x").pop().expect("a non-ASCII link");
        app.pending_link = None;
        app.request_link_open(&cyrillic);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("caratteri non ASCII")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("Dominio nel link: аpple.com")), "{text:?}");
        assert!(!text.iter().any(|line| line.contains("Host di destinazione")), "a non-ASCII spelling is not the browser host: {text:?}");

        // A mailto opens a mail client and names its explicit recipient, not a browser host.
        let mailto = reading::find_links("mailto:help@example.com?subject=Preventivo").pop().expect("a mailto link");
        app.pending_link = None;
        app.request_link_open(&mailto);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("client di posta")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("Destinatario: help@example.com")), "{text:?}");
        assert!(!text.iter().any(|line| line.contains("Host di destinazione")), "{text:?}");
    }

    #[test]
    fn the_reading_pane_link_widget_opens_the_sheet() {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        app.seed_screenshot(ScreenshotState::ReadingAttachments);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(1280.0, 800.0);
        settle_at(&ctx, &mut app, size);
        let url = "https://example.com/preventivo/postino-2026?token=anteprima";
        let link = probe::link_rect(&ctx, url).expect("the reading-pane link widget");
        click_at(&ctx, &mut app, link.center(), size);
        let pending = app.pending_link.as_ref().expect("clicking the link opens the sheet");
        assert_eq!(pending.url, url);
    }

    #[test]
    fn userinfo_and_mailto_hidden_recipients_are_never_links() {
        // The pure scanner owns the rule; this test keeps the app from presenting a crafted link.
        for raw in [
            "https://bank.example@evil.example/",
            "https://user:pass@bank.example/",
            "mailto:help@example.com?bcc=attacker@evil.example",
            "mailto:help@example.com?cc=attacker@evil.example",
            "mailto:attacker@evil.example,help@example.com",
            "mailto:attacker@evil.example%2Chelp@example.com",
            "mailto:attacker@evil.example;help@example.com",
            "mailto:help@example.com?attach=C:\\Users\\feder\\secret.pdf",
            "mailto:help@example.com?attachment=%5C%5Cattacker.example%5Cshare%5Csecret.pdf",
            "mailto:help@example.com?%61ttach=%5C%5Cattacker.example%5Cshare%5Csecret.pdf",
        ] {
            assert!(reading::find_links(raw).is_empty(), "{raw}");
            assert!(!reading::is_allowed_url(raw), "{raw}");
        }
    }

    #[test]
    fn the_rules_page_renders_list_editor_and_preview_at_980_light_and_dark() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            let mut app = PostinoApp::demo(true);
            app.seed_screenshot(ScreenshotState::Rules);
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(text.iter().any(|line| line == "REGOLE"), "the rules page header is missing: {text:?}");
            assert!(text.iter().any(|line| line.contains("Fatture")), "the seeded rule list is missing: {text:?}");
            assert!(text.iter().any(|line| line.contains("Salva")), "the editor actions are missing: {text:?}");
            assert!(text.iter().any(|line| line == "Rimuovi"), "the editor action row is missing: {text:?}");
            assert!(text.iter().any(|line| line == "Etichetta"), "the seeded action kind is missing: {text:?}");
            assert!(!text.iter().any(|line| line.contains("rule_run_")), "a raw run id reached the page: {text:?}");
            assert!(!text.iter().any(|line| line.contains(" · off")), "an English state reached the page: {text:?}");
            let page = app.rules_page.as_ref().expect("the rules page is open");
            assert!(!page.is_forward_allowed(), "the demo has no SMTP outbox and must disable forward with a reason");

            let mut app = PostinoApp::demo(true);
            app.seed_screenshot(ScreenshotState::RulePreview);
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(text.iter().any(|line| line.contains("Fatture e ricevute")), "the editor name is missing: {text:?}");
            assert!(text.iter().any(|line| line.contains("ANTEPRIMA")), "the preview section is missing: {text:?}");
            assert!(text.iter().any(|line| line.contains("applica")), "the preview effects are missing: {text:?}");
        }
    }

    #[test]
    fn the_rules_page_owns_the_keyboard_and_escapes_back() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        let rows = app.visible_messages().0;
        let id = rows[0].id.clone();
        app.open_message(id.clone());
        let before = app.engine.message(&id).expect("message");
        let selected_before = app.selected.clone();
        assert!(app.reading_open);
        frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::K, Modifiers::COMMAND)]);
        assert!(app.show_palette);
        app.palette_query = "regole".to_owned();
        app.palette_selected = 0;
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.rules_page.is_some(), "the palette command must open the rules page");

        for pressed in [Key::E, Key::Delete, Key::Z, Key::J] {
            frame(&ctx, &mut app, vec![key(pressed, Modifiers::NONE)]);
        }
        let after = app.engine.message(&id).expect("message");
        assert_eq!(after.summary.folder_id, before.summary.folder_id, "a shell shortcut archived/deleted mail behind the page");
        assert_eq!(after.summary.unread, before.summary.unread, "a shell shortcut changed the hidden message");
        assert_eq!(app.selected, selected_before, "J moved the hidden selection");
        assert!(app.reading_open, "the hidden reading pane closed");

        frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.rules_page.is_none(), "Esc closes the rules page");
        assert!(!app.show_palette);
        assert!(app.reading_open, "Esc must close only the rules page, not the reading pane");
        assert_eq!(app.selected, selected_before);
    }

    #[test]
    fn settings_renders_every_section_at_the_minimum_size_light_and_dark() {
        let (mut app, _, _) = seeded_attachment_app("p6c-settings-980", "report.pdf", None);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        app.open_settings(&ctx, None);
        for dark in [false, true] {
            app.dark = dark;
            theme::apply(&ctx, dark);
            let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
            // Capture after the first settled frame: `Context::default()` starts with a huge
            // placeholder viewport, which would make the light assertions vacuous.
            let content = ctx.content_rect();
            let text = runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>();
            for section in ["ACCOUNT", "ASPETTO", "DATI", "INFORMAZIONI", "Licenze e attribuzioni", "Scorciatoie da tastiera"] {
                assert!(text.iter().any(|line| line.contains(section)), "missing {section}: {text:?}");
            }
            assert!(text.iter().any(|line| line.contains("Ultimo sync")), "the account row names its last sync: {text:?}");
            let scroll = probe::settings_scroll_view(&ctx).expect("the settings scroll viewport");
            assert_eq!(probe::settings_scroll_offset(&ctx), Some(0.0), "the settings shot starts at the top");
            let sections = probe::settings_sections(&ctx);
            for section in ["ACCOUNT", "ASPETTO", "DATI", "INFORMAZIONI"] {
                let rect = sections.get(section).copied().unwrap_or_else(|| panic!("{section} was not probed"));
                assert!(content.contains_rect(rect), "{section} is outside the window: {rect:?}");
                assert!(scroll.contains_rect(rect), "{section} is clipped by the scroll viewport: {rect:?} in {scroll:?}");
                let run = runs
                    .iter()
                    .find(|run| run.text == section || run.text.starts_with(section))
                    .unwrap_or_else(|| panic!("{section} text run missing: {text:?}"));
                assert!(run.clip.contains_rect(run.rect), "{section} text is clipped: {run:?}");
            }
            let footer = probe::settings_footer_rect(&ctx).expect("the settings footer");
            assert!(content.contains_rect(footer), "the footer is off-screen at 980x600: {footer:?}");
            for key in ["store", "blob"] {
                let copy = probe::settings_copy_rect(&ctx, key).expect("each path has a copy action");
                assert!(content.contains_rect(copy), "the {key} copy action is off-screen: {copy:?}");
            }
            for label in ["Copia archivio", "Copia allegati", "Licenze e attribuzioni", "Scorciatoie da tastiera"] {
                let run = runs.iter().find(|run| run.text.contains(label)).unwrap_or_else(|| panic!("{label} text run missing: {text:?}"));
                assert!(scroll.contains_rect(run.rect), "{label} is clipped at 980x600: {:?} in {scroll:?}", run.rect);
                assert!(run.clip.contains_rect(run.rect), "{label} text is clipped: {run:?}");
            }
        }
    }

    #[test]
    fn the_settings_screenshot_state_has_the_same_unread_total_at_both_sizes() {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        for size in [Vec2::new(980.0, 600.0), Vec2::new(1600.0, 1000.0)] {
            let mut app = PostinoApp::demo_at(true, clock);
            app.seed_screenshot(ScreenshotState::Settings);
            assert_eq!(app.account_unread("acc_demo"), 5, "the settings fixture must not read a message at {size:?}");
            let (_, total, unread) = app.visible_messages();
            assert_eq!((total, unread), (7, 4), "the inbox total must be identical at {size:?}");
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, false);
            let text = painted_text(&ctx, &mut app, size);
            assert!(text.iter().any(|line| line.contains("5 non letti")), "the sidebar badge at {size:?}: {text:?}");
        }
    }

    #[test]
    fn settings_closes_with_escape_and_returns_focus_to_the_account_switcher() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let mut app = PostinoApp::demo(true);
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        let switcher = app.account_switcher_id.expect("the sidebar account row");
        app.open_settings(&ctx, None);
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert!(app.show_accounts_dialog);
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.show_accounts_dialog);
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(switcher), "focus returns to the opener");
    }

    #[test]
    fn settings_shows_the_store_path_and_uses_the_injected_folder_opener() {
        let (mut app, _, _) = seeded_attachment_app("p6c-settings-data", "report.pdf", None);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.open_settings(&ctx, None);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("mail.db")), "the store path is visible: {text:?}");
        assert!(text.iter().any(|line| line.contains("mail.db.blobs")), "the blob path is visible: {text:?}");
        let opener = RecordingFolderOpener::new();
        app.folder_opener = opener.clone();
        app.open_data_folder();
        let opened = opener.opened();
        assert_eq!(opened.len(), 1);
        assert!(opened[0].ends_with("mail.db.blobs"), "{:?}", opened[0]);
    }

    #[test]
    fn settings_actions_are_real_buttons() {
        let (mut app, _, _) = seeded_attachment_app("p6c-settings-actions", "report.pdf", Some(b"bytes"));
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(980.0, 600.0);
        let store_path = app.store_path.clone().expect("the store path");
        let store = store_path.display().to_string();
        let blob = postino_store::blob_root(&store_path).display().to_string();
        let _ = std::fs::create_dir_all(postino_store::blob_root(&store_path));
        app.open_settings(&ctx, None);

        settle_at(&ctx, &mut app, size);
        let copy = probe::settings_copy_rect(&ctx, "store").expect("Copia archivio");
        let output = click_at_output(&ctx, &mut app, copy.center(), size);
        assert_eq!(copied_text(&output).as_deref(), Some(store.as_str()), "Copia archivio puts the path on the clipboard");

        settle_at(&ctx, &mut app, size);
        let copy = probe::settings_copy_rect(&ctx, "blob").expect("Copia allegati");
        let output = click_at_output(&ctx, &mut app, copy.center(), size);
        assert_eq!(copied_text(&output).as_deref(), Some(blob.as_str()), "Copia allegati puts the blob path on the clipboard");

        let opener = RecordingFolderOpener::new();
        app.folder_opener = opener.clone();
        settle_at(&ctx, &mut app, size);
        let open = probe::settings_action_rect(&ctx, "open_folder").expect("Apri la cartella");
        click_at(&ctx, &mut app, open.center(), size);
        assert_eq!(opener.opened(), vec![postino_store::blob_root(&store_path)], "the real folder button calls the injected opener");

        settle_at(&ctx, &mut app, size);
        let licences = probe::settings_action_rect(&ctx, "licences").expect("Licenze e attribuzioni");
        click_at(&ctx, &mut app, licences.center(), size);
        assert!(app.show_licences, "the real Licenze button opens the local dialog");
        frame_at(&ctx, &mut app, size, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!app.show_licences);
        // Reopen settings: the nested licences modal and the settings modal both see Escape, so the
        // test reopens rather than asserting which modal closed first.
        app.open_settings(&ctx, None);

        settle_at(&ctx, &mut app, size);
        let shortcuts = probe::settings_action_rect(&ctx, "shortcuts").expect("Scorciatoie da tastiera");
        click_at(&ctx, &mut app, shortcuts.center(), size);
        assert!(app.show_shortcuts, "the real shortcut button opens the sheet");
    }

    #[test]
    fn settings_shows_each_accounts_own_last_sync() {
        let now = Utc::now();
        let first = (now - chrono::Duration::hours(2)).timestamp_millis();
        let second = (now - chrono::Duration::minutes(20)).timestamp_millis();
        let mut app = booted_app("p6c-settings-sync", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
            for (account, last_success) in [("acc_a", first), ("acc_b", second)] {
                let status = postino_sync::SyncStatus {
                    state: postino_sync::SyncPhase::Idle,
                    last_attempt: last_success,
                    last_success: Some(last_success),
                    error: None,
                    folders_done: 1,
                    folders_total: 1,
                    messages: 1,
                };
                store
                    .set_meta(&postino_sync::status_key(account), &serde_json::to_string(&status).expect("status JSON"))
                    .expect("seed the sync status");
            }
        });
        let expected_first = format!("Ultimo sync: {}", format_millis(first, app.now_utc));
        let expected_second = format!("Ultimo sync: {}", format_millis(second, app.now_utc));
        assert_ne!(expected_first, expected_second);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.open_settings(&ctx, None);
        let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        assert!(text.iter().any(|line| line.contains(&expected_first) && line.contains("a@example.com")), "acc_a shows its own sync: {text:?}");
        assert!(text.iter().any(|line| line.contains(&expected_second) && line.contains("b@example.com")), "acc_b shows its own sync: {text:?}");
    }

    #[test]
    fn the_palette_entry_opens_settings_and_the_licences_dialog_is_local() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let mut app = PostinoApp::demo(true);
        assert!(command("settings").is_some());
        app.execute_command("settings", &ctx);
        assert!(app.show_accounts_dialog);
        app.open_licences(&ctx);
        let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        assert!(text.iter().any(|line| line.contains("Licenze e attribuzioni")));
        assert!(
            text.iter().any(|line| line.contains("Postino onboarding welcome") || line.contains("Phosphor")),
            "the embedded attribution is shown"
        );
    }

    #[test]
    fn the_account_removal_button_warns_and_starts_on_cancel() {
        let mut app = booted_app("p6c-settings-remove", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        app.open_settings(&ctx, None);
        settle_at(&ctx, &mut app, Vec2::new(980.0, 600.0));
        let remove = probe::account_remove_button_rect(&ctx, "acc_b").expect("the Rimuovi button");
        click_at(&ctx, &mut app, remove.center(), Vec2::new(980.0, 600.0));
        assert!(app.account_remove.is_some(), "the confirmation sheet");
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("portachiavi")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("cache cifrata degli allegati")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("Le mail sul server non vengono toccate")), "{text:?}");
        let cancel_id = probe::dialog_button_focus(&ctx, "Rimuovere l'account?", false).expect("the Annulla button");
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(cancel_id), "Enter must land on Annulla");
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(app.account_remove.is_none());
        assert_eq!(app.engine.accounts().len(), 2, "Enter on Annulla removes nothing");

        settle_at(&ctx, &mut app, Vec2::new(980.0, 600.0));
        let remove = probe::account_remove_button_rect(&ctx, "acc_b").expect("the Rimuovi button");
        click_at(&ctx, &mut app, remove.center(), Vec2::new(980.0, 600.0));
        click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Rimuovere l'account?", true);
        assert_eq!(app.engine.accounts().len(), 1, "the real Rimuovi button removes the account");
    }

    #[test]
    fn the_text_view_label_explains_the_mode_for_plain_text_mail_too() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let mut app = PostinoApp::demo(true);
        let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
        app.open_message(id);
        let text = painted_text(&ctx, &mut app, Vec2::new(1280.0, 720.0));
        assert!(text.iter().any(|line| line.contains("Vista testo")), "the reading pane names its text view: {text:?}");
    }

    #[test]
    fn a_long_link_paragraph_wraps_inside_the_reading_column() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let t = theme::tokens(false);
        let text = format!("{} https://example.com/preventivo", "parola ".repeat(80));
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(320.0, 600.0))), ..Default::default() };
        let mut width = 0.0;
        let mut output = ctx.run_ui(raw, |ui| {
            ui.set_max_width(320.0);
            link_paragraph(ui, &text, &t, false, reading::MAX_LINKS_PER_MESSAGE);
            width = ui.min_rect().width();
        });
        output.textures_delta.clear();
        assert!(width <= 320.0, "a long paragraph must wrap, not extend past the column: {width}");
    }

    #[test]
    fn there_is_one_identity_and_one_offline_retry() {
        let (mut app, _, _) = seeded_attachment_app("p6c-chrome", "report.pdf", None);
        let email = app.engine.accounts().first().map(|account| account.email.clone()).expect("the seeded account");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert_eq!(probe::identity_count(&ctx), 1, "the account identity exists once (sidebar)");
        let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        let identity_lines = text.iter().filter(|line| line.contains(&email)).count();
        assert_eq!(identity_lines, 1, "the account e-mail must appear once in the chrome: {text:?}");
        assert!(text.iter().any(|line| line.contains("1 non letto")), "the unread badge names unread mail: {text:?}");
        assert!(account_unread_tooltip(1).contains("tutte le cartelle"));
        app.sync_view = SyncView::Offline { last_sync: Some(1_700_000_000_000) };
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert_eq!(probe::retry_count(&ctx), 1, "only the offline banner offers Retry");
        // Click the banner's Retry. With no worker, `request_sync` writes its refusal; a dead button
        // would leave the status empty.
        app.sync_worker = None;
        let banner_retry = probe::retry_rects(&ctx)[0];
        click_at(&ctx, &mut app, banner_retry.center(), Vec2::new(1200.0, 800.0));
        assert_eq!(app.status, "Il servizio di sincronizzazione non è disponibile", "the banner Retry calls request_sync");
        app.sync_view = SyncView::Error { reason: "credenziali rifiutate".to_owned(), last_sync: None };
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert_eq!(probe::retry_count(&ctx), 1, "only the status bar offers Retry for an error");

        // Below the reading breakpoint the list (and its banner) is not drawn: the status bar must
        // become the one owner of Retry instead of the state showing none.
        app.sync_view = SyncView::Offline { last_sync: Some(1_700_000_000_000) };
        let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a seeded message");
        app.open_message(id);
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
        assert_eq!(probe::retry_count(&ctx), 1, "the 980 reading overlay keeps exactly one Retry");
        app.status.clear();
        let status_retry = probe::retry_rects(&ctx)[0];
        click_at(&ctx, &mut app, status_retry.center(), Vec2::new(980.0, 600.0));
        assert_eq!(app.status, "Il servizio di sincronizzazione non è disponibile", "the compact status Retry calls request_sync");
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert_eq!(probe::retry_count(&ctx), 1, "the wide reading split still has exactly one Retry");
        app.reading_full = true;
        frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
        assert_eq!(probe::retry_count(&ctx), 1, "wide full-screen reading keeps exactly one Retry");
    }

    #[test]
    fn the_attachment_chip_shows_a_sanitized_name() {
        let (mut app, message_id, _) = seeded_attachment_app("p6c-bidi-chip", "invoice\u{202E}fdp.exe", Some(b"chip bytes"));
        app.open_message(message_id);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
        assert!(!text.iter().any(|line| line.contains('\u{202E}')), "the chip must not render a bidi control: {text:?}");
        assert!(text.iter().any(|line| line.contains("invoicefdp.exe")), "the sanitized name is visible: {text:?}");
    }

    #[test]
    fn four_attachments_fit_the_980_reading_pane() {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
        let mut message = app.engine.message(&id).expect("the open message");
        message.attachments = (0..4)
            .map(|index| Attachment {
                id: format!("att_many_{index}"),
                filename: format!("allegato-{index}.pdf"),
                mime: "application/pdf".to_owned(),
                size: 1024,
            })
            .collect();
        app.screenshot_message = Some(message);
        app.open_message(id);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(980.0, 600.0);
        settle_at(&ctx, &mut app, size);
        let content = ctx.content_rect();
        let strip = probe::reading_attachments_rect(&ctx).expect("the pinned attachment strip");
        assert!(content.contains_rect(strip), "the 4-attachment strip is off-screen: {strip:?}");
        assert!(strip.height() > 0.0);
        let save = probe::attachment_save_rect(&ctx, "att_many_0").expect("the first attachment Save");
        assert!(strip.contains_rect(save), "the first attachment must stay visible: {save:?} in {strip:?}");
    }

    #[test]
    fn a_hostile_message_caps_link_widgets() {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
        let mut message = app.engine.message(&id).expect("the open message");
        message.body_text = (0..100).map(|index| format!("https://example.com/{index}")).collect::<Vec<_>>().join("\n\n");
        app.screenshot_message = Some(message);
        app.open_message(id);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        settle_at(&ctx, &mut app, Vec2::new(1280.0, 800.0));
        assert_eq!(probe::link_count(&ctx), reading::MAX_LINKS_PER_MESSAGE, "the per-message link budget caps widgets");
    }

    #[test]
    fn an_overlong_chosen_name_keeps_its_extension_warning() {
        let long_name = format!("{}.exe", "a".repeat(130));
        let path = directory("save-long-chosen").join(&long_name);
        let _ = std::fs::remove_file(&path);
        let (mut app, _, attachment) = seeded_attachment_app("p6c-long-chosen", "report.pdf", Some(b"long bytes"));
        let saver = RecordingFileSaver::new();
        saver.set_path(Some(path.clone()));
        app.file_saver = saver.clone();
        app.request_attachment_save(&attachment);
        let warning = app.attachment_warning.as_ref().expect("the long chosen .exe must be warned");
        assert_eq!(warning.dangerous, Some(DangerousKind::Executable));
        assert!(saver.writes().is_empty());
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Salvare un file potenzialmente pericoloso?", true);
        assert_eq!(std::fs::read(&path).expect("saved after the warning"), b"long bytes");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_980_reading_overlay_shows_the_remote_line_and_attachment_save_in_both_themes() {
        for dark in [false, true] {
            let clock = screenshot_clock().expect("the frozen screenshot clock");
            let mut app = PostinoApp::demo_at(true, clock);
            app.seed_screenshot(ScreenshotState::ReadingAttachments);
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, dark);
            let size = Vec2::new(980.0, 600.0);
            settle_at(&ctx, &mut app, size);
            let fixture = app.screenshot_message.as_ref().expect("the stored-truth screenshot fixture");
            assert_eq!(fixture.remote_images, 1, "dark={dark}: the fixture must carry the stored count");
            let html = fixture.body_html.as_deref().expect("sanitized HTML");
            assert!(!html.contains("http") && !html.contains("<img"), "dark={dark}: the fixture must not inject a tracker: {html}");
            let content = ctx.content_rect();
            let strip = probe::reading_attachments_rect(&ctx).expect("the pinned attachment strip");
            let save = probe::attachment_save_rect(&ctx, "att_001").expect("the attachment Save button");
            assert!(content.contains_rect(strip), "the attachment strip is off-screen in dark={dark}: {strip:?}");
            assert!(strip.contains_rect(save), "Save is outside the attachment strip in dark={dark}: {save:?} in {strip:?}");
            assert!(content.contains_rect(save), "Save is off-screen at 980x600 in dark={dark}: {save:?}");
            assert_eq!(probe::attachment_save_is_enabled(&ctx, "att_001"), Some(true), "the store/screenshot path keeps Save usable");
            let text = painted_text(&ctx, &mut app, size);
            assert!(text.iter().any(|line| line.contains("ALLEGATI")), "dark={dark}: {text:?}");
            assert!(text.iter().any(|line| line == "Salva"), "the Salva label must be painted in dark={dark}: {text:?}");
            let remote = text.iter().position(|line| line.contains("1 immagine remota bloccata")).expect("the stored count line");
            let mode = text.iter().position(|line| line.contains("Vista testo")).expect("the text-view line");
            assert!(remote < mode, "the remote line must sit above Vista testo in dark={dark}: {text:?}");
            let line = &text[remote];
            assert!(!line.contains("http") && !line.contains("tracker"), "the line must not name a URL: {line:?}");
        }
    }

    #[test]
    fn the_cc_line_is_painted_and_stays_inside_the_reading_column() {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
        app.open_message(id.clone());
        let mut message = app.engine.message(&id).expect("the open message");
        message.cc = vec![
            Address::new("Giulia Ferri", "giulia.ferri@example.com"),
            Address::new("Marco Bianchi", "marco.bianchi@example.com"),
            Address::new("Libreria Aurora", "ordini@libreria-aurora.example"),
        ];
        app.screenshot_message = Some(message);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(1280.0, 800.0);
        let text = painted_text(&ctx, &mut app, size);
        assert!(text.iter().any(|line| line.starts_with("Cc:")), "{text:?}");
        let cc = probe::reading_cc_rect(&ctx).expect("the Cc line");
        assert!(ctx.content_rect().contains_rect(cc), "the Cc line is outside the window: {cc:?}");
        assert!(cc.height() <= 20.0, "the Cc line is one truncated line, not a clipped block: {cc:?}");
    }

    #[test]
    fn the_remote_content_line_uses_the_stored_count_and_never_a_url() {
        for (count, expected) in [
            (0, None),
            (1, Some("1 immagine remota bloccata")),
            (2, Some("2 immagini remote bloccate")),
            (10_000, Some("10000 immagini remote bloccate")),
        ] {
            let clock = screenshot_clock().expect("the frozen screenshot clock");
            let mut app = PostinoApp::demo_at(true, clock);
            let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
            app.open_message(id.clone());
            let mut message = app.engine.message(&id).expect("the open message");
            // The sanitized HTML has no remote reference left: only the stored count may produce the
            // line. A body with no link makes a URL in the painted copy a real failure.
            message.body_html = Some("<p>Corpo già sanificato.</p>".to_owned());
            message.body_text = "Corpo già sanificato.".to_owned();
            message.remote_images = count;
            app.screenshot_message = Some(message);
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            theme::apply(&ctx, false);
            let text = painted_text(&ctx, &mut app, Vec2::new(1280.0, 800.0));
            let line = text.iter().find(|line| line.contains("immagin"));
            match expected {
                Some(expected) => {
                    let line = line.unwrap_or_else(|| panic!("the count {count} has no remote-content line: {text:?}"));
                    assert!(line.contains(expected), "count {count} must read {expected:?}: {line:?}");
                    assert!(line.contains("il contenuto arriva dal server e non viene caricato"), "{line:?}");
                    assert!(!line.contains("http") && !line.contains("tracker"), "the line must not name a URL: {line:?}");
                }
                None => assert!(line.is_none(), "a message with no blocked image must not invent a line: {text:?}"),
            }
            assert!(!text.iter().any(|line| line.contains("http")), "the pane painted a URL with no link in the body: {text:?}");
        }
    }

    #[test]
    fn a_file_created_after_the_overwrite_check_is_not_silently_replaced() {
        let path = directory("save-race").join("report.pdf");
        std::fs::write(&path, b"creato da un altro processo").expect("the racing file");
        let (mut app, _, _) = seeded_attachment_app("p6c-save-race", "report.pdf", Some(b"i miei byte"));
        let pending = PendingAttachment {
            filename: "report.pdf".to_owned(),
            bytes: b"i miei byte".to_vec(),
            path: Some(path.clone()),
            dangerous: None,
            focus_cancel: false,
            overwrite_confirmed: false,
        };
        app.write_attachment(pending);
        assert_eq!(std::fs::read(&path).expect("the racing file survives"), b"creato da un altro processo");
        assert!(app.attachment_overwrite.is_some(), "the late file needs the overwrite sheet, not a silent replace");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_sync_time_names_its_date_when_it_is_not_today() {
        let now = Utc::now();
        assert_eq!(format_sync_time(now, now).len(), 5, "today stays a clock time");
        let yesterday = now - chrono::Duration::days(1);
        let shown = format_sync_time(yesterday, now);
        assert!(shown.contains('/'), "a sync from yesterday must carry a date: {shown}");
        assert!(shown.contains(':'), "and still a time: {shown}");
    }

    #[test]
    fn a_long_link_label_is_capped_but_keeps_the_full_url_in_the_sheet() {
        let long = format!("https://example.com/{}", "a".repeat(500));
        let shown = truncate_link_label(&long);
        assert!(shown.chars().count() <= reading::MAX_LINK_LABEL_CHARS, "{shown}");
        assert!(shown.ends_with('…'));
        assert_eq!(truncate_link_label("https://example.com/short"), "https://example.com/short");
        let tooltip = truncate_tooltip_url(&"t".repeat(reading::MAX_LINK_TOOLTIP_CHARS + 100));
        assert!(tooltip.chars().count() <= reading::MAX_LINK_TOOLTIP_CHARS, "{tooltip}");
        assert!(tooltip.ends_with('…'));
    }

    #[test]
    fn a_reply_uses_the_active_account_identity() {
        let mut app = booted_app("p6c-reply-identity", Vec::new(), |store| {
            seed_account_rows(store, "acc_a", "a@example.com", None);
            seed_account_rows(store, "acc_b", "b@example.com", None);
        });
        app.switch_account("acc_b".to_owned());
        let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("an acc_b message");
        app.reply_to(&id, ComposeMode::Reply);
        assert!(app.composer.open);
        assert_eq!(app.account().map(|account| account.id.as_str()), Some("acc_b"));
        app.composer.body.push_str("\nRisposta");
        app.composer.touch(0.0);
        assert!(app.save_composer(false));
        let draft = app.composer.draft_id.clone().expect("the reply draft");
        assert_eq!(app.engine.message(&draft).map(|message| message.summary.folder_id.clone()), Some("fld_acc_b_drafts".to_owned()));
    }

    #[test]
    fn the_outbox_renders_every_state_at_980x600_light_and_dark() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, dark);
            let mut app = PostinoApp::demo_at(true, Utc::now());
            app.outbox = demo_outbox(app.now_utc.timestamp_millis());
            app.outbox_open = true;
            app.outbox_loaded = true;
            frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            for expected in ["Coda di invio", "Invio incerto", "Non riuscito", "Dati demo", "In coda", "Inviato"] {
                assert!(text.iter().any(|line| line.contains(expected)), "the outbox is missing {expected:?}: {text:?}");
            }
        }
    }

    /// A store-backed app with an SMTP endpoint and the two folders a send needs. The IMAP port is
    /// closed, so the Sent APPEND fails and the row stays `append_pending`: that is exactly the state
    /// the UI must still reflect (the message was submitted, its server copy is pending).
    fn smtp_app(name: &str, smtp_port: u16, ca: CertificateDer<'static>) -> PostinoApp {
        let (path, key, vault) = fresh_store(name);
        {
            let store = postino_store::Store::open(&path, key.as_ref()).expect("seed store");
            crate::test_support::seed_smtp_account(&store, &vault, "acc_smtp", 1, smtp_port);
            for (suffix, display, kind) in
                [("drafts", "Bozze", postino_store::FolderKind::Drafts), ("sent", "Inviati", postino_store::FolderKind::Sent)]
            {
                store
                    .upsert_folder(&postino_store::Folder {
                        id: format!("fld_acc_smtp_{suffix}"),
                        account_id: "acc_smtp".to_owned(),
                        name: display.to_owned(),
                        kind,
                        unread: 0,
                        total: 0,
                        uid_validity: None,
                        uid_next: None,
                        last_sync_at: None,
                    })
                    .expect("folder");
            }
        }
        let vault_dyn: Arc<dyn CredentialVault> = vault;
        let booted =
            boot::boot(Launch::Store(Some(path.clone())), KeySource::Fixed { key: key.clone(), vault: vault_dyn }, Utc::now()).expect("boot");
        PostinoApp::from_booted(booted, true, Utc::now(), Launch::Store(Some(path)), vec![ca])
    }

    fn app_draft(app: &mut PostinoApp, body: &str) -> String {
        let content = postino_core::DraftContent {
            to: vec![postino_core::Address::new("", "one@example.com")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Prova".to_owned(),
            body_text: body.to_owned(),
            attachments: Vec::new(),
            in_reply_to: None,
        };
        app.engine.save_draft(None, None, content).expect("draft").summary.id
    }

    /// Advance the app's clock to `now_ms`, ask the worker for a pass and wait for its event.
    fn run_send_at(app: &mut PostinoApp, now_ms: i64) {
        app.now_utc = chrono::DateTime::<Utc>::from_timestamp_millis(now_ms).expect("instant");
        app.request_send();
        for _ in 0..1500 {
            app.handle_send_events();
            if !app.send_worker.as_ref().is_some_and(SendWorker::is_running) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the send worker did not finish");
    }

    #[test]
    fn ctrl_enter_respects_the_send_enabled_rule() {
        let (ctx, mut app) = ready_app();
        let wide = Vec2::new(1280.0, 720.0);
        app.compose_new();
        app.composer.to = "giulia@example.com".to_owned();
        app.composer.subject = "Scorciatoia".to_owned();
        app.composer.body = "corpo".to_owned();
        app.composer.touch(0.0);
        frame_at(&ctx, &mut app, wide, Vec::new());
        // The demo has no SMTP endpoint: the button is disabled, so the shortcut must do nothing.
        assert!(!app.send_allowed_for_active_account());
        frame_at(&ctx, &mut app, wide, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::Enter, Modifiers::COMMAND)]);
        assert!(app.composer.open, "Ctrl+Enter closed the composer without sending");
        assert!(app.composer.save_error.is_none(), "Ctrl+Enter attempted a send the button would refuse");
    }

    #[test]
    fn a_successful_send_reloads_the_mailbox_and_the_draft_leaves_drafts() {
        let smtp = RunningSmtp::start(1);
        let mut app = smtp_app("p7-app-send-reload", smtp.port(), smtp.ca());
        let draft_id = app_draft(&mut app, "corpo");
        let queued = app.engine.message_send(&draft_id).expect("queue");
        run_send_at(&mut app, queued.outbox.send_at);

        // The worker wrote through its own connection; the app reloaded the mailbox from the store.
        let sent = app.engine.message(&draft_id).expect("the Sent copy");
        assert_eq!(sent.summary.folder_id, "fld_acc_smtp_sent", "the stale draft is still in Drafts");
        assert_eq!(app.engine.messages().iter().filter(|message| message.folder_id == "fld_acc_smtp_sent").count(), 1);
        assert!(!app.engine.messages().iter().any(|message| message.folder_id == "fld_acc_smtp_drafts"), "a draft survived a send");
        // The old draft id is a Sent message now: sending it again must fail.
        assert!(app.engine.message_send(&draft_id).is_err(), "a stale draft could be sent again");
        assert_eq!(app.outbox.entries.iter().filter(|row| row.state == postino_store::OutboxState::AppendPending).count(), 1);
        let submitted = smtp.submitted();
        smtp.finish();
        assert_eq!(submitted.len(), 1);
    }

    #[test]
    fn a_row_queued_by_another_process_is_sent_when_due() {
        let smtp = RunningSmtp::start(1);
        let mut app = smtp_app("p7-app-external-row", smtp.port(), smtp.ca());
        let draft_id = app_draft(&mut app, "da un altro processo");
        // The row is created by a second process (the CLI) after the app booted; the app's cached
        // outbox is empty and knows nothing about it.
        let queued = {
            let mut cli = app.store.as_ref().expect("a store-backed app").open_engine().expect("cli engine");
            cli.message_send(&draft_id).expect("queue")
        };
        app.outbox = empty_outbox();
        app.outbox_loaded = true;
        assert!(app.next_outbox_due().is_some(), "the store query cannot see a row the cache does not have");
        app.now_utc = chrono::DateTime::<Utc>::from_timestamp_millis(queued.outbox.send_at).expect("instant");
        app.poll_outbox();
        assert!(app.send_worker.as_ref().is_some_and(SendWorker::is_running), "the scheduler did not wake for an external row");
        for _ in 0..600 {
            app.handle_send_events();
            if !app.send_worker.as_ref().is_some_and(SendWorker::is_running) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let submitted = smtp.submitted();
        smtp.finish();
        assert_eq!(submitted.len(), 1, "the externally queued row was not submitted");
        // The worker can report completion a frame before the app has consumed the sent row.
        for _ in 0..200 {
            app.poll_outbox();
            app.handle_send_events();
            if app.engine.message(&draft_id).is_some_and(|message| message.summary.folder_id == "fld_acc_smtp_sent") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.engine.message(&draft_id).map(|message| message.summary.folder_id.clone()), Some("fld_acc_smtp_sent".to_owned()));
    }

    #[test]
    fn a_send_from_another_process_reloads_the_app_mailbox() {
        let smtp = RunningSmtp::start(1);
        let mut app = smtp_app("p7-app-external-send", smtp.port(), smtp.ca());
        let draft_id = app_draft(&mut app, "inviata da un altro processo");
        // The CLI (a second engine and store connection) queues and submits it.
        let queued = {
            let mut cli = app.store.as_ref().expect("a store-backed app").open_engine().expect("cli engine");
            cli.set_test_extra_roots(vec![smtp.ca()]);
            let queued = cli.message_send(&draft_id).expect("queue");
            let report = cli.process_outbox(queued.outbox.send_at).expect("send");
            // The IMAP port is closed, so the server copy is pending; the submission happened.
            assert_eq!(report.sent, 0, "{report:?}");
            assert_eq!(report.append_pending, 1, "{report:?}");
            queued
        };
        // Before the poll the app still believes the draft is in Drafts; the engine guard refuses to
        // let that stale copy overwrite the Sent row.
        assert_eq!(app.engine.message(&draft_id).map(|message| message.summary.folder_id.clone()), Some("fld_acc_smtp_drafts".to_owned()));
        let stale = postino_core::DraftContent {
            to: vec![postino_core::Address::new("", "one@example.com")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Prova".to_owned(),
            body_text: "una modifica arrivata tardi".to_owned(),
            attachments: Vec::new(),
            in_reply_to: None,
        };
        let refused = app.engine.save_draft(None, Some(&draft_id), stale).expect_err("a stale Drafts save must be refused");
        assert!(
            refused.to_string().contains("changed in another session") || refused.to_string().contains("coda ("),
            "a stale save was not refused with a state reason: {refused}"
        );
        let stored = app.store.as_ref().expect("store").open_store().expect("store").get_message(&draft_id).expect("message").expect("row");
        assert_eq!(stored.summary.folder_id, "fld_acc_smtp_sent", "the stale save overwrote the Sent row");
        // The next scheduler tick sees the delivered row and reloads the whole mailbox.
        app.now_utc = chrono::DateTime::<Utc>::from_timestamp_millis(queued.outbox.send_at).expect("instant");
        app.poll_outbox();
        assert_eq!(app.engine.message(&draft_id).map(|message| message.summary.folder_id.clone()), Some("fld_acc_smtp_sent".to_owned()));
        assert_eq!(app.outbox.entries.iter().filter(|row| row.state == postino_store::OutboxState::AppendPending).count(), 1);
        let submitted = smtp.submitted();
        smtp.finish();
        assert_eq!(submitted.len(), 1);
    }

    #[test]
    fn the_outbox_dialog_offers_a_discard_and_shows_warnings() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, false);
        let mut app = PostinoApp::demo_at(true, Utc::now());
        app.outbox = demo_outbox(app.now_utc.timestamp_millis());
        app.outbox.warnings.push("una riga della coda è danneggiata".to_owned());
        app.outbox_open = true;
        app.outbox_loaded = true;
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Scarta")), "a failed row has no discard action: {text:?}");
        assert!(text.iter().any(|line| line.contains("danneggiata")), "a warning never reached the journal: {text:?}");
    }

    #[test]
    fn the_status_bar_outbox_button_opens_the_journal() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, false);
        let mut app = PostinoApp::demo_at(true, Utc::now());
        app.outbox = demo_outbox(app.now_utc.timestamp_millis());
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
        let button = probe::outbox_button_rect(&ctx).expect("the status bar has no outbox button when rows are active");
        assert!(button.width() > 0.0 && button.height() > 0.0);
        app.execute_command("outbox", &ctx);
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
        assert!(app.outbox_open);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Riprova manualmente")), "the uncertain row offers no manual retry: {text:?}");
        assert!(text.iter().any(|line| line.contains("Segna come inviato")), "the uncertain row offers no resolution: {text:?}");
    }

    #[test]
    fn send_failures_are_italian_and_never_raw_engine_text() {
        assert!(send_error_message("invalid arguments: draft is already queued", false).contains("già in coda"));
        assert!(send_error_message("account \"acc_1\" has no SMTP endpoint", false).contains("SMTP"));
        assert!(send_error_message("the demo mailbox has no outbox", true).contains("demo"));
        let attachment_refusal = send_error_message("attachment \"x.pdf\" has no stored bytes", false);
        assert!(attachment_refusal.to_lowercase().contains("allegato"), "{attachment_refusal}");
        assert!(attachment_refusal.contains("x.pdf"), "{attachment_refusal}");
        assert!(send_error_message("draft \"d\" has a failed submission; discard that row", false).contains("rifiutato"));
        assert!(!send_error_message("some internal sentence", false).contains("internal"));
    }

    fn label_unread(app: &PostinoApp, id: &str) -> Option<u32> {
        app.labels_cache.iter().find(|label| label.id == id).map(|label| label.unread_count)
    }

    fn has_label(app: &PostinoApp, message_id: &str, label_id: &str) -> bool {
        app.engine.message(message_id).is_some_and(|message| message.summary.labels.iter().any(|candidate| candidate == label_id))
    }
}
