//! Sign-in sessions: the `AuthConfig::Login` arm executed as its own HTTP
//! exchange, with the value it fetches cached in the vault until it expires.
//!
//! A [`LoginSession`] is resolved while a send is being prepared and carried
//! into execution, so a 401 can sign in once more and resend without
//! recompiling the request. Sign-in exchanges are never written to history;
//! the fetched value only ever reaches the request as a redacted header.

use std::ptr;

use crate::{
    AuthConfig, Body, Collection, CollectionId, CompileContext, DEFAULT_LOGIN_TTL_SECS,
    Environment, Folder, HttpMethod, KeyValueRow, PreparedRequest, RawBodyKind, RequestId,
    RequestSettings, SavedRequest, SecretRef, SecretStore, SecretValue, Variable, WorkspaceId,
    compile_request, effective_auth,
};

use super::secrets::{self, DraftSecrets};
use super::transport::{FileCapabilities, OperationPhase, WorkbenchTransport};

/// Sessions this close to expiry are renewed before the send, matching the
/// compiler's own safety margin so a send never races its cache.
const EXPIRY_SAFETY_SECS: i64 = 30;

/// Whose Login auth is in effect — and so where its refreshed cache is
/// written back. Folders and collections have no UI to hold one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginOwner {
    Request,
    Environment,
}

/// A Login auth in effect for a send, plus everything templating its
/// sign-in request needs.
#[derive(Clone)]
pub struct LoginSession {
    pub owner: LoginOwner,
    /// `request.<id>` or `environment.<id>` — the vault scope of the cache.
    pub scope: String,
    /// The `Login` arm, cache included; updated in place by [`Self::sign_in`].
    pub auth: AuthConfig,
    pub environment: Vec<Variable>,
    pub base_url: String,
    pub secrets: DraftSecrets,
    pub allow_private_network: bool,
}

/// What a session needs to know about the auth chain a send resolves through.
pub struct LoginChain<'a> {
    pub globals: &'a [Variable],
    pub request: &'a SavedRequest,
    pub folders: &'a [&'a Folder],
    pub collection: Option<&'a Collection>,
    pub environment_auth: &'a AuthConfig,
    pub environment_scope: &'a str,
    pub environment: &'a [Variable],
    pub base_url: &'a str,
    pub secrets: &'a DraftSecrets,
}

impl LoginSession {
    /// The Login auth the chain resolves to, if that is what it resolves to.
    pub fn resolve(chain: LoginChain<'_>) -> Result<Option<Self>, String> {
        let effective = effective_auth(
            chain.request,
            chain.folders,
            chain.collection,
            Some(chain.environment_auth),
        );
        if !matches!(effective, AuthConfig::Login { .. }) {
            return Ok(None);
        }
        let (owner, scope) = if ptr::eq(effective, &chain.request.auth) {
            (
                LoginOwner::Request,
                format!("request.{}", chain.request.id.as_str()),
            )
        } else if ptr::eq(effective, chain.environment_auth) {
            (LoginOwner::Environment, chain.environment_scope.to_string())
        } else {
            return Err(
                "A sign-in request can only be configured on a request or an environment.".into(),
            );
        };
        Ok(Some(Self {
            owner,
            scope,
            auth: effective.clone(),
            environment: chain
                .globals
                .iter()
                .chain(
                    chain
                        .collection
                        .into_iter()
                        .flat_map(|collection| &collection.variables),
                )
                .chain(chain.folders.iter().flat_map(|folder| &folder.variables))
                .chain(chain.environment)
                .chain(&chain.request.variables)
                .cloned()
                .collect(),
            base_url: chain.base_url.to_string(),
            secrets: chain.secrets.clone(),
            allow_private_network: chain.request.settings.allow_private_network,
        }))
    }

    /// A session straight from an environment's own Login auth — the Envs
    /// tab's "Sign in now", which has no request in hand.
    pub fn for_environment(
        auth: &AuthConfig,
        scope: &str,
        environment: &[Variable],
        base_url: &str,
        secrets: &DraftSecrets,
    ) -> Option<Self> {
        matches!(auth, AuthConfig::Login { .. }).then(|| Self {
            owner: LoginOwner::Environment,
            scope: scope.to_string(),
            auth: auth.clone(),
            environment: environment.to_vec(),
            base_url: base_url.to_string(),
            secrets: secrets.clone(),
            allow_private_network: RequestSettings::default().allow_private_network,
        })
    }

