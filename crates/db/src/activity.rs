//! Activity monitor (DBX-5b): the sessions and running queries of a server, and the
//! statements that cancel a query or end a session.
//!
//! * PostgreSQL: `pg_stat_activity`; `pg_cancel_backend` / `pg_terminate_backend`.
//! * SQL Server: `sys.dm_exec_sessions` + `sys.dm_exec_requests` + `sys.dm_exec_sql_text`;
//!   `KILL <spid>` (no per-request cancel from another session).
//! * Oracle: `V$SESSION` + `V$SQL`; `ALTER SYSTEM CANCEL SQL` (18c+) and
//!   `ALTER SYSTEM KILL SESSION … IMMEDIATE`.
//! * Snowflake: running queries from `INFORMATION_SCHEMA.QUERY_HISTORY`;
//!   `SYSTEM$CANCEL_QUERY`.
//! * Cloudflare D1: not supported.
//!
//! Missing privileges become an [`ActivityHint`], never an error. Session ids only reach
//! SQL as integers (or a validated Snowflake query id) inside a [`SessionTarget`], and
//! [`act`] refuses to touch the session it runs on.
//!
//! These functions are for the app's activity tab only: the MCP server and `swy` must
//! never expose them.

use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;

use crate::driver::DbSession;
use crate::error::DbError;
use crate::stream::ResultEvent;
use crate::value::Engine;

/// Rows listed at most.
pub const LIMIT: usize = 500;

/// Characters of SQL shown in the list (the full text is kept for the detail view).
pub const SQL_PREVIEW_CHARS: usize = 200;

/// Errors from the activity monitor.
#[derive(Debug, thiserror::Error)]
pub enum ActivityError {
    /// The engine (or server version) has no such action, or no activity views.
    #[error("{0}")]
    Unsupported(String),
    /// The target is the monitor's own session.
    #[error("refusing to cancel or end the activity monitor's own session")]
    OwnSession,
    /// A session or query id that is not a valid id.
    #[error("invalid session id: {0}")]
    InvalidId(String),
    /// The server refused or failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

/// Result alias for this module.
pub type Result<T, E = ActivityError> = std::result::Result<T, E>;

/// What to do to a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityAction {
    /// Cancel the running query; the session stays.
    CancelQuery,
    /// End the session (its open transaction rolls back).
    Terminate,
}

impl ActivityAction {
    /// Verb for buttons and history.
    pub fn label(self) -> &'static str {
        match self {
            ActivityAction::CancelQuery => "Cancel query",
            ActivityAction::Terminate => "Terminate session",
        }
    }
}

/// A validated session (or query) to act on. Built from the listing's own columns,
/// never from text typed or copied by the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTarget {
    /// PostgreSQL backend pid or SQL Server session id (spid).
    Backend {
        /// The pid / spid.
        id: i64,
    },
    /// Oracle `sid,serial#`.
    Oracle {
        /// `SID`.
        sid: i64,
        /// `SERIAL#`.
        serial: i64,
    },
    /// A running Snowflake query.
    SnowflakeQuery {
        /// Query id (a UUID).
        query_id: String,
        /// The session that runs it.
        session: i64,
    },
}

impl SessionTarget {
    /// The session part, compared with the monitor's own session.
    pub fn session_key(&self) -> i64 {
        match self {
            SessionTarget::Backend { id } => *id,
            SessionTarget::Oracle { sid, .. } => *sid,
            SessionTarget::SnowflakeQuery { session, .. } => *session,
        }
    }

    /// How the target reads in the UI and history (`1234`, `12,345`, a query id).
    pub fn label(&self) -> String {
        match self {
            SessionTarget::Backend { id } => id.to_string(),
            SessionTarget::Oracle { sid, serial } => format!("{sid},{serial}"),
            SessionTarget::SnowflakeQuery { query_id, .. } => query_id.clone(),
        }
    }
}

/// Something the monitor cannot see, and how to fix it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityHint {
    /// What is missing.
    pub message: String,
    /// Statement an administrator can run to fix it.
    pub fix: Option<String>,
}

