//! Server workspaces of the Default side: named scopes over Hosts, database, cloud and
//! container connections and remote desktops, each with its own tabs, layout and
//! Explorer state. The built-in "All" shows everything. The runtime stores them
//! ([`switchyard_core::servers`]); this module keeps the UI side: the scoped profile
//! list the Explorer and the Servers view draw, which open tab belongs to which
//! workspace, switching, and the add / remove actions of menus and the palette.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, EntityId, SharedString, Window};
use switchyard_core::servers::{LaunchState, ReachStatus, ServersCommand, ServersEvent};
use switchyard_core::store::{
    ALL_ID, FileProtocol, Profile, ProfileId, SavedTab, ServerFacts, Workspace as SavedWorkspace,
    WorkspaceScope,
};
use switchyard_core::{Command, RequestId};

use crate::app_state::Profiles;
use crate::explorer_tree::ExplorerGroup;
use crate::sidebar::{CtxTarget, SideTab};
use crate::workspace::{AppMode, Tab, Workspace};

/// Workspaces listed in a context menu's "Add to workspace ▸" submenu; more are reached
/// through the workspace settings sheet.
pub(crate) const MENU_SLOTS: usize = 24;

/// The submenu's actions, one per listed workspace (the menu's actions are static).
pub(crate) const WS_ADD: [&str; MENU_SLOTS] = [
    "ws_add_0",
    "ws_add_1",
    "ws_add_2",
    "ws_add_3",
    "ws_add_4",
    "ws_add_5",
    "ws_add_6",
    "ws_add_7",
    "ws_add_8",
    "ws_add_9",
    "ws_add_10",
    "ws_add_11",
    "ws_add_12",
    "ws_add_13",
    "ws_add_14",
    "ws_add_15",
    "ws_add_16",
    "ws_add_17",
    "ws_add_18",
    "ws_add_19",
    "ws_add_20",
    "ws_add_21",
    "ws_add_22",
    "ws_add_23",
];

/// Which tab a workspace owns: an entity, or the one Servers view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TabKey {
    /// A tab backed by an entity.
    Entity(EntityId),
    /// The Servers view.
    Servers,
}

/// The ownership key of a tab (`None` for Welcome, which every workspace shows).
pub(crate) fn tab_key(tab: &Tab) -> Option<TabKey> {
    use crate::infra::InfraTab;
    let id = match tab {
        Tab::Welcome => return None,
        Tab::Servers => return Some(TabKey::Servers),
        Tab::Sql(e) => e.entity_id(),
        Tab::Terminal(e) => e.entity_id(),
        Tab::Files(e) => e.entity_id(),
        Tab::Editor(e) => e.entity_id(),
        Tab::Workload(e) => e.entity_id(),
        Tab::Ddl(e) => e.entity_id(),
        Tab::Object(e) => e.entity_id(),
        Tab::Activity(e) => e.entity_id(),
        Tab::Er(e) => e.entity_id(),
        Tab::Redis(e) => e.entity_id(),
        Tab::Cloud(e) => e.entity_id(),
        Tab::Infra(InfraTab::Tf(e)) => e.entity_id(),
        Tab::Infra(InfraTab::Inventory(e)) => e.entity_id(),
        Tab::Infra(InfraTab::Checks(e)) => e.entity_id(),
        Tab::Infra(InfraTab::Logs(e)) => e.entity_id(),
        Tab::Fleet(e) => e.entity_id(),
        Tab::Compare(e) => e.entity_id(),
        Tab::Runbook(e) => e.entity_id(),
        Tab::Containers(e) => e.entity_id(),
        Tab::Rdp(e) => e.entity_id(),
    };
    Some(TabKey::Entity(id))
}

/// The Host an item lives on (a database through its tunnel, SFTP, a Host terminal or
/// container connection): it shows wherever its Host is a member.
fn parent_host(p: &Profile) -> Option<&ProfileId> {
    match p {
        Profile::Db(d) => d.via_host.as_ref(),
        Profile::File(f) => match &f.protocol {
            FileProtocol::Sftp { host_id } => Some(host_id),
            FileProtocol::Ftp { .. } => None,
        },
        Profile::Terminal(t) => t.host_id.as_ref(),
        Profile::Containers(c) => c.host_id.as_ref(),
        _ => None,
    }
}

/// The profiles a workspace shows, in saved order: its members and rule matches, what
/// they need (jump and tunnel Hosts, credentials), and the items living on a member Host.
pub(crate) fn scope_profiles(all: &[Profile], scope: &WorkspaceScope) -> Vec<Profile> {
    if scope.all {
        return all.to_vec();
    }
    let visible = scope.visible(all);
    let matched: HashSet<&ProfileId> = all
        .iter()
        .filter(|p| scope.matches(p))
        .map(|p| p.id())
        .collect();
    all.iter()
        .filter(|p| {
            visible.contains(p.id())
                || (!scope.excluded.contains(p.id())
                    && parent_host(p).is_some_and(|h| matched.contains(h)))
        })
        .cloned()
        .collect()
}

/// What a context menu or palette command adds to (or removes from) a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WsTarget {
    /// These items.
    Items(Vec<ProfileId>),
    /// A folder rule (every item in the folder, now and later).
    Folder(String),
    /// A tag rule.
    Tag(String),
}

