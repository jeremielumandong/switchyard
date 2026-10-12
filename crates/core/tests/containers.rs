//! Docker and Kubernetes connections through core. The first tests use a stand-in
//! `kubectl` script (no cluster needed); `docker_through_core` needs a Docker daemon:
//! `cargo test -p switchyard-core --test containers -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use switchyard_core::containers::{
    Action, ContainersError, ForwardKind, Listing, LogOptions, LogSource, PortForward, Query,
};
use switchyard_core::store::{
    ContainerConnection, ContainerPlatform, EnvironmentLabel, Profile, ProfileId,
};
use switchyard_core::{
    Command, Core, Event, EventReceiver, ForwardState, RuntimeHandle, ServiceConfig,
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

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../containers/testdata")
}

/// A `kubectl` stand-in answering from the recorded fixtures.
fn fake_kubectl(dir: &Path) -> PathBuf {
    let path = dir.join("kubectl");
    let script = format!(
        r#"#!/bin/sh
case "$*" in
  *"get pods"*) cat '{pods}' ;;
  *"logs"*) i=0; while true; do echo "log $i"; i=$((i+1)); sleep 0.05; done ;;
  *"port-forward"*) echo "Forwarding from 127.0.0.1:43210 -> 80"; exec sleep 30 ;;
  *"scale"*) echo "deployment.apps/web scaled" ;;
  *) echo "error: unknown command $*" >&2; exit 1 ;;
