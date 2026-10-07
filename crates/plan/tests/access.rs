//! Workload (access analysis) and hypothetical indexes against the docker PostgreSQL
//! (pg_stat_statements preloaded, HypoPG installed; `SWITCHYARD_PG_*` as in `capture.rs`).
//! The tests create a role without `pg_read_all_stats` and a database without extensions.
//!
//! Run with `cargo test -p switchyard-plan --test access -- --ignored --test-threads 1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::driver::{DbConfig, DbSession, Driver, SslMode};
use switchyard_db::pg::PgDriver;
use switchyard_db::value::Engine;
use switchyard_plan::access::{Source, workload};
use switchyard_plan::whatif::what_if;

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

async fn pg_as(db: &str, user: &str, password: &str) -> Box<dyn DbSession> {
    let mut cfg = DbConfig::new(Engine::Postgres, env("SWITCHYARD_PG_HOST", "127.0.0.1"), db);
    cfg.port = env("SWITCHYARD_PG_PORT", "5432").parse().unwrap();
    cfg.user = user.to_owned();
    cfg.password = Some(SecretString::from(password.to_owned()));
    cfg.ssl_mode = SslMode::Disable;
    PgDriver.connect(&cfg, None).await.expect("connect")
}

async fn admin(db: &str) -> Box<dyn DbSession> {
    pg_as(
        db,
        &env("SWITCHYARD_PG_USER", "switchyard"),
        &env("SWITCHYARD_PG_PASSWORD", "switchyard"),
    )
    .await
}

async fn exec(s: &mut dyn DbSession, sql: &str) {
    let mut st = s.execute(sql, &[]).await.expect(sql);
    while let Some(ev) = st.next().await {
        ev.expect(sql);
    }
}

#[tokio::test]
#[ignore]
async fn workload_with_extensions_and_privileges() {
    let mut s = admin("shop").await;
    exec(
        s.as_mut(),
        "select count(*) from orders where status = 'paid'",
    )
    .await;
    let w = workload(s.as_mut(), Engine::Postgres)
        .await
        .expect("workload");
    assert!(w.hypopg.is_some(), "HypoPG is in the test image");
    let orders = w
        .tables
        .iter()
        .find(|t| t.name == "orders")
        .expect("orders");
    assert!(
        orders.rows.unwrap_or(0.0) >= 900_000.0,
        "row count survives a stats reset: {orders:?}"
    );
    assert!(
        w.indexes
            .iter()
            .any(|i| i.name == "orders_customer_id_idx" && i.definition.is_some())
    );
    assert!(
        w.statements.iter().any(|q| q.query.contains("orders")),
        "pg_stat_statements lists the query: {:?}",
        w.statements.iter().map(|q| &q.query).collect::<Vec<_>>()
    );
    assert!(
        !w.statements
            .iter()
            .any(|q| q.query.contains("swy:access") || q.query.contains("swy_access")),
        "probes and their savepoints are left out"
    );
    assert!(w.hints.is_empty(), "{:?}", w.hints);
}

#[tokio::test]
#[ignore]
async fn missing_privilege_is_a_hint_with_the_grant() {
    let mut a = admin("shop").await;
    exec(
        a.as_mut(),
        "DO $$ BEGIN
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'swy_limited') THEN
             CREATE ROLE swy_limited LOGIN PASSWORD 'limited';
           END IF;
         END $$",
    )
    .await;
    exec(a.as_mut(), "GRANT SELECT ON orders TO swy_limited").await;

    let mut s = pg_as("shop", "swy_limited", "limited").await;
    exec(s.as_mut(), "select count(*) from orders where total > 999").await;
    // Inside an open transaction the probes must leave it usable.
    s.begin().await.expect("begin");
    let w = workload(s.as_mut(), Engine::Postgres)
        .await
        .expect("workload");
    exec(s.as_mut(), "select 1").await;
    s.rollback().await.expect("rollback");

    let grant = w
        .hints
        .iter()
        .find(|h| h.source == Source::Statements)
        .and_then(|h| h.fix.clone())
        .expect("statements hint");
    assert_eq!(grant, "GRANT pg_read_all_stats TO \"swy_limited\";");
    assert!(
        w.statements
            .iter()
            .all(|q| q.query != "<insufficient privilege>"),
        "other roles' statements are hidden, not shown as placeholders"
    );
}

