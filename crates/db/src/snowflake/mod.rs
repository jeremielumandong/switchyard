//! Snowflake driver over the SQL REST API v2 (`/api/v2/statements`).
//!
//! Each request runs in its own server session, so there are no interactive
//! transactions; `USE DATABASE | SCHEMA | WAREHOUSE | ROLE` is applied to the requests
//! that follow on the client side. Authentication is a key-pair JWT minted from the
//! user's RSA private key, or a programmatic access token. The SQL API does not accept
//! passwords.
//!
//! `DbConfig::host` is the account identifier (`myorg-myaccount`, `xy12345.us-east-1`)
//! or the full `<account>.snowflakecomputing.com` host; `DbConfig::options` carries
//! `warehouse`, `role`, `schema` and `private_key_path`.

mod catalog;
pub mod jwt;
mod wire;

use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value as Json, json};
use tokio::sync::Notify;
use tracing::debug;

use crate::batch::ColumnMeta;
use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, snowflake::SnowflakeDialect};
use crate::driver::{
    CancelHandle, ComponentId, DbAuthMethod, DbConfig, DbSession, Driver, TunnelEndpoint,
};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, ResultEvent, ResultStream};
use crate::value::{Engine, Value};
use wire::{CODE_RUNNING, Response, SubmitRequest};

/// Longest one HTTP round trip may take. Statements that run longer come back as
/// "still running" and are polled.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// The Snowflake driver.
#[derive(Clone, Debug, Default)]
pub struct SnowflakeDriver {
    /// Base URL override (tests use a local server).
    base: Option<String>,
}

impl SnowflakeDriver {
    /// A driver talking to another base URL instead of the account's host.
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            base: Some(base.into().trim_end_matches('/').to_owned()),
        }
    }
}

/// `https://<account>.snowflakecomputing.com` for an account identifier or host.
pub fn account_url(account: &str) -> String {
    let a = account
        .trim()
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if a.to_ascii_lowercase().ends_with(".snowflakecomputing.com") {
        format!("https://{a}")
    } else {
        format!("https://{a}.snowflakecomputing.com")
    }
}

/// How requests prove who they are.
enum Auth {
    KeyPair {
        key: Box<jwt::KeyPair>,
        account: String,
        user: String,
        /// The current token and when it was minted (Unix seconds).
        token: Mutex<Option<(SecretString, u64)>>,
    },
    Token(SecretString),
}

impl Auth {
    fn token_type(&self) -> &'static str {
        match self {
            Auth::KeyPair { .. } => "KEYPAIR_JWT",
            Auth::Token(_) => "PROGRAMMATIC_ACCESS_TOKEN",
        }
    }

    /// The bearer header, re-minting a key-pair token after 50 minutes.
    fn header(&self) -> Result<HeaderValue> {
        let token = match self {
            Auth::Token(t) => t.expose_secret().to_owned(),
            Auth::KeyPair {
                key,
                account,
                user,
                token,
            } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let mut slot = token
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match slot.as_ref() {
                    Some((t, at)) if now.saturating_sub(*at) < 50 * 60 => {
                        t.expose_secret().to_owned()
                    }
                    _ => {
                        let t = key.jwt(account, user, now).map_err(DbError::Connect)?;
                        *slot = Some((SecretString::from(t.clone()), now));
                        t
                    }
                }
            }
        };
        let mut v = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| DbError::Connect("the token contains invalid characters".into()))?;
        v.set_sensitive(true);
        Ok(v)
    }

    /// What to add to a rejected login.
    fn rejection_hint(&self) -> String {
        match self {
            Auth::KeyPair { key, user, .. } => {
                let user = user.to_ascii_uppercase();
                format!(
                    " Switchyard signed with {}; compare it with RSA_PUBLIC_KEY_FP from \
                     `DESCRIBE USER {user}`, and if they differ register the public key with \
                     `ALTER USER {user} SET RSA_PUBLIC_KEY='…'`.",
                    key.fingerprint()
                )
            }
            Auth::Token(_) => {
                " Check that the programmatic access token is valid and not expired.".into()
            }
        }
    }
}

/// Session context applied to every request (`USE ...` updates it).
#[derive(Clone, Debug, Default)]
struct Context {
    database: Option<String>,
    schema: Option<String>,
    warehouse: Option<String>,
    role: Option<String>,
}

