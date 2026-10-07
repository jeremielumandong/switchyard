//! A small client over `switchyard-core`'s command/event bus: the CLI and the MCP server
//! drive the same core as the app, so connections, sessions, guards and history behave the
//! same everywhere.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use futures::StreamExt as _;
use secrecy::SecretString;
use switchyard_core::db::{CatalogChunk, Engine, IntrospectScope, dialect_for};
use switchyard_core::plan::access::Workload;
use switchyard_core::plan::whatif::WhatIf;
use switchyard_core::plan::{Finding, Plan};
use switchyard_core::store::{AppPaths, DbConnection, Profile};
use switchyard_core::{
    AgentRows, Command, Core, Event, EventReceiver, FetchLimit, QueryEvent, ServiceConfig,
    SessionId, StatementRequest,
};

static NEXT: AtomicU64 = AtomicU64::new(1 << 40);

/// A fresh request / session / query id (kept clear of the app's range).
pub fn next_id() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// How long a single core operation may take before the CLI gives up waiting.
const WAIT: Duration = Duration::from_secs(600);

/// A captured plan with its findings and history entry.
pub struct PlanResult {
    pub plan: Arc<Plan>,
    pub findings: Vec<Finding>,
    pub history_id: Option<i64>,
}

/// The core plus its event stream.
pub struct Client {
    core: Core,
    events: EventReceiver,
    profiles: Vec<Profile>,
}

impl Client {
    /// Start a core on the user's profile store. With `SWITCHYARD_VAULT_PASSWORD` set, the
    /// fallback vault (no OS keychain) is unlocked first.
    pub async fn start() -> Result<Self> {
        let paths = AppPaths::resolve().context("could not determine a home directory")?;
        let (core, events) = Core::start(ServiceConfig::from_paths(&paths))?;
        let mut c = Self {
            core,
            events,
            profiles: Vec::new(),
        };
        if let Ok(pw) = std::env::var("SWITCHYARD_VAULT_PASSWORD") {
            c.send(Command::UnlockVault {
                password: SecretString::from(pw),
            });
            c.wait(|e| match e {
                Event::SecretBackend { locked: false, .. } => Some(Ok(())),
                Event::Error { context, message }
                    if context.contains("vault") || context.contains("Vault") =>
                {
                    Some(Err(anyhow!("{context}: {message}")))
                }
                _ => None,
            })
            .await??;
        }
        c.send(Command::LoadProfiles);
        c.profiles = c
            .wait(|e| match e {
                Event::Profiles(p) => Some(p),
                _ => None,
            })
            .await?;
        Ok(c)
    }

    /// Send a command.
    pub fn send(&self, c: Command) {
        self.core.handle().send(c);
    }

