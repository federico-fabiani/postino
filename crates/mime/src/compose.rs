//! Compose-side email HTML.
//!
//! The editor writes a small Markdown-like dialect: paragraphs, headings, bullet and numbered
//! lists, quotes, fenced code, pipe tables and inline bold/italic/code/links. This module turns it
//! into HTML that survives mail clients: every rule is an inline style, layout uses plain tables
//! with `role="presentation"`, and there is no script, class, stylesheet, remote image or web font.
//! The same pass builds the plain-text alternative so a message never carries two different bodies.

use crate::MimeError;
use crate::util::normalize_crlf;

/// Largest source accepted by [`compile_html_email`].
pub const MAX_COMPOSE_SOURCE_BYTES: usize = 1024 * 1024;
/// Largest link destination accepted inside inline markup.
const MAX_LINK_CHARS: usize = 2048;
/// Largest link label accepted; also bounds the search for `](` so a body full of brackets cannot
/// turn inline formatting into quadratic work.
const MAX_LINK_LABEL_BYTES: usize = 512;

const FONT: &str = "font-family:Inter,Arial,Helvetica,sans-serif";
const TEXT: &str = "color:#173D32";
const MUTED: &str = "color:#4A5B53";
const BORDER: &str = "border:1px solid #D9CDB8";
const PANEL: &str = "background-color:#F5EBD6";

/// One compiled body: the HTML part and the text alternative generated from the same blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedEmail {
    /// Email-safe HTML with inline CSS only.
    pub html: String,
    /// Plain-text alternative with the same visible content.
    pub text: String,
}