/// One session (or, on Snowflake, one running query).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActivitySession {
    /// Id as shown (`pid`, `spid`, `sid,serial`, query id).
    pub id: String,
    /// What actions can target; `None` when the row's id did not validate.
    pub target: Option<SessionTarget>,
    /// Login / user.
    pub user: Option<String>,
    /// Database (or schema, Oracle).
    pub database: Option<String>,
    /// Client address or host.
    pub client: Option<String>,
    /// Program / application name.
    pub program: Option<String>,
    /// State (`active`, `idle`, `running`, `sleeping`, …).
    pub state: Option<String>,
    /// What it waits on.
    pub wait: Option<String>,
    /// When the current (or last) query started, Unix ms.
    pub started_ms: Option<i64>,
    /// How long the current query has run, ms (only while running).
    pub duration_ms: Option<i64>,
    /// Current (or last) SQL.
    pub sql: Option<String>,
    /// Sessions blocking this one.
    pub blocked_by: Option<String>,
    /// A query is running.
    pub running: bool,
    /// This is the monitor's own session.
    pub is_self: bool,
}

impl ActivitySession {
    /// The SQL on one line, cut to [`SQL_PREVIEW_CHARS`].
    pub fn sql_preview(&self) -> String {
        let Some(sql) = &self.sql else {
            return String::new();
        };
        let one: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.chars().count() <= SQL_PREVIEW_CHARS {
            one
        } else {
            let mut s: String = one.chars().take(SQL_PREVIEW_CHARS).collect();
            s.push('…');
            s
        }
    }

    /// The row as tab-separated text (per-row copy).
    pub fn to_tsv(&self) -> String {
        let o = |v: &Option<String>| v.clone().unwrap_or_default();
        let n = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
        [
            self.id.clone(),
            o(&self.user),
            o(&self.database),
            o(&self.client),
            o(&self.program),
            o(&self.state),
            o(&self.wait),
            n(self.started_ms),
            n(self.duration_ms),
            o(&self.blocked_by),
            self.sql
                .clone()
                .unwrap_or_default()
                .replace(['\t', '\n', '\r'], " "),
        ]
        .join("\t")
    }
}

/// The sessions of one server.
#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    /// Engine.
    pub engine: Engine,
    /// Sessions, running ones first.
    pub sessions: Vec<ActivitySession>,
    /// What could not be read and how to fix it.
    pub hints: Vec<ActivityHint>,
    /// Whether "Cancel query" is available.
    pub can_cancel: bool,
    /// Whether "Terminate session" is available.
    pub can_terminate: bool,
}

/// Whether the activity monitor works for `engine` at all.
pub fn supported(engine: Engine) -> bool {
    !matches!(engine, Engine::D1 | Engine::MongoDb)
}

/// Whether `action` exists on `engine` at `server_version`.
pub fn supports(engine: Engine, action: ActivityAction, server_version: &str) -> bool {
    match (engine, action) {
        (Engine::Postgres, _) => true,
        (Engine::SqlServer, ActivityAction::Terminate) => true,
        // SQL Server cannot cancel another session's request; only KILL.
        (Engine::SqlServer, ActivityAction::CancelQuery) => false,
        // `ALTER SYSTEM CANCEL SQL` arrived in 18c.
        (Engine::Oracle, ActivityAction::CancelQuery) => major_version(server_version) >= 18,
        (Engine::Oracle, ActivityAction::Terminate) => true,
        (Engine::Snowflake, ActivityAction::CancelQuery) => true,
        (Engine::Snowflake, ActivityAction::Terminate) => false,
        (Engine::D1 | Engine::MongoDb, _) => false,
    }
}

/// Leading number of a version string (`19.3.0.0.0` → 19, `Oracle 23ai` → 23).
fn major_version(v: &str) -> u32 {
    let digits: String = v
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(0)
}

// ---- SQL ---------------------------------------------------------------------------------

