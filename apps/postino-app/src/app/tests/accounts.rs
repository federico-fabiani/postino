//! Onboarding, store accounts, sync, the settings dialog and the app lifecycle.

use crate::daemon;

use super::*;

#[test]
fn settings_round_trip_through_the_app_and_never_include_a_draft() {
    let mut app = PostinoApp::demo(true);
    app.compose_new();
    app.composer.subject = "OGGETTO SEGRETO".to_owned();
    app.composer.body = "CORPO SEGRETO".to_owned();
    app.composer.touch(0.0);
    app.dark = true;
    app.system_theme = true;
    app.folder = "archive".to_owned();
    app.reading_ratio = 0.44;
    app.reduced_motion = true;
    let path = crate::settings::test_directory().join(format!("app-round-trip-{}.json", std::process::id()));
    app.settings_path = Some(path.clone());
    app.persist_settings();

    let loaded = Settings::load_from(&path);
    assert!(loaded.dark);
    assert!(loaded.system_theme, "the OS-following choice is a preference");
    assert_eq!(loaded.folder, "archive");
    assert_eq!(loaded.account.as_deref(), Some("acc_demo"), "the active account is a preference");
    assert!((loaded.reading_pane_ratio - 0.44).abs() < f32::EPSILON);
    assert!(loaded.reduced_motion);
    let text = std::fs::read_to_string(&path).expect("the settings file");
    assert!(!text.contains("SEGRETO"), "draft text must never be persisted: {text}");
    let _ = std::fs::remove_file(&path);

    let mut restored = PostinoApp::demo(true);
    restored.apply_settings(loaded);
    assert_eq!(restored.folder, "archive");
    assert_eq!(restored.active_account.as_deref(), Some("acc_demo"));
    assert!(restored.dark);
    assert!(restored.system_theme);
    assert!(restored.reduced_motion);

    // A folder or account the engine does not know falls back to the current account and its
    // inbox.
    let unknown = Settings { folder: "cartella-inesistente".to_owned(), account: Some("acc_inesistente".to_owned()), ..Settings::default() };
    restored.apply_settings(unknown);
    assert_eq!(restored.folder, "inbox");
    assert_eq!(restored.active_account.as_deref(), Some("acc_demo"));
}

#[test]
fn sync_is_enabled_only_for_a_store_account() {
    assert!(command("refresh").is_none(), "the enabled no-op must not exist");
    let sync = command("sync").expect("sync is registered");
    assert_eq!(sync.label, "Aggiorna la posta");
    let app = PostinoApp::demo(true);
    assert!(!app.command_enabled(sync));
    assert!(sync.rule.reason().is_some_and(|reason| reason.contains("account")));
}

#[test]
fn a_first_run_demo_can_return_to_onboarding_and_settings() {
    let mut app = booted_app("demo-return-onboarding", Vec::new(), |_| {});
    assert!(app.onboarding.is_some());
    app.open_demo();
    assert!(app.demo && app.demo_can_return());
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    settle_at(&ctx, &mut app, size);
    let back = probe::demo_return_button_rect(&ctx).expect("the demo status bar offers a way back");
    click_at(&ctx, &mut app, back.center(), size);
    frame_at(&ctx, &mut app, size, Vec::new());
    assert!(!app.demo, "the demo exit did not reopen the store");
    assert!(app.store.is_some());
    assert!(app.onboarding.is_some(), "an empty store returns to first-run onboarding");
    assert_eq!(app.onboarding.as_ref().map(Onboarding::stage), Some(Stage::Welcome));

    // Settings is the second visible path; it must not dead-end in the demo either.
    let mut app = booted_app("demo-return-settings", Vec::new(), |_| {});
    app.open_demo();
    app.open_settings(&ctx, None);
    settle_at(&ctx, &mut app, size);
    let leave = probe::settings_action_rect(&ctx, "leave_demo").expect("Settings offers the archive return");
    click_at(&ctx, &mut app, leave.center(), size);
    frame_at(&ctx, &mut app, size, Vec::new());
    assert!(!app.demo && app.store.is_some() && app.onboarding.is_some());
}

#[test]
fn a_recovery_demo_can_return_to_the_original_store() {
    let mut app = booted_app("demo-return-recovery", Vec::new(), |_| {});
    app.boot_failure = Some(BootFailure {
        path: Some(PathBuf::from("C:/Users/demo/AppData/Local/Postino/mail.db")),
        stage: BootStage::Keyring,
        reason: "locked".to_owned(),
    });
    app.onboarding = None;
    app.open_demo();
    assert!(app.demo && app.boot_failure.is_none());
    app.leave_demo();
    assert!(!app.demo);
    assert!(app.boot_failure.is_none(), "the retained handle reopens without asking the keyring again");
    assert!(app.store.is_some());
    assert!(app.onboarding.is_some(), "the store still has no account");
}

