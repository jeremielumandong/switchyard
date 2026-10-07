//! Workspace-scoped HTTP cookie metadata backed by the Workbench secret vault.
//!
//! The jar is safe to serialize: cookie values are represented only by opaque
//! [`SecretRef`]s. Resolving a request header always goes through [`SecretStore`]
//! and fails closed if any selected value is unavailable.

use super::{SecretRef, SecretStore, SecretStoreError, SecretValue, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::{fmt, net::IpAddr, str::FromStr, time::UNIX_EPOCH};
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

/// Persistence-safe cookie metadata. `value` is an opaque vault reference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub domain: String,
    pub path: String,
    pub value: SecretRef,
    #[serde(default = "default_true")]
    pub host_only: bool,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub same_site: Option<SameSite>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CookieJar {
    pub workspace_id: WorkspaceId,
    #[serde(default)]
    cookies: Vec<Cookie>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CookieMutation {
    Stored(SecretRef),
    Deleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CookieJarError {
    InvalidRequestUrl,
    InvalidSetCookie,
    SecretStore(SecretStoreError),
}

impl fmt::Display for CookieJarError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequestUrl => formatter.write_str("cookie request URL is invalid"),
            Self::InvalidSetCookie => formatter.write_str("Set-Cookie header is invalid"),
            Self::SecretStore(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CookieJarError {}

impl From<SecretStoreError> for CookieJarError {
    fn from(value: SecretStoreError) -> Self {
        Self::SecretStore(value)
    }
}

impl CookieJar {
    pub fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            cookies: Vec::new(),
        }
    }

    pub fn cookies(&self) -> &[Cookie] {
        &self.cookies
    }

    /// Applies one Set-Cookie response header at `now` (Unix seconds).
    ///
    /// Replacement keeps the existing vault reference, so updating a cookie
    /// does not create a second durable secret. Metadata changes are applied
    /// only after the vault write succeeds.
    pub fn set_cookie(
        &mut self,
        store: &dyn SecretStore,
        request_url: &str,
        header: &str,
        now: i64,
    ) -> Result<CookieMutation, CookieJarError> {
        let request = cookie_url(request_url)?;
        if header.contains(['\r', '\n']) {
            return Err(CookieJarError::InvalidSetCookie);
        }
        let request_host = request
            .host_str()
            .ok_or(CookieJarError::InvalidRequestUrl)?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let default_path = default_cookie_path(request.path());

        let mut fields = header.split(';');
        let (name, value) = fields
            .next()
            .and_then(|pair| pair.trim().split_once('='))
            .ok_or(CookieJarError::InvalidSetCookie)?;
        let name = name.trim();
        let value = value.trim();
        if !valid_cookie_name(name) || !valid_cookie_value(value) {
            return Err(CookieJarError::InvalidSetCookie);
        }

        let mut domain = request_host.clone();
        let mut host_only = true;
        let mut path = default_path;
        let mut secure = false;
        let mut http_only = false;
        let mut same_site = None;
        let mut expires_at = None;
        let mut max_age = None;

        for field in fields {
            let (attribute, attribute_value) = field
                .trim()
                .split_once('=')
                .map_or((field.trim(), None), |(name, value)| {
                    (name.trim(), Some(value.trim()))
                });
            match attribute.to_ascii_lowercase().as_str() {
                "domain" => {
                    let candidate = attribute_value
                        .ok_or(CookieJarError::InvalidSetCookie)?
                        .trim_start_matches('.')
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    if candidate.is_empty()
                        || candidate.bytes().any(|byte| {
                            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
                        })
                        || IpAddr::from_str(&request_host).is_ok()
                        || !domain_matches(&request_host, &candidate)
                        || is_public_suffix(&candidate)
                    {
                        return Err(CookieJarError::InvalidSetCookie);
                    }
                    domain = candidate;
                    host_only = false;
                }
                "path" => {
                    if let Some(candidate) = attribute_value.filter(|value| value.starts_with('/'))
                    {
                        if candidate.bytes().any(|byte| byte.is_ascii_control()) {
                            return Err(CookieJarError::InvalidSetCookie);
                        }
                        path = candidate.to_owned();
                    }
                }
                "secure" => secure = true,
                "httponly" => http_only = true,
                "samesite" => {
                    same_site = match attribute_value.map(str::to_ascii_lowercase).as_deref() {
                        Some("strict") => Some(SameSite::Strict),
                        Some("lax") => Some(SameSite::Lax),
                        Some("none") => Some(SameSite::None),
                        _ => None,
                    };
                }
                "max-age" => {
                    max_age = attribute_value.and_then(|value| value.parse::<i64>().ok());
                }
                "expires" => {
                    expires_at = attribute_value
                        .and_then(|value| httpdate::parse_http_date(value).ok())
                        .map(|time| {
                            time.duration_since(UNIX_EPOCH)
                                .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
                                .unwrap_or(0)
                        });
                }
                _ => {}
            }
        }

        // Max-Age takes precedence over Expires. Saturation avoids an attacker
        // wrapping a huge positive lifetime into an already-expired timestamp.
        if let Some(max_age) = max_age {
            expires_at = Some(if max_age <= 0 {
                now
            } else {
                now.saturating_add(max_age)
            });
        }

        validate_cookie_fields(name, &domain, &path)?;

        let existing = self.cookies.iter().position(|cookie| {
            cookie.name == name && cookie.domain == domain && cookie.path == path
        });
        if expires_at.is_some_and(|expiry| expiry <= now) {
            if let Some(index) = existing {
                let reference = self.cookies[index].value.clone();
                store.delete_secret(&self.workspace_id, &reference)?;
                self.cookies.remove(index);
            }
            return Ok(CookieMutation::Deleted);
        }

        let reference = if let Some(index) = existing {
            let reference = self.cookies[index].value.clone();
            store.set_secret(&self.workspace_id, &reference, SecretValue::new(value))?;
            reference
        } else {
            store.create_secret(&self.workspace_id, SecretValue::new(value))?
        };

        let cookie = Cookie {
            name: name.to_owned(),
            domain,
            path,
            value: reference.clone(),
            host_only,
            secure,
            http_only,
            same_site,
            expires_at,
        };
        validate_cookie_metadata(&cookie)?;
        if let Some(index) = existing {
            self.cookies[index] = cookie;
        } else {
            self.cookies.push(cookie);
        }
        Ok(CookieMutation::Stored(reference))
    }

    /// Produces the Cookie request-header value for a URL.
    ///
    /// If any matching value cannot be resolved, no partial header is returned.
    pub fn header_for_url(
        &self,
        store: &dyn SecretStore,
        request_url: &str,
        now: i64,
    ) -> Result<Option<SecretValue>, CookieJarError> {
        let request = cookie_url(request_url)?;
        let host = request
            .host_str()
            .ok_or(CookieJarError::InvalidRequestUrl)?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let secure_transport = matches!(request.scheme(), "https" | "wss");
        let mut matches = self
            .cookies
            .iter()
            .enumerate()
            .filter(|(_, cookie)| {
                cookie.expires_at.is_none_or(|expiry| expiry > now)
                    && (!cookie.secure || secure_transport)
                    && if cookie.host_only {
                        host == cookie.domain
                    } else {
                        domain_matches(&host, &cookie.domain)
                    }
                    && path_matches(request.path(), &cookie.path)
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(index, cookie)| (std::cmp::Reverse(cookie.path.len()), *index));

        let mut header = ZeroizingString::default();
        for (_, cookie) in matches {
            validate_cookie_metadata(cookie)?;
            let value = store.get_secret(&self.workspace_id, &cookie.value)?;
            if !valid_cookie_value(value.expose_secret()) {
                return Err(CookieJarError::InvalidSetCookie);
            }
            if !header.0.is_empty() {
                header.0.push_str("; ");
            }
            header.0.push_str(&cookie.name);
            header.0.push('=');
            header.0.push_str(value.expose_secret());
        }
        Ok((!header.0.is_empty()).then(|| SecretValue::new(header.0.as_str())))
    }

    /// Removes expired metadata and the corresponding vault entries.
    pub fn purge_expired(
        &mut self,
        store: &dyn SecretStore,
        now: i64,
    ) -> Result<usize, CookieJarError> {
        let expired = self
            .cookies
            .iter()
            .filter(|cookie| cookie.expires_at.is_some_and(|expiry| expiry <= now))
            .map(|cookie| cookie.value.clone())
            .collect::<Vec<_>>();
        for reference in &expired {
            store.delete_secret(&self.workspace_id, reference)?;
        }
        self.cookies
            .retain(|cookie| cookie.expires_at.is_none_or(|expiry| expiry > now));
        Ok(expired.len())
    }

    pub fn delete_cookie(
        &mut self,
        store: &dyn SecretStore,
        name: &str,
        domain: &str,
        path: &str,
    ) -> Result<bool, CookieJarError> {
        let domain = domain
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let Some(index) = self.cookies.iter().position(|cookie| {
            cookie.name == name && cookie.domain == domain && cookie.path == path
        }) else {
            return Ok(false);
        };
        let reference = self.cookies[index].value.clone();
        store.delete_secret(&self.workspace_id, &reference)?;
        self.cookies.remove(index);
        Ok(true)
    }
}

/// Keeps the assembled header zeroized even if resolution fails midway.
#[derive(Default)]
struct ZeroizingString(zeroize::Zeroizing<String>);

fn cookie_url(input: &str) -> Result<Url, CookieJarError> {
    let url = Url::parse(input).map_err(|_| CookieJarError::InvalidRequestUrl)?;
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") || url.host_str().is_none() {
        return Err(CookieJarError::InvalidRequestUrl);
    }
    Ok(url)
}

fn valid_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte > 0x20
                && byte < 0x7f
                && !matches!(
                    byte,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        })
}