/// Compile the compose dialect into an email-safe HTML part plus its text alternative.
///
/// The compiler is deliberately tolerant of incomplete markup while a person types: an unclosed
/// `**` or table row stays literal text. Only size and hostile links are refused or dropped.
pub fn compile_html_email(source: &str) -> Result<ComposedEmail, MimeError> {
    if source.len() > MAX_COMPOSE_SOURCE_BYTES {
        return Err(MimeError::TooLarge { size: source.len(), limit: MAX_COMPOSE_SOURCE_BYTES });
    }
    let normalized = normalize_crlf(source);
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let mut html = String::with_capacity(normalized.len().saturating_add(1024));
    let mut text = String::with_capacity(normalized.len());
    html.push_str(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" width=\"100%\" data-postino-compose=\"v1\" style=\"",
    );
    html.push_str(PANEL);
    html.push_str(";\"><tr><td align=\"center\" style=\"padding:24px 12px;\"><table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" width=\"100%\" style=\"max-width:640px;");
    html.push_str(PANEL);
    html.push_str(";border-radius:8px;\"><tr><td style=\"padding:28px;");
    html.push_str(FONT);
    html.push_str(";font-size:15px;line-height:1.55;");
    html.push_str(TEXT);
    html.push_str(";\">");
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        if let Some(code) = fenced_code(&lines, &mut index) {
            push_pre(&mut html, &code);
            push_text_block(&mut text, &code);
            continue;
        }
        if let Some((level, heading)) = heading(trimmed) {
            push_heading(&mut html, level, heading);
            push_text_block(&mut text, &inline_text(heading));
            index += 1;
            continue;
        }
        if trimmed == "---" || trimmed == "***" {
            html.push_str("<div style=\"border-top:1px solid #D9CDB8;margin:18px 0;\"></div>");
            text.push_str("--------------------------------\n");
            index += 1;
            continue;
        }
        if is_bullet(line) {
            let mut items = Vec::new();
            while index < lines.len() && is_bullet(lines[index]) {
                items.push(strip_bullet(lines[index]).to_owned());
                index += 1;
            }
            push_list(&mut html, false, &items);
            for item in &items {
                text.push_str("• ");
                text.push_str(&inline_text(item));
                text.push('\n');
            }
            text.push('\n');
            continue;
        }
        if numbered_item(line).is_some() {
            let mut items = Vec::new();
            while index < lines.len() {
                let Some(item) = numbered_item(lines[index]) else {
                    break;
                };
                items.push(item.to_owned());
                index += 1;
            }
            push_list(&mut html, true, &items);
            for (number, item) in items.iter().enumerate() {
                text.push_str(&format!("{}. {}\n", number + 1, inline_text(item)));
            }
            text.push('\n');
            continue;
        }
        if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut quotes = vec![quote.to_owned()];
            index += 1;
            while index < lines.len() {
                let Some(next) = lines[index].trim().strip_prefix("> ") else {
                    break;
                };
                quotes.push(next.to_owned());
                index += 1;
            }
            html.push_str("<blockquote style=\"margin:0 0 14px 0;padding:2px 0 2px 12px;border-left:3px solid #E4B96A;");
            html.push_str(MUTED);
            html.push_str(";\">");
            for (position, quote) in quotes.iter().enumerate() {
                if position > 0 {
                    html.push_str("<br>");
                }
                push_inline(&mut html, quote);
            }
            html.push_str("</blockquote>");
            for quote in &quotes {
                text.push_str("> ");
                text.push_str(&inline_text(quote));
                text.push('\n');
            }
            text.push('\n');
            continue;
        }
        if let Some(table) = pipe_table(&lines, index) {
            push_table(&mut html, &table);
            for (position, row) in table.rows.iter().enumerate() {
                text.push_str(&row.iter().map(|cell| inline_text(cell)).collect::<Vec<_>>().join("\t"));
                text.push('\n');
                if position == 0 {
                    text.push_str(&"-".repeat(24));
                    text.push('\n');
                }
            }
            text.push('\n');
            index = table.end;
            continue;
        }
        let mut paragraph = vec![line.trim().to_owned()];
        index += 1;
        while index < lines.len() {
            let next = lines[index];
            if next.trim().is_empty() || starts_block(next) || pipe_table(&lines, index).is_some() {
                break;
            }
            paragraph.push(next.trim().to_owned());
            index += 1;
        }
        html.push_str("<p style=\"margin:0 0 14px 0;white-space:pre-wrap;\">");
        for (position, part) in paragraph.iter().enumerate() {
            if position > 0 {
                html.push_str("<br>");
            }
            push_inline(&mut html, part);
        }
        html.push_str("</p>");
        text.push_str(&paragraph.iter().map(|part| inline_text(part)).collect::<Vec<_>>().join("\n"));
        text.push_str("\n\n");
    }
    html.push_str("</td></tr></table></td></tr></table>");
    let text = text.trim_end().to_owned();
    Ok(ComposedEmail { html, text })
}

fn fenced_code(lines: &[&str], index: &mut usize) -> Option<String> {
    if !lines.get(*index)?.trim_start().starts_with("```") {
        return None;
    }
    *index += 1;
    let mut code = Vec::new();
    while *index < lines.len() {
        if lines[*index].trim_start().starts_with("```") {
            *index += 1;
            break;
        }
        code.push(lines[*index]);
        *index += 1;
    }
    Some(code.join("\n"))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    for level in 1..=3 {
        if let Some(rest) = line.strip_prefix(&"#".repeat(level))
            && let Some(text) = rest.strip_prefix(' ')
            && !text.trim().is_empty()
        {
            return Some((level, text.trim()));
        }
    }
    None
}

fn is_bullet(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("• ")
}

fn strip_bullet(line: &str) -> &str {
    let trimmed = line.trim_start();
    trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("• ")).unwrap_or(trimmed)
}

fn numbered_item(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let dot = trimmed.find(". ")?;
    if dot == 0 || dot > 3 || !trimmed[..dot].bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    trimmed.get(dot + 2..).map(str::trim_start)
}

fn starts_block(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("```")
        || heading(trimmed).is_some()
        || is_bullet(line)
        || numbered_item(line).is_some()
        || trimmed.starts_with("> ")
        || trimmed == "---"
        || trimmed == "***"
}

struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
    end: usize,
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

impl Align {
    fn css(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }
}

