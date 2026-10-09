//! Plan capture on MySQL, MongoDB and SQLite.
//!
//! MySQL and MongoDB tests are ignored: they need the docker `mysql` and `mongo` services
//! (configured like the `switchyard-db` integration tests with `SWITCHYARD_MYSQL_*` and
//! `SWITCHYARD_MONGO_*`). The SQLite test opens a file in the temp directory.
//!
//! Run with `cargo test -p switchyard-plan --test engines -- --include-ignored --test-threads 1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::driver::{DbConfig, DbSession, Driver, SslMode};
use switchyard_db::mongo::MongoDriver;
use switchyard_db::mysql::MySqlDriver;
use switchyard_db::sqlite::SqliteDriver;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::Engine;
use switchyard_plan::capture::{Mode, capture};
use switchyard_plan::{PlanError, PlanKind, PlanSource, Rule, Thresholds, analyze};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// Run `sql` and return the first cell of the last row.
async fn scalar(s: &mut dyn DbSession, sql: &str) -> String {
    let mut stream = s.execute(sql, &[]).await.expect(sql);
    let mut last = String::new();
    while let Some(ev) = stream.next().await {
        if let ResultEvent::Rows(b) = ev.expect(sql)
            && !b.is_empty()
        {
            last = b.cell(b.len() - 1, 0).to_display();
        }
    }
    last
}

fn rules(plan: &switchyard_plan::Plan) -> Vec<Rule> {
    analyze(plan, &Thresholds::default())
        .into_iter()
        .map(|f| f.rule)
        .collect()
}

async fn mysql() -> Box<dyn DbSession> {
    let mut cfg = DbConfig::new(
        Engine::MySql,
        env("SWITCHYARD_MYSQL_HOST", "127.0.0.1"),
        env("SWITCHYARD_MYSQL_DATABASE", "shop"),
    );
    cfg.port = env("SWITCHYARD_MYSQL_PORT", "3306").parse().unwrap();
    cfg.user = env("SWITCHYARD_MYSQL_USER", "switchyard");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_MYSQL_PASSWORD",
        "switchyard",
    )));
    cfg.ssl_mode = SslMode::Disable;
    MySqlDriver.connect(&cfg, None).await.expect("connect")
}

#[tokio::test]
#[ignore = "needs the docker mysql service"]
async fn mysql_estimated_actual_and_rollback() {
    let mut s = mysql().await;
    let sql = "SELECT c.email, SUM(o.total) t FROM orders o JOIN customers c \
               ON c.id = o.customer_id WHERE o.total > 100 GROUP BY c.email \
               ORDER BY t DESC LIMIT 10";
    let est = capture(s.as_mut(), Engine::MySql, sql, Mode::Estimated)
        .await
        .unwrap();
    assert_eq!(
        (est.source, est.kind),
        (PlanSource::MySql, PlanKind::Estimated)
    );
    assert!(est.root.cost.is_some_and(|c| c > 0.0));
    assert!(
        est.nodes()
            .iter()
            .any(|n| n.operation.starts_with("Nested Loop"))
    );
    assert!(est.nodes().iter().any(|n| n.object.as_deref() == Some("o")));

    let act = capture(s.as_mut(), Engine::MySql, sql, Mode::Actual)
        .await
        .unwrap();
    assert_eq!(act.kind, PlanKind::Actual);
    assert!(act.execution_ms.is_some_and(|t| t > 0.0));
    assert!(act.nodes().iter().any(|n| n.operation == "Table scan"));
    // 798 groups sorted: tagged as a filesort, below the rule's 1,000-row threshold.
    assert!(
        act.nodes()
            .iter()
            .any(|n| n.warnings.iter().any(|w| w.contains("filesort")))
    );
    assert!(!s.in_transaction());

    // An actual plan of a write runs it inside a transaction that is rolled back.
    let before = scalar(s.as_mut(), "SELECT COUNT(*) FROM orders").await;
    let del = capture(
        s.as_mut(),
        Engine::MySql,
        "DELETE FROM orders WHERE total > 450",
        Mode::Actual,
    )
    .await;
    // MySQL 8.4 measures only what its iterator executor runs; single-table DELETE is not.
    match del {
        Ok(p) => assert_eq!(p.kind, PlanKind::Actual),
        Err(PlanError::Unsupported(m)) => assert!(m.contains("Explain"), "{m}"),
        Err(e) => panic!("{e}"),
    }
    assert!(!s.in_transaction());
    assert_eq!(
        scalar(s.as_mut(), "SELECT COUNT(*) FROM orders").await,
        before
    );
    let sum = "SELECT CAST(SUM(total) AS CHAR) FROM orders WHERE id <= 5";
    let before = scalar(s.as_mut(), sum).await;
    // Whether or not MySQL can measure it, the update is undone.
    let _ = capture(
        s.as_mut(),
        Engine::MySql,
        "UPDATE orders o JOIN customers c ON c.id = o.customer_id SET o.total = o.total + 1 \
         WHERE o.id <= 5",
        Mode::Actual,
    )
    .await;
    assert!(!s.in_transaction());
    assert_eq!(scalar(s.as_mut(), sum).await, before);

    // Inside an open transaction the plan uses a savepoint and leaves it open.
    s.begin().await.unwrap();
    capture(s.as_mut(), Engine::MySql, sql, Mode::Actual)
        .await
        .unwrap();
    assert!(s.in_transaction());
    s.rollback().await.unwrap();
}

