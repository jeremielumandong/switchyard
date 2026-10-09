//! Agent queries: the read-only path the MCP server uses. Safety is enforced here, in the
//! shared core, whatever the calling tool or coding CLI allows:
//!
//! * only a single SELECT / WITH query (parsed with `sqlparser`);
//! * PostgreSQL and Oracle run it in a read-only transaction, SQL Server in a transaction,
//!   and every transaction is rolled back;
//! * a row cap and a timeout, after which the statement is cancelled on the server;
//! * every call is written to query history with its tags, even when the connection has
//!   history turned off.

use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use switchyard_db::{DbError, Engine, ResultEvent, Value, dialect_for, guard};
use switchyard_store::{HistoryEntry, HistoryStatus, now_ms};
use tracing::warn;

use super::{Service, SessionInner};
use crate::bus::{Event, QueryId, SessionId};

/// Rows returned to an agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRows {
    /// Column names.
    pub columns: Vec<String>,
    /// Column types as the engine names them.
    pub types: Vec<String>,
    /// Rows, cells as JSON (numbers stay numbers; exact decimals and everything else as text).
    pub rows: Vec<Vec<serde_json::Value>>,
    /// More rows existed than the cap allowed.
    pub truncated: bool,
    /// Time taken, ms.
    pub elapsed_ms: u64,
}

/// A cell as JSON.
pub fn cell_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or_else(
            || serde_json::Value::String(f.to_string()),
            serde_json::Value::Number,
        ),
        other => serde_json::Value::String(other.to_display()),
    }
}

impl Service {
    /// Run `sql` for an agent under the guards above; answer with [`Event::AgentRows`].
    pub(super) async fn agent_query(
        &self,
        session: SessionId,
        query: QueryId,
        sql: String,
        row_cap: usize,
        timeout: Duration,
        tags: Vec<String>,
    ) {
        let reply = |result: Result<AgentRows, String>| Event::AgentRows { query, result };
        let Some(slot) = self.slot(session) else {
            return self.emit(reply(Err(DbError::Closed.to_string())));
        };
        let conn = slot.connection.clone();
        let started_at = now_ms();
        let started = Instant::now();
        let result = if let Some(why) = refusal(conn.engine, &sql) {
            Err(why)
        } else {
            let mut inner = slot.inner.lock().await;
            if inner.session.in_transaction() {
                Err("the session has an open transaction".to_owned())
            } else {
                let cancel = inner.session.cancel_handle();
                let run = tokio::time::timeout(
                    timeout,
                    read_only(&mut inner, conn.engine, &sql, row_cap.max(1)),
                )
                .await;
                let r = match run {
                    Ok(r) => r,
                    Err(_) => {
                        let _ = cancel.cancel().await;
                        Err(format!(
                            "the query was stopped after {} s",
                            timeout.as_secs_f64()
                        ))
                    }
                };
                // Whatever happened, nothing stays open.
                if inner.session.in_transaction()
                    && let Err(e) = inner.session.rollback().await
                {
                    warn!(error = %e, "agent rollback failed");
                }
                r
            }
        };
        let elapsed = started.elapsed();
        let result = result.map(|mut rows| {
            rows.elapsed_ms = elapsed.as_millis() as u64;
            rows
        });
        let (status, error, rows) = match &result {
            Ok(r) => (HistoryStatus::Ok, None, r.rows.len() as i64),
            Err(e) => (HistoryStatus::Error, Some(e.clone()), 0),
        };
        self.agent_history(
            &conn,
            sql,
            started_at,
            elapsed,
            Some(rows),
            status,
            error,
            tags,
        )
        .await;
        self.emit(reply(result));
    }

    /// Record one agent call in history (always, whatever the connection's setting).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn agent_history(
        &self,
        conn: &switchyard_store::DbConnection,
        sql: String,
        started_at: i64,
        elapsed: Duration,
        rows: Option<i64>,
        status: HistoryStatus,
        error: Option<String>,
        mut tags: Vec<String>,
    ) {
        if !tags.iter().any(|t| t == "agent") {
            tags.insert(0, "agent".into());
        }
        let entry = HistoryEntry {
            id: 0,
            connection_id: Some(conn.id.clone()),
            connection_name: conn.name.clone(),
            sql,
            started_at,
            duration_ms: elapsed.as_millis() as i64,
            rows,
            affected: None,
            status,
            error,
            tags,
            has_plan: false,
        };
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "agent history write failed");
        }
    }

    /// [`crate::Command::RecordAgentCall`].
    pub(super) async fn record_agent_call(
        &self,
        session: SessionId,
        summary: String,
        error: Option<String>,
        tags: Vec<String>,
    ) {
        let Some(slot) = self.slot(session) else {
            return;
        };
        let status = if error.is_some() {
            HistoryStatus::Error
        } else {
            HistoryStatus::Ok
        };
        self.agent_history(
            &slot.connection,
            summary,
            now_ms(),
            Duration::ZERO,
            None,
            status,
            error,
            tags,
        )
        .await;
    }
}