/// PostgreSQL: client backends from `pg_stat_activity`.
pub const PG_LIST: &str = "\
SELECT a.pid::text AS id,
       a.usename AS username,
       a.datname AS database_name,
       COALESCE(host(a.client_addr), CASE WHEN a.client_port = -1 THEN 'local socket' END) AS client,
       a.application_name AS program,
       a.state,
       CASE WHEN a.wait_event IS NOT NULL THEN a.wait_event_type || ': ' || a.wait_event END AS wait,
       (extract(epoch FROM (now() - COALESCE(a.query_start, a.backend_start))) * 1000)::bigint AS age_ms,
       (a.state IS NOT NULL AND a.state <> 'idle') AS running,
       a.query AS sql_text,
       NULLIF(array_to_string(pg_blocking_pids(a.pid), ', '), '') AS blocked_by,
       (a.pid = pg_backend_pid()) AS is_self
FROM pg_stat_activity a
WHERE a.backend_type = 'client backend'
ORDER BY running DESC, a.query_start NULLS LAST
LIMIT 500";

/// SQL Server: user sessions, their requests and SQL text (needs `VIEW SERVER STATE` to
/// see other sessions).
pub const MSSQL_LIST: &str = "\
SELECT CAST(s.session_id AS varchar(11)) AS id,
       s.login_name AS username,
       DB_NAME(COALESCE(r.database_id, s.database_id)) AS database_name,
       COALESCE(c.client_net_address, s.host_name) AS client,
       s.program_name AS program,
       COALESCE(r.status, s.status) AS state,
       r.wait_type + COALESCE(' (' + NULLIF(r.wait_resource, '') + ')', '') AS wait,
       DATEDIFF_BIG(MILLISECOND, COALESCE(r.start_time, s.last_request_start_time), GETDATE()) AS age_ms,
       CASE WHEN r.session_id IS NULL THEN 0 ELSE 1 END AS running,
       t.text AS sql_text,
       CAST(NULLIF(r.blocking_session_id, 0) AS varchar(11)) AS blocked_by,
       CASE WHEN s.session_id = @@SPID THEN 1 ELSE 0 END AS is_self
FROM sys.dm_exec_sessions s
LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
OUTER APPLY (SELECT TOP (1) ec.client_net_address
             FROM sys.dm_exec_connections ec
             WHERE ec.session_id = s.session_id) c
OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
WHERE s.is_user_process = 1
ORDER BY running DESC, age_ms DESC
OFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY";

/// SQL Server: whether the login can see other sessions.
pub const MSSQL_PERMISSION: &str = "SELECT HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE') AS allowed, SUSER_SNAME() AS login_name";

/// Oracle: user sessions and their current SQL (needs `SELECT` on `V_$SESSION` and
/// `V_$SQL`, e.g. `SELECT_CATALOG_ROLE`).
pub const ORACLE_LIST: &str = "\
SELECT s.sid || ',' || s.serial# AS id,
       s.username AS username,
       s.schemaname AS database_name,
       s.machine AS client,
       s.program AS program,
       s.status AS state,
       CASE WHEN s.wait_class <> 'Idle' THEN s.event END AS wait,
       s.last_call_et * 1000 AS age_ms,
       CASE WHEN s.status = 'ACTIVE' THEN 1 ELSE 0 END AS running,
       q.sql_text AS sql_text,
       TO_CHAR(s.blocking_session) AS blocked_by,
       CASE WHEN s.sid = TO_NUMBER(SYS_CONTEXT('USERENV', 'SID')) THEN 1 ELSE 0 END AS is_self
FROM v$session s
LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = s.sql_child_number
WHERE s.type = 'USER'
ORDER BY running DESC, s.last_call_et DESC
FETCH FIRST 500 ROWS ONLY";

/// Snowflake: queries still running (or queued) that the role can see.
pub const SNOWFLAKE_LIST: &str = "\
SELECT query_id AS id,
       TO_VARCHAR(session_id) AS session_id,
       user_name AS username,
       database_name,
       warehouse_name AS program,
       execution_status AS state,
       DATEDIFF('millisecond', start_time, CURRENT_TIMESTAMP()) AS age_ms,
       1 AS running,
       query_text AS sql_text,
       CASE WHEN TO_VARCHAR(session_id) = CURRENT_SESSION() THEN 1 ELSE 0 END AS is_self