    /// No cached value, or one about to expire.
    pub fn needs_sign_in(&self, now: i64) -> bool {
        match &self.auth {
            AuthConfig::Login {
                access_token,
                expires_at,
                ..
            } => {
                access_token.is_none()
                    || expires_at.is_none_or(|expires_at| {
                        expires_at <= now.saturating_add(EXPIRY_SAFETY_SECS)
                    })
            }
            _ => false,
        }
    }

    /// Copy the session's cache back onto whichever auth owns it.
    pub fn apply_to(&self, request: &mut SavedRequest, environment_auth: &mut AuthConfig) {
        match self.owner {
            LoginOwner::Request => request.auth = self.auth.clone(),
            LoginOwner::Environment => *environment_auth = self.auth.clone(),
        }
    }

    /// The saved environment with this session's cache on it — when the saved
    /// auth is the one the session was resolved from. An unsaved Envs draft
    /// keeps its cache in the vault only; the next send signs in again.
    pub fn updated_environment(&self, saved: Option<&Environment>) -> Option<Environment> {
        if self.owner != LoginOwner::Environment {
            return None;
        }
        let saved = saved?;
        same_login_source(&saved.auth, &self.auth).then(|| Environment {
            auth: self.auth.clone(),
            ..saved.clone()
        })
    }

    /// Run the sign-in request and cache what it returns. The value comes
    /// back so the caller can put it on the Authorization header of a
    /// request that was compiled before the session was renewed.
    pub fn sign_in(
        &mut self,
        workspace: &WorkspaceId,
        secret_store: &dyn SecretStore,
        transport: &dyn WorkbenchTransport,
        files: &FileCapabilities,
        now: i64,
    ) -> Result<String, String> {
        let AuthConfig::Login {
            url,
            method,
            body,
            token_path,
            ttl_secs,
            ..
        } = &self.auth
        else {
            return Err("not a sign-in request auth".into());
        };
        let prepared = self.prepare(url, method, body)?;
        let phase = OperationPhase {
            operation_id: prepared.request_id.as_str().to_string(),
            starts_operation: true,
            final_phase: true,
        };
        let response = transport
            .send(&prepared, files, &phase)
            .map_err(|error| format!("Sign-in request failed: {error}"))?;
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "Sign-in request failed with HTTP {} {}",
                response.status,
                response.reason.trim()
            ));
        }
        let document: serde_json::Value = serde_json::from_str(&response.body)
            .map_err(|_| "Sign-in response is not JSON".to_string())?;
        let value = json_path(&document, token_path)
            .and_then(json_scalar)
            .ok_or_else(|| format!("Sign-in response has no value at {token_path}"))?;
        let ttl = document
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .or(*ttl_secs)
            .unwrap_or(DEFAULT_LOGIN_TTL_SECS);
        let reference = SecretRef::new(secrets::auth_secret_reference(&self.scope, "token"))?;
        secret_store
            .set_secret(workspace, &reference, SecretValue::new(&value))
            .map_err(|error| error.to_string())?;
        // A stale copy in the draft would shadow the vault at compile time.
        self.secrets.forget(&reference);
        if let AuthConfig::Login {
            access_token,
            expires_at,
            ..
        } = &mut self.auth
        {
            *access_token = Some(reference);
            *expires_at = Some(now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX)));
        }
        Ok(value)
    }

    /// The sign-in as a compiled request: templated against the same
    /// variables the send uses, so `{{login_user}}` and a relative URL
    /// against the base URL both work. Optional Basic credentials are sent
    /// only to this endpoint; the API request uses the returned bearer token.
    fn prepare(&self, url: &str, method: &str, body: &str) -> Result<PreparedRequest, String> {
        fn contains_redacted(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::String(value) => value == "<redacted>",
                serde_json::Value::Array(values) => values.iter().any(contains_redacted),
                serde_json::Value::Object(values) => values.values().any(contains_redacted),
                _ => false,
            }
        }
        if serde_json::from_str(body).is_ok_and(|value| contains_redacted(&value)) {
            return Err("Sign-in body contains a saved <redacted> value. Replace credential literals with {{login_password}} references and set secret:login_password in the environment Variables, then save again.".into());
        }
        let mut headers = match &self.auth {
            AuthConfig::Login { headers, .. } => headers.clone(),
            _ => Vec::new(),
        };
        if !body.trim().is_empty()
            && !headers
                .iter()
                .any(|row| row.enabled && row.key.eq_ignore_ascii_case("content-type"))
        {
            headers.push(KeyValueRow::enabled("Content-Type", "application/json"));
        }
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: CollectionId::default(),
            folder_id: None,
            name: "sign-in".into(),
            method: HttpMethod::new(method)?,
            url: url.to_string(),
            params: Vec::new(),
            headers,
            auth: match &self.auth {
                AuthConfig::Login {
                    basic: Some(basic), ..
                } => AuthConfig::Basic {
                    username: basic.username.clone(),
                    password: basic.password.clone(),
                },
                _ => AuthConfig::None,
            },
            body: if body.trim().is_empty() {
                Body::None
            } else {
                Body::Raw {
                    media_type: RawBodyKind::Json,
                    text: body.to_string(),
                }
            },
            variables: Vec::new(),
            scripts: Default::default(),
            settings: RequestSettings {
                allow_private_network: self.allow_private_network,
                ..Default::default()
            },
            extensions: Default::default(),
            sort_key: 0,
        };
        let context = CompileContext {
            global: &[],
            environment: &self.environment,
            data: &[],
            local: &[],
            secrets: &self.secrets,
            environment_base_url: Some(&self.base_url),
            environment_auth: None,
        };
        compile_request(&request, None, &context)
            .map(|(prepared, _)| prepared)
            .map_err(|error| format!("Sign-in request could not be prepared: {error}"))
    }
}

