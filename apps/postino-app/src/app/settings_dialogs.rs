//! Settings dialogs: the PGP passphrase, import and delete prompts, and the template and
//! sender-identity editors. The settings page owns the state; these modals only collect input.

use super::*;

impl PostinoApp {
    pub(super) fn pgp_prompt_dialog(&mut self, ui: &mut Ui) {
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
                    let Some(id) = self.reading.pgp_reading.as_ref().map(|state| state.message_id.clone()) else { return };
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

    pub(super) fn pgp_import_dialog(&mut self, ui: &mut Ui) {
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

    pub(super) fn pgp_delete_dialog(&mut self, ui: &mut Ui) {
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

    pub(super) fn template_editor_dialog(&mut self, ui: &mut Ui) {
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

    pub(super) fn template_delete_dialog(&mut self, ui: &mut Ui) {
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

    pub(super) fn identity_editor_dialog(&mut self, ui: &mut Ui) {
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

    pub(super) fn identity_delete_dialog(&mut self, ui: &mut Ui) {
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
}
