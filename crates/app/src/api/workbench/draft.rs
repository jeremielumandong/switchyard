//! Conversion between the compact GPUI editors and the typed Workbench domain.

use std::collections::BTreeMap;
use switchyard_api::vault::{parse_vault_expression, vault_reference_expression};

#[cfg(test)]
pub use switchyard_api::runtime::secrets::compile_context;
pub use switchyard_api::runtime::secrets::{
    DraftSecrets, parse_cookie_pairs, parse_data_rows, parse_key_value_rows,
    parse_session_variables,
};
use switchyard_api::{
    ApiKeyLocation, AuthConfig, BasicLoginCredentials, Body, CollectionId, HttpMethod, KeyValueRow,
    MultipartRow, MultipartValue, RawBodyKind, RequestId, RequestSettings, SavedRequest, Scripts,
    SecretRef,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BodyMode {
    #[default]
    None,
    Json,
    Xml,
    Text,
    Form,
    Multipart,
    Binary,
    GraphQl,
}

impl BodyMode {
    pub const ALL: [Self; 8] = [
        Self::None,
        Self::Json,
        Self::Xml,
        Self::Text,
        Self::Form,
        Self::Multipart,
        Self::Binary,
        Self::GraphQl,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Text => "text",
            Self::Form => "form",
            Self::Multipart => "multipart",
            Self::Binary => "file",
            Self::GraphQl => "GraphQL",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AuthMode {
    Inherit,
    #[default]
    None,
    ApiKeyHeader,
    ApiKeyQuery,
    Basic,
    Bearer,
    OAuth2,
    AwsSigV4,
    Login,
}

impl AuthMode {
    pub const ALL: [Self; 9] = [
        Self::Inherit,
        Self::None,
        Self::ApiKeyHeader,
        Self::ApiKeyQuery,
        Self::Basic,
        Self::Bearer,
        Self::OAuth2,
        Self::AwsSigV4,
        Self::Login,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::None => "none",
            Self::ApiKeyHeader => "API key header",
            Self::ApiKeyQuery => "API key query",
            Self::Basic => "Basic",
            Self::Bearer => "Bearer",
            Self::OAuth2 => "OAuth 2",
            Self::AwsSigV4 => "AWS SigV4",
            Self::Login => "Sign-in request",
        }
    }
}

/// How a typed auth field is edited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthFieldKind {
    /// Plain single-line text.
    Text,
    /// Masked; a blank field on a saved auth keeps the vault reference.
    Secret,
    /// Multi-line text — always the last `key=` in the serialized editor.
    Multiline,
    /// Header rows edited as multiline `Name: value` text.
    Headers,
    /// One of a fixed set of values, rendered as chips.
    Choice(&'static [&'static str]),
}

/// One field of a typed auth form. The forms render from this table and
/// serialize back into the `key=value` text `parse_auth` reads, so the two
/// can never disagree about which keys exist.
#[derive(Clone, Copy, Debug)]
pub struct AuthFieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: AuthFieldKind,
    pub placeholder: &'static str,
    /// OAuth 2 only: the `flow` values this field belongs to (empty = all).
    pub flows: &'static [&'static str],
}

const fn field(
    key: &'static str,
    label: &'static str,
    kind: AuthFieldKind,
    placeholder: &'static str,
) -> AuthFieldSpec {
    AuthFieldSpec {
        key,
        label,
        kind,
        placeholder,
        flows: &[],
    }
}

const fn flow_field(
    key: &'static str,
    label: &'static str,
    kind: AuthFieldKind,
    placeholder: &'static str,
    flows: &'static [&'static str],
) -> AuthFieldSpec {
    AuthFieldSpec {
        key,
        label,
        kind,
        placeholder,
        flows,
    }
}

pub const OAUTH_FLOWS: [&str; 4] = [
    "",
    "client_credentials",
    "password",
    "authorization_code_pkce",
];

