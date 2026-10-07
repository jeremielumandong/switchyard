//! Access analysis (workload view): how tables and indexes are used, the statements that
//! cost the most, and the indexes SQL Server reports as missing.
//!
//! * PostgreSQL: `pg_stat_user_tables`, `pg_stat_user_indexes`, `pg_stat_statements`.
//! * SQL Server: `sys.dm_db_index_usage_stats`, the missing-index DMVs, Query Store.
//!
//! Every source is optional. A missing extension or permission becomes a [`Hint`] with the
//! statement that fixes it, never an error. Probes carry the [`MARKER`] comment so they are
//! left out of the statement list, and on PostgreSQL inside an open transaction they run
//! under a savepoint so a failing probe cannot abort the user's transaction.

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use switchyard_db::driver::DbSession;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::{Engine, Value};

use crate::model::MissingIndex;
use crate::{PlanError, Result};

/// Comment that marks Switchyard's own probe statements.
pub const MARKER: &str = "/* swy:access */";

const SAVEPOINT: &str = "swy_access";

/// Rows kept per list.
const LIMIT: usize = 100;

/// Where a piece of the workload comes from (for hints).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Table scan counters.
    Tables,
    /// Index usage counters.
    Indexes,
    /// `pg_stat_statements` or Query Store.
    Statements,
    /// SQL Server's missing-index DMVs.
    MissingIndexes,
    /// HypoPG (hypothetical indexes).
    HypoPg,
}

/// Something that is unavailable, why, and how to fix it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hint {
    /// What it affects.
    pub source: Source,
    /// What is missing.
    pub message: String,
    /// Statement(s) an administrator can run to fix it.
    pub fix: Option<String>,
}

/// How one table is read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TableUsage {
    /// Schema.
    pub schema: String,
    /// Table.
    pub name: String,
    /// Sequential (full) scans; SQL Server: scans of the heap or clustered index.
    pub seq_scans: Option<f64>,
    /// Rows read by sequential scans (PostgreSQL).
    pub seq_rows_read: Option<f64>,
    /// Index scans (SQL Server: seeks and scans of other indexes).
    pub index_scans: Option<f64>,
    /// Writes (SQL Server: user updates; PostgreSQL: inserted + updated + deleted rows).
    pub writes: Option<f64>,
    /// Live rows.
    pub rows: Option<f64>,
    /// Dead rows waiting for vacuum (PostgreSQL).
    pub dead_rows: Option<f64>,
    /// Size on disk including indexes, bytes.
    pub bytes: Option<f64>,
    /// Last (auto)analyze, Unix milliseconds (PostgreSQL).
    pub analyzed_ms: Option<i64>,
}

impl TableUsage {
    /// Share of scans that were sequential (0–1), when both counters are known.
    pub fn seq_share(&self) -> Option<f64> {
        let (s, i) = (self.seq_scans?, self.index_scans.unwrap_or(0.0));
        (s + i > 0.0).then(|| s / (s + i))
    }

    /// Mostly read by full scans while big enough for that to matter.
    pub fn mostly_sequential(&self) -> bool {
        self.seq_share().is_some_and(|s| s >= 0.5)
            && self.seq_scans.is_some_and(|s| s >= 10.0)
            && self.rows.is_some_and(|r| r >= 10_000.0)
    }
}

/// How one index is used.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexUsage {
    /// Schema.
    pub schema: String,
    /// Table.
    pub table: String,
    /// Index.
    pub name: String,
    /// Times it was used to read (PostgreSQL scans; SQL Server seeks + scans + lookups).
    pub scans: Option<f64>,
    /// Times it had to be maintained by writes (SQL Server).
    pub writes: Option<f64>,
    /// Size, bytes.
    pub bytes: Option<f64>,
    /// Unique index.
    pub unique: bool,
    /// Primary key.
    pub primary: bool,
    /// Definition (PostgreSQL `CREATE INDEX …`, SQL Server index type).
    pub definition: Option<String>,
}

