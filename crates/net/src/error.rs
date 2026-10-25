use std::time::Duration;

use thiserror::Error;

/// Longest server-supplied text kept inside an error, in bytes.
pub(crate) const MAX_REASON: usize = 200;

/// Everything this crate refuses, with a message an agent or a log can act on.
///
/// A message never contains the password, a command line or certificate bytes. Server text is
/// reduced to printable ASCII and cut to [`MAX_REASON`] bytes before it enters an error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NetError {
    /// The host name could not be resolved.
    #[error("could not resolve {host}: {reason}")]
    Dns {
        /// Host name as configured.
        host: String,
        /// Resolver message, sanitised.
        reason: String,
    },
    /// The TCP connection could not be established.
    #[error("could not connect to {host}:{port}: {reason}")]
    Connect {
        /// Host name as configured.
        host: String,
        /// Port as configured.
        port: u16,
        /// Operating-system message, sanitised.
        reason: String,
    },
    /// The TLS handshake or certificate verification failed.
    #[error("TLS with {host} failed: {reason}")]
    Tls {
        /// Host name that was verified.
        host: String,
        /// Verification failure, sanitised. Never contains the certificate.
        reason: String,
    },
    /// The server rejected the credentials, or refused to accept them at all.
    #[error("authentication failed: {reason}")]
    Auth {
        /// What the server said, sanitised.
        reason: String,
    },
    /// A phase of the session exceeded its timeout.
    ///
    /// The session is unusable after this error and must be reopened.
    #[error("{phase} timed out after {after:?}")]
    Timeout {
        /// Phase that timed out: `dns`, `connect`, `tls`, `greeting`, `capability`, ...
        phase: &'static str,
        /// Configured timeout.
        after: Duration,
    },
    /// The connection ended, or the client closed it, during a phase.
    #[error("the connection closed during {phase}")]
    Closed {
        /// Phase that lost the connection.
        phase: &'static str,
    },
    /// The exchange cannot proceed: the server sent malformed IMAP, or the caller asked for a
    /// request this crate refuses to build.
    ///
    /// `reason` says which, in printable ASCII.
    #[error("the exchange cannot proceed: {reason}")]
    Protocol {
        /// What is wrong.
        reason: String,
    },
    /// The server answered `NO` to a command.
    #[error("{command} was refused: {reason}")]
    ServerNo {
        /// Command verb, without arguments.
        command: &'static str,
        /// Response code and text, sanitised.
        reason: String,
    },
    /// The server answered `BAD` to a command.
    #[error("{command} was rejected: {reason}")]
    ServerBad {
        /// Command verb, without arguments.
        command: &'static str,
        /// Response code and text, sanitised.
        reason: String,
    },
    /// An input or a response reached a configured bound.
    #[error("{what} exceeds the limit of {limit}")]
    TooLarge {
        /// The bound that was hit: `line`, `literal`, `command`, `untagged responses`, ...
        what: &'static str,
        /// Configured ceiling, in bytes or in items.
        limit: usize,
    },
    /// The server does not offer a capability this crate requires.
    #[error("{feature} is not supported by this server")]
    Unsupported {
        /// Missing capability or feature.
        feature: &'static str,
    },
}

/// Reduce untrusted text to bounded printable ASCII.
pub(crate) fn sanitize_reason(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_REASON));
    for ch in text.chars() {
        if out.len() >= MAX_REASON {
            break;
        }
        if ch.is_ascii_graphic() || ch == ' ' {
            out.push(ch);
        } else if ch.is_ascii() {
            out.push(' ');
        } else {
            out.push('?');
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() { "no reason given".to_owned() } else { trimmed.to_owned() }
}

/// Build a [`NetError::Protocol`] with a sanitised reason.
pub(crate) fn protocol(reason: impl Into<String>) -> NetError {
    NetError::Protocol { reason: sanitize_reason(&reason.into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_printable_and_bounded() {
        let dirty = format!("bad\tthing\r\n{}", "x".repeat(MAX_REASON * 2));
        let clean = sanitize_reason(&dirty);
        assert!(clean.len() <= MAX_REASON);
        assert!(clean.bytes().all(|byte| byte.is_ascii_graphic() || byte == b' '));
        assert!(clean.starts_with("bad thing"));
    }

    #[test]
    fn empty_reason_stays_readable() {
        assert_eq!(sanitize_reason(" \u{0}\t "), "no reason given");
    }

    #[test]
    fn protocol_error_never_carries_raw_bytes() {
        let error = protocol("nope\u{1F600}\u{0}");
        let text = error.to_string();
        assert!(text.is_ascii(), "{text}");
        assert!(text.contains("nope"));
    }
}
