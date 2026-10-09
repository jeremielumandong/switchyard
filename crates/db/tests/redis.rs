//! Redis integration tests. Need the docker `redis` service (`docker compose -f
//! docker/compose.yml up -d redis`); run with `cargo test -p switchyard-db --test redis --
//! --ignored`. `SWITCHYARD_REDIS_PORT` overrides the port.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use secrecy::SecretString;
use switchyard_db::redis::{self, KeyEdit, KeyKind, KeyValue, RedisClient, browse, command};
use switchyard_db::{DbConfig, Engine};

fn cfg(db: &str) -> DbConfig {
    let mut c = DbConfig::new(Engine::Redis, "127.0.0.1", db);
    c.port = std::env::var("SWITCHYARD_REDIS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6379);
    c.password = Some(SecretString::from("switchyard"));
    c.ssl_mode = switchyard_db::SslMode::Disable;
    c
}

async fn client(db: &str) -> RedisClient {
    RedisClient::connect(&cfg(db), None).await.unwrap()
}

#[tokio::test]
#[ignore]
async fn connects_and_reports_version() {
    let c = client("3").await;
    assert!(
        c.server_version().starts_with("Redis "),
        "{}",
        c.server_version()
    );
    assert_eq!(c.db(), 3);
}

#[tokio::test]
#[ignore]
async fn wrong_password_is_refused() {
    let mut cfg = cfg("0");
    cfg.password = Some(SecretString::from("nope"));
    let e = RedisClient::connect(&cfg, None).await.err().unwrap();
    let msg = e.to_string();
    assert!(msg.contains("authentication failed"), "{msg}");
    assert!(!msg.contains("nope"));
}

#[tokio::test]
#[ignore]
async fn browses_and_edits_every_type() {
    let mut c = client("5").await;
    c.call(&["FLUSHDB"]).await.unwrap().ok().unwrap();
    c.call(&["SET", "s:1", "hello"]).await.unwrap();
    c.call(&["HSET", "h:1", "a", "1", "b", "2"]).await.unwrap();
    c.call(&["RPUSH", "l:1", "x", "y", "z"]).await.unwrap();
    c.call(&["SADD", "set:1", "m1", "m2"]).await.unwrap();
    c.call(&["ZADD", "z:1", "1.5", "one", "3", "three"])
        .await
        .unwrap();
    c.call(&["XADD", "st:1", "*", "f", "v"]).await.unwrap();
    c.call(&[b"SET".as_slice(), b"bin\xff", b"\x00\x01"])
        .await
        .unwrap();

    // Walk the whole keyspace.
    let mut keys = Vec::new();
    let mut cursor = 0;
    loop {
        let page = browse::scan(&mut c, cursor, "*", None, 2).await.unwrap();
        keys.extend(page.keys);
        cursor = page.cursor;
        if cursor == 0 {
            break;
        }
    }
    keys.sort_by(|a, b| a.key.cmp(&b.key));
    let kinds: Vec<_> = keys
        .iter()
        .map(|k| (k.key.clone(), k.kind.clone()))
        .collect();
    assert_eq!(kinds.len(), 7);
    assert!(kinds.contains(&(b"z:1".to_vec(), KeyKind::ZSet)));
    assert!(kinds.contains(&(b"bin\xff".to_vec(), KeyKind::String)));
    // SCAN … TYPE: only the sorted set, walking every page.
    let mut zsets = Vec::new();
    let mut cursor = 0;
    loop {
        let page = browse::scan(&mut c, cursor, "*", Some(&KeyKind::ZSet), 2)
            .await
            .unwrap();
        zsets.extend(page.keys.into_iter().map(|k| k.key));
        cursor = page.cursor;
        if cursor == 0 {
            break;
        }
    }
    assert_eq!(zsets, [b"z:1".to_vec()]);
    let only_h = browse::scan(&mut c, 0, "h:*", None, 100).await.unwrap();
    assert_eq!(only_h.keys.len(), 1);
    // The key list's TTL and size columns.
    assert_eq!(only_h.keys[0].ttl_ms, None);
    assert!(only_h.keys[0].memory.is_some_and(|m| m > 0));
    c.call(&["EXPIRE", "s:1", "100"]).await.unwrap();
    let s1 = browse::scan(&mut c, 0, "s:*", None, 100).await.unwrap();
    assert!(
        s1.keys[0]
            .ttl_ms
            .is_some_and(|t| t > 90_000 && t <= 100_000)
    );

    let d = browse::load(&mut c, b"h:1", 100).await.unwrap();
    assert_eq!(d.len, 2);
    assert_eq!(d.ttl_ms, None);
    let KeyValue::Hash(mut fields) = d.value else {
        panic!()
    };
    fields.sort();
    assert_eq!(
        fields,
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec())
        ]
    );

    let d = browse::load(&mut c, b"l:1", 2).await.unwrap();
    assert!(d.truncated());
    assert_eq!(d.value, KeyValue::List(vec![b"x".to_vec(), b"y".to_vec()]));

    let d = browse::load(&mut c, b"z:1", 10).await.unwrap();
    assert_eq!(
        d.value,
        KeyValue::ZSet(vec![(b"one".to_vec(), 1.5), (b"three".to_vec(), 3.0)])
    );
    let d = browse::load(&mut c, b"st:1", 10).await.unwrap();
    let KeyValue::Stream(entries) = d.value else {
        panic!()
    };
    assert_eq!(entries[0].fields, vec![(b"f".to_vec(), b"v".to_vec())]);

    // Edits.
    browse::edit(
        &mut c,
        b"h:1",
        &KeyEdit::HashSet(b"c".to_vec(), b"3".to_vec()),
        false,
    )
    .await
    .unwrap();
    browse::edit(&mut c, b"h:1", &KeyEdit::HashDelete(b"a".to_vec()), false)
        .await
        .unwrap();
    browse::edit(&mut c, b"s:1", &KeyEdit::Expire(Some(100)), false)
        .await
        .unwrap();
    let d = browse::load(&mut c, b"s:1", 10).await.unwrap();
    assert!(d.ttl_ms.is_some_and(|t| t > 0 && t <= 100_000));
    browse::edit(&mut c, b"s:1", &KeyEdit::SetString(b"bye".to_vec()), false)
        .await
        .unwrap();
    let d = browse::load(&mut c, b"s:1", 10).await.unwrap();
    assert_eq!(d.value, KeyValue::String(b"bye".to_vec()));
    assert!(d.ttl_ms.is_some(), "SET keeps the TTL");

    // New keys never overwrite; renames never clobber.
    let e = browse::edit(&mut c, b"s:1", &KeyEdit::SetString(b"x".to_vec()), true).await;
    assert!(e.is_err());
    let e = browse::edit(&mut c, b"s:1", &KeyEdit::Rename(b"h:1".to_vec()), false).await;
    assert!(e.is_err());
    browse::edit(&mut c, b"set:new", &KeyEdit::SetAdd(b"m".to_vec()), true)
        .await
        .unwrap();
    browse::edit(&mut c, b"set:new", &KeyEdit::Delete, false)
        .await
        .unwrap();
    let d = browse::load(&mut c, b"set:new", 10).await.unwrap();
    assert_eq!(d.kind, KeyKind::Missing);
    assert_eq!(browse::dbsize(&mut c).await.unwrap(), 7);
}

