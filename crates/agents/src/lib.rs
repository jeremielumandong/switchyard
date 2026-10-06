//! Coding-CLI adapters (milestone M5).
//!
//! Each supported CLI (Claude Code, Codex CLI, Gemini CLI, custom) implements
//! [`AgentKind`]-specific invocation in its own adapter; the UI only consumes normalized
//! [`AgentEvent`]s.

use serde::{Deserialize, Serialize};

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

/// A normalized event from an agent run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    /// Assistant text.
    Text(String),
    /// A tool call to Switchyard's MCP server.
    ToolCall {
        /// Tool name.
        name: String,
        /// JSON arguments.
        arguments: String,
    },
    /// The run finished.
    Done,
    /// The run failed.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(AgentKind::Gemini.history_tag(), "agent:gemini");
    }
}
