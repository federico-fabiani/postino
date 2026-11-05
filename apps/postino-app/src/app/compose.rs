//! The composer surface: new mail, replies, drafts, sending, the outbox journal and the
//! composer's own widgets. The shell still owns the state; this module owns the behaviour.

use super::*;

impl PostinoApp {
    /// Attach bytes to a draft through the same op the CLI uses. The daemon path base64-encodes
    /// into the bounded frame and refuses an oversized file before it writes anything.
    fn add_draft_attachment(&mut self, draft_id: &str, filename: &str, mime: Option<&str>, bytes: &[u8]) -> Result<Message, String> {
        if self.daemon_link.is_some() {
            let data = daemon::base64_payload(bytes)?;
            let args = serde_json::json!({ "draft": draft_id, "filename": filename, "mime": mime, "data_base64": data });
            self.op("draft_attachment_add", args).map_err(|error| error.to_string())?;
            self.reload_engine_after_sync(false);
            return self.engine.message(draft_id).ok_or_else(|| "La bozza è stata aggiornata ma l'archivio non la rilegge.".to_owned());
        }
        self.engine.add_draft_attachment(draft_id, filename, mime, bytes).map_err(|error| op_error_copy(&error))
    }

    fn remove_draft_attachment(&mut self, draft_id: &str, attachment: &str) -> Result<Message, String> {
        if self.daemon_link.is_some() {
            let args = serde_json::json!({ "draft": draft_id, "attachment": attachment });
            self.op("draft_attachment_remove", args).map_err(|error| error.to_string())?;
            self.reload_engine_after_sync(false);
            return self.engine.message(draft_id).ok_or_else(|| "La bozza è stata aggiornata ma l'archivio non la rilegge.".to_owned());
        }
        self.engine.remove_draft_attachment(draft_id, attachment).map_err(|error| op_error_copy(&error))
    }

    /// Copy a forwarded attachment into the new draft: bytes are read from the local archive and
    /// written through the daemon op when a daemon owns the store.
    fn copy_attachment_to_draft(&mut self, draft_id: &str, source: &str, metadata: Option<&Attachment>) -> Result<Message, String> {
        if self.daemon_link.is_some() {
            let bytes = self.engine.attachment_bytes(source).map_err(|error| op_error_copy(&error))?;
            let filename = metadata.map_or_else(|| "allegato".to_owned(), |attachment| attachment.filename.clone());
            let mime = metadata.map(|attachment| attachment.mime.clone());
            return self.add_draft_attachment(draft_id, &filename, mime.as_deref(), &bytes);
        }
        self.engine.copy_attachment_to_draft(draft_id, source).map_err(|error| op_error_copy(&error))
    }

    pub(super) fn start_composer(&mut self, mut state: ComposerState) {
        if self.composer.open && self.composer.dirty && !self.save_composer(false) {
            return;
        }
        state.open = true;
        // Over the modal, the first empty field owns the keyboard. A reply with every field filled
        // falls back to the body.
        state.focus = Some(state.first_empty_focus());
        self.composer = state;
        self.pgp_prompt = None;
        self.composer_preview = false;
        self.composer_focus_pending = true;
        self.show_palette = false;
        self.show_shortcuts = false;
        self.show_about = false;
        self.label_menu = None;
    }

    pub(super) fn compose_new(&mut self) {
        let Some(account) = self.account().cloned() else {
            self.status = "Nessun account configurato".to_owned();
            return;
        };
        let mut state = ComposerState::new(ComposeMode::New);
        state.body = postino_core::new_message_body(&account.signature);
        self.start_composer(state);
    }

    pub(super) fn reply_to(&mut self, id: &str, mode: ComposeMode) {
        let Some(message) = self.engine.message(id) else {
            self.status = "Messaggio non trovato".to_owned();
            return;
        };
        let Some(account) = self.account().cloned() else {
            self.status = "Nessun account configurato".to_owned();
            return;
        };
        let target = self.engine.reply_target(id).unwrap_or(postino_core::ReplyTarget { parent_message_id: None, reply_to: Vec::new() });
        let reply_mode = match mode {
            ComposeMode::Forward => postino_core::ReplyMode::Forward,
            ComposeMode::ReplyAll => postino_core::ReplyMode::ReplyAll,
            _ => postino_core::ReplyMode::Reply,
        };
        let prefill = postino_core::compose_reply(&account, &message, target.parent_message_id.as_deref(), &target.reply_to, reply_mode);
        let mut state = ComposerState::new(mode);
        state.to = Address::format_list(&prefill.to);
        state.cc = Address::format_list(&prefill.cc);
        state.show_cc = !prefill.cc.is_empty();
        state.show_bcc = false;
        state.subject = prefill.subject;
        state.body = prefill.body;
        state.in_reply_to = prefill.in_reply_to;
        state.forward_sources =
            if mode == ComposeMode::Forward { prefill.attachments.iter().map(|attachment| attachment.id.clone()).collect() } else { Vec::new() };
        state.attachments = prefill.attachments;
        // A prefill is content: Send, Salva bozza and Esc must create the draft even if the person
        // has not typed anything yet. The old guard treated it as pristine and silently discarded it.
        state.dirty = true;
        state.last_edit = self.now;
        self.start_composer(state);
    }

    pub(super) fn edit_draft(&mut self, id: &str) {
        let Some(message) = self.engine.message(id) else {
            self.status = "Bozza non trovata".to_owned();
            return;
        };
        let is_draft = self
            .engine
            .folders()
            .iter()
            .find(|folder| folder.id == message.summary.folder_id)
            .is_some_and(|folder| folder.kind == FolderKind::Drafts);
        if !is_draft {
            self.status = "Questo messaggio non è una bozza".to_owned();
            return;
        }
        let mut state = ComposerState::new(ComposeMode::Draft);
        state.draft_id = Some(message.summary.id.clone());
        state.to = Address::format_list(&message.summary.to);
        state.cc = Address::format_list(&message.cc);
        state.bcc = Address::format_list(&message.bcc);
        state.show_cc = !message.cc.is_empty();
        state.show_bcc = !message.bcc.is_empty();
        state.subject = message.summary.subject.clone();
        state.body = message.body_text.clone();
        if message.body_html.is_some() {
            state.body_format = BodyFormat::Html;
        }
        if self.account().is_none_or(|account| !account.email.eq_ignore_ascii_case(&message.summary.from.email)) {
            state.sender = Some(message.summary.from.email.clone());
        }
        state.attachments = message.attachments.clone();
        state.in_reply_to = self.engine.draft_reply_link(&message.summary.id).map(str::to_owned);
        state.saved_at = Some(format_time(message.summary.date));
        self.start_composer(state);
    }

