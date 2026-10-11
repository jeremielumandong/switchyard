//! MongoDB driver over the official `mongodb` crate.
//!
//! Statements are written in a mongosh subset ([`shell`]) and each becomes one server
//! command; cursor results stream as columnar batches whose columns are inferred from
//! the documents ([`flatten`]). Databases show as schemas in the explorer, collections
//! and views as its objects. `DbConfig::host` may list several `host[:port]` seeds
//! separated by commas; `DbConfig::options` takes `auth_source`, `replica_set` and
//! `srv` (`true` resolves a `mongodb+srv://` seed list, as MongoDB Atlas gives it).

mod catalog;
pub mod edit;
pub mod flatten;
pub mod shell;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use mongodb::{Client, Cursor};
use secrecy::ExposeSecret;
use tokio::sync::Notify;
use tracing::debug;

use crate::batch::ColumnMeta;
use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::{Dialect, mongo::MongoDialect};
use crate::driver::{
    CancelHandle, ComponentId, DbConfig, DbSession, Driver, SslMode, TunnelEndpoint,
};
use crate::error::{DbError, ErrorPosition, Result, ServerError};
use crate::stream::{Completion, DEFAULT_BATCH_ROWS, Notice, ResultEvent, ResultStream};
use crate::value::{DataType, Engine, Value};
use flatten::Layout;
use shell::{Op, Shape};

/// Database used when the profile names none (the shell's default).
pub const DEFAULT_DATABASE: &str = "test";

/// The MongoDB driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct MongoDriver;

/// `host[:port]` seeds from a comma-separated list.
fn seeds(server: &str, default_port: u16) -> Result<Vec<ServerAddress>> {
    let mut out = Vec::new();
    for part in server.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (host, port) = match part.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') || h.starts_with('[') => (
                h.trim_start_matches('[').trim_end_matches(']'),
                p.parse::<u16>()
                    .map_err(|_| DbError::Connect(format!("invalid port in `{part}`")))?,
            ),
            _ => (part, default_port),
        };
        out.push(ServerAddress::Tcp {
            host: host.to_owned(),
            port: Some(port),
        });
    }
    if out.is_empty() {
        return Err(DbError::Connect("a host is required".into()));
    }
    Ok(out)
}

fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("true" | "1" | "yes" | "on"))
}

/// A per-connection CA written where the driver can read it (it takes a file path).
/// The file has a random name, is created exclusively (never through an existing path) and
/// is readable by this user only; it is removed when the returned handle drops, so the
/// session keeps it for as long as its client may reconnect.
fn ca_file(pem: &str) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let stage = |e: std::io::Error| DbError::Tls(format!("could not stage the CA: {e}"));
    let mut f = tempfile::Builder::new()
        .prefix("switchyard-mongo-ca-")
        .suffix(".pem")
        .tempfile()
        .map_err(stage)?;
    f.write_all(pem.as_bytes()).map_err(stage)?;
    f.flush().map_err(stage)?;
    Ok(f)
}

/// The client options, plus the staged CA file they point at (kept alive by the session).
async fn client_options(
    cfg: &DbConfig,
    via: Option<&TunnelEndpoint>,
) -> Result<(ClientOptions, Option<tempfile::NamedTempFile>)> {
    let srv = truthy(cfg.option("srv"));
    let mut o = if srv && via.is_none() {
        let host = cfg.host.trim();
        if host.is_empty() || host.contains(',') {
            return Err(DbError::Connect(
                "an SRV connection takes one host name (cluster0.abcde.mongodb.net)".into(),
            ));
        }
        ClientOptions::parse(format!("mongodb+srv://{host}/"))
            .await
            .map_err(|e| DbError::Connect(message(&e)))?
    } else {
        let mut o = ClientOptions::default();
        o.hosts = match via {
            Some(t) => vec![ServerAddress::Tcp {
                host: t.host.clone(),
                port: Some(t.port),
            }],
            None => seeds(&cfg.host, cfg.port)?,
        };
        o
    };
    if via.is_some() || (!srv && o.hosts.len() == 1 && cfg.option("replica_set").is_none()) {
        // One seed (or a tunnel's local port): talk to that server only, whatever the
        // replica set advertises (its member names are not reachable from here).
        o.direct_connection = Some(true);
    }
    if let Some(rs) = cfg.option("replica_set") {
        o.repl_set_name = Some(rs.to_owned());
    }
    o.app_name = Some(cfg.application_name.clone());
    o.connect_timeout = Some(cfg.connect_timeout);
    o.server_selection_timeout = Some(cfg.connect_timeout);
    let user = cfg.user.trim();
    if !user.is_empty() {
        let mut c = Credential::default();
        c.username = Some(user.to_owned());
        c.password = cfg.password.as_ref().map(|p| p.expose_secret().to_owned());
        c.source = Some(
            cfg.option("auth_source")
                .map(str::to_owned)
                .unwrap_or_else(|| "admin".into()),
        );
        o.credential = Some(c);
    }
    let tls_on = match cfg.ssl_mode {
        SslMode::Require | SslMode::VerifyFull => true,
        // MongoDB cannot negotiate TLS: "prefer" keeps what the seed list says (on for
        // SRV, off otherwise).
        SslMode::Prefer => srv,
        SslMode::Disable => false,
    };
    let mut ca = None;
    o.tls = Some(if tls_on {
        let mut t = TlsOptions::default();
        if let Some(pem) = &cfg.trusted_ca_pem {
            let f = ca_file(pem)?;
            t.ca_file_path = Some(f.path().to_path_buf());
            ca = Some(f);
        }
        Tls::Enabled(t)
    } else {
        Tls::Disabled
    });
    Ok((o, ca))
}