impl IndexUsage {
    /// Never used since the counters were reset, and not enforcing a key.
    pub fn unused(&self) -> bool {
        self.scans == Some(0.0) && !self.unique && !self.primary
    }
}

/// One normalized statement and what it cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatementStat {
    /// `queryid` (PostgreSQL) or Query Store `query_id`.
    pub id: String,
    /// Normalized text (parameters as `$1` / `@P1`).
    pub query: String,
    /// Executions.
    pub calls: f64,
    /// Total execution time, ms.
    pub total_ms: f64,
    /// Mean execution time, ms.
    pub mean_ms: f64,
    /// Rows returned or affected, all executions.
    pub rows: Option<f64>,
    /// Pages read (shared buffers hit + read; SQL Server logical reads), all executions.
    pub pages: Option<f64>,
}

/// The workload of one database.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workload {
    /// Engine.
    pub engine: Engine,
    /// When the counters start (stats reset or server start), Unix milliseconds.
    pub since_ms: Option<i64>,
    /// Tables, most rows read by full scans first.
    pub tables: Vec<TableUsage>,
    /// Indexes, least used and largest first.
    pub indexes: Vec<IndexUsage>,
    /// Statements, most total time first.
    pub statements: Vec<StatementStat>,
    /// Indexes the engine reports as missing (SQL Server), most valuable first.
    pub missing_indexes: Vec<MissingIndex>,
    /// What could not be read and how to fix it.
    pub hints: Vec<Hint>,
    /// HypoPG version when installed in this database (PostgreSQL).
    pub hypopg: Option<String>,
}

impl Workload {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            since_ms: None,
            tables: Vec::new(),
            indexes: Vec::new(),
            statements: Vec::new(),
            missing_indexes: Vec::new(),
            hints: Vec::new(),
            hypopg: None,
        }
    }

    fn hint(&mut self, source: Source, message: impl Into<String>, fix: Option<String>) {
        self.hints.push(Hint {
            source,
            message: message.into(),
            fix,
        });
    }
}

/// Read the workload of the session's database.
pub async fn workload(session: &mut dyn DbSession, engine: Engine) -> Result<Workload> {
    match engine {
        Engine::Postgres => postgres(session).await,
        Engine::SqlServer => sql_server(session).await,
        Engine::D1 => Err(PlanError::Unsupported(
            "workload statistics are not available for Cloudflare D1".into(),
        )),
        Engine::Snowflake => Err(PlanError::Unsupported(
            "workload statistics are not available for Snowflake yet".into(),
        )),
        Engine::Oracle => Err(PlanError::Unsupported(
            "workload statistics are not available for Oracle yet".into(),
        )),
    }
}

// ---- reading small result sets ---------------------------------------------------------

/// A small result set as text cells (probe results are bounded by [`LIMIT`]).
#[derive(Debug, Default)]
pub(crate) struct Rows {
    cols: Vec<String>,
    data: Vec<Vec<Option<String>>>,
}

impl Rows {
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    fn col(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.eq_ignore_ascii_case(name))
    }

    pub(crate) fn text(&self, row: usize, name: &str) -> Option<&str> {
        let c = self.col(name)?;
        self.data.get(row)?.get(c)?.as_deref()
    }

    pub(crate) fn num(&self, row: usize, name: &str) -> Option<f64> {
        self.text(row, name)?.trim().parse().ok()
    }

    pub(crate) fn flag(&self, row: usize, name: &str) -> bool {
        matches!(
            self.text(row, name).map(str::trim),
            Some("true" | "t" | "1" | "True" | "TRUE")
        )
    }
}

/// Run `sql` and keep the last result set that had columns.
pub(crate) async fn rows(session: &mut dyn DbSession, sql: &str, params: &[Value]) -> Result<Rows> {
    let mut stream = session.execute(sql, params).await?;
    let mut out = Rows::default();
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(cols) => {
                out = Rows {
                    cols: cols.iter().map(|c| c.name.clone()).collect(),
                    data: Vec::new(),
                };
            }
            ResultEvent::Rows(batch) => {
                for r in 0..batch.len() {
                    out.data.push(
                        (0..out.cols.len())
                            .map(|c| {
                                let cell = batch.cell(r, c);
                                (!cell.is_null()).then(|| cell.to_display())
                            })
                            .collect(),
                    );
                }
            }
            ResultEvent::NextResultSet | ResultEvent::Notice(_) | ResultEvent::Done(_) => {}
        }
    }
    Ok(out)
}

