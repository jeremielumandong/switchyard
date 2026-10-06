//! The core service: owns the profile store, secret store, drivers, sessions and running
//! queries, and handles [`Command`]s on the runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bus::{TermId, TermTarget};
use crate::terminals::{TermInput, Terminals};
use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::d1::D1Driver;
use switchyard_db::guard;
use switchyard_db::pg::PgDriver;
use switchyard_db::{
    CancelHandle, DbConfig, DbError, DbSession, Driver, Engine, IntrospectScope, ResultEvent,
    dialect_for,
};
use switchyard_remote::{LocalFs, RemoteFs};
use switchyard_store::{
    AppPaths, DbConnection, HistoryEntry, HistoryStatus, KeychainStore, MemoryStore, Profile,
    ProfileId, SecretRef, SecretStore, Store, StoreError, VaultStore, now_ms,
};
use switchyard_term::{LocalShell, TermSize};
use tokio::sync::mpsc;
use tracing::{Instrument, info, info_span, warn};

use crate::bus::{
    Command, Event, FetchLimit, QueryEvent, QueryId, RequestId, SessionId, StatementRequest,
};
use crate::error::{CoreError, Result};
use crate::runtime::EventSender;

/// Where secrets go.
#[derive(Clone, Debug)]
pub enum SecretBackendChoice {
    /// OS keychain if available, else the vault at this path.
    Auto(PathBuf),
    /// In-memory (tests, demos).
    Memory,
}

/// Service configuration.
#[derive(Clone)]
pub struct ServiceConfig {
    /// Profile store file; `None` keeps it in memory.
    pub store_path: Option<PathBuf>,
    /// Secret backend.
    pub secrets: SecretBackendChoice,
    /// Extra drivers (tests); the PostgreSQL driver is always registered.
    pub extra_drivers: Vec<(Engine, Arc<dyn Driver>)>,
}

impl ServiceConfig {
    /// Everything in memory (tests).
    pub fn in_memory() -> Self {
        Self {
            store_path: None,
            secrets: SecretBackendChoice::Memory,
            extra_drivers: Vec::new(),
        }
    }

    /// Files under the platform directories.
    pub fn from_paths(paths: &AppPaths) -> Self {
        let secrets = match std::env::var("SWITCHYARD_SECRETS").as_deref() {
            Ok("memory") => SecretBackendChoice::Memory,
            _ => SecretBackendChoice::Auto(paths.vault_file()),
        };
        Self {
            store_path: Some(paths.store_file()),
            secrets,
            extra_drivers: Vec::new(),
        }
    }
}

enum Resume {
    More,
    All,
    Cancel,
}

struct QueryControl {
    cancel: CancelHandle,
    resume: mpsc::UnboundedSender<Resume>,
}

struct SessionInner {
    session: Box<dyn DbSession>,
    txn_statements: u32,
}

struct SessionSlot {
    connection: DbConnection,
    inner: tokio::sync::Mutex<SessionInner>,
}

/// The core service.
pub struct Service {
    events: EventSender,
    store: Arc<Mutex<Store>>,
    secrets: Arc<dyn SecretStore>,
    vault: Option<Arc<VaultStore>>,
    drivers: HashMap<Engine, Arc<dyn Driver>>,
    sessions: Mutex<HashMap<SessionId, Arc<SessionSlot>>>,
    queries: Mutex<HashMap<QueryId, QueryControl>>,
    terminals: Arc<Terminals>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Rows fetched per [`Command::FetchMore`] after the limit.
const FETCH_STEP: usize = 10_000;

impl Service {
    /// Build the service.
    pub fn new(config: ServiceConfig, events: EventSender) -> Result<Self> {
        let store = match &config.store_path {
            Some(p) => Store::open(p)?,
            None => Store::open_in_memory()?,
        };
        let (secrets, vault): (Arc<dyn SecretStore>, Option<Arc<VaultStore>>) =
            match &config.secrets {
                SecretBackendChoice::Memory => (Arc::new(MemoryStore::default()), None),
                SecretBackendChoice::Auto(vault_path) => {
                    if KeychainStore::available() {
                        (Arc::new(KeychainStore), None)
                    } else {
                        let v = Arc::new(VaultStore::new(vault_path));
                        (v.clone(), Some(v))
                    }
                }
            };
        info!(backend = secrets.backend(), "secret backend selected");
        let mut drivers: HashMap<Engine, Arc<dyn Driver>> = HashMap::new();
        drivers.insert(Engine::Postgres, Arc::new(PgDriver));
        drivers.insert(Engine::D1, Arc::new(D1Driver::default()));
        for (engine, d) in config.extra_drivers {
            drivers.insert(engine, d);
        }
        Ok(Self {
            events,
            store: Arc::new(Mutex::new(store)),
            secrets,
            vault,
            drivers,
            sessions: Mutex::default(),
            queries: Mutex::default(),
            terminals: Arc::default(),
        })
    }