fn pipe_table(lines: &[&str], start: usize) -> Option<Table> {
    let header = split_row(lines.get(start)?)?;
    let separator = split_row(lines.get(start + 1)?)?;
    if header.len() < 2 || separator.len() != header.len() {
        return None;
    }
    let mut aligns = Vec::with_capacity(separator.len());
    for cell in &separator {
        let trimmed = cell.trim();
        let left = trimmed.starts_with(':');
        let right = trimmed.ends_with(':');
        let dashes = trimmed.trim_matches(':');
        if dashes.len() < 3 || !dashes.bytes().all(|byte| byte == b'-') {
            return None;
        }
        aligns.push(match (left, right) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    let mut rows = Vec::new();
    let mut end = start + 2;
    while let Some(row) = lines.get(end).and_then(|line| split_row(line)) {
        if rows.len() >= 200 {
            break;
        }
        let mut row = row;
        row.truncate(header.len());
        while row.len() < header.len() {
            row.push(String::new());
        }
        rows.push(row);
        end += 1;
    }
    if rows.is_empty() {
        return None;
    }
    Some(Table { headers: header, aligns, rows, end })
}

fn split_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let body = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let body = body.strip_suffix('|').unwrap_or(body);
    let cells = body.split('|').take(32).map(|cell| cell.trim().to_owned()).collect::<Vec<_>>();
    if cells.len() < 2 { None } else { Some(cells) }
}

fn push_heading(html: &mut String, level: usize, text: &str) {
    let size = match level {
        1 => 22,
        2 => 19,
        _ => 16,
    };
    html.push_str("<h");
    html.push_str(&level.to_string());
    html.push_str(" style=\"margin:0 0 12px 0;");
    html.push_str(FONT);
    html.push_str(";font-size:");
    html.push_str(&size.to_string());
    html.push_str("px;line-height:1.3;font-weight:600;");
    html.push_str(TEXT);
    html.push_str(";\">");
    push_inline(html, text);
    html.push_str("</h");
    html.push_str(&level.to_string());
    html.push('>');
}

fn push_list(html: &mut String, ordered: bool, items: &[String]) {
    let tag = if ordered { "ol" } else { "ul" };
    html.push('<');
    html.push_str(tag);
    html.push_str(" style=\"margin:0 0 14px 0;padding:0 0 0 20px;\">");
    for item in items {
        html.push_str("<li style=\"margin:0 0 6px 0;\">");
        push_inline(html, item);
        html.push_str("</li>");
    }
    html.push_str("</");
    html.push_str(tag);
    html.push('>');
}

fn push_pre(html: &mut String, code: &str) {
    html.push_str("<pre style=\"margin:0 0 14px 0;padding:12px;");
    html.push_str(PANEL);
    html.push_str(";border-radius:6px;overflow:auto;");
    html.push_str(FONT);
    html.push_str(";font-family:'JetBrains Mono',Consolas,monospace;font-size:13px;line-height:1.45;");
    html.push_str(TEXT);
    html.push_str(";\"><code>");
    escape_into(html, code);
    html.push_str("</code></pre>");
}

fn push_table(html: &mut String, table: &Table) {
    html.push_str("<table cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\" border-collapse:collapse;width:100%;margin:0 0 16px 0;");
    html.push_str(FONT);
    html.push_str(";font-size:14px;line-height:1.45;");
    html.push_str(TEXT);
    html.push_str(";\"><thead><tr>");
    for (position, header) in table.headers.iter().enumerate() {
        let align = table.aligns.get(position).copied().unwrap_or(Align::Left).css();
        html.push_str("<th style=\"");
        html.push_str(BORDER);
        html.push_str(";padding:8px 10px;text-align:");
        html.push_str(align);
        html.push(';');
        html.push_str(PANEL);
        html.push_str(";font-weight:600;\">");
        push_inline(html, header);
        html.push_str("</th>");
    }
    html.push_str("</tr></thead><tbody>");
    for row in &table.rows {
        html.push_str("<tr>");
        for (position, cell) in row.iter().enumerate() {
            let align = table.aligns.get(position).copied().unwrap_or(Align::Left).css();
            html.push_str("<td style=\"");
            html.push_str(BORDER);
            html.push_str(";padding:8px 10px;text-align:");
            html.push_str(align);
            html.push_str(";vertical-align:top;\">");
            push_inline(html, cell);
            html.push_str("</td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table>");
}

fn push_text_block(text: &mut String, block: &str) {
    text.push_str(block);
    text.push_str("\n\n");
}

fn push_inline(html: &mut String, source: &str) {
    let mut rest = source;
    let can_link = source.contains("](");
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
            && end > 0
        {
            html.push_str("<strong>");
            escape_into(html, &after[..end]);
            html.push_str("</strong>");
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
            && end > 0
        {
            html.push_str("<em>");
            escape_into(html, &after[..end]);
            html.push_str("</em>");
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
            && end > 0
        {
            html.push_str("<code style=\"padding:1px 4px;");
            html.push_str(PANEL);
            html.push_str(";border-radius:3px;font-family:'JetBrains Mono',Consolas,monospace;font-size:13px;\">");
            escape_into(html, &after[..end]);
            html.push_str("</code>");
            rest = &after[end + 1..];
            continue;
        }
        if can_link
            && let Some(after) = rest.strip_prefix('[')
            && let Some((label, tail)) = link_label(after)
            && let Some((url, consumed)) = link_destination(tail)
            && is_safe_link(url)
        {
            html.push_str("<a href=\"");
            escape_attr_into(html, url);
            html.push_str("\" style=\"color:#173D32;text-decoration:underline;\">");
            escape_into(html, label);
            html.push_str("</a>");
            rest = &tail[consumed..];
            continue;
        }
        let ch = rest.chars().next().unwrap_or(' ');
        escape_char_into(html, ch);
        rest = rest.get(ch.len_utf8()..).unwrap_or("");
    }
}

fn link_label(after: &str) -> Option<(&str, &str)> {
    let window = after.as_bytes().get(..after.len().min(MAX_LINK_LABEL_BYTES))?;
    let close = window.windows(2).position(|pair| pair == b"](")?;
    Some((after.get(..close)?, after.get(close + 2..)?))
}

fn link_destination(source: &str) -> Option<(&str, usize)> {
    let close = source.find(')')?;
    let url = source.get(..close)?.trim();
    if url.len() > MAX_LINK_CHARS || url.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return None;
    }
    Some((url, close + 1))
}

fn is_safe_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:"))
        && !url.chars().any(|ch| ch.is_control() || ch == '"' || ch == '\'' || ch == '<' || ch == '>' || ch == '`')
}

fn inline_text(source: &str) -> String {
    let mut text = String::with_capacity(source.len());
    let mut rest = source;
    let can_link = source.contains("](");
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
            && end > 0
        {
            text.push_str(&after[..end]);
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
            && end > 0
        {
            text.push_str(&after[..end]);
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
            && end > 0
        {
            text.push_str(&after[..end]);
            rest = &after[end + 1..];
            continue;
        }
        if can_link
            && let Some(after) = rest.strip_prefix('[')
            && let Some((label, tail)) = link_label(after)
            && let Some((url, consumed)) = link_destination(tail)
            && is_safe_link(url)
        {
            if label != url {
                text.push_str(label);
                text.push_str(" <");
                text.push_str(url);
                text.push('>');
            } else {
                text.push_str(url);
            }
            rest = &tail[consumed..];
            continue;
        }
        let ch = rest.chars().next().unwrap_or(' ');
        text.push(ch);
        rest = rest.get(ch.len_utf8()..).unwrap_or("");
    }
    text
}

fn escape_into(out: &mut String, source: &str) {
    for ch in source.chars() {
        escape_char_into(out, ch);
    }
}

fn escape_attr_into(out: &mut String, source: &str) {
    for ch in source.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

fn escape_char_into(out: &mut String, ch: char) {
    match ch {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        _ => out.push(ch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_paragraphs_bold_italic_code_and_links_without_remote_content() {
        let source = "Ciao **Ada**,\nusa *Postino* e `cargo test`.\n\n[Il sito](https://example.com/a?b=1)";
        let out = compile_html_email(source).expect("compile");
        assert!(out.html.contains("<strong>Ada</strong>"));
        assert!(out.html.contains("<em>Postino</em>"));
        assert!(out.html.contains("<code"));
        assert!(out.html.contains("href=\"https://example.com/a?b=1\""));
        assert!(!out.html.contains("<img"));
        assert!(!out.html.contains("<script"));
        assert!(out.text.contains("Ciao Ada"), "{}", out.text);
        assert!(out.text.contains("https://example.com/a?b=1"), "{}", out.text);
    }

    #[test]
    fn tables_get_presentation_markup_inline_css_and_alignment() {
        let source = "| Voce | Importo |\n| :--- | ---: |\n| Canone | **120** |\n| Extra | 15 |";
        let out = compile_html_email(source).expect("compile");
        assert!(out.html.contains("<table cellpadding=\"0\" cellspacing=\"0\" border=\"0\""));
        assert!(out.html.contains("text-align:right;"));
        assert!(out.html.contains("<strong>120</strong>"));
        assert_css_is_inline(&out.html);
        assert!(out.text.contains("Canone\t120"), "{}", out.text);
    }

    #[test]
    fn a_table_directly_under_a_paragraph_stays_a_table() {
        let source = "Introduzione\n| Voce | Importo |\n| --- | ---: |\n| Canone | 120 |";
        let out = compile_html_email(source).expect("compile");
        assert!(out.html.contains("Introduzione</p>"), "{}", out.html);
        assert!(out.html.contains("<table"), "the table must not be swallowed by the paragraph: {}", out.html);
        assert!(!out.html.contains("| Voce |"), "{}", out.html);
        assert!(out.text.contains("Canone\t120"), "{}", out.text);
    }

    #[test]
    fn a_body_full_of_brackets_without_links_compiles_in_linear_time() {
        let source = "[".repeat(MAX_COMPOSE_SOURCE_BYTES - 16);
        let out = compile_html_email(&source).expect("compile");
        assert!(out.html.contains("[[["));
        assert!(!out.html.contains("<a "));
    }

    #[test]
    fn hostile_html_and_links_stay_text() {
        let source = "<script>alert(1)</script> [x](javascript:alert(1)) [y](data:text/html,hi)";
        let out = compile_html_email(source).expect("compile");
        assert!(!out.html.contains("<script>"));
        assert!(!out.html.contains("href=\"javascript:"));
        assert!(!out.html.contains("href=\"data:"));
        assert!(out.html.contains("&lt;script&gt;"));
        assert!(out.text.contains("<script>"));
    }

    #[test]
    fn lists_quotes_code_and_headings_have_text_alternatives() {
        let source = "# Titolo\n\n- uno\n- due\n\n1. primo\n2. secondo\n\n> una citazione\n\n```\n<b>codice</b>\n```";
        let out = compile_html_email(source).expect("compile");
        assert!(out.html.contains("<h1"));
        assert!(out.html.contains("<ul"));
        assert!(out.html.contains("<ol"));
        assert!(out.html.contains("<blockquote"));
        assert!(out.html.contains("&lt;b&gt;codice&lt;/b&gt;"));
        assert!(!out.text.contains("#"));
        assert!(out.text.contains("• uno"));
        assert!(out.text.contains("1. primo"));
        assert!(out.text.contains("> una citazione"));
        assert!(out.text.contains("<b>codice</b>"));
    }

    #[test]
    fn oversized_sources_are_refused_before_building_html() {
        let source = "x".repeat(MAX_COMPOSE_SOURCE_BYTES + 1);
        assert!(matches!(compile_html_email(&source), Err(MimeError::TooLarge { .. })));
    }

    fn assert_css_is_inline(html: &str) {
        assert!(!html.contains("<style"));
        assert!(!html.contains(" class="));
        assert!(!html.contains("background-image:"));
    }
}