FROM TABLE(INFORMATION_SCHEMA.QUERY_HISTORY(RESULT_LIMIT => 10000))
WHERE execution_status IN ('RUNNING', 'QUEUED', 'RESUMING_WAREHOUSE', 'BLOCKED')
ORDER BY start_time
LIMIT 500";

/// The listing statement for `engine`.
pub fn list_sql(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some(PG_LIST),
        Engine::SqlServer => Some(MSSQL_LIST),
        Engine::Oracle => Some(ORACLE_LIST),
        Engine::Snowflake => Some(SNOWFLAKE_LIST),
        Engine::D1 | Engine::MongoDb => None,
    }
}

/// The statement that reads the monitor's own session id.
pub fn own_session_sql(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some("SELECT pg_backend_pid()::text AS id"),
        Engine::SqlServer => Some("SELECT CAST(@@SPID AS varchar(11)) AS id"),
        Engine::Oracle => Some("SELECT SYS_CONTEXT('USERENV', 'SID') AS id FROM dual"),
        Engine::Snowflake => Some("SELECT CURRENT_SESSION() AS id"),
        Engine::D1 | Engine::MongoDb => None,
    }
}

/// The statement for `action` on `target`. Ids are formatted from integers (and a
/// validated query id) only.
pub fn action_sql(
    engine: Engine,
    action: ActivityAction,
    target: &SessionTarget,
) -> Result<String> {
    use ActivityAction::{CancelQuery, Terminate};
    let unsupported =
        || ActivityError::Unsupported(format!("{} is not available here", action.label()));
    match (engine, target) {
        (Engine::Postgres, SessionTarget::Backend { id }) => {
            let id = positive(*id)?;
            Ok(match action {
                CancelQuery => format!("SELECT pg_cancel_backend({id}) AS done"),
                Terminate => format!("SELECT pg_terminate_backend({id}) AS done"),
            })
        }
        (Engine::SqlServer, SessionTarget::Backend { id }) => match action {
            Terminate => Ok(format!("KILL {}", positive(*id)?)),
            CancelQuery => Err(unsupported()),
        },
        (Engine::Oracle, SessionTarget::Oracle { sid, serial }) => {
            let (sid, serial) = (positive(*sid)?, positive(*serial)?);
            Ok(match action {
                CancelQuery => format!("ALTER SYSTEM CANCEL SQL '{sid},{serial}'"),
                Terminate => format!("ALTER SYSTEM KILL SESSION '{sid},{serial}' IMMEDIATE"),
            })
        }
        (Engine::Snowflake, SessionTarget::SnowflakeQuery { query_id, .. }) => match action {
            CancelQuery => Ok(format!(
                "SELECT SYSTEM$CANCEL_QUERY('{}') AS done",
                validate_query_id(query_id)?
            )),
            Terminate => Err(unsupported()),
        },
        _ => Err(unsupported()),
    }
}

fn positive(id: i64) -> Result<i64> {
    if id > 0 {
        Ok(id)
    } else {
        Err(ActivityError::InvalidId(id.to_string()))
    }
}

/// A Snowflake query id: a UUID (hex digits and dashes in 8-4-4-4-12 groups).
pub fn validate_query_id(id: &str) -> Result<&str> {
    let groups: Vec<&str> = id.split('-').collect();
    let ok = groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()));
    if ok {
        Ok(id)
    } else {
        Err(ActivityError::InvalidId(id.to_owned()))
    }
}

/// A plain positive integer (`1234`), nothing else: no sign, spaces or SQL.
fn parse_int(s: &str) -> Result<i64> {
    let t = s.trim();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
        return Err(ActivityError::InvalidId(s.to_owned()));
    }
    t.parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ActivityError::InvalidId(s.to_owned()))
}

