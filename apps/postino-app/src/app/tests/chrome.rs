//! Dates, screenshots, the status bar, toasts and the other shell chrome.

use super::*;

#[test]
fn the_compact_date_fits_its_column() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(400.0, 200.0))), ..Default::default() };
    let mut width = 0.0;
    let mut output = ctx.run_ui(raw, |ui| {
        let galley = ui.painter().layout_no_wrap("24/10/26".to_owned(), FontId::new(type_size::CAPTION, FontFamily::Monospace), Color32::WHITE);
        width = galley.size().x;
    });
    output.textures_delta.clear();
    assert!(width <= ui_size::LIST_DATE, "dd/MM/yy measures {width} pt, more than the {} pt column", ui_size::LIST_DATE);
}

#[test]
fn screenshot_states_parse_and_reject_unknown_labels() {
    for (name, state) in [
        ("shell", ScreenshotState::Shell),
        ("reading", ScreenshotState::Reading),
        ("reading_full", ScreenshotState::ReadingFull),
        ("compose", ScreenshotState::Compose),
        ("palette", ScreenshotState::Palette),
        ("labels", ScreenshotState::Labels),
        ("label_menu", ScreenshotState::LabelMenu),
        ("label_editor", ScreenshotState::LabelEditor),
        ("empty_unread", ScreenshotState::EmptyUnread),
        ("empty_attachments", ScreenshotState::EmptyAttachments),
        ("empty_label_unread", ScreenshotState::EmptyLabelUnread),
        ("onboarding_welcome", ScreenshotState::OnboardingWelcome),
        ("onboarding_provider", ScreenshotState::OnboardingProvider),
        ("onboarding_form", ScreenshotState::OnboardingForm),
        ("onboarding_error", ScreenshotState::OnboardingError),
        ("onboarding_folders", ScreenshotState::OnboardingFolders),
        ("onboarding_sync", ScreenshotState::OnboardingSync),
        ("mailbox", ScreenshotState::Mailbox),
        ("offline", ScreenshotState::Offline),
        ("reading_attachments", ScreenshotState::ReadingAttachments),
        ("reading_invite", ScreenshotState::ReadingInvite),
        ("link_confirm", ScreenshotState::LinkConfirm),
        ("settings", ScreenshotState::Settings),
        ("settings_signature", ScreenshotState::SettingsSignature),
        ("outbox", ScreenshotState::Outbox),
        ("search", ScreenshotState::Search),
        ("search_error", ScreenshotState::SearchError),
        ("rules", ScreenshotState::Rules),
        ("rule_editor", ScreenshotState::RuleEditor),
        ("rule_preview", ScreenshotState::RulePreview),
    ] {
        assert_eq!(ScreenshotState::parse(name), Ok(state), "{name} must be a known scene");
    }
    let error = ScreenshotState::parse("nope").expect_err("nope is not a scene");
    assert!(error.contains("unknown screenshot state"), "{error}");
    assert!(error.contains("empty_label_unread"), "{error}");
}

#[test]
fn the_screenshot_clock_is_a_fixed_instant() {
    let frozen = screenshot_clock().expect("the screenshot clock parses");
    assert_eq!(frozen.format("%Y-%m-%dT%H:%M:%SZ").to_string(), SCREENSHOT_CLOCK);
    let app = PostinoApp::demo_at(true, frozen);
    assert_eq!(app.now_utc, frozen);
    assert_eq!(app.engine.messages()[0].date, frozen - chrono::Duration::minutes(8), "demo dates must follow the seam");
}

#[test]
fn frozen_clock_formats_dates_without_the_wall_clock() {
    let frozen = screenshot_clock().expect("the screenshot clock parses");
    let date = DateTime::parse_from_rfc3339("2026-10-24T09:30:00Z").expect("the fixture date parses").with_timezone(&Utc);
    let first = format_when(date, frozen);
    // Real time moving on must not change what the same message renders as.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(format_when(date, frozen), first, "the real clock must not leak into a screenshot date");
    let same_day = format_when(date, date + chrono::Duration::minutes(5));
    assert_eq!(same_day, date.with_timezone(&Local).format("%H:%M").to_string(), "a same-day instant keeps the time format");
    assert_ne!(format_when(date, date + chrono::Duration::days(400)), first, "another year shows the full date");
}

#[test]
fn screenshot_mode_never_advances_the_clock() {
    let frozen = screenshot_clock().expect("the screenshot clock parses");
    let mut app = PostinoApp::demo_at(true, frozen);
    app.screenshot = Some(ScreenshotMode { out: PathBuf::from("unused.png"), frames: 0, requested: false, waited: 0 });
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    frame(&ctx, &mut app, Vec::new());
    assert_eq!(app.now_utc, frozen, "a screenshot must render against its own clock, not the wall clock");
}

