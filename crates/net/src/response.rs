use crate::NetError;
use crate::error::{protocol, sanitize_reason};
use crate::fetch::{FetchData, parse_fetch, parse_flag_list};
use crate::folder::{RemoteFolder, SpecialUse};
use crate::mailbox::{MAX_NAME_BYTES, decode_utf7};
use crate::message::Flag;
use crate::parser::{Cursor, Segment};

/// Deepest number of LIST attributes accepted for one folder.
const MAX_ATTRIBUTES: usize = 32;
/// Deepest number of capabilities accepted in one `[CAPABILITY ...]` code.
const MAX_CODE_CAPABILITIES: usize = 64;
/// Longest unknown response code kept, in bytes.
const MAX_CODE_TEXT: usize = 128;

/// A status word, the first token of every tagged and of most untagged responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Ok,
    No,
    Bad,
    PreAuth,
    Bye,
}

/// A bracketed response code, e.g. `[UIDVALIDITY 42]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusCode {
    Alert,
    BadCharset(Vec<String>),
    Capability(Vec<String>),
    Parse,
    PermanentFlags(Vec<Flag>),
    ReadOnly,
    ReadWrite,
    TryCreate,
    UidNext(u32),
    UidValidity(u32),
    Unseen(u32),
    HighestModSeq(u64),
    Other(String),
}

impl StatusCode {
    /// Wire-ish description used in error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Alert => "ALERT".to_owned(),
            Self::BadCharset(charsets) if charsets.is_empty() => "BADCHARSET".to_owned(),
            Self::BadCharset(charsets) => format!("BADCHARSET ({})", charsets.join(" ")),
            Self::Capability(items) => format!("CAPABILITY {}", items.join(" ")),
            Self::Parse => "PARSE".to_owned(),
            Self::PermanentFlags(flags) => {
                let names = flags.iter().map(Flag::as_wire).collect::<Vec<_>>();
                format!("PERMANENTFLAGS ({})", names.join(" "))
            }
            Self::ReadOnly => "READ-ONLY".to_owned(),
            Self::ReadWrite => "READ-WRITE".to_owned(),
            Self::TryCreate => "TRYCREATE".to_owned(),
            Self::UidNext(value) => format!("UIDNEXT {value}"),
            Self::UidValidity(value) => format!("UIDVALIDITY {value}"),
            Self::Unseen(value) => format!("UNSEEN {value}"),
            Self::HighestModSeq(value) => format!("HIGHESTMODSEQ {value}"),
            Self::Other(text) => text.clone(),
        }
    }
}

/// A status response with its optional code and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusResponse {
    pub(crate) status: Status,
    pub(crate) code: Option<StatusCode>,
    pub(crate) text: String,
}

impl StatusResponse {
    /// Turn `NO` and `BAD` into actionable errors, keeping the response code visible.
    pub(crate) fn into_error(self, command: &'static str) -> NetError {
        let reason = match &self.code {
            Some(code) => sanitize_reason(&format!("[{}] {}", code.describe(), self.text)),
            None => sanitize_reason(&self.text),
        };
        match self.status {
            Status::No => NetError::ServerNo { command, reason },
            Status::Bad => NetError::ServerBad { command, reason },
            _ => protocol(format!("{command} ended with an unexpected status")),
        }
    }
}

/// One `* ...` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Untagged {
    Status(StatusResponse),
    Capability(Vec<String>),
    List(RemoteFolder),
    Exists(u32),
    Recent(u32),
    Expunge(u32),
    Fetch(Box<FetchData>),
    Flags(Vec<Flag>),
    /// A response this client does not use. It was read and bounded, then dropped.
    Other,
}

/// One complete server response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Response {
    Untagged(Untagged),
    Continuation(String),
    Tagged { tag: String, response: StatusResponse },
}

