//! The search query language.
//!
//! One hand-written recursive-descent parser, one AST and one canonical printer. The printer always
//! quotes values and parenthesises every non-leaf node, so `parse(print(ast)) == ast` holds without a
//! special-case list. The parser never interpolates text into SQL or into an FTS expression: the
//! operation layer escapes each value into an FTS5 phrase and the store binds it as `MATCH ?`.

use std::fmt;

use chrono::{NaiveDate, TimeZone, Utc};

/// A query longer than this is refused before parsing.
pub const MAX_QUERY_BYTES: usize = 1024;
/// Deepest parenthesised expression.
pub const MAX_DEPTH: usize = 8;
/// Most primary clauses in one query.
pub const MAX_CLAUSES: usize = 64;
/// Most text terms in one query.
pub const MAX_TERMS: usize = 16;

/// The documented text fields. `label` and `folder` resolve to local ids; `is`, `has`, dates and
/// sizes are typed filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    From,
    To,
    Cc,
    Bcc,
    Subject,
    Label,
    Folder,
    Is,
    Has,
    Before,
    After,
    Larger,
    Smaller,
}

impl Field {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "from" => Some(Self::From),
            "to" => Some(Self::To),
            "cc" => Some(Self::Cc),
            "bcc" => Some(Self::Bcc),
            "subject" => Some(Self::Subject),
            "label" => Some(Self::Label),
            "folder" => Some(Self::Folder),
            "is" => Some(Self::Is),
            "has" => Some(Self::Has),
            "before" => Some(Self::Before),
            "after" => Some(Self::After),
            "larger" => Some(Self::Larger),
            "smaller" => Some(Self::Smaller),
            _ => None,
        }
    }

    /// The canonical name used by the printer and by `matched`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::From => "from",
            Self::To => "to",
            Self::Cc => "cc",
            Self::Bcc => "bcc",
            Self::Subject => "subject",
            Self::Label => "label",
            Self::Folder => "folder",
            Self::Is => "is",
            Self::Has => "has",
            Self::Before => "before",
            Self::After => "after",
            Self::Larger => "larger",
            Self::Smaller => "smaller",
        }
    }

    /// Whether the field is backed by an FTS column.
    pub fn is_text(self) -> bool {
        matches!(self, Self::From | Self::To | Self::Cc | Self::Bcc | Self::Subject)
    }
}

/// The `is:` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsFilter {
    Unread,
    Read,
    Starred,
    Unstarred,
}

impl IsFilter {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "unread" => Some(Self::Unread),
            "read" => Some(Self::Read),
            "starred" => Some(Self::Starred),
            "unstarred" => Some(Self::Unstarred),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::Read => "read",
            Self::Starred => "starred",
            Self::Unstarred => "unstarred",
        }
    }
}

/// A parsed query. `And`/`Or` keep their n-ary shape and `Not` stays explicit, so the printer can
/// round-trip the tree without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    /// A bare term, or a text field followed by a value.
    Text {
        field: Option<Field>,
        value: String,
        prefix: bool,
    },
    Label(String),
    Folder(String),
    Is(IsFilter),
    HasAttachment,
    /// Messages dated on or after this instant (Unix ms, UTC midnight).
    After(i64),
    /// Messages dated strictly before this instant.
    Before(i64),
    Larger(u64),
    Smaller(u64),
}

impl Expr {
    /// The canonical text. Parsing it again yields an equal tree.
    pub fn canonical(&self) -> String {
        self.to_string()
    }

    /// Text values in source order, used for snippets and highlights. Bounded by [`MAX_TERMS`].
    pub fn terms(&self) -> Vec<String> {
        let mut terms = Vec::new();
        self.collect_terms(&mut terms);
        terms
    }

