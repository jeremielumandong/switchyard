//! PostgreSQL through an SSH tunnel: query, cancel, sharing and stopping. Needs the local
//! SSH test servers (`scripts/ssh-test-servers.sh`) on a machine that also runs the seeded
//! PostgreSQL on 127.0.0.1:5432. Run with
//! `cargo test -p switchyard-core --test tunnel_pg -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::db::Engine;
use switchyard_core::remote::ssh::{HostKeyDecision, TunnelStatus};
use switchyard_core::store::{DbConnection, Host, Profile, SshAuth};
use switchyard_core::{
    Command, Core, Event, EventReceiver, FetchLimit, PromptAnswer, QueryEvent, ServiceConfig,
    StatementRequest,
};

async fn next<T>(rx: &mut EventReceiver, secs: u64, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            if let Some(t) = f(rx.next().await.expect("events")) {
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

fn keys() -> String {
    std::env::var("SWITCHYARD_SSH_KEYS").unwrap_or_else(|_| "/tmp/switchyard-ssh".into())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ssh servers and postgres"]
async fn postgres_through_a_tunnel() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();

    let mut host = Host::new("bastion", "127.0.0.1", "swy");
    host.port = 2222;
    host.auth = SshAuth::PublicKey {
        key_path: format!("{}/id_ed25519", keys()),
    };
    let host_id = host.id.clone();
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Host(host),
        secret: None,
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 1, .. }).then_some(())
    })
    .await;

    let mut db = DbConnection::new("shop_via_bastion", Engine::Postgres);
    db.server = "127.0.0.1".into();
    db.database = "shop".into();
    db.user = "switchyard".into();
    db.via_host = Some(host_id);
    let db_id = db.id.clone();
    h.send(Command::SaveProfile {
        request: 2,
        profile: Profile::Db(db),
        secret: Some(SecretString::from("switchyard".to_owned())),
    });
    next(&mut rx, 5, |e| {
        matches!(e, Event::ProfileSaved { request: 2, .. }).then_some(())
    })
    .await;

    // First session: the Host's key is new, so the tunnel asks.
    h.send(Command::OpenSession {
        session: 1,
        connection: db_id.clone(),
    });
    let request = next(&mut rx, 10, |e| match e {
        Event::HostKeyPrompt { request, .. } => Some(request),
        Event::SessionFailed { message, .. } => panic!("{message}"),
        _ => None,
    })
    .await;
    h.send(Command::AnswerPrompt {
        request,
        answer: PromptAnswer::HostKey(HostKeyDecision::TrustOnce),
    });
    let version = next(&mut rx, 10, |e| match e {
        Event::SessionOpened {
            session: 1,
            server_version,
        } => Some(server_version),
        Event::SessionFailed { message, .. } => panic!("{message}"),
        _ => None,
    })
    .await;
    assert!(version.starts_with("PostgreSQL"), "{version}");

    // A second session to the same target reuses the tunnel (and the login).
    h.send(Command::OpenSession {
        session: 2,
        connection: db_id,
    });
    next(&mut rx, 10, |e| match e {
        Event::SessionOpened { session: 2, .. } => Some(()),
        Event::HostKeyPrompt { .. } => panic!("the shared session must not prompt again"),
        Event::SessionFailed { message, .. } => panic!("{message}"),
        _ => None,
    })
    .await;
    h.send(Command::ListTunnels);
    let tunnels = next(&mut rx, 5, |e| match e {
        Event::Tunnels(t) => Some(t),
        _ => None,
    })
    .await;
    assert_eq!(tunnels.len(), 1, "{tunnels:?}");
    assert_eq!(tunnels[0].remote, "127.0.0.1:5432");
    assert_eq!(tunnels[0].status, TunnelStatus::Active);
    assert_eq!(tunnels[0].connections, 2);

    // A query streams through it.
    h.send(Command::Execute {
        session: 1,
        query: 10,
        statements: vec![stmt("select id from orders order by id limit 5000")],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::All,
    });
    let mut rows = 0;
    next(&mut rx, 15, |e| match e {
        Event::Query {
            query: 10,
            event: QueryEvent::Rows(b),
        } => {
            rows += b.len();
            None
        }
        Event::Query {
            query: 10,
            event: QueryEvent::Finished { .. },
        } => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(rows, 5000);

    // Cancel reaches the server through the same tunnel.
    h.send(Command::Execute {
        session: 1,
        query: 11,
        statements: vec![stmt("select pg_sleep(30)")],
        tags: vec![],
        confirmed_destructive: false,
        fetch_limit: FetchLimit::All,
    });
    next(&mut rx, 5, |e| {
        matches!(
            e,
            Event::Query {
                query: 11,
                event: QueryEvent::StatementStarted { .. }
            }
        )
        .then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    h.send(Command::Cancel { query: 11 });
    let cancelled = next(&mut rx, 5, |e| match e {
        Event::Query {
            query: 11,
            event: QueryEvent::Finished { cancelled, .. },
        } => Some(cancelled),
        _ => None,
    })
    .await;
    assert!(cancelled);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());

    // Stopping the tunnel ends both sessions with a clear message.
    h.send(Command::StopTunnel { id: tunnels[0].id });
    let mut ended = Vec::new();
    while ended.len() < 2 {
        let (s, m) = next(&mut rx, 5, |e| match e {
            Event::SessionFailed { session, message } => Some((session, message)),
            _ => None,
        })
        .await;
        assert!(m.contains("tunnel") && m.contains("stopped"), "{m}");
        ended.push(s);
    }
    ended.sort_unstable();
    assert_eq!(ended, [1, 2]);
}
