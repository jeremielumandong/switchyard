//! Fleet through the bus against the docker compose `ssh` service: one command on several
//! Hosts (two that share the server, one unreachable), the Production refusal, a
//! read-only probe and a key audit.
//!
//! ```text
//! docker compose -f docker/compose.yml up -d ssh
//! cargo test -p switchyard-core --test fleet_ssh -- --ignored
//! ```
//!
//! `SWITCHYARD_SMOKE_SSH_PORT` (2222) overrides the port.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_core::fleet::{FleetAction, FleetCommand, FleetEvent, HostStatus, KeyOp};
use switchyard_core::remote::ssh::HostKeyDecision;
use switchyard_core::store::{EnvironmentLabel, Host, Profile, ProfileId, SshAuth};
use switchyard_core::{Command, Core, Event, EventReceiver, PromptAnswer, ServiceConfig};

fn port() -> u16 {
    std::env::var("SWITCHYARD_SMOKE_SSH_PORT")
        .map(|p| p.parse().unwrap())
        .unwrap_or(2222)
}

async fn save(
    h: &switchyard_core::RuntimeHandle,
    rx: &mut EventReceiver,
    request: u64,
    host: Host,
) -> ProfileId {
    let id = host.id.clone();
    h.send(Command::SaveProfile {
        request,
        profile: Profile::Host(host),
        secret: Some(SecretString::from("switchyard")),
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Event::ProfileSaved { request: r, .. } = rx.next().await.unwrap()
                && r == request
            {
                return;
            }
        }
    })
    .await
    .expect("ProfileSaved");
    id
}

/// Collect a run's events until `Finished`, trusting host keys on the way.
async fn collect(
    h: &switchyard_core::RuntimeHandle,
    rx: &mut EventReceiver,
    run: u64,
) -> Vec<FleetEvent> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match rx.next().await.unwrap() {
                Event::HostKeyPrompt { request, .. } => h.send(Command::AnswerPrompt {
                    request,
                    answer: PromptAnswer::HostKey(HostKeyDecision::TrustAndSave),
                }),
                Event::Fleet(ev) => {
                    let finished = matches!(ev, FleetEvent::Finished { run: r, .. } if r == run);
                    let refused = matches!(ev, FleetEvent::Refused { run: r, .. } if r == run);
                    out.push(ev);
                    if finished || refused {
                        return;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("run {run} did not finish: {out:#?}"));
    out
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the docker ssh service"]
async fn fleet_runs_probes_and_audits_on_the_ssh_service() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();

    let mut a = Host::new("fleet-a", "127.0.0.1", "deploy");
    a.port = port();
    a.auth = SshAuth::Password;
    let mut b = a.clone();
    b.id = ProfileId::new();
    b.name = "fleet-b".into();
    // Nothing listens on port 1: the Host is unreachable, the others still run.
    let mut down = a.clone();
    down.id = ProfileId::new();
    down.name = "fleet-down".into();
    down.port = 1;
    let a = save(&h, &mut rx, 1, a).await;
    let b = save(&h, &mut rx, 2, b).await;
    let down = save(&h, &mut rx, 3, down).await;

    h.send(Command::Fleet(FleetCommand::Run {
        run: 10,
        hosts: vec![a.clone(), b.clone(), down.clone()],
        action: FleetAction::Command("echo fleet-ok; uname -s; exit 3".into()),
        concurrency: Some(2),
        timeout_secs: 30,
        confirmed: false,
    }));
    let events = collect(&h, &mut rx, 10).await;
    let mut done = HashMap::new();
    let mut output: HashMap<ProfileId, String> = HashMap::new();
    for ev in &events {
        match ev {
            FleetEvent::Done { host, outcome, .. } => {
                done.insert(host.clone(), outcome.clone());
            }
            FleetEvent::Output {
                host,
                text,
                stderr: false,
                ..
            } => output.entry(host.clone()).or_default().push_str(text),
            _ => {}
        }
    }
    for id in [&a, &b] {
        let o = &done[id];
        assert_eq!(o.status, HostStatus::Failed, "{o:?}");
        assert_eq!(o.exit_status, Some(3));
        assert_eq!(o.first_line, "fleet-ok");
        assert!(output[id].contains("fleet-ok\nLinux\n"), "{:?}", output[id]);
    }
    assert_eq!(done[&a].digest, done[&b].digest);
    assert_eq!(done[&down].status, HostStatus::Unreachable);

    // A Production Host needs the typed confirmation.
    let mut prod = Host::new("fleet-prod", "127.0.0.1", "deploy");
    prod.port = port();
    prod.environment = EnvironmentLabel::Production;
    let prod = save(&h, &mut rx, 4, prod).await;
    h.send(Command::Fleet(FleetCommand::Run {
        run: 11,
        hosts: vec![a.clone(), prod.clone()],
        action: FleetAction::Command("true".into()),
        concurrency: None,
        timeout_secs: 30,
        confirmed: false,
    }));
    let events = collect(&h, &mut rx, 11).await;
    assert!(
        matches!(events.last(), Some(FleetEvent::Refused { .. })),
        "{events:#?}"
    );

    // The read-only probe parses the container's real output.
    h.send(Command::Fleet(FleetCommand::Probe {
        run: 12,
        hosts: vec![a.clone()],
    }));
    let events = collect(&h, &mut rx, 12).await;
    let report = events
        .iter()
        .find_map(|e| match e {
            FleetEvent::Probed { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("probed")
        .expect("probe ok");
    assert!(report.uptime_secs.is_some(), "{report:?}");
    assert!(report.memory.is_some(), "{report:?}");
    assert!(report.os.is_some(), "{report:?}");

    // A key audit reads authorized_keys without changing it.
    h.send(Command::Fleet(FleetCommand::Keys {
        run: 13,
        hosts: vec![a.clone()],
        op: KeyOp::Audit(None),
        concurrency: None,
        confirmed: false,
    }));
    let events = collect(&h, &mut rx, 13).await;
    let keys = events
        .iter()
        .find_map(|e| match e {
            FleetEvent::Keys { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("keys");
    let report = keys.expect("audit ok");
    assert!(!report.backed_up);
    assert_eq!(report.session_key, None, "a password login has no key");

    // Add twice (idempotent), then remove: each change writes the backup first.
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFZBlaQXr6uQ0SrRZ/3r/oESt9ssMorfNJnKwAT+8fCW fleet-test";
    let keys_run = |run: u64, op: KeyOp| {
        h.send(Command::Fleet(FleetCommand::Keys {
            run,
            hosts: vec![a.clone()],
            op,
            concurrency: None,
            confirmed: false,
        }));
        run
    };
    for (run, op, has, changed) in [
        (14, KeyOp::Add(KEY.into()), true, true),
        (15, KeyOp::Add(KEY.into()), true, false),
        (16, KeyOp::Remove(KEY.into()), false, true),
    ] {
        let run = keys_run(run, op);
        let events = collect(&h, &mut rx, run).await;
        let report = events
            .iter()
            .find_map(|e| match e {
                FleetEvent::Keys { result, .. } => Some(result.clone()),
                _ => None,
            })
            .expect("keys")
            .unwrap_or_else(|e| panic!("run {run}: {e}"));
        assert_eq!(report.has_key, Some(has), "run {run}: {report:?}");
        assert_eq!(report.backed_up, changed, "run {run}: {report:?}");
    }
}
