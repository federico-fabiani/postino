//! The message list surface: the flat table, the conversation list, their rows and the
//! list-only helpers (columns, dates, accessible labels, context menus). The shell still owns
//! the state; this module owns the behaviour.

use super::*;

impl PostinoApp {
    pub(super) fn list_panel(&mut self, ui: &mut Ui) {
        if self.threads_view {
            self.thread_list_panel(ui);
            return;
        }
        let t = theme::tokens(self.dark);
        #[cfg(test)]
        probe::clear_bulk_bar(ui);
        #[cfg(test)]
        probe::clear_empty_state(ui);
        // The panel width left after the sidebar and the reading pane. It includes the list's own
        // margin; the columns are laid out against the content width inside the panel.
        let list_panel_width = ui.available_width();
        let _ = list_panel_width;
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
        let searching = !self.search_committed.trim().is_empty();
        let saved_account_wide = self.saved_folder_scope && !self.saved_query_folder;
        let search_error = self.search_error.clone();
        let search_capped = self.search_capped;
        let index_progress = self.index_progress.clone();
        let multi = self.selection.len() > 1;
        let mut clear_filter = false;
        // The header always names the scope, so the label counts cannot read as folder-local.
        let scope = if saved_account_wide {
            Some("in tutte le mail".to_owned())
        } else {
            label_filter.as_ref().map(|(_, _, _)| match &narrowed_folder_name {
                Some(name) => format!("in {name}"),
                None => "in tutte le cartelle".to_owned(),
            })
        };
        let meta = if searching {
            search_meta(total, search_capped, scope.as_deref())
        } else {
            match &scope {
                Some(scope) => format!("{} · {} · {scope}", unread_text(unread), messages_text(total)),
                None => format!("{} · {}", unread_text(unread), messages_text(total)),
            }
        };
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL, inset::LG))).show(ui, |ui| {
            if matches!(self.sync_view, SyncView::Offline { .. }) {
                self.offline_banner(ui, &t);
                ui.add_space(space::SM);
            }
            if multi {
                self.bulk_bar(ui, &t);
            } else {
                // The action is reserved first so it can never be clipped; the title and the scope
                // share the remaining width on two lines. At 1280 with the reading pane open the
                // list is about 375 px wide and the scope would not fit beside the title.
                let title = if saved_account_wide {
                    "Tutte le mail".to_owned()
                } else {
                    label_filter.as_ref().map_or(current_folder_name.clone(), |(_, name, _)| name.clone())
                };
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
                    ui.label(typography::text(Role::Meta, format!("Risultati per “{}”", self.search_committed), t.text_muted));
                    if Button::ghost("Cancella").size(ButtonSize::Sm).show(ui).clicked() {
                        self.clear_search();
                    }
                });
                let chips = search_chips(&self.search_committed);
                if !chips.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for chip in chips.into_iter().take(4) {
                            if chip.starts_with("NOT ") {
                                ui.label(typography::text(Role::Caption, chip, t.text_muted));
                            } else if Chip::new(chip.as_str()).show(ui).clicked() {
                                self.query = chip;
                                self.recording_recent = true;
                                self.commit_search();
                            }
                        }
                    });
                }
                ui.label(typography::text(
                    Role::Caption,
                    "from: · to: · subject: · label: · folder: · is: · has: · after: · larger: — AND/OR/NOT, \"frase\", prefisso*",
                    t.text_faint,
                ));
            }
            if let Some((code, position)) = &search_error {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, search_error_text(code, *position), t.danger));
            }
            if self.saved_index_pending {
                ui.add_space(space::XS);
                ui.label(typography::text(
                    Role::Meta,
                    "Indice di ricerca in preparazione: la ricerca salvata sarà disponibile tra poco.",
                    t.text_muted,
                ));
            } else if self.saved_error.is_some() {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, "Ricerca salvata non valida: correggila o eliminala.", t.danger));
            }
            if let Some(index) = &index_progress {
                ui.add_space(space::XS);
                let line = index.last_error.as_deref().map_or_else(
                    || format!("Indice di ricerca in preparazione ({}/{})", index.indexed, index.total),
                    |error| format!("Indice di ricerca non disponibile: {error}"),
                );
                ui.label(typography::text(Role::Meta, line, if index.last_error.is_some() { t.danger } else { t.text_muted }));
            }
            if searching
                && rows.len() as u32 >= self.search_limit
                && total > rows.len()
                && Button::ghost("Mostra altri").size(ButtonSize::Sm).show(ui).clicked()
            {
                self.search_limit = self.search_limit.saturating_add(200).min(5_000);
                self.list_cache = None;
            }
            ui.add_space(space::MD);
            if rows.is_empty() && search_error.is_none() && index_progress.is_none() {
                let mut menu_action = None;
                let background = ui.interact(ui.available_rect_before_wrap(), ui.id().with("empty-list-menu"), Sense::click());
                let has_label_filter = label_filter.is_some();
                let sync_ready = self.sync_ready();
                let sync_reason = if self.demo { "La modalità demo non ha un server" } else { "Nessun account da sincronizzare" };
                background.context_menu(|ui| {
                    let items = [
                        menu_item("label_new"),
                        menu_item("label_clear_filter").enabled(has_label_filter).reason("Nessun filtro etichetta attivo"),
                        MenuItem::separator(),
                        MenuItem::new(command_label("sync", "Aggiorna la posta")).icon(icons::SYNC).enabled(sync_ready).reason(sync_reason),
                        menu_item("select_all").enabled(false).reason("Non ci sono messaggi da selezionare"),
                        menu_item("compose"),
                        MenuItem::new("Cancella ricerca").icon(icons::CLOSE).enabled(searching).reason("Nessuna ricerca attiva"),
                    ];
                    match Menu::new(&items).show(ui) {
                        Some(0) => {
                            menu_action = Some(EmptyMenuAction::NewLabel);
                            ui.close();
                        }
                        Some(1) => {
                            menu_action = Some(EmptyMenuAction::ClearLabel);
                            ui.close();
                        }
                        Some(3) if sync_ready => {
                            menu_action = Some(EmptyMenuAction::Sync);
                            ui.close();
                        }
                        Some(5) => {
                            menu_action = Some(EmptyMenuAction::Compose);
                            ui.close();
                        }
                        Some(6) => {
                            menu_action = Some(EmptyMenuAction::ClearSearch);
                            ui.close();
                        }
                        _ => {}
                    }
                });
                let label_name = label_filter.as_ref().map(|(_, name, _)| name.as_str());
                let view = self.empty_view(label_name, scope.as_deref());
                let mut state = EmptyState::new(view.icon, &view.title, &view.body);
                if let Some((label, _)) = view.primary {
                    state = state.action(label);
                }
                if let Some((label, _)) = view.secondary {
                    state = state.secondary(label);
                }
                #[cfg(test)]
                probe::empty_state(ui, &view.title, &view.body, view.primary.map(|(label, _)| label), view.secondary.map(|(label, _)| label));
                let response = state.show(ui);
                let mut action = None;
                if response.primary_clicked() {
                    action = view.primary.map(|(_, action)| action);
                } else if response.secondary_clicked() {
                    action = view.secondary.map(|(_, action)| action);
                }
                if let Some(action) = action {
                    self.apply_empty_action(action);
                }
                match menu_action {
                    Some(EmptyMenuAction::NewLabel) => self.start_label_create(),
                    Some(EmptyMenuAction::ClearLabel) => self.apply_empty_action(EmptyAction::ClearLabel),
                    Some(EmptyMenuAction::Compose) => self.compose_new(),
                    Some(EmptyMenuAction::ClearSearch) => self.clear_search(),
                    Some(EmptyMenuAction::Sync) => self.request_sync(),
                    None => {}
                }
                return;
            }

            let mut clicked = None;
            let mut double_clicked = None;
            let mut star_clicked = None;
            let mut context_clicked = None;
            let mut context_action = None;
            let modifiers = ui.input(|input| input.modifiers);
            let content_width = ui.available_width();
            // The date rule is measured on the list panel, before its margins: the plan names the
            // 499 pt list at 1280×720, of which ~467 pt is content.
            let columns = list_columns(list_panel_width);
            let show_avatars = show_list_avatars(content_width);
            #[cfg(test)]
            probe::list_width(ui, content_width, list_panel_width);
            #[cfg(test)]
            probe::date_column(ui, columns.date.is_some());
            #[cfg(test)]
            probe::clear_tooltips(ui);
            #[cfg(test)]
            probe::clear_message_rows(ui);
            #[cfg(test)]
            probe::clear_row_facts(ui);
            #[cfg(test)]
            probe::reset_subject_width(ui);
            self.row_ids.clear();
            let ctx = ui.ctx().clone();
            let _ = &ctx;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut focus_rects = Vec::new();
                // The table inserts `item_spacing.x` around every column; at five columns that is
                // 40 pt the subject cannot spare. The cells carry their own insets instead.
                ui.spacing_mut().item_spacing.x = 0.0;
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
                table = table.column(egui_extras::Column::exact(columns.sender));
                table = table.column(egui_extras::Column::remainder());
                if let Some(date) = columns.date {
                    table = table.column(egui_extras::Column::exact(date));
                }
                table
                    .column(egui_extras::Column::exact(28.0))
                    .header(24.0, |mut header| {
                        header.col(|_| {});
                        if show_avatars {
                            header.col(|_| {});
                        }
                        header.col(|ui| {
                            ui.add_space(space::XS);
                            ui.add(egui::Label::new(typography::text(Role::Caption, "MITTENTE", t.text_faint)).selectable(false));
                        });
                        header.col(|ui| {
                            ui.add_space(space::XS);
                            ui.add(egui::Label::new(typography::text(Role::Caption, "OGGETTO", t.text_faint)).selectable(false));
                        });
                        // The compact date sits between the subject and the star, as in every mail
                        // client; the header drops it with the column instead of leaving a hole.
                        if columns.date.is_some() {
                            header.col(|ui| {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.add(egui::Label::new(typography::text(Role::Caption, "DATA", t.text_faint)).selectable(false));
                                });
                            });
                        }
                        header.col(|_| {});
                    })
                    .body(|mut body| {
                        for message in &rows {
                            let focused = self.selected.as_deref() == Some(message.id.as_str());
                            let selected = self.selection.contains(&message.id);
                            let foreground = if selected { t.selected_text } else { t.text };
                            let row_font =
                                FontId::new(type_size::BODY_SMALL, if message.unread { theme::medium() } else { FontFamily::Proportional });
                            let mut sender_truncated = false;
                            let mut subject_truncated = false;
                            let date_label = format_when(message.date, now_utc);
                            let accessible = row_accessible_label(message, &date_label, selected);
                            body.row(ui_size::ROW, |mut row| {
                                row.set_selected(selected);
                                row.col(|ui| {
                                    if message.unread {
                                        // Gold ink on ivory; the light gold returns on the forest
                                        // selection where it measures 6.5:1.
                                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                        ui.painter().circle_filled(rect.center(), 3.0, if selected { t.gold } else { t.gold_ink });
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
                                    ui.add_space(space::XS);
                                    sender_truncated = !text_fits(ui, name, &row_font, ui.available_width());
                                    let text = RichText::new(name).font(row_font.clone()).color(foreground);
                                    ui.add(egui::Label::new(text).truncate().selectable(false));
                                });
                                row.col(|ui| {
                                    // The cell layout starts content at the top. Centre the two lines
                                    // explicitly so the subject never touches the selected row edge.
                                    let top = ((ui.available_height() - 28.0) * 0.5).max(0.0);
                                    ui.add_space(top);
                                    ui.add_space(space::XS);
                                    #[cfg(test)]
                                    probe::subject_width(ui, ui.available_width());
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        let subject_text =
                                            if message.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.subject.as_str() };
                                        subject_truncated = !text_fits(ui, subject_text, &row_font, ui.available_width());
                                        let subject = RichText::new(subject_text).font(row_font.clone()).color(foreground);
                                        ui.add(egui::Label::new(subject).truncate().selectable(false));
                                        let snippet = RichText::new(message.snippet.as_str()).size(type_size::META).color(if selected {
                                            t.selected_muted
                                        } else {
                                            t.text_muted
                                        });
                                        ui.add(egui::Label::new(snippet).truncate().selectable(false));
                                    });
                                });
                                if columns.date.is_some() {
                                    row.col(|ui| {
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            ui.add(
                                                egui::Label::new(typography::mono(
                                                    date_label.as_str(),
                                                    type_size::CAPTION,
                                                    if selected { t.selected_muted } else { t.text_faint },
                                                ))
                                                .selectable(false),
                                            );
                                        });
                                    });
                                }
                                row.col(|ui| {
                                    let star_label = if message.starred { "Togli la stella" } else { "Segna con stella" };
                                    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                                    if response.hovered() {
                                        let fill = if selected { tokens::mix(t.selected, t.selected_text, 0.12) } else { t.hover };
                                        ui.painter().rect_filled(rect, CornerRadius::same(radius::SM), fill);
                                    }
                                    if response.has_focus() {
                                        tokens::paint_focus_ring_on(ui, rect, radius::SM, &t, selected);
                                    }
                                    let star_color = if message.starred {
                                        if selected { t.gold } else { t.gold_ink }
                                    } else if selected {
                                        t.selected_muted
                                    } else {
                                        t.text_faint
                                    };
                                    let font = if message.starred { icons::font_filled(13.0) } else { icons::font(13.0) };
                                    let glyph = if message.starred { icons::STAR_FILL } else { icons::STAR };
                                    let galley = ui.painter().layout_no_wrap(glyph.to_owned(), font, star_color);
                                    ui.painter().galley(
                                        pos2(rect.center().x - galley.size().x * 0.5, rect.center().y - galley.size().y * 0.5),
                                        galley,
                                        Color32::PLACEHOLDER,
                                    );
                                    if response.clicked()
                                        || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter) || input.key_pressed(Key::Space)))
                                    {
                                        star_clicked = Some(message.id.clone());
                                    }
                                    if response.hovered() {
                                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                    }
                                    #[cfg(test)]
                                    probe::star(ui, &message.id, rect);
                                    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, star_label));
                                    response.on_hover_text(star_label);
                                });
                                let response = row.response();
                                self.row_ids.insert(message.id.clone(), response.id);
                                // The table reports its row rect from the measuring pass, which can be
                                // wider than the list. Clamp it to the visible content for the focus
                                // ring and for the pointer tests.
                                let row_rect = Rect::from_min_max(
                                    response.rect.min,
                                    pos2(response.rect.max.x.min(response.rect.min.x + content_width), response.rect.max.y),
                                );
                                #[cfg(test)]
                                probe::message_row(&ctx, &message.id, row_rect);
                                #[cfg(test)]
                                probe::row_facts(&ctx, &message.id, &accessible, response.has_focus());
                                // Accessibility: every row is a named button. The label carries all
                                // the facts a sighted user reads from the columns plus the Enter
                                // affordance, so a screen reader is not left with "message row".
                                response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, accessible.clone()));
                                if focused && self.scroll_to_selected {
                                    response.scroll_to_me(None);
                                }
                                if response.has_focus() {
                                    // The ring follows real keyboard focus, not the selection: a
                                    // selected-but-unfocused row must not look focused, and Tab can
                                    // land on the list without changing what is selected.
                                    focus_rects.push((row_rect.shrink(1.0), selected));
                                    #[cfg(test)]
                                    probe::focus_ring(&ctx, &message.id);
                                }
                                if response.secondary_clicked() {
                                    context_clicked = Some(message.id.clone());
                                }
                                if response.clicked() {
                                    clicked = Some((message.id.clone(), modifiers));
                                }
                                if double_activated(&response) {
                                    double_clicked = Some(message.id.clone());
                                }
                                let mut tooltip = String::new();
                                if sender_truncated {
                                    let name =
                                        if message.from.name.trim().is_empty() { message.from.email.as_str() } else { message.from.name.as_str() };
                                    tooltip.push_str(name);
                                }
                                if subject_truncated {
                                    if !tooltip.is_empty() {
                                        tooltip.push('\n');
                                    }
                                    tooltip.push_str(if message.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.subject.as_str() });
                                }
                                if !tooltip.is_empty() {
                                    // A tooltip on hover has the usual delay; a keyboard-focused row
                                    // shows it at once, so truncation is never information loss
                                    // without a mouse.
                                    if response.has_focus() && !response.hovered() {
                                        #[cfg(test)]
                                        probe::tooltip(&ctx, &tooltip);
                                        let _ = egui::Tooltip::for_widget(&response).show(|ui| {
                                            ui.label(typography::text(Role::BodySmall, tooltip.as_str(), t.text));
                                        });
                                    }
                                    response.clone().on_hover_text(tooltip);
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
                for (rect, on_selected) in focus_rects {
                    crate::ui::tokens::paint_focus_ring_on(ui, rect, radius::CONTROL, &t, on_selected);
                }
            });
            if self.focus_selected_row {
                if let Some(id) = self.selected.as_ref().and_then(|selected| self.row_ids.get(selected)) {
                    ui.ctx().memory_mut(|memory| memory.request_focus(*id));
                    self.focus_selected_row = false;
                } else if self.selected.is_none() {
                    self.focus_selected_row = false;
                }
            }
            self.scroll_to_selected = false;
            if let Some(id) = star_clicked {
                self.toggle_star_for(&id);
            } else {
                if let Some(id) = context_clicked
                    && !self.selection.contains(&id)
                {
                    self.focus_message(id);
                }
                let overlay = reading_overlay(ui.ctx().content_rect().width());
                if let Some(id) = double_clicked {
                    // A double-click is the physical "open and expand": on a wide window the pane
                    // grows to the full content area; on a narrow one the overlay is already full.
                    self.open_message(id);
                    self.reading_full = !overlay;
                } else if let Some((id, modifiers)) = clicked {
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

    /// The conversation list. It is a separate surface from the flat message table: fewer columns,
    /// one row per thread, and every action goes through the thread operations or `message_move`.
    fn thread_list_panel(&mut self, ui: &mut Ui) {
        let t = theme::tokens(self.dark);
        let current_folder_name =
            self.engine.folders().iter().find(|folder| folder.id == self.folder).map_or_else(|| "Posta".to_owned(), |folder| folder.name.clone());
        let label_filter = self
            .label_filter
            .as_deref()
            .and_then(|id| self.label_summary(id))
            .map(|label| (label.id.clone(), label.name.clone(), label_color(label.color, &t)));
        let (threads, total, unread_threads, unread_messages) = self.visible_threads();
        let searching = !self.search_committed.trim().is_empty();
        let saved_account_wide = self.saved_folder_scope && !self.saved_query_folder;
        let search_error = self.search_error.clone();
        let saved_error = self.saved_error.clone();
        let saved_pending = self.saved_index_pending;
        let index_progress = self.index_progress.clone();
        let mut clear_filter = false;
        let title = if saved_account_wide {
            "Tutte le mail".to_owned()
        } else {
            label_filter.as_ref().map_or(current_folder_name.clone(), |(_, name, _)| name.clone())
        };
        let meta = if searching {
            format!(
                "{} nei risultati · {} nei risultati",
                plural(usize::try_from(total).unwrap_or(usize::MAX), "conversazione", "conversazioni"),
                plural(usize::try_from(unread_messages).unwrap_or(usize::MAX), "messaggio non letto", "messaggi non letti")
            )
        } else {
            format!(
                "{} con messaggi non letti · {}",
                plural(usize::try_from(unread_threads).unwrap_or(usize::MAX), "conversazione", "conversazioni"),
                plural(usize::try_from(total).unwrap_or(usize::MAX), "conversazione", "conversazioni")
            )
        };
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL, inset::LG))).show(ui, |ui| {
            if matches!(self.sync_view, SyncView::Offline { .. }) {
                self.offline_banner(ui, &t);
                ui.add_space(space::SM);
            }
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
                        ui.add(egui::Label::new(typography::text(Role::Meta, meta.as_str(), t.text_muted)).truncate().selectable(false))
                            .on_hover_text(meta.as_str());
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
                if Chip::new("Messaggi").icon(icons::LIST).show(ui).clicked() {
                    self.toggle_threads_view();
                }
            });
            if searching {
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    ui.label(typography::text(Role::Meta, format!("Risultati per “{}”", self.search_committed), t.text_muted));
                    if Button::ghost("Cancella").size(ButtonSize::Sm).show(ui).clicked() {
                        self.cancel_search();
                    }
                });
                let chips = search_chips(&self.search_committed);
                if !chips.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for chip in chips.into_iter().take(4) {
                            if chip.starts_with("NOT ") {
                                ui.label(typography::text(Role::Caption, chip, t.text_muted));
                            } else if Chip::new(chip.as_str()).show(ui).clicked() {
                                self.query = chip;
                                self.recording_recent = true;
                                self.commit_search();
                            }
                        }
                    });
                }
            }
            if let Some((code, position)) = &search_error {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, search_error_text(code, *position), t.danger));
            }
            if saved_pending {
                ui.add_space(space::XS);
                ui.label(typography::text(
                    Role::Meta,
                    "Indice di ricerca in preparazione: la ricerca salvata sarà disponibile tra poco.",
                    t.text_muted,
                ));
            } else if let Some(_error) = &saved_error {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Meta, "Ricerca salvata non valida: correggila o eliminala.", t.danger));
            }
            if let Some(index) = &index_progress {
                ui.add_space(space::XS);
                let line = index.last_error.as_deref().map_or_else(
                    || format!("Indice di ricerca in preparazione ({}/{})", index.indexed, index.total),
                    |error| format!("Indice di ricerca non disponibile: {error}"),
                );
                ui.label(typography::text(Role::Meta, line, if index.last_error.is_some() { t.danger } else { t.text_muted }));
            }
            if self.query.trim().is_empty() && self.search_focus && !self.recent_searches.is_empty() {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, "RICERCHE RECENTI", t.text_faint));
                ui.horizontal_wrapped(|ui| {
                    let recent = self.recent_searches.clone();
                    for query in recent {
                        if Chip::new(query.as_str()).show(ui).clicked() {
                            self.query = query;
                            self.recording_recent = true;
                            self.commit_search();
                        }
                    }
                });
            }
            if !searching
                && usize::try_from(total).unwrap_or(usize::MAX) > threads.len()
                && Button::ghost("Carica altre conversazioni").size(ButtonSize::Sm).show(ui).clicked()
            {
                self.thread_list_limit = self.thread_list_limit.saturating_add(200).min(5_000);
                self.threads_cache = None;
                self.list_cache = None;
            }
            ui.add_space(space::MD);
            if threads.is_empty() && search_error.is_none() {
                let label_name = label_filter.as_ref().map(|(_, name, _)| name.as_str());
                let scope = label_filter
                    .as_ref()
                    .map(|_| if saved_account_wide { "in tutte le mail".to_owned() } else { format!("in {current_folder_name}") });
                let view = self.empty_view_in(label_name, scope.as_deref(), true);
                let mut state = EmptyState::new(view.icon, &view.title, &view.body);
                if let Some((label, _)) = view.primary {
                    state = state.action(label);
                }
                if let Some((label, _)) = view.secondary {
                    state = state.secondary(label);
                }
                #[cfg(test)]
                probe::empty_state(ui, &view.title, &view.body, view.primary.map(|(label, _)| label), view.secondary.map(|(label, _)| label));
                let response = state.show(ui);
                let action = if response.primary_clicked() {
                    view.primary.map(|(_, action)| action)
                } else if response.secondary_clicked() {
                    view.secondary.map(|(_, action)| action)
                } else {
                    None
                };
                if let Some(action) = action {
                    self.apply_empty_action(action);
                }
                return;
            }
            let mut action: Option<ThreadRowAction> = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for thread in &threads {
                    let selected = self.selected_thread.as_deref() == Some(thread.id.as_str());
                    let participants = {
                        let mut names = thread
                            .participants
                            .iter()
                            .take(2)
                            .map(|address| if address.name.trim().is_empty() { address.email.clone() } else { address.name.clone() })
                            .collect::<Vec<_>>()
                            .join(", ");
                        if thread.participants.len() > 2 {
                            names.push('…');
                        }
                        names
                    };
                    let meta = format_when(thread.last_date, self.now_utc);
                    let counts = messages_text(usize::try_from(thread.message_count).unwrap_or(usize::MAX));
                    let unread_detail = if thread.unread_count > 0 {
                        format!(" · {}", unread_text(usize::try_from(thread.unread_count).unwrap_or(usize::MAX)))
                    } else {
                        String::new()
                    };
                    let subtitle = format!("{participants} · {counts}{unread_detail}");
                    let subject = thread_subject(&thread.subject);
                    let subject = if subject.trim().is_empty() { "(Nessun oggetto)" } else { subject.as_str() };
                    let starred = thread.starred_count > 0;
                    let row =
                        ListRow::thread(subject, &subtitle, &meta).unread(thread.unread_count > 0).starred(starred).selected(selected).height(48.0);
                    let response = row.show(ui);
                    #[cfg(test)]
                    probe::row(ui, &format!("thread:{}", thread.id), response.rect);
                    #[cfg(test)]
                    probe::thread_star(ui, &thread.id, starred, response.rect);
                    let mut accessible = format!("{subject}. {subtitle}. {meta}.");
                    if thread.unread_count > 0 {
                        accessible.push_str(" Non letta.");
                    }
                    if starred {
                        accessible.push_str(" Con stella.");
                    }
                    if selected {
                        accessible.push_str(" Selezionata.");
                    }
                    accessible.push_str(" Premi Invio per aprire.");
                    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, accessible.clone()));
                    response.clone().on_hover_text(accessible);
                    if response.clicked() || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
                        action = Some(ThreadRowAction::Open(thread.id.clone()));
                    }
                    response.context_menu(|ui| {
                        let (star_id, _) = thread_star_menu(starred);
                        let items = [
                            menu_item("open"),
                            MenuItem::separator(),
                            menu_item("archive"),
                            menu_item("delete").danger(true),
                            MenuItem::separator(),
                            menu_item("read"),
                            menu_item("unread"),
                            menu_item(star_id),
                        ];
                        match Menu::new(&items).show(ui) {
                            Some(0) => {
                                action = Some(ThreadRowAction::Open(thread.id.clone()));
                                ui.close();
                            }
                            Some(2) => {
                                action = Some(ThreadRowAction::Archive(thread.id.clone()));
                                ui.close();
                            }
                            Some(3) => {
                                action = Some(ThreadRowAction::Delete(thread.id.clone()));
                                ui.close();
                            }
                            Some(5) => {
                                action = Some(ThreadRowAction::Flag(thread.id.clone(), Some(false), None));
                                ui.close();
                            }
                            Some(6) => {
                                action = Some(ThreadRowAction::Flag(thread.id.clone(), Some(true), None));
                                ui.close();
                            }
                            Some(7) => {
                                let (_, next) = thread_star_menu(starred);
                                action = Some(ThreadRowAction::Flag(thread.id.clone(), None, Some(next)));
                                ui.close();
                            }
                            _ => {}
                        }
                    });
                }
            });
            if let Some(action) = action {
                match action {
                    ThreadRowAction::Open(id) => self.select_thread(id),
                    ThreadRowAction::Archive(id) => {
                        if self.selected_thread.as_deref() != Some(id.as_str()) {
                            self.select_thread(id);
                        }
                        self.move_selected(FolderKind::Archive, "Conversazione archiviata", "Conversazioni archiviate");
                    }
                    ThreadRowAction::Delete(id) => {
                        if self.selected_thread.as_deref() != Some(id.as_str()) {
                            self.select_thread(id);
                        }
                        self.move_selected(FolderKind::Trash, "Conversazione spostata nel cestino", "Conversazioni spostate nel cestino");
                    }
                    ThreadRowAction::Flag(id, unread, starred) => {
                        if self.selected_thread.as_deref() != Some(id.as_str()) {
                            self.load_thread_page(id, false, None);
                        }
                        self.set_thread_flag(unread, starred);
                    }
                }
            }
        });
        if clear_filter {
            self.clear_label_filter();
        }
    }
}

