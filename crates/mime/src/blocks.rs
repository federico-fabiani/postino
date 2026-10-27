//! Bounded block model for a stored, already-sanitized HTML body fragment.
//!
//! The input is [`crate::sanitize_html`] output: the allowlisted subset the store keeps. This module
//! never parses raw HTML, never resolves a URL, never fetches anything and never talks to a window.
//! It is the headless half of the P11a reading-pane renderer: a shell maps [`Block`] to its own
//! widgets, and [`BodyView::Text`] keeps a damaged or oversized body readable.
//!
//! The emitted tree carries text and structure, never markup:
//!
//! - scripts, styles and the other clean-content containers are dropped with their body even if a
//!   caller passes unsanitized input by mistake, and no tag, attribute or `on*` handler is copied;
//! - an image keeps a `cid:` source only, so no remote URL can be painted; a removed remote image
//!   leaves only its alt text;
//! - a link keeps the exact sanitized `href`, including userinfo or a backslash, so the app's P6c
//!   gate (`reading::find_links` / `reading::is_allowed_url`) computes the same effective
//!   destination it would have computed in the text fallback. This module opens nothing.
//!
//! Bounds: [`MAX_BODY_BYTES`] of input, [`MAX_DEPTH`] nested containers, [`MAX_BLOCKS`] blocks,
//! [`MAX_TEXT_CHARS`] of visible text (including link labels and image alt text), [`MAX_TABLE_ROWS`]
//! and [`MAX_TABLE_COLUMNS`] per table, [`MAX_LIST_ITEMS`] items per list, [`MAX_LINK_BYTES`] per
//! link and image source and [`MAX_ALT_BYTES`] per alt text. Anything clipped sets
//! [`Body::truncated`]; the scan is linear and never recurses on input nesting.

use crate::html_text::{Tag, decode_entities, find_ci, html_to_text, parse_tag, tag_end};
use crate::sanitize::CLEAN_CONTENT_TAGS;
use crate::util::truncate_chars;

/// Largest sanitized fragment accepted by [`render_body`].
///
/// It matches [`crate::Limits::HARD_MAX_HTML_BYTES`], the ceiling a configured sanitizer may use, so
/// a legitimate stored body is never rejected. A larger input becomes a bounded text fallback
/// instead of being parsed.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Maximum nested block depth (quote, list item, table cell). Deeper input is flattened into the
/// current container and marks the body truncated.
pub const MAX_DEPTH: usize = 32;

/// Maximum number of blocks in one rendered body. A hostile body of many tiny paragraphs stops here.
pub const MAX_BLOCKS: usize = 4096;

/// Maximum visible characters across every text run in one body.
pub const MAX_TEXT_CHARS: usize = 1 << 20;

/// Maximum rows kept from one table.
pub const MAX_TABLE_ROWS: usize = 256;

/// Maximum cells kept in one table row.
pub const MAX_TABLE_COLUMNS: usize = 32;

/// Maximum items kept in one list, per nesting level.
pub const MAX_LIST_ITEMS: usize = 256;

/// Maximum bytes of one link destination or `cid:` image source.
pub const MAX_LINK_BYTES: usize = 4096;

/// Maximum bytes of one image alt text. A longer `alt` is clipped and the body is truncated.
pub const MAX_ALT_BYTES: usize = 1024;

/// Characters of a fallback body fed to [`html_to_text`] before it is scanned.
const MAX_FALLBACK_INPUT_CHARS: usize = 256 * 1024;

/// Characters kept from a fallback body.
const MAX_FALLBACK_CHARS: usize = 64 * 1024;

/// Result of [`render_body`]: the block tree, or the bounded text fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyView {
    /// A parsable body, mapped by the shell to text, headings, lists and tables.
    Blocks(Body),
    /// A body that must stay text, with the reason to show next to the fallback.
    Text(TextFallback),
}

/// A parsed body with its block list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    /// Top-level blocks in reading order.
    pub blocks: Vec<Block>,
    /// Whether a bound cut the input or the output. The shell can show a short "messaggio
    /// troncato" note instead of pretending the body is complete.
    pub truncated: bool,
}

/// Why [`render_body`] returned text instead of blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextReason {
    /// The fragment had no visible content (empty, whitespace only, or only decorative images).
    Empty,
    /// The fragment is larger than [`MAX_BODY_BYTES`]; only a bounded text prefix is returned.
    TooLarge,
}

/// The text fallback for a body [`render_body`] cannot turn into blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFallback {
    /// Readable text, already free of markup, entities and control characters.
    pub text: String,
    /// Why the fallback was used.
    pub reason: TextReason,
}

