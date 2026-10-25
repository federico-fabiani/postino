use crate::message::Flag;

/// A folder role the server labelled with an RFC 6154 special-use attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialUse {
    /// `\Inbox`
    Inbox,
    /// `\Sent`
    Sent,
    /// `\Drafts`
    Drafts,
    /// `\Trash`
    Trash,
    /// `\Junk`
    Junk,
    /// `\Archive`
    Archive,
    /// `\Flagged`
    Flagged,
    /// `\All`
    All,
}

impl SpecialUse {
    /// Parse one LIST attribute, case-insensitively.
    pub(crate) fn from_attribute(attribute: &str) -> Option<Self> {
        if !attribute.starts_with('\\') {
            return None;
        }
        Some(match attribute.to_ascii_lowercase().as_str() {
            "\\inbox" => Self::Inbox,
            "\\sent" => Self::Sent,
            "\\drafts" => Self::Drafts,
            "\\trash" => Self::Trash,
            "\\junk" => Self::Junk,
            "\\archive" => Self::Archive,
            "\\flagged" => Self::Flagged,
            "\\all" => Self::All,
            _ => return None,
        })
    }
}

/// One folder from `LIST`.
///
/// `name` is decoded from modified UTF-7 for display; `wire_name` is exactly what the server sent
/// and is what [`crate::Session::select`] and friends expect, because re-encoding a decoded name
/// could change bytes the server considers meaningful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    /// Decoded display name (modified UTF-7 resolved).
    pub name: String,
    /// Name as it appears on the wire; pass this back to the server.
    pub wire_name: String,
    /// Hierarchy delimiter reported by the server, or `None` for a flat mailbox.
    pub delimiter: Option<char>,
    /// Raw LIST attributes, e.g. `\Noselect`, `\HasNoChildren`, `\Sent`.
    pub attributes: Vec<String>,
    /// Special use, when the server labelled the folder.
    pub special_use: Option<SpecialUse>,
}

impl RemoteFolder {
    /// Whether the folder can hold messages: `\Noselect` folders cannot, but their children may.
    #[must_use]
    pub fn is_selectable(&self) -> bool {
        !self.attributes.iter().any(|attribute| attribute.eq_ignore_ascii_case("\\Noselect"))
    }
}

/// What `SELECT` or `EXAMINE` reported about a folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailboxStatus {
    /// Number of messages in the folder (`EXISTS`).
    pub exists: u32,
    /// Number of messages with `\Recent`.
    pub recent: u32,
    /// UID validity value. A change invalidates every cached UID for the folder.
    pub uid_validity: u32,
    /// Next UID the server will assign, when it reports it.
    pub uid_next: Option<u32>,
    /// First unseen message sequence number, when the server reports it.
    pub unseen: Option<u32>,
    /// Highest CONDSTORE modification sequence, when the server reports it.
    pub highest_modseq: Option<u64>,
    /// Whether the folder is read-only (`EXAMINE`, or `[READ-ONLY]`).
    pub read_only: bool,
    /// Flags a client may change persistently in this folder.
    pub permanent_flags: Vec<Flag>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_use_is_case_insensitive() {
        assert_eq!(SpecialUse::from_attribute("\\Sent"), Some(SpecialUse::Sent));
        assert_eq!(SpecialUse::from_attribute("\\sent"), Some(SpecialUse::Sent));
        assert_eq!(SpecialUse::from_attribute("\\All"), Some(SpecialUse::All));
        assert_eq!(SpecialUse::from_attribute("\\Noselect"), None);
        assert_eq!(SpecialUse::from_attribute("Sent"), None);
    }

    #[test]
    fn noselect_folders_are_not_selectable() {
        let folder = RemoteFolder {
            name: "Archive".to_owned(),
            wire_name: "Archive".to_owned(),
            delimiter: Some('/'),
            attributes: vec!["\\Noselect".to_owned(), "\\HasChildren".to_owned()],
            special_use: Some(SpecialUse::Archive),
        };
        assert!(!folder.is_selectable());
    }
}
