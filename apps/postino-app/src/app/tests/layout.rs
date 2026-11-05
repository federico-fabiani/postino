//! Responsive layout: the narrow window, modal fit and the cards at the minimum size.

use super::*;

#[test]
fn narrow_window_uses_the_reading_overlay_and_returns_to_the_list() {
    let (ctx, mut app) = ready_app();
    let narrow = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, narrow, vec![]);
    let id = app.visible_messages().0[0].id.clone();
    app.open_message(id.clone());
    frame_at(&ctx, &mut app, narrow, vec![]);
    assert!(app.reading_open, "clicking a row opens the overlay");
    assert!(!app.reading_full, "the overlay is derived from the width, not a separate mode");
    frame_at(&ctx, &mut app, narrow, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.reading_open, "Esc returns to the list");
    assert_eq!(app.selected.as_deref(), Some(id.as_str()), "the row stays selected");
}

#[test]
fn narrow_reading_overlay_keeps_triage_actions() {
    let (ctx, mut app) = ready_app();
    let narrow = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, narrow, vec![]);
    let id = app.visible_messages().0[0].id.clone();
    app.open_message(id);
    frame_at(&ctx, &mut app, narrow, vec![]);
    let actions = probe::actions(&ctx);
    assert!(actions.contains(&"back"), "the overlay keeps Back: {actions:?}");
    assert!(actions.contains(&"archive"), "the overlay keeps Archivia: {actions:?}");
    assert!(actions.contains(&"delete"), "the overlay keeps Elimina: {actions:?}");

    app.close_reading();
    frame(&ctx, &mut app, vec![]);
    let wide = app.visible_messages().0[0].id.clone();
    app.open_message(wide);
    frame(&ctx, &mut app, vec![]);
    let actions = probe::actions(&ctx);
    assert!(actions.contains(&"archive") && actions.contains(&"delete"), "the wide pane keeps its triage actions: {actions:?}");
}

#[test]
fn narrow_sidebar_keeps_the_editor_between_labels_and_searches() {
    let (ctx, mut app) = ready_app();
    let narrow = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, narrow, vec![]);

    app.start_label_create();
    for _ in 0..20 {
        frame_at(&ctx, &mut app, narrow, vec![]);
    }

    let view = probe::scroll_view(&ctx).expect("the sidebar scroll area");
    let editor = probe::editor_rect(&ctx).expect("the editor content");
    let labels_header = probe::sidebar_header_rect(&ctx, "ETICHETTE").expect("the ETICHETTE header");
    let searches_header = probe::sidebar_header_rect(&ctx, "RICERCHE").expect("the RICERCHE header");
    let save = probe::saved_at(&ctx).expect("the editor save button");
    let cancel = probe::cancel_at(&ctx).expect("the editor cancel button");

    assert!(view.expand2(Vec2::new(2.0, 0.0)).contains_rect(editor), "the inline editor stays in the scrolling list: {editor:?} vs {view:?}");
    assert!(editor.contains_rect(save) && editor.contains_rect(cancel), "Save and Cancel are inside the editor card");
    assert!(labels_header.max.y <= editor.min.y + 0.5, "the editor follows the label rows: {editor:?} vs {labels_header:?}");
    assert!(searches_header.min.y >= editor.max.y - 0.5, "the searches section follows the editor: {editor:?} vs {searches_header:?}");

    // The error line is the tallest editor state; it must still fit without clipping the controls.
    if let Some(editor) = app.label_editor.as_mut() {
        editor.text = "Lavoro".to_owned();
        editor.error = Some("Esiste già un'etichetta con questo nome.".to_owned());
    }
    app.scroll_editor_into_view = true;
    for _ in 0..4 {
        frame_at(&ctx, &mut app, narrow, vec![]);
    }
    let editor = probe::editor_rect(&ctx).expect("the editor with an error");
    let save = probe::saved_at(&ctx).expect("the editor save button with an error");
    let view = probe::scroll_view(&ctx).expect("the sidebar scroll area with an error");
    assert!(editor.contains_rect(save), "an error line must not clip Save: {save:?} vs {editor:?}");
    assert!(view.expand2(Vec2::new(2.0, 0.0)).contains_rect(editor), "the error state still fits the sidebar viewport: {editor:?} vs {view:?}");

    let content = probe::scroll_content(&ctx).expect("the scroll content size");
    assert!(content.y > view.height(), "with the editor open the sidebar overflows, so its bar is available");

    // The editor is part of the flow, not a pinned drawer: scrolling still reaches the last row.
    frame_at(&ctx, &mut app, narrow, wheel(view.center(), 80));
    frame_at(&ctx, &mut app, narrow, vec![]);
    let scrolled = probe::rows(&ctx);
    let fatture = scrolled.get("Fatture").copied().expect("the last label row is still drawn");
    assert!(fatture.bottom() <= view.bottom() + 1.0, "a wheel scroll brings the last label row into view: {fatture:?} vs {view:?}");
}

