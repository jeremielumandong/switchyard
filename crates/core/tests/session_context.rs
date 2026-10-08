//! Per-tab database / schema switching against the docker PostgreSQL service (DBX-4a).
//! SQL Server's `USE` is covered in `switchyard-db`'s `mssql` tests (they need a server
//! certificate signed by a test CA, which core profiles cannot carry).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::db::{Engine, SslMode};
use switchyard_core::store::{DbConnection, EnvironmentLabel, Profile};
use switchyard_core::{
    Command, Core, Event, EventReceiver, FetchLimit, QueryEvent, ServiceConfig, SessionContext,
    StatementRequest,
};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

async fn next<T>(rx: &mut EventReceiver, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(t) = f(rx.next().await.expect("events")) {
                return t;
            }
        }
    })
    .await
    .expect("timed out")
}

async fn open(c: DbConnection, secret: String) -> (Core, EventReceiver) {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let id = c.id.clone();
    let h = core.handle();
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Db(c),
        secret: Some(SecretString::from(secret)),
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

async fn switch(
    core: &Core,
    rx: &mut EventReceiver,
    request: u64,
    database: Option<&str>,
    schema: Option<&str>,
) -> SessionContext {
    core.handle().send(Command::SetSessionContext {
        session: 1,
        request,
        database: database.map(str::to_owned),
        schema: schema.map(str::to_owned),
    });
    next(rx, |e| match e {
        Event::SessionContext {
            request: r, result, ..
        } if r == request => Some(result),
        _ => None,
    })
    .await
    .expect("switch")
}

/// The first cell of the first row `sql` returns.
async fn scalar(core: &Core, rx: &mut EventReceiver, query: u64, sql: &str) -> String {
    core.handle().send(Command::Execute {
        session: 1,
        query,
        statements: vec![StatementRequest {
            sql: sql.into(),
            params: vec![],
            offset: 0,
        }],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::All,
    });
    next(rx, |e| match e {
        Event::Query {
            query: q,
            event: QueryEvent::Rows(b),
        } if q == query => Some(b.cell(0, 0).to_display()),
        Event::Query {
            query: q,
            event: QueryEvent::Failed { error, .. },
        } if q == query => panic!("{error}"),
        _ => None,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker postgres"]
async fn postgres_reconnects_to_another_database_and_sets_search_path() {
    let mut c = DbConnection::new("shop", Engine::Postgres);
    c.server = env("SWITCHYARD_PG_HOST", "127.0.0.1");
    c.port = env("SWITCHYARD_PG_PORT", "5432").parse().unwrap();
    c.user = env("SWITCHYARD_PG_USER", "switchyard");
    c.database = env("SWITCHYARD_PG_DB", "shop");
    c.ssl_mode = SslMode::Disable;
    c.environment = EnvironmentLabel::Development;
    let (core, mut rx) = open(c, env("SWITCHYARD_PG_PASSWORD", "switchyard")).await;

    let ctx = switch(&core, &mut rx, 10, Some("postgres"), None).await;
    assert_eq!(ctx.database.as_deref(), Some("postgres"));
    assert_eq!(
        scalar(&core, &mut rx, 11, "SELECT current_database()").await,
        "postgres"
    );
    let ctx = switch(&core, &mut rx, 12, None, Some("pg_catalog")).await;
    assert_eq!(ctx.schema.as_deref(), Some("pg_catalog"));
    assert_eq!(
        scalar(&core, &mut rx, 13, "SELECT current_schema()").await,
        "pg_catalog"
    );
}