    pub(super) fn save_composer(&mut self, notify: bool) -> bool {
        if !self.composer.open {
            return false;
        }
        if !self.composer.dirty && self.composer.draft_id.is_none() {
            let has_content = !self.composer.to.trim().is_empty()
                || !self.composer.cc.trim().is_empty()
                || !self.composer.bcc.trim().is_empty()
                || !self.composer.subject.trim().is_empty()
                || !self.composer.body.trim().is_empty()
                || !self.composer.attachments.is_empty()
                || self.composer.in_reply_to.is_some();
            if !has_content {
                if notify {
                    self.status = "Nessuna modifica da salvare".to_owned();
                }
                return true;
            }
        }
        if !self.composer.dirty && self.composer.draft_id.is_some() {
            if notify {
                self.status = "Bozza già salvata".to_owned();
            }
            return true;
        }
        let (to, cc, bcc) = match self.composer.recipients() {
            Ok(recipients) => recipients,
            Err(error) => {
                self.composer.mark_failed(error, self.now);
                if notify {
                    self.status = "Controlla gli indirizzi della bozza".to_owned();
                }
                return false;
            }
        };
        let pending_forward = if self.composer.draft_id.is_none() && !self.composer.forward_sources.is_empty() {
            self.composer.forward_sources.clone()
        } else {
            Vec::new()
        };
        let attachments_for_save = if pending_forward.is_empty() { self.composer.attachments.clone() } else { Vec::new() };
        let mut args = json!({
            "to": to,
            "cc": cc,
            "bcc": bcc,
            "subject": self.composer.subject,
            "body_text": self.composer.body,
            "attachments": attachments_for_save,
        });
        if (self.composer.draft_id.is_none() || self.composer.body_format_explicit)
            && let Some(object) = args.as_object_mut()
        {
            object.insert("body_format".to_owned(), json!(self.composer.body_format));
        }
        if let Some(from) = self.composer.sender.as_deref().or_else(|| self.account().map(|account| account.email.as_str()))
            && let Some(object) = args.as_object_mut()
        {
            object.insert("from".to_owned(), json!(from));
        }
        if let Some(link) = &self.composer.in_reply_to
            && let Some(object) = args.as_object_mut()
        {
            object.insert("in_reply_to".to_owned(), json!(link));
        }
        if let Some(id) = &self.composer.draft_id
            && let Some(object) = args.as_object_mut()
        {
            object.insert("id".to_owned(), json!(id));
        } else if let Some(account) = &self.active_account
            && let Some(object) = args.as_object_mut()
        {
            object.insert("account".to_owned(), json!(account));
        }
        let result = self.op("draft_save", args);
        match result {
            Ok(value) => match serde_json::from_value::<DraftSave>(value) {
                Ok(saved) => {
                    let id = saved.draft.summary.id.clone();
                    self.composer.draft_id = Some(id.clone());
                    let forward_metadata = self.composer.attachments.clone();
                    self.composer.attachments = saved.draft.attachments;
                    let copy = if pending_forward.is_empty() {
                        ForwardCopy::default()
                    } else {
                        self.copy_forward_attachments(&id, &pending_forward, &forward_metadata)
                    };
                    if let Some(notice) = copy.notice {
                        // The body is saved, but the attachment set is incomplete. Keep the composer
                        // dirty so the state does not claim a complete save, and block Send until the
                        // named rows are removed.
                        self.composer.attachment_failure = Some(notice.clone());
                        self.composer.failed_attachment_ids = copy.failed_ids;
                        self.composer.dirty = true;
                        self.composer.last_edit = self.now;
                        self.composer.notice = Some(notice);
                    } else if self.composer.failed_attachment_ids.is_empty() {
                        self.composer.mark_saved(format_time(self.now_utc));
                        self.composer.attachment_failure = None;
                    } else {
                        // A previous failed forward row is still present; the body save is fine but
                        // the draft is not complete. Keep the blocking sentence and the dirty state.
                        self.composer.dirty = true;
                        self.composer.last_edit = self.now;
                    }
                    if notify {
                        self.status = if self.composer.attachment_failure.is_some() {
                            "Bozza salvata, ma un allegato non è stato copiato".to_owned()
                        } else {
                            "Bozza salvata".to_owned()
                        };
                    }
                    true
                }
                Err(_) => {
                    let message = draft_save_error_message(&self.composer.attachment_failure, "Non è stato possibile salvare la bozza.");
                    self.composer.mark_failed(message, self.now);
                    false
                }
            },
            Err(error) => {
                let message = draft_save_error_message(&self.composer.attachment_failure, &error.to_string());
                self.composer.mark_failed(message, self.now);
                if notify {
                    self.status = "La bozza non è stata salvata".to_owned();
                }
                false
            }
        }
    }

    /// Copy a forwarded message's attachment bytes into the new draft.
    ///
    /// The first save deliberately writes no metadata rows for them: a row without bytes would be
    /// visible before the copy. Every successful copy replaces the composer's list with the stored
    /// rows; a failed source becomes a metadata-only row with a fresh id, which is reported and makes
    /// the next send refuse instead of silently dropping it.
    fn copy_forward_attachments(&mut self, draft_id: &str, sources: &[String], sources_metadata: &[Attachment]) -> ForwardCopy {
        let mut copied = Vec::new();
        let mut failures = Vec::new();
        for source in sources {
            let metadata = sources_metadata.iter().find(|attachment| attachment.id == *source);
            match self.copy_attachment_to_draft(draft_id, source, metadata) {
                Ok(draft) => copied = draft.attachments,
                Err(error) => {
                    let metadata = sources_metadata.iter().find(|attachment| attachment.id == *source).cloned().unwrap_or(Attachment {
                        id: String::new(),
                        filename: "allegato".to_owned(),
                        mime: "application/octet-stream".to_owned(),
                        size: 0,
                    });
                    failures.push((metadata, error.to_string()));
                }
            }
        }
        let mut attachments = copied;
        let mut failed_ids = Vec::new();
        for (index, (metadata, _)) in failures.iter().enumerate() {
            let mut metadata = metadata.clone();
            // A real id makes the row removable before it is persisted. It is also valid for the
            // next draft_save, which turns the metadata-only row into a visible blocker.
            metadata.id = format!("{draft_id}_local_att_{index}");
            failed_ids.push(metadata.id.clone());
            attachments.push(metadata);
        }
        self.composer.attachments = attachments;
        self.composer.forward_sources.clear();
        if failures.is_empty() {
            return ForwardCopy::default();
        }
        let first = failures.first().map(|(attachment, error)| format!("{} ({error})", attachment.filename)).unwrap_or_default();
        let more = failures.len().saturating_sub(1);
        let suffix = if more == 0 {
            ""
        } else if more == 1 {
            " e un altro file"
        } else {
            " e altri file"
        };
        ForwardCopy { notice: Some(format!("Allegato non copiato: {first}{suffix}. Rimuovilo per poter inviare.")), failed_ids }
    }

    pub(super) fn send_composer(&mut self) {
        self.send_composer_with(None);
    }

