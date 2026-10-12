//! Workspaces of the Default side (servers and databases). Each one is named, has a
//! scope (explicit members plus tag and folder rules, minus explicit exclusions) and its
//! own layout; SQL buffers belong to one workspace. "All" ([`ALL_ID`]) is built in, shows
//! everything, cannot be deleted, and is where the single pre-workspace layout went.
//!
//! Removing an item from a workspace never deletes it: it stays in "All". Deleting a
//! workspace removes its scope, layout and buffers; only when asked does it also delete
//! the profiles that are in no other workspace ([`Store::exclusive_profiles`]).

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::model::{Profile, ProfileId, Workspace};
use crate::store::{Store, now_ms};

/// Id of the built-in workspace that shows everything.
pub const ALL_ID: &str = "all";
/// Name of the built-in workspace.
pub const ALL_NAME: &str = "All";
/// Setting holding the current workspace's id.
pub const CURRENT_KEY: &str = "workspace.current";
/// Longest workspace name, in characters.
pub const MAX_NAME_LEN: usize = 80;

/// [`ALL_ID`] as a `String` (serde default).
pub fn all_id() -> String {
    ALL_ID.to_owned()
}

/// What a workspace shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceScope {
    /// Everything ("All").
    #[serde(default)]
    pub all: bool,
    /// Profiles added by hand.
    #[serde(default)]
    pub members: Vec<ProfileId>,
    /// Profiles carrying any of these tags (normalized).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Profiles in any of these folders (or a sub-folder `name/…`).
    #[serde(default)]
    pub folders: Vec<String>,
    /// Profiles removed by hand although a rule matches them.
    #[serde(default)]
    pub excluded: Vec<ProfileId>,
}

impl WorkspaceScope {
    /// The scope of "All".
    pub fn everything() -> Self {
        Self {
            all: true,
            ..Self::default()
        }
    }

    /// Whether a tag or folder rule matches `p`.
    fn rule_matches(&self, p: &Profile) -> bool {
        let tag_hit = !self.tags.is_empty()
            && p.tags()
                .iter()
                .filter_map(|t| crate::tags::normalize_tag(t))
                .any(|t| {
                    self.tags
                        .iter()
                        .any(|w| crate::tags::normalize_tag(w) == Some(t.clone()))
                });
        let folder_hit = p.folder().is_some_and(|f| {
            self.folders.iter().map(|r| r.trim()).any(|r| {
                !r.is_empty()
                    && (f.eq_ignore_ascii_case(r)
                        || f.len() > r.len()
                            && f[..r.len()].eq_ignore_ascii_case(r)
                            && f[r.len()..].starts_with('/'))
            })
        });
        tag_hit || folder_hit
    }

    /// Whether `p` itself is in the workspace (not counting what its members need).
    pub fn matches(&self, p: &Profile) -> bool {
        if self.all {
            return true;
        }
        if self.excluded.contains(p.id()) {
            return false;
        }
        self.members.contains(p.id()) || self.rule_matches(p)
    }

    /// Ids shown in the workspace: the matches plus what they reference (jump Hosts,
    /// tunnel Hosts, SFTP Hosts, credentials), so a member always works.
    pub fn visible(&self, all: &[Profile]) -> HashSet<ProfileId> {
        let mut out: HashSet<ProfileId> = all
            .iter()
            .filter(|p| self.matches(p))
            .map(|p| p.id().clone())
            .collect();
        if self.all {
            return out;
        }
        let by_id: HashMap<&ProfileId, &Profile> = all.iter().map(|p| (p.id(), p)).collect();
        let mut queue: Vec<ProfileId> = out.iter().cloned().collect();
        while let Some(id) = queue.pop() {
            if let Some(p) = by_id.get(&id) {
                for r in p.references() {
                    if out.insert(r.clone()) {
                        queue.push(r.clone());
                    }
                }
            }
        }
        out
    }