impl Driver for MongoDriver {
    fn engine(&self) -> Engine {
        Engine::MongoDb
    }

    fn dialect(&self) -> &dyn Dialect {
        &MongoDialect
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
            let (options, ca_file) = client_options(cfg, via.as_ref()).await?;
            let (client, info) = match open(options.clone()).await {
                Ok(c) => c,
                Err(e) if is_auth_failure(&e) => {
                    return Err(DbError::Connect(auth_failure(&e, &options, cfg).await));
                }
                Err(e) => return Err(DbError::Connect(message(&e))),
            };
            let version = format!(
                "MongoDB {}",
                info.get_str("version").unwrap_or("(unknown version)")
            );
            let database = match cfg.database.trim() {
                "" => DEFAULT_DATABASE.to_owned(),
                d => d.to_owned(),
            };
            debug!(%version, "mongodb connected");
            Ok(Box::new(MongoSession {
                client,
                database,
                cancel: Cancel::default(),
                version,
                closed: false,
                _ca_file: ca_file,
            }) as Box<dyn DbSession>)
        })
    }
}

/// The cancel flag and its wake-up.
#[derive(Clone, Default)]
struct Cancel {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Cancel {
    fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    fn requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves once cancellation is requested.
    async fn wait(&self) {
        loop {
            let n = self.notify.notified();
            if self.requested() {
                return;
            }
            n.await;
        }
    }

