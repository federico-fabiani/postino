//! The composer: attachments, reply and forward, signature, send from the composer.

use super::*;

#[test]
fn the_demo_composer_disables_attaching_with_a_reason_and_never_opens_a_picker() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.compose_new();
    frame(&ctx, &mut app, vec![]);
    assert_eq!(probe::composer_attach_enabled(&ctx), Some(false), "the demo must not pretend it can attach");
    let picker = crate::test_support::RecordingFilePicker::new();
    app.file_picker = picker.clone();
    let rect = probe::composer_attach_rect(&ctx).expect("the Allega file button");
    click(&ctx, &mut app, rect.center());
    assert_eq!(picker.calls(), 0, "a disabled Allega button must not open a dialog");
    assert!(app.composer.attachments.is_empty());
}

#[test]
fn the_picker_attaches_exact_bytes_lists_the_total_and_removes_the_row() {
    let mut app = picker_app("p8-picker");
    let directory = test_directory("p8-picker-files");
    let path = directory.join("contratto.pdf");
    let bytes = b"%PDF-1.7 esatto";
    std::fs::write(&path, bytes).expect("fixture file");
    let picker = crate::test_support::RecordingFilePicker::new();
    picker.set_paths(vec![path]);
    app.file_picker = picker.clone();

    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.compose_new();
    app.composer.to = "Giulia <giulia@example.com>".to_owned();
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let attach = probe::composer_attach_rect(&ctx).expect("the Allega file button");
    let card = probe::composer_card_rect(&ctx).expect("the composer card");
    assert_eq!(probe::composer_attach_enabled(&ctx), Some(true), "a store-backed composer must allow attaching");
    assert!(card.contains_rect(attach), "the attach button is outside the card: {attach:?} in {card:?}");
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    click_settled(&ctx, &mut app, attach.center(), Vec2::new(980.0, 600.0));
    assert!(probe::composer_attach_click_count(&ctx) > 0, "the click never reached the attach button");

    assert_eq!(picker.calls(), 1, "attach rect {attach:?} in card {card:?}");
    let draft = app.composer.draft_id.clone().expect("the picker must create the draft");
    assert_eq!(app.composer.attachments.len(), 1);
    assert_eq!(app.composer.attachments[0].filename, "contratto.pdf");
    assert_eq!(app.engine.attachment_bytes(&app.composer.attachments[0].id).expect("stored bytes"), bytes);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("1 allegato")), "the total is missing: {text:?}");

    let remove = probe::composer_attachment_remove_rect(&ctx, &app.composer.attachments[0].id).expect("the remove button");
    click_settled(&ctx, &mut app, remove.center(), Vec2::new(980.0, 600.0));
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    assert!(app.composer.attachments.is_empty(), "the row was not removed");
    assert!(app.engine.message(&draft).expect("draft").attachments.is_empty());

    let before = app.composer.attachments.len();
    picker.cancel();
    app.pick_composer_files();
    assert_eq!(app.composer.attachments.len(), before, "a cancelled picker adds nothing");
    assert_eq!(picker.calls(), 2, "the picker was not asked a second time");
}

#[test]
fn a_failed_or_missing_pick_is_one_readable_toast_and_writes_no_row() {
    let mut app = picker_app("p8-picker-fail");
    let directory = test_directory("p8-picker-missing");
    let missing = directory.join("non-esiste.pdf");
    let picker = crate::test_support::RecordingFilePicker::new();
    picker.set_paths(vec![missing]);
    app.file_picker = picker;
    app.compose_new();
    app.composer.to = "Giulia <giulia@example.com>".to_owned();
    let before = app.composer.attachments.len();
    app.pick_composer_files();
    assert_eq!(app.composer.attachments.len(), before);
    let toast = app.toast.as_ref().map(|toast| toast.message.clone()).expect("one toast");
    assert!(toast.contains("non-esiste.pdf"), "the toast must name the file: {toast}");
    assert!(!toast.contains("p8-picker-missing"), "the toast must not leak the path: {toast}");
}

#[test]
fn dropping_files_on_the_composer_uses_the_same_bounded_path() {
    let mut app = picker_app("p8-drop");
    let directory = test_directory("p8-drop-files");
    let path = directory.join("nota.txt");
    std::fs::write(&path, b"drop bytes").expect("fixture file");
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.compose_new();
    app.composer.to = "Giulia <giulia@example.com>".to_owned();
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    let card = probe::composer_card_rect(&ctx).expect("the composer card");
    frame_with_drop(&ctx, &mut app, size, card.center(), vec![path]);
    frame_at(&ctx, &mut app, size, vec![]);
    assert_eq!(app.composer.attachments.len(), 1, "the dropped file was not attached");
    assert_eq!(app.composer.attachments[0].filename, "nota.txt");
    assert_eq!(app.engine.attachment_bytes(&app.composer.attachments[0].id).expect("bytes"), b"drop bytes");
}

