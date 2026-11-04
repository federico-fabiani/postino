//! The outbox journal and the send worker.

use super::*;

#[test]
fn the_outbox_renders_every_state_at_980x600_light_and_dark() {
    for dark in [false, true] {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, dark);
        let mut app = PostinoApp::demo_at(true, Utc::now());
        app.outbox = demo_outbox(app.now_utc.timestamp_millis());
        app.outbox_open = true;
        app.outbox_loaded = true;
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
        let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
        for expected in ["Coda di invio", "Invio incerto", "Non riuscito", "Dati demo", "In coda", "Inviato"] {
            assert!(text.iter().any(|line| line.contains(expected)), "the outbox is missing {expected:?}: {text:?}");
        }
    }
}

#[test]
fn ctrl_enter_respects_the_send_enabled_rule() {
    let (ctx, mut app) = ready_app();
    let wide = Vec2::new(1280.0, 720.0);
    app.compose_new();
    app.composer.to = "giulia@example.com".to_owned();
    app.composer.subject = "Scorciatoia".to_owned();
    app.composer.body = "corpo".to_owned();
    app.composer.touch(0.0);
    frame_at(&ctx, &mut app, wide, Vec::new());
    // The demo has no SMTP endpoint: the button is disabled, so the shortcut must do nothing.
    assert!(!app.send_allowed_for_active_account());
    frame_at(&ctx, &mut app, wide, vec![egui::Event::ModifiersChanged(Modifiers::COMMAND), key(Key::Enter, Modifiers::COMMAND)]);
    assert!(app.composer.open, "Ctrl+Enter closed the composer without sending");
    assert!(app.composer.save_error.is_none(), "Ctrl+Enter attempted a send the button would refuse");
}

#[test]
fn a_successful_send_reloads_the_mailbox_and_the_draft_leaves_drafts() {
    let smtp = RunningSmtp::start(1);
    let mut app = smtp_app("p7-app-send-reload", smtp.port(), smtp.ca());
    let draft_id = app_draft(&mut app, "corpo");
    let queued = app.engine.message_send(&draft_id).expect("queue");
    run_send_at(&mut app, queued.outbox.send_at);

    // The worker wrote through its own connection; the app reloaded the mailbox from the store.
    let sent = app.engine.message(&draft_id).expect("the Sent copy");
    assert_eq!(sent.summary.folder_id, "fld_acc_smtp_sent", "the stale draft is still in Drafts");
    assert_eq!(app.engine.messages().iter().filter(|message| message.folder_id == "fld_acc_smtp_sent").count(), 1);
    assert!(!app.engine.messages().iter().any(|message| message.folder_id == "fld_acc_smtp_drafts"), "a draft survived a send");
    // The old draft id is a Sent message now: sending it again must fail.
    assert!(app.engine.message_send(&draft_id).is_err(), "a stale draft could be sent again");
    assert_eq!(app.outbox.entries.iter().filter(|row| row.state == postino_store::OutboxState::AppendPending).count(), 1);
    let submitted = smtp.submitted();
    smtp.finish();
    assert_eq!(submitted.len(), 1);
}

