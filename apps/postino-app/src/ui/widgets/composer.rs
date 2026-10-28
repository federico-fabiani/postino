//! The composer surface: recipients, subject, body, formatting, attachments and draft state.
//! The component owns no mail state. The screen saves and sends through the operation registry.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::containers::Sides;
use eframe::egui::text::{CCursor, CCursorRange};
use eframe::egui::{self, Align, FontFamily, FontId, Frame, Layout, Margin, Rect, ScrollArea, Stroke, TextEdit, Ui, UiBuilder, Vec2, pos2};
use postino_core::{
    Account, Address, Attachment, BodyFormat, Identity, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_TOTAL_BYTES, MAX_DRAFT_ATTACHMENTS, MessageTemplate,
    PgpKey,
};

use crate::theme::Tokens;
use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, size as ui_size, space, type_size};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonSize, IconButton, IconButtonSize, SectionHeader, TextField};

/// Wait this long after the last keystroke before autosaving. Short enough to feel automatic,
/// long enough that typing a sentence does not write on every key.
pub const AUTOSAVE_DELAY: f64 = 1.2;

/// Shown next to the Send button: sending is real and the undo window is short. The text never
/// claims a local-only move.
pub const SEND_NOTICE: &str = "Invio reale via SMTP. Puoi annullare entro 5 secondi.";
/// Why Send is unavailable for the demo. Shared by the button and Ctrl+Enter so both say the
/// same sentence.
pub const DEMO_SEND_REASON: &str = "La modalità demo non invia: usa un account reale con SMTP";
/// Why Send is unavailable when a store account has no submission endpoint.
pub const NO_SMTP_SEND_REASON: &str = "Questo account non ha un server SMTP: aggiungilo nelle impostazioni per inviare";

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
    /// Selected From address when it is not the account address. The engine validates it against
    /// the stored identities at save and send time.
    pub sender: Option<String>,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub show_cc: bool,
    pub show_bcc: bool,
    pub subject: String,
    pub body: String,
    /// Plain source or compiled email-safe HTML. The source is always `body`; the engine owns the
    /// compilation and never accepts raw HTML from the composer.
    pub body_format: BodyFormat,
    /// Whether this edit explicitly chose a representation. An unchanged existing draft omits
    /// `body_format` so a legacy HTML part is preserved instead of being recompiled by accident.
    pub body_format_explicit: bool,
    pub attachments: Vec<Attachment>,
    /// Parent `Message-ID` for a reply. It is part of the draft, not of the body.
    pub in_reply_to: Option<String>,
    /// Parent attachment ids a forward has not copied yet. The first save copies their bytes
    /// through the engine, then clears this list.
    pub forward_sources: Vec<String>,
    /// A failed forward copy blocks Send until the named rows are removed. It is kept separately
    /// from the draft body state because the body may already be saved.
    pub attachment_failure: Option<String>,
    /// Composer-local failure rows (metadata only, no blob). They are removable even before the
    /// draft metadata save gives them a store row.
    pub failed_attachment_ids: Vec<String>,
    pub focus: Option<ComposerFocus>,
    pub dirty: bool,
    pub last_edit: f64,
    pub saved_at: Option<String>,
    pub save_error: Option<String>,
    pub notice: Option<String>,
    /// Public key ids selected for this send. Signing uses `pgp_sign`; encryption uses every id.
    pub pgp_sign: Option<String>,
    pub pgp_encrypt: Vec<String>,
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
            sender: None,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            show_cc: false,
            show_bcc: false,
            subject: String::new(),
            body: String::new(),
            body_format: BodyFormat::Plain,
            body_format_explicit: false,
            attachments: Vec::new(),
            in_reply_to: None,
            forward_sources: Vec::new(),
            attachment_failure: None,
            failed_attachment_ids: Vec::new(),
            focus: Some(ComposerFocus::To),
            dirty: false,
            last_edit: 0.0,
            saved_at: None,
            save_error: None,
            notice: None,
            pgp_sign: None,
            pgp_encrypt: Vec::new(),
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
    /// Ask the screen to open the injected file picker.
    pub pick_files: bool,
    /// Files dropped on the composer this frame. The screen reads and bounds them; the component
    /// never touches the filesystem.
    pub dropped_files: Vec<PathBuf>,
    /// Ask the screen to remove one attachment row through the operation registry.
    pub remove_attachment: Option<String>,
    /// Ask the screen to insert one local template. The component never edits the body itself.
    pub insert_template: Option<String>,
    /// Ask the screen to show the email-safe HTML preview.
    pub preview: bool,
}

