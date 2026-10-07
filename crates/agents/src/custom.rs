//! A user-defined coding CLI: a command template, where the prompt goes, how to read its
//! output (plain text or JSON lines with a field mapping) and an MCP config template.
//!
//! Placeholders, in arguments and the MCP config template:
//! `{prompt}`, `{model}`, `{session}`, `{workdir}`, `{mcp_config}` (the written config file),
//! `{system_prompt}`; in the MCP template also `{command}`, `{command_json}`, `{args_json}`,
//! `{env_json}` (an object), `{env_names_json}` (an array) and `{server}`.
//!
//! The safety of a custom CLI rests on the MCP server (every tool is read-only there): a
//! custom command's own tools are whatever its template allows.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::adapter::{AgentAdapter, Invocation, RunContext, StreamParser};
use crate::process::write_private;
use crate::{AgentError, AgentEvent, AgentKind, MCP_SERVER_NAME, RunSummary};

/// A user-defined CLI (Settings → Assistant → Custom).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCli {
    /// Display name.
    pub name: String,
    /// Executable (name on PATH or a path).
    pub program: String,
    /// Arguments for one run; placeholders as above.
    pub args: Vec<String>,
    /// Arguments added when continuing a conversation (`["--resume", "{session}"]`).
    pub resume_args: Vec<String>,
    /// Arguments to start it interactively in a terminal ("Open in terminal").
    pub interactive_args: Vec<String>,
    /// Send the prompt on stdin instead of through `{prompt}`.
    pub prompt_on_stdin: bool,
    /// How to read its output.
    pub output: OutputFormat,
    /// MCP config file written into the run directory, if the CLI takes one.
    pub mcp_config: Option<McpConfigTemplate>,
}

/// How a custom CLI's standard output is read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum OutputFormat {
    /// Everything it prints is the answer.
    #[default]
    Text,
    /// One JSON object per line, mapped with JSON pointers.
    Jsonl(Box<JsonlMapping>),
}

/// Where things are in a JSON-lines event. Pointers are RFC 6901 (`/item/text`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JsonlMapping {
    /// The event's type field (`/type`); empty: every line is matched by its fields alone.
    pub type_field: String,
    /// Type of assistant text events (`message`), and where the text is (`/content`).
    pub text_type: String,
    /// Pointer to the text.
    pub text_field: String,
    /// Type of tool call events, and where the name, arguments and call id are.
    pub tool_type: String,
    /// Pointer to the tool name.
    pub tool_name_field: String,
    /// Pointer to the tool arguments.
    pub tool_args_field: String,
    /// Pointer to the tool call id.
    pub tool_id_field: String,
    /// Type of tool result events, and where their text is.
    pub result_type: String,
    /// Pointer to the result text.
    pub result_field: String,
    /// Type of the event that ends a successful run.
    pub done_type: String,
    /// Pointer to a session / conversation id, wherever it appears.
    pub session_field: String,
    /// Type of error events, and where the message is.
    pub error_type: String,
    /// Pointer to the error message.
    pub error_field: String,
}

/// An MCP config file for the CLI.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfigTemplate {
    /// File name inside the run directory (`mcp.json`, `.mycli/config.toml`).
    pub file: String,
    /// Contents, with placeholders.
    pub template: String,
}

impl CustomCli {
    /// A Claude-Code-compatible starting point for the settings page.
    pub fn example() -> Self {
        Self {
            name: "My CLI".into(),
            program: "mycli".into(),
            args: vec![
                "--mcp-config".into(),
                "{mcp_config}".into(),
                "--print".into(),
                "{prompt}".into(),
            ],
            resume_args: Vec::new(),
            interactive_args: vec!["--mcp-config".into(), "{mcp_config}".into()],
            prompt_on_stdin: false,
            output: OutputFormat::Text,
            mcp_config: Some(McpConfigTemplate {
                file: "mcp.json".into(),
                template: r#"{"mcpServers":{"{server}":{"command":{command_json},"args":{args_json},"env":{env_json}}}}"#.into(),
            }),
        }
    }
}

/// The adapter for one [`CustomCli`].
#[derive(Clone, Debug)]
pub struct Custom(pub CustomCli);

struct Values<'a> {
    pairs: Vec<(&'static str, String)>,
    _ctx: std::marker::PhantomData<&'a ()>,
}

