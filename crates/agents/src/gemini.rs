//! Gemini CLI: `gemini -p "" --output-format stream-json`, one process per turn.
//!
//! Checked against Gemini CLI 0.63 (see `docs/DECISIONS.md`, M5-13):
//! - Switchyard's MCP server is in `.gemini/settings.json` inside the run's private directory
//!   (Gemini's workspace settings), token in its `env` block. Gemini connects workspace MCP
//!   servers only in trusted folders: `GEMINI_CLI_TRUST_WORKSPACE=true` plus `--skip-trust`
//!   trust that directory for this process without touching the user's trust list;
//! - an *admin* policy file (`--admin-policy`, the highest tier, so the user's own policies
//!   cannot widen it) allows Switchyard's MCP tools and denies every other tool; denied tools
//!   are not even shown to the model. `--allowed-mcp-server-names switchyard` and `-e none`
//!   keep the user's other MCP servers and extensions out;
//! - the prompt goes in on stdin (`-p ""` is appended to it), instructions as `GEMINI.md`
//!   (workspace context);
//! - follow-ups: sessions are stored per project directory and every run has a new one, so
//!   `--resume <id>` cannot find them; the adapter finds the session's file under Gemini's
//!   home and passes `--session-file`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::adapter::{AgentAdapter, Invocation, RunContext, StreamParser};
use crate::process::write_private;
use crate::{AgentError, AgentEvent, AgentKind, MCP_SERVER_NAME, RunSummary};

/// How long Gemini waits for one Switchyard tool call.
const TOOL_TIMEOUT_MS: u64 = 150_000;

/// The admin policy: Switchyard's tools, nothing else.
pub const POLICY: &str = r#"# Written by Switchyard for one assistant run.
[[rule]]
mcpName = "switchyard"
toolName = "*"
decision = "allow"
priority = 999

[[rule]]
toolName = "*"
decision = "deny"
priority = 998
"#;

/// The Gemini CLI adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gemini;

/// The workspace settings Gemini reads from `<workdir>/.gemini/settings.json`.
pub fn settings(ctx: &RunContext<'_>) -> Value {
    let env: serde_json::Map<String, Value> = ctx
        .mcp
        .env
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "command": ctx.mcp.command.to_string_lossy(),
                "args": ctx.mcp.args,
                "env": env,
                "trust": true,
                "timeout": TOOL_TIMEOUT_MS,
            }
        }
    })
}

/// Gemini's home (`GEMINI_CLI_HOME`, else the user's home) from `env` or the process.
fn gemini_home(extra_env: &[(String, String)]) -> Option<PathBuf> {
    let get = |k: &str| {
        extra_env
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| std::ffi::OsString::from(v))
            .or_else(|| std::env::var_os(k))
    };
    get("GEMINI_CLI_HOME")
        .or_else(|| {
            if cfg!(windows) {
                get("USERPROFILE")
            } else {
                None
            }
        })
        .or_else(|| get("HOME"))
        .map(PathBuf::from)
}

/// The saved chat of session `id`: `<home>/.gemini/tmp/<project>/chats/session-…-<id8>.json[l]`
/// whose first record names the session.
pub fn find_session(home: &Path, id: &str) -> Option<PathBuf> {
    let short = id.get(..8)?;
    let projects = std::fs::read_dir(home.join(".gemini").join("tmp")).ok()?;
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for project in projects.flatten() {
        let Ok(chats) = std::fs::read_dir(project.path().join("chats")) else {
            continue;
        };
        for f in chats.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".jsonl")
                .or_else(|| name.strip_suffix(".json"));
            if !stem.is_some_and(|s| s.starts_with("session-") && s.ends_with(short)) {
                continue;
            }
            let head = std::fs::read_to_string(f.path()).unwrap_or_default();
            let first = head.lines().next().unwrap_or_default();
            let names_it = serde_json::from_str::<Value>(first)
                .ok()
                .is_some_and(|v| v["sessionId"] == id);
            if names_it {
                let modified = f.metadata().and_then(|m| m.modified()).ok();
                found.push((modified.unwrap_or(std::time::UNIX_EPOCH), f.path()));
            }
        }
    }
    found.sort();
    found.pop().map(|(_, p)| p)
}

/// The argv after `gemini`, given the policy path and the session file to continue.
pub fn args(ctx: &RunContext<'_>, policy: &Path, session: Option<&Path>) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "",
        "--output-format",
        "stream-json",
        "--skip-trust",
        "--allowed-mcp-server-names",
        MCP_SERVER_NAME,
        "-e",
        "none",
        "--admin-policy",
    ]
    .map(String::from)
    .into();
    a.push(policy.to_string_lossy().into_owned());
    if let Some(m) = ctx.model.filter(|m| !m.trim().is_empty()) {
        a.extend(["-m".into(), m.to_owned()]);
    }
    if let Some(s) = session {
        a.extend(["--session-file".into(), s.to_string_lossy().into_owned()]);
    }
    a.extend(ctx.extra_args.iter().cloned());
    a
}