pub struct Composer<'a> {
    state: &'a mut ComposerState,
    account: &'a Account,
    send_available: bool,
    demo: bool,
    attachments_available: bool,
    pgp_keys: &'a [PgpKey],
    templates: &'a [MessageTemplate],
    identities: &'a [Identity],
}

impl<'a> Composer<'a> {
    #[must_use]
    pub fn new(state: &'a mut ComposerState, account: &'a Account) -> Self {
        Self { state, account, send_available: true, demo: true, attachments_available: false, pgp_keys: &[], templates: &[], identities: &[] }
    }

    /// Alternate senders for the account. An empty list keeps the From row read-only.
    #[must_use]
    pub fn identities(mut self, identities: &'a [Identity]) -> Self {
        self.identities = identities;
        self
    }

    /// Local compose templates. The screen owns loading and insertion; an empty list hides the menu.
    #[must_use]
    pub fn templates(mut self, templates: &'a [MessageTemplate]) -> Self {
        self.templates = templates;
        self
    }

    /// The account's stored PGP keys. The screen loads them through the registry; the component
    /// never calls the engine and an empty list hides the row instead of showing a dead toggle.
    #[must_use]
    pub fn pgp_keys(mut self, keys: &'a [PgpKey]) -> Self {
        self.pgp_keys = keys;
        self
    }

    /// Whether this account can submit. The demo has no SMTP endpoint and must not pretend.
    #[must_use]
    pub fn send_available(mut self, available: bool) -> Self {
        self.send_available = available;
        self
    }

    /// Whether the mailbox is the synthetic demo. A real account without an SMTP endpoint gets its
    /// own sentence: "demo" is not the reason it cannot send.
    #[must_use]
    pub fn demo(mut self, demo: bool) -> Self {
        self.demo = demo;
        self
    }