esac
"#,
        pods = fixtures().join("kubectl_pods.json").display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn save(h: &RuntimeHandle, rx: &mut EventReceiver, c: ContainerConnection) -> ProfileId {
    let id = c.id.clone();
    h.send(Command::SaveProfile {
        request: 900,
        profile: Profile::Containers(c),
        secret: None,
    });
    next(rx, 10, |e| match e {
        Event::ProfileSaved { request: 900, .. } => Some(()),
        Event::ProfileError { message, .. } => panic!("{message}"),
        _ => None,
    })
    .await;
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn kubernetes_with_a_stand_in_cli() {
    let dir = tempfile::tempdir().unwrap();
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let mut c = ContainerConnection::new("cluster", ContainerPlatform::Kubernetes);
    c.executable = Some(fake_kubectl(dir.path()).display().to_string());
    c.environment = EnvironmentLabel::Production;
    let id = save(&h, &mut rx, c.clone()).await;

    // Listing.
    h.send(Command::ContainersList {
        request: 1,
        connection: id.clone(),
        context: None,
        query: Query::Pods { namespace: None },
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::ContainersListed { request: 1, result } => Some(result),
        _ => None,
    })
    .await;
    let Ok(Listing::Pods(pods)) = r else {
        panic!("{r:?}")
    };
    assert_eq!(pods.len(), 6);

    // A CLI error comes back as its message.
    h.send(Command::ContainersList {
        request: 2,
        connection: id.clone(),
        context: Some("other".into()),
        query: Query::Services { namespace: None },
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::ContainersListed { request: 2, result } => Some(result),
        _ => None,
    })
    .await;
    assert!(
        matches!(&r, Err(ContainersError::Failed { message, .. }) if message.contains("--context other get services")),
        "{r:?}"
    );

    // Production: a scale needs the typed confirmation.
    let scale = Action::Scale {
        namespace: "shop".into(),
        name: "web".into(),
        replicas: 2,
    };
    h.send(Command::ContainersAction {
        request: 3,
        connection: id.clone(),
        context: None,
        action: scale.clone(),
        confirmed: false,
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::ContainersActionDone { request: 3, result } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r, Err(ContainersError::NeedsConfirmation));
    h.send(Command::ContainersAction {
        request: 4,
        connection: id.clone(),
        context: None,
        action: scale,
        confirmed: true,
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::ContainersActionDone { request: 4, result } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r, Ok("Scaled web to 2".into()));

    // A followed log streams until cancelled.
    h.send(Command::ContainersLogs {
        request: 5,
        connection: id.clone(),
        context: None,
        source: LogSource::Pod {
            namespace: "shop".into(),
            pod: "web-1".into(),
            container: None,
        },
        options: LogOptions::default(),
    });
    let mut lines = Vec::new();
    next(&mut rx, 10, |e| match e {
        Event::ContainersLog {
            request: 5,
            lines: l,
        } => {
            lines.extend(l);
            (lines.len() >= 10).then_some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(lines[0], "log 0");
    h.send(Command::ContainersCancel { request: 5 });
    let r = next(&mut rx, 10, |e| match e {
        Event::ContainersLogEnded { request: 5, result } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r, Ok(()));

    // Port forward: running with kubectl's port, then stopped.
    h.send(Command::ContainersForward {
        id: 7,
        connection: id.clone(),
        context: None,
        forward: PortForward {
            namespace: "shop".into(),
            kind: ForwardKind::Service,
            name: "web".into(),
            local_port: 0,
            remote_port: 80,
        },
    });
    next(&mut rx, 10, |e| match e {
        Event::ContainerForwards(list) => list
            .iter()
            .any(|f| f.id == 7 && f.state == ForwardState::Running(43210))
            .then_some(()),
        _ => None,
    })
    .await;
    h.send(Command::ContainersStopForward { id: 7 });
    next(&mut rx, 10, |e| match e {
        Event::ContainerForwards(list) => list.iter().all(|f| f.id != 7).then_some(()),
        _ => None,
    })
    .await;

    // Read-only refuses actions.
    c.read_only = true;
    c.executable = Some(dir.path().join("missing-kubectl").display().to_string());
    save(&h, &mut rx, c).await;
    h.send(Command::ContainersAction {
        request: 8,
        connection: id.clone(),
        context: None,
        action: Action::DeletePod {
            namespace: "shop".into(),
            name: "web-1".into(),
        },
        confirmed: true,
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::ContainersActionDone { request: 8, result } => Some(result),
        _ => None,
    })
    .await;
    assert_eq!(r, Err(ContainersError::ReadOnly));
    // A missing CLI is a hint.
    h.send(Command::ContainersList {
        request: 9,
        connection: id,
        context: None,
        query: Query::Namespaces,
    });
    let r = next(&mut rx, 10, |e| match e {
        Event::ContainersListed { request: 9, result } => Some(result),
        _ => None,
    })
    .await;
    assert!(r.as_ref().is_err_and(ContainersError::is_missing), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon"]
async fn docker_through_core() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let id = save(
        &h,
        &mut rx,
        ContainerConnection::new("Local Docker", ContainerPlatform::Docker),
    )
    .await;
    for (i, q) in [
        Query::Containers { all: true },
        Query::Images,
        Query::Volumes,
        Query::Networks,
    ]
    .into_iter()
    .enumerate()
    {
        let request = 10 + i as u64;
        h.send(Command::ContainersList {
            request,
            connection: id.clone(),
            context: None,
            query: q,
        });
        let r = next(&mut rx, 60, |e| match e {
            Event::ContainersListed { request: r, result } if r == request => Some(result),
            _ => None,
        })
        .await;
        assert!(r.is_ok(), "{r:?}");
    }
    // Logs of a container that does not exist end with docker's message.
    h.send(Command::ContainersLogs {
        request: 20,
        connection: id,
        context: None,
        source: LogSource::Docker {
            id: "swy-no-such-container".into(),
        },
        options: LogOptions::default(),
    });
    let r = next(&mut rx, 60, |e| match e {
        Event::ContainersLogEnded {
            request: 20,
            result,
        } => Some(result),
        _ => None,
    })
    .await;
    assert!(
        matches!(&r, Err(ContainersError::Failed { message, .. }) if message.contains("No such container")),
        "{r:?}"
    );
}
