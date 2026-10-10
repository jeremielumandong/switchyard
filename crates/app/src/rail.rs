//! The window chrome of design v3, built for many kinds of sources: the activity rail
//! (Explorer, Schema, Tools, Activity, Settings), the Tools and Activity sidebar panes,
//! the title bar's menus and the "What do you want to connect to?" chooser.
//!
//! Every menu entry and tool is a [`CommandId`], so the menus, the Tools pane and the
//! palette run the same commands.

use std::borrow::Cow;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AssetSource, Context, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    div, px, svg,
};
use switchyard_core::db::Engine;
use switchyard_core::remote::ssh::TunnelStatus;
use switchyard_core::store::{CloudProvider, CloudService, Profile};

use crate::actions::CommandId;
use crate::app_state::Profiles;
use crate::appearance::{rpx, ts};
use crate::explorer_tree::ExplorerGroup;
use crate::sidebar::SideTab;
use crate::theme::{MONO, Palette, SANS};
use crate::ui;
use crate::workspace::{AppMode, Tab, Workspace};

// ------------------------------------------------------------------ assets

/// The app's own icons (Lucide, ISC: `assets/icons/LICENSE-LUCIDE`), served before
/// gpui-kit's component set.
const ICONS: [(&str, &[u8]); 5] = [
    (
        "icons/swy-layers.svg",
        include_bytes!("../assets/icons/swy-layers.svg"),
    ),
    (
        "icons/swy-database.svg",
        include_bytes!("../assets/icons/swy-database.svg"),
    ),
    (
        "icons/swy-wrench.svg",
        include_bytes!("../assets/icons/swy-wrench.svg"),
    ),
    (
        "icons/swy-activity.svg",
        include_bytes!("../assets/icons/swy-activity.svg"),
    ),
    (
        "icons/swy-sliders-horizontal.svg",
        include_bytes!("../assets/icons/swy-sliders-horizontal.svg"),
    ),
];

/// gpui-kit's assets plus [`ICONS`].
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        match ICONS.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut out = gpui_kit::assets::Assets.list(path)?;
        out.extend(
            ICONS
                .iter()
                .filter(|(p, _)| p.starts_with(path))
                .map(|(p, _)| SharedString::from(*p)),
        );
        Ok(out)
    }
}

// ------------------------------------------------------------------- menus

/// A title bar menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopMenu {
    /// New, import, open, close, settings.
    File,
    /// Sidebar panes, grouping, panels, zoom, theme.
    View,
    /// Open anything, tabs, workspaces.
    Go,
    /// The SQL tab in front: run, explain, transactions.
    Query,
    /// The terminal in front.
    Terminal,
    /// Welcome, shortcuts, updates, logs.
    Help,
}

impl TopMenu {
    /// Label in the bar.
    pub fn label(self) -> &'static str {
        match self {
            TopMenu::File => "File",
            TopMenu::View => "View",
            TopMenu::Go => "Go",
            TopMenu::Query => "Query",
            TopMenu::Terminal => "Terminal",
            TopMenu::Help => "Help",
        }
    }
}

