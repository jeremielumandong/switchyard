//! Managed OAuth 2 caches: silent token-endpoint refreshes before a send or
//! run, and where a fetched token is written back (request or environment).
//!
//! Browser-driven PKCE authorization is deliberately not here: the desktop
//! app finishes that grant through [`BrowserAuthorization`], and a headless
//! caller that needs one fails closed with a message pointing at the app.

use crate::{
    AuthConfig, Collection, CompileContext, Environment, Folder, KeyValueRow, SavedRequest,
    SecretRef, SecretResolver, SecretStore, SecretValue, Variable, WorkbenchStore, WorkspaceId,
    compile_auth_headers, effective_auth,
};

use super::secrets::DraftSecrets;
use super::send::{StandaloneSendInput, now_seconds};
use super::transport::{OAuthTokenRequest, OAuthTokenResponse, WorkbenchTransport};

/// Managed caches this close to expiry are renewed before the send.
pub const OAUTH_EXPIRY_SAFETY_SECS: i64 = 30;

/// What a headless send says when only the browser can get a token.
pub const PKCE_NEEDS_BROWSER: &str =
    "OAuth 2 PKCE needs a browser sign-in — open the request in the app and click Acquire token";

/// A browser grant the desktop app finished (or will finish) for one send.
///
/// The callback receives the auth that owns the cache, its vault scope
/// (`request.<id>` or `environment.<id>`), the session secrets, the
/// workspace and the vault; it must record the fetched token on the auth
/// (see [`store_oauth_token`]). Headless callers leave it `None`.
pub struct BrowserAuthorization {
    #[allow(clippy::type_complexity)]
    pub complete: Box<
        dyn FnOnce(
                &mut AuthConfig,
                &str,
                &DraftSecrets,
                &WorkspaceId,
                &dyn SecretStore,
            ) -> Result<(), String>
            + Send,
    >,
}

impl std::fmt::Debug for BrowserAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserAuthorization")
    }
}

/// A vault miss for an auth field, phrased as what to do about it: the
/// deterministic reference names the scope and the field, and the editor
/// keeps masked fields blank on purpose.
pub fn describe_missing_auth_secret(message: String) -> String {
    let Some(rest) = message.strip_prefix("secret workbench.") else {
        return message;
    };
    let Some((reference, _)) = rest.split_once(" is not available") else {
        return message;
    };
    let Some((scope, field)) = reference.split_once(".auth.") else {
        return message;
    };
    let field = field.replace('_', " ");
    if scope.starts_with("environment") {
        format!(
            "The environment's {field} is not in the vault — enter it again under Envs › Authentication and save."
        )
    } else {
        format!("The {field} is not in the vault — enter it again on the Auth tab and save.")
    }
}

pub fn preserve_managed_oauth_state(original: &AuthConfig, rebuilt: &mut AuthConfig) {
    match (original, rebuilt) {
        (
            AuthConfig::OAuth2ClientCredentials {
                token_endpoint: old_endpoint,
                headers: old_headers,
                client_id: old_client,
                client_secret: old_secret,
                scopes: old_scopes,
                access_token: old_access,
                expires_at: old_expiry,
            },
            AuthConfig::OAuth2ClientCredentials {
                token_endpoint,
                headers,
                client_id,
                client_secret,
                scopes,
                access_token,
                expires_at,
            },
        ) if old_endpoint == token_endpoint
            && old_client == client_id
            && old_secret == client_secret
            && old_scopes == scopes
            && same_auth_headers(old_headers, headers) =>
        {
            *access_token = old_access.clone();
            *expires_at = *old_expiry;
        }
        (
            AuthConfig::OAuth2Password {
                token_endpoint: old_endpoint,
                headers: old_headers,
                client_id: old_client,
                client_secret: old_secret,
                username: old_user,
                password: old_pw,
                scopes: old_scopes,
                access_token: old_access,
                refresh_token: old_refresh,
                expires_at: old_expiry,
            },
            AuthConfig::OAuth2Password {
                token_endpoint,
                headers,
                client_id,
                client_secret,
                username,
                password: pw,
                scopes,
                access_token,
                refresh_token,
                expires_at,
            },
        ) if old_endpoint == token_endpoint
            && old_client == client_id
            && old_secret == client_secret
            && old_user == username
            && old_pw == pw
            && old_scopes == scopes
            && same_auth_headers(old_headers, headers) =>
        {
            *access_token = old_access.clone();
            *refresh_token = old_refresh.clone();
            *expires_at = *old_expiry;
        }
        (
            AuthConfig::OAuth2AuthorizationCodePkce {
                authorization_endpoint: old_authorization,
                token_endpoint: old_token,
                headers: old_headers,
                client_id: old_client,
                scopes: old_scopes,
                redirect_uri: old_redirect,
                access_token: old_access,
                refresh_token: old_refresh,
                expires_at: old_expiry,
            },
            AuthConfig::OAuth2AuthorizationCodePkce {
                authorization_endpoint,
                token_endpoint,
                headers,
                client_id,
                scopes,
                redirect_uri,
                access_token,
                refresh_token,
                expires_at,
            },
        ) if old_authorization == authorization_endpoint
            && old_token == token_endpoint
            && old_client == client_id
            && old_scopes == scopes
            && same_auth_headers(old_headers, headers)
            && old_redirect == redirect_uri =>
        {
            *access_token = old_access.clone();
            *refresh_token = old_refresh.clone();
            *expires_at = *old_expiry;
        }
        (
            AuthConfig::Login {
                url: old_url,
                headers: old_headers,
                basic: old_basic,
                method: old_method,
                body: old_body,
                token_path: old_path,
                ttl_secs: old_ttl,
                access_token: old_access,
                expires_at: old_expiry,
            },
            AuthConfig::Login {
                url,
                headers,
                basic,
                method,
                body,
                token_path,
                ttl_secs,
                access_token,
                expires_at,
            },
        ) if old_url == url
            && old_basic == basic
            && same_auth_headers(old_headers, headers)
            && old_method == method
            && old_body == body
            && old_path == token_path
            && old_ttl == ttl_secs =>
        {
            *access_token = old_access.clone();
            *expires_at = *old_expiry;
        }
        _ => {}
    }
}