#[test]
fn png_writer_round_trips_a_two_by_two_image() {
    let pixels = [Color32::from_rgb(23, 61, 50), Color32::from_rgb(245, 235, 214), Color32::from_rgb(228, 185, 106), Color32::from_rgb(0, 0, 0)];
    let image = egui::ColorImage::new([2, 2], pixels.to_vec());
    let directory = std::env::temp_dir().join("postino-app-p01-tests");
    std::fs::create_dir_all(&directory).expect("a private temp folder");
    let path = directory.join(format!("png-round-trip-{}.png", std::process::id()));
    write_png(&path, &image).expect("the PNG writer");

    let file = std::fs::File::open(&path).expect("the written PNG");
    let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().expect("a PNG reader");
    let mut buffer = vec![0; reader.output_buffer_size().expect("an RGBA buffer")];
    let info = reader.next_frame(&mut buffer).expect("the PNG frame");
    assert_eq!((info.width, info.height), (2, 2));
    let expected = pixels.iter().flat_map(|pixel| pixel.to_array()).collect::<Vec<_>>();
    assert_eq!(&buffer[..info.buffer_size()], &expected[..]);
    assert!(!PathBuf::from(format!("{}.tmp", path.display())).exists(), "the atomic write must remove its temp file");
    std::fs::remove_file(&path).expect("remove the test image");
}

#[test]
fn unread_header_is_singular_for_one() {
    assert_eq!(unread_text(1), "1 non letto");
    assert_eq!(unread_text(0), "0 non letti");
    assert_eq!(unread_text(3), "3 non letti");
    assert_eq!(messages_text(1), "1 messaggio");
    assert_eq!(messages_text(6), "6 messaggi");
}

#[test]
fn label_errors_are_italian_ui_copy() {
    assert_eq!(
        label_error_message("invalid arguments: a label named \"lavoro\" already exists; label names are unique ignoring case"),
        "Esiste già un'etichetta con questo nome."
    );
    assert_eq!(label_error_message("invalid arguments: label names are 1-48 characters"), "Il nome deve avere da 1 a 48 caratteri.");
    assert!(!label_error_message("failed for an unknown reason").contains("failed"));
}

#[test]
fn responsive_breakpoints_keep_the_list_usable() {
    let pane = reading_pane_width(1280.0);
    assert!((pane - (1280.0 - ui_size::SIDEBAR) * 0.52).abs() < 0.01, "the pane opens at 52% of the width left by the sidebar");
    assert!(1280.0 - ui_size::SIDEBAR - pane >= ui_size::LIST_MIN, "the list keeps its 460 pt floor");
    assert!(
        1280.0 - ui_size::SIDEBAR - reading_pane_max_width(1280.0) >= ui_size::LIST_MIN,
        "even a full drag of the pane cannot squeeze the list below 460"
    );
    assert!(pane < 1280.0 * 0.52, "the old window-relative 52% left a 374 pt list");
    assert_eq!(reading_pane_width(800.0), READING_PANE_MIN_SIZE, "it never goes below 420");
    assert!(reading_pane_width(1280.0) <= (1280.0 - ui_size::SIDEBAR) * READING_PANE_MAX_RATIO);
    assert!(reading_overlay(980.0), "980x600 must use the overlay");
    assert!(!reading_overlay(1000.0), "1000 px switches to the side pane");
    assert!(!show_list_avatars(719.0));
    assert!(show_list_avatars(720.0));
    // The compact date stays until the panel itself is below 480 pt.
    assert_eq!(list_columns(479.0).date, None, "below 480 the date column is dropped before the subject");
    assert_eq!(list_columns(479.0).sender, ui_size::LIST_SENDER_NARROW);
    assert_eq!(list_columns(480.0).date, Some(ui_size::LIST_DATE));
    assert_eq!(list_columns(559.0).sender, ui_size::LIST_SENDER_NARROW);
    assert_eq!(list_columns(560.0).sender, ui_size::LIST_SENDER);
    assert_eq!(list_columns(1280.0).date, Some(52.0), "the compact date needs 52 pt");
    // The 1280×720 reading split leaves a 499 pt list; the subject keeps its floor there.
    let list = 1280.0 - ui_size::SIDEBAR - reading_pane_width(1280.0);
    assert!((list - 499.0).abs() < 2.0, "the list is about 499 pt, got {list}");
    assert!(subject_width(list) >= 260.0, "the subject gets {} pt at a 499 pt list", subject_width(list));
}

