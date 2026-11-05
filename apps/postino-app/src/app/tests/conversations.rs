//! The conversation (thread) list and its triage actions.

use super::*;

#[test]
fn conversation_rows_are_a_real_surface_and_the_toggle_returns_to_messages() {
    let (ctx, mut app) = conversations_app();
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let rows = probe::rows(&ctx);
    assert!(rows.contains_key("thread:thr_preventivo"), "the conversation row was not painted: {:?}", rows.keys().collect::<Vec<_>>());
    assert!(!rows.keys().any(|key| key.starts_with("msg_")), "the flat rows must not be painted in the threaded view");
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Preventivo")), "the thread subject is missing: {text:?}");
    assert!(text.iter().any(|line| line.contains("3 messaggi")), "the thread count is missing: {text:?}");

    app.toggle_threads_view();
    assert!(!app.threads_view);
    assert!(app.selected_thread.is_none());
    app.close_reading();
    assert!(!app.reading_open && !app.reading_full);
    assert!(!app.visible_messages().0.is_empty(), "the flat view has no messages");
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let flat = probe::message_rows(&ctx);
    assert!(flat.keys().any(|key| key.starts_with("msg_")), "the flat message table did not come back: {:?}", flat.keys().collect::<Vec<_>>());
}

#[test]
fn the_threaded_filter_empty_states_offer_the_resolver_action() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let cases = [
        (ScreenshotState::EmptyUnread, "Nessuna conversazione non letta", "Mostra tutti", EmptyAction::ShowAll),
        (ScreenshotState::EmptyAttachments, "Nessuna conversazione con allegati", "Mostra tutti", EmptyAction::ShowAll),
        (
            ScreenshotState::EmptyLabelUnread,
            "Nessuna conversazione non letta con l'etichetta “Lavoro”",
            "Togli filtro “non letti”",
            EmptyAction::ClearUnread,
        ),
    ];
    for (state, title, primary, action) in cases {
        let mut app = PostinoApp::demo(true);
        app.seed_screenshot(state);
        app.threads_view = true;
        app.threads_cache = None;
        app.list_cache = None;
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        let view = probe::empty_state_view(&ctx).expect("the threaded empty state");
        assert_eq!(view.title, title, "{state:?}");
        assert_eq!(view.primary.as_deref(), Some(primary), "{state:?}");
        assert!(!text.iter().any(|line| line.contains("Nessun messaggio")), "{state:?}: flat copy leaked: {text:?}");
        assert!(!view.body.contains("è vuota"), "{state:?} must not blame the folder");
        app.apply_empty_action(action);
        assert!(!app.visible_threads().0.is_empty(), "{state:?}: the way out revealed no conversation");
    }
}

#[test]
fn a_threaded_parse_error_does_not_also_render_the_empty_state() {
    let (ctx, mut app) = ready_app();
    app.query = "subject:\"unclosed".to_owned();
    app.commit_search();
    app.threads_view = true;
    app.threads_cache = None;
    let _ = app.visible_threads();
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Ricerca non valida")), "{text:?}");
    assert!(!text.iter().any(|line| line.contains("Nessuna conversazione")), "the invalid query showed a threaded empty state: {text:?}");
}

#[test]
fn opening_a_message_refreshes_the_conversation_unread_count() {
    let mut app = PostinoApp::demo(true);
    app.threads_view = true;
    app.select_thread("thr_preventivo".to_owned());
    let before = app.thread_summary.as_ref().expect("the thread summary").unread_count;
    let open = app.thread_messages.iter().find(|message| message.unread).expect("an unread card").id.clone();
    app.mark_read_after_delay(&open);
    let after = app.thread_summary.as_ref().expect("the refreshed summary").unread_count;
    assert_eq!(after + 1, before, "the banner kept the stale unread count");
    assert!(
        app.thread_messages.iter().find(|message| message.id == open).is_some_and(|message| !message.unread),
        "the loaded card still claims to be unread"
    );
}

