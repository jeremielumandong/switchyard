//! The RDP server's TLS certificate: what it is, and whether to trust it.
//!
//! RDP servers mostly present a self-signed certificate, so a CA check alone would refuse
//! nearly every server. The TLS handshake records the certificate and whether the OS
//! store vouches for it (the handshake signature is always checked), and nothing is sent
//! until [`check`] decides:
//!
//! - a pinned fingerprint that matches: connect;
//! - no pin, and the OS store trusts the certificate for this name: connect;
//! - no pin, untrusted: ask the user (trust once, or trust and pin it to this server);
//! - a pin that does not match: refuse (like a changed SSH host key). The user removes the
//!   pin first, then gets the question again.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// The certificate a server presented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedCert {
    /// SHA-256 of the certificate (DER), as `AB:CD:…`.
    pub fingerprint: String,
    /// The OS trust store vouches for it for this server name.
    pub trusted: bool,
    /// Why the OS store does not (shown with the question).
    pub problem: Option<String>,
}

/// What to do with a presented certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertCheck {
    /// Connect.
    Accept,
    /// Ask the user.
    Ask,
    /// The certificate differs from the pinned one: refuse.
    Changed {
        /// The pinned fingerprint.
        pinned: String,
    },
}

/// The user's answer to an untrusted certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertDecision {
    /// Connect this time only.
    TrustOnce,
    /// Connect and pin the certificate to this server.
    TrustAndPin,
    /// Do not connect.
    Reject,
}

/// The question put to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertRequest {
    /// The server name the certificate was checked against.
    pub server: String,
    /// The connection's display name.
    pub connection: String,
    /// The certificate.
    pub cert: PresentedCert,
}

/// Decide about `cert` given the connection's pinned fingerprint.
pub fn check(pin: Option<&str>, cert: &PresentedCert) -> CertCheck {
    match pin.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) if same_fingerprint(p, &cert.fingerprint) => CertCheck::Accept,
        Some(p) => CertCheck::Changed {
            pinned: p.to_owned(),
        },
        None if cert.trusted => CertCheck::Accept,
        None => CertCheck::Ask,
    }
}

/// What the user's answer means: connect or not, and the fingerprint to pin.
pub fn apply(decision: CertDecision, cert: &PresentedCert) -> (bool, Option<String>) {
    match decision {
        CertDecision::TrustOnce => (true, None),
        CertDecision::TrustAndPin => (true, Some(cert.fingerprint.clone())),
        CertDecision::Reject => (false, None),
    }
}

/// Two fingerprints are the same whatever the case and separators.
pub fn same_fingerprint(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(char::is_ascii_hexdigit)
            .map(|c| c.to_ascii_uppercase())
            .collect::<String>()
    };
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && a == b
}

/// SHA-256 of `der` as `AB:CD:…`.
pub fn fingerprint(der: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, der);
    d.as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// One DER element: (tag, contents, rest).
fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, mut rest) = rest.split_first()?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let (bytes, r) = rest.split_at(n);
        rest = r;
        bytes
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | usize::from(*b))
    };
    if rest.len() < len {
        return None;
    }
    let (body, rest) = rest.split_at(len);
    Some((tag, body, rest))
}

/// The subject public key (the BIT STRING's bytes) of an X.509 certificate: CredSSP
/// binds the authentication to it.
pub fn subject_public_key(cert_der: &[u8]) -> Option<Vec<u8>> {
    const SEQUENCE: u8 = 0x30;
    const BIT_STRING: u8 = 0x03;
    const VERSION: u8 = 0xA0;
    let (tag, cert, _) = tlv(cert_der)?;
    if tag != SEQUENCE {
        return None;
    }
    let (tag, tbs, _) = tlv(cert)?;
    if tag != SEQUENCE {
        return None;
    }
    let mut rest = tbs;
    // [0] version (optional), serial, signature algorithm, issuer, validity, subject.
    let (tag, _, r) = tlv(rest)?;
    rest = if tag == VERSION { tlv(r)?.2 } else { r };
    for _ in 0..4 {
        rest = tlv(rest)?.2;
    }
    let (tag, spki, _) = tlv(rest)?;
    if tag != SEQUENCE {
        return None;
    }
    let (_, _, after_alg) = tlv(spki)?;
    let (tag, bits, _) = tlv(after_alg)?;
    if tag != BIT_STRING {
        return None;
    }
    // The first byte counts unused bits; keys are whole bytes.
    match bits.split_first() {
        Some((0, key)) if !key.is_empty() => Some(key.to_vec()),
        _ => None,
    }
}

/// What the handshake saw: the end-entity certificate and the OS store's verdict.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// Certificate (DER).
    pub der: Vec<u8>,
    /// `Ok` when the OS store trusts it for the server name, else why not.
    pub verdict: Option<Result<(), String>>,
}

/// A verifier that records the certificate and the OS store's verdict and lets the
/// handshake finish, so the decision can be made (and the user asked) before anything
/// is sent. Handshake signatures are verified as usual: the server must hold the key.
#[derive(Debug)]
pub struct CapturingVerifier {
    webpki: Option<Arc<rustls::client::WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
    seen: Mutex<Seen>,
}

