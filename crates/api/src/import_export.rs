use super::compile::{
    redact_export_graph, redact_freeform_json_map, redact_saved_request_fields, redact_text,
    secret_redaction_variants,
};
use super::{
    ApiKeyLocation, AuthConfig, Body, Collection, CollectionId, Environment, EnvironmentId,
    Example, ExampleId, Folder, FolderId, HttpMethod, KeyValueRow, MultipartRow, MultipartValue,
    PreparedRequest, RawBodyKind, RedactedRequestSnapshot, RequestId, RequestSettings,
    ResponseSnapshot, RowId, SavedRequest, Scripts, SecretRef, SecretResolver, Variable,
    VariableValue, WorkspaceId, redact_example,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const MAX_IMPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMPORTED_REQUESTS: usize = 10_000;
const MAX_REFERENCE_DOCUMENTS: usize = 64;
const MAX_REFERENCE_DEPTH: usize = 16;

mod compatibility;
mod har;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportFormat {
    OpenApi,
    PostmanCollection,
    PostmanEnvironment,
    Insomnia,
    Har,
    Curl,
    AgentOps,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportResult {
    pub format: ImportFormat,
    /// Canonical URI of the selected root document, when the caller supplied
    /// one. This remains attached to staged imports so relative references and
    /// review diagnostics do not lose their trust boundary.
    #[serde(default)]
    pub origin: Option<ImportOrigin>,
    pub collection: Collection,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub requests: Vec<SavedRequest>,
    #[serde(default)]
    pub environments: Vec<Environment>,
    #[serde(default)]
    pub examples: Vec<Example>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportOrigin {
    pub uri: String,
}

impl ImportOrigin {
    pub fn new(uri: impl Into<String>) -> Result<Self, String> {
        let uri = uri.into();
        let parsed = url::Url::parse(&uri)
            .map_err(|error| format!("import origin must be an absolute URI: {error}"))?;
        if !matches!(parsed.scheme(), "file" | "https") {
            return Err("import origin must use file: or https:".into());
        }
        Ok(Self { uri })
    }
}

/// Fetches one canonical, fragment-free reference URI. Implementations own the
/// file-root/HTTPS allowlist, SSRF checks, redirect policy, deadline, and byte
/// ceiling; core only performs bounded parsing and reference traversal.
pub trait ImportReferenceResolver: Send + Sync {
    fn resolve(&self, canonical_uri: &str) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportSelection {
    pub include_collection: bool,
    #[serde(default)]
    pub folder_ids: BTreeSet<FolderId>,
    #[serde(default)]
    pub request_ids: BTreeSet<RequestId>,
    #[serde(default)]
    pub environment_ids: BTreeSet<EnvironmentId>,
    #[serde(default)]
    pub example_ids: BTreeSet<ExampleId>,
}

impl ImportSelection {
    pub fn all(imported: &ImportResult) -> Self {
        Self {
            include_collection: imported.format != ImportFormat::PostmanEnvironment,
            folder_ids: imported
                .folders
                .iter()
                .map(|value| value.id.clone())
                .collect(),
            request_ids: imported
                .requests
                .iter()
                .map(|value| value.id.clone())
                .collect(),
            environment_ids: imported
                .environments
                .iter()
                .map(|value| value.id.clone())
                .collect(),
            example_ids: imported
                .examples
                .iter()
                .map(|value| value.id.clone())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortableBundle {
    pub version: u32,
    pub workspace_id: WorkspaceId,
    pub collection: Collection,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub requests: Vec<SavedRequest>,
    #[serde(default)]
    pub environments: Vec<Environment>,
    #[serde(default)]
    pub examples: Vec<Example>,
}

#[derive(Clone, PartialEq)]
pub struct RedactedExportRequest {
    definition: SavedRequest,
    redactions: Arc<[String]>,
}

#[derive(Clone, PartialEq)]
pub struct PreparedCollectionExport {
    collection: Collection,
    folders: Vec<Folder>,
    requests: Vec<RedactedExportRequest>,
    environments: Vec<Environment>,
    examples: Vec<Example>,
    redaction_count: usize,
}

struct BasicAuthOwner {
    username: String,
    password: SecretRef,
}

impl std::fmt::Debug for RedactedExportRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedactedExportRequest")
            .field("definition", &self.definition)
            .field("redaction_count", &self.redactions.len())
            .finish()
    }
}

impl std::fmt::Debug for PreparedCollectionExport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedCollectionExport")
            .field("folder_count", &self.folders.len())
            .field("request_count", &self.requests.len())
            .field("environment_count", &self.environments.len())
            .field("example_count", &self.examples.len())
            .field("redaction_count", &self.redaction_count)
            .finish()
    }
}

impl RedactedExportRequest {
    pub fn from_compiled(
        definition: &SavedRequest,
        prepared: &PreparedRequest,
    ) -> Result<Self, String> {
        if definition.id != prepared.request_id {
            return Err("compiled request does not match the saved request".into());
        }
        Self::from_portable(definition, Arc::from(prepared.redactions.as_slice()))
    }

    pub fn definition(&self) -> &SavedRequest {
        &self.definition
    }

    fn from_portable(definition: &SavedRequest, redactions: Arc<[String]>) -> Result<Self, String> {
        let mut safe = super::persistence_safe_saved_request(definition)?;
        redact_saved_request_fields(&mut safe, &redactions);
        Ok(Self {
            definition: safe,
            redactions,
        })
    }
}

impl PreparedCollectionExport {
    /// Prepare portable request definitions without requiring them to be
    /// executable. Secret references are resolved only to build the redaction
    /// set; templates, relative URLs, and unsupported imported auth remain
    /// intact for the destination format.
    pub fn new(
        collection: &Collection,
        folders: &[Folder],
        requests: &[SavedRequest],
        environments: &[Environment],
        examples: &[Example],
        secrets: &dyn SecretResolver,
    ) -> Result<Self, String> {
        let raw_requests = requests
            .iter()
            .cloned()
            .map(|definition| RedactedExportRequest {
                definition,
                redactions: Arc::from([]),
            })
            .collect::<Vec<_>>();
        validate_export_graph(collection, folders, &raw_requests, environments, examples)?;
        let mut references = BTreeMap::<String, SecretRef>::new();
        let mut basic_auth_owners = Vec::new();
        // Named vault references can occur in any template without an
        // explicit Variable row. Include their values in graph redactions.
        for value in [
            serde_json::to_value(collection),
            serde_json::to_value(folders),
            serde_json::to_value(requests),
            serde_json::to_value(environments),
            serde_json::to_value(examples),
        ] {
            collect_named_vault_references(
                &value.map_err(|error| error.to_string())?,
                &mut references,
            )?;
        }
        collect_variable_secret_references(&collection.variables, &mut references);
        collect_auth_secret_references(&collection.auth, &mut references, &mut basic_auth_owners);
        for folder in folders {
            collect_variable_secret_references(&folder.variables, &mut references);
            collect_auth_secret_references(&folder.auth, &mut references, &mut basic_auth_owners);
        }
        for request in requests {
            collect_variable_secret_references(&request.variables, &mut references);
            collect_auth_secret_references(&request.auth, &mut references, &mut basic_auth_owners);
        }
        for environment in environments {
            collect_variable_secret_references(&environment.variables, &mut references);
            collect_auth_secret_references(
                &environment.auth,
                &mut references,
                &mut basic_auth_owners,
            );
        }
        let mut redactions = BTreeSet::new();
        let mut resolved_secrets = BTreeMap::<String, String>::new();
        for (key, reference) in &references {
            // The artifact carries references, never values. A secret this session
            // cannot resolve (never entered, deleted from the vault, a token not fetched
            // yet) has no value that could be leaked through it, so it must not block
            // the export.
            if let Ok(secret) = secrets.resolve(reference) {
                add_redaction_variants(&mut redactions, &secret);
                resolved_secrets.insert(key.clone(), secret);
            }
        }
        let mut templated_passwords = BTreeSet::new();
        for owner in &basic_auth_owners {
            let Some(password) = resolved_secrets.get(owner.password.as_str()) else {
                continue;
            };
            let vault_username = crate::vault::parse_vault_expression(&owner.username)
                .ok()
                .flatten()
                .and_then(|reference| resolved_secrets.get(reference.as_str()));
            // Collection runs can introduce arbitrary iteration-data and
            // script-local values, so a templated username can produce a wire
            // credential the export cannot compute. Find those by decoding
            // what the graph actually contains instead.
            if owner.username.contains("{{") && vault_username.is_none() {
                templated_passwords.insert(password.clone());
                continue;
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{password}",
                vault_username.unwrap_or(&owner.username)
            ));
            add_redaction_variants(&mut redactions, &encoded);
        }
        if !templated_passwords.is_empty() {
            for value in [
                serde_json::to_value(collection),
                serde_json::to_value(folders),
                serde_json::to_value(requests),
                serde_json::to_value(environments),
                serde_json::to_value(examples),
            ] {
                collect_basic_credentials_for_passwords(
                    &value.map_err(|error| error.to_string())?,
                    &templated_passwords,
                    &mut redactions,
                );
            }
        }
        let redactions = redactions.into_iter().collect::<Vec<_>>();
        let redaction_count = redactions.len();
        let mut safe_collection = super::persistence_safe_collection(collection);
        let mut safe_folders = folders
            .iter()
            .map(super::persistence_safe_folder)
            .collect::<Vec<_>>();
        let mut safe_requests = requests
            .iter()
            .map(super::persistence_safe_saved_request)
            .collect::<Result<Vec<_>, _>>()?;
        let mut safe_environments = environments
            .iter()
            .map(super::persistence_safe_environment)
            .collect::<Vec<_>>();
        let mut safe_examples = examples.to_vec();
        redact_export_graph(
            &mut safe_collection,
            &mut safe_folders,
            &mut safe_requests,
            &mut safe_environments,
            &mut safe_examples,
            &redactions,
        );
        let empty_redactions: Arc<[String]> = Arc::from([]);
        let mut requests = safe_requests
            .into_iter()
            .map(|definition| RedactedExportRequest {
                definition,
                redactions: Arc::clone(&empty_redactions),
            })
            .collect::<Vec<_>>();
        validate_export_graph(
            &safe_collection,
            &safe_folders,
            &requests,
            &safe_environments,
            &safe_examples,
        )?;
        safe_folders.sort_by(|left, right| {
            left.sort_key
                .cmp(&right.sort_key)
                .then(left.id.cmp(&right.id))
        });
        requests.sort_by(|left, right| {
            left.definition
                .sort_key
                .cmp(&right.definition.sort_key)
                .then(left.definition.id.cmp(&right.definition.id))
        });
        safe_environments
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(Self {
            collection: safe_collection,
            folders: safe_folders,
            requests,
            environments: safe_environments,
            examples: safe_examples,
            redaction_count,
        })
    }
}

fn add_redaction_variants(redactions: &mut BTreeSet<String>, value: &str) {
    for variant in secret_redaction_variants(value) {
        redactions.insert(variant);
    }
}

/// Redact every Basic-auth token in `value` whose decoded password is one of
/// `passwords`, whatever username it was built with. Tokens are looked for in
/// each string as stored and percent-decoded, so form-encoded copies match.
fn collect_basic_credentials_for_passwords(
    value: &Value,
    passwords: &BTreeSet<String>,
    redactions: &mut BTreeSet<String>,
) {
    match value {
        Value::String(text) => {
            for candidate in [text.clone(), percent_decode_lossy(text)] {
                for token in candidate
                    .split(|character: char| {
                        !(character.is_ascii_alphanumeric() || "+/=-_".contains(character))
                    })
                    .filter(|token| token.len() >= 4)
                {
                    if basic_token_uses_password(token, passwords) {
                        add_redaction_variants(redactions, token);
                    }
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_basic_credentials_for_passwords(value, passwords, redactions);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_basic_credentials_for_passwords(value, passwords, redactions);
            }
        }
        _ => {}
    }
}

fn basic_token_uses_password(token: &str, passwords: &BTreeSet<String>) -> bool {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
        .iter()
        .filter_map(|engine| engine.decode(token).ok())
        .filter_map(|bytes| String::from_utf8(bytes).ok())
        .any(|decoded| {
            decoded
                .split_once(':')
                .is_some_and(|(_, password)| passwords.contains(password))
        })
}

fn percent_decode_lossy(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn collect_named_vault_references(
    value: &Value,
    references: &mut BTreeMap<String, SecretRef>,
) -> Result<(), String> {
    match value {
        Value::String(value) => {
            let mut remaining = value.as_str();
            while let Some(start) = remaining.find("{{vault.") {
                let after = &remaining[start + "{{vault.".len()..];
                // A malformed reference names no vault entry, so there is no
                // value to redact; it travels as the text it is.
                let Some(end) = after.find("}}") else {
                    break;
                };
                if let Ok(reference) = crate::vault::vault_secret_reference(after[..end].trim()) {
                    references
                        .entry(reference.as_str().to_string())
                        .or_insert(reference);
                }
                remaining = &after[end + 2..];
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_named_vault_references(value, references)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_named_vault_references(value, references)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_variable_secret_references(
    variables: &[Variable],
    references: &mut BTreeMap<String, SecretRef>,
) {
    for variable in variables {
        if let VariableValue::Secret(reference) = &variable.value {
            references
                .entry(reference.as_str().to_string())
                .or_insert_with(|| reference.clone());
        }
    }
}

fn collect_auth_secret_references(
    auth: &AuthConfig,
    references: &mut BTreeMap<String, SecretRef>,
    basic_auth_owners: &mut Vec<BasicAuthOwner>,
) {
    let mut add = |reference: &SecretRef| {
        references
            .entry(reference.as_str().to_string())
            .or_insert_with(|| reference.clone());
    };
    match auth {
        AuthConfig::ApiKey { value, .. } => add(value),
        AuthConfig::Basic { username, password } => {
            add(password);
            basic_auth_owners.push(BasicAuthOwner {
                username: username.clone(),
                password: password.clone(),
            });
        }
        AuthConfig::Bearer { token } | AuthConfig::OAuth2 { token } => add(token),
        AuthConfig::OAuth2AuthorizationCodePkce {
            access_token,
            refresh_token,
            ..
        } => {
            if let Some(reference) = access_token {
                add(reference);
            }
            if let Some(reference) = refresh_token {
                add(reference);
            }
        }
        AuthConfig::OAuth2ClientCredentials {
            client_secret,
            access_token,
            ..
        } => {
            add(client_secret);
            if let Some(reference) = access_token {
                add(reference);
            }
        }
        AuthConfig::OAuth2Password {
            client_secret,
            password,
            access_token,
            refresh_token,
            ..
        } => {
            if let Some(reference) = client_secret {
                add(reference);
            }
            add(password);
            if let Some(reference) = access_token {
                add(reference);
            }
            if let Some(reference) = refresh_token {
                add(reference);
            }
        }
        AuthConfig::AwsSigV4 {
            access_key,
            secret_key,
            session_token,
            ..
        } => {
            add(access_key);
            add(secret_key);
            if let Some(reference) = session_token {
                add(reference);
            }
        }
        AuthConfig::Login {
            basic,
            access_token,
            ..
        } => {
            if let Some(basic) = basic {
                add(&basic.password);
                basic_auth_owners.push(BasicAuthOwner {
                    username: basic.username.clone(),
                    password: basic.password.clone(),
                });
            }
            if let Some(reference) = access_token {
                add(reference);
            }
        }
        AuthConfig::Inherit | AuthConfig::None | AuthConfig::Unsupported { .. } => {}
    }
}

pub fn import(workspace: &WorkspaceId, input: &[u8]) -> Result<ImportResult, String> {
    import_inner(workspace, input, None, None)
}

pub fn import_with_origin(
    workspace: &WorkspaceId,
    input: &[u8],
    origin: ImportOrigin,
    resolver: &dyn ImportReferenceResolver,
) -> Result<ImportResult, String> {
    import_inner(workspace, input, Some(origin), Some(resolver))
}

fn import_inner(
    workspace: &WorkspaceId,
    input: &[u8],
    origin: Option<ImportOrigin>,
    resolver: Option<&dyn ImportReferenceResolver>,
) -> Result<ImportResult, String> {
    if input.len() > MAX_IMPORT_BYTES {
        return Err(format!("import exceeds {MAX_IMPORT_BYTES} bytes"));
    }
    let text = std::str::from_utf8(input).map_err(|_| "import must be UTF-8 text".to_string())?;
    let trimmed = text.trim_start();
    if trimmed.starts_with("curl ") || trimmed.starts_with("curl\t") {
        let mut imported = import_curl(workspace, trimmed)?;
        imported.origin = origin;
        let credential_literals = collect_curl_credential_literals(&imported.requests);
        apply_import_credential_redactions(&mut imported, &credential_literals, &BTreeSet::new());
        return sanitize_import_result(&imported);
    }
    let mut value: Value = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(trimmed).map_err(|error| format!("invalid import JSON: {error}"))?
    } else {
        serde_yaml::from_str(trimmed).map_err(|error| format!("invalid import YAML: {error}"))?
    };
    if (value.get("openapi").is_some() || value.get("swagger").is_some())
        && let (Some(origin), Some(resolver)) = (origin.as_ref(), resolver)
    {
        value = resolve_external_openapi_references(value, origin, resolver)?;
    }
    let mut credential_discovery = collect_known_credential_literals(&value)?;
    if value.get("log").is_some() {
        har::collect_credentials(&value, &mut credential_discovery.literals);
    }
    let mut imported = if value.get("version").and_then(Value::as_u64) == Some(1)
        && value.get("collection").is_some()
        && value.get("workspace_id").is_some()
    {
        import_agentops(workspace, value)
    } else if value.get("openapi").is_some() || value.get("swagger").is_some() {
        import_openapi(workspace, value)
    } else if value
        .get("info")
        .and_then(|info| info.get("schema"))
        .is_some()
        && value.get("item").is_some()
    {
        import_postman(workspace, value)
    } else if value.get("_postman_variable_scope").and_then(Value::as_str) == Some("environment")
        || (value.get("values").is_some() && value.get("item").is_none())
    {
        import_postman_environment(workspace, value)
    } else if value.get("log").is_some() {
        har::import_har(workspace, value)
    } else if value.get("_type").and_then(Value::as_str) == Some("export") {
        import_insomnia(workspace, value)
    } else {
        Err(
            "unrecognized import format; expected OpenAPI, Postman, Insomnia, HAR, cURL, or AgentOps"
                .into(),
        )
    }?;
    if imported.format == ImportFormat::Har {
        credential_discovery
            .literals
            .extend(collect_curl_credential_literals(&imported.requests));
        credential_discovery
            .literals
            .sort_by_key(|value| std::cmp::Reverse(value.len()));
        credential_discovery.literals.dedup();
    }
    imported.origin = origin;
    apply_import_credential_redactions(
        &mut imported,
        &credential_discovery.literals,
        &credential_discovery.secret_template_variables,
    );
    compatibility::review(&mut imported);
    sanitize_import_result(&imported)
}

/// Re-applies the import trust-boundary policy immediately before persistence.
///
/// Import results are intentionally editable so the native review surface can
/// stage a selection. That also means callers can mutate retained extension or
/// unsupported-auth data after parsing. Persistence must therefore consume a
/// freshly sanitized copy rather than trusting the earlier parse pass.
pub(super) fn sanitize_import_result(imported: &ImportResult) -> Result<ImportResult, String> {
    let mut safe = imported.clone();
    invalidate_imported_auth(&mut safe.collection.auth);
    invalidate_imported_variables(&mut safe.collection.variables);
    sanitize_extensions(&mut safe.collection.extensions);
    safe.collection = super::persistence_safe_collection(&safe.collection);
    for folder in &mut safe.folders {
        invalidate_imported_auth(&mut folder.auth);
        invalidate_imported_variables(&mut folder.variables);
        sanitize_extensions(&mut folder.extensions);
        *folder = super::persistence_safe_folder(folder);
    }
    for request in &mut safe.requests {
        invalidate_imported_auth(&mut request.auth);
        invalidate_imported_variables(&mut request.variables);
        *request = super::persistence_safe_saved_request(request)?;
    }
    for environment in &mut safe.environments {
        invalidate_imported_auth(&mut environment.auth);
        invalidate_imported_variables(&mut environment.variables);
        sanitize_extensions(&mut environment.extensions);
        *environment = super::persistence_safe_environment(environment);
    }
    for example in &mut safe.examples {
        *example = redact_example(example, &[]);
        if let Some(request) = &mut example.request
            && let Some(replay) = &mut request.replay
        {
            invalidate_imported_auth(&mut replay.auth);
            invalidate_imported_variables(&mut replay.variables);
            super::persistence_safety::sanitize_replay_snapshot(replay)?;
        }
        sanitize_extensions(&mut example.extensions);
    }
    Ok(safe)
}

/// Compiled requests do not contain credentials from collection owners they
/// shadow, unused secret variables, or environments omitted from Postman.
/// Collection export therefore requires [`PreparedCollectionExport`].
pub fn export_agentops_bundle(
    _workspace: &WorkspaceId,
    _collection: &Collection,
    _folders: &[Folder],
    _requests: &[RedactedExportRequest],
    _environments: &[Environment],
    _examples: &[Example],
) -> Result<String, String> {
    Err("collection export requires a complete PreparedCollectionExport redaction context".into())
}

pub fn export_prepared_agentops_bundle(
    prepared: &PreparedCollectionExport,
) -> Result<String, String> {
    serde_json::to_string_pretty(&PortableBundle {
        version: 1,
        workspace_id: prepared.collection.workspace_id.clone(),
        collection: prepared.collection.clone(),
        folders: prepared.folders.clone(),
        requests: prepared
            .requests
            .iter()
            .map(|request| request.definition.clone())
            .collect(),
        environments: prepared.environments.clone(),
        examples: prepared.examples.clone(),
    })
    .map(|mut output| {
        output.push('\n');
        output
    })
    .map_err(|error| format!("serialize AgentOps bundle: {error}"))
}

/// Request-local compiler output cannot prove a complete collection redaction
/// set. Use [`export_prepared_postman_collection`] instead.
pub fn export_postman_collection(
    _collection: &Collection,
    _folders: &[Folder],
    _requests: &[RedactedExportRequest],
    _examples: &[Example],
) -> Result<String, String> {
    Err("collection export requires a complete PreparedCollectionExport redaction context".into())
}

pub fn export_prepared_postman_collection(
    prepared: &PreparedCollectionExport,
) -> Result<String, String> {
    let items = postman_items_for_parent(
        &prepared.collection,
        &prepared.folders,
        &prepared.requests,
        &prepared.examples,
        None,
    );
    let mut root = prepared.collection.extensions.clone();
    let mut info = match root.remove("postman_info") {
        Some(Value::Object(value)) => value,
        _ => serde_json::Map::new(),
    };
    info.insert(
        "name".into(),
        Value::String(prepared.collection.name.clone()),
    );
    info.insert(
        "description".into(),
        Value::String(prepared.collection.description.clone()),
    );
    info.insert(
        "schema".into(),
        Value::String(
            "https://schema.getpostman.com/json/collection/v2.1.0/collection.json".into(),
        ),
    );
    root.insert("info".into(), Value::Object(info));
    root.insert("item".into(), Value::Array(items));
    if let Some(auth) = postman_auth(&prepared.collection.auth) {
        root.insert("auth".into(), auth);
    }
    if !prepared.collection.variables.is_empty() {
        root.insert(
            "variable".into(),
            Value::Array(postman_variables(&prepared.collection.variables)),
        );
    }
    let events = postman_events(&prepared.collection.scripts);
    if !events.is_empty() {
        root.insert("event".into(), Value::Array(events));
    }
    serde_json::to_string_pretty(&Value::Object(root))
        .map(|mut output| {
            output.push('\n');
            output
        })
        .map_err(|error| format!("serialize Postman collection: {error}"))
}

pub fn export_curl(snapshot: &RedactedRequestSnapshot) -> String {
    let mut parts = vec![
        "curl".to_string(),
        "--request".into(),
        shell_quote(&snapshot.method),
    ];
    for (name, value) in &snapshot.headers {
        parts.push("--header".into());
        parts.push(shell_quote(&format!("{name}: {value}")));
    }
    if !snapshot.body.is_empty() {
        parts.push("--data-raw".into());
        parts.push(shell_quote(&snapshot.body));
    }
    parts.push(shell_quote(&snapshot.url));
    parts.join(" ")
}

fn import_agentops(workspace: &WorkspaceId, value: Value) -> Result<ImportResult, String> {
    let mut bundle: PortableBundle = serde_json::from_value(value)
        .map_err(|error| format!("invalid AgentOps bundle: {error}"))?;
    if bundle.version != 1 {
        return Err(format!(
            "unsupported AgentOps bundle version {}",
            bundle.version
        ));
    }
    let source_collection_id = bundle.collection.id.clone();
    let new_collection_id = CollectionId::new();
    let folder_ids: BTreeMap<FolderId, FolderId> = bundle
        .folders
        .iter()
        .map(|folder| (folder.id.clone(), FolderId::new()))
        .collect();
    let request_ids: BTreeMap<RequestId, RequestId> = bundle
        .requests
        .iter()
        .map(|request| (request.id.clone(), RequestId::new()))
        .collect();
    if folder_ids.len() != bundle.folders.len() || request_ids.len() != bundle.requests.len() {
        return Err("AgentOps bundle contains duplicate ids".into());
    }

    bundle.workspace_id = workspace.clone();
    bundle.collection.id = new_collection_id.clone();
    bundle.collection.workspace_id = workspace.clone();
    scrub_auth(&mut bundle.collection.auth);
    sanitize_extensions(&mut bundle.collection.extensions);
    for folder in &mut bundle.folders {
        if folder.collection_id != source_collection_id {
            return Err("AgentOps folder references a collection outside the bundle".into());
        }
        folder.id = folder_ids
            .get(&folder.id)
            .ok_or_else(|| "AgentOps folder id is missing from the bundle".to_string())?
            .clone();
        folder.collection_id = new_collection_id.clone();
        folder.parent_id = folder
            .parent_id
            .as_ref()
            .map(|parent| {
                folder_ids
                    .get(parent)
                    .cloned()
                    .ok_or_else(|| "AgentOps folder references a missing parent".to_string())
            })
            .transpose()?;
        scrub_auth(&mut folder.auth);
        sanitize_extensions(&mut folder.extensions);
    }
    for request in &mut bundle.requests {
        if request.collection_id != source_collection_id {
            return Err("AgentOps request references a collection outside the bundle".into());
        }
        request.id = request_ids
            .get(&request.id)
            .ok_or_else(|| "AgentOps request id is missing from the bundle".to_string())?
            .clone();
        request.collection_id = new_collection_id.clone();
        request.folder_id = request
            .folder_id
            .as_ref()
            .map(|folder| {
                folder_ids
                    .get(folder)
                    .cloned()
                    .ok_or_else(|| "AgentOps request references a missing folder".to_string())
            })
            .transpose()?;
        scrub_auth(&mut request.auth);
        sanitize_extensions(&mut request.extensions);
    }
    for environment in &mut bundle.environments {
        environment.id = EnvironmentId::new();
        environment.workspace_id = workspace.clone();
        scrub_environment_secrets(environment);
        sanitize_extensions(&mut environment.extensions);
    }
    for example in &mut bundle.examples {
        example.id = ExampleId::new();
        example.request_id = request_ids
            .get(&example.request_id)
            .cloned()
            .ok_or_else(|| "AgentOps example references a missing request".to_string())?;
        *example = redact_example(example, &[]);
        sanitize_extensions(&mut example.extensions);
    }
    let imported = ImportResult {
        format: ImportFormat::AgentOps,
        origin: None,
        collection: bundle.collection,
        folders: bundle.folders,
        requests: bundle.requests,
        environments: bundle.environments,
        examples: bundle.examples,
        warnings: Vec::new(),
    };
    super::store::validate_import_graph(workspace, &imported)
        .map_err(|error| format!("invalid AgentOps bundle graph: {error}"))?;
    Ok(imported)
}

fn import_openapi(workspace: &WorkspaceId, value: Value) -> Result<ImportResult, String> {
    let title = value
        .pointer("/info/title")
        .and_then(Value::as_str)
        .unwrap_or("Imported OpenAPI");
    let description = value
        .pointer("/info/description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut collection = new_collection(workspace, title, description);
    let mut warnings = Vec::new();
    collection.auth = import_openapi_auth(&value, value.get("security"), &mut warnings, title);
    collection.extensions = extension_map(&value, &["paths", "info", "servers"]);
    if let Some(info) = value.get("info") {
        collection
            .extensions
            .insert("openapi_info".into(), sanitize_import_value(info.clone()));
    }
    if let Some(servers) = value.get("servers") {
        collection.extensions.insert(
            "openapi_servers".into(),
            sanitize_import_value(servers.clone()),
        );
    }
    if let Some(metadata) = openapi_oauth_metadata(&value) {
        collection
            .extensions
            .insert("openapi_auth_metadata".into(), metadata);
    }
    let (base, server_variables) = openapi_base_url(&value);
    collection.variables.extend(server_variables);
    // An absolute server URL becomes an environment's Base URL and the
    // requests keep relative paths; a relative server (`/api/v3`) stays on
    // the request so the user only has to fill in the host.
    let mut environments = Vec::new();
    let base = if super::compile::is_relative_url(&base) {
        base
    } else {
        environments.push(Environment {
            id: super::EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: title.into(),
            base_url: base,
            auth: Default::default(),
            variables: Vec::new(),
            active: false,
            extensions: Default::default(),
        });
        String::new()
    };
    let mut requests = Vec::new();
    let mut folders = Vec::new();
    let mut folder_by_tag = BTreeMap::new();
    let mut examples = Vec::new();
    let Some(paths) = value.get("paths").and_then(Value::as_object) else {
        return Err("OpenAPI document has no paths object".into());
    };
    for (path, raw_path_item) in paths {
        let path_item = resolve_openapi_value(&value, raw_path_item);
        let Some(operations) = path_item.as_object() else {
            warnings.push(format!("ignored non-object path {path:?}"));
            continue;
        };
        for (method, raw_operation) in operations {
            if !matches!(
                method.as_str(),
                "get" | "post" | "put" | "patch" | "delete" | "head" | "options" | "trace"
            ) {
                continue;
            }
            let operation = resolve_openapi_value(&value, raw_operation);
            let source_operation = raw_path_item
                .as_object()
                .and_then(|path_item| path_item.get(method))
                .unwrap_or(raw_operation);
            let name = operation
                .get("summary")
                .or_else(|| operation.get("operationId"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{} {path}", method.to_ascii_uppercase()));
            let mut request = empty_request(
                &collection.id,
                name,
                method.to_ascii_uppercase(),
                format!("{base}{}", openapi_template(path)),
            )?;
            request.sort_key = i64::try_from(requests.len()).unwrap_or(i64::MAX);
            if let Some(tag) = operation
                .get("tags")
                .and_then(Value::as_array)
                .and_then(|tags| tags.first())
                .and_then(Value::as_str)
            {
                let folder_id = folder_by_tag.entry(tag.to_string()).or_insert_with(|| {
                    let id = FolderId::new();
                    folders.push(Folder {
                        id: id.clone(),
                        collection_id: collection.id.clone(),
                        parent_id: None,
                        name: tag.to_string(),
                        auth: AuthConfig::Inherit,
                        variables: Vec::new(),
                        scripts: Scripts::default(),
                        sort_key: i64::try_from(folders.len()).unwrap_or(i64::MAX),
                        extensions: Default::default(),
                    });
                    id
                });
                request.folder_id = Some(folder_id.clone());
            }
            request.auth = if operation.get("security").is_some() {
                import_openapi_auth(
                    &value,
                    operation.get("security"),
                    &mut warnings,
                    &request.name,
                )
            } else {
                AuthConfig::Inherit
            };
            request.extensions.insert(
                "openapi_operation".into(),
                sanitize_import_value(source_operation.clone()),
            );
            request.extensions.insert(
                "openapi_path_item".into(),
                sanitize_import_value(raw_path_item.clone()),
            );
            let mut swagger_form_data = Vec::new();
            let mut imported_parameters = BTreeSet::new();
            for parameter in operation
                .get("parameters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .chain(
                    path_item
                        .get("parameters")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
            {
                let parameter = resolve_openapi_value(&value, parameter);
                let Some(parameter_name) = parameter.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let location = parameter
                    .get("in")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !imported_parameters.insert((location.to_string(), parameter_name.to_string())) {
                    continue;
                }
                let default_value = parameter
                    .get("example")
                    .or_else(|| parameter.get("default"))
                    .or_else(|| parameter.pointer("/schema/default"))
                    .or_else(|| parameter.pointer("/schema/example"))
                    .map(json_scalar)
                    .unwrap_or_default();
                let mut row = KeyValueRow::enabled(parameter_name, default_value);
                row.description = parameter
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into();
                row.enabled = !parameter
                    .get("deprecated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match location {
                    "query" => request.params.push(row),
                    "header" => request.headers.push(row),
                    "path" => {
                        let description = if row.enabled {
                            row.description
                        } else if row.description.is_empty() {
                            "Deprecated OpenAPI path parameter".into()
                        } else {
                            format!("{} (deprecated)", row.description)
                        };
                        request.variables.push(Variable {
                            id: RowId::new(),
                            key: row.key,
                            value: VariableValue::Plain(row.value),
                            // Deprecated path parameters can still be required
                            // to form a valid URL. Preserve deprecation as
                            // metadata instead of disabling substitution.
                            enabled: true,
                            description,
                        });
                    }
                    "body" => {
                        let content_type = operation
                            .get("consumes")
                            .or_else(|| value.get("consumes"))
                            .and_then(Value::as_array)
                            .and_then(|values| values.first())
                            .and_then(Value::as_str)
                            .unwrap_or("application/json");
                        let example = openapi_schema_example(&value, parameter.get("schema"));
                        request.body = Body::Raw {
                            media_type: if content_type.contains("json") {
                                RawBodyKind::Json
                            } else if content_type.contains("xml") {
                                RawBodyKind::Xml
                            } else {
                                RawBodyKind::Text
                            },
                            text: if let Some(example) = example.as_str() {
                                example.into()
                            } else {
                                serde_json::to_string_pretty(&example).unwrap_or_default()
                            },
                        };
                        request
                            .headers
                            .push(KeyValueRow::enabled("Content-Type", content_type));
                    }
                    "formData" => swagger_form_data.push((
                        row,
                        parameter.get("type").and_then(Value::as_str) == Some("file"),
                    )),
                    _ => {}
                }
            }
            if !swagger_form_data.is_empty() && matches!(&request.body, Body::None) {
                let content_type = operation
                    .get("consumes")
                    .or_else(|| value.get("consumes"))
                    .and_then(Value::as_array)
                    .and_then(|values| values.first())
                    .and_then(Value::as_str)
                    .unwrap_or("multipart/form-data");
                if content_type == "application/x-www-form-urlencoded" {
                    request.body = Body::UrlEncoded {
                        rows: swagger_form_data.into_iter().map(|(row, _)| row).collect(),
                    };
                } else {
                    request.body = Body::Multipart {
                        rows: swagger_form_data
                            .into_iter()
                            .map(|(row, file)| MultipartRow {
                                id: row.id,
                                key: row.key,
                                value: if file {
                                    MultipartValue::File(row.value)
                                } else {
                                    MultipartValue::Text(row.value)
                                },
                                enabled: row.enabled,
                                description: row.description,
                            })
                            .collect(),
                    };
                }
                request
                    .headers
                    .push(KeyValueRow::enabled("Content-Type", content_type));
            }
            if let Some(raw_body) = operation.get("requestBody") {
                let body = resolve_openapi_value(&value, raw_body);
                if let Some((content_type, media)) = preferred_openapi_media(body.get("content")) {
                    let example = openapi_media_example(&value, media);
                    request.body = Body::Raw {
                        media_type: if content_type.contains("json") {
                            RawBodyKind::Json
                        } else if content_type.contains("xml") {
                            RawBodyKind::Xml
                        } else {
                            RawBodyKind::Text
                        },
                        text: if example.is_string() {
                            example.as_str().unwrap_or_default().to_string()
                        } else {
                            serde_json::to_string_pretty(&example).unwrap_or_else(|_| "{}".into())
                        },
                    };
                    if !request
                        .headers
                        .iter()
                        .any(|row| row.key.eq_ignore_ascii_case("content-type"))
                    {
                        request
                            .headers
                            .push(KeyValueRow::enabled("Content-Type", content_type));
                    }
                }
            }
            import_openapi_examples(&value, &operation, &request, &mut examples, &mut warnings);
            requests.push(request);
            if requests.len() > MAX_IMPORTED_REQUESTS {
                return Err(format!(
                    "import contains more than {MAX_IMPORTED_REQUESTS} requests"
                ));
            }
        }
    }
    if requests.is_empty() {
        warnings.push("OpenAPI document contains no supported HTTP operations".into());
    }
    Ok(ImportResult {
        format: ImportFormat::OpenApi,
        origin: None,
        collection,
        folders,
        requests,
        environments,
        examples,
        warnings,
    })
}

fn resolve_openapi_value(document: &Value, value: &Value) -> Value {
    fn resolve(document: &Value, value: &Value, seen: &mut BTreeSet<String>) -> Value {
        match value {
            Value::Object(object) => {
                let mut merged = if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                {
                    if let Some(pointer) = reference.strip_prefix('#') {
                        if seen.insert(reference.to_string()) {
                            let resolved = document
                                .pointer(pointer)
                                .map(|target| resolve(document, target, seen))
                                .unwrap_or_else(|| value.clone());
                            seen.remove(reference);
                            resolved.as_object().cloned().unwrap_or_default()
                        } else {
                            object.clone()
                        }
                    } else {
                        object.clone()
                    }
                } else {
                    serde_json::Map::new()
                };
                for (key, value) in object {
                    if key != "$ref" {
                        merged.insert(key.clone(), resolve(document, value, seen));
                    }
                }
                Value::Object(merged)
            }
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|value| resolve(document, value, seen))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }
    resolve(document, value, &mut BTreeSet::new())
}

fn resolve_external_openapi_references(
    root: Value,
    origin: &ImportOrigin,
    resolver: &dyn ImportReferenceResolver,
) -> Result<Value, String> {
    struct State<'a> {
        resolver: &'a dyn ImportReferenceResolver,
        documents: BTreeMap<String, Value>,
        fetched_bytes: usize,
    }

    impl State<'_> {
        fn document(&mut self, uri: &str) -> Result<Value, String> {
            if let Some(document) = self.documents.get(uri) {
                return Ok(document.clone());
            }
            if self.documents.len() >= MAX_REFERENCE_DOCUMENTS {
                return Err(format!(
                    "OpenAPI import exceeds {MAX_REFERENCE_DOCUMENTS} reference documents"
                ));
            }
            let bytes = self.resolver.resolve(uri)?;
            self.fetched_bytes = self
                .fetched_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| "OpenAPI reference byte count overflowed".to_string())?;
            if self.fetched_bytes > MAX_IMPORT_BYTES {
                return Err(format!(
                    "OpenAPI references exceed {MAX_IMPORT_BYTES} total bytes"
                ));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| format!("OpenAPI reference {uri:?} is not UTF-8"))?;
            let document: Value = if text.trim_start().starts_with(['{', '[']) {
                serde_json::from_str(text)
                    .map_err(|error| format!("invalid JSON reference {uri:?}: {error}"))?
            } else {
                serde_yaml::from_str(text)
                    .map_err(|error| format!("invalid YAML reference {uri:?}: {error}"))?
            };
            self.documents.insert(uri.to_string(), document.clone());
            Ok(document)
        }

        fn value(
            &mut self,
            current_uri: &str,
            current_document: &Value,
            value: &Value,
            depth: usize,
            seen: &mut BTreeSet<String>,
        ) -> Result<Value, String> {
            if depth > MAX_REFERENCE_DEPTH {
                return Err(format!(
                    "OpenAPI reference nesting exceeds {MAX_REFERENCE_DEPTH} levels"
                ));
            }
            match value {
                Value::Object(object) => {
                    let mut merged = serde_json::Map::new();
                    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                        let base = url::Url::parse(current_uri).map_err(|error| {
                            format!("invalid OpenAPI reference base {current_uri:?}: {error}")
                        })?;
                        let target = base.join(reference).map_err(|error| {
                            format!("invalid OpenAPI reference {reference:?}: {error}")
                        })?;
                        if !matches!(target.scheme(), "file" | "https") {
                            return Err(format!(
                                "OpenAPI reference must use file: or https:, got {:?}",
                                target.scheme()
                            ));
                        }
                        let fragment = target.fragment().unwrap_or_default().to_string();
                        let mut document_uri = target.clone();
                        document_uri.set_fragment(None);
                        let document_uri = document_uri.to_string();
                        let cycle_key = format!("{document_uri}#{fragment}");
                        if !seen.insert(cycle_key.clone()) {
                            return Err(format!("cyclic OpenAPI reference {cycle_key:?}"));
                        }
                        let document = if document_uri == current_uri {
                            current_document.clone()
                        } else {
                            self.document(&document_uri)?
                        };
                        let target_value = if fragment.is_empty() {
                            document.clone()
                        } else {
                            let pointer = fragment.strip_prefix('/').map_or_else(
                                || format!("/{fragment}"),
                                |pointer| format!("/{pointer}"),
                            );
                            document.pointer(&pointer).cloned().ok_or_else(|| {
                                format!("OpenAPI reference {cycle_key:?} does not exist")
                            })?
                        };
                        let resolved =
                            self.value(&document_uri, &document, &target_value, depth + 1, seen)?;
                        seen.remove(&cycle_key);
                        merged = resolved.as_object().cloned().ok_or_else(|| {
                            format!("OpenAPI reference {cycle_key:?} is not an object")
                        })?;
                    }
                    for (key, child) in object {
                        if key != "$ref" {
                            merged.insert(
                                key.clone(),
                                self.value(current_uri, current_document, child, depth, seen)?,
                            );
                        }
                    }
                    Ok(Value::Object(merged))
                }
                Value::Array(values) => values
                    .iter()
                    .map(|value| self.value(current_uri, current_document, value, depth, seen))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array),
                _ => Ok(value.clone()),
            }
        }
    }

    let mut root_uri =
        url::Url::parse(&origin.uri).map_err(|error| format!("invalid import origin: {error}"))?;
    root_uri.set_fragment(None);
    let root_uri = root_uri.to_string();
    let mut state = State {
        resolver,
        documents: BTreeMap::from([(root_uri.clone(), root.clone())]),
        fetched_bytes: 0,
    };
    state.value(&root_uri, &root, &root, 0, &mut BTreeSet::new())
}

fn preferred_openapi_media(content: Option<&Value>) -> Option<(&str, &Value)> {
    let content = content?.as_object()?;
    content
        .get_key_value("application/json")
        .or_else(|| content.iter().find(|(kind, _)| kind.ends_with("+json")))
        .or_else(|| content.iter().next())
        .map(|(kind, value)| (kind.as_str(), value))
}

fn openapi_media_example(document: &Value, media: &Value) -> Value {
    if let Some(example) = media.get("example") {
        return resolve_openapi_value(document, example);
    }
    if let Some(example) = media
        .get("examples")
        .and_then(Value::as_object)
        .and_then(|examples| examples.values().next())
    {
        let example = resolve_openapi_value(document, example);
        return example.get("value").cloned().unwrap_or(example);
    }
    openapi_schema_example(document, media.get("schema"))
}

fn openapi_schema_example(document: &Value, schema: Option<&Value>) -> Value {
    let Some(schema) = schema else {
        return Value::Object(Default::default());
    };
    let schema = resolve_openapi_value(document, schema);
    if let Some(example) = schema.get("example") {
        return example.clone();
    }
    if let Some(default) = schema.get("default") {
        return default.clone();
    }
    if let Some(value) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return value.clone();
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => Value::Object(
            schema
                .get("properties")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|properties| properties.iter())
                .map(|(name, property)| {
                    (
                        name.clone(),
                        openapi_schema_example(document, Some(property)),
                    )
                })
                .collect(),
        ),
        Some("array") => Value::Array(vec![openapi_schema_example(document, schema.get("items"))]),
        Some("boolean") => Value::Bool(false),
        Some("integer") | Some("number") => json!(0),
        _ => Value::String(String::new()),
    }
}

fn import_openapi_auth(
    document: &Value,
    security: Option<&Value>,
    warnings: &mut Vec<String>,
    owner: &str,
) -> AuthConfig {
    let Some(requirements) = security.and_then(Value::as_array) else {
        return AuthConfig::None;
    };
    if requirements.is_empty() {
        return AuthConfig::None;
    }
    let Some((name, required_scopes)) = requirements
        .iter()
        .filter_map(Value::as_object)
        .find_map(|requirement| requirement.iter().next())
    else {
        return AuthConfig::None;
    };
    let scheme = document
        .pointer(&format!("/components/securitySchemes/{name}"))
        .or_else(|| document.pointer(&format!("/securityDefinitions/{name}")))
        .map(|scheme| resolve_openapi_value(document, scheme));
    let Some(scheme) = scheme else {
        warnings.push(format!(
            "{owner:?} references missing OpenAPI security scheme {name:?}"
        ));
        return AuthConfig::Unsupported {
            name: name.clone(),
            raw: json!({"security_requirement": name}),
        };
    };
    let secret = |label: &str| SecretRef::generated(&format!("imported-{label}"));
    let required_scopes = required_scopes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    match scheme.get("type").and_then(Value::as_str) {
        Some("apiKey") => AuthConfig::ApiKey {
            name: scheme
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .into(),
            value: secret("openapi-api-key"),
            location: if scheme.get("in").and_then(Value::as_str) == Some("query") {
                ApiKeyLocation::Query
            } else {
                ApiKeyLocation::Header
            },
        },
        Some("http") if scheme.get("scheme").and_then(Value::as_str) == Some("basic") => {
            AuthConfig::Basic {
                username: String::new(),
                password: secret("openapi-basic-password"),
            }
        }
        Some("http") if scheme.get("scheme").and_then(Value::as_str) == Some("bearer") => {
            AuthConfig::Bearer {
                token: secret("openapi-bearer-token"),
            }
        }
        Some("basic") => AuthConfig::Basic {
            username: String::new(),
            password: secret("openapi-basic-password"),
        },
        Some("oauth2") => {
            let authorization_code = scheme.pointer("/flows/authorizationCode").or_else(|| {
                (scheme.get("flow").and_then(Value::as_str) == Some("accessCode"))
                    .then_some(&scheme)
            });
            if let Some(flow) = authorization_code
                && let (Some(authorization_endpoint), Some(token_endpoint)) = (
                    flow.get("authorizationUrl").and_then(Value::as_str),
                    flow.get("tokenUrl").and_then(Value::as_str),
                )
            {
                return AuthConfig::OAuth2AuthorizationCodePkce {
                    headers: Vec::new(),
                    authorization_endpoint: authorization_endpoint.into(),
                    token_endpoint: token_endpoint.into(),
                    client_id: String::new(),
                    scopes: required_scopes,
                    redirect_uri: String::new(),
                    access_token: None,
                    refresh_token: None,
                    expires_at: None,
                };
            }

            let client_credentials = scheme.pointer("/flows/clientCredentials").or_else(|| {
                (scheme.get("flow").and_then(Value::as_str) == Some("application"))
                    .then_some(&scheme)
            });
            if let Some(flow) = client_credentials
                && let Some(token_endpoint) = flow.get("tokenUrl").and_then(Value::as_str)
            {
                return AuthConfig::OAuth2ClientCredentials {
                    headers: Vec::new(),
                    token_endpoint: token_endpoint.into(),
                    client_id: String::new(),
                    client_secret: secret("openapi-oauth-client-secret"),
                    scopes: required_scopes,
                    access_token: None,
                    expires_at: None,
                };
            }

            warnings.push(format!(
                "{owner:?} uses an OAuth 2 flow that the Workbench cannot execute; its sanitized definition was retained"
            ));
            unsupported_openapi_oauth(name, scheme)
        }
        Some("openIdConnect") => {
            warnings.push(format!(
                "{owner:?} uses OpenID Connect discovery, which requires explicit OAuth endpoints before execution; its sanitized definition was retained"
            ));
            unsupported_openapi_oauth(name, scheme)
        }
        _ => {
            warnings.push(format!(
                "{owner:?} uses unsupported OpenAPI security scheme {name:?}; its sanitized definition was retained"
            ));
            AuthConfig::Unsupported {
                name: name.clone(),
                raw: sanitize_import_value(scheme),
            }
        }
    }
}

fn unsupported_openapi_oauth(name: &str, scheme: Value) -> AuthConfig {
    AuthConfig::Unsupported {
        name: name.into(),
        raw: sanitize_import_value(scheme),
    }
}

fn openapi_oauth_metadata(document: &Value) -> Option<Value> {
    let schemes = document
        .pointer("/components/securitySchemes")
        .or_else(|| document.get("securityDefinitions"))
        .and_then(Value::as_object)?;
    let mut output = serde_json::Map::new();
    for (name, scheme) in schemes {
        let kind = scheme.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("oauth2") | Some("openIdConnect")) {
            continue;
        }
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "scheme_kind".into(),
            Value::String(kind.unwrap_or_default().into()),
        );
        if let Some(discovery_url) = scheme.get("openIdConnectUrl").and_then(Value::as_str) {
            metadata.insert(
                "discovery_endpoint".into(),
                openapi_endpoint_metadata(discovery_url),
            );
        }
        if let Some(flows) = scheme.get("flows").and_then(Value::as_object) {
            let mut retained_flows = serde_json::Map::new();
            for (grant, flow) in flows {
                let mut retained = serde_json::Map::new();
                if let Some(login_url) = flow.get("authorizationUrl").and_then(Value::as_str) {
                    retained.insert(
                        "login_endpoint".into(),
                        openapi_endpoint_metadata(login_url),
                    );
                }
                if let Some(exchange_url) = flow.get("tokenUrl").and_then(Value::as_str) {
                    retained.insert(
                        "exchange_endpoint".into(),
                        openapi_endpoint_metadata(exchange_url),
                    );
                }
                if let Some(scopes) = flow.get("scopes") {
                    retained.insert("available_scopes".into(), scopes.clone());
                }
                retained_flows.insert(grant.clone(), Value::Object(retained));
            }
            metadata.insert("flows".into(), Value::Object(retained_flows));
        }
        if let Some(flow) = scheme.get("flow").and_then(Value::as_str) {
            metadata.insert("swagger_grant".into(), Value::String(flow.into()));
        }
        output.insert(name.clone(), Value::Object(metadata));
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

fn openapi_endpoint_metadata(value: &str) -> Value {
    let Ok(url) = url::Url::parse(value) else {
        return json!({"invalid": true});
    };
    let query = url
        .query_pairs()
        .map(|(name, value)| {
            json!({
                "name": name,
                "value": if sensitive_extension_key(&name) {
                    "<redacted>".into()
                } else {
                    value.into_owned()
                }
            })
        })
        .collect::<Vec<_>>();
    json!({
        "scheme": url.scheme(),
        "host": url.host_str().unwrap_or_default(),
        "port": url.port(),
        "path": url.path(),
        "query": query,
    })
}

fn import_openapi_examples(
    document: &Value,
    operation: &Value,
    request: &SavedRequest,
    output: &mut Vec<Example>,
    warnings: &mut Vec<String>,
) {
    let Some(responses) = operation.get("responses").and_then(Value::as_object) else {
        return;
    };
    for (status_key, raw_response) in responses {
        let response = resolve_openapi_value(document, raw_response);
        let status = status_key.parse::<u16>().unwrap_or_default();
        let reason = response
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let headers = response
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|headers| headers.iter())
            .map(|(name, raw_header)| {
                let header = resolve_openapi_value(document, raw_header);
                let value = header
                    .get("example")
                    .or_else(|| header.get("default"))
                    .or_else(|| header.pointer("/schema/example"))
                    .or_else(|| header.pointer("/schema/default"))
                    .map(json_scalar)
                    .unwrap_or_default();
                (name.clone(), value)
            })
            .collect::<Vec<_>>();
        let mut media_examples = Vec::new();
        if let Some(content) = response.get("content").and_then(Value::as_object) {
            for (content_type, media) in content {
                if let Some(named) = media.get("examples").and_then(Value::as_object) {
                    for (example_name, raw_example) in named {
                        let example = resolve_openapi_value(document, raw_example);
                        media_examples.push((
                            format!("{status_key} {content_type} — {example_name}"),
                            example.get("value").cloned().unwrap_or(example),
                            Some(content_type.as_str()),
                        ));
                    }
                } else {
                    media_examples.push((
                        format!("{status_key} {content_type}"),
                        openapi_media_example(document, media),
                        Some(content_type.as_str()),
                    ));
                }
            }
        } else if let Some(swagger_examples) = response.get("examples").and_then(Value::as_object) {
            for (content_type, example) in swagger_examples {
                media_examples.push((
                    format!("{status_key} {content_type}"),
                    example.clone(),
                    Some(content_type.as_str()),
                ));
            }
        }
        if media_examples.is_empty() && response.get("schema").is_some() {
            media_examples.push((
                status_key.clone(),
                openapi_schema_example(document, response.get("schema")),
                None,
            ));
        }
        for (name, body, content_type) in media_examples {
            let mut example_headers = headers.clone();
            if let Some(content_type) = content_type {
                example_headers.push(("Content-Type".into(), content_type.into()));
            }
            let body = if let Some(body) = body.as_str() {
                body.to_string()
            } else {
                serde_json::to_string_pretty(&body).unwrap_or_default()
            };
            output.push(Example {
                id: ExampleId::new(),
                request_id: request.id.clone(),
                name,
                request: None,
                response: ResponseSnapshot {
                    status,
                    reason: reason.into(),
                    headers: example_headers,
                    body_base64: base64::engine::general_purpose::STANDARD.encode(body),
                    ..ResponseSnapshot::default()
                },
                extensions: extension_map(
                    &response,
                    &["description", "headers", "content", "examples", "schema"],
                ),
                sort_key: i64::try_from(output.len()).unwrap_or(i64::MAX),
            });
        }
    }
    if output.len() > MAX_IMPORTED_REQUESTS {
        warnings.push(format!(
            "OpenAPI examples were limited by the {MAX_IMPORTED_REQUESTS}-item import budget"
        ));
        output.truncate(MAX_IMPORTED_REQUESTS);
    }
}

fn import_postman(workspace: &WorkspaceId, value: Value) -> Result<ImportResult, String> {
    let name = value
        .pointer("/info/name")
        .and_then(Value::as_str)
        .unwrap_or("Imported Postman collection");
    let description = value
        .pointer("/info/description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut collection = new_collection(workspace, name, description);
    let mut requests = Vec::new();
    let mut folders = Vec::new();
    let mut examples = Vec::new();
    let mut warnings = Vec::new();
    collection.auth = import_postman_auth(value.get("auth"), &mut warnings, "collection");
    collection.scripts = import_postman_scripts(value.get("event"));
    collection.variables = import_postman_variables(value.get("variable"), &mut warnings);
    collection.extensions = extension_map(&value, &["info", "item", "auth", "event", "variable"]);
    if let Some(info) = value.get("info") {
        let info_extensions =
            extension_map(info, &["name", "description", "schema", "_postman_id"]);
        if !info_extensions.is_empty() {
            collection
                .extensions
                .insert("postman_info".into(), Value::Object(info_extensions));
        }
    }
    let items = value
        .get("item")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    postman_items(
        items,
        &collection.id,
        None,
        &mut folders,
        &mut requests,
        &mut examples,
        &mut warnings,
    )?;
    Ok(ImportResult {
        format: ImportFormat::PostmanCollection,
        origin: None,
        collection,
        folders,
        requests,
        environments: Vec::new(),
        examples,
        warnings,
    })
}

fn postman_items(
    items: &[Value],
    collection_id: &CollectionId,
    parent_id: Option<&FolderId>,
    folders: &mut Vec<Folder>,
    requests: &mut Vec<SavedRequest>,
    examples: &mut Vec<Example>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    for (sort_key, item) in items.iter().enumerate() {
        if let Some(children) = item.get("item").and_then(Value::as_array) {
            let folder_id = FolderId::new();
            let folder_name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Imported folder");
            folders.push(Folder {
                id: folder_id.clone(),
                collection_id: collection_id.clone(),
                parent_id: parent_id.cloned(),
                name: folder_name.into(),
                auth: import_postman_auth(item.get("auth"), warnings, folder_name),
                variables: import_postman_variables(item.get("variable"), warnings),
                scripts: import_postman_scripts(item.get("event")),
                sort_key: i64::try_from(sort_key).unwrap_or(i64::MAX),
                extensions: extension_map(item, &["name", "item", "auth", "event", "variable"]),
            });
            postman_items(
                children,
                collection_id,
                Some(&folder_id),
                folders,
                requests,
                examples,
                warnings,
            )?;
            continue;
        }
        let Some(request_value) = item.get("request") else {
            warnings.push("ignored a Postman item without a request".into());
            continue;
        };
        let method = request_value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET");
        let url = match request_value.get("url") {
            Some(Value::String(url)) => url.clone(),
            Some(Value::Object(url)) => url
                .get("raw")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Imported request");
        // Authentication values are never imported as reusable plaintext, but
        // collections often duplicate them in URLs, bodies, or custom headers.
        // Capture those literals before converting auth to opaque references
        // so the typed request and examples can be scrubbed as one unit.
        let imported_auth_secrets = postman_auth_secret_literals(request_value.get("auth"));
        let mut request = empty_request(collection_id, name, method, url)?;
        request.folder_id = parent_id.cloned();
        request.sort_key = i64::try_from(sort_key).unwrap_or(i64::MAX);
        request.auth = import_postman_auth(request_value.get("auth"), warnings, name);
        request.scripts = import_postman_scripts(item.get("event"));
        request.variables = import_postman_variables(item.get("variable"), warnings);
        request.extensions =
            extension_map(item, &["name", "request", "response", "event", "variable"]);
        let request_extensions = extension_map(
            request_value,
            &["method", "header", "body", "url", "auth", "description"],
        );
        if !request_extensions.is_empty() {
            request
                .extensions
                .insert("postman_request".into(), Value::Object(request_extensions));
        }
        if let Some(Value::Object(url)) = request_value.get("url") {
            request.params = import_rows(url.get("query"));
            // Postman writes the same query both into `raw` and the structured
            // `query` rows. The Workbench compiler appends enabled rows, so the
            // saved URL must contain only the base and fragment when those rows
            // are present.
            if !request.params.is_empty() {
                request.url = without_query(&request.url);
            }
            let nested = extension_map(&Value::Object(url.clone()), &["raw", "query"]);
            if !nested.is_empty() {
                request
                    .extensions
                    .insert("postman_url".into(), Value::Object(nested));
            }
        }
        for header in request_value
            .get("header")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(key) = header.get("key").and_then(Value::as_str) {
                let mut row = KeyValueRow::enabled(
                    key,
                    header
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                row.enabled = !header
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                request.headers.push(row);
            }
        }
        if let Some(body) = request_value.get("body") {
            let nested = extension_map(
                body,
                &[
                    "mode",
                    "raw",
                    "urlencoded",
                    "formdata",
                    "file",
                    "graphql",
                    "options",
                ],
            );
            if !nested.is_empty() {
                request
                    .extensions
                    .insert("postman_body".into(), Value::Object(nested));
            }
            request.body = match body.get("mode").and_then(Value::as_str) {
                Some("raw") => Body::Raw {
                    media_type: match body
                        .pointer("/options/raw/language")
                        .and_then(Value::as_str)
                    {
                        Some("xml") => RawBodyKind::Xml,
                        Some("text") | Some("html") | Some("javascript") => RawBodyKind::Text,
                        _ => RawBodyKind::Json,
                    },
                    text: body
                        .get("raw")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                },
                Some("urlencoded") => Body::UrlEncoded {
                    rows: import_rows(body.get("urlencoded")),
                },
                Some("formdata") => Body::Multipart {
                    rows: import_multipart_rows(body.get("formdata")),
                },
                Some("file") => Body::Binary {
                    path: body
                        .pointer("/file/src")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                },
                Some("graphql") => Body::GraphQl {
                    query: body
                        .pointer("/graphql/query")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    variables: body
                        .pointer("/graphql/variables")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .into(),
                },
                Some(other) => {
                    warnings.push(format!(
                        "request {name:?} has unsupported body mode {other:?}; its sanitized Postman body was retained"
                    ));
                    request.extensions.insert(
                        "postman_unsupported_body".into(),
                        sanitize_import_value(body.clone()),
                    );
                    Body::None
                }
                None => Body::None,
            };
        }
        redact_saved_request_fields(&mut request, &imported_auth_secrets);
        import_postman_examples(
            item.get("response"),
            &request,
            &imported_auth_secrets,
            examples,
            warnings,
        );
        requests.push(request);
        if requests.len() > MAX_IMPORTED_REQUESTS {
            return Err(format!(
                "import contains more than {MAX_IMPORTED_REQUESTS} requests"
            ));
        }
    }
    Ok(())
}

fn import_postman_auth(
    value: Option<&Value>,
    warnings: &mut Vec<String>,
    owner: &str,
) -> AuthConfig {
    let Some(value) = value else {
        return AuthConfig::Inherit;
    };
    if value.is_null() {
        return AuthConfig::None;
    }
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let field = |section: &str, key: &str| {
        value
            .get(section)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|entry| entry.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|entry| entry.get("value"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let secret = |label: &str| SecretRef::generated(&format!("imported-{label}"));
    match kind {
        "noauth" => AuthConfig::None,
        "apikey" => AuthConfig::ApiKey {
            name: field("apikey", "key"),
            value: secret("api-key"),
            location: if field("apikey", "in").eq_ignore_ascii_case("query") {
                ApiKeyLocation::Query
            } else {
                ApiKeyLocation::Header
            },
        },
        "basic" => AuthConfig::Basic {
            username: field("basic", "username"),
            password: secret("basic-password"),
        },
        "bearer" => AuthConfig::Bearer {
            token: secret("bearer-token"),
        },
        "oauth2" => AuthConfig::OAuth2 {
            token: secret("oauth-token"),
        },
        "awsv4" => AuthConfig::AwsSigV4 {
            access_key: secret("aws-access-key"),
            secret_key: secret("aws-secret-key"),
            session_token: (!field("awsv4", "sessionToken").is_empty())
                .then(|| secret("aws-session-token")),
            region: field("awsv4", "region"),
            service: field("awsv4", "service"),
        },
        other => {
            warnings.push(format!(
                "{owner:?} uses unsupported Postman authentication {other:?}"
            ));
            AuthConfig::Unsupported {
                name: other.into(),
                raw: scrub_postman_auth(value.clone()),
            }
        }
    }
}

fn postman_auth_secret_literals(value: Option<&Value>) -> Vec<String> {
    const SECRET_KEYS: &[&str] = &[
        "value",
        "password",
        "token",
        "accesstoken",
        "refreshtoken",
        "clientsecret",
        "accesskey",
        "secretkey",
        "sessiontoken",
    ];
    let Some(Value::Object(auth)) = value else {
        return Vec::new();
    };
    let mut secrets = Vec::new();
    for entries in auth.values().filter_map(Value::as_array) {
        for entry in entries.iter().filter_map(Value::as_object) {
            let key = entry
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>();
            if SECRET_KEYS.contains(&key.as_str())
                && let Some(secret) = entry
                    .get("value")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            {
                push_credential(&mut secrets, secret);
            }
        }
    }
    if let Some(credential) = postman_basic_auth_credential(auth) {
        push_credential(&mut secrets, &credential);
    }
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets
}

fn postman_basic_auth_credential(auth: &serde_json::Map<String, Value>) -> Option<String> {
    if auth.get("type").and_then(Value::as_str) != Some("basic") {
        return None;
    }
    let field = |key: &str| {
        auth.get("basic")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|entry| entry.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|entry| entry.get("value"))
            .and_then(Value::as_str)
    };
    let username = field("username").unwrap_or_default();
    let password = field("password").unwrap_or_default();
    Some(base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}")))
}

fn import_postman_scripts(value: Option<&Value>) -> Scripts {
    let mut scripts = Scripts::default();
    for event in value.and_then(Value::as_array).into_iter().flatten() {
        let source = event
            .pointer("/script/exec")
            .map(|value| match value {
                Value::Array(lines) => lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                Value::String(source) => source.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        match event.get("listen").and_then(Value::as_str) {
            Some("prerequest") => scripts.pre_request = source,
            Some("test") => scripts.tests = source,
            _ => {}
        }
    }
    scripts
}

fn import_postman_variables(value: Option<&Value>, warnings: &mut Vec<String>) -> Vec<Variable> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let key = entry.get("key")?.as_str()?.to_string();
            let is_secret = entry.get("type").and_then(Value::as_str) == Some("secret");
            if is_secret {
                warnings.push(format!(
                    "secret variable {key:?} was omitted and must be entered again"
                ));
            }
            Some(Variable {
                id: super::RowId::new(),
                key,
                value: if is_secret {
                    VariableValue::MissingSecret(
                        SecretRef::new(format!("imported-variable-{}", uuid::Uuid::new_v4()))
                            .ok()?,
                    )
                } else {
                    VariableValue::Plain(entry.get("value").map(json_scalar).unwrap_or_default())
                },
                enabled: !entry
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            })
        })
        .collect()
}

fn import_postman_examples(
    value: Option<&Value>,
    request: &SavedRequest,
    redactions: &[String],
    output: &mut Vec<Example>,
    warnings: &mut Vec<String>,
) {
    for (sort_key, response) in value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let headers = response
            .get("header")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|header| {
                Some((
                    header.get("key")?.as_str()?.to_string(),
                    header
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ))
            })
            .collect();
        let body = response
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let original_request = response
            .get("originalRequest")
            .and_then(|value| postman_original_request_snapshot(value, redactions));
        let mut extensions = extension_map(
            response,
            &[
                "name",
                "code",
                "status",
                "header",
                "body",
                "originalRequest",
                "cookie",
            ],
        );
        if let Some(original) = response.get("originalRequest") {
            extensions.insert(
                "postman_original_request".into(),
                sanitize_import_value(original.clone()),
            );
            warnings.push(format!(
                "example {:?} retained a sanitized Postman originalRequest",
                response
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Example")
            ));
        }
        if let Some(cookies) = response.get("cookie") {
            extensions.insert(
                "postman_cookies".into(),
                sanitize_cookie_value(cookies.clone()),
            );
            warnings.push(format!(
                "example {:?} retained Postman cookies with values redacted",
                response
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Example")
            ));
        }
        let example = Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: response
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Example")
                .into(),
            request: original_request,
            response: ResponseSnapshot {
                status: response
                    .get("code")
                    .and_then(Value::as_u64)
                    .and_then(|value| u16::try_from(value).ok())
                    .unwrap_or(0),
                reason: response
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                headers,
                body_base64: base64::engine::general_purpose::STANDARD.encode(body.as_bytes()),
                duration_ms: 0,
                truncated: false,
                ..ResponseSnapshot::default()
            },
            extensions,
            sort_key: i64::try_from(sort_key).unwrap_or(i64::MAX),
        };
        output.push(redact_example(&example, redactions));
    }
}

fn import_postman_environment(
    workspace: &WorkspaceId,
    value: Value,
) -> Result<ImportResult, String> {
    let collection = new_collection(workspace, "Imported environment", "");
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Environment");
    let variables = value
        .get("values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let key = entry.get("key")?.as_str()?.to_string();
            let value = entry
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let secret = entry.get("type").and_then(Value::as_str) == Some("secret");
            Some(super::Variable {
                id: super::RowId::new(),
                key,
                value: if secret {
                    super::VariableValue::MissingSecret(
                        super::SecretRef::new(format!("imported-{}", uuid::Uuid::new_v4())).ok()?,
                    )
                } else {
                    super::VariableValue::Plain(value.into())
                },
                enabled: entry
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                description: String::new(),
            })
        })
        .collect::<Vec<super::Variable>>();
    // Postman keeps the host in a variable by convention; lift it into the
    // Base URL (the variable stays, so `{{baseUrl}}` URLs keep working).
    let base_url = variables
        .iter()
        .filter(|variable| variable.enabled)
        .find(|variable| {
            super::compile::BASE_URL_VARIABLES.contains(&variable.key.as_str())
                || variable.key == "host"
        })
        .and_then(|variable| match &variable.value {
            super::VariableValue::Plain(value)
                if !super::compile::is_relative_url(value.trim()) && !value.trim().is_empty() =>
            {
                Some(value.trim().to_string())
            }
            _ => None,
        })
        .unwrap_or_default();
    Ok(ImportResult {
        format: ImportFormat::PostmanEnvironment,
        origin: None,
        collection,
        folders: Vec::new(),
        requests: Vec::new(),
        environments: vec![Environment {
            id: super::EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: name.into(),
            base_url,
            auth: Default::default(),
            variables,
            active: false,
            extensions: extension_map(
                &value,
                &[
                    "name",
                    "values",
                    "_postman_variable_scope",
                    "_postman_exported_at",
                    "_postman_exported_using",
                ],
            ),
        }],
        examples: Vec::new(),
        warnings: vec!["secret environment values were omitted and must be entered again".into()],
    })
}

fn import_insomnia(workspace: &WorkspaceId, value: Value) -> Result<ImportResult, String> {
    let resources = value
        .get("resources")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let workspace_resource = resources
        .iter()
        .find(|entry| entry.get("_type").and_then(Value::as_str) == Some("workspace"));
    let name = workspace_resource
        .and_then(|entry| entry.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("Imported Insomnia collection");
    let description = workspace_resource
        .and_then(|entry| entry.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut warnings = Vec::new();
    let mut collection = new_collection(workspace, name, description);
    if let Some(resource) = workspace_resource {
        collection.extensions = extension_map(
            resource,
            &["_id", "_type", "parentId", "name", "description", "scope"],
        );
        if resource.get("authentication").is_some() {
            collection.auth =
                import_insomnia_auth(resource.get("authentication"), &mut warnings, name);
            if let Some(options) = insomnia_auth_options(resource.get("authentication")) {
                collection
                    .extensions
                    .insert("insomnia_security_options".into(), options);
            }
        }
        collection.scripts = import_insomnia_scripts(resource);
    }
    let export_extensions = extension_map(&value, &["_type", "resources"]);
    if !export_extensions.is_empty() {
        collection
            .extensions
            .insert("insomnia_export".into(), Value::Object(export_extensions));
    }
    let mut requests = Vec::new();
    let mut folders = Vec::new();
    let mut environments = Vec::new();
    let mut examples = Vec::new();
    let workspace_source_id = workspace_resource
        .and_then(|resource| resource.get("_id"))
        .and_then(Value::as_str);
    let folder_ids: BTreeMap<String, FolderId> = resources
        .iter()
        .filter(|resource| {
            matches!(
                resource.get("_type").and_then(Value::as_str),
                Some("request_group" | "folder")
            )
        })
        .filter_map(|resource| Some((resource.get("_id")?.as_str()?.to_string(), FolderId::new())))
        .collect();
    for (sort_key, resource) in resources.iter().enumerate() {
        if !matches!(
            resource.get("_type").and_then(Value::as_str),
            Some("request_group" | "folder")
        ) {
            continue;
        }
        let Some(source_id) = resource.get("_id").and_then(Value::as_str) else {
            warnings.push("ignored an Insomnia folder without an id".into());
            continue;
        };
        let Some(id) = folder_ids.get(source_id).cloned() else {
            continue;
        };
        let parent_id = resource
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|parent| folder_ids.get(parent))
            .cloned();
        let mut extensions = extension_map(
            resource,
            &[
                "_id",
                "_type",
                "parentId",
                "name",
                "authentication",
                "scripts",
                "preRequestScript",
                "afterResponseScript",
                "metaSortKey",
            ],
        );
        if let Some(options) = insomnia_auth_options(resource.get("authentication")) {
            extensions.insert("insomnia_security_options".into(), options);
        }
        folders.push(Folder {
            id,
            collection_id: collection.id.clone(),
            parent_id,
            name: resource
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Imported folder")
                .into(),
            auth: import_insomnia_auth(
                resource.get("authentication"),
                &mut warnings,
                resource
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("folder"),
            ),
            variables: Vec::new(),
            scripts: import_insomnia_scripts(resource),
            sort_key: resource
                .get("metaSortKey")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| i64::try_from(sort_key).unwrap_or(i64::MAX)),
            extensions,
        });
    }
    let mut request_by_source_id = BTreeMap::new();
    for resource in resources {
        match resource.get("_type").and_then(Value::as_str) {
            Some("environment") => {
                environments.push(import_insomnia_environment(
                    workspace,
                    resource,
                    environments.len(),
                    &mut warnings,
                ));
                continue;
            }
            Some("request") => {}
            _ => continue,
        }
        if requests.len() >= MAX_IMPORTED_REQUESTS {
            return Err(format!(
                "import contains more than {MAX_IMPORTED_REQUESTS} requests"
            ));
        }
        let parent = resource.get("parentId").and_then(Value::as_str);
        if parent.is_some()
            && parent != workspace_source_id
            && !parent.is_some_and(|parent| folder_ids.contains_key(parent))
        {
            warnings.push(format!(
                "request {:?} references an unknown Insomnia parent",
                resource
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Imported request")
            ));
        }
        let method = resource
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET");
        let url = resource
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = resource
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Imported request");
        let auth_literals = insomnia_auth_secret_literals(resource.get("authentication"));
        let mut request = empty_request(&collection.id, name, method, url)?;
        request.folder_id = parent.and_then(|parent| folder_ids.get(parent)).cloned();
        request.sort_key = resource
            .get("metaSortKey")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| i64::try_from(requests.len()).unwrap_or(i64::MAX));
        request.auth = import_insomnia_auth(resource.get("authentication"), &mut warnings, name);
        request.scripts = import_insomnia_scripts(resource);
        request.params = import_insomnia_rows(resource.get("parameters"));
        if !request.params.is_empty() {
            request.url = without_query(&request.url);
        }
        request.headers = import_insomnia_rows(resource.get("headers"));
        request.body = import_insomnia_body(resource.get("body"), name, &mut warnings);
        request.extensions = extension_map(
            resource,
            &[
                "_id",
                "_type",
                "parentId",
                "name",
                "method",
                "url",
                "parameters",
                "headers",
                "authentication",
                "body",
                "scripts",
                "preRequestScript",
                "afterResponseScript",
                "metaSortKey",
            ],
        );
        if let Some(options) = insomnia_auth_options(resource.get("authentication")) {
            request
                .extensions
                .insert("insomnia_security_options".into(), options);
        }
        if let Some(body) = resource.get("body") {
            let body_extensions = extension_map(body, &["mimeType", "text", "params", "fileName"]);
            if !body_extensions.is_empty() {
                request
                    .extensions
                    .insert("insomnia_body".into(), Value::Object(body_extensions));
            }
            if body.get("params").is_some() {
                request.extensions.insert(
                    "insomnia_body_source".into(),
                    sanitize_import_value(body.clone()),
                );
            }
        }
        redact_saved_request_fields(&mut request, &auth_literals);
        if let Some(source_id) = resource.get("_id").and_then(Value::as_str) {
            request_by_source_id.insert(source_id.to_string(), request.id.clone());
        }
        requests.push(request);
    }
    for resource in resources {
        if !matches!(
            resource.get("_type").and_then(Value::as_str),
            Some("response" | "response_example")
        ) {
            continue;
        }
        let Some(request_id) = resource
            .get("parentId")
            .or_else(|| resource.get("requestId"))
            .and_then(Value::as_str)
            .and_then(|parent| request_by_source_id.get(parent))
            .cloned()
        else {
            warnings.push("ignored an Insomnia response without a matching request".into());
            continue;
        };
        examples.push(import_insomnia_example(
            resource,
            request_id,
            examples.len(),
        ));
    }
    if requests.is_empty() {
        warnings.push("Insomnia export contains no requests".into());
    }
    Ok(ImportResult {
        format: ImportFormat::Insomnia,
        origin: None,
        collection,
        folders,
        requests,
        environments,
        examples,
        warnings,
    })
}

