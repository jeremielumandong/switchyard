use super::{
    ApiKeyLocation, AuthConfig, AwsSigV4Signing, Body, Collection, CollectionRun, Environment,
    Example, Exchange, Folder, KeyValueRow, MultipartRow, MultipartValue, PreparedBody,
    PreparedMultipartPart, PreparedMultipartValue, PreparedRequest, RedactedRequestSnapshot,
    ReplayRequestSnapshot, ResponseSnapshot, SavedRequest, Scripts, TestResult, Variable,
    VariableValue,
};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use base64::Engine as _;
use chrono::{DateTime, Local, SecondsFormat, Utc, format::StrftimeItems};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

#[path = "compile_samples.rs"]
mod samples;
use samples::sample_variable;

pub use crate::secrets::SecretResolver;

pub struct CompileContext<'a> {
    pub global: &'a [Variable],
    pub environment: &'a [Variable],
    pub data: &'a [Variable],
    pub local: &'a [Variable],
    pub secrets: &'a dyn SecretResolver,
    /// The active environment's base URL: prefixed onto relative request
    /// URLs and readable as `{{base_url}}` unless a variable shadows it.
    pub environment_base_url: Option<&'a str>,
    /// The active environment's auth — the last stop of the `Inherit` chain.
    pub environment_auth: Option<&'a AuthConfig>,
}

/// The variable names `environment_base_url` answers to when no scope defines
/// them. `base_url` is the documented one; `baseUrl` is what Postman exports
/// usually call it.
pub const BASE_URL_VARIABLES: [&str; 2] = ["base_url", "baseUrl"];

/// Whether a rendered request URL should be prefixed with the base URL:
/// anything without a scheme, so `/pets`, `pets` and `pets?x=1` all qualify.
pub fn is_relative_url(url: &str) -> bool {
    let url = url.trim();
    !url.is_empty()
        && !url
            .split_once("://")
            .is_some_and(|(scheme, _)| !scheme.is_empty() && !scheme.contains('/'))
}

/// `base + path`, with exactly one `/` between them and query-only paths
/// (`?page=2`) appended directly.
pub fn join_base_url(base: &str, path: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let path = path.trim();
    if path.starts_with('?') || path.starts_with('#') {
        format!("{base}{path}")
    } else {
        format!("{base}/{}", path.trim_start_matches('/'))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompileError {
    InvalidUrl(String),
    InvalidInput(String),
    UnresolvedVariable(String),
    VariableCycle(Vec<String>),
    Secret(String),
    Unsupported(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(message)
            | Self::InvalidInput(message)
            | Self::Secret(message)
            | Self::Unsupported(message) => formatter.write_str(message),
            Self::UnresolvedVariable(name) => write!(formatter, "unresolved variable {name:?}"),
            Self::VariableCycle(path) => write!(formatter, "variable cycle: {}", path.join(" -> ")),
        }
    }
}

impl std::error::Error for CompileError {}

/// Resolve a built-in using a caller-owned clock snapshot. Callers cache each
/// expression for the lifetime of a compilation or script execution.
pub fn runtime_variable(name: &str, timestamp_millis: i64) -> Result<String, CompileError> {
    let runtime_now = DateTime::<Utc>::from_timestamp_millis(timestamp_millis)
        .ok_or_else(|| CompileError::InvalidInput("invalid runtime timestamp".into()))?;
    runtime_variable_at(name, runtime_now)
}

fn runtime_variable_at(name: &str, runtime_now: DateTime<Utc>) -> Result<String, CompileError> {
    if let Some(value) = sample_variable(name, runtime_now) {
        return Ok(value);
    }
    let value = match name {
        "$isoTimestamp" => runtime_now.to_rfc3339_opts(SecondsFormat::Millis, true),
        "$timestamp" => runtime_now.timestamp().to_string(),
        "$timestampMs" => runtime_now.timestamp_millis().to_string(),
        "$date" => runtime_now.format("%Y-%m-%d").to_string(),
        "$time" => runtime_now.format("%H:%M:%S").to_string(),
        "$uuid" | "$guid" | "$randomUUID" => uuid::Uuid::new_v4().to_string(),
        "$randomInt" => {
            // The first 32 UUID bits are random (version/variant bits are
            // elsewhere). Rejection sampling avoids modulo bias.
            let limit = u32::MAX - u32::MAX % 1001;
            loop {
                let bytes = uuid::Uuid::new_v4();
                let b = bytes.as_bytes();
                let random = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                if random < limit {
                    break (random % 1001).to_string();
                }
            }
        }
        _ => {
            let (local, format) = if let Some(format) = name.strip_prefix("$datetime:") {
                (false, format)
            } else if let Some(format) = name.strip_prefix("$localDatetime:") {
                (true, format)
            } else {
                return Err(CompileError::UnresolvedVariable(name.to_string()));
            };
            let items = StrftimeItems::new(format).parse().map_err(|_| {
                CompileError::InvalidInput(format!("invalid date/time format in {name:?}"))
            })?;
            if format.is_empty() {
                return Err(CompileError::InvalidInput(
                    "date/time format cannot be empty".into(),
                ));
            }
            // write_to also catches formats unsupported by the datetime,
            // without the panic caused by Display::to_string on fmt::Error.
            let mut rendered = String::new();
            let result = if local {
                runtime_now
                    .with_timezone(&Local)
                    .format_with_items(items.iter())
                    .write_to(&mut rendered)
            } else {
                runtime_now
                    .format_with_items(items.iter())
                    .write_to(&mut rendered)
            };
            result.map_err(|_| {
                CompileError::InvalidInput(format!("invalid date/time format in {name:?}"))
            })?;
            rendered
        }
    };
    Ok(value)
}

struct Variables<'a> {
    values: BTreeMap<String, &'a VariableValue>,
    /// [`BASE_URL_VARIABLES`] fall back to this when no scope defines them.
    base_url: Option<VariableValue>,
    secrets: &'a dyn SecretResolver,
    redactions: Vec<String>,
    runtime_now: DateTime<Utc>,
    runtime_values: HashMap<String, String>,
}

impl<'a> Variables<'a> {
    fn new(
        context: &CompileContext<'a>,
        collection: Option<&'a Collection>,
        folders: &[&'a Folder],
        request: &'a SavedRequest,
    ) -> Self {
        Self::from_scopes(context, collection, folders, request.variables.as_slice())
    }

    fn from_scopes(
        context: &CompileContext<'a>,
        collection: Option<&'a Collection>,
        folders: &[&'a Folder],
        request_variables: &'a [Variable],
    ) -> Self {
        let mut values = BTreeMap::new();
        let mut add_scope = |scope: &'a [Variable]| {
            for variable in scope.iter().filter(|variable| variable.enabled) {
                if !variable.key.trim().is_empty() {
                    values.insert(variable.key.clone(), &variable.value);
                }
            }
        };
        add_scope(context.global);
        if let Some(collection) = collection {
            add_scope(&collection.variables);
        }
        for folder in folders {
            add_scope(&folder.variables);
        }
        add_scope(context.environment);
        add_scope(context.data);
        add_scope(request_variables);
        add_scope(context.local);
        Self {
            values,
            base_url: context
                .environment_base_url
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(|url| VariableValue::Plain(url.to_string())),
            secrets: context.secrets,
            redactions: Vec::new(),
            runtime_now: Utc::now(),
            runtime_values: HashMap::new(),
        }
    }

    /// The base URL as templated text, when the environment has one.
    fn base_url(&mut self) -> Result<Option<String>, CompileError> {
        match self.base_url.clone() {
            Some(VariableValue::Plain(url)) => self.template(&url).map(Some),
            _ => Ok(None),
        }
    }

    fn template(&mut self, input: &str) -> Result<String, CompileError> {
        self.template_inner(input, &mut Vec::new())
    }

    /// Built-ins are fallback variables, evaluated once per expression per
    /// compilation. All date expressions share the same clock snapshot.
    fn runtime_value(&mut self, name: &str) -> Result<String, CompileError> {
        if let Some(value) = self.runtime_values.get(name) {
            return Ok(value.clone());
        }
        let value = runtime_variable_at(name, self.runtime_now)?;
        self.runtime_values.insert(name.to_string(), value.clone());
        Ok(value)
    }

    fn template_inner(
        &mut self,
        input: &str,
        stack: &mut Vec<String>,
    ) -> Result<String, CompileError> {
        let mut rendered = String::with_capacity(input.len());
        let mut remainder = input;
        while let Some(start) = remainder.find("{{") {
            rendered.push_str(&remainder[..start]);
            let after_open = &remainder[start + 2..];
            let Some(end) = after_open.find("}}") else {
                return Err(CompileError::InvalidInput(
                    "variable expression is missing `}}`".into(),
                ));
            };
            let name = after_open[..end].trim();
            if name.is_empty() {
                return Err(CompileError::InvalidInput(
                    "variable expression cannot be empty".into(),
                ));
            }
            if let Some(name) = name.strip_prefix("vault.") {
                let reference =
                    crate::vault::vault_secret_reference(name).map_err(CompileError::Secret)?;
                rendered.push_str(&self.direct_secret(&reference)?);
                remainder = &after_open[end + 2..];
                continue;
            }
            if let Some(position) = stack.iter().position(|entry| entry == name) {
                let mut cycle = stack[position..].to_vec();
                cycle.push(name.to_string());
                return Err(CompileError::VariableCycle(cycle));
            }
            let base_url = BASE_URL_VARIABLES
                .contains(&name)
                .then(|| self.base_url.clone())
                .flatten();
            let value = match (self.values.get(name).copied(), base_url) {
                (Some(value), _) => value.clone(),
                (None, Some(base)) => base,
                (None, None) => {
                    rendered.push_str(&self.runtime_value(name)?);
                    remainder = &after_open[end + 2..];
                    continue;
                }
            };
            stack.push(name.to_string());
            let resolved = match &value {
                VariableValue::Plain(value) => self.template_inner(value, stack)?,
                VariableValue::Secret(reference) => {
                    let value = self
                        .secrets
                        .resolve(reference)
                        .map_err(CompileError::Secret)?;
                    self.record_secret(&value);
                    value
                }
                VariableValue::MissingSecret(reference) => {
                    return Err(CompileError::Secret(format!(
                        "secret {} is unavailable",
                        reference.as_str()
                    )));
                }
            };
            stack.pop();
            rendered.push_str(&resolved);
            remainder = &after_open[end + 2..];
        }
        rendered.push_str(remainder);
        Ok(rendered)
    }

    fn direct_secret(&mut self, reference: &super::SecretRef) -> Result<String, CompileError> {
        let value = self
            .secrets
            .resolve(reference)
            .map_err(CompileError::Secret)?;
        self.record_secret(&value);
        Ok(value)
    }

    fn record_secret(&mut self, value: &str) {
        for variant in secret_redaction_variants(value) {
            if !self.redactions.contains(&variant) {
                self.redactions.push(variant);
            }
        }
    }
}

pub(crate) fn secret_redaction_variants(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut variants = vec![value.to_string()];

    // URL query pairs and application/x-www-form-urlencoded bodies transform
    // credentials before snapshots are built. Retain those exact wire variants
    // so redaction still works after encoding.
    let encoded_pair = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("", value)
        .finish();
    if let Some(encoded) = encoded_pair.strip_prefix('=') {
        variants.push(encoded.to_string());
        variants.push(encoded.replace('+', "%20"));
    }

    // JSON serialization can escape quotes and control characters in GraphQL
    // request bodies. The outer quotes are not part of the persisted body.
    if let Ok(encoded) = serde_json::to_string(value)
        && let Some(encoded) = encoded.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
    {
        variants.push(encoded.to_string());
    }

    variants.retain(|variant| !variant.is_empty());
    variants.sort();
    variants.dedup();
    variants
}

pub fn compile_request(
    request: &SavedRequest,
    collection: Option<&Collection>,
    context: &CompileContext<'_>,
) -> Result<(PreparedRequest, RedactedRequestSnapshot), CompileError> {
    compile_request_with_folder(request, collection, None, context)
}

/// Resolve authentication request headers using the same scopes as the API request.
pub fn compile_auth_headers(
    rows: &[KeyValueRow],
    request_variables: &[Variable],
    collection: Option<&Collection>,
    folders: &[&Folder],
    context: &CompileContext<'_>,
) -> Result<Vec<(String, String)>, CompileError> {
    let mut variables = Variables::from_scopes(context, collection, folders, request_variables);
    rows.iter()
        .filter(|row| row.enabled)
        .map(|row| {
            let name = row.key.trim();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err(CompileError::InvalidInput(format!(
                    "invalid header name {:?}",
                    row.key
                )));
            }
            Ok((name.to_string(), variables.template(&row.value)?))
        })
        .collect()
}