pub(crate) fn same_auth_headers(left: &[KeyValueRow], right: &[KeyValueRow]) -> bool {
    left.iter()
        .map(|row| (row.enabled, &row.key, &row.value))
        .eq(right.iter().map(|row| (row.enabled, &row.key, &row.value)))
}

pub fn oauth_headers(auth: &AuthConfig) -> &[KeyValueRow] {
    match auth {
        AuthConfig::OAuth2ClientCredentials { headers, .. }
        | AuthConfig::OAuth2Password { headers, .. }
        | AuthConfig::OAuth2AuthorizationCodePkce { headers, .. } => headers,
        _ => &[],
    }
}

#[derive(Default)]
pub struct OAuthVariables<'a> {
    pub globals: &'a [Variable],
    pub collection: Option<&'a Collection>,
    pub folders: &'a [&'a Folder],
    pub environment: &'a [Variable],
    pub base_url: Option<&'a str>,
}

impl OAuthVariables<'_> {
    fn text(
        &self,
        value: &str,
        request_variables: &[Variable],
        secrets: &DraftSecrets,
    ) -> Result<String, String> {
        compile_auth_headers(
            &[KeyValueRow::enabled("X-Value", value)],
            request_variables,
            self.collection,
            self.folders,
            &CompileContext {
                global: self.globals,
                environment: self.environment,
                data: &[],
                local: &[],
                secrets,
                environment_base_url: self.base_url,
                environment_auth: None,
            },
        )
        .map(|rows| rows[0].1.clone())
        .map_err(|error| error.to_string())
    }

    pub fn resolve_token_request(
        &self,
        request: &mut OAuthTokenRequest,
        variables: &[Variable],
        secrets: &DraftSecrets,
    ) -> Result<(), String> {
        request.token_url = self.text(&request.token_url, variables, secrets)?;
        request.client_id = self.text(&request.client_id, variables, secrets)?;
        for field in [
            &mut request.username,
            &mut request.scope,
            &mut request.redirect_uri,
        ] {
            if let Some(value) = field.as_mut() {
                *value = self.text(value, variables, secrets)?;
            }
        }
        Ok(())
    }

    /// Resolve browser-acquisition parameters without replacing saved templates.
    pub fn resolve_auth(
        &self,
        auth: &AuthConfig,
        variables: &[Variable],
        secrets: &DraftSecrets,
    ) -> Result<AuthConfig, String> {
        let mut auth = auth.clone();
        match &mut auth {
            AuthConfig::OAuth2ClientCredentials {
                token_endpoint,
                client_id,
                scopes,
                ..
            }
            | AuthConfig::OAuth2Password {
                token_endpoint,
                client_id,
                scopes,
                ..
            } => {
                *token_endpoint = self.text(token_endpoint, variables, secrets)?;
                *client_id = self.text(client_id, variables, secrets)?;
                for scope in scopes {
                    *scope = self.text(scope, variables, secrets)?;
                }
            }
            AuthConfig::OAuth2AuthorizationCodePkce {
                authorization_endpoint,
                token_endpoint,
                client_id,
                scopes,
                redirect_uri,
                ..
            } => {
                *authorization_endpoint = self.text(authorization_endpoint, variables, secrets)?;
                *token_endpoint = self.text(token_endpoint, variables, secrets)?;
                *client_id = self.text(client_id, variables, secrets)?;
                *redirect_uri = self.text(redirect_uri, variables, secrets)?;
                for scope in scopes {
                    *scope = self.text(scope, variables, secrets)?;
                }
            }
            _ => {}
        }
        if let AuthConfig::OAuth2Password { username, .. } = &mut auth {
            *username = self.text(username, variables, secrets)?;
        }
        Ok(auth)
    }

    pub fn headers(
        &self,
        auth: &AuthConfig,
        request_variables: &[Variable],
        secrets: &DraftSecrets,
    ) -> Result<Vec<(String, String)>, String> {
        compile_auth_headers(
            oauth_headers(auth),
            request_variables,
            self.collection,
            self.folders,
            &CompileContext {
                global: self.globals,
                environment: self.environment,
                data: &[],
                local: &[],
                secrets,
                environment_base_url: self.base_url,
                environment_auth: None,
            },
        )
        .map_err(|error| error.to_string())
    }
}

/// A token-endpoint exchange a managed OAuth cache needs before a send.
pub enum OAuthRefresh {
    ClientCredentials {
        request: OAuthTokenRequest,
        secret: SecretRef,
    },
    Password {
        request: OAuthTokenRequest,
        client_secret: Option<SecretRef>,
        password: SecretRef,
    },
    RefreshToken {
        request: OAuthTokenRequest,
        secret: SecretRef,
        client_secret: Option<SecretRef>,
    },
}