/// One line of a menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuLine {
    /// A command: label, shortcut, command, check mark.
    Item(&'static str, SharedString, CommandId, bool),
    /// A small heading.
    Head(&'static str),
    /// A divider.
    Sep,
}

/// What the workspace looks like, for the menus' check marks.
#[derive(Clone, Copy, Debug, Default)]
pub struct MenuState {
    /// Sidebar shown.
    pub sidebar_open: bool,
    /// Sidebar pane.
    pub side: Option<SideTab>,
    /// Explorer grouping.
    pub group: ExplorerGroup,
    /// Assistant shown.
    pub assistant: bool,
    /// Value inspector shown.
    pub inspector: bool,
    /// API workspace in front.
    pub api: bool,
}

fn k(mac: &str, other: &str) -> SharedString {
    ui::keys(mac, other)
}

/// The menus for the tab in front: File, View, Go, then Query or Terminal when one is
/// in front, then Help.
pub fn menus_for(front: Option<TopMenu>) -> Vec<TopMenu> {
    let mut v = vec![TopMenu::File, TopMenu::View, TopMenu::Go];
    v.extend(front);
    v.push(TopMenu::Help);
    v
}

/// A menu's lines.
pub fn menu_lines(menu: TopMenu, s: MenuState) -> Vec<MenuLine> {
    use CommandId::*;
    use MenuLine::{Head, Item, Sep};
    let none = SharedString::default;
    let side = |t: SideTab| s.sidebar_open && s.side == Some(t);
    match menu {
        TopMenu::File => vec![
            Item("New…", k("⌘N", "Ctrl+N"), NewChooser, false),
            Item("New SQL tab", k("⌥⌘N", "Ctrl+Alt+N"), NewQueryTab, false),
            Item("New terminal", k("⌘T", "Ctrl+Shift+T"), NewTerminal, false),
            Sep,
            Item("Import from ~/.ssh/config…", none(), ImportSshConfig, false),
            Item(
                "Export profiles (no secrets)…",
                none(),
                ExportProfiles,
                false,
            ),
            Sep,
            Item("Open anything…", k("⌘P", "Ctrl+P"), OpenAnything, false),
            Item("Close tab", k("⌘W", "Ctrl+W"), CloseTab, false),
            Sep,
            Item("Settings…", k("⌘,", "Ctrl+,"), Settings, false),
        ],
        TopMenu::View => vec![
            Item("Explorer", none(), ShowExplorer, side(SideTab::Explorer)),
            Item("Schema", none(), ShowSchema, side(SideTab::Schema)),
            Item("Tools", none(), ShowTools, side(SideTab::Tools)),
            Item(
                "Activity",
                none(),
                ShowActivityPane,
                side(SideTab::Activity),
            ),
            Item(
                "Hide sidebar",
                k("⌘B", "Ctrl+B"),
                ToggleSidebar,
                !s.sidebar_open,
            ),
            Sep,
            Head("Group Explorer by"),
            Item(
                "Place",
                none(),
                GroupByPlace,
                s.group == ExplorerGroup::Place,
            ),
            Item("Type", none(), GroupByType, s.group == ExplorerGroup::Type),
            Sep,
            Item("Assistant", k("⌘J", "Ctrl+J"), ToggleAssistant, s.assistant),
            Item("Value inspector", none(), ToggleInspector, s.inspector),
            Item("Split right", k("⌘\\", "Ctrl+\\"), SplitRight, false),
            Item("Split down", k("⇧⌘\\", "Ctrl+Shift+\\"), SplitDown, false),
            Sep,
            Item("Zoom in", k("⌘=", "Ctrl+="), ZoomIn, false),
            Item("Zoom out", k("⌘-", "Ctrl+-"), ZoomOut, false),
            Item("Reset zoom", k("⌘0", "Ctrl+0"), ResetZoom, false),
            Item("Toggle light / dark theme", none(), ToggleTheme, false),
        ],
        TopMenu::Go => vec![
            Item("Open anything…", k("⌘P", "Ctrl+P"), OpenAnything, false),
            Item(
                "Command palette…",
                k("⇧⌘P", "Ctrl+Shift+P"),
                OpenCommands,
                false,
            ),
            Sep,
            Item("Next tab", k("⌃⇥", "Ctrl+Tab"), NextTab, false),
            Item("Previous tab", k("⌃⇧⇥", "Ctrl+Shift+Tab"), PrevTab, false),
            Sep,
            Head("Workspace"),
            Item("Default", none(), SwitchToDefault, !s.api),
            Item("API", none(), SwitchToApi, s.api),
        ],
        TopMenu::Query => vec![
            Item("Run statement", k("⌘↵", "Ctrl+Enter"), RunStatement, false),
            Item("Run script", k("⇧⌘↵", "Ctrl+Shift+Enter"), RunScript, false),
            Item("Stop", k("⌘.", "Ctrl+."), StopQuery, false),
            Item("Explain", k("⌘E", "Ctrl+E"), Explain, false),
            Item(
                "Explain analyze",
                k("⇧⌘E", "Ctrl+Shift+E"),
                ExplainAnalyze,
                false,
            ),
            Item("Format SQL", k("⇧⌘F", "Ctrl+Shift+F"), FormatSql, false),
            Sep,
            Head("Transactions"),
            Item("Commit", none(), CommitTransaction, false),
            Item("Rollback", none(), RollbackTransaction, false),
            Sep,
            Item(
                "Query history",
                k("⇧⌘H", "Ctrl+Shift+H"),
                ShowHistory,
                false,
            ),
            Item("Workload statistics", none(), ShowWorkload, false),
            Item("Activity monitor", none(), ShowActivity, false),
            Item("Refresh schema", none(), RefreshSchema, false),
        ],
        TopMenu::Terminal => vec![
            Item("New terminal", k("⌘T", "Ctrl+Shift+T"), NewTerminal, false),
            Item("Split right", k("⌘\\", "Ctrl+\\"), SplitRight, false),
            Item("Split down", k("⇧⌘\\", "Ctrl+Shift+\\"), SplitDown, false),
            Item("Close split", none(), Unsplit, false),
            Sep,
            Item("Port forwards", none(), ShowActivityPane, false),
        ],
        TopMenu::Help => vec![
            Item("Welcome", none(), ShowWelcome, false),
            Item("Keyboard shortcuts", none(), SettingsKeybindings, false),
            Sep,
            Item("Check for updates…", none(), CheckForUpdates, false),
            Item("Open log folder", none(), OpenLogFolder, false),
        ],
    }
}

// ------------------------------------------------------------------- tools

/// One tool: a small, focused feature that reuses its group's sign-in.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// Monogram.
    pub badge: &'static str,
    /// Name.
    pub name: &'static str,
    /// One line on what it does.
    pub desc: &'static str,
    /// What a click runs.
    pub cmd: CommandId,
    /// `Open`, or `Add` when nothing of it is saved yet.
    pub tag: &'static str,
}

