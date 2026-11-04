//! The reading pane surface: the open message's header and participants, the block body,
//! attachments, links, remote content, invites and the pane's own sheets. The shell still owns
//! the state; this module owns the behaviour.

use super::*;

/// Everything the reading pane owns while a message is open: the cached body, the invite, the
/// pending link, the attachment sheets and the unsubscribe choice. It is defined here because the
/// pane is the only screen that reads it; `close_reading` and `open_message` clear the parts that
/// belong to the previous message.
#[derive(Default)]
pub(super) struct ReadingUi {
    /// A link waiting for the confirmation sheet. The URL has already passed the allowlist.
    pub(super) pending_link: Option<PendingLink>,
    /// The block view of the open message: parsed once per body, not once per frame.
    pub(super) body_cache: Option<BodyCache>,
    /// A dangerous attachment waiting for the explicit warning.
    pub(super) attachment_warning: Option<PendingAttachment>,
    /// An attachment whose chosen path already exists and waits for overwrite confirmation.
    pub(super) attachment_overwrite: Option<PendingAttachment>,
    /// Stable Save-button ids so a headless test can drive the keyboard path.
    pub(super) attachment_save_ids: BTreeMap<String, Id>,
    /// A bounded, display-only attachment preview. The bytes are dropped when the sheet closes.
    pub(super) attachment_preview: Option<AttachmentPreviewState>,
    /// One-entry cache for the parsed invite of the open message.
    pub(super) invite_cache: Option<(String, Option<CalendarInviteView>)>,
    /// The PGP verdict and any display-only decrypted body for the open message.
    pub(super) pgp_reading: Option<PgpReadingState>,
    /// The unsubscribe choices for the open message. Empty means the header was absent or invalid.
    pub(super) unsubscribe_info: Option<UnsubscribeInfo>,
    /// The message whose headers were already checked; a negative result is cached too.
    pub(super) unsubscribe_checked: Option<String>,
    /// The choice sheet is open. It never sends or opens anything until the person picks a target.
    pub(super) show_unsubscribe: bool,
}

