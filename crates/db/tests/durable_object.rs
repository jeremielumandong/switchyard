//! Durable Object SQLite driver against a local stand-in for Cloudflare's `query/v2`
//! endpoint that runs the queries on an in-memory SQLite database, as the object would.
//! Runs without network access; the real Cloudflare API has not been exercised here.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use rusqlite::types::ValueRef;
use secrecy::SecretString;
use serde_json::{Value as Json, json};
use switchyard_db::catalog::{CatalogChunk, IntrospectScope, ObjectKind};
use switchyard_db::d1::{
    DurableObjectDriver, JURISDICTION_OPTION, OBJECT_KIND_OPTION, OBJECT_OPTION,
};
use switchyard_db::driver::{DbConfig, DbSession, Driver};
use switchyard_db::error::DbError;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::{DataType, Engine, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ACCOUNT: &str = "acct123";
const NAMESPACE: &str = "5fd1cafff895419c8bcc647fc64ab8f0";
const TOKEN: &str = "good-token";
const OBJECT_ID: &str = "fe7803fc55b964e09d94666545aab688d360c6bda69ba349ced1e5f28d2fc2c8";

/// One object's database per selector, like Cloudflare keeps one per object.
type Objects = Arc<Mutex<Vec<(String, rusqlite::Connection)>>>;

fn to_json(v: ValueRef<'_>) -> Json {
    match v {
        ValueRef::Null => Json::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => json!(b),
    }
}

fn run(db: &rusqlite::Connection, sql: &str, params: &[Json]) -> Result<Json, String> {
    let mut st = db.prepare(sql).map_err(|e| e.to_string())?;
    let columns: Vec<String> = st.column_names().iter().map(|c| c.to_string()).collect();
    let params: Vec<rusqlite::types::Value> = params
        .iter()
        .map(|p| match p {
            Json::Null => rusqlite::types::Value::Null,
            Json::Number(n) if n.is_i64() => rusqlite::types::Value::Integer(n.as_i64().unwrap()),
            Json::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
            Json::String(s) => rusqlite::types::Value::Text(s.clone()),
            other => rusqlite::types::Value::Text(other.to_string()),
        })
        .collect();
    let n = columns.len();
    let mut rows = st
        .query(rusqlite::params_from_iter(params))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        out.push(Json::Array(
            (0..n).map(|i| to_json(r.get_ref(i).unwrap())).collect(),
        ));
    }
    let written = db.changes();
    Ok(json!({
        "columns": columns,
        "rows": out,
        "meta": {"rows_read": out.len(), "rows_written": if n == 0 { written } else { 0 }}
    }))
}

fn reply(head: &str, body: &Json, objects: &Objects) -> (u16, Json) {
    let path_ok = head.starts_with(&format!(
        "POST /client/v4/accounts/{ACCOUNT}/workers/durable_objects/namespaces/{NAMESPACE}/query/v2 "
    ));
    let auth_ok = head
        .lines()
        .any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {TOKEN}")));
    if !path_ok {
        return (
            404,
            json!({"success": false, "errors": [{"code": 10090, "message": "namespace not found"}], "messages": []}),
        );
    }
    if !auth_ok {
        return (
            403,
            json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}], "messages": []}),
        );
    }
    // The API's two request shapes: by id, or by name with a jurisdiction.
    let key = match (
        body["durable_object_id"].as_str(),
        body["durable_object_name"].as_str(),
        body["jurisdiction"].as_str(),
    ) {
        (Some(id), None, None) => format!("id:{id}"),
        (None, Some(name), Some(j)) => format!("name:{j}:{name}"),
        _ => {
            return (
                400,
                json!({"success": false, "errors": [{"code": 10001, "message": "invalid body"}], "messages": []}),
            );
        }
    };
    let mut objects = objects.lock().unwrap();
    if !objects.iter().any(|(k, _)| *k == key) {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        // Cloudflare's own bookkeeping table, which the catalog must hide.
        db.execute_batch("CREATE TABLE _cf_KV (key TEXT PRIMARY KEY, value BLOB);")
            .unwrap();
        objects.push((key.clone(), db));
    }
    let db = &objects.iter().find(|(k, _)| *k == key).unwrap().1;
    let mut results = Vec::new();
    for q in body["queries"].as_array().cloned().unwrap_or_default() {
        let sql = q["sql"].as_str().unwrap_or_default();
        let params = q["params"].as_array().cloned().unwrap_or_default();
        match run(db, sql, &params) {
            Ok(r) => results.push(r),
            Err(e) => {
                return (
                    200,
                    json!({"success": true, "errors": [], "messages": [],
                           "result": {"results": results, "error": format!("{e}: SQLITE_ERROR")}}),
                );
            }
        }
    }
    (
        200,
        json!({"success": true, "errors": [], "messages": [], "result": {"results": results}}),
    )
}

async fn handle(mut sock: TcpStream, objects: Objects) {
    let mut buf = Vec::new();
    loop {
        let head_end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            let mut chunk = [0u8; 8192];
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        while buf.len() < head_end + len {
            let mut chunk = [0u8; 8192];
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body: Json =
            serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Json::Null);
        buf.drain(..head_end + len);
        let (status, reply) = reply(&head, &body, &objects);
        let text = reply.to_string();
        let resp = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{text}",
            text.len()
        );
        if sock.write_all(resp.as_bytes()).await.is_err() {
            return;
        }
    }
}

async fn server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let objects: Objects = Arc::default();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(handle(sock, objects.clone()));
        }
    });
    format!("http://{addr}/client/v4")
}

