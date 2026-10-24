//! Postino core: the domain model and the single operation registry shared by the desktop app,
//! the CLI and the MCP server.
//!
//! The rule is the same as the Crafting Apps: every user-visible action is an operation with a
//! typed request, a JSON Schema and a handler. A frontend never reaches past [`Engine`].
//!
//! M0 is a walking skeleton: the engine holds synthetic demo mail in memory. IMAP, SMTP, SQLite
//! and the daemon arrive in M1-M4. The operation surface is intentionally small and honest.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod demo;
pub mod engine;
pub mod model;
pub mod ops;

pub use engine::{Engine, OpError};
pub use model::{Account, Address, Attachment, DraftContent, Folder, FolderKind, Message, MessageSummary};
pub use ops::{AccountsList, AppInfo, DraftSave, DraftSend, FoldersList, MessageFlag, MessageGet, MessagesList, OpDef, SearchResult, op_defs};
