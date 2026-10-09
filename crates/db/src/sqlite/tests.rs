use super::*;
use crate::batch::BatchList;
use crate::catalog::ObjectKind;

struct Result_ {
    columns: Vec<Arc<[ColumnMeta]>>,
    rows: Vec<BatchList>,
    notices: Vec<Notice>,
    affected: Option<u64>,
    batches: usize,
}

async fn run(s: &mut dyn DbSession, sql: &str, params: &[Value]) -> Result<Result_> {
    let mut stream = s.execute(sql, params).await?;
    let mut out = Result_ {
        columns: Vec::new(),
        rows: Vec::new(),
        notices: Vec::new(),
        affected: None,
        batches: 0,
    };
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(c) => {
                out.columns.push(c);
                out.rows.push(BatchList::default());
            }
            ResultEvent::Rows(b) => {
                out.batches += 1;
                if let Some(list) = out.rows.last_mut() {
                    list.push(b);
                }
            }
            ResultEvent::Notice(n) => out.notices.push(n),
            ResultEvent::NextResultSet => {}
            ResultEvent::Done(c) => out.affected = c.affected,
        }
    }
    Ok(out)
}

fn cell(r: &Result_, set: usize, row: usize, col: usize) -> Value {
    let t = r.columns[set][col].data_type;
    r.rows[set]
        .cell(row, col)
        .map(|c| c.to_value(t))
        .unwrap_or(Value::Null)
}

fn config(path: &std::path::Path) -> DbConfig {
    DbConfig::new(Engine::Sqlite, "", path.to_string_lossy())
}

async fn open_temp() -> (tempfile::TempDir, Box<dyn DbSession>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.db");
    let s = SqliteDriver
        .connect(&config(&path), None)
        .await
        .expect("connect");
    (dir, s)
}

#[tokio::test]
async fn creates_queries_and_types_columns() {
    let (_dir, mut s) = open_temp().await;
    assert!(s.server_version().starts_with("SQLite 3."));
    let r = run(
        s.as_mut(),
        "create table t (id integer primary key, name text, price real, data blob, note);
         insert into t values (1, 'a', 2.5, x'0102', 'x'), (2, 'b', 3, null, 7);",
        &[],
    )
    .await
    .expect("script");
    assert_eq!(r.affected, Some(2));

    let r = run(
        s.as_mut(),
        "select id, name, price, data, note, id * 2 as twice from t where id >= ?1 order by id",
        &[Value::Int(1)],
    )
    .await
    .expect("select");
    let types: Vec<DataType> = r.columns[0].iter().map(|c| c.data_type).collect();
    assert_eq!(
        types,
        [
            DataType::Int64,
            DataType::Text,
            DataType::Float64,
            DataType::Bytes,
            DataType::Text,
            DataType::Int64
        ]
    );
    assert_eq!(r.columns[0][2].type_name, "REAL");
    assert_eq!(cell(&r, 0, 1, 2), Value::Float(3.0));
    assert_eq!(cell(&r, 0, 0, 3), Value::Bytes(vec![1, 2]));
    assert_eq!(cell(&r, 0, 1, 4), Value::Text("7".into()));
    assert_eq!(cell(&r, 0, 1, 5), Value::Int(4));
    // Every column of a plain table read points at the same table (inline editing).
    let ids: Vec<_> = r.columns[0].iter().map(|c| c.table_id).collect();
    assert!(ids[..5].iter().all(|i| i.is_some() && *i == ids[0]));
    assert_eq!(ids[5], None);
}

#[tokio::test]
async fn streams_batches_and_flags_late_type_changes() {
    let (_dir, mut s) = open_temp().await;
    run(
        s.as_mut(),
        "create table n (v integer);
         with recursive c(i) as (select 1 union all select i + 1 from c where i < 2500)
         insert into n select i from c;
         insert into n values ('oops');",
        &[],
    )
    .await
    .expect("seed");
    let r = run(s.as_mut(), "select v from n order by rowid", &[])
        .await
        .expect("select");
    assert_eq!(r.batches, 3);
    assert_eq!(r.rows[0].len(), 2501);
    assert_eq!(r.columns[0][0].data_type, DataType::Int64);
    assert_eq!(cell(&r, 0, 2499, 0), Value::Int(2500));
    assert_eq!(cell(&r, 0, 2500, 0), Value::Null);
    assert_eq!(r.notices.len(), 1);
    assert!(r.notices[0].message.starts_with("v held values"));
}

