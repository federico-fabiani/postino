//! SMTP submission client.
//!
//! This module is the network half of P7: one TCP connection, implicit TLS or STARTTLS, `EHLO`,
//! `AUTH PLAIN`/`LOGIN`, `MAIL`, `RCPT`, `DATA`, `RSET`, `NOOP` and `QUIT`. It does not know what a
//! draft is, does not write the store and never retries on its own. The outbox above it owns the
//! journal and decides when an attempt is safe.
//!
//! # What is deliberately absent
//!
//! `PIPELINING` is parsed but never used: every command waits for its reply. `DSN` is available only
//! when the server advertises it; the compose path does not request it. There is no plaintext mode,
//! no way to skip certificate verification and no password fallback for an OAuth account: when a
//! session carries an access token it authenticates with `AUTH XOAUTH2` or not at all.
//!
//! # Bounds and secrets
//!
//! A response line is at most [`SmtpLimits::max_line`] (8 KiB), one reply at most
//! [`SmtpLimits::max_response`] (64 KiB), a message at most [`SmtpLimits::max_message`] (40 MiB) and
//! a transaction at most 100 recipients. The password lives in a [`Secret`] that zeroises itself and
//! prints as `[redacted]`. The message body and the recipient list never enter a log line; the only
//! values logged are command verbs and counts.
//!
//! # The one dangerous window
//!
//! Once the final `.` of `DATA` has been written, the server may have accepted the message even if
//! the connection dies before its `250`. Every failure after that point is
//! [`SmtpError::Uncertain`], never [`SmtpError::Transient`]: retrying it automatically could deliver
//! the message twice. The outbox turns that into `uncertain` and asks the user.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, lookup_host};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_rustls::TlsConnector;
use zeroize::Zeroizing;

use crate::config::TlsMode;
use crate::error::sanitize_reason;
use crate::secret::Secret;
use crate::tls;

/// Default per-phase timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The protocol bounds this client applies.
///
/// Every field is clamped to a hard ceiling in [`SmtpLimits::clamped`]; a caller can lower a bound
/// but never raise it past the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmtpLimits {
    /// Largest accepted reply line, in bytes, excluding CRLF.
    pub max_line: usize,
    /// Largest accepted reply, all continuation lines together, in bytes.
    pub max_response: usize,
    /// Largest accepted message after dot-stuffing, in bytes.
    pub max_message: usize,
    /// Most recipients accepted in one transaction.
    pub max_recipients: usize,
}

impl Default for SmtpLimits {
    fn default() -> Self {
        Self { max_line: 8 * 1024, max_response: 64 * 1024, max_message: 40 * 1024 * 1024, max_recipients: 100 }
    }
}

impl SmtpLimits {
    /// Hard ceiling for [`SmtpLimits::max_line`].
    pub const HARD_MAX_LINE: usize = 8 * 1024;
    /// Hard ceiling for [`SmtpLimits::max_response`].
    pub const HARD_MAX_RESPONSE: usize = 64 * 1024;
    /// Hard ceiling for [`SmtpLimits::max_message`].
    pub const HARD_MAX_MESSAGE: usize = 64 * 1024 * 1024;
    /// Hard ceiling for [`SmtpLimits::max_recipients`].
    pub const HARD_MAX_RECIPIENTS: usize = 100;

    /// Clamp every bound to its hard ceiling and to at least one.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            max_line: self.max_line.clamp(1, Self::HARD_MAX_LINE),
            max_response: self.max_response.clamp(1, Self::HARD_MAX_RESPONSE),
            max_message: self.max_message.clamp(1, Self::HARD_MAX_MESSAGE),
            max_recipients: self.max_recipients.clamp(1, Self::HARD_MAX_RECIPIENTS),
        }
    }
}

/// Everything needed to open one SMTP session.
#[derive(Clone)]
pub struct SmtpConfig {
    /// Server host name, also used for TLS name verification.
    pub host: String,
    /// Server port. Typical values are 465 (implicit TLS) and 587 (STARTTLS).
    pub port: u16,
    /// Implicit TLS or STARTTLS. There is no plaintext variant.
    pub tls: TlsMode,
    /// Login name. Normally the account address.
    pub username: String,
    /// Password or app password. Zeroised on drop and never printed.
    ///
    /// Empty for an OAuth account: [`SmtpConfig::oauth`] carries the credential instead.
    pub password: Secret,
    /// OAuth 2.0 access token for SASL `XOAUTH2`, memory-only. Never printed.
    pub oauth: Option<Secret>,
    /// Timeout for DNS, TCP, TLS, the greeting and every command.
    pub timeout: Duration,
    /// Protocol bounds; clamped on connect.
    pub limits: SmtpLimits,
    /// Extra trust anchors for tests. Empty in production; this only adds roots.
    pub extra_roots: Vec<crate::CertificateDer<'static>>,
}

impl SmtpConfig {
    /// Build a configuration with the default timeout and bounds.
    pub fn new(host: impl Into<String>, port: u16, tls: TlsMode, username: impl Into<String>, password: Secret) -> Self {
        Self {
            host: host.into(),
            port,
            tls,
            username: username.into(),
            password,
            oauth: None,
            timeout: DEFAULT_TIMEOUT,
            limits: SmtpLimits::default(),
            extra_roots: Vec::new(),
        }
    }

