//! Capturing a plan from an open session.
//!
//! Actual plans execute the statement (`EXPLAIN ANALYZE`, `SET STATISTICS XML ON`), so they
//! always run inside a transaction, or a savepoint when one is already open, that is rolled
//! back: a DELETE leaves its rows in place. MongoDB actual plans (`executionStats`) only
//! run statements that read. Which engines offer which plans is
//! [`Dialect::plans`](switchyard_db::Dialect::plans).

use futures::StreamExt;
use switchyard_db::batch::DOCUMENT_COLUMN;
use switchyard_db::dialect::dialect_for;
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
    let support = dialect_for(engine).plans();
    let offered = engine.is_sql()
        && match mode {
            Mode::Estimated => support.estimated,
            Mode::Actual => support.actual,
        };
    if !offered {
        return Err(PlanError::Unsupported(match (mode, support.estimated) {
            (Mode::Actual, true) => format!(
                "{} has no actual plans; use Explain for the estimated plan",
                engine.display_name()
            ),
            _ => format!(
                "query plans are not available for {} yet",
                engine.display_name()
            ),
        }));
    }
    match engine {
        Engine::Postgres => postgres(session, sql, mode).await,
        Engine::SqlServer => sql_server(session, sql, mode).await,
        Engine::MySql => mysql(session, sql, mode).await,
        Engine::Sqlite => sqlite(session, sql).await,
        Engine::MongoDb => mongo(session, sql, mode).await,
        Engine::D1 | Engine::DurableObject | Engine::Redis | Engine::Snowflake | Engine::Oracle => {
            Err(PlanError::Unsupported(format!(
                "query plans are not available for {} yet",
                engine.display_name()
            )))
        }
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

/// MySQL and MariaDB return their plan in one `EXPLAIN` / `ANALYZE` column.
fn first(cells: Vec<String>, what: &str) -> Result<String> {
    cells
        .into_iter()
        .next()
        .ok_or_else(|| PlanError::Parse(format!("{what} returned no rows")))
}

async fn mysql(session: &mut dyn DbSession, sql: &str, mode: Mode) -> Result<Plan> {
    if mode == Mode::Estimated {
        let json = collect(session, &format!("EXPLAIN FORMAT=JSON {sql}"), any_column).await?;
        return crate::mysql::parse_json(&first(json, "EXPLAIN")?, sql);
    }
    // MariaDB has no EXPLAIN ANALYZE; its ANALYZE FORMAT=JSON runs and measures.
    let version = collect(session, "SELECT VERSION()", any_column).await?;
    let mariadb = version.iter().any(|v| v.contains("MariaDB"));
    let explain = if mariadb {
        format!("ANALYZE FORMAT=JSON {sql}")
    } else {
        format!("EXPLAIN ANALYZE {sql}")
    };
    let sp = (
        format!("SAVEPOINT {SAVEPOINT}"),
        format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
    );
    let out = collect_rolled_back(session, (&sp.0, &sp.1), &explain, any_column).await?;
    let text = first(out, "EXPLAIN ANALYZE")?;
    if mariadb {
        crate::mysql::parse_json(&text, sql)
    } else {
        crate::mysql::parse_tree(&text, sql)
    }
}

async fn sqlite(session: &mut dyn DbSession, sql: &str) -> Result<Plan> {
    let explain = format!("EXPLAIN QUERY PLAN {sql}");
    let mut stream = session.execute(&explain, &[]).await?;
    let mut cols: Option<(usize, usize, usize)> = None;
    let mut rows = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(c) if cols.is_none() => {
                let at = |name: &str| c.iter().position(|m| m.name.eq_ignore_ascii_case(name));
                cols = Some((
                    at("id").unwrap_or(0),
                    at("parent").unwrap_or(1),
                    at("detail").unwrap_or(c.len().saturating_sub(1)),
                ));
            }
            ResultEvent::Rows(batch) => {
                let Some((id, parent, detail)) = cols else {
                    continue;
                };
                for r in 0..batch.len() {
                    let int = |c: usize| batch.cell(r, c).to_display().trim().parse().unwrap_or(0);
                    rows.push(crate::sqlite::Row {
                        id: int(id),
                        parent: int(parent),
                        detail: batch.cell(r, detail).to_display(),
                    });
                }
            }
            _ => {}
        }
    }
    crate::sqlite::parse(&rows, sql)
}

fn is_document(column: &str) -> bool {
    column == DOCUMENT_COLUMN
}

async fn mongo(session: &mut dyn DbSession, sql: &str, mode: Mode) -> Result<Plan> {
    use switchyard_db::mongo::shell::{self, Effect, Op};
    let op = shell::parse(sql).map_err(|e| PlanError::Unsupported(e.message))?;
    let Op::Command { command, .. } = &op else {
        return Err(PlanError::Unsupported(
            "explain works on a find() or aggregate() statement".into(),
        ));
    };
    match command.keys().next().map(String::as_str) {
        Some("find" | "aggregate") => {}
        Some("explain") => {
            return Err(PlanError::Unsupported(
                "remove .explain(): Explain and Analyze add it themselves".into(),
            ));
        }
        _ => {
            return Err(PlanError::Unsupported(
                "explain works on a find() or aggregate() statement".into(),
            ));
        }
    }
    let verbosity = match mode {
        Mode::Estimated => "queryPlanner",
        Mode::Actual => {
            // executionStats runs the pipeline; one that writes ($out, $merge) is refused.
            if op.effect() != Effect::Read {
                return Err(PlanError::Unsupported(
                    "an actual plan would run this pipeline's write ($out / $merge); use Explain"
                        .into(),
                ));
            }
            "executionStats"
        }
    };
    let explain = format!("{sql}.explain(\"{verbosity}\")");
    let docs = collect(session, &explain, is_document).await?;
    crate::mongo::parse(&first(docs, "explain")?, sql)
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
