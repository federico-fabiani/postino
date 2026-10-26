//! Text-only reading helpers: safe links, attachment names and safe destinations.
//!
//! These functions are deliberately pure and dependency-free so the security rules can be tested
//! without a window: the UI only renders what `find_links` returns, and it never hands an unvetted
//! scheme to the injected browser.

use std::ops::Range;

const ALLOWED_PREFIXES: [&str; 3] = ["http://", "https://", "mailto:"];
const TRAILING_PUNCTUATION: [char; 12] = ['.', ',', ';', ':', '!', '?', ')', ']', '}', '»', '”', '’'];
/// A crafted body can carry thousands of links in one paragraph. The pane keeps the first few
/// clickable and renders the rest as ordinary text.
const MAX_LINKS_PER_PARAGRAPH: usize = 16;
/// A displayed inline link is capped; the full URL stays in the hover tooltip and the confirmation
/// sheet, so truncating the visible label never changes what the opener receives.
pub const MAX_LINK_LABEL_CHARS: usize = 120;

/// One allowed link found in a message body. `range` indexes the original UTF-8 string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedLink {
    pub range: Range<usize>,
    pub url: String,
    /// The host the browser will actually use: lowercased, without userinfo, port or brackets. For
    /// a `mailto:` URL this is the recipient domain.
    pub host: String,
    /// A host with a literal `xn--` label **or** non-ASCII characters, so the sheet must warn.
    pub idn: bool,
    /// The host contains non-ASCII characters: the browser will display it in punycode.
    pub non_ascii: bool,
    /// The raw URL contains a backslash, which a browser reads as a path separator.
    pub backslash: bool,
}

/// Links the reader may present. Only absolute `http`, `https` and `mailto` URLs survive; anything
/// else is ordinary text, never a clickable target.
///
/// Userinfo (`user@host`) and `mailto:` queries that add `to`, `cc` or `bcc` recipients are
/// rejected outright: the visible sheet must never hide a second destination.
#[must_use]
pub fn find_links(text: &str) -> Vec<ExtractedLink> {
    let lower = text.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut search = 0;
    while search < text.len() {
        if links.len() >= MAX_LINKS_PER_PARAGRAPH {
            break;
        }
        let Some(start) = earliest_prefix(&lower, search) else {
            break;
        };
        if !starts_at_a_word_boundary(text, start) {
            search = start.saturating_add(1);
            continue;
        }
        let Some(end) = link_end(text, start) else {
            search = start.saturating_add(1);
            continue;
        };
        let Some(raw) = text.get(start..end) else {
            search = end.max(start.saturating_add(1));
            continue;
        };
        if let Some(link) = classify_link(raw, start..end) {
            links.push(link);
            search = end;
        } else {
            search = start.saturating_add(1);
        }
    }
    links
}

fn earliest_prefix(lower: &str, from: usize) -> Option<usize> {
    let tail = lower.get(from..)?;
    ALLOWED_PREFIXES.iter().filter_map(|prefix| tail.find(prefix).map(|position| from.saturating_add(position))).min()
}

fn starts_at_a_word_boundary(text: &str, start: usize) -> bool {
    let Some(before) = text.get(..start).and_then(|prefix| prefix.chars().next_back()) else {
        return true;
    };
    !(before.is_alphanumeric() || matches!(before, '_' | '-' | '@' | '/' | '.'))
}

fn link_end(text: &str, start: usize) -> Option<usize> {
    let tail = text.get(start..)?;
    let lower = tail.to_ascii_lowercase();
    let mut end = start;
    for (offset, character) in tail.char_indices() {
        if character.is_whitespace() || matches!(character, '<' | '>' | '"' | '\'' | '`' | '\u{00A0}') {
            break;
        }
        // Two adjacent URLs in prose (`…/foo,https://…`) must not become one merged target.
        if matches!(character, ',' | ';') {
            let after = offset.saturating_add(character.len_utf8());
            if lower.get(after..).is_some_and(starts_with_url_scheme) {
                break;
            }
        }
        end = start.saturating_add(offset).saturating_add(character.len_utf8());
    }
    let end = trim_trailing_punctuation(text, start, end);
    (end > start).then_some(end)
}

fn starts_with_url_scheme(tail: &str) -> bool {
    ALLOWED_PREFIXES.iter().any(|prefix| tail.starts_with(prefix))
}