pub fn compile_request_with_folder(
    request: &SavedRequest,
    collection: Option<&Collection>,
    folder: Option<&Folder>,
    context: &CompileContext<'_>,
) -> Result<(PreparedRequest, RedactedRequestSnapshot), CompileError> {
    match folder {
        Some(folder) => compile_request_with_folder_chain(request, collection, &[folder], context),
        None => compile_request_with_folder_chain(request, collection, &[], context),
    }
}

pub fn compile_request_with_folder_chain(
    request: &SavedRequest,
    collection: Option<&Collection>,
    folders: &[&Folder],
    context: &CompileContext<'_>,
) -> Result<(PreparedRequest, RedactedRequestSnapshot), CompileError> {
    compile_request_with_folder_chain_for_purpose(
        request,
        collection,
        folders,
        context,
        CompilePurpose::Execute,
    )
}

/// Compiles a persistence-safe preview without requiring a managed OAuth or
/// login token to be fresh enough to send. Static validation, secret lookup,
/// and redaction are identical to executable compilation.
pub fn compile_redacted_request_with_folder_chain(
    request: &SavedRequest,
    collection: Option<&Collection>,
    folders: &[&Folder],
    context: &CompileContext<'_>,
) -> Result<RedactedRequestSnapshot, CompileError> {
    compile_request_with_folder_chain_for_purpose(
        request,
        collection,
        folders,
        context,
        CompilePurpose::RedactedPreview,
    )
    .map(|(_, snapshot)| snapshot)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CompilePurpose {
    Execute,
    RedactedPreview,
}

fn compile_request_with_folder_chain_for_purpose(
    request: &SavedRequest,
    collection: Option<&Collection>,
    folders: &[&Folder],
    context: &CompileContext<'_>,
    purpose: CompilePurpose,
) -> Result<(PreparedRequest, RedactedRequestSnapshot), CompileError> {
    if request.name.trim().is_empty() {
        return Err(CompileError::InvalidInput(
            "request name cannot be empty".into(),
        ));
    }
    if !folders.is_empty() {
        if collection.is_none_or(|collection| {
            folders
                .iter()
                .any(|folder| folder.collection_id != collection.id)
        }) || folders
            .first()
            .is_some_and(|folder| folder.parent_id.is_some())
            || folders
                .windows(2)
                .any(|pair| pair[1].parent_id.as_ref() != Some(&pair[0].id))
            || request.folder_id.as_ref() != folders.last().map(|folder| &folder.id)
        {
            return Err(CompileError::InvalidInput(
                "request, folder chain, and collection do not belong to one graph".into(),
            ));
        }
    } else if request.folder_id.is_some() {
        return Err(CompileError::InvalidInput(
            "nested request needs its complete folder chain".into(),
        ));
    }
    let mut variables = Variables::new(context, collection, folders, request);
    let rendered_url = variables.template(&request.url)?;
    let rendered_url = if is_relative_url(&rendered_url) {
        match variables.base_url()? {
            Some(base) => join_base_url(&base, &rendered_url),
            None => {
                return Err(CompileError::InvalidUrl(
                    "Relative URL needs a Base URL on the active environment.".into(),
                ));
            }
        }
    } else {
        rendered_url
    };
    let mut url = url::Url::parse(rendered_url.trim())
        .map_err(|error| CompileError::InvalidUrl(format!("invalid request URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CompileError::InvalidUrl(
            "request URL must use http or https".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CompileError::InvalidUrl(
            "request URL must not include user information".into(),
        ));
    }
    append_params(&mut url, &request.params, &mut variables)?;

    let mut headers = Vec::new();
    for row in request.headers.iter().filter(|row| row.enabled) {
        let name = row.key.trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(CompileError::InvalidInput(format!(
                "invalid header name {:?}",
                row.key
            )));
        }
        headers.push((name.to_string(), variables.template(&row.value)?));
    }

    let auth = effective_auth(request, folders, collection, context.environment_auth);
    let aws_sigv4 = apply_auth(auth, &mut url, &mut headers, &mut variables, purpose)?;
    let body = prepare_body(&request.body, &mut variables)?;
    if request.method.conventionally_has_no_body() && body != PreparedBody::None {
        return Err(CompileError::InvalidInput(format!(
            "{} requests cannot include a body",
            request.method.as_str()
        )));
    }

    let prepared = PreparedRequest {
        request_id: request.id.clone(),
        method: request.method.clone(),
        url: url.into(),
        headers,
        body,
        settings: request.settings.clone(),
        aws_sigv4,
        redactions: variables.redactions,
    };
    let mut snapshot = prepared.redacted_snapshot();
    snapshot.replay = Some(redacted_replay_snapshot(
        request,
        auth,
        &prepared.redactions,
    ));
    Ok((prepared, snapshot))
}

fn append_params(
    url: &mut url::Url,
    rows: &[KeyValueRow],
    variables: &mut Variables<'_>,
) -> Result<(), CompileError> {
    for row in rows.iter().filter(|row| row.enabled) {
        if row.key.trim().is_empty() {
            return Err(CompileError::InvalidInput(
                "enabled query parameter needs a name".into(),
            ));
        }
        let key = variables.template(&row.key)?;
        let value = variables.template(&row.value)?;
        url.query_pairs_mut().append_pair(&key, &value);
    }
    Ok(())
}

/// The auth a request actually sends with: the request's own, else the
/// nearest folder's, else the collection's, else the environment's. A request
/// set to `None` means exactly that; on folders and collections `None` falls
/// through like `Inherit` (there is no UI to set them, and a collection that
/// says nothing must not hide the environment's auth).
pub fn effective_auth<'a>(
    request: &'a SavedRequest,
    folders: &[&'a Folder],
    collection: Option<&'a Collection>,
    environment_auth: Option<&'a AuthConfig>,
) -> &'a AuthConfig {
    if !matches!(request.auth, AuthConfig::Inherit) {
        return &request.auth;
    }
    let inherits = |auth: &&AuthConfig| matches!(auth, AuthConfig::Inherit | AuthConfig::None);
    folders
        .iter()
        .rev()
        .map(|folder| &folder.auth)
        .chain(collection.map(|collection| &collection.auth))
        .chain(environment_auth)
        .find(|auth| !inherits(auth))
        .unwrap_or(&AuthConfig::None)
}

fn apply_auth(
    auth: &AuthConfig,
    url: &mut url::Url,
    headers: &mut Vec<(String, String)>,
    variables: &mut Variables<'_>,
    purpose: CompilePurpose,
) -> Result<Option<AwsSigV4Signing>, CompileError> {
    match auth {
        AuthConfig::Inherit | AuthConfig::None => Ok(None),
        AuthConfig::ApiKey {
            name,
            value,
            location,
        } => {
            let value = variables.direct_secret(value)?;
            match location {
                ApiKeyLocation::Header => headers.push((name.clone(), value)),
                ApiKeyLocation::Query => {
                    url.query_pairs_mut().append_pair(name, &value);
                }
            }
            Ok(None)
        }
        AuthConfig::Basic { username, password } => {
            let password = variables.direct_secret(password)?;
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{password}", variables.template(username)?));
            headers.push(("Authorization".into(), format!("Basic {encoded}")));
            variables.record_secret(&encoded);
            Ok(None)
        }
        AuthConfig::Bearer { token } | AuthConfig::OAuth2 { token } => {
            let token = variables.direct_secret(token)?;
            headers.push(("Authorization".into(), format!("Bearer {token}")));
            Ok(None)
        }
        AuthConfig::OAuth2AuthorizationCodePkce {
            access_token,
            expires_at,
            ..
        }
        | AuthConfig::OAuth2ClientCredentials {
            access_token,
            expires_at,
            ..
        }
        | AuthConfig::OAuth2Password {
            access_token,
            expires_at,
            ..
        }
        | AuthConfig::Login {
            access_token,
            expires_at,
            ..
        } => {
            let login = matches!(auth, AuthConfig::Login { .. });
            if purpose == CompilePurpose::Execute
                && expires_at.is_some_and(|expires_at| {
                    expires_at <= unix_timestamp().saturating_add(OAUTH_EXPIRY_SAFETY_SECS)
                })
            {
                return Err(CompileError::Unsupported(if login {
                    "sign-in session has expired; sign in again before sending".into()
                } else {
                    "OAuth 2 access token has expired; refresh the request before sending".into()
                }));
            }
            let token = access_token.as_ref().ok_or_else(|| {
                CompileError::Unsupported(if login {
                    "not signed in yet; sign in before sending".into()
                } else {
                    "OAuth 2 access token is unavailable; authorize or refresh the request first"
                        .into()
                })
            })?;
            let token = variables.direct_secret(token)?;
            headers.push(("Authorization".into(), format!("Bearer {token}")));
            Ok(None)
        }
        AuthConfig::AwsSigV4 {
            access_key,
            secret_key,
            session_token,
            region,
            service,
        } => {
            if region.trim().is_empty() || service.trim().is_empty() {
                return Err(CompileError::InvalidInput(
                    "AWS SigV4 requires a region and service".into(),
                ));
            }
            Ok(Some(AwsSigV4Signing {
                access_key: variables.direct_secret(access_key)?,
                secret_key: variables.direct_secret(secret_key)?,
                session_token: session_token
                    .as_ref()
                    .map(|token| variables.direct_secret(token))
                    .transpose()?,
                region: variables.template(region)?,
                service: variables.template(service)?,
            }))
        }
        AuthConfig::Unsupported { name, .. } => Err(CompileError::Unsupported(format!(
            "imported authentication {name:?} is not executable"
        ))),
    }
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

const OAUTH_EXPIRY_SAFETY_SECS: i64 = 30;

fn prepare_body(body: &Body, variables: &mut Variables<'_>) -> Result<PreparedBody, CompileError> {
    let prepared = match body {
        Body::None => PreparedBody::None,
        Body::Raw { media_type, text } => PreparedBody::Bytes {
            content_type: match media_type {
                super::RawBodyKind::Json => "application/json",
                super::RawBodyKind::Xml => "application/xml",
                super::RawBodyKind::Text => "text/plain; charset=utf-8",
            }
            .into(),
            bytes: variables.template(text)?.into_bytes(),
        },
        Body::UrlEncoded { rows } => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for row in rows.iter().filter(|row| row.enabled) {
                serializer.append_pair(
                    &variables.template(&row.key)?,
                    &variables.template(&row.value)?,
                );
            }
            PreparedBody::Bytes {
                content_type: "application/x-www-form-urlencoded".into(),
                bytes: serializer.finish().into_bytes(),
            }
        }
        Body::Multipart { rows } => PreparedBody::Multipart(
            rows.iter()
                .filter(|row| row.enabled)
                .map(|row| {
                    let value = match &row.value {
                        MultipartValue::Text(value) => {
                            PreparedMultipartValue::Text(variables.template(value)?)
                        }
                        MultipartValue::File(path) => {
                            PreparedMultipartValue::File(variables.template(path)?)
                        }
                    };
                    Ok(PreparedMultipartPart {
                        name: variables.template(&row.key)?,
                        value,
                    })
                })
                .collect::<Result<_, CompileError>>()?,
        ),
        Body::Binary { path } => PreparedBody::File(path.clone()),
        Body::GraphQl {
            query,
            variables: body_variables,
        } => {
            let query = variables.template(query)?;
            let rendered_variables = variables.template(body_variables)?;
            let variables_json: serde_json::Value = if rendered_variables.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&rendered_variables).map_err(|error| {
                    CompileError::InvalidInput(format!("invalid GraphQL variables JSON: {error}"))
                })?
            };
            PreparedBody::Bytes {
                content_type: "application/json".into(),
                // `Value`'s `Display` is its JSON text and cannot fail.
                bytes: serde_json::json!({
                    "query": query,
                    "variables": variables_json,
                })
                .to_string()
                .into_bytes(),
            }
        }
    };
    Ok(prepared)
}