#[test]
fn forwarding_copies_attachment_bytes_and_a_missing_blob_is_reported_and_blocks_send() {
    let mut app = booted_app("p8-forward", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        store
            .replace_attachments(
                "msg_acc_a_1",
                &[postino_store::Attachment {
                    id: "att_parent".to_owned(),
                    filename: "preventivo.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 0,
                    blob_hash: None,
                }],
            )
            .expect("attachment row");
    });
    app.engine.store_attachment_bytes("msg_acc_a_1", "att_parent", b"byte del forward").expect("source bytes");
    app.reply_to("msg_acc_a_1", ComposeMode::Forward);
    app.composer.touch(0.0);
    assert!(app.save_composer(false), "the forward draft must save");
    let draft = app.composer.draft_id.clone().expect("forward draft");
    let attachments = app.engine.message(&draft).expect("draft").attachments.clone();
    assert_eq!(attachments.len(), 1, "the forwarded attachment was dropped");
    assert_eq!(app.engine.attachment_bytes(&attachments[0].id).expect("copied bytes"), b"byte del forward");
    assert_eq!(attachments[0].filename, "preventivo.pdf");

    let second = booted_app("p8-forward-missing", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        store
            .replace_attachments(
                "msg_acc_a_1",
                &[postino_store::Attachment {
                    id: "att_parent".to_owned(),
                    filename: "manca.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 4,
                    blob_hash: None,
                }],
            )
            .expect("attachment row");
    });
    let mut second = second;
    second.reply_to("msg_acc_a_1", ComposeMode::Forward);
    second.composer.touch(0.0);
    assert!(second.save_composer(false));
    let draft = second.composer.draft_id.clone().expect("forward draft");
    let notice = second.composer.notice.clone().expect("the failed copy is reported");
    assert!(notice.contains("manca.pdf"), "{notice}");
    // The row is in the composer; the next save (what Send does first) persists it and keeps
    // the send blocked on the missing bytes.
    second.composer.to = "Giulia <giulia@example.com>".to_owned();
    second.composer.touch(0.0);
    assert!(second.save_composer(false));
    assert_eq!(second.engine.message(&draft).expect("draft").attachments.len(), 1, "the failed row stays visible");
    let error = second.engine.call("message_send", json!({ "draft": draft })).expect_err("send must refuse");
    assert!(error.to_string().contains("manca.pdf"), "{error}");
}

#[test]
fn a_pristine_reply_sends_from_a_real_click_and_saves_before_close() {
    let ca = postino_testkit::TlsFixture::generate().ca();
    let mut app = smtp_app("p8-pristine-reply", 1, ca);
    let source = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Origine".to_owned(),
                body_text: "Corpo originale".to_owned(),
                attachments: Vec::new(),
                in_reply_to: None,
            },
        )
        .expect("source draft");
    app.reply_to(&source.summary.id, ComposeMode::Reply);
    assert!(app.composer.dirty, "a prefill must be content even before the first keystroke");
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    let send = probe::composer_send_rect(&ctx).expect("the Send button");
    click_settled(&ctx, &mut app, send.center(), size);
    assert!(!app.composer.open, "Send did not close the composer: {:?}", app.composer.save_error);
    let queued = app.engine.outbox_list(None, 500, 0).expect("outbox");
    assert_eq!(queued.entries.len(), 1, "an unedited reply did not create an outbox row");

    let second = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Seconda origine".to_owned(),
                body_text: "Corpo".to_owned(),
                attachments: Vec::new(),
                in_reply_to: None,
            },
        )
        .expect("second source");
    app.reply_to(&second.summary.id, ComposeMode::Reply);
    assert!(app.save_composer(true), "Salva bozza must create the pristine reply");
    assert!(app.composer.draft_id.is_some());
    assert!(app.engine.messages().iter().any(|message| message.subject == "Re: Seconda origine"));

    let third = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Terza origine".to_owned(),
                body_text: "Corpo".to_owned(),
                attachments: Vec::new(),
                in_reply_to: None,
            },
        )
        .expect("third source");
    app.reply_to(&third.summary.id, ComposeMode::Reply);
    frame_at(&ctx, &mut app, size, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.composer.open, "Esc did not close the reply");
    assert!(app.engine.messages().iter().any(|message| message.subject == "Re: Terza origine"), "Esc discarded the reply");
}

#[test]
fn typing_then_attaching_or_removing_does_not_lose_the_body() {
    let ca = postino_testkit::TlsFixture::generate().ca();
    let mut app = smtp_app("p8-dirty-attach", 1, ca);
    let source = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza da conservare".to_owned(),
                body_text: "corpo vecchio".to_owned(),
                attachments: Vec::new(),
                in_reply_to: None,
            },
        )
        .expect("source draft");
    app.edit_draft(&source.summary.id);
    app.composer.body = "NUOVO TESTO".to_owned();
    app.composer.touch(1.0);
    let directory = test_directory("p8-dirty-attach-files");
    let path = directory.join("nota.txt");
    std::fs::write(&path, b"allegato").expect("fixture");
    app.attach_composer_files(vec![path]);
    assert!(app.composer.dirty, "attaching must not claim the unsaved body is saved");
    let attachment = app.composer.attachments[0].id.clone();
    app.remove_composer_attachment(&attachment);
    assert!(app.composer.dirty, "removing must not claim the unsaved body is saved");
    app.send_composer();
    assert!(!app.composer.open, "send failed: {:?}", app.composer.save_error);
    assert_eq!(app.engine.message(&source.summary.id).expect("draft").body_text, "NUOVO TESTO");
    assert_eq!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.len(), 1);

    let second = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza chiusa".to_owned(),
                body_text: "corpo vecchio".to_owned(),
                attachments: Vec::new(),
                in_reply_to: None,
            },
        )
        .expect("second draft");
    app.edit_draft(&second.summary.id);
    app.composer.body = "TESTO PRIMA DELLA CHIUSURA".to_owned();
    app.composer.touch(2.0);
    let path = directory.join("seconda.txt");
    std::fs::write(&path, b"secondo").expect("fixture");
    app.attach_composer_files(vec![path]);
    app.close_composer();
    assert_eq!(app.engine.message(&second.summary.id).expect("closed draft").body_text, "TESTO PRIMA DELLA CHIUSURA");
}

