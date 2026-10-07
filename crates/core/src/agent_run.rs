//! Coding-agent runs: the `swy mcp` session token, and starting a CLI with it.
//!
//! A run's token names the connections the run may use. It lives in
//! `<data>/agent-tokens/<sha256 of the token>.json` (the token itself is never stored),
//! expires on its own, and is revoked (the file removed) when the run ends. `swy mcp`
//! started with [`TOKEN_ENV`] serves only the token's connections that also have agent
//! access, and checks the token again on every tool call, so a revoked or expired token
//! stops an MCP server the CLI left running.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use switchyard_agents::{
    AgentAdapter, AgentError, AgentKind, AgentRun, ClaudeCode, CustomCli, McpServer, RunRequest,
};
use switchyard_store::random::{hex, random_hex};
use switchyard_store::{DbConnection, ProfileId, now_ms};

use crate::error::{CoreError, Result};

/// Environment variable carrying the session token to `swy mcp`.
pub const TOKEN_ENV: &str = "SWITCHYARD_MCP_TOKEN";
/// Environment variable naming the CLI to `swy mcp` (its history tag).
pub const AGENT_ENV: &str = "SWITCHYARD_AGENT";
/// How long a token lives if its run never ends cleanly.
pub const TOKEN_TTL: Duration = Duration::from_secs(2 * 60 * 60);

const TOKEN_DIR: &str = "agent-tokens";

/// What a session token allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenScope {
    /// Connection ids the run may use (agent access is still required on each).
    pub connections: Vec<String>,
    /// The CLI the token was issued to.
    pub agent: AgentKind,
    /// Expiry, ms since the epoch.
    pub expires_ms: i64,
}

impl TokenScope {
    /// Whether the connection with this id is in scope.
    pub fn allows(&self, id: &ProfileId) -> bool {
        self.connections.contains(&id.0)
    }
}

/// Why a token was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Unknown or revoked: the run that owned it has ended.
    #[error("this agent session has ended (its token was revoked)")]
    Revoked,
    /// Past its expiry.
    #[error("this agent session has expired")]
    Expired,
}

fn token_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_DIR)
}

fn token_file(data_dir: &Path, token: &str) -> PathBuf {
    token_dir(data_dir).join(format!(
        "{}.json",
        hex(&Sha256::digest(token.trim().as_bytes()))
    ))
}

/// A live session token. Dropping it revokes it.
pub struct SessionToken {
    value: SecretString,
    file: PathBuf,
}

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionToken").finish_non_exhaustive()
    }
}

impl SessionToken {
    /// Issue a token for `connections`, valid for `ttl` unless revoked sooner.
    pub fn issue(
        data_dir: &Path,
        connections: &[ProfileId],
        agent: AgentKind,
        ttl: Duration,
    ) -> std::io::Result<Self> {
        let dir = token_dir(data_dir);
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            b.mode(0o700);
        }
        b.create(&dir)?;
        remove_expired(&dir);
        let value = random_hex(32);
        let scope = TokenScope {
            connections: connections.iter().map(|c| c.0.clone()).collect(),
            agent,
            expires_ms: now_ms().saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)),
        };
        let file = token_file(data_dir, &value);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut f = options.open(&file)?;
        f.write_all(&serde_json::to_vec(&scope).map_err(std::io::Error::other)?)?;
        Ok(Self {
            value: SecretString::from(value),
            file,
        })
    }

    /// The token, for the MCP server's environment.
    pub fn expose(&self) -> &str {
        self.value.expose_secret()
    }

    /// Revoke now (same as dropping).
    pub fn revoke(self) {}
}

impl Drop for SessionToken {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.file)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "could not revoke an agent session token");
        }
    }
}

/// The scope of a live token.
pub fn verify_token(data_dir: &Path, token: &str) -> Result<TokenScope, TokenError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(TokenError::Revoked);
    }
    let scope: TokenScope = std::fs::read(token_file(data_dir, token))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(TokenError::Revoked)?;
    if now_ms() >= scope.expires_ms {
        return Err(TokenError::Expired);
    }
    Ok(scope)
}

/// Remove token files past their expiry (left by a crash).
fn remove_expired(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = now_ms();
    for e in entries.flatten() {
        let expired = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<TokenScope>(&b).ok())
            .is_none_or(|s| now >= s.expires_ms);
        if expired && e.path().extension().is_some_and(|x| x == "json") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The adapter for a CLI; the custom one needs its settings.
pub fn adapter_for(kind: AgentKind, custom: Option<&CustomCli>) -> Option<Arc<dyn AgentAdapter>> {
    match kind {
        AgentKind::ClaudeCode => Some(Arc::new(ClaudeCode)),
        AgentKind::Codex => Some(Arc::new(switchyard_agents::Codex)),
        AgentKind::Gemini => Some(Arc::new(switchyard_agents::Gemini)),
        AgentKind::Custom => custom
            .filter(|c| !c.program.trim().is_empty())
            .map(|c| Arc::new(switchyard_agents::Custom(c.clone())) as Arc<dyn AgentAdapter>),
    }
}

/// The `swy` binary: `SWITCHYARD_SWY`, else next to this executable, else on PATH.
pub fn swy_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SWITCHYARD_SWY") {
        return Some(PathBuf::from(p));
    }
    let name = format!("swy{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(&name)))
        .filter(|p| p.is_file())
        .or_else(|| switchyard_agents::find_program("swy", None))
}

