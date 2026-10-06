//! SSH terminals through the bus, against the local test servers (see
//! `scripts/ssh-test-servers.sh`). Run with
//! `cargo test -p switchyard-core --test ssh_terminal -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use futures::StreamExt;
use switchyard_core::remote::ssh::HostKeyDecision;
use switchyard_core::store::{Host, Profile, SshAuth};
use switchyard_core::term::TermSize;
use switchyard_core::{
    Command, Core, Event, EventReceiver, PromptAnswer, ServiceConfig, TermStatus, TermTarget,
};

async fn next<T>(rx: &mut EventReceiver, secs: u64, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let ev = rx.next().await.expect("events");
            if let Some(t) = f(ev) {
                return t;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn keys() -> String {
    std::env::var("SWITCHYARD_SSH_KEYS").unwrap_or_else(|_| "/tmp/switchyard-ssh".into())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ssh servers"]
async fn ssh_terminal_prompts_runs_and_reconnects() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    let mut host = Host::new("test-box", "127.0.0.1", "swy");
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

    h.send(Command::OpenTerminal {
        term: 9,
        target: TermTarget::Host(host_id),
        size: TermSize { cols: 80, rows: 24 },
    });
    let terminal = next(&mut rx, 5, |e| match e {
        Event::TerminalOpened {
            term: 9, terminal, ..
        } => Some(terminal),
        _ => None,
    })
    .await;
    let request = next(&mut rx, 10, |e| match e {
        Event::HostKeyPrompt { request, key } => {
            assert!(key.fingerprint.starts_with("SHA256:"));
            Some(request)
        }
        _ => None,
    })
    .await;
    h.send(Command::AnswerPrompt {
        request,
        answer: PromptAnswer::HostKey(HostKeyDecision::TrustAndSave),
    });
    let description = next(&mut rx, 10, |e| match e {
        Event::TerminalStatus {
            term: 9,
            status: TermStatus::Connected { description },
        } => Some(description),
        _ => None,
    })
    .await;
    assert!(
        description.contains("swy@127.0.0.1 · ed25519"),
        "{description}"
    );

    // The server-side user processes exist once the shell runs (it sets the title).
    next(&mut rx, 10, |e| {
        matches!(e, Event::TerminalTitle { term: 9, .. }).then_some(())
    })
    .await;

    // Drop the connection from the server side: the terminal reconnects on its own.
    std::process::Command::new("pkill")
        .args(["-KILL", "-u", "swy"])
        .status()
        .unwrap();
    next(&mut rx, 15, |e| match e {
        Event::TerminalStatus {
            term: 9,
            status: TermStatus::Reconnecting {
                attempt: 1, of: 5, ..
            },
        } => Some(()),
        _ => None,
    })
    .await;
    next(&mut rx, 15, |e| match e {
        Event::TerminalStatus {
            term: 9,
            status: TermStatus::Connected { .. },
        } => Some(()),
        Event::HostKeyPrompt { .. } => panic!("the saved key must not prompt again"),
        _ => None,
    })
    .await;

    for b in b"echo ssh-$((6*7)); exit 3\r" {
        h.send(Command::TerminalInput {
            term: 9,
            bytes: vec![*b],
        });
    }
    let code = next(&mut rx, 10, |e| match e {
        Event::TerminalExited { term: 9, code, .. } => Some(code),
        _ => None,
    })
    .await;
    assert_eq!(code, Some(3));
    let text: Vec<String> = terminal
        .snapshot()
        .lines
        .iter()
        .map(|l| l.text.trim_end().to_owned())
        .collect();
    assert!(text.iter().any(|l| l == "ssh-42"), "{text:?}");
}