    /// The real send path. `passphrase` is present only after the signing sheet confirmed it; it is
    /// inserted into the operation arguments once and never stored on the draft or in Settings.
    pub(super) fn send_composer_with(&mut self, passphrase: Option<&str>) {
        if !self.composer.open {
            return;
        }
        // The Send button, Ctrl+Enter and the composer's own enabled rule must agree: a demo or an
        // account without an SMTP endpoint has no path to a submission, so the shortcut must not
        // attempt one and fail later.
        if !self.send_allowed_for_active_account() {
            let reason = if self.demo { DEMO_SEND_REASON } else { NO_SMTP_SEND_REASON };
            self.composer.notice = Some(reason.to_owned());
            self.status = reason.to_owned();
            return;
        }
        if !self.pgp_keys_loaded && !self.demo {
            self.refresh_pgp_keys();
        }
        if !self.save_composer(true) {
            return;
        }
        if let Some(failure) = self.composer.attachment_failure.clone() {
            self.composer.mark_failed(failure, self.now);
            return;
        }
        let Some(id) = self.composer.draft_id.clone() else {
            self.composer.mark_failed("Aggiungi almeno un destinatario prima di inviare", self.now);
            return;
        };
        let sign = self.composer.pgp_sign.clone();
        let encrypt = self.composer.pgp_encrypt.clone();
        if let Some(key_id) = sign.as_deref()
            && !self.pgp_keys.iter().any(|key| key.id == key_id)
        {
            // A transient list failure must not turn into "key no longer available": try once more
            // with a fresh request before telling the user to pick another key.
            self.pgp_keys_retry_at = 0.0;
            self.refresh_pgp_keys();
        }
        if let Some(key_id) = sign.as_deref() {
            let Some(key) = self.pgp_keys.iter().find(|key| key.id == key_id) else {
                self.composer.mark_failed("La chiave PGP scelta non è più disponibile: scegline un'altra.", self.now);
                return;
            };
            if key.protected && passphrase.is_none() {
                self.pgp_prompt = Some(PgpPromptState::new(PgpPromptPurpose::Send));
                return;
            }
        }
        let mut args = json!({ "draft": id });
        if let Some(pgp) = pgp_send_request(sign.as_deref(), &encrypt, passphrase) {
            args["pgp"] = pgp;
        }
        match self.op("message_send", args) {
            Ok(value) => match serde_json::from_value::<SendQueued>(value) {
                Ok(queued) => {
                    let row = queued.queued.outbox;
                    let until = queued.queued.undo_until;
                    self.composer_preview = false;
                    self.composer = ComposerState::closed();
                    self.status = "Messaggio in coda: puoi annullarlo entro pochi secondi".to_owned();
                    self.pending_send = Some(row.send_at);
                    let seconds = until.map_or(5, |at| ((at - self.now_utc.timestamp_millis()).max(0) as f64 / 1000.0).ceil() as u64);
                    self.push_send_toast(
                        format!("Messaggio in coda. Invio tra {seconds} secondi."),
                        UndoAction::CancelSend(row.id),
                        until.map_or(self.now + 5.0, |at| self.now + ((at - self.now_utc.timestamp_millis()).max(0) as f64 / 1000.0)),
                    );
                    self.refresh_outbox();
                }
                Err(_) => self.composer.mark_failed("Non è stato possibile leggere la coda di invio.", self.now),
            },
            Err(error) => {
                let raw = error.to_string();
                let message = send_error_message(&raw, self.demo);
                self.composer.mark_failed(message.clone(), self.now);
                if passphrase.is_some() && (raw.contains("passphrase does not unlock") || raw.contains("signing key is passphrase-protected")) {
                    let mut prompt = PgpPromptState::new(PgpPromptPurpose::Send);
                    prompt.error = Some("La passphrase non apre la chiave. Riprova.".to_owned());
                    self.pgp_prompt = Some(prompt);
                }
            }
        }
    }

    /// Queue the due rows on the worker thread. The undo window has already elapsed when the caller
    /// asks; a second request while a pass runs is ignored.
    pub(super) fn request_send(&mut self) {
        let Some(worker) = self.send_worker.as_mut() else {
            return;
        };
        if worker.request(self.now_utc.timestamp_millis()) {
            self.pending_send = None;
        }
    }

    pub(super) fn handle_send_events(&mut self) {
        let Some(worker) = self.send_worker.as_mut() else {
            return;
        };
        let events = worker.poll();
        if events.is_empty() {
            return;
        }
        let mut failed = None;
        let mut moved = false;
        for event in events {
            match event {
                SendEvent::Finished(report) => {
                    // A submitted message is moved to Sent even when the server APPEND is still
                    // pending, so both counts mean "the local draft may have moved".
                    moved |= report.sent > 0 || report.append_pending > 0;
                    self.after_outbox_run(&report);
                }
                SendEvent::Failed(message) => failed = Some(message),
            }
        }
        if moved {
            // The worker wrote through its own connection: the draft moved to Sent and the Sent row
            // carries the Message-ID. Reload the mailbox (keeping every view state) or the UI keeps
            // showing a draft that no longer exists and a Sent folder that is missing the copy.
            self.reload_engine_after_sync(false);
            self.labels_cache = self.engine.label_summaries();
        }
        self.refresh_outbox();
        if let Some(message) = failed {
            self.push_info_toast(message, ToastKind::Error);
        }
        if let Some(next) = self.next_outbox_due() {
            self.pending_send = Some(next);
        }
    }

    fn after_outbox_run(&mut self, report: &OutboxRunReport) {
        if report.sent > 0 {
            self.status = "Messaggio inviato".to_owned();
        }
        if report.uncertain > 0 {
            self.push_info_toast("Invio incerto: controlla la cartella Inviati del provider prima di riprovare.", ToastKind::Error);
        } else if report.failed > 0 {
            let reason = report.errors.first().cloned().unwrap_or_else(|| "L'invio non è riuscito.".to_owned());
            self.push_info_toast(reason, ToastKind::Error);
        } else if !report.halted.is_empty() {
            self.push_info_toast("L'invio è fermo per un account: controlla le credenziali nella coda.", ToastKind::Error);
        } else if report.sent > 0 {
            self.push_info_toast("Messaggio inviato", ToastKind::Success);
        }
    }

    /// Refresh the cached journal. Returns whether a row reached a delivered state or vanished since
    /// the previous refresh: another process may have sent a draft or marked an uncertain row sent,
    /// and the mailbox (not only the journal) must be reloaded.
    pub(super) fn refresh_outbox(&mut self) -> bool {
        if self.demo {
            return false;
        }
        let Ok(outbox) = self.engine.outbox_list(None, 500, 0) else {
            return false;
        };
        let previous = std::mem::replace(&mut self.outbox, empty_outbox());
        let delivered =
            |state: postino_store::OutboxState| matches!(state, postino_store::OutboxState::Sent | postino_store::OutboxState::AppendPending);
        let transitioned = outbox
            .entries
            .iter()
            .any(|entry| delivered(entry.state) && previous.entries.iter().find(|old| old.id == entry.id).is_none_or(|old| !delivered(old.state)));
        let vanished = previous.entries.iter().any(|old| !outbox.entries.iter().any(|entry| entry.id == old.id));
        self.outbox = outbox;
        transitioned || vanished
    }

    /// Open the outbox through one path: the command table, the tray startup flag and any future
    /// caller all land here, so they cannot drift on whether the journal is refreshed first.
    pub(super) fn open_outbox(&mut self) {
        self.refresh_outbox();
        self.outbox_open = true;
        self.status.clear();
    }

    /// One scheduler tick: refresh the journal, reload the mailbox if another process delivered
    /// something, then cache the next wake time and ask the worker if it is due. The 2 s poll (and
    /// the cached `pending_send`) is the only scheduler; no per-frame database query.
    pub(super) fn poll_outbox(&mut self) {
        if self.refresh_outbox() {
            // Keep every view state: the existing post-sync reload repairs the selection and folder.
            self.reload_engine_after_sync(false);
            self.labels_cache = self.engine.label_summaries();
            self.refresh_outbox();
        }
        self.pending_send = self.next_outbox_due();
        self.pump_outbox();
    }

    pub(super) fn next_outbox_due(&self) -> Option<i64> {
        // The store answers directly: a queued or unowned append row's due time, or a leased row's
        // lease expiry. The cached list is a display surface, never the scheduler.
        self.engine.next_outbox_wake().ok().flatten()
    }

    /// Queue and process a send from a worker when the cached wake time is due. Used by launch
    /// recovery, by the undo deadline and by the 2 s poll.
    pub(super) fn pump_outbox(&mut self) {
        if self.send_worker.as_ref().is_some_and(|worker| worker.is_running()) {
            return;
        }
        let now = self.now_utc.timestamp_millis();
        if self.pending_send.is_some_and(|at| at <= now) {
            self.request_send();
        }
    }

    fn push_send_toast(&mut self, message: impl Into<String>, undo: UndoAction, until: f64) {
        self.failure = None;
        self.toast = Some(ToastState { message: message.into(), undo: Some(undo), until, kind: ToastKind::Info });
    }