    /// Whether the composer can read files and store their bytes. The demo and a store-less
    /// screenshot session pass false: the button stays visible but refuses with a reason.
    #[must_use]
    pub fn attachments_available(mut self, available: bool) -> Self {
        self.attachments_available = available;
        self
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
        // A drop anywhere on the composer is an attachment; the component reports paths and the
        // screen owns the bounded read. Doing it here (not inside the pinned strip) keeps the body
        // a valid drop target too.
        if ui.ctx().pointer_interact_pos().is_some_and(|pointer| area.contains(pointer)) {
            for file in ui.ctx().input(|input| input.raw.dropped_files.clone()) {
                response.dropped_files.push(file.path().to_path_buf());
            }
        }
        let header_rect = Rect::from_min_size(area.min, Vec2::new(area.width(), ui_size::COMPOSER_HEADER_BLOCK));
        let footer_rect = Rect::from_min_max(pos2(area.min.x, area.max.y - ui_size::COMPOSER_FOOTER_BLOCK), area.max);
        let strip_height = self.attachment_strip_height();
        let strip_rect = Rect::from_min_max(
            pos2(area.min.x, (footer_rect.min.y - strip_height - space::XS).max(header_rect.max.y)),
            pos2(area.max.x, (footer_rect.min.y - space::XS).max(header_rect.max.y)),
        );
        let body_rect = Rect::from_min_max(pos2(area.min.x, header_rect.max.y + space::XS), pos2(area.max.x, strip_rect.min.y - space::XS));

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
                    self.pgp_bar(ui, &t);
                    ui.add_space(space::XS);

                    let subject = TextField::new(&mut self.state.subject, "Oggetto")
                        .id_salt(("composer-subject", self.state.session))
                        .icon(icons::MAIL)
                        .focus(self.state.focus == Some(ComposerFocus::Subject))
                        .show(ui);
                    changed |= subject.changed();
                    ui.add_space(space::SM);

                    self.formatting_bar(ui, &t, now, &mut response);
                    changed |= self.body_field(ui, &t);
                    // A little tail so the last line of the body breathes at the scroll edge.
                    ui.add_space(space::SM);
                });
            #[cfg(test)]
            crate::app::probe::composer_scroll(ui, scroll.state.offset.y, scroll.inner_rect, scroll.content_size);
            #[cfg(not(test))]
            let _ = scroll;
        });

        if strip_height > 0.0 {
            ui.scope_builder(UiBuilder::new().max_rect(strip_rect), |ui| {
                self.attachment_section(ui, &t, &mut response);
            });
        }

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
                if self.identities.is_empty() {
                    ui.add(
                        egui::Label::new(typography::text(Role::Meta, format!("{} <{}>", self.account.name, self.account.email), t.text_muted))
                            .truncate(),
                    );
                    return;
                }
                let selected = self.state.sender.as_deref();
                let label =
                    selected.and_then(|email| self.identities.iter().find(|identity| identity.email.eq_ignore_ascii_case(email))).map_or_else(
                        || format!("{} <{}>", self.account.name, self.account.email),
                        |identity| format!("{} <{}>", identity.name, identity.email),
                    );
                let mut chosen = None;
                let combo = egui::ComboBox::from_id_salt(("composer-from", self.state.session))
                    .selected_text(label)
                    .width((ui.available_width() - 8.0).max(160.0))
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(selected.is_none(), format!("{} <{}>", self.account.name, self.account.email)).clicked() {
                            chosen = Some(None);
                        }
                        for identity in self.identities {
                            if ui
                                .selectable_label(
                                    selected.is_some_and(|email| email.eq_ignore_ascii_case(&identity.email)),
                                    format!("{} <{}>", identity.name, identity.email),
                                )
                                .clicked()
                            {
                                chosen = Some(Some(identity.email.clone()));
                            }
                        }
                    });
                #[cfg(test)]
                crate::app::probe::composer_from(ui, combo.response.rect);
                #[cfg(not(test))]
                let _ = combo;
                if let Some(chosen) = chosen
                    && chosen.as_deref() != selected
                {
                    let now = ui.input(|input| input.time);
                    self.change_sender(chosen, now);
                }
            });
        });
    }

    /// Switch the From address and carry the conventional signature with it. Text after the old
    /// signature is preserved; a body the person already edited without a matching signature is
    /// never rewritten.
    fn change_sender(&mut self, sender: Option<String>, now: f64) {
        let old_signature = self.sender_signature(self.state.sender.as_deref()).to_owned();
        let new_signature = self.sender_signature(sender.as_deref()).to_owned();
        if old_signature != new_signature {
            let old_prefix = postino_core::new_message_body(&old_signature);
            let new_prefix = postino_core::new_message_body(&new_signature);
            if !old_prefix.is_empty() && self.state.body.starts_with(&old_prefix) {
                self.state.body.replace_range(0..old_prefix.len(), &new_prefix);
            } else if self.state.body.is_empty() {
                self.state.body = new_prefix;
            }
        }
        self.state.sender = sender;
        self.state.touch(now);
    }

    fn sender_signature(&self, email: Option<&str>) -> &str {
        match email {
            Some(email) => {
                self.identities.iter().find(|identity| identity.email.eq_ignore_ascii_case(email)).map_or("", |identity| identity.signature.as_str())
            }
            None => self.account.signature.as_str(),
        }
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
            // The A row also carries the Cc and Bcc reveal buttons to its right. Reserving only
            // their label width let the row grow past the 640 pt card and shifted the whole modal.
            let reserved = 112.0;
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

    /// Signing and encryption are per-send choices, not draft fields: cancelling a draft leaves no
    /// key selected. The row disappears when the account has no keys, so it never offers a dead
    /// toggle.
    fn pgp_bar(&mut self, ui: &mut Ui, t: &Tokens) {
        if self.pgp_keys.is_empty() {
            return;
        }
        let keys = self.pgp_keys.to_vec();
        let has_secret = keys.iter().any(|key| key.has_secret);
        let _row = ui.horizontal_wrapped(|ui| {
            ui.label(icons::icon(icons::LOCK, 14.0, t.text_faint));
            ui.add_space(space::XS);
            if has_secret {
                let selected = self
                    .state
                    .pgp_sign
                    .as_deref()
                    .and_then(|id| keys.iter().find(|key| key.id == id))
                    .map_or_else(|| "Nessuna firma".to_owned(), pgp_key_label);
                egui::ComboBox::from_id_salt(("composer-pgp-sign", self.state.session)).width(190.0).selected_text(selected).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.state.pgp_sign, None, "Nessuna firma");
                    for key in keys.iter().filter(|key| key.has_secret) {
                        ui.selectable_value(&mut self.state.pgp_sign, Some(key.id.clone()), pgp_key_label(key));
                    }
                });
            } else {
                ui.label(typography::text(Role::Meta, "Nessuna chiave privata per firmare", t.text_faint));
            }
            let count = self.state.pgp_encrypt.len();
            let label = if count == 0 { "Cifra".to_owned() } else { format!("Cifra · {count}") };
            ui.menu_button(label, |ui| {
                for key in &keys {
                    let mut selected = self.state.pgp_encrypt.contains(&key.id);
                    if ui.checkbox(&mut selected, pgp_key_label(key)).changed() {
                        if selected {
                            self.state.pgp_encrypt.push(key.id.clone());
                        } else {
                            self.state.pgp_encrypt.retain(|id| id != &key.id);
                        }
                    }
                }
            });
        });
        #[cfg(test)]
        crate::app::probe::composer_pgp(ui, _row.response.rect);
    }

    fn formatting_bar(&mut self, ui: &mut Ui, t: &Tokens, now: f64, response: &mut ComposerResponse) {
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
                    if self.state.body_format == BodyFormat::Html {
                        let preview = IconButton::new(icons::EYE, "Anteprima HTML").size(IconButtonSize::Sm).show(ui);
                        #[cfg(test)]
                        crate::app::probe::composer_preview_button(ui, preview.rect);
                        if preview.clicked() {
                            response.preview = true;
                        }
                    }
                    let format = match self.state.body_format {
                        BodyFormat::Plain => "Testo",
                        BodyFormat::Html => "HTML",
                    };
                    if Button::ghost(format).size(ButtonSize::Sm).show(ui).clicked() {
                        self.state.body_format = match self.state.body_format {
                            BodyFormat::Plain => BodyFormat::Html,
                            BodyFormat::Html => BodyFormat::Plain,
                        };
                        self.state.body_format_explicit = true;
                        self.state.touch(now);
                    }
                    if !self.templates.is_empty() {
                        ui.menu_button(format!("{} Modelli", icons::TEMPLATE), |ui| {
                            for template in self.templates {
                                if ui.button(&template.name).clicked() {
                                    response.insert_template = Some(template.id.clone());
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .on_hover_text("Inserisci un modello locale");
                    }
                    ui.label(typography::text(Role::Caption, "Markdown", t.text_faint));
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

    /// The pinned attachment strip height. Two rows fit; more scroll inside the strip. An empty
    /// composer with a usable picker shows a one-line hint; the demo shows nothing because its
    /// disabled button already carries the reason.
    fn attachment_strip_height(&self) -> f32 {
        if self.state.attachments.is_empty() {
            if self.attachments_available { 32.0 } else { 0.0 }
        } else {
            // Header + total + two content-sized rows + the small tail.
            144.0
        }
    }

    fn attachment_section(&mut self, ui: &mut Ui, t: &Tokens, response: &mut ComposerResponse) -> bool {
        ui.add_space(space::XS);
        if !self.state.attachments.is_empty() {
            SectionHeader::new("ALLEGATI").count(self.state.attachments.len()).show(ui);
            let total = self.state.attachments.iter().map(|attachment| attachment.size).sum::<u64>();
            ui.label(typography::mono(
                format!("{} · {}", plural_attachments(self.state.attachments.len()), format_size_it(total)),
                9.5,
                t.text_muted,
            ));
            ScrollArea::vertical().max_height(ui_size::ATTACHMENT_LIST_MAX).auto_shrink([false, true]).show(ui, |ui| {
                let mut remove = None;
                for attachment in &self.state.attachments {
                    Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::SM, inset::XS)).show(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.label(icons::icon(icons::PAPERCLIP, ui_size::ICON_SM, t.text_muted));
                                let name = ui.add(egui::Label::new(typography::text(Role::Caption, attachment.filename.as_str(), t.text)).truncate());
                                name.on_hover_text(attachment.filename.as_str());
                                ui.label(typography::mono(attachment.mime.as_str(), 9.0, t.text_faint));
                                ui.label(typography::mono(format_size_it(attachment.size), 9.5, t.text_faint));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let button = IconButton::new(icons::CLOSE, "Rimuovi allegato").size(IconButtonSize::Sm).show(ui);
                                    #[cfg(test)]
                                    crate::app::probe::composer_attachment_remove(ui, &attachment.id, button.rect);
                                    #[cfg(test)]
                                    crate::app::probe::composer_attachment_remove_id(ui, &attachment.id, button.id);
                                    #[cfg(test)]
                                    crate::app::probe::composer_attachment_remove_clicked(ui, &attachment.id, button.clicked());
                                    if button.clicked() {
                                        remove = Some(attachment.id.clone());
                                    }
                                });
                            });
                        },
                    );
                }
                if let Some(id) = remove {
                    response.remove_attachment = Some(id);
                }
            });
            if let Some(warning) = attachment_limit_warning(&self.state.attachments) {
                ui.label(typography::text(Role::Caption, warning, t.danger));
            }
        } else if self.attachments_available {
            Frame::new()
                .fill(t.panel_alt)
                .stroke(Stroke::new(1.0, t.border))
                .corner_radius(radius::CONTROL)
                .inner_margin(Margin::symmetric(inset::LG, inset::SM))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(icons::icon(icons::PAPERCLIP, ui_size::ICON_SM, t.text_faint));
                        ui.label(typography::text(
                            Role::Caption,
                            format!(
                                "Trascina qui i file o usa Allega file · max {} per file, {MAX_DRAFT_ATTACHMENTS} allegati",
                                format_size_it(MAX_ATTACHMENT_BYTES)
                            ),
                            t.text_faint,
                        ));
                    });
                });
        }
        false
    }

    fn footer(&mut self, ui: &mut Ui, t: &Tokens, rect: Rect, addresses_ok: bool, send_ready: bool, response: &mut ComposerResponse) {
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, t.border));
        let notice_text = self.state.notice.as_deref().unwrap_or(if self.send_available {
            SEND_NOTICE
        } else if self.demo {
            "Demo: nessun messaggio lascia il computer."
        } else {
            "Nessun server SMTP configurato: questo account può solo ricevere."
        });
        let notice_rect = Rect::from_min_size(pos2(rect.min.x, rect.top() + space::XS), Vec2::new(rect.width(), 14.0));
        ui.scope_builder(UiBuilder::new().max_rect(notice_rect), |ui| {
            let notice = ui.add(egui::Label::new(typography::text(Role::Caption, notice_text, t.text_muted)).truncate());
            #[cfg(test)]
            probe::send_notice(notice_text, notice.rect);
            #[cfg(not(test))]
            let _ = notice;
        });
        let row_rect = Rect::from_min_size(pos2(rect.min.x, notice_rect.bottom() + space::XS), Vec2::new(rect.width(), ui_size::CONTROL_LG));
        // Two explicit columns. A `Sides` row lets the left group grow under the actions when the
        // draft state is long; the footer then widens the whole card. The left column always ends
        // before the right one, so the saved state truncates and Send stays visible.
        let right_width = 350.0_f32.min(row_rect.width() * 0.62);
        let right_rect = Rect::from_min_max(pos2(row_rect.right() - right_width, row_rect.top()), row_rect.max);
        let left_rect = Rect::from_min_max(row_rect.min, pos2(row_rect.right() - right_width - space::SM, row_rect.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(left_rect).layout(Layout::left_to_right(Align::Center)), |ui| {
            let (text, color) = if let Some(error) = &self.state.save_error {
                (format!("Bozza non salvata · {error}"), t.danger)
            } else if self.state.dirty {
                ("Salvataggio automatico in corso…".to_owned(), t.text_faint)
            } else if let Some(at) = &self.state.saved_at {
                (format!("Bozza salvata alle {at}"), t.success)
            } else {
                ("La bozza si salva da sola".to_owned(), t.text_faint)
            };
            let attach_reason = if self.attachments_available {
                "Scegli uno o più file da allegare"
            } else if self.demo {
                "La modalità demo non ha un archivio locale: gli allegati reali richiedono un account"
            } else {
                "Questo account non ha un archivio locale: riapri Postino su un archivio per allegare file"
            };
            let attach = Button::new("Allega file")
                .icon(icons::PAPERCLIP)
                .size(ButtonSize::Sm)
                .enabled(self.attachments_available)
                .disabled_reason(attach_reason)
                .show(ui);
            #[cfg(test)]
            crate::app::probe::composer_attach(ui, attach.rect, self.attachments_available);
            #[cfg(test)]
            crate::app::probe::composer_attach_id(ui, attach.id);
            #[cfg(test)]
            crate::app::probe::composer_attach_clicked(ui, attach.clicked());
            if attach.clicked() {
                response.pick_files = true;
            }
            let remaining = ui.available_width().max(0.0);
            let state = ui.add_sized([remaining, ui_size::CONTROL_LG], egui::Label::new(typography::text(Role::Caption, text, color)).truncate());
            #[cfg(test)]
            crate::app::probe::composer_footer_state(ui, state.rect);
            #[cfg(not(test))]
            let _ = state;
        });
        ui.scope_builder(UiBuilder::new().max_rect(right_rect).layout(Layout::right_to_left(Align::Center)), |ui| {
            let send_reason = if !self.send_available && self.demo {
                DEMO_SEND_REASON
            } else if !self.send_available {
                NO_SMTP_SEND_REASON
            } else if !addresses_ok {
                "Controlla gli indirizzi: c'è un destinatario non valido"
            } else if !send_ready {
                "Aggiungi almeno un destinatario"
            } else {
                ""
            };
            let send_label = "Invia";
            let send = Button::primary(send_label)
                .icon(icons::SEND)
                .shortcut("Ctrl+Invio")
                .enabled(send_ready && self.send_available)
                .disabled_reason(send_reason)
                .show(ui);
            #[cfg(test)]
            crate::app::probe::composer_send(ui, send.rect);
            if send.clicked() {
                response.send = true;
            }
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

fn plural_attachments(count: usize) -> String {
    if count == 1 { "1 allegato".to_owned() } else { format!("{count} allegati") }
}

fn format_size_it(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0).replace('.', ",")
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)).replace('.', ",")
    }
}

