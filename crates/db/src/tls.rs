//! Shared rustls client configuration: the `ring` provider and the platform's root
//! certificates, with verification always on.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject as _;

use crate::error::{DbError, Result};

/// A rustls client config that verifies servers against the OS trust store.
pub fn client_config() -> Result<rustls::ClientConfig> {
    client_config_with(None)
}

/// Like [`client_config`], also trusting the certificates in `extra_pem` (a company CA or a
/// pinned server certificate).
pub fn client_config_with(extra_pem: Option<&str>) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(loaded.certs);
    if let Some(pem) = extra_pem {
        let certs = rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| DbError::Tls(format!("the trusted certificate is not valid PEM: {e}")))?;
        if certs.is_empty() {
            return Err(DbError::Tls(
                "the trusted certificate file holds no certificate".into(),
            ));
        }
        roots.add_parsable_certificates(certs);
    }
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| DbError::Tls(e.to_string()))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}

/// Make `ring` the process-wide rustls provider. Libraries that build their own TLS
/// configuration (tiberius) then use it too; otherwise they fall back to aws-lc-rs.
pub fn install_default_provider() {
    // Fails only when a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
