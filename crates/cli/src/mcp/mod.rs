//! `swy mcp`: the MCP server coding agents use.
//!
//! Safety lives here, not in agent settings (CLAUDE.md, agent safety rules):
//! - only connections with agent access are visible or usable, so Production stays hidden
//!   unless someone enabled it on purpose; a run the app started carries a session token
//!   that narrows this further to the run's connections, and every call checks that the
//!   token is still live (it is revoked when the run ends);
//! - `run_query` takes one SELECT/WITH and goes through the core's read-only, rolled-back,
//!   capped, timed agent query;
//! - plans are estimated unless the user approves an actual plan of that exact statement in
//!   the app (app-started runs only, never on Production; writes are rolled back); no tool
//!   runs DDL;
//! - Redis takes read-only commands only (`redis_command`), MongoDB read-only statements;
//! - `run_ssh_command` runs on a Host only in a run the app started, and only after the user
//!   approved that exact command in the app (which runs it and records it);
//! - every call is recorded in history tagged `agent` and `agent:<cli>`;
//! - output names connections only: hosts, ports and users of every saved profile are
//!   scrubbed from all text, errors included.

pub mod server;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde_json::{Value, json};
use switchyard_core::agent_run::{TokenScope, verify_token};
use switchyard_core::db::Engine;
use switchyard_core::db::guard::{is_single_plannable, is_single_select};
use switchyard_core::db::{CatalogChunk, IntrospectScope, ObjectKind, dialect_for};
use switchyard_core::handoff::{
    AgentCommand, AgentCommandOutput, AgentPlan, HandoffError, ask_agent_command, ask_agent_plan,
};
use switchyard_core::service::agent_plan::plan_refusal;
use switchyard_core::service::agent_ssh::{APPROVAL_WAIT, MAX_COMMAND_TIME};
use switchyard_core::store::{DbConnection, Host, Profile};
use switchyard_core::{Command, SessionId};

use crate::client::Client;
use crate::render;
use server::{ToolDef, ToolHost, ToolResult};

/// Rows `run_query` returns unless asked for fewer or more.
pub const DEFAULT_ROW_CAP: usize = 200;
/// The most rows `run_query` returns.
pub const MAX_ROW_CAP: usize = 1000;
/// `run_query` timeout unless asked otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest `run_query` timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(120);
/// `run_ssh_command` timeout unless asked otherwise.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// History tag for the coding CLI driving this server, from `SWITCHYARD_AGENT`.
pub fn agent_tag() -> String {
    let name = std::env::var("SWITCHYARD_AGENT").unwrap_or_default();
    let name = match name.trim() {
        n @ ("claude-code" | "codex" | "gemini") => n,
        _ => "custom",
    };
    format!("agent:{name}")
}

/// Removes connection details from text bound for an agent.
#[derive(Debug, Default)]
pub struct Scrubber {
    /// Lower-case terms, longest first.
    terms: Vec<String>,
}

impl Scrubber {
    /// Terms from every saved profile: database servers and users, SSH addresses and users.
    pub fn from_profiles(profiles: &[Profile]) -> Self {
        let mut terms = Vec::new();
        for p in profiles {
            match p {
                Profile::Db(d) => {
                    terms.push(format!("{}:{}", d.server, d.port));
                    terms.push(d.server.clone());
                    terms.push(d.user.clone());
                }
                Profile::Host(h) => {
                    terms.push(format!("{}:{}", h.address, h.port));
                    terms.push(h.address.clone());
                    terms.push(h.user.clone());
                }
                _ => {}
            }
        }
        Self::new(terms)
    }

    /// Scrub exactly these terms.
    pub fn new(terms: impl IntoIterator<Item = String>) -> Self {
        let mut terms: Vec<String> = terms
            .into_iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
        terms.dedup();
        Self { terms }
    }

    /// `text` with every term (case-insensitive, whole words) replaced.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for term in &self.terms {
            out = replace_word(&out, term, "[redacted]");
        }
        out
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@')
}

