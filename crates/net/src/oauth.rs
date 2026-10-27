//! OAuth 2.0 authorization-code flow with PKCE, for SASL XOAUTH2 mail authentication.
//!
//! The flow has four bounded steps and this module owns all four:
//!
//! 1. [`AuthRequest::start`] builds the authorization URL with a fresh verifier and `state`.
//! 2. [`Loopback::bind`] listens on `127.0.0.1:0`; the browser comes back to that exact port.
//! 3. [`Loopback::wait`] accepts one GET, checks `state`, and yields the code or a typed refusal.
//! 4. [`TokenEndpoint`] exchanges the code for tokens over a bounded HTTP/1.1 POST inside TLS.
//!
//! Everything is bounded: the callback request, the token body, the token length and the time spent
//! waiting. Nothing is logged, and a token lives in a [`Secret`] that zeroises itself and prints
//! `[redacted]`.
//!
//! The provider seam is [`OAuthProvider`]: a table of endpoints, a scope and the extra
//! authorization parameters. Gmail is the only built-in entry; a test or a future provider supplies
//! its own. There is no client secret in this crate and no shared application identity.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, lookup_host};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_rustls::TlsConnector;
use zeroize::Zeroizing;

use crate::NetError;
use crate::error::sanitize_reason;
use crate::secret::Secret;
use crate::tls;

/// Longest access or refresh token accepted from a token endpoint, in bytes.
pub const MAX_TOKEN_BYTES: usize = 4096;
/// Longest token-endpoint response body accepted, in bytes.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Longest callback request line accepted by the loopback listener, in bytes.
pub const MAX_CALLBACK_BYTES: usize = 8 * 1024;
/// Longest authorization URL this module will build or open, in bytes.
pub const MAX_URL_BYTES: usize = 8 * 1024;
/// Access tokens are refreshed this long before they expire.
pub const REFRESH_SKEW: Duration = Duration::from_secs(60);
/// Default budget for one token request and for the consent window.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// One OAuth 2.0 provider: where to send the browser, where to exchange the code, what to ask for.
///
/// The struct is data, not a trait: a provider is a table of endpoints and parameters, and adding
/// one must not add a code path. [`OAuthProvider::gmail`] is the only built-in entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProvider {
    /// Short stable name, `snake_case`, safe for a log line.
    pub name: &'static str,
    /// Authorization endpoint the system browser opens.
    pub authorize_endpoint: &'static str,
    /// Token endpoint for the code exchange and every refresh.
    pub token_endpoint: &'static str,
    /// Revocation endpoint, called when an account is removed.
    pub revoke_endpoint: &'static str,
    /// Space-separated scope list.
    pub scope: &'static str,
    /// Extra query parameters the provider requires on the authorization URL.
    pub authorize_params: &'static [(&'static str, &'static str)],
}

impl OAuthProvider {
    /// Gmail over IMAP and SMTP, scope `https://mail.google.com/`.
    ///
    /// The extra parameters are Google's: `access_type=offline` asks for a refresh token and
    /// `prompt=consent` makes it come back even on a repeat authorization.
    #[must_use]
    pub const fn gmail() -> Self {
        Self {
            name: "gmail",
            authorize_endpoint: "https://accounts.google.com/o/oauth2/v2/auth",
            token_endpoint: "https://oauth2.googleapis.com/token",
            revoke_endpoint: "https://oauth2.googleapis.com/revoke",
            scope: "https://mail.google.com/",
            authorize_params: &[("access_type", "offline"), ("prompt", "consent")],
        }
    }

    /// Look a provider up by the name the operation registry uses.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "gmail" => Some(Self::gmail()),
            _ => None,
        }
    }

    /// Point the provider at other endpoints. Tests and self-hosted providers only: production
    /// uses the built-in table, and the token endpoint must still be `https`.
    #[must_use]
    pub fn with_endpoints(mut self, authorize: &'static str, token: &'static str, revoke: &'static str) -> Self {
        self.authorize_endpoint = authorize;
        self.token_endpoint = token;
        self.revoke_endpoint = revoke;
        self
    }
}

/// A PKCE verifier and its S256 challenge (RFC 7636).
///
/// The verifier is 43-128 unreserved ASCII characters; it is generated from 32 random bytes and
/// never printed.
#[derive(Clone)]
pub struct Pkce {
    verifier: Zeroizing<String>,
    challenge: String,
}