#[derive(Clone, Copy)]
enum EmptyMenuAction {
    NewLabel,
    ClearLabel,
    Compose,
    ClearSearch,
    Sync,
}

/// One action chosen from a conversation row (click or context menu).
enum ThreadRowAction {
    Open(String),
    Archive(String),
    Delete(String),
    Flag(String, Option<bool>, Option<bool>),
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

/// Up to eight leaf clauses of a parsed search query, in source order, for the chips under the
/// search field. A click replaces the query with that clause. An unparsable query has no chips; the
/// inline error explains why.
pub(super) fn search_chips(query: &str) -> Vec<String> {
    let Ok(parsed) = postino_core::parse_search(query) else { return Vec::new() };
    fn collect(expr: &postino_core::SearchExpr, out: &mut Vec<String>) {
        match expr {
            postino_core::SearchExpr::And(children) | postino_core::SearchExpr::Or(children) => {
                for child in children {
                    if out.len() < 8 {
                        collect(child, out);
                    }
                }
            }
            postino_core::SearchExpr::Not(child) => {
                if out.len() < 8 {
                    out.push(format!("NOT ({})", child.canonical()));
                }
            }
            leaf => {
                if out.len() < 8 {
                    out.push(leaf.canonical());
                }
            }
        }
    }
    let mut chips = Vec::new();
    collect(&parsed.expr, &mut chips);
    chips
}

/// The star command a conversation row exposes. A starred thread removes the star; an unstarred
/// one adds it, so the label and the action can never disagree.
pub(super) fn thread_star_menu(starred: bool) -> (&'static str, bool) {
    if starred { ("unstar", false) } else { ("star", true) }
}

pub(super) fn search_meta(total: usize, capped: bool, scope: Option<&str>) -> String {
    let total_text = if capped { "primi 10.000 risultati".to_owned() } else { messages_text(total) };
    match scope {
        Some(scope) => format!("{total_text} · {scope}"),
        None => total_text,
    }
}

/// The Italian sentence for a search parse error. The code is stable English; the position is the
/// byte offset in the query, so the field can point at the exact token.
pub(super) fn search_error_text(code: &str, position: usize) -> String {
    let reason = match code {
        "empty" => "la ricerca è vuota",
        "too_long" => "la ricerca supera i 1024 byte",
        "too_many_clauses" => "la ricerca ha più di 64 clausole",
        "too_deep" => "la ricerca ha più di 8 parentesi annidate",
        "too_many_terms" => "la ricerca ha più di 16 termini",
        "control" => "la ricerca contiene un carattere di controllo",
        "unclosed_quote" => "una virgoletta non è chiusa",
        "unclosed_paren" => "una parentesi non è chiusa",
        "unexpected_token" => "c'è un token inatteso",
        "dangling_operator" => "AND o OR non hanno un operando",
        "dangling_field" => "un campo non ha un valore",
        "negative_only" => "NOT richiede almeno un termine positivo",
        "bad_date" => "la data deve essere un giorno YYYY-MM-DD valido",
        "bad_size" => "la dimensione deve essere un numero con k, m o g",
        "bad_is" => "is: accetta unread, read, starred o unstarred",
        "bad_has" => "has: accetta attachment",
        _ => "la ricerca non è valida",
    };
    format!("Ricerca non valida: {reason} (posizione {position}). Correggi il testo o premi Esc per cancellarla.")
}

/// The columns of the message table at a given *list panel* width (before the 16 pt side margins).
/// The compact date is dropped only below 480 pt; the sender narrows below 560 pt so the subject
/// keeps its 260 pt floor at the 499 pt list the 1280×720 reading split leaves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ListColumns {
    pub(super) sender: f32,
    pub(super) date: Option<f32>,
}