    /// Authenticate with an OAuth 2.0 access token (SASL `XOAUTH2`) instead of a password.
    ///
    /// Gmail needs this for `smtp.gmail.com` when the account was authorized through
    /// [`crate::oauth`]. The password path is untouched.
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
    pub fn with_limits(mut self, limits: SmtpLimits) -> Self {
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

impl std::fmt::Debug for SmtpConfig {
    /// Print the transport and the bounds, never the identity or the secret.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpConfig")
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

/// The SMTP envelope, independent of the message headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpEnvelope {
    /// Bare sender address.
    pub from: String,
    /// Deduplicated recipient addresses. Blind recipients are included here and never in the body.
    pub recipients: Vec<String>,
}

/// What the server advertised in its final `EHLO` reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SmtpCapabilities {
    /// Advertised `SIZE` ceiling in bytes, when present.
    pub size: Option<u64>,
    /// `8BITMIME`: the server accepts unencoded 8-bit bodies.
    pub eight_bit_mime: bool,
    /// `SMTPUTF8`: internationalised addresses and headers.
    pub smtputf8: bool,
    /// `DSN`: delivery status notification parameters.
    pub dsn: bool,
    /// `AUTH PLAIN`.
    pub auth_plain: bool,
    /// `AUTH LOGIN`.
    pub auth_login: bool,
    /// `AUTH XOAUTH2`.
    pub auth_xoauth2: bool,
    /// `STARTTLS` was advertised before the upgrade.
    pub starttls: bool,
    /// `PIPELINING` was advertised. It is parsed for diagnostics and never used.
    pub pipelining: bool,
}

impl SmtpCapabilities {
    fn parse(lines: &[String]) -> Self {
        let mut capabilities = Self::default();
        for line in lines {
            let upper = line.trim().to_ascii_uppercase();
            let mut fields = upper.split_whitespace();
            let Some(keyword) = fields.next() else {
                continue;
            };
            match keyword {
                "SIZE" => capabilities.size = fields.next().and_then(|value| value.parse::<u64>().ok()),
                "8BITMIME" => capabilities.eight_bit_mime = true,
                "SMTPUTF8" => capabilities.smtputf8 = true,
                "DSN" => capabilities.dsn = true,
                "STARTTLS" => capabilities.starttls = true,
                "PIPELINING" => capabilities.pipelining = true,
                "AUTH" => {
                    for mechanism in fields {
                        match mechanism {
                            "PLAIN" => capabilities.auth_plain = true,
                            "LOGIN" => capabilities.auth_login = true,
                            "XOAUTH2" => capabilities.auth_xoauth2 = true,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        capabilities
    }
}

/// Delivery status options for one transaction. They are refused unless `DSN` is advertised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DsnOptions {
    /// Return the full message in a bounce (`RET=FULL`) instead of only the headers.
    pub return_full: bool,
    /// When to request a delivery notification. Empty leaves the server default.
    pub notify: Vec<DsnNotify>,
    /// Envelope id echoed in delivery reports. ASCII, at most 100 bytes.
    pub envid: Option<String>,
}

/// One DSN notification condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsnNotify {
    /// Notify on successful delivery.
    Success,
    /// Notify on failure.
    Failure,
    /// Notify on a delayed delivery.
    Delay,
}

impl DsnNotify {
    fn as_wire(self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Failure => "FAILURE",
            Self::Delay => "DELAY",
        }
    }
}

/// The successful end of one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// Final SMTP code, normally 250.
    pub code: u16,
    /// Server text, sanitized and bounded.
    pub message: String,
    /// Recipients the server accepted.
    pub accepted: usize,
}

/// Everything this module refuses, with a message a person or an agent can act on.
///
/// A server reply that is not a success becomes [`SmtpError::Rejected`] with its exact code and the
/// stage it answered. [`SmtpError::Transient`] and [`SmtpError::Permanent`] are local conclusions
/// before a decision point; [`SmtpError::Uncertain`] is the one case that must never be retried
/// automatically.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SmtpError {
    /// The host name could not be resolved.
    #[error("could not resolve {host}: {reason}")]
    Dns {
        /// Host as configured.
        host: String,
        /// Resolver message, sanitized.
        reason: String,
    },
    /// The TCP connection could not be established.
    #[error("could not connect to {host}:{port}: {reason}")]
    Connect {
        /// Host as configured.
        host: String,
        /// Port as configured.
        port: u16,
        /// Operating-system message, sanitized.
        reason: String,
    },
    /// The TLS handshake or certificate verification failed.
    #[error("TLS with {host} failed: {reason}")]
    Tls {
        /// Host that was verified.
        host: String,
        /// Verification failure, sanitized. Never contains the certificate.
        reason: String,
    },
    /// The server rejected the credentials, or offered no usable mechanism.
    ///
    /// `reason` is a fixed sentence built locally: a hostile or compromised server can echo the
    /// submitted password in a `535`, so its reply text is never part of this error.
    #[error("authentication failed: {reason}")]
    Auth {
        /// What went wrong, without server text.
        reason: String,
    },
    /// The server answered a command with a non-success code.
    ///
    /// No server text is kept, displayed or formatted: a hostile or compromised server has just
    /// received the password (and, at DATA, the whole body) and can echo either in a refusal. The
    /// stage and the code are all a caller needs; the human sentence is built by the engine.
    #[error("{stage} was refused with {code}")]
    Rejected {
        /// Three-digit SMTP code.
        code: u16,
        /// Stage that was answered: `ehlo`, `mail`, `rcpt`, `data`, ...
        stage: &'static str,
    },
    /// A local or transport failure that is safe to retry.
    #[error("{stage} failed transiently: {reason}")]
    Transient {
        /// Stage that failed.
        stage: &'static str,
        /// Sanitized reason.
        reason: String,
    },
    /// A local or transport failure that will not improve by retrying.
    #[error("{stage} failed permanently: {reason}")]
    Permanent {
        /// Stage that failed.
        stage: &'static str,
        /// Sanitized reason.
        reason: String,
    },
    /// The final `.` was written but no success reply was read. The message may have been
    /// delivered: never retry automatically.
    #[error("the outcome of the submission is unknown after {reason}; check the Sent folder before retrying")]
    Uncertain {
        /// Sanitized reason.
        reason: String,
    },
    /// A phase exceeded its timeout.
    #[error("{phase} timed out after {after:?}")]
    Timeout {
        /// Phase that timed out.
        phase: &'static str,
        /// Configured timeout.
        after: Duration,
    },
    /// The connection ended during a phase.
    #[error("the connection closed during {phase}")]
    Closed {
        /// Phase that lost the connection.
        phase: &'static str,
    },
    /// A bound was reached.
    #[error("{what} exceeds the limit of {limit}")]
    TooLarge {
        /// Bound that was hit.
        what: &'static str,
        /// Configured ceiling.
        limit: usize,
    },
    /// The server does not offer a capability this request requires.
    #[error("{feature} is not supported by this server")]
    Unsupported {
        /// Missing capability.
        feature: &'static str,
    },
    /// The exchange cannot proceed: malformed reply, invalid input or a refused request.
    #[error("the exchange cannot proceed: {reason}")]
    Protocol {
        /// Sanitized reason.
        reason: String,
    },
}

impl SmtpError {
    /// Whether the caller may retry the whole transaction without risking a second delivery.
    ///
    /// `false` for [`SmtpError::Uncertain`], an authentication failure, a permanent server code,
    /// an oversized message, a missing capability and a local permanent error.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Dns { .. } | Self::Connect { .. } | Self::Timeout { .. } | Self::Closed { .. } | Self::Transient { .. } => true,
            Self::Rejected { code, .. } => (400..500).contains(code),
            Self::Tls { .. }
            | Self::Auth { .. }
            | Self::Permanent { .. }
            | Self::Uncertain { .. }
            | Self::TooLarge { .. }
            | Self::Unsupported { .. } => false,
            Self::Protocol { .. } => false,
        }
    }

    /// Whether the failure means the message may already have reached the server.
    #[must_use]
    pub fn is_uncertain(&self) -> bool {
        matches!(self, Self::Uncertain { .. })
    }

    /// Whether the user has to fix a credential.
    #[must_use]
    pub fn is_authentication(&self) -> bool {
        matches!(self, Self::Auth { .. })
    }
}

