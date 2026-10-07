//! `swy` and `swy mcp` end to end against the docker PostgreSQL service
//! (`docker compose -f docker/compose.yml up -d`). Run with `--ignored --test-threads 1`.
//!
//! Each test builds a private Switchyard home with a fallback vault, saves connections
//! through the core, then runs the real `swy` binary on it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command as Proc, Output, Stdio};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use secrecy::SecretString;
use serde_json::{Value, json};
use switchyard_core::agent_run::{
    AgentRunRequest, SessionToken, TOKEN_ENV, TOKEN_TTL, start_agent_run,
};
use switchyard_core::agents::{AgentEvent, AgentKind};
use switchyard_core::db::{Engine, SslMode};
use switchyard_core::store::{AppPaths, DbConnection, EnvironmentLabel, Profile, ProfileId, Store};
use switchyard_core::{Command, Core, Event, SecretBackendChoice, ServiceConfig};

const VAULT_PASSWORD: &str = "test vault password";
/// A login with SELECT only; agents connect as this role.
const READER: &str = "swy_mcp_reader";
const READER_PASSWORD: &str = "reader-Secret-77";

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn pg(name: &str, user: &str, env_label: EnvironmentLabel, agents: bool) -> DbConnection {
    let mut c = DbConnection::new(name, Engine::Postgres);
    c.server = env("SWITCHYARD_PG_HOST", "127.0.0.1");
    c.port = env("SWITCHYARD_PG_PORT", "5432").parse().unwrap();
    c.user = user.into();
    c.database = env("SWITCHYARD_PG_DB", "shop");
    c.ssl_mode = SslMode::Disable;
    c.environment = env_label;
    c.agent_access = agents;
    c
}

/// A Switchyard home holding `admin` (the docker superuser, no agent access), `shop` and
/// `prod-agents` (the reader; agents allowed) and `prod` (reader, Production, no agents).
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    async fn new() -> Self {
        let admin_pw = env("SWITCHYARD_PG_PASSWORD", "switchyard");
        let home = Self::with(vec![
            (
                pg(
                    "admin",
                    &env("SWITCHYARD_PG_USER", "switchyard"),
                    EnvironmentLabel::Development,
                    false,
                ),
                admin_pw,
            ),
            (
                pg("shop", READER, EnvironmentLabel::Development, true),
                READER_PASSWORD.into(),
            ),
            (
                pg("prod", READER, EnvironmentLabel::Production, false),
                READER_PASSWORD.into(),
            ),
            (
                pg("prod-agents", READER, EnvironmentLabel::Production, true),
                READER_PASSWORD.into(),
            ),
        ])
        .await;
        // The reader role (idempotent).
        let out = home.swy(&[
            "query",
            "admin",
            &format!(
                "DO $$ BEGIN
                   IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{READER}') THEN
                     CREATE ROLE {READER} LOGIN PASSWORD '{READER_PASSWORD}';
                   END IF;
                 END $$;
                 GRANT SELECT ON ALL TABLES IN SCHEMA public TO {READER};
                 GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO {READER};"
            ),
        ]);
        assert!(out.status.success(), "{}", stderr(&out));
        home
    }

    /// A home holding `profiles` (with their passwords) in a fallback vault.
    async fn with(profiles: Vec<(DbConnection, String)>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::under(dir.path().to_owned(), false);
        let mut cfg = ServiceConfig::from_paths(&paths);
        cfg.secrets = SecretBackendChoice::Vault(paths.vault_file());
        let (core, mut rx) = Core::start(cfg).unwrap();
        let h = core.handle();
        h.send(Command::UnlockVault {
            password: SecretString::from(VAULT_PASSWORD),
        });
        wait(&mut rx, |e| {
            matches!(e, Event::SecretBackend { locked: false, .. }).then_some(())
        })
        .await;
        for (i, (c, pw)) in profiles.into_iter().enumerate() {
            let request = 100 + i as u64;
            h.send(Command::SaveProfile {
                request,
                profile: Profile::Db(c),
                secret: Some(SecretString::from(pw)),
            });
            wait(&mut rx, |e| {
                matches!(e, Event::ProfileSaved { request: r, .. } if r == request).then_some(())
            })
            .await;
        }
        drop(core);
        Self { dir }
    }

    fn cmd(&self) -> Proc {
        let mut p = Proc::new(env!("CARGO_BIN_EXE_swy"));
        p.env("SWITCHYARD_HOME", self.dir.path())
            .env("SWITCHYARD_SECRETS", "vault")
            .env("SWITCHYARD_VAULT_PASSWORD", VAULT_PASSWORD)
            .env_remove("SWITCHYARD_AGENT")
            .env_remove("RUST_LOG");
        // SQL Server's test certificate is signed by the CA from scripts/mssql-test-server.sh.
        if let Ok(ca) = std::env::var("SWITCHYARD_MSSQL_CA") {
            p.env("SSL_CERT_FILE", ca)
                .env("SSL_CERT_DIR", "/nonexistent");
        }
        p
    }

    fn swy(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn mcp(&self) -> Mcp {
        self.mcp_env(&[("SWITCHYARD_AGENT", "claude-code")])
    }

    fn mcp_env(&self, env: &[(&str, &str)]) -> Mcp {
        let mut child = self
            .cmd()
            .arg("mcp")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut m = Mcp {
            child,
            stdin,
            stdout,
            next: 0,
            seen: String::new(),
        };
        let init = m.request("initialize", json!({}));
        assert_eq!(init["serverInfo"]["name"], "switchyard");
        m
    }

    fn data_dir(&self) -> std::path::PathBuf {
        AppPaths::under(self.dir.path().to_owned(), false).data
    }

    fn id(&self, name: &str) -> ProfileId {
        self.store()
            .profiles()
            .unwrap()
            .into_iter()
            .find_map(|p| match p {
                Profile::Db(d) if d.name == name => Some(d.id),
                _ => None,
            })
            .unwrap()
    }

    fn store(&self) -> Store {
        Store::open(&AppPaths::under(self.dir.path().to_owned(), false).store_file()).unwrap()
    }
}