pub(super) fn list_columns(list_width: f32) -> ListColumns {
    let sender = if list_width < ui_size::LIST_DATE_MIN + 80.0 { ui_size::LIST_SENDER_NARROW } else { ui_size::LIST_SENDER };
    if list_width < ui_size::LIST_DATE_MIN { ListColumns { sender, date: None } } else { ListColumns { sender, date: Some(ui_size::LIST_DATE) } }
}

/// The width the subject cell has at a given list panel width, with the fixed columns (unread
/// dot, sender, date, star) and the list margins removed. The table's column spacing is set to
/// zero, so it does not enter the sum. Kept next to [`list_columns`] so the 260 pt promise can be
/// tested without a window.
#[cfg(test)]
pub(super) fn subject_width(list_width: f32) -> f32 {
    let columns = list_columns(list_width);
    // The dot, the sender, the 52 pt date and the 28 pt star, plus the table gutter and the 4 pt
    // inset in front of the subject text.
    let fixed = 12.0 + columns.sender + columns.date.unwrap_or(0.0) + 28.0 + TABLE_GUTTER + 4.0;
    (list_width - 2.0 * space::XL - fixed).max(0.0)
}

/// The table's own vertical scroll gutter, measured on the real layout. It is taken out of the
/// subject so the 260 pt promise is about the text that is actually drawn, not the column label.
#[cfg(test)]
const TABLE_GUTTER: f32 = 8.0;

