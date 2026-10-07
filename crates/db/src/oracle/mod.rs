//! Oracle Database driver through the `oracle` crate (ODPI-C). ODPI-C loads the Oracle
//! Client library (Instant Client) at runtime: nothing Oracle is linked at build time.
//! Core passes the Driver Manager's install folder as `DbConfig::options["client_lib_dir"]`.
//!
//! The client API is blocking, so every call runs on tokio's blocking pool; a query
//! streams its rows from there through a bounded channel. Stop calls `OCIBreak` from
//! another thread, which ends the running call with ORA-01013.
//!
//! `DbConfig::host` / `port` / `database` give an Easy Connect string
//! (`//host:port/service`); a `database` that is a TNS alias or a full descriptor is used
//! as is when `host` is empty.

mod catalog;
mod decode;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use oracle::sql_type::{OracleType, ToSql};
use oracle::{Connection, Connector, InitParams};
use secrecy::ExposeSecret;
use tokio::sync::mpsc;
use tracing::debug;

use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, oracle::OracleDialect};
use crate::driver::{
    CancelHandle, ComponentId, DbAuthMethod, DbConfig, DbSession, Driver, TunnelEndpoint,
};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, Notice, ResultEvent, ResultStream};
use crate::value::{Engine, Value};

/// The Driver Manager component that provides the client library.
pub const CLIENT_COMPONENT: &str = "oracle-instant-client";

/// Rows fetched per round trip.
const FETCH_ROWS: u32 = 1000;

/// The Oracle driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct OracleDriver;

/// Initialize ODPI-C once, from the Driver Manager's folder when given. A failed load is
/// not cached, so installing the client later works without a restart.
fn init_client(lib_dir: Option<&str>) -> Result<()> {
    let mut params = InitParams::new();
    params
        .default_driver_name(format!("Switchyard : {}", env!("CARGO_PKG_VERSION")))
        .map_err(ora_error)?;
    if let Some(dir) = lib_dir {
        params.oracle_client_lib_dir(dir).map_err(ora_error)?;
    }
    params.init().map(|_| ()).map_err(|e| {
        let text = e.to_string();
        let fix = if text.contains("libnnz") || text.contains("libclntshcore") {
            " Restart Switchyard to finish setting up Oracle Instant Client."
        } else if text.contains("libaio") {
            // Ubuntu 24.04+ renamed the library; Oracle documents this link.
            " Instant Client needs libaio: install libaio1 (libaio on Fedora / RHEL), or on \
             Ubuntu 24.04 and later: sudo apt install libaio1t64 && sudo ln -s \
             /usr/lib/x86_64-linux-gnu/libaio.so.1t64 /usr/lib/x86_64-linux-gnu/libaio.so.1"
        } else {
            " Install it from Settings → Drivers."
        };
        DbError::Unsupported(format!(
            "Oracle Instant Client could not be loaded: {text}.{fix}"
        ))
    })
}

