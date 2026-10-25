use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ammonia::{Builder, UrlRelative};

use crate::util::{contains_ci, truncate_chars};

/// The tags the sanitizer keeps. Everything else is unwrapped (or dropped with its content, for
/// the dangerous containers below).
const ALLOWED_TAGS: &[&str] = &[
    "p",
    "br",
    "div",
    "span",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "s",
    "small",
    "sub",
    "sup",
    "blockquote",
    "pre",
    "code",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "a",
    "img",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "hr",
    "figure",
    "figcaption",
    "caption",
    "colgroup",
    "col",
];

/// Tags whose content is removed with the tag, not unwrapped. A `<script>` body must never become
/// visible text, and an `<svg>` payload must never survive as markup.
const CLEAN_CONTENT_TAGS: &[&str] =
    &["script", "style", "iframe", "object", "embed", "svg", "math", "template", "noscript", "video", "audio", "canvas"];

/// Markers that mean "the sanitizer had something to remove", used only for the warning.
const DANGEROUS_MARKERS: &[&[u8]] = &[
    b"<script",
    b"<style",
    b"<iframe",
    b"<object",
    b"<embed",
    b"<form",
    b"<svg",
    b"<math",
    b"<template",
    b"<noscript",
    b"<video",
    b"<audio",
    b"<canvas",
    b"onerror=",
    b"onclick=",
    b"onload=",
    b"onmouseover=",
    b"javascript:",
    b"vbscript:",
    b"style=",
];

/// Maximum HTML nesting accepted by the sanitizer. Deeper input is cut before ammonia sees it:
/// the DOM walk and serializer are recursive, and a 2 MiB document of `<div>` tags must not be able
/// to exhaust the stack.
const MAX_HTML_DEPTH: usize = 64;

/// Result of [`sanitize_html`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sanitized {
    /// The sanitized fragment. It contains only allowlisted tags and attributes.
    pub html: String,
    /// Images with an explicit `http`/`https` source that were removed. Protocol-relative sources
    /// are removed too but not counted, because the sanitizer's relative-URL pass drops them before
    /// this crate sees the attribute.
    pub remote_images_blocked: usize,
    /// Whether the input or the output was cut at `max_bytes`.
    pub truncated: bool,
    /// Whether a dangerous construct in the input had to be removed.
    pub stripped: bool,
    /// Whether the sanitizer failed internally and produced an empty body.
    pub failed: bool,
}

/// Sanitize an HTML fragment with a strict allowlist.
///
/// - allowed tags: the subset listed in the crate README and `ALLOWED_TAGS`.
/// - `a`: `href` only for `http`, `https` and `mailto`; `rel="noopener noreferrer"` is added and
///   `target` is removed.
/// - `img`: `src` only for `cid:`; `alt`, `width` and `height` (digits) survive. Remote images are
///   removed entirely and counted; `data:`, `file:`, `javascript:` and relative URLs never survive.
/// - no `style`, `class`, `id`, `on*` attributes and no scripts, forms, frames, SVG or MathML.
/// - the output never exceeds `max_bytes`; when it would, it is cut between tags and
///   [`Sanitized::truncated`] is set.
#[must_use]
pub fn sanitize_html(input: &str, max_bytes: usize) -> Sanitized {
    let mut truncated = false;
    let limited = truncate_chars(input, max_bytes);
    if limited.len() != input.len() {
        truncated = true;
    }
    let (limited, too_deep) = truncate_deep_html(limited, MAX_HTML_DEPTH);
    truncated |= too_deep;
    let stripped = DANGEROUS_MARKERS.iter().any(|marker| contains_ci(limited.as_bytes(), marker));
    let blocked = Arc::new(AtomicUsize::new(0));

    let cleaned = {
        let counter = Arc::clone(&blocked);
        let builder = builder(counter);
        catch_unwind(AssertUnwindSafe(move || builder.clean(limited).to_string()))
    };
    let mut html = match cleaned {
        Ok(html) => strip_images_without_src(&html),
        Err(_) => {
            return Sanitized { html: String::new(), remote_images_blocked: blocked.load(Ordering::Relaxed), truncated, stripped, failed: true };
        }
    };
    if html.len() > max_bytes {
        let cut = safe_cut(&html, max_bytes);
        html.truncate(cut);
        truncated = true;
    }
    Sanitized { html, remote_images_blocked: blocked.load(Ordering::Relaxed), truncated, stripped, failed: false }
}