/// The typed fields for `mode`, in display order.
pub fn auth_fields(mode: AuthMode) -> &'static [AuthFieldSpec] {
    use AuthFieldKind::*;
    match mode {
        AuthMode::Inherit | AuthMode::None => &[],
        AuthMode::ApiKeyHeader => {
            const F: &[AuthFieldSpec] = &[
                field("name", "Header name", Text, ""),
                field("value", "Value", Secret, ""),
            ];
            F
        }
        AuthMode::ApiKeyQuery => {
            const F: &[AuthFieldSpec] = &[
                field("name", "Query parameter", Text, ""),
                field("value", "Value", Secret, ""),
            ];
            F
        }
        AuthMode::Basic => {
            const F: &[AuthFieldSpec] = &[
                field("username", "Username", Text, ""),
                field("password", "Password", Secret, ""),
                field(
                    "auth_url",
                    "Auth URL (optional)",
                    Text,
                    "https://auth.example.com/login",
                ),
                field("method", "Method", Choice(&["POST", "GET"]), ""),
                field("token_path", "Token path in response", Text, "access_token"),
                field("ttl_secs", "Session lifetime (seconds)", Text, "3600"),
                field(
                    "headers",
                    "Sign-in headers",
                    Headers,
                    "X-Tenant: {{tenant}}",
                ),
            ];
            F
        }
        AuthMode::Bearer => {
            const F: &[AuthFieldSpec] = &[field("token", "Token", Secret, "")];
            F
        }
        AuthMode::OAuth2 => {
            const F: &[AuthFieldSpec] = &[
                field("flow", "Flow", Choice(&OAUTH_FLOWS), ""),
                flow_field("token", "Token", Secret, "", &[""]),
                flow_field(
                    "authorization_endpoint",
                    "Authorization endpoint",
                    Text,
                    "https://id.example.com/authorize",
                    &["authorization_code_pkce"],
                ),
                flow_field(
                    "token_endpoint",
                    "Token endpoint",
                    Text,
                    "https://id.example.com/token",
                    &["client_credentials", "password", "authorization_code_pkce"],
                ),
                flow_field(
                    "client_id",
                    "Client id",
                    Text,
                    "",
                    &["client_credentials", "password", "authorization_code_pkce"],
                ),
                flow_field(
                    "client_secret",
                    "Client secret",
                    Secret,
                    "",
                    &["client_credentials"],
                ),
                flow_field(
                    "client_secret",
                    "Client secret (optional)",
                    Secret,
                    "",
                    &["password"],
                ),
                flow_field("username", "Username", Text, "", &["password"]),
                flow_field("password", "Password", Secret, "", &["password"]),
                flow_field(
                    "redirect_uri",
                    "Redirect URI",
                    Text,
                    "http://127.0.0.1:18765/callback",
                    &["authorization_code_pkce"],
                ),
                flow_field(
                    "scopes",
                    "Scopes",
                    Text,
                    "read write",
                    &["client_credentials", "password", "authorization_code_pkce"],
                ),
                flow_field(
                    "headers",
                    "Token request headers",
                    Headers,
                    "X-Tenant: {{tenant}}",
                    &["client_credentials", "password", "authorization_code_pkce"],
                ),
            ];
            F
        }
        AuthMode::AwsSigV4 => {
            const F: &[AuthFieldSpec] = &[
                field("access_key", "Key id", Secret, ""),
                field("secret_key", "Signing key", Secret, ""),
                field("session_token", "Session (optional)", Secret, ""),
                field("region", "Region", Text, "us-east-1"),
                field("service", "Service", Text, "execute-api"),
            ];
            F
        }
        AuthMode::Login => {
            const F: &[AuthFieldSpec] = &[
                field("url", "Sign-in URL", Text, "https://api.example.com/login"),
                field("method", "Method", Choice(&["POST", "GET"]), ""),
                field("token_path", "Token path in response", Text, "data.token"),
                field("ttl_secs", "Session lifetime (seconds)", Text, "3600"),
                field(
                    "headers",
                    "Sign-in headers",
                    Headers,
                    "X-Tenant: {{tenant}}",
                ),
                field(
                    "body",
                    "JSON body",
                    Multiline,
                    "{\"user\": \"{{login_user}}\", \"pass\": \"{{login_pass}}\"}",
                ),
            ];
            F
        }
    }
}

