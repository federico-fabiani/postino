//! The server-side rules page.

use super::*;

#[test]
fn the_rules_page_renders_list_editor_and_preview_at_980_light_and_dark() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let mut app = PostinoApp::demo(true);
        app.seed_screenshot(ScreenshotState::Rules);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line == "REGOLE"), "the rules page header is missing: {text:?}");
        assert!(text.iter().any(|line| line.contains("Fatture")), "the seeded rule list is missing: {text:?}");
        assert!(text.iter().any(|line| line.contains("Salva")), "the editor actions are missing: {text:?}");
        assert!(text.iter().any(|line| line == "Rimuovi"), "the editor action row is missing: {text:?}");
        assert!(text.iter().any(|line| line == "Etichetta"), "the seeded action kind is missing: {text:?}");
        assert!(!text.iter().any(|line| line.contains("rule_run_")), "a raw run id reached the page: {text:?}");
        assert!(!text.iter().any(|line| line.contains(" · off")), "an English state reached the page: {text:?}");
        let page = app.rules_page.as_ref().expect("the rules page is open");
        assert!(!page.is_forward_allowed(), "the demo has no SMTP outbox and must disable forward with a reason");

        let mut app = PostinoApp::demo(true);
        app.seed_screenshot(ScreenshotState::RulePreview);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Fatture e ricevute")), "the editor name is missing: {text:?}");
        assert!(text.iter().any(|line| line.contains("ANTEPRIMA")), "the preview section is missing: {text:?}");
        assert!(text.iter().any(|line| line.contains("applica")), "the preview effects are missing: {text:?}");
    }
}

#[test]
fn the_rules_page_owns_the_keyboard_and_escapes_back() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    let rows = app.visible_messages().0;
    let id = rows[0].id.clone();
    app.open_message(id.clone());
    let before = app.engine.message(&id).expect("message");
    let selected_before = app.selected.clone();
    assert!(app.reading_open);
    frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::K, Modifiers::COMMAND)]);
    assert!(app.show_palette);
    app.palette_query = "regole".to_owned();
    app.palette_selected = 0;
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(app.rules_page.is_some(), "the palette command must open the rules page");

    for pressed in [Key::E, Key::Delete, Key::Z, Key::J] {
        frame(&ctx, &mut app, vec![key(pressed, Modifiers::NONE)]);
    }
    let after = app.engine.message(&id).expect("message");
    assert_eq!(after.summary.folder_id, before.summary.folder_id, "a shell shortcut archived/deleted mail behind the page");
    assert_eq!(after.summary.unread, before.summary.unread, "a shell shortcut changed the hidden message");
    assert_eq!(app.selected, selected_before, "J moved the hidden selection");
    assert!(app.reading_open, "the hidden reading pane closed");

    frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.rules_page.is_none(), "Esc closes the rules page");
    assert!(!app.show_palette);
    assert!(app.reading_open, "Esc must close only the rules page, not the reading pane");
    assert_eq!(app.selected, selected_before);
}
