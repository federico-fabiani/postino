//! Postino core: the domain model and the single operation registry shared by the desktop app,
//! the CLI and the MCP server.
//!
//! The rule is the same as the Crafting Apps: every user-visible action is an operation with a
//! typed request, a JSON Schema and a handler. A frontend never reaches past [`Engine`].
//!
//! The engine runs on synthetic demo mail in memory or on the encrypted local store; IMAP, SMTP
//! and the daemon arrive in M1-M4. P6a adds the account lifecycle: credentials live behind the
//! [`CredentialVault`] seam (the OS keyring in production), IMAP settings live in the encrypted
//! store, and `sync_now` is the only operation that speaks to a server.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod demo;
pub mod engine;
pub mod model;
pub mod ops;
mod store_map;
pub mod vault;

pub use engine::{AccountSetup, Engine, OpError};
pub use model::{
    Account, AccountView, Address, Attachment, DraftContent, Folder, FolderKind, Label, LabelColor, LabelSummary, MailTls, Message, MessageSummary,
    Provider, ServerSettings,
};
pub use ops::{
    AccountAdd, AccountRemove, AccountTest, AccountsList, AppInfo, DraftSave, DraftSend, FoldersList, LabelApply, LabelCreate, LabelDelete,
    LabelUpdate, LabelsList, MessageFlag, MessageGet, MessagesList, OpDef, SearchResult, SyncNow, SyncReport, SyncStatusReport, SyncStatusView,
    op_defs,
};
pub use postino_sync::{SyncPhase, SyncStatus};
pub use vault::{CredentialVault, VaultError};
