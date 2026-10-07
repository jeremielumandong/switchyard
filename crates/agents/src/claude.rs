//! Claude Code: `claude -p` with stream-json in and out.
//!
//! Scope, deliberately narrow (flags checked against Claude Code 2.1):
//! - `--tools ""` removes every built-in tool (shell, file reads and edits, web), and
//!   `--restricted` ignores the user's and project's settings files, so nothing there can
//!   add tools or permissions back;
//! - `--strict-mcp-config --mcp-config <file>` attaches only Switchyard's MCP server; the
//!   session token sits in that file's `env` block (owner-only, in the run's private
//!   directory), never on a command line;
//! - `--allowedTools mcp__switchyard` pre-approves Switchyard's tools (all read-only
//!   server-side) and `--permission-mode dontAsk` refuses anything else instead of asking;
//! - the prompt goes in as one stream-json user message on stdin, so it never shows in a
//!   process list or meets Windows' command-line limit.
//!
//! Output is one JSON object per line. This CLI version sends each content block of an
//! assistant message as its own event; older versions resent the whole message each time.
//! The parser handles both by remembering what it emitted per message id.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::adapter::{AgentAdapter, Invocation, RunContext, StreamParser};
use crate::process::write_private;
use crate::{AgentError, AgentEvent, AgentKind, MCP_SERVER_NAME, RunSummary};

/// How long Claude Code waits for one MCP tool call: the longest `run_query` timeout
/// (120 s) plus room for connecting.
const MCP_TOOL_TIMEOUT_MS: u64 = 150_000;

/// The Claude Code adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeCode;

/// Claude Code's name for Switchyard's tools (`mcp__switchyard__explain`).
fn tool_prefix() -> String {
    format!("mcp__{MCP_SERVER_NAME}__")
}

/// The MCP config file Claude Code reads with `--mcp-config`.
pub fn mcp_config(ctx: &RunContext<'_>) -> Value {
    let env: serde_json::Map<String, Value> = ctx
        .mcp
        .env
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "type": "stdio",
                "command": ctx.mcp.command.to_string_lossy(),
                "args": ctx.mcp.args,
                "env": env,
            }
        }
    })
}

/// The argv after `claude`.
pub fn args(ctx: &RunContext<'_>, mcp_config: &std::path::Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--restricted",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
    ]
    .map(String::from)
    .into();
    a.push(mcp_config.to_string_lossy().into_owned());
    a.extend([
        "--allowedTools".into(),
        format!("mcp__{MCP_SERVER_NAME}"),
        "--permission-mode".into(),
        "dontAsk".into(),
        "--append-system-prompt".into(),
        ctx.system_prompt.to_owned(),
    ]);
    if let Some(m) = ctx.model.filter(|m| !m.trim().is_empty()) {
        a.extend(["--model".into(), m.to_owned()]);
    }
    if let Some(r) = ctx.resume.filter(|r| !r.trim().is_empty()) {
        a.extend(["--resume".into(), r.to_owned()]);
    }
    a.extend(ctx.extra_args.iter().cloned());
    a
}

/// One stream-json user message.
pub fn user_message(text: &str) -> String {
    let mut line = json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
    })
    .to_string();
    line.push('\n');
    line
}

impl AgentAdapter for ClaudeCode {
    fn kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }

    fn program(&self) -> &str {
        "claude"
    }

    fn prepare(&self, ctx: &RunContext<'_>) -> Result<Invocation, AgentError> {
        let path = ctx.workdir.join("mcp.json");
        let config = serde_json::to_vec_pretty(&mcp_config(ctx)).map_err(std::io::Error::other)?;
        write_private(&path, &config)?;
        Ok(Invocation {
            args: args(ctx, &path),
            env: vec![("MCP_TOOL_TIMEOUT".into(), MCP_TOOL_TIMEOUT_MS.to_string())],
            stdin: Some(user_message(ctx.prompt)),
        })
    }

    fn parser(&self) -> Box<dyn StreamParser> {
        Box::new(Parser::default())
    }

    fn interactive(&self, ctx: &RunContext<'_>) -> Option<Result<Invocation, AgentError>> {
        Some((|| {
            let path = ctx.workdir.join("mcp.json");
            let config =
                serde_json::to_vec_pretty(&mcp_config(ctx)).map_err(std::io::Error::other)?;
            write_private(&path, &config)?;
            // The same restrictions as a run, minus the headless stream: no built-in tools,
            // only Switchyard's server; the person in the terminal answers any prompt.
            let mut a: Vec<String> = [
                "--restricted",
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
            ]
            .map(String::from)
            .into();
            a.push(path.to_string_lossy().into_owned());
            a.extend([
                "--allowedTools".into(),
                format!("mcp__{MCP_SERVER_NAME}"),
                "--append-system-prompt".into(),
                ctx.system_prompt.to_owned(),
            ]);
            if let Some(m) = ctx.model.filter(|m| !m.trim().is_empty()) {
                a.extend(["--model".into(), m.to_owned()]);
            }
            if let Some(r) = ctx.resume.filter(|r| !r.trim().is_empty()) {
                a.extend(["--resume".into(), r.to_owned()]);
            }
            a.extend(ctx.extra_args.iter().cloned());
            Ok(Invocation {
                args: a,
                env: vec![("MCP_TOOL_TIMEOUT".into(), MCP_TOOL_TIMEOUT_MS.to_string())],
                stdin: None,
            })
        })())
    }
}

