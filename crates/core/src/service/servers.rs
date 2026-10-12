//! Workspaces, workspace files, reachability checks, shared credentials and remote
//! desktop launches on the runtime ([`crate::servers`]).

use std::collections::HashMap;
use std::time::Duration;

use secrecy::SecretString;
use switchyard_store::workspace_file::{ImportChoice, SecretStatus, WorkspaceFile};
use switchyard_store::{
    Credential, DesktopServer, HistoryEntry, HistoryStatus, Profile, ProfileId, SecretRef,
    StoreError, now_ms,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{Service, lock};
use crate::bus::{Event, RequestId};
use crate::error::{CoreError, Result};
use crate::servers::launch::{self, PrivateDir};
use crate::servers::rdp::{self, Client, Endpoint, GatewayLogin, Login};
use crate::servers::reach::{self, ReachTarget};
use crate::servers::{
    ImportPreviewInfo, LaunchState, MissingSecret, REACH_CONCURRENCY, REACH_INTERVAL_SECS,
    REACH_TIMEOUT_MS, ServersCommand, ServersEvent,
};

/// The running reachability loop and the launched clients, to stop.
#[derive(Default)]
pub(crate) struct ServersState {
    reach: std::sync::Mutex<Option<CancellationToken>>,
    launches: std::sync::Mutex<HashMap<RequestId, CancellationToken>>,
}

impl Service {
    fn servers_emit(&self, e: ServersEvent) {
        self.emit(Event::Servers(e));
    }

    pub(super) async fn servers(&self, cmd: ServersCommand) {
        match cmd {
            ServersCommand::Load => {
                self.emit_workspaces().await;
                self.emit_facts().await;
            }
            ServersCommand::Switch { id, load } => {
                let r = self
                    .with_store(move |s| {
                        s.set_current_workspace(&id)?;
                        if load {
                            s.workspace().map(Some)
                        } else {
                            Ok(None)
                        }
                    })
                    .await;
                match r {
                    Ok(Some(w)) => self.emit(Event::Workspace(w)),
                    Ok(None) => {}
                    Err(e) => self.error("Workspace", e),
                }
                self.emit_workspaces().await;
            }
            ServersCommand::Create {
                request,
                name,
                scope,
            } => {
                match self
                    .with_store(move |s| s.create_workspace(&name, scope))
                    .await
                {
                    Ok(workspace) => self.servers_emit(ServersEvent::Created {
                        request,
                        workspace: Box::new(workspace),
                    }),
                    Err(e) => self.error("New workspace", e),
                }
                self.emit_workspaces().await;
            }
            ServersCommand::Rename { id, name } => {
                if let Err(e) = self
                    .with_store(move |s| s.rename_workspace(&id, &name))
                    .await
                {
                    self.error("Rename workspace", e);
                }
                self.emit_workspaces().await;
            }
            ServersCommand::Duplicate { request, id, name } => {
                match self
                    .with_store(move |s| s.duplicate_workspace(&id, &name))
                    .await
                {
                    Ok(workspace) => self.servers_emit(ServersEvent::Created {
                        request,
                        workspace: Box::new(workspace),
                    }),
                    Err(e) => self.error("Duplicate workspace", e),
                }
                self.emit_workspaces().await;
            }
            ServersCommand::SetScope { id, scope } => {
                let r = self
                    .with_store(move |s| {
                        let mut w = s
                            .workspace_by_id(&id)?
                            .ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))?;
                        w.scope = scope;
                        s.save_workspace(&w)
                    })
                    .await;
                if let Err(e) = r {
                    self.error("Workspace", e);
                }
                self.emit_workspaces().await;
            }
            ServersCommand::PreviewDelete { id } => {
                let i = id.clone();
                match self.with_store(move |s| s.exclusive_profiles(&i)).await {
                    Ok(list) => self.servers_emit(ServersEvent::DeletePreview {
                        id,
                        exclusive: list
                            .iter()
                            .map(|p| (p.id().clone(), p.name().to_owned(), p.kind().to_owned()))
                            .collect(),
                    }),
                    Err(e) => self.error("Delete workspace", e),
                }
            }
            ServersCommand::Delete {
                id,
                delete_profiles,
            } => {
                let i = id.clone();
                match self
                    .with_store(move |s| s.delete_workspace(&i, delete_profiles))
                    .await
                {
                    Ok(d) => {
                        for p in &d.deleted {
                            self.forget_profile_secret(p).await;
                        }
                        self.servers_emit(ServersEvent::Deleted {
                            id,
                            deleted: d.deleted.len(),
                            kept: d.kept,
                        });
                        if !d.deleted.is_empty() {
                            self.emit_profiles().await;
                        }
                    }
                    Err(e) => self.error("Delete workspace", e),
                }
                self.emit_workspaces().await;
            }
            ServersCommand::Add { id, profiles } => {
                if let Err(e) = self
                    .with_store(move |s| s.add_to_workspace(&id, &profiles))
                    .await
                {
                    self.error("Add to workspace", e);
                }
                self.emit_workspaces().await;
            }
            ServersCommand::Remove { id, profiles } => {
                if let Err(e) = self
                    .with_store(move |s| s.remove_from_workspace(&id, &profiles))
                    .await
                {
                    self.error("Remove from workspace", e);
                }
                self.emit_workspaces().await;
            }
            ServersCommand::Export {
                id,
                path,
                passphrase,
            } => match self.export_workspace(id, path, passphrase).await {
                Ok(msg) => self.emit(Event::Toast(msg)),
                Err(e) => self.error("Export workspace", e),
            },
            ServersCommand::PreviewImport { path } => {
                let p = path.clone();
                let result = self
                    .with_store(move |s| {
                        let json = std::fs::read_to_string(&p)?;
                        let file = WorkspaceFile::parse(&json)?;
                        let pv = s.preview_workspace_import(&file)?;
                        Ok(ImportPreviewInfo {
                            workspace: pv.workspace,
                            workspace_exists: pv.workspace_exists,
                            items: pv.items,
                            encrypted: pv.encrypted,
                        })
                    })
                    .await
                    .map_err(|e| e.to_string());
                self.servers_emit(ServersEvent::ImportPreview { path, result });
            }
            ServersCommand::Import {
                path,
                passphrase,
                choices,
            } => self.import_workspace(path, passphrase, choices).await,
            ServersCommand::StartReachability { ids } => self.start_reach(ids).await,
            ServersCommand::StopReachability => {
                if let Some(c) = lock(&self.servers_state.reach).take() {
                    c.cancel();
                }
            }
            ServersCommand::Launch { request, id } => self.launch_desktop(request, id).await,
            ServersCommand::CancelLaunch { request } => {
                if let Some(c) = lock(&self.servers_state.launches).remove(&request) {
                    c.cancel();
                }
            }
            ServersCommand::CredentialUsers { id } => {
                let i = id.clone();
                match self.with_store(move |s| credential_users(s, &i)).await {
                    Ok(users) => self.servers_emit(ServersEvent::CredentialUsers { id, users }),
                    Err(e) => self.error("Credential", e),
                }
            }
            ServersCommand::DeleteCredential { id, detach } => {
                match self.delete_credential(id, detach).await {
                    Ok(name) => self.emit(Event::Toast(format!("Deleted credential {name}"))),
                    Err(e) => self.error("Delete credential", e),
                }
                self.emit_profiles().await;
            }
        }
    }

    pub(super) async fn emit_workspaces(&self) {
        match self
            .with_store(|s| Ok((s.workspaces()?, s.current_workspace_id()?)))
            .await
        {
            Ok((list, current)) => self.servers_emit(ServersEvent::Workspaces { list, current }),
            Err(e) => self.error("Workspaces", e),
        }
    }

    pub(super) async fn emit_facts(&self) {
        match self.with_store(|s| s.server_facts()).await {
            Ok(f) => self.servers_emit(ServersEvent::Facts(f)),
            Err(e) => warn!(error = %e, "server facts not loaded"),
        }
    }

    /// Remember a connection to `id` (Servers view: last connected, recents).
    pub(super) async fn touch_connected(&self, id: ProfileId) {
        if let Err(e) = self.with_store(move |s| s.touch_connected(&id)).await {
            warn!(error = %e, "last-connected not saved");
        }
        self.emit_facts().await;
    }

    /// Keep the OS a Fleet probe found, for the Servers view.
    pub(super) async fn remember_os(&self, id: ProfileId, os: String) {
        if let Err(e) = self.with_store(move |s| s.set_server_os(&id, &os)).await {
            warn!(error = %e, "server os not saved");
        }
        self.emit_facts().await;
    }

    /// Remove a deleted profile's stored secret (named secrets are references: kept).
    async fn forget_profile_secret(&self, p: &Profile) {
        if let Some(key) = p.secret().cloned()
            && key.named_secret().is_none()
            && let Err(e) = self.with_secrets(move |s| s.delete(&key)).await
        {
            warn!(error = %e, "could not remove a deleted profile's secret");
        }
    }

    async fn export_workspace(
        &self,
        id: String,
        path: std::path::PathBuf,
        passphrase: Option<SecretString>,
    ) -> Result<String> {
        let mut file = self.with_store(move |s| s.export_workspace(&id)).await?;
        let mut included = 0;
        if let Some(pass) = passphrase {
            let mut values = Vec::new();
            for e in &file.secrets.entries {
                let key = SecretRef::for_profile(&e.profile, &e.purpose);
                if let Some(v) = self.with_secrets(move |s| s.get(&key)).await? {
                    values.push((e.profile.clone(), v));
                }
            }
            included = values.len();
            let f2 = tokio::task::spawn_blocking(move || {
                file.seal_secrets(&pass, &values).map(|()| file)
            })
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))??;
            file = f2;
        }
        let missing = file
            .secrets
            .entries
            .iter()
            .filter(|e| e.status == SecretStatus::NeedsValue)
            .count();
        let json = file.to_json()?;
        let p = path.clone();
        tokio::task::spawn_blocking(move || std::fs::write(&p, json))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .map_err(StoreError::from)?;
        info!(
            profiles = file.profiles.len(),
            included, "workspace exported"
        );
        Ok(if included > 0 {
            format!(
                "Exported {} · {included} secret{} encrypted",
                path.display(),
                if included == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "Exported {} · no secrets ({missing} need a value on import)",
                path.display()
            )
        })
    }

    async fn import_workspace(
        &self,
        path: std::path::PathBuf,
        passphrase: Option<SecretString>,
        choices: Vec<(ProfileId, ImportChoice)>,
    ) {
        let p = path.clone();
        let file = match self
            .with_store(move |_| {
                let json = std::fs::read_to_string(&p)?;
                WorkspaceFile::parse(&json)
            })
            .await
        {
            Ok(f) => f,
            Err(e) => {
                return self.servers_emit(ServersEvent::ImportFailed {
                    path,
                    message: e.to_string(),
                    wrong_passphrase: false,
                });
            }
        };
        // Decrypt first: a wrong passphrase imports nothing.
        let values = match passphrase {
            Some(pass) if file.secrets.encrypted => {
                let f = file.clone();
                match tokio::task::spawn_blocking(move || f.open_secrets(&pass)).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        return self.servers_emit(ServersEvent::ImportFailed {
                            path,
                            message: e.to_string(),
                            wrong_passphrase: matches!(e, StoreError::WrongPassphrase),
                        });
                    }
                    Err(e) => return self.error("Import workspace", e),
                }
            }
            _ => Vec::new(),
        };
        let choices: HashMap<ProfileId, ImportChoice> = choices.into_iter().collect();
        let f = file.clone();
        let outcome = match self
            .with_store(move |s| s.apply_workspace_import(&f, &choices))
            .await
        {
            Ok(o) => o,
            Err(e) => {
                return self.servers_emit(ServersEvent::ImportFailed {
                    path,
                    message: e.to_string(),
                    wrong_passphrase: false,
                });
            }
        };
        // Store the decrypted values under the imported ids.
        let mut stored: Vec<ProfileId> = Vec::new();
        for (old, value) in values {
            let Some(new) = outcome.id_map.get(&old).cloned() else {
                continue;
            };
            let purpose = file
                .secrets
                .entries
                .iter()
                .find(|e| e.profile == old)
                .map_or_else(|| "password".to_owned(), |e| e.purpose.clone());
            let key = SecretRef::for_profile(&new, &purpose);
            let k = key.clone();
            if let Err(e) = self.with_secrets(move |s| s.set(&k, &value)).await {
                self.error("Import workspace", e);
                continue;
            }
            let n = new.clone();
            let r = self
                .with_store(move |s| {
                    if let Some(mut p) = s.profile(&n)? {
                        p.set_secret(key);
                        s.save_profile(&p)?;
                    }
                    Ok(())
                })
                .await;
            if let Err(e) = r {
                self.error("Import workspace", e);
            } else {
                stored.push(new);
            }
        }
        // What still needs a value: listed in the file, not stored here.
        let mut missing = Vec::new();
        for e in &file.secrets.entries {
            let Some(new) = outcome.id_map.get(&e.profile).cloned() else {
                continue;
            };
            if stored.contains(&new) {
                continue;
            }
            let n = new.clone();
            if let Ok(Some(p)) = self.with_store(move |s| s.profile(&n)).await
                && p.secret().is_none()
            {
                missing.push(MissingSecret {
                    id: new,
                    name: p.name().to_owned(),
                    purpose: e.purpose.clone(),
                });
            }
        }
        let summary = format!(
            "Imported {}: {} added, {} updated, {} skipped",
            file.workspace.name, outcome.added, outcome.updated, outcome.skipped
        );
        // The UI switches to the workspace and asks for missing secrets: send what it
        // needs first.
        self.emit_profiles().await;
        self.emit_workspaces().await;
        self.servers_emit(ServersEvent::Imported {
            workspace: outcome.workspace_id,
            missing,
            summary,
        });
    }

    async fn start_reach(&self, ids: Vec<ProfileId>) {
        let profiles = match self.with_store(|s| s.profiles()).await {
            Ok(p) => p,
            Err(e) => return self.error("Reachability", e),
        };
        let targets: Vec<ReachTarget> = ids
            .iter()
            .filter_map(|id| profiles.iter().find(|p| p.id() == id))
            .filter_map(|p| match p {
                Profile::Host(h) => Some(ReachTarget {
                    id: h.id.clone(),
                    address: h.jump_hosts.is_empty().then(|| h.address.trim().to_owned()),
                    port: h.port,
                }),
                Profile::Desktop(d) => Some(ReachTarget {
                    id: d.id.clone(),
                    address: (d.via_host.is_none() && d.gateway.is_none())
                        .then(|| d.address.trim().to_owned()),
                    port: d.port,
                }),
                _ => None,
            })
            .collect();
        let cancel = CancellationToken::new();
        if let Some(old) = lock(&self.servers_state.reach).replace(cancel.clone()) {
            old.cancel();
        }
        let events = self.events.clone();
        tokio::spawn(reach::run(
            targets,
            Duration::from_secs(REACH_INTERVAL_SECS),
            REACH_CONCURRENCY,
            Duration::from_millis(REACH_TIMEOUT_MS),
            cancel,
            move |id, status| events.emit(Event::Servers(ServersEvent::Reach { id, status })),
        ));
    }

    async fn credential(&self, id: &ProfileId) -> Result<Credential> {
        let i = id.clone();
        match self.with_store(move |s| s.profile(&i)).await? {
            Some(Profile::Credential(c)) => Ok(c),
            _ => Err(CoreError::NotFound(format!("credential {}", id.0))),
        }
    }

    /// The sign-in of a Host, connection or desktop that uses a credential: the
    /// credential (rotating it changes every user at their next connect).
    pub(super) async fn resolve_host_credential(
        &self,
        h: switchyard_store::Host,
    ) -> Result<switchyard_store::Host> {
        match &h.credential {
            Some(c) => Ok(h.with_credential(&self.credential(c).await?)),
            None => Ok(h),
        }
    }

    /// See [`Service::resolve_host_credential`].
    pub(super) async fn resolve_db_credential(
        &self,
        d: switchyard_store::DbConnection,
    ) -> Result<switchyard_store::DbConnection> {
        match &d.credential {
            Some(c) => Ok(d.with_credential(&self.credential(c).await?)),
            None => Ok(d),
        }
    }

    pub(super) async fn desktop_login(
        &self,
        d: &DesktopServer,
    ) -> Result<(Login, Option<GatewayLogin>)> {
        let login = match &d.credential {
            Some(c) => {
                let cred = self.credential(c).await?;
                Login {
                    user: cred.username.trim().to_owned(),
                    domain: cred.domain().map(str::to_owned),
                    password: self.secret_value(None, cred.secret.clone()).await?,
                }
            }
            None => Login {
                user: d.user.trim().to_owned(),
                domain: d.domain.clone(),
                password: self.secret_value(None, d.secret.clone()).await?,
            },
        };
        let gateway = match &d.gateway {
            Some(g) => Some(GatewayLogin {
                host: g.host.trim().to_owned(),
                login: match &g.credential {
                    Some(c) => {
                        let cred = self.credential(c).await?;
                        Some(Login {
                            user: cred.username.trim().to_owned(),
                            domain: cred.domain().map(str::to_owned),
                            password: self.secret_value(None, cred.secret.clone()).await?,
                        })
                    }
                    None if !g.user.trim().is_empty() => Some(Login {
                        user: g.user.trim().to_owned(),
                        domain: None,
                        password: None,
                    }),
                    None => None,
                },
            }),
            None => None,
        };
        Ok((login, gateway))
    }

    async fn launch_desktop(&self, request: RequestId, id: ProfileId) {
        let fail = |message: String, install: Option<String>| {
            self.servers_emit(ServersEvent::Launch {
                request,
                id: id.clone(),
                state: LaunchState::Failed { message, install },
            })
        };
        let i = id.clone();
        let d = match self.with_store(move |s| s.profile(&i)).await {
            Ok(Some(Profile::Desktop(d))) => d,
            Ok(_) => return fail("This remote desktop no longer exists".into(), None),
            Err(e) => return fail(e.to_string(), None),
        };
        let component = match d.protocol {
            switchyard_store::DesktopProtocol::Rdp => rdp::RDP_COMPONENT,
            switchyard_store::DesktopProtocol::Vnc => rdp::VNC_COMPONENT,
        };
        let reg = self.components.clone();
        let comp = match tokio::task::spawn_blocking(move || reg.component(component)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return fail(e.to_string(), None),
            Err(e) => return fail(e.to_string(), None),
        };
        let (location, version) = match &comp.status {
            switchyard_drivers::ComponentStatus::Installed {
                location, version, ..
            } => (std::path::PathBuf::from(location), version.clone()),
            _ => {
                return fail(
                    format!(
                        "No {} client found. Install one from Settings → Drivers ({}).",
                        d.protocol.badge(),
                        comp.name
                    ),
                    Some(component.to_owned()),
                );
            }
        };
        let Some(client) =
            Client::from_location(&location, version.as_deref()).filter(|c| c.speaks(d.protocol))
        else {
            return fail(
                format!(
                    "{} is not a {} client Switchyard knows how to start",
                    location.display(),
                    d.protocol.badge()
                ),
                Some(component.to_owned()),
            );
        };
        let (login, gateway) = match self.desktop_login(&d).await {
            Ok(l) => l,
            Err(e) => return fail(e.to_string(), None),
        };
        // Through an SSH tunnel: the client connects to 127.0.0.1:<local port>.
        let tunnel = match &d.via_host {
            Some(h) => match self.tunnel_for(h, d.address.trim(), d.port).await {
                Ok(t) => Some(t),
                Err(e) => return fail(format!("SSH tunnel: {e}"), None),
            },
            None => None,
        };
        let at = match &tunnel {
            Some(t) => Endpoint {
                host: t.local().ip().to_string(),
                port: t.local().port(),
            },
            None => Endpoint {
                host: d.address.trim().to_owned(),
                port: d.port,
            },
        };
        let dir = match PrivateDir::create() {
            Ok(dir) => dir,
            Err(e) => return fail(format!("could not prepare the session files: {e}"), None),
        };
        let plan = rdp::plan(
            &d,
            client,
            &location,
            &at,
            &login,
            gateway.as_ref(),
            dir.path(),
        );
        let cancel = CancellationToken::new();
        lock(&self.servers_state.launches).insert(request, cancel.clone());
        // History, tagged `rdp` / `vnc`: who connected where, never the password.
        let entry = HistoryEntry {
            id: 0,
            connection_id: Some(d.id.clone()),
            connection_name: d.name.clone(),
            sql: format!(
                "{} {} {}{}",
                d.protocol.badge(),
                client.name(),
                d.address.trim(),
                d.via_host
                    .as_ref()
                    .map_or(String::new(), |_| " (SSH tunnel)".into())
            ),
            started_at: now_ms(),
            duration_ms: 0,
            rows: None,
            affected: None,
            status: HistoryStatus::Ok,
            error: None,
            tags: vec![d.protocol.history_tag().to_owned()],
            has_plan: false,
        };
        let events = self.events.clone();
        let started_id = d.id.clone();
        let name = client.name().to_owned();
        let result = launch::run(plan, dir, cancel, move || {
            events.emit(Event::Servers(ServersEvent::Launch {
                request,
                id: started_id,
                state: LaunchState::Started(name),
            }));
        })
        .await;
        lock(&self.servers_state.launches).remove(&request);
        let mut entry = entry;
        entry.duration_ms = now_ms() - entry.started_at;
        if let Err(e) = &result {
            entry.status = HistoryStatus::Error;
            entry.error = Some(e.to_string());
        }
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "history write failed");
        }
        drop(tunnel);
        match result {
            Ok(()) => {
                self.touch_connected(d.id.clone()).await;
                self.servers_emit(ServersEvent::Launch {
                    request,
                    id: d.id,
                    state: LaunchState::Ended,
                });
            }
            Err(e) => fail(e.to_string(), None),
        }
        self.emit(Event::Tunnels(self.tunnel_infos()));
    }

    async fn delete_credential(&self, id: ProfileId, detach: bool) -> Result<String> {
        let i = id.clone();
        let removed = self
            .with_store(move |s| {
                let users = credential_users(s, &i)?;
                if !users.is_empty() && !detach {
                    let names: Vec<String> = users.into_iter().map(|(_, n)| n).collect();
                    return Err(StoreError::InUse(i.0.clone(), names.join(", ")));
                }
                let username = match s.profile(&i)? {
                    Some(Profile::Credential(c)) => c.username.trim().to_owned(),
                    _ => String::new(),
                };
                for (uid, _) in &users {
                    if let Some(mut p) = s.profile(uid)? {
                        p.detach_credential(&i);
                        // Keep the user name it signed in with; the secret is typed anew.
                        match &mut p {
                            Profile::Host(h) if h.user.trim().is_empty() => {
                                h.user.clone_from(&username)
                            }
                            Profile::Db(d) if d.user.trim().is_empty() => {
                                d.user.clone_from(&username)
                            }
                            Profile::Desktop(d) if d.user.trim().is_empty() => {
                                d.user.clone_from(&username)
                            }
                            _ => {}
                        }
                        s.save_profile(&p)?;
                    }
                }
                s.delete_profile(&i)
            })
            .await?;
        match removed {
            Some(p) => {
                self.forget_profile_secret(&p).await;
                Ok(p.name().to_owned())
            }
            None => Err(CoreError::NotFound(format!("credential {}", id.0))),
        }
    }
}

