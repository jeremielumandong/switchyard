//! Native HTTP for the API workbench: sending requests (redirects, cookies, AWS SigV4,
//! the outbound address policy) and OAuth token exchanges. Ported from AgentOps's
//! agent-service `native_routines` (MIT, same owner).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One transient request from the native API workbench. This deliberately has
/// no routine identity, auth profile, retry policy, or persistence fields.
#[derive(Debug, Clone)]
pub struct ApiWorkbenchRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
    pub allow_private_network: bool,
    pub timeout_ms: u64,
    pub max_redirects: u8,
    pub response_limit_bytes: usize,
    pub aws_sigv4: Option<ApiWorkbenchAwsSigV4>,
}

#[derive(Debug, Clone)]
pub struct ApiWorkbenchAwsSigV4 {
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    pub service: String,
    pub session_token: Option<String>,
}

/// An upstream answer returned through the authenticated loopback API.
#[derive(Debug, Clone)]
pub struct ApiWorkbenchResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    /// Exact Set-Cookie fields for the native cookie jar. They are kept out
    /// of the ordinary inspection headers so callers cannot accidentally
    /// persist or export them as response metadata.
    pub set_cookies: Vec<String>,
    /// Redirect-hop cookie mutations retain their source URL so the native jar
    /// cannot misattribute a host-only cookie to the final redirect target.
    pub cookie_mutations: Vec<ApiWorkbenchCookieMutation>,
    pub body: Vec<u8>,
    pub final_url: String,
    pub http_version: String,
    pub received_bytes: u64,
    pub stored_bytes: u64,
    /// SHA-256 of the complete upstream representation. This is absent when
    /// the storage ceiling made us stop before EOF; a prefix hash must never
    /// masquerade as a full-payload identity.
    pub full_body_sha256: Option<String>,
    pub timings: ApiWorkbenchTimings,
    pub cookies: Vec<ApiWorkbenchCookie>,
    pub duration_ms: u64,
    pub truncated: bool,
    pub redirects: Vec<ApiWorkbenchRedirect>,
}

#[derive(Debug, Clone)]
pub struct ApiWorkbenchCookieMutation {
    pub source_url: String,
    pub header: String,
}