/// Claude Code's stream-json output → [`AgentEvent`]s.
#[derive(Debug, Default)]
pub struct Parser {
    /// Text and thinking already emitted, per assistant message id.
    emitted: HashMap<String, (String, String)>,
    tools: HashSet<String>,
    session_id: Option<String>,
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

/// A `tool_result`'s `content` (string or blocks) as text.
fn result_text(c: &Value) -> String {
    match c {
        Value::String(t) => t.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => s(&b["text"]),
                Some("image") => "[image]".into(),
                _ => b.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// What `now` adds to `before`: the suffix when `now` repeats it (cumulative messages),
/// nothing for a resent block, else all of `now` (one block per event).
fn new_part(before: &mut String, now: &str) -> Option<String> {
    if now.is_empty() {
        return None;
    }
    if let Some(rest) = now.strip_prefix(before.as_str()) {
        if rest.is_empty() {
            return None;
        }
        let rest = rest.to_owned();
        *before = now.to_owned();
        return Some(rest);
    }
    if before.ends_with(now) {
        return None;
    }
    before.push_str(now);
    Some(now.to_owned())
}

fn normalize_tool(name: &str) -> String {
    name.strip_prefix(&tool_prefix()).unwrap_or(name).to_owned()
}

impl Parser {
    fn assistant(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        let msg = &v["message"];
        let id = s(&msg["id"]);
        let (mut text, mut thinking) = (String::new(), String::new());
        for b in msg["content"].as_array().into_iter().flatten() {
            match b["type"].as_str() {
                Some("text") => text.push_str(b["text"].as_str().unwrap_or_default()),
                Some("thinking") => thinking.push_str(b["thinking"].as_str().unwrap_or_default()),
                Some("tool_use") => {
                    let tid = s(&b["id"]);
                    let name = s(&b["name"]);
                    if !tid.is_empty() && !name.is_empty() && self.tools.insert(tid.clone()) {
                        out.push(AgentEvent::ToolCall {
                            id: tid,
                            name: normalize_tool(&name),
                            arguments: b["input"].clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        let (seen_text, seen_thinking) = self.emitted.entry(id).or_default();
        // Thinking, then text, then the tool calls the message makes.
        let mut first = Vec::new();
        if let Some(t) = new_part(seen_thinking, &thinking) {
            first.push(AgentEvent::Thinking(t));
        }
        if let Some(t) = new_part(seen_text, &text) {
            first.push(AgentEvent::Text(t));
        }
        out.splice(0..0, first);
    }

    fn result(&mut self, v: &Value) -> AgentEvent {
        let sid = s(&v["session_id"]);
        if !sid.is_empty() {
            self.session_id = Some(sid);
        }
        let subtype = s(&v["subtype"]);
        let text = v["result"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or_default()
            .to_owned();
        if v["is_error"].as_bool().unwrap_or(false) || subtype.starts_with("error") {
            return AgentEvent::Error(if text.is_empty() {
                let reason = if subtype.is_empty() {
                    "unknown"
                } else {
                    &subtype
                };
                format!("Claude Code reported an error ({reason})")
            } else {
                text
            });
        }
        let u = &v["usage"];
        AgentEvent::Done(RunSummary {
            text,
            session_id: self.session_id.clone(),
            cost_usd: v
                .get("total_cost_usd")
                .or_else(|| v.get("cost_usd"))
                .and_then(Value::as_f64),
            duration_ms: v["duration_ms"].as_u64(),
            turns: v["num_turns"].as_u64(),
            input_tokens: u["input_tokens"].as_u64(),
            output_tokens: u["output_tokens"].as_u64(),
        })
    }
}

impl StreamParser for Parser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or_default() {
            "system" if v["subtype"] == "init" => {
                let sid = s(&v["session_id"]);
                if !sid.is_empty() {
                    self.session_id = Some(sid);
                }
                out.push(AgentEvent::Started {
                    session_id: self.session_id.clone(),
                    model: v["model"].as_str().map(str::to_owned),
                });
            }
            "assistant" => self.assistant(&v, &mut out),
            "user" => {
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_result" {
                        out.push(AgentEvent::ToolResult {
                            id: s(&b["tool_use_id"]),
                            text: result_text(&b["content"]),
                            is_error: b["is_error"].as_bool().unwrap_or(false),
                        });
                    }
                }
            }
            "result" => out.push(self.result(&v)),
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpServer;
    use std::path::Path;

    fn feed_all(lines: &str) -> Vec<AgentEvent> {
        let mut p = Parser::default();
        lines.lines().flat_map(|l| p.feed(l)).collect()
    }

    #[test]
    fn one_block_per_event_and_cumulative_messages() {
        // Current CLI: each block in its own event (same message id).
        let events = feed_all(concat!(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"hmm"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Hello"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":" world"}]}}"#,
            "\n",
            // Older CLI: the whole message again, grown.
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Ab"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Abc"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Abc"}]}}"#,
        ));
        assert_eq!(
            events,
            [
                AgentEvent::Thinking("hmm".into()),
                AgentEvent::Text("Hello".into()),
                AgentEvent::Text(" world".into()),
                AgentEvent::Text("Ab".into()),
                AgentEvent::Text("c".into()),
            ]
        );
    }

    #[test]
    fn tools_results_and_outcome() {
        let events = feed_all(concat!(
            r#"{"type":"system","subtype":"init","session_id":"s-1","model":"m","tools":[]}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"mcp__switchyard__explain","input":{"connection":"shop"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"mcp__switchyard__explain","input":{"connection":"shop"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"Seq Scan"}]}]}}"#,
            "\n",
            "not json\n",
            r#"{"type":"rate_limit_event"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Add an index.","session_id":"s-1","total_cost_usd":0.01,"num_turns":3,"duration_ms":900,"usage":{"input_tokens":5,"output_tokens":7}}"#,
        ));
        assert_eq!(
            events,
            [
                AgentEvent::Started {
                    session_id: Some("s-1".into()),
                    model: Some("m".into())
                },
                AgentEvent::ToolCall {
                    id: "t1".into(),
                    name: "explain".into(),
                    arguments: json!({"connection": "shop"}),
                },
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    text: "Seq Scan".into(),
                    is_error: false
                },
                AgentEvent::Done(RunSummary {
                    text: "Add an index.".into(),
                    session_id: Some("s-1".into()),
                    cost_usd: Some(0.01),
                    duration_ms: Some(900),
                    turns: Some(3),
                    input_tokens: Some(5),
                    output_tokens: Some(7),
                }),
            ]
        );
    }

    #[test]
    fn errors() {
        let events = feed_all(concat!(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key"}"#,
        ));
        assert_eq!(
            events,
            [
                AgentEvent::Error("Claude Code reported an error (error_max_turns)".into()),
                AgentEvent::Error("Invalid API key".into()),
            ]
        );
    }

    #[test]
    fn invocation_allows_only_switchyard() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = McpServer {
            command: "/opt/swy".into(),
            args: vec!["mcp".into()],
            env: vec![("SWITCHYARD_MCP_TOKEN".into(), "tok".into())],
        };
        let ctx = RunContext {
            workdir: dir.path(),
            prompt: "why is it \"slow\"?\nline 2",
            resume: Some("s-1"),
            model: Some("sonnet"),
            mcp: &mcp,
            system_prompt: "be brief",
            extra_args: &[],
            extra_env: &[],
        };
        let inv = ClaudeCode.prepare(&ctx).unwrap();
        let a = &inv.args;
        let after = |flag: &str| {
            let i = a.iter().position(|x| x == flag).unwrap();
            a[i + 1].as_str()
        };
        assert_eq!(after("--tools"), "");
        assert_eq!(after("--allowedTools"), "mcp__switchyard");
        assert_eq!(after("--permission-mode"), "dontAsk");
        assert_eq!(after("--resume"), "s-1");
        assert_eq!(after("--model"), "sonnet");
        assert!(a.contains(&"--restricted".into()) && a.contains(&"--strict-mcp-config".into()));
        assert!(
            !a.iter().any(|x| x.contains("tok") || x.contains("slow")),
            "no token or prompt in argv"
        );
        let cfg_path = Path::new(after("--mcp-config"));
        assert!(cfg_path.starts_with(dir.path()));
        let cfg: Value = serde_json::from_slice(&std::fs::read(cfg_path).unwrap()).unwrap();
        let server = &cfg["mcpServers"]["switchyard"];
        assert_eq!(server["command"], "/opt/swy");
        assert_eq!(server["args"], json!(["mcp"]));
        assert_eq!(server["env"]["SWITCHYARD_MCP_TOKEN"], "tok");
        let stdin: Value = serde_json::from_str(inv.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(
            stdin["message"]["content"][0]["text"],
            "why is it \"slow\"?\nline 2"
        );
    }
}
