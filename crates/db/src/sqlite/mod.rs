//! Local SQLite driver through `rusqlite` (SQLite compiled in, nothing to install).
//!
//! `DbConfig::database` carries the file path (`~/` is expanded; `:memory:` opens a scratch
//! database). A missing file is created unless the connection is read-only. SQLite is a
//! blocking library, so each session owns one worker thread that holds the
//! `rusqlite::Connection`; the async side sends it jobs and receives result events over
//! channels. Cancel interrupts the running statement with `sqlite3_interrupt`.

mod catalog;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, ErrorCode, InterruptHandle, OpenFlags};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, sqlite::SqliteDialect};
use crate::driver::{CancelHandle, ComponentId, DbConfig, DbSession, Driver, TunnelEndpoint};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, DEFAULT_BATCH_ROWS, Notice, ResultEvent, ResultStream};
use crate::value::{DataType, Engine, Value};

/// How long a statement waits for another process's lock before failing with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Result batches buffered between the worker and the grid before the worker waits.
const EVENT_BUFFER: usize = 4;

/// The local SQLite driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteDriver;

impl Driver for SqliteDriver {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn dialect(&self) -> &dyn Dialect {
        &SqliteDialect::LOCAL
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
                    "a SQLite file is opened locally and cannot use an SSH tunnel".into(),
                ));
            }
            let path = resolve_path(&cfg.database)?;
            let read_only = cfg.read_only;
            let (ready_tx, ready_rx) = oneshot::channel();
            let (jobs, rx) = std_mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("sqlite".into())
                .spawn(move || worker(path, read_only, rx, ready_tx))
                .map_err(|e| DbError::Connect(e.to_string()))?;
            let opened = ready_rx
                .await
                .map_err(|_| DbError::Connect("the SQLite worker stopped".into()))??;
            Ok(Box::new(SqliteSession {
                jobs,
                interrupt: Arc::new(opened.interrupt),
                cancel: Arc::new(AtomicBool::new(false)),
                in_txn: Arc::new(AtomicBool::new(false)),
                closed: Arc::new(AtomicBool::new(false)),
                version: opened.version,
            }) as Box<dyn DbSession>)
        })
    }
}

/// The file to open: trimmed, `~/` expanded; `:memory:` and `file:` URIs pass through.
fn resolve_path(raw: &str) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DbError::Connect("choose a database file".into()));
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| DbError::Connect("cannot expand ~: no home directory".into()))?;
        return Ok(PathBuf::from(home).join(rest));
    }
    Ok(PathBuf::from(raw))
}

fn is_special(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    s == ":memory:" || s.starts_with("file:")
}

/// What the worker reports once the file is open.
struct Opened {
    interrupt: InterruptHandle,
    version: String,
}

/// Work run on the session's thread.
type Job = Box<dyn FnOnce(&mut Connection) + Send>;

fn open(path: &std::path::Path, read_only: bool) -> Result<Connection> {
    if read_only && !is_special(path) && !path.exists() {
        return Err(DbError::Connect(format!(
            "{} does not exist (a read-only connection does not create it)",
            path.display()
        )));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && !is_special(path)
        && !parent.is_dir()
    {
        return Err(DbError::Connect(format!(
            "folder {} does not exist",
            parent.display()
        )));
    }
    let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    flags |= if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    };
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| DbError::Connect(format!("cannot open {}: {e}", path.display())))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| DbError::Connect(e.to_string()))?;
    if read_only {
        conn.execute_batch("PRAGMA query_only = ON")
            .map_err(|e| DbError::Connect(e.to_string()))?;
    }
    // Reading the schema proves the file is a database (a text file opens fine and only
    // fails here with "file is not a database").
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
        .map_err(|e| DbError::Connect(e.to_string()))?;
    Ok(conn)
}

