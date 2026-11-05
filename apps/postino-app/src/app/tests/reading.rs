//! The reading pane: attachments, links, invites, block bodies and remote content.

use crate::reading;

use super::*;

#[test]
fn a_conversation_attachment_save_stays_inside_the_980_window_in_both_themes() {
    for dark in [false, true] {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        app.seed_screenshot(ScreenshotState::Conversation);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let size = Vec2::new(980.0, 600.0);
        settle_at(&ctx, &mut app, size);
        let content = ctx.content_rect();
        let strip = probe::reading_attachments_rect(&ctx).expect("the conversation attachment strip");
        let save = probe::attachment_save_rect(&ctx, "att_001").expect("the attachment Save");
        let body = probe::reading_body_rect(&ctx).expect("the reading body");
        assert!(content.contains_rect(strip), "dark={dark}: strip {strip:?} leaves the window");
        assert!(content.contains_rect(save), "dark={dark}: Save {save:?} leaves the window");
        assert!(strip.contains_rect(save), "dark={dark}: Save {save:?} is outside the strip {strip:?}");
        assert!(strip.height() >= 80.0, "dark={dark}: the strip cannot show its row: {strip:?}");
        assert!(body.bottom() <= strip.top(), "dark={dark}: the body viewport overlaps the strip: {body:?} {strip:?}");
        assert!(save.bottom() <= size.y - ui_size::STATUS, "dark={dark}: Save is under the status bar: {save:?}");
    }
}

#[test]
fn attachment_sizes_use_the_italian_decimal_comma() {
    assert_eq!(format_size(248_320), "242,5 KB");
}

#[test]
fn preview_helpers_bound_text_and_replace_bidi_controls() {
    let (text, truncated) = preview_text(&vec![b'a'; MAX_PREVIEW_TEXT + 16]);
    assert!(truncated);
    assert_eq!(text.len(), MAX_PREVIEW_TEXT);
    let (hostile, _) = preview_text("titolo \u{202e}eseguito".as_bytes());
    assert!(hostile.contains('\u{FFFD}'), "bidi controls must not reorder the sheet: {hostile:?}");
    assert!(!hostile.contains('\u{202e}'));
}

#[test]
fn attachment_preview_shows_bounded_text_at_980_in_both_themes_without_saving() {
    let (mut app, message_id, attachment) = seeded_attachment_app("p6c-preview-text", "nota.txt", Some("riga uno\nriga due".as_bytes()));
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    for dark in [false, true] {
        theme::apply(&ctx, dark);
        app.open_message(message_id.clone());
        settle_at(&ctx, &mut app, Vec2::new(980.0, 600.0));
        let preview = probe::attachment_preview_rect(&ctx, &attachment.id).expect("the Anteprima button");
        click_at(&ctx, &mut app, preview.center(), Vec2::new(980.0, 600.0));
        let state = app.reading.attachment_preview.as_ref().expect("the preview sheet");
        assert_eq!(state.text.as_deref(), Some("riga uno\nriga due"), "dark={dark}");
        assert!(state.error.is_none(), "dark={dark}: {:?}", state.error);
        let modal = probe::attachment_preview_modal_rect(&ctx).expect("the preview modal");
        let content = ctx.content_rect();
        assert!(content.contains_rect(modal), "dark={dark}: the preview is off-screen: {modal:?} vs {content:?}");
        app.close_reading();
        assert!(app.reading.attachment_preview.is_none(), "closing the reading pane must drop the preview");
    }
}

#[test]
fn attachment_preview_decodes_a_small_png_and_explains_an_unsupported_pdf() {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&[255, 0, 0, 255]).expect("png data");
    }
    let (mut app, message_id, attachment) = seeded_attachment_app("p6c-preview-png", "pixel.png", Some(&png));
    app.open_message(message_id);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    settle_at(&ctx, &mut app, Vec2::new(980.0, 600.0));
    let preview = probe::attachment_preview_rect(&ctx, &attachment.id).expect("the Anteprima button");
    click_at(&ctx, &mut app, preview.center(), Vec2::new(980.0, 600.0));
    assert!(app.reading.attachment_preview.as_ref().and_then(|state| state.texture.as_ref()).is_some(), "the PNG texture");

    let (mut pdf_app, pdf_message, pdf_attachment) = seeded_attachment_app("p6c-preview-pdf", "report.pdf", Some(b"%PDF-1.7"));
    pdf_app.open_message(pdf_message);
    settle_at(&ctx, &mut pdf_app, Vec2::new(980.0, 600.0));
    let preview = probe::attachment_preview_rect(&ctx, &pdf_attachment.id).expect("the PDF preview button");
    click_at(&ctx, &mut pdf_app, preview.center(), Vec2::new(980.0, 600.0));
    let state = pdf_app.reading.attachment_preview.as_ref().expect("the PDF preview sheet");
    assert!(state.error.as_deref().is_some_and(|error| error.contains("Anteprima non disponibile")), "{:?}", state.error);
}

