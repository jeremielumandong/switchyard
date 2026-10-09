//! MySQL integration tests. Need a server with the sample schema (the docker `mysql`
//! service, seeded from `docker/mysql`); configure with `SWITCHYARD_MYSQL_HOST`, `_PORT`,
//! `_USER`, `_PASSWORD` and `_DATABASE`. TLS is off: the docker server's certificate is
//! self-signed.
//! Run with `cargo test -p switchyard-db --test mysql -- --ignored --test-threads 1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::batch::{BatchList, ColumnMeta};
use switchyard_db::catalog::{CatalogChunk, IntrospectScope, ObjectKind};
use switchyard_db::dialect::{Dialect, mysql::MySqlDialect};
use switchyard_db::driver::{DbConfig, DbSession, Driver, SslMode};
use switchyard_db::error::{DbError, ErrorPosition};
use switchyard_db::mysql::MySqlDriver;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::{DataType, Engine, Value};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn config() -> DbConfig {
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
    cfg
}

async fn session() -> Box<dyn DbSession> {
    MySqlDriver.connect(&config(), None).await.expect("connect")
}

struct Collected {
    sets: Vec<(Vec<ColumnMeta>, BatchList)>,
    batch_sizes: Vec<usize>,
    notices: Vec<String>,
    affected: Option<u64>,
}

async fn drain(s: &mut dyn DbSession, sql: &str, params: &[Value]) -> Result<Collected, DbError> {
    let mut stream = s.execute(sql, params).await?;
    let mut c = Collected {
        sets: Vec::new(),
        batch_sizes: Vec::new(),
        notices: Vec::new(),
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
            ResultEvent::Notice(n) => c.notices.push(n.message),
            ResultEvent::NextResultSet => {}
        }
    }
    Ok(c)
}

