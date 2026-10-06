//! Plan capture against live servers (ignored; they need the docker services or the same
//! seed). PostgreSQL uses `SWITCHYARD_PG_*`, SQL Server `SWITCHYARD_MSSQL_*` like the
//! `switchyard-db` integration tests.
//!
//! With `SWITCHYARD_WRITE_FIXTURES=1` the SQL Server test also writes the showplans it
//! captures to `tests/fixtures/mssql/` for the snapshot tests.
//!
//! Run with `cargo test -p switchyard-plan --test capture -- --ignored --test-threads 1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::dialect::Dialect;
use switchyard_db::dialect::tsql::TSqlDialect;
use switchyard_db::driver::{DbConfig, DbSession, Driver, SslMode};
use switchyard_db::mssql::MssqlDriver;
use switchyard_db::pg::PgDriver;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::Engine;
use switchyard_plan::capture::{Mode, capture};
use switchyard_plan::{PlanKind, Rule, Thresholds, analyze};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

async fn pg() -> Box<dyn DbSession> {
    let mut cfg = DbConfig::new(
        Engine::Postgres,
        env("SWITCHYARD_PG_HOST", "127.0.0.1"),
        env("SWITCHYARD_PG_DB", "shop"),
    );
    cfg.port = env("SWITCHYARD_PG_PORT", "5432").parse().unwrap();
    cfg.user = env("SWITCHYARD_PG_USER", "switchyard");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_PG_PASSWORD",
        "switchyard",
    )));
    cfg.ssl_mode = SslMode::Disable;
    PgDriver.connect(&cfg, None).await.expect("connect")
}

async fn mssql(database: &str) -> Box<dyn DbSession> {
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
    MssqlDriver.connect(&cfg, None).await.expect("connect")
}

/// Run `sql` and return the first cell of the last result set.
async fn scalar(s: &mut dyn DbSession, sql: &str) -> String {
    let mut stream = s.execute(sql, &[]).await.expect(sql);
    let mut last = String::new();
    while let Some(ev) = stream.next().await {
        if let ResultEvent::Rows(b) = ev.expect(sql)
            && !b.is_empty()
        {
            last = b.cell(0, 0).to_display();
        }
    }
    last
}

#[tokio::test]
#[ignore = "needs PostgreSQL with the seed schema"]
async fn postgres_estimated_actual_and_rollback() {
    let mut s = pg().await;
    let sql = "select c.segment, count(*) from orders o join customers c on c.id = o.customer_id \
               where o.total > 990 group by c.segment;";
    let est = capture(s.as_mut(), Engine::Postgres, sql, Mode::Estimated)
        .await
        .unwrap();
    assert_eq!(est.kind, PlanKind::Estimated);
    assert!(est.nodes().iter().all(|n| n.actual_rows.is_none()));
    let act = capture(s.as_mut(), Engine::Postgres, sql, Mode::Actual)
        .await
        .unwrap();
    assert_eq!(act.kind, PlanKind::Actual);
    assert!(act.execution_ms.is_some() && act.root.actual_rows.is_some());
    assert!(
        !s.in_transaction(),
        "the capture's own transaction is closed"
    );

    // An actual plan of a DELETE runs it, then rolls it back.
    let count = "select count(*) from abandoned_carts";
    let before = scalar(s.as_mut(), count).await;
    assert_ne!(before, "0");
    let del = capture(
        s.as_mut(),
        Engine::Postgres,
        "delete from abandoned_carts",
        Mode::Actual,
    )
    .await
    .unwrap();
    assert!(
        del.root.operation.contains("Delete"),
        "{}",
        del.root.operation
    );
    assert!(del.root.children[0].actual_rows.is_some_and(|r| r > 0.0));
    assert_eq!(scalar(s.as_mut(), count).await, before);

    // Inside the user's own transaction: a savepoint, and the transaction stays open.
    s.begin().await.unwrap();
    capture(
        s.as_mut(),
        Engine::Postgres,
        "update orders set total = 0 where id < 100",
        Mode::Actual,
    )
    .await
    .unwrap();
    assert!(s.in_transaction());
    assert_ne!(
        scalar(
            s.as_mut(),
            "select min(total)::text from orders where id < 100"
        )
        .await,
        "0.00"
    );
    s.rollback().await.unwrap();

    // A failing statement leaves no transaction behind.
    assert!(
        capture(
            s.as_mut(),
            Engine::Postgres,
            "select * from nope",
            Mode::Actual
        )
        .await
        .is_err()
    );
    assert!(!s.in_transaction());
    assert_eq!(scalar(s.as_mut(), "select 1").await, "1");
}

/// A table big enough for interesting plans, created once.
async fn mssql_seed() -> Box<dyn DbSession> {
    let mut master = mssql("master").await;
    let seed = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker/mssql/seed.sql"
    ))
    .unwrap();
    for span in TSqlDialect.split_script(&seed) {
        scalar(master.as_mut(), span.text(&seed)).await;
    }
    let mut s = mssql("shop").await;
    scalar(
        s.as_mut(),
        "IF OBJECT_ID('dbo.plan_orders') IS NULL BEGIN
           CREATE TABLE dbo.plan_orders (
             id INT IDENTITY PRIMARY KEY, customer_id INT NOT NULL,
             status VARCHAR(20) NOT NULL, total DECIMAL(12,2) NOT NULL, note NVARCHAR(100) NULL);
           INSERT INTO dbo.plan_orders (customer_id, status, total, note)
           SELECT TOP (50000) ABS(CHECKSUM(NEWID())) % 1000 + 1,
                  CASE ABS(CHECKSUM(NEWID())) % 4 WHEN 0 THEN 'new' WHEN 1 THEN 'paid'
                       WHEN 2 THEN 'shipped' ELSE 'cancelled' END,
                  ABS(CHECKSUM(NEWID())) % 100000 / 100.0, N'x'
           FROM sys.all_objects a CROSS JOIN sys.all_objects b;
           CREATE INDEX ix_plan_orders_customer ON dbo.plan_orders (customer_id);
         END",
    )
    .await;
    s
}

