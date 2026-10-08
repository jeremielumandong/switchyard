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
    setup_with(env, read_only, true).await
}

async fn setup_with(
    env: EnvironmentLabel,
    read_only: bool,
    history: bool,
) -> (Core, EventReceiver) {
    let (core, mut rx) = start();
    let h = core.handle();
    let mut c = DbConnection::new("shop", Engine::Postgres);
    c.user = "app".into();
    c.database = "shop".into();
    c.environment = env;
    c.read_only = read_only;
    c.history_enabled = history;
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

#[tokio::test(flavor = "multi_thread")]
async fn history_off_switch_records_nothing() {
    let (core, mut rx) = setup_with(EnvironmentLabel::Development, false, false).await;
    let h = core.handle();
    h.send(Command::Execute {
        session: 7,
        query: 1,
        statements: vec![stmt("rows 3")],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::Rows(10_000),
    });
    next_matching(&mut rx, |e| {
        matches!(
            e,
            Event::Query {
                event: QueryEvent::Finished { .. },
                ..
            }
        )
        .then_some(())
    })
    .await;
    h.send(Command::SearchHistory {
        request: 9,
        query: String::new(),
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
    assert!(entries.is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn local_terminal_round_trip() {
    use switchyard_core::TermTarget;
    use switchyard_core::term::TermSize;

    let (core, mut rx) = start();
    let h = core.handle();
    h.send(Command::OpenTerminal {
        term: 5,
        target: TermTarget::Local { profile: None },
        size: TermSize { cols: 60, rows: 10 },
    });
    let terminal = next_matching(&mut rx, |e| match e {
        Event::TerminalOpened {
            term: 5, terminal, ..
        } => Some(terminal),
        Event::TerminalFailed { message, .. } => panic!("{message}"),
        _ => None,
    })
    .await;
    // One command per byte: they must arrive in order.
    for b in b"echo out-$((6*7)); exit 4\r" {
        h.send(Command::TerminalInput {
            term: 5,
            bytes: vec![*b],
        });
    }
    let code = next_matching(&mut rx, |e| match e {
        Event::TerminalExited { term: 5, code, .. } => Some(code),
        _ => None,
    })
    .await;
    assert_eq!(code, Some(4));
    let snap = terminal.snapshot();
    assert!(
        snap.lines
            .iter()
            .any(|l| l.text.trim_end().ends_with("out-42")),
        "{:?}",
        snap.lines
            .iter()
            .map(|l| l.text.trim_end())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn driver_manager_commands_report_on_the_bus() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ServiceConfig::in_memory();
    cfg.drivers_dir = dir.path().join("drivers");
    let (core, mut rx) = Core::start(cfg).unwrap();
    let h = core.handle();

    h.send(Command::DetectComponents);
    let comps = next_matching(&mut rx, |e| match e {
        Event::Components(c) => Some(c),
        _ => None,
    })
    .await;
    let ids: Vec<_> = comps.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "oracle-instant-client",
            "gssapi",
            "ssh-agent",
            "x-server",
            "claude-code",
            "codex-cli",
            "gemini-cli"
        ]
    );

    // A path without the library is refused with a reason.
    h.send(Command::UseComponentPath {
        id: "gssapi".into(),
        path: dir.path().to_owned(),
    });
    let (id, message) = next_matching(&mut rx, |e| match e {
        Event::ComponentFailed { id, message, .. } => Some((id, message)),
        _ => None,
    })
    .await;
    assert_eq!(id, "gssapi");
    assert!(
        message.contains("does not contain libgssapi_krb5.so.2"),
        "{message}"
    );

    // A folder holding the library is accepted and remembered.
    let lib = dir.path().join("krb/libgssapi_krb5.so.2");
    std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
    std::fs::write(&lib, b"").unwrap();
    h.send(Command::UseComponentPath {
        id: "gssapi".into(),
        path: lib.parent().unwrap().to_owned(),
    });
    let c = next_matching(&mut rx, |e| match e {
        Event::ComponentInstalled { component } => Some(component),
        _ => None,
    })
    .await;
    assert!(
        matches!(
            &c.status,
            switchyard_core::drivers::ComponentStatus::Installed {
                source: switchyard_core::drivers::Source::UserPath,
                ..
            }
        ),
        "{:?}",
        c.status
    );
    assert!(dir.path().join("drivers/paths.json").is_file());

    // Manual-only components cannot be "installed".
    h.send(Command::InstallComponent {
        id: if cfg!(target_os = "macos") {
            "nope"
        } else {
            "ssh-agent"
        }
        .into(),
        accept_license: false,
    });
    next_matching(&mut rx, |e| match e {
        Event::ComponentFailed { .. } => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn transfer_queue_runs_four_at_a_time() {
    use switchyard_core::{FsRef, OnConflict};
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    for i in 0..6 {
        std::fs::write(dir.path().join(format!("f{i}.bin")), vec![1u8; 20_000_000]).unwrap();
    }
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    for i in 0..6u64 {
        h.send(Command::Transfer {
            id: 100 + i,
            from: FsRef::Local,
            path: dir.path().join(format!("f{i}.bin")),
            to: FsRef::Local,
            dir: Some(out.clone()),
            on_conflict: OnConflict::Ask,
            resume: false,
        });
    }
    let mut queued = 0;
    let mut done = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        while done < 6 {
            match rx.next().await.unwrap() {
                Event::TransferQueued { .. } => queued += 1,
                Event::TransferDone { result, .. } => {
                    result.unwrap();
                    done += 1;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(queued, 2, "four run, two wait");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn switches_database_and_schema_in_place() {
    use switchyard_core::SessionContext;
    let (core, mut rx) = setup(EnvironmentLabel::Development, false).await;
    let h = core.handle();
    // PostgreSQL reconnects the same session id to the other database, then sets the
    // search path on the new connection.
    h.send(Command::SetSessionContext {
        session: 7,
        request: 40,
        database: Some("analytics".into()),
        schema: Some("sales".into()),
    });
    let ctx = next_matching(&mut rx, |e| match e {
        Event::SessionContext {
            session: 7,
            request: 40,
            result,
        } => Some(result),
        _ => None,
    })
    .await
    .unwrap();
    assert_eq!(
        ctx,
        SessionContext {
            database: Some("analytics".into()),
            schema: Some("sales".into()),
        }
    );
    // The session still runs queries.
    h.send(Command::Execute {
        session: 7,
        query: 41,
        statements: vec![stmt("rows 3")],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::All,
    });
    let cancelled = next_matching(&mut rx, |e| match e {
        Event::Query {
            query: 41,
            event: QueryEvent::Finished { cancelled, .. },
        } => Some(cancelled),
        _ => None,
    })
    .await;
    assert!(!cancelled);
    // Switching inside a manual transaction is refused.
    h.send(Command::Begin { session: 7 });
    next_matching(&mut rx, |e| {
        matches!(e, Event::Transaction { session: 7, .. }).then_some(())
    })
    .await;
    h.send(Command::SetSessionContext {
        session: 7,
        request: 42,
        database: Some("shop".into()),
        schema: None,
    });
    let r = next_matching(&mut rx, |e| match e {
        Event::SessionContext {
            request: 42,
            result,
            ..
        } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.is_err());
}
