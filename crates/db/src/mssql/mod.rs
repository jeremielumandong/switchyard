//! SQL Server driver over `tiberius` (TDS 7.4/8.0, rustls).
//!
//! tiberius' result stream borrows the client mutably, and cancelling needs the client too
//! (`cancel_query` sends the TDS attention signal). So one task owns the client and serves
//! requests in order: a query streams its events through a bounded channel (back-pressure
//! reaches the socket), and a cancel makes the task drop the stream and send the attention.
//!
//! SQL Server can acknowledge the attention in a TDS message of its own, after the one that
//! ends the cancelled batch; tiberius stops reading at the first message boundary and reports
//! it never saw the acknowledgement, leaving the connection one response behind. The task then
//! reconnects and tells the user what state was lost (see `docs/DECISIONS.md`).

mod catalog;
mod decode;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures::future::BoxFuture;
use futures::{StreamExt as _, TryStreamExt as _};
use secrecy::ExposeSecret;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, Query, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};
use tracing::{debug, warn};

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, tsql::TSqlDialect};
use crate::driver::{
    CancelHandle, ComponentId, DbAuthMethod, DbConfig, DbSession, Driver, SslMode, TunnelEndpoint,
};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, DEFAULT_BATCH_ROWS, Notice, ResultEvent, ResultStream};
use crate::value::{Engine, Value};

type TdsClient = Client<Compat<TcpStream>>;

/// Rows in the first batch of a result, so the grid fills quickly.
const FIRST_BATCH_ROWS: usize = 200;

/// The SQL Server driver.
#[derive(Clone, Debug, Default)]
pub struct MssqlDriver;

pub(crate) fn map_error(e: tiberius::error::Error) -> DbError {
    match e {
        tiberius::error::Error::Server(t) => DbError::Server(Box::new(ServerError {
            severity: if t.class() > 10 { "ERROR" } else { "INFO" }.into(),
            code: Some(t.code().to_string()),
            message: t.message().to_owned(),
            detail: (!t.procedure().is_empty()).then(|| format!("in {}", t.procedure())),
            hint: None,
            position: (t.line() > 0).then(|| ErrorPosition::Line(t.line())),
        })),
        tiberius::error::Error::Io { message, .. } => DbError::Connect(message),
        tiberius::error::Error::Tls(m) => DbError::Tls(m),
        other => DbError::Protocol(other.to_string()),
    }
}

fn tiberius_config(cfg: &DbConfig, host: &str, port: u16) -> Result<Config> {
    let mut c = Config::new();
    c.host(host);
    c.port(port);
    if !cfg.database.is_empty() {
        c.database(&cfg.database);
    }
    c.application_name(&cfg.application_name);
    c.encryption(match cfg.ssl_mode {
        // SQL Server always encrypts the login; Disable keeps the rest in clear text.
        SslMode::Disable => EncryptionLevel::Off,
        SslMode::Prefer => EncryptionLevel::On,
        SslMode::Require | SslMode::VerifyFull => EncryptionLevel::Required,
    });
    if let Some(pem) = &cfg.trusted_ca_pem {
        c.trust_cert_ca_bundle(pem.as_bytes().to_vec());
    }
    c.readonly(cfg.read_only);
    match cfg.auth {
        DbAuthMethod::Password => {
            let pw = cfg
                .password
                .as_ref()
                .map(|p| p.expose_secret().to_owned())
                .unwrap_or_default();
            c.authentication(AuthMethod::sql_server(&cfg.user, pw));
        }
        DbAuthMethod::Integrated => {
            return Err(DbError::Unsupported(
                "integrated authentication to SQL Server arrives with the Driver Manager (M3-7)"
                    .into(),
            ));
        }
        DbAuthMethod::EntraInteractive
        | DbAuthMethod::EntraDeviceCode
        | DbAuthMethod::EntraPassword
        | DbAuthMethod::EntraServicePrincipal => {
            let token = cfg.access_token.as_ref().ok_or_else(|| {
                DbError::Connect("Microsoft Entra sign-in did not produce a token".into())
            })?;
            c.authentication(AuthMethod::aad_token(token.expose_secret()));
        }
    }
    Ok(c)
}