/// One block of the reading pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// One paragraph of inline content.
    Paragraph(Vec<Inline>),
    /// A heading. Only levels 1 to 3 exist; `h4`–`h6` are clamped to [`HeadingLevel::Three`].
    Heading {
        /// Visual level, 1 to 3.
        level: HeadingLevel,
        /// Heading text with its inline emphasis.
        inlines: Vec<Inline>,
    },
    /// An ordered or unordered list. Each item is a small block list, so nested lists and
    /// paragraphs inside an item survive.
    List(List),
    /// A blockquote: a nested block list.
    Quote(Vec<Block>),
    /// A preformatted block. Whitespace and line breaks are preserved verbatim; no markup remains.
    Pre(String),
    /// A simple table with an optional caption.
    Table(Table),
    /// A horizontal rule.
    Rule,
}

/// Heading levels the model exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeadingLevel {
    /// `h1`.
    One,
    /// `h2`.
    Two,
    /// `h3`, and any deeper heading tag.
    Three,
}

/// A list block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    /// Whether the source was `<ol>`.
    pub ordered: bool,
    /// Items in source order; each item is a bounded block list.
    pub items: Vec<Vec<Block>>,
}

/// A simple table. The sanitizer removes spans, so every row has at most [`MAX_TABLE_COLUMNS`]
/// cells and no cell merges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Caption text, empty when the source had none.
    pub caption: Vec<Inline>,
    /// Rows in source order.
    pub rows: Vec<TableRow>,
}

/// One table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// Cells in source order.
    pub cells: Vec<TableCell>,
}

/// One table cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    /// Whether the cell came from a `<th>` or a row inside `<thead>`.
    pub header: bool,
    /// Cell content flattened to inline runs, one per paragraph, with a [`Inline::Break`] between
    /// blocks. Nested tables are flattened to `a | b` text.
    pub inlines: Vec<Inline>,
}

/// One inline run inside a paragraph, heading or table cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    /// Plain text with its emphasis.
    Text(TextRun),
    /// A link: the visible label and the exact sanitized destination.
    Link(Link),
    /// A `cid:` image shown as its alt text. The shell never paints a remote source.
    Image(Image),
    /// A line break (`br`).
    Break,
}

/// A styled text run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    /// Visible text. Whitespace is collapsed and entities are decoded.
    pub text: String,
    /// Emphasis carried by the source markup.
    pub style: Style,
}

/// The emphasis a run inherits from its ancestors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    /// `b`, `strong`.
    pub bold: bool,
    /// `i`, `em`.
    pub italic: bool,
    /// `code`, `kbd`, `samp`, `tt`.
    pub mono: bool,
    /// `u`.
    pub underline: bool,
    /// `s`, `strike`, `del`.
    pub strike: bool,
}

/// A link destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Visible label, with whitespace collapsed. Empty labels are never emitted.
    pub text: String,
    /// The sanitized destination exactly as stored: no rewriting, no decoding, no normalisation.
    /// The app must still pass it through the P6c gate before opening it.
    pub href: String,
}

/// A `cid:` image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Alt text; may be empty for a decorative image.
    pub alt: String,
    /// Local `cid:` source, normalised to lowercase scheme. Never `http`, `https` or `data`.
    pub src: String,
}

impl Inline {
    /// The inline run as plain text. A link appends its destination in angle brackets, matching
    /// [`crate::html_to_text`].
    #[must_use]
    pub fn plain_text(&self) -> String {
        match self {
            Inline::Text(run) => run.text.clone(),
            Inline::Link(link) => format!("{} <{}>", link.text, link.href),
            Inline::Image(image) => image.alt.clone(),
            Inline::Break => "\n".to_owned(),
        }
    }
}

impl Block {
    /// The block as plain text, one block per line. Useful for copy, search snippets and tests.
    #[must_use]
    pub fn plain_text(&self) -> String {
        inlines_to_text(&flatten_blocks(std::slice::from_ref(self)))
    }
}

impl Body {
    /// The whole body as plain text, one block per line.
    #[must_use]
    pub fn plain_text(&self) -> String {
        inlines_to_text(&flatten_blocks(&self.blocks))
    }
}

/// Parse a stored, already-sanitized HTML fragment into a bounded block tree.
///
/// The parser is deliberately more tolerant than an HTML DOM: unclosed tags are closed at the end
/// of the input, stray end tags are ignored and an unknown element is unwrapped so its text stays
/// readable. It is never allowed to panic, recurse on input nesting or allocate without a bound.
#[must_use]
pub fn render_body(html: &str) -> BodyView {
    if html.len() > MAX_BODY_BYTES {
        let limited = fallback_text(truncate_chars(html, MAX_FALLBACK_INPUT_CHARS));
        return BodyView::Text(TextFallback { text: limited, reason: TextReason::TooLarge });
    }
    let body = Parser::new().parse(html);
    if body.blocks.is_empty() {
        return BodyView::Text(TextFallback { text: fallback_text(html), reason: TextReason::Empty });
    }
    BodyView::Blocks(body)
}