    /// Run `fut` unless cancelled first. Dropping a cursor's future kills it on the server.
    async fn run<T>(&self, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {
            r = fut => r,
            () = self.wait() => Err(DbError::Cancelled),
        }
    }
}

/// One MongoDB connection (a pooled client) with a current database.
pub struct MongoSession {
    client: Client,
    database: String,
    cancel: Cancel,
    version: String,
    closed: bool,
    /// The staged CA the client reads on every (re)connect; removed when the session drops.
    _ca_file: Option<tempfile::NamedTempFile>,
}

/// A client for `options` and the server's `buildInfo` (the first round trip, which
/// signs in).
async fn open(options: ClientOptions) -> mongodb::error::Result<(Client, Document)> {
    let client = Client::with_options(options)?;
    let info = client
        .database("admin")
        .run_command(doc! { "buildInfo": 1 })
        .await?;
    Ok((client, info))
}

fn is_auth_failure(e: &mongodb::error::Error) -> bool {
    matches!(
        e.kind.as_ref(),
        mongodb::error::ErrorKind::Authentication { .. }
    )
}

/// The text of a refused sign-in. The server only says "Authentication failed", for a
/// wrong password and for a user that lives in another database alike, so this names
/// the auth database used and, when the user signs in against the connection's own
/// database instead (a `mongodb://…/<db>` URI's default), says to use that one.
async fn auth_failure(
    e: &mongodb::error::Error,
    options: &ClientOptions,
    cfg: &DbConfig,
) -> String {
    let source = options
        .credential
        .as_ref()
        .and_then(|c| c.source.clone())
        .unwrap_or_else(|| "admin".into());
    let database = cfg.database.trim();
    let mut text = format!("{} (auth database {source})", message(e));
    if !database.is_empty() && database != source {
        let mut probe = options.clone();
        if let Some(c) = probe.credential.as_mut() {
            c.source = Some(database.to_owned());
        }
        if open(probe).await.is_ok() {
            text.push_str(&format!(
                ". The user signs in with auth database {database}: set Auth database to {database}"
            ));
            return text;
        }
    }
    text.push_str(
        ". Check the password, and that Auth database is the database the user was \
         created in (where db.createUser ran)",
    );
    text
}

/// The driver's error text without its `Kind: ` wrapping.
fn message(e: &mongodb::error::Error) -> String {
    use mongodb::error::ErrorKind;
    match e.kind.as_ref() {
        ErrorKind::Command(c) => c.message.clone(),
        ErrorKind::Authentication { message, .. } => format!("authentication failed: {message}"),
        ErrorKind::ServerSelection { message, .. } => {
            format!(
                "no server answered: {}",
                message.lines().next().unwrap_or(message)
            )
        }
        _ => e.to_string(),
    }
}

fn server_error(e: &mongodb::error::Error) -> DbError {
    use mongodb::error::ErrorKind;
    match e.kind.as_ref() {
        ErrorKind::Command(c) => DbError::Server(Box::new(ServerError {
            severity: "ERROR".into(),
            code: Some(if c.code_name.is_empty() {
                c.code.to_string()
            } else {
                c.code_name.clone()
            }),
            message: c.message.clone(),
            detail: None,
            hint: None,
            position: None,
        })),
        ErrorKind::Authentication { .. } | ErrorKind::ServerSelection { .. } => {
            DbError::Connect(message(e))
        }
        _ => DbError::Protocol(message(e)),
    }
}

fn parse_error(e: shell::ParseError, sql: &str) -> DbError {
    let mut b = e.offset.min(sql.len());
    while !sql.is_char_boundary(b) {
        b -= 1;
    }
    DbError::Server(Box::new(ServerError {
        severity: "ERROR".into(),
        code: Some("SyntaxError".into()),
        message: e.message,
        detail: None,
        hint: None,
        position: Some(ErrorPosition::Offset(sql[..b].chars().count() as u32 + 1)),
    }))
}

fn notice(message: impl Into<String>) -> ResultEvent {
    ResultEvent::Notice(Notice {
        severity: "INFO".into(),
        code: None,
        message: message.into(),
    })
}

fn done(started: Instant, affected: Option<u64>) -> ResultEvent {
    ResultEvent::Done(Completion {
        affected,
        elapsed: started.elapsed(),
    })
}

/// Columns and rows for a finite list of documents.
fn table(docs: &[Document], keep_document: bool, out: &mut Vec<ResultEvent>) {
    let layout = Layout::infer(docs);
    let mut columns: Vec<ColumnMeta> = layout.columns().to_vec();
    let batch = layout.batch(docs);
    if keep_document {
        out.push(ResultEvent::Columns(Arc::from(columns)));
        out.push(ResultEvent::Rows(batch));
        return;
    }
    // Drop the trailing document column from metadata and rows alike.
    columns.pop();
    let mut b = crate::batch::RowBatchBuilder::for_columns(&columns, batch.len());
    for r in 0..batch.len() {
        for (c, meta) in columns.iter().enumerate() {
            b.push_value(&batch.cell(r, c).to_value(meta.data_type));
        }
    }
    out.push(ResultEvent::Columns(Arc::from(columns)));
    out.push(ResultEvent::Rows(b.finish()));
}

/// The reply without driver bookkeeping (`ok`, cluster time).
fn clean(mut reply: Document) -> Document {
    for k in ["ok", "$clusterTime", "operationTime", "lastCommittedOpTime"] {
        reply.remove(k);
    }
    reply
}

fn count_of(reply: &Document, key: &str) -> u64 {
    match reply.get(key) {
        Some(Bson::Int32(n)) => u64::try_from(*n).unwrap_or(0),
        Some(Bson::Int64(n)) => u64::try_from(*n).unwrap_or(0),
        Some(Bson::Double(f)) => *f as u64,
        _ => 0,
    }
}

/// Fill in the current database where a statement left it out (`renameCollection`).
fn qualify(command: &mut Document, current: &str) {
    for key in ["renameCollection", "to"] {
        if let Some(Bson::String(s)) = command.get_mut(key)
            && s.starts_with('.')
        {
            *s = format!("{current}{s}");
        }
    }
}

/// Up to `max` documents from `cursor`; fewer means it is exhausted.
async fn chunk(
    cursor: &mut Cursor<Document>,
    cancel: &Cancel,
    max: usize,
) -> Result<Vec<Document>> {
    let mut docs = Vec::new();
    while docs.len() < max {
        let next = cancel.run(async { Ok(cursor.next().await) }).await?;
        match next {
            Some(Ok(d)) => docs.push(d),
            Some(Err(e)) => return Err(server_error(&e)),
            None => break,
        }
    }
    Ok(docs)
}

/// Streaming state for a cursor result.
struct CursorRows {
    cursor: Option<Cursor<Document>>,
    layout: Layout,
    cancel: Cancel,
    queued: VecDeque<ResultEvent>,
    started: Instant,
    finished: bool,
}

fn cursor_stream(state: CursorRows) -> ResultStream {
    Box::pin(stream::unfold(state, |mut s| async move {
        if let Some(e) = s.queued.pop_front() {
            return Some((Ok(e), s));
        }
        if s.finished {
            return None;
        }
        let Some(cursor) = s.cursor.as_mut() else {
            s.finished = true;
            return Some((Ok(done(s.started, None)), s));
        };
        match chunk(cursor, &s.cancel, DEFAULT_BATCH_ROWS).await {
            Ok(docs) => {
                if docs.len() < DEFAULT_BATCH_ROWS {
                    s.cursor = None;
                }
                if docs.is_empty() {
                    s.finished = true;
                    return Some((Ok(done(s.started, None)), s));
                }
                let batch = s.layout.batch(&docs);
                Some((Ok(ResultEvent::Rows(batch)), s))
            }
            Err(e) => {
                s.finished = true;
                s.cursor = None;
                Some((Err(e), s))
            }
        }
    }))
}

impl MongoSession {
    fn db_name(&self, db: Option<&str>) -> String {
        db.unwrap_or(&self.database).to_owned()
    }