/// The narrow, tall rect egui painted for the sidebar's vertical scroll bar, in the bar lane at
/// the right edge of the scroll area. egui always emits the bar shapes while the area can scroll,
/// with alpha 0 while dormant, so the alpha tells a bar the user can feel from one only a test
/// can see.
fn painted_sidebar_bar(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, events: Vec<egui::Event>) -> Option<(Rect, egui::Color32)> {
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), events, ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    output.textures_delta.clear();
    let panel = probe::sidebar_panel_rect(ctx).expect("the sidebar panel");
    let view = probe::scroll_view(ctx).expect("the sidebar scroll area");
    fn collect(shape: &egui::epaint::Shape, panel: Rect, view: Rect, bars: &mut Vec<(Rect, egui::Color32)>) {
        match shape {
            egui::epaint::Shape::Rect(rect) => {
                let painted = rect.rect;
                if painted.center().x >= view.right() - 14.0 && painted.width() <= 12.0 && painted.height() >= 20.0 && panel.contains_rect(painted) {
                    bars.push((painted, rect.fill));
                }
            }
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, panel, view, bars);
                }
            }
            _ => {}
        }
    }
    let mut bars = Vec::new();
    for clipped in &output.shapes {
        collect(&clipped.shape, panel, view, &mut bars);
    }
    bars.into_iter().find(|(_, fill)| fill.a() > 0)
}

/// The folder/label sidebar scrolls like the list and the reading pane: no local bar override, so
/// nothing is painted while the pointer is elsewhere, a thin sliver appears on hover and the same
/// handle thickens over the bar lane and while dragging. The 1280×900 demo fits; 980×600 does not.
#[test]
fn sidebar_scroll_bar_follows_the_shared_visible_when_needed_behaviour() {
    for (size, overflows) in [(Vec2::new(1280.0, 900.0), false), (Vec2::new(980.0, 600.0), true)] {
        for dark in [false, true] {
            let (ctx, mut app) = ready_app();
            theme::apply(&ctx, dark);
            app.dark = dark;
            ctx.all_styles_mut(|style| style.animation_time = 0.0);
            settle_at(&ctx, &mut app, size);
            let view = probe::scroll_view(&ctx).expect("the sidebar scroll area");
            let content = probe::scroll_content(&ctx).expect("the scroll content size");
            assert_eq!(content.y > view.height(), overflows, "the demo sidebar overflow at {size:?} (dark={dark})");

            // The pointer rests in the mail area: a shared floating bar is dormant, not painted.
            let outside = sidebar_outside(view);
            assert!(
                painted_sidebar_bar(&ctx, &mut app, size, vec![egui::Event::PointerMoved(outside)]).is_none(),
                "the sidebar must not paint a bar before the pointer reaches it (dark={dark}, {size:?})"
            );

            // Poised over the folders, still away from the bar lane.
            let Some((resting, fill)) = painted_sidebar_bar(&ctx, &mut app, size, vec![egui::Event::PointerMoved(view.center())]) else {
                assert!(!overflows, "an overflowing sidebar shows its bar on hover (dark={dark}, {size:?})");
                continue;
            };
            assert!(overflows, "a sidebar that fits paints no bar, even hovered (dark={dark}): {resting:?}");
            assert!(resting.width() <= 4.0, "the resting bar is a thin sliver, not a slab (dark={dark}): {resting:?}");
            assert!(fill.a() > 0, "hovering the folders wakes the bar (dark={dark}), not leaves it dormant");

            // Over the bar lane itself the same handle thickens, exactly like the list and pane.
            let lane = pos2(resting.center().x, view.center().y);
            let (thick, _) = painted_sidebar_bar(&ctx, &mut app, size, vec![egui::Event::PointerMoved(lane)]).expect("the bar under the pointer");
            assert!(thick.width() > resting.width() + 2.0, "hovering the bar thickens it (dark={dark}): {thick:?} vs {resting:?}");

            // A pressed handle keeps that thickness instead of shrinking back to a sliver.
            frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(lane), pointer(lane, true)]);
            let (dragged, _) =
                painted_sidebar_bar(&ctx, &mut app, size, vec![egui::Event::PointerMoved(pos2(lane.x, lane.y - 30.0))]).expect("the dragged bar");
            assert!(dragged.width() > resting.width() + 2.0, "a dragged bar stays thick (dark={dark}): {dragged:?}");
        }
    }
}