fn valid_cookie_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f && byte != b';'))
}

fn validate_cookie_metadata(cookie: &Cookie) -> Result<(), CookieJarError> {
    validate_cookie_fields(&cookie.name, &cookie.domain, &cookie.path)
}

fn validate_cookie_fields(name: &str, domain: &str, path: &str) -> Result<(), CookieJarError> {
    let unbracketed_domain = domain
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(domain);
    let valid_domain = IpAddr::from_str(unbracketed_domain).is_ok()
        || (!domain.is_empty()
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && domain
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.')));
    if !valid_cookie_name(name)
        || !valid_domain
        || !path.starts_with('/')
        || path.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(CookieJarError::InvalidSetCookie);
    }
    Ok(())
}

const fn default_true() -> bool {
    true
}

fn default_cookie_path(request_path: &str) -> String {
    if !request_path.starts_with('/') || request_path == "/" {
        return "/".to_owned();
    }
    match request_path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(index) => request_path[..index].to_owned(),
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn is_public_suffix(domain: &str) -> bool {
    // `domain` returns the registrable eTLD+1. A bare public suffix (including
    // private PSL entries such as hosting boundaries) has no registrable
    // domain and must not be allowed to widen a cookie's scope.
    psl::domain(domain.as_bytes()).is_none()
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || request_path
            .strip_prefix(cookie_path)
            .is_some_and(|suffix| cookie_path.ends_with('/') || suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemorySecretStore;

    fn fixture() -> (CookieJar, MemorySecretStore) {
        (
            CookieJar::new(WorkspaceId::new("project-a").unwrap()),
            MemorySecretStore::new(),
        )
    }

    #[test]
    fn serialized_jar_contains_a_reference_but_never_the_cookie_value() {
        let (mut jar, store) = fixture();
        jar.set_cookie(
            &store,
            "https://api.example.com/v1/items",
            "session=actual-super-secret; Path=/; Secure; HttpOnly",
            1_000,
        )
        .unwrap();

        let json = serde_json::to_string(&jar).unwrap();
        assert!(json.contains("wbsec-"));
        assert!(!json.contains("actual-super-secret"));
        assert!(!format!("{jar:?}").contains("actual-super-secret"));
    }

    #[test]
    fn deserialized_cookie_without_scope_metadata_defaults_to_host_only() {
        let cookie: Cookie = serde_json::from_value(serde_json::json!({
            "name": "session",
            "domain": "api.example.com",
            "path": "/",
            "value": "opaque-reference"
        }))
        .unwrap();

        assert!(cookie.host_only);
    }

    #[test]
    fn matching_honors_host_domain_path_secure_and_expiry_rules() {
        let (mut jar, store) = fixture();
        jar.set_cookie(
            &store,
            "https://api.example.com/v1/login",
            "host=one; Path=/v1",
            1_000,
        )
        .unwrap();
        jar.set_cookie(
            &store,
            "https://api.example.com/v1/login",
            "domain=two; Domain=example.com; Path=/; Secure",
            1_000,
        )
        .unwrap();
        jar.set_cookie(
            &store,
            "https://api.example.com/v1/login",
            "expired=three; Path=/; Max-Age=10",
            1_000,
        )
        .unwrap();

        assert_eq!(
            jar.header_for_url(&store, "https://api.example.com/v1/items", 1_005)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "host=one; domain=two; expired=three"
        );
        assert_eq!(
            jar.header_for_url(&store, "http://other.example.com/v1/items", 1_020)
                .unwrap(),
            None
        );
        assert_eq!(
            jar.header_for_url(&store, "https://other.example.com/items", 1_020)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "domain=two"
        );
    }

    #[test]
    fn update_reuses_reference_and_deletion_removes_the_vault_value() {
        let (mut jar, store) = fixture();
        let first = jar
            .set_cookie(&store, "https://example.com/a", "token=old; Path=/", 100)
            .unwrap();
        let first_ref = match first {
            CookieMutation::Stored(reference) => reference,
            CookieMutation::Deleted => panic!("cookie was unexpectedly deleted"),
        };
        let second = jar
            .set_cookie(&store, "https://example.com/a", "token=new; Path=/", 101)
            .unwrap();
        assert_eq!(second, CookieMutation::Stored(first_ref.clone()));
        assert_eq!(
            jar.header_for_url(&store, "https://example.com/a", 101)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "token=new"
        );

        assert_eq!(
            jar.set_cookie(
                &store,
                "https://example.com/a",
                "token=gone; Path=/; Max-Age=0",
                102,
            )
            .unwrap(),
            CookieMutation::Deleted
        );
        assert_eq!(
            store.get_secret(&jar.workspace_id, &first_ref),
            Err(SecretStoreError::Missing)
        );
    }

    struct FailingStore;

    impl SecretStore for FailingStore {
        fn create_secret(
            &self,
            _: &WorkspaceId,
            _: SecretValue,
        ) -> Result<SecretRef, SecretStoreError> {
            Err(SecretStoreError::BackendUnavailable)
        }

        fn set_secret(
            &self,
            _: &WorkspaceId,
            _: &SecretRef,
            _: SecretValue,
        ) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::BackendUnavailable)
        }

        fn get_secret(
            &self,
            _: &WorkspaceId,
            _: &SecretRef,
        ) -> Result<SecretValue, SecretStoreError> {
            Err(SecretStoreError::BackendUnavailable)
        }

        fn delete_secret(&self, _: &WorkspaceId, _: &SecretRef) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::BackendUnavailable)
        }
    }

    #[test]
    fn backend_failure_never_persists_plaintext_or_partial_metadata() {
        let mut jar = CookieJar::new(WorkspaceId::new("project-a").unwrap());
        assert_eq!(
            jar.set_cookie(
                &FailingStore,
                "https://example.com/",
                "session=must-not-persist",
                100,
            ),
            Err(CookieJarError::SecretStore(
                SecretStoreError::BackendUnavailable
            ))
        );
        assert!(jar.cookies().is_empty());
        assert!(
            !serde_json::to_string(&jar)
                .unwrap()
                .contains("must-not-persist")
        );
    }

    #[test]
    fn rejects_cross_domain_and_header_injection() {
        let (mut jar, store) = fixture();
        assert_eq!(
            jar.set_cookie(
                &store,
                "https://api.example.com/",
                "session=x; Domain=attacker.example",
                100,
            ),
            Err(CookieJarError::InvalidSetCookie)
        );
        assert_eq!(
            jar.set_cookie(
                &store,
                "https://api.example.com/",
                "session=x\r\nX-Evil: yes",
                100,
            ),
            Err(CookieJarError::InvalidSetCookie)
        );

        let reference = store
            .create_secret(&jar.workspace_id, SecretValue::new("x\r\nX-Evil: yes"))
            .unwrap();
        jar.cookies.push(Cookie {
            name: "crafted".to_owned(),
            domain: "api.example.com".to_owned(),
            path: "/".to_owned(),
            value: reference,
            host_only: true,
            secure: false,
            http_only: false,
            same_site: None,
            expires_at: None,
        });
        assert_eq!(
            jar.header_for_url(&store, "https://api.example.com/", 100),
            Err(CookieJarError::InvalidSetCookie)
        );
    }

    #[test]
    fn rejects_public_suffix_domain_before_it_can_cross_sites() {
        let (mut jar, store) = fixture();

        assert_eq!(
            jar.set_cookie(
                &store,
                "https://example.com/login",
                "session=must-stay-local; Domain=com; Path=/",
                100,
            ),
            Err(CookieJarError::InvalidSetCookie)
        );
        assert!(jar.cookies().is_empty());
        assert_eq!(
            jar.header_for_url(&store, "https://unrelated.com/", 100)
                .unwrap(),
            None
        );

        assert_eq!(
            jar.set_cookie(
                &store,
                "https://example.co.uk/login",
                "session=must-stay-local; Domain=co.uk; Path=/",
                100,
            ),
            Err(CookieJarError::InvalidSetCookie)
        );
    }
}