async fn wait<T>(
    rx: &mut switchyard_core::EventReceiver,
    mut f: impl FnMut(Event) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(t) = f(rx.next().await.expect("events ended")) {
                return t;
            }
        }
    })
    .await
    .expect("timed out")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A running `swy mcp`.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
    /// Every line the server wrote.
    seen: String,
}

impl Mcp {
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let line = json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        let mut reply = String::new();
        self.stdout.read_line(&mut reply).unwrap();
        self.seen.push_str(&reply);
        let v: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["id"], self.next, "{v}");
        v["result"].clone()
    }

    /// Call a tool: (is_error, text).
    fn call(&mut self, tool: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        (
            r["isError"].as_bool().unwrap(),
            r["content"][0]["text"].as_str().unwrap().to_owned(),
        )
    }

    fn rows(&mut self, args: Value) -> Value {
        let (err, text) = self.call("run_query", args);
        assert!(!err, "{text}");
        serde_json::from_str(&text).unwrap()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
#[ignore = "needs docker"]
async fn cli_query_formats_and_connections() {
    let home = Home::new().await;

    let out = home.swy(&["connections", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let list: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 4);
    let text = stdout(&out);
    assert!(
        !text.contains("127.0.0.1") && !text.contains(READER),
        "{text}"
    );

    let sql = "select id, email from customers where id <= 3 order by id";
    let out = home.swy(&["query", "shop", sql, "--format", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "id,email\n1,user1@example.com\n2,user2@example.com\n3,user3@example.com\n"
    );

    let out = home.swy(&["query", "shop", sql, "--format", "json"]);
    let first: Value = serde_json::from_str(stdout(&out).lines().next().unwrap()).unwrap();
    assert_eq!(first, json!({"id": 1, "email": "user1@example.com"}));

    let out = home.swy(&["query", "shop", sql]);
    let table = stdout(&out);
    assert!(table.starts_with("id  email"), "{table}");
    assert!(table.ends_with("(3 rows)\n"), "{table}");

    // Standard input, two statements, two result sets.
    let mut child = home
        .cmd()
        .args(["query", "shop", "--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"select 1 as a;\nselect 2 as b;\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(stdout(&out), "a\n1\n\nb\n2\n");

    // Errors go to stderr with a failing status.
    let out = home.swy(&["query", "shop", "select * from no_such_table"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no_such_table"), "{}", stderr(&out));
    let out = home.swy(&["query", "nope", "select 1"]);
    assert!(
        stderr(&out).contains("no connection named"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test]
#[ignore = "needs docker"]
async fn cli_explain_workload_and_open() {
    let home = Home::new().await;
    let sql = "select * from orders where total = 123.45";

    let out = home.swy(&["explain", "shop", sql]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("Estimated plan"), "{text}");
    assert!(text.contains("on orders"), "{text}");
    assert!(
        text.contains("Hotspots:") || text.contains("No hotspots."),
        "{text}"
    );

    let out = home.swy(&["explain", "shop", sql, "--analyze", "--format", "json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["plan"]["kind"], "actual");
    assert!(v["history_id"].as_i64().is_some());

    // An actual plan of a write on Production needs --yes.
    let out = home.swy(&[
        "explain",
        "prod",
        "update orders set status = status where id = 1",
        "--analyze",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));

    // --open without a running app says so and still succeeds.
    let out = home.swy(&["explain", "shop", sql, "--open"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("not running"), "{}", stderr(&out));

    let out = home.swy(&["workload", "admin"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Top statements"), "{}", stdout(&out));
    let out = home.swy(&["workload", "admin", "--json"]);
    let w: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(w["tables"].as_array().is_some_and(|t| !t.is_empty()));
}

#[tokio::test]
#[ignore = "needs docker"]
async fn cli_explain_open_hands_off_to_the_app() {
    let home = Home::new().await;
    // Stand in for the app: a core with the handoff listener on the same data dir.
    let paths = AppPaths::under(home.dir.path().to_owned(), false);
    let mut cfg = ServiceConfig::from_paths(&paths);
    cfg.secrets = SecretBackendChoice::Memory;
    let (core, mut rx) = Core::start(cfg).unwrap();
    core.handle().send(Command::StartHandoff {
        data_dir: paths.data.clone(),
    });
    let file = switchyard_core::handoff::handoff_file(&paths.data);
    let start = Instant::now();
    while !file.exists() && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let sql = "select count(*) from customers";
    let out = home.swy(&["explain", "shop", sql, "--open"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Opened in Switchyard"),
        "{}",
        stderr(&out)
    );
    let got = wait(&mut rx, |e| match e {
        Event::Handoff(h) => Some(h),
        _ => None,
    })
    .await;
    let switchyard_core::handoff::Handoff::OpenPlan { history_id, sql: s } = got;
    assert_eq!(s, sql);
    assert!(home.store().plan(history_id).unwrap().is_some());
}

#[tokio::test]
#[ignore = "needs docker"]
async fn mcp_lists_only_agent_enabled_connections() {
    let home = Home::new().await;
    let mut m = home.mcp();
    let tools = m.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "list_connections",
            "list_tables",
            "describe_table",
            "run_query",
            "explain",
            "workload",
            "what_if"
        ]
    );
    let (err, text) = m.call("list_connections", json!({}));
    assert!(!err);
    let list: Value = serde_json::from_str(&text).unwrap();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    // Production without agent access and the admin connection stay hidden.
    assert_eq!(names, ["shop", "prod-agents"]);
    for hidden in ["prod", "admin"] {
        let (err, text) = m.call(
            "run_query",
            json!({"connection": hidden, "sql": "select 1"}),
        );
        assert!(err);
        assert!(
            text.contains("not available") || text.contains("no connection"),
            "{text}"
        );
    }
}

#[tokio::test]
#[ignore = "needs docker"]
async fn mcp_rejects_writes() {
    let home = Home::new().await;
    let mut m = home.mcp();
    let before = m.rows(json!({"connection": "shop", "sql": "select count(*) as n from orders"}));
    for sql in [
        "delete from orders",
        "update orders set total = 0 where id = 1",
        "insert into products (sku, name, price) values ('x', 'x', 1)",
        "create table agent_was_here (id int)",
        "drop table orders",
        "select 1; delete from orders",
        "with d as (delete from orders where id = 1 returning *) select * from d",
        "select * into agent_copy from orders",
        "truncate orders",
    ] {
        let (err, text) = m.call("run_query", json!({"connection": "shop", "sql": sql}));
        assert!(err, "{sql} was accepted: {text}");
        assert!(text.contains("SELECT"), "{sql}: {text}");
    }
    // A SELECT with a side effect fails inside the read-only transaction.
    let (err, text) = m.call(
        "run_query",
        json!({"connection": "shop", "sql": "select nextval('orders_id_seq')"}),
    );
    assert!(err, "{text}");
    assert!(text.contains("read-only"), "{text}");
    // explain refuses DDL and actual plans.
    let (err, _) = m.call(
        "explain",
        json!({"connection": "shop", "sql": "drop table orders"}),
    );
    assert!(err);
    let (err, text) = m.call(
        "explain",
        json!({"connection": "shop", "sql": "select 1", "analyze": true}),
    );
    assert!(err);
    assert!(text.contains("approval"), "{text}");
    // Estimated plans of DML run nothing. (PostgreSQL still checks the DELETE privilege
    // while planning, which this SELECT-only role lacks.)
    let (_, text) = m.call(
        "explain",
        json!({"connection": "shop", "sql": "delete from orders where total > 10"}),
    );
    assert!(text.contains("orders"), "{text}");
    let after = m.rows(json!({"connection": "shop", "sql": "select count(*) as n from orders"}));
    assert_eq!(before["rows"], after["rows"]);

    // Every call is in history, tagged.
    drop(m);
    let entries = home
        .store()
        .search_history("agent:claude-code", None, 100)
        .unwrap();
    assert!(entries.len() >= 14, "{}", entries.len());
    assert!(entries.iter().all(|e| e.tags.iter().any(|t| t == "agent")));
    assert!(entries.iter().any(|e| e.sql == "drop table orders"));
}

#[tokio::test]
#[ignore = "needs docker"]
async fn mcp_row_cap_and_timeout_hold() {
    let home = Home::new().await;
    let mut m = home.mcp();
    let sql = "select g from generate_series(1, 5000) g";
    let v = m.rows(json!({"connection": "shop", "sql": sql}));
    assert_eq!(v["rows"].as_array().unwrap().len(), 200, "default cap");
    assert_eq!(v["truncated"], true);
    let v = m.rows(json!({"connection": "shop", "sql": sql, "max_rows": 7}));
    assert_eq!(v["rows"].as_array().unwrap().len(), 7);
    let v = m.rows(json!({"connection": "shop", "sql": sql, "max_rows": 100000}));
    assert_eq!(v["rows"].as_array().unwrap().len(), 1000, "hard cap");
    let v = m.rows(json!({"connection": "shop", "sql": "select 1 as one"}));
    assert_eq!(v["truncated"], false);
    assert_eq!(v["rows"], json!([[1]]));

    let started = Instant::now();
    let (err, text) = m.call(
        "run_query",
        json!({"connection": "shop", "sql": "select pg_sleep(30)", "timeout_seconds": 1}),
    );
    assert!(err, "{text}");
    assert!(text.contains("stopped after"), "{text}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // The session is usable afterwards.
    let v = m.rows(json!({"connection": "shop", "sql": "select 2 as two"}));
    assert_eq!(v["rows"], json!([[2]]));
}

#[tokio::test]
#[ignore = "needs docker"]
async fn mcp_output_never_shows_hosts_users_or_secrets() {
    let home = Home::new().await;
    let mut m = home.mcp();
    let host = env("SWITCHYARD_PG_HOST", "127.0.0.1");
    let calls = [
        ("list_connections", json!({})),
        ("list_tables", json!({"connection": "shop"})),
        (
            "describe_table",
            json!({"connection": "shop", "table": "public.orders"}),
        ),
        (
            "run_query",
            json!({"connection": "shop", "sql": "select current_user as u"}),
        ),
        (
            "explain",
            json!({"connection": "prod-agents", "sql": "select * from orders o join customers c on c.id = o.customer_id where c.email = 'x'"}),
        ),
        ("workload", json!({"connection": "shop"})),
        (
            "what_if",
            json!({"connection": "shop", "sql": "select * from orders where total = 1", "indexes": ["create index on orders (total)"]}),
        ),
        // Errors too.
        (
            "run_query",
            json!({"connection": "shop", "sql": "select * from missing_table"}),
        ),
    ];
    for (tool, args) in calls {
        let (err, text) = m.call(tool, args.clone());
        if tool != "run_query" || !args["sql"].as_str().unwrap_or("").contains("missing") {
            assert!(!err, "{tool}: {text}");
        }
    }
    let seen = m.seen.clone();
    for secret in [READER, READER_PASSWORD, VAULT_PASSWORD, &format!("{host}:")] {
        assert!(!seen.contains(secret), "{secret:?} leaked:\n{seen}");
    }
    // The connection's user, read back from the database, is scrubbed too.
    assert!(seen.contains("[redacted]"), "{seen}");
    assert!(
        !seen.contains(&format!("\\\"{host}\\\"")) && !seen.contains(&format!("\"{host}\"")),
        "{seen}"
    );
    assert!(seen.contains("orders"), "{seen}");
    assert!(seen.contains("uses the hypothetical index"), "{seen}");
}

#[tokio::test]
#[ignore = "needs docker"]
async fn mcp_sql_server_read_only_capped_and_timed() {
    let mut c = DbConnection::new("mssql", Engine::SqlServer);
    c.server = env("SWITCHYARD_MSSQL_HOST", "localhost");
    c.port = env("SWITCHYARD_MSSQL_PORT", "1433").parse().unwrap();
    c.user = env("SWITCHYARD_MSSQL_USER", "sa");
    c.database = "master".into();
    c.ssl_mode = SslMode::Prefer;
    c.agent_access = true;
    let home = Home::with(vec![(
        c,
        env("SWITCHYARD_MSSQL_PASSWORD", "Switchyard!2026"),
    )])
    .await;
    let mut m = home.mcp();
    let sql = "select value from generate_series(1, 5000)";
    let v = m.rows(json!({"connection": "mssql", "sql": sql}));
    assert_eq!(v["rows"].as_array().unwrap().len(), 200);
    assert_eq!(v["truncated"], true);
    let v = m.rows(json!({"connection": "mssql", "sql": "select suser_sname() as u"}));
    assert_eq!(v["rows"], json!([["[redacted]"]]));
    for sql in [
        "delete from spt_values",
        "select 1; drop table spt_values",
        "exec sp_who",
        "select * into #copy from sys.objects",
    ] {
        let (err, text) = m.call("run_query", json!({"connection": "mssql", "sql": sql}));
        assert!(err, "{sql} was accepted: {text}");
    }
    let started = Instant::now();
    let (err, text) = m.call(
        "run_query",
        json!({
            "connection": "mssql",
            "sql": "select count_big(*) from sys.all_objects a cross join sys.all_objects b \
                    cross join sys.all_objects c \
                    where checksum(a.name, b.name, c.name) = 7",
            "timeout_seconds": 1
        }),
    );
    assert!(err, "{text}");
    assert!(text.contains("stopped after"), "{text}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    // No transaction is left open: the next query sees only its own (2 would mean a leak).
    let v = m.rows(json!({"connection": "mssql", "sql": "select @@trancount as n"}));
    assert_eq!(v["rows"], json!([[1]]));
    let (err, text) = m.call(
        "explain",
        json!({"connection": "mssql", "sql": "select name from sys.objects where object_id = 5"}),
    );
    assert!(!err, "{text}");
    assert!(text.starts_with("Estimated plan"), "{text}");
}

#[tokio::test]
#[ignore = "needs docker"]
async fn mcp_session_token_scopes_and_revokes() {
    let home = Home::new().await;
    // "prod" has no agent access, so the token cannot open it; "prod-agents" is not in it.
    let token = SessionToken::issue(
        &home.data_dir(),
        &[home.id("shop"), home.id("prod")],
        AgentKind::Codex,
        TOKEN_TTL,
    )
    .unwrap();
    let mut m = home.mcp_env(&[
        (TOKEN_ENV, token.expose()),
        ("SWITCHYARD_AGENT", "claude-code"),
    ]);
    let (err, text) = m.call("list_connections", json!({}));
    assert!(!err, "{text}");
    let list: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["name"], "shop");
    for hidden in ["prod", "prod-agents", "admin"] {
        let (err, text) = m.call(
            "run_query",
            json!({"connection": hidden, "sql": "select 1"}),
        );
        assert!(err && text.contains("no connection"), "{hidden}: {text}");
    }
    let rows = m.rows(json!({"connection": "shop", "sql": "select 1 as one"}));
    assert_eq!(rows["rows"][0][0], 1, "{rows}");
    // The token's agent tags history, whatever SWITCHYARD_AGENT says.
    let tagged = home
        .store()
        .search_history("agent:codex", None, 10)
        .unwrap();
    assert!(!tagged.is_empty());

    // Revoked: the running server refuses every call.
    token.revoke();
    let (err, text) = m.call("list_connections", json!({}));
    assert!(err && text.contains("ended"), "{text}");

    // A server started with a dead token does not start.
    let out = home
        .cmd()
        .arg("mcp")
        .env(TOKEN_ENV, "0123abcd")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("ended"), "{}", stderr(&out));
}

/// One real Claude Code run against the docker PostgreSQL. Needs `claude` signed in:
/// `SWITCHYARD_LIVE_AGENT=claude cargo test -p switchyard-cli --test cli live_claude -- --ignored`
/// (`SWITCHYARD_LIVE_CLAUDE` picks the executable, `SWITCHYARD_LIVE_MODEL` the model).
#[tokio::test]
#[ignore = "needs docker and a signed-in Claude Code"]
async fn live_claude_code_run() {
    if std::env::var("SWITCHYARD_LIVE_AGENT").as_deref() != Ok("claude") {
        eprintln!("set SWITCHYARD_LIVE_AGENT=claude to run");
        return;
    }
    let home = Home::new().await;
    let data = home.data_dir();
    let home_dir = home.dir.path().to_string_lossy().into_owned();
    let mut run = start_agent_run(
        &data,
        AgentRunRequest {
            agent: AgentKind::ClaudeCode,
            program: std::env::var_os("SWITCHYARD_LIVE_CLAUDE").map(Into::into),
            prompt: "On the connection named \"shop\": call describe_table for the orders table, \
                     then explain `SELECT * FROM orders WHERE customer_id = 42`. Then list the \
                     connections you can see. Answer in two sentences: the plan's top \
                     operation, and the connection names."
                .into(),
            resume: None,
            model: Some(std::env::var("SWITCHYARD_LIVE_MODEL").unwrap_or_else(|_| "haiku".into())),
            connections: vec![home.id("shop")],
            swy: Some(env!("CARGO_BIN_EXE_swy").into()),
            // Test-only: the vault password goes to swy through the config's env block.
            mcp_env: vec![
                ("SWITCHYARD_HOME".into(), home_dir),
                ("SWITCHYARD_SECRETS".into(), "vault".into()),
                ("SWITCHYARD_VAULT_PASSWORD".into(), VAULT_PASSWORD.into()),
            ],
            temp_root: None,
        },
    )
    .unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(300), async {
        while let Some(e) = run.next().await {
            eprintln!("{e:?}");
            events.push(e);
        }
    })
    .await
    .expect("the run finished");

    let tools: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(tools.contains(&"describe_table"), "{tools:?}");
    assert!(tools.contains(&"explain"), "{tools:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { is_error: true, .. })),
        "a tool failed: {events:?}"
    );
    let answer = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Done(s) => Some(s.text.clone()),
            _ => None,
        })
        .expect("an answer");
    assert!(
        !answer.contains("prod-agents"),
        "out-of-scope connection seen: {answer}"
    );
    assert!(matches!(events.last(), Some(AgentEvent::Exited(Some(0)))));
    let history = home
        .store()
        .search_history("agent:claude-code", None, 50)
        .unwrap();
    assert!(history.len() >= 2, "{}", history.len());
    // Revoked: no live token files remain.
    let tokens = data.join("agent-tokens");
    assert_eq!(std::fs::read_dir(tokens).unwrap().count(), 0);
}
