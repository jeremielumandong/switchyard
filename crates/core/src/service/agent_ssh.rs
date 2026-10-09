//! Coding agents' shell commands on Hosts. `swy mcp` hands each one to the app
//! ([`crate::handoff::ask_agent_command`]); the app checks the run's session token and the
//! Host's agent access, shows the command ([`Event::AgentApproval`]) and runs it over the
//! Host's shared SSH session only when the user approves. Every request is recorded in
//! history, declined ones included.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use switchyard_store::{HistoryEntry, HistoryStatus, Host, Profile, now_ms};
use tracing::warn;

use super::{Service, lock};
use crate::agent_run::verify_token;
use crate::bus::{AgentApproval, ApprovalKind, Event};
use crate::handoff::{AgentCommand, AgentCommandOutput, AgentResponder};

/// How long a command or an actual plan waits for the user's answer.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(10 * 60);
/// The longest a command may run.
pub const MAX_COMMAND_TIME: Duration = Duration::from_secs(10 * 60);
/// Bytes kept of each output stream.
pub const OUTPUT_CAP: usize = 64 * 1024;

static NEXT_APPROVAL: AtomicU64 = AtomicU64::new(1);

/// Approvals waiting for the user.
pub(super) type Approvals = Mutex<HashMap<u64, oneshot::Sender<bool>>>;

/// The user's answer to an [`AgentApproval`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Answer {
    /// Run it.
    Approved,
    /// Refused (or the panel went away).
    Declined,
    /// Nobody answered within [`APPROVAL_WAIT`].
    TimedOut,
}

/// Removes a waiting approval and tells the UI it is gone, however the wait ends.
struct Waiting<'a> {
    service: &'a Service,
    id: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        lock(&self.service.agent_approvals).remove(&self.id);
        self.service
            .emit(Event::AgentApprovalClosed { id: self.id });
    }
}

impl Service {
    /// The handoff listener's answer to [`AgentCommand`]s.
    pub(super) fn agent_responder(self: &Arc<Self>) -> AgentResponder {
        let weak = Arc::downgrade(self);
        Arc::new(move |req| {
            let weak = weak.clone();
            Box::pin(async move {
                let Some(this) = weak.upgrade() else {
                    return Err("Switchyard is closing".to_owned());
                };
                this.agent_command(req).await
            })
        })
    }

    /// Show `approval` (its id is assigned here) and wait for the user's answer. The card
    /// closes however the wait ends, including when the caller stops waiting.
    pub(super) async fn ask_user(&self, mut approval: AgentApproval) -> Answer {
        let id = NEXT_APPROVAL.fetch_add(1, Ordering::Relaxed);
        approval.id = id;
        let (tx, rx) = oneshot::channel();
        lock(&self.agent_approvals).insert(id, tx);
        let _waiting = Waiting { service: self, id };
        self.emit(Event::AgentApproval(approval));
        match tokio::time::timeout(APPROVAL_WAIT, rx).await {
            Ok(Ok(true)) => Answer::Approved,
            Ok(_) => Answer::Declined,
            Err(_) => Answer::TimedOut,
        }
    }

    /// [`crate::Command::AnswerAgentApproval`].
    pub(super) fn answer_agent_approval(&self, id: u64, approve: bool) {
        if let Some(tx) = lock(&self.agent_approvals).remove(&id) {
            let _ = tx.send(approve);
        }
    }

    async fn agent_command(&self, req: AgentCommand) -> Result<AgentCommandOutput, String> {
        let scope = verify_token(&self.data_dir, &req.session_token).map_err(|e| e.to_string())?;
        let name = req.host.trim().to_owned();
        let profiles = self
            .with_store(|s| s.profiles())
            .await
            .map_err(|e| e.to_string())?;
        let host = agent_host(&profiles, &name, |h| scope.allows(&h.id))
            // Same answer for "missing" and "not enabled".
            .ok_or_else(|| format!("no Host named {name:?} is available to agents"))?;
        let command = req.command.trim().to_owned();
        if command.is_empty() {
            return Err("the command is empty".to_owned());
        }
        let tags = vec![
            "agent".to_owned(),
            scope.agent.history_tag().to_owned(),
            "ssh".to_owned(),
        ];

        let answer = self
            .ask_user(AgentApproval {
                id: 0,
                agent: scope.agent,
                kind: ApprovalKind::SshCommand,
                target: host.id.clone(),
                target_name: host.name.clone(),
                environment: host.environment,
                text: command.clone(),
            })
            .await;
        let refused = match answer {
            Answer::Approved => None,
            Answer::Declined => Some("the user declined to run this command"),
            Answer::TimedOut => Some("nobody approved the command in Switchyard in time"),
        };
        if let Some(e) = refused {
            let e = e.to_owned();
            self.ssh_history(&host, command, Duration::ZERO, Some(e.clone()), tags)
                .await;
            return Err(e);
        }

        let timeout = Duration::from_secs(req.timeout_secs.max(1)).min(MAX_COMMAND_TIME);
        let started = Instant::now();
        let result = async {
            let target = self.ssh_target(&host.id).await.map_err(|e| e.to_string())?;
            let conn = self.ssh.session(&target).await.map_err(|e| e.to_string())?;
            conn.run_command(&command, OUTPUT_CAP, timeout)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        let elapsed = started.elapsed();
        let result = result.map(|o| AgentCommandOutput {
            exit_status: o.exit_status,
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            truncated: o.truncated,
            timed_out: o.timed_out,
        });
        let error = match &result {
            Ok(o) if o.timed_out => Some(format!("stopped after {} s", timeout.as_secs())),
            Ok(o) => o
                .exit_status
                .filter(|c| *c != 0)
                .map(|c| format!("exit status {c}")),
            Err(e) => Some(e.clone()),
        };
        self.ssh_history(&host, command, elapsed, error, tags).await;
        result
    }

    async fn ssh_history(
        &self,
        host: &Host,
        command: String,
        elapsed: Duration,
        error: Option<String>,
        tags: Vec<String>,
    ) {
        let entry = HistoryEntry {
            id: 0,
            connection_id: Some(host.id.clone()),
            connection_name: host.name.clone(),
            sql: command,
            started_at: now_ms(),
            duration_ms: elapsed.as_millis() as i64,
            rows: None,
            affected: None,
            status: if error.is_some() {
                HistoryStatus::Error
            } else {
                HistoryStatus::Ok
            },
            error,
            tags,
            has_plan: false,
        };
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "agent command history write failed");
        }
    }
}

/// The agent-enabled Host called `name` (case-insensitive) that `in_scope` allows.
fn agent_host(profiles: &[Profile], name: &str, in_scope: impl Fn(&Host) -> bool) -> Option<Host> {
    profiles.iter().find_map(|p| match p {
        Profile::Host(h) if h.agent_access && in_scope(h) && h.name.eq_ignore_ascii_case(name) => {
            Some(h.clone())
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_agent_enabled_hosts_in_scope() {
        let mut web = Host::new("web", "web.example", "deploy");
        web.agent_access = true;
        let db = Host::new("db", "db.example", "deploy");
        let profiles = vec![Profile::Host(web.clone()), Profile::Host(db)];
        assert_eq!(
            agent_host(&profiles, "WEB", |_| true).map(|h| h.id),
            Some(web.id.clone())
        );
        assert!(agent_host(&profiles, "web", |_| false).is_none());
        assert!(agent_host(&profiles, "db", |_| true).is_none());
        assert!(agent_host(&profiles, "nope", |_| true).is_none());
    }
}
