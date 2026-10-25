//! The composer surface: recipients, subject, body, formatting, attachments and draft state.
//! The component owns no mail state. The screen saves and sends through the operation registry.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::containers::Sides;
use eframe::egui::text::{CCursor, CCursorRange};
use eframe::egui::{self, Align, FontFamily, FontId, Frame, Layout, Margin, Rect, ScrollArea, Stroke, TextEdit, Ui, UiBuilder, Vec2, pos2};
use postino_core::{Account, Address, Attachment};

use crate::theme::Tokens;
use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, size as ui_size, space, type_size};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonSize, IconButton, IconButtonSize, SectionHeader, TextField};

/// Wait this long after the last keystroke before autosaving. Short enough to feel automatic,
/// long enough that typing a sentence does not write on every key.
pub const AUTOSAVE_DELAY: f64 = 1.2;

/// Shown next to the Send button before the click, not only in the after-the-fact toast: this
/// build opens no SMTP connection and the message stays in Sent on this computer.
pub const SEND_NOTICE: &str = "Nessun invio SMTP: il messaggio resta in Inviati su questo computer.";

/// Each composer gets a fresh text-edit id so undo history and cursor state cannot leak from a
/// previous message.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeMode {
    New,
    Draft,
    Reply,
    ReplyAll,
    Forward,
}

