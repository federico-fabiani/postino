//! The shell itself: first frame, message list, selection, bulk actions and the palette.

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
    let id = app.engine.messages()[0].id.clone();
    app.open_message(id.clone());
    assert_eq!(app.selected.as_deref(), Some(id.as_str()));
    assert!(app.reading_open);
    assert!(!app.reading_full);
}

#[test]
fn focusing_does_not_open_the_reading_pane() {
    let mut app = PostinoApp::demo(true);
    let id = app.engine.messages()[0].id.clone();
    app.focus_message(id.clone());
    assert_eq!(app.selected.as_deref(), Some(id.as_str()));
    assert!(!app.reading_open);
}

#[test]
fn opening_an_unread_message_marks_it_read_after_the_delay_and_undoes() {
    let mut app = PostinoApp::demo(true);
    let id = app.engine.messages().iter().find(|message| message.unread).expect("an unread demo message").id.clone();
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
    let ids = app.engine.messages().iter().filter(|message| message.unread).take(2).map(|message| message.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    app.set_unread_many(&ids, false);
    assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| !message.summary.unread)));
    app.undo_last();
    assert!(ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.unread)), "one undo restores every flag change");
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
fn palette_explains_a_disabled_selection_and_enter_is_not_silent() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.show_palette = true;
    app.palette_query = "rinomina".to_owned();
    app.palette_selected = 0;
    frame(&ctx, &mut app, vec![]);
    let rename = command("label_rename").expect("label_rename is registered");
    assert!(!app.command_enabled(rename));
    assert_eq!(rename.rule.reason(), Some("Seleziona prima un'etichetta"), "the palette can paint a visible reason");

    // The line under the list is a real painted galley, not only a hover tooltip.
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))), ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    let painted = output.shapes.iter().any(|shape| match &shape.shape {
        egui::epaint::Shape::Text(text) => text.galley.text().contains("Seleziona prima un'etichetta"),
        _ => false,
    });
    output.textures_delta.clear();
    assert!(painted, "the disabled reason must be visible under the palette list");

    frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(app.show_palette, "Enter on a disabled command must not close the palette");
    assert!(app.status.contains("Seleziona prima un'etichetta"), "Enter must explain the refusal: {}", app.status);
}

#[test]
fn archiving_closes_the_pane_and_moves_the_message() {
    let mut app = PostinoApp::demo(true);
    let id = app.engine.messages()[0].id.clone();
    app.open_message(id.clone());
    app.move_selected(FolderKind::Archive, "archiviato", "archiviati");
    assert!(app.selected.is_none());
    assert!(!app.reading_open);
    assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.clone()), Some("archive".to_owned()));
}

#[test]
fn palette_filters_commands_and_keeps_every_row() {
    let all = filtered_commands("");
    assert!(all.len() > 4, "the palette should list more than the first command");
    let search = filtered_commands("stella");
    assert!(search.iter().any(|command| command.id.as_str() == "star"));
    assert!(search.iter().all(|command| command.label.to_lowercase().contains("stella")));
}

