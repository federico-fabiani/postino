//! The app shell's headless tests, split by theme. This file owns the shared fixtures: the
//! painted-run recorder, the frame and click drivers, and the store and daemon seeders a themed
//! file builds on.

use std::path::PathBuf;
use std::sync::Arc;

use postino_core::CredentialVault;
use postino_store::{
    Account as StoreAccount, AccountServer, AccountTls, Address as StoreAddress, Attachment as StoreAttachment, Folder as StoreFolder,
    FolderKind as StoreFolderKind,
};
use postino_store::{Message as StoreMessage, MessageSummary as StoreMessageSummary};

use crate::boot::{BootStage, KeySource};
use crate::test_support::{
    MemoryVault, RecordingFileSaver, RecordingFolderOpener, RecordingLinkOpener, RunningServer, RunningSmtp, directory, three_folders,
};

use super::*;

mod accounts;
mod chrome;
mod compose;
mod conversations;
mod daemon;
mod labels;
mod layout;
mod outbox;
mod reading;
mod rules;
mod search;
mod shell;
mod update;

/// One text run actually painted by a frame: the galley text, its rectangle and the clip that
/// applied to it. `clip` is what makes a layout assertion clip-aware: a section painted outside
/// its scroll viewport still appears in `output.shapes`, but its clip no longer contains it.
#[derive(Debug)]
struct PaintedRun {
    text: String,
    rect: Rect,
    clip: Rect,
}

fn painted_runs(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) -> Vec<PaintedRun> {
    fn collect(shape: &egui::epaint::Shape, clip: Rect, runs: &mut Vec<PaintedRun>) {
        match shape {
            egui::epaint::Shape::Text(shape) => {
                let rect = Rect::from_min_size(shape.pos, shape.galley.size());
                runs.push(PaintedRun { text: shape.galley.text().to_owned(), rect, clip });
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
    // A modal animates in; tests need its settled layout, not a translated frame mid-animation.
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    // A modal is laid out on its own layer and enters with a short offset; the first frames
    // create the area and settle it, then the content paints. Real windows run them, so tests
    // read the settled frame.
    for _ in 0..4 {
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        runs.clear();
        for clipped in &output.shapes {
            collect(&clipped.shape, clipped.clip_rect, &mut runs);
        }
        output.textures_delta.clear();
    }
    runs
}

/// The text actually painted by one frame, for assertions that a settings section or a warning
/// reached the screen rather than only the state.
fn painted_text(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) -> Vec<String> {
    painted_runs(ctx, app, size).into_iter().map(|run| run.text).collect()
}

/// The first painted run containing `needle`, asserting its scroll clip does not cut it. A run
/// outside its clip is still in the shape list but never reaches the eye.
fn painted_visible(runs: &[PaintedRun], needle: &str) -> String {
    let run = runs
        .iter()
        .find(|run| run.text.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not painted: {:?}", runs.iter().map(|run| run.text.as_str()).collect::<Vec<_>>()));
    assert!(run.clip.contains_rect(run.rect), "{needle:?} is painted outside its clip: {run:?}");
    run.text.clone()
}

fn frame_at(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, events: Vec<egui::Event>) {
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), events, ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    output.textures_delta.clear();
}

/// One frame that returns its viewport commands, for activation and quit assertions.
fn frame_output(ctx: &egui::Context, app: &mut PostinoApp, events: Vec<egui::Event>) -> egui::FullOutput {
    let raw =
        egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))), events, ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    output.textures_delta.clear();
    output
}

fn frame(ctx: &egui::Context, app: &mut PostinoApp, events: Vec<egui::Event>) {
    frame_at(ctx, app, Vec2::new(1200.0, 800.0), events);
}

/// One frame where the window manager asks to close, the way the real close button does.
fn close_request(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) -> egui::FullOutput {
    let mut input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
    let mut info = egui::ViewportInfo::default();
    info.events.push(egui::ViewportEvent::Close);
    input.viewports.insert(egui::ViewportId::ROOT, info);
    let mut output = ctx.run_ui(input, |ui| app.show(ui));
    output.textures_delta.clear();
    output
}

