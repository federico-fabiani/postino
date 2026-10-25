/// Convert HTML to readable plain text with a bounded, single-pass scan.
///
/// The contract is part of the public API:
///
/// - block tags (`p`, `div`, headings, lists, tables, quotes, `pre`, `hr`, `br`) become newlines;
/// - list items start with `- `;
/// - links become `label <url>`;
/// - images become their `alt` text;
/// - whitespace is collapsed, entities are decoded, and no tag survives.
///
/// The scan is linear in the input, never recurses and never allocates more than the input plus
/// the text it emits. It is safe to call on untrusted HTML, though [`crate::sanitize_html`] output is
/// the intended input.
#[must_use]
pub fn html_to_text(html: &str) -> String {
    let mut sink = Sink::default();
    let mut index = 0usize;
    let mut skip: Option<String> = None;

    while index < html.len() {
        if let Some(tag) = skip.as_deref() {
            let needle = format!("</{tag}");
            if let Some(offset) = find_ci(html.get(index..).unwrap_or_default(), &needle) {
                index = index.saturating_add(offset);
                skip = None;
                continue;
            }
            break;
        }
        let Some(offset) = html.get(index..).and_then(|rest| rest.find('<')) else {
            if let Some(text) = html.get(index..) {
                push_text(&mut sink, text);
            }
            break;
        };
        let start = index.saturating_add(offset);
        if let Some(text) = html.get(index..start) {
            push_text(&mut sink, text);
        }
        if let Some(rest) = html.get(start..)
            && rest.starts_with("<!--")
        {
            match rest.find("-->") {
                Some(end) => {
                    index = start.saturating_add(end).saturating_add(3);
                    continue;
                }
                None => break,
            }
        }
        let Some(end) = tag_end(html, start) else {
            break;
        };
        let Some(tag) = parse_tag(html.get(start..=end).unwrap_or_default()) else {
            index = end.saturating_add(1);
            continue;
        };
        index = end.saturating_add(1);

        if tag.closing {
            if is_block(&tag.name) || tag.name == "li" {
                sink.newline();
            }
            if tag.name == "a"
                && let Some(href) = sink.take_link()
            {
                sink.link_suffix(&href);
            }
            continue;
        }

        match tag.name.as_str() {
            "br" | "hr" => sink.newline(),
            "li" => {
                sink.newline();
                sink.inline("- ");
            }
            "td" | "th" => sink.inline(" "),
            "img" => {
                if let Some(alt) = tag.attribute("alt").filter(|alt| !alt.trim().is_empty()) {
                    push_text(&mut sink, alt);
                }
            }
            "a" => {
                if let Some(href) = tag.attribute("href") {
                    sink.start_link(href.to_owned());
                }
            }
            "script" | "style" => skip = Some(tag.name.clone()),
            name if is_block(name) => sink.newline(),
            _ => {}
        }
    }
    sink.finish()
}

#[derive(Default)]
struct Sink {
    out: String,
    pending_space: bool,
    pending_newline: bool,
    link: Option<String>,
}

impl Sink {
    fn text(&mut self, text: &str) {
        let mut decoded = String::new();
        decode_entities(text, &mut decoded);
        for character in decoded.chars() {
            if character.is_whitespace() {
                self.pending_space = true;
            } else if character.is_control() {
                // A character reference such as `&#0;` can smuggle a control character into text.
                // It has no place in a text alternative.
                continue;
            } else {
                self.flush();
                self.out.push(character);
            }
        }
    }

    fn newline(&mut self) {
        if !self.out.is_empty() {
            self.pending_newline = true;
            self.pending_space = false;
        }
    }

    /// Append a marker that must stay on the current line (`- `, `<url>`).
    fn inline(&mut self, marker: &str) {
        self.flush();
        if marker == "- " {
            // A list marker is a prefix, not a word: drop whatever whitespace precedes it.
            while self.out.ends_with([' ', '\t']) {
                self.out.pop();
            }
        }
        self.out.push_str(marker);
        self.pending_space = marker.ends_with(' ');
    }