#[test]
fn thread_rows_show_their_star_and_the_menu_matches_it() {
    let (ctx, mut app) = conversations_app();
    let wide = Vec2::new(1280.0, 720.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    let (starred, row) = probe::thread_star_state(&ctx, "thr_preventivo").expect("the starred thread row");
    assert!(starred, "the Preventivo thread has a starred message and must show it");
    assert!(ctx.content_rect().contains_rect(row));
    let other = app
        .visible_threads()
        .0
        .into_iter()
        .find(|thread| thread.id != "thr_preventivo" && thread.starred_count == 0)
        .expect("an unstarred thread")
        .id;
    let (unstarred, _) = probe::thread_star_state(&ctx, &other).expect("an unstarred thread row");
    assert!(!unstarred);
    assert_eq!(thread_star_menu(true), ("unstar", false));
    assert_eq!(thread_star_menu(false), ("star", true));
    assert_eq!(command_label("unstar", "x"), "Togli la stella");
    assert_eq!(command_label("star", "x"), "Aggiungi una stella");
}

#[test]
fn clearing_a_search_reconnects_the_open_hit_to_its_thread() {
    let mut app = PostinoApp::demo(true);
    app.threads_view = true;
    app.query = "from:giulia".to_owned();
    app.commit_search();
    let hit = app.visible_messages().0.first().expect("a search hit").id.clone();
    app.open_message(hit.clone());
    assert!(!app.threads_view && app.selected_thread.is_none());
    app.clear_search();
    assert!(app.threads_view, "the folder returns to conversations");
    assert!(app.reading_open, "the open hit stays readable");
    assert_eq!(app.selected.as_deref(), Some(hit.as_str()));
    assert_eq!(app.thread_open.as_deref(), Some(hit.as_str()));
    let thread = app.selected_thread.as_deref().expect("the hit's thread is selected");
    assert!(app.thread_messages.iter().any(|message| message.id == hit && message.thread_id == thread));
}

#[test]
fn the_conversation_opens_the_latest_unread_card_and_alt_jk_moves_it() {
    let (ctx, mut app) = conversations_app();
    app.select_thread("thr_preventivo".to_owned());
    assert!(app.reading_open);
    assert_eq!(app.thread_messages.len(), 3);
    assert_eq!(app.thread_total, 3);
    assert_eq!(app.selected.as_deref(), Some("msg_001"), "the latest unread message is the open card");
    assert_eq!(app.thread_open.as_deref(), Some("msg_001"));
    app.step_thread_message(-1);
    assert_eq!(app.selected.as_deref(), Some("msg_002"), "Alt+K moves to the previous card");
    assert_eq!(app.thread_open.as_deref(), Some("msg_002"));

    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("CONVERSAZIONE")), "the conversation banner is missing at 980: {text:?}");
    assert!(text.iter().any(|line| line.contains("Preventivo")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("Giulia")), "the sender cards are missing: {text:?}");
}

#[test]
fn a_thread_archive_moves_every_card_in_one_undo() {
    let mut app = PostinoApp::demo(true);
    app.threads_view = true;
    app.select_thread("thr_preventivo".to_owned());
    app.move_selected(FolderKind::Archive, "Conversazione archiviata", "Conversazioni archiviate");
    for id in ["msg_001", "msg_002", "msg_003"] {
        assert_eq!(app.engine.message(id).map(|message| message.summary.folder_id), Some("archive".to_owned()), "{id}");
    }
    assert!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some(), "the archive must offer one undo");
    app.undo_last();
    assert_eq!(app.engine.message("msg_001").map(|message| message.summary.folder_id), Some("inbox".to_owned()));
    assert_eq!(app.engine.message("msg_002").map(|message| message.summary.folder_id), Some("sent".to_owned()));
    assert_eq!(app.engine.message("msg_003").map(|message| message.summary.folder_id), Some("inbox".to_owned()));
}