impl WsTarget {
    /// The scope of a new workspace holding just this target.
    pub(crate) fn scope(&self) -> WorkspaceScope {
        let mut s = WorkspaceScope::default();
        match self {
            WsTarget::Items(ids) => s.members = ids.clone(),
            WsTarget::Folder(f) => s.folders = vec![f.clone()],
            WsTarget::Tag(t) => s.tags = vec![t.clone()],
        }
        s
    }

    /// A short description ("3 items", "folder Prod").
    pub(crate) fn describe(&self) -> String {
        match self {
            WsTarget::Items(ids) if ids.len() == 1 => "1 item".into(),
            WsTarget::Items(ids) => format!("{} items", ids.len()),
            WsTarget::Folder(f) => format!("folder {f}"),
            WsTarget::Tag(t) => format!("tag {t}"),
        }
    }
}

/// `scope` with a folder or tag rule added or removed. Removing a rule also drops the
/// matching items' memberships (they stay in "All"); adding one lets excluded matches
/// back in.
pub(crate) fn scope_with_rule(
    scope: &WorkspaceScope,
    target: &WsTarget,
    add: bool,
    all: &[Profile],
) -> WorkspaceScope {
    let mut s = scope.clone();
    let in_rule = |p: &Profile| match target {
        WsTarget::Folder(f) => p
            .folder()
            .is_some_and(|pf| pf == f || pf.starts_with(&format!("{f}/"))),
        WsTarget::Tag(t) => switchyard_core::store::tags::has_tag(p.tags(), t),
        WsTarget::Items(ids) => ids.contains(p.id()),
    };
    match (target, add) {
        (WsTarget::Items(_), true) => {
            for p in all.iter().filter(|p| in_rule(p)) {
                s.add(p);
            }
        }
        (WsTarget::Items(_), false) => {
            for p in all.iter().filter(|p| in_rule(p)) {
                s.remove(p);
            }
        }
        (WsTarget::Folder(f), true) => {
            if !s.folders.contains(f) {
                s.folders.push(f.clone());
            }
            let back: Vec<ProfileId> = all
                .iter()
                .filter(|p| in_rule(p))
                .map(|p| p.id().clone())
                .collect();
            s.excluded.retain(|x| !back.contains(x));
        }
        (WsTarget::Tag(t), true) => {
            let t = t.to_lowercase();
            if !s.tags.contains(&t) {
                s.tags.push(t);
            }
            let back: Vec<ProfileId> = all
                .iter()
                .filter(|p| in_rule(p))
                .map(|p| p.id().clone())
                .collect();
            s.excluded.retain(|x| !back.contains(x));
        }
        (WsTarget::Folder(f), false) => {
            s.folders.retain(|x| x != f);
            for p in all.iter().filter(|p| in_rule(p)) {
                s.remove(p);
            }
        }
        (WsTarget::Tag(t), false) => {
            s.tags.retain(|x| !x.eq_ignore_ascii_case(t));
            for p in all.iter().filter(|p| in_rule(p)) {
                s.remove(p);
            }
        }
    }
    s.normalized()
}

/// How the Servers view groups its rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ServerGroup {
    /// One list.
    #[default]
    None,
    /// By folder.
    Folder,
    /// By tag (a server with several tags lists under each).
    Tag,
    /// By environment, Production first.
    Environment,
}

/// The Servers view's own state.
pub(crate) struct ServersView {
    /// Cards instead of the list.
    pub cards: bool,
    /// Grouping.
    pub group: ServerGroup,
    /// Search (name, address, tag), created on first render.
    pub search: Option<Entity<InputState>>,
    /// Picked rows (Ctrl/⌘-click) for "Add to workspace".
    pub selection: Vec<ProfileId>,
}

/// Something sent to the runtime whose answer the UI waits for.
pub(crate) enum Pending {
    /// A new workspace: switch to it once it exists.
    NewWorkspace {
        /// Switch to it.
        switch: bool,
    },
    /// A profile saved from the connection editor: its workspace checkboxes, applied
    /// once it exists.
    Membership(Vec<(String, bool)>),
}

/// Workspace and Servers view state of the window.
pub(crate) struct ServersUi {
    /// Every workspace ("All" first), as the runtime last sent them.
    pub workspaces: Vec<SavedWorkspace>,
    /// The current workspace's id.
    pub current: String,
    /// The profiles the current workspace shows (the Explorer and Servers view list these).
    pub scoped: Profiles,
    /// The workspace each open tab belongs to (tabs not listed belong to the current one).
    pub owner: HashMap<TabKey, String>,
    /// The tab each workspace had in front when the user switched away.
    pub last_active: HashMap<String, TabKey>,
    /// Workspaces whose saved tabs were reopened in this run.
    pub restored: HashSet<String>,
    /// The current workspace's saved layout arrived (layout saves wait for it).
    pub layout_ready: bool,
    /// Saved tabs to reopen once profiles are loaded.
    pub pending_tabs: Vec<SavedTab>,
    /// OS and last connection per profile.
    pub facts: HashMap<ProfileId, ServerFacts>,
    /// Reachability per profile.
    pub reach: HashMap<ProfileId, ReachStatus>,
    /// Reachability checks are on (setting).
    pub reach_on: bool,
    /// What the running reachability loop checks.
    pub reach_running: Vec<ProfileId>,
    /// Remote desktop launches in flight: request → (profile, client).
    pub launches: HashMap<RequestId, (ProfileId, Option<String>)>,
    /// The Servers view.
    pub view: ServersView,
    /// Requests waiting for an answer.
    pub pending: HashMap<RequestId, Pending>,
}