fn import_insomnia_rows(value: Option<&Value>) -> Vec<KeyValueRow> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let key = entry.get("name").or_else(|| entry.get("key"))?.as_str()?;
            let mut row =
                KeyValueRow::enabled(key, entry.get("value").map(json_scalar).unwrap_or_default());
            row.enabled = !entry
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            row.description = entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into();
            Some(row)
        })
        .collect()
}

fn import_insomnia_body(value: Option<&Value>, owner: &str, warnings: &mut Vec<String>) -> Body {
    let Some(body) = value.filter(|body| !body.is_null()) else {
        return Body::None;
    };
    let mime = body
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if mime.starts_with("multipart/") {
        let rows = body
            .get("params")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let key = entry.get("name").or_else(|| entry.get("key"))?.as_str()?;
                let is_file = entry.get("type").and_then(Value::as_str) == Some("file")
                    || entry.get("fileName").is_some();
                let mut row = if is_file {
                    MultipartRow::file(
                        key,
                        entry
                            .get("fileName")
                            .or_else(|| entry.get("value"))
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                } else {
                    MultipartRow::text(key, entry.get("value").map(json_scalar).unwrap_or_default())
                };
                row.enabled = !entry
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                row.description = entry
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into();
                Some(row)
            })
            .collect();
        return Body::Multipart { rows };
    }
    if mime == "application/x-www-form-urlencoded" {
        return Body::UrlEncoded {
            rows: import_insomnia_rows(body.get("params")),
        };
    }
    if let Some(path) = body.get("fileName").and_then(Value::as_str) {
        return Body::Binary { path: path.into() };
    }
    if let Some(text) = body.get("text").and_then(Value::as_str) {
        if mime == "application/graphql" {
            return Body::GraphQl {
                query: text.into(),
                variables: "{}".into(),
            };
        }
        return Body::Raw {
            media_type: if mime.contains("json") {
                RawBodyKind::Json
            } else if mime.contains("xml") {
                RawBodyKind::Xml
            } else {
                RawBodyKind::Text
            },
            text: text.into(),
        };
    }
    if body.get("params").is_some() {
        warnings.push(format!(
            "request {owner:?} has Insomnia body parameters without a supported MIME type; the sanitized body was retained as an extension"
        ));
    }
    Body::None
}

