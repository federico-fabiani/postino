//! Gmail OAuth 2.0 for IMAP and SMTP: the grant the keyring keeps and the short-lived access token.
//!
//! Two secrets have two lifetimes. The **refresh token** and the BYO **client secret** are durable and
//! live in the OS keyring under `account:<id>:oauth`; the **access token** lives only in this engine's
//! memory, is reused until [`postino_net::REFRESH_SKEW`] before its expiry, and is never written to
//! the store, a log line, an error sentence or an operation result.
//!
//! The auth kind is not a store column: an account whose keyring holds an OAuth grant authenticates
//! with XOAUTH2, and an account with only an IMAP/SMTP password is an app-password account exactly as
//! before. A grant that cannot be refreshed fails with a sentence; it never falls back to a password.
//!
//! The provider is a seam ([`postino_net::OAuthProvider`]), not a Gmail special case: the grant
//! records nothing about the provider, and the account row decides which endpoint is used.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postino_net::oauth::TokenEndpoint;
use postino_net::{CertificateDer, ImapConfig, NetError, OAuthProvider, REFRESH_SKEW, Secret, SmtpConfig};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::engine::{Engine, OpError};
use crate::store_map;
use crate::vault::VaultError;

/// Longest accepted client id. Google ids are ~72 characters; the ceiling is a bound, not a format.
pub const MAX_CLIENT_ID_BYTES: usize = 256;
/// Longest accepted client secret.
pub const MAX_CLIENT_SECRET_BYTES: usize = 512;
/// Longest accepted refresh token. Google issues ~100 characters; the ceiling bounds the keyring.
pub const MAX_REFRESH_TOKEN_BYTES: usize = 4096;
/// A refresh is a small POST: it must not hold an operation for the consent budget.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
/// A revocation is a courtesy call during `account_remove`; it must not delay the removal.
pub const REVOKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the browser authorization may take before the loopback listener gives up. The user may
/// have to sign in and consent, so this is generous but bounded.
pub const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(300);

/// The durable half of an authorization: what the keyring keeps and the engine reuses.
///
/// The client id is not secret; the client secret and the refresh token are, and are zeroised on
/// drop. [`Debug`] prints neither value, so a `{:?}` of a setup or a request cannot leak them.
#[derive(Clone)]
pub struct OAuthGrant {
    client_id: String,
    client_secret: Zeroizing<String>,
    refresh_token: Zeroizing<String>,
}

#[derive(Serialize, Deserialize)]
struct GrantEnvelope {
    v: u8,
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

impl OAuthGrant {
    /// Build a grant from the three values an authorization returns.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>, refresh_token: impl Into<String>) -> Self {
        Self { client_id: client_id.into(), client_secret: Zeroizing::new(client_secret.into()), refresh_token: Zeroizing::new(refresh_token.into()) }
    }

    /// The BYO client id. Not a secret.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The BYO client secret. Must be passed to the token endpoint, never formatted.
    #[must_use]
    pub fn client_secret(&self) -> &str {
        self.client_secret.as_str()
    }

    /// The refresh token. Must be passed to the token endpoint, never formatted.
    #[must_use]
    pub fn refresh_token(&self) -> &str {
        self.refresh_token.as_str()
    }

    /// Reject a grant that cannot be true before it reaches a keyring or a network call.
    pub fn validate(&self) -> Result<(), String> {
        let checks = [
            ("client id", self.client_id.len(), MAX_CLIENT_ID_BYTES, self.client_id.chars().any(char::is_control)),
            ("client secret", self.client_secret.len(), MAX_CLIENT_SECRET_BYTES, self.client_secret.chars().any(char::is_control)),
            ("refresh token", self.refresh_token.len(), MAX_REFRESH_TOKEN_BYTES, self.refresh_token.chars().any(char::is_control)),
        ];
        for (what, length, ceiling, control) in checks {
            if length == 0 {
                return Err(format!("the OAuth {what} is empty"));
            }
            if length > ceiling {
                return Err(format!("the OAuth {what} is longer than {ceiling} bytes"));
            }
            if control {
                return Err(format!("the OAuth {what} contains a control character"));
            }
        }
        Ok(())
    }

