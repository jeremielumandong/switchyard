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

/// Wait for the event `f` picks; on timeout, name the wait and list the events skipped.
async fn next<T>(
    rx: &mut EventReceiver,
    what: &str,
    secs: u64,
    mut f: impl FnMut(Event) -> Option<T>,
) -> T {
    let mut skipped = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let ev = rx.next().await.expect("events");
            let seen = format!("{ev:?}");
            if let Some(t) = f(ev) {
                return t;
            }
            skipped.push(seen.chars().take(200).collect::<String>());
        }
    })
    .await;
    match found {
        Ok(t) => t,
        Err(_) => panic!("timed out waiting for {what}; skipped: {skipped:#?}"),
    }
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
    next(&mut rx, "ProfileSaved", 5, |e| {
        matches!(e, Event::ProfileSaved { request: 1, .. }).then_some(())
    })
    .await;

    h.send(Command::OpenTerminal {
        term: 9,
        target: TermTarget::Host(host_id),
        size: TermSize { cols: 80, rows: 24 },
    });
    let terminal = next(&mut rx, "TerminalOpened", 5, |e| match e {
        Event::TerminalOpened {
            term: 9, terminal, ..
        } => Some(terminal),
        _ => None,
    })
    .await;
    let request = next(&mut rx, "HostKeyPrompt", 10, |e| match e {
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
    let description = next(&mut rx, "Connected", 10, |e| match e {
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

    // The server-side user processes exist once the shell runs. Have it set the title
    // itself: whether its rc files do depends on the machine (CI runners' don't).
    h.send(Command::TerminalInput {
        term: 9,
        bytes: b"printf '\\033]0;swy-ready\\007'\r".to_vec(),
    });
    next(&mut rx, "TerminalTitle", 10, |e| match e {
        Event::TerminalTitle { term: 9, title } if title == "swy-ready" => Some(()),
        _ => None,
    })
    .await;

    // Drop the connection from the server side: the terminal reconnects on its own.
    // Kill only the user's sshd session process (`sshd: swy@pts/N`, `sshd-session:` on
    // OpenSSH 9.8+), not every process of the user: killing its `systemd --user` too can
    // stall the next PAM login while logind cleans up, and the reconnected shell then
    // never answers.
    // The test user's processes belong to someone else on CI runners: fall back to sudo.
    let session = ["-KILL", "-u", "swy", "-f", "^sshd(-session)?: swy"];
    let killed = std::process::Command::new("pkill")
        .args(session)
        .status()
        .unwrap();
    if !killed.success() {
        std::process::Command::new("sudo")
            .args(["-n", "pkill"])
            .args(session)
            .status()
            .unwrap();
    }
    next(&mut rx, "Reconnecting 1/5", 15, |e| match e {
        Event::TerminalStatus {
            term: 9,
            status: TermStatus::Reconnecting {
                attempt: 1, of: 5, ..
            },
        } => Some(()),
        _ => None,
    })
    .await;
    next(&mut rx, "Connected again", 15, |e| match e {
        Event::TerminalStatus {
            term: 9,
            status: TermStatus::Connected { .. },
        } => Some(()),
        Event::HostKeyPrompt { .. } => panic!("the saved key must not prompt again"),
        _ => None,
    })
    .await;
    // Connected means the channel is up, not that the new shell reads yet: input typed
    // before it sets up the terminal can be discarded. Ask it to set the title until it
    // does, then type.
    let mut ready = false;
    for _ in 0..20 {
        h.send(Command::TerminalInput {
            term: 9,
            bytes: b"printf '\\033]0;swy-again\\007'\r".to_vec(),
        });
        let title = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Event::TerminalTitle { term: 9, title } = rx.next().await.expect("events")
                    && title == "swy-again"
                {
                    return;
                }
            }
        })
        .await;
        if title.is_ok() {
            ready = true;
            break;
        }
    }
    assert!(ready, "the reconnected shell never answered");

    for b in b"echo ssh-$((6*7)); exit 3\r" {
        h.send(Command::TerminalInput {
            term: 9,
            bytes: vec![*b],
        });
    }
    let code = next(&mut rx, "TerminalExited", 10, |e| match e {
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
