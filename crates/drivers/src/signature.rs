//! minisign (Ed25519) signatures on downloaded manifests.

use minisign_verify::{PublicKey, Signature};

use crate::error::{DriverError, Result};

/// Switchyard's manifest signing key (minisign public key, base64), set at build time with
/// `SWITCHYARD_MANIFEST_PUBKEY`. Without it only the bundled manifest is used.
pub const MANIFEST_PUBLIC_KEY: Option<&str> = option_env!("SWITCHYARD_MANIFEST_PUBKEY");

/// Check `data` against a `.minisig` file's contents. Only current (prehashed) minisign
/// signatures are accepted.
pub fn verify(data: &[u8], signature: &str, public_key: &str) -> Result<()> {
    let pk = PublicKey::from_base64(public_key.trim())
        .map_err(|e| DriverError::Signature(format!("bad public key: {e}")))?;
    let sig = Signature::decode(signature)
        .map_err(|e| DriverError::Signature(format!("bad signature file: {e}")))?;
    pk.verify(data, &sig, false)
        .map_err(|e| DriverError::Signature(e.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");

    fn read(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}{name}")).unwrap()
    }

    #[test]
    fn good_signature_verifies() {
        verify(
            read("manifest.json").as_bytes(),
            &read("manifest.json.minisig"),
            &read("test.pub"),
        )
        .unwrap();
    }

    #[test]
    fn tampered_manifest_is_refused() {
        let tampered = read("manifest.json").replace("2.1", "6.6");
        let err = verify(
            tampered.as_bytes(),
            &read("manifest.json.minisig"),
            &read("test.pub"),
        )
        .unwrap_err();
        assert!(matches!(err, DriverError::Signature(_)), "{err}");
    }

    #[test]
    fn other_key_is_refused() {
        // Same format, different key id and key.
        let other = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let err = verify(
            read("manifest.json").as_bytes(),
            &read("manifest.json.minisig"),
            other,
        )
        .unwrap_err();
        assert!(matches!(err, DriverError::Signature(_)), "{err}");
    }
}