fn commands(output: &egui::FullOutput) -> &[egui::ViewportCommand] {
    output.viewport_output.get(&egui::ViewportId::ROOT).map_or(&[], |output| output.commands.as_slice())
}

fn key(key: Key, modifiers: Modifiers) -> egui::Event {
    egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

/// A dropped file for a headless frame. The bytes are never read by the composer; the screen
/// opens the path itself through the same bounded reader as the picker.
#[derive(Debug)]
struct TestDroppedFile(PathBuf);

impl egui::DroppedFile for TestDroppedFile {
    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|error| error.to_string())
    }
}

fn frame_with_drop(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, pos: egui::Pos2, paths: Vec<PathBuf>) {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events: vec![egui::Event::PointerMoved(pos)],
        dropped_files: paths.into_iter().map(|path| Arc::new(TestDroppedFile(path)) as egui::DroppedFileHandle).collect(),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    output.textures_delta.clear();
}

fn ready_app() -> (egui::Context, PostinoApp) {
    let ctx = egui::Context::default();
    theme::install_fonts(&ctx);
    theme::apply(&ctx, false);
    let app = PostinoApp::demo(true);
    (ctx, app)
}

fn pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
}

/// A real press/release pair on the widget whose rect the last frame recorded. The two events
/// arrive in consecutive frames, like a physical mouse, so egui can resolve the click target
/// from a settled layout.
fn click_at(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) {
    frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos), pointer(pos, true)]);
    frame_at(ctx, app, size, vec![pointer(pos, false)]);
}

fn click(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2) {
    click_at(ctx, app, pos, Vec2::new(1200.0, 800.0));
}

/// A click on a modal that has just appeared: move, press and release arrive on separate frames,
/// so egui resolves the hover before the button sees the press.
fn click_settled(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) {
    frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos)]);
    frame_at(ctx, app, size, vec![pointer(pos, true)]);
    frame_at(ctx, app, size, vec![pointer(pos, false)]);
    frame_at(ctx, app, size, vec![]);
}

/// A click whose release-frame platform output is returned, for clipboard assertions.
fn click_at_output(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) -> egui::PlatformOutput {
    frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos), pointer(pos, true)]);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events: vec![pointer(pos, false)],
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    output.textures_delta.clear();
    output.platform_output
}

fn copied_text(output: &egui::PlatformOutput) -> Option<String> {
    output.commands.iter().find_map(|command| match command {
        egui::OutputCommand::CopyText(text) => Some(text.clone()),
        _ => None,
    })
}

/// Let a modal animate in and register its probes without injecting input.
fn settle_at(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2) {
    for _ in 0..4 {
        frame_at(ctx, app, size, Vec::new());
    }
}

/// Click a dialog button by the probe the last settled frame recorded.
fn click_dialog_button(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, title: &str, primary: bool) {
    settle_at(ctx, app, size);
    let rect = probe::dialog_button_rect(ctx, title, primary).unwrap_or_else(|| panic!("the {title:?} button was not probed"));
    click_at(ctx, app, rect.center(), size);
}

/// Two press/release pairs inside the double-click window, like a physical mouse. The second
/// press resolves against the layout of the frame after the first click, as it does for a user.
fn double_click_at(ctx: &egui::Context, app: &mut PostinoApp, pos: egui::Pos2, size: Vec2) {
    frame_at(ctx, app, size, vec![egui::Event::PointerMoved(pos), pointer(pos, true)]);
    frame_at(ctx, app, size, vec![pointer(pos, false)]);
    // Let a pane opened by the first click settle before the second press resolves its hit.
    frame_at(ctx, app, size, vec![]);
    frame_at(ctx, app, size, vec![pointer(pos, true)]);
    frame_at(ctx, app, size, vec![pointer(pos, false)]);
}