/// `~/x` → `$HOME/x` (`%USERPROFILE%` on Windows).
fn expand_home(path: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (
        path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")),
        home,
    ) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(path),
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

impl Driver for SnowflakeDriver {
    fn engine(&self) -> Engine {
        Engine::Snowflake
    }

    fn dialect(&self) -> &dyn Dialect {
        &SnowflakeDialect
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
                    "Snowflake is reached over HTTPS and cannot use an SSH tunnel".into(),
                ));
            }
            let account = cfg.host.trim();
            if account.is_empty() {
                return Err(DbError::Connect("the account identifier is missing".into()));
            }
            let auth = match cfg.auth {
                DbAuthMethod::KeyPair => {
                    let path = cfg.option("private_key_path").ok_or_else(|| {
                        DbError::Connect("choose the private key file for key-pair sign-in".into())
                    })?;
                    let path = expand_home(path);
                    let pem = tokio::task::spawn_blocking(move || std::fs::read_to_string(path))
                        .await
                        .map_err(|e| DbError::Connect(e.to_string()))?
                        .map_err(|e| {
                            DbError::Connect(format!("cannot read the private key file: {e}"))
                        })?;
                    let passphrase = cfg.password.as_ref().map(|p| p.expose_secret().to_owned());
                    let key = jwt::KeyPair::from_pem(&pem, passphrase.as_deref())
                        .map_err(DbError::Connect)?;
                    Auth::KeyPair {
                        key: Box::new(key),
                        account: account.to_owned(),
                        user: cfg.user.trim().to_owned(),
                        token: Mutex::new(None),
                    }
                }
                DbAuthMethod::AccessToken => {
                    Auth::Token(cfg.password.clone().ok_or_else(|| {
                        DbError::Connect("a programmatic access token is required".into())
                    })?)
                }
                DbAuthMethod::Password => {
                    return Err(DbError::Connect(
                        "Snowflake's SQL API does not accept passwords: use a key pair or a \
                         programmatic access token"
                            .into(),
                    ));
                }
                other => {
                    return Err(DbError::Unsupported(format!(
                        "{} is not available for Snowflake",
                        other.label()
                    )));
                }
            };
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
            let base = self.base.clone().unwrap_or_else(|| account_url(account));
            let mut session = SnowflakeSession {
                inner: Arc::new(Inner {
                    http,
                    base,
                    auth,
                    cancel: AtomicBool::new(false),
                    notify: Notify::new(),
                    running: Mutex::new(None),
                }),
                context: Context {
                    database: non_empty(&cfg.database),
                    schema: cfg.option("schema").map(str::to_owned),
                    warehouse: cfg.option("warehouse").map(str::to_owned),
                    role: cfg.option("role").map(str::to_owned),
                },
                version: String::new(),
                closed: false,
            };
            let version =
                session
                    .scalar("SELECT CURRENT_VERSION()")
                    .await
                    .map_err(|e| match e {
                        DbError::Server(s) => DbError::Connect(s.message),
                        other => other,
                    })?;
            session.version = format!("Snowflake {version}");
            Ok(Box::new(session) as Box<dyn DbSession>)
        })
    }
}

/// What the stream task and the cancel handle share with the session.
struct Inner {
    http: reqwest::Client,
    base: String,
    auth: Auth,
    cancel: AtomicBool,
    notify: Notify,
    /// Handle of the statement in flight, for server-side cancel.
    running: Mutex<Option<String>>,
}

impl Inner {
    fn set_running(&self, handle: Option<String>) {
        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = handle;
    }

    async fn cancelled(&self) {
        loop {
            let n = self.notify.notified();
            if self.cancel.load(Ordering::SeqCst) {
                return;
            }
            n.await;
        }
    }