#[derive(Debug, Clone)]
pub struct ApiWorkbenchRedirect {
    pub status: u16,
    pub from: String,
    pub to: String,
    pub method: String,
    pub cross_origin: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ApiWorkbenchTimings {
    pub dns_ms: u64,
    pub first_byte_ms: u64,
    pub download_ms: u64,
}

/// Value-free cookie facts safe to inspect and persist. Raw `Set-Cookie`
/// fields remain transient input to the native cookie jar.
#[derive(Debug, Clone)]
pub struct ApiWorkbenchCookie {
    pub name: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchOAuthRequest {
    #[serde(default = "oauth_wire_version")]
    pub version: u32,
    pub flow: WorkbenchOAuthFlow,
    pub token_url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub code_verifier: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Resource-owner credentials for the `password` grant.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub credentials_in: WorkbenchOAuthCredentialsIn,
    /// Authorization callbacks must present both values. The service checks
    /// them before it sends the code to a token endpoint.
    #[serde(default)]
    pub expected_state: Option<String>,
    #[serde(default)]
    pub callback_state: Option<String>,
    #[serde(default)]
    pub allow_private_network: bool,
    #[serde(default = "oauth_default_timeout", rename = "timeoutMs")]
    pub timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkbenchOAuthFlow {
    ClientCredentials,
    #[serde(alias = "pkce")]
    AuthorizationCode,
    RefreshToken,
    /// RFC 6749 §4.3 resource-owner password credentials — a user token
    /// for APIs that need a signed-in user, where no browser is possible.
    Password,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkbenchOAuthCredentialsIn {
    #[default]
    ClientSecretPost,
    ClientSecretBasic,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchOAuthResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
    pub id_token: Option<String>,
}

const fn oauth_wire_version() -> u32 {
    1
}

const fn oauth_default_timeout() -> u64 {
    30_000
}

/// The RFC-defined method/entity state for one followed redirect. Keeping the
/// transition in one typed helper prevents the interactive client and Routine
/// HTTP steps from drifting into different redirect semantics.
#[derive(Debug, Clone)]
struct RedirectTransition {
    method: reqwest::Method,
    preserve_entity: bool,
}

fn redirect_transition(status: u16, method: &reqwest::Method) -> Option<RedirectTransition> {
    let (method, preserve_entity) = match status {
        // The long-established user-agent compatibility rule rewrites POST,
        // while other methods retain their semantics for 301/302.
        301 | 302 if *method == reqwest::Method::POST => (reqwest::Method::GET, false),
        301 | 302 => (method.clone(), true),
        // RFC 9110 requires 303 to retrieve with GET, except HEAD remains HEAD.
        303 if *method == reqwest::Method::HEAD => (reqwest::Method::HEAD, false),
        303 => (reqwest::Method::GET, false),
        // 307/308 were defined specifically to preserve method and content.
        307 | 308 => (method.clone(), true),
        _ => return None,
    };
    Some(RedirectTransition {
        method,
        preserve_entity,
    })
}

/// Send one interactive API workbench request through the same network boundary
/// as Routine HTTP steps. Upstream 4xx/5xx answers are data, not local errors.
pub async fn send_api_workbench_request(
    request: ApiWorkbenchRequest,
) -> Result<ApiWorkbenchResponse, String> {
    let started = Instant::now();
    let mut method = reqwest::Method::from_bytes(request.method.trim().as_bytes())
        .map_err(|error| format!("invalid HTTP method: {error}"))?;
    let url = request.url.trim();
    if url.is_empty() {
        return Err("API request needs a URL".into());
    }
    if matches!(method, reqwest::Method::GET | reqwest::Method::HEAD)
        && request.body.as_deref().is_some_and(|body| !body.is_empty())
    {
        return Err(format!("{method} requests cannot include a body"));
    }
    if !(100..=300_000).contains(&request.timeout_ms) {
        return Err("API request timeout must be between 100 and 300000 ms".into());
    }
    if request.max_redirects > 10 {
        return Err("API request redirect limit cannot exceed 10".into());
    }
    if request.response_limit_bytes > 5 * 1024 * 1024 {
        return Err("API response limit must be zero (complete) or at most 5 MiB".into());
    }
    if let Some(signing) = request.aws_sigv4.as_ref() {
        validate_aws_sigv4(signing)?;
    }
    let deadline = started + Duration::from_millis(request.timeout_ms);
    validate_http_step_url(url, request.allow_private_network)?;
    let mut url = reqwest::Url::parse(url).map_err(|error| format!("invalid API URL: {error}"))?;
    let original_origin = origin_of(&url);
    let mut redirects = Vec::new();
    let mut body = request.body;
    let mut drop_entity_headers = false;
    let mut dns_duration = Duration::ZERO;
    let mut redirect_cookie_jar = RedirectCookieJar::default();
    let mut redirect_cookie_mutations = Vec::new();

    for redirect in 0..=request.max_redirects {
        validate_http_step_url(url.as_str(), request.allow_private_network)?;
        let dns_started = Instant::now();
        let validated_addresses = resolve_http_step_addresses_before(
            &url,
            request.allow_private_network,
            deadline,
            "API request",
        )
        .await?;
        dns_duration = dns_duration.saturating_add(dns_started.elapsed());
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "API request exceeded its deadline".to_string())?;
        let client = pinned_http_client(&url, &validated_addresses, remaining, "API")?;
        let same_origin = origin_of(&url) == original_origin;
        let mut outbound_headers = reqwest::header::HeaderMap::new();
        let mut has_content_type = false;
        for (name, value) in &request.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.trim().as_bytes())
                .map_err(|error| format!("invalid HTTP header name: {error}"))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|error| format!("invalid HTTP header value: {error}"))?;
            if request.aws_sigv4.is_some()
                && matches!(
                    name.as_str(),
                    "authorization"
                        | "host"
                        | "x-amz-date"
                        | "x-amz-content-sha256"
                        | "x-amz-security-token"
                )
            {
                continue;
            }
            if drop_entity_headers
                && matches!(
                    name,
                    reqwest::header::CONTENT_TYPE
                        | reqwest::header::CONTENT_LENGTH
                        | reqwest::header::TRANSFER_ENCODING
                )
            {
                continue;
            }
            // User-supplied secrets are not limited to Authorization: a common
            // API key header must not follow a redirect to another origin.
            if same_origin {
                has_content_type |= name == reqwest::header::CONTENT_TYPE;
                outbound_headers.append(name, value);
            }
        }
        if same_origin {
            redirect_cookie_jar.apply_to_headers(&url, &mut outbound_headers, SystemTime::now())?;
            if !has_content_type
                && !drop_entity_headers
                && let Some(content_type) = request.content_type.as_deref()
            {
                outbound_headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    reqwest::header::HeaderValue::from_str(content_type)
                        .map_err(|error| format!("invalid content type: {error}"))?,
                );
            }
            if let Some(signing) = request.aws_sigv4.as_ref() {
                apply_aws_sigv4(
                    &mut outbound_headers,
                    signing,
                    &method,
                    &url,
                    body.as_deref().unwrap_or_default(),
                    SystemTime::now(),
                )?;
            }
        }
        let mut outbound = client
            .request(method.clone(), url.clone())
            .headers(outbound_headers);
        if same_origin && let Some(body) = body.as_ref() {
            outbound = outbound.body(body.clone());
        }
        let response =
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), outbound.send())
                .await
                .map_err(|_| "API request exceeded its deadline".to_string())?
                .map_err(|error| format!("API request failed: {}", error.without_url()))?;
        let redirect_status = response.status().as_u16();
        if redirect < request.max_redirects {
            let Some(transition) = redirect_transition(redirect_status, &method) else {
                return api_workbench_response(
                    response,
                    url,
                    started,
                    request.response_limit_bytes,
                    redirects,
                    dns_duration,
                    deadline,
                    redirect_cookie_mutations,
                )
                .await;
            };
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "API redirect has no valid Location".to_string())?;
            let next = url
                .join(location)
                .map_err(|error| format!("invalid API redirect URL: {error}"))?;
            capture_redirect_cookies(
                response.headers(),
                &url,
                &mut redirect_cookie_jar,
                &mut redirect_cookie_mutations,
            );
            redirects.push(ApiWorkbenchRedirect {
                status: redirect_status,
                from: url.to_string(),
                to: next.to_string(),
                method: method.as_str().to_string(),
                cross_origin: origin_of(&url) != origin_of(&next),
            });
            method = transition.method;
            if !transition.preserve_entity {
                body = None;
                drop_entity_headers = true;
            }
            url = next;
            continue;
        }
        return api_workbench_response(
            response,
            url,
            started,
            request.response_limit_bytes,
            redirects,
            dns_duration,
            deadline,
            redirect_cookie_mutations,
        )
        .await;
    }
    Err("API request made no request".into())
}