impl PreparedRequest {
    pub fn redacted_snapshot(&self) -> RedactedRequestSnapshot {
        let redact = |input: &str| redact(input, &self.redactions);
        let body = request_body_text(&self.body, &redact);
        let mut snapshot = RedactedRequestSnapshot {
            method: self.method.as_str().into(),
            url: redact(&self.url),
            headers: self
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        redacted_header_value(name, value, &self.redactions),
                    )
                })
                .collect(),
            body,
            replay: None,
            body_bytes: prepared_body_bytes(&self.body),
            body_sensitive: prepared_body_contains_secret(&self.body, &self.redactions),
            body_binary: prepared_body_is_binary(&self.body),
            body_truncated: false,
            body_omitted_reason: None,
        };
        super::persistence_safety::sanitize_redacted_request_snapshot_or_redact(&mut snapshot);
        snapshot
    }
}

fn request_body_text(body: &PreparedBody, redact_value: &impl Fn(&str) -> String) -> String {
    match body {
        PreparedBody::None => String::new(),
        PreparedBody::Bytes { bytes, .. } => redact_value(&String::from_utf8_lossy(bytes)),
        PreparedBody::Multipart(rows) => rows
            .iter()
            .map(|part| match &part.value {
                PreparedMultipartValue::Text(value) => {
                    format!("{}={}", redact_value(&part.name), redact_value(value))
                }
                PreparedMultipartValue::File(_) => {
                    format!("{}=<file>", redact_value(&part.name))
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        // File grants are deliberately not represented by path/token text in
        // snippets or durable request snapshots.
        PreparedBody::File(_) => "<file>".into(),
    }
}

fn prepared_body_bytes(body: &PreparedBody) -> u64 {
    let bytes = match body {
        PreparedBody::None | PreparedBody::File(_) => 0,
        PreparedBody::Bytes { bytes, .. } => bytes.len(),
        PreparedBody::Multipart(parts) => parts
            .iter()
            .map(|part| {
                part.name.len()
                    + match &part.value {
                        PreparedMultipartValue::Text(value) => value.len(),
                        PreparedMultipartValue::File(_) => 0,
                    }
            })
            .sum(),
    };
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn prepared_body_contains_secret(body: &PreparedBody, redactions: &[String]) -> bool {
    let contains = |value: &str| {
        redactions
            .iter()
            .any(|secret| !secret.is_empty() && value.contains(secret))
    };
    match body {
        PreparedBody::None | PreparedBody::File(_) => false,
        PreparedBody::Bytes { bytes, .. } => contains(&String::from_utf8_lossy(bytes)),
        PreparedBody::Multipart(parts) => parts.iter().any(|part| {
            contains(&part.name)
                || matches!(&part.value, PreparedMultipartValue::Text(value) if contains(value))
        }),
    }
}

fn prepared_body_is_binary(body: &PreparedBody) -> bool {
    match body {
        PreparedBody::File(_) => true,
        PreparedBody::Multipart(parts) => parts
            .iter()
            .any(|part| matches!(part.value, PreparedMultipartValue::File(_))),
        PreparedBody::Bytes { content_type, .. } => is_binary_content_type(content_type),
        PreparedBody::None => false,
    }
}

fn is_binary_content_type(value: &str) -> bool {
    let media_type = value.split(';').next().unwrap_or(value).trim();
    media_type.starts_with("image/")
        || media_type.starts_with("audio/")
        || media_type.starts_with("video/")
        || media_type.starts_with("font/")
        || media_type.eq_ignore_ascii_case("application/octet-stream")
}

fn redacted_replay_snapshot(
    request: &SavedRequest,
    effective_auth: &AuthConfig,
    redactions: &[String],
) -> ReplayRequestSnapshot {
    let mut replay = ReplayRequestSnapshot {
        name: request.name.clone(),
        method: request.method.clone(),
        url: request.url.clone(),
        params: request.params.clone(),
        headers: request.headers.clone(),
        // Resolve inheritance at submission time. Replaying history must not
        // silently adopt collection/folder auth that changed afterward.
        auth: effective_auth.clone(),
        body: request.body.clone(),
        variables: request.variables.clone(),
        scripts: request.scripts.clone(),
        settings: request.settings.clone(),
    };
    scrub_replay_file_references(&mut replay.body);
    // Replay is itself durable. Structural credentials supplied as raw rows
    // must be removed even when they did not originate in the vault resolver.
    super::persistence_safety::sanitize_replay_snapshot_or_redact(&mut replay);
    let redactor = Redactor::new(redactions);
    redact_replay_fields(&mut replay, &redactor);
    replay
}

fn scrub_replay_file_references(body: &mut Body) {
    match body {
        Body::Multipart { rows } => {
            for row in rows {
                if let MultipartValue::File(reference) = &mut row.value {
                    *reference = "<file>".into();
                }
            }
        }
        Body::Binary { path } => *path = "<file>".into(),
        _ => {}
    }
}

/// Returns a persistence-safe copy of an exchange using the exact secret values
/// discovered while compiling its request.
pub fn redact_exchange(exchange: &Exchange, redactions: &[String]) -> Exchange {
    let redactions = expanded_redactions(redactions);
    let redactor = Redactor::new(&redactions);
    let mut safe = exchange.clone();
    redact_console(&mut safe.console, &redactor);
    for result in &mut safe.test_results {
        redact_test_result(result, &redactor);
    }
    redact_request_snapshot(&mut safe.request, &redactor);
    if let Some(response) = &mut safe.response {
        redact_response_snapshot(response, &redactor);
    }
    if let Some(error) = &mut safe.error {
        *error = redactor.redact(error);
    }
    safe
}

/// Returns a persistence-safe copy of an example using the exact secret values
/// discovered while compiling its request.
pub fn redact_example(example: &Example, redactions: &[String]) -> Example {
    let redactions = expanded_redactions(redactions);
    let redactor = Redactor::new(&redactions);
    let mut safe = example.clone();
    redact_example_fields(&mut safe, &redactor);
    safe
}

fn redact_example_fields(safe: &mut Example, redactor: &Redactor) {
    safe.name = redactor.redact(&safe.name);
    if let Some(request) = &mut safe.request {
        redact_request_snapshot(request, redactor);
    }
    redact_response_snapshot(&mut safe.response, redactor);
    redact_freeform_json_map_with(&mut safe.extensions, redactor);
}

/// Returns a persistence-safe runner result. Persisting a run is deliberately
/// routed through this boundary because response bodies and JavaScript test
/// output can echo credentials even when the request snapshot was redacted.
pub fn redact_collection_run(run: &CollectionRun, redactions: &[String]) -> CollectionRun {
    let redactions = expanded_redactions(redactions);
    let redactor = Redactor::new(&redactions);
    let mut safe = run.clone();
    for item in &mut safe.item_results {
        redact_console(&mut item.console, &redactor);
        if let Some(error) = &mut item.error {
            *error = redactor.redact(error);
        }
        if let Some(response) = &mut item.response {
            redact_response_snapshot(response, &redactor);
        }
        for result in &mut item.test_results {
            redact_test_result(result, &redactor);
        }
    }
    safe
}

fn expanded_redactions(redactions: &[String]) -> Vec<String> {
    let mut expanded = redactions
        .iter()
        .flat_map(|value| secret_redaction_variants(value))
        .collect::<Vec<_>>();
    expanded.sort();
    expanded.dedup();
    expanded
}

fn redact_request_snapshot(snapshot: &mut RedactedRequestSnapshot, redactor: &Redactor) {
    snapshot.url = redactor.redact(&snapshot.url);
    let redacted_body = redactor.redact(&snapshot.body);
    snapshot.body_sensitive |= redacted_body != snapshot.body;
    snapshot.body = redacted_body;
    redact_headers(&mut snapshot.headers, redactor);
    if let Some(replay) = &mut snapshot.replay {
        redact_replay_fields(replay, redactor);
    }
    super::persistence_safety::sanitize_redacted_request_snapshot_or_redact(snapshot);
}

fn redact_replay_fields(replay: &mut ReplayRequestSnapshot, redactor: &Redactor) {
    redact_string(&mut replay.name, redactor);
    redact_string(&mut replay.url, redactor);
    redact_rows(&mut replay.params, redactor);
    redact_rows(&mut replay.headers, redactor);
    redact_auth_fields(&mut replay.auth, redactor);
    redact_body_fields(&mut replay.body, redactor);
    redact_variables(&mut replay.variables, redactor);
    redact_scripts(&mut replay.scripts, redactor);
}

/// Redacts only user-controlled fields in a typed request. Structural values
/// such as IDs, HTTP methods, enum discriminants, and secret references must
/// remain untouched so even a one-character secret cannot corrupt the model.
pub(crate) fn redact_saved_request_fields(request: &mut SavedRequest, redactions: &[String]) {
    let redactor = Redactor::new(redactions);
    redact_saved_request_fields_with(request, &redactor);
}

fn redact_saved_request_fields_with(request: &mut SavedRequest, redactor: &Redactor) {
    redact_string(&mut request.name, redactor);
    redact_string(&mut request.url, redactor);
    redact_rows(&mut request.params, redactor);
    redact_rows(&mut request.headers, redactor);
    redact_auth_fields(&mut request.auth, redactor);
    redact_body_fields(&mut request.body, redactor);
    redact_variables(&mut request.variables, redactor);
    redact_scripts(&mut request.scripts, redactor);
    redact_freeform_json_map_with(&mut request.extensions, redactor);
}

fn redact_collection_fields_with(collection: &mut Collection, redactor: &Redactor) {
    redact_string(&mut collection.name, redactor);
    redact_string(&mut collection.description, redactor);
    redact_auth_fields(&mut collection.auth, redactor);
    redact_variables(&mut collection.variables, redactor);
    redact_scripts(&mut collection.scripts, redactor);
    redact_freeform_json_map_with(&mut collection.extensions, redactor);
}

fn redact_folder_fields_with(folder: &mut Folder, redactor: &Redactor) {
    redact_string(&mut folder.name, redactor);
    redact_auth_fields(&mut folder.auth, redactor);
    redact_variables(&mut folder.variables, redactor);
    redact_scripts(&mut folder.scripts, redactor);
    redact_freeform_json_map_with(&mut folder.extensions, redactor);
}

fn redact_environment_fields_with(environment: &mut Environment, redactor: &Redactor) {
    redact_string(&mut environment.name, redactor);
    redact_string(&mut environment.base_url, redactor);
    redact_auth_fields(&mut environment.auth, redactor);
    redact_variables(&mut environment.variables, redactor);
    redact_freeform_json_map_with(&mut environment.extensions, redactor);
}

pub(crate) fn redact_export_graph(
    collection: &mut Collection,
    folders: &mut [Folder],
    requests: &mut [SavedRequest],
    environments: &mut [Environment],
    examples: &mut [Example],
    redactions: &[String],
) {
    let redactor = Redactor::new(redactions);
    redact_collection_fields_with(collection, &redactor);
    for folder in folders {
        redact_folder_fields_with(folder, &redactor);
    }
    for request in requests {
        redact_saved_request_fields_with(request, &redactor);
    }
    for environment in environments {
        redact_environment_fields_with(environment, &redactor);
    }
    for example in examples {
        redact_example_fields(example, &redactor);
    }
}

fn redact_string(value: &mut String, redactor: &Redactor) {
    *value = redactor.redact(value);
}

fn redact_rows(rows: &mut [KeyValueRow], redactor: &Redactor) {
    for row in rows {
        redact_string(&mut row.key, redactor);
        redact_string(&mut row.value, redactor);
        redact_string(&mut row.description, redactor);
    }
}

fn redact_multipart_rows(rows: &mut [MultipartRow], redactor: &Redactor) {
    for row in rows {
        redact_string(&mut row.key, redactor);
        match &mut row.value {
            MultipartValue::Text(value) | MultipartValue::File(value) => {
                redact_string(value, redactor);
            }
        }
        redact_string(&mut row.description, redactor);
    }
}

fn redact_variables(variables: &mut [Variable], redactor: &Redactor) {
    for variable in variables {
        redact_string(&mut variable.key, redactor);
        if let VariableValue::Plain(value) = &mut variable.value {
            redact_string(value, redactor);
        }
        redact_string(&mut variable.description, redactor);
    }
}

fn redact_scripts(scripts: &mut Scripts, redactor: &Redactor) {
    redact_string(&mut scripts.pre_request, redactor);
    redact_string(&mut scripts.tests, redactor);
}

fn redact_body_fields(body: &mut Body, redactor: &Redactor) {
    match body {
        Body::None => {}
        Body::Raw { text, .. } => redact_string(text, redactor),
        Body::UrlEncoded { rows } => redact_rows(rows, redactor),
        Body::Multipart { rows } => redact_multipart_rows(rows, redactor),
        Body::Binary { path } => redact_string(path, redactor),
        Body::GraphQl { query, variables } => {
            redact_string(query, redactor);
            redact_string(variables, redactor);
        }
    }
}

fn redact_auth_fields(auth: &mut AuthConfig, redactor: &Redactor) {
    match auth {
        AuthConfig::Login { headers, .. }
        | AuthConfig::OAuth2AuthorizationCodePkce { headers, .. }
        | AuthConfig::OAuth2ClientCredentials { headers, .. }
        | AuthConfig::OAuth2Password { headers, .. } => redact_rows(headers, redactor),
        _ => {}
    }
    match auth {
        AuthConfig::Inherit
        | AuthConfig::None
        | AuthConfig::Bearer { .. }
        | AuthConfig::OAuth2 { .. } => {}
        AuthConfig::ApiKey { name, .. } => redact_string(name, redactor),
        AuthConfig::Basic { username, .. } => redact_string(username, redactor),
        AuthConfig::OAuth2AuthorizationCodePkce {
            authorization_endpoint,
            token_endpoint,
            client_id,
            scopes,
            redirect_uri,
            ..
        } => {
            redact_string(authorization_endpoint, redactor);
            redact_string(token_endpoint, redactor);
            redact_string(client_id, redactor);
            for scope in scopes {
                redact_string(scope, redactor);
            }
            redact_string(redirect_uri, redactor);
        }
        AuthConfig::OAuth2ClientCredentials {
            token_endpoint,
            client_id,
            scopes,
            ..
        } => {
            redact_string(token_endpoint, redactor);
            redact_string(client_id, redactor);
            for scope in scopes {
                redact_string(scope, redactor);
            }
        }
        AuthConfig::OAuth2Password {
            token_endpoint,
            client_id,
            username,
            scopes,
            ..
        } => {
            redact_string(token_endpoint, redactor);
            redact_string(client_id, redactor);
            redact_string(username, redactor);
            for scope in scopes {
                redact_string(scope, redactor);
            }
        }
        AuthConfig::AwsSigV4 {
            region, service, ..
        } => {
            redact_string(region, redactor);
            redact_string(service, redactor);
        }
        AuthConfig::Login {
            url,
            method,
            body,
            token_path,
            basic,
            ..
        } => {
            redact_string(url, redactor);
            redact_string(method, redactor);
            redact_string(body, redactor);
            redact_string(token_path, redactor);
            if let Some(basic) = basic {
                redact_string(&mut basic.username, redactor);
            }
        }
        AuthConfig::Unsupported { name, raw } => {
            redact_string(name, redactor);
            redact_freeform_json_value_with(raw, redactor);
        }
    }
}

fn redact_console(entries: &mut Vec<super::ConsoleEntry>, redactor: &Redactor) {
    entries.truncate(202);
    for entry in entries {
        entry.phase = redactor.redact(&entry.phase);
        entry.level = redactor.redact(&entry.level);
        entry.message = redactor.redact(&entry.message);
        for text in [&mut entry.phase, &mut entry.level, &mut entry.message] {
            if text.len() > 4096 {
                let mut end = 4096;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("… [truncated]");
            }
        }
    }
}

fn redact_response_snapshot(response: &mut ResponseSnapshot, redactor: &Redactor) {
    redact_console(&mut response.console, redactor);
    response.reason = redactor.redact(&response.reason);
    response.final_url = redactor.redact(&response.final_url);
    redact_headers(&mut response.headers, redactor);
    for redirect in &mut response.redirects {
        redirect.from_url = redactor.redact(&redirect.from_url);
        redirect.to_url = redactor.redact(&redirect.to_url);
    }
    for cookie in &mut response.cookies {
        cookie.name = redactor.redact(&cookie.name);
        cookie.domain = redactor.redact(&cookie.domain);
        cookie.path = redactor.redact(&cookie.path);
    }
    for result in &mut response.test_results {
        redact_test_result(result, redactor);
    }
    response.body_base64 = match base64::engine::general_purpose::STANDARD
        .decode(&response.body_base64)
    {
        Ok(bytes) => {
            base64::engine::general_purpose::STANDARD.encode(redactor.redact_bytes(&bytes))
        }
        Err(_) => base64::engine::general_purpose::STANDARD.encode("<redacted: invalid base64>"),
    };
}

fn redact_test_result(result: &mut TestResult, redactor: &Redactor) {
    result.name = redactor.redact(&result.name);
    if let Some(error) = &mut result.error {
        *error = redactor.redact(error);
    }
}

fn redact_headers(headers: &mut [(String, String)], redactor: &Redactor) {
    for (name, value) in headers {
        *value = redacted_header_value_with(name, value, redactor);
    }
}

/// A header value as history and the trace may show it: credentials are
/// replaced wholesale, except that a bearer JWT leaves behind the summary of
/// its public claims so two tokens can be told apart without revealing either.
fn redacted_header_value(name: &str, value: &str, redactions: &[String]) -> String {
    redacted_header_value_with(name, value, &Redactor::new(redactions))
}

fn redacted_header_value_with(name: &str, value: &str, redactor: &Redactor) -> String {
    if !sensitive_header(name) {
        return redactor.redact(value);
    }
    // Snapshots are redacted again before persisting; keep the summary the
    // first pass produced instead of collapsing it to the bare marker.
    if is_redacted_bearer_summary(value) {
        return value.into();
    }
    if name.eq_ignore_ascii_case("authorization")
        && let Some(claims) = value
            .strip_prefix("Bearer ")
            .and_then(|token| jwt_claims_summary(token.trim()))
    {
        return format!("Bearer <redacted> · {claims}");
    }
    "<redacted>".into()
}

const JWT_SUMMARY_CLAIMS: [&str; 8] = [
    "iss",
    "aud",
    "client_id",
    "scope",
    "sub",
    "idp",
    "iat",
    "exp",
];

/// Whether a header value is the summary `redacted_header_value` leaves in
/// place of a bearer JWT — and therefore already safe to persist. Anything
/// after the marker must be whitelisted `claim=value` pairs, never a token.
pub(super) fn is_redacted_bearer_summary(value: &str) -> bool {
    let Some(claims) = value.strip_prefix("Bearer <redacted> · ") else {
        return false;
    };
    !claims.is_empty()
        && claims.len() <= 512
        && claims.split(' ').all(|piece| {
            piece.len() <= 200
                && piece
                    .split_once('=')
                    .is_none_or(|(key, _)| JWT_SUMMARY_CLAIMS.contains(&key))
        })
}

/// The claims that identify what a JWT is for — issuer, audience, client,
/// scope, subject and lifetime — as `key=value` pairs. Nothing else from the
/// payload is copied and the signature is never checked: this is a label,
/// not a verification.
pub fn jwt_claims_summary(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&bytes).ok()?;
    let render = |value: &serde_json::Value| match value {
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| match item {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let summary = JWT_SUMMARY_CLAIMS
        .iter()
        .filter_map(|key| {
            claims
                .get(*key)
                .map(|value| format!("{key}={}", render(value)))
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!summary.is_empty()).then_some(summary)
}

/// Redacts a schemaless JSON map, including its keys. Redacted keys can
/// collide, so entries are sorted first and assigned stable numeric suffixes
/// rather than silently discarding a value.
pub(crate) fn redact_freeform_json_map(
    map: &mut serde_json::Map<String, serde_json::Value>,
    redactions: &[String],
) {
    redact_freeform_json_map_with(map, &Redactor::new(redactions));
}

fn redact_freeform_json_map_with(
    map: &mut serde_json::Map<String, serde_json::Value>,
    redactor: &Redactor,
) {
    let mut entries = std::mem::take(map).into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut key_allocator = RedactedJsonKeyAllocator::default();
    for (key, mut value) in entries {
        redact_freeform_json_value_with(&mut value, redactor);
        let redacted_key = redactor.redact(&key);
        let unique_key = key_allocator.allocate(map, redacted_key);
        map.insert(unique_key, value);
    }
}

#[derive(Default)]
struct RedactedJsonKeyAllocator {
    next_suffixes: HashMap<String, u64>,
    #[cfg(test)]
    existence_checks: usize,
}

impl RedactedJsonKeyAllocator {
    fn allocate(
        &mut self,
        map: &serde_json::Map<String, serde_json::Value>,
        key: String,
    ) -> String {
        #[cfg(test)]
        {
            self.existence_checks += 1;
        }
        if !map.contains_key(&key) {
            return key;
        }

        let mut ordinal = self.next_suffixes.get(&key).copied().unwrap_or(2);
        loop {
            let candidate = format!("{key}#{ordinal}");
            #[cfg(test)]
            {
                self.existence_checks += 1;
            }
            if !map.contains_key(&candidate) {
                self.next_suffixes.insert(key, ordinal.saturating_add(1));
                return candidate;
            }
            // A finite map cannot use every suffix, so this never saturates in practice.
            ordinal = ordinal.saturating_add(1);
        }
    }
}

fn redact_freeform_json_value_with(value: &mut serde_json::Value, redactor: &Redactor) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                redact_freeform_json_value_with(value, redactor);
            }
        }
        serde_json::Value::Object(map) => redact_freeform_json_map_with(map, redactor),
        _ => redact_json_value_with(value, redactor),
    }
}

/// Redacts values in typed JSON without changing structural field names.
fn redact_json_value_with(value: &mut serde_json::Value, redactor: &Redactor) {
    match value {
        serde_json::Value::String(text) => *text = redactor.redact(text),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value_with(value, redactor);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                redact_json_value_with(value, redactor);
            }
        }
        _ => {}
    }
}

