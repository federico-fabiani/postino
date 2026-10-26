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
pub const MAX_LINKS_PER_PARAGRAPH: usize = 16;
/// The whole message is bounded too: a body of many tiny paragraphs must not create unlimited
/// widgets or confirmation sheets.
pub const MAX_LINKS_PER_MESSAGE: usize = 64;
/// A link scanner stops at this many bytes. The sheet and tooltip never receive a multi-megabyte
/// string, and a URL longer than this is not presented as clickable text.
pub const MAX_LINK_BYTES: usize = 4096;
/// A displayed inline link and its hover tooltip are capped; the confirmation sheet shows the full
/// URL in a scrollable body, so truncating the inline label never changes what the opener receives.
pub const MAX_LINK_LABEL_CHARS: usize = 120;
pub const MAX_LINK_TOOLTIP_CHARS: usize = 512;
/// A suggested filename is bounded, but its final extension is preserved so a 130-character stem
/// cannot hide `.exe` from the chosen-path warning.
pub const MAX_FILENAME_CHARS: usize = 120;

/// One allowed link found in a message body. `range` indexes the original UTF-8 string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedLink {
    pub range: Range<usize>,
    pub url: String,
    /// The host the browser will actually use for `http`/`https`: lowercased, without userinfo, port
    /// or brackets. For a `mailto:` URL this is the recipient domain.
    pub host: String,
    /// The full `mailto:` recipient (before the query), for an explicit `Destinatario:` line.
    pub recipient: Option<String>,
    /// The link is a `mailto:` URL: the sheet must say “client di posta”, not “browser”.
    pub mailto: bool,
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
/// Userinfo (`user@host`) is rejected. A `mailto:` may carry only `subject` and `body` query fields:
/// `to`, `cc`, `bcc`, `attach`/`attachment`, comma/semicolon recipient lists and every unknown field
/// are rejected, so the visible sheet cannot hide a second destination or a local file.
#[must_use]
pub fn find_links(text: &str) -> Vec<ExtractedLink> {
    find_links_limited(text, MAX_LINKS_PER_PARAGRAPH)
}

