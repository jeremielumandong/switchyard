//! Cloudflare D1 driver against a local stand-in for the `raw` endpoint. Runs without
//! network access. The real Cloudflare API has not been exercised yet (see
//! docs/DECISIONS.md).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use serde_json::{Value as Json, json};
use switchyard_db::catalog::{CatalogChunk, IntrospectScope, ObjectKind};
use switchyard_db::d1::D1Driver;
use switchyard_db::driver::{DbConfig, DbSession, Driver};
use switchyard_db::error::{DbError, ErrorPosition};
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::{DataType, Engine, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ACCOUNT: &str = "acct123";
const DATABASE: &str = "db-uuid-1";
const TOKEN: &str = "good-token";

fn ok(results: Json) -> (u16, Json) {
    (
        200,
        json!({"success": true, "errors": [], "messages": [], "result": results}),
    )
}

fn rows(columns: &[&str], rows: Json, meta: Json) -> Json {
    json!({"success": true, "meta": meta, "results": {"columns": columns, "rows": rows}})
}

/// Answer one statement the way D1 would.
fn answer(sql: &str, params: &[Json]) -> Json {
    if sql == "SELECT sqlite_version()" {
        return rows(&["sqlite_version()"], json!([["3.45.1"]]), json!({}));
    }
    if sql.starts_with("select id, name, score from users") {
        let data: Vec<Json> = (1..=2500)
            .map(|i| {
                json!([
                    i,
                    format!("user {i}"),
                    if i % 2 == 0 { json!(1.5) } else { json!(2) }
                ])
            })
            .collect();
        return rows(
            &["id", "name", "score"],
            Json::Array(data),
            json!({"rows_read": 2500, "rows_written": 0, "served_by_region": "WEUR",
                   "timings": {"sql_duration_ms": 1.25}}),
        );
    }
    if sql.starts_with("insert") {
        return json!({"success": true, "meta": {"changed_db": true, "changes": 2, "rows_written": 2},
                      "results": {"columns": [], "rows": []}});
    }
    if sql.starts_with("select ?1") {
        return rows(&["a", "b"], json!([params]), json!({}));
    }
    if sql.contains("FROM sqlite_master m WHERE m.type = ?1") {
        assert_eq!(params, [json!("table")]);
        return rows(&["name"], json!([["orders"], ["users"]]), json!({}));
    }
    rows(&[], json!([]), json!({}))
}

async fn handle(mut sock: TcpStream, hits: Arc<AtomicUsize>) {
    let mut buf = Vec::new();
    loop {
        // Read one request (headers, then Content-Length bytes of body).
        let head_end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            let mut chunk = [0u8; 8192];
            let n = match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
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
            let n = match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
        }
        let body: Json =
            serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Json::Null);
        buf.drain(..head_end + len);
        hits.fetch_add(1, Ordering::SeqCst);

        let path_ok = head.starts_with(&format!(
            "POST /client/v4/accounts/{ACCOUNT}/d1/database/{DATABASE}/raw "
        ));
        let auth_ok = head
            .lines()
            .any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {TOKEN}")));
        let (status, reply) = if !path_ok {
            (
                404,
                json!({"success": false, "errors": [{"code": 7404, "message": "database not found"}]}),
            )
        } else if !auth_ok {
            (
                403,
                json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]}),
            )
        } else if let Some(batch) = body.get("batch").and_then(Json::as_array) {
            let results: Vec<Json> = batch
                .iter()
                .map(|st| {
                    let sql = st["sql"].as_str().unwrap_or_default();
                    let params = st["params"].as_array().cloned().unwrap_or_default();
                    answer(sql, &params)
                })
                .collect();
            ok(Json::Array(results))
        } else {
            let sql = body["sql"].as_str().unwrap_or_default().to_owned();
            let params = body["params"].as_array().cloned().unwrap_or_default();
            if sql.starts_with("slow") {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if sql.contains("boom") {
                (
                    400,
                    json!({"success": false, "errors": [{"code": 7500,
                        "message": "near \"boom\": syntax error at offset 7: SQLITE_ERROR"}]}),
                )
            } else {
                ok(json!([answer(&sql, &params)]))
            }
        };
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

async fn server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(handle(sock, h.clone()));
        }
    });
    (format!("http://{addr}/client/v4"), hits)
}

fn config(token: &str) -> DbConfig {
    let mut cfg = DbConfig::new(Engine::D1, ACCOUNT, DATABASE);
    cfg.password = Some(SecretString::from(token.to_owned()));
    cfg
}

