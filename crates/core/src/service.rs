//! The core service: owns the profile store, secret store, drivers, sessions and running
//! queries, and handles [`Command`]s on the runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::bus::{TermId, TermTarget};
use crate::components::MIRROR_SETTING;
use crate::entra::EntraSignIn;
use crate::prompts::BusPrompter;
use crate::terminals::{SshTerminalSpec, TermInput, Terminals};
use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::d1::D1Driver;
use switchyard_db::guard;
use switchyard_db::mssql::MssqlDriver;
use switchyard_db::pg::PgDriver;
use switchyard_db::{
    CancelHandle, DbConfig, DbError, DbSession, Driver, Engine, IntrospectScope, ResultEvent,
    dialect_for,
};
use switchyard_db::{DbAuthMethod, TunnelEndpoint};
use switchyard_drivers::Registry;
use switchyard_drivers::install::CommandRunner;
use switchyard_remote::ssh::{
    KnownHosts, SshAuthMethod, SshManager, SshTarget, Tunnel, TunnelInfo,
};
use switchyard_remote::{LocalFs, RemoteFs};
use switchyard_store::{
    AppPaths, DbConnection, HistoryEntry, HistoryStatus, Host, KeychainStore, MemoryStore, Profile,
    ProfileId, SecretRef, SecretStore, SshAuth, Store, StoreError, VaultStore, now_ms,
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
    /// Host key files.
    pub known_hosts: KnownHosts,
    /// App-managed native components (`<data>/drivers`).
    pub drivers_dir: PathBuf,
    /// Package-manager runner (tests replace it).
    pub package_runner: Option<Arc<dyn CommandRunner>>,
}

impl ServiceConfig {
    /// Everything in memory (tests).
    pub fn in_memory() -> Self {
        Self {
            store_path: None,
            secrets: SecretBackendChoice::Memory,
            extra_drivers: Vec::new(),
            known_hosts: KnownHosts {
                user_file: None,
                app_file: std::env::temp_dir()
                    .join(format!("switchyard-known-hosts-{}", std::process::id())),
            },
            drivers_dir: std::env::temp_dir()
                .join(format!("switchyard-drivers-{}", std::process::id())),
            package_runner: None,
        }
    }

    /// Files under the platform directories.
    pub fn from_paths(paths: &AppPaths) -> Self {
        let secrets = match std::env::var("SWITCHYARD_SECRETS").as_deref() {
            Ok("memory") => SecretBackendChoice::Memory,
            _ => SecretBackendChoice::Auto(paths.vault_file()),
        };
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        Self {
            store_path: Some(paths.store_file()),
            secrets,
            extra_drivers: Vec::new(),
            known_hosts: KnownHosts {
                user_file: home.map(|h| PathBuf::from(h).join(".ssh").join("known_hosts")),
                app_file: paths.data.join("known_hosts"),
            },
            drivers_dir: paths.drivers_dir(),
            package_runner: None,
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
    /// Keeps the SSH tunnel open while the session uses it.
    tunnel: Option<Arc<Tunnel>>,
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
    ssh: Arc<SshManager>,
    tunnels: Mutex<Vec<Weak<Tunnel>>>,
    /// Serializes tunnel creation so concurrent sessions share one tunnel.
    tunnel_open: tokio::sync::Mutex<()>,
    next_tunnel: std::sync::atomic::AtomicU64,
    prompter: Arc<BusPrompter>,
    entra: EntraSignIn,
    components: Arc<Registry>,
    package_runner: Arc<dyn CommandRunner>,
    files: Arc<crate::files::Files>,
}

fn endpoint_of(t: &Tunnel) -> TunnelEndpoint {
    TunnelEndpoint {
        host: t.local().ip().to_string(),
        port: t.local().port(),
    }
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
        let prompter = BusPrompter::new(events.clone());
        let ssh = Arc::new(SshManager::new(
            config.known_hosts.clone(),
            prompter.clone(),
        ));
        let components = Arc::new(Registry::new(config.drivers_dir.clone()));
        // Debug builds only: try the Driver Manager against a local, unsigned manifest.
        if cfg!(debug_assertions)
            && let Some(path) = std::env::var_os("SWITCHYARD_DEV_MANIFEST")
        {
            match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|b| switchyard_drivers::Manifest::parse(&b).map_err(|e| e.to_string()))
            {
                Ok(m) => components.set_manifest(m),
                Err(e) => warn!(error = %e, "SWITCHYARD_DEV_MANIFEST ignored"),
            }
        }
        if let Ok(Some(mirror)) = store.setting::<String>(MIRROR_SETTING) {
            components.set_mirror(Some(mirror));
        }
        let mut drivers: HashMap<Engine, Arc<dyn Driver>> = HashMap::new();
        drivers.insert(Engine::Postgres, Arc::new(PgDriver));
        drivers.insert(Engine::D1, Arc::new(D1Driver::default()));
        drivers.insert(Engine::SqlServer, Arc::new(MssqlDriver));
        for (engine, d) in config.extra_drivers {
            drivers.insert(engine, d);
        }
        Ok(Self {
            events,
            store: Arc::new(Mutex::new(store)),
            secrets: secrets.clone(),
            vault,
            drivers,
            sessions: Mutex::default(),
            queries: Mutex::default(),
            terminals: Arc::default(),
            ssh,
            tunnels: Mutex::default(),
            tunnel_open: tokio::sync::Mutex::new(()),
            next_tunnel: std::sync::atomic::AtomicU64::new(1),
            entra: EntraSignIn::new(prompter.clone(), secrets.clone()),
            components,
            files: Arc::default(),
            package_runner: config
                .package_runner
                .clone()
                .unwrap_or_else(|| Arc::new(crate::components::system_runner())),
            prompter,
        })
    }