#[allow(clippy::too_many_arguments)]
async fn api_workbench_response(
    mut response: reqwest::Response,
    url: reqwest::Url,
    started: Instant,
    response_limit_bytes: usize,
    redirects: Vec<ApiWorkbenchRedirect>,
    dns_duration: Duration,
    deadline: Instant,
    redirect_cookie_mutations: Vec<(reqwest::Url, String)>,
) -> Result<ApiWorkbenchResponse, String> {
    let response_limit_bytes = (response_limit_bytes != 0).then_some(response_limit_bytes);
    let status = response.status().as_u16();
    let reason = response
        .status()
        .canonical_reason()
        .unwrap_or_default()
        .to_string();
    let http_version = match response.version() {
        reqwest::Version::HTTP_09 => "HTTP/0.9",
        reqwest::Version::HTTP_10 => "HTTP/1.0",
        reqwest::Version::HTTP_11 => "HTTP/1.1",
        reqwest::Version::HTTP_2 => "HTTP/2",
        reqwest::Version::HTTP_3 => "HTTP/3",
        _ => "unknown",
    }
    .to_string();
    let first_byte_ms = elapsed_millis(started.elapsed());
    let final_set_cookies: Vec<String> = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect();
    // Inspection metadata is value-free; redirect mutations themselves retain
    // their source URL in the typed wire field below.
    let mut cookies = redirect_cookie_mutations
        .iter()
        .filter_map(|(source, header)| redacted_cookie_metadata(header, source))
        .collect::<Vec<_>>();
    cookies.extend(
        final_set_cookies
            .iter()
            .filter_map(|header| redacted_cookie_metadata(header, &url)),
    );
    let cookie_mutations = redirect_cookie_mutations
        .into_iter()
        .map(|(source, header)| ApiWorkbenchCookieMutation {
            source_url: source.to_string(),
            header: redirect_cookie_mutation(&source, &header),
        })
        .collect::<Vec<_>>();
    let set_cookies = final_set_cookies;
    let headers = response_headers(response.headers());
    let declared_length = response.content_length();
    let mut body = Vec::new();
    let mut truncated = false;
    let mut received_bytes = 0_u64;
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    let download_started = Instant::now();
    let complete = loop {
        let chunk = match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            response.chunk(),
        )
        .await
        {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(_)) if response_limit_bytes == Some(body.len()) => {
                // reqwest's client deadline may surface as a body error before
                // the outer timeout. Once the retained prefix is full, keep
                // it inspectable with unknown completeness.
                truncated = true;
                break false;
            }
            Ok(Err(error)) => return Err(format!("read API response: {error}")),
            Err(_) if response_limit_bytes == Some(body.len()) => {
                // The retained prefix is still inspectable. A timeout while
                // probing for EOF proves neither completeness nor a digest.
                truncated = true;
                break false;
            }
            Err(_) => return Err("API response exceeded its deadline".into()),
        };
        let Some(chunk) = chunk else { break true };
        received_bytes = received_bytes.saturating_add(chunk.len() as u64);
        digest.update(&chunk);
        let remaining = response_limit_bytes
            .map(|limit| limit.saturating_sub(body.len()))
            .unwrap_or(chunk.len());
        let stored = remaining.min(chunk.len());
        body.extend_from_slice(&chunk[..stored]);
        if stored < chunk.len() {
            truncated = true;
            break false;
        }
        if response_limit_bytes == Some(body.len())
            && declared_length.is_some_and(|length| length > received_bytes)
        {
            truncated = true;
            break false;
        }
        // At exactly the retention ceiling, an unknown/equal declared length
        // is not proof of EOF. Loop once more: EOF yields a complete digest;
        // another chunk or the absolute deadline yields a truncated prefix.
    };
    let stored_bytes = body.len() as u64;
    let full_body_sha256 = complete.then(|| hex_lower(&digest.finalize()));
    let duration_ms = elapsed_millis(started.elapsed());
    Ok(ApiWorkbenchResponse {
        status,
        reason,
        headers,
        set_cookies,
        cookie_mutations,
        body,
        final_url: url.to_string(),
        http_version,
        received_bytes,
        stored_bytes,
        full_body_sha256,
        timings: ApiWorkbenchTimings {
            dns_ms: elapsed_millis(dns_duration),
            first_byte_ms,
            download_ms: elapsed_millis(download_started.elapsed()),
        },
        cookies,
        duration_ms,
        truncated,
        redirects,
    })
}

fn elapsed_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

const MAX_REDIRECT_COOKIES: usize = 256;
const MAX_SET_COOKIE_BYTES: usize = 16 * 1024;

#[derive(Debug)]
struct RedirectCookie {
    name: String,
    /// `None` is an in-operation deletion tombstone. It must survive until the
    /// next hop so an original caller Cookie header cannot resurrect a value
    /// that the redirect response just expired.
    value: Option<String>,
    domain: String,
    path: String,
    host_only: bool,
    secure: bool,
    expires_at: Option<i64>,
}

#[derive(Default)]
struct RedirectCookieJar {
    cookies: Vec<RedirectCookie>,
}

