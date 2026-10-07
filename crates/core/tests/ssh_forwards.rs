//! A Host's saved port forwards through the bus, against the local test servers (see
//! `scripts/ssh-test-servers.sh`). Run with
//! `cargo test -p switchyard-core --test ssh_forwards -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use switchyard_core::remote::ssh::{ForwardKind, HostKeyDecision, TunnelInfo, TunnelStatus};
use switchyard_core::store::{ForwardDirection, Host, PortForward, Profile, SshAuth};
use switchyard_core::term::TermSize;
use switchyard_core::{
    Command, Core, Event, EventReceiver, PromptAnswer, RuntimeHandle, ServiceConfig, TermTarget,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn keys() -> String {
    std::env::var("SWITCHYARD_SSH_KEYS").unwrap_or_else(|_| "/tmp/switchyard-ssh".into())
}

/// Wait for the event `f` picks, trusting host keys on the way.
async fn next<T>(
    rx: &mut EventReceiver,
    h: &RuntimeHandle,
    what: &str,
    mut f: impl FnMut(&Event) -> Option<T>,
) -> T {
    let found = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let ev = rx.next().await.expect("events");
            if let Event::HostKeyPrompt { request, .. } = &ev {
                h.send(Command::AnswerPrompt {
                    request: *request,
                    answer: PromptAnswer::HostKey(HostKeyDecision::TrustAndSave),
                });
            }
            if let Some(t) = f(&ev) {
                return t;
            }
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn echo_server() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

fn running<'a>(list: &'a [TunnelInfo], forward: &str) -> Option<&'a TunnelInfo> {
    list.iter()
        .find(|t| t.forward_id.as_deref() == Some(forward) && t.status == TunnelStatus::Active)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ssh servers"]
async fn saved_forwards_auto_start_start_and_stop() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let echo = echo_server().await;
    let local_port = free_port();

    let mut host = Host::new("fwd-box", "127.0.0.1", "swy");
    host.port = 2222;
    host.auth = SshAuth::PublicKey {
        key_path: format!("{}/id_ed25519", keys()),
    };
    let mut local = PortForward::new(ForwardDirection::Local);
    local.bind_port = local_port;
    local.target_host = "127.0.0.1".into();
    local.target_port = echo;
    local.auto_start = true;
    let mut socks = PortForward::new(ForwardDirection::Dynamic);
    socks.bind_port = free_port();
    let (local_id, socks_id) = (local.id.clone(), socks.id.clone());
    host.forwards = vec![local, socks];
    let host_id = host.id.clone();
    h.send(Command::SaveProfile {
        request: 1,
        profile: Profile::Host(host),
        secret: None,
    });
    next(&mut rx, &h, "ProfileSaved", |e| {
        matches!(e, Event::ProfileSaved { request: 1, .. }).then_some(())
    })
    .await;

    // A terminal on the Host starts its auto-start forward.
    h.send(Command::OpenTerminal {
        term: 1,
        target: TermTarget::Host(host_id.clone()),
        size: TermSize { cols: 80, rows: 24 },
    });
    let info = next(&mut rx, &h, "auto-start forward", |e| match e {
        Event::Tunnels(list) => running(list, &local_id).cloned(),
        _ => None,
    })
    .await;
    assert_eq!(info.kind, ForwardKind::Local);
    assert_eq!(info.local_port, local_port);
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", local_port))
        .await
        .unwrap();
    s.write_all(b"ping").await.unwrap();
    let mut got = [0u8; 4];
    s.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"ping");

    // The dynamic one starts on request.
    h.send(Command::StartForward {
        host: host_id.clone(),
        forward: socks_id.clone(),
    });
    let socks_info = next(&mut rx, &h, "SOCKS forward", |e| match e {
        Event::Tunnels(list) => running(list, &socks_id).cloned(),
        _ => None,
    })
    .await;
    assert_eq!(socks_info.kind, ForwardKind::Dynamic);

    // Starting a running forward again is a no-op; an unknown one is an error.
    h.send(Command::StartForward {
        host: host_id.clone(),
        forward: "nope".into(),
    });
    let msg = next(&mut rx, &h, "error", |e| match e {
        Event::Error { context, message } if context == "Port forward" => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(msg.contains("not found"), "{msg}");

    // Stop: it leaves the list and its port closes.
    h.send(Command::StopTunnel { id: info.id });
    next(&mut rx, &h, "stopped", |e| match e {
        Event::Tunnels(list) => (!list.iter().any(|t| t.id == info.id)).then_some(()),
        _ => None,
    })
    .await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .is_err()
    );
    drop(s);
}