/// Validate a listed id into a target: an integer (PostgreSQL, SQL Server), the Oracle
/// `sid,serial` pair, or a Snowflake query id with its session (`session` column).
pub fn parse_target(engine: Engine, id: &str, session: Option<&str>) -> Result<SessionTarget> {
    match engine {
        Engine::Postgres | Engine::SqlServer => Ok(SessionTarget::Backend { id: parse_int(id)? }),
        Engine::Oracle => {
            let (sid, serial) = id
                .split_once(',')
                .ok_or_else(|| ActivityError::InvalidId(id.to_owned()))?;
            Ok(SessionTarget::Oracle {
                sid: parse_int(sid)?,
                serial: parse_int(serial)?,
            })
        }
        Engine::Snowflake => Ok(SessionTarget::SnowflakeQuery {
            query_id: validate_query_id(id.trim())?.to_owned(),
            session: parse_int(session.unwrap_or_default())?,
        }),
        Engine::D1 | Engine::MongoDb => Err(ActivityError::Unsupported(format!(
            "the activity monitor is not available for {}",
            engine.display_name()
        ))),
    }
}

/// Refuse `target` when it is the monitor's own session (`own`, as read by
/// [`own_session_sql`]).
pub fn check_not_self(target: &SessionTarget, own: &str) -> Result<()> {
    match parse_int(own) {
        Ok(own) if own == target.session_key() => Err(ActivityError::OwnSession),
        Ok(_) => Ok(()),
        // Unknown own id: refuse rather than risk it.
        Err(_) => Err(ActivityError::Unsupported(
            "could not read the monitor's own session id".into(),
        )),
    }
}

// ---- running -----------------------------------------------------------------------------

/// A small result set as text cells.
#[derive(Debug, Default)]
struct Rows {
    cols: Vec<String>,
    data: Vec<Vec<Option<String>>>,
}

impl Rows {
    fn text(&self, row: usize, name: &str) -> Option<&str> {
        let c = self
            .cols
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))?;
        self.data.get(row)?.get(c)?.as_deref()
    }

    fn string(&self, row: usize, name: &str) -> Option<String> {
        self.text(row, name)
            .map(str::trim_end)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    fn num(&self, row: usize, name: &str) -> Option<f64> {
        self.text(row, name)?.trim().parse().ok()
    }

    fn flag(&self, row: usize, name: &str) -> bool {
        matches!(
            self.text(row, name).map(str::trim),
            Some("true" | "t" | "1" | "True" | "TRUE")
        )
    }
}