/// Replace whole-word, case-insensitive occurrences of the lower-case `term`.
fn replace_word(text: &str, term: &str, with: &str) -> String {
    let lower = text.to_lowercase();
    // Lower-casing can change byte lengths (rare scripts); scrub everything then.
    if lower.len() != text.len() {
        return if lower.contains(term) {
            with.to_owned()
        } else {
            text.to_owned()
        };
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(off) = lower[i..].find(term) {
        let start = i + off;
        let end = start + term.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        // A trailing '.' ends a sentence, not a hostname.
        let after_word = after.is_some_and(|c| {
            is_word(c) && !(c == '.' && !text[end + 1..].starts_with(|c: char| is_word(c)))
        });
        if before.is_some_and(is_word) || after_word {
            out.push_str(&text[i..end]);
        } else {
            out.push_str(&text[i..start]);
            out.push_str(with);
        }
        i = end;
    }
    out.push_str(&text[i..]);
    out
}

/// An app-started run's session token and what it allows.
pub struct Session {
    /// The token, re-checked on every call.
    pub token: String,
    /// Its scope when the server started.
    pub scope: TokenScope,
}

/// The tools, on one core client.
pub struct Tools {
    client: Client,
    scrub: Scrubber,
    tags: Vec<String>,
    sessions: HashMap<String, SessionId>,
    session: Option<Session>,
}

fn s<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    s(args, key).ok_or_else(|| format!("missing argument: {key}"))
}

fn kind_name(k: ObjectKind) -> &'static str {
    match k {
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        _ => "table",
    }
}

const CONNECTION: &str = "Connection name from list_connections.";
const HOST: &str = "SSH host name from list_connections (kind \"ssh\").";

/// Tools that need a SQL engine's catalog or planner.
fn sql_only(conn: &DbConnection, tool: &str) -> Result<(), String> {
    match conn.engine {
        Engine::Redis => Err(format!(
            "{tool} is for SQL connections; {} is Redis: use redis_command",
            conn.name
        )),
        e if e.is_document_store() && tool != "list_tables" && tool != "describe_table" => {
            Err(format!(
                "{tool} is for SQL connections; {} is MongoDB: use run_query with a read-only \
                 mongosh statement (db.coll.find(…), aggregate, countDocuments)",
                conn.name
            ))
        }
        _ => Ok(()),
    }
}