impl Values<'_> {
    fn fill(&self, template: &str) -> String {
        let mut out = template.to_owned();
        for (k, v) in &self.pairs {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }
}

impl Custom {
    fn values<'a>(&self, ctx: &RunContext<'a>, config: Option<&std::path::Path>) -> Values<'a> {
        let mut pairs = vec![
            ("model", ctx.model.unwrap_or_default().to_owned()),
            ("session", ctx.resume.unwrap_or_default().to_owned()),
            ("workdir", ctx.workdir.to_string_lossy().into_owned()),
            ("system_prompt", ctx.system_prompt.to_owned()),
            ("server", MCP_SERVER_NAME.to_owned()),
            ("command", ctx.mcp.command.to_string_lossy().into_owned()),
            (
                "command_json",
                json!(ctx.mcp.command.to_string_lossy()).to_string(),
            ),
            ("args_json", json!(ctx.mcp.args).to_string()),
            (
                "env_json",
                Value::Object(
                    ctx.mcp
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), json!(v)))
                        .collect(),
                )
                .to_string(),
            ),
            (
                "env_names_json",
                json!(ctx.mcp.env.iter().map(|(k, _)| k).collect::<Vec<_>>()).to_string(),
            ),
            (
                "mcp_config",
                config
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        ];
        if !self.0.prompt_on_stdin {
            pairs.push(("prompt", ctx.prompt.to_owned()));
        }
        Values {
            pairs,
            _ctx: std::marker::PhantomData,
        }
    }

    fn write_config(&self, ctx: &RunContext<'_>) -> Result<Option<std::path::PathBuf>, AgentError> {
        let Some(t) = self
            .0
            .mcp_config
            .as_ref()
            .filter(|t| !t.file.trim().is_empty())
        else {
            return Ok(None);
        };
        let rel = std::path::Path::new(t.file.trim());
        // Inside the run directory only.
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(AgentError::Io(std::io::Error::other(
                "the MCP config file must be a relative path inside the run directory",
            )));
        }
        let path = ctx.workdir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = self.values(ctx, Some(&path)).fill(&t.template);
        write_private(&path, body.as_bytes())?;
        Ok(Some(path))
    }

    fn invocation(&self, ctx: &RunContext<'_>, args: &[String]) -> Result<Invocation, AgentError> {
        let config = self.write_config(ctx)?;
        let v = self.values(ctx, config.as_deref());
        let mut a: Vec<String> = args.iter().map(|x| v.fill(x)).collect();
        if ctx.resume.is_some_and(|r| !r.trim().is_empty()) {
            a.extend(self.0.resume_args.iter().map(|x| v.fill(x)));
        }
        a.extend(ctx.extra_args.iter().cloned());
        Ok(Invocation {
            args: a,
            env: ctx.mcp.env.clone(),
            stdin: self.0.prompt_on_stdin.then(|| ctx.prompt.to_owned()),
        })
    }
}

impl AgentAdapter for Custom {
    fn kind(&self) -> AgentKind {
        AgentKind::Custom
    }

    fn program(&self) -> &str {
        &self.0.program
    }

    fn prepare(&self, ctx: &RunContext<'_>) -> Result<Invocation, AgentError> {
        self.invocation(ctx, &self.0.args)
    }

    fn interactive(&self, ctx: &RunContext<'_>) -> Option<Result<Invocation, AgentError>> {
        if self.0.interactive_args.is_empty() && self.0.mcp_config.is_none() {
            return None;
        }
        Some(self.invocation(ctx, &self.0.interactive_args).map(|mut i| {
            i.stdin = None;
            i
        }))
    }

    fn parser(&self) -> Box<dyn StreamParser> {
        match &self.0.output {
            OutputFormat::Text => Box::new(TextParser::default()),
            OutputFormat::Jsonl(m) => Box::new(JsonlParser {
                map: (**m).clone(),
                answer: String::new(),
                session: None,
                done: false,
            }),
        }
    }
}

/// Plain text: every line is answer text; a clean exit is the outcome.
#[derive(Debug, Default)]
struct TextParser {
    answer: String,
}