    /// The keyring encoding. One entry carries the whole grant so a rotation is one write.
    #[must_use]
    pub fn to_bytes(&self) -> Zeroizing<Vec<u8>> {
        let envelope = GrantEnvelope {
            v: 1,
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.to_string(),
            refresh_token: self.refresh_token.to_string(),
        };
        match serde_json::to_vec(&envelope) {
            Ok(bytes) => Zeroizing::new(bytes),
            // Serialising three strings cannot fail; an empty buffer still refuses the grant on read
            // instead of writing something a later load would misread.
            Err(_) => Zeroizing::new(Vec::new()),
        }
    }

    /// Read a keyring entry written by [`Self::to_bytes`]. The error is a sentence, never the bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, VaultError> {
        if bytes.is_empty() || bytes.len() > MAX_REFRESH_TOKEN_BYTES + MAX_CLIENT_SECRET_BYTES + MAX_CLIENT_ID_BYTES + 128 {
            return Err(VaultError::new("the stored OAuth grant is empty or larger than any valid grant"));
        }
        let envelope: GrantEnvelope = serde_json::from_slice(bytes)
            .map_err(|_| VaultError::new("the stored OAuth grant is not readable; remove the account and add it again with OAuth"))?;
        if envelope.v != 1 {
            return Err(VaultError::new(format!("the stored OAuth grant has version {}; this build reads version 1 only", envelope.v)));
        }
        let grant = Self::new(envelope.client_id, envelope.client_secret, envelope.refresh_token);
        grant
            .validate()
            .map_err(|reason| VaultError::new(format!("the stored OAuth grant is invalid ({reason}); remove the account and add it again")))?;
        Ok(grant)
    }
}

impl std::fmt::Debug for OAuthGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthGrant")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .finish()
    }
}

impl PartialEq for OAuthGrant {
    fn eq(&self, other: &Self) -> bool {
        self.client_id == other.client_id
            && self.client_secret.as_str() == other.client_secret.as_str()
            && self.refresh_token.as_str() == other.refresh_token.as_str()
    }
}

impl Eq for OAuthGrant {}

/// One IMAP or SMTP credential: a stored password or a freshly refreshed access token.
pub(crate) enum Credential {
    Password(Zeroizing<String>),
    OAuth(Zeroizing<String>),
}

impl Credential {
    /// True when the account authenticates with XOAUTH2. Used by tests and error wording only.
    #[cfg(test)]
    fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }
}

/// A cached access token and the instant it stops being usable.
#[derive(Clone)]
pub(crate) struct CachedAccess {
    text: Zeroizing<String>,
    expires_at_ms: i64,
}

impl CachedAccess {
    /// Fresh means it is worth more than [`REFRESH_SKEW`] of remaining life.
    fn is_fresh(&self, now_ms: i64) -> bool {
        self.expires_at_ms.saturating_sub(now_ms) > i64::try_from(REFRESH_SKEW.as_millis()).unwrap_or(i64::MAX)
    }

    /// Keep the token a code exchange just produced, so `account_add` can probe with it.
    pub(crate) fn from_hint(hint: &OAuthAccess) -> Self {
        Self { text: Zeroizing::new(hint.token().to_owned()), expires_at_ms: hint.expires_at_ms }
    }
}

/// An access token produced by one code exchange, with the instant it stops being worth using.
///
/// It is memory-only: the CLI and the desktop hand it to `account_add` so the first probe spends the
/// token the exchange already produced instead of asking the token endpoint again immediately. The
/// refresh token, not this value, is what reaches the OS keyring.
#[derive(Clone)]
pub struct OAuthAccess {
    text: Zeroizing<String>,
    expires_at_ms: i64,
}