    /// One request; the answer parsed whatever the HTTP status.
    async fn call(&self, req: reqwest::RequestBuilder) -> Result<(u16, Response)> {
        let req = req
            .header(AUTHORIZATION, self.auth.header()?)
            .header(
                "X-Snowflake-Authorization-Token-Type",
                self.auth.token_type(),
            )
            .header(ACCEPT, "application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "gzip");
        let response = tokio::select! {
            r = req.send() => r,
            () = self.cancelled() => return Err(DbError::Cancelled),
        };
        let response = response.map_err(|e| {
            if e.is_timeout() {
                DbError::Protocol("Snowflake did not answer in time".into())
            } else {
                DbError::Connect(e.to_string())
            }
        })?;
        let status = response.status().as_u16();
        let gzip = response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
        let body = response
            .bytes()
            .await
            .map_err(|e| DbError::Protocol(e.to_string()))?;
        let body = if gzip || body.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&body[..])
                .read_to_end(&mut out)
                .map_err(|e| DbError::Protocol(format!("bad compressed response: {e}")))?;
            out
        } else {
            body.to_vec()
        };
        if status == 401 || status == 403 {
            let message = serde_json::from_slice::<Response>(&body)
                .ok()
                .and_then(|r| r.message)
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(DbError::Connect(format!(
                "Snowflake rejected the sign-in: {message}.{}",
                self.auth.rejection_hint()
            )));
        }
        let parsed: Response = serde_json::from_slice(&body).map_err(|_| {
            DbError::Protocol(format!(
                "unexpected response from Snowflake (HTTP {status})"
            ))
        })?;
        Ok((status, parsed))
    }

    /// Wait for a statement to finish, then return its first page.
    async fn settle(&self, mut status: u16, mut r: Response, sql: &str) -> Result<Response> {
        let mut delay = Duration::from_millis(100);
        loop {
            match status {
                200 => return Ok(r),
                202 if r.code.as_deref() == Some(CODE_RUNNING) || r.statement_handle.is_some() => {
                    let handle = r
                        .statement_handle
                        .clone()
                        .ok_or_else(|| DbError::Protocol("no statement handle to poll".into()))?;
                    self.set_running(Some(handle.clone()));
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        () = self.cancelled() => return Err(DbError::Cancelled),
                    }
                    delay = (delay * 2).min(Duration::from_secs(2));
                    (status, r) = self
                        .call(
                            self.http
                                .get(format!("{}/api/v2/statements/{handle}", self.base)),
                        )
                        .await?;
                }
                408 => {
                    return Err(DbError::Protocol(
                        "the statement exceeded its timeout".into(),
                    ));
                }
                429 => return Err(DbError::Protocol("rate limited by Snowflake".into())),
                _ => return Err(server_error(&r, sql)),
            }
        }
    }

    async fn submit(&self, sql: &str, params: &[Value], ctx: &Context) -> Result<Response> {
        self.cancel.store(false, Ordering::SeqCst);
        let mut bindings = serde_json::Map::new();
        for (i, v) in params.iter().enumerate() {
            bindings.insert((i + 1).to_string(), binding(v));
        }
        let statements = SnowflakeDialect.split_script(sql).len();
        let mut parameters = serde_json::Map::new();
        parameters.insert(
            "MULTI_STATEMENT_COUNT".into(),
            Json::from(if statements > 1 { "0" } else { "1" }),
        );
        let body = SubmitRequest {
            statement: sql,
            timeout: 0,
            database: ctx.database.as_deref(),
            schema: ctx.schema.as_deref(),
            warehouse: ctx.warehouse.as_deref(),
            role: ctx.role.as_deref(),
            bindings,
            parameters,
        };
        let url = format!(
            "{}/api/v2/statements?requestId={}",
            self.base,
            uuid::Uuid::new_v4()
        );
        let (status, r) = self.call(self.http.post(url).json(&body)).await?;
        let r = self.settle(status, r, sql).await;
        self.set_running(None);
        r
    }

    /// A finished statement by handle (one of a multi-statement request).
    async fn fetch(&self, handle: &str, partition: Option<usize>, sql: &str) -> Result<Response> {
        let mut url = format!("{}/api/v2/statements/{handle}", self.base);
        if let Some(p) = partition {
            url.push_str(&format!("?partition={p}"));
        }
        let (status, r) = self.call(self.http.get(url)).await?;
        self.settle(status, r, sql).await
    }
}

/// A binding for one parameter (sent as text the server converts).
fn binding(v: &Value) -> Json {
    let (ty, value) = match v {
        Value::Null => ("TEXT", Json::Null),
        Value::Bool(b) => ("BOOLEAN", Json::from(b.to_string())),
        Value::Int(i) => ("FIXED", Json::from(i.to_string())),
        Value::Float(f) => ("REAL", Json::from(f.to_string())),
        Value::Numeric(s) => ("FIXED", Json::from(s.clone())),
        other => ("TEXT", Json::from(other.to_display())),
    };
    json!({ "type": ty, "value": value })
}

/// `syntax error line 2 at position 7` → position in the statement.
fn error_position(message: &str) -> Option<ErrorPosition> {
    let rest = &message[message.find(" line ")? + " line ".len()..];
    let line: u32 = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some(ErrorPosition::Line(line))
}