fn value(c: &Collected, set: usize, row: usize, col: usize) -> Value {
    let (cols, list) = &c.sets[set];
    list.cell(row, col).unwrap().to_value(cols[col].data_type)
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn connects_and_reports_version() {
    let s = session().await;
    let v = s.server_version();
    assert!(v.starts_with("MySQL ") || v.starts_with("MariaDB "), "{v}");
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn wrong_password_is_a_connect_error() {
    let mut cfg = config();
    cfg.password = Some(SecretString::from("nope".to_owned()));
    let err = MySqlDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("must fail");
    assert!(
        matches!(&err, DbError::Connect(m) if m.contains("Access denied")),
        "{err}"
    );
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn self_signed_certificate_is_refused_by_default() {
    let mut cfg = config();
    cfg.ssl_mode = SslMode::Prefer;
    let err = MySqlDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("must fail");
    assert!(matches!(err, DbError::Tls(_)), "{err}");
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn maps_every_type() {
    let mut s = session().await;
    let mariadb = s.server_version().starts_with("MariaDB");
    let sql = "SELECT CAST(1 AS UNSIGNED) = 1 AS b, CAST(-3 AS SIGNED) AS i, \
               18446744073709551615 AS ubig, 12.345 AS dec_, 1.5e0 AS dbl, \
               'héllo' AS txt, X'DEAD' AS bin, DATE '2024-02-29' AS d, \
               TIMESTAMP '2024-02-29 13:45:30.25' AS ts, TIME '-26:03:04.5' AS t, \
               JSON_OBJECT('a', 1) AS j, b'1' AS bit1, NULL AS n";
    // Text protocol (no parameters), then binary (a prepared statement).
    for params in [vec![], vec![Value::Int(1)]] {
        let sql = if params.is_empty() {
            sql.to_owned()
        } else {
            format!("{sql}, ? AS p")
        };
        let c = drain(s.as_mut(), &sql, &params).await.unwrap();
        let cols = &c.sets[0].0;
        let types: Vec<DataType> = cols.iter().map(|c| c.data_type).collect();
        assert_eq!(types[2], DataType::Numeric, "{cols:?}");
        assert_eq!(types[3], DataType::Numeric);
        assert_eq!(types[6], DataType::Bytes);
        assert_eq!(types[7], DataType::Date);
        assert_eq!(types[8], DataType::Timestamp);
        // MariaDB's JSON is a LONGTEXT alias.
        if !mariadb {
            assert_eq!(types[10], DataType::Json);
        }
        let shown: Vec<String> = (0..13).map(|i| value(&c, 0, 0, i).to_display()).collect();
        assert_eq!(
            shown,
            [
                "1",
                "-3",
                "18446744073709551615",
                "12.345",
                "1.5",
                "héllo",
                "\\xdead",
                "2024-02-29",
                "2024-02-29 13:45:30.25",
                "-26:03:04.5",
                "{\"a\": 1}",
                "\\x01",
                "NULL",
            ],
            "params: {params:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn several_result_sets_and_affected_rows() {
    let mut s = session().await;
    let c = drain(s.as_mut(), "SELECT 1 AS a; SELECT 2 AS b, 3 AS c", &[])
        .await
        .unwrap();
    assert_eq!(c.sets.len(), 2);
    assert_eq!(value(&c, 1, 0, 1), Value::Int(3));
    assert_eq!(c.affected, None);

    drain(s.as_mut(), "CREATE TEMPORARY TABLE t (x INT)", &[])
        .await
        .unwrap();
    let c = drain(s.as_mut(), "INSERT INTO t VALUES (1), (2), (3)", &[])
        .await
        .unwrap();
    assert_eq!(c.affected, Some(3));
    // A procedure returns its result sets, then a status.
    let c = drain(s.as_mut(), "CALL recent_orders('2026-01-01')", &[])
        .await
        .unwrap();
    assert_eq!(c.sets.len(), 2);
    assert_eq!(value(&c, 0, 0, 0), Value::Int(1000));
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn warnings_become_notices() {
    let mut s = session().await;
    let c = drain(s.as_mut(), "SELECT CAST('12abc' AS SIGNED) AS n", &[])
        .await
        .unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(12));
    assert!(
        c.notices.iter().any(|n| n.contains("Truncated")),
        "{:?}",
        c.notices
    );
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn streams_in_batches() {
    let mut s = session().await;
    let c = drain(s.as_mut(), "SELECT * FROM customers ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(c.sets[0].1.len(), 1000);
    assert_eq!(c.batch_sizes[0], 200);
    assert!(c.batch_sizes.len() >= 2, "{:?}", c.batch_sizes);
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn cancel_stops_sleep_quickly_and_the_session_survives() {
    let mut s = session().await;
    let cancel = s.cancel_handle();
    let mut stream = s.execute("SELECT SLEEP(30) AS slept", &[]).await.unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let started = Instant::now();
    let mut outcome = None;
    while let Some(ev) = stream.next().await {
        if let Err(e) = ev {
            outcome = Some(e);
        }
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    // SLEEP() returns 1 when interrupted instead of failing; either way it stopped.
    if let Some(e) = outcome {
        assert!(matches!(e, DbError::Cancelled), "{e}");
    }
    drop(stream);
    let c = drain(s.as_mut(), "SELECT 42", &[]).await.unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(42));
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn dropping_a_stream_midway_keeps_the_session_usable() {
    let mut s = session().await;
    let mut stream = s
        .execute(
            "SELECT a.id FROM customers a CROSS JOIN customers b LIMIT 1000000",
            &[],
        )
        .await
        .unwrap();
    let mut rows = 0;
    while let Some(ev) = stream.next().await {
        if let ResultEvent::Rows(b) = ev.unwrap() {
            rows += b.len();
            if rows >= 2000 {
                break;
            }
        }
    }
    drop(stream);
    let started = Instant::now();
    let c = drain(s.as_mut(), "SELECT 7", &[]).await.unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(7));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn errors_carry_code_and_line() {
    let mut s = session().await;
    let err = drain(s.as_mut(), "SELECT 1\nFORM customers", &[])
        .await
        .err()
        .unwrap();
    let server = err.as_server().expect("server error");
    assert_eq!(server.code.as_deref(), Some("1064"));
    assert_eq!(server.position, Some(ErrorPosition::Line(2)));
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn named_parameters_bind_positionally() {
    let mut s = session().await;
    let (sql, names) =
        MySqlDialect.bind_params("SELECT :answer + 1 AS a, :name AS n, :answer AS b");
    assert_eq!(names, [":answer", ":name", ":answer"]);
    let c = drain(
        s.as_mut(),
        &sql,
        &[Value::Int(41), Value::Text("x".into()), Value::Int(41)],
    )
    .await
    .unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(42));
    assert_eq!(value(&c, 0, 0, 1), Value::Text("x".into()));
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn rollback_discards_changes() {
    let mut s = session().await;
    s.begin().await.unwrap();
    assert!(s.in_transaction());
    drain(s.as_mut(), "DELETE FROM orders WHERE id = 1", &[])
        .await
        .unwrap();
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    let c = drain(s.as_mut(), "SELECT COUNT(*) FROM orders WHERE id = 1", &[])
        .await
        .unwrap();
    assert_eq!(value(&c, 0, 0, 0), Value::Int(1));
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn read_only_sessions_refuse_writes() {
    let mut cfg = config();
    cfg.read_only = true;
    let mut s = MySqlDriver.connect(&cfg, None).await.unwrap();
    let err = drain(
        s.as_mut(),
        "UPDATE orders SET total = total WHERE id = 1",
        &[],
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        err.as_server().and_then(|e| e.code.as_deref()),
        Some("1792")
    );
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn scripts_with_delimiters_run_statement_by_statement() {
    let mut s = session().await;
    let script = "DROP PROCEDURE IF EXISTS sy_twice;\nDELIMITER //\n\
                  CREATE PROCEDURE sy_twice(IN x INT) BEGIN SELECT x * 2 AS y; END//\n\
                  DELIMITER ;\nCALL sy_twice(21);";
    let mut last = None;
    for span in MySqlDialect.split_script(script) {
        last = Some(drain(s.as_mut(), span.text(script), &[]).await.unwrap());
    }
    assert_eq!(value(&last.unwrap(), 0, 0, 0), Value::Int(42));
    drain(s.as_mut(), "DROP PROCEDURE sy_twice", &[])
        .await
        .unwrap();
}

async fn introspect(s: &mut dyn DbSession, scope: IntrospectScope) -> CatalogChunk {
    s.introspect(scope).await.unwrap()
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn catalog_of_the_sample_schema() {
    let mut s = session().await;
    let CatalogChunk::Databases(dbs) = introspect(s.as_mut(), IntrospectScope::Databases).await
    else {
        panic!()
    };
    assert_eq!(dbs, ["shop"]);
    let CatalogChunk::Schemas(schemas) = introspect(s.as_mut(), IntrospectScope::Schemas).await
    else {
        panic!()
    };
    assert!(schemas.iter().any(|x| x.name == "shop" && !x.is_system));
    assert!(
        schemas
            .iter()
            .any(|x| x.name == "information_schema" && x.is_system)
    );

    let objects = |kind| IntrospectScope::Objects {
        schema: "shop".into(),
        kind,
    };
    let CatalogChunk::Objects(tables) = introspect(s.as_mut(), objects(ObjectKind::Table)).await
    else {
        panic!()
    };
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["customers", "orders"]);
    assert_eq!(tables[0].detail.as_deref(), Some("People who order"));
    let CatalogChunk::Objects(views) = introspect(s.as_mut(), objects(ObjectKind::View)).await
    else {
        panic!()
    };
    assert_eq!(views[0].name, "big_orders");
    let CatalogChunk::Objects(procs) = introspect(s.as_mut(), objects(ObjectKind::Procedure)).await
    else {
        panic!()
    };
    assert_eq!(procs[0].name, "recent_orders");
    assert_eq!(procs[0].detail.as_deref(), Some("(IN since date)"));

    // An empty schema means the connection's database.
    let CatalogChunk::Detail(d) = introspect(
        s.as_mut(),
        IntrospectScope::Detail {
            schema: String::new(),
            name: "orders".into(),
            kind: ObjectKind::Table,
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(d.object.schema, "shop");
    let cols: Vec<(&str, &str, bool)> = d
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.data_type.as_str(), c.is_primary_key))
        .collect();
    // MariaDB keeps the display width (`bigint(20)`).
    assert!(cols[0].0 == "id" && cols[0].1.starts_with("bigint") && cols[0].2);
    assert_eq!(cols[2], ("total", "decimal(10,2)", false));
    assert!(d.ddl.starts_with("CREATE TABLE `orders`"), "{}", d.ddl);
    let ix = d
        .indexes
        .iter()
        .find(|i| i.name == "ix_orders_placed")
        .unwrap();
    assert_eq!(ix.columns, ["placed_on", "customer_id"]);
    assert_eq!(d.foreign_keys[0].references, "shop.customers");
    assert_eq!(d.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    let kinds: Vec<&str> = d.constraints.iter().map(|c| c.kind.as_str()).collect();
    assert!(
        kinds.contains(&"PRIMARY KEY")
            && kinds.contains(&"FOREIGN KEY")
            && kinds.contains(&"CHECK")
    );

    let routine = |name: &str| IntrospectScope::RoutineDefinition {
        schema: "shop".into(),
        name: name.into(),
        kind: ObjectKind::Procedure,
        signature: None,
    };
    // The seed's procedure belongs to root: MySQL hides its body from this user
    // (MariaDB shows it).
    let CatalogChunk::Detail(p) = introspect(s.as_mut(), routine("recent_orders")).await else {
        panic!()
    };
    assert!(
        p.ddl.contains("SHOW_ROUTINE") || p.ddl.contains("PROCEDURE `recent_orders`"),
        "{}",
        p.ddl
    );
    assert_eq!(p.columns[0].name, "since");
    assert_eq!(p.columns[0].data_type, "IN date");
    drain(s.as_mut(), "DROP PROCEDURE IF EXISTS sy_mine", &[])
        .await
        .unwrap();
    drain(
        s.as_mut(),
        "CREATE PROCEDURE sy_mine(OUT n INT) BEGIN SELECT COUNT(*) INTO n FROM orders; END",
        &[],
    )
    .await
    .unwrap();
    let CatalogChunk::Detail(p) = introspect(s.as_mut(), routine("sy_mine")).await else {
        panic!()
    };
    assert!(p.ddl.starts_with("CREATE DEFINER"), "{}", p.ddl);
    drain(s.as_mut(), "DROP PROCEDURE sy_mine", &[])
        .await
        .unwrap();

    let CatalogChunk::AllColumns(all) = introspect(s.as_mut(), IntrospectScope::AllColumns).await
    else {
        panic!()
    };
    assert!(
        all.iter()
            .any(|c| c.table == "customers" && c.name == "email")
    );

    let CatalogChunk::Objects(hits) = introspect(
        s.as_mut(),
        IntrospectScope::Search {
            pattern: "ORDER".into(),
            limit: 10,
            include_system: false,
        },
    )
    .await
    else {
        panic!()
    };
    let hits: Vec<String> = hits
        .iter()
        .map(|h| format!("{:?} {}.{}", h.kind, h.schema, h.name))
        .collect();
    assert_eq!(
        hits,
        [
            "Table shop.orders",
            "View shop.big_orders",
            "Procedure shop.recent_orders"
        ]
    );
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn dependencies_both_directions() {
    let mut s = session().await;
    let mariadb = s.server_version().starts_with("MariaDB");
    let deps = |name: &str, kind| IntrospectScope::Dependencies {
        schema: "shop".into(),
        name: name.into(),
        kind,
    };
    let CatalogChunk::Dependencies(d) =
        introspect(s.as_mut(), deps("customers", ObjectKind::Table)).await
    else {
        panic!()
    };
    let used_by: Vec<String> = d
        .used_by
        .iter()
        .map(|x| format!("{} ({})", x.name, x.dependency))
        .collect();
    assert!(
        used_by.contains(&"orders (foreign key)".into()),
        "{used_by:?}"
    );
    if mariadb {
        // No VIEW_TABLE_USAGE: foreign keys only, and a hint.
        assert!(d.hint.is_some());
        return;
    }
    assert!(used_by.contains(&"big_orders (view)".into()), "{used_by:?}");
    let CatalogChunk::Dependencies(d) =
        introspect(s.as_mut(), deps("big_orders", ObjectKind::View)).await
    else {
        panic!()
    };
    let uses: Vec<&str> = d.uses.iter().map(|x| x.name.as_str()).collect();
    assert!(
        uses.contains(&"orders") && uses.contains(&"customers"),
        "{uses:?}"
    );
}

#[tokio::test]
#[ignore = "needs mysql"]
async fn users_need_a_privilege_or_show_a_hint() {
    let mut s = session().await;
    match introspect(
        s.as_mut(),
        IntrospectScope::Objects {
            schema: String::new(),
            kind: ObjectKind::Role,
        },
    )
    .await
    {
        CatalogChunk::Hint(h) => assert!(h.contains("mysql.user")),
        CatalogChunk::Objects(users) => assert!(!users.is_empty()),
        other => panic!("{other:?}"),
    }
}
