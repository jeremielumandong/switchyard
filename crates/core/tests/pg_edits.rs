//! Inline edits against PostgreSQL (needs the docker services or a seeded server).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::db::Engine;
use switchyard_core::store::{DbConnection, EnvironmentLabel, Profile};
use switchyard_core::{Command, Core, Event, EventReceiver, ServiceConfig};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

async fn next<T>(rx: &mut EventReceiver, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = f(rx.next().await.expect("events")) {
                return t;
            }
        }
    })
    .await
    .expect("timed out")
}

async fn open() -> (Core, EventReceiver) {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let mut c = DbConnection::new("shop", Engine::Postgres);
    c.server = env("SWITCHYARD_PG_HOST", "127.0.0.1");
    c.port = env("SWITCHYARD_PG_PORT", "5432").parse().unwrap();
    c.user = env("SWITCHYARD_PG_USER", "switchyard");
    c.database = env("SWITCHYARD_PG_DB", "shop");
    c.ssl_mode = switchyard_core::db::SslMode::Disable;
    c.environment = EnvironmentLabel::Development;
    let id = c.id.clone();
    let h = core.handle();
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Db(c),
        secret: Some(SecretString::from(env(
            "SWITCHYARD_PG_PASSWORD",
            "switchyard",
        ))),
    });
    next(&mut rx, |e| {
        matches!(e, Event::ProfileSaved { .. }).then_some(())
    })
    .await;
    h.send(Command::OpenSession {
        session: 1,
        connection: id,
    });
    next(&mut rx, |e| match e {
        Event::SessionOpened { .. } => Some(()),
        Event::SessionFailed { message, .. } => panic!("{message}"),
        _ => None,
    })
    .await;
    (core, rx)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker postgres"]
async fn edits_commit_and_roll_back_atomically() {
    let (core, mut rx) = open().await;
    let h = core.handle();
    let stmts = vec![
        "UPDATE customers SET segment = 'edit-test' WHERE id = 7;".to_owned(),
        "UPDATE customers SET segment = 'edit-test' WHERE id = 8;".to_owned(),
    ];
    h.send(Command::ApplyEdits {
        session: 1,
        request: 2,
        statements: stmts,
    });
    let r = next(&mut rx, |e| match e {
        Event::EditsApplied {
            request: 2, result, ..
        } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r, Ok(2));

    // A statement that matches no row rolls the whole batch back.
    let stmts = vec![
        "UPDATE customers SET segment = 'should-not-stick' WHERE id = 7;".to_owned(),
        "UPDATE customers SET segment = 'x' WHERE id = -1;".to_owned(),
    ];
    h.send(Command::ApplyEdits {
        session: 1,
        request: 3,
        statements: stmts,
    });
    let r = next(&mut rx, |e| match e {
        Event::EditsApplied {
            request: 3, result, ..
        } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.unwrap_err().contains("changed 0"));

    // Check and restore through a plain query.
    h.send(Command::Execute {
        session: 1,
        query: 9,
        statements: vec![switchyard_core::StatementRequest {
            sql: "SELECT segment FROM customers WHERE id = 7".into(),
            params: vec![],
            offset: 0,
        }],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: switchyard_core::FetchLimit::All,
    });
    let seg = next(&mut rx, |e| match e {
        Event::Query {
            event: switchyard_core::QueryEvent::Rows(b),
            ..
        } => Some(b.cell(0, 0).to_display()),
        _ => None,
    })
    .await;
    assert_eq!(seg, "edit-test");
    h.send(Command::ApplyEdits {
        session: 1,
        request: 4,
        statements: vec![
            "UPDATE customers SET segment = 'new' WHERE id = 7;".into(),
            "UPDATE customers SET segment = 'vip' WHERE id = 8;".into(),
        ],
    });
    next(&mut rx, |e| {
        matches!(e, Event::EditsApplied { request: 4, .. }).then_some(())
    })
    .await;
}