fn import_insomnia_scripts(resource: &Value) -> Scripts {
    fn script_text(value: Option<&Value>) -> String {
        match value {
            Some(Value::String(source)) => source.clone(),
            Some(Value::Array(lines)) => lines
                .iter()
                .filter_map(|line| {
                    line.as_str().or_else(|| {
                        line.get("source")
                            .or_else(|| line.get("code"))
                            .and_then(Value::as_str)
                    })
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Some(Value::Object(script)) => script
                .get("source")
                .or_else(|| script.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            _ => String::new(),
        }
    }
    Scripts {
        pre_request: script_text(
            resource
                .get("preRequestScript")
                .or_else(|| resource.pointer("/scripts/preRequest")),
        ),
        tests: script_text(
            resource
                .get("afterResponseScript")
                .or_else(|| resource.pointer("/scripts/afterResponse")),
        ),
    }
}

fn insomnia_auth_secret_literals(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut secrets = Vec::new();
    if let Value::Object(fields) = value {
        for (key, value) in fields {
            if (sensitive_credential_field(key) || key.eq_ignore_ascii_case("value"))
                && let Some(secret) = value.as_str()
            {
                push_credential(&mut secrets, secret);
            }
        }
    }
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    secrets
}

fn insomnia_auth_options(value: Option<&Value>) -> Option<Value> {
    let options = extension_map(
        value?,
        &[
            "type", "token", "password", "value", "username", "key", "name", "addTo", "in",
        ],
    );
    (!options.is_empty()).then_some(Value::Object(options))
}

fn import_insomnia_auth(
    value: Option<&Value>,
    warnings: &mut Vec<String>,
    owner: &str,
) -> AuthConfig {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return AuthConfig::Inherit;
    };
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_ascii_lowercase();
    let secret = |label: &str| SecretRef::generated(&format!("imported-{label}"));
    match kind.as_str() {
        "none" | "noauth" => AuthConfig::None,
        "bearer" => AuthConfig::Bearer {
            token: secret("insomnia-bearer-token"),
        },
        "basic" => AuthConfig::Basic {
            username: value
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            password: secret("insomnia-basic-password"),
        },
        "apikey" | "api_key" => AuthConfig::ApiKey {
            name: value
                .get("key")
                .or_else(|| value.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("X-API-Key")
                .into(),
            value: secret("insomnia-api-key"),
            location: if value
                .get("addTo")
                .or_else(|| value.get("in"))
                .and_then(Value::as_str)
                .is_some_and(|location| location.eq_ignore_ascii_case("query"))
            {
                ApiKeyLocation::Query
            } else {
                ApiKeyLocation::Header
            },
        },
        "oauth2" => AuthConfig::OAuth2 {
            token: secret("insomnia-oauth-token"),
        },
        _ => {
            warnings.push(format!(
                "{owner:?} uses unsupported Insomnia authentication {kind:?}; its sanitized definition was retained"
            ));
            AuthConfig::Unsupported {
                name: kind,
                raw: sanitize_import_value(value.clone()),
            }
        }
    }
}

fn import_insomnia_environment(
    workspace: &WorkspaceId,
    resource: &Value,
    sort_key: usize,
    warnings: &mut Vec<String>,
) -> Environment {
    let mut variables = Vec::new();
    if let Some(data) = resource.get("data").and_then(Value::as_object) {
        for (key, value) in data {
            let secret = sensitive_extension_key(key);
            if secret {
                warnings.push(format!(
                    "secret Insomnia environment variable {key:?} was omitted and must be entered again"
                ));
            }
            variables.push(Variable {
                id: super::RowId::new(),
                key: key.clone(),
                value: if secret {
                    VariableValue::MissingSecret(SecretRef::generated("imported-insomnia"))
                } else {
                    VariableValue::Plain(json_scalar(value))
                },
                enabled: true,
                description: String::new(),
            });
        }
    }
    let mut extensions = extension_map(
        resource,
        &["_id", "_type", "parentId", "name", "data", "metaSortKey"],
    );
    if let Some(parent) = resource.get("parentId") {
        extensions.insert("insomnia_parent_id".into(), parent.clone());
    }
    Environment {
        id: EnvironmentId::new(),
        workspace_id: workspace.clone(),
        name: resource
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Environment")
            .into(),
        base_url: String::new(),
        auth: Default::default(),
        variables,
        active: sort_key == 0,
        extensions,
    }
}

fn import_insomnia_example(resource: &Value, request_id: RequestId, sort_key: usize) -> Example {
    let body = resource
        .get("body")
        .and_then(|body| {
            body.as_str()
                .or_else(|| body.get("text").and_then(Value::as_str))
        })
        .unwrap_or_default();
    let headers = resource
        .get("headers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|header| {
            let name = header
                .get("name")
                .or_else(|| header.get("key"))?
                .as_str()?
                .to_string();
            let value = if sensitive_header(&name) {
                "<redacted>".into()
            } else {
                header.get("value").map(json_scalar).unwrap_or_default()
            };
            Some((name, value))
        })
        .collect();
    Example {
        id: ExampleId::new(),
        request_id,
        name: resource
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Example")
            .into(),
        request: None,
        response: ResponseSnapshot {
            status: resource
                .get("statusCode")
                .or_else(|| resource.get("status"))
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
                .unwrap_or_default(),
            reason: resource
                .get("statusMessage")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            headers,
            body_base64: base64::engine::general_purpose::STANDARD.encode(body),
            duration_ms: resource
                .get("elapsedTime")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            ..ResponseSnapshot::default()
        },
        extensions: extension_map(
            resource,
            &[
                "_id",
                "_type",
                "parentId",
                "requestId",
                "name",
                "statusCode",
                "status",
                "statusMessage",
                "headers",
                "body",
                "elapsedTime",
            ],
        ),
        sort_key: i64::try_from(sort_key).unwrap_or(i64::MAX),
    }
}

fn import_curl(workspace: &WorkspaceId, text: &str) -> Result<ImportResult, String> {
    let tokens = shell_words(text)?;
    let collection = new_collection(workspace, "Imported cURL", "");
    let mut method = "GET".to_string();
    let mut url = None;
    let mut headers = Vec::new();
    let mut body = None;
    let mut index = 1;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "-X" | "--request" => {
                index += 1;
                method = tokens
                    .get(index)
                    .ok_or("cURL --request needs a method")?
                    .clone();
            }
            "-H" | "--header" => {
                index += 1;
                let header = tokens.get(index).ok_or("cURL --header needs a value")?;
                let (name, value) = header
                    .split_once(':')
                    .ok_or("cURL header needs `Name: value`")?;
                headers.push(KeyValueRow::enabled(name.trim(), value.trim()));
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" => {
                index += 1;
                body = Some(
                    tokens
                        .get(index)
                        .ok_or("cURL data flag needs a value")?
                        .clone(),
                );
                if method == "GET" {
                    method = "POST".into();
                }
            }
            token if token.starts_with('-') => {
                return Err(format!("unsupported cURL option {token:?}"));
            }
            token => url = Some(token.to_string()),
        }
        index += 1;
    }
    let mut request = empty_request(
        &collection.id,
        "Imported cURL request",
        &method,
        url.ok_or("cURL command has no URL")?,
    )?;
    request.headers = headers;
    if let Some(body) = body {
        request.body = Body::Raw {
            media_type: RawBodyKind::Text,
            text: body,
        };
    }
    Ok(ImportResult {
        format: ImportFormat::Curl,
        origin: None,
        collection,
        folders: Vec::new(),
        requests: vec![request],
        environments: Vec::new(),
        examples: Vec::new(),
        warnings: Vec::new(),
    })
}