#[test]
fn saving_an_attachment_writes_exact_blob_bytes() {
    let path = directory("save-exact").join("report.pdf");
    let _ = std::fs::remove_file(&path);
    let (mut app, message_id, attachment) = seeded_attachment_app("p6c-save-exact", "report.pdf", Some(b"blob esatto\nseconda riga"));
    let saver = RecordingFileSaver::new();
    saver.set_path(Some(path.clone()));
    app.file_saver = saver.clone();
    app.open_message(message_id);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    settle_at(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    let save = probe::attachment_save_rect(&ctx, &attachment.id).expect("the attachment Save button");
    click_at(&ctx, &mut app, save.center(), Vec2::new(1200.0, 800.0));
    assert!(!app.reading.attachment_warning.is_some(), "a PDF needs no warning");
    assert!(!app.reading.attachment_overwrite.is_some(), "a new path needs no overwrite confirmation");
    assert_eq!(std::fs::read(&path).expect("the saved file"), b"blob esatto\nseconda riga");
    assert_eq!(saver.writes(), vec![(path.clone(), b"blob esatto\nseconda riga".to_vec())]);
    assert_eq!(app.toast.as_ref().map(|toast| toast.kind), Some(ToastKind::Success));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_existing_file_is_not_overwritten_until_confirmed() {
    let path = directory("save-overwrite").join("report.pdf");
    std::fs::write(&path, b"vecchio contenuto").expect("an existing file");
    let (mut app, _, attachment) = seeded_attachment_app("p6c-overwrite", "report.pdf", Some(b"nuovo contenuto"));
    let saver = RecordingFileSaver::new();
    saver.set_path(Some(path.clone()));
    app.file_saver = saver.clone();
    app.request_attachment_save(&attachment);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Sostituire il file?")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("I dati del file esistente saranno persi")), "the overwrite body must name the loss: {text:?}");
    assert_eq!(std::fs::read(&path).expect("old content is intact"), b"vecchio contenuto");
    assert!(saver.writes().is_empty());
    click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Sostituire il file?", true);
    assert_eq!(std::fs::read(&path).expect("new content"), b"nuovo contenuto");
    assert_eq!(app.toast.as_ref().map(|toast| toast.kind), Some(ToastKind::Success));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn dangerous_attachments_warn_before_choosing_a_path() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    for (filename, kind) in [("setup.exe\u{00A0}", DangerousKind::Executable), ("report.docm", DangerousKind::Macro)] {
        let path = directory("save-warning").join(format!("{kind:?}.bin"));
        let _ = std::fs::remove_file(&path);
        let (mut app, _, attachment) = seeded_attachment_app(&format!("p6c-warning-{kind:?}"), filename, Some(b"warning bytes"));
        let saver = RecordingFileSaver::new();
        saver.set_path(Some(path.clone()));
        app.file_saver = saver.clone();
        app.request_attachment_save(&attachment);
        let warning = app.reading.attachment_warning.as_ref().expect("a warning before the native dialog");
        assert_eq!(warning.dangerous, Some(kind));
        assert_eq!(saver.choose_calls(), 0, "the path must not be chosen before the warning");
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Salvare un file potenzialmente pericoloso?")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("setup.exe") || line.contains("report.docm")), "{text:?}");
        assert!(text.iter().any(|line| line.contains(kind.message())), "the warning body must carry the reason: {text:?}");
        click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Salvare un file potenzialmente pericoloso?", true);
        assert_eq!(saver.choose_calls(), 1, "the path is chosen only after the warning");
        assert_eq!(std::fs::read(&path).expect("saved after confirmation"), b"warning bytes");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn a_dangerous_extension_chosen_in_the_native_dialog_is_warned_again() {
    let path = directory("save-chosen-exe").join("chosen.exe");
    let _ = std::fs::remove_file(&path);
    let (mut app, _, attachment) = seeded_attachment_app("p6c-chosen-exe", "report.pdf", Some(b"chosen bytes"));
    let saver = RecordingFileSaver::new();
    saver.set_path(Some(path.clone()));
    app.file_saver = saver.clone();
    app.request_attachment_save(&attachment);
    assert_eq!(saver.choose_calls(), 1);
    let warning = app.reading.attachment_warning.as_ref().expect("the chosen path must be re-classified");
    assert_eq!(warning.dangerous, Some(DangerousKind::Executable));
    assert_eq!(warning.path.as_deref(), Some(path.as_path()));
    assert!(saver.writes().is_empty(), "nothing is written before the second confirmation");
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Destinazione scelta")), "{text:?}");
    click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Salvare un file potenzialmente pericoloso?", true);
    assert_eq!(std::fs::read(&path).expect("saved after the second confirmation"), b"chosen bytes");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cancelling_a_save_writes_nothing_and_a_missing_blob_is_readable() {
    let path = directory("save-cancel").join("report.pdf");
    let (mut app, _, attachment) = seeded_attachment_app("p6c-cancel", "report.pdf", Some(b"bytes"));
    let saver = RecordingFileSaver::new();
    saver.set_path(None);
    app.file_saver = saver.clone();
    app.request_attachment_save(&attachment);
    assert_eq!(saver.choose_calls(), 1);
    assert!(saver.writes().is_empty());
    assert_eq!(app.status, "Salvataggio annullato");

    let (mut missing, _, attachment) = seeded_attachment_app("p6c-missing", "report.pdf", None);
    let saver = RecordingFileSaver::new();
    saver.set_path(Some(path));
    missing.file_saver = saver.clone();
    missing.request_attachment_save(&attachment);
    assert_eq!(saver.choose_calls(), 0);
    assert!(saver.writes().is_empty());
    let toast = missing.toast.as_ref().expect("a readable missing-blob toast");
    assert_eq!(toast.kind, ToastKind::Error);
    assert!(toast.message.contains("non è disponibile"));
}

#[test]
fn demo_save_is_disabled_and_never_reports_a_corrupt_attachment() {
    let (ctx, mut app) = ready_app();
    let (message_id, attachment) = app
        .engine
        .messages()
        .iter()
        .filter(|message| message.has_attachments)
        .map(|message| message.id.clone())
        .find_map(|id| app.engine.message(&id))
        .and_then(|message| message.attachments.first().map(|attachment| (message.summary.id.clone(), attachment.clone())))
        .expect("a demo attachment");
    app.open_message(message_id);
    settle_at(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert_eq!(probe::attachment_save_is_enabled(&ctx, &attachment.id), Some(false), "demo Save must be visibly disabled");
    let save = probe::attachment_save_rect(&ctx, &attachment.id).expect("the disabled Save rect");
    click_at(&ctx, &mut app, save.center(), Vec2::new(1200.0, 800.0));
    assert!(app.reading.attachment_warning.is_none());
    assert!(app.reading.attachment_overwrite.is_none());
    assert!(app.toast.is_none(), "a disabled button must not open a misleading error");
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(text.iter().any(|line| line.contains("La modalità demo non ha allegati reali")), "{text:?}");

    app.request_attachment_save(&attachment);
    let toast = app.toast.as_ref().expect("the demo reason");
    assert_eq!(toast.kind, ToastKind::Info);
    assert!(toast.message.contains("La modalità demo non ha allegati reali"));
}

#[test]
fn the_attachment_save_button_is_keyboard_reachable() {
    let path = directory("save-keyboard").join("report.pdf");
    let _ = std::fs::remove_file(&path);
    let (mut app, message_id, attachment) = seeded_attachment_app("p6c-keyboard-save", "report.pdf", Some(b"keyboard bytes"));
    let saver = RecordingFileSaver::new();
    saver.set_path(Some(path.clone()));
    app.file_saver = saver.clone();
    app.open_message(message_id);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert!(probe::attachment_save_rect(&ctx, &attachment.id).is_some());
    let save_id = app.reading.attachment_save_ids.get(&attachment.id).copied().expect("the Save button id");
    ctx.memory_mut(|memory| memory.request_focus(save_id));
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), vec![key(Key::Enter, Modifiers::NONE)]);
    assert_eq!(saver.writes().len(), 1, "Enter on the focused Save button saves");
    assert_eq!(std::fs::read(&path).expect("the keyboard-saved file"), b"keyboard bytes");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_link_requires_confirmation_and_reaches_the_injected_opener_exactly() {
    let mut app = PostinoApp::demo(true);
    let opener = RecordingLinkOpener::new();
    app.link_opener = opener.clone();
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    let link = reading::find_links("Vai su https://example.com/preventivo?token=abc, grazie.").pop().expect("a link");
    app.request_link_open(&link);
    assert_eq!(app.reading.pending_link.as_ref().map(|pending| pending.url.as_str()), Some("https://example.com/preventivo?token=abc"));
    assert!(opener.opened().is_empty(), "the sheet has not been confirmed");
    click_dialog_button(&ctx, &mut app, size, "Aprire il link?", true);
    assert_eq!(opener.opened(), vec!["https://example.com/preventivo?token=abc".to_owned()]);
    assert!(app.reading.pending_link.is_none());

    // The secondary button, not a hand-cleared field, is the cancel path.
    app.request_link_open(&link);
    click_dialog_button(&ctx, &mut app, size, "Aprire il link?", false);
    assert!(app.reading.pending_link.is_none());
    assert_eq!(opener.opened().len(), 1, "Annulla calls nothing");

    // The action boundary rechecks even when a pending value is set directly.
    app.reading.pending_link = Some(PendingLink {
        url: "javascript:alert(1)".to_owned(),
        host: "example.com".to_owned(),
        recipient: None,
        mailto: false,
        idn: false,
        non_ascii: false,
        backslash: false,
        focus_cancel: false,
    });
    app.confirm_pending_link();
    assert_eq!(opener.opened().len(), 1, "an unvetted URL never reaches the opener");
    assert_eq!(app.toast.as_ref().map(|toast| toast.kind), Some(ToastKind::Error));

    let bad = ExtractedLink {
        range: 0..10,
        url: "javascript:alert(1)".to_owned(),
        host: "example.com".to_owned(),
        recipient: None,
        mailto: false,
        idn: false,
        non_ascii: false,
        backslash: false,
    };
    app.request_link_open(&bad);
    assert!(app.reading.pending_link.is_none(), "a hostile scheme never reaches the sheet");
}

#[test]
fn the_link_sheet_shows_the_full_destination_and_the_effective_host() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let mut app = PostinoApp::demo(true);
    let link = reading::find_links("https://xn--bcher-kva.example/preventivo").pop().expect("an IDN link");
    app.request_link_open(&link);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("xn--bcher-kva.example/preventivo")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("Host di destinazione: xn--bcher-kva.example")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("punycode")), "the IDN warning must be visible");

    // A backslash ends the authority in a browser, so the sheet names evil.example and warns.
    let backslash = reading::find_links("https://evil.example\\@bank.example/").pop().expect("a backslash link");
    app.reading.pending_link = None;
    app.request_link_open(&backslash);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Host di destinazione: evil.example")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("barra rovesciata")), "{text:?}");

    // A Cyrillic host has no literal xn-- label but the browser punycodes it. The line names the
    // spelling in the link, not a host the browser will contact.
    let cyrillic = reading::find_links("https://аpple.com/x").pop().expect("a non-ASCII link");
    app.reading.pending_link = None;
    app.request_link_open(&cyrillic);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("caratteri non ASCII")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("Dominio nel link: аpple.com")), "{text:?}");
    assert!(!text.iter().any(|line| line.contains("Host di destinazione")), "a non-ASCII spelling is not the browser host: {text:?}");

    // A mailto opens a mail client and names its explicit recipient, not a browser host.
    let mailto = reading::find_links("mailto:help@example.com?subject=Preventivo").pop().expect("a mailto link");
    app.reading.pending_link = None;
    app.request_link_open(&mailto);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("client di posta")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("Destinatario: help@example.com")), "{text:?}");
    assert!(!text.iter().any(|line| line.contains("Host di destinazione")), "{text:?}");
}

#[test]
fn the_reading_pane_link_widget_opens_the_sheet() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    app.seed_screenshot(ScreenshotState::ReadingAttachments);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 800.0);
    settle_at(&ctx, &mut app, size);
    let url = "https://example.com/preventivo/postino-2026?token=anteprima";
    let link = probe::link_rect(&ctx, url).expect("the reading-pane link widget");
    click_at(&ctx, &mut app, link.center(), size);
    let pending = app.reading.pending_link.as_ref().expect("clicking the link opens the sheet");
    assert_eq!(pending.url, url);
}

#[test]
fn userinfo_and_mailto_hidden_recipients_are_never_links() {
    // The pure scanner owns the rule; this test keeps the app from presenting a crafted link.
    for raw in [
        "https://bank.example@evil.example/",
        "https://user:pass@bank.example/",
        "mailto:help@example.com?bcc=attacker@evil.example",
        "mailto:help@example.com?cc=attacker@evil.example",
        "mailto:attacker@evil.example,help@example.com",
        "mailto:attacker@evil.example%2Chelp@example.com",
        "mailto:attacker@evil.example;help@example.com",
        "mailto:help@example.com?attach=C:\\Users\\feder\\secret.pdf",
        "mailto:help@example.com?attachment=%5C%5Cattacker.example%5Cshare%5Csecret.pdf",
        "mailto:help@example.com?%61ttach=%5C%5Cattacker.example%5Cshare%5Csecret.pdf",
    ] {
        assert!(reading::find_links(raw).is_empty(), "{raw}");
        assert!(!reading::is_allowed_url(raw), "{raw}");
    }
}

#[test]
fn the_text_view_names_the_reason_plain_text_mail_is_not_rendered() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let mut app = PostinoApp::demo(true);
    let id = app
        .visible_messages()
        .0
        .iter()
        .map(|row| row.id.clone())
        .find(|id| app.engine.message(id).is_some_and(|message| message.body_html.is_none()))
        .expect("a demo message without an HTML part");
    app.open_message(id);
    let text = painted_text(&ctx, &mut app, Vec2::new(1280.0, 720.0));
    assert!(text.iter().any(|line| line.contains("Vista testo")), "the reading pane names its text view: {text:?}");
    assert!(text.iter().any(|line| line.contains("non ha una versione HTML")), "the fallback names its reason: {text:?}");
    assert!(!text.iter().any(|line| line.contains("Vista formattata")), "plain text must not claim the block view: {text:?}");
}

#[test]
fn a_long_link_paragraph_wraps_inside_the_reading_column() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let t = theme::tokens(false);
    let text = format!("{} https://example.com/preventivo", "parola ".repeat(80));
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(320.0, 600.0))), ..Default::default() };
    let mut width = 0.0;
    let mut output = ctx.run_ui(raw, |ui| {
        ui.set_max_width(320.0);
        link_paragraph(ui, &text, &t, false, reading::MAX_LINKS_PER_MESSAGE);
        width = ui.min_rect().width();
    });
    output.textures_delta.clear();
    assert!(width <= 320.0, "a long paragraph must wrap, not extend past the column: {width}");
}

