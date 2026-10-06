//! PostgreSQL driver on `tokio-postgres` with `rustls`.

pub mod catalog;
pub mod decode;
pub mod params;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{FutureExt, SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::net::TcpStream;
use tokio_postgres::tls::{MakeTlsConnect, NoTls};
use tokio_postgres::{AsyncMessage, Client, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::{debug, warn};

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, postgres::PostgresDialect};
use crate::driver::{
    CancelHandle, ComponentId, DbConfig, DbSession, Driver, SslMode, TunnelEndpoint,
};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, DEFAULT_BATCH_ROWS, Notice, ResultEvent, ResultStream};
use crate::value::{DataType, Engine, Value};
use decode::RawCell;
use params::PgParam;

/// The PostgreSQL driver.
#[derive(Clone, Debug, Default)]
pub struct PgDriver;

/// Build a rustls connector with the platform's root certificates.
pub fn tls_connector() -> Result<MakeRustlsConnect> {
    Ok(MakeRustlsConnect::new(crate::tls::client_config()?))
}

pub(crate) fn map_error(e: tokio_postgres::Error, cancelled: bool) -> DbError {
    if let Some(db) = e.as_db_error() {
        if cancelled && db.code().code() == "57014" {
            return DbError::Cancelled;
        }
        return DbError::Server(Box::new(ServerError {
            severity: db.severity().to_owned(),
            code: Some(db.code().code().to_owned()),
            message: db.message().to_owned(),
            detail: db.detail().map(str::to_owned),
            hint: db.hint().map(str::to_owned),
            position: match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => {
                    Some(ErrorPosition::Offset(*p))
                }
                _ => None,
            },
        }));
    }
    if e.is_closed() {
        return DbError::Closed;
    }
    DbError::Protocol(e.to_string())
}

fn notice_from(db: &tokio_postgres::error::DbError) -> Notice {
    Notice {
        severity: db.severity().to_owned(),
        code: Some(db.code().code().to_owned()),
        message: db.message().to_owned(),
    }
}

/// Where and how to reach the server again (cancel requests open a second connection).
#[derive(Clone)]
struct Endpoint {
    connect_host: String,
    connect_port: u16,
    tls_host: String,
    tls: Option<MakeRustlsConnect>,
}

impl Driver for PgDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn dialect(&self) -> &dyn Dialect {
        &PostgresDialect
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
            let mut pg = tokio_postgres::Config::new();
            pg.dbname(&cfg.database)
                .user(&cfg.user)
                .application_name(&cfg.application_name)
                .connect_timeout(cfg.connect_timeout)
                .ssl_mode(match cfg.ssl_mode {
                    SslMode::Disable => tokio_postgres::config::SslMode::Disable,
                    SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
                    SslMode::Require | SslMode::VerifyFull => {
                        tokio_postgres::config::SslMode::Require
                    }
                });
            if let Some(pw) = &cfg.password {
                pg.password(pw.expose_secret());
            }
            let (connect_host, connect_port) = match &via {
                Some(t) => (t.host.clone(), t.port),
                None => (cfg.host.clone(), cfg.port),
            };
            let endpoint = Endpoint {
                connect_host,
                connect_port,
                tls_host: cfg.host.clone(),
                tls: match cfg.ssl_mode {
                    SslMode::Disable => None,
                    _ => Some(tls_connector()?),
                },
            };
            let stream = tokio::time::timeout(
                cfg.connect_timeout,
                TcpStream::connect((endpoint.connect_host.as_str(), endpoint.connect_port)),
            )
            .await
            .map_err(|_| DbError::Connect("timed out".into()))?
            .map_err(|e| DbError::Connect(e.to_string()))?;
            let _ = stream.set_nodelay(true);