pub(super) struct Redactor {
    matcher: Option<AhoCorasick>,
    /// These patterns must run before existing markers are protected because
    /// the marker itself is part of the secret to replace.
    marker_spanning_matcher: Option<AhoCorasick>,
}

impl Redactor {
    pub(super) fn new(redactions: &[String]) -> Self {
        let patterns = redactions
            .iter()
            .filter(|value| !value.is_empty())
            .map(|value| normalize_percent_escapes(value.as_bytes()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let build_matcher = |patterns: Vec<Vec<u8>>| {
            // Literal patterns always build; a failure leaves this matcher out.
            (!patterns.is_empty())
                .then(|| {
                    AhoCorasickBuilder::new()
                        .match_kind(MatchKind::LeftmostLongest)
                        .build(patterns)
                        .ok()
                })
                .flatten()
        };
        let marker_spanning_patterns = patterns
            .iter()
            .filter(|pattern| find_bytes(pattern, b"<redacted>").is_some())
            .cloned()
            .collect();
        let marker_spanning_matcher = build_matcher(marker_spanning_patterns);
        let matcher = build_matcher(patterns);
        Self {
            matcher,
            marker_spanning_matcher,
        }
    }

    fn redact_bytes(&self, input: &[u8]) -> Vec<u8> {
        const REDACTED: &[u8] = b"<redacted>";
        let Some(matcher) = &self.matcher else {
            return input.to_vec();
        };
        let spanning_redacted = self.marker_spanning_matcher.as_ref().map(|matcher| {
            let mut output = Vec::with_capacity(input.len());
            self.redact_matches(input, matcher, &mut output);
            output
        });
        let input = spanning_redacted.as_deref().unwrap_or(input);
        let mut output = Vec::with_capacity(input.len());
        let mut cursor = 0;
        while let Some(marker_offset) = find_bytes(&input[cursor..], REDACTED) {
            let marker_start = cursor + marker_offset;
            self.redact_matches(&input[cursor..marker_start], matcher, &mut output);
            output.extend_from_slice(REDACTED);
            cursor = marker_start + REDACTED.len();
        }
        self.redact_matches(&input[cursor..], matcher, &mut output);
        output
    }

    fn redact_matches(&self, input: &[u8], matcher: &AhoCorasick, output: &mut Vec<u8>) {
        const REDACTED: &[u8] = b"<redacted>";
        let normalized = normalize_percent_escapes(input);
        let mut cursor = 0;
        for matched in matcher.find_iter(&normalized) {
            output.extend_from_slice(&input[cursor..matched.start()]);
            output.extend_from_slice(REDACTED);
            cursor = matched.end();
        }
        output.extend_from_slice(&input[cursor..]);
    }

    pub(super) fn redact(&self, input: &str) -> String {
        // Replacing complete UTF-8 patterns keeps the text valid UTF-8.
        String::from_utf8_lossy(&self.redact_bytes(input.as_bytes())).into_owned()
    }
}

fn normalize_percent_escapes(input: &[u8]) -> Vec<u8> {
    let mut normalized = input.to_vec();
    let mut cursor = 0;
    while cursor + 2 < normalized.len() {
        if normalized[cursor] == b'%'
            && normalized[cursor + 1].is_ascii_hexdigit()
            && normalized[cursor + 2].is_ascii_hexdigit()
        {
            normalized[cursor + 1] = normalized[cursor + 1].to_ascii_uppercase();
            normalized[cursor + 2] = normalized[cursor + 2].to_ascii_uppercase();
            cursor += 3;
        } else {
            cursor += 1;
        }
    }
    normalized
}

fn find_bytes(input: &[u8], needle: &[u8]) -> Option<usize> {
    input
        .windows(needle.len())
        .position(|candidate| candidate == needle)
}

fn redact_bytes(input: &[u8], redactions: &[String]) -> Vec<u8> {
    Redactor::new(redactions).redact_bytes(input)
}

pub(crate) fn redact_text(input: &str, redactions: &[String]) -> String {
    String::from_utf8_lossy(&redact_bytes(input.as_bytes(), redactions)).into_owned()
}

fn redact(input: &str, redactions: &[String]) -> String {
    redact_text(input, redactions)
}

fn sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "proxy-authorization" | "set-cookie"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CollectionId, ExampleId, FolderId, HttpMethod, RequestId, RequestSettings, RowId,
        SecretRef, WorkspaceId,
    };
    use std::collections::HashMap;