#[test]
fn the_attachment_chip_shows_a_sanitized_name() {
    let (mut app, message_id, _) = seeded_attachment_app("p6c-bidi-chip", "invoice\u{202E}fdp.exe", Some(b"chip bytes"));
    app.open_message(message_id);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(!text.iter().any(|line| line.contains('\u{202E}')), "the chip must not render a bidi control: {text:?}");
    assert!(text.iter().any(|line| line.contains("invoicefdp.exe")), "the sanitized name is visible: {text:?}");
}

#[test]
fn four_attachments_fit_the_980_reading_pane() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    let mut message = app.engine.message(&id).expect("the open message");
    message.attachments = (0..4)
        .map(|index| Attachment {
            id: format!("att_many_{index}"),
            filename: format!("allegato-{index}.pdf"),
            mime: "application/pdf".to_owned(),
            size: 1024,
        })
        .collect();
    app.screenshot_message = Some(message);
    app.open_message(id);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    settle_at(&ctx, &mut app, size);
    let content = ctx.content_rect();
    let strip = probe::reading_attachments_rect(&ctx).expect("the pinned attachment strip");
    assert!(content.contains_rect(strip), "the 4-attachment strip is off-screen: {strip:?}");
    assert!(strip.height() > 0.0);
    let save = probe::attachment_save_rect(&ctx, "att_many_0").expect("the first attachment Save");
    assert!(strip.contains_rect(save), "the first attachment must stay visible: {save:?} in {strip:?}");
}

#[test]
fn a_hostile_message_caps_link_widgets() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    let mut message = app.engine.message(&id).expect("the open message");
    message.body_html = None;
    message.body_text = (0..100).map(|index| format!("https://example.com/{index}")).collect::<Vec<_>>().join("\n\n");
    app.screenshot_message = Some(message);
    app.open_message(id);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    settle_at(&ctx, &mut app, Vec2::new(1280.0, 800.0));
    assert_eq!(probe::link_count(&ctx), reading::MAX_LINKS_PER_MESSAGE, "the per-message link budget caps widgets");
}

#[test]
fn an_overlong_chosen_name_keeps_its_extension_warning() {
    let long_name = format!("{}.exe", "a".repeat(130));
    let path = directory("save-long-chosen").join(&long_name);
    let _ = std::fs::remove_file(&path);
    let (mut app, _, attachment) = seeded_attachment_app("p6c-long-chosen", "report.pdf", Some(b"long bytes"));
    let saver = RecordingFileSaver::new();
    saver.set_path(Some(path.clone()));
    app.file_saver = saver.clone();
    app.request_attachment_save(&attachment);
    let warning = app.reading.attachment_warning.as_ref().expect("the long chosen .exe must be warned");
    assert_eq!(warning.dangerous, Some(DangerousKind::Executable));
    assert!(saver.writes().is_empty());
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Salvare un file potenzialmente pericoloso?", true);
    assert_eq!(std::fs::read(&path).expect("saved after the warning"), b"long bytes");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_980_reading_overlay_shows_the_remote_line_and_attachment_save_in_both_themes() {
    for dark in [false, true] {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        app.seed_screenshot(ScreenshotState::ReadingAttachments);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let size = Vec2::new(980.0, 600.0);
        settle_at(&ctx, &mut app, size);
        let fixture = app.screenshot_message.as_ref().expect("the stored-truth screenshot fixture");
        assert_eq!(fixture.remote_images, 1, "dark={dark}: the fixture must carry the stored count");
        let html = fixture.body_html.as_deref().expect("sanitized HTML");
        assert!(
            !html.contains("<script") && !html.contains("<iframe") && !html.contains("<form") && !html.contains("src=\"http"),
            "dark={dark}: the fixture must be stored sanitized truth with no live source: {html}"
        );
        let content = ctx.content_rect();
        let strip = probe::reading_attachments_rect(&ctx).expect("the pinned attachment strip");
        let save = probe::attachment_save_rect(&ctx, "att_001").expect("the attachment Save button");
        assert!(content.contains_rect(strip), "the attachment strip is off-screen in dark={dark}: {strip:?}");
        assert!(strip.contains_rect(save), "Save is outside the attachment strip in dark={dark}: {save:?} in {strip:?}");
        assert!(content.contains_rect(save), "Save is off-screen at 980x600 in dark={dark}: {save:?}");
        assert_eq!(probe::attachment_save_is_enabled(&ctx, "att_001"), Some(true), "the store/screenshot path keeps Save usable");
        let text = painted_text(&ctx, &mut app, size);
        assert!(text.iter().any(|line| line.contains("ALLEGATI")), "dark={dark}: {text:?}");
        assert!(text.iter().any(|line| line == "Salva"), "the Salva label must be painted in dark={dark}: {text:?}");
        assert!(text.iter().any(|line| line.contains("Preventivo postino 2026")), "the heading block is missing in dark={dark}: {text:?}");
        assert!(text.iter().any(|line| line.contains("PDF del preventivo")), "the paragraph block is missing in dark={dark}: {text:?}");
        assert!(text.iter().any(|line| line.contains("Vista formattata")), "the block caption is missing in dark={dark}: {text:?}");
        let remote = text.iter().position(|line| line.contains("1 immagine remota bloccata")).expect("the stored count line");
        let mode = text.iter().position(|line| line.contains("Vista formattata")).expect("the block-view line");
        assert!(remote < mode, "the remote line must sit above the body caption in dark={dark}: {text:?}");
        let line = &text[remote];
        assert!(!line.contains("http") && !line.contains("tracker"), "the line must not name a URL: {line:?}");
    }
}

#[test]
fn the_980_invite_card_renders_in_both_themes_with_live_actions() {
    for dark in [false, true] {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        app.seed_screenshot(ScreenshotState::ReadingInvite);
        let message = app.screenshot_message.as_mut().expect("the screenshot message");
        message.remote_images = 2;
        message.attachments = vec![Attachment {
            id: "att_invite_pdf".to_owned(),
            filename: "preventivo.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            size: 248_320,
        }];
        message.summary.has_attachments = true;
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let size = Vec2::new(980.0, 600.0);
        settle_at(&ctx, &mut app, size);
        let card = probe::invite_card_rect(&ctx).expect("the invite card");
        let body = probe::reading_body_rect(&ctx).expect("the reading body region");
        assert!(card.min.y >= body.min.y - 0.5, "dark={dark}: the card starts outside the body budget: {card:?} in {body:?}");
        assert!(
            card.left() >= body.left() - 0.5 && card.right() <= body.right() + 0.5,
            "dark={dark}: the card is wider than the body: {card:?} in {body:?}"
        );
        for action in ["invite_accept", "invite_tentative", "invite_decline"] {
            let rect = probe::invite_action_rect(&ctx, action).unwrap_or_else(|| panic!("dark={dark}: {action} is missing"));
            assert!(card.contains_rect(rect), "dark={dark}: {action} is outside the card: {rect:?} in {card:?}");
            assert_eq!(probe::invite_action_is_enabled(&ctx, action), Some(true), "dark={dark}: {action} must be usable");
        }
        assert!(probe::attachment_save_rect(&ctx, "att_invite_pdf").is_some(), "dark={dark}: the attachment strip must stay visible");
        let save = probe::attachment_save_rect(&ctx, "att_invite_pdf").expect("the save button");
        assert!(save.min.y >= body.bottom() - 0.5, "dark={dark}: the strip overlaps the body: {save:?} vs {body:?}");
        let text = painted_text(&ctx, &mut app, size).join(" ");
        for expected in ["INVITO", "Revisione preventivo", "Giulia Ferri", "2 immagini remote bloccate", "preventivo.pdf"] {
            assert!(text.contains(expected), "dark={dark}: {expected:?} is missing from {text:?}");
        }
        // The card scrolls inside the body budget; the automatic-reply note is reachable, not
        // pushed outside the pane where a user could never read it.
        frame_at(&ctx, &mut app, size, wheel_by(body.center(), 30, -6.0));
        let text = painted_text(&ctx, &mut app, size).join(" ");
        assert!(text.contains("Nessuna risposta automatica"), "dark={dark}: the note is unreachable: {text:?}");
        let enabled = app.command_enabled(command("invite_accept").expect("the accept command"));
        assert!(enabled, "dark={dark}: the command table must agree with the visible button");
    }
}

#[test]
fn an_invalid_cancelled_or_organizerless_invite_has_no_live_reply() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    app.seed_screenshot(ScreenshotState::ReadingInvite);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);

    let invite = app.screenshot_invite.as_mut().expect("the screenshot invite");
    invite.organizer = None;
    invite.can_reply = false;
    settle_at(&ctx, &mut app, size);
    for action in ["invite_accept", "invite_tentative", "invite_decline"] {
        assert_eq!(probe::invite_action_is_enabled(&ctx, action), Some(false), "{action} must be visibly disabled without an organizer");
    }
    assert!(probe::invite_action_rect(&ctx, "invite_accept").is_some());
    assert!(!app.command_enabled(command("invite_accept").expect("the accept command")), "the palette must agree when the organizer is missing");
    let text = painted_text(&ctx, &mut app, size).join(" ");
    assert!(text.contains("Manca l'organizzatore"), "{text:?}");

    let invite = app.screenshot_invite.as_mut().expect("the screenshot invite");
    invite.cancelled = true;
    settle_at(&ctx, &mut app, size);
    for action in ["invite_accept", "invite_tentative", "invite_decline"] {
        assert!(probe::invite_action_rect(&ctx, action).is_none(), "a cancelled invite must not offer {action}");
    }
    let text = painted_text(&ctx, &mut app, size).join(" ");
    assert!(text.contains("annullato"), "{text:?}");

    let invite = app.screenshot_invite.as_mut().expect("the screenshot invite");
    invite.cancelled = false;
    invite.valid = false;
    invite.invalid_reason = Some("L'evento non ha una data di inizio.".to_owned());
    settle_at(&ctx, &mut app, size);
    assert!(probe::invite_action_rect(&ctx, "invite_accept").is_none(), "an invalid event must not offer a reply");
}