/// The `key=value` editor text for a saved auth: every plain field, so a
/// reloaded form shows what was configured while the masked ones keep their
/// vault reference (see `parse_auth`).
pub fn format_auth(auth: &AuthConfig) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let AuthConfig::Login { headers, .. }
    | AuthConfig::OAuth2ClientCredentials { headers, .. }
    | AuthConfig::OAuth2Password { headers, .. }
    | AuthConfig::OAuth2AuthorizationCodePkce { headers, .. } = auth
        && !headers.is_empty()
    {
        let text = headers
            .iter()
            .map(|row| {
                format!(
                    "{}{}: {}",
                    if row.enabled { "" } else { "# " },
                    row.key,
                    row.value
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        lines.push(format!("headers={}", serde_json::Value::from(text)));
    }
    let mut push = |key: &str, value: &str| {
        if !value.is_empty() {
            lines.push(format!("{key}={value}"));
        }
    };
    let references = match auth {
        AuthConfig::ApiKey { value, .. } => vec![("value", value)],
        AuthConfig::Basic { password, .. } => vec![("password", password)],
        AuthConfig::Bearer { token } | AuthConfig::OAuth2 { token } => vec![("token", token)],
        AuthConfig::OAuth2ClientCredentials { client_secret, .. } => {
            vec![("client_secret", client_secret)]
        }
        AuthConfig::OAuth2Password {
            password,
            client_secret,
            ..
        } => {
            let mut fields = vec![("password", password)];
            fields.extend(
                client_secret
                    .iter()
                    .map(|reference| ("client_secret", reference)),
            );
            fields
        }
        AuthConfig::AwsSigV4 {
            access_key,
            secret_key,
            session_token,
            ..
        } => {
            let mut fields = vec![("access_key", access_key), ("secret_key", secret_key)];
            fields.extend(
                session_token
                    .iter()
                    .map(|reference| ("session_token", reference)),
            );
            fields
        }
        AuthConfig::Login {
            basic: Some(basic), ..
        } => vec![("password", &basic.password)],
        _ => Vec::new(),
    };
    for (name, reference) in references {
        if let Some(expression) = vault_reference_expression(reference) {
            push(name, &expression);
        }
    }
    match auth {
        AuthConfig::AwsSigV4 {
            region, service, ..
        } => {
            push("region", region);
            push("service", service);
        }
        AuthConfig::ApiKey {
            name: parameter, ..
        } => push("name", parameter),
        AuthConfig::Inherit
        | AuthConfig::None
        | AuthConfig::Bearer { .. }
        | AuthConfig::OAuth2 { .. }
        | AuthConfig::Unsupported { .. } => {}
        AuthConfig::Basic { username, .. } => push("username", username),
        AuthConfig::OAuth2ClientCredentials {
            token_endpoint,
            client_id,
            scopes,
            ..
        } => {
            push("flow", "client_credentials");
            push("token_endpoint", token_endpoint);
            push("client_id", client_id);
            push("scopes", &scopes.join(" "));
        }
        AuthConfig::OAuth2Password {
            token_endpoint,
            client_id,
            username,
            scopes,
            ..
        } => {
            push("flow", "password");
            push("token_endpoint", token_endpoint);
            push("client_id", client_id);
            push("username", username);
            push("scopes", &scopes.join(" "));
        }
        AuthConfig::OAuth2AuthorizationCodePkce {
            authorization_endpoint,
            token_endpoint,
            client_id,
            scopes,
            redirect_uri,
            ..
        } => {
            push("flow", "authorization_code_pkce");
            push("authorization_endpoint", authorization_endpoint);
            push("token_endpoint", token_endpoint);
            push("client_id", client_id);
            push("redirect_uri", redirect_uri);
            push("scopes", &scopes.join(" "));
        }
        AuthConfig::Login {
            url,
            method,
            basic,
            body,
            token_path,
            ttl_secs,
            ..
        } => {
            if let Some(basic) = basic {
                push("username", &basic.username);
                push("auth_url", url);
            } else {
                push("url", url);
            }
            push("method", method);
            push("token_path", token_path);
            push(
                "ttl_secs",
                &ttl_secs.map(|ttl| ttl.to_string()).unwrap_or_default(),
            );
            // Always last: everything after `body=` is the body.
            push("body", body);
        }
    }
    lines.join("\n")
}

pub struct DraftInput<'a> {
    pub request_id: Option<RequestId>,
    pub collection_id: CollectionId,
    pub name: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub params: &'a str,
    pub headers: &'a str,
    pub cookies: &'a str,
    pub body_mode: BodyMode,
    pub body: &'a str,
    pub auth_mode: AuthMode,
    pub auth: &'a str,
    pub variables: &'a str,
    pub pre_request_script: &'a str,
    pub test_script: &'a str,
    pub allow_private_network: bool,
}

pub fn build(input: DraftInput<'_>) -> Result<(SavedRequest, DraftSecrets), String> {
    let request_id = input.request_id.unwrap_or_default();
    let secret_scope = format!("request.{}", request_id.as_str());
    let mut headers = parse_header_rows(input.headers)?;
    headers.retain(|row| !row.key.eq_ignore_ascii_case("cookie"));
    // Validate editor syntax here, but leave values to the vault-backed
    // CookieJar at compile time. SavedRequest must never carry plaintext
    // cookies as ordinary headers.
    let _ = parse_cookie_pairs(input.cookies)?;
    let (auth, mut secrets) = parse_auth(input.auth_mode, input.auth, &secret_scope)?;
    let (variables, variable_secrets) = parse_session_variables(input.variables, &secret_scope)?;
    secrets.merge(variable_secrets);
    Ok((
        SavedRequest {
            id: request_id,
            collection_id: input.collection_id,
            folder_id: None,
            name: if input.name.trim().is_empty() {
                "Untitled request".into()
            } else {
                input.name.trim().into()
            },
            method: HttpMethod::new(input.method)?,
            url: input.url.trim().into(),
            params: parse_key_value_rows(input.params, '=')?,
            headers,
            auth,
            body: parse_body(input.body_mode, input.body)?,
            variables,
            scripts: Scripts {
                pre_request: input.pre_request_script.into(),
                tests: input.test_script.into(),
            },
            settings: RequestSettings {
                allow_private_network: input.allow_private_network,
                ..RequestSettings::default()
            },
            extensions: Default::default(),
            sort_key: 0,
        },
        secrets,
    ))
}

/// Number of native-picker selections needed to bind the current upload body.
/// Editor/import path text is intentionally ignored: only the shape of the
/// body decides how many picker grants are required.
pub fn upload_slot_count(mode: BodyMode, input: &str) -> Result<usize, String> {
    match mode {
        BodyMode::Binary => {
            let _ = input;
            Ok(1)
        }
        BodyMode::Multipart => Ok(parse_multipart_rows(input)?
            .into_iter()
            .filter(|row| row.enabled && matches!(row.value, MultipartValue::File(_)))
            .count()),
        _ => Ok(0),
    }
}

fn parse_body(mode: BodyMode, input: &str) -> Result<Body, String> {
    Ok(match mode {
        BodyMode::None => Body::None,
        BodyMode::Json => Body::Raw {
            media_type: RawBodyKind::Json,
            text: input.into(),
        },
        BodyMode::Xml => Body::Raw {
            media_type: RawBodyKind::Xml,
            text: input.into(),
        },
        BodyMode::Text => Body::Raw {
            media_type: RawBodyKind::Text,
            text: input.into(),
        },
        BodyMode::Form => Body::UrlEncoded {
            rows: parse_key_value_rows(input, '=')?,
        },
        BodyMode::Multipart => Body::Multipart {
            rows: parse_multipart_rows(input)?,
        },
        BodyMode::Binary => Body::Binary {
            path: input.trim().into(),
        },
        BodyMode::GraphQl => {
            let parsed = super::body_editor::decode_graphql(input);
            Body::GraphQl {
                query: parsed.query,
                variables: parsed.variables,
            }
        }
    })
}

pub fn parse_auth(
    mode: AuthMode,
    input: &str,
    secret_scope: &str,
) -> Result<(AuthConfig, DraftSecrets), String> {
    // `body=` runs to the end of the text so JSON may span lines.
    let (rows, body) = split_auth_body(input);
    let mut fields: BTreeMap<String, String> = parse_key_value_rows(rows, '=')?
        .into_iter()
        .map(|row| (row.key.to_ascii_lowercase(), row.value))
        .collect();
    if let Some(body) = body {
        fields.insert("body".into(), body.to_string());
    }
    let headers = fields
        .get("headers")
        .map(|text| {
            let text = serde_json::from_str::<String>(text).unwrap_or_else(|_| text.clone());
            parse_header_rows(&text)
        })
        .transpose()?
        .unwrap_or_default();
    let mut secrets = DraftSecrets::default();
    // A blank masked field is the restored representation of a saved value:
    // keep the deterministic reference and let the vault fail closed at send
    // time if nothing was ever stored under it.
    let mut secret = |name: &str, fallback: Option<&str>| -> Result<SecretRef, String> {
        let value = fields
            .get(name)
            .map(String::as_str)
            .or(fallback)
            .unwrap_or_default()
            .trim()
            .to_string();
        if let Some(reference) = parse_vault_expression(&value)? {
            return Ok(reference);
        }
        let reference = SecretRef::new(format!("workbench.{secret_scope}.auth.{name}"))?;
        if !value.is_empty() {
            secrets.insert(&reference, value);
        }
        Ok(reference)
    };
    let auth = match mode {
        AuthMode::Inherit => AuthConfig::Inherit,
        AuthMode::None => AuthConfig::None,
        AuthMode::ApiKeyHeader | AuthMode::ApiKeyQuery => AuthConfig::ApiKey {
            name: fields
                .get("name")
                .cloned()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    "API key authentication needs name=<header or query name>".to_string()
                })?,
            value: secret("value", None)?,
            location: if mode == AuthMode::ApiKeyHeader {
                ApiKeyLocation::Header
            } else {
                ApiKeyLocation::Query
            },
        },
        AuthMode::Basic => {
            let username = fields.get("username").cloned().unwrap_or_default();
            let password = secret("password", None)?;
            if let Some(url) = fields.get("auth_url").filter(|url| !url.trim().is_empty()) {
                AuthConfig::Login {
                    url: url.clone(),
                    headers,
                    method: login_method(&fields)?,
                    basic: Some(BasicLoginCredentials { username, password }),
                    body: String::new(),
                    token_path: fields
                        .get("token_path")
                        .filter(|path| !path.trim().is_empty())
                        .cloned()
                        .unwrap_or_else(|| "access_token".into()),
                    ttl_secs: login_ttl(&fields)?,
                    access_token: None,
                    expires_at: None,
                }
            } else {
                AuthConfig::Basic { username, password }
            }
        }
        AuthMode::Bearer => AuthConfig::Bearer {
            token: secret("token", Some(input.trim().trim_start_matches("Bearer ")))?,
        },
        AuthMode::OAuth2 => match fields.get("flow").map(String::as_str) {
            Some("client_credentials") => AuthConfig::OAuth2ClientCredentials {
                headers,
                token_endpoint: required_auth_field(
                    &fields,
                    "token_endpoint",
                    "OAuth token endpoint",
                )?,
                client_id: required_auth_field(&fields, "client_id", "OAuth client id")?,
                client_secret: secret("client_secret", None)?,
                scopes: oauth_scopes(fields.get("scopes").or_else(|| fields.get("scope"))),
                access_token: optional_secret(&fields, &mut secrets, secret_scope, "token")?,
                expires_at: None,
            },
            Some("password") => AuthConfig::OAuth2Password {
                headers,
                token_endpoint: required_auth_field(
                    &fields,
                    "token_endpoint",
                    "OAuth token endpoint",
                )?,
                client_id: required_auth_field(&fields, "client_id", "OAuth client id")?,
                // `secret` borrows the draft secrets; use it before `optional_secret`.
                password: secret("password", None)?,
                username: required_auth_field(&fields, "username", "OAuth username")?,
                client_secret: optional_secret(
                    &fields,
                    &mut secrets,
                    secret_scope,
                    "client_secret",
                )?,
                scopes: oauth_scopes(fields.get("scopes").or_else(|| fields.get("scope"))),
                access_token: optional_secret(&fields, &mut secrets, secret_scope, "token")?,
                refresh_token: optional_secret(
                    &fields,
                    &mut secrets,
                    secret_scope,
                    "refresh_token",
                )?,
                expires_at: None,
            },
            Some("authorization_code_pkce") => AuthConfig::OAuth2AuthorizationCodePkce {
                headers,
                authorization_endpoint: required_auth_field(
                    &fields,
                    "authorization_endpoint",
                    "OAuth authorization endpoint",
                )?,
                token_endpoint: required_auth_field(
                    &fields,
                    "token_endpoint",
                    "OAuth token endpoint",
                )?,
                client_id: required_auth_field(&fields, "client_id", "OAuth client id")?,
                scopes: oauth_scopes(fields.get("scopes").or_else(|| fields.get("scope"))),
                redirect_uri: required_auth_field(&fields, "redirect_uri", "OAuth redirect URI")?,
                access_token: optional_secret(&fields, &mut secrets, secret_scope, "token")?,
                refresh_token: optional_secret(
                    &fields,
                    &mut secrets,
                    secret_scope,
                    "refresh_token",
                )?,
                expires_at: None,
            },
            Some(other) => {
                return Err(format!(
                    "unsupported OAuth flow {other:?}; use client_credentials, password or authorization_code_pkce"
                ));
            }
            None => AuthConfig::OAuth2 {
                token: secret("token", Some(input.trim().trim_start_matches("Bearer ")))?,
            },
        },
        AuthMode::AwsSigV4 => {
            let session_token = fields
                .get("session_token")
                .filter(|value| !value.trim().is_empty())
                .map(|_| secret("session_token", None))
                .transpose()?;
            AuthConfig::AwsSigV4 {
                access_key: secret("access_key", None)?,
                secret_key: secret("secret_key", None)?,
                session_token,
                region: fields.get("region").cloned().unwrap_or_default(),
                service: fields.get("service").cloned().unwrap_or_default(),
            }
        }
        AuthMode::Login => AuthConfig::Login {
            headers,
            url: required_auth_field(&fields, "url", "Sign-in URL")?,
            method: login_method(&fields)?,
            basic: None,
            body: fields.get("body").cloned().unwrap_or_default(),
            token_path: required_auth_field(&fields, "token_path", "Token path in response")?,
            ttl_secs: login_ttl(&fields)?,
            access_token: None,
            expires_at: None,
        },
    };
    Ok((auth, secrets))
}

