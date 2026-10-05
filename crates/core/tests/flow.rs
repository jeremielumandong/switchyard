//! End-to-end core flows against the mock driver (no network).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::db::Engine;
use switchyard_core::db::mock::MockDriver;
use switchyard_core::store::{DbConnection, EnvironmentLabel, Profile};
use switchyard_core::{
    Command, Core, Event, EventReceiver, FetchLimit, QueryEvent, ServiceConfig, StatementRequest,
};

fn start() -> (Core, EventReceiver) {
    let mut cfg = ServiceConfig::in_memory();
    cfg.extra_drivers
        .push((Engine::Postgres, Arc::new(MockDriver)));
    Core::start(cfg).unwrap()
}

async fn next_matching<T>(rx: &mut EventReceiver, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = rx.next().await.expect("event stream ended");
            if let Some(t) = f(ev) {
                return t;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn stmt(sql: &str) -> StatementRequest {
    StatementRequest {
        sql: sql.into(),
        params: vec![],
        offset: 0,
    }
}

async fn setup(env: EnvironmentLabel, read_only: bool) -> (Core, EventReceiver) {
    let (core, mut rx) = start();
    let h = core.handle();
    let mut c = DbConnection::new("shop", Engine::Postgres);
    c.user = "app".into();
    c.database = "shop".into();
    c.environment = env;
    c.read_only = read_only;
    let id = c.id.clone();
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Db(c),
        secret: Some(SecretString::from("pw")),
    });
    next_matching(&mut rx, |e| {
        matches!(e, Event::ProfileSaved { request: 1, .. }).then_some(())
    })
    .await;
    h.send(Command::OpenSession {
        session: 7,
        connection: id,
    });
    next_matching(&mut rx, |e| {
        matches!(e, Event::SessionOpened { session: 7, .. }).then_some(())
    })
    .await;
    (core, rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_with_fetch_limit_and_fetch_all() {
    let (core, mut rx) = setup(EnvironmentLabel::Development, false).await;
    let h = core.handle();
    h.send(Command::Execute {
        session: 7,
        query: 1,
        statements: vec![stmt("rows 25000")],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::Rows(10_000),
    });
    let mut rows = 0;
    let paused_at = next_matching(&mut rx, |e| match e {
        Event::Query {
            event: QueryEvent::Rows(b),
            ..
        } => {
            rows += b.len();
            None
        }
        Event::Query {
            event: QueryEvent::Paused { rows },
            ..
        } => Some(rows),
        _ => None,
    })
    .await;
    assert_eq!(paused_at, 10_000);
    h.send(Command::FetchMore {
        query: 1,
        all: true,
    });
    next_matching(&mut rx, |e| match e {
        Event::Query {
            event: QueryEvent::Rows(b),
            ..
        } => {
            rows += b.len();
            None
        }
        Event::Query {
            event: QueryEvent::Finished { cancelled, .. },
            ..
        } => {
            assert!(!cancelled);
            Some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(rows, 25_000);

    // The statement landed in history.
    h.send(Command::SearchHistory {
        request: 9,
        query: "rows".into(),
        connection: None,
    });
    let entries = next_matching(&mut rx, |e| {
        if let Event::History { entries, .. } = e {
            Some(entries)
        } else {
            None
        }
    })
    .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].rows, Some(25_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_running_query_quickly() {
    let (core, mut rx) = setup(EnvironmentLabel::Development, false).await;
    let h = core.handle();
    h.send(Command::Execute {
        session: 7,
        query: 2,
        statements: vec![stmt("sleep 30000"), stmt("rows 5")],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::All,
    });
    next_matching(&mut rx, |e| {
        matches!(
            e,
            Event::Query {
                event: QueryEvent::StatementStarted { index: 0 },
                ..
            }
        )
        .then_some(())
    })
    .await;
    let t = std::time::Instant::now();
    h.send(Command::Cancel { query: 2 });
    let cancelled = next_matching(&mut rx, |e| match e {
        Event::Query {
            event: QueryEvent::Finished { cancelled, .. },
            ..
        } => Some(cancelled),
        Event::Query {
            event: QueryEvent::StatementStarted { index: 1 },
            ..
        } => {
            panic!("second statement must not run")
        }
        _ => None,
    })
    .await;
    assert!(cancelled);
    assert!(t.elapsed() < Duration::from_millis(500));
}

#[tokio::test(flavor = "multi_thread")]
async fn production_requires_confirmation() {
    let (core, mut rx) = setup(EnvironmentLabel::Production, false).await;
    let h = core.handle();
    h.send(Command::Execute {
        session: 7,
        query: 3,
        statements: vec![stmt("DELETE FROM abandoned_carts")],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::All,
    });
    let d = next_matching(&mut rx, |e| match e {
        Event::Query {
            event: QueryEvent::NeedsConfirmation { destructive, .. },
            ..
        } => Some(destructive),
        Event::Query {
            event: QueryEvent::StatementStarted { .. },
            ..
        } => panic!("must not run"),
        _ => None,
    })
    .await;
    assert_eq!(d[0].objects, ["abandoned_carts"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_blocks_writes() {
    let (core, mut rx) = setup(EnvironmentLabel::Production, true).await;
    let h = core.handle();
    h.send(Command::Execute {
        session: 7,
        query: 4,
        statements: vec![stmt("UPDATE t SET a = 1 WHERE id = 1")],
        tags: vec![],
        confirmed_destructive: true,
        fetch_limit: FetchLimit::All,
    });
    let msg = next_matching(&mut rx, |e| match e {
        Event::Query {
            event: QueryEvent::Failed { error, .. },
            ..
        } => Some(error.to_string()),
        Event::Query {
            event: QueryEvent::StatementStarted { .. },
            ..
        } => panic!("must not run"),
        _ => None,
    })
    .await;
    assert!(msg.contains("read-only"), "{msg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_profile_reports_field() {
    let (core, mut rx) = start();
    let mut c = DbConnection::new("", Engine::Postgres);
    c.user = "x".into();
    core.handle().send(Command::SaveProfile {
        request: 5,
        profile: Profile::Db(c),
        secret: None,
    });
    let field = next_matching(&mut rx, |e| match e {
        Event::ProfileError {
            request: 5, field, ..
        } => Some(field),
        _ => None,
    })
    .await;
    assert_eq!(field, Some("name"));
}