#[test]
fn clicking_an_enabled_action_on_a_stored_invite_creates_the_text_and_calendar_reply() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1200.0, 800.0);
    let mut app = invite_store_app("app-invite-reply");
    settle_at(&ctx, &mut app, size);
    assert!(app.invite_ready, "the stored REQUEST is answerable");
    let rect = probe::invite_action_rect(&ctx, "invite_accept").expect("the Accetta button");
    click_at(&ctx, &mut app, rect.center(), size);
    assert!(app.composer.open, "the click opens a draft");
    let draft_id = app.composer.draft_id.clone().expect("the draft id");
    assert!(app.composer.to.contains("giulia.ferri@example.com"), "{:?}", app.composer.to);
    assert!(app.composer.body.contains("Nessuna risposta automatica"), "{:?}", app.composer.body);
    assert_eq!(app.engine.draft_reply_link(&draft_id), Some("msg_invite@example.com"));
    assert_eq!(app.composer.attachments.len(), 1, "the stored path carries the calendar reply");
    assert_eq!(app.composer.attachments[0].mime, "text/calendar");
    let bytes = app.engine.attachment_bytes(&app.composer.attachments[0].id).expect("reply bytes");
    assert!(String::from_utf8(bytes).expect("utf-8").contains("PARTSTAT=ACCEPTED"));

    app.composer.open = false;
    app.composer.dirty = false;
    let small = Vec2::new(980.0, 600.0);
    settle_at(&ctx, &mut app, small);
    let mods = Modifiers { command: true, shift: true, ..Modifiers::NONE };
    frame_at(&ctx, &mut app, small, vec![egui::Event::ModifiersChanged(mods), key(Key::Num2, mods)]);
    assert!(app.composer.open, "Ctrl+Shift+2 reaches the same command table at 980x600");
    let tentative_id = app.composer.draft_id.clone().expect("the tentative draft id");
    assert_ne!(Some(tentative_id.clone()), Some(draft_id));
    assert!(app.composer.subject.starts_with("Tentativo:"), "{:?}", app.composer.subject);

    // Scarta then undo: the standard undo toast must bring the draft back, not a manual move.
    app.discard_composer();
    assert!(app.toast.as_ref().and_then(|toast| toast.undo.as_ref()).is_some(), "discarding a draft offers undo");
    app.undo_last();
    assert_eq!(app.engine.message(&tentative_id).expect("restored draft").summary.folder_id, "fld_acc_invite_drafts");
}

#[test]
fn an_outgoing_copy_and_a_demo_invite_do_not_offer_a_live_reply() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    let mut app = invite_store_app("app-invite-sent");
    app.open_message("msg_sent".to_owned());
    settle_at(&ctx, &mut app, size);
    assert!(!app.invite_ready, "a Sent copy is informational");
    assert_eq!(probe::invite_action_is_enabled(&ctx, "invite_accept"), Some(false));
    assert!(!app.command_enabled(command("invite_accept").expect("the accept command")));
}

#[test]
fn the_invite_reply_buttons_are_keyboard_reachable() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    app.seed_screenshot(ScreenshotState::ReadingInvite);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1200.0, 800.0);
    settle_at(&ctx, &mut app, size);
    let id = probe::invite_action_id(&ctx, "invite_accept").expect("the Accetta widget id");
    ctx.memory_mut(|memory| memory.request_focus(id));
    frame_at(&ctx, &mut app, size, vec![key(Key::Enter, Modifiers::NONE)]);
    let toast = app.toast.as_ref().expect("Enter reached the invite action");
    assert!(toast.message.contains("non contiene un invito leggibile"), "{:?}", toast.message);
}

#[test]
fn the_block_view_renders_every_supported_block() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let mut message = app.engine.message(&id).expect("the open message");
    message.body_text = "alternativa semplice".to_owned();
    message.body_html = Some(
        concat!(
            "<h1>Titolo uno</h1><h2>Titolo due</h2><h3>Titolo tre</h3>",
            "<p>Testo <em>enfasi</em> e <strong>forte</strong> e <code>mono</code>.</p>",
            "<ul><li>Punto primo</li><li>Punto secondo</li></ul><ol><li>Passo uno</li></ol>",
            "<blockquote><p>Citazione salvata</p></blockquote>",
            "<pre>riga mono</pre>",
            "<table><caption>Voci</caption><tr><th>Voce</th><th>Importo</th></tr><tr><td>Setup</td><td>480 euro</td></tr></table>",
            "<hr><p><img src=\"cid:logo@postino\" alt=\"Logo Postino\"></p>",
            "<p><a href=\"https://example.com/preventivo\">Apri il preventivo</a></p>",
        )
        .to_owned(),
    );
    app.screenshot_message = Some(message);
    app.reading_full = true;
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 1400.0);
    let text = painted_text(&ctx, &mut app, size);
    for expected in [
        "Titolo uno",
        "Titolo due",
        "Titolo tre",
        "enfasi",
        "forte",
        "mono",
        "Punto primo",
        "Punto secondo",
        "Passo uno",
        "Citazione salvata",
        "riga mono",
        "Voci",
        "Setup",
        "480 euro",
        "Logo Postino",
        "Apri il preventivo",
    ] {
        assert!(text.iter().any(|line| line.contains(expected)), "{expected} is missing: {text:?}");
    }
    assert!(text.iter().any(|line| line.contains("Vista formattata")), "the caption names the block view: {text:?}");
    assert!(
        !text.iter().any(|line| line.contains("<p>") || line.contains("<h1>") || line.contains("<table>") || line.contains("<li>")),
        "no markup may be painted: {text:?}"
    );
    assert!(!text.iter().any(|line| line.contains("cid:")), "a cid source must never be painted: {text:?}");
    assert!(!text.iter().any(|line| line.contains("http")), "no destination is painted as text: {text:?}");
    assert_eq!(probe::link_count(&ctx), 1, "the safe anchor is the only link widget");
}

#[test]
fn a_reloaded_body_replaces_the_cached_view_once() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let mut message = app.engine.message(&id).expect("the open message");
    message.body_text = String::new();
    message.body_html = Some("<p>prima versione</p>".to_owned());
    let first = app.rendered_body(&message);
    assert!(matches!(first.as_ref(), body::View::Blocks(body) if body.plain_text().contains("prima versione")), "{first:?}");
    assert!(Arc::ptr_eq(&first, &app.rendered_body(&message)), "an unchanged body must reuse the parsed view");

    // A sync refresh can replace only the fragment; the cached block view must not survive it.
    message.body_html = Some("<p>seconda versione</p>".to_owned());
    let reloaded = app.rendered_body(&message);
    assert!(!Arc::ptr_eq(&first, &reloaded), "a new fragment must invalidate the cache");
    assert!(matches!(reloaded.as_ref(), body::View::Blocks(body) if body.plain_text().contains("seconda versione")), "{reloaded:?}");

    // An oversized fragment with empty stored text falls back to the parser's prefix: the view
    // text never equals `body_text`, so the cache key must hold the stored input.
    message.body_html = Some(format!("<p>{}</p>", "a".repeat(postino_mime::MAX_BODY_BYTES + 1)));
    let fallback = app.rendered_body(&message);
    assert!(matches!(fallback.as_ref(), body::View::Text { reason: body::TextReason::TooLarge, .. }), "{fallback:?}");
    assert!(Arc::ptr_eq(&fallback, &app.rendered_body(&message)), "an oversized body with empty text must be cached too");
}

#[test]
fn a_block_body_cannot_exceed_the_per_message_link_budget() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let mut message = app.engine.message(&id).expect("the open message");
    let html: String = (0..80).map(|index| format!("<p><a href=\"https://example.com/{index}\">link {index}</a></p>")).collect();
    message.body_html = Some(html);
    message.body_text = "ottanta link".to_owned();
    app.screenshot_message = Some(message);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    painted_text(&ctx, &mut app, Vec2::new(1280.0, 800.0));
    assert_eq!(probe::link_count(&ctx), reading::MAX_LINKS_PER_MESSAGE, "the block path must honour the per-message budget");
    assert!(probe::link_rect(&ctx, "https://example.com/0").is_some(), "the first anchors stay widgets");
    assert!(probe::link_rect(&ctx, "https://example.com/64").is_none(), "an anchor over the budget stays readable text");
}