/// Would `text` have to be truncated to fit `width` at this font? Used to attach a tooltip only
/// when something is actually cut off.
fn text_fits(ui: &Ui, text: &str, font: &FontId, width: f32) -> bool {
    ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x <= width
}

pub(super) fn show_list_avatars(list_width: f32) -> bool {
    list_width >= AVATAR_MIN_LIST_WIDTH
}

/// What a screen reader hears for one message row. It repeats the visual facts — sender, subject,
/// unread, attachment, date, selection — and names the Enter affordance, so the row is usable
/// without seeing which column the pointer is over.
fn row_accessible_label(message: &MessageSummary, date: &str, selected: bool) -> String {
    let sender = if message.from.name.trim().is_empty() { message.from.email.as_str() } else { message.from.name.as_str() };
    let subject = if message.subject.trim().is_empty() { "(Nessun oggetto)" } else { message.subject.as_str() };
    let mut label = format!("{sender}. {subject}.");
    if message.unread {
        label.push_str(" Non letto.");
    }
    if message.has_attachments {
        label.push_str(" Con allegati.");
    }
    label.push(' ');
    label.push_str(date);
    label.push('.');
    if selected {
        label.push_str(" Selezionato.");
    }
    label.push_str(" Premi Invio per aprire.");
    label
}
