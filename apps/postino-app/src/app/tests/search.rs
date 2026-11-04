//! Search, filters and saved searches.

use super::*;

#[test]
fn search_unread_and_attachment_filters_apply_before_the_page() {
    let mut app = booted_app("p9a-search-filters", Vec::new(), |store| {
        seed_account_rows(store, "acc_f", "f@example.com", None);
        for index in 0..250u32 {
            let id = format!("msg_f_{index:03}");
            let mut row = store_message("fld_acc_f_inbox", "acc_f", &id, "Comune");
            row.summary.date = 1_700_000_000_000 + i64::from(index);
            row.summary.unread = index < 10;
            row.summary.has_attachments = index < 5;
            row.body_text = "comune nel corpo".to_owned();
            store.upsert_message(&row).expect("message");
            if index < 5 {
                seed_attachment(store, &id, &format!("att_{index}"), "nota.txt");
            }
        }
    });
    app.query = "comune".to_owned();
    app.unread_only = true;
    app.commit_search();
    let (rows, total, _) = app.visible_messages();
    assert_eq!(total, 10, "the unread filter must be part of the search page, not a post-filter");
    assert_eq!(rows.len(), 10);
    assert!(rows.iter().all(|message| message.unread));

    app.unread_only = false;
    app.attachments_only = true;
    app.commit_search();
    let (rows, total, _) = app.visible_messages();
    assert_eq!(total, 5);
    assert!(rows.iter().all(|message| message.has_attachments));
}

#[test]
fn search_is_scoped_to_the_current_folder() {
    let mut app = booted_app("p9a-search-folder", Vec::new(), |store| {
        seed_account_rows(store, "acc_s", "s@example.com", None);
        for (folder, prefix) in [("fld_acc_s_inbox", "inbox"), ("fld_acc_s_sent", "sent")] {
            for index in 0..3u32 {
                let id = format!("msg_{prefix}_{index}");
                let mut row = store_message(folder, "acc_s", &id, "Comune");
                row.summary.date = 1_700_000_000_000 + i64::from(index);
                row.body_text = "comune nel corpo".to_owned();
                store.upsert_message(&row).expect("message");
            }
        }
    });
    app.folder = "fld_acc_s_sent".to_owned();
    app.query = "comune".to_owned();
    app.commit_search();
    let (rows, total, _) = app.visible_messages();
    assert_eq!(total, 3);
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|message| message.folder_id == "fld_acc_s_sent"), "search escaped the current folder");
}

#[test]
fn search_meta_shows_the_cap_and_index_progress_has_a_banner() {
    assert_eq!(search_meta(10_000, true, None), "primi 10.000 risultati");
    assert_eq!(search_meta(3, false, Some("in Lavoro")), "3 messaggi · in Lavoro");
    let (ctx, mut app) = ready_app();
    app.query = "preventivo".to_owned();
    app.commit_search();
    let _ = app.visible_messages();
    app.index_progress = Some(postino_core::SearchStatusResult {
        ready: false,
        indexed: 500,
        total: 1_000,
        last_error: None,
        size_bytes: 4096,
        state: "building".to_owned(),
        updated_at: 1,
    });
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Indice di ricerca in preparazione (500/1000)")), "{text:?}");
}

#[test]
fn a_parse_error_does_not_also_render_the_empty_state() {
    let (ctx, mut app) = ready_app();
    app.query = "subject:\"unclosed".to_owned();
    app.commit_search();
    let _ = app.visible_messages();
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Ricerca non valida")), "{text:?}");
    assert!(!text.iter().any(|line| line.contains("Nessun risultato")), "the invalid query showed an empty state: {text:?}");
}

#[test]
fn search_debounces_commits_on_enter_and_explains_parse_errors() {
    let mut app = PostinoApp::demo(true);
    app.now = 10.0;
    app.query = "preventivo".to_owned();
    app.search_debounce_until = Some(10.1);
    assert_eq!(app.search_committed, "");
    app.now = 10.2;
    let (_rows, _total, _) = app.visible_messages();
    assert_eq!(app.search_committed, "preventivo");

    app.query = "fattura".to_owned();
    app.commit_search();
    assert_eq!(app.search_committed, "fattura");
    assert!(app.search_debounce_until.is_none());

    app.query = "subject:\"unclosed".to_owned();
    app.commit_search();
    let (rows, _, _) = app.visible_messages();
    assert!(rows.is_empty());
    let (code, position) = app.search_error.clone().expect("parse error");
    assert_eq!((code.as_str(), position), ("unclosed_quote", 8));
    assert!(search_error_text("unclosed_quote", position).contains("posizione 8"));

    app.clear_search();
    assert!(app.query.is_empty() && app.search_committed.is_empty() && app.search_error.is_none());
    assert!(search_chips("from:giulia is:unread label:Lavoro").len() == 3);
    assert_eq!(search_chips("from:giulia AND subject:\"fattura\""), vec!["from:\"giulia\"".to_owned(), "subject:\"fattura\"".to_owned()]);
    let chips = search_chips("from:giulia NOT from:spam");
    assert!(chips.iter().any(|chip| chip.starts_with("NOT (")), "{chips:?}");
}