    pub(super) fn close_composer(&mut self) {
        if !self.composer.open {
            return;
        }
        if self.composer.dirty && !self.save_composer(true) {
            return;
        }
        self.pgp_prompt = None;
        self.composer_preview = false;
        self.composer = ComposerState::closed();
    }

    pub(super) fn discard_composer(&mut self) {
        if !self.composer.open {
            return;
        }
        if let Some(id) = self.composer.draft_id.clone()
            && let Some(message) = self.engine.message(&id)
        {
            let account = self.folder(&message.summary.folder_id).map(|folder| folder.account_id.clone());
            let previous = message.summary.folder_id.clone();
            let Some(trash) = account.as_deref().and_then(|account| self.folder_for_kind_in(account, FolderKind::Trash)) else {
                self.composer.mark_failed("Il server non ha una cartella Cestino.", self.now);
                return;
            };
            match self.op("message_move", json!({ "id": id, "folder": trash })) {
                Ok(_) => {
                    self.composer = ComposerState::closed();
                    self.pgp_prompt = None;
                    self.composer_preview = false;
                    self.status = "Bozza spostata nel cestino".to_owned();
                    self.push_toast("Bozza scartata", UndoAction::Move(vec![(id, previous)]));
                }
                Err(_) => self.composer.mark_failed("Non è stato possibile spostare la bozza nel cestino.", self.now),
            }
            return;
        }
        self.composer = ComposerState::closed();
        self.composer_preview = false;
        self.status = "Bozza scartata".to_owned();
    }

    /// Open the injected picker and attach every chosen file. A cancellation is a no-op; a demo or
    /// store-less session refuses before the picker (and therefore before any filesystem read).
    pub(super) fn pick_composer_files(&mut self) {
        if !self.composer.open {
            return;
        }
        if self.demo || self.store.is_none() {
            self.push_info_toast("La modalità demo non allega file: serve un account su un archivio locale cifrato.", ToastKind::Error);
            return;
        }
        match self.file_picker.choose_files() {
            Ok(paths) if paths.is_empty() => {}
            Ok(paths) => self.attach_composer_files(paths),
            Err(_) => self.push_info_toast("Non è stato possibile aprire la scelta dei file.", ToastKind::Error),
        }
    }

