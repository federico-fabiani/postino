use chrono::{DateTime, FixedOffset};

use crate::NetError;
use crate::error::protocol;
use crate::message::{Address, Flag};
use crate::parser::Cursor;

/// Deepest number of addresses accepted in one envelope list.
const MAX_ADDRESSES: usize = 256;
/// Deepest number of flags accepted in one list.
const MAX_FLAGS: usize = 256;

/// One `* n FETCH (...)` response, before it is mapped to a public [`crate::Envelope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchData {
    pub(crate) uid: Option<u32>,
    pub(crate) flags: Vec<Flag>,
    pub(crate) internal_date: Option<DateTime<FixedOffset>>,
    pub(crate) size: Option<u32>,
    pub(crate) envelope: Option<RawEnvelope>,
    pub(crate) body: Option<Vec<u8>>,
}

/// The parts of an IMAP `ENVELOPE` this crate keeps.
///
/// `env-date` and `sender` are parsed and discarded: `INTERNALDATE` is the timestamp the store
/// needs, and `sender` duplicates `from` for display. `references` and `list-id` are not in
/// `ENVELOPE` at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEnvelope {
    pub(crate) subject: Option<String>,
    pub(crate) from: Vec<Address>,
    pub(crate) reply_to: Vec<Address>,
    pub(crate) to: Vec<Address>,
    pub(crate) cc: Vec<Address>,
    pub(crate) bcc: Vec<Address>,
    pub(crate) in_reply_to: Option<String>,
    pub(crate) message_id: Option<String>,
}

/// Parse the contents of a `FETCH` list. The opening `(` has already been consumed.
pub(crate) fn parse_fetch(cursor: &mut Cursor<'_>) -> Result<FetchData, NetError> {
    let mut data = FetchData { uid: None, flags: Vec::new(), internal_date: None, size: None, envelope: None, body: None };
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b')') {
            break;
        }
        if !cursor.has_value() {
            return Err(protocol("unterminated FETCH list"));
        }
        let key = fetch_key(cursor)?;
        match key.to_ascii_uppercase().as_str() {
            "UID" => data.uid = Some(cursor.nz_u32()?),
            "FLAGS" => data.flags = parse_flag_list(cursor)?,
            "INTERNALDATE" => data.internal_date = parse_internal_date(cursor.text_string()?.as_deref())?,
            "RFC822.SIZE" => data.size = Some(cursor.u32_value()?),
            "ENVELOPE" => data.envelope = Some(parse_envelope(cursor)?),
            // A server answers BODY.PEEK[] with BODY[]; RFC822 is the ancient spelling of the same
            // thing and is accepted defensively.
            "BODY[]" | "BODY" | "RFC822" => {
                let body = cursor.string()?.ok_or_else(|| protocol("BODY[] without a value"))?;
                data.body = Some(body);
            }
            _ => cursor.skip_value()?,
        }
    }
    Ok(data)
}

/// Read a FETCH data-item name, which may contain a bracketed section with spaces inside it.
fn fetch_key(cursor: &mut Cursor<'_>) -> Result<String, NetError> {
    cursor.skip_ws();
    let mut key = String::new();
    let mut brackets: u32 = 0;
    while let Some(byte) = cursor.peek() {
        match byte {
            b' ' | b'\t' if brackets == 0 => break,
            b'(' | b')' if brackets == 0 => break,
            b'[' => {
                brackets += 1;
                key.push('[');
                cursor.advance();
            }
            b']' => {
                if brackets == 0 {
                    return Err(protocol("unexpected ']' in a FETCH item"));
                }
                brackets -= 1;
                key.push(']');
                cursor.advance();
            }
            byte if byte.is_ascii_graphic() || byte == b' ' => {
                key.push(byte as char);
                cursor.advance();
            }
            _ => return Err(protocol("control character in a FETCH item")),
        }
        if key.len() > 512 {
            return Err(protocol("FETCH item name is too long"));
        }
    }
    if key.is_empty() {
        return Err(protocol("expected a FETCH item"));
    }
    if brackets != 0 {
        return Err(protocol("unterminated FETCH section"));
    }
    Ok(key)
}

/// Parse `FLAGS (\Seen \Answered)`; the cursor is before the list.
pub(crate) fn parse_flag_list(cursor: &mut Cursor<'_>) -> Result<Vec<Flag>, NetError> {
    if !cursor.list_open()? {
        return Err(protocol("expected a flag list"));
    }
    let mut flags: Vec<Flag> = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b')') {
            break;
        }
        if !cursor.has_value() {
            return Err(protocol("unterminated flag list"));
        }
        let atom = cursor.flag_atom()?;
        if atom == "\\*" {
            // PERMANENTFLAGS advertises "the server may create keywords"; it is not a flag.
            continue;
        }
        let flag = Flag::parse_wire(&atom)?;
        if !flags.iter().any(|seen| seen.identity() == flag.identity()) {
            flags.push(flag);
        }
        if flags.len() > MAX_FLAGS {
            return Err(protocol("too many flags"));
        }
    }
    cursor.list_close()?;
    Ok(flags)
}