#[test]
fn the_command_table_is_exhaustive_and_unique() {
    // The table is the single source of the palette, the menus and the shortcut sheet, so a
    // missing, duplicated or half-registered command is a dead button. Pin all three.
    const ALL: &[CommandId] = &[
        CommandId::Compose,
        CommandId::Open,
        CommandId::Reply,
        CommandId::ReplyAll,
        CommandId::Forward,
        CommandId::InviteAccept,
        CommandId::InviteTentative,
        CommandId::InviteDecline,
        CommandId::EditDraft,
        CommandId::Send,
        CommandId::Outbox,
        CommandId::ExportEml,
        CommandId::ImportEml,
        CommandId::Expand,
        CommandId::Read,
        CommandId::Unread,
        CommandId::Star,
        CommandId::Unstar,
        CommandId::Archive,
        CommandId::Delete,
        CommandId::LabelMenu,
        CommandId::LabelNew,
        CommandId::LabelRename,
        CommandId::LabelDelete,
        CommandId::LabelClearFilter,
        CommandId::Inbox,
        CommandId::Starred,
        CommandId::Theme,
        CommandId::About,
        CommandId::Shortcuts,
        CommandId::SelectAll,
        CommandId::Toggle,
        CommandId::ClearSelection,
        CommandId::FocusSearch,
        CommandId::ThreadsToggle,
        CommandId::SavedSearchSave,
        CommandId::SavedSearchRename,
        CommandId::SavedSearchDelete,
        CommandId::Rules,
        CommandId::RulesNew,
        CommandId::SavedSearchClearRecent,
        CommandId::Next,
        CommandId::Previous,
        CommandId::ExtendNext,
        CommandId::ExtendPrevious,
        CommandId::Close,
        CommandId::Undo,
        CommandId::Palette,
        CommandId::Sync,
        CommandId::Settings,
        CommandId::AddAccount,
        CommandId::DaemonStart,
        CommandId::DaemonStop,
        CommandId::Quit,
        CommandId::DaemonStatus,
        CommandId::ReducedMotion,
        CommandId::MarkFolderRead,
        CommandId::Back,
    ];
    assert_eq!(ALL.len(), 58, "one variant per command");
    assert_eq!(COMMANDS.len(), ALL.len(), "one table entry per CommandId");

    let mut listed = Vec::new();
    for command in COMMANDS {
        assert!(!listed.contains(&command.id), "{:?} is listed twice", command.id);
        listed.push(command.id);
    }
    for id in ALL {
        assert!(listed.contains(id), "{id:?} has no COMMANDS entry");
    }

    // Star/unstar share S because the key toggles, and the three Escape commands belong to
    // different surfaces. Every other shortcut must name exactly one command.
    let mut shortcuts: std::collections::HashMap<&'static str, Vec<CommandId>> = std::collections::HashMap::new();
    for command in COMMANDS.iter().filter(|command| !command.shortcut.is_empty()) {
        shortcuts.entry(command.shortcut).or_default().push(command.id);
    }
    for (shortcut, ids) in shortcuts {
        let mut ids = ids;
        ids.sort_by_key(|id| id.as_str());
        let expected: &[CommandId] = match shortcut {
            "S" => &[CommandId::Star, CommandId::Unstar],
            "Esc" => &[CommandId::Back, CommandId::ClearSelection, CommandId::Close],
            _ => {
                assert_eq!(ids.len(), 1, "the shortcut {shortcut} is assigned to more than one command: {ids:?}");
                continue;
            }
        };
        assert_eq!(ids, expected, "the shared shortcut {shortcut} drifted to another command");
    }
}

#[test]
fn the_empty_state_names_each_filter_and_never_blames_the_folder() {
    let mut app = PostinoApp::demo(true);

    let inbox = app.empty_view(None, None);
    assert_eq!(inbox.title, "Nessun messaggio");
    assert_eq!(inbox.body, "La posta in arrivo è vuota.");
    assert!(inbox.primary.is_none(), "an honestly empty inbox needs no way out");

    app.unread_only = true;
    let unread = app.empty_view(None, None);
    assert_eq!(unread.title, "Nessun messaggio non letto");
    assert_eq!(unread.primary.map(|(label, _)| label), Some("Mostra tutti"));
    assert!(unread.body.contains("non letti"));
    assert!(!unread.body.contains("è vuota"), "a filter must never be reported as an empty folder");

    app.unread_only = false;
    app.attachments_only = true;
    let attachments = app.empty_view(None, None);
    assert_eq!(attachments.title, "Nessun messaggio con allegati");
    assert_eq!(attachments.primary.map(|(label, _)| label), Some("Mostra tutti"));
    assert!(!attachments.body.contains("è vuota"));

    app.attachments_only = false;
    app.select_label("lbl_1".to_owned());
    let label = app.empty_view(Some("Lavoro"), Some("in tutte le cartelle"));
    assert_eq!(label.title, "Nessun messaggio");
    assert_eq!(label.body, "Nessun messaggio con l'etichetta “Lavoro” in tutte le cartelle.");
    assert_eq!(label.primary.map(|(label, _)| label), Some("Togli filtro etichetta"));
    assert_eq!(label.secondary.map(|(label, _)| label), Some("Mostra tutti"));

    app.unread_only = true;
    let label_unread = app.empty_view(Some("Lavoro"), Some("in tutte le cartelle"));
    assert_eq!(label_unread.title, "Nessun messaggio non letto con l'etichetta “Lavoro”");
    assert_eq!(label_unread.primary.map(|(label, _)| label), Some("Togli filtro “non letti”"));
    assert!(!label_unread.body.contains("è vuota"));

    app.unread_only = false;
    app.query = "nessun-risultato".to_owned();
    let search = app.empty_view(None, None);
    assert_eq!(search.title, "Nessun risultato");
    assert_eq!(search.body, "Nessun messaggio corrisponde a “nessun-risultato” in tutte le cartelle.");
    assert_eq!(search.primary.map(|(label, _)| label), Some("Cancella ricerca"));
    let search_in_label = app.empty_view(Some("Lavoro"), Some("in tutte le cartelle"));
    assert!(search_in_label.body.contains("nell'etichetta “Lavoro”"), "{}", search_in_label.body);

    app.query.clear();
    app.clear_label_filter();
    app.select_folder("drafts");
    let drafts = app.empty_view(None, None);
    assert_eq!(drafts.title, "Nessuna bozza");
    assert_eq!(drafts.primary.map(|(label, _)| label), Some("Scrivi"));

    app.select_folder("archive");
    let folder = app.empty_view(None, None);
    assert_eq!(folder.body, "Questa cartella è vuota.");
    assert_eq!(folder.primary.map(|(label, _)| label), Some("Vai a Posta in arrivo"));
}