/// One sentence for the composer when a row set is already over a cap. The operation refuses the
/// next file, but an older draft can still carry an over-limit set that must be visible.
fn attachment_limit_warning(attachments: &[Attachment]) -> Option<String> {
    if attachments.len() > MAX_DRAFT_ATTACHMENTS {
        return Some(format!("Troppi allegati: il limite è {MAX_DRAFT_ATTACHMENTS}. Rimuovine alcuni prima di inviare."));
    }
    if attachments.iter().any(|attachment| attachment.size > MAX_ATTACHMENT_BYTES) {
        return Some(format!("Un allegato supera {}. Sostituiscilo con un file più piccolo.", format_size_it(MAX_ATTACHMENT_BYTES)));
    }
    let total = attachments.iter().map(|attachment| attachment.size).sum::<u64>();
    if total > MAX_ATTACHMENT_TOTAL_BYTES {
        return Some(format!("Gli allegati totali superano {}. Rimuovine alcuni prima di inviare.", format_size_it(MAX_ATTACHMENT_TOTAL_BYTES)));
    }
    None
}

fn pgp_key_label(key: &PgpKey) -> String {
    let name = key.user_id.as_deref().unwrap_or("chiave PGP");
    let short = key.fingerprint.chars().take(16).collect::<String>();
    if short.is_empty() { name.to_owned() } else { format!("{name} · {short}") }
}