#[test]
fn a_thread_flag_marks_every_card_and_an_idempotent_repeat_changes_nothing() {
    let mut app = PostinoApp::demo(true);
    app.threads_view = true;
    app.select_thread("thr_preventivo".to_owned());
    app.mark_unread();
    assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
    app.toggle_read();
    assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| !message.summary.unread)));
    app.toggle_read();
    assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
}

#[test]
fn search_is_flat_until_the_user_toggles_conversations() {
    let (_ctx, mut app) = conversations_app();
    app.query = "preventivo".to_owned();
    app.commit_search();
    assert!(!app.threads_view, "a new search starts flat");
    app.toggle_threads_view();
    assert!(app.threads_view);
    let (threads, total, _, _) = app.visible_threads();
    assert_eq!(total, 1);
    assert_eq!(threads[0].id, "thr_preventivo");
    app.clear_search();
    assert!(app.threads_view, "clearing the search restores the folder's threaded default");
}

#[test]
fn the_thread_and_saved_scenes_render_at_980_light_and_dark() {
    for dark in [false, true] {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        for (state, expected) in
            [(ScreenshotState::Threads, "Preventivo"), (ScreenshotState::Conversation, "CONVERSAZIONE"), (ScreenshotState::SavedSearches, "RICERCHE")]
        {
            let mut app = PostinoApp::demo_at(true, clock);
            app.dark = dark;
            app.seed_screenshot(state);
            frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
            let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
            assert!(text.iter().any(|line| line.contains(expected)), "{state:?} at 980 (dark={dark}) is missing {expected:?}: {text:?}");
        }
    }
}

#[test]
fn keyboard_only_thread_flow_moves_opens_a_card_and_closes() {
    let (ctx, mut app) = conversations_app();
    frame(&ctx, &mut app, vec![key(Key::J, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some("thr_preventivo"));
    assert!(!app.reading_open, "J only selects the thread row");
    frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(app.reading_open);
    assert_eq!(app.selected.as_deref(), Some("msg_001"));
    assert_eq!(app.thread_messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), vec!["msg_003", "msg_002", "msg_001"]);
    frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::ALT), key(Key::K, Modifiers::ALT)]);
    assert_eq!(app.selected.as_deref(), Some("msg_002"), "Alt+K moves the open card, not the thread row");
    assert_eq!(app.thread_open.as_deref(), Some("msg_002"));
    frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::NONE), key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.reading_open);
    assert_eq!(app.selected_thread.as_deref(), Some("thr_preventivo"), "Esc goes back to the list, not away from the thread");
}

/// Thirty one-message conversations with distinct subjects, newest first. One message per thread
/// is the case that used to trap the arrows: `step_thread_message` clamped inside the open thread.
fn many_conversations_app(name: &str) -> PostinoApp {
    let mut app = booted_app(name, Vec::new(), |store| {
        seed_account_rows(store, "acc_many", "many@example.com", None);
        for index in 0..30i64 {
            let id = format!("msg_many_{index:02}");
            let mut row = store_message("fld_acc_many_inbox", "acc_many", &id, &format!("Conversazione {index:02}"));
            row.summary.date = 1_700_000_000_000 - index * 1_000;
            store.upsert_message(&row).expect("message row");
        }
    });
    app.threads_view = true;
    app.threads_cache = None;
    app
}