#[test]
fn selected_label_dots_keep_their_identity() {
    let light = theme::tokens(false);
    assert_eq!(label_dot(LabelColor::Gold, &light, true), light.gold);
    assert_eq!(label_dot(LabelColor::Forest, &light, true), light.selected_text);
    let seeded = [LabelColor::Gold, LabelColor::Success, LabelColor::Danger];
    for (index, color) in seeded.iter().enumerate() {
        let dot = label_dot(*color, &light, true);
        assert!(crate::ui::tokens::contrast_ratio(dot, light.selected) >= 3.0, "{color:?} dot is invisible on the selected row");
        assert!(
            seeded.iter().enumerate().all(|(other, candidate)| other == index || label_dot(*candidate, &light, true) != dot),
            "the three seeded label colours must stay distinct on selection"
        );
    }
    assert_eq!(label_dot(LabelColor::Success, &light, false), light.success);
}

#[test]
fn reduced_motion_turns_every_animation_off() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    theme::apply_motion(&ctx, false);
    assert!(theme::motion_seconds(&ctx, 0.14) > 0.0);
    assert!((ctx.global_style().animation_time - theme::ANIMATION_TIME).abs() < f32::EPSILON);

    theme::apply_motion(&ctx, true);
    assert_eq!(theme::motion_seconds(&ctx, 0.14), 0.0);
    assert_eq!(ctx.global_style().animation_time, 0.0);
}

#[test]
fn the_status_bar_has_three_honest_states() {
    assert_eq!(status_kind(true, false), StatusKind::Demo);
    assert_eq!(status_kind(false, false), StatusKind::Ready);
    assert_eq!(status_kind(true, true), StatusKind::Error);
    assert_eq!(status_kind(false, true), StatusKind::Error);

    let mut app = PostinoApp::demo(true);
    assert_eq!(app.status_kind(), StatusKind::Demo);
    app.demo = false;
    app.sync_view = SyncView::Offline { last_sync: Some(1_700_000_000_000) };
    assert_eq!(app.status_kind(), StatusKind::Offline);
    app.sync_view = SyncView::Error { reason: "credenziali rifiutate".to_owned(), last_sync: None };
    assert_eq!(app.status_kind(), StatusKind::Error);
    app.sync_view = SyncView::Idle { last_sync: None };
    app.demo = true;
    app.report_failure("Non è stato possibile leggere i messaggi. Riprova.", true);
    assert_eq!(app.status_kind(), StatusKind::Error);
    assert!(app.toast.is_none(), "an error is a toast, not an undo toast");
    app.dismiss_failure();
    assert_eq!(app.status_kind(), StatusKind::Demo);
    assert!(app.list_error_ack, "a dismissed list failure is not re-opened for the same view");
    app.query = "cambio-vista".to_owned();
    let _ = app.visible_messages();
    assert!(!app.list_error_ack, "a new filter is a fresh read and may report again");
    app.query.clear();
}

#[test]
fn an_error_is_one_dismissable_toast_and_never_a_raw_status_line() {
    let (ctx, mut app) = ready_app();
    // The toast slides in; the probe and the click must see the same settled rectangle.
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    let wide = Vec2::new(1200.0, 800.0);
    frame_at(&ctx, &mut app, wide, vec![]);
    let message = "Non è stato possibile aggiornare il messaggio.";
    app.report_failure(message, false);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    assert!(!app.status.contains(message), "the failure must not also become a status line: {}", app.status);
    crate::ui::widgets::toast_probe::clear(&ctx);
    frame_at(&ctx, &mut app, wide, vec![]);
    let buttons = crate::ui::widgets::toast_probe::take(&ctx);
    let dismiss = buttons.iter().find(|(label, _)| label == "Chiudi").map(|(_, rect)| *rect).expect("the error toast has a dismiss control");
    assert!(!buttons.iter().any(|(label, _)| label == "Riprova"), "a non-list failure has no meaningful retry");
    click_at(&ctx, &mut app, dismiss.center(), wide);
    assert!(app.failure.is_none(), "dismiss clears the failure");
    assert_eq!(app.status_kind(), StatusKind::Demo);

    // The list failure is the one case with a meaningful retry.
    app.report_failure("Non è stato possibile leggere i messaggi. Riprova.", true);
    frame_at(&ctx, &mut app, wide, vec![]);
    frame_at(&ctx, &mut app, wide, vec![]);
    crate::ui::widgets::toast_probe::clear(&ctx);
    frame_at(&ctx, &mut app, wide, vec![]);
    let buttons = crate::ui::widgets::toast_probe::take(&ctx);
    let retry = buttons.iter().find(|(label, _)| label == "Riprova").map(|(_, rect)| *rect).expect("the list failure offers a retry");
    click_at(&ctx, &mut app, retry.center(), wide);
    assert!(app.failure.is_none());
    assert_eq!(app.status, "Elenco aggiornato");
}