fn config(token: &str, object: &str, kind: &str) -> DbConfig {
    let mut cfg = DbConfig::new(Engine::DurableObject, ACCOUNT, NAMESPACE);
    cfg.password = Some(SecretString::from(token.to_owned()));
    cfg.options.insert(OBJECT_OPTION.into(), object.into());
    cfg.options.insert(OBJECT_KIND_OPTION.into(), kind.into());
    cfg
}

async fn connect(base: &str, cfg: &DbConfig) -> Box<dyn DbSession> {
    DurableObjectDriver::with_base(base)
        .connect(cfg, None)
        .await
        .expect("connect")
}

async fn collect(s: &mut dyn DbSession, sql: &str, params: &[Value]) -> Vec<ResultEvent> {
    let mut stream = s.execute(sql, params).await.expect("execute");
    let mut out = Vec::new();
    while let Some(ev) = stream.next().await {
        out.push(ev.expect("event"));
    }
    out
}

#[tokio::test]
async fn reads_and_writes_one_named_object() {
    let base = server().await;
    let mut s = connect(&base, &config(TOKEN, "lobby", "name")).await;
    assert_eq!(s.server_version(), "Cloudflare Durable Object (SQLite)");
    collect(
        s.as_mut(),
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT NOT NULL, score REAL)",
        &[],
    )
    .await;
    collect(
        s.as_mut(),
        "INSERT INTO messages (body, score) VALUES (?, ?)",
        &[Value::Text("hello".into()), Value::Float(1.5)],
    )
    .await;
    let events = collect(s.as_mut(), "SELECT id, body, score FROM messages", &[]).await;
    let ResultEvent::Columns(cols) = &events[0] else {
        panic!("columns first: {events:?}")
    };
    let types: Vec<DataType> = cols.iter().map(|c| c.data_type).collect();
    assert_eq!(types, [DataType::Int64, DataType::Text, DataType::Float64]);
    assert!(matches!(&events[1], ResultEvent::Rows(b) if b.len() == 1));

    // Another name is another object, with its own storage.
    let mut other = connect(&base, &config(TOKEN, "room-2", "name")).await;
    let err = other
        .execute("SELECT * FROM messages", &[])
        .await
        .err()
        .expect("room-2 has no messages table");
    assert!(err.to_string().contains("no such table"), "{err}");
}

#[tokio::test]
async fn sql_errors_and_unknown_tables_are_server_errors() {
    let base = server().await;
    let mut s = connect(&base, &config(TOKEN, "lobby", "name")).await;
    let err = s
        .execute("SELECT * FROM nowhere", &[])
        .await
        .err()
        .expect("error");
    let server = err.as_server().expect("server error");
    assert!(server.message.contains("no such table"), "{server:?}");
    assert_eq!(server.code.as_deref(), Some("SQLITE_ERROR"));
}

#[tokio::test]
async fn opens_an_object_by_id_and_checks_its_shape() {
    let base = server().await;
    connect(&base, &config(TOKEN, OBJECT_ID, "id")).await;
    let err = DurableObjectDriver::with_base(&base)
        .connect(&config(TOKEN, "not-an-id", "id"), None)
        .await
        .err()
        .expect("refused");
    assert!(matches!(err, DbError::Connect(m) if m.contains("64")));
}

#[tokio::test]
async fn bad_token_and_unknown_namespace_fail_to_connect() {
    let base = server().await;
    let err = DurableObjectDriver::with_base(&base)
        .connect(&config("bad", "lobby", "name"), None)
        .await
        .err()
        .expect("refused");
    assert!(matches!(err, DbError::Connect(m) if m.contains("API token")));
    let mut cfg = config(TOKEN, "lobby", "name");
    cfg.database = "ffffffffffffffffffffffffffffffff".into();
    let err = DurableObjectDriver::with_base(&base)
        .connect(&cfg, None)
        .await
        .err()
        .expect("refused");
    assert!(matches!(err, DbError::Connect(m) if m.contains("not found")));
    let mut cfg = config(TOKEN, "lobby", "name");
    cfg.options
        .insert(JURISDICTION_OPTION.into(), "mars".into());
    assert!(
        DurableObjectDriver::with_base(&base)
            .connect(&cfg, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn catalog_lists_user_tables_and_columns_without_cloudflare_tables() {
    let base = server().await;
    let mut cfg = config(TOKEN, "catalog", "name");
    cfg.options.insert(JURISDICTION_OPTION.into(), "eu".into());
    let mut s = connect(&base, &cfg).await;
    collect(
        s.as_mut(),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE)",
        &[],
    )
    .await;
    collect(
        s.as_mut(),
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id))",
        &[],
    )
    .await;
    let CatalogChunk::Objects(objs) = s
        .introspect(IntrospectScope::Objects {
            schema: "main".into(),
            kind: ObjectKind::Table,
        })
        .await
        .expect("objects")
    else {
        panic!("objects")
    };
    let names: Vec<_> = objs.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["orders", "users"]);
    let CatalogChunk::Detail(d) = s
        .introspect(IntrospectScope::Detail {
            schema: "main".into(),
            name: "orders".into(),
            kind: ObjectKind::Table,
        })
        .await
        .expect("detail")
    else {
        panic!("detail")
    };
    let cols: Vec<_> = d.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "user_id"]);
    assert_eq!(d.foreign_keys.len(), 1);
}

#[tokio::test]
async fn transactions_are_refused() {
    let base = server().await;
    let mut s = connect(&base, &config(TOKEN, "lobby", "name")).await;
    assert!(matches!(s.begin().await, Err(DbError::Unsupported(_))));
    assert!(!Engine::DurableObject.supports_transactions());
}
