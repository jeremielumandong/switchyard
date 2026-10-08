//! Activity monitor against the docker PostgreSQL and SQL Server (`docker/compose.yml`);
//! same overrides as `pg.rs` and `mssql.rs`.
//! Run with `cargo test -p switchyard-db --test activity -- --ignored postgres` (the
//! `sql_server` test needs `SWITCHYARD_MSSQL_CA`, see `scripts/mssql-test-server.sh`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::activity::{self, ActivityAction, ActivityError, SessionTarget};
use switchyard_db::pg::PgDriver;
use switchyard_db::{DbConfig, DbError, DbSession, Driver, Engine, SslMode};

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

#[tokio::test]
#[ignore = "needs docker postgres"]
async fn postgres_lists_and_cancels_a_sleeping_query() {
    let mut monitor = session().await;
    let mut sleeper = session().await;
    let marker = format!("swy_activity_test_{}", std::process::id());
    let sql = format!("SELECT pg_sleep(30) AS {marker}");
    let task = tokio::spawn(async move {
        let mut stream = sleeper.execute(&sql, &[]).await?;
        while let Some(ev) = stream.next().await {
            ev?;
        }
        Ok::<_, DbError>(())
    });

    // Wait until the sleep shows up as running.
    let version = monitor.server_version();
    let deadline = Instant::now() + Duration::from_secs(10);
    let target = loop {
        let a = activity::list(monitor.as_mut(), Engine::Postgres, &version)
            .await
            .unwrap();
        assert!(a.can_cancel && a.can_terminate);
        assert!(a.sessions.iter().any(|s| s.is_self), "own session listed");
        if let Some(s) = a.sessions.iter().find(|s| {
            s.running && !s.is_self && s.sql.as_deref().is_some_and(|q| q.contains(&marker))
        }) {
            assert!(s.duration_ms.is_some());
            assert!(s.user.is_some());
            break s.target.clone().expect("validated target");
        }
        assert!(Instant::now() < deadline, "pg_sleep never listed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // The monitor refuses to act on itself.
    let own = activity::list(monitor.as_mut(), Engine::Postgres, &version)
        .await
        .unwrap()
        .sessions
        .into_iter()
        .find(|s| s.is_self)
        .and_then(|s| s.target)
        .expect("own target");
    assert!(matches!(
        activity::act(
            monitor.as_mut(),
            Engine::Postgres,
            ActivityAction::Terminate,
            &own
        )
        .await,
        Err(ActivityError::OwnSession)
    ));

    let msg = activity::act(
        monitor.as_mut(),
        Engine::Postgres,
        ActivityAction::CancelQuery,
        &target,
    )
    .await
    .unwrap();
    assert!(msg.contains("Cancel sent"), "{msg}");

    let r = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("cancelled in time")
        .unwrap();
    let err = r.expect_err("the sleep was cancelled");
    let code = err.as_server().and_then(|s| s.code.clone());
    assert_eq!(code.as_deref(), Some("57014"), "{err}");

    // A pid that does not exist is reported, not an error.
    let gone = SessionTarget::Backend {
        id: i32::MAX as i64,
    };
    let msg = activity::act(
        monitor.as_mut(),
        Engine::Postgres,
        ActivityAction::CancelQuery,
        &gone,
    )
    .await
    .unwrap();
    assert!(msg.contains("not signalled"), "{msg}");
}

fn mssql_config() -> DbConfig {
    let mut cfg = DbConfig::new(
        Engine::SqlServer,
        env("SWITCHYARD_MSSQL_HOST", "localhost"),
        "master",
    );
    cfg.port = env("SWITCHYARD_MSSQL_PORT", "1433").parse().unwrap();
    cfg.user = env("SWITCHYARD_MSSQL_USER", "sa");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_MSSQL_PASSWORD",
        "Switchyard!2026",
    )));
    cfg.ssl_mode = SslMode::Prefer;
    cfg.trusted_ca_pem = std::env::var("SWITCHYARD_MSSQL_CA")
        .ok()
        .map(|p| std::fs::read_to_string(p).expect("CA file"));
    cfg
}

#[tokio::test]
#[ignore = "needs docker mssql"]
async fn sql_server_lists_and_kills_a_waiting_session() {
    use switchyard_db::mssql::MssqlDriver;
    let mut monitor = MssqlDriver.connect(&mssql_config(), None).await.unwrap();
    let mut sleeper = MssqlDriver.connect(&mssql_config(), None).await.unwrap();
    let marker = format!("swy_activity_test_{}", std::process::id());
    let sql = format!("/* {marker} */ WAITFOR DELAY '00:00:30'");
    let task = tokio::spawn(async move {
        let mut stream = sleeper.execute(&sql, &[]).await?;
        while let Some(ev) = stream.next().await {
            ev?;
        }
        Ok::<_, DbError>(())
    });
    let version = monitor.server_version();
    let deadline = Instant::now() + Duration::from_secs(10);
    let target = loop {
        let a = activity::list(monitor.as_mut(), Engine::SqlServer, &version)
            .await
            .unwrap();
        assert!(!a.can_cancel && a.can_terminate);
        assert!(a.sessions.iter().any(|s| s.is_self));
        if let Some(s) = a.sessions.iter().find(|s| {
            s.running && !s.is_self && s.sql.as_deref().is_some_and(|q| q.contains(&marker))
        }) {
            assert!(s.wait.is_some(), "{s:?}");
            break s.target.clone().expect("validated target");
        }
        assert!(Instant::now() < deadline, "WAITFOR never listed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(matches!(
        activity::act(
            monitor.as_mut(),
            Engine::SqlServer,
            ActivityAction::CancelQuery,
            &target
        )
        .await,
        Err(ActivityError::Unsupported(_))
    ));
    let msg = activity::act(
        monitor.as_mut(),
        Engine::SqlServer,
        ActivityAction::Terminate,
        &target,
    )
    .await
    .unwrap();
    assert!(msg.contains("ended"), "{msg}");
    let r = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("killed in time")
        .unwrap();
    assert!(r.is_err(), "the killed session's batch fails");
}