    /// Read, bound and attach a batch of files through the same engine path the CLI and MCP use.
    ///
    /// Every file is checked by metadata and then by the number of bytes actually read, so a file
    /// that grows between the two checks is refused before decoding. Failures are collected into one
    /// toast; the draft keeps the attachments that did succeed.
    pub(super) fn attach_composer_files(&mut self, mut paths: Vec<PathBuf>) {
        if !self.composer.open || paths.is_empty() {
            return;
        }
        // A drop can be reported by more than one rectangle; a picker can return the same path
        // twice. Canonicalize for the key (or keep the literal path when it cannot be resolved) so
        // one file is attached once.
        let mut seen = BTreeSet::new();
        paths.retain(|path| seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())));
        if paths.is_empty() {
            return;
        }
        if self.demo || self.store.is_none() {
            self.push_info_toast("La modalità demo non allega file: serve un account su un archivio locale cifrato.", ToastKind::Error);
            return;
        }
        let mut was_dirty = self.composer.dirty;
        if self.composer.draft_id.is_none() {
            // A first attachment needs a draft id. Save the metadata-only draft first; the composer
            // is already open and its content is what the person sees.
            self.composer.dirty = true;
            self.composer.last_edit = self.now;
            if !self.save_composer(false) {
                self.push_info_toast("Non è stato possibile preparare la bozza per gli allegati.", ToastKind::Error);
                return;
            }
            // The initial save persisted the body; only an attachment-only change remains.
            was_dirty = false;
        }
        let Some(draft_id) = self.composer.draft_id.clone() else {
            return;
        };
        let mut failures = Vec::new();
        let mut added = 0usize;
        for path in paths {
            let filename = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            match read_attachment_file(&path) {
                Ok(bytes) => match self.add_draft_attachment(&draft_id, &filename, None, &bytes) {
                    Ok(draft) => {
                        self.composer.attachments = draft.attachments;
                        added = added.saturating_add(1);
                    }
                    Err(error) => failures.push(format!("{filename}: {error}")),
                },
                Err(error) => failures.push(format!("{filename}: {error}")),
            }
        }
        if added > 0 {
            if was_dirty {
                // The attachment operation persisted the old body snapshot; the text the person just
                // typed is still unsaved. Keep the composer dirty so Send/Close/autosave save it.
                self.composer.dirty = true;
                self.composer.last_edit = self.now;
            } else {
                self.composer.mark_saved(format_time(self.now_utc));
            }
            self.status = if added == 1 { "Allegato aggiunto".to_owned() } else { format!("{added} allegati aggiunti") };
        }
        if !failures.is_empty() {
            let first = failures.first().cloned().unwrap_or_default();
            let more = failures.len().saturating_sub(1);
            let suffix = if more == 0 { String::new() } else { format!(" (e altri {more})") };
            self.push_info_toast(format!("File non allegato: {first}{suffix}"), ToastKind::Error);
        }
    }

    /// Remove one attachment row. A demo or unsaved row is removed from the composer list only;
    /// with a store the operation deletes the row and the composer adopts the returned draft.
    pub(super) fn remove_composer_attachment(&mut self, attachment: &str) {
        let pending_forward = self.composer.forward_sources.iter().any(|source| source == attachment);
        if pending_forward {
            self.composer.attachments.retain(|candidate| candidate.id != attachment);
            self.composer.forward_sources.retain(|source| source != attachment);
            self.composer.touch(self.now);
            return;
        }
        let failed = self.composer.failed_attachment_ids.iter().any(|id| id == attachment) || attachment.is_empty();
        let local_only = failed || self.demo || self.store.is_none() || self.composer.draft_id.is_none();
        if local_only {
            self.composer.attachments.retain(|candidate| candidate.id != attachment);
            self.composer.failed_attachment_ids.retain(|id| id != attachment);
            if self.composer.failed_attachment_ids.is_empty() {
                self.composer.attachment_failure = None;
            }
            self.composer.touch(self.now);
            self.status = "Allegato rimosso".to_owned();
            return;
        }
        let was_dirty = self.composer.dirty;
        let Some(draft_id) = self.composer.draft_id.clone() else {
            return;
        };
        match self.remove_draft_attachment(&draft_id, attachment) {
            Ok(draft) => {
                self.composer.attachments = draft.attachments;
                if self.composer.failed_attachment_ids.is_empty() {
                    self.composer.attachment_failure = None;
                }
                if was_dirty {
                    self.composer.dirty = true;
                    self.composer.last_edit = self.now;
                } else {
                    self.composer.mark_saved(format_time(self.now_utc));
                }
                self.status = "Allegato rimosso".to_owned();
            }
            Err(_) => self.push_info_toast("Non è stato possibile rimuovere l'allegato.", ToastKind::Error),
        }
    }

    pub(super) fn start_invite_reply(&mut self, message_id: &str, response: InviteResponse) {
        let parent = self.engine.reply_target(message_id).ok().and_then(|target| target.parent_message_id);
        match self.op("invite_reply_draft", json!({ "message": message_id, "response": response })) {
            Ok(value) => match serde_json::from_value::<DraftSave>(value) {
                Ok(saved) => {
                    let draft = saved.draft;
                    let mut state = ComposerState::new(ComposeMode::Draft);
                    state.draft_id = Some(draft.summary.id.clone());
                    state.to = Address::format_list(&draft.summary.to);
                    state.cc = Address::format_list(&draft.cc);
                    state.bcc = Address::format_list(&draft.bcc);
                    state.show_cc = !draft.cc.is_empty();
                    state.show_bcc = !draft.bcc.is_empty();
                    state.subject = draft.summary.subject.clone();
                    state.body = draft.body_text.clone();
                    if draft.body_html.is_some() {
                        state.body_format = BodyFormat::Html;
                    }
                    if self.account().is_none_or(|account| !account.email.eq_ignore_ascii_case(&draft.summary.from.email)) {
                        state.sender = Some(draft.summary.from.email.clone());
                    }
                    state.attachments = draft.attachments.clone();
                    state.in_reply_to = parent;
                    state.saved_at = Some(format_time(draft.summary.date));
                    self.start_composer(state);
                    self.push_info_toast("Bozza creata: rivedi e invia. Nessuna risposta automatica.", ToastKind::Info);
                }
                Err(_) => self.push_info_toast("La bozza è stata creata ma non è stato possibile aprirla: cercala in Bozze.", ToastKind::Info),
            },
            Err(error) => self.push_info_toast(invite_reply_error(&error.to_string()), ToastKind::Info),
        }
    }

    /// Show the exact HTML part the engine will compile from the current source. Nothing is sent,
    /// stored or fetched: the preview is a local render of the email-safe compiler output.
    pub(super) fn composer_preview_modal(&mut self, ui: &mut Ui) {
        if !self.composer_preview || !self.composer.open {
            return;
        }
        let source = self.composer.body.clone();
        let compiled = postino_mime::compile_html_email(&source);
        let t = theme::tokens(self.dark);
        let mut close = false;
        let mut clicked_link = None;
        let response = egui::Modal::new(Id::new("postino-composer-preview"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(760.0_f32.min(ui.ctx().content_rect().width() - 40.0));
                ui.label(typography::strong(Role::Emphasis, "Anteprima HTML", t.text));
                ui.label(typography::text(
                    Role::Caption,
                    "Email-safe: tabelle con CSS inline, nessuna richiesta remota, nessuno script.",
                    t.text_faint,
                ));
                ui.add_space(space::MD);
                let max_height = (ui.ctx().content_rect().height() - 260.0).clamp(180.0, 430.0);
                ScrollArea::vertical().max_height(max_height).auto_shrink([false, false]).show(ui, |ui| match &compiled {
                    Ok(composed) => {
                        let view = body::SubsetBodyRenderer.render(&body::StoredBody { html: Some(&composed.html), text: &composed.text });
                        let mut budget = reading::MAX_LINKS_PER_MESSAGE;
                        match view {
                            body::View::Blocks(body) => clicked_link = body_ui::blocks(ui, &body, &t, &mut budget),
                            body::View::Text { text, .. } => {
                                for paragraph in text.split("\n\n") {
                                    let paragraph = paragraph.trim();
                                    if paragraph.is_empty() {
                                        continue;
                                    }
                                    let (link, used) = link_paragraph(ui, paragraph, &t, false, budget);
                                    if link.is_some() {
                                        clicked_link = link;
                                    }
                                    budget = budget.saturating_sub(used);
                                    ui.add_space(space::SM);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        ui.label(typography::text(Role::Secondary, format!("Il corpo non può essere compilato: {error}"), t.text_muted));
                    }
                });
                ui.add_space(space::LG);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = Button::primary("Chiudi").show(ui).clicked();
                });
            });
        #[cfg(test)]
        probe::composer_preview_modal(ui, response.response.rect);
        if let Some(link) = clicked_link {
            self.request_link_open(&link);
        }
        if close || response.should_close() {
            self.composer_preview = false;
        }
    }

    /// Whether an account has a submission endpoint. The demo never does, and a store account is
    /// read-only until SMTP is configured; the composer disables Send with a reason instead of
    /// queueing a row that cannot leave.
    fn smtp_available_for(&self, account_id: &str) -> bool {
        if self.demo {
            return false;
        }
        self.engine
            .account_views()
            .ok()
            .and_then(|views| views.into_iter().find(|view| view.account.id == account_id))
            .and_then(|view| view.server)
            .is_some_and(|server| server.smtp.is_some())
    }

    /// Whether the active account can submit at all. The composer, its Send button and Ctrl+Enter
    /// all read this one rule. A failed account lookup is a "no" with the same visible reason.
    pub(super) fn send_allowed_for_active_account(&self) -> bool {
        if self.demo {
            return false;
        }
        self.account().is_some_and(|account| self.smtp_available_for(&account.id))
    }

    /// The submission journal. Rows are the product's honesty surface: every state is named, a
    /// failure keeps the server's sentence, and an uncertain row never offers a silent retry.
    pub(super) fn outbox_dialog(&mut self, ui: &mut Ui) {
        if !self.outbox_open {
            return;
        }
        let t = theme::tokens(self.dark);
        let window = ui.ctx().content_rect();
        let width = (window.width() - 2.0 * space::HUGE).clamp(560.0, 780.0);
        // The card must end above the status bar even at 980×600. The frame is capped first, then
        // the list takes only the room left after the fixed header and footer.
        let card_height = (window.height() - ui_size::STATUS - 2.0 * space::XXL).clamp(280.0, 500.0);
        let mut close = false;
        let mut open_settings = false;
        let mut action = None;
        let response = egui::Modal::new(Id::new("postino-outbox"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XXL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(width);
                // `set_max_height` applies to the content, and the modal frame adds its inner
                // margin on both sides. Subtract that margin so the painted card (not just its
                // contents) ends above the status bar.
                ui.set_max_height(card_height - 2.0 * f32::from(inset::XXL));
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            ui.label(typography::strong(Role::Section, "Coda di invio", t.text));
                            if self.demo {
                                Badge::new("Dati demo").kind(BadgeKind::Accent).show(ui);
                            }
                        });
                        ui.label(typography::text(
                            Role::Meta,
                            "Il messaggio è salvato prima di partire. Puoi annullare finché è in coda; dopo l'invio la copia in Inviati viene ritentata da sola. Postino invia solo mentre l'app o il comando postino send è in esecuzione.",
                            t.text_faint,
                        ));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if Button::ghost("Chiudi").show(ui).clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(space::SM);
                if !self.outbox.halted.is_empty() {
                    for halt in self.outbox.halted.clone() {
                        Frame::new()
                            .fill(t.danger.gamma_multiply(0.12))
                            .stroke(Stroke::new(1.0, t.danger))
                            .corner_radius(radius::CONTROL)
                            .inner_margin(Margin::symmetric(inset::LG, inset::SM))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(icons::icon(icons::WARNING, ui_size::ICON_SM, t.danger));
                                    ui.vertical(|ui| {
                                        ui.label(typography::strong(Role::BodySmall, format!("Invio fermo per {}", halt.account), t.danger));
                                        ui.label(typography::text(Role::Caption, halt.error.as_str(), t.text_muted));
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if Button::ghost("Apri impostazioni").size(ButtonSize::Sm).show(ui).clicked() {
                                            open_settings = true;
                                        }
                                    });
                                });
                            });
                        ui.add_space(space::XS);
                    }
                }
                if self.outbox.entries.is_empty() {
                    ui.add_space(space::LG);
                    ui.vertical_centered(|ui| {
                        ui.label(icons::icon(icons::SEND, 28.0, t.text_faint));
                        ui.add_space(space::SM);
                        ui.label(typography::strong(Role::Emphasis, "Nessun messaggio in coda", t.text));
                        ui.label(typography::text(Role::BodySmall, "Le bozze restano in Bozze finché non scegli Invia.", t.text_muted));
                    });
                    ui.add_space(space::LG);
                } else {
                    let list_height = (ui.available_height() - 44.0).max(80.0);
                    ScrollArea::vertical().id_salt("outbox-scroll").max_height(list_height).auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().scroll = crate::ui::tokens::scroll_style();
                        for entry in self.outbox.entries.clone() {
                            Self::outbox_row(ui, &t, &entry, self.now_utc.timestamp_millis(), self.now_utc, &mut action);
                            ui.add_space(space::XS);
                        }
                    });
                }
                // A damaged row is shown with "Senza oggetto" and no explanation unless its warning
                // reaches the screen; the CLI prints these, so the journal must too.
                for warning in self.outbox.warnings.clone() {
                    ui.add_space(space::XS);
                    ui.horizontal(|ui| {
                        ui.label(icons::icon(icons::WARNING, ui_size::ICON_SM, t.text_muted));
                        ui.label(typography::text(Role::Caption, warning, t.text_muted));
                    });
                }
                ui.add_space(space::SM);
                let _footer = ui.horizontal(|ui| {
                    ui.label(typography::text(Role::Caption, "Ctrl+Shift+O apre questa coda · Esc chiude", t.text_faint));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if Button::ghost("Aggiorna").icon(icons::SYNC).show(ui).clicked() {
                            self.outbox_loaded = false;
                        }
                    });
                });
                #[cfg(test)]
                probe::outbox_footer(ui, _footer.response.rect);
            });
        #[cfg(test)]
        probe::outbox_card(ui, response.response.rect);
        if close || response.should_close() {
            self.outbox_open = false;
            self.outbox_confirm = None;
            ui.ctx().memory_mut(|memory| memory.stop_text_input());
        }
        if open_settings {
            self.outbox_open = false;
            self.outbox_confirm = None;
            self.open_settings(ui.ctx(), None);
        }
        match action {
            Some(OutboxUiAction::Cancel(id)) => match self.op("outbox_cancel", json!({ "id": id })) {
                Ok(_) => {
                    self.refresh_outbox();
                    self.push_info_toast("Invio annullato: la bozza resta in Bozze.", ToastKind::Success);
                }
                Err(_) => self.push_info_toast("Non è stato possibile annullare: l'invio potrebbe essere già partito.", ToastKind::Error),
            },
            Some(OutboxUiAction::Discard(id)) => match self.op("outbox_cancel", json!({ "id": id })) {
                Ok(_) => {
                    self.refresh_outbox();
                    self.push_info_toast("Riga scartata: la bozza resta in Bozze e si può correggere.", ToastKind::Success);
                }
                Err(_) => self.push_info_toast("Non è stato possibile scartare la riga: aggiorna la coda.", ToastKind::Error),
            },
            Some(OutboxUiAction::Retry { id, subject, uncertain }) => {
                self.outbox_confirm = Some(OutboxConfirm::Retry { id, subject, uncertain });
            }
            Some(OutboxUiAction::MarkSent { id, subject }) => {
                self.outbox_confirm = Some(OutboxConfirm::MarkSent { id, subject });
            }
            None => {}
        }
    }

    fn outbox_row(
        ui: &mut Ui,
        t: &crate::theme::Tokens,
        entry: &OutboxView,
        now_ms: i64,
        now_utc: DateTime<Utc>,
        action: &mut Option<OutboxUiAction>,
    ) {
        let (label, kind) = outbox_state_label(entry.state);
        Frame::new()
            .fill(t.panel_alt)
            .stroke(Stroke::new(1.0, if entry.state == postino_store::OutboxState::Uncertain { t.danger } else { t.border }))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::SM))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            let subject = if entry.subject.trim().is_empty() { "Senza oggetto" } else { entry.subject.as_str() };
                            ui.add(egui::Label::new(typography::strong(Role::BodySmall, subject, t.text)).truncate());
                            Badge::new(label).kind(kind).show(ui);
                        });
                        let recipients = plural(entry.recipient_count, "destinatario", "destinatari");
                        let when = if entry.state == postino_store::OutboxState::Queued && entry.send_at > now_ms {
                            format!("parte {} · richiede Postino aperto", format_millis(entry.send_at, now_utc))
                        } else {
                            format_millis(entry.send_at, now_utc)
                        };
                        ui.label(typography::text(Role::Caption, format!("{recipients} · {} · tentativi {}", when, entry.attempts), t.text_faint));
                        if !entry.message_id.is_empty() {
                            ui.add(egui::Label::new(typography::mono(entry.message_id.as_str(), type_size::CAPTION, t.text_faint)).truncate());
                        }
                        if let Some(error) = &entry.last_error {
                            ui.add_space(space::XS);
                            ui.label(typography::text(Role::Caption, error.as_str(), t.danger));
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match entry.state {
                        postino_store::OutboxState::Queued => {
                            if Button::ghost("Annulla").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::Cancel(entry.id.clone()));
                            }
                        }
                        postino_store::OutboxState::Uncertain => {
                            if Button::danger("Riprova manualmente").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::Retry { id: entry.id.clone(), subject: entry.subject.clone(), uncertain: true });
                            }
                            if Button::ghost("Segna come inviato").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::MarkSent { id: entry.id.clone(), subject: entry.subject.clone() });
                            }
                        }
                        postino_store::OutboxState::Failed => {
                            let clicked = Button::primary("Riprova").size(ButtonSize::Sm).show(ui).clicked();
                            if clicked {
                                *action = Some(OutboxUiAction::Retry { id: entry.id.clone(), subject: entry.subject.clone(), uncertain: false });
                            }
                            if Button::ghost("Scarta").size(ButtonSize::Sm).show(ui).clicked() {
                                *action = Some(OutboxUiAction::Discard(entry.id.clone()));
                            }
                        }
                        _ => {}
                    });
                });
            });
    }

    pub(super) fn outbox_confirm_dialog(&mut self, ui: &mut Ui) {
        let Some(confirm) = self.outbox_confirm.clone() else {
            return;
        };
        let t = theme::tokens(self.dark);
        let (title, body, confirm_label, dangerous) = match &confirm {
            OutboxConfirm::Retry { uncertain, subject, .. } => (
                "Riprovare l'invio?",
                if *uncertain {
                    format!(
                        "«{subject}» ha un esito incerto: il server potrebbe averlo già accettato. Riprovare può consegnarlo due volte. Controlla prima la cartella Inviati del provider."
                    )
                } else {
                    format!("«{subject}» verrà rimesso in coda con gli stessi byte salvati. Il server potrebbe rifiutarlo di nuovo.")
                },
                "Riprova",
                *uncertain,
            ),
            OutboxConfirm::MarkSent { subject, .. } => (
                "Segnare come inviato?",
                format!(
                    "«{subject}» verrà marcato come inviato senza contattare il server. Postino non può dimostrare che sia arrivato: controlla la cartella Inviati del provider."
                ),
                "Segna come inviato",
                false,
            ),
        };
        let mut close = false;
        let mut accept = false;
        let response = egui::Modal::new(Id::new("postino-outbox-confirm"))
            .backdrop_color(theme::scrim(self.dark))
            .frame(Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XXL)))
            .show(ui.ctx(), |ui| {
                ui.set_width(440.0);
                ui.label(typography::strong(Role::Section, title, t.text));
                ui.add_space(space::SM);
                ui.label(typography::text(Role::BodySmall, body, t.text_muted));
                ui.add_space(space::LG);
                ui.horizontal(|ui| {
                    if Button::ghost("Annulla").show(ui).clicked() {
                        close = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = confirm_label;
                        let button = if dangerous { Button::danger(label) } else { Button::primary(label) };
                        if button.show(ui).clicked() {
                            accept = true;
                        }
                    });
                });
            });
        if close || response.should_close() {
            self.outbox_confirm = None;
            return;
        }
        if !accept {
            return;
        }
        let result = match &confirm {
            OutboxConfirm::Retry { id, .. } => self.op("outbox_retry", json!({ "id": id, "confirm": true })),
            OutboxConfirm::MarkSent { id, .. } => self.op("outbox_mark_sent", json!({ "id": id, "confirm": true })),
        };
        self.outbox_confirm = None;
        let marked_sent = matches!(confirm, OutboxConfirm::MarkSent { .. });
        match result {
            Ok(_) => {
                if marked_sent {
                    // `outbox_mark_sent` moves the local draft to Sent; the UI engine still has it in
                    // Drafts until the mailbox is reloaded from the store.
                    self.reload_engine_after_sync(false);
                    self.labels_cache = self.engine.label_summaries();
                }
                self.refresh_outbox();
                self.outbox_loaded = false;
                self.pending_send = self.next_outbox_due();
                self.push_info_toast("Coda aggiornata.", ToastKind::Success);
            }
            Err(_) => self.push_info_toast("Non è stato possibile aggiornare la coda.", ToastKind::Error),
        }
    }

    /// Insert a local template without discarding what the person already wrote: an empty subject
    /// takes the template subject, an empty body takes the template body, otherwise the body is
    /// appended after a blank line. Insertion is undoable through the composer's own text undo.
    pub(super) fn insert_template(&mut self, id: &str) {
        let Some(template) = self.templates.iter().find(|template| template.id == id).cloned() else {
            self.push_info_toast("Il modello non è più disponibile.", ToastKind::Error);
            return;
        };
        if self.composer.subject.trim().is_empty() {
            self.composer.subject = template.subject.clone();
        }
        if self.composer.body.trim().is_empty() {
            self.composer.body = template.body.clone();
        } else {
            self.composer.body.push_str("\n\n");
            self.composer.body.push_str(&template.body);
        }
        self.composer.focus = Some(ComposerFocus::Body);
        self.composer.touch(self.now);
        self.push_info_toast(format!("Modello inserito: {}", template.name), ToastKind::Info);
    }

    pub(super) fn composer_window(&mut self, ui: &mut Ui) {
        if !self.composer.open {
            return;
        }
        if !self.pgp_keys_loaded && !self.demo {
            self.refresh_pgp_keys();
        }
        self.refresh_templates();
        self.refresh_identities();
        let Some(account) = self.account().cloned() else {
            self.composer_preview = false;
            self.composer = ComposerState::closed();
            return;
        };
        let t = theme::tokens(self.dark);
        let window = ui.ctx().content_rect();
        let body_area = Rect::from_min_max(window.min, pos2(window.max.x, (window.max.y - ui_size::STATUS).max(window.min.y)));
        let (card, _sheet) = composer_card_rect(window, body_area);
        let margin = Vec2::new(f32::from(inset::XL), f32::from(inset::LG));
        let inner = Rect::from_min_size(card.min + margin, (card.size() - margin * 2.0).max(Vec2::ZERO));
        #[cfg(test)]
        probe::composer_card(ui, card);
        let frame = Frame::new()
            .fill(t.panel)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::WINDOW)
            .inner_margin(Margin::symmetric(inset::XL, inset::LG));
        // A modal layer, not a window: the scrim dims the shell for the whole content area, the
        // backdrop swallows every click and no title bar can be dragged over the toolbar.
        let id = Id::new("postino-composer");
        let area = egui::Modal::default_area(id).anchor(Align2::LEFT_TOP, card.min - window.min).constrain(false);
        let smtp_available = self.smtp_available_for(&account.id);
        let attachments_available = !self.demo && self.store.is_some();
        let response = egui::Modal::new(id).area(area).backdrop_color(theme::scrim(self.dark)).frame(frame).show(ui.ctx(), |ui| {
            ui.set_min_size(inner.size());
            ui.set_max_width(inner.width());
            ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
                let mut fetch = |token: &str| -> Vec<crate::ui::widgets::suggest::Suggestion> {
                    self.engine
                        .correspondents(Some(&account.id), token, 6)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|row| {
                            let label = if row.name.is_empty() { row.address.clone() } else { format!("{} <{}>", row.name, row.address) };
                            crate::ui::widgets::suggest::Suggestion { insert: label.clone(), label, kind: "person" }
                        })
                        .collect()
                };
                Composer::new(&mut self.composer, &account)
                    .send_available(smtp_available)
                    .demo(self.demo)
                    .attachments_available(attachments_available)
                    .pgp_keys(&self.pgp_keys)
                    .templates(&self.templates)
                    .identities(&self.identities)
                    .correspondents(&mut fetch)
                    .show(ui)
            })
            .inner
        });
        #[cfg(test)]
        probe::composer_surface(ui, response.response.rect);
        let action = response.inner;
        if action.discard {
            self.discard_composer();
            return;
        }
        if action.close {
            self.close_composer();
            return;
        }
        if let Some(attachment) = action.remove_attachment.clone() {
            self.remove_composer_attachment(&attachment);
        }
        if let Some(id) = action.insert_template.clone() {
            self.insert_template(&id);
        }
        if action.pick_files {
            self.pick_composer_files();
        }
        if !action.dropped_files.is_empty() {
            self.attach_composer_files(action.dropped_files);
        }
        if action.preview {
            self.composer_preview = true;
        }
        if action.send {
            self.send_composer();
        } else if action.save {
            let _ = self.save_composer(true);
        }
        if !self.composer.open {
            ui.ctx().memory_mut(|memory| memory.stop_text_input());
        }
    }
}

