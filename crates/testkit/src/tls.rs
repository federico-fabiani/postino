//! A throwaway certificate authority and the server half of a TLS configuration.

use std::sync::Arc;

use postino_net::CertificateDer;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::TlsAcceptor;

/// A throwaway CA and the server half of a TLS configuration for `localhost`.
///
/// The client trusts [`TlsFixture::ca`] only through `ImapConfig::extra_roots`; verification is the
/// operating system's verifier, exactly as in production, and there is no bypass.
pub struct TlsFixture {
    acceptor: TlsAcceptor,
    ca: CertificateDer<'static>,
}

impl TlsFixture {
    /// Generate a CA and a leaf certificate for `localhost`.
    #[must_use]
    pub fn generate() -> Self {
        let ca_key = KeyPair::generate().expect("CA key");
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(DnType::CommonName, "Postino P5b test CA");
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        let ca = CertifiedIssuer::self_signed(ca_params, ca_key).expect("CA certificate");

        let leaf_key = KeyPair::generate().expect("leaf key");
        let mut leaf_params = CertificateParams::new(vec!["localhost".to_owned()]).expect("leaf parameters");
        leaf_params.distinguished_name.push(DnType::CommonName, "localhost");
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = leaf_params.signed_by(&leaf_key, &ca).expect("leaf certificate");

        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![leaf.der().clone()], key)
            .expect("server certificate");
        Self { acceptor: TlsAcceptor::from(Arc::new(config)), ca: ca.der().clone() }
    }

    /// The CA certificate to trust on the client side.
    #[must_use]
    pub fn ca(&self) -> CertificateDer<'static> {
        self.ca.clone()
    }

    /// The server half of the handshake, for [`crate::Conn::accept_tls`] and `start_tls`.
    #[must_use]
    pub fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }
}