impl CapturingVerifier {
    /// A verifier over the OS trust store.
    pub fn new(provider: Arc<CryptoProvider>) -> Arc<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .ok();
        Arc::new(Self {
            webpki,
            provider,
            seen: Mutex::default(),
        })
    }

    /// What the handshake saw.
    pub fn seen(&self) -> Seen {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verdict = match &self.webpki {
            Some(v) => v
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            None => Err("no trusted certificate authorities on this computer".into()),
        };
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Seen {
            der: end_entity.as_ref().to_vec(),
            verdict: Some(verdict),
        };
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The presented certificate from what the handshake saw.
pub fn presented(seen: &Seen) -> PresentedCert {
    let (trusted, problem) = match &seen.verdict {
        Some(Ok(())) => (true, None),
        Some(Err(e)) => (false, Some(e.clone())),
        None => (false, Some("no certificate".into())),
    };
    PresentedCert {
        fingerprint: fingerprint(&seen.der),
        trusted,
        problem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(trusted: bool) -> PresentedCert {
        PresentedCert {
            fingerprint: "AB:CD:01".into(),
            trusted,
            problem: (!trusted).then(|| "UnknownIssuer".into()),
        }
    }

    #[test]
    fn pinning_decisions() {
        // Untrusted, nothing pinned: ask.
        assert_eq!(check(None, &cert(false)), CertCheck::Ask);
        assert_eq!(check(Some("  "), &cert(false)), CertCheck::Ask);
        // CA-trusted, nothing pinned: connect without asking.
        assert_eq!(check(None, &cert(true)), CertCheck::Accept);
        // The pin matches (any case or separators): connect.
        assert_eq!(check(Some("ab cd 01"), &cert(false)), CertCheck::Accept);
        assert_eq!(check(Some("abcd01"), &cert(false)), CertCheck::Accept);
        // The pin differs: refuse, even when a CA vouches for the new one.
        assert_eq!(
            check(Some("AB:CD:02"), &cert(true)),
            CertCheck::Changed {
                pinned: "AB:CD:02".into()
            }
        );
    }

    #[test]
    fn answers_connect_and_pin_only_when_asked() {
        let c = cert(false);
        assert_eq!(apply(CertDecision::TrustOnce, &c), (true, None));
        assert_eq!(
            apply(CertDecision::TrustAndPin, &c),
            (true, Some("AB:CD:01".into()))
        );
        assert_eq!(apply(CertDecision::Reject, &c), (false, None));
    }

    #[test]
    fn fingerprints_are_sha256_with_colons() {
        let f = fingerprint(b"abc");
        assert!(f.starts_with("BA:78:16:BF"), "{f}");
        assert_eq!(f.len(), 32 * 3 - 1);
        assert!(!same_fingerprint("", ""));
        assert!(same_fingerprint(&f, &f.to_lowercase().replace(':', "")));
    }

    /// DER helpers to build a certificate-shaped structure.
    fn der(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if body.len() < 0x80 {
            out.push(u8::try_from(body.len()).unwrap());
        } else {
            let len = u16::try_from(body.len()).unwrap().to_be_bytes();
            out.push(0x82);
            out.extend(len);
        }
        out.extend(body);
        out
    }

    fn fake_cert(with_version: bool, key: &[u8]) -> Vec<u8> {
        let mut tbs = Vec::new();
        if with_version {
            tbs.extend(der(0xA0, &der(0x02, &[2])));
        }
        tbs.extend(der(0x02, &[1, 2, 3])); // serial
        tbs.extend(der(0x30, &der(0x06, &[1, 2]))); // signature algorithm
        tbs.extend(der(0x30, &[])); // issuer
        tbs.extend(der(0x30, &[0u8; 200])); // validity (long form length)
        tbs.extend(der(0x30, &[])); // subject
        let mut bits = vec![0u8];
        bits.extend(key);
        let mut spki = der(0x30, &der(0x06, &[9, 9]));
        spki.extend(der(0x03, &bits));
        tbs.extend(der(0x30, &spki));
        let mut cert = der(0x30, &tbs);
        cert.extend(der(0x30, &der(0x06, &[1, 2])));
        cert.extend(der(0x03, &[0, 7, 7]));
        der(0x30, &cert)
    }

    #[test]
    fn subject_public_key_is_read_from_the_certificate() {
        let key = [0x30u8, 0x0A, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(
            subject_public_key(&fake_cert(true, &key)).as_deref(),
            Some(&key[..])
        );
        assert_eq!(
            subject_public_key(&fake_cert(false, &key)).as_deref(),
            Some(&key[..])
        );
        assert_eq!(subject_public_key(b"\x30\x05junk"), None);
        assert_eq!(subject_public_key(&[]), None);
    }

    #[test]
    fn presented_reports_the_os_verdict() {
        let ok = presented(&Seen {
            der: b"abc".to_vec(),
            verdict: Some(Ok(())),
        });
        assert!(ok.trusted && ok.problem.is_none());
        let bad = presented(&Seen {
            der: b"abc".to_vec(),
            verdict: Some(Err("UnknownIssuer".into())),
        });
        assert!(!bad.trusted);
        assert_eq!(bad.problem.as_deref(), Some("UnknownIssuer"));
    }
}