#[test]
fn removing_an_account_prunes_its_saved_and_recent_ui_rows() {
    let mut app = booted_app("p9b-remove-account", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    app.active_account = Some("acc_a".to_owned());
    app.engine.call("saved_search_create", json!({ "account": "acc_a", "name": "Segreta", "query": "is:unread" })).expect("save");
    app.engine.record_recent_search(Some("acc_a"), "subject:segreto").expect("recent");
    app.refresh_saved_searches();
    assert_eq!(app.saved_searches.len(), 1);
    assert_eq!(app.recent_searches.len(), 1);
    app.selected_thread = Some("thr_x".to_owned());
    app.engine.call("account_remove", json!({ "id": "acc_a", "confirm": true })).expect("remove");
    app.after_account_removal("acc_a", None);
    assert!(app.saved_searches.is_empty(), "the removed account's saved searches stay in the sidebar");
    assert!(app.recent_searches.is_empty(), "the removed account's recent chips stay");
    assert!(app.selected_thread.is_none());
    assert_eq!(app.active_account.as_deref(), Some("acc_b"));
}

#[test]
fn an_empty_store_boots_into_onboarding_not_a_fake_mailbox() {
    let mut app = booted_app("app-empty-store", Vec::new(), |_| {});
    assert!(!app.demo);
    assert!(app.engine.accounts().is_empty());
    assert!(app.onboarding.is_some());
    assert_eq!(app.onboarding.as_ref().map(Onboarding::stage), Some(Stage::Welcome));
    assert_eq!(app.onboarding.as_ref().map(Onboarding::mode), Some(onboarding::Mode::FirstRun));
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    let text = output.shapes.iter().any(|shape| match &shape.shape {
        egui::epaint::Shape::Text(text) => text.galley.text().contains("Aggiungi il tuo account"),
        _ => false,
    });
    output.textures_delta.clear();
    assert!(text, "the welcome screen must offer the first-run action");
}

#[test]
fn a_store_failure_renders_the_recovery_screen() {
    let mut app = PostinoApp::with_engine(
        Engine::empty(),
        None,
        false,
        Some(BootFailure {
            path: Some(PathBuf::from("C:/Users/demo/AppData/Local/Postino/mail.db")),
            stage: BootStage::Keyring,
            reason: "the keyring is locked".to_owned(),
        }),
        Launch::Store(None),
        true,
        Utc::now(),
        Vec::new(),
    );
    assert_eq!(app.status_kind(), StatusKind::Error);
    assert!(app.onboarding.is_none());
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    let text = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    output.textures_delta.clear();
    assert!(text.iter().any(|line| line.contains("portachiavi")), "the recovery screen must name the keyring: {text:?}");
    assert!(!text.iter().any(|line| line.contains("the keyring is locked")), "a raw engine sentence reached the recovery card: {text:?}");
    app.open_demo();
    assert!(app.demo);
    assert!(app.boot_failure.is_none());
    assert!(app.engine.is_demo());
}

#[test]
fn demo_is_still_a_labelled_fallback() {
    let app = PostinoApp::demo(true);
    assert!(app.demo);
    assert_eq!(app.status_kind(), StatusKind::Demo);
    assert!(app.sync_text().is_none(), "the demo has no sync state to claim");
}

#[test]
fn switching_and_removing_accounts_keeps_data_and_credentials_isolated() {
    let mut app = booted_app("app-accounts", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    assert_eq!(app.engine.accounts().len(), 2);
    assert_eq!(app.active_account.as_deref(), Some("acc_a"));
    assert!(app.visible_messages().0.iter().any(|message| message.subject.contains("acc_a")));
    assert!(!app.visible_messages().0.iter().any(|message| message.subject.contains("acc_b")));

    app.switch_account("acc_b".to_owned());
    assert_eq!(app.active_account.as_deref(), Some("acc_b"));
    assert_eq!(app.folder, "fld_acc_b_inbox");
    assert!(app.visible_messages().0.iter().any(|message| message.subject.contains("acc_b")));
    assert!(!app.visible_messages().0.iter().any(|message| message.subject.contains("acc_a")));

    let state = AccountRemoveState {
        id: "acc_b".to_owned(),
        name: "Account acc_b".to_owned(),
        email: "b@example.com".to_owned(),
        last: false,
        focus_cancel: true,
    };
    app.remove_account(&state);
    assert_eq!(app.engine.accounts().len(), 1);
    assert_eq!(app.active_account.as_deref(), Some("acc_a"));
    assert!(app.engine.messages().iter().all(|message| !message.id.contains("acc_b")));

    let state = AccountRemoveState {
        id: "acc_a".to_owned(),
        name: "Account acc_a".to_owned(),
        email: "a@example.com".to_owned(),
        last: true,
        focus_cancel: true,
    };
    app.remove_account(&state);
    assert_eq!(app.engine.accounts().len(), 0);
    assert!(app.onboarding.is_some(), "removing the last account returns to onboarding");
    assert!(app.active_account.is_none());
    assert!(app.sync_worker.is_none());
}

#[test]
fn a_second_sync_request_says_it_is_already_running() {
    let server = RunningServer::mailbox(three_folders(), 1);
    let port = server.port();
    let mut app = booted_app("app-second-sync", vec![server.ca()], |store| {
        seed_account_rows(store, "acc_sync", postino_testkit::USER, Some(port));
    });
    set_test_password(&app, "acc_sync");
    app.request_sync();
    assert!(matches!(app.sync_view, SyncView::Running(_)));
    app.request_sync();
    assert_eq!(app.status, "Sincronizzazione già in corso");
    wait_for_sync(&mut app);
    server.finish();
}

#[test]
fn a_failed_sync_is_offline_and_never_empties_the_list() {
    let mut app = booted_app("app-offline", Vec::new(), |store| {
        seed_account_rows(store, "acc_offline", postino_testkit::USER, Some(1));
    });
    set_test_password(&app, "acc_offline");
    app.last_sync = Some(1_700_000_000_000);
    let before = app.visible_messages().0.len();
    assert_eq!(before, 1);
    app.request_sync();
    wait_for_sync(&mut app);
    assert!(matches!(app.sync_view, SyncView::Offline { last_sync: Some(1_700_000_000_000) }), "{:?}", app.sync_view);
    assert_eq!(app.visible_messages().0.len(), before, "an offline sync keeps the cached mail");
    assert_eq!(app.status_kind(), StatusKind::Offline, "offline is its own honest state, not a red error");
}

#[test]
fn a_real_onboarding_creates_the_account_and_first_sync() {
    let server = RunningServer::mailbox(three_folders(), 3);
    let roots = vec![server.ca()];
    let port = server.port();
    let mut app = booted_app("app-onboarding-real", roots.clone(), |_| {});
    {
        let onboarding = app.onboarding.as_mut().expect("first run");
        onboarding.seed_for_test(Stage::Credentials, ProviderChoice::Custom, None, None);
        onboarding.set_custom("localhost", &port.to_string());
        onboarding.set_fields("Postino Test", postino_testkit::USER, postino_testkit::PASSWORD);
        onboarding.set_probe_timeout(Duration::from_secs(3));
        onboarding.start_probe(roots.clone(), false);
    }
    for _ in 0..500 {
        let onboarding = app.onboarding.as_mut().expect("onboarding");
        onboarding.poll_probe();
        if !onboarding.is_testing() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let action = {
        let onboarding = app.onboarding.as_mut().expect("onboarding");
        assert!(onboarding.connection_error().is_none(), "{:?}", onboarding.connection_error());
        let action = onboarding.advance(roots.clone());
        assert!(matches!(action, onboarding::Action::None));
        onboarding.advance(roots.clone())
    };
    assert!(matches!(action, onboarding::Action::AddAccount(_)));
    app.handle_onboarding_action(action);
    for _ in 0..1500 {
        app.poll_add_account();
        app.handle_sync_events();
        if app.onboarding.is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.onboarding.is_none(), "the first sync closes the wizard");
    assert_eq!(app.engine.accounts().len(), 1);
    assert_eq!(app.engine.messages().len(), 3);
    assert!(app.engine.messages().iter().all(|message| message.folder_id != "inbox"));
    assert!(matches!(app.sync_view, SyncView::Idle { last_sync: Some(_) }));
    server.finish();
}

#[test]
fn a_successful_sync_reloads_without_losing_selection_filters_or_the_open_message() {
    let server = RunningServer::mailbox(three_folders(), 2);
    let port = server.port();
    let mut app = booted_app("app-reload", vec![server.ca()], |store| {
        seed_account_rows(store, "acc_reload", postino_testkit::USER, Some(port));
    });
    set_test_password(&app, "acc_reload");
    app.request_sync();
    wait_for_sync(&mut app);
    let rows = app.visible_messages().0;
    let selected = rows.first().map(|message| message.id.clone()).expect("synced mail");
    app.open_message(selected.clone());
    app.query = "Synthetic".to_owned();
    app.unread_only = true;
    assert!(app.reading_open);
    app.request_sync();
    wait_for_sync(&mut app);
    assert_eq!(app.selected.as_deref(), Some(selected.as_str()), "the open message survives the reload");
    assert!(app.reading_open);
    assert_eq!(app.query, "Synthetic");
    assert!(app.unread_only);
    assert!(app.engine.message(&selected).is_some());
    server.finish();
}

#[test]
fn compose_uses_the_active_account_identity_and_drafts_folder() {
    let mut app = booted_app("app-compose-identity", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    app.switch_account("acc_b".to_owned());
    app.compose_new();
    assert_eq!(app.account().map(|account| account.id.as_str()), Some("acc_b"));
    app.composer.to = "Destinatario <dest@example.com>".to_owned();
    app.composer.subject = "Bozza dell'account attivo".to_owned();
    app.composer.body = "Corpo".to_owned();
    app.composer.touch(0.0);
    assert!(app.save_composer(false));
    let draft = app.composer.draft_id.clone().expect("a saved draft");
    assert_eq!(app.engine.message(&draft).map(|message| message.summary.folder_id.clone()), Some("fld_acc_b_drafts".to_owned()));
    assert_eq!(
        app.engine.message(&draft).map(|message| message.summary.to.first().map(|address| address.email.clone())),
        Some(Some("dest@example.com".to_owned()))
    );
}

#[test]
fn the_account_menu_and_settings_dialog_render_at_the_minimum_size() {
    let mut app = booted_app("app-account-menu", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.show_accounts_dialog = true;
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    app.show_accounts_dialog = false;
    app.show_accounts_menu = true;
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    app.show_accounts_menu = false;
    app.account_remove = Some(AccountRemoveState {
        id: "acc_b".to_owned(),
        name: "Account acc_b".to_owned(),
        email: "b@example.com".to_owned(),
        last: false,
        focus_cancel: true,
    });
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    assert!(app.account_remove.is_some(), "Esc was not pressed: the confirmation stays open");
}

#[test]
fn a_store_mailbox_renders_at_the_minimum_size_and_in_dark() {
    let mut app = booted_app("app-mailbox-980", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
    });
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    for dark in [false, true] {
        theme::apply(&ctx, dark);
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        assert!(!output.shapes.is_empty(), "the store-backed mailbox painted nothing (dark={dark})");
        output.textures_delta.clear();
    }
}

#[test]
fn the_starred_view_is_scoped_to_the_active_account() {
    let mut app = booted_app("app-starred", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    app.switch_account("acc_b".to_owned());
    let starred = app.visible_messages().0.first().map(|message| message.id.clone()).expect("an acc_b message");
    app.set_starred_for(&starred, true);
    assert_eq!(app.starred_counts("acc_b"), (1, 1));
    app.switch_account("acc_a".to_owned());
    assert_eq!(app.starred_counts("acc_a"), (0, 0));
    app.select_folder("fld_acc_a_starred");
    assert!(app.visible_messages().0.is_empty(), "another account's star must not leak into Preferiti");
}

#[test]
fn settings_renders_every_section_at_the_minimum_size_light_and_dark() {
    let (mut app, _, _) = seeded_attachment_app("p6c-settings-980", "report.pdf", None);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    app.background = Box::new(background::tests::RecorderBridge::with_rows());
    app.background_rows = None;
    app.connection = daemon::Connection::Connected(daemon::Hello { pid: 7, version: "test".to_owned() });
    app.open_settings(&ctx, None);
    for dark in [false, true] {
        app.dark = dark;
        theme::apply(&ctx, dark);
        // Start every mode at the top: the previous mode may have scrolled to the footer.
        if let Some(scroll) = probe::settings_scroll_view(&ctx) {
            frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), wheel_by(scroll.center(), 40, 6.0));
        }
        let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
        // Capture after the first settled frame: `Context::default()` starts with a huge
        // placeholder viewport, which would make the light assertions vacuous.
        let content = ctx.content_rect();
        let text = runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>();
        for section in ["ACCOUNT", "ASPETTO", "SISTEMA", "Barra delle applicazioni", "Notifiche", "Anteprime", "Avvio automatico"] {
            assert!(text.iter().any(|line| line.contains(section)), "missing {section}: {text:?}");
        }
        assert!(text.iter().any(|line| line.contains("Ultimo sync")), "the account row names its last sync: {text:?}");
        let scroll = probe::settings_scroll_view(&ctx).expect("the settings scroll viewport");
        assert_eq!(probe::settings_scroll_offset(&ctx), Some(0.0), "the settings shot starts at the top");
        let sections = probe::settings_sections(&ctx);
        for section in ["ACCOUNT", "ASPETTO", "SISTEMA"] {
            let rect = sections.get(section).copied().unwrap_or_else(|| panic!("{section} was not probed"));
            assert!(content.contains_rect(rect), "{section} is outside the window: {rect:?}");
            assert!(scroll.contains_rect(rect), "{section} is clipped by the scroll viewport: {rect:?} in {scroll:?}");
            let run = runs
                .iter()
                .find(|run| run.text == section || run.text.starts_with(section))
                .unwrap_or_else(|| panic!("{section} text run missing: {text:?}"));
            assert!(run.clip.contains_rect(run.rect), "{section} text is clipped: {run:?}");
        }
        for key in ["tray", "notify", "previews", "autostart"] {
            let action = probe::settings_action_rect(&ctx, key).unwrap_or_else(|| panic!("the {key} switch was not probed"));
            assert!(scroll.contains_rect(action), "the {key} switch is clipped at 980x600: {action:?} in {scroll:?}");
        }
        // The added section moves the last two sections below the fold; a real wheel scroll
        // reaches them and the footer never leaves the window.
        frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), wheel(scroll.center(), 40));
        assert!(probe::settings_scroll_offset(&ctx).unwrap_or(0.0) > 0.0, "the settings page must scroll");
        let runs = painted_runs(&ctx, &mut app, Vec2::new(980.0, 600.0));
        let text = runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>();
        let sections = probe::settings_sections(&ctx);
        for section in ["DATI", "INFORMAZIONI"] {
            let rect = sections.get(section).copied().unwrap_or_else(|| panic!("{section} was not probed"));
            assert!(scroll.contains_rect(rect), "{section} is not reachable at 980x600: {rect:?} in {scroll:?}");
        }
        let footer = probe::settings_footer_rect(&ctx).expect("the settings footer");
        assert!(content.contains_rect(footer), "the footer is off-screen at 980x600: {footer:?}");
        for key in ["store", "blob"] {
            let copy = probe::settings_copy_rect(&ctx, key).expect("each path has a copy action");
            assert!(content.contains_rect(copy), "the {key} copy action is off-screen: {copy:?}");
        }
        for label in ["Copia archivio", "Copia allegati", "Licenze e attribuzioni", "Scorciatoie da tastiera"] {
            let run = runs.iter().find(|run| run.text.contains(label)).unwrap_or_else(|| panic!("{label} text run missing: {text:?}"));
            assert!(scroll.contains_rect(run.rect), "{label} is clipped at 980x600: {:?} in {scroll:?}", run.rect);
            assert!(run.clip.contains_rect(run.rect), "{label} text is clipped: {run:?}");
        }
    }
}

#[test]
fn settings_background_rows_show_live_state_and_write_back() {
    let (mut app, _, _) = seeded_attachment_app("p10b2-settings-rows", "report.pdf", None);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    app.background = Box::new(background::tests::RecorderBridge::with_rows());
    app.background_rows = None;
    app.connection = daemon::Connection::Connected(daemon::Hello { pid: 7, version: "test".to_owned() });
    app.open_settings(&ctx, None);
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let rows = app.background_rows.clone().expect("the rows were read");
    assert!(rows.tray.available && rows.tray.on);
    assert!(rows.notify.available && rows.notify.on);
    assert!(rows.previews.available && !rows.previews.on);
    assert!(rows.autostart.available && !rows.autostart.on);

    // A click writes through the bridge; the next frame shows the value read back from it.
    let scroll = probe::settings_scroll_view(&ctx).expect("the settings scroll viewport");
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), wheel(scroll.center(), 12));
    let rect = probe::settings_action_rect(&ctx, "tray").expect("the tray switch");
    assert!(scroll.contains_rect(rect), "the tray switch must be clickable: {rect:?} in {scroll:?}");
    click_at(&ctx, &mut app, rect.center(), Vec2::new(980.0, 600.0));
    let rows = app.background_rows.clone().expect("the rows were re-read");
    assert!(!rows.tray.on, "the switch must show the stored value, not the click");
    assert!(rows.notify.on);

    // Anteprime is a real dependent row, not a dead switch.
    let mut off = background::tests::RecorderBridge::with_rows();
    if let Some(daemon) = off.daemon.as_mut() {
        daemon.notify_enabled = false;
        daemon.notify_previews = true;
    }
    app.background = Box::new(off);
    app.background_rows = None;
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let rows = app.background_rows.clone().expect("the rows were re-read");
    assert!(!rows.previews.available);
    assert_eq!(rows.previews.reason.as_deref(), Some("Attiva prima le notifiche."));

    // No daemon: the three daemon rows are visibly unavailable, autostart still answers.
    app.background = Box::new(background::tests::RecorderBridge::default());
    app.background_rows = None;
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![]);
    let rows = app.background_rows.clone().expect("the rows were re-read");
    assert!(!rows.tray.available);
    assert!(rows.tray.reason.as_deref().is_some_and(|reason| reason.contains("daemon")));
    assert!(rows.autostart.available);
}

