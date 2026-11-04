//! Daemon-channel events, restarts and the operations that reach the engine over IPC.

use crate::daemon;

use super::*;

#[test]
fn a_daemon_write_repaints_the_list_without_a_local_revision() {
    let (mut app, daemon, _path, _key, _vault) = daemon_app("p10a2-daemon-writes", |store| seed_account_rows(store, "acc_w", "w@example.com", None));
    assert!(app.daemon_link.is_some(), "the window must be a daemon client, not in-process");
    let (rows, _, unread) = app.visible_messages();
    assert_eq!(unread, 1, "the seeded message is unread");
    let id = rows.first().map(|message| message.id.clone()).expect("a seeded message");

    app.op("message_flag", json!({ "id": id, "unread": false })).expect("the daemon flags the message");
    let (rows, _, unread) = app.visible_messages();
    assert!(!rows[0].unread, "the row the daemon flagged must repaint");
    assert_eq!(unread, 0, "the unread counter must follow the daemon write");

    app.op("message_flag", json!({ "id": id, "starred": true })).expect("the daemon stars the message");
    let (rows, _, _) = app.visible_messages();
    assert!(rows[0].starred, "the star the daemon wrote must repaint");

    let label = app.op("label_create", json!({ "name": "Da rivedere", "color": "gold" })).expect("a label over the daemon");
    let label_id = label["label"]["id"].as_str().expect("the label id").to_owned();
    app.op("label_apply", json!({ "id": label_id, "message_ids": [id], "apply": true })).expect("the daemon applies the label");
    let (rows, _, _) = app.visible_messages();
    assert!(rows[0].labels.iter().any(|candidate| candidate == &label_id), "the label the daemon wrote must repaint");

    daemon.shutdown().expect("the daemon stops");
}

#[test]
fn a_daemon_index_event_updates_the_building_banner() {
    let mut app = PostinoApp::demo_at(true, screenshot_clock().expect("clock"));
    app.apply_daemon_event(json!({
        "kind": "index",
        "data": { "state": "building", "indexed": 7, "total": 9, "ready": false, "last_error": null, "size_bytes": 4096, "updated_at": 42 },
    }));
    let progress = app.index_progress.as_ref().expect("the daemon's index progress is kept");
    assert_eq!((progress.indexed, progress.total), (7, 9));
    assert_eq!(progress.size_bytes, 4096, "the full status shape must deserialize");
    assert!(!progress.ready);
    // A daemon that publishes only the counters still moves the banner: it must never stall.
    app.apply_daemon_event(json!({
        "kind": "index",
        "data": { "state": "building", "indexed": 9, "total": 9, "ready": false },
    }));
    assert_eq!(app.index_progress.as_ref().map(|progress| progress.indexed), Some(9));
}

#[test]
fn a_rules_run_by_another_client_repaints_the_labels() {
    let (mut app, daemon, path, _key, _vault) = daemon_app("p10a2-daemon-rules", |store| {
        seed_account_rows(store, "acc_r", "rules@example.com", None);
        // Rules only scan mail ingested after the rule existed: write the rule first, then the
        // message it labels, exactly as a sync would deliver it.
        let created_at = Utc::now().timestamp_millis() - 60_000;
        store
            .create_label(&postino_store::Label {
                id: "lbl_rules".to_owned(),
                account_id: None,
                name: "Da rivedere".to_owned(),
                color: "gold".to_owned(),
            })
            .expect("a label");
        store
            .create_rule_limited(
                &postino_store::Rule {
                    id: "rule_rules".to_owned(),
                    account_id: "acc_r".to_owned(),
                    name: "Oggetto locale".to_owned(),
                    enabled: true,
                    priority: 0,
                    stop: false,
                    query: "subject:Messaggio".to_owned(),
                    actions_json: r#"[{"kind":"label","label":"lbl_rules"}]"#.to_owned(),
                    created_at,
                    updated_at: created_at,
                },
                200,
            )
            .expect("a rule");
        store.upsert_message(&store_message("fld_acc_r_inbox", "acc_r", "msg_acc_r_2", "Messaggio di oggi")).expect("a message row");
    });
    // Another client of the same daemon: the window never sees the op, only the event.
    let mut other = daemon::Link::connect(&path).expect("a second client");
    let id = "msg_acc_r_2".to_owned();
    assert!(!has_label(&app, &id, "lbl_rules"), "the rule has not run yet");
    let report = other.call("rules_run", json!({ "scope": "all" })).expect("the manual rules pass");
    assert!(report["run"]["matched"].as_u64().unwrap_or(0) > 0, "the seeded message must match: {report}");
    assert!(
        poll_until(&mut app, |app| has_label(app, &id, "lbl_rules")),
        "the daemon must publish the rules pass, or the labels stay stale until the next GC"
    );
    let rows = app.visible_messages().0;
    let row = rows.iter().find(|message| message.id == id).expect("the labelled message stays in the list");
    assert!(row.labels.iter().any(|candidate| candidate == "lbl_rules"), "the list must repaint with the rule's label");
    drop(other);
    daemon.shutdown().expect("the daemon stops");
}

