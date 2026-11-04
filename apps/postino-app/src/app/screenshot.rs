//! Deterministic screenshot mode: the seeded scenes a docs capture renders, the frame-driven
//! capture loop and the atomic PNG writer. Nothing here runs unless `--screenshot` built the app.

use super::*;
use reading_pane::PendingLink;

impl PostinoApp {
    fn seed_background_rows(&mut self) {
        let daemon = background::DaemonView {
            tray: true,
            notify_enabled: true,
            notify_previews: false,
            debounce_secs: 60,
            tray_available: true,
            tray_running: true,
        };
        let autostart = background::AutostartView {
            enabled: true,
            command: Some(r"C:\Program Files\Postino\postino-daemon.exe --store C:\Users\me\AppData\Roaming\Postino\mail.db".to_owned()),
        };
        self.background_rows = Some(background::Rows::read(Ok(daemon), Ok(autostart)));
    }
    /// Apply the state named on the command line. Every scene is deterministic: no timer, no
    /// pointer and no focus side effect may change it between the first frame and the capture.
    /// The settings capture shows a live-looking but deterministic background state. It never reads
    /// the registry or talks to a daemon: a docs image must not depend on the machine that builds it.
    pub(super) fn seed_screenshot(&mut self, state: ScreenshotState) {
        self.screenshot_message = None;
        self.screenshot_invite = None;
        self.reading.invite_cache = None;
        self.invite_ready = false;
        match state {
            ScreenshotState::Shell => {}
            ScreenshotState::Reading => {
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id);
                    // Show the steady state after the 600 ms read delay: message read, no undo
                    // toast racing the capture.
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                }
            }
            ScreenshotState::ReadingFull => {
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id);
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    self.reading_full = true;
                }
            }
            ScreenshotState::EmptyUnread => {
                let ids = self
                    .engine
                    .messages()
                    .iter()
                    .filter(|message| message.folder_id == "inbox" && message.unread)
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    let _ = self.op("message_flag", json!({ "id": id, "unread": false, "starred": null }));
                }
                self.refresh_labels();
                self.unread_only = true;
                self.clear_selection();
            }
            ScreenshotState::EmptyAttachments => {
                // Inbox mail with an attachment is moved out, so the attachments filter is the only
                // reason the list is empty. The copy names the filter, not an empty folder.
                let ids = self
                    .engine
                    .messages()
                    .iter()
                    .filter(|message| message.folder_id == "inbox" && message.has_attachments)
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    let _ = self.op("message_move", json!({ "id": id, "folder": "archive" }));
                }
                self.refresh_labels();
                self.attachments_only = true;
                self.clear_selection();
            }
            ScreenshotState::EmptyLabelUnread => {
                self.select_label("lbl_1".to_owned());
                let ids = self
                    .engine
                    .messages()
                    .iter()
                    .filter(|message| message.unread && message.labels.iter().any(|id| id == "lbl_1"))
                    .map(|message| message.id.clone())
                    .collect::<Vec<_>>();
                for id in ids {
                    let _ = self.op("message_flag", json!({ "id": id, "unread": false, "starred": null }));
                }
                self.refresh_labels();
                self.unread_only = true;
            }
            ScreenshotState::Compose => {
                let mut composer = ComposerState::new(ComposeMode::New);
                composer.to = "Giulia Ferri <giulia.ferri@example.com>".to_owned();
                composer.subject = "Preventivo sito web — bozza finale".to_owned();
                composer.body = "Ciao Giulia,\n\nti confermo il preventivo per il sito: 2.400 € + IVA.".to_owned();
                composer.attachments = vec![
                    Attachment {
                        id: "att_screenshot".to_owned(),
                        filename: "preventivo-postino.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        size: 248_320,
                    },
                    Attachment {
                        id: "att_screenshot_2".to_owned(),
                        filename: "planimetria-ufficio.png".to_owned(),
                        mime: "image/png".to_owned(),
                        size: 1_048_576,
                    },
                ];
                composer.saved_at = Some(format_time(self.now_utc));
                self.start_composer(composer);
            }
            ScreenshotState::Palette => {
                self.show_palette = true;
                self.palette_focus = true;
                // A filtered palette is more useful than an empty query in a docs shot: it shows
                // the search, a handful of rows and the disabled reason line together.
                self.palette_query = "etichetta".to_owned();
                self.palette_selected = 0;
            }
            ScreenshotState::Labels => {
                // Keep the list visible at every size: the 980×600 shot must be able to prove the
                // header and "Mostra tutte" are not clipped. The reading-pane chips stay in the
                // label_menu scene.
                self.label_filter = self.labels_cache.iter().find(|label| label.name == "Lavoro").map(|label| label.id.clone());
                self.label_folder = None;
            }
            ScreenshotState::LabelEditor => {
                self.start_label_create();
                if let Some(editor) = self.label_editor.as_mut() {
                    editor.text = "Ufficio".to_owned();
                    editor.focus = true;
                }
            }
            ScreenshotState::LabelMenu => {
                self.label_filter = self.labels_cache.iter().find(|label| label.name == "Lavoro").map(|label| label.id.clone());
                self.label_folder = None;
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id.clone());
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    // The captured targets are the visible message; the Lavoro chip stays visible in
                    // the reading pane behind the modal scrim.
                    self.open_label_menu(vec![id]);
                }
            }
            ScreenshotState::OnboardingWelcome => self.seed_onboarding_screenshot(Stage::Welcome, ProviderChoice::Aruba, None, None, None),
            ScreenshotState::OnboardingProvider => self.seed_onboarding_screenshot(Stage::Provider, ProviderChoice::Gmail, None, None, None),
            ScreenshotState::OnboardingForm => self.seed_onboarding_screenshot(Stage::Credentials, ProviderChoice::Aruba, None, None, None),
            ScreenshotState::OnboardingError => self.seed_onboarding_screenshot(
                Stage::Credentials,
                ProviderChoice::Aruba,
                None,
                Some(ConnectionError {
                    kind: ErrorKind::Password,
                    message: "La password non è stata accettata da imaps.aruba.it. Controlla di averla digitata bene; se hai attivo il secondo fattore, serve una password per le app.".to_owned(),
                    focus: FocusField::Password,
                }),
                None,
            ),
            ScreenshotState::OnboardingFolders => self.seed_onboarding_screenshot(Stage::Folders, ProviderChoice::Aruba, Some(demo_folders_preview()), None, None),
            ScreenshotState::OnboardingSync => self.seed_onboarding_screenshot(
                Stage::Sync,
                ProviderChoice::Aruba,
                Some(demo_folders_preview()),
                None,
                Some(SyncProgress { folders_done: 3, folders_total: 7, messages: 128 }),
            ),
            ScreenshotState::Mailbox => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
            }
            ScreenshotState::Offline => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 87 * 60 * 1000;
                self.sync_view = SyncView::Offline { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id);
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                }
            }
            ScreenshotState::ReadingAttachments | ScreenshotState::LinkConfirm => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id.clone());
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    if let Some(mut message) = self.engine.message(&id) {
                        message.body_text = "Ciao Federico,\n\nho caricato il PDF del preventivo nella cartella condivisa.\n\nDettaglio: https://example.com/preventivo/postino-2026?token=anteprima\n\nScrivimi a mailto:giulia.ferri@example.com.\n\nGrazie,\nGiulia"
                            .to_owned();
                        // Stored truth, not a live tracker: the sanitizer removed the remote image and
                        // sync stored the count. This HTML is already the sanitized form the block
                        // renderer consumes: a heading, emphasis, a list, two safe anchors and a local
                        // `cid:` image shown as its alt text, with no remote `src` and no script.
                        message.body_html = Some(
                            concat!(
                                "<h1>Preventivo postino 2026</h1>",
                                "<p>Ciao Federico, ho caricato il <strong>PDF del preventivo</strong> nella cartella condivisa.</p>",
                                "<ul><li>Preventivo firmato</li><li>Dettaglio dei costi</li></ul>",
                                "<p><a href=\"https://example.com/preventivo/postino-2026?token=anteprima\">Apri il preventivo</a> oppure scrivimi a <a href=\"mailto:giulia.ferri@example.com\">giulia.ferri@example.com</a>.</p>",
                                "<p><img src=\"cid:logo@postino\" alt=\"Logo Postino\"></p>",
                            )
                            .to_owned(),
                        );
                        message.remote_images = 1;
                        if message.attachments.is_empty() {
                            message.attachments.push(Attachment {
                                id: "att_screenshot".to_owned(),
                                filename: "preventivo-postino.pdf".to_owned(),
                                mime: "application/pdf".to_owned(),
                                size: 248_320,
                            });
                            message.summary.has_attachments = true;
                        }
                        self.screenshot_message = Some(message);
                    }
                }
                if state == ScreenshotState::LinkConfirm {
                    self.reading.pending_link = Some(PendingLink {
                        url: "https://example.com/preventivo/postino-2026?token=anteprima".to_owned(),
                        host: "example.com".to_owned(),
                        recipient: None,
                        mailto: false,
                        idn: false,
                        non_ascii: false,
                        backslash: false,
                        focus_cancel: false,
                    });
                }
            }
            ScreenshotState::ReadingInvite => {
                self.demo = false;
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.status.clear();
                let first = self.visible_messages().0.first().map(|message| message.id.clone());
                if let Some(id) = first {
                    self.open_message(id.clone());
                    self.now = 1.0;
                    self.maybe_complete_read();
                    self.toast = None;
                    self.status.clear();
                    if let Some(mut message) = self.engine.message(&id) {
                        message.summary.subject = "Revisione preventivo".to_owned();
                        message.summary.from = Address::new("Giulia Ferri", "giulia.ferri@example.com");
                        message.body_text = "Ciao Federico,\n\nti confermo la revisione del preventivo giovedì mattina in studio. Se non riesci, rispondi pure con un tentativo e troviamo un'altra data.\n\nGiulia"
                            .to_owned();
                        message.body_html = None;
                        message.remote_images = 0;
                        message.attachments.clear();
                        message.summary.has_attachments = false;
                        self.screenshot_message = Some(message);
                        self.screenshot_invite = Some(CalendarInviteView {
                            method: "request".to_owned(),
                            summary: "Revisione preventivo".to_owned(),
                            organizer: Some("giulia.ferri@example.com".to_owned()),
                            attendees: vec!["giulia.ferri@example.com".to_owned(), "federico@example.it".to_owned()],
                            start: Some("2026-11-05T09:00:00".to_owned()),
                            end: Some("2026-11-05T10:00:00".to_owned()),
                            location: Some("Studio Postino, Via Roma 1".to_owned()),
                            recurrence: false,
                            timezone: Some("Europe/Rome".to_owned()),
                            valid: true,
                            invalid_reason: None,
                            cancelled: false,
                            can_reply: true,
                            warnings: vec!["Fuso orario dichiarato ma non applicato: gli orari sono quelli scritti nell'invito.".to_owned()],
                        });
                    }
                }
            }
            ScreenshotState::Settings => {
                // A settings capture must not inherit a selected/read message: the unread badge and
                // the inbox total have to be the same at every window size.
                self.clear_selection();
                self.close_reading();
                self.demo = false;
                self.seed_background_rows();
                let last = self.now_utc.timestamp_millis() - 12 * 60 * 1000;
                self.sync_view = SyncView::Idle { last_sync: Some(last) };
                self.last_sync = Some(last);
                self.store_path = Some(PathBuf::from("C:/Users/demo/AppData/Local/Postino/mail.db"));
                self.show_accounts_dialog = true;
                self.settings_focus_pending = false;
                self.status.clear();
            }
            ScreenshotState::SettingsSignature => {
                self.clear_selection();
                self.close_reading();
                self.demo = false;
                self.seed_background_rows();
                self.show_accounts_dialog = true;
                self.settings_focus_pending = false;
                if let Some(account) = self.engine.accounts().first().cloned() {
                    self.signature_editor = Some(SignatureEditor {
                        account: account.id,
                        text: "Federico\nStudio Postino · example.it\n+39 000 000 0000".to_owned(),
                        focus: false,
                        error: None,
                    });
                }
                self.status.clear();
            }
            ScreenshotState::Outbox => {
                self.outbox = demo_outbox(self.now_utc.timestamp_millis());
                self.outbox_open = true;
                self.outbox_loaded = true;
                self.status.clear();
            }
            ScreenshotState::Search => {
                self.query = "from:giulia is:unread".to_owned();
                self.recording_recent = false;
                self.commit_search();
                self.status.clear();
            }
            ScreenshotState::SearchError => {
                self.query = "subject:\"unclosed".to_owned();
                self.recording_recent = false;
                self.commit_search();
                self.search_error = Some(("unclosed_quote".to_owned(), 8));
                self.status.clear();
            }
            ScreenshotState::Threads => {
                self.threads_view = true;
                self.clear_selection();
                self.close_reading();
                self.clear_thread_selection();
                self.threads_cache = None;
                self.list_cache = None;
                self.status.clear();
                self.toast = None;
            }
            ScreenshotState::Conversation => {
                self.threads_view = true;
                self.select_thread("thr_preventivo".to_owned());
                self.now = 1.0;
                self.maybe_complete_read();
                self.toast = None;
                self.status.clear();
            }
            ScreenshotState::SavedSearches => {
                if !self.engine.accounts().is_empty() && self.saved_searches.is_empty() {
                    for (name, query) in [("Da Giulia", "from:giulia"), ("Fatture", "label:Fatture"), ("Non letti", "is:unread")] {
                        let _ = self.op("saved_search_create", json!({ "name": name, "query": query }));
                    }
                }
                self.refresh_saved_searches();
                self.threads_view = false;
                if let Some(saved) = self.saved_searches.iter().find(|saved| saved.name == "Da Giulia").cloned() {
                    self.apply_saved_search(&saved.id);
                }
                self.status.clear();
                self.toast = None;
            }
            ScreenshotState::Rules | ScreenshotState::RuleEditor | ScreenshotState::RulePreview => {
                let account = self.engine.accounts().first().map(|account| account.id.clone());
                if let Some(account) = account {
                    let empty = self.engine.rules_list(Some(&account)).is_ok_and(|rules| rules.is_empty());
                    if empty {
                        for (name, query, actions, priority) in [
                            ("Da Giulia", "from:giulia", json!([{ "kind": "label", "label": "lbl_1" }]), 10),
                            ("Fatture", "subject:fattura", json!([{ "kind": "label", "label": "lbl_3" }]), 20),
                            ("Newsletter", "subject:newsletter", json!([{ "kind": "move", "folder": "archive" }]), 30),
                        ] {
                            let _ = self.op(
                                "rules_create",
                                json!({ "name": name, "query": query, "actions": actions, "priority": priority, "account": account }),
                            );
                        }
                        let _ = self.op("rules_run", json!({ "account": account, "scope": "new" }));
                    }
                    self.active_account = Some(account);
                }
                self.open_rules(false);
                if state == ScreenshotState::RuleEditor || state == ScreenshotState::RulePreview {
                    let actions = if state == ScreenshotState::RuleEditor {
                        // The 980x600 editor scrolls after one action row; keep the seeded screenshot
                        // and its seed in step instead of showing a second row behind the fold.
                        vec![postino_core::RuleAction::Move { folder: "archive".to_owned() }]
                    } else {
                        vec![postino_core::RuleAction::Move { folder: "archive".to_owned() }, postino_core::RuleAction::Read]
                    };
                    if let Some(page) = self.rules_page.as_mut() {
                        page.seed_editor(&self.engine, "Fatture e ricevute", "subject:fattura", &actions);
                    }
                    if state == ScreenshotState::RulePreview
                        && let Some(page) = self.rules_page.as_mut()
                    {
                        page.preview_now(&mut self.engine);
                    }
                }
                self.status.clear();
                self.toast = None;
            }
        }
    }
    fn seed_onboarding_screenshot(
        &mut self,
        stage: Stage,
        provider: ProviderChoice,
        preview: Option<FoldersPreview>,
        error: Option<ConnectionError>,
        sync: Option<SyncProgress>,
    ) {
        self.engine = Engine::empty();
        self.demo = false;
        self.store = None;
        self.store_path = None;
        self.writer_lock = None;
        self.active_account = None;
        self.folder.clear();
        self.labels_cache.clear();
        self.clear_selection();
        self.close_reading();
        self.sync_view = SyncView::Idle { last_sync: None };
        self.last_sync = None;
        self.sync_worker = None;
        self.boot_failure = None;
        self.status.clear();
        self.onboarding = Some(Onboarding::screenshot(stage, provider, preview, error, sync));
    }
    /// Drive the deterministic capture. Runs only when `--screenshot` built the app.
    pub(super) fn capture_screenshot(&mut self, ctx: &egui::Context) {
        let Some(mode) = self.screenshot.as_mut() else {
            return;
        };
        if !mode.requested {
            mode.frames = mode.frames.saturating_add(1);
            if mode.frames >= SCREENSHOT_SETTLE_FRAMES {
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(UserData::default()));
                mode.requested = true;
            }
            ctx.request_repaint();
            return;
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(image) = image {
            let out = mode.out.clone();
            self.screenshot = None;
            match write_png(&out, &image) {
                Ok(()) => eprintln!("postino-app: screenshot saved to {}", out.display()),
                Err(error) => {
                    eprintln!("postino-app: cannot save screenshot: {error}");
                    std::process::exit(1);
                }
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        mode.waited = mode.waited.saturating_add(1);
        if mode.waited > SCREENSHOT_WAIT_FRAMES {
            eprintln!("postino-app: the window did not return a screenshot after {SCREENSHOT_WAIT_FRAMES} frames; is it hidden or minimized?");
            std::process::exit(1);
        }
        ctx.request_repaint();
    }
}

/// Let the layout and the font atlas settle before asking for the surface.
const SCREENSHOT_SETTLE_FRAMES: u32 = 4;
/// A visible window replies within a frame or two. This bound turns a hidden or minimized window
/// into an actionable error instead of a hung process.
const SCREENSHOT_WAIT_FRAMES: u32 = 120;

/// The folder preview a docs screenshot shows. It is synthetic, like the demo mailbox.
fn demo_folders_preview() -> FoldersPreview {
    FoldersPreview {
        folders: vec![
            FolderPreview { name: "Posta in arrivo".to_owned(), role: FolderRole::Inbox, aggregate: false },
            FolderPreview { name: "Inviati".to_owned(), role: FolderRole::Sent, aggregate: false },
            FolderPreview { name: "Bozze".to_owned(), role: FolderRole::Drafts, aggregate: false },
            FolderPreview { name: "Cestino".to_owned(), role: FolderRole::Trash, aggregate: false },
            FolderPreview { name: "Spam".to_owned(), role: FolderRole::Spam, aggregate: false },
            FolderPreview { name: "Archivio".to_owned(), role: FolderRole::Archive, aggregate: false },
            FolderPreview { name: "Tutte le mail".to_owned(), role: FolderRole::Custom, aggregate: true },
        ],
        warnings: vec!["Il server non dichiara una cartella Preferiti: i preferiti restano locali.".to_owned()],
    }
}

/// Encode the viewport surface exactly as eframe captured it. `ColorImage` is sRGB with the same
/// channel order as PNG's RGBA, so no conversion is involved.
///
/// The bytes go to `<out>.tmp` first and are renamed over the destination, so a failed or killed
/// capture never leaves a truncated PNG behind. The temp file is removed on error.
pub(super) fn write_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    let mut temp_os = path.as_os_str().to_os_string();
    temp_os.push(".tmp");
    let temp = PathBuf::from(temp_os);
    if let Err(error) = encode_png(&temp, image) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("{}: {error}", path.display())
    })
}

fn encode_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    let width = u32::try_from(image.size[0]).map_err(|_| "the screenshot is wider than a PNG allows".to_owned())?;
    let height = u32::try_from(image.size[1]).map_err(|_| "the screenshot is taller than a PNG allows".to_owned())?;
    let pixels = image.pixels.iter().flat_map(|color| color.to_array()).collect::<Vec<u8>>();
    let file = std::fs::File::create(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| format!("{}: {error}", path.display()))?;
    writer.write_image_data(&pixels).map_err(|error| format!("{}: {error}", path.display()))?;
    writer.finish().map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
}
