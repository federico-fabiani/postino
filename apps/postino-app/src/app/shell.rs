//! The shell chrome: title bar, toolbar, loading line, status bar, command palette, shortcut
//! sheet and the label windows that float over the message list.

use super::*;

impl PostinoApp {
    /// The quiet loading line under the toolbar. It is painted only after [`LOADING_DELAY`]; the
    /// fade uses the same global animation switch, so reduced motion draws it at full alpha at once.
    pub(super) fn loading_line(&mut self, ui: &Ui) {
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
    pub(super) fn title_bar(&mut self, ui: &mut Ui) {
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
    pub(super) fn toolbar(&mut self, ui: &mut Ui) {
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
    pub(super) fn status_bar(&mut self, ui: &mut Ui) {
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
                    // Who owns the store is part of the product surface: the window must never
                    // look like the only writer while a daemon holds the archive.
                    if !self.demo && self.boot_failure.is_none() {
                        ui.add_space(space::MD);
                        let color = match self.connection {
                            daemon::Connection::Connected(_) => t.success,
                            daemon::Connection::Reconnecting { .. } | daemon::Connection::Lost => t.gold_ink,
                            _ => t.text_muted,
                        };
                        let chip = ui.add(egui::Label::new(typography::text(Role::Meta, self.connection.label(), color)).truncate());
                        chip.on_hover_text(self.connection.detail());
                        if self.connection.can_start(self.demo) {
                            ui.add_space(space::XS);
                            if Button::ghost("Avvia daemon").size(ButtonSize::Sm).show(ui).clicked() {
                                self.request_start_daemon();
                            }
                        } else if self.connection.remote() {
                            ui.add_space(space::XS);
                            if Button::ghost("Ferma daemon").size(ButtonSize::Sm).show(ui).clicked() {
                                self.request_stop_daemon();
                            }
                        }
                    }
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
    /// The command palette. It is a modal layer, not a window: no title band, no close button, a
    /// scrim that blocks the shell, the search field on top, ten rows with an internal scroll and
    /// the keyboard hint at the bottom. The command table remains the only source of rows.
    pub(super) fn command_palette(&mut self, ui: &mut Ui) {
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
    pub(super) fn shortcuts_sheet(&mut self, ui: &mut Ui) {
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
    pub(super) fn label_menu_window(&mut self, ui: &mut Ui) {
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
    pub(super) fn label_delete_dialog(&mut self, ui: &mut Ui) {
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
