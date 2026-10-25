/// Decoded header fields in the order they appeared.
///
/// Names keep the spelling used in the message; lookups are ASCII case-insensitive. Values are
/// decoded text (RFC 2047 encoded words expanded, folding removed), never raw bytes. Duplicate
/// fields are preserved: [`Headers::get`] returns the first, [`Headers::get_all`] every one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    /// Build an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The first value for `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.iter().find(|(field, _)| field.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    /// Every value for `name`, in document order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries.iter().filter(move |(field, _)| field.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    /// Every field as `(name, value)`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(field, value)| (field.as_str(), value.as_str()))
    }

    /// Number of fields, duplicates included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn push(&mut self, name: String, value: String) {
        self.entries.push((name, value));
    }
}

impl<'a> IntoIterator for &'a Headers {
    type Item = (&'a str, &'a str);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (String, String)>, fn(&'a (String, String)) -> (&'a str, &'a str)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Headers {
        let mut headers = Headers::new();
        headers.push("Subject".to_owned(), "Ciao".to_owned());
        headers.push("X-Trace".to_owned(), "one".to_owned());
        headers.push("x-trace".to_owned(), "two".to_owned());
        headers
    }

    #[test]
    fn lookup_is_case_insensitive_and_keeps_duplicates() {
        let headers = sample();
        assert_eq!(headers.get("subject"), Some("Ciao"));
        assert_eq!(headers.get("X-TRACE"), Some("one"));
        assert_eq!(headers.get_all("x-trace").collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!(headers.get("missing"), None);
    }

    #[test]
    fn iteration_preserves_document_order_and_spelling() {
        let headers = sample();
        let pairs = headers.iter().collect::<Vec<_>>();
        assert_eq!(pairs, [("Subject", "Ciao"), ("X-Trace", "one"), ("x-trace", "two")]);
        assert_eq!(headers.len(), 3);
        assert_eq!((&headers).into_iter().count(), 3);
    }
}
