//! Snowflake driver against a local stand-in for the SQL API v2. Runs without network
//! access; the real service has not been exercised from this repository (see
//! docs/DECISIONS.md).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use serde_json::{Value as Json, json};
use switchyard_db::catalog::{CatalogChunk, IntrospectScope, ObjectKind};
use switchyard_db::driver::{DbAuthMethod, DbConfig, DbSession, Driver};
use switchyard_db::error::{DbError, ErrorPosition};
use switchyard_db::snowflake::SnowflakeDriver;
use switchyard_db::stream::ResultEvent;
use switchyard_db::value::{DataType, Engine, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "pat-good";

/// What the stand-in saw.
#[derive(Default)]
struct Log {
    /// (method, path, body) of every request.
    requests: Vec<(String, String, Json)>,
    /// Authorization header values.
    auth: Vec<String>,
}

type Shared = Arc<Mutex<Log>>;

fn result_set(row_type: Json, data: Json, partitions: usize, handle: &str) -> Json {
    json!({
        "code": "090001",
        "statementHandle": handle,
        "message": "Statement executed successfully.",
        "resultSetMetaData": {
            "format": "jsonv2",
            "rowType": row_type,
            "partitionInfo": (0..partitions).map(|_| json!({"rowCount": 1})).collect::<Vec<_>>(),
        },
        "data": data,
    })
}

fn rows(n: std::ops::Range<usize>) -> Json {
    Json::Array(
        n.map(|i| json!([i.to_string(), format!("row {i}")]))
            .collect(),
    )
}

fn id_name() -> Json {
    json!([
        {"name": "ID", "type": "fixed", "precision": 18, "scale": 0},
        {"name": "NAME", "type": "text"}
    ])
}

/// (status, body, gzip)
fn route(method: &str, path: &str, body: &Json) -> (u16, Json, bool) {
    if method == "POST" && path.starts_with("/api/v2/statements?requestId=") {
        let sql = body["statement"].as_str().unwrap_or_default();
        let multi = body["parameters"]["MULTI_STATEMENT_COUNT"].as_str();
        return match sql {
            "SELECT CURRENT_VERSION()" => (
                200,
                result_set(
                    json!([{"name": "V", "type": "text"}]),
                    json!([["9.30.1"]]),
                    1,
                    "h-v",
                ),
                false,
            ),
            "select big" => (
                202,
                json!({"code": "333334", "message": "Asynchronous execution in progress.", "statementHandle": "h-big"}),
                false,
            ),
            "select slow" => (
                202,
                json!({"code": "333334", "statementHandle": "h-slow"}),
                false,
            ),
            "select 1; select 2" => {
                assert_eq!(
                    multi,
                    Some("0"),
                    "several statements need MULTI_STATEMENT_COUNT=0"
                );
                (
                    200,
                    json!({"code": "090001", "statementHandles": ["h-1", "h-2"], "data": [["Multiple statements executed successfully."]], "resultSetMetaData": {"rowType": [{"name": "status", "type": "text"}]}}),
                    false,
                )
            }
            "select :1, :2" => {
                assert_eq!(multi, Some("1"));
                let b = &body["bindings"];
                (
                    200,
                    result_set(
                        json!([{"name": "A", "type": "text"}, {"name": "B", "type": "fixed", "precision": 18, "scale": 0}]),
                        json!([[b["1"]["value"], b["2"]["value"]]]),
                        1,
                        "h-b",
                    ),
                    false,
                )
            }
            "insert into t values (1)" => (
                200,
                json!({"code": "090001", "statementHandle": "h-i",
                       "resultSetMetaData": {"rowType": [{"name": "number of rows inserted", "type": "fixed", "precision": 18, "scale": 0}], "partitionInfo": [{}]},
                       "data": [["3"]], "stats": {"numRowsInserted": 3}}),
                false,
            ),
            "select boom" => (
                422,
                json!({"code": "001003", "sqlState": "42000",
                       "message": "SQL compilation error:\nsyntax error line 1 at position 7 unexpected 'boom'."}),
                false,
            ),
            "USE WAREHOUSE reporting" => (
                200,
                result_set(
                    json!([{"name": "status", "type": "text"}]),
                    json!([["Statement executed successfully."]]),
                    1,
                    "h-u",
                ),
                false,
            ),
            "select ctx" => (
                200,
                result_set(
                    json!([{"name": "W", "type": "text"}, {"name": "D", "type": "text"}]),
                    json!([[body["warehouse"], body["database"]]]),
                    1,
                    "h-c",
                ),
                false,
            ),
            s if s.contains("INFORMATION_SCHEMA.TABLES") => {
                assert_eq!(body["bindings"]["1"]["value"], "PUBLIC");
                (
                    200,
                    result_set(
                        json!([{"name": "TABLE_NAME", "type": "text"}, {"name": "ROW_COUNT", "type": "fixed", "precision": 18, "scale": 0}, {"name": "COMMENT", "type": "text"}]),
                        json!([["ORDERS", "42", null]]),
                        1,
                        "h-t",
                    ),
                    false,
                )
            }
            other => panic!("unexpected statement {other:?}"),
        };
    }
    match (method, path) {
        ("GET", "/api/v2/statements/h-big") => {
            (200, result_set(id_name(), rows(0..1000), 3, "h-big"), false)
        }
        ("GET", "/api/v2/statements/h-big?partition=1") => {
            (200, json!({"data": rows(1000..2000)}), true)
        }
        ("GET", "/api/v2/statements/h-big?partition=2") => {
            (200, json!({"data": rows(2000..2500)}), false)
        }
        ("GET", "/api/v2/statements/h-slow") => (
            202,
            json!({"code": "333334", "statementHandle": "h-slow"}),
            false,
        ),
        ("POST", "/api/v2/statements/h-slow/cancel") => (
            200,
            json!({"code": "000604", "message": "cancelled"}),
            false,
        ),
        ("GET", "/api/v2/statements/h-1") => (
            200,
            result_set(
                json!([{"name": "ONE", "type": "fixed", "precision": 1, "scale": 0}]),
                json!([["1"]]),
                1,
                "h-1",
            ),
            false,
        ),
        ("GET", "/api/v2/statements/h-2") => (
            200,
            result_set(
                json!([{"name": "TWO", "type": "fixed", "precision": 1, "scale": 0}]),
                json!([["2"]]),
                1,
                "h-2",
            ),
            false,
        ),
        _ => (
            404,
            json!({"message": format!("no route {method} {path}")}),
            false,
        ),
    }
}

async fn handle(mut sock: TcpStream, log: Shared, token: String) {
    let mut buf = Vec::new();
    loop {
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
        let header = |name: &str| {
            head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
            })
        };
        let len: usize = header("content-length")
            .and_then(|v| v.parse().ok())
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
        let mut first = head.lines().next().unwrap_or_default().split(' ');
        let method = first.next().unwrap_or_default().to_owned();
        let path = first.next().unwrap_or_default().to_owned();
        let auth = header("authorization").unwrap_or_default();
        {
            let mut l = log.lock().unwrap();
            l.requests
                .push((method.clone(), path.clone(), body.clone()));
            l.auth.push(auth.clone());
        }
        let (status, reply, gzip) = if token.is_empty() {
            // Key-pair mode: any well-formed JWT.
            if auth.starts_with("Bearer ey") && auth.matches('.').count() == 2 {
                route(&method, &path, &body)
            } else {
                (401, json!({"message": "JWT token is invalid."}), false)
            }
        } else if auth != format!("Bearer {token}") {
            (
                401,
                json!({"code": "390303", "message": "Invalid OAuth access token."}),
                false,
            )
        } else {
            route(&method, &path, &body)
        };
        let mut payload = reply.to_string().into_bytes();
        let mut extra = String::new();
        if gzip {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&payload).unwrap();
            payload = enc.finish().unwrap();
            extra.push_str("content-encoding: gzip\r\n");
        }
        let resp = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\n\r\n",
            payload.len()
        );
        if sock.write_all(resp.as_bytes()).await.is_err() || sock.write_all(&payload).await.is_err()
        {
            return;
        }
    }
}