fn server_error(r: &Response, _sql: &str) -> DbError {
    let message = r
        .message
        .clone()
        .unwrap_or_else(|| "the statement failed".into());
    DbError::Server(Box::new(ServerError {
        severity: "ERROR".into(),
        position: error_position(&message),
        code: r.code.clone().or_else(|| r.sql_state.clone()),
        detail: r.sql_state.as_ref().map(|s| format!("SQLSTATE {s}")),
        hint: None,
        message,
    }))
}

/// `USE [DATABASE|SCHEMA|WAREHOUSE|ROLE] <name>`: what to change, if `sql` is one.
fn use_target(sql: &str) -> Option<(&'static str, String)> {
    let mut words = sql.trim().trim_end_matches(';').split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("USE") {
        return None;
    }
    let first = words.next()?;
    let (kind, name) = match first.to_ascii_uppercase().as_str() {
        k @ ("DATABASE" | "SCHEMA" | "WAREHOUSE" | "ROLE") => {
            let kind = match k {
                "DATABASE" => "database",
                "SCHEMA" => "schema",
                "WAREHOUSE" => "warehouse",
                _ => "role",
            };
            (kind, words.next()?)
        }
        _ => ("database", first),
    };
    if words.next().is_some() {
        return None;
    }
    // Unquoted names fold to upper case; quoted ones keep their spelling.
    let name = match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(quoted) => quoted.replace("\"\"", "\""),
        None => name.to_ascii_uppercase(),
    };
    Some((kind, name))
}

/// One Snowflake account.
pub struct SnowflakeSession {
    inner: Arc<Inner>,
    context: Context,
    version: String,
    closed: bool,
}

impl SnowflakeSession {
    /// The first column of the first row of a one-off query, as text.
    async fn scalar(&mut self, sql: &str) -> Result<String> {
        let r = self.inner.submit(sql, &[], &self.context).await?;
        Ok(r.data
            .as_ref()
            .and_then(|d| d.first())
            .and_then(|row| row.first())
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    /// All rows of a small query (catalog), as strings.
    pub(crate) async fn rows(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<(Vec<String>, Vec<Vec<Option<String>>>)> {
        let first = self.inner.submit(sql, params, &self.context).await?;
        let meta = first.result_set_meta_data.clone().unwrap_or_default();
        let names = meta.row_type.iter().map(|t| t.name.clone()).collect();
        let mut rows: Vec<Vec<Json>> = first.data.unwrap_or_default();
        if let Some(handle) = first.statement_handle.as_deref() {
            for p in 1..meta.partition_info.len() {
                let page = self.inner.fetch(handle, Some(p), sql).await?;
                rows.extend(page.data.unwrap_or_default());
            }
        }
        let rows = rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| match v {
                        Json::Null => None,
                        Json::String(s) => Some(s),
                        other => Some(other.to_string()),
                    })
                    .collect()
            })
            .collect();
        Ok((names, rows))
    }
}

type Tx = mpsc::Sender<Result<ResultEvent>>;

/// Stream one statement's result set (all partitions) into `tx`.
async fn send_result(
    inner: &Inner,
    first: Response,
    sql: &str,
    tx: &mut Tx,
) -> Result<Option<u64>> {
    let meta = first.result_set_meta_data.clone().unwrap_or_default();
    let affected = first
        .stats
        .as_ref()
        .map(|s| s.num_rows_inserted + s.num_rows_updated + s.num_rows_deleted);
    if meta.row_type.is_empty() {
        return Ok(affected);
    }
    let (columns, kinds): (std::sync::Arc<[ColumnMeta]>, _) = wire::columns(&meta.row_type);
    if tx
        .send(Ok(ResultEvent::Columns(columns.clone())))
        .await
        .is_err()
    {
        return Ok(affected);
    }
    let send_rows = async |rows: Vec<Vec<Json>>, tx: &mut Tx| -> bool {
        for b in wire::batches(&kinds, &columns, &rows) {
            if tx.send(Ok(ResultEvent::Rows(b))).await.is_err() {
                return false;
            }
        }
        true
    };
    if !send_rows(first.data.unwrap_or_default(), tx).await {
        return Ok(affected);
    }
    if let Some(handle) = first.statement_handle.as_deref() {
        for p in 1..meta.partition_info.len() {
            let page = inner.fetch(handle, Some(p), sql).await?;
            if !send_rows(page.data.unwrap_or_default(), tx).await {
                break;
            }
        }
    }
    Ok(affected)
}