/// Settings key of [`AssistantSettings`] (Settings → Assistant).
pub const ASSISTANT_SETTINGS_KEY: &str = "assistant";

/// One CLI's settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CliSettings {
    /// Executable; empty looks it up on PATH.
    pub path: String,
    /// Model; empty keeps the CLI's default.
    pub model: String,
    /// Extra arguments, added to every run.
    pub extra_args: Vec<String>,
}

/// Settings → Assistant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistantSettings {
    /// The CLI used unless a connection names another.
    pub default_agent: AgentKind,
    /// Claude Code.
    pub claude_code: CliSettings,
    /// Codex CLI.
    pub codex: CliSettings,
    /// Gemini CLI.
    pub gemini: CliSettings,
    /// The custom CLI's model and extra arguments (its program is in `custom`).
    pub custom_settings: CliSettings,
    /// The custom CLI.
    pub custom: CustomCli,
}

impl Default for AssistantSettings {
    fn default() -> Self {
        Self {
            default_agent: AgentKind::ClaudeCode,
            claude_code: CliSettings::default(),
            codex: CliSettings::default(),
            gemini: CliSettings::default(),
            custom_settings: CliSettings::default(),
            custom: CustomCli::example(),
        }
    }
}

impl AssistantSettings {
    /// One CLI's settings.
    pub fn cli(&self, kind: AgentKind) -> &CliSettings {
        match kind {
            AgentKind::ClaudeCode => &self.claude_code,
            AgentKind::Codex => &self.codex,
            AgentKind::Gemini => &self.gemini,
            AgentKind::Custom => &self.custom_settings,
        }
    }

    /// One CLI's settings, to change.
    pub fn cli_mut(&mut self, kind: AgentKind) -> &mut CliSettings {
        match kind {
            AgentKind::ClaudeCode => &mut self.claude_code,
            AgentKind::Codex => &mut self.codex,
            AgentKind::Gemini => &mut self.gemini,
            AgentKind::Custom => &mut self.custom_settings,
        }
    }

    /// The CLI for a connection: its override, else the default.
    pub fn agent_for(&self, connection: Option<&DbConnection>) -> AgentKind {
        connection
            .and_then(|c| c.assistant_agent.as_deref())
            .and_then(AgentKind::from_id)
            .unwrap_or(self.default_agent)
    }

    /// A run request for `kind` from these settings.
    pub fn request(
        &self,
        kind: AgentKind,
        prompt: String,
        resume: Option<String>,
        connections: Vec<ProfileId>,
    ) -> AgentRunRequest {
        let cli = self.cli(kind);
        let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_owned());
        AgentRunRequest {
            agent: kind,
            program: some(&cli.path).map(PathBuf::from),
            prompt,
            resume,
            model: some(&cli.model),
            connections,
            swy: None,
            mcp_env: Vec::new(),
            temp_root: None,
            extra_args: cli.extra_args.clone(),
            extra_env: Vec::new(),
            custom: (kind == AgentKind::Custom).then(|| self.custom.clone()),
        }
    }
}

/// A request to run a coding agent.
#[derive(Clone, Debug)]
pub struct AgentRunRequest {
    /// Which CLI.
    pub agent: AgentKind,
    /// The CLI's executable from settings; `None` looks it up.
    pub program: Option<PathBuf>,
    /// The user's request.
    pub prompt: String,
    /// Conversation to continue.
    pub resume: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Connections the run may use (each still needs agent access).
    pub connections: Vec<ProfileId>,
    /// `swy`; `None` uses [`swy_path`].
    pub swy: Option<PathBuf>,
    /// Extra environment for `swy mcp` (tests point `SWITCHYARD_HOME` at their home).
    pub mcp_env: Vec<(String, String)>,
    /// Parent of the run's temp directory; `None` is the system temp directory.
    pub temp_root: Option<PathBuf>,
    /// Extra CLI arguments (Settings → Assistant).
    pub extra_args: Vec<String>,
    /// Extra environment for the CLI (Settings → Assistant).
    pub extra_env: Vec<(String, String)>,
    /// The custom CLI, for [`AgentKind::Custom`].
    pub custom: Option<CustomCli>,
}

fn agent_error(e: AgentError) -> CoreError {
    match e {
        AgentError::NotFound(name) => CoreError::NotFound(name),
        AgentError::Io(e) => CoreError::Internal(e.to_string()),
    }
}