impl RedirectCookieJar {
    /// Apply one Set-Cookie mutation to the transient redirect jar. Invalid or
    /// over-limit fields are ignored: the durable native jar remains the owner
    /// of reporting/import policy, while this jar must never replay malformed
    /// credentials during the current network operation.
    fn store(&mut self, url: &reqwest::Url, header: &str, now: SystemTime) {
        if header.len() > MAX_SET_COOKIE_BYTES || header.contains(['\r', '\n']) {
            return;
        }
        let Some(request_host) = normalized_cookie_host(url) else {
            return;
        };
        let mut fields = header.split(';');
        let Some((name, value)) = fields.next().and_then(|pair| pair.trim().split_once('=')) else {
            return;
        };
        let name = name.trim();
        let value = value.trim();
        if !valid_redirect_cookie_name(name) || !valid_redirect_cookie_value(value) {
            return;
        }

        let mut domain = request_host.clone();
        let mut host_only = true;
        let mut path = cookie_default_path(url.path());
        let mut secure = false;
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
                    let Some(candidate) = attribute_value else {
                        return;
                    };
                    let candidate = candidate
                        .trim_start_matches('.')
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    if candidate.is_empty()
                        || candidate.bytes().any(|byte| {
                            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
                        })
                        || request_host.parse::<IpAddr>().is_ok()
                        || psl::domain(candidate.as_bytes()).is_none()
                        || !cookie_domain_matches(&request_host, &candidate)
                    {
                        return;
                    }
                    domain = candidate;
                    host_only = false;
                }
                "path" => {
                    if let Some(candidate) = attribute_value.filter(|value| value.starts_with('/'))
                    {
                        if candidate.bytes().any(|byte| byte.is_ascii_control()) {
                            return;
                        }
                        path = candidate.to_string();
                    }
                }
                "secure" => secure = true,
                "max-age" => max_age = attribute_value.and_then(|value| value.parse::<i64>().ok()),
                "expires" => {
                    expires_at = attribute_value
                        .and_then(|value| httpdate::parse_http_date(value).ok())
                        .map(system_time_seconds);
                }
                _ => {}
            }
        }
        let now = system_time_seconds(now);
        if let Some(seconds) = max_age {
            expires_at = Some(if seconds <= 0 {
                now
            } else {
                now.saturating_add(seconds)
            });
        }
        let existing = self.cookies.iter().position(|cookie| {
            cookie.name == name && cookie.domain == domain && cookie.path == path
        });
        let cookie = RedirectCookie {
            name: name.to_string(),
            value: (!expires_at.is_some_and(|expiry| expiry <= now)).then(|| value.to_string()),
            domain,
            path,
            host_only,
            secure,
            expires_at: expires_at.filter(|expiry| *expiry > now),
        };
        if let Some(index) = existing {
            self.cookies[index] = cookie;
        } else if self.cookies.len() < MAX_REDIRECT_COOKIES {
            self.cookies.push(cookie);
        }
    }

    fn apply_to_headers(
        &mut self,
        url: &reqwest::Url,
        headers: &mut reqwest::header::HeaderMap,
        now: SystemTime,
    ) -> Result<(), String> {
        let Some(host) = normalized_cookie_host(url) else {
            return Ok(());
        };
        let now = system_time_seconds(now);
        let mut selected = self
            .cookies
            .iter()
            .filter(|cookie| {
                (!cookie.secure || url.scheme() == "https")
                    && if cookie.host_only {
                        host == cookie.domain
                    } else {
                        cookie_domain_matches(&host, &cookie.domain)
                    }
                    && cookie_path_matches(url.path(), &cookie.path)
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        if selected.is_empty() {
            return Ok(());
        }

        let selected_names = selected
            .iter()
            .map(|cookie| cookie.name.as_str())
            .collect::<HashSet<_>>();
        let mut pairs = headers
            .get_all(reqwest::header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| {
                let pair = pair.trim();
                let (name, _) = pair.split_once('=')?;
                (!selected_names.contains(name.trim())).then(|| pair.to_string())
            })
            .collect::<Vec<_>>();
        pairs.extend(selected.into_iter().filter_map(|cookie| {
            cookie
                .value
                .as_ref()
                .filter(|_| cookie.expires_at.is_none_or(|expiry| expiry > now))
                .map(|value| format!("{}={value}", cookie.name))
        }));
        headers.remove(reqwest::header::COOKIE);
        headers.insert(
            reqwest::header::COOKIE,
            reqwest::header::HeaderValue::from_str(&pairs.join("; "))
                .map_err(|error| format!("invalid redirect cookie header: {error}"))?,
        );
        Ok(())
    }
}

fn capture_redirect_cookies(
    headers: &reqwest::header::HeaderMap,
    url: &reqwest::Url,
    jar: &mut RedirectCookieJar,
    mutations: &mut Vec<(reqwest::Url, String)>,
) {
    for header in headers.get_all(reqwest::header::SET_COOKIE) {
        let Ok(header) = header.to_str() else {
            continue;
        };
        jar.store(url, header, SystemTime::now());
        if mutations.len() < MAX_REDIRECT_COOKIES && header.len() <= MAX_SET_COOKIE_BYTES {
            mutations.push((url.clone(), header.to_string()));
        }
    }
}

/// The current response wire associates every mutation with finalUrl. Same-
/// origin redirects have the same host but can have a different path, so make
/// an implicit default path explicit before handing the mutation to the native
/// durable jar. Explicit attributes remain byte-for-byte unchanged.
fn redirect_cookie_mutation(source: &reqwest::Url, header: &str) -> String {
    if header.split(';').skip(1).any(|field| {
        field
            .trim()
            .split_once('=')
            .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("path"))
    }) {
        header.to_string()
    } else {
        format!("{header}; Path={}", cookie_default_path(source.path()))
    }
}

