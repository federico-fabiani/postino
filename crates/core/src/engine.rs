//! The engine owns the mailbox data: in memory for the demo, in the encrypted store when one is
//! attached. The operation surface is identical for both.
//!
//! With a store the engine is **load once, write through**: [`Engine::open`] hydrates the mailbox
//! with one query per table, every mutating operation writes the repository first and only then
//! updates memory, and folder counts are recomputed locally instead of re-read. A failed write
//! leaves memory untouched: caller mistakes are [`OpError::InvalidArgs`] and storage failures are
//! [`OpError::Failed`], and each engine mutation is one store transaction.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use chrono::Utc;
use postino_store::{FlagUpdate, Store};
use serde_json::Value;

use crate::model::{
    Account, Address, Attachment, DraftContent, Folder, FolderKind, Label, LabelColor, LabelSummary, Message, MessageSummary, snippet,
};
use crate::ops::{self, OpDef};
use crate::store_map;

/// A caller-supplied id that does not exist. The engine's pre-checks and the store's `NotFound`
/// both map to this sentence, so a mistake is `InvalidArgs` whichever layer notices it first.
pub(crate) fn unknown_account(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown account {id:?}; use an id returned by accounts_list"))
}

pub(crate) fn unknown_folder(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown folder {id:?}; use an id returned by folders_list"))
}

pub(crate) fn unknown_message(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown message {id:?}; use an id returned by messages_list"))
}

pub(crate) fn unknown_label(id: &str) -> OpError {
    OpError::InvalidArgs(format!("unknown label {id:?}; use an id returned by labels_list"))
}

const MAX_SUBJECT_BYTES: usize = 998;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RECIPIENTS_PER_FIELD: usize = 100;
const MAX_ATTACHMENTS: usize = 50;
const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
const MAX_LABEL_NAME_CHARS: usize = 48;
const MAX_LABEL_MESSAGES: usize = 500;

/// Why an operation failed. The message is meant for a person or an agent to read and act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    #[error("unknown operation {0:?}")]
    UnknownOp(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("{0}")]
    Failed(String),
}

pub struct Engine {
    accounts: Vec<Account>,
    folders: Vec<Folder>,
    messages: Vec<Message>,
    labels: Vec<Label>,
    next_label_seq: u32,
    demo: bool,
    /// `None` for the demo and the empty engine; every mutation is memory-only there.
    store: Option<Store>,
}

impl Engine {
    /// An engine with no accounts. Every list is empty until a store or demo data is attached.
    pub fn empty() -> Self {
        Self { accounts: Vec::new(), folders: Vec::new(), messages: Vec::new(), labels: Vec::new(), next_label_seq: 1, demo: false, store: None }
    }

    /// The M0 walking skeleton: a synthetic account with realistic mail, held in memory only.
    pub fn demo() -> Self {
        crate::demo::seed()
    }

    /// The M0 walking skeleton seeded as if the clock read `now`. A screenshot passes a fixed
    /// instant so the rendered dates never depend on the wall clock.
    pub fn demo_at(now: chrono::DateTime<Utc>) -> Self {
        crate::demo::seed_at(now)
    }