fn fallback_text(html: &str) -> String {
    let text = html_to_text(html);
    truncate_chars(&text, MAX_FALLBACK_CHARS).to_owned()
}

#[derive(Clone, Copy, Default)]
struct StyleCounts {
    bold: u8,
    italic: u8,
    mono: u8,
    underline: u8,
    strike: u8,
}

impl StyleCounts {
    fn style(self) -> Style {
        Style { bold: self.bold > 0, italic: self.italic > 0, mono: self.mono > 0, underline: self.underline > 0, strike: self.strike > 0 }
    }

    fn open(&mut self, name: &str) {
        match name {
            "b" | "strong" => self.bold = self.bold.saturating_add(1),
            "i" | "em" => self.italic = self.italic.saturating_add(1),
            "code" | "kbd" | "samp" | "tt" => self.mono = self.mono.saturating_add(1),
            "u" => self.underline = self.underline.saturating_add(1),
            "s" | "strike" | "del" => self.strike = self.strike.saturating_add(1),
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "b" | "strong" => self.bold = self.bold.saturating_sub(1),
            "i" | "em" => self.italic = self.italic.saturating_sub(1),
            "code" | "kbd" | "samp" | "tt" => self.mono = self.mono.saturating_sub(1),
            "u" => self.underline = self.underline.saturating_sub(1),
            "s" | "strike" | "del" => self.strike = self.strike.saturating_sub(1),
            _ => {}
        }
    }
}

struct PendingLink {
    href: String,
    text: String,
}

#[derive(Default)]
struct Container {
    blocks: Vec<Block>,
    para: Vec<Inline>,
    link: Option<PendingLink>,
    style: StyleCounts,
    pre: String,
    pending_space: bool,
}

impl Container {
    /// Append a text node with normal HTML whitespace collapsing.
    fn text(&mut self, raw: &str, remaining: usize) -> (usize, bool) {
        let mut decoded = String::new();
        decode_entities(raw, &mut decoded);
        let mut used = 0usize;
        for character in decoded.chars() {
            if character.is_whitespace() {
                self.pending_space = true;
                continue;
            }
            if character.is_control() {
                continue;
            }
            if used >= remaining {
                return (used, true);
            }
            self.push_char(character);
            used = used.saturating_add(1);
        }
        (used, false)
    }