impl StreamParser for TextParser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let text = format!("{line}\n");
        self.answer.push_str(&text);
        vec![AgentEvent::Text(text)]
    }

    fn finish(&mut self, exit: Option<i32>) -> Vec<AgentEvent> {
        match exit {
            Some(0) => vec![AgentEvent::Done(RunSummary {
                text: self.answer.trim_end().to_owned(),
                ..Default::default()
            })],
            _ => Vec::new(),
        }
    }
}

/// JSON lines mapped with [`JsonlMapping`].
#[derive(Debug)]
struct JsonlParser {
    map: JsonlMapping,
    answer: String,
    session: Option<String>,
    done: bool,
}

fn at<'v>(v: &'v Value, pointer: &str) -> Option<&'v Value> {
    if pointer.is_empty() {
        return None;
    }
    v.pointer(pointer)
}

fn text_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

impl JsonlParser {
    fn is(&self, v: &Value, ty: &str) -> bool {
        if ty.is_empty() {
            return false;
        }
        if self.map.type_field.is_empty() {
            return true;
        }
        at(v, &self.map.type_field).and_then(Value::as_str) == Some(ty)
    }
}

impl StreamParser for JsonlParser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(s) = text_of(at(&v, &self.map.session_field))
            && self.session.as_deref() != Some(&s)
        {
            out.push(AgentEvent::Started {
                session_id: Some(s.clone()),
                model: None,
            });
            self.session = Some(s);
        }
        let m = &self.map;
        if self.is(&v, &m.tool_type)
            && let Some(name) = text_of(at(&v, &m.tool_name_field))
        {
            out.push(AgentEvent::ToolCall {
                id: text_of(at(&v, &m.tool_id_field)).unwrap_or_default(),
                name: name
                    .strip_prefix(&format!("mcp__{MCP_SERVER_NAME}__"))
                    .unwrap_or(&name)
                    .to_owned(),
                arguments: at(&v, &m.tool_args_field).cloned().unwrap_or(Value::Null),
            });
        } else if self.is(&v, &m.result_type)
            && let Some(text) = text_of(at(&v, &m.result_field))
        {
            out.push(AgentEvent::ToolResult {
                id: text_of(at(&v, &m.tool_id_field)).unwrap_or_default(),
                text,
                is_error: false,
            });
        } else if self.is(&v, &m.error_type)
            && let Some(msg) = text_of(at(&v, &m.error_field))
        {
            self.done = true;
            out.push(AgentEvent::Error(msg));
        } else if self.is(&v, &m.text_type)
            && let Some(text) = text_of(at(&v, &m.text_field))
        {
            self.answer.push_str(&text);
            out.push(AgentEvent::Text(text));
        }
        if !self.done && self.is(&v, &m.done_type) {
            self.done = true;
            out.push(AgentEvent::Done(RunSummary {
                text: std::mem::take(&mut self.answer),
                session_id: self.session.clone(),
                ..Default::default()
            }));
        }
        out
    }

    fn finish(&mut self, exit: Option<i32>) -> Vec<AgentEvent> {
        // No done event configured: a clean exit ends the run.
        if !self.done && self.map.done_type.is_empty() && exit == Some(0) {
            self.done = true;
            return vec![AgentEvent::Done(RunSummary {
                text: std::mem::take(&mut self.answer),
                session_id: self.session.clone(),
                ..Default::default()
            })];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpServer;

    fn ctx_parts() -> (tempfile::TempDir, McpServer) {
        (
            tempfile::tempdir().unwrap(),
            McpServer {
                command: "/opt/swy".into(),
                args: vec!["mcp".into()],
                env: vec![("SWITCHYARD_MCP_TOKEN".into(), "tok".into())],
            },
        )
    }

    #[test]
    fn template_and_mcp_config() {
        let (dir, mcp) = ctx_parts();
        let ctx = RunContext {
            workdir: dir.path(),
            prompt: "why \"slow\"?",
            resume: Some("s-9"),
            model: Some("m1"),
            mcp: &mcp,
            system_prompt: "sys",
            extra_args: &["--x".to_owned()],
            extra_env: &[],
        };
        let mut cli = CustomCli::example();
        cli.resume_args = vec!["--resume".into(), "{session}".into()];
        cli.args.push("--model={model}".into());
        let inv = Custom(cli).prepare(&ctx).unwrap();
        let cfg_path = dir.path().join("mcp.json");
        assert_eq!(
            inv.args,
            [
                "--mcp-config",
                &cfg_path.to_string_lossy(),
                "--print",
                "why \"slow\"?",
                "--model=m1",
                "--resume",
                "s-9",
                "--x"
            ]
        );
        let cfg: Value = serde_json::from_slice(&std::fs::read(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            cfg,
            json!({"mcpServers": {"switchyard": {"command": "/opt/swy", "args": ["mcp"],
                   "env": {"SWITCHYARD_MCP_TOKEN": "tok"}}}})
        );
        assert!(inv.stdin.is_none());
        assert!(
            inv.env
                .contains(&("SWITCHYARD_MCP_TOKEN".into(), "tok".into()))
        );

        // Stdin prompt: `{prompt}` stays literal, the prompt goes on stdin.
        let (dir2, _) = ctx_parts();
        let ctx = RunContext {
            workdir: dir2.path(),
            ..ctx
        };
        let mut cli = CustomCli::example();
        cli.prompt_on_stdin = true;
        let inv = Custom(cli).prepare(&ctx).unwrap();
        assert!(inv.args.contains(&"{prompt}".to_owned()));
        assert_eq!(inv.stdin.as_deref(), Some("why \"slow\"?"));
    }

    #[test]
    fn config_file_stays_in_the_run_directory() {
        let (dir, mcp) = ctx_parts();
        let ctx = RunContext {
            workdir: dir.path(),
            prompt: "",
            resume: None,
            model: None,
            mcp: &mcp,
            system_prompt: "",
            extra_args: &[],
            extra_env: &[],
        };
        for bad in ["../evil.json", "/etc/evil.json"] {
            let mut cli = CustomCli::example();
            cli.mcp_config = Some(McpConfigTemplate {
                file: bad.into(),
                template: "{}".into(),
            });
            assert!(Custom(cli).prepare(&ctx).is_err(), "{bad}");
        }
    }

    #[test]
    fn plain_text_output() {
        let mut p = Custom(CustomCli::example()).parser();
        let mut ev = p.feed("line one");
        ev.extend(p.feed("line two"));
        ev.extend(p.finish(Some(0)));
        assert_eq!(
            ev,
            [
                AgentEvent::Text("line one\n".into()),
                AgentEvent::Text("line two\n".into()),
                AgentEvent::Done(RunSummary {
                    text: "line one\nline two".into(),
                    ..Default::default()
                })
            ]
        );
        assert!(
            p.finish(Some(1)).is_empty(),
            "a failure is the runner's to report"
        );
    }

    #[test]
    fn jsonl_mapping() {
        let mut cli = CustomCli::example();
        cli.output = OutputFormat::Jsonl(Box::new(JsonlMapping {
            type_field: "/type".into(),
            text_type: "text".into(),
            text_field: "/text".into(),
            tool_type: "tool".into(),
            tool_name_field: "/name".into(),
            tool_args_field: "/input".into(),
            tool_id_field: "/id".into(),
            result_type: "tool_result".into(),
            result_field: "/output".into(),
            done_type: "end".into(),
            session_field: "/session".into(),
            error_type: "error".into(),
            error_field: "/message".into(),
        }));
        let mut p = Custom(cli).parser();
        let ev: Vec<AgentEvent> = [
            r#"{"type":"start","session":"c-1"}"#,
            r#"{"type":"tool","id":"t1","name":"mcp__switchyard__explain","input":{"sql":"select 1"}}"#,
            r#"{"type":"tool_result","id":"t1","output":"Result"}"#,
            r#"{"type":"text","text":"Done."}"#,
            "garbage",
            r#"{"type":"end"}"#,
        ]
        .iter()
        .flat_map(|l| p.feed(l))
        .collect();
        assert_eq!(
            ev,
            [
                AgentEvent::Started {
                    session_id: Some("c-1".into()),
                    model: None
                },
                AgentEvent::ToolCall {
                    id: "t1".into(),
                    name: "explain".into(),
                    arguments: json!({"sql": "select 1"})
                },
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    text: "Result".into(),
                    is_error: false
                },
                AgentEvent::Text("Done.".into()),
                AgentEvent::Done(RunSummary {
                    text: "Done.".into(),
                    session_id: Some("c-1".into()),
                    ..Default::default()
                }),
            ]
        );
        assert!(p.finish(Some(0)).is_empty(), "already done");
    }
}