async fn open_client(cfg: &DbConfig, via: Option<&TunnelEndpoint>) -> Result<TdsClient> {
    crate::tls::install_default_provider();
    // The TCP target can be a tunnel; the TLS name is always the real server.
    let (mut host, mut port) = (cfg.host.clone(), cfg.port);
    for _redirect in 0..3 {
        let config = tiberius_config(cfg, &host, port)?;
        let (tcp_host, tcp_port) = match via {
            Some(t) => (t.host.clone(), t.port),
            None => (host.clone(), port),
        };
        let tcp = tokio::time::timeout(
            cfg.connect_timeout,
            TcpStream::connect((tcp_host.as_str(), tcp_port)),
        )
        .await
        .map_err(|_| DbError::Connect("timed out".into()))?
        .map_err(|e| DbError::Connect(e.to_string()))?;
        let _ = tcp.set_nodelay(true);
        match Client::connect(config, tcp.compat_write()).await {
            Ok(c) => return Ok(c),
            // Azure SQL gateways redirect to the node that holds the database.
            Err(tiberius::error::Error::Routing {
                host: h, port: p, ..
            }) if via.is_none() => {
                debug!(%h, p, "sql server redirect");
                host = h;
                port = p;
            }
            Err(e) => {
                return Err(match map_error(e) {
                    DbError::Server(s) => DbError::Connect(s.message),
                    other => other,
                });
            }
        }
    }
    Err(DbError::Connect("too many redirects".into()))
}

impl Driver for MssqlDriver {
    fn engine(&self) -> Engine {
        Engine::SqlServer
    }

    fn dialect(&self) -> &dyn Dialect {
        &TSqlDialect
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
            let mut client = open_client(cfg, via.as_ref()).await?;
            let version = server_version(&mut client).await.unwrap_or_default();
            let (tx, rx) = mpsc::unbounded_channel();
            let cancel = Arc::new(Notify::new());
            let requested = Arc::new(AtomicBool::new(false));
            let closed = Arc::new(AtomicBool::new(false));
            let in_txn = Arc::new(AtomicBool::new(false));
            tokio::spawn(actor(
                client,
                rx,
                Actor {
                    cfg: cfg.clone(),
                    via,
                    cancel: cancel.clone(),
                    requested: requested.clone(),
                    closed: closed.clone(),
                    in_txn: in_txn.clone(),
                },
            ));
            Ok(Box::new(MssqlSession {
                tx,
                cancel,
                requested,
                closed,
                in_txn,
                version,
            }) as Box<dyn DbSession>)
        })
    }
}

async fn server_version(client: &mut TdsClient) -> Result<String> {
    let rows = simple_rows(
        client,
        "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(64)), \
                CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), @@VERSION",
    )
    .await?;
    let row = rows.first().cloned().unwrap_or_default();
    let text = |i: usize| match row.get(i) {
        Some(Value::Text(s)) => s.clone(),
        _ => String::new(),
    };
    // "Microsoft SQL Server 2022 (RTM-CU12) ..." → "SQL Server 2022"
    let product = text(2)
        .lines()
        .next()
        .unwrap_or_default()
        .split(" (")
        .next()
        .unwrap_or_default()
        .trim_start_matches("Microsoft ")
        .to_owned();
    let product = if product.is_empty() {
        "SQL Server".to_owned()
    } else {
        product
    };
    let edition = text(1);
    let edition = edition.split(" (").next().unwrap_or_default();
    Ok(if edition.is_empty() {
        format!("{product} {}", text(0))
    } else {
        format!("{product} {} · {edition}", text(0))
    })
}