/// A point in the mail area left of the sidebar, so the sidebar is not hovered.
fn sidebar_outside(view: Rect) -> egui::Pos2 {
    pos2(view.center().x + 400.0, view.center().y)
}

#[test]
fn composer_sheet_fits_the_minimum_window_and_scrolls_its_body() {
    let (ctx, mut app) = ready_app();
    let narrow = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, narrow, vec![]);
    app.compose_new();
    app.composer.subject = "Preventivo".to_owned();
    app.composer.body = (0..80).map(|line| format!("Riga {line} di una bozza lunga")).collect::<Vec<_>>().join("\n");
    app.composer.touch(0.0);
    for _ in 0..3 {
        frame_at(&ctx, &mut app, narrow, vec![]);
    }

    let content = ctx.content_rect();
    let body_area = Rect::from_min_max(content.min, pos2(content.max.x, content.max.y - ui_size::STATUS));
    let card = probe::composer_card_rect(&ctx).expect("the composer surface");
    assert!(card.width() > 900.0, "below 1000×700 the composer is a sheet: {card:?}");
    assert!(
        card.min.x >= body_area.min.x && card.max.x <= body_area.max.x,
        "the sheet stays inside the content rect horizontally: {card:?} vs {body_area:?}"
    );
    assert!(card.min.y >= body_area.min.y && card.max.y <= body_area.max.y, "the sheet never goes under the status bar: {card:?} vs {body_area:?}");

    let send = probe::composer_send_rect(&ctx).expect("the Send button");
    let cancel = probe::composer_cancel_rect(&ctx).expect("the Scarta button");
    for (name, rect) in [("Send", send), ("Cancel", cancel)] {
        assert!(
            rect.min.x >= card.min.x && rect.max.x <= card.max.x && rect.min.y >= card.min.y && rect.max.y <= card.max.y,
            "{name} must be inside the sheet: {rect:?} vs {card:?}"
        );
    }

    let (offset, viewport, body) = probe::composer_scroll_state(&ctx).expect("the composer body scroll");
    assert!(body.y > viewport.height() + 1.0, "a long draft makes the body scroll inside the sheet: {body:?} vs {viewport:?}");
    frame_at(&ctx, &mut app, narrow, wheel(viewport.center(), 6));
    frame_at(&ctx, &mut app, narrow, vec![]);
    let (scrolled, _, _) = probe::composer_scroll_state(&ctx).expect("the composer body scroll");
    assert!(scrolled > offset, "the wheel moves the body, not the header or the footer: {scrolled} vs {offset}");
}