/// Swap the bearer value a compiled request carries for a freshly fetched
/// one, keeping the redaction list in step.
pub fn replace_bearer(prepared: &mut PreparedRequest, value: &str) {
    prepared
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    prepared
        .headers
        .push(("Authorization".into(), format!("Bearer {value}")));
    prepared.redactions.push(value.to_string());
}

/// Two Login auths that describe the same sign-in, cache aside.
pub fn same_login_source(a: &AuthConfig, b: &AuthConfig) -> bool {
    match (a, b) {
        (
            AuthConfig::Login {
                url: url_a,
                basic: basic_a,
                headers: headers_a,
                method: method_a,
                body: body_a,
                token_path: path_a,
                ttl_secs: ttl_a,
                ..
            },
            AuthConfig::Login {
                url: url_b,
                basic: basic_b,
                headers: headers_b,
                method: method_b,
                body: body_b,
                token_path: path_b,
                ttl_secs: ttl_b,
                ..
            },
        ) => {
            url_a == url_b
                && basic_a == basic_b
                && super::oauth::same_auth_headers(headers_a, headers_b)
                && method_a == method_b
                && body_a == body_b
                && path_a == path_b
                && ttl_a == ttl_b
        }
        _ => false,
    }
}

/// `data.token` / `tokens.0.value` — dotted path, arrays by index.
fn json_path<'a>(document: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .filter(|segment| !segment.is_empty())
        .try_fold(document, |current, segment| match current {
            serde_json::Value::Object(map) => map.get(segment),
            serde_json::Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get(index)),
            _ => None,
        })
}