#[test]
fn a_failed_daemon_start_returns_to_a_coherent_window() {
    let mut app = booted_app("p10a2-start-failure", Vec::new(), |store| seed_account_rows(store, "acc_f", "fail@example.com", None));
    let path = app.store_path.clone().expect("a store path");
    // The start command has already released the writer lock and silenced the workers.
    app.writer_lock = None;
    app.recover_after_failed_start("Il daemon non è partito: la prima prova".to_owned());
    assert!(matches!(app.connection, daemon::Connection::InProcess), "a free archive returns to the in-process mode");
    assert!(app.writer_lock.is_some(), "the writer lock comes back");
    assert!(app.sync_worker.is_some(), "the in-process sync worker comes back");
    assert!(app.send_worker.is_some(), "the in-process send worker comes back");

    // The archive is still owned by another writer (a daemon finishing its start): the window
    // must stay a readable client labelled as lost, never "reconnecting" with no workers.
    app.writer_lock = None;
    let foreign = postino_store::WriterLock::acquire_as(&path, "other", None).expect("another writer owns the archive");
    app.recover_after_failed_start("Il daemon non è partito: la seconda prova".to_owned());
    assert!(matches!(app.connection, daemon::Connection::Lost), "an owned archive is labelled lost, not in-process");
    assert!(app.writer_lock.is_none(), "the window must not fight for the lock");
    assert_eq!(app.connection.label(), "Daemon · non raggiungibile");
    assert!(app.sync_worker.is_some(), "a readable client keeps a daemon sync worker");
    assert!(app.daemon_events.is_some(), "a readable client keeps listening for the daemon");
    drop(foreign);
}

#[test]
fn an_explicit_start_is_refused_in_demo_and_spawns_nothing() {
    let mut app = PostinoApp::demo_at(true, screenshot_clock().expect("clock"));
    app.request_start_daemon();
    assert!(app.daemon_job.is_none(), "the demo must never spawn a daemon");
    assert!(matches!(app.connection, daemon::Connection::Demo));
    assert!(app.status.to_lowercase().contains("demo"), "the refusal must say why: {}", app.status);
}

#[test]
fn a_real_daemon_restart_keeps_the_view_and_the_explicit_stop_takes_over() {
    let (mut app, daemon, path, key, vault) = daemon_app("p10a2-lost-channel", |store| seed_account_rows(store, "acc_l", "lost@example.com", None));
    assert!(matches!(app.connection, daemon::Connection::Connected(_)), "the window must really be a daemon client");
    let account = app.active_account.clone().expect("the seeded account is active");
    let folder = app.folder.clone();
    app.unread_only = true;
    app.label_filter = None;
    let open = app.visible_messages().0.first().map(|message| message.id.clone());
    assert!(open.is_some(), "the seeded message is on screen");
    app.selected = open.clone();
    app.reading_open = true;
    app.threads_view = true;
    app.selected_thread = Some("thr_msg_acc_l_1".to_owned());
    app.thread_open = Some("thr_msg_acc_l_1".to_owned());
    app.composer = ComposerState::new(ComposeMode::New);
    app.composer.to = "Giulia <giulia.ferri@example.com>".to_owned();

    daemon.shutdown().expect("the daemon stops");
    assert!(
        poll_until(&mut app, |app| matches!(app.connection, daemon::Connection::Lost)),
        "four failed reconnects are the lost state, not a spinner that never settles"
    );
    assert_eq!(app.connection.label(), "Daemon · non raggiungibile");
    assert!(!app.visible_messages().0.is_empty(), "the read-only archive stays readable while the daemon is away");

    let restarted = spawn_daemon(&path, &key, &vault);
    assert!(poll_until(&mut app, |app| app.connection.is_live()), "the event worker must reconnect to the restarted daemon");
    assert_eq!(app.active_account.as_deref(), Some(account.as_str()));
    assert_eq!(app.folder, folder, "the selected folder survives the restart");
    assert!(app.unread_only, "the filters survive the restart");
    assert_eq!(app.selected, open, "the open message survives the restart");
    assert!(app.reading_open);
    assert!(app.threads_view, "the thread view survives the restart");
    assert_eq!(app.selected_thread.as_deref(), Some("thr_msg_acc_l_1"));
    assert_eq!(app.thread_open.as_deref(), Some("thr_msg_acc_l_1"));
    assert_eq!(app.composer.to, "Giulia <giulia.ferri@example.com>", "the draft is the window's, not the daemon's");

    app.request_stop_daemon();
    assert!(app.daemon_job.is_some(), "the stop is explicit command, not automatic");
    assert!(poll_until(&mut app, |app| matches!(app.connection, daemon::Connection::InProcess)), "the stop hands the archive back");
    assert!(app.writer_lock.is_some(), "the window owns the archive again after the stop");
    drop(restarted);
    assert_eq!(app.folder, folder, "the folder survives the explicit stop");
    assert!(app.unread_only);
    assert_eq!(app.composer.to, "Giulia <giulia.ferri@example.com>");
}