/// An approved command's result as the agent reads it.
fn command_text(o: &AgentCommandOutput) -> String {
    let mut out = match (o.timed_out, o.exit_status) {
        (true, _) => "Stopped at the timeout.\n".to_owned(),
        (false, Some(c)) => format!("Exit status {c}.\n"),
        (false, None) => "No exit status (the command was killed).\n".to_owned(),
    };
    if !o.stdout.is_empty() {
        let _ = write!(out, "--- stdout ---\n{}", o.stdout);
        if !o.stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !o.stderr.is_empty() {
        let _ = write!(out, "--- stderr ---\n{}", o.stderr);
        if !o.stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    if o.truncated {
        out.push_str("(output cut at 64 KB per stream; narrow the command)\n");
    }
    out
}

impl Tools {
    /// Serve `client`'s agent-enabled connections, narrowed to `session`'s when given.
    pub fn new(client: Client, session: Option<Session>) -> Self {
        let scrub = Scrubber::from_profiles(client.profiles());
        let tag = match &session {
            Some(s) => s.scope.agent.history_tag().to_owned(),
            None => agent_tag(),
        };
        Self {
            client,
            scrub,
            tags: vec!["agent".into(), tag],
            sessions: HashMap::new(),
            session,
        }
    }

    /// Connections agents may see: agent access on (it is off by default, so Production
    /// shows only when someone enabled it for that connection), and in the session's
    /// scope when there is one.
    fn visible(&self) -> Vec<&DbConnection> {
        self.client
            .connections()
            .into_iter()
            .filter(|c| c.agent_access)
            .filter(|c| self.session.as_ref().is_none_or(|s| s.scope.allows(&c.id)))
            .collect()
    }

    /// Hosts agents may use: only in an app-started run (the app approves each command),
    /// with agent access on and in the run's scope.
    fn visible_hosts(&self) -> Vec<&Host> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        self.client
            .profiles()
            .iter()
            .filter_map(|p| match p {
                Profile::Host(h) if h.agent_access && session.scope.allows(&h.id) => Some(h),
                _ => None,
            })
            .collect()
    }

    /// The session token is still live (always true without one).
    fn check_session(&self) -> Result<(), String> {
        match &self.session {
            Some(s) => verify_token(self.client.data_dir(), &s.token)
                .map(drop)
                .map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    fn connection(&self, args: &Value) -> Result<DbConnection, String> {
        let name = required(args, "connection")?;
        self.visible()
            .into_iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .cloned()
            // Same answer for "missing" and "not enabled": names of hidden connections leak nothing.
            .ok_or_else(|| {
                format!(
                    "no connection named {name:?} is available to agents (see list_connections)"
                )
            })
    }

    async fn session(&mut self, conn: &DbConnection) -> Result<SessionId, String> {
        if let Some(s) = self.sessions.get(&conn.id.0) {
            return Ok(*s);
        }
        let s = if conn.engine == Engine::Redis {
            self.client.open_redis(conn).await
        } else {
            self.client.open(conn).await
        }
        .map_err(|e| e.to_string())?;
        self.sessions.insert(conn.id.0.clone(), s);
        Ok(s)
    }

    /// Record a call that does not write its own history entry.
    fn record(&self, session: SessionId, summary: String, error: Option<String>) {
        self.client.send(Command::RecordAgentCall {
            session,
            summary,
            error,
            tags: self.tags.clone(),
        });
    }

    /// Close every session.
    pub fn close(&mut self) {
        for (_, s) in self.sessions.drain() {
            self.client.close(s);
        }
    }

    fn list_connections(&self) -> Result<String, String> {
        let list: Vec<Value> = self
            .visible()
            .into_iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "kind": "database",
                    "engine": c.engine.display_name(),
                    "environment": c.environment.name(),
                })
            })
            .chain(self.visible_hosts().into_iter().map(|h| {
                json!({
                    "name": h.name,
                    "kind": "ssh",
                    "environment": h.environment.name(),
                })
            }))
            .collect();
        if list.is_empty() {
            return Ok(
                "No connections are enabled for agents. In Switchyard, edit a \
                       connection or Host and turn on \"Allow coding agents\"."
                    .into(),
            );
        }
        serde_json::to_string_pretty(&list).map_err(|e| e.to_string())
    }

    async fn list_tables(&mut self, args: &Value) -> Result<String, String> {
        let conn = self.connection(args)?;
        sql_only(&conn, "list_tables")?;
        let session = self.session(&conn).await?;
        let schemas: Vec<String> = match s(args, "schema") {
            Some(schema) => vec![schema.to_owned()],
            None => match self
                .client
                .introspect(session, IntrospectScope::Schemas)
                .await
                .map_err(|e| e.to_string())?
            {
                CatalogChunk::Schemas(list) => list
                    .into_iter()
                    .filter(|s| !s.is_system)
                    .map(|s| s.name)
                    .collect(),
                _ => Vec::new(),
            },
        };
        let mut out = String::new();
        let mut count = 0;
        'outer: for schema in &schemas {
            for kind in [
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
            ] {
                let scope = IntrospectScope::Objects {
                    schema: schema.clone(),
                    kind,
                };
                let Ok(CatalogChunk::Objects(objs)) = self.client.introspect(session, scope).await
                else {
                    continue;
                };
                for o in objs {
                    count += 1;
                    if count > 500 {
                        out.push_str("… more than 500 objects; pass a schema to narrow.\n");
                        break 'outer;
                    }
                    let rows = o
                        .estimated_rows
                        .map_or(String::new(), |r| format!(" (~{r} rows)"));
                    let _ = writeln!(out, "{}.{} {}{rows}", o.schema, o.name, kind_name(kind));
                }
            }
        }
        self.record(session, "list_tables".into(), None);
        if out.is_empty() {
            out = "No tables or views found.".into();
        }
        Ok(out)
    }

    async fn describe_table(&mut self, args: &Value) -> Result<String, String> {
        let conn = self.connection(args)?;
        sql_only(&conn, "describe_table")?;
        let table = required(args, "table")?;
        let (schema, name) = match (s(args, "schema"), table.split_once('.')) {
            (Some(sc), _) => (sc.to_owned(), table.to_owned()),
            (None, Some((sc, n))) => (sc.to_owned(), n.to_owned()),
            (None, None) => (
                dialect_for(conn.engine).default_schema().to_owned(),
                table.to_owned(),
            ),
        };
        let session = self.session(&conn).await?;
        let mut last = String::new();
        for kind in [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
        ] {
            let scope = IntrospectScope::Detail {
                schema: schema.clone(),
                name: name.clone(),
                kind,
            };
            match self.client.introspect(session, scope).await {
                Ok(CatalogChunk::Detail(d)) if !d.columns.is_empty() => {
                    self.record(session, format!("describe_table {schema}.{name}"), None);
                    return Ok(render::detail_text(&d));
                }
                Ok(_) => {}
                Err(e) => last = e.to_string(),
            }
        }
        if last.is_empty() {
            last = format!("{schema}.{name} not found");
        }
        self.record(
            session,
            format!("describe_table {schema}.{name}"),
            Some(last.clone()),
        );
        Err(last)
    }

    async fn run_query(&mut self, args: &Value) -> Result<String, String> {
        let conn = self.connection(args)?;
        let sql = required(args, "sql")?.to_owned();
        if conn.engine == Engine::Redis {
            return sql_only(&conn, "run_query").map(|()| String::new());
        }
        // MongoDB statements are checked by the core (one statement that only reads).
        if !conn.engine.is_document_store() && !is_single_select(dialect_for(conn.engine), &sql) {
            let session = self.session(&conn).await?;
            let err = "run_query accepts one SELECT or WITH … SELECT statement only; \
                       writes, DDL and multiple statements are refused";
            self.record(session, sql, Some(err.into()));
            return Err(err.into());
        }
        let cap = args
            .get("max_rows")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_ROW_CAP, |n| (n as usize).clamp(1, MAX_ROW_CAP));
        let timeout = args
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_TIMEOUT, |n| {
                Duration::from_secs(n.max(1)).min(MAX_TIMEOUT)
            });
        let session = self.session(&conn).await?;
        let rows = self
            .client
            .agent_query(session, &sql, cap, timeout, self.tags.clone())
            .await
            .map_err(|e| e.to_string())?;
        let mut v = serde_json::to_value(&rows).map_err(|e| e.to_string())?;
        if rows.truncated {
            v["note"] = json!(format!(
                "only the first {cap} rows are shown; aggregate or filter for the rest"
            ));
        }
        serde_json::to_string(&v).map_err(|e| e.to_string())
    }

    async fn explain(&mut self, args: &Value) -> Result<String, String> {
        let conn = self.connection(args)?;
        sql_only(&conn, "explain")?;
        let sql = required(args, "sql")?.to_owned();
        let session = self.session(&conn).await?;
        let analyze = args.get("analyze").and_then(Value::as_bool) == Some(true);
        if analyze && let Err(err) = self.approve_actual_plan(&conn, &sql).await {
            self.record(
                session,
                format!("explain analyze: {sql}"),
                Some(err.clone()),
            );
            return Err(err);
        }
        if !is_single_plannable(dialect_for(conn.engine), &sql) {
            let err = "explain takes one SELECT, INSERT, UPDATE, DELETE or MERGE statement";
            self.record(session, format!("explain: {sql}"), Some(err.into()));
            return Err(err.into());
        }
        let r = self
            .client
            .explain(session, &sql, analyze, false, self.tags.clone())
            .await;
        // The core records plans only where history is on; agent calls are always recorded.
        if !conn.history_enabled || r.is_err() {
            let what = if analyze {
                "explain analyze"
            } else {
                "explain"
            };
            self.record(
                session,
                format!("{what}: {sql}"),
                r.as_ref().err().map(ToString::to_string),
            );
        }
        let r = r.map_err(|e| e.to_string())?;
        Ok(render::plan_text(&r.plan, &r.findings))
    }

    /// Ask the user, in the app, to approve an actual plan of `sql` on `conn`. Only runs the
    /// app started can ask; Production connections are estimated only.
    async fn approve_actual_plan(&self, conn: &DbConnection, sql: &str) -> Result<(), String> {
        let Some(session) = &self.session else {
            return Err(
                "actual plans (ANALYZE / STATISTICS XML) run the statement and need the \
                        user's approval in the Switchyard app, which only assistant runs started \
                        there can ask for; use the estimated plan"
                    .into(),
            );
        };
        plan_refusal(conn, sql)?;
        let req = AgentPlan {
            session_token: session.token.clone(),
            connection: conn.name.clone(),
            sql: sql.to_owned(),
        };
        let wait = APPROVAL_WAIT + Duration::from_secs(30);
        let data = self.client.data_dir().to_owned();
        tokio::task::spawn_blocking(move || ask_agent_plan(&data, req, wait))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| match e {
                HandoffError::NotRunning => {
                    "the Switchyard app is not running; it must be open to approve an actual plan"
                        .to_owned()
                }
                other => other.to_string(),
            })
    }

    async fn workload(&mut self, args: &Value) -> Result<String, String> {
        let conn = self.connection(args)?;
        sql_only(&conn, "workload")?;
        let session = self.session(&conn).await?;
        let r = self.client.workload(session).await;
        self.record(
            session,
            "workload".into(),
            r.as_ref().err().map(ToString::to_string),
        );
        let w = r.map_err(|e| e.to_string())?;
        Ok(render::workload_text(&w))
    }

    async fn what_if(&mut self, args: &Value) -> Result<String, String> {
        let conn = self.connection(args)?;
        sql_only(&conn, "what_if")?;
        let sql = required(args, "sql")?.to_owned();
        let indexes: Vec<String> = args
            .get("indexes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if indexes.is_empty() {
            return Err("pass at least one CREATE INDEX statement in indexes".into());
        }
        let session = self.session(&conn).await?;
        if !is_single_plannable(dialect_for(conn.engine), &sql) {
            let err = "what_if takes one SELECT, INSERT, UPDATE, DELETE or MERGE statement";
            self.record(session, format!("what_if: {sql}"), Some(err.into()));
            return Err(err.into());
        }
        let r = self.client.what_if(session, &sql, indexes.clone()).await;
        self.record(
            session,
            format!("what_if: {sql}\n-- with: {}", indexes.join("; ")),
            r.as_ref().err().map(ToString::to_string),
        );
        let w = r.map_err(|e| e.to_string())?;
        let mut out = String::new();
        let c = &w.comparison.cost;
        let _ = writeln!(
            out,
            "Estimated cost {} → {}. The planner {} the hypothetical index{}.",
            c.a.map_or("?".into(), |v| format!("{v:.0}")),
            c.b.map_or("?".into(), |v| format!("{v:.0}")),
            if w.uses_hypothetical() {
                "uses"
            } else {
                "does not use"
            },
            if w.indexes.len() == 1 { "" } else { "es" },
        );
        for i in &w.indexes {
            let size = i
                .bytes
                .map_or(String::new(), |b| format!(" (~{:.1} MB)", b / 1_048_576.0));
            let _ = writeln!(out, "- {}{size}", i.definition);
        }
        let _ = writeln!(out, "\nPlan with the indexes:");
        out.push_str(&render::plan_text(&w.after, &[]));
        let _ = writeln!(out, "\nPlan today:");
        out.push_str(&render::plan_text(&w.before, &[]));
        Ok(out)
    }
}