impl ComposeMode {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::New => "Nuovo messaggio",
            Self::Draft => "Modifica bozza",
            Self::Reply => "Rispondi",
            Self::ReplyAll => "Rispondi a tutti",
            Self::Forward => "Inoltra",
        }
    }

    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::New | Self::Draft => icons::COMPOSE,
            Self::Reply => icons::REPLY,
            Self::ReplyAll => icons::REPLY_ALL,
            Self::Forward => icons::FORWARD,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposerFocus {
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

pub type ComposerRecipients = (Vec<Address>, Vec<Address>, Vec<Address>);

#[derive(Debug)]
pub struct ComposerState {
    pub open: bool,
    pub mode: ComposeMode,
    pub draft_id: Option<String>,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub show_cc: bool,
    pub show_bcc: bool,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<Attachment>,
    pub focus: Option<ComposerFocus>,
    pub dirty: bool,
    pub last_edit: f64,
    pub saved_at: Option<String>,
    pub save_error: Option<String>,
    pub notice: Option<String>,
    session: u64,
    body_selection: Option<(usize, usize)>,
    pending_selection: Option<(usize, usize)>,
}

impl ComposerState {
    #[must_use]
    pub fn new(mode: ComposeMode) -> Self {
        Self {
            open: true,
            mode,
            draft_id: None,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            show_cc: false,
            show_bcc: false,
            subject: String::new(),
            body: String::new(),
            attachments: Vec::new(),
            focus: Some(ComposerFocus::To),
            dirty: false,
            last_edit: 0.0,
            saved_at: None,
            save_error: None,
            notice: None,
            session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            body_selection: None,
            pending_selection: None,
        }
    }

    pub fn touch(&mut self, now: f64) {
        self.dirty = true;
        self.last_edit = now;
        self.save_error = None;
        self.notice = None;
    }

    pub fn mark_saved(&mut self, at: String) {
        self.dirty = false;
        self.saved_at = Some(at);
        self.save_error = None;
        self.notice = None;
    }

    pub fn mark_failed(&mut self, error: impl Into<String>, now: f64) {
        self.save_error = Some(error.into());
        self.dirty = true;
        self.last_edit = now;
        self.notice = None;
    }

    /// Parse the three recipient fields before a save or a send. The UI keeps the raw text so a
    /// half-typed address is never silently rewritten.
    pub fn recipients(&self) -> Result<ComposerRecipients, String> {
        Ok((Address::parse_list(&self.to)?, Address::parse_list(&self.cc)?, Address::parse_list(&self.bcc)?))
    }

    /// The field a freshly opened composer should focus: the first empty one, in reading order.
    /// A reply or a reopened draft usually has every field filled, so the body is the fallback.
    #[must_use]
    pub fn first_empty_focus(&self) -> ComposerFocus {
        if self.to.trim().is_empty() {
            ComposerFocus::To
        } else if self.show_cc && self.cc.trim().is_empty() {
            ComposerFocus::Cc
        } else if self.show_bcc && self.bcc.trim().is_empty() {
            ComposerFocus::Bcc
        } else if self.subject.trim().is_empty() {
            ComposerFocus::Subject
        } else {
            ComposerFocus::Body
        }
    }

    fn apply_format(&mut self, action: FormatAction, now: f64) {
        let chars = self.body.chars().collect::<Vec<_>>();
        let (start, end) = self.body_selection.unwrap_or((chars.len(), chars.len()));
        let start = start.min(chars.len());
        let end = end.min(chars.len());
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        let start_byte = byte_at_char(&self.body, start);
        let end_byte = byte_at_char(&self.body, end);
        let selected = self.body.get(start_byte..end_byte).unwrap_or("");

        let (replacement, selection) = match action {
            FormatAction::Bold => {
                let (replacement, (selection_start, selection_end)) = wrap(selected, "**", "grassetto");
                (replacement, (start + selection_start, start + selection_end))
            }
            FormatAction::Italic => {
                let (replacement, (selection_start, selection_end)) = wrap(selected, "*", "testo");
                (replacement, (start + selection_start, start + selection_end))
            }
            FormatAction::Link => {
                if selected.is_empty() {
                    ("[testo](https://)".to_owned(), (start + 1, start + 6))
                } else {
                    let replacement = format!("[{selected}](https://)");
                    let selection_end = start + replacement.chars().count();
                    (replacement, (start, selection_end))
                }
            }
            FormatAction::Bullet => prefix_lines(&self.body, start_byte, end_byte, "• ", start_byte == end_byte),
            FormatAction::Quote => prefix_lines(&self.body, start_byte, end_byte, "> ", start_byte == end_byte),
        };

        let before = self.body.get(..start_byte).unwrap_or("");
        let after = self.body.get(end_byte..).unwrap_or("");
        self.body = format!("{before}{replacement}{after}");
        self.pending_selection = Some(selection);
        self.focus = Some(ComposerFocus::Body);
        self.touch(now);
    }
}

impl ComposerState {
    #[must_use]
    pub fn closed() -> Self {
        let mut state = Self::new(ComposeMode::New);
        state.open = false;
        state
    }
}

impl Default for ComposerState {
    fn default() -> Self {
        Self::closed()
    }
}

#[derive(Clone, Copy)]
enum FormatAction {
    Bold,
    Italic,
    Bullet,
    Quote,
    Link,
}

#[derive(Default)]
pub struct ComposerResponse {
    pub close: bool,
    pub save: bool,
    pub send: bool,
    pub discard: bool,
}

pub struct Composer<'a> {
    state: &'a mut ComposerState,
    account: &'a Account,
}

impl<'a> Composer<'a> {
    #[must_use]
    pub fn new(state: &'a mut ComposerState, account: &'a Account) -> Self {
        Self { state, account }
    }

    pub fn show(self, ui: &mut Ui) -> ComposerResponse {
        let session = self.state.session;
        ui.push_id(("composer-session", session), |ui| self.show_session(ui)).inner
    }

