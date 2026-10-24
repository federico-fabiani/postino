//! The egui shell. Panels are deliberately plain: folder rail, message table, reading pane.
//! Every action goes through `Engine::call`, the same operation registry the CLI and MCP expose.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, Utc};
use eframe::egui::containers::Sides;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Frame, Key, Layout, Margin, Modifiers, Rect, ResizeDirection,
    RichText, Sense, Stroke, Ui, UiBuilder, Vec2, ViewportCommand, WidgetInfo, WidgetType,
};
use postino_core::{Account, Address, DraftSave, DraftSend, Engine, FolderKind, Message, MessageSummary, MessagesList, SearchResult};
use serde_json::json;

use crate::theme::{self, GOLD, IVORY, Tokens};
use crate::ui::icons;
use crate::ui::tokens::{inset, radius, size as ui_size, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{
    AUTOSAVE_DELAY, Button, ButtonSize, Chip, ComposeMode, Composer, ComposerFocus, ComposerState, EmptyState, IconButton, IconButtonSize, ListRow,
    Menu, MenuItem, SearchField, SectionHeader, Toast, compose_body, quote_body, signature_body,
};

pub struct PostinoApp {
    engine: Engine,
    folder: String,
    /// Keyboard focus and the message shown in the reading pane.
    selected: Option<String>,
    /// The rows painted as selected. It always contains `selected` when non-empty.
    selection: BTreeSet<String>,
    /// Shift-click and Shift+arrow range anchor.
    anchor: Option<String>,
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
    custom_chrome: bool,
    reading_open: bool,
    reading_full: bool,
    composer: ComposerState,
}

impl PostinoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, custom_chrome: bool) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, false);
        Self::demo(custom_chrome)
    }

    fn demo(custom_chrome: bool) -> Self {
        Self {
            engine: Engine::demo(),
            folder: "inbox".to_owned(),
            selected: None,
            selection: BTreeSet::new(),
            anchor: None,
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
            custom_chrome,
            reading_open: false,
            reading_full: false,
            composer: ComposerState::closed(),
        }
    }

    fn select_folder(&mut self, folder: impl Into<String>) {
        self.folder = folder.into();
        self.query.clear();
        self.clear_selection();
        self.reading_open = false;
        self.reading_full = false;
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
        }
        match error {
            Some(error) => self.status = format!("Errore: {error}"),
            None => self.status = "Annullato".to_owned(),
        }
    }

    fn visible_messages(&mut self) -> (Vec<MessageSummary>, usize, usize) {
        let searching = !self.query.trim().is_empty();
        let (name, args) = if searching {
            ("search", json!({ "query": self.query, "limit": 200 }))
        } else {
            (
                "messages_list",
                json!({
                    "folder": self.folder,
                    "unread_only": self.unread_only,
                    "with_attachments": self.attachments_only,
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
        let typing = ctx.egui_wants_keyboard_input();
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
            } else if self.reading_full {
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
        if select_all {
            let (rows, _, _) = self.visible_messages();
            self.select_all(&rows);
        }
        if toggle && let Some(id) = self.selected.clone() {
            self.toggle_selection(id);
        }
        if enter {
            if self.reading_open {
                self.reading_full = true;
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
        state.saved_at = Some(message.summary.date.with_timezone(&Local).format("%H:%M").to_string());
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
                    self.composer.mark_saved(Local::now().format("%H:%M").to_string());
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
                        "Messaggio spostato in {} (demo)",
                        self.engine
                            .folders()
                            .iter()
                            .find(|folder| folder.id == sent.message.folder_id)
                            .map_or("Inviati", |folder| folder.name.as_str())
                    );
                    self.push_toast("Messaggio inviato in modalità demo", UndoAction::Send(id));
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
        self.now = ctx.input(|input| input.time);
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
                    let _ = SearchField::new(&mut self.query, "Cerca in tutte le mail…").width(search_width).focus(self.focus_search).show(ui);
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
                let scroll_height = (ui.available_height() - footer_height).max(80.0);
                let mut folder_action = None;
                egui::ScrollArea::vertical().max_height(scroll_height).auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    SectionHeader::new("CARTELLE").show(ui);
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
                            .selected(self.folder == folder.id)
                            .height(32.0);
                        let response = row.show(ui);
                        if response.clicked() {
                            self.select_folder(folder.id.clone());
                        }
                        response.context_menu(|ui| {
                            let items = [
                                MenuItem::new("Nuova etichetta").enabled(false).reason("Le etichette arrivano in U0.5"),
                                MenuItem::new("Rinomina").enabled(false).reason("Le cartelle locali arrivano con lo store M1"),
                                MenuItem::new("Elimina").enabled(false).reason("Le cartelle locali arrivano con lo store M1"),
                                MenuItem::separator(),
                                menu_item("mark_folder_read"),
                            ];
                            if let Some(4) = Menu::new(&items).show(ui) {
                                folder_action = Some((folder.id.clone(), FolderAction::MarkAllRead));
                                ui.close();
                            }
                        });
                    }
                    ui.add_space(space::LG);
                    SectionHeader::new("ETICHETTE").show(ui);
                    ui.add_space(space::XS);
                    for (label, color) in [("Lavoro", theme::GOLD), ("Personale", t.success), ("Fatture", t.danger)] {
                        label_row(ui, label, color, &t);
                    }
                });
                if let Some((folder, FolderAction::MarkAllRead)) = folder_action {
                    self.mark_all_read(&folder);
                }
                let remaining = ui.available_height();
                if remaining > footer_height {
                    ui.add_space(remaining - footer_height);
                }
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.label(typography::text(Role::Caption, format!("Postino {} · M0 + U0.4", env!("CARGO_PKG_VERSION")), t.text_faint));
                    ui.label(typography::text(Role::Caption, "Locale · offline", t.text_faint));
                });
            });
    }

    fn list_panel(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let folder_name =
            self.engine.folders().iter().find(|folder| folder.id == self.folder).map_or_else(|| "Posta".to_owned(), |folder| folder.name.clone());
        let (rows, total, unread) = self.visible_messages();
        self.prune_selection(&rows);
        let searching = !self.query.trim().is_empty();
        let multi = self.selection.len() > 1;
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL, inset::LG))).show(ui, |ui| {
            if multi {
                self.bulk_bar(ui, &t);
            } else {
                ui.horizontal(|ui| {
                    ui.label(typography::strong(Role::Section, folder_name, t.text));
                    ui.add_space(space::SM);
                    ui.label(typography::text(Role::Meta, format!("{unread} non letti · {total}"), t.text_muted));
                });
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
                background.context_menu(|ui| {
                    let items = [
                        MenuItem::new("Nuova etichetta").enabled(false).reason("Le etichette arrivano in U0.5"),
                        MenuItem::separator(),
                        menu_item("refresh"),
                        menu_item("select_all").enabled(false).reason("Non ci sono messaggi da selezionare"),
                        menu_item("compose"),
                        MenuItem::new("Cancella ricerca").icon(icons::CLOSE).enabled(searching).reason("Nessuna ricerca attiva"),
                    ];
                    match Menu::new(&items).show(ui) {
                        Some(2) => {
                            empty_action = Some(EmptyAction::Refresh);
                            ui.close();
                        }
                        Some(4) => {
                            empty_action = Some(EmptyAction::Compose);
                            ui.close();
                        }
                        Some(5) => {
                            empty_action = Some(EmptyAction::ClearSearch);
                            ui.close();
                        }
                        _ => {}
                    }
                });
                let (icon, title, body, action) = if searching {
                    (icons::SEARCH, "Nessun risultato", "Nessun messaggio corrisponde alla ricerca.", Some("Cancella ricerca"))
                } else if self.folder == "drafts" {
                    (icons::DRAFTS, "Nessuna bozza", "Le bozze si salvano da sole mentre scrivi.", Some("Scrivi"))
                } else if self.folder == "inbox" {
                    (icons::INBOX, "Nessun messaggio", "La posta in arrivo è vuota.", None)
                } else {
                    (icons::INBOX, "Nessun messaggio", "Questa cartella è vuota.", Some("Vai a Posta in arrivo"))
                };
                let mut state = EmptyState::new(icon, title, body);
                if let Some(action) = action {
                    state = state.action(action);
                }
                if let Some(response) = state.show(ui)
                    && response.clicked()
                {
                    if searching {
                        self.query.clear();
                    } else if self.folder == "drafts" {
                        self.compose_new();
                    } else {
                        self.select_folder("inbox");
                    }
                }
                match empty_action {
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
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut focus_rect = None;
                egui_extras::TableBuilder::new(ui)
                    .id_salt("message-list")
                    .striped(false)
                    .resizable(false)
                    .sense(Sense::click())
                    .auto_shrink([false, false])
                    .cell_layout(Layout::left_to_right(Align::Center))
                    .column(egui_extras::Column::exact(12.0))
                    .column(egui_extras::Column::exact(30.0))
                    .column(egui_extras::Column::exact(140.0))
                    .column(egui_extras::Column::exact(52.0))
                    .column(egui_extras::Column::remainder())
                    .column(egui_extras::Column::exact(24.0))
                    .header(24.0, |mut header| {
                        header.col(|_| {});
                        header.col(|_| {});
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
                                row.col(|ui| {
                                    avatar(
                                        ui,
                                        24.0,
                                        &message.from.initials(),
                                        if selected { GOLD } else { t.panel_alt },
                                        if selected { theme::FOREST } else { t.text },
                                    );
                                });
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
                                                format_when(message.date),
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
    }

    fn reading_pane(&mut self, ui: &mut Ui) {
        if !self.reading_open {
            return;
        }
        let t = theme::tokens(self.dark);
        if self.reading_full {
            egui::CentralPanel::default()
                .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(inset::XXL, inset::XL)))
                .show(ui, |ui| self.reading_content(ui, &t, true));
        } else {
            egui::Panel::right("reading")
                .resizable(true)
                .default_size(600.0)
                .min_size(420.0)
                .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(inset::LG, inset::XL)))
                .show(ui, |ui| self.reading_content(ui, &t, false));
        }
    }

    fn reading_content(&mut self, ui: &mut Ui, t: &Tokens, full: bool) {
        let message = self.selected.as_deref().and_then(|id| self.engine.message(id)).cloned();
        let Some(message) = message else {
            self.close_reading();
            return;
        };
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
                if full {
                    if Button::ghost(command_label("back", "Torna alla posta"))
                        .icon(command_icon("back", icons::ARROW_LEFT))
                        .shortcut(command_shortcut("back", "Esc"))
                        .show(ui)
                        .clicked()
                    {
                        action = Some(ReadingAction::Back);
                    }
                } else if compact {
                    if IconButton::new(command_icon("archive", icons::ARCHIVE), command_label("archive", "Archivia")).show(ui).clicked() {
                        action = Some(ReadingAction::Archive);
                    }
                    if IconButton::new(command_icon("delete", icons::TRASH), command_label("delete", "Elimina")).danger(true).show(ui).clicked() {
                        action = Some(ReadingAction::Delete);
                    }
                } else {
                    if Button::new(command_label("archive", "Archivia"))
                        .icon(command_icon("archive", icons::ARCHIVE))
                        .shortcut(command_shortcut("archive", "E"))
                        .show(ui)
                        .clicked()
                    {
                        action = Some(ReadingAction::Archive);
                    }
                    if Button::danger(command_label("delete", "Elimina"))
                        .icon(command_icon("delete", icons::TRASH))
                        .shortcut(command_shortcut("delete", "Canc"))
                        .show(ui)
                        .clicked()
                    {
                        action = Some(ReadingAction::Delete);
                    }
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
                ui.label(typography::mono(format_when(message.summary.date), 11.0, t.text_faint));
            },
        );
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
            Some(ReadingAction::Back) => self.reading_full = false,
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
            .show(ui.ctx(), |ui| Composer::new(&mut self.composer, &account, self.engine.is_demo()).show(ui));
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
                    ui.label(RichText::new(format!("{} · M0 + U0.4", env!("CARGO_PKG_VERSION"))).size(11.0).color(t.text_muted));
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
                    let has_message = self.selected.is_some();
                    if enter {
                        let command = filtered[self.palette_selected];
                        if !command.needs_message || has_message {
                            chosen = Some(command.id);
                        }
                    }
                    ui.add_space(space::SM);
                    egui::ScrollArea::vertical().max_height(330.0).auto_shrink([false, true]).show(ui, |ui| {
                        for (index, command) in filtered.iter().enumerate() {
                            let enabled = !command.needs_message || has_message;
                            let selected = index == self.palette_selected;
                            let response = Button::ghost(command.label)
                                .icon(command.icon)
                                .shortcut(command.shortcut)
                                .full_width()
                                .selected(selected)
                                .enabled(enabled)
                                .disabled_reason("Seleziona prima un messaggio")
                                .show(ui);
                            if response.clicked() {
                                chosen = Some(command.id);
                            }
                            if selected && navigated {
                                response.scroll_to_me(None);
                            }
                        }
                    });
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

    fn show_inner(&mut self, ui: &mut egui::Ui) {
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
        if self.reading_full && self.reading_open {
            self.reading_pane(ui);
        } else {
            self.reading_pane(ui);
            self.list_panel(ui);
        }
        self.about(ui);
        self.command_palette(ui);
        self.shortcuts_sheet(ui);
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
}

#[derive(Clone, Copy)]
struct Command {
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    shortcut: &'static str,
    needs_message: bool,
    in_palette: bool,
}

const COMMANDS: &[Command] = &[
    Command { id: "compose", label: "Scrivi", icon: icons::COMPOSE, shortcut: "C", needs_message: false, in_palette: true },
    Command { id: "open", label: "Apri messaggio", icon: icons::MAIL, shortcut: "Invio", needs_message: true, in_palette: true },
    Command { id: "reply", label: "Rispondi", icon: icons::REPLY, shortcut: "R", needs_message: true, in_palette: true },
    Command { id: "reply_all", label: "Rispondi a tutti", icon: icons::REPLY_ALL, shortcut: "A", needs_message: true, in_palette: true },
    Command { id: "forward", label: "Inoltra", icon: icons::FORWARD, shortcut: "F", needs_message: true, in_palette: true },
    Command { id: "edit_draft", label: "Modifica bozza", icon: icons::COMPOSE, shortcut: "", needs_message: true, in_palette: false },
    Command { id: "send", label: "Invia (demo)", icon: icons::SEND, shortcut: "Ctrl+Invio", needs_message: false, in_palette: false },
    Command { id: "expand", label: "Apri a schermo intero", icon: icons::SQUARE, shortcut: "", needs_message: true, in_palette: true },
    Command { id: "read", label: "Segna come letto", icon: icons::MARK_READ, shortcut: "", needs_message: true, in_palette: true },
    Command { id: "unread", label: "Segna come non letto", icon: icons::MARK_UNREAD, shortcut: "U", needs_message: true, in_palette: true },
    Command { id: "star", label: "Aggiungi stella", icon: icons::STAR, shortcut: "S", needs_message: true, in_palette: true },
    Command { id: "unstar", label: "Togli stella", icon: icons::STAR, shortcut: "S", needs_message: true, in_palette: true },
    Command { id: "archive", label: "Archivia", icon: icons::ARCHIVE, shortcut: "E", needs_message: true, in_palette: true },
    Command { id: "delete", label: "Elimina", icon: icons::TRASH, shortcut: "Canc", needs_message: true, in_palette: true },
    Command { id: "inbox", label: "Vai a Posta in arrivo", icon: icons::INBOX, shortcut: "", needs_message: false, in_palette: true },
    Command { id: "starred", label: "Vai a Preferiti", icon: icons::STAR, shortcut: "", needs_message: false, in_palette: true },
    Command { id: "theme", label: "Cambia tema", icon: icons::MOON, shortcut: "", needs_message: false, in_palette: true },
    Command { id: "about", label: "Informazioni su Postino", icon: icons::INFO, shortcut: "", needs_message: false, in_palette: true },
    Command { id: "shortcuts", label: "Scorciatoie da tastiera", icon: icons::KEYBOARD, shortcut: "?", needs_message: false, in_palette: true },
    Command { id: "select_all", label: "Seleziona tutto", icon: icons::LIST, shortcut: "Ctrl+A", needs_message: false, in_palette: true },
    Command { id: "toggle", label: "Seleziona o deseleziona", icon: icons::CHECK, shortcut: "X / Spazio", needs_message: true, in_palette: true },
    Command { id: "clear_selection", label: "Deseleziona", icon: icons::X_CIRCLE, shortcut: "Esc", needs_message: false, in_palette: true },
    Command { id: "focus_search", label: "Cerca nelle mail", icon: icons::SEARCH, shortcut: "/", needs_message: false, in_palette: true },
    Command { id: "next", label: "Messaggio successivo", icon: icons::CHEVRON_DOWN, shortcut: "J / ↓", needs_message: false, in_palette: false },
    Command { id: "previous", label: "Messaggio precedente", icon: icons::CHEVRON_UP, shortcut: "K / ↑", needs_message: false, in_palette: false },
    Command {
        id: "extend_next",
        label: "Estendi selezione in basso",
        icon: icons::CHEVRON_DOWN,
        shortcut: "Shift+J / Shift+↓",
        needs_message: false,
        in_palette: false,
    },
    Command {
        id: "extend_previous",
        label: "Estendi selezione in alto",
        icon: icons::CHEVRON_UP,
        shortcut: "Shift+K / Shift+↑",
        needs_message: false,
        in_palette: false,
    },
    Command { id: "close", label: "Chiudi lettura", icon: icons::CLOSE, shortcut: "Esc", needs_message: false, in_palette: false },
    Command { id: "undo", label: "Annulla ultima azione", icon: icons::UNDO, shortcut: "Z", needs_message: false, in_palette: false },
    Command { id: "palette", label: "Apri i comandi", icon: icons::COMMAND, shortcut: "Ctrl+K", needs_message: false, in_palette: false },
    Command { id: "refresh", label: "Aggiorna", icon: icons::SYNC, shortcut: "", needs_message: false, in_palette: false },
    Command {
        id: "mark_folder_read",
        label: "Segna tutto come letto",
        icon: icons::MARK_READ,
        shortcut: "",
        needs_message: false,
        in_palette: false,
    },
    Command { id: "back", label: "Torna alla posta", icon: icons::ARROW_LEFT, shortcut: "Esc", needs_message: false, in_palette: false },
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
            MenuItem::new("Apri in finestra").icon(icons::EXTERNAL).enabled(false).reason("La finestra separata arriva in U0.5"),
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
    items.push(MenuItem::new("Etichetta").icon(icons::TAG).enabled(false).reason("Le etichette arrivano in U0.5"));
    actions.push(None);
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

fn label_row(ui: &mut Ui, label: &str, color: Color32, t: &Tokens) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.0, color);
        ui.label(typography::text(Role::BodySmall, label, t.text_muted));
    });
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

fn format_when(date: DateTime<Utc>) -> String {
    let local = date.with_timezone(&Local);
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else if local.year() == now.year() {
        local.format("%d/%m").to_string()
    } else {
        local.format("%d/%m/%Y").to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, app: &mut PostinoApp, events: Vec<egui::Event>) {
        let raw =
            egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))), events, ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        output.textures_delta.clear();
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
}
