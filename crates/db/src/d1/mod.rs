//! Cloudflare D1 driver over the REST API
//! (`POST /accounts/{account_id}/d1/database/{database_id}/raw`).
//!
//! A D1 "connection" is stateless: every statement is one HTTPS request authenticated with
//! an API token. `DbConfig::host` carries the account id, `DbConfig::database` the database
//! id and `DbConfig::password` the API token. Interactive transactions do not exist; each
//! request commits on its own.

mod catalog;
mod wire;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value as Json;
use tokio::sync::Notify;
use tracing::debug;

use crate::batch::{ColumnMeta, RowBatch, RowBatchBuilder};
use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, sqlite::SqliteDialect};
use crate::driver::{CancelHandle, ComponentId, DbConfig, DbSession, Driver, TunnelEndpoint};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, DEFAULT_BATCH_ROWS, Notice, ResultEvent, ResultStream};
use crate::value::{DataType, Engine, Value};
use wire::{Envelope, RawResult, Statement};

/// Production API base URL.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Longest a single statement may take before the request is abandoned. D1 itself stops
/// queries after 30 s; the extra margin covers network time.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The Cloudflare D1 driver.
#[derive(Clone, Debug)]
pub struct D1Driver {
    base: String,
}

impl Default for D1Driver {
    fn default() -> Self {
        Self {
            base: API_BASE.into(),
        }
    }
}

impl D1Driver {
    /// A driver talking to another API base (tests use a local server).
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }
}

fn is_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl Driver for D1Driver {
    fn engine(&self) -> Engine {
        Engine::D1
    }

    fn dialect(&self) -> &dyn Dialect {
        &SqliteDialect
    }

    fn requirements(&self, _cfg: &DbConfig) -> Vec<ComponentId> {
        Vec::new()
    }

    fn connect<'a>(
        &'a self,
        cfg: &'a DbConfig,
        via: Option<TunnelEndpoint>,
    ) -> BoxFuture<'a, Result<Box<dyn DbSession>>> {
        Box::pin(async move {
            if via.is_some() {
                return Err(DbError::Unsupported(
                    "Cloudflare D1 is reached over HTTPS and cannot use an SSH tunnel".into(),
                ));
            }
            let account = cfg.host.trim();
            let database = cfg.database.trim();
            if !is_id(account) {
                return Err(DbError::Connect("account id is missing or invalid".into()));
            }
            if !is_id(database) {
                return Err(DbError::Connect("database id is missing or invalid".into()));
            }
            let token = cfg
                .password
                .clone()
                .ok_or_else(|| DbError::Connect("an API token is required".into()))?;
            let http = reqwest::Client::builder()
                .tls_backend_preconfigured(crate::tls::client_config()?)
                .connect_timeout(cfg.connect_timeout)
                .timeout(REQUEST_TIMEOUT)
                .user_agent(format!(
                    "{}/{}",
                    cfg.application_name,
                    env!("CARGO_PKG_VERSION")
                ))
                .build()
                .map_err(|e| DbError::Connect(e.to_string()))?;
            let mut session = D1Session {
                http,
                url: format!(
                    "{}/accounts/{account}/d1/database/{database}/raw",
                    self.base
                ),
                token,
                cancel: Arc::new(AtomicBool::new(false)),
                notify: Arc::new(Notify::new()),
                version: String::new(),
                closed: false,
            };
            let sqlite = session
                .scalar("SELECT sqlite_version()")
                .await
                .map_err(|e| match e {
                    DbError::Server(s) => DbError::Connect(s.message),
                    other => other,
                })?;
            session.version = format!("Cloudflare D1 · SQLite {sqlite}");
            Ok(Box::new(session) as Box<dyn DbSession>)
        })
    }
}

/// One D1 database.
pub struct D1Session {
    http: reqwest::Client,
    url: String,
    token: SecretString,
    cancel: Arc<AtomicBool>,
    notify: Arc<Notify>,
    version: String,
    closed: bool,
}

impl D1Session {
    fn auth_header(&self) -> Result<HeaderValue> {
        let mut v = HeaderValue::from_str(&format!("Bearer {}", self.token.expose_secret()))
            .map_err(|_| DbError::Connect("the API token contains invalid characters".into()))?;
        v.set_sensitive(true);
        Ok(v)
    }