impl Pkce {
    /// Generate a verifier from 32 random bytes.
    pub fn generate() -> Result<Self, NetError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| random_error())?;
        Self::from_verifier(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Rebuild a pair from a known verifier. Used by tests and by anyone resuming a stored flow.
    pub fn from_verifier(verifier: impl Into<String>) -> Result<Self, NetError> {
        let verifier = Zeroizing::new(verifier.into());
        if verifier.len() < 43 || verifier.len() > 128 || !verifier.bytes().all(is_unreserved) {
            return Err(NetError::OAuthConsent { reason: "the PKCE verifier must be 43-128 unreserved ASCII characters".to_owned() });
        }
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Ok(Self { verifier, challenge })
    }

    /// The verifier sent with the code exchange.
    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The S256 challenge sent with the authorization request.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Pkce").field("verifier", &"[redacted]").field("challenge", &self.challenge).finish()
    }
}

/// One authorization request: the URL to open, and the secrets needed to finish the flow.
pub struct AuthRequest {
    /// The URL the system browser must open. Contains no secret.
    pub url: String,
    pkce: Pkce,
    state: Zeroizing<String>,
}

impl AuthRequest {
    /// Build an authorization URL for `provider` and a loopback `redirect_uri`.
    ///
    /// The redirect URI must be plain `http` on `127.0.0.1` with an explicit port: a loopback
    /// listener is the only redirect this crate accepts, so a code can never be delivered to a
    /// remote host.
    pub fn start(provider: &OAuthProvider, client_id: &str, redirect_uri: &str) -> Result<Self, NetError> {
        validate_client_id(client_id)?;
        validate_redirect_uri(redirect_uri)?;
        let mut state_bytes = [0u8; 32];
        getrandom::fill(&mut state_bytes).map_err(|_| random_error())?;
        let pkce = Pkce::generate()?;
        let state = Zeroizing::new(URL_SAFE_NO_PAD.encode(state_bytes));
        let url = authorize_url(provider, client_id, redirect_uri, &state, pkce.challenge())?;
        Ok(Self { url, pkce, state })
    }

    /// The `state` the callback must echo. Never log it.
    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    /// The PKCE pair.
    #[must_use]
    pub fn pkce(&self) -> &Pkce {
        &self.pkce
    }
}

impl fmt::Debug for AuthRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AuthRequest").field("url", &self.url).field("state", &"[redacted]").field("pkce", &self.pkce).finish()
    }
}

/// The authorization URL, built from its parts. Separated so a test can pin the exact shape.
fn authorize_url(provider: &OAuthProvider, client_id: &str, redirect_uri: &str, state: &str, challenge: &str) -> Result<String, NetError> {
    let mut url = String::with_capacity(512);
    url.push_str(provider.authorize_endpoint);
    url.push('?');
    let mut separator = "";
    for (key, value) in [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", provider.scope),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ] {
        url.push_str(separator);
        url.push_str(key);
        url.push('=');
        url.push_str(&encode_component(value));
        separator = "&";
    }
    for (key, value) in provider.authorize_params {
        url.push('&');
        url.push_str(key);
        url.push('=');
        url.push_str(&encode_component(value));
    }
    if url.len() > MAX_URL_BYTES {
        return Err(NetError::TooLarge { what: "authorization URL", limit: MAX_URL_BYTES });
    }
    Ok(url)
}

/// A loopback listener for the authorization redirect.
pub struct Loopback {
    listener: TcpListener,
    redirect_uri: String,
}

impl Loopback {
    /// Bind `127.0.0.1:0` and learn the port the browser must come back to.
    pub async fn bind() -> Result<Self, NetError> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|error| NetError::Connect {
            host: "127.0.0.1".to_owned(),
            port: 0,
            reason: sanitize_reason(&error.to_string()),
        })?;
        let port = listener
            .local_addr()
            .map_err(|error| NetError::Connect { host: "127.0.0.1".to_owned(), port: 0, reason: sanitize_reason(&error.to_string()) })?
            .port();
        Ok(Self { listener, redirect_uri: format!("http://127.0.0.1:{port}") })
    }

    /// The redirect URI to pass to [`AuthRequest::start`].
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for the single redirect, validate `state`, and return the code.
    ///
    /// The wait is bounded by `budget`. `cancelled` is checked between accept attempts, so a user
    /// pressing Cancel ends the wait within one poll interval instead of at the timeout. The
    /// listener is consumed: a second redirect can never be served.
    pub async fn wait(self, request: &AuthRequest, budget: Duration, cancelled: &AtomicBool) -> Result<Zeroizing<String>, NetError> {
        if budget.is_zero() {
            return Err(NetError::Timeout { phase: "oauth callback", after: budget });
        }
        let deadline = Instant::now() + budget;
        let stream = loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(NetError::OAuthConsent { reason: "the sign-in was cancelled".to_owned() });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(NetError::Timeout { phase: "oauth callback", after: budget });
            }
            let poll = remaining.min(Duration::from_millis(100));
            match timeout_at(deadline, timeout(poll, self.listener.accept())).await {
                Ok(Ok(Ok((stream, _)))) => break stream,
                Ok(Ok(Err(error))) => {
                    return Err(NetError::OAuthConsent { reason: format!("the loopback listener failed: {}", sanitize_reason(&error.to_string())) });
                }
                Ok(Err(_)) | Err(_) => continue,
            }
        };
        let (query, mut stream) = read_callback(stream).await?;
        let code = callback_code(&query, request);
        // The success page is served only after the state and the code have been validated: a
        // forged or replayed redirect must never be told that access was completed.
        respond(&mut stream, if code.is_ok() { 200 } else { 400 }).await;
        code
    }
}