    /// Add `p` (undoing an exclusion; a rule may already cover it).
    pub fn add(&mut self, p: &Profile) {
        let id = p.id();
        self.excluded.retain(|x| x != id);
        if !self.all && !self.members.contains(id) && !self.rule_matches(p) {
            self.members.push(id.clone());
        }
    }

    /// Remove `p` without deleting it: drop the membership, and exclude it when a tag or
    /// folder rule would still match.
    pub fn remove(&mut self, p: &Profile) {
        let id = p.id();
        self.members.retain(|x| x != id);
        if self.rule_matches(p) && !self.excluded.contains(id) {
            self.excluded.push(id.clone());
        }
    }

    /// Whether it shows nothing by itself.
    pub fn is_empty(&self) -> bool {
        !self.all && self.members.is_empty() && self.tags.is_empty() && self.folders.is_empty()
    }

    /// Normalize rules (tags lower case, folders trimmed, no duplicates).
    pub fn normalized(mut self) -> Self {
        self.tags = crate::tags::normalize_tags(&self.tags);
        let mut folders: Vec<String> = Vec::new();
        for f in &self.folders {
            let f = f.trim().trim_end_matches('/').to_owned();
            if !f.is_empty() && !folders.contains(&f) {
                folders.push(f);
            }
        }
        self.folders = folders;
        let mut seen = HashSet::new();
        self.members.retain(|m| seen.insert(m.clone()));
        let mut seen = HashSet::new();
        self.excluded.retain(|m| seen.insert(m.clone()));
        self
    }
}

/// A non-SQL tab a workspace reopens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SavedTab {
    /// A terminal (`None`: the local shell).
    Terminal {
        /// Host.
        #[serde(default)]
        host: Option<ProfileId>,
    },
    /// The file browser (`None`: local files).
    Files {
        /// Host.
        #[serde(default)]
        host: Option<ProfileId>,
    },
    /// The Servers view.
    Servers,
    /// Any other saved profile (Redis, cloud tools, containers).
    Profile {
        /// Profile.
        id: ProfileId,
    },
}

/// The profiles a workspace deletion would remove when asked to, and the workspace.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkspaceDeletion {
    /// Profiles deleted (their secrets are the caller's to remove).
    pub deleted: Vec<Profile>,
    /// Profiles that could not be deleted because something outside still uses them.
    pub kept: Vec<String>,
}

fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(StoreError::Invalid("A workspace needs a name".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(StoreError::Invalid("That name is too long".into()));
    }
    Ok(name.to_owned())
}