#[test]
fn composer_modal_blocks_the_background_and_escape_saves_or_stops() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1280.0, 720.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    let row = probe::message_rows(&ctx).values().next().copied().expect("a message row");
    app.compose_new();
    app.composer.subject = "Bozza aperta".to_owned();
    app.composer.touch(0.0);
    for _ in 0..3 {
        frame_at(&ctx, &mut app, wide, vec![]);
    }

    // Left of the centred card is the scrim. The row behind must not open.
    click_at(&ctx, &mut app, pos2(row.left() + 8.0, row.center().y), wide);
    assert!(app.composer.open, "clicking the scrim never closes the composer");
    assert!(app.selected.is_none(), "the scrim swallows the background click");
    assert_eq!(app.composer.subject, "Bozza aperta", "the background Scrivi button cannot replace the open composer");

    // An invalid recipient makes the unsaved guard refuse to close on Esc.
    app.composer.to = "non-una-mail".to_owned();
    app.composer.touch(0.0);
    frame_at(&ctx, &mut app, wide, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.composer.open, "Esc keeps an unsaved, invalid draft open");
    assert!(app.composer.save_error.is_some(), "the composer says why it did not save");
    assert!(!app.engine.messages().iter().any(|message| message.subject == "Bozza aperta"), "a refused save must not create the draft");

    app.composer = ComposerState::closed();
    app.compose_new();
    frame_at(&ctx, &mut app, wide, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.composer.open, "Esc closes an empty, unchanged composer");
}

#[test]
fn composer_focus_enters_the_first_empty_field_and_returns_on_close() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1200.0, 800.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    let button = probe::compose_button_rect(&ctx).expect("the toolbar Scrivi button");
    click_at(&ctx, &mut app, button.center(), wide);
    assert!(app.composer.open, "the toolbar button opens the composer");
    let previous = app.composer_return_focus.expect("the widget that opened the composer");
    assert!(ctx.text_edit_focused(), "focus enters the first empty field (A)");

    frame_at(&ctx, &mut app, wide, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.composer.open);
    frame_at(&ctx, &mut app, wide, vec![]);
    assert_eq!(ctx.memory(|memory| memory.focused()), Some(previous), "focus returns to the Scrivi button");
}

#[test]
fn palette_is_modal_and_closes_on_an_outside_click_without_touching_the_list() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1200.0, 800.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    let row = probe::message_rows(&ctx).values().next().copied().expect("a message row");
    app.show_palette = true;
    app.palette_focus = true;
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    assert!(app.show_palette);
    assert!(ctx.text_edit_focused(), "the palette keeps the keyboard in its search field");

    // The palette is centred at 560 pt: the row's left edge is outside it.
    click_at(&ctx, &mut app, pos2(row.left() + 4.0, row.center().y), wide);
    assert!(!app.show_palette, "an outside click closes the modal palette");
    assert!(app.selected.is_none(), "the background row never receives the click");
}

#[test]
fn palette_arrows_keep_focus_in_the_search_field() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.show_palette = true;
    app.palette_focus = true;
    frame(&ctx, &mut app, vec![]);
    assert!(ctx.text_edit_focused());
    frame(&ctx, &mut app, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    assert_eq!(app.palette_selected, 1, "arrows still move the selected command");
    assert!(ctx.text_edit_focused(), "the field keeps the keyboard after arrow navigation");
}

#[test]
fn palette_fits_the_minimum_window_with_ten_rows() {
    let (ctx, mut app) = ready_app();
    let narrow = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, narrow, vec![]);
    frame_at(&ctx, &mut app, narrow, vec![]);
    app.show_palette = true;
    app.palette_focus = true;
    app.palette_query.clear();
    for _ in 0..3 {
        frame_at(&ctx, &mut app, narrow, vec![]);
    }
    assert!(filtered_commands("").len() > 10, "the empty query needs the ten-row cap");
    let rect = probe::palette_rect(&ctx).expect("the palette surface");
    assert!(rect.width() <= narrow.x - 2.0 * space::XL + 0.5, "the palette is min(560, available - 2*XL): {rect:?}");
    assert!(rect.min.y >= 0.0 && rect.max.y <= narrow.y - ui_size::STATUS, "the palette stays above the status bar: {rect:?}");
    assert!(rect.height() <= 502.0, "ten rows are capped with an internal scroll: {rect:?}");
}