    /// Send one request body and return its per-statement results. Abandoned when the
    /// cancel handle fires; D1 has no server-side cancel, so a statement that already
    /// reached the database still runs to completion there.
    async fn send(&self, body: &wire::Request<'_>, sql_for_errors: &str) -> Result<Vec<RawResult>> {
        let request = self
            .http
            .post(&self.url)
            .header(AUTHORIZATION, self.auth_header()?)
            .json(body)
            .send();
        let notify = self.notify.clone();
        let flag = self.cancel.clone();
        let cancelled = async move {
            loop {
                let n = notify.notified();
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                n.await;
            }
        };
        let response = tokio::select! {
            r = request => r,
            () = cancelled => return Err(DbError::Cancelled),
        };
        let response = response.map_err(|e| {
            if e.is_timeout() {
                DbError::Protocol("Cloudflare did not answer in time".into())
            } else {
                DbError::Connect(e.to_string())
            }
        })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| DbError::Protocol(e.to_string()))?;
        let envelope: Envelope = serde_json::from_str(&text).map_err(|_| {
            DbError::Protocol(format!(
                "unexpected response from Cloudflare (HTTP {})",
                status.as_u16()
            ))
        })?;
        if !envelope.success || !status.is_success() {
            return Err(api_error(status.as_u16(), &envelope, sql_for_errors));
        }
        let results = envelope.result.unwrap_or_default();
        if let Some(failed) = results.iter().find(|r| r.success == Some(false)) {
            let message = failed
                .error
                .clone()
                .unwrap_or_else(|| "statement failed".into());
            return Err(server_error(None, &message, sql_for_errors));
        }
        Ok(results)
    }

    async fn query(&self, sql: &str, params: Vec<Json>) -> Result<Vec<RawResult>> {
        self.cancel.store(false, Ordering::SeqCst);
        let body = wire::Request::Single(Statement { sql, params });
        self.send(&body, sql).await
    }

    /// Several statements in one request. D1 runs a batch as a single transaction.
    async fn batch(&self, statements: Vec<Statement<'_>>) -> Result<Vec<RawResult>> {
        self.cancel.store(false, Ordering::SeqCst);
        let body = wire::Request::Batch { batch: statements };
        self.send(&body, "").await
    }

    async fn scalar(&self, sql: &str) -> Result<String> {
        let results = self.query(sql, Vec::new()).await?;
        Ok(results
            .first()
            .and_then(|r| r.results.as_ref())
            .and_then(|r| r.rows.first())
            .and_then(|row| row.first())
            .map(json_text)
            .unwrap_or_default())
    }
}

fn json_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => other.to_string(),
    }
}

/// Map an API-level failure (bad token, unknown database, SQL error) to a `DbError`.
fn api_error(status: u16, envelope: &Envelope, sql: &str) -> DbError {
    let first = envelope.errors.first();
    let message = first
        .map(|e| e.message.clone())
        .unwrap_or_else(|| format!("request failed with HTTP {status}"));
    match status {
        401 | 403 => DbError::Connect(format!("Cloudflare rejected the API token: {message}")),
        404 => DbError::Connect(format!("account or database not found: {message}")),
        429 => DbError::Protocol(format!("rate limited by Cloudflare: {message}")),
        _ => server_error(first.and_then(|e| e.code), &message, sql),
    }
}

/// D1 reports SQL errors as `... at offset N: SQLITE_ERROR` with a 0-based byte offset.
fn server_error(code: Option<i64>, message: &str, sql: &str) -> DbError {
    let position = message
        .find("at offset ")
        .map(|i| &message[i + "at offset ".len()..])
        .and_then(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<usize>().ok()
        })
        .filter(|_| !sql.is_empty())
        .map(|byte| {
            let mut b = byte.min(sql.len());
            while !sql.is_char_boundary(b) {
                b -= 1;
            }
            ErrorPosition::Offset(sql[..b].chars().count() as u32 + 1)
        });
    let sqlite_code = message
        .rsplit(": ")
        .next()
        .filter(|s| s.starts_with("SQLITE_"))
        .map(str::to_owned);
    let text = match &sqlite_code {
        Some(c) => message
            .strip_suffix(c.as_str())
            .map(|m| m.trim_end_matches([':', ' ']))
            .unwrap_or(message),
        None => message,
    };
    DbError::Server(Box::new(ServerError {
        severity: "ERROR".into(),
        code: sqlite_code.or_else(|| code.map(|c| c.to_string())),
        message: text.to_owned(),
        detail: None,
        hint: None,
        position,
    }))
}

/// What a column held across all rows of a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    Nothing,
    Bool,
    Int,
    Float,
    Text,
    Bytes,
    Json,
    Mixed,
}

