//! An in-memory driver for tests. It generates deterministic rows, honors cancellation and
//! can simulate slow statements, so core and UI code can be exercised without a server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream;

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::catalog::{CatalogChunk, IntrospectScope, ObjectInfo, ObjectKind, SchemaInfo};
use crate::dialect::{Dialect, postgres::PostgresDialect};
use crate::driver::{CancelHandle, ComponentId, DbConfig, DbSession, Driver, TunnelEndpoint};
use crate::error::{DbError, Result};
use crate::stream::{Completion, Notice, ResultEvent, ResultStream};
use crate::value::{DataType, Engine, Value};

/// Driver that serves generated data.
#[derive(Clone, Debug, Default)]
pub struct MockDriver;

impl Driver for MockDriver {
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
        _via: Option<TunnelEndpoint>,
    ) -> BoxFuture<'a, Result<Box<dyn DbSession>>> {
        Box::pin(async move {
            if cfg.host == "unreachable" {
                return Err(DbError::Connect("no route to host".into()));
            }
            Ok(Box::new(MockSession::default()) as Box<dyn DbSession>)
        })
    }
}

/// A mock session. Recognized statements:
///
/// - `rows N` — streams N rows of (id int8, name text, amount numeric, note text NULL every 7th)
/// - `sleep MS` — waits MS milliseconds (cancellable), returns no rows
/// - `fail` — returns a server error
/// - anything else — one row with the statement echoed
#[derive(Debug, Default)]
pub struct MockSession {
    cancel: Arc<AtomicBool>,
    in_txn: bool,
}

fn columns() -> Arc<[ColumnMeta]> {
    Arc::from(vec![
        ColumnMeta::new("id", "int8", DataType::Int64),
        ColumnMeta::new("name", "text", DataType::Text),
        ColumnMeta::new("amount", "numeric", DataType::Numeric),
        ColumnMeta::new("note", "text", DataType::Text),
    ])
}