#[tokio::test]
#[ignore = "needs SQL Server"]
async fn sql_server_estimated_actual_and_rollback() {
    let mut s = mssql_seed().await;
    let cases = [
        (
            "scan_filter",
            "SELECT * FROM dbo.plan_orders WHERE status = 'cancelled' AND total > 900",
        ),
        (
            "key_lookup",
            "SELECT id, status, total FROM dbo.plan_orders WHERE customer_id = 42",
        ),
        (
            "hash_join",
            "SELECT c.segment, COUNT(*) FROM dbo.plan_orders o JOIN dbo.customers c ON c.id = o.customer_id \
             WHERE o.total > 500 GROUP BY c.segment",
        ),
        (
            // A join forces full optimization; trivial plans never report missing indexes.
            "missing_index",
            "SELECT o.id, o.total, c.email FROM dbo.plan_orders o JOIN dbo.customers c ON c.id = o.customer_id \
             WHERE o.total = 123.45",
        ),
        (
            "sort",
            "SELECT TOP (100) * FROM dbo.plan_orders ORDER BY total DESC",
        ),
    ];
    let write = std::env::var("SWITCHYARD_WRITE_FIXTURES").is_ok_and(|v| v == "1");
    for (name, sql) in cases {
        if write {
            for (mode, suffix) in [(Mode::Estimated, "est"), (Mode::Actual, "act")] {
                let xml = raw_showplan(s.as_mut(), sql, mode).await;
                std::fs::write(
                    format!(
                        "{}/tests/fixtures/mssql/{name}_{suffix}.xml",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    xml,
                )
                .unwrap();
            }
        }
        let est = capture(s.as_mut(), Engine::SqlServer, sql, Mode::Estimated)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(est.kind, PlanKind::Estimated, "{name}");
        let act = capture(s.as_mut(), Engine::SqlServer, sql, Mode::Actual)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(act.kind, PlanKind::Actual, "{name}");
        assert!(act.root.actual_rows.is_some(), "{name}");
    }
    // The point lookup through a non-covering index needs a Key Lookup per row.
    let lookup = capture(s.as_mut(), Engine::SqlServer, cases[1].1, Mode::Actual)
        .await
        .unwrap();
    assert!(
        lookup.nodes().iter().any(|n| n.operation == "Key Lookup"),
        "{:#?}",
        lookup
            .nodes()
            .iter()
            .map(|n| &n.operation)
            .collect::<Vec<_>>()
    );
    let _ = analyze(&lookup, &Thresholds::default());
    // A filter on an unindexed column: SQL Server reports the missing index.
    let scan = capture(s.as_mut(), Engine::SqlServer, cases[3].1, Mode::Estimated)
        .await
        .unwrap();
    assert!(
        analyze(&scan, &Thresholds::default())
            .iter()
            .any(|f| f.rule == Rule::MissingIndex),
        "{:?}",
        scan.missing_indexes
    );

    // An actual plan of a DELETE runs it, then rolls it back.
    let count = "SELECT COUNT(*) FROM dbo.plan_orders WHERE status = 'cancelled'";
    let before = scalar(s.as_mut(), count).await;
    capture(
        s.as_mut(),
        Engine::SqlServer,
        "DELETE FROM dbo.plan_orders WHERE status = 'cancelled'",
        Mode::Actual,
    )
    .await
    .unwrap();
    assert_eq!(scalar(s.as_mut(), count).await, before);
    assert!(!s.in_transaction());
    // Showplan settings are off again: a plain query returns plain rows.
    assert_eq!(scalar(s.as_mut(), "SELECT 41 + 1").await, "42");
}

/// The showplan XML exactly as SQL Server returned it (fixtures).
async fn raw_showplan(s: &mut dyn DbSession, sql: &str, mode: Mode) -> String {
    let (on, off) = match mode {
        Mode::Estimated => ("SET SHOWPLAN_XML ON", "SET SHOWPLAN_XML OFF"),
        Mode::Actual => ("SET STATISTICS XML ON", "SET STATISTICS XML OFF"),
    };
    scalar(s, on).await;
    if mode == Mode::Actual {
        s.begin().await.unwrap();
    }
    let mut stream = s.execute(sql, &[]).await.unwrap();
    let mut xml = String::new();
    let mut col = None;
    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            ResultEvent::Columns(c) => {
                col = c.iter().position(|m| m.name.contains("Showplan"));
            }
            ResultEvent::Rows(b) => {
                if let Some(c) = col
                    && !b.is_empty()
                {
                    xml = b.cell(0, c).to_display();
                }
            }
            _ => {}
        }
    }
    drop(stream);
    if mode == Mode::Actual {
        s.rollback().await.unwrap();
    }
    scalar(s, off).await;
    xml
}