/// The exchange that renews `auth`'s cache when it is missing or about to
/// expire — `None` when the cache is still good or `auth` is not managed.
/// Client credentials are fetched the first time as well, since that needs
/// no browser; a PKCE grant without a refresh token has to be authorized
/// again by hand.
pub fn oauth_refresh_needed(
    auth: &AuthConfig,
    allow_private_network: bool,
    now: i64,
) -> Result<Option<OAuthRefresh>, String> {
    // A token the issuer gave no lifetime for is kept until it is replaced.
    let stale = |expires_at: &Option<i64>| {
        expires_at
            .is_some_and(|expires_at| expires_at <= now.saturating_add(OAUTH_EXPIRY_SAFETY_SECS))
    };
    let token_request =
        |flow: &str, token_endpoint: &str, client_id: &str, scopes: &[String]| OAuthTokenRequest {
            flow: flow.into(),
            token_url: token_endpoint.to_string(),
            headers: oauth_headers(auth)
                .iter()
                .filter(|row| row.enabled)
                .map(|row| (row.key.clone(), row.value.clone()))
                .collect(),
            client_id: client_id.to_string(),
            client_secret: None,
            scope: (!scopes.is_empty()).then(|| scopes.join(" ")),
            code: None,
            redirect_uri: None,
            code_verifier: None,
            refresh_token: None,
            username: None,
            password: None,
            expected_state: None,
            callback_state: None,
            allow_private_network,
        };
    Ok(match auth {
        AuthConfig::OAuth2ClientCredentials {
            token_endpoint,
            client_id,
            client_secret,
            scopes,
            access_token,
            expires_at,
            ..
        } if access_token.is_none() || stale(expires_at) => Some(OAuthRefresh::ClientCredentials {
            request: token_request("client_credentials", token_endpoint, client_id, scopes),
            secret: client_secret.clone(),
        }),
        AuthConfig::OAuth2AuthorizationCodePkce {
            token_endpoint,
            client_id,
            scopes,
            refresh_token: Some(refresh_token),
            access_token: Some(_),
            expires_at,
            ..
        } if stale(expires_at) => Some(OAuthRefresh::RefreshToken {
            request: token_request("refresh_token", token_endpoint, client_id, scopes),
            secret: refresh_token.clone(),
            client_secret: None,
        }),
        AuthConfig::OAuth2Password {
            token_endpoint,
            client_id,
            client_secret,
            refresh_token: Some(refresh_token),
            access_token: Some(_),
            scopes,
            expires_at,
            ..
        } if stale(expires_at) => Some(OAuthRefresh::RefreshToken {
            request: token_request("refresh_token", token_endpoint, client_id, scopes),
            secret: refresh_token.clone(),
            client_secret: client_secret.clone(),
        }),
        AuthConfig::OAuth2Password {
            token_endpoint,
            client_id,
            client_secret,
            username,
            password,
            scopes,
            access_token,
            expires_at,
            ..
        } if access_token.is_none() || stale(expires_at) => {
            let mut request = token_request("password", token_endpoint, client_id, scopes);
            request.username = Some(username.clone());
            Some(OAuthRefresh::Password {
                request,
                client_secret: client_secret.clone(),
                password: password.clone(),
            })
        }
        AuthConfig::OAuth2AuthorizationCodePkce {
            refresh_token: None,
            access_token: Some(_),
            expires_at,
            ..
        } if stale(expires_at) => {
            return Err(
                "OAuth access token expired and no refresh token is available; send the request from Compose to authorize in the browser again."
                    .into(),
            );
        }
        _ => None,
    })
}

/// Put a freshly issued token into the vault under `scope`'s deterministic
/// references and record it on `auth`. Returns the references so a draft
/// that still caches the stale values can forget them.
pub fn store_oauth_token(
    auth: &mut AuthConfig,
    scope: &str,
    token: &OAuthTokenResponse,
    workspace: &WorkspaceId,
    secret_store: &dyn SecretStore,
    now: i64,
) -> Result<(SecretRef, SecretRef), String> {
    let access_ref = SecretRef::new(format!("workbench.{scope}.auth.token"))?;
    let refresh_ref = SecretRef::new(format!("workbench.{scope}.auth.refresh_token"))?;
    secret_store
        .set_secret(
            workspace,
            &access_ref,
            SecretValue::new(&token.access_token),
        )
        .map_err(|error| error.to_string())?;
    if let Some(refresh_token) = &token.refresh_token {
        secret_store
            .set_secret(workspace, &refresh_ref, SecretValue::new(refresh_token))
            .map_err(|error| error.to_string())?;
    }
    let expires_at = token
        .expires_in
        .and_then(|seconds| i64::try_from(seconds).ok())
        .map(|seconds| now.saturating_add(seconds));
    match auth {
        AuthConfig::OAuth2ClientCredentials {
            access_token,
            expires_at: stored_expiry,
            ..
        } => {
            *access_token = Some(access_ref.clone());
            *stored_expiry = expires_at;
        }
        AuthConfig::OAuth2AuthorizationCodePkce {
            access_token,
            refresh_token,
            expires_at: stored_expiry,
            ..
        }
        | AuthConfig::OAuth2Password {
            access_token,
            refresh_token,
            expires_at: stored_expiry,
            ..
        } => {
            *access_token = Some(access_ref.clone());
            if token.refresh_token.is_some() {
                *refresh_token = Some(refresh_ref.clone());
            }
            *stored_expiry = expires_at;
        }
        _ => return Err("only OAuth 2 auth caches a token".into()),
    }
    Ok((access_ref, refresh_ref))
}