#[test]
fn search_scenes_render_at_980_light_and_dark() {
    for dark in [false, true] {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let mut app = PostinoApp::demo_at(true, clock);
        app.dark = dark;
        app.seed_screenshot(ScreenshotState::Search);
        assert!(!app.threads_view, "the first search is flat, not threaded");
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Risultati per")), "search results are missing at 980: {text:?}");
        assert!(text.iter().any(|line| line.contains("from:")), "the syntax help is missing at 980: {text:?}");
        assert!(text.iter().any(|line| line == "MITTENTE"), "the flat result table is missing: {text:?}");
        assert!(!text.iter().any(|line| line.contains("CONVERSAZIONE")), "the search stayed threaded: {text:?}");

        app.seed_screenshot(ScreenshotState::SearchError);
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Ricerca non valida")), "the parse error is missing at 980: {text:?}");
    }
}

#[test]
fn filter_and_search_intersect() {
    let mut app = PostinoApp::demo(true);
    app.select_label("lbl_1".to_owned());
    app.query = "preventivo".to_owned();
    let (rows, total, unread) = app.visible_messages();
    assert_eq!(total, 3, "three Lavoro messages mention the quote");
    assert_eq!(unread, 2);
    assert!(rows.iter().all(|message| message.labels.contains(&"lbl_1".to_owned())));

    app.select_label("lbl_3".to_owned());
    app.query = "preventivo".to_owned();
    assert_eq!(app.visible_messages().1, 0, "the same search under Fatture has no results");
}

#[test]
fn the_search_placeholder_names_the_active_label() {
    let mut app = PostinoApp::demo(true);
    assert_eq!(app.search_placeholder(), "Cerca in tutte le mail…");
    app.select_label("lbl_1".to_owned());
    assert_eq!(app.search_placeholder(), "Cerca in Lavoro…");
    app.select_label("lbl_3".to_owned());
    assert_eq!(app.search_placeholder(), "Cerca in Fatture…");
}

#[test]
fn label_cache_tracks_auto_read_batch_read_and_undo() {
    let mut app = PostinoApp::demo(true);
    app.now = 10.0;
    assert_eq!(label_unread(&app, "lbl_1"), Some(2));
    let unread = app
        .engine
        .messages()
        .iter()
        .find(|message| message.unread && message.labels.iter().any(|id| id == "lbl_1"))
        .map(|message| message.id.clone())
        .expect("an unread Lavoro message");
    app.open_message(unread);
    app.now = 10.7;
    app.maybe_complete_read();
    assert_eq!(label_unread(&app, "lbl_1"), Some(1), "auto-read updates the sidebar counts");
    app.undo_last();
    assert_eq!(label_unread(&app, "lbl_1"), Some(2), "undo updates them back");

    let remaining = app
        .engine
        .messages()
        .iter()
        .filter(|message| message.unread && message.labels.iter().any(|id| id == "lbl_1"))
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    app.set_unread_many(&remaining, false);
    assert_eq!(label_unread(&app, "lbl_1"), Some(0), "a batch read updates the cache once");
}

#[test]
fn palette_clear_filter_is_disabled_until_a_label_is_active() {
    let mut app = PostinoApp::demo(true);
    let clear = command("label_clear_filter").expect("label_clear_filter is registered");
    assert_eq!(clear.label, "Togli filtro etichetta");
    assert!(!app.command_enabled(clear));
    assert_eq!(clear.rule.disabled_reason(), "Nessun filtro etichetta attivo");
    let rename = command("label_rename").expect("label_rename is registered");
    assert!(!app.command_enabled(rename));
    assert_eq!(rename.rule.disabled_reason(), "Seleziona prima un'etichetta");

    app.select_label("lbl_1".to_owned());
    assert!(app.command_enabled(clear));
    assert!(app.command_enabled(rename));
    let ctx = egui::Context::default();
    app.execute_command(CommandId::LabelClearFilter, &ctx);
    assert!(app.label_filter.is_none(), "the enabled command changes the view");
}