#[test]
fn the_first_hide_notice_is_persisted_and_read_back() {
    let ctx = egui::Context::default();
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("settings.json");
    let mut app = PostinoApp::demo_at(true, screenshot_clock().expect("clock"));
    app.settings_path = Some(path.clone());
    assert!(!app.tray_notice_shown);

    app.hide_to_tray(&ctx);
    assert!(app.tray_notice_shown, "the explanation is accepted once");
    assert!(app.hidden);
    assert!(!app.settings_dirty, "the notice flag is written immediately, not with the next autosave");
    let saved = Settings::load_from(&path);
    assert!(saved.tray_notice_shown, "a relaunch must know the explanation was already shown");

    // A second hide is silent and keeps the persisted flag: the dialog cannot come back.
    let mut relaunched = PostinoApp::demo_at(true, screenshot_clock().expect("clock"));
    relaunched.settings_path = Some(path.clone());
    relaunched.apply_settings(saved);
    assert!(relaunched.tray_notice_shown, "the flag reads back on the next launch");
    relaunched.hide_to_tray(&ctx);
    assert!(Settings::load_from(&path).tray_notice_shown);
}

#[test]
fn closing_hides_only_with_a_live_tray_and_exits_otherwise() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    let mut app = PostinoApp::demo(true);
    app.background = Box::new(background::tests::RecorderBridge::with_rows());
    app.background_rows = None;
    app.connection = daemon::Connection::Connected(daemon::Hello { pid: 7, version: "test".to_owned() });
    app.tray_notice_shown = false;
    let size = Vec2::new(980.0, 600.0);

    // First close with the tray on asks once instead of hiding silently.
    let output = close_request(&ctx, &mut app, size);
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::CancelClose)));
    assert!(app.close_prompt);
    assert!(!app.allow_close);

    // Once explained, closing hides; the notice is persisted so it never asks again.
    app.close_prompt = false;
    app.tray_notice_shown = true;
    let output = close_request(&ctx, &mut app, size);
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::Visible(false))));

    // Tray disabled: the close is the exit, not a hide.
    let mut off = background::tests::RecorderBridge::with_rows();
    if let Some(daemon) = off.daemon.as_mut() {
        daemon.tray = false;
    }
    app.background = Box::new(off);
    app.background_rows = None;
    app.tray_notice_shown = true;
    let output = close_request(&ctx, &mut app, size);
    assert!(app.allow_close);
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::Close)));
}