/// [`find_links`] with a caller-supplied cap, used by the reading pane to keep a per-message budget.
#[must_use]
pub fn find_links_limited(text: &str, limit: usize) -> Vec<ExtractedLink> {
    let mut links = Vec::new();
    let mut search = 0;
    while search < text.len() {
        if links.len() >= limit {
            break;
        }
        let Some(start) = earliest_prefix(text, search) else {
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
        if end.saturating_sub(start) > MAX_LINK_BYTES {
            search = end;
            continue;
        }
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

/// Find the earliest allowed scheme without copying the paragraph to lowercase on every frame.
fn earliest_prefix(text: &str, from: usize) -> Option<usize> {
    let tail = text.get(from..)?;
    tail.char_indices().find_map(|(offset, _)| {
        let position = from.saturating_add(offset);
        ALLOWED_PREFIXES
            .iter()
            .any(|prefix| text.get(position..position.saturating_add(prefix.len())).is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix)))
            .then_some(position)
    })
}

fn starts_at_a_word_boundary(text: &str, start: usize) -> bool {
    let Some(before) = text.get(..start).and_then(|prefix| prefix.chars().next_back()) else {
        return true;
    };
    !(before.is_alphanumeric() || matches!(before, '_' | '-' | '@' | '/' | '.'))
}

fn link_end(text: &str, start: usize) -> Option<usize> {
    let tail = text.get(start..)?;
    let mut end = start;
    for (offset, character) in tail.char_indices() {
        if character.is_whitespace() || matches!(character, '<' | '>' | '"' | '\'' | '`' | '\u{00A0}') {
            break;
        }
        // Two adjacent URLs in prose (`…/foo,https://…`) must not become one merged target.
        if matches!(character, ',' | ';') {
            let after = offset.saturating_add(character.len_utf8());
            if tail.get(after..).is_some_and(starts_with_url_scheme) {
                break;
            }
        }
        end = start.saturating_add(offset).saturating_add(character.len_utf8());
        if end.saturating_sub(start) > MAX_LINK_BYTES {
            break;
        }
    }
    let end = trim_trailing_punctuation(text, start, end);
    (end > start).then_some(end)
}

fn starts_with_url_scheme(tail: &str) -> bool {
    ALLOWED_PREFIXES.iter().any(|prefix| tail.get(..prefix.len()).is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix)))
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
    if raw.len() > MAX_LINK_BYTES {
        return None;
    }
    if raw.chars().any(|character| character.is_control() || is_format_control(character) || character.is_whitespace()) {
        return None;
    }
    if raw.chars().any(|character| matches!(character, '"' | '\'' | '`' | '<' | '>')) {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    let (scheme, rest) = lower.split_once(':')?;
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
            let host = parse_authority(authority)?;
            if !is_valid_host(host) {
                return None;
            }
            let non_ascii = !host.is_ascii();
            let idn = non_ascii || host.split('.').any(|label| label.to_ascii_lowercase().starts_with("xn--"));
            // A backslash only matters for a browser path; a mailto body is not a URL path.
            let backslash = raw.contains('\\');
            Some(ExtractedLink { range, url: raw.to_owned(), host: host.to_owned(), recipient: None, mailto: false, idn, non_ascii, backslash })
        }
        "mailto" => {
            let (address, query) = match rest.split_once('?') {
                Some((address, query)) => (address, Some(query)),
                None => (rest, None),
            };
            // Keep the recipient's original spelling for the sheet; the lowercased form above is
            // only for validation and query keys.
            let raw_rest = raw.get(scheme.len().saturating_add(1)..)?;
            let raw_address = raw_rest.split('?').next()?;
            // A comma or semicolon is an address-list separator in many mail handlers, and the
            // displayed host would only name the last recipient. Reject the whole list, including
            // the percent-encoded forms.
            if address.contains(',') || address.contains(';') {
                return None;
            }
            let decoded = decode_percent(address);
            if decoded.contains(',') || decoded.contains(';') || decoded.matches('@').count() != 1 || address.to_ascii_lowercase().contains("%25") {
                return None;
            }
            let (local, domain) = address.rsplit_once('@')?;
            if local.is_empty() || !is_valid_host(domain) {
                return None;
            }
            // Only `subject` and `body` are display/prefill fields. `to`, `cc`, `bcc`, `attach` and
            // `attachment` can add a recipient or make a mail handler read a local/UNC file, and an
            // unknown field is not something the sheet can honestly describe.
            if let Some(query) = query
                && !query.is_empty()
                && !mailto_query_is_allowed(query)
            {
                return None;
            }
            let non_ascii = !domain.is_ascii();
            let idn = non_ascii || domain.split('.').any(|label| label.to_ascii_lowercase().starts_with("xn--"));
            Some(ExtractedLink {
                range,
                url: raw.to_owned(),
                host: domain.to_owned(),
                recipient: Some(raw_address.to_owned()),
                mailto: true,
                idn,
                non_ascii,
                backslash: false,
            })
        }
        _ => None,
    }
}

/// Parse the authority of an http(s) URL. A bracketed literal must close at the end of the authority
/// (optionally followed by `:port`), and a bare host may carry at most one numeric port.
fn parse_authority(authority: &str) -> Option<&str> {
    if let Some(inside) = authority.strip_prefix('[') {
        let close = inside.find(']')?;
        let host = inside.get(..close)?;
        let after = inside.get(close.saturating_add(1)..)?;
        if !after.is_empty() {
            let port = after.strip_prefix(':')?;
            if !valid_port(port) {
                return None;
            }
        }
        if !is_ipv6_literal(host) {
            return None;
        }
        return Some(host);
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|port| !valid_port(port)) {
        return None;
    }
    Some(host)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok()
}