/// Validate one callback query against the request and return the code. No response is written here;
/// the caller answers the browser according to the result.
fn callback_code(query: &str, request: &AuthRequest) -> Result<Zeroizing<String>, NetError> {
    let params = parse_query(query)?;
    if let Some(error) = params.get("error") {
        let reason = match error.as_str() {
            "access_denied" => "the person declined the authorization".to_owned(),
            other => format!("the provider refused the authorization ({})", sanitize_reason(other)),
        };
        return Err(NetError::OAuthConsent { reason });
    }
    let state = params.get("state").ok_or_else(|| NetError::OAuthConsent { reason: "the redirect carried no state".to_owned() })?;
    if !constant_time_eq(state.as_bytes(), request.state().as_bytes()) {
        return Err(NetError::OAuthConsent { reason: "the redirect did not carry the state this request issued".to_owned() });
    }
    let code = params.get("code").ok_or_else(|| NetError::OAuthConsent { reason: "the redirect carried no authorization code".to_owned() })?;
    if code.is_empty() || code.len() > MAX_TOKEN_BYTES || code.chars().any(char::is_control) {
        return Err(NetError::OAuthConsent { reason: "the authorization code is not usable".to_owned() });
    }
    Ok(Zeroizing::new(code.clone()))
}

impl fmt::Debug for Loopback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Loopback").field("redirect_uri", &self.redirect_uri).finish()
    }
}

/// Read one bounded HTTP request and answer a malformed request. The page never echoes the query:
/// a code or an error description must not end up in a browser history entry we wrote.
async fn read_callback(mut stream: TcpStream) -> Result<(String, TcpStream), NetError> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() >= MAX_CALLBACK_BYTES {
            return Err(NetError::TooLarge { what: "callback request", limit: MAX_CALLBACK_BYTES });
        }
        match timeout(Duration::from_secs(10), stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(read)) => {
                let room = MAX_CALLBACK_BYTES.saturating_sub(buffer.len());
                buffer.extend_from_slice(chunk.get(..read.min(room)).unwrap_or_default());
            }
            Ok(Err(error)) => {
                return Err(NetError::OAuthConsent { reason: format!("the redirect could not be read: {}", sanitize_reason(&error.to_string())) });
            }
            Err(_) => return Err(NetError::Timeout { phase: "oauth callback", after: Duration::from_secs(10) }),
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let request_line = text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method != "GET" || !version.starts_with("HTTP/1.") {
        respond(&mut stream, 400).await;
        return Err(NetError::OAuthConsent { reason: "the redirect was not a plain GET".to_owned() });
    }
    if target.len() > MAX_URL_BYTES {
        respond(&mut stream, 414).await;
        return Err(NetError::TooLarge { what: "callback target", limit: MAX_URL_BYTES });
    }
    let query = target.split_once('?').map_or("", |(_, query)| query);
    let query = query.split('#').next().unwrap_or_default().to_owned();
    Ok((query, stream))
}

/// The page the loopback listener serves after a validated callback. Static text, no interpolation.
const PAGE: &str =
    "<!doctype html><meta charset=\"utf-8\"><title>Postino</title><p>Accesso completato. Puoi chiudere questa scheda e tornare a Postino.</p>";

/// The page for anything that was not a valid callback. It never confirms access.
const PAGE_FAILED: &str =
    "<!doctype html><meta charset=\"utf-8\"><title>Postino</title><p>Autorizzazione non completata. Torna a Postino e riprova.</p>";

async fn respond(stream: &mut TcpStream, status: u16) {
    let (reason, page) = if status == 200 { ("OK", PAGE) } else { ("Bad Request", PAGE_FAILED) };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// Query parameters of a loopback redirect, bounded.
fn parse_query(query: &str) -> Result<std::collections::BTreeMap<String, String>, NetError> {
    let mut params = std::collections::BTreeMap::new();
    if query.is_empty() {
        return Ok(params);
    }
    if query.split('&').count() > 32 {
        return Err(NetError::OAuthConsent { reason: "the redirect carried too many parameters".to_owned() });
    }
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if params.len() >= 32 {
            break;
        }
        params.insert(decode_component(key)?, decode_component(value)?);
    }
    Ok(params)
}