    fn show_session(mut self, ui: &mut Ui) -> ComposerResponse {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let now = ui.input(|input| input.time);
        let session = self.state.session;
        let mut response = ComposerResponse::default();

        // The caller sizes the surface: a centred card at >= 1000×700, a sheet with LG margins
        // below it. The header and the footer are pinned here; only the body scrolls.
        let area = ui.max_rect();
        let header_rect = Rect::from_min_size(area.min, Vec2::new(area.width(), ui_size::COMPOSER_HEADER_BLOCK));
        let footer_rect = Rect::from_min_max(pos2(area.min.x, area.max.y - ui_size::COMPOSER_FOOTER_BLOCK), area.max);
        let body_rect = Rect::from_min_max(pos2(area.min.x, header_rect.max.y + space::XS), pos2(area.max.x, footer_rect.min.y - space::XS));

        self.header(ui, &t, header_rect, &mut response);

        let parsed = self.state.recipients();
        let addresses_ok = parsed.is_ok();
        let send_ready = parsed.as_ref().is_ok_and(|(to, cc, bcc)| !to.is_empty() || !cc.is_empty() || !bcc.is_empty());
        let recipient_error = parsed.err();
        let mut changed = false;
        ui.scope_builder(UiBuilder::new().max_rect(body_rect), |ui| {
            ui.spacing_mut().scroll = tokens::scroll_style();
            let scroll = egui::ScrollArea::vertical()
                .id_salt(("composer-body-scroll", session))
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                .show(ui, |ui| {
                    changed |= self.recipient_field(ui, &t, "A", ComposerFocus::To, icons::USER);
                    if self.state.show_cc {
                        changed |= self.recipient_field(ui, &t, "Cc", ComposerFocus::Cc, icons::USER);
                    }
                    if self.state.show_bcc {
                        changed |= self.recipient_field(ui, &t, "Bcc", ComposerFocus::Bcc, icons::USER);
                    }
                    if let Some(error) = &recipient_error {
                        ui.label(typography::text(Role::Caption, error.as_str(), t.danger));
                    }
                    ui.add_space(space::XS);

                    let subject = TextField::new(&mut self.state.subject, "Oggetto")
                        .id_salt(("composer-subject", self.state.session))
                        .icon(icons::MAIL)
                        .focus(self.state.focus == Some(ComposerFocus::Subject))
                        .show(ui);
                    changed |= subject.changed();
                    ui.add_space(space::SM);

                    self.formatting_bar(ui, &t, now);
                    changed |= self.body_field(ui, &t);
                    changed |= self.attachment_section(ui, &t, now);
                    // A little tail so the last attachment breathes instead of touching the footer.
                    ui.add_space(space::SM);
                });
            #[cfg(test)]
            crate::app::probe::composer_scroll(ui, scroll.state.offset.y, scroll.inner_rect, scroll.content_size);
            #[cfg(not(test))]
            let _ = scroll;
        });

        self.footer(ui, &t, footer_rect, addresses_ok, send_ready, &mut response);

        if changed {
            self.state.touch(now);
        }
        self.state.focus = None;
        response
    }