/// Run a batch and collect every row as owned values (catalog and small helpers).
pub(crate) async fn simple_rows(client: &mut TdsClient, sql: &str) -> Result<Vec<Vec<Value>>> {
    let stream = client.simple_query(sql).await.map_err(map_error)?;
    let rows = stream.into_first_result().await.map_err(map_error)?;
    Ok(rows
        .iter()
        .map(|r| r.cells().map(|(_, d)| decode::to_value(d)).collect())
        .collect())
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

struct Actor {
    cfg: DbConfig,
    via: Option<TunnelEndpoint>,
    cancel: Arc<Notify>,
    requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    in_txn: Arc<AtomicBool>,
}

async fn actor(mut client: TdsClient, mut rx: mpsc::UnboundedReceiver<Request>, a: Actor) {
    let Actor {
        cfg,
        via,
        cancel,
        requested,
        closed,
        in_txn,
    } = a;
    while let Some(req) = rx.recv().await {
        let next = match req {
            Request::Execute {
                sql,
                params,
                events,
            } => run_query(&mut client, &sql, &params, &events, &cancel, &requested).await,
            Request::Batch { sql, reply } => {
                let r = match client.simple_query(sql).await {
                    Ok(s) => s.into_results().await.map(|_| ()).map_err(map_error),
                    Err(e) => Err(map_error(e)),
                };
                let next = after(&r);
                let _ = reply.send(r);
                next
            }
            Request::Introspect { scope, reply } => {
                let r = catalog::introspect(&mut client, scope).await;
                let next = after(&r);
                let _ = reply.send(r);
                next
            }
        };
        match next {
            After::Ready => {}
            After::Closed => break,
            After::Reconnect => {
                warn!("sql server cancel left the connection out of step; reconnecting");
                in_txn.store(false, Ordering::SeqCst);
                let old = std::mem::replace(
                    &mut client,
                    match open_client(&cfg, via.as_ref()).await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(error = %e, "sql server reconnect failed");
                            break;
                        }
                    },
                );
                drop(old);
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
    let _ = client.close().await;
}

fn after<T>(r: &Result<T>) -> After {
    if matches!(r, Err(DbError::Connect(_))) {
        After::Closed
    } else {
        After::Ready
    }
}

/// Run one batch.
async fn run_query(
    client: &mut TdsClient,
    sql: &str,
    params: &[Value],
    events: &mpsc::Sender<Result<ResultEvent>>,
    cancel: &Notify,
    requested: &AtomicBool,
) -> After {
    let started = Instant::now();
    // The request future, the stream it yields and the cancel wait all borrow the
    // client; they end with this block so the attention can use it afterwards.
    let outcome = {
        let request = async {
            if params.is_empty() {
                client.simple_query(sql).await
            } else {
                let mut q = Query::new(sql);
                for p in params {
                    decode::bind(&mut q, p);
                }
                q.query(client).await
            }
        };
        tokio::pin!(request);
        // tiberius returns the stream only once the server answers, which can take as
        // long as the statement (`WAITFOR`), so the cancel wait starts here already.
        let started_stream = loop {
            tokio::select! {
                biased;
                () = cancel.notified() => {
                    if requested.load(Ordering::SeqCst) {
                        break None;
                    }
                }
                r = &mut request => break Some(r),
            }
        };
        match started_stream {
            None => Outcome::Cancelled,
            Some(Ok(stream)) => pump(stream, events, cancel, requested).await,
            Some(Err(e)) => Outcome::Failed(map_error(e)),
        }
    };
    match outcome {
        Outcome::Done { had_results } => {
            // tiberius' query stream drops DONE row counts; @@ROWCOUNT still holds the
            // last statement's count at the start of the next batch.
            let affected = if had_results {
                None
            } else {
                match simple_rows(client, "SELECT CAST(@@ROWCOUNT AS bigint)").await {
                    Ok(rows) => match rows.first().and_then(|r| r.first()) {
                        Some(Value::Int(n)) => Some(*n as u64),
                        _ => None,
                    },
                    Err(_) => None,
                }
            };
            let _ = events
                .send(Ok(ResultEvent::Done(Completion {
                    affected,
                    elapsed: started.elapsed(),
                })))
                .await;
            After::Ready
        }
        Outcome::Cancelled => {
            let t = Instant::now();
            let r = client.cancel_query().await;
            debug!(elapsed = ?t.elapsed(), ok = r.is_ok(), "sql server attention");
            let next = if r.is_ok() {
                After::Ready
            } else {
                let _ = events
                    .send(Ok(ResultEvent::Notice(Notice {
                        severity: "WARNING".into(),
                        code: None,
                        message: RESET_NOTICE.into(),
                    })))
                    .await;
                After::Reconnect
            };
            let _ = events.send(Err(DbError::Cancelled)).await;
            next
        }
        Outcome::ReceiverGone => {
            // Nobody reads the rest: stop the server instead of draining it.
            match client.cancel_query().await {
                Ok(_) => After::Ready,
                Err(_) => After::Reconnect,
            }
        }
        Outcome::Failed(e) => {
            let fatal = matches!(e, DbError::Connect(_) | DbError::Protocol(_));
            if fatal {
                warn!(error = %e, "sql server connection failed");
            }
            let _ = events.send(Err(e)).await;
            if fatal { After::Closed } else { After::Ready }
        }
    }
}

/// What the connection needs after a query.
#[derive(Debug, PartialEq, Eq)]
enum After {
    /// Ready for the next request.
    Ready,
    /// The attention acknowledgement could not be read, so the token stream is out of
    /// step with the server: open a fresh connection.
    Reconnect,
    /// Unusable.
    Closed,
}

const RESET_NOTICE: &str = "Cancelled. The server's acknowledgement could not be read, so \
    Switchyard reconnected: any open transaction was rolled back and temporary tables and \
    SET options were reset.";

enum Outcome {
    Done { had_results: bool },
    Cancelled,
    ReceiverGone,
    Failed(DbError),
}

async fn pump(
    mut stream: tiberius::QueryStream<'_>,
    events: &mpsc::Sender<Result<ResultEvent>>,
    cancel: &Notify,
    requested: &AtomicBool,
) -> Outcome {
    let mut builder: Option<(Arc<[ColumnMeta]>, RowBatchBuilder)> = None;
    let mut sets = 0usize;
    let mut sent_rows = false;
    loop {
        let item = tokio::select! {
            biased;
            // A wake-up left over from a cancel with nothing running is ignored.
            () = cancel.notified() => {
                if requested.load(Ordering::SeqCst) {
                    return Outcome::Cancelled;
                }
                continue;
            }
            item = stream.try_next() => item,
        };
        if requested.load(Ordering::SeqCst) {
            return Outcome::Cancelled;
        }
        match item {
            Ok(Some(QueryItem::Metadata(meta))) => {
                if let Some((_, b)) = builder.take()
                    && !b.is_empty()
                    && events
                        .send(Ok(ResultEvent::Rows(b.finish())))
                        .await
                        .is_err()
                {
                    return Outcome::ReceiverGone;
                }
                if sets > 0 && events.send(Ok(ResultEvent::NextResultSet)).await.is_err() {
                    return Outcome::ReceiverGone;
                }
                sets += 1;
                sent_rows = false;
                let cols: Arc<[ColumnMeta]> =
                    meta.columns().iter().map(decode::column_meta).collect();
                if events
                    .send(Ok(ResultEvent::Columns(cols.clone())))
                    .await
                    .is_err()
                {
                    return Outcome::ReceiverGone;
                }
                let b = RowBatchBuilder::for_columns(&cols, FIRST_BATCH_ROWS);
                builder = Some((cols, b));
            }
            Ok(Some(QueryItem::Row(row))) => {
                let Some((cols, b)) = builder.as_mut() else {
                    continue;
                };
                for (i, (_, data)) in row.cells().enumerate() {
                    decode::push(b, cols[i].data_type, data);
                }
                let limit = if sent_rows {
                    DEFAULT_BATCH_ROWS
                } else {
                    FIRST_BATCH_ROWS
                };
                if b.len() >= limit {
                    let full = std::mem::replace(
                        b,
                        RowBatchBuilder::for_columns(cols, DEFAULT_BATCH_ROWS),
                    );
                    sent_rows = true;
                    if events
                        .send(Ok(ResultEvent::Rows(full.finish())))
                        .await
                        .is_err()
                    {
                        return Outcome::ReceiverGone;
                    }
                }
            }
            Ok(None) => {
                if let Some((_, b)) = builder.take()
                    && !b.is_empty()
                    && events
                        .send(Ok(ResultEvent::Rows(b.finish())))
                        .await
                        .is_err()
                {
                    return Outcome::ReceiverGone;
                }
                return Outcome::Done {
                    had_results: sets > 0,
                };
            }
            Err(e) => return Outcome::Failed(map_error(e)),
        }
    }
}

/// A SQL Server session.
pub struct MssqlSession {
    tx: mpsc::UnboundedSender<Request>,
    cancel: Arc<Notify>,
    requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    /// Cleared by the connection task when it had to reconnect.
    in_txn: Arc<AtomicBool>,
    version: String,
}

impl MssqlSession {
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
}

impl DbSession for MssqlSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            self.requested.store(false, Ordering::SeqCst);
            // A few batches of buffer: enough to keep the socket busy, small enough that
            // a paused grid stops the server through TCP back-pressure.
            let (events, rx) = mpsc::channel(4);
            self.tx
                .send(Request::Execute {
                    sql: sql.to_owned(),
                    params: params.to_vec(),
                    events,
                })
                .map_err(|_| DbError::Closed)?;
            Ok(Box::pin(tokio_stream_from(rx)) as ResultStream)
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let notify = self.cancel.clone();
        let flag = self.requested.clone();
        CancelHandle::new(self.requested.clone(), move || {
            flag.store(true, Ordering::SeqCst);
            notify.notify_one();
            Box::pin(async { Ok(()) })
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
            self.batch("BEGIN TRANSACTION").await?;
            self.in_txn.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    fn commit(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let r = self.batch("IF @@TRANCOUNT > 0 COMMIT TRANSACTION").await;
            self.in_txn.store(false, Ordering::SeqCst);
            r
        })
    }

    fn rollback(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let r = self.batch("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
            self.in_txn.store(false, Ordering::SeqCst);
            r
        })
    }

    fn in_transaction(&self) -> bool {
        self.in_txn.load(Ordering::SeqCst)
    }

    fn server_version(&self) -> String {
        self.version.clone()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.tx.is_closed()
    }
}

fn tokio_stream_from(
    mut rx: mpsc::Receiver<Result<ResultEvent>>,
) -> impl futures::Stream<Item = Result<ResultEvent>> + Send + 'static {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).fuse()
}
