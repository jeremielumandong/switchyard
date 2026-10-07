use super::{
    AuthConfig, Body, Collection, Environment, Folder, MultipartValue, RedactedRequestSnapshot,
    ReplayRequestSnapshot, SavedRequest, SecretRef, Variable, VariableValue,
};
use serde_json::Value;

const REDACTED: &str = "<redacted>";

/// Reject a sign-in definition that would lose credentials when saved. Export
/// and import sanitization still scrubs these values, but an interactive save
/// must not report success for a definition that can no longer sign in.
pub(super) fn validate_saved_login_auth(auth: &AuthConfig) -> Result<(), String> {
    if let AuthConfig::Login { headers, .. }
    | AuthConfig::OAuth2ClientCredentials { headers, .. }
    | AuthConfig::OAuth2Password { headers, .. }
    | AuthConfig::OAuth2AuthorizationCodePkce { headers, .. } = auth
    {
        let mut sanitized = headers.clone();
        sanitize_auth_headers(&mut sanitized);
        if sanitized != *headers {
            return Err(
                "Authentication headers contain literal credentials that cannot be saved. Use a secret reference such as {{vault.api_key}} for the header value."
                    .into(),
            );
        }
    }
    let AuthConfig::Login { body, .. } = auth else {
        return Ok(());
    };
    let mut sanitized = body.clone();
    sanitize_json_text(&mut sanitized);
    if sanitized != *body {
        return Err(
            "Sign-in payload contains literal credentials that cannot be saved. Use a secret variable, for example {{login_password}} in the JSON payload and secret:login_password = your value in Variables, then save again."
                .into(),
        );
    }
    Ok(())
}

/// Produces the only `SavedRequest` shape that may cross a durable boundary.
///
/// Opaque vault references are retained. Literal values in credential-shaped
/// headers, query/form rows, variables, JSON fields, and unsupported auth are
/// replaced with a fresh missing-secret marker or a disabled redacted row. URL
/// user information is rejected because silently rewriting the authority is too
/// likely to make the saved request target a different endpoint than intended.
pub fn persistence_safe_saved_request(request: &SavedRequest) -> Result<SavedRequest, String> {
    let mut safe = request.clone();
    sanitize_url(&mut safe.url)?;
    sanitize_rows(&mut safe.params);
    sanitize_headers(&mut safe.headers);
    sanitize_variables(&mut safe.variables);
    sanitize_body(&mut safe.body);
    sanitize_auth_payload(&mut safe.auth);
    sanitize_json_map(&mut safe.extensions);
    Ok(safe)
}

pub fn persistence_safe_collection(collection: &Collection) -> Collection {
    let mut safe = collection.clone();
    sanitize_variables(&mut safe.variables);
    sanitize_auth_payload(&mut safe.auth);
    sanitize_json_map(&mut safe.extensions);
    safe
}

pub fn persistence_safe_folder(folder: &Folder) -> Folder {
    let mut safe = folder.clone();
    sanitize_variables(&mut safe.variables);
    sanitize_auth_payload(&mut safe.auth);
    sanitize_json_map(&mut safe.extensions);
    safe
}

pub fn persistence_safe_environment(environment: &Environment) -> Environment {
    let mut safe = environment.clone();
    sanitize_variables(&mut safe.variables);
    sanitize_auth_payload(&mut safe.auth);
    sanitize_json_map(&mut safe.extensions);
    safe
}

pub(super) fn sanitize_replay_snapshot(replay: &mut ReplayRequestSnapshot) -> Result<(), String> {
    sanitize_url(&mut replay.url)?;
    sanitize_rows(&mut replay.params);
    sanitize_headers(&mut replay.headers);
    sanitize_variables(&mut replay.variables);
    sanitize_body(&mut replay.body);
    sanitize_auth_payload(&mut replay.auth);
    Ok(())
}

pub(super) fn sanitize_redacted_request_snapshot(
    snapshot: &mut RedactedRequestSnapshot,
) -> Result<(), String> {
    sanitize_url(&mut snapshot.url)?;
    for (name, value) in &mut snapshot.headers {
        if (sensitive_header(name) || sensitive_name(name))
            && !super::compile::is_redacted_bearer_summary(value)
        {
            *value = REDACTED.into();
        }
    }
    sanitize_json_text(&mut snapshot.body);
    if let Some(replay) = &mut snapshot.replay {
        sanitize_replay_snapshot(replay)?;
    }
    Ok(())
}