/// A failed forward copy. `notice` is the user-facing sentence and `failed_ids` names the
/// composer-local metadata rows so Remove can find them before a store save assigns ids.
#[derive(Default)]
struct ForwardCopy {
    notice: Option<String>,
    failed_ids: Vec<String>,
}

enum OutboxUiAction {
    Cancel(String),
    Retry {
        id: String,
        subject: String,
        uncertain: bool,
    },
    MarkSent {
        id: String,
        subject: String,
    },
    /// Throw away a permanently failed journal row. No delivery happened, so the draft becomes
    /// editable again. Never offered for an uncertain row.
    Discard(String),
}

fn outbox_state_label(state: postino_store::OutboxState) -> (&'static str, BadgeKind) {
    use postino_store::OutboxState;
    match state {
        OutboxState::Queued => ("In coda", BadgeKind::Neutral),
        OutboxState::Sending => ("Invio in corso", BadgeKind::Accent),
        OutboxState::Sent => ("Inviato", BadgeKind::Neutral),
        OutboxState::AppendPending => ("Copia in Inviati", BadgeKind::Accent),
        OutboxState::Uncertain => ("Invio incerto", BadgeKind::Danger),
        OutboxState::Failed => ("Non riuscito", BadgeKind::Danger),
        OutboxState::Cancelled => ("Annullato", BadgeKind::Neutral),
    }
}

