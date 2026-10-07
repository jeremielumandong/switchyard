//! TLS for outbound API requests: rustls with the `ring` provider and the OS trust store.

use std::sync::Arc;

/// A client config that verifies servers against the OS trust store.
pub(crate) fn client_config() -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    let found = rustls_native_certs::load_native_certs();
    for cert in found.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err("no trusted root certificates were found on this system".into());
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}