/// Small wheel steps are applied without easing, so one frame scrolls deterministically.
fn wheel(pos: egui::Pos2, steps: usize) -> Vec<egui::Event> {
    wheel_by(pos, steps, -6.0)
}

fn wheel_by(pos: egui::Pos2, steps: usize, step: f32) -> Vec<egui::Event> {
    let mut events = vec![egui::Event::PointerMoved(pos)];
    events.extend((0..steps).map(|_| egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: Vec2::new(0.0, step),
        phase: egui::TouchPhase::Move,
        modifiers: Modifiers::NONE,
    }));
    events
}

/// A horizontal wheel, the way a shift-wheel or a trackpad reports it.
fn wheel_horizontal(pos: egui::Pos2, steps: usize) -> Vec<egui::Event> {
    let mut events = vec![egui::Event::PointerMoved(pos)];
    events.extend((0..steps).map(|_| egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: Vec2::new(0.0, -60.0),
        phase: egui::TouchPhase::Move,
        modifiers: Modifiers::SHIFT,
    }));
    events
}

fn picker_app(name: &str) -> PostinoApp {
    booted_app(name, Vec::new(), |store| seed_account_rows(store, "acc_a", "a@example.com", None))
}

fn frame_time(ctx: &egui::Context, app: &mut PostinoApp, size: Vec2, events: Vec<egui::Event>, time: f64) {
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), time: Some(time), events, ..Default::default() };
    let mut output = ctx.run_ui(raw, |ui| app.show(ui));
    output.textures_delta.clear();
}

fn conversations_app() -> (egui::Context, PostinoApp) {
    let (ctx, mut app) = ready_app();
    app.threads_view = true;
    app.threads_cache = None;
    (ctx, app)
}

fn test_directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join("opencode").join("p6c-fix2-postino").join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&path);
    let _ = std::fs::remove_file(path.join("mail.db"));
    let _ = std::fs::remove_dir_all(path.join("mail.db.blobs"));
    path
}

fn fresh_store(name: &str) -> (PathBuf, Arc<postino_store::StoreKey>, Arc<MemoryVault>) {
    let directory = test_directory(name);
    let path = directory.join("mail.db");
    (path, Arc::new(postino_store::StoreKey::from_bytes([0x5a; 32])), MemoryVault::new())
}

