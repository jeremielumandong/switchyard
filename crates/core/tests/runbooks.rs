//! Runbook steps against the core service: a SQL step on PostgreSQL (needs the docker
//! services), plus storage, secrets and a local shell step (no network).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::db::Engine;
use switchyard_core::runbook::ParamValues;
use switchyard_core::store::{
    DbConnection, EnvironmentLabel, Profile, Runbook, RunbookParam, RunbookStep, ShellTarget,
    StepKind,
};
use switchyard_core::{Command, Core, Event, EventReceiver, QueryEvent, ServiceConfig};

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

async fn save(
    core: &Core,
    rx: &mut EventReceiver,
    rb: Runbook,
    secrets: Vec<(String, SecretString)>,
) -> Runbook {
    core.handle().send(Command::SaveRunbook {
        request: 7,
        runbook: rb,
        secrets,
    });
    next(rx, |e| match e {
        Event::RunbookSaved { request: 7, result } => Some(result.unwrap()),
        _ => None,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_step_uses_stored_secret_and_records_history() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let mut rb = Runbook::new("Echo, test");
    rb.params = vec![
        RunbookParam::new("who", "world"),
        RunbookParam::secret("pw"),
    ];
    rb.steps = vec![RunbookStep::new(
        "say",
        StepKind::Shell {
            target: ShellTarget::Local,
            command: "echo hello {{who}} {{pw}}".into(),
            timeout_secs: 10,
        },
    )];
    let rb = save(
        &core,
        &mut rx,
        rb,
        vec![("pw".into(), SecretString::from("p4ss"))],
    )
    .await;
    assert!(!rb.id.is_empty());

    let h = core.handle();
    h.send(Command::RunbookStep {
        query: 50,
        session: None,
        runbook: rb.clone(),
        step: 0,
        values: ParamValues::new(),
        confirmed_destructive: false,
    });
    let mut output = String::new();
    let mut resolved = String::new();
    let result = next(&mut rx, |e| match e {
        Event::RunbookResolved { query: 50, text } => {
            resolved = text;
            None
        }
        Event::RunbookOutput { query: 50, text } => {
            output.push_str(&text);
            None
        }
        Event::RunbookStepDone {
            query: 50, result, ..
        } => Some(result),
        _ => None,
    })
    .await;
    if cfg!(unix) {
        assert_eq!(result, Ok("exit 0".into()));
        assert!(output.contains("hello world"), "{output}");
        assert!(!output.contains("p4ss"), "secrets are masked in output");
    }
    assert!(resolved.starts_with("echo hello world ") && !resolved.contains("p4ss"));

    h.send(Command::SearchHistory {
        request: 3,
        query: "runbook:Echo".into(),
        connection: None,
    });
    let entries = next(&mut rx, |e| match e {
        Event::History {
            request: 3,
            entries,
        } => Some(entries),
        _ => None,
    })
    .await;
    assert_eq!(entries.len(), 1);
    assert!(entries[0].tags.contains(&"runbook".to_owned()));
    assert!(entries[0].tags.contains(&"shell".to_owned()));
    assert!(!entries[0].sql.contains("p4ss"));

    // A secret with no stored value and none entered is an error, not an empty string.
    let mut other = rb.clone();
    other.id = String::new();
    h.send(Command::RunbookStep {
        query: 51,
        session: None,
        runbook: other,
        step: 0,
        values: ParamValues::new(),
        confirmed_destructive: false,
    });
    let r = next(&mut rx, |e| match e {
        Event::RunbookStepDone {
            query: 51, result, ..
        } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.unwrap_err().contains("{{pw}}"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker postgres"]
async fn sql_step_runs_on_postgres_with_bound_secret() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let mut c = DbConnection::new("shop", Engine::Postgres);
    c.server = env("SWITCHYARD_PG_HOST", "127.0.0.1");
    c.port = env("SWITCHYARD_PG_PORT", "5432").parse().unwrap();
    c.user = env("SWITCHYARD_PG_USER", "switchyard");
    c.database = env("SWITCHYARD_PG_DB", "shop");
    c.ssl_mode = switchyard_core::db::SslMode::Disable;
    c.environment = EnvironmentLabel::Development;
    let conn = c.id.clone();
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
        connection: conn.clone(),
    });
    next(&mut rx, |e| match e {
        Event::SessionOpened { .. } => Some(()),
        Event::SessionFailed { message, .. } => panic!("{message}"),
        _ => None,
    })
    .await;

    let mut rb = Runbook::new("Pg check");
    rb.params = vec![
        RunbookParam::new("label", "it's"),
        RunbookParam::secret("token"),
    ];
    rb.steps = vec![RunbookStep::new(
        "probe",
        StepKind::Sql {
            connection: Some(conn),
            sql: "SELECT {{label}} AS label, {{token}}::text AS token;".into(),
        },
    )];
    let rb = save(&core, &mut rx, rb, Vec::new()).await;
    h.send(Command::RunbookStep {
        query: 60,
        session: Some(1),
        runbook: rb,
        step: 0,
        values: ParamValues::new().secret("token", SecretString::from("t0k")),
        confirmed_destructive: false,
    });
    let mut resolved = String::new();
    let mut cells: Vec<String> = Vec::new();
    let failed = next(&mut rx, |e| match e {
        Event::RunbookResolved { query: 60, text } => {
            resolved = text;
            None
        }
        Event::RunbookStepDone {
            query: 60, result, ..
        } => panic!("step did not run: {result:?}"),
        Event::Query { query: 60, event } => match event {
            QueryEvent::Rows(b) => {
                for c in 0..b.column_count() {
                    cells.push(b.cell(0, c).to_display());
                }
                None
            }
            QueryEvent::Failed { error, .. } => Some(Some(error.to_string())),
            QueryEvent::Finished { .. } => Some(None),
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(failed, None);
    assert_eq!(cells, ["it's", "t0k"]);
    assert!(resolved.contains("'it''s'"), "{resolved}");
    assert!(
        resolved.contains("$1") && !resolved.contains("t0k"),
        "{resolved}"
    );

    h.send(Command::SearchHistory {
        request: 4,
        query: "runbook:Pg".into(),
        connection: None,
    });
    let entries = next(&mut rx, |e| match e {
        Event::History {
            request: 4,
            entries,
        } => Some(entries),
        _ => None,
    })
    .await;
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].sql.contains("t0k"));
}