impl AgentAdapter for Gemini {
    fn kind(&self) -> AgentKind {
        AgentKind::Gemini
    }

    fn program(&self) -> &str {
        "gemini"
    }

    fn prepare(&self, ctx: &RunContext<'_>) -> Result<Invocation, AgentError> {
        let dir = ctx.workdir.join(".gemini");
        std::fs::create_dir_all(&dir)?;
        let s = serde_json::to_vec_pretty(&settings(ctx)).map_err(std::io::Error::other)?;
        write_private(&dir.join("settings.json"), &s)?;
        let policy = ctx.workdir.join("switchyard-policy.toml");
        write_private(&policy, POLICY.as_bytes())?;
        std::fs::write(ctx.workdir.join("GEMINI.md"), ctx.system_prompt)?;
        let env = vec![("GEMINI_CLI_TRUST_WORKSPACE".to_owned(), "true".to_owned())];
        let session = match ctx.resume.filter(|r| !r.trim().is_empty()) {
            Some(id) => {
                let file = gemini_home(ctx.extra_env).and_then(|h| find_session(&h, id));
                if file.is_none() {
                    tracing::warn!(
                        session = id,
                        "Gemini session file not found; starting a new conversation"
                    );
                }
                file
            }
            None => None,
        };
        Ok(Invocation {
            args: args(ctx, &policy, session.as_deref()),
            env,
            stdin: Some(ctx.prompt.to_owned()),
        })
    }

    fn parser(&self) -> Box<dyn StreamParser> {
        Box::new(Parser::default())
    }
}