    fn header(&mut self, ui: &mut Ui, t: &Tokens, rect: Rect, response: &mut ComposerResponse) {
        let title_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), ui_size::COMPOSER_HEADER));
        ui.scope_builder(UiBuilder::new().max_rect(title_rect), |ui| {
            Sides::new().height(ui_size::COMPOSER_HEADER).show(
                ui,
                |ui| {
                    ui.horizontal(|ui| {
                        ui.label(icons::icon(self.state.mode.icon(), ui_size::ICON_LG, t.gold_ink));
                        ui.add_space(space::SM);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(typography::strong(Role::Emphasis, self.state.mode.title(), t.text));
                            let (subtitle, color) = if let Some(error) = &self.state.save_error {
                                (format!("Non salvata · {error}"), t.danger)
                            } else if let Some(notice) = &self.state.notice {
                                (notice.clone(), t.danger)
                            } else if self.state.dirty {
                                ("Modifiche non salvate".to_owned(), t.text_faint)
                            } else if let Some(at) = &self.state.saved_at {
                                (format!("Bozza salvata alle {at}"), t.text_faint)
                            } else {
                                ("Bozza · salvataggio automatico".to_owned(), t.text_faint)
                            };
                            ui.add(egui::Label::new(typography::text(Role::Caption, subtitle, color)).truncate());
                        });
                    });
                },
                |ui| {
                    if IconButton::new(icons::CLOSE, "Chiudi e conserva la bozza").show(ui).clicked() {
                        response.close = true;
                    }
                },
            );
        });

        let rule_y = title_rect.bottom() + space::XS;
        ui.painter().hline(rect.x_range(), rule_y, Stroke::new(1.0, t.border));
        let account_rect = Rect::from_min_size(pos2(rect.min.x, rule_y + 1.0 + space::SM), Vec2::new(rect.width(), ui_size::COMPOSER_ACCOUNT_ROW));
        ui.scope_builder(UiBuilder::new().max_rect(account_rect), |ui| {
            ui.horizontal(|ui| {
                ui.add_sized([24.0, ui_size::COMPOSER_ACCOUNT_ROW], egui::Label::new(typography::text(Role::Meta, "Da", t.text_faint)));
                ui.add(
                    egui::Label::new(typography::text(Role::Meta, format!("{} <{}>", self.account.name, self.account.email), t.text_muted))
                        .truncate(),
                );
            });
        });
    }

    fn recipient_field(&mut self, ui: &mut Ui, t: &Tokens, label: &str, field: ComposerFocus, icon: &'static str) -> bool {
        let (text, hint) = match field {
            ComposerFocus::To => (&mut self.state.to, "nome@esempio.it, altro@esempio.it"),
            ComposerFocus::Cc => (&mut self.state.cc, "Cc"),
            ComposerFocus::Bcc => (&mut self.state.bcc, "Bcc"),
            ComposerFocus::Subject | ComposerFocus::Body => return false,
        };
        let salt = match field {
            ComposerFocus::To => ("composer-to", self.state.session),
            ComposerFocus::Cc => ("composer-cc", self.state.session),
            ComposerFocus::Bcc => ("composer-bcc", self.state.session),
            ComposerFocus::Subject | ComposerFocus::Body => ("composer-field", self.state.session),
        };
        ui.horizontal(|ui| {
            ui.add_sized([24.0, ui_size::CONTROL], egui::Label::new(typography::text(Role::Meta, label, t.text_faint)));
            let hide = matches!(field, ComposerFocus::Cc | ComposerFocus::Bcc);
            let reserved = 76.0;
            let field_response = TextField::new(text, hint)
                .id_salt(salt)
                .icon(icon)
                .width((ui.available_width() - reserved).max(120.0))
                .focus(self.state.focus == Some(field))
                .show(ui);
            if hide {
                let empty = text.trim().is_empty();
                if IconButton::new(icons::CLOSE, if empty { "Nascondi campo" } else { "Svuota il campo per nasconderlo" })
                    .size(IconButtonSize::Sm)
                    .enabled(empty)
                    .show(ui)
                    .clicked()
                {
                    match field {
                        ComposerFocus::Cc => self.state.show_cc = false,
                        ComposerFocus::Bcc => self.state.show_bcc = false,
                        _ => {}
                    }
                }
            }
            if field == ComposerFocus::To {
                if !self.state.show_cc && Button::ghost("Cc").size(ButtonSize::Sm).show(ui).clicked() {
                    self.state.show_cc = true;
                    self.state.focus = Some(ComposerFocus::Cc);
                }
                if !self.state.show_bcc && Button::ghost("Bcc").size(ButtonSize::Sm).show(ui).clicked() {
                    self.state.show_bcc = true;
                    self.state.focus = Some(ComposerFocus::Bcc);
                }
            }
            field_response.changed()
        })
        .inner
    }

    fn formatting_bar(&mut self, ui: &mut Ui, t: &Tokens, now: f64) {
        let mut action = None;
        Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::XS, inset::XS)).show(ui, |ui| {
            ui.horizontal(|ui| {
                if IconButton::new(icons::TEXT_BOLD, "Grassetto (Markdown)").size(IconButtonSize::Sm).show(ui).clicked() {
                    action = Some(FormatAction::Bold);
                }
                if IconButton::new(icons::TEXT_ITALIC, "Corsivo (Markdown)").size(IconButtonSize::Sm).show(ui).clicked() {
                    action = Some(FormatAction::Italic);
                }
                ui.add_space(space::XS);
                if IconButton::new(icons::LIST_BULLETS, "Elenco puntato").size(IconButtonSize::Sm).show(ui).clicked() {
                    action = Some(FormatAction::Bullet);
                }
                if IconButton::new(icons::QUOTES, "Citazione").size(IconButtonSize::Sm).show(ui).clicked() {
                    action = Some(FormatAction::Quote);
                }
                if IconButton::new(icons::LINK, "Inserisci link").size(IconButtonSize::Sm).show(ui).clicked() {
                    action = Some(FormatAction::Link);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(typography::text(Role::Caption, "Markdown · HTML in M5", t.text_faint));
                });
            });
        });
        if let Some(action) = action {
            self.state.apply_format(action, now);
        }
    }

    fn body_field(&mut self, ui: &mut Ui, t: &Tokens) -> bool {
        let frame = Frame::new()
            .fill(t.field)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::SM))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The body grows with the text and the modal body scrolls; a second scroll area
                // inside the editor would trap the wheel at 980×600.
                let body_salt = ("composer-body", self.state.session);
                let body_id = ui.make_persistent_id(body_salt);
                let edit = TextEdit::multiline(&mut self.state.body)
                    .id_salt(body_salt)
                    .hint_text("Scrivi il messaggio…")
                    .font(FontId::new(type_size::BODY_SMALL, FontFamily::Proportional))
                    .text_color(t.text)
                    .frame(Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .min_size(Vec2::new(0.0, ui_size::COMPOSER_BODY_MIN))
                    .margin(Margin::ZERO);
                let mut output = edit.show(ui);
                self.state.body_selection = output.cursor_range.map(|range| {
                    let range = range.as_sorted_char_range();
                    (range.start.0, range.end.0)
                });
                if let Some((start, end)) = self.state.pending_selection.take() {
                    let range = CCursorRange::two(CCursor::new(start), CCursor::new(end));
                    output.state.cursor.set_char_range(Some(range));
                    output.state.store(ui.ctx(), body_id);
                    output.response.request_focus();
                    self.state.body_selection = Some((start, end));
                }
                if self.state.focus == Some(ComposerFocus::Body) {
                    output.response.request_focus();
                }
                (output.response.changed(), output.response.has_focus())
            });
        let (changed, has_focus) = frame.inner;
        if has_focus {
            tokens::paint_focus_ring(ui, frame.response.rect, radius::CONTROL, t);
        }
        changed
    }

    fn attachment_section(&mut self, ui: &mut Ui, t: &Tokens, now: f64) -> bool {
        let mut changed = false;
        if !self.state.attachments.is_empty() {
            ui.add_space(space::XS);
            SectionHeader::new("ALLEGATI").count(self.state.attachments.len()).show(ui);
            ScrollArea::vertical().max_height(ui_size::ATTACHMENT_LIST_MAX).auto_shrink([false, true]).show(ui, |ui| {
                let mut remove = None;
                for (index, attachment) in self.state.attachments.iter().enumerate() {
                    Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::SM, inset::XS)).show(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.label(icons::icon(icons::PAPERCLIP, ui_size::ICON_SM, t.text_muted));
                                ui.add(egui::Label::new(typography::text(Role::Caption, attachment.filename.as_str(), t.text)).truncate());
                                ui.label(typography::mono(format_size(attachment.size), 9.5, t.text_faint));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if IconButton::new(icons::CLOSE, "Rimuovi allegato").size(IconButtonSize::Sm).show(ui).clicked() {
                                        remove = Some(index);
                                    }
                                });
                            });
                        },
                    );
                }
                if let Some(index) = remove
                    && index < self.state.attachments.len()
                {
                    self.state.attachments.remove(index);
                    changed = true;
                }
            });
        }

        if self.state.attachments.is_empty() {
            Frame::new()
                .fill(t.panel_alt)
                .stroke(Stroke::new(1.0, t.border))
                .corner_radius(radius::CONTROL)
                .inner_margin(Margin::symmetric(inset::LG, inset::SM))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(icons::icon(icons::PAPERCLIP, ui_size::ICON_SM, t.text_faint));
                        ui.label(typography::text(Role::Caption, "Trascina un file qui per allegarlo · solo metadati in M0", t.text_faint));
                    });
                });
        }

        let dropped = ui.ctx().input(|input| input.raw.dropped_files.clone());
        if !dropped.is_empty() && ui.rect_contains_pointer(ui.max_rect()) {
            let mut added = false;
            for file in dropped {
                let path = file.path().to_path_buf();
                if let Some(attachment) = attachment_from_path(&path, self.state.attachments.len()) {
                    self.state.attachments.push(attachment);
                    added = true;
                } else {
                    self.state.notice = Some(format!("Impossibile leggere il file {}", path.display()));
                }
            }
            if added {
                self.state.touch(now);
                changed = true;
            }
        }
        changed
    }

    fn footer(&mut self, ui: &mut Ui, t: &Tokens, rect: Rect, addresses_ok: bool, send_ready: bool, response: &mut ComposerResponse) {
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, t.border));
        let row_rect = Rect::from_min_size(pos2(rect.min.x, rect.top() + space::MD), Vec2::new(rect.width(), ui_size::CONTROL_LG));
        ui.scope_builder(UiBuilder::new().max_rect(row_rect), |ui| {
            Sides::new().height(ui_size::CONTROL_LG).show(
                ui,
                |ui| {
                    let (text, color) = if let Some(error) = &self.state.save_error {
                        (format!("Bozza non salvata · {error}"), t.danger)
                    } else if self.state.dirty {
                        ("Salvataggio automatico in corso…".to_owned(), t.text_faint)
                    } else if let Some(at) = &self.state.saved_at {
                        (format!("Bozza salvata alle {at}"), t.success)
                    } else {
                        ("La bozza si salva da sola".to_owned(), t.text_faint)
                    };
                    ui.add(egui::Label::new(typography::text(Role::Caption, text, color)).truncate());
                },
                |ui| {
                    let send_reason = if !addresses_ok {
                        "Controlla gli indirizzi: c'è un destinatario non valido"
                    } else if !send_ready {
                        "Aggiungi almeno un destinatario"
                    } else {
                        ""
                    };
                    let send_label = "Invia (locale)";
                    let send = Button::primary(send_label)
                        .icon(icons::SEND)
                        .shortcut("Ctrl+Invio")
                        .enabled(send_ready)
                        .disabled_reason(send_reason)
                        .show(ui);
                    #[cfg(test)]
                    crate::app::probe::composer_send(ui, send.rect);
                    if send.clicked() {
                        response.send = true;
                    }
                    ui.add_space(space::XS);
                    let notice = IconButton::new(icons::INFO, SEND_NOTICE).size(IconButtonSize::Sm).show(ui);
                    #[cfg(test)]
                    probe::send_notice(SEND_NOTICE, notice.rect);
                    #[cfg(not(test))]
                    let _ = notice;
                    ui.add_space(space::XS);
                    if Button::new("Salva bozza")
                        .icon(icons::SAVE)
                        .size(ButtonSize::Sm)
                        .enabled(addresses_ok)
                        .disabled_reason("Controlla gli indirizzi: c'è un destinatario non valido")
                        .show(ui)
                        .clicked()
                    {
                        response.save = true;
                    }
                    ui.add_space(space::XS);
                    let discard = Button::ghost("Scarta").size(ButtonSize::Sm).show(ui);
                    #[cfg(test)]
                    crate::app::probe::composer_cancel(ui, discard.rect);
                    if discard.clicked() {
                        response.discard = true;
                    }
                },
            );
        });
    }
}