#[test]
fn a_hostile_or_oversized_body_never_panics_and_names_the_fallback() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 800.0);

    // 2 MiB of hostile markup under the parser's byte cap: the view is bounded and truncated.
    let mut message = app.engine.message(&id).expect("the open message");
    message.body_text = "alternativa semplice".to_owned();
    message.body_html = Some("<p onmouseover=\"alert(1)\">riga</p><script>alert(1)</script>".repeat(50_000));
    let hostile = message.clone();
    app.screenshot_message = Some(message);
    let started = std::time::Instant::now();
    let runs = painted_runs(&ctx, &mut app, size);
    // A bounded body must not turn one frame into an unbounded paint loop.
    assert!(started.elapsed() < Duration::from_secs(5), "a bounded large body took {:?} to paint", started.elapsed());
    let text: Vec<String> = runs.iter().map(|run| run.text.clone()).collect();
    assert!(text.iter().any(|line| line.contains("riga")), "the bounded body still renders: {text:?}");
    assert!(!text.iter().any(|line| line.contains("alert(") || line.contains("<script") || line.contains("onmouseover")), "{text:?}");
    let note = painted_visible(&runs, "Messaggio troncato");
    assert!(note.contains("Messaggio troncato"), "the truncation note must be honest and unclipped: {note:?}");
    let view = app.rendered_body(&hostile);
    assert!(
        matches!(view.as_ref(), body::View::Blocks(body) if body.blocks.len() <= postino_mime::MAX_BLOCKS),
        "the paint model must stay within the block budget: {view:?}"
    );

    // Over the parser's byte cap the pane keeps the stored text alternative and says why.
    let mut message = app.engine.message(&id).expect("the open message");
    message.body_text = "alternativa semplice".to_owned();
    message.body_html = Some(format!("<p>{}</p>", "a".repeat(postino_mime::MAX_BODY_BYTES + 1)));
    app.screenshot_message = Some(message);
    let runs = painted_runs(&ctx, &mut app, size);
    let caption = painted_visible(&runs, "Vista testo");
    assert!(caption.contains("supera il limite"), "the fallback names the reason and is unclipped: {caption:?}");
    let alternative = painted_visible(&runs, "alternativa semplice");
    assert!(alternative.contains("alternativa semplice"), "the stored alternative is painted and unclipped: {alternative:?}");
}

#[test]
fn block_links_open_only_through_the_existing_confirmation_sheet() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let mut message = app.engine.message(&id).expect("the open message");
    message.body_text = "alternativa semplice".to_owned();
    let idn = "https://xn--bcher-kva.example/acquisto";
    let backslash = "https://evil.example\\@bank.example/";
    let mailto = "mailto:help@example.com?subject=Preventivo";
    message.body_html = Some(format!(
        concat!(
            "<p><a href=\"javascript:alert(1)\">Non aprire</a>, <a href=\"https://bank.example@evil.example/\">Userinfo</a>, ",
            "<a href=\"https://example.com/sicuro\">Sicuro</a>, <a href=\"{idn}\">IDN</a>, ",
            "<a href=\"{backslash}\">Barra</a> e <a href=\"{mailto}\">Posta</a>.</p>",
        ),
        idn = idn,
        backslash = backslash,
        mailto = mailto,
    ));
    app.screenshot_message = Some(message);
    let opener = RecordingLinkOpener::new();
    app.link_opener = opener.clone();
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    settle_at(&ctx, &mut app, size);
    let text = painted_text(&ctx, &mut app, size);
    assert!(text.iter().any(|line| line.contains("Non aprire")), "a rejected anchor stays readable text: {text:?}");
    assert!(!text.iter().any(|line| line.contains("javascript:")), "a hostile destination is never painted: {text:?}");
    assert_eq!(probe::link_count(&ctx), 4, "only the safe, IDN, backslash and mailto destinations become widgets");
    assert!(probe::link_rect(&ctx, "https://bank.example@evil.example/").is_none(), "userinfo is never a widget");

    // The plain https anchor: the sheet names the exact host and only Apri opens it.
    let link = probe::link_rect(&ctx, "https://example.com/sicuro").expect("the safe anchor");
    click_at(&ctx, &mut app, link.center(), size);
    let pending = app.reading.pending_link.as_ref().expect("the confirmation sheet opens");
    assert_eq!(pending.url, "https://example.com/sicuro");
    assert_eq!(pending.host, "example.com");
    assert!(opener.opened().is_empty(), "the click alone must not open a browser");
    app.confirm_pending_link();
    assert_eq!(opener.opened(), vec!["https://example.com/sicuro".to_owned()]);

    // An IDN anchor keeps the punycode warning instead of claiming the spelling is the host.
    app.reading.pending_link = None;
    settle_at(&ctx, &mut app, size);
    let link = probe::link_rect(&ctx, idn).expect("the IDN anchor");
    click_at(&ctx, &mut app, link.center(), size);
    let pending = app.reading.pending_link.as_ref().expect("the IDN sheet opens");
    assert!(pending.idn, "the IDN flag survives the block path");
    let sheet = painted_text(&ctx, &mut app, size);
    assert!(sheet.iter().any(|line| line.contains("Host di destinazione: xn--bcher-kva.example")), "{sheet:?}");
    assert!(sheet.iter().any(|line| line.contains("punycode")), "{sheet:?}");
    assert_eq!(opener.opened().len(), 1, "the IDN sheet must not open without Apri");

    // A backslash anchor is presented with the authority a browser would really use.
    app.reading.pending_link = None;
    settle_at(&ctx, &mut app, size);
    let link = probe::link_rect(&ctx, backslash).expect("the backslash anchor");
    click_at(&ctx, &mut app, link.center(), size);
    let pending = app.reading.pending_link.as_ref().expect("the backslash sheet opens");
    assert!(pending.backslash, "the backslash flag survives the block path");
    let sheet = painted_text(&ctx, &mut app, size);
    assert!(sheet.iter().any(|line| line.contains("Host di destinazione: evil.example")), "{sheet:?}");
    assert!(sheet.iter().any(|line| line.contains("barra rovesciata")), "{sheet:?}");

    // A mailto anchor names its explicit recipient and reaches the mail client only on Apri.
    app.reading.pending_link = None;
    settle_at(&ctx, &mut app, size);
    let link = probe::link_rect(&ctx, mailto).expect("the mailto anchor");
    click_at(&ctx, &mut app, link.center(), size);
    let pending = app.reading.pending_link.as_ref().expect("the mailto sheet opens");
    assert!(pending.mailto, "the mailto flag survives the block path");
    assert_eq!(pending.recipient.as_deref(), Some("help@example.com"));
    let sheet = painted_text(&ctx, &mut app, size);
    assert!(sheet.iter().any(|line| line.contains("client di posta")), "{sheet:?}");
    assert!(sheet.iter().any(|line| line.contains("Destinatario: help@example.com")), "{sheet:?}");
    app.confirm_pending_link();
    assert_eq!(opener.opened(), vec!["https://example.com/sicuro".to_owned(), mailto.to_owned()]);
}

#[test]
fn the_keyboard_reaches_a_block_link_and_its_sheet() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    app.seed_screenshot(ScreenshotState::ReadingAttachments);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 800.0);
    settle_at(&ctx, &mut app, size);
    let url = "https://example.com/preventivo/postino-2026?token=anteprima";
    let link = probe::link_id(&ctx, url).expect("the block link widget");
    ctx.memory_mut(|memory| memory.request_focus(link));
    frame_at(&ctx, &mut app, size, vec![key(Key::Enter, Modifiers::NONE)]);
    let pending = app.reading.pending_link.as_ref().expect("Enter on a focused link opens the sheet");
    assert_eq!(pending.url, url);
}

#[test]
fn the_cc_line_is_painted_and_stays_inside_the_reading_column() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let mut message = app.engine.message(&id).expect("the open message");
    message.cc = vec![
        Address::new("Giulia Ferri", "giulia.ferri@example.com"),
        Address::new("Marco Bianchi", "marco.bianchi@example.com"),
        Address::new("Libreria Aurora", "ordini@libreria-aurora.example"),
    ];
    app.screenshot_message = Some(message);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 800.0);
    let text = painted_text(&ctx, &mut app, size);
    assert!(text.iter().any(|line| line.starts_with("Cc:")), "{text:?}");
    let cc = probe::reading_cc_rect(&ctx).expect("the Cc line");
    assert!(ctx.content_rect().contains_rect(cc), "the Cc line is outside the window: {cc:?}");
    assert!(cc.height() <= 20.0, "the Cc line is one truncated line, not a clipped block: {cc:?}");
}

#[test]
fn the_remote_content_line_uses_the_stored_count_and_never_a_url() {
    for (count, expected) in [
        (0, None),
        (1, Some("1 immagine remota bloccata")),
        (2, Some("2 immagini remote bloccate")),
        (10_000, Some("10000 immagini remote bloccate")),
    ] {
        let clock = screenshot_clock().expect("the frozen screenshot clock");
        let mut app = PostinoApp::demo_at(true, clock);
        let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
        app.open_message(id.clone());
        let mut message = app.engine.message(&id).expect("the open message");
        // The sanitized HTML has no remote reference left: only the stored count may produce the
        // line. A body with no link makes a URL in the painted copy a real failure.
        message.body_html = Some("<p>Corpo già sanificato.</p>".to_owned());
        message.body_text = "Corpo già sanificato.".to_owned();
        message.remote_images = count;
        app.screenshot_message = Some(message);
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let text = painted_text(&ctx, &mut app, Vec2::new(1280.0, 800.0));
        let line = text.iter().find(|line| line.contains("immagin"));
        match expected {
            Some(expected) => {
                let line = line.unwrap_or_else(|| panic!("the count {count} has no remote-content line: {text:?}"));
                assert!(line.contains(expected), "count {count} must read {expected:?}: {line:?}");
                assert!(line.contains("il contenuto arriva dal server e non viene caricato"), "{line:?}");
                assert!(!line.contains("http") && !line.contains("tracker"), "the line must not name a URL: {line:?}");
            }
            None => assert!(line.is_none(), "a message with no blocked image must not invent a line: {text:?}"),
        }
        assert!(!text.iter().any(|line| line.contains("http")), "the pane painted a URL with no link in the body: {text:?}");
    }
}

