//! The assistant: coding-CLI runs and "Open in terminal" (M5-14, M5-15).

use switchyard_agents::{AgentEvent, AgentKind};
use switchyard_store::{DbConnection, Profile, ProfileId};
use switchyard_term::{LocalShell, TermSize};

use super::{Service, lock};
use crate::agent_run::{
    ASSISTANT_SETTINGS_KEY, AgentRunRequest, AssistantSettings, prepare_agent_terminal,
    start_agent_run,
};
use crate::bus::{AgentRunId, Event, TermId};
use crate::error::{CoreError, Result};

impl Service {
    /// Settings → Assistant (defaults when never saved).
    pub(crate) async fn assistant_settings(&self) -> AssistantSettings {
        self.with_store(|s| s.setting::<AssistantSettings>(ASSISTANT_SETTINGS_KEY))
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// The CLI and its request for a question about `connection`; see [`agent_scope`].
    async fn agent_request(
        &self,
        agent: Option<AgentKind>,
        connection: Option<ProfileId>,
        databases: bool,
        prompt: String,
        resume: Option<String>,
    ) -> Result<(AgentKind, AgentRunRequest)> {
        let settings = self.assistant_settings().await;
        let profiles = self.with_store(|s| s.profiles()).await?;
        let dbs: Vec<&DbConnection> = profiles
            .iter()
            .filter_map(|p| match p {
                Profile::Db(d) => Some(d),
                _ => None,
            })
            .collect();
        let (conn, scope) = agent_scope(&dbs, connection.as_ref(), databases)?;
        let kind = agent.unwrap_or_else(|| settings.agent_for(conn));
        let mut req = settings.request(kind, prompt, resume, scope);
        req.swy.clone_from(&self.swy);
        Ok((kind, req))
    }

    fn agent_event(&self, run: AgentRunId, agent: AgentKind, event: AgentEvent) {
        self.emit(Event::Agent { run, agent, event });
    }

    /// Run the assistant; every event goes out as [`Event::Agent`].
    pub(crate) async fn run_agent(
        &self,
        run: AgentRunId,
        agent: Option<AgentKind>,
        connection: Option<ProfileId>,
        databases: bool,
        prompt: String,
        resume: Option<String>,
    ) {
        let (kind, req) = match self
            .agent_request(agent, connection, databases, prompt, resume)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let kind = agent.unwrap_or(AgentKind::ClaudeCode);
                self.agent_event(run, kind, AgentEvent::Error(e.to_string()));
                self.agent_event(run, kind, AgentEvent::Exited(None));
                return;
            }
        };
        let data = self.data_dir.clone();
        let started = tokio::task::spawn_blocking(move || start_agent_run(&data, req))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))
            .and_then(|r| r);
        let mut handle = match started {
            Ok(h) => h,
            Err(e) => {
                let message = match e {
                    CoreError::NotFound(what) => format!(
                        "{what} was not found. Install it (see Drivers) or set its path in \
                         Settings → Assistant."
                    ),
                    other => other.to_string(),
                };
                self.agent_event(run, kind, AgentEvent::Error(message));
                self.agent_event(run, kind, AgentEvent::Exited(None));
                return;
            }
        };
        lock(&self.agent_runs).insert(run, handle.cancel_handle());
        while let Some(e) = handle.next().await {
            self.agent_event(run, kind, e);
        }
        lock(&self.agent_runs).remove(&run);
    }

    /// Stop a run.
    pub(crate) fn cancel_agent(&self, run: AgentRunId) {
        if let Some(h) = lock(&self.agent_runs).get(&run) {
            h.cancel();
        }
    }

    /// "Open in terminal": the CLI interactively in a local terminal, tools attached.
    pub(crate) async fn open_agent_terminal(
        &self,
        term: TermId,
        agent: Option<AgentKind>,
        connection: Option<ProfileId>,
        size: TermSize,
    ) {
        let result = async {
            let (kind, req) = self
                .agent_request(agent, connection, true, String::new(), None)
                .await?;
            let data = self.data_dir.clone();
            let session = tokio::task::spawn_blocking(move || prepare_agent_terminal(&data, req))
                .await
                .map_err(|e| CoreError::Internal(e.to_string()))??;
            let shell = LocalShell {
                program: Some(session.program.to_string_lossy().into_owned()),
                args: session.args,
                cwd: Some(session.cwd),
                env: session.env,
            };
            let terminal = self
                .terminals
                .open_local(
                    term,
                    shell,
                    size,
                    switchyard_term::DEFAULT_SCROLLBACK,
                    self.events.clone(),
                    session.guards,
                )
                .map_err(|e| CoreError::Internal(e.to_string()))?;
            Ok::<_, CoreError>((
                terminal,
                format!("{} · Switchyard tools", kind.display_name()),
            ))
        }
        .await;
        match result {
            Ok((terminal, description)) => self.emit(Event::TerminalOpened {
                term,
                terminal,
                description,
            }),
            Err(e) => self.emit(Event::TerminalFailed {
                term,
                message: e.to_string(),
            }),
        }
    }
}

/// The connection a run is about and the connections its token may reach. With
/// `databases` off (an API Workbench question) the run reaches none. Otherwise `connection`
/// must allow agents and is the only one in scope; without one, every agent-enabled
/// connection is.
fn agent_scope<'a>(
    dbs: &[&'a DbConnection],
    connection: Option<&ProfileId>,
    databases: bool,
) -> Result<(Option<&'a DbConnection>, Vec<ProfileId>)> {
    if !databases {
        return Ok((None, Vec::new()));
    }
    match connection {
        Some(id) => {
            let c = *dbs
                .iter()
                .find(|c| &c.id == id)
                .ok_or_else(|| CoreError::NotFound("the connection".into()))?;
            if !c.agent_access {
                return Err(CoreError::Unsupported(format!(
                    "Coding agents are off for {}. Turn on \u{201c}Allow coding agents\u{201d} in its \
                     settings to ask the assistant about it.",
                    c.name
                )));
            }
            Ok((Some(c), vec![c.id.clone()]))
        }
        None => Ok((
            None,
            dbs.iter()
                .filter(|c| c.agent_access)
                .map(|c| c.id.clone())
                .collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use switchyard_db::Engine;

    fn conn(name: &str, agent_access: bool) -> DbConnection {
        let mut c = DbConnection::new(name, Engine::Postgres);
        c.agent_access = agent_access;
        c
    }

    #[test]
    fn api_runs_reach_no_connection() {
        let (open, closed) = (conn("app", true), conn("billing", false));
        let dbs = [&open, &closed];
        let (c, scope) = agent_scope(&dbs, Some(&open.id), false).unwrap();
        assert!(c.is_none());
        assert!(scope.is_empty());
        // Not even a connection that refuses agents is looked at.
        assert!(agent_scope(&dbs, Some(&closed.id), false).is_ok());
    }

    #[test]
    fn database_runs_keep_their_scope() {
        let (open, other, closed) = (conn("app", true), conn("ops", true), conn("billing", false));
        let dbs = [&open, &other, &closed];
        let (c, scope) = agent_scope(&dbs, Some(&open.id), true).unwrap();
        assert_eq!(c.map(|c| c.name.as_str()), Some("app"));
        assert_eq!(scope, vec![open.id.clone()]);
        let (_, scope) = agent_scope(&dbs, None, true).unwrap();
        assert_eq!(scope, vec![open.id.clone(), other.id.clone()]);
        assert!(matches!(
            agent_scope(&dbs, Some(&closed.id), true),
            Err(CoreError::Unsupported(m)) if m.contains("Allow coding agents")
        ));
    }
}