/// Renew whichever managed OAuth cache `auth` carries, whoever owns it:
/// `scope` is `request.<id>` or `environment.<id>`, the vault scope of the
/// cache. `true` when a token was fetched; the caller persists `auth`.
#[allow(clippy::too_many_arguments)]
pub fn refresh_expired_oauth_auth_with(
    auth: &mut AuthConfig,
    scope: &str,
    allow_private_network: bool,
    secrets: &mut DraftSecrets,
    workspace: &WorkspaceId,
    secret_store: &dyn SecretStore,
    now: i64,
    exchange: impl FnOnce(OAuthTokenRequest) -> Result<OAuthTokenResponse, String>,
) -> Result<bool, String> {
    let Some(refresh) = oauth_refresh_needed(auth, allow_private_network, now)? else {
        return Ok(false);
    };
    let token = match refresh {
        OAuthRefresh::ClientCredentials {
            mut request,
            secret,
        } => {
            request.client_secret = Some(secrets.resolve(&secret)?);
            exchange(request)?
        }
        OAuthRefresh::Password {
            mut request,
            client_secret,
            password,
        } => {
            request.client_secret = client_secret
                .map(|secret| secrets.resolve(&secret))
                .transpose()?;
            request.password = Some(secrets.resolve(&password)?);
            if let Some(username) = request.username.as_mut()
                && let Some(reference) = crate::vault::parse_vault_expression(username)?
            {
                *username = secrets.resolve(&reference)?;
            }
            exchange(request)?
        }
        OAuthRefresh::RefreshToken {
            mut request,
            secret,
            client_secret,
        } => {
            request.client_secret = client_secret
                .map(|secret| secrets.resolve(&secret))
                .transpose()?;
            request.refresh_token = Some(secrets.resolve(&secret)?);
            exchange(request)?
        }
    };
    let (access_ref, refresh_ref) =
        store_oauth_token(auth, scope, &token, workspace, secret_store, now)?;
    // The editor draft may still cache the expired `token=` value under this
    // deterministic reference. Remove it so subsequent compilation resolves
    // the replacement just written to the vault.
    secrets.forget(&access_ref);
    if token.refresh_token.is_some() {
        secrets.forget(&refresh_ref);
    }
    Ok(true)
}

pub fn refresh_expired_oauth_token_with(
    definition: &mut SavedRequest,
    secrets: &mut DraftSecrets,
    workspace: &WorkspaceId,
    store: &WorkbenchStore,
    secret_store: &dyn SecretStore,
    now: i64,
    exchange: impl FnOnce(OAuthTokenRequest) -> Result<OAuthTokenResponse, String>,
) -> Result<bool, String> {
    let scope = format!("request.{}", definition.id.as_str());
    let refreshed = refresh_expired_oauth_auth_with(
        &mut definition.auth,
        &scope,
        definition.settings.allow_private_network,
        secrets,
        workspace,
        secret_store,
        now,
        exchange,
    )?;
    if refreshed {
        store
            .upsert_request(definition)
            .map_err(|error| error.to_string())?;
    }
    Ok(refreshed)
}

pub fn refresh_expired_oauth_token(
    definition: &mut SavedRequest,
    secrets: &mut DraftSecrets,
    workspace: &WorkspaceId,
    store: &WorkbenchStore,
    secret_store: &dyn SecretStore,
    transport: &dyn WorkbenchTransport,
) -> Result<bool, String> {
    let globals = store
        .global_variables(workspace)
        .map_err(|error| error.to_string())?;
    refresh_expired_oauth_token_in_context(
        definition,
        secrets,
        workspace,
        store,
        secret_store,
        transport,
        OAuthVariables {
            globals: &globals,
            ..Default::default()
        },
    )
}

pub fn refresh_expired_oauth_token_in_context(
    definition: &mut SavedRequest,
    secrets: &mut DraftSecrets,
    workspace: &WorkspaceId,
    store: &WorkbenchStore,
    secret_store: &dyn SecretStore,
    transport: &dyn WorkbenchTransport,
    variables: OAuthVariables<'_>,
) -> Result<bool, String> {
    if oauth_refresh_needed(
        &definition.auth,
        definition.settings.allow_private_network,
        now_seconds(),
    )?
    .is_none()
    {
        return Ok(false);
    }
    let headers = variables.headers(&definition.auth, &definition.variables, secrets)?;
    let request_variables = definition.variables.clone();
    let request_secrets = secrets.clone();
    refresh_expired_oauth_token_with(
        definition,
        secrets,
        workspace,
        store,
        secret_store,
        now_seconds(),
        |mut request| {
            variables.resolve_token_request(&mut request, &request_variables, &request_secrets)?;
            request.headers = headers;
            transport.exchange_oauth_token(request)
        },
    )
}