impl OAuthAccess {
    pub fn new(token: impl Into<String>, expires_at_ms: i64) -> Self {
        Self { text: Zeroizing::new(token.into()), expires_at_ms }
    }

    /// Build one from the token lifetime the provider just announced.
    pub fn from_expires_in(token: impl Into<String>, expires_in: Duration) -> Self {
        let expires_at_ms = now_ms().saturating_add(i64::try_from(expires_in.as_millis()).unwrap_or(i64::MAX));
        Self::new(token, expires_at_ms)
    }

    /// Whether it is worth more than [`REFRESH_SKEW`] of remaining life.
    pub fn is_fresh(&self, now_ms: i64) -> bool {
        self.expires_at_ms.saturating_sub(now_ms) > i64::try_from(REFRESH_SKEW.as_millis()).unwrap_or(i64::MAX)
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    /// Freshness against the current clock.
    pub fn is_fresh_now(&self) -> bool {
        self.is_fresh(now_ms())
    }

    /// The token itself. Never stored, logged or formatted.
    pub fn token(&self) -> &str {
        self.text.as_str()
    }
}

impl std::fmt::Debug for OAuthAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OAuthAccess").field("token", &"[redacted]").field("expires_at_ms", &self.expires_at_ms).finish()
    }
}

/// What one browser authorization produced: the durable grant and the short-lived access token for
/// the first probe. The access token is memory-only and is dropped by the caller.
pub struct Authorized {
    grant: OAuthGrant,
    access: OAuthAccess,
}

impl Authorized {
    /// The grant to store in the OS keyring.
    pub fn grant(&self) -> &OAuthGrant {
        &self.grant
    }

    /// Take the grant, leaving the access token to be dropped.
    pub fn into_grant(self) -> OAuthGrant {
        self.grant
    }

    /// The access token for the first IMAP/SMTP probe. Never stored, logged or formatted.
    pub fn access(&self) -> &str {
        self.access.token()
    }

    /// A copy of the access token and its expiry, to hand to `account_add` so it does not refresh
    /// the grant the exchange just produced.
    pub fn access_hint(&self) -> OAuthAccess {
        self.access.clone()
    }
}