#[test]
fn the_open_flag_reaches_the_journal_through_the_launch_path() {
    // The parser and the launch share one vocabulary, so `--open outbox` cannot parse into a
    // view the window never applies.
    let startup = StartupView::parse("outbox").expect("the parser accepts the tray view");
    let mut app = PostinoApp::demo(true);
    app.apply_launch_view(startup);
    assert!(app.outbox_open, "--open outbox must open the Coda di invio");
    assert!(StartupView::parse("sent").is_none(), "only the supported view parses");
}

#[test]
fn ctrl_q_is_a_real_quit_shortcut() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);
    assert!(!app.allow_close);
    let output = frame_output(&ctx, &mut app, vec![key(Key::Q, Modifiers::COMMAND)]);
    assert!(app.allow_close, "Ctrl+Q is the advertised quit and must reach the handler");
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::Close)));
}

#[test]
fn a_tray_activation_focuses_the_hidden_window_and_quit_exits() {
    let (ctx, mut app) = ready_app();
    frame(&ctx, &mut app, vec![]);

    app.hidden = true;
    app.apply_daemon_event(json!({ "kind": "activation", "data": { "action": "open" } }));
    let output = frame_output(&ctx, &mut app, vec![]);
    assert!(!app.hidden, "Apri Postino must bring the hidden window back");
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::Visible(true))));
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::Focus)));

    app.hidden = true;
    app.outbox_open = false;
    app.apply_daemon_event(json!({ "kind": "activation", "data": { "action": "outbox" } }));
    let output = frame_output(&ctx, &mut app, vec![]);
    assert!(app.outbox_open, "Coda di invio must open the journal it names");
    assert!(!app.hidden);
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::Visible(true))));

    app.apply_daemon_event(json!({ "kind": "activation", "data": { "action": "quit" } }));
    let output = frame_output(&ctx, &mut app, vec![]);
    assert!(app.allow_close, "Esci da Postino is explicit: it must not raise the close prompt");
    assert!(commands(&output).iter().any(|command| matches!(command, egui::ViewportCommand::Close)));
}