/// Issue the token and describe the run for the agents runner.
fn build(data_dir: &Path, req: AgentRunRequest) -> Result<(Arc<dyn AgentAdapter>, RunRequest)> {
    let adapter = adapter_for(req.agent, req.custom.as_ref()).ok_or_else(|| {
        CoreError::Unsupported(format!(
            "{} is not set up: give it a program in Settings → Assistant",
            req.agent.display_name()
        ))
    })?;
    let swy = req
        .swy
        .or_else(swy_path)
        .ok_or_else(|| CoreError::NotFound("the swy executable".into()))?;
    let token = SessionToken::issue(data_dir, &req.connections, req.agent, TOKEN_TTL)
        .map_err(|e| CoreError::Internal(format!("agent session token: {e}")))?;
    let mut env = vec![
        (TOKEN_ENV.to_owned(), token.expose().to_owned()),
        (AGENT_ENV.to_owned(), req.agent.id().to_owned()),
    ];
    // Where swy finds the profiles and secrets, when not the platform default. Not the
    // vault password: that stays in the environment it came from.
    for key in ["SWITCHYARD_HOME", "SWITCHYARD_SECRETS"] {
        if let Ok(v) = std::env::var(key) {
            env.push((key.to_owned(), v));
        }
    }
    for (k, v) in req.mcp_env {
        env.retain(|(e, _)| *e != k);
        env.push((k, v));
    }
    Ok((
        adapter,
        RunRequest {
            program: req.program,
            prompt: req.prompt,
            resume: req.resume,
            model: req.model,
            mcp: McpServer {
                command: swy,
                args: vec!["mcp".into()],
                env,
            },
            temp_root: req.temp_root,
            extra_args: req.extra_args,
            extra_env: req.extra_env,
            guards: vec![Box::new(token)],
        },
    ))
}

/// Start a run: issue its token, then start the CLI with `swy mcp` attached. The token is
/// revoked when the CLI exits (or the run is cancelled or dropped).
pub fn start_agent_run(data_dir: &Path, req: AgentRunRequest) -> Result<AgentRun> {
    let (adapter, run) = build(data_dir, req)?;
    switchyard_agents::runner::start(adapter, run).map_err(agent_error)
}

/// Prepare the CLI to run interactively in a terminal with `swy mcp` attached. The
/// session's guards revoke the token and remove the run directory when dropped.
pub fn prepare_agent_terminal(
    data_dir: &Path,
    req: AgentRunRequest,
) -> Result<switchyard_agents::InteractiveSession> {
    let (adapter, run) = build(data_dir, req)?;
    switchyard_agents::runner::prepare_interactive(adapter, run).map_err(agent_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_verify_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let ids = [ProfileId("a".into()), ProfileId("b".into())];
        let t = SessionToken::issue(dir.path(), &ids, AgentKind::ClaudeCode, TOKEN_TTL).unwrap();
        let scope = verify_token(dir.path(), t.expose()).unwrap();
        assert!(scope.allows(&ProfileId("b".into())));
        assert!(!scope.allows(&ProfileId("c".into())));
        assert_eq!(scope.agent, AgentKind::ClaudeCode);
        // The token is not stored anywhere in clear.
        for f in std::fs::read_dir(token_dir(dir.path())).unwrap() {
            let body = std::fs::read_to_string(f.unwrap().path()).unwrap();
            assert!(!body.contains(t.expose()));
        }
        assert_eq!(verify_token(dir.path(), "nope"), Err(TokenError::Revoked));
        assert_eq!(verify_token(dir.path(), ""), Err(TokenError::Revoked));
        let value = t.expose().to_owned();
        t.revoke();
        assert_eq!(verify_token(dir.path(), &value), Err(TokenError::Revoked));
    }

    #[test]
    fn expiry_and_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let old = SessionToken::issue(dir.path(), &[], AgentKind::Codex, Duration::ZERO).unwrap();
        assert_eq!(
            verify_token(dir.path(), old.expose()),
            Err(TokenError::Expired)
        );
        let path = old.file.clone();
        std::mem::forget(old); // as if the app crashed
        assert!(path.exists());
        let _new = SessionToken::issue(dir.path(), &[], AgentKind::Codex, TOKEN_TTL).unwrap();
        assert!(!path.exists(), "expired tokens are swept on issue");
    }

    #[test]
    fn unsupported_agents_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let r = start_agent_run(
            dir.path(),
            AgentRunRequest {
                agent: AgentKind::Custom,
                program: None,
                prompt: String::new(),
                resume: None,
                model: None,
                connections: vec![],
                swy: Some("swy".into()),
                mcp_env: vec![],
                temp_root: None,
                extra_args: vec![],
                extra_env: vec![],
                custom: None,
            },
        );
        assert!(matches!(r, Err(CoreError::Unsupported(_))));
    }
}