/// Whether `saved` and `live` describe the same OAuth client or sign-in
/// request, cache aside — when they do, a cache fetched for one belongs on
/// the other.
pub fn same_managed_auth_source(saved: &AuthConfig, live: &AuthConfig) -> bool {
    let mut probe = saved.clone();
    preserve_managed_oauth_state(live, &mut probe);
    match (&mut probe, live) {
        (
            AuthConfig::Login { headers, .. },
            AuthConfig::Login {
                headers: live_headers,
                ..
            },
        )
        | (
            AuthConfig::OAuth2ClientCredentials { headers, .. },
            AuthConfig::OAuth2ClientCredentials {
                headers: live_headers,
                ..
            },
        )
        | (
            AuthConfig::OAuth2Password { headers, .. },
            AuthConfig::OAuth2Password {
                headers: live_headers,
                ..
            },
        )
        | (
            AuthConfig::OAuth2AuthorizationCodePkce { headers, .. },
            AuthConfig::OAuth2AuthorizationCodePkce {
                headers: live_headers,
                ..
            },
        ) if same_auth_headers(headers, live_headers) => *headers = live_headers.clone(),
        _ => {}
    }
    probe == *live
}

/// Renew the active environment's OAuth cache when the send resolves to it,
/// writing the cache back onto the saved environment when the Envs editor
/// still describes the same client.
pub fn refresh_environment_oauth(input: &mut StandaloneSendInput) -> Result<(), String> {
    if !send_auth_owner(input).1 {
        return Ok(());
    }
    if oauth_refresh_needed(
        &input.environment_auth,
        input.definition.settings.allow_private_network,
        now_seconds(),
    )?
    .is_none()
    {
        return Ok(());
    }
    let (environment, _) = super::secrets::parse_session_variables(
        &input.environment_source,
        &input.environment_scope,
    )?;
    let folder_refs = input.folders.iter().collect::<Vec<_>>();
    let globals = input
        .store
        .global_variables(&input.workspace)
        .map_err(|e| e.to_string())?;
    let variables = OAuthVariables {
        globals: &globals,
        collection: input.collection.as_ref(),
        folders: &folder_refs,
        environment: &environment,
        base_url: Some(&input.environment_base_url),
    };
    let headers = variables.headers(
        &input.environment_auth,
        &input.definition.variables,
        &input.secrets,
    )?;
    let request_secrets = input.secrets.clone();
    let request_variables = input.definition.variables.clone();
    let refreshed = refresh_expired_oauth_auth_with(
        &mut input.environment_auth,
        &input.environment_scope,
        input.definition.settings.allow_private_network,
        &mut input.secrets,
        &input.workspace,
        input.secret_store.as_ref(),
        now_seconds(),
        |mut request| {
            variables.resolve_token_request(&mut request, &request_variables, &request_secrets)?;
            request.headers = headers;
            input.transport.exchange_oauth_token(request)
        },
    )?;
    if !refreshed {
        return Ok(());
    }
    if let Some(saved) = input
        .environment
        .as_ref()
        .filter(|saved| same_managed_auth_source(&saved.auth, &input.environment_auth))
    {
        let environment = Environment {
            auth: input.environment_auth.clone(),
            ..saved.clone()
        };
        input
            .store
            .upsert_environment(&environment)
            .map_err(|error| error.to_string())?;
        input.environment = Some(environment);
    }
    Ok(())
}

/// The auth a send resolves to and where its cache lives: the environment's
/// scope when the chain ends there, else the request's own.
pub fn send_auth_owner(input: &StandaloneSendInput) -> (&AuthConfig, bool) {
    let folder_refs = input.folders.iter().collect::<Vec<_>>();
    let effective = effective_auth(
        &input.definition,
        &folder_refs,
        input.collection.as_ref(),
        Some(&input.environment_auth),
    );
    let environment_owned = std::ptr::eq(effective, &input.environment_auth);
    (effective, environment_owned)
}

/// Whether `auth` is a PKCE client only the browser can get a token for.
pub fn pkce_needs_browser(auth: &AuthConfig, now: i64) -> bool {
    match auth {
        AuthConfig::OAuth2AuthorizationCodePkce {
            refresh_token: None,
            access_token,
            expires_at,
            ..
        } => {
            access_token.is_none()
                || expires_at.is_some_and(|expires_at| {
                    expires_at <= now.saturating_add(OAUTH_EXPIRY_SAFETY_SECS)
                })
        }
        _ => false,
    }
}