#[test]
fn the_three_filter_empty_states_render_and_offer_a_way_out() {
    let (ctx, _) = ready_app();
    let wide = Vec2::new(1280.0, 720.0);
    let cases = [
        (
            ScreenshotState::EmptyUnread,
            "Nessun messaggio non letto",
            "non ci sono messaggi non letti",
            Some("Mostra tutti"),
            None,
            EmptyAction::ShowAll,
        ),
        (
            ScreenshotState::EmptyAttachments,
            "Nessun messaggio con allegati",
            "non ci sono messaggi con allegati",
            Some("Mostra tutti"),
            None,
            EmptyAction::ShowAll,
        ),
        (
            ScreenshotState::EmptyLabelUnread,
            "Nessun messaggio non letto con l'etichetta “Lavoro”",
            "con l'etichetta “Lavoro”",
            Some("Togli filtro “non letti”"),
            None,
            EmptyAction::ClearUnread,
        ),
    ];
    for (state, title, body, primary, secondary, action) in cases {
        let mut app = PostinoApp::demo(true);
        app.seed_screenshot(state);
        frame_at(&ctx, &mut app, wide, vec![]);
        frame_at(&ctx, &mut app, wide, vec![]);
        let view = probe::empty_state_view(&ctx).expect("the empty state is painted");
        assert_eq!(view.title, title, "{state:?}");
        assert!(view.body.contains(body), "{state:?} body is {:?}", view.body);
        assert!(!view.body.contains("è vuota"), "{state:?} must not blame the folder");
        assert_eq!(view.primary.as_deref(), primary, "{state:?}");
        assert_eq!(view.secondary.as_deref(), secondary, "{state:?}");
        assert!(app.visible_messages().0.is_empty(), "{state:?} must really be empty");
        app.apply_empty_action(action);
        frame_at(&ctx, &mut app, wide, vec![]);
        assert!(!app.visible_messages().0.is_empty(), "{state:?} action must reveal mail");
        assert!(probe::empty_state_view(&ctx).is_none(), "{state:?} empty state must disappear");
    }
}

#[test]
fn rows_are_named_buttons_and_the_star_is_a_28_pt_control() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1280.0, 720.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    let (rows, _, _) = app.visible_messages();
    let unread = rows.iter().find(|message| message.unread).expect("a demo unread message");
    let label = probe::row_facts_for(&ctx, &unread.id).expect("the row registered its accessible name").0;
    let sender = if unread.from.name.trim().is_empty() { unread.from.email.as_str() } else { unread.from.name.as_str() };
    assert!(label.contains(sender), "the name names the sender: {label}");
    assert!(label.contains(unread.subject.as_str()), "the name names the subject: {label}");
    assert!(label.contains("Non letto"), "{label}");
    assert!(label.contains("Premi Invio per aprire"), "{label}");
    assert!(label.contains(&format_when(unread.date, app.now_utc)), "the name carries the date: {label}");
    if unread.has_attachments {
        assert!(label.contains("Con allegati"), "{label}");
    }

    let star = probe::star_rect(&ctx, &unread.id).expect("the star rectangle");
    assert!(star.width() >= 28.0 && star.height() >= 28.0, "the star target is {star:?}, smaller than 28×28");

    // A click on the star toggles it and never opens the message behind it.
    click_at(&ctx, &mut app, star.center(), wide);
    assert_eq!(app.engine.message(&unread.id).map(|message| message.summary.starred), Some(!unread.starred));
    assert!(!app.reading_open, "a star click must not open the message");

    // A near miss at the star's edge still lands in the target, not on the row.
    let corner = star.min + Vec2::new(1.0, 1.0);
    let starred = app.engine.message(&unread.id).is_some_and(|message| message.summary.starred);
    click_at(&ctx, &mut app, corner, wide);
    assert_eq!(app.engine.message(&unread.id).map(|message| message.summary.starred), Some(!starred));
    assert!(!app.reading_open);

    // A selected row says so.
    app.select_only(unread.id.clone());
    frame_at(&ctx, &mut app, wide, vec![]);
    let selected = probe::row_facts_for(&ctx, &unread.id).expect("the selected row").0;
    assert!(selected.contains("Selezionato."), "{selected}");
}