fn builder(counter: Arc<AtomicUsize>) -> Builder<'static> {
    let tags: HashSet<&'static str> = ALLOWED_TAGS.iter().copied().collect();
    let clean_content: HashSet<&'static str> = CLEAN_CONTENT_TAGS.iter().copied().collect();
    let mut attributes: HashMap<&'static str, HashSet<&'static str>> = HashMap::new();
    attributes.insert("a", ["href", "title"].into_iter().collect());
    attributes.insert("img", ["src", "alt", "width", "height"].into_iter().collect());

    let mut builder = Builder::empty();
    builder
        .tags(tags)
        .generic_attributes(HashSet::new())
        .tag_attributes(attributes)
        .clean_content_tags(clean_content)
        .url_schemes(["http", "https", "mailto", "cid"].into_iter().collect())
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .attribute_filter(move |element, attribute, value| match (element, attribute) {
            ("a", "href") => allow_link(value).map(Cow::Owned),
            ("a", "title") => Some(Cow::Borrowed(value)),
            // Ammonia adds this itself, after the tag allowlist and before this filter. A value
            // supplied by the message never reaches here: `rel` is not in `tag_attributes`.
            ("a", "rel") => Some(Cow::Borrowed(value)),
            ("img", "src") => {
                if allow_image(value) {
                    Some(Cow::Owned(value.trim().to_owned()))
                } else {
                    if remote_image(value) {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    None
                }
            }
            ("img", "alt") => Some(Cow::Borrowed(value)),
            ("img", "width" | "height") => {
                if !value.is_empty() && value.chars().all(|character| character.is_ascii_digit()) {
                    Some(Cow::Borrowed(value))
                } else {
                    None
                }
            }
            _ => None,
        });
    builder
}

fn allow_link(value: &str) -> Option<String> {
    let value = clean_url(value)?;
    let (scheme, _) = split_scheme(value)?;
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" | "mailto" => Some(value.to_owned()),
        _ => None,
    }
}

fn allow_image(value: &str) -> bool {
    let Some(value) = clean_url(value) else {
        return false;
    };
    let Some((scheme, rest)) = split_scheme(value) else {
        return false;
    };
    scheme.eq_ignore_ascii_case("cid")
        && !rest.trim().is_empty()
        && !rest.chars().any(|character| character.is_whitespace() || character.is_control() || matches!(character, '<' | '>' | '"' | '\''))
}

fn remote_image(value: &str) -> bool {
    let Some(value) = clean_url(value) else {
        return false;
    };
    if value.starts_with("//") {
        return true;
    }
    split_scheme(value).is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
}

/// Trim surrounding ASCII whitespace and refuse embedded control characters.
fn clean_url(value: &str) -> Option<&str> {
    let value = value.trim_matches(|character: char| character.is_ascii_whitespace());
    if value.is_empty() || value.chars().any(|character| character.is_control()) {
        return None;
    }
    Some(value)
}

/// Split `scheme:rest` without trusting the input: the scheme must be a valid RFC 3986 scheme.
fn split_scheme(value: &str) -> Option<(&str, &str)> {
    let colon = value.find(':')?;
    let scheme = value.get(..colon)?;
    let rest = value.get(colon + 1..)?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() || !chars.all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character)) {
        return None;
    }
    Some((scheme, rest))
}

/// Cut the input before the tag that would exceed `max_depth`. The scanner only counts tags; it
/// does not build a tree, so it cannot itself be attacked with nesting.
fn truncate_deep_html(input: &str, max_depth: usize) -> (&str, bool) {
    let mut depth = 0usize;
    let mut index = 0usize;
    while let Some(offset) = input.get(index..).and_then(|rest| rest.find('<')) {
        let start = index.saturating_add(offset);
        let Some(end) = tag_end(input, start) else {
            break;
        };
        let raw = input.get(start..=end).unwrap_or_default();
        let inner = raw.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')).unwrap_or_default().trim();
        if inner.starts_with('!') || inner.starts_with('?') {
            index = end.saturating_add(1);
            continue;
        }
        let closing = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim_start();
        let name_end = body.find(|character: char| character.is_whitespace() || character == '/').unwrap_or(body.len());
        let name = body.get(..name_end).unwrap_or_default().to_ascii_lowercase();
        let self_closing = body.ends_with('/');
        if closing {
            depth = depth.saturating_sub(1);
        } else if !self_closing && !is_void_tag(&name) {
            depth = depth.saturating_add(1);
            if depth > max_depth {
                return (input.get(..start).unwrap_or_default(), true);
            }
        }
        index = end.saturating_add(1);
    }
    (input, false)
}

fn is_void_tag(name: &str) -> bool {
    matches!(name, "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr")
}