async fn run(inner: Arc<Inner>, first: Response, sql: String, mut tx: Tx, started: Instant) {
    let result: Result<Option<u64>> = async {
        match first.statement_handles.clone() {
            Some(handles) if !handles.is_empty() => {
                let mut affected: Option<u64> = None;
                for (i, h) in handles.iter().enumerate() {
                    if i > 0 && tx.send(Ok(ResultEvent::NextResultSet)).await.is_err() {
                        return Ok(affected);
                    }
                    let r = inner.fetch(h, None, &sql).await?;
                    if let Some(n) = send_result(&inner, r, &sql, &mut tx).await? {
                        affected = Some(affected.unwrap_or(0) + n);
                    }
                }
                Ok(affected)
            }
            _ => send_result(&inner, first, &sql, &mut tx).await,
        }
    }
    .await;
    let event = result.map(|affected| {
        ResultEvent::Done(Completion {
            affected,
            elapsed: started.elapsed(),
        })
    });
    let _ = tx.send(event).await;
}

impl DbSession for SnowflakeSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            let started = Instant::now();
            debug!(params = params.len(), "snowflake query");
            let first = self.inner.submit(sql, params, &self.context).await?;
            if let Some((kind, name)) = use_target(sql) {
                match kind {
                    "database" => {
                        self.context.database = Some(name);
                        self.context.schema = None;
                    }
                    "schema" => self.context.schema = Some(name),
                    "warehouse" => self.context.warehouse = Some(name),
                    _ => self.context.role = Some(name),
                }
            }
            let (tx, rx) = mpsc::channel::<Result<ResultEvent>>(4);
            tokio::spawn(run(self.inner.clone(), first, sql.to_owned(), tx, started));
            Ok(rx.boxed())
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let inner = self.inner.clone();
        let flag = Arc::new(AtomicBool::new(false));
        CancelHandle::new(flag, move || {
            let inner = inner.clone();
            Box::pin(async move {
                inner.cancel.store(true, Ordering::SeqCst);
                inner.notify.notify_waiters();
                let handle = inner
                    .running
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(handle) = handle {
                    // The flag stops the client; this stops the warehouse.
                    let url = format!("{}/api/v2/statements/{handle}/cancel", inner.base);
                    let req = inner
                        .http
                        .post(url)
                        .header(AUTHORIZATION, inner.auth.header()?)
                        .header(
                            "X-Snowflake-Authorization-Token-Type",
                            inner.auth.token_type(),
                        )
                        .header(ACCEPT, "application/json");
                    req.send()
                        .await
                        .map_err(|e| DbError::Protocol(format!("cancel failed: {e}")))?;
                }
                Ok(())
            })
        })
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        Box::pin(async move { catalog::introspect(self, scope).await })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            Err(DbError::Unsupported(
                "Snowflake's SQL API runs each request on its own; transactions cannot span requests"
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

    #[test]
    fn account_urls() {
        assert_eq!(
            account_url("myorg-acct"),
            "https://myorg-acct.snowflakecomputing.com"
        );
        assert_eq!(
            account_url("https://xy1.us-east-1.snowflakecomputing.com/"),
            "https://xy1.us-east-1.snowflakecomputing.com"
        );
    }

    #[test]
    fn use_statements_update_context() {
        assert_eq!(
            use_target("use warehouse wh"),
            Some(("warehouse", "WH".into()))
        );
        assert_eq!(
            use_target("USE SCHEMA \"Mixed\";"),
            Some(("schema", "Mixed".into()))
        );
        assert_eq!(
            use_target("use analytics"),
            Some(("database", "ANALYTICS".into()))
        );
        assert_eq!(use_target("select 1"), None);
        assert_eq!(use_target("use secondary roles all"), None);
    }

    #[test]
    fn compile_errors_carry_the_line() {
        let r = Response {
            code: Some("001003".into()),
            message: Some(
                "SQL compilation error:\nsyntax error line 2 at position 7 unexpected 'x'.".into(),
            ),
            sql_state: Some("42000".into()),
            ..Default::default()
        };
        let e = server_error(&r, "");
        let s = e.as_server().expect("server error");
        assert_eq!(s.position, Some(ErrorPosition::Line(2)));
        assert_eq!(s.code.as_deref(), Some("001003"));
    }

    #[test]
    fn bindings_are_typed_text() {
        assert_eq!(
            binding(&Value::Int(3)),
            json!({"type": "FIXED", "value": "3"})
        );
        assert_eq!(
            binding(&Value::Null),
            json!({"type": "TEXT", "value": null})
        );
    }
}