    fn collect_terms(&self, out: &mut Vec<String>) {
        match self {
            Self::And(children) | Self::Or(children) => children.iter().for_each(|child| child.collect_terms(out)),
            Self::Not(child) => child.collect_terms(out),
            Self::Text { value, .. } if out.len() < MAX_TERMS => out.push(value.clone()),
            Self::Text { .. } => {}
            _ => {}
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::And(children) => write_group(formatter, children, " AND "),
            Self::Or(children) => write_group(formatter, children, " OR "),
            Self::Not(child) => write!(formatter, "(NOT ({child}))"),
            Self::Text { field, value, prefix } => {
                if let Some(field) = field {
                    write!(formatter, "{}:", field.as_str())?;
                }
                write_quoted(formatter, value)?;
                if *prefix {
                    formatter.write_str("*")?;
                }
                Ok(())
            }
            Self::Label(value) => write!(formatter, "label:{}", quote(value)),
            Self::Folder(value) => write!(formatter, "folder:{}", quote(value)),
            Self::Is(value) => write!(formatter, "is:{}", value.as_str()),
            Self::HasAttachment => formatter.write_str("has:attachment"),
            Self::After(instant) => write!(formatter, "after:{}", format_date(*instant)),
            Self::Before(instant) => write!(formatter, "before:{}", format_date(*instant)),
            Self::Larger(bytes) => write!(formatter, "larger:{bytes}"),
            Self::Smaller(bytes) => write!(formatter, "smaller:{bytes}"),
        }
    }
}

fn write_group(formatter: &mut fmt::Formatter<'_>, children: &[Expr], separator: &str) -> fmt::Result {
    formatter.write_str("(")?;
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            formatter.write_str(separator)?;
        }
        write!(formatter, "{child}")?;
    }
    formatter.write_str(")")
}

fn write_quoted(formatter: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    formatter.write_str(&quote(value))
}

fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

/// The error a person or an agent can act on. `position` is a byte offset into the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchError {
    pub code: &'static str,
    pub position: usize,
}

impl SearchError {
    fn new(code: &'static str, position: usize) -> Self {
        Self { code, position }
    }

    /// English one-line explanation for CLI and MCP. The desktop UI adds its own Italian sentence.
    pub fn explanation(&self) -> &'static str {
        match self.code {
            "empty" => "the query is empty",
            "too_long" => "the query is longer than 1024 bytes",
            "too_many_clauses" => "the query has more than 64 clauses",
            "too_deep" => "the query nests deeper than 8 parentheses",
            "too_many_terms" => "the query has more than 16 terms",
            "control" => "the query contains a control character",
            "unclosed_quote" => "a quoted value is not closed",
            "unclosed_paren" => "a '(' is not closed",
            "unexpected_token" => "the query has an unexpected token",
            "dangling_operator" => "an AND or OR has no operand",
            "dangling_field" => "a field is missing its value",
            "negative_only" => "NOT needs a positive term",
            "bad_date" => "a date must be a real YYYY-MM-DD day",
            "bad_size" => "a size must be digits with an optional k, m or g suffix",
            "bad_is" => "is: accepts unread, read, starred or unstarred",
            "bad_has" => "has: accepts attachment",
            _ => "the query is not valid",
        }
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at position {}", self.explanation(), self.position)
    }
}

/// The parse result: the tree, and the original text without surrounding whitespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    pub expr: Expr,
    pub text: String,
}

/// Parse one query. All limits are enforced here, before a store sees the query.
pub fn parse(input: &str) -> Result<ParsedQuery, SearchError> {
    if input.len() > MAX_QUERY_BYTES {
        return Err(SearchError::new("too_long", MAX_QUERY_BYTES));
    }
    for (index, character) in input.char_indices() {
        if character.is_control() {
            return Err(SearchError::new("control", index));
        }
    }
    let mut parser = Parser { input, pos: 0, depth: 0, clauses: 0, terms: 0 };
    parser.skip_space();
    if parser.at_end() {
        return Err(SearchError::new("empty", 0));
    }
    let expr = parser.parse_or()?;
    parser.skip_space();
    if !parser.at_end() {
        return Err(SearchError::new("unexpected_token", parser.pos));
    }
    if !has_positive(&expr) {
        return Err(SearchError::new("negative_only", 0));
    }
    Ok(ParsedQuery { expr, text: input.trim().to_owned() })
}

fn has_positive(expr: &Expr) -> bool {
    match expr {
        Expr::And(children) | Expr::Or(children) => children.iter().any(has_positive),
        Expr::Not(_) => false,
        _ => true,
    }
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    depth: usize,
    clauses: usize,
    terms: usize,
}

