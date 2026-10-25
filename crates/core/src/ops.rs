//! The operation registry. One typed request per operation, one JSON Schema, one handler.
//!
//! Names are `snake_case` because MCP clients reject dots. Unknown argument keys are refused, so
//! a typo fails loudly instead of being silently ignored.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::engine::{Engine, OpError};
use crate::model::{Account, Address, Attachment, DraftContent, Folder, FolderKind, Label, LabelColor, LabelSummary, Message, MessageSummary};

/// One operation as the CLI and MCP see it.
#[derive(Debug, Clone, Serialize)]
pub struct OpDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub read_only: bool,
    pub destructive: bool,
}

macro_rules! def {
    ($name:literal, $title:literal, $desc:literal, $args:ty, $ro:literal, $destructive:literal) => {
        OpDef {
            name: $name,
            title: $title,
            description: $desc,
            input_schema: serde_json::to_value(schemars::schema_for!($args)).unwrap_or(Value::Null),
            read_only: $ro,
            destructive: $destructive,
        }
    };
}

/// Every operation this build exposes.
pub fn op_defs() -> Vec<OpDef> {
    vec![
        def!(
            "app_info",
            "Application info",
            "Version, platform, data directory, whether the engine is running on synthetic demo data and whether it is backed by the encrypted local store.",
            AppInfoArgs,
            true,
            false
        ),
        def!(
            "accounts_list",
            "List accounts",
            "The configured mail accounts. The demo engine has one synthetic account; a store engine returns what onboarding saved.",
            AccountsListArgs,
            true,
            false
        ),
        def!("folders_list", "List folders", "Folders for an account, with unread and total counts.", FoldersListArgs, true, false),
        def!(
            "messages_list",
            "List messages",
            "Message summaries, newest first. `label` alone spans every folder; with `folder` it returns the intersection; with neither it defaults to `inbox`. Unknown folder or label ids are an error. Bodies are fetched separately with message_get.",
            MessagesListArgs,
            true,
            false
        ),
        def!("message_get", "Get a message", "One message with its body, recipients and attachments.", MessageGetArgs, true, false),
        def!(
            "message_flag",
            "Change message flags",
            "Set the read/unread or starred state of one message. At least one flag is required.",
            MessageFlagArgs,
            false,
            false
        ),
        def!(
            "message_move",
            "Move a message",
            "Move one message to a regular folder, for example `archive` or `trash`. Virtual folders such as `starred` cannot receive messages.",
            MessageMoveArgs,
            false,
            false
        ),
        def!(
            "draft_save",
            "Save a draft",
            "Create or patch a draft in Drafts. Omit `id` to create one: absent content fields are empty. With `id`, an absent field keeps its current value and a present field replaces it, including an empty list or empty string. `account` chooses the account on create; it is required when several accounts exist.",
            DraftSaveArgs,
            false,
            false
        ),
        def!(
            "draft_send",
            "Move a draft to Sent (local)",
            "Move a draft to the Sent folder of its own account and record the current timestamp. No SMTP connection is opened in this build: the move is local and durable on a store, and real submission arrives with M2. At least one To, Cc or Bcc recipient is required.",
            DraftSendArgs,
            false,
            false
        ),
        def!(
            "search",
            "Search mail",
            "Case-insensitive search across subjects, people, snippets and bodies of the local mailbox, demo or store-backed. Optional `folder` and `label` filters are applied before the row limit; unknown ids are an error.",
            SearchArgs,
            true,
            false
        ),
        def!(
            "labels_list",
            "List labels",
            "Every local label with its mailbox-wide total and unread message counts. Labels are local-only metadata: persisted in the encrypted store when one is attached, never synced to a server.",
            LabelsListArgs,
            true,
            false
        ),
        def!(
            "label_create",
            "Create a label",
            "Create a local label with a semantic colour: gold, forest, success, danger or muted. Names are trimmed, 1-48 characters, free of control, bidi and format characters, and unique ignoring case. Ids are never reused. Labels are local-only metadata.",
            LabelCreateArgs,
            false,
            false
        ),
        def!(
            "label_update",
            "Rename or recolour a label",
            "Change a label's name and/or semantic colour. Provide at least one field; the name follows label_create validation and the id never changes. Labels are local-only metadata.",
            LabelUpdateArgs,
            false,
            false
        ),
        def!(
            "label_delete",
            "Delete a label",
            "Delete a label and detach it from every message that carries it. Returns the deleted label and the number of affected messages. This removes metadata only, never mail; labels are local-only.",
            LabelDeleteArgs,
            false,
            false
        ),
        def!(
            "label_apply",
            "Apply or remove a label",
            "Apply one label to `message_ids` when `apply` is true, or remove it when false. Accepts 1-500 message ids, is idempotent, and rejects unknown labels or messages before changing anything. Labels are local metadata: kept in the encrypted store when one is attached, never synced to a server.",
            LabelApplyArgs,
            false,
            false
        ),
    ]
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppInfoArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountsListArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FoldersListArgs {
    /// Account id; omit for every account.
    pub account: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessagesListArgs {
    /// Folder id; `starred` is the virtual starred folder. With `label` and no folder, every
    /// folder is searched; with neither, this defaults to `inbox`.
    pub folder: Option<String>,
    /// Account id; omit for every account.
    pub account: Option<String>,
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: bool,
    /// Only starred messages.
    #[serde(default)]
    pub starred_only: bool,
    /// Only messages with at least one attachment.
    #[serde(default)]
    pub with_attachments: bool,
    /// Only messages carrying this label id. Unknown label ids are an error.
    pub label: Option<String>,
    /// Page size, 1-200. Defaults to 50.
    pub limit: Option<u32>,
    /// Rows to skip before the page. Defaults to 0.
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageGetArgs {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageFlagArgs {
    pub id: String,
    /// New read state: true means unread.
    pub unread: Option<bool>,
    pub starred: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageMoveArgs {
    pub id: String,
    /// Destination folder id. `starred` is virtual and is refused.
    pub folder: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftSaveArgs {
    /// Existing draft id returned by a previous draft_save. Omit to create a draft.
    pub id: Option<String>,
    /// Account id for a new draft. Required when the mailbox has more than one account; an update
    /// keeps the draft's own account.
    pub account: Option<String>,
    /// Primary recipients. Omit to keep the current list on update; an empty list clears it.
    pub to: Option<Vec<Address>>,
    /// Cc recipients. Omit to keep the current list on update; an empty list clears it.
    pub cc: Option<Vec<Address>>,
    /// Bcc recipients. Omit to keep the current list on update; an empty list clears it.
    pub bcc: Option<Vec<Address>>,
    /// Subject. Omit to keep the current subject on update; an empty string clears it.
    pub subject: Option<String>,
    /// Body text. Omit to keep the current body on update; an empty string clears it.
    pub body_text: Option<String>,
    /// Attachment metadata. Omit to keep the current attachments on update; an empty list clears
    /// them. M0 stores the name, MIME type and size; the bytes arrive in M2.
    pub attachments: Option<Vec<DraftAttachment>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachment {
    /// Stable id. Omit it and the engine assigns one when the draft is saved.
    pub id: Option<String>,
    pub filename: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftSendArgs {
    /// Draft id returned by draft_save.
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Words to look for. Empty queries are refused.
    pub query: String,
    /// Restrict to one folder id.
    pub folder: Option<String>,
    /// Restrict to messages carrying this label id. Unknown label ids are an error.
    pub label: Option<String>,
    /// Maximum matches, 1-200. Defaults to 50. Applied after the folder and label filters.
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelsListArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelCreateArgs {
    /// 1-48 characters after trimming, no control, bidi or format characters, unique ignoring case.
    pub name: String,
    /// Semantic colour: gold, forest, success, danger or muted.
    pub color: LabelColor,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelUpdateArgs {
    /// Stable label id returned by label_create or labels_list.
    pub id: String,
    /// New name. Omit to keep the current name.
    pub name: Option<String>,
    /// New semantic colour. Omit to keep the current colour.
    pub color: Option<LabelColor>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelDeleteArgs {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelApplyArgs {
    /// Label id returned by labels_list.
    pub id: String,
    /// One or more message ids, at most 500 per call.
    pub message_ids: Vec<String>,
    /// True applies the label; false removes it. The operation is idempotent.
    pub apply: bool,
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub platform: String,
    pub data_dir: Option<String>,
    /// Synthetic demo data, held in memory only.
    pub demo: bool,
    /// The engine is backed by the encrypted local store, so mutations survive a restart.
    pub store: bool,
    pub operations: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsList {
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldersList {
    pub folders: Vec<Folder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesList {
    /// The folder id when the list is folder-scoped, or null when a label spans every folder.
    pub folder: Option<String>,
    pub messages: Vec<MessageSummary>,
    pub total: usize,
    pub unread: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageGet {
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageFlag {
    pub message: MessageSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMove {
    pub message: MessageSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftSave {
    pub draft: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftSend {
    pub message: MessageSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub query: String,
    pub messages: Vec<MessageSummary>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelsList {
    pub labels: Vec<LabelSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelCreate {
    pub label: LabelSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelUpdate {
    pub label: LabelSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelDelete {
    pub label: Label,
    pub affected: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelApply {
    pub label: LabelSummary,
    /// Messages whose label set actually changed. Repeating the same call returns 0.
    pub changed: usize,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn dispatch(engine: &mut Engine, name: &str, args: Value) -> Result<Value, OpError> {
    match name {
        "app_info" => {
            let _: AppInfoArgs = parse(args)?;
            value(app_info(engine))
        }
        "accounts_list" => {
            let _: AccountsListArgs = parse(args)?;
            value(AccountsList { accounts: engine.accounts().to_vec() })
        }
        "folders_list" => {
            let args: FoldersListArgs = parse(args)?;
            folders(engine, args)
        }
        "messages_list" => {
            let args: MessagesListArgs = parse(args)?;
            messages(engine, args)
        }
        "message_get" => {
            let args: MessageGetArgs = parse(args)?;
            message(engine, args)
        }
        "message_flag" => {
            let args: MessageFlagArgs = parse(args)?;
            flag(engine, args)
        }
        "message_move" => {
            let args: MessageMoveArgs = parse(args)?;
            value(MessageMove { message: engine.move_message(&args.id, &args.folder)? })
        }
        "draft_save" => {
            let args: DraftSaveArgs = parse(args)?;
            let content = draft_content(engine, args)?;
            value(DraftSave { draft: content })
        }
        "draft_send" => {
            let args: DraftSendArgs = parse(args)?;
            value(DraftSend { message: engine.send_draft(&args.id)? })
        }
        "search" => {
            let args: SearchArgs = parse(args)?;
            search(engine, args)
        }
        "labels_list" => {
            let _: LabelsListArgs = parse(args)?;
            value(LabelsList { labels: engine.label_summaries() })
        }
        "label_create" => {
            let args: LabelCreateArgs = parse(args)?;
            value(LabelCreate { label: engine.create_label(&args.name, args.color)? })
        }
        "label_update" => {
            let args: LabelUpdateArgs = parse(args)?;
            value(LabelUpdate { label: engine.update_label(&args.id, args.name.as_deref(), args.color)? })
        }
        "label_delete" => {
            let args: LabelDeleteArgs = parse(args)?;
            let (label, affected) = engine.delete_label(&args.id)?;
            value(LabelDelete { label, affected })
        }
        "label_apply" => {
            let args: LabelApplyArgs = parse(args)?;
            let (label, changed) = engine.set_label(&args.id, &args.message_ids, args.apply)?;
            value(LabelApply { label, changed })
        }
        other => Err(OpError::UnknownOp(other.to_owned())),
    }
}

/// Merge a `draft_save` request into full draft content. On create absent fields are empty; on
/// update absent fields keep the current value and present fields (including empty ones) replace it.
fn draft_content(engine: &mut Engine, args: DraftSaveArgs) -> Result<Message, OpError> {
    let DraftSaveArgs { id, account, to, cc, bcc, subject, body_text, attachments } = args;
    let content = match &id {
        Some(id) => {
            let current = engine.message(id).ok_or_else(|| OpError::InvalidArgs(format!("unknown draft {id:?}; omit id to create a draft")))?;
            DraftContent {
                to: to.unwrap_or_else(|| current.summary.to.clone()),
                cc: cc.unwrap_or_else(|| current.cc.clone()),
                bcc: bcc.unwrap_or_else(|| current.bcc.clone()),
                subject: subject.unwrap_or_else(|| current.summary.subject.clone()),
                body_text: body_text.unwrap_or_else(|| current.body_text.clone()),
                attachments: match attachments {
                    Some(attachments) => draft_attachments(attachments),
                    None => current.attachments.clone(),
                },
            }
        }
        None => DraftContent {
            to: to.unwrap_or_default(),
            cc: cc.unwrap_or_default(),
            bcc: bcc.unwrap_or_default(),
            subject: subject.unwrap_or_default(),
            body_text: body_text.unwrap_or_default(),
            attachments: attachments.map(draft_attachments).unwrap_or_default(),
        },
    };
    engine.save_draft(account.as_deref(), id.as_deref(), content)
}

fn draft_attachments(attachments: Vec<DraftAttachment>) -> Vec<Attachment> {
    attachments
        .into_iter()
        .map(|attachment| Attachment {
            id: attachment.id.unwrap_or_default(),
            filename: attachment.filename,
            mime: attachment.mime,
            size: attachment.size,
        })
        .collect()
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, OpError> {
    serde_json::from_value(args).map_err(|error| OpError::InvalidArgs(error.to_string()))
}

fn value<T: Serialize>(result: T) -> Result<Value, OpError> {
    serde_json::to_value(result).map_err(|error| OpError::Failed(format!("could not serialize the result: {error}")))
}

fn app_info(engine: &Engine) -> AppInfo {
    AppInfo {
        name: "Postino".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: std::env::consts::OS.to_owned(),
        data_dir: Engine::data_dir().map(|path| path.display().to_string()),
        demo: engine.is_demo(),
        store: engine.is_persistent(),
        operations: op_defs().len(),
    }
}

fn folders(engine: &Engine, args: FoldersListArgs) -> Result<Value, OpError> {
    let mut folders = engine.folders().to_vec();
    if let Some(account) = args.account {
        if !engine.accounts().iter().any(|candidate| candidate.id == account) {
            return Err(OpError::Failed(format!("unknown account {account:?}")));
        }
        folders.retain(|folder| folder.account_id == account);
    }
    value(FoldersList { folders })
}

fn find_folder<'a>(engine: &'a Engine, id: &str) -> Result<&'a Folder, OpError> {
    engine.folders().iter().find(|folder| folder.id == id).ok_or_else(|| OpError::Failed(format!("unknown folder {id:?}")))
}

fn page_bounds(limit: Option<u32>, offset: Option<u32>) -> (usize, usize) {
    let limit = usize::try_from(limit.unwrap_or(50).clamp(1, 200)).unwrap_or(50);
    let offset = usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX);
    (limit, offset)
}

fn messages(engine: &Engine, args: MessagesListArgs) -> Result<Value, OpError> {
    if let Some(label) = &args.label
        && engine.label(label).is_none()
    {
        return Err(OpError::Failed(format!("unknown label {label:?}")));
    }
    let folder = match args.folder.as_deref() {
        Some(id) => Some(find_folder(engine, id)?),
        // A label on its own is a mailbox-wide view; without either filter the list stays inbox-first.
        None if args.label.is_some() => None,
        None => Some(find_folder(engine, "inbox")?),
    };
    if let (Some(account), Some(folder)) = (&args.account, folder)
        && folder.account_id != *account
    {
        return Err(OpError::Failed(format!("folder {:?} does not belong to account {account:?}", folder.id)));
    }
    let account_folders = match (&args.account, folder) {
        (Some(account), None) => {
            if !engine.accounts().iter().any(|candidate| candidate.id == *account) {
                return Err(OpError::Failed(format!("unknown account {account:?}")));
            }
            Some(engine.folders().iter().filter(|folder| folder.account_id == *account).map(|folder| folder.id.as_str()).collect::<BTreeSet<_>>())
        }
        _ => None,
    };

    let mut rows: Vec<&Message> = engine
        .messages()
        .iter()
        .filter(|message| {
            let in_scope = match folder {
                Some(folder) if folder.kind == FolderKind::Starred => message.summary.starred,
                Some(folder) => message.summary.folder_id == folder.id,
                None => account_folders.as_ref().is_none_or(|folders| folders.contains(message.summary.folder_id.as_str())),
            };
            in_scope
                && (!args.unread_only || message.summary.unread)
                && (!args.starred_only || message.summary.starred)
                && (!args.with_attachments || message.summary.has_attachments)
                && args.label.as_ref().is_none_or(|label| message.summary.labels.contains(label))
        })
        .collect();
    rows.sort_by_key(|left| std::cmp::Reverse(left.summary.date));

    let total = rows.len();
    let unread = rows.iter().filter(|message| message.summary.unread).count();
    let (limit, offset) = page_bounds(args.limit, args.offset);
    let messages = rows.into_iter().skip(offset).take(limit).map(|message| message.summary.clone()).collect();
    value(MessagesList { folder: folder.map(|folder| folder.id.clone()), messages, total, unread })
}

fn message(engine: &Engine, args: MessageGetArgs) -> Result<Value, OpError> {
    let message = engine.message(&args.id).cloned().ok_or_else(|| OpError::Failed(format!("unknown message {:?}", args.id)))?;
    value(MessageGet { message })
}

fn flag(engine: &mut Engine, args: MessageFlagArgs) -> Result<Value, OpError> {
    let message = engine.set_flags(&args.id, args.unread, args.starred)?;
    value(MessageFlag { message })
}

fn search(engine: &Engine, args: SearchArgs) -> Result<Value, OpError> {
    let query = args.query.trim().to_lowercase();
    if query.is_empty() {
        return Err(OpError::InvalidArgs("query must not be empty".to_owned()));
    }
    let folder = match args.folder.as_deref() {
        Some(id) => Some(find_folder(engine, id)?),
        None => None,
    };
    if let Some(label) = &args.label
        && engine.label(label).is_none()
    {
        return Err(OpError::Failed(format!("unknown label {label:?}")));
    }

    let mut rows: Vec<&Message> =
        engine
            .messages()
            .iter()
            .filter(|message| {
                let in_folder = folder.is_none_or(|folder| {
                    if folder.kind == FolderKind::Starred { message.summary.starred } else { message.summary.folder_id == folder.id }
                });
                let in_label = args.label.as_ref().is_none_or(|label| message.summary.labels.contains(label));
                in_folder && in_label && matches_query(message, &query)
            })
            .collect();
    rows.sort_by_key(|left| std::cmp::Reverse(left.summary.date));

    let total = rows.len();
    let limit = usize::try_from(args.limit.unwrap_or(50).clamp(1, 200)).unwrap_or(50);
    let messages = rows.into_iter().take(limit).map(|message| message.summary.clone()).collect();
    value(SearchResult { query, messages, total })
}

fn matches_query(message: &Message, query: &str) -> bool {
    let summary = &message.summary;
    summary.subject.to_lowercase().contains(query)
        || summary.from.display().to_lowercase().contains(query)
        || summary.to.iter().any(|address| address.display().to_lowercase().contains(query))
        || message.cc.iter().any(|address| address.display().to_lowercase().contains(query))
        || message.bcc.iter().any(|address| address.display().to_lowercase().contains(query))
        || summary.snippet.to_lowercase().contains(query)
        || message.body_text.to_lowercase().contains(query)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use serde_json::json;

    use crate::engine::Engine;
    use crate::model::{Account, Address, Folder, FolderKind, Label, LabelColor, Message, MessageSummary};

    fn call(engine: &mut Engine, name: &str, args: serde_json::Value) -> serde_json::Value {
        engine.call(name, args).expect("operation")
    }

    fn engine_with_search_corpus(count: usize, labelled_index: usize) -> Engine {
        let now = Utc::now();
        let account = Account {
            id: "acc_test".to_owned(),
            name: "Test".to_owned(),
            email: "test@example.com".to_owned(),
            provider: "demo".to_owned(),
            signature: String::new(),
        };
        let folder = Folder {
            id: "inbox".to_owned(),
            account_id: "acc_test".to_owned(),
            name: "Posta in arrivo".to_owned(),
            kind: FolderKind::Inbox,
            unread: 0,
            total: 0,
        };
        let messages = (0..count)
            .map(|index| {
                let id = format!("msg_{index:03}");
                let date = now - Duration::minutes(i64::try_from(index).expect("small index"));
                let mut labels = Vec::new();
                if index == labelled_index {
                    labels.push("lbl_1".to_owned());
                }
                Message {
                    summary: MessageSummary {
                        id: id.clone(),
                        thread_id: format!("thr_{id}"),
                        folder_id: "inbox".to_owned(),
                        from: Address::new("Mittente", "mittente@example.com"),
                        to: vec![Address::new("Test", "test@example.com")],
                        subject: "Rapporto mensile".to_owned(),
                        snippet: "Rapporto mensile completo".to_owned(),
                        date,
                        unread: false,
                        starred: false,
                        has_attachments: false,
                        labels,
                    },
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    body_text: "Rapporto mensile completo".to_owned(),
                    body_html: None,
                    attachments: Vec::new(),
                }
            })
            .collect();
        let labels = vec![Label { id: "lbl_1".to_owned(), name: "Archivio".to_owned(), color: LabelColor::Forest }];
        Engine::from_parts(vec![account], vec![folder], messages, labels, false, None)
    }

    #[test]
    fn table_is_well_formed() {
        let defs = super::op_defs();
        let mut names = std::collections::HashSet::new();
        for def in &defs {
            assert!(names.insert(def.name), "duplicate operation {}", def.name);
            assert!(def.input_schema.is_object(), "{} has no object schema", def.name);
            assert!(!def.title.is_empty());
            assert!(!def.description.is_empty());
        }
        assert!(defs.iter().any(|def| def.name == "messages_list"));
    }

    #[test]
    fn lists_inbox_and_paginates() {
        let mut engine = Engine::demo();
        let result = call(&mut engine, "messages_list", json!({ "folder": "inbox", "limit": 2 }));
        assert_eq!(result["folder"], "inbox");
        assert!(result["messages"].as_array().is_some_and(|rows| rows.len() <= 2));
        assert!(result["total"].as_u64().is_some_and(|total| total >= 2));

        let default = call(&mut engine, "messages_list", json!({}));
        assert_eq!(default["folder"], "inbox", "without folder or label the list still defaults to inbox");
    }

    #[test]
    fn starred_folder_is_virtual() {
        let mut engine = Engine::demo();
        let starred = call(&mut engine, "messages_list", json!({ "folder": "starred" }));
        let rows = starred["messages"].as_array().expect("rows");
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row["starred"] == json!(true)));
    }

    #[test]
    fn search_reads_bodies() {
        let mut engine = Engine::demo();
        let result = call(&mut engine, "search", json!({ "query": "postino" }));
        assert!(result["total"].as_u64().is_some_and(|total| total >= 1));
    }

    #[test]
    fn search_applies_the_label_before_the_row_limit() {
        let mut engine = engine_with_search_corpus(250, 249);

        let unfiltered = call(&mut engine, "search", json!({ "query": "rapporto", "limit": 200 }));
        assert_eq!(unfiltered["total"], 250);
        let rows = unfiltered["messages"].as_array().expect("rows");
        assert_eq!(rows.len(), 200);
        assert!(!rows.iter().any(|row| row["id"] == "msg_249"), "the oldest match is beyond the first 200 rows");

        let filtered = call(&mut engine, "search", json!({ "query": "rapporto", "label": "lbl_1", "limit": 200 }));
        assert_eq!(filtered["total"], 1);
        assert_eq!(filtered["messages"][0]["id"], "msg_249");

        let unknown = engine.call("search", json!({ "query": "rapporto", "label": "lbl_missing" })).expect_err("unknown label");
        assert!(matches!(unknown, crate::OpError::Failed(_)));
    }

    #[test]
    fn flag_requires_a_change() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].summary.id.clone();
        let error = engine.call("message_flag", json!({ "id": id })).expect_err("no flags");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn unknown_folder_fails() {
        let mut engine = Engine::demo();
        let error = engine.call("messages_list", json!({ "folder": "nope" })).expect_err("folder");
        assert!(matches!(error, crate::OpError::Failed(_)));
    }

    #[test]
    fn empty_search_is_refused() {
        let mut engine = Engine::demo();
        let error = engine.call("search", json!({ "query": "   " })).expect_err("query");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn moving_a_message_updates_both_folders() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].summary.id.clone();
        let inbox_before = engine.folders().iter().find(|folder| folder.id == "inbox").map_or(0, |folder| folder.total);
        let archive_before = engine.folders().iter().find(|folder| folder.id == "archive").map_or(0, |folder| folder.total);
        let result = call(&mut engine, "message_move", json!({ "id": id, "folder": "archive" }));
        assert_eq!(result["message"]["folder_id"], "archive");
        let inbox_after = engine.folders().iter().find(|folder| folder.id == "inbox").map_or(0, |folder| folder.total);
        let archive_after = engine.folders().iter().find(|folder| folder.id == "archive").map_or(0, |folder| folder.total);
        assert_eq!(inbox_after, inbox_before.saturating_sub(1));
        assert_eq!(archive_after, archive_before.saturating_add(1));
    }

    #[test]
    fn virtual_folders_cannot_receive_messages() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].summary.id.clone();
        let error = engine.call("message_move", json!({ "id": id, "folder": "starred" })).expect_err("virtual folder");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn moving_an_unknown_message_fails() {
        let mut engine = Engine::demo();
        let error = engine.call("message_move", json!({ "id": "missing", "folder": "archive" })).expect_err("unknown message");
        assert!(matches!(error, crate::OpError::Failed(_)));
    }

    #[test]
    fn draft_save_and_send_work_through_the_registry() {
        let mut engine = Engine::demo();
        let saved = call(
            &mut engine,
            "draft_save",
            json!({
                "to": [{ "name": "Giulia", "email": "giulia.ferri@example.com" }],
                "subject": "Proposta",
                "body_text": "Ciao Giulia,\n\nci sentiamo domani."
            }),
        );
        let id = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
        assert_eq!(saved["draft"]["summary"]["folder_id"], "drafts");
        let sent = call(&mut engine, "draft_send", json!({ "id": id }));
        assert_eq!(sent["message"]["folder_id"], "sent");
        let drafts = call(&mut engine, "messages_list", json!({ "folder": "drafts" }));
        assert!(!drafts["messages"].as_array().expect("rows").iter().any(|row| row["id"] == json!(id)));
    }

    #[test]
    fn draft_save_update_is_a_patch_and_create_defaults_to_empty() {
        let mut engine = Engine::demo();
        let saved = call(
            &mut engine,
            "draft_save",
            json!({
                "to": [{ "name": "Giulia", "email": "giulia.ferri@example.com" }],
                "cc": [{ "email": "marco.bianchi@example.com" }],
                "subject": "Proposta",
                "body_text": "Prima versione",
                "attachments": [{ "filename": "preventivo.pdf", "mime": "application/pdf", "size": 1200 }]
            }),
        );
        let id = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();

        let updated = call(&mut engine, "draft_save", json!({ "id": id, "body_text": "Solo il corpo cambia" }));
        assert_eq!(updated["draft"]["body_text"], "Solo il corpo cambia");
        assert_eq!(updated["draft"]["summary"]["subject"], "Proposta", "an omitted subject was wiped");
        assert_eq!(updated["draft"]["summary"]["to"], json!([{ "name": "Giulia", "email": "giulia.ferri@example.com" }]));
        assert_eq!(updated["draft"]["cc"], json!([{ "name": "", "email": "marco.bianchi@example.com" }]));
        assert_eq!(updated["draft"]["attachments"].as_array().map(Vec::len), Some(1), "omitted attachments were wiped");
        assert_eq!(updated["draft"]["summary"]["id"], json!(id), "the patch created a second draft");

        let cleared =
            call(&mut engine, "draft_save", json!({ "id": id, "to": [], "cc": [], "bcc": [], "subject": "", "body_text": "", "attachments": [] }));
        assert_eq!(cleared["draft"]["summary"]["to"], json!([]));
        assert_eq!(cleared["draft"]["cc"], json!([]));
        assert_eq!(cleared["draft"]["summary"]["subject"], "");
        assert_eq!(cleared["draft"]["body_text"], "");
        assert_eq!(cleared["draft"]["attachments"], json!([]));

        let created = call(&mut engine, "draft_save", json!({ "subject": "Solo oggetto" }));
        assert_eq!(created["draft"]["summary"]["to"], json!([]));
        assert_eq!(created["draft"]["cc"], json!([]));
        assert_eq!(created["draft"]["bcc"], json!([]));
        assert_eq!(created["draft"]["body_text"], "");
        assert_eq!(created["draft"]["attachments"], json!([]));
    }

    #[test]
    fn draft_schema_exposes_structured_recipients() {
        let schema = super::op_defs().into_iter().find(|def| def.name == "draft_save").expect("draft_save").input_schema;
        assert!(schema["properties"]["to"]["items"].is_object());
        assert!(schema["properties"]["body_text"].is_object());
        assert!(schema["properties"]["attachments"]["items"].is_object());
    }

    #[test]
    fn draft_recipients_reject_unknown_fields() {
        let mut engine = Engine::demo();
        let error = engine
            .call(
                "draft_save",
                json!({
                    "to": [{ "email": "giulia.ferri@example.com", "nmae": "typo" }],
                    "subject": "Ciao",
                    "body_text": "Corpo"
                }),
            )
            .expect_err("unknown address field");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn labels_list_reports_demo_counts() {
        let mut engine = Engine::demo();
        let result = call(&mut engine, "labels_list", json!({}));
        let labels = result["labels"].as_array().expect("labels");
        assert_eq!(labels.len(), 3);
        let lavoro = labels.iter().find(|label| label["name"] == "Lavoro").expect("Lavoro");
        assert_eq!(lavoro["id"], "lbl_1");
        assert_eq!(lavoro["color"], "gold");
        assert_eq!(lavoro["message_count"], 6);
        assert_eq!(lavoro["unread_count"], 2);
        let fatture = labels.iter().find(|label| label["name"] == "Fatture").expect("Fatture");
        assert_eq!(fatture["message_count"], 1);
        assert_eq!(fatture["unread_count"], 1);
    }

    #[test]
    fn label_crud_validates_names_and_colors() {
        let mut engine = Engine::demo();
        let created = call(&mut engine, "label_create", json!({ "name": "  Ufficio  ", "color": "forest" }));
        assert_eq!(created["label"]["name"], "Ufficio", "names are stored trimmed");
        let id = created["label"]["id"].as_str().expect("id").to_owned();
        assert_eq!(created["label"]["message_count"], 0);

        let duplicate = engine.call("label_create", json!({ "name": "ufficio", "color": "gold" })).expect_err("duplicate");
        assert!(matches!(duplicate, crate::OpError::InvalidArgs(_)));
        let duplicate = engine.call("label_update", json!({ "id": id, "name": "LAVORO" })).expect_err("duplicate on rename");
        assert!(matches!(duplicate, crate::OpError::InvalidArgs(_)));
        assert!(matches!(engine.call("label_create", json!({ "name": "Blu", "color": "blue" })), Err(crate::OpError::InvalidArgs(_))));
        assert!(matches!(engine.call("label_create", json!({ "name": "   ", "color": "gold" })), Err(crate::OpError::InvalidArgs(_))));
        let long = "x".repeat(49);
        assert!(matches!(engine.call("label_create", json!({ "name": long, "color": "gold" })), Err(crate::OpError::InvalidArgs(_))));
        assert!(matches!(engine.call("label_create", json!({ "name": "Ciao\nBcc", "color": "gold" })), Err(crate::OpError::InvalidArgs(_))));
        assert!(matches!(engine.call("label_update", json!({ "id": id })), Err(crate::OpError::InvalidArgs(_))));

        let updated = call(&mut engine, "label_update", json!({ "id": id, "name": "Ufficio", "color": "muted" }));
        assert_eq!(updated["label"]["color"], "muted");
        let deleted = call(&mut engine, "label_delete", json!({ "id": id }));
        assert_eq!(deleted["label"]["name"], "Ufficio");
        assert_eq!(deleted["affected"], 0);
        assert!(engine.label(&id).is_none());
        assert!(matches!(engine.call("label_update", json!({ "id": id, "name": "X" })), Err(crate::OpError::Failed(_))));
    }

    #[test]
    fn deleting_a_label_detaches_it_from_every_message() {
        let mut engine = Engine::demo();
        let result = call(&mut engine, "label_delete", json!({ "id": "lbl_1" }));
        assert_eq!(result["label"]["id"], "lbl_1");
        assert_eq!(result["affected"], 6);
        assert!(engine.messages().iter().all(|message| !message.summary.labels.iter().any(|label| label == "lbl_1")));
        let labels = call(&mut engine, "labels_list", json!({}));
        assert_eq!(labels["labels"].as_array().expect("labels").len(), 2);
    }

    #[test]
    fn label_apply_and_remove_are_idempotent() {
        let mut engine = Engine::demo();
        let args = json!({ "id": "lbl_2", "message_ids": ["msg_001"], "apply": true });
        let first = call(&mut engine, "label_apply", args.clone());
        assert_eq!(first["changed"], 1);
        assert!(engine.message("msg_001").is_some_and(|message| message.summary.labels.contains(&"lbl_2".to_owned())));
        let second = call(&mut engine, "label_apply", args);
        assert_eq!(second["changed"], 0);

        let remove = json!({ "id": "lbl_2", "message_ids": ["msg_001"], "apply": false });
        let first = call(&mut engine, "label_apply", remove.clone());
        assert_eq!(first["changed"], 1);
        let second = call(&mut engine, "label_apply", remove);
        assert_eq!(second["changed"], 0);
        assert!(engine.message("msg_001").is_some_and(|message| !message.summary.labels.iter().any(|label| label == "lbl_2")));
    }

    #[test]
    fn label_apply_validates_the_batch_before_mutating() {
        let mut engine = Engine::demo();
        let error =
            engine.call("label_apply", json!({ "id": "lbl_2", "message_ids": ["msg_001", "missing"], "apply": true })).expect_err("unknown message");
        assert!(matches!(error, crate::OpError::Failed(_)));
        assert!(
            engine.message("msg_001").is_some_and(|message| !message.summary.labels.iter().any(|label| label == "lbl_2")),
            "the valid message must be untouched when the batch fails"
        );
        assert!(matches!(
            engine.call("label_apply", json!({ "id": "lbl_2", "message_ids": [], "apply": true })),
            Err(crate::OpError::InvalidArgs(_))
        ));
        assert!(matches!(
            engine.call("label_apply", json!({ "id": "missing", "message_ids": ["msg_001"], "apply": true })),
            Err(crate::OpError::Failed(_))
        ));
        let ids = (0..501).map(|index| format!("msg_{index:03}")).collect::<Vec<_>>();
        let error = engine.call("label_apply", json!({ "id": "lbl_2", "message_ids": ids, "apply": true })).expect_err("cap");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn messages_can_be_filtered_by_label_and_unknown_labels_fail() {
        let mut engine = Engine::demo();

        let global = call(&mut engine, "messages_list", json!({ "label": "lbl_1" }));
        let rows = global["messages"].as_array().expect("rows");
        assert_eq!(global["folder"], json!(null), "a label without a folder spans every folder");
        assert_eq!(global["total"], 6);
        assert_eq!(global["unread"], 2);
        assert!(rows.iter().all(|row| row["labels"].as_array().is_some_and(|labels| labels.contains(&json!("lbl_1")))));
        assert!(rows.iter().any(|row| row["folder_id"] == "sent"));
        assert!(rows.iter().any(|row| row["folder_id"] == "archive"));

        let inbox = call(&mut engine, "messages_list", json!({ "folder": "inbox", "label": "lbl_1" }));
        assert_eq!(inbox["folder"], "inbox");
        assert_eq!(inbox["total"], 2, "folder and label intersect");
        assert_eq!(inbox["unread"], 2);

        let sent = call(&mut engine, "messages_list", json!({ "folder": "sent", "label": "lbl_1" }));
        assert_eq!(sent["total"], 2);
        assert_eq!(sent["messages"][0]["id"], "msg_002");

        let starred = call(&mut engine, "messages_list", json!({ "folder": "starred", "label": "lbl_1" }));
        let rows = starred["messages"].as_array().expect("starred rows");
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row["starred"] == json!(true)));
        assert!(rows.iter().all(|row| row["labels"].as_array().is_some_and(|labels| labels.contains(&json!("lbl_1")))));

        let error = engine.call("messages_list", json!({ "label": "lbl_missing" })).expect_err("unknown label");
        assert!(matches!(error, crate::OpError::Failed(_)));
        let error = engine.call("messages_list", json!({ "folder": "inbox", "label": "lbl_missing" })).expect_err("unknown label with folder");
        assert!(matches!(error, crate::OpError::Failed(_)));
    }

    #[test]
    fn label_ids_are_not_reused() {
        let mut engine = Engine::demo();
        call(&mut engine, "label_delete", json!({ "id": "lbl_3" }));
        let created = call(&mut engine, "label_create", json!({ "name": "Quarta", "color": "forest" }));
        assert_eq!(created["label"]["id"], "lbl_4");
        let created = call(&mut engine, "label_create", json!({ "name": "Quinta", "color": "muted" }));
        assert_eq!(created["label"]["id"], "lbl_5");
    }

    #[test]
    fn label_names_refuse_bidi_controls_and_accept_accents() {
        let mut engine = Engine::demo();
        let bidi = engine.call("label_create", json!({ "name": "A\u{202e}B", "color": "gold" })).expect_err("bidi override");
        assert!(matches!(bidi, crate::OpError::InvalidArgs(_)));
        let created = call(&mut engine, "label_create", json!({ "name": "Città", "color": "gold" }));
        assert_eq!(created["label"]["name"], "Città");
        let renamed = engine
            .call("label_update", json!({ "id": created["label"]["id"], "name": "Città \u{2060}studio" }))
            .expect_err("update validates names through the same rule");
        assert!(matches!(renamed, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn draft_attachments_can_omit_the_id() {
        let mut engine = Engine::demo();
        let saved = call(
            &mut engine,
            "draft_save",
            json!({
                "to": [{ "email": "giulia.ferri@example.com" }],
                "subject": "Con allegato",
                "body_text": "Corpo",
                "attachments": [{ "filename": "preventivo.pdf", "mime": "application/pdf", "size": 1200 }]
            }),
        );
        let id = saved["draft"]["attachments"][0]["id"].as_str().expect("generated attachment id");
        assert!(!id.is_empty());
    }
}