async fn server(token: &str) -> (String, Shared) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let log = Shared::default();
    let l = log.clone();
    let token = token.to_owned();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(handle(sock, l.clone(), token.clone()));
        }
    });
    (format!("http://{addr}"), log)
}

fn config(token: &str) -> DbConfig {
    let mut cfg = DbConfig::new(Engine::Snowflake, "myorg-acct", "ANALYTICS");
    cfg.user = "reader".into();
    cfg.auth = DbAuthMethod::AccessToken;
    cfg.password = Some(SecretString::from(token.to_owned()));
    cfg.options.insert("warehouse".into(), "COMPUTE_WH".into());
    cfg
}

async fn connect(base: &str) -> Box<dyn DbSession> {
    SnowflakeDriver::with_base(base)
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

fn row_count(events: &[ResultEvent]) -> usize {
    events
        .iter()
        .map(|e| match e {
            ResultEvent::Rows(b) => b.len(),
            _ => 0,
        })
        .sum()
}

#[tokio::test]
async fn connects_and_reports_the_version() {
    let (base, log) = server(TOKEN).await;
    let s = connect(&base).await;
    assert_eq!(s.server_version(), "Snowflake 9.30.1");
    let l = log.lock().unwrap();
    let (_, _, body) = &l.requests[0];
    assert_eq!(body["warehouse"], "COMPUTE_WH");
    assert_eq!(body["database"], "ANALYTICS");
    assert!(body.get("schema").is_none(), "unset options are not sent");
}

#[tokio::test]
async fn a_bad_token_is_a_connect_error() {
    let (base, _) = server(TOKEN).await;
    let err = SnowflakeDriver::with_base(&base)
        .connect(&config("nope"), None)
        .await
        .err()
        .expect("rejected");
    assert!(
        matches!(&err, DbError::Connect(m) if m.contains("Invalid OAuth access token")),
        "{err:?}"
    );
}

#[tokio::test]
async fn passwords_are_refused_before_any_request() {
    let (base, log) = server(TOKEN).await;
    let mut cfg = config(TOKEN);
    cfg.auth = DbAuthMethod::Password;
    let err = SnowflakeDriver::with_base(&base)
        .connect(&cfg, None)
        .await
        .err()
        .expect("refused");
    assert!(matches!(&err, DbError::Connect(m) if m.contains("does not accept passwords")));
    assert!(log.lock().unwrap().requests.is_empty());
}

#[tokio::test]
async fn polls_then_streams_every_partition() {
    let (base, log) = server(TOKEN).await;
    let mut s = connect(&base).await;
    let events = collect(s.as_mut(), "select big", &[]).await;
    let ResultEvent::Columns(cols) = &events[0] else {
        panic!("columns first: {events:?}");
    };
    assert_eq!(cols[0].data_type, DataType::Int64);
    assert_eq!(cols[1].data_type, DataType::Text);
    assert_eq!(
        row_count(&events),
        2500,
        "three partitions, one gzip-compressed"
    );
    assert!(matches!(events.last(), Some(ResultEvent::Done(_))));
    let paths: Vec<String> = log
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|r| r.1.clone())
        .collect();
    assert!(paths.contains(&"/api/v2/statements/h-big?partition=2".to_owned()));
}

