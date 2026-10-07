//! Codex CLI: `codex exec --json`, one process per turn.
//!
//! Checked against codex-cli 0.160.1 (see `docs/DECISIONS.md`, M5-12):
//! - `--ignore-user-config` skips the user's `config.toml` but keeps the login stored in
//!   `CODEX_HOME`, so nothing of the user's is copied or replaced;
//! - Switchyard's MCP server comes in as `-c mcp_servers.switchyard.*` overrides (Codex's own
//!   TOML config syntax). `env_vars` names the variables Codex passes to `swy mcp` from its
//!   own environment, so the session token is never on a command line or in a file;
//! - `default_tools_approval_mode = "approve"` lets the read-only tools run in `exec`, where
//!   nobody could answer an approval prompt;
//! - built-in tools are switched off by feature flag (shell, exec, images, browser, computer
//!   use, sub-agents, plugins, …) and web search by config, the sandbox is read-only and
//!   approvals are never asked, so Codex can do nothing but call Switchyard;
//! - the prompt goes in on stdin (`-`), instructions as `developer_instructions`;
//! - follow-ups: `codex exec resume [options] <thread id> -`.

use std::collections::HashSet;

use serde_json::Value;

use crate::adapter::{AgentAdapter, Invocation, RunContext, StreamParser};
use crate::{AgentError, AgentEvent, AgentKind, MCP_SERVER_NAME, RunSummary};

/// Built-in Codex features turned off for every run.
pub const DISABLED_FEATURES: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "view_image",
    "image_generation",
    "multi_agent",
    "goals",
    "browser_use",
    "in_app_browser",
    "computer_use",
    "apps",
    "plugins",
    "skill_search",
    "tool_suggest",
    "sleep_tool",
];

/// How long Codex waits for one Switchyard tool call, in seconds.
const TOOL_TIMEOUT_SECS: u64 = 150;

/// The Codex CLI adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Codex;

/// A TOML basic string.
pub fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The argv after `codex`.
pub fn args(ctx: &RunContext<'_>) -> Vec<String> {
    let mut a: Vec<String> = vec!["exec".into()];
    let resume = ctx.resume.filter(|r| !r.trim().is_empty());
    if resume.is_some() {
        a.push("resume".into());
    }
    a.extend(["--json", "--skip-git-repo-check", "--ignore-user-config"].map(String::from));
    for f in DISABLED_FEATURES {
        a.extend(["--disable".into(), (*f).to_owned()]);
    }
    let server = format!("mcp_servers.{MCP_SERVER_NAME}");
    let names: Vec<String> = ctx.mcp.env.iter().map(|(k, _)| toml_str(k)).collect();
    let args: Vec<String> = ctx.mcp.args.iter().map(|x| toml_str(x)).collect();
    let config = [
        "web_search=\"disabled\"".to_owned(),
        "sandbox_mode=\"read-only\"".to_owned(),
        "approval_policy=\"never\"".to_owned(),
        format!("developer_instructions={}", toml_str(ctx.system_prompt)),
        format!(
            "{server}.command={}",
            toml_str(&ctx.mcp.command.to_string_lossy())
        ),
        format!("{server}.args=[{}]", args.join(",")),
        format!("{server}.env_vars=[{}]", names.join(",")),
        format!("{server}.default_tools_approval_mode=\"approve\""),
        format!("{server}.tool_timeout_sec={TOOL_TIMEOUT_SECS}"),
    ];
    for c in config {
        a.extend(["-c".into(), c]);
    }
    if let Some(m) = ctx.model.filter(|m| !m.trim().is_empty()) {
        a.extend(["-m".into(), m.to_owned()]);
    }
    a.extend(ctx.extra_args.iter().cloned());
    if let Some(r) = resume {
        a.push(r.to_owned());
    }
    // The prompt comes from stdin.
    a.push("-".into());
    a
}

impl AgentAdapter for Codex {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn program(&self) -> &str {
        "codex"
    }

    fn prepare(&self, ctx: &RunContext<'_>) -> Result<Invocation, AgentError> {
        Ok(Invocation {
            args: args(ctx),
            // Codex hands these to `swy mcp` through `env_vars`.
            env: ctx.mcp.env.clone(),
            stdin: Some(ctx.prompt.to_owned()),
        })
    }

    fn parser(&self) -> Box<dyn StreamParser> {
        Box::new(Parser::default())
    }
}