    /// Wait for the first event `f` accepts.
    pub async fn wait<T>(&mut self, mut f: impl FnMut(Event) -> Option<T>) -> Result<T> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let ev = tokio::time::timeout_at(deadline, self.events.next())
                .await
                .map_err(|_| anyhow!("timed out waiting for Switchyard's core"))?
                .ok_or_else(|| anyhow!("Switchyard's core stopped"))?;
            if let Some(t) = f(ev) {
                return Ok(t);
            }
        }
    }

    /// Every saved profile.
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    /// Saved database connections.
    pub fn connections(&self) -> Vec<&DbConnection> {
        self.profiles
            .iter()
            .filter_map(|p| match p {
                Profile::Db(d) => Some(d),
                _ => None,
            })
            .collect()
    }

    /// The connection called `name` (case-insensitive).
    pub fn connection(&self, name: &str) -> Result<DbConnection> {
        self.connections()
            .into_iter()
            .find(|c| c.name.eq_ignore_ascii_case(name.trim()))
            .cloned()
            .ok_or_else(|| anyhow!("no connection named {name:?} (see `swy connections`)"))
    }

    /// Open a session.
    pub async fn open(&mut self, conn: &DbConnection) -> Result<SessionId> {
        let session = next_id();
        self.send(Command::OpenSession {
            session,
            connection: conn.id.clone(),
        });
        self.wait(|e| match e {
            Event::SessionOpened { session: s, .. } if s == session => Some(Ok(session)),
            Event::SessionFailed {
                session: s,
                message,
            } if s == session => Some(Err(anyhow!("{message}"))),
            _ => None,
        })
        .await?
    }

    /// Close a session.
    pub fn close(&self, session: SessionId) {
        self.send(Command::CloseSession { session });
    }

    /// Run a script (split into statements by the engine's dialect), handing every query event to `on`. Returns when the query finishes.
    pub async fn execute(
        &mut self,
        session: SessionId,
        engine: Engine,
        sql: &str,
        confirmed: bool,
        tags: Vec<String>,
        mut on: impl FnMut(QueryEvent) -> Result<()>,
    ) -> Result<()> {
        let query = next_id();
        let statements: Vec<StatementRequest> = dialect_for(engine)
            .split_script(sql)
            .into_iter()
            .map(|span| StatementRequest {
                sql: span.text(sql).to_owned(),
                params: Vec::new(),
                offset: span.start,
            })
            .collect();
        self.send(Command::Execute {
            session,
            query,
            statements,
            tags,
            confirmed_destructive: confirmed,
            fetch_limit: FetchLimit::All,
        });
        let mut failure = None;
        loop {
            let ev = self
                .wait(|e| match e {
                    Event::Query { query: q, event } if q == query => Some(event),
                    _ => None,
                })
                .await?;
            match ev {
                QueryEvent::Finished { .. } => break,
                QueryEvent::Failed { ref error, .. } => failure = Some(error.to_string()),
                QueryEvent::NeedsConfirmation {
                    ref destructive, ..
                } => {
                    let what: Vec<String> = destructive.iter().map(|d| d.headline()).collect();
                    failure = Some(format!(
                        "this is a Production connection and the statement is destructive ({}); \
                         nothing ran. Pass --yes to run it.",
                        what.join("; ")
                    ));
                }
                other => on(other)?,
            }
        }
        match failure {
            Some(f) => bail!(f),
            None => Ok(()),
        }
    }

    /// Capture a plan.
    pub async fn explain(
        &mut self,
        session: SessionId,
        sql: &str,
        analyze: bool,
        confirmed: bool,
        tags: Vec<String>,
    ) -> Result<PlanResult> {
        let query = next_id();
        self.send(Command::Explain {
            session,
            query,
            sql: sql.to_owned(),
            analyze,
            confirmed,
            tags,
        });
        self.wait(|e| match e {
            Event::Plan {
                request,
                history_id,
                plan,
                findings,
            } if request == query => Some(Ok(PlanResult {
                plan,
                findings,
                history_id,
            })),
            Event::PlanFailed {
                request,
                error,
                needs_confirmation,
            } if request == query => Some(Err(if needs_confirmation {
                anyhow!("{error}; pass --yes to run it")
            } else {
                anyhow!("{error}")
            })),
            _ => None,
        })
        .await?
    }

    /// Read the workload.
    pub async fn workload(&mut self, session: SessionId) -> Result<Arc<Workload>> {
        let request = next_id();
        self.send(Command::Workload { session, request });
        self.wait(|e| match e {
            Event::Workload { request: r, result } if r == request => {
                Some(result.map_err(|e| anyhow!(e)))
            }
            _ => None,
        })
        .await?
    }

    /// Plan with hypothetical indexes.
    pub async fn what_if(
        &mut self,
        session: SessionId,
        sql: &str,
        indexes: Vec<String>,
    ) -> Result<Arc<WhatIf>> {
        let query = next_id();
        self.send(Command::WhatIf {
            session,
            query,
            sql: sql.to_owned(),
            indexes,
        });
        self.wait(|e| match e {
            Event::WhatIf { request, result } if request == query => {
                Some(result.map_err(|e| anyhow!(e)))
            }
            _ => None,
        })
        .await?
    }

    /// The guarded read-only query agents use.
    pub async fn agent_query(
        &mut self,
        session: SessionId,
        sql: &str,
        row_cap: usize,
        timeout: Duration,
        tags: Vec<String>,
    ) -> Result<AgentRows> {
        let query = next_id();
        self.send(Command::AgentQuery {
            session,
            query,
            sql: sql.to_owned(),
            row_cap,
            timeout,
            tags,
        });
        self.wait(|e| match e {
            Event::AgentRows { query: q, result } if q == query => {
                Some(result.map_err(|e| anyhow!(e)))
            }
            _ => None,
        })
        .await?
    }

    /// Load a catalog scope (fresh, not from the cache).
    pub async fn introspect(
        &mut self,
        session: SessionId,
        scope: IntrospectScope,
    ) -> Result<CatalogChunk> {
        self.send(Command::Introspect {
            session,
            scope: scope.clone(),
            refresh: true,
        });
        self.wait(|e| match e {
            Event::Catalog {
                session: s,
                scope: sc,
                result,
                ..
            } if s == session && sc == scope => Some(result.map_err(|e| anyhow!(e))),
            _ => None,
        })
        .await?
    }
}