async fn connect(base: &str) -> Box<dyn DbSession> {
    D1Driver::with_base(base)
        .connect(&config(TOKEN), None)
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
async fn connects_and_reports_version() {
    let (base, _) = server().await;
    let s = connect(&base).await;
    assert_eq!(s.server_version(), "Cloudflare D1 · SQLite 3.45.1");
    assert!(!s.in_transaction());
}

#[tokio::test]
async fn bad_token_fails_to_connect() {
    let (base, _) = server().await;
    let err = D1Driver::with_base(&base)
        .connect(&config("wrong"), None)
        .await
        .err()
        .expect("must fail");
    assert!(
        matches!(&err, DbError::Connect(m) if m.contains("API token")),
        "{err}"
    );
}

#[tokio::test]
async fn streams_rows_in_batches_with_inferred_types() {
    let (base, _) = server().await;
    let mut s = connect(&base).await;
    let events = collect(s.as_mut(), "select id, name, score from users", &[]).await;
    let ResultEvent::Columns(cols) = &events[0] else {
        panic!("columns first")
    };
    let types: Vec<_> = cols.iter().map(|c| c.data_type).collect();
    assert_eq!(types, [DataType::Int64, DataType::Text, DataType::Float64]);
    let batches: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            ResultEvent::Rows(b) => Some(b.len()),
            _ => None,
        })
        .collect();
    assert_eq!(batches, [1000, 1000, 500]);
    let notice = events.iter().find_map(|e| match e {
        ResultEvent::Notice(n) => Some(n.message.clone()),
        _ => None,
    });
    assert_eq!(
        notice.as_deref(),
        Some("rows read 2500 · rows written 0 · 1.25 ms in database · served by WEUR")
    );
    assert!(matches!(events.last(), Some(ResultEvent::Done(d)) if d.affected.is_none()));
}

#[tokio::test]
async fn writes_report_affected_rows() {
    let (base, _) = server().await;
    let mut s = connect(&base).await;
    let events = collect(s.as_mut(), "insert into t values (1), (2)", &[]).await;
    assert!(matches!(events.last(), Some(ResultEvent::Done(d)) if d.affected == Some(2)));
}

#[tokio::test]
async fn binds_positional_params() {
    let (base, _) = server().await;
    let mut s = connect(&base).await;
    let events = collect(
        s.as_mut(),
        "select ?1 as a, ?2 as b",
        &[Value::Int(42), Value::Text("x".into())],
    )
    .await;
    let row = events.iter().find_map(|e| match e {
        ResultEvent::Rows(b) => Some(b.clone()),
        _ => None,
    });
    let b = row.expect("a row");
    assert_eq!(b.len(), 1);
}

#[tokio::test]
async fn sql_errors_carry_position() {
    let (base, _) = server().await;
    let mut s = connect(&base).await;
    let err = s.execute("select boom", &[]).await.err().expect("error");
    let server = err.as_server().expect("server error");
    assert_eq!(server.code.as_deref(), Some("SQLITE_ERROR"));
    assert_eq!(server.position, Some(ErrorPosition::Offset(8)));
}

#[tokio::test]
async fn cancel_abandons_the_request() {
    let (base, _) = server().await;
    let mut s = connect(&base).await;
    let cancel = s.cancel_handle();
    let started = Instant::now();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel().await.expect("cancel");
    });
    let err = s.execute("slow", &[]).await.err().expect("cancelled");
    assert!(matches!(err, DbError::Cancelled), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn transactions_are_refused() {
    let (base, _) = server().await;
    let mut s = connect(&base).await;
    assert!(matches!(s.begin().await, Err(DbError::Unsupported(_))));
}

#[tokio::test]
async fn introspects_tables() {
    let (base, _) = server().await;
    let mut s = connect(&base).await;
    let chunk = s
        .introspect(IntrospectScope::Objects {
            schema: "main".into(),
            kind: ObjectKind::Table,
        })
        .await
        .expect("objects");
    let CatalogChunk::Objects(objs) = chunk else {
        panic!("objects")
    };
    let names: Vec<_> = objs.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["orders", "users"]);
    let CatalogChunk::Schemas(schemas) = s
        .introspect(IntrospectScope::Schemas)
        .await
        .expect("schemas")
    else {
        panic!("schemas")
    };
    assert_eq!(schemas[0].name, "main");
}
