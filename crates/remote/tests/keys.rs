//! Private key parsing (no network).
//!
//! The fixtures are throwaway test keys (never authorized anywhere): P-256 keys whose private
//! scalar OpenSSH stored in 31 bytes, which the unpatched `ssh-key` rejected (about one key in
//! 256; see `vendor/ssh-key/VENDORED.md`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use russh::keys::{Algorithm, EcdsaCurve, load_secret_key};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn ecdsa_key_with_a_short_scalar_loads() {
    let key = load_secret_key(fixture("ecdsa_short_scalar"), None).unwrap();
    assert_eq!(
        key.algorithm(),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256
        }
    );
}

#[test]
fn encrypted_ecdsa_key_with_a_short_scalar_loads() {
    let key = load_secret_key(fixture("ecdsa_short_scalar_enc"), Some("keypass")).unwrap();
    assert_eq!(
        key.algorithm(),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256
        }
    );
}