/// Tools of one place: servers and databases, or one cloud provider.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolGroup {
    /// Monogram.
    pub badge: &'static str,
    /// Name.
    pub name: &'static str,
    /// What is saved there.
    pub ident: String,
    /// Something of it is saved.
    pub ready: bool,
    /// Its tools.
    pub tools: Vec<Tool>,
}

/// What a cloud service is for, in the Tools pane.
fn service_desc(s: CloudService) -> &'static str {
    match s {
        CloudService::S3 => "Buckets, objects, versions and presigned links",
        CloudService::R2 => "Buckets and objects on Cloudflare",
        CloudService::AzureBlob => "Containers, blobs and SAS links",
        CloudService::AppConfig => "Key-values, labels and feature flags",
        CloudService::KeyVault => "Reveal, copy and version secrets",
        CloudService::SecretsManager => "Reveal on demand, versions",
        CloudService::ParameterStore => "Browse and edit parameters",
        CloudService::WorkersKv => "Namespaces, keys, values and TTLs",
    }
}

/// The Tools pane's groups: servers and databases first, then each cloud provider
/// with one tool per service (`Open` when one is saved, else `Add`).
pub fn tool_groups(profiles: &Profiles) -> Vec<ToolGroup> {
    let hosts = profiles.hosts().count();
    let dbs = profiles.dbs().count();
    let mut out = vec![ToolGroup {
        badge: "SY",
        name: "Servers & databases",
        ident: format!(
            "{hosts} server{} · {dbs} database{}",
            if hosts == 1 { "" } else { "s" },
            if dbs == 1 { "" } else { "s" }
        ),
        ready: true,
        tools: vec![
            Tool {
                badge: "TUN",
                name: "Port forwards",
                desc: "Every tunnel across servers: start, stop, inspect",
                cmd: CommandId::ShowActivityPane,
                tag: "Open",
            },
            Tool {
                badge: "SSH",
                name: "Import ~/.ssh/config",
                desc: "Add the hosts your SSH config already knows",
                cmd: CommandId::ImportSshConfig,
                tag: "Open",
            },
            Tool {
                badge: "HIS",
                name: "Query history",
                desc: "Every statement you and agents ran",
                cmd: CommandId::ShowHistory,
                tag: "Open",
            },
            Tool {
                badge: "WL",
                name: "Workload statistics",
                desc: "Top queries and index usage of a database",
                cmd: CommandId::ShowWorkload,
                tag: "Open",
            },
            Tool {
                badge: "AM",
                name: "Activity monitor",
                desc: "Sessions and running queries on a server",
                cmd: CommandId::ShowActivity,
                tag: "Open",
            },
            Tool {
                badge: "SN",
                name: "Snippets",
                desc: "Saved SQL with placeholders",
                cmd: CommandId::ManageSnippets,
                tag: "Open",
            },
            Tool {
                badge: "SV",
                name: "Secret vault",
                desc: "Named secrets, local or from Key Vault",
                cmd: CommandId::ManageSecrets,
                tag: "Open",
            },
            Tool {
                badge: "DRV",
                name: "Drivers",
                desc: "Optional native components",
                cmd: CommandId::SettingsDrivers,
                tag: "Open",
            },
        ],
    }];
    for (provider, badge) in [
        (CloudProvider::Aws, "AWS"),
        (CloudProvider::Azure, "AZ"),
        (CloudProvider::Cloudflare, "CF"),
    ] {
        let saved = |s: CloudService| {
            profiles
                .all
                .iter()
                .any(|p| matches!(p, Profile::Cloud(c) if c.service == s))
        };
        let mut tools: Vec<Tool> = CloudService::ALL
            .into_iter()
            .filter(|s| s.provider() == provider)
            .map(|s| Tool {
                badge: s.badge(),
                name: s.short_name(),
                desc: service_desc(s),
                cmd: CommandId::OpenCloud(s),
                tag: if saved(s) { "Open" } else { "Add" },
            })
            .collect();
        if provider == CloudProvider::Cloudflare {
            for (e, desc) in [
                (Engine::D1, "Opens as a database connection"),
                (Engine::DurableObject, "One Durable Object's SQLite storage"),
            ] {
                tools.push(Tool {
                    badge: e.badge(),
                    name: e.display_name(),
                    desc,
                    cmd: CommandId::OpenEngine(e),
                    tag: if profiles.dbs().any(|d| d.engine == e) {
                        "Open"
                    } else {
                        "Add"
                    },
                });
            }
        }
        let n = tools.iter().filter(|t| t.tag == "Open").count();
        out.push(ToolGroup {
            badge,
            name: provider.display_name(),
            ident: if n == 0 {
                "Nothing added yet".into()
            } else {
                format!("{n} service{} saved", if n == 1 { "" } else { "s" })
            },
            ready: n > 0,
            tools,
        });
    }
    out
}

// --------------------------------------------------------------- activity