#[test]
fn a_failed_forward_copy_blocks_send_until_the_row_is_removed() {
    let ca = postino_testkit::TlsFixture::generate().ca();
    let mut app = smtp_app("p8-forward-block", 1, ca);
    let source = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Con allegato mancante".to_owned(),
                body_text: "Corpo".to_owned(),
                attachments: vec![postino_core::Attachment {
                    id: "source_missing".to_owned(),
                    filename: "manca.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 4,
                }],
                in_reply_to: None,
            },
        )
        .expect("source draft");
    app.reply_to(&source.summary.id, ComposeMode::Forward);
    app.composer.to = "Giulia <giulia@example.com>".to_owned();
    app.send_composer();
    assert!(app.composer.open, "a failed forward copy must keep the composer open");
    let failure = app.composer.attachment_failure.clone().expect("a blocking failure sentence");
    assert!(failure.contains("manca.pdf"), "{failure}");
    assert!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.is_empty(), "the failed attachment was silently dropped");
    assert_eq!(app.composer.failed_attachment_ids.len(), 1);

    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    let failed_id = app.composer.failed_attachment_ids[0].clone();
    let remove = probe::composer_attachment_remove_rect(&ctx, &failed_id).expect("the failed row remove button");
    let remove_id = probe::composer_attachment_remove_widget_id(&ctx, &failed_id).expect("the remove widget id");
    let card = probe::composer_card_rect(&ctx).expect("card");
    assert!(card.contains_rect(remove), "failed remove outside card: {remove:?} in {card:?}");
    ctx.memory_mut(|memory| memory.request_focus(remove_id));
    frame_at(&ctx, &mut app, size, vec![]);
    assert!(ctx.memory(|memory| memory.has_focus(remove_id)), "the failed row remove button cannot take keyboard focus");
    frame_at(&ctx, &mut app, size, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(probe::composer_attachment_remove_was_clicked(&ctx, &failed_id), "Enter on the remove button did not reach it");
    assert!(app.composer.attachment_failure.is_none(), "removing the failed row did not unblock send (rect {remove:?})");
    app.composer.to = "Giulia <giulia@example.com>".to_owned();
    app.send_composer();
    assert!(!app.composer.open, "send after removal failed: {:?}", app.composer.save_error);
    assert_eq!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.len(), 1);
}

#[test]
fn a_missing_attachment_send_error_names_the_file_and_the_good_one_stays() {
    let ca = postino_testkit::TlsFixture::generate().ca();
    let mut app = smtp_app("p8-send-name", 1, ca);
    let source = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Due allegati".to_owned(),
                body_text: "Corpo".to_owned(),
                attachments: vec![postino_core::Attachment {
                    id: "missing".to_owned(),
                    filename: "manca.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 4,
                }],
                in_reply_to: None,
            },
        )
        .expect("draft with a missing row");
    app.engine.add_draft_attachment(&source.summary.id, "buono.txt", None, b"ok").expect("good blob");
    app.edit_draft(&source.summary.id);
    app.send_composer();
    let error = app.composer.save_error.clone().expect("a readable send refusal");
    assert!(error.contains("manca.pdf"), "the filename is missing from the refusal: {error}");
    assert!(error.contains("mancante o illeggibile"), "{error}");
    assert_eq!(app.engine.message(&source.summary.id).expect("draft").attachments.len(), 2, "the good attachment was dropped");
    assert!(app.engine.outbox_list(None, 500, 0).expect("outbox").entries.is_empty());
}

#[test]
fn a_drop_on_the_attachment_strip_is_added_once() {
    let mut app = picker_app("p8-drop-strip");
    let directory = test_directory("p8-drop-strip-files");
    let path = directory.join("striscia.txt");
    std::fs::write(&path, b"una volta").expect("fixture");
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.compose_new();
    app.composer.to = "Giulia <giulia@example.com>".to_owned();
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    let card = probe::composer_card_rect(&ctx).expect("composer card");
    frame_with_drop(&ctx, &mut app, size, pos2(card.center().x, card.max.y - 60.0), vec![path]);
    frame_at(&ctx, &mut app, size, vec![]);
    assert_eq!(app.composer.attachments.len(), 1, "a drop over the strip was added twice");
    assert_eq!(app.composer.attachments[0].filename, "striscia.txt");
}

#[test]
fn a_large_reply_subject_or_body_has_a_readable_save_refusal() {
    let mut app = picker_app("p8-large-reply");
    let source = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "x".repeat(998),
                body_text: "Corpo".to_owned(),
                attachments: Vec::new(),
                in_reply_to: None,
            },
        )
        .expect("source");
    app.reply_to(&source.summary.id, ComposeMode::Reply);
    app.composer.touch(1.0);
    assert!(!app.save_composer(true), "the oversized Re: subject must be refused");
    let error = app.composer.save_error.clone().expect("readable error");
    assert!(error.contains("998"), "{error}");
    assert!(!error.contains("Non è stato possibile salvare la bozza. Controlla"), "a generic error hid the limit: {error}");
    assert!(draft_save_error_message(&None, "invalid arguments: body_text is larger than 1048576 bytes").contains("1 MiB"));
}