/// The profiles using credential `id`: (id, name).
pub(crate) fn credential_users(
    s: &switchyard_store::Store,
    id: &ProfileId,
) -> std::result::Result<Vec<(ProfileId, String)>, StoreError> {
    Ok(s.profiles()?
        .into_iter()
        .filter(|p| p.uses_credential(id))
        .map(|p| (p.id().clone(), p.name().to_owned()))
        .collect())
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;
    use switchyard_db::Engine;
    use switchyard_store::{DbConnection, Host};

    use super::*;
    use crate::runtime::EventSender;
    use crate::service::ServiceConfig;

    fn service() -> Service {
        let (tx, _rx) = futures::channel::mpsc::unbounded();
        Service::new(ServiceConfig::in_memory(), EventSender::new(tx)).unwrap()
    }

    async fn stored_credential(svc: &Service, id: &ProfileId) -> Profile {
        let i = id.clone();
        svc.with_store(move |s| s.profile(&i))
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn references_resolve_and_rotation_reaches_every_user() {
        let svc = service();
        let cred = Credential::new("ops", "ops");
        svc.save_profile(1, Profile::Credential(cred.clone()), Some("v1".into()))
            .await;
        let mut hosts = Vec::new();
        for name in ["web-1", "web-2"] {
            let mut h = Host::new(name, "10.0.0.1", "");
            h.credential = Some(cred.id.clone());
            svc.save_profile(2, Profile::Host(h.clone()), None).await;
            hosts.push(h.id);
        }
        for id in &hosts {
            let t = svc.ssh_target(id).await.unwrap();
            assert_eq!(t.user, "ops");
            assert_eq!(t.secret.unwrap().expose_secret(), "v1");
        }

        // Rotate: one save of the credential.
        let saved = stored_credential(&svc, &cred.id).await;
        svc.save_profile(3, saved, Some("v2".into())).await;
        for id in &hosts {
            let t = svc.ssh_target(id).await.unwrap();
            assert_eq!(t.secret.unwrap().expose_secret(), "v2", "rotated");
        }
        let mut d = DbConnection::new("orders", Engine::Postgres);
        d.credential = Some(cred.id.clone());
        let cfg = svc.db_config(&d, None).await.unwrap();
        assert_eq!(cfg.user, "ops");
        assert_eq!(cfg.password.unwrap().expose_secret(), "v2");

        // In use: deleting asks for detach first.
        let users = svc
            .with_store({
                let id = cred.id.clone();
                move |s| credential_users(s, &id)
            })
            .await
            .unwrap();
        assert_eq!(users.len(), 2);
        assert!(svc.delete_credential(cred.id.clone(), false).await.is_err());
        svc.delete_credential(cred.id.clone(), true).await.unwrap();
        let h = svc.host(&hosts[0]).await.unwrap();
        assert!(h.credential.is_none());
        assert_eq!(h.user, "ops", "detached Hosts keep the user name");
    }
}