    /// Append a text node inside `<pre>`, preserving spaces and line breaks.
    fn pre_text(&mut self, raw: &str, remaining: usize) -> (usize, bool) {
        let mut decoded = String::new();
        decode_entities(raw, &mut decoded);
        let mut used = 0usize;
        let mut chars = decoded.chars().peekable();
        while let Some(character) = chars.next() {
            let character = match character {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    '\n'
                }
                '\n' | '\t' => character,
                other if other.is_control() => continue,
                other => other,
            };
            if used >= remaining {
                return (used, true);
            }
            self.pre.push(character);
            used = used.saturating_add(1);
        }
        (used, false)
    }

    /// Append alt text of a dropped image without decoding entities a second time; [`parse_tag`]
    /// already decoded it. Charges the visible-text budget like a text node.
    fn alt_text(&mut self, alt: &str, remaining: usize) -> (usize, bool) {
        let mut used = 0usize;
        for character in alt.chars() {
            if character.is_whitespace() {
                self.pending_space = true;
            } else if !character.is_control() {
                if used >= remaining {
                    return (used, true);
                }
                self.push_char(character);
                used = used.saturating_add(1);
            }
        }
        (used, false)
    }

    fn push_char(&mut self, character: char) {
        let leading_space = self.pending_space;
        let style = self.style.style();
        if let Some(link) = self.link.as_mut() {
            if leading_space && !link.text.is_empty() {
                link.text.push(' ');
            }
            link.text.push(character);
        } else {
            if leading_space && !self.para.is_empty() {
                self.push_separator();
            }
            let mut buffer = [0u8; 4];
            self.push_run(character.encode_utf8(&mut buffer), style);
        }
        self.pending_space = false;
    }

    fn push_run(&mut self, text: &str, style: Style) {
        if let Some(Inline::Text(run)) = self.para.last_mut()
            && run.style == style
        {
            run.text.push_str(text);
            return;
        }
        self.para.push(Inline::Text(TextRun { text: text.to_owned(), style }));
    }

    /// Emits a collapsed space in the previous run's style, so a style boundary
    /// does not start the next run with a leading space.
    fn push_separator(&mut self) {
        let style = self.style.style();
        if let Some(Inline::Text(run)) = self.para.last_mut()
            && (run.style == style || run.style == Style::default())
        {
            run.text.push(' ');
            return;
        }
        self.para.push(Inline::Text(TextRun { text: " ".to_owned(), style: Style::default() }));
    }

    /// Append a `cid:` image. Its alt text is visible content, so it is charged to the budget and
    /// clipped like any other text run.
    fn push_image(&mut self, alt: &str, remaining: usize, src: String) -> (usize, bool) {
        self.flush_link();
        let mut kept = String::new();
        let mut used = 0usize;
        for character in alt.chars() {
            if character.is_control() {
                continue;
            }
            if used >= remaining {
                break;
            }
            kept.push(character);
            used = used.saturating_add(1);
        }
        let clipped = used < alt.chars().filter(|character| !character.is_control()).count();
        self.para.push(Inline::Image(Image { alt: kept, src }));
        self.pending_space = true;
        (used, clipped)
    }

    fn push_break(&mut self) {
        self.flush_link();
        if !self.para.is_empty() && !matches!(self.para.last(), Some(Inline::Break)) {
            self.para.push(Inline::Break);
        }
        self.pending_space = false;
    }

    fn flush_link(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        let text = link.text.trim_end();
        if text.is_empty() {
            return;
        }
        self.para.push(Inline::Link(Link { text: text.to_owned(), href: link.href }));
        self.pending_space = true;
    }

    fn finish_inlines(&mut self) -> Vec<Inline> {
        self.flush_link();
        self.pending_space = false;
        while let Some(Inline::Text(run)) = self.para.last_mut() {
            let trimmed = run.text.trim_end().len();
            if trimmed == 0 {
                self.para.pop();
            } else {
                run.text.truncate(trimmed);
                break;
            }
        }
        let first_empty = match self.para.first_mut() {
            Some(Inline::Text(run)) => {
                let leading = run.text.len().saturating_sub(run.text.trim_start().len());
                if leading > 0 {
                    run.text.drain(..leading);
                }
                run.text.is_empty()
            }
            _ => false,
        };
        if first_empty {
            self.para.remove(0);
        }
        while matches!(self.para.last(), Some(Inline::Break)) {
            self.para.pop();
        }
        std::mem::take(&mut self.para)
    }

    fn finish_paragraph(&mut self) -> Option<Block> {
        let inlines = self.finish_inlines();
        has_visible(&inlines).then_some(Block::Paragraph(inlines))
    }
}

enum Kind {
    Root,
    Quote,
    List { ordered: bool, items: Vec<Vec<Block>> },
    Item,
    Table { head: bool, rows: Vec<TableRow> },
    Row { head: bool, cells: Vec<TableCell> },
    Cell { header: bool },
    Heading { level: HeadingLevel },
    Pre,
}

struct Frame {
    kind: Kind,
    container: Container,
}

struct Parser {
    frames: Vec<Frame>,
    blocks: usize,
    chars: usize,
    truncated: bool,
    stopped: bool,
}

impl Parser {
    fn new() -> Self {
        Self { frames: vec![Frame { kind: Kind::Root, container: Container::default() }], blocks: 0, chars: 0, truncated: false, stopped: false }
    }

    fn parse(mut self, html: &str) -> Body {
        self.scan(html);
        while self.frames.len() > 1 {
            let before = self.frames.len();
            self.close_top();
            if self.frames.len() == before {
                // Defensive: every closer removes its frame, so this only guards a future edit.
                self.frames.pop();
            }
        }
        self.flush_para();
        let root = match self.frames.pop() {
            Some(frame) => frame.container.blocks,
            None => Vec::new(),
        };
        Body { blocks: root, truncated: self.truncated }
    }

