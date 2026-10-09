//! MySQL driver on `mysql_async` with `rustls` (also MariaDB, which speaks the same
//! protocol).
//!
//! A `mysql_async` connection is driven through `&mut`, so the session runs it on a task
//! and talks to it over a channel, like the SQL Server driver. Cancel opens a second
//! connection (through the same tunnel endpoint) and sends `KILL QUERY <id>`.

mod catalog;
mod decode;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use mysql_async::prelude::{Protocol, Queryable as _};
use mysql_async::{Conn, Opts, OptsBuilder, Params, QueryResult, SslOpts};
use secrecy::ExposeSecret;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, mysql::MySqlDialect};
use crate::driver::{
    CancelHandle, ComponentId, DbAuthMethod, DbConfig, DbSession, Driver, SslMode, TunnelEndpoint,
};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, DEFAULT_BATCH_ROWS, Notice, ResultEvent, ResultStream};
use crate::value::{Engine, Value};

/// Rows in the first batch of a result, so the grid paints quickly.
const FIRST_BATCH_ROWS: usize = 200;
/// `ER_QUERY_INTERRUPTED`: the statement was stopped by `KILL QUERY`.
const QUERY_INTERRUPTED: u16 = 1317;

/// The MySQL driver.
#[derive(Clone, Debug, Default)]
pub struct MySqlDriver;