    fn start_link(&mut self, href: String) {
        self.link = Some(href);
    }

    fn take_link(&mut self) -> Option<String> {
        self.link.take()
    }

    fn link_suffix(&mut self, href: &str) {
        self.flush();
        if !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
            self.out.push(' ');
        }
        self.out.push('<');
        self.out.push_str(href);
        self.out.push('>');
        self.pending_space = false;
    }

    fn flush(&mut self) {
        if self.out.is_empty() {
            self.pending_space = false;
            self.pending_newline = false;
            return;
        }
        while self.out.ends_with([' ', '\t']) {
            self.out.pop();
        }
        if self.out.is_empty() {
            self.pending_space = false;
            self.pending_newline = false;
            return;
        }
        if self.pending_newline {
            if !self.out.ends_with('\n') {
                self.out.push('\n');
            }
        } else if self.pending_space && !self.out.ends_with('\n') {
            self.out.push(' ');
        }
        self.pending_space = false;
        self.pending_newline = false;
    }

    fn finish(mut self) -> String {
        while self.out.ends_with([' ', '\t', '\n']) {
            self.out.pop();
        }
        self.out
    }
}

fn push_text(sink: &mut Sink, text: &str) {
    sink.text(text);
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "blockquote"
            | "pre"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "ul"
            | "ol"
            | "dl"
            | "dt"
            | "dd"
            | "figure"
            | "figcaption"
            | "caption"
            | "colgroup"
    )
}

fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, character) in html.get(start..)?.char_indices() {
        if offset == 0 {
            continue;
        }
        match character {
            '"' | '\'' if quote.is_none() => quote = Some(character),
            '"' | '\'' if quote == Some(character) => quote = None,
            '>' if quote.is_none() => return Some(start.saturating_add(offset)),
            _ => {}
        }
    }
    None
}

struct Tag {
    name: String,
    closing: bool,
    attributes: Vec<(String, String)>,
}

impl Tag {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str())
    }
}

fn parse_tag(raw: &str) -> Option<Tag> {
    let inner = raw.strip_prefix('<')?.strip_suffix('>')?.trim();
    if inner.starts_with('!') || inner.starts_with('?') {
        return None;
    }
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, inner),
    };
    let name_end = inner.find(|character: char| character.is_whitespace() || character == '/').unwrap_or(inner.len());
    let name = inner.get(..name_end)?.to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let mut attributes = Vec::new();
    if !closing {
        let mut rest = inner.get(name_end..).unwrap_or_default();
        loop {
            rest = rest.trim_start();
            if rest.is_empty() || rest.starts_with('/') {
                break;
            }
            let field_end = rest.find(|character: char| character.is_whitespace() || character == '=').unwrap_or(rest.len());
            let field = rest.get(..field_end).unwrap_or_default().to_ascii_lowercase();
            if field.is_empty() {
                break;
            }
            rest = rest.get(field_end..).unwrap_or_default();
            rest = rest.trim_start();
            let mut value = String::new();
            if let Some(after_equals) = rest.strip_prefix('=') {
                let after_equals = after_equals.trim_start();
                if let Some(quote) = after_equals.chars().next().filter(|character| *character == '"' || *character == '\'') {
                    let body = after_equals.get(1..).unwrap_or_default();
                    match body.find(quote) {
                        Some(end) => {
                            decode_entities(body.get(..end).unwrap_or_default(), &mut value);
                            rest = body.get(end + 1..).unwrap_or_default();
                        }
                        None => {
                            decode_entities(body, &mut value);
                            rest = "";
                        }
                    }
                } else {
                    let end = after_equals.find(|character: char| character.is_whitespace() || character == '/').unwrap_or(after_equals.len());
                    decode_entities(after_equals.get(..end).unwrap_or_default(), &mut value);
                    rest = after_equals.get(end..).unwrap_or_default();
                }
            }
            if !field.is_empty() {
                attributes.push((field, value));
            }
        }
    }
    Some(Tag { name, closing, attributes })
}