#[test]
fn reading_pane_keeps_the_list_wide_and_tooltips_the_truncated_subject() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1280.0, 720.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    let first = app.visible_messages().0[0].id.clone();
    app.open_message(first);
    frame_at(&ctx, &mut app, wide, vec![]);

    let (content, panel) = probe::list_widths(&ctx).expect("the list width");
    assert!(content >= ui_size::LIST_MIN, "the list content is {content} pt, below the 460 pt floor");
    assert!(panel >= content, "the panel includes the list margin: {panel} vs {content}");
    assert!((panel - 499.0).abs() < 2.0, "the reading split leaves a ~499 pt list at 1280×720, got {panel} (content {content})");
    assert_eq!(probe::date_column_visible(&ctx), Some(true), "a 499 pt list keeps the compact date column");
    let subject = probe::subject_width_value(&ctx).expect("the subject cell width");
    assert!(subject >= 260.0, "the subject gets {subject} pt at a {panel} pt list and {content} pt of content, below the 260 pt floor");

    // A long subject is the case the tooltip exists for. Drafts have a real subject and are a
    // folder the demo can add to deterministically.
    let long = "Un oggetto davvero lungo che non entra nella colonna del soggetto e deve essere troncato";
    let value = app.engine.call("draft_save", json!({ "subject": long, "body_text": "x" })).expect("a long draft");
    let draft = serde_json::from_value::<DraftSave>(value).expect("draft response").draft.summary.id;
    app.select_folder("drafts");
    frame_at(&ctx, &mut app, wide, vec![]);
    app.open_message(draft);
    app.focus_selected_row = true;
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    let tips = probe::tooltips(&ctx);
    assert!(tips.iter().any(|tip| tip == long), "a keyboard-focused truncated subject carries its full text as a tooltip: {tips:?}");

    app.close_reading();
    frame_at(&ctx, &mut app, Vec2::new(1600.0, 900.0), vec![]);
    assert_eq!(probe::date_column_visible(&ctx), Some(true), "the date returns once the list has room");
}

#[test]
fn double_click_opens_and_expands_and_the_reading_header_restores() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1200.0, 800.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    let id = app.visible_messages().0[0].id.clone();
    let row = probe::message_rows(&ctx).get(&id).copied().expect("the first message row");
    double_click_at(&ctx, &mut app, pos2(row.left() + 30.0, row.center().y), wide);
    assert!(app.reading_open, "a row double-click opens the message");
    assert!(app.reading_full, "a row double-click expands the reading pane on a wide window");

    frame_at(&ctx, &mut app, wide, vec![]);
    let header = probe::reading_header_rect(&ctx).expect("the reading header");
    double_click_at(&ctx, &mut app, header.center(), wide);
    assert!(app.reading_open && !app.reading_full, "a header double-click restores the split view");

    // On a narrow window the same gesture uses the full-width overlay and the header closes it.
    app.close_reading();
    let narrow = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, narrow, vec![]);
    frame_at(&ctx, &mut app, narrow, vec![]);
    frame_at(&ctx, &mut app, narrow, vec![]);
    let row = probe::message_rows(&ctx).get(&id).copied().expect("the first message row");
    // On a narrow window the first click already replaces the list with the full-width overlay;
    // keep the gesture away from the label chips the overlay shows at its left edge.
    let pos = pos2(row.left() + row.width() * 0.6, row.center().y);
    double_click_at(&ctx, &mut app, pos, narrow);
    assert!(app.reading_open, "a narrow row double-click opens the overlay");
    frame_at(&ctx, &mut app, narrow, vec![]);
    let header = probe::reading_header_rect(&ctx).expect("the overlay header");
    // The action strip owns the left half of the header on a narrow overlay; the empty gutter
    // before the right edge is the double-click target.
    double_click_at(&ctx, &mut app, pos2(header.right() - 100.0, header.center().y), narrow);
    assert!(!app.reading_open, "a header double-click on the overlay returns to the list (header {header:?}, full {})", app.reading_full);
}