/// One authenticated SMTP submission session.
pub struct SmtpClient {
    conn: Connection,
    config: SmtpConfig,
    capabilities: SmtpCapabilities,
    authenticated: bool,
    closed: bool,
}

impl std::fmt::Debug for SmtpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpClient")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("capabilities", &self.capabilities)
            .field("authenticated", &self.authenticated)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl SmtpClient {
    /// Open one connection, read the greeting, `EHLO`, and upgrade with `STARTTLS` when configured.
    ///
    /// No credential crosses a plaintext socket: with [`TlsMode::StartTls`] only the greeting,
    /// `EHLO` and `STARTTLS` happen before the handshake, and the capability list from before the
    /// upgrade is discarded. Authentication is a separate call so a caller can probe capabilities
    /// without sending a password.
    pub async fn connect(config: SmtpConfig) -> Result<Self, SmtpError> {
        if config.host.trim().is_empty() {
            return Err(SmtpError::Connect { host: config.host, port: config.port, reason: "host is empty".to_owned() });
        }
        if config.timeout.is_zero() {
            return Err(SmtpError::Timeout { phase: "connect", after: config.timeout });
        }
        let mut config = config;
        config.limits = config.limits.clamped();
        let timeout = config.timeout;
        let addresses = resolve(&config.host, config.port, timeout).await?;
        let tcp = connect_any(&config.host, config.port, &addresses, timeout).await?;
        if let Err(error) = tcp.set_nodelay(true) {
            log::debug!("smtp: could not disable Nagle: {error}");
        }
        let conn = Connection::new(tcp, config.limits, timeout);
        let mut client = Self { conn, config, capabilities: SmtpCapabilities::default(), authenticated: false, closed: false };
        if client.config.tls == TlsMode::Implicit {
            client.handshake().await?;
        }
        client.greet().await?;
        if client.config.tls == TlsMode::StartTls {
            client.starttls().await?;
        } else {
            client.ehlo().await?;
        }
        Ok(client)
    }

    /// What the server advertised in its final `EHLO`.
    #[must_use]
    pub fn capabilities(&self) -> &SmtpCapabilities {
        &self.capabilities
    }

    /// Whether [`SmtpClient::authenticate`] already succeeded.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Authenticate: `AUTH XOAUTH2` for an OAuth account, otherwise `AUTH PLAIN` when advertised,
    /// otherwise `AUTH LOGIN`.
    ///
    /// The credential is dropped and zeroised after a success. A server that offers none of the
    /// mechanisms this account can use is [`SmtpError::Unsupported`]; a rejected credential is
    /// [`SmtpError::Auth`], never a raw server `NO` that could be mistaken for a transient failure.
    /// An OAuth account never falls back to a password: it has none.
    pub async fn authenticate(&mut self) -> Result<(), SmtpError> {
        if self.closed {
            return Err(SmtpError::Closed { phase: "auth" });
        }
        if self.authenticated {
            return Ok(());
        }
        let result = if self.config.oauth.is_some() {
            self.auth_xoauth2().await
        } else if self.capabilities.auth_plain {
            self.auth_plain().await
        } else if self.capabilities.auth_login {
            self.auth_login().await
        } else {
            Err(SmtpError::Unsupported { feature: "AUTH PLAIN or AUTH LOGIN" })
        };
        match result {
            Ok(()) => {
                self.authenticated = true;
                self.config.password = Secret::new(String::new());
                self.config.oauth = None;
                Ok(())
            }
            Err(error) => {
                // Never keep the credential in memory after a refused attempt: the session is not
                // reused for auth and the caller has to decide whether to reopen.
                self.config.password = Secret::new(String::new());
                self.config.oauth = None;
                Err(error)
            }
        }
    }

    /// Submit one message: `MAIL FROM`, one `RCPT TO` per recipient, `DATA`, the body, the final `.`.
    ///
    /// The [`SmtpEnvelope`] is validated before the first command. `SIZE` is checked against the
    /// advertised ceiling before `MAIL`. After the final `.` is written, every failure is
    /// [`SmtpError::Uncertain`].
    pub async fn send(&mut self, envelope: &SmtpEnvelope, raw: &[u8], dsn: Option<&DsnOptions>) -> Result<Delivery, SmtpError> {
        if self.closed {
            return Err(SmtpError::Closed { phase: "mail" });
        }
        if !self.authenticated {
            return Err(SmtpError::Auth { reason: "authenticate before sending".to_owned() });
        }
        let limits = self.config.limits;
        validate_envelope(envelope, limits, self.capabilities.smtputf8)?;
        let prepared = prepare_data(raw, limits.max_message)?;
        // RFC 1870: `SIZE 0` means "no fixed maximum", not a ceiling of zero bytes.
        if let Some(size) = self.capabilities.size.filter(|size| *size > 0)
            && size < prepared.len() as u64
        {
            return Err(SmtpError::TooLarge { what: "message", limit: usize::try_from(size).unwrap_or(usize::MAX) });
        }
        let needs_utf8 = envelope_needs_utf8(envelope) || header_needs_utf8(raw);
        let needs_8bit = raw.iter().any(|byte| !byte.is_ascii());
        if needs_utf8 && !self.capabilities.smtputf8 {
            return Err(SmtpError::Unsupported { feature: "SMTPUTF8" });
        }
        if needs_8bit && !self.capabilities.eight_bit_mime && !needs_utf8 {
            return Err(SmtpError::Unsupported { feature: "8BITMIME" });
        }
        if dsn.is_some() && !self.capabilities.dsn {
            return Err(SmtpError::Unsupported { feature: "DSN" });
        }

        let mut mail = format!("MAIL FROM:<{}>", envelope.from);
        if self.capabilities.size.is_some() {
            mail.push_str(&format!(" SIZE={}", prepared.len()));
        }
        if needs_8bit && self.capabilities.eight_bit_mime {
            mail.push_str(" BODY=8BITMIME");
        }
        if needs_utf8 {
            mail.push_str(" SMTPUTF8");
        }
        if let Some(dsn) = dsn {
            if dsn.return_full {
                mail.push_str(" RET=FULL");
            }
            if let Some(envid) = dsn.envid.as_deref() {
                validate_xtext(envid)?;
                mail.push_str(&format!(" ENVID={envid}"));
            }
        }
        let reply = self.command("mail", &mail).await?;
        if !is_success(&reply) {
            let error = rejected("mail", &reply);
            self.note_failure(&error);
            return Err(error);
        }

        let notify =
            dsn.filter(|dsn| !dsn.notify.is_empty()).map(|dsn| dsn.notify.iter().map(|notify| notify.as_wire()).collect::<Vec<_>>().join(","));
        let mut accepted = 0usize;
        for recipient in &envelope.recipients {
            let mut line = format!("RCPT TO:<{recipient}>");
            if let Some(notify) = notify.as_deref() {
                line.push_str(&format!(" NOTIFY={notify}"));
            }
            let reply = self.command("rcpt", &line).await?;
            if is_success(&reply) {
                accepted = accepted.saturating_add(1);
                continue;
            }
            let error = rejected("rcpt", &reply);
            self.note_failure(&error);
            return Err(error);
        }
        if accepted == 0 {
            return Err(SmtpError::Transient { stage: "rcpt", reason: "the server accepted no recipient".to_owned() });
        }

        let reply = self.command("data", "DATA").await?;
        if reply.code != 354 {
            let error = rejected("data", &reply);
            self.note_failure(&error);
            return Err(error);
        }
        if let Err(error) = self.conn.write_all(&prepared, "data").await {
            self.closed = true;
            return Err(SmtpError::Uncertain { reason: sanitize_reason(&format!("writing the message body: {error}")) });
        }
        // From here on the server may have the message. Every failure is Uncertain, including a
        // 4xx/5xx reply: the README and the brief promise that nothing after the final dot is ever
        // retried automatically, and a refusal is resolved by a person who can see whether the
        // message arrived. Only the code is kept; the server text may repeat the body.
        match self.conn.read_reply(&self.config.limits, "data-end").await {
            Ok(reply) if is_success(&reply) => Ok(Delivery { code: reply.code, message: reply.text(), accepted }),
            Ok(reply) => Err(SmtpError::Uncertain { reason: format!("the server answered {} after the final dot", reply.code) }),
            Err(error) => {
                self.closed = true;
                Err(SmtpError::Uncertain { reason: error.to_string() })
            }
        }
    }

    /// `RSET`: abandon the current transaction and keep the session.
    pub async fn reset(&mut self) -> Result<(), SmtpError> {
        let reply = self.command("rset", "RSET").await?;
        if is_success(&reply) { Ok(()) } else { Err(rejected("rset", &reply)) }
    }

    /// `NOOP`: a cheap round trip.
    pub async fn noop(&mut self) -> Result<(), SmtpError> {
        let reply = self.command("noop", "NOOP").await?;
        if is_success(&reply) { Ok(()) } else { Err(rejected("noop", &reply)) }
    }

    /// Best-effort `QUIT`, then close. A broken connection on the way out is not an error.
    pub async fn quit(mut self) -> Result<(), SmtpError> {
        if !self.closed {
            let _ = self.command("quit", "QUIT").await;
        }
        self.closed = true;
        Ok(())
    }

    /// Open a session, authenticate and quit: the `account_add` and `account_test` probe.
    pub async fn verify(config: SmtpConfig) -> Result<SmtpCapabilities, SmtpError> {
        let mut client = Self::connect(config).await?;
        client.authenticate().await?;
        let capabilities = client.capabilities;
        client.quit().await?;
        Ok(capabilities)
    }

    async fn greet(&mut self) -> Result<(), SmtpError> {
        let reply = self.conn.read_reply(&self.config.limits, "greeting").await?;
        if reply.code == 220 || (200..300).contains(&reply.code) {
            return Ok(());
        }
        self.closed = true;
        Err(rejected("greeting", &reply))
    }

    async fn ehlo(&mut self) -> Result<(), SmtpError> {
        let host = sanitize_hello_name(&self.config.host);
        let reply = self.command("ehlo", &format!("EHLO {host}")).await?;
        if !is_success(&reply) {
            let error = rejected("ehlo", &reply);
            self.note_failure(&error);
            return Err(error);
        }
        self.capabilities = SmtpCapabilities::parse(&reply.lines);
        Ok(())
    }

    async fn starttls(&mut self) -> Result<(), SmtpError> {
        // The greeting may carry no extension list; EHLO before the upgrade is required.
        self.ehlo().await?;
        if !self.capabilities.starttls {
            return Err(SmtpError::Unsupported { feature: "STARTTLS" });
        }
        let reply = self.command("starttls", "STARTTLS").await?;
        if reply.code != 220 {
            let error = rejected("starttls", &reply);
            self.note_failure(&error);
            return Err(error);
        }
        if self.conn.has_buffered() {
            self.closed = true;
            return Err(SmtpError::Protocol { reason: "the server sent plaintext after the STARTTLS acceptance".to_owned() });
        }
        self.handshake().await?;
        self.capabilities = SmtpCapabilities::default();
        self.authenticated = false;
        self.ehlo().await
    }

    async fn handshake(&mut self) -> Result<(), SmtpError> {
        let config = tls::client_config(&self.config.extra_roots, &self.config.host)
            .map_err(|error| SmtpError::Tls { host: self.config.host.clone(), reason: sanitize_reason(&error.to_string()) })?;
        let connector = TlsConnector::from(config);
        let name = ServerName::try_from(self.config.host.clone())
            .map_err(|_| SmtpError::Tls { host: self.config.host.clone(), reason: "host is not a valid DNS name or IP address".to_owned() })?;
        let deadline = self.deadline();
        let stream = self.conn.take_stream();
        match timeout_at(deadline, connector.connect(name, stream)).await {
            Err(_) => {
                self.closed = true;
                Err(SmtpError::Timeout { phase: "tls", after: self.config.timeout })
            }
            Ok(Err(error)) => {
                self.closed = true;
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                ) {
                    return Err(SmtpError::Closed { phase: "tls" });
                }
                Err(SmtpError::Tls { host: self.config.host.clone(), reason: sanitize_reason(&error.to_string()) })
            }
            Ok(Ok(stream)) => {
                self.conn.set_stream(stream);
                self.conn.discard_buffered();
                Ok(())
            }
        }
    }

    async fn auth_plain(&mut self) -> Result<(), SmtpError> {
        let payload = sasl_plain(&self.config.username, self.config.password.expose());
        let line = Zeroizing::new(format!("AUTH PLAIN {}", payload.as_str()));
        let reply = self.command("auth", &line).await?;
        let reply = if reply.code == 334 {
            // Some servers ignore the initial response and ask for it again.
            let line = Zeroizing::new(payload.as_str().to_owned());
            self.command("auth", &line).await?
        } else {
            reply
        };
        if is_success(&reply) { Ok(()) } else { Err(auth_error(&reply)) }
    }

    async fn auth_login(&mut self) -> Result<(), SmtpError> {
        let reply = self.command("auth", "AUTH LOGIN").await?;
        if reply.code != 334 {
            return Err(auth_error(&reply));
        }
        let user = Zeroizing::new(STANDARD.encode(self.config.username.as_bytes()));
        let reply = self.command("auth", &user).await?;
        if reply.code != 334 {
            return Err(auth_error(&reply));
        }
        let password = Zeroizing::new(STANDARD.encode(self.config.password.expose().as_bytes()));
        let reply = self.command("auth", &password).await?;
        if is_success(&reply) { Ok(()) } else { Err(auth_error(&reply)) }
    }

    async fn auth_xoauth2(&mut self) -> Result<(), SmtpError> {
        if !self.capabilities.auth_xoauth2 {
            return Err(SmtpError::Unsupported { feature: "AUTH XOAUTH2" });
        }
        let token = match self.config.oauth.as_ref() {
            Some(token) if !token.is_empty() => token,
            _ => return Err(SmtpError::Auth { reason: "this session has no OAuth access token".to_owned() }),
        };
        let payload = crate::oauth::sasl_xoauth2(&self.config.username, token.expose());
        let line = Zeroizing::new(format!("AUTH XOAUTH2 {}", payload.as_str()));
        let reply = self.command("auth", &line).await?;
        if reply.code == 334 {
            // The provider asked for a SASL error acknowledgement. An empty line makes it send the
            // final status; the challenge text itself is never read back or logged.
            let reply = self.command("auth", "").await?;
            return if is_success(&reply) { Ok(()) } else { Err(auth_error(&reply)) };
        }
        if is_success(&reply) { Ok(()) } else { Err(auth_error(&reply)) }
    }

    async fn command(&mut self, stage: &'static str, line: &str) -> Result<Reply, SmtpError> {
        if self.closed {
            return Err(SmtpError::Closed { phase: stage });
        }
        let mut wire = Zeroizing::new(line.to_owned());
        wire.push_str("\r\n");
        log::debug!("smtp: {stage}");
        if let Err(error) = self.conn.write_all(wire.as_bytes(), stage).await {
            self.note_failure(&error);
            return Err(error);
        }
        match self.conn.read_reply(&self.config.limits, stage).await {
            Ok(reply) => Ok(reply),
            Err(error) => {
                self.note_failure(&error);
                Err(error)
            }
        }
    }

    /// A failure that leaves the stream unusable must not be retried on the same session.
    /// `Rejected` and `Auth` are complete replies, so the stream stays at a command boundary.
    fn note_failure(&mut self, error: &SmtpError) {
        if matches!(
            error,
            SmtpError::Timeout { .. }
                | SmtpError::Closed { .. }
                | SmtpError::Tls { .. }
                | SmtpError::Protocol { .. }
                | SmtpError::TooLarge { .. }
                | SmtpError::Uncertain { .. }
        ) {
            self.closed = true;
            self.conn.abandon();
        }
    }

    fn deadline(&self) -> Instant {
        Instant::now().checked_add(self.config.timeout).unwrap_or_else(Instant::now)
    }
}

