//! Text-only reading helpers: safe links, attachment names and the remote-content count.
//!
//! These functions are deliberately pure and dependency-free so the security rules can be tested
//! without a window: the UI only renders what `find_links` returns, and it never hands an unvetted
//! scheme to the injected browser.

use std::ops::Range;

const ALLOWED_PREFIXES: [&str; 3] = ["http://", "https://", "mailto:"];
const TRAILING_PUNCTUATION: [char; 12] = ['.', ',', ';', ':', '!', '?', ')', ']', '}', '»', '”', '’'];

/// One allowed link found in a message body. `range` indexes the original UTF-8 string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedLink {
    pub range: Range<usize>,
    pub url: String,
    /// The host uses punycode (`xn--`), so the confirmation sheet must warn.
    pub idn: bool,
}

/// Links the reader may present. Only absolute `http`, `https` and `mailto` URLs survive; anything
/// else is ordinary text, never a clickable target.
#[must_use]
pub fn find_links(text: &str) -> Vec<ExtractedLink> {
    let lower = text.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut search = 0;
    while search < text.len() {
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
    let mut end = start;
    for (offset, character) in tail.char_indices() {
        if character.is_whitespace() || matches!(character, '<' | '>' | '"' | '\'' | '`' | '\u{00A0}') {
            break;
        }
        end = start.saturating_add(offset).saturating_add(character.len_utf8());
    }
    while end > start {
        let Some(last) = text.get(..end).and_then(|prefix| prefix.chars().next_back()) else {
            break;
        };
        if !TRAILING_PUNCTUATION.contains(&last) {
            break;
        }
        end = end.saturating_sub(last.len_utf8());
    }
    (end > start).then_some(end)
}

fn classify_link(raw: &str, range: Range<usize>) -> Option<ExtractedLink> {
    let lower = raw.to_ascii_lowercase();
    let (scheme, rest) = lower.split_once(':')?;
    if !matches!(scheme, "http" | "https" | "mailto") {
        return None;
    }
    if raw.chars().any(char::is_control) {
        return None;
    }
    let host = match scheme {
        "http" | "https" => {
            let rest = rest.strip_prefix("//")?;
            let authority = rest.split(['/', '?', '#']).next()?;
            let host = authority.rsplit('@').next()?;
            host.split(':').next()?
        }
        "mailto" => {
            let address = rest.split('?').next()?;
            let (local, domain) = address.rsplit_once('@')?;
            if local.is_empty() {
                return None;
            }
            domain
        }
        _ => return None,
    };
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') {
        return None;
    }
    let idn = host.split('.').any(|label| label.to_ascii_lowercase().starts_with("xn--"));
    Some(ExtractedLink { range, url: raw.to_owned(), idn })
}

/// Whether a URL is one of the three schemes the confirmation sheet and browser may receive. This
/// is a second gate at the action boundary, not a replacement for [`find_links`].
#[must_use]
pub fn is_allowed_url(url: &str) -> bool {
    classify_link(url, 0..url.len()).is_some()
}

/// Count remote images that survived into the stored HTML. The MIME sanitizer removes remote images
/// and counts them internally, but the engine deliberately exposes only the sanitized HTML, so the
/// UI can only count references still present (fixtures and future store shapes). Zero means no
/// remote-reference line is shown.
#[must_use]
pub fn remote_content_count(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();
    let mut count = 0usize;
    let mut from = 0;
    while let Some(position) = lower.get(from..).and_then(|tail| tail.find("<img")) {
        let start = from.saturating_add(position);
        let Some(tail) = lower.get(start..) else {
            break;
        };
        let Some(end) = tail.find('>') else {
            break;
        };
        let tag = tail.get(..end).unwrap_or_default();
        let remote =
            ["src=\"http://", "src='http://", "src=\"https://", "src='https://", "src=\"//", "src='//"].iter().any(|needle| tag.contains(needle));
        if remote {
            count = count.saturating_add(1);
        }
        from = start.saturating_add(end).saturating_add(1);
    }
    count
}

/// A filename that is safe as a Save suggestion: no directory component, control character or
/// Windows-reserved punctuation, bounded length, never an empty or dot-only name.
#[must_use]
pub fn safe_filename(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut cleaned = String::new();
    for character in last.chars() {
        if character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(character);
        }
        if cleaned.chars().count() >= 120 {
            break;
        }
    }
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." { "allegato".to_owned() } else { cleaned.to_owned() }
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

/// Detect the extension classes that need an explicit warning before Save.
#[must_use]
pub fn dangerous_kind(filename: &str) -> Option<DangerousKind> {
    let extension = filename.rsplit_once('.')?.1.to_ascii_lowercase();
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
    fn punycode_hosts_are_flagged() {
        let link = find_links("https://xn--bcher-kva.example/path").pop().expect("a link");
        assert!(link.idn);
        assert!(!find_links("https://example.com").pop().expect("a link").idn);
        assert!(find_links("mailto:info@xn--bcher-kva.example").pop().expect("a link").idn);
    }

    #[test]
    fn the_action_boundary_rechecks_the_scheme() {
        for url in ["https://example.com", "http://example.com/a", "mailto:a@example.com"] {
            assert!(is_allowed_url(url), "{url}");
        }
        for url in ["javascript:alert(1)", "file:///C:/secret", "data:text/html,x", "vbscript:x", "../x", "https://"] {
            assert!(!is_allowed_url(url), "{url}");
        }
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
    fn dangerous_extensions_are_explicit_and_case_insensitive() {
        for name in ["setup.exe", "SETUP.MSI", "run.bat", "x.cmd", "x.ps1", "x.js", "x.vbs", "x.scr", "x.jar"] {
            assert_eq!(dangerous_kind(name), Some(DangerousKind::Executable), "{name}");
        }
        for name in ["report.docm", "sheet.XLSM", "slides.pptm"] {
            assert_eq!(dangerous_kind(name), Some(DangerousKind::Macro), "{name}");
        }
        assert_eq!(dangerous_kind("report.pdf"), None);
        assert_eq!(dangerous_kind("no-extension"), None);
    }

    #[test]
    fn the_remote_count_only_counts_remote_images() {
        assert_eq!(remote_content_count("<p>ciao</p>"), 0);
        assert_eq!(remote_content_count("<img src=\"cid:logo\">"), 0);
        assert_eq!(remote_content_count("<img src=\"https://tracker.example/p.gif\"><img src='http://x.example/i.png'>"), 2);
        assert_eq!(remote_content_count("<a href=\"https://example.com\">x</a>"), 0);
    }
}
