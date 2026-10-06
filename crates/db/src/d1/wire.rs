//! JSON shapes of the D1 `raw` endpoint.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// One statement with positional parameters.
#[derive(Debug, Serialize)]
pub(super) struct Statement<'a> {
    pub sql: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Json>,
}

/// Request body: a single statement, or a batch that D1 runs as one transaction.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum Request<'a> {
    Single(Statement<'a>),
    Batch { batch: Vec<Statement<'a>> },
}

/// The Cloudflare v4 API envelope.
#[derive(Debug, Deserialize)]
pub(super) struct Envelope {
    pub success: bool,
    #[serde(default)]
    pub errors: Vec<ApiMessage>,
    #[serde(default)]
    pub result: Option<Vec<RawResult>>,
}

/// An error or message entry.
#[derive(Debug, Deserialize)]
pub(super) struct ApiMessage {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: String,
}

/// The outcome of one statement.
#[derive(Debug, Default, Deserialize)]
pub(super) struct RawResult {
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub meta: Option<Meta>,
    #[serde(default)]
    pub results: Option<Rows>,
}

/// Rows as arrays, with column names alongside.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Rows {
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<Json>>,
}

/// Execution statistics.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Meta {
    #[serde(default)]
    pub changed_db: Option<bool>,
    #[serde(default)]
    pub changes: Option<u64>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub rows_read: Option<u64>,
    #[serde(default)]
    pub rows_written: Option<u64>,
    #[serde(default)]
    pub served_by_region: Option<String>,
    #[serde(default)]
    pub timings: Option<Timings>,
}

/// Timing detail.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Timings {
    #[serde(default)]
    pub sql_duration_ms: Option<f64>,
}
