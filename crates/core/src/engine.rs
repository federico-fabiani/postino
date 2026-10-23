//! The engine owns the mailbox data. M0 keeps it in memory; M1 replaces the storage behind the
//! same accessors with SQLite.

use std::path::PathBuf;

use serde_json::Value;

use crate::model::{Account, Folder, FolderKind, Message, MessageSummary};
use crate::ops::{self, OpDef};

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
    demo: bool,
}

impl Engine {
    /// An engine with no accounts. Every list is empty until a store or demo data is attached.
    pub fn empty() -> Self {
        Self { accounts: Vec::new(), folders: Vec::new(), messages: Vec::new(), demo: false }
    }

    /// The M0 walking skeleton: a synthetic account with realistic mail.
    pub fn demo() -> Self {
        crate::demo::seed()
    }

    pub(crate) fn from_parts(accounts: Vec<Account>, folders: Vec<Folder>, messages: Vec<Message>, demo: bool) -> Self {
        let mut engine = Self { accounts, folders, messages, demo };
        engine.recount();
        engine
    }

    pub fn is_demo(&self) -> bool {
        self.demo
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

    /// Change read/starred flags. At least one flag must be present.
    pub fn set_flags(&mut self, id: &str, unread: Option<bool>, starred: Option<bool>) -> Result<MessageSummary, OpError> {
        if unread.is_none() && starred.is_none() {
            return Err(OpError::InvalidArgs("provide unread and/or starred".to_owned()));
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
        let target =
            self.folders.iter().find(|folder| folder.id == folder_id).ok_or_else(|| OpError::Failed(format!("unknown folder {folder_id:?}")))?;
        if target.kind.is_virtual() {
            return Err(OpError::InvalidArgs(format!("folder {folder_id:?} is virtual and cannot receive messages")));
        }
        let target_id = target.id.clone();
        let Some(message) = self.messages.iter_mut().find(|message| message.summary.id == id) else {
            return Err(OpError::Failed(format!("unknown message {id:?}")));
        };
        message.summary.folder_id = target_id;
        let summary = message.summary.clone();
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

    /// Where the local store will live. M0 only reports it; nothing is written yet.
    pub fn data_dir() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().join("Postino"))
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

#[cfg(test)]
mod tests {
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
}
