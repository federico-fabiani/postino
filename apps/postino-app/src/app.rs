//! The egui shell. Panels are deliberately plain: folder rail, message table, reading pane.
//! Every action goes through `Engine::call`, the same operation registry the CLI and MCP expose.

use chrono::{DateTime, Datelike, Local, Utc};
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Key, Layout, Margin, RichText, Sense, Stroke, Ui, Vec2};
use postino_core::{Engine, Folder, MessageSummary, MessagesList, SearchResult};
use serde_json::json;

use crate::theme::{self, GOLD, IVORY, Tokens};

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
}

impl PostinoApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, false);
        Self::demo()
    }

    fn demo() -> Self {
        let engine = Engine::demo();
        let mut app = Self {
            engine,
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
        };
        app.selected = app.first_message_in("inbox");
        app
    }

    fn first_message_in(&self, folder: &str) -> Option<String> {
        self.engine
            .messages()
            .iter()
            .filter(|message| if folder == "starred" { message.summary.starred } else { message.summary.folder_id == folder })
            .max_by(|left, right| left.summary.date.cmp(&right.summary.date))
            .map(|message| message.summary.id.clone())
    }

    fn select_folder(&mut self, folder: impl Into<String>) {
        self.folder = folder.into();
        self.query.clear();
        self.selected = self.first_message_in(&self.folder);
    }

    fn select_message(&mut self, id: String) {
        self.selected = Some(id.clone());
        let unread = self.engine.message(&id).is_some_and(|message| message.summary.unread);
        if unread && let Err(error) = self.engine.call("message_flag", json!({ "id": id, "unread": false })) {
            self.status = format!("Errore: {error}");
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

    fn sync_now(&mut self) {
        self.status = "Demo: la sincronizzazione IMAP arriva in M1".to_owned();
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
        let mut next = false;
        let mut previous = false;
        let typing = ctx.egui_wants_keyboard_input();
        ctx.input(|input| {
            palette = input.modifiers.command && input.key_pressed(Key::K);
            escape = input.key_pressed(Key::Escape);
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
            self.show_palette = false;
            self.show_about = false;
        }
        if !self.show_palette {
            if next {
                self.step_selection(1);
            }
            if previous {
                self.step_selection(-1);
            }
        }
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        egui::Panel::top("top")
            .exact_size(58.0)
            .frame(Frame::new().fill(t.chrome).inner_margin(Margin::symmetric(16, 8)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    logo(ui, 30.0);
                    ui.add_space(8.0);
                    ui.label(RichText::new("postino").font(FontId::new(23.0, theme::display())).color(t.text));
                    ui.add_space(2.0);
                    Frame::new().fill(GOLD).corner_radius(4.0).inner_margin(Margin::symmetric(6, 2)).show(ui, |ui| {
                        ui.label(RichText::new("DEMO").font(FontId::new(9.0, theme::medium())).color(theme::FOREST));
                    });
                    ui.add_space(22.0);
                    ui.add_sized([330.0, 30.0], egui::TextEdit::singleline(&mut self.query).hint_text("Cerca in tutte le mail…"));
                    if !self.query.is_empty() && ui.add(egui::Button::new("×").frame(false)).clicked() {
                        self.query.clear();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        avatar(ui, 30.0, "F", t.gold, theme::FOREST);
                        ui.add_space(8.0);
                        let label = if self.dark { "Chiaro" } else { "Scuro" };
                        let theme_button = ui.add(egui::Button::new(RichText::new(label).size(11.0)).frame(false)).on_hover_text("Cambia tema");
                        if theme_button.clicked() {
                            self.dark = !self.dark;
                            theme::apply(ui.ctx(), self.dark);
                        }
                        ui.add_space(6.0);
                        let write = egui::Button::new(RichText::new("Scrivi").font(FontId::new(14.0, theme::medium())).color(if self.dark {
                            theme::FOREST
                        } else {
                            IVORY
                        }))
                        .fill(if self.dark { GOLD } else { theme::FOREST });
                        if ui.add(write).clicked() {
                            self.status = "Composizione in arrivo (M2)".to_owned();
                        }
                        if ui.add(egui::Button::new("Sincronizza").fill(t.panel_alt)).clicked() {
                            self.sync_now();
                        }
                    });
                });
            });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        egui::Panel::left("folders")
            .resizable(true)
            .default_size(242.0)
            .min_size(204.0)
            .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(12, 14)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    avatar(ui, 34.0, "F", t.gold, theme::FOREST);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(RichText::new("Federico").font(FontId::new(14.0, theme::medium())).color(t.text));
                        ui.label(RichText::new("federico@example.it").size(11.0).color(t.text_muted));
                        ui.label(RichText::new("Aruba · demo").size(10.0).color(t.text_faint));
                    });
                });
                ui.add_space(16.0);
                section_label(ui, "CARTELLE", &t);
                ui.add_space(6.0);
                let folders = self.engine.folders().to_vec();
                for folder in folders {
                    if folder_row(ui, &folder, self.folder == folder.id, &t).clicked() {
                        self.select_folder(folder.id.clone());
                    }
                }
                ui.add_space(18.0);
                section_label(ui, "ETICHETTE", &t);
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for (label, color) in [("Lavoro", GOLD), ("Personale", t.success), ("Fatture", t.danger)] {
                        if tag_chip(ui, label, color, &t).clicked() {
                            self.status = "Etichette in arrivo (M3)".to_owned();
                        }
                    }
                });
                let remaining = ui.available_height();
                if remaining > 46.0 {
                    ui.add_space(remaining - 46.0);
                }
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.label(RichText::new(format!("Postino {} · M0", env!("CARGO_PKG_VERSION"))).size(10.0).color(t.text_faint));
                    ui.label(RichText::new("Ctrl+K · comandi").size(10.0).color(t.text_faint));
                });
            });
    }

    fn list_panel(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let folder_name =
            self.engine.folders().iter().find(|folder| folder.id == self.folder).map_or_else(|| "Posta".to_owned(), |folder| folder.name.clone());
        let (rows, total, unread) = self.visible_messages();
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(18, 14))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(folder_name).font(FontId::new(21.0, theme::display())).color(t.text));
                ui.label(RichText::new(format!("{unread} non letti · {total}")).size(11.5).color(t.text_muted));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if chip(ui, "Allegati", self.attachments_only, &t).clicked() {
                        self.attachments_only = !self.attachments_only;
                    }
                    if chip(ui, "Non letti", self.unread_only, &t).clicked() {
                        self.unread_only = !self.unread_only;
                    }
                    if chip(ui, "Tutti", !self.unread_only && !self.attachments_only, &t).clicked() {
                        self.unread_only = false;
                        self.attachments_only = false;
                    }
                });
            });
            if !self.query.trim().is_empty() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Risultati per {:?}", self.query.trim())).size(11.5).color(t.text_muted));
                    if ui.add(egui::Button::new(RichText::new("Cancella").size(11.0)).frame(false)).clicked() {
                        self.query.clear();
                    }
                });
            }
            ui.add_space(8.0);
            if rows.is_empty() {
                ui.add_space(70.0);
                ui.vertical_centered(|ui| {
                    logo(ui, 44.0);
                    ui.add_space(10.0);
                    ui.label(RichText::new("Nessun messaggio").font(FontId::new(17.0, theme::display())).color(t.text_muted));
                    ui.label(
                        RichText::new(if self.query.trim().is_empty() {
                            "Questa cartella è vuota."
                        } else {
                            "Nessun risultato per questa ricerca."
                        })
                        .size(12.0)
                        .color(t.text_faint),
                    );
                });
                return;
            }

            let mut clicked = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui_extras::TableBuilder::new(ui)
                    .id_salt("message-list")
                    .striped(false)
                    .resizable(false)
                    .sense(Sense::click())
                    .auto_shrink([false, false])
                    .cell_layout(Layout::left_to_right(Align::Center))
                    .column(egui_extras::Column::exact(16.0))
                    .column(egui_extras::Column::exact(36.0))
                    .column(egui_extras::Column::exact(148.0))
                    .column(egui_extras::Column::remainder())
                    .column(egui_extras::Column::exact(58.0))
                    .column(egui_extras::Column::exact(22.0))
                    .header(24.0, |mut header| {
                        header.col(|_| {});
                        header.col(|_| {});
                        header.col(|ui| {
                            ui.label(RichText::new("MITTENTE").size(9.5).color(t.text_faint));
                        });
                        header.col(|ui| {
                            ui.label(RichText::new("OGGETTO").size(9.5).color(t.text_faint));
                        });
                        header.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new("DATA").size(9.5).color(t.text_faint));
                            });
                        });
                        header.col(|_| {});
                    })
                    .body(|mut body| {
                        for message in &rows {
                            let selected = self.selected.as_deref() == Some(message.id.as_str());
                            let fg = if selected { t.selected_text } else { t.text };
                            body.row(58.0, |mut row| {
                                row.set_selected(selected);
                                row.col(|ui| {
                                    if message.unread {
                                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                                        ui.painter().circle_filled(rect.center(), 3.2, GOLD);
                                    }
                                });
                                row.col(|ui| {
                                    avatar(
                                        ui,
                                        28.0,
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
                                        .color(fg);
                                    ui.add(egui::Label::new(text).truncate());
                                });
                                row.col(|ui| {
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        let subject = RichText::new(message.subject.as_str())
                                            .font(FontId::new(13.0, if message.unread { theme::medium() } else { FontFamily::Proportional }))
                                            .color(fg);
                                        ui.add(egui::Label::new(subject).truncate());
                                        let snippet = RichText::new(message.snippet.as_str()).size(11.0).color(if selected {
                                            t.selected_muted
                                        } else {
                                            t.text_muted
                                        });
                                        ui.add(egui::Label::new(snippet).truncate());
                                    });
                                });
                                row.col(|ui| {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(
                                            RichText::new(format_when(message.date))
                                                .font(FontId::new(11.0, FontFamily::Monospace))
                                                .color(if selected { t.selected_muted } else { t.text_faint }),
                                        );
                                    });
                                });
                                row.col(|ui| {
                                    ui.label(RichText::new(if message.starred { "★" } else { "☆" }).size(13.0).color(if message.starred {
                                        GOLD
                                    } else {
                                        t.text_faint
                                    }));
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
        let t = theme::tokens(self.dark);
        egui::Panel::right("reading")
            .resizable(true)
            .default_size(500.0)
            .min_size(360.0)
            .frame(Frame::new().fill(t.panel).inner_margin(Margin::symmetric(18, 14)))
            .show(ui, |ui| {
                let message = self.selected.as_deref().and_then(|id| self.engine.message(id)).cloned();
                let Some(message) = message else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(90.0);
                        logo(ui, 56.0);
                        ui.add_space(14.0);
                        ui.label(RichText::new("Seleziona un messaggio").font(FontId::new(18.0, theme::display())).color(t.text_muted));
                        ui.label(RichText::new("La lettura resta locale e offline.").size(12.0).color(t.text_faint));
                    });
                    return;
                };
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Rispondi").clicked() {
                        self.status = "Risposta in arrivo (M2)".to_owned();
                    }
                    if ui.button("Inoltra").clicked() {
                        self.status = "Inoltro in arrivo (M2)".to_owned();
                    }
                    if ui.button("Archivia").clicked() {
                        self.status = "Archiviazione in arrivo (M3)".to_owned();
                    }
                    if ui.button("Elimina").clicked() {
                        self.status = "Eliminazione in arrivo (M3)".to_owned();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let star_color = if message.summary.starred { GOLD } else { t.text_muted };
                        let star =
                            ui.add(egui::Button::new(RichText::new(if message.summary.starred { "★" } else { "☆" }).color(star_color)).frame(false));
                        if star.on_hover_text("Aggiungi o togli la stella").clicked() {
                            self.toggle_star();
                        }
                        let read = ui.add(egui::Button::new(if message.summary.unread { "●" } else { "○" }).frame(false));
                        if read.on_hover_text("Segna come letto o non letto").clicked() {
                            self.toggle_read();
                        }
                    });
                });
                ui.add_space(10.0);
                ui.label(RichText::new(message.summary.subject.as_str()).font(FontId::new(22.0, theme::display())).color(t.text));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    avatar(ui, 36.0, &message.summary.from.initials(), t.gold, theme::FOREST);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(RichText::new(message.summary.from.display()).font(FontId::new(13.5, theme::medium())).color(t.text));
                        let to = message.summary.to.iter().map(|address| address.display()).collect::<Vec<_>>().join(", ");
                        ui.label(RichText::new(format!("a {to}")).size(11.0).color(t.text_muted));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format_when(message.summary.date)).font(FontId::new(11.0, FontFamily::Monospace)).color(t.text_faint));
                    });
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for paragraph in message.body_text.split("\n\n") {
                        let paragraph = paragraph.trim();
                        if paragraph.is_empty() {
                            continue;
                        }
                        if paragraph.starts_with('>') {
                            Frame::new().fill(t.panel_alt).inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
                                ui.label(RichText::new(paragraph).size(12.5).color(t.text_muted));
                            });
                        } else {
                            ui.label(RichText::new(paragraph).size(14.0).color(t.text));
                        }
                        ui.add_space(10.0);
                    }
                    if !message.attachments.is_empty() {
                        ui.add_space(6.0);
                        section_label(ui, "ALLEGATI", &t);
                        ui.add_space(4.0);
                        for attachment in &message.attachments {
                            Frame::new().fill(t.panel_alt).corner_radius(6.0).inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        ui.label(RichText::new(attachment.filename.as_str()).font(FontId::new(12.5, theme::medium())).color(t.text));
                                        ui.label(
                                            RichText::new(format!("{} · {}", attachment.mime, format_size(attachment.size)))
                                                .size(10.5)
                                                .color(t.text_faint),
                                        );
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.button("Salva").clicked() {
                                            self.status = "Salvataggio allegati in arrivo (M1)".to_owned();
                                        }
                                    });
                                });
                            });
                            ui.add_space(6.0);
                        }
                    }
                    ui.add_space(12.0);
                    ui.label(RichText::new("Modalità demo · nessun server collegato").size(10.5).color(t.text_faint));
                });
            });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        egui::Panel::bottom("status")
            .exact_size(28.0)
            .frame(Frame::new().fill(t.panel_alt).inner_margin(Margin::symmetric(14, 4)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.0, GOLD);
                    ui.label(RichText::new(self.status.as_str()).size(10.5).color(t.text_muted));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("Ctrl+K comandi · J/K naviga").size(10.5).color(t.text_faint));
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
        self.handle_shortcuts(ui.ctx());
        // egui 0.36 nests panels in the order they are added. The status bar goes first so it is
        // the outermost panel and spans the full width; the top bar and the three columns sit
        // above it.
        self.status_bar(ui);
        self.top_bar(ui);
        self.sidebar(ui);
        self.reading_pane(ui);
        self.list_panel(ui);
        self.about(ui);
        self.command_palette(ui);
    }
}

