//! The operation registry. One typed request per operation, one JSON Schema, one handler.
//!
//! Names are `snake_case` because MCP clients reject dots. Unknown argument keys are refused, so
//! a typo fails loudly instead of being silently ignored.

use std::collections::BTreeSet;

use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use zeroize::Zeroizing;

use postino_sync::SyncStatus;

use crate::engine::{AccountSetup, Engine, OpError, unknown_account, unknown_folder, unknown_label, unknown_message};
use crate::model::{
    AccountView, Address, Attachment, DraftContent, Folder, FolderKind, Label, LabelColor, LabelSummary, MailTls, Message, MessageSummary, Provider,
};
use crate::store_map;

/// The largest standard-base64 string that can decode to [`crate::MAX_ATTACHMENT_BYTES`]. Checked
/// before allocating the decoded buffer: a hostile caller cannot make the engine allocate 25 MiB
/// from a one-line "payload" that claims to be enormous.
const MAX_ATTACHMENT_BASE64_CHARS: usize = ((crate::engine::MAX_ATTACHMENT_BYTES as usize).saturating_add(2) / 3).saturating_mul(4);

/// One operation as the CLI and MCP see it.
#[derive(Debug, Clone, Serialize)]
pub struct OpDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub read_only: bool,
    pub destructive: bool,
    /// The operation handles a credential (a password or a keyring entry). The CLI and the desktop
    /// app may call it; the MCP server must neither list it nor dispatch it.
    pub sensitive: bool,
}

