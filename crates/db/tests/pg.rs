//! PostgreSQL integration tests. Need the docker services (`docker/compose.yml`) or any
//! server with the seed schema; override with `SWITCHYARD_PG_HOST`, `_PORT`, `_USER`,
//! `_PASSWORD`, `_DB`. Run with `cargo test -p switchyard-db -- --ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::pg::PgDriver;
use switchyard_db::{
    CatalogChunk, CellRef, DbConfig, DbError, DbSession, Driver, Engine, IntrospectScope,
    ObjectKind, ResultEvent, RowBatch, SslMode, Value,
};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn config() -> DbConfig {
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
    cfg
}

async fn session() -> Box<dyn DbSession> {
    PgDriver.connect(&config(), None).await.expect("connect")
}

struct Collected {
    columns: Vec<String>,
    batches: Vec<RowBatch>,
    affected: Option<u64>,
    notices: Vec<String>,
}

async fn run(
    s: &mut Box<dyn DbSession>,
    sql: &str,
    params: &[Value],
) -> Result<Collected, DbError> {
    let mut stream = s.execute(sql, params).await?;
    let mut out = Collected {
        columns: vec![],
        batches: vec![],
        affected: None,
        notices: vec![],
    };
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(c) => out.columns = c.iter().map(|c| c.name.clone()).collect(),
            ResultEvent::Rows(b) => out.batches.push(b),
            ResultEvent::Notice(n) => out.notices.push(n.message),
            ResultEvent::NextResultSet => {}
            ResultEvent::Done(d) => out.affected = d.affected,
        }
    }
    Ok(out)
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn type_mapping() {
    let mut s = session().await;
    let r = run(&mut s, "SELECT * FROM type_samples ORDER BY id", &[])
        .await
        .unwrap();
    let b = &r.batches[0];
    let col = |name: &str| r.columns.iter().position(|c| c == name).unwrap();
    let shown = |row, name: &str| b.cell(row, col(name)).to_display();
    assert_eq!(b.cell(0, col("b")), CellRef::Bool(true));
    assert_eq!(b.cell(0, col("i2")), CellRef::Int(-2));
    assert_eq!(b.cell(0, col("i4")), CellRef::Int(40000));
    assert_eq!(b.cell(0, col("i8")), CellRef::Int(9_000_000_000));
    assert_eq!(b.cell(0, col("f4")), CellRef::Float(1.5));
    assert_eq!(b.cell(0, col("f8")), CellRef::Float(2.25));
    assert_eq!(shown(0, "n"), "4812.4000");
    assert_eq!(shown(0, "t"), "text");
    assert_eq!(shown(0, "vc"), "varchar");
    assert_eq!(shown(0, "c"), "ch  ");
    assert_eq!(shown(0, "by"), "\\xdeadbeef");
    assert_eq!(shown(0, "u"), "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(shown(0, "j"), "{\"a\": 1}");
    assert_eq!(shown(0, "jb"), "{\"b\": [1, 2]}");
    assert_eq!(shown(0, "d"), "2026-10-05");
    assert_eq!(shown(0, "tm"), "08:14:22.5");
    assert_eq!(shown(0, "ts"), "2026-10-05 08:14:22");
    assert_eq!(shown(0, "tstz"), "2026-10-05 08:14:22+00");
    assert_eq!(shown(0, "iv"), "1 year 2 mons 3 days 04:05:06");
    assert_eq!(shown(0, "arr"), "{1,NULL,3}");
    assert_eq!(shown(0, "st"), "paid");
    assert_eq!(shown(0, "ip"), "10.0.4.12");
    // NULL and empty string stay distinct.
    assert!(b.cell(1, col("b")).is_null());
    assert_eq!(b.cell(1, col("t")), CellRef::Text(""));
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn parameters_bind_from_text() {
    let mut s = session().await;
    let r = run(
        &mut s,
        "SELECT $1::int8 + 1 AS a, $2::numeric * 2 AS b, $3::date AS c, $4::text AS d",
        &[
            Value::Text("41".into()),
            Value::Text("1.25".into()),
            Value::Text("2026-10-05".into()),
            Value::Null,
        ],
    )
    .await
    .unwrap();
    let b = &r.batches[0];
    assert_eq!(b.cell(0, 0), CellRef::Int(42));
    assert_eq!(b.cell(0, 1).to_display(), "2.50");
    assert_eq!(b.cell(0, 2).to_display(), "2026-10-05");
    assert!(b.cell(0, 3).is_null());
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn streams_a_million_rows_in_batches() {
    let mut s = session().await;
    let started = Instant::now();
    let mut stream = s
        .execute("SELECT id, customer_id, total FROM orders", &[])
        .await
        .unwrap();
    let mut rows = 0usize;
    let mut batches = 0usize;
    let mut max_batch_bytes = 0usize;
    let mut first_rows = None;
    while let Some(ev) = stream.next().await {
        if let ResultEvent::Rows(b) = ev.unwrap() {
            first_rows.get_or_insert_with(|| started.elapsed());
            rows += b.len();
            batches += 1;
            max_batch_bytes = max_batch_bytes.max(b.heap_bytes());
            assert!(b.len() <= 1000);
            // Dropping the batch here keeps memory bounded by the batch size.
        }
    }
    assert_eq!(rows, 1_000_000);
    assert!(batches >= 1000);
    assert!(
        max_batch_bytes < 512 * 1024,
        "batch too large: {max_batch_bytes}"
    );
    eprintln!(
        "1M rows in {:?}, first batch after {:?}",
        started.elapsed(),
        first_rows.unwrap()
    );
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn cancel_pg_sleep() {
    let mut s = session().await;
    let handle = s.cancel_handle();
    let started = Instant::now();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel().await.unwrap();
    });
    let r = run(&mut s, "SELECT pg_sleep(30)", &[]).await;
    assert!(matches!(r, Err(DbError::Cancelled)), "{:?}", r.err());
    assert!(started.elapsed() < Duration::from_secs(1));
    // The session is still usable.
    let r = run(&mut s, "SELECT 1", &[]).await.unwrap();
    assert_eq!(r.batches[0].cell(0, 0), CellRef::Int(1));
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn rollback_discards_changes() {
    let mut s = session().await;
    let count = |r: Collected| match r.batches[0].cell(0, 0) {
        CellRef::Int(n) => n,
        other => panic!("{other:?}"),
    };
    let before = count(
        run(&mut s, "SELECT count(*) FROM abandoned_carts", &[])
            .await
            .unwrap(),
    );
    s.begin().await.unwrap();
    assert!(s.in_transaction());
    let r = run(&mut s, "DELETE FROM abandoned_carts WHERE id <= 10", &[])
        .await
        .unwrap();
    assert_eq!(r.affected, Some(10));
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    let after = count(
        run(&mut s, "SELECT count(*) FROM abandoned_carts", &[])
            .await
            .unwrap(),
    );
    assert_eq!(before, after);
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn notices_and_multi_statement() {
    let mut s = session().await;
    let r = run(
        &mut s,
        "DO $$ BEGIN RAISE NOTICE 'hello from plpgsql'; END $$",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(r.notices, ["hello from plpgsql"]);
    let r = run(&mut s, "SELECT 1 AS a; SELECT 2 AS b, 3 AS c", &[])
        .await
        .unwrap();
    assert_eq!(r.columns, ["b", "c"]);
    assert_eq!(r.batches.len(), 2);
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn server_error_has_position() {
    let mut s = session().await;
    let r = run(
        &mut s,
        "SELECT c.id\nFROM customers c\nJOIN orders o ON o.customer_ud = c.id",
        &[],
    )
    .await;
    let Err(DbError::Server(e)) = r else {
        panic!("expected server error")
    };
    assert_eq!(e.code.as_deref(), Some("42703"));
    assert!(e.position.is_some());
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn introspection_snapshots() {
    let mut s = session().await;
    let CatalogChunk::Schemas(schemas) = s.introspect(IntrospectScope::Schemas).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(schemas[0].name, "public");
    let mut snap = Vec::new();
    for kind in [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Function,
        ObjectKind::Sequence,
        ObjectKind::Type,
    ] {
        let CatalogChunk::Objects(objs) = s
            .introspect(IntrospectScope::Objects {
                schema: "public".into(),
                kind,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        for o in objs {
            snap.push(format!(
                "{:?} {}{}",
                kind,
                o.name,
                o.detail.unwrap_or_default()
            ));
        }
    }
    insta::assert_snapshot!("pg_public_objects", snap.join("\n"));
    let CatalogChunk::Detail(detail) = s
        .introspect(IntrospectScope::Detail {
            schema: "public".into(),
            name: "orders".into(),
            kind: ObjectKind::Table,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    insta::assert_snapshot!("pg_orders_ddl", detail.ddl);
    assert_eq!(detail.foreign_keys.len(), 1);
    assert!(
        detail
            .columns
            .iter()
            .any(|c| c.name == "id" && c.is_primary_key)
    );
    // DBX-2b extras: size, FK actions and index methods.
    assert!(detail.size_bytes.is_some_and(|b| b > 0));
    assert!(detail.foreign_keys[0].on_delete.is_some());
    assert!(detail.foreign_keys[0].on_update.is_some());
    assert!(
        detail
            .indexes
            .iter()
            .all(|i| i.method.as_deref() == Some("btree"))
    );
}

/// DBX-2b: comments, row estimate and trigger definitions, on a scratch schema so the
/// seed objects (and their snapshots) stay as they are.
#[tokio::test]
#[ignore = "needs docker postgres"]
async fn object_properties_detail() {
    let mut s = session().await;
    for sql in [
        "DROP SCHEMA IF EXISTS dbx2b CASCADE",
        "CREATE SCHEMA dbx2b",
        "CREATE TABLE dbx2b.items (id int PRIMARY KEY, label text DEFAULT 'x')",
        "COMMENT ON TABLE dbx2b.items IS 'Things we sell'",
        "COMMENT ON COLUMN dbx2b.items.label IS 'Shown to customers'",
        "INSERT INTO dbx2b.items SELECT g, 'n' || g FROM generate_series(1, 50) g",
        "ANALYZE dbx2b.items",
        "CREATE FUNCTION dbx2b.touch() RETURNS trigger LANGUAGE plpgsql AS          'BEGIN RETURN NEW; END'",
        "CREATE TRIGGER items_touch BEFORE INSERT OR UPDATE ON dbx2b.items          FOR EACH ROW EXECUTE FUNCTION dbx2b.touch()",
    ] {
        run(&mut s, sql, &[]).await.unwrap();
    }
    let CatalogChunk::Detail(d) = s
        .introspect(IntrospectScope::Detail {
            schema: "dbx2b".into(),
            name: "items".into(),
            kind: ObjectKind::Table,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    run(&mut s, "DROP SCHEMA dbx2b CASCADE", &[]).await.unwrap();
    assert_eq!(d.comment.as_deref(), Some("Things we sell"));
    assert_eq!(d.object.estimated_rows, Some(50));
    assert!(d.size_bytes.is_some_and(|b| b > 0));
    let label = d.columns.iter().find(|c| c.name == "label").unwrap();
    assert_eq!(label.comment.as_deref(), Some("Shown to customers"));
    assert_eq!(label.default.as_deref(), Some("'x'::text"));
    assert_eq!(d.triggers, ["items_touch"]);
    let t = &d.trigger_details[0];
    assert_eq!(t.timing, "BEFORE FOR EACH ROW");
    assert_eq!(t.event, "INSERT OR UPDATE");
    assert!(
        t.definition
            .starts_with("CREATE TRIGGER items_touch BEFORE INSERT OR UPDATE")
    );
}

/// `RoutineDefinition` of a routine: (ddl, [(param, type)]).
async fn routine(
    s: &mut Box<dyn DbSession>,
    name: &str,
    kind: ObjectKind,
    signature: Option<&str>,
) -> (String, Vec<(String, String)>) {
    let chunk = s
        .introspect(IntrospectScope::RoutineDefinition {
            schema: "public".into(),
            name: name.into(),
            kind,
            signature: signature.map(str::to_owned),
        })
        .await
        .expect("routine definition");
    let CatalogChunk::Detail(d) = chunk else {
        panic!("routine definition answers with a Detail");
    };
    let params = d
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.data_type.clone()))
        .collect();
    (d.ddl, params)
}

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn routine_definitions() {
    let mut s = session().await;
    let (ddl, params) = routine(
        &mut s,
        "customer_revenue",
        ObjectKind::Function,
        Some("(cid bigint)"),
    )
    .await;
    assert!(
        ddl.starts_with("CREATE OR REPLACE FUNCTION public.customer_revenue(cid bigint)"),
        "{ddl}"
    );
    assert_eq!(params, [("cid".to_owned(), "bigint".to_owned())]);
    // Overloads, OUT parameters and a procedure, inside a transaction rolled back.
    s.begin().await.unwrap();
    for sql in [
        "CREATE FUNCTION swy_over(a int) RETURNS int LANGUAGE sql AS 'SELECT a'",
        "CREATE FUNCTION swy_over(a text, OUT n int, b numeric(10,2)) LANGUAGE sql \
         AS 'SELECT 1'",
        "CREATE PROCEDURE swy_proc() LANGUAGE sql AS 'SELECT 1'",
    ] {
        run(&mut s, sql, &[]).await.unwrap();
    }
    let (_, params) = routine(
        &mut s,
        "swy_over",
        ObjectKind::Function,
        // As the tree lists it (`pg_get_function_identity_arguments`).
        Some("(a text, OUT n integer, b numeric)"),
    )
    .await;
    assert_eq!(
        params,
        [
            ("a".to_owned(), "text".to_owned()),
            ("b".to_owned(), "numeric".to_owned())
        ]
    );
    let (ddl, params) = routine(&mut s, "swy_proc", ObjectKind::Procedure, None).await;
    assert!(ddl.contains("PROCEDURE public.swy_proc()"), "{ddl}");
    assert!(params.is_empty());
    let missing = s
        .introspect(IntrospectScope::RoutineDefinition {
            schema: "public".into(),
            name: "swy_none".into(),
            kind: ObjectKind::Function,
            signature: None,
        })
        .await;
    assert!(missing.is_err());
    s.rollback().await.unwrap();
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
#[ignore = "needs docker postgres"]
async fn global_object_search() {
    let mut s = session().await;
    // Case-insensitive, across kinds, shortest names first.
    let hits = search(s.as_mut(), "REVENUE").await;
    assert_eq!(
        hits,
        [
            "public.daily_revenue MaterializedView",
            "public.customer_revenue Function"
        ]
    );
    let hits = search(s.as_mut(), "order").await;
    assert_eq!(hits[0], "public.orders Table");
    assert!(hits.contains(&"public.order_items Table".to_owned()));
    assert!(hits.contains(&"public.orders_id_seq Sequence".to_owned()));
    // `_` is literal: "e_s" matches type_samples, not customers or orders.
    assert_eq!(
        search(s.as_mut(), "e_s").await,
        ["public.type_samples Table"]
    );
    assert!(search(s.as_mut(), "100%").await.is_empty());
    // System schemas are left out by default.
    assert!(search(s.as_mut(), "pg_class").await.is_empty());
}

/// Rows of `sql` as text, one string per row (`a|b|NULL`).
async fn rows_text(s: &mut Box<dyn DbSession>, sql: &str) -> Vec<String> {
    let r = run(s, sql, &[]).await.unwrap();
    let mut out = Vec::new();
    for b in &r.batches {
        for row in 0..b.len() {
            let cells: Vec<String> = (0..r.columns.len())
                .map(|c| match b.cell(row, c) {
                    CellRef::Null => "NULL".to_owned(),
                    cell => cell.to_display(),
                })
                .collect();
            out.push(cells.join("|"));
        }
    }
    out
}

/// Applies `statements` in one transaction like `ApplyEdits`: each must change one row.
async fn apply(s: &mut Box<dyn DbSession>, statements: &[String], commit: bool) {
    s.begin().await.unwrap();
    for sql in statements {
        let r = run(s, sql, &[]).await.unwrap();
        assert_eq!(r.affected, Some(1), "{sql}");
    }
    if commit {
        s.commit().await.unwrap();
    } else {
        s.rollback().await.unwrap();
    }
}

/// DBX-3b: insert (defaults), duplicate (minus key and serial) and delete, rolled back
/// and then committed.
#[tokio::test]
#[ignore = "needs docker postgres"]
async fn row_insert_delete_duplicate() {
    use switchyard_db::dialect::postgres::PostgresDialect;
    use switchyard_db::edit::{
        EditTable, RowDelete, RowInsert, delete_statements, duplicate_values, generated_columns,
        insert_statements,
    };
    let mut s = session().await;
    for sql in [
        "DROP SCHEMA IF EXISTS dbx3b CASCADE",
        "CREATE SCHEMA dbx3b",
        "CREATE TABLE dbx3b.lines (id serial PRIMARY KEY, sku text NOT NULL, \
         qty int NOT NULL DEFAULT 1, note text)",
        "INSERT INTO dbx3b.lines (sku, qty, note) VALUES ('A-1', 5, 'gift')",
    ] {
        run(&mut s, sql, &[]).await.unwrap();
    }
    let CatalogChunk::Detail(d) = s
        .introspect(IntrospectScope::Detail {
            schema: "dbx3b".into(),
            name: "lines".into(),
            kind: ObjectKind::Table,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let generated = generated_columns(&d);
    assert_eq!(generated, ["id"]);
    let pk: Vec<String> = d
        .columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(pk, ["id"]);
    let columns: Vec<String> = ["id", "sku", "qty", "note"].map(String::from).to_vec();
    let source = vec![
        ("id".to_owned(), Value::Int(1)),
        ("sku".to_owned(), Value::Text("A-1".into())),
        ("qty".to_owned(), Value::Int(5)),
        ("note".to_owned(), Value::Text("gift".into())),
    ];
    let t = EditTable {
        schema: Some("dbx3b".into()),
        table: "lines".into(),
    };
    let dialect = PostgresDialect;
    let mut statements = delete_statements(
        &dialect,
        &t,
        &[RowDelete {
            key: vec![("id".into(), Value::Int(1))],
        }],
    );
    statements.extend(insert_statements(
        &dialect,
        &t,
        &columns,
        &generated,
        &[
            RowInsert {
                values: vec![("sku".into(), Value::Text("B-2".into()))],
            },
            RowInsert {
                values: duplicate_values(&source, &pk, &generated),
            },
        ],
    ));
    let all = "SELECT id, sku, qty, note FROM dbx3b.lines ORDER BY id";

    apply(&mut s, &statements, false).await;
    assert_eq!(rows_text(&mut s, all).await, ["1|A-1|5|gift"]);

    apply(&mut s, &statements, true).await;
    let rows = rows_text(&mut s, all).await;
    run(&mut s, "DROP SCHEMA dbx3b CASCADE", &[]).await.unwrap();
    // The rolled-back inserts used ids 2 and 3 of the sequence.
    assert_eq!(rows, ["4|B-2|1|NULL", "5|A-1|5|gift"]);
}

/// DBX-3a / 3c: a filtered, sorted page and a key filter run on the server.
#[tokio::test]
#[ignore = "needs docker postgres"]
async fn select_page_filters_sorts_and_pages() {
    use switchyard_db::Dialect;
    use switchyard_db::dialect::SortKey;
    use switchyard_db::dialect::postgres::PostgresDialect;
    use switchyard_db::edit::{key_condition, page_order, validate_where};
    let mut s = session().await;
    for sql in [
        "DROP SCHEMA IF EXISTS dbx3a CASCADE",
        "CREATE SCHEMA dbx3a",
        "CREATE TABLE dbx3a.items (id int PRIMARY KEY, grp text, n int)",
        "INSERT INTO dbx3a.items SELECT g, CASE WHEN g % 2 = 0 THEN 'even' ELSE 'odd' END, \
         g * 10 FROM generate_series(1, 25) g",
    ] {
        run(&mut s, sql, &[]).await.unwrap();
    }
    let d = PostgresDialect;
    let q = d.qualified("dbx3a", "items");
    let cond = "grp = 'even'";
    validate_where(&d, cond).unwrap();
    let order = page_order(&[], &["id".into()]);
    let page2 = d.select_page(&q, &[], Some(cond), &order, 5, 5);
    let ids: Vec<String> = rows_text(&mut s, &page2)
        .await
        .into_iter()
        .map(|r| r.split('|').next().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(ids, ["12", "14", "16", "18", "20"]);
    let desc = [SortKey {
        column: "n".into(),
        descending: true,
    }];
    let top = d.select_page(&q, &["id".into()], None, &page_order(&desc, &[]), 2, 0);
    assert_eq!(rows_text(&mut s, &top).await, ["25", "24"]);
    let key = key_condition(
        &d,
        &[
            ("id".into(), Value::Int(7)),
            ("grp".into(), Value::Text("odd".into())),
        ],
    );
    let one = d.select_page(&q, &["n".into()], Some(&key), &[], 10, 0);
    let rows = rows_text(&mut s, &one).await;
    run(&mut s, "DROP SCHEMA dbx3a CASCADE", &[]).await.unwrap();
    assert_eq!(rows, ["70"]);
}

/// `direction kind schema.name (link)` lines of a dependency answer.
async fn dependencies(
    s: &mut dyn DbSession,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Vec<String> {
    let chunk = s
        .introspect(IntrospectScope::Dependencies {
            schema: schema.into(),
            name: name.into(),
            kind,
        })
        .await
        .expect("dependencies");
    let CatalogChunk::Dependencies(d) = chunk else {
        panic!("dependencies answer with Dependencies");
    };
    assert_eq!(d.hint, None);
    let line = |dir: &str, x: &switchyard_db::DependencyInfo| {
        format!(
            "{dir} {:?} {}.{} ({})",
            x.kind, x.schema, x.name, x.dependency
        )
    };
    d.uses
        .iter()
        .map(|x| line("uses", x))
        .chain(d.used_by.iter().map(|x| line("used_by", x)))
        .collect()
}

/// DBX-5a: a view on a table, a foreign key, a serial column's sequence and a trigger
/// function, in both directions, on a scratch schema.
#[tokio::test]
#[ignore = "needs docker postgres"]
async fn dependencies_both_directions() {
    let mut s = session().await;
    for sql in [
        "DROP SCHEMA IF EXISTS dbx5a CASCADE",
        "CREATE SCHEMA dbx5a",
        "CREATE TABLE dbx5a.parent (id int PRIMARY KEY)",
        "CREATE TABLE dbx5a.child (id serial PRIMARY KEY, \
         parent_id int CONSTRAINT child_parent_fk REFERENCES dbx5a.parent (id))",
        "CREATE VIEW dbx5a.parent_v AS SELECT p.id FROM dbx5a.parent p",
        "CREATE FUNCTION dbx5a.touch() RETURNS trigger LANGUAGE plpgsql \
         AS 'BEGIN RETURN NEW; END'",
        "CREATE TRIGGER child_touch BEFORE INSERT ON dbx5a.child \
         FOR EACH ROW EXECUTE FUNCTION dbx5a.touch()",
    ] {
        run(&mut s, sql, &[]).await.unwrap();
    }
    let parent = dependencies(s.as_mut(), "dbx5a", "parent", ObjectKind::Table).await;
    let view = dependencies(s.as_mut(), "dbx5a", "parent_v", ObjectKind::View).await;
    let child = dependencies(s.as_mut(), "dbx5a", "child", ObjectKind::Table).await;
    let func = dependencies(s.as_mut(), "dbx5a", "touch", ObjectKind::Function).await;
    run(&mut s, "DROP SCHEMA dbx5a CASCADE", &[]).await.unwrap();
    assert_eq!(
        parent,
        [
            "used_by Some(Table) dbx5a.child (foreign key child_parent_fk)",
            "used_by Some(View) dbx5a.parent_v (query)",
        ]
    );
    assert_eq!(view, ["uses Some(Table) dbx5a.parent (query)"]);
    assert!(
        child.contains(&"uses Some(Table) dbx5a.parent (foreign key child_parent_fk)".to_owned())
    );
    assert!(child.contains(&"uses Some(Sequence) dbx5a.child_id_seq (column default)".to_owned()));
    assert!(child.contains(&"uses Some(Function) dbx5a.touch (trigger child_touch)".to_owned()));
    assert!(child.contains(&"used_by Some(Sequence) dbx5a.child_id_seq (owned by)".to_owned()));
    assert_eq!(
        func,
        ["used_by Some(Table) dbx5a.child (trigger child_touch)"]
    );
}

/// DBX-5c: roles and extensions sit outside any schema, with attribute / version lines,
/// `CREATE` text and search hits.
#[tokio::test]
#[ignore = "needs docker postgres"]
async fn roles_and_extensions() {
    let mut s = session().await;
    let objects = |chunk: CatalogChunk| match chunk {
        CatalogChunk::Objects(o) => o,
        other => panic!("objects expected, got {other:?}"),
    };
    let roles = objects(
        s.introspect(IntrospectScope::Objects {
            schema: String::new(),
            kind: ObjectKind::Role,
        })
        .await
        .unwrap(),
    );
    let me = roles
        .iter()
        .find(|r| r.name == "switchyard")
        .expect("the test login is listed");
    assert!(me.schema.is_empty());
    assert!(me.detail.as_deref().unwrap_or("").starts_with("user"));
    assert!(roles.iter().all(|r| !r.name.starts_with("pg_")));

    let exts = objects(
        s.introspect(IntrospectScope::Objects {
            schema: String::new(),
            kind: ObjectKind::Extension,
        })
        .await
        .unwrap(),
    );
    let names: Vec<&str> = exts.iter().map(|e| e.name.as_str()).collect();
    // hypopg is optional in the image; pg_stat_statements and plpgsql are always there.
    for e in ["pg_stat_statements", "plpgsql"] {
        assert!(names.contains(&e), "{e} in {names:?}");
    }
    let plpgsql = exts.iter().find(|e| e.name == "plpgsql").unwrap();
    assert!(
        plpgsql
            .detail
            .as_deref()
            .unwrap()
            .ends_with(" · pg_catalog")
    );

    let detail = |chunk: CatalogChunk| match chunk {
        CatalogChunk::Detail(d) => d,
        other => panic!("detail expected, got {other:?}"),
    };
    let role = detail(
        s.introspect(IntrospectScope::Detail {
            schema: String::new(),
            name: "switchyard".into(),
            kind: ObjectKind::Role,
        })
        .await
        .unwrap(),
    );
    assert!(
        role.ddl
            .starts_with("CREATE ROLE switchyard WITH LOGIN SUPERUSER"),
        "{}",
        role.ddl
    );
    let ext = detail(
        s.introspect(IntrospectScope::Detail {
            schema: String::new(),
            name: "pg_stat_statements".into(),
            kind: ObjectKind::Extension,
        })
        .await
        .unwrap(),
    );
    assert!(
        ext.ddl.starts_with(
            "CREATE EXTENSION IF NOT EXISTS pg_stat_statements WITH SCHEMA public VERSION '"
        ),
        "{}",
        ext.ddl
    );
    assert!(
        s.introspect(IntrospectScope::Detail {
            schema: String::new(),
            name: "swy_no_such_ext".into(),
            kind: ObjectKind::Extension,
        })
        .await
        .is_err()
    );
    assert!(
        search(s.as_mut(), "stat_statements")
            .await
            .contains(&".pg_stat_statements Extension".to_owned())
    );
    assert!(
        search(s.as_mut(), "switchyard")
            .await
            .contains(&".switchyard Role".to_owned())
    );
}
