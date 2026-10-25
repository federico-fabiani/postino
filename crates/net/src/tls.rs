use std::sync::Arc;

use rustls::ClientConfig;
use rustls::pki_types::CertificateDer;

use crate::NetError;
use crate::error::sanitize_reason;

/// Build a client configuration that verifies certificates with the operating system.
///
/// The verifier is `rustls-platform-verifier`: on Windows it is CryptoAPI, on macOS the Security
/// framework, on Linux the system trust store through WebPKI. `extra_roots` adds trust anchors for
/// tests; with an empty slice the platform verifier is used unchanged. There is no configuration
/// that skips verification, and the error carries the host and the verifier's own words, never the
/// certificate.
pub(crate) fn client_config(extra_roots: &[CertificateDer<'static>], host: &str) -> Result<Arc<ClientConfig>, NetError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(extra_roots.iter().cloned(), provider.clone())
        .map_err(|error| NetError::Tls { host: host.to_owned(), reason: sanitize_reason(&error.to_string()) })?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| NetError::Tls { host: host.to_owned(), reason: sanitize_reason(&error.to_string()) })?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}