impl Default for ServersUi {
    fn default() -> Self {
        Self {
            workspaces: Vec::new(),
            current: ALL_ID.to_owned(),
            scoped: Profiles::default(),
            owner: HashMap::new(),
            last_active: HashMap::new(),
            restored: HashSet::new(),
            layout_ready: false,
            pending_tabs: Vec::new(),
            facts: HashMap::new(),
            reach: HashMap::new(),
            reach_on: true,
            reach_running: Vec::new(),
            launches: HashMap::new(),
            view: ServersView {
                cards: false,
                group: ServerGroup::None,
                search: None,
                selection: Vec::new(),
            },
            pending: HashMap::new(),
        }
    }
}

impl ServersUi {
    /// The current workspace.
    pub(crate) fn current_ws(&self) -> Option<&SavedWorkspace> {
        self.workspaces.iter().find(|w| w.id == self.current)
    }

    /// A workspace by id.
    pub(crate) fn ws(&self, id: &str) -> Option<&SavedWorkspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    /// Whether the current workspace is "All".
    pub(crate) fn in_all(&self) -> bool {
        self.current == ALL_ID
    }

    /// The workspaces a context menu offers to add to (not "All"): (id, name).
    pub(crate) fn menu_targets(&self) -> Vec<(String, String)> {
        self.workspaces
            .iter()
            .filter(|w| w.id != ALL_ID)
            .take(MENU_SLOTS)
            .map(|w| (w.id.clone(), w.name.clone()))
            .collect()
    }
}

/// The saved key of a sidebar page.
pub(crate) fn side_tab_key(t: SideTab) -> &'static str {
    match t {
        SideTab::Explorer => "explorer",
        SideTab::Schema => "schema",
        SideTab::Tools => "tools",
        SideTab::Activity => "activity",
    }
}

/// A sidebar page from its saved key.
pub(crate) fn side_tab_from_key(s: &str) -> Option<SideTab> {
    Some(match s {
        "explorer" => SideTab::Explorer,
        "schema" => SideTab::Schema,
        "tools" => SideTab::Tools,
        "activity" => SideTab::Activity,
        _ => return None,
    })
}

impl Workspace {
    /// Recompute the current workspace's profiles (after profiles or scopes changed).
    pub(crate) fn refresh_scope(&mut self) {
        let scope = self
            .servers
            .current_ws()
            .map(|w| w.scope.clone())
            .unwrap_or_else(WorkspaceScope::everything);
        self.servers.scoped = Profiles {
            all: scope_profiles(&self.profiles.all, &scope),
        };
    }

    /// Whether tab `ix` shows in the current workspace.
    pub(crate) fn tab_visible(&self, ix: usize) -> bool {
        match self.tabs.get(ix).and_then(tab_key) {
            None => self.tabs.get(ix).is_some(),
            Some(k) => self
                .servers
                .owner
                .get(&k)
                .is_none_or(|o| *o == self.servers.current),
        }
    }

    /// The indices of the tabs shown in the current workspace.
    pub(crate) fn visible_tabs(&self) -> Vec<usize> {
        (0..self.tabs.len())
            .filter(|i| self.tab_visible(*i))
            .collect()
    }

    /// Bring tab `ix` into the current workspace (it was opened again from here).
    pub(crate) fn adopt_tab(&mut self, ix: usize) {
        if let Some(k) = self.tabs.get(ix).and_then(tab_key) {
            self.servers.owner.insert(k, self.servers.current.clone());
        }
    }

    /// The current layout as a saved workspace (no buffers: those autosave themselves).
    pub(crate) fn layout_snapshot(&self, cx: &gpui_kit::App) -> SavedWorkspace {
        let active_buffer = match self.tabs.get(self.active) {
            Some(Tab::Sql(t)) => Some(t.read(cx).buffer_id().to_owned()),
            _ => None,
        };
        let mut w = self
            .servers
            .current_ws()
            .cloned()
            .unwrap_or(SavedWorkspace {
                id: self.servers.current.clone(),
                name: self.workspace_name.clone(),
                ..Default::default()
            });
        w.buffers.clear();
        w.tabs = self.saved_tabs(cx);
        let mut collapsed: Vec<String> = self.collapsed.iter().cloned().collect();
        collapsed.sort();
        w.explorer_collapsed = collapsed;
        w.side_tab = Some(side_tab_key(self.side_tab).to_owned());
        w.explorer_group = Some(self.explorer_group.key().to_owned());
        w.sidebar_width = Some(self.sidebar_width);
        w.sidebar_collapsed = !self.sidebar_open;
        w.active_buffer = active_buffer;
        w
    }