impl eframe::App for PostinoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

fn section_label(ui: &mut Ui, text: &str, t: &Tokens) {
    ui.label(RichText::new(text).font(FontId::new(9.5, theme::medium())).color(t.text_faint));
}

fn chip(ui: &mut Ui, label: &str, active: bool, t: &Tokens) -> egui::Response {
    let fill = if active { t.selected } else { t.panel_alt };
    let color = if active { t.selected_text } else { t.text_muted };
    ui.add(egui::Button::new(RichText::new(label).size(11.0).color(color)).fill(fill))
}

fn folder_row(ui: &mut Ui, folder: &Folder, selected: bool, t: &Tokens) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 29.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(6), t.selected);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), t.hover);
    }
    let fg = if selected { t.selected_text } else { t.text };
    let muted = if selected { t.selected_muted } else { t.text_muted };
    let dot = if folder.unread > 0 { GOLD } else { t.border };
    ui.painter().circle_filled(egui::pos2(rect.left() + 10.0, rect.center().y), 2.6, dot);
    ui.painter().text(
        egui::pos2(rect.left() + 22.0, rect.center().y),
        Align2::LEFT_CENTER,
        &folder.name,
        FontId::new(13.0, FontFamily::Proportional),
        fg,
    );
    if folder.unread > 0 {
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            folder.unread.to_string(),
            FontId::new(11.0, FontFamily::Monospace),
            if selected { GOLD } else { muted },
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn tag_chip(ui: &mut Ui, label: &str, color: Color32, t: &Tokens) -> egui::Response {
    let response = Frame::new().fill(t.panel_alt).corner_radius(10.0).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(6.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.0, color);
            ui.label(RichText::new(label).size(10.5).color(t.text_muted));
        });
    });
    ui.interact(response.response.rect, response.response.id.with("chip"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand)
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
        let mut app = PostinoApp::demo();
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
        let mut app = PostinoApp::demo();
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        let painted = !output.shapes.is_empty();
        output.textures_delta.clear();
        assert!(painted, "the dark shell painted nothing");
    }

    #[test]
    fn demo_starts_on_the_newest_inbox_message() {
        let app = PostinoApp::demo();
        assert_eq!(app.folder, "inbox");
        let selected = app.selected.as_deref().expect("a selected message");
        let message = app.engine.message(selected).expect("the selected message exists");
        assert_eq!(message.summary.folder_id, "inbox");
    }
}
