//! Assistant runs through the bus with fake coding CLIs (shell scripts replaying recorded
//! output): settings, per-connection choice, refusal without agent access, cancel, and
//! "Open in terminal" releasing its token.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use switchyard_core::agent_run::{ASSISTANT_SETTINGS_KEY, AssistantSettings};
use switchyard_core::agents::{AgentEvent, AgentKind};
use switchyard_core::db::Engine;
use switchyard_core::store::{DbConnection, Profile};
use switchyard_core::term::TermSize;
use switchyard_core::{Command, Core, Event, EventReceiver, RuntimeHandle, ServiceConfig};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const CLAUDE_OUT: &str = r#"cat >/dev/null
echo '{"type":"system","subtype":"init","session_id":"c-1","model":"m"}'
echo '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Add an index."}]}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Add an index.","session_id":"c-1"}'"#;

const CODEX_OUT: &str = r#"cat >/dev/null
echo '{"type":"thread.started","thread_id":"x-1"}'
echo '{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"From Codex."}}'
echo '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'"#;

async fn collect(
    rx: &mut EventReceiver,
    h: &RuntimeHandle,
    run: u64,
) -> (AgentKind, Vec<AgentEvent>) {
    let _ = h;
    let mut out = Vec::new();
    let mut kind = None;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Event::Agent {
                run: r,
                agent,
                event,
            } = rx.next().await.expect("events")
                && r == run
            {
                kind = Some(agent);
                let last = matches!(event, AgentEvent::Exited(_));
                out.push(event);
                if last {
                    break;
                }
            }
        }
    })
    .await
    .expect("run ended");
    (kind.unwrap(), out)
}