fn wrap(selected: &str, marker: &str, placeholder: &str) -> (String, (usize, usize)) {
    if selected.is_empty() {
        let replacement = format!("{marker}{placeholder}{marker}");
        let start = marker.chars().count();
        let end = start + placeholder.chars().count();
        (replacement, (start, end))
    } else {
        let replacement = format!("{marker}{selected}{marker}");
        let start = marker.chars().count();
        let end = start + selected.chars().count();
        (replacement, (start, end))
    }
}

fn prefix_lines(body: &str, start_byte: usize, end_byte: usize, prefix: &str, include_empty: bool) -> (String, (usize, usize)) {
    let line_start = body.get(..start_byte).and_then(|before| before.rfind('\n')).map_or(0, |index| index + 1);
    let line_end = body.get(end_byte..).and_then(|after| after.find('\n')).map_or(body.len(), |index| end_byte + index);
    let block = body.get(line_start..line_end).unwrap_or("");
    let replacement = block
        .lines()
        .map(|line| if line.is_empty() && !include_empty { line.to_owned() } else { format!("{prefix}{line}") })
        .collect::<Vec<_>>()
        .join("\n");
    let start = body.get(..line_start).map_or(0, |before| before.chars().count());
    let end = start + replacement.chars().count();
    (replacement, (start, end))
}