#[tokio::test]
async fn reports_errors_with_position_and_code() {
    let (_dir, mut s) = open_temp().await;
    let e = run(s.as_mut(), "select 1;\nselect * frm t", &[])
        .await
        .err()
        .expect("error");
    let se = e.as_server().expect("server error");
    assert_eq!(se.code.as_deref(), Some("SQLITE_ERROR"));
    assert!(se.message.contains("syntax error"), "{}", se.message);
    // `frm` starts at character 20 of the whole script.
    assert_eq!(se.position, Some(ErrorPosition::Offset(20)));

    run(s.as_mut(), "create table u (id integer primary key)", &[])
        .await
        .expect("create");
    run(s.as_mut(), "insert into u values (1)", &[])
        .await
        .expect("insert");
    let e = run(s.as_mut(), "insert into u values (1)", &[])
        .await
        .err()
        .expect("duplicate");
    assert_eq!(
        e.as_server().and_then(|s| s.code.as_deref()),
        Some("SQLITE_CONSTRAINT")
    );
    let e = run(s.as_mut(), "select ?1, ?2", &[Value::Int(1)])
        .await
        .err()
        .expect("missing parameter");
    assert!(matches!(e, DbError::Param(_)));
}

#[tokio::test]
async fn transactions_roll_back() {
    let (_dir, mut s) = open_temp().await;
    run(s.as_mut(), "create table t (x)", &[])
        .await
        .expect("create");
    s.begin().await.expect("begin");
    assert!(s.in_transaction());
    run(s.as_mut(), "insert into t values (1)", &[])
        .await
        .expect("insert");
    s.rollback().await.expect("rollback");
    assert!(!s.in_transaction());
    let r = run(s.as_mut(), "select count(*) from t", &[])
        .await
        .expect("count");
    assert_eq!(cell(&r, 0, 0, 0), Value::Int(0));
}

#[tokio::test]
async fn read_only_neither_creates_nor_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ro.db");
    let mut cfg = config(&path);
    cfg.read_only = true;
    let e = SqliteDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("missing");
    assert!(matches!(e, DbError::Connect(_)));
    assert!(!path.exists());

    let mut rw = SqliteDriver
        .connect(&config(&path), None)
        .await
        .expect("create");
    run(rw.as_mut(), "create table t (x)", &[])
        .await
        .expect("create table");
    drop(rw);
    let mut ro = SqliteDriver.connect(&cfg, None).await.expect("open ro");
    let e = run(ro.as_mut(), "insert into t values (1)", &[])
        .await
        .err()
        .expect("write refused");
    assert!(e.as_server().is_some(), "{e}");
}

#[tokio::test]
async fn rejects_files_that_are_not_databases_and_tunnels() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("notes.txt");
    std::fs::write(
        &path,
        "this is not a database, just some text that is long enough",
    )
    .expect("write");
    let e = SqliteDriver
        .connect(&config(&path), None)
        .await
        .err()
        .expect("not a database");
    assert!(matches!(e, DbError::Connect(_)), "{e}");
    let tunnel = TunnelEndpoint {
        host: "127.0.0.1".into(),
        port: 1,
    };
    let e = SqliteDriver
        .connect(&config(&path), Some(tunnel))
        .await
        .err()
        .expect("tunnel");
    assert!(matches!(e, DbError::Unsupported(_)));
}

#[tokio::test]
async fn cancel_interrupts_a_running_query() {
    let (_dir, mut s) = open_temp().await;
    let cancel = s.cancel_handle();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel().await
    });
    let e = run(
        s.as_mut(),
        "with recursive c(i) as (select 1 union all select i + 1 from c) select count(*) from c",
        &[],
    )
    .await
    .err()
    .expect("cancelled");
    assert!(matches!(e, DbError::Cancelled), "{e}");
    task.await.expect("join").expect("cancel");
    // The session still works afterwards.
    let r = run(s.as_mut(), "select 1", &[]).await.expect("after");
    assert_eq!(cell(&r, 0, 0, 0), Value::Int(1));
}