/// A token-endpoint client for one account: client identity, endpoints, trust and timeout.
///
/// `Debug` prints the provider and the client id but never the client secret.
pub struct TokenEndpoint {
    provider: OAuthProvider,
    client_id: String,
    client_secret: Zeroizing<String>,
    extra_roots: Vec<crate::CertificateDer<'static>>,
    timeout: Duration,
}

impl TokenEndpoint {
    /// Build a client for `provider`. The secret is a BYO value copied into a zeroising buffer.
    pub fn new(provider: OAuthProvider, client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            provider,
            client_id: client_id.into(),
            client_secret: Zeroizing::new(client_secret.into()),
            extra_roots: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Trust one more certificate authority. Tests only: production trusts the OS store.
    #[must_use]
    pub fn with_extra_root(mut self, root: crate::CertificateDer<'static>) -> Self {
        self.extra_roots.push(root);
        self
    }

    /// Trust several more certificate authorities. Tests only.
    #[must_use]
    pub fn with_extra_roots(mut self, roots: Vec<crate::CertificateDer<'static>>) -> Self {
        self.extra_roots.extend(roots);
        self
    }

    /// Override the request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Exchange an authorization code for tokens.
    pub async fn exchange(&self, code: &str, redirect_uri: &str, verifier: &str) -> Result<TokenSet, NetError> {
        validate_client_id(&self.client_id)?;
        if code.is_empty() {
            return Err(NetError::OAuthConsent { reason: "the authorization code is empty".to_owned() });
        }
        let form = form(&[
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ]);
        let body = self.post(self.provider.token_endpoint, &form).await?;
        parse_tokens(&body)
    }

    /// Refresh an access token from a stored refresh token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenSet, NetError> {
        validate_client_id(&self.client_id)?;
        if refresh_token.is_empty() {
            return Err(NetError::OAuthGrant { reason: "no refresh token is stored for this account".to_owned() });
        }
        let form = form(&[
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ]);
        let body = self.post(self.provider.token_endpoint, &form).await?;
        parse_tokens(&body)
    }

    /// Revoke a refresh token at the provider. `Ok(())` when the provider answers `200`.
    pub async fn revoke(&self, token: &str) -> Result<(), NetError> {
        if token.is_empty() {
            return Ok(());
        }
        let form = form(&[("client_id", &self.client_id), ("client_secret", &self.client_secret), ("token", token)]);
        let body = self.post(self.provider.revoke_endpoint, &form).await?;
        drop(body);
        Ok(())
    }

    /// Blocking [`TokenEndpoint::exchange`], for the CLI and the engine.
    pub fn exchange_blocking(&self, code: &str, redirect_uri: &str, verifier: &str) -> Result<TokenSet, NetError> {
        block_on(self.exchange(code, redirect_uri, verifier))
    }

    /// Blocking [`TokenEndpoint::refresh`], for the CLI and the engine.
    pub fn refresh_blocking(&self, refresh_token: &str) -> Result<TokenSet, NetError> {
        block_on(self.refresh(refresh_token))
    }

    /// Blocking [`TokenEndpoint::revoke`], for account removal.
    pub fn revoke_blocking(&self, token: &str) -> Result<(), NetError> {
        block_on(self.revoke(token))
    }

    /// One bounded HTTP/1.1 POST inside TLS. Never logs the form.
    async fn post(&self, endpoint: &str, form: &Zeroizing<String>) -> Result<Vec<u8>, NetError> {
        let form = Zeroizing::new(form.to_string());
        let target = HttpsTarget::parse(endpoint)?;
        let mut last_error = None;
        let mut stream = None;
        for address in lookup_host((target.host.as_str(), target.port))
            .await
            .map_err(|error| NetError::Dns { host: target.host.clone(), reason: sanitize_reason(&error.to_string()) })?
        {
            match timeout(self.timeout, TcpStream::connect(address)).await {
                Ok(Ok(candidate)) => {
                    stream = Some(candidate);
                    break;
                }
                Ok(Err(error)) => last_error = Some(error.to_string()),
                Err(_) => return Err(NetError::Timeout { phase: "oauth connect", after: self.timeout }),
            }
        }
        let stream = stream.ok_or_else(|| NetError::Connect {
            host: target.host.clone(),
            port: target.port,
            reason: sanitize_reason(last_error.as_deref().unwrap_or("no address")),
        })?;
        let _ = stream.set_nodelay(true);
        let config = tls::client_config(&self.extra_roots, &target.host)?;
        let name = ServerName::try_from(target.host.clone())
            .map_err(|_| NetError::Tls { host: target.host.clone(), reason: "host is not a valid DNS name".to_owned() })?;
        let mut stream = match timeout(self.timeout, TlsConnector::from(config).connect(name, stream)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                return Err(NetError::Tls { host: target.host.clone(), reason: sanitize_reason(&error.to_string()) });
            }
            Err(_) => return Err(NetError::Timeout { phase: "oauth tls", after: self.timeout }),
        };
        let request = Zeroizing::new(format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-www-form-urlencoded\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            target.path,
            target.authority,
            form.len(),
            form.as_str()
        ));
        match timeout(self.timeout, stream.write_all(request.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(protocol_io("oauth request", &error)),
            Err(_) => return Err(NetError::Timeout { phase: "oauth request", after: self.timeout }),
        }
        let mut response = Vec::with_capacity(2048);
        let mut chunk = [0u8; 4096];
        loop {
            match timeout(self.timeout, stream.read(&mut chunk)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(read)) => {
                    let room = MAX_RESPONSE_BYTES.saturating_sub(response.len());
                    if room == 0 {
                        return Err(NetError::TooLarge { what: "token response", limit: MAX_RESPONSE_BYTES });
                    }
                    response.extend_from_slice(chunk.get(..read.min(room)).unwrap_or_default());
                }
                Ok(Err(error)) => return Err(protocol_io("oauth response", &error)),
                Err(_) => return Err(NetError::Timeout { phase: "oauth response", after: self.timeout }),
            }
        }
        parse_http_response(&response)
    }
}