/// [`sanitize_redacted_request_snapshot`] that cannot fail: a URL it cannot sanitize is
/// replaced with the redaction marker (fail closed), then everything else is sanitized.
pub(super) fn sanitize_redacted_request_snapshot_or_redact(snapshot: &mut RedactedRequestSnapshot) {
    if sanitize_redacted_request_snapshot(snapshot).is_err() {
        snapshot.url = REDACTED.into();
        if let Some(replay) = &mut snapshot.replay {
            replay.url = REDACTED.into();
        }
        let _ = sanitize_redacted_request_snapshot(snapshot);
    }
}

/// [`sanitize_replay_snapshot`] that cannot fail, failing closed like
/// [`sanitize_redacted_request_snapshot_or_redact`].
pub(super) fn sanitize_replay_snapshot_or_redact(replay: &mut ReplayRequestSnapshot) {
    if sanitize_replay_snapshot(replay).is_err() {
        replay.url = REDACTED.into();
        let _ = sanitize_replay_snapshot(replay);
    }
}

pub(super) fn persistence_safe_request_url(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err(
            "Request URL cannot be empty. Use / to target the environment base URL.".into(),
        );
    }
    let mut safe = value.to_string();
    sanitize_url(&mut safe)?;
    Ok(safe)
}

fn sanitize_url(value: &mut String) -> Result<(), String> {
    if url_has_userinfo(value) {
        return Err("saved request URL must not include user information".into());
    }
    let Ok(mut url) = url::Url::parse(value.as_str()) else {
        sanitize_raw_query(value);
        return Ok(());
    };
    let pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    if pairs.iter().any(|(_, value)| is_template_reference(value)) {
        // Keep reference braces intact; serializing query pairs would encode
        // them and prevent the compiler from resolving the saved reference.
        sanitize_raw_query(value);
        return Ok(());
    }
    if pairs.iter().any(|(key, _)| sensitive_name(key)) {
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs.iter().map(|(key, value)| {
                (
                    key.as_str(),
                    if sensitive_name(key) { REDACTED } else { value },
                )
            }));
        *value = url.into();
    }
    Ok(())
}

/// Detects user information without requiring the host to be parseable. This
/// matters for request templates such as `https://user:pass@{{host}}/items`,
/// where the URL parser cannot validate the authority yet the credential is
/// still plainly present in the durable model.
pub(super) fn url_has_userinfo(value: &str) -> bool {
    if let Ok(url) = url::Url::parse(value) {
        return !url.username().is_empty() || url.password().is_some();
    }

    let authority = if let Some(authority) = value.strip_prefix("//") {
        authority
    } else if let Some((_, authority)) = value.split_once("://") {
        authority
    } else {
        return false;
    };
    let authority_end = authority.find(['/', '?', '#']).unwrap_or(authority.len());
    authority[..authority_end].contains('@')
}