/// Save the assistant settings and wait until they read back (commands run concurrently).
async fn set_settings(h: &RuntimeHandle, rx: &mut EventReceiver, settings: &AssistantSettings) {
    let value = serde_json::to_value(settings).unwrap();
    h.send(Command::SetSetting {
        key: ASSISTANT_SETTINGS_KEY.into(),
        value: value.clone(),
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            h.send(Command::LoadSetting {
                key: ASSISTANT_SETTINGS_KEY.into(),
            });
            loop {
                if let Event::Setting { key, value: v } = rx.next().await.unwrap()
                    && key == ASSISTANT_SETTINGS_KEY
                {
                    if v.as_ref() == Some(&value) {
                        return;
                    }
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("settings saved");
}

fn conn(name: &str) -> DbConnection {
    let mut c = DbConnection::new(name, Engine::Postgres);
    c.server = "127.0.0.1".into();
    c.user = "u".into();
    c.database = "d".into();
    c
}

async fn saved(rx: &mut EventReceiver, request: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Event::ProfileSaved { request: r, .. } = rx.next().await.unwrap()
                && r == request
            {
                return;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_follow_settings_and_connection_choice() {
    let bin = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let claude = script(bin.path(), "claude", CLAUDE_OUT);
    let codex = script(bin.path(), "codex", CODEX_OUT);
    let sleeper = script(bin.path(), "slow", "cat >/dev/null; sleep 30");
    let mut cfg = ServiceConfig::in_memory();
    cfg.data_dir = data.path().to_owned();
    cfg.swy = Some("/bin/true".into());
    let (core, mut rx) = Core::start(cfg).unwrap();
    let h = core.handle();

    let mut settings = AssistantSettings::default();
    settings.claude_code.path = claude.to_string_lossy().into_owned();
    settings.codex.path = codex.to_string_lossy().into_owned();
    set_settings(&h, &mut rx, &settings).await;

    let mut allowed = conn("shop");
    allowed.agent_access = true;
    let mut codex_conn = conn("analytics");
    codex_conn.agent_access = true;
    codex_conn.assistant_agent = Some("codex".into());
    let closed = conn("billing");
    for (i, c) in [&allowed, &codex_conn, &closed].into_iter().enumerate() {
        h.send(Command::SaveProfile {
            request: 10 + i as u64,
            profile: Profile::Db(c.clone()),
            secret: None,
        });
        saved(&mut rx, 10 + i as u64).await;
    }

    // The default CLI (Claude Code).
    h.send(Command::RunAgent {
        run: 1,
        agent: None,
        connection: Some(allowed.id.clone()),
        prompt: "why slow?".into(),
        resume: None,
    });
    let (kind, events) = collect(&mut rx, &h, 1).await;
    assert_eq!(kind, AgentKind::ClaudeCode);
    assert!(
        events.contains(&AgentEvent::Text("Add an index.".into())),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Done(s) if s.session_id.as_deref() == Some("c-1")))
    );
    assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(0))));

    // The connection's own choice.
    h.send(Command::RunAgent {
        run: 2,
        agent: None,
        connection: Some(codex_conn.id.clone()),
        prompt: "q".into(),
        resume: None,
    });
    let (kind, events) = collect(&mut rx, &h, 2).await;
    assert_eq!(kind, AgentKind::Codex);
    assert!(
        events.contains(&AgentEvent::Text("From Codex.".into())),
        "{events:?}"
    );

    // Agents off for this connection: refused before anything starts.
    h.send(Command::RunAgent {
        run: 3,
        agent: None,
        connection: Some(closed.id.clone()),
        prompt: "q".into(),
        resume: None,
    });
    let (_, events) = collect(&mut rx, &h, 3).await;
    assert!(
        matches!(&events[0], AgentEvent::Error(m) if m.contains("Allow coding agents")),
        "{events:?}"
    );

    // Cancel.
    let mut s2 = settings.clone();
    s2.gemini.path = sleeper.to_string_lossy().into_owned();
    set_settings(&h, &mut rx, &s2).await;
    h.send(Command::RunAgent {
        run: 4,
        agent: Some(AgentKind::Gemini),
        connection: Some(allowed.id.clone()),
        prompt: "q".into(),
        resume: None,
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.send(Command::CancelAgent { run: 4 });
    let (_, events) = collect(&mut rx, &h, 4).await;
    assert!(
        events.contains(&AgentEvent::Error("The run was cancelled.".into())),
        "{events:?}"
    );

    // Every run's token is gone.
    let tokens = data.path().join("agent-tokens");
    assert_eq!(std::fs::read_dir(&tokens).unwrap().count(), 0);

    // A CLI that is not installed says where to look.
    let mut s3 = settings.clone();
    s3.claude_code.path = "/nonexistent/claude".into();
    set_settings(&h, &mut rx, &s3).await;
    h.send(Command::RunAgent {
        run: 5,
        agent: None,
        connection: Some(allowed.id.clone()),
        prompt: "q".into(),
        resume: None,
    });
    let (_, events) = collect(&mut rx, &h, 5).await;
    assert!(
        matches!(&events[0], AgentEvent::Error(m) if m.contains("Settings → Assistant")),
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn open_in_terminal_holds_its_token_until_the_terminal_ends() {
    let bin = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let out = bin.path().join("args");
    let claude = script(
        bin.path(),
        "claude",
        &format!(
            "printf '%s\\n' \"$@\" > {}; pwd >> {}; sleep 30",
            out.display(),
            out.display()
        ),
    );
    let mut cfg = ServiceConfig::in_memory();
    cfg.data_dir = data.path().to_owned();
    cfg.swy = Some("/bin/true".into());
    let (core, mut rx) = Core::start(cfg).unwrap();
    let h = core.handle();
    let mut settings = AssistantSettings::default();
    settings.claude_code.path = claude.to_string_lossy().into_owned();
    set_settings(&h, &mut rx, &settings).await;
    h.send(Command::OpenAgentTerminal {
        term: 7,
        agent: None,
        connection: None,
        size: TermSize { cols: 80, rows: 24 },
    });
    let description = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.next().await.unwrap() {
                Event::TerminalOpened {
                    term: 7,
                    description,
                    ..
                } => return description,
                Event::TerminalFailed { term: 7, message } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(description, "Claude Code · Switchyard tools");
    let tokens = data.path().join("agent-tokens");
    // The script's `>` creates the file before printf fills it, and `pwd >>` comes last:
    // wait for that final working-directory line, not just for the file.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let args = loop {
        let args = std::fs::read_to_string(&out).unwrap_or_default();
        if args.lines().last().is_some_and(|l| l.starts_with('/'))
            || std::time::Instant::now() >= deadline
        {
            break args;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        args.contains("--mcp-config") && args.contains("--strict-mcp-config"),
        "{args}"
    );
    assert!(
        !args.lines().any(|l| l == "-p"),
        "interactive, not headless: {args}"
    );
    assert_eq!(
        std::fs::read_dir(&tokens).unwrap().count(),
        1,
        "live while the CLI runs"
    );

    h.send(Command::CloseTerminal { term: 7 });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(&tokens).unwrap().count() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        std::fs::read_dir(&tokens).unwrap().count(),
        0,
        "revoked when it ended"
    );
    let run_dir = args.lines().last().unwrap().to_owned();
    assert!(!Path::new(&run_dir).exists(), "run directory removed");
}
