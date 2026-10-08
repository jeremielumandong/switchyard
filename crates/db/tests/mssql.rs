//! SQL Server integration tests. Need a server with the sample schema (the docker `mssql`
//! service and its seed, or any SQL Server); configure with `SWITCHYARD_MSSQL_HOST`,
//! `_PORT`, `_USER`, `_PASSWORD` and `SWITCHYARD_MSSQL_CA` (PEM of the CA that signed the
//! server's certificate, when it is not publicly trusted). The `shop` database is created
//! from `docker/mssql/seed.sql` when missing.
//! Run with `cargo test -p switchyard-db --test mssql -- --ignored --test-threads 1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::batch::{BatchList, ColumnMeta};
use switchyard_db::catalog::{CatalogChunk, IntrospectScope, ObjectKind};
use switchyard_db::dialect::{Dialect, tsql::TSqlDialect};
use switchyard_db::driver::{DbConfig, DbSession, Driver, SslMode};
use switchyard_db::error::{DbError, ErrorPosition};
use switchyard_db::mssql::MssqlDriver;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::{DataType, Engine, Value};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn config(database: &str) -> DbConfig {
    let mut cfg = DbConfig::new(
        Engine::SqlServer,
        env("SWITCHYARD_MSSQL_HOST", "localhost"),
        database,
    );
    cfg.port = env("SWITCHYARD_MSSQL_PORT", "1433").parse().unwrap();
    cfg.user = env("SWITCHYARD_MSSQL_USER", "sa");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_MSSQL_PASSWORD",
        "Switchyard!2026",
    )));
    cfg.ssl_mode = SslMode::Prefer;
    cfg.trusted_ca_pem = std::env::var("SWITCHYARD_MSSQL_CA")
        .ok()
        .map(|p| std::fs::read_to_string(p).expect("CA file"));
    cfg
}

async fn session(database: &str) -> Box<dyn DbSession> {
    MssqlDriver
        .connect(&config(database), None)
        .await
        .expect("connect")
}

/// Create the sample database once.
async fn shop() -> Box<dyn DbSession> {
    let mut master = session("master").await;
    let seed = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker/mssql/seed.sql"
    ))
    .unwrap();
    for span in TSqlDialect.split_script(&seed) {
        drain(master.as_mut(), span.text(&seed), &[])
            .await
            .expect("seed");
    }
    session("shop").await
}

struct Collected {
    sets: Vec<(Vec<ColumnMeta>, BatchList)>,
    batch_sizes: Vec<usize>,
    affected: Option<u64>,
}

async fn drain(s: &mut dyn DbSession, sql: &str, params: &[Value]) -> Result<Collected, DbError> {
    let mut stream = s.execute(sql, params).await?;
    let mut c = Collected {
        sets: Vec::new(),
        batch_sizes: Vec::new(),
        affected: None,
    };
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(cols) => c.sets.push((cols.to_vec(), BatchList::default())),
            ResultEvent::Rows(b) => {
                c.batch_sizes.push(b.len());
                c.sets.last_mut().unwrap().1.push(b);
            }
            ResultEvent::Done(d) => c.affected = d.affected,
            ResultEvent::Notice(_) | ResultEvent::NextResultSet => {}
        }
    }
    Ok(c)
}