fn sanitize_raw_query(value: &mut String) {
    let (before_fragment, fragment) = value
        .split_once('#')
        .map_or((value.as_str(), None), |(before, after)| {
            (before, Some(after))
        });
    let Some((base, query)) = before_fragment.split_once('?') else {
        return;
    };
    let sanitized = query
        .split('&')
        .map(|part| {
            let (raw_key, raw_value) = part
                .split_once('=')
                .map_or((part, None), |(key, value)| (key, Some(value)));
            let decoded_key = url::form_urlencoded::parse(format!("{raw_key}=").as_bytes())
                .next()
                .map(|(key, _)| key.into_owned())
                .unwrap_or_default();
            if sensitive_name(&decoded_key)
                && raw_value.is_some_and(|value| !is_template_reference(value))
            {
                format!("{raw_key}={REDACTED}")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    *value = match fragment {
        Some(fragment) => format!("{base}?{sanitized}#{fragment}"),
        None => format!("{base}?{sanitized}"),
    };
}

fn sanitize_rows(rows: &mut [super::KeyValueRow]) {
    for row in rows {
        if sensitive_name(&row.key) && !is_template_reference(&row.value) {
            row.value = REDACTED.into();
            row.enabled = false;
        }
    }
}

fn sanitize_headers(rows: &mut [super::KeyValueRow]) {
    for row in rows {
        if (sensitive_header(&row.key) || sensitive_name(&row.key))
            && !is_header_reference(&row.value)
        {
            row.value = REDACTED.into();
            row.enabled = false;
        }
    }
}

fn sanitize_auth_headers(rows: &mut [super::KeyValueRow]) {
    for row in rows {
        let reference = is_template_reference(&row.value)
            || row.value.split_once(' ').is_some_and(|(scheme, value)| {
                matches!(scheme.to_ascii_lowercase().as_str(), "bearer" | "basic")
                    && is_template_reference(value)
            });
        if !reference {
            sanitize_headers(std::slice::from_mut(row));
        }
    }
}

pub(super) fn sanitize_variables(variables: &mut [Variable]) {
    for variable in variables {
        if sensitive_name(&variable.key)
            && matches!(&variable.value, VariableValue::Plain(value) if !is_template_reference(value))
        {
            variable.value = VariableValue::MissingSecret(missing_secret_ref());
        }
    }
}

fn sanitize_body(body: &mut Body) {
    match body {
        Body::Raw { text, .. } => sanitize_json_text(text),
        Body::UrlEncoded { rows } => sanitize_rows(rows),
        Body::Multipart { rows } => {
            for row in rows {
                if sensitive_name(&row.key)
                    && !matches!(&row.value, MultipartValue::Text(value) if is_template_reference(value))
                {
                    if matches!(row.value, MultipartValue::Text(_)) {
                        row.value = MultipartValue::Text(REDACTED.into());
                    }
                    row.enabled = false;
                }
            }
        }
        Body::GraphQl { variables, .. } => sanitize_json_text(variables),
        Body::None | Body::Binary { .. } => {}
    }
}

fn sanitize_json_text(text: &mut String) {
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let original = value.clone();
    sanitize_json_value(None, &mut value);
    if value != original
        && let Ok(safe) = serde_json::to_string_pretty(&value)
    {
        *text = safe;
    }
}

fn sanitize_auth_payload(auth: &mut AuthConfig) {
    match auth {
        AuthConfig::Unsupported { raw, .. } => sanitize_json_value(Some("auth"), raw),
        // A sign-in body is a template: `{{login_secret}}` references stay,
        // literal values in credential-shaped fields do not.
        AuthConfig::Login { body, headers, .. } => {
            sanitize_json_text(body);
            sanitize_auth_headers(headers);
        }
        AuthConfig::OAuth2ClientCredentials { headers, .. }
        | AuthConfig::OAuth2Password { headers, .. }
        | AuthConfig::OAuth2AuthorizationCodePkce { headers, .. } => sanitize_auth_headers(headers),
        _ => {}
    }
}

/// `{{name}}` references — alone or several in a row — are pointers into the
/// variable scopes, not values, so redacting them would only break the
/// template without hiding anything.
fn is_template_reference(value: &str) -> bool {
    let mut rest = value.trim();
    if rest.is_empty() {
        return false;
    }
    while let Some(after) = rest.strip_prefix("{{") {
        let Some(close) = after.find("}}") else {
            return false;
        };
        rest = after[close + 2..].trim_start();
    }
    rest.is_empty()
}

fn is_header_reference(value: &str) -> bool {
    let named = |value: &str| matches!(crate::vault::parse_vault_expression(value), Ok(Some(_)));
    named(value)
        || value.split_once(' ').is_some_and(|(scheme, value)| {
            matches!(scheme.to_ascii_lowercase().as_str(), "bearer" | "basic") && named(value)
        })
}

fn sanitize_json_map(map: &mut serde_json::Map<String, Value>) {
    for (key, value) in map {
        sanitize_json_value(Some(key), value);
    }
}

fn sanitize_json_value(field: Option<&str>, value: &mut Value) {
    sanitize_json_value_with_ancestor(field, false, value);
}

fn sanitize_json_value_with_ancestor(
    field: Option<&str>,
    sensitive_ancestor: bool,
    value: &mut Value,
) {
    let sensitive = sensitive_ancestor || field.is_some_and(sensitive_name);
    if sensitive && !value.is_object() && !value.is_array() {
        if !value.as_str().is_some_and(is_template_reference) {
            *value = Value::String(REDACTED.into());
        }
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                sanitize_json_value_with_ancestor(field, sensitive, value);
            }
        }
        Value::Object(values) => {
            let names_sensitive_header = values
                .get("key")
                .or_else(|| values.get("name"))
                .and_then(Value::as_str)
                .is_some_and(sensitive_header);
            if names_sensitive_header
                && let Some(value) = values.get_mut("value")
                && !value.as_str().is_some_and(is_template_reference)
            {
                *value = Value::String(REDACTED.into());
            }
            for (key, value) in values {
                sanitize_json_value_with_ancestor(Some(key), sensitive, value);
            }
        }
        _ => {}
    }
}

fn sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "proxy-authorization" | "set-cookie"
    )
}

