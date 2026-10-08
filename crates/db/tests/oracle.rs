//! Oracle integration tests. Need Oracle Instant Client (folder in
//! `SWITCHYARD_ORACLE_CLIENT`, else the usual library search) and a database: the docker
//! `oracle` service (`docker compose -f docker/compose.yml --profile oracle up -d oracle`)
//! or any Oracle reachable with `SWITCHYARD_ORACLE_HOST`, `_PORT`, `_SERVICE`, `_USER`,
//! `_PASSWORD`.
//! Run with `cargo test -p switchyard-db --test oracle -- --ignored --test-threads 1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::batch::BatchList;
use switchyard_db::catalog::{CatalogChunk, IntrospectScope, ObjectKind};
use switchyard_db::driver::{DbConfig, DbSession, Driver};
use switchyard_db::error::{DbError, ErrorPosition};
use switchyard_db::oracle::OracleDriver;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::{DataType, Engine, Value};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn config() -> DbConfig {
    let mut cfg = DbConfig::new(
        Engine::Oracle,
        env("SWITCHYARD_ORACLE_HOST", "localhost"),
        env("SWITCHYARD_ORACLE_SERVICE", "FREEPDB1"),
    );
    cfg.port = env("SWITCHYARD_ORACLE_PORT", "1521").parse().unwrap();
    cfg.user = env("SWITCHYARD_ORACLE_USER", "app");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_ORACLE_PASSWORD",
        "Switchyard1",
    )));
    if let Ok(dir) = std::env::var("SWITCHYARD_ORACLE_CLIENT") {
        cfg.options.insert("client_lib_dir".into(), dir);
    }
    cfg
}