#[test]
fn a_demo_draft_attachment_can_be_removed_locally_without_a_store() {
    let mut app = PostinoApp::demo(true);
    let draft = app
        .engine
        .save_draft(
            None,
            None,
            postino_core::DraftContent {
                to: vec![postino_core::Address::new("Giulia", "giulia.ferri@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Bozza demo".to_owned(),
                body_text: "Corpo".to_owned(),
                attachments: vec![postino_core::Attachment {
                    id: "att_demo".to_owned(),
                    filename: "demo.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 4,
                }],
                in_reply_to: None,
            },
        )
        .expect("demo draft");
    app.edit_draft(&draft.summary.id);
    app.remove_composer_attachment("att_demo");
    assert!(app.composer.attachments.is_empty(), "the demo remove button did nothing");
    assert!(app.composer.dirty);
    assert!(app.save_composer(false));
    assert!(app.engine.message(&draft.summary.id).expect("draft").attachments.is_empty());
}

#[test]
fn the_attach_button_is_reachable_from_the_keyboard() {
    let mut app = picker_app("p8-attach-keyboard");
    let directory = test_directory("p8-attach-keyboard-files");
    let path = directory.join("tastiera.txt");
    std::fs::write(&path, b"keyboard bytes").expect("fixture");
    let picker = crate::test_support::RecordingFilePicker::new();
    picker.set_paths(vec![path]);
    app.file_picker = picker.clone();
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.compose_new();
    app.composer.to = "Giulia <giulia@example.com>".to_owned();
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    let widget = probe::composer_attach_widget_id(&ctx).expect("the attach widget id");
    ctx.memory_mut(|memory| memory.request_focus(widget));
    frame_at(&ctx, &mut app, size, vec![]);
    assert!(ctx.memory(|memory| memory.has_focus(widget)), "the attach button did not take keyboard focus");
    frame_at(&ctx, &mut app, size, vec![key(Key::Enter, Modifiers::NONE)]);
    frame_at(&ctx, &mut app, size, vec![]);
    assert_eq!(picker.calls(), 1, "Enter on the focused attach button did not open the picker");
    assert_eq!(app.composer.attachments.len(), 1);
    assert_eq!(app.engine.attachment_bytes(&app.composer.attachments[0].id).expect("bytes"), b"keyboard bytes");
}

#[test]
fn the_signature_editor_saves_per_account_and_escape_returns_to_the_account_row() {
    let mut app = booted_app("p8-signature", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.open_settings(&ctx, None);
    let size = Vec2::new(980.0, 600.0);
    settle_at(&ctx, &mut app, size);
    let open = probe::signature_open_button_rect(&ctx, "acc_b").expect("the Firma button");
    click_at(&ctx, &mut app, open.center(), size);
    settle_at(&ctx, &mut app, size);
    assert!(
        probe::signature_open_click_count(&ctx) > 0,
        "the Firma button did not receive the click (rect {open:?}, remove rect {:?}, hover {:?}, remove state {:?}, settings {:?})",
        probe::account_remove_button_rect(&ctx, "acc_b"),
        probe::signature_open_hover_state(&ctx, "acc_b"),
        app.account_remove.is_some(),
        probe::settings_sections(&ctx)
    );
    assert_eq!(app.signature_editor.as_ref().map(|editor| editor.account.as_str()), Some("acc_b"));
    let editor_rect = probe::signature_editor_rect(&ctx, "acc_b").expect("the signature text area");
    let appearance = probe::settings_sections(&ctx).get("ASPETTO").copied().expect("the appearance section");
    assert!(editor_rect.max.y <= appearance.min.y + 1.0, "the signature editor crosses into ASPETTO: {editor_rect:?} vs {appearance:?}");
    assert!(probe::signature_cancel_rect(&ctx, "acc_b").is_some(), "the cancel button is missing");
    if let Some(editor) = app.signature_editor.as_mut() {
        editor.text = "Firma B\nb@example.com".to_owned();
    }
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let save = probe::signature_save_rect(&ctx, "acc_b").expect("Salva firma");
    click_at(&ctx, &mut app, save.center(), size);
    settle_at(&ctx, &mut app, size);
    assert!(app.signature_editor.is_none(), "the editor did not close after Save");
    assert_eq!(
        app.engine.accounts().iter().find(|account| account.id == "acc_b").map(|account| account.signature.as_str()),
        Some("Firma B\nb@example.com")
    );
    assert_eq!(
        app.engine.accounts().iter().find(|account| account.id == "acc_a").map(|account| account.signature.as_str()),
        Some(""),
        "the other account was changed"
    );

    app.switch_account("acc_b".to_owned());
    app.compose_new();
    assert!(app.composer.body.contains("Firma B"), "the new message did not use the saved signature: {}", app.composer.body);

    app.composer = ComposerState::closed();
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let open = probe::signature_open_button_rect(&ctx, "acc_a").expect("the Firma button");
    let open_id = probe::signature_open_button_widget_id(&ctx, "acc_a").expect("the Firma widget id");
    click_at(&ctx, &mut app, open.center(), size);
    settle_at(&ctx, &mut app, size);
    assert!(app.signature_editor.is_some());
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.signature_editor.is_none(), "Escape must close the editor");
    assert!(app.show_accounts_dialog, "Escape must not close Settings with the editor open");
    assert_eq!(app.signature_focus_account, None, "the focus request must be spent on the account button");
    assert!(ctx.memory(|memory| memory.has_focus(open_id)), "Escape did not return focus to the Firma button");

    // Closing Settings discards an unsaved editor instead of showing stale text on reopen.
    let open = probe::signature_open_button_rect(&ctx, "acc_a").expect("the Firma button");
    click_at(&ctx, &mut app, open.center(), size);
    settle_at(&ctx, &mut app, size);
    if let Some(editor) = app.signature_editor.as_mut() {
        editor.text = "testo mai salvato".to_owned();
    }
    app.close_settings(&ctx);
    assert!(app.signature_editor.is_none());
    app.open_settings(&ctx, None);
    assert!(app.signature_editor.is_none(), "closing Settings kept the unsaved editor text");
}

#[test]
fn composer_attachments_and_the_signature_editor_render_at_980_light_and_dark() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let mut app = PostinoApp::demo_at(true, screenshot_clock().expect("clock"));
        app.dark = dark;
        let mut composer = ComposerState::new(ComposeMode::New);
        composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
        composer.subject = "Allegati".to_owned();
        composer.attachments = vec![
            Attachment {
                id: "att_p8_1".to_owned(),
                filename: "preventivo-postino.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 248_320,
            },
            Attachment { id: "att_p8_2".to_owned(), filename: "planimetria-ufficio.png".to_owned(), mime: "image/png".to_owned(), size: 1_048_576 },
        ];
        app.composer = composer;
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let card = probe::composer_card_rect(&ctx).expect("the composer card");
        let send = probe::composer_send_rect(&ctx).expect("the Send button");
        assert!(card.contains_rect(send), "Send is outside the composer at 980x600: {send:?} in {card:?}");
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(
            text.iter().any(|line| line.contains("2 allegati")),
            "the total is missing in {} mode: {text:?}",
            if dark { "dark" } else { "light" }
        );
        assert!(text.iter().any(|line| line.contains("preventivo-postino.pdf")), "the first attachment is missing: {text:?}");

        app.composer = ComposerState::closed();
        app.seed_screenshot(ScreenshotState::SettingsSignature);
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        assert!(text.iter().any(|line| line.contains("Firma")), "the signature editor is missing: {text:?}");
        assert!(text.iter().any(|line| line.contains("byte")), "the byte count is missing: {text:?}");
    }
}