#[test]
fn settings_rows_are_dropped_on_close_and_re_read_on_open() {
    let (mut app, _, _) = seeded_attachment_app("p10b2-settings-cache", "report.pdf", None);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    app.background = Box::new(background::tests::RecorderBridge::with_rows());
    app.connection = daemon::Connection::Connected(daemon::Hello { pid: 7, version: "test".to_owned() });

    app.open_settings(&ctx, None);
    frame(&ctx, &mut app, vec![]);
    assert!(app.background_rows.as_ref().is_some_and(|rows| rows.tray.available && rows.tray.on));

    app.close_settings(&ctx);
    assert!(app.background_rows.is_none(), "closing Settings must drop the cached rows");

    // The daemon died while the page was closed: the next open must say so instead of serving
    // the last live bool.
    app.connection = daemon::Connection::InProcess;
    app.background_rows = Some(background::Rows::read(
        Ok(background::DaemonView { tray: true, tray_available: true, tray_running: true, ..background::DaemonView::default() }),
        Ok(background::AutostartView::default()),
    ));
    app.open_settings(&ctx, None);
    assert!(app.background_rows.is_none(), "opening Settings must not reuse the previous read");
    frame(&ctx, &mut app, vec![]);
    let rows = app.background_rows.as_ref().expect("the rows are re-read on open");
    assert!(!rows.tray.available, "a dead daemon must disable the tray row, not show a stale switch");
    assert!(rows.tray.reason.as_deref().is_some_and(|reason| reason.contains("daemon")));
}

#[test]
fn demo_mode_disables_autostart_with_its_own_reason() {
    let mut app = PostinoApp::demo(true);
    app.background = Box::new(background::tests::RecorderBridge::with_rows());
    app.connection = daemon::Connection::Demo;
    app.refresh_background();
    let rows = app.background_rows.as_ref().expect("the demo still reads its platform row");
    assert!(!rows.autostart.available, "the demo has no store to start the daemon with");
    let reason = rows.autostart.reason.as_deref().unwrap_or_default();
    assert!(reason.contains("archivio locale"), "the demo reason must name the missing archive: {reason}");
    assert!(!reason.contains("accanto all'app"), "the demo must not get the missing-binary sentence: {reason}");
    assert!(!rows.autostart.on, "a disabled demo row never claims to be on");
}

#[test]
fn the_settings_screenshot_state_has_the_same_unread_total_at_both_sizes() {
    let clock = screenshot_clock().expect("the frozen screenshot clock");
    for size in [Vec2::new(980.0, 600.0), Vec2::new(1600.0, 1000.0)] {
        let mut app = PostinoApp::demo_at(true, clock);
        app.seed_screenshot(ScreenshotState::Settings);
        assert_eq!(app.account_unread("acc_demo"), 5, "the settings fixture must not read a message at {size:?}");
        let (_, total, unread) = app.visible_messages();
        assert_eq!((total, unread), (7, 4), "the inbox total must be identical at {size:?}");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let text = painted_text(&ctx, &mut app, size);
        assert!(!text.iter().any(|line| line.contains("5 non letti")), "the account row must not repeat the account total at {size:?}: {text:?}");
        assert!(text.iter().any(|line| line == "4"), "the inbox folder badge still carries its unread count at {size:?}: {text:?}");
    }
}