/// Gemini's stream-json output → [`AgentEvent`]s.
#[derive(Debug, Default)]
pub struct Parser {
    session_id: Option<String>,
    answer: String,
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn tool_name(name: &str) -> String {
    name.strip_prefix(&format!("mcp_{MCP_SERVER_NAME}_"))
        .unwrap_or(name)
        .to_owned()
}

impl StreamParser for Parser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return Vec::new();
        };
        match v["type"].as_str().unwrap_or_default() {
            "init" => {
                let id = s(&v["session_id"]);
                self.session_id = (!id.is_empty()).then_some(id);
                vec![AgentEvent::Started {
                    session_id: self.session_id.clone(),
                    model: v["model"].as_str().map(str::to_owned),
                }]
            }
            "message" if v["role"] == "assistant" => {
                let text = s(&v["content"]);
                if text.is_empty() {
                    return Vec::new();
                }
                self.answer.push_str(&text);
                vec![AgentEvent::Text(text)]
            }
            "tool_use" => vec![AgentEvent::ToolCall {
                id: s(&v["tool_id"]),
                name: tool_name(&s(&v["tool_name"])),
                arguments: v["parameters"].clone(),
            }],
            "tool_result" => {
                let ok = v["status"] == "success";
                let text = match v["output"].as_str() {
                    Some(o) if !o.is_empty() => o.to_owned(),
                    _ => s(&v["error"]["message"]),
                };
                vec![AgentEvent::ToolResult {
                    id: s(&v["tool_id"]),
                    text,
                    is_error: !ok,
                }]
            }
            "error" => vec![AgentEvent::Log(
                v["message"]
                    .as_str()
                    .or_else(|| v["error"]["message"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
            )],
            "result" => {
                if v["status"] != "success" {
                    let msg = v["error"]["message"]
                        .as_str()
                        .unwrap_or("Gemini CLI reported an error");
                    return vec![AgentEvent::Error(msg.to_owned())];
                }
                let st = &v["stats"];
                vec![AgentEvent::Done(RunSummary {
                    text: std::mem::take(&mut self.answer),
                    session_id: self.session_id.clone(),
                    cost_usd: None,
                    duration_ms: st["duration_ms"].as_u64(),
                    turns: Some(1),
                    input_tokens: st["input_tokens"].as_u64(),
                    output_tokens: st["output_tokens"].as_u64(),
                })]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpServer;

    fn feed_all(lines: &str) -> Vec<AgentEvent> {
        let mut p = Parser::default();
        lines.lines().flat_map(|l| p.feed(l)).collect()
    }

    #[test]
    fn events() {
        let events = feed_all(concat!(
            r#"{"type":"init","session_id":"s-1","model":"gemini-2.5-flash"}"#,
            "\n",
            r#"{"type":"message","role":"user","content":"q"}"#,
            "\n",
            r#"{"type":"tool_use","tool_name":"mcp_switchyard_explain","tool_id":"t1","parameters":{"connection":"shop"}}"#,
            "\n",
            r#"{"type":"tool_result","tool_id":"t1","status":"success","output":"Seq Scan"}"#,
            "\n",
            r#"{"type":"tool_result","tool_id":"t2","status":"error","output":"","error":{"message":"denied"}}"#,
            "\n",
            r#"{"type":"message","role":"assistant","content":"Add ","delta":true}"#,
            "\n",
            r#"{"type":"message","role":"assistant","content":"an index.","delta":true}"#,
            "\n",
            r#"{"type":"result","status":"success","stats":{"input_tokens":3,"output_tokens":4,"duration_ms":9}}"#,
        ));
        assert_eq!(
            events,
            [
                AgentEvent::Started {
                    session_id: Some("s-1".into()),
                    model: Some("gemini-2.5-flash".into())
                },
                AgentEvent::ToolCall {
                    id: "t1".into(),
                    name: "explain".into(),
                    arguments: json!({"connection": "shop"})
                },
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    text: "Seq Scan".into(),
                    is_error: false
                },
                AgentEvent::ToolResult {
                    id: "t2".into(),
                    text: "denied".into(),
                    is_error: true
                },
                AgentEvent::Text("Add ".into()),
                AgentEvent::Text("an index.".into()),
                AgentEvent::Done(RunSummary {
                    text: "Add an index.".into(),
                    session_id: Some("s-1".into()),
                    duration_ms: Some(9),
                    turns: Some(1),
                    input_tokens: Some(3),
                    output_tokens: Some(4),
                    ..Default::default()
                }),
            ]
        );
        assert_eq!(
            feed_all(r#"{"type":"result","status":"error","error":{"message":"quota"}}"#),
            [AgentEvent::Error("quota".into())]
        );
    }

    #[test]
    fn workspace_files_and_resume() {
        let work = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // A saved session in some other project's folder.
        let chats = home.path().join(".gemini/tmp/old-run/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let id = "7d0c8c1e-1111-4222-8333-944455556666";
        std::fs::write(
            chats.join("session-2026-10-07T16-44-7d0c8c1e.jsonl"),
            format!("{{\"sessionId\":\"{id}\",\"kind\":\"main\"}}\n"),
        )
        .unwrap();
        let mcp = McpServer {
            command: "/opt/swy".into(),
            args: vec!["mcp".into()],
            env: vec![("SWITCHYARD_MCP_TOKEN".into(), "secret-token".into())],
        };
        let extra_env = vec![(
            "GEMINI_CLI_HOME".to_owned(),
            home.path().to_string_lossy().into_owned(),
        )];
        let ctx = RunContext {
            workdir: work.path(),
            prompt: "why slow?",
            resume: Some(id),
            model: None,
            mcp: &mcp,
            system_prompt: "be brief",
            extra_args: &[],
            extra_env: &extra_env,
        };
        let inv = Gemini.prepare(&ctx).unwrap();
        let cfg: Value = serde_json::from_slice(
            &std::fs::read(work.path().join(".gemini/settings.json")).unwrap(),
        )
        .unwrap();
        let server = &cfg["mcpServers"]["switchyard"];
        assert_eq!(server["env"]["SWITCHYARD_MCP_TOKEN"], "secret-token");
        assert_eq!(server["trust"], true);
        assert_eq!(
            std::fs::read_to_string(work.path().join("GEMINI.md")).unwrap(),
            "be brief"
        );
        let a = &inv.args;
        let after = |f: &str| a[a.iter().position(|x| x == f).unwrap() + 1].clone();
        assert_eq!(after("--allowed-mcp-server-names"), "switchyard");
        assert_eq!(after("-e"), "none");
        assert_eq!(
            std::fs::read_to_string(after("--admin-policy")).unwrap(),
            POLICY
        );
        assert!(after("--session-file").ends_with("session-2026-10-07T16-44-7d0c8c1e.jsonl"));
        assert!(
            !a.iter()
                .any(|x| x.contains("secret-token") || x.contains("why slow"))
        );
        assert!(
            inv.env
                .contains(&("GEMINI_CLI_TRUST_WORKSPACE".into(), "true".into()))
        );
        assert_eq!(inv.stdin.as_deref(), Some("why slow?"));
        // An unknown session starts afresh.
        assert!(find_session(home.path(), "00000000-0000-0000-0000-000000000000").is_none());
    }
}