/// Run a probe; on PostgreSQL inside an open transaction, under a savepoint so a failure
/// leaves the user's transaction usable.
pub(crate) async fn probe(
    session: &mut dyn DbSession,
    engine: Engine,
    sql: &str,
    params: &[Value],
) -> Result<Rows> {
    let guarded = engine == Engine::Postgres && session.in_transaction();
    if guarded {
        rows(session, &format!("SAVEPOINT {SAVEPOINT}"), &[]).await?;
    }
    let result = rows(session, sql, params).await;
    if guarded {
        let undo = if result.is_ok() {
            format!("RELEASE SAVEPOINT {SAVEPOINT}")
        } else {
            format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}")
        };
        rows(session, &undo, &[]).await?;
    }
    result
}

/// Whether the error is the server refusing for lack of permission.
fn permission_denied(e: &PlanError) -> bool {
    let PlanError::Db(db) = e else { return false };
    let Some(s) = db.as_server() else {
        return false;
    };
    // PostgreSQL 42501 insufficient_privilege; SQL Server 297/300 (server state),
    // 229 (object permission), 262 (statement permission).
    matches!(
        s.code.as_deref(),
        Some("42501" | "297" | "300" | "229" | "262")
    ) || s.message.to_ascii_lowercase().contains("permission")
}

fn ms(v: Option<f64>) -> Option<i64> {
    v.map(|v| v.round() as i64)
}

/// `"name"` with embedded quotes doubled (PostgreSQL identifier).
fn pg_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `[name]` with `]` doubled (SQL Server identifier).
fn ms_ident(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}

// ---- PostgreSQL --------------------------------------------------------------------------

