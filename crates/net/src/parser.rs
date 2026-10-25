use crate::NetError;
use crate::error::protocol;

/// One piece of a logical response: text from a line, or the body of a literal that ended a line.
///
/// A response is a sequence `Text, [Literal, Text]...` produced by
/// [`crate::wire::Connection::read_response`]. A literal always has a continuation line after it,
/// so the sequence always ends with text (possibly empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Bytes of one line, without CRLF and without the trailing literal marker.
    Text(String),
    /// Bytes of a literal body.
    Literal(Vec<u8>),
}

/// Deepest list nesting accepted inside one response.
const MAX_NESTING: u32 = 32;
/// Longest accepted atom, in bytes.
const MAX_ATOM: usize = 1024;
/// Digits accepted in one number: larger values are refused, never wrapped.
const MAX_NUMBER_DIGITS: usize = 20;

/// A byte cursor over response segments.
///
/// The parser is deliberately synchronous and independent of the socket: every hostile case is a
/// unit test, and the network layer only has to produce segments.
pub(crate) struct Cursor<'a> {
    segments: &'a [Segment],
    index: usize,
    offset: usize,
    depth: u32,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(segments: &'a [Segment]) -> Self {
        Self { segments, index: 0, offset: 0, depth: 0 }
    }

    fn normalize(&mut self) {
        loop {
            let exhausted = match self.segments.get(self.index) {
                Some(Segment::Text(text)) => self.offset >= text.len(),
                _ => false,
            };
            if !exhausted {
                return;
            }
            self.index += 1;
            self.offset = 0;
        }
    }

    fn text(&mut self) -> Option<&'a str> {
        self.normalize();
        let segments = self.segments;
        match segments.get(self.index)? {
            Segment::Text(text) => text.get(self.offset..),
            Segment::Literal(_) => None,
        }
    }

    pub(crate) fn peek(&mut self) -> Option<u8> {
        self.text()?.as_bytes().first().copied()
    }

    fn bump(&mut self) {
        self.offset += 1;
    }

    pub(crate) fn advance(&mut self) {
        self.bump();
    }

    pub(crate) fn pending_literal(&mut self) -> bool {
        self.normalize();
        matches!(self.segments.get(self.index), Some(Segment::Literal(_)))
    }

    pub(crate) fn has_value(&mut self) -> bool {
        self.peek().is_some() || self.pending_literal()
    }

    pub(crate) fn take_literal(&mut self) -> Option<&'a [u8]> {
        self.normalize();
        let segments = self.segments;
        let bytes = match segments.get(self.index)? {
            Segment::Literal(bytes) => bytes.as_slice(),
            Segment::Text(_) => return None,
        };
        self.index += 1;
        self.offset = 0;
        Some(bytes)
    }

    pub(crate) fn skip_ws(&mut self) {
        while let Some(text) = self.text() {
            let trimmed = text.trim_start_matches([' ', '\t']);
            let skipped = text.len() - trimmed.len();
            if skipped == 0 {
                return;
            }
            self.offset += skipped;
        }
    }

    pub(crate) fn expect_ws(&mut self) -> Result<(), NetError> {
        let Some(text) = self.text() else {
            return Err(protocol("expected a space"));
        };
        let trimmed = text.trim_start_matches([' ', '\t']);
        if trimmed.len() == text.len() {
            return Err(protocol("expected a space"));
        }
        self.offset += text.len() - trimmed.len();
        Ok(())
    }

    pub(crate) fn expect_char(&mut self, expected: u8) -> Result<(), NetError> {
        self.skip_ws();
        if self.peek() != Some(expected) {
            return Err(protocol(format!("expected {:?}", expected as char)));
        }
        self.bump();
        Ok(())
    }

    pub(crate) fn atom(&mut self) -> Result<String, NetError> {
        self.skip_ws();
        let mut out = String::new();
        while let Some(byte) = self.peek() {
            if !is_atom_byte(byte) {
                break;
            }
            if out.len() >= MAX_ATOM {
                return Err(protocol("atom is too long"));
            }
            out.push(byte as char);
            self.bump();
        }
        if out.is_empty() {
            return Err(protocol("expected an atom"));
        }
        Ok(out)
    }

    /// Read a flag atom: an optional backslash followed by atom bytes.
    pub(crate) fn flag_atom(&mut self) -> Result<String, NetError> {
        self.skip_ws();
        let mut out = String::new();
        if self.peek() == Some(b'\\') {
            out.push('\\');
            self.bump();
            if self.peek() == Some(b'*') {
                out.push('*');
                self.bump();
                return Ok(out);
            }
        }
        while let Some(byte) = self.peek() {
            if !is_atom_byte(byte) {
                break;
            }
            if out.len() >= MAX_ATOM {
                return Err(protocol("flag is too long"));
            }
            out.push(byte as char);
            self.bump();
        }
        if out == "\\" {
            return Err(protocol("expected a flag name after the backslash"));
        }
        if out.is_empty() {
            return Err(protocol("expected a flag"));
        }
        Ok(out)
    }

    pub(crate) fn number(&mut self) -> Result<u64, NetError> {
        self.skip_ws();
        let Some(text) = self.text() else {
            return Err(protocol("expected a number"));
        };
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(protocol("expected a number"));
        }
        if digits > MAX_NUMBER_DIGITS {
            return Err(protocol("number is too large"));
        }
        let slice = text.get(..digits).ok_or_else(|| protocol("expected a number"))?;
        let value = slice.parse::<u64>().map_err(|_| protocol("number is too large"))?;
        self.offset += digits;
        Ok(value)
    }

    pub(crate) fn u32_value(&mut self) -> Result<u32, NetError> {
        u32::try_from(self.number()?).map_err(|_| protocol("number is out of range"))
    }

    pub(crate) fn nz_u32(&mut self) -> Result<u32, NetError> {
        let value = self.u32_value()?;
        if value == 0 {
            return Err(protocol("expected a non-zero number"));
        }
        Ok(value)
    }

    pub(crate) fn quoted(&mut self) -> Result<Vec<u8>, NetError> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err(protocol("unterminated quoted string")),
                Some(b'"') => {
                    self.bump();
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.bump();
                    match self.peek() {
                        Some(next) => {
                            out.push(next);
                            self.bump();
                        }
                        None => return Err(protocol("unterminated quoted escape")),
                    }
                }
                Some(byte) => {
                    out.push(byte);
                    self.bump();
                }
            }
        }
    }

    pub(crate) fn string(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        self.skip_ws();
        if let Some(bytes) = self.take_literal() {
            return Ok(Some(bytes.to_vec()));
        }
        match self.peek() {
            Some(b'"') => {
                self.bump();
                self.quoted().map(Some)
            }
            Some(byte) if is_atom_byte(byte) => {
                let atom = self.atom()?;
                if atom.eq_ignore_ascii_case("NIL") { Ok(None) } else { Ok(Some(atom.into_bytes())) }
            }
            _ => Err(protocol("expected a string")),
        }
    }

    pub(crate) fn text_string(&mut self) -> Result<Option<String>, NetError> {
        Ok(self.string()?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub(crate) fn list_open(&mut self) -> Result<bool, NetError> {
        self.skip_ws();
        if self.peek() != Some(b'(') {
            return Ok(false);
        }
        self.bump();
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(protocol("lists nest too deeply"));
        }
        Ok(true)
    }

    pub(crate) fn list_close(&mut self) -> Result<(), NetError> {
        self.expect_char(b')')?;
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    /// Consume one value of any shape, for data items this client does not interpret.
    pub(crate) fn skip_value(&mut self) -> Result<(), NetError> {
        self.skip_ws();
        if self.take_literal().is_some() {
            return Ok(());
        }
        match self.peek() {
            Some(b'(') => {
                self.list_open()?;
                loop {
                    self.skip_ws();
                    if self.peek() == Some(b')') {
                        break;
                    }
                    if !self.has_value() {
                        return Err(protocol("unterminated list"));
                    }
                    self.skip_value()?;
                }
                self.list_close()
            }
            Some(b'"') => {
                self.bump();
                self.quoted().map(|_| ())
            }
            Some(_) => self.atom().map(|_| ()),
            None => Err(protocol("expected a value")),
        }
    }

    /// Require that the response has been fully consumed.
    pub(crate) fn finish(&mut self) -> Result<(), NetError> {
        self.skip_ws();
        if self.index < self.segments.len() {
            return Err(protocol("trailing data in response"));
        }
        Ok(())
    }

    /// Consume every remaining segment without interpreting it.
    pub(crate) fn drain(&mut self) {
        self.index = self.segments.len();
        self.offset = 0;
    }

    /// Read the rest of the current text segment, refusing a pending literal.
    pub(crate) fn remainder(&mut self, what: &'static str) -> Result<String, NetError> {
        if self.pending_literal() {
            return Err(protocol(format!("{what} cannot be a literal")));
        }
        let Some(text) = self.text() else {
            return Ok(String::new());
        };
        let text = text.to_owned();
        self.offset += text.len();
        Ok(text)
    }
}

/// Bytes accepted in an atom: printable ASCII except the RFC 3501 atom-specials.
fn is_atom_byte(byte: u8) -> bool {
    byte.is_ascii_graphic() && !matches!(byte, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_and_numbers_stop_at_delimiters() {
        let segments = vec![Segment::Text("UID 42)".to_owned())];
        let mut cursor = Cursor::new(&segments);
        assert_eq!(cursor.atom().as_deref(), Ok("UID"));
        assert_eq!(cursor.u32_value().ok(), Some(42));
        assert_eq!(cursor.peek(), Some(b')'));
    }

    #[test]
    fn refuses_huge_and_negative_numbers() {
        for text in ["99999999999999999999", "-1", "NIL", ""] {
            let segments = vec![Segment::Text(text.to_owned())];
            let mut cursor = Cursor::new(&segments);
            assert!(cursor.number().is_err(), "{text:?}");
        }
    }

    #[test]
    fn quoted_strings_unescape() {
        let segments = vec![Segment::Text("\"a\\\"b\\\\c\"".to_owned())];
        let mut cursor = Cursor::new(&segments);
        assert_eq!(cursor.string().ok().flatten(), Some(b"a\"b\\c".to_vec()));
    }

    #[test]
    fn literals_are_values_and_text_continues() {
        let segments = vec![Segment::Text(" ".to_owned()), Segment::Literal(b"hello".to_vec()), Segment::Text(")".to_owned())];
        let mut cursor = Cursor::new(&segments);
        assert_eq!(cursor.string().ok().flatten(), Some(b"hello".to_vec()));
        assert_eq!(cursor.peek(), Some(b')'));
        cursor.bump();
        assert!(cursor.finish().is_ok());
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("{}{}", "(".repeat(40), ")".repeat(40));
        let segments = vec![Segment::Text(deep)];
        let mut cursor = Cursor::new(&segments);
        assert!(cursor.skip_value().is_err());
    }

    #[test]
    fn trailing_data_is_refused() {
        let segments = vec![Segment::Text("OK done ".to_owned()), Segment::Literal(b"x".to_vec()), Segment::Text(String::new())];
        let mut cursor = Cursor::new(&segments);
        assert_eq!(cursor.atom().as_deref(), Ok("OK"));
        assert!(cursor.finish().is_err());
    }
}