#[tokio::test]
async fn several_statements_are_several_result_sets() {
    let (base, _) = server(TOKEN).await;
    let mut s = connect(&base).await;
    let events = collect(s.as_mut(), "select 1; select 2", &[]).await;
    let sets = events
        .iter()
        .filter(|e| matches!(e, ResultEvent::Columns(_)))
        .count();
    let next = events
        .iter()
        .filter(|e| matches!(e, ResultEvent::NextResultSet))
        .count();
    assert_eq!((sets, next), (2, 1));
    assert_eq!(row_count(&events), 2);
}

#[tokio::test]
async fn parameters_bind_by_position() {
    let (base, _) = server(TOKEN).await;
    let mut s = connect(&base).await;
    let events = collect(
        s.as_mut(),
        "select :1, :2",
        &[Value::Text("hi".into()), Value::Int(7)],
    )
    .await;
    let ResultEvent::Rows(b) = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(b.len(), 1);
}

#[tokio::test]
async fn dml_reports_affected_rows() {
    let (base, _) = server(TOKEN).await;
    let mut s = connect(&base).await;
    let events = collect(s.as_mut(), "insert into t values (1)", &[]).await;
    let Some(ResultEvent::Done(done)) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(done.affected, Some(3));
}

#[tokio::test]
async fn compile_errors_are_server_errors_with_a_line() {
    let (base, _) = server(TOKEN).await;
    let mut s = connect(&base).await;
    let err = s.execute("select boom", &[]).await.err().expect("error");
    let server = err.as_server().expect("server error");
    assert_eq!(server.code.as_deref(), Some("001003"));
    assert_eq!(server.position, Some(ErrorPosition::Line(1)));
}

