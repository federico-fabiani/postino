//! Parse the RFC 2369/8058 unsubscribe headers captured at ingest.
//!
//! The parser only classifies complete `<...>` targets. Anything without an angle bracket pair, a
//! `mailto:` address or an `http(s):` URL with a non-empty authority is dropped: a malformed header
//! must not become a link the person can click by accident.

use crate::model::{UnsubscribeInfo, UnsubscribeMethod, UnsubscribeTarget};

const MAX_TARGETS: usize = 4;
const MAX_TARGET_CHARS: usize = 2048;

#[derive(serde::Deserialize)]
struct Stored {
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    one_click: bool,
}

/// Decode the encrypted meta value written at ingest. Callers turn a damaged row into a readable
/// error instead of pretending the message has no unsubscribe option.
pub(crate) fn decode(value: &str) -> Result<UnsubscribeInfo, serde_json::Error> {
    let stored: Stored = serde_json::from_str(value)?;
    Ok(parse(&stored.targets, stored.one_click))
}

/// Parse the stored `List-Unsubscribe` values and the `One-Click` flag.
pub(crate) fn parse(values: &[String], one_click_header: bool) -> UnsubscribeInfo {
    let mut targets = Vec::new();
    for value in values {
        let mut rest = value.as_str();
        while let Some(open) = rest.find('<') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('>') else { break };
            let target = rest[..close].trim();
            rest = &rest[close + 1..];
            if target.chars().count() > MAX_TARGET_CHARS {
                continue;
            }
            let Some(method) = classify(target) else { continue };
            if targets.iter().any(|known: &UnsubscribeTarget| known.target == target) {
                continue;
            }
            targets.push(UnsubscribeTarget { method, target: target.to_owned() });
            if targets.len() == MAX_TARGETS {
                break;
            }
        }
        if targets.len() == MAX_TARGETS {
            break;
        }
    }
    let one_click = one_click_header && targets.iter().any(|target| target.method == UnsubscribeMethod::Https);
    UnsubscribeInfo { targets, one_click }
}

fn classify(target: &str) -> Option<UnsubscribeMethod> {
    if target.chars().any(unsafe_header_char) {
        return None;
    }
    let lower = target.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("mailto:") {
        let address = rest.split('?').next().unwrap_or_default();
        let mut parts = address.split('@');
        let local = parts.next().unwrap_or_default();
        let domain = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || local.is_empty()
            || domain.is_empty()
            || address.chars().any(|character| matches!(character, ',' | ';' | '"' | '<' | '>'))
            || ["%2c", "%3b", "%0a", "%0d", "%3c", "%3e"].iter().any(|encoded| lower.contains(encoded))
        {
            return None;
        }
        return Some(UnsubscribeMethod::Mailto);
    }
    for (scheme, method) in [("https://", UnsubscribeMethod::Https), ("http://", UnsubscribeMethod::Http)] {
        if let Some(rest) = lower.strip_prefix(scheme) {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            if authority.is_empty() || authority.contains('@') {
                return None;
            }
            return Some(method);
        }
    }
    None
}

/// Control, whitespace and invisible format characters cannot appear in a header-derived target.
/// Format controls would visually reorder an address or URL in the sheet and draft.
fn unsafe_header_char(character: char) -> bool {
    character.is_control()
        || character.is_whitespace()
        || matches!(
            character,
            '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mailto_and_https_targets_and_keeps_both() {
        let values = vec!["<mailto:list@example.com?subject=unsubscribe>, <https://example.com/u?id=42>".to_owned()];
        let info = parse(&values, true);
        assert_eq!(info.targets.len(), 2);
        assert_eq!(info.targets[0].method, UnsubscribeMethod::Mailto);
        assert_eq!(info.targets[1].method, UnsubscribeMethod::Https);
        assert!(info.one_click);
    }

    #[test]
    fn one_click_is_honest_about_http_and_invalid_targets() {
        let http = parse(&["<http://example.com/u>".to_owned()], true);
        assert_eq!(http.targets[0].method, UnsubscribeMethod::Http);
        assert!(!http.one_click, "One-Click requires HTTPS");
        let malformed = parse(&["https://example.com/u".to_owned()], true);
        assert!(malformed.targets.is_empty(), "a target outside angle brackets is ignored");
        let hostile = parse(&["<javascript:alert(1)>".to_owned()], true);
        assert!(hostile.targets.is_empty());
        let userinfo = parse(&["<https://user:pass@example.com/u>".to_owned()], false);
        assert!(userinfo.targets.is_empty(), "credentials in a target are refused");
        let two = parse(&["<mailto:a@example.com,c@evil.example>".to_owned()], false);
        assert!(two.targets.is_empty(), "a mailto target cannot smuggle two recipients");
        let encoded = parse(&["<mailto:a@example.com%2Cb@evil.example>".to_owned()], false);
        assert!(encoded.targets.is_empty(), "encoded separators are refused");
        let bidi = parse(&["<https://example.com/\u{202e}u>".to_owned()], false);
        assert!(bidi.targets.is_empty(), "format controls are refused");
    }

    #[test]
    fn caps_targets_and_deduplicates_exact_urls() {
        let value = (0..10).map(|index| format!("<https://example.com/u/{index}>")).collect::<Vec<_>>().join(", ");
        let info = parse(&[value], false);
        assert_eq!(info.targets.len(), MAX_TARGETS);
        let duplicate = parse(&["<mailto:a@example.com>, <mailto:a@example.com>".to_owned()], false);
        assert_eq!(duplicate.targets.len(), 1);
    }
}