fn trim_trailing_punctuation(text: &str, start: usize, mut end: usize) -> usize {
    while end > start {
        let Some(last) = text.get(..end).and_then(|prefix| prefix.chars().next_back()) else {
            break;
        };
        if !TRAILING_PUNCTUATION.contains(&last) {
            break;
        }
        if matches!(last, ')' | ']' | '}') && !closing_bracket_is_unmatched(text, start, end, last) {
            // A balanced closing bracket belongs to the URL (`/a_(b)`); an unbalanced one is the
            // prose punctuation around it (`(https://example.com)`).
            break;
        }
        end = end.saturating_sub(last.len_utf8());
    }
    end
}

/// `https://example.com/a_(b)` keeps its balanced closing bracket, while the `)` after
/// `(https://example.com)` is prose punctuation and is trimmed.
fn closing_bracket_is_unmatched(text: &str, start: usize, end: usize, close: char) -> bool {
    let open = match close {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        _ => return false,
    };
    let mut depth = 0i32;
    for character in text.get(start..end).unwrap_or_default().chars() {
        if character == open {
            depth = depth.saturating_add(1);
        } else if character == close {
            depth = depth.saturating_sub(1);
        }
    }
    depth < 0
}

fn classify_link(raw: &str, range: Range<usize>) -> Option<ExtractedLink> {
    if raw.chars().any(|character| character.is_control() || is_format_control(character) || character.is_whitespace()) {
        return None;
    }
    if raw.chars().any(|character| matches!(character, '"' | '\'' | '`' | '<' | '>')) {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    let (scheme, rest) = lower.split_once(':')?;
    let backslash = raw.contains('\\');
    match scheme {
        "http" | "https" => {
            let rest = rest.strip_prefix("//")?;
            // WHATWG treats a backslash as a path separator for special schemes, so the authority
            // ends there too. `https://evil.example\@bank.example/` therefore has host
            // `evil.example`, not `bank.example`.
            let authority_end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
            let authority = rest.get(..authority_end)?;
            if authority.is_empty() || authority.contains('@') {
                return None;
            }
            let host = if let Some(inside) = authority.strip_prefix('[') {
                let end = inside.find(']')?;
                inside.get(..end)?
            } else {
                authority.split(':').next()?
            };
            if !is_valid_host(host) {
                return None;
            }
            let non_ascii = !host.is_ascii();
            let idn = non_ascii || host.split('.').any(|label| label.to_ascii_lowercase().starts_with("xn--"));
            Some(ExtractedLink { range, url: raw.to_owned(), host: host.to_owned(), idn, non_ascii, backslash })
        }
        "mailto" => {
            let (address, query) = match rest.split_once('?') {
                Some((address, query)) => (address, Some(query)),
                None => (rest, None),
            };
            let (local, domain) = address.rsplit_once('@')?;
            if local.is_empty() || !is_valid_host(domain) {
                return None;
            }
            // A hidden Cc/Bcc is not a "full destination" a person can read from an opaque query.
            if let Some(query) = query
                && query_has_hidden_recipient(query)
            {
                return None;
            }
            let non_ascii = !domain.is_ascii();
            let idn = non_ascii || domain.split('.').any(|label| label.to_ascii_lowercase().starts_with("xn--"));
            Some(ExtractedLink { range, url: raw.to_owned(), host: domain.to_owned(), idn, non_ascii, backslash })
        }
        _ => None,
    }
}

fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    host.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | ':') || !character.is_ascii())
}

fn query_has_hidden_recipient(query: &str) -> bool {
    query.split('&').any(|field| {
        let key = field.split('=').next().unwrap_or_default();
        matches!(decode_percent(key).to_ascii_lowercase().as_str(), "to" | "cc" | "bcc")
    })
}

/// Small bounded percent decoder for a query field name, so `b%63c` cannot hide a Bcc.
fn decode_percent(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes.get(index) == Some(&b'%')
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).and_then(|byte| (*byte as char).to_digit(16)),
                bytes.get(index + 2).and_then(|byte| (*byte as char).to_digit(16)),
            )
            && let Some(character) = char::from_u32(high * 16 + low)
        {
            decoded.push(character);
            index += 3;
            continue;
        }
        if let Some(character) = value.get(index..).and_then(|tail| tail.chars().next()) {
            decoded.push(character);
            index += character.len_utf8();
        } else {
            break;
        }
    }
    decoded
}

fn is_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// Whether a URL is one of the three schemes the confirmation sheet and browser may receive. This
/// is a second gate at the action boundary, not a replacement for [`find_links`].
#[must_use]
pub fn is_allowed_url(url: &str) -> bool {
    classify_link(url, 0..url.len()).is_some()
}