    struct Secrets(HashMap<String, String>);

    impl SecretResolver for Secrets {
        fn resolve(&self, reference: &SecretRef) -> Result<String, String> {
            self.0
                .get(reference.as_str())
                .cloned()
                .ok_or_else(|| "missing secret".into())
        }
    }

    fn variable(key: &str, value: VariableValue) -> Variable {
        Variable {
            id: RowId::new(),
            key: key.into(),
            value,
            enabled: true,
            description: String::new(),
        }
    }

    fn request() -> SavedRequest {
        SavedRequest {
            id: RequestId::new(),
            collection_id: CollectionId::new(),
            folder_id: None,
            name: "Get item".into(),
            method: HttpMethod::new("POST").unwrap(),
            url: "https://{{host}}/items".into(),
            params: vec![KeyValueRow::enabled("tag", "{{tag}}")],
            headers: vec![KeyValueRow::enabled("X-Token", "{{token}}")],
            auth: AuthConfig::None,
            body: Body::Raw {
                media_type: super::super::RawBodyKind::Json,
                text: r#"{"token":"{{token}}"}"#.into(),
            },
            variables: vec![variable("tag", VariableValue::Plain("request".into()))],
            scripts: Default::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        }
    }

    #[test]
    fn named_vault_values_are_shared_while_environment_secrets_stay_project_scoped() {
        use crate::secrets::{
            MemorySecretStore, SecretStore, SecretValue, WorkspaceSecretResolver,
        };

        let store = MemorySecretStore::new();
        let alpha = WorkspaceId::new("project-alpha").unwrap();
        let beta = WorkspaceId::new("project-beta").unwrap();
        let shared = crate::vault::vault_secret_reference("api_token").unwrap();
        let local = SecretRef::new("environment.local-token").unwrap();
        store
            .set_secret(&alpha, &shared, SecretValue::new("shared-first"))
            .unwrap();
        store
            .set_secret(&alpha, &local, SecretValue::new("alpha-only"))
            .unwrap();
        store
            .set_secret(&beta, &local, SecretValue::new("beta-only"))
            .unwrap();
        let environment = [variable("local_token", VariableValue::Secret(local))];
        let mut definition = request();
        definition.url = "https://example.test/items".into();
        definition.params.clear();
        definition.body = Body::None;
        definition.headers = vec![
            KeyValueRow::enabled("X-Shared", "{{vault.api_token}}"),
            KeyValueRow::enabled("X-Local", "{{local_token}}"),
        ];
        let check = |scope: &WorkspaceId, shared_value: &str, local_value: &str| {
            let resolver = WorkspaceSecretResolver::new(scope, &store);
            let (prepared, snapshot) = compile_request(
                &definition,
                None,
                &CompileContext {
                    global: &[],
                    environment: &environment,
                    data: &[],
                    local: &[],
                    secrets: &resolver,
                    environment_base_url: None,
                    environment_auth: None,
                },
            )
            .unwrap();
            assert!(
                prepared
                    .headers
                    .contains(&("X-Shared".into(), shared_value.into()))
            );
            assert!(
                prepared
                    .headers
                    .contains(&("X-Local".into(), local_value.into()))
            );
            let saved = serde_json::to_string(&snapshot).unwrap();
            assert!(!saved.contains(shared_value));
            assert!(!saved.contains(local_value));
        };
        check(&alpha, "shared-first", "alpha-only");
        check(&beta, "shared-first", "beta-only");
        store
            .set_secret(&beta, &shared, SecretValue::new("shared-updated"))
            .unwrap();
        check(&alpha, "shared-updated", "alpha-only");
        check(&beta, "shared-updated", "beta-only");
    }