fn store_message(folder: &str, account_id: &str, id: &str, subject: &str) -> StoreMessage {
    StoreMessage {
        summary: StoreMessageSummary {
            id: id.to_owned(),
            account_id: account_id.to_owned(),
            folder_id: folder.to_owned(),
            thread_id: format!("thr_{id}"),
            from: StoreAddress::new("Mittente", "mittente@example.com"),
            to: vec![StoreAddress::new("Postino Test", "test@example.com")],
            subject: subject.to_owned(),
            snippet: format!("{subject} — anteprima"),
            date: 1_700_000_000_000,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: None,
        message_id: None,
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: format!("{subject}\n\nCorpo salvato nell'archivio locale."),
        body_html: None,
        remote_images: 0,
        size: None,
    }
}

fn seed_attachment(store: &postino_store::Store, message_id: &str, id: &str, filename: &str) {
    let mime = if filename.to_ascii_lowercase().ends_with(".txt") {
        "text/plain"
    } else if filename.to_ascii_lowercase().ends_with(".png") {
        "image/png"
    } else {
        "application/pdf"
    };
    store
        .replace_attachments(
            message_id,
            &[StoreAttachment { id: id.to_owned(), filename: filename.to_owned(), mime: mime.to_owned(), size: 11, blob_hash: None }],
        )
        .expect("attachment row");
}

/// Seed one account, its inbox folder and one local message. `port` adds the IMAP server row.
fn seed_account_rows(store: &postino_store::Store, id: &str, email: &str, port: Option<u16>) {
    store
        .upsert_account(&StoreAccount {
            id: id.to_owned(),
            name: format!("Account {id}"),
            email: email.to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
            created_at: 1_700_000_000_000,
        })
        .expect("account row");
    if let Some(port) = port {
        store
            .upsert_account_server(&AccountServer {
                account_id: id.to_owned(),
                host: "localhost".to_owned(),
                port,
                tls: AccountTls::Implicit,
                username: email.to_owned(),
                created_at: 1_700_000_000_000,
                smtp: None,
            })
            .expect("server row");
    }
    let folder = format!("fld_{id}_inbox");
    for (suffix, name, kind) in [
        ("inbox", "Posta in arrivo", StoreFolderKind::Inbox),
        ("sent", "Inviati", StoreFolderKind::Sent),
        ("drafts", "Bozze", StoreFolderKind::Drafts),
        ("trash", "Cestino", StoreFolderKind::Trash),
        ("archive", "Archivio", StoreFolderKind::Archive),
        ("starred", "Preferiti", StoreFolderKind::Starred),
    ] {
        store
            .upsert_folder(&StoreFolder {
                id: format!("fld_{id}_{suffix}"),
                account_id: id.to_owned(),
                name: name.to_owned(),
                kind,
                unread: 0,
                total: 0,
                uid_validity: None,
                uid_next: None,
                last_sync_at: None,
            })
            .expect("folder row");
    }
    store.upsert_message(&store_message(&folder, id, &format!("msg_{id}_1"), &format!("Messaggio di {id}"))).expect("message row");
}

/// A booted store-backed window with a received REQUEST and an outgoing Sent copy, both carrying
/// the same real `text/calendar` blob. The demo has no calendar part, so the full reply path is
/// exercised here and not through the screenshot fixture.
fn invite_store_app(name: &str) -> PostinoApp {
    const CALENDAR: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//Inviti//IT\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:invito-app@example.com\r\nDTSTAMP:20261024T084200Z\r\nDTSTART:20261105T090000Z\r\nDTEND:20261105T100000Z\r\nSUMMARY:Revisione preventivo\r\nLOCATION:Studio Postino\r\nORGANIZER:mailto:giulia.ferri@example.com\r\nATTENDEE:mailto:federico@example.it\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let mut app = booted_app(name, Vec::new(), |store| {
        seed_account_rows(store, "acc_invite", "federico@example.it", None);
        for (id, folder, subject) in
            [("msg_invite", "fld_acc_invite_inbox", "Revisione preventivo"), ("msg_sent", "fld_acc_invite_sent", "Revisione inviata")]
        {
            let mut message = store_message(folder, "acc_invite", id, subject);
            message.message_id = Some(format!("{id}@example.com"));
            message.summary.unread = false;
            message.summary.has_attachments = true;
            store.upsert_message(&message).expect("invite message");
            let attachment_id = format!("att_{id}");
            let size = u64::try_from(CALENDAR.len()).expect("calendar size");
            store
                .replace_attachments(
                    id,
                    &[postino_store::Attachment {
                        id: attachment_id.clone(),
                        filename: "invito.ics".to_owned(),
                        mime: "text/calendar".to_owned(),
                        size,
                        blob_hash: None,
                    }],
                )
                .expect("attachment row");
            let hash = store.blobs().put(CALENDAR.as_bytes()).expect("calendar blob");
            store.set_attachment_blob_hash(id, &attachment_id, Some(&hash), size).expect("calendar hash");
        }
    });
    app.open_message("msg_invite".to_owned());
    app
}

fn booted_app(name: &str, roots: Vec<CertificateDer<'static>>, seed: impl FnOnce(&postino_store::Store)) -> PostinoApp {
    let (path, key, vault) = fresh_store(name);
    {
        let store = postino_store::Store::open(&path, key.as_ref()).expect("seed store");
        seed(&store);
    }
    let vault_dyn: Arc<dyn CredentialVault> = vault;
    let booted = boot::boot(Launch::Store(Some(path.clone())), KeySource::Fixed { key: key.clone(), vault: vault_dyn }, Utc::now()).expect("boot");
    let mut app = PostinoApp::from_booted(booted, true, Utc::now(), Launch::Store(Some(path)), roots);
    // Legacy store tests exercise the flat table; P9b tests opt in to the threaded default.
    app.threads_view = false;
    app
}

