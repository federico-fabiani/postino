//! The operation registry. One typed request per operation, one JSON Schema, one handler.
//!
//! Names are `snake_case` because MCP clients reject dots. Unknown argument keys are refused, so
//! a typo fails loudly instead of being silently ignored.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::engine::{Engine, OpError};
use crate::model::{Account, Address, Attachment, DraftContent, Folder, FolderKind, Message, MessageSummary};

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
            "Version, platform, data directory and whether the engine is running on synthetic demo data.",
            AppInfoArgs,
            true,
            false
        ),
        def!(
            "accounts_list",
            "List accounts",
            "The configured mail accounts. M0 has at most the synthetic demo account.",
            AccountsListArgs,
            true,
            false
        ),
        def!("folders_list", "List folders", "Folders for an account, with unread and total counts.", FoldersListArgs, true, false),
        def!(
            "messages_list",
            "List messages",
            "Message summaries in a folder, newest first. Bodies are fetched separately with message_get.",
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
            "Create or update a draft in Drafts. Omit `id` for a new draft, pass the returned id to update it in place. Recipients are structured addresses; header injection is refused.",
            DraftSaveArgs,
            false,
            false
        ),
        def!(
            "draft_send",
            "Send a draft (demo)",
            "Demo only: move a draft to Sent and record the current timestamp. No SMTP server is contacted; real submission arrives in M2. At least one To, Cc or Bcc recipient is required.",
            DraftSendArgs,
            false,
            false
        ),
        def!(
            "search",
            "Search mail",
            "Case-insensitive search across subjects, people, snippets and bodies of the demo mailbox.",
            SearchArgs,
            true,
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
    /// Folder id; defaults to `inbox`. `starred` is the virtual starred folder.
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
    /// Primary recipients. An empty list is allowed while drafting.
    #[serde(default)]
    pub to: Vec<Address>,
    #[serde(default)]
    pub cc: Vec<Address>,
    #[serde(default)]
    pub bcc: Vec<Address>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body_text: String,
    /// Attachment metadata. M0 stores the name, MIME type and size; the bytes arrive in M2.
    #[serde(default)]
    pub attachments: Vec<DraftAttachment>,
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
    /// Maximum matches, 1-200. Defaults to 50.
    pub limit: Option<u32>,
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
    pub demo: bool,
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
    pub folder: String,
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
            let content = DraftContent {
                to: args.to,
                cc: args.cc,
                bcc: args.bcc,
                subject: args.subject,
                body_text: args.body_text,
                attachments: args
                    .attachments
                    .into_iter()
                    .map(|attachment| Attachment {
                        id: attachment.id.unwrap_or_default(),
                        filename: attachment.filename,
                        mime: attachment.mime,
                        size: attachment.size,
                    })
                    .collect(),
            };
            value(DraftSave { draft: engine.save_draft(args.id.as_deref(), content)? })
        }
        "draft_send" => {
            let args: DraftSendArgs = parse(args)?;
            value(DraftSend { message: engine.send_draft(&args.id)? })
        }
        "search" => {
            let args: SearchArgs = parse(args)?;
            search(engine, args)
        }
        other => Err(OpError::UnknownOp(other.to_owned())),
    }
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
    let folder = find_folder(engine, args.folder.as_deref().unwrap_or("inbox"))?;
    if let Some(account) = &args.account
        && folder.account_id != *account
    {
        return Err(OpError::Failed(format!("folder {:?} does not belong to account {account:?}", folder.id)));
    }

    let mut rows: Vec<&Message> = engine
        .messages()
        .iter()
        .filter(|message| {
            let in_folder = if folder.kind == FolderKind::Starred { message.summary.starred } else { message.summary.folder_id == folder.id };
            in_folder
                && (!args.unread_only || message.summary.unread)
                && (!args.starred_only || message.summary.starred)
                && (!args.with_attachments || message.summary.has_attachments)
        })
        .collect();
    rows.sort_by_key(|left| std::cmp::Reverse(left.summary.date));

    let total = rows.len();
    let unread = rows.iter().filter(|message| message.summary.unread).count();
    let (limit, offset) = page_bounds(args.limit, args.offset);
    let messages = rows.into_iter().skip(offset).take(limit).map(|message| message.summary.clone()).collect();
    value(MessagesList { folder: folder.id.clone(), messages, total, unread })
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

    let mut rows: Vec<&Message> =
        engine
            .messages()
            .iter()
            .filter(|message| {
                let in_folder = folder.is_none_or(|folder| {
                    if folder.kind == FolderKind::Starred { message.summary.starred } else { message.summary.folder_id == folder.id }
                });
                in_folder && matches_query(message, &query)
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
    use serde_json::json;

    use crate::engine::Engine;

    fn call(engine: &mut Engine, name: &str, args: serde_json::Value) -> serde_json::Value {
        engine.call(name, args).expect("operation")
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
