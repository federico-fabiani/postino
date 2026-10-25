//! The egui shell. Panels are deliberately plain: folder rail, message table, reading pane.
//! Every action goes through `Engine::call`, the same operation registry the CLI and MCP expose.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, Utc};
use eframe::egui::containers::Sides;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Frame, Id, Key, Layout, Margin, Modifiers, Rect, ResizeDirection,
    RichText, Sense, Stroke, Ui, UiBuilder, UserData, Vec2, ViewportCommand, WidgetInfo, WidgetType,
};
use postino_core::{
    Account, Address, Attachment, DraftSave, DraftSend, Engine, FolderKind, LabelApply, LabelColor, LabelCreate, LabelDelete, LabelSummary,
    LabelUpdate, Message, MessageSummary, MessagesList, SearchResult,
};
use serde_json::json;

use crate::theme::{self, GOLD, IVORY, Tokens};
use crate::ui::icons;
use crate::ui::tokens::{inset, radius, size as ui_size, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{
    AUTOSAVE_DELAY, Button, ButtonSize, Chip, ComposeMode, Composer, ComposerFocus, ComposerState, Dialog, EmptyState, IconButton, IconButtonSize,
    ListRow, Menu, MenuItem, SearchField, SectionHeader, TextField, Toast, compose_body, quote_body, signature_body,
};

/// The shell scenes `--screenshot` can render. Add a label here and a branch in
/// [`PostinoApp::seed_screenshot`] to grow the set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotState {
    Shell,
    Reading,
    Compose,
    Palette,
    Labels,
    LabelMenu,
    LabelEditor,
}