#[test]
fn arrows_walk_the_conversation_list_and_the_preview_follows() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let mut app = many_conversations_app("p11a-thread-arrows");
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, Vec::new());
    let (threads, _, _, _) = app.visible_threads();
    assert!(threads.len() >= 4, "the fixture needs more conversations than fit a short viewport");
    let ids = threads.iter().take(3).map(|thread| thread.id.clone()).collect::<Vec<_>>();
    let subjects = threads.iter().take(3).map(|thread| thread_subject(&thread.subject)).collect::<Vec<_>>();

    frame_at(&ctx, &mut app, size, vec![key(Key::J, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(ids[0].as_str()), "J selects the first conversation");
    assert!(!app.reading_open, "J only selects the row");

    frame_at(&ctx, &mut app, size, vec![key(Key::J, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(ids[1].as_str()), "J moves to the second conversation");
    assert!(!app.reading_open, "a closed pane stays closed while browsing");

    frame_at(&ctx, &mut app, size, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(app.reading_open, "Enter opens the preview");
    let second_open = app.selected.clone().expect("an open card");
    assert!(
        app.engine.message(&second_open).is_some_and(|message| message.summary.thread_id == ids[1]),
        "the preview shows the second conversation: {second_open}"
    );

    frame_at(&ctx, &mut app, size, vec![key(Key::J, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(ids[2].as_str()), "J walks the list while the preview is open");
    assert!(app.reading_open, "the preview follows the new conversation instead of closing");
    let third_open = app.selected.clone().expect("the new open card");
    assert_ne!(third_open, second_open, "the preview content changed with the thread");
    assert!(app.engine.message(&third_open).is_some_and(|message| message.summary.thread_id == ids[2]));
    let text = painted_text(&ctx, &mut app, size);
    assert!(text.iter().any(|line| line.contains(&subjects[2])), "the preview shows {:?}: {text:?}", subjects[2]);

    frame_at(&ctx, &mut app, size, vec![key(Key::ArrowUp, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(ids[1].as_str()), "ArrowUp walks back to the second conversation");
    assert!(app.reading_open, "ArrowUp keeps the preview open");

    frame_at(&ctx, &mut app, size, vec![key(Key::ArrowUp, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(ids[0].as_str()));
    let top_message = app.selected.clone();
    frame_at(&ctx, &mut app, size, vec![key(Key::ArrowUp, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(ids[0].as_str()), "ArrowUp at the top must not wrap");
    assert_eq!(app.selected, top_message, "the top row reopened the same message");
    let focused = ctx.memory(|memory| memory.focused());
    assert!(focused.is_none_or(|focused| app.row_ids.values().any(|id| *id == focused)), "the focus left the conversation list: {focused:?}");
}

#[test]
fn the_selected_conversation_row_stays_in_the_list_viewport() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let mut app = many_conversations_app("p11a-thread-scroll");
    let size = Vec2::new(980.0, 600.0);
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    frame_at(&ctx, &mut app, size, Vec::new());
    for _ in 0..20 {
        frame_at(&ctx, &mut app, size, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    }
    frame_at(&ctx, &mut app, size, Vec::new());
    let selected = app.selected_thread.clone().expect("a selected conversation");
    let rows = probe::rows(&ctx);
    let rect = rows
        .get(&format!("thread:{selected}"))
        .copied()
        .unwrap_or_else(|| panic!("the selected conversation row {selected:?} is not painted: {:?}", rows.keys().collect::<Vec<_>>()));
    let content = ctx.content_rect();
    let top = content.top() + ui_size::TITLE_BAR + ui_size::TOOLBAR;
    let bottom = content.bottom() - ui_size::STATUS;
    assert!(rect.top() >= top && rect.bottom() <= bottom, "the selected row is outside the list viewport: {rect:?} in {top}..{bottom}");
    let focused = ctx.memory(|memory| memory.focused());
    assert!(focused.is_none_or(|focused| app.row_ids.values().any(|id| *id == focused)), "the focus left the conversation list: {focused:?}");
}

#[test]
fn alt_arrows_move_the_open_card_and_keep_the_selected_conversation() {
    let (ctx, mut app) = conversations_app();
    app.select_thread("thr_preventivo".to_owned());
    app.step_thread_message(-1);
    assert_eq!(app.selected.as_deref(), Some("msg_002"), "the fixture starts on the middle card");
    let thread = app.selected_thread.clone();
    frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::ALT), key(Key::ArrowDown, Modifiers::ALT)]);
    assert_eq!(app.selected.as_deref(), Some("msg_001"), "Alt+ArrowDown moves to the next card");
    assert_eq!(app.thread_open.as_deref(), Some("msg_001"));
    assert_eq!(app.selected_thread, thread, "the conversation must not move");
    frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::ALT), key(Key::ArrowUp, Modifiers::ALT)]);
    assert_eq!(app.selected.as_deref(), Some("msg_002"), "Alt+ArrowUp moves back");
    assert_eq!(app.selected_thread, thread);
    frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::NONE), key(Key::ArrowDown, Modifiers::NONE)]);
    assert_ne!(app.selected_thread, thread, "a plain arrow moves the conversation instead of the card");
}

#[test]
fn arrows_take_keyboard_focus_away_from_the_clicked_conversation_row() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let mut app = many_conversations_app("p11a-thread-focus-ring");
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, Vec::new());
    let (threads, _, _, _) = app.visible_threads();
    let first = threads[0].id.clone();
    let second = threads[1].id.clone();
    let first_rect = probe::rows(&ctx).get(&format!("thread:{first}")).copied().expect("the first conversation row");

    click_at(&ctx, &mut app, first_rect.center(), size);
    assert_eq!(app.selected_thread.as_deref(), Some(first.as_str()), "the click selects the row");
    let first_row = app.row_ids.get(&first).copied().expect("the clicked row is registered");
    assert_eq!(ctx.memory(|memory| memory.focused()), Some(first_row), "the click gives the row keyboard focus");

    // The click opens the preview overlay at this width. Esc would also drop egui's focus, which
    // is the state under test, so close the pane directly and keep the clicked row focused.
    app.close_reading();
    frame_at(&ctx, &mut app, size, Vec::new());
    assert!(!app.reading_open);
    let first_rect = probe::rows(&ctx).get(&format!("thread:{first}")).copied().expect("the first row, list visible");
    assert_eq!(ctx.memory(|memory| memory.focused()), Some(first_row), "closing the pane keeps the clicked row focused");

    let rings = gold_stroke_rects_after(&ctx, &mut app, size, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(second.as_str()), "ArrowDown selects the second conversation");
    let second_row = app.row_ids.get(&second).copied().expect("the second row is registered");
    assert_eq!(ctx.memory(|memory| memory.focused()), Some(second_row), "keyboard focus follows the selection");
    assert_no_thread_ring(&ctx, &rings);
    assert!(!rings.iter().any(|ring| same_rect(*ring, first_rect.expand(2.0))), "the clicked row kept the gold ring: {rings:?}");

    let second_rect = probe::rows(&ctx).get(&format!("thread:{second}")).copied().expect("the second row");
    let rings = gold_stroke_rects_after(&ctx, &mut app, size, vec![key(Key::ArrowUp, Modifiers::NONE)]);
    assert_eq!(app.selected_thread.as_deref(), Some(first.as_str()), "ArrowUp returns to the first conversation");
    assert_eq!(ctx.memory(|memory| memory.focused()), Some(first_row), "focus follows the selection back");
    assert_no_thread_ring(&ctx, &rings);
    assert!(!rings.iter().any(|ring| same_rect(*ring, second_rect.expand(2.0))), "the previously selected row kept the gold ring: {rings:?}");
}

/// No conversation row may paint the gold ring while its row is selected or the selection moved:
/// the forest fill is the single cue, and an unselected row must not keep focus.
fn assert_no_thread_ring(ctx: &egui::Context, rings: &[Rect]) {
    for (name, rect) in probe::rows(ctx).iter().filter(|(name, _)| name.starts_with("thread:")) {
        assert!(!rings.iter().any(|ring| same_rect(*ring, rect.expand(2.0))), "{name} painted a gold focus ring: {rings:?}");
    }
}

#[test]
fn thread_actions_after_j_target_the_selected_conversation() {
    let mut app = PostinoApp::demo(true);
    app.threads_view = true;
    app.select_thread("thr_preventivo".to_owned());
    app.close_reading();
    app.step_selection(1, false);
    let target = app.selected_thread.clone().expect("a second thread");
    assert_ne!(target, "thr_preventivo");
    let target_ids = app.thread_messages.iter().map(|message| message.id.clone()).collect::<Vec<_>>();
    assert!(!target_ids.is_empty());
    app.move_selected(FolderKind::Archive, "Conversazione archiviata", "Conversazioni archiviate");
    assert!(target_ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id == "archive")));
    assert!(["msg_001", "msg_002", "msg_003"].iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id != "archive")));
    assert!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some());
    app.undo_last();
    assert!(target_ids.iter().all(|id| app.engine.message(id).is_some_and(|message| message.summary.folder_id != "archive")));
}

