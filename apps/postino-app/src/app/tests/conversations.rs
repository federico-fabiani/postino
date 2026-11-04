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
    assert_eq!(command_label("unstar", "x"), "Togli stella");
    assert_eq!(command_label("star", "x"), "Aggiungi stella");
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
fn the_conversation_opens_the_latest_unread_card_and_jk_moves_it() {
    let (ctx, mut app) = conversations_app();
    app.select_thread("thr_preventivo".to_owned());
    assert!(app.reading_open);
    assert_eq!(app.thread_messages.len(), 3);
    assert_eq!(app.thread_total, 3);
    assert_eq!(app.selected.as_deref(), Some("msg_001"), "the latest unread message is the open card");
    assert_eq!(app.thread_open.as_deref(), Some("msg_001"));
    app.step_selection(-1, false);
    assert_eq!(app.selected.as_deref(), Some("msg_002"), "K moves to the previous card");
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
    frame(&ctx, &mut app, vec![key(Key::K, Modifiers::NONE)]);
    assert_eq!(app.selected.as_deref(), Some("msg_002"));
    assert_eq!(app.thread_open.as_deref(), Some("msg_002"));
    frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.reading_open);
    assert_eq!(app.selected_thread.as_deref(), Some("thr_preventivo"), "Esc goes back to the list, not away from the thread");
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
