use std::fmt;

use zeroize::Zeroizing;

/// A password or token that zeroises its buffer on drop and never prints itself.
///
/// `Debug` and `Display` both render `[redacted]`, so a `Secret` can sit inside a struct that is
/// logged without leaking the value. [`Secret::expose`] is the only way to read it, and the two
/// places this crate calls it are the SASL payload and the `LOGIN` command line — both wrapped in
/// `Zeroizing` and never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wrap a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Borrow the plaintext. Callers must not log, format or store the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([redacted])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_are_redacted() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret([redacted])");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn a_struct_deriving_debug_does_not_leak_the_secret() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            user: &'static str,
            password: Secret,
        }
        let text = format!("{:?}", Holder { user: "a@example.com", password: Secret::new("s3cret") });
        assert!(!text.contains("s3cret"), "{text}");
        assert!(text.contains("a@example.com"), "{text}");
    }

    #[test]
    fn empty_secret_is_empty() {
        assert!(Secret::new("").is_empty());
        assert!(!Secret::new("x").is_empty());
    }
}