fn worker(
    path: PathBuf,
    read_only: bool,
    jobs: std_mpsc::Receiver<Job>,
    ready: oneshot::Sender<Result<Opened>>,
) {
    let mut conn = match open(&path, read_only) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let version = format!("SQLite {}", rusqlite::version());
    let opened = Opened {
        interrupt: conn.get_interrupt_handle(),
        version,
    };
    if ready.send(Ok(opened)).is_err() {
        return;
    }
    // Ends when the session (the only sender) is dropped; the connection closes with it.
    while let Ok(job) = jobs.recv() {
        job(&mut conn);
    }
}

/// One open SQLite file.
pub struct SqliteSession {
    jobs: std_mpsc::Sender<Job>,
    interrupt: Arc<InterruptHandle>,
    cancel: Arc<AtomicBool>,
    in_txn: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    version: String,
}

impl SqliteSession {
    /// Run `f` on the worker thread and wait for its answer.
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        let in_txn = self.in_txn.clone();
        self.submit(Box::new(move |conn| {
            let r = f(conn);
            in_txn.store(!conn.is_autocommit(), Ordering::SeqCst);
            let _ = tx.send(r);
        }))?;
        rx.await.map_err(|_| DbError::Closed)?
    }

    fn submit(&self, job: Job) -> Result<()> {
        self.jobs.send(job).map_err(|_| {
            self.closed.store(true, Ordering::SeqCst);
            DbError::Closed
        })
    }

    async fn exec_plain(&self, sql: &'static str) -> Result<()> {
        self.call(move |conn| conn.execute_batch(sql).map_err(|e| map_error(e, sql, 0)))
            .await
    }
}

impl DbSession for SqliteSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            self.cancel.store(false, Ordering::SeqCst);
            debug!(params = params.len(), "sqlite query");
            let (tx, mut rx) = mpsc::channel(EVENT_BUFFER);
            let sql = sql.to_owned();
            let params = params.to_vec();
            let cancel = self.cancel.clone();
            let in_txn = self.in_txn.clone();
            self.submit(Box::new(move |conn| {
                let sink = Sink { tx: &tx };
                if let Err(e) = run_script(conn, &sql, &params, &cancel, &sink) {
                    let e = if cancel.load(Ordering::SeqCst) {
                        DbError::Cancelled
                    } else {
                        e
                    };
                    sink.send(Err(e));
                }
                in_txn.store(!conn.is_autocommit(), Ordering::SeqCst);
            }))?;
            // The first event decides between an error (the statement did not run) and a
            // stream of results, like the network drivers.
            let first = rx.recv().await.ok_or(DbError::Closed)?;
            let first = first?;
            let rest = stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            });
            Ok(Box::pin(stream::once(async move { Ok(first) }).chain(rest)) as ResultStream)
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let interrupt = self.interrupt.clone();
        let flag = self.cancel.clone();
        CancelHandle::new(self.cancel.clone(), move || {
            flag.store(true, Ordering::SeqCst);
            interrupt.interrupt();
            Box::pin(async { Ok(()) })
        })
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        Box::pin(async move {
            self.call(move |conn| catalog::introspect(conn, scope))
                .await
        })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.exec_plain("BEGIN"))
    }

    fn commit(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.exec_plain("COMMIT"))
    }

    fn rollback(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.exec_plain("ROLLBACK"))
    }

    fn in_transaction(&self) -> bool {
        self.in_txn.load(Ordering::SeqCst)
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// The worker's end of a result stream.
struct Sink<'a> {
    tx: &'a mpsc::Sender<Result<ResultEvent>>,
}

impl Sink<'_> {
    /// Send one event, waiting while the grid catches up. `false` once nobody listens.
    fn send(&self, event: Result<ResultEvent>) -> bool {
        self.tx.blocking_send(event).is_ok()
    }
}

