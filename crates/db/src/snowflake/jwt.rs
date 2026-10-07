//! Snowflake key-pair authentication: the RS256 JWT the SQL API accepts as a bearer token
//! (<https://docs.snowflake.com/en/developer-guide/sql-api/authenticating>).
//!
//! Details that a hand-rolled JWT gets wrong:
//!
//! * The account in the claims drops region and cloud segments and is upper-cased, so
//!   `xy12345.us-east-1.aws` and `XY12345` give the same issuer.
//! * The fingerprint is SHA-256 over the DER `SubjectPublicKeyInfo` of the public key,
//!   standard base64 with padding: what `DESCRIBE USER` shows as `RSA_PUBLIC_KEY_FP`.
//! * Snowflake rejects lifetimes over one hour.
//!
//! `rsa` only parses the key (PKCS#8, encrypted PKCS#8, PKCS#1); signing goes through
//! `ring`, whose RSA is constant-time.

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};
use sha2::{Digest, Sha256};

/// Lifetime of a minted token: under Snowflake's one-hour cap, with a minute of slack
/// for clock skew.
pub const JWT_LIFETIME_SECS: u64 = 59 * 60;

/// A parsed private key, ready to sign.
pub struct KeyPair {
    signer: RsaKeyPair,
    fingerprint: String,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl KeyPair {
    /// Parse a PEM private key, encrypted or not.
    pub fn from_pem(pem: &str, passphrase: Option<&str>) -> Result<Self, String> {
        let key = parse_private_key(pem, passphrase)?;
        let fingerprint = public_key_fingerprint(&key)?;
        let der = key
            .to_pkcs8_der()
            .map_err(|e| format!("cannot encode the private key: {e}"))?;
        let signer = RsaKeyPair::from_pkcs8(der.as_bytes())
            .map_err(|e| format!("the private key cannot sign (Snowflake needs RSA 2048+): {e}"))?;
        Ok(Self {
            signer,
            fingerprint,
        })
    }

    /// `SHA256:<base64>` of the public key, as `DESCRIBE USER` reports it.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// A token for `user` on `account`, issued at `issued_at` (Unix seconds).
    pub fn jwt(&self, account: &str, user: &str, issued_at: u64) -> Result<String, String> {
        let user = user.trim();
        if user.is_empty() {
            return Err("key-pair authentication needs a user name".into());
        }
        let account = account_identifier(account);
        if account.is_empty() {
            return Err("key-pair authentication needs an account".into());
        }
        let qualified = format!("{account}.{}", user.to_ascii_uppercase());
        let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        let claims = serde_json::json!({
            "iss": format!("{qualified}.{}", self.fingerprint),
            "sub": qualified,
            "iat": issued_at,
            "exp": issued_at + JWT_LIFETIME_SECS,
        });
        let input = format!(
            "{}.{}",
            segment(header.to_string().as_bytes()),
            segment(claims.to_string().as_bytes())
        );
        let mut signature = vec![0; self.signer.public().modulus_len()];
        self.signer
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "cannot sign the login token".to_string())?;
        Ok(format!("{input}.{}", segment(&signature)))
    }
}

fn segment(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Parse PKCS#8 (`BEGIN PRIVATE KEY`), encrypted PKCS#8 (`BEGIN ENCRYPTED PRIVATE KEY`,
/// what `openssl pkcs8 -topk8` writes) or PKCS#1 (`BEGIN RSA PRIVATE KEY`).
pub fn parse_private_key(pem: &str, passphrase: Option<&str>) -> Result<RsaPrivateKey, String> {
    let pem = pem.trim();
    if pem.is_empty() {
        return Err("the private key file is empty".into());
    }
    let passphrase = passphrase.filter(|p| !p.is_empty());
    if pem.contains("ENCRYPTED PRIVATE KEY") {
        let passphrase = passphrase.ok_or("the private key is encrypted: enter its passphrase")?;
        return RsaPrivateKey::from_pkcs8_encrypted_pem(pem, passphrase)
            .map_err(|e| format!("cannot decrypt the private key (check the passphrase): {e}"));
    }
    RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|e| RsaPrivateKey::from_pkcs1_pem(pem).map_err(|_| e))
        .map_err(|e| format!("cannot read the private key: {e}"))
}

/// `SHA256:<base64>` over the DER `SubjectPublicKeyInfo`.
pub fn public_key_fingerprint(key: &RsaPrivateKey) -> Result<String, String> {
    let der = key
        .to_public_key()
        .to_public_key_der()
        .map_err(|e| format!("cannot encode the public key: {e}"))?;
    Ok(format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(der.as_bytes()))
    ))
}