/// Finish the grant the desktop app planned and cache the
/// token where the auth lives — on the saved request, or on the saved
/// environment when the Envs editor still describes the same client.
pub fn finish_browser_authorization(input: &mut StandaloneSendInput) -> Result<(), String> {
    let Some(acquisition) = input.browser_authorization.take() else {
        return Ok(());
    };
    let (_, environment_owned) = send_auth_owner(input);
    if environment_owned {
        (acquisition.complete)(
            &mut input.environment_auth,
            &input.environment_scope,
            &input.secrets,
            &input.workspace,
            input.secret_store.as_ref(),
        )?;
        if let Some(saved) = input
            .environment
            .as_ref()
            .filter(|saved| same_managed_auth_source(&saved.auth, &input.environment_auth))
        {
            let environment = Environment {
                auth: input.environment_auth.clone(),
                ..saved.clone()
            };
            input
                .store
                .upsert_environment(&environment)
                .map_err(|error| error.to_string())?;
            input.environment = Some(environment);
        }
    } else {
        let scope = format!("request.{}", input.definition.id.as_str());
        (acquisition.complete)(
            &mut input.definition.auth,
            &scope,
            &input.secrets,
            &input.workspace,
            input.secret_store.as_ref(),
        )?;
        input
            .store
            .upsert_request(&input.definition)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::workspace::WorkspaceData;
    use crate::{Body, HttpMethod, MemorySecretStore, RequestId, RequestSettings, Scripts};
    use std::sync::Arc;

    #[test]
    fn globals_resolve_oauth_endpoints_headers_and_browser_fields_with_scope_precedence() {
        let (globals, secrets) = super::super::secrets::parse_session_variables("endpoint=https://identity.test/token\nclient=global-client\ntenant=global\npermission=read", "globals").unwrap();
        let (environment, _) =
            super::super::secrets::parse_session_variables("tenant=environment", "environment")
                .unwrap();
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "kind": "o_auth2_client_credentials", "token_endpoint": "{{endpoint}}",
            "client_id": "{{client}}", "client_secret": "client-secret", "scopes": ["{{permission}}"]
        })).unwrap();
        let variables = OAuthVariables {
            globals: &globals,
            environment: &environment,
            ..Default::default()
        };
        let resolved = variables.resolve_auth(&auth, &[], &secrets).unwrap();
        let AuthConfig::OAuth2ClientCredentials {
            token_endpoint,
            client_id,
            scopes,
            ..
        } = resolved
        else {
            unreachable!()
        };
        assert_eq!(token_endpoint, "https://identity.test/token");
        assert_eq!(client_id, "global-client");
        assert_eq!(scopes, ["read"]);
        assert_eq!(
            variables.text("{{tenant}}", &[], &secrets).unwrap(),
            "environment"
        );
        let OAuthRefresh::ClientCredentials { mut request, .. } =
            oauth_refresh_needed(&auth, false, 0).unwrap().unwrap()
        else {
            unreachable!()
        };
        variables
            .resolve_token_request(&mut request, &[], &secrets)
            .unwrap();
        assert_eq!(request.token_url, token_endpoint);
        assert_eq!(request.client_id, client_id);
        assert!(
            serde_json::to_string(&auth)
                .unwrap()
                .contains("{{endpoint}}")
        );
    }

    #[test]
    fn oauth_headers_resolve_request_environment_and_vault_values() {
        let mut auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "kind": "o_auth2_client_credentials", "token_endpoint": "https://identity.test/token",
            "client_id": "desktop", "client_secret": "client-secret"
        }))
        .unwrap();
        let AuthConfig::OAuth2ClientCredentials { headers, .. } = &mut auth else {
            unreachable!()
        };
        *headers = vec![
            KeyValueRow::enabled("X-Tenant", "{{tenant}}"),
            KeyValueRow::enabled("X-Region", "{{region}}"),
            KeyValueRow::enabled("Authorization", "Bearer {{vault.oauth-gateway}}"),
        ];
        let mut disabled = KeyValueRow::enabled("X-Disabled", "{{missing}}");
        disabled.enabled = false;
        headers.push(disabled);
        let (environment, _) = super::super::secrets::parse_session_variables(
            "tenant=environment\nregion=west",
            "environment.test",
        )
        .unwrap();
        let (request, _) =
            super::super::secrets::parse_session_variables("tenant=request", "request.test")
                .unwrap();
        let mut secrets = DraftSecrets::default();
        secrets.insert(
            &crate::vault::vault_secret_reference("oauth-gateway").unwrap(),
            "gateway-secret",
        );
        let resolved = OAuthVariables {
            globals: &[],
            environment: &environment,
            ..Default::default()
        }
        .headers(&auth, &request, &secrets)
        .unwrap();
        assert_eq!(
            resolved,
            vec![
                ("X-Tenant".into(), "request".into()),
                ("X-Region".into(), "west".into()),
                ("Authorization".into(), "Bearer gateway-secret".into())
            ]
        );
        assert!(
            serde_json::to_string(&auth)
                .unwrap()
                .contains("{{vault.oauth-gateway}}")
        );
        assert!(
            !serde_json::to_string(&auth)
                .unwrap()
                .contains("Bearer gateway-secret")
        );
    }

    #[test]
    fn managed_auth_headers_preserve_cache_across_editor_row_ids_and_invalidate_changes() {
        for value in [
            serde_json::json!({"kind":"o_auth2_client_credentials", "token_endpoint":"https://identity.test/token", "client_id":"desktop", "client_secret":"client-secret"}),
            serde_json::json!({"kind":"o_auth2_password", "token_endpoint":"https://identity.test/token", "client_id":"desktop", "username":"user", "password":"password"}),
            serde_json::json!({"kind":"o_auth2_authorization_code_pkce", "authorization_endpoint":"https://identity.test/authorize", "token_endpoint":"https://identity.test/token", "client_id":"desktop", "redirect_uri":"http://127.0.0.1:18765/callback"}),
            serde_json::json!({"kind":"login", "url":"https://identity.test/login", "token_path":"access_token"}),
        ] {
            let old: AuthConfig = serde_json::from_value(value.clone()).unwrap();
            assert!(
                !serde_json::to_value(old)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .contains_key("headers"),
                "old saves default to empty headers"
            );
            let mut original = value.clone();
            original["headers"] =
                serde_json::to_value(vec![KeyValueRow::enabled("X-Tenant", "team")]).unwrap();
            original["access_token"] = serde_json::json!("cached-token");
            original["expires_at"] = serde_json::json!(3600);
            let original: AuthConfig = serde_json::from_value(original).unwrap();
            let mut rebuilt = value.clone();
            rebuilt["headers"] =
                serde_json::to_value(vec![KeyValueRow::enabled("X-Tenant", "team")]).unwrap();
            let mut rebuilt: AuthConfig = serde_json::from_value(rebuilt).unwrap();
            preserve_managed_oauth_state(&original, &mut rebuilt);
            assert!(same_managed_auth_source(&original, &rebuilt));
            let mut changed = value;
            changed["headers"] =
                serde_json::to_value(vec![KeyValueRow::enabled("X-Tenant", "other")]).unwrap();
            let mut changed: AuthConfig = serde_json::from_value(changed).unwrap();
            preserve_managed_oauth_state(&original, &mut changed);
            assert!(!same_managed_auth_source(&original, &changed));
            assert!(serde_json::to_value(changed).unwrap()["access_token"].is_null());
        }
    }

    #[test]
    fn expired_client_credentials_token_is_refreshed_and_persisted_before_compile() {
        let workspace = WorkspaceId::new("/test/oauth-refresh").unwrap();
        let path = std::env::temp_dir().join(format!(
            "agentops-core-oauth-refresh-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let mut data = WorkspaceData::open(&path, workspace.clone()).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        let secret_store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let request_id = RequestId::new();
        let client_secret = SecretRef::new(format!(
            "workbench.request.{}.auth.client_secret",
            request_id.as_str()
        ))
        .unwrap();
        let stale_access = SecretRef::new(format!(
            "workbench.request.{}.auth.token",
            request_id.as_str()
        ))
        .unwrap();
        let refresh_now = now_seconds();
        let mut request = SavedRequest {
            id: request_id.clone(),
            collection_id: collection_id.clone(),
            folder_id: None,
            name: "OAuth".into(),
            method: HttpMethod::get(),
            url: "https://example.test/private".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::OAuth2ClientCredentials {
                headers: Vec::new(),
                token_endpoint: "https://identity.example.test/token".into(),
                client_id: "desktop-client".into(),
                client_secret: client_secret.clone(),
                scopes: vec!["read".into(), "write".into()],
                access_token: Some(stale_access.clone()),
                // Inside the 30-second refresh safety window.
                expires_at: Some(refresh_now.saturating_add(25)),
            },
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        data.store.upsert_request(&request).unwrap();
        let mut secrets = DraftSecrets::with_store(secret_store.clone(), workspace.clone());
        secrets.insert(&client_secret, "client-secret-value");
        secrets.insert(&stale_access, "stale-token");
        secrets.persist(secret_store.as_ref(), &workspace).unwrap();

        assert!(
            refresh_expired_oauth_token_with(
                &mut request,
                &mut secrets,
                &workspace,
                data.store.as_ref(),
                secret_store.as_ref(),
                refresh_now,
                |token_request| {
                    assert_eq!(token_request.flow, "client_credentials");
                    assert_eq!(
                        token_request.client_secret.as_deref(),
                        Some("client-secret-value")
                    );
                    assert_eq!(token_request.scope.as_deref(), Some("read write"));
                    Ok(OAuthTokenResponse {
                        access_token: "fresh-token".into(),
                        expires_in: Some(60),
                        refresh_token: None,
                    })
                },
            )
            .unwrap()
        );

        let AuthConfig::OAuth2ClientCredentials {
            access_token: Some(access_ref),
            expires_at,
            ..
        } = &request.auth
        else {
            panic!("expected refreshed client credentials auth");
        };
        assert_eq!(*expires_at, Some(refresh_now.saturating_add(60)));
        assert_eq!(
            secret_store
                .get_secret(&workspace, access_ref)
                .unwrap()
                .expose_secret(),
            "fresh-token"
        );
        let (compiled, _) = crate::compile_request(
            &request,
            None,
            &crate::CompileContext {
                global: &[],
                environment: &[],
                data: &[],
                local: &[],
                secrets: &secrets,
                environment_base_url: None,
                environment_auth: None,
            },
        )
        .unwrap();
        assert!(compiled.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer fresh-token"
        }));
        let reopened = WorkspaceData::hydrate(data.store, workspace).unwrap();
        assert!(reopened.requests.iter().any(|saved| saved == &request));
    }

    #[test]
    fn only_a_pkce_client_without_a_usable_token_needs_the_browser_on_send() {
        let now = now_seconds();
        let pkce = AuthConfig::OAuth2AuthorizationCodePkce {
            headers: Vec::new(),
            authorization_endpoint: "https://identity.example.test/authorize".into(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "desktop-client".into(),
            scopes: Vec::new(),
            redirect_uri: "http://127.0.0.1:0/callback".into(),
            access_token: None,
            refresh_token: None,
            expires_at: None,
        };
        assert!(pkce_needs_browser(&pkce, now), "no token yet");
        let mut cached = pkce.clone();
        store_oauth_token(
            &mut cached,
            "request.r1",
            &OAuthTokenResponse {
                access_token: "t".into(),
                expires_in: Some(600),
                refresh_token: None,
            },
            &WorkspaceId::new("/test/pkce").unwrap(),
            &MemorySecretStore::new(),
            now,
        )
        .unwrap();
        assert!(!pkce_needs_browser(&cached, now), "a good token is reused");
        assert!(
            pkce_needs_browser(&cached, now + 600),
            "an expired token with no refresh token goes back to the browser"
        );
        let mut refreshable = pkce.clone();
        store_oauth_token(
            &mut refreshable,
            "request.r1",
            &OAuthTokenResponse {
                access_token: "t".into(),
                expires_in: Some(1),
                refresh_token: Some("r".into()),
            },
            &WorkspaceId::new("/test/pkce").unwrap(),
            &MemorySecretStore::new(),
            now,
        )
        .unwrap();
        assert!(
            !pkce_needs_browser(&refreshable, now + 600),
            "a refresh token renews silently at the token endpoint"
        );
        let client = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "c".into(),
            client_secret: SecretRef::new("workbench.request.r1.auth.secret").unwrap(),
            scopes: Vec::new(),
            access_token: None,
            expires_at: None,
        };
        assert!(!pkce_needs_browser(&client, now));
        assert!(!pkce_needs_browser(&AuthConfig::None, now));
    }

    #[test]
    fn managed_oauth_state_survives_only_an_equivalent_editor_rebuild() {
        let client_secret = SecretRef::new("oauth.client-secret").unwrap();
        let access_token = SecretRef::new("oauth.access-token").unwrap();
        let original = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "desktop-client".into(),
            client_secret: client_secret.clone(),
            scopes: vec!["read".into()],
            access_token: Some(access_token.clone()),
            expires_at: Some(900),
        };
        let mut equivalent = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "desktop-client".into(),
            client_secret: client_secret.clone(),
            scopes: vec!["read".into()],
            access_token: None,
            expires_at: None,
        };
        preserve_managed_oauth_state(&original, &mut equivalent);
        assert!(matches!(
            equivalent,
            AuthConfig::OAuth2ClientCredentials {
                access_token: Some(ref token),
                expires_at: Some(900),
                ..
            } if token == &access_token
        ));

        let mut changed_endpoint = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://other.example.test/token".into(),
            client_id: "desktop-client".into(),
            client_secret,
            scopes: vec!["read".into()],
            access_token: None,
            expires_at: None,
        };
        preserve_managed_oauth_state(&original, &mut changed_endpoint);
        assert!(matches!(
            changed_endpoint,
            AuthConfig::OAuth2ClientCredentials {
                access_token: None,
                expires_at: None,
                ..
            }
        ));
    }

    #[test]
    fn consecutive_pkce_refreshes_use_the_rotated_vault_token() {
        let workspace = WorkspaceId::new("/test/oauth-refresh-rotation").unwrap();
        let path = std::env::temp_dir().join(format!(
            "agentops-core-oauth-refresh-rotation-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let mut data = WorkspaceData::open(&path, workspace.clone()).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        let secret_store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let refresh_now = now_seconds();
        let request_id = RequestId::new();
        let stale_access = SecretRef::new(format!(
            "workbench.request.{}.auth.token",
            request_id.as_str()
        ))
        .unwrap();
        let old_refresh = SecretRef::new(format!(
            "workbench.request.{}.auth.refresh_token",
            request_id.as_str()
        ))
        .unwrap();
        let mut request = SavedRequest {
            id: request_id,
            collection_id,
            folder_id: None,
            name: "PKCE rotation".into(),
            method: HttpMethod::get(),
            url: "https://example.test/private".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::OAuth2AuthorizationCodePkce {
                headers: Vec::new(),
                authorization_endpoint: "https://identity.example.test/authorize".into(),
                token_endpoint: "https://identity.example.test/token".into(),
                client_id: "desktop-client".into(),
                scopes: Vec::new(),
                redirect_uri: "http://127.0.0.1/callback".into(),
                access_token: Some(stale_access.clone()),
                refresh_token: Some(old_refresh.clone()),
                expires_at: None,
            },
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let AuthConfig::OAuth2AuthorizationCodePkce { expires_at, .. } = &mut request.auth else {
            panic!("expected PKCE auth");
        };
        *expires_at = Some(refresh_now.saturating_add(25));
        data.store.upsert_request(&request).unwrap();
        let mut secrets = DraftSecrets::with_store(secret_store.clone(), workspace.clone());
        secrets.insert(&stale_access, "stale-access");
        secrets.insert(&old_refresh, "old-refresh");
        secrets.persist(secret_store.as_ref(), &workspace).unwrap();

        assert!(
            refresh_expired_oauth_token_with(
                &mut request,
                &mut secrets,
                &workspace,
                data.store.as_ref(),
                secret_store.as_ref(),
                refresh_now,
                |token_request| {
                    assert_eq!(token_request.refresh_token.as_deref(), Some("old-refresh"));
                    Ok(OAuthTokenResponse {
                        access_token: "access-one".into(),
                        expires_in: Some(1),
                        refresh_token: Some("rotated-refresh".into()),
                    })
                },
            )
            .unwrap()
        );
        assert!(
            refresh_expired_oauth_token_with(
                &mut request,
                &mut secrets,
                &workspace,
                data.store.as_ref(),
                secret_store.as_ref(),
                refresh_now.saturating_add(2),
                |token_request| {
                    assert_eq!(
                        token_request.refresh_token.as_deref(),
                        Some("rotated-refresh")
                    );
                    Ok(OAuthTokenResponse {
                        access_token: "access-two".into(),
                        expires_in: Some(60),
                        refresh_token: None,
                    })
                },
            )
            .unwrap()
        );
    }
}