fn new_collection(workspace: &WorkspaceId, name: &str, description: &str) -> Collection {
    Collection {
        id: CollectionId::new(),
        workspace_id: workspace.clone(),
        name: name.into(),
        description: description.into(),
        auth: AuthConfig::None,
        variables: Vec::new(),
        scripts: Scripts::default(),
        extensions: Default::default(),
    }
}

fn empty_request(
    collection_id: &CollectionId,
    name: impl Into<String>,
    method: impl AsRef<str>,
    url: impl Into<String>,
) -> Result<SavedRequest, String> {
    Ok(SavedRequest {
        id: RequestId::new(),
        collection_id: collection_id.clone(),
        folder_id: None,
        name: name.into(),
        method: HttpMethod::new(method.as_ref())?,
        url: url.into(),
        params: Vec::new(),
        headers: Vec::new(),
        auth: AuthConfig::None,
        body: Body::None,
        variables: Vec::new(),
        scripts: Scripts::default(),
        settings: RequestSettings::default(),
        extensions: Default::default(),
        sort_key: 0,
    })
}

fn openapi_base_url(value: &Value) -> (String, Vec<Variable>) {
    if let Some(server) = value.pointer("/servers/0").and_then(Value::as_object)
        && let Some(url) = server.get("url").and_then(Value::as_str)
    {
        let variables = server
            .get("variables")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|variables| variables.iter())
            .map(|(name, definition)| Variable {
                id: RowId::new(),
                key: name.clone(),
                value: VariableValue::Plain(
                    definition
                        .get("default")
                        .or_else(|| definition.get("example"))
                        .or_else(|| {
                            definition
                                .get("enum")
                                .and_then(Value::as_array)
                                .and_then(|values| values.first())
                        })
                        .map(json_scalar)
                        .unwrap_or_default(),
                ),
                enabled: true,
                description: definition
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            })
            .collect();
        return (openapi_template(url.trim_end_matches('/')), variables);
    }
    let scheme = value
        .get("schemes")
        .and_then(Value::as_array)
        .and_then(|schemes| schemes.first())
        .and_then(Value::as_str)
        .unwrap_or("https");
    let host = value
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("example.test");
    let base = value
        .get("basePath")
        .and_then(Value::as_str)
        .unwrap_or_default();
    (
        openapi_template(&format!("{scheme}://{host}{}", base.trim_end_matches('/'))),
        Vec::new(),
    )
}

fn openapi_template(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(open) = remaining.find('{') {
        output.push_str(&remaining[..open]);
        let candidate = &remaining[open + 1..];
        let Some(close) = candidate.find('}') else {
            output.push_str(&remaining[open..]);
            return output;
        };
        let name = &candidate[..close];
        if !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            output.push_str("{{");
            output.push_str(name);
            output.push_str("}}");
        } else {
            output.push('{');
            output.push_str(name);
            output.push('}');
        }
        remaining = &candidate[close + 1..];
    }
    output.push_str(remaining);
    output
}

fn import_rows(value: Option<&Value>) -> Vec<KeyValueRow> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let key = entry.get("key")?.as_str()?;
            let value = entry
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut row = KeyValueRow::enabled(key, value);
            row.enabled = !entry
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(row)
        })
        .collect()
}

fn import_multipart_rows(value: Option<&Value>) -> Vec<MultipartRow> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let key = entry.get("key")?.as_str()?;
            let value = if entry.get("type").and_then(Value::as_str) == Some("file") {
                let path = entry
                    .get("src")
                    .and_then(|value| match value {
                        Value::Array(paths) => paths.first().and_then(Value::as_str),
                        Value::String(path) => Some(path.as_str()),
                        _ => None,
                    })
                    .unwrap_or_default();
                MultipartValue::File(path.into())
            } else {
                MultipartValue::Text(
                    entry
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                )
            };
            Some(MultipartRow {
                id: super::RowId::new(),
                key: key.into(),
                value,
                enabled: !entry
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            })
        })
        .collect()
}

fn postman_rows(rows: &[KeyValueRow]) -> Vec<Value> {
    rows.iter()
        .map(|row| json!({"key":row.key, "value":row.value, "disabled":!row.enabled}))
        .collect()
}

fn postman_multipart_rows(rows: &[MultipartRow]) -> Vec<Value> {
    rows.iter()
        .map(|row| match &row.value {
            MultipartValue::Text(value) => json!({
                "key": row.key,
                "value": value,
                "type": "text",
                "disabled": !row.enabled,
                "description": row.description,
            }),
            MultipartValue::File(path) => json!({
                "key": row.key,
                "src": path,
                "type": "file",
                "disabled": !row.enabled,
                "description": row.description,
            }),
        })
        .collect()
}

fn postman_items_for_parent(
    collection: &Collection,
    folders: &[Folder],
    requests: &[RedactedExportRequest],
    examples: &[Example],
    parent: Option<&FolderId>,
) -> Vec<Value> {
    let mut items = Vec::new();
    for folder in folders
        .iter()
        .filter(|folder| folder.parent_id.as_ref() == parent)
    {
        let mut object = folder.extensions.clone();
        object.insert("name".into(), Value::String(folder.name.clone()));
        object.insert(
            "item".into(),
            Value::Array(postman_items_for_parent(
                collection,
                folders,
                requests,
                examples,
                Some(&folder.id),
            )),
        );
        let events = postman_events(&folder.scripts);
        if !events.is_empty() {
            object.insert("event".into(), Value::Array(events));
        }
        if let Some(auth) = postman_auth(&folder.auth) {
            object.insert("auth".into(), auth);
        }
        if !folder.variables.is_empty() {
            object.insert(
                "variable".into(),
                Value::Array(postman_variables(&folder.variables)),
            );
        }
        items.push((
            folder.sort_key,
            folder.id.to_string(),
            Value::Object(object),
        ));
    }
    for export in requests.iter().filter(|request| {
        request.definition.collection_id == collection.id
            && request.definition.folder_id.as_ref() == parent
    }) {
        items.push((
            export.definition.sort_key,
            export.definition.id.to_string(),
            postman_request_item(export, examples),
        ));
    }
    items.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    items.into_iter().map(|(_, _, value)| value).collect()
}

fn postman_request_item(export: &RedactedExportRequest, examples: &[Example]) -> Value {
    let request = export.definition();
    let mut item = request.extensions.clone();
    let unsupported_body = item.remove("postman_unsupported_body");
    let body_extensions = match item.remove("postman_body") {
        Some(Value::Object(value)) => value,
        _ => serde_json::Map::new(),
    };
    let url_extensions = match item.remove("postman_url") {
        Some(Value::Object(value)) => value,
        _ => serde_json::Map::new(),
    };
    let headers: Vec<Value> = request
        .headers
        .iter()
        .map(|row| {
            json!({
                "key": row.key,
                "value": row.value,
                "disabled": !row.enabled,
                "description": row.description,
            })
        })
        .collect();
    let mut body = match &request.body {
        Body::None => unsupported_body.unwrap_or(Value::Null),
        Body::Raw { media_type, text } => json!({
            "mode":"raw",
            "raw":text,
            "options":{"raw":{"language": match media_type {
                RawBodyKind::Json => "json",
                RawBodyKind::Xml => "xml",
                RawBodyKind::Text => "text",
            }}}
        }),
        Body::UrlEncoded { rows } => {
            json!({"mode":"urlencoded", "urlencoded": postman_rows(rows)})
        }
        Body::Multipart { rows } => {
            json!({"mode":"formdata", "formdata": postman_multipart_rows(rows)})
        }
        Body::Binary { path } => json!({"mode":"file", "file":{"src":path}}),
        Body::GraphQl { query, variables } => {
            json!({"mode":"graphql", "graphql":{"query":query, "variables":variables}})
        }
    };
    if let Value::Object(body) = &mut body {
        for (key, value) in body_extensions {
            body.entry(key).or_insert(value);
        }
    }
    let responses = examples
        .iter()
        .filter(|example| example.request_id == request.id)
        .map(postman_example)
        .collect();
    let mut postman_request = match item.remove("postman_request") {
        Some(Value::Object(value)) => value,
        _ => serde_json::Map::new(),
    };
    postman_request.insert(
        "method".into(),
        Value::String(request.method.as_str().into()),
    );
    postman_request.insert("header".into(), Value::Array(headers));
    let mut url = url_extensions;
    url.insert("raw".into(), Value::String(request.url.clone()));
    url.insert("query".into(), Value::Array(postman_rows(&request.params)));
    postman_request.insert("url".into(), Value::Object(url));
    postman_request.insert("body".into(), body);
    if let Some(auth) = postman_auth(&request.auth) {
        postman_request.insert("auth".into(), auth);
    } else {
        postman_request.remove("auth");
    }
    item.insert("name".into(), Value::String(request.name.clone()));
    item.insert("request".into(), Value::Object(postman_request));
    item.insert("response".into(), Value::Array(responses));
    let events = postman_events(&request.scripts);
    if !events.is_empty() {
        item.insert("event".into(), Value::Array(events));
    }
    if !request.variables.is_empty() {
        item.insert(
            "variable".into(),
            Value::Array(postman_variables(&request.variables)),
        );
    }
    Value::Object(item)
}

fn postman_auth(auth: &AuthConfig) -> Option<Value> {
    let field = |key: &str, value: String| json!({"key":key, "value":value, "type":"string"});
    let secret = || "<redacted>".to_string();
    match auth {
        AuthConfig::Inherit => None,
        AuthConfig::None => Some(json!({"type":"noauth"})),
        AuthConfig::ApiKey { name, location, .. } => Some(json!({
            "type":"apikey",
            "apikey":[
                field("key", name.clone()),
                field("value", secret()),
                field("in", match location { ApiKeyLocation::Header => "header", ApiKeyLocation::Query => "query" }.into())
            ]
        })),
        AuthConfig::Basic { username, .. } => Some(json!({
            "type":"basic",
            "basic":[field("username", username.clone()), field("password", secret())]
        })),
        AuthConfig::Bearer { .. } => Some(json!({
            "type":"bearer", "bearer":[field("token", secret())]
        })),
        AuthConfig::OAuth2 { .. }
        | AuthConfig::OAuth2AuthorizationCodePkce { .. }
        | AuthConfig::OAuth2ClientCredentials { .. }
        | AuthConfig::OAuth2Password { .. } => Some(json!({
            "type":"oauth2", "oauth2":[field("accessToken", secret())]
        })),
        // Postman has no sign-in-first auth; export the header the cached
        // session produces.
        AuthConfig::Login { .. } => Some(json!({
            "type":"bearer", "bearer":[field("token", secret())]
        })),
        AuthConfig::AwsSigV4 {
            session_token,
            region,
            service,
            ..
        } => {
            let mut fields = vec![
                field("accessKey", secret()),
                field("secretKey", secret()),
                field("region", region.clone()),
                field("service", service.clone()),
            ];
            if session_token.is_some() {
                fields.push(field("sessionToken", secret()));
            }
            Some(json!({"type":"awsv4", "awsv4":fields}))
        }
        AuthConfig::Unsupported { raw, .. } => Some(raw.clone()),
    }
}

fn postman_variables(variables: &[Variable]) -> Vec<Value> {
    variables
        .iter()
        .map(|variable| {
            let (value, kind) = match &variable.value {
                VariableValue::Plain(value) => (value.clone(), "default"),
                VariableValue::Secret(_) | VariableValue::MissingSecret(_) => {
                    ("<redacted>".into(), "secret")
                }
            };
            json!({
                "key": variable.key,
                "value": value,
                "type": kind,
                "disabled": !variable.enabled,
                "description": variable.description,
            })
        })
        .collect()
}

fn postman_events(scripts: &Scripts) -> Vec<Value> {
    [
        ("prerequest", scripts.pre_request.as_str()),
        ("test", scripts.tests.as_str()),
    ]
    .into_iter()
    .filter(|(_, source)| !source.is_empty())
    .map(|(listen, source)| {
        json!({
            "listen": listen,
            "script": {"type":"text/javascript", "exec": source.lines().collect::<Vec<_>>()}
        })
    })
    .collect()
}

fn postman_example(example: &Example) -> Value {
    let body = base64::engine::general_purpose::STANDARD
        .decode(&example.response.body_base64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let mut value = example.extensions.clone();
    if let Some(original) = value.remove("postman_original_request") {
        value.insert("originalRequest".into(), original);
    } else if let Some(original) = &example.request {
        value.insert(
            "originalRequest".into(),
            json!({
                "method": original.method,
                "url": original.url,
                "header": original.headers.iter().map(|(key, value)| {
                    json!({"key": key, "value": value})
                }).collect::<Vec<_>>(),
                "body": {"mode": "raw", "raw": original.body},
            }),
        );
    }
    if let Some(cookies) = value.remove("postman_cookies") {
        value.insert("cookie".into(), cookies);
    }
    value.insert("name".into(), Value::String(example.name.clone()));
    value.insert("code".into(), json!(example.response.status));
    value.insert(
        "status".into(),
        Value::String(example.response.reason.clone()),
    );
    value.insert(
        "header".into(),
        Value::Array(
            example
                .response
                .headers
                .iter()
                .map(|(key, value)| json!({"key":key, "value":value}))
                .collect(),
        ),
    );
    value.insert("body".into(), Value::String(body));
    Value::Object(value)
}

fn postman_original_request_snapshot(
    value: &Value,
    redactions: &[String],
) -> Option<RedactedRequestSnapshot> {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("GET");
    let url = match value.get("url") {
        Some(Value::String(url)) => url.clone(),
        Some(Value::Object(url)) => url
            .get("raw")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        _ => String::new(),
    };
    let headers = value
        .get("header")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|header| {
            let name = header.get("key")?.as_str()?.to_string();
            let value = if sensitive_header(&name) {
                "<redacted>".into()
            } else {
                header
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            Some((name, value))
        })
        .collect();
    let body = value
        .pointer("/body/raw")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut snapshot = RedactedRequestSnapshot {
        method: method.into(),
        url,
        headers,
        body,
        ..RedactedRequestSnapshot::default()
    };
    snapshot.url = redact_text(&snapshot.url, redactions);
    snapshot.body = redact_text(&snapshot.body, redactions);
    for (name, value) in &mut snapshot.headers {
        *value = if sensitive_header(name) {
            "<redacted>".into()
        } else {
            redact_text(value, redactions)
        };
    }
    Some(snapshot)
}

fn sanitize_import_value(mut value: Value) -> Value {
    sanitize_named_secrets(&mut value, None, false);
    if let Some(auth) = value.get_mut("auth") {
        *auth = scrub_postman_auth(std::mem::take(auth));
    }
    if let Some(headers) = value.get_mut("header").and_then(Value::as_array_mut) {
        for header in headers {
            if let Some(object) = header.as_object_mut() {
                let sensitive = object
                    .get("key")
                    .and_then(Value::as_str)
                    .is_some_and(sensitive_header);
                if sensitive {
                    object.insert("value".into(), Value::String("<redacted>".into()));
                }
            }
        }
    }
    value
}

fn sanitize_cookie_value(mut value: Value) -> Value {
    sanitize_named_secrets(&mut value, None, true);
    value
}

fn sanitize_named_secrets(value: &mut Value, field: Option<&str>, redact_value_field: bool) {
    if field.is_some_and(|field| {
        let field = field.to_ascii_lowercase();
        (redact_value_field && field == "value")
            || matches!(
                field.as_str(),
                "token" | "password" | "secret" | "accesskey" | "secretkey" | "clientsecret"
            )
    }) && !value.is_null()
    {
        *value = Value::String("<redacted>".into());
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                sanitize_named_secrets(value, field, redact_value_field);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                sanitize_named_secrets(value, Some(key), redact_value_field);
            }
        }
        _ => {}
    }
}

fn sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "proxy-authorization" | "set-cookie"
    ) || sensitive_extension_key(name)
}

fn extension_map(value: &Value, known: &[&str]) -> serde_json::Map<String, Value> {
    let mut extensions: serde_json::Map<String, Value> = value
        .as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    sanitize_extensions(&mut extensions);
    extensions
}

fn sanitize_extensions(extensions: &mut serde_json::Map<String, Value>) {
    for (key, value) in extensions.iter_mut() {
        sanitize_extension_value(Some(key), value);
    }
}

#[derive(Default)]
struct ImportCredentialDiscovery {
    literals: Vec<String>,
    secret_template_variables: BTreeSet<String>,
}

const MAX_CREDENTIAL_TEMPLATE_VARIANTS: usize = 256;
const MAX_CREDENTIAL_TEMPLATE_RECURSION_DEPTH: usize = 64;

/// Finds literal credentials while the source document still has its native
/// authentication shape. Importers replace supported credentials with secret
/// references, so this pass must happen before conversion. Postman auth rows
/// are classified by their key: metadata such as Basic usernames and API-key
/// placement must not become document-wide redaction patterns.
fn collect_known_credential_literals(value: &Value) -> Result<ImportCredentialDiscovery, String> {
    fn visit(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, output);
                }
            }
            Value::Object(values) => {
                if let Some(credential) = postman_basic_auth_credential(values) {
                    push_credential(output, &credential);
                }
                // Postman uses {"key":"token", "value":"..."}; HAR uses
                // "name" in place of "key". The value is sensitive when its
                // companion key identifies an actual credential.
                let credential_row = values
                    .get("key")
                    .or_else(|| values.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(sensitive_credential_field)
                    || values.get("type").and_then(Value::as_str) == Some("secret")
                    || values.get("secret").and_then(Value::as_bool) == Some(true);
                if credential_row && let Some(secret) = values.get("value").and_then(Value::as_str)
                {
                    push_credential(output, secret);
                }
                for (key, value) in values {
                    if sensitive_credential_field(key)
                        && let Some(secret) = value.as_str()
                    {
                        push_credential(output, secret);
                    }
                    visit(value, output);
                }
            }
            _ => {}
        }
    }

    let mut discovery = ImportCredentialDiscovery::default();
    visit(value, &mut discovery.literals);
    collect_postman_templated_credentials(value, &mut discovery)?;
    discovery
        .literals
        .sort_by_key(|value| std::cmp::Reverse(value.len()));
    discovery.literals.dedup();
    Ok(discovery)
}

fn collect_postman_variable_values(value: Option<&Value>) -> BTreeMap<String, BTreeSet<String>> {
    let mut output = BTreeMap::new();
    for row in value.and_then(Value::as_array).into_iter().flatten() {
        let enabled = !row
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && row.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        if !enabled {
            continue;
        }
        let Some(key) = row.get("key").and_then(Value::as_str) else {
            continue;
        };
        let Some(value) = row.get("value") else {
            continue;
        };
        output
            .entry(key.to_string())
            .or_insert_with(BTreeSet::new)
            .insert(json_scalar(value));
    }
    output
}

