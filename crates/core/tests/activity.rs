//! Activity monitor guards in core (DBX-5b), against the mock driver (no network).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::db::Engine;
use switchyard_core::db::activity::{ActivityAction, SessionTarget};
use switchyard_core::db::mock::MockDriver;
use switchyard_core::store::{DbConnection, EnvironmentLabel, HistoryEntry, Profile, ProfileId};
use switchyard_core::{Command, Core, Event, EventReceiver, ServiceConfig};

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

/// A core with one connection (history off) whose monitor session 7 is open.
async fn setup(env: EnvironmentLabel) -> (Core, EventReceiver, ProfileId) {
    let mut cfg = ServiceConfig::in_memory();
    cfg.extra_drivers
        .push((Engine::Postgres, Arc::new(MockDriver)));
    let (core, mut rx) = Core::start(cfg).unwrap();
    let h = core.handle();
    let mut c = DbConnection::new("shop", Engine::Postgres);
    c.user = "app".into();
    c.database = "shop".into();
    c.environment = env;
    c.history_enabled = false;
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
    // The first listing opens the monitor's own session.
    h.send(Command::Activity {
        session: 7,
        connection: id.clone(),
        request: 2,
    });
    let r = next_matching(&mut rx, |e| match e {
        Event::Activity { request: 2, result } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.is_ok(), "{r:?}");
    (core, rx, id)
}

async fn history(core: &Core, rx: &mut EventReceiver) -> Vec<HistoryEntry> {
    core.handle().send(Command::SearchHistory {
        request: 9,
        query: String::new(),
        connection: None,
    });
    next_matching(rx, |e| match e {
        Event::History { entries, .. } => Some(entries),
        _ => None,
    })
    .await
}

async fn act(
    core: &Core,
    rx: &mut EventReceiver,
    id: i64,
    confirmed: bool,
) -> Result<String, String> {
    core.handle().send(Command::SessionAction {
        session: 7,
        request: 3,
        action: ActivityAction::Terminate,
        target: SessionTarget::Backend { id },
        confirmed,
    });
    next_matching(rx, |e| match e {
        Event::SessionAction { request: 3, result } => Some(result),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn production_needs_the_typed_confirmation() {
    let (core, mut rx, _) = setup(EnvironmentLabel::Production).await;
    let r = act(&core, &mut rx, 4242, false).await;
    assert!(r.unwrap_err().contains("Production"));
    // Refused before anything ran: nothing to record.
    assert!(history(&core, &mut rx).await.is_empty());
}

#[tokio::test]
async fn every_action_is_recorded_even_with_history_off() {
    let (core, mut rx, id) = setup(EnvironmentLabel::Development).await;
    // The mock answers every query with ids starting at 1, so its own session is 1.
    let r = act(&core, &mut rx, 1, false).await;
    assert!(r.unwrap_err().contains("own session"));
    let r = act(&core, &mut rx, 4242, false).await;
    assert!(r.is_ok(), "{r:?}");
    let h = history(&core, &mut rx).await;
    assert_eq!(h.len(), 2);
    assert!(h.iter().all(|e| e.tags == vec!["activity".to_owned()]));
    assert!(h.iter().all(|e| e.connection_id.as_ref() == Some(&id)));
    assert!(h.iter().any(|e| {
        e.error
            .as_deref()
            .is_some_and(|m| m.contains("own session"))
    }));
    assert!(
        h.iter()
            .any(|e| e.sql.contains("pg_terminate_backend(4242)")),
        "{h:?}"
    );
}