/// A window that is really a daemon client: the store is seeded, a live daemon owns it, and the
/// app boots through the same probe a person's launch uses.
fn daemon_app(
    name: &str,
    seed: impl FnOnce(&postino_store::Store),
) -> (PostinoApp, postino_daemon::Daemon, PathBuf, Arc<postino_store::StoreKey>, Arc<MemoryVault>) {
    let (path, key, vault) = fresh_store(name);
    {
        let store = postino_store::Store::open(&path, key.as_ref()).expect("seed store");
        seed(&store);
    }
    let daemon = spawn_daemon(&path, &key, &vault);
    let vault_dyn: Arc<dyn CredentialVault> = Arc::clone(&vault) as Arc<dyn CredentialVault>;
    let booted = boot::boot(Launch::Store(Some(path.clone())), KeySource::Fixed { key: Arc::clone(&key), vault: vault_dyn }, Utc::now())
        .expect("a daemon-owned store boots");
    let mut app = PostinoApp::from_booted(booted, false, Utc::now(), Launch::Store(Some(path.clone())), Vec::new());
    app.threads_view = false;
    (app, daemon, path, key, vault)
}

fn spawn_daemon(path: &std::path::Path, key: &Arc<postino_store::StoreKey>, vault: &Arc<MemoryVault>) -> postino_daemon::Daemon {
    let vault_dyn: Arc<dyn CredentialVault> = Arc::clone(vault) as Arc<dyn CredentialVault>;
    let mut config = postino_daemon::DaemonConfig::new(path.to_path_buf(), Arc::clone(key), vault_dyn);
    config.sync_on_start = false;
    config.sync_jitter = false;
    config.outbox_on_start = false;
    postino_daemon::Daemon::spawn(config).expect("the daemon starts")
}