/// `//host:port/service`, or `database` itself (TNS alias, descriptor, Easy Connect).
pub fn connect_string(cfg: &DbConfig, via: Option<&TunnelEndpoint>) -> String {
    let (host, port) = match via {
        Some(t) => (t.host.clone(), t.port),
        None => (cfg.host.trim().to_owned(), cfg.port),
    };
    let db = cfg.database.trim();
    if host.is_empty() {
        return db.to_owned();
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    format!("//{host}:{port}/{db}")
}

fn ora_error(e: oracle::Error) -> DbError {
    match e.db_error() {
        Some(db) => {
            let code = db.code();
            if code == 1013 {
                return DbError::Cancelled;
            }
            let message = db.message().trim_end().to_owned();
            let offset = db.offset();
            DbError::Server(Box::new(ServerError {
                severity: "ERROR".into(),
                code: Some(format!("ORA-{code:05}")),
                message,
                detail: None,
                hint: None,
                // ODPI-C offsets are 0-based characters; 0 also means "unknown".
                position: (offset > 0).then_some(ErrorPosition::Offset(offset + 1)),
            }))
        }
        None => DbError::Protocol(e.to_string()),
    }
}

impl Driver for OracleDriver {
    fn engine(&self) -> Engine {
        Engine::Oracle
    }

    fn dialect(&self) -> &dyn Dialect {
        &OracleDialect
    }

    fn requirements(&self, _cfg: &DbConfig) -> Vec<ComponentId> {
        vec![ComponentId(CLIENT_COMPONENT.into())]
    }

    fn connect<'a>(
        &'a self,
        cfg: &'a DbConfig,
        via: Option<TunnelEndpoint>,
    ) -> BoxFuture<'a, Result<Box<dyn DbSession>>> {
        Box::pin(async move {
            if cfg.auth != DbAuthMethod::Password {
                return Err(DbError::Unsupported(format!(
                    "{} is not available for Oracle",
                    cfg.auth.label()
                )));
            }
            let lib_dir = cfg.option("client_lib_dir").map(str::to_owned);
            let user = cfg.user.trim().to_owned();
            let password = cfg
                .password
                .as_ref()
                .map(|p| p.expose_secret().to_owned())
                .unwrap_or_default();
            let target = connect_string(cfg, via.as_ref());
            let app = cfg.application_name.clone();
            let timeout = cfg.connect_timeout;
            let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
                init_client(lib_dir.as_deref())?;
                let mut c = Connector::new(user, password, target);
                c.driver_name(app.clone());
                let conn = c.connect().map_err(|e| match ora_error(e) {
                    DbError::Server(s) => {
                        DbError::Connect(format!("{}: {}", s.code.unwrap_or_default(), s.message))
                    }
                    other => other,
                })?;
                conn.set_call_timeout(Some(timeout)).map_err(ora_error)?;
                let _ = conn.set_module(&app);
                // DBMS_OUTPUT lines come back as notices.
                let _ = conn.execute("BEGIN DBMS_OUTPUT.ENABLE(NULL); END;", &[]);
                // The call timeout guards the login only; statements run until stopped.
                conn.set_call_timeout(None).map_err(ora_error)?;
                Ok(conn)
            })
            .await
            .map_err(|e| DbError::Connect(e.to_string()))??;
            let version = conn
                .server_version()
                .map(|(v, banner)| {
                    let first = banner.lines().next().unwrap_or_default().to_owned();
                    if first.is_empty() {
                        format!("Oracle {v}")
                    } else {
                        first
                    }
                })
                .unwrap_or_else(|_| "Oracle".into());
            Ok(Box::new(OracleSession {
                conn: Arc::new(conn),
                busy: Arc::new(Mutex::new(())),
                in_tx: false,
                version,
                cancel: Arc::new(AtomicBool::new(false)),
            }) as Box<dyn DbSession>)
        })
    }
}

/// One Oracle connection.
pub struct OracleSession {
    conn: Arc<Connection>,
    /// Held by the worker running a statement, so two never interleave.
    busy: Arc<Mutex<()>>,
    in_tx: bool,
    version: String,
    cancel: Arc<AtomicBool>,
}

/// A parameter as the client library takes it.
fn param(v: &Value) -> Box<dyn ToSql + Send> {
    match v {
        Value::Null => Box::new(None::<String>),
        Value::Bool(b) => Box::new(i64::from(*b)),
        Value::Int(i) => Box::new(*i),
        Value::Float(f) => Box::new(*f),
        Value::Bytes(b) => Box::new(b.clone()),
        other => Box::new(other.to_display()),
    }
}

/// DBMS_OUTPUT lines produced by the last call.
fn dbms_output(conn: &Connection) -> Vec<String> {
    let mut lines = Vec::new();
    let Ok(mut stmt) = conn
        .statement("BEGIN DBMS_OUTPUT.GET_LINE(:line, :status); END;")
        .build()
    else {
        return lines;
    };
    // Bounded: a runaway producer must not hang the session.
    for _ in 0..100_000 {
        let line_ty = OracleType::Varchar2(32767);
        let status_ty = OracleType::Number(0, 0);
        if stmt
            .execute_named(&[("line", &line_ty), ("status", &status_ty)])
            .is_err()
        {
            break;
        }
        let status: i64 = stmt.bind_value("status").unwrap_or(1);
        if status != 0 {
            break;
        }
        let line: Option<String> = stmt.bind_value("line").unwrap_or(None);
        lines.push(line.unwrap_or_default());
    }
    lines
}

type Tx = mpsc::Sender<Result<ResultEvent>>;