    async fn command(&self, db: &str, command: Document) -> Result<Document> {
        let database = self.client.database(db);
        let fut = database.run_command(command);
        self.cancel
            .run(async { fut.await.map_err(|e| server_error(&e)) })
            .await
    }

    async fn run(&mut self, sql: &str, started: Instant) -> Result<ResultStream> {
        let op = shell::parse(sql).map_err(|e| parse_error(e, sql))?;
        let (db, mut command, shape, inserted) = match op {
            Op::Use(db) => {
                self.database = db.clone();
                let events = vec![
                    Ok(notice(format!("switched to db {db}"))),
                    Ok(done(started, None)),
                ];
                return Ok(Box::pin(stream::iter(events)));
            }
            Op::Command {
                db,
                command,
                shape,
                inserted,
            } => (self.db_name(db.as_deref()), command, shape, inserted),
        };
        qualify(&mut command, &self.database);
        debug!(database = %db, command = command.keys().next().map(String::as_str).unwrap_or(""), "mongodb command");
        let mut out: Vec<ResultEvent> = Vec::new();
        let mut affected = None;
        match shape {
            Shape::Cursor => {
                let database = self.client.database(&db);
                let fut = database.run_cursor_command(command);
                let mut cursor = self
                    .cancel
                    .run(async { fut.await.map_err(|e| server_error(&e)) })
                    .await?;
                let first = chunk(&mut cursor, &self.cancel, DEFAULT_BATCH_ROWS).await?;
                let layout = Layout::infer(&first);
                let mut queued = VecDeque::new();
                queued.push_back(ResultEvent::Columns(layout.columns()));
                if !first.is_empty() {
                    queued.push_back(ResultEvent::Rows(layout.batch(&first)));
                }
                let more = first.len() == DEFAULT_BATCH_ROWS;
                return Ok(cursor_stream(CursorRows {
                    cursor: more.then_some(cursor),
                    layout,
                    cancel: self.cancel.clone(),
                    queued,
                    started,
                    finished: false,
                }));
            }
            Shape::Reply => {
                let reply = clean(self.command(&db, command).await?);
                table(&[reply], true, &mut out);
            }
            Shape::Count => {
                let reply = self.command(&db, command).await?;
                let n = count_of(&reply, "n");
                out.push(ResultEvent::Columns(Arc::from(vec![ColumnMeta::new(
                    "count",
                    "long",
                    DataType::Int64,
                )])));
                let mut b = crate::batch::RowBatchBuilder::new(&[DataType::Int64], 1);
                b.push_i64(i64::try_from(n).unwrap_or(i64::MAX));
                out.push(ResultEvent::Rows(b.finish()));
            }
            Shape::Distinct => {
                let reply = self.command(&db, command).await?;
                let docs: Vec<Document> = reply
                    .get_array("values")
                    .map(|vs| vs.iter().map(|v| doc! { "value": v.clone() }).collect())
                    .unwrap_or_default();
                table(&docs, false, &mut out);
                out.push(notice(format!("{} distinct values", docs.len())));
            }
            Shape::Databases => {
                let reply = self.command("admin", command).await?;
                let docs: Vec<Document> = reply
                    .get_array("databases")
                    .map(|ds| ds.iter().filter_map(|d| d.as_document().cloned()).collect())
                    .unwrap_or_default();
                table(&docs, false, &mut out);
            }
            Shape::Write => {
                let verb = command.keys().next().cloned().unwrap_or_default();
                let reply = self.command(&db, command).await?;
                let n = count_of(&reply, "n");
                if let Ok(errors) = reply.get_array("writeErrors")
                    && let Some(first) = errors.first().and_then(Bson::as_document)
                {
                    return Err(DbError::Server(Box::new(ServerError {
                        severity: "ERROR".into(),
                        code: first.get_i32("code").ok().map(|c| c.to_string()),
                        message: first.get_str("errmsg").unwrap_or("write failed").to_owned(),
                        detail: Some(format!(
                            "{} of the documents were written before the error",
                            n
                        )),
                        hint: None,
                        position: None,
                    })));
                }
                let summary = match verb.as_str() {
                    "insert" => format!("inserted {n}"),
                    "update" => {
                        let upserted = reply.get_array("upserted").map(Vec::len).unwrap_or(0);
                        format!(
                            "matched {n}, modified {}, upserted {upserted}",
                            count_of(&reply, "nModified")
                        )
                    }
                    "delete" => format!("deleted {n}"),
                    _ => format!("{n} affected"),
                };
                // Matched documents for updates, as PostgreSQL counts them: an
                // update that leaves a document as it was still found its document.
                affected = Some(n);
                if !inserted.is_empty() {
                    let docs: Vec<Document> = inserted
                        .into_iter()
                        .map(|id| doc! { "insertedId": id })
                        .collect();
                    table(&docs, false, &mut out);
                }
                out.push(notice(summary));
            }
        }
        out.push(done(started, affected));
        Ok(Box::pin(stream::iter(out.into_iter().map(Ok))))
    }
}

impl DbSession for MongoSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        Box::pin(async move {
            if !params.is_empty() {
                return Err(DbError::Param(
                    "MongoDB statements take values inline, not as parameters".into(),
                ));
            }
            self.cancel.reset();
            let started = Instant::now();
            self.run(sql, started).await
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        let cancel = self.cancel.clone();
        CancelHandle::new(self.cancel.flag.clone(), move || {
            cancel.flag.store(true, Ordering::SeqCst);
            cancel.notify.notify_waiters();
            Box::pin(async { Ok(()) })
        })
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        Box::pin(async move { catalog::introspect(self, scope).await })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            Err(DbError::Unsupported(
                "MongoDB statements commit on their own here; multi-document transactions are not offered yet"
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
    fn seed_lists() {
        let s = seeds("a.example, b.example:27018", 27017).expect("seeds");
        assert_eq!(
            s,
            vec![
                ServerAddress::Tcp {
                    host: "a.example".into(),
                    port: Some(27017)
                },
                ServerAddress::Tcp {
                    host: "b.example".into(),
                    port: Some(27018)
                }
            ]
        );
        assert!(seeds(" ", 27017).is_err());
        assert!(seeds("h:x", 27017).is_err());
    }

    #[test]
    fn parse_errors_carry_a_position() {
        let sql = "db.users.find({ a: })";
        let e = shell::parse(sql).expect_err("bad");
        let e = parse_error(e, sql);
        assert_eq!(
            e.as_server().and_then(|s| s.position),
            Some(ErrorPosition::Offset(20))
        );
    }

    #[test]
    fn rename_takes_the_current_database() {
        let mut c = doc! { "renameCollection": ".a", "to": ".b" };
        qualify(&mut c, "shop");
        assert_eq!(c, doc! { "renameCollection": "shop.a", "to": "shop.b" });
    }

    #[tokio::test]
    async fn options_for_a_tunnel_and_credentials() {
        let mut cfg = DbConfig::new(Engine::MongoDb, "db.internal", "shop");
        cfg.user = "app".into();
        cfg.password = Some("pw".to_owned().into());
        cfg.options.insert("auth_source".into(), "shop".into());
        let via = TunnelEndpoint {
            host: "127.0.0.1".into(),
            port: 40000,
        };
        let o = client_options(&cfg, Some(&via)).await.expect("options").0;
        assert_eq!(o.direct_connection, Some(true));
        assert_eq!(
            o.hosts,
            vec![ServerAddress::Tcp {
                host: "127.0.0.1".into(),
                port: Some(40000)
            }]
        );
        let c = o.credential.expect("credential");
        assert_eq!(c.source.as_deref(), Some("shop"));
        assert!(matches!(o.tls, Some(Tls::Disabled)));
        cfg.ssl_mode = SslMode::Require;
        let o = client_options(&cfg, None).await.expect("options").0;
        assert!(matches!(o.tls, Some(Tls::Enabled(_))));
    }
}