impl fmt::Debug for TokenEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenEndpoint")
            .field("provider", &self.provider.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("extra_roots", &self.extra_roots.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// One authorization-code flow: loopback listener, PKCE, state, system browser, code exchange.
///
/// The browser is a callback, not a dependency: the CLI and the desktop both pass
/// `postino_platform::open_in_browser`. Nothing here embeds a webview or starts a shell, and the
/// function is synchronous so a caller without a runtime (the engine, the onboarding probe thread)
/// can run it and receive a finished [`TokenSet`].
pub struct Authorization {
    provider: OAuthProvider,
    client_id: String,
    client_secret: Zeroizing<String>,
    extra_roots: Vec<crate::CertificateDer<'static>>,
    timeout: Duration,
}

impl Authorization {
    /// Build one flow for `provider`. `client_secret` is a BYO value kept zeroising.
    pub fn new(provider: OAuthProvider, client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            provider,
            client_id: client_id.into(),
            client_secret: Zeroizing::new(client_secret.into()),
            extra_roots: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Trust one more certificate authority for the token endpoint. Tests only.
    #[must_use]
    pub fn with_extra_root(mut self, root: crate::CertificateDer<'static>) -> Self {
        self.extra_roots.push(root);
        self
    }

    /// Bound the whole flow: the wait for the callback and the token exchange.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run the flow. `open` hands the authorization URL to the browser and reports why it failed;
    /// `cancelled` stops the wait for the callback without waiting out the budget.
    ///
    /// The listener is bound *before* the URL is built, so the redirect URI and the open port always
    /// agree, and the browser is opened only when a listener is actually waiting for the code.
    pub fn run(self, open: impl FnOnce(&str) -> Result<(), String>, cancelled: &AtomicBool) -> Result<TokenSet, NetError> {
        let Self { provider, client_id, client_secret, extra_roots, timeout } = self;
        block_on(async move {
            let loopback = Loopback::bind().await?;
            let redirect = loopback.redirect_uri().to_owned();
            let request = AuthRequest::start(&provider, &client_id, &redirect)?;
            open(&request.url).map_err(|reason| NetError::OAuthConsent { reason: sanitize_reason(&reason) })?;
            let code = loopback.wait(&request, timeout, cancelled).await?;
            let endpoint = TokenEndpoint::new(provider, client_id, client_secret.as_str()).with_timeout(timeout).with_extra_roots(extra_roots);
            endpoint.exchange(&code, &redirect, request.pkce().verifier()).await
        })
    }
}

impl fmt::Debug for Authorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Authorization")
            .field("provider", &self.provider.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("extra_roots", &self.extra_roots.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Tokens returned by an authorization or refresh.
#[derive(Clone)]
pub struct TokenSet {
    access: Secret,
    refresh: Option<Secret>,
    expires_in: Duration,
}

impl TokenSet {
    /// The access token used as the XOAUTH2 bearer credential.
    #[must_use]
    pub fn access_token(&self) -> &Secret {
        &self.access
    }

    /// A new refresh token, when the provider rotated one.
    #[must_use]
    pub fn refresh_token(&self) -> Option<&Secret> {
        self.refresh.as_ref()
    }

    /// Lifetime of the access token as reported by the provider.
    #[must_use]
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &self.refresh.is_some())
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// The SASL XOAUTH2 payload for IMAP `AUTHENTICATE` and SMTP `AUTH`.
///
/// The intermediate string holds the bearer token, so it is zeroised: the returned base64 line is
/// the only copy the wire sees.
pub(crate) fn sasl_xoauth2(user: &str, token: &str) -> Zeroizing<String> {
    let plain = Zeroizing::new(format!("user={user}\x01auth=Bearer {token}\x01\x01"));
    Zeroizing::new(STANDARD.encode(plain.as_bytes()))
}

fn protocol_io(phase: &'static str, error: &std::io::Error) -> NetError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    ) {
        return NetError::Closed { phase };
    }
    NetError::OAuthToken { reason: format!("the token request failed: {}", sanitize_reason(&error.to_string())) }
}

fn protocol_target(value: &str) -> NetError {
    NetError::OAuthToken { reason: format!("the token endpoint URL is not usable: {}", sanitize_reason(value)) }
}

/// An `https://host[:port]/path` target, split into the parts an HTTP/1.1 request needs.
struct HttpsTarget {
    authority: String,
    host: String,
    port: u16,
    path: String,
}

impl HttpsTarget {
    fn parse(url: &str) -> Result<Self, NetError> {
        if url.len() > MAX_URL_BYTES {
            return Err(NetError::TooLarge { what: "token endpoint URL", limit: MAX_URL_BYTES });
        }
        let rest = url.strip_prefix("https://").ok_or_else(|| protocol_target("only https is accepted"))?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], rest.get(index..).unwrap_or("/")),
            None => (rest, "/"),
        };
        if authority.is_empty() || authority.contains('@') || authority.chars().any(|ch| ch.is_control() || ch == ' ' || ch == '?') {
            return Err(protocol_target("the authority is empty or has characters a request cannot carry"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.starts_with('[') => {
                let port = port.parse::<u16>().map_err(|_| protocol_target("the port is not a number"))?;
                (host, port)
            }
            _ => (authority, 443),
        };
        if host.is_empty() {
            return Err(protocol_target("the host is empty"));
        }
        let authority = if port == 443 { host.to_owned() } else { format!("{host}:{port}") };
        Ok(Self { authority, host: host.to_owned(), port, path: path.to_owned() })
    }
}

/// Parse a bounded HTTP/1.1 response: status line, headers, body. Statuses other than `200` become
/// a typed refusal carrying the provider's error code, never the body.
fn parse_http_response(response: &[u8]) -> Result<Vec<u8>, NetError> {
    let text = String::from_utf8_lossy(response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| NetError::OAuthToken { reason: "the token endpoint response has no header terminator".to_owned() })?;
    if head.len() > 16 * 1024 {
        return Err(NetError::TooLarge { what: "token response headers", limit: 16 * 1024 });
    }
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| NetError::OAuthToken { reason: "the token endpoint response has no status code".to_owned() })?;
    if !version.starts_with("HTTP/1.") {
        return Err(NetError::OAuthToken { reason: "the token endpoint did not answer HTTP/1.1".to_owned() });
    }
    if lines.any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:") && line.to_ascii_lowercase().contains("chunked")) {
        return Err(NetError::OAuthToken { reason: "the token endpoint used a chunked response, which this client does not read".to_owned() });
    }
    if status != 200 {
        let code = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| value.get("error").and_then(serde_json::Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| "no error code".to_owned());
        return Err(classify_error(status, &code));
    }
    Ok(body.as_bytes().to_vec())
}

/// Turn a provider error code into the sentence that reaches the person.
fn classify_error(status: u16, code: &str) -> NetError {
    let code = sanitize_reason(code);
    match code.as_str() {
        "access_denied" => NetError::OAuthConsent { reason: "the person declined the authorization".to_owned() },
        "invalid_grant" => NetError::OAuthGrant { reason: "the provider no longer accepts the stored grant; authorize the account again".to_owned() },
        "invalid_client" | "unauthorized_client" => NetError::OAuthRejected { status, code },
        _ => NetError::OAuthRejected { status, code },
    }
}

/// Parse the token JSON. Only the fields this client needs are read; everything else is ignored.
fn parse_tokens(body: &[u8]) -> Result<TokenSet, NetError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(NetError::TooLarge { what: "token response", limit: MAX_RESPONSE_BYTES });
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| NetError::OAuthToken { reason: "the token endpoint response is not valid JSON".to_owned() })?;
    let object = value.as_object().ok_or_else(|| NetError::OAuthToken { reason: "the token response is not a JSON object".to_owned() })?;
    if let Some(error) = object.get("error").and_then(serde_json::Value::as_str) {
        return Err(classify_error(400, error));
    }
    let access = object
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NetError::OAuthToken { reason: "the token response has no access token".to_owned() })?;
    validate_token(access, "access token")?;
    let refresh = match object.get("refresh_token").and_then(serde_json::Value::as_str) {
        Some(value) => {
            validate_token(value, "refresh token")?;
            Some(Secret::new(value))
        }
        None => None,
    };
    let seconds = object.get("expires_in").and_then(serde_json::Value::as_u64).unwrap_or(3600).clamp(60, 7 * 24 * 3600);
    Ok(TokenSet { access: Secret::new(access), refresh, expires_in: Duration::from_secs(seconds) })
}