/// One row of the Activity pane.
struct ActRow {
    badge: &'static str,
    label: SharedString,
    sub: SharedString,
    color: gpui_kit::Hsla,
    /// Tab to bring to front.
    tab: Option<usize>,
    /// Open the Files tab (transfers).
    files: bool,
    /// Show the tunnels popover.
    tunnels: bool,
}

impl Workspace {
    /// Bring a sidebar pane to front; clicking the one in front hides the sidebar.
    pub(crate) fn show_side(&mut self, t: SideTab, toggle: bool, cx: &mut Context<Self>) {
        if toggle && self.sidebar_open && self.side_tab == t {
            self.sidebar_open = false;
        } else {
            self.side_tab = t;
            self.sidebar_open = true;
        }
        cx.notify();
    }

    /// The 48px rail left of the sidebar.
    pub(crate) fn render_rail(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use gpui_kit::component::tooltip::Tooltip;
        let alert = self
            .tunnels
            .iter()
            .any(|t| matches!(t.status, TunnelStatus::Failed(_)))
            || self
                .transfers
                .read(cx)
                .items
                .iter()
                .any(|i| matches!(i.state, crate::transfers::State::Failed(_)));
        let item = |id: &'static str,
                    icon: &'static str,
                    tip: &'static str,
                    tab: SideTab,
                    badge: bool,
                    cx: &mut Context<Self>| {
            let on = self.sidebar_open && self.side_tab == tab;
            div()
                .id(id)
                .relative()
                .size(rpx(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .cursor_pointer()
                .when(on, |d| d.bg(p.hover))
                .hover(|s| s.bg(p.hover))
                .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
                .on_click(cx.listener(move |this, _, _, cx| this.show_side(tab, true, cx)))
                .child(
                    div()
                        .absolute()
                        .left(rpx(-6.))
                        .top(rpx(9.))
                        .bottom(rpx(9.))
                        .w(rpx(2.))
                        .rounded(px(2.))
                        .when(on, |d| d.bg(p.fg)),
                )
                .child(
                    svg()
                        .path(icon)
                        .size(rpx(18.))
                        .text_color(if on { p.fg } else { p.fg3 }),
                )
                .when(badge, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top(rpx(5.))
                            .right(rpx(5.))
                            .size(rpx(7.))
                            .rounded_full()
                            .bg(p.prod),
                    )
                })
        };
        let explorer = item(
            "rail-explorer",
            "icons/swy-layers.svg",
            "Explorer",
            SideTab::Explorer,
            false,
            cx,
        );
        let schema = item(
            "rail-schema",
            "icons/swy-database.svg",
            "Schema",
            SideTab::Schema,
            false,
            cx,
        );
        let tools = item(
            "rail-tools",
            "icons/swy-wrench.svg",
            "Tools",
            SideTab::Tools,
            false,
            cx,
        );
        let activity = item(
            "rail-activity",
            "icons/swy-activity.svg",
            "Activity",
            SideTab::Activity,
            alert,
            cx,
        );
        div()
            .w(rpx(48.))
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(rpx(4.))
            .py(rpx(8.))
            .bg(p.panel)
            .border_r_1()
            .border_color(p.bd)
            .child(explorer)
            .child(schema)
            .child(tools)
            .child(activity)
            .child(div().flex_1())
            .child(
                div()
                    .id("rail-settings")
                    .size(rpx(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .tooltip(|w, cx| {
                        Tooltip::new("Settings")
                            .action(&crate::actions::OpenSettings, None)
                            .build(w, cx)
                    })
                    .on_click(
                        cx.listener(|this, _, w, cx| this.run_command(CommandId::Settings, w, cx)),
                    )
                    .child(
                        svg()
                            .path("icons/swy-sliders-horizontal.svg")
                            .size(rpx(18.))
                            .text_color(p.fg3),
                    ),
            )
            .into_any_element()
    }

    /// The Tools pane: per place, the tools it offers.
    pub(crate) fn render_tools_pane(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let groups = tool_groups(&self.profiles);
        let mut list = div()
            .id("tools-pane")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb(rpx(8.));
        let mut n = 0usize;
        for g in groups {
            list = list.child(
                div()
                    .h(rpx(28.))
                    .pt(rpx(8.))
                    .px(rpx(12.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .text_size(ts::SMALL)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg3)
                    .child(ui::dot(if g.ready { p.dev } else { p.fg3 }, 6.))
                    .child(div().flex_1().truncate().child(g.name.to_uppercase())),
            );
            for t in g.tools {
                let cmd = t.cmd;
                n += 1;
                list = list.child(
                    div()
                        .id(("tool", n))
                        .h(rpx(26.))
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .pl(rpx(14.))
                        .pr(rpx(12.))
                        .text_size(ts::UI)
                        .cursor_pointer()
                        .hover(|s| s.bg(p.hover))
                        .on_click(cx.listener(move |this, _, w, cx| this.run_command(cmd, w, cx)))
                        .child(ui::monogram(t.badge, 28., p))
                        .child(div().flex_1().min_w_0().truncate().child(t.name))
                        .child(
                            div()
                                .text_size(ts::SMALL)
                                .text_color(if t.tag == "Open" { p.acc } else { p.fg3 })
                                .child(t.tag),
                        ),
                );
            }
        }
        list.into_any_element()
    }

    fn activity_rows(&self, p: &Palette, cx: &Context<Self>) -> Vec<(&'static str, Vec<ActRow>)> {
        let mut sessions = Vec::new();
        let mut running = Vec::new();
        for (i, t) in self.tabs.iter().enumerate() {
            let (badge, title, _, _) = self.tab_info(t, cx);
            let badge: &'static str = match badge.as_ref() {
                "SSH" => "SSH",
                "SH" => "SH",
                _ => "",
            };
            match t {
                Tab::Terminal(term) => sessions.push(ActRow {
                    badge: if badge.is_empty() { "SH" } else { badge },
                    label: title,
                    sub: term.read(cx).status().into(),
                    color: p.dev,
                    tab: Some(i),
                    files: false,
                    tunnels: false,
                }),
                Tab::Sql(s) => {
                    let s = s.read(cx);
                    let Some(c) = &s.connection else { continue };
                    let badge = c.engine.badge();
                    if let crate::app_state::SessionState::Open { version } = &s.session_state {
                        sessions.push(ActRow {
                            badge,
                            label: c.name.clone().into(),
                            sub: version.clone().into(),
                            color: p.dev,
                            tab: Some(i),
                            files: false,
                            tunnels: false,
                        });
                    }
                    let (label, color, meta, busy) = s.status(p);
                    if busy {
                        running.push(ActRow {
                            badge,
                            label: s.title.clone(),
                            sub: format!("{label} · {meta}").into(),
                            color,
                            tab: Some(i),
                            files: false,
                            tunnels: false,
                        });
                    }
                }
                Tab::Redis(r) => {
                    let r = r.read(cx);
                    if r.is_open() {
                        sessions.push(ActRow {
                            badge: r.connection.engine.badge(),
                            label: r.connection.name.clone().into(),
                            sub: "connected".into(),
                            color: p.dev,
                            tab: Some(i),
                            files: false,
                            tunnels: false,
                        });
                    }
                }
                Tab::Cloud(c) => {
                    let c = c.read(cx);
                    if c.is_open() {
                        sessions.push(ActRow {
                            badge: c.connection.service.badge(),
                            label: c.connection.name.clone().into(),
                            sub: c.connection.service.provider().display_name().into(),
                            color: p.dev,
                            tab: Some(i),
                            files: false,
                            tunnels: false,
                        });
                    }
                }
                _ => {}
            }
        }
        let tunnels = self
            .tunnels
            .iter()
            .map(|t| {
                let (sub, color) = match &t.status {
                    TunnelStatus::Active => ("Active", p.dev),
                    TunnelStatus::Reconnecting => ("Reconnecting", p.stg),
                    TunnelStatus::Failed(_) => ("Failed", p.prod),
                    TunnelStatus::Stopped => ("Stopped", p.fg3),
                };
                ActRow {
                    badge: "TUN",
                    label: format!(":{}  {} → {}", t.local_port, t.host, t.remote).into(),
                    sub: sub.into(),
                    color,
                    tab: None,
                    files: false,
                    tunnels: true,
                }
            })
            .collect();
        use crate::transfers::State;
        let transfers = self
            .transfers
            .read(cx)
            .items
            .iter()
            .rev()
            .filter(|i| i.state != State::Cancelled)
            .map(|i| {
                let pct = i
                    .total
                    .filter(|t| *t > 0)
                    .map(|t| (i.done.saturating_mul(100) / t).min(100));
                let (state, color) = match &i.state {
                    State::Queued => ("queued", p.fg3),
                    State::Running if i.upload() => ("uploading", p.acc),
                    State::Running => ("downloading", p.acc),
                    State::Paused => ("paused", p.stg),
                    State::Done(_) => ("done", p.dev),
                    State::Failed(_) => ("failed", p.prod),
                    State::Exists | State::Partial(_) => ("needs an answer", p.stg),
                    State::Cancelled => ("cancelled", p.fg3),
                };
                ActRow {
                    badge: if i.upload() { "↑" } else { "↓" },
                    label: i.name.clone().into(),
                    sub: match pct {
                        Some(n) if !matches!(i.state, State::Done(_)) => format!("{n}% · {state}"),
                        _ => state.to_owned(),
                    }
                    .into(),
                    color,
                    tab: None,
                    files: true,
                    tunnels: false,
                }
            })
            .collect();
        vec![
            ("SESSIONS", sessions),
            ("RUNNING", running),
            ("TUNNELS", tunnels),
            ("TRANSFERS", transfers),
        ]
    }

    /// The Activity pane: open sessions, running queries, tunnels and transfers.
    pub(crate) fn render_activity_pane(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let sections = self.activity_rows(p, cx);
        let mut list = div()
            .id("activity-pane")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb(rpx(8.));
        let mut n = 0usize;
        for (title, rows) in sections {
            let key = format!("act:{title}");
            let collapsed = self.collapsed.contains(&key);
            let count = rows.len();
            let toggle_key = key.clone();
            list = list.child(
                div()
                    .id(SharedString::from(key))
                    .h(rpx(30.))
                    .pt(rpx(8.))
                    .pl(rpx(12.))
                    .pr(rpx(10.))
                    .flex()
                    .items_center()
                    .text_size(ts::SMALL)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg3)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&toggle_key) {
                            this.collapsed.insert(toggle_key.clone());
                        }
                        cx.notify();
                    }))
                    .child(div().flex_1().child(title))
                    .child(
                        div()
                            .font_family(MONO)
                            .font_weight(FontWeight::NORMAL)
                            .child(if collapsed {
                                "show".to_owned()
                            } else {
                                count.to_string()
                            }),
                    ),
            );
            if collapsed {
                continue;
            }
            if rows.is_empty() {
                list = list.child(
                    div()
                        .pl(rpx(26.))
                        .h(rpx(24.))
                        .flex()
                        .items_center()
                        .text_size(ts::BODY)
                        .text_color(p.fg3)
                        .child(match title {
                            "SESSIONS" => "Nothing connected",
                            "RUNNING" => "No queries running",
                            "TUNNELS" => "No tunnels",
                            _ => "No transfers",
                        }),
                );
            }
            for r in rows {
                n += 1;
                let (tab, files, tunnels) = (r.tab, r.files, r.tunnels);
                list = list.child(
                    div()
                        .id(("act-row", n))
                        .h(rpx(26.))
                        .flex()
                        .items_center()
                        .gap(rpx(7.))
                        .pl(rpx(18.))
                        .pr(rpx(10.))
                        .text_size(ts::UI)
                        .cursor_pointer()
                        .hover(|s| s.bg(p.hover))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            if let Some(i) = tab {
                                this.activate(i, cx);
                            } else if files {
                                this.open_files(w, cx);
                            } else if tunnels {
                                this.tunnels_open = true;
                            }
                            cx.notify();
                        }))
                        .child(ui::dot(r.color, 7.))
                        .child(ui::monogram(r.badge, 26., p))
                        .child(div().flex_1().min_w_0().truncate().child(r.label))
                        .child(
                            div()
                                .max_w(gpui_kit::relative(0.46))
                                .truncate()
                                .font_family(MONO)
                                .text_size(ts::SMALL)
                                .text_color(r.color)
                                .child(r.sub),
                        ),
                );
            }
        }
        list.into_any_element()
    }

    /// `2 tunnels · 1 running · 3 transferring`, for the status bar.
    pub(crate) fn activity_summary(&self, cx: &Context<Self>) -> String {
        let tunnels = self
            .tunnels
            .iter()
            .filter(|t| !matches!(t.status, TunnelStatus::Failed(_) | TunnelStatus::Stopped))
            .count();
        let running = self
            .tabs
            .iter()
            .filter(|t| match t {
                Tab::Sql(s) => s.read(cx).status(&crate::theme::palette(cx)).3,
                _ => false,
            })
            .count();
        let moving = self
            .transfers
            .read(cx)
            .items
            .iter()
            .filter(|i| i.state == crate::transfers::State::Running)
            .count();
        format!(
            "{tunnels} tunnel{} · {running} running · {moving} transferring",
            if tunnels == 1 { "" } else { "s" }
        )
    }

    // ---------------------------------------------------------- title menus

    fn menu_state(&self) -> MenuState {
        MenuState {
            sidebar_open: self.sidebar_open,
            side: Some(self.side_tab),
            group: self.explorer_group,
            assistant: self.assistant_open,
            inspector: self.inspector_open,
            api: self.mode == AppMode::Api,
        }
    }

    /// The menu that belongs to the tab in front.
    fn front_menu(&self) -> Option<TopMenu> {
        match self.tabs.get(self.active) {
            Some(Tab::Sql(_)) => Some(TopMenu::Query),
            Some(Tab::Terminal(_)) => Some(TopMenu::Terminal),
            _ => None,
        }
    }

    /// File, View, Go, the tab's own menu and Help, for the title bar.
    pub(crate) fn render_menu_bar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let front = if self.mode == AppMode::Default {
            self.front_menu()
        } else {
            None
        };
        let state = self.menu_state();
        let open = self.top_menu;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(rpx(1.))
            .children(menus_for(front).into_iter().map(|m| {
                let is_open = open == Some(m);
                let lines = is_open.then(|| menu_lines(m, state));
                div()
                    .relative()
                    .child(
                        div()
                            .id(m.label())
                            .h(rpx(26.))
                            .px(rpx(8.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .occlude()
                            .cursor_pointer()
                            .text_size(ts::UI)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if is_open || Some(m) == front {
                                p.fg
                            } else {
                                p.fg2
                            })
                            .when(is_open, |d| d.bg(p.hover))
                            .hover(|s| s.bg(p.hover).text_color(p.fg))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.top_menu = if this.top_menu == Some(m) {
                                        None
                                    } else {
                                        Some(m)
                                    };
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            )
                            // Moving across the bar with a menu open switches menus.
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered && this.top_menu.is_some_and(|o| o != m) {
                                    this.top_menu = Some(m);
                                    cx.notify();
                                }
                            }))
                            .child(m.label()),
                    )
                    .when_some(lines, |d, lines| d.child(self.render_menu(lines, p, cx)))
            }))
            .into_any_element()
    }

    fn render_menu(&self, lines: Vec<MenuLine>, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = div()
            .id("top-menu")
            .absolute()
            .top(rpx(30.))
            .left_0()
            .min_w(rpx(270.))
            .p(rpx(4.))
            .bg(p.elev)
            .border_1()
            .border_color(p.bd)
            .rounded(px(8.))
            .shadow(ui::shadow(p))
            .font_family(SANS)
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.top_menu = None;
                cx.notify();
            }));
        for (i, line) in lines.into_iter().enumerate() {
            panel = match line {
                MenuLine::Sep => panel.child(div().my(rpx(4.)).h(rpx(1.)).bg(p.bd)),
                MenuLine::Head(h) => panel.child(
                    div()
                        .pt(rpx(6.))
                        .pb(rpx(2.))
                        .pl(rpx(30.))
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child(h),
                ),
                MenuLine::Item(label, key, cmd, check) => panel.child(
                    div()
                        .id(("menu-item", i))
                        .h(rpx(26.))
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .px(rpx(8.))
                        .rounded(px(5.))
                        .text_size(ts::UI)
                        .text_color(p.fg)
                        .cursor_pointer()
                        .hover(|s| s.bg(p.sel))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.top_menu = None;
                            this.run_command(cmd, w, cx);
                        }))
                        .child(
                            div()
                                .w(rpx(14.))
                                .flex_none()
                                .text_color(p.acc)
                                .text_size(ts::SMALL)
                                .child(if check { "✓" } else { "" }),
                        )
                        .child(div().flex_1().whitespace_nowrap().child(label))
                        .child(
                            div()
                                .font_family(MONO)
                                .text_size(ts::CAPTION_PLUS)
                                .text_color(p.fg3)
                                .whitespace_nowrap()
                                .child(key),
                        ),
                ),
            };
        }
        gpui_kit::deferred(panel)
            .with_priority(3)
            .into_any_element()
    }

    // ---------------------------------------------------------- new chooser

    /// "What do you want to connect to?"
    pub(crate) fn open_new_chooser(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        self.top_menu = None;
        self.overlay = Some(crate::overlays::Overlay::NewChooser);
        window.focus(&self.overlay_focus, cx);
        cx.notify();
    }

    /// The chooser's dialog.
    pub(crate) fn render_new_chooser(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use crate::conn_editor::ConnKind;
        let options: [(&[&str], &str, &str, ConnKind); 4] = [
            (
                &["SSH"],
                "Server",
                "Terminal, SFTP files and database tunnels, all over one SSH login.",
                ConnKind::Ssh,
            ),
            (
                &["PG", "MS", "MY", "D1"],
                "Database",
                "PostgreSQL, SQL Server, MySQL, Oracle, SQLite, MongoDB, Redis or Cloudflare D1: direct, or through a saved server.",
                ConnKind::Db(Engine::Postgres),
            ),
            (
                &["SFTP", "FTP"],
                "File server",
                "SFTP on a saved server, or a standalone FTP / FTPS login.",
                ConnKind::Ftp,
            ),
            (
                &["AWS", "AZ", "CF"],
                "Cloud service",
                "Buckets, config, secrets and key-value stores on AWS, Azure or Cloudflare.",
                ConnKind::Cloud(CloudService::S3),
            ),
        ];
        let card = |i: usize,
                    (badges, title, desc, kind): (&[&str], &str, &str, ConnKind),
                    cx: &mut Context<Self>| {
            div()
                .id(("new-opt", i))
                .flex_1()
                .min_w_0()
                .p(rpx(14.))
                .flex()
                .flex_col()
                .gap(rpx(6.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(8.))
                .bg(p.panel)
                .cursor_pointer()
                .hover(|s| s.border_color(p.acc))
                .on_click(cx.listener(move |this, _, w, cx| {
                    this.overlay = None;
                    this.open_conn_editor(kind, None, w, cx);
                }))
                .child(
                    div()
                        .flex()
                        .gap(rpx(4.))
                        .children(badges.iter().map(|b| ui::monogram(*b, 30., p))),
                )
                .child(
                    div()
                        .text_size(ts::BASE_PLUS)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.to_owned()),
                )
                .child(
                    div()
                        .text_size(ts::BODY)
                        .text_color(p.fg2)
                        .child(desc.to_owned()),
                )
        };
        let mut cards = options.into_iter().enumerate().map(|(i, o)| card(i, o, cx));
        let mut grid = div()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .px(rpx(20.))
            .py(rpx(14.));
        for _ in 0..2 {
            let row = div()
                .flex()
                .gap(rpx(10.))
                .children(cards.next())
                .children(cards.next());
            grid = grid.child(row);
        }
        crate::overlays::scrim(p, false)
            .key_context("Overlay")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| this.dismiss(w, cx)),
            )
            .child(
                div()
                    .id("new-chooser")
                    .w(rpx(640.))
                    .bg(p.elev)
                    .rounded(px(10.))
                    .shadow(ui::shadow(p))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .pt(rpx(18.))
                            .px(rpx(20.))
                            .pb(rpx(4.))
                            .flex()
                            .flex_col()
                            .gap(rpx(4.))
                            .child(
                                div()
                                    .text_size(ts::HEADING)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("What do you want to connect to?"),
                            )
                            .child(div().text_size(ts::UI).text_color(p.fg2).child(
                                "Everything you add lands in Explorer, under the place it lives.",
                            )),
                    )
                    .child(grid)
                    .child(
                        div()
                            .px(rpx(20.))
                            .pt(rpx(2.))
                            .pb(rpx(18.))
                            .flex()
                            .flex_col()
                            .gap(rpx(6.))
                            .child(
                                div()
                                    .text_size(ts::SMALL)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.fg3)
                                    .child("FOUND ON THIS MACHINE"),
                            )
                            .child(
                                div()
                                    .id("new-import-ssh")
                                    .h(rpx(32.))
                                    .flex()
                                    .items_center()
                                    .gap(rpx(10.))
                                    .px(rpx(10.))
                                    .border_1()
                                    .border_color(p.bd)
                                    .rounded(px(6.))
                                    .text_size(ts::UI)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(p.hover))
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.overlay = None;
                                        this.run_command(CommandId::ImportSshConfig, w, cx);
                                    }))
                                    .child(
                                        div()
                                            .font_family(MONO)
                                            .text_size(ts::LABEL)
                                            .child("~/.ssh/config"),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_color(p.fg2)
                                            .child("Hosts, jump hosts and identity files"),
                                    )
                                    .child(div().text_color(p.acc).child("Import")),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tab_in_front_adds_its_menu() {
        assert_eq!(
            menus_for(Some(TopMenu::Query)),
            [
                TopMenu::File,
                TopMenu::View,
                TopMenu::Go,
                TopMenu::Query,
                TopMenu::Help
            ]
        );
        assert_eq!(menus_for(None).len(), 4);
    }

    #[test]
    fn view_menu_checks_the_pane_and_grouping() {
        let s = MenuState {
            sidebar_open: true,
            side: Some(SideTab::Tools),
            group: ExplorerGroup::Type,
            ..Default::default()
        };
        let checked: Vec<&str> = menu_lines(TopMenu::View, s)
            .into_iter()
            .filter_map(|l| match l {
                MenuLine::Item(label, _, _, true) => Some(label),
                _ => None,
            })
            .collect();
        assert_eq!(checked, ["Tools", "Type"]);
    }

    #[test]
    fn every_menu_has_commands() {
        for m in [
            TopMenu::File,
            TopMenu::View,
            TopMenu::Go,
            TopMenu::Query,
            TopMenu::Terminal,
            TopMenu::Help,
        ] {
            assert!(
                menu_lines(m, MenuState::default())
                    .iter()
                    .any(|l| matches!(l, MenuLine::Item(..))),
                "{m:?}"
            );
        }
    }

    #[test]
    fn tools_offer_add_until_a_service_is_saved() {
        let empty = Profiles::default();
        let groups = tool_groups(&empty);
        assert_eq!(
            groups.iter().map(|g| g.name).collect::<Vec<_>>(),
            ["Servers & databases", "AWS", "Azure", "Cloudflare"]
        );
        assert!(groups[1..].iter().all(|g| !g.ready));
        let s3: switchyard_core::store::CloudConnection =
            serde_json::from_value(serde_json::json!({
                "id": "c1", "name": "assets", "service": "s3", "environment": "production",
            }))
            .unwrap_or_else(|e| panic!("{e}"));
        let saved = Profiles {
            all: vec![Profile::Cloud(s3)],
        };
        let aws = tool_groups(&saved).into_iter().find(|g| g.name == "AWS");
        let aws = aws.unwrap_or_else(|| panic!("no AWS group"));
        assert!(aws.ready);
        let tags: Vec<_> = aws.tools.iter().map(|t| (t.badge, t.tag)).collect();
        assert!(tags.contains(&("S3", "Open")));
        assert!(tags.iter().filter(|(_, t)| *t == "Add").count() == 2);
        let cf = tool_groups(&empty)
            .into_iter()
            .find(|g| g.name == "Cloudflare");
        assert!(cf.is_some_and(|g| {
            g.tools
                .iter()
                .any(|t| t.cmd == CommandId::OpenEngine(Engine::D1))
        }));
    }
}