pub(crate) fn sensitive_name(name: &str) -> bool {
    let normalized = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized == "sid"
        || [
            "authorization",
            "bearer",
            "credential",
            "password",
            "passwd",
            "secret",
            "token",
            "apikey",
            "accesskey",
            "privatekey",
            "cookie",
            "sessionid",
        ]
        .iter()
        .any(|marker| normalized.contains(marker))
        || normalized == "auth"
        || normalized.ends_with("auth")
}

pub(super) fn missing_secret_ref() -> SecretRef {
    SecretRef::generated("imported-missing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CollectionId, EnvironmentId, FolderId, HttpMethod, KeyValueRow, RawBodyKind, RequestId,
        RequestSettings, Scripts, WorkspaceId,
    };

    #[test]
    fn sanitizer_removes_literal_credentials_from_every_typed_request_location() {
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: CollectionId::new(),
            folder_id: None,
            name: "Unsafe".into(),
            method: HttpMethod::new("POST").unwrap(),
            url: "https://example.test/items?api_key=url-secret&safe=yes".into(),
            params: vec![KeyValueRow::enabled("access_token", "param-secret")],
            headers: vec![KeyValueRow::enabled(
                "Authorization",
                "Bearer header-secret",
            )],
            auth: AuthConfig::None,
            body: Body::Raw {
                media_type: RawBodyKind::Json,
                text: r#"{"password":"body-secret","safe":"kept"}"#.into(),
            },
            variables: vec![Variable {
                id: Default::default(),
                key: "client_secret".into(),
                value: VariableValue::Plain("variable-secret".into()),
                enabled: true,
                description: String::new(),
            }],
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: serde_json::from_value(serde_json::json!({
                "backup_token": "extension-secret",
                "shape": {"kept": true}
            }))
            .unwrap(),
            sort_key: 0,
        };

        let safe = persistence_safe_saved_request(&request).unwrap();
        let bytes = serde_json::to_string(&safe).unwrap();
        for secret in [
            "url-secret",
            "param-secret",
            "header-secret",
            "body-secret",
            "variable-secret",
            "extension-secret",
        ] {
            assert!(!bytes.contains(secret), "persisted {secret:?}: {bytes}");
        }
        assert!(bytes.contains("kept"));
        assert!(matches!(
            safe.variables[0].value,
            VariableValue::MissingSecret(_)
        ));
    }

    #[test]
    fn sanitizer_scrubs_templated_urls_and_sensitive_json_ancestors() {
        let mut request = SavedRequest {
            id: RequestId::new(),
            collection_id: CollectionId::new(),
            folder_id: None,
            name: "Templated".into(),
            method: HttpMethod::get(),
            url: "{{base_url}}/items?api%5Fkey=query-secret&safe=kept#fragment".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::Unsupported {
                name: "custom".into(),
                raw: serde_json::json!({"scheme":{"value":"unsupported-auth-secret"}}),
            },
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: serde_json::from_value(serde_json::json!({
                "x-auth": {"value": "nested-extension-secret"},
                "safe": {"value": "kept"}
            }))
            .unwrap(),
            sort_key: 0,
        };
        let safe = persistence_safe_saved_request(&request).unwrap();
        let serialized = serde_json::to_string(&safe).unwrap();
        assert!(!serialized.contains("query-secret"));
        assert!(!serialized.contains("unsupported-auth-secret"));
        assert!(!serialized.contains("nested-extension-secret"));
        assert!(serialized.contains("safe"));
        assert!(serialized.contains("kept"));
        assert!(
            safe.url
                .ends_with("api%5Fkey=<redacted>&safe=kept#fragment")
        );

        request.url = "{{base_url}}?safe=query-secret".into();
        assert!(
            persistence_safe_saved_request(&request)
                .unwrap()
                .url
                .contains("safe=query-secret")
        );

        request.url = "https://user:literal-password@{{host}}/items".into();
        let error = persistence_safe_saved_request(&request).unwrap_err();
        assert!(error.contains("user information"), "{error}");
    }

    #[test]
    fn every_durable_container_uses_the_same_variable_and_extension_policy() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection_id = CollectionId::new();
        let variable = Variable {
            id: Default::default(),
            key: "api_token".into(),
            value: VariableValue::Plain("plain-container-secret".into()),
            enabled: true,
            description: String::new(),
        };
        let extensions: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({
                "x-auth": {"value": "nested-container-secret"}
            }))
            .unwrap();
        let collection = persistence_safe_collection(&Collection {
            id: collection_id.clone(),
            workspace_id: workspace.clone(),
            name: "Collection".into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: vec![variable.clone()],
            scripts: Scripts::default(),
            extensions: extensions.clone(),
        });
        let folder = persistence_safe_folder(&Folder {
            id: FolderId::new(),
            collection_id,
            parent_id: None,
            name: "Folder".into(),
            auth: AuthConfig::None,
            variables: vec![variable.clone()],
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: extensions.clone(),
        });
        let environment = persistence_safe_environment(&Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace,
            name: "Environment".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: vec![variable],
            active: false,
            extensions,
        });
        for serialized in [
            serde_json::to_string(&collection).unwrap(),
            serde_json::to_string(&folder).unwrap(),
            serde_json::to_string(&environment).unwrap(),
        ] {
            assert!(!serialized.contains("plain-container-secret"));
            assert!(!serialized.contains("nested-container-secret"));
            assert!(serialized.contains("missing_secret"));
        }
    }

    #[test]
    fn authentication_headers_preserve_references_and_reject_literal_credentials_on_save() {
        for kind in [
            "login",
            "o_auth2_client_credentials",
            "o_auth2_password",
            "o_auth2_authorization_code_pkce",
        ] {
            let mut auth: AuthConfig = serde_json::from_value(serde_json::json!({
                "kind": kind, "url": "/login", "token_path": "token",
                "token_endpoint": "https://example.test/token", "client_id": "client",
                "client_secret": "client-secret", "username": "user", "password": "password-secret",
                "authorization_endpoint": "https://example.test/authorize", "redirect_uri": "http://127.0.0.1/callback",
                "headers": [
                    KeyValueRow::enabled("X-Api-Key", "{{vault.login_key}}"),
                    KeyValueRow::enabled("Authorization", "Bearer {{login_token}}"),
                    KeyValueRow::enabled("X-Tenant", "example"),
                ]
            })).unwrap();
            assert!(validate_saved_login_auth(&auth).is_ok());
            let original = auth.clone();
            sanitize_auth_payload(&mut auth);
            assert_eq!(auth, original);
            let headers = match &mut auth {
                AuthConfig::Login { headers, .. }
                | AuthConfig::OAuth2ClientCredentials { headers, .. }
                | AuthConfig::OAuth2Password { headers, .. }
                | AuthConfig::OAuth2AuthorizationCodePkce { headers, .. } => headers,
                _ => unreachable!(),
            };
            headers.push(KeyValueRow::enabled("X-Api-Key", "literal-header-secret"));
            assert!(
                validate_saved_login_auth(&auth)
                    .unwrap_err()
                    .contains("Authentication headers")
            );
            sanitize_auth_payload(&mut auth);
            let serialized = serde_json::to_string(&auth).unwrap();
            assert!(!serialized.contains("literal-header-secret"));
            assert!(serialized.contains("{{vault.login_key}}"));
        }
    }

    #[test]
    fn environment_sign_in_bodies_lose_literals_but_keep_template_references() {
        let workspace = WorkspaceId::new("project").unwrap();
        // Built through serde so the test reads like a stored definition.
        let sign_in = |body: &str| {
            serde_json::from_value::<AuthConfig>(serde_json::json!({
                "kind": "login",
                "url": "https://api.example.test/login",
                "body": body,
                "token_path": "data.session",
            }))
            .unwrap()
        };
        let literal = persistence_safe_environment(&Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Environment".into(),
            base_url: "https://api.example.test".into(),
            auth: sign_in(r#"{"user":"ops","password":"plain-sign-in-secret"}"#),
            variables: Vec::new(),
            active: false,
            extensions: Default::default(),
        });
        let serialized = serde_json::to_string(&literal).unwrap();
        assert!(!serialized.contains("plain-sign-in-secret"), "{serialized}");
        assert!(serialized.contains(REDACTED));
        assert_eq!(literal.base_url, "https://api.example.test");

        let templated = persistence_safe_environment(&Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace,
            name: "Environment".into(),
            base_url: String::new(),
            auth: sign_in(r#"{"user":"{{login_user}}","password":"{{login_password}}"}"#),
            variables: Vec::new(),
            active: false,
            extensions: Default::default(),
        });
        let serialized = serde_json::to_string(&templated).unwrap();
        assert!(serialized.contains("{{login_password}}"), "{serialized}");
        assert!(!serialized.contains(REDACTED));
        assert!(is_template_reference("{{a}} {{b}}"));
        assert!(!is_template_reference("{{a}}x"));
        assert!(!is_template_reference(""));
    }
}
