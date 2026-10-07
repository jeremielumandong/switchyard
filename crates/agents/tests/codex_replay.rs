//! The Codex adapter end to end with a fake `codex`: a shell script that records what it
//! was given and replays `codex exec --json` output recorded from codex-cli 0.160.1 (two
//! Switchyard tool calls, scripted by a mock model).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use switchyard_agents::{AgentEvent, AgentRun, Codex, McpServer, RunRequest, runner};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/codex-list-describe.jsonl"
);

fn fake_codex(dir: &Path, out: &Path) -> PathBuf {
    let script = dir.join("codex");
    let o = out.display();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {o}/argv\ncat > {o}/stdin\n\
             printf '%s' \"$SWITCHYARD_MCP_TOKEN\" > {o}/token\ncat {FIXTURE}\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn collect(mut run: AgentRun) -> Vec<AgentEvent> {
    let mut v = Vec::new();
    while let Some(e) = run.blocking_next() {
        v.push(e);
    }
    v
}

#[test]
fn replays_a_recorded_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (bin, out) = (tmp.path().join("bin"), tmp.path().join("out"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let codex = fake_codex(&bin, &out);
    let request = |resume: Option<&str>| RunRequest {
        program: Some(codex.clone()),
        prompt: "describe orders".into(),
        resume: resume.map(str::to_owned),
        model: None,
        mcp: McpServer {
            command: "/opt/switchyard/swy".into(),
            args: vec!["mcp".into()],
            env: vec![("SWITCHYARD_MCP_TOKEN".into(), "secret-token".into())],
        },
        temp_root: Some(tmp.path().join("runs")),
        extra_args: Vec::new(),
        extra_env: Vec::new(),
        guards: Vec::new(),
    };
    let events = collect(runner::start(Arc::new(Codex), request(None)).unwrap());

    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall {
                name, arguments, ..
            } => Some((name.as_str(), arguments.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("list_connections", json!({})),
            (
                "describe_table",
                json!({"connection": "shop", "table": "orders"})
            ),
        ]
    );
    // The second call failed in the recording (no such connection there): an error result.
    let results: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .collect();
    assert_eq!(results, [false, true]);
    assert!(events.contains(&AgentEvent::Log(
        "Model metadata for `mock-model` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.".into()
    )));
    let Some(AgentEvent::Done(summary)) = events.iter().find(|e| e.is_outcome()) else {
        panic!("{events:?}")
    };
    assert_eq!(summary.session_id.as_deref(), Some("recorded-thread-1"));
    assert_eq!(summary.text, "Plan: Bitmap Heap Scan on orders.");
    assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(0))));

    // Prompt on stdin, token in the environment, neither on the command line.
    assert_eq!(
        std::fs::read_to_string(out.join("stdin")).unwrap(),
        "describe orders"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("token")).unwrap(),
        "secret-token"
    );
    let argv = std::fs::read_to_string(out.join("argv")).unwrap();
    assert!(!argv.contains("secret-token") && !argv.contains("describe orders"));
    assert!(argv.contains("--ignore-user-config"));

    // Follow-up: `exec resume … <thread> -`.
    collect(runner::start(Arc::new(Codex), request(summary.session_id.as_deref())).unwrap());
    let argv = std::fs::read_to_string(out.join("argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(&argv[..2], ["exec", "resume"]);
    assert_eq!(&argv[argv.len() - 2..], ["recorded-thread-1", "-"]);
}