    /// Load the mailbox from `store` and keep it for write-through.
    ///
    /// The load is bounded by the table count, not by the mailbox: accounts, folders, orphan
    /// drafts, message rows with their labels and attachments grouped by message. A draft row
    /// without a message row is repaired into a real message row in one transaction *before* the
    /// mailbox is hydrated; if that repair fails the open fails with an actionable error instead of
    /// serving a draft that cannot be flagged, labelled or moved.
    pub fn open(store: Store) -> Result<Self, OpError> {
        let accounts = store.list_accounts().map_err(store_map::read_error)?.into_iter().map(store_map::account).collect::<Vec<_>>();
        let folders = store.list_folders(None).map_err(store_map::read_error)?.into_iter().map(store_map::folder).collect::<Vec<_>>();
        repair_orphan_drafts(&store, &accounts, &folders)?;
        let mut attachments = store.list_all_attachments().map_err(store_map::read_error)?;
        let messages = store
            .list_all_messages()
            .map_err(store_map::read_error)?
            .into_iter()
            .map(|row| {
                let rows = attachments.remove(&row.summary.id).unwrap_or_default();
                store_map::message(row, rows)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let labels = store.list_labels(None).map_err(store_map::read_error)?.into_iter().map(store_map::label).collect::<Result<Vec<_>, _>>()?;
        let label_seq = repair_label_seq(&store, &labels)?;

        let mut engine = Self::from_parts(accounts, folders, messages, labels, false, Some(store));
        engine.next_label_seq = label_seq.saturating_add(1);
        Ok(engine)
    }

    pub(crate) fn from_parts(
        accounts: Vec<Account>,
        folders: Vec<Folder>,
        messages: Vec<Message>,
        labels: Vec<Label>,
        demo: bool,
        store: Option<Store>,
    ) -> Self {
        let next_label_seq = labels.iter().filter_map(label_number).max().unwrap_or(0).saturating_add(1);
        let mut engine = Self { accounts, folders, messages, labels, next_label_seq, demo, store };
        engine.recount();
        engine
    }

    /// True for the synthetic demo mailbox. Data written by a store-backed engine is real local
    /// data even while no server is connected.
    pub fn is_demo(&self) -> bool {
        self.demo
    }

    /// True when mutations are written to the encrypted store rather than kept in memory.
    pub fn is_persistent(&self) -> bool {
        self.store.is_some()
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn message(&self, id: &str) -> Option<&Message> {
        self.messages.iter().find(|message| message.summary.id == id)
    }

    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    pub fn label(&self, id: &str) -> Option<&Label> {
        self.labels.iter().find(|label| label.id == id)
    }

    /// Labels with mailbox-wide counts, computed in one pass over the messages so the work does
    /// not grow with labels × messages.
    pub fn label_summaries(&self) -> Vec<LabelSummary> {
        let mut counts: HashMap<&str, (u32, u32)> = HashMap::new();
        for message in &self.messages {
            for id in &message.summary.labels {
                let entry = counts.entry(id.as_str()).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(1);
                if message.summary.unread {
                    entry.1 = entry.1.saturating_add(1);
                }
            }
        }
        self.labels
            .iter()
            .map(|label| {
                let (message_count, unread_count) = counts.get(label.id.as_str()).copied().unwrap_or((0, 0));
                LabelSummary { id: label.id.clone(), name: label.name.clone(), color: label.color, message_count, unread_count }
            })
            .collect()
    }

    pub fn label_summary(&self, id: &str) -> Option<LabelSummary> {
        let label = self.label(id)?;
        let (message_count, unread_count) = self
            .messages
            .iter()
            .filter(|message| message.summary.labels.iter().any(|candidate| candidate == id))
            .fold((0u32, 0u32), |(total, unread), message| {
                (total.saturating_add(1), unread.saturating_add(if message.summary.unread { 1 } else { 0 }))
            });
        Some(LabelSummary { id: label.id.clone(), name: label.name.clone(), color: label.color, message_count, unread_count })
    }

    /// Create a local label. The name is trimmed, must be 1-48 characters with no control, bidi
    /// or format characters, and is unique case-insensitively. Ids are never reused.
    pub fn create_label(&mut self, name: &str, color: LabelColor) -> Result<LabelSummary, OpError> {
        let name = validate_label_name(name)?;
        if self.label_name_taken(&name, None) {
            return Err(OpError::InvalidArgs(format!("a label named {name:?} already exists; label names are unique ignoring case")));
        }
        let id = self.next_label_id()?;
        if let Some(store) = &self.store {
            store.create_label(&store_map::label_row(&id, &name, color)).map_err(store_map::write_error)?;
        }
        // Only now is the id spent: a failed write must not leave a hole in the sequence.
        self.next_label_seq = self.next_label_seq.saturating_add(1);
        self.labels.push(Label { id: id.clone(), name, color });
        self.label_summary(&id).ok_or_else(|| OpError::Failed(format!("could not read back the new label {id:?}")))
    }

    /// Rename and/or recolour a label. At least one field is required; the id never changes.
    pub fn update_label(&mut self, id: &str, name: Option<&str>, color: Option<LabelColor>) -> Result<LabelSummary, OpError> {
        if name.is_none() && color.is_none() {
            return Err(OpError::InvalidArgs("provide name and/or color".to_owned()));
        }
        let index = self.labels.iter().position(|label| label.id == id).ok_or_else(|| unknown_label(id))?;
        let new_name = match name {
            Some(name) => {
                let name = validate_label_name(name)?;
                if self.label_name_taken(&name, Some(id)) {
                    return Err(OpError::InvalidArgs(format!("a label named {name:?} already exists; label names are unique ignoring case")));
                }
                Some(name)
            }
            None => None,
        };
        let current = self.labels.get(index).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))?;
        let resolved_name = new_name.clone().unwrap_or_else(|| current.name.clone());
        let resolved_color = color.unwrap_or(current.color);
        if let Some(store) = &self.store {
            store.update_label(id, Some(&resolved_name), Some(resolved_color.as_str())).map_err(store_map::write_error)?;
        }
        let label = self.labels.get_mut(index).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))?;
        label.name = resolved_name;
        label.color = resolved_color;
        self.label_summary(id).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))
    }

    /// Delete a label and detach it from every message. Returns the removed label and the number
    /// of messages that carried it. Mail is never touched.
    pub fn delete_label(&mut self, id: &str) -> Result<(Label, usize), OpError> {
        let index = self.labels.iter().position(|label| label.id == id).ok_or_else(|| unknown_label(id))?;
        if let Some(store) = &self.store {
            store.delete_label(id).map_err(store_map::write_error)?;
        }
        let label = self.labels.remove(index);
        let mut affected = 0usize;
        for message in &mut self.messages {
            let before = message.summary.labels.len();
            message.summary.labels.retain(|candidate| candidate != id);
            if message.summary.labels.len() != before {
                affected = affected.saturating_add(1);
            }
        }
        Ok((label, affected))
    }

    /// Apply or remove one label on a batch of messages. The call is atomic: every message id is
    /// validated before anything changes, and repeating it changes nothing.
    pub fn set_label(&mut self, id: &str, message_ids: &[String], apply: bool) -> Result<(LabelSummary, usize), OpError> {
        if self.label(id).is_none() {
            return Err(unknown_label(id));
        }
        if message_ids.is_empty() {
            return Err(OpError::InvalidArgs("message_ids must contain at least one message id".to_owned()));
        }
        if message_ids.len() > MAX_LABEL_MESSAGES {
            return Err(OpError::InvalidArgs(format!("label_apply accepts at most {MAX_LABEL_MESSAGES} message ids")));
        }
        let ids = message_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
        for message_id in &ids {
            if !self.messages.iter().any(|message| message.summary.id == **message_id) {
                return Err(unknown_message(message_id));
            }
        }
        if let Some(store) = &self.store {
            let batch = ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
            let result = if apply { store.apply_label(id, &batch) } else { store.remove_label(id, &batch) };
            result.map_err(store_map::write_error)?;
        }
        let mut changed = 0usize;
        for message in &mut self.messages {
            if !ids.contains(message.summary.id.as_str()) {
                continue;
            }
            let has = message.summary.labels.iter().any(|candidate| candidate == id);
            if apply && !has {
                message.summary.labels.push(id.to_owned());
                changed = changed.saturating_add(1);
            } else if !apply && has {
                message.summary.labels.retain(|candidate| candidate != id);
                changed = changed.saturating_add(1);
            }
        }
        let summary = self.label_summary(id).ok_or_else(|| OpError::Failed(format!("unknown label {id:?}")))?;
        Ok((summary, changed))
    }

    fn label_name_taken(&self, name: &str, except: Option<&str>) -> bool {
        let name = name.to_lowercase();
        self.labels.iter().filter(|label| except != Some(label.id.as_str())).any(|label| label.name.to_lowercase() == name)
    }

    /// The next free label id. The sequence is only advanced once the label is stored, so a failed
    /// write does not burn an id.
    fn next_label_id(&self) -> Result<String, OpError> {
        if self.next_label_seq == u32::MAX {
            return Err(OpError::Failed("could not allocate a new label id".to_owned()));
        }
        let id = format!("lbl_{}", self.next_label_seq);
        if self.labels.iter().any(|label| label.id == id) {
            return Err(OpError::Failed(format!("could not allocate a new label id: {id:?} is already taken")));
        }
        Ok(id)
    }

    /// Change read/starred flags. At least one flag must be present.
    pub fn set_flags(&mut self, id: &str, unread: Option<bool>, starred: Option<bool>) -> Result<MessageSummary, OpError> {
        if unread.is_none() && starred.is_none() {
            return Err(OpError::InvalidArgs("provide unread and/or starred".to_owned()));
        }
        if !self.messages.iter().any(|message| message.summary.id == id) {
            return Err(unknown_message(id));
        }
        if let Some(store) = &self.store {
            store.set_flags(&[id.to_owned()], FlagUpdate { unread, starred }).map_err(store_map::write_error)?;
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.summary.id == id) else {
            return Err(OpError::Failed(format!("unknown message {id:?}")));
        };
        if let Some(unread) = unread {
            message.summary.unread = unread;
        }
        if let Some(starred) = starred {
            message.summary.starred = starred;
        }
        let summary = message.summary.clone();
        self.recount();
        Ok(summary)
    }

    /// Move one message to a regular folder. Virtual folders such as `starred` are views, not
    /// destinations.
    pub fn move_message(&mut self, id: &str, folder_id: &str) -> Result<MessageSummary, OpError> {
        let target = self.folders.iter().find(|folder| folder.id == folder_id).ok_or_else(|| unknown_folder(folder_id))?;
        if target.kind.is_virtual() {
            return Err(OpError::InvalidArgs(format!("folder {folder_id:?} is virtual and cannot receive messages")));
        }
        let target_id = target.id.clone();
        if !self.messages.iter().any(|message| message.summary.id == id) {
            return Err(unknown_message(id));
        }
        if let Some(store) = &self.store {
            store.move_messages(&[id.to_owned()], &target_id).map_err(store_map::write_error)?;
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.summary.id == id) else {
            return Err(OpError::Failed(format!("unknown message {id:?}")));
        };
        message.summary.folder_id = target_id;
        let summary = message.summary.clone();
        self.recount();
        Ok(summary)
    }

    /// Create or update a draft in the Drafts folder, replacing every content field.
    ///
    /// `account` chooses the account on create: it may be omitted when the mailbox has exactly one
    /// account and is required when several exist. An update keeps the draft's own account and
    /// folder and rejects an explicit `account` that disagrees. An empty recipient list is allowed
    /// while composing; sending validates it separately. Header injection is refused here, before
    /// any state changes.
    ///
    /// With a store the message, attachments, revision and folder counts are one transaction. A
    /// failure returns before memory changes and leaves the previous rows exactly as they were.
    pub fn save_draft(&mut self, account: Option<&str>, id: Option<&str>, draft: DraftContent) -> Result<Message, OpError> {
        validate_draft(&draft)?;
        let index = match id {
            Some(id) => {
                if id.trim().is_empty() {
                    return Err(OpError::InvalidArgs("draft id must not be empty".to_owned()));
                }
                let index = self
                    .messages
                    .iter()
                    .position(|message| message.summary.id == id)
                    .ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}; omit id to create a draft")))?;
                let kind = self.folders.iter().find(|folder| folder.id == self.messages[index].summary.folder_id).map(|folder| folder.kind);
                if kind != Some(FolderKind::Drafts) {
                    return Err(OpError::InvalidArgs(format!("message {id:?} is not a draft")));
                }
                Some(index)
            }
            None => None,
        };
        let current_folder = index.and_then(|index| self.messages.get(index)).map(|message| message.summary.folder_id.as_str());
        let current_account =
            current_folder.and_then(|folder_id| self.folders.iter().find(|folder| folder.id == folder_id)).map(|folder| folder.account_id.clone());
        let account = match (index, current_account) {
            (Some(_), Some(account_id)) => {
                if let Some(requested) = account
                    && requested != account_id
                {
                    return Err(OpError::InvalidArgs(format!(
                        "draft {id:?} belongs to account {account_id:?}; omit account or pass {account_id:?} to update it"
                    )));
                }
                self.accounts
                    .iter()
                    .find(|candidate| candidate.id == account_id)
                    .cloned()
                    .ok_or_else(|| OpError::Failed(format!("the local store has draft {id:?} in a folder for unknown account {account_id:?}")))?
            }
            (Some(_), None) => {
                return Err(OpError::Failed(format!("the local store has draft {id:?} in an unknown folder")));
            }
            _ => self.draft_account(account)?,
        };
        let draft_folder = match index.and_then(|index| self.messages.get(index)) {
            Some(current) => current.summary.folder_id.clone(),
            None => self
                .folders
                .iter()
                .find(|folder| folder.account_id == account.id && folder.kind == FolderKind::Drafts)
                .map(|folder| folder.id.clone())
                .ok_or_else(|| OpError::Failed(format!("account {:?} has no Drafts folder", account.id)))?,
        };

        let message_id = match (index, id) {
            (Some(_), Some(id)) => id.to_owned(),
            _ => self.next_id("draft")?,
        };
        let attachments = draft
            .attachments
            .into_iter()
            .enumerate()
            .map(|(position, mut attachment)| {
                if attachment.id.trim().is_empty() {
                    attachment.id = format!("{message_id}_att_{position}");
                }
                attachment
            })
            .collect::<Vec<_>>();
        validate_unique_attachment_ids(&attachments)?;
        let has_attachments = !attachments.is_empty();
        let summary_snippet = snippet(&draft.body_text);
        // An update keeps what the draft form does not own: its thread, starred flag, labels and
        // HTML body. The composer patches `body_text`; a subject-only save must not destroy the
        // stored HTML part.
        let (thread_id, starred, labels, body_html) = match index.and_then(|index| self.messages.get(index)) {
            Some(current) => (current.summary.thread_id.clone(), current.summary.starred, current.summary.labels.clone(), current.body_html.clone()),
            None => (format!("thr_{message_id}"), false, Vec::new(), None),
        };
        let saved = Message {
            summary: MessageSummary {
                id: message_id.clone(),
                thread_id,
                folder_id: draft_folder,
                from: Address::new(account.name.clone(), account.email.clone()),
                to: draft.to,
                subject: draft.subject,
                snippet: summary_snippet,
                date: Utc::now(),
                unread: false,
                starred,
                has_attachments,
                labels,
            },
            cc: draft.cc,
            bcc: draft.bcc,
            body_text: draft.body_text,
            body_html,
            attachments,
        };

        if let Some(store) = &self.store {
            let row = store_map::message_row(&saved, &account.id);
            store.save_draft_message(&row, &store_map::attachment_rows(&saved.attachments)).map_err(store_map::write_error)?;
        }

        match index {
            Some(index) => {
                let Some(message) = self.messages.get_mut(index) else {
                    return Err(OpError::Failed(format!("unknown draft {message_id:?}")));
                };
                *message = saved.clone();
            }
            None => self.messages.push(saved.clone()),
        }
        self.recount();
        Ok(saved)
    }

    /// Pick the account for a new draft. One account is implied; several require an explicit id.
    fn draft_account(&self, requested: Option<&str>) -> Result<Account, OpError> {
        if let Some(requested) = requested {
            return self
                .accounts
                .iter()
                .find(|account| account.id == requested)
                .cloned()
                .ok_or_else(|| OpError::InvalidArgs(format!("unknown account {requested:?}; run accounts_list to see the configured ids")));
        }
        match self.accounts.as_slice() {
            [] => Err(OpError::Failed("no account is configured".to_owned())),
            [account] => Ok(account.clone()),
            accounts => Err(OpError::InvalidArgs(format!(
                "the mailbox has {} accounts; pass `account` to draft_save to choose where the draft lives (for example {:?})",
                accounts.len(),
                accounts.first().map(|account| account.id.as_str()).unwrap_or_default()
            ))),
        }
    }

    /// Local submission: move a draft to the Sent folder of its own account and record the current
    /// timestamp. No SMTP connection is opened in this build; the operation description and the UI
    /// say the move is local and that real submission arrives with M2. A store-backed engine
    /// persists the move, and keeps the draft revision sequence for a later move back.
    pub fn send_draft(&mut self, id: &str) -> Result<MessageSummary, OpError> {
        let index =
            self.messages.iter().position(|message| message.summary.id == id).ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}")))?;
        let Some(current) = self.messages.get(index) else {
            return Err(OpError::InvalidArgs(format!("unknown draft {id:?}")));
        };
        let folder = self
            .folders
            .iter()
            .find(|folder| folder.id == current.summary.folder_id)
            .ok_or_else(|| OpError::Failed(format!("the draft {id:?} lives in unknown folder {:?}", current.summary.folder_id)))?;
        if folder.kind != FolderKind::Drafts {
            return Err(OpError::InvalidArgs(format!("message {id:?} is not a draft")));
        }
        if current.summary.to.is_empty() && current.cc.is_empty() && current.bcc.is_empty() {
            return Err(OpError::InvalidArgs("a message needs at least one recipient".to_owned()));
        }
        validate_addresses("to", &current.summary.to)?;
        validate_addresses("cc", &current.cc)?;
        validate_addresses("bcc", &current.bcc)?;
        validate_header("subject", &current.summary.subject, MAX_SUBJECT_BYTES)?;
        let account_id = folder.account_id.clone();
        let sent = self
            .folders
            .iter()
            .find(|folder| folder.account_id == account_id && folder.kind == FolderKind::Sent)
            .map(|folder| folder.id.clone())
            .ok_or_else(|| OpError::Failed(format!("account {account_id:?} has no Sent folder")))?;

        let mut message = current.clone();
        message.summary.folder_id = sent;
        message.summary.date = Utc::now();
        message.summary.unread = false;
        message.summary.starred = false;
        message.summary.snippet = snippet(&message.body_text);
        message.summary.has_attachments = !message.attachments.is_empty();

        if let Some(store) = &self.store {
            // One transaction: move the display row to Sent and drop the draft row. `draft_seq` is
            // deliberately kept, so a move back to Drafts continues the revision sequence.
            let row = store_map::message_row(&message, &account_id);
            store.send_draft_message(&row).map_err(store_map::write_error)?;
        }

        let Some(slot) = self.messages.get_mut(index) else {
            return Err(OpError::Failed(format!("unknown draft {id:?}")));
        };
        *slot = message;
        let summary =
            self.messages.get(index).map(|message| message.summary.clone()).ok_or_else(|| OpError::Failed(format!("unknown draft {id:?}")))?;
        self.recount();
        Ok(summary)
    }

    /// Call any operation by name with JSON arguments. This is the only door a frontend uses.
    pub fn call(&mut self, name: &str, args: Value) -> Result<Value, OpError> {
        ops::dispatch(self, name, args)
    }

    /// The operation catalogue, with JSON Schemas. CLI and MCP share it.
    pub fn tools(&self) -> Vec<OpDef> {
        ops::op_defs()
    }

    /// The platform data directory. The CLI's `--store` path is explicit until onboarding (P6)
    /// chooses the default location; this only reports where Postino keeps its data.
    pub fn data_dir() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().join("Postino"))
    }

    fn next_id(&self, prefix: &str) -> Result<String, OpError> {
        let base = self.messages.len().saturating_add(1);
        for offset in 0..10_000 {
            let candidate = format!("{prefix}_{}", base.saturating_add(offset));
            if !self.messages.iter().any(|message| message.summary.id == candidate) {
                return Ok(candidate);
            }
        }
        Err(OpError::Failed(format!("could not allocate a new {prefix} id")))
    }

    pub(crate) fn recount(&mut self) {
        for folder in &mut self.folders {
            let (total, unread) = if folder.kind == FolderKind::Starred {
                self.messages.iter().filter(|message| message.summary.starred).fold((0u32, 0u32), |(total, unread), message| {
                    (total.saturating_add(1), unread.saturating_add(if message.summary.unread { 1 } else { 0 }))
                })
            } else {
                self.messages.iter().filter(|message| message.summary.folder_id == folder.id).fold((0u32, 0u32), |(total, unread), message| {
                    (total.saturating_add(1), unread.saturating_add(if message.summary.unread { 1 } else { 0 }))
                })
            };
            folder.total = total;
            folder.unread = unread;
        }
    }
}

