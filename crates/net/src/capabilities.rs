/// What the server says it can do, in upper case and without duplicates.
///
/// Only what the server lists is advertised: there is no hardcoded capability table and no
/// assumption that a provider "must" support something.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    items: Vec<String>,
}

impl Capabilities {
    /// Build from wire atoms, dropping anything that is not a plausible capability token.
    pub(crate) fn from_items(items: impl IntoIterator<Item = String>) -> Self {
        let mut kept: Vec<String> = Vec::new();
        for item in items {
            let upper = item.to_ascii_uppercase();
            if upper.is_empty() || upper.len() > 128 || !upper.bytes().all(is_capability_byte) {
                continue;
            }
            if !kept.contains(&upper) {
                kept.push(upper);
            }
        }
        Self { items: kept }
    }

    /// Whether the server lists `capability`, compared case-insensitively.
    #[must_use]
    pub fn has(&self, capability: &str) -> bool {
        let needle = capability.to_ascii_uppercase();
        self.items.contains(&needle)
    }

    /// Iterate over the capabilities as written by the server, upper-cased.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(String::as_str)
    }

    /// Number of capabilities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the server listed none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// `IMAP4rev1`, required for every session.
    #[must_use]
    pub fn imap4rev1(&self) -> bool {
        self.has("IMAP4REV1")
    }

    /// `STARTTLS`, required by [`crate::TlsMode::StartTls`].
    #[must_use]
    pub fn starttls(&self) -> bool {
        self.has("STARTTLS")
    }

    /// `IDLE`, required by [`crate::Session::idle`].
    #[must_use]
    pub fn idle(&self) -> bool {
        self.has("IDLE")
    }

    /// `AUTH=PLAIN`, preferred over `LOGIN`.
    #[must_use]
    pub fn auth_plain(&self) -> bool {
        self.has("AUTH=PLAIN")
    }

    /// `AUTH=XOAUTH2`, required for an OAuth access token.
    #[must_use]
    pub fn auth_xoauth2(&self) -> bool {
        self.has("AUTH=XOAUTH2")
    }

    /// `LOGINDISABLED`: the server refuses plaintext credentials even now.
    #[must_use]
    pub fn login_disabled(&self) -> bool {
        self.has("LOGINDISABLED")
    }

    /// `SPECIAL-USE` (RFC 6154), advertised by servers that label folders.
    #[must_use]
    pub fn special_use(&self) -> bool {
        self.has("SPECIAL-USE")
    }

    /// Gmail's `X-GM-EXT-1`: `X-GM-LABELS` may be fetched and is meaningful.
    #[must_use]
    pub fn x_gm_ext(&self) -> bool {
        self.has("X-GM-EXT-1")
    }
}

/// Bytes allowed inside a capability token: a printable atom.
fn is_capability_byte(byte: u8) -> bool {
    byte.is_ascii_graphic() && !matches!(byte, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> Capabilities {
        Capabilities::from_items(items.iter().map(|item| (*item).to_string()))
    }

    #[test]
    fn matches_case_insensitively() {
        let caps = caps(&["imap4rev1", "auth=plain", "IDLE"]);
        assert!(caps.imap4rev1());
        assert!(caps.auth_plain());
        assert!(caps.idle());
        assert!(caps.has("idle"));
        assert!(!caps.starttls());
        assert!(!caps.login_disabled());
    }

    #[test]
    fn deduplicates_and_uppercases() {
        let caps = caps(&["IMAP4rev1", "imap4rev1", "IMAP4REV1"]);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps.iter().collect::<Vec<_>>(), vec!["IMAP4REV1"]);
    }

    #[test]
    fn drops_implausible_tokens() {
        let caps = caps(&["", "bad token", "has(paren)", "good-caps"]);
        assert_eq!(caps.iter().collect::<Vec<_>>(), vec!["GOOD-CAPS"]);
    }
}
