//! Accounts and settings surfaces: the account switcher, the settings dialog with its signature
//! editor, the licences and remove-account modals, and the About window.

use super::*;

impl PostinoApp {
    pub(super) fn account_menu(&mut self, ui: &mut Ui) {
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

    pub(super) fn open_data_folder(&mut self) {
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

    pub(super) fn accounts_dialog(&mut self, ui: &mut Ui) {
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
        let mut copied_path = false;
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
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
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
                ui.add_space(space::XS);
                let _settings_scroll = egui::ScrollArea::vertical()
                    .id_salt("settings-scroll")
                    .max_height(scroll_height)
                    .content_margin(Margin { right: 10, ..Margin::ZERO })
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
                            ui.add_space(space::SM);
                            // The two path actions sit on the section line, left-aligned with the rest of
                            // the DATI controls, instead of floating at the right edge of the heading.
                            let copy_store = Button::ghost("Copia archivio").size(ButtonSize::Sm).icon(icons::COPY).show(ui);
                            #[cfg(test)]
                            probe::settings_copy(ui, "store", copy_store.rect);
                            if copy_store.clicked() {
                                ui.ctx().copy_text(path.clone());
                                copied_path = true;
                            }
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
                                copied_path = true;
                            }
                        });
                        ui.add_space(space::XS);
                        ui.add(
                            egui::Label::new(typography::mono(path.as_str(), type_size::CAPTION, t.text_faint)).selectable(true).wrap(),
                        );
                        if let Some(blob) = &blob {
                            ui.add(
                                egui::Label::new(typography::mono(blob.as_str(), type_size::CAPTION, t.text_faint)).selectable(true).wrap(),
                            );
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
                });
                #[cfg(test)]
                probe::settings_footer(ui, _footer.response.rect);
            });
        // A copy icon only raises the flag; the confirmation is written once the modal is closed,
        // the same deferred-apply pattern the background toggles use.
        if copied_path {
            self.status = "Percorso copiato".to_owned();
        }
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
            self.reset_reading_pane_width(ui.ctx());
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

    pub(super) fn licences_dialog(&mut self, ui: &mut Ui) {
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
                        ui.add(egui::Label::new(typography::mono(include_str!("../../../../ATTRIBUTION.md"), 10.5, t.text_muted)).selectable(true));
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
    pub(super) fn account_remove_dialog(&mut self, ui: &mut Ui) {
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
    pub(super) fn about(&mut self, ui: &mut Ui) {
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
                    wordmark(ui, "Postino", 22.0, t.text);
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