#[test]
fn the_composer_keeps_its_640_card_and_the_footer_stays_separate_at_1280() {
    for dark in [false, true] {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        app.seed_screenshot(ScreenshotState::Compose);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let size = Vec2::new(1280.0, 720.0);
        settle_at(&ctx, &mut app, size);
        let card = probe::composer_card_rect(&ctx).expect("the composer card");
        let surface = probe::composer_surface_rect(&ctx).expect("the composer surface");
        let send = probe::composer_send_rect(&ctx).expect("the Send button");
        let cancel = probe::composer_cancel_rect(&ctx).expect("Scarta");
        let state = probe::composer_footer_state_rect(&ctx).expect("the saved state");
        assert!((card.width() - ui_size::COMPOSER_CARD_WIDTH).abs() < 1.0, "dark={dark}: card {card:?}");
        assert!(surface.width() <= card.width() + 4.5, "dark={dark}: the modal grew past its card: {surface:?} {card:?}");
        assert!(surface.contains_rect(card), "dark={dark}: the card escaped the modal area: {surface:?} {card:?}");
        assert!((card.center().x - ctx.content_rect().center().x).abs() < 1.0, "dark={dark}: {card:?}");
        assert!(card.contains_rect(send), "dark={dark}: Send {send:?} is outside {card:?}");
        assert!(card.contains_rect(cancel), "dark={dark}: Scarta {cancel:?} is outside {card:?}");
        assert!(state.right() <= cancel.left(), "dark={dark}: saved state {state:?} runs under Scarta {cancel:?}");
    }
}

#[test]
fn the_outbox_card_keeps_its_footer_inside_at_980_in_both_themes() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let mut app = PostinoApp::demo_at(true, Utc::now());
        app.outbox = demo_outbox(app.now_utc.timestamp_millis());
        app.outbox_open = true;
        app.outbox_loaded = true;
        let size = Vec2::new(980.0, 600.0);
        settle_at(&ctx, &mut app, size);
        let card = probe::outbox_card_rect(&ctx).expect("the outbox card");
        let footer = probe::outbox_footer_rect(&ctx).expect("the outbox footer");
        assert!(ctx.content_rect().contains_rect(card), "dark={dark}: card {card:?}");
        assert!(card.contains_rect(footer), "dark={dark}: footer {footer:?} is outside card {card:?}");
        assert!(card.bottom() <= size.y - ui_size::STATUS, "dark={dark}: the card reaches the status bar: {card:?}");
        let text = painted_text(&ctx, &mut app, size);
        assert!(text.iter().any(|line| line.contains("richiede Postino aperto")), "dark={dark}: {text:?}");
        assert!(text.iter().any(|line| line.contains("Postino invia solo mentre")), "dark={dark}: {text:?}");
    }
}

#[test]
fn a_list_row_paints_the_focus_ring_only_while_it_is_not_selected() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let gold = theme::tokens(false).gold;
    let size = Vec2::new(460.0, 160.0);
    for (selected, focused, expect_ring) in [(true, true, false), (true, false, false), (false, true, true)] {
        let row_rect = std::cell::Cell::new(Rect::NOTHING);
        let mut rings = Vec::new();
        for _ in 0..2 {
            let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                let row = ListRow::thread("Preventivo sito web", "Giulia Ferri, Federico · 3 messaggi", "01/11").selected(selected).focused(focused);
                row_rect.set(row.show(ui).rect);
            });
            output.textures_delta.clear();
            rings.clear();
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
            for clipped in &output.shapes {
                collect(&clipped.shape, gold, &mut rings);
            }
        }
        let target = row_rect.get().expand(2.0);
        let ring = rings.iter().any(|rect| (rect.min - target.min).length() < 0.5 && (rect.max - target.max).length() < 0.5);
        assert_eq!(ring, expect_ring, "selected={selected} focused={focused}: rings {rings:?} around {target:?}");
    }
}