#[tokio::test]
async fn use_carries_over_to_later_requests() {
    let (base, _) = server(TOKEN).await;
    let mut s = connect(&base).await;
    collect(s.as_mut(), "USE WAREHOUSE reporting", &[]).await;
    let mut stream = s.execute("select ctx", &[]).await.expect("execute");
    let mut warehouse = None;
    while let Some(ev) = stream.next().await {
        if let ResultEvent::Rows(b) = ev.expect("event") {
            warehouse = Some(b.cell(0, 0).to_value(DataType::Text));
        }
    }
    assert_eq!(warehouse, Some(Value::Text("REPORTING".into())));
}

#[tokio::test]
async fn cancel_stops_polling_and_cancels_on_the_server() {
    let (base, log) = server(TOKEN).await;
    let mut s = connect(&base).await;
    let handle = s.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.cancel().await
    });
    let err = s
        .execute("select slow", &[])
        .await
        .err()
        .expect("cancelled");
    assert!(matches!(err, DbError::Cancelled), "{err:?}");
    canceller.await.expect("join").expect("cancel");
    assert!(
        log.lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.1 == "/api/v2/statements/h-slow/cancel")
    );
}

#[tokio::test]
async fn catalog_lists_tables_with_row_counts() {
    let (base, _) = server(TOKEN).await;
    let mut s = connect(&base).await;
    let chunk = s
        .introspect(IntrospectScope::Objects {
            schema: "PUBLIC".into(),
            kind: ObjectKind::Table,
        })
        .await
        .expect("objects");
    let CatalogChunk::Objects(objects) = chunk else {
        panic!("objects");
    };
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].name, "ORDERS");
    assert_eq!(objects[0].estimated_rows, Some(42));
}

#[tokio::test]
async fn key_pair_sign_in_sends_a_jwt() {
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("key");
    let pem = key.to_pkcs8_pem(LineEnding::LF).expect("pem");
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("rsa_key.p8");
    std::fs::write(&path, pem.as_bytes()).expect("write");

    let (base, log) = server("").await;
    let mut cfg = config("");
    cfg.auth = DbAuthMethod::KeyPair;
    cfg.password = None;
    cfg.options.insert(
        "private_key_path".into(),
        path.to_string_lossy().into_owned(),
    );
    let s = SnowflakeDriver::with_base(&base)
        .connect(&cfg, None)
        .await
        .expect("connect");
    assert_eq!(s.server_version(), "Snowflake 9.30.1");
    assert!(log.lock().unwrap().auth[0].starts_with("Bearer ey"));
}
