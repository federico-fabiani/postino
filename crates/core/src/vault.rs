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

use crate::oauth::OAuthGrant;

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
/// the account is removed. A missing password is `Ok(None)`: it is a state, not a crash. The SMTP
/// password is separate: when no `smtp` credential exists, the engine falls back to the IMAP one
/// (the common Aruba/Gmail app-password setup) and never copies it into the other scope.
pub trait CredentialVault: Send + Sync {
    /// Create or replace one account's IMAP password.
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError>;

    /// Read one account's IMAP password. `Ok(None)` when no credential is stored.
    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError>;

    /// Delete one account's IMAP password. Deleting a missing credential succeeds, so cleanup is
    /// retryable.
    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError>;

    /// Create or replace one account's SMTP password.
    fn store_smtp_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError>;

    /// Read one account's SMTP password. `Ok(None)` means "use the IMAP credential".
    fn load_smtp_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError>;

    /// Delete one account's SMTP password. Deleting a missing credential succeeds.
    fn delete_smtp_password(&self, account_id: &str) -> Result<(), VaultError>;

    /// Create or replace one account's OAuth grant: the BYO client id and secret plus the refresh
    /// token, in one entry. Its presence is what makes the account an OAuth account; an account with
    /// only passwords never has one. The default fails loudly so a vault that predates OAuth cannot
    /// silently store a token somewhere else or drop it.
    fn store_oauth_grant(&self, _account_id: &str, _grant: &OAuthGrant) -> Result<(), VaultError> {
        Err(VaultError::new("this credential vault does not support OAuth grants"))
    }

    /// Read one account's OAuth grant. `Ok(None)` means the account uses an app password. The
    /// default is `Ok(None)`, so every existing vault keeps its password accounts working.
    fn load_oauth_grant(&self, _account_id: &str) -> Result<Option<OAuthGrant>, VaultError> {
        Ok(None)
    }

    /// Delete one account's OAuth grant. Deleting a missing grant succeeds.
    fn delete_oauth_grant(&self, _account_id: &str) -> Result<(), VaultError> {
        Ok(())
    }
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