/// Run every statement of `sql` in order, streaming results. Returns early (without
/// `Done`) when the receiver goes away.
fn run_script(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    cancel: &AtomicBool,
    sink: &Sink<'_>,
) -> Result<()> {
    let started = Instant::now();
    let mut affected: Option<u64> = None;
    let mut sets = 0usize;
    for span in SqliteDialect::LOCAL.split_script(sql) {
        if cancel.load(Ordering::SeqCst) {
            return Err(DbError::Cancelled);
        }
        let text = span.text(sql);
        let mut stmt = conn
            .prepare(text)
            .map_err(|e| map_error(e, sql, span.start))?;
        bind(&mut stmt, params)?;
        if stmt.column_count() == 0 {
            stmt.raw_execute()
                .map_err(|e| map_error(e, sql, span.start))?;
            if !stmt.readonly() {
                affected = Some(affected.unwrap_or(0) + conn.changes());
            }
            continue;
        }
        if sets > 0 && !sink.send(Ok(ResultEvent::NextResultSet)) {
            return Ok(());
        }
        sets += 1;
        let writes = !stmt.readonly();
        if !stream_rows(&mut stmt, sink).map_err(|e| map_error(e, sql, span.start))? {
            return Ok(());
        }
        if writes {
            // INSERT … RETURNING and friends.
            affected = Some(affected.unwrap_or(0) + conn.changes());
        }
    }
    sink.send(Ok(ResultEvent::Done(Completion {
        affected,
        elapsed: started.elapsed(),
    })));
    Ok(())
}

/// Bind `params` by position: the dialect rewrites every placeholder to `?N`, so index `N`
/// is the same across the statements of a script.
fn bind(stmt: &mut rusqlite::Statement<'_>, params: &[Value]) -> Result<()> {
    for i in 1..=stmt.parameter_count() {
        let Some(v) = params.get(i - 1) else {
            let name = stmt.parameter_name(i).unwrap_or("?").to_owned();
            return Err(DbError::Param(format!("no value for parameter {name}")));
        };
        stmt.raw_bind_parameter(i, param_value(v))
            .map_err(|e| DbError::Param(e.to_string()))?;
    }
    Ok(())
}

fn param_value(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Int(i) => SqlValue::Integer(*i),
        Value::Float(f) => SqlValue::Real(*f),
        Value::Bytes(b) => SqlValue::Blob(b.clone()),
        Value::Text(s) | Value::Other(s) | Value::Numeric(s) | Value::Json(s) => {
            SqlValue::Text(s.clone())
        }
        Value::Date(_) | Value::Time(_) | Value::Timestamp(_) | Value::TimestampTz(_) => {
            // SQLite keeps dates as ISO-8601 text.
            SqlValue::Text(crate::dialect::temporal_text(v).unwrap_or_default())
        }
        other => SqlValue::Text(other.to_display()),
    }
}

/// Storage classes seen in a column (bit set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen(u8);

impl Seen {
    const INT: u8 = 1;
    const REAL: u8 = 2;
    const TEXT: u8 = 4;
    const BLOB: u8 = 8;

    fn add(&mut self, v: ValueRef<'_>) {
        self.0 |= match v {
            ValueRef::Null => 0,
            ValueRef::Integer(_) => Self::INT,
            ValueRef::Real(_) => Self::REAL,
            ValueRef::Text(_) => Self::TEXT,
            ValueRef::Blob(_) => Self::BLOB,
        };
    }
}

/// SQLite's type affinity of a declared column type (section 3.1 of the datatype docs).
fn affinity(decl: &str) -> DataType {
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        DataType::Int64
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        DataType::Text
    } else if d.contains("BLOB") {
        DataType::Bytes
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        DataType::Float64
    } else {
        DataType::Text
    }
}

/// The grid type of a column: SQLite types values, not columns, so the first batch's values
/// decide; the declared type only answers for a column that held nothing but NULLs.
fn column_type(decl: Option<&str>, seen: Seen) -> DataType {
    match seen.0 {
        0 => decl.map_or(DataType::Text, affinity),
        Seen::INT => DataType::Int64,
        x if x & !(Seen::INT | Seen::REAL) == 0 => DataType::Float64,
        Seen::BLOB => DataType::Bytes,
        _ => DataType::Text,
    }
}

fn type_name(decl: Option<&str>, data_type: DataType) -> String {
    match decl.filter(|d| !d.is_empty()) {
        Some(d) => d.to_ascii_uppercase(),
        None => match data_type {
            DataType::Int64 => "INTEGER",
            DataType::Float64 => "REAL",
            DataType::Bytes => "BLOB",
            _ => "TEXT",
        }
        .into(),
    }
}