#[test]
fn a_file_created_after_the_overwrite_check_is_not_silently_replaced() {
    let path = directory("save-race").join("report.pdf");
    std::fs::write(&path, b"creato da un altro processo").expect("the racing file");
    let (mut app, _, _) = seeded_attachment_app("p6c-save-race", "report.pdf", Some(b"i miei byte"));
    let pending = PendingAttachment {
        filename: "report.pdf".to_owned(),
        bytes: b"i miei byte".to_vec(),
        path: Some(path.clone()),
        dangerous: None,
        focus_cancel: false,
        overwrite_confirmed: false,
    };
    app.write_attachment(pending);
    assert_eq!(std::fs::read(&path).expect("the racing file survives"), b"creato da un altro processo");
    assert!(app.reading.attachment_overwrite.is_some(), "the late file needs the overwrite sheet, not a silent replace");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_long_link_label_is_capped_but_keeps_the_full_url_in_the_sheet() {
    let long = format!("https://example.com/{}", "a".repeat(500));
    let shown = truncate_link_label(&long);
    assert!(shown.chars().count() <= reading::MAX_LINK_LABEL_CHARS, "{shown}");
    assert!(shown.ends_with('…'));
    assert_eq!(truncate_link_label("https://example.com/short"), "https://example.com/short");
    let tooltip = truncate_tooltip_url(&"t".repeat(reading::MAX_LINK_TOOLTIP_CHARS + 100));
    assert!(tooltip.chars().count() <= reading::MAX_LINK_TOOLTIP_CHARS, "{tooltip}");
    assert!(tooltip.ends_with('…'));
}

#[test]
fn pgp_badges_and_the_decrypt_sheet_are_rendered_from_the_engine_verdict() {
    let (ctx, mut app) = ready_app();
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let id =
        app.selected.clone().or_else(|| app.visible_messages().0.first().map(|message| message.id.clone())).expect("the demo has one open message");
    app.open_message(id.clone());
    app.pgp_checked = Some(id.clone());
    app.reading.pgp_reading = Some(PgpReadingState {
        message_id: id.clone(),
        kind: PgpMimeKind::Signed,
        verify: Some(PgpVerifyResult {
            state: PgpVerifyState::Verified,
            kind: PgpMimeKind::Signed,
            key_id: Some("A6079AAB69465D6D".to_owned()),
            fingerprint: Some("D529DDA09F683768022096EBA6079AAB69465D6D".to_owned()),
            signer: Some("Ada <ada@example.com>".to_owned()),
            reason: None,
        }),
        decrypt: None,
        view: None,
    });
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Firma verificata");
    assert!(probe::reading_pgp_rect(&ctx).is_some(), "the badge row must be probe-able");

    app.reading.pgp_reading = Some(PgpReadingState {
        message_id: id.clone(),
        kind: PgpMimeKind::Encrypted,
        verify: Some(PgpVerifyResult {
            state: PgpVerifyState::Unknown,
            kind: PgpMimeKind::Encrypted,
            key_id: None,
            fingerprint: None,
            signer: None,
            reason: Some("the retained message is encrypted, not signed; decrypt it first".to_owned()),
        }),
        decrypt: None,
        view: None,
    });
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Crittografato");
    painted_visible(&runs, "Decifra");
    app.dark = true;
    theme::apply(&ctx, true);
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Crittografato");
    app.dark = false;
    theme::apply(&ctx, false);

    app.reading.pgp_reading.as_mut().expect("the encrypted state").decrypt = Some(PgpDecryptResult {
        state: PgpDecryptState::Decrypted,
        key_id: Some("key_ada".to_owned()),
        fingerprint: None,
        text: Some("Testo decifrato".to_owned()),
        html: None,
        reason: None,
    });
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Decifrato · solo in questa vista");
    painted_visible(&runs, "Nascondi");

    app.pgp_prompt = Some(PgpPromptState::new(PgpPromptPurpose::Decrypt));
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Passphrase della chiave PGP");
    painted_visible(&runs, "Decifra");
    assert!(app.pgp_prompt.is_some(), "Escape was not pressed: the sheet stays open");
}

#[test]
fn a_non_pgp_message_is_verified_once_and_never_gets_a_badge() {
    let (mut app, daemon, _path, _key, _vault) =
        daemon_app("p12c2c-plain-check", |store| seed_account_rows(store, "acc_a", "save@example.com", None));
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a seeded message");
    app.open_message(id.clone());
    app.pgp_checked = None;
    app.reading.pgp_reading = None;
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    assert_eq!(app.pgp_checked.as_deref(), Some(id.as_str()), "the pane checks a normal message once");
    assert!(app.reading.pgp_reading.is_none(), "a non-PGP message must not get a PGP badge");
    daemon.shutdown().expect("the daemon stops");
}

#[test]
fn the_split_reading_pane_keeps_its_width_across_messages_and_remembers_a_drag() {
    let (ctx, mut app) = ready_app();
    let size = Vec2::new(1280.0, 720.0);
    let ids: Vec<String> = app.visible_messages().0.iter().take(2).map(|message| message.id.clone()).collect();
    assert_eq!(ids.len(), 2, "the demo has two messages to compare");

    app.open_message(ids[0].clone());
    frame_at(&ctx, &mut app, size, Vec::new());
    let first = probe::reading_panel_rect(&ctx).expect("the reading pane rect");
    let ratio = app.reading_ratio;

    app.open_message(ids[1].clone());
    frame_at(&ctx, &mut app, size, Vec::new());
    let second = probe::reading_panel_rect(&ctx).expect("the reading pane rect");
    assert!((first.width() - second.width()).abs() < 0.5, "the pane changed width between messages: {} then {}", first.width(), second.width());
    assert!((first.left() - second.left()).abs() < 0.5, "the pane moved between messages: {first:?} then {second:?}");

    // Grab the boundary and drag it left: the pane must widen and remember the new width.
    let edge = pos2(second.left() + 1.0, second.center().y);
    frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(edge)]);
    frame_at(&ctx, &mut app, size, vec![pointer(edge, true)]);
    frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(pos2(edge.x - 60.0, edge.y))]);
    assert!((app.reading_ratio - ratio).abs() < 0.005, "the width is persisted mid-drag");
    frame_at(&ctx, &mut app, size, vec![pointer(pos2(edge.x - 60.0, edge.y), false)]);
    frame_at(&ctx, &mut app, size, Vec::new());
    assert!(app.reading_ratio > ratio + 0.005, "the drag did not widen the pane: {} -> {}", ratio, app.reading_ratio);
    assert!(app.settings_dirty, "the new width must be saved");
    let dragged = probe::reading_panel_rect(&ctx).expect("the dragged pane rect");
    assert!(dragged.width() > first.width() + 10.0, "the drag did not change the pane: {first:?} -> {dragged:?}");

    app.open_message(ids[0].clone());
    frame_at(&ctx, &mut app, size, Vec::new());
    let after = probe::reading_panel_rect(&ctx).expect("the pane rect after reopening");
    assert!((after.width() - dragged.width()).abs() < 0.5, "the dragged width was forgotten: {} then {}", dragged.width(), after.width());

    // The settings reset must also drop the persisted panel width, not only the stored fraction.
    app.reset_reading_pane_width(&ctx);
    frame_at(&ctx, &mut app, size, Vec::new());
    let reset = probe::reading_panel_rect(&ctx).expect("the pane rect after reset");
    assert!((reset.width() - first.width()).abs() < 0.5, "the reset kept the dragged width: {} then {}", first.width(), reset.width());
}

#[test]
fn the_reading_split_rescales_with_the_window_and_keeps_the_stored_ratio() {
    let (ctx, mut app) = ready_app();
    let wide_size = Vec2::new(1280.0, 720.0);
    let ids: Vec<String> = app.visible_messages().0.iter().take(2).map(|message| message.id.clone()).collect();
    assert_eq!(ids.len(), 2, "the demo has two messages to compare");

    app.open_message(ids[0].clone());
    frame_at(&ctx, &mut app, wide_size, Vec::new());
    let wide = probe::reading_panel_rect(&ctx).expect("the reading pane at 1280");
    let ratio = app.reading_ratio;
    let expected = reading_pane_width_with_ratio(wide_size.x, ratio);
    assert!((wide.width() - expected).abs() < 1.0, "the pane is not the stored fraction of the window: {} vs {expected}", wide.width());
    let (_, wide_list) = probe::list_widths(&ctx).expect("the list width at 1280");

    // A different message must not change the split: the model is the stored ratio, not the first
    // message's rendered width.
    app.open_message(ids[1].clone());
    frame_at(&ctx, &mut app, wide_size, Vec::new());
    let second = probe::reading_panel_rect(&ctx).expect("the second message pane");
    assert!((second.width() - wide.width()).abs() < 0.5, "the pane changed width between messages: {} then {}", wide.width(), second.width());
    assert!((second.left() - wide.left()).abs() < 0.5, "the pane moved between messages: {wide:?} then {second:?}");

    // Shrinking the window applies the same fraction to the narrower content area; it must not
    // freeze the old pixel width or rewrite the ratio from the clamped render.
    let narrow_size = Vec2::new(1120.0, 720.0);
    frame_at(&ctx, &mut app, narrow_size, Vec::new());
    let narrow = probe::reading_panel_rect(&ctx).expect("the reading pane at 1120");
    let expected = reading_pane_width_with_ratio(narrow_size.x, ratio);
    assert!((app.reading_ratio - ratio).abs() < f32::EPSILON, "resizing rewrote the stored ratio: {ratio} -> {}", app.reading_ratio);
    assert!((narrow.width() - expected).abs() < 1.0, "the pane did not scale with the window: {} vs {expected}", narrow.width());
    assert!(narrow.width() < wide.width() - 50.0, "the pane kept its stale absolute width: {} then {}", wide.width(), narrow.width());
    assert!(narrow.right() <= narrow_size.x + 1.0, "the pane is not anchored to the right edge: {narrow:?}");
    let (_, narrow_list) = probe::list_widths(&ctx).expect("the list width at 1120");
    let remaining = narrow_size.x - ui_size::SIDEBAR;
    assert!((narrow_list - (remaining - narrow.width())).abs() < 1.5, "the list did not take the width the pane left: {narrow_list}");
    assert!(narrow_list <= wide_list + 0.5, "the list widened while the pane shrank: {wide_list} then {narrow_list}");

    // Growing back must return to the same model width, with the list back to its 1280 width.
    frame_at(&ctx, &mut app, wide_size, Vec::new());
    let restored = probe::reading_panel_rect(&ctx).expect("the reading pane after growing back");
    assert!((app.reading_ratio - ratio).abs() < f32::EPSILON, "growing back rewrote the stored ratio: {ratio} -> {}", app.reading_ratio);
    assert!((restored.width() - wide.width()).abs() < 0.5, "the pane did not return to its model width: {} then {}", wide.width(), restored.width());
    let (_, restored_list) = probe::list_widths(&ctx).expect("the list width after growing back");
    assert!((restored_list - wide_list).abs() < 0.5, "the list did not return to its model width: {wide_list} then {restored_list}");
    assert!(restored_list <= wide_size.x - ui_size::SIDEBAR - restored.width() + 1.5, "the list widened past the pane's model width");

    // The OS resize keeps the pointer down on the window border. The pane must follow the window
    // in that same frame, not wait for the button release.
    let mid_size = Vec2::new(1240.0, 720.0);
    frame_at(&ctx, &mut app, mid_size, vec![pointer(pos2(8.0, 8.0), true)]);
    let mid = probe::reading_panel_rect(&ctx).expect("the reading pane while the window resizes");
    let mid_expected = reading_pane_width_with_ratio(mid_size.x, ratio);
    assert!((mid.width() - mid_expected).abs() < 1.0, "the pane waited for the pointer release while resizing: {} vs {mid_expected}", mid.width());
    frame_at(&ctx, &mut app, mid_size, vec![pointer(pos2(8.0, 8.0), false)]);

    // Below the minimum the list no longer fits beside the pane and the reading view takes over.
    assert!(reading_overlay(980.0), "980 px must use the reading overlay");
    assert!(!reading_overlay(1120.0), "1120 px must keep the split");
}