#[test]
fn a_row_queued_by_another_process_is_sent_when_due() {
    let smtp = RunningSmtp::start(1);
    let mut app = smtp_app("p7-app-external-row", smtp.port(), smtp.ca());
    let draft_id = app_draft(&mut app, "da un altro processo");
    // The row is created by a second process (the CLI) after the app booted; the app's cached
    // outbox is empty and knows nothing about it.
    let queued = {
        let mut cli = app.store.as_ref().expect("a store-backed app").open_engine().expect("cli engine");
        cli.message_send(&draft_id).expect("queue")
    };
    app.outbox = empty_outbox();
    app.outbox_loaded = true;
    assert!(app.next_outbox_due().is_some(), "the store query cannot see a row the cache does not have");
    app.now_utc = chrono::DateTime::<Utc>::from_timestamp_millis(queued.outbox.send_at).expect("instant");
    app.poll_outbox();
    assert!(app.send_worker.as_ref().is_some_and(SendWorker::is_running), "the scheduler did not wake for an external row");
    for _ in 0..600 {
        app.handle_send_events();
        if !app.send_worker.as_ref().is_some_and(SendWorker::is_running) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let submitted = smtp.submitted();
    smtp.finish();
    assert_eq!(submitted.len(), 1, "the externally queued row was not submitted");
    // The worker can report completion a frame before the app has consumed the sent row.
    for _ in 0..200 {
        app.poll_outbox();
        app.handle_send_events();
        if app.engine.message(&draft_id).is_some_and(|message| message.summary.folder_id == "fld_acc_smtp_sent") {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(app.engine.message(&draft_id).map(|message| message.summary.folder_id.clone()), Some("fld_acc_smtp_sent".to_owned()));
}

#[test]
fn a_send_from_another_process_reloads_the_app_mailbox() {
    let smtp = RunningSmtp::start(1);
    let mut app = smtp_app("p7-app-external-send", smtp.port(), smtp.ca());
    let draft_id = app_draft(&mut app, "inviata da un altro processo");
    // The CLI (a second engine and store connection) queues and submits it.
    let queued = {
        let mut cli = app.store.as_ref().expect("a store-backed app").open_engine().expect("cli engine");
        cli.set_test_extra_roots(vec![smtp.ca()]);
        let queued = cli.message_send(&draft_id).expect("queue");
        let report = cli.process_outbox(queued.outbox.send_at).expect("send");
        // The IMAP port is closed, so the server copy is pending; the submission happened.
        assert_eq!(report.sent, 0, "{report:?}");
        assert_eq!(report.append_pending, 1, "{report:?}");
        queued
    };
    // Before the poll the app still believes the draft is in Drafts; the engine guard refuses to
    // let that stale copy overwrite the Sent row.
    assert_eq!(app.engine.message(&draft_id).map(|message| message.summary.folder_id.clone()), Some("fld_acc_smtp_drafts".to_owned()));
    let stale = postino_core::DraftContent {
        to: vec![postino_core::Address::new("", "one@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Prova".to_owned(),
        body_text: "una modifica arrivata tardi".to_owned(),
        attachments: Vec::new(),
        in_reply_to: None,
    };
    let refused = app.engine.save_draft(None, Some(&draft_id), stale).expect_err("a stale Drafts save must be refused");
    assert!(
        refused.to_string().contains("changed in another session") || refused.to_string().contains("coda ("),
        "a stale save was not refused with a state reason: {refused}"
    );
    let stored = app.store.as_ref().expect("store").open_store().expect("store").get_message(&draft_id).expect("message").expect("row");
    assert_eq!(stored.summary.folder_id, "fld_acc_smtp_sent", "the stale save overwrote the Sent row");
    // The next scheduler tick sees the delivered row and reloads the whole mailbox.
    app.now_utc = chrono::DateTime::<Utc>::from_timestamp_millis(queued.outbox.send_at).expect("instant");
    app.poll_outbox();
    assert_eq!(app.engine.message(&draft_id).map(|message| message.summary.folder_id.clone()), Some("fld_acc_smtp_sent".to_owned()));
    assert_eq!(app.outbox.entries.iter().filter(|row| row.state == postino_store::OutboxState::AppendPending).count(), 1);
    let submitted = smtp.submitted();
    smtp.finish();
    assert_eq!(submitted.len(), 1);
}

#[test]
fn the_outbox_dialog_offers_a_discard_and_shows_warnings() {
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    crate::theme::apply(&ctx, false);
    let mut app = PostinoApp::demo_at(true, Utc::now());
    app.outbox = demo_outbox(app.now_utc.timestamp_millis());
    app.outbox.warnings.push("una riga della coda è danneggiata".to_owned());
    app.outbox_open = true;
    app.outbox_loaded = true;
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Scarta")), "a failed row has no discard action: {text:?}");
    assert!(text.iter().any(|line| line.contains("danneggiata")), "a warning never reached the journal: {text:?}");
}

#[test]
fn the_status_bar_outbox_button_opens_the_journal() {
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    crate::theme::apply(&ctx, false);
    let mut app = PostinoApp::demo_at(true, Utc::now());
    app.outbox = demo_outbox(app.now_utc.timestamp_millis());
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
    let button = probe::outbox_button_rect(&ctx).expect("the status bar has no outbox button when rows are active");
    assert!(button.width() > 0.0 && button.height() > 0.0);
    app.execute_command(CommandId::Outbox, &ctx);
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
    assert!(app.outbox_open);
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("Riprova manualmente")), "the uncertain row offers no manual retry: {text:?}");
    assert!(text.iter().any(|line| line.contains("Segna come inviato")), "the uncertain row offers no resolution: {text:?}");
}

#[test]
fn send_failures_are_italian_and_never_raw_engine_text() {
    assert!(send_error_message("invalid arguments: draft is already queued", false).contains("già in coda"));
    assert!(send_error_message("account \"acc_1\" has no SMTP endpoint", false).contains("SMTP"));
    assert!(send_error_message("the demo mailbox has no outbox", true).contains("demo"));
    let attachment_refusal = send_error_message("attachment \"x.pdf\" has no stored bytes", false);
    assert!(attachment_refusal.to_lowercase().contains("allegato"), "{attachment_refusal}");
    assert!(attachment_refusal.contains("x.pdf"), "{attachment_refusal}");
    assert!(send_error_message("draft \"d\" has a failed submission; discard that row", false).contains("rifiutato"));
    assert!(!send_error_message("some internal sentence", false).contains("internal"));
}