struct Reply {
    code: u16,
    lines: Vec<String>,
}

impl Reply {
    fn text(&self) -> String {
        let text = self.lines.join(" ");
        sanitize_reason(&text)
    }
}

fn is_success(reply: &Reply) -> bool {
    (200..300).contains(&reply.code)
}

fn rejected(stage: &'static str, reply: &Reply) -> SmtpError {
    SmtpError::Rejected { code: reply.code, stage }
}

/// The authentication failure. The server's text is deliberately dropped: it has just received the
/// password and may repeat it, and this error is public and may be printed by any caller.
fn auth_error(reply: &Reply) -> SmtpError {
    let reason = match reply.code {
        535 => "the server rejected the credential (535)".to_owned(),
        534 => "the server requires a different authentication mechanism (534)".to_owned(),
        538 => "the server refused the encrypted authentication mechanism (538)".to_owned(),
        code => format!("the server answered {code} at authentication"),
    };
    SmtpError::Auth { reason }
}

fn validate_envelope(envelope: &SmtpEnvelope, limits: SmtpLimits, smtputf8: bool) -> Result<(), SmtpError> {
    if envelope.recipients.is_empty() {
        return Err(SmtpError::Permanent { stage: "rcpt", reason: "a message needs at least one recipient".to_owned() });
    }
    if envelope.recipients.len() > limits.max_recipients {
        return Err(SmtpError::TooLarge { what: "recipients", limit: limits.max_recipients });
    }
    validate_address("from", &envelope.from, smtputf8)?;
    for recipient in &envelope.recipients {
        validate_address("rcpt", recipient, smtputf8)?;
    }
    Ok(())
}