/// Materialise every `drafts` row that has no display row, in one store transaction, before the
/// mailbox is hydrated. Unknown accounts/folders and unreadable dates fail the open with the reason.
fn repair_orphan_drafts(store: &Store, accounts: &[Account], folders: &[Folder]) -> Result<(), OpError> {
    let drafts = store.list_orphan_drafts().map_err(store_map::read_error)?;
    if drafts.is_empty() {
        return Ok(());
    }
    let mut messages = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let account = accounts
            .iter()
            .find(|account| account.id == draft.account_id)
            .ok_or_else(|| OpError::Failed(format!("the local store has draft {:?} for unknown account {:?}", draft.id, draft.account_id)))?;
        let folder_id = folders
            .iter()
            .find(|folder| folder.account_id == draft.account_id && folder.kind == FolderKind::Drafts)
            .map(|folder| folder.id.clone())
            .ok_or_else(|| {
                OpError::Failed(format!("the local store has draft {:?} but no Drafts folder for account {:?}", draft.id, draft.account_id))
            })?;
        let message = store_map::draft_message(&draft, account, &folder_id)?;
        messages.push(store_map::message_row(&message, &account.id));
    }
    store.repair_orphan_drafts(&messages).map_err(store_map::repair_error)?;
    Ok(())
}