/// Parse one logical response, including any literals already attached to it.
pub(crate) fn parse_response(segments: &[Segment]) -> Result<Response, NetError> {
    let mut cursor = Cursor::new(segments);
    let Some(first) = cursor.peek() else {
        return Err(protocol("empty response"));
    };
    match first {
        b'*' => {
            cursor.expect_char(b'*')?;
            cursor.expect_ws()?;
            Ok(Response::Untagged(parse_untagged(&mut cursor)?))
        }
        b'+' => {
            cursor.expect_char(b'+')?;
            cursor.skip_ws();
            let text = cursor.remainder("continuation text")?;
            Ok(Response::Continuation(sanitize_reason(&text)))
        }
        _ => {
            let tag = cursor.atom()?;
            if tag.len() > 64 {
                return Err(protocol("tag is too long"));
            }
            cursor.expect_ws()?;
            let word = cursor.atom()?;
            let status = status_from_word(&word)?;
            let response = parse_status_tail(&mut cursor, status)?;
            cursor.finish()?;
            Ok(Response::Tagged { tag, response })
        }
    }
}

fn status_from_word(word: &str) -> Result<Status, NetError> {
    match word.to_ascii_uppercase().as_str() {
        "OK" => Ok(Status::Ok),
        "NO" => Ok(Status::No),
        "BAD" => Ok(Status::Bad),
        "PREAUTH" => Ok(Status::PreAuth),
        "BYE" => Ok(Status::Bye),
        _ => Err(protocol(format!("unknown status {word:?}"))),
    }
}

fn parse_status_tail(cursor: &mut Cursor<'_>, status: Status) -> Result<StatusResponse, NetError> {
    cursor.skip_ws();
    let code = if cursor.peek() == Some(b'[') {
        cursor.expect_char(b'[')?;
        Some(parse_code(cursor)?)
    } else {
        None
    };
    cursor.skip_ws();
    let text = cursor.remainder("status text")?;
    Ok(StatusResponse { status, code, text: sanitize_reason(&text) })
}

fn parse_code(cursor: &mut Cursor<'_>) -> Result<StatusCode, NetError> {
    let name = cursor.atom()?;
    let code = match name.to_ascii_uppercase().as_str() {
        "ALERT" => StatusCode::Alert,
        "READ-ONLY" => StatusCode::ReadOnly,
        "READ-WRITE" => StatusCode::ReadWrite,
        "TRYCREATE" => StatusCode::TryCreate,
        "PARSE" => StatusCode::Parse,
        "CAPABILITY" => {
            let mut items = Vec::new();
            loop {
                cursor.skip_ws();
                if cursor.peek() == Some(b']') {
                    break;
                }
                if cursor.peek().is_none() {
                    return Err(protocol("unterminated CAPABILITY response code"));
                }
                items.push(cursor.atom()?);
                if items.len() > MAX_CODE_CAPABILITIES {
                    return Err(protocol("too many capabilities in a response code"));
                }
            }
            StatusCode::Capability(items)
        }
        "PERMANENTFLAGS" => StatusCode::PermanentFlags(parse_flag_list(cursor)?),
        "UIDNEXT" => StatusCode::UidNext(cursor.nz_u32()?),
        "UIDVALIDITY" => StatusCode::UidValidity(cursor.nz_u32()?),
        "UNSEEN" => StatusCode::Unseen(cursor.nz_u32()?),
        "HIGHESTMODSEQ" => StatusCode::HighestModSeq(cursor.number()?),
        "BADCHARSET" => {
            let mut charsets = Vec::new();
            if cursor.list_open()? {
                loop {
                    cursor.skip_ws();
                    if cursor.peek() == Some(b')') {
                        break;
                    }
                    if !cursor.has_value() {
                        return Err(protocol("unterminated BADCHARSET list"));
                    }
                    charsets.push(cursor.atom()?);
                    if charsets.len() > MAX_CODE_CAPABILITIES {
                        return Err(protocol("too many charsets in a response code"));
                    }
                }
                cursor.list_close()?;
            }
            StatusCode::BadCharset(charsets)
        }
        other => {
            let mut rest = String::new();
            loop {
                match cursor.peek() {
                    None => return Err(protocol("unterminated response code")),
                    Some(b']') => break,
                    Some(byte) if byte.is_ascii_graphic() || byte == b' ' => {
                        rest.push(byte as char);
                        cursor.advance();
                        if rest.len() > MAX_CODE_TEXT {
                            return Err(protocol("response code is too long"));
                        }
                    }
                    Some(_) => return Err(protocol("control character in a response code")),
                }
            }
            let rest = rest.trim();
            StatusCode::Other(if rest.is_empty() { other.to_owned() } else { format!("{other} {rest}") })
        }
    };
    cursor.expect_char(b']')?;
    Ok(code)
}