            let notices: Arc<Mutex<Vec<Notice>>> = Arc::default();
            let closed = Arc::new(AtomicBool::new(false));
            let (client, task) = match &endpoint.tls {
                Some(tls) => {
                    let mut tls = tls.clone();
                    let connector =
                        <MakeRustlsConnect as MakeTlsConnect<TcpStream>>::make_tls_connect(
                            &mut tls,
                            &endpoint.tls_host,
                        )
                        .map_err(|e| DbError::Tls(e.to_string()))?;
                    let (client, conn) = pg
                        .connect_raw(stream, connector)
                        .await
                        .map_err(connect_error)?;
                    (
                        client,
                        Box::pin(drive(conn, notices.clone(), closed.clone()))
                            as BoxFuture<'static, ()>,
                    )
                }
                None => {
                    let (client, conn) =
                        pg.connect_raw(stream, NoTls).await.map_err(connect_error)?;
                    (
                        client,
                        Box::pin(drive(conn, notices.clone(), closed.clone()))
                            as BoxFuture<'static, ()>,
                    )
                }
            };
            let task = tokio::spawn(task);
            let client = Arc::new(client);
            let version = match client.simple_query("SHOW server_version").await {
                Ok(msgs) => msgs
                    .iter()
                    .find_map(|m| match m {
                        SimpleQueryMessage::Row(r) => r.get(0).map(|v| {
                            let short = v.split_whitespace().next().unwrap_or(v);
                            format!("PostgreSQL {short}")
                        }),
                        _ => None,
                    })
                    .unwrap_or_else(|| "PostgreSQL".into()),
                Err(e) => return Err(map_error(e, false)),
            };
            if cfg.read_only {
                client
                    .batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
                    .await
                    .map_err(|e| map_error(e, false))?;
            }
            debug!(version = %version, "postgres session open");
            Ok(Box::new(PgSession {
                client,
                endpoint,
                notices,
                closed,
                cancel: Arc::new(AtomicBool::new(false)),
                in_txn: false,
                version,
                task,
            }) as Box<dyn DbSession>)
        })
    }
}

fn connect_error(e: tokio_postgres::Error) -> DbError {
    match map_error(e, false) {
        DbError::Server(s) => DbError::Connect(s.message),
        DbError::Protocol(m) => DbError::Connect(m),
        other => other,
    }
}

/// Drive the connection, collecting notices, until it closes.
async fn drive<S, T>(
    mut conn: tokio_postgres::Connection<S, T>,
    notices: Arc<Mutex<Vec<Notice>>>,
    closed: Arc<AtomicBool>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match futures::future::poll_fn(|cx| conn.poll_message(cx)).await {
            Some(Ok(AsyncMessage::Notice(n))) => {
                if let Ok(mut q) = notices.lock() {
                    q.push(notice_from(&n));
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                warn!(error = %e, "postgres connection error");
                break;
            }
            None => break,
        }
    }
    closed.store(true, Ordering::SeqCst);
}

/// An open PostgreSQL session.
pub struct PgSession {
    client: Arc<Client>,
    endpoint: Endpoint,
    notices: Arc<Mutex<Vec<Notice>>>,
    closed: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    in_txn: bool,
    version: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PgSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PgSession {
    /// The underlying client, for catalog and plan queries.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    fn track_transaction(&mut self, sql: &str) {
        let first: String = sql
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect::<String>()
            .to_ascii_uppercase();
        match first.as_str() {
            "BEGIN" | "START" => self.in_txn = true,
            "COMMIT" | "ROLLBACK" | "END" | "ABORT" => self.in_txn = false,
            _ => {}
        }
    }
}

type Tx = mpsc::Sender<Result<ResultEvent>>;

