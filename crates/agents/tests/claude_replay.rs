//! The Claude Code adapter end to end with a fake `claude`: a shell script that records
//! what it was given and replays stream-json recorded from a real run (Claude Code
//! 2.1.292: describe_table, explain and list_connections against the docker PostgreSQL).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use switchyard_agents::{AgentEvent, AgentRun, ClaudeCode, McpServer, RunRequest, runner};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/claude-describe-explain.jsonl"
);

/// A fake `claude` writing its argv, stdin and MCP config (with its mode) into `out`.
fn fake_claude(dir: &Path, out: &Path) -> PathBuf {
    let script = dir.join("claude");
    let o = out.display();
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > {o}/argv
cat > {o}/stdin
pwd > {o}/cwd
prev=
for a in "$@"; do
  if [ "$prev" = "--mcp-config" ]; then cp "$a" {o}/mcp.json; stat -c %a "$a" > {o}/mode 2>/dev/null || stat -f %Lp "$a" > {o}/mode; fi
  prev=$a
done
cat {FIXTURE}
"#
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn request(program: &Path, root: &Path, resume: Option<&str>) -> RunRequest {
    RunRequest {
        program: Some(program.to_owned()),
        prompt: "Why is `select * from orders where customer_id = 42` slow?".into(),
        resume: resume.map(str::to_owned),
        model: None,
        mcp: McpServer {
            command: "/opt/switchyard/swy".into(),
            args: vec!["mcp".into()],
            env: vec![
                ("SWITCHYARD_MCP_TOKEN".into(), "secret-token".into()),
                ("SWITCHYARD_AGENT".into(), "claude-code".into()),
            ],
        },
        temp_root: Some(root.to_owned()),
        extra_args: Vec::new(),
        extra_env: Vec::new(),
        guards: Vec::new(),
    }
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
    let (bin, out, root) = (
        tmp.path().join("bin"),
        tmp.path().join("out"),
        tmp.path().join("runs"),
    );
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let claude = fake_claude(&bin, &out);

    let run = runner::start(Arc::new(ClaudeCode), request(&claude, &root, None)).unwrap();
    let events = collect(run);

    // Normalized: Switchyard's tools by their MCP names, each result matched to its call.
    let calls: Vec<(String, Value)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall {
                name, arguments, ..
            } => Some((name.clone(), arguments.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        [
            (
                "describe_table".to_owned(),
                json!({"connection": "shop", "table": "orders"})
            ),
            (
                "explain".to_owned(),
                json!({"connection": "shop", "sql": "SELECT * FROM orders WHERE customer_id = 42"})
            ),
            ("list_connections".to_owned(), json!({})),
        ]
    );
    let call_ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let result_ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult { id, is_error, .. } => {
                assert!(!is_error);
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(call_ids, result_ids);
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { text, .. } if text.contains("Bitmap Heap Scan"))
    ));

    assert!(matches!(
        &events[0],
        AgentEvent::Started { session_id: Some(s), model: Some(_) } if s == "recorded-session-1"
    ));
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let Some(AgentEvent::Done(summary)) = events.iter().find(|e| e.is_outcome()) else {
        panic!("no outcome: {events:?}");
    };
    assert_eq!(summary.text, text, "streamed text is the answer, once");
    assert_eq!(summary.session_id.as_deref(), Some("recorded-session-1"));
    assert_eq!(summary.turns, Some(4));
    assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(0))));

    // What the CLI was given.
    let argv = std::fs::read_to_string(out.join("argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    for flag in [
        "-p",
        "--restricted",
        "--strict-mcp-config",
        "--verbose",
        "dontAsk",
        "mcp__switchyard",
    ] {
        assert!(argv.contains(&flag), "{flag} missing: {argv:?}");
    }
    assert!(!argv.contains(&"--resume"));
    assert!(
        !argv
            .iter()
            .any(|a| a.contains("secret-token") || a.contains("customer_id")),
        "token or prompt on the command line"
    );
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("stdin")).unwrap()).unwrap();
    assert_eq!(stdin["type"], "user");
    assert!(
        stdin["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("customer_id = 42")
    );
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("mcp.json")).unwrap()).unwrap();
    assert_eq!(
        cfg["mcpServers"]["switchyard"]["env"]["SWITCHYARD_MCP_TOKEN"],
        "secret-token"
    );
    assert_eq!(
        cfg["mcpServers"]["switchyard"]["command"],
        "/opt/switchyard/swy"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("mode")).unwrap().trim(),
        "600"
    );
    // It ran in a private directory under the root, gone now.
    let cwd = std::fs::read_to_string(out.join("cwd")).unwrap();
    assert!(Path::new(cwd.trim()).starts_with(std::fs::canonicalize(&root).unwrap()));
    assert!(!Path::new(cwd.trim()).exists());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);

    // Resuming passes the conversation id back.
    let session = summary.session_id.clone().unwrap();
    let run = runner::start(
        Arc::new(ClaudeCode),
        request(&claude, &root, Some(&session)),
    )
    .unwrap();
    collect(run);
    let argv = std::fs::read_to_string(out.join("argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    let i = argv.iter().position(|a| *a == "--resume").unwrap();
    assert_eq!(argv[i + 1], "recorded-session-1");
}
