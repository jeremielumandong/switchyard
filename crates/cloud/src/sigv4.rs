//! AWS Signature Version 4 (also used by S3-compatible services such as Cloudflare R2).

use std::time::SystemTime;

use data_encoding::HEXLOWER;
use secrecy::{ExposeSecret, SecretString};

use crate::http::{Req, hmac256, sha256_hex, uri_encode};
use crate::time::amz_date;

/// SHA-256 of an empty body.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A set of AWS credentials.
#[derive(Clone)]
pub struct AwsCredentials {
    /// Access key id (not secret; shown in errors and logs).
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: SecretString,
    /// Session token for temporary credentials (SSO, assumed roles).
    pub session_token: Option<SecretString>,
    /// When temporary credentials stop working, ms since the epoch.
    pub expires_ms: Option<i64>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("session_token", &self.session_token.is_some())
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

/// Sign `req` in place: adds `x-amz-date`, the session token when there is one, `host`,
/// and `authorization`. `s3` also sends `x-amz-content-sha256` (S3 requires it).
pub(crate) fn sign(
    req: &mut Req,
    creds: &AwsCredentials,
    region: &str,
    service: &str,
    now: SystemTime,
    s3: bool,
) {
    let date_time = amz_date(now);
    let date = &date_time[..8];
    let payload = if req.body.is_empty() {
        EMPTY_SHA256.to_owned()
    } else {
        sha256_hex(&req.body)
    };
    req.set_header("x-amz-date", date_time.clone());
    if s3 {
        req.set_header("x-amz-content-sha256", payload.clone());
    }
    if let Some(t) = &creds.session_token {
        req.set_header("x-amz-security-token", t.expose_secret().to_owned());
    }
    let host = req.host();
    req.set_header("host", host);

    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    headers.sort();
    let signed = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let mut query: Vec<(String, String)> = req
        .query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method.as_str(),
        req.full_path(),
        canonical_query,
        canonical_headers,
        signed,
        payload
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{date_time}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let k_date = hmac256(
        format!("AWS4{}", creds.secret_access_key.expose_secret()).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac256(&k_date, region.as_bytes());
    let k_service = hmac256(&k_region, service.as_bytes());
    let k_signing = hmac256(&k_service, b"aws4_request");
    let signature = HEXLOWER.encode(&hmac256(&k_signing, to_sign.as_bytes()));
    req.set_header(
        "authorization",
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            creds.access_key_id
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn creds(secret: &str) -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: SecretString::from(secret.to_owned()),
            session_token: None,
            expires_ms: None,
        }
    }

    fn auth(req: &Req) -> String {
        req.headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// `get-vanilla` from the AWS Signature Version 4 test suite.
    #[test]
    fn get_vanilla() {
        let base = url::Url::parse("https://example.amazonaws.com").unwrap();
        let mut req = Req::new(reqwest::Method::GET, &base, "/");
        let now = UNIX_EPOCH + Duration::from_secs(1_440_938_160); // 20150830T123600Z
        sign(
            &mut req,
            &creds("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            "us-east-1",
            "service",
            now,
            false,
        );
        assert_eq!(
            auth(&req),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// `get-vanilla-query-order-key-case` (query parameters sorted by name).
    #[test]
    fn query_order() {
        let base = url::Url::parse("https://example.amazonaws.com").unwrap();
        let mut req = Req::new(reqwest::Method::GET, &base, "/")
            .query("Param2", "value2")
            .query("Param1", "value1");
        let now = UNIX_EPOCH + Duration::from_secs(1_440_938_160);
        sign(
            &mut req,
            &creds("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            "us-east-1",
            "service",
            now,
            false,
        );
        assert!(
            auth(&req).ends_with(
                "Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
            ),
            "{}",
            auth(&req)
        );
    }

    /// The GET Object example from the S3 documentation (Range header, S3 payload header).
    #[test]
    fn s3_get_object() {
        let base = url::Url::parse("https://examplebucket.s3.amazonaws.com").unwrap();
        let mut req =
            Req::new(reqwest::Method::GET, &base, "/test.txt").header("range", "bytes=0-9");
        let now = UNIX_EPOCH + Duration::from_secs(1_369_353_600); // 20130524T000000Z
        let mut c = creds("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        c.access_key_id = "AKIAIOSFODNN7EXAMPLE".into();
        sign(&mut req, &c, "us-east-1", "s3", now, true);
        assert_eq!(
            auth(&req),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }
}
