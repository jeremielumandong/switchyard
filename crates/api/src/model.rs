pub use crate::secrets::{SecretRef, SecretScope as WorkspaceId};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! durable_id {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            pub fn parse(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                Uuid::parse_str(&value).map_err(|_| format!("invalid {} id {value:?}", $kind))?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

durable_id!(CollectionId, "collection");
durable_id!(FolderId, "folder");
durable_id!(RequestId, "request");
durable_id!(EnvironmentId, "environment");
durable_id!(ExchangeId, "exchange");
durable_id!(RunId, "run");
durable_id!(RowId, "row");
durable_id!(ExampleId, "example");

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableValue {
    Plain(String),
    Secret(SecretRef),
    MissingSecret(SecretRef),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Variable {
    pub id: RowId,
    pub key: String,
    pub value: VariableValue,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyValueRow {
    pub id: RowId,
    pub key: String,
    pub value: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MultipartRow {
    pub id: RowId,
    pub key: String,
    pub value: MultipartValue,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: String,
}

impl MultipartRow {
    pub fn text(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            id: RowId::new(),
            key: key.into(),
            value: MultipartValue::Text(value.into()),
            enabled: true,
            description: String::new(),
        }
    }

    pub fn file(key: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            id: RowId::new(),
            key: key.into(),
            value: MultipartValue::File(path.into()),
            enabled: true,
            description: String::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum MultipartValue {
    Text(String),
    File(String),
}

impl KeyValueRow {
    pub fn enabled(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            id: RowId::new(),
            key: key.into(),
            value: value.into(),
            enabled: true,
            description: String::new(),
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct HttpMethod(String);

impl<'de> Deserialize<'de> for HttpMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl HttpMethod {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || !value.bytes().all(is_http_token_byte) {
            return Err(format!("invalid HTTP method {value:?}"));
        }
        Ok(Self(value))
    }

    pub fn get() -> Self {
        Self("GET".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn conventionally_has_no_body(&self) -> bool {
        matches!(self.0.as_str(), "GET" | "HEAD")
    }
}

const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

impl Default for HttpMethod {
    fn default() -> Self {
        Self::get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawBodyKind {
    Json,
    Xml,
    Text,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    #[default]
    None,
    Raw {
        media_type: RawBodyKind,
        text: String,
    },
    UrlEncoded {
        rows: Vec<KeyValueRow>,
    },
    Multipart {
        rows: Vec<MultipartRow>,
    },
    Binary {
        path: String,
    },
    GraphQl {
        query: String,
        variables: String,
    },
}

/// Basic credentials used only for a separate sign-in exchange.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BasicLoginCredentials {
    pub username: String,
    pub password: SecretRef,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthConfig {
    Inherit,
    #[default]
    None,
    ApiKey {
        name: String,
        value: SecretRef,
        location: ApiKeyLocation,
    },
    Basic {
        username: String,
        password: SecretRef,
    },
    Bearer {
        token: SecretRef,
    },
    OAuth2 {
        token: SecretRef,
    },
    OAuth2AuthorizationCodePkce {
        authorization_endpoint: String,
        token_endpoint: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<KeyValueRow>,
        client_id: String,
        #[serde(default)]
        scopes: Vec<String>,
        redirect_uri: String,
        access_token: Option<SecretRef>,
        refresh_token: Option<SecretRef>,
        #[serde(default)]
        expires_at: Option<i64>,
    },
    OAuth2ClientCredentials {
        token_endpoint: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<KeyValueRow>,
        client_id: String,
        client_secret: SecretRef,
        #[serde(default)]
        scopes: Vec<String>,
        access_token: Option<SecretRef>,
        #[serde(default)]
        expires_at: Option<i64>,
    },
    /// RFC 6749 §4.3 resource-owner password grant: a *user* token from the
    /// token endpoint, for APIs that need a signed-in user where no browser
    /// round trip is possible. The client secret is optional (public
    /// clients); the user's credentials live in the vault.
    OAuth2Password {
        token_endpoint: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<KeyValueRow>,
        client_id: String,
        #[serde(default)]
        client_secret: Option<SecretRef>,
        username: String,
        password: SecretRef,
        #[serde(default)]
        scopes: Vec<String>,
        access_token: Option<SecretRef>,
        #[serde(default)]
        refresh_token: Option<SecretRef>,
        #[serde(default)]
        expires_at: Option<i64>,
    },
    AwsSigV4 {
        access_key: SecretRef,
        secret_key: SecretRef,
        #[serde(default)]
        session_token: Option<SecretRef>,
        region: String,
        service: String,
    },
    /// Authenticate by calling an endpoint first. The last two fields are
    /// the managed cache, like the OAuth2 arms; the value itself never lives
    /// in the definition.
    Login {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        basic: Option<BasicLoginCredentials>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<KeyValueRow>,
        #[serde(default = "default_login_method")]
        method: String,
        #[serde(default)]
        body: String,
        token_path: String,
        #[serde(default)]
        ttl_secs: Option<u64>,
        #[serde(default)]
        access_token: Option<SecretRef>,
        #[serde(default)]
        expires_at: Option<i64>,
    },
    Unsupported {
        name: String,
        raw: serde_json::Value,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    Header,
    Query,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct Scripts {
    #[serde(default)]
    pub pre_request: String,
    #[serde(default)]
    pub tests: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestSettings {
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_redirects")]
    pub max_redirects: u8,
    #[serde(default = "default_true")]
    pub allow_private_network: bool,
    #[serde(default = "default_true")]
    pub follow_redirects: bool,
}

fn default_login_method() -> String {
    "POST".into()
}

/// How long a Login reply is trusted when it names no `expires_in`.
pub const DEFAULT_LOGIN_TTL_SECS: u64 = 3600;

const fn default_timeout() -> u64 {
    30_000
}

const fn default_redirects() -> u8 {
    5
}

impl Default for RequestSettings {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout(),
            max_redirects: default_redirects(),
            allow_private_network: true,
            follow_redirects: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: CollectionId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub variables: Vec<Variable>,
    #[serde(default)]
    pub scripts: Scripts,
    #[serde(default)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub collection_id: CollectionId,
    pub parent_id: Option<FolderId>,
    pub name: String,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub variables: Vec<Variable>,
    #[serde(default)]
    pub scripts: Scripts,
    #[serde(default)]
    pub sort_key: i64,
    #[serde(default)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedRequest {
    pub id: RequestId,
    pub collection_id: CollectionId,
    pub folder_id: Option<FolderId>,
    pub name: String,
    pub method: HttpMethod,
    pub url: String,
    #[serde(default)]
    pub params: Vec<KeyValueRow>,
    #[serde(default)]
    pub headers: Vec<KeyValueRow>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub body: Body,
    #[serde(default)]
    pub variables: Vec<Variable>,
    #[serde(default)]
    pub scripts: Scripts,
    #[serde(default)]
    pub settings: RequestSettings,
    #[serde(default)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub sort_key: i64,
}

/// How dangerous an environment is: the same four levels as database
/// connections. Drives the workbench colour and the confirmation asked
/// before a write is sent to Production.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentLabel {
    /// Red; non-safe methods ask for confirmation before they are sent.
    Production,
    /// Amber.
    Staging,
    /// Green.
    Development,
    /// No label (neutral).
    #[default]
    Local,
}

impl EnvironmentLabel {
    /// All labels in UI order.
    pub const ALL: [EnvironmentLabel; 4] = [
        EnvironmentLabel::Production,
        EnvironmentLabel::Staging,
        EnvironmentLabel::Development,
        EnvironmentLabel::Local,
    ];

    /// Title-case name.
    pub fn name(self) -> &'static str {
        match self {
            EnvironmentLabel::Production => "Production",
            EnvironmentLabel::Staging => "Staging",
            EnvironmentLabel::Development => "Development",
            EnvironmentLabel::Local => "Local",
        }
    }

    /// Stored form (`production`, `staging`, `development`, `local`).
    pub fn as_str(self) -> &'static str {
        match self {
            EnvironmentLabel::Production => "production",
            EnvironmentLabel::Staging => "staging",
            EnvironmentLabel::Development => "development",
            EnvironmentLabel::Local => "local",
        }
    }

    pub fn is_production(self) -> bool {
        self == EnvironmentLabel::Production
    }

    pub fn is_local(&self) -> bool {
        *self == EnvironmentLabel::Local
    }
}

/// Whether sending `method` under an environment labelled `label` must be
/// confirmed first: Production and anything but GET, HEAD or OPTIONS.
/// Methods are case-sensitive on the wire, so `get` is not treated as safe.
pub fn send_needs_confirmation(method: &str, label: EnvironmentLabel) -> bool {
    label.is_production() && !matches!(method, "GET" | "HEAD" | "OPTIONS")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub id: EnvironmentId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// Production / Staging / Development / Local; absent means Local.
    #[serde(default, skip_serializing_if = "EnvironmentLabel::is_local")]
    pub label: EnvironmentLabel,
    /// Prefixed onto relative request URLs; also readable as `{{base_url}}`.
    #[serde(default)]
    pub base_url: String,
    /// What requests that inherit all the way up authenticate with.
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub variables: Vec<Variable>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

/// A previewed edit to one saved request URL. The original URL protects
/// concurrent edits when the batch is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestUrlUpdate {
    pub request_id: RequestId,
    pub original_url: String,
    pub url: String,
}

#[derive(Clone, Eq, PartialEq)]
pub enum PreparedBody {
    None,
    Bytes {
        content_type: String,
        bytes: Vec<u8>,
    },
    Multipart(Vec<PreparedMultipartPart>),
    File(String),
}

#[derive(Clone, Eq, PartialEq)]
pub struct PreparedMultipartPart {
    pub name: String,
    pub value: PreparedMultipartValue,
}

#[derive(Clone, Eq, PartialEq)]
pub enum PreparedMultipartValue {
    Text(String),
    File(String),
}

#[derive(Clone, Eq, PartialEq)]
pub struct AwsSigV4Signing {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub region: String,
    pub service: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct PreparedRequest {
    pub request_id: RequestId,
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: PreparedBody,
    pub settings: RequestSettings,
    pub aws_sigv4: Option<AwsSigV4Signing>,
    pub redactions: Vec<String>,
}

impl fmt::Debug for PreparedBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Bytes {
                content_type,
                bytes,
            } => formatter
                .debug_struct("Bytes")
                .field("content_type", content_type)
                .field("length", &bytes.len())
                .finish(),
            Self::Multipart(rows) => formatter
                .debug_tuple("Multipart")
                .field(&format_args!("{} parts", rows.len()))
                .finish(),
            Self::File(_) => formatter.debug_tuple("File").field(&"<redacted>").finish(),
        }
    }
}

impl fmt::Debug for PreparedRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRequest")
            .field("request_id", &self.request_id)
            .field("method", &self.method)
            .field("url", &redact_debug(&self.url, &self.redactions))
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| (name, "<redacted>"))
                    .collect::<Vec<_>>(),
            )
            .field("body", &self.body)
            .field("settings", &self.settings)
            .field(
                "aws_sigv4",
                &self.aws_sigv4.as_ref().map(|_| "<credentials>"),
            )
            .finish_non_exhaustive()
    }
}

fn redact_debug(input: &str, redactions: &[String]) -> String {
    redactions
        .iter()
        .filter(|value| !value.is_empty())
        .fold(input.to_string(), |text, value| {
            text.replace(value, "<redacted>")
        })
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RedactedRequestSnapshot {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// The submitted typed request, retained independently of the mutable saved
    /// request so history replay does not have to guess body/auth modes.
    #[serde(default)]
    pub replay: Option<ReplayRequestSnapshot>,
    #[serde(default)]
    pub body_bytes: u64,
    #[serde(default)]
    pub body_sensitive: bool,
    #[serde(default)]
    pub body_binary: bool,
    #[serde(default)]
    pub body_truncated: bool,
    #[serde(default)]
    pub body_omitted_reason: Option<String>,
}

/// A persistence-safe, typed copy of the request definition that was
/// submitted. Secret-bearing auth fields contain only opaque [`SecretRef`]s;
/// resolved values remain confined to `PreparedRequest` and its redaction set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayRequestSnapshot {
    pub name: String,
    pub method: HttpMethod,
    pub url: String,
    #[serde(default)]
    pub params: Vec<KeyValueRow>,
    #[serde(default)]
    pub headers: Vec<KeyValueRow>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub body: Body,
    #[serde(default)]
    pub variables: Vec<Variable>,
    #[serde(default)]
    pub scripts: Scripts,
    #[serde(default)]
    pub settings: RequestSettings,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponseSnapshot {
    #[serde(default)]
    pub console: Vec<ConsoleEntry>,
    pub status: u16,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body_base64: String,
    /// SHA-256 of the complete upstream body, computed before any display or
    /// persistence truncation/omission. Hashes are safe to persist and let Diff
    /// distinguish payloads whose retained previews are identical or empty.
    #[serde(default)]
    pub full_body_sha256: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub final_url: String,
    #[serde(default)]
    pub redirects: Vec<RedirectSnapshot>,
    #[serde(default)]
    pub http_version: String,
    #[serde(default)]
    pub received_bytes: u64,
    #[serde(default)]
    pub stored_bytes: u64,
    #[serde(default)]
    pub timings: ResponseTimings,
    #[serde(default)]
    pub cookies: Vec<ResponseCookie>,
    #[serde(default)]
    pub test_results: Vec<TestResult>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub body_omitted_reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RedirectSnapshot {
    pub status: u16,
    pub from_url: String,
    pub to_url: String,
    pub method: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponseTimings {
    #[serde(default)]
    pub dns_ms: Option<u64>,
    #[serde(default)]
    pub connect_ms: Option<u64>,
    #[serde(default)]
    pub tls_ms: Option<u64>,
    #[serde(default)]
    pub first_byte_ms: Option<u64>,
    #[serde(default)]
    pub download_ms: Option<u64>,
}

/// Cookie metadata exposed by a response. Values never enter durable history.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseCookie {
    pub name: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub same_site: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// Redacted, bounded output from one request's script execution.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub phase: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exchange {
    #[serde(default)]
    pub console: Vec<ConsoleEntry>,
    #[serde(default)]
    pub test_results: Vec<TestResult>,
    pub id: ExchangeId,
    pub workspace_id: WorkspaceId,
    pub request_id: Option<RequestId>,
    pub request: RedactedRequestSnapshot,
    pub response: Option<ResponseSnapshot>,
    pub error: Option<String>,
    pub started_at: i64,
    pub completed_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Example {
    pub id: ExampleId,
    pub request_id: RequestId,
    pub name: String,
    pub request: Option<RedactedRequestSnapshot>,
    pub response: ResponseSnapshot,
    #[serde(default)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub sort_key: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollectionRun {
    pub id: RunId,
    pub workspace_id: WorkspaceId,
    pub collection_id: CollectionId,
    #[serde(default)]
    pub environment_id: Option<EnvironmentId>,
    #[serde(default)]
    pub iteration_count: u32,
    #[serde(default)]
    pub stop_on_error: bool,
    /// Optional folder root selected when the run was started. `None` means the
    /// collection root.
    #[serde(default)]
    pub selected_folder_id: Option<FolderId>,
    /// Explicit request selection captured at submission time. An empty list
    /// means every request within `selected_folder_id` (or the collection).
    #[serde(default)]
    pub selected_request_ids: Vec<RequestId>,
    /// Delay applied between sequential requests.
    #[serde(default)]
    pub delay_ms: u64,
    /// Whether JavaScript variable mutations carry into the next iteration.
    /// This defaults to true to preserve the behavior of runs serialized before
    /// the field was added.
    #[serde(default = "default_true")]
    pub keep_variable_values: bool,
    #[serde(default)]
    pub item_results: Vec<RunItemResult>,
    pub status: RunStatus,
    pub started_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunItemResult {
    #[serde(default)]
    pub console: Vec<ConsoleEntry>,
    pub request_id: RequestId,
    pub iteration: u32,
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub error: Option<String>,
    #[serde(default)]
    pub response: Option<ResponseSnapshot>,
    #[serde(default)]
    pub test_results: Vec<TestResult>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Canceled,
}

#[cfg(test)]
mod tests {
    use super::{
        AuthConfig, CollectionRun, Environment, EnvironmentLabel, HttpMethod, RequestSettings,
        send_needs_confirmation,
    };

    #[test]
    fn only_unsafe_methods_against_production_need_confirmation() {
        for label in EnvironmentLabel::ALL {
            for method in ["GET", "HEAD", "OPTIONS"] {
                assert!(
                    !send_needs_confirmation(method, label),
                    "{method} {label:?}"
                );
            }
            for method in ["POST", "PUT", "PATCH", "DELETE", "PURGE", "get"] {
                assert_eq!(
                    send_needs_confirmation(method, label),
                    label == EnvironmentLabel::Production,
                    "{method} {label:?}"
                );
            }
        }
    }

    #[test]
    fn environment_label_is_optional_in_json() {
        let legacy: Environment = serde_json::from_value(serde_json::json!({
            "id": "6f1b8e4e-8f0e-4f6e-9a57-2c1d5d0f8a11",
            "workspace_id": "project",
            "name": "Legacy"
        }))
        .unwrap();
        assert_eq!(legacy.label, EnvironmentLabel::Local);
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("label")
                .is_none()
        );
        let production = Environment {
            label: EnvironmentLabel::Production,
            ..legacy
        };
        let value = serde_json::to_value(&production).unwrap();
        assert_eq!(value["label"], "production");
        let back: Environment = serde_json::from_value(value).unwrap();
        assert_eq!(back, production);
    }

    #[test]
    fn legacy_login_auth_defaults_to_no_basic_credentials() {
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "kind": "login",
            "url": "/login",
            "token_path": "access_token"
        }))
        .unwrap();
        assert!(
            matches!(&auth, AuthConfig::Login { basic: None, method, headers, .. } if method == "POST" && headers.is_empty())
        );
        assert!(serde_json::to_value(auth).unwrap().get("basic").is_none());
    }

    #[test]
    fn request_settings_allow_private_networks_by_default() {
        assert!(RequestSettings::default().allow_private_network);
        let decoded: RequestSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(decoded.allow_private_network);
    }

    #[test]
    fn http_method_preserves_case_and_accepts_every_rfc_token_character() {
        let method = HttpMethod::new("custom!#$%&'*+-.^_`|~09Az").unwrap();
        assert_eq!(method.as_str(), "custom!#$%&'*+-.^_`|~09Az");
    }

    #[test]
    fn http_method_rejects_whitespace_separators_and_non_ascii() {
        for invalid in ["", " GET", "GET ", "GE T", "GET/", "GET:", "méthod"] {
            assert!(HttpMethod::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn http_method_deserialization_cannot_bypass_token_validation() {
        assert_eq!(
            serde_json::from_str::<HttpMethod>(r#""mIxEd""#)
                .unwrap()
                .as_str(),
            "mIxEd"
        );
        assert!(serde_json::from_str::<HttpMethod>(r#""GET /""#).is_err());
    }

    #[test]
    fn legacy_collection_runs_receive_compatible_configuration_defaults() {
        let run: CollectionRun = serde_json::from_value(serde_json::json!({
            "id":"run-legacy",
            "workspace_id":"project",
            "collection_id":"collection-legacy",
            "iteration_count":1,
            "stop_on_error":false,
            "item_results":[],
            "status":"completed",
            "started_at":1,
            "completed_at":2
        }))
        .unwrap();

        assert_eq!(run.selected_folder_id, None);
        assert!(run.selected_request_ids.is_empty());
        assert_eq!(run.delay_ms, 0);
        assert!(run.keep_variable_values);
    }
}