#[test]
fn compose_opens_with_the_signature_and_renders_headlessly() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.compose_new();
    assert!(app.composer.open);
    assert_eq!(app.composer.mode, ComposeMode::New);
    assert!(app.composer.body.contains("Federico"));
    assert_eq!(app.composer.focus, Some(ComposerFocus::To));
    frame(&ctx, &mut app, vec![]);
    assert!(app.composer.open);
}

#[test]
fn c_opens_the_composer_and_escape_keeps_the_draft() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    frame(&ctx, &mut app, vec![key(Key::C, Modifiers::NONE)]);
    assert!(app.composer.open, "C opens the composer");
    app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
    app.composer.subject = "Bozza da tastiera".to_owned();
    app.composer.body = "Corpo".to_owned();
    app.composer.touch(0.0);
    frame(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.composer.open, "Esc closes the composer");
    assert!(app.engine.messages().iter().any(|message| message.subject == "Bozza da tastiera"), "Esc must save the draft before closing");
}

#[test]
fn ctrl_enter_in_the_demo_does_not_claim_a_submission() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    app.compose_new();
    app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
    app.composer.subject = "Invio da tastiera".to_owned();
    app.composer.body = "Corpo".to_owned();
    app.composer.touch(0.0);
    frame(&ctx, &mut app, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::Enter, Modifiers::COMMAND)]);
    assert!(app.composer.open, "the demo must keep the composer open instead of pretending the message left");
    // The shortcut reads the same enabled rule as the button and shows the reason instead of
    // doing nothing. There is no save error, because no submission was attempted.
    assert!(app.composer.save_error.is_none(), "the shortcut attempted a disabled send: {:?}", app.composer.save_error);
    assert_eq!(app.composer.notice.as_deref(), Some(DEMO_SEND_REASON));
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(text.iter().any(|line| line.contains("La modalità demo non invia")), "the refusal is not visible: {text:?}");
    assert!(text.iter().any(|line| line.contains("Invio non disponibile")), "the status bar still promises Ctrl+Invio: {text:?}");
    assert!(
        !app.engine.messages().iter().any(|message| message.subject == "Invio da tastiera" && message.folder_id == "sent"),
        "the demo moved the message to Sent without a submission"
    );
}