fn collect_postman_templated_credentials(
    value: &Value,
    discovery: &mut ImportCredentialDiscovery,
) -> Result<(), String> {
    fn auth_field<'a>(
        auth: &'a serde_json::Map<String, Value>,
        section: &str,
        key: &str,
    ) -> Option<&'a str> {
        auth.get(section)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|entry| entry.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|entry| entry.get("value"))
            .and_then(Value::as_str)
    }

    fn credential_values(
        expression: &str,
        variables: &BTreeMap<String, BTreeSet<String>>,
        secret_variables: Option<&mut BTreeSet<String>>,
    ) -> Result<Vec<String>, String> {
        let mut referenced = BTreeSet::new();
        let mut stack = Vec::new();
        let values =
            expand_credential_template(expression, variables, &mut stack, &mut referenced, 0)?;
        if let Some(secret_variables) = secret_variables {
            secret_variables.extend(referenced);
        }
        Ok(values)
    }

    fn inspect_auth(
        auth: &serde_json::Map<String, Value>,
        variables: &BTreeMap<String, BTreeSet<String>>,
        discovery: &mut ImportCredentialDiscovery,
    ) -> Result<(), String> {
        let Some(kind) = auth.get("type").and_then(Value::as_str) else {
            return Ok(());
        };
        if kind == "basic" {
            let username = auth_field(auth, "basic", "username").unwrap_or_default();
            let password = auth_field(auth, "basic", "password").unwrap_or_default();
            let usernames = credential_values(username, variables, None)?;
            let passwords = credential_values(
                password,
                variables,
                Some(&mut discovery.secret_template_variables),
            )?;
            for password in &passwords {
                push_credential(&mut discovery.literals, password);
                for username in &usernames {
                    let encoded = base64::engine::general_purpose::STANDARD
                        .encode(format!("{username}:{password}"));
                    push_credential(&mut discovery.literals, &encoded);
                }
            }
            return Ok(());
        }

        for entries in auth.values().filter_map(Value::as_array) {
            for entry in entries.iter().filter_map(Value::as_object) {
                let key = entry.get("key").and_then(Value::as_str).unwrap_or_default();
                if !postman_auth_credential_field(kind, key) {
                    continue;
                }
                let Some(expression) = entry.get("value").and_then(Value::as_str) else {
                    continue;
                };
                for secret in credential_values(
                    expression,
                    variables,
                    Some(&mut discovery.secret_template_variables),
                )? {
                    push_credential(&mut discovery.literals, &secret);
                }
            }
        }
        Ok(())
    }

    fn visit(
        value: &Value,
        variables: &BTreeMap<String, BTreeSet<String>>,
        discovery: &mut ImportCredentialDiscovery,
    ) -> Result<(), String> {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, variables, discovery)?;
                }
            }
            Value::Object(values) => {
                // Postman variables are lexical: collection variables flow to
                // folders and requests, while an item's variables affect only
                // that item's subtree. Disabled rows do not participate in
                // template resolution. Keeping that provenance prevents a
                // disabled or sibling variable from falsely proving that a
                // credential template can be expanded safely.
                let local = collect_postman_variable_values(values.get("variable").or_else(|| {
                    values
                        .get("_postman_variable_scope")
                        .is_some()
                        .then(|| values.get("values"))
                        .flatten()
                }));
                let scoped;
                let variables = if local.is_empty() {
                    variables
                } else {
                    scoped = {
                        let mut scoped = variables.clone();
                        for (key, candidates) in local {
                            scoped.insert(key, candidates);
                        }
                        scoped
                    };
                    &scoped
                };
                inspect_auth(values, variables, discovery)?;
                for (key, value) in values {
                    if key != "variable" && key != "values" {
                        visit(value, variables, discovery)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    visit(value, &BTreeMap::new(), discovery)
}

fn expand_credential_template(
    expression: &str,
    variables: &BTreeMap<String, BTreeSet<String>>,
    stack: &mut Vec<String>,
    referenced: &mut BTreeSet<String>,
    recursion_depth: usize,
) -> Result<Vec<String>, String> {
    let Some(open) = expression.find("{{") else {
        if expression.contains("}}") {
            return Err("cannot safely import malformed credential template".into());
        }
        return Ok(vec![expression.to_string()]);
    };
    let after_open = &expression[open + 2..];
    let Some(close) = after_open.find("}}") else {
        return Err("cannot safely import malformed credential template".into());
    };
    let name = after_open[..close].trim();
    if recursion_depth >= MAX_CREDENTIAL_TEMPLATE_RECURSION_DEPTH {
        return Err(format!(
            "cannot safely import credential template: expansion nesting exceeds {MAX_CREDENTIAL_TEMPLATE_RECURSION_DEPTH} levels"
        ));
    }
    if name.is_empty() || stack.iter().any(|entry| entry == name) {
        return Err(format!(
            "cannot safely import cyclic or empty credential template {name:?}"
        ));
    }
    let Some(candidates) = variables.get(name) else {
        return Err(format!(
            "cannot safely import credential template {{{{{name}}}}}: its variable scope is unavailable"
        ));
    };
    referenced.insert(name.to_string());
    let mut output = Vec::new();
    for candidate in candidates {
        stack.push(name.to_string());
        let candidate_values = expand_credential_template(
            candidate,
            variables,
            stack,
            referenced,
            recursion_depth + 1,
        )?;
        stack.pop();
        for candidate in candidate_values {
            let replaced = format!(
                "{}{}{}",
                &expression[..open],
                candidate,
                &after_open[close + 2..]
            );
            output.extend(expand_credential_template(
                &replaced,
                variables,
                stack,
                referenced,
                recursion_depth + 1,
            )?);
            if output.len() > MAX_CREDENTIAL_TEMPLATE_VARIANTS {
                return Err(format!(
                    "credential template expands to more than {MAX_CREDENTIAL_TEMPLATE_VARIANTS} variants"
                ));
            }
        }
    }
    output.sort();
    output.dedup();
    Ok(output)
}

fn normalize_field(field: &str) -> String {
    field
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn sensitive_credential_field(field: &str) -> bool {
    matches!(
        normalize_field(field).as_str(),
        "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "password"
            | "passwd"
            | "secret"
            | "clientsecret"
            | "apikey"
            | "accesskey"
            | "secretkey"
            | "sessiontoken"
            | "privatekey"
    )
}

fn postman_auth_credential_field(kind: &str, field: &str) -> bool {
    let field = normalize_field(field);
    match kind {
        "apikey" => field == "value",
        "basic" | "digest" | "ntlm" => field == "password",
        "bearer" => field == "token",
        "oauth2" => matches!(
            field.as_str(),
            "accesstoken" | "refreshtoken" | "idtoken" | "clientsecret" | "password"
        ),
        "awsv4" => matches!(field.as_str(), "accesskey" | "secretkey" | "sessiontoken"),
        "edgegrid" => matches!(
            field.as_str(),
            "accesstoken" | "clienttoken" | "clientsecret"
        ),
        "hawk" => field == "authkey",
        "oauth1" => matches!(
            field.as_str(),
            "consumersecret" | "token" | "tokensecret" | "privatekey" | "verifier"
        ),
        "jwt" => matches!(field.as_str(), "secret" | "privatekey"),
        "asap" => field == "privatekey",
        _ => sensitive_credential_field(&field),
    }
}

fn push_credential(output: &mut Vec<String>, secret: &str) {
    let secret = secret.trim();
    if !secret.is_empty() && secret != "<redacted>" && !secret.contains("{{") {
        output.extend(secret_redaction_variants(secret));
    }
}

fn collect_curl_credential_literals(requests: &[SavedRequest]) -> Vec<String> {
    let mut output = Vec::new();
    for request in requests {
        for header in &request.headers {
            if header.key.eq_ignore_ascii_case("authorization")
                || header.key.eq_ignore_ascii_case("proxy-authorization")
            {
                let credential = header
                    .value
                    .split_once(char::is_whitespace)
                    .map_or(header.value.as_str(), |(_, value)| value.trim());
                push_credential(&mut output, credential);
            } else if header.key.eq_ignore_ascii_case("cookie") {
                push_credential(&mut output, &header.value);
                for cookie in header.value.split(';') {
                    let Some((_, value)) = cookie.split_once('=') else {
                        continue;
                    };
                    push_credential(&mut output, value.trim().trim_matches(['\'', '"']));
                }
            } else if sensitive_header(&header.key) {
                push_credential(&mut output, &header.value);
            }
        }
    }
    output.sort_by_key(|value| std::cmp::Reverse(value.len()));
    output.dedup();
    output
}

fn apply_import_credential_redactions(
    imported: &mut ImportResult,
    literals: &[String],
    secret_template_variables: &BTreeSet<String>,
) {
    let invalidate_variables = |variables: &mut [Variable]| {
        for variable in variables {
            if secret_template_variables.contains(&variable.key) {
                variable.value =
                    VariableValue::MissingSecret(super::persistence_safety::missing_secret_ref());
            }
        }
    };
    invalidate_variables(&mut imported.collection.variables);
    for folder in &mut imported.folders {
        invalidate_variables(&mut folder.variables);
    }
    for request in &mut imported.requests {
        invalidate_variables(&mut request.variables);
    }
    for environment in &mut imported.environments {
        invalidate_variables(&mut environment.variables);
    }
    redact_export_graph(
        &mut imported.collection,
        &mut imported.folders,
        &mut imported.requests,
        &mut imported.environments,
        &mut imported.examples,
        literals,
    );
    scrub_import_extension_literals(imported, literals);
}

fn scrub_import_extension_literals(imported: &mut ImportResult, literals: &[String]) {
    let scrub = |extensions: &mut serde_json::Map<String, Value>| {
        sanitize_extensions(extensions);
        redact_freeform_json_map(extensions, literals);
    };
    scrub(&mut imported.collection.extensions);
    for folder in &mut imported.folders {
        scrub(&mut folder.extensions);
    }
    for request in &mut imported.requests {
        scrub(&mut request.extensions);
    }
    for environment in &mut imported.environments {
        scrub(&mut environment.extensions);
    }
    for example in &mut imported.examples {
        scrub(&mut example.extensions);
    }
}

/// Preserve unsupported import data for round trips, but never retain values
/// under credential-like field names. The comparison removes punctuation and
/// casing so aliases such as `x-backup-token`, `client_secret`, and `apiKey`
/// share one fail-closed rule.
fn sanitize_extension_value(field: Option<&str>, value: &mut Value) {
    if field.is_some_and(sensitive_extension_key) {
        match value {
            Value::String(_) | Value::Number(_) => {
                *value = Value::String("<redacted>".into());
                return;
            }
            Value::Bool(_) | Value::Null => return,
            Value::Array(_) | Value::Object(_) => {}
        }
    }
    match value {
        Value::Array(values) => {
            for value in values {
                sanitize_extension_value(field, value);
            }
        }
        Value::Object(values) => {
            let inherited_sensitive = field.is_some_and(sensitive_extension_key)
                && !field.is_some_and(|field| normalize_field(field).ends_with("cookies"));
            let names_sensitive_header = values
                .get("key")
                .or_else(|| values.get("name"))
                .and_then(Value::as_str)
                .is_some_and(sensitive_header);
            if names_sensitive_header && let Some(value) = values.get_mut("value") {
                *value = Value::String("<redacted>".into());
            }
            for (key, value) in values.iter_mut() {
                sanitize_extension_value(
                    if inherited_sensitive {
                        field
                    } else {
                        Some(key)
                    },
                    value,
                );
            }
        }
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
}

fn sensitive_extension_key(field: &str) -> bool {
    let normalized = normalize_field(field);
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

fn without_query(url: &str) -> String {
    let (before_fragment, fragment) = url
        .split_once('#')
        .map_or((url, None), |(before, after)| (before, Some(after)));
    let base = before_fragment
        .split_once('?')
        .map_or(before_fragment, |(before, _)| before);
    match fragment {
        Some(fragment) => format!("{base}#{fragment}"),
        None => base.to_string(),
    }
}

fn scrub_postman_auth(mut value: Value) -> Value {
    scrub_auth_value(&mut value, None);
    value
}

fn scrub_auth_value(value: &mut Value, field: Option<&str>) {
    if field.is_some_and(|field| {
        matches!(
            field.to_ascii_lowercase().as_str(),
            "value" | "token" | "password" | "secret" | "accesskey" | "secretkey" | "clientsecret"
        )
    }) && !value.is_null()
    {
        *value = Value::String("<redacted>".into());
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                scrub_auth_value(value, field);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                scrub_auth_value(value, Some(key));
            }
        }
        _ => {}
    }
}

fn scrub_auth(auth: &mut AuthConfig) {
    if let AuthConfig::Unsupported { raw, .. } = auth {
        *raw = scrub_postman_auth(std::mem::take(raw));
    }
}

fn invalidate_imported_auth(auth: &mut AuthConfig) {
    let fresh = || super::persistence_safety::missing_secret_ref();
    match auth {
        AuthConfig::ApiKey { value, .. } => *value = fresh(),
        AuthConfig::Basic { password, .. } => *password = fresh(),
        AuthConfig::Bearer { token } | AuthConfig::OAuth2 { token } => *token = fresh(),
        AuthConfig::OAuth2AuthorizationCodePkce {
            access_token,
            refresh_token,
            ..
        } => {
            if access_token.is_some() {
                *access_token = Some(fresh());
            }
            if refresh_token.is_some() {
                *refresh_token = Some(fresh());
            }
        }
        AuthConfig::OAuth2ClientCredentials {
            client_secret,
            access_token,
            ..
        } => {
            *client_secret = fresh();
            if access_token.is_some() {
                *access_token = Some(fresh());
            }
        }
        AuthConfig::OAuth2Password {
            client_secret,
            password,
            access_token,
            refresh_token,
            ..
        } => {
            if client_secret.is_some() {
                *client_secret = Some(fresh());
            }
            *password = fresh();
            if access_token.is_some() {
                *access_token = Some(fresh());
            }
            if refresh_token.is_some() {
                *refresh_token = Some(fresh());
            }
        }
        AuthConfig::AwsSigV4 {
            access_key,
            secret_key,
            session_token,
            ..
        } => {
            *access_key = fresh();
            *secret_key = fresh();
            if session_token.is_some() {
                *session_token = Some(fresh());
            }
        }
        AuthConfig::Login {
            basic,
            access_token,
            expires_at,
            ..
        } => {
            if let Some(basic) = basic {
                basic.password = fresh();
            }
            *access_token = None;
            *expires_at = None;
        }
        AuthConfig::Unsupported { raw, .. } => {
            *raw = scrub_postman_auth(std::mem::take(raw));
        }
        AuthConfig::Inherit | AuthConfig::None => {}
    }
}

fn invalidate_imported_variables(variables: &mut [Variable]) {
    for variable in variables {
        if matches!(
            variable.value,
            VariableValue::Secret(_) | VariableValue::MissingSecret(_)
        ) {
            variable.value =
                VariableValue::MissingSecret(super::persistence_safety::missing_secret_ref());
        }
    }
}

fn validate_export_graph(
    collection: &Collection,
    folders: &[Folder],
    requests: &[RedactedExportRequest],
    environments: &[Environment],
    examples: &[Example],
) -> Result<(), String> {
    let folder_by_id: BTreeMap<&FolderId, &Folder> =
        folders.iter().map(|folder| (&folder.id, folder)).collect();
    let folder_ids: BTreeSet<&FolderId> = folder_by_id.keys().copied().collect();
    let request_ids: BTreeSet<&RequestId> = requests
        .iter()
        .map(|request| &request.definition.id)
        .collect();
    let environment_ids: BTreeSet<&EnvironmentId> = environments
        .iter()
        .map(|environment| &environment.id)
        .collect();
    let example_ids: BTreeSet<&ExampleId> = examples.iter().map(|example| &example.id).collect();
    if folder_ids.len() != folders.len()
        || request_ids.len() != requests.len()
        || environment_ids.len() != environments.len()
        || example_ids.len() != examples.len()
    {
        return Err("export graph contains duplicate ids".into());
    }
    if environments
        .iter()
        .any(|environment| environment.workspace_id != collection.workspace_id)
    {
        return Err("export environment belongs to another workspace".into());
    }
    for folder in folders {
        if folder.collection_id != collection.id {
            return Err("export folder belongs to another collection".into());
        }
        if folder
            .parent_id
            .as_ref()
            .is_some_and(|parent| !folder_ids.contains(parent))
        {
            return Err("export folder references a missing parent".into());
        }
        let mut cursor = folder.parent_id.as_ref();
        let mut visited = BTreeSet::new();
        while let Some(parent_id) = cursor {
            if !visited.insert(parent_id) {
                return Err("export folder hierarchy contains a cycle".into());
            }
            cursor = folder_by_id
                .get(parent_id)
                .ok_or_else(|| "export folder references a missing parent".to_string())?
                .parent_id
                .as_ref();
        }
    }
    for request in requests {
        if request.definition.collection_id != collection.id {
            return Err("export request belongs to another collection".into());
        }
        if request
            .definition
            .folder_id
            .as_ref()
            .is_some_and(|folder| !folder_ids.contains(folder))
        {
            return Err("export request references a missing folder".into());
        }
    }
    if examples
        .iter()
        .any(|example| !request_ids.contains(&example.request_id))
    {
        return Err("export example references a missing request".into());
    }
    if examples
        .iter()
        .filter_map(|example| example.request.as_ref())
        .any(|snapshot| {
            super::persistence_safety::url_has_userinfo(&snapshot.url)
                || snapshot
                    .replay
                    .as_ref()
                    .is_some_and(|replay| super::persistence_safety::url_has_userinfo(&replay.url))
        })
    {
        return Err("export example request URL must not include user information".into());
    }
    Ok(())
}

fn scrub_environment_secrets(environment: &mut Environment) {
    for variable in &mut environment.variables {
        if let super::VariableValue::Secret(reference) = &variable.value {
            variable.value = super::VariableValue::MissingSecret(reference.clone());
        }
    }
}

fn json_scalar(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-._~:/?&=%".contains(&byte))
    {
        return value.into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_words(input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                word.push(character);
            }
        } else if character.is_whitespace() && quote.is_none() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if escaped || quote.is_some() {
        return Err("unterminated escape or quote in cURL command".into());
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompileContext, SecretResolver, compile_request};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Secrets(HashMap<String, String>);

    impl SecretResolver for Secrets {
        fn resolve(&self, reference: &SecretRef) -> Result<String, String> {
            self.0
                .get(reference.as_str())
                .cloned()
                .ok_or_else(|| "missing secret".into())
        }
    }

    struct CountingSecrets {
        reference: String,
        value: String,
        calls: AtomicUsize,
    }

    impl SecretResolver for CountingSecrets {
        fn resolve(&self, reference: &SecretRef) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if reference.as_str() == self.reference {
                Ok(self.value.clone())
            } else {
                Err("missing secret".into())
            }
        }
    }

    struct DerivedSecrets;

    impl SecretResolver for DerivedSecrets {
        fn resolve(&self, reference: &SecretRef) -> Result<String, String> {
            Ok(format!("resolved-secret-{}", reference.as_str()))
        }
    }

    struct References(HashMap<String, Vec<u8>>);

    impl ImportReferenceResolver for References {
        fn resolve(&self, canonical_uri: &str) -> Result<Vec<u8>, String> {
            self.0
                .get(canonical_uri)
                .cloned()
                .ok_or_else(|| format!("unexpected reference {canonical_uri}"))
        }
    }

    #[test]
    fn openapi_import_resolves_bounded_external_refs_and_preserves_origin_metadata() {
        let workspace = WorkspaceId::new("project").unwrap();
        let origin = ImportOrigin::new("https://schemas.example.test/root.yaml").unwrap();
        let resolver = References(HashMap::from([(
            "https://schemas.example.test/models.yaml".into(),
            br#"components:
  schemas:
    Payload:
      type: object
      properties:
        external_id:
          type: string
          example: resolved
"#
            .to_vec(),
        )]));
        let imported = import_with_origin(
            &workspace,
            br#"openapi: 3.1.0
info: {title: External, version: '1'}
servers:
  - url: https://api.example.test/{version}
    variables: {version: {default: v2}}
paths:
  /items:
    post:
      requestBody:
        content:
          application/json:
            schema:
              $ref: './models.yaml#/components/schemas/Payload'
      responses: {'200': {description: ok}}
"#,
            origin.clone(),
            &resolver,
        )
        .unwrap();

        assert_eq!(imported.origin, Some(origin));
        assert!(
            imported
                .collection
                .extensions
                .contains_key("openapi_servers")
        );
        assert!(matches!(
            &imported.requests[0].body,
            Body::Raw { text, .. } if text.contains("resolved")
        ));
    }

    #[test]
    fn openapi_external_reference_depth_and_byte_budgets_fail_closed() {
        let workspace = WorkspaceId::new("project").unwrap();
        let origin = ImportOrigin::new("https://schemas.example.test/root.yaml").unwrap();
        let root = br#"openapi: 3.1.0
info: {title: Bounded, version: '1'}
paths: {}
components:
  schemas:
    Payload: {$ref: './ref-0.yaml'}
"#;
        let mut documents = HashMap::new();
        for index in 0..=MAX_REFERENCE_DEPTH {
            documents.insert(
                format!("https://schemas.example.test/ref-{index}.yaml"),
                if index == MAX_REFERENCE_DEPTH {
                    b"type: object\n".to_vec()
                } else {
                    format!("$ref: './ref-{}.yaml'\n", index + 1).into_bytes()
                },
            );
        }
        let depth_error =
            import_with_origin(&workspace, root, origin.clone(), &References(documents))
                .unwrap_err();
        assert!(depth_error.contains("nesting exceeds"), "{depth_error}");

        let bytes_error = import_with_origin(
            &workspace,
            root,
            origin,
            &References(HashMap::from([(
                "https://schemas.example.test/ref-0.yaml".into(),
                vec![b' '; MAX_IMPORT_BYTES + 1],
            )])),
        )
        .unwrap_err();
        assert!(bytes_error.contains("total bytes"), "{bytes_error}");
    }

    #[test]
    fn openapi_external_reference_document_budget_fails_closed() {
        let workspace = WorkspaceId::new("project").unwrap();
        let origin = ImportOrigin::new("https://schemas.example.test/root.yaml").unwrap();
        let mut schemas = serde_json::Map::new();
        let mut documents = HashMap::new();
        for index in 0..MAX_REFERENCE_DOCUMENTS {
            schemas.insert(
                format!("Schema{index}"),
                json!({"$ref": format!("./schema-{index}.json")}),
            );
            documents.insert(
                format!("https://schemas.example.test/schema-{index}.json"),
                br#"{"type":"object"}"#.to_vec(),
            );
        }
        let root = serde_json::to_vec(&json!({
            "openapi":"3.1.0",
            "info":{"title":"Bounded", "version":"1"},
            "paths":{},
            "components":{"schemas": Value::Object(schemas)}
        }))
        .unwrap();
        let error =
            import_with_origin(&workspace, &root, origin, &References(documents)).unwrap_err();
        assert!(error.contains("reference documents"), "{error}");
    }

    #[test]
    fn openapi_external_reference_schemes_and_cycles_fail_closed() {
        let workspace = WorkspaceId::new("project").unwrap();
        let origin = ImportOrigin::new("https://schemas.example.test/root.yaml").unwrap();
        let unsafe_root = br#"openapi: 3.1.0
info: {title: Unsafe, version: '1'}
paths: {}
components:
  schemas:
    Payload: {$ref: 'http://schemas.example.test/payload.yaml'}
"#;
        let unsafe_error = import_with_origin(
            &workspace,
            unsafe_root,
            origin.clone(),
            &References(HashMap::new()),
        )
        .unwrap_err();
        assert!(unsafe_error.contains("file: or https:"), "{unsafe_error}");

        let cyclic_root = br#"openapi: 3.1.0
info: {title: Cyclic, version: '1'}
paths: {}
components:
  schemas:
    Payload: {$ref: './a.yaml'}
"#;
        let resolver = References(HashMap::from([
            (
                "https://schemas.example.test/a.yaml".into(),
                b"$ref: './b.yaml'\n".to_vec(),
            ),
            (
                "https://schemas.example.test/b.yaml".into(),
                b"$ref: './a.yaml'\n".to_vec(),
            ),
        ]));
        let cycle_error =
            import_with_origin(&workspace, cyclic_root, origin, &resolver).unwrap_err();
        assert!(
            cycle_error.contains("cyclic OpenAPI reference"),
            "{cycle_error}"
        );
    }

    #[test]
    fn imports_openapi_operations_and_parameters() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br##"{
              "openapi":"3.1.0",
              "info":{"title":"Pets","version":"1"},
              "servers":[{"url":"https://api.example.test/v1"}],
              "components":{
                "parameters":{"Limit":{"name":"limit","in":"query","schema":{"default":20}}},
                "schemas":{"Pet":{"type":"object","properties":{"name":{"type":"string","example":"Mochi"}}}},
                "securitySchemes":{"oauth":{"type":"oauth2","flows":{"clientCredentials":{"tokenUrl":"https://auth.example/token","scopes":{}}}}}
              },
              "paths":{"/pets":{"post":{"summary":"List pets","x-operation-id":"preserved","security":[{"oauth":[]}],"parameters":[
                {"$ref":"#/components/parameters/Limit"}
              ],"requestBody":{"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Pet"},"example":{"name":"Mochi"}}}}}}}
            }"##,
        )
        .unwrap();
        assert_eq!(imported.format, ImportFormat::OpenApi);
        assert_eq!(imported.requests.len(), 1);
        assert_eq!(imported.requests[0].url, "/pets");
        assert_eq!(imported.environments.len(), 1);
        assert_eq!(imported.environments[0].name, "Pets");
        assert_eq!(
            imported.environments[0].base_url,
            "https://api.example.test/v1"
        );
        assert!(!imported.environments[0].active);
        assert_eq!(imported.requests[0].params[0].value, "20");
        assert!(matches!(
            &imported.requests[0].body,
            Body::Raw { text, .. } if text.contains("Mochi")
        ));
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("openapi_operation")
                .and_then(|value| value.get("x-operation-id")),
            Some(&json!("preserved"))
        );
        assert!(imported.collection.extensions.contains_key("components"));
    }

    #[test]
    fn openapi_server_and_path_variables_compile_as_workbench_templates() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br##"{
              "openapi":"3.1.0",
              "info":{"title":"Templated","version":"1"},
              "servers":[{"url":"https://{region}.example.test/{version}","variables":{
                "region":{"default":"us-east"},
                "version":{"default":"v2","description":"API version"}
              }}],
              "paths":{"/users/{userId}":{"get":{"parameters":[
                {"name":"userId","in":"path","required":true,"deprecated":true,"schema":{"default":"42"}},
                {"name":"expand","in":"query","schema":{"default":"profile"}}
              ]}}}
            }"##,
        )
        .unwrap();

        assert_eq!(imported.requests[0].url, "/users/{{userId}}");
        assert_eq!(
            imported.environments[0].base_url,
            "https://{{region}}.example.test/{{version}}"
        );
        assert_eq!(
            imported
                .collection
                .variables
                .iter()
                .map(|variable| (variable.key.as_str(), &variable.value))
                .collect::<Vec<_>>(),
            vec![
                ("region", &VariableValue::Plain("us-east".into())),
                ("version", &VariableValue::Plain("v2".into())),
            ]
        );
        assert!(
            imported.requests[0]
                .variables
                .iter()
                .any(|variable| variable.key == "userId"
                    && variable.value == VariableValue::Plain("42".into()))
        );

        let secrets = Secrets(HashMap::new());
        let context = CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: Some(&imported.environments[0].base_url),
            environment_auth: None,
        };
        let (prepared, _) =
            compile_request(&imported.requests[0], Some(&imported.collection), &context).unwrap();
        assert_eq!(
            prepared.url,
            "https://us-east.example.test/v2/users/42?expand=profile"
        );
    }

    #[test]
    fn openapi_oauth_flows_use_typed_grants_and_retain_unsupported_metadata() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br##"{
              "openapi":"3.1.0",
              "info":{"title":"OAuth","version":"1"},
              "servers":[{"url":"https://api.example.test"}],
              "components":{"securitySchemes":{
                "machine":{"type":"oauth2","flows":{"clientCredentials":{
                  "tokenUrl":"https://auth.example.test/token",
                  "scopes":{"read:items":"Read items","write:items":"Write items"}
                }}},
                "interactive":{"type":"oauth2","flows":{"authorizationCode":{
                  "authorizationUrl":"https://auth.example.test/authorize",
                  "tokenUrl":"https://auth.example.test/token",
                  "scopes":{"openid":"Sign in","profile":"Profile"}
                }}},
                "implicitOnly":{"type":"oauth2","flows":{"implicit":{
                  "authorizationUrl":"https://auth.example.test/authorize",
                  "scopes":{"legacy":"Legacy"}
                }}}
              }},
              "paths":{
                "/machine":{"get":{"security":[{"machine":["read:items"]}]}},
                "/interactive":{"get":{"security":[{"interactive":["openid","profile"]}]}},
                "/legacy":{"get":{"security":[{"implicitOnly":["legacy"]}]}}
              }
            }"##,
        )
        .unwrap();

        let by_url = |suffix: &str| {
            imported
                .requests
                .iter()
                .find(|request| request.url.ends_with(suffix))
                .unwrap()
        };
        assert!(matches!(
            &by_url("/machine").auth,
            AuthConfig::OAuth2ClientCredentials { token_endpoint, scopes, access_token, .. }
                if token_endpoint == "https://auth.example.test/token"
                    && scopes == &["read:items"]
                    && access_token.is_none()
        ));
        assert!(matches!(
            &by_url("/interactive").auth,
            AuthConfig::OAuth2AuthorizationCodePkce {
                authorization_endpoint,
                token_endpoint,
                scopes,
                access_token,
                refresh_token,
                ..
            } if authorization_endpoint == "https://auth.example.test/authorize"
                && token_endpoint == "https://auth.example.test/token"
                && scopes == &["openid", "profile"]
                && access_token.is_none()
                && refresh_token.is_none()
        ));
        assert!(matches!(
            &by_url("/legacy").auth,
            AuthConfig::Unsupported { .. }
        ));
        assert_eq!(
            imported.collection.extensions["openapi_auth_metadata"]
                .pointer("/implicitOnly/flows/implicit/login_endpoint/host"),
            Some(&json!("auth.example.test"))
        );
        assert_eq!(
            imported.collection.extensions["openapi_auth_metadata"]
                .pointer("/implicitOnly/flows/implicit/login_endpoint/path"),
            Some(&json!("/authorize"))
        );
        assert!(
            imported
                .collection
                .extensions
                .get("components")
                .and_then(|value| {
                    value.pointer("/securitySchemes/interactive/flows/authorizationCode")
                })
                .is_some()
        );
    }

    #[test]
    fn openapi_import_resolves_chained_refs_security_tags_and_response_examples() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br##"{
              "openapi":"3.1.0",
              "info":{"title":"Orders","version":"1"},
              "servers":[{"url":"https://api.example.test"}],
              "security":[{"api_key":[]}],
              "components":{
                "securitySchemes":{"api_key":{"type":"apiKey","name":"X-API-Key","in":"header"}},
                "schemas":{
                  "OrderAlias":{"$ref":"#/components/schemas/Order"},
                  "Order":{"type":"object","properties":{"id":{"type":"integer","default":7},"state":{"type":"string","enum":["new"]}}}
                },
                "requestBodies":{"CreateOrder":{"content":{"application/problem+json":{"schema":{"$ref":"#/components/schemas/OrderAlias"}}}}},
                "responses":{"Created":{"description":"Created","content":{"application/json":{"examples":{"sample":{"value":{"id":9,"state":"new"}}}}}}}
              },
              "paths":{"/orders":{"post":{
                "tags":["Orders"],
                "requestBody":{"$ref":"#/components/requestBodies/CreateOrder"},
                "responses":{"201":{"$ref":"#/components/responses/Created"}},
                "x-safe":{"shape":{"kept":true}}
              }}}
            }"##,
        )
        .unwrap();

        assert!(matches!(
            imported.collection.auth,
            AuthConfig::ApiKey { .. }
        ));
        assert_eq!(imported.folders.len(), 1);
        assert_eq!(
            imported.requests[0].folder_id,
            Some(imported.folders[0].id.clone())
        );
        assert!(matches!(imported.requests[0].auth, AuthConfig::Inherit));
        assert!(matches!(
            &imported.requests[0].body,
            Body::Raw { media_type: RawBodyKind::Json, text }
                if text.contains("\"id\": 7") && text.contains("\"state\": \"new\"")
        ));
        assert_eq!(
            imported.requests[0]
                .headers
                .iter()
                .find(|header| header.key == "Content-Type")
                .map(|header| header.value.as_str()),
            Some("application/problem+json")
        );
        assert_eq!(imported.examples.len(), 1);
        assert_eq!(imported.examples[0].response.status, 201);
        assert_eq!(
            String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(&imported.examples[0].response.body_base64)
                    .unwrap()
            )
            .unwrap(),
            "{\n  \"id\": 9,\n  \"state\": \"new\"\n}"
        );
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("openapi_operation")
                .and_then(|operation| operation.pointer("/requestBody/$ref")),
            Some(&json!("#/components/requestBodies/CreateOrder"))
        );
    }

    #[test]
    fn swagger_import_resolves_body_schemas_and_form_data() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br##"{
              "swagger":"2.0","info":{"title":"Legacy","version":"1"},
              "host":"api.example.test","basePath":"/v1","schemes":["https"],
              "consumes":["application/json"],
              "definitions":{"Pet":{"type":"object","properties":{"name":{"type":"string","default":"Mochi"}}}},
              "paths":{
                "/pets":{"post":{"parameters":[{"name":"pet","in":"body","schema":{"$ref":"#/definitions/Pet"}}],"responses":{"201":{"description":"Created","schema":{"$ref":"#/definitions/Pet"}}}}},
                "/uploads":{"post":{"consumes":["multipart/form-data"],"parameters":[{"name":"label","in":"formData","type":"string","default":"avatar"},{"name":"file","in":"formData","type":"file"}],"responses":{"204":{"description":"Done"}}}}
              }
            }"##,
        )
        .unwrap();

        let pets = imported
            .requests
            .iter()
            .find(|request| request.url.ends_with("/pets"))
            .unwrap();
        assert!(matches!(
            &pets.body,
            Body::Raw { media_type: RawBodyKind::Json, text } if text.contains("Mochi")
        ));
        let uploads = imported
            .requests
            .iter()
            .find(|request| request.url.ends_with("/uploads"))
            .unwrap();
        assert!(matches!(
            &uploads.body,
            Body::Multipart { rows }
                if matches!(&rows[0].value, MultipartValue::Text(value) if value == "avatar")
                    && matches!(&rows[1].value, MultipartValue::File(_))
        ));
        assert_eq!(imported.examples.len(), 1);
        assert_eq!(imported.examples[0].response.status, 201);
    }

    #[test]
    fn insomnia_import_preserves_supported_hierarchy_and_round_trip_shapes() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"{
              "_type":"export","__export_format":4,"x-export":{"kept":true},
              "resources":[
                {"_id":"wrk_1","_type":"workspace","name":"Payments","description":"API","x-workspace":{"kept":true}},
                {"_id":"fld_1","_type":"request_group","parentId":"wrk_1","name":"Admin","metaSortKey":10,"x-folder":[1,{"kept":true}]},
                {"_id":"env_1","_type":"environment","parentId":"wrk_1","name":"Dev","data":{"base_url":"https://dev.example","api_token":"env-secret"},"color":"blue"},
                {"_id":"req_1","_type":"request","parentId":"fld_1","name":"Create","method":"POST",
                 "url":"https://example.test/items?old=duplicated","parameters":[{"name":"old","value":"one"},{"name":"off","value":"two","disabled":true}],
                 "headers":[{"name":"Accept","value":"application/json"},{"name":"Accept","value":"text/plain"}],
                 "authentication":{"type":"bearer","token":"insomnia-secret","prefix":"Token","x-option":{"kept":true}},
                 "body":{"mimeType":"multipart/form-data","params":[{"name":"label","value":"insomnia-secret"},{"name":"upload","type":"file","fileName":"/tmp/file.bin"}],"x-body":{"kept":true}},
                 "preRequestScript":"pre();","afterResponseScript":"test();","description":"kept description","x-request":{"backup":"insomnia-secret","shape":[1,2]}},
                {"_id":"res_1","_type":"response","parentId":"req_1","name":"Created","statusCode":201,"statusMessage":"Created",
                 "headers":[{"name":"Content-Type","value":"application/json"}],"body":{"text":"{\"token\":\"insomnia-secret\"}"},"x-response":{"kept":true}}
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(imported.folders.len(), 1);
        assert_eq!(
            imported
                .collection
                .extensions
                .get("insomnia_export")
                .and_then(|value| value.pointer("/x-export/kept")),
            Some(&json!(true))
        );
        assert_eq!(
            imported.requests[0].folder_id,
            Some(imported.folders[0].id.clone())
        );
        assert_eq!(imported.requests[0].url, "https://example.test/items");
        assert_eq!(imported.requests[0].params.len(), 2);
        assert!(!imported.requests[0].params[1].enabled);
        assert_eq!(imported.requests[0].headers.len(), 2);
        assert!(matches!(
            imported.requests[0].auth,
            AuthConfig::Bearer { .. }
        ));
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("insomnia_security_options")
                .and_then(|value| value.pointer("/prefix")),
            Some(&json!("Token"))
        );
        assert_eq!(imported.requests[0].scripts.pre_request, "pre();");
        assert_eq!(imported.requests[0].scripts.tests, "test();");
        assert!(matches!(
            &imported.requests[0].body,
            Body::Multipart { rows }
                if matches!(&rows[0].value, MultipartValue::Text(value) if value == "<redacted>")
                    && matches!(&rows[1].value, MultipartValue::File(path) if path == "/tmp/file.bin")
        ));
        assert_eq!(imported.environments.len(), 1);
        assert!(matches!(
            imported.environments[0]
                .variables
                .iter()
                .find(|variable| variable.key == "api_token")
                .unwrap()
                .value,
            VariableValue::MissingSecret(_)
        ));
        assert_eq!(imported.examples.len(), 1);
        assert_eq!(imported.examples[0].response.status, 201);
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("x-request")
                .and_then(|value| value.pointer("/shape/1")),
            Some(&json!(2))
        );
        let serialized = serde_json::to_string(&imported).unwrap();
        assert!(!serialized.contains("insomnia-secret"));
        assert!(!serialized.contains("env-secret"));
        assert!(serialized.contains("kept description"));
    }

    #[test]
    fn imports_curl_with_quoted_body_and_repeated_headers() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"curl -H 'Accept: text/plain' -H 'Accept: application/json' --data-raw '{"ok":true}' https://example.test/items"#,
        )
        .unwrap();
        let request = &imported.requests[0];
        assert_eq!(request.method.as_str(), "POST");
        assert_eq!(request.headers.len(), 2);
        assert!(matches!(&request.body, Body::Raw { text, .. } if text == r#"{"ok":true}"#));
    }

    #[test]
    fn curl_import_never_retains_literal_authorization_credentials() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"curl -H 'Authorization: Bearer actual-curl-secret' https://example.test"#,
        )
        .unwrap();
        let serialized = serde_json::to_string(&imported).unwrap();
        assert!(!serialized.contains("actual-curl-secret"));
        assert_eq!(imported.requests[0].headers[0].value, "<redacted>");
        assert!(!imported.requests[0].headers[0].enabled);
    }

    #[test]
    fn curl_import_scrubs_authorization_credentials_copied_into_the_body() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"curl -H 'Authorization: Bearer actual-curl-secret' --data-raw 'copy=actual-curl-secret' https://example.test"#,
        )
        .unwrap();

        let serialized = serde_json::to_string(&imported).unwrap();
        assert!(!serialized.contains("actual-curl-secret"), "{serialized}");
        assert!(matches!(
            &imported.requests[0].body,
            Body::Raw { text, .. } if text == "copy=<redacted>"
        ));
        let prepared = PreparedCollectionExport::new(
            &imported.collection,
            &imported.folders,
            &imported.requests,
            &imported.environments,
            &imported.examples,
            &Secrets(HashMap::new()),
        )
        .unwrap();
        for output in [
            export_prepared_agentops_bundle(&prepared).unwrap(),
            export_prepared_postman_collection(&prepared).unwrap(),
        ] {
            assert!(!output.contains("actual-curl-secret"), "{output}");
        }
    }

    #[test]
    fn curl_import_scrubs_api_keys_and_individual_cookie_values_copied_into_the_body() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"curl -H 'X-API-Key: actual-api-key' -H 'Cookie: theme=dark; sid=actual-cookie-secret' --data-raw 'api=actual-api-key&session=actual-cookie-secret' https://example.test"#,
        )
        .unwrap();

        let serialized = serde_json::to_string(&imported).unwrap();
        for secret in ["actual-api-key", "actual-cookie-secret"] {
            assert!(!serialized.contains(secret), "{serialized}");
        }
        assert!(matches!(
            &imported.requests[0].body,
            Body::Raw { text, .. }
                if text == "api=<redacted>&session=<redacted>"
        ));
        assert!(
            imported.requests[0]
                .headers
                .iter()
                .all(|header| !header.enabled && header.value == "<redacted>")
        );

        let prepared = PreparedCollectionExport::new(
            &imported.collection,
            &imported.folders,
            &imported.requests,
            &imported.environments,
            &imported.examples,
            &Secrets(HashMap::new()),
        )
        .unwrap();
        for output in [
            export_prepared_agentops_bundle(&prepared).unwrap(),
            export_prepared_postman_collection(&prepared).unwrap(),
        ] {
            for secret in ["actual-api-key", "actual-cookie-secret"] {
                assert!(!output.contains(secret), "{output}");
            }
        }
    }

    #[test]
    fn postman_import_preserves_hierarchy_scripts_examples_and_extensions_without_credentials() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"{
              "info":{"name":"Payments","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
              "x-company":{"owner":"platform"},
              "auth":{"type":"bearer","bearer":[{"key":"token","value":"actual-imported-secret"}]},
              "event":[{"listen":"prerequest","script":{"exec":["pm.variables.set('a', 'b');"]}}],
              "item":[{"name":"Admin","protocolProfileBehavior":{"disableBodyPruning":true},"item":[
                {"name":"Create","x-request-extra":42,"event":[{"listen":"test","script":{"exec":["pm.test('ok', () => {});"]}}],
                 "request":{"method":"POST","url":"https://example.test","body":{"mode":"formdata","formdata":[
                   {"key":"label","value":"one","type":"text"},{"key":"payload","src":"/tmp/a.bin","type":"file"}
                 ]}},
                 "response":[{"name":"Created","code":201,"status":"Created","header":[],"body":"{\"ok\":true}","x-example-extra":true}]}
              ]}]
            }"#,
        )
        .unwrap();
        assert_eq!(imported.folders.len(), 1);
        assert_eq!(
            imported.requests[0].folder_id,
            Some(imported.folders[0].id.clone())
        );
        assert!(matches!(
            imported.collection.auth,
            AuthConfig::Bearer { .. }
        ));
        assert!(
            imported
                .collection
                .scripts
                .pre_request
                .contains("pm.variables")
        );
        assert!(imported.requests[0].scripts.tests.contains("pm.test"));
        assert!(imported.collection.extensions.contains_key("x-company"));
        assert!(
            imported.folders[0]
                .extensions
                .contains_key("protocolProfileBehavior")
        );
        assert!(
            imported.requests[0]
                .extensions
                .contains_key("x-request-extra")
        );
        assert_eq!(imported.examples.len(), 1);
        assert_eq!(imported.examples[0].response.status, 201);
        assert!(
            imported.examples[0]
                .extensions
                .contains_key("x-example-extra")
        );
        assert!(matches!(
            &imported.requests[0].body,
            Body::Multipart { rows }
                if matches!(rows[1].value, MultipartValue::File(ref path) if path == "/tmp/a.bin")
        ));
        assert!(
            !serde_json::to_string(&imported)
                .unwrap()
                .contains("actual-imported-secret")
        );
    }

    #[test]
    fn postman_structured_query_is_not_duplicated_by_compilation() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"{
              "info":{"name":"Queries","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
              "item":[{"name":"List","request":{"method":"GET","url":{
                "raw":"https://example.test/items?q=one&tag=a#results",
                "query":[{"key":"q","value":"one"},{"key":"tag","value":"a"}]
              }}}]
            }"#,
        )
        .unwrap();

        let request = &imported.requests[0];
        assert_eq!(request.url, "https://example.test/items#results");
        assert_eq!(request.params.len(), 2);
    }

    #[test]
    fn unknown_extensions_round_trip_non_sensitive_scalars_but_redact_credentials() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"{
              "info":{"name":"Extensions","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
              "x-company":{"owner":"platform","count":42,"enabled":true,
                "nested":{"region":"us-east-1","clientSecret":"do-not-store"}},
              "x-backup-token":"also-do-not-store",
              "item":[]
            }"#,
        )
        .unwrap();

        assert_eq!(
            imported.collection.extensions.get("x-company"),
            Some(&json!({
                "owner":"platform", "count":42, "enabled":true,
                "nested":{"region":"us-east-1", "clientSecret":"<redacted>"}
            }))
        );
        assert_eq!(
            imported.collection.extensions.get("x-backup-token"),
            Some(&json!("<redacted>"))
        );
    }

    #[test]
    fn postman_import_sanitizes_credentials_in_unknown_extensions() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"{
              "info":{"name":"Payments","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
              "x-backup-token":"collection-secret",
              "x-company":{"owner":"platform","x-auth":"alias-secret","count":42},
              "bearer":"bearer-secret",
              "sid":"sid-secret",
              "x-header-backup":[{"key":"Authorization","value":"header-secret"}],
              "item":[{"name":"Create","x-private":{"apiKey":"request-secret","region":"us-east-1"},
                "request":{"method":"POST","url":"https://example.test"}}]
            }"#,
        )
        .unwrap();

        assert_eq!(
            imported.collection.extensions.get("x-backup-token"),
            Some(&json!("<redacted>"))
        );
        assert_eq!(
            imported
                .collection
                .extensions
                .get("x-company")
                .and_then(|value| value.pointer("/owner")),
            Some(&json!("platform"))
        );
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("x-private")
                .and_then(|value| value.pointer("/apiKey")),
            Some(&json!("<redacted>"))
        );
        let serialized = serde_json::to_string(&imported).unwrap();
        assert!(!serialized.contains("collection-secret"));
        assert!(!serialized.contains("request-secret"));
        assert!(!serialized.contains("header-secret"));
        assert!(!serialized.contains("alias-secret"));
        assert!(!serialized.contains("bearer-secret"));
        assert!(!serialized.contains("sid-secret"));
        assert!(serialized.contains("platform"));
        assert_eq!(
            imported
                .collection
                .extensions
                .get("x-company")
                .and_then(|value| value.pointer("/count")),
            Some(&json!(42))
        );
    }

    #[test]
    fn imported_auth_literals_are_scrubbed_from_innocently_named_extensions_at_all_scopes() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"{
              "info":{"name":"Secrets","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
              "auth":{"type":"bearer","bearer":[{"key":"token","value":"collection-literal"}]},
              "variable":[{"key":"opaque_name","value":"environment-literal","type":"secret"}],
              "x-backup":{"value_copy":"collection-literal","environment_copy":"environment-literal","safe":{"shape":[1,true,null]}},
              "item":[{"name":"Folder","auth":{"type":"basic","basic":[{"key":"password","value":"folder-literal"}]},
                "x-copy":"folder-literal","item":[{"name":"Request","x-values":["request-literal",{"still":"safe"}],
                  "request":{"method":"GET","url":"https://example.test","auth":{"type":"bearer","bearer":[{"key":"token","value":"request-literal"}]}}
                }]}
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            imported
                .collection
                .extensions
                .get("x-backup")
                .and_then(|value| value.pointer("/value_copy")),
            Some(&json!("<redacted>"))
        );
        assert_eq!(
            imported
                .collection
                .extensions
                .get("x-backup")
                .and_then(|value| value.pointer("/environment_copy")),
            Some(&json!("<redacted>"))
        );
        assert_eq!(
            imported.folders[0].extensions.get("x-copy"),
            Some(&json!("<redacted>"))
        );
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("x-values")
                .and_then(|value| value.pointer("/0")),
            Some(&json!("<redacted>"))
        );
        assert_eq!(
            imported
                .collection
                .extensions
                .get("x-backup")
                .and_then(|value| value.pointer("/safe/shape")),
            Some(&json!([1, true, null]))
        );
        let serialized = serde_json::to_string(&imported).unwrap();
        assert!(!serialized.contains("collection-literal"));
        assert!(!serialized.contains("folder-literal"));
        assert!(!serialized.contains("request-literal"));
        assert!(!serialized.contains("environment-literal"));
    }

    #[test]
    fn postman_import_scrubs_derived_basic_credentials_before_both_exports() {
        let workspace = WorkspaceId::new("project").unwrap();
        let encoded =
            base64::engine::general_purpose::STANDARD.encode("operator:actual-basic-password!");
        let encoded_form = encoded.replace('=', "%3D");
        let empty_password_encoded =
            base64::engine::general_purpose::STANDARD.encode("empty-password-user:");
        assert_ne!(encoded_form, encoded);
        let source = json!({
            "info": {
                "name": "Imported Basic",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "item": [
                {
                    "name": "Captured request",
                    "x-note": format!("Basic {encoded}; Basic {encoded_form}"),
                    "request": {
                        "method": "GET",
                        "url": "https://example.test",
                        "auth": {
                            "type": "basic",
                            "basic": [
                                {"key": "username", "value": "operator"},
                                {"key": "password", "value": "actual-basic-password!"}
                            ]
                        }
                    }
                },
                {
                    "name": "Empty password",
                    "x-note": format!("Basic {empty_password_encoded}"),
                    "request": {
                        "method": "GET",
                        "url": "https://example.test/empty",
                        "auth": {
                            "type": "basic",
                            "basic": [
                                {"key": "username", "value": "empty-password-user"},
                                {"key": "password", "value": ""}
                            ]
                        }
                    }
                }
            ]
        });
        let imported = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap();
        let imported_json = serde_json::to_string(&imported).unwrap();
        assert!(!imported_json.contains(&encoded), "{imported_json}");
        assert!(!imported_json.contains(&encoded_form), "{imported_json}");
        assert!(
            !imported_json.contains(&empty_password_encoded),
            "{imported_json}"
        );

        let prepared = PreparedCollectionExport::new(
            &imported.collection,
            &imported.folders,
            &imported.requests,
            &imported.environments,
            &imported.examples,
            &Secrets(HashMap::new()),
        )
        .unwrap();
        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains(&encoded), "{output}");
            assert!(!output.contains(&encoded_form), "{output}");
            assert!(!output.contains(&empty_password_encoded), "{output}");
            assert!(output.contains("<redacted>"), "{output}");
        }
    }

    #[test]
    fn postman_import_scrubs_static_templated_auth_and_container_copies() {
        let workspace = WorkspaceId::new("project").unwrap();
        let basic =
            base64::engine::general_purpose::STANDARD.encode("operator:actual-template-password");
        let source = json!({
            "info": {
                "name": format!("Captured {basic}"),
                "description": format!("Bearer actual-template-session; Basic {basic}"),
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "auth": {
                "type": "basic",
                "basic": [
                    {"key": "username", "value": "operator"},
                    {"key": "password", "value": "{{pw}}"}
                ]
            },
            "variable": [
                {"key": "pw", "value": "actual-template-password"},
                {"key": "session", "value": "actual-template-session"}
            ],
            "item": [{
                "name": "Bearer request",
                "request": {
                    "method": "GET",
                    "url": "https://example.test/captured/actual-template-session",
                    "auth": {
                        "type": "bearer",
                        "bearer": [{"key": "token", "value": "{{session}}"}]
                    }
                }
            }]
        });

        let imported = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap();
        let imported_json = serde_json::to_string(&imported).unwrap();
        for secret in [
            "actual-template-password",
            "actual-template-session",
            &basic,
        ] {
            assert!(!imported_json.contains(secret), "{imported_json}");
        }
        for key in ["pw", "session"] {
            assert!(matches!(
                imported
                    .collection
                    .variables
                    .iter()
                    .find(|variable| variable.key == key)
                    .map(|variable| &variable.value),
                Some(VariableValue::MissingSecret(_))
            ));
        }

        let prepared = PreparedCollectionExport::new(
            &imported.collection,
            &imported.folders,
            &imported.requests,
            &imported.environments,
            &imported.examples,
            &Secrets(HashMap::new()),
        )
        .unwrap();
        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            for secret in [
                "actual-template-password",
                "actual-template-session",
                &basic,
            ] {
                assert!(!output.contains(secret), "{output}");
            }
        }
    }

    #[test]
    fn postman_auth_discovery_does_not_redact_basic_usernames_or_metadata() {
        let workspace = WorkspaceId::new("project").unwrap();
        let source = json!({
            "info": {
                "name": "Catalog",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "auth": {
                "type": "basic",
                "basic": [
                    {"key": "username", "value": "a"},
                    {"key": "password", "value": "long-password"}
                ]
            },
            "item": [{
                "name": "Catalog request",
                "request": {"method": "GET", "url": "https://example.test/catalog"}
            }]
        });

        let imported = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(imported.collection.name, "Catalog");
        assert_eq!(imported.requests[0].name, "Catalog request");
        assert_eq!(imported.requests[0].url, "https://example.test/catalog");
    }

    #[test]
    fn imported_inherited_basic_credentials_are_scrubbed_from_typed_requests() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection_credential =
            base64::engine::general_purpose::STANDARD.encode("operator:imported-password!");
        let encoded_collection_credential = collection_credential.replace('=', "%3D");
        let folder_credential =
            base64::engine::general_purpose::STANDARD.encode("folder-user:folder-password");
        let source = json!({
            "info": {
                "name": "Inherited Basic",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "auth": {
                "type": "basic",
                "basic": [
                    {"key": "username", "value": "operator"},
                    {"key": "password", "value": "imported-password!"}
                ]
            },
            "item": [
                {
                    "name": "Collection request",
                    "request": {
                        "method": "POST",
                        "url": format!("https://example.test/items?captured={encoded_collection_credential}"),
                        "body": {"mode": "raw", "raw": format!("Basic {collection_credential}")}
                    }
                },
                {
                    "name": "Folder",
                    "auth": {
                        "type": "basic",
                        "basic": [
                            {"key": "username", "value": "folder-user"},
                            {"key": "password", "value": "folder-password"}
                        ]
                    },
                    "item": [{
                        "name": "Folder request",
                        "request": {
                            "method": "GET",
                            "url": format!("https://example.test/folder?captured={folder_credential}")
                        }
                    }]
                }
            ]
        });

        let imported = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap();
        let imported_json = serde_json::to_string(&imported).unwrap();
        assert!(
            !imported_json.contains(&collection_credential),
            "{imported_json}"
        );
        assert!(
            !imported_json.contains(&encoded_collection_credential),
            "{imported_json}"
        );
        assert!(
            !imported_json.contains(&folder_credential),
            "{imported_json}"
        );
        let prepared = PreparedCollectionExport::new(
            &imported.collection,
            &imported.folders,
            &imported.requests,
            &imported.environments,
            &imported.examples,
            &Secrets(HashMap::new()),
        )
        .unwrap();
        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains(&collection_credential), "{output}");
            assert!(!output.contains(&encoded_collection_credential), "{output}");
            assert!(!output.contains(&folder_credential), "{output}");
        }
    }

    #[test]
    fn imported_templated_basic_auth_fails_closed_without_a_resolved_password() {
        let workspace = WorkspaceId::new("project").unwrap();
        let source = json!({
            "info": {
                "name": "Templated Basic",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "auth": {
                "type": "basic",
                "basic": [
                    {"key": "username", "value": "{{user}}"},
                    {"key": "password", "value": "imported-password"}
                ]
            },
            "item": []
        });
        let error = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap_err();
        assert!(error.contains("variable scope is unavailable"), "{error}");
    }

    #[test]
    fn imported_templated_auth_rejects_disabled_and_unrelated_variable_decoys() {
        let workspace = WorkspaceId::new("project").unwrap();
        for source in [
            json!({
                "info": {
                    "name": "Disabled decoy",
                    "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
                },
                "auth": {
                    "type": "basic",
                    "basic": [
                        {"key": "username", "value": "operator"},
                        {"key": "password", "value": "{{pw}}"}
                    ]
                },
                "variable": [
                    {"key": "pw", "value": "disabled-decoy", "disabled": true}
                ],
                "item": []
            }),
            json!({
                "info": {
                    "name": "Unrelated decoy",
                    "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
                },
                "auth": {
                    "type": "basic",
                    "basic": [
                        {"key": "username", "value": "operator"},
                        {"key": "password", "value": "{{pw}}"}
                    ]
                },
                "item": [{
                    "name": "Unrelated folder",
                    "variable": [{"key": "pw", "value": "folder-decoy"}],
                    "item": []
                }]
            }),
        ] {
            let error = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap_err();
            assert!(error.contains("variable scope is unavailable"), "{error}");
        }
    }

    #[test]
    fn imported_credential_template_depth_is_bounded_before_stack_exhaustion() {
        const TEMPLATE_LINKS: usize = 12_000;

        let workspace = WorkspaceId::new("project").unwrap();
        let variables = (0..TEMPLATE_LINKS)
            .map(|index| {
                let value = if index + 1 == TEMPLATE_LINKS {
                    "terminal-secret".to_string()
                } else {
                    format!("{{{{v{}}}}}", index + 1)
                };
                json!({"key": format!("v{index}"), "value": value})
            })
            .collect::<Vec<_>>();
        let source = json!({
            "info": {
                "name": "Deep credential template",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "auth": {
                "type": "bearer",
                "bearer": [{"key": "token", "value": "{{v0}}"}]
            },
            "variable": variables,
            "item": []
        });

        let error = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap_err();
        assert!(error.contains("expansion nesting exceeds"), "{error}");
    }

    #[test]
    fn imported_credential_template_sequential_expansion_is_depth_bounded() {
        const TEMPLATE_USES: usize = 12_000;

        let workspace = WorkspaceId::new("project").unwrap();
        let source = json!({
            "info": {
                "name": "Wide credential template",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "auth": {
                "type": "bearer",
                "bearer": [{
                    "key": "token",
                    "value": "{{part}}".repeat(TEMPLATE_USES)
                }]
            },
            "variable": [{"key": "part", "value": "x"}],
            "item": []
        });

        let error = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap_err();
        assert!(error.contains("expansion nesting exceeds"), "{error}");
    }

    #[test]
    fn imported_encoded_bearer_credentials_are_scrubbed_from_typed_requests() {
        let workspace = WorkspaceId::new("project").unwrap();
        let secret = "actual bearer token!";
        let encoded = secret_redaction_variants(secret)
            .into_iter()
            .find(|variant| variant.contains('%'))
            .unwrap();
        let source = json!({
            "info": {
                "name": "Inherited Bearer",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
            },
            "auth": {
                "type": "bearer",
                "bearer": [{"key": "token", "value": secret}]
            },
            "item": [{
                "name": "Inherited request",
                "request": {
                    "method": "POST",
                    "url": format!("https://example.test/items?captured={encoded}"),
                    "body": {"mode": "raw", "raw": encoded}
                }
            }]
        });

        let imported = import(&workspace, &serde_json::to_vec(&source).unwrap()).unwrap();
        let imported_json = serde_json::to_string(&imported).unwrap();
        assert!(!imported_json.contains(secret), "{imported_json}");
        assert!(!imported_json.contains(&encoded), "{imported_json}");
        let prepared = PreparedCollectionExport::new(
            &imported.collection,
            &imported.folders,
            &imported.requests,
            &imported.environments,
            &imported.examples,
            &Secrets(HashMap::new()),
        )
        .unwrap();
        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains(secret), "{output}");
            assert!(!output.contains(&encoded), "{output}");
        }
    }

    #[test]
    fn agentops_import_sanitizes_credentials_in_extensions() {
        let source_workspace = WorkspaceId::new("source").unwrap();
        let target_workspace = WorkspaceId::new("target").unwrap();
        let mut collection = new_collection(&source_workspace, "Payments", "");
        collection.extensions.insert(
            "x-backup-token".into(),
            Value::String("collection-secret".into()),
        );
        let mut request =
            empty_request(&collection.id, "Create", "POST", "https://example.test").unwrap();
        request.extensions.insert(
            "x-private".into(),
            json!({"clientSecret":"request-secret", "region":"us-east-1"}),
        );
        let bundle = PortableBundle {
            version: 1,
            workspace_id: source_workspace,
            collection,
            folders: Vec::new(),
            requests: vec![request],
            environments: Vec::new(),
            examples: Vec::new(),
        };

        let imported = import(&target_workspace, &serde_json::to_vec(&bundle).unwrap()).unwrap();
        let serialized = serde_json::to_string(&imported).unwrap();
        assert!(!serialized.contains("collection-secret"));
        assert!(!serialized.contains("request-secret"));
        assert!(serialized.contains("<redacted>"));
        assert!(serialized.contains("us-east-1"));
    }

    #[test]
    fn agentops_import_replaces_every_supported_vault_reference() {
        let source_workspace = WorkspaceId::new("source").unwrap();
        let target_workspace = WorkspaceId::new("target").unwrap();
        let mut collection = new_collection(&source_workspace, "Payments", "");
        collection.auth = AuthConfig::Bearer {
            token: SecretRef::new("existing-workspace-vault-reference").unwrap(),
        };
        let request = empty_request(
            &collection.id,
            "Create",
            "POST",
            "https://attacker.example.test",
        )
        .unwrap();
        let environment_reference = SecretRef::new("existing-environment-vault-reference").unwrap();
        let bundle = PortableBundle {
            version: 1,
            workspace_id: source_workspace,
            collection,
            folders: Vec::new(),
            requests: vec![request],
            environments: vec![Environment {
                id: EnvironmentId::new(),
                workspace_id: target_workspace.clone(),
                name: "Imported environment".into(),
                base_url: String::new(),
                auth: AuthConfig::Bearer {
                    token: environment_reference.clone(),
                },
                variables: Vec::new(),
                active: false,
                extensions: Default::default(),
            }],
            examples: Vec::new(),
        };

        let imported = import(&target_workspace, &serde_json::to_vec(&bundle).unwrap()).unwrap();
        let AuthConfig::Bearer { token } = &imported.collection.auth else {
            panic!("bearer metadata should be preserved");
        };
        assert_ne!(token.as_str(), "existing-workspace-vault-reference");
        assert!(token.as_str().starts_with("imported-missing-"));
        let AuthConfig::Bearer { token } = &imported.environments[0].auth else {
            panic!("environment bearer metadata should be preserved");
        };
        assert_ne!(token, &environment_reference);
        assert!(token.as_str().starts_with("imported-missing-"));
    }

    #[test]
    fn deterministic_exports_use_compiler_redaction_for_url_body_and_extensions() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "API", "");
        let reference = SecretRef::new("vault-token").unwrap();
        collection.variables.push(Variable {
            id: super::super::RowId::new(),
            key: "token".into(),
            value: VariableValue::Secret(reference),
            enabled: true,
            description: String::new(),
        });
        collection.extensions.insert(
            "x-note".into(),
            Value::String("never print actual-secret here".into()),
        );
        let mut request = empty_request(
            &collection.id,
            "Create",
            "POST",
            "https://example.test/items?token={{token}}",
        )
        .unwrap();
        request.body = Body::Raw {
            media_type: RawBodyKind::Json,
            text: "{\"token\":\"{{token}}\"}".into(),
        };
        let secrets = Secrets(HashMap::from([(
            "vault-token".into(),
            "actual-secret".into(),
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
        let (_, snapshot) = compile_request(&request, Some(&collection), &context).unwrap();
        let example = Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: "Credential-bearing example".into(),
            request: Some(RedactedRequestSnapshot {
                method: "POST".into(),
                url: "https://example.test/items?token=actual-secret".into(),
                headers: vec![
                    ("Authorization".into(), "Bearer actual-secret".into()),
                    ("X-Trace".into(), "trace".into()),
                ],
                body: "{\"token\":\"actual-secret\"}".into(),
                ..RedactedRequestSnapshot::default()
            }),
            response: ResponseSnapshot {
                status: 201,
                reason: "Created".into(),
                headers: vec![("Set-Cookie".into(), "sid=actual-secret".into())],
                body_base64: base64::engine::general_purpose::STANDARD
                    .encode("{\"token\":\"actual-secret\"}"),
                duration_ms: 1,
                truncated: false,
                ..ResponseSnapshot::default()
            },
            extensions: Default::default(),
            sort_key: 0,
        };
        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            std::slice::from_ref(&request),
            &[],
            std::slice::from_ref(&example),
            &secrets,
        )
        .unwrap();
        let first = export_prepared_agentops_bundle(&prepared).unwrap();
        let second = export_prepared_agentops_bundle(&prepared).unwrap();
        assert_eq!(first, second);
        assert!(!first.contains("actual-secret"));
        assert!(first.contains("<redacted>"));
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        assert!(!postman.contains("actual-secret"));
        let postman: Value = serde_json::from_str(&postman).unwrap();
        assert_eq!(
            postman.pointer("/item/0/response/0/originalRequest/header/0/value"),
            Some(&json!("<redacted>"))
        );
        assert!(!export_curl(&snapshot).contains("actual-secret"));
    }

    #[test]
    fn compiled_list_exports_reject_even_redacted_basic_requests() {
        let workspace = WorkspaceId::new("project").unwrap();
        let password = SecretRef::new("basic-password").unwrap();
        let mut collection = new_collection(&workspace, "Compiled Basic", "");
        collection.auth = AuthConfig::Basic {
            username: "operator".into(),
            password: password.clone(),
        };
        let encoded =
            base64::engine::general_purpose::STANDARD.encode("operator:actual-basic-password!");
        let encoded_form = encoded.replace('=', "%3D");
        assert_ne!(encoded_form, encoded);
        collection.extensions.insert(
            "x-note".into(),
            Value::String(format!("captured Basic {encoded_form}")),
        );
        let mut request = empty_request(
            &collection.id,
            "Inherited Basic",
            "GET",
            "https://example.test",
        )
        .unwrap();
        request.auth = AuthConfig::Inherit;
        request.extensions.insert(
            "x-copy".into(),
            Value::String(format!("captured Basic {encoded_form}")),
        );
        let secrets = Secrets(HashMap::from([(
            password.as_str().to_string(),
            "actual-basic-password!".into(),
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
        let (compiled, _) = compile_request(&request, Some(&collection), &context).unwrap();
        let export = RedactedExportRequest::from_compiled(&request, &compiled).unwrap();

        let agentops_error = export_agentops_bundle(
            &workspace,
            &collection,
            &[],
            std::slice::from_ref(&export),
            &[],
            &[],
        )
        .unwrap_err();
        let postman_error =
            export_postman_collection(&collection, &[], std::slice::from_ref(&export), &[])
                .unwrap_err();
        for error in [agentops_error, postman_error] {
            assert!(error.contains("PreparedCollectionExport"), "{error}");
        }
    }

    #[test]
    fn compiled_list_exports_fail_closed_and_prepared_exports_redact_every_request_and_example() {
        let workspace = WorkspaceId::new("project").unwrap();
        let token = SecretRef::new("collection-token").unwrap();
        let request_token = SecretRef::new("request-token").unwrap();
        let mut collection = new_collection(&workspace, "Aggregate redactions", "");
        collection.auth = AuthConfig::Bearer {
            token: token.clone(),
        };
        let mut shadowing = empty_request(
            &collection.id,
            "Shadowing request",
            "POST",
            "https://example.test/items?captured=shadowed-secret",
        )
        .unwrap();
        shadowing.auth = AuthConfig::Bearer {
            token: request_token.clone(),
        };
        shadowing.body = Body::Raw {
            media_type: RawBodyKind::Text,
            text: "captured shadowed-secret".into(),
        };
        let mut second_shadowing = empty_request(
            &collection.id,
            "Second shadowing request",
            "POST",
            "https://example.test/other?captured=cross-request-secret",
        )
        .unwrap();
        second_shadowing.auth = AuthConfig::None;
        second_shadowing.body = Body::Raw {
            media_type: RawBodyKind::Text,
            text: "captured cross-request-secret".into(),
        };
        let example = Example {
            id: ExampleId::new(),
            request_id: second_shadowing.id.clone(),
            name: "Captured shadowed-secret and cross-request-secret".into(),
            request: None,
            response: ResponseSnapshot::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let secrets = Secrets(HashMap::from([
            (token.as_str().to_string(), "shadowed-secret".into()),
            (
                request_token.as_str().to_string(),
                "cross-request-secret".into(),
            ),
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
        let (shadowing_compiled, _) =
            compile_request(&shadowing, Some(&collection), &context).unwrap();
        let (second_compiled, _) =
            compile_request(&second_shadowing, Some(&collection), &context).unwrap();
        let requests = [
            RedactedExportRequest::from_compiled(&shadowing, &shadowing_compiled).unwrap(),
            RedactedExportRequest::from_compiled(&second_shadowing, &second_compiled).unwrap(),
        ];

        let agentops_error = export_agentops_bundle(
            &workspace,
            &collection,
            &[],
            &requests,
            &[],
            std::slice::from_ref(&example),
        )
        .unwrap_err();
        let postman_error =
            export_postman_collection(&collection, &[], &requests, std::slice::from_ref(&example))
                .unwrap_err();
        for error in [agentops_error, postman_error] {
            assert!(error.contains("PreparedCollectionExport"), "{error}");
        }

        let definitions = [shadowing, second_shadowing];
        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            &definitions,
            &[],
            std::slice::from_ref(&example),
            &secrets,
        )
        .unwrap();
        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains("shadowed-secret"), "{output}");
            assert!(!output.contains("cross-request-secret"), "{output}");
            assert!(output.contains("<redacted>"), "{output}");
        }
    }

    #[test]
    fn named_vault_templates_are_included_in_export_redaction() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Named vault", "");
        collection.description = "This copied response contains vault-export-sentinel".into();
        let mut request = empty_request(
            &collection.id,
            "Login",
            "POST",
            "https://api.test/{{vault.token}}",
        )
        .unwrap();
        request.auth = AuthConfig::Basic {
            username: "{{vault.user}}".into(),
            password: crate::vault::vault_secret_reference("password").unwrap(),
        };
        let secrets = Secrets(HashMap::from([
            (
                crate::vault::vault_secret_reference("token")
                    .unwrap()
                    .as_str()
                    .into(),
                "vault-export-sentinel".into(),
            ),
            (
                crate::vault::vault_secret_reference("user")
                    .unwrap()
                    .as_str()
                    .into(),
                "vault-user-value".into(),
            ),
            (
                crate::vault::vault_secret_reference("password")
                    .unwrap()
                    .as_str()
                    .into(),
                "vault-password-value".into(),
            ),
        ]));
        let prepared =
            PreparedCollectionExport::new(&collection, &[], &[request], &[], &[], &secrets)
                .unwrap();
        for output in [
            export_prepared_agentops_bundle(&prepared).unwrap(),
            export_prepared_postman_collection(&prepared).unwrap(),
        ] {
            assert!(!output.contains("vault-export-sentinel"));
            assert!(!output.contains("vault-user-value"));
            assert!(!output.contains("vault-password-value"));
            assert!(output.contains("{{vault.token}}"));
        }
    }

    #[test]
    fn prepared_exports_do_not_redact_typed_discriminants_for_short_secrets() {
        let workspace = WorkspaceId::new("project").unwrap();
        let token = SecretRef::new("short-token").unwrap();
        let mut collection = new_collection(&workspace, "Short secret", "");
        collection.auth = AuthConfig::Bearer {
            token: token.clone(),
        };
        let secrets = Secrets(HashMap::from([(token.as_str().to_string(), "a".into())]));
        let mut request = empty_request(&collection.id, "X", "POST", "https://xyz.test").unwrap();
        request.auth = AuthConfig::Bearer {
            token: token.clone(),
        };
        request.body = Body::Raw {
            media_type: RawBodyKind::Json,
            text: "a".into(),
        };
        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            std::slice::from_ref(&request),
            &[],
            &[],
            &secrets,
        )
        .unwrap();

        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let bundle: PortableBundle = serde_json::from_str(&agentops).unwrap();
        assert!(matches!(bundle.collection.auth, AuthConfig::Bearer { .. }));
        assert!(matches!(
            &bundle.requests[0],
            SavedRequest {
                auth: AuthConfig::Bearer { .. },
                body: Body::Raw {
                    media_type: RawBodyKind::Json,
                    text,
                },
                ..
            } if text == "<redacted>"
        ));
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        let postman: Value = serde_json::from_str(&postman).unwrap();
        assert_eq!(postman.pointer("/auth/type"), Some(&json!("bearer")));
        assert_eq!(
            postman.pointer("/item/0/request/auth/type"),
            Some(&json!("bearer"))
        );
        assert_eq!(
            postman.pointer("/item/0/request/body/mode"),
            Some(&json!("raw"))
        );
        assert_eq!(
            postman.pointer("/item/0/request/body/options/raw/language"),
            Some(&json!("json"))
        );
        assert_eq!(
            postman.pointer("/item/0/request/body/raw"),
            Some(&json!("<redacted>"))
        );
    }

    #[test]
    fn prepared_exports_discard_redactions_after_sanitizing_each_request() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = new_collection(&workspace, "Shared redactions", "");
        let mut secret_values = HashMap::new();
        let requests = (0..3)
            .map(|index| {
                let reference = SecretRef::new(format!("request-token-{index}")).unwrap();
                secret_values.insert(
                    reference.as_str().to_string(),
                    format!("distinct-secret-{index}"),
                );
                let mut request = empty_request(
                    &collection.id,
                    format!("Request {index}"),
                    "GET",
                    "https://example.test",
                )
                .unwrap();
                request.auth = AuthConfig::Bearer { token: reference };
                request
            })
            .collect::<Vec<_>>();
        let secrets = Secrets(secret_values);

        let prepared =
            PreparedCollectionExport::new(&collection, &[], &requests, &[], &[], &secrets).unwrap();
        assert_eq!(prepared.redaction_count, requests.len());
        assert!(
            prepared
                .requests
                .iter()
                .all(|request| request.redactions.is_empty())
        );
    }

    #[test]
    fn prepared_exports_own_the_complete_graph_after_source_mutation() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection =
            new_collection(&workspace, "Original collection", "Original description");
        let mut folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Original folder".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let mut request = empty_request(
            &collection.id,
            "Original request",
            "GET",
            "https://example.test/original",
        )
        .unwrap();
        request.folder_id = Some(folder.id.clone());
        let mut environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace,
            name: "Original environment".into(),
            base_url: "https://environment.example.test".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        let mut example = Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: "Original example".into(),
            request: None,
            response: ResponseSnapshot::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let prepared = PreparedCollectionExport::new(
            &collection,
            std::slice::from_ref(&folder),
            std::slice::from_ref(&request),
            std::slice::from_ref(&environment),
            std::slice::from_ref(&example),
            &Secrets(HashMap::new()),
        )
        .unwrap();

        let late_secret = "post-prepare-source-mutation-secret";
        collection.name = late_secret.into();
        collection
            .extensions
            .insert("late".into(), Value::String(late_secret.into()));
        folder.name = late_secret.into();
        request.url = format!("https://example.test/{late_secret}");
        environment.name = late_secret.into();
        example.name = late_secret.into();

        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [&agentops, &postman] {
            assert!(!output.contains(late_secret), "{output}");
            assert!(output.contains("Original collection"), "{output}");
            assert!(output.contains("Original folder"), "{output}");
            assert!(output.contains("Original request"), "{output}");
            assert!(output.contains("Original example"), "{output}");
        }
        let bundle: PortableBundle = serde_json::from_str(&agentops).unwrap();
        assert_eq!(bundle.environments[0].name, "Original environment");
    }

    #[test]
    fn prepared_export_supports_the_maximum_request_count() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = new_collection(&workspace, "Maximum collection", "");
        let requests = (0..MAX_IMPORTED_REQUESTS)
            .map(|index| {
                let mut request = empty_request(
                    &collection.id,
                    format!("Request {index}"),
                    "GET",
                    format!("https://example.test/{index}"),
                )
                .unwrap();
                request.auth = AuthConfig::Bearer {
                    token: SecretRef::new(format!("token-{index:05}")).unwrap(),
                };
                request
            })
            .collect::<Vec<_>>();

        let prepared =
            PreparedCollectionExport::new(&collection, &[], &requests, &[], &[], &DerivedSecrets)
                .unwrap();
        assert_eq!(prepared.requests.len(), MAX_IMPORTED_REQUESTS);
        assert_eq!(prepared.redaction_count, MAX_IMPORTED_REQUESTS);
        assert!(
            prepared
                .requests
                .iter()
                .all(|request| request.redactions.is_empty())
        );

        let agentops: PortableBundle =
            serde_json::from_str(&export_prepared_agentops_bundle(&prepared).unwrap()).unwrap();
        assert_eq!(agentops.requests.len(), MAX_IMPORTED_REQUESTS);
        let postman: Value =
            serde_json::from_str(&export_prepared_postman_collection(&prepared).unwrap()).unwrap();
        assert_eq!(
            postman
                .pointer("/item")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(MAX_IMPORTED_REQUESTS)
        );
    }

    #[test]
    fn prepared_exports_preserve_portable_requests_that_are_not_sendable() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Portable API", "");
        collection.auth = AuthConfig::Unsupported {
            name: "legacy".into(),
            raw: json!({"legacy": [{"key": "mode", "value": "portable"}]}),
        };
        let mut request = empty_request(
            &collection.id,
            "Relative request",
            "GET",
            "/api/{{missing_id}}",
        )
        .unwrap();
        request.auth = AuthConfig::Bearer {
            token: SecretRef::new("imported-missing-token").unwrap(),
        };
        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            std::slice::from_ref(&request),
            &[],
            &[],
            &Secrets(HashMap::new()),
        )
        .unwrap();

        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let agentops: PortableBundle = serde_json::from_str(&agentops).unwrap();
        assert_eq!(agentops.requests[0].url, "/api/{{missing_id}}");
        assert!(matches!(
            agentops.collection.auth,
            AuthConfig::Unsupported { .. }
        ));
        assert!(matches!(
            agentops.requests[0].auth,
            AuthConfig::Bearer { .. }
        ));

        let postman = export_prepared_postman_collection(&prepared).unwrap();
        let postman: Value = serde_json::from_str(&postman).unwrap();
        assert_eq!(
            postman.pointer("/item/0/request/url/raw"),
            Some(&json!("/api/{{missing_id}}"))
        );
    }

    #[test]
    fn prepared_exports_redact_resolved_secrets_even_for_an_empty_collection() {
        let workspace = WorkspaceId::new("project").unwrap();
        let reference = SecretRef::new("vault-token").unwrap();
        let mut collection = new_collection(&workspace, "Empty API", "");
        collection.variables.push(Variable {
            id: super::super::RowId::new(),
            key: "token".into(),
            value: VariableValue::Secret(reference),
            enabled: true,
            description: String::new(),
        });
        collection.extensions.insert(
            "x-note".into(),
            Value::String("copied actual-secret here".into()),
        );
        let secrets = Secrets(HashMap::from([(
            "vault-token".into(),
            "actual-secret".into(),
        )]));
        let prepared =
            PreparedCollectionExport::new(&collection, &[], &[], &[], &[], &secrets).unwrap();

        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains("actual-secret"), "{output}");
            assert!(output.contains("<redacted>"), "{output}");
        }
    }

    #[test]
    fn prepared_exports_redact_secrets_that_contain_an_existing_marker() {
        let workspace = WorkspaceId::new("project").unwrap();
        let reference = SecretRef::new("marker-bearing-token").unwrap();
        let secret = "left<redacted>right";
        let mut collection = new_collection(&workspace, "Marker-bearing secret", "");
        collection.auth = AuthConfig::Bearer {
            token: reference.clone(),
        };
        collection.extensions.insert(
            "x-note".into(),
            Value::String(format!("captured {secret} here")),
        );
        let secrets = Secrets(HashMap::from([(
            reference.as_str().to_string(),
            secret.into(),
        )]));
        let prepared =
            PreparedCollectionExport::new(&collection, &[], &[], &[], &[], &secrets).unwrap();

        for output in [
            export_prepared_agentops_bundle(&prepared).unwrap(),
            export_prepared_postman_collection(&prepared).unwrap(),
        ] {
            assert!(!output.contains(secret), "{output}");
            assert!(output.contains("<redacted>"), "{output}");
        }
    }

    #[test]
    fn prepared_exports_redact_resolvable_imported_missing_references() {
        let workspace = WorkspaceId::new("project").unwrap();
        let reference = SecretRef::new("imported-missing-live-token").unwrap();
        let mut collection = new_collection(&workspace, "Recovered imported secret", "");
        collection.auth = AuthConfig::Bearer {
            token: reference.clone(),
        };
        collection.extensions.insert(
            "x-note".into(),
            Value::String("copied recovered-secret here".into()),
        );
        let secrets = Secrets(HashMap::from([(
            reference.as_str().to_string(),
            "recovered-secret".into(),
        )]));
        let prepared =
            PreparedCollectionExport::new(&collection, &[], &[], &[], &[], &secrets).unwrap();

        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains("recovered-secret"), "{output}");
            assert!(output.contains("<redacted>"), "{output}");
        }
    }

    #[test]
    fn prepared_exports_resolve_each_basic_password_reference_once() {
        let workspace = WorkspaceId::new("project").unwrap();
        let password = SecretRef::new("shared-basic-password").unwrap();
        let mut collection = new_collection(&workspace, "Cached Basic", "");
        collection.auth = AuthConfig::Basic {
            username: "operator".into(),
            password: password.clone(),
        };
        let requests = (0..20)
            .map(|index| {
                let mut request = empty_request(
                    &collection.id,
                    format!("Request {index}"),
                    "GET",
                    "https://example.test",
                )
                .unwrap();
                request.auth = AuthConfig::Inherit;
                request
            })
            .collect::<Vec<_>>();
        let environments = (0..10)
            .map(|index| Environment {
                id: EnvironmentId::new(),
                workspace_id: workspace.clone(),
                name: format!("Environment {index}"),
                base_url: String::new(),
                auth: AuthConfig::None,
                variables: Vec::new(),
                active: false,
                extensions: Default::default(),
            })
            .collect::<Vec<_>>();
        let secrets = CountingSecrets {
            reference: password.as_str().to_string(),
            value: "actual-basic-password".into(),
            calls: AtomicUsize::new(0),
        };

        PreparedCollectionExport::new(&collection, &[], &requests, &environments, &[], &secrets)
            .unwrap();
        assert_eq!(secrets.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn prepared_exports_redact_derived_basic_auth_credentials() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Basic API", "");
        collection.auth = AuthConfig::Basic {
            username: "operator".into(),
            password: SecretRef::new("basic-password").unwrap(),
        };
        let encoded_credential =
            base64::engine::general_purpose::STANDARD.encode("operator:actual-basic-password!");
        let environment_encoded_credential = base64::engine::general_purpose::STANDARD
            .encode("environment-operator:actual-basic-password!");
        let encoded_form_credential = encoded_credential.replace('=', "%3D");
        let environment_encoded_form_credential =
            environment_encoded_credential.replace('=', "%3D");
        let encoded_lowercase_form_credential = encoded_credential.replace('=', "%3d");
        let encoded_mixed_case_form_credential = encoded_credential
            .replacen('=', "%3d", 1)
            .replace('=', "%3D");
        let lowercased_payload_lookalike = encoded_form_credential.to_ascii_lowercase();
        assert_ne!(encoded_form_credential, encoded_credential);
        assert_ne!(
            environment_encoded_form_credential,
            environment_encoded_credential
        );
        let captured_wire = format!(
            "Basic {encoded_credential}; Basic {encoded_form_credential}; Basic {encoded_lowercase_form_credential}; Basic {encoded_mixed_case_form_credential}; lookalike {lowercased_payload_lookalike}; Basic {environment_encoded_credential}; Basic {environment_encoded_form_credential}"
        );
        collection
            .extensions
            .insert("x-note".into(), Value::String(captured_wire.clone()));
        let persistence_safe = super::super::persistence_safe_collection(&collection);
        assert_eq!(
            persistence_safe
                .extensions
                .get("x-note")
                .and_then(Value::as_str),
            Some(captured_wire.as_str())
        );
        let secrets = Secrets(HashMap::from([(
            "basic-password".into(),
            "actual-basic-password!".into(),
        )]));
        let environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Active".into(),
            base_url: String::new(),
            auth: AuthConfig::Basic {
                username: "environment-operator".into(),
                password: SecretRef::new("basic-password").unwrap(),
            },
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            &[],
            std::slice::from_ref(&environment),
            &[],
            &secrets,
        )
        .unwrap();
        assert!(prepared.redaction_count >= 2);

        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains(&encoded_credential), "{output}");
            assert!(!output.contains(&encoded_form_credential), "{output}");
            assert!(
                !output.contains(&encoded_lowercase_form_credential),
                "{output}"
            );
            assert!(
                !output.contains(&encoded_mixed_case_form_credential),
                "{output}"
            );
            assert!(output.contains(&lowercased_payload_lookalike), "{output}");
            assert!(
                !output.contains(&environment_encoded_credential),
                "{output}"
            );
            assert!(
                !output.contains(&environment_encoded_form_credential),
                "{output}"
            );
            assert!(output.contains("<redacted>"), "{output}");
        }
    }

    #[test]
    fn basic_login_exports_redact_credentials_and_imports_clear_vault_access() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = new_collection(&workspace, "Basic login API", "");
        let mut request = empty_request(&collection.id, "Protected", "GET", "/items").unwrap();
        request.auth = AuthConfig::Login {
            url: "https://auth.example.test/login".into(),
            headers: Vec::new(),
            basic: Some(crate::BasicLoginCredentials {
                username: "operator".into(),
                password: SecretRef::new("login-password").unwrap(),
            }),
            method: "POST".into(),
            body: String::new(),
            token_path: "data.token".into(),
            ttl_secs: Some(600),
            access_token: Some(SecretRef::new("login-token").unwrap()),
            expires_at: Some(1234),
        };
        let credential =
            base64::engine::general_purpose::STANDARD.encode("operator:secret-password!");
        request.extensions.insert(
            "note".into(),
            json!(format!("{credential} secret-password! cached-token")),
        );
        let secrets = Secrets(HashMap::from([
            ("login-password".into(), "secret-password!".into()),
            ("login-token".into(), "cached-token".into()),
        ]));
        let prepared =
            PreparedCollectionExport::new(&collection, &[], &[request], &[], &[], &secrets)
                .unwrap();
        let native = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [&native, &postman] {
            assert!(!output.contains("secret-password!"));
            assert!(!output.contains("cached-token"));
            assert!(!output.contains(&credential));
        }
        let imported = import(&workspace, native.as_bytes()).unwrap();
        let AuthConfig::Login {
            url,
            basic: Some(basic),
            token_path,
            access_token,
            expires_at,
            ..
        } = &imported.requests[0].auth
        else {
            panic!("Basic login must survive native export/import");
        };
        assert_eq!(url, "https://auth.example.test/login");
        assert_eq!(basic.username, "operator");
        assert!(basic.password.as_str().starts_with("imported-missing-"));
        assert_eq!(token_path, "data.token");
        assert!(access_token.is_none());
        assert!(expires_at.is_none());
    }

    #[test]
    fn prepared_exports_redact_mixed_escape_examples_and_freeform_keys() {
        let workspace = WorkspaceId::new("project").unwrap();
        let password = SecretRef::new("basic-password").unwrap();
        let mut collection = new_collection(&workspace, "Free-form redaction", "");
        collection.auth = AuthConfig::Basic {
            username: "operator".into(),
            password: password.clone(),
        };
        let encoded =
            base64::engine::general_purpose::STANDARD.encode("operator:actual-basic-password!");
        let encoded_form = encoded.replace('=', "%3D");
        let mixed_escape = encoded.replacen('=', "%3d", 1).replace('=', "%3D");
        assert_ne!(mixed_escape, encoded);

        collection
            .extensions
            .insert(format!("collection-{encoded_form}"), json!({"kept": 1}));
        collection
            .extensions
            .insert("collection-<redacted>".into(), json!({"kept": 2}));
        let mut folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Folder".into(),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        folder
            .extensions
            .insert(format!("folder-{mixed_escape}"), json!(true));
        let mut request = empty_request(
            &collection.id,
            "Inherited Basic",
            "GET",
            "https://example.test",
        )
        .unwrap();
        request.folder_id = Some(folder.id.clone());
        request.auth = AuthConfig::Inherit;
        request.extensions.insert(
            "nested".into(),
            Value::Object(serde_json::Map::from_iter([(
                format!("request-{mixed_escape}"),
                json!("kept"),
            )])),
        );
        let example = Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: format!("Captured {mixed_escape}"),
            request: None,
            response: ResponseSnapshot::default(),
            extensions: serde_json::Map::from_iter([(
                "outer".into(),
                Value::Object(serde_json::Map::from_iter([(
                    format!("example-{mixed_escape}"),
                    json!("kept"),
                )])),
            )]),
            sort_key: 0,
        };
        let secrets = Secrets(HashMap::from([(
            password.as_str().to_string(),
            "actual-basic-password!".into(),
        )]));
        let prepared = PreparedCollectionExport::new(
            &collection,
            std::slice::from_ref(&folder),
            std::slice::from_ref(&request),
            &[],
            std::slice::from_ref(&example),
            &secrets,
        )
        .unwrap();

        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            assert!(!output.contains(&encoded), "{output}");
            assert!(!output.contains(&encoded_form), "{output}");
            assert!(!output.contains(&mixed_escape), "{output}");
            assert!(output.contains("<redacted>"), "{output}");
            assert!(output.contains("collection-<redacted>#2"), "{output}");
        }
    }

    fn assert_export_omits(prepared: &PreparedCollectionExport, credentials: &[&str]) {
        let outputs = [
            export_prepared_postman_collection(prepared).unwrap(),
            export_prepared_agentops_bundle(prepared).unwrap(),
        ];
        for output in outputs {
            for credential in credentials {
                assert!(
                    !output.contains(*credential),
                    "{credential} leaked: {output}"
                );
            }
        }
    }

    #[test]
    fn prepared_exports_keep_unresolvable_secrets_as_references() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Vault API", "");
        collection.auth = AuthConfig::Bearer {
            token: SecretRef::new("workbench.never-fetched.token").unwrap(),
        };
        collection.variables.push(Variable {
            id: RowId::new(),
            key: "key".into(),
            value: VariableValue::Secret(SecretRef::new("deleted-from-vault").unwrap()),
            enabled: true,
            description: String::new(),
        });
        let mut request = empty_request(
            &collection.id,
            "Vault request",
            "GET",
            "https://example.test/{{vault.missing}}/{{vault.bad name}}",
        )
        .unwrap();
        request.auth = AuthConfig::Inherit;
        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            std::slice::from_ref(&request),
            &[],
            &[],
            &Secrets(HashMap::new()),
        )
        .unwrap();
        let output = export_prepared_agentops_bundle(&prepared).unwrap();
        assert!(output.contains("{{vault.missing}}"), "{output}");
        export_prepared_postman_collection(&prepared).unwrap();
    }

    #[test]
    fn prepared_exports_allow_runtime_scoped_basic_auth_usernames() {
        let workspace = WorkspaceId::new("project").unwrap();
        let password = SecretRef::new("runtime-basic-password").unwrap();
        let mut collection = new_collection(&workspace, "Runtime Basic API", "");
        collection.auth = AuthConfig::Basic {
            username: "{{user}}".into(),
            password: password.clone(),
        };
        collection.variables.push(Variable {
            id: RowId::new(),
            key: "user".into(),
            value: VariableValue::Plain("stored-user".into()),
            enabled: true,
            description: String::new(),
        });
        let mut request = empty_request(
            &collection.id,
            "Runtime auth",
            "GET",
            "https://example.test/auth-check",
        )
        .unwrap();
        request.auth = AuthConfig::Inherit;
        let secrets = Secrets(HashMap::from([(
            password.as_str().to_string(),
            "actual-basic-password".into(),
        )]));

        for (_scope_name, data, local, runtime_user) in [
            (
                "data",
                vec![Variable {
                    id: RowId::new(),
                    key: "user".into(),
                    value: VariableValue::Plain("data-user".into()),
                    enabled: true,
                    description: String::new(),
                }],
                Vec::new(),
                "data-user",
            ),
            (
                "local",
                Vec::new(),
                vec![Variable {
                    id: RowId::new(),
                    key: "user".into(),
                    value: VariableValue::Plain("local-user".into()),
                    enabled: true,
                    description: String::new(),
                }],
                "local-user",
            ),
        ] {
            let context = CompileContext {
                global: &[],
                environment: &[],
                data: &data,
                local: &local,
                secrets: &secrets,
                environment_base_url: None,
                environment_auth: None,
            };
            let (compiled, _) = compile_request(&request, Some(&collection), &context).unwrap();
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(format!("{runtime_user}:actual-basic-password"));
            assert!(compiled.headers.iter().any(|(name, value)| {
                name == "Authorization" && value == &format!("Basic {encoded}")
            }));

            let prepared = PreparedCollectionExport::new(
                &collection,
                &[],
                std::slice::from_ref(&request),
                &[],
                &[],
                &secrets,
            )
            .unwrap();
            assert_export_omits(&prepared, &[]);
        }
    }

    #[test]
    fn prepared_exports_redact_each_templated_basic_auth_owner() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Owned Basic API", "");
        let password = SecretRef::new("shared-basic-password").unwrap();
        collection.auth = AuthConfig::Basic {
            username: "{{user}}".into(),
            password: password.clone(),
        };
        collection.variables.push(Variable {
            id: RowId::new(),
            key: "user".into(),
            value: VariableValue::Plain("collection-user".into()),
            enabled: true,
            description: String::new(),
        });
        let folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Owned folder".into(),
            auth: AuthConfig::Basic {
                username: "{{user}}".into(),
                password: password.clone(),
            },
            variables: vec![Variable {
                id: RowId::new(),
                key: "user".into(),
                value: VariableValue::Plain("folder-user".into()),
                enabled: true,
                description: String::new(),
            }],
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let mut request = empty_request(
            &collection.id,
            "Inherited folder auth",
            "GET",
            "https://example.test/auth-check",
        )
        .unwrap();
        request.folder_id = Some(folder.id.clone());
        request.auth = AuthConfig::Inherit;
        let collection_credential = base64::engine::general_purpose::STANDARD
            .encode("collection-user:actual-basic-password");
        let folder_credential =
            base64::engine::general_purpose::STANDARD.encode("folder-user:actual-basic-password");
        collection.extensions.insert(
            "x-note".into(),
            Value::String(format!(
                "Basic {collection_credential}; Basic {folder_credential}"
            )),
        );
        let secrets = Secrets(HashMap::from([(
            password.as_str().to_string(),
            "actual-basic-password".into(),
        )]));

        let prepared = PreparedCollectionExport::new(
            &collection,
            std::slice::from_ref(&folder),
            std::slice::from_ref(&request),
            &[],
            &[],
            &secrets,
        )
        .unwrap();
        assert_export_omits(&prepared, &[&collection_credential, &folder_credential]);
    }

    #[test]
    fn prepared_exports_redact_templated_basic_auth_with_inactive_environments() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Inactive environment API", "");
        let password = SecretRef::new("basic-password").unwrap();
        collection.auth = AuthConfig::Basic {
            username: "{{user}}".into(),
            password: password.clone(),
        };
        collection.variables.push(Variable {
            id: RowId::new(),
            key: "user".into(),
            value: VariableValue::Plain("no-environment-user".into()),
            enabled: true,
            description: String::new(),
        });
        let mut request = empty_request(
            &collection.id,
            "No active environment",
            "GET",
            "https://example.test/auth-check",
        )
        .unwrap();
        request.auth = AuthConfig::Inherit;
        let environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Inactive".into(),
            base_url: String::new(),
            auth: AuthConfig::None,
            variables: vec![Variable {
                id: RowId::new(),
                key: "user".into(),
                value: VariableValue::Plain("inactive-user".into()),
                enabled: true,
                description: String::new(),
            }],
            active: false,
            extensions: Default::default(),
        };
        let no_environment_credential = base64::engine::general_purpose::STANDARD
            .encode("no-environment-user:actual-basic-password");
        let inactive_environment_credential =
            base64::engine::general_purpose::STANDARD.encode("inactive-user:actual-basic-password");
        collection.extensions.insert(
            "x-note".into(),
            Value::String(format!(
                "Basic {no_environment_credential}; Basic {inactive_environment_credential}"
            )),
        );
        let secrets = Secrets(HashMap::from([(
            password.as_str().to_string(),
            "actual-basic-password".into(),
        )]));

        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            std::slice::from_ref(&request),
            std::slice::from_ref(&environment),
            &[],
            &secrets,
        )
        .unwrap();
        assert_export_omits(
            &prepared,
            &[&no_environment_credential, &inactive_environment_credential],
        );
    }

    #[test]
    fn prepared_exports_redact_templated_inherited_basic_auth_credentials() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Inherited Basic API", "");
        let password = SecretRef::new("environment-basic-password").unwrap();
        let environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Active".into(),
            base_url: String::new(),
            auth: AuthConfig::Basic {
                username: "{{user}}".into(),
                password: password.clone(),
            },
            variables: vec![Variable {
                id: RowId::new(),
                key: "user".into(),
                value: VariableValue::Plain("environment-user".into()),
                enabled: true,
                description: String::new(),
            }],
            active: true,
            extensions: Default::default(),
        };
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: collection.id.clone(),
            folder_id: None,
            name: "Inherited auth".into(),
            method: HttpMethod::get(),
            url: "https://example.test/auth-check".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::Inherit,
            body: Body::None,
            variables: vec![Variable {
                id: RowId::new(),
                key: "user".into(),
                value: VariableValue::Plain("request-user".into()),
                enabled: true,
                description: String::new(),
            }],
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let encoded = base64::engine::general_purpose::STANDARD
            .encode("request-user:actual-environment-password");
        let encoded_form = encoded.replace('=', "%3D");
        assert_ne!(encoded_form, encoded);
        collection.extensions.insert(
            "x-captured-wire".into(),
            Value::String(format!("Basic {encoded}; Basic {encoded_form}")),
        );
        let secrets = Secrets(HashMap::from([(
            password.as_str().to_string(),
            "actual-environment-password".into(),
        )]));
        let empty = [];
        let context = CompileContext {
            global: &empty,
            environment: &environment.variables,
            data: &empty,
            local: &empty,
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: Some(&environment.auth),
        };
        let (compiled, _) = compile_request(&request, Some(&collection), &context).unwrap();
        assert!(compiled.headers.iter().any(|(name, value)| {
            name == "Authorization" && value == &format!("Basic {encoded}")
        }));

        let prepared = PreparedCollectionExport::new(
            &collection,
            &[],
            std::slice::from_ref(&request),
            std::slice::from_ref(&environment),
            &[],
            &secrets,
        )
        .unwrap();
        assert_export_omits(&prepared, &[&encoded, &encoded_form]);
    }

    #[test]
    fn prepared_exports_allow_templated_basic_auth_without_a_scope() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Empty templated Basic API", "");
        collection.auth = AuthConfig::Basic {
            username: "{{user}}".into(),
            password: SecretRef::new("basic-password").unwrap(),
        };
        let secrets = Secrets(HashMap::from([(
            "basic-password".into(),
            "actual-basic-password".into(),
        )]));

        let prepared =
            PreparedCollectionExport::new(&collection, &[], &[], &[], &[], &secrets).unwrap();
        assert_export_omits(&prepared, &[]);
    }

    #[test]
    fn prepared_collection_export_debug_reports_counts_without_secrets() {
        let workspace = WorkspaceId::new("project").unwrap();
        let reference = SecretRef::new("vault-token").unwrap();
        let mut collection = new_collection(&workspace, "Debug-safe API", "");
        collection.variables.push(Variable {
            id: super::super::RowId::new(),
            key: "token".into(),
            value: VariableValue::Secret(reference),
            enabled: true,
            description: String::new(),
        });
        let secrets = Secrets(HashMap::from([(
            "vault-token".into(),
            "debug-must-not-print-this-secret".into(),
        )]));
        let prepared =
            PreparedCollectionExport::new(&collection, &[], &[], &[], &[], &secrets).unwrap();

        let debug = format!("{prepared:?}");
        assert!(
            !debug.contains("debug-must-not-print-this-secret"),
            "{debug}"
        );
        assert!(debug.contains("request_count: 0"), "{debug}");
        assert!(debug.contains("redaction_count:"), "{debug}");
    }

    #[test]
    fn exports_apply_container_and_templated_url_persistence_safety() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "API", "");
        collection.variables.push(Variable {
            id: super::super::RowId::new(),
            key: "api_token".into(),
            value: VariableValue::Plain("literal-collection-export-secret".into()),
            enabled: true,
            description: String::new(),
        });
        collection.extensions.insert(
            "x-auth".into(),
            json!({"value":"literal-extension-export-secret"}),
        );
        let folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Folder".into(),
            auth: AuthConfig::None,
            variables: vec![Variable {
                id: super::super::RowId::new(),
                key: "client_secret".into(),
                value: VariableValue::Plain("literal-folder-export-secret".into()),
                enabled: true,
                description: String::new(),
            }],
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let request = empty_request(
            &collection.id,
            "Templated",
            "GET",
            "{{base_url}}?api_key=literal-query-export-secret",
        )
        .unwrap();
        let secrets = Secrets(HashMap::new());
        let environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Environment".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: vec![Variable {
                id: super::super::RowId::new(),
                key: "password".into(),
                value: VariableValue::Plain("literal-environment-export-secret".into()),
                enabled: true,
                description: String::new(),
            }],
            active: false,
            extensions: Default::default(),
        };
        let prepared = PreparedCollectionExport::new(
            &collection,
            std::slice::from_ref(&folder),
            std::slice::from_ref(&request),
            std::slice::from_ref(&environment),
            &[],
            &secrets,
        )
        .unwrap();
        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let postman = export_prepared_postman_collection(&prepared).unwrap();
        for output in [agentops, postman] {
            for secret in [
                "literal-collection-export-secret",
                "literal-extension-export-secret",
                "literal-folder-export-secret",
                "literal-query-export-secret",
                "literal-environment-export-secret",
            ] {
                assert!(
                    !output.contains(secret),
                    "export retained {secret:?}: {output}"
                );
            }
        }

        let unsafe_example = Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: "Unsafe templated authority".into(),
            request: Some(RedactedRequestSnapshot {
                method: "GET".into(),
                url: "https://user:literal-password@{{host}}/items".into(),
                ..RedactedRequestSnapshot::default()
            }),
            response: ResponseSnapshot::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let error = PreparedCollectionExport::new(
            &collection,
            &[],
            std::slice::from_ref(&request),
            &[],
            std::slice::from_ref(&unsafe_example),
            &secrets,
        )
        .unwrap_err();
        assert!(error.contains("user information"), "{error}");
    }

    #[test]
    fn exports_preserve_request_structure_hierarchy_auth_and_variables() {
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = new_collection(&workspace, "Structured API", "");
        collection.auth = AuthConfig::Basic {
            username: "operator".into(),
            password: SecretRef::new("basic-password").unwrap(),
        };
        collection.variables.push(Variable {
            id: super::super::RowId::new(),
            key: "host".into(),
            value: VariableValue::Plain("api.example.test".into()),
            enabled: true,
            description: "API host".into(),
        });
        let folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Users".into(),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let mut request = empty_request(
            &collection.id,
            "Create user",
            "POST",
            "https://{{host}}/users",
        )
        .unwrap();
        request.folder_id = Some(folder.id.clone());
        request.auth = AuthConfig::Inherit;
        request.params = vec![KeyValueRow::enabled("dry_run", "true")];
        request.variables.push(Variable {
            id: super::super::RowId::new(),
            key: "role".into(),
            value: VariableValue::Plain("admin".into()),
            enabled: true,
            description: String::new(),
        });
        request.body = Body::UrlEncoded {
            rows: vec![KeyValueRow::enabled("role", "{{role}}")],
        };
        let secrets = Secrets(HashMap::from([(
            "basic-password".into(),
            "actual-basic-password".into(),
        )]));
        let prepared = PreparedCollectionExport::new(
            &collection,
            std::slice::from_ref(&folder),
            std::slice::from_ref(&request),
            &[],
            &[],
            &secrets,
        )
        .unwrap();
        let agentops = export_prepared_agentops_bundle(&prepared).unwrap();
        let bundle: PortableBundle = serde_json::from_str(&agentops).unwrap();
        assert_eq!(bundle.folders, vec![folder.clone()]);
        assert_eq!(bundle.requests[0].params, request.params);
        assert_eq!(bundle.requests[0].variables, request.variables);
        assert_eq!(bundle.requests[0].auth, AuthConfig::Inherit);
        assert_eq!(bundle.requests[0].body, request.body);
        assert!(!agentops.contains("actual-basic-password"));

        let postman = export_prepared_postman_collection(&prepared).unwrap();
        let postman: Value = serde_json::from_str(&postman).unwrap();
        assert_eq!(postman.pointer("/auth/type"), Some(&json!("basic")));
        assert_eq!(postman.pointer("/variable/0/key"), Some(&json!("host")));
        assert_eq!(
            postman.pointer("/item/0/item/0/request/url/query/0/key"),
            Some(&json!("dry_run"))
        );
        assert_eq!(
            postman.pointer("/item/0/item/0/request/body/mode"),
            Some(&json!("urlencoded"))
        );
        assert_eq!(
            postman.pointer("/item/0/item/0/variable/0/key"),
            Some(&json!("role"))
        );
        assert!(!postman.to_string().contains("actual-basic-password"));
    }

    #[test]
    fn postman_import_retains_unsupported_shapes_and_sanitizes_example_credentials() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br#"{
              "info":{"name":"Legacy","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
              "item":[{"name":"Legacy request","request":{
                "method":"POST","url":{"raw":"https://example.test","x-url":{"nested":true}},
                "auth":{"type":"digest","digest":[{"key":"password","value":"actual-auth-secret"}]},
                "body":{"mode":"mystery","payload":{"x-nested":{"kept":true},"copy":"actual-auth-secret"}}
              },"response":[{"name":"Example","code":200,"status":"OK","header":[],"body":"ok",
                "originalRequest":{"method":"GET","url":"https://example.test?copy=actual-auth-secret","header":[
                  {"key":"Authorization","value":"actual-example-secret"},{"key":"X-Trace","value":"trace"}
                ],"body":{"mode":"raw","raw":"actual-auth-secret"}},
                "cookie":[{"name":"sid","value":"actual-cookie-secret","domain":"example.test"}]
              }]}]
            }"#,
        )
        .unwrap();
        assert!(matches!(
            &imported.requests[0].auth,
            AuthConfig::Unsupported { name, raw }
                if name == "digest" && raw.to_string().contains("<redacted>")
        ));
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("postman_unsupported_body")
                .and_then(|value| value.pointer("/payload/x-nested/kept")),
            Some(&json!(true))
        );
        assert_eq!(
            imported.requests[0]
                .extensions
                .get("postman_url")
                .and_then(|value| value.pointer("/x-url/nested")),
            Some(&json!(true))
        );
        let example = &imported.examples[0];
        assert_eq!(
            example.request.as_ref().unwrap().headers,
            vec![
                ("Authorization".into(), "<redacted>".into()),
                ("X-Trace".into(), "trace".into())
            ]
        );
        assert_eq!(
            example
                .extensions
                .get("postman_cookies")
                .and_then(|value| value.pointer("/0/value")),
            Some(&json!("<redacted>"))
        );
        let serialized = serde_json::to_string(&imported).unwrap();
        assert!(!serialized.contains("actual-auth-secret"));
        assert!(!serialized.contains("actual-example-secret"));
        assert!(!serialized.contains("actual-cookie-secret"));
        assert!(
            imported
                .warnings
                .iter()
                .any(|warning| warning.contains("unsupported Postman authentication"))
        );
        assert!(
            imported
                .warnings
                .iter()
                .any(|warning| warning.contains("unsupported body mode"))
        );
        assert!(
            imported
                .warnings
                .iter()
                .any(|warning| warning.contains("originalRequest"))
        );
        assert!(
            imported
                .warnings
                .iter()
                .any(|warning| warning.contains("cookies"))
        );
    }

    #[test]
    fn agentops_export_rejects_foreign_and_duplicate_environments() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = new_collection(&workspace, "API", "");
        let mut environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: WorkspaceId::new("other").unwrap(),
            name: "Foreign".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: Vec::new(),
            active: false,
            extensions: Default::default(),
        };
        let secrets = Secrets(HashMap::new());
        let error = PreparedCollectionExport::new(
            &collection,
            &[],
            &[],
            std::slice::from_ref(&environment),
            &[],
            &secrets,
        )
        .unwrap_err();
        assert!(error.contains("another workspace"), "{error}");
        environment.workspace_id = workspace.clone();
        let error = PreparedCollectionExport::new(
            &collection,
            &[],
            &[],
            &[environment.clone(), environment],
            &[],
            &secrets,
        )
        .unwrap_err();
        assert!(error.contains("duplicate ids"), "{error}");
    }

    #[test]
    fn rejects_oversized_import_before_parsing() {
        let workspace = WorkspaceId::new("project").unwrap();
        assert!(import(&workspace, &vec![b'x'; MAX_IMPORT_BYTES + 1]).is_err());
    }

    #[test]
    fn exports_reject_folder_cycles_instead_of_silently_omitting_them() {
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = new_collection(&workspace, "API", "");
        let first_id = FolderId::new();
        let second_id = FolderId::new();
        let first = Folder {
            id: first_id.clone(),
            collection_id: collection.id.clone(),
            parent_id: Some(second_id.clone()),
            name: "First".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let second = Folder {
            id: second_id,
            collection_id: collection.id.clone(),
            parent_id: Some(first_id),
            name: "Second".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 1,
            extensions: Default::default(),
        };

        let error = PreparedCollectionExport::new(
            &collection,
            &[first.clone(), second.clone()],
            &[],
            &[],
            &[],
            &Secrets(HashMap::new()),
        )
        .unwrap_err();
        assert!(error.contains("cycle"), "{error}");
    }

    #[test]
    fn relative_openapi_servers_stay_on_the_request_without_an_environment() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br##"{
              "openapi":"3.0.2",
              "info":{"title":"Petstore","version":"1"},
              "servers":[{"url":"/api/v3"}],
              "paths":{"/pet":{"put":{"summary":"Update an existing pet."}}}
            }"##,
        )
        .unwrap();
        assert_eq!(imported.requests[0].url, "/api/v3/pet");
        assert!(imported.environments.is_empty());
    }

    #[test]
    fn postman_environment_host_variable_becomes_the_base_url() {
        let workspace = WorkspaceId::new("project").unwrap();
        let imported = import(
            &workspace,
            br##"{
              "name":"Staging",
              "_postman_variable_scope":"environment",
              "values":[
                {"key":"baseUrl","value":"https://staging.example.test/","enabled":true},
                {"key":"host","value":"ignored.example.test","enabled":true}
              ]
            }"##,
        )
        .unwrap();
        let environment = &imported.environments[0];
        assert_eq!(environment.base_url, "https://staging.example.test/");
        assert_eq!(environment.variables.len(), 2);

        let imported = import(
            &workspace,
            br##"{
              "name":"Local",
              "_postman_variable_scope":"environment",
              "values":[{"key":"host","value":"localhost:8080","enabled":true}]
            }"##,
        )
        .unwrap();
        assert_eq!(imported.environments[0].base_url, "");
    }
}
