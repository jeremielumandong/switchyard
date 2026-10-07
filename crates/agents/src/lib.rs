//! Coding-CLI adapters (milestone M5).
//!
//! Each supported CLI (Claude Code, Codex CLI, Gemini CLI, custom) implements
//! [`AgentAdapter`]: how to invoke it and how to read its output. The shared [`runner`]
//! owns everything else (a private temp working directory, the child process, cancel,
//! cleanup), and the UI only consumes normalized [`AgentEvent`]s.

mod adapter;
pub mod claude;
pub mod codex;
mod process;
pub mod runner;
mod workdir;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use adapter::{AgentAdapter, Invocation, McpServer, RunContext, StreamParser};
pub use claude::ClaudeCode;
pub use codex::Codex;
pub use process::find_program;
pub use runner::{AgentRun, CancelHandle, RunRequest};

/// Name of Switchyard's MCP server in every generated CLI config.
pub const MCP_SERVER_NAME: &str = "switchyard";

/// Instructions every adapter appends to the CLI's system prompt.
pub const SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

/// Supported coding CLIs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    /// Anthropic Claude Code (`claude`).
    ClaudeCode,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// Google Gemini CLI (`gemini`).
    Gemini,
    /// A user-defined command.
    Custom,
}

impl AgentKind {
    /// Stable id (`claude-code`, `codex`, `gemini`, `custom`).
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
            AgentKind::Gemini => "gemini",
            AgentKind::Custom => "custom",
        }
    }

    /// The kind with this [`id`](Self::id).
    pub fn from_id(id: &str) -> Option<Self> {
        [Self::ClaudeCode, Self::Codex, Self::Gemini, Self::Custom]
            .into_iter()
            .find(|k| k.id() == id)
    }

    /// Display name.
    pub fn display_name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Codex => "Codex CLI",
            AgentKind::Gemini => "Gemini CLI",
            AgentKind::Custom => "Custom CLI",
        }
    }

    /// History tag for agent calls (`agent:claude-code`, ...).
    pub fn history_tag(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "agent:claude-code",
            AgentKind::Codex => "agent:codex",
            AgentKind::Gemini => "agent:gemini",
            AgentKind::Custom => "agent:custom",
        }
    }
}

/// How a run ended successfully.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The final answer, when the CLI reports one separately from the streamed text.
    pub text: String,
    /// Conversation id to resume with, when the CLI has one.
    pub session_id: Option<String>,
    /// Cost the CLI reported, in US dollars.
    pub cost_usd: Option<f64>,
    /// Wall time the CLI reported.
    pub duration_ms: Option<u64>,
    /// Model turns.
    pub turns: Option<u64>,
    /// Input tokens.
    pub input_tokens: Option<u64>,
    /// Output tokens.
    pub output_tokens: Option<u64>,
}

/// A normalized event from an agent run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    /// The CLI started a conversation.
    Started {
        /// Conversation id to resume with.
        session_id: Option<String>,
        /// Model the CLI chose.
        model: Option<String>,
    },
    /// New assistant text.
    Text(String),
    /// New reasoning text, where the CLI shows it.
    Thinking(String),
    /// A tool call. Switchyard's tools appear under their MCP name (`explain`); other
    /// tools keep the CLI's name.
    ToolCall {
        /// Call id, to match the result.
        id: String,
        /// Tool name.
        name: String,
        /// JSON arguments.
        arguments: Value,
    },
    /// A tool call's result.
    ToolResult {
        /// Call id.
        id: String,
        /// Result text.
        text: String,
        /// The tool failed or was refused.
        is_error: bool,
    },
    /// The run finished with an answer.
    Done(RunSummary),
    /// The run failed (or was cancelled).
    Error(String),
    /// A diagnostic line from the CLI (stderr).
    Log(String),
    /// The CLI process ended: the last event of every run. Its temp directory and session
    /// token are gone by now.
    Exited(Option<i32>),
}

impl AgentEvent {
    /// `Done` or `Error`: the run has its outcome.
    pub fn is_outcome(&self) -> bool {
        matches!(self, AgentEvent::Done(_) | AgentEvent::Error(_))
    }
}

/// Errors starting a run.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The CLI is not installed (or not where the settings say).
    #[error("{0} is not installed or not on PATH")]
    NotFound(String),
    /// Files or the process could not be set up.
    #[error("could not start the agent: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_ids() {
        assert_eq!(AgentKind::Gemini.history_tag(), "agent:gemini");
        for k in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Gemini,
            AgentKind::Custom,
        ] {
            assert_eq!(AgentKind::from_id(k.id()), Some(k));
            assert_eq!(k.history_tag(), format!("agent:{}", k.id()));
        }
        assert_eq!(AgentKind::from_id("other"), None);
    }
}
