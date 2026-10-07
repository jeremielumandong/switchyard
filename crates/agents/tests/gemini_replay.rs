//! The Gemini adapter end to end with a fake `gemini`: a shell script that records what it
//! was given and replays stream-json recorded from Gemini CLI 0.63 (two Switchyard tool
//! calls, scripted by a mock Gemini API).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use switchyard_agents::{AgentEvent, AgentRun, Gemini, McpServer, RunRequest, runner};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/gemini-list-describe.jsonl"
);

fn fake_gemini(dir: &Path, out: &Path) -> PathBuf {
    let script = dir.join("gemini");
    let o = out.display();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {o}/argv\ncat > {o}/stdin\n\
             cp .gemini/settings.json {o}/settings.json\nls -a > {o}/files\n\
             printf '%s' \"$GEMINI_CLI_TRUST_WORKSPACE\" > {o}/trust\ncat {FIXTURE}\n"
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
    let gemini = fake_gemini(&bin, &out);
    let run = runner::start(
        Arc::new(Gemini),
        RunRequest {
            program: Some(gemini),
            prompt: "describe orders".into(),
            resume: None,
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
        },
    )
    .unwrap();
    let events = collect(run);
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
    let Some(AgentEvent::Done(summary)) = events.iter().find(|e| e.is_outcome()) else {
        panic!("{events:?}")
    };
    assert_eq!(summary.session_id.as_deref(), Some("recorded-session-1"));
    assert_eq!(summary.text, "Plan: Bitmap Heap Scan on orders.");
    assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(0))));

    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        settings["mcpServers"]["switchyard"]["env"]["SWITCHYARD_MCP_TOKEN"],
        "secret-token"
    );
    let files = std::fs::read_to_string(out.join("files")).unwrap();
    for f in [".gemini", "GEMINI.md", "switchyard-policy.toml"] {
        assert!(files.lines().any(|l| l == f), "{f} missing: {files}");
    }
    assert_eq!(std::fs::read_to_string(out.join("trust")).unwrap(), "true");
    assert_eq!(
        std::fs::read_to_string(out.join("stdin")).unwrap(),
        "describe orders"
    );
    let argv = std::fs::read_to_string(out.join("argv")).unwrap();
    assert!(!argv.contains("secret-token") && !argv.contains("describe orders"));
}