    fn scan(&mut self, html: &str) {
        let mut index = 0usize;
        let mut skip: Option<String> = None;
        while index < html.len() && !self.stopped {
            if let Some(tag) = skip.as_deref() {
                let needle = format!("</{tag}");
                match find_ci(html.get(index..).unwrap_or_default(), &needle) {
                    Some(offset) => {
                        index = index.saturating_add(offset);
                        skip = None;
                        continue;
                    }
                    None => break,
                }
            }
            let Some(offset) = html.get(index..).and_then(|rest| rest.find('<')) else {
                if let Some(text) = html.get(index..) {
                    self.text(text);
                }
                break;
            };
            let start = index.saturating_add(offset);
            if let Some(text) = html.get(index..start) {
                self.text(text);
            }
            if html.get(start..).is_some_and(|rest| rest.starts_with("<!--")) {
                match html.get(start..).and_then(|rest| rest.find("-->")) {
                    Some(end) => {
                        index = start.saturating_add(end).saturating_add(3);
                        continue;
                    }
                    None => {
                        // An unclosed comment swallows the rest of the fragment.
                        self.truncated = true;
                        break;
                    }
                }
            }
            let Some(end) = tag_end(html, start) else {
                // An unterminated tag at the end of the fragment: drop the tail, keep what was read.
                self.truncated = true;
                break;
            };
            let Some(tag) = parse_tag(html.get(start..=end).unwrap_or_default()) else {
                index = end.saturating_add(1);
                continue;
            };
            index = end.saturating_add(1);
            if tag.closing {
                self.end_tag(&tag.name);
            } else if CLEAN_CONTENT_TAGS.contains(&tag.name.as_str()) {
                skip = Some(tag.name);
            } else {
                self.start_tag(&tag);
            }
        }
    }