fn json_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The Envs tab's status line for a managed cache: a Login session or an
/// OAuth 2 token.
pub fn session_status(auth: &AuthConfig, now: i64) -> Option<String> {
    let (cached, expires_at, noun, renew) = match auth {
        AuthConfig::Login {
            access_token,
            expires_at,
            ..
        } => (
            access_token.is_some(),
            *expires_at,
            "session",
            "sign in again",
        ),
        AuthConfig::OAuth2ClientCredentials {
            access_token,
            expires_at,
            ..
        }
        | AuthConfig::OAuth2AuthorizationCodePkce {
            access_token,
            expires_at,
            ..
        }
        | AuthConfig::OAuth2Password {
            access_token,
            expires_at,
            ..
        } => (
            access_token.is_some(),
            *expires_at,
            "token",
            "acquire it again",
        ),
        _ => return None,
    };
    Some(match (cached, expires_at) {
        (true, Some(expires_at)) if expires_at > now => {
            let left = expires_at - now;
            if left >= 3_600 {
                format!("{noun} cached · expires in {} h", left / 3_600)
            } else {
                format!("{noun} cached · expires in {} min", (left / 60).max(1))
            }
        }
        (true, Some(_)) => format!("{noun} expired · {renew}"),
        (true, None) => format!("{noun} cached"),
        (false, _) if noun == "session" => "not signed in".into(),
        (false, _) => "no token yet".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_basic_login_credentials_invalidates_the_managed_cache() {
        let original = AuthConfig::Login {
            url: "https://auth.example.test/login".into(),
            basic: Some(crate::BasicLoginCredentials {
                username: "operator".into(),
                password: SecretRef::new("original-password").unwrap(),
            }),
            method: "POST".into(),
            headers: Vec::new(),
            body: String::new(),
            token_path: "access_token".into(),
            ttl_secs: None,
            access_token: Some(SecretRef::new("cached-token").unwrap()),
            expires_at: Some(1234),
        };
        for change in 0..5 {
            let mut rebuilt = original.clone();
            let AuthConfig::Login {
                basic,
                headers,
                access_token,
                expires_at,
                ..
            } = &mut rebuilt
            else {
                unreachable!()
            };
            *access_token = None;
            *expires_at = None;
            match change {
                1 => basic.as_mut().unwrap().username = "another-user".into(),
                2 => basic.as_mut().unwrap().password = SecretRef::new("another-password").unwrap(),
                3 => *basic = None,
                4 => headers.push(KeyValueRow::enabled("X-Tenant", "another-tenant")),
                _ => {}
            }
            assert_eq!(same_login_source(&original, &rebuilt), change == 0);
            super::super::oauth::preserve_managed_oauth_state(&original, &mut rebuilt);
            assert!(
                matches!(rebuilt, AuthConfig::Login { access_token, expires_at, .. }
                if access_token.is_some() == (change == 0) && expires_at.is_some() == (change == 0))
            );
        }
    }

    #[test]
    fn environment_basic_sign_in_permits_private_auth_endpoints_and_caches_the_token() {
        use std::sync::Arc;

        use base64::Engine as _;

        use crate::{MemorySecretStore, ResponseSnapshot, SecretResolver};

        use super::super::transport::{
            Response, ScriptRequestView, ScriptResponseView, ScriptResult, ScriptScopes,
        };

        struct PrivateEndpointTransport;

        impl WorkbenchTransport for PrivateEndpointTransport {
            fn send(
                &self,
                request: &PreparedRequest,
                _: &FileCapabilities,
                _: &OperationPhase,
            ) -> Result<Response, String> {
                if !request.settings.allow_private_network {
                    return Err("private network destinations are disabled".into());
                }
                assert_eq!(request.url, "http://127.0.0.1:8080/login");
                assert_eq!(
                    request.headers,
                    vec![(
                        "Authorization".into(),
                        "Basic b3BlcmF0b3I6cGFzc3dvcmQ=".into()
                    )]
                );
                Ok(super::super::response_from_snapshot(&ResponseSnapshot {
                    status: 200,
                    body_base64: base64::engine::general_purpose::STANDARD
                        .encode(r#"{"access_token":"private-login-token","expires_in":600}"#),
                    ..Default::default()
                }))
            }

            fn run_script(
                &self,
                _: &str,
                _: ScriptScopes,
                _: ScriptRequestView,
                _: Option<ScriptResponseView>,
                _: Option<&str>,
                _: Option<&OperationPhase>,
            ) -> Result<ScriptResult, String> {
                unreachable!("environment sign-in has no request script")
            }

            fn cancel(&self, _: &str) -> Result<bool, String> {
                Ok(false)
            }
        }

        let workspace = WorkspaceId::new("test/private-environment-login").unwrap();
        let store = Arc::new(MemorySecretStore::new());
        let password = SecretRef::new("login-password").unwrap();
        let mut secrets = DraftSecrets::with_store(store.clone(), workspace.clone());
        secrets.insert(&password, "password");
        let auth = AuthConfig::Login {
            url: "/login".into(),
            basic: Some(crate::BasicLoginCredentials {
                username: "operator".into(),
                password,
            }),
            headers: Vec::new(),
            method: "POST".into(),
            body: String::new(),
            token_path: "access_token".into(),
            ttl_secs: None,
            access_token: None,
            expires_at: None,
        };
        let mut session = LoginSession::for_environment(
            &auth,
            "environment.private-login",
            &[],
            "http://127.0.0.1:8080",
            &secrets,
        )
        .unwrap();
        let value = session
            .sign_in(
                &workspace,
                store.as_ref(),
                &PrivateEndpointTransport,
                &FileCapabilities::default(),
                1_000,
            )
            .unwrap();
        assert_eq!(value, "private-login-token");
        let AuthConfig::Login {
            access_token: Some(reference),
            expires_at: Some(1_600),
            ..
        } = &session.auth
        else {
            panic!("successful Basic sign-in must cache the returned token")
        };
        assert_eq!(session.secrets.resolve(reference).unwrap(), value);
        assert!(!session.needs_sign_in(1_001));
        assert!(
            !serde_json::to_string(&session.auth)
                .unwrap()
                .contains(&value)
        );
    }

    #[test]
    fn saved_redacted_sign_in_body_is_rejected_before_transport() {
        let session = LoginSession {
            owner: LoginOwner::Environment,
            scope: "environment.test".into(),
            auth: AuthConfig::None,
            environment: Vec::new(),
            base_url: "https://example.test".into(),
            secrets: DraftSecrets::default(),
            allow_private_network: false,
        };
        for body in [
            r#"{"password":"<redacted>"}"#,
            r#"{"SignInRequest":{"credentials":["<redacted>"]}}"#,
            r#"{"password":"\u003credacted\u003e"}"#,
        ] {
            let error = session.prepare("/login", "POST", body).unwrap_err();
            assert!(error.contains("secret:login_password"));
            assert!(!error.contains("HTTP 401"));
        }
        assert!(
            session
                .prepare("/login", "POST", r#"{"user":"ops"}"#)
                .is_ok()
        );
    }

    #[test]
    fn sign_in_headers_keep_explicit_content_type_and_resolve_vault_values() {
        let mut auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "kind": "login", "url": "/login", "token_path": "token",
            "body": "{}"
        }))
        .unwrap();
        let AuthConfig::Login { headers, .. } = &mut auth else {
            unreachable!()
        };
        headers.push(KeyValueRow::enabled(
            "content-type",
            "application/vnd.example+json",
        ));
        headers.push(KeyValueRow::enabled("X-Api-Key", "{{vault.login_key}}"));
        let mut disabled = KeyValueRow::enabled("X-Disabled", "{{missing}}");
        disabled.enabled = false;
        headers.push(disabled);
        let mut secrets = DraftSecrets::default();
        let reference = crate::vault::parse_vault_expression("{{vault.login_key}}")
            .unwrap()
            .unwrap();
        secrets.insert(&reference, "header-secret");
        let session =
            LoginSession::for_environment(&auth, "env", &[], "https://example.test", &secrets)
                .unwrap();
        let prepared = session.prepare("/login", "POST", "{}").unwrap();
        assert_eq!(
            prepared.headers,
            vec![
                ("content-type".into(), "application/vnd.example+json".into()),
                ("X-Api-Key".into(), "header-secret".into()),
            ]
        );
        assert!(
            !serde_json::to_string(&prepared.redacted_snapshot())
                .unwrap()
                .contains("header-secret")
        );
    }

    #[test]
    fn json_path_walks_objects_and_arrays() {
        let document: serde_json::Value =
            serde_json::json!({"data": {"token": "t1", "tokens": [{"value": 7}]}});
        assert_eq!(
            json_path(&document, "data.token")
                .and_then(json_scalar)
                .as_deref(),
            Some("t1")
        );
        assert_eq!(
            json_path(&document, "data.tokens.0.value")
                .and_then(json_scalar)
                .as_deref(),
            Some("7")
        );
        assert!(json_path(&document, "data.missing").is_none());
        assert!(json_path(&document, "data").and_then(json_scalar).is_none());
    }

    #[test]
    fn session_status_reads_the_cache() {
        let login = |cache: Option<SecretRef>, expires_at: Option<i64>| AuthConfig::Login {
            url: "/login".into(),
            basic: None,
            headers: Vec::new(),
            method: "POST".into(),
            body: String::new(),
            token_path: "token".into(),
            ttl_secs: None,
            access_token: cache,
            expires_at,
        };
        let reference = SecretRef::new("workbench.environment.e.auth.token").unwrap();
        assert_eq!(
            session_status(&login(None, None), 1_000).as_deref(),
            Some("not signed in")
        );
        assert_eq!(
            session_status(&login(Some(reference.clone()), Some(1_000 + 90)), 1_000).as_deref(),
            Some("session cached · expires in 1 min")
        );
        assert_eq!(
            session_status(&login(Some(reference.clone()), Some(1_000 + 7_200)), 1_000).as_deref(),
            Some("session cached · expires in 2 h")
        );
        assert_eq!(
            session_status(&login(Some(reference), Some(900)), 1_000).as_deref(),
            Some("session expired · sign in again")
        );
        assert_eq!(session_status(&AuthConfig::None, 0), None);
    }
}