#[test]
fn the_focus_ring_follows_keyboard_focus_and_tab_reaches_the_list() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1280.0, 720.0);
    frame_at(&ctx, &mut app, wide, vec![]);

    // Selected but not keyboard-focused: no ring.
    let first = app.visible_messages().0[0].id.clone();
    app.select_only(first.clone());
    frame_at(&ctx, &mut app, wide, vec![]);
    assert!(probe::focused_rows(&ctx).is_empty(), "a programmatic selection must not paint a focus ring");
    assert!(probe::focus_rings(&ctx).is_empty(), "no row held keyboard focus");

    // J requests real keyboard focus on the row.
    app.clear_selection();
    frame_at(&ctx, &mut app, wide, vec![key(Key::J, Modifiers::NONE)]);
    frame_at(&ctx, &mut app, wide, vec![]);
    let focused = probe::focused_rows(&ctx);
    assert_eq!(focused.first().map(String::as_str), Some(first.as_str()), "J focuses the row: {focused:?}");
    assert!(probe::focus_rings(&ctx).contains(&first), "the focus ring is painted on the focused row");
    assert_eq!(ctx.memory(|memory| memory.focused()), app.row_ids.get(&first).copied(), "the row widget owns focus");

    // Tab from a fresh app eventually reaches the list without changing the selection.
    let (ctx, mut app) = ready_app();
    frame_at(&ctx, &mut app, wide, vec![]);
    let mut reached = false;
    for _ in 0..80 {
        frame_at(&ctx, &mut app, wide, vec![key(Key::Tab, Modifiers::NONE)]);
        if !probe::focused_rows(&ctx).is_empty() {
            reached = true;
            break;
        }
    }
    assert!(reached, "Tab never reached a message row");
}

#[test]
fn u_right_after_opening_cancels_the_auto_read() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1200.0, 800.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    let id = app.engine.messages().iter().find(|message| message.unread).map(|message| message.id.clone()).expect("an unread demo message");
    frame_time(&ctx, &mut app, wide, vec![], 10.0);
    app.open_message(id.clone());
    assert!(app.pending_read.is_some(), "opening starts the auto-read timer");
    frame_time(&ctx, &mut app, wide, vec![key(Key::U, Modifiers::NONE)], 10.01);
    assert!(app.pending_read.is_none(), "U cancels the pending auto-read");
    frame_time(&ctx, &mut app, wide, vec![], 12.0);
    app.maybe_complete_read();
    assert!(app.engine.message(&id).is_some_and(|message| message.summary.unread), "U must win over the timer");
}

#[test]
fn a_star_also_cancels_the_auto_read() {
    let mut app = PostinoApp::demo(true);
    let id = app.engine.messages().iter().find(|message| message.unread).map(|message| message.id.clone()).expect("an unread demo message");
    app.now = 5.0;
    app.open_message(id.clone());
    app.toggle_star_for(&id);
    assert!(app.pending_read.is_none(), "an explicit flag action cancels the auto-read");
    app.now = 6.0;
    app.maybe_complete_read();
    assert!(app.engine.message(&id).is_some_and(|message| message.summary.unread));
}