    #[test]
    fn named_vault_templates_survive_save_resolve_and_redact_every_request_surface() {
        let reference = crate::vault::vault_secret_reference("api_token").unwrap();
        let secrets = Secrets(HashMap::from([(
            reference.as_str().into(),
            "vault-only-value".into(),
        )]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut definition = request();
        definition.url =
            "https://example.test/{{vault.api_token}}?api_key={{vault.api_token}}".into();
        definition.params = vec![KeyValueRow::enabled("token", "{{vault.api_token}}")];
        definition.headers = vec![KeyValueRow::enabled(
            "Authorization",
            "Bearer {{vault.api_token}}",
        )];
        definition.variables = vec![variable(
            "vault.api_token",
            VariableValue::Plain("cannot-shadow-vault".into()),
        )];
        definition.body = Body::Raw {
            media_type: super::super::RawBodyKind::Json,
            text: r#"{"password":"{{vault.api_token}}"}"#.into(),
        };
        let saved = crate::persistence_safe_saved_request(&definition).unwrap();
        assert_eq!(saved.url, definition.url);
        assert_eq!(saved.headers, definition.headers);
        assert_eq!(saved.params, definition.params);
        let (prepared, snapshot) = compile_request(&saved, None, &context).unwrap();
        assert!(
            prepared
                .url
                .contains("/vault-only-value?api_key=vault-only-value")
        );
        assert!(
            prepared
                .headers
                .contains(&("Authorization".into(), "Bearer vault-only-value".into()))
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("vault-only-value")
        );
        assert!(!prepared.url.contains("cannot-shadow-vault"));
        definition.headers.clear();
        definition.auth = AuthConfig::Basic {
            username: "{{vault.api_token}}".into(),
            password: reference,
        };
        let (prepared, snapshot) = compile_request(&definition, None, &context).unwrap();
        let encoded =
            base64::engine::general_purpose::STANDARD.encode("vault-only-value:vault-only-value");
        assert!(
            prepared
                .headers
                .contains(&("Authorization".into(), format!("Basic {encoded}")))
        );
        assert!(!serde_json::to_string(&snapshot).unwrap().contains(&encoded));
        definition.url = "https://example.test/{{vault.missing}}".into();
        assert!(matches!(
            compile_request(&definition, None, &context),
            Err(CompileError::Secret(_))
        ));
    }

    #[test]
    fn runtime_dates_share_one_snapshot_and_validate_formats() {
        let secrets = Secrets(HashMap::new());
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut variables = Variables::from_scopes(&context, None, &[], &[]);
        variables.runtime_now = DateTime::parse_from_rfc3339("2026-09-07T13:14:15.123Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            variables
                .template("{{$date}} {{$time}} {{$isoTimestamp}}")
                .unwrap(),
            "2026-09-07 13:14:15 2026-09-07T13:14:15.123Z"
        );
        assert_eq!(
            variables
                .template("{{$timestamp}} {{$timestampMs}}")
                .unwrap(),
            format!(
                "{} {}",
                variables.runtime_now.timestamp(),
                variables.runtime_now.timestamp_millis()
            )
        );
        assert_eq!(
            variables
                .template("{{$datetime:%Y-%m-%dT00:00}} / {{$datetime:%Y-%m-%dT23:59}}")
                .unwrap(),
            "2026-09-07T00:00 / 2026-09-07T23:59"
        );
        assert_eq!(
            variables
                .template("{{$localDatetime:%Y-%m-%dT%H:%M:%S%:z}}")
                .unwrap(),
            variables
                .runtime_now
                .with_timezone(&Local)
                .format("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        );
        for expression in [
            "{{$datetime:%Q}}",
            "{{$datetime:%}}",
            "{{$datetime:}}",
            "{{$localDatetime:%Q}}",
        ] {
            assert!(
                matches!(
                    variables.template(expression),
                    Err(CompileError::InvalidInput(_))
                ),
                "{expression}"
            );
        }
        assert_eq!(
            variables.template("{{$unknown}}"),
            Err(CompileError::UnresolvedVariable("$unknown".into()))
        );
    }

    #[test]
    fn runtime_variables_resolve_aliases_across_request_fields_and_refresh_per_compile() {
        let secrets = Secrets(HashMap::new());
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/{{$uuid}}".into();
        request.params = vec![KeyValueRow::enabled("id", "{{Id}}")];
        request.headers = vec![KeyValueRow::enabled("X-Id", "{{Id}}")];
        request.variables = vec![
            variable("Id", VariableValue::Plain("{{$uuid}}".into())),
            variable(
                "DateFrom",
                VariableValue::Plain("{{$datetime:%Y-%m-%dT00:00}}".into()),
            ),
            variable("$date", VariableValue::Plain("overridden".into())),
        ];
        request.body = Body::Raw {
            media_type: super::super::RawBodyKind::Json,
            text: r#"{"id":"{{Id}}","date":"{{DateFrom}}","shadow":"{{$date}}","guid":"{{$guid}}","random":{{$randomInt}},"randomAgain":{{$randomInt}}}"#.into(),
        };
        let (prepared, _) = compile_request(&request, None, &context).unwrap();
        let PreparedBody::Bytes { bytes, .. } = &prepared.body else {
            panic!("expected bytes")
        };
        let payload: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        let id = payload["id"].as_str().unwrap();
        assert_eq!(uuid::Uuid::parse_str(id).unwrap().get_version_num(), 4);
        assert_eq!(prepared.url, format!("https://example.test/{id}?id={id}"));
        assert!(prepared.headers.contains(&("X-Id".into(), id.into())));
        assert!(payload["date"].as_str().unwrap().ends_with("T00:00"));
        assert_eq!(payload["shadow"], "overridden");
        assert_eq!(
            uuid::Uuid::parse_str(payload["guid"].as_str().unwrap())
                .unwrap()
                .get_version_num(),
            4
        );
        assert!(payload["random"].as_u64().unwrap() <= 1000);
        assert_eq!(payload["random"], payload["randomAgain"]);
        let (next, _) = compile_request(&request, None, &context).unwrap();
        assert_ne!(prepared.url, next.url);
    }

    #[test]
    fn compiles_precedence_duplicates_and_redacts_secrets() {
        let secret_ref = SecretRef::new("vault-token").unwrap();
        let secrets = Secrets(HashMap::from([("vault-token".into(), "shh-123".into())]));
        let global = vec![
            variable("host", VariableValue::Plain("api.example.test".into())),
            variable("tag", VariableValue::Plain("global".into())),
            variable("token", VariableValue::Secret(secret_ref)),
        ];
        let environment = vec![variable("tag", VariableValue::Plain("environment".into()))];
        let context = CompileContext {
            global: &global,
            environment: &environment,
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let (prepared, snapshot) = compile_request(&request(), None, &context).unwrap();
        assert_eq!(prepared.url, "https://api.example.test/items?tag=request");
        assert_eq!(prepared.headers, vec![("X-Token".into(), "shh-123".into())]);
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("shh-123")
        );
        assert!(!format!("{prepared:?}").contains("shh-123"));
    }

    #[test]
    fn a_cached_user_token_from_the_password_grant_is_sent_as_a_bearer() {
        let token_ref = SecretRef::new("cached-access-token-ref").unwrap();
        let secrets = Secrets(HashMap::from([(
            token_ref.as_str().to_string(),
            "user-token".into(),
        )]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;
        request.auth = AuthConfig::OAuth2Password {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "native-client".into(),
            client_secret: None,
            username: "jane".into(),
            password: SecretRef::new("oauth-user-secret").unwrap(),
            scopes: vec!["web_api".into()],
            access_token: Some(token_ref),
            refresh_token: None,
            expires_at: Some(unix_timestamp() + 3_600),
        };

        let (prepared, snapshot) = compile_request(&request, None, &context).unwrap();
        assert_eq!(
            prepared.headers,
            vec![("Authorization".into(), "Bearer user-token".into())]
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("user-token")
        );
    }

    #[test]
    fn api_key_headers_and_cached_pkce_tokens_attach_their_credential() {
        let key_ref = SecretRef::new("api-key").unwrap();
        let pkce_ref = SecretRef::new("pkce-access").unwrap();
        let secrets = Secrets(HashMap::from([
            (key_ref.as_str().to_string(), "k-123".into()),
            (pkce_ref.as_str().to_string(), "pkce-token".into()),
        ]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;

        request.auth = AuthConfig::ApiKey {
            name: "X-Api-Key".into(),
            value: key_ref,
            location: ApiKeyLocation::Header,
        };
        let (prepared, snapshot) = compile_request(&request, None, &context).unwrap();
        assert_eq!(prepared.headers, vec![("X-Api-Key".into(), "k-123".into())]);
        assert_eq!(
            snapshot.headers,
            vec![("X-Api-Key".into(), "<redacted>".into())]
        );

        request.auth = AuthConfig::OAuth2AuthorizationCodePkce {
            headers: Vec::new(),
            authorization_endpoint: "https://identity.example.test/authorize".into(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "native".into(),
            scopes: vec!["openid".into(), "web_api".into()],
            redirect_uri: "http://127.0.0.1:18765/callback".into(),
            access_token: Some(pkce_ref),
            refresh_token: None,
            expires_at: Some(unix_timestamp() + 3_600),
        };
        let (prepared, snapshot) = compile_request(&request, None, &context).unwrap();
        assert_eq!(
            prepared.headers,
            vec![("Authorization".into(), "Bearer pkce-token".into())]
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("pkce-token")
        );
    }

    #[test]
    fn a_bearer_jwt_is_redacted_down_to_its_identifying_claims() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iss": "https://identity.example.test",
                "aud": ["web_api", "roadstar"],
                "client_id": "automation",
                "scope": ["web_api"],
                "sub": "42",
                "exp": 1_800_000_000u64,
                "secret_claim": "never-shown"
            })
            .to_string(),
        );
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.c2ln");
        let shown = redacted_header_value("Authorization", &format!("Bearer {token}"), &[]);
        assert_eq!(
            shown,
            "Bearer <redacted> · iss=https://identity.example.test aud=web_api roadstar client_id=automation scope=web_api sub=42 exp=1800000000"
        );
        assert!(!shown.contains(&payload));
        assert_eq!(
            redacted_header_value("Authorization", "Bearer opaque-token", &[]),
            "<redacted>"
        );
        assert_eq!(
            redacted_header_value("Authorization", "Basic dXNlcjpwYXNz", &[]),
            "<redacted>"
        );
        assert_eq!(redacted_header_value("Cookie", "a=b", &[]), "<redacted>");

        // The summary survives the persistence sanitizer that runs on every
        // snapshot, so the Trace and history show it; the raw token does not.
        let token_ref = SecretRef::new("oauth-jwt").unwrap();
        let secrets = Secrets(HashMap::from([(
            token_ref.as_str().to_string(),
            token.clone(),
        )]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;
        request.auth = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "automation".into(),
            client_secret: SecretRef::new("oauth-client-secret").unwrap(),
            scopes: vec!["web_api".into()],
            access_token: Some(token_ref),
            expires_at: Some(unix_timestamp() + 3_600),
        };
        let (_, snapshot) = compile_request(&request, None, &context).unwrap();
        assert_eq!(
            snapshot.headers,
            vec![("Authorization".into(), shown.clone())]
        );
        // The second redaction pass every exchange gets before it is shown or
        // stored is idempotent on the summary.
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: crate::ExchangeId::new(),
            workspace_id: WorkspaceId::new("project").unwrap(),
            request_id: Some(request.id.clone()),
            request: snapshot.clone(),
            response: None,
            error: None,
            started_at: 0,
            completed_at: 0,
        };
        let safe = redact_exchange(&exchange, std::slice::from_ref(&token));
        assert_eq!(safe.request.headers, vec![("Authorization".into(), shown)]);
        assert!(!serde_json::to_string(&snapshot).unwrap().contains(&payload));
        assert!(is_redacted_bearer_summary(&snapshot.headers[0].1));
        assert!(!is_redacted_bearer_summary(&format!(
            "Bearer <redacted> · {token}"
        )));
        assert!(!is_redacted_bearer_summary("Bearer <redacted> · "));
    }

    #[test]
    fn rejects_expired_typed_oauth_access_tokens_before_sending() {
        let token_ref = SecretRef::new("oauth-access-token").unwrap();
        let secrets = Secrets(HashMap::from([(
            token_ref.as_str().to_string(),
            "expired-token".into(),
        )]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;
        request.auth = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "native-client".into(),
            client_secret: SecretRef::new("oauth-client-secret").unwrap(),
            scopes: vec![],
            access_token: Some(token_ref),
            expires_at: Some(1),
        };

        assert_eq!(
            compile_request(&request, None, &context).unwrap_err(),
            CompileError::Unsupported(
                "OAuth 2 access token has expired; refresh the request before sending".into()
            )
        );
    }

    #[test]
    fn redacted_preview_accepts_expired_inherited_oauth_without_exposing_its_token() {
        let token_ref = SecretRef::new("oauth-access-token").unwrap();
        let secrets = Secrets(HashMap::from([(
            token_ref.as_str().to_string(),
            "expired-token".into(),
        )]));
        let environment_auth = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "native-client".into(),
            client_secret: SecretRef::new("oauth-client-secret").unwrap(),
            scopes: vec![],
            access_token: Some(token_ref),
            expires_at: Some(1),
        };
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: Some(&environment_auth),
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;
        request.auth = AuthConfig::Inherit;

        assert_eq!(
            compile_request(&request, None, &context).unwrap_err(),
            CompileError::Unsupported(
                "OAuth 2 access token has expired; refresh the request before sending".into()
            )
        );

        let snapshot =
            compile_redacted_request_with_folder_chain(&request, None, &[], &context).unwrap();
        assert_eq!(
            snapshot.headers,
            vec![("Authorization".into(), "<redacted>".into())]
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("expired-token")
        );
    }

    #[test]
    fn redacted_preview_accepts_expired_login_but_still_requires_a_cached_token() {
        let token_ref = SecretRef::new("login-access-token").unwrap();
        let secrets = Secrets(HashMap::from([(
            token_ref.as_str().to_string(),
            "expired-session".into(),
        )]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let login = |access_token| AuthConfig::Login {
            url: "https://api.example.test/login".into(),
            basic: None,
            headers: Vec::new(),
            method: "POST".into(),
            body: r#"{"user":"{{login_user}}"}"#.into(),
            token_path: "data.session".into(),
            ttl_secs: None,
            access_token,
            expires_at: Some(1),
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;
        request.auth = login(Some(token_ref));

        let snapshot =
            compile_redacted_request_with_folder_chain(&request, None, &[], &context).unwrap();
        assert_eq!(
            snapshot.headers,
            vec![("Authorization".into(), "<redacted>".into())]
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("expired-session")
        );

        request.auth = login(None);
        assert_eq!(
            compile_redacted_request_with_folder_chain(&request, None, &[], &context).unwrap_err(),
            CompileError::Unsupported("not signed in yet; sign in before sending".into())
        );
    }

    #[test]
    fn redacts_encoded_secret_variants_from_query_and_form_snapshots() {
        let secret_ref = SecretRef::new("vault-token").unwrap();
        let secret = "p@ss/word+?";
        let secrets = Secrets(HashMap::from([("vault-token".into(), secret.into())]));
        let global = vec![variable("token", VariableValue::Secret(secret_ref.clone()))];
        let context = CompileContext {
            global: &global,
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params = vec![KeyValueRow::enabled("query", "{{token}}")];
        request.auth = AuthConfig::ApiKey {
            name: "api_key".into(),
            value: secret_ref,
            location: ApiKeyLocation::Query,
        };
        request.body = Body::UrlEncoded {
            rows: vec![KeyValueRow::enabled("form", "{{token}}")],
        };

        let (prepared, snapshot) = compile_request(&request, None, &context).unwrap();
        let encoded = "p%40ss%2Fword%2B%3F";
        assert!(prepared.url.contains(encoded));
        assert!(matches!(
            &prepared.body,
            PreparedBody::Bytes { bytes, .. }
                if String::from_utf8_lossy(bytes).contains(encoded)
        ));
        let persisted = serde_json::to_string(&snapshot).unwrap();
        assert!(!persisted.contains(secret));
        assert!(!persisted.contains(encoded));
        assert!(persisted.contains("<redacted>"));
    }

    #[test]
    fn basic_auth_records_every_encoded_wire_variant_for_later_export() {
        let password_ref = SecretRef::new("basic-password").unwrap();
        let password = "actual-basic-password";
        let secrets = Secrets(HashMap::from([(
            password_ref.as_str().to_string(),
            password.into(),
        )]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = bare_request();
        request.url = "https://example.test/items".into();
        request.auth = AuthConfig::Basic {
            username: "collection-user".into(),
            password: password_ref,
        };

        let (prepared, _) = compile_request(&request, None, &context).unwrap();
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("collection-user:{password}"));
        let encoded_credential = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("", &credential)
            .finish()
            .strip_prefix('=')
            .unwrap()
            .to_string();

        assert!(prepared.redactions.contains(&credential));
        assert!(prepared.redactions.contains(&encoded_credential));
    }

    #[test]
    fn examples_redact_mixed_case_percent_escapes_and_nested_object_keys() {
        let credential = base64::engine::general_purpose::STANDARD
            .encode("collection-user:actual-basic-password");
        let encoded_credential = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("", &credential)
            .finish()
            .strip_prefix('=')
            .unwrap()
            .replace("%3D", "%3d");
        assert_ne!(encoded_credential, credential);

        let mut nested = serde_json::Map::new();
        nested.insert(credential.clone(), serde_json::json!("raw-key"));
        nested.insert(
            encoded_credential.clone(),
            serde_json::json!({ encoded_credential.clone(): format!("Basic {encoded_credential}") }),
        );
        let mut extensions = serde_json::Map::new();
        extensions.insert("nested".into(), serde_json::Value::Object(nested));
        let example = Example {
            id: ExampleId::new(),
            request_id: RequestId::new(),
            name: format!("captured {encoded_credential}"),
            request: None,
            response: ResponseSnapshot::default(),
            extensions,
            sort_key: 0,
        };

        let safe = redact_example(&example, std::slice::from_ref(&credential));
        let serialized = serde_json::to_string(&safe).unwrap();
        assert!(!serialized.contains(&credential));
        assert!(!serialized.contains(&encoded_credential));
        assert_eq!(safe.name, "captured <redacted>");

        let nested = safe.extensions["nested"].as_object().unwrap();
        assert_eq!(
            nested.len(),
            2,
            "redacted key collisions must retain both values"
        );
        assert!(nested.contains_key("<redacted>"));
        assert!(nested.contains_key("<redacted>#2"));
        assert!(
            nested
                .values()
                .any(|value| value == &serde_json::json!("raw-key"))
        );
        assert!(nested.values().any(|value| {
            value.as_object().is_some_and(|map| {
                map.get("<redacted>") == Some(&serde_json::json!("Basic <redacted>"))
            })
        }));
    }

    #[test]
    fn multi_pattern_redactor_prefers_longest_and_preserves_existing_markers() {
        let redactions = vec![
            "secret".to_string(),
            "secret-value".to_string(),
            "a".to_string(),
        ];
        let redactor = Redactor::new(&redactions);

        assert_eq!(
            redactor.redact("secret-value <redacted> secret"),
            "<redacted> <redacted> <redacted>"
        );
        let marker_spanning = Redactor::new(&["left<redacted>right".into()]);
        let once = marker_spanning.redact("before left<redacted>right after");
        assert_eq!(
            once, "before <redacted> after",
            "a secret containing an existing marker must still be matched as a whole"
        );
        assert_eq!(marker_spanning.redact(&once), once);
    }

    #[test]
    fn redacted_json_key_suffix_allocation_is_linear_at_supported_scale() {
        const KEY_COUNT: usize = 10_000;
        let mut map = serde_json::Map::new();
        let mut allocator = RedactedJsonKeyAllocator::default();

        for ordinal in 1..=KEY_COUNT {
            let key = allocator.allocate(&map, "<redacted>".into());
            map.insert(key, serde_json::json!(ordinal));
        }

        assert_eq!(map.len(), KEY_COUNT);
        assert!(map.contains_key("<redacted>"));
        assert!(map.contains_key(&format!("<redacted>#{KEY_COUNT}")));
        assert_eq!(
            allocator.existence_checks,
            KEY_COUNT * 2 - 1,
            "each collision must resume from the next unused suffix"
        );
    }

    #[test]
    fn short_secrets_do_not_rewrite_typed_replay_discriminants() {
        let secret_ref = SecretRef::new("short-secret").unwrap();
        let secrets = Secrets(HashMap::from([(
            secret_ref.as_str().to_string(),
            "a".into(),
        )]));
        let global = vec![variable("token", VariableValue::Secret(secret_ref.clone()))];
        let context = CompileContext {
            global: &global,
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/items".into();
        request.params.clear();
        request.auth = AuthConfig::Bearer { token: secret_ref };
        request.body = Body::Raw {
            media_type: super::super::RawBodyKind::Json,
            text: "a".into(),
        };

        let (_, snapshot) = compile_request(&request, None, &context).unwrap();
        let replay = snapshot.replay.unwrap();
        assert_eq!(replay.method, request.method);
        assert_eq!(replay.headers[0].value, "<redacted>");
        assert!(matches!(replay.auth, AuthConfig::Bearer { .. }));
        assert!(matches!(
            replay.body,
            Body::Raw {
                media_type: super::super::RawBodyKind::Json,
                ref text
            } if text == "<redacted>"
        ));
    }

    #[test]
    fn rejects_url_userinfo_before_a_snippet_or_history_snapshot_exists() {
        let secrets = Secrets(HashMap::from([(
            "userinfo-password".into(),
            "actual-password".into(),
        )]));
        let global = vec![variable(
            "password",
            VariableValue::Secret(SecretRef::new("userinfo-password").unwrap()),
        )];
        let context = CompileContext {
            global: &global,
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        for url in [
            "https://operator:actual-password@example.test/items",
            "https://operator@example.test/items",
            "https://operator:{{password}}@example.test/items",
        ] {
            let mut request = request();
            request.url = url.into();
            request.params.clear();

            let error = compile_request(&request, None, &context).unwrap_err();
            assert_eq!(
                error,
                CompileError::InvalidUrl("request URL must not include user information".into())
            );
            assert!(!error.to_string().contains("actual-password"));
        }
    }

    #[test]
    fn snapshot_retains_the_submitted_typed_request_for_lossless_replay() {
        let secrets = Secrets(HashMap::new());
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut submitted = request();
        submitted.url = "https://example.test/graphql".into();
        submitted.params = vec![KeyValueRow::enabled("revision", "two")];
        submitted.headers = vec![KeyValueRow::enabled("X-Duplicate", "first")];
        submitted.body = Body::GraphQl {
            query: "query Item { item { id } }".into(),
            variables: r#"{"id":2}"#.into(),
        };
        submitted.settings.timeout_ms = 9_876;

        let (_, snapshot) = compile_request(&submitted, None, &context).unwrap();
        let replay = snapshot
            .replay
            .expect("compiler must retain typed replay data");
        assert_eq!(replay.name, submitted.name);
        assert_eq!(replay.method, submitted.method);
        assert_eq!(replay.url, submitted.url);
        assert_eq!(replay.params, submitted.params);
        assert_eq!(replay.headers, submitted.headers);
        assert_eq!(replay.auth, submitted.auth);
        assert_eq!(replay.body, submitted.body);
        assert_eq!(replay.variables, submitted.variables);
        assert_eq!(replay.scripts, submitted.scripts);
        assert_eq!(replay.settings, submitted.settings);
    }

    #[test]
    fn replay_snapshot_scrubs_literal_credentials_from_templated_urls_and_headers() {
        let secrets = Secrets(HashMap::new());
        let global = vec![variable(
            "base_url",
            VariableValue::Plain("https://example.test/items".into()),
        )];
        let context = CompileContext {
            global: &global,
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut submitted = request();
        submitted.url = "{{base_url}}?api_key=literal-replay-query-secret".into();
        submitted.params.clear();
        submitted.headers = vec![KeyValueRow::enabled(
            "Authorization",
            "Bearer literal-replay-header-secret",
        )];
        submitted.body = Body::None;

        let (_, snapshot) = compile_request(&submitted, None, &context).unwrap();
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains("literal-replay-query-secret"));
        assert!(!serialized.contains("literal-replay-header-secret"));
        let replay = snapshot.replay.unwrap();
        assert!(replay.url.contains("api_key=<redacted>"));
        assert!(!replay.headers[0].enabled);
    }

    #[test]
    fn reports_variable_cycles_with_the_path() {
        let secrets = Secrets(HashMap::new());
        let global = vec![
            variable("a", VariableValue::Plain("{{b}}".into())),
            variable("b", VariableValue::Plain("{{a}}".into())),
        ];
        let context = CompileContext {
            global: &global,
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/{{a}}".into();
        assert_eq!(
            compile_request(&request, None, &context).unwrap_err(),
            CompileError::VariableCycle(vec!["a".into(), "b".into(), "a".into()])
        );
    }

    #[test]
    fn workspace_ids_are_normalized_and_non_empty() {
        assert_eq!(
            WorkspaceId::new(r"C:\work\api").unwrap().as_str(),
            "C:/work/api"
        );
        assert!(WorkspaceId::new("  ").is_err());
    }

    #[test]
    fn compiles_multipart_files_and_defers_sigv4_with_redacted_credentials() {
        let secrets = Secrets(HashMap::from([
            ("aws-access".into(), "AKIA-ACTUAL".into()),
            ("aws-secret".into(), "aws-actual-secret".into()),
            ("aws-session".into(), "aws-actual-session-token".into()),
            ("part-name".into(), "actual-secret-field-name".into()),
        ]));
        let global = vec![variable(
            "part_name",
            VariableValue::Secret(SecretRef::new("part-name").unwrap()),
        )];
        let context = CompileContext {
            global: &global,
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = request();
        request.url = "https://example.test/upload".into();
        request.params.clear();
        request.headers.clear();
        request.auth = AuthConfig::AwsSigV4 {
            access_key: SecretRef::new("aws-access").unwrap(),
            secret_key: SecretRef::new("aws-secret").unwrap(),
            session_token: Some(SecretRef::new("aws-session").unwrap()),
            region: "us-east-1".into(),
            service: "execute-api".into(),
        };
        request.body = Body::Multipart {
            rows: vec![
                super::super::MultipartRow::text("{{part_name}}", "safe"),
                super::super::MultipartRow::file("{{part_name}}", "/home/user/private/payload.bin"),
            ],
        };
        let (prepared, snapshot) = compile_request(&request, None, &context).unwrap();
        let signing = prepared.aws_sigv4.as_ref().unwrap();
        assert_eq!(signing.access_key, "AKIA-ACTUAL");
        assert_eq!(signing.secret_key, "aws-actual-secret");
        assert_eq!(
            signing.session_token.as_deref(),
            Some("aws-actual-session-token")
        );
        assert!(matches!(
            &prepared.body,
            PreparedBody::Multipart(parts)
                if matches!(parts[1].value, PreparedMultipartValue::File(ref path) if path == "/home/user/private/payload.bin")
        ));
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("aws-actual-secret"));
        assert!(!debug.contains("aws-actual-session-token"));
        let snapshot = serde_json::to_string(&snapshot).unwrap();
        assert!(snapshot.contains("<redacted>=<file>"));
        assert!(!snapshot.contains("actual-secret-field-name"));
        assert!(!snapshot.contains("/home/user/private/payload.bin"));
        assert!(!snapshot.contains("aws-actual-secret"));
        assert!(!snapshot.contains("aws-actual-session-token"));
    }

    #[test]
    fn folder_variables_and_auth_override_collection_inheritance() {
        let secrets = Secrets(HashMap::from([(
            "folder-password".into(),
            "actual-password".into(),
        )]));
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = Collection {
            id: CollectionId::new(),
            workspace_id: workspace,
            name: "API".into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: vec![variable(
                "host",
                VariableValue::Plain("collection.example.test".into()),
            )],
            scripts: Default::default(),
            extensions: Default::default(),
        };
        collection.auth = AuthConfig::Bearer {
            token: SecretRef::new("unused-collection-token").unwrap(),
        };
        let folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Admin".into(),
            auth: AuthConfig::Basic {
                username: "operator".into(),
                password: SecretRef::new("folder-password").unwrap(),
            },
            variables: vec![variable(
                "host",
                VariableValue::Plain("folder.example.test".into()),
            )],
            scripts: Default::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let mut request = request();
        request.collection_id = collection.id.clone();
        request.folder_id = Some(folder.id.clone());
        request.url = "https://{{host}}/items".into();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;
        request.method = HttpMethod::get();
        request.auth = AuthConfig::Inherit;
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let (prepared, snapshot) =
            compile_request_with_folder(&request, Some(&collection), Some(&folder), &context)
                .unwrap();
        assert_eq!(prepared.url, "https://folder.example.test/items");
        assert!(
            snapshot
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "<redacted>")
        );
        assert!(matches!(
            snapshot.replay.as_ref().map(|replay| &replay.auth),
            Some(AuthConfig::Basic { username, password })
                if username == "operator" && password.as_str() == "folder-password"
        ));
        assert!(!format!("{prepared:?}").contains("actual-password"));
    }

    fn bare_request() -> SavedRequest {
        let mut request = request();
        request.params.clear();
        request.headers.clear();
        request.body = Body::None;
        request.method = HttpMethod::get();
        request.variables.clear();
        request
    }

    #[test]
    fn relative_urls_take_the_environment_base_url() {
        let secrets = Secrets(HashMap::new());
        let environment = vec![variable("v", VariableValue::Plain("v3".into()))];
        let mut context = CompileContext {
            global: &[],
            environment: &environment,
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: Some("https://api.example.test/{{v}}/"),
            environment_auth: None,
        };
        for (url, expected) in [
            ("/pets?limit=1", "https://api.example.test/v3/pets?limit=1"),
            ("pets", "https://api.example.test/v3/pets"),
            ("{{base_url}}/pets", "https://api.example.test/v3//pets"),
            ("{{baseUrl}}/pets", "https://api.example.test/v3//pets"),
            (
                "https://other.example.test/pets",
                "https://other.example.test/pets",
            ),
        ] {
            let mut request = bare_request();
            request.url = url.into();
            let (prepared, _) = compile_request(&request, None, &context).unwrap();
            assert_eq!(prepared.url, expected, "{url}");
        }

        // An explicit variable shadows the synthetic one.
        let shadowed = vec![variable(
            "base_url",
            VariableValue::Plain("https://shadow.example.test".into()),
        )];
        context.global = &shadowed;
        let mut request = bare_request();
        request.url = "{{base_url}}/pets".into();
        let (prepared, _) = compile_request(&request, None, &context).unwrap();
        assert_eq!(prepared.url, "https://shadow.example.test/pets");

        // Without a base URL a relative path is a clear error.
        context.global = &[];
        context.environment_base_url = None;
        request.url = "/pets".into();
        assert_eq!(
            compile_request(&request, None, &context).unwrap_err(),
            CompileError::InvalidUrl(
                "Relative URL needs a Base URL on the active environment.".into()
            )
        );
        assert!(is_relative_url("pets?x=1"));
        assert!(!is_relative_url("http://h/pets"));
        assert!(!is_relative_url(" "));
        assert_eq!(join_base_url("https://h/", "?page=2"), "https://h?page=2");
        assert_eq!(join_base_url("https://h", "pets"), "https://h/pets");
    }

    #[test]
    fn inherit_falls_through_to_the_environment_auth() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = Collection {
            id: CollectionId::new(),
            workspace_id: workspace,
            name: "API".into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Default::default(),
            extensions: Default::default(),
        };
        let folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Admin".into(),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Default::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let environment_auth = AuthConfig::Bearer {
            token: SecretRef::new("env-bearer").unwrap(),
        };
        let mut request = bare_request();
        request.collection_id = collection.id.clone();
        request.folder_id = Some(folder.id.clone());
        request.auth = AuthConfig::Inherit;
        assert_eq!(
            effective_auth(
                &request,
                &[&folder],
                Some(&collection),
                Some(&environment_auth)
            ),
            &environment_auth
        );
        assert_eq!(
            effective_auth(&request, &[&folder], Some(&collection), None),
            &AuthConfig::None
        );
        // A request that says "none" means none, even with an environment auth.
        request.auth = AuthConfig::None;
        assert_eq!(
            effective_auth(
                &request,
                &[&folder],
                Some(&collection),
                Some(&environment_auth)
            ),
            &AuthConfig::None
        );
        // Anything explicit closer to the request wins.
        request.auth = AuthConfig::Inherit;
        let mut folder_with_auth = folder.clone();
        folder_with_auth.auth = AuthConfig::Basic {
            username: "operator".into(),
            password: SecretRef::new("folder-password").unwrap(),
        };
        assert_eq!(
            effective_auth(
                &request,
                &[&folder_with_auth],
                Some(&collection),
                Some(&environment_auth)
            ),
            &folder_with_auth.auth
        );

        // And the compiled request carries the environment header.
        let secrets = Secrets(HashMap::from([("env-bearer".into(), "env-value".into())]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: Some(&environment_auth),
        };
        request.url = "https://api.example.test/pets".into();
        let (prepared, snapshot) =
            compile_request_with_folder_chain(&request, Some(&collection), &[&folder], &context)
                .unwrap();
        assert!(
            prepared
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer env-value")
        );
        assert!(!format!("{snapshot:?}").contains("env-value"));
    }

    #[test]
    fn login_auth_sends_the_cached_session_or_asks_to_sign_in() {
        let secrets = Secrets(HashMap::from([("login-cache".into(), "sess-1".into())]));
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: None,
        };
        let mut request = bare_request();
        request.url = "https://api.example.test/pets".into();
        let login = |cache: Option<SecretRef>, expires_at: Option<i64>| AuthConfig::Login {
            url: "https://api.example.test/login".into(),
            basic: None,
            headers: Vec::new(),
            method: "POST".into(),
            body: r#"{"user":"{{login_user}}"}"#.into(),
            token_path: "data.session".into(),
            ttl_secs: None,
            access_token: cache,
            expires_at,
        };
        request.auth = login(None, None);
        assert_eq!(
            compile_request(&request, None, &context).unwrap_err(),
            CompileError::Unsupported("not signed in yet; sign in before sending".into())
        );
        request.auth = login(Some(SecretRef::new("login-cache").unwrap()), Some(1));
        assert_eq!(
            compile_request(&request, None, &context).unwrap_err(),
            CompileError::Unsupported(
                "sign-in session has expired; sign in again before sending".into()
            )
        );
        request.auth = login(
            Some(SecretRef::new("login-cache").unwrap()),
            Some(unix_timestamp() + 600),
        );
        let (prepared, snapshot) = compile_request(&request, None, &context).unwrap();
        assert!(
            prepared
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer sess-1")
        );
        assert!(!format!("{snapshot:?}").contains("sess-1"));
    }
}