fn validate_address(stage: &'static str, address: &str, smtputf8: bool) -> Result<(), SmtpError> {
    if address.is_empty() || address.len() > 320 || address.chars().any(|character| character.is_control() || character.is_whitespace()) {
        return Err(SmtpError::Permanent { stage, reason: "an envelope address is empty, too long or contains whitespace".to_owned() });
    }
    if address.contains(['<', '>', ',', ';', ':']) {
        return Err(SmtpError::Permanent { stage, reason: "an envelope address contains a structural character".to_owned() });
    }
    if !address.is_ascii() && !smtputf8 {
        return Err(SmtpError::Unsupported { feature: "SMTPUTF8" });
    }
    let mut parts = address.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(SmtpError::Permanent { stage, reason: "an envelope address is not local@domain".to_owned() });
    };
    if local.is_empty() || domain.is_empty() {
        return Err(SmtpError::Permanent { stage, reason: "an envelope address is not local@domain".to_owned() });
    }
    Ok(())
}

fn envelope_needs_utf8(envelope: &SmtpEnvelope) -> bool {
    !envelope.from.is_ascii() || envelope.recipients.iter().any(|recipient| !recipient.is_ascii())
}

/// Whether the header block of the message contains bytes that require `SMTPUTF8`.
fn header_needs_utf8(raw: &[u8]) -> bool {
    let end = raw.windows(4).position(|window| window == b"\r\n\r\n").unwrap_or(raw.len());
    raw.get(..end).is_some_and(|header| header.iter().any(|byte| !byte.is_ascii()))
}