fn login_method(fields: &BTreeMap<String, String>) -> Result<String, String> {
    let method = fields
        .get("method")
        .map(|method| method.trim().to_ascii_uppercase())
        .filter(|method| !method.is_empty())
        .unwrap_or_else(|| "POST".into());
    Ok(HttpMethod::new(method)?.as_str().to_string())
}

fn login_ttl(fields: &BTreeMap<String, String>) -> Result<Option<u64>, String> {
    fields
        .get("ttl_secs")
        .map(|ttl| ttl.trim())
        .filter(|ttl| !ttl.is_empty())
        .map(|ttl| {
            ttl.parse::<u64>()
                .map_err(|_| format!("session lifetime must be a number of seconds, not {ttl:?}"))
        })
        .transpose()
}

/// Split the auth editor at its `body=` line: the rows before it and the
/// body text after it (`None` when there is no body line).
/// The `key=value` fields of an auth editor text, keys lower-cased, `body`
/// carrying everything after its line — what the typed forms seed from.
/// Malformed lines are skipped here; `parse_auth` is where they become
/// errors, and it reads the same text.
pub fn auth_field_values(input: &str) -> BTreeMap<String, String> {
    let (rows, body) = split_auth_body(input);
    let mut fields: BTreeMap<String, String> = rows
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .filter(|(key, _)| !key.trim().is_empty())
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    if let Some(body) = body {
        fields.insert("body".into(), body.to_string());
    }
    if let Some(headers) = fields.get_mut("headers") {
        *headers = serde_json::from_str::<String>(headers).unwrap_or_else(|_| headers.clone());
    }
    fields
}

