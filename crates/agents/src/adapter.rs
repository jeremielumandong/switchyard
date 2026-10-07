//! The adapter contract: what differs between coding CLIs.

use std::path::{Path, PathBuf};

use crate::{AgentError, AgentEvent, AgentKind};

/// How to start Switchyard's MCP server (`swy mcp`) from a CLI's config.
#[derive(Clone, Debug)]
pub struct McpServer {
    /// The `swy` executable.
    pub command: PathBuf,
    /// Arguments (`["mcp"]`).
    pub args: Vec<String>,
    /// Environment for the server: the session token and Switchyard's home. Written only
    /// into files inside the run's private directory, never onto a command line.
    pub env: Vec<(String, String)>,
}

/// What an adapter gets to prepare one run.
#[derive(Debug)]
pub struct RunContext<'a> {
    /// The run's private, empty working directory. Config files go here.
    pub workdir: &'a Path,
    /// The user's request.
    pub prompt: &'a str,
    /// Conversation to continue.
    pub resume: Option<&'a str>,
    /// Model override; `None` keeps the CLI's default.
    pub model: Option<&'a str>,
    /// Switchyard's MCP server.
    pub mcp: &'a McpServer,
    /// Instructions to append to the CLI's system prompt.
    pub system_prompt: &'a str,
    /// Extra arguments from the user's settings, placed before any positional arguments.
    pub extra_args: &'a [String],
    /// Extra environment the CLI will get (an adapter may read its own settings from it).
    pub extra_env: &'a [(String, String)],
}

/// The process to start, as an adapter describes it.
#[derive(Clone, Debug, Default)]
pub struct Invocation {
    /// Arguments after the program.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Written to the CLI's standard input, which is then closed.
    pub stdin: Option<String>,
}

/// Turns a CLI's standard output into [`AgentEvent`]s.
pub trait StreamParser: Send {
    /// Parse one line. Lines that mean nothing yield nothing.
    fn feed(&mut self, line: &str) -> Vec<AgentEvent>;

    /// The process exited with `exit` (`None`: by a signal) after its last line. CLIs that
    /// end a run only by exiting report their outcome here.
    fn finish(&mut self, exit: Option<i32>) -> Vec<AgentEvent> {
        let _ = exit;
        Vec::new()
    }
}

/// One coding CLI.
pub trait AgentAdapter: Send + Sync {
    /// Which CLI this is.
    fn kind(&self) -> AgentKind;
    /// Executable name looked up when the user did not set a path.
    fn program(&self) -> &str;
    /// Write the CLI's config (MCP server, permissions) into `ctx.workdir` and describe the
    /// invocation. The config must allow only Switchyard's MCP tools.
    fn prepare(&self, ctx: &RunContext<'_>) -> Result<Invocation, AgentError>;
    /// A fresh parser for one run's output.
    fn parser(&self) -> Box<dyn StreamParser>;

    /// The invocation for an interactive session in a terminal ("Open in terminal"), with
    /// the same MCP server and restrictions; `None` when the CLI has none.
    fn interactive(&self, ctx: &RunContext<'_>) -> Option<Result<Invocation, AgentError>> {
        let _ = ctx;
        None
    }
}