#[test]
fn header_words_wrap_without_being_split_unless_a_token_is_wider_than_the_pane() {
    let (ctx, mut app) = ready_app();
    frame_at(&ctx, &mut app, Vec2::new(1280.0, 720.0), Vec::new());
    let font = Role::Page.font();
    let normal =
        crate::app::reading_pane::header_layout_job(&ctx, "Preventivo per la migrazione del sito web", font.clone(), egui::Color32::BLACK, 220.0);
    assert!(!normal.wrap.break_anywhere, "normal prose must wrap at spaces, not split a word");

    let token = crate::app::reading_pane::header_layout_job(&ctx, &"a".repeat(80), font, egui::Color32::BLACK, 220.0);
    assert!(token.wrap.break_anywhere, "an unbreakable token wider than the pane must be cut at a glyph");
}

#[test]
fn a_long_subject_and_sender_stay_inside_the_pane_and_tooltip_the_full_text() {
    let (ctx, mut app) = ready_app();
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let mut message = app.engine.message(&id).expect("the open message");
    let subject = format!("Report Domain: federicofabiani.it Submitter: {} END-OF-SUBJECT", "a".repeat(220));
    let sender = format!("no-reply-{}@example.com", "b".repeat(220));
    message.summary.subject = subject.clone();
    message.summary.from = Address::new("", sender.clone());
    app.screenshot_message = Some(message);

    for size in [Vec2::new(1280.0, 720.0), Vec2::new(1100.0, 720.0)] {
        ctx.all_styles_mut(|style| style.interaction.tooltip_delay = 0.0);
        frame_at(&ctx, &mut app, size, Vec::new());
        let panel = probe::reading_panel_rect(&ctx).expect("the reading pane rect");
        let runs = painted_runs(&ctx, &mut app, size);

        let subject_run = runs
            .iter()
            .find(|run| run.text.contains("Report Domain") && run.rect.top() >= panel.top() && run.rect.top() < panel.top() + 80.0)
            .expect("the painted subject");
        assert!(
            subject_run.clip.contains_rect(subject_run.rect),
            "the subject paints outside its clip: {:?} {:?}",
            subject_run.rect,
            subject_run.clip
        );
        assert!(subject_run.rect.right() <= panel.right() + 0.5, "the subject paints outside the pane: {:?} vs {panel:?}", subject_run.rect);
        assert!(subject_run.rect.height() <= 3.0 * Role::Page.font().size + 16.0, "the subject is not capped at three rows: {subject_run:?}");

        let sender_run = runs
            .iter()
            .find(|run| run.text.starts_with("no-reply-") && run.rect.top() >= panel.top() && run.rect.top() < panel.top() + 220.0)
            .expect("the painted sender");
        assert!(sender_run.clip.contains_rect(sender_run.rect), "the sender paints outside its clip: {:?} {:?}", sender_run.rect, sender_run.clip);
        assert!(sender_run.rect.right() <= panel.right() + 0.5, "the sender paints outside the pane: {:?} vs {panel:?}", sender_run.rect);

        // The truncated header keeps its full text in a tooltip, so the clipping is not data loss.
        let subject_center = subject_run.rect.center();
        frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(subject_center)]);
        let tooltip_runs = painted_runs(&ctx, &mut app, size);
        assert!(
            tooltip_runs.iter().any(|run| run.text.contains("END-OF-SUBJECT") && !runs.iter().any(|old| old.rect == run.rect)),
            "the subject tooltip does not carry the full text at {size:?}"
        );
        let sender_center = sender_run.rect.center();
        frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(pos2(0.0, 0.0))]);
        frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(sender_center)]);
        let tooltip_runs = painted_runs(&ctx, &mut app, size);
        let tooltip_text = tooltip_runs
            .iter()
            .filter(|run| run.rect.top() > sender_run.rect.bottom() + 1.0 && run.text.contains("no-reply-"))
            .map(|run| run.text.as_str())
            .collect::<String>();
        assert!(tooltip_text.contains(&sender), "the sender tooltip does not carry the full address at {size:?}: {tooltip_text:?}");
    }
}

#[test]
fn a_windowed_pane_keeps_its_width_with_wide_thread_content_and_after_a_click() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    app.seed_screenshot(ScreenshotState::Conversation);
    let id = app.selected.clone().expect("the open thread message");
    let mut message = app.engine.message(&id).expect("the open message");
    message.summary.subject = format!("Report Domain: federicofabiani.it Submitter: google.com Report-ID: 15533896837282044035 {}", "x".repeat(80));
    message.summary.from = Address::new("", "noreply-dmarc-support@google.com");
    message.remote_images = 3;
    message.attachments = vec![Attachment {
        id: "att_long".to_owned(),
        filename: "google.com!federicofabiani.it!1792713600!1792799999-very-long-attachment-name.zip".to_owned(),
        mime: "application/zip".to_owned(),
        size: 719,
    }];
    app.screenshot_message = Some(message);
    if let Some(summary) = app.thread_summary.as_mut() {
        summary.subject = format!("Report Domain: federicofabiani.it Submitter: google.com Report-ID: 15533896837282044035 {}", "y".repeat(80));
    }
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 720.0);
    frame_at(&ctx, &mut app, size, Vec::new());
    frame_at(&ctx, &mut app, size, Vec::new());

    let panel = probe::reading_panel_rect(&ctx).expect("the reading pane rect");
    let expected = reading_pane_width_with_ratio(size.x, app.reading_ratio);
    assert!((panel.width() - expected).abs() < 1.0, "the wide thread content inflated the pane: {} vs {expected}", panel.width());
    assert!(panel.right() <= size.x + 1.0, "the pane paints past the window: {panel:?}");
    let list_before = panel.left() - ui_size::SIDEBAR;

    // A press inside the body must not resize the pane and must not change the split. This is the
    // behaviour that made the idle and clicked screenshots disagree at the same window size.
    let click = pos2(panel.left() + panel.width() / 2.0, panel.top() + 180.0);
    frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(click)]);
    frame_at(&ctx, &mut app, size, vec![pointer(click, true)]);
    frame_at(&ctx, &mut app, size, vec![pointer(click, false)]);
    let clicked = probe::reading_panel_rect(&ctx).expect("the pane after the click");
    assert!(
        (clicked.width() - panel.width()).abs() < 0.5,
        "a click inside the body changed the pane width: {} then {}",
        panel.width(),
        clicked.width()
    );
    let list_after = clicked.left() - ui_size::SIDEBAR;
    assert!((list_after - list_before).abs() < 0.5, "a click changed the list width: {list_before} then {list_after}");

    // The panel's frame must not be anchored left of its own content. Every run painted with the
    // pane's clip rect must start inside the pane; before the fix the banner widened the frame and
    // the body started to the left of the panel's reported rect.
    let runs = painted_runs(&ctx, &mut app, size);
    for run in runs.iter().filter(|run| run.clip.left() >= panel.left() - 0.5 && run.rect.top() >= panel.top() - 1.0) {
        assert!(
            run.rect.left() >= panel.left() - 0.5,
            "reading content starts left of the pane at {:?}: {:?} (clip {:?})",
            run.text,
            run.rect,
            run.clip
        );
    }
}

