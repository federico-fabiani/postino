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