fn validate_token(token: &str, what: &str) -> Result<(), NetError> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.chars().any(char::is_control) {
        return Err(NetError::OAuthToken { reason: format!("the token response has an unusable {what}") });
    }
    Ok(())
}

fn validate_client_id(client_id: &str) -> Result<(), NetError> {
    if client_id.is_empty() || client_id.len() > 256 || client_id.chars().any(char::is_control) {
        return Err(NetError::OAuthToken { reason: "the OAuth client id must be 1-256 printable characters".to_owned() });
    }
    Ok(())
}

fn validate_redirect_uri(redirect_uri: &str) -> Result<(), NetError> {
    let rest = redirect_uri
        .strip_prefix("http://127.0.0.1:")
        .ok_or_else(|| NetError::OAuthConsent { reason: "the redirect must be a loopback http://127.0.0.1:<port> URI".to_owned() })?;
    if rest.is_empty() || rest.len() > 6 || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(NetError::OAuthConsent { reason: "the loopback redirect has no usable port".to_owned() });
    }
    Ok(())
}

fn random_error() -> NetError {
    NetError::OAuthConsent { reason: "the operating system could not supply random bytes for PKCE".to_owned() }
}

/// Build an `application/x-www-form-urlencoded` body in a zeroising buffer.
fn form(pairs: &[(&str, &str)]) -> Zeroizing<String> {
    let mut body = Zeroizing::new(String::with_capacity(256));
    let mut separator = "";
    for (key, value) in pairs {
        body.push_str(separator);
        body.push_str(&encode_component(key));
        body.push('=');
        body.push_str(&encode_component(value));
        separator = "&";
    }
    body
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// RFC 3986 percent-encoding for a query component.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn decode_component(value: &str) -> Result<String, NetError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes.get(index) {
            Some(b'%') => {
                let high = bytes.get(index + 1).copied().and_then(hex_value);
                let low = bytes.get(index + 2).copied().and_then(hex_value);
                match (high, low) {
                    (Some(high), Some(low)) => out.push(high * 16 + low),
                    _ => return Err(NetError::OAuthConsent { reason: "the redirect carried a malformed escape".to_owned() }),
                }
                index += 3;
            }
            Some(b'+') => {
                out.push(b' ');
                index += 1;
            }
            Some(byte) => {
                out.push(*byte);
                index += 1;
            }
            None => break,
        }
    }
    String::from_utf8(out).map_err(|_| NetError::OAuthConsent { reason: "the redirect is not valid UTF-8".to_owned() })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Compare two byte strings without leaking the first difference through timing.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Run one token request on a private current-thread runtime.