    fn text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let remaining = MAX_TEXT_CHARS.saturating_sub(self.chars);
        let (used, clipped) = match self.frames.last_mut() {
            Some(frame) => {
                if matches!(&frame.kind, Kind::Pre) {
                    frame.container.pre_text(raw, remaining)
                } else {
                    frame.container.text(raw, remaining)
                }
            }
            None => (0, false),
        };
        self.chars = self.chars.saturating_add(used);
        if clipped {
            self.truncated = true;
            self.stopped = true;
        }
    }

    fn start_tag(&mut self, tag: &Tag) {
        match tag.name.as_str() {
            "p" | "div" | "figure" | "figcaption" | "dl" | "dt" | "dd" | "address" | "center" | "section" | "article" | "header" | "footer"
            | "main" | "aside" | "nav" => self.flush_para(),
            "h1" => self.start_heading(HeadingLevel::One),
            "h2" => self.start_heading(HeadingLevel::Two),
            "h3" | "h4" | "h5" | "h6" => self.start_heading(HeadingLevel::Three),
            "blockquote" => self.open(Kind::Quote),
            "pre" => self.open(Kind::Pre),
            "ul" => self.open(Kind::List { ordered: false, items: Vec::new() }),
            "ol" => self.open(Kind::List { ordered: true, items: Vec::new() }),
            "li" => self.start_item(),
            "table" => self.open(Kind::Table { head: false, rows: Vec::new() }),
            "thead" => self.set_table_head(true),
            "tbody" | "tfoot" => self.set_table_head(false),
            "tr" => self.start_row(),
            "td" => self.start_cell(false),
            "th" => self.start_cell(true),
            "caption" => self.flush_para(),
            "br" => self.line_break(),
            "hr" => {
                self.flush_para();
                self.block(Block::Rule);
            }
            "img" => self.image(tag),
            "a" => self.start_link(tag),
            "b" | "strong" | "i" | "em" | "u" | "s" | "strike" | "del" | "code" | "kbd" | "samp" | "tt" => {
                if let Some(frame) = self.frames.last_mut() {
                    frame.container.style.open(&tag.name);
                }
            }
            // `span`, `small`, `sub`, `sup`, `col`, `colgroup` and every unknown element are
            // transparent: their text stays, their tag does not.
            _ => {}
        }
    }

    fn end_tag(&mut self, name: &str) {
        match name {
            "p" | "div" | "figure" | "figcaption" | "dl" | "dt" | "dd" | "address" | "center" | "section" | "article" | "header" | "footer"
            | "main" | "aside" | "nav" | "caption" => self.flush_para(),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.close_heading(),
            "blockquote" => self.close_quote(),
            "pre" => self.close_pre(),
            "ul" | "ol" => self.close_list(),
            "li" => self.close_item(),
            "table" => self.close_table(),
            "thead" | "tbody" | "tfoot" => self.set_table_head(false),
            "tr" => self.close_row(),
            "td" | "th" => self.close_cell(),
            "br" | "hr" | "img" | "col" | "colgroup" => {}
            "a" => {
                if let Some(frame) = self.frames.last_mut() {
                    frame.container.flush_link();
                }
            }
            "b" | "strong" | "i" | "em" | "u" | "s" | "strike" | "del" | "code" | "kbd" | "samp" | "tt" => {
                if let Some(frame) = self.frames.last_mut() {
                    frame.container.style.close(name);
                }
            }
            _ => {}
        }
    }

    fn top_kind(&self) -> Option<&Kind> {
        self.frames.last().map(|frame| &frame.kind)
    }

    fn open(&mut self, kind: Kind) {
        self.flush_para();
        self.push_frame(kind);
    }

    fn push_frame(&mut self, kind: Kind) {
        if self.frames.len() >= MAX_DEPTH {
            self.truncated = true;
            return;
        }
        let style = self.frames.last().map(|frame| frame.container.style).unwrap_or_default();
        self.frames.push(Frame { kind, container: Container { style, ..Container::default() } });
    }

    fn block(&mut self, block: Block) {
        if self.blocks >= MAX_BLOCKS {
            self.truncated = true;
            self.stopped = true;
            return;
        }
        self.blocks = self.blocks.saturating_add(1);
        if let Some(frame) = self.frames.last_mut() {
            frame.container.blocks.push(block);
        }
    }

    fn flush_para(&mut self) {
        let paragraph = match self.frames.last_mut() {
            Some(frame) => frame.container.finish_paragraph(),
            None => None,
        };
        if let Some(paragraph) = paragraph {
            self.block(paragraph);
        }
    }

    fn line_break(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            if matches!(&frame.kind, Kind::Pre) {
                frame.container.pre.push('\n');
            } else {
                frame.container.push_break();
            }
        }
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        self.flush_para();
        self.push_frame(Kind::Heading { level });
    }

    fn close_heading(&mut self) {
        let level = match self.top_kind() {
            Some(Kind::Heading { level }) => *level,
            _ => return,
        };
        let Some(mut frame) = self.frames.pop() else {
            return;
        };
        let inlines = frame.container.finish_inlines();
        if has_visible(&inlines) {
            self.block(Block::Heading { level, inlines });
        }
    }

    fn close_quote(&mut self) {
        if !matches!(self.top_kind(), Some(Kind::Quote)) {
            return;
        }
        self.flush_para();
        let Some(frame) = self.frames.pop() else {
            return;
        };
        if !frame.container.blocks.is_empty() {
            self.block(Block::Quote(frame.container.blocks));
        }
    }

    fn close_pre(&mut self) {
        if !matches!(self.top_kind(), Some(Kind::Pre)) {
            return;
        }
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let text = trim_pre(&frame.container.pre);
        if !text.is_empty() {
            self.block(Block::Pre(text));
        }
    }

    fn set_table_head(&mut self, head: bool) {
        if let Some(frame) = self.frames.last_mut()
            && let Kind::Table { head: current, .. } = &mut frame.kind
        {
            *current = head;
        }
    }

    fn start_item(&mut self) {
        if matches!(self.top_kind(), Some(Kind::Item)) {
            self.close_item();
        }
        if !matches!(self.top_kind(), Some(Kind::List { .. })) {
            return;
        }
        let mut overflow = false;
        if let Some(frame) = self.frames.last_mut() {
            let stray = frame.container.finish_paragraph();
            if let Kind::List { items, .. } = &mut frame.kind
                && let Some(stray) = stray
            {
                if items.len() < MAX_LIST_ITEMS {
                    items.push(vec![stray]);
                } else {
                    overflow = true;
                }
            }
        }
        if overflow {
            self.truncated = true;
        }
        self.push_frame(Kind::Item);
    }

    fn close_item(&mut self) {
        if !matches!(self.top_kind(), Some(Kind::Item)) {
            return;
        }
        self.flush_para();
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let blocks = frame.container.blocks;
        if blocks.is_empty() {
            return;
        }
        let mut overflow = false;
        if let Some(parent) = self.frames.last_mut()
            && let Kind::List { items, .. } = &mut parent.kind
        {
            if items.len() < MAX_LIST_ITEMS {
                items.push(blocks);
            } else {
                overflow = true;
            }
        }
        if overflow {
            self.truncated = true;
        }
    }

    fn close_list(&mut self) {
        if matches!(self.top_kind(), Some(Kind::Item)) {
            self.close_item();
        }
        if !matches!(self.top_kind(), Some(Kind::List { .. })) {
            return;
        }
        let Some(mut frame) = self.frames.pop() else {
            return;
        };
        let Kind::List { ordered, mut items } = frame.kind else {
            return;
        };
        if let Some(stray) = frame.container.finish_paragraph() {
            if items.len() < MAX_LIST_ITEMS {
                items.push(vec![stray]);
            } else {
                self.truncated = true;
            }
        }
        if !frame.container.blocks.is_empty() {
            if items.len() < MAX_LIST_ITEMS {
                items.push(frame.container.blocks);
            } else {
                // Content after a full list was dropped, not moved: the body is not complete.
                self.truncated = true;
            }
        }
        if !items.is_empty() {
            self.block(Block::List(List { ordered, items }));
        }
    }

    fn start_row(&mut self) {
        if matches!(self.top_kind(), Some(Kind::Cell { .. })) {
            self.close_cell();
        }
        if matches!(self.top_kind(), Some(Kind::Row { .. })) {
            self.close_row();
        }
        let head = match self.top_kind() {
            Some(Kind::Table { head, .. }) => *head,
            _ => return,
        };
        self.flush_para();
        self.push_frame(Kind::Row { head, cells: Vec::new() });
    }

    fn close_row(&mut self) {
        if matches!(self.top_kind(), Some(Kind::Cell { .. })) {
            self.close_cell();
        }
        if !matches!(self.top_kind(), Some(Kind::Row { .. })) {
            return;
        }
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let Kind::Row { cells, .. } = frame.kind else {
            return;
        };
        if cells.is_empty() {
            return;
        }
        let mut overflow = false;
        if let Some(parent) = self.frames.last_mut()
            && let Kind::Table { rows, .. } = &mut parent.kind
        {
            if rows.len() < MAX_TABLE_ROWS {
                rows.push(TableRow { cells });
            } else {
                overflow = true;
            }
        }
        if overflow {
            self.truncated = true;
        }
    }

    fn start_cell(&mut self, header: bool) {
        if matches!(self.top_kind(), Some(Kind::Cell { .. })) {
            self.close_cell();
        }
        if matches!(self.top_kind(), Some(Kind::Table { .. })) {
            self.start_row();
        }
        let row_head = match self.top_kind() {
            Some(Kind::Row { head, .. }) => *head,
            _ => return,
        };
        self.flush_para();
        self.push_frame(Kind::Cell { header: header || row_head });
    }

    fn close_cell(&mut self) {
        if !matches!(self.top_kind(), Some(Kind::Cell { .. })) {
            return;
        }
        self.flush_para();
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let header = matches!(frame.kind, Kind::Cell { header: true });
        let inlines = flatten_blocks(&frame.container.blocks);
        let mut overflow = false;
        if let Some(parent) = self.frames.last_mut()
            && let Kind::Row { cells, .. } = &mut parent.kind
        {
            if cells.len() < MAX_TABLE_COLUMNS {
                cells.push(TableCell { header, inlines });
            } else {
                overflow = true;
            }
        }
        if overflow {
            self.truncated = true;
        }
    }

    fn close_table(&mut self) {
        loop {
            if matches!(self.top_kind(), Some(Kind::Cell { .. })) {
                self.close_cell();
            } else if matches!(self.top_kind(), Some(Kind::Row { .. })) {
                self.close_row();
            } else {
                break;
            }
        }
        if !matches!(self.top_kind(), Some(Kind::Table { .. })) {
            return;
        }
        let Some(mut frame) = self.frames.pop() else {
            return;
        };
        let Kind::Table { rows, .. } = frame.kind else {
            return;
        };
        let mut caption = flatten_blocks(&frame.container.blocks);
        let tail = frame.container.finish_inlines();
        if has_visible(&tail) {
            if !caption.is_empty() {
                caption.push(Inline::Break);
            }
            caption.extend(tail);
        }
        if rows.is_empty() && !has_visible(&caption) {
            return;
        }
        self.block(Block::Table(Table { caption, rows }));
    }

    fn close_top(&mut self) {
        match self.top_kind() {
            Some(Kind::Quote) => self.close_quote(),
            Some(Kind::List { .. }) => self.close_list(),
            Some(Kind::Item) => self.close_item(),
            Some(Kind::Table { .. }) => self.close_table(),
            Some(Kind::Row { .. }) => self.close_row(),
            Some(Kind::Cell { .. }) => self.close_cell(),
            Some(Kind::Heading { .. }) => self.close_heading(),
            Some(Kind::Pre) => self.close_pre(),
            Some(Kind::Root) | None => {}
        }
    }

    fn start_link(&mut self, tag: &Tag) {
        let raw = tag.attribute("href");
        if raw.is_some_and(|href| href.trim_matches(|character: char| character.is_ascii_whitespace()).len() > MAX_LINK_BYTES) {
            // The destination is too long to keep: the anchor survives as plain label text only.
            self.truncated = true;
        }
        let href = raw.and_then(safe_href);
        if let Some(frame) = self.frames.last_mut() {
            frame.container.flush_link();
            frame.container.link = href.map(|href| PendingLink { href, text: String::new() });
        }
    }

    fn image(&mut self, tag: &Tag) {
        let raw_alt = tag.attribute("alt").unwrap_or_default().trim();
        let alt = truncate_chars(raw_alt, MAX_ALT_BYTES);
        if alt.len() < raw_alt.len() {
            // A clipped alt text loses visible content, even when it is the only content an image has.
            self.truncated = true;
        }
        let src = tag.attribute("src").and_then(cid_source);
        if src.is_none() && alt.is_empty() {
            return;
        }
        let remaining = MAX_TEXT_CHARS.saturating_sub(self.chars);
        let Some(frame) = self.frames.last_mut() else {
            return;
        };
        let (used, clipped) = match src {
            Some(src) => frame.container.push_image(alt, remaining, src),
            // A remote, data or relative image is never painted; its alt text is still readable.
            None => frame.container.alt_text(alt, remaining),
        };
        self.chars = self.chars.saturating_add(used);
        if clipped {
            self.truncated = true;
            self.stopped = true;
        }
    }
}