#[test]
fn thread_flag_after_j_updates_the_page_and_offers_undo_each_time() {
    let mut app = PostinoApp::demo(true);
    app.threads_view = true;
    app.select_thread("thr_preventivo".to_owned());
    app.close_reading();
    app.mark_unread();
    assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
    app.toggle_read();
    assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| !message.summary.unread)));
    app.toggle_read();
    assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.unread)));
    let undo_steps = app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some();
    assert!(undo_steps, "the third flag must still offer undo");
    app.set_starred(true);
    assert!(app.thread_messages.iter().all(|message| app.engine.message(&message.id).is_some_and(|message| message.summary.starred)));
}

#[test]
fn enter_opens_the_selected_card_and_reply_targets_it() {
    let (ctx, mut app) = conversations_app();
    app.select_thread("thr_preventivo".to_owned());
    app.step_thread_message(-1);
    assert_eq!(app.selected.as_deref(), Some("msg_002"));
    frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
    assert_eq!(app.selected.as_deref(), Some("msg_002"), "Enter must not jump back to the latest unread");
    assert_eq!(app.thread_open.as_deref(), Some("msg_002"));
    frame(&ctx, &mut app, vec![key(Key::R, Modifiers::NONE)]);
    assert!(app.composer.open, "R opens the reply composer");
    assert!(app.composer.in_reply_to.as_deref().is_some_and(|parent| parent.contains("msg_002")), "{:?}", app.composer.in_reply_to);
}