fn invite_reply_error(raw: &str) -> String {
    let raw = raw.strip_prefix("invalid arguments: ").unwrap_or(raw);
    if raw.contains("no readable text/calendar") {
        return "Questa email non contiene un invito leggibile.".to_owned();
    }
    if raw.contains("cannot be answered") {
        return "L'invito non è valido o è stato annullato: non si può rispondere.".to_owned();
    }
    if raw.contains("only a valid METHOD:REQUEST") {
        return "Solo una richiesta di partecipazione ricevuta può essere risposta.".to_owned();
    }
    if raw.contains("no organizer") {
        return "Manca l'organizzatore: non c'è a chi rispondere.".to_owned();
    }
    "Non è stato possibile creare la bozza di risposta. Riprova o rispondi con una bozza normale.".to_owned()
}

/// Translate a save failure without losing the actionable limit. A failed forward copy already has a
/// precise sentence and must not be replaced by the generic save line.
pub(super) fn draft_save_error_message(attachment_failure: &Option<String>, raw: &str) -> String {
    if let Some(failure) = attachment_failure {
        return failure.clone();
    }
    let raw = raw.strip_prefix("invalid arguments: ").unwrap_or(raw);
    if raw.contains("subject is longer") {
        return "L'oggetto è troppo lungo (massimo 998 byte): accorcialo prima di salvare.".to_owned();
    }
    if raw.contains("body_text is larger") {
        return "Il corpo è troppo lungo (massimo 1 MiB): accorcia il testo o la citazione prima di salvare.".to_owned();
    }
    if raw.contains("attachments") && raw.contains("at most") {
        return "Gli allegati superano un limite (20 file, 25 MiB per file, 25 MiB totali).".to_owned();
    }
    if raw.contains("in_reply_to") {
        return "Il collegamento alla conversazione non è valido: riapri la risposta dal messaggio originale.".to_owned();
    }
    "Non è stato possibile salvare la bozza. Controlla oggetto, destinatari e allegati.".to_owned()
}