/// `codex exec --json` output → [`AgentEvent`]s.
#[derive(Debug, Default)]
pub struct Parser {
    session_id: Option<String>,
    started_tools: HashSet<String>,
    /// The turn's messages, for the summary.
    answer: String,
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

/// An MCP result's `content` blocks as text.
fn content_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| match b["type"].as_str() {
            Some("text") => s(&b["text"]),
            Some("image") => "[image]".into(),
            _ => b.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Parser {
    fn tool_name(item: &Value) -> String {
        // Switchyard's tools under their own names; anything else says where it came from.
        let tool = s(&item["tool"]);
        match item["server"].as_str() {
            Some(MCP_SERVER_NAME) | None => tool,
            Some(server) => format!("{server}.{tool}"),
        }
    }

    fn call(&mut self, item: &Value, out: &mut Vec<AgentEvent>) {
        let id = s(&item["id"]);
        if self.started_tools.insert(id.clone()) {
            out.push(AgentEvent::ToolCall {
                id,
                name: Self::tool_name(item),
                arguments: item["arguments"].clone(),
            });
        }
    }

    fn item(&mut self, item: &Value, completed: bool, out: &mut Vec<AgentEvent>) {
        match item["type"].as_str().unwrap_or_default() {
            "mcp_tool_call" => {
                self.call(item, out);
                if completed {
                    let error = item["error"]["message"]
                        .as_str()
                        .or_else(|| item["error"].as_str())
                        .map(str::to_owned);
                    let failed = error.is_some() || item["status"] == "failed";
                    out.push(AgentEvent::ToolResult {
                        id: s(&item["id"]),
                        text: error.unwrap_or_else(|| content_text(&item["result"])),
                        is_error: failed,
                    });
                }
            }
            // Should never happen with the shell off; shown, not hidden, if it does.
            "command_execution" if !completed => out.push(AgentEvent::ToolCall {
                id: s(&item["id"]),
                name: "shell".into(),
                arguments: Value::String(s(&item["command"])),
            }),
            "agent_message" if completed => {
                let text = s(&item["text"]);
                if !text.is_empty() {
                    if !self.answer.is_empty() {
                        self.answer.push_str("\n\n");
                        out.push(AgentEvent::Text("\n\n".into()));
                    }
                    self.answer.push_str(&text);
                    out.push(AgentEvent::Text(text));
                }
            }
            "reasoning" if completed => {
                let text = s(&item["text"]);
                if !text.is_empty() {
                    out.push(AgentEvent::Thinking(text));
                }
            }
            // Warnings Codex reports as items (unknown model metadata, …).
            "error" if completed => out.push(AgentEvent::Log(s(&item["message"]))),
            _ => {}
        }
    }
}

impl StreamParser for Parser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or_default() {
            "thread.started" => {
                let id = s(&v["thread_id"]);
                self.session_id = (!id.is_empty()).then_some(id);
                out.push(AgentEvent::Started {
                    session_id: self.session_id.clone(),
                    model: None,
                });
            }
            "item.started" | "item.updated" => self.item(&v["item"], false, &mut out),
            "item.completed" => self.item(&v["item"], true, &mut out),
            "turn.completed" => {
                let u = &v["usage"];
                out.push(AgentEvent::Done(RunSummary {
                    text: std::mem::take(&mut self.answer),
                    session_id: self.session_id.clone(),
                    cost_usd: None,
                    duration_ms: None,
                    turns: Some(1),
                    input_tokens: u["input_tokens"].as_u64(),
                    output_tokens: u["output_tokens"].as_u64(),
                }));
            }
            "turn.failed" => out.push(AgentEvent::Error(
                v["error"]["message"]
                    .as_str()
                    .unwrap_or("Codex reported an error")
                    .to_owned(),
            )),
            "error" => out.push(AgentEvent::Error(
                v["message"]
                    .as_str()
                    .unwrap_or("Codex reported an error")
                    .to_owned(),
            )),
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpServer;
    use serde_json::json;

    fn feed_all(lines: &str) -> Vec<AgentEvent> {
        let mut p = Parser::default();
        lines.lines().flat_map(|l| p.feed(l)).collect()
    }

    #[test]
    fn tool_calls_text_and_outcome() {
        let events = feed_all(concat!(
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            "\n",
            r#"{"type":"item.started","item":{"id":"i1","type":"mcp_tool_call","server":"switchyard","tool":"explain","arguments":{"connection":"shop"},"status":"in_progress"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i1","type":"mcp_tool_call","server":"switchyard","tool":"explain","arguments":{"connection":"shop"},"result":{"content":[{"type":"text","text":"Seq Scan"}]},"error":null,"status":"completed"}}"#,
            "\n",
            // A completed call without its start still shows the call.
            r#"{"type":"item.completed","item":{"id":"i2","type":"mcp_tool_call","server":"switchyard","tool":"workload","arguments":{},"result":null,"error":{"message":"denied"},"status":"failed"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i3","type":"reasoning","text":"thinking"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i4","type":"agent_message","text":"Add an index."}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":4}}"#,
        ));
        assert_eq!(
            events,
            [
                AgentEvent::Started {
                    session_id: Some("t-1".into()),
                    model: None
                },
                AgentEvent::ToolCall {
                    id: "i1".into(),
                    name: "explain".into(),
                    arguments: json!({"connection": "shop"})
                },
                AgentEvent::ToolResult {
                    id: "i1".into(),
                    text: "Seq Scan".into(),
                    is_error: false
                },
                AgentEvent::ToolCall {
                    id: "i2".into(),
                    name: "workload".into(),
                    arguments: json!({})
                },
                AgentEvent::ToolResult {
                    id: "i2".into(),
                    text: "denied".into(),
                    is_error: true
                },
                AgentEvent::Thinking("thinking".into()),
                AgentEvent::Text("Add an index.".into()),
                AgentEvent::Done(RunSummary {
                    text: "Add an index.".into(),
                    session_id: Some("t-1".into()),
                    turns: Some(1),
                    input_tokens: Some(3),
                    output_tokens: Some(4),
                    ..Default::default()
                }),
            ]
        );
    }

    #[test]
    fn failures() {
        assert_eq!(
            feed_all(r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#),
            [AgentEvent::Error("401 Unauthorized".into())]
        );
        assert_eq!(
            feed_all(r#"{"type":"error","message":"stream disconnected"}"#),
            [AgentEvent::Error("stream disconnected".into())]
        );
    }

    #[test]
    fn toml_strings() {
        assert_eq!(toml_str("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
        assert_eq!(toml_str("C:\\swy.exe"), r#""C:\\swy.exe""#);
    }

    fn ctx_args(resume: Option<&str>) -> Vec<String> {
        let mcp = McpServer {
            command: "/opt/swy".into(),
            args: vec!["mcp".into()],
            env: vec![
                ("SWITCHYARD_MCP_TOKEN".into(), "secret-token".into()),
                ("SWITCHYARD_AGENT".into(), "codex".into()),
            ],
        };
        let dir = std::env::temp_dir();
        let ctx = RunContext {
            workdir: &dir,
            prompt: "why slow?",
            resume,
            model: Some("gpt-5"),
            mcp: &mcp,
            system_prompt: "be \"brief\"\nplease",
            extra_args: &[],
        };
        let inv = Codex.prepare(&ctx).unwrap();
        assert_eq!(inv.stdin.as_deref(), Some("why slow?"));
        assert!(
            inv.env
                .iter()
                .any(|(k, v)| k == "SWITCHYARD_MCP_TOKEN" && v == "secret-token")
        );
        inv.args
    }

    #[test]
    fn invocation_allows_only_switchyard() {
        let a = ctx_args(None);
        assert_eq!(&a[..2], ["exec", "--json"]);
        assert_eq!(a.last().map(String::as_str), Some("-"));
        for f in ["shell_tool", "unified_exec", "view_image", "multi_agent"] {
            assert!(
                a.windows(2).any(|w| w[0] == "--disable" && w[1] == f),
                "{f} not disabled"
            );
        }
        let configs: Vec<&str> = a
            .windows(2)
            .filter(|w| w[0] == "-c")
            .map(|w| w[1].as_str())
            .collect();
        for c in [
            "web_search=\"disabled\"",
            "sandbox_mode=\"read-only\"",
            "approval_policy=\"never\"",
            "mcp_servers.switchyard.command=\"/opt/swy\"",
            "mcp_servers.switchyard.args=[\"mcp\"]",
            "mcp_servers.switchyard.env_vars=[\"SWITCHYARD_MCP_TOKEN\",\"SWITCHYARD_AGENT\"]",
            "mcp_servers.switchyard.default_tools_approval_mode=\"approve\"",
            "developer_instructions=\"be \\\"brief\\\"\\nplease\"",
        ] {
            assert!(configs.contains(&c), "missing -c {c}: {configs:?}");
        }
        assert!(
            !a.iter()
                .any(|x| x.contains("secret-token") || x.contains("why slow"))
        );
        assert!(a.contains(&"--ignore-user-config".into()));
    }

    #[test]
    fn resume_puts_the_thread_last() {
        let a = ctx_args(Some("t-1"));
        assert_eq!(&a[..2], ["exec", "resume"]);
        assert_eq!(&a[a.len() - 2..], ["t-1", "-"]);
    }
}