fn validate_xtext(value: &str) -> Result<(), SmtpError> {
    if value.is_empty() || value.len() > 100 || value.bytes().any(|byte| !(33..=126).contains(&byte) || byte == b'=') {
        return Err(SmtpError::Protocol { reason: "ENVID must be 1-100 printable ASCII bytes without `=`".to_owned() });
    }
    Ok(())
}

/// Apply dot-stuffing, require CRLF line endings and append the terminating `.`.
fn prepare_data(raw: &[u8], max: usize) -> Result<Vec<u8>, SmtpError> {
    if raw.len() > max {
        return Err(SmtpError::TooLarge { what: "message", limit: max });
    }
    // No bare CR or LF: SMTP data lines end with CRLF, and a bare LF (or a CR that does not start a
    // CRLF) could terminate a line early on a server that normalizes line endings.
    for (index, byte) in raw.iter().enumerate() {
        if *byte == b'\n' && (index == 0 || raw.get(index.wrapping_sub(1)) != Some(&b'\r')) {
            return Err(SmtpError::Permanent { stage: "data", reason: "the message contains a bare LF".to_owned() });
        }
        if *byte == b'\r' && raw.get(index.saturating_add(1)) != Some(&b'\n') {
            return Err(SmtpError::Permanent { stage: "data", reason: "the message contains a bare CR".to_owned() });
        }
        if *byte == 0 {
            return Err(SmtpError::Permanent { stage: "data", reason: "the message contains NUL".to_owned() });
        }
    }
    let lines = if raw.is_empty() || raw.ends_with(b"\r\n") { 0 } else { 1 };
    let dots =
        raw.iter().enumerate().filter(|(index, byte)| **byte == b'.' && (*index == 0 || raw.get(index.wrapping_sub(1)) == Some(&b'\n'))).count();
    let capacity = raw.len().saturating_add(dots).saturating_add(5).saturating_add(lines * 2);
    let mut out = Vec::with_capacity(capacity);
    let mut at_line_start = true;
    for byte in raw {
        if at_line_start && *byte == b'.' {
            out.push(b'.');
        }
        out.push(*byte);
        at_line_start = *byte == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    if out.len() > max.saturating_add(5) {
        return Err(SmtpError::TooLarge { what: "message", limit: max });
    }
    Ok(out)
}

fn sanitize_hello_name(host: &str) -> String {
    let cleaned = host.chars().filter(|character| character.is_ascii_graphic() && *character != '[' && *character != ']').collect::<String>();
    if cleaned.is_empty() { "localhost".to_owned() } else { cleaned }
}

fn sasl_plain(username: &str, password: &str) -> Zeroizing<String> {
    let mut raw = Zeroizing::new(Vec::new());
    raw.push(0);
    raw.extend_from_slice(username.as_bytes());
    raw.push(0);
    raw.extend_from_slice(password.as_bytes());
    Zeroizing::new(STANDARD.encode(raw.as_slice()))
}

async fn resolve(host: &str, port: u16, after: Duration) -> Result<Vec<SocketAddr>, SmtpError> {
    match timeout(after, lookup_host((host, port))).await {
        Err(_) => Err(SmtpError::Timeout { phase: "dns", after }),
        Ok(Err(error)) => Err(SmtpError::Dns { host: host.to_owned(), reason: sanitize_reason(&error.to_string()) }),
        Ok(Ok(addresses)) => {
            let list = addresses.collect::<Vec<_>>();
            if list.is_empty() {
                return Err(SmtpError::Dns { host: host.to_owned(), reason: "the resolver returned no addresses".to_owned() });
            }
            Ok(list)
        }
    }
}

async fn connect_any(host: &str, port: u16, addresses: &[SocketAddr], timeout_after: Duration) -> Result<TcpStream, SmtpError> {
    let deadline = Instant::now().checked_add(timeout_after).unwrap_or_else(Instant::now);
    let mut last = "no address was tried".to_owned();
    let mut left = u32::try_from(addresses.len()).unwrap_or(u32::MAX).max(1);
    for address in addresses {
        let now = Instant::now();
        if now >= deadline {
            return Err(SmtpError::Timeout { phase: "connect", after: timeout_after });
        }
        let share = deadline.saturating_duration_since(now) / left;
        let attempt_deadline = now.checked_add(share.min(Duration::from_secs(5))).unwrap_or(deadline);
        left = left.saturating_sub(1).max(1);
        match timeout_at(attempt_deadline, TcpStream::connect(address)).await {
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(SmtpError::Timeout { phase: "connect", after: timeout_after });
                }
            }
            Ok(Err(error)) => last = sanitize_reason(&error.to_string()),
            Ok(Ok(stream)) => return Ok(stream),
        }
    }
    Err(SmtpError::Connect { host: host.to_owned(), port, reason: last })
}