async fn mongo() -> Box<dyn DbSession> {
    let mut cfg = DbConfig::new(
        Engine::MongoDb,
        env("SWITCHYARD_MONGO_HOST", "localhost"),
        "switchyard_plan_it",
    );
    cfg.port = env("SWITCHYARD_MONGO_PORT", "27017").parse().unwrap();
    cfg.user = env("SWITCHYARD_MONGO_USER", "switchyard");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_MONGO_PASSWORD",
        "switchyard",
    )));
    cfg.connect_timeout = Duration::from_secs(5);
    MongoDriver.connect(&cfg, None).await.expect("connect")
}

#[tokio::test]
#[ignore = "needs the docker mongo service"]
async fn mongo_estimated_and_actual() {
    let mut s = mongo().await;
    scalar(s.as_mut(), "db.items.drop()").await;
    let docs: Vec<String> = (0..12_000)
        .map(|i| format!("{{ _id: {i}, k: {}, v: {} }}", i % 100, i % 7))
        .collect();
    scalar(
        s.as_mut(),
        &format!("db.items.insertMany([{}])", docs.join(",")),
    )
    .await;
    scalar(s.as_mut(), "db.items.createIndex({ k: 1 })").await;

    let scan = "db.items.find({ v: 3 }).sort({ k: -1 })";
    let est = capture(s.as_mut(), Engine::MongoDb, scan, Mode::Estimated)
        .await
        .unwrap();
    assert_eq!(
        (est.source, est.kind),
        (PlanSource::MongoDb, PlanKind::Estimated)
    );
    assert!(
        est.nodes()
            .iter()
            .any(|n| n.operation == "COLLSCAN" || n.operation == "IXSCAN")
    );
    let act = capture(
        s.as_mut(),
        Engine::MongoDb,
        "db.items.find({ v: 3 })",
        Mode::Actual,
    )
    .await
    .unwrap();
    assert_eq!(act.kind, PlanKind::Actual);
    assert!(rules(&act).contains(&Rule::FullScan), "{:?}", act.root);

    let ix = capture(
        s.as_mut(),
        Engine::MongoDb,
        "db.items.find({ k: 5 })",
        Mode::Actual,
    )
    .await
    .unwrap();
    assert!(ix.nodes().iter().any(|n| n.operation == "IXSCAN"));
    assert!(!rules(&ix).contains(&Rule::FullScan));

    let agg = capture(
        s.as_mut(),
        Engine::MongoDb,
        "db.items.aggregate([{ $match: { v: 1 } }, { $group: { _id: '$k', n: { $sum: 1 } } }])",
        Mode::Actual,
    )
    .await
    .unwrap();
    assert!(agg.nodes().len() >= 2);

    // Writes and statements that are not find / aggregate are refused.
    for (sql, mode) in [
        ("db.items.aggregate([{ $out: 'copy' }])", Mode::Actual),
        ("db.items.countDocuments({})", Mode::Estimated),
        ("db.items.find().explain()", Mode::Estimated),
    ] {
        let e = capture(s.as_mut(), Engine::MongoDb, sql, mode)
            .await
            .unwrap_err();
        assert!(matches!(e, PlanError::Unsupported(_)), "{sql}: {e}");
    }
    assert_eq!(
        scalar(s.as_mut(), "db.items.countDocuments({})").await,
        "12000"
    );
    scalar(s.as_mut(), "db.items.drop()").await;
}

#[tokio::test]
async fn sqlite_estimated_only() {
    let path = std::env::temp_dir().join(format!("swy-plan-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = DbConfig::new(Engine::Sqlite, "", path.to_string_lossy().into_owned());
    let mut s = SqliteDriver.connect(&cfg, None).await.expect("open");
    scalar(
        s.as_mut(),
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, v TEXT)",
    )
    .await;
    scalar(s.as_mut(), "CREATE INDEX ix_t_k ON t (k)").await;
    let scan = capture(
        s.as_mut(),
        Engine::Sqlite,
        "SELECT * FROM t WHERE v LIKE 'x%' ORDER BY v;",
        Mode::Estimated,
    )
    .await
    .unwrap();
    assert_eq!(
        (scan.source, scan.kind),
        (PlanSource::Sqlite, PlanKind::Estimated)
    );
    let found = rules(&scan);
    assert!(found.contains(&Rule::FullScan), "{found:?}");
    assert!(found.contains(&Rule::TempStructure), "{found:?}");
    let seek = capture(
        s.as_mut(),
        Engine::Sqlite,
        "SELECT * FROM t WHERE k = 3",
        Mode::Estimated,
    )
    .await
    .unwrap();
    assert_eq!(seek.root.operation, "Index Search");
    assert_eq!(seek.root.object.as_deref(), Some("t.ix_t_k"));
    let e = capture(s.as_mut(), Engine::Sqlite, "SELECT 1", Mode::Actual)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("Explain"), "{e}");
    drop(s);
    let _ = std::fs::remove_file(&path);
}