fn decode_entities(input: &str, out: &mut String) {
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        if let Some(head) = rest.get(..amp) {
            out.push_str(head);
        }
        let tail = rest.get(amp..).unwrap_or_default();
        let decoded = tail.find(';').filter(|semicolon| *semicolon <= 12).and_then(|semicolon| tail.get(1..semicolon).and_then(decode_entity));
        match decoded {
            Some(character) => {
                out.push(character);
                rest = tail.get(tail.find(';').unwrap_or(tail.len()).saturating_add(1)..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        _ => {
            if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else {
                entity.strip_prefix('#').and_then(|decimal| decimal.parse::<u32>().ok()).and_then(char::from_u32)
            }
        }
    }
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_become_newlines() {
        let text = html_to_text("<div>uno</div><div>due</div><p>tre</p>");
        assert_eq!(text, "uno\ndue\ntre");
    }

    #[test]
    fn list_items_get_a_dash_prefix() {
        let text = html_to_text("<ul><li>Primo</li><li>Secondo</li></ul>");
        assert_eq!(text, "- Primo\n- Secondo");
    }

    #[test]
    fn links_show_label_and_destination() {
        let text = html_to_text("<p>Vedi <a href=\"https://example.com/x?a=1&amp;b=2\">il sito</a>.</p>");
        assert_eq!(text, "Vedi il sito <https://example.com/x?a=1&b=2>.");
        let text = html_to_text("<a href=\"https://example.com\">https://example.com</a>");
        assert!(text.contains("https://example.com <https://example.com>"));
    }

    #[test]
    fn images_become_alt_text() {
        let text = html_to_text("<p>Logo: <img src=\"cid:x\" alt=\"Postino\"> fine</p>");
        assert_eq!(text, "Logo: Postino fine");
        assert_eq!(html_to_text("<img src=\"cid:x\" alt=\"\">"), "");
    }

    #[test]
    fn whitespace_is_collapsed_and_entities_decoded() {
        let text = html_to_text("<p>  ciao   \n\t mondo &amp; <b>amici</b>&nbsp;!</p>");
        assert_eq!(text, "ciao mondo & amici !");
        assert_eq!(html_to_text("&#67;itt&#224;"), "Città");
    }

    #[test]
    fn no_tag_survives() {
        let text = html_to_text("<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>");
        assert!(!text.contains('<') && !text.contains('>'));
        assert_eq!(text, "a b\nc d");
    }

    #[test]
    fn tables_keep_cells_readable() {
        let text = html_to_text("<table><thead><tr><th>Nome</th><th>Valore</th></tr></thead><tbody><tr><td>Alfa</td><td>1</td></tr></tbody></table>");
        assert_eq!(text, "Nome Valore\nAlfa 1");
    }

    #[test]
    fn script_and_style_content_is_not_text() {
        let text = html_to_text("<style>p{color:red}</style><script>alert(1)</script><p>visibile</p>");
        assert_eq!(text, "visibile");
    }

    #[test]
    fn unterminated_tags_do_not_hang_or_panic() {
        for html in ["<p", "<p>", "<a href=\"x", "<!--", "<img alt=\"x", "&", "&#", "&#x;", "<"] {
            let text = html_to_text(html);
            assert!(!text.contains('<'));
        }
        assert_eq!(html_to_text("<p>ok"), "ok");
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(html_to_text("<p>a<!-- nascosto -->b</p>"), "ab");
    }

    #[test]
    fn attribute_values_are_decoded() {
        let text = html_to_text("<p><img alt=\"A &amp; B\" src=\"cid:x\"></p>");
        assert_eq!(text, "A & B");
    }
}
