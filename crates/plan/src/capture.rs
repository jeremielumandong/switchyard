//! Capturing a plan from an open session.
//!
//! Actual plans execute the statement (`EXPLAIN ANALYZE`, `SET STATISTICS XML ON`), so they
//! always run inside a transaction, or a savepoint when one is already open, that is rolled
//! back: a DELETE leaves its rows in place.

use futures::StreamExt;
use switchyard_db::driver::DbSession;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::Engine;

use crate::model::Plan;
use crate::{PlanError, Result};

/// Name of the savepoint used inside an already open transaction.
const SAVEPOINT: &str = "swy_explain";

/// Estimated plan (nothing runs) or actual plan (runs, then rolls back).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Optimizer estimate only.
    Estimated,
    /// Execute and measure, then roll back.
    Actual,
}

/// Capture the plan for one statement `sql` on `session`.
pub async fn capture(
    session: &mut dyn DbSession,
    engine: Engine,
    sql: &str,
    mode: Mode,
) -> Result<Plan> {
    let sql = statement(sql)?;
    match engine {
        Engine::Postgres => postgres(session, sql, mode).await,
        Engine::SqlServer => sql_server(session, sql, mode).await,
        Engine::D1 => Err(PlanError::Unsupported(
            "query plans are not available for Cloudflare D1".into(),
        )),
        Engine::MongoDb => Err(PlanError::Unsupported(
            "visual plans are not available for MongoDB yet; add .explain() to a find or aggregate"
                .into(),
        )),
        Engine::Sqlite => Err(PlanError::Unsupported(
            "query plans are not available for SQLite yet".into(),
        )),
        Engine::Snowflake => Err(PlanError::Unsupported(
            "query plans are not available for Snowflake yet".into(),
        )),
        Engine::Oracle => Err(PlanError::Unsupported(
            "query plans are not available for Oracle yet".into(),
        )),
        Engine::MySql => Err(PlanError::Unsupported(
            "query plans are not available for MySQL yet".into(),
        )),
    }
}

/// The statement without surrounding whitespace and trailing semicolons.
fn statement(sql: &str) -> Result<&str> {
    let s = sql.trim().trim_end_matches(';').trim_end();
    if s.is_empty() {
        return Err(PlanError::Unsupported("nothing to explain".into()));
    }
    Ok(s)
}

/// Every non-null cell of the columns `want` accepts, across all result sets, as text.
async fn collect(
    session: &mut dyn DbSession,
    sql: &str,
    want: fn(&str) -> bool,
) -> Result<Vec<String>> {
    let mut stream = session.execute(sql, &[]).await?;
    let mut keep: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(cols) => {
                keep = cols
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| want(&c.name))
                    .map(|(i, _)| i)
                    .collect();
            }
            ResultEvent::Rows(batch) => {
                for row in 0..batch.len() {
                    for &c in &keep {
                        let cell = batch.cell(row, c);
                        if !cell.is_null() {
                            out.push(cell.to_display());
                        }
                    }
                }
            }
            ResultEvent::NextResultSet => keep.clear(),
            ResultEvent::Notice(_) | ResultEvent::Done(_) => {}
        }
    }
    Ok(out)
}

async fn run(session: &mut dyn DbSession, sql: &str) -> Result<()> {
    fn none(_: &str) -> bool {
        false
    }
    collect(session, sql, none).await.map(|_| ())
}

/// Run `sql` inside a transaction (or savepoint) that is always rolled back, collecting
/// the cells of the columns `want` accepts.
async fn collect_rolled_back(
    session: &mut dyn DbSession,
    savepoint: (&str, &str),
    sql: &str,
    want: fn(&str) -> bool,
) -> Result<Vec<String>> {
    let nested = session.in_transaction();
    if nested {
        run(session, savepoint.0).await?;
    } else {
        session.begin().await?;
    }
    let result = collect(session, sql, want).await;
    let undone = if nested {
        run(session, savepoint.1).await
    } else {
        session.rollback().await.map_err(PlanError::from)
    };
    match (result, undone) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), _) => {
            // The statement failed; the transaction is aborted on PostgreSQL either way.
            if !nested && session.in_transaction() {
                let _ = session.rollback().await;
            }
            Err(e)
        }
        (Ok(_), Err(e)) => Err(PlanError::Rollback(e.to_string())),
    }
}

fn any_column(_: &str) -> bool {
    true
}

async fn postgres(session: &mut dyn DbSession, sql: &str, mode: Mode) -> Result<Plan> {
    let json = match mode {
        Mode::Estimated => {
            let explain = format!("EXPLAIN (FORMAT JSON) {sql}");
            collect(session, &explain, any_column).await?
        }
        Mode::Actual => {
            let explain = format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}");
            let sp = (
                format!("SAVEPOINT {SAVEPOINT}"),
                format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
            );
            collect_rolled_back(session, (&sp.0, &sp.1), &explain, any_column).await?
        }
    };
    let text = json
        .first()
        .ok_or_else(|| PlanError::Parse("EXPLAIN returned no rows".into()))?;
    crate::pg::parse(text, sql)
}

/// SQL Server returns showplan XML in a column of this name.
fn is_showplan(column: &str) -> bool {
    column.contains("Showplan")
}

async fn sql_server(session: &mut dyn DbSession, sql: &str, mode: Mode) -> Result<Plan> {
    let docs = match mode {
        Mode::Estimated => {
            // `SET SHOWPLAN_XML ON` must be alone in its batch; while it is on nothing runs.
            run(session, "SET SHOWPLAN_XML ON").await?;
            let docs = collect(session, sql, is_showplan).await;
            let off = run(session, "SET SHOWPLAN_XML OFF").await;
            let docs = docs?;
            off?;
            docs
        }
        Mode::Actual => {
            let sp = (
                format!("SAVE TRANSACTION {SAVEPOINT}"),
                format!("ROLLBACK TRANSACTION {SAVEPOINT}"),
            );
            run(session, "SET STATISTICS XML ON").await?;
            // The statement's own results arrive too; only the showplan is kept.
            let docs = collect_rolled_back(session, (&sp.0, &sp.1), sql, is_showplan).await;
            let off = run(session, "SET STATISTICS XML OFF").await;
            let docs = docs?;
            off?;
            docs
        }
    };
    if docs.is_empty() {
        return Err(PlanError::Parse("SQL Server returned no showplan".into()));
    }
    crate::mssql::parse_many(&docs, sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_trimming() {
        assert_eq!(statement(" select 1 ;; \n").ok(), Some("select 1"));
        assert!(statement(" ; ").is_err());
    }
}