/// Run one statement on the blocking pool, sending its events.
fn run(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    autocommit: bool,
    tx: &Tx,
) -> Result<Option<u64>> {
    let (bound_sql, names) = OracleDialect.bind_params(sql);
    let bound: Vec<(String, Box<dyn ToSql + Send>)> = names
        .iter()
        .enumerate()
        .map(|(i, _)| {
            (
                format!("p{}", i + 1),
                param(params.get(i).unwrap_or(&Value::Null)),
            )
        })
        .collect();
    let named: Vec<(&str, &dyn ToSql)> = bound
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_ref() as &dyn ToSql))
        .collect();
    let plsql = crate::dialect::oracle::is_plsql(sql);
    // SQL must not end with `;`; PL/SQL must keep its final `END;`.
    let text = if plsql {
        bound_sql.as_str()
    } else {
        bound_sql.trim_end().trim_end_matches(';')
    };
    let mut stmt = conn
        .statement(text)
        .fetch_array_size(FETCH_ROWS)
        .prefetch_rows(FETCH_ROWS)
        .build()
        .map_err(ora_error)?;
    if stmt.is_query() {
        let rows = stmt.query_named(&named).map_err(ora_error)?;
        let (meta, kinds) = decode::columns(rows.column_info());
        if tx
            .blocking_send(Ok(ResultEvent::Columns(meta.clone())))
            .is_err()
        {
            return Ok(None);
        }
        let mut builder = crate::batch::RowBatchBuilder::for_columns(&meta, FETCH_ROWS as usize);
        for row in rows {
            let row = row.map_err(ora_error)?;
            decode::push_row(&mut builder, &kinds, &row);
            if builder.is_full()
                && tx
                    .blocking_send(Ok(ResultEvent::Rows(builder.take())))
                    .is_err()
            {
                return Ok(None);
            }
        }
        if !builder.is_empty() {
            let _ = tx.blocking_send(Ok(ResultEvent::Rows(builder.take())));
        }
        return Ok(None);
    }
    stmt.execute_named(&named).map_err(ora_error)?;
    let affected = if stmt.is_dml() {
        stmt.row_count().ok()
    } else {
        None
    };
    if autocommit && stmt.is_dml() {
        conn.commit().map_err(ora_error)?;
    }
    if stmt.is_plsql() {
        for line in dbms_output(conn) {
            let _ = tx.blocking_send(Ok(ResultEvent::Notice(Notice {
                severity: "OUTPUT".into(),
                code: None,
                message: line,
            })));
        }
    }
    Ok(affected)
}

impl DbSession for OracleSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            debug!(params = params.len(), "oracle query");
            self.cancel.store(false, Ordering::SeqCst);
            let started = Instant::now();
            let (tx, mut rx) = mpsc::channel::<Result<ResultEvent>>(4);
            let conn = self.conn.clone();
            let busy = self.busy.clone();
            let sql = sql.to_owned();
            let params = params.to_vec();
            let autocommit = !self.in_tx;
            tokio::task::spawn_blocking(move || {
                let _guard = busy
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let result = run(&conn, &sql, &params, autocommit, &tx);
                let event = result.map(|affected| {
                    ResultEvent::Done(Completion {
                        affected,
                        elapsed: started.elapsed(),
                    })
                });
                let _ = tx.blocking_send(event);
            });
            let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
            Ok(stream.boxed())
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let conn = self.conn.clone();
        let flag = self.cancel.clone();
        CancelHandle::new(self.cancel.clone(), move || {
            flag.store(true, Ordering::SeqCst);
            let conn = conn.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || conn.break_execution())
                    .await
                    .map_err(|e| DbError::Protocol(e.to_string()))?
                    .map_err(ora_error)
            })
        })
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        let conn = self.conn.clone();
        let busy = self.busy.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let _guard = busy
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                catalog::introspect(&conn, scope)
            })
            .await
            .map_err(|e| DbError::Protocol(e.to_string()))?
        })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Oracle starts a transaction with the first change; stop committing for it.
            self.in_tx = true;
            Ok(())
        })
    }

    fn commit(&mut self) -> BoxFuture<'_, Result<()>> {
        let conn = self.conn.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || conn.commit())
                .await
                .map_err(|e| DbError::Protocol(e.to_string()))?
                .map_err(ora_error)?;
            self.in_tx = false;
            Ok(())
        })
    }

    fn rollback(&mut self) -> BoxFuture<'_, Result<()>> {
        let conn = self.conn.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || conn.rollback())
                .await
                .map_err(|e| DbError::Protocol(e.to_string()))?
                .map_err(ora_error)?;
            self.in_tx = false;
            Ok(())
        })
    }

    fn in_transaction(&self) -> bool {
        self.in_tx
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn is_closed(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_strings() {
        let mut cfg = DbConfig::new(Engine::Oracle, "db.example.com", "ORCLPDB1");
        assert_eq!(connect_string(&cfg, None), "//db.example.com:1521/ORCLPDB1");
        let via = TunnelEndpoint {
            host: "127.0.0.1".into(),
            port: 40001,
        };
        assert_eq!(
            connect_string(&cfg, Some(&via)),
            "//127.0.0.1:40001/ORCLPDB1"
        );
        cfg.host = "::1".into();
        assert_eq!(connect_string(&cfg, None), "//[::1]:1521/ORCLPDB1");
        cfg.host.clear();
        cfg.database = "PROD_TNS".into();
        assert_eq!(connect_string(&cfg, None), "PROD_TNS");
    }
}