/// Why `sql` is not an agent query on `engine`, if it is not: one SELECT / WITH on SQL
/// engines, one statement that only reads on MongoDB (no transaction holds it back there).
fn refusal(engine: Engine, sql: &str) -> Option<String> {
    match engine {
        Engine::MongoDb => {
            use switchyard_db::mongo::shell::{Effect, Op, parse};
            match parse(sql) {
                Ok(Op::Use(_)) => Some(
                    "`use` is not allowed; name the database with db.getSiblingDB(\"…\")".into(),
                ),
                Ok(op) if op.effect() == Effect::Read => None,
                Ok(_) => Some(
                    "only a single statement that reads (find, aggregate without \
                               $out/$merge, countDocuments, distinct, …) is allowed"
                        .into(),
                ),
                Err(e) => Some(format!("could not parse the statement: {}", e.message)),
            }
        }
        Engine::Redis => Some("Redis connections take redis_command, not queries".into()),
        e if guard::is_single_select(dialect_for(e), sql) => None,
        _ => Some("only a single SELECT or WITH query is allowed".into()),
    }
}

/// Open the read-only wrapper, run `sql`, keep up to `cap` rows.
async fn read_only(
    inner: &mut SessionInner,
    engine: Engine,
    sql: &str,
    cap: usize,
) -> Result<AgentRows, String> {
    let s = inner.session.as_mut();
    match engine {
        Engine::Postgres | Engine::Oracle => {
            s.begin().await.map_err(|e| e.to_string())?;
            drain(s.execute("SET TRANSACTION READ ONLY", &[]).await)
                .await
                .map_err(|e| e.to_string())?;
        }
        // Rolled back below whatever the statement did.
        Engine::SqlServer | Engine::Sqlite => s.begin().await.map_err(|e| e.to_string())?,
        // MySQL fixes a transaction's access mode when it starts.
        Engine::MySql => drain(s.execute("START TRANSACTION READ ONLY", &[]).await)
            .await
            .map_err(|e| e.to_string())?,
        // No transactions across requests: the SELECT-only check is the guard.
        Engine::D1 | Engine::Snowflake | Engine::MongoDb => {}
        // Never a SQL session; agent tools are SQL only.
        Engine::Redis => return Err("Redis connections have no SQL tools".into()),
    }
    let mut stream = s.execute(sql, &[]).await.map_err(|e| e.to_string())?;
    let mut out = AgentRows {
        columns: Vec::new(),
        types: Vec::new(),
        rows: Vec::new(),
        truncated: false,
        elapsed_ms: 0,
    };
    let mut meta = None;
    while let Some(ev) = stream.next().await {
        match ev.map_err(|e| e.to_string())? {
            ResultEvent::Columns(cols) if meta.is_none() => {
                out.columns = cols.iter().map(|c| c.name.clone()).collect();
                out.types = cols.iter().map(|c| c.type_name.clone()).collect();
                meta = Some(cols);
            }
            ResultEvent::Rows(batch) => {
                let Some(cols) = &meta else { continue };
                for r in 0..batch.len() {
                    if out.rows.len() >= cap {
                        out.truncated = true;
                        break;
                    }
                    out.rows.push(
                        (0..cols.len())
                            .map(|c| cell_json(&batch.cell(r, c).to_value(cols[c].data_type)))
                            .collect(),
                    );
                }
                if out.truncated {
                    break;
                }
            }
            // A single SELECT has one result set; anything after it is ignored.
            ResultEvent::NextResultSet => break,
            _ => {}
        }
    }
    Ok(out)
}

async fn drain(stream: Result<switchyard_db::ResultStream, DbError>) -> Result<(), DbError> {
    let mut stream = stream?;
    while let Some(ev) = stream.next().await {
        ev?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mongodb_agents_only_read() {
        let r = |s: &str| refusal(Engine::MongoDb, s);
        assert_eq!(r("db.orders.find({ status: \"A\" }).limit(5)"), None);
        assert_eq!(
            r("db.orders.aggregate([{ $group: { _id: \"$s\" } }])"),
            None
        );
        assert_eq!(r("show collections"), None);
        assert!(r("db.orders.aggregate([{ $out: \"x\" }])").is_some());
        assert!(r("db.orders.deleteMany({})").is_some());
        assert!(r("db.orders.insertOne({ a: 1 })").is_some());
        assert!(r("db.orders.drop()").is_some());
        assert!(r("use shop").is_some());
        assert_eq!(r_sql("select 1"), None);
        assert!(r_sql("delete from t").is_some());
    }

    fn r_sql(s: &str) -> Option<String> {
        refusal(Engine::Postgres, s)
    }

    #[test]
    fn cells_keep_numbers_and_text() {
        assert_eq!(cell_json(&Value::Int(3)), serde_json::json!(3));
        assert_eq!(
            cell_json(&Value::Numeric("1.10".into())),
            serde_json::json!("1.10")
        );
        assert_eq!(cell_json(&Value::Null), serde_json::Value::Null);
        assert_eq!(cell_json(&Value::Float(f64::NAN)), serde_json::json!("NaN"));
    }
}