    /// The current workspace's tabs as they reopen next time (SQL editors are buffers).
    fn saved_tabs(&self, cx: &gpui_kit::App) -> Vec<SavedTab> {
        let mut out = Vec::new();
        for i in self.visible_tabs() {
            let t = match &self.tabs[i] {
                Tab::Terminal(t) => match t.read(cx).saved_target() {
                    Some(host) => SavedTab::Terminal { host },
                    None => continue,
                },
                Tab::Files(f) => SavedTab::Files {
                    host: f.read(cx).right_host().cloned(),
                },
                Tab::Servers => SavedTab::Servers,
                Tab::Redis(r) => SavedTab::Profile {
                    id: r.read(cx).connection.id.clone(),
                },
                Tab::Cloud(c) => SavedTab::Profile {
                    id: c.read(cx).connection.id.clone(),
                },
                Tab::Containers(c) => SavedTab::Profile {
                    id: c.read(cx).connection.id.clone(),
                },
                _ => continue,
            };
            if !out.contains(&t) {
                out.push(t);
            }
        }
        out
    }

    /// Apply a saved layout (sidebar, Explorer, name).
    pub(crate) fn apply_layout(&mut self, w: &SavedWorkspace) {
        self.workspace_name = w.name.clone();
        self.sidebar_open = !w.sidebar_collapsed;
        self.collapsed = w.explorer_collapsed.iter().cloned().collect();
        if let Some(t) = w.side_tab.as_deref().and_then(side_tab_from_key) {
            self.side_tab = t;
        }
        if let Some(g) = &w.explorer_group {
            self.explorer_group = ExplorerGroup::from_key(g);
        }
        if let Some(width) = w.sidebar_width {
            self.sidebar_width = width.max(crate::sidebar::SIDEBAR_MIN);
        }
    }

