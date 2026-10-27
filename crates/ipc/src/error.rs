//! One error type for the transport, the handshake and a refused request.

use crate::protocol::RpcError;

/// A result from this crate.
pub type Result<T> = std::result::Result<T, IpcError>;

/// A transport or protocol failure. Every variant is safe to show to a person: none carries a
/// token, a credential, a message body or a path outside the endpoint/token file the caller
/// already named.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("local transport failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("a frame exceeds the {limit} byte limit")]
    FrameTooLarge { limit: usize },
    #[error("the daemon closed the connection")]
    Closed,
    #[error("timed out while {0}")]
    Timeout(&'static str),
    /// Deliberately generic: a wrong token and an unknown endpoint must not become an oracle.
    #[error("the daemon refused this client")]
    Unauthorized,
    #[error("protocol version mismatch: the client speaks {client}, the daemon speaks {daemon}")]
    Version { client: u32, daemon: u32 },
    #[error("the daemon is busy: {0}")]
    Busy(String),
    #[error("the request was cancelled")]
    Cancelled,
    #[error("too many clients or requests: try again")]
    Overloaded,
    #[error("the token file is not owner-only: {0}")]
    InsecureTokenFile(String),
    #[error("the token file is unavailable: {0}")]
    TokenFile(String),
    #[error("the daemon returned an error: {0}")]
    Rpc(#[from] RpcError),
    #[error("the OS random source failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("the daemon binding is invalid: {0}")]
    Configuration(String),
}

impl IpcError {
    /// Whether a caller may retry the same request on a new connection without changing anything.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Closed | Self::Timeout(_) | Self::Overloaded)
    }

    /// Whether the failure means "the daemon is not running" rather than "the daemon said no".
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Io(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut))
            || matches!(self, Self::Closed | Self::Timeout(_))
    }
}

impl From<tokio::time::error::Elapsed> for IpcError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Timeout("waiting for the daemon")
    }
}