/// The line of a syntax error: MySQL ends the message with `... at line N`.
fn error_line(message: &str) -> Option<u32> {
    let at = message.rfind("at line ")?;
    message[at + 8..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

pub(crate) fn map_error(e: mysql_async::Error, cancelled: bool) -> DbError {
    match e {
        mysql_async::Error::Server(s) => {
            if cancelled && s.code == QUERY_INTERRUPTED {
                return DbError::Cancelled;
            }
            DbError::Server(Box::new(ServerError {
                severity: "ERROR".into(),
                code: Some(s.code.to_string()),
                position: error_line(&s.message).map(ErrorPosition::Line),
                message: s.message,
                detail: (!s.state.is_empty()).then(|| format!("SQLSTATE {}", s.state)),
                hint: None,
            }))
        }
        mysql_async::Error::Io(io) => {
            let text = io.to_string();
            if matches!(io, mysql_async::IoError::Tls(_)) || text.contains("certificate") {
                DbError::Tls(format!(
                    "{text}. MySQL servers often use a self-signed certificate: trust its CA \
                     for this connection, or set SSL mode to disable."
                ))
            } else {
                DbError::Connect(text)
            }
        }
        mysql_async::Error::Driver(d) => DbError::Protocol(d.to_string()),
        other => DbError::Protocol(other.to_string()),
    }
}

/// TLS settings: the OS trust store plus the connection's own CA, verification on. Through
/// a tunnel the certificate is checked against the configured host, not `127.0.0.1`.
fn ssl_opts(cfg: &DbConfig, via: Option<&TunnelEndpoint>) -> SslOpts {
    let mut roots: Vec<Vec<u8>> = rustls_native_certs::load_native_certs()
        .certs
        .into_iter()
        .map(|c| c.as_ref().to_vec())
        .collect();
    if let Some(pem) = &cfg.trusted_ca_pem {
        roots.push(pem.as_bytes().to_vec());
    }
    let ssl = SslOpts::default()
        .with_root_certs(roots.into_iter().map(Into::into).collect())
        .with_disable_built_in_roots(true);
    if via.is_some() {
        ssl.with_danger_tls_hostname_override(Some(cfg.host.clone()))
    } else {
        ssl
    }
}

/// Connection options for `cfg`, reaching the server directly or through `via`.
fn opts(cfg: &DbConfig, via: Option<&TunnelEndpoint>, tls: bool) -> Result<Opts> {
    if cfg.auth != DbAuthMethod::Password {
        return Err(DbError::Unsupported(format!(
            "{} is not available for MySQL",
            cfg.auth.label()
        )));
    }
    let (host, port) = match via {
        Some(t) => (t.host.clone(), t.port),
        None => (cfg.host.clone(), cfg.port),
    };
    let b = OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(port)
        .user((!cfg.user.is_empty()).then(|| cfg.user.clone()))
        .pass(cfg.password.as_ref().map(|p| p.expose_secret().to_owned()))
        .db_name((!cfg.database.is_empty()).then(|| cfg.database.clone()))
        .tcp_nodelay(true)
        .connect_attribute("program_name", cfg.application_name.clone())
        .ssl_opts(tls.then(|| ssl_opts(cfg, via)));
    Ok(b.into())
}

async fn open(opts: Opts, cfg: &DbConfig) -> Result<Conn> {
    crate::tls::install_default_provider();
    tokio::time::timeout(cfg.connect_timeout, Conn::new(opts))
        .await
        .map_err(|_| DbError::Connect("timed out".into()))?
        .map_err(|e| match map_error(e, false) {
            DbError::Server(s) => DbError::Connect(s.message),
            DbError::Protocol(m) => DbError::Connect(m),
            other => other,
        })
}

/// Connect with the configured TLS policy. `prefer` falls back to plain TCP only when the
/// server has no TLS at all; an untrusted certificate is still an error.
async fn connect_with_policy(cfg: &DbConfig, via: Option<&TunnelEndpoint>) -> Result<(Conn, Opts)> {
    let tls = cfg.ssl_mode != SslMode::Disable;
    let first = opts(cfg, via, tls)?;
    match open(first.clone(), cfg).await {
        Ok(c) => Ok((c, first)),
        Err(DbError::Connect(m))
            if cfg.ssl_mode == SslMode::Prefer && m.contains("does not have this capability") =>
        {
            debug!("mysql server has no TLS; continuing without (ssl mode prefer)");
            let plain = opts(cfg, via, false)?;
            Ok((open(plain.clone(), cfg).await?, plain))
        }
        Err(e) => Err(e),
    }
}

/// `KILL QUERY <id>` over a second connection.
async fn kill_query(opts: Opts, connect_timeout: std::time::Duration, id: u32) -> Result<()> {
    let mut conn = tokio::time::timeout(connect_timeout, Conn::new(opts))
        .await
        .map_err(|_| DbError::Connect("timed out".into()))?
        .map_err(|e| map_error(e, false))?;
    let r = conn
        .query_drop(format!("KILL QUERY {id}"))
        .await
        .map_err(|e| map_error(e, false));
    let _ = conn.disconnect().await;
    r
}

/// `MySQL 8.4.2` or `MariaDB 11.4.2` from `SELECT VERSION()`.
fn product_version(version: &str) -> String {
    let number = version.split('-').next().unwrap_or(version);
    if version.to_ascii_lowercase().contains("mariadb") {
        format!("MariaDB {number}")
    } else {
        format!("MySQL {number}")
    }
}

/// Rows of a small query (catalog, version), as owned values. Uses a prepared statement
/// when there are parameters.
pub(crate) async fn simple_rows(
    conn: &mut Conn,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<Vec<mysql_async::Value>>> {
    let rows: Vec<mysql_async::Row> = if params.is_empty() {
        conn.query(sql).await
    } else {
        conn.exec(
            sql,
            Params::Positional(params.iter().map(decode::param).collect()),
        )
        .await
    }
    .map_err(|e| map_error(e, false))?;
    Ok(rows
        .into_iter()
        .map(|r| {
            r.unwrap_raw()
                .into_iter()
                .map(|v| v.unwrap_or(mysql_async::Value::NULL))
                .collect()
        })
        .collect())
}

impl Driver for MySqlDriver {
    fn engine(&self) -> Engine {
        Engine::MySql
    }

    fn dialect(&self) -> &dyn Dialect {
        &MySqlDialect
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
            let (mut conn, opts) = connect_with_policy(cfg, via.as_ref()).await?;
            let version = simple_rows(&mut conn, "SELECT VERSION()", Vec::new())
                .await?
                .first()
                .map(|r| decode::text(r.first()))
                .map(|v| product_version(&v))
                .unwrap_or_else(|| "MySQL".into());
            if cfg.read_only {
                conn.query_drop("SET SESSION TRANSACTION READ ONLY")
                    .await
                    .map_err(|e| map_error(e, false))?;
            }
            debug!(version = %version, "mysql session open");
            let id = conn.id();
            let (tx, rx) = mpsc::unbounded_channel();
            let requested = Arc::new(AtomicBool::new(false));
            let closed = Arc::new(AtomicBool::new(false));
            let kill = Killer {
                opts,
                connect_timeout: cfg.connect_timeout,
                id,
            };
            tokio::spawn(actor(
                conn,
                rx,
                kill.clone(),
                requested.clone(),
                closed.clone(),
            ));
            Ok(Box::new(MySqlSession {
                tx,
                kill,
                requested,
                closed,
                in_txn: false,
                version,
            }) as Box<dyn DbSession>)
        })
    }
}

/// What a cancel needs: where to open the second connection and which thread to stop.
#[derive(Clone)]
struct Killer {
    opts: Opts,
    connect_timeout: std::time::Duration,
    id: u32,
}

impl Killer {
    async fn kill(&self) -> Result<()> {
        kill_query(self.opts.clone(), self.connect_timeout, self.id).await
    }
}

enum Request {
    Execute {
        sql: String,
        params: Vec<Value>,
        events: mpsc::Sender<Result<ResultEvent>>,
    },
    Batch {
        sql: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Introspect {
        scope: IntrospectScope,
        reply: oneshot::Sender<Result<CatalogChunk>>,
    },
}

async fn actor(
    mut conn: Conn,
    mut rx: mpsc::UnboundedReceiver<Request>,
    kill: Killer,
    requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
) {
    while let Some(req) = rx.recv().await {
        let fatal = match req {
            Request::Execute {
                sql,
                params,
                events,
            } => run_query(&mut conn, &sql, &params, &events, &kill, &requested).await,
            Request::Batch { sql, reply } => {
                let r = conn.query_drop(sql).await.map_err(|e| map_error(e, false));
                let fatal = is_fatal(&r);
                let _ = reply.send(r);
                fatal
            }
            Request::Introspect { scope, reply } => {
                let r = catalog::introspect(&mut conn, scope).await;
                let fatal = is_fatal(&r);
                let _ = reply.send(r);
                fatal
            }
        };
        if fatal || conn.is_disconnected() {
            break;
        }
    }
    closed.store(true, Ordering::SeqCst);
    let _ = conn.disconnect().await;
}

fn is_fatal<T>(r: &Result<T>) -> bool {
    matches!(r, Err(DbError::Connect(_) | DbError::Closed))
}

/// How the result stream ended.
enum Outcome {
    Done {
        sets: usize,
        affected: Option<u64>,
        warnings: u16,
    },
    ReceiverGone,
    Failed(mysql_async::Error),
}

/// Run one statement or script and stream its results. Returns whether the connection
/// is unusable afterwards.
async fn run_query(
    conn: &mut Conn,
    sql: &str,
    params: &[Value],
    events: &mpsc::Sender<Result<ResultEvent>>,
    kill: &Killer,
    requested: &AtomicBool,
) -> bool {
    let started = Instant::now();
    let outcome = if params.is_empty() {
        match conn.query_iter(sql).await {
            Ok(r) => pump(r, events, kill).await,
            Err(e) => Outcome::Failed(e),
        }
    } else {
        let bound = Params::Positional(params.iter().map(decode::param).collect());
        match conn.exec_iter(sql, bound).await {
            Ok(r) => pump(r, events, kill).await,
            Err(e) => Outcome::Failed(e),
        }
    };
    match outcome {
        Outcome::Done {
            sets,
            affected,
            warnings,
        } => {
            if warnings > 0 {
                for n in show_warnings(conn).await {
                    if events.send(Ok(ResultEvent::Notice(n))).await.is_err() {
                        return false;
                    }
                }
            }
            let _ = events
                .send(Ok(ResultEvent::Done(Completion {
                    affected: if sets > 0 { None } else { affected },
                    elapsed: started.elapsed(),
                })))
                .await;
            false
        }
        // `pump` stopped the statement and drained what was left.
        Outcome::ReceiverGone => false,
        Outcome::Failed(e) => {
            let fatal = e.is_fatal();
            if fatal {
                warn!(error = %e, "mysql connection failed");
            }
            let _ = events
                .send(Err(map_error(e, requested.load(Ordering::SeqCst))))
                .await;
            fatal
        }
    }
}

/// `SHOW WARNINGS` after a statement that reported some.
async fn show_warnings(conn: &mut Conn) -> Vec<Notice> {
    match simple_rows(conn, "SHOW WARNINGS", Vec::new()).await {
        Ok(rows) => rows
            .iter()
            .map(|r| Notice {
                severity: decode::text(r.first()).to_ascii_uppercase(),
                code: Some(decode::text(r.get(1))).filter(|c| !c.is_empty()),
                message: decode::text(r.get(2)),
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Stream every result set of `result` into `events`.
async fn pump<P: Protocol>(
    mut result: QueryResult<'_, 'static, P>,
    events: &mpsc::Sender<Result<ResultEvent>>,
    kill: &Killer,
) -> Outcome {
    let mut sets = 0usize;
    let mut affected = None;
    let mut warnings = 0u16;
    while let Some(cols) = result.columns() {
        warnings = warnings.max(result.warnings());
        if cols.is_empty() {
            // An OK packet (DML, DDL, SET): no rows, just a count.
            affected = Some(result.affected_rows());
            if let Err(e) = result.next().await {
                return Outcome::Failed(e);
            }
        } else {
            if sets > 0 && events.send(Ok(ResultEvent::NextResultSet)).await.is_err() {
                return gone(result, kill).await;
            }
            sets += 1;
            let meta: Arc<[ColumnMeta]> = cols.iter().map(decode::column_meta).collect();
            if events
                .send(Ok(ResultEvent::Columns(meta.clone())))
                .await
                .is_err()
            {
                return gone(result, kill).await;
            }
            let mut builder = RowBatchBuilder::for_columns(&meta, FIRST_BATCH_ROWS);
            let mut limit = FIRST_BATCH_ROWS;
            loop {
                match result.next().await {
                    Ok(Some(row)) => {
                        for (i, m) in meta.iter().enumerate() {
                            decode::push(&mut builder, m.data_type, row.as_ref(i));
                        }
                        if builder.len() >= limit {
                            let full = std::mem::replace(
                                &mut builder,
                                RowBatchBuilder::for_columns(&meta, DEFAULT_BATCH_ROWS),
                            );
                            limit = DEFAULT_BATCH_ROWS;
                            if events
                                .send(Ok(ResultEvent::Rows(full.finish())))
                                .await
                                .is_err()
                            {
                                return gone(result, kill).await;
                            }
                        }
                    }
                    Ok(None) => {
                        // The set's closing packet carries its warning count.
                        warnings = warnings.max(result.warnings());
                        break;
                    }
                    Err(e) => {
                        if !builder.is_empty() {
                            let _ = events.send(Ok(ResultEvent::Rows(builder.finish()))).await;
                        }
                        return Outcome::Failed(e);
                    }
                }
            }
            if !builder.is_empty()
                && events
                    .send(Ok(ResultEvent::Rows(builder.finish())))
                    .await
                    .is_err()
            {
                return gone(result, kill).await;
            }
        }
        if result.is_empty() {
            break;
        }
    }
    Outcome::Done {
        sets,
        affected,
        warnings,
    }
}

/// The reader went away: stop the statement rather than stream it to nowhere, and drain
/// what the server still sends so the connection stays usable.
async fn gone<P: Protocol>(result: QueryResult<'_, 'static, P>, kill: &Killer) -> Outcome {
    let (killed, _) = tokio::join!(kill.kill(), result.drop_result());
    if let Err(e) = killed {
        debug!(error = %e, "mysql kill after the reader left failed");
    }
    Outcome::ReceiverGone
}

/// A MySQL session.
pub struct MySqlSession {
    tx: mpsc::UnboundedSender<Request>,
    kill: Killer,
    requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    in_txn: bool,
    version: String,
}

impl MySqlSession {
    async fn batch(&mut self, sql: &str) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Batch {
                sql: sql.to_owned(),
                reply,
            })
            .map_err(|_| DbError::Closed)?;
        rx.await.map_err(|_| DbError::Closed)?
    }

    /// Follow `START TRANSACTION` / `BEGIN` / `COMMIT` / `ROLLBACK` run as statements.
    fn track_transaction(&mut self, sql: &str) {
        let mut words = sql
            .split(|c: char| c.is_whitespace() || c == ';')
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_uppercase);
        let first = words.next().unwrap_or_default();
        let second = words.next().unwrap_or_default();
        match (first.as_str(), second.as_str()) {
            ("START", "TRANSACTION") | ("BEGIN", "" | "WORK") => self.in_txn = true,
            ("ROLLBACK", s) if s == "TO" || words.next().is_some_and(|w| w == "TO") => {}
            ("COMMIT" | "ROLLBACK", _) => self.in_txn = false,
            _ => {}
        }
    }
}

impl DbSession for MySqlSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Err(DbError::Closed);
            }
            self.requested.store(false, Ordering::SeqCst);
            self.track_transaction(sql);
            // A few batches of buffer: a paused grid stops the server through TCP
            // back-pressure.
            let (events, rx) = mpsc::channel(4);
            self.tx
                .send(Request::Execute {
                    sql: sql.to_owned(),
                    params: params.to_vec(),
                    events,
                })
                .map_err(|_| DbError::Closed)?;
            let mut rx = rx;
            Ok(futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
                .fuse()
                .boxed())
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let kill = self.kill.clone();
        CancelHandle::new(self.requested.clone(), move || {
            let kill = kill.clone();
            Box::pin(async move { kill.kill().await })
        })
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        Box::pin(async move {
            let (reply, rx) = oneshot::channel();
            self.tx
                .send(Request::Introspect { scope, reply })
                .map_err(|_| DbError::Closed)?;
            rx.await.map_err(|_| DbError::Closed)?
        })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.batch("START TRANSACTION").await?;
            self.in_txn = true;
            Ok(())
        })
    }

    fn commit(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let r = self.batch("COMMIT").await;
            self.in_txn = false;
            r
        })
    }

    fn rollback(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let r = self.batch("ROLLBACK").await;
            self.in_txn = false;
            r
        })
    }

    fn in_transaction(&self) -> bool {
        self.in_txn
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.tx.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(product_version("8.4.2"), "MySQL 8.4.2");
        assert_eq!(product_version("11.4.2-MariaDB-ubu2404"), "MariaDB 11.4.2");
        assert_eq!(product_version("8.0.36-0ubuntu0.22.04.1"), "MySQL 8.0.36");
    }

    #[test]
    fn syntax_error_lines() {
        assert_eq!(
            error_line(
                "You have an error in your SQL syntax; check the manual that corresponds to \
                 your MySQL server version for the right syntax to use near 'FORM t' at line 2"
            ),
            Some(2)
        );
        assert_eq!(error_line("Table 'x.t' doesn't exist"), None);
    }

    fn txn_after(sqls: &[&str]) -> bool {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut s = MySqlSession {
            tx,
            kill: Killer {
                opts: OptsBuilder::default().into(),
                connect_timeout: std::time::Duration::from_secs(1),
                id: 0,
            },
            requested: Arc::default(),
            closed: Arc::default(),
            in_txn: false,
            version: String::new(),
        };
        for sql in sqls {
            s.track_transaction(sql);
        }
        s.in_txn
    }

    #[test]
    fn transaction_tracking() {
        assert!(txn_after(&["START TRANSACTION READ ONLY"]));
        assert!(txn_after(&["begin;"]));
        assert!(!txn_after(&["begin", "commit"]));
        assert!(txn_after(&["begin", "ROLLBACK TO SAVEPOINT a"]));
        assert!(txn_after(&["begin", "rollback work to a"]));
        assert!(!txn_after(&["begin", "rollback"]));
        assert!(!txn_after(&["START REPLICA"]));
    }
}