#[cfg(test)]
mod probe {
    use std::cell::RefCell;

    use eframe::egui::Rect;

    thread_local! {
        static SEND_NOTICE: RefCell<Option<(String, Rect)>> = const { RefCell::new(None) };
    }

    pub(super) fn send_notice(text: &str, rect: Rect) {
        SEND_NOTICE.with(|slot| {
            *slot.borrow_mut() = Some((text.to_owned(), rect));
        });
    }

    pub(super) fn take_send_notice() -> Option<(String, Rect)> {
        SEND_NOTICE.with(|slot| slot.borrow().clone())
    }

    pub(super) fn clear() {
        SEND_NOTICE.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Pos2, RawInput, Rect, Vec2};

    use super::*;

    #[test]
    fn quote_prefixes_every_line() {
        assert_eq!(postino_core::quote_body("Ciao\n\nGiulia"), "> Ciao\n>\n> Giulia");
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
        assert_eq!(SEND_NOTICE, "Invio reale via SMTP. Puoi annullare entro 5 secondi.");
        assert!(rect.width() > 0.0 && rect.height() > 0.0, "the notice has no clickable/tooltip surface: {rect:?}");
    }

    #[test]
    fn a_demo_composer_disables_send_with_a_reason() {
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
            let _ = Composer::new(&mut state, &account).send_available(false).show(ui);
        });
        output.textures_delta.clear();
        let (notice, _) = probe::take_send_notice().expect("the demo notice");
        assert!(notice.contains("nessun messaggio lascia il computer"), "{notice}");
    }

    #[test]
    fn a_real_account_without_smtp_says_so_instead_of_blaming_the_demo() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, false);
        let account = Account {
            id: "acc_real".to_owned(),
            name: "Federico".to_owned(),
            email: "federico@example.it".to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
        };
        let mut state = ComposerState::new(ComposeMode::New);
        state.to = "giulia.ferri@example.com".to_owned();
        probe::clear();
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| {
            let _ = Composer::new(&mut state, &account).send_available(false).demo(false).show(ui);
        });
        output.textures_delta.clear();
        let (notice, _) = probe::take_send_notice().expect("the no-SMTP notice");
        assert!(notice.contains("Nessun server SMTP"), "{notice}");
        assert!(!notice.contains("Demo"), "a real read-only account is not the demo: {notice}");
    }
}