impl Store {
    /// Create "All" from the single pre-workspace layout (the `workspace` setting) when
    /// it does not exist yet.
    pub(crate) fn ensure_all_workspace(&mut self) -> Result<()> {
        let exists: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM workspaces WHERE id = ?1", [ALL_ID], |r| {
                r.get(0)
            })
            .optional()?;
        if exists.is_some() {
            return Ok(());
        }
        let legacy: Workspace = self.setting("workspace")?.unwrap_or_default();
        let all = Workspace {
            id: ALL_ID.into(),
            name: ALL_NAME.into(),
            scope: WorkspaceScope::everything(),
            pinned: legacy.pinned,
            sidebar_collapsed: legacy.sidebar_collapsed,
            active_buffer: legacy.active_buffer,
            ..Workspace::default()
        };
        self.write_workspace_row(&all, Some(0))
    }

    fn write_workspace_row(&mut self, w: &Workspace, order: Option<i64>) -> Result<()> {
        let mut layout = w.clone();
        layout.buffers.clear();
        let data = serde_json::to_string(&layout)?;
        let order = match order {
            Some(o) => o,
            None => self.conn.query_row(
                "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM workspaces",
                [],
                |r| r.get(0),
            )?,
        };
        self.conn.execute(
            "INSERT INTO workspaces (id, name, sort_order, data, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = ?2, data = ?4, updated_at = ?5",
            params![w.id, w.name, order, data, now_ms()],
        )?;
        Ok(())
    }

    /// Every workspace ("All" first, then in creation order), without buffers.
    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM workspaces ORDER BY sort_order, name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for data in rows {
            out.push(serde_json::from_str(&data?)?);
        }
        Ok(out)
    }

    /// One workspace with its buffers.
    pub fn workspace_by_id(&self, id: &str) -> Result<Option<Workspace>> {
        let data: Option<String> = self
            .conn
            .query_row("SELECT data FROM workspaces WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        let mut w: Workspace = serde_json::from_str(&data)?;
        w.id = id.to_owned();
        w.buffers = self.buffers_of(id)?;
        Ok(Some(w))
    }

    /// The current workspace's id ("All" when unset or gone).
    pub fn current_workspace_id(&self) -> Result<String> {
        let id: Option<String> = self.setting(CURRENT_KEY)?;
        match id {
            Some(id) if self.workspace_exists(&id)? => Ok(id),
            _ => Ok(ALL_ID.into()),
        }
    }

    fn workspace_exists(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM workspaces WHERE id = ?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some())
    }

    /// Make `id` the current workspace (new buffers are saved into it).
    pub fn set_current_workspace(&mut self, id: &str) -> Result<()> {
        if !self.workspace_exists(id)? {
            return Err(StoreError::NotFound(format!("workspace {id}")));
        }
        self.set_setting(CURRENT_KEY, &id)
    }

    /// A new, empty-layout workspace with `scope`.
    pub fn create_workspace(&mut self, name: &str, scope: WorkspaceScope) -> Result<Workspace> {
        let name = validate_name(name)?;
        self.check_unique_name(&name, None)?;
        let w = Workspace {
            id: crate::random::random_hex(8),
            name,
            scope: WorkspaceScope {
                all: false,
                ..scope.normalized()
            },
            ..Workspace::default()
        };
        self.write_workspace_row(&w, None)?;
        Ok(w)
    }

    fn check_unique_name(&self, name: &str, except: Option<&str>) -> Result<()> {
        let clash = self
            .workspaces()?
            .into_iter()
            .any(|w| w.name.eq_ignore_ascii_case(name) && Some(w.id.as_str()) != except);
        if clash {
            return Err(StoreError::Invalid(format!(
                "A workspace named \"{name}\" exists already"
            )));
        }
        Ok(())
    }

    /// Rename a workspace ("All" keeps its name).
    pub fn rename_workspace(&mut self, id: &str, name: &str) -> Result<Workspace> {
        if id == ALL_ID {
            return Err(StoreError::Invalid("\"All\" cannot be renamed".into()));
        }
        let name = validate_name(name)?;
        self.check_unique_name(&name, Some(id))?;
        let mut w = self
            .workspace_by_id(id)?
            .ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))?;
        w.name = name;
        self.write_workspace_row(&w, None)?;
        Ok(w)
    }

    /// Save a workspace's layout and scope (not its buffers). "All" keeps its name and
    /// shows everything; an unknown id creates the workspace.
    pub fn save_workspace(&mut self, w: &Workspace) -> Result<()> {
        let mut w = w.clone();
        if w.id.is_empty() {
            w.id = ALL_ID.into();
        }
        if w.id == ALL_ID {
            w.name = ALL_NAME.into();
            w.scope = WorkspaceScope::everything();
        } else {
            w.name = validate_name(&w.name)?;
            w.scope = WorkspaceScope {
                all: false,
                ..w.scope.normalized()
            };
        }
        self.write_workspace_row(&w, None)
    }

    /// A copy of a workspace (scope, layout and SQL buffers) named `name`.
    pub fn duplicate_workspace(&mut self, id: &str, name: &str) -> Result<Workspace> {
        let src = self
            .workspace_by_id(id)?
            .ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))?;
        let name = validate_name(name)?;
        self.check_unique_name(&name, None)?;
        let mut copy = src.clone();
        copy.id = crate::random::random_hex(8);
        copy.name = name;
        if id == ALL_ID {
            // A copy of "All" is an ordinary workspace listing today's items.
            copy.scope = WorkspaceScope {
                members: self
                    .profiles()?
                    .iter()
                    .filter(|p| p.is_connection())
                    .map(|p| p.id().clone())
                    .collect(),
                ..WorkspaceScope::default()
            };
        }
        copy.buffers.clear();
        let mut id_map = HashMap::new();
        for (i, b) in src.buffers.iter().enumerate() {
            let mut nb = b.clone();
            nb.id = format!("b{}", crate::random::random_hex(8));
            id_map.insert(b.id.clone(), nb.id.clone());
            self.write_buffer(&nb, i as i64, &copy.id)?;
        }
        copy.active_buffer = copy.active_buffer.and_then(|a| id_map.get(&a).cloned());
        self.write_workspace_row(&copy, None)?;
        Ok(copy)
    }

    /// The profiles that only workspace `id` shows: matched by it and by no other
    /// workspace ("All" aside). Credentials are never workspace members.
    pub fn exclusive_profiles(&self, id: &str) -> Result<Vec<Profile>> {
        if id == ALL_ID {
            return Ok(Vec::new());
        }
        let profiles = self.profiles()?;
        let all = self.workspaces()?;
        let Some(this) = all.iter().find(|w| w.id == id) else {
            return Ok(Vec::new());
        };
        let mut elsewhere: HashSet<ProfileId> = HashSet::new();
        for w in all.iter().filter(|w| w.id != id && w.id != ALL_ID) {
            elsewhere.extend(w.scope.visible(&profiles));
        }
        Ok(profiles
            .into_iter()
            .filter(|p| p.is_connection() && this.scope.matches(p) && !elsewhere.contains(p.id()))
            .collect())
    }

    /// Delete a workspace: its scope, layout and buffers. With `delete_profiles`, also
    /// the profiles in no other workspace ([`Store::exclusive_profiles`]); one still used
    /// by a profile that stays is kept and named in [`WorkspaceDeletion::kept`]. "All"
    /// cannot be deleted; deleting the current workspace makes "All" current.
    pub fn delete_workspace(
        &mut self,
        id: &str,
        delete_profiles: bool,
    ) -> Result<WorkspaceDeletion> {
        if id == ALL_ID {
            return Err(StoreError::Invalid("\"All\" cannot be deleted".into()));
        }
        if !self.workspace_exists(id)? {
            return Err(StoreError::NotFound(format!("workspace {id}")));
        }
        let mut out = WorkspaceDeletion::default();
        if delete_profiles {
            let mut doomed: Vec<Profile> = self.exclusive_profiles(id)?;
            // Delete dependents first; stop when nothing more can go.
            loop {
                let remaining = self.profiles()?;
                let pos = doomed.iter().position(|d| {
                    !remaining
                        .iter()
                        .any(|p| p.id() != d.id() && p.references().contains(&d.id()))
                });
                let Some(pos) = pos else { break };
                let p = doomed.remove(pos);
                if let Some(p) = self.delete_profile(p.id())? {
                    out.deleted.push(p);
                }
            }
            out.kept = doomed.into_iter().map(|p| p.name().to_owned()).collect();
        }
        self.conn
            .execute("DELETE FROM buffers WHERE workspace_id = ?1", [id])?;
        self.conn
            .execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
        if self.setting::<String>(CURRENT_KEY)?.as_deref() == Some(id) {
            self.set_setting(CURRENT_KEY, &ALL_ID)?;
        }
        Ok(out)
    }

    /// Add profiles to a workspace ("All" already has everything).
    pub fn add_to_workspace(&mut self, id: &str, ids: &[ProfileId]) -> Result<Workspace> {
        self.edit_scope(id, ids, true)
    }

    /// Remove profiles from a workspace without deleting them.
    pub fn remove_from_workspace(&mut self, id: &str, ids: &[ProfileId]) -> Result<Workspace> {
        self.edit_scope(id, ids, false)
    }

    fn edit_scope(&mut self, id: &str, ids: &[ProfileId], add: bool) -> Result<Workspace> {
        let mut w = self
            .workspace_by_id(id)?
            .ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))?;
        if id == ALL_ID {
            return Ok(w);
        }
        let profiles = self.profiles()?;
        for pid in ids {
            if let Some(p) = profiles.iter().find(|p| p.id() == pid) {
                if add {
                    w.scope.add(p);
                } else {
                    w.scope.remove(p);
                }
            }
        }
        self.write_workspace_row(&w, None)?;
        Ok(w)
    }

    /// The workspaces (other than "All") that show profile `id` itself.
    pub fn workspaces_of(&self, id: &ProfileId) -> Result<Vec<String>> {
        let Some(p) = self.profile(id)? else {
            return Ok(Vec::new());
        };
        Ok(self
            .workspaces()?
            .into_iter()
            .filter(|w| w.id != ALL_ID && w.scope.matches(&p))
            .map(|w| w.id)
            .collect())
    }

    /// Insert a buffer into workspace `ws` (or update it, keeping its workspace).
    pub(crate) fn write_buffer(
        &mut self,
        b: &crate::model::BufferState,
        position: i64,
        ws: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO buffers (id, title, connection_id, text, cursor, position, updated_at, workspace_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET title = ?2, connection_id = ?3, text = ?4,
                 cursor = ?5, position = ?6, updated_at = ?7",
            params![
                b.id,
                b.title,
                b.connection_id.as_ref().map(|c| c.0.clone()),
                b.text,
                b.cursor as i64,
                position,
                now_ms(),
                ws
            ],
        )?;
        Ok(())
    }

    /// Buffers of workspace `ws` in tab order.
    pub fn buffers_of(&self, ws: &str) -> Result<Vec<crate::model::BufferState>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, connection_id, text, cursor FROM buffers WHERE workspace_id = ?1
             ORDER BY position, updated_at",
        )?;
        let rows = stmt.query_map([ws], |r| {
            Ok(crate::model::BufferState {
                id: r.get(0)?,
                title: r.get(1)?,
                connection_id: r.get::<_, Option<String>>(2)?.map(ProfileId),
                text: r.get(3)?,
                cursor: r.get::<_, i64>(4)?.max(0) as usize,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- server facts (Servers view) ----

    /// Remember the operating system a Fleet probe found on a Host.
    pub fn set_server_os(&mut self, id: &ProfileId, os: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO server_facts (profile_id, os) VALUES (?1, ?2)
             ON CONFLICT(profile_id) DO UPDATE SET os = ?2",
            params![id.0, os],
        )?;
        Ok(())
    }

    /// Remember that the user connected to a profile now.
    pub fn touch_connected(&mut self, id: &ProfileId) -> Result<()> {
        if self.profile(id)?.is_none() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO server_facts (profile_id, last_connected) VALUES (?1, ?2)
             ON CONFLICT(profile_id) DO UPDATE SET last_connected = ?2",
            params![id.0, now_ms()],
        )?;
        Ok(())
    }

    /// Known facts per profile: (operating system, last connected ms).
    pub fn server_facts(&self) -> Result<Vec<ServerFacts>> {
        let mut stmt = self
            .conn
            .prepare("SELECT profile_id, os, last_connected FROM server_facts")?;
        let rows = stmt.query_map([], |r| {
            Ok(ServerFacts {
                id: ProfileId(r.get(0)?),
                os: r.get(1)?,
                last_connected: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// What the Servers view knows about a profile beyond its settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerFacts {
    /// Profile.
    pub id: ProfileId,
    /// Operating system (`PRETTY_NAME` from a Fleet probe).
    pub os: Option<String>,
    /// Last connection, ms since the epoch.
    pub last_connected: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BufferState, DbConnection, EnvironmentLabel, Host};
    use switchyard_db::Engine;

    fn host(name: &str, folder: Option<&str>, tags: &[&str]) -> Profile {
        let mut h = Host::new(name, "10.0.0.1", "root");
        h.folder = folder.map(str::to_owned);
        h.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        h.environment = EnvironmentLabel::Production;
        Profile::Host(h)
    }

    #[test]
    fn scope_rules_members_and_exclusions() {
        let web = host("web", Some("Prod/Web"), &["role:web"]);
        let db = host("db", Some("Production"), &["role:db"]);
        let lab = host("lab", None, &[]);
        let mut s = WorkspaceScope {
            folders: vec!["prod".into()],
            tags: vec!["Role:DB".into()],
            ..Default::default()
        };
        assert!(s.matches(&web), "sub-folder of a folder rule");
        assert!(s.matches(&db), "tag rule (normalized)");
        assert!(!s.matches(&lab));
        s.add(&lab);
        assert!(s.matches(&lab));
        assert_eq!(s.members, [lab.id().clone()]);
        s.add(&web);
        assert_eq!(s.members.len(), 1, "a rule already covers web");
        s.remove(&web);
        assert!(!s.matches(&web), "removal excludes a rule match");
        s.add(&web);
        assert!(s.matches(&web), "adding back undoes the exclusion");
        s.remove(&lab);
        assert!(!s.matches(&lab) && s.excluded.is_empty());
        assert!(WorkspaceScope::everything().matches(&lab));
    }

    #[test]
    fn visible_includes_what_members_need() {
        let jump = host("jump", None, &[]);
        let mut d = DbConnection::new("orders", Engine::Postgres);
        d.via_host = Some(jump.id().clone());
        let d = Profile::Db(d);
        let s = WorkspaceScope {
            members: vec![d.id().clone()],
            ..Default::default()
        };
        let v = s.visible(&[jump.clone(), d.clone()]);
        assert!(v.contains(jump.id()) && v.contains(d.id()));
    }

    fn buffer(id: &str, text: &str) -> BufferState {
        BufferState {
            id: id.into(),
            title: id.into(),
            connection_id: None,
            text: text.into(),
            cursor: 0,
        }
    }

    #[test]
    fn workspaces_round_trip_with_their_own_buffers() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(s.workspaces().unwrap().len(), 1);
        assert_eq!(s.current_workspace_id().unwrap(), ALL_ID);
        s.save_buffer(&buffer("b1", "select 1"), 0).unwrap();

        let prod = s
            .create_workspace(
                " Prod ",
                WorkspaceScope {
                    tags: vec!["env:prod".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(prod.name, "Prod");
        assert!(
            s.create_workspace("prod", WorkspaceScope::default())
                .is_err()
        );
        s.set_current_workspace(&prod.id).unwrap();
        s.save_buffer(&buffer("b2", "select 2"), 0).unwrap();
        // An update of a buffer from another workspace keeps it there.
        s.save_buffer(&buffer("b1", "select 11"), 0).unwrap();

        let mut w = s.workspace().unwrap();
        assert_eq!(w.id, prod.id);
        assert_eq!(w.buffers.len(), 1);
        assert_eq!(w.buffers[0].id, "b2");
        w.tabs = vec![SavedTab::Servers, SavedTab::Terminal { host: None }];
        w.explorer_collapsed = vec!["sec:cloud".into()];
        w.sidebar_collapsed = true;
        s.save_workspace(&w).unwrap();
        let back = s.workspace_by_id(&prod.id).unwrap().unwrap();
        assert_eq!(back.tabs, w.tabs);
        assert_eq!(back.explorer_collapsed, w.explorer_collapsed);
        assert!(back.sidebar_collapsed);
        let all = s.workspace_by_id(ALL_ID).unwrap().unwrap();
        assert_eq!(all.buffers[0].text, "select 11");

        let renamed = s.rename_workspace(&prod.id, "Production").unwrap();
        assert_eq!(renamed.name, "Production");
        assert!(s.rename_workspace(ALL_ID, "x").is_err());
        let copy = s.duplicate_workspace(&prod.id, "Production 2").unwrap();
        let copy = s.workspace_by_id(&copy.id).unwrap().unwrap();
        assert_eq!(copy.scope, back.scope);
        assert_eq!(copy.tabs, back.tabs);
        assert_eq!(copy.buffers.len(), 1);
        assert_ne!(copy.buffers[0].id, "b2");
        assert_eq!(s.workspaces().unwrap().len(), 3);
        assert_eq!(s.workspaces().unwrap()[0].id, ALL_ID);
    }

    #[test]
    fn the_old_layout_becomes_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        {
            // A version-7 store with the single layout in the `workspace` setting.
            let conn = rusqlite::Connection::open(&path).unwrap();
            for sql in &crate::store::MIGRATIONS[..7] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 7).unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('workspace', ?1)",
                [r#"{"name":"Default","sidebar_collapsed":true,"active_buffer":"b1"}"#],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO buffers (id, title, text, updated_at) VALUES ('b1', 'q', 'select 1', 0)",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.schema_version().unwrap(),
            crate::store::MIGRATIONS.len() as i64
        );
        let w = s.workspace().unwrap();
        assert_eq!((w.id.as_str(), w.name.as_str()), (ALL_ID, ALL_NAME));
        assert!(w.scope.all && w.sidebar_collapsed);
        assert_eq!(w.active_buffer.as_deref(), Some("b1"));
        assert_eq!(w.buffers.len(), 1, "existing buffers belong to All");
    }

    #[test]
    fn delete_only_the_workspace_or_its_exclusive_items_too() {
        let mut s = Store::open_in_memory().unwrap();
        let a = host("a", None, &[]);
        let b = host("b", None, &[]);
        let c = host("c", None, &[]);
        for p in [&a, &b, &c] {
            s.save_profile(p).unwrap();
        }
        let w1 = s
            .create_workspace("one", WorkspaceScope::default())
            .unwrap();
        let w2 = s
            .create_workspace("two", WorkspaceScope::default())
            .unwrap();
        s.add_to_workspace(&w1.id, &[a.id().clone(), b.id().clone()])
            .unwrap();
        s.add_to_workspace(&w2.id, &[b.id().clone()]).unwrap();
        assert_eq!(s.workspaces_of(b.id()).unwrap().len(), 2);
        let only: Vec<String> = s
            .exclusive_profiles(&w1.id)
            .unwrap()
            .iter()
            .map(|p| p.name().to_owned())
            .collect();
        assert_eq!(only, ["a"]);

        // Removing from a workspace never deletes.
        s.remove_from_workspace(&w2.id, &[b.id().clone()]).unwrap();
        assert!(s.profile(b.id()).unwrap().is_some());
        s.add_to_workspace(&w2.id, &[b.id().clone()]).unwrap();

        // Default: only the workspace goes.
        s.set_current_workspace(&w2.id).unwrap();
        let gone = s.delete_workspace(&w2.id, false).unwrap();
        assert!(gone.deleted.is_empty());
        assert_eq!(s.profiles().unwrap().len(), 3);
        assert_eq!(s.current_workspace_id().unwrap(), ALL_ID);

        // With the option: the items in no other workspace go too.
        let gone = s.delete_workspace(&w1.id, true).unwrap();
        let mut names: Vec<&str> = gone.deleted.iter().map(|p| p.name()).collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(s.profiles().unwrap().len(), 1);
        assert!(s.delete_workspace(ALL_ID, false).is_err());
        assert_eq!(s.workspaces().unwrap().len(), 1);
    }
}
