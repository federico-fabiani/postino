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
    if target.chars().any(|character| character.is_control() || character.is_whitespace()) {
        return None;
    }
    let lower = target.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("mailto:") {
        let address = rest.split('?').next().unwrap_or_default();
        if address.is_empty() || !address.contains('@') || address.starts_with('@') || address.ends_with('@') {
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