/// `ddl` with server-generated constraint names (`SYS_C0012345`) replaced, so the snapshot
/// does not change between databases.
fn redact_sys_names(ddl: &str) -> String {
    let mut out = String::new();
    let mut rest = ddl;
    while let Some(i) = rest.find("SYS_C") {
        out.push_str(&rest[..i]);
        out.push_str("SYS_C<generated>");
        let tail = &rest[i + "SYS_C".len()..];
        let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

async fn connect() -> Box<dyn DbSession> {
    OracleDriver
        .connect(&config(), None)
        .await
        .expect("connect")
}

struct Run {
    sets: Vec<(Vec<String>, Vec<DataType>, BatchList)>,
    notices: Vec<String>,
    affected: Option<u64>,
}

async fn run(s: &mut dyn DbSession, sql: &str, params: &[Value]) -> Result<Run, DbError> {
    let mut stream = s.execute(sql, params).await?;
    let mut out = Run {
        sets: Vec::new(),
        notices: Vec::new(),
        affected: None,
    };
    while let Some(ev) = stream.next().await {
        match ev? {
            ResultEvent::Columns(c) => out.sets.push((
                c.iter().map(|m| m.name.clone()).collect(),
                c.iter().map(|m| m.data_type).collect(),
                BatchList::default(),
            )),
            ResultEvent::Rows(b) => out.sets.last_mut().expect("columns first").2.push(b),
            ResultEvent::Notice(n) => out.notices.push(n.message),
            ResultEvent::Done(d) => out.affected = d.affected,
            ResultEvent::NextResultSet => {}
        }
    }
    Ok(out)
}

fn cell(r: &Run, row: usize, col: usize) -> Value {
    let (_, types, list) = &r.sets[0];
    list.cell(row, col).expect("cell").to_value(types[col])
}

#[tokio::test]
#[ignore]
async fn connects_and_reports_the_server() {
    let s = connect().await;
    assert!(
        s.server_version().contains("Oracle"),
        "{}",
        s.server_version()
    );
}

#[tokio::test]
#[ignore]
async fn decodes_common_types() {
    let mut s = connect().await;
    let r = run(
        s.as_mut(),
        "select cast(42 as number(10)) n, cast(12.5 as number(8,2)) d, \
                cast(1.5 as binary_double) f, 'hé' t, hextoraw('dead') b, \
                date '2024-02-29' dt, timestamp '2024-02-29 10:11:12.5' ts, \
                timestamp '2024-02-29 10:00:00 +02:00' tz, cast(null as varchar2(1)) z \
         from dual",
        &[],
    )
    .await
    .expect("query");
    let (names, types, _) = &r.sets[0];
    assert_eq!(names[0], "N");
    assert_eq!(
        types[..8],
        [
            DataType::Int64,
            DataType::Numeric,
            DataType::Float64,
            DataType::Text,
            DataType::Bytes,
            DataType::Timestamp,
            DataType::Timestamp,
            DataType::TimestampTz
        ]
    );
    assert_eq!(cell(&r, 0, 0), Value::Int(42));
    assert_eq!(cell(&r, 0, 1), Value::Numeric("12.5".into()));
    assert_eq!(cell(&r, 0, 2), Value::Float(1.5));
    assert_eq!(cell(&r, 0, 3), Value::Text("hé".into()));
    assert_eq!(cell(&r, 0, 4), Value::Bytes(vec![0xde, 0xad]));
    // 2024-02-29 = 19782 days after 1970-01-01.
    assert_eq!(cell(&r, 0, 5), Value::Timestamp(19782 * 86_400_000_000));
    assert_eq!(
        cell(&r, 0, 6),
        Value::Timestamp(19782 * 86_400_000_000 + 36_672_500_000)
    );
    assert_eq!(
        cell(&r, 0, 7),
        Value::TimestampTz(19782 * 86_400_000_000 + 8 * 3_600_000_000)
    );
    assert_eq!(cell(&r, 0, 8), Value::Null);
}

#[tokio::test]
#[ignore]
async fn streams_many_rows_in_batches() {
    let mut s = connect().await;
    let r = run(
        s.as_mut(),
        "select level id, 'row ' || level name from dual connect by level <= 25000",
        &[],
    )
    .await
    .expect("query");
    assert_eq!(r.sets[0].2.len(), 25_000);
}

#[tokio::test]
#[ignore]
async fn binds_named_parameters_once_per_name() {
    let mut s = connect().await;
    let r = run(
        s.as_mut(),
        "select :a || '-' || :b || '-' || :a from dual",
        &[Value::Text("x".into()), Value::Int(7)],
    )
    .await
    .expect("query");
    assert_eq!(cell(&r, 0, 0), Value::Text("x-7-x".into()));
}

#[tokio::test]
#[ignore]
async fn dml_transactions_and_plsql_output() {
    let mut s = connect().await;
    let _ = run(s.as_mut(), "drop table swy_t purge", &[]).await;
    run(
        s.as_mut(),
        "create table swy_t (id number(10) primary key, name varchar2(20))",
        &[],
    )
    .await
    .expect("create");
    let r = run(
        s.as_mut(),
        "insert into swy_t select level, 'n' || level from dual connect by level <= 3",
        &[],
    )
    .await
    .expect("insert");
    assert_eq!(r.affected, Some(3));

    // An explicit transaction rolls back.
    s.begin().await.expect("begin");
    run(s.as_mut(), "delete from swy_t", &[])
        .await
        .expect("delete");
    s.rollback().await.expect("rollback");
    let r = run(s.as_mut(), "select count(*) from swy_t", &[])
        .await
        .expect("count");
    assert_eq!(cell(&r, 0, 0), Value::Numeric("3".into()));

    let r = run(
        s.as_mut(),
        "begin\n  for i in 1..2 loop dbms_output.put_line('line ' || i); end loop;\nend;",
        &[],
    )
    .await
    .expect("plsql");
    assert_eq!(r.notices, ["line 1", "line 2"]);

    let chunk = s
        .introspect(IntrospectScope::Detail {
            schema: "APP".into(),
            name: "SWY_T".into(),
            kind: ObjectKind::Table,
        })
        .await
        .expect("detail");
    let CatalogChunk::Detail(d) = chunk else {
        panic!("detail");
    };
    assert_eq!(d.columns.len(), 2);
    assert!(d.columns[0].is_primary_key);
    assert!(d.ddl.contains("CREATE TABLE"), "{}", d.ddl);
    insta::assert_snapshot!("oracle_swy_t_ddl", redact_sys_names(&d.ddl));
    run(s.as_mut(), "drop table swy_t purge", &[])
        .await
        .expect("drop");
}

#[tokio::test]
#[ignore]
async fn routine_definitions() {
    use switchyard_db::dialect::{Dialect, oracle::OracleDialect};
    let mut s = connect().await;
    for sql in [
        "create or replace procedure swy_p(p_id in number, p_name in varchar2, p_out out number) \
         as begin p_out := p_id; end;",
        "create or replace function swy_f(x number) return number as begin return x * 2; end;",
    ] {
        run(s.as_mut(), sql, &[]).await.expect("create routine");
    }
    let mut routine = async |name: &str, kind| {
        let chunk = s
            .introspect(IntrospectScope::RoutineDefinition {
                schema: "APP".into(),
                name: name.into(),
                kind,
                signature: None,
            })
            .await;
        chunk.map(|c| match c {
            CatalogChunk::Detail(d) => (
                d.ddl,
                d.columns
                    .iter()
                    .map(|c| format!("{} {}", c.name, c.data_type))
                    .collect::<Vec<_>>(),
            ),
            other => panic!("{other:?}"),
        })
    };
    let (ddl, params) = routine("SWY_P", ObjectKind::Procedure)
        .await
        .expect("procedure");
    assert!(ddl.contains("PROCEDURE"), "{ddl}");
    // OUT parameters are left out of the EXEC template.
    assert_eq!(params, ["P_ID NUMBER", "P_NAME VARCHAR2"]);
    assert!(
        OracleDialect
            .script_create(ObjectKind::Procedure, &ddl)
            .ends_with("\n/"),
        "PL/SQL runs to a / line"
    );
    let (ddl, params) = routine("SWY_F", ObjectKind::Function)
        .await
        .expect("function");
    assert!(ddl.contains("return x * 2"), "{ddl}");
    assert_eq!(params, ["X NUMBER"]);
    assert!(routine("SWY_NONE", ObjectKind::Function).await.is_err());
    for sql in ["drop procedure swy_p", "drop function swy_f"] {
        run(s.as_mut(), sql, &[]).await.expect("drop routine");
    }
}

#[tokio::test]
#[ignore]
async fn errors_carry_code_and_position() {
    let mut s = connect().await;
    let err = run(s.as_mut(), "select nope from dual", &[])
        .await
        .err()
        .expect("error");
    let server = err.as_server().expect("server error");
    assert_eq!(server.code.as_deref(), Some("ORA-00904"));
    assert_eq!(server.position, Some(ErrorPosition::Offset(8)));
}

#[tokio::test]
#[ignore]
async fn cancel_breaks_a_running_statement() {
    // A long SQL statement; `DBMS_SESSION.SLEEP` would not do: the server only honours
    // the break once the sleep ends.
    let mut s = connect().await;
    let cancel = s.cancel_handle();
    let started = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    let err = run(
        s.as_mut(),
        "select count(*) from all_objects a, all_objects b, all_objects c",
        &[],
    )
    .await
    .err()
    .expect("cancelled");
    canceller.await.expect("join").expect("cancel");
    assert!(matches!(err, DbError::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // The session is usable afterwards.
    let r = run(s.as_mut(), "select 1 from dual", &[])
        .await
        .expect("after cancel");
    assert_eq!(r.sets.len(), 1);
}

#[tokio::test]
#[ignore]
async fn lists_schemas_and_tables() {
    let mut s = connect().await;
    let CatalogChunk::Schemas(schemas) = s
        .introspect(IntrospectScope::Schemas)
        .await
        .expect("schemas")
    else {
        panic!("schemas");
    };
    assert!(schemas.iter().any(|x| x.name == "APP" && !x.is_system));
    assert!(schemas.iter().any(|x| x.name == "SYS" && x.is_system));
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
#[ignore]
async fn global_object_search() {
    let mut s = connect().await;
    for t in ["swy_search_t", "swyxsearch_t"] {
        run(
            s.as_mut(),
            &format!(
                "begin execute immediate 'drop table {t} purge'; \
                 exception when others then null; end;"
            ),
            &[],
        )
        .await
        .expect("drop old");
        run(s.as_mut(), &format!("create table {t} (id number)"), &[])
            .await
            .expect("create");
    }
    // Case-insensitive, and `_` is literal: SWYXSEARCH_T does not match.
    assert_eq!(
        search(s.as_mut(), "swy_search").await,
        ["APP.SWY_SEARCH_T Table"]
    );
    assert_eq!(search(s.as_mut(), "Search_T").await.len(), 2);
    // Oracle-maintained users are left out by default.
    assert!(search(s.as_mut(), "DBA_TABLES").await.is_empty());
    for t in ["swy_search_t", "swyxsearch_t"] {
        run(s.as_mut(), &format!("drop table {t} purge"), &[])
            .await
            .expect("drop");
    }
}