fn classify(v: &Json) -> Option<Seen> {
    Some(match v {
        Json::Null => return None,
        Json::Bool(_) => Seen::Bool,
        Json::Number(n) if n.is_i64() => Seen::Int,
        Json::Number(_) => Seen::Float,
        Json::String(_) => Seen::Text,
        Json::Array(a) if a.iter().all(|x| x.as_u64().is_some_and(|b| b <= 255)) => Seen::Bytes,
        Json::Array(_) | Json::Object(_) => Seen::Json,
    })
}

fn merge(a: Seen, b: Seen) -> Seen {
    match (a, b) {
        (Seen::Nothing, x) => x,
        (x, y) if x == y => x,
        (Seen::Int, Seen::Float) | (Seen::Float, Seen::Int) => Seen::Float,
        _ => Seen::Mixed,
    }
}

/// Column metadata inferred from the values (the raw API returns names only).
fn infer_columns(names: &[String], rows: &[Vec<Json>]) -> (Arc<[ColumnMeta]>, Vec<Seen>) {
    let mut seen = vec![Seen::Nothing; names.len()];
    for row in rows {
        for (i, v) in row.iter().enumerate().take(names.len()) {
            if let Some(k) = classify(v) {
                seen[i] = merge(seen[i], k);
            }
        }
    }
    let meta: Vec<ColumnMeta> = names
        .iter()
        .zip(&seen)
        .map(|(name, s)| {
            let (type_name, dt) = match s {
                Seen::Bool => ("BOOLEAN", DataType::Bool),
                Seen::Int => ("INTEGER", DataType::Int64),
                Seen::Float => ("REAL", DataType::Float64),
                Seen::Bytes => ("BLOB", DataType::Bytes),
                Seen::Json => ("JSON", DataType::Json),
                Seen::Text | Seen::Mixed | Seen::Nothing => ("TEXT", DataType::Text),
            };
            ColumnMeta::new(name.clone(), type_name, dt)
        })
        .collect();
    (Arc::from(meta), seen)
}

fn push_cell(b: &mut RowBatchBuilder, seen: Seen, v: Option<&Json>) {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        b.push_null();
        return;
    };
    match (seen, v) {
        (Seen::Bool, Json::Bool(x)) => b.push_bool(*x),
        (Seen::Int, Json::Number(n)) => b.push_i64(n.as_i64().unwrap_or_default()),
        (Seen::Float, Json::Number(n)) => b.push_f64(n.as_f64().unwrap_or_default()),
        (Seen::Bytes, Json::Array(a)) => {
            let bytes: Vec<u8> = a
                .iter()
                .map(|x| x.as_u64().unwrap_or_default() as u8)
                .collect();
            b.push_bytes(&bytes);
        }
        (_, Json::String(s)) => b.push_str(s),
        (_, other) => b.push_str(&other.to_string()),
    }
}

/// Columnar batches for one result set.
fn to_batches(names: &[String], rows: &[Vec<Json>]) -> (Arc<[ColumnMeta]>, Vec<RowBatch>) {
    let (meta, seen) = infer_columns(names, rows);
    let batches = rows
        .chunks(DEFAULT_BATCH_ROWS)
        .map(|chunk| {
            let mut b = RowBatchBuilder::for_columns(&meta, chunk.len());
            for row in chunk {
                for (i, s) in seen.iter().enumerate() {
                    push_cell(&mut b, *s, row.get(i));
                }
            }
            b.finish()
        })
        .collect();
    (meta, batches)
}

fn param_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::from(i64::from(*b)),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Text(s) | Value::Other(s) | Value::Numeric(s) | Value::Json(s) => {
            Json::String(s.clone())
        }
        other => Json::String(other.to_display()),
    }
}

fn stats_notice(r: &RawResult) -> Option<Notice> {
    let m = r.meta.as_ref()?;
    let mut parts = Vec::new();
    if let Some(n) = m.rows_read {
        parts.push(format!("rows read {n}"));
    }
    if let Some(n) = m.rows_written {
        parts.push(format!("rows written {n}"));
    }
    if let Some(d) = m
        .timings
        .as_ref()
        .and_then(|t| t.sql_duration_ms)
        .or(m.duration)
    {
        parts.push(format!("{d:.2} ms in database"));
    }
    if let Some(region) = &m.served_by_region {
        parts.push(format!("served by {region}"));
    }
    (!parts.is_empty()).then(|| Notice {
        severity: "INFO".into(),
        code: None,
        message: parts.join(" · "),
    })
}