impl<'a> Parser<'a> {
    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.byte(), Some(b' ' | b'\t')) {
            self.pos = self.pos.saturating_add(1);
        }
    }

    /// Consume one uppercase operator when it is a whole word. Lowercase words are ordinary terms.
    fn keyword(&mut self, expected: &str) -> bool {
        let start = self.pos;
        let mut end = start;
        while let Some(byte) = self.input.as_bytes().get(end) {
            if byte.is_ascii_alphabetic() {
                end = end.saturating_add(1);
            } else {
                break;
            }
        }
        let Some(word) = self.input.get(start..end) else {
            self.pos = start;
            return false;
        };
        let boundary = self.input.as_bytes().get(end).is_none_or(|byte| matches!(byte, b' ' | b'\t' | b'(' | b')'));
        if word == expected && boundary {
            self.pos = end;
            return true;
        }
        self.pos = start;
        false
    }

    fn parse_or(&mut self) -> Result<Expr, SearchError> {
        let mut children = vec![self.parse_and()?];
        loop {
            self.skip_space();
            if !self.keyword("OR") {
                break;
            }
            let operator = self.pos.saturating_sub(2);
            self.skip_space();
            if self.at_end() || self.byte() == Some(b')') {
                return Err(SearchError::new("dangling_operator", operator));
            }
            children.push(self.parse_and()?);
        }
        Ok(if children.len() == 1 { children.remove(0) } else { Expr::Or(children) })
    }

    fn parse_and(&mut self) -> Result<Expr, SearchError> {
        let mut children = vec![self.parse_unary()?];
        loop {
            self.skip_space();
            if self.at_end() || self.byte() == Some(b')') {
                break;
            }
            if self.keyword("OR") {
                self.pos = self.pos.saturating_sub(2);
                break;
            }
            if self.keyword("AND") {
                let operator = self.pos.saturating_sub(3);
                self.skip_space();
                if self.at_end() || self.byte() == Some(b')') {
                    return Err(SearchError::new("dangling_operator", operator));
                }
                if self.keyword("OR") {
                    return Err(SearchError::new("dangling_operator", operator));
                }
                children.push(self.parse_unary()?);
            } else {
                // Juxtaposition is AND.
                children.push(self.parse_unary()?);
            }
        }
        Ok(if children.len() == 1 { children.remove(0) } else { Expr::And(children) })
    }

    fn parse_unary(&mut self) -> Result<Expr, SearchError> {
        self.skip_space();
        if self.keyword("NOT") {
            let position = self.pos.saturating_sub(3);
            self.skip_space();
            if self.at_end() || self.byte() == Some(b')') || self.keyword("AND") || self.keyword("OR") {
                return Err(SearchError::new("dangling_operator", position));
            }
            let child = self.parse_unary()?;
            return Ok(Expr::Not(Box::new(child)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, SearchError> {
        self.skip_space();
        let start = self.pos;
        let checkpoint = self.pos;
        if self.keyword("AND") || self.keyword("OR") {
            self.pos = checkpoint;
            return Err(SearchError::new("dangling_operator", start));
        }
        match self.byte() {
            None => Err(SearchError::new("dangling_operator", start)),
            Some(b')') => Err(SearchError::new("unexpected_token", start)),
            Some(b'(') => {
                self.pos = self.pos.saturating_add(1);
                self.depth = self.depth.saturating_add(1);
                if self.depth > MAX_DEPTH {
                    return Err(SearchError::new("too_deep", start));
                }
                self.skip_space();
                if self.byte() == Some(b')') {
                    return Err(SearchError::new("empty", self.pos));
                }
                let expr = self.parse_or()?;
                self.skip_space();
                if self.byte() != Some(b')') {
                    return Err(SearchError::new("unclosed_paren", start));
                }
                self.pos = self.pos.saturating_add(1);
                self.depth = self.depth.saturating_sub(1);
                Ok(expr)
            }
            Some(_) => self.parse_field_or_term(),
        }
    }

    fn parse_field_or_term(&mut self) -> Result<Expr, SearchError> {
        let start = self.pos;
        let mut end = start;
        while let Some(byte) = self.input.as_bytes().get(end) {
            if byte.is_ascii_alphabetic() {
                end = end.saturating_add(1);
            } else {
                break;
            }
        }
        let field = self.input.get(start..end).filter(|_name| self.input.as_bytes().get(end) == Some(&b':')).and_then(Field::parse);
        let Some(field) = field else {
            return self.parse_term(start);
        };
        self.pos = end.saturating_add(1);
        self.bump_clause(start)?;
        let value_start = self.pos;
        let (value, prefix) = self.parse_value(value_start)?;
        let field_end = self.pos;
        match field {
            Field::From | Field::To | Field::Cc | Field::Bcc | Field::Subject => {
                if value.is_empty() {
                    return Err(SearchError::new("dangling_field", field_end));
                }
                self.bump_term(start)?;
                Ok(Expr::Text { field: Some(field), value, prefix })
            }
            Field::Label => {
                if value.is_empty() {
                    return Err(SearchError::new("dangling_field", field_end));
                }
                Ok(Expr::Label(value))
            }
            Field::Folder => {
                if value.is_empty() {
                    return Err(SearchError::new("dangling_field", field_end));
                }
                Ok(Expr::Folder(value))
            }
            Field::Is => IsFilter::parse(&value.to_ascii_lowercase()).map(Expr::Is).ok_or_else(|| SearchError::new("bad_is", field_end)),
            Field::Has => {
                if value.eq_ignore_ascii_case("attachment") {
                    Ok(Expr::HasAttachment)
                } else {
                    Err(SearchError::new("bad_has", field_end))
                }
            }
            Field::Before => parse_date(&value, field_end).map(Expr::Before),
            Field::After => parse_date(&value, field_end).map(Expr::After),
            Field::Larger => parse_size(&value, field_end).map(Expr::Larger),
            Field::Smaller => parse_size(&value, field_end).map(Expr::Smaller),
        }
    }

    fn parse_term(&mut self, start: usize) -> Result<Expr, SearchError> {
        self.bump_clause(start)?;
        self.bump_term(start)?;
        let (value, prefix) = self.parse_value(start)?;
        if value.is_empty() {
            return Err(SearchError::new("dangling_field", self.pos));
        }
        Ok(Expr::Text { field: None, value, prefix })
    }

    fn bump_clause(&mut self, position: usize) -> Result<(), SearchError> {
        self.clauses = self.clauses.saturating_add(1);
        if self.clauses > MAX_CLAUSES {
            return Err(SearchError::new("too_many_clauses", position));
        }
        Ok(())
    }

    fn bump_term(&mut self, position: usize) -> Result<(), SearchError> {
        self.terms = self.terms.saturating_add(1);
        if self.terms > MAX_TERMS {
            return Err(SearchError::new("too_many_terms", position));
        }
        Ok(())
    }

    /// A quoted value (with `\"` and `\\` escapes) or a bare run up to whitespace, `(` or `)`.
    /// A trailing `*` is the prefix marker and is not part of the value.
    fn parse_value(&mut self, start: usize) -> Result<(String, bool), SearchError> {
        let mut value = String::new();
        let quoted = self.byte() == Some(b'"');
        let mut prefix = false;
        if quoted {
            self.pos = self.pos.saturating_add(1);
            loop {
                let Some(byte) = self.input.as_bytes().get(self.pos).copied() else {
                    return Err(SearchError::new("unclosed_quote", start));
                };
                self.pos = self.pos.saturating_add(1);
                match byte {
                    b'"' => break,
                    b'\\' => {
                        let Some(escaped) = self.input.as_bytes().get(self.pos).copied() else {
                            return Err(SearchError::new("unclosed_quote", start));
                        };
                        self.pos = self.pos.saturating_add(1);
                        match escaped {
                            b'"' | b'\\' => value.push(char::from(escaped)),
                            _ => {
                                value.push('\\');
                                if let Some(character) = self.input.get(self.pos.saturating_sub(1)..).and_then(|rest| rest.chars().next()) {
                                    value.push(character);
                                    self.pos = self.pos.saturating_add(character.len_utf8().saturating_sub(1));
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some(character) = self.input.get(self.pos.saturating_sub(1)..).and_then(|rest| rest.chars().next()) {
                            value.push(character);
                            self.pos = self.pos.saturating_add(character.len_utf8().saturating_sub(1));
                        }
                    }
                }
            }
            if self.byte() == Some(b'*') {
                self.pos = self.pos.saturating_add(1);
                prefix = true;
            }
        } else {
            let value_start = self.pos;
            while let Some(byte) = self.byte() {
                if matches!(byte, b' ' | b'\t' | b'(' | b')') {
                    break;
                }
                self.pos = self.pos.saturating_add(1);
            }
            if let Some(raw) = self.input.get(value_start..self.pos) {
                value.push_str(raw);
            }
            prefix = value.ends_with('*');
            if prefix {
                value.pop();
            }
        }
        Ok((value, prefix))
    }
}

fn parse_date(value: &str, position: usize) -> Result<i64, SearchError> {
    if value.len() != 10 || value.as_bytes().get(4) != Some(&b'-') || value.as_bytes().get(7) != Some(&b'-') {
        return Err(SearchError::new("bad_date", position));
    }
    let year = value.get(0..4).and_then(|text| text.parse::<i32>().ok());
    let month = value.get(5..7).and_then(|text| text.parse::<u32>().ok());
    let day = value.get(8..10).and_then(|text| text.parse::<u32>().ok());
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Err(SearchError::new("bad_date", position));
    };
    if !(1900..=9999).contains(&year) {
        return Err(SearchError::new("bad_date", position));
    }
    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| SearchError::new("bad_date", position))?;
    let instant = date.and_hms_opt(0, 0, 0).map(|naive| Utc.from_utc_datetime(&naive));
    instant.map(|date| date.timestamp_millis()).ok_or_else(|| SearchError::new("bad_date", position))
}

fn format_date(instant: i64) -> String {
    match chrono::DateTime::<Utc>::from_timestamp_millis(instant) {
        Some(date) => date.format("%Y-%m-%d").to_string(),
        None => "1970-01-01".to_owned(),
    }
}

fn parse_size(value: &str, position: usize) -> Result<u64, SearchError> {
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (value.get(..value.len().saturating_sub(1)).unwrap_or(""), 1024u64),
        Some(b'm' | b'M') => (value.get(..value.len().saturating_sub(1)).unwrap_or(""), 1024u64.saturating_mul(1024)),
        Some(b'g' | b'G') => (value.get(..value.len().saturating_sub(1)).unwrap_or(""), 1024u64.saturating_mul(1024).saturating_mul(1024)),
        _ => (value, 1u64),
    };
    if digits.is_empty() || !digits.as_bytes().iter().all(u8::is_ascii_digit) {
        return Err(SearchError::new("bad_size", position));
    }
    let base = digits.parse::<u64>().map_err(|_| SearchError::new("bad_size", position))?;
    base.checked_mul(multiplier).ok_or_else(|| SearchError::new("bad_size", position))
}

/// A plain-text snippet, never HTML. The window is at most 240 characters and highlights are the
/// terms that occur inside it, in source order, capped at eight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub highlights: Vec<String>,
}

pub fn make_snippet(subject: &str, body: &str, terms: &[String]) -> Snippet {
    let subject_chars = normalised_chars(subject, 240);
    if first_match(&subject_chars, terms).is_some() {
        let text = take(&subject_chars, 0, 240);
        return Snippet { highlights: highlights_in(&text, terms), text };
    }
    let body_chars = normalised_chars(body, 16_384);
    let (start, matched) = match first_match(&body_chars, terms) {
        Some((start, length)) => (start.saturating_sub(60), Some(length)),
        None => (0, None),
    };
    let end = start.saturating_add(240).min(body_chars.len());
    let mut text = take(&body_chars, start, end);
    if body_chars.len() > end {
        text.push('…');
    }
    if start > 0 {
        text.insert(0, '…');
    }
    if matched.is_none() && terms.is_empty() {
        text = take(&subject_chars, 0, 240);
    }
    if text.chars().count() > 240 {
        text = text.chars().take(239).collect::<String>();
        text.push('…');
    }
    let highlights = highlights_in(&text, terms);
    Snippet { text, highlights }
}

fn normalised_chars(text: &str, limit: usize) -> Vec<char> {
    let mut chars = Vec::with_capacity(limit.min(text.len()));
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() || character.is_control() || is_format_control(character) {
            pending_space = !chars.is_empty();
            continue;
        }
        if pending_space {
            chars.push(' ');
            pending_space = false;
        }
        if chars.len() >= limit {
            break;
        }
        chars.push(character);
    }
    chars
}

fn is_format_control(character: char) -> bool {
    matches!(character, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

fn take(chars: &[char], start: usize, end: usize) -> String {
    let start = start.min(chars.len());
    let end = end.min(chars.len()).max(start);
    chars.get(start..end).map(|slice| slice.iter().collect()).unwrap_or_default()
}

fn first_match(haystack: &[char], terms: &[String]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for term in terms {
        let needle = term.chars().map(lower).collect::<Vec<_>>();
        if needle.is_empty() || needle.len() > haystack.len() {
            continue;
        }
        for start in 0..=haystack.len().saturating_sub(needle.len()) {
            let candidate = haystack.get(start..start.saturating_add(needle.len()));
            if candidate.is_some_and(|slice| slice.iter().copied().map(lower).eq(needle.iter().copied())) {
                if best.is_none_or(|(current, _)| start < current) {
                    best = Some((start, needle.len()));
                }
                break;
            }
        }
    }
    best
}

fn lower(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

fn highlights_in(text: &str, terms: &[String]) -> Vec<String> {
    let haystack = text.chars().map(lower).collect::<Vec<_>>();
    let mut highlights = Vec::new();
    for term in terms {
        if highlights.len() >= 8 {
            break;
        }
        let needle = term.chars().map(lower).collect::<Vec<_>>();
        if needle.is_empty() || needle.len() > haystack.len() {
            continue;
        }
        let matched = (0..=haystack.len().saturating_sub(needle.len()))
            .any(|start| haystack.get(start..start.saturating_add(needle.len())).is_some_and(|slice| slice == needle.as_slice()));
        if matched {
            let value = term.chars().take(64).collect::<String>();
            if !highlights.contains(&value) {
                highlights.push(value);
            }
        }
    }
    highlights
}

/// The FTS5 phrase for one value. The value is always a quoted phrase; a `"` inside it is doubled,
/// which is FTS5's own escape. Only the optional `*` prefix marker is added outside the quotes, and
/// the column name is a fixed identifier from [`Field`], never caller text.
pub fn fts_phrase(column: Option<&str>, value: &str, prefix: bool) -> String {
    let escaped = value.replace('"', "\"\"");
    let mut phrase = String::with_capacity(escaped.len().saturating_add(4));
    phrase.push('"');
    phrase.push_str(&escaped);
    phrase.push('"');
    if prefix {
        phrase.push('*');
    }
    match column {
        Some(column) => format!("{column} : {phrase}"),
        None => phrase,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(input: &str) -> Expr {
        parse(input).unwrap_or_else(|error| panic!("{input:?} must parse: {error}")).expr
    }

    #[test]
    fn canonical_print_round_trips_a_grammar_matrix() {
        let queries = [
            "from:giulia",
            "subject:\"fattura 2026\"",
            "label:\"Lavoro urgente\"",
            "folder:archive",
            "is:unread",
            "has:attachment",
            "after:2026-09-01",
            "larger:2m",
            "from:giulia OR subject:fattura",
            "from:giulia is:unread",
            "NOT from:spam from:giulia",
            "from:giulia AND NOT (subject:spam OR label:junk)",
            "foo*",
            "\"foo \\\"bar\\\"\"",
            "http://example.com Re:",
            "(is:unread OR is:starred) has:attachment",
        ];
        for query in queries {
            let expr = parse_ok(query);
            let printed = expr.canonical();
            assert_eq!(parse_ok(&printed), expr, "{query:?} printed as {printed:?}");
        }
    }

    #[test]
    fn operators_are_uppercase_and_juxtaposition_is_and() {
        let expr = parse_ok("from:giulia subject:fattura");
        assert!(matches!(expr, Expr::And(children) if children.len() == 2));
        let expr = parse_ok("from:giulia or subject:fattura");
        assert!(matches!(expr, Expr::And(ref children) if children.len() == 3), "{expr:?}");
        let expr = parse_ok("foo:bar");
        assert!(matches!(expr, Expr::Text { field: None, ref value, .. } if value == "foo:bar"), "unknown fields stay text");
        let expr = parse_ok("from:giulia NOT from:spam");
        assert!(matches!(expr, Expr::And(ref children) if matches!(children.get(1), Some(Expr::Not(_)))), "{expr:?}");
        let expr = parse_ok("is:unread OR is:starred");
        assert!(matches!(expr, Expr::Or(ref children) if children.len() == 2));
    }

    #[test]
    fn errors_carry_a_code_and_a_position() {
        let cases = [
            ("", "empty"),
            ("from:", "dangling_field"),
            ("from:giulia AND", "dangling_operator"),
            ("from:giulia OR AND b", "dangling_operator"),
            ("from:giulia AND AND b", "dangling_operator"),
            ("AND from:giulia", "dangling_operator"),
            ("NOT from:spam", "negative_only"),
            ("(from:giulia", "unclosed_paren"),
            ("subject:\"unclosed", "unclosed_quote"),
            ("is:maybe", "bad_is"),
            ("has:image", "bad_has"),
            ("after:2026-02-30", "bad_date"),
            ("larger:2x", "bad_size"),
            ("from:giulia\x07", "control"),
        ];
        for (query, code) in cases {
            let error = parse(query).expect_err(query);
            assert_eq!(error.code, code, "{query:?} -> {error}");
        }
    }

    #[test]
    fn caps_are_enforced_before_the_store() {
        assert_eq!(parse(&"a".repeat(MAX_QUERY_BYTES + 1)).expect_err("too long").code, "too_long");
        let deep = format!("{}x{}", "(".repeat(MAX_DEPTH + 1), ")".repeat(MAX_DEPTH + 1));
        assert_eq!(parse(&deep).expect_err("too deep").code, "too_deep");
        let clauses = (0..=MAX_CLAUSES).map(|_| "is:unread".to_owned()).collect::<Vec<_>>().join(" ");
        assert_eq!(parse(&clauses).expect_err("too many clauses").code, "too_many_clauses");
        let terms = (0..=MAX_TERMS).map(|index| format!("t{index}")).collect::<Vec<_>>().join(" ");
        assert_eq!(parse(&terms).expect_err("too many terms").code, "too_many_terms");
    }

    #[test]
    fn every_small_composition_round_trips_through_its_canonical_text() {
        let leaves = [
            Expr::Text { field: None, value: "nota".to_owned(), prefix: false },
            Expr::Text { field: Some(Field::Subject), value: "fattura 2026".to_owned(), prefix: false },
            Expr::Label("Lavoro urgente".to_owned()),
            Expr::Is(IsFilter::Unread),
            Expr::Larger(2 * 1024 * 1024),
            Expr::After(1_788_220_800_000),
        ];
        for (index, leaf) in leaves.iter().enumerate() {
            let variants = [
                leaf.clone(),
                Expr::And(vec![leaf.clone(), Expr::Not(Box::new(leaves[(index + 1) % leaves.len()].clone()))]),
                Expr::Or(vec![leaf.clone(), Expr::Not(Box::new(leaves[(index + 2) % leaves.len()].clone()))]),
                Expr::And(vec![leaf.clone(), Expr::Or(vec![leaves[(index + 3) % leaves.len()].clone(), leaves[(index + 4) % leaves.len()].clone()])]),
            ];
            for expr in variants {
                let printed = expr.canonical();
                let parsed = parse(&printed).unwrap_or_else(|error| panic!("{printed:?}: {error}"));
                assert_eq!(parsed.expr, expr, "printed as {printed:?}");
            }
        }
    }

    #[test]
    fn fts_phrases_escape_quotes_and_keep_metacharacters_literal() {
        assert_eq!(fts_phrase(None, "foo bar", false), "\"foo bar\"");
        assert_eq!(fts_phrase(Some("subject"), "a\"b", true), "subject : \"a\"\"b\"*");
        assert_eq!(fts_phrase(None, "AND OR * ( )", false), "\"AND OR * ( )\"");
    }

    #[test]
    fn snippets_strip_format_controls() {
        let snippet = make_snippet("Nota", "testo \u{202E}malevolo\u{202C} fine", &["testo".to_owned()]);
        assert!(!snippet.text.contains('\u{202E}'));
        assert!(!snippet.text.contains('\u{202C}'));
        assert!(snippet.text.contains("malevolo"));
    }

    #[test]
    fn snippets_are_plain_and_bounded() {
        let body = format!("{} fattura 2026 {}", "x".repeat(500), "y".repeat(500));
        let snippet = make_snippet("Nota", &body, &["fattura".to_owned()]);
        assert!(snippet.text.chars().count() <= 240, "{}", snippet.text.len());
        assert!(snippet.text.contains("fattura"));
        assert!(!snippet.text.contains('<'));
        assert_eq!(snippet.highlights, vec!["fattura".to_owned()]);
    }
}