#[test]
fn a_saved_search_clears_the_label_keeps_folder_scope_only_with_folder_and_esc_restores() {
    let (_ctx, mut app) = conversations_app();
    app.select_label("lbl_1".to_owned());
    app.query = "preventivo".to_owned();
    app.commit_search();
    app.engine.call("saved_search_create", json!({ "name": "Da Giulia", "query": "from:giulia" })).expect("save");
    app.refresh_saved_searches();
    let saved = app.saved_searches.iter().find(|saved| saved.name == "Da Giulia").cloned().expect("saved");
    let folder_before = app.folder.clone();
    let query_before = app.query.clone();
    let threads_before = app.threads_view;
    app.apply_saved_search(&saved.id);
    assert_eq!(app.query, "from:giulia");
    assert!(app.label_filter.is_none(), "applying a saved search clears the label filter");
    assert!(app.saved_folder_scope);
    assert!(!app.threads_view, "a saved search is a search view and starts flat");
    assert_eq!(app.applied_saved.as_deref(), Some(saved.id.as_str()));
    assert_eq!(app.folder, folder_before);
    app.cancel_search();
    assert_eq!(app.query, query_before);
    assert_eq!(app.label_filter.as_deref(), Some("lbl_1"));
    assert!(app.applied_saved.is_none());
    assert_eq!(app.threads_view, threads_before, "the view mode before the saved search comes back");

    app.clear_search();
    assert!(app.threads_view, "the empty search restores the folder's threaded default");
    app.apply_saved_search(&saved.id);
    assert!(!app.threads_view);
    app.cancel_search();
    assert!(app.threads_view, "Esc restores the folder view when the saved search started from it");

    app.engine.call("saved_search_create", json!({ "name": "Inviati", "query": "folder:sent" })).expect("folder query");
    app.refresh_saved_searches();
    let folder_saved = app.saved_searches.iter().find(|saved| saved.name == "Inviati").cloned().expect("saved");
    app.folder = "inbox".to_owned();
    app.apply_saved_search(&folder_saved.id);
    let (rows, _, _) = app.visible_messages();
    assert!(!rows.is_empty(), "the saved folder query returned nothing");
    assert!(rows.iter().all(|message| message.folder_id == "sent"), "the saved query's folder is the only folder filter");
}

#[test]
fn saved_search_delete_offers_an_undo_that_restores_the_row() {
    let (_ctx, mut app) = conversations_app();
    app.engine.call("saved_search_create", json!({ "name": "Fatture", "query": "label:Fatture" })).expect("save");
    app.refresh_saved_searches();
    let id = app.saved_searches[0].id.clone();
    app.delete_saved_search(&id);
    assert!(app.saved_searches.is_empty());
    app.undo_last();
    assert_eq!(app.saved_searches.len(), 1);
    assert_eq!(app.saved_searches[0].name, "Fatture");
}

#[test]
fn saved_search_rename_is_undoable() {
    let (_ctx, mut app) = conversations_app();
    app.engine.call("saved_search_create", json!({ "name": "Prima", "query": "is:unread" })).expect("save");
    app.refresh_saved_searches();
    let id = app.saved_searches[0].id.clone();
    app.start_saved_search_rename(&id);
    if let Some(editor) = app.saved_editor.as_mut() {
        editor.name = "Dopo".to_owned();
    }
    app.commit_saved_search_editor();
    assert_eq!(app.saved_searches[0].name, "Dopo");
    app.undo_last();
    assert_eq!(app.saved_searches[0].name, "Prima");
}

#[test]
fn saved_search_drawer_saves_the_current_query_and_rejects_an_empty_name() {
    let (ctx, mut app) = conversations_app();
    app.query = "is:unread".to_owned();
    app.commit_search();
    app.start_saved_search_save();
    assert!(app.saved_editor.is_some());
    if let Some(editor) = app.saved_editor.as_mut() {
        editor.name.clear();
    }
    app.commit_saved_search_editor();
    assert!(app.saved_searches.is_empty(), "an empty name writes nothing");
    assert!(app.saved_editor.as_ref().and_then(|editor| editor.error.as_deref()).is_some());
    if let Some(editor) = app.saved_editor.as_mut() {
        editor.name = "Non letti".to_owned();
    }
    app.commit_saved_search_editor();
    assert_eq!(app.saved_searches.len(), 1);
    assert_eq!(app.saved_searches[0].name, "Non letti");
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("RICERCHE")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("Non letti")), "{text:?}");
}

#[test]
fn deleting_a_label_used_by_a_saved_search_marks_it_invalid_on_the_next_refresh() {
    let (_ctx, mut app) = conversations_app();
    app.engine.call("saved_search_create", json!({ "name": "Fatture", "query": "label:Fatture" })).expect("save");
    app.refresh_saved_searches();
    assert!(app.saved_searches[0].valid);
    app.engine.call("label_delete", json!({ "id": "lbl_3" })).expect("delete label");
    app.refresh_saved_searches_if_stale();
    assert!(!app.saved_searches[0].valid);
    assert!(!app.saved_searches[0].index_pending);
}

#[test]
fn applying_an_account_wide_saved_search_clears_chips_and_names_the_scope() {
    let (ctx, mut app) = conversations_app();
    app.unread_only = true;
    app.engine.call("saved_search_create", json!({ "name": "Da Giulia", "query": "from:giulia" })).expect("save");
    app.refresh_saved_searches();
    let saved = app.saved_searches[0].clone();
    app.apply_saved_search(&saved.id);
    assert!(!app.unread_only && !app.attachments_only, "the saved search replaces the chips too");
    assert!(app.saved_folder_scope && !app.saved_query_folder);
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Tutte le mail")), "the account-wide scope is not named: {text:?}");
}