#[test]
fn ctrl_enter_without_smtp_names_the_missing_server() {
    let mut app = booted_app("send-no-smtp-notice", Vec::new(), |store| {
        seed_account_rows(store, "acc_no_smtp", "no-smtp@example.com", None);
    });
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.compose_new();
    app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
    app.composer.subject = "Senza SMTP".to_owned();
    app.send_composer();
    assert_eq!(app.composer.notice.as_deref(), Some(NO_SMTP_SEND_REASON));
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(text.iter().any(|line| line.contains("Questo account non ha un server SMTP")), "{text:?}");
}

#[test]
fn reply_and_forward_prefill_the_composer() {
    let mut app = PostinoApp::demo(true);
    let with_attachment = app
        .engine
        .messages()
        .iter()
        .find(|message| message.has_attachments)
        .map(|message| message.id.clone())
        .expect("a demo message with an attachment");

    app.reply_to(&with_attachment, ComposeMode::Reply);
    assert!(app.composer.open);
    assert_eq!(app.composer.mode, ComposeMode::Reply);
    assert!(app.composer.to.contains("giulia.ferri@example.com"));
    assert!(app.composer.subject.starts_with("Re:"));
    assert!(app.composer.body.contains("ha scritto:"));
    assert!(app.composer.attachments.is_empty(), "reply does not copy the original attachments");

    app.composer = ComposerState::closed();
    app.reply_to(&with_attachment, ComposeMode::Forward);
    assert_eq!(app.composer.mode, ComposeMode::Forward);
    assert!(app.composer.to.is_empty());
    assert!(app.composer.subject.starts_with("Fwd:"));
    assert_eq!(app.composer.attachments.len(), 1, "forward copies the attachment metadata");
}

#[test]
fn an_invalid_recipient_blocks_saving_and_keeps_the_composer_open() {
    let mut app = PostinoApp::demo(true);
    app.compose_new();
    app.composer.to = "Giulia <non-una-mail>".to_owned();
    app.composer.subject = "Non salvabile".to_owned();
    app.composer.touch(1.0);
    assert!(!app.save_composer(true));
    assert!(app.composer.open);
    assert!(app.composer.save_error.is_some(), "the composer explains why it did not save");
    assert!(!app.engine.messages().iter().any(|message| message.subject == "Non salvabile"));
}

#[test]
fn autosave_updates_one_draft_in_place() {
    let mut app = PostinoApp::demo(true);
    let before = app.engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
    app.compose_new();
    app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
    app.composer.subject = "Bozza di prova".to_owned();
    app.composer.body = "Prima versione".to_owned();
    app.composer.touch(1.0);
    assert!(app.save_composer(false));
    let id = app.composer.draft_id.clone().expect("draft id");
    app.composer.body = "Seconda versione".to_owned();
    app.composer.touch(2.0);
    assert!(app.save_composer(false));
    assert_eq!(app.composer.draft_id.as_deref(), Some(id.as_str()));
    assert_eq!(app.engine.message(&id).map(|message| message.body_text), Some("Seconda versione".to_owned()));
    let after = app.engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
    assert_eq!(after, before + 1, "autosave must not duplicate the draft");
}

#[test]
fn closing_keeps_the_draft_and_the_demo_send_is_refused_honestly() {
    let mut app = PostinoApp::demo(true);
    app.compose_new();
    app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();
    app.composer.subject = "Invio demo".to_owned();
    app.composer.body = "Corpo".to_owned();
    app.composer.touch(1.0);
    app.close_composer();
    assert!(!app.composer.open);
    let id = app.engine.messages().iter().find(|message| message.subject == "Invio demo").map(|message| message.id.clone()).expect("saved draft");
    assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.clone()), Some("drafts".to_owned()));

    app.edit_draft(&id);
    app.send_composer();
    assert!(app.composer.open, "the demo send must not close the composer as if the message left");
    assert!(app.composer.save_error.is_none(), "the demo attempted a submission the button refuses: {:?}", app.composer.save_error);
    assert_eq!(app.engine.message(&id).map(|message| message.summary.folder_id.clone()), Some("drafts".to_owned()));
}

#[test]
fn reply_all_excludes_the_account_address() {
    let mut app = PostinoApp::demo(true);
    let sent = app
        .engine
        .messages()
        .iter()
        .find(|message| message.folder_id == "sent" && message.to.len() > 1)
        .map(|message| message.id.clone())
        .expect("a sent message with several recipients");
    app.reply_to(&sent, ComposeMode::ReplyAll);
    assert!(app.composer.to.contains("marco.bianchi@example.com"));
    assert!(app.composer.to.contains("giulia.ferri@example.com"));
    assert!(!app.composer.to.contains("federico@example.it"));
    assert!(!app.composer.cc.contains("federico@example.it"));
}