/// Read the persisted label high-water mark, raise it to whatever labels already exist and repair an
/// older store that has no `label_seq` row. The returned number is the highest id ever allocated.
fn repair_label_seq(store: &Store, labels: &[Label]) -> Result<u32, OpError> {
    let raw = store.get_meta(postino_store::LABEL_SEQ_KEY).map_err(store_map::read_error)?;
    let stored = raw.as_deref().and_then(|value| value.parse::<u32>().ok()).unwrap_or(0);
    let existing = labels.iter().filter_map(label_number).max().unwrap_or(0);
    let high = stored.max(existing);
    if raw.as_deref().and_then(|value| value.parse::<u32>().ok()) != Some(high) {
        store.set_meta(postino_store::LABEL_SEQ_KEY, &high.to_string()).map_err(store_map::read_error)?;
    }
    Ok(high)
}

fn label_number(label: &Label) -> Option<u32> {
    label.id.strip_prefix("lbl_").and_then(|suffix| suffix.parse::<u32>().ok())
}

fn validate_label_name(name: &str) -> Result<String, OpError> {
    let name = name.trim();
    let characters = name.chars().count();
    if characters == 0 {
        return Err(OpError::InvalidArgs("a label name must have 1-48 characters after trimming".to_owned()));
    }
    if characters > MAX_LABEL_NAME_CHARS {
        return Err(OpError::InvalidArgs(format!("a label name must have 1-{MAX_LABEL_NAME_CHARS} characters after trimming")));
    }
    if name.chars().any(char::is_control) {
        return Err(OpError::InvalidArgs("a label name must not contain control characters".to_owned()));
    }
    if name.chars().any(is_bidi_or_format_control) {
        return Err(OpError::InvalidArgs(
            "a label name must not contain bidi or format control characters (U+061C, U+200B-U+200F, U+202A-U+202E, U+2060-U+206F, U+FEFF)"
                .to_owned(),
        ));
    }
    Ok(name.to_owned())
}

