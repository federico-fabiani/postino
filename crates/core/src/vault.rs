//! The credential seam between the engine and the operating system's keyring.
//!
//! The engine never calls a keyring crate. It asks a [`CredentialVault`] scoped by account id, and
//! the production adapter (the CLI today, the desktop app in P6b) maps that id to
//! `postino-platform`'s `account:<id>:imap` scope. Tests pass an in-memory vault, so no test can
//! touch the real vault or `%LOCALAPPDATA%\Postino`.
//!
//! Errors are sentences safe to show: a platform adapter must reduce its own error to text before it
//! builds a [`VaultError`], and no error may contain a password. The loaded value is a zeroising
//! buffer; callers must not format or clone it without the same guarantee.

use zeroize::Zeroizing;

/// A credential-vault failure, already reduced to a redacted sentence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct VaultError {
    message: String,
}

impl VaultError {
    /// Wrap a redacted, actionable sentence. Never pass secret bytes.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    /// The sentence, for adding context in an operation error.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Storage for account credentials that must survive a restart without touching the database.
///
/// The engine stores an account's password only after the server accepted it, and deletes it when
/// the account is removed. A missing password is `Ok(None)`: it is a state, not a crash.
pub trait CredentialVault: Send + Sync {
    /// Create or replace one account's IMAP password.
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError>;

    /// Read one account's IMAP password. `Ok(None)` when no credential is stored.
    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError>;

    /// Delete one account's IMAP password. Deleting a missing credential succeeds, so cleanup is
    /// retryable.
    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vault_error_prints_only_the_redacted_sentence() {
        let error = VaultError::new("the OS keyring is unavailable");
        assert_eq!(error.to_string(), "the OS keyring is unavailable");
        assert_eq!(error.message(), "the OS keyring is unavailable");
    }
}