impl PostinoApp {
    /// `full` is the overlay/full-screen path: the reading pane covers the list and offers
    /// "Torna alla posta". It is used for the explicit full-screen mode and below
    /// [`READING_PANE_MIN_WIDTH`], where a side pane cannot keep a usable list.
    pub(super) fn reading_pane(&mut self, ui: &mut Ui, full: bool) {
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
        self.reading.attachment_save_ids = save_ids;
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
    pub(super) fn rendered_body(&mut self, message: &Message) -> Arc<body::View> {
        let stale = match self.reading.body_cache.as_ref() {
            None => true,
            Some(cache) => {
                cache.message_id != message.summary.id || cache.html.as_deref() != message.body_html.as_deref() || cache.text != message.body_text
            }
        };
        if stale {
            let view = Arc::new(body::SubsetBodyRenderer.render(&body::StoredBody { html: message.body_html.as_deref(), text: &message.body_text }));
            self.reading.body_cache = Some(BodyCache {
                message_id: message.summary.id.clone(),
                html: message.body_html.clone(),
                text: message.body_text.clone(),
                view: Arc::clone(&view),
            });
            return view;
        }
        match &self.reading.body_cache {
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
        self.reading.pgp_reading = None;
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
        self.reading.pgp_reading =
            Some(PgpReadingState { message_id: message.summary.id.clone(), kind: result.kind, verify: Some(result), decrypt: None, view: None });
    }

    /// Read the unsubscribe headers once per open message. A missing or invalid header is cached as
    /// a negative result so the frame loop does not repeat the operation.
    fn sync_unsubscribe(&mut self, message: &Message) {
        if self.demo
            || self.screenshot_message.as_ref().is_some_and(|shot| shot.summary.id == message.summary.id)
            || self.reading.unsubscribe_checked.as_deref() == Some(message.summary.id.as_str())
        {
            return;
        }
        self.reading.unsubscribe_info = None;
        self.reading.show_unsubscribe = false;
        let mut args = json!({ "id": message.summary.id });
        if let Some(account) = &self.active_account {
            args["account"] = json!(account);
        }
        // A transient daemon error must not hide the entry point until the message is reopened.
        let Ok(value) = self.op("message_unsubscribe", args) else { return };
        let Ok(info) = serde_json::from_value::<UnsubscribeInfo>(value) else { return };
        self.reading.unsubscribe_checked = Some(message.summary.id.clone());
        if !info.targets.is_empty() {
            self.reading.unsubscribe_info = Some(info);
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
        self.reading.show_unsubscribe = false;
        self.request_link_open(&link);
        ui.ctx().request_repaint();
    }

    /// The explicit choice sheet. It lists every target from the header and never performs one.
    pub(super) fn unsubscribe_sheet(&mut self, ui: &mut Ui) {
        if !self.reading.show_unsubscribe {
            return;
        }
        let Some(info) = self.reading.unsubscribe_info.clone() else {
            self.reading.show_unsubscribe = false;
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
            self.reading.show_unsubscribe = false;
            self.unsubscribe_mailto(&target);
        } else if let Some(target) = web {
            self.unsubscribe_web(&target, ui);
        } else if close || response.should_close() {
            self.reading.show_unsubscribe = false;
        }
    }

    /// Ask the engine to decrypt with no passphrase first. An unprotected key opens immediately; a
    /// protected key answers `Locked` and only then the sheet asks for the secret.
    fn begin_pgp_decrypt(&mut self) {
        let Some(state) = self.reading.pgp_reading.as_ref() else { return };
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

    pub(super) fn pgp_decrypt(&mut self, id: &str, passphrase: &str) -> Option<PgpDecryptResult> {
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

    pub(super) fn apply_pgp_decrypt(&mut self, result: PgpDecryptResult) {
        let Some(state) = self.reading.pgp_reading.as_mut() else { return };
        if result.state == PgpDecryptState::Decrypted {
            let text = result.text.as_deref().unwrap_or_default();
            let view = body::SubsetBodyRenderer.render(&body::StoredBody { html: result.html.as_deref(), text });
            state.view = Some(Arc::new(view));
        }
        state.decrypt = Some(result);
    }

    fn pgp_body_view(&self, message_id: &str) -> Option<Arc<body::View>> {
        let state = self.reading.pgp_reading.as_ref().filter(|state| state.message_id == message_id)?;
        state.decrypt.as_ref().filter(|result| result.state == PgpDecryptState::Decrypted)?;
        state.view.clone()
    }

    /// The badge and the decrypt action. It reads the cached verdict, never the parsed body: the
    /// UI cannot tell a signed message from an unsigned one without the retained raw bytes.
    fn pgp_reading_row(&mut self, ui: &mut Ui, t: &Tokens, message_id: &str) {
        let Some(state) = self.reading.pgp_reading.as_ref().filter(|state| state.message_id == message_id) else { return };
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
            && let Some(state) = self.reading.pgp_reading.as_mut()
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
        if let Some((id, invite)) = &self.reading.invite_cache
            && id == &message.summary.id
        {
            return invite.clone();
        }
        let invite = self.engine.message_invite(&message.summary.id).ok().flatten();
        self.reading.invite_cache = Some((message.summary.id.clone(), invite.clone()));
        invite
    }

    /// Ask the engine for the reply draft, then open it in the composer. The engine writes the
    /// draft; this method never sends and never marks anything on a calendar.
    pub(super) fn reply_to_invite(&mut self, response: InviteResponse) {
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

        let has_unsubscribe = self.reading.unsubscribe_info.is_some();
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
            Some(ReadingAction::Unsubscribe) => self.reading.show_unsubscribe = self.reading.unsubscribe_info.is_some(),
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
    pub(super) fn request_link_open(&mut self, link: &ExtractedLink) {
        if !reading::is_allowed_url(&link.url) {
            self.push_info_toast("Questo link non è apribile: sono ammessi solo http, https e mailto.", ToastKind::Error);
            return;
        }
        self.reading.pending_link = Some(PendingLink {
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

    pub(super) fn confirm_pending_link(&mut self) {
        let Some(pending) = self.reading.pending_link.take() else {
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

    pub(super) fn link_confirmation(&mut self, ui: &mut Ui) {
        let Some(pending) = self.reading.pending_link.clone() else {
            return;
        };
        let focus_cancel = pending.focus_cancel;
        if let Some(link) = self.reading.pending_link.as_mut() {
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
            self.reading.pending_link = None;
        }
    }

    pub(super) fn request_attachment_save(&mut self, attachment: &Attachment) {
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
            self.reading.attachment_warning = Some(pending);
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
                self.reading.attachment_preview = Some(AttachmentPreviewState {
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
            self.reading.attachment_preview = Some(state);
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
        self.reading.attachment_preview = Some(state);
    }

    pub(super) fn attachment_preview_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.reading.attachment_preview.as_mut() else { return };
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
            if let Some(state) = self.reading.attachment_preview.take() {
                self.request_attachment_save(&state.attachment);
            }
        } else if close || response.should_close() {
            self.reading.attachment_preview = None;
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
                    self.reading.attachment_warning = Some(pending);
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
            self.reading.attachment_overwrite = Some(pending);
        } else {
            self.write_attachment(pending);
        }
    }

    pub(super) fn write_attachment(&mut self, pending: PendingAttachment) {
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
                    self.reading.attachment_overwrite = Some(pending);
                    return;
                }
                self.push_info_toast(format!("Non è stato possibile salvare “{}”: {error}", pending.filename), ToastKind::Error);
            }
        }
    }

    pub(super) fn attachment_warning_dialog(&mut self, ui: &mut Ui) {
        let Some(pending) = self.reading.attachment_warning.as_ref() else {
            return;
        };
        let filename = pending.filename.clone();
        let chosen = pending.path.as_ref().map(|path| path.display().to_string());
        let message = pending.dangerous.unwrap_or(DangerousKind::Executable).message();
        let focus_cancel = pending.focus_cancel;
        if let Some(pending) = self.reading.attachment_warning.as_mut() {
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
            if let Some(pending) = self.reading.attachment_warning.take() {
                if pending.path.is_some() {
                    self.finish_attachment_destination(pending);
                } else {
                    self.choose_attachment_path(pending);
                }
            }
        } else if cancel || response.should_close() {
            self.reading.attachment_warning = None;
            self.status = "Salvataggio annullato".to_owned();
        }
    }

    pub(super) fn attachment_overwrite_dialog(&mut self, ui: &mut Ui) {
        let Some(pending) = self.reading.attachment_overwrite.as_ref() else {
            return;
        };
        let path = pending.path.as_ref().map_or_else(|| pending.filename.clone(), |path| path.display().to_string());
        let focus_cancel = pending.focus_cancel;
        if let Some(pending) = self.reading.attachment_overwrite.as_mut() {
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
            if let Some(mut pending) = self.reading.attachment_overwrite.take() {
                pending.overwrite_confirmed = true;
                self.write_attachment(pending);
            }
        } else if cancel || response.should_close() {
            self.reading.attachment_overwrite = None;
            self.status = "Salvataggio annullato".to_owned();
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

/// A link the reader asked to open. The confirmation sheet is the only path to the opener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingLink {
    pub(super) url: String,
    /// The host or recipient domain, already normalized by `reading::find_links`.
    pub(super) host: String,
    /// The explicit `mailto:` recipient, shown instead of a browser host.
    pub(super) recipient: Option<String>,
    pub(super) mailto: bool,
    pub(super) idn: bool,
    pub(super) non_ascii: bool,
    pub(super) backslash: bool,
    /// Request focus on "Annulla" on the first frame, then let the keyboard move freely.
    pub(super) focus_cancel: bool,
}

/// The block view of the open message, keyed by the stored fragment and text it was built from.
pub(super) struct BodyCache {
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
pub(super) struct PgpReadingState {
    pub(super) message_id: String,
    pub(super) kind: PgpMimeKind,
    pub(super) verify: Option<PgpVerifyResult>,
    pub(super) decrypt: Option<PgpDecryptResult>,
    pub(super) view: Option<Arc<body::View>>,
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
pub(super) struct AttachmentPreviewState {
    attachment: Attachment,
    pub(super) text: Option<String>,
    pub(super) texture: Option<egui::TextureHandle>,
    pub(super) error: Option<String>,
    truncated: bool,
}

pub(super) struct PendingAttachment {
    pub(super) filename: String,
    pub(super) bytes: Vec<u8>,
    /// `Some` once a path has been chosen, including while a post-dialog warning is on screen.
    pub(super) path: Option<PathBuf>,
    /// The warning already accepted for this save, if any.
    pub(super) dangerous: Option<DangerousKind>,
    /// Request focus on "Annulla" on the first frame of a sheet.
    pub(super) focus_cancel: bool,
    /// The person already confirmed replacing this exact path.
    pub(super) overwrite_confirmed: bool,
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

pub(super) const MAX_PREVIEW_TEXT: usize = 64 * 1024;
const MAX_PREVIEW_IMAGE_DIMENSION: u32 = 2048;
const MAX_PREVIEW_IMAGE_ALLOC: u64 = 32 * 1024 * 1024;

/// Text previews are bounded before decoding and control characters are replaced so a hostile
/// attachment cannot reorder the interface with bidi controls.
pub(super) fn preview_text(bytes: &[u8]) -> (String, bool) {
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

/// The warning class of the file the person actually chose, using the same sanitizer and
/// classifier as the suggested name.
fn attachment_destination_kind(path: &Path) -> Option<DangerousKind> {
    let name = path.file_name()?.to_string_lossy();
    // The sanitized name preserves a truncated extension; classifying the raw name too catches a
    // path the dialog returned with a long stem that the suggestion would shorten.
    reading::dangerous_kind(&reading::safe_filename(&name)).or_else(|| reading::dangerous_kind(&name))
}