fn byte_at_char(text: &str, char_index: usize) -> usize {
    text.char_indices().nth(char_index).map_or(text.len(), |(index, _)| index)
}

fn attachment_from_path(path: &Path, position: usize) -> Option<Attachment> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let filename = path.file_name()?.to_string_lossy().into_owned();
    if filename.trim().is_empty() {
        return None;
    }
    let mime = mime_for(&filename);
    Some(Attachment { id: format!("att_drop_{position}"), filename, mime: mime.to_owned(), size: metadata.len() })
}

fn mime_for(filename: &str) -> &'static str {
    let extension = filename.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "md" => "text/plain",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
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

#[must_use]
pub fn quote_body(body: &str) -> String {
    body.lines().map(|line| if line.is_empty() { ">".to_owned() } else { format!("> {line}") }).collect::<Vec<_>>().join("\n")
}

#[must_use]
pub fn signature_body(signature: &str) -> String {
    if signature.trim().is_empty() { String::new() } else { format!("\n\n-- \n{}", signature.trim_end()) }
}

#[must_use]
pub fn compose_body(signature: &str, quoted: Option<String>) -> String {
    let mut body = signature_body(signature);
    if let Some(quoted) = quoted {
        body.push_str("\n\n");
        body.push_str(&quoted);
    }
    body
}