fn is_bidi_or_format_control(character: char) -> bool {
    matches!(character, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

fn validate_draft(draft: &DraftContent) -> Result<(), OpError> {
    validate_addresses("to", &draft.to)?;
    validate_addresses("cc", &draft.cc)?;
    validate_addresses("bcc", &draft.bcc)?;
    validate_header("subject", &draft.subject, MAX_SUBJECT_BYTES)?;
    if draft.body_text.len() > MAX_BODY_BYTES {
        return Err(OpError::InvalidArgs(format!("body_text is larger than {MAX_BODY_BYTES} bytes")));
    }
    validate_attachments(&draft.attachments)
}

fn validate_addresses(field: &str, addresses: &[Address]) -> Result<(), OpError> {
    if addresses.len() > MAX_RECIPIENTS_PER_FIELD {
        return Err(OpError::InvalidArgs(format!("{field} has more than {MAX_RECIPIENTS_PER_FIELD} recipients")));
    }
    for address in addresses {
        validate_header("recipient name", &address.name, 256)?;
        validate_header("recipient email", &address.email, 320)?;
        let email = address.email.trim();
        let local = email.split_once('@').map(|(local, _)| local).unwrap_or("");
        let domain = email.split_once('@').map(|(_, domain)| domain).unwrap_or("");
        if local.is_empty() || domain.is_empty() || email.chars().any(char::is_whitespace) || email.contains(['<', '>', ',', ';']) {
            return Err(OpError::InvalidArgs(format!("{field} contains an invalid email address: {:?}", address.email)));
        }
    }
    Ok(())
}

fn validate_header(field: &str, value: &str, max_bytes: usize) -> Result<(), OpError> {
    if value.chars().any(|character| character.is_control() && character != '\t') {
        return Err(OpError::InvalidArgs(format!("{field} must not contain line breaks or control characters")));
    }
    if value.len() > max_bytes {
        return Err(OpError::InvalidArgs(format!("{field} is longer than {max_bytes} bytes")));
    }
    Ok(())
}

fn validate_unique_attachment_ids(attachments: &[Attachment]) -> Result<(), OpError> {
    let mut seen = BTreeSet::new();
    for attachment in attachments {
        if !seen.insert(attachment.id.as_str()) {
            return Err(OpError::InvalidArgs(format!(
                "attachment id {:?} is repeated in the draft; give every attachment a unique id",
                attachment.id
            )));
        }
    }
    Ok(())
}

fn validate_attachments(attachments: &[Attachment]) -> Result<(), OpError> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(OpError::InvalidArgs(format!("a draft can carry at most {MAX_ATTACHMENTS} attachments")));
    }
    for attachment in attachments {
        validate_header("attachment id", &attachment.id, 128)?;
        validate_header("attachment filename", &attachment.filename, 255)?;
        validate_header("attachment type", &attachment.mime, 127)?;
        if attachment.filename.trim().is_empty() || attachment.filename.contains(['/', '\\', ':']) {
            return Err(OpError::InvalidArgs("attachment filenames must be a plain file name".to_owned()));
        }
        if attachment.size > MAX_ATTACHMENT_BYTES {
            return Err(OpError::InvalidArgs(format!("attachment {:?} is larger than {MAX_ATTACHMENT_BYTES} bytes", attachment.filename)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use serde_json::json;

    use super::*;

    #[test]
    fn demo_engine_has_accounts_and_mail() {
        let engine = Engine::demo();
        assert!(engine.is_demo());
        assert_eq!(engine.accounts().len(), 1);
        assert!(engine.messages().len() >= 10);
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Inbox));
    }

    #[test]
    fn demo_dates_are_relative_to_the_requested_instant() {
        let now = DateTime::parse_from_rfc3339("2026-10-24T10:42:00Z").expect("fixture").with_timezone(&Utc);
        let engine = Engine::demo_at(now);
        assert_eq!(engine.messages()[0].summary.date, now - chrono::Duration::minutes(8));
        assert!(engine.messages().iter().all(|message| message.summary.date < now));
    }

    #[test]
    fn flags_update_folder_counts() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].summary.id.clone();
        let before = engine.folders().iter().find(|f| f.id == "inbox").map_or(0, |f| f.unread);
        let was_unread = engine.message(&id).is_some_and(|m| m.summary.unread);
        let summary = engine.set_flags(&id, Some(!was_unread), None).expect("flag");
        assert_eq!(summary.unread, !was_unread);
        let after = engine.folders().iter().find(|f| f.id == "inbox").map_or(0, |f| f.unread);
        assert_eq!(after, if was_unread { before.saturating_sub(1) } else { before.saturating_add(1) });
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        let mut engine = Engine::demo();
        let error = engine.call("messages_list", json!({ "nope": true })).expect_err("unknown key");
        assert!(matches!(error, OpError::InvalidArgs(_)));
    }

    #[test]
    fn unknown_operation_is_reported() {
        let mut engine = Engine::demo();
        assert!(matches!(engine.call("mail_steal", json!({})), Err(OpError::UnknownOp(_))));
    }

    fn draft(subject: &str, body: &str) -> DraftContent {
        DraftContent {
            to: vec![Address::new("Giulia Ferri", "giulia.ferri@example.com")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.to_owned(),
            body_text: body.to_owned(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn saving_twice_updates_the_same_draft_and_the_folder_count() {
        let mut engine = Engine::demo();
        let before = engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        let first = engine.save_draft(None, None, draft("Ciao", "Prima versione")).expect("new draft");
        assert_eq!(first.summary.folder_id, "drafts");
        let after_first = engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        assert_eq!(after_first, before.saturating_add(1));

        let mut changed = draft("Ciao", "Seconda versione");
        changed.cc.push(Address::new("Marco Bianchi", "marco.bianchi@example.com"));
        let second = engine.save_draft(None, Some(&first.summary.id), changed).expect("update");
        assert_eq!(second.summary.id, first.summary.id);
        assert_eq!(second.body_text, "Seconda versione");
        assert_eq!(second.cc.len(), 1);
        let after_second = engine.folders().iter().find(|folder| folder.kind == FolderKind::Drafts).map_or(0, |folder| folder.total);
        assert_eq!(after_second, after_first, "an update must not create a second draft");
    }

    #[test]
    fn sending_a_draft_moves_it_to_sent_and_undo_can_move_it_back() {
        let mut engine = Engine::demo();
        let draft = engine.save_draft(None, None, draft("Ciao", "Corpo")).expect("draft");
        let id = draft.summary.id;
        let sent = engine.send_draft(&id).expect("send");
        assert_eq!(sent.folder_id, "sent");
        assert!(!sent.unread);
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Sent && folder.total >= 1));
        engine.move_message(&id, "drafts").expect("undo move");
        assert!(engine.folders().iter().any(|folder| folder.kind == FolderKind::Drafts && folder.total >= 1));
    }

    #[test]
    fn drafts_refuse_header_injection_and_invalid_addresses() {
        let mut engine = Engine::demo();
        let mut injected = draft("Ciao", "Corpo");
        injected.subject = "Ciao\nBcc: attacker@example.com".to_owned();
        assert!(matches!(engine.save_draft(None, None, injected), Err(OpError::InvalidArgs(_))));

        let mut bad_address = draft("Ciao", "Corpo");
        bad_address.to = vec![Address::new("Giulia", "not-an-address")];
        assert!(matches!(engine.save_draft(None, None, bad_address), Err(OpError::InvalidArgs(_))));
    }

    #[test]
    fn an_empty_draft_cannot_be_sent() {
        let mut engine = Engine::demo();
        let mut empty = draft("", "");
        empty.to.clear();
        let draft = engine.save_draft(None, None, empty).expect("draft");
        assert!(matches!(engine.send_draft(&draft.summary.id), Err(OpError::InvalidArgs(_))));
    }

    #[test]
    fn a_sent_message_is_not_a_draft_any_more() {
        let mut engine = Engine::demo();
        let draft = engine.save_draft(None, None, draft("Ciao", "Corpo")).expect("draft");
        let id = draft.summary.id;
        engine.send_draft(&id).expect("send");
        assert!(matches!(engine.send_draft(&id), Err(OpError::InvalidArgs(_))));
    }

    #[test]
    fn deleting_the_highest_label_does_not_reuse_its_id() {
        let mut engine = Engine::demo();
        engine.delete_label("lbl_3").expect("delete the highest label");
        let created = engine.create_label("Nuova", LabelColor::Forest).expect("create after delete");
        assert_eq!(created.id, "lbl_4", "a deleted id must never come back");
        let next = engine.create_label("Altra", LabelColor::Muted).expect("second create");
        assert_eq!(next.id, "lbl_5");
        assert!(engine.delete_label("lbl_1").is_ok());
        let after_low_delete = engine.create_label("Terza", LabelColor::Gold).expect("create after low delete");
        assert_eq!(after_low_delete.id, "lbl_6", "the sequence also ignores deleted low ids");
    }

    #[test]
    fn label_summaries_count_each_label_in_one_pass() {
        let mut engine = Engine::demo();
        engine.set_flags("msg_001", Some(false), None).expect("mark read");
        engine.set_label("lbl_2", &["msg_004".to_owned(), "msg_005".to_owned()], true).expect("apply Personale");

        let summaries = engine.label_summaries();
        let lavoro = summaries.iter().find(|label| label.id == "lbl_1").expect("Lavoro");
        assert_eq!((lavoro.message_count, lavoro.unread_count), (6, 1));
        let personale = summaries.iter().find(|label| label.id == "lbl_2").expect("Personale");
        assert_eq!((personale.message_count, personale.unread_count), (4, 1));
        let fatture = summaries.iter().find(|label| label.id == "lbl_3").expect("Fatture");
        assert_eq!((fatture.message_count, fatture.unread_count), (1, 1), "a message can feed two labels");
    }

    #[test]
    fn label_names_reject_bidi_controls_and_keep_accents() {
        let mut engine = Engine::demo();
        assert!(matches!(engine.create_label("A\u{202e}B", LabelColor::Gold), Err(OpError::InvalidArgs(_))));
        assert!(matches!(engine.create_label("Zero\u{200b}width", LabelColor::Gold), Err(OpError::InvalidArgs(_))));
        let accented = engine.create_label("Città", LabelColor::Gold).expect("accented names are ordinary text");
        assert_eq!(accented.name, "Città");
        let emoji = engine.create_label("Casa 🏡", LabelColor::Gold).expect("emoji are allowed");
        assert_eq!(emoji.name, "Casa 🏡");
    }
}