impl ScreenshotState {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "shell" => Ok(Self::Shell),
            "reading" => Ok(Self::Reading),
            "compose" => Ok(Self::Compose),
            "palette" => Ok(Self::Palette),
            "labels" => Ok(Self::Labels),
            "label_menu" => Ok(Self::LabelMenu),
            "label_editor" => Ok(Self::LabelEditor),
            other => Err(format!(
                "unknown screenshot state {other:?}; expected one of: shell, reading, compose, palette, labels, label_menu, label_editor"
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
/// The pane opens at 52% of the window and resizes between 420 px and 62%.
const READING_PANE_DEFAULT_RATIO: f32 = 0.52;
const READING_PANE_MAX_RATIO: f32 = 0.62;
const READING_PANE_MIN_SIZE: f32 = 420.0;
/// The message list shows avatars only when its column is at least this wide (`docs/ui/plan.md`).
const AVATAR_MIN_LIST_WIDTH: f32 = 720.0;
/// `label_apply` accepts at most 500 ids per call; undoing a delete restores in these batches.
const LABEL_APPLY_CHUNK: usize = 500;

pub struct PostinoApp {
    engine: Engine,
    folder: String,
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
    unread_only: bool,
    attachments_only: bool,
    dark: bool,
    status: String,
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
    composer: ComposerState,
    /// Deterministic PNG capture, set only by `--screenshot`.
    screenshot: Option<ScreenshotMode>,
}

impl PostinoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, custom_chrome: bool) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, false);
        Self::demo(custom_chrome)
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
                }
            }
            ScreenshotState::Compose => {
                let mut composer = ComposerState::new(ComposeMode::New);
                composer.to = "Giulia Ferri <giulia.ferri@example.com>".to_owned();
                composer.cc = "Marco Bianchi <marco.bianchi@example.com>".to_owned();
                composer.show_cc = true;
                composer.subject = "Preventivo sito web — bozza finale".to_owned();
                composer.body = "Ciao Giulia,\n\nti confermo il preventivo per il sito: 2.400 € + IVA, consegna in tre settimane.\nResto a disposizione per la call di lunedì.\n\n-- \nFederico\nfederico@example.it"
                    .to_owned();
                composer.attachments = vec![Attachment {
                    id: "att_screenshot".to_owned(),
                    filename: "preventivo-postino.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 248_320,
                }];
                composer.saved_at = Some(format_time(self.now_utc));
                composer.focus = Some(ComposerFocus::Body);
                self.start_composer(composer);
            }
            ScreenshotState::Palette => {
                self.show_palette = true;
                self.palette_focus = true;
                self.palette_query = "scr".to_owned();
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
                    // The captured targets are the visible message; the Lavoro chip stays visible in
                    // the reading pane behind the modal scrim.
                    self.open_label_menu(vec![id]);
                }
            }
        }
    }

    fn demo(custom_chrome: bool) -> Self {
        Self::demo_at(custom_chrome, Utc::now())
    }

    fn demo_at(custom_chrome: bool, now: DateTime<Utc>) -> Self {
        let engine = Engine::demo_at(now);
        let labels_cache = engine.label_summaries();
        Self {
            engine,
            folder: "inbox".to_owned(),
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
            unread_only: false,
            attachments_only: false,
            dark: false,
            status: "Demo · dati sintetici, nessun server collegato".to_owned(),
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
            composer: ComposerState::closed(),
            screenshot: None,
        }
    }

    fn select_folder(&mut self, folder: impl Into<String>) {
        self.folder = folder.into();
        // A folder click narrows an active label view to the intersection instead of dropping the
        // filter; "Mostra tutte" is the explicit way back to the whole mail list.
        if self.label_filter.is_some() {
            self.label_folder = Some(self.folder.clone());
        }
        self.label_editor = None;
        self.label_menu = None;
        self.query.clear();
        self.clear_selection();
        self.reading_open = false;
        self.reading_full = false;
    }

    fn refresh_labels(&mut self) {
        self.labels_cache = self.engine.label_summaries();
    }

    fn label_summary(&self, id: &str) -> Option<&LabelSummary> {
        self.labels_cache.iter().find(|label| label.id == id)
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
        self.query.clear();
        self.unread_only = false;
        self.attachments_only = false;
        self.clear_selection();
        self.reading_open = false;
        self.reading_full = false;
    }

    fn clear_label_filter(&mut self) {
        self.label_filter = None;
        self.label_folder = None;
    }

    fn start_label_create(&mut self) {
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
        let Some(label) = self.engine.label(id) else {
            self.status = format!("Etichetta {id:?} non trovata");
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
            self.status = format!("Etichetta {id:?} non trovata");
            return;
        };
        let message_ids = self
            .engine
            .messages()
            .iter()
            .filter(|message| message.summary.labels.iter().any(|candidate| candidate == id))
            .map(|message| message.summary.id.clone())
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
                Err(error) => self.status = format!("Errore: {error}"),
            },
            Err(error) => self.status = format!("Errore: {error}"),
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
            query: self.query.clone(),
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
            || menu.context.query != self.query
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
            self.status = format!("Etichetta {id:?} non trovata");
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
                Err(error) => self.status = format!("Errore: {error}"),
            },
            Err(error) => self.status = format!("Errore: {error}"),
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
            RowAction::Archive => self.move_many(&targets, "archive", "Messaggio archiviato", "Messaggi archiviati"),
            RowAction::Delete => self.move_many(&targets, "trash", "Messaggio spostato nel cestino", "Messaggi spostati nel cestino"),
        }
    }

    fn bulk_bar(&mut self, ui: &mut Ui, t: &Tokens) {
        let ids = self.action_ids();
        if ids.len() < 2 {
            return;
        }
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
            Some(BulkAction::Archive) => self.move_many(&ids, "archive", "Messaggio archiviato", "Messaggi archiviati"),
            Some(BulkAction::Delete) => self.move_many(&ids, "trash", "Messaggio spostato nel cestino", "Messaggi spostati nel cestino"),
            Some(BulkAction::Clear) => self.clear_selection(),
            None => {}
        }
    }

    fn close_reading(&mut self) {
        self.reading_open = false;
        self.reading_full = false;
        self.pending_read = None;
    }

    fn move_selected(&mut self, folder: &str, confirmation: &str, plural: &str) {
        let ids = self.action_ids();
        self.move_many(&ids, folder, confirmation, plural);
    }

    fn move_many(&mut self, ids: &[String], folder: &str, confirmation: &str, plural: &str) {
        let mut moved = Vec::new();
        for id in ids {
            let Some(previous) = self.engine.message(id).map(|message| message.summary.folder_id.clone()) else {
                continue;
            };
            match self.engine.call("message_move", json!({ "id": id, "folder": folder })) {
                Ok(_) => moved.push((id.clone(), previous)),
                Err(error) => {
                    self.status = format!("Errore: {error}");
                    return;
                }
            }
        }
        if moved.is_empty() {
            return;
        }
        let message = if moved.len() == 1 { confirmation.to_owned() } else { plural.to_owned() };
        self.status = message.clone();
        self.clear_selection();
        self.reading_open = false;
        self.reading_full = false;
        self.push_toast(message, UndoAction::Move(moved));
    }

    fn step_selection(&mut self, delta: i32, extend: bool) {
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
            self.focus_message(message.id.clone());
        }
        self.scroll_to_selected = true;
    }

    fn set_flag_for(&mut self, id: &str, unread: Option<bool>, starred: Option<bool>) -> bool {
        match self.engine.call("message_flag", json!({ "id": id, "unread": unread, "starred": starred })) {
            Ok(_) => true,
            Err(error) => {
                self.status = format!("Errore: {error}");
                false
            }
        }
    }

    fn toggle_star(&mut self) {
        let ids = self.action_ids();
        if ids.is_empty() {
            return;
        }
        let all_starred = ids.iter().all(|id| self.engine.message(id).is_some_and(|message| message.summary.starred));
        self.set_starred_many(&ids, !all_starred);
    }

    fn set_starred(&mut self, starred: bool) {
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
            format!("Stella aggiunta a {} messaggi", changes.len())
        } else {
            format!("Stella rimossa da {} messaggi", changes.len())
        };
        self.status = message.clone();
        self.push_toast(message, UndoAction::Starred(changes));
    }

    fn toggle_read(&mut self) {
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
            format!("{} messaggi segnati come non letti", changes.len())
        } else {
            format!("{} messaggi segnati come letti", changes.len())
        };
        self.status = message.clone();
        self.push_toast(message, UndoAction::Unread(changes));
        self.refresh_labels();
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
        self.toast = Some(ToastState { message: message.into(), undo, until: self.now + 8.0 });
    }

    fn undo_last(&mut self) {
        let Some(toast) = self.toast.take() else {
            return;
        };
        let mut error = None;
        match toast.undo {
            UndoAction::Unread(changes) => {
                for (id, unread) in changes {
                    if let Err(cause) = self.engine.call("message_flag", json!({ "id": id, "unread": unread, "starred": null })) {
                        error = Some(cause);
                        break;
                    }
                }
            }
            UndoAction::Starred(changes) => {
                for (id, starred) in changes {
                    if let Err(cause) = self.engine.call("message_flag", json!({ "id": id, "unread": null, "starred": starred })) {
                        error = Some(cause);
                        break;
                    }
                }
            }
            UndoAction::Move(changes) => {
                for (id, folder) in changes {
                    if let Err(cause) = self.engine.call("message_move", json!({ "id": id, "folder": folder })) {
                        error = Some(cause);
                        break;
                    }
                }
            }
            UndoAction::Send(id) => {
                if let Err(cause) = self.engine.call("message_move", json!({ "id": id, "folder": "drafts" })) {
                    error = Some(cause);
                }
            }
            UndoAction::Labels(changes) => {
                for (message_id, label_id, was_applied) in changes {
                    if let Err(cause) = self.engine.call("label_apply", json!({ "id": label_id, "message_ids": [message_id], "apply": was_applied }))
                    {
                        error = Some(cause);
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
                            if let Err(cause) =
                                self.engine.call("label_apply", json!({ "id": created.label.id, "message_ids": chunk, "apply": true }))
                            {
                                error = Some(cause);
                                break;
                            }
                        }
                    }
                    Err(cause) => error = Some(postino_core::OpError::Failed(cause.to_string())),
                },
                Err(cause) => error = Some(cause),
            },
            UndoAction::LabelRename { id, name, color } => {
                if let Err(cause) = self.engine.call("label_update", json!({ "id": id, "name": name, "color": color })) {
                    error = Some(cause);
                }
            }
        }
        if error.is_none() {
            self.refresh_labels();
        }
        match error {
            Some(error) => self.status = format!("Errore: {error}"),
            None => self.status = "Annullato".to_owned(),
        }
    }

    fn visible_messages(&mut self) -> (Vec<MessageSummary>, usize, usize) {
        let searching = !self.query.trim().is_empty();
        // A label view spans every folder until the user narrows it. A plain view is one folder.
        let label_scoped = self.label_filter.is_some();
        let list_folder = if label_scoped { self.label_folder.clone() } else { Some(self.folder.clone()) };
        let search_folder = if label_scoped { self.label_folder.clone() } else { None };
        let (name, args) = if searching {
            ("search", json!({ "query": self.query, "label": self.label_filter, "folder": search_folder, "limit": 200 }))
        } else {
            (
                "messages_list",
                json!({
                    "folder": list_folder,
                    "unread_only": self.unread_only,
                    "with_attachments": self.attachments_only,
                    "label": self.label_filter,
                    "limit": 200
                }),
            )
        };
        match self.engine.call(name, args) {
            Ok(value) if searching => match serde_json::from_value::<SearchResult>(value) {
                Ok(result) => {
                    let mut rows = result.messages;
                    if self.unread_only {
                        rows.retain(|message| message.unread);
                    }
                    if self.attachments_only {
                        rows.retain(|message| message.has_attachments);
                    }
                    let unread = rows.iter().filter(|message| message.unread).count();
                    let total = rows.len();
                    (rows, total, unread)
                }
                Err(error) => {
                    self.status = format!("Errore: {error}");
                    (Vec::new(), 0, 0)
                }
            },
            Ok(value) => match serde_json::from_value::<MessagesList>(value) {
                Ok(result) => (result.messages, result.total, result.unread),
                Err(error) => {
                    self.status = format!("Errore: {error}");
                    (Vec::new(), 0, 0)
                }
            },
            Err(error) => {
                self.status = format!("Errore: {error}");
                (Vec::new(), 0, 0)
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
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
        // Only a real text field may swallow the single-key map. `egui_wants_keyboard_input()`
        // is also true for a focused row or button, and clicking a label left the app deaf to
        // F2/L/J/K/? until Esc.
        let typing = ctx.text_edit_focused();
        ctx.input(|input| {
            palette = input.modifiers.command && input.key_pressed(Key::K);
            escape = input.key_pressed(Key::Escape);
            shortcuts = !typing && !input.modifiers.command && input.key_pressed(Key::Questionmark);
            enter = !typing && !input.modifiers.command && input.key_pressed(Key::Enter);
            open = !typing && !input.modifiers.command && input.key_pressed(Key::O);
            next = !typing && !input.modifiers.command && (input.key_pressed(Key::J) || input.key_pressed(Key::ArrowDown));
            previous = !typing && !input.modifiers.command && (input.key_pressed(Key::K) || input.key_pressed(Key::ArrowUp));
            extend = input.modifiers.shift;
            mark_unread = !typing && !input.modifiers.command && input.key_pressed(Key::U);
            star = !typing && !input.modifiers.command && input.key_pressed(Key::S);
            archive = !typing && !input.modifiers.command && input.key_pressed(Key::E);
            delete = !typing && !input.modifiers.command && input.key_pressed(Key::Delete);
            undo = !typing && !input.modifiers.command && input.key_pressed(Key::Z);
            toggle = !typing && !input.modifiers.command && (input.key_pressed(Key::Space) || input.key_pressed(Key::X));
            select_all = !typing && input.modifiers.command && input.key_pressed(Key::A);
            focus_search = !typing && !input.modifiers.command && input.key_pressed(Key::Slash);
            compose = !typing && !input.modifiers.command && input.key_pressed(Key::C);
            reply = !typing && !input.modifiers.command && input.key_pressed(Key::R);
            reply_all = !typing && !input.modifiers.command && input.key_pressed(Key::A);
            forward = !typing && !input.modifiers.command && input.key_pressed(Key::F);
            labels = !typing && !input.modifiers.command && input.key_pressed(Key::L);
            rename_label = !typing && !input.modifiers.command && input.key_pressed(Key::F2);
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
            if self.reading_open {
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
            self.move_selected("archive", "Messaggio archiviato", "Messaggi archiviati");
        }
        if delete {
            self.move_selected("trash", "Messaggio spostato nel cestino", "Messaggi spostati nel cestino");
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
        let ids = self.action_ids();
        self.set_unread_many(&ids, true);
    }

    fn mark_all_read(&mut self, folder: &str) {
        let ids = self
            .engine
            .messages()
            .iter()
            .filter(|message| message.summary.unread)
            .filter(|message| if folder == "starred" { message.summary.starred } else { message.summary.folder_id == folder })
            .map(|message| message.summary.id.clone())
            .collect::<Vec<_>>();
        self.set_unread_many(&ids, false);
    }

    fn account(&self) -> Option<&Account> {
        self.engine.accounts().first()
    }

    fn start_composer(&mut self, mut state: ComposerState) {
        if self.composer.open && self.composer.dirty && !self.save_composer(false) {
            return;
        }
        state.open = true;
        self.composer = state;
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
        state.body = signature_body(&account.signature);
        state.focus = Some(ComposerFocus::To);
        self.start_composer(state);
    }

    fn reply_to(&mut self, id: &str, mode: ComposeMode) {
        let Some(message) = self.engine.message(id).cloned() else {
            self.status = "Messaggio non trovato".to_owned();
            return;
        };
        let Some(account) = self.account().cloned() else {
            self.status = "Nessun account configurato".to_owned();
            return;
        };
        let own = account.email.trim().to_ascii_lowercase();
        let (to, cc) =
            if mode == ComposeMode::Forward { (Vec::new(), Vec::new()) } else { reply_recipients(&message, &own, mode == ComposeMode::ReplyAll) };
        let subject = match mode {
            ComposeMode::Forward => forward_subject(&message.summary.subject),
            _ => reply_subject(&message.summary.subject),
        };
        let quoted = match mode {
            ComposeMode::Forward => Some(forward_block(&message)),
            ComposeMode::Reply | ComposeMode::ReplyAll => Some(reply_block(&message)),
            ComposeMode::New | ComposeMode::Draft => None,
        };
        let mut state = ComposerState::new(mode);
        state.to = Address::format_list(&to);
        state.cc = Address::format_list(&cc);
        state.show_cc = !cc.is_empty();
        state.show_bcc = false;
        state.subject = subject;
        state.body = compose_body(&account.signature, quoted);
        state.attachments = if mode == ComposeMode::Forward { message.attachments.clone() } else { Vec::new() };
        state.focus = Some(ComposerFocus::Body);
        self.start_composer(state);
    }

    fn edit_draft(&mut self, id: &str) {
        let Some(message) = self.engine.message(id).cloned() else {
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
        state.saved_at = Some(format_time(message.summary.date));
        state.focus = Some(ComposerFocus::Body);
        self.start_composer(state);
    }

    fn save_composer(&mut self, notify: bool) -> bool {
        if !self.composer.open {
            return false;
        }
        if !self.composer.dirty && self.composer.draft_id.is_none() {
            if notify {
                self.status = "Nessuna modifica da salvare".to_owned();
            }
            return true;
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
        let mut args = json!({
            "to": to,
            "cc": cc,
            "bcc": bcc,
            "subject": self.composer.subject,
            "body_text": self.composer.body,
            "attachments": self.composer.attachments,
        });
        if let Some(id) = &self.composer.draft_id
            && let Some(object) = args.as_object_mut()
        {
            object.insert("id".to_owned(), json!(id));
        }
        let result = self.engine.call("draft_save", args);
        match result {
            Ok(value) => match serde_json::from_value::<DraftSave>(value) {
                Ok(saved) => {
                    let id = saved.draft.summary.id.clone();
                    self.composer.draft_id = Some(id);
                    self.composer.attachments = saved.draft.attachments;
                    self.composer.mark_saved(format_time(self.now_utc));
                    if notify {
                        self.status = "Bozza salvata".to_owned();
                    }
                    true
                }
                Err(error) => {
                    self.composer.mark_failed(format!("risposta non valida: {error}"), self.now);
                    false
                }
            },
            Err(error) => {
                self.composer.mark_failed(error.to_string(), self.now);
                if notify {
                    self.status = format!("Errore: {error}");
                }
                false
            }
        }
    }

    fn send_composer(&mut self) {
        if !self.composer.open {
            return;
        }
        if !self.save_composer(true) {
            return;
        }
        let Some(id) = self.composer.draft_id.clone() else {
            self.composer.mark_failed("Aggiungi almeno un destinatario prima di inviare", self.now);
            return;
        };
        match self.engine.call("draft_send", json!({ "id": id })) {
            Ok(value) => match serde_json::from_value::<DraftSend>(value) {
                Ok(sent) => {
                    self.composer = ComposerState::closed();
                    self.status = format!(
                        "Messaggio spostato in {} (in locale, nessun SMTP)",
                        self.engine
                            .folders()
                            .iter()
                            .find(|folder| folder.id == sent.message.folder_id)
                            .map_or("Inviati", |folder| folder.name.as_str())
                    );
                    self.push_toast("Bozza spostata in Inviati: nessun messaggio è stato inviato", UndoAction::Send(id));
                }
                Err(error) => self.composer.mark_failed(format!("risposta non valida: {error}"), self.now),
            },
            Err(error) => self.composer.mark_failed(error.to_string(), self.now),
        }
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
            && self.engine.message(&id).is_some()
        {
            match self.engine.call("message_move", json!({ "id": id, "folder": "trash" })) {
                Ok(_) => {
                    self.composer = ComposerState::closed();
                    self.status = "Bozza spostata nel cestino".to_owned();
                    self.push_toast("Bozza scartata", UndoAction::Move(vec![(id, "drafts".to_owned())]));
                }
                Err(error) => self.composer.mark_failed(error.to_string(), self.now),
            }
            return;
        }
        self.composer = ComposerState::closed();
        self.status = "Bozza scartata".to_owned();
    }

    fn tick(&mut self, ctx: &egui::Context) {
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
    }

    fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some(state) = &self.toast else {
            return;
        };
        let message = state.message.clone();
        if let Some(response) = Toast::new(&message).undo("Annulla").show_overlay(ctx, "postino-toast")
            && response.clicked()
        {
            self.undo_last();
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

                let wordmark_rect = Rect::from_min_max(rect.min, egui::pos2(rect.min.x + 240.0, rect.max.y));
                let controls_rect = Rect::from_min_max(egui::pos2(rect.max.x - controls_width, rect.min.y), rect.max);
                ui.scope_builder(UiBuilder::new().max_rect(wordmark_rect).layout(Layout::left_to_right(Align::Center)), |ui| {
                    ui.label(icons::icon(icons::MAIL, 16.0, theme::GOLD));
                    ui.add_space(space::SM);
                    ui.label(RichText::new("postino").font(FontId::new(15.0, theme::display())).color(theme::IVORY));
                });
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
                    if let Some(account) = self.engine.accounts().first() {
                        let details = if account.provider.trim().is_empty() {
                            account.email.clone()
                        } else {
                            format!("{} · {}", account.email, account.provider)
                        };
                        avatar(ui, 24.0, &initials(&account.name), t.gold, theme::FOREST);
                        ui.add_space(space::SM);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(typography::strong(Role::BodySmall, account.name.clone(), t.text));
                            ui.label(typography::text(Role::Caption, details, t.text_faint));
                        });
                    }
                });
                ui.scope_builder(UiBuilder::new().max_rect(search_rect).layout(Layout::left_to_right(Align::Center)), |ui| {
                    let placeholder = self.search_placeholder();
                    let _ = SearchField::new(&mut self.query, &placeholder).width(search_width).focus(self.focus_search).show(ui);
                    if self.focus_search {
                        self.focus_search = false;
                    }
                });
                ui.scope_builder(UiBuilder::new().max_rect(right_rect).layout(Layout::right_to_left(Align::Center)), |ui| {
                    if Button::primary("Scrivi").icon(icons::COMPOSE).shortcut("C").show(ui).clicked() {
                        self.compose_new();
                    }
                    ui.add_space(space::SM);
                    let theme_icon = if self.dark { icons::SUN } else { icons::MOON };
                    if IconButton::new(theme_icon, "Cambia tema").show(ui).clicked() {
                        self.dark = !self.dark;
                        theme::apply(ui.ctx(), self.dark);
                    }
                });
            });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        egui::Panel::left("folders")
            .resizable(false)
            .exact_size(ui_size::SIDEBAR)
            .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(inset::LG, inset::XL)))
            .show_separator_line(false)
            .show(ui, |ui| {
                let footer_height = 42.0;
                let labels = self.labels_cache.clone();
                let editor_open = self.label_editor.is_some();
                let mut folder_action = None;
                let mut label_action = None;
                let mut editor_action = LabelEditorAction::None;

                // Footer first so it stays pinned; everything above it is one scroll area. At
                // 980×600 with the inline editor open the content overflows, and the scroll bar is
                // the affordance that keeps Cestino and the label rows reachable. Two fixed
                // sub-panels used to clip the folder list instead of scrolling it.
                egui::Panel::bottom("sidebar-footer").exact_size(footer_height).frame(Frame::NONE).show_separator_line(false).show(ui, |ui| {
                    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                        ui.label(typography::text(Role::Caption, format!("Postino {} · M0 + U0.5", env!("CARGO_PKG_VERSION")), t.text_faint));
                        ui.label(typography::text(Role::Caption, "Locale · offline", t.text_faint));
                    });
                });

                #[cfg(test)]
                probe::clear_rows(ui);
                // A visible bar, local to the sidebar: egui's default is a 2 pt floating sliver
                // that only appears on hover, which at 980×600 hid the overflow instead of
                // revealing it. Text colour on ivory is 10:1, and on the dark panel 12:1.
                ui.spacing_mut().scroll = sidebar_scroll_style();
                let scroll = egui::ScrollArea::vertical()
                    .id_salt("sidebar-scroll")
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        // Rows sit flush instead of carrying a two-pixel gap each.
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.scope(|ui| {
                            ui.spacing_mut().interact_size.y = 20.0;
                            SectionHeader::new("CARTELLE").show(ui);
                        });
                        ui.add_space(space::XS);
                        let folders = self.engine.folders().to_vec();
                        for folder in folders {
                            let meta = if folder.kind == FolderKind::Drafts && folder.total > 0 {
                                folder.total.to_string()
                            } else if folder.unread > 0 {
                                folder.unread.to_string()
                            } else {
                                String::new()
                            };
                            let row = ListRow::folder(folder.name.as_str(), meta.as_str())
                                .icon(folder_icon(folder.kind))
                                .selected(if self.label_filter.is_some() {
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
                                    MenuItem::new("Rinomina").enabled(false).reason("Le cartelle locali arrivano con lo store M1"),
                                    MenuItem::new("Elimina").enabled(false).reason("Le cartelle locali arrivano con lo store M1"),
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
                        // The editor lives directly under the ETICHETTE header, before the rows,
                        // so the field, its error, the palette and the pointer controls stay
                        // together however long the list grows.
                        if let Some(editor) = self.label_editor.as_mut() {
                            let focus = editor.focus;
                            let editor_scope = ui.scope(|ui| label_editor_widget(ui, editor, &t));
                            let rect = editor_scope.response.rect;
                            if focus {
                                // Opening the editor must reveal it, not leave its controls under
                                // the footer at the minimum window size.
                                ui.scroll_to_rect(rect.expand2(Vec2::new(0.0, space::MD)), Some(Align::BOTTOM));
                            }
                            editor_action = editor_scope.inner;
                        }
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
                    });
                #[cfg(test)]
                probe::scroll(ui, scroll.inner_rect, scroll.content_size);
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
                    LabelEditorAction::Save => self.commit_label_editor(),
                    LabelEditorAction::Cancel => self.cancel_label_editor(),
                    LabelEditorAction::None => {}
                }
            });
    }

    fn list_panel(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
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
        let searching = !self.query.trim().is_empty();
        let multi = self.selection.len() > 1;
        let mut clear_filter = false;
        // The header always names the scope, so the label counts cannot read as folder-local.
        let scope = label_filter.as_ref().map(|(_, _, _)| match &narrowed_folder_name {
            Some(name) => format!("in {name}"),
            None => "in tutte le cartelle".to_owned(),
        });
        let meta = match &scope {
            Some(scope) => format!("{} · {} · {scope}", unread_text(unread), messages_text(total)),
            None => format!("{} · {}", unread_text(unread), messages_text(total)),
        };
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL, inset::LG))).show(ui, |ui| {
            if multi {
                self.bulk_bar(ui, &t);
            } else {
                // The action is reserved first so it can never be clipped; the title and the scope
                // share the remaining width on two lines. At 1280 with the reading pane open the
                // list is about 375 px wide and the scope would not fit beside the title.
                let title = label_filter.as_ref().map_or(current_folder_name.clone(), |(_, name, _)| name.clone());
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
                    ui.label(typography::text(Role::Meta, format!("Risultati per “{}”", self.query.trim()), t.text_muted));
                    if Button::ghost("Cancella").size(ButtonSize::Sm).show(ui).clicked() {
                        self.query.clear();
                    }
                });
            }
            ui.add_space(space::MD);
            if rows.is_empty() {
                let mut empty_action = None;
                let background = ui.interact(ui.available_rect_before_wrap(), ui.id().with("empty-list-menu"), Sense::click());
                let has_label_filter = label_filter.is_some();
                background.context_menu(|ui| {
                    let items = [
                        menu_item("label_new"),
                        menu_item("label_clear_filter").enabled(has_label_filter).reason("Nessun filtro etichetta attivo"),
                        MenuItem::separator(),
                        menu_item("refresh"),
                        menu_item("select_all").enabled(false).reason("Non ci sono messaggi da selezionare"),
                        menu_item("compose"),
                        MenuItem::new("Cancella ricerca").icon(icons::CLOSE).enabled(searching).reason("Nessuna ricerca attiva"),
                    ];
                    match Menu::new(&items).show(ui) {
                        Some(0) => {
                            empty_action = Some(EmptyAction::NewLabel);
                            ui.close();
                        }
                        Some(1) => {
                            empty_action = Some(EmptyAction::ClearLabel);
                            ui.close();
                        }
                        Some(3) => {
                            empty_action = Some(EmptyAction::Refresh);
                            ui.close();
                        }
                        Some(5) => {
                            empty_action = Some(EmptyAction::Compose);
                            ui.close();
                        }
                        Some(6) => {
                            empty_action = Some(EmptyAction::ClearSearch);
                            ui.close();
                        }
                        _ => {}
                    }
                });
                let label_name = label_filter.as_ref().map(|(_, name, _)| name.as_str());
                let (icon, title, body, action) = if let Some(name) = label_name {
                    let scope = scope.clone().unwrap_or_else(|| "in tutte le cartelle".to_owned());
                    (icons::TAG, "Nessun messaggio", format!("Nessun messaggio con l'etichetta “{name}” {scope}."), Some("Mostra tutte"))
                } else if searching {
                    (icons::SEARCH, "Nessun risultato", "Nessun messaggio corrisponde alla ricerca.".to_owned(), Some("Cancella ricerca"))
                } else if self.folder == "drafts" {
                    (icons::DRAFTS, "Nessuna bozza", "Le bozze si salvano da sole mentre scrivi.".to_owned(), Some("Scrivi"))
                } else if self.folder == "inbox" {
                    (icons::INBOX, "Nessun messaggio", "La posta in arrivo è vuota.".to_owned(), None)
                } else {
                    (icons::INBOX, "Nessun messaggio", "Questa cartella è vuota.".to_owned(), Some("Vai a Posta in arrivo"))
                };
                let mut state = EmptyState::new(icon, title, &body);
                if let Some(action) = action {
                    state = state.action(action);
                }
                if let Some(response) = state.show(ui)
                    && response.clicked()
                {
                    if label_name.is_some() {
                        self.clear_label_filter();
                    } else if searching {
                        self.query.clear();
                    } else if self.folder == "drafts" {
                        self.compose_new();
                    } else {
                        self.select_folder("inbox");
                    }
                }
                match empty_action {
                    Some(EmptyAction::NewLabel) => self.start_label_create(),
                    Some(EmptyAction::ClearLabel) => self.clear_label_filter(),
                    Some(EmptyAction::Refresh) => self.status = "Dati demo già aggiornati".to_owned(),
                    Some(EmptyAction::Compose) => self.compose_new(),
                    Some(EmptyAction::ClearSearch) => self.query.clear(),
                    None => {}
                }
                return;
            }

            let mut clicked = None;
            let mut star_clicked = None;
            let mut context_clicked = None;
            let mut context_action = None;
            let modifiers = ui.input(|input| input.modifiers);
            let list_width = ui.available_width();
            let show_avatars = show_list_avatars(list_width);
            // A list squeezed by the 52% reading pane trades sender/date space for the subject.
            let sender_width = if list_width < 560.0 { 96.0 } else { 140.0 };
            let date_width = if list_width < 560.0 { 44.0 } else { 52.0 };
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut focus_rect = None;
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
                table
                    .column(egui_extras::Column::exact(sender_width))
                    .column(egui_extras::Column::exact(date_width))
                    .column(egui_extras::Column::remainder())
                    .column(egui_extras::Column::exact(24.0))
                    .header(24.0, |mut header| {
                        header.col(|_| {});
                        if show_avatars {
                            header.col(|_| {});
                        }
                        header.col(|ui| {
                            ui.add(egui::Label::new(typography::text(Role::Caption, "MITTENTE", t.text_faint)).selectable(false));
                        });
                        header.col(|ui| {
                            ui.add(egui::Label::new(typography::text(Role::Caption, "DATA", t.text_faint)).selectable(false));
                        });
                        header.col(|ui| {
                            ui.add(egui::Label::new(typography::text(Role::Caption, "OGGETTO", t.text_faint)).selectable(false));
                        });
                        header.col(|_| {});
                    })
                    .body(|mut body| {
                        for message in &rows {
                            let focused = self.selected.as_deref() == Some(message.id.as_str());
                            let selected = self.selection.contains(&message.id);
                            let foreground = if selected { t.selected_text } else { t.text };
                            body.row(ui_size::ROW, |mut row| {
                                row.set_selected(selected);
                                row.col(|ui| {
                                    if message.unread {
                                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                        ui.painter().circle_filled(rect.center(), 3.0, GOLD);
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
                                    let text = RichText::new(name)
                                        .font(FontId::new(13.0, if message.unread { theme::medium() } else { FontFamily::Proportional }))
                                        .color(foreground);
                                    ui.add(egui::Label::new(text).truncate().selectable(false));
                                });
                                row.col(|ui| {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.add(
                                            egui::Label::new(typography::mono(
                                                format_when(message.date, now_utc),
                                                10.5,
                                                if selected { t.selected_muted } else { t.text_faint },
                                            ))
                                            .selectable(false),
                                        );
                                    });
                                });
                                row.col(|ui| {
                                    // The cell layout starts content at the top. Centre the two lines
                                    // explicitly so the subject never touches the selected row edge.
                                    let top = ((ui.available_height() - 28.0) * 0.5).max(0.0);
                                    ui.add_space(top);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        let subject_text =
                                            if message.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.subject.as_str() };
                                        let subject = RichText::new(subject_text)
                                            .font(FontId::new(13.0, if message.unread { theme::medium() } else { FontFamily::Proportional }))
                                            .color(foreground);
                                        ui.add(egui::Label::new(subject).truncate().selectable(false));
                                        let snippet = RichText::new(message.snippet.as_str()).size(11.0).color(if selected {
                                            t.selected_muted
                                        } else {
                                            t.text_muted
                                        });
                                        ui.add(egui::Label::new(snippet).truncate().selectable(false));
                                    });
                                });
                                row.col(|ui| {
                                    let star_color = if message.starred {
                                        if selected { t.gold } else { theme::GOLD_INK }
                                    } else if selected {
                                        t.selected_muted
                                    } else {
                                        t.text_faint
                                    };
                                    let star = if message.starred {
                                        icons::icon_filled(icons::STAR_FILL, 13.0, star_color)
                                    } else {
                                        icons::icon(icons::STAR, 13.0, star_color)
                                    };
                                    let response = ui.add(egui::Label::new(star).sense(Sense::click()).selectable(false));
                                    if response.clicked() {
                                        star_clicked = Some(message.id.clone());
                                    }
                                    if response.hovered() {
                                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                    }
                                });
                                let response = row.response();
                                if focused && self.scroll_to_selected {
                                    response.scroll_to_me(None);
                                }
                                if focused && !selected {
                                    focus_rect = Some(response.rect.shrink(1.0));
                                }
                                if response.secondary_clicked() {
                                    context_clicked = Some(message.id.clone());
                                }
                                if response.clicked() {
                                    clicked = Some((message.id.clone(), modifiers));
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
                if let Some(rect) = focus_rect {
                    crate::ui::tokens::paint_focus_ring(ui, rect, radius::CONTROL, &t);
                }
            });
            self.scroll_to_selected = false;
            if let Some(id) = star_clicked {
                self.toggle_star_for(&id);
            } else {
                if let Some(id) = context_clicked
                    && !self.selection.contains(&id)
                {
                    self.focus_message(id);
                }
                if let Some((id, modifiers)) = clicked {
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
            let default_width = reading_pane_width(available);
            let max_width = (available * READING_PANE_MAX_RATIO).max(READING_PANE_MIN_SIZE);
            egui::Panel::right("reading")
                .resizable(true)
                .default_size(default_width)
                .min_size(READING_PANE_MIN_SIZE)
                .max_size(max_width)
                .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(inset::LG, inset::XL)))
                .show(ui, |ui| self.reading_content(ui, &t, false));
        }
    }

    fn reading_content(&mut self, ui: &mut Ui, t: &Tokens, full: bool) {
        let message = self.selected.as_deref().and_then(|id| self.engine.message(id)).cloned();
        let now_utc = self.now_utc;
        let Some(message) = message else {
            self.close_reading();
            return;
        };
        #[cfg(test)]
        probe::clear_actions(ui);
        let is_draft = self
            .engine
            .folders()
            .iter()
            .find(|folder| folder.id == message.summary.folder_id)
            .is_some_and(|folder| folder.kind == FolderKind::Drafts);

        let (left_action, right_action) = Sides::new().height(36.0).shrink_left().show(
            ui,
            |ui| {
                let mut action = None;
                let compact = ui.available_width() < 560.0;
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

        ui.add_space(space::LG);
        let subject_text = if message.summary.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.summary.subject.as_str() };
        let subject = ui.label(typography::strong(Role::Page, subject_text, t.text));
        reading_context_menu(subject, message.summary.subject.clone(), message.summary.from.display());
        ui.add_space(space::MD);
        let to = message.summary.to.iter().map(|address| address.display()).collect::<Vec<_>>().join(", ");
        Sides::new().height(38.0).show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    avatar(ui, 34.0, &message.summary.from.initials(), t.gold, theme::FOREST);
                    ui.add_space(space::SM);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        let sender = ui.label(typography::strong(Role::BodySmall, message.summary.from.display(), t.text));
                        reading_context_menu(sender, message.summary.subject.clone(), message.summary.from.display());
                        ui.label(typography::text(Role::Meta, format!("a {to}"), t.text_muted));
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
            ui.add_space(space::SM);
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
        ui.add_space(space::MD);
        ui.separator();
        ui.add_space(space::MD);
        egui::ScrollArea::vertical().id_salt(("reading-scroll", message.summary.id.as_str())).auto_shrink([false, false]).show(ui, |ui| {
            for paragraph in message.body_text.split("\n\n") {
                let paragraph = paragraph.trim();
                if paragraph.is_empty() {
                    continue;
                }
                if paragraph.starts_with('>') {
                    Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::MD)).show(
                        ui,
                        |ui| {
                            ui.label(typography::text(Role::Secondary, paragraph, t.text_muted));
                        },
                    );
                } else {
                    ui.label(typography::text(Role::Body, paragraph, t.text));
                }
                ui.add_space(space::LG);
            }
            if !message.attachments.is_empty() {
                ui.add_space(space::SM);
                ui.label(typography::strong(Role::Caption, "ALLEGATI", t.text_faint));
                ui.add_space(space::XS);
                for attachment in &message.attachments {
                    Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::MD)).show(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.label(icons::icon(icons::PAPERCLIP, 16.0, t.text_muted));
                                ui.add_space(space::SM);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(typography::strong(Role::Secondary, attachment.filename.as_str(), t.text));
                                    ui.label(typography::text(
                                        Role::Caption,
                                        format!("{} · {}", attachment.mime, format_size(attachment.size)),
                                        t.text_faint,
                                    ));
                                });
                            });
                        },
                    );
                    ui.add_space(space::SM);
                }
            }
        });

        let action = left_action.or(right_action);
        match action {
            Some(ReadingAction::Archive) => self.move_selected("archive", "Messaggio archiviato", "Messaggi archiviati"),
            Some(ReadingAction::Delete) => self.move_selected("trash", "Messaggio spostato nel cestino", "Messaggi spostati nel cestino"),
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

    fn status_bar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        egui::Panel::bottom("status")
            .exact_size(ui_size::STATUS)
            .frame(Frame::new().fill(t.panel_alt).inner_margin(Margin::symmetric(inset::LG, inset::XS)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    let dot = if self.engine.is_demo() { theme::GOLD } else { t.success };
                    ui.painter().circle_filled(rect.center(), 3.0, dot);
                    ui.label(typography::text(Role::Meta, self.status.as_str(), t.text_muted));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let hint = if self.composer.open {
                            "Ctrl+Invio invia · Esc chiude e conserva"
                        } else {
                            "Ctrl+K comandi · J/K naviga · ? scorciatoie"
                        };
                        ui.label(typography::text(Role::Meta, hint, t.text_faint));
                    });
                });
            });
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
        let height = (ui.ctx().content_rect().height() - 48.0).clamp(ui_size::COMPOSER_MIN_HEIGHT, ui_size::COMPOSER_HEIGHT);
        let response = egui::Window::new("Postino composer")
            .id(egui::Id::new("postino-composer"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([ui_size::COMPOSER_WIDTH, height])
            .frame(
                Frame::new()
                    .fill(t.panel)
                    .stroke(Stroke::new(1.0, t.border))
                    .corner_radius(radius::WINDOW)
                    .inner_margin(Margin::symmetric(inset::XL, inset::LG)),
            )
            .show(ui.ctx(), |ui| Composer::new(&mut self.composer, &account).show(ui));
        let Some(inner) = response else {
            return;
        };
        let Some(action) = inner.inner else {
            return;
        };
        if action.discard {
            self.discard_composer();
        } else if action.close {
            self.close_composer();
        } else if action.send {
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
                    ui.label(RichText::new(format!("{} · M0 + U0.5", env!("CARGO_PKG_VERSION"))).size(11.0).color(t.text_muted));
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
            ui.label(RichText::new("Inter · JetBrains Mono (OFL). Dati demo sintetici.").size(10.5).color(t.text_faint));
            ui.label(RichText::new("MIT OR Apache-2.0").size(10.5).color(t.text_faint));
        });
        self.show_about = open;
    }

    fn command_palette(&mut self, ui: &mut Ui) {
        if !self.show_palette {
            return;
        }
        let t = theme::tokens(self.dark);
        let ctx = ui.ctx().clone();
        let mut open = self.show_palette;
        let mut chosen = None;
        // Consume the navigation keys before the text field sees them. Otherwise the caret
        // swallows the arrows and only the first command can ever run.
        let down = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowDown));
        let up = ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowUp));
        let enter = ctx.input(|input| input.key_pressed(Key::Enter));
        let navigated = down || up;
        egui::Window::new("Comandi")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 110.0])
            .default_width(480.0)
            .show(&ctx, |ui| {
                let response =
                    ui.add_sized([ui.available_width(), 32.0], egui::TextEdit::singleline(&mut self.palette_query).hint_text("Cerca un comando…"));
                if self.palette_focus {
                    response.request_focus();
                    self.palette_focus = false;
                }
                if response.changed() {
                    self.palette_selected = 0;
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
                    egui::ScrollArea::vertical().max_height(330.0).auto_shrink([false, true]).show(ui, |ui| {
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
                    if let Some(command) = filtered.get(self.palette_selected)
                        && !self.command_enabled(command)
                        && let Some(reason) = command.rule.reason()
                    {
                        ui.add_space(space::XS);
                        ui.label(typography::text(Role::Meta, reason, t.text_muted));
                    }
                }
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "↑↓ scegli · Invio esegue · Esc chiude", t.text_faint));
            });
        self.show_palette = open;
        if !open {
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
                ui.label(typography::text(Role::Caption, "Etichette locali, in memoria fino a M1.", t.text_faint));
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
            "L'etichetta “{}” sarà rimossa da {targets}. Le mail non vengono eliminate; le etichette sono metadati locali in memoria fino a M1. Puoi annullare per 8 secondi.",
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
        let response = egui::Modal::new(egui::Id::new("postino-label-delete")).frame(Frame::NONE).show(ui.ctx(), |ui| {
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
            "open" => {
                if let Some(message) = self.selected.clone() {
                    self.open_message(message);
                }
            }
            "expand" => {
                if let Some(message) = self.selected.clone() {
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
            "archive" => self.move_selected("archive", "Messaggio archiviato", "Messaggi archiviati"),
            "delete" => self.move_selected("trash", "Messaggio spostato nel cestino", "Messaggi spostati nel cestino"),
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
            "inbox" => self.select_folder("inbox"),
            "starred" => self.select_folder("starred"),
            "theme" => {
                self.dark = !self.dark;
                theme::apply(ctx, self.dark);
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
            "next" => self.step_selection(1, false),
            "previous" => self.step_selection(-1, false),
            "extend_next" => self.step_selection(1, true),
            "extend_previous" => self.step_selection(-1, true),
            "refresh" => self.status = "Dati demo già aggiornati".to_owned(),
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
        self.capture_screenshot(ui.ctx());
        self.tick(ui.ctx());
        self.resize_handles(ui.ctx());
        self.handle_shortcuts(ui.ctx());
        // egui 0.36 nests panels in the order they are added. The status bar goes first so it is
        // the outermost panel and spans the full width; the title bar and toolbar sit above the
        // three columns.
        self.status_bar(ui);
        if self.custom_chrome {
            self.title_bar(ui);
        }
        self.toolbar(ui);
        self.sidebar(ui);
        // Below 1000 px the side pane cannot keep a usable list, so reading reuses the full path
        // as an overlay. At or above it the pane opens at 52% and resizes between 420 and 62%.
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
        self.composer_window(ui);
        self.toast_overlay(ui.ctx());
    }
}

impl eframe::App for PostinoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
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
enum EmptyAction {
    NewLabel,
    ClearLabel,
    Refresh,
    Compose,
    ClearSearch,
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
    undo: UndoAction,
    until: f64,
}

enum UndoAction {
    Unread(Vec<(String, bool)>),
    Starred(Vec<(String, bool)>),
    Move(Vec<(String, String)>),
    Send(String),
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
}

enum LabelEditorMode {
    Create,
    Rename(String),
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

/// What a command needs to be enabled. The palette, the context menus and the keyboard map read
/// the same rule, so a disabled action always explains itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandRule {
    Always,
    Message,
    ActiveLabel,
    LabelFilter,
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
    Command { id: "send", label: "Invia (locale)", icon: icons::SEND, shortcut: "Ctrl+Invio", rule: CommandRule::Always, in_palette: false },
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
    Command {
        id: "next", label: "Messaggio successivo", icon: icons::CHEVRON_DOWN, shortcut: "J / ↓", rule: CommandRule::Always, in_palette: false
    },
    Command {
        id: "previous",
        label: "Messaggio precedente",
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
    Command { id: "refresh", label: "Aggiorna", icon: icons::SYNC, shortcut: "", rule: CommandRule::Always, in_palette: false },
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

fn reading_context_menu(response: egui::Response, subject: String, sender: String) {
    response.context_menu(|ui| {
        let items = [
            MenuItem::new("Copia oggetto").icon(icons::COPY),
            MenuItem::new("Copia mittente").icon(icons::COPY),
            MenuItem::separator(),
            MenuItem::new("Apri in finestra").icon(icons::EXTERNAL).enabled(false).reason("La finestra separata arriva dopo U0.5"),
            MenuItem::new("Mostra originale").icon(icons::EYE).enabled(false).reason("Il sorgente MIME arriva con lo store M1"),
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

fn reply_recipients(message: &Message, own_email: &str, all: bool) -> (Vec<Address>, Vec<Address>) {
    let mut to = Vec::new();
    if !message.summary.from.email.trim().eq_ignore_ascii_case(own_email) {
        to.push(message.summary.from.clone());
    }
    if to.is_empty() {
        to.extend(message.summary.to.iter().filter(|address| !address.email.trim().eq_ignore_ascii_case(own_email)).cloned());
    }
    let mut cc = if all { message.summary.to.iter().chain(message.cc.iter()).cloned().collect::<Vec<_>>() } else { Vec::new() };
    let to = dedupe_recipients(to, own_email);
    let excluded = to.iter().map(|address| address.email.trim().to_ascii_lowercase()).collect::<BTreeSet<_>>();
    cc.retain(|address| !excluded.contains(&address.email.trim().to_ascii_lowercase()));
    let cc = dedupe_recipients(cc, own_email);
    (to, cc)
}

fn dedupe_recipients(addresses: Vec<Address>, own_email: &str) -> Vec<Address> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for address in addresses {
        let email = address.email.trim().to_ascii_lowercase();
        if email.is_empty() || email == own_email || !seen.insert(email) {
            continue;
        }
        unique.push(address);
    }
    unique
}

fn reply_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject.to_ascii_lowercase().starts_with("re:") { subject.to_owned() } else { format!("Re: {subject}") }
}

fn forward_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject.to_ascii_lowercase().starts_with("fwd:") { subject.to_owned() } else { format!("Fwd: {subject}") }
}

fn reply_block(message: &Message) -> String {
    let date = message.summary.date.with_timezone(&Local).format("%d/%m/%Y %H:%M");
    format!("Il giorno {date}, {} ha scritto:\n{}", message.summary.from.display(), quote_body(&message.body_text))
}

fn forward_block(message: &Message) -> String {
    let to = message.summary.to.iter().map(Address::display).collect::<Vec<_>>().join(", ");
    let date = message.summary.date.with_timezone(&Local).format("%d/%m/%Y %H:%M");
    let mut block = format!(
        "---------- Messaggio inoltrato ----------\nDa: {}\nData: {date}\nA: {to}\nOggetto: {}\n",
        message.summary.from.display(),
        message.summary.subject
    );
    if !message.cc.is_empty() {
        block.push_str(&format!("Cc: {}\n", message.cc.iter().map(Address::display).collect::<Vec<_>>().join(", ")));
    }
    block.push('\n');
    block.push_str(&message.body_text);
    block
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
fn plural(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 { format!("1 {singular}") } else { format!("{count} {plural}") }
}

fn messages_text(count: usize) -> String {
    plural(count, "messaggio", "messaggi")
}

fn unread_text(unread: usize) -> String {
    plural(unread, "non letto", "non letti")
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

/// A thin but always-visible scroll bar for the sidebar, where the overflow must be an affordance.
/// egui's default is a 2 pt sliver with zero dormant opacity: only a hover reveals it, so at
/// 980×600 the clipped list looked broken. The text colour gives 10:1 on ivory and 12:1 on forest.
fn sidebar_scroll_style() -> egui::style::ScrollStyle {
    let mut style = egui::style::ScrollStyle::thin();
    style.foreground_color = true;
    style.floating_width = 4.0;
    style
}

fn reading_pane_width(available: f32) -> f32 {
    let max = (available * READING_PANE_MAX_RATIO).max(READING_PANE_MIN_SIZE);
    (available * READING_PANE_DEFAULT_RATIO).clamp(READING_PANE_MIN_SIZE, max)
}

fn show_list_avatars(list_width: f32) -> bool {
    list_width >= AVATAR_MIN_LIST_WIDTH
}

/// Below the breakpoint the reading pane is a full-width overlay, not a resizable side pane.
fn reading_overlay(available: f32) -> bool {
    available < READING_PANE_MIN_WIDTH
}

fn initials(name: &str) -> String {
    let mut letters = name.split_whitespace().filter_map(|word| word.chars().next()).filter(|c| c.is_alphanumeric()).take(2).collect::<String>();
    if letters.is_empty() {
        letters = name.chars().filter(|c| c.is_alphanumeric()).take(1).collect();
    }
    letters.to_uppercase()
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
        local.format("%d/%m/%Y").to_string()
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
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
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
mod probe {
    //! Deterministic per-frame facts a headless test can assert on: which rows were drawn, where
    //! they were and which actions the reading toolbar offered. Nothing is recorded in a shipped
    //! build; the call sites are `#[cfg(test)]` statements.

    use std::collections::BTreeMap;

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

    pub fn scroll(ui: &Ui, viewport: Rect, content: Vec2) {
        ui.ctx().data_mut(|data| {
            data.insert_temp(id("scroll-view"), viewport);
            data.insert_temp(id("scroll-content"), content);
        });
    }

    pub fn scroll_view(ctx: &Context) -> Option<Rect> {
        ctx.data(|data| data.get_temp(id("scroll-view")))
    }

    pub fn scroll_content(ctx: &Context) -> Option<Vec2> {
        ctx.data(|data| data.get_temp(id("scroll-content")))
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
    use super::*;

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

    /// Small wheel steps are applied without easing, so one frame scrolls deterministically.
    fn wheel(pos: egui::Pos2, steps: usize) -> Vec<egui::Event> {
        wheel_by(pos, steps, -6.0)
    }

    /// Scroll towards the top of the scroll area.
    fn wheel_up(pos: egui::Pos2, steps: usize) -> Vec<egui::Event> {
        wheel_by(pos, steps, 6.0)
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
        let id = app.engine.messages()[0].summary.id.clone();
        app.open_message(id.clone());
        assert_eq!(app.selected.as_deref(), Some(id.as_str()));
        assert!(app.reading_open);
        assert!(!app.reading_full);
    }

    #[test]
    fn focusing_does_not_open_the_reading_pane() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages()[0].summary.id.clone();
        app.focus_message(id.clone());
        assert_eq!(app.selected.as_deref(), Some(id.as_str()));
        assert!(!app.reading_open);
    }

    #[test]
    fn opening_an_unread_message_marks_it_read_after_the_delay_and_undoes() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages().iter().find(|message| message.summary.unread).expect("an unread demo message").summary.id.clone();
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
        let ids = app
            .engine
            .messages()
            .iter()
            .filter(|message| message.summary.unread)
            .take(2)
            .map(|message| message.summary.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        app.set_unread_many(&ids, false);
        assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| !message.summary.unread)));
        app.undo_last();
        assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.unread)), "one undo restores every flag change");
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
        assert!(app.engine.messages().iter().any(|message| message.summary.subject == "Bozza da tastiera"), "Esc must save the draft before closing");
    }

    #[test]
    fn ctrl_enter_sends_from_the_composer() {
        let (ctx, mut app) = ready_app();
        frame(&ctx, &mut app, vec![]);
        app.compose_new();
        app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        app.composer.subject = "Invio da tastiera".to_owned();
        app.composer.body = "Corpo".to_owned();
        app.composer.touch(0.0);
        frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::Enter, Modifiers::COMMAND)]);
        assert!(!app.composer.open, "Ctrl+Enter sends and closes the composer");
        assert!(
            app.engine.messages().iter().any(|message| message.summary.subject == "Invio da tastiera" && message.summary.folder_id == "sent"),
            "the demo send moves the message to Sent"
        );
    }

    #[test]
    fn reply_and_forward_prefill_the_composer() {
        let mut app = PostinoApp::demo(true);
        let with_attachment = app
            .engine
            .messages()
            .iter()
            .find(|message| message.summary.has_attachments && !message.attachments.is_empty())
            .map(|message| message.summary.id.clone())
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
        assert!(!app.engine.messages().iter().any(|message| message.summary.subject == "Non salvabile"));
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
        assert_eq!(app.engine.message(&id).map(|message| message.body_text.as_str()), Some("Seconda versione"));
        let after = app.engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        assert_eq!(after, before + 1, "autosave must not duplicate the draft");
    }

    #[test]
    fn closing_keeps_the_draft_and_sending_moves_it_to_sent() {
        let mut app = PostinoApp::demo(true);
        app.compose_new();
        app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        app.composer.subject = "Invio demo".to_owned();
        app.composer.body = "Corpo".to_owned();
        app.composer.touch(1.0);
        app.close_composer();
        assert!(!app.composer.open);
        let id = app
            .engine
            .messages()
            .iter()
            .find(|message| message.summary.subject == "Invio demo")
            .map(|message| message.summary.id.clone())
            .expect("saved draft");
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.as_str()), Some("drafts"));

        app.edit_draft(&id);
        app.send_composer();
        assert!(!app.composer.open);
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.as_str()), Some("sent"));
        app.undo_last();
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.as_str()), Some("drafts"));
    }

    #[test]
    fn reply_all_excludes_the_account_address() {
        let mut app = PostinoApp::demo(true);
        let sent = app
            .engine
            .messages()
            .iter()
            .find(|message| message.summary.folder_id == "sent" && message.summary.to.len() > 1)
            .map(|message| message.summary.id.clone())
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
        let id = app.engine.messages()[0].summary.id.clone();
        app.open_message(id.clone());
        app.move_selected("archive", "archiviato", "archiviati");
        assert!(app.selected.is_none());
        assert!(!app.reading_open);
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.as_str()), Some("archive"));
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
    fn screenshot_states_parse_and_reject_unknown_labels() {
        for (name, state) in [
            ("shell", ScreenshotState::Shell),
            ("reading", ScreenshotState::Reading),
            ("compose", ScreenshotState::Compose),
            ("palette", ScreenshotState::Palette),
            ("labels", ScreenshotState::Labels),
            ("label_menu", ScreenshotState::LabelMenu),
            ("label_editor", ScreenshotState::LabelEditor),
        ] {
            assert_eq!(ScreenshotState::parse(name), Ok(state), "{name} must be a known scene");
        }
        let error = ScreenshotState::parse("nope").expect_err("nope is not a scene");
        assert!(error.contains("unknown screenshot state"), "{error}");
        assert!(error.contains("shell, reading, compose, palette, labels, label_menu, label_editor"), "{error}");
    }

    #[test]
    fn the_screenshot_clock_is_a_fixed_instant() {
        let frozen = screenshot_clock().expect("the screenshot clock parses");
        assert_eq!(frozen.format("%Y-%m-%dT%H:%M:%SZ").to_string(), SCREENSHOT_CLOCK);
        let app = PostinoApp::demo_at(true, frozen);
        assert_eq!(app.now_utc, frozen);
        assert_eq!(app.engine.messages()[0].summary.date, frozen - chrono::Duration::minutes(8), "demo dates must follow the seam");
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
        assert_eq!(reading_pane_width(1280.0), 1280.0 * 0.52, "the pane opens at 52%");
        assert_eq!(reading_pane_width(800.0), READING_PANE_MIN_SIZE, "it never goes below 420");
        assert!(reading_pane_width(1280.0) <= 1280.0 * READING_PANE_MAX_RATIO);
        assert!(reading_overlay(980.0), "980x600 must use the overlay");
        assert!(!reading_overlay(1000.0), "1000 px switches to the side pane");
        assert!(!show_list_avatars(719.0));
        assert!(show_list_avatars(720.0));
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
        assert!(matches!(app.toast.as_ref().map(|toast| &toast.undo), Some(UndoAction::LabelRename { .. })), "rename offers an undo");
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
        assert!(matches!(app.toast.as_ref().map(|toast| &toast.undo), Some(UndoAction::Labels(_))), "the apply is one undo step");
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
            .find(|message| message.summary.unread && message.summary.labels.iter().any(|id| id == "lbl_1"))
            .map(|message| message.summary.id.clone())
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
            .filter(|message| message.summary.unread && message.summary.labels.iter().any(|id| id == "lbl_1"))
            .map(|message| message.summary.id.clone())
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
    fn narrow_sidebar_keeps_cestino_reachable_with_the_editor_open() {
        let (ctx, mut app) = ready_app();
        let narrow = Vec2::new(980.0, 600.0);
        frame_at(&ctx, &mut app, narrow, vec![]);
        app.start_label_create();
        for _ in 0..20 {
            frame_at(&ctx, &mut app, narrow, vec![]);
        }

        let view = probe::scroll_view(&ctx).expect("the sidebar scroll area");
        let save = probe::saved_at(&ctx).expect("the editor save button");
        assert!(
            save.top() >= view.top() && save.bottom() <= view.bottom(),
            "opening the editor must reveal its controls, not clip them under the footer: {save:?} vs {view:?}"
        );
        let content = probe::scroll_content(&ctx).expect("the scroll content size");
        assert!(content.y > view.height(), "with the editor open the sidebar overflows and shows a scroll bar");
        let style = sidebar_scroll_style();
        assert_eq!(style.dormant_handle_opacity, 1.0, "the sidebar bar must be drawn without hovering");
        assert!(style.foreground_color, "its handle must use the text colour, not the panel fill");
        assert!(style.floating_width >= 4.0, "it must be more than a hairline");

        frame_at(&ctx, &mut app, narrow, wheel_up(view.center(), 40));
        // The scroll offset applies to the next frame's layout, like any egui input.
        frame_at(&ctx, &mut app, narrow, vec![]);
        let rows = probe::rows(&ctx);
        let trash = rows.get("Cestino").copied().expect("Cestino is in the sidebar");
        assert!(
            trash.top() >= view.top() - 1.0 && trash.bottom() <= view.bottom() + 1.0,
            "a wheel scroll reaches Cestino while the editor stays open: {trash:?} vs {view:?}"
        );
        let last = rows.get("Fatture").copied().expect("the last label row");
        assert!(last.bottom() > view.bottom(), "the label rows sit below the fold, so only the scroll bar reaches them");

        frame_at(&ctx, &mut app, narrow, wheel(view.center(), 80));
        frame_at(&ctx, &mut app, narrow, vec![]);
        let scrolled = probe::rows(&ctx);
        let fatture = scrolled.get("Fatture").copied().expect("the last label row is still drawn");
        assert!(fatture.bottom() <= view.bottom() + 1.0, "a wheel scroll brings the last label row into view: {fatture:?} vs {view:?}");
    }

    fn label_unread(app: &PostinoApp, id: &str) -> Option<u32> {
        app.labels_cache.iter().find(|label| label.id == id).map(|label| label.unread_count)
    }

    fn has_label(app: &PostinoApp, message_id: &str, label_id: &str) -> bool {
        app.engine.message(message_id).is_some_and(|message| message.summary.labels.iter().any(|candidate| candidate == label_id))
    }
}