fn split_auth_body(input: &str) -> (&str, Option<&str>) {
    let mut offset = 0;
    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case("body=") {
            let start = offset + (line.len() - trimmed.len()) + 5;
            return (&input[..offset], Some(input[start..].trim()));
        }
        offset += line.len();
    }
    (input, None)
}

fn required_auth_field(
    fields: &BTreeMap<String, String>,
    name: &str,
    label: &str,
) -> Result<String, String> {
    fields
        .get(name)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| format!("{label} is required"))
}

fn oauth_scopes(value: Option<&String>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split([' ', ',']))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn optional_secret(
    fields: &BTreeMap<String, String>,
    secrets: &mut DraftSecrets,
    secret_scope: &str,
    name: &str,
) -> Result<Option<SecretRef>, String> {
    let Some(value) = fields.get(name).filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    if let Some(reference) = parse_vault_expression(value)? {
        return Ok(Some(reference));
    }
    let reference = SecretRef::new(format!("workbench.{secret_scope}.auth.{name}"))?;
    secrets.insert(&reference, value.trim());
    Ok(Some(reference))
}

fn parse_header_rows(input: &str) -> Result<Vec<KeyValueRow>, String> {
    parse_key_value_rows(input, ':')
}

fn parse_multipart_rows(input: &str) -> Result<Vec<MultipartRow>, String> {
    Ok(parse_key_value_rows(input, '=')?
        .into_iter()
        .map(|row| MultipartRow {
            id: row.id,
            key: row.key,
            value: row.value.strip_prefix("file:").map_or_else(
                || MultipartValue::Text(row.value.clone()),
                |path| MultipartValue::File(path.trim().into()),
            ),
            enabled: row.enabled,
            description: row.description,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_api::{SecretResolver, VariableValue};

    #[test]
    fn graphql_editor_keeps_unexpanded_and_invalid_variables_in_the_variables_field() {
        for variables in ["{\"id\": {{$randomInt}}}", "{\"id\":"] {
            let source =
                super::super::body_editor::encode_graphql("query User { user { id } }", variables);
            assert_eq!(
                parse_body(BodyMode::GraphQl, &source).unwrap(),
                Body::GraphQl {
                    query: "query User { user { id } }".into(),
                    variables: variables.into(),
                }
            );
        }
    }

    #[test]
    fn named_vault_auth_and_variable_references_round_trip_without_copying_values() {
        for (mode, source) in [
            (
                AuthMode::Basic,
                "username={{vault.user}}\npassword={{vault.password}}",
            ),
            (AuthMode::Bearer, "token={{vault.token}}"),
            (
                AuthMode::ApiKeyHeader,
                "name=X-Api-Key\nvalue={{vault.key}}",
            ),
        ] {
            let (auth, secrets) = parse_auth(mode, source, "request.scope").unwrap();
            let formatted = format_auth(&auth);
            assert!(formatted.contains("{{vault."));
            let (restored, _) = parse_auth(mode, &formatted, "request.scope").unwrap();
            assert_eq!(auth, restored);
            for name in ["user", "password", "token", "key"] {
                assert!(
                    !secrets
                        .has_value(&switchyard_api::vault::vault_secret_reference(name).unwrap())
                );
            }
        }
        let (variables, secrets) =
            parse_session_variables("secret:access_token={{vault.token}}", "environment.scope")
                .unwrap();
        let VariableValue::Secret(reference) = &variables[0].value else {
            panic!("expected named ref")
        };
        assert_eq!(
            vault_reference_expression(reference).as_deref(),
            Some("{{vault.token}}")
        );
        assert!(!secrets.has_value(reference));
    }

    #[test]
    fn composer_builds_structured_body_auth_cookies_and_custom_method() {
        let (request, secrets) = build(DraftInput {
            request_id: None,
            collection_id: CollectionId::new(),
            name: "Create widget",
            method: "PURGE",
            url: "https://example.test/widgets",
            params: "trace=1",
            headers: "Accept: application/json",
            cookies: "session=abc; theme=dark",
            body_mode: BodyMode::Form,
            body: "name=one\n# ignored=two",
            auth_mode: AuthMode::ApiKeyQuery,
            auth: "name=api_key\nvalue=top-secret",
            variables: "region=us-east-1",
            pre_request_script: "set nonce={{region}}",
            test_script: "status == 201",
            allow_private_network: true,
        })
        .unwrap();

        assert_eq!(request.method.as_str(), "PURGE");
        assert!(matches!(request.body, Body::UrlEncoded { ref rows } if rows.len() == 2));
        assert!(
            request
                .headers
                .iter()
                .all(|row| !row.key.eq_ignore_ascii_case("cookie"))
        );
        assert_eq!(
            parse_cookie_pairs("session=abc; theme=dark").unwrap(),
            [
                ("session".into(), "abc".into()),
                ("theme".into(), "dark".into())
            ]
        );
        assert_eq!(request.scripts.tests, "status == 201");
        assert!(request.settings.allow_private_network);
        let AuthConfig::ApiKey { value, .. } = &request.auth else {
            panic!("expected API key auth")
        };
        assert_eq!(secrets.resolve(value).unwrap(), "top-secret");
    }

    #[test]
    fn canonical_compiler_masks_secrets_in_url_body_and_snippet() {
        let (request, secrets) = build(DraftInput {
            request_id: None,
            collection_id: CollectionId::new(),
            name: "Secret request",
            method: "POST",
            url: "https://example.test/items?token={{token}}",
            params: "",
            headers: "",
            cookies: "",
            body_mode: BodyMode::Json,
            body: r#"{"token":"{{token}}"}"#,
            auth_mode: AuthMode::Bearer,
            auth: "token=actual-secret",
            variables: "secret:token=actual-secret",
            pre_request_script: "",
            test_script: "",
            allow_private_network: false,
        })
        .unwrap();
        let context = compile_context(&[], &[], &secrets);
        let (prepared, snapshot) =
            switchyard_api::compile_request(&request, None, &context).unwrap();
        assert!(prepared.url.contains("actual-secret"));
        assert!(!snapshot.url.contains("actual-secret"));
        assert!(!snapshot.body.contains("actual-secret"));
        let snippet =
            switchyard_api::generate_snippet(switchyard_api::SnippetLanguage::Curl, &snapshot);
        assert!(snippet.contains("<redacted>"));
        assert!(!snippet.contains("actual-secret"));
    }

    #[test]
    fn upload_slots_are_derived_without_rewriting_editor_paths() {
        assert_eq!(
            upload_slot_count(BodyMode::Binary, "/private/imported.txt").unwrap(),
            1
        );
        assert_eq!(
            upload_slot_count(
                BodyMode::Multipart,
                "title=hello\nasset=file:/private/imported.txt\n# skip=file:/tmp/nope"
            )
            .unwrap(),
            1
        );
        let editor = "title=hello\nasset=file:/private/imported.txt\n# skip=file:/tmp/nope";
        assert!(!editor.contains("agentops-upload-capability:"));
    }

    #[test]
    fn equal_secret_field_names_are_isolated_by_request_identity() {
        let collection_id = CollectionId::new();
        let first_id = RequestId::new();
        let second_id = RequestId::new();
        let make = |request_id, token| {
            build(DraftInput {
                request_id: Some(request_id),
                collection_id: collection_id.clone(),
                name: "Scoped",
                method: "GET",
                url: "https://example.test",
                params: "",
                headers: "",
                cookies: "",
                body_mode: BodyMode::None,
                body: "",
                auth_mode: AuthMode::Bearer,
                auth: token,
                variables: "",
                pre_request_script: "",
                test_script: "",
                allow_private_network: false,
            })
            .unwrap()
        };
        let (first, mut vault) = make(first_id, "token=first-secret");
        let (second, second_vault) = make(second_id, "token=second-secret");
        let AuthConfig::Bearer { token: first_ref } = first.auth else {
            panic!("expected bearer auth")
        };
        let AuthConfig::Bearer { token: second_ref } = second.auth else {
            panic!("expected bearer auth")
        };
        assert_ne!(first_ref, second_ref);
        assert!(
            second_vault.resolve(&first_ref).is_err(),
            "loading the second request must not make its value satisfy the first request's ref"
        );
        vault.merge(second_vault);
        assert_eq!(vault.resolve(&first_ref).unwrap(), "first-secret");
        assert_eq!(vault.resolve(&second_ref).unwrap(), "second-secret");
    }

    #[test]
    fn oauth_editor_builds_typed_client_credentials_and_pkce_grants() {
        let collection_id = CollectionId::new();
        let make = |auth: &str| {
            build(DraftInput {
                request_id: Some(RequestId::new()),
                collection_id: collection_id.clone(),
                name: "OAuth",
                method: "GET",
                url: "https://example.test",
                params: "",
                headers: "",
                cookies: "",
                body_mode: BodyMode::None,
                body: "",
                auth_mode: AuthMode::OAuth2,
                auth,
                variables: "",
                pre_request_script: "",
                test_script: "",
                allow_private_network: false,
            })
            .unwrap()
            .0
            .auth
        };
        assert!(matches!(
            make("flow=client_credentials\ntoken_endpoint=https://id.test/token\nclient_id=native\nclient_secret=secret\nscopes=read write"),
            AuthConfig::OAuth2ClientCredentials { scopes, access_token: None, .. }
                if scopes == ["read", "write"]
        ));
        assert!(matches!(
            make("flow=password\ntoken_endpoint=https://id.test/token\nclient_id=native\nusername=jane\npassword=secret\nscopes=web_api"),
            AuthConfig::OAuth2Password { username, client_secret: None, access_token: None, refresh_token: None, .. }
                if username == "jane"
        ));
        assert!(matches!(
            make(
                "flow=authorization_code_pkce\nauthorization_endpoint=https://id.test/authorize\ntoken_endpoint=https://id.test/token\nclient_id=native\nredirect_uri=http://127.0.0.1:18765/callback"
            ),
            AuthConfig::OAuth2AuthorizationCodePkce {
                access_token: None,
                refresh_token: None,
                ..
            }
        ));
    }

    #[test]
    fn blank_masked_secret_keeps_its_vault_reference_without_overwriting_it() {
        let (variables, pending) =
            parse_session_variables("secret:token=", "request.restored").unwrap();
        let VariableValue::Secret(reference) = &variables[0].value else {
            panic!("masked variable must remain a secret reference")
        };
        assert_eq!(
            reference.as_str(),
            "workbench.request.restored.variable.token"
        );
        assert!(
            pending.resolve(reference).is_err(),
            "a blank mask must not create an empty in-memory secret"
        );
    }

    #[test]
    fn every_typed_auth_field_round_trips_through_the_editor_text() {
        for mode in AuthMode::ALL {
            let fields = auth_fields(mode);
            let flows: Vec<&str> = fields
                .iter()
                .find(|spec| spec.key == "flow")
                .map(|spec| match spec.kind {
                    AuthFieldKind::Choice(values) => values.to_vec(),
                    _ => Vec::new(),
                })
                .unwrap_or_else(|| vec![""]);
            for flow in flows {
                let text = fields
                    .iter()
                    .filter(|spec| spec.flows.is_empty() || spec.flows.contains(&flow))
                    .filter_map(|spec| {
                        let value = match (spec.key, spec.kind) {
                            // The form leaves the plain-token flow unwritten,
                            // as `AuthForm::serialize` does for empty choices.
                            ("flow", _) if flow.is_empty() => return None,
                            ("flow", _) => flow.to_string(),
                            ("ttl_secs", _) => "120".into(),
                            (_, AuthFieldKind::Headers) => "X-Test: value".into(),
                            (_, AuthFieldKind::Choice(values)) => values[0].to_string(),
                            (key, _) => format!("{key}-value"),
                        };
                        Some(format!("{}={value}", spec.key))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let (auth, _) = parse_auth(mode, &text, "request.r1")
                    .unwrap_or_else(|error| panic!("{mode:?}/{flow:?}: {error}: {text}"));
                let restored = format_auth(&auth);
                // Every plain field comes back; masked ones never do.
                for spec in fields
                    .iter()
                    .filter(|spec| spec.flows.is_empty() || spec.flows.contains(&flow))
                {
                    let line = format!("{}=", spec.key);
                    match spec.kind {
                        AuthFieldKind::Secret => assert!(!restored.contains(&line), "{restored}"),
                        _ if spec.key == "flow" && flow.is_empty() => {}
                        _ => assert!(restored.contains(&line), "{mode:?}: {restored}"),
                    }
                }
                // The masked text re-parses to the same editor text; the
                // vault references themselves are restored by the panel
                // from the saved auth, not from the text.
                let (again, _) = parse_auth(mode, &restored, "request.r1").unwrap();
                assert_eq!(format_auth(&again), restored, "{mode:?}/{flow:?}");
            }
        }
    }

    #[test]
    fn blank_masked_auth_fields_keep_their_vault_reference() {
        let (auth, pending) = parse_auth(AuthMode::Bearer, "", "request.restored").unwrap();
        let AuthConfig::Bearer { token } = &auth else {
            panic!("expected bearer auth")
        };
        assert_eq!(token.as_str(), "workbench.request.restored.auth.token");
        assert!(pending.resolve(token).is_err());
        let (auth, _) = parse_auth(AuthMode::Basic, "username=ops", "request.restored").unwrap();
        assert!(matches!(auth, AuthConfig::Basic { username, .. } if username == "ops"));
    }

    #[test]
    fn auth_headers_round_trip_with_vault_references_and_multiline_login_body() {
        let header_text =
            "X-Tenant: {{tenant}}\nAuthorization: Bearer {{vault.login-token}}\n# X-Disabled: skip";
        let encoded = serde_json::to_string(header_text).unwrap();
        for (mode, prefix) in [
            (
                AuthMode::Basic,
                "username={{vault.user}}\npassword={{vault.password}}\nauth_url=https://auth.test/login",
            ),
            (
                AuthMode::Login,
                "url=https://auth.test/login\ntoken_path=access_token",
            ),
            (
                AuthMode::OAuth2,
                "flow=client_credentials\ntoken_endpoint=https://auth.test/token\nclient_id=desktop\nclient_secret={{vault.client}}",
            ),
            (
                AuthMode::OAuth2,
                "flow=password\ntoken_endpoint=https://auth.test/token\nclient_id=desktop\nusername=user\npassword={{vault.password}}",
            ),
            (
                AuthMode::OAuth2,
                "flow=authorization_code_pkce\nauthorization_endpoint=https://auth.test/authorize\ntoken_endpoint=https://auth.test/token\nclient_id=desktop\nredirect_uri=http://127.0.0.1:18765/callback",
            ),
        ] {
            let body = if mode == AuthMode::Login {
                "\nbody={\n  \"password\": \"{{vault.password}}\"\n}"
            } else {
                ""
            };
            let text = format!("{prefix}\nheaders={encoded}{body}");
            let (auth, _) = parse_auth(mode, &text, "environment.test").unwrap();
            let restored = format_auth(&auth);
            assert_eq!(auth_field_values(&restored)["headers"], header_text);
            if mode == AuthMode::Login {
                assert_eq!(
                    auth_field_values(&restored)["body"],
                    "{\n  \"password\": \"{{vault.password}}\"\n}"
                );
            }
            let (rebuilt, _) = parse_auth(mode, &restored, "environment.test").unwrap();
            assert_eq!(format_auth(&rebuilt), restored);
        }
    }

    #[test]
    fn basic_login_url_round_trips_with_masked_credentials_and_defaults() {
        let (auth, secrets) = parse_auth(
            AuthMode::Basic,
            "username={{login_user}}\npassword=login-password\nauth_url={{auth_host}}/login",
            "request.basic-login",
        )
        .unwrap();
        let AuthConfig::Login {
            url,
            method,
            basic: Some(basic),
            token_path,
            body,
            ..
        } = &auth
        else {
            panic!("expected Basic login auth");
        };
        assert_eq!(url, "{{auth_host}}/login");
        assert_eq!(method, "POST");
        assert_eq!(token_path, "access_token");
        assert!(body.is_empty());
        assert_eq!(basic.username, "{{login_user}}");
        assert_eq!(secrets.resolve(&basic.password).unwrap(), "login-password");
        let restored = format_auth(&auth);
        assert!(!restored.contains("login-password"));
        let (rebuilt, pending) =
            parse_auth(AuthMode::Basic, &restored, "request.basic-login").unwrap();
        assert_eq!(rebuilt, auth);
        assert!(pending.resolve(&basic.password).is_err());

        let (direct, _) = parse_auth(
            AuthMode::Basic,
            "username=ops\nauth_url=",
            "request.basic-login",
        )
        .unwrap();
        assert!(matches!(direct, AuthConfig::Basic { .. }));
        let (custom, _) = parse_auth(
            AuthMode::Basic,
            "auth_url=/login\nmethod=GET\ntoken_path=data.token\nttl_secs=120",
            "environment.basic-login",
        )
        .unwrap();
        assert!(
            matches!(custom, AuthConfig::Login { method, token_path, ttl_secs: Some(120), .. }
            if method == "GET" && token_path == "data.token")
        );
        assert!(parse_auth(AuthMode::Basic, "auth_url=/login\nttl_secs=soon", "e").is_err());
    }

    #[test]
    fn sign_in_request_body_spans_lines_and_defaults_to_post() {
        let text = "url=https://api.example.test/login\ntoken_path=data.session\nbody={\n  \"user\": \"{{login_user}}\"\n}";
        let (auth, _) = parse_auth(AuthMode::Login, text, "environment.e1").unwrap();
        let AuthConfig::Login {
            url,
            method,
            body,
            token_path,
            ttl_secs,
            ..
        } = &auth
        else {
            panic!("expected sign-in auth")
        };
        assert_eq!(url, "https://api.example.test/login");
        assert_eq!(method, "POST");
        assert_eq!(token_path, "data.session");
        assert_eq!(ttl_secs, &None);
        assert_eq!(body, "{\n  \"user\": \"{{login_user}}\"\n}");
        assert!(format_auth(&auth).ends_with(body));
        let failure = |text: &str| parse_auth(AuthMode::Login, text, "e").err().unwrap();
        assert!(failure("url=https://x\ntoken_path=t\nttl_secs=soon").contains("seconds"));
        assert!(failure("token_path=t").contains("Sign-in URL"));
    }
}