impl DbSession for MockSession {
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        _params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>> {
        self.cancel.store(false, Ordering::SeqCst);
        let cancel = self.cancel.clone();
        let sql = sql.trim().to_owned();
        Box::pin(async move {
            let started = Instant::now();
            if sql == "fail" {
                return Err(DbError::Server(Box::new(crate::error::ServerError {
                    severity: "ERROR".into(),
                    code: Some("42601".into()),
                    message: "syntax error".into(),
                    detail: None,
                    hint: None,
                    position: Some(crate::error::ErrorPosition::Offset(1)),
                })));
            }
            if let Some(ms) = sql
                .strip_prefix("sleep ")
                .and_then(|s| s.parse::<u64>().ok())
            {
                let deadline = Instant::now() + Duration::from_millis(ms);
                while Instant::now() < deadline {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(DbError::Cancelled);
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let events = vec![Ok(ResultEvent::Done(Completion {
                    affected: None,
                    elapsed: started.elapsed(),
                }))];
                return Ok(Box::pin(stream::iter(events)) as ResultStream);
            }
            let total: usize = sql
                .strip_prefix("rows ")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let cols = columns();
            let batch_rows = 1000;
            let s = stream::unfold((0usize, false, false), move |(next, sent_cols, done)| {
                let cols = cols.clone();
                let cancel = cancel.clone();
                async move {
                    if done {
                        return None;
                    }
                    if !sent_cols {
                        let ev = ResultEvent::Columns(cols);
                        return Some((Ok(ev), (next, true, false)));
                    }
                    if cancel.load(Ordering::SeqCst) {
                        return Some((Err(DbError::Cancelled), (next, true, true)));
                    }
                    if next >= total {
                        let ev = ResultEvent::Done(Completion {
                            affected: None,
                            elapsed: started.elapsed(),
                        });
                        return Some((Ok(ev), (next, true, true)));
                    }
                    let n = batch_rows.min(total - next);
                    let mut b = RowBatchBuilder::for_columns(&cols, n);
                    for i in next..next + n {
                        b.push_i64(i as i64 + 1);
                        b.push_str(&format!("row {}", i + 1));
                        b.push_str(&format!("{}.{:02}", i * 3, i % 100));
                        if i % 7 == 0 {
                            b.push_null()
                        } else {
                            b.push_str("")
                        }
                    }
                    tokio::task::yield_now().await;
                    Some((Ok(ResultEvent::Rows(b.finish())), (next + n, true, false)))
                }
            });
            let notice = stream::iter(vec![Ok(ResultEvent::Notice(Notice {
                severity: "NOTICE".into(),
                code: None,
                message: "mock session".into(),
            }))]);
            Ok(Box::pin(futures::StreamExt::chain(s, notice)) as ResultStream)
        })
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::flag_only(self.cancel.clone())
    }

    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>> {
        Box::pin(async move {
            Ok(match scope {
                IntrospectScope::Databases => CatalogChunk::Databases(vec!["mock".into()]),
                IntrospectScope::Schemas => CatalogChunk::Schemas(vec![SchemaInfo {
                    name: "public".into(),
                    is_system: false,
                }]),
                IntrospectScope::Objects { schema, kind } => CatalogChunk::Objects(
                    (1..=3)
                        .map(|i| ObjectInfo {
                            schema: schema.clone(),
                            name: format!("{}_{i}", kind.folder_label().to_lowercase()),
                            kind,
                            estimated_rows: Some(i * 100),
                            detail: None,
                        })
                        .collect(),
                ),
                IntrospectScope::Search { pattern, limit, .. } => CatalogChunk::Objects(
                    (1..=3)
                        .map(|i| ObjectInfo {
                            schema: "public".into(),
                            name: format!("tables_{i}"),
                            kind: ObjectKind::Table,
                            estimated_rows: None,
                            detail: None,
                        })
                        .filter(|o| o.name.contains(&pattern.to_lowercase()))
                        .take(limit as usize)
                        .collect(),
                ),
                IntrospectScope::Detail { .. } | IntrospectScope::AllColumns => {
                    return Err(DbError::Unsupported("mock detail".into()));
                }
            })
        })
    }

    fn begin(&mut self) -> BoxFuture<'_, Result<()>> {
        self.in_txn = true;
        Box::pin(async { Ok(()) })
    }

    fn commit(&mut self) -> BoxFuture<'_, Result<()>> {
        self.in_txn = false;
        Box::pin(async { Ok(()) })
    }

    fn rollback(&mut self) -> BoxFuture<'_, Result<()>> {
        self.in_txn = false;
        Box::pin(async { Ok(()) })
    }

    fn in_transaction(&self) -> bool {
        self.in_txn
    }

    fn server_version(&self) -> String {
        "Mock 1.0".into()
    }

    fn is_closed(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn streams_batches() {
        let driver = MockDriver;
        let cfg = DbConfig::new(Engine::Postgres, "localhost", "mock");
        let mut s = driver.connect(&cfg, None).await.unwrap();
        let mut stream = s.execute("rows 2500", &[]).await.unwrap();
        let mut rows = 0;
        let mut batches = 0;
        let mut done = false;
        while let Some(ev) = stream.next().await {
            match ev.unwrap() {
                ResultEvent::Rows(b) => {
                    rows += b.len();
                    batches += 1;
                }
                ResultEvent::Done(_) => done = true,
                _ => {}
            }
        }
        assert_eq!((rows, batches, done), (2500, 3, true));
    }

    #[tokio::test]
    async fn cancel_stops_sleep() {
        let driver = MockDriver;
        let cfg = DbConfig::new(Engine::Postgres, "localhost", "mock");
        let mut s = driver.connect(&cfg, None).await.unwrap();
        let handle = s.cancel_handle();
        let started = Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            handle.cancel().await.unwrap();
        });
        let r = s.execute("sleep 5000", &[]).await;
        assert!(matches!(r, Err(DbError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