#[test]
fn settings_closes_with_escape_and_returns_focus_to_the_account_switcher() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let mut app = PostinoApp::demo(true);
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    let switcher = app.account_switcher_id.expect("the sidebar account row");
    app.open_settings(&ctx, None);
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert!(app.show_accounts_dialog);
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.show_accounts_dialog);
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert_eq!(ctx.memory(|memory| memory.focused()), Some(switcher), "focus returns to the opener");
}

#[test]
fn settings_shows_the_store_path_and_uses_the_injected_folder_opener() {
    let (mut app, _, _) = seeded_attachment_app("p6c-settings-data", "report.pdf", None);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.open_settings(&ctx, None);
    settle_at(&ctx, &mut app, Vec2::new(980.0, 600.0));
    let scroll = probe::settings_scroll_view(&ctx).expect("the settings scroll viewport");
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), wheel(scroll.center(), 40));
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("mail.db")), "the store path is visible: {text:?}");
    assert!(text.iter().any(|line| line.contains("mail.db.blobs")), "the blob path is visible: {text:?}");
    let opener = RecordingFolderOpener::new();
    app.folder_opener = opener.clone();
    app.open_data_folder();
    let opened = opener.opened();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].ends_with("mail.db.blobs"), "{:?}", opened[0]);
}

#[test]
fn settings_actions_are_real_buttons() {
    let (mut app, _, _) = seeded_attachment_app("p6c-settings-actions", "report.pdf", Some(b"bytes"));
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let size = Vec2::new(980.0, 600.0);
    let store_path = app.store_path.clone().expect("the store path");
    let store = store_path.display().to_string();
    let blob = postino_store::blob_root(&store_path).display().to_string();
    let _ = std::fs::create_dir_all(postino_store::blob_root(&store_path));
    app.open_settings(&ctx, None);

    // The background section moved the data actions below the fold; a wheel reaches them.
    settle_at(&ctx, &mut app, size);
    let scroll = probe::settings_scroll_view(&ctx).expect("the settings scroll viewport");
    frame_at(&ctx, &mut app, size, wheel(scroll.center(), 40));
    settle_at(&ctx, &mut app, size);
    let copy = probe::settings_copy_rect(&ctx, "store").expect("Copia archivio");
    let output = click_at_output(&ctx, &mut app, copy.center(), size);
    assert_eq!(copied_text(&output).as_deref(), Some(store.as_str()), "Copia archivio puts the path on the clipboard");

    settle_at(&ctx, &mut app, size);
    let copy = probe::settings_copy_rect(&ctx, "blob").expect("Copia allegati");
    let output = click_at_output(&ctx, &mut app, copy.center(), size);
    assert_eq!(copied_text(&output).as_deref(), Some(blob.as_str()), "Copia allegati puts the blob path on the clipboard");

    let opener = RecordingFolderOpener::new();
    app.folder_opener = opener.clone();
    settle_at(&ctx, &mut app, size);
    let open = probe::settings_action_rect(&ctx, "open_folder").expect("Apri la cartella");
    click_at(&ctx, &mut app, open.center(), size);
    assert_eq!(opener.opened(), vec![postino_store::blob_root(&store_path)], "the real folder button calls the injected opener");

    settle_at(&ctx, &mut app, size);
    let scroll = probe::settings_scroll_view(&ctx).expect("the settings scroll viewport");
    frame_at(&ctx, &mut app, size, wheel(scroll.center(), 120));
    settle_at(&ctx, &mut app, size);
    let licences = probe::settings_action_rect(&ctx, "licences").expect("Licenze e attribuzioni");
    assert!(scroll.contains_rect(licences), "Licenze is clipped: {licences:?} in {scroll:?}");
    click_at(&ctx, &mut app, licences.center(), size);
    assert!(app.show_licences, "the real Licenze button opens the local dialog: {licences:?}");
    frame_at(&ctx, &mut app, size, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(!app.show_licences);
    // Reopen settings: the nested licences modal and the settings modal both see Escape, so the
    // test reopens rather than asserting which modal closed first.
    app.open_settings(&ctx, None);

    settle_at(&ctx, &mut app, size);
    if let Some(scroll) = probe::settings_scroll_view(&ctx) {
        frame_at(&ctx, &mut app, size, wheel(scroll.center(), 40));
    }
    let shortcuts = probe::settings_action_rect(&ctx, "shortcuts").expect("Scorciatoie da tastiera");
    click_at(&ctx, &mut app, shortcuts.center(), size);
    assert!(app.show_shortcuts, "the real shortcut button opens the sheet");
}

#[test]
fn settings_shows_each_accounts_own_last_sync() {
    let now = Utc::now();
    let first = (now - chrono::Duration::hours(2)).timestamp_millis();
    let second = (now - chrono::Duration::minutes(20)).timestamp_millis();
    let mut app = booted_app("p6c-settings-sync", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
        for (account, last_success) in [("acc_a", first), ("acc_b", second)] {
            let status = postino_sync::SyncStatus {
                state: postino_sync::SyncPhase::Idle,
                last_attempt: last_success,
                last_success: Some(last_success),
                error: None,
                folders_done: 1,
                folders_total: 1,
                messages: 1,
            };
            store.set_meta(&postino_sync::status_key(account), &serde_json::to_string(&status).expect("status JSON")).expect("seed the sync status");
        }
    });
    let expected_first = format!("Ultimo sync: {}", format_millis(first, app.now_utc));
    let expected_second = format!("Ultimo sync: {}", format_millis(second, app.now_utc));
    assert_ne!(expected_first, expected_second);
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.open_settings(&ctx, None);
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(text.iter().any(|line| line.contains(&expected_first) && line.contains("a@example.com")), "acc_a shows its own sync: {text:?}");
    assert!(text.iter().any(|line| line.contains(&expected_second) && line.contains("b@example.com")), "acc_b shows its own sync: {text:?}");
}