/// A filename that is safe as a Save suggestion: no directory component, control character,
/// bidi/format control or Windows-reserved punctuation, no trailing dot or space, bounded length,
/// and never an empty name or a Windows device name.
#[must_use]
pub fn safe_filename(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut cleaned = String::new();
    for character in last.chars() {
        if character.is_control() || is_format_control(character) {
            continue;
        }
        if matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(character);
        }
        if cleaned.chars().count() >= 120 {
            break;
        }
    }
    // Unicode whitespace includes NBSP, thin space and narrow no-break space; Windows also drops
    // trailing dots and spaces, which is exactly what hid `setup.exe` from the warning.
    let cleaned = cleaned.trim_end_matches(|character: char| character.is_whitespace() || character == '.');
    let cleaned = cleaned.trim_start_matches(|character: char| character.is_whitespace() || character == '.');
    if cleaned.is_empty() {
        return "allegato".to_owned();
    }
    let stem = cleaned.split('.').next().unwrap_or(cleaned).trim_end();
    if is_reserved_device_name(stem) { format!("_{cleaned}") } else { cleaned.to_owned() }
}

fn is_reserved_device_name(stem: &str) -> bool {
    let stem = stem.trim().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        || stem.strip_prefix("LPT").is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

/// Why a Save action needs a warning before it can continue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DangerousKind {
    Executable,
    Macro,
}

impl DangerousKind {
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Executable => {
                "Il file è eseguibile. Aprire file eseguibili ricevuti via email può danneggiare il computer o installare software indesiderato."
            }
            Self::Macro => "Il documento contiene macro. Le macro possono eseguire codice quando apri il file; aprilo solo se ti fidi del mittente.",
        }
    }
}