#[test]
fn a_large_thread_opens_on_its_newest_page_and_loads_older_messages() {
    let mut app = booted_app("p9b-large-thread", Vec::new(), |store| {
        seed_account_rows(store, "acc_big", "big@example.com", None);
        for index in 0..250u32 {
            let id = format!("msg_big_{index:03}");
            let mut row = store_message("fld_acc_big_inbox", "acc_big", &id, &format!("Discussione {index}"));
            row.summary.thread_id = "big@example.com".to_owned();
            row.summary.date = 1_700_000_000_000 + i64::from(index);
            row.message_id = Some(format!("<big-{index}@example.com>"));
            store.upsert_synced_message_with_refs(&row, &[], Some("big@example.com"), &["big@example.com".to_owned()]).expect("message");
        }
        store.recount_folder("fld_acc_big_inbox").expect("counts");
    });
    app.threads_view = true;
    app.folder = "fld_acc_big_inbox".to_owned();
    app.thread_list_limit = 200;
    app.select_thread("big@example.com".to_owned());
    assert_eq!(app.thread_total, 250);
    assert_eq!(app.thread_messages.len(), 200);
    assert_eq!(app.thread_offset, 50);
    assert_eq!(app.selected.as_deref(), Some("msg_big_249"));
    app.load_older_thread_messages();
    assert_eq!(app.thread_messages.len(), 250);
    assert_eq!(app.thread_offset, 0);
}

#[test]
fn a_large_thread_list_loads_the_next_page() {
    let mut app = booted_app("p9b-large-list", Vec::new(), |store| {
        seed_account_rows(store, "acc_list", "list@example.com", None);
        for index in 0..200u32 {
            let id = format!("msg_thread_{index:03}");
            let mut row = store_message("fld_acc_list_inbox", "acc_list", &id, &format!("Filo {index}"));
            row.summary.thread_id = format!("thread-{index}");
            row.summary.date = 1_700_000_000_000 + i64::from(index);
            store.upsert_message(&row).expect("message");
        }
        store.recount_folder("fld_acc_list_inbox").expect("counts");
    });
    app.threads_view = true;
    app.folder = "fld_acc_list_inbox".to_owned();
    app.thread_list_limit = 200;
    let (rows, total, _, _) = app.visible_threads();
    assert_eq!(total, 201);
    assert_eq!(rows.len(), 200);
    app.thread_list_limit = 400;
    app.threads_cache = None;
    let (rows, total, _, _) = app.visible_threads();
    assert_eq!(total, 201);
    assert_eq!(rows.len(), 201);
}