#[tokio::test]
#[ignore]
async fn missing_extensions_are_hints_not_errors() {
    let mut a = admin("shop").await;
    exec(a.as_mut(), "DROP DATABASE IF EXISTS swy_noext").await;
    exec(a.as_mut(), "CREATE DATABASE swy_noext TEMPLATE template0").await;
    let mut s = admin("swy_noext").await;
    exec(s.as_mut(), "create table t (id int primary key, v text)").await;
    let w = workload(s.as_mut(), Engine::Postgres)
        .await
        .expect("workload");
    drop(s);

    assert!(w.statements.is_empty());
    let stmt = w
        .hints
        .iter()
        .find(|h| h.source == Source::Statements)
        .expect("statements hint");
    assert_eq!(
        stmt.fix.as_deref(),
        Some("CREATE EXTENSION pg_stat_statements;")
    );
    let hypo = w
        .hints
        .iter()
        .find(|h| h.source == Source::HypoPg)
        .expect("hypopg hint");
    assert!(hypo.message.contains("available"), "{}", hypo.message);
    assert!(w.tables.iter().any(|t| t.name == "t"));

    let err = what_if(
        admin("swy_noext").await.as_mut(),
        Engine::Postgres,
        "select * from t where v = 'x'",
        &["CREATE INDEX ON t (v)".into()],
    )
    .await
    .expect_err("no hypopg");
    assert!(err.to_string().contains("CREATE EXTENSION hypopg"), "{err}");
    exec(a.as_mut(), "DROP DATABASE swy_noext").await;
}

async fn order_indexes(s: &mut dyn DbSession) -> String {
    let mut st = s
        .execute(
            "select count(*) from pg_indexes where tablename = 'orders'",
            &[],
        )
        .await
        .unwrap();
    let mut n = String::new();
    while let Some(ev) = st.next().await {
        if let switchyard_db::stream::ResultEvent::Rows(b) = ev.unwrap() {
            n = b.cell(0, 0).to_display();
        }
    }
    n
}

#[tokio::test]
#[ignore]
async fn hypothetical_index_changes_the_plan_without_creating_it() {
    let mut s = admin("shop").await;
    let before_count = order_indexes(s.as_mut()).await;
    let sql = "select id, total from orders where total = 123.45";
    let w = what_if(
        s.as_mut(),
        Engine::Postgres,
        sql,
        &["CREATE INDEX ON orders (total);".into()],
    )
    .await
    .expect("what if");

    assert_eq!(w.indexes.len(), 1);
    assert!(w.indexes[0].bytes.unwrap_or(0.0) > 0.0);
    assert!(w.uses_hypothetical(), "plan after: {:#?}", w.after.root);
    let (b, a) = (w.before.root.cost.unwrap(), w.after.root.cost.unwrap());
    assert!(a < b / 10.0, "cost {b} -> {a}");
    assert_eq!(
        order_indexes(s.as_mut()).await,
        before_count,
        "no real index created"
    );

    // The hypothetical index is gone afterwards.
    let again = switchyard_plan::capture::capture(
        s.as_mut(),
        Engine::Postgres,
        sql,
        switchyard_plan::capture::Mode::Estimated,
    )
    .await
    .unwrap();
    assert_eq!(again.root.cost, w.before.root.cost);
}

#[tokio::test]
#[ignore]
async fn hypothetical_indexes_refuse_anything_but_create_index() {
    let mut s = admin("shop").await;
    let err = what_if(
        s.as_mut(),
        Engine::Postgres,
        "select 1",
        &["CREATE INDEX ON orders (total); DROP TABLE orders".into()],
    )
    .await
    .expect_err("refused");
    assert!(
        err.to_string().contains("exactly one CREATE INDEX"),
        "{err}"
    );
}

// ---- SQL Server (`SWITCHYARD_MSSQL_*`, `scripts/mssql-test-server.sh`) -------------------

async fn mssql_as(database: &str, user: &str, password: &str) -> Box<dyn DbSession> {
    let mut cfg = DbConfig::new(
        Engine::SqlServer,
        env("SWITCHYARD_MSSQL_HOST", "localhost"),
        database,
    );
    cfg.port = env("SWITCHYARD_MSSQL_PORT", "1433").parse().unwrap();
    cfg.user = user.to_owned();
    cfg.password = Some(SecretString::from(password.to_owned()));
    cfg.ssl_mode = SslMode::Prefer;
    cfg.trusted_ca_pem = std::env::var("SWITCHYARD_MSSQL_CA")
        .ok()
        .map(|p| std::fs::read_to_string(p).expect("CA file"));
    switchyard_db::mssql::MssqlDriver
        .connect(&cfg, None)
        .await
        .expect("connect")
}

async fn mssql_sa(database: &str) -> Box<dyn DbSession> {
    mssql_as(
        database,
        &env("SWITCHYARD_MSSQL_USER", "sa"),
        &env("SWITCHYARD_MSSQL_PASSWORD", "Switchyard!2026"),
    )
    .await
}