/// The account as JWT claims spell it: upper-cased, without region, cloud or domain. A
/// global URL (`<account>-<id>.global`) keeps only the part before the first `-`.
pub fn account_identifier(account: &str) -> String {
    let lower = account.trim().to_ascii_lowercase();
    let account = lower
        .strip_suffix(".snowflakecomputing.com")
        .unwrap_or(&lower);
    let cut = if account.contains(".global") {
        account.find('-')
    } else {
        account.find('.')
    };
    let id = match cut {
        Some(i) if i > 0 => &account[..i],
        _ => account,
    };
    id.to_ascii_uppercase()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::pkcs8::LineEnding;
    use std::sync::OnceLock;

    /// Passphrase of the encrypted fixture.
    pub(crate) const PASSPHRASE: &str = "test-passphrase";

    /// One generated key in the three forms a user may have. Generated, never committed:
    /// a private key in the tree ends up registered on a real user one day.
    pub(crate) struct Keys {
        pub(crate) pkcs8: String,
        pub(crate) pkcs1: String,
        pub(crate) encrypted: String,
    }

    pub(crate) fn keys() -> &'static Keys {
        static KEYS: OnceLock<Keys> = OnceLock::new();
        KEYS.get_or_init(|| {
            let mut rng = rsa::rand_core::OsRng;
            let key = RsaPrivateKey::new(&mut rng, 2048).expect("generate key");
            let pkcs8 = key.to_pkcs8_pem(LineEnding::LF).expect("pkcs8").to_string();
            let pkcs1 = key.to_pkcs1_pem(LineEnding::LF).expect("pkcs1").to_string();
            // PBKDF2-SHA256 + AES-256-CBC, as `openssl pkcs8 -topk8 -v2 aes-256-cbc`
            // writes (the default scrypt is far too slow for unit tests).
            let params = rsa::pkcs8::pkcs5::pbes2::Parameters::pbkdf2_sha256_aes256cbc(
                2048,
                b"saltsalt",
                b"0123456789abcdef",
            )
            .expect("pbes2 params");
            let der = key.to_pkcs8_der().expect("der");
            let info = rsa::pkcs8::PrivateKeyInfo::try_from(der.as_bytes()).expect("info");
            let encrypted = info
                .encrypt_with_params(params, PASSPHRASE)
                .expect("encrypt")
                .to_pem("ENCRYPTED PRIVATE KEY", LineEnding::LF)
                .expect("pem")
                .to_string();
            Keys {
                pkcs8,
                pkcs1,
                encrypted,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{PASSPHRASE, keys};
    use super::*;

    fn decode(seg: &str) -> serde_json::Value {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(seg)
            .expect("base64url");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[test]
    fn account_identifier_strips_region_cloud_and_domain() {
        for (input, want) in [
            ("xy12345", "XY12345"),
            ("xy12345.us-east-1", "XY12345"),
            ("xy12345.us-east-1.aws", "XY12345"),
            ("xy12345.us-east-1.aws.snowflakecomputing.com", "XY12345"),
            ("myorg-myaccount", "MYORG-MYACCOUNT"),
            ("myorg-myaccount.snowflakecomputing.com", "MYORG-MYACCOUNT"),
            ("xy12345-abc.global", "XY12345"),
        ] {
            assert_eq!(account_identifier(input), want, "{input}");
        }
    }

    #[test]
    fn every_key_form_gives_the_same_fingerprint() {
        let k = keys();
        let a = KeyPair::from_pem(&k.pkcs8, None).expect("pkcs8");
        let b = KeyPair::from_pem(&k.pkcs1, None).expect("pkcs1");
        let c = KeyPair::from_pem(&k.encrypted, Some(PASSPHRASE)).expect("encrypted");
        assert!(a.fingerprint().starts_with("SHA256:"));
        assert!(
            a.fingerprint().ends_with('='),
            "standard base64 keeps padding"
        );
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), c.fingerprint());
    }

    #[test]
    fn encrypted_key_needs_the_right_passphrase() {
        let k = keys();
        let e = KeyPair::from_pem(&k.encrypted, None).expect_err("no passphrase");
        assert!(e.contains("passphrase"), "{e}");
        let e = KeyPair::from_pem(&k.encrypted, Some("wrong")).expect_err("wrong passphrase");
        assert!(e.contains("check the passphrase"), "{e}");
    }

    #[test]
    fn jwt_claims_and_signature_verify() {
        let k = keys();
        let pair = KeyPair::from_pem(&k.pkcs8, None).expect("key");
        let token = pair
            .jwt("xy12345.us-east-1", "reader", 1_700_000_000)
            .expect("jwt");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(decode(parts[0])["alg"], "RS256");
        let claims = decode(parts[1]);
        assert_eq!(claims["sub"], "XY12345.READER");
        assert_eq!(
            claims["iss"],
            format!("XY12345.READER.{}", pair.fingerprint())
        );
        assert_eq!(claims["exp"], 1_700_000_000u64 + JWT_LIFETIME_SECS);

        let public = pair.signer.public().as_ref().to_vec();
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .expect("signature");
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            public,
        )
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("signature verifies");
    }

    #[test]
    fn missing_user_or_account_is_an_error() {
        let pair = KeyPair::from_pem(&keys().pkcs8, None).expect("key");
        assert!(pair.jwt("acct", " ", 0).is_err());
        assert!(pair.jwt("", "u", 0).is_err());
    }

    #[test]
    fn garbage_is_not_a_key() {
        assert!(KeyPair::from_pem("", None).is_err());
        assert!(
            KeyPair::from_pem(
                "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----",
                None
            )
            .is_err()
        );
    }
}