/// The stream before or after the TLS handshake.
enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<Stream>>),
    Placeholder,
}

impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_read(context, buffer),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_read(context, buffer),
            Self::Placeholder => std::task::Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::NotConnected))),
        }
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>, bytes: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_write(context, bytes),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_write(context, bytes),
            Self::Placeholder => std::task::Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::NotConnected))),
        }
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_flush(context),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_flush(context),
            Self::Placeholder => std::task::Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(context),
            Self::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_shutdown(context),
            Self::Placeholder => std::task::Poll::Ready(Ok(())),
        }
    }
}

struct Connection {
    stream: Stream,
    buffer: Vec<u8>,
    position: usize,
    timeout: Duration,
}

impl Connection {
    fn new(tcp: TcpStream, _limits: SmtpLimits, timeout: Duration) -> Self {
        Self { stream: Stream::Plain(tcp), buffer: Vec::new(), position: 0, timeout }
    }

    fn take_stream(&mut self) -> Stream {
        std::mem::replace(&mut self.stream, Stream::Placeholder)
    }

    fn set_stream(&mut self, stream: tokio_rustls::client::TlsStream<Stream>) {
        self.stream = Stream::Tls(Box::new(stream));
    }

    fn has_buffered(&self) -> bool {
        self.position < self.buffer.len()
    }

    fn discard_buffered(&mut self) {
        self.buffer.clear();
        self.position = 0;
    }

    fn abandon(&mut self) {
        self.stream = Stream::Placeholder;
        self.buffer.clear();
        self.position = 0;
    }

    /// Write one command or body.
    ///
    /// The timeout is per chunk, not per buffer: a 25 MiB body on a slow but healthy uplink must not
    /// be declared stuck just because one write call takes longer than a command timeout. A dead
    /// connection is still caught within one timeout, and the whole transfer is bounded by
    /// [`crate::MAX_BODY_TRANSFER`] so it cannot outlive the outbox lease.
    async fn write_all(&mut self, bytes: &[u8], phase: &'static str) -> Result<(), SmtpError> {
        const WRITE_CHUNK: usize = 16 * 1024;
        let transfer_deadline = Instant::now().checked_add(crate::MAX_BODY_TRANSFER).unwrap_or_else(Instant::now);
        for chunk in bytes.chunks(WRITE_CHUNK) {
            let chunk_deadline = Instant::now().checked_add(self.timeout).unwrap_or_else(Instant::now);
            let deadline = chunk_deadline.min(transfer_deadline);
            match timeout_at(deadline, async {
                self.stream.write_all(chunk).await?;
                self.stream.flush().await
            })
            .await
            {
                Err(_) if deadline == transfer_deadline => {
                    return Err(SmtpError::Timeout { phase, after: crate::MAX_BODY_TRANSFER });
                }
                Err(_) => return Err(SmtpError::Timeout { phase, after: self.timeout }),
                Ok(Err(error)) => return Err(closed_or_io(phase, &error)),
                Ok(Ok(())) => {}
            }
        }
        Ok(())
    }

    async fn read_reply(&mut self, limits: &SmtpLimits, phase: &'static str) -> Result<Reply, SmtpError> {
        let deadline = Instant::now().checked_add(self.timeout).unwrap_or_else(Instant::now);
        let mut lines = Vec::new();
        let mut total = 0usize;
        let mut code: Option<u16> = None;
        loop {
            let line = self.read_line(limits, phase, deadline, &mut total).await?;
            if line.len() < 3 {
                return Err(SmtpError::Protocol { reason: format!("a {phase} reply line is shorter than three digits") });
            }
            let digits = line.get(..3).unwrap_or_default();
            let Ok(code_here) = digits.parse::<u16>() else {
                return Err(SmtpError::Protocol { reason: format!("a {phase} reply line does not start with a code") });
            };
            if !(100..600).contains(&code_here) {
                return Err(SmtpError::Protocol { reason: format!("a {phase} reply code is outside 100-599") });
            }
            let separator = line.as_bytes().get(3).copied();
            let text = line.get(4..).unwrap_or("").to_owned();
            match (code, separator) {
                (None, Some(b'-' | b' ')) => code = Some(code_here),
                (Some(expected), Some(b'-' | b' ')) if expected == code_here => {}
                (Some(_), _) => {
                    return Err(SmtpError::Protocol { reason: format!("a multiline {phase} reply changed its code") });
                }
                (None, _) => {
                    return Err(SmtpError::Protocol { reason: format!("a {phase} reply has no code separator") });
                }
            }
            lines.push(text);
            if separator == Some(b' ') {
                return Ok(Reply { code: code_here, lines });
            }
            if lines.len() > 1000 {
                return Err(SmtpError::TooLarge { what: "reply lines", limit: 1000 });
            }
        }
    }