fn has_visible(inlines: &[Inline]) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Text(run) => !run.text.trim().is_empty(),
        Inline::Link(link) => !link.text.trim().is_empty(),
        Inline::Image(image) => !image.alt.trim().is_empty(),
        Inline::Break => false,
    })
}

fn trim_pre(text: &str) -> String {
    let text = text.strip_prefix('\n').unwrap_or(text);
    text.trim_end_matches(['\n', '\r', ' ', '\t']).to_owned()
}

/// Accept only an absolute `http`, `https` or `mailto` destination and keep it verbatim.
///
/// The value is not normalised on purpose: the P6c gate in the app must see exactly what the
/// sanitizer stored, so a userinfo or backslash trick cannot be resolved differently by the two
/// layers. `cid:` is a valid image scheme, never a link.
fn safe_href(value: &str) -> Option<String> {
    let value = value.trim_matches(|character: char| character.is_ascii_whitespace());
    if value.is_empty() || value.len() > MAX_LINK_BYTES {
        return None;
    }
    if value.chars().any(|character| character.is_control() || character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>' | '`')) {
        return None;
    }
    let (scheme, rest) = value.split_once(':')?;
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => {
            if !rest.starts_with("//") || rest.trim_start_matches('/').is_empty() {
                return None;
            }
        }
        "mailto" => {
            if rest.trim().is_empty() {
                return None;
            }
        }
        _ => return None,
    }
    Some(value.to_owned())
}

/// Accept only a non-empty local `cid:` source. No remote URL can reach [`Image::src`].
fn cid_source(value: &str) -> Option<String> {
    let value = value.trim_matches(|character: char| character.is_ascii_whitespace());
    let (scheme, rest) = value.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("cid") || rest.is_empty() || rest.len() > MAX_LINK_BYTES {
        return None;
    }
    if rest.chars().any(|character| character.is_control() || character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>')) {
        return None;
    }
    Some(format!("cid:{rest}"))
}

fn flatten_blocks(blocks: &[Block]) -> Vec<Inline> {
    let mut out = Vec::new();
    for block in blocks {
        let mut piece = flatten_block(block);
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Inline::Break);
        }
        out.append(&mut piece);
    }
    out
}

/// Flatten one nested block for a table cell or for [`Body::plain_text`]. Recursion is bounded by
/// [`MAX_DEPTH`] because the tree itself is.
fn flatten_block(block: &Block) -> Vec<Inline> {
    match block {
        Block::Paragraph(inlines) | Block::Heading { inlines, .. } => inlines.clone(),
        Block::Quote(blocks) => flatten_blocks(blocks),
        Block::List(list) => {
            let mut out = Vec::new();
            for (index, item) in list.items.iter().enumerate() {
                let prefix = if list.ordered { format!("{}. ", index.saturating_add(1)) } else { "- ".to_owned() };
                if !out.is_empty() {
                    out.push(Inline::Break);
                }
                out.push(Inline::Text(TextRun { text: prefix, style: Style::default() }));
                out.extend(flatten_blocks(item));
            }
            out
        }
        Block::Pre(text) => {
            let mut out = Vec::new();
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    out.push(Inline::Break);
                }
                if !line.is_empty() {
                    out.push(Inline::Text(TextRun { text: line.to_owned(), style: Style { mono: true, ..Style::default() } }));
                }
            }
            out
        }
        Block::Table(table) => {
            let mut out = Vec::new();
            if has_visible(&table.caption) {
                out.extend(table.caption.clone());
                out.push(Inline::Break);
            }
            for row in &table.rows {
                let mut first = true;
                for cell in &row.cells {
                    if !first {
                        out.push(Inline::Text(TextRun { text: " | ".to_owned(), style: Style::default() }));
                    }
                    out.extend(cell.inlines.clone());
                    first = false;
                }
                if !first {
                    out.push(Inline::Break);
                }
            }
            while matches!(out.last(), Some(Inline::Break)) {
                out.pop();
            }
            out
        }
        Block::Rule => Vec::new(),
    }
}

fn inlines_to_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Break => out.push('\n'),
            other => out.push_str(&other.plain_text()),
        }
    }
    out.trim().to_owned()
}