    /// Reopen saved tabs (terminals, files, the Servers view, Redis / cloud / container
    /// browsers) once profiles are loaded.
    pub(crate) fn reopen_saved_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.profiles_loaded || self.servers.pending_tabs.is_empty() {
            return;
        }
        let active = self.active;
        for t in std::mem::take(&mut self.servers.pending_tabs) {
            match t {
                SavedTab::Terminal { host } => {
                    if host
                        .as_ref()
                        .is_none_or(|h| self.profiles.host(h).is_some())
                    {
                        self.open_terminal(host, cx);
                    }
                }
                SavedTab::Files { host } => self.open_files_for(host, cx),
                SavedTab::Servers => self.open_servers(cx),
                SavedTab::Profile { id } => {
                    if self.profiles.all.iter().any(|p| p.id() == &id) {
                        self.open_profile(&id, window, cx);
                    }
                }
            }
        }
        // Reopening does not change the tab in front.
        if self.tab_visible(active) {
            self.active = active;
        }
    }

    /// Show workspace `id` (and the Default side).
    pub(crate) fn switch_workspace(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode_menu = false;
        if self.mode != AppMode::Default {
            self.set_mode(AppMode::Default, window, cx);
        }
        if id == self.servers.current || self.servers.ws(id).is_none() {
            cx.notify();
            return;
        }
        let old = self.servers.current.clone();
        // Remember the old workspace's layout, here and in the store.
        let snap = self.layout_snapshot(cx);
        if let Some(w) = self.servers.workspaces.iter_mut().find(|w| w.id == old) {
            *w = snap.clone();
        }
        if self.servers.layout_ready {
            self.core.send(Command::SaveWorkspace(snap));
        }
        // Tabs not yet claimed belong to the workspace they were opened in.
        for t in &self.tabs {
            if let Some(k) = tab_key(t) {
                self.servers.owner.entry(k).or_insert_with(|| old.clone());
            }
        }
        if let Some(k) = self.tabs.get(self.active).and_then(tab_key) {
            self.servers.last_active.insert(old, k);
        }
        self.servers.current = id.to_owned();
        let first = self.servers.restored.insert(id.to_owned());
        self.core.send(Command::Servers(ServersCommand::Switch {
            id: id.to_owned(),
            load: first,
        }));
        if let Some(w) = self.servers.ws(id).cloned() {
            self.apply_layout(&w);
        }
        self.pick_active_tab();
        self.host_selection.clear();
        self.servers.view.selection.clear();
        self.refresh_scope();
        self.sync_reach();
        self.sync_schema(cx);
        cx.notify();
    }

    /// After a switch: the tab the workspace last had in front, else its first tab.
    fn pick_active_tab(&mut self) {
        let visible = self.visible_tabs();
        let last = self.servers.last_active.get(&self.servers.current).copied();
        let ix = last
            .and_then(|k| {
                visible
                    .iter()
                    .copied()
                    .find(|i| self.tabs.get(*i).and_then(tab_key) == Some(k))
            })
            .or_else(|| {
                visible
                    .iter()
                    .copied()
                    .find(|i| !matches!(self.tabs[*i], Tab::Welcome))
            })
            .or_else(|| visible.first().copied())
            .unwrap_or(0);
        self.active = ix;
        if let Some(sp) = &self.split
            && !self.tab_visible(sp.other)
        {
            self.split = None;
        }
    }

    /// Start, change or stop the reachability checks: they run while the Servers view is
    /// open in the current workspace and the setting is on.
    pub(crate) fn sync_reach(&mut self) {
        let shown = self.servers.reach_on
            && self
                .tabs
                .iter()
                .enumerate()
                .any(|(i, t)| matches!(t, Tab::Servers) && self.tab_visible(i));
        let ids: Vec<ProfileId> = if shown {
            self.servers
                .scoped
                .all
                .iter()
                .filter(|p| matches!(p, Profile::Host(_) | Profile::Desktop(_)))
                .map(|p| p.id().clone())
                .collect()
        } else {
            Vec::new()
        };
        if ids == self.servers.reach_running {
            return;
        }
        if ids.is_empty() {
            self.core
                .send(Command::Servers(ServersCommand::StopReachability));
        } else {
            self.core
                .send(Command::Servers(ServersCommand::StartReachability {
                    ids: ids.clone(),
                }));
        }
        self.servers.reach_running = ids;
    }

    /// Open (or focus) the Servers view.
    pub(crate) fn open_servers(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, Tab::Servers)) {
            self.adopt_tab(ix);
            self.activate(ix, cx);
        } else {
            self.tabs.push(Tab::Servers);
            self.active = self.tabs.len() - 1;
            self.core.send(Command::Servers(ServersCommand::Load));
            self.save_layout(cx);
        }
        self.sync_reach();
        cx.notify();
    }

    /// A runtime answer about workspaces, reachability, launches or credentials.
    pub(crate) fn on_servers_event(
        &mut self,
        ev: ServersEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            ServersEvent::Workspaces { list, current } => {
                self.servers.workspaces = list;
                // The runtime moved to "All" (the current workspace was deleted).
                if self.servers.ws(&self.servers.current).is_none() {
                    let gone = std::mem::replace(&mut self.servers.current, current);
                    self.forget_workspace_tabs(&gone, cx);
                    self.pick_active_tab();
                }
                if let Some(w) = self.servers.current_ws() {
                    self.workspace_name = w.name.clone();
                }
                self.refresh_scope();
                self.sync_reach();
            }
            ServersEvent::Created { request, workspace } => {
                if !self.servers.workspaces.iter().any(|w| w.id == workspace.id) {
                    self.servers.workspaces.push((*workspace).clone());
                }
                if let Some(Pending::NewWorkspace { switch }) =
                    self.servers.pending.remove(&request)
                {
                    self.toast(format!("Created workspace {}", workspace.name), cx);
                    if switch {
                        self.switch_workspace(&workspace.id, window, cx);
                    }
                }
            }
            ServersEvent::DeletePreview { id, exclusive } => {
                self.on_delete_preview(&id, exclusive, cx)
            }
            ServersEvent::Deleted { id, deleted, kept } => {
                let mut msg = String::from("Workspace deleted");
                if deleted > 0 {
                    msg = format!(
                        "Workspace deleted with {deleted} item{}",
                        if deleted == 1 { "" } else { "s" }
                    );
                }
                if !kept.is_empty() {
                    msg.push_str(&format!(" · kept {} (still used)", kept.join(", ")));
                }
                self.toast(msg, cx);
                if self.servers.current == id {
                    self.servers.current = ALL_ID.to_owned();
                    if let Some(w) = self.servers.ws(ALL_ID).cloned() {
                        self.apply_layout(&w);
                    }
                }
                self.forget_workspace_tabs(&id, cx);
                self.servers.workspaces.retain(|w| w.id != id);
                self.pick_active_tab();
                self.refresh_scope();
                self.sync_reach();
            }
            ServersEvent::ImportPreview { path, result } => {
                self.on_import_preview(path, result, window, cx)
            }
            ServersEvent::Imported {
                workspace,
                missing,
                summary,
            } => {
                self.toast(summary, cx);
                if matches!(
                    &self.overlay,
                    Some(crate::overlays::Overlay::Servers(
                        crate::workspace_dialogs::ServersOverlay::Import(_)
                    ))
                ) {
                    self.overlay = None;
                }
                // Its tabs (if any) come from the file's layout.
                self.servers.restored.remove(&workspace);
                self.switch_workspace(&workspace, window, cx);
                if !missing.is_empty() {
                    self.open_missing_secrets(missing, window, cx);
                }
            }
            ServersEvent::ImportFailed {
                path: _,
                message,
                wrong_passphrase,
            } => self.on_import_failed(message, wrong_passphrase, cx),
            ServersEvent::Facts(list) => {
                self.servers.facts = list.into_iter().map(|f| (f.id.clone(), f)).collect();
            }
            ServersEvent::Reach { id, status } => {
                self.servers.reach.insert(id, status);
            }
            ServersEvent::Launch { request, id, state } => {
                let name = self
                    .profiles
                    .all
                    .iter()
                    .find(|p| p.id() == &id)
                    .map(|p| p.name().to_owned())
                    .unwrap_or_default();
                match state {
                    LaunchState::Started(client) => {
                        self.servers
                            .launches
                            .insert(request, (id, Some(client.clone())));
                        self.toast(format!("{client} opened {name}"), cx);
                        self.core.send(Command::Servers(ServersCommand::Load));
                    }
                    LaunchState::Ended => {
                        self.servers.launches.remove(&request);
                    }
                    LaunchState::Failed { message, install } => {
                        self.servers.launches.remove(&request);
                        self.toast(message, cx);
                        if install.is_some() {
                            self.open_settings(crate::overlays::SettingsPage::Drivers, window, cx);
                        }
                    }
                }
            }
            ServersEvent::CredentialUsers { id, users } => self.on_credential_users(&id, users, cx),
        }
        cx.notify();
    }

    /// Close the open tabs of a deleted workspace (unsaved files move to "All").
    fn forget_workspace_tabs(&mut self, id: &str, cx: &mut Context<Self>) {
        let doomed: Vec<usize> = (0..self.tabs.len())
            .filter(|i| {
                self.tabs
                    .get(*i)
                    .and_then(tab_key)
                    .and_then(|k| self.servers.owner.get(&k))
                    .is_some_and(|o| o == id)
            })
            .collect();
        for i in doomed.into_iter().rev() {
            let dirty = matches!(&self.tabs[i], Tab::Editor(e) if e.read(cx).dirty);
            if dirty {
                if let Some(k) = tab_key(&self.tabs[i]) {
                    self.servers.owner.insert(k, ALL_ID.to_owned());
                }
            } else {
                self.close_tab_now(i, cx);
            }
        }
        self.servers.owner.retain(|_, o| o != id);
        self.servers.last_active.remove(id);
        self.servers.restored.remove(id);
    }

    /// Launch a remote desktop with the system client.
    pub(crate) fn launch_desktop(&mut self, id: &ProfileId, cx: &mut Context<Self>) {
        let request = crate::app_state::next_id();
        self.servers.launches.insert(request, (id.clone(), None));
        self.core.send(Command::Servers(ServersCommand::Launch {
            request,
            id: id.clone(),
        }));
        self.toast("Starting the remote desktop client…", cx);
    }

    /// What workspace actions apply to: the selection, else the context menu's target.
    pub(crate) fn ws_target_of(&self, target: &CtxTarget) -> Option<WsTarget> {
        match target {
            CtxTarget::Profile(id) => Some(WsTarget::Items(vec![id.clone()])),
            CtxTarget::Hosts(ids) | CtxTarget::Items(ids) => Some(WsTarget::Items(ids.clone())),
            CtxTarget::Folder(f) => Some(WsTarget::Folder(f.clone())),
            CtxTarget::Tag(t) => Some(WsTarget::Tag(t.clone())),
            _ => None,
        }
    }

    /// The palette's target: picked items, else what the tab in front is connected to.
    pub(crate) fn palette_ws_target(&self, cx: &gpui_kit::App) -> Option<WsTarget> {
        let picked: Vec<ProfileId> = if !self.servers.view.selection.is_empty()
            && self
                .tabs
                .get(self.active)
                .is_some_and(|t| matches!(t, Tab::Servers))
        {
            self.servers.view.selection.clone()
        } else {
            self.host_selection.clone()
        };
        if !picked.is_empty() {
            return Some(WsTarget::Items(picked));
        }
        let id = match self.tabs.get(self.active)? {
            Tab::Sql(t) => t.read(cx).connection.as_ref().map(|c| c.id.clone()),
            Tab::Terminal(t) => t.read(cx).host().cloned(),
            Tab::Files(f) => f.read(cx).right_host().cloned(),
            Tab::Redis(r) => Some(r.read(cx).connection.id.clone()),
            Tab::Cloud(c) => Some(c.read(cx).connection.id.clone()),
            Tab::Containers(c) => Some(c.read(cx).connection.id.clone()),
            _ => None,
        }?;
        Some(WsTarget::Items(vec![id]))
    }

    /// Add `target` to workspace `ws`.
    pub(crate) fn add_to_workspace(&mut self, ws: &str, target: &WsTarget, cx: &mut Context<Self>) {
        let name = self
            .servers
            .ws(ws)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        match target {
            WsTarget::Items(ids) => self.core.send(Command::Servers(ServersCommand::Add {
                id: ws.to_owned(),
                profiles: ids.clone(),
            })),
            rule => {
                let Some(scope) = self.servers.ws(ws).map(|w| w.scope.clone()) else {
                    return;
                };
                let scope = scope_with_rule(&scope, rule, true, &self.profiles.all);
                self.core.send(Command::Servers(ServersCommand::SetScope {
                    id: ws.to_owned(),
                    scope,
                }));
            }
        }
        self.toast(format!("Added {} to {name}", target.describe()), cx);
    }

    /// Remove `target` from the current workspace (it stays in "All").
    pub(crate) fn remove_from_workspace(&mut self, target: &WsTarget, cx: &mut Context<Self>) {
        if self.servers.in_all() {
            self.toast("“All” shows everything · switch to a workspace first", cx);
            return;
        }
        let ws = self.servers.current.clone();
        match target {
            WsTarget::Items(ids) => self.core.send(Command::Servers(ServersCommand::Remove {
                id: ws,
                profiles: ids.clone(),
            })),
            rule => {
                let Some(scope) = self.servers.ws(&ws).map(|w| w.scope.clone()) else {
                    return;
                };
                let scope = scope_with_rule(&scope, rule, false, &self.profiles.all);
                self.core
                    .send(Command::Servers(ServersCommand::SetScope { id: ws, scope }));
            }
        }
        self.host_selection.clear();
        self.servers.view.selection.clear();
        self.toast(
            format!(
                "Removed {} from this workspace · still in All",
                target.describe()
            ),
            cx,
        );
    }

    /// A context menu's workspace action; `false` when `action` is not one.
    pub(crate) fn ws_ctx_action(
        &mut self,
        action: &str,
        target: &CtxTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(t) = self.ws_target_of(target) else {
            return false;
        };
        if let Some(i) = WS_ADD.iter().position(|a| *a == action) {
            if let Some((id, _)) = self.servers.menu_targets().get(i).cloned() {
                self.add_to_workspace(&id, &t, cx);
            }
            return true;
        }
        match action {
            "ws_new" => {
                self.open_new_workspace(Some(t), window, cx);
                true
            }
            "ws_remove" => {
                self.remove_from_workspace(&t, cx);
                true
            }
            _ => false,
        }
    }

    /// The items a new connection editor's workspace checkboxes start from: every
    /// workspace but "All", ticked where the profile (or, for a new one, the current
    /// workspace) is a member.
    pub(crate) fn workspace_choices(
        &self,
        existing: Option<&Profile>,
    ) -> Vec<(String, String, bool)> {
        self.servers
            .workspaces
            .iter()
            .filter(|w| w.id != ALL_ID)
            .map(|w| {
                let on = match existing {
                    Some(p) => w.scope.matches(p),
                    None => w.id == self.servers.current,
                };
                (w.id.clone(), w.name.clone(), on)
            })
            .collect()
    }

    /// Apply an editor's workspace checkboxes once its profile is saved.
    pub(crate) fn apply_membership(&mut self, id: &ProfileId, changes: Vec<(String, bool)>) {
        for (ws, on) in changes {
            let cmd = if on {
                ServersCommand::Add {
                    id: ws,
                    profiles: vec![id.clone()],
                }
            } else {
                ServersCommand::Remove {
                    id: ws,
                    profiles: vec![id.clone()],
                }
            };
            self.core.send(Command::Servers(cmd));
        }
    }

    /// The number of items workspace `w` shows (credentials aside).
    pub(crate) fn ws_item_count(&self, w: &SavedWorkspace) -> usize {
        scope_profiles(&self.profiles.all, &w.scope)
            .iter()
            .filter(|p| p.is_connection())
            .count()
    }

    /// A text field for a dialog.
    pub(crate) fn text_input(
        &self,
        value: &str,
        placeholder: &str,
        masked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let (value, placeholder): (SharedString, SharedString) =
            (value.to_owned().into(), placeholder.to_owned().into());
        cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .masked(masked)
                .default_value(value)
        })
    }
}

