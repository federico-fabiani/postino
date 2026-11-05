//! Postino core: the domain model and the single operation registry shared by the desktop app,
//! the CLI and the MCP server.
//!
//! The rule is the same as the Crafting Apps: every user-visible action is an operation with a
//! typed request, a JSON Schema and a handler. A frontend never reaches past [`Engine`].
//!
//! The engine runs on synthetic demo mail in memory or on the encrypted local store; IMAP, SMTP
//! submission and the outbox are implemented (P5-P7), the daemon arrives in M4. P6a adds the account
//! lifecycle: credentials live behind the [`CredentialVault`] seam (the OS keyring in production),
//! IMAP/SMTP settings live in the encrypted store, and `sync_now` / `message_send` are the operations
//! that speak to a server.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod demo;
pub mod engine;
pub mod model;
pub mod oauth;
pub mod ops;
pub mod outbox;
pub mod pgp;
pub mod pgp_mime;
mod reply;
mod rules;
pub mod search;
mod store_map;
mod unsubscribe;
pub mod update;
pub mod vault;

pub use engine::{
    AccountRequest, AccountSetup, AccountUpdateRequest, Engine, FolderMboxExport, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_TOTAL_BYTES,
    MAX_DRAFT_ATTACHMENTS, MAX_EML_BYTES, MAX_MBOX_BYTES, MAX_MBOX_MESSAGES, MAX_SIGNATURE_BYTES, OpError, ReplyTarget, SmtpSetup,
    ThreadDetailResult, ThreadFlagResult, ThreadListRequest, ThreadPageResult,
};
pub use model::{
    Account, AccountView, Address, Attachment, BodyFormat, CalendarInviteView, DraftContent, Folder, FolderKind, Identity, InviteResponse, Label,
    LabelColor, LabelSummary, MailTls, Message, MessageSummary, MessageTemplate, PgpKey, PgpPublicKey, Provider, Rule, RuleAction, RuleEffectStatus,
    RulePreviewEffect, RulePreviewRow, RuleRun, RuleRunScope, SavedSearch, ServerSettings, SmtpSettings, ThreadSummary, UnsubscribeInfo,
    UnsubscribeMethod, UnsubscribeTarget,
};
pub use oauth::{Authorized, OAuthAccess, OAuthGrant, authorize};
pub use ops::{
    AccountAdd, AccountRemove, AccountSignature, AccountTest, AccountUpdate, AccountsList, AppInfo, AttachmentGet, DraftAttachmentResult, DraftSave,
    DryRunResult, FolderExportMbox, FolderImportMbox, FoldersList, IdentityDeleteResult, LabelApply, LabelCreate, LabelDelete, LabelUpdate,
    LabelsList, MessageExport, MessageFlag, MessageGet, MessageImport, MessagesList, OpDef, OutboxAction, OutboxListResult, RuleDeleteResult,
    RuleResult, RulesListResult, RulesPreviewResult, RulesRunResult, RulesRunsResult, RulesUndoResult, SavedSearchDeleteResult,
    SavedSearchRecentClearResult, SavedSearchResult, SavedSearchesListResult, SearchHit, SearchReindexResult, SearchResult, SearchSort,
    SearchStatusResult, SendQueued, SyncNow, SyncReport, SyncStatusReport, SyncStatusView, TemplateDeleteResult, ThreadFlagOutput, ThreadGetResult,
    ThreadsListResult, op_defs,
};
pub use outbox::{OutboxHalt, OutboxList, OutboxRunReport, OutboxView, SendDryRun, SendEnvelope};
pub use pgp_mime::{
    MAX_PGP_RECIPIENTS, MAX_RAW_MESSAGE_BYTES, PgpDecryptResult, PgpDecryptState, PgpMimeKind, PgpSendError, PgpSendOptions, PgpVerifyResult,
    PgpVerifyState,
};
pub use postino_sync::{SyncPhase, SyncStatus};
pub use reply::{ReplyMode, ReplyPrefill, compose_reply, forward_subject, new_message_body, quote_body, reply_subject, signature_block};
pub use rules::{
    MAX_FORWARD_RECIPIENTS, MAX_KEPT_RUNS, MAX_PREVIEW_ROWS, MAX_RULE_ACTIONS, MAX_RULES, MAX_RUN_FORWARDS, MAX_RUN_MESSAGES, RuleRunOutcome,
    RuleUndoOutcome,
};
pub use search::{Expr as SearchExpr, Field as SearchField, IsFilter, ParsedQuery, SearchError, Snippet, make_snippet, parse as parse_search};
pub use update::{RELEASES_PAGE, Release, ReleaseError, UpdateStatus};
pub use vault::{CredentialVault, VaultError};