/// The stream for the results of one request.
fn events(results: Vec<RawResult>, started: Instant) -> Vec<Result<ResultEvent>> {
    let mut out = Vec::new();
    let mut affected = None;
    for (i, r) in results.into_iter().enumerate() {
        if i > 0 {
            out.push(Ok(ResultEvent::NextResultSet));
        }
        let rows = r.results.as_ref();
        if let Some(rows) = rows.filter(|x| !x.columns.is_empty()) {
            let (meta, batches) = to_batches(&rows.columns, &rows.rows);
            out.push(Ok(ResultEvent::Columns(meta)));
            out.extend(batches.into_iter().map(|b| Ok(ResultEvent::Rows(b))));
        }
        if let Some(m) = &r.meta
            && m.changed_db == Some(true)
        {
            affected = Some(affected.unwrap_or(0) + m.changes.unwrap_or(0));
        }
        if let Some(n) = stats_notice(&r) {
            out.push(Ok(ResultEvent::Notice(n)));
        }
    }
    out.push(Ok(ResultEvent::Done(Completion {
        affected,
        elapsed: started.elapsed(),
    })));
    out
}

impl DbSession for D1Session {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            let started = Instant::now();
            let params: Vec<Json> = params.iter().map(param_json).collect();
            debug!(params = params.len(), "d1 query");
            let results = self.query(sql, params).await?;
            Ok(Box::pin(stream::iter(events(results, started))) as ResultStream)
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let notify = self.notify.clone();
        let flag = self.cancel.clone();
        CancelHandle::new(self.cancel.clone(), move || {
            flag.store(true, Ordering::SeqCst);
            notify.notify_waiters();
            Box::pin(async { Ok(()) })
        })
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        Box::pin(async move { catalog::introspect(self, scope).await })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            Err(DbError::Unsupported(
                "Cloudflare D1 has no interactive transactions; each statement commits on its own"
                    .into(),
            ))
        })
    }

    fn commit(&mut self) -> BoxFuture<'_, Result<()>> {
        self.begin()
    }

    fn rollback(&mut self) -> BoxFuture<'_, Result<()>> {
        self.begin()
    }

    fn in_transaction(&self) -> bool {
        false
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn is_closed(&self) -> bool {
        self.closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::BatchList;
    use serde_json::json;

    #[test]
    fn infers_column_types_from_values() {
        let names: Vec<String> = ["id", "price", "name", "blob", "mixed", "empty"]
            .map(String::from)
            .into();
        let rows = vec![
            vec![
                json!(1),
                json!(2),
                json!("a"),
                json!([1, 2]),
                json!(1),
                Json::Null,
            ],
            vec![
                json!(2),
                json!(2.5),
                Json::Null,
                Json::Null,
                json!("x"),
                Json::Null,
            ],
        ];
        let (meta, batches) = to_batches(&names, &rows);
        let types: Vec<DataType> = meta.iter().map(|c| c.data_type).collect();
        assert_eq!(
            types,
            [
                DataType::Int64,
                DataType::Float64,
                DataType::Text,
                DataType::Bytes,
                DataType::Text,
                DataType::Text
            ]
        );
        let mut list = BatchList::default();
        for b in batches {
            list.push(b);
        }
        assert_eq!(list.len(), 2);
        let value = |r: usize, c: usize| {
            list.cell(r, c)
                .map(|cell| cell.to_value(meta[c].data_type))
                .expect("cell")
        };
        assert_eq!(value(1, 1), Value::Float(2.5));
        assert_eq!(value(0, 3), Value::Bytes(vec![1, 2]));
        assert_eq!(value(1, 4), Value::Text("x".into()));
        assert_eq!(value(0, 4), Value::Text("1".into()));
        assert_eq!(value(1, 2), Value::Null);
    }

    #[test]
    fn maps_sql_error_offset_and_code() {
        let sql = "selec * from t";
        let e = server_error(
            Some(7500),
            "near \"selec\": syntax error at offset 0: SQLITE_ERROR",
            sql,
        );
        let s = e.as_server().expect("server error");
        assert_eq!(s.code.as_deref(), Some("SQLITE_ERROR"));
        assert_eq!(s.message, "near \"selec\": syntax error at offset 0");
        assert_eq!(s.position, Some(ErrorPosition::Offset(1)));
    }

    #[test]
    fn bad_token_is_a_connect_error() {
        let env: Envelope = serde_json::from_value(json!({
            "success": false,
            "errors": [{"code": 10000, "message": "Authentication error"}],
            "messages": [],
            "result": null
        }))
        .expect("envelope");
        assert!(matches!(api_error(403, &env, ""), DbError::Connect(m) if m.contains("API token")));
    }
}