#[tokio::test]
#[ignore]
async fn console_runs_and_guards() {
    let mut c = client("6").await;
    let r = redis::run_console(&mut c, &command::split("SET k \"a b\"").unwrap())
        .await
        .unwrap();
    assert_eq!(redis::format_reply(&r), "OK");
    let r = redis::run_console(&mut c, &command::split("GET k").unwrap())
        .await
        .unwrap();
    assert_eq!(redis::format_reply(&r), "\"a b\"");
    // A server error keeps the connection usable.
    let r = redis::run_console(&mut c, &command::split("HGET k f").unwrap())
        .await
        .unwrap();
    assert!(redis::format_reply(&r).starts_with("(error) WRONGTYPE"));
    assert!(
        redis::run_console(&mut c, &command::split("SUBSCRIBE x").unwrap())
            .await
            .is_err()
    );
    assert!(
        redis::run_console(&mut c, &command::split("PING").unwrap())
            .await
            .is_ok()
    );

    let mut ro = cfg("6");
    ro.read_only = true;
    let mut c = RedisClient::connect(&ro, None).await.unwrap();
    assert!(
        redis::run_console(&mut c, &command::split("DEL k").unwrap())
            .await
            .is_err()
    );
    assert!(
        browse::edit(&mut c, b"k", &KeyEdit::Delete, false)
            .await
            .is_err()
    );
    assert!(
        redis::run_console(&mut c, &command::split("GET k").unwrap())
            .await
            .is_ok()
    );
}