#[cfg(test)]
mod probe {
    use std::sync::Mutex;

    use eframe::egui::Rect;

    static SEND_NOTICE: Mutex<Option<(&'static str, Rect)>> = Mutex::new(None);

    pub(super) fn send_notice(text: &'static str, rect: Rect) {
        if let Ok(mut slot) = SEND_NOTICE.lock() {
            *slot = Some((text, rect));
        }
    }

    pub(super) fn take_send_notice() -> Option<(&'static str, Rect)> {
        SEND_NOTICE.lock().ok().and_then(|slot| *slot)
    }

    pub(super) fn clear() {
        if let Ok(mut slot) = SEND_NOTICE.lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Pos2, RawInput, Rect, Vec2};

    use super::*;

    #[test]
    fn quote_prefixes_every_line() {
        assert_eq!(quote_body("Ciao\n\nGiulia"), "> Ciao\n>\n> Giulia");
    }

    #[test]
    fn bold_and_list_formatting_keep_a_usable_selection() {
        let mut state = ComposerState::new(ComposeMode::New);
        state.body = "Ciao".to_owned();
        state.body_selection = Some((0, 4));
        state.apply_format(FormatAction::Bold, 1.0);
        assert_eq!(state.body, "**Ciao**");
        assert_eq!(state.pending_selection, Some((2, 6)));
        assert!(state.dirty);

        state.body = "uno\ndue".to_owned();
        state.body_selection = Some((0, 7));
        state.apply_format(FormatAction::Bullet, 2.0);
        assert_eq!(state.body, "• uno\n• due");
    }

    #[test]
    fn the_send_notice_says_nothing_leaves_the_machine() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, false);
        let account = Account {
            id: "acc_demo".to_owned(),
            name: "Federico".to_owned(),
            email: "federico@example.it".to_owned(),
            provider: "Aruba".to_owned(),
            signature: String::new(),
        };
        let mut state = ComposerState::new(ComposeMode::New);
        state.to = "giulia.ferri@example.com".to_owned();
        probe::clear();

        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| {
            let _ = Composer::new(&mut state, &account).show(ui);
        });
        output.textures_delta.clear();

        let (notice, rect) = probe::take_send_notice().expect("the composer did not draw the send notice next to Send");
        assert_eq!(notice, SEND_NOTICE);
        assert_eq!(SEND_NOTICE, "Nessun invio SMTP: il messaggio resta in Inviati su questo computer.");
        assert!(rect.width() > 0.0 && rect.height() > 0.0, "the notice has no clickable/tooltip surface: {rect:?}");
    }
}