fn is_ipv6_literal(host: &str) -> bool {
    !host.is_empty()
        && host.contains(':')
        && host.chars().any(|character| character.is_ascii_hexdigit())
        && host.chars().all(|character| character.is_ascii_hexdigit() || matches!(character, ':' | '.'))
}

fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    host.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | ':') || !character.is_ascii())
}

fn mailto_query_is_allowed(query: &str) -> bool {
    query.split('&').all(|field| {
        let key = field.split_once('=').map_or(field, |(key, _)| key);
        matches!(decode_percent(key).to_ascii_lowercase().as_str(), "subject" | "body")
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
///
/// When a long name must be truncated, the final extension is preserved. A chosen path is a local
/// trust boundary: this function sanitises the name, but does not canonicalise a symlink, reparse
/// point or NTFS alternate data stream that the person selected in the native dialog. The reserved
/// list covers the classic ASCII Win32 device names; `COM¹`/`COM²`/`COM³`, `CONIN$`/`CONOUT$` and
/// stream suffixes remain local-path edge cases.
#[must_use]
pub fn safe_filename(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let extension = trailing_extension(last);
    let mut cleaned = sanitize_component(last, MAX_FILENAME_CHARS);
    if cleaned.chars().count() >= MAX_FILENAME_CHARS
        && let Some(extension) = extension
    {
        let stem_limit = MAX_FILENAME_CHARS.saturating_sub(extension.chars().count().saturating_add(1));
        let stem = sanitize_component(last, stem_limit);
        let stem = stem.trim_end_matches(|character: char| character.is_whitespace() || character == '.');
        if !stem.is_empty() {
            cleaned = format!("{stem}.{extension}");
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

fn sanitize_component(raw: &str, limit: usize) -> String {
    let mut cleaned = String::new();
    for character in raw.chars() {
        if character.is_control() || is_format_control(character) {
            continue;
        }
        if matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(character);
        }
        if cleaned.chars().count() >= limit {
            break;
        }
    }
    cleaned
}

/// The final ASCII extension of the raw name, ignoring trailing whitespace/dots and bidi/format
/// controls. Only a short alphanumeric extension is preserved, so this cannot smuggle punctuation
/// or a path into the truncated suggestion.
fn trailing_extension(raw: &str) -> Option<String> {
    let tail = raw.chars().rev().take(64).collect::<Vec<_>>().into_iter().rev().collect::<String>();
    let tail = tail.chars().filter(|character| !character.is_control() && !is_format_control(*character)).collect::<String>();
    let trimmed = tail.trim_end_matches(|character: char| character.is_whitespace() || character == '.');
    let (_, extension) = trimmed.rsplit_once('.')?;
    if (1..=16).contains(&extension.chars().count()) && extension.chars().all(|character| character.is_ascii_alphanumeric()) {
        Some(extension.to_ascii_lowercase())
    } else {
        None
    }
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
            "mailto:help@example.com?attach=C:\\Users\\feder\\secret.pdf",
            "mailto:help@example.com?attachment=%5C%5Cattacker.example%5Cshare%5Csecret.pdf",
            "mailto:help@example.com?%61ttach=%5C%5Cattacker.example%5Cshare%5Csecret.pdf",
            "mailto:help@example.com?unknown=value",
            "mailto:help@example.com?subject=Preventivo&bcc=attacker@evil.example",
        ] {
            assert!(find_links(raw).is_empty(), "{raw}");
            assert!(!is_allowed_url(raw), "{raw}");
        }
        for raw in [
            "mailto:help@example.com?subject=Preventivo",
            "mailto:help@example.com?body=Ciao%20Federico",
            "mailto:help@example.com?subject=Preventivo&body=Ciao",
        ] {
            assert!(find_links(raw).pop().is_some(), "{raw}");
            assert!(is_allowed_url(raw), "{raw}");
        }
    }

    #[test]
    fn mailto_recipient_lists_are_rejected_not_merged() {
        for raw in [
            "mailto:attacker@evil.example,help@example.com?subject=Preventivo",
            "mailto:attacker@evil.example%2Chelp@example.com?subject=Preventivo",
            "mailto:attacker@evil.example;help@example.com",
            "mailto:attacker@evil.example%3Bhelp@example.com",
            "mailto:attacker@evil.example%252Chelp@example.com",
            "mailto:attacker@evil.example,help@example.com",
        ] {
            assert!(find_links(raw).is_empty(), "{raw}");
            assert!(!is_allowed_url(raw), "{raw}");
        }
    }

    #[test]
    fn a_mailto_link_keeps_the_explicit_recipient() {
        let link = find_links("mailto:help@example.com?subject=Preventivo").pop().expect("a mailto link");
        assert!(link.mailto);
        assert_eq!(link.recipient.as_deref(), Some("help@example.com"));
        assert_eq!(link.host, "example.com");

        let cased = find_links("mailto:Help@Example.com?subject=Preventivo").pop().expect("a cased mailto link");
        assert_eq!(cased.recipient.as_deref(), Some("Help@Example.com"), "the recipient line keeps the original spelling");
    }

    #[test]
    fn malformed_authorities_are_not_links() {
        for raw in ["https://[::1]extra/x", "https://[evil.example]/x", "https://example.com:80:90/x", "https://example.com:/x", "https://[:]/x"] {
            assert!(find_links(raw).is_empty(), "{raw}");
            assert!(!is_allowed_url(raw), "{raw}");
        }
        assert!(find_links("https://[::1]:443/x").pop().is_some());
        assert!(find_links("https://example.com:443/x").pop().is_some());
    }

    #[test]
    fn overlong_links_are_never_presented() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_BYTES));
        assert!(find_links(&long).is_empty(), "a link over the bound is plain text");
        assert!(!is_allowed_url(&long));
        let at_bound = format!("https://example.com/{}", "a".repeat(MAX_LINK_BYTES - "https://example.com/".len()));
        assert_eq!(at_bound.len(), MAX_LINK_BYTES);
        assert!(find_links(&at_bound).pop().is_some(), "a link at the documented bound is still allowed");
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
        assert_eq!(find_links_limited(&paragraph, 5).len(), 5);
    }

    #[test]
    fn attachment_names_cannot_escape_the_chosen_folder() {
        assert_eq!(safe_filename("../../secret.txt"), "secret.txt");
        assert_eq!(safe_filename("C:\\temp\\report.pdf"), "report.pdf");
        assert_eq!(safe_filename("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_filename(".."), "allegato");
        assert_eq!(safe_filename("   "), "allegato");
        assert!(safe_filename(&"a".repeat(200)).chars().count() <= MAX_FILENAME_CHARS);
    }

    #[test]
    fn a_truncated_filename_keeps_its_extension() {
        let long_exe = format!("{}.exe", "a".repeat(130));
        let safe = safe_filename(&long_exe);
        assert!(safe.chars().count() <= MAX_FILENAME_CHARS, "{safe}");
        assert!(safe.ends_with(".exe"), "{safe}");
        assert_eq!(dangerous_kind(&safe), Some(DangerousKind::Executable));

        let long_pdf = format!("{}.pdf", "b".repeat(200));
        let safe = safe_filename(&long_pdf);
        assert!(safe.chars().count() <= MAX_FILENAME_CHARS, "{safe}");
        assert!(safe.ends_with(".pdf"), "{safe}");
        assert_eq!(dangerous_kind(&safe), None);
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
        assert_eq!(dangerous_kind("setup.exe.bak"), None, "a suffix after the extension is not the extension");
        assert_eq!(dangerous_kind("setup.exe1"), None);
        assert_eq!(dangerous_kind("setup.exe\u{00A0}"), Some(DangerousKind::Executable), "trailing whitespace must not hide the extension");
    }
}