fn normalized_cookie_host(url: &reqwest::Url) -> Option<String> {
    Some(
        url.host_str()?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase(),
    )
}

fn system_time_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

fn valid_redirect_cookie_name(name: &str) -> bool {
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

fn valid_redirect_cookie_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f && byte != b';'))
}

fn cookie_domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn cookie_path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || request_path
            .strip_prefix(cookie_path)
            .is_some_and(|suffix| cookie_path.ends_with('/') || suffix.starts_with('/'))
}

fn redacted_cookie_metadata(header: &str, url: &reqwest::Url) -> Option<ApiWorkbenchCookie> {
    let mut fields = header.split(';');
    let (name, _) = fields.next()?.trim().split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut cookie = ApiWorkbenchCookie {
        name: name.to_string(),
        domain: url.host_str().unwrap_or_default().to_ascii_lowercase(),
        path: cookie_default_path(url.path()),
        secure: false,
        http_only: false,
        same_site: None,
        expires_at: None,
    };
    let mut max_age = None;
    for field in fields {
        let field = field.trim();
        let (attribute, value) = field.split_once('=').unwrap_or((field, ""));
        match attribute.trim().to_ascii_lowercase().as_str() {
            "domain" if !value.trim().is_empty() => {
                cookie.domain = value.trim().trim_start_matches('.').to_ascii_lowercase();
            }
            "path" if value.trim().starts_with('/') => cookie.path = value.trim().to_string(),
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "samesite" => {
                cookie.same_site = match value.trim().to_ascii_lowercase().as_str() {
                    "strict" => Some("Strict".into()),
                    "lax" => Some("Lax".into()),
                    "none" => Some("None".into()),
                    _ => None,
                };
            }
            "max-age" => max_age = value.trim().parse::<i64>().ok(),
            "expires" => {
                cookie.expires_at = httpdate::parse_http_date(value.trim())
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .and_then(|duration| duration.as_secs().try_into().ok());
            }
            _ => {}
        }
    }
    if let Some(seconds) = max_age {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .try_into()
            .unwrap_or(i64::MAX);
        cookie.expires_at = Some(now.saturating_add(seconds));
    }
    Some(cookie)
}

fn cookie_default_path(request_path: &str) -> String {
    if !request_path.starts_with('/') || request_path == "/" {
        return "/".into();
    }
    request_path
        .rsplit_once('/')
        .map_or(
            "/",
            |(prefix, _)| if prefix.is_empty() { "/" } else { prefix },
        )
        .to_string()
}

fn validate_aws_sigv4(signing: &ApiWorkbenchAwsSigV4) -> Result<(), String> {
    if signing.access_key.trim().is_empty() || signing.secret_key.is_empty() {
        return Err("AWS SigV4 requires an access key and secret key".into());
    }
    for (label, value) in [
        ("region", signing.region.trim()),
        ("service", signing.service.trim()),
    ] {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(format!(
                "AWS SigV4 {label} must contain only letters, digits, '-' or '_'"
            ));
        }
    }
    if signing
        .session_token
        .as_deref()
        .is_some_and(|token| token.trim().is_empty())
    {
        return Err("AWS SigV4 session token cannot be blank".into());
    }
    Ok(())
}

fn apply_aws_sigv4(
    headers: &mut reqwest::header::HeaderMap,
    signing: &ApiWorkbenchAwsSigV4,
    method: &reqwest::Method,
    url: &reqwest::Url,
    body: &[u8],
    now: SystemTime,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};

    let seconds: i64 = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_string())?
        .as_secs()
        .try_into()
        .map_err(|_| "system clock is outside the AWS timestamp range".to_string())?;
    let rfc3339 = chrono::DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| "system clock is outside the AWS timestamp range".to_string())?
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let amz_date = format!(
        "{}{}{}T{}{}{}Z",
        &rfc3339[0..4],
        &rfc3339[5..7],
        &rfc3339[8..10],
        &rfc3339[11..13],
        &rfc3339[14..16],
        &rfc3339[17..19]
    );
    let date = &amz_date[..8];
    let host = aws_host(url)?;
    let payload_hash = hex_lower(&Sha256::digest(body));
    headers.insert(
        reqwest::header::HeaderName::from_static("x-amz-date"),
        reqwest::header::HeaderValue::from_str(&amz_date)
            .map_err(|error| format!("invalid AWS signing date: {error}"))?,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("x-amz-content-sha256"),
        reqwest::header::HeaderValue::from_str(&payload_hash)
            .map_err(|error| format!("invalid AWS payload hash: {error}"))?,
    );
    if let Some(session_token) = signing.session_token.as_deref() {
        headers.insert(
            reqwest::header::HeaderName::from_static("x-amz-security-token"),
            reqwest::header::HeaderValue::from_str(session_token)
                .map_err(|error| format!("invalid AWS session token: {error}"))?,
        );
    }
    let (canonical_headers, signed_headers) = aws_canonical_headers(headers, &host)?;
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.as_str(),
        aws_canonical_path(url.path(), signing.service.trim()),
        aws_canonical_query(url),
        canonical_headers,
        signed_headers,
        payload_hash,
    );
    let scope = format!(
        "{date}/{}/{}/aws4_request",
        signing.region.trim(),
        signing.service.trim()
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex_lower(&Sha256::digest(canonical_request.as_bytes()))
    );
    let date_key = hmac_sha256(
        format!("AWS4{}", signing.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let region_key = hmac_sha256(&date_key, signing.region.trim().as_bytes());
    let service_key = hmac_sha256(&region_key, signing.service.trim().as_bytes());
    let signing_key = hmac_sha256(&service_key, b"aws4_request");
    let signature = hex_lower(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        signing.access_key.trim()
    );
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&authorization)
            .map_err(|error| format!("invalid AWS authorization header: {error}"))?,
    );
    Ok(())
}