/// Every gold stroke painted by one frame, as a rectangle. The focus ring is the only gold
/// `rect_stroke` a row paints, so the selection can be checked by the absence of one around it.
fn gold_stroke_rects(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) -> Vec<Rect> {
    gold_stroke_rects_after(ctx, app, size, Vec::new())
}

/// The same gold strokes, on a frame driven by `events`, so a test can inspect the ring in the
/// very frame the selection moved.
fn gold_stroke_rects_after(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, events: Vec<egui::Event>) -> Vec<Rect> {
    let gold = theme::tokens(app.dark).gold;
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), events, ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    output.textures_delta.clear();
    fn collect(shape: &egui::epaint::Shape, gold: egui::Color32, rects: &mut Vec<Rect>) {
        match shape {
            egui::epaint::Shape::Rect(rect) if rect.stroke.color == gold && rect.stroke.width >= 1.5 => rects.push(rect.rect),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, gold, rects);
                }
            }
            _ => {}
        }
    }
    let mut rects = Vec::new();
    for clipped in &output.shapes {
        collect(&clipped.shape, gold, &mut rects);
    }
    rects
}

fn same_rect(left: Rect, right: Rect) -> bool {
    (left.min - right.min).length() < 0.5 && (left.max - right.max).length() < 0.5
}

#[test]
fn a_selected_thread_row_does_not_keep_the_focus_ring() {
    let (ctx, mut app) = conversations_app();
    let size = Vec2::new(1280.0, 900.0);
    frame_at(&ctx, &mut app, size, vec![]);
    // Tab until a conversation row owns keyboard focus. The row must paint the ring now, because
    // nothing is selected yet: this is the keyboard-focus-without-selection case.
    let mut focused = None;
    for _ in 0..160 {
        frame_at(&ctx, &mut app, size, vec![key(Key::Tab, Modifiers::NONE)]);
        let rings = gold_stroke_rects(&ctx, &mut app, size);
        let rows = probe::rows(&ctx);
        if let Some((name, rect)) =
            rows.iter().filter(|(name, _)| name.starts_with("thread:")).find(|(_, rect)| rings.iter().any(|ring| same_rect(*ring, rect.expand(2.0))))
        {
            focused = Some((name.clone(), *rect));
            break;
        }
    }
    let (name, _) = focused.expect("Tab must reach a conversation row");
    assert!(app.selected_thread.is_none(), "Tab moves focus without selecting the conversation");

    // Select the focused row without moving focus: the forest fill is now the only cue.
    let id = name.strip_prefix("thread:").expect("a conversation probe key").to_owned();
    app.load_thread_page(id.clone(), false, None);
    frame_at(&ctx, &mut app, size, vec![]);
    assert_eq!(app.selected_thread.as_deref(), Some(id.as_str()));
    assert!(ctx.memory(|memory| memory.focused()).is_some(), "the row should still own keyboard focus");
    let row = probe::rows(&ctx).get(&name).copied().expect("the selected conversation row");
    let rings = gold_stroke_rects(&ctx, &mut app, size);
    let target = row.expand(2.0);
    assert!(
        rings.iter().all(|rect| !same_rect(*rect, target)),
        "a focused, selected row must not paint the gold ring: rings {rings:?} around {target:?}"
    );
}