    /// Process commands until the channel closes.
    pub async fn run(self: Arc<Self>, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.emit_secret_backend();
        // Tunnel byte counters change without any command; report them once a second
        // while something changed.
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            let mut last = Vec::new();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(this) = weak.upgrade() else { break };
                let now = this.tunnel_infos();
                if now != last {
                    this.emit(Event::Tunnels(now.clone()));
                    last = now;
                }
            }
        });
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
                Command::ReconnectTerminal { term } => {
                    self.terminals.send(term, TermInput::Reconnect);
                    continue;
                }
                Command::AnswerPrompt { request, answer } => {
                    self.prompter.answer(request, answer);
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
            } => {
                // New user, tenant or method: the next connect asks Microsoft again
                // (the stored refresh token is tried first).
                if let Profile::Db(d) = &profile {
                    self.entra.drop_cached(&d.id);
                }
                self.save_profile(request, profile, secret).await
            }
            Command::DeleteProfile { id } => {
                let removed = self.with_store(move |s| s.delete_profile(&id)).await;
                match removed {
                    Ok(Some(p)) => {
                        if let Some(key) = p.secret().cloned() {
                            let _ = self.with_secrets(move |s| s.delete(&key)).await;
                        }
                        if let Profile::Db(d) = &p
                            && d.auth.is_entra()
                        {
                            let _ = self.entra.forget(&d.id).await;
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
            Command::TestHost {
                request,
                host,
                secret,
            } => {
                let result = self.test_host(host, secret).await;
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
            Command::ReconnectTerminal { term } => {
                self.terminals.send(term, TermInput::Reconnect);
            }
            Command::AnswerPrompt { request, answer } => self.prompter.answer(request, answer),
            Command::AcceptChangedHostKey { host, fingerprint } => {
                self.ssh.accept_changed_key(&host.0, &fingerprint);
            }
            Command::StopTunnel { id } => self.stop_tunnel(id),
            Command::ListTunnels => self.emit(Event::Tunnels(self.tunnel_infos())),
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
            Command::ListDir { request, fs, path } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, posix)) => {
                        let path = match path {
                            Some(p) => crate::files::expand_path(&p, &f.home(), posix),
                            None => f.home(),
                        };
                        let r = f.list(&path).await.map_err(|e| e.to_string());
                        Ok((path, r))
                    }
                    Err(e) => Err(e.to_string()),
                };
                let (path, result) = match result {
                    Ok((p, r)) => (p, r),
                    Err(e) => (PathBuf::new(), Err(e)),
                };
                self.emit(Event::FsListing {
                    request,
                    fs,
                    path,
                    result,
                });
            }
            Command::Transfer {
                id,
                from,
                path,
                to,
                dir,
                on_conflict,
                resume,
            } => {
                let result = self
                    .transfer(id, &from, &path, &to, dir, on_conflict, resume)
                    .await;
                self.files.finish(id);
                self.emit(Event::TransferDone { id, result });
            }
            Command::CancelTransfer { id } => self.files.cancel(id),
            Command::PauseTransfer { id } => self.files.pause(id),
            Command::FsOp { request, fs, op } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, posix)) => match &op {
                        crate::bus::FsOp::Mkdir(p) => f.mkdir(p).await,
                        crate::bus::FsOp::Rename(a, b) => f.rename(a, b).await,
                        crate::bus::FsOp::Delete(p) => {
                            crate::files::delete_tree(f.as_ref(), p, posix).await
                        }
                    }
                    .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                self.emit(Event::FsOpDone {
                    request,
                    fs,
                    result,
                });
            }
            Command::ReadTextFile { request, fs, path } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, _)) => crate::files::read_text(f.as_ref(), &path).await,
                    Err(e) => Err(e.to_string()),
                };
                self.emit(Event::TextFileRead { request, result });
            }
            Command::WriteTextFile {
                request,
                fs,
                path,
                content,
                expect_modified,
                force,
            } => {
                let result = match self.file_system(&fs).await {
                    Ok((f, _)) => {
                        crate::files::write_text(
                            f.as_ref(),
                            &path,
                            &content,
                            expect_modified,
                            force,
                        )
                        .await
                    }
                    Err(e) => Err(crate::bus::SaveError::Failed(e.to_string())),
                };
                self.emit(Event::TextFileSaved { request, result });
            }
            Command::LoadSetting { key } => {
                let k = key.clone();
                let value = self
                    .with_store(move |s| s.setting::<serde_json::Value>(&k))
                    .await
                    .unwrap_or(None);
                self.emit(Event::Setting { key, value });
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
            Command::PreviewSshConfig => match self.preview_ssh_config().await {
                Ok((path, hosts)) => self.emit(Event::SshConfigPreview { path, hosts }),
                Err(e) => self.error("Read ssh config", e),
            },
            Command::ImportSshConfig { only } => match self.import_ssh_config(only).await {
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
            Command::DetectComponents => self.emit_components().await,
            Command::InstallComponent { id, accept_license } => {
                let r = crate::components::install(
                    &self.components,
                    &self.events,
                    &id,
                    accept_license,
                    None,
                    self.package_runner.as_ref(),
                )
                .await;
                if let Err(e) = r {
                    self.emit(crate::components::failure_event(&id, &e));
                }
                self.emit_components().await;
            }
            Command::InstallComponentFromFile { id, path } => {
                let r = crate::components::install(
                    &self.components,
                    &self.events,
                    &id,
                    true,
                    Some(path),
                    self.package_runner.as_ref(),
                )
                .await;
                if let Err(e) = r {
                    self.emit(crate::components::failure_event(&id, &e));
                }
                self.emit_components().await;
            }
            Command::UseComponentPath { id, path } => {
                let reg = self.components.clone();
                let i = id.clone();
                let r = tokio::task::spawn_blocking(move || reg.set_path(&i, &path))
                    .await
                    .map_err(|e| switchyard_drivers::DriverError::Io(e.to_string()))
                    .and_then(|r| r);
                match r {
                    Ok(component) => self.emit(Event::ComponentInstalled { component }),
                    Err(e) => self.emit(crate::components::failure_event(&id, &e)),
                }
                self.emit_components().await;
            }
            Command::RemoveComponent { id } => {
                let reg = self.components.clone();
                let i = id.clone();
                let r = tokio::task::spawn_blocking(move || reg.remove(&i))
                    .await
                    .map_err(|e| switchyard_drivers::DriverError::Io(e.to_string()))
                    .and_then(|r| r);
                match r {
                    Ok(c) => self.emit(Event::Toast(format!("Removed {}", c.name))),
                    Err(e) => self.emit(crate::components::failure_event(&id, &e)),
                }
                self.emit_components().await;
            }
            Command::SetDriverMirror { url } => {
                let url = url.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty());
                self.components.set_mirror(url.clone());
                let saved = url.clone().unwrap_or_default();
                if let Err(e) = self
                    .with_store(move |s| s.set_setting(MIRROR_SETTING, &saved))
                    .await
                {
                    self.error("Save mirror", e);
                }
                self.emit_components().await;
            }
        }
    }

    /// The file system behind `fs`, and whether its paths are POSIX. SFTP opens once per
    /// Host on the shared SSH session (prompting like a terminal would).
    async fn file_system(
        &self,
        fs: &crate::bus::FsRef,
    ) -> Result<(Arc<dyn switchyard_remote::RemoteFs>, bool)> {
        let id = match fs {
            crate::bus::FsRef::Local => return Ok((Arc::new(LocalFs), cfg!(unix))),
            crate::bus::FsRef::Host(id) => id,
        };
        let mut open = self.files.sftp.lock().await;
        if let Some(f) = open.get(&id.0)
            && !f.is_closed()
        {
            return Ok((f.clone(), true));
        }
        let host = self.host(id).await?;
        let target = self.ssh_target(id).await?;
        let conn = self
            .ssh
            .session(&target)
            .await
            .map_err(|e| CoreError::Unsupported(e.to_string()))?;
        let f = Arc::new(
            switchyard_remote::SftpFs::open(conn, host.name.clone())
                .await
                .map_err(|e| CoreError::Unsupported(format!("SFTP on {}: {e}", host.name)))?,
        );
        open.insert(id.0.clone(), f.clone());
        Ok((f, true))
    }

    #[allow(clippy::too_many_arguments)]
    async fn transfer(
        &self,
        id: u64,
        from: &crate::bus::FsRef,
        path: &std::path::Path,
        to: &crate::bus::FsRef,
        dir: Option<PathBuf>,
        on_conflict: crate::bus::OnConflict,
        resume: bool,
    ) -> std::result::Result<PathBuf, crate::bus::TransferError> {
        use crate::bus::TransferError;
        // Pause / cancel work while waiting for a slot too.
        let control = self.files.start(id);
        let slot = match self.files.slots.clone().try_acquire_owned() {
            Ok(s) => s,
            Err(_) => {
                self.emit(Event::TransferQueued { id });
                self.files
                    .slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| TransferError::Failed(e.to_string()))?
            }
        };
        let _slot = slot;
        match control.load(std::sync::atomic::Ordering::SeqCst) {
            1 => return Err(TransferError::Cancelled),
            2 => return Err(TransferError::Paused),
            _ => {}
        }
        let fail = |e: CoreError| TransferError::Failed(e.to_string());
        let (src, src_posix) = self.file_system(from).await.map_err(fail)?;
        let (dst, dst_posix) = self.file_system(to).await.map_err(fail)?;
        let dir = match dir {
            Some(d) => d,
            None => {
                let d = LocalFs.home().join("Downloads");
                tokio::fs::create_dir_all(&d)
                    .await
                    .map_err(|e| TransferError::Failed(e.to_string()))?;
                d
            }
        };
        let name = switchyard_remote::fs::file_name(path);
        let events = self.events.clone();
        let progress = move |done: u64, total: Option<u64>| {
            events.emit(Event::TransferProgress {
                id,
                name: name.clone(),
                done,
                total,
            })
        };
        crate::files::transfer(
            src.as_ref(),
            src_posix,
            path,
            dst.as_ref(),
            dst_posix,
            &dir,
            on_conflict,
            resume,
            &control,
            &progress,
        )
        .await
    }

    async fn emit_components(&self) {
        let reg = self.components.clone();
        let comps = tokio::task::spawn_blocking(move || reg.components())
            .await
            .unwrap_or_default();
        self.emit(Event::Components(comps));
    }

    async fn read_ssh_config(&self) -> Result<(PathBuf, Vec<switchyard_remote::SshConfigHost>)> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| CoreError::NotFound("home directory".into()))?;
        let path = PathBuf::from(home).join(".ssh").join("config");
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| CoreError::Store(StoreError::Io(e)))?;
        Ok((path, switchyard_remote::parse_ssh_config(&text)))
    }

    fn default_ssh_user() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "root".into())
    }

    async fn saved_host_names(&self) -> Result<HashMap<String, ProfileId>> {
        self.with_store(|s| {
            Ok(s.profiles()?
                .into_iter()
                .filter_map(|p| match p {
                    Profile::Host(h) => Some((h.name, h.id)),
                    _ => None,
                })
                .collect())
        })
        .await
    }

    async fn preview_ssh_config(
        &self,
    ) -> Result<(PathBuf, Vec<crate::ssh_import::SshImportCandidate>)> {
        let (path, parsed) = self.read_ssh_config().await?;
        let saved: std::collections::HashSet<String> =
            self.saved_host_names().await?.into_keys().collect();
        let hosts = crate::ssh_import::candidates(&parsed, &saved, &Self::default_ssh_user());
        Ok((path, hosts))
    }

    async fn import_ssh_config(&self, only: Option<Vec<String>>) -> Result<usize> {
        let (_, parsed) = self.read_ssh_config().await?;
        let saved = self.saved_host_names().await?;
        let only: Option<std::collections::HashSet<String>> = only.map(|o| o.into_iter().collect());
        let hosts =
            crate::ssh_import::plan(&parsed, &saved, only.as_ref(), &Self::default_ssh_user());
        self.with_store(move |s| {
            for h in &hosts {
                s.save_profile(&Profile::Host(h.clone()))?;
            }
            Ok(hosts.len())
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
            TermTarget::Host(host_id) => match self.ssh_target(&host_id).await {
                Ok(target) => {
                    let description = format!("{}@{}", target.user, target.address);
                    let terminal = self.terminals.open_ssh(
                        SshTerminalSpec {
                            term,
                            host_id,
                            target,
                            size,
                        },
                        self.ssh.clone(),
                        self.events.clone(),
                    );
                    Ok((terminal, description))
                }
                Err(e) => Err(e.to_string()),
            },
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

    /// The SSH target for a Host, with its jump chain and keychain secrets.
    pub(crate) async fn ssh_target(&self, id: &ProfileId) -> Result<SshTarget> {
        let host = self.host(id).await?;
        let mut chain = Vec::new();
        for jid in host.jump_hosts.iter().take(8) {
            if jid == id {
                return Err(CoreError::Unsupported(
                    "a Host cannot jump through itself".into(),
                ));
            }
            chain.push(self.host(jid).await?);
        }
        let mut jump: Option<Box<SshTarget>> = None;
        for h in chain {
            let mut t = self.one_target(&h).await?;
            t.jump = jump.take();
            jump = Some(Box::new(t));
        }
        let mut t = self.one_target(&host).await?;
        t.jump = jump;
        Ok(t)
    }

    async fn test_host(&self, host: Host, secret: Option<SecretString>) -> Result<String> {
        let mut target = self.one_target(&host).await?;
        if secret.is_some() {
            target.secret = secret;
        }
        let mut jump: Option<Box<SshTarget>> = None;
        for jid in host.jump_hosts.iter().take(8) {
            let mut t = self.one_target(&self.host(jid).await?).await?;
            t.jump = jump.take();
            jump = Some(Box::new(t));
        }
        target.jump = jump;
        // A throwaway id: the test never reuses or replaces a live session.
        target.id = format!("test:{}", host.id.0);
        let conn = self
            .ssh
            .session(&target)
            .await
            .map_err(|e| CoreError::Unsupported(e.to_string()))?;
        Ok(format!("Connected · {}", conn.description))
    }

    async fn host(&self, id: &ProfileId) -> Result<Host> {
        let id2 = id.clone();
        match self.with_store(move |s| s.profile(&id2)).await? {
            Some(Profile::Host(h)) => Ok(h),
            _ => Err(CoreError::NotFound(format!("Host {}", id.0))),
        }
    }

    async fn one_target(&self, h: &Host) -> Result<SshTarget> {
        let secret = match h.secret.clone() {
            Some(key) => self.with_secrets(move |s| s.get(&key)).await?,
            None => None,
        };
        Ok(SshTarget {
            id: h.id.0.clone(),
            label: h.name.clone(),
            address: h.address.clone(),
            port: h.port,
            user: h.user.clone(),
            auth: match &h.auth {
                SshAuth::Password => SshAuthMethod::Password,
                SshAuth::PublicKey { key_path } => SshAuthMethod::PublicKey {
                    key_path: key_path.clone(),
                },
                SshAuth::KeyboardInteractive => SshAuthMethod::KeyboardInteractive,
                SshAuth::Agent => SshAuthMethod::Agent,
            },
            secret,
            keepalive: Duration::from_secs(u64::from(h.keepalive_secs)),
            jump: None,
            agent_socket: h.identity_agent.clone(),
            agent_key: h.agent_key.clone(),
        })
    }

    fn tunnel_infos(&self) -> Vec<TunnelInfo> {
        let mut list = lock(&self.tunnels);
        list.retain(|w| w.strong_count() > 0);
        list.iter()
            .filter_map(Weak::upgrade)
            .filter(|t| !t.is_stopped())
            .map(|t| t.info())
            .collect()
    }

    /// The live tunnel to `host:port` through Host `host_id`, or a new one. Tunnels are
    /// shared by every session that needs the same target.
    async fn tunnel_for(&self, host_id: &ProfileId, host: &str, port: u16) -> Result<Arc<Tunnel>> {
        let _creating = self.tunnel_open.lock().await;
        let existing = lock(&self.tunnels)
            .iter()
            .filter_map(Weak::upgrade)
            .find(|t| !t.is_stopped() && t.host_id() == host_id.0 && t.remote() == (host, port));
        if let Some(t) = existing {
            return Ok(t);
        }
        let target = self.ssh_target(host_id).await?;
        let id = self
            .next_tunnel
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = Arc::new(
            Tunnel::open(id, self.ssh.clone(), target, host.to_owned(), port)
                .await
                .map_err(|e| CoreError::Unsupported(e.to_string()))?,
        );
        lock(&self.tunnels).push(Arc::downgrade(&t));
        self.emit(Event::Tunnels(self.tunnel_infos()));
        Ok(t)
    }

    /// The endpoint a connection should use: a tunnel when it goes through a Host.
    async fn endpoint(&self, c: &DbConnection) -> Result<Option<Arc<Tunnel>>> {
        match &c.via_host {
            Some(h) => Ok(Some(self.tunnel_for(h, &c.server, c.port).await?)),
            None => Ok(None),
        }
    }

    /// Stop a tunnel; sessions that used it end with a message saying why.
    fn stop_tunnel(&self, id: u64) {
        let tunnel = lock(&self.tunnels)
            .iter()
            .filter_map(Weak::upgrade)
            .find(|t| t.id() == id);
        let Some(t) = tunnel else { return };
        t.stop();
        let message = format!(
            "The tunnel through {} on port {} was stopped · reconnect to continue",
            t.info().host,
            t.local().port()
        );
        let ended: Vec<SessionId> = lock(&self.sessions)
            .iter()
            .filter(|(_, slot)| slot.tunnel.as_ref().is_some_and(|s| s.id() == id))
            .map(|(id, _)| *id)
            .collect();
        for session in ended {
            lock(&self.sessions).remove(&session);
            self.emit(Event::SessionFailed {
                session,
                message: message.clone(),
            });
        }
        self.emit(Event::Tunnels(self.tunnel_infos()));
    }

    async fn db_config(&self, c: &DbConnection, secret: Option<SecretString>) -> Result<DbConfig> {
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
        if c.auth.is_entra() {
            cfg.access_token = Some(self.entra.token(c, cfg.password.clone()).await?);
        }
        // Windows signs in with SSPI inside the driver; elsewhere Kerberos needs GSSAPI.
        if c.auth == DbAuthMethod::Integrated && c.engine == Engine::SqlServer && !cfg!(windows) {
            let k = crate::components::kerberos(&self.components).await?;
            cfg.security = Some(Arc::new(k));
        }
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
        let tunnel = self.endpoint(&c).await?;
        let started = Instant::now();
        let session = driver
            .connect(&cfg, tunnel.as_deref().map(endpoint_of))
            .await?;
        let ms = started.elapsed().as_millis();
        let via = if tunnel.is_some() { " via tunnel" } else { "" };
        Ok(format!(
            "Connected · {} · {ms} ms{via}",
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
        let tunnel = self.endpoint(&conn).await?;
        let s = driver
            .connect(&cfg, tunnel.as_deref().map(endpoint_of))
            .await?;
        let version = s.server_version();
        lock(&self.sessions).insert(
            session,
            Arc::new(SessionSlot {
                connection: conn,
                inner: tokio::sync::Mutex::new(SessionInner {
                    session: s,
                    txn_statements: 0,
                }),
                tunnel,
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