#[test]
fn a_hidden_window_reopens_when_the_daemon_disappears() {
    let (mut app, daemon, _path, _key, _vault) = daemon_app("p10b2-orphan", |store| seed_account_rows(store, "acc_o", "orphan@example.com", None));
    assert!(matches!(app.connection, daemon::Connection::Connected(_)), "the window is a daemon client");
    // The person closed to the tray: the icon is the only way back.
    app.hidden = true;
    daemon.shutdown().expect("the daemon stops");
    assert!(
        poll_until(&mut app, |app| matches!(app.connection, daemon::Connection::Lost)),
        "the lost channel must settle instead of spinning forever"
    );
    assert!(!app.hidden, "a hidden window with no daemon and no tray must never stay unreachable");
    assert!(app.status.contains("primo piano"), "the offline state must say the window came back: {}", app.status);
    assert!(!app.visible_messages().0.is_empty(), "the mailbox stays readable in the restored window");
}

#[test]
fn an_account_add_over_the_local_channel_reaches_the_engine_without_leaking_the_password() {
    let (mut app, daemon, _path, _key, _vault) = daemon_app("p10a2-account-add", |store| seed_account_rows(store, "acc_a", "add@example.com", None));
    let secret = "correct-horse-battery-staple";
    let result = app.op(
        "account_add",
        json!({
            "provider": "custom",
            "email": "nuovo@example.com",
            "name": "Nuovo",
            "password": secret,
            "host": "127.0.0.1",
            "port": 1,
            "tls": "implicit",
        }),
    );
    assert!(result.is_err(), "an unreachable server must fail the add");
    let sentence = result.err().map(|error| error.to_string()).unwrap_or_default();
    assert!(!sentence.is_empty(), "the failure must be readable");
    assert!(!sentence.contains(secret), "the password must never appear in a sentence");
    assert!(!app.status.contains(secret), "the status line must never carry the password");
    let accounts = app.op("accounts_list", json!({})).expect("a read after the refused add");
    assert_eq!(accounts["accounts"].as_array().map(Vec::len), Some(1), "the refused add must not create an account");
    daemon.shutdown().expect("the daemon stops");
}

#[test]
fn pgp_key_import_and_public_export_reach_the_daemon() {
    let (mut app, daemon, _path, _key, _vault) = daemon_app("p12c2c-pgp-export", |store| seed_account_rows(store, "acc_a", "save@example.com", None));
    let armor = include_str!("../../../../../testdata/pgp/public.asc");
    let imported =
        app.op("pgp_key_import", json!({ "account": "acc_a", "armored_key": armor, "passphrase": "" })).expect("import the test public key");
    let key_id = imported["id"].as_str().expect("the imported key id").to_owned();
    assert_eq!(imported["has_secret"], json!(false), "a public key must not claim secret material");

    app.pgp_keys_loaded = false;
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    app.open_settings(&ctx, None);
    let size = Vec2::new(1200.0, 900.0);
    frame_at(&ctx, &mut app, size, vec![]);
    if let Some(scroll) = probe::settings_scroll_view(&ctx) {
        frame_at(&ctx, &mut app, size, wheel(scroll.center(), 80));
    }
    frame_at(&ctx, &mut app, size, vec![]);
    let rect = probe::settings_pgp_export_rect(&ctx, key_id.as_str()).expect("the public export action");
    let output = click_at_output(&ctx, &mut app, rect.center(), size);
    let copied = copied_text(&output).unwrap_or_default();
    assert!(copied.contains("BEGIN PGP PUBLIC KEY BLOCK"), "the export must put the public armor on the clipboard");
    assert!(app.pgp_notice.as_deref().is_some_and(|notice| notice.contains("copiata")), "the export must report its result");

    app.pgp_checked = Some("msg_open".to_owned());
    app.reading.pgp_reading =
        Some(PgpReadingState { message_id: "msg_open".to_owned(), kind: PgpMimeKind::Signed, verify: None, decrypt: None, view: None });
    app.delete_pgp_key(key_id.as_str());
    assert!(app.pgp_keys.is_empty(), "the deleted key leaves the list");
    assert!(app.pgp_checked.is_none(), "the deleted key must invalidate the open message verdict");
    assert!(app.reading.pgp_reading.is_none(), "the stale badge must not survive a key deletion");
    daemon.shutdown().expect("the daemon stops");
}