#[test]
fn the_palette_entry_opens_settings_and_the_licences_dialog_is_local() {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let mut app = PostinoApp::demo(true);
    assert!(command("settings").is_some());
    app.execute_command(CommandId::Settings, &ctx);
    assert!(app.show_accounts_dialog);
    app.open_licences(&ctx);
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    assert!(text.iter().any(|line| line.contains("Licenze e attribuzioni")));
    assert!(text.iter().any(|line| line.contains("Postino onboarding welcome") || line.contains("Phosphor")), "the embedded attribution is shown");
}

#[test]
fn the_account_removal_button_warns_and_starts_on_cancel() {
    let mut app = booted_app("p6c-settings-remove", Vec::new(), |store| {
        seed_account_rows(store, "acc_a", "a@example.com", None);
        seed_account_rows(store, "acc_b", "b@example.com", None);
    });
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.open_settings(&ctx, None);
    settle_at(&ctx, &mut app, Vec2::new(980.0, 600.0));
    let remove = probe::account_remove_button_rect(&ctx, "acc_b").expect("the Rimuovi button");
    click_at(&ctx, &mut app, remove.center(), Vec2::new(980.0, 600.0));
    assert!(app.account_remove.is_some(), "the confirmation sheet");
    let text = painted_text(&ctx, &mut app, Vec2::new(980.0, 600.0));
    assert!(text.iter().any(|line| line.contains("portachiavi")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("cache cifrata degli allegati")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("Le mail sul server non vengono toccate")), "{text:?}");
    let cancel_id = probe::dialog_button_focus(&ctx, "Rimuovere l'account?", false).expect("the Annulla button");
    assert_eq!(ctx.memory(|memory| memory.focused()), Some(cancel_id), "Enter must land on Annulla");
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(app.account_remove.is_none());
    assert_eq!(app.engine.accounts().len(), 2, "Enter on Annulla removes nothing");

    settle_at(&ctx, &mut app, Vec2::new(980.0, 600.0));
    let remove = probe::account_remove_button_rect(&ctx, "acc_b").expect("the Rimuovi button");
    click_at(&ctx, &mut app, remove.center(), Vec2::new(980.0, 600.0));
    click_dialog_button(&ctx, &mut app, Vec2::new(980.0, 600.0), "Rimuovere l'account?", true);
    assert_eq!(app.engine.accounts().len(), 1, "the real Rimuovi button removes the account");
}