fn block_on<F, T>(future: F) -> Result<T, NetError>
where
    F: Future<Output = Result<T, NetError>>,
{
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(future),
        Err(error) => Err(futures_refusal(&error.to_string())),
    }
}

fn futures_refusal(detail: &str) -> NetError {
    NetError::OAuthToken { reason: format!("the token request could not start: {}", sanitize_reason(detail)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc7636_example() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").expect("verifier");
        assert_eq!(pkce.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn a_generated_pkce_pair_is_usable() {
        let pkce = Pkce::generate().expect("generate");
        assert!((43..=128).contains(&pkce.verifier().len()));
        assert_eq!(pkce.challenge().len(), 43);
        assert!(pkce.verifier().bytes().all(is_unreserved));
    }

    #[test]
    fn a_bad_verifier_is_refused() {
        assert!(Pkce::from_verifier("short").is_err());
        assert!(Pkce::from_verifier("a".repeat(129)).is_err());
        assert!(Pkce::from_verifier(format!("{}!", "a".repeat(43))).is_err());
    }

    #[test]
    fn the_authorization_url_carries_every_parameter() {
        let request = AuthRequest::start(&OAuthProvider::gmail(), "client-id.apps.googleusercontent.com", "http://127.0.0.1:4567").expect("request");
        assert!(request.url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?response_type=code&"));
        assert!(request.url.contains("client_id=client-id.apps.googleusercontent.com"));
        assert!(request.url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A4567"));
        assert!(request.url.contains("scope=https%3A%2F%2Fmail.google.com%2F"));
        assert!(request.url.contains("code_challenge_method=S256"));
        assert!(request.url.contains("access_type=offline"));
        assert!(request.url.contains("prompt=consent"));
        assert!(request.url.contains(request.state()));
        assert!(!format!("{request:?}").contains(&format!("state: \"{}\"", request.state())));
        assert!(format!("{request:?}").contains("state: \"[redacted]\""));
    }

    #[test]
    fn state_is_32_random_bytes() {
        let client = OAuthProvider::gmail();
        let first = AuthRequest::start(&client, "id", "http://127.0.0.1:1").expect("first");
        let second = AuthRequest::start(&client, "id", "http://127.0.0.1:1").expect("second");
        assert_ne!(first.state(), second.state());
        assert_eq!(first.state().len(), 43);
        assert_ne!(first.pkce().verifier(), second.pkce().verifier());
    }

    #[test]
    fn a_remote_redirect_is_refused() {
        assert!(AuthRequest::start(&OAuthProvider::gmail(), "id", "https://evil.example/callback").is_err());
        assert!(AuthRequest::start(&OAuthProvider::gmail(), "id", "http://localhost:1234").is_err());
        assert!(AuthRequest::start(&OAuthProvider::gmail(), "id", "http://127.0.0.1:not-a-port").is_err());
    }

    #[test]
    fn the_xoauth2_payload_has_the_rfc_shape() {
        let payload = sasl_xoauth2("user@example.com", "ya29.token");
        assert_eq!(STANDARD.decode(payload.as_bytes()).expect("base64"), b"user=user@example.com\x01auth=Bearer ya29.token\x01\x01");
        assert!(!payload.as_str().contains("ya29.token"), "the wire form is base64, not the plaintext token");
    }

    #[test]
    fn a_token_set_never_prints_its_tokens() {
        let tokens = parse_tokens(br#"{"access_token":"ya29.secret","refresh_token":"1//refresh","expires_in":3599}"#).expect("tokens");
        let text = format!("{tokens:?}");
        assert!(!text.contains("ya29.secret"), "{text}");
        assert!(!text.contains("1//refresh"), "{text}");
        assert_eq!(tokens.expires_in(), Duration::from_secs(3599));
        assert_eq!(tokens.access_token().expose(), "ya29.secret");
        assert_eq!(tokens.refresh_token().map(Secret::expose), Some("1//refresh"));
    }

    #[test]
    fn malformed_and_oversized_token_responses_are_typed() {
        assert!(matches!(parse_tokens(b"not json"), Err(NetError::OAuthToken { .. })));
        assert!(matches!(parse_tokens(br#"{"refresh_token":"x"}"#), Err(NetError::OAuthToken { .. })));
        assert!(matches!(parse_tokens(&vec![b'x'; MAX_RESPONSE_BYTES + 1]), Err(NetError::TooLarge { .. })));
        let long = format!(r#"{{"access_token":"{}"}}"#, "a".repeat(MAX_TOKEN_BYTES + 1));
        assert!(matches!(parse_tokens(long.as_bytes()), Err(NetError::OAuthToken { .. })));
    }

    #[test]
    fn provider_errors_are_readable_and_never_echo_the_body() {
        let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 30\r\n\r\n{\"error\":\"invalid_grant\"}   ";
        let error = parse_http_response(response).expect_err("refused");
        assert!(matches!(error, NetError::OAuthGrant { .. }), "{error:?}");
        let text = error.to_string();
        assert!(text.contains("authorize the account again"), "{text}");
        let denied = parse_http_response(b"HTTP/1.1 403 Forbidden\r\n\r\n{\"error\":\"access_denied\"}").expect_err("denied");
        assert!(matches!(denied, NetError::OAuthConsent { .. }));
    }

    #[test]
    fn an_endpoint_must_be_https() {
        assert!(HttpsTarget::parse("http://127.0.0.1:1/token").is_err());
        assert!(HttpsTarget::parse("https://user@host/token").is_err());
        let target = HttpsTarget::parse("https://localhost:8443/token?x=1").expect("target");
        assert_eq!(target.host, "localhost");
        assert_eq!(target.port, 8443);
        assert_eq!(target.authority, "localhost:8443");
        assert_eq!(target.path, "/token?x=1");
        assert_eq!(HttpsTarget::parse("https://example.com").expect("bare").port, 443);
    }

    #[test]
    fn query_parsing_is_bounded_and_decodes() {
        let params = parse_query("code=a%2Fb&state=c%2Bd&plain=a+b").expect("query");
        assert_eq!(params.get("code").map(String::as_str), Some("a/b"));
        assert_eq!(params.get("state").map(String::as_str), Some("c+d"));
        assert_eq!(params.get("plain").map(String::as_str), Some("a b"));
        assert!(parse_query("%zz").is_err());
    }

    #[test]
    fn the_client_secret_is_redacted_in_debug() {
        let endpoint = TokenEndpoint::new(OAuthProvider::gmail(), "client", "very-secret");
        let text = format!("{endpoint:?}");
        assert!(!text.contains("very-secret"), "{text}");
        assert!(text.contains("[redacted]"));
        assert!(text.contains("gmail"));
    }
}