#[test]
fn plain_j_k_collapse_and_shift_extends_from_the_anchor() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1200.0, 800.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    let (rows, _, _) = app.visible_messages();
    app.select_only(rows[0].id.clone());
    frame_at(&ctx, &mut app, wide, vec![]);

    frame_at(&ctx, &mut app, wide, vec![egui::Event::ModifiersChanged(Modifiers::SHIFT), key(Key::J, Modifiers::SHIFT)]);
    assert_eq!(app.selection.len(), 2, "Shift+J extends from the anchor");
    assert_eq!(app.anchor.as_deref(), Some(rows[0].id.as_str()));
    let bulk = probe::bulk_bar_rect(&ctx).expect("the bulk bar is visible while the anchor is alive");
    assert!(bulk.height() > 0.0);

    frame_at(&ctx, &mut app, wide, vec![key(Key::J, Modifiers::SHIFT)]);
    assert_eq!(app.selection.len(), 3, "Shift+J extends the range from the anchor: {:?}", app.selection);
    frame_at(&ctx, &mut app, wide, vec![key(Key::K, Modifiers::SHIFT)]);
    assert_eq!(app.selection.len(), 2, "Shift+K contracts the range from the anchor");
    assert!(app.selection.contains(&rows[0].id) && app.selection.contains(&rows[1].id));

    // Plain J moves the focus and collapses to that row; the batch and the bulk bar go.
    frame_at(&ctx, &mut app, wide, vec![egui::Event::ModifiersChanged(Modifiers::NONE), key(Key::J, Modifiers::NONE)]);
    assert_eq!(app.selected.as_deref(), Some(rows[2].id.as_str()), "plain J moves the focus");
    assert_eq!(app.selection.len(), 1, "plain J collapses the selection");
    assert_eq!(app.anchor.as_deref(), Some(rows[2].id.as_str()), "the anchor follows the collapsed focus");
    assert!(
        probe::bulk_bar_rect(&ctx).is_none(),
        "the bulk bar disappears with the batch: selection {:?}, bulk {:?}",
        app.selection,
        probe::bulk_bar_rect(&ctx)
    );
    assert!(!app.reading_open);
}

#[test]
fn the_reading_pane_width_is_restored_from_settings() {
    let (ctx, mut app) = ready_app();
    let size = Vec2::new(1600.0, 900.0);
    app.reading_ratio = 0.55;
    let id = app.visible_messages().0[0].id.clone();
    app.open_message(id);
    for _ in 0..3 {
        frame_at(&ctx, &mut app, size, vec![]);
    }
    let (content, panel) = probe::list_widths(&ctx).expect("the list width");
    let remaining = size.x - ui_size::SIDEBAR;
    let pane = size.x - ui_size::SIDEBAR - panel;
    assert!((pane / remaining - 0.55).abs() < 0.02, "the stored 0.55 fraction opens the pane at {pane} of {remaining} ({content} list content)");
    assert!((app.reading_ratio - 0.55).abs() < 0.02, "the measured fraction is written back: {}", app.reading_ratio);
    assert!((app.settings_snapshot().reading_pane_ratio - app.reading_ratio).abs() < f32::EPSILON, "the snapshot would persist the ratio");
}

#[test]
fn the_reading_pane_clamps_a_hostile_fraction() {
    for ratio in [-1.0, 0.0, 0.99, f32::NAN] {
        let width = reading_pane_width_with_ratio(1600.0, ratio);
        let remaining = 1600.0 - ui_size::SIDEBAR;
        assert!(width >= READING_PANE_MIN_SIZE, "{ratio} produced {width}");
        assert!(1600.0 - ui_size::SIDEBAR - width >= ui_size::LIST_MIN, "{ratio} squeezed the list: {width}");
        assert!(width <= remaining * READING_PANE_MAX_RATIO + 0.01, "{ratio} produced an oversized pane: {width}");
    }
}

#[test]
fn a_partial_batch_move_still_offers_undo_for_the_moved_messages() {
    let mut app = PostinoApp::demo(true);
    app.threads_view = false;
    app.move_many(&["msg_001".to_owned(), "missing".to_owned()], "archive", "Messaggio archiviato", "Messaggi archiviati");
    assert_eq!(app.engine.message("msg_001").map(|message| message.summary.folder_id), Some("archive".to_owned()));
    assert!(app.failure.is_some(), "the missing row is reported");
    assert!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some(), "the moved message is undoable");
    app.undo_last();
    assert_eq!(app.engine.message("msg_001").map(|message| message.summary.folder_id), Some("inbox".to_owned()));
}