fn parse_envelope(cursor: &mut Cursor<'_>) -> Result<RawEnvelope, NetError> {
    if !cursor.list_open()? {
        return Err(protocol("ENVELOPE requires a list"));
    }
    let _date = cursor.text_string()?;
    let subject = cursor.text_string()?;
    let from = parse_address_list(cursor)?;
    let _sender = parse_address_list(cursor)?;
    let reply_to = parse_address_list(cursor)?;
    let to = parse_address_list(cursor)?;
    let cc = parse_address_list(cursor)?;
    let bcc = parse_address_list(cursor)?;
    let in_reply_to = cursor.text_string()?;
    let message_id = cursor.text_string()?;
    cursor.list_close()?;
    Ok(RawEnvelope { subject, from, reply_to, to, cc, bcc, in_reply_to, message_id })
}

fn parse_address_list(cursor: &mut Cursor<'_>) -> Result<Vec<Address>, NetError> {
    if !cursor.list_open()? {
        return match cursor.text_string()? {
            None => Ok(Vec::new()),
            Some(other) => Err(protocol(format!("address list must be NIL or a list, not {other:?}"))),
        };
    }
    let mut addresses = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b')') {
            break;
        }
        if !cursor.has_value() {
            return Err(protocol("unterminated address list"));
        }
        addresses.push(parse_address(cursor)?);
        if addresses.len() > MAX_ADDRESSES {
            return Err(protocol("too many addresses"));
        }
    }
    cursor.list_close()?;
    Ok(addresses)
}

fn parse_address(cursor: &mut Cursor<'_>) -> Result<Address, NetError> {
    if !cursor.list_open()? {
        return Err(protocol("address must be a list"));
    }
    let name = cursor.text_string()?;
    let adl = cursor.text_string()?;
    let mailbox = cursor.text_string()?;
    let host = cursor.text_string()?;
    cursor.list_close()?;
    Ok(Address { name, adl, mailbox, host })
}

fn parse_internal_date(raw: Option<&str>) -> Result<Option<DateTime<FixedOffset>>, NetError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.len() > 64 {
        return Err(protocol("INTERNALDATE is too long"));
    }
    DateTime::parse_from_str(raw, "%d-%b-%Y %H:%M:%S %z").map(Some).map_err(|_| protocol(format!("invalid INTERNALDATE {raw:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Segment;

    fn fetch(text: &str) -> Result<FetchData, NetError> {
        let segments = vec![Segment::Text(text.to_owned())];
        let mut cursor = Cursor::new(&segments);
        if !cursor.list_open()? {
            return Err(protocol("test needs a list"));
        }
        let data = parse_fetch(&mut cursor)?;
        cursor.list_close()?;
        Ok(data)
    }

    #[test]
    fn parses_the_common_items() {
        let data = fetch("(UID 42 FLAGS (\\Seen $Forwarded) INTERNALDATE \"17-Jul-1996 02:44:25 -0700\" RFC822.SIZE 1234)")
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(data.uid, Some(42));
        assert_eq!(data.flags, vec![Flag::Seen, Flag::Keyword("$Forwarded".to_owned())]);
        assert_eq!(data.size, Some(1234));
        assert!(data.internal_date.is_some());
    }

    #[test]
    fn skips_unknown_items_with_nested_sections() {
        let segments = vec![
            Segment::Text("(UID 1 BODY[HEADER.FIELDS (DATE FROM)] ".to_owned()),
            Segment::Literal(b"abc".to_vec()),
            Segment::Text(" X-GM-LABELS (\"a\" \"b\"))".to_owned()),
        ];
        let mut cursor = Cursor::new(&segments);
        assert!(cursor.list_open().is_ok_and(|opened| opened));
        let data = parse_fetch(&mut cursor).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(data.uid, Some(1));
        assert!(data.body.is_none());
        assert!(cursor.list_close().is_ok());
        assert!(cursor.finish().is_ok());
    }

    #[test]
    fn parses_an_envelope() {
        let text = concat!(
            "(UID 9 ENVELOPE (\"Mon, 1 Jan 2024 10:00:00 +0000\" \"Ciao\" ((\"Giulia\" NIL \"giulia\" \"example.com\")) ",
            "((\"Giulia\" NIL \"giulia\" \"example.com\")) ((\"Giulia\" NIL \"giulia\" \"example.com\")) ",
            "((\"Me\" NIL \"me\" \"example.it\")) NIL NIL NIL \"<id@example.com>\") FLAGS ())"
        );
        let data = fetch(text).unwrap_or_else(|error| panic!("{error}"));
        let envelope = data.envelope.unwrap_or_else(|| panic!("no envelope"));
        assert_eq!(envelope.subject.as_deref(), Some("Ciao"));
        assert_eq!(envelope.from.len(), 1);
        assert_eq!(envelope.from.first().and_then(|address| address.mailbox.as_deref()), Some("giulia"));
        assert_eq!(envelope.message_id.as_deref(), Some("<id@example.com>"));
    }

    #[test]
    fn refuses_negative_sizes_and_nil_uids() {
        assert!(fetch("(RFC822.SIZE -1)").is_err());
        assert!(fetch("(UID NIL)").is_err());
        assert!(fetch("(RFC822.SIZE 99999999999999999999)").is_err());
    }

    #[test]
    fn refuses_a_truncated_envelope() {
        assert!(fetch("(UID 1 ENVELOPE (\"x\" \"y\"))").is_err());
        assert!(fetch("(UID 1 ENVELOPE (\"x\" \"y\" NIL NIL NIL NIL NIL NIL))").is_err());
    }

    #[test]
    fn refuses_an_unterminated_fetch_item() {
        assert!(fetch("(UID 1 BODY[HEADER.FIELDS (DATE)").is_err());
    }
}