fn aws_canonical_headers(
    headers: &reqwest::header::HeaderMap,
    host: &str,
) -> Result<(String, String), String> {
    let mut canonical = BTreeMap::<String, Vec<String>>::new();
    canonical.insert("host".into(), vec![normalize_aws_header_value(host)]);
    for name in headers.keys() {
        let name = name.as_str();
        if name != "content-type" && !name.starts_with("x-amz-") {
            continue;
        }
        let values = headers
            .get_all(name)
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map(normalize_aws_header_value)
                    .map_err(|error| format!("AWS signed header {name:?} is not text: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        canonical.insert(name.to_string(), values);
    }
    let signed_headers = canonical.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers = canonical
        .into_iter()
        .map(|(name, values)| format!("{name}:{}\n", values.join(",")))
        .collect::<String>();
    Ok((canonical_headers, signed_headers))
}

fn normalize_aws_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn aws_host(url: &reqwest::Url) -> Result<String, String> {
    let host = url
        .host()
        .ok_or_else(|| "AWS SigV4 URL has no host".to_string())?
        .to_string();
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn aws_canonical_path(path: &str, service: &str) -> String {
    // AWS services normally apply a second URI-encoding pass to the already
    // encoded request path. S3 is the deliberate exception: object keys are
    // signed without path normalization or double encoding.
    let double_encode = !service.eq_ignore_ascii_case("s3");
    let mut encoded = String::with_capacity(path.len().max(1));
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            if double_encode {
                encoded.push_str("%25");
            } else {
                encoded.push('%');
            }
            encoded.push(char::from(bytes[index + 1]).to_ascii_uppercase());
            encoded.push(char::from(bytes[index + 2]).to_ascii_uppercase());
            index += 3;
            continue;
        }
        if byte == b'/' || byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
        index += 1;
    }
    if encoded.is_empty() {
        "/".into()
    } else {
        encoded
    }
}

fn aws_canonical_query(url: &reqwest::Url) -> String {
    let mut pairs = url
        .query()
        .into_iter()
        .flat_map(|query| query.split('&'))
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                aws_uri_encode(&aws_percent_decode(name.as_bytes())),
                aws_uri_encode(&aws_percent_decode(value.as_bytes())),
            )
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn aws_percent_decode(value: &[u8]) -> Vec<u8> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%'
            && index + 2 < value.len()
            && let (Some(high), Some(low)) = (digit(value[index + 1]), digit(value[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
            continue;
        }
        // Unlike application/x-www-form-urlencoded, AWS treats '+' as a
        // literal plus byte and therefore canonicalizes it to %2B.
        decoded.push(value[index]);
        index += 1;
    }
    decoded
}

fn aws_uri_encode(value: &[u8]) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut key = if key.len() > 64 {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    key.resize(64, 0);
    let mut inner_key = [0x36_u8; 64];
    let mut outer_key = [0x5c_u8; 64];
    for (index, byte) in key.into_iter().enumerate() {
        inner_key[index] ^= byte;
        outer_key[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(value);
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn hex_lower(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn origin_of(url: &reqwest::Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_string(),
        url.host_str().unwrap_or_default().to_string(),
        url.port_or_known_default(),
    )
}

/// Do not forward server-set cookies or arbitrary diagnostic headers through a
/// local UI response. These are the stable response facts useful to inspection.
fn response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        // Set-Cookie has its own transient channel into the native cookie jar.
        // Authentication challenges are inspection data, not credentials, and
        // must remain visible so a 401/407 can be diagnosed correctly.
        .filter(|(name, _)| name.as_str() != "set-cookie")
        .map(|(name, value)| {
            let value = if let Ok(value) = value.to_str() {
                value.to_string()
            } else {
                format!(
                    "base64:{}",
                    base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
                )
            };
            (name.to_string(), value)
        })
        .collect()
}

/// Exchange OAuth credentials for a transient Workbench token. The caller is
/// responsible for placing returned secrets in the native vault; this service
/// never logs or persists the request or response.
pub async fn exchange_workbench_oauth_token(
    request: WorkbenchOAuthRequest,
) -> Result<WorkbenchOAuthResponse, String> {
    const MAX_TOKEN_RESPONSE_BYTES: usize = 256 * 1024;

    if request.version != 1 {
        return Err(format!(
            "unsupported Workbench OAuth version {}",
            request.version
        ));
    }
    if request.client_id.trim().is_empty() {
        return Err("OAuth clientId is required".into());
    }
    validate_http_step_url(&request.token_url, request.allow_private_network)?;
    let url = reqwest::Url::parse(&request.token_url)
        .map_err(|error| format!("invalid OAuth token URL: {error}"))?;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    for (name, value) in &request.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "invalid OAuth request header name".to_string())?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| "invalid OAuth request header value".to_string())?;
        headers.insert(name, value);
    }
    let mut form = Vec::<(String, String)>::new();
    let retained_refresh_token = matches!(request.flow, WorkbenchOAuthFlow::RefreshToken)
        .then(|| request.refresh_token.clone())
        .flatten();
    match request.flow {
        WorkbenchOAuthFlow::ClientCredentials => {
            form.push(("grant_type".into(), "client_credentials".into()));
            if request
                .client_secret
                .as_deref()
                .unwrap_or_default()
                .is_empty()
            {
                return Err("OAuth client credentials require clientSecret".into());
            }
        }
        WorkbenchOAuthFlow::AuthorizationCode => {
            let code = request
                .code
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth authorization-code exchange requires code".to_string())?;
            let redirect_uri = request
                .redirect_uri
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "OAuth authorization-code exchange requires redirectUri".to_string()
                })?;
            let verifier = request
                .code_verifier
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth PKCE exchange requires codeVerifier".to_string())?;
            let expected_state = request
                .expected_state
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth authorization callback requires expectedState".to_string())?;
            let callback_state = request
                .callback_state
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth authorization callback requires callbackState".to_string())?;
            if !constant_time_bytes_equal(expected_state.as_bytes(), callback_state.as_bytes()) {
                return Err("OAuth authorization callback state does not match".into());
            }
            let redirect = reqwest::Url::parse(redirect_uri)
                .map_err(|error| format!("invalid OAuth redirectUri: {error}"))?;
            if !redirect.username().is_empty()
                || redirect.password().is_some()
                || redirect.fragment().is_some()
            {
                return Err("OAuth redirectUri must not contain credentials or a fragment".into());
            }
            if !(43..=128).contains(&verifier.len())
                || !verifier.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
                })
            {
                return Err(
                    "OAuth PKCE codeVerifier must be 43-128 unreserved ASCII characters".into(),
                );
            }
            form.extend([
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), code.into()),
                ("redirect_uri".into(), redirect_uri.into()),
                ("code_verifier".into(), verifier.into()),
            ]);
        }
        WorkbenchOAuthFlow::RefreshToken => {
            let token = request
                .refresh_token
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth refresh requires refreshToken".to_string())?;
            form.extend([
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), token.into()),
            ]);
        }
        WorkbenchOAuthFlow::Password => {
            let user = request
                .username
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth password grant requires username".to_string())?;
            let secret = request
                .password
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth password grant requires password".to_string())?;
            form.extend([
                ("grant_type".into(), "password".into()),
                ("username".into(), user.into()),
                ("password".into(), secret.into()),
            ]);
        }
    }
    let use_basic = matches!(
        request.credentials_in,
        WorkbenchOAuthCredentialsIn::ClientSecretBasic
    );
    if use_basic
        && request
            .client_secret
            .as_deref()
            .unwrap_or_default()
            .is_empty()
    {
        return Err("OAuth client_secret_basic requires clientSecret".into());
    }
    // Resolve only after every local callback/credential invariant passes.
    // A forged state must not trigger even a DNS lookup.
    let addresses = resolve_http_step_addresses(&url, request.allow_private_network).await?;
    let timeout = Duration::from_millis(request.timeout_ms.clamp(100, 120_000));
    let client = pinned_http_client(&url, &addresses, timeout, "OAuth token")?;
    let mut outbound = client.post(url).headers(headers);
    if use_basic {
        outbound = outbound.basic_auth(&request.client_id, request.client_secret.as_deref());
    } else {
        form.push(("client_id".into(), request.client_id));
        if let Some(secret) = request.client_secret.filter(|value| !value.is_empty()) {
            form.push(("client_secret".into(), secret));
        }
    }
    if let Some(scope) = request.scope.filter(|value| !value.trim().is_empty()) {
        form.push(("scope".into(), scope));
    }
    if let Some(audience) = request.audience.filter(|value| !value.trim().is_empty()) {
        form.push(("audience".into(), audience));
    }
    let mut response = outbound
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("OAuth token request failed: {}", error.without_url()))?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read OAuth token response: {error}"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
            return Err("OAuth token response exceeds the 262144-byte limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    let parsed = serde_json::from_slice::<Value>(&body);
    if !status.is_success() {
        // Report the HTTP status first: an ingress, WAF, or redirect answers
        // with an empty or HTML body, and "not valid JSON" would hide the
        // status that actually explains the refusal.
        let detail = parsed
            .as_ref()
            .ok()
            .and_then(|value| {
                value["error_description"]
                    .as_str()
                    .or_else(|| value["error"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| describe_non_json_body(&body, response.headers()));
        return Err(format!("OAuth token request returned {status}: {detail}"));
    }
    let value = parsed.map_err(|_| {
        format!(
            "OAuth token response ({status}) is not valid JSON: {}",
            describe_non_json_body(&body, response.headers())
        )
    })?;
    let access_token = value["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "OAuth token response contains no access_token".to_string())?
        .to_string();
    Ok(WorkbenchOAuthResponse {
        access_token,
        token_type: value["token_type"].as_str().unwrap_or("Bearer").to_string(),
        expires_in: value["expires_in"].as_u64(),
        refresh_token: value["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or(retained_refresh_token),
        scope: value["scope"].as_str().map(str::to_string),
        id_token: value["id_token"].as_str().map(str::to_string),
    })
}

/// Summarize a token-endpoint body that carried no OAuth error object so the
/// user sees what the server actually answered: nothing, a redirect target, or
/// the first line of an HTML/text error page.
fn describe_non_json_body(body: &[u8], headers: &reqwest::header::HeaderMap) -> String {
    if let Some(location) = headers
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
    {
        return format!("redirect to {location} (redirects are not followed)");
    }
    if body.is_empty() {
        return "empty body".into();
    }
    let text = String::from_utf8_lossy(body);
    let snippet: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();
    let kind = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown content type");
    format!("{} bytes of {kind}: {snippet}", body.len())
}

fn constant_time_bytes_equal(left: &[u8], right: &[u8]) -> bool {
    let mut different = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or_default();
        let b = right.get(index).copied().unwrap_or_default();
        different |= usize::from(a ^ b);
    }
    different == 0
}

fn validate_http_step_url(raw: &str, allow_private: bool) -> Result<(), String> {
    let url =
        reqwest::Url::parse(raw).map_err(|error| format!("invalid HTTP step URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("HTTP step URL must use http:// or https://".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("HTTP step URL must not contain credentials".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "HTTP step URL has no host".to_string())?;
    if !allow_private {
        if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
            return Err(
                "HTTP step URL resolves to localhost; enable private-network access explicitly"
                    .into(),
            );
        }
        // `Url::host_str` retains brackets around IPv6 literals. Strip only
        // that URL syntax before applying the same address policy used after
        // DNS resolution.
        if let Ok(ip) = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            && !globally_routable_ip(ip)
        {
            return Err("HTTP step URL targets a private, loopback, or link-local address".into());
        }
    }
    Ok(())
}

/// Resolve once, validate every candidate, and return exactly the addresses
/// that an outbound client is allowed to connect to. Callers at an SSRF
/// boundary must pin this set into reqwest rather than permitting a second DNS
/// lookup between policy validation and connection establishment.
async fn resolve_http_step_addresses(
    url: &reqwest::Url,
    allow_private: bool,
) -> Result<Vec<SocketAddr>, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "HTTP step URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "HTTP step URL has no port".to_string())?;
    let addresses = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| "resolve HTTP step host: DNS lookup timed out".to_string())?
    .map_err(|error| format!("resolve HTTP step host: {error}"))?
    .collect::<Vec<_>>();
    validate_resolved_http_step_addresses(&addresses, allow_private)?;
    Ok(addresses)
}

async fn resolve_http_step_addresses_before(
    url: &reqwest::Url,
    allow_private: bool,
    deadline: Instant,
    label: &str,
) -> Result<Vec<SocketAddr>, String> {
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        resolve_http_step_addresses(url, allow_private),
    )
    .await
    .map_err(|_| format!("{label} exceeded its deadline while resolving DNS"))?
}

fn validate_resolved_http_step_addresses(
    addresses: &[SocketAddr],
    allow_private: bool,
) -> Result<(), String> {
    if addresses.is_empty() {
        return Err("HTTP step host resolved to no addresses".into());
    }
    if !allow_private
        && addresses
            .iter()
            .any(|address| !globally_routable_ip(address.ip()))
    {
        return Err("HTTP step host resolves to a private, loopback, or link-local address".into());
    }
    Ok(())
}

fn pinned_http_client(
    url: &reqwest::Url,
    validated_addresses: &[SocketAddr],
    timeout: Duration,
    label: &str,
) -> Result<reqwest::Client, String> {
    let host = url
        .host_str()
        .ok_or_else(|| format!("{label} URL has no host"))?;
    if validated_addresses.is_empty() {
        return Err(format!("{label} host resolved to no addresses"));
    }
    let tls = crate::tls::client_config().map_err(|e| format!("{label}: {e}"))?;
    let mut builder = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        // Identify the client the way every other API tool does: ingress
        // gateways and WAF rule sets commonly refuse requests that carry no
        // User-Agent at all. A request-level header still takes precedence.
        .user_agent(concat!("Switchyard/", env!("CARGO_PKG_VERSION")))
        // A configured HTTP proxy would resolve the hostname again and bypass
        // the validated address set, so native SSRF-boundary traffic connects
        // directly.
        .no_proxy();
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, validated_addresses);
    }
    builder
        .build()
        .map_err(|error| format!("build {label} client: {error}"))
}

fn globally_routable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[..3] == [192, 0, 0])
                || (octets[..3] == [192, 88, 99])
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || octets[0] >= 240)
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            // Global unicast is 2000::/3. Starting from that positive
            // classification automatically excludes site-local FEC0::/10,
            // ULA, link-local, multicast, and every future special-use range
            // outside the globally routed block.
            (segments[0] & 0xe000) == 0x2000
                && segments[..2] != [0x2001, 0x0db8]
                // 3fff::/20 is the IETF documentation prefix and is not
                // globally routable despite sitting inside 2000::/3.
                && !(segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
                // IPv4 translation/tunnel prefixes can otherwise hide a
                // private IPv4 destination behind a globally shaped IPv6
                // literal or DNS answer. Refuse the special-use prefix as a
                // whole rather than trusting the translator to enforce its
                // RFC's intended IPv4 scope.
                && !(segments[0] == 0x2001 && segments[1] <= 0x01ff)
                && segments[0] != 0x2002
                && ip
                    .to_ipv4()
                    .is_none_or(|mapped| globally_routable_ip(mapped.into()))
        }
    }
}