    /// Process commands until the channel closes.
    pub async fn run(self: Arc<Self>, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.emit_secret_backend();
        while let Some(cmd) = commands.recv().await {
            // Keystrokes must reach the program in the order they were typed, so terminal
            // input is delivered here (it never blocks) instead of on a spawned task.
            let cmd = match cmd {
                Command::TerminalInput { term, bytes } => {
                    self.terminals.send(term, TermInput::Data(bytes));
                    continue;
                }
                Command::TerminalResize { term, size } => {
                    self.terminals.send(term, TermInput::Resize(size));
                    continue;
                }
                other => other,
            };
            let this = self.clone();
            tokio::spawn(async move { this.handle(cmd).await });
        }
    }

    fn emit(&self, e: Event) {
        self.events.emit(e);
    }

    fn emit_secret_backend(&self) {
        self.emit(Event::SecretBackend {
            name: self.secrets.backend(),
            locked: self.vault.as_ref().is_some_and(|v| !v.is_unlocked()),
        });
    }

    async fn with_store<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&mut lock(&store)))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .map_err(CoreError::from)
    }

    async fn with_secrets<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SecretStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T> {
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || f(secrets.as_ref()))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .map_err(CoreError::from)
    }

    async fn emit_profiles(&self) {
        match self.with_store(|s| s.profiles()).await {
            Ok(p) => self.emit(Event::Profiles(p)),
            Err(e) => self.error("Loading profiles", e),
        }
    }

    fn error(&self, context: &str, e: impl std::fmt::Display) {
        warn!(context, error = %e, "command failed");
        self.emit(Event::Error {
            context: context.into(),
            message: e.to_string(),
        });
    }

    async fn handle(&self, cmd: Command) {
        match cmd {
            Command::Ping { id, delay } => {
                tokio::time::sleep(delay).await;
                self.emit(Event::Pong { id });
            }
            Command::LoadProfiles => self.emit_profiles().await,
            Command::SaveProfile {
                request,
                profile,
                secret,
            } => self.save_profile(request, profile, secret).await,
            Command::DeleteProfile { id } => {
                let removed = self.with_store(move |s| s.delete_profile(&id)).await;
                match removed {
                    Ok(Some(p)) => {
                        if let Some(key) = p.secret().cloned() {
                            let _ = self.with_secrets(move |s| s.delete(&key)).await;
                        }
                        self.emit(Event::Toast(format!("Deleted {}", p.name())));
                    }
                    Ok(None) => {}
                    Err(e) => self.error("Delete", e),
                }
                self.emit_profiles().await;
            }
            Command::ReorderProfiles { ids } => {
                if let Err(e) = self.with_store(move |s| s.reorder(&ids)).await {
                    self.error("Reorder", e);
                }
                self.emit_profiles().await;
            }
            Command::ExportProfiles { path } => {
                let r = self
                    .with_store(move |s| {
                        let json = s.export_json()?;
                        std::fs::write(&path, json)?;
                        Ok(path)
                    })
                    .await;
                match r {
                    Ok(p) => self.emit(Event::Toast(format!(
                        "Exported profiles to {} · no secrets",
                        p.display()
                    ))),
                    Err(e) => self.error("Export", e),
                }
            }
            Command::ImportProfiles { path } => {
                let r = self
                    .with_store(move |s| {
                        let json = std::fs::read_to_string(&path)?;
                        s.import_json(&json)
                    })
                    .await;
                match r {
                    Ok(n) => self.emit(Event::Toast(format!("Imported {n} profiles"))),
                    Err(e) => self.error("Import", e),
                }
                self.emit_profiles().await;
            }
            Command::UnlockVault { password } => {
                if let Some(v) = self.vault.clone() {
                    let r = tokio::task::spawn_blocking(move || v.unlock(&password)).await;
                    match r {
                        Ok(Ok(())) => self.emit(Event::Toast("Vault unlocked".into())),
                        Ok(Err(e)) => self.error("Unlock vault", e),
                        Err(e) => self.error("Unlock vault", e),
                    }
                }
                self.emit_secret_backend();
            }
            Command::TestConnection {
                request,
                connection,
                secret,
            } => {
                let result = self.test_connection(connection, secret).await;
                self.emit(Event::TestResult {
                    request,
                    result: result.map_err(|e| e.to_string()),
                });
            }
            Command::OpenSession {
                session,
                connection,
            } => match self.open_session(session, connection).await {
                Ok(v) => self.emit(Event::SessionOpened {
                    session,
                    server_version: v,
                }),
                Err(e) => self.emit(Event::SessionFailed {
                    session,
                    message: e.to_string(),
                }),
            },
            Command::OpenTerminal { term, target, size } => {
                self.open_terminal(term, target, size).await
            }
            Command::TerminalInput { term, bytes } => {
                self.terminals.send(term, TermInput::Data(bytes));
            }
            Command::TerminalResize { term, size } => {
                self.terminals.send(term, TermInput::Resize(size));
            }
            Command::CloseTerminal { term } => self.terminals.close(term),
            Command::CloseSession { session } => {
                lock(&self.sessions).remove(&session);
            }
            Command::Execute {
                session,
                query,
                statements,
                tags,
                confirmed_destructive,
                fetch_limit,
            } => {
                let span = info_span!("query", query, session);
                self.execute(
                    session,
                    query,
                    statements,
                    tags,
                    confirmed_destructive,
                    fetch_limit,
                )
                .instrument(span)
                .await
            }
            Command::FetchMore { query, all } => {
                if let Some(q) = lock(&self.queries).get(&query) {
                    let _ = q.resume.send(if all { Resume::All } else { Resume::More });
                }
            }
            Command::Cancel { query } => {
                let ctl = lock(&self.queries)
                    .get(&query)
                    .map(|q| (q.cancel.clone(), q.resume.clone()));
                if let Some((cancel, resume)) = ctl {
                    let _ = resume.send(Resume::Cancel);
                    if let Err(e) = cancel.cancel().await {
                        self.error("Cancel", e);
                    }
                }
            }
            Command::Begin { session } => self.transaction(session, TxnOp::Begin).await,
            Command::Commit { session } => self.transaction(session, TxnOp::Commit).await,
            Command::Rollback { session } => self.transaction(session, TxnOp::Rollback).await,
            Command::Introspect {
                session,
                scope,
                refresh,
            } => self.introspect(session, scope, refresh).await,
            Command::SearchHistory {
                request,
                query,
                connection,
            } => {
                let r = self
                    .with_store(move |s| s.search_history(&query, connection.as_ref(), 500))
                    .await;
                match r {
                    Ok(entries) => self.emit(Event::History { request, entries }),
                    Err(e) => self.error("History", e),
                }
            }
            Command::SaveBuffer { buffer, position } => {
                if let Err(e) = self
                    .with_store(move |s| s.save_buffer(&buffer, position))
                    .await
                {
                    self.error("Autosave", e);
                }
            }
            Command::DeleteBuffer { id } => {
                let _ = self.with_store(move |s| s.delete_buffer(&id)).await;
            }
            Command::LoadWorkspace => match self.with_store(|s| s.workspace()).await {
                Ok(w) => self.emit(Event::Workspace(w)),
                Err(e) => self.error("Workspace", e),
            },
            Command::SaveWorkspace(w) => {
                if let Err(e) = self.with_store(move |s| s.save_workspace(&w)).await {
                    self.error("Workspace", e);
                }
            }
            Command::SetSetting { key, value } => {
                if let Err(e) = self.with_store(move |s| s.set_setting(&key, &value)).await {
                    self.error("Settings", e);
                }
            }
            Command::ListLocalDir { request, path } => {
                let result = LocalFs.list(&path).await.map_err(|e| e.to_string());
                self.emit(Event::DirListing {
                    request,
                    path,
                    result,
                });
            }
            Command::WriteFile { path, contents } => {
                match tokio::fs::write(&path, contents).await {
                    Ok(()) => self.emit(Event::Toast(format!("Saved {}", path.display()))),
                    Err(e) => self.error("Export", e),
                }
            }
            Command::ImportSshConfig => match self.import_ssh_config().await {
                Ok(0) => self.emit(Event::Toast("No new Hosts found in ~/.ssh/config".into())),
                Ok(n) => {
                    self.emit(Event::Toast(format!(
                        "Imported {n} Hosts from ~/.ssh/config"
                    )));
                    self.emit_profiles().await;
                }
                Err(e) => self.error("Import ssh config", e),
            },
            Command::ApplyEdits {
                session,
                request,
                statements,
            } => {
                let started = Instant::now();
                let result = self
                    .apply_edits(session, statements)
                    .await
                    .map_err(|e| e.to_string());
                self.emit(Event::EditsApplied {
                    request,
                    result,
                    elapsed: started.elapsed(),
                });
            }
            Command::DetectComponents => {
                let comps = tokio::task::spawn_blocking(switchyard_drivers::detect_all)
                    .await
                    .unwrap_or_default();
                self.emit(Event::Components(comps));
            }
        }
    }

    async fn import_ssh_config(&self) -> Result<usize> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| CoreError::NotFound("home directory".into()))?;
        let path = PathBuf::from(home).join(".ssh").join("config");
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| CoreError::Store(StoreError::Io(e)))?;
        let parsed = switchyard_remote::parse_ssh_config(&text);
        self.with_store(move |s| {
            let existing = s.profiles()?;
            let mut by_alias: HashMap<String, ProfileId> = existing
                .iter()
                .filter_map(|p| match p {
                    Profile::Host(h) => Some((h.name.clone(), h.id.clone())),
                    _ => None,
                })
                .collect();
            // Create every new Host first, then wire ProxyJump references.
            let mut created = Vec::new();
            for h in &parsed {
                if by_alias.contains_key(&h.alias) {
                    continue;
                }
                let mut host = switchyard_store::Host::new(
                    h.alias.clone(),
                    h.hostname.clone(),
                    h.user
                        .clone()
                        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "root".into())),
                );
                host.port = h.port.unwrap_or(22);
                if let Some(key) = &h.identity_file {
                    host.auth = switchyard_store::SshAuth::PublicKey {
                        key_path: key.clone(),
                    };
                }
                by_alias.insert(h.alias.clone(), host.id.clone());
                created.push((host, h.proxy_jump.clone()));
            }
            for (host, _) in &created {
                s.save_profile(&Profile::Host(host.clone()))?;
            }
            for (mut host, jumps) in created.clone() {
                host.jump_hosts = jumps
                    .iter()
                    .filter_map(|j| by_alias.get(j).cloned())
                    .collect();
                if !host.jump_hosts.is_empty() {
                    s.save_profile(&Profile::Host(host))?;
                }
            }
            Ok(created.len())
        })
        .await
    }

    async fn apply_edits(&self, session: SessionId, statements: Vec<String>) -> Result<u64> {
        let slot = self
            .slot(session)
            .ok_or_else(|| CoreError::NotFound("session".into()))?;
        if slot.connection.read_only {
            return Err(CoreError::Unsupported(
                "this connection is locked read-only".into(),
            ));
        }
        let mut inner = slot.inner.lock().await;
        let own_txn = !inner.session.in_transaction();
        if own_txn {
            inner.session.begin().await?;
        }
        let mut total = 0u64;
        for sql in &statements {
            let outcome: Result<u64> = async {
                let mut stream = inner.session.execute(sql, &[]).await?;
                let mut affected = 0;
                while let Some(ev) = stream.next().await {
                    if let ResultEvent::Done(c) = ev? {
                        affected = c.affected.unwrap_or(0);
                    }
                }
                Ok(affected)
            }
            .await;
            match outcome {
                Ok(1) => total += 1,
                Ok(n) => {
                    if own_txn {
                        let _ = inner.session.rollback().await;
                    }
                    return Err(CoreError::Unsupported(format!(
                        "expected to change 1 row but changed {n}; nothing was saved"
                    )));
                }
                Err(e) => {
                    if own_txn {
                        let _ = inner.session.rollback().await;
                    }
                    return Err(e);
                }
            }
        }
        if own_txn {
            inner.session.commit().await?;
        }
        let conn = slot.connection.clone();
        for sql in &statements {
            let st = StatementRequest {
                sql: sql.clone(),
                params: vec![],
                offset: 0,
            };
            self.record(
                &conn,
                &st,
                now_ms(),
                Duration::ZERO,
                0,
                &(HistoryStatus::Ok, None, Some(1)),
                &["edit".into()],
            )
            .await;
        }
        Ok(total)
    }

    async fn save_profile(
        &self,
        request: RequestId,
        mut profile: Profile,
        secret: Option<SecretString>,
    ) {
        if let Some(secret) = secret {
            let purpose = match &profile {
                Profile::Host(_) => "passphrase",
                _ => "password",
            };
            let key = SecretRef::for_profile(profile.id(), purpose);
            let k = key.clone();
            if let Err(e) = self.with_secrets(move |s| s.set(&k, &secret)).await {
                self.emit(Event::ProfileError {
                    request,
                    field: None,
                    message: e.to_string(),
                });
                self.emit_secret_backend();
                return;
            }
            match &mut profile {
                Profile::Host(h) => h.secret = Some(key),
                Profile::Db(d) => d.secret = Some(key),
                Profile::File(f) => f.secret = Some(key),
                Profile::Terminal(_) => {}
            }
        }
        let id = profile.id().clone();
        match self.with_store(move |s| s.save_profile(&profile)).await {
            Ok(()) => self.emit(Event::ProfileSaved { request, id }),
            Err(CoreError::Store(StoreError::Validation(v))) => self.emit(Event::ProfileError {
                request,
                field: Some(v.field),
                message: v.message,
            }),
            Err(e) => self.emit(Event::ProfileError {
                request,
                field: None,
                message: e.to_string(),
            }),
        }
        self.emit_profiles().await;
    }

    async fn open_terminal(&self, term: TermId, target: TermTarget, size: TermSize) {
        let result = match target {
            TermTarget::Local { profile } => {
                let profile = match profile {
                    Some(id) => match self.with_store(move |s| s.profile(&id)).await {
                        Ok(Some(Profile::Terminal(t))) => Some(t),
                        _ => None,
                    },
                    None => None,
                };
                let shell = LocalShell {
                    program: profile
                        .as_ref()
                        .map(|t| t.shell.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                    env: profile.as_ref().map(|t| t.env.clone()).unwrap_or_default(),
                    ..LocalShell::default()
                };
                let description = shell
                    .program
                    .clone()
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "login shell".into());
                let startup = profile.and_then(|t| t.startup_command);
                self.terminals
                    .open_local(
                        term,
                        shell,
                        size,
                        switchyard_term::DEFAULT_SCROLLBACK,
                        self.events.clone(),
                    )
                    .map(|t| {
                        if let Some(cmd) = startup {
                            self.terminals
                                .send(term, TermInput::Data(format!("{cmd}\r").into_bytes()));
                        }
                        (t, description)
                    })
                    .map_err(|e| e.to_string())
            }
            TermTarget::Host(_) => Err("SSH terminals are not available yet (milestone M2)".into()),
        };
        match result {
            Ok((terminal, description)) => self.emit(Event::TerminalOpened {
                term,
                terminal,
                description,
            }),
            Err(message) => self.emit(Event::TerminalFailed { term, message }),
        }
    }

    async fn db_config(&self, c: &DbConnection, secret: Option<SecretString>) -> Result<DbConfig> {
        if c.via_host.is_some() {
            return Err(CoreError::Unsupported(
                "connecting through a Host needs SSH tunnels, which are not available yet".into(),
            ));
        }
        let password = match secret {
            Some(s) => Some(s),
            None => match c.secret.clone() {
                Some(key) => self.with_secrets(move |s| s.get(&key)).await?,
                None => None,
            },
        };
        let mut cfg = DbConfig::new(c.engine, c.server.clone(), c.database.clone());
        cfg.port = c.port;
        cfg.user = c.user.clone();
        cfg.password = password;
        cfg.auth = c.auth;
        cfg.ssl_mode = c.ssl_mode;
        cfg.read_only = c.read_only;
        Ok(cfg)
    }

    fn driver(&self, engine: Engine) -> Result<Arc<dyn Driver>> {
        self.drivers.get(&engine).cloned().ok_or_else(|| {
            CoreError::Unsupported(format!(
                "{} driver is not available yet",
                engine.display_name()
            ))
        })
    }

    async fn test_connection(
        &self,
        c: DbConnection,
        secret: Option<SecretString>,
    ) -> Result<String> {
        let driver = self.driver(c.engine)?;
        let cfg = self.db_config(&c, secret).await?;
        let started = Instant::now();
        let session = driver.connect(&cfg, None).await?;
        let ms = started.elapsed().as_millis();
        Ok(format!(
            "Connected · {} · {ms} ms",
            session.server_version()
        ))
    }

    async fn open_session(&self, session: SessionId, id: ProfileId) -> Result<String> {
        let profile = self.with_store(move |s| s.profile(&id)).await?;
        let Some(Profile::Db(conn)) = profile else {
            return Err(CoreError::NotFound("connection".into()));
        };
        let driver = self.driver(conn.engine)?;
        let cfg = self.db_config(&conn, None).await?;
        let s = driver.connect(&cfg, None).await?;
        let version = s.server_version();
        lock(&self.sessions).insert(
            session,
            Arc::new(SessionSlot {
                connection: conn,
                inner: tokio::sync::Mutex::new(SessionInner {
                    session: s,
                    txn_statements: 0,
                }),
            }),
        );
        Ok(version)
    }

    fn slot(&self, session: SessionId) -> Option<Arc<SessionSlot>> {
        lock(&self.sessions).get(&session).cloned()
    }

    async fn transaction(&self, session: SessionId, op: TxnOp) {
        let Some(slot) = self.slot(session) else {
            return self.error("Transaction", "session is not open");
        };
        let mut inner = slot.inner.lock().await;
        let r = match op {
            TxnOp::Begin => inner.session.begin().await,
            TxnOp::Commit => inner.session.commit().await,
            TxnOp::Rollback => inner.session.rollback().await,
        };
        if let Err(e) = r {
            self.error("Transaction", e);
        }
        inner.txn_statements = 0;
        let open = inner.session.in_transaction();
        self.emit(Event::Transaction {
            session,
            open,
            statements: 0,
        });
        if matches!(op, TxnOp::Commit) {
            self.emit(Event::Toast("Committed".into()));
        } else if matches!(op, TxnOp::Rollback) {
            self.emit(Event::Toast("Rolled back".into()));
        }
    }

    async fn introspect(&self, session: SessionId, scope: IntrospectScope, refresh: bool) {
        let Some(slot) = self.slot(session) else {
            return self.emit(Event::Catalog {
                session,
                scope,
                result: Err("session is not open".into()),
                cached_at: now_ms(),
            });
        };
        let conn_id = slot.connection.id.clone();
        let key = serde_json::to_string(&scope).unwrap_or_default();
        if !refresh {
            let (c, k) = (conn_id.clone(), key.clone());
            if let Ok(Some((chunk, at))) = self.with_store(move |s| s.cached_schema(&c, &k)).await {
                return self.emit(Event::Catalog {
                    session,
                    scope,
                    result: Ok(chunk),
                    cached_at: at,
                });
            }
        }
        let result = {
            let mut inner = slot.inner.lock().await;
            inner.session.introspect(scope.clone()).await
        };
        if let Ok(chunk) = &result {
            let chunk = chunk.clone();
            let _ = self
                .with_store(move |s| s.cache_schema(&conn_id, &key, &chunk))
                .await;
        }
        self.emit(Event::Catalog {
            session,
            scope,
            result: result.map_err(|e| e.to_string()),
            cached_at: now_ms(),
        });
    }

    fn query_event(&self, query: QueryId, event: QueryEvent) {
        self.emit(Event::Query { query, event });
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        session: SessionId,
        query: QueryId,
        statements: Vec<StatementRequest>,
        tags: Vec<String>,
        confirmed_destructive: bool,
        fetch_limit: FetchLimit,
    ) {
        let started = Instant::now();
        let Some(slot) = self.slot(session) else {
            self.query_event(
                query,
                QueryEvent::Failed {
                    index: 0,
                    error: DbError::Closed,
                    location: None,
                },
            );
            self.query_event(
                query,
                QueryEvent::Finished {
                    elapsed: started.elapsed(),
                    cancelled: false,
                },
            );
            return;
        };
        let conn = slot.connection.clone();
        let dialect = dialect_for(conn.engine);
        // Guards run before taking the session, so nothing executes on refusal.
        for (index, st) in statements.iter().enumerate() {
            let class = guard::classify(dialect, &st.sql);
            if conn.read_only && !class.is_read_only() {
                self.query_event(
                    query,
                    QueryEvent::Failed {
                        index,
                        error: DbError::Unsupported(
                            "this connection is locked read-only; the statement would write".into(),
                        ),
                        location: None,
                    },
                );
                self.query_event(
                    query,
                    QueryEvent::Finished {
                        elapsed: started.elapsed(),
                        cancelled: false,
                    },
                );
                return;
            }
            if conn.environment.is_production() && !confirmed_destructive {
                let destructive: Vec<_> = class.destructive().to_vec();
                if !destructive.is_empty() {
                    self.query_event(query, QueryEvent::NeedsConfirmation { index, destructive });
                    self.query_event(
                        query,
                        QueryEvent::Finished {
                            elapsed: started.elapsed(),
                            cancelled: false,
                        },
                    );
                    return;
                }
            }
        }

        let mut inner = slot.inner.lock().await;
        let (resume_tx, mut resume_rx) = mpsc::unbounded_channel();
        let cancel = inner.session.cancel_handle();
        lock(&self.queries).insert(
            query,
            QueryControl {
                cancel: cancel.clone(),
                resume: resume_tx,
            },
        );

        let mut cancelled = false;
        let mut limit = match fetch_limit {
            FetchLimit::Rows(n) => Some(n),
            FetchLimit::All => None,
        };
        'statements: for (index, st) in statements.iter().enumerate() {
            if cancel.is_requested() {
                cancelled = true;
                break;
            }
            self.query_event(query, QueryEvent::StatementStarted { index });
            let st_started = Instant::now();
            let started_at = now_ms();
            let mut rows_total: i64 = 0;
            let mut outcome: (HistoryStatus, Option<String>, Option<i64>) =
                (HistoryStatus::Ok, None, None);
            match inner.session.execute(&st.sql, &st.params).await {
                Err(e) => {
                    outcome = self.fail(query, index, dialect, st, e, &mut cancelled);
                    self.record(
                        &conn,
                        st,
                        started_at,
                        st_started.elapsed(),
                        rows_total,
                        &outcome,
                        &tags,
                    )
                    .await;
                    break 'statements;
                }
                Ok(mut stream) => {
                    let mut rows_in_set = 0usize;
                    let mut failed = false;
                    while let Some(ev) = stream.next().await {
                        match ev {
                            Ok(ResultEvent::Columns(c)) => {
                                rows_in_set = 0;
                                self.query_event(query, QueryEvent::Columns(c));
                            }
                            Ok(ResultEvent::Rows(b)) => {
                                rows_in_set += b.len();
                                rows_total += b.len() as i64;
                                self.query_event(query, QueryEvent::Rows(b));
                                if let Some(l) = limit
                                    && rows_in_set >= l
                                {
                                    self.query_event(
                                        query,
                                        QueryEvent::Paused { rows: rows_in_set },
                                    );
                                    match resume_rx.recv().await {
                                        Some(Resume::More) => limit = Some(l + FETCH_STEP),
                                        Some(Resume::All) => limit = None,
                                        Some(Resume::Cancel) | None => {
                                            cancelled = true;
                                            outcome.0 = HistoryStatus::Cancelled;
                                            drop(stream);
                                            self.record(
                                                &conn,
                                                st,
                                                started_at,
                                                st_started.elapsed(),
                                                rows_total,
                                                &outcome,
                                                &tags,
                                            )
                                            .await;
                                            break 'statements;
                                        }
                                    }
                                }
                            }
                            Ok(ResultEvent::Notice(n)) => {
                                self.query_event(query, QueryEvent::Notice(n))
                            }
                            Ok(ResultEvent::NextResultSet) => {
                                self.query_event(query, QueryEvent::NextResultSet)
                            }
                            Ok(ResultEvent::Done(c)) => {
                                outcome.2 = c.affected.map(|a| a as i64);
                                self.query_event(
                                    query,
                                    QueryEvent::StatementDone {
                                        index,
                                        completion: c,
                                    },
                                );
                            }
                            Err(e) => {
                                outcome = self.fail(query, index, dialect, st, e, &mut cancelled);
                                failed = true;
                                break;
                            }
                        }
                    }
                    self.record(
                        &conn,
                        st,
                        started_at,
                        st_started.elapsed(),
                        rows_total,
                        &outcome,
                        &tags,
                    )
                    .await;
                    if failed {
                        break 'statements;
                    }
                }
            }
            // Transaction bookkeeping.
            let open = inner.session.in_transaction();
            inner.txn_statements = if open { inner.txn_statements + 1 } else { 0 };
            self.emit(Event::Transaction {
                session,
                open,
                statements: inner.txn_statements,
            });
            if is_ddl(&st.sql) {
                let id = conn.id.clone();
                let _ = self.with_store(move |s| s.invalidate_schema(&id)).await;
            }
        }
        drop(inner);
        lock(&self.queries).remove(&query);
        self.query_event(
            query,
            QueryEvent::Finished {
                elapsed: started.elapsed(),
                cancelled,
            },
        );
    }

    fn fail(
        &self,
        query: QueryId,
        index: usize,
        dialect: &dyn switchyard_db::Dialect,
        st: &StatementRequest,
        e: DbError,
        cancelled: &mut bool,
    ) -> (HistoryStatus, Option<String>, Option<i64>) {
        if matches!(e, DbError::Cancelled) {
            *cancelled = true;
            return (HistoryStatus::Cancelled, None, None);
        }
        let location = e.as_server().and_then(|s| {
            // Relative to the statement; the UI adds the statement's buffer position.
            s.position.map(|p| dialect.error_line_col(&st.sql, p))
        });
        let message = e.to_string();
        self.query_event(
            query,
            QueryEvent::Failed {
                index,
                error: e,
                location,
            },
        );
        (HistoryStatus::Error, Some(message), None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        conn: &DbConnection,
        st: &StatementRequest,
        started_at: i64,
        elapsed: Duration,
        rows: i64,
        outcome: &(HistoryStatus, Option<String>, Option<i64>),
        tags: &[String],
    ) {
        if !conn.history_enabled {
            return;
        }
        let entry = HistoryEntry {
            id: 0,
            connection_id: Some(conn.id.clone()),
            connection_name: conn.name.clone(),
            sql: st.sql.clone(),
            started_at,
            duration_ms: elapsed.as_millis() as i64,
            rows: Some(rows),
            affected: outcome.2,
            status: outcome.0,
            error: outcome.1.clone(),
            tags: tags.to_vec(),
        };
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "history write failed");
        }
    }
}

enum TxnOp {
    Begin,
    Commit,
    Rollback,
}

fn is_ddl(sql: &str) -> bool {
    let first: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "CREATE" | "ALTER" | "DROP" | "COMMENT" | "GRANT" | "REVOKE"
    )
}
