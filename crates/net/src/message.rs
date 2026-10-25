use chrono::{DateTime, FixedOffset};

use crate::NetError;
use crate::error::protocol;

/// A message flag: an RFC 3501 system flag or a server keyword.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flag {
    /// `\Seen`
    Seen,
    /// `\Answered`
    Answered,
    /// `\Flagged`
    Flagged,
    /// `\Deleted`
    Deleted,
    /// `\Draft`
    Draft,
    /// `\Recent`. Servers set it; a client cannot store it.
    Recent,
    /// A server keyword such as `$Forwarded` or `NonJunk`, preserved with its case.
    Keyword(String),
}

impl Flag {
    /// The flag as it appears on the wire.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Seen => "\\Seen",
            Self::Answered => "\\Answered",
            Self::Flagged => "\\Flagged",
            Self::Deleted => "\\Deleted",
            Self::Draft => "\\Draft",
            Self::Recent => "\\Recent",
            Self::Keyword(keyword) => keyword,
        }
    }

    /// Parse one flag atom, e.g. `\Seen` or `$Forwarded`.
    pub(crate) fn parse_wire(atom: &str) -> Result<Self, NetError> {
        if atom.len() > 64 {
            return Err(protocol("flag is too long"));
        }
        if let Some(name) = atom.strip_prefix('\\') {
            return match name.to_ascii_lowercase().as_str() {
                "seen" => Ok(Self::Seen),
                "answered" => Ok(Self::Answered),
                "flagged" => Ok(Self::Flagged),
                "deleted" => Ok(Self::Deleted),
                "draft" => Ok(Self::Draft),
                "recent" => Ok(Self::Recent),
                _ => Err(protocol(format!("unknown system flag {atom:?}"))),
            };
        }
        if !keyword_is_valid(atom) {
            return Err(protocol(format!("invalid keyword {atom:?}")));
        }
        Ok(Self::Keyword(atom.to_owned()))
    }

    /// Case-insensitive identity used to deduplicate a flag list.
    pub(crate) fn identity(&self) -> String {
        self.as_wire().to_ascii_lowercase()
    }
}

/// How [`crate::Session::store_flags`] changes flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagMode {
    /// `+FLAGS.SILENT`
    Add,
    /// `-FLAGS.SILENT`
    Remove,
    /// `FLAGS.SILENT`
    Replace,
}

impl FlagMode {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Add => "+FLAGS.SILENT",
            Self::Remove => "-FLAGS.SILENT",
            Self::Replace => "FLAGS.SILENT",
        }
    }
}

/// One event observed during `IDLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleEvent {
    /// `* n EXISTS`: the folder now holds `n` messages.
    Exists(u32),
    /// `* n EXPUNGE`: sequence number `n` was removed.
    Expunge(u32),
    /// `* n RECENT`: `n` messages are recent.
    Recent(u32),
    /// `* BYE text`: the server is closing the connection.
    Bye(String),
    /// Anything else the server volunteered during the wait, ignored by the sync layer.
    Other,
}

/// One address from an IMAP `ENVELOPE`, split into its four raw parts.
///
/// No RFC 5322 parsing, no display-name decoding: `name` is whatever the server put in the field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Address {
    /// Display name.
    pub name: Option<String>,
    /// Source route (`adl`), almost always `NIL`.
    pub adl: Option<String>,
    /// Local part.
    pub mailbox: Option<String>,
    /// Domain.
    pub host: Option<String>,
}

/// One message's UID and flags from a UID/FLAGS-only fetch.
///
/// This is the liveness scan of a folder: no envelope, no body, one small response per message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UidFlags {
    /// UID, stable for the lifetime of the folder's `UIDVALIDITY`.
    pub uid: u32,
    /// Flags the server reports.
    pub flags: Vec<Flag>,
}

/// One message's headers as IMAP reports them, with no MIME parsing.
///
/// `references` and `list_id` are always empty at this layer: RFC 3501's `ENVELOPE` has no slot for
/// either, and fetching them means a second `BODY.PEEK[HEADER.FIELDS (...)]` that belongs to the
/// sync package. The fields exist so P5b can fill them without changing this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// UID, stable for the lifetime of the folder's `UIDVALIDITY`.
    pub uid: u32,
    /// Flags reported with the envelope.
    pub flags: Vec<Flag>,
    /// Server `INTERNALDATE`, parsed as a fixed-offset timestamp.
    pub internal_date: Option<DateTime<FixedOffset>>,
    /// `RFC822.SIZE` in bytes, or 0 when the server did not report it.
    pub size: u32,
    /// Subject exactly as decoded from the envelope string.
    pub subject: String,
    /// `From`.
    pub from: Vec<Address>,
    /// `To`.
    pub to: Vec<Address>,
    /// `Cc`.
    pub cc: Vec<Address>,
    /// `Bcc`.
    pub bcc: Vec<Address>,
    /// `Reply-To`, separate from `From` because mail clients show it.
    pub reply_to: Vec<Address>,
    /// `Message-ID` including angle brackets when the server keeps them.
    pub message_id: Option<String>,
    /// `In-Reply-To`.
    pub in_reply_to: Option<String>,
    /// Empty at this layer; see the type documentation.
    pub references: Vec<String>,
    /// Empty at this layer; see the type documentation.
    pub list_id: Option<String>,
}

/// Whether a keyword can be written as one IMAP flag atom.
///
/// Public because [`Flag::Keyword`] can be built by hand: a keyword with a space, a bracket or a
/// CR/LF must never reach a `STORE` command.
pub(crate) fn keyword_is_valid(keyword: &str) -> bool {
    !keyword.is_empty()
        && keyword.len() <= 64
        && keyword.bytes().all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_flags_round_trip() {
        for flag in [Flag::Seen, Flag::Answered, Flag::Flagged, Flag::Deleted, Flag::Draft, Flag::Recent] {
            let parsed = Flag::parse_wire(flag.as_wire());
            assert_eq!(parsed.as_ref(), Ok(&flag));
        }
        assert_eq!(Flag::parse_wire("\\SEEN"), Ok(Flag::Seen));
        assert_eq!(Flag::parse_wire("\\seen"), Ok(Flag::Seen));
    }

    #[test]
    fn keywords_keep_their_case() {
        assert_eq!(Flag::parse_wire("$Forwarded"), Ok(Flag::Keyword("$Forwarded".to_owned())));
        assert_eq!(Flag::parse_wire("NonJunk").map(|flag| flag.as_wire().to_owned()).as_deref(), Ok("NonJunk"));
    }

    #[test]
    fn unknown_system_flags_and_junk_are_refused() {
        for atom in ["\\Noselect", "\\*", "", "has space", "a(b)"] {
            assert!(Flag::parse_wire(atom).is_err(), "{atom:?}");
        }
    }

    #[test]
    fn modes_map_to_store_verbs() {
        assert_eq!(FlagMode::Add.as_wire(), "+FLAGS.SILENT");
        assert_eq!(FlagMode::Remove.as_wire(), "-FLAGS.SILENT");
        assert_eq!(FlagMode::Replace.as_wire(), "FLAGS.SILENT");
    }
}