/// Pull the attachment filename out of the engine's send refusal. The filename is already
/// sanitized; a quoted name is shown with its quotes escaped rather than dropped.
fn missing_attachment_name(raw: &str) -> Option<String> {
    let rest = raw.split("attachment \"").nth(1)?;
    let end = rest.find("\" has no stored bytes")?;
    rest.get(..end).map(|name| name.replace("\\\"", "\""))
}

/// The optional `pgp` object on a send. Encryption alone takes no passphrase; a signature adds it
/// only after the sheet collected it. No key ids means a plain send and no `pgp` field at all.
pub(super) fn pgp_send_request(sign: Option<&str>, encrypt: &[String], passphrase: Option<&str>) -> Option<Value> {
    if sign.is_none() && encrypt.is_empty() {
        return None;
    }
    let mut request = json!({ "encrypt": encrypt });
    if let Some(sign) = sign {
        request["sign"] = json!(sign);
        if let Some(passphrase) = passphrase {
            request["passphrase"] = json!(passphrase);
        }
    }
    Some(request)
}

/// Translate an engine send failure into Italian copy. The raw sentence never reaches the shell.
pub(super) fn send_error_message(raw: &str, demo: bool) -> String {
    if demo || raw.contains("demo mailbox") {
        return "La modalità demo non invia messaggi: aggiungi un account reale con un server SMTP.".to_owned();
    }
    if raw.contains("no SMTP endpoint") {
        return "Questo account non ha un server di invio (SMTP): rimuovilo e aggiungilo con i dati SMTP.".to_owned();
    }
    if raw.contains("no stored bytes") || raw.contains("attachments without content") {
        return match missing_attachment_name(raw) {
            Some(name) => format!("Allegato \"{name}\" mancante o illeggibile: rimuovilo o riattaccalo prima di inviare."),
            None => "Un allegato è mancante o illeggibile: rimuovilo o riattaccalo prima di inviare.".to_owned(),
        };
    }
    if raw.contains("at least one recipient") && !raw.contains("recipient key") {
        return "Aggiungi almeno un destinatario prima di inviare.".to_owned();
    }
    if raw.contains("passphrase does not unlock") {
        return "La passphrase non apre la chiave PGP: il messaggio non è stato messo in coda.".to_owned();
    }
    if raw.contains("signing key is passphrase-protected") {
        return "La chiave PGP di firma è protetta: inserisci la passphrase per inviare.".to_owned();
    }
    if raw.contains("signing key has no stored secret") {
        return "La chiave PGP scelta non contiene la parte privata: importala di nuovo.".to_owned();
    }
    if raw.contains("PGP key id") {
        return "La chiave PGP scelta non esiste più per questo account: ricarica le chiavi.".to_owned();
    }
    if raw.contains("recipient key is revoked") {
        return "Una chiave PGP del destinatario è revocata: il messaggio non è stato messo in coda.".to_owned();
    }
    if raw.contains("recipient key is expired") {
        return "Una chiave PGP del destinatario è scaduta: il messaggio non è stato messo in coda.".to_owned();
    }
    if raw.contains("no usable encryption key") {
        return "Una chiave PGP del destinatario non può cifrare: chiedine una con una sottochiave di cifratura.".to_owned();
    }
    if raw.contains("stored PGP key could not be read") {
        return "Una chiave PGP salvata non è leggibile: esportala e importala di nuovo.".to_owned();
    }
    if raw.contains("encrypted send addresses more than 100") {
        return "Un invio cifrato può avere al massimo 100 chiavi.".to_owned();
    }
    if raw.contains("encrypted send needs at least one recipient key") {
        return "Scegli almeno una chiave PGP per cifrare il messaggio.".to_owned();
    }
    if raw.contains("outgoing message has no MIME entity") {
        return "Il messaggio non ha un contenuto MIME da firmare o cifrare.".to_owned();
    }
    if raw.contains("100") && raw.contains("recipients") {
        return "Il messaggio ha troppi destinatari: il limite è 100.".to_owned();
    }
    if raw.contains("failed submission") {
        return "L'ultimo invio di questa bozza è stato rifiutato: scartalo dalla coda e correggi la bozza.".to_owned();
    }
    if raw.contains("already sent") {
        return "Questa bozza è già stata inviata: crea una nuova bozza per inviarla di nuovo.".to_owned();
    }
    if raw.contains("queued") || raw.contains("outbox") {
        return "La bozza è già in coda: annulla l'invio dalla coda oppure attendi l'esito.".to_owned();
    }
    "Non è stato possibile mettere il messaggio in coda. Riprova.".to_owned()
}

/// The composer surface rectangle. A window of at least 1000×700 gets a centred card capped to the
/// content height and never touching the status bar; anything smaller gets a sheet with LG margins
/// over the body area so its controls stay reachable at 980×600.
fn composer_card_rect(window: Rect, body: Rect) -> (Rect, bool) {
    let card = window.width() >= COMPOSER_CARD_MIN_WIDTH && window.height() >= COMPOSER_CARD_MIN_HEIGHT;
    if !card {
        return (body.shrink(space::LG), true);
    }
    let margin = Vec2::new(space::XXL, space::XL);
    let size = Vec2::new(
        ui_size::COMPOSER_CARD_WIDTH.min((body.width() - 2.0 * margin.x).max(0.0)),
        ui_size::COMPOSER_HEIGHT.min((body.height() - 2.0 * margin.y).max(0.0)),
    );
    (Rect::from_center_size(body.center(), size), false)
}

/// Read one attachment with the P8 cap enforced before and during the read. The error never includes
/// the full path: a toast names the file, not the folder the person happens to keep it in.
fn read_attachment_file(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    let metadata = std::fs::metadata(path).map_err(|_| "il file non è leggibile".to_owned())?;
    if !metadata.is_file() {
        return Err("non è un file".to_owned());
    }
    if metadata.len() > postino_core::MAX_ATTACHMENT_BYTES {
        return Err(format!("supera {}", format_size(postino_core::MAX_ATTACHMENT_BYTES)));
    }
    let file = std::fs::File::open(path).map_err(|_| "il file non è leggibile".to_owned())?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(postino_core::MAX_ATTACHMENT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "la lettura del file è fallita".to_owned())?;
    if bytes.len() as u64 > postino_core::MAX_ATTACHMENT_BYTES {
        return Err(format!("supera {}", format_size(postino_core::MAX_ATTACHMENT_BYTES)));
    }
    Ok(bytes)
}