impl Tools {
    async fn redis_command(&mut self, args: &Value) -> Result<String, String> {
        let conn = self.connection(args)?;
        if conn.engine != Engine::Redis {
            return Err(format!(
                "{} is not a Redis connection; use run_query",
                conn.name
            ));
        }
        let line = required(args, "command")?.to_owned();
        let session = self.session(&conn).await?;
        self.client
            .agent_redis(session, &line, self.tags.clone())
            .await
            .map_err(|e| e.to_string())
    }

    async fn run_ssh_command(&mut self, args: &Value) -> Result<String, String> {
        let Some(session) = &self.session else {
            return Err(
                "run_ssh_command works only in assistant runs started by the \
                        Switchyard app, where the user approves each command"
                    .into(),
            );
        };
        let name = required(args, "host")?;
        let host = self
            .visible_hosts()
            .into_iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.name.clone())
            .ok_or_else(|| {
                format!("no SSH host named {name:?} is available to agents (see list_connections)")
            })?;
        let command = required(args, "command")?.to_owned();
        let timeout = args
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_COMMAND_TIME_SECS, |n| {
                n.clamp(1, MAX_COMMAND_TIME.as_secs())
            });
        let req = AgentCommand {
            session_token: session.token.clone(),
            host,
            command,
            timeout_secs: timeout,
        };
        // The user's answer, the command, and some slack for the SSH login.
        let wait = APPROVAL_WAIT + Duration::from_secs(timeout) + Duration::from_secs(60);
        let data = self.client.data_dir().to_owned();
        let out = tokio::task::spawn_blocking(move || ask_agent_command(&data, req, wait))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| match e {
                HandoffError::NotRunning => {
                    "the Switchyard app is not running; it must be open to approve the command"
                        .to_owned()
                }
                other => other.to_string(),
            })?;
        Ok(command_text(&out))
    }
}