async fn rows(session: &mut dyn DbSession, sql: &str) -> Result<Rows, DbError> {
    let mut stream = session.execute(sql, &[]).await?;
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
                    if out.data.len() >= LIMIT {
                        break;
                    }
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

/// Whether the server refused for lack of permission (or hides the view from the user).
fn permission_denied(e: &DbError) -> bool {
    let Some(s) = e.as_server() else {
        return false;
    };
    // PostgreSQL 42501; SQL Server 297/300 (server state), 229/262; Oracle ORA-00942
    // (view not visible), ORA-01031 (insufficient privileges); Snowflake 003001.
    matches!(
        s.code.as_deref(),
        Some("42501" | "297" | "300" | "229" | "262" | "ORA-00942" | "ORA-01031" | "003001")
    ) || s.message.to_ascii_lowercase().contains("permission")
        || s.message
            .to_ascii_lowercase()
            .contains("insufficient privilege")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Turn listing rows into sessions.
fn sessions(engine: Engine, r: &Rows) -> Vec<ActivitySession> {
    let now = now_ms();
    (0..r.data.len())
        .map(|i| {
            let id = r.string(i, "id").unwrap_or_default();
            let running = r.flag(i, "running");
            let age = r.num(i, "age_ms").map(|v| v.round() as i64);
            ActivitySession {
                target: parse_target(engine, &id, r.text(i, "session_id")).ok(),
                id,
                user: r.string(i, "username"),
                database: r.string(i, "database_name"),
                client: r.string(i, "client"),
                program: r.string(i, "program"),
                state: r.string(i, "state"),
                wait: r.string(i, "wait"),
                started_ms: age.map(|a| now - a),
                duration_ms: age.filter(|_| running),
                sql: r.string(i, "sql_text"),
                blocked_by: r.string(i, "blocked_by"),
                running,
                is_self: r.flag(i, "is_self"),
            }
        })
        .collect()
}

/// List the sessions of the server `session` is connected to.
pub async fn list(
    session: &mut dyn DbSession,
    engine: Engine,
    server_version: &str,
) -> Result<Activity> {
    let Some(sql) = list_sql(engine) else {
        return Err(ActivityError::Unsupported(
            "the activity monitor is not available for Cloudflare D1".into(),
        ));
    };
    let mut out = Activity {
        engine,
        sessions: Vec::new(),
        hints: Vec::new(),
        can_cancel: supports(engine, ActivityAction::CancelQuery, server_version),
        can_terminate: supports(engine, ActivityAction::Terminate, server_version),
    };
    if engine == Engine::SqlServer
        && let Ok(p) = rows(session, MSSQL_PERMISSION).await
        && !p.flag(0, "allowed")
    {
        let login = p
            .string(0, "login_name")
            .unwrap_or_else(|| "<login>".into());
        out.hints.push(ActivityHint {
            message: "Only your own sessions are visible without VIEW SERVER STATE.".into(),
            fix: Some(format!(
                "GRANT VIEW SERVER STATE TO [{}];",
                login.replace(']', "]]")
            )),
        });
    }
    match rows(session, sql).await {
        Ok(r) => out.sessions = sessions(engine, &r),
        Err(e) if permission_denied(&e) => {
            out.hints.push(denied_hint(engine, &e));
            return Ok(out);
        }
        Err(e) => return Err(e.into()),
    }
    if engine == Engine::Postgres
        && out
            .sessions
            .iter()
            .any(|s| s.sql.as_deref() == Some("<insufficient privilege>"))
    {
        out.hints.push(ActivityHint {
            message: "Other users' queries are hidden: the role needs pg_read_all_stats \
                      (and pg_signal_backend to cancel them)."
                .into(),
            fix: Some("GRANT pg_read_all_stats, pg_signal_backend TO current_user;".into()),
        });
    }
    if engine == Engine::Snowflake {
        out.hints.push(ActivityHint {
            message: "Snowflake lists running queries the current role can see; MONITOR on \
                      a warehouse shows other users' queries there."
                .into(),
            fix: None,
        });
    }
    Ok(out)
}

fn denied_hint(engine: Engine, e: &DbError) -> ActivityHint {
    let (message, fix) = match engine {
        Engine::Postgres => (
            "Reading pg_stat_activity was refused.",
            Some("GRANT pg_read_all_stats TO current_user;"),
        ),
        Engine::SqlServer => (
            "Reading the session DMVs needs VIEW SERVER STATE.",
            Some("GRANT VIEW SERVER STATE TO [<login>];"),
        ),
        Engine::Oracle => (
            "Reading V$SESSION and V$SQL needs SELECT on them (SELECT_CATALOG_ROLE).",
            Some("GRANT SELECT_CATALOG_ROLE TO <user>;"),
        ),
        Engine::Snowflake => (
            "Reading INFORMATION_SCHEMA.QUERY_HISTORY was refused (choose a database, or a role \
             with MONITOR on the warehouse).",
            None,
        ),
        Engine::D1 | Engine::MongoDb => ("Not available.", None),
    };
    ActivityHint {
        message: format!("{message} ({e})"),
        fix: fix.map(str::to_owned),
    }
}

/// Cancel the query of, or end, `target`. Reads the session's own id first and refuses
/// when the target is the session this runs on. Returns a short outcome for the UI.
pub async fn act(
    session: &mut dyn DbSession,
    engine: Engine,
    action: ActivityAction,
    target: &SessionTarget,
) -> Result<String> {
    if !supports(engine, action, &session.server_version()) {
        return Err(ActivityError::Unsupported(format!(
            "{} is not available on this server",
            action.label()
        )));
    }
    let sql = action_sql(engine, action, target)?;
    let own_sql = own_session_sql(engine).ok_or_else(|| {
        ActivityError::Unsupported("the activity monitor is not available here".into())
    })?;
    let own = rows(session, own_sql).await?;
    check_not_self(target, own.text(0, "id").unwrap_or_default())?;
    let r = rows(session, &sql).await?;
    // pg_cancel_backend / pg_terminate_backend return false for a pid that is gone.
    if engine == Engine::Postgres && !r.flag(0, "done") {
        return Ok(format!(
            "Session {} was not signalled (it may have ended)",
            target.label()
        ));
    }
    Ok(match action {
        ActivityAction::CancelQuery => format!("Cancel sent to {}", target.label()),
        ActivityAction::Terminate => format!("Session {} ended", target.label()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const QID: &str = "01a2b3c4-0000-1234-0000-00000000abcd";

    #[test]
    fn listing_sql() {
        insta::assert_snapshot!("activity_pg_list", PG_LIST);
        insta::assert_snapshot!("activity_mssql_list", MSSQL_LIST);
        insta::assert_snapshot!("activity_mssql_permission", MSSQL_PERMISSION);
        insta::assert_snapshot!("activity_oracle_list", ORACLE_LIST);
        insta::assert_snapshot!("activity_snowflake_list", SNOWFLAKE_LIST);
        let own: Vec<String> = [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
        ]
        .into_iter()
        .map(|e| format!("{e:?}: {}", own_session_sql(e).unwrap_or_default()))
        .collect();
        insta::assert_snapshot!("activity_own_session", own.join("\n"));
        assert!(list_sql(Engine::D1).is_none());
        assert!(own_session_sql(Engine::D1).is_none());
    }

    #[test]
    fn action_statements() {
        use ActivityAction::{CancelQuery, Terminate};
        let b = SessionTarget::Backend { id: 4242 };
        let o = SessionTarget::Oracle {
            sid: 12,
            serial: 345,
        };
        let s = SessionTarget::SnowflakeQuery {
            query_id: QID.into(),
            session: 99,
        };
        let mut out = Vec::new();
        for (engine, target) in [
            (Engine::Postgres, &b),
            (Engine::SqlServer, &b),
            (Engine::Oracle, &o),
            (Engine::Snowflake, &s),
        ] {
            for action in [CancelQuery, Terminate] {
                let sql = action_sql(engine, action, target).unwrap_or_else(|e| format!("<{e}>"));
                out.push(format!("{engine:?} {action:?}: {sql}"));
            }
        }
        insta::assert_snapshot!("activity_actions", out.join("\n"));
        // A target of the wrong shape for the engine is refused.
        assert!(action_sql(Engine::Postgres, Terminate, &o).is_err());
        assert!(action_sql(Engine::Oracle, Terminate, &b).is_err());
        assert!(action_sql(Engine::D1, Terminate, &b).is_err());
        // Non-positive ids never reach SQL.
        assert!(
            action_sql(
                Engine::Postgres,
                Terminate,
                &SessionTarget::Backend { id: 0 }
            )
            .is_err()
        );
        assert!(
            action_sql(
                Engine::SqlServer,
                Terminate,
                &SessionTarget::Backend { id: -5 }
            )
            .is_err()
        );
        let bad = SessionTarget::SnowflakeQuery {
            query_id: "x'); DROP TABLE t; --".into(),
            session: 1,
        };
        assert!(action_sql(Engine::Snowflake, CancelQuery, &bad).is_err());
    }

    #[test]
    fn ids_are_validated() {
        assert_eq!(
            parse_target(Engine::Postgres, "1234", None).ok(),
            Some(SessionTarget::Backend { id: 1234 })
        );
        assert_eq!(
            parse_target(Engine::SqlServer, " 53 ", None).ok(),
            Some(SessionTarget::Backend { id: 53 })
        );
        for bad in [
            "",
            "-1",
            "0",
            "+5",
            "1.5",
            "12; DROP TABLE x",
            "1 2",
            "abc",
            "99999999999999999999",
        ] {
            assert!(parse_target(Engine::Postgres, bad, None).is_err(), "{bad}");
            assert!(parse_target(Engine::SqlServer, bad, None).is_err(), "{bad}");
        }
        assert_eq!(
            parse_target(Engine::Oracle, "12,345", None).ok(),
            Some(SessionTarget::Oracle {
                sid: 12,
                serial: 345
            })
        );
        for bad in [
            "12",
            "12,",
            ",3",
            "12,3,4",
            "12;3",
            "a,b",
            "12,3' IMMEDIATE --",
        ] {
            assert!(parse_target(Engine::Oracle, bad, None).is_err(), "{bad}");
        }
        assert_eq!(
            parse_target(Engine::Snowflake, QID, Some("99")).ok(),
            Some(SessionTarget::SnowflakeQuery {
                query_id: QID.into(),
                session: 99
            })
        );
        assert!(parse_target(Engine::Snowflake, QID, None).is_err());
        assert!(parse_target(Engine::Snowflake, "not-a-uuid", Some("1")).is_err());
        assert!(
            parse_target(
                Engine::Snowflake,
                "01a2b3c4-0000-1234-0000-00000000abcg",
                Some("1")
            )
            .is_err()
        );
        assert!(parse_target(Engine::D1, "1", None).is_err());
    }

    #[test]
    fn own_session_is_refused() {
        let t = SessionTarget::Backend { id: 77 };
        assert!(matches!(
            check_not_self(&t, "77"),
            Err(ActivityError::OwnSession)
        ));
        assert!(check_not_self(&t, "78").is_ok());
        // Unknown own id: refused too.
        assert!(check_not_self(&t, "").is_err());
        let o = SessionTarget::Oracle { sid: 12, serial: 9 };
        assert!(matches!(
            check_not_self(&o, "12"),
            Err(ActivityError::OwnSession)
        ));
        let s = SessionTarget::SnowflakeQuery {
            query_id: QID.into(),
            session: 5,
        };
        assert!(matches!(
            check_not_self(&s, "5"),
            Err(ActivityError::OwnSession)
        ));
    }

    #[test]
    fn capabilities_per_engine() {
        use ActivityAction::{CancelQuery, Terminate};
        assert!(supports(Engine::Postgres, CancelQuery, "16.2"));
        assert!(!supports(Engine::SqlServer, CancelQuery, "16.0"));
        assert!(supports(Engine::SqlServer, Terminate, "16.0"));
        assert!(supports(Engine::Oracle, CancelQuery, "19.3.0.0.0"));
        assert!(!supports(Engine::Oracle, CancelQuery, "12.2.0.1.0"));
        assert!(supports(Engine::Oracle, Terminate, "12.2.0.1.0"));
        assert!(supports(Engine::Snowflake, CancelQuery, ""));
        assert!(!supports(Engine::Snowflake, Terminate, ""));
        assert!(!supported(Engine::D1));
        assert!(!supports(Engine::D1, Terminate, ""));
    }

    #[test]
    fn rows_become_sessions() {
        let r = Rows {
            cols: [
                "id",
                "username",
                "age_ms",
                "running",
                "sql_text",
                "is_self",
                "blocked_by",
            ]
            .map(String::from)
            .to_vec(),
            data: vec![
                vec![
                    Some("10".into()),
                    Some("app".into()),
                    Some("1500".into()),
                    Some("true".into()),
                    Some("SELECT\n  pg_sleep(5)".into()),
                    Some("false".into()),
                    Some("11, 12".into()),
                ],
                vec![
                    Some("x".into()),
                    None,
                    Some("20".into()),
                    Some("false".into()),
                    None,
                    Some("true".into()),
                    None,
                ],
            ],
        };
        let s = sessions(Engine::Postgres, &r);
        assert_eq!(s[0].target, Some(SessionTarget::Backend { id: 10 }));
        assert_eq!(s[0].duration_ms, Some(1500));
        assert_eq!(s[0].sql_preview(), "SELECT pg_sleep(5)");
        assert_eq!(s[0].blocked_by.as_deref(), Some("11, 12"));
        assert!(s[0].started_ms.is_some());
        // An id that does not validate has no target; idle rows have no duration.
        assert_eq!(s[1].target, None);
        assert_eq!(s[1].duration_ms, None);
        assert!(s[1].is_self);
        assert!(s[0].to_tsv().starts_with("10\tapp\t"));
    }
}