async fn postgres(s: &mut dyn DbSession) -> Result<Workload> {
    let e = Engine::Postgres;
    let mut w = Workload::new(e);
    let me = probe(
        s,
        e,
        &format!(
            "SELECT {MARKER} current_user AS who,
                    (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) AS su,
                    pg_has_role(current_user, 'pg_read_all_stats', 'USAGE') AS read_all,
                    (SELECT setting FROM pg_settings WHERE name = 'shared_preload_libraries') AS preload,
                    (SELECT extversion FROM pg_extension WHERE extname = 'pg_stat_statements') AS pgss,
                    (SELECT extversion FROM pg_extension WHERE extname = 'hypopg') AS hypopg,
                    EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'hypopg') AS hypopg_available,
                    (SELECT round(extract(epoch FROM stats_reset) * 1000) FROM pg_stat_database
                      WHERE datname = current_database()) AS since_ms"
        ),
        &[],
    )
    .await?;
    let who = me.text(0, "who").unwrap_or("").to_owned();
    let privileged = me.flag(0, "su") || me.flag(0, "read_all");
    // Hidden (NULL) for roles without pg_read_all_settings.
    let preload_known = me.text(0, "preload").is_some();
    let preload = me.text(0, "preload").unwrap_or("").to_owned();
    let pgss = me.text(0, "pgss").map(str::to_owned);
    w.hypopg = me.text(0, "hypopg").map(str::to_owned);
    w.since_ms = ms(me.num(0, "since_ms"));

    // Tables: everyone can read the counters.
    let t = probe(
        s,
        e,
        &format!(
            "SELECT {MARKER} s.schemaname, s.relname, s.seq_scan, s.seq_tup_read, s.idx_scan,
                    s.n_tup_ins + s.n_tup_upd + s.n_tup_del AS writes,
                    -- The counter starts at 0 after a stats reset; the planner's estimate stays.
                    CASE WHEN s.n_live_tup > 0 THEN s.n_live_tup
                         WHEN c.reltuples >= 0 THEN c.reltuples::bigint END AS n_live_tup,
                    s.n_dead_tup, pg_total_relation_size(s.relid) AS bytes,
                    round(extract(epoch FROM greatest(s.last_analyze, s.last_autoanalyze)) * 1000) AS analyzed_ms
               FROM pg_stat_user_tables s JOIN pg_class c ON c.oid = s.relid
              ORDER BY s.seq_tup_read DESC NULLS LAST, s.relname
              LIMIT {LIMIT}"
        ),
        &[],
    )
    .await;
    match t {
        Ok(t) => {
            for r in 0..t.len() {
                w.tables.push(TableUsage {
                    schema: t.text(r, "schemaname").unwrap_or("").to_owned(),
                    name: t.text(r, "relname").unwrap_or("").to_owned(),
                    seq_scans: t.num(r, "seq_scan"),
                    seq_rows_read: t.num(r, "seq_tup_read"),
                    index_scans: t.num(r, "idx_scan"),
                    writes: t.num(r, "writes"),
                    rows: t.num(r, "n_live_tup"),
                    dead_rows: t.num(r, "n_dead_tup"),
                    bytes: t.num(r, "bytes"),
                    analyzed_ms: ms(t.num(r, "analyzed_ms")),
                });
            }
        }
        Err(err) => w.hint(Source::Tables, format!("Table statistics: {err}"), None),
    }

    let i = probe(
        s,
        e,
        &format!(
            "SELECT {MARKER} s.schemaname, s.relname, s.indexrelname, s.idx_scan,
                    pg_relation_size(s.indexrelid) AS bytes, i.indisunique, i.indisprimary,
                    pg_get_indexdef(s.indexrelid) AS def
               FROM pg_stat_user_indexes s JOIN pg_index i ON i.indexrelid = s.indexrelid
              ORDER BY s.idx_scan ASC NULLS FIRST, pg_relation_size(s.indexrelid) DESC
              LIMIT {LIMIT}"
        ),
        &[],
    )
    .await;
    match i {
        Ok(i) => {
            for r in 0..i.len() {
                w.indexes.push(IndexUsage {
                    schema: i.text(r, "schemaname").unwrap_or("").to_owned(),
                    table: i.text(r, "relname").unwrap_or("").to_owned(),
                    name: i.text(r, "indexrelname").unwrap_or("").to_owned(),
                    scans: i.num(r, "idx_scan"),
                    writes: None,
                    bytes: i.num(r, "bytes"),
                    unique: i.flag(r, "indisunique"),
                    primary: i.flag(r, "indisprimary"),
                    definition: i.text(r, "def").map(str::to_owned),
                });
            }
        }
        Err(err) => w.hint(Source::Indexes, format!("Index statistics: {err}"), None),
    }

    // Statements.
    // Unknown preload list: assume loaded and let the query tell.
    let loaded = !preload_known
        || preload
            .split(',')
            .any(|l| l.trim().trim_matches('"') == "pg_stat_statements");
    let preload_fix = || {
        if !preload_known {
            // Without the current list, ALTER SYSTEM would overwrite other libraries.
            return "-- add pg_stat_statements to shared_preload_libraries (postgresql.conf), \
                    then restart the server"
                .to_owned();
        }
        let list = if preload.trim().is_empty() {
            "pg_stat_statements".to_owned()
        } else {
            format!("{}, pg_stat_statements", preload.trim())
        };
        format!(
            "ALTER SYSTEM SET shared_preload_libraries = '{}';\n-- then restart the server",
            list.replace('\'', "''")
        )
    };
    match (&pgss, loaded) {
        (None, true) => w.hint(
            Source::Statements,
            "pg_stat_statements is loaded but not installed in this database.",
            Some("CREATE EXTENSION pg_stat_statements;".into()),
        ),
        (None, false) => w.hint(
            Source::Statements,
            "pg_stat_statements is not set up: it must be preloaded by the server and \
             installed in this database.",
            Some(format!(
                "{}\nCREATE EXTENSION pg_stat_statements;",
                preload_fix()
            )),
        ),
        (Some(_), false) => w.hint(
            Source::Statements,
            "pg_stat_statements is installed but not loaded by the server.",
            Some(preload_fix()),
        ),
        (Some(version), true) => {
            // 1.8 (PostgreSQL 13) renamed total_time to total_exec_time.
            let new_names = version
                .split('.')
                .map(|p| p.parse::<u32>().unwrap_or(0))
                .collect::<Vec<_>>()
                >= vec![1, 8];
            let (total, mean) = if new_names {
                ("total_exec_time", "mean_exec_time")
            } else {
                ("total_time", "mean_time")
            };
            let sql = format!(
                "SELECT {MARKER} queryid::text AS id, query, calls, {total} AS total_ms,
                        {mean} AS mean_ms, rows, shared_blks_hit + shared_blks_read AS pages
                   FROM pg_stat_statements
                  WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
                    AND query NOT LIKE '%swy:access%'
                    AND query NOT LIKE '%SAVEPOINT {SAVEPOINT}%'
                    AND query <> '<insufficient privilege>'
                  ORDER BY {total} DESC
                  LIMIT {LIMIT}"
            );
            match probe(s, e, &sql, &[]).await {
                Ok(q) => {
                    for r in 0..q.len() {
                        w.statements.push(StatementStat {
                            id: q.text(r, "id").unwrap_or("").to_owned(),
                            query: q.text(r, "query").unwrap_or("").to_owned(),
                            calls: q.num(r, "calls").unwrap_or(0.0),
                            total_ms: q.num(r, "total_ms").unwrap_or(0.0),
                            mean_ms: q.num(r, "mean_ms").unwrap_or(0.0),
                            rows: q.num(r, "rows"),
                            pages: q.num(r, "pages"),
                        });
                    }
                    if !privileged {
                        w.hint(
                            Source::Statements,
                            "Only your own statements are listed: other roles' statements \
                             need the pg_read_all_stats role.",
                            Some(format!("GRANT pg_read_all_stats TO {};", pg_ident(&who))),
                        );
                    }
                }
                Err(err) if permission_denied(&err) => w.hint(
                    Source::Statements,
                    "Reading pg_stat_statements was refused.",
                    Some(format!("GRANT pg_read_all_stats TO {};", pg_ident(&who))),
                ),
                Err(err) if err.to_string().contains("shared_preload_libraries") => w.hint(
                    Source::Statements,
                    "pg_stat_statements is installed but not loaded by the server.",
                    Some(preload_fix()),
                ),
                Err(err) => w.hint(
                    Source::Statements,
                    format!("pg_stat_statements: {err}"),
                    None,
                ),
            }
        }
    }

    if w.hypopg.is_none() {
        let available = me.flag(0, "hypopg_available");
        w.hint(
            Source::HypoPg,
            if available {
                "HypoPG is available but not installed in this database: hypothetical \
                 indexes are off."
            } else {
                "HypoPG is not installed on the server: hypothetical indexes are off. \
                 Install the hypopg package for this PostgreSQL version first."
            },
            Some("CREATE EXTENSION hypopg;".into()),
        );
    }
    Ok(w)
}