macro_rules! def {
    ($name:literal, $title:literal, $desc:literal, $args:ty, $ro:literal, $destructive:literal) => {
        def!($name, $title, $desc, $args, $ro, $destructive, false)
    };
    ($name:literal, $title:literal, $desc:literal, $args:ty, $ro:literal, $destructive:literal, $sensitive:literal) => {
        OpDef {
            name: $name,
            title: $title,
            description: $desc,
            input_schema: serde_json::to_value(schemars::schema_for!($args)).unwrap_or(Value::Null),
            read_only: $ro,
            destructive: $destructive,
            sensitive: $sensitive,
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
            "Every configured account with its IMAP provider, address, server host/port/TLS, optional SMTP submission endpoint and last sync status. A damaged sync_status value is reported as null with a warning, never as a listing failure. Never includes a password or a credential: account_add and account_remove are sensitive operations available only from the CLI and the desktop app.",
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
            "Create or patch a draft in Drafts. Omit `id` to create one: absent content fields are empty. With `id`, an absent field keeps its current value and a present field replaces it, including an empty list or empty string. `account` chooses the account on create; it is required when several accounts exist. `attachments` carries metadata only; use draft_attachment_add for the bytes. `in_reply_to` stores the parent Message-ID for a reply and is serialized as In-Reply-To/References.",
            DraftSaveArgs,
            false,
            false
        ),
        def!(
            "draft_attachment_add",
            "Attach bytes to a draft",
            "Add one file to a draft and store its exact bytes in the encrypted local blob store. `data_base64` is bounded to 25 MiB decoded and is rejected on an invalid alphabet; the bytes leave the caller and are stored only in the encrypted store. At most 20 files, 25 MiB per file and 25 MiB total; a duplicate filename is renamed (`name (2).ext`) and the filename is sanitized. The MIME type comes from the extension table; a declared type is accepted only when it agrees with that table, so an unknown extension is application/octet-stream. MCP callers send bytes, never a filesystem path. Returns the updated draft.",
            DraftAttachmentAddArgs,
            false,
            false
        ),
        def!(
            "draft_attachment_remove",
            "Remove an attachment from a draft",
            "Remove one attachment row from a draft by attachment id or exact filename. The encrypted blob stays in the content-addressed store until a later garbage collector, so this frees the row, not necessarily the disk file. Returns the updated draft.",
            DraftAttachmentRemoveArgs,
            false,
            false
        ),
        def!(
            "message_send",
            "Send a draft",
            "Queue a draft for submission through the account's SMTP server, with a 5-second undo window before the first attempt. The exact RFC 5322 bytes and the SMTP envelope are persisted before any network call; a draft with a live outbox row cannot be edited or moved until it is sent, discarded or cancelled. The draft stays in Drafts while queued. Sending leaves the machine: the desktop worker or `postino send` processes the queue; the MCP server only queues and this build has no daemon. A failed submission keeps its typed state and a fixed error sentence in the outbox.",
            MessageSendArgs,
            false,
            false
        ),
        def!(
            "send_schedule",
            "Schedule a draft",
            "Queue a draft for submission at `send_at` (RFC 3339 with a timezone). The bytes and envelope are persisted now; editing the draft afterwards is refused until the row is cancelled or sent. A time in the past is treated as now. The MCP server only queues: the desktop worker or `postino send` submits it when due.",
            SendScheduleArgs,
            false,
            false
        ),
        def!(
            "outbox_list",
            "List the outbox",
            "Journal rows, newest first, with state (queued, sending, append_pending, sent, uncertain, failed, cancelled), attempts, a fixed last-error sentence (stage and code, never server text), recipient count, subject and message id, plus the accounts whose queue is stopped by an authentication failure. `limit` (1-500, default 500) and `offset` page through the history; terminal rows older than 30 days are pruned. Read-only and offline. Never includes a password or a message body.",
            OutboxListArgs,
            true,
            false
        ),
        def!(
            "outbox_cancel",
            "Cancel or discard a send",
            "Discard a row that is still `queued` before the worker leases it, or a `failed` row (a permanent refusal, so no delivery happened); the draft stays in Drafts and becomes editable again. After `sending`, `append_pending` or `uncertain` the only resolutions are the uncertain policy (`outbox_retry` or `outbox_mark_sent`), because the message may already have been delivered.",
            OutboxCancelArgs,
            false,
            false
        ),
        def!(
            "outbox_retry",
            "Retry a failed send",
            "Manually retry one `failed` or `uncertain` row with `confirm: true`. An uncertain row may already have reached the server, so retrying can deliver it twice; check Sent first. Clears an account authentication halt. Never retried automatically.",
            OutboxRetryArgs,
            false,
            false
        ),
        def!(
            "outbox_mark_sent",
            "Mark an uncertain send as sent",
            "Resolve one `uncertain` row as delivered without contacting the server, with `confirm: true`. This records the user's decision and moves the local draft to Sent; it never submits again and never proves delivery.",
            OutboxMarkSentArgs,
            false,
            false
        ),
        def!(
            "send_dry_run",
            "Dry-run a send",
            "Return the exact RFC 5322 bytes (base64), the SMTP envelope including Bcc, the byte count, subject and Message-ID that a send would use, without writing a queue row or opening a connection. The serializer uses the draft's own saved date, so these bytes are identical to what a later send submits. Read-only.",
            SendDryRunArgs,
            true,
            false
        ),
        def!(
            "search",
            "Search mail",
            "Search the encrypted local index. Grammar: `from:`, `to:`, `cc:`, `bcc:`, `subject:`, `label:`, `folder:`, `is:unread|read|starred|unstarred`, `has:attachment`, `before:`/`after:` (YYYY-MM-DD UTC), `larger:`/`smaller:` (k/m/g). Bare terms search all text, `*` marks a prefix, quoted values support `\\\"`, and AND/OR/NOT (uppercase) plus parentheses compose clauses. Hits are bm25-ranked with an exact global date/id tie-break; `sort: \"date\"` orders newest first. `unread_only` and `with_attachments` filter the same scope as the page. `total` is exact up to 10 000 and `total_capped` says when it is not. At most 200 hits per page and offset 5000. Snippets are plain text, never HTML. Unknown folder or label names/ids are an error. A parse error is an invalid argument carrying the error code and position. While a pending index is building, the result has empty hits and an `index` object with state/counts/last_error.",
            SearchArgs,
            true,
            false
        ),
        def!(
            "search_status",
            "Search index status",
            "Report whether the local full-text index is ready, how many documents and message rows it covers, FTS/rank row-id drift, the database+WAL size, the last error and the last index update. Read-only and offline. A failed build is reported here and never blocks opening the mailbox; a `ready: false` index is repairable with search_reindex.",
            SearchStatusArgs,
            true,
            false
        ),
        def!(
            "search_reindex",
            "Rebuild the search index",
            "Delete and rebuild the encrypted full-text and rank indexes from the stored message rows in resumable 500-row batches. Idempotent; it never vacuums the database and never contacts a server. It is a single-process maintenance action: do not run it from the desktop and MCP at once. Returns the document count and duration. It also rebuilds the thread cache from the stored messages; it does not re-derive the thread id of mail cached before P9b.",
            SearchReindexArgs,
            false,
            false
        ),
        def!(
            "threads_list",
            "List conversations",
            "Conversation rows, newest first: id, subject, plain snippet, unique participants, mailbox-wide message/unread/starred counts, labels and the latest message id. A folder, label, unread, starred or attachment filter decides which threads are listed; it never narrows the counts on a row. `total`, `unread_threads` and `unread_messages` describe the filtered scope. At most 200 rows per page and offset 5000. Read-only and offline.",
            ThreadsListArgs,
            true,
            false
        ),
        def!(
            "thread_get",
            "Get a conversation",
            "One conversation and an ascending page of its message summaries (at most 200; bodies are read with message_get). `resolved_id` is the current id after a merge: pass an old id and it still resolves through the alias table. `truncated` says whether more messages exist after the page; the desktop loads the newest page first and prepends older pages. Read-only and offline.",
            ThreadGetArgs,
            true,
            false
        ),
        def!(
            "thread_flag",
            "Change conversation flags",
            "Set the read/unread or starred state of every message in one conversation, up to 500 messages. At least one flag is required. Idempotent: repeating the same state returns `changed: 0`. Returns the refreshed thread row. This is local state only; it never contacts a server.",
            ThreadFlagArgs,
            false,
            false
        ),
        def!(
            "saved_searches_list",
            "List saved searches",
            "Every saved search for one account, by name ignoring case, with computed `total`/`unread` counts, `valid`/`error` and `index_pending`. A stored query whose label or folder no longer exists is `valid: false`, never a dead entry; a valid text query while the FTS index is still building is `valid: true, index_pending: true` with zero counts. `recent` is the last eight successful queries, newest first. Counts are cached until the mailbox changes. Read-only and offline.",
            SavedSearchesListArgs,
            true,
            false
        ),
        def!(
            "saved_search_create",
            "Save a search",
            "Store a named search for one account. Names are 1-48 characters, unique ignoring case and capped at 12 per account; the cap and insert are one store transaction, so two processes cannot both create the 13th. The query is the documented search language and is validated now: an unclosed quote, an unknown label or an unknown folder is refused. `sort` is `relevance` (default) or `date`. Returns the row with its computed counts. Saved searches are local metadata in the encrypted store.",
            SavedSearchCreateArgs,
            false,
            false
        ),
        def!(
            "saved_search_update",
            "Update a saved search",
            "Change the name, query and/or sort of one saved search. At least one field is required; the id and account never change. `account` is required when the mailbox has several accounts and the row must belong to it. The query is validated like saved_search_create. Returns the row with recomputed counts.",
            SavedSearchUpdateArgs,
            false,
            false
        ),
        def!(
            "saved_search_delete",
            "Delete a saved search",
            "Delete one saved search row. `account` is required when the mailbox has several accounts and the row must belong to it. This removes local metadata only, never a message. Returns the deleted row.",
            SavedSearchDeleteArgs,
            false,
            true
        ),
        def!(
            "saved_search_recent_clear",
            "Clear recent searches",
            "Forget one account's eight recent successful queries. They live only in the encrypted store's `meta` table and are never sent anywhere. Returns whether anything was removed.",
            SavedSearchRecentClearArgs,
            false,
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
        def!(
            "account_add",
            "Add an account",
            "Sensitive: the IMAP password and an optional separate SMTP password are read from the caller, used once for the login checks and stored in the OS keyring (`account:<id>:imap` and `account:<id>:smtp`), never logged or written to the database. Known providers are offline: aruba uses imaps.aruba.it:993 and smtps.aruba.it:465 implicit; gmail uses imap.gmail.com:993 and smtp.gmail.com:465 implicit and needs 2-Step Verification plus an app password (OAuth arrives in P12); custom requires IMAP host/port/tls and may add SMTP host/port/tls. Both servers are contacted before anything is written, so a failed login writes nothing. When no separate SMTP password is supplied, the IMAP credential is used for submission. Returns the account with its endpoints and sync state; never the credentials.",
            AccountAddArgs,
            false,
            false,
            true
        ),
        def!(
            "account_update",
            "Update an account's SMTP endpoint",
            "Sensitive: change where an existing account submits mail without touching its IMAP settings, cached mail or labels. Supply smtp_host, smtp_port and smtp_tls (known providers accept only their preset), optionally smtp_username and a new smtp_password; omit the password to keep the existing SMTP secret or fall back to the IMAP credential. clear_smtp: true removes the endpoint and its separate credential. The SMTP server is probed before anything is written. Available only from the CLI and the desktop app, never over MCP.",
            AccountUpdateArgs,
            false,
            false,
            true
        ),
        def!(
            "account_signature_set",
            "Set an account signature",
            "Set the plain-text signature of one account, up to 4 KiB. Newlines and tabs are the only control characters accepted; HTML, images and tracking are not supported. The signature belongs to the account and is inserted in new, reply and forward messages for that identity; a draft already saved keeps the text it owns. Returns the account view.",
            AccountSignatureArgs,
            false,
            false
        ),
        def!(
            "account_test",
            "Test an account",
            "Log in with the account's stored credential and list its folders. Read-only: nothing is written, no mailbox is selected and no message is fetched. Returns the selectable folder count and the server capabilities. A rejected credential is an auth error naming the provider and the likely cause; the cached mail is unchanged.",
            AccountTestArgs,
            true,
            false
        ),
        def!(
            "account_remove",
            "Remove an account",
            "Sensitive and destructive: delete the account's IMAP credential, its server configuration and its local cache (folders, messages, drafts and attachments). Labels stay. Requires confirm: true. The cache is deleted first and the keyring entry second, so a failure can leave an orphan credential but never cached mail whose password is gone. Available only from the CLI and the desktop app, never over MCP.",
            AccountRemoveArgs,
            false,
            true,
            true
        ),
        def!(
            "sync_now",
            "Sync an account now",
            "Run one IMAP sync for an account and cache new mail, bodies, flags and attachments in the encrypted store. `account` may be omitted when exactly one account exists and is required when several do. Returns what the run did; a failed run keeps the previous cache and every message committed before the stop.",
            SyncNowArgs,
            false,
            false
        ),
        def!(
            "sync_status",
            "Sync status",
            "The last persisted sync status per account, read offline: phase, last attempt, last success, error sentence and folders done/total. `account` narrows the answer to one account. Never includes a credential, a host name or message content.",
            SyncStatusArgs,
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
    /// them. The bytes are stored through the encrypted attachment seam; sending requires them.
    pub attachments: Option<Vec<DraftAttachment>>,
    /// Parent `Message-ID` for a reply, with or without angle brackets. Omit on update to keep the
    /// stored linkage; a new draft starts with none. It is serialized as `In-Reply-To` and the
    /// parent id alone is written to `References` until P9 carries the full chain.
    pub in_reply_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachment {
    /// Stable id. Omit it and the engine assigns one when the draft is saved.
    pub id: Option<String>,
    pub filename: String,
    pub mime: String,
    /// Declared size. The exact stored size is read back from the encrypted blob after a write.
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachmentAddArgs {
    /// Draft id returned by draft_save.
    pub draft: String,
    /// File name. It is sanitized; CR, LF and NUL are refused and a duplicate is renamed.
    pub filename: String,
    /// Declared `type/subtype`. It is only a hint: the extension table wins unless the declaration
    /// agrees with it, and unknown extensions become application/octet-stream.
    pub mime: Option<String>,
    /// Standard base64 of the exact bytes, at most 25 MiB decoded.
    pub data_base64: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftAttachmentRemoveArgs {
    /// Draft id returned by draft_save.
    pub draft: String,
    /// Attachment id returned by draft_save or message_get, or its exact filename.
    pub attachment: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountSignatureArgs {
    /// Account id returned by accounts_list.
    pub account: String,
    /// Plain text, at most 4 KiB, with newline and tab as the only control characters.
    pub signature: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageSendArgs {
    /// Draft id returned by draft_save.
    pub draft: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendScheduleArgs {
    /// Draft id returned by draft_save.
    pub draft: String,
    /// RFC 3339 instant with a timezone, for example `2026-10-26T18:30:00+01:00`.
    pub send_at: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxListArgs {
    /// Account id; omit for every account.
    pub account: Option<String>,
    /// Rows per page, 1-500. Defaults to 500.
    pub limit: Option<u32>,
    /// Rows to skip, newest first. Defaults to 0.
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxCancelArgs {
    /// Outbox row id returned by message_send, send_schedule or outbox_list.
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxRetryArgs {
    /// Outbox row id.
    pub id: String,
    /// Must be true. An uncertain row may already have been delivered.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboxMarkSentArgs {
    /// Outbox row id.
    pub id: String,
    /// Must be true: this records a local decision and never proves delivery.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendDryRunArgs {
    /// Draft id returned by draft_save.
    pub draft: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// The query: words, phrases and fields such as from:, subject:, label:, is:, has:, after:.
    /// Bare terms search every indexed field; `*` marks a prefix; AND/OR/NOT are uppercase.
    pub query: String,
    /// Restrict to one account. Required when the mailbox has several accounts.
    pub account: Option<String>,
    /// Restrict to one folder id or exact folder name.
    pub folder: Option<String>,
    /// Restrict to messages carrying this label id or exact label name.
    pub label: Option<String>,
    /// Maximum hits, 1-200. Defaults to 50.
    pub limit: Option<u32>,
    /// Row offset, 0-5000.
    pub offset: Option<u32>,
    /// Only unread messages, from the same SQL/FTS scope as the page.
    #[serde(default)]
    pub unread_only: bool,
    /// Only messages with attachments, from the same scope.
    #[serde(default)]
    pub with_attachments: bool,
    /// relevance (default) or date.
    pub sort: Option<SearchSort>,
}

/// Search result order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// bm25 rank, then newest first.
    #[default]
    Relevance,
    /// Newest first.
    Date,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchStatusArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchReindexArgs {}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadsListArgs {
    /// Account id. Required when the mailbox has several accounts.
    pub account: Option<String>,
    /// Folder id or exact folder name. `starred` is the virtual starred view.
    pub folder: Option<String>,
    /// Label id or exact label name.
    pub label: Option<String>,
    /// Only threads with at least one unread message in scope.
    #[serde(default)]
    pub unread_only: bool,
    /// Only threads with at least one starred message in scope.
    #[serde(default)]
    pub starred_only: bool,
    /// Only threads with at least one message carrying an attachment in scope.
    #[serde(default)]
    pub with_attachments: bool,
    /// Page size, 1-200. Defaults to 50.
    pub limit: Option<u32>,
    /// Rows to skip before the page, 0-5000. Defaults to 0.
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadGetArgs {
    /// Thread id from threads_list. An id merged into another still resolves.
    pub id: String,
    /// Account id. Required when the mailbox has several accounts.
    pub account: Option<String>,
    /// Message page size, 1-200. Defaults to 50.
    pub limit: Option<u32>,
    /// Messages to skip, 0-5000.
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadFlagArgs {
    pub id: String,
    pub account: Option<String>,
    /// New read state: true means unread.
    pub unread: Option<bool>,
    pub starred: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedSearchesListArgs {
    /// Account id. Required when the mailbox has several accounts.
    pub account: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedSearchCreateArgs {
    /// Account id. Required when the mailbox has several accounts.
    pub account: Option<String>,
    /// 1-48 characters, unique per account ignoring case, at most 12 rows per account.
    pub name: String,
    /// The documented search language, validated before it is stored.
    pub query: String,
    /// relevance (default) or date.
    pub sort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedSearchUpdateArgs {
    pub id: String,
    /// Account that owns the search. Required when the mailbox has several accounts.
    pub account: Option<String>,
    /// New name. Omit to keep the current one.
    pub name: Option<String>,
    /// New query. Omit to keep the current one.
    pub query: Option<String>,
    /// New sort. Omit to keep the current one.
    pub sort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedSearchDeleteArgs {
    pub id: String,
    /// Account that owns the search. Required when the mailbox has several accounts.
    pub account: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedSearchRecentClearArgs {
    /// Account id. Required when the mailbox has several accounts.
    pub account: Option<String>,
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

/// Add an account. Sensitive: never exposed over MCP.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountAddArgs {
    /// Offline provider: aruba, gmail or custom.
    pub provider: Provider,
    /// The account address; it is also the login name.
    pub email: String,
    /// Display name. Omit it to use the local part of the address.
    pub name: Option<String>,
    /// Password or app password. Read from the caller, never logged and never written to the
    /// database; it is stored in the OS keyring only after the server accepts it.
    pub password: String,
    /// Only for provider custom; known providers own their endpoint.
    pub host: Option<String>,
    /// Only for provider custom; 1-65535.
    pub port: Option<u16>,
    /// Only for provider custom: implicit or starttls. There is no plaintext mode.
    pub tls: Option<MailTls>,
    /// SMTP submission host. Known providers use their preset; custom accounts may omit the whole
    /// endpoint and then read mail without being able to send.
    pub smtp_host: Option<String>,
    /// SMTP submission port; required together with smtp_host and smtp_tls for a custom endpoint.
    pub smtp_port: Option<u16>,
    /// SMTP TLS mode: implicit or starttls. There is no plaintext mode.
    pub smtp_tls: Option<MailTls>,
    /// SMTP login when it differs from the address. Known providers use the address.
    pub smtp_username: Option<String>,
    /// SMTP password when it differs from the IMAP one. When omitted, the IMAP credential is used
    /// for submission too; the value is never logged or written to the database.
    pub smtp_password: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountUpdateArgs {
    /// Account id returned by accounts_list.
    pub id: String,
    /// SMTP submission host. Known providers accept only their preset; omit it with clear_smtp.
    pub smtp_host: Option<String>,
    /// SMTP submission port.
    pub smtp_port: Option<u16>,
    /// SMTP TLS mode: implicit or starttls.
    pub smtp_tls: Option<MailTls>,
    /// SMTP login when it differs from the address.
    pub smtp_username: Option<String>,
    /// A new SMTP password. Omit it to keep the existing SMTP secret or the IMAP fallback.
    pub smtp_password: Option<String>,
    /// Remove the SMTP endpoint and its separate credential.
    #[serde(default)]
    pub clear_smtp: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountTestArgs {
    /// Account id returned by accounts_list.
    pub id: String,
}

/// Remove an account and its cache. Sensitive and destructive: never exposed over MCP.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountRemoveArgs {
    /// Account id returned by accounts_list.
    pub id: String,
    /// Must be true. The confirmation is explicit because cached mail is deleted.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncNowArgs {
    /// Account id. Omit it when exactly one account exists.
    pub account: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncStatusArgs {
    /// Account id. Omit it for every account.
    pub account: Option<String>,
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
    pub accounts: Vec<AccountView>,
    /// Cache warnings, for example an unreadable `sync_status` value that was shown as null.
    #[serde(default)]
    pub warnings: Vec<String>,
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
pub struct DraftAttachmentResult {
    pub draft: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendQueued {
    #[serde(flatten)]
    pub queued: crate::outbox::SendQueued,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxAction {
    pub outbox: crate::outbox::OutboxView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxListResult {
    #[serde(flatten)]
    pub outbox: crate::outbox::OutboxList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunResult {
    #[serde(flatten)]
    pub dry_run: crate::outbox::SendDryRun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub message: MessageSummary,
    /// Plain text, at most 240 characters, never HTML.
    pub snippet: String,
    /// The query terms that occur in the snippet, in source order, at most eight.
    pub highlights: Vec<String>,
    /// The field that produced the hit, or `all` for a bare term.
    pub matched: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub query: String,
    /// Exact total up to 10 000; above that `total_capped` is true.
    pub total: usize,
    pub total_capped: bool,
    pub hits: Vec<SearchHit>,
    /// Deprecated alias of `hits[].message`, kept for one milestone so existing clients do not break.
    pub messages: Vec<MessageSummary>,
    pub took_ms: u64,
    /// Present while the index is not ready: state, counts and last error. `hits` is empty then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<SearchStatusResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchStatusResult {
    pub ready: bool,
    pub indexed: u32,
    pub total: u32,
    pub last_error: Option<String>,
    pub size_bytes: u64,
    pub state: String,
    pub updated_at: i64,
}

impl SearchStatusResult {
    pub fn from_store(status: postino_store::SearchIndexStatus) -> Self {
        Self {
            ready: status.ready,
            indexed: status.indexed,
            total: status.total,
            last_error: status.last_error,
            size_bytes: status.size_bytes,
            state: status.state,
            updated_at: status.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReindexResult {
    pub documents: u32,
    /// Thread cache rows rebuilt from the stored messages.
    pub threads: u32,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadsListResult {
    pub threads: Vec<crate::model::ThreadSummary>,
    pub total: u32,
    pub unread_threads: u32,
    pub unread_messages: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadGetResult {
    pub thread: crate::model::ThreadSummary,
    pub messages: Vec<MessageSummary>,
    pub total: u32,
    pub truncated: bool,
    pub resolved_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadFlagOutput {
    pub thread: crate::model::ThreadSummary,
    pub changed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchesListResult {
    pub searches: Vec<crate::model::SavedSearch>,
    pub recent: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchResult {
    pub search: crate::model::SavedSearch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchDeleteResult {
    pub search: crate::model::SavedSearch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchRecentClearResult {
    pub account: String,
    pub cleared: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountAdd {
    pub account: AccountView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountUpdate {
    pub account: AccountView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSignature {
    pub account: AccountView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTest {
    pub account: String,
    /// Selectable folders the server listed.
    pub folders: u32,
    /// Capabilities advertised after authentication, upper-cased.
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRemove {
    pub account: AccountView,
}

/// What one sync run did. A serialisable mirror of `postino_sync::SyncOutcome`; no password, host
/// name or message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub account: String,
    pub folders: u32,
    pub fetched: u32,
    pub updated_flags: u32,
    pub removed: u32,
    pub attachments_stored: u32,
    pub warnings: Vec<String>,
}

impl From<postino_sync::SyncOutcome> for SyncReport {
    fn from(outcome: postino_sync::SyncOutcome) -> Self {
        Self {
            account: outcome.account,
            folders: outcome.folders,
            fetched: outcome.fetched,
            updated_flags: outcome.updated_flags,
            removed: outcome.removed,
            attachments_stored: outcome.attachments_stored,
            warnings: outcome.warnings,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncNow {
    pub outcome: SyncReport,
}

/// One account's persisted sync health. `status` is null when the account has never synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatusView {
    pub account: String,
    pub email: String,
    pub status: Option<SyncStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatusReport {
    pub accounts: Vec<SyncStatusView>,
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
            let (accounts, warnings) = engine.account_views_report()?;
            value(AccountsList { accounts, warnings })
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
        "draft_attachment_add" => {
            let args: DraftAttachmentAddArgs = parse(args)?;
            if args.data_base64.len() > MAX_ATTACHMENT_BASE64_CHARS {
                return Err(OpError::InvalidArgs(format!(
                    "data_base64 is {} characters; the decoded payload must be at most {} bytes ({} base64 characters)",
                    args.data_base64.len(),
                    crate::engine::MAX_ATTACHMENT_BYTES,
                    MAX_ATTACHMENT_BASE64_CHARS
                )));
            }
            let data = base64::engine::general_purpose::STANDARD
                .decode(args.data_base64.as_bytes())
                .map_err(|_| OpError::InvalidArgs("data_base64 is not valid standard base64".to_owned()))?;
            let draft = engine.add_draft_attachment(&args.draft, &args.filename, args.mime.as_deref(), &data)?;
            value(DraftAttachmentResult { draft })
        }
        "draft_attachment_remove" => {
            let args: DraftAttachmentRemoveArgs = parse(args)?;
            let draft = engine.remove_draft_attachment(&args.draft, &args.attachment)?;
            value(DraftAttachmentResult { draft })
        }
        "message_send" => {
            let args: MessageSendArgs = parse(args)?;
            value(SendQueued { queued: engine.message_send(&args.draft)? })
        }
        "send_schedule" => {
            let args: SendScheduleArgs = parse(args)?;
            let send_at = chrono::DateTime::parse_from_rfc3339(args.send_at.trim())
                .map_err(|error| OpError::InvalidArgs(format!("send_at is not an RFC 3339 instant: {error}")))?
                .with_timezone(&chrono::Utc);
            value(SendQueued { queued: engine.send_schedule(&args.draft, send_at)? })
        }
        "outbox_list" => {
            let args: OutboxListArgs = parse(args)?;
            value(OutboxListResult { outbox: engine.outbox_list(args.account.as_deref(), args.limit.unwrap_or(500), args.offset.unwrap_or(0))? })
        }
        "outbox_cancel" => {
            let args: OutboxCancelArgs = parse(args)?;
            value(OutboxAction { outbox: engine.outbox_cancel(&args.id)? })
        }
        "outbox_retry" => {
            let args: OutboxRetryArgs = parse(args)?;
            value(OutboxAction { outbox: engine.outbox_retry(&args.id, args.confirm)? })
        }
        "outbox_mark_sent" => {
            let args: OutboxMarkSentArgs = parse(args)?;
            value(OutboxAction { outbox: engine.outbox_mark_sent(&args.id, args.confirm)? })
        }
        "send_dry_run" => {
            let args: SendDryRunArgs = parse(args)?;
            value(DryRunResult { dry_run: engine.send_dry_run(&args.draft)? })
        }
        "search" => {
            let args: SearchArgs = parse(args)?;
            search(engine, args)
        }
        "search_status" => {
            let _args: SearchStatusArgs = parse(args)?;
            search_status(engine)
        }
        "search_reindex" => {
            let _args: SearchReindexArgs = parse(args)?;
            search_reindex(engine)
        }
        "threads_list" => {
            let args: ThreadsListArgs = parse(args)?;
            threads_list(engine, args)
        }
        "thread_get" => {
            let args: ThreadGetArgs = parse(args)?;
            thread_get(engine, args)
        }
        "thread_flag" => {
            let args: ThreadFlagArgs = parse(args)?;
            let result = engine.thread_flag(args.account.as_deref(), &args.id, args.unread, args.starred)?;
            value(ThreadFlagOutput { thread: result.thread, changed: result.changed })
        }
        "saved_searches_list" => {
            let args: SavedSearchesListArgs = parse(args)?;
            engine.reload_saved_searches(args.account.as_deref())?;
            let searches = engine.saved_searches_list(args.account.as_deref())?;
            let recent = engine.recent_searches(args.account.as_deref())?;
            value(SavedSearchesListResult { searches, recent })
        }
        "saved_search_create" => {
            let args: SavedSearchCreateArgs = parse(args)?;
            let search = engine.create_saved_search(args.account.as_deref(), &args.name, &args.query, args.sort.as_deref())?;
            value(SavedSearchResult { search })
        }
        "saved_search_update" => {
            let args: SavedSearchUpdateArgs = parse(args)?;
            let search =
                engine.update_saved_search(args.account.as_deref(), &args.id, args.name.as_deref(), args.query.as_deref(), args.sort.as_deref())?;
            value(SavedSearchResult { search })
        }
        "saved_search_delete" => {
            let args: SavedSearchDeleteArgs = parse(args)?;
            let search = engine.delete_saved_search(args.account.as_deref(), &args.id)?;
            value(SavedSearchDeleteResult { search })
        }
        "saved_search_recent_clear" => {
            let args: SavedSearchRecentClearArgs = parse(args)?;
            let account = engine.requested_account_id(args.account.as_deref(), "saved_search_recent_clear")?;
            let cleared = engine.clear_recent_searches(Some(&account))?;
            value(SavedSearchRecentClearResult { account, cleared })
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
        "account_add" => {
            let args: AccountAddArgs = parse(args)?;
            let smtp = if args.smtp_host.is_some() || args.smtp_port.is_some() || args.smtp_tls.is_some() || args.smtp_username.is_some() {
                Some(crate::engine::SmtpSetup {
                    host: args.smtp_host.unwrap_or_default(),
                    port: args.smtp_port.unwrap_or(0),
                    tls: args.smtp_tls.unwrap_or(MailTls::Implicit),
                    username: args.smtp_username.unwrap_or_default(),
                    password: args.smtp_password.map(Zeroizing::new),
                })
            } else {
                None
            };
            let setup = AccountSetup {
                provider: args.provider,
                email: args.email,
                name: args.name,
                password: Zeroizing::new(args.password),
                host: args.host,
                port: args.port,
                tls: args.tls,
                smtp,
                extra_roots: Vec::new(),
            };
            value(AccountAdd { account: engine.add_account(setup)? })
        }
        "account_update" => {
            let args: AccountUpdateArgs = parse(args)?;
            let request = crate::engine::AccountUpdateRequest {
                id: args.id,
                smtp_host: args.smtp_host,
                smtp_port: args.smtp_port,
                smtp_tls: args.smtp_tls,
                smtp_username: args.smtp_username,
                smtp_password: args.smtp_password.map(Zeroizing::new),
                clear_smtp: args.clear_smtp,
            };
            value(AccountUpdate { account: engine.update_account(request)? })
        }
        "account_signature_set" => {
            let args: AccountSignatureArgs = parse(args)?;
            value(AccountSignature { account: engine.set_account_signature(&args.account, &args.signature)? })
        }
        "account_test" => {
            let args: AccountTestArgs = parse(args)?;
            value(engine.test_account(&args.id)?)
        }
        "account_remove" => {
            let args: AccountRemoveArgs = parse(args)?;
            value(AccountRemove { account: engine.remove_account(&args.id, args.confirm)? })
        }
        "sync_now" => {
            let args: SyncNowArgs = parse(args)?;
            value(SyncNow { outcome: engine.sync_now(args.account.as_deref())? })
        }
        "sync_status" => {
            let args: SyncStatusArgs = parse(args)?;
            value(SyncStatusReport { accounts: engine.sync_statuses(args.account.as_deref())? })
        }
        other => Err(OpError::UnknownOp(other.to_owned())),
    }
}

/// Merge a `draft_save` request into full draft content. On create absent fields are empty; on
/// update absent fields keep the current value and present fields (including empty ones) replace it.
fn draft_content(engine: &mut Engine, args: DraftSaveArgs) -> Result<Message, OpError> {
    let DraftSaveArgs { id, account, to, cc, bcc, subject, body_text, attachments, in_reply_to } = args;
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
                in_reply_to: in_reply_to.or_else(|| engine.draft_reply_link(id).map(str::to_owned)),
            }
        }
        None => DraftContent {
            to: to.unwrap_or_default(),
            cc: cc.unwrap_or_default(),
            bcc: bcc.unwrap_or_default(),
            subject: subject.unwrap_or_default(),
            body_text: body_text.unwrap_or_default(),
            attachments: attachments.map(draft_attachments).unwrap_or_default(),
            in_reply_to,
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
            return Err(unknown_account(&account));
        }
        folders.retain(|folder| folder.account_id == account);
    }
    value(FoldersList { folders })
}

fn find_folder<'a>(engine: &'a Engine, id: &str) -> Result<&'a Folder, OpError> {
    engine.folders().iter().find(|folder| folder.id == id).ok_or_else(|| unknown_folder(id))
}

fn find_inbox<'a>(engine: &'a Engine, account: Option<&str>) -> Result<&'a Folder, OpError> {
    engine
        .folders()
        .iter()
        .find(|folder| folder.kind == FolderKind::Inbox && account.is_none_or(|account| folder.account_id == account))
        .ok_or_else(|| unknown_folder("inbox"))
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
        return Err(unknown_label(label));
    }
    let folder = match args.folder.as_deref() {
        Some(id) => Some(find_folder(engine, id)?),
        // A label on its own is a mailbox-wide view; without either filter the list stays inbox-first.
        None if args.label.is_some() => None,
        None => Some(find_inbox(engine, args.account.as_deref())?),
    };
    if let (Some(account), Some(folder)) = (&args.account, folder)
        && folder.account_id != *account
    {
        return Err(OpError::InvalidArgs(format!(
            "folder {:?} belongs to account {:?}, not {account:?}; use a folder from the same account",
            folder.id, folder.account_id
        )));
    }
    if let Some(account) = &args.account
        && !engine.accounts().iter().any(|candidate| candidate.id == *account)
    {
        return Err(unknown_account(account));
    }
    let (limit, offset) = page_bounds(args.limit, args.offset);
    if let Some(store) = engine.store() {
        let filter = postino_store::MessageFilter {
            account_id: args.account.clone(),
            folder_id: folder.filter(|folder| !folder.kind.is_virtual()).map(|folder| folder.id.clone()),
            label_id: args.label.clone(),
            unread: if args.unread_only { Some(true) } else { None },
            starred: match folder {
                Some(folder) if folder.kind == FolderKind::Starred => Some(true),
                _ if args.starred_only => Some(true),
                _ => None,
            },
            has_attachments: if args.with_attachments { Some(true) } else { None },
            limit: u32::try_from(limit).unwrap_or(200),
            offset: u32::try_from(offset).unwrap_or(0),
        };
        let page = store.list_messages(&filter).map_err(store_map::read_error)?;
        // The virtual starred view is derived from flags; a real folder named starred is not a
        // mailbox. The store filter already asks for starred rows, so no second pass is needed.
        return value(MessagesList {
            folder: folder.map(|folder| folder.id.clone()),
            messages: page.messages.into_iter().map(store_map::summary).collect(),
            total: usize::try_from(page.total).unwrap_or(usize::MAX),
            unread: usize::try_from(page.unread).unwrap_or(usize::MAX),
        });
    }
    let account_folders = match (&args.account, folder) {
        (Some(account), None) => {
            Some(engine.folders().iter().filter(|folder| folder.account_id == *account).map(|folder| folder.id.as_str()).collect::<BTreeSet<_>>())
        }
        _ => None,
    };
    let mut rows: Vec<&MessageSummary> = engine
        .messages()
        .iter()
        .filter(|message| {
            let in_scope = match folder {
                Some(folder) if folder.kind == FolderKind::Starred => message.starred,
                Some(folder) => message.folder_id == folder.id,
                None => account_folders.as_ref().is_none_or(|folders| folders.contains(message.folder_id.as_str())),
            };
            in_scope
                && (!args.unread_only || message.unread)
                && (!args.starred_only || message.starred)
                && (!args.with_attachments || message.has_attachments)
                && args.label.as_ref().is_none_or(|label| message.labels.contains(label))
        })
        .collect();
    rows.sort_by_key(|left| std::cmp::Reverse(left.date));

    let total = rows.len();
    let unread = rows.iter().filter(|message| message.unread).count();
    let messages = rows.into_iter().skip(offset).take(limit).cloned().collect();
    value(MessagesList { folder: folder.map(|folder| folder.id.clone()), messages, total, unread })
}

fn message(engine: &Engine, args: MessageGetArgs) -> Result<Value, OpError> {
    let message = engine.message_result(&args.id)?.ok_or_else(|| unknown_message(&args.id))?;
    value(MessageGet { message })
}

fn flag(engine: &mut Engine, args: MessageFlagArgs) -> Result<Value, OpError> {
    let message = engine.set_flags(&args.id, args.unread, args.starred)?;
    value(MessageFlag { message })
}

fn search(engine: &Engine, args: SearchArgs) -> Result<Value, OpError> {
    value(execute_search(engine, args)?)
}

/// The one search execution path shared by the `search` operation and the saved-search counts.
pub(crate) fn execute_search(engine: &Engine, args: SearchArgs) -> Result<SearchResult, OpError> {
    let parsed = crate::search::parse(&args.query)
        .map_err(|error| OpError::InvalidArgs(format!("search query: {} at position {}", error.code, error.position)))?;
    let account = search_account(engine, args.account.as_deref())?;
    let mut filters = Vec::new();
    let mut folder_filter = None;
    let mut label_filter = None;
    if let Some(folder) = args.folder.as_deref() {
        let folder = resolve_folder(engine, folder, account.as_deref())?;
        folder_filter = Some(folder.clone());
        filters.push(folder_expr(engine, folder));
    }
    if let Some(label) = args.label.as_deref() {
        let label = resolve_label(engine, label)?;
        label_filter = Some(label.id.clone());
        filters.push(postino_store::SearchExpr::Label(label.id));
    }
    let expression = translate_search(&parsed.expr, engine, account.as_deref())?;
    if args.unread_only {
        filters.push(postino_store::SearchExpr::Unread(true));
    }
    if args.with_attachments {
        filters.push(postino_store::SearchExpr::HasAttachments(true));
    }
    filters.push(expression);
    let expression = match filters.len() {
        0 => None,
        1 => filters.pop(),
        _ => Some(postino_store::SearchExpr::And(filters)),
    };
    let limit = args.limit.unwrap_or(50).clamp(1, 200);
    let offset = args.offset.unwrap_or(0).min(5_000);
    let sort = match args.sort.unwrap_or_default() {
        SearchSort::Relevance => postino_store::SearchSort::Relevance,
        SearchSort::Date => postino_store::SearchSort::Date,
    };
    let started = std::time::Instant::now();
    if let Some(store) = engine.store()
        && expression_uses_fts(expression.as_ref())
        && !store.search_index_ready().map_err(store_map::read_error)?
    {
        // One resumable batch per search. A pending v7 index must not block opening the mailbox or
        // the first search for the whole rebuild; the UI shows the progress carried in `index`.
        let _ = store.ensure_search_index_batch();
        let status = store.search_status().map_err(store_map::read_error)?;
        if !status.ready {
            return Ok(SearchResult {
                query: parsed.text,
                total: 0,
                total_capped: false,
                hits: Vec::new(),
                messages: Vec::new(),
                took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                index: Some(SearchStatusResult::from_store(status)),
            });
        }
    }
    let (hits, total, total_capped) = match engine.store() {
        Some(store) => {
            // With one account every row is in scope; omitting the account filter lets the fast
            // relevance path count inside FTS5 without a message-row lookup per match. With several
            // accounts the caller's account is part of the query.
            let store_account = if engine.accounts().len() > 1 { account.clone() } else { None };
            let page = store
                .search(&postino_store::SearchFilter { account_id: store_account, expr: expression, sort, limit, offset })
                .map_err(store_map::read_error)?;
            let terms = parsed.expr.terms();
            let hits = page
                .hits
                .into_iter()
                .map(|hit| {
                    let snippet = crate::search::make_snippet(&hit.message.subject, &hit.body_text, &terms);
                    SearchHit {
                        message: store_map::summary(hit.message),
                        snippet: snippet.text,
                        highlights: snippet.highlights,
                        matched: matched_label(&parsed.expr),
                    }
                })
                .collect::<Vec<_>>();
            (hits, usize::try_from(page.total).unwrap_or(usize::MAX), page.total_capped)
        }
        None => demo_search(engine, &parsed.expr, limit, offset, label_filter.as_deref(), folder_filter.as_deref(), account.as_deref())?,
    };
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let messages = hits.iter().map(|hit| hit.message.clone()).collect();
    Ok(SearchResult { query: parsed.text, total, total_capped, hits, messages, took_ms, index: None })
}

fn expression_uses_fts(expression: Option<&postino_store::SearchExpr>) -> bool {
    fn walk(expr: &postino_store::SearchExpr) -> bool {
        match expr {
            postino_store::SearchExpr::Match(_) => true,
            postino_store::SearchExpr::And(children) | postino_store::SearchExpr::Or(children) => children.iter().any(walk),
            postino_store::SearchExpr::Not(child) => walk(child),
            _ => false,
        }
    }
    expression.is_some_and(walk)
}

fn search_status(engine: &Engine) -> Result<Value, OpError> {
    let store = engine.require_store_for_read("search_status")?;
    let status = store.search_status().map_err(store_map::read_error)?;
    value(SearchStatusResult::from_store(status))
}

fn search_reindex(engine: &Engine) -> Result<Value, OpError> {
    let store = engine.require_store_for_read("search_reindex")?;
    let outcome = store.search_reindex().map_err(store_map::read_error)?;
    let threads = store.refresh_all_threads().map_err(store_map::read_error)?;
    value(SearchReindexResult { documents: outcome.documents, threads, took_ms: outcome.took_ms })
}

fn threads_list(engine: &Engine, args: ThreadsListArgs) -> Result<Value, OpError> {
    let request = crate::engine::ThreadListRequest {
        account: args.account,
        folder: args.folder,
        label: args.label,
        unread_only: args.unread_only,
        starred_only: args.starred_only,
        with_attachments: args.with_attachments,
        limit: args.limit,
        offset: args.offset,
    };
    let page = engine.threads_list(&request)?;
    value(ThreadsListResult { threads: page.threads, total: page.total, unread_threads: page.unread_threads, unread_messages: page.unread_messages })
}

fn thread_get(engine: &Engine, args: ThreadGetArgs) -> Result<Value, OpError> {
    let detail = engine.thread_get(args.account.as_deref(), &args.id, args.limit, args.offset)?;
    value(ThreadGetResult {
        thread: detail.thread,
        messages: detail.messages,
        total: detail.total,
        truncated: detail.truncated,
        resolved_id: detail.resolved_id,
    })
}

/// Parse and resolve a saved query without executing it. The registration path uses this so an
/// unknown label or folder is refused at save time instead of appearing as a valid but empty row.
pub(crate) fn validate_search_query(engine: &Engine, query: &str, account: Option<&str>) -> Result<(), OpError> {
    let parsed =
        crate::search::parse(query).map_err(|error| OpError::InvalidArgs(format!("search query: {} at position {}", error.code, error.position)))?;
    translate_search(&parsed.expr, engine, account)?;
    Ok(())
}

/// Resolve the account scope of a search: one account is implied, several require an explicit id.
fn search_account(engine: &Engine, requested: Option<&str>) -> Result<Option<String>, OpError> {
    if let Some(id) = requested {
        if !engine.accounts().iter().any(|account| account.id == id) {
            return Err(unknown_account(id));
        }
        return Ok(Some(id.to_owned()));
    }
    match engine.accounts() {
        [] => Ok(None),
        [account] => Ok(Some(account.id.clone())),
        accounts => Err(OpError::InvalidArgs(format!(
            "the mailbox has {} accounts; pass `account` with one of {}",
            accounts.len(),
            accounts.iter().map(|account| format!("{:?}", account.id)).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn resolve_folder(engine: &Engine, value: &str, account: Option<&str>) -> Result<String, OpError> {
    let folder = engine
        .folders()
        .iter()
        .find(|folder| folder.id == value && account.is_none_or(|account| folder.account_id == account))
        .or_else(|| {
            let wanted = value.to_lowercase();
            engine.folders().iter().find(|folder| account.is_none_or(|account| folder.account_id == account) && folder.name.to_lowercase() == wanted)
        })
        .ok_or_else(|| unknown_folder(value))?;
    if let Some(account) = account
        && folder.account_id != account
    {
        return Err(OpError::InvalidArgs(format!(
            "folder {:?} belongs to account {:?}, not {account:?}; use a folder from the same account",
            folder.id, folder.account_id
        )));
    }
    Ok(folder.id.clone())
}

fn folder_expr(engine: &Engine, id: String) -> postino_store::SearchExpr {
    if engine.folders().iter().any(|folder| folder.id == id && folder.kind == FolderKind::Starred) {
        postino_store::SearchExpr::Starred(true)
    } else {
        postino_store::SearchExpr::Folder(id)
    }
}

fn resolve_label(engine: &Engine, value: &str) -> Result<Label, OpError> {
    let wanted = value.to_lowercase();
    engine
        .labels()
        .iter()
        .find(|label| label.id == value)
        .or_else(|| engine.labels().iter().find(|label| label.name.to_lowercase() == wanted))
        .cloned()
        .ok_or_else(|| unknown_label(value))
}

/// Translate the parsed AST into the store's typed filter tree. Text leaves become a safe FTS5
/// phrase; label and folder names become ids here, in core, so the store never sees a name lookup.
fn translate_search(expr: &crate::search::Expr, engine: &Engine, account: Option<&str>) -> Result<postino_store::SearchExpr, OpError> {
    use crate::search::{Expr, Field};
    Ok(match expr {
        Expr::And(children) => {
            postino_store::SearchExpr::And(children.iter().map(|child| translate_search(child, engine, account)).collect::<Result<Vec<_>, _>>()?)
        }
        Expr::Or(children) => {
            postino_store::SearchExpr::Or(children.iter().map(|child| translate_search(child, engine, account)).collect::<Result<Vec<_>, _>>()?)
        }
        Expr::Not(child) => postino_store::SearchExpr::Not(Box::new(translate_search(child, engine, account)?)),
        Expr::Text { field, value, prefix } => {
            let column = match field {
                Some(Field::From) => Some("search_sender"),
                Some(Field::To) => Some("search_to"),
                Some(Field::Cc) => Some("search_cc"),
                Some(Field::Bcc) => Some("search_bcc"),
                Some(Field::Subject) => Some("subject"),
                _ => None,
            };
            postino_store::SearchExpr::Match(crate::search::fts_phrase(column, value, *prefix))
        }
        Expr::Label(value) => postino_store::SearchExpr::Label(resolve_label(engine, value)?.id),
        Expr::Folder(value) => folder_expr(engine, resolve_folder(engine, value, account)?),
        Expr::Is(value) => match value {
            crate::search::IsFilter::Unread => postino_store::SearchExpr::Unread(true),
            crate::search::IsFilter::Read => postino_store::SearchExpr::Unread(false),
            crate::search::IsFilter::Starred => postino_store::SearchExpr::Starred(true),
            crate::search::IsFilter::Unstarred => postino_store::SearchExpr::Starred(false),
        },
        Expr::HasAttachment => postino_store::SearchExpr::HasAttachments(true),
        Expr::After(instant) => postino_store::SearchExpr::After(*instant),
        Expr::Before(instant) => postino_store::SearchExpr::Before(*instant),
        Expr::Larger(bytes) => postino_store::SearchExpr::Larger(*bytes),
        Expr::Smaller(bytes) => postino_store::SearchExpr::Smaller(*bytes),
    })
}

fn matched_label(expr: &crate::search::Expr) -> String {
    use crate::search::Expr;
    fn collect(expr: &Expr, fields: &mut BTreeSet<&'static str>, bare: &mut bool, filters: &mut bool) {
        match expr {
            Expr::Text { field: Some(field), .. } => {
                fields.insert(field.as_str());
            }
            Expr::Text { field: None, .. } => *bare = true,
            Expr::And(children) | Expr::Or(children) => children.iter().for_each(|child| collect(child, fields, bare, filters)),
            Expr::Not(child) => collect(child, fields, bare, filters),
            _ => *filters = true,
        }
    }
    let mut fields = BTreeSet::new();
    let mut bare = false;
    let mut filters = false;
    collect(expr, &mut fields, &mut bare, &mut filters);
    let mut parts = fields.into_iter().map(str::to_owned).collect::<Vec<_>>();
    if bare {
        parts.push("all".to_owned());
    }
    if filters || parts.is_empty() {
        parts.push("filter".to_owned());
    }
    parts.join(", ")
}

/// The demo mailbox has no FTS table: evaluate the AST over its full in-memory messages, then sort
/// by date. The operation surface and the result shape are identical to the store-backed path.
fn demo_search(
    engine: &Engine,
    expr: &crate::search::Expr,
    limit: u32,
    offset: u32,
    label: Option<&str>,
    folder: Option<&str>,
    account: Option<&str>,
) -> Result<(Vec<SearchHit>, usize, bool), OpError> {
    let terms = expr.terms();
    let account_folders = account.map(|account| {
        engine.folders().iter().filter(|folder| folder.account_id == account).map(|folder| folder.id.as_str()).collect::<BTreeSet<_>>()
    });
    let starred_folder = folder.is_some_and(|id| engine.folders().iter().any(|folder| folder.id == id && folder.kind == FolderKind::Starred));
    let mut rows = engine
        .messages()
        .iter()
        .filter(|summary| {
            label.is_none_or(|label| summary.labels.iter().any(|candidate| candidate == label))
                && folder.is_none_or(|folder| if starred_folder { summary.starred } else { summary.folder_id == folder })
                && account_folders.as_ref().is_none_or(|folders| folders.contains(summary.folder_id.as_str()))
        })
        .filter_map(|summary| engine.message(&summary.id).map(|message| (summary, message)))
        .filter(|(_, message)| demo_matches(expr, message, engine))
        .collect::<Vec<_>>();
    rows.sort_by_key(|(summary, _)| std::cmp::Reverse(summary.date));
    let total = rows.len();
    let hits = rows
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(50))
        .map(|(summary, message)| {
            let snippet = crate::search::make_snippet(&message.summary.subject, &message.body_text, &terms);
            SearchHit { message: summary.clone(), snippet: snippet.text, highlights: snippet.highlights, matched: matched_label(expr) }
        })
        .collect::<Vec<_>>();
    Ok((hits, total, total > 10_000))
}

fn demo_matches(expr: &crate::search::Expr, message: &Message, engine: &Engine) -> bool {
    use crate::search::Expr;
    match expr {
        Expr::And(children) => children.iter().all(|child| demo_matches(child, message, engine)),
        Expr::Or(children) => children.iter().any(|child| demo_matches(child, message, engine)),
        Expr::Not(child) => !demo_matches(child, message, engine),
        Expr::Text { field, value, prefix } => {
            let needle = value.to_lowercase();
            let matches = |haystack: &str| {
                let haystack = haystack.to_lowercase();
                if *prefix {
                    haystack.split(|character: char| !character.is_alphanumeric()).any(|word| word.starts_with(&needle))
                } else {
                    haystack.contains(&needle)
                }
            };
            match field {
                Some(crate::search::Field::From) => message.summary.from.display().to_lowercase().contains(&needle),
                Some(crate::search::Field::To) => message.summary.to.iter().any(|address| address.display().to_lowercase().contains(&needle)),
                Some(crate::search::Field::Cc) => message.cc.iter().any(|address| address.display().to_lowercase().contains(&needle)),
                Some(crate::search::Field::Bcc) => message.bcc.iter().any(|address| address.display().to_lowercase().contains(&needle)),
                Some(crate::search::Field::Subject) => matches(&message.summary.subject),
                _ => {
                    matches(&message.summary.subject)
                        || matches(&message.body_text)
                        || matches(&message.summary.from.display())
                        || message.summary.to.iter().any(|address| matches(&address.display()))
                        || message.cc.iter().any(|address| matches(&address.display()))
                        || message.bcc.iter().any(|address| matches(&address.display()))
                        || message.attachments.iter().any(|attachment| matches(&attachment.filename))
                }
            }
        }
        Expr::Label(value) => resolve_label(engine, value).is_ok_and(|label| message.summary.labels.contains(&label.id)),
        Expr::Folder(value) => resolve_folder(engine, value, None).is_ok_and(|folder| {
            if engine.folders().iter().any(|candidate| candidate.id == folder && candidate.kind == FolderKind::Starred) {
                message.summary.starred
            } else {
                message.summary.folder_id == folder
            }
        }),
        Expr::Is(value) => match value {
            crate::search::IsFilter::Unread => message.summary.unread,
            crate::search::IsFilter::Read => !message.summary.unread,
            crate::search::IsFilter::Starred => message.summary.starred,
            crate::search::IsFilter::Unstarred => !message.summary.starred,
        },
        Expr::HasAttachment => message.summary.has_attachments,
        Expr::After(instant) => message.summary.date.timestamp_millis() >= *instant,
        Expr::Before(instant) => message.summary.date.timestamp_millis() < *instant,
        // The demo messages carry no RFC 822 size; a size filter matches nothing rather than
        // pretending a body length is a server size.
        Expr::Larger(_) | Expr::Smaller(_) => false,
    }
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
                    remote_images: 0,
                    attachments: Vec::new(),
                }
            })
            .collect();
        let labels = vec![Label { id: "lbl_1".to_owned(), name: "Archivio".to_owned(), color: LabelColor::Forest }];
        Engine::from_parts(vec![account], vec![folder], messages, labels, false, None)
    }

    #[test]
    fn send_operations_have_the_documented_surface() {
        let defs = super::op_defs();
        let find = |name: &str| defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("{name} is missing"));
        for name in ["message_send", "send_schedule", "outbox_cancel", "outbox_retry", "outbox_mark_sent"] {
            let def = find(name);
            assert!(!def.read_only, "{name} is a write");
            assert!(!def.destructive, "{name} must not claim to be destructive");
            assert!(!def.sensitive, "{name} takes no password");
        }
        for name in ["outbox_list", "send_dry_run"] {
            assert!(find(name).read_only, "{name} must be read-only");
        }
        let send = find("message_send");
        assert!(send.input_schema["properties"]["draft"].is_object(), "message_send schema has no draft field");
        let dry = find("send_dry_run");
        assert!(dry.description.contains("without writing a queue row"), "{}", dry.description);
        assert!(!defs.iter().any(|def| def.name == "draft_send"), "the local-only draft_send operation must be gone");
    }

    #[test]
    fn compose_depth_operations_have_the_documented_surface() {
        let defs = super::op_defs();
        let find = |name: &str| defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("{name} is missing"));
        for name in ["draft_attachment_add", "draft_attachment_remove", "account_signature_set"] {
            let def = find(name);
            assert!(!def.read_only, "{name} is a write");
            assert!(!def.destructive, "{name} must not claim to be destructive");
            assert!(!def.sensitive, "{name} takes no credential");
            assert!(def.input_schema.is_object(), "{name} has no object schema");
        }
        let add = find("draft_attachment_add");
        let properties = add.input_schema["properties"].as_object().expect("properties");
        assert!(properties.contains_key("data_base64"), "{properties:?}");
        assert!(!properties.contains_key("path"), "the operation must never accept a filesystem path");
        assert!(add.description.contains("encrypted"), "{}", add.description);
        assert!(add.description.contains("25 MiB"), "{}", add.description);
        let signature = find("account_signature_set");
        assert!(signature.description.contains("4 KiB"), "{}", signature.description);
        let save = find("draft_save");
        assert!(save.input_schema["properties"]["in_reply_to"].is_object(), "draft_save has no in_reply_to field");
        assert!(save.description.contains("metadata only"), "{}", save.description);
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
        let find = |name: &str| defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("{name} is missing"));
        assert!(find("search").read_only);
        assert!(find("search_status").read_only);
        assert!(!find("search_reindex").read_only);
        assert!(!find("search_reindex").destructive);
        for name in ["search", "search_status", "search_reindex"] {
            assert!(!find(name).sensitive, "{name} handles no credential");
        }
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
        assert!(matches!(unknown, crate::OpError::InvalidArgs(_)), "{unknown}");
    }

    #[test]
    fn flag_requires_a_change() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].id.clone();
        let error = engine.call("message_flag", json!({ "id": id })).expect_err("no flags");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn unknown_folder_fails() {
        let mut engine = Engine::demo();
        let error = engine.call("messages_list", json!({ "folder": "nope" })).expect_err("folder");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)), "{error}");
    }

    #[test]
    fn empty_search_is_refused() {
        let mut engine = Engine::demo();
        let error = engine.call("search", json!({ "query": "   " })).expect_err("query");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn every_unknown_id_path_is_invalid_args() {
        let mut engine = Engine::demo();
        let message = engine.messages()[0].id.clone();
        let cases = [
            ("folders_list", json!({ "account": "acc_missing" })),
            ("messages_list", json!({ "folder": "missing" })),
            ("messages_list", json!({ "label": "lbl_missing" })),
            ("messages_list", json!({ "label": "lbl_1", "account": "acc_missing" })),
            ("messages_list", json!({ "folder": "inbox", "account": "acc_missing" })),
            ("search", json!({ "query": "posta", "folder": "missing" })),
            ("search", json!({ "query": "posta", "label": "lbl_missing" })),
            ("message_get", json!({ "id": "missing" })),
            ("message_flag", json!({ "id": "missing", "unread": true })),
            ("message_move", json!({ "id": "missing", "folder": "archive" })),
            ("message_move", json!({ "id": message, "folder": "missing" })),
            ("label_update", json!({ "id": "missing", "name": "X" })),
            ("label_delete", json!({ "id": "missing" })),
            ("label_apply", json!({ "id": "missing", "message_ids": [message], "apply": true })),
            ("label_apply", json!({ "id": "lbl_1", "message_ids": ["missing"], "apply": true })),
        ];
        for (name, args) in cases {
            let error = engine.call(name, args.clone()).expect_err("an unknown id must be refused");
            assert!(matches!(error, crate::OpError::InvalidArgs(_)), "{name} {args} -> {error}");
            assert!(!error.to_string().contains("free disk space"), "{name} {args} reports a caller mistake as a storage failure");
        }
    }

    #[test]
    fn moving_a_message_updates_both_folders() {
        let mut engine = Engine::demo();
        let id = engine.messages()[0].id.clone();
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
        let id = engine.messages()[0].id.clone();
        let error = engine.call("message_move", json!({ "id": id, "folder": "starred" })).expect_err("virtual folder");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)));
    }

    #[test]
    fn moving_an_unknown_message_fails() {
        let mut engine = Engine::demo();
        let error = engine.call("message_move", json!({ "id": "missing", "folder": "archive" })).expect_err("unknown message");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)), "{error}");
    }

    #[test]
    fn draft_save_and_local_sent_move_work() {
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
        assert!(engine.call("message_send", json!({ "draft": id })).is_err(), "the demo engine has no SMTP account and must not claim a submission");
        let sent = engine.send_draft(&id).expect("local sent move");
        assert_eq!(sent.folder_id, "sent");
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
        assert!(matches!(engine.call("label_update", json!({ "id": id, "name": "X" })), Err(crate::OpError::InvalidArgs(_))));
    }

    #[test]
    fn deleting_a_label_detaches_it_from_every_message() {
        let mut engine = Engine::demo();
        let result = call(&mut engine, "label_delete", json!({ "id": "lbl_1" }));
        assert_eq!(result["label"]["id"], "lbl_1");
        assert_eq!(result["affected"], 6);
        assert!(engine.messages().iter().all(|message| !message.labels.iter().any(|label| label == "lbl_1")));
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
        assert!(matches!(error, crate::OpError::InvalidArgs(_)), "{error}");
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
            Err(crate::OpError::InvalidArgs(_))
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
        assert!(matches!(error, crate::OpError::InvalidArgs(_)), "{error}");
        let error = engine.call("messages_list", json!({ "folder": "inbox", "label": "lbl_missing" })).expect_err("unknown label with folder");
        assert!(matches!(error, crate::OpError::InvalidArgs(_)), "{error}");
    }

    #[test]
    fn account_and_sync_operations_carry_the_documented_annotations() {
        let defs = super::op_defs();
        let by_name = |name: &str| defs.iter().find(|def| def.name == name).unwrap_or_else(|| panic!("missing operation {name}"));
        for name in ["accounts_list", "account_test", "sync_status"] {
            assert!(by_name(name).read_only, "{name} must be read-only");
            assert!(!by_name(name).destructive, "{name} must not be destructive");
            assert!(!by_name(name).sensitive, "{name} must stay available over MCP");
        }
        assert!(by_name("account_add").sensitive);
        assert!(!by_name("account_add").read_only);
        assert!(by_name("account_remove").sensitive);
        assert!(by_name("account_remove").destructive);
        assert!(!by_name("sync_now").read_only);
        assert!(by_name("account_add").description.contains("never logged"));
        assert!(by_name("account_add").description.contains("app password"));
        assert!(by_name("account_add").input_schema["properties"]["password"].is_object());
        assert!(by_name("account_remove").input_schema["properties"]["confirm"].is_object());
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