#[test]
fn a_reply_uses_the_active_account_identity() {
    let mut app = booted_app("p6c-reply-identity", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    app.switch_account("acc_b".to_owned());
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("an acc_b message");
    app.reply_to(&id, ComposeMode::Reply);
    assert!(app.composer.open);
    assert_eq!(app.account().map(|account| account.id.as_str()), Some("acc_b"));
    app.composer.body.push_str("\nRisposta");
    app.composer.touch(0.0);
    assert!(app.save_composer(false));
    let draft = app.composer.draft_id.clone().expect("the reply draft");
    assert_eq!(app.engine.message(&draft).map(|message| message.summary.folder_id.clone()), Some("fld_acc_b_drafts".to_owned()));
}

#[test]
fn escape_while_the_pgp_sheet_is_open_keeps_the_composer_and_closes_only_the_sheet() {
    let (ctx, mut app) = ready_app();
    app.pgp_keys_loaded = true;
    app.pgp_keys = vec![pgp_key("key_ada")];
    app.compose_new();
    app.pgp_prompt = Some(PgpPromptState::new(PgpPromptPurpose::Send));
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Firma con la chiave PGP");
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(app.pgp_prompt.is_none(), "Escape closes the sheet");
    assert!(app.composer.open, "Escape in the sheet must not close the composer behind it");
}

#[test]
fn pgp_send_errors_never_claim_a_queued_message() {
    let wrong = send_error_message("invalid arguments: the passphrase does not unlock the signing key; nothing was queued", false);
    assert!(wrong.contains("passphrase") && !wrong.contains("già in coda"), "wrong passphrase copy: {wrong}");
    let revoked = send_error_message("invalid arguments: a recipient key is revoked; the message was not queued", false);
    assert!(revoked.contains("revocata") && !revoked.contains("già in coda"), "revoked-key copy: {revoked}");
    let expired = send_error_message("invalid arguments: a recipient key is expired; the message was not queued and no cleartext was sent", false);
    assert!(expired.contains("scaduta") && !expired.contains("già in coda"), "expired-key copy: {expired}");
    let missing_key = send_error_message("invalid arguments: an encrypted send needs at least one recipient key", false);
    assert!(missing_key.contains("chiave PGP"), "missing encryption key copy: {missing_key}");
}

#[test]
fn composer_pgp_row_uses_the_stored_keys_and_offers_encryption_without_a_dead_toggle() {
    let (ctx, mut app) = ready_app();
    app.pgp_keys_loaded = true;
    app.pgp_keys = vec![pgp_key("key_ada")];
    app.compose_new();
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Nessuna firma");
    painted_visible(&runs, "Cifra");
    assert!(probe::composer_pgp_rect(&ctx).is_some(), "the composer PGP row must be probe-able");

    app.composer.pgp_sign = Some("key_ada".to_owned());
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Ada Lovelace");
}

#[test]
fn unsubscribe_choices_open_a_draft_and_never_send_from_the_sheet() {
    let (mut app, daemon, _path, _key, _vault) = daemon_app("p13d-unsubscribe", |store| {
        seed_account_rows(store, "acc_u", "u@example.com", None);
        store.upsert_message(&store_message("fld_acc_u_inbox", "acc_u", "msg_u", "Notiziario")).expect("message row");
        store
            .set_meta(
                "unsubscribe:msg_u",
                &json!({
                    "targets": ["<mailto:leave@example.com?subject=unsubscribe>", "<https://example.com/u/1>"],
                    "one_click": true
                })
                .to_string(),
            )
            .expect("unsubscribe meta");
    });
    app.selected = Some("msg_u".to_owned());
    app.reading_open = true;
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let info = app.unsubscribe_info.clone().expect("the stored choices are loaded once");
    assert_eq!(info.targets.len(), 2);
    assert!(info.one_click);
    app.show_unsubscribe = true;
    let size = Vec2::new(980.0, 600.0);
    frame_at(
        &ctx,
        &mut app,
        size,
        vec![egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }],
    );
    assert!(!app.show_unsubscribe, "Escape closes the choice sheet without action");
    app.show_unsubscribe = true;
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "Disiscrizione");
    painted_visible(&runs, "Scrivi la disiscrizione");
    painted_visible(&runs, "Apri la pagina");
    let button = runs.iter().find(|run| run.text.contains("Scrivi la disiscrizione")).expect("the mailto button is painted");
    click_at(&ctx, &mut app, button.rect.center(), size);

    assert!(app.composer.open, "the mailto choice opens the ordinary composer");
    assert_eq!(app.composer.to, "leave@example.com");
    assert_eq!(app.composer.subject, "unsubscribe");
    assert!(!app.show_unsubscribe);
    assert!(app.outbox.entries.is_empty(), "the sheet never queues a send by itself");
    daemon.shutdown().expect("the daemon stops");
}

#[test]
fn the_composer_picker_switches_the_sender_and_its_signature() {
    let (ctx, mut app) = ready_app();
    app.identities_loaded = true;
    app.identities = vec![identity("idn_1", "Ada", "ada@example.com", "Laboratorio")];
    app.compose_new();
    let account_email = app.account().expect("demo account").email.clone();
    let size = Vec2::new(980.0, 600.0);
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "Da");
    painted_visible(&runs, account_email.as_str());
    let from = probe::composer_from_rect(&ctx).expect("the From picker").center();
    click_at(&ctx, &mut app, from, size);

    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "Ada <ada@example.com>");
    let option = runs.iter().find(|run| run.text.contains("ada@example.com")).expect("the identity option").rect.center();
    click_at(&ctx, &mut app, option, size);

    assert_eq!(app.composer.sender.as_deref(), Some("ada@example.com"));
    assert!(app.composer.body.starts_with("-- \nLaboratorio\n\n"), "the identity signature replaces the account one: {:?}", app.composer.body);
    assert!(app.composer.dirty, "choosing a sender is part of the draft");

    app.composer.sender = Some("gone@example.com".to_owned());
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "gone@example.com · mittente non disponibile");
}

