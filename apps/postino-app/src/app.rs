//! The egui shell. Panels are deliberately plain: folder rail, message table, reading pane.
//! Every action goes through `Engine::call`, the same operation registry the CLI and MCP expose.

use chrono::{DateTime, Datelike, Local, Utc};
use eframe::egui::containers::Sides;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Frame, Key, Layout, Margin, Rect, ResizeDirection, RichText, Sense,
    Stroke, Ui, UiBuilder, Vec2, ViewportCommand, WidgetInfo, WidgetType,
};
use postino_core::{Engine, FolderKind, MessageSummary, MessagesList, SearchResult};
use serde_json::json;

use crate::theme::{self, GOLD, IVORY, Tokens};
use crate::ui::icons;
use crate::ui::tokens::{inset, radius, size as ui_size, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonSize, Chip, EmptyState, IconButton, ListRow, SearchField, SectionHeader};

pub struct PostinoApp {
    engine: Engine,
    folder: String,
    selected: Option<String>,
    query: String,
    unread_only: bool,
    attachments_only: bool,
    dark: bool,
    status: String,
    show_about: bool,
    show_palette: bool,
    palette_query: String,
    palette_focus: bool,
    custom_chrome: bool,
    reading_open: bool,
    reading_full: bool,
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
            query: String::new(),
            unread_only: false,
            attachments_only: false,
            dark: false,
            status: "Demo · dati sintetici, nessun server collegato".to_owned(),
            show_about: false,
            show_palette: false,
            palette_query: String::new(),
            palette_focus: false,
            custom_chrome,
            reading_open: false,
            reading_full: false,
        }
    }

    fn select_folder(&mut self, folder: impl Into<String>) {
        self.folder = folder.into();
        self.query.clear();
        self.selected = None;
        self.reading_open = false;
        self.reading_full = false;
    }

    fn select_message(&mut self, id: String) {
        self.selected = Some(id);
        self.reading_open = true;
    }

    fn move_selected(&mut self, folder: &str, confirmation: &str) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        match self.engine.call("message_move", json!({ "id": id, "folder": folder })) {
            Ok(_) => {
                self.status = confirmation.to_owned();
                self.selected = None;
                self.reading_open = false;
                self.reading_full = false;
            }
            Err(error) => self.status = format!("Errore: {error}"),
        }
    }

    fn step_selection(&mut self, delta: i32) {
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
        if let Some(message) = rows.get(next) {
            self.select_message(message.id.clone());
        }
    }

    fn set_flag(&mut self, unread: Option<bool>, starred: Option<bool>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        match self.engine.call("message_flag", json!({ "id": id, "unread": unread, "starred": starred })) {
            Ok(_) => self.status = "Aggiornato · demo".to_owned(),
            Err(error) => self.status = format!("Errore: {error}"),
        }
    }

    fn toggle_star(&mut self) {
        let Some(message) = self.selected.as_deref().and_then(|id| self.engine.message(id)).cloned() else {
            return;
        };
        self.set_flag(None, Some(!message.summary.starred));
    }

    fn toggle_read(&mut self) {
        let Some(message) = self.selected.as_deref().and_then(|id| self.engine.message(id)).cloned() else {
            return;
        };
        self.set_flag(Some(!message.summary.unread), None);
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
        let mut palette = false;
        let mut escape = false;
        let mut enter = false;
        let mut next = false;
        let mut previous = false;
        let typing = ctx.egui_wants_keyboard_input();
        ctx.input(|input| {
            palette = input.modifiers.command && input.key_pressed(Key::K);
            escape = input.key_pressed(Key::Escape);
            enter = !typing && !input.modifiers.command && input.key_pressed(Key::Enter);
            next = !typing && !input.modifiers.command && input.key_pressed(Key::J);
            previous = !typing && !input.modifiers.command && input.key_pressed(Key::K);
        });
        if palette {
            self.show_palette = !self.show_palette;
            self.palette_focus = self.show_palette;
            if self.show_palette {
                self.palette_query.clear();
            }
        }
        if escape {
            if self.reading_full {
                self.reading_full = false;
            } else if self.reading_open {
                self.reading_open = false;
            }
            self.show_palette = false;
            self.show_about = false;
        }
        if !self.show_palette && !self.show_about {
            if enter && self.reading_open {
                self.reading_full = true;
            }
            if next {
                self.step_selection(1);
            }
            if previous {
                self.step_selection(-1);
            }
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
                        avatar(ui, 24.0, &initials(&account.name), t.gold, theme::FOREST);
                        ui.add_space(space::SM);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(typography::strong(Role::BodySmall, account.name.clone(), t.text));
                            ui.label(typography::text(Role::Caption, account.email.clone(), t.text_faint));
                        });
                    }
                });
                ui.scope_builder(UiBuilder::new().max_rect(search_rect).layout(Layout::left_to_right(Align::Center)), |ui| {
                    SearchField::new(&mut self.query, "Cerca in tutte le mail…").width(search_width).show(ui);
                });
                ui.scope_builder(UiBuilder::new().max_rect(right_rect).layout(Layout::right_to_left(Align::Center)), |ui| {
                    let _ = Button::primary("Scrivi")
                        .icon(icons::COMPOSE)
                        .shortcut("C")
                        .enabled(false)
                        .disabled_reason("Il composer arriva in U0.4")
                        .show(ui);
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
                if let Some(account) = self.engine.accounts().first() {
                    ui.horizontal(|ui| {
                        avatar(ui, 34.0, &initials(&account.name), t.gold, theme::FOREST);
                        ui.add_space(space::SM);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(typography::strong(Role::BodySmall, account.name.clone(), t.text));
                            ui.label(typography::text(Role::Meta, account.email.clone(), t.text_muted));
                            ui.label(typography::text(Role::Caption, account.provider.clone(), t.text_faint));
                        });
                    });
                }
                ui.add_space(space::LG);
                let footer_height = 42.0;
                let scroll_height = (ui.available_height() - footer_height).max(80.0);
                egui::ScrollArea::vertical().max_height(scroll_height).auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    SectionHeader::new("CARTELLE").show(ui);
                    ui.add_space(space::XS);
                    let folders = self.engine.folders().to_vec();
                    for folder in folders {
                        let meta = if folder.unread > 0 { folder.unread.to_string() } else { String::new() };
                        let row = ListRow::folder(folder.name.as_str(), meta.as_str())
                            .icon(folder_icon(folder.kind))
                            .selected(self.folder == folder.id)
                            .height(32.0);
                        if row.show(ui).clicked() {
                            self.select_folder(folder.id.clone());
                        }
                    }
                    ui.add_space(space::LG);
                    SectionHeader::new("ETICHETTE").show(ui);
                    ui.add_space(space::XS);
                    for (label, color) in [("Lavoro", theme::GOLD), ("Personale", t.success), ("Fatture", t.danger)] {
                        label_row(ui, label, color, &t);
                    }
                });
                let remaining = ui.available_height();
                if remaining > footer_height {
                    ui.add_space(remaining - footer_height);
                }
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.label(typography::text(Role::Caption, format!("Postino {} · M0", env!("CARGO_PKG_VERSION")), t.text_faint));
                    ui.label(typography::text(Role::Caption, "Locale · offline", t.text_faint));
                });
            });
    }

    fn list_panel(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let folder_name =
            self.engine.folders().iter().find(|folder| folder.id == self.folder).map_or_else(|| "Posta".to_owned(), |folder| folder.name.clone());
        let (rows, total, unread) = self.visible_messages();
        let searching = !self.query.trim().is_empty();
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL, inset::LG))).show(ui, |ui| {
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
                let (icon, title, body, action) = if searching {
                    (icons::SEARCH, "Nessun risultato", "Nessun messaggio corrisponde alla ricerca.", Some("Cancella ricerca"))
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
                    } else {
                        self.select_folder("inbox");
                    }
                }
                return;
            }

            let list_width = ui.available_width();
            let mut clicked = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let fixed_columns = 12.0 + 30.0 + 120.0 + 52.0 + 20.0;
                // Leave room for the vertical scroll bar so the star column is not hidden.
                let subject_width = (list_width - fixed_columns - 16.0).max(80.0);
                egui_extras::TableBuilder::new(ui)
                    .id_salt("message-list")
                    .striped(false)
                    .resizable(false)
                    .sense(Sense::click())
                    .auto_shrink([false, false])
                    .cell_layout(Layout::left_to_right(Align::Center))
                    .column(egui_extras::Column::exact(12.0))
                    .column(egui_extras::Column::exact(30.0))
                    .column(egui_extras::Column::exact(120.0))
                    .column(egui_extras::Column::exact(subject_width))
                    .column(egui_extras::Column::exact(52.0))
                    .column(egui_extras::Column::exact(20.0))
                    .header(24.0, |mut header| {
                        header.col(|_| {});
                        header.col(|_| {});
                        header.col(|ui| {
                            ui.add(egui::Label::new(typography::text(Role::Caption, "MITTENTE", t.text_faint)).selectable(false));
                        });
                        header.col(|ui| {
                            ui.add(egui::Label::new(typography::text(Role::Caption, "OGGETTO", t.text_faint)).selectable(false));
                        });
                        header.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add(egui::Label::new(typography::text(Role::Caption, "DATA", t.text_faint)).selectable(false));
                            });
                        });
                        header.col(|_| {});
                    })
                    .body(|mut body| {
                        for message in &rows {
                            let selected = self.selected.as_deref() == Some(message.id.as_str());
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
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        let subject = RichText::new(message.subject.as_str())
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
                                    let star_color = if message.starred {
                                        if selected { t.gold } else { theme::GOLD_INK }
                                    } else if selected {
                                        t.selected_muted
                                    } else {
                                        t.text_faint
                                    };
                                    ui.add(egui::Label::new(icons::icon(icons::STAR, 12.0, star_color)).selectable(false));
                                });
                                if row.response().clicked() {
                                    clicked = Some(message.id.clone());
                                }
                            });
                        }
                    });
            });
            if let Some(id) = clicked {
                self.select_message(id);
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
            self.reading_open = false;
            return;
        };

        let (left_action, right_action) = Sides::new().height(36.0).shrink_left().show(
            ui,
            |ui| {
                let mut action = None;
                if full {
                    if Button::ghost("Torna alla posta").icon(icons::ARROW_LEFT).shortcut("Esc").show(ui).clicked() {
                        action = Some(ReadingAction::Back);
                    }
                } else {
                    if Button::new("Archivia").icon(icons::ARCHIVE).shortcut("E").show(ui).clicked() {
                        action = Some(ReadingAction::Archive);
                    }
                    if Button::danger("Elimina").icon(icons::TRASH).show(ui).clicked() {
                        action = Some(ReadingAction::Delete);
                    }
                }
                ui.add_space(space::SM);
                let star_label = if message.summary.starred { "Togli stella" } else { "Aggiungi stella" };
                if IconButton::new(icons::STAR, star_label).selected(message.summary.starred).show(ui).clicked() {
                    action = Some(ReadingAction::Star);
                }
                let read_label = if message.summary.unread { "Segna come letto" } else { "Segna come non letto" };
                let read_icon = if message.summary.unread { icons::MARK_READ } else { icons::MARK_UNREAD };
                if IconButton::new(read_icon, read_label).show(ui).clicked() {
                    action = Some(ReadingAction::Read);
                }
                action
            },
            |ui| {
                let mut action = None;
                if !full {
                    if IconButton::new(icons::SQUARE, "Apri a schermo intero").show(ui).clicked() {
                        action = Some(ReadingAction::Expand);
                    }
                    if IconButton::new(icons::CLOSE, "Chiudi anteprima").show(ui).clicked() {
                        action = Some(ReadingAction::Close);
                    }
                }
                action
            },
        );

        ui.add_space(space::LG);
        ui.label(typography::strong(Role::Page, message.summary.subject.as_str(), t.text));
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
                        ui.label(typography::strong(Role::BodySmall, message.summary.from.display(), t.text));
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
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
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
            Some(ReadingAction::Archive) => self.move_selected("archive", "Messaggio archiviato"),
            Some(ReadingAction::Delete) => self.move_selected("trash", "Messaggio spostato nel cestino"),
            Some(ReadingAction::Star) => self.toggle_star(),
            Some(ReadingAction::Read) => self.toggle_read(),
            Some(ReadingAction::Close) => self.reading_open = false,
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
                        ui.label(typography::text(Role::Meta, "Ctrl+K comandi · J/K naviga", t.text_faint));
                    });
                });
            });
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
                    ui.label(RichText::new(format!("{} · M0 walking skeleton", env!("CARGO_PKG_VERSION"))).size(11.0).color(t.text_muted));
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
        egui::Window::new("Comandi")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 110.0])
            .default_width(440.0)
            .show(&ctx, |ui| {
                let response = ui.add_sized([420.0, 30.0], egui::TextEdit::singleline(&mut self.palette_query).hint_text("Cerca un comando…"));
                if self.palette_focus {
                    response.request_focus();
                    self.palette_focus = false;
                }
                ui.add_space(6.0);
                let query = self.palette_query.trim().to_lowercase();
                let actions = [
                    ("Segna come letto", "read"),
                    ("Segna come non letto", "unread"),
                    ("Aggiungi stella", "star"),
                    ("Togli stella", "unstar"),
                    ("Vai a Posta in arrivo", "inbox"),
                    ("Vai a Preferiti", "starred"),
                    ("Tema chiaro/scuro", "theme"),
                    ("Informazioni su Postino", "about"),
                ];
                let mut first = None;
                for (label, id) in actions {
                    if !query.is_empty() && !label.to_lowercase().contains(&query) {
                        continue;
                    }
                    if first.is_none() {
                        first = Some(id);
                    }
                    if ui.selectable_label(false, RichText::new(label).size(13.0)).clicked() {
                        chosen = Some(id);
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Invio esegue il primo comando · Esc chiude").size(10.0).color(t.text_faint));
                if ui.input(|input| input.key_pressed(Key::Enter))
                    && let Some(id) = first
                {
                    chosen = Some(id);
                }
            });
        self.show_palette = open;
        if let Some(action) = chosen {
            self.execute_command(action, &ctx);
        }
    }

    fn execute_command(&mut self, action: &str, ctx: &egui::Context) {
        match action {
            "read" => self.set_flag(Some(false), None),
            "unread" => self.set_flag(Some(true), None),
            "star" => self.set_flag(None, Some(true)),
            "unstar" => self.set_flag(None, Some(false)),
            "inbox" => self.select_folder("inbox"),
            "starred" => self.select_folder("starred"),
            "theme" => {
                self.dark = !self.dark;
                theme::apply(ctx, self.dark);
            }
            "about" => self.show_about = true,
            _ => {}
        }
        self.show_palette = false;
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
    Star,
    Read,
    Close,
    Expand,
    Back,
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
        app.select_message(id.clone());
        assert_eq!(app.selected.as_deref(), Some(id.as_str()));
        assert!(app.reading_open);
        assert!(!app.reading_full);
    }

    #[test]
    fn archiving_closes_the_pane_and_moves_the_message() {
        let mut app = PostinoApp::demo(true);
        let id = app.engine.messages()[0].summary.id.clone();
        app.select_message(id.clone());
        app.move_selected("archive", "archiviato");
        assert!(app.selected.is_none());
        assert!(!app.reading_open);
        assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.as_str()), Some("archive"));
    }
}