fn drain_notices(notices: &Arc<Mutex<Vec<Notice>>>) -> Vec<Notice> {
    notices
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

async fn send_notices(tx: &mut Tx, notices: &Arc<Mutex<Vec<Notice>>>) -> bool {
    for n in drain_notices(notices) {
        if tx.send(Ok(ResultEvent::Notice(n))).await.is_err() {
            return false;
        }
    }
    true
}

/// Run a prepared (extended protocol) statement, streaming rows into `tx`.
struct Job {
    client: Arc<Client>,
    notices: Arc<Mutex<Vec<Notice>>>,
    cancel: Arc<AtomicBool>,
    tx: Tx,
    started: Instant,
}

async fn run_extended(job: Job, stmt: tokio_postgres::Statement, params: Vec<Value>) {
    let Job {
        client,
        notices,
        cancel,
        mut tx,
        started,
    } = job;
    let meta: Vec<ColumnMeta> = stmt
        .columns()
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_owned(),
            type_name: c.type_().name().to_owned(),
            data_type: decode::data_type(c.type_()),
            table_id: c.table_oid(),
            table_column: c.column_id(),
        })
        .collect();
    let types: Vec<(DataType, tokio_postgres::types::Type)> = stmt
        .columns()
        .iter()
        .map(|c| (decode::data_type(c.type_()), c.type_().clone()))
        .collect();
    let has_columns = !meta.is_empty();
    if has_columns
        && tx
            .send(Ok(ResultEvent::Columns(meta.clone().into())))
            .await
            .is_err()
    {
        return;
    }
    let bound: Vec<PgParam<'_>> = params.iter().map(PgParam).collect();
    let rows = match client.query_raw(&stmt, bound.iter()).await {
        Ok(r) => r,
        Err(e) => {
            let _ = send_notices(&mut tx, &notices).await;
            let _ = tx
                .send(Err(map_error(e, cancel.load(Ordering::SeqCst))))
                .await;
            return;
        }
    };
    futures::pin_mut!(rows);
    // The first batch is small so the grid paints quickly; later batches are full size.
    let mut capacity = 200;
    let mut builder = RowBatchBuilder::for_columns(&meta, capacity);
    let mut scratch = String::new();
    loop {
        // Take every row already buffered; flush when we would otherwise wait.
        let next = match rows.next().now_or_never() {
            Some(item) => item,
            None => {
                if !builder.is_empty() {
                    let batch = std::mem::replace(
                        &mut builder,
                        RowBatchBuilder::for_columns(&meta, DEFAULT_BATCH_ROWS),
                    )
                    .finish();
                    capacity = DEFAULT_BATCH_ROWS;
                    if tx.send(Ok(ResultEvent::Rows(batch))).await.is_err() {
                        return;
                    }
                }
                rows.next().await
            }
        };
        match next {
            Some(Ok(row)) => {
                for (i, (dt, ty)) in types.iter().enumerate() {
                    match row.try_get::<_, Option<RawCell<'_>>>(i) {
                        Ok(Some(raw)) => {
                            decode::push_value(&mut builder, *dt, ty, raw.0, &mut scratch)
                        }
                        _ => builder.push_null(),
                    }
                }
                if builder.len() >= capacity {
                    let batch = std::mem::replace(
                        &mut builder,
                        RowBatchBuilder::for_columns(&meta, DEFAULT_BATCH_ROWS),
                    )
                    .finish();
                    capacity = DEFAULT_BATCH_ROWS;
                    if tx.send(Ok(ResultEvent::Rows(batch))).await.is_err() {
                        return;
                    }
                }
            }
            Some(Err(e)) => {
                if !builder.is_empty() {
                    let _ = tx.send(Ok(ResultEvent::Rows(builder.finish()))).await;
                }
                let _ = send_notices(&mut tx, &notices).await;
                let _ = tx
                    .send(Err(map_error(e, cancel.load(Ordering::SeqCst))))
                    .await;
                return;
            }
            None => break,
        }
    }
    if !builder.is_empty()
        && tx
            .send(Ok(ResultEvent::Rows(builder.finish())))
            .await
            .is_err()
    {
        return;
    }
    if !send_notices(&mut tx, &notices).await {
        return;
    }
    let affected = if has_columns {
        None
    } else {
        rows.rows_affected()
    };
    let _ = tx
        .send(Ok(ResultEvent::Done(Completion {
            affected,
            elapsed: started.elapsed(),
        })))
        .await;
}

/// Run a multi-statement string through the simple protocol (text results).
async fn run_simple(job: Job, sql: String) {
    let Job {
        client,
        notices,
        cancel,
        mut tx,
        started,
    } = job;
    let stream = match client.simple_query_raw(&sql).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx
                .send(Err(map_error(e, cancel.load(Ordering::SeqCst))))
                .await;
            return;
        }
    };
    futures::pin_mut!(stream);
    let mut builder: Option<RowBatchBuilder> = None;
    let mut sets = 0usize;
    let mut last_affected = None;
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(SimpleQueryMessage::RowDescription(cols)) => {
                if sets > 0 && tx.send(Ok(ResultEvent::NextResultSet)).await.is_err() {
                    return;
                }
                sets += 1;
                let meta: Vec<ColumnMeta> = cols
                    .iter()
                    .map(|c| ColumnMeta::new(c.name(), "text", DataType::Text))
                    .collect();
                builder = Some(RowBatchBuilder::for_columns(&meta, DEFAULT_BATCH_ROWS));
                if tx
                    .send(Ok(ResultEvent::Columns(meta.clone().into())))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Ok(SimpleQueryMessage::Row(row)) => {
                if let Some(b) = builder.as_mut() {
                    for i in 0..row.len() {
                        match row.get(i) {
                            Some(v) => b.push_str(v),
                            None => b.push_null(),
                        }
                    }
                    if b.is_full() {
                        let batch = b.take();
                        if tx.send(Ok(ResultEvent::Rows(batch))).await.is_err() {
                            return;
                        }
                    }
                }
            }
            Ok(SimpleQueryMessage::CommandComplete(n)) => {
                last_affected = Some(n);
                if let Some(b) = builder.take()
                    && !b.is_empty()
                    && tx.send(Ok(ResultEvent::Rows(b.finish()))).await.is_err()
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let _ = send_notices(&mut tx, &notices).await;
                let _ = tx
                    .send(Err(map_error(e, cancel.load(Ordering::SeqCst))))
                    .await;
                return;
            }
        }
    }
    if !send_notices(&mut tx, &notices).await {
        return;
    }
    let affected = if sets > 0 { None } else { last_affected };
    let _ = tx
        .send(Ok(ResultEvent::Done(Completion {
            affected,
            elapsed: started.elapsed(),
        })))
        .await;
}