/// Detect the extension classes that need an explicit warning before Save. The caller must pass the
/// **sanitized** filename: classifying the raw sender string is the warning bypass this guards.
#[must_use]
pub fn dangerous_kind(filename: &str) -> Option<DangerousKind> {
    let name = filename.trim_end_matches(|character: char| character.is_whitespace() || character == '.');
    let extension = name.rsplit_once('.')?.1.trim().to_ascii_lowercase();
    if extension.chars().any(|character| !character.is_ascii_alphanumeric()) {
        return None;
    }
    match extension.as_str() {
        "exe" | "msi" | "bat" | "cmd" | "ps1" | "js" | "vbs" | "scr" | "jar" => Some(DangerousKind::Executable),
        "docm" | "xlsm" | "pptm" => Some(DangerousKind::Macro),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absolute_http_https_and_mailto_links_are_presented() {
        let text = "Vai su https://example.com/nota, oppure http://example.it. Mail: mailto:info@example.it. \
                    Non: javascript:alert(1), file:///C:/x, data:text/html,x, vbscript:msgbox(1), ../relativo, www.example.com";
        let links = find_links(text);
        let urls = links.iter().map(|link| link.url.as_str()).collect::<Vec<_>>();
        assert_eq!(urls, vec!["https://example.com/nota", "http://example.it", "mailto:info@example.it"]);
    }

    #[test]
    fn a_scheme_inside_another_word_is_not_a_link() {
        assert!(find_links("xhttps://example.com").is_empty());
        assert_eq!(find_links("(https://example.com)")[0].url, "https://example.com");
    }

    #[test]
    fn punycode_and_non_ascii_hosts_are_flagged() {
        let link = find_links("https://xn--bcher-kva.example/path").pop().expect("a link");
        assert!(link.idn);
        assert!(!link.non_ascii);
        assert!(!find_links("https://example.com").pop().expect("a link").idn);
        assert!(find_links("mailto:info@xn--bcher-kva.example").pop().expect("a link").idn);

        let cyrillic = find_links("https://аpple.com/x").pop().expect("a non-ASCII link");
        assert!(cyrillic.idn, "a non-ASCII host must warn even without an xn-- label");
        assert!(cyrillic.non_ascii);
        assert_eq!(cyrillic.host, "аpple.com");
    }

    #[test]
    fn the_confirmation_sheet_sees_the_effective_destination() {
        // WHATWG terminates the authority at the backslash: the browser opens evil.example.
        let link = find_links("https://evil.example\\@bank.example/").pop().expect("a backslash link");
        assert_eq!(link.host, "evil.example");
        assert!(link.backslash);

        // Userinfo before the path is rejected outright, not presented with a misleading host.
        for raw in ["https://bank.example@evil.example/", "https://user:pass@bank.example/"] {
            assert!(find_links(raw).is_empty(), "{raw}");
            assert!(!is_allowed_url(raw), "{raw}");
        }
    }

    #[test]
    fn mailto_queries_cannot_hide_a_second_recipient() {
        for raw in [
            "mailto:help@example.com?bcc=attacker@evil.example",
            "mailto:help@example.com?cc=attacker@evil.example",
            "mailto:help@example.com?to=attacker@evil.example",
            "mailto:help@example.com?b%63c=attacker@evil.example",
        ] {
            assert!(find_links(raw).is_empty(), "{raw}");
            assert!(!is_allowed_url(raw), "{raw}");
        }
        assert!(find_links("mailto:help@example.com?subject=Preventivo").pop().is_some());
    }

    #[test]
    fn the_action_boundary_rechecks_the_scheme() {
        for url in ["https://example.com", "http://example.com/a", "mailto:a@example.com"] {
            assert!(is_allowed_url(url), "{url}");
        }
        for url in [
            "javascript:alert(1)",
            "file:///C:/secret",
            "data:text/html,x",
            "vbscript:x",
            "../x",
            "https://",
            "https://example.com/a b",
            "https://example.com/\"quoted\"",
        ] {
            assert!(!is_allowed_url(url), "{url}");
        }
    }

    #[test]
    fn the_scanner_keeps_balanced_brackets_and_stops_between_two_urls() {
        let balanced = find_links("https://example.com/a_(b)").pop().expect("a balanced link");
        assert_eq!(balanced.url, "https://example.com/a_(b)");
        let prose = find_links("(vedi https://example.com/nota)").pop().expect("a link");
        assert_eq!(prose.url, "https://example.com/nota");
        let two = find_links("https://example.com/foo,https://evil.example/bar");
        assert_eq!(two.len(), 2, "{two:?}");
        assert_eq!(two[0].url, "https://example.com/foo");
        assert_eq!(two[1].url, "https://evil.example/bar");
        let semicolon = find_links("https://example.com/foo;https://evil.example/bar");
        assert_eq!(semicolon.len(), 2, "{semicolon:?}");
    }

    #[test]
    fn a_hostile_paragraph_is_bounded() {
        let paragraph = (0..200).map(|index| format!("https://example.com/{index}")).collect::<Vec<_>>().join(" ");
        assert_eq!(find_links(&paragraph).len(), MAX_LINKS_PER_PARAGRAPH);
    }

    #[test]
    fn attachment_names_cannot_escape_the_chosen_folder() {
        assert_eq!(safe_filename("../../secret.txt"), "secret.txt");
        assert_eq!(safe_filename("C:\\temp\\report.pdf"), "report.pdf");
        assert_eq!(safe_filename("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_filename(".."), "allegato");
        assert_eq!(safe_filename("   "), "allegato");
        assert!(safe_filename(&"a".repeat(200)).chars().count() <= 120);
    }

    #[test]
    fn unicode_whitespace_and_bidi_controls_cannot_hide_a_dangerous_extension() {
        for name in ["setup.exe\u{00A0}", "setup.exe\u{2009}", "setup.exe\u{202F}", "setup.exe.", "setup.exe ", "setup.exe\u{202E}"] {
            let safe = safe_filename(name);
            assert_eq!(safe, "setup.exe", "{name:?}");
            assert_eq!(dangerous_kind(&safe), Some(DangerousKind::Executable), "{name:?}");
        }
        for name in ["CON.pdf", "nul.txt", "COM1.docm", "LPT9"] {
            assert!(safe_filename(name).starts_with('_'), "{name}: {:?}", safe_filename(name));
        }
    }

    #[test]
    fn dangerous_extensions_are_explicit_and_case_insensitive() {
        for name in ["setup.exe", "SETUP.MSI", "run.bat", "x.cmd", "x.ps1", "x.js", "x.vbs", "x.scr", "x.jar"] {
            assert_eq!(dangerous_kind(name), Some(DangerousKind::Executable), "{name}");
        }
        for name in ["report.docm", "sheet.XLSM", "slides.pptm"] {
            assert_eq!(dangerous_kind(name), Some(DangerousKind::Macro), "{name}");
        }
        assert_eq!(dangerous_kind("report.pdf"), None);
        assert_eq!(dangerous_kind("no-extension"), None);
        assert_eq!(dangerous_kind("setup.exe\u{00A0}"), Some(DangerousKind::Executable), "trailing whitespace must not hide the extension");
    }
}