/// A stable id for a source table, so inline editing can tell that every column comes
/// from the same one (FNV-1a over `database.table`).
fn table_id(database: &str, table: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in database.bytes().chain(*b".").chain(table.bytes()) {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Column name, declared type and origin, read before the statement runs.
struct ColumnSource {
    name: String,
    decl: Option<String>,
    table: Option<u32>,
}

fn column_sources(stmt: &rusqlite::Statement<'_>) -> Vec<ColumnSource> {
    let decls = stmt.columns();
    let origins = stmt.columns_with_metadata();
    decls
        .iter()
        .zip(&origins)
        .map(|(c, o)| ColumnSource {
            name: c.name().to_owned(),
            decl: c.decl_type().map(str::to_owned),
            table: match (o.table_name(), o.origin_name()) {
                (Some(t), Some(_)) => Some(table_id(o.database_name().unwrap_or("main"), t)),
                _ => None,
            },
        })
        .collect()
}

/// Append one cell to a column of a fixed type. `false` when the value does not fit (a
/// later row holding another storage class than the first batch showed); NULL goes in.
fn push_cell(b: &mut RowBatchBuilder, data_type: DataType, v: ValueRef<'_>) -> bool {
    match (data_type, v) {
        (_, ValueRef::Null) => b.push_null(),
        (DataType::Int64, ValueRef::Integer(i)) => b.push_i64(i),
        (DataType::Int64, ValueRef::Real(f))
            if f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64 =>
        {
            b.push_i64(f as i64)
        }
        (DataType::Float64, ValueRef::Integer(i)) => b.push_f64(i as f64),
        (DataType::Float64, ValueRef::Real(f)) => b.push_f64(f),
        (DataType::Bytes, ValueRef::Blob(x) | ValueRef::Text(x)) => b.push_bytes(x),
        (DataType::Text, ValueRef::Integer(i)) => b.push_i64(i),
        (DataType::Text, ValueRef::Real(f)) => b.push_f64(f),
        (DataType::Text, ValueRef::Text(x)) => b.push_str(&String::from_utf8_lossy(x)),
        (DataType::Text, ValueRef::Blob(x)) => b.push_bytes(x),
        _ => {
            b.push_null();
            return false;
        }
    }
    true
}

/// Stream one statement's rows: the first batch is read ahead to type the columns, the
/// rest goes straight into batches. `Ok(false)` once the receiver is gone.
fn stream_rows(stmt: &mut rusqlite::Statement<'_>, sink: &Sink<'_>) -> rusqlite::Result<bool> {
    let sources = column_sources(stmt);
    let n = sources.len();
    let mut rows = stmt.raw_query();

    let mut head: Vec<SqlValue> = Vec::new();
    let mut seen = vec![Seen::default(); n];
    let mut head_rows = 0usize;
    let mut more = false;
    while let Some(row) = rows.next()? {
        for (i, s) in seen.iter_mut().enumerate() {
            let v = row.get_ref(i)?;
            s.add(v);
            head.push(owned(v));
        }
        head_rows += 1;
        if head_rows == DEFAULT_BATCH_ROWS {
            more = true;
            break;
        }
    }

    let meta: Vec<ColumnMeta> = sources
        .iter()
        .zip(&seen)
        .enumerate()
        .map(|(i, (src, s))| {
            let dt = column_type(src.decl.as_deref(), *s);
            let mut m = ColumnMeta::new(src.name.clone(), type_name(src.decl.as_deref(), dt), dt);
            m.table_id = src.table;
            m.table_column = src.table.map(|_| i16::try_from(i + 1).unwrap_or(i16::MAX));
            m
        })
        .collect();
    let types: Vec<DataType> = meta.iter().map(|m| m.data_type).collect();
    if !sink.send(Ok(ResultEvent::Columns(Arc::from(meta)))) {
        return Ok(false);
    }

    let mut misfit = vec![false; n];
    let mut b = RowBatchBuilder::new(&types, DEFAULT_BATCH_ROWS);
    for (i, v) in head.iter().enumerate() {
        let c = i % n.max(1);
        if !push_cell(&mut b, types[c], v.into()) {
            misfit[c] = true;
        }
    }
    drop(head);
    if more {
        while let Some(row) = rows.next()? {
            for (c, t) in types.iter().enumerate() {
                if !push_cell(&mut b, *t, row.get_ref(c)?) {
                    misfit[c] = true;
                }
            }
            if b.is_full() && !sink.send(Ok(ResultEvent::Rows(b.take()))) {
                return Ok(false);
            }
        }
    }
    if !b.is_empty() && !sink.send(Ok(ResultEvent::Rows(b.finish()))) {
        return Ok(false);
    }
    let names: Vec<&str> = sources
        .iter()
        .zip(&misfit)
        .filter(|(_, m)| **m)
        .map(|(s, _)| s.name.as_str())
        .collect();
    if !names.is_empty() {
        let notice = Notice {
            severity: "WARNING".into(),
            code: None,
            message: format!(
                "{} held values of another type than the first rows; those cells show as NULL. CAST the column in the query to see them.",
                names.join(", ")
            ),
        };
        return Ok(sink.send(Ok(ResultEvent::Notice(notice))));
    }
    Ok(true)
}

fn owned(v: ValueRef<'_>) -> SqlValue {
    match v {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(i) => SqlValue::Integer(i),
        ValueRef::Real(f) => SqlValue::Real(f),
        ValueRef::Text(t) => SqlValue::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
    }
}

/// Name of a SQLite result code, e.g. `SQLITE_CONSTRAINT`.
fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::ConstraintViolation => "SQLITE_CONSTRAINT",
        ErrorCode::DatabaseBusy => "SQLITE_BUSY",
        ErrorCode::DatabaseLocked => "SQLITE_LOCKED",
        ErrorCode::ReadOnly => "SQLITE_READONLY",
        ErrorCode::TypeMismatch => "SQLITE_MISMATCH",
        ErrorCode::TooBig => "SQLITE_TOOBIG",
        ErrorCode::DiskFull => "SQLITE_FULL",
        ErrorCode::CannotOpen => "SQLITE_CANTOPEN",
        ErrorCode::PermissionDenied => "SQLITE_PERM",
        ErrorCode::AuthorizationForStatementDenied => "SQLITE_AUTH",
        ErrorCode::DatabaseCorrupt => "SQLITE_CORRUPT",
        ErrorCode::NotADatabase => "SQLITE_NOTADB",
        ErrorCode::SystemIoFailure => "SQLITE_IOERR",
        ErrorCode::SchemaChanged => "SQLITE_SCHEMA",
        _ => "SQLITE_ERROR",
    }
}