fn parse_untagged(cursor: &mut Cursor<'_>) -> Result<Untagged, NetError> {
    cursor.skip_ws();
    match cursor.peek() {
        Some(byte) if byte.is_ascii_digit() => {
            let number = cursor.number()?;
            let count = u32::try_from(number).map_err(|_| protocol("untagged count is out of range"))?;
            cursor.expect_ws()?;
            let word = cursor.atom()?;
            match word.to_ascii_uppercase().as_str() {
                "EXISTS" => {
                    cursor.finish()?;
                    Ok(Untagged::Exists(count))
                }
                "RECENT" => {
                    cursor.finish()?;
                    Ok(Untagged::Recent(count))
                }
                "EXPUNGE" => {
                    if count == 0 {
                        return Err(protocol("EXPUNGE sequence number 0"));
                    }
                    cursor.finish()?;
                    Ok(Untagged::Expunge(count))
                }
                "FETCH" => {
                    if count == 0 {
                        return Err(protocol("FETCH sequence number 0"));
                    }
                    cursor.expect_ws()?;
                    if !cursor.list_open()? {
                        return Err(protocol("FETCH requires a list"));
                    }
                    let data = parse_fetch(cursor)?;
                    cursor.list_close()?;
                    cursor.finish()?;
                    Ok(Untagged::Fetch(Box::new(data)))
                }
                _ => {
                    cursor.drain();
                    Ok(Untagged::Other)
                }
            }
        }
        Some(_) => {
            let word = cursor.atom()?;
            if let Ok(status) = status_from_word(&word) {
                return Ok(Untagged::Status(parse_status_tail(cursor, status)?));
            }
            match word.to_ascii_uppercase().as_str() {
                "CAPABILITY" => {
                    cursor.skip_ws();
                    let mut items = Vec::new();
                    while cursor.peek().is_some() {
                        items.push(cursor.atom()?);
                        if items.len() > 128 {
                            return Err(protocol("too many capabilities"));
                        }
                    }
                    cursor.finish()?;
                    Ok(Untagged::Capability(items))
                }
                "LIST" => parse_list(cursor),
                "FLAGS" => {
                    let flags = parse_flag_list(cursor)?;
                    cursor.finish()?;
                    Ok(Untagged::Flags(flags))
                }
                _ => {
                    cursor.drain();
                    Ok(Untagged::Other)
                }
            }
        }
        None => Err(protocol("empty untagged response")),
    }
}