const DEFAULT_COMMAND_TIME_SECS: u64 = DEFAULT_COMMAND_TIMEOUT.as_secs();

impl ToolHost for Tools {
    fn tools(&self) -> Vec<ToolDef> {
        let conn_only = json!({
            "type": "object",
            "properties": { "connection": { "type": "string", "description": CONNECTION } },
            "required": ["connection"]
        });
        vec![
            ToolDef {
                name: "list_connections",
                description: "List the connections available to agents: databases (name, \
                              engine, environment) and SSH hosts (kind \"ssh\").",
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            ToolDef {
                name: "list_tables",
                description: "List tables and views, optionally in one schema.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "connection": { "type": "string", "description": CONNECTION },
                        "schema": { "type": "string" }
                    },
                    "required": ["connection"]
                }),
            },
            ToolDef {
                name: "describe_table",
                description: "Columns, indexes, constraints and foreign keys of a table or view.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "connection": { "type": "string", "description": CONNECTION },
                        "table": { "type": "string", "description": "Name, or schema.name." },
                        "schema": { "type": "string" }
                    },
                    "required": ["connection", "table"]
                }),
            },
            ToolDef {
                name: "run_query",
                description: "Run one read-only SELECT/WITH query (read-only transaction, rolled \
                              back), or on MongoDB one mongosh statement that reads \
                              (db.coll.find(…), aggregate, countDocuments, distinct). Returns \
                              JSON with columns and rows; capped at max_rows (default 200, at \
                              most 1000).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "connection": { "type": "string", "description": CONNECTION },
                        "sql": { "type": "string" },
                        "max_rows": { "type": "integer", "minimum": 1, "maximum": MAX_ROW_CAP },
                        "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT.as_secs() }
                    },
                    "required": ["connection", "sql"]
                }),
            },
            ToolDef {
                name: "explain",
                description: "Estimated query plan as an operator tree, with hotspots and \
                              missing-index suggestions. Does not run the statement, unless \
                              analyze asks for an actual plan, which the user must approve in \
                              Switchyard first.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "connection": { "type": "string", "description": CONNECTION },
                        "sql": { "type": "string" },
                        "analyze": { "type": "boolean", "description": "Actual plan (runs the statement; writes are rolled back). The user must approve the exact statement in Switchyard; never on Production. Prefer the estimated plan unless timings matter." }
                    },
                    "required": ["connection", "sql"]
                }),
            },
            ToolDef {
                name: "workload",
                description: "The busiest statements, table scan and index usage statistics, \
                              and missing indexes the engine reports.",
                input_schema: conn_only,
            },
            ToolDef {
                name: "what_if",
                description: "PostgreSQL with HypoPG: plan a statement as if the given indexes \
                              existed. Nothing is created.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "connection": { "type": "string", "description": CONNECTION },
                        "sql": { "type": "string" },
                        "indexes": { "type": "array", "items": { "type": "string" }, "description": "CREATE INDEX statements." }
                    },
                    "required": ["connection", "sql", "indexes"]
                }),
            },
            ToolDef {
                name: "redis_command",
                description: "Redis connections: run one read-only command in redis-cli syntax \
                              (SCAN 0 MATCH user:* COUNT 100, TYPE, GET, HGETALL, LRANGE, \
                              ZRANGE … WITHSCORES, TTL, MEMORY USAGE, INFO, SLOWLOG GET). \
                              Writes and KEYS are refused.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "connection": { "type": "string", "description": CONNECTION },
                        "command": { "type": "string" }
                    },
                    "required": ["connection", "command"]
                }),
            },
            ToolDef {
                name: "run_ssh_command",
                description: "Run one shell command on an SSH host. The user sees the exact \
                              command in Switchyard and must approve it before it runs; a \
                              declined command returns an error. Prefer short read-only \
                              commands, one per call, and say why you need each.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": HOST },
                        "command": { "type": "string" },
                        "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": MAX_COMMAND_TIME.as_secs() }
                    },
                    "required": ["host", "command"]
                }),
            },
        ]
    }

    async fn call(&mut self, name: &str, args: &Value) -> ToolResult {
        if let Err(e) = self.check_session() {
            return ToolResult::error(e);
        }
        let r = match name {
            "list_connections" => self.list_connections(),
            "list_tables" => self.list_tables(args).await,
            "describe_table" => self.describe_table(args).await,
            "run_query" => self.run_query(args).await,
            "explain" => self.explain(args).await,
            "workload" => self.workload(args).await,
            "what_if" => self.what_if(args).await,
            "redis_command" => self.redis_command(args).await,
            "run_ssh_command" => self.run_ssh_command(args).await,
            other => Err(format!("unknown tool: {other}")),
        };
        match r {
            Ok(text) => ToolResult::text(self.scrub.scrub(&text)),
            Err(e) => ToolResult::error(self.scrub.scrub(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_whole_words_case_insensitively() {
        let s = Scrubber::new([
            "db.example.com:5432".into(),
            "db.example.com".into(),
            "sa".into(),
        ]);
        assert_eq!(
            s.scrub("could not connect to DB.example.com:5432. Login failed for user 'sa'."),
            "could not connect to [redacted]. Login failed for user '[redacted]'."
        );
        assert_eq!(s.scrub("host db.example.com."), "host [redacted].");
        // Parts of longer words stay.
        assert_eq!(
            s.scrub("usage of salt in db.example.company"),
            "usage of salt in db.example.company"
        );
    }

    /// The activity monitor's cancel / kill (DBX-5b) is app-only: no MCP tool exposes it
    /// and `swy` never sends its commands.
    #[test]
    fn sql_tools_refuse_redis_and_mongodb() {
        let redis = DbConnection::new("cache", Engine::Redis);
        let mongo = DbConnection::new("docs", Engine::MongoDb);
        let pg = DbConnection::new("app", Engine::Postgres);
        assert!(
            sql_only(&redis, "explain")
                .unwrap_err()
                .contains("redis_command")
        );
        assert!(sql_only(&redis, "list_tables").is_err());
        assert!(sql_only(&mongo, "list_tables").is_ok());
        assert!(
            sql_only(&mongo, "workload")
                .unwrap_err()
                .contains("run_query")
        );
        assert!(sql_only(&pg, "what_if").is_ok());
    }

    #[test]
    fn command_output_reads_plainly() {
        let text = command_text(&AgentCommandOutput {
            exit_status: Some(1),
            stdout: "a".into(),
            stderr: "boom\n".into(),
            truncated: true,
            timed_out: false,
        });
        assert_eq!(
            text,
            "Exit status 1.\n--- stdout ---\na\n--- stderr ---\nboom\n\
             (output cut at 64 KB per stream; narrow the command)\n"
        );
        let timed = command_text(&AgentCommandOutput {
            timed_out: true,
            ..Default::default()
        });
        assert_eq!(timed, "Stopped at the timeout.\n");
    }

    #[test]
    fn no_session_kill_over_mcp() {
        let sources = [
            include_str!("mod.rs"),
            include_str!("server.rs"),
            include_str!("../client.rs"),
            include_str!("../main.rs"),
        ];
        // Split so this test's own text does not match.
        let forbidden = [
            ["Session", "Action"].concat(),
            ["Command::", "Activity"].concat(),
            ["activity::", "act("].concat(),
        ];
        for src in sources {
            // Tool names declared as `name: "…"`.
            for name in src
                .split("name: \"")
                .skip(1)
                .filter_map(|r| r.split('"').next())
            {
                let n = name.to_ascii_lowercase();
                assert!(
                    !["kill", "terminate", "cancel", "activity", "session"]
                        .iter()
                        .any(|w| n.contains(w)),
                    "MCP tool {name} looks like a session action"
                );
            }
            for f in &forbidden {
                assert!(!src.contains(f.as_str()), "{f} is referenced from the CLI");
            }
        }
    }

    #[test]
    fn agent_tag_defaults_to_custom() {
        // SAFETY of the env read: only this test touches SWITCHYARD_AGENT.
        if std::env::var("SWITCHYARD_AGENT").is_err() {
            assert_eq!(agent_tag(), "agent:custom");
        }
    }
}
