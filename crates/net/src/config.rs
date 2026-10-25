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
/// representation is `[redacted]`.
#[derive(Debug, Clone)]
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
    pub password: Secret,
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
            timeout: DEFAULT_TIMEOUT,
            limits: Limits::default(),
            extra_roots: Vec::new(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_password() {
        let config = ImapConfig::new("imap.example.com", 993, TlsMode::Implicit, "user@example.com", Secret::new("pw-123"));
        let text = format!("{config:?}");
        assert!(!text.contains("pw-123"), "{text}");
        assert!(text.contains("imap.example.com"));
        assert!(text.contains("Implicit"));
    }
}
