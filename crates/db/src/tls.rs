//! Shared rustls client configuration: the `ring` provider and the platform's root
//! certificates, with verification always on.

use std::sync::Arc;

use crate::error::{DbError, Result};

/// A rustls client config that verifies servers against the OS trust store.
pub fn client_config() -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(loaded.certs);
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| DbError::Tls(e.to_string()))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}
