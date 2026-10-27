use std::time::Duration;

use crate::limits::Limits;
use crate::secret::Secret;

/// How the connection is protected.
///
/// There is deliberately no plaintext variant: mail credentials do not travel unencrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// TLS from the first byte, usually port 993.
    Implicit,
    /// Plaintext greeting and `CAPABILITY`, then `STARTTLS` and TLS for everything else, usually
    /// port 143. The session refuses to continue if the server does not advertise `STARTTLS`.
    StartTls,
}

/// Default per-command timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything needed to open one session.
///
/// The password is a [`Secret`]: cloning the config clones a zeroising buffer, and the `Debug`
/// representation prints `[redacted]` for both the username and the password, because an address is
/// personal data that has no business in a log.
#[derive(Clone)]
pub struct ImapConfig {
    /// Server host name, also used for TLS name verification.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Implicit TLS or STARTTLS.
    pub tls: TlsMode,
    /// Login name. Quoted and escaped when written, never logged.
    pub username: String,
    /// Password or app password. Zeroised on drop.
    ///
    /// Empty for an OAuth account: [`ImapConfig::oauth`] carries the credential instead and the
    /// session never falls back to this field.
    pub password: Secret,
    /// OAuth 2.0 access token for SASL `XOAUTH2`, memory-only.
    ///
    /// When set, [`crate::Session::login`] sends `AUTHENTICATE XOAUTH2` and refuses the session if
    /// the server does not advertise it. It never downgrades to a password: an OAuth account has
    /// none.
    pub oauth: Option<Secret>,
    /// Timeout for each phase: DNS, TCP connect, TLS handshake, greeting, every command and the
    /// `DONE` half of IDLE.
    pub timeout: Duration,
    /// Protocol bounds. Clamped on connect.
    pub limits: Limits,
    /// Extra trust anchors, added to the operating system's roots.
    ///
    /// Empty in production. Tests pass the DER of a throwaway CA so a local server can be trusted
    /// without touching the OS store. This adds anchors; it never disables verification, and there
    /// is no way to make the client accept an unverified certificate.
    pub extra_roots: Vec<crate::CertificateDer<'static>>,
}

impl ImapConfig {
    /// Build a configuration with the default timeout and limits.
    pub fn new(host: impl Into<String>, port: u16, tls: TlsMode, username: impl Into<String>, password: Secret) -> Self {
        Self {
            host: host.into(),
            port,
            tls,
            username: username.into(),
            password,
            oauth: None,
            timeout: DEFAULT_TIMEOUT,
            limits: Limits::default(),
            extra_roots: Vec::new(),
        }
    }

    /// Authenticate with an OAuth 2.0 access token (SASL `XOAUTH2`) instead of a password.
    ///
    /// The token is copied into a zeroising buffer and never printed. Use this for a Gmail account
    /// authorized through [`crate::oauth`]; the password path is untouched.
    #[must_use]
    pub fn with_oauth_token(mut self, token: Secret) -> Self {
        self.oauth = Some(token);
        self
    }

    /// Override the per-phase timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the protocol bounds; they are clamped on connect.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Trust one more certificate authority.
    #[must_use]
    pub fn with_extra_root(mut self, root: crate::CertificateDer<'static>) -> Self {
        self.extra_roots.push(root);
        self
    }
}

impl std::fmt::Debug for ImapConfig {
    /// Print the transport and the bounds, never the identity: a username is an address, and an
    /// address in a log is personal data.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImapConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("username", &"[redacted]")
            .field("password", &self.password)
            .field("oauth", &self.oauth.is_some())
            .field("timeout", &self.timeout)
            .field("limits", &self.limits)
            .field("extra_roots", &self.extra_roots.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_keeps_the_transport_and_redacts_the_identity() {
        let config = ImapConfig::new("imap.example.com", 993, TlsMode::Implicit, "user@example.com", Secret::new("pw-123"));
        let text = format!("{config:?}");
        assert!(!text.contains("pw-123"), "{text}");
        assert!(!text.contains("user@example.com"), "{text}");
        assert!(text.contains("imap.example.com"), "{text}");
        assert!(text.contains("993"), "{text}");
        assert!(text.contains("Implicit"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
    }
}