/// `PK__customer__3213E83F1A2B3C4D` → `PK__<generated>`: SQL Server names unnamed
/// constraints with a random suffix.
fn redact_generated_names(ddl: &str) -> String {
    let mut out = String::new();
    let mut rest = ddl;
    while let Some(i) = rest.find("__") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(']').unwrap_or(tail.len());
        out.push_str("__<generated>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn value(c: &Collected, set: usize, row: usize, col: usize) -> Value {
    let (cols, list) = &c.sets[set];
    list.cell(row, col).unwrap().to_value(cols[col].data_type)
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn connects_and_reports_version() {
    let s = session("master").await;
    assert!(
        s.server_version().starts_with("SQL Server"),
        "{}",
        s.server_version()
    );
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn wrong_password_is_a_connect_error() {
    let mut cfg = config("master");
    cfg.password = Some(SecretString::from("nope".to_owned()));
    let err = MssqlDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("must fail");
    assert!(
        matches!(&err, DbError::Connect(m) if m.contains("Login failed")),
        "{err}"
    );
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn untrusted_certificate_is_refused() {
    let mut cfg = config("master");
    cfg.trusted_ca_pem = None;
    if std::env::var("SWITCHYARD_MSSQL_CA").is_err() {
        return; // the server's certificate is publicly trusted here
    }
    let err = MssqlDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("must fail");
    assert!(
        matches!(err, DbError::Tls(_) | DbError::Connect(_)),
        "{err}"
    );
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn maps_every_type() {
    let mut s = session("master").await;
    let c = drain(
        s.as_mut(),
        "SELECT CAST(1 AS bit) AS b, CAST(200 AS tinyint) AS ti, CAST(-3 AS smallint) AS si, \
                CAST(7 AS int) AS i, CAST(9000000000 AS bigint) AS bi, \
                CAST(12.345 AS decimal(10,3)) AS dec, CAST(19.99 AS money) AS m, \
                CAST(1.5 AS float) AS f, CAST(2.25 AS real) AS r, \
                CAST('abc' AS char(3)) AS ch, CAST('héllo' AS varchar(20)) AS vc, N'日本' AS nv, \
                CAST(0xDEAD AS varbinary(10)) AS vb, CAST(0x01 AS binary(1)) AS bin, \
                CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS u, \
                CAST('2024-02-29' AS date) AS d, CAST('13:45:30.1234567' AS time) AS t, \
                CAST('2024-02-29T13:45:30.123' AS datetime) AS dt, \
                CAST('2024-02-29T13:45:30.1234567' AS datetime2) AS dt2, \
                CAST('2024-02-29T13:45:30.5+02:00' AS datetimeoffset) AS dto, \
                CAST('<a>1</a>' AS xml) AS x, CAST(NULL AS int) AS n",
        &[],
    )
    .await
    .unwrap();
    let cols = &c.sets[0].0;
    let types: Vec<(&str, DataType)> = cols
        .iter()
        .map(|c| (c.type_name.as_str(), c.data_type))
        .collect();
    assert_eq!(types[0], ("bit", DataType::Bool));
    assert_eq!(types[5].1, DataType::Numeric);
    assert_eq!(types[14], ("uniqueidentifier", DataType::Uuid));
    assert_eq!(types[19], ("datetimeoffset", DataType::TimestampTz));
    let v = |i| value(&c, 0, 0, i).to_display();
    let shown: Vec<String> = (0..cols.len()).map(v).collect();
    assert_eq!(
        shown,
        [
            "true",
            "200",
            "-3",
            "7",
            "9000000000",
            "12.345",
            "19.99",
            "1.5",
            "2.25",
            "abc",
            "héllo",
            "日本",
            "\\xdead",
            "\\x01",
            "6f9619ff-8b86-d011-b42d-00c04fc964ff",
            "2024-02-29",
            "13:45:30.123456",
            "2024-02-29 13:45:30.123",
            "2024-02-29 13:45:30.123456",
            "2024-02-29 11:45:30.5+00",
            "<a>1</a>",
            "NULL",
        ]
    );
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn several_result_sets_and_affected_rows() {
    let mut s = session("master").await;
    let c = drain(s.as_mut(), "SELECT 1 AS a; SELECT 'x' AS b, 2 AS c", &[])
        .await
        .unwrap();
    assert_eq!(c.sets.len(), 2);
    assert_eq!(c.sets[1].0.len(), 2);
    assert_eq!(value(&c, 1, 0, 0), Value::Text("x".into()));
    assert_eq!(c.affected, None);

    let c = drain(
        s.as_mut(),
        "CREATE TABLE #t (a int); INSERT INTO #t VALUES (1), (2), (3)",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(c.affected, Some(3));
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn streams_in_batches() {
    let mut s = session("master").await;
    let c = drain(
        s.as_mut(),
        "SELECT TOP (5000) a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(c.batch_sizes.iter().sum::<usize>(), 5000);
    assert_eq!(c.batch_sizes[0], 200, "small first batch");
    assert_eq!(c.batch_sizes[1], 1000);
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn cancel_stops_waitfor_quickly_and_the_session_survives() {
    let mut s = session("master").await;
    let cancel = s.cancel_handle();
    let t = Instant::now();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let err = drain(s.as_mut(), "WAITFOR DELAY '00:00:30'", &[])
        .await
        .err()
        .expect("cancelled");
    assert!(matches!(err, DbError::Cancelled), "{err}");
    assert!(
        t.elapsed() < Duration::from_millis(1300),
        "{:?}",
        t.elapsed()
    );
    let c = drain(s.as_mut(), "SELECT 42", &[]).await.unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(42));
    // A cancel with nothing running does not hit the next query.
    s.cancel_handle().cancel().await.unwrap();
    let c = drain(s.as_mut(), "SELECT 43", &[]).await.unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(43));
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn cancel_while_rows_stream_and_the_session_survives() {
    let mut s = session("master").await;
    let cancel = s.cancel_handle();
    let mut stream = s
        .execute(
            "SELECT a.object_id, b.name FROM sys.all_columns a CROSS JOIN sys.all_columns b",
            &[],
        )
        .await
        .unwrap();
    let mut rows = 0;
    let mut notices = 0;
    let end = loop {
        match stream.next().await {
            Some(Ok(ResultEvent::Rows(b))) => {
                rows += b.len();
                if rows >= 2000 {
                    cancel.cancel().await.unwrap();
                }
            }
            Some(Ok(ResultEvent::Notice(_))) => notices += 1,
            Some(Ok(_)) => {}
            Some(Err(e)) => break e,
            None => panic!("finished without cancel"),
        }
    };
    drop(stream);
    assert!(matches!(end, DbError::Cancelled), "{end}");
    // SQL Server acknowledges the attention in a message of its own; the driver reconnects
    // and says so once.
    assert!(notices <= 1, "{notices}");
    let c = drain(s.as_mut(), "SELECT 44", &[]).await.unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(44));
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn errors_carry_code_and_line() {
    let mut s = session("master").await;
    let err = drain(s.as_mut(), "SELECT 1\n\nSELECT * FROM nope", &[])
        .await
        .err()
        .expect("error");
    let server = err.as_server().expect("server error");
    assert_eq!(server.code.as_deref(), Some("208"));
    assert_eq!(server.position, Some(ErrorPosition::Line(3)));
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn named_parameters_bind_positionally() {
    let mut s = session("master").await;
    let (sql, names) = TSqlDialect.bind_params("SELECT @answer + 1 AS a, @name AS n, @answer AS b");
    assert_eq!(names, ["@answer", "@name"]);
    let c = drain(
        s.as_mut(),
        &sql,
        &[Value::Int(41), Value::Text("swy".into())],
    )
    .await
    .unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(42));
    assert_eq!(value(&c, 0, 0, 1), Value::Text("swy".into()));
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn rollback_discards_changes() {
    let mut s = session("master").await;
    drain(s.as_mut(), "CREATE TABLE #r (a int)", &[])
        .await
        .unwrap();
    s.begin().await.unwrap();
    assert!(s.in_transaction());
    drain(s.as_mut(), "INSERT INTO #r VALUES (1)", &[])
        .await
        .unwrap();
    s.rollback().await.unwrap();
    let c = drain(s.as_mut(), "SELECT COUNT(*) FROM #r", &[])
        .await
        .unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(0));
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn catalog_of_the_sample_schema() {
    let mut s = shop().await;
    let CatalogChunk::Schemas(schemas) = s.introspect(IntrospectScope::Schemas).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(schemas[0].name, "dbo");
    assert!(!schemas[0].is_system);
    let CatalogChunk::Objects(tables) = s
        .introspect(IntrospectScope::Objects {
            schema: "dbo".into(),
            kind: ObjectKind::Table,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let customers = tables
        .iter()
        .find(|t| t.name == "customers")
        .expect("customers");
    assert_eq!(customers.estimated_rows, Some(1000));
    let CatalogChunk::Detail(detail) = s
        .introspect(IntrospectScope::Detail {
            schema: "dbo".into(),
            name: "customers".into(),
            kind: ObjectKind::Table,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let cols: Vec<(&str, &str, bool, bool)> = detail
        .columns
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.data_type.as_str(),
                c.nullable,
                c.is_primary_key,
            )
        })
        .collect();
    assert_eq!(
        cols,
        [
            ("id", "int", false, true),
            ("email", "nvarchar(200)", false, false),
            ("segment", "nvarchar(20)", true, false),
            ("created_at", "datetime2", false, false),
        ]
    );
    assert!(
        detail
            .ddl
            .starts_with("CREATE TABLE [dbo].[customers] (\n    [id] int IDENTITY NOT NULL"),
        "{}",
        detail.ddl
    );
    assert!(detail.ddl.contains("PRIMARY KEY ([id])"), "{}", detail.ddl);
    insta::assert_snapshot!("mssql_customers_ddl", redact_generated_names(&detail.ddl));
    let mut snap = Vec::new();
    for kind in TSqlDialect.object_folders() {
        let CatalogChunk::Objects(objs) = s
            .introspect(IntrospectScope::Objects {
                schema: "dbo".into(),
                kind: *kind,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        for o in objs {
            snap.push(format!("{kind:?} {}", o.name));
        }
    }
    insta::assert_snapshot!("mssql_dbo_objects", snap.join("\n"));
    let CatalogChunk::AllColumns(all) = s.introspect(IntrospectScope::AllColumns).await.unwrap()
    else {
        panic!()
    };
    assert!(
        all.iter()
            .any(|c| c.table == "customers" && c.name == "email")
    );
}

/// Records what the driver asked for and hands back a token the server can't accept.
struct FakeKerberos(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

struct FakeStep(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl switchyard_db::SecurityContext for FakeStep {
    fn step(&mut self, input: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
        self.0
            .lock()
            .unwrap()
            .push(format!("step {}", input.map_or(0, <[u8]>::len)));
        // An RFC 2743 initial-context token header around junk.
        Ok(Some(vec![0x60, 0x06, 0x06, 0x01, 0x00, 0x00, 0x00, 0x00]))
    }
}

impl switchyard_db::SecurityProvider for FakeKerberos {
    fn start(&self, spn: &str) -> Result<Box<dyn switchyard_db::SecurityContext>, String> {
        self.0.lock().unwrap().push(format!("start {spn}"));
        Ok(Box::new(FakeStep(self.0.clone())))
    }
}

#[tokio::test]
#[ignore = "needs SQL Server"]
async fn integrated_auth_sends_the_providers_token_in_the_login() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut cfg = config("master");
    cfg.auth = switchyard_db::DbAuthMethod::Integrated;
    cfg.password = None;
    cfg.security = Some(std::sync::Arc::new(FakeKerberos(seen.clone())));
    let err = match MssqlDriver.connect(&cfg, None).await {
        Ok(_) => panic!("a junk token must not sign in"),
        Err(e) => e.to_string(),
    };
    let seen = seen.lock().unwrap().clone();
    // The service name comes from the configured server, and the token went out in LOGIN7.
    assert_eq!(
        seen.first().map(String::as_str),
        Some("start MSSQLSvc/localhost:1433")
    );
    assert_eq!(seen.get(1).map(String::as_str), Some("step 0"));
    // The server answers with a login failure, not a protocol error or a hang.
    let lower = err.to_lowercase();
    assert!(
        lower.contains("login") || lower.contains("sspi") || lower.contains("security"),
        "{err}"
    );
}

#[tokio::test]
#[ignore = "needs SQL Server"]
async fn windows_account_runs_ntlm_and_the_server_refuses_an_unknown_domain() {
    let mut cfg = config("master");
    cfg.auth = switchyard_db::DbAuthMethod::WindowsPassword;
    cfg.user = "SWITCHYARD\\nobody".into();
    cfg.password = Some(SecretString::from("not-a-real-password".to_owned()));
    let err = match MssqlDriver.connect(&cfg, None).await {
        Ok(_) => panic!("an unknown domain account must not sign in"),
        Err(e) => e.to_string(),
    };
    let lower = err.to_lowercase();
    assert!(
        lower.contains("login") || lower.contains("domain") || lower.contains("sspi"),
        "{err}"
    );
}

/// `schema.name Kind` of every hit of a global object search.
async fn search(s: &mut dyn DbSession, pattern: &str) -> Vec<String> {
    let chunk = s
        .introspect(IntrospectScope::Search {
            pattern: pattern.into(),
            limit: 200,
            include_system: false,
        })
        .await
        .expect("search");
    let CatalogChunk::Objects(hits) = chunk else {
        panic!("search returns objects");
    };
    hits.iter()
        .map(|o| format!("{}.{} {:?}", o.schema, o.name, o.kind))
        .collect()
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn global_object_search() {
    let mut s = shop().await;
    let hits = search(s.as_mut(), "CUSTOM").await;
    assert_eq!(hits[0], "dbo.customers Table");
    // `[` and `_` are literal, not LIKE wildcards.
    assert!(search(s.as_mut(), "[c]ustomers").await.is_empty());
    assert!(search(s.as_mut(), "cust_mers").await.is_empty());
    // sys objects are left out by default.
    assert!(search(s.as_mut(), "sysobjects").await.is_empty());
}

#[tokio::test]
#[ignore = "needs sql server"]
async fn use_database_switches_the_session() {
    let mut s = session("master").await;
    let sql = TSqlDialect.use_database("tempdb").expect("USE statement");
    drain(s.as_mut(), &sql, &[]).await.expect("use");
    let c = drain(s.as_mut(), "SELECT DB_NAME()", &[]).await.unwrap();
    assert_eq!(c.sets[0].1.cell(0, 0).unwrap().to_display(), "tempdb");
    // SQL Server has no per-session default schema to switch.
    assert_eq!(TSqlDialect.use_schema("sales"), None);
}