#[test]
fn the_loading_line_waits_for_the_delay_and_never_flickers() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    theme::apply_motion(&ctx, true);
    let mut app = PostinoApp::demo(true);
    let wide = Vec2::new(1200.0, 800.0);
    frame_time(&ctx, &mut app, wide, vec![], 0.0);

    app.begin_loading();
    frame_time(&ctx, &mut app, wide, vec![], 0.1);
    assert!(probe::loading_line(&ctx).is_none(), "a short operation must not flash the line");
    app.end_loading();

    // Work that stays open past the delay is shown, at full alpha under reduced motion.
    app.begin_loading();
    frame_time(&ctx, &mut app, wide, vec![], 0.31);
    let (rect, alpha) = probe::loading_line(&ctx).expect("the delayed loading line");
    assert!(rect.height() > 0.0);
    assert_eq!(alpha, 1.0, "reduced motion shows the line at once");
    app.end_loading();
    frame_time(&ctx, &mut app, wide, vec![], 0.42);
    assert!(probe::loading_line(&ctx).is_none());
}

#[test]
fn the_compact_date_formats_are_the_documented_ones() {
    let frozen = screenshot_clock().expect("the screenshot clock");
    let this_year = DateTime::parse_from_rfc3339("2026-03-04T08:15:00Z").expect("a date").with_timezone(&Utc);
    let older = DateTime::parse_from_rfc3339("2024-12-31T08:15:00Z").expect("a date").with_timezone(&Utc);
    assert_eq!(format_when(frozen, frozen), frozen.with_timezone(&Local).format("%H:%M").to_string(), "today shows HH:MM");
    assert_eq!(format_when(this_year, frozen), this_year.with_timezone(&Local).format("%d/%m").to_string(), "this year shows dd/MM");
    assert_eq!(format_when(older, frozen), older.with_timezone(&Local).format("%d/%m/%y").to_string(), "older shows dd/MM/yy");
    assert_eq!(format_when(older, frozen).len(), 8, "the compact year is two digits");
}

#[test]
fn there_is_exactly_one_demo_notice_and_no_milestone_code_in_the_copy() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    for source in ["app.rs", "ui/widgets/composer.rs", "ui/gallery.rs", "rules_ui.rs"] {
        let path = format!("{root}/{source}");
        let text = std::fs::read_to_string(&path).expect("the source file");
        let production = text.split_once("\nmod tests {").map_or(text.as_str(), |(before, _)| before);
        for forbidden in ["M0 + U0.5", "Locale · offline", "HTML in M5", "metadati in M0", "memoria fino a M1", "dopo U0.5", "store M1", "SMTP (P7)"]
        {
            assert!(!production.contains(forbidden), "{source} still contains {forbidden:?}");
        }
    }
    let demo = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../crates/core/src/demo.rs")).expect("the demo data");
    assert!(!demo.contains("modalità M0"), "the demo body still names the milestone");
}

#[test]
fn every_label_colour_is_legible_on_ivory_and_on_forest() {
    let light = theme::tokens(false);
    let dark = theme::tokens(true);
    for (color, name) in LABEL_COLORS {
        let on_ivory = crate::ui::tokens::contrast_ratio(label_color(color, &light), theme::IVORY);
        assert!(on_ivory >= 3.0, "{name} on ivory is {on_ivory:.2}:1");
        let on_forest = crate::ui::tokens::contrast_ratio(label_dot(color, &light, true), theme::FOREST);
        assert!(on_forest >= 3.0, "{name} on the selected forest row is {on_forest:.2}:1");
        let on_dark = crate::ui::tokens::contrast_ratio(label_color(color, &dark), theme::DARK_PANEL);
        assert!(on_dark >= 3.0, "{name} on the dark panel is {on_dark:.2}:1");
    }
    assert_eq!(label_color(LabelColor::Gold, &light), theme::GOLD_INK, "gold uses its ink on a light surface");
    assert_eq!(label_color(LabelColor::Gold, &dark), theme::GOLD, "the dark theme keeps the light gold");
}

#[test]
fn the_shell_paints_one_product_wordmark() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    app.seed_screenshot(ScreenshotState::Shell);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    for size in [Vec2::new(980.0, 600.0), Vec2::new(1280.0, 720.0)] {
        let text = painted_text(&ctx, &mut app, size);
        assert_eq!(text.iter().filter(|line| line.as_str() == "postino").count(), 1, "two product marks at {size:?}: {text:?}");
    }
}