/// Run the Gmail authorization-code flow once and return the grant to persist.
///
/// `open` is injected (the CLI passes `postino_platform::open_in_browser`, the desktop passes the
/// same) so core keeps no browser dependency and tests can drive the loopback callback. The access
/// token is returned only for the probe that follows; the caller stores the refresh token in the OS
/// keyring. `cancelled` lets a UI stop waiting for the browser.
pub fn authorize(
    client_id: &str,
    client_secret: &str,
    open: impl FnOnce(&str) -> Result<(), String>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Authorized, OpError> {
    let provider = OAuthProvider::gmail();
    let authorization = postino_net::oauth::Authorization::new(provider, client_id, client_secret).with_timeout(AUTHORIZE_TIMEOUT);
    let tokens = authorization.run(open, cancelled).map_err(authorize_error)?;
    let refresh = tokens.refresh_token().ok_or_else(|| {
        OpError::Auth("Google returned no refresh token: remove Postino from the account's third-party access and authorize again".to_owned())
    })?;
    let access = OAuthAccess::from_expires_in(tokens.access_token().expose(), tokens.expires_in());
    let grant = OAuthGrant::new(client_id, client_secret, refresh.expose());
    grant.validate().map_err(|reason| OpError::InvalidArgs(format!("the OAuth authorization was refused: {reason}")))?;
    Ok(Authorized { grant, access })
}

fn authorize_error(error: NetError) -> OpError {
    match error {
        NetError::OAuthConsent { reason } => OpError::Auth(format!("the Google authorization was not granted: {reason}")),
        NetError::OAuthRejected { .. } | NetError::OAuthGrant { .. } => {
            OpError::Auth("Google refused the authorization: check the client id and secret and authorize again".to_owned())
        }
        NetError::OAuthToken { .. } | NetError::Protocol { .. } => {
            OpError::Failed("the token endpoint answered with something Postino cannot use; authorize again".to_owned())
        }
        other => OpError::Network(format!("the Google authorization could not be completed: {other}")),
    }
}

/// The result of one refresh: the access token to use now, and a rotated grant to persist first.
pub(crate) struct Refreshed {
    cached: CachedAccess,
    pub(crate) rotated: Option<OAuthGrant>,
}

impl Refreshed {
    pub(crate) fn access(&self) -> Zeroizing<String> {
        self.cached.text.clone()
    }

    /// A copy of the cache entry, to keep the token `add_account` already refreshed instead of
    /// refreshing again for the first sync.
    pub(crate) fn cached(&self) -> CachedAccess {
        self.cached.clone()
    }
}

/// An IMAP configuration for either credential. A password account is untouched; an OAuth account
/// carries the token and an empty password, so the session cannot fall back to LOGIN.
pub(crate) fn imap_config(server: &postino_store::AccountServer, credential: Credential, extra_roots: Vec<CertificateDer<'static>>) -> ImapConfig {
    imap_config_for(server.host.clone(), server.port, store_map::mail_tls(server.tls), server.username.clone(), credential, extra_roots)
}

/// The same builder for an account that is being added and has no store row yet.
pub(crate) fn imap_config_for(
    host: String,
    port: u16,
    tls: crate::model::MailTls,
    username: String,
    credential: Credential,
    extra_roots: Vec<CertificateDer<'static>>,
) -> ImapConfig {
    let mode = store_map::net_tls(tls);
    let mut config = match credential {
        Credential::Password(password) => ImapConfig::new(host, port, mode, username, Secret::new(password.as_str().to_owned())),
        Credential::OAuth(token) => {
            ImapConfig::new(host, port, mode, username, Secret::new(String::new())).with_oauth_token(Secret::new(token.as_str().to_owned()))
        }
    };
    for root in extra_roots {
        config = config.with_extra_root(root);
    }
    config
}

/// A submission configuration for either credential. Same rule: no password next to a token.
pub(crate) fn smtp_config(
    host: String,
    port: u16,
    tls: crate::model::MailTls,
    username: String,
    credential: Credential,
    extra_roots: Vec<CertificateDer<'static>>,
) -> SmtpConfig {
    let mode = store_map::net_tls(tls);
    let mut config = match credential {
        Credential::Password(password) => SmtpConfig::new(host, port, mode, username, Secret::new(password.as_str().to_owned())),
        Credential::OAuth(token) => {
            SmtpConfig::new(host, port, mode, username, Secret::new(String::new())).with_oauth_token(Secret::new(token.as_str().to_owned()))
        }
    };
    for root in extra_roots {
        config = config.with_extra_root(root);
    }
    config
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}

impl Engine {
    /// The stored grant for one account, or `None` for an app-password account.
    pub(crate) fn oauth_grant(&self, account_id: &str) -> Result<Option<OAuthGrant>, OpError> {
        let vault = self.require_vault("reading the OAuth grant")?;
        vault
            .load_oauth_grant(account_id)
            .map_err(|error| OpError::Failed(format!("the OS keyring could not provide the OAuth grant for account {account_id:?}: {error}")))
    }

    /// The provider registered for this account's name. The seam decides, not a `gmail` string test
    /// scattered through the engine.
    pub(crate) fn oauth_provider(&self, account_id: &str) -> Result<OAuthProvider, OpError> {
        let account = self.find_account(account_id)?;
        self.oauth_provider_for(&account.provider).ok_or_else(|| {
            OpError::InvalidArgs(format!(
                "account {account_id:?} uses provider {:?}, which has no OAuth provider in this build; remove it and add it again with an app password",
                account.provider
            ))
        })
    }

    /// The OAuth seam for a provider name, with the debug-only test endpoint override applied. An
    /// account is not in the engine yet while `add_account` refreshes it, so this takes the name.
    pub(crate) fn oauth_provider_for(&self, provider_name: &str) -> Option<OAuthProvider> {
        let provider = OAuthProvider::by_name(provider_name)?;
        #[cfg(debug_assertions)]
        if let Some((authorize, token, revoke)) = &self.test_oauth_endpoints {
            // The provider stores `&'static str`; a debug-only hook may leak the test endpoint once
            // per refresh, which is irrelevant for a process that lives for one test run.
            return Some(provider.with_endpoints(
                Box::leak(authorize.clone().into_boxed_str()),
                Box::leak(token.clone().into_boxed_str()),
                Box::leak(revoke.clone().into_boxed_str()),
            ));
        }
        Some(provider)
    }

    fn oauth_endpoint(&self, account_id: &str, grant: &OAuthGrant, timeout: Duration) -> Result<TokenEndpoint, OpError> {
        let provider = self.oauth_provider(account_id)?;
        Ok(TokenEndpoint::new(provider, grant.client_id(), grant.client_secret()).with_extra_roots(self.extra_roots.clone()).with_timeout(timeout))
    }

    /// A usable access token for one account, refreshed at most once per expiry.
    ///
    /// The cache is memory-only and holds a zeroising copy. A refresh that returns a rotated refresh
    /// token writes it back before the token is used, so a crash after a rotation cannot lose it.
    pub(crate) fn oauth_access(&self, account_id: &str, operation: &str) -> Result<Zeroizing<String>, OpError> {
        if let Some(cached) = self.oauth_access.borrow().get(account_id)
            && cached.is_fresh(now_ms())
        {
            return Ok(cached.text.clone());
        }
        let grant = self.oauth_grant(account_id)?.ok_or_else(|| {
            OpError::Failed(format!("account {account_id:?} has no OAuth grant in the OS keyring; add it again with OAuth, or use an app password"))
        })?;
        let provider = self.oauth_provider(account_id)?;
        let refreshed = self.refresh_grant(provider, &grant, account_id, operation)?;
        if let Some(rotated) = &refreshed.rotated {
            self.store_oauth_grant(account_id, rotated)?;
        }
        let access = refreshed.cached.text.clone();
        self.oauth_access.borrow_mut().insert(account_id.to_owned(), refreshed.cached);
        Ok(access)
    }

    /// Refresh one grant without reading or writing the keyring. Callers that are adding an account
    /// refresh before the grant is stored; a rotated token comes back in [`Refreshed::rotated`] so
    /// the caller can store the one the provider will honour next time.
    pub(crate) fn refresh_grant(&self, provider: OAuthProvider, grant: &OAuthGrant, account_id: &str, operation: &str) -> Result<Refreshed, OpError> {
        grant.validate().map_err(|reason| OpError::InvalidArgs(format!("the OAuth grant was refused: {reason}")))?;
        let endpoint = TokenEndpoint::new(provider, grant.client_id(), grant.client_secret())
            .with_extra_roots(self.extra_roots.clone())
            .with_timeout(REFRESH_TIMEOUT);
        let tokens = endpoint.refresh_blocking(grant.refresh_token()).map_err(|error| refresh_error(account_id, operation, &error))?;
        let access = Zeroizing::new(tokens.access_token().expose().to_owned());
        let rotated = tokens
            .refresh_token()
            .filter(|token| token.expose() != grant.refresh_token())
            .map(|token| OAuthGrant::new(grant.client_id(), grant.client_secret(), token.expose()));
        let expires_in_ms = i64::try_from(tokens.expires_in().as_millis()).unwrap_or(i64::MAX);
        let expires_at_ms = now_ms().saturating_add(expires_in_ms);
        Ok(Refreshed { cached: CachedAccess { text: access.clone(), expires_at_ms }, rotated })
    }

    /// The credential an operation should use: a fresh access token when the keyring holds a grant,
    /// otherwise the stored password, exactly as before.
    pub(crate) fn credential(&self, account_id: &str, operation: &str) -> Result<Credential, OpError> {
        match self.oauth_grant(account_id)? {
            Some(_) => Ok(Credential::OAuth(self.oauth_access(account_id, operation)?)),
            None => Ok(Credential::Password(self.load_password(account_id, operation)?)),
        }
    }

    /// The submission credential for one account: the OAuth token when there is a grant, otherwise
    /// the SMTP secret or the IMAP fallback.
    pub(crate) fn smtp_credential(&self, account_id: &str, operation: &str) -> Result<Credential, OpError> {
        match self.oauth_grant(account_id)? {
            Some(_) => Ok(Credential::OAuth(self.oauth_access(account_id, operation)?)),
            None => Ok(Credential::Password(self.load_smtp_password(account_id, operation)?)),
        }
    }

    /// Write the grant to the keyring and drop any cached access token for the account.
    pub(crate) fn store_oauth_grant(&self, account_id: &str, grant: &OAuthGrant) -> Result<(), OpError> {
        grant.validate().map_err(|reason| OpError::InvalidArgs(format!("the OAuth grant was refused: {reason}")))?;
        let vault = self.require_vault("storing the OAuth grant")?;
        vault
            .store_oauth_grant(account_id, grant)
            .map_err(|error| OpError::Failed(format!("the OAuth grant could not be stored in the OS keyring: {error}")))
    }

    /// Forget a cached access token, e.g. after a refused login.
    pub(crate) fn forget_oauth_access(&self, account_id: &str) {
        self.oauth_access.borrow_mut().remove(account_id);
    }

    /// Revoke the stored grant at the provider.
    ///
    /// Returns `None` when there was nothing to revoke or the provider accepted the request, and a
    /// bounded warning sentence when it could not. It never fails: `account_remove` has already
    /// deleted the local account, so a revocation failure must not turn into a removal error and
    /// must not make a retry look like an unknown account. The warning says that the grant stays
    /// valid until it expires or the user revokes it in the provider's security page.
    pub(crate) fn revoke_oauth(&self, account_id: &str) -> Option<String> {
        let grant = match self.oauth_grant(account_id) {
            Ok(Some(grant)) => grant,
            Ok(None) => return None,
            Err(error) => {
                return Some(format!(
                    "the OAuth grant for account {account_id:?} could not be read before revocation ({error}); revoke Postino's access in the Google account security page"
                ));
            }
        };
        let endpoint = match self.oauth_endpoint(account_id, &grant, REVOKE_TIMEOUT) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return Some(format!(
                    "the OAuth grant for account {account_id:?} could not be revoked ({error}); revoke Postino's access in the Google account security page"
                ));
            }
        };
        if let Err(error) = endpoint.revoke_blocking(grant.refresh_token()) {
            return Some(format!(
                "the OAuth grant for account {account_id:?} could not be revoked ({error}); revoke Postino's access in the Google account security page"
            ));
        }
        self.forget_oauth_access(account_id);
        None
    }
}