#[test]
fn the_account_row_gives_the_name_the_full_width_and_leaves_unread_to_the_badges() {
    let mut app = booted_app("p9c-account-row-name", Vec::new(), |store| {
        seed_account_rows(store, "acc_federico", "studio@federicofabiani.it", None);
    });
    let name = app.active_account().map(|account| account.name.clone()).expect("the seeded account");
    let unread = app.account_unread("acc_federico");
    assert_eq!(name, "Account acc_federico");
    assert!(unread > 0, "the seeded inbox is unread, so the old row painted a total beside the name");
    let repeated = unread_text(unread as usize);

    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    for dark in [false, true] {
        theme::apply(&ctx, dark);
        app.dark = dark;
        for size in [Vec2::new(1280.0, 900.0), Vec2::new(980.0, 600.0)] {
            let runs = painted_runs(&ctx, &mut app, size);
            let row = runs.iter().find(|run| run.text.starts_with("Account acc_")).unwrap_or_else(|| {
                panic!("the account name is painted at {size:?} (dark={dark}): {:?}", runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>())
            });
            assert_eq!(row.text, name, "the name keeps the full row width instead of truncating under a count (dark={dark}, {size:?})");

            // The account block is the only sidebar content above the scrolling folders and labels.
            let panel = probe::sidebar_panel_rect(&ctx).expect("the sidebar panel");
            let view = probe::scroll_view(&ctx).expect("the sidebar scroll area");
            let header: Vec<&str> = runs
                .iter()
                .filter(|run| run.rect.max.y <= view.min.y + 1.0 && run.rect.min.x >= panel.min.x - 1.0 && run.rect.max.x <= panel.max.x + 1.0)
                .map(|run| run.text.as_str())
                .collect();
            assert!(
                !header.iter().any(|text| text.contains("non lett") || text.contains(&repeated)),
                "the account block must not repeat the folder badge (dark={dark}, {size:?}): {header:?}"
            );
        }
    }

    // The shipped demo account too: its name is painted whole, never merged with a count.
    let (ctx, mut demo) = ready_app();
    let demo_name = demo.active_account().map(|account| account.name.clone()).expect("the demo account");
    let runs = painted_runs(&ctx, &mut demo, Vec2::new(980.0, 600.0));
    assert!(
        runs.iter().any(|run| run.text == demo_name),
        "the demo account name {demo_name:?} is painted whole: {:?}",
        runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn there_is_one_identity_and_one_offline_retry() {
    let (mut app, _, _) = seeded_attachment_app("p6c-chrome", "report.pdf", None);
    let email = app.engine.accounts().first().map(|account| account.email.clone()).expect("the seeded account");
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert_eq!(probe::identity_count(&ctx), 1, "the account identity exists once (sidebar)");
    let text = painted_text(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    let identity_lines = text.iter().filter(|line| line.contains(&email)).count();
    assert_eq!(identity_lines, 1, "the account e-mail must appear once in the chrome: {text:?}");
    assert!(!text.iter().any(|line| line.contains("1 non letto")), "the account row must not repeat the folder badge on the name line: {text:?}");
    assert!(text.iter().any(|line| line == "1"), "the inbox folder badge names unread mail: {text:?}");
    assert!(account_unread_tooltip(1).contains("tutte le cartelle"));
    app.sync_view = SyncView::Offline { last_sync: Some(1_700_000_000_000) };
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert_eq!(probe::retry_count(&ctx), 1, "only the offline banner offers Retry");
    // Click the banner's Retry. With no worker, `request_sync` writes its refusal; a dead button
    // would leave the status empty.
    app.sync_worker = None;
    let banner_retry = probe::retry_rects(&ctx)[0];
    click_at(&ctx, &mut app, banner_retry.center(), Vec2::new(1200.0, 800.0));
    assert_eq!(app.status, "Il servizio di sincronizzazione non è disponibile", "the banner Retry calls request_sync");
    app.sync_view = SyncView::Error { reason: "credenziali rifiutate".to_owned(), last_sync: None };
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert_eq!(probe::retry_count(&ctx), 1, "only the status bar offers Retry for an error");

    // Below the reading breakpoint the list (and its banner) is not drawn: the status bar must
    // become the one owner of Retry instead of the state showing none.
    app.sync_view = SyncView::Offline { last_sync: Some(1_700_000_000_000) };
    let id = app.visible_messages().0.first().map(|message| message.id.clone()).expect("a seeded message");
    app.open_message(id);
    frame_at(&ctx, &mut app, Vec2::new(980.0, 600.0), Vec::new());
    assert_eq!(probe::retry_count(&ctx), 1, "the 980 reading overlay keeps exactly one Retry");
    app.status.clear();
    let status_retry = probe::retry_rects(&ctx)[0];
    click_at(&ctx, &mut app, status_retry.center(), Vec2::new(980.0, 600.0));
    assert_eq!(app.status, "Il servizio di sincronizzazione non è disponibile", "the compact status Retry calls request_sync");
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert_eq!(probe::retry_count(&ctx), 1, "the wide reading split still has exactly one Retry");
    app.reading_full = true;
    frame_at(&ctx, &mut app, Vec2::new(1200.0, 800.0), Vec::new());
    assert_eq!(probe::retry_count(&ctx), 1, "wide full-screen reading keeps exactly one Retry");
}

#[test]
fn a_sync_time_names_its_date_when_it_is_not_today() {
    let now = Utc::now();
    assert_eq!(format_sync_time(now, now).len(), 5, "today stays a clock time");
    let yesterday = now - chrono::Duration::days(1);
    let shown = format_sync_time(yesterday, now);
    assert!(shown.contains('/'), "a sync from yesterday must carry a date: {shown}");
    assert!(shown.contains(':'), "and still a time: {shown}");
}

#[test]
fn settings_lists_local_templates_with_edit_and_delete() {
    let (ctx, mut app) = ready_app();
    app.templates_loaded = true;
    app.templates = vec![MessageTemplate {
        id: "tpl_1".to_owned(),
        name: "Preventivo".to_owned(),
        subject: "Il tuo preventivo".to_owned(),
        body: "Ciao".to_owned(),
        created_at: 1,
        updated_at: 1,
    }];
    app.open_settings(&ctx, None);
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    if let Some(scroll) = probe::settings_scroll_view(&ctx) {
        frame_at(&ctx, &mut app, size, wheel(scroll.center(), 120));
    }
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "MODELLI");
    painted_visible(&runs, "Preventivo");
    painted_visible(&runs, "Modifica");
    painted_visible(&runs, "Nuovo modello");
}

#[test]
fn settings_lists_local_identities_with_edit_and_delete() {
    let (ctx, mut app) = ready_app();
    app.identities_loaded = true;
    app.identities = vec![identity("idn_1", "Ada", "ada@example.com", "Laboratorio")];
    app.open_settings(&ctx, None);
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    if let Some(scroll) = probe::settings_scroll_view(&ctx) {
        frame_at(&ctx, &mut app, size, wheel(scroll.center(), 160));
    }
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "MITTENTI");
    painted_visible(&runs, "Ada <ada@example.com>");
    painted_visible(&runs, "Firma impostata");
    painted_visible(&runs, "Modifica");
    painted_visible(&runs, "Nuovo mittente");
}

#[test]
fn eml_import_opens_the_copy_and_export_writes_a_readable_file() {
    let (mut app, daemon, store_path, _key, _vault) = daemon_app("p13d-eml-import-export", |store| {
        seed_account_rows(store, "acc_a", "ada@example.com", None);
    });
    let dir = store_path.parent().expect("store directory");
    let source_path = dir.join("import.eml");
    let source = postino_testkit::synthetic_message_with_attachment(7, "nota.txt", "text/plain", b"allegato import\n");
    std::fs::write(&source_path, &source.raw).expect("write fixture .eml");
    let picker = crate::test_support::RecordingFilePicker::new();
    picker.set_paths(vec![source_path]);
    app.file_picker = picker.clone();

    app.import_eml();

    assert_eq!(picker.calls(), 1);
    assert_eq!(app.folder, "fld_import_acc_a");
    let id = app.selected.clone().expect("the imported message opens");
    assert!(app.reading_open, "the imported copy is visible in the reading pane");
    let message = app.engine.message(&id).expect("the engine reloaded the imported row");
    assert_eq!(message.summary.subject, "Synthetic message with attachment 7");
    assert_eq!(message.summary.folder_id, "fld_import_acc_a");
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].filename, "nota.txt");

    let out = dir.join("esportato.eml");
    let saver = RecordingFileSaver::new();
    saver.set_path(Some(out.clone()));
    app.file_saver = saver.clone();
    app.export_selected_eml();

    assert_eq!(saver.choose_calls(), 1);
    let writes = saver.writes();
    assert_eq!(writes.len(), 1, "export must write exactly one file");
    assert_eq!(writes[0].0, out);
    let raw = String::from_utf8_lossy(&writes[0].1).into_owned();
    assert!(raw.starts_with("From: "), "{}", &raw[..raw.len().min(80)]);
    assert!(raw.contains("Subject: Synthetic message with attachment 7"));
    assert!(raw.contains("nota.txt"));
    assert!(app.toast.as_ref().is_some_and(|toast| toast.message.contains("ricostruito")));
    daemon.shutdown().expect("the daemon stops");
}

#[test]
fn settings_lists_pgp_keys_with_export_and_delete() {
    let (ctx, mut app) = ready_app();
    app.pgp_keys_loaded = true;
    app.pgp_keys = vec![pgp_key("key_ada")];
    app.open_settings(&ctx, None);
    let size = Vec2::new(980.0, 600.0);
    frame_at(&ctx, &mut app, size, vec![]);
    if let Some(scroll) = probe::settings_scroll_view(&ctx) {
        frame_at(&ctx, &mut app, size, wheel(scroll.center(), 80));
    }
    let runs = painted_runs(&ctx, &mut app, size);
    painted_visible(&runs, "Importa chiave PGP");
    painted_visible(&runs, "Ada Lovelace");
    painted_visible(&runs, "Copia pubblica");
    painted_visible(&runs, "Elimina");
}