impl Workspace {
    /// The Default section of the title bar's workspace menu: every server workspace
    /// (click to switch, ⋯ for its settings), New and Import.
    pub(crate) fn render_workspace_menu(
        &self,
        p: &crate::theme::Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        use crate::appearance::{rpx, ts};
        use gpui_kit::prelude::FluentBuilder as _;
        use gpui_kit::{
            FontWeight, InteractiveElement as _, IntoElement as _, ParentElement as _,
            StatefulInteractiveElement as _, Styled as _, div, px,
        };
        let p = *p;
        let default = self.mode == AppMode::Default;
        let rows = self.servers.workspaces.iter().enumerate().map(|(i, w)| {
            let on = default && w.id == self.servers.current;
            let (id, set_id) = (w.id.clone(), w.id.clone());
            let count = self.ws_item_count(w);
            div()
                .id(("ws-menu", i))
                .flex()
                .items_center()
                .gap(rpx(6.))
                .px(rpx(10.))
                .py(rpx(5.))
                .rounded(px(5.))
                .cursor_pointer()
                .when(on, |d| d.bg(p.sel))
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(move |this, _, w, cx| this.switch_workspace(&id, w, cx)))
                .child(
                    div()
                        .w(rpx(12.))
                        .text_color(p.acc)
                        .child(if on { "✓" } else { "" }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(ts::UI)
                        .font_weight(FontWeight::MEDIUM)
                        .child(w.name.clone()),
                )
                .child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child(count.to_string()),
                )
                .child(
                    div()
                        .id(("ws-menu-set", i))
                        .px(rpx(5.))
                        .rounded(px(4.))
                        .text_color(p.fg3)
                        .hover(|s| s.bg(p.bd).text_color(p.fg))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            cx.stop_propagation();
                            this.open_workspace_settings(&set_id, w, cx)
                        }))
                        .child("⋯"),
                )
        });
        let action =
            |id: &'static str,
             label: &'static str,
             cx: &mut Context<Self>,
             f: fn(&mut Workspace, &mut Window, &mut Context<Workspace>)| {
                div()
                    .id(id)
                    .px(rpx(10.))
                    .py(rpx(5.))
                    .pl(rpx(28.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_size(ts::UI)
                    .text_color(p.fg2)
                    .hover(|s| s.bg(p.hover).text_color(p.fg))
                    .on_click(cx.listener(move |this, _, w, cx| f(this, w, cx)))
                    .child(label)
            };
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(rpx(10.))
                    .pt(rpx(4.))
                    .pb(rpx(2.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(ts::UI)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Default"),
                    )
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child("Databases, terminals, files and tunnels · by workspace"),
                    ),
            )
            .children(rows)
            .child(action(
                "ws-menu-new",
                "New workspace…",
                cx,
                |this, w, cx| this.open_new_workspace(None, w, cx),
            ))
            .child(action(
                "ws-menu-import",
                "Import workspace…",
                cx,
                |this, _, cx| {
                    this.mode_menu = false;
                    this.import_workspace(cx)
                },
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::Engine;
    use switchyard_core::store::{DbConnection, DesktopProtocol, DesktopServer, Host};

    fn host(name: &str, folder: Option<&str>, tags: &[&str]) -> Profile {
        let mut h = Host::new(name, "10.0.0.1", "root");
        h.folder = folder.map(str::to_owned);
        h.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        Profile::Host(h)
    }

    fn db_on(name: &str, host: Option<&Profile>) -> Profile {
        let mut d = DbConnection::new(name, Engine::Postgres);
        d.via_host = host.map(|h| h.id().clone());
        Profile::Db(d)
    }

    fn names(list: &[Profile]) -> Vec<&str> {
        list.iter().map(|p| p.name()).collect()
    }

    #[test]
    fn a_scope_shows_members_rules_and_what_lives_on_member_hosts() {
        let web = host("web", Some("Prod"), &[]);
        let lab = host("lab", None, &["lab"]);
        let jump = host("jump", None, &[]);
        let mut bastioned = host("db-host", None, &[]);
        if let Profile::Host(h) = &mut bastioned {
            h.jump_hosts = vec![jump.id().clone()];
        }
        let shop = db_on("shop", Some(&web));
        let direct = db_on("direct", None);
        let mut rdp = DesktopServer::new("dc", DesktopProtocol::Rdp);
        rdp.via_host = Some(lab.id().clone());
        let rdp = Profile::Desktop(rdp);
        let all = vec![
            web.clone(),
            lab.clone(),
            jump.clone(),
            bastioned.clone(),
            shop.clone(),
            direct.clone(),
            rdp.clone(),
        ];

        let everything = scope_profiles(&all, &WorkspaceScope::everything());
        assert_eq!(everything.len(), all.len());

        // A folder rule brings the folder's Host and the database on it.
        let prod = WorkspaceScope {
            folders: vec!["Prod".into()],
            ..Default::default()
        };
        assert_eq!(names(&scope_profiles(&all, &prod)), ["web", "shop"]);

        // A member that needs a jump Host shows it too; a member desktop its tunnel Host.
        let picked = WorkspaceScope {
            members: vec![bastioned.id().clone(), rdp.id().clone()],
            ..Default::default()
        };
        assert_eq!(
            names(&scope_profiles(&all, &picked)),
            ["lab", "jump", "db-host", "dc"]
        );

        // An excluded child stays out even though its Host is in.
        let mut no_shop = prod.clone();
        no_shop.excluded = vec![shop.id().clone()];
        assert_eq!(names(&scope_profiles(&all, &no_shop)), ["web"]);
    }

    #[test]
    fn rules_from_menus_add_and_remove() {
        let a = host("a", Some("Prod"), &["web"]);
        let b = host("b", Some("Prod/eu"), &[]);
        let c = host("c", None, &["web"]);
        let all = vec![a.clone(), b.clone(), c.clone()];
        let empty = WorkspaceScope::default();

        let s = scope_with_rule(&empty, &WsTarget::Folder("Prod".into()), true, &all);
        assert_eq!(s.folders, ["Prod"]);
        assert_eq!(names(&scope_profiles(&all, &s)), ["a", "b"]);

        // Removing one item excludes it (the rule still matches); adding it back lets it in.
        let s2 = scope_with_rule(&s, &WsTarget::Items(vec![b.id().clone()]), false, &all);
        assert_eq!(names(&scope_profiles(&all, &s2)), ["a"]);
        let s3 = scope_with_rule(&s2, &WsTarget::Items(vec![b.id().clone()]), true, &all);
        assert_eq!(names(&scope_profiles(&all, &s3)), ["a", "b"]);

        // A tag rule; removing it again leaves nothing behind.
        let t = scope_with_rule(&empty, &WsTarget::Tag("Web".into()), true, &all);
        assert_eq!(names(&scope_profiles(&all, &t)), ["a", "c"]);
        let t2 = scope_with_rule(&t, &WsTarget::Tag("web".into()), false, &all);
        assert!(scope_profiles(&all, &t2).is_empty());

        // A new workspace from a target.
        assert_eq!(WsTarget::Tag("web".into()).scope().tags, ["web"]);
        assert_eq!(
            WsTarget::Items(vec![c.id().clone()]).scope().members,
            [c.id().clone()]
        );
    }

    #[test]
    fn sidebar_page_keys_round_trip() {
        for t in [
            SideTab::Explorer,
            SideTab::Schema,
            SideTab::Tools,
            SideTab::Activity,
        ] {
            assert_eq!(side_tab_from_key(side_tab_key(t)), Some(t));
        }
        assert_eq!(side_tab_from_key("?"), None);
    }
}