fn is_multi_statement_error(e: &tokio_postgres::Error) -> bool {
    e.as_db_error().is_some_and(|d| {
        d.message()
            .contains("cannot insert multiple commands into a prepared statement")
    })
}

impl DbSession for PgSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Err(DbError::Closed);
            }
            self.cancel.store(false, Ordering::SeqCst);
            self.track_transaction(sql);
            let started = Instant::now();
            let (tx, rx) = mpsc::channel::<Result<ResultEvent>>(4);
            let client = self.client.clone();
            let notices = self.notices.clone();
            let cancel = self.cancel.clone();
            let sql_owned = sql.to_owned();
            match client.prepare(sql).await {
                Ok(stmt) => {
                    if stmt.params().len() != params.len() {
                        return Err(DbError::Param(format!(
                            "statement expects {} parameter(s), got {}",
                            stmt.params().len(),
                            params.len()
                        )));
                    }
                    let params = params.to_vec();
                    let job = Job {
                        client,
                        notices,
                        cancel,
                        tx,
                        started,
                    };
                    tokio::spawn(run_extended(job, stmt, params));
                }
                Err(e) if is_multi_statement_error(&e) && params.is_empty() => {
                    let job = Job {
                        client,
                        notices,
                        cancel,
                        tx,
                        started,
                    };
                    tokio::spawn(run_simple(job, sql_owned));
                }
                Err(e) => return Err(map_error(e, self.cancel.load(Ordering::SeqCst))),
            }
            Ok(Box::pin(rx) as ResultStream)
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let token = self.client.cancel_token();
        let endpoint = self.endpoint.clone();
        CancelHandle::new(self.cancel.clone(), move || {
            let token = token.clone();
            let endpoint = endpoint.clone();
            Box::pin(async move {
                let stream =
                    TcpStream::connect((endpoint.connect_host.as_str(), endpoint.connect_port))
                        .await
                        .map_err(|e| DbError::Connect(e.to_string()))?;
                match endpoint.tls {
                    Some(mut tls) => {
                        let connector =
                            <MakeRustlsConnect as MakeTlsConnect<TcpStream>>::make_tls_connect(
                                &mut tls,
                                &endpoint.tls_host,
                            )
                            .map_err(|e| DbError::Tls(e.to_string()))?;
                        token.cancel_query_raw(stream, connector).await
                    }
                    None => token.cancel_query_raw(stream, NoTls).await,
                }
                .map_err(|e| map_error(e, false))
            })
        })
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        let client = self.client.clone();
        Box::pin(async move { catalog::introspect(&client, scope).await })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.client
                .batch_execute("BEGIN")
                .await
                .map_err(|e| map_error(e, false))?;
            self.in_txn = true;
            Ok(())
        })
    }

    fn commit(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.client
                .batch_execute("COMMIT")
                .await
                .map_err(|e| map_error(e, false))?;
            self.in_txn = false;
            Ok(())
        })
    }

    fn rollback(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.client
                .batch_execute("ROLLBACK")
                .await
                .map_err(|e| map_error(e, false))?;
            self.in_txn = false;
            Ok(())
        })
    }

    fn in_transaction(&self) -> bool {
        self.in_txn
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.client.is_closed()
    }
}