fn parse_list(cursor: &mut Cursor<'_>) -> Result<Untagged, NetError> {
    let mut attributes = Vec::new();
    if !cursor.list_open()? {
        return Err(protocol("LIST requires an attribute list"));
    }
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b')') {
            break;
        }
        if !cursor.has_value() {
            return Err(protocol("unterminated LIST attribute list"));
        }
        attributes.push(cursor.flag_atom()?);
        if attributes.len() > MAX_ATTRIBUTES {
            return Err(protocol("too many LIST attributes"));
        }
    }
    cursor.list_close()?;

    cursor.expect_ws()?;
    let delimiter = match cursor.text_string()? {
        None => None,
        Some(text) => {
            let mut chars = text.chars();
            let first = chars.next().ok_or_else(|| protocol("LIST delimiter is empty"))?;
            if chars.next().is_some() {
                return Err(protocol("LIST delimiter is not one character"));
            }
            Some(first)
        }
    };

    cursor.expect_ws()?;
    let raw = cursor.string()?.ok_or_else(|| protocol("LIST returned NIL as a mailbox name"))?;
    let wire_name = String::from_utf8(raw).map_err(|_| protocol("mailbox name is not valid UTF-8"))?;
    if wire_name.is_empty() {
        return Err(protocol("LIST returned an empty mailbox name"));
    }
    if wire_name.len() > MAX_NAME_BYTES {
        return Err(NetError::TooLarge { what: "mailbox name", limit: MAX_NAME_BYTES });
    }
    cursor.finish()?;

    let name = decode_utf7(&wire_name).unwrap_or_else(|| wire_name.clone());
    let special_use = attributes.iter().find_map(|attribute| SpecialUse::from_attribute(attribute));
    Ok(Untagged::List(RemoteFolder { name, wire_name, delimiter, attributes, special_use }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Response, NetError> {
        let segments = vec![Segment::Text(text.to_owned())];
        parse_response(&segments)
    }

    #[test]
    fn parses_tagged_statuses() {
        let response = parse("A0001 OK [UIDVALIDITY 42] UIDs valid").unwrap_or_else(|error| panic!("{error}"));
        match response {
            Response::Tagged { tag, response } => {
                assert_eq!(tag, "A0001");
                assert_eq!(response.status, Status::Ok);
                assert_eq!(response.code, Some(StatusCode::UidValidity(42)));
                assert_eq!(response.text, "UIDs valid");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_capability_and_list() {
        match parse("* CAPABILITY IMAP4rev1 AUTH=PLAIN IDLE").unwrap_or_else(|error| panic!("{error}")) {
            Response::Untagged(Untagged::Capability(items)) => assert_eq!(items.len(), 3),
            other => panic!("{other:?}"),
        }
        match parse("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent Items\"").unwrap_or_else(|error| panic!("{error}")) {
            Response::Untagged(Untagged::List(folder)) => {
                assert_eq!(folder.name, "Sent Items");
                assert_eq!(folder.wire_name, "Sent Items");
                assert_eq!(folder.delimiter, Some('/'));
                assert_eq!(folder.special_use, Some(SpecialUse::Sent));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_nil_delimiter_and_modified_utf7() {
        match parse("* LIST (\\Noselect) NIL \"&ZeVnLIqe-\"").unwrap_or_else(|error| panic!("{error}")) {
            Response::Untagged(Untagged::List(folder)) => {
                assert_eq!(folder.delimiter, None);
                assert_eq!(folder.name, "日本語");
                assert_eq!(folder.wire_name, "&ZeVnLIqe-");
                assert!(!folder.is_selectable());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_numbers_and_states() {
        assert_eq!(parse("* 12 EXISTS").ok(), Some(Response::Untagged(Untagged::Exists(12))));
        assert_eq!(parse("* 0 EXISTS").ok(), Some(Response::Untagged(Untagged::Exists(0))));
        assert_eq!(parse("* 3 RECENT").ok(), Some(Response::Untagged(Untagged::Recent(3))));
        assert_eq!(parse("* 2 EXPUNGE").ok(), Some(Response::Untagged(Untagged::Expunge(2))));
        assert_eq!(parse("+ idling").ok(), Some(Response::Continuation("idling".to_owned())));
    }

    #[test]
    fn refuses_bad_tags_and_statuses() {
        assert!(parse("A0001 MAYBE done").is_err());
        assert!(parse("A0001").is_err());
        assert!(parse("* 5").is_err());
        assert!(parse("*").is_err());
        assert!(parse("A0001OK done").is_err());
    }

    #[test]
    fn refuses_trailing_garbage() {
        assert!(parse("* 3 EXISTS junk").is_err());
        assert!(parse("* LIST (\\Sent) \"/\" \"A\" extra").is_err());
        assert!(parse("* 1 FETCH (UID 1) extra").is_err());
    }

    #[test]
    fn refuses_malformed_response_codes() {
        assert!(parse("A0001 OK [UIDVALIDITY 0] bad").is_err());
        assert!(parse("A0001 OK [UIDVALIDITY NIL] bad").is_err());
        assert!(parse("A0001 OK [CAPABILITY IMAP4rev1").is_err());
        assert!(parse("A0001 NO [ALERT missing bracket").is_err());
    }
}