#[test]
fn saving_from_an_identity_keeps_the_chosen_sender() {
    let (mut app, daemon, _store_path, _key, _vault) = daemon_app("p14-identities-draft", |store| {
        seed_account_rows(store, "acc_a", "ada@example.com", None);
    });
    let created = app
        .op("identity_create", json!({ "account": "acc_a", "name": "Laboratorio", "email": "lab@example.com", "signature": "Lab" }))
        .expect("create the identity");
    assert_eq!(created["email"], "lab@example.com");
    app.refresh_identities();
    assert_eq!(app.identities.len(), 1, "the settings cache sees the new sender");
    app.compose_new();
    app.composer.to = "grace@example.com".to_owned();
    app.composer.subject = "Dal laboratorio".to_owned();
    app.composer.sender = Some("lab@example.com".to_owned());
    app.composer.dirty = true;
    assert!(app.save_composer(true), "the draft saves through the registry");
    let draft = app.engine.messages().iter().find(|message| message.subject == "Dal laboratorio").cloned().expect("the saved draft");
    let full = app.engine.message(&draft.id).expect("draft body");
    assert_eq!(full.summary.from.email, "lab@example.com");
    daemon.shutdown().expect("the daemon stops");
}

#[test]
fn the_composer_previews_the_compiled_html_and_escape_keeps_the_draft_open() {
    let (ctx, mut app) = ready_app();
    app.compose_new();
    let size = Vec2::new(980.0, 600.0);
    let runs = painted_runs(&ctx, &mut app, size);
    assert!(probe::composer_preview_button_rect(&ctx).is_none(), "plain text has no HTML preview to offer");
    let toggle = runs.iter().find(|run| run.text == "Testo").expect("the body format toggle is painted");
    click_at(&ctx, &mut app, toggle.rect.center(), size);
    assert_eq!(app.composer.body_format, BodyFormat::Html, "the toggle must really switch to HTML");
    app.composer.body = "**Ciao** Ada,\n\n| Voce | Importo |\n| --- | ---: |\n| Progetto | 120 |\n".to_owned();
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "HTML");
    let eye = probe::composer_preview_button_rect(&ctx).expect("HTML mode offers the preview");
    click_at(&ctx, &mut app, eye.center(), size);
    assert!(app.composer_preview, "the eye button opens the preview");
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "Anteprima HTML");
    painted_visible(&runs, "Email-safe");
    painted_visible(&runs, "Ciao");
    painted_visible(&runs, "Progetto");
    let modal = probe::composer_preview_modal_rect(&ctx).expect("the preview is probe-able");
    assert!(ctx.content_rect().contains_rect(modal), "the preview stays inside the 980x600 shell");
    app.dark = true;
    theme::apply(&ctx, true);
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "Email-safe");
    let modal = probe::composer_preview_modal_rect(&ctx).expect("the dark preview is probe-able");
    assert!(ctx.content_rect().contains_rect(modal), "the dark preview stays inside the 980x600 shell");
    app.dark = false;
    theme::apply(&ctx, false);

    frame_at(
        &ctx,
        &mut app,
        size,
        vec![egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }],
    );
    assert!(!app.composer_preview, "Escape closes only the preview sheet");
    assert!(app.composer.open, "the draft remains open with its text");
    assert_eq!(app.composer.body, "**Ciao** Ada,\n\n| Voce | Importo |\n| --- | ---: |\n| Progetto | 120 |\n");
}

#[test]
fn the_composer_offers_local_templates_and_never_discards_an_existing_body() {
    let (ctx, mut app) = ready_app();
    app.templates_loaded = true;
    app.templates = vec![MessageTemplate {
        id: "tpl_1".to_owned(),
        name: "Preventivo".to_owned(),
        subject: "Il tuo preventivo".to_owned(),
        body: "Ciao,\n\nil preventivo è in allegato.".to_owned(),
        created_at: 1,
        updated_at: 1,
    }];
    app.compose_new();
    app.composer.body.clear();
    let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
    painted_visible(&runs, "Modelli");

    app.insert_template("tpl_1");
    assert_eq!(app.composer.subject, "Il tuo preventivo");
    assert_eq!(app.composer.body, "Ciao,\n\nil preventivo è in allegato.");
    assert_eq!(app.composer.focus, Some(ComposerFocus::Body));

    app.composer.body = "Testo già scritto".to_owned();
    app.insert_template("tpl_1");
    assert_eq!(app.composer.body, "Testo già scritto\n\nCiao,\n\nil preventivo è in allegato.");
    app.insert_template("tpl_missing");
    assert_eq!(app.composer.body, "Testo già scritto\n\nCiao,\n\nil preventivo è in allegato.");
}

#[test]
fn pgp_send_request_carries_keys_and_only_the_signed_passphrase() {
    assert_eq!(pgp_send_request(None, &[], None), None);
    let encrypt = vec!["key_b".to_owned()];
    let request = pgp_send_request(None, &encrypt, None).expect("an encrypt-only send");
    assert_eq!(request["encrypt"], json!(["key_b"]));
    assert!(request.get("sign").is_none());
    assert!(request.get("passphrase").is_none());

    let request = pgp_send_request(Some("key_a"), &encrypt, Some("segreta")).expect("a signed send");
    assert_eq!(request["sign"], json!("key_a"));
    assert_eq!(request["passphrase"], json!("segreta"));
}