/// Turn a token-endpoint failure into an actionable sentence that never quotes the endpoint body.
fn refresh_error(account_id: &str, operation: &str, error: &NetError) -> OpError {
    match error {
        NetError::OAuthConsent { .. } | NetError::OAuthRejected { .. } | NetError::OAuthGrant { .. } => OpError::Auth(format!(
            "Google refused the stored authorization for account {account_id:?} while {operation} ({error}); remove the account and add it again with OAuth"
        )),
        NetError::OAuthToken { .. } | NetError::Protocol { .. } | NetError::Unsupported { .. } => OpError::Failed(format!(
            "the token endpoint answered account {account_id:?} with something unusable while {operation} ({error}); try again, and add the account again if it repeats"
        )),
        _ => OpError::Network(format!(
            "the token endpoint for account {account_id:?} could not be reached while {operation} ({error}); check the connection and retry"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_access_hint_and_revoke_budget_are_bounded_and_redacted() {
        let access = OAuthAccess::new("ya29.access-secret", 42);
        let printed = format!("{access:?}");
        assert!(!printed.contains("ya29.access-secret"), "{printed}");
        assert!(printed.contains("[redacted]"), "{printed}");
        assert_eq!(access.expires_at_ms(), 42);
        assert!(!access.is_fresh(0), "an expiry in 1970 is not fresh");
        assert!(OAuthAccess::from_expires_in("ya29.other", Duration::from_secs(3600)).is_fresh_now());
        assert_eq!(REVOKE_TIMEOUT, Duration::from_secs(15), "account_remove must not hang on Google");
    }

    #[test]
    fn a_grant_round_trips_without_printing_its_secrets() {
        let grant = OAuthGrant::new("123.apps.googleusercontent.com", "GOCSPX-secret", "1//refresh");
        let bytes = grant.to_bytes();
        let read = OAuthGrant::from_bytes(&bytes).expect("round trip");
        assert_eq!(read, grant);
        let printed = format!("{grant:?}");
        assert!(printed.contains("123.apps.googleusercontent.com"), "{printed}");
        assert!(!printed.contains("GOCSPX-secret"), "{printed}");
        assert!(!printed.contains("1//refresh"), "{printed}");
    }

    #[test]
    fn an_invalid_grant_is_refused_before_it_reaches_a_server() {
        for grant in [
            OAuthGrant::new("", "secret", "refresh"),
            OAuthGrant::new("id", "", "refresh"),
            OAuthGrant::new("id", "secret", ""),
            OAuthGrant::new("id\n", "secret", "refresh"),
            OAuthGrant::new("i".repeat(MAX_CLIENT_ID_BYTES + 1), "secret", "refresh"),
            OAuthGrant::new("id", "secret", "r".repeat(MAX_REFRESH_TOKEN_BYTES + 1)),
        ] {
            assert!(grant.validate().is_err(), "accepted {grant:?}");
        }
        assert!(OAuthGrant::new("id", "secret", "refresh").validate().is_ok());
    }

    #[test]
    fn malformed_keyring_bytes_are_a_readable_sentence() {
        for bytes in [b"".as_slice(), b"not json", b"{\"v\":2}", b"{\"v\":1,\"client_id\":\"\",\"client_secret\":\"s\",\"refresh_token\":\"r\"}"] {
            let error = OAuthGrant::from_bytes(bytes).expect_err("must refuse");
            assert!(error.message().contains("OAuth"), "{}", error.message());
            assert!(!error.message().contains("secret"), "{}", error.message());
        }
        let oversized = vec![b'x'; MAX_REFRESH_TOKEN_BYTES + MAX_CLIENT_SECRET_BYTES + MAX_CLIENT_ID_BYTES + 129];
        assert!(OAuthGrant::from_bytes(&oversized).is_err());
    }

    #[test]
    fn a_cached_token_is_fresh_only_outside_the_refresh_skew() {
        let skew_ms = i64::try_from(REFRESH_SKEW.as_millis()).expect("skew");
        let access = CachedAccess { text: Zeroizing::new("token".to_owned()), expires_at_ms: 1_000_000 };
        assert!(access.is_fresh(1_000_000 - skew_ms - 1));
        assert!(!access.is_fresh(1_000_000 - skew_ms));
        assert!(!access.is_fresh(1_000_001));
        let _ = Credential::Password(Zeroizing::new("p".to_owned()));
        assert!(!Credential::Password(Zeroizing::new("p".to_owned())).is_oauth());
    }
}