/// Map a rusqlite error for the statement starting at byte `start` of `sql`.
fn map_error(e: rusqlite::Error, sql: &str, start: usize) -> DbError {
    let (code, message, offset) = match e {
        rusqlite::Error::SqliteFailure(f, msg) => {
            if f.code == ErrorCode::OperationInterrupted {
                return DbError::Cancelled;
            }
            (
                Some(code_name(f.code)),
                msg.unwrap_or_else(|| f.to_string()),
                None,
            )
        }
        rusqlite::Error::SqlInputError {
            error, msg, offset, ..
        } => (
            Some(code_name(error.code)),
            msg,
            usize::try_from(offset).ok(),
        ),
        rusqlite::Error::MultipleStatement => (
            None,
            "several statements in one; separate them with ;".into(),
            None,
        ),
        other => (None, other.to_string(), None),
    };
    let position = offset.map(|o| {
        let mut b = (start + o).min(sql.len());
        while !sql.is_char_boundary(b) {
            b -= 1;
        }
        ErrorPosition::Offset(sql[..b].chars().count() as u32 + 1)
    });
    DbError::Server(Box::new(ServerError {
        severity: "ERROR".into(),
        code: code.map(str::to_owned),
        message,
        detail: None,
        hint: None,
        position,
    }))
}

#[cfg(test)]
mod tests;