#[tokio::test]
async fn introspects_tables_indexes_keys_and_triggers() {
    let (dir, mut s) = open_temp().await;
    let other = dir.path().join("other.db");
    let attach = format!("attach database '{}' as aux", other.display());
    run(
        s.as_mut(),
        &format!(
            "create table parent (id integer primary key, code text unique);
             create table child (id integer primary key, parent_id integer not null default 0
                 references parent(id) on delete cascade, label text);
             create index child_label on child(label);
             create view v as select * from child;
             create trigger child_ai after insert on child begin select 1; end;
             {attach};
             create table aux.extra (k);"
        ),
        &[],
    )
    .await
    .expect("schema");

    let CatalogChunk::Schemas(schemas) = s
        .introspect(IntrospectScope::Schemas)
        .await
        .expect("schemas")
    else {
        panic!("schemas")
    };
    let names: Vec<_> = schemas.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["main", "aux"]);

    let CatalogChunk::Objects(tables) = s
        .introspect(IntrospectScope::Objects {
            schema: "main".into(),
            kind: ObjectKind::Table,
        })
        .await
        .expect("tables")
    else {
        panic!("objects")
    };
    let names: Vec<_> = tables.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["child", "parent"]);

    let CatalogChunk::Detail(d) = s
        .introspect(IntrospectScope::Detail {
            schema: "main".into(),
            name: "child".into(),
            kind: ObjectKind::Table,
        })
        .await
        .expect("detail")
    else {
        panic!("detail")
    };
    assert_eq!(d.columns.len(), 3);
    assert!(d.columns[0].is_primary_key);
    assert!(!d.columns[1].nullable);
    assert_eq!(d.columns[1].default.as_deref(), Some("0"));
    assert_eq!(d.indexes.len(), 1);
    assert_eq!(d.indexes[0].columns, ["label"]);
    assert_eq!(d.foreign_keys.len(), 1);
    assert_eq!(d.foreign_keys[0].references, "main.parent");
    assert_eq!(d.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(d.triggers, ["child_ai"]);
    assert_eq!(d.trigger_details[0].timing, "AFTER");
    assert!(d.ddl.contains("CREATE INDEX child_label"));

    let CatalogChunk::Objects(hits) = s
        .introspect(IntrospectScope::Search {
            pattern: "EXT".into(),
            limit: 10,
            include_system: false,
        })
        .await
        .expect("search")
    else {
        panic!("search")
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(
        (hits[0].schema.as_str(), hits[0].name.as_str()),
        ("aux", "extra")
    );

    let CatalogChunk::AllColumns(all) = s
        .introspect(IntrospectScope::AllColumns)
        .await
        .expect("all")
    else {
        panic!("all columns")
    };
    assert_eq!(all.len(), 3 + 2 + 3 + 1);
}

#[test]
fn resolves_paths() {
    assert!(resolve_path("  ").is_err());
    assert_eq!(
        resolve_path(":memory:").ok(),
        Some(PathBuf::from(":memory:"))
    );
    if let Some(home) = std::env::var_os("HOME") {
        assert_eq!(
            resolve_path("~/x.db").ok(),
            Some(PathBuf::from(home).join("x.db"))
        );
    }
}

#[test]
fn column_types_follow_values_then_declarations() {
    let seen = |bits| Seen(bits);
    assert_eq!(column_type(Some("TEXT"), seen(Seen::INT)), DataType::Int64);
    assert_eq!(
        column_type(None, seen(Seen::INT | Seen::REAL)),
        DataType::Float64
    );
    assert_eq!(
        column_type(Some("INTEGER"), seen(Seen::INT | Seen::TEXT)),
        DataType::Text
    );
    assert_eq!(column_type(Some("VARCHAR(20)"), seen(0)), DataType::Text);
    assert_eq!(column_type(Some("BIGINT"), seen(0)), DataType::Int64);
    assert_eq!(column_type(Some("DOUBLE"), seen(0)), DataType::Float64);
    assert_eq!(column_type(None, seen(0)), DataType::Text);
}

#[tokio::test]
async fn single_table_results_are_editable() {
    let (_dir, mut s) = open_temp().await;
    run(
        s.as_mut(),
        "create table t (id integer primary key, v text)",
        &[],
    )
    .await
    .expect("create");
    let sql = "select id, v from t";
    let r = run(s.as_mut(), sql, &[]).await.expect("select");
    let table = crate::edit::editable_table(&SqliteDialect::LOCAL, sql, &r.columns[0]);
    assert_eq!(table.map(|t| t.table), Some("t".to_owned()));
    let sql = "select id, v, 1 as one from t";
    let r = run(s.as_mut(), sql, &[]).await.expect("select");
    assert!(crate::edit::editable_table(&SqliteDialect::LOCAL, sql, &r.columns[0]).is_none());
}