// ---- SQL Server --------------------------------------------------------------------------

/// SQL Server lists columns as `[a], [b]`.
fn bracket_list(s: Option<&str>) -> Vec<String> {
    s.map(|s| {
        s.split(',')
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

async fn sql_server(s: &mut dyn DbSession) -> Result<Workload> {
    let e = Engine::SqlServer;
    let mut w = Workload::new(e);
    let me = probe(
        s,
        e,
        &format!(
            "SELECT {MARKER} SUSER_SNAME() AS login, USER_NAME() AS db_user,
                    CAST(SERVERPROPERTY('EngineEdition') AS int) AS edition,
                    ISNULL(HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE'), 0) AS server_state,
                    ISNULL(HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER PERFORMANCE STATE'), 0) AS perf_state,
                    ISNULL(HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DATABASE STATE'), 0) AS db_state"
        ),
        &[],
    )
    .await?;
    let login = me.text(0, "login").unwrap_or("").to_owned();
    let db_user = me.text(0, "db_user").unwrap_or("").to_owned();
    // Azure SQL Database (edition 5) scopes the DMVs to the database.
    let azure_db = me.num(0, "edition") == Some(5.0);
    let usage = if azure_db {
        me.flag(0, "db_state")
    } else {
        me.flag(0, "server_state") || me.flag(0, "perf_state")
    };
    let db_state = me.flag(0, "db_state") || usage;
    let usage_fix = if azure_db {
        format!("GRANT VIEW DATABASE STATE TO {};", ms_ident(&db_user))
    } else {
        format!(
            "-- in master; on SQL Server 2022 VIEW SERVER PERFORMANCE STATE also works\n\
             GRANT VIEW SERVER STATE TO {};",
            ms_ident(&login)
        )
    };
    if !usage {
        w.hint(
            Source::Indexes,
            "Index usage and missing-index statistics need VIEW SERVER STATE; sizes and row \
             counts are shown without them.",
            Some(usage_fix.clone()),
        );
    } else if !azure_db
        && let Ok(r) = probe(
            s,
            e,
            &format!(
                "SELECT {MARKER} DATEDIFF_BIG(millisecond, '19700101',
                        DATEADD(minute, -DATEPART(TZOFFSET, SYSDATETIMEOFFSET()), sqlserver_start_time)) AS since_ms
                   FROM sys.dm_os_sys_info"
            ),
            &[],
        )
        .await
    {
        w.since_ms = ms(r.num(0, "since_ms"));
    }

    // Per-index usage, joined only when it can be read.
    let usage_join = if usage {
        "LEFT JOIN sys.dm_db_index_usage_stats u
                ON u.object_id = i.object_id AND u.index_id = i.index_id AND u.database_id = DB_ID()"
    } else {
        "OUTER APPLY (SELECT CAST(NULL AS bigint) AS user_seeks, CAST(NULL AS bigint) AS user_scans,
                             CAST(NULL AS bigint) AS user_lookups, CAST(NULL AS bigint) AS user_updates) u"
    };
    let t = probe(
        s,
        e,
        &format!(
            "SELECT TOP ({LIMIT}) {MARKER} sc.name AS schemaname, t.name AS relname,
                    SUM(CASE WHEN i.index_id IN (0, 1) THEN u.user_scans END) AS seq_scan,
                    SUM(u.user_seeks + CASE WHEN i.index_id > 1 THEN u.user_scans ELSE 0 END) AS idx_scan,
                    MAX(CASE WHEN i.index_id IN (0, 1) THEN u.user_updates END) AS writes,
                    (SELECT SUM(p.rows) FROM sys.partitions p
                      WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)) AS n_live_tup,
                    (SELECT SUM(a.total_pages) * 8192 FROM sys.partitions p
                       JOIN sys.allocation_units a ON a.container_id = p.partition_id
                      WHERE p.object_id = t.object_id) AS bytes
               FROM sys.tables t
               JOIN sys.schemas sc ON sc.schema_id = t.schema_id
               JOIN sys.indexes i ON i.object_id = t.object_id
               {usage_join}
              WHERE t.is_ms_shipped = 0
              GROUP BY sc.name, t.name, t.object_id
              ORDER BY seq_scan DESC, n_live_tup DESC"
        ),
        &[],
    )
    .await;
    match t {
        Ok(t) => {
            for r in 0..t.len() {
                w.tables.push(TableUsage {
                    schema: t.text(r, "schemaname").unwrap_or("").to_owned(),
                    name: t.text(r, "relname").unwrap_or("").to_owned(),
                    seq_scans: t.num(r, "seq_scan").or(usage.then_some(0.0)),
                    index_scans: t.num(r, "idx_scan").or(usage.then_some(0.0)),
                    writes: t.num(r, "writes"),
                    rows: t.num(r, "n_live_tup"),
                    bytes: t.num(r, "bytes"),
                    ..Default::default()
                });
            }
        }
        Err(err) => w.hint(Source::Tables, format!("Table statistics: {err}"), None),
    }

    let i = probe(
        s,
        e,
        &format!(
            "SELECT TOP ({LIMIT}) {MARKER} sc.name AS schemaname, t.name AS relname, i.name AS indexname,
                    u.user_seeks + u.user_scans + u.user_lookups AS scans, u.user_updates AS writes,
                    (SELECT SUM(a.used_pages) * 8192 FROM sys.partitions p
                       JOIN sys.allocation_units a ON a.container_id = p.partition_id
                      WHERE p.object_id = i.object_id AND p.index_id = i.index_id) AS bytes,
                    i.is_unique, i.is_primary_key, i.type_desc
               FROM sys.indexes i
               JOIN sys.tables t ON t.object_id = i.object_id
               JOIN sys.schemas sc ON sc.schema_id = t.schema_id
               {usage_join}
              WHERE i.index_id > 0 AND i.is_hypothetical = 0 AND t.is_ms_shipped = 0
              ORDER BY ISNULL(u.user_seeks + u.user_scans + u.user_lookups, 0), bytes DESC"
        ),
        &[],
    )
    .await;
    match i {
        Ok(i) => {
            for r in 0..i.len() {
                w.indexes.push(IndexUsage {
                    schema: i.text(r, "schemaname").unwrap_or("").to_owned(),
                    table: i.text(r, "relname").unwrap_or("").to_owned(),
                    name: i.text(r, "indexname").unwrap_or("").to_owned(),
                    // Never touched since the server started: no row in the DMV.
                    scans: i.num(r, "scans").or(usage.then_some(0.0)),
                    writes: i.num(r, "writes").or(usage.then_some(0.0)),
                    bytes: i.num(r, "bytes"),
                    unique: i.flag(r, "is_unique"),
                    primary: i.flag(r, "is_primary_key"),
                    definition: i.text(r, "type_desc").map(str::to_owned),
                });
            }
        }
        Err(err) => w.hint(Source::Indexes, format!("Index statistics: {err}"), None),
    }

    if usage {
        let m = probe(
            s,
            e,
            &format!(
                "SELECT TOP (50) {MARKER}
                        QUOTENAME(OBJECT_SCHEMA_NAME(d.object_id, d.database_id)) + '.' +
                        QUOTENAME(OBJECT_NAME(d.object_id, d.database_id)) AS tbl,
                        d.equality_columns, d.inequality_columns, d.included_columns,
                        gs.avg_user_impact AS impact
                   FROM sys.dm_db_missing_index_details d
                   JOIN sys.dm_db_missing_index_groups g ON g.index_handle = d.index_handle
                   JOIN sys.dm_db_missing_index_group_stats gs ON gs.group_handle = g.index_group_handle
                  WHERE d.database_id = DB_ID()
                  ORDER BY gs.avg_total_user_cost * gs.avg_user_impact * (gs.user_seeks + gs.user_scans) DESC"
            ),
            &[],
        )
        .await;
        match m {
            Ok(m) => {
                for r in 0..m.len() {
                    w.missing_indexes.push(MissingIndex {
                        impact: m.num(r, "impact"),
                        table: m.text(r, "tbl").unwrap_or("").to_owned(),
                        equality: bracket_list(m.text(r, "equality_columns")),
                        inequality: bracket_list(m.text(r, "inequality_columns")),
                        include: bracket_list(m.text(r, "included_columns")),
                    });
                }
            }
            Err(err) => w.hint(
                Source::MissingIndexes,
                format!("Missing-index statistics: {err}"),
                permission_denied(&err).then(|| usage_fix.clone()),
            ),
        }
    } else {
        w.hint(
            Source::MissingIndexes,
            "Missing-index suggestions need VIEW SERVER STATE.",
            Some(usage_fix.clone()),
        );
    }

    // Query Store.
    let qs_fix = format!("GRANT VIEW DATABASE STATE TO {};", ms_ident(&db_user));
    if !db_state {
        w.hint(
            Source::Statements,
            "Query Store needs VIEW DATABASE STATE.",
            Some(qs_fix),
        );
        return Ok(w);
    }
    let state = probe(
        s,
        e,
        &format!("SELECT {MARKER} actual_state_desc FROM sys.database_query_store_options"),
        &[],
    )
    .await;
    let state = match state {
        Ok(r) => r.text(0, "actual_state_desc").map(str::to_owned),
        Err(err) if permission_denied(&err) => {
            w.hint(
                Source::Statements,
                "Reading Query Store was refused.",
                Some(qs_fix),
            );
            return Ok(w);
        }
        Err(err) => {
            w.hint(
                Source::Statements,
                format!("Query Store is not available (SQL Server 2016 or later): {err}"),
                None,
            );
            return Ok(w);
        }
    };
    if !matches!(state.as_deref(), Some("READ_WRITE" | "READ_ONLY")) {
        w.hint(
            Source::Statements,
            format!(
                "Query Store is {} for this database, so there are no statement statistics.",
                state.as_deref().unwrap_or("OFF").to_lowercase()
            ),
            Some("ALTER DATABASE CURRENT SET QUERY_STORE = ON;".into()),
        );
        return Ok(w);
    }
    // avg_duration is in microseconds.
    let q = probe(
        s,
        e,
        &format!(
            "SELECT TOP ({LIMIT}) {MARKER} CAST(q.query_id AS varchar(20)) AS id,
                    qt.query_sql_text AS query,
                    SUM(rs.count_executions) AS calls,
                    SUM(rs.avg_duration * rs.count_executions) / 1000.0 AS total_ms,
                    SUM(rs.avg_duration * rs.count_executions) / NULLIF(SUM(rs.count_executions), 0) / 1000.0 AS mean_ms,
                    SUM(rs.avg_rowcount * rs.count_executions) AS rows,
                    SUM(rs.avg_logical_io_reads * rs.count_executions) AS pages
               FROM sys.query_store_query q
               JOIN sys.query_store_query_text qt ON qt.query_text_id = q.query_text_id
               JOIN sys.query_store_plan p ON p.query_id = q.query_id
               JOIN sys.query_store_runtime_stats rs ON rs.plan_id = p.plan_id
              WHERE qt.query_sql_text NOT LIKE '%swy:access%'
              GROUP BY q.query_id, qt.query_sql_text
              ORDER BY total_ms DESC"
        ),
        &[],
    )
    .await;
    match q {
        Ok(q) => {
            for r in 0..q.len() {
                w.statements.push(StatementStat {
                    id: q.text(r, "id").unwrap_or("").to_owned(),
                    query: q.text(r, "query").unwrap_or("").to_owned(),
                    calls: q.num(r, "calls").unwrap_or(0.0),
                    total_ms: q.num(r, "total_ms").unwrap_or(0.0),
                    mean_ms: q.num(r, "mean_ms").unwrap_or(0.0),
                    rows: q.num(r, "rows"),
                    pages: q.num(r, "pages"),
                });
            }
        }
        Err(err) => w.hint(Source::Statements, format!("Query Store: {err}"), None),
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_helpers() {
        let t = TableUsage {
            seq_scans: Some(40.0),
            index_scans: Some(10.0),
            rows: Some(1e6),
            ..Default::default()
        };
        assert_eq!(t.seq_share(), Some(0.8));
        assert!(t.mostly_sequential());
        let small = TableUsage {
            rows: Some(50.0),
            ..t.clone()
        };
        assert!(!small.mostly_sequential());
        let ix = IndexUsage {
            scans: Some(0.0),
            ..Default::default()
        };
        assert!(ix.unused());
        assert!(
            !IndexUsage {
                primary: true,
                ..ix.clone()
            }
            .unused()
        );
        assert!(
            !IndexUsage { scans: None, ..ix }.unused(),
            "unknown is not unused"
        );
        assert_eq!(pg_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(ms_ident("dom\\us]er"), "[dom\\us]]er]");
        assert_eq!(
            bracket_list(Some("[a], [b]")),
            vec!["[a]".to_owned(), "[b]".to_owned()]
        );
        assert!(bracket_list(None).is_empty());
    }
}