    async fn read_line(&mut self, limits: &SmtpLimits, phase: &'static str, deadline: Instant, total: &mut usize) -> Result<String, SmtpError> {
        loop {
            let available = self.buffer.get(self.position..).unwrap_or_default();
            if let Some(end) = find_crlf(available) {
                if end > limits.max_line {
                    return Err(SmtpError::TooLarge { what: "line", limit: limits.max_line });
                }
                *total = total.saturating_add(end + 2);
                if *total > limits.max_response {
                    return Err(SmtpError::TooLarge { what: "reply", limit: limits.max_response });
                }
                let raw = self.buffer.get(self.position..self.position + end).unwrap_or_default();
                if raw.iter().any(|byte| *byte == 0 || *byte == b'\r' || *byte == b'\n') {
                    return Err(SmtpError::Protocol { reason: "a reply line contains NUL, a bare CR or a bare LF".to_owned() });
                }
                let line = String::from_utf8_lossy(raw).into_owned();
                self.position += end + 2;
                self.compact();
                return Ok(line);
            }
            let room = limits.max_line.saturating_add(2);
            if available.len() > room {
                return Err(SmtpError::TooLarge { what: "line", limit: limits.max_line });
            }
            let mut chunk = [0u8; 4096];
            let read = timeout_at(deadline, self.stream.read(&mut chunk)).await;
            let count = match read {
                Err(_) => return Err(SmtpError::Timeout { phase, after: self.timeout }),
                Ok(Err(error)) => return Err(closed_or_io(phase, &error)),
                Ok(Ok(count)) => count,
            };
            if count == 0 {
                return Err(SmtpError::Closed { phase });
            }
            self.buffer.extend_from_slice(chunk.get(..count).unwrap_or_default());
        }
    }

    fn compact(&mut self) {
        if self.position == 0 {
            return;
        }
        if self.position == self.buffer.len() {
            self.buffer.clear();
            self.position = 0;
        } else if self.position >= 16 * 1024 {
            self.buffer.drain(..self.position);
            self.position = 0;
        }
    }
}

fn closed_or_io(phase: &'static str, error: &std::io::Error) -> SmtpError {
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::BrokenPipe => SmtpError::Closed { phase },
        _ => SmtpError::Transient { stage: phase, reason: sanitize_reason(&error.to_string()) },
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes.get(index) == Some(&b'\r') && bytes.get(index + 1) == Some(&b'\n') {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_read_from_the_ehlo_lines() {
        let lines = vec![
            "mail.example.com".to_owned(),
            "SIZE 35882577".to_owned(),
            "8BITMIME".to_owned(),
            "SMTPUTF8".to_owned(),
            "DSN".to_owned(),
            "PIPELINING".to_owned(),
            "AUTH PLAIN LOGIN".to_owned(),
        ];
        let capabilities = SmtpCapabilities::parse(&lines);
        assert_eq!(capabilities.size, Some(35_882_577));
        assert!(capabilities.eight_bit_mime);
        assert!(capabilities.smtputf8);
        assert!(capabilities.dsn);
        assert!(capabilities.pipelining);
        assert!(capabilities.auth_plain && capabilities.auth_login);
    }

    #[test]
    fn dot_stuffing_and_termination_are_exact() {
        let prepared = prepare_data(b"Ciao\r\n.principio\r\n..due\r\n", 1024).expect("prepared");
        assert_eq!(prepared, b"Ciao\r\n..principio\r\n...due\r\n.\r\n");
        let prepared = prepare_data(b"una riga", 1024).expect("prepared");
        assert_eq!(prepared, b"una riga\r\n.\r\n");
    }

    #[test]
    fn bare_lf_and_nul_are_refused_before_data() {
        assert!(matches!(prepare_data(b"a\nb", 1024), Err(SmtpError::Permanent { .. })));
        assert!(matches!(prepare_data(b"a\0b", 1024), Err(SmtpError::Permanent { .. })));
    }

    #[test]
    fn oversized_messages_are_refused() {
        assert!(matches!(prepare_data(&vec![b'a'; 2048], 1024), Err(SmtpError::TooLarge { .. })));
    }

    #[test]
    fn the_config_never_prints_the_secret_or_the_identity() {
        let config = SmtpConfig::new("smtp.example.com", 465, TlsMode::Implicit, "user@example.com", Secret::new("hunter2"));
        let text = format!("{config:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("user@example.com"), "{text}");
        assert!(text.contains("smtp.example.com"));
        assert!(text.contains("465"));
    }

    #[test]
    fn addresses_are_validated_before_the_wire() {
        let limits = SmtpLimits::default();
        let good = SmtpEnvelope { from: "a@example.com".to_owned(), recipients: vec!["b@example.com".to_owned()] };
        assert!(validate_envelope(&good, limits, false).is_ok());
        for bad in [
            SmtpEnvelope { from: "a@example.com".to_owned(), recipients: Vec::new() },
            SmtpEnvelope { from: "a@example.com".to_owned(), recipients: vec!["not-an-address".to_owned()] },
            SmtpEnvelope { from: "a@example.com".to_owned(), recipients: vec!["a@example.com\r\nRCPT TO:<x@example.com>".to_owned()] },
        ] {
            assert!(validate_envelope(&bad, limits, false).is_err(), "{bad:?}");
        }
        let many = SmtpEnvelope { from: "a@example.com".to_owned(), recipients: vec!["b@example.com".to_owned(); 101] };
        assert!(matches!(validate_envelope(&many, limits, false), Err(SmtpError::TooLarge { .. })));
    }

    #[test]
    fn error_classification_matches_the_outbox_policy() {
        assert!(SmtpError::Transient { stage: "mail", reason: "reset".to_owned() }.is_transient());
        assert!(SmtpError::Rejected { code: 451, stage: "data" }.is_transient());
        assert!(!SmtpError::Rejected { code: 550, stage: "rcpt" }.is_transient());
        assert!(!SmtpError::Uncertain { reason: "dropped".to_owned() }.is_transient());
        assert!(SmtpError::Uncertain { reason: "dropped".to_owned() }.is_uncertain());
        assert!(SmtpError::Auth { reason: "bad".to_owned() }.is_authentication());
    }

    #[test]
    fn size_limit_is_compared_before_mail() {
        let limits = SmtpLimits::default();
        assert_eq!(limits.clamped(), limits);
        let huge = SmtpLimits { max_message: usize::MAX, ..limits }.clamped();
        assert_eq!(huge.max_message, SmtpLimits::HARD_MAX_MESSAGE);
    }
}