/// `shop` with a table that has no index on the column the tests filter by.
async fn mssql_shop() -> Box<dyn DbSession> {
    let mut master = mssql_sa("master").await;
    exec(
        master.as_mut(),
        "IF DB_ID('shop') IS NULL CREATE DATABASE shop",
    )
    .await;
    let mut s = mssql_sa("shop").await;
    exec(
        s.as_mut(),
        "IF OBJECT_ID('dbo.access_orders') IS NULL BEGIN
           CREATE TABLE dbo.access_orders (id INT IDENTITY PRIMARY KEY, customer_id INT NOT NULL,
                                           total DECIMAL(12,2) NOT NULL);
           INSERT INTO dbo.access_orders (customer_id, total)
           SELECT TOP (20000) ABS(CHECKSUM(NEWID())) % 1000 + 1, ABS(CHECKSUM(NEWID())) % 100000 / 100.0
             FROM sys.all_objects a CROSS JOIN sys.all_objects b;
         END",
    )
    .await;
    s
}

#[tokio::test]
#[ignore]
async fn sql_server_workload_with_query_store_and_dmvs() {
    let mut s = mssql_shop().await;
    exec(
        s.as_mut(),
        "ALTER DATABASE shop SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE)",
    )
    .await;
    for _ in 0..3 {
        exec(
            s.as_mut(),
            // Not a trivial plan: those never record missing indexes.
            "SELECT customer_id, SUM(total) AS t FROM dbo.access_orders \
             WHERE customer_id = 42 GROUP BY customer_id ORDER BY t",
        )
        .await;
    }
    let w = workload(s.as_mut(), Engine::SqlServer)
        .await
        .expect("workload");
    assert!(w.hints.is_empty(), "{:?}", w.hints);
    assert!(w.since_ms.is_some());
    let t = w
        .tables
        .iter()
        .find(|t| t.name == "access_orders")
        .expect("table");
    assert_eq!(t.rows, Some(20000.0));
    assert!(t.seq_scans.unwrap_or(0.0) >= 1.0, "{t:?}");
    assert!(
        w.indexes
            .iter()
            .any(|i| i.table == "access_orders" && i.primary)
    );
    assert!(
        w.missing_indexes
            .iter()
            .any(|m| m.table.contains("access_orders") && m.equality == ["[customer_id]"]),
        "{:?}",
        w.missing_indexes
    );
    assert!(
        w.statements
            .iter()
            .any(|q| q.query.contains("access_orders")),
        "Query Store lists the query"
    );
}

#[tokio::test]
#[ignore]
async fn sql_server_query_store_off_is_a_hint() {
    let mut s = mssql_shop().await;
    exec(s.as_mut(), "ALTER DATABASE shop SET QUERY_STORE = OFF").await;
    let w = workload(s.as_mut(), Engine::SqlServer)
        .await
        .expect("workload");
    exec(s.as_mut(), "ALTER DATABASE shop SET QUERY_STORE = ON").await;
    let h = w
        .hints
        .iter()
        .find(|h| h.source == Source::Statements)
        .expect("statements hint");
    assert_eq!(
        h.fix.as_deref(),
        Some("ALTER DATABASE CURRENT SET QUERY_STORE = ON;")
    );
}

#[tokio::test]
#[ignore]
async fn sql_server_missing_permissions_are_hints_with_grants() {
    let _ = mssql_shop().await;
    let mut master = mssql_sa("master").await;
    exec(
        master.as_mut(),
        "IF SUSER_ID('swy_limited') IS NULL
           CREATE LOGIN swy_limited WITH PASSWORD = 'Limited!2026', CHECK_POLICY = OFF",
    )
    .await;
    let mut shop = mssql_sa("shop").await;
    exec(
        shop.as_mut(),
        "IF USER_ID('swy_limited') IS NULL CREATE USER swy_limited FOR LOGIN swy_limited;
         GRANT SELECT ON dbo.access_orders TO swy_limited;",
    )
    .await;

    let mut s = mssql_as("shop", "swy_limited", "Limited!2026").await;
    let w = workload(s.as_mut(), Engine::SqlServer)
        .await
        .expect("workload");
    let fix = |source: Source| {
        w.hints
            .iter()
            .find(|h| h.source == source)
            .and_then(|h| h.fix.clone())
            .unwrap_or_default()
    };
    assert!(fix(Source::Indexes).contains("GRANT VIEW SERVER STATE TO [swy_limited];"));
    assert!(fix(Source::MissingIndexes).contains("GRANT VIEW SERVER STATE"));
    assert_eq!(
        fix(Source::Statements),
        "GRANT VIEW DATABASE STATE TO [swy_limited];"
    );
    // Sizes and row counts still come through without the DMVs.
    let t = w
        .tables
        .iter()
        .find(|t| t.name == "access_orders")
        .expect("table");
    assert_eq!(t.rows, Some(20000.0));
    assert_eq!(t.seq_scans, None, "usage unknown, not zero");
}