#[test]
fn a_thread_row_has_one_tooltip_carrying_subject_participants_and_date() {
    let (ctx, mut app) = conversations_app();
    let size = Vec2::new(1280.0, 900.0);
    frame_at(&ctx, &mut app, size, vec![]);
    let (threads, _, _, _) = app.visible_threads();
    let thread = threads.iter().find(|thread| thread.id == "thr_preventivo").expect("the demo conversation");
    let subject = thread_subject(&thread.subject);
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
    let count = messages_text(usize::try_from(thread.message_count).unwrap_or(usize::MAX));
    let date = format_when(thread.last_date, app.now_utc);

    // Accessibility tool mode paints every registered tooltip without waiting for a hover, so a
    // second stacked tooltip would appear as a second run.
    ctx.memory_mut(|memory| memory.set_everything_is_visible(true));
    let runs = painted_runs(&ctx, &mut app, size);
    ctx.memory_mut(|memory| memory.set_everything_is_visible(false));

    let tips = runs.iter().filter(|run| run.text.contains("Premi Invio per aprire") && run.text.contains(&subject)).collect::<Vec<_>>();
    assert_eq!(tips.len(), 1, "one tooltip per thread row, found {:?}", tips.iter().map(|tip| &tip.text).collect::<Vec<_>>());
    let tip = &tips[0].text;
    assert!(tip.contains(&subject), "the tooltip names the subject: {tip}");
    assert!(tip.contains(&participants), "the tooltip carries the participants: {tip}");
    assert!(tip.contains(&count), "the tooltip carries the message count: {tip}");
    assert!(tip.contains(&date), "the tooltip carries the date: {tip}");
    let stacked = format!("{subject} — {participants} · {count}");
    assert!(!runs.iter().any(|run| run.text == stacked), "the default title — subtitle tooltip must be replaced, not stacked: {stacked}");
}

#[test]
fn the_thread_header_and_rows_keep_only_totals_and_the_unread_dot() {
    let (ctx, mut app) = conversations_app();
    let size = Vec2::new(1280.0, 900.0);
    frame_at(&ctx, &mut app, size, vec![]);
    let (_, total, _, _) = app.visible_threads();
    let expected_meta = plural(usize::try_from(total).unwrap_or(usize::MAX), "conversazione", "conversazioni");
    let row = probe::rows(&ctx).get("thread:thr_preventivo").copied().expect("the conversation row");
    let (folder_name, folder_unread) = app
        .engine
        .folders()
        .iter()
        .find(|folder| folder.id == app.folder)
        .map(|folder| (folder.name.clone(), folder.unread))
        .expect("the active folder");
    let runs = painted_runs(&ctx, &mut app, size);

    let header = runs
        .iter()
        .filter(|run| run.rect.center().x >= row.left() - 1.0 && run.rect.center().x <= row.right() + 1.0 && run.rect.bottom() <= row.top() + 1.0)
        .collect::<Vec<_>>();
    assert!(header.iter().any(|run| run.text == expected_meta), "the header still names the total: {header:?}");
    assert!(
        header.iter().all(|run| !run.text.contains("non lett")),
        "the header no longer repeats the unread count: {:?}",
        header.iter().map(|run| &run.text).collect::<Vec<_>>()
    );

    let thread_runs = runs.iter().filter(|run| row.contains(run.rect.center())).collect::<Vec<_>>();
    assert!(thread_runs.iter().any(|run| run.text.contains("messaggi")), "the thread row keeps its message count: {thread_runs:?}");
    assert!(
        thread_runs.iter().all(|run| !run.text.contains("non lett")),
        "the thread subtitle no longer repeats the unread count: {:?}",
        thread_runs.iter().map(|run| &run.text).collect::<Vec<_>>()
    );

    // The folder badge answers the same question once, in the sidebar.
    let folder_row = probe::rows(&ctx).get(&folder_name).copied().expect("the inbox folder row");
    assert!(folder_unread > 0, "the demo inbox has unread mail to badge");
    assert!(
        runs.iter().any(|run| run.text == folder_unread.to_string() && folder_row.contains(run.rect.center())),
        "the folder badge keeps its unread count {folder_unread}: {folder_name}"
    );
}