fn tag_end(input: &str, start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, character) in input.get(start..)?.char_indices() {
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

/// Remove `<img>` elements whose usable source was stripped, so a blocked tracking pixel does not
/// survive as an empty image element.
fn strip_images_without_src(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut index = 0usize;
    while let Some(offset) = html.get(index..).and_then(|rest| rest.find('<')) {
        let start = index.saturating_add(offset);
        if let Some(text) = html.get(index..start) {
            out.push_str(text);
        }
        let Some(end_offset) = html.get(start..).and_then(|rest| rest.find('>')) else {
            if let Some(rest) = html.get(start..) {
                out.push_str(rest);
            }
            return out;
        };
        let end = start.saturating_add(end_offset);
        let tag = html.get(start..=end).unwrap_or_default();
        let lower = tag.to_ascii_lowercase();
        let is_image =
            lower.starts_with("<img") && lower.as_bytes().get(4).is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'>' || *byte == b'/');
        if !is_image || lower.contains("src=") {
            out.push_str(tag);
        }
        index = end.saturating_add(1);
    }
    if let Some(rest) = html.get(index..) {
        out.push_str(rest);
    }
    out
}

/// Cut HTML at a character boundary that is not inside a tag or a character reference.
fn safe_cut(html: &str, max_bytes: usize) -> usize {
    let limit = max_bytes.min(html.len());
    let mut safe = 0usize;
    let mut tag_start = 0usize;
    let mut in_tag = false;
    let mut quote: Option<char> = None;
    for (index, character) in html.char_indices() {
        if index >= limit {
            break;
        }
        if in_tag {
            match character {
                '"' | '\'' if quote.is_none() => quote = Some(character),
                '"' | '\'' if quote == Some(character) => quote = None,
                '>' if quote.is_none() => {
                    in_tag = false;
                    safe = index.saturating_add(character.len_utf8());
                }
                _ => {}
            }
        } else if character == '<' {
            in_tag = true;
            tag_start = index;
        } else {
            safe = index.saturating_add(character.len_utf8());
        }
    }
    if in_tag {
        safe = tag_start.min(limit);
    }
    // Do not cut in the middle of `&name;` or `&#123;`.
    if let Some(rest) = html.get(safe..limit)
        && let (Some(amp), semicolon) = (rest.rfind('&'), rest.rfind(';'))
        && semicolon.is_none_or(|semicolon| amp > semicolon)
    {
        safe = safe.saturating_add(amp);
    }
    let mut safe = safe.min(limit);
    while safe > 0 && !html.is_char_boundary(safe) {
        safe -= 1;
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(input: &str) -> String {
        sanitize_html(input, 64 * 1024).html
    }

    #[test]
    fn keeps_only_allowlisted_tags_and_drops_scripts_with_content() {
        let html = clean("<script>alert('x')</script><style>p{color:red}</style><p onclick=\"x\">testo</p><custom>fuori</custom>");
        assert!(!html.contains("script"));
        assert!(!html.contains("alert"));
        assert!(!html.contains("color:red"));
        assert!(html.contains("<p>testo</p>"));
        assert!(!html.contains("custom"));
        assert!(html.contains("fuori"));
    }

    #[test]
    fn removes_every_dangerous_container() {
        let html = clean(concat!(
            "<iframe src=\"https://evil.example\"></iframe><object data=\"x\"></object><embed src=\"x\">",
            "<form action=\"/steal\"><input name=\"q\"><button>Invia</button></form>",
            "<svg onload=\"alert(1)\"><text>svg</text></svg><math><mi>x</mi></math><template><p>t</p></template>",
            "<noscript><img src=\"https://evil.example/n.png\"></noscript><video src=\"x\"></video><audio src=\"x\"></audio><canvas></canvas>"
        ));
        for forbidden in
            ["<iframe", "<object", "<embed", "<form", "<input", "<button", "<svg", "<math", "<template", "<noscript", "<video", "<audio", "<canvas"]
        {
            assert!(!html.contains(forbidden), "{forbidden} survived: {html}");
        }
        assert!(!html.contains("alert(1)"));
        assert!(!html.contains("svg"));
        assert!(html.contains("Invia"));
    }

    #[test]
    fn link_schemes_are_an_allowlist() {
        for allowed in ["http://example.com/x", "https://example.com/x?a=1&b=2", "mailto:info@example.com"] {
            let html = clean(&format!("<a href=\"{allowed}\">ok</a>"));
            assert!(html.contains("href="), "{allowed} was dropped: {html}");
            assert!(html.contains("rel=\"noopener noreferrer\""));
            assert!(!html.contains("target="));
        }
        for dead in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java&#115;cript:alert(1)",
            "vbscript:msgbox(1)",
            "VBScript:msgbox(1)",
            "file:///C:/secret.txt",
            "data:text/html;base64,PHNjcmlwdD4=",
            "//evil.example/x",
            "\\\\evil.example\\x",
            "/relative",
            "cid:inline@example.com",
        ] {
            let html = clean(&format!("<a href=\"{dead}\">bad</a>"));
            assert!(!html.contains("href="), "{dead} survived: {html}");
        }
        let html = clean("<a href=\"https://example.com\" target=\"_blank\" rel=\"opener\">ok</a>");
        assert!(!html.contains("target="));
        assert!(!html.contains("opener\"") || html.contains("noopener"));
    }

    #[test]
    fn images_keep_cid_and_lose_remote_sources() {
        let sanitized = sanitize_html(
            "<img src=\"cid:logo@example.com\" alt=\"Logo\"><img src=\"https://tracker.example/p.gif\" alt=\"Pixel\"><img src=\"//tracker.example/q.gif\">",
            64 * 1024,
        );
        assert!(sanitized.html.contains("src=\"cid:logo@example.com\""));
        assert!(!sanitized.html.contains("tracker.example"));
        assert!(!sanitized.html.contains("<img alt=\"Pixel\">"));
        assert_eq!(sanitized.remote_images_blocked, 1);
    }

    #[test]
    fn data_file_and_script_image_sources_are_dead() {
        for dead in [
            "data:image/png;base64,AAAA",
            "Data:image/png;base64,AAAA",
            "javascript:alert(1)",
            "file:///C:/x.png",
            "//evil.example/x.png",
            "/relative.png",
            "cid:",
            "cid: bad",
        ] {
            let sanitized = sanitize_html(&format!("<img src=\"{dead}\" alt=\"x\">"), 64 * 1024);
            assert!(!sanitized.html.contains("<img"), "{dead} survived: {}", sanitized.html);
        }
    }

    #[test]
    fn strips_style_class_id_and_event_attributes() {
        let html = clean("<p style=\"color:red\" class=\"x\" id=\"y\" onmouseover=\"alert(1)\" data-x=\"1\">testo</p>");
        assert_eq!(html.trim(), "<p>testo</p>");
    }

    #[test]
    fn width_and_height_must_be_digits() {
        let html = clean("<img src=\"cid:x\" alt=\"a\" width=\"10\" height=\"20\" onload=\"x\">");
        assert!(html.contains("width=\"10\""));
        assert!(html.contains("height=\"20\""));
        assert!(!html.contains("onload"));
        let html = clean("<img src=\"cid:x\" alt=\"a\" width=\"10px\" height=\"calc(1)\">");
        assert!(!html.contains("width="));
        assert!(!html.contains("height="));
    }

    #[test]
    fn mixed_case_and_entity_encoded_schemes_are_dead() {
        for dead in ["&#106;avascript:alert(1)", "j&#97;vascript:alert(1)", "jav&#x61;script:alert(1)", "JAVASCRIPT:alert(1)"] {
            let html = clean(&format!("<a href=\"{dead}\">x</a>"));
            assert!(!html.to_ascii_lowercase().contains("href="), "{dead} survived: {html}");
        }
    }

    #[test]
    fn output_is_capped_between_tags() {
        let body = "<p>\"</p>".repeat(200);
        let sanitized = sanitize_html(&body, 300);
        assert!(sanitized.html.len() <= 300, "{} bytes", sanitized.html.len());
        assert!(sanitized.truncated);
        assert!(!sanitized.html.ends_with('<'));
        assert!(sanitized.html.matches('<').count() == sanitized.html.matches('>').count());
    }

    #[test]
    fn input_is_capped_before_parsing() {
        let sanitized = sanitize_html(&"x".repeat(10_000), 100);
        assert_eq!(sanitized.html, "x".repeat(100));
        assert!(sanitized.truncated);
    }

    #[test]
    fn deep_html_is_cut_before_the_sanitizer() {
        let deep = format!("{}testo", "<div>".repeat(500));
        let sanitized = sanitize_html(&deep, 64 * 1024);
        assert!(sanitized.truncated);
        assert!(sanitized.html.matches("<div>").count() <= MAX_HTML_DEPTH, "{}", sanitized.html.matches("<div>").count());
    }

    #[test]
    fn safe_cut_never_splits_a_tag() {
        let html = "<p>abc</p><p>def</p>";
        for max in 0..html.len() {
            let cut = safe_cut(html, max);
            let prefix = html.get(..cut).unwrap_or_default();
            assert!(!prefix.ends_with('<'));
            assert_eq!(prefix.matches('<').count(), prefix.matches('>').count(), "cut {max}: {prefix}");
        }
    }
}