#[test]
fn arrow_navigation_between_messages_keeps_the_pane_width() {
    let (ctx, mut app) = ready_app();
    let ids: Vec<String> = app.visible_messages().0.iter().take(3).map(|message| message.id.clone()).collect();
    assert_eq!(ids.len(), 3, "the demo has three messages to navigate");
    app.open_message(ids[0].clone());
    let size = Vec2::new(1280.0, 720.0);
    let mut widths = Vec::new();

    for (index, id) in ids.iter().enumerate() {
        assert_eq!(app.selected.as_deref(), Some(id.as_str()), "navigation did not select the expected message");
        let mut message = app.engine.message(id).expect("the navigated message");
        message.summary.subject = format!("Report Domain: federicofabiani.it Submitter: {} END", "z".repeat(120));
        message.attachments = if index % 2 == 0 {
            vec![Attachment {
                id: format!("att_nav_{index}"),
                filename: format!("google.com!federicofabiani.it!1792713600!1792799999-{index}-a-very-long-attachment-name.zip"),
                mime: "application/zip".to_owned(),
                size: 719,
            }]
        } else {
            Vec::new()
        };
        app.screenshot_message = Some(message);
        frame_at(&ctx, &mut app, size, Vec::new());
        let panel = probe::reading_panel_rect(&ctx).expect("the pane during navigation");
        widths.push(panel.width());
        if index + 1 < ids.len() {
            frame_at(&ctx, &mut app, size, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        }
    }

    let widest = widths.iter().copied().fold(f32::MIN, f32::max);
    let narrowest = widths.iter().copied().fold(f32::MAX, f32::min);
    assert!(widest - narrowest < 0.5, "the pane width followed the message content: {widths:?}");
}

#[test]
fn a_wide_table_scrolls_sideways_while_paragraphs_still_wrap() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    let mut app = PostinoApp::demo_at(true, clock);
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a demo message");
    app.open_message(id.clone());
    let mut message = app.engine.message(&id).expect("the open message");
    let paragraph = "parola ".repeat(80);
    let header: String = (0..12).map(|index| format!("<th>Colonna {index}</th>")).collect();
    let row: String = (0..12).map(|index| format!("<td>Valore {index}</td>")).collect();
    message.body_text = paragraph.clone();
    message.body_html = Some(format!("<p>{paragraph}</p><table><tr>{header}</tr><tr>{row}</tr></table><p>{paragraph}</p>"));
    message.attachments =
        vec![Attachment { id: "att_wide_table".to_owned(), filename: "tabella.csv".to_owned(), mime: "text/csv".to_owned(), size: 4096 }];
    app.screenshot_message = Some(message);
    app.reading_full = false;
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 720.0);
    frame_at(&ctx, &mut app, size, Vec::new());

    let view = probe::reading_scroll_view(&ctx).expect("the body viewport");
    let content = probe::reading_scroll_content(&ctx).expect("the body content size");
    assert!(content.x > view.width() + 40.0, "the wide table did not grow the body: content {content:?}, viewport {view:?}");
    let strip = probe::reading_attachments_rect(&ctx).expect("the pinned attachment strip");
    assert!(view.bottom() <= strip.top() + 1.0, "the horizontal bar band is covered by the attachment strip: body {view:?}, strip {strip:?}");
    assert_eq!(probe::reading_scroll_offset(&ctx).map(|offset| offset.x), Some(0.0), "the body starts at the left edge");
    let pane_before = probe::reading_panel_rect(&ctx).expect("the pane before the bar drag");
    let ratio_before = app.reading_ratio;

    // The floating bar lives in the last few pixels of the viewport, above the pinned strip. Drag
    // its thumb: a bar that only reacts to the wheel would still leave the table unreachable.
    let bar = pos2(view.left() + 6.0, view.bottom() - 2.0);
    frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(bar)]);
    frame_at(&ctx, &mut app, size, vec![pointer(bar, true)]);
    frame_at(&ctx, &mut app, size, vec![egui::Event::PointerMoved(pos2(bar.x + 60.0, bar.y))]);
    let dragged = probe::reading_scroll_offset(&ctx).expect("the body offset after a bar drag");
    assert!(dragged.x > 0.0, "dragging the horizontal bar did not scroll the body: {dragged:?}");
    let pane_after = probe::reading_panel_rect(&ctx).expect("the pane after the bar drag");
    assert!((pane_after.width() - pane_before.width()).abs() < 0.5, "the bar drag resized the pane: {pane_before:?} then {pane_after:?}");
    assert!((app.reading_ratio - ratio_before).abs() < f32::EPSILON, "the bar drag rewrote the split ratio: {ratio_before} -> {}", app.reading_ratio);
    frame_at(&ctx, &mut app, size, vec![pointer(pos2(bar.x + 60.0, bar.y), false)]);
    let pane_released = probe::reading_panel_rect(&ctx).expect("the pane after releasing the bar");
    assert!((pane_released.width() - pane_before.width()).abs() < 0.5, "releasing the bar resized the pane: {pane_before:?} then {pane_released:?}");

    let offset = probe::reading_scroll_offset(&ctx).expect("the body offset");
    let before_wheel = offset.x;
    frame_at(&ctx, &mut app, size, wheel_horizontal(view.center(), 4));
    frame_at(&ctx, &mut app, size, wheel_horizontal(view.center(), 4));
    let offset = probe::reading_scroll_offset(&ctx).expect("the body offset");
    assert!(offset.x > before_wheel, "the horizontal wheel did not scroll the body: {offset:?}");
    assert!(offset.x <= content.x - view.width() + 1.0, "the body scrolled past its content: {offset:?}");

    // The same app without the table must not gain a horizontal overflow from wrapped text.
    let plain = app.engine.message(&id).expect("the plain demo message");
    app.screenshot_message = Some(plain);
    frame_at(&ctx, &mut app, size, Vec::new());
    let view = probe::reading_scroll_view(&ctx).expect("the plain body viewport");
    let content = probe::reading_scroll_content(&ctx).expect("the plain body content size");
    assert!(content.x <= view.width() + 1.0, "wrapped paragraphs created a horizontal overflow: content {content:?}, viewport {view:?}");
}

/// The header action rects for the last painted frame, from the reading pane's own test hook.
fn reading_header_actions(ctx: &egui::Context) -> Vec<(&'static str, Rect)> {
    crate::app::reading_pane::header_action_rects(ctx)
}

/// The painted text of one frame that draws both the app and the export picker. The picker is not
/// wired into `app.rs` yet, so the test calls the same method the shell will call.
fn painted_with_export_picker(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, events: Vec<egui::Event>) -> Vec<String> {
    fn collect(shape: &egui::epaint::Shape, clip: Rect, runs: &mut Vec<PaintedRun>) {
        match shape {
            egui::epaint::Shape::Text(text) => {
                let rect = Rect::from_min_size(text.pos, text.galley.size());
                runs.push(PaintedRun { text: text.galley.text().to_owned(), rect, clip });
            }
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, clip, runs);
                }
            }
            _ => {}
        }
    }
    let mut runs = Vec::new();
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    for _ in 0..4 {
        let raw =
            egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), events: events.clone(), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| {
            app.show(ui);
            app.export_picker_dialog(ui);
        });
        runs.clear();
        for clipped in &output.shapes {
            collect(&clipped.shape, clipped.clip_rect, &mut runs);
        }
        output.textures_delta.clear();
    }
    runs.into_iter().map(|run| run.text).collect()
}

#[test]
fn the_reading_header_actions_never_overlap_at_any_width() {
    // 1600 keeps the labelled strip; 1280 keeps the split pane; 1024 and 900 are the overlay widths
    // where a fixed 400 pt breakpoint used to let the star/read group run into expand/close.
    for size in [Vec2::new(1600.0, 720.0), Vec2::new(1280.0, 720.0), Vec2::new(1024.0, 720.0), Vec2::new(900.0, 720.0)] {
        let (ctx, mut app) = ready_app();
        let ids = app.visible_messages().0;
        app.open_message(ids.first().expect("a demo message").id.clone());
        frame_at(&ctx, &mut app, size, Vec::new());

        let actions = reading_header_actions(&ctx);
        assert!(actions.len() >= 5, "the header lost actions at {size:?}: {actions:?}");
        let header = probe::reading_header_rect(&ctx).expect("the reading header rect");
        for (index, (name, rect)) in actions.iter().enumerate() {
            assert!(rect.width() > 0.0 && rect.height() > 0.0, "{name} has an empty hit rect: {rect:?}");
            assert!(header.contains_rect(*rect), "{name} escapes the header at {size:?}: {rect:?} in {header:?}");
            for (other, other_rect) in actions.iter().skip(index + 1) {
                assert!(!rect.intersects(*other_rect), "{name} and {other} overlap at {size:?}: {rect:?} vs {other_rect:?}");
            }
        }
        let names: Vec<&str> = actions.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"star") && names.contains(&"read"), "the star/read group vanished at {size:?}: {names:?}");
        if size.x < 1000.0 {
            assert!(names.contains(&"back"), "the overlay lost its back action at {size:?}: {names:?}");
        } else {
            assert!(names.contains(&"expand") && names.contains(&"close"), "the split lost its right group at {size:?}: {names:?}");
        }
    }
}

#[test]
fn the_export_icon_opens_the_format_picker_instead_of_saving() {
    let (mut app, id, _attachment) = seeded_attachment_app("reading-export-picker", "nota.txt", Some(b"ciao"));
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(1280.0, 720.0);
    app.open_message(id);
    frame_at(&ctx, &mut app, size, Vec::new());

    let actions = reading_header_actions(&ctx);
    let export = actions
        .iter()
        .find(|(name, _)| *name == "export_eml")
        .map(|(_, rect)| *rect)
        .unwrap_or_else(|| panic!("the export icon in the reading header: {actions:?}"));
    click_at(&ctx, &mut app, export.center(), size);
    assert!(app.reading.export_open, "the export icon exported instead of asking for a format");
    assert!(app.reading.attachment_warning.is_none(), "the picker must not start a save");

    let text = painted_with_export_picker(&ctx, &mut app, size, Vec::new());
    assert!(text.iter().any(|line| line.contains("Esporta messaggio")), "the picker title is not painted: {text:?}");
    assert!(text.iter().any(|line| line.contains("Messaggio .eml")), "the .eml choice is missing: {text:?}");
    assert!(text.iter().any(|line| line.contains("Documento PDF")), "the PDF choice is missing: {text:?}");
    assert!(text.iter().any(|line| line.contains("In arrivo")), "the PDF row has no badge: {text:?}");
    assert!(text.iter().any(|line| line.contains("arriva in una versione successiva")), "the PDF row hides its reason: {text:?}");

    painted_with_export_picker(&ctx, &mut app, size, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.reading.export_open, "Escape did not close the export picker");
}