/// Poll the window's daemon channel until `done` holds, with a bounded wall clock. The frame's
/// context is not needed for the state assertions these tests make; viewport commands are
/// asserted from a real `frame_output` instead.
fn poll_until(app: &mut PostinoApp, mut done: impl FnMut(&PostinoApp) -> bool) -> bool {
    let ctx = egui::Context::default();
    for _ in 0..400 {
        app.poll_daemon(&ctx);
        app.poll_daemon_job();
        if done(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_sync(app: &mut PostinoApp) {
    for _ in 0..1500 {
        app.handle_sync_events();
        if !app.sync_running() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the sync did not finish");
}

fn set_test_password(app: &PostinoApp, account: &str) {
    let store = app.store.as_ref().expect("a store-backed app");
    store.vault().store_imap_password(account, postino_testkit::PASSWORD.as_bytes()).expect("the test vault stores the password");
}

fn seeded_attachment_app(name: &str, filename: &str, bytes: Option<&[u8]>) -> (PostinoApp, String, Attachment) {
    let filename = filename.to_owned();
    let mut app = booted_app(name, Vec::new(), move |store| {
        seed_account_rows(store, "acc_save", "save@example.com", None);
        seed_attachment(store, "msg_acc_save_1", "att_save", &filename);
    });
    let message_id = "msg_acc_save_1".to_owned();
    let attachment = app.engine.message(&message_id).and_then(|message| message.attachments.first().cloned()).expect("the seeded attachment");
    if let Some(bytes) = bytes {
        app.engine.store_attachment_bytes(&message_id, &attachment.id, bytes).expect("store the test blob");
    }
    (app, message_id, attachment)
}

/// A store-backed app with an SMTP endpoint and the two folders a send needs. The IMAP port is
/// closed, so the Sent APPEND fails and the row stays `append_pending`: that is exactly the state
/// the UI must still reflect (the message was submitted, its server copy is pending).
fn smtp_app(name: &str, smtp_port: u16, ca: CertificateDer<'static>) -> PostinoApp {
    let (path, key, vault) = fresh_store(name);
    {
        let store = postino_store::Store::open(&path, key.as_ref()).expect("seed store");
        crate::test_support::seed_smtp_account(&store, &vault, "acc_smtp", 1, smtp_port);
        for (suffix, display, kind) in [("drafts", "Bozze", postino_store::FolderKind::Drafts), ("sent", "Inviati", postino_store::FolderKind::Sent)]
        {
            store
                .upsert_folder(&postino_store::Folder {
                    id: format!("fld_acc_smtp_{suffix}"),
                    account_id: "acc_smtp".to_owned(),
                    name: display.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                })
                .expect("folder");
        }
    }
    let vault_dyn: Arc<dyn CredentialVault> = vault;
    let booted = boot::boot(Launch::Store(Some(path.clone())), KeySource::Fixed { key: key.clone(), vault: vault_dyn }, Utc::now()).expect("boot");
    PostinoApp::from_booted(booted, true, Utc::now(), Launch::Store(Some(path)), vec![ca])
}

fn app_draft(app: &mut PostinoApp, body: &str) -> String {
    let content = postino_core::DraftContent {
        to: vec![postino_core::Address::new("", "one@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Prova".to_owned(),
        body_text: body.to_owned(),
        attachments: Vec::new(),
        in_reply_to: None,
    };
    app.engine.save_draft(None, None, content).expect("draft").summary.id
}

/// Advance the app's clock to `now_ms`, ask the worker for a pass and wait for its event.
fn run_send_at(app: &mut PostinoApp, now_ms: i64) {
    app.now_utc = chrono::DateTime::<Utc>::from_timestamp_millis(now_ms).expect("instant");
    app.request_send();
    for _ in 0..1500 {
        app.handle_send_events();
        if !app.send_worker.as_ref().is_some_and(SendWorker::is_running) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the send worker did not finish");
}

fn label_unread(app: &PostinoApp, id: &str) -> Option<u32> {
    app.labels_cache.iter().find(|label| label.id == id).map(|label| label.unread_count)
}

fn has_label(app: &PostinoApp, message_id: &str, label_id: &str) -> bool {
    app.engine.message(message_id).is_some_and(|message| message.summary.labels.iter().any(|candidate| candidate == label_id))
}

fn identity(id: &str, name: &str, email: &str, signature: &str) -> Identity {
    Identity {
        id: id.to_owned(),
        account_id: "acc_demo".to_owned(),
        name: name.to_owned(),
        email: email.to_owned(),
        signature: signature.to_owned(),
        created_at: 1,
        updated_at: 1,
    }
}

fn pgp_key(id: &str) -> PgpKey {
    PgpKey {
        id: id.to_owned(),
        account_id: "acc_a".to_owned(),
        fingerprint: "D529DDA09F683768022096EBA6079AAB69465D6D".to_owned(),
        key_id: "A6079AAB69465D6D".to_owned(),
        user_id: Some("Ada Lovelace <ada@example.com>".to_owned()),
        protected: true,
        has_secret: true,
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn folder_names_read_in_italian_without_losing_custom_paths() {
    assert_eq!(folder_display_name("INBOX", FolderKind::Inbox), "Posta in arrivo");
    assert_eq!(folder_display_name("INBOX.Drafts", FolderKind::Custom), "Bozze");
    assert_eq!(folder_display_name("INBOX.Sent Items", FolderKind::Custom), "Inviate");
    assert_eq!(folder_display_name("[Gmail]/Sent Mail", FolderKind::Custom), "Inviate");
    assert_eq!(folder_display_name("[Gmail]/All Mail", FolderKind::Custom), "Tutti i messaggi");
    assert_eq!(folder_display_name("Progetti.Cliente", FolderKind::Custom), "Progetti / Cliente");
    assert_eq!(folder_display_name("Viaggi", FolderKind::Custom), "Viaggi");
}
